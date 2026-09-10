use std::{
    collections::VecDeque,
    fmt,
    sync::{Arc, Mutex, MutexGuard},
};

use rmcp::{
    ErrorData as McpError,
    model::Task,
    task_manager::{TaskContext, TaskFuture, TaskManager, TaskOptions},
};
use tokio::sync::{OwnedSemaphorePermit, Semaphore, TryAcquireError};

use crate::config::Config;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdmissionError {
    Closed,
    ActiveLimit,
    RetainedLimit,
}

impl AdmissionError {
    pub fn message(self) -> &'static str {
        match self {
            Self::Closed => "the task manager is shutting down",
            Self::ActiveLimit => "the active task limit has been reached",
            Self::RetainedLimit => "the retained task limit has been reached",
        }
    }
}

/// Server-local bounds around RMCP's task manager.
#[derive(Clone)]
pub struct TaskAdmission {
    manager: TaskManager,
    active: Arc<Semaphore>,
    state: Arc<Mutex<TaskState>>,
    max_retained: usize,
    ttl_ms: u64,
}

#[derive(Debug, Default)]
struct TaskState {
    closed: bool,
    retained: VecDeque<RetainedTask>,
}

#[derive(Debug)]
struct RetainedTask {
    task_id: String,
}

impl fmt::Debug for TaskAdmission {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("TaskAdmission")
            .field("max_retained", &self.max_retained)
            .field("ttl_ms", &self.ttl_ms)
            .field("retained", &self.retained_count())
            .field("active", &self.manager.running_task_count())
            .finish_non_exhaustive()
    }
}

impl TaskAdmission {
    pub fn new(config: &Config) -> Self {
        Self {
            manager: TaskManager::new(),
            active: Arc::new(Semaphore::new(
                usize::try_from(config.limits.max_active_tasks).unwrap_or(usize::MAX),
            )),
            state: Arc::new(Mutex::new(TaskState::default())),
            max_retained: usize::try_from(config.limits.max_retained_tasks).unwrap_or(usize::MAX),
            ttl_ms: config.task_ttl_ms(),
        }
    }

    /// Starts a task when both active and retained limits allow it.
    ///
    /// # Errors
    ///
    /// Returns [`AdmissionError::ActiveLimit`] or
    /// [`AdmissionError::RetainedLimit`] when the corresponding bound is full.
    pub fn spawn<F>(
        &self,
        status_message: &'static str,
        make_future: F,
    ) -> Result<Task, AdmissionError>
    where
        F: FnOnce(TaskContext) -> TaskFuture + Send + 'static,
    {
        let permit = self.active.clone().try_acquire_owned().map_err(|error| {
            if matches!(error, TryAcquireError::Closed) {
                AdmissionError::Closed
            } else {
                AdmissionError::ActiveLimit
            }
        })?;

        let mut state = self.state();
        if state.closed {
            drop(state);
            drop(permit);
            return Err(AdmissionError::Closed);
        }
        self.sweep_retained(&mut state);
        if state.retained.len() >= self.max_retained {
            drop(state);
            drop(permit);
            return Err(AdmissionError::RetainedLimit);
        }
        let options = TaskOptions::new()
            .with_ttl_ms(self.ttl_ms)
            .with_poll_interval_ms(1_000)
            .with_status_message(status_message);
        let task = self.manager.spawn(options, move |context| {
            Box::pin(async move {
                let _permit = permit;
                make_future(context).await
            })
        });
        state.retained.push_back(RetainedTask {
            task_id: task.task_id.clone(),
        });
        Ok(task)
    }

    /// Returns a retained task by identifier.
    ///
    /// # Errors
    ///
    /// Returns the RMCP error when the task does not exist or cannot be read.
    pub fn get(&self, task_id: &str) -> Result<rmcp::model::DetailedTask, McpError> {
        self.manager.get_task(task_id)
    }

    /// Adds responses to a retained task.
    ///
    /// # Errors
    ///
    /// Returns the RMCP error when the task does not exist or cannot be updated.
    pub fn update(
        &self,
        task_id: &str,
        responses: impl IntoIterator<Item = (String, serde_json::Value)>,
    ) -> Result<(), McpError> {
        self.manager.update_task(task_id, responses)
    }

    /// Cancels a retained task.
    ///
    /// # Errors
    ///
    /// Returns the RMCP error when the task does not exist or cannot be cancelled.
    pub fn cancel(&self, task_id: &str) -> Result<(), McpError> {
        self.manager.cancel_task(task_id)
    }

    pub fn retained_count(&self) -> usize {
        let mut state = self.state();
        self.sweep_retained(&mut state);
        state.retained.len()
    }

    pub fn shutdown(&self) {
        let mut state = self.state();
        if state.closed {
            return;
        }
        state.closed = true;
        self.active.close();
        self.manager.shutdown();
        state.retained.clear();
    }

    fn sweep_retained(&self, state: &mut TaskState) {
        state.retained.retain(|task| {
            self.manager
                .get_task(&task.task_id)
                .map(|_| true)
                .unwrap_or_else(|error| error.code != rmcp::model::ErrorCode::INVALID_PARAMS)
        });
    }

    fn state(&self) -> MutexGuard<'_, TaskState> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

#[allow(dead_code)]
fn _permit_is_send(_: OwnedSemaphorePermit) {}

#[cfg(test)]
mod tests {
    use super::*;
    use rmcp::model::{CallToolResult, ContentBlock, TaskStatus};
    use std::time::Duration;

    #[tokio::test]
    async fn active_and_retained_limits_are_non_blocking() {
        let mut config = Config::defaults_at("/workspace");
        config.limits.max_active_tasks = 1;
        config.limits.max_retained_tasks = 1;
        let admission = TaskAdmission::new(&config);
        let (release, wait) = tokio::sync::oneshot::channel::<()>();

        let task = admission
            .spawn("working", move |_| {
                Box::pin(async move {
                    let _ = wait.await;
                    Ok(CallToolResult::success(vec![ContentBlock::text("done")]))
                })
            })
            .expect("first task is admitted");
        assert!(matches!(
            admission.spawn("blocked", |_| Box::pin(async { unreachable!() })),
            Err(AdmissionError::ActiveLimit)
        ));

        let _ = release.send(());
        loop {
            let current = admission.get(&task.task_id).expect("task remains visible");
            if current.status() == TaskStatus::Completed {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert!(matches!(
            admission.spawn("retained", |_| Box::pin(async { unreachable!() })),
            Err(AdmissionError::RetainedLimit)
        ));

        admission.shutdown();
        assert_eq!(admission.retained_count(), 0);
        assert!(matches!(
            admission.spawn("closed", |_| Box::pin(async { unreachable!() })),
            Err(AdmissionError::Closed)
        ));
    }

    #[tokio::test]
    async fn shutdown_race_is_terminal_for_later_spawns() {
        for _ in 0..32 {
            let admission = Arc::new(TaskAdmission::new(&Config::defaults_at("/workspace")));
            let barrier = Arc::new(tokio::sync::Barrier::new(2));
            let spawning = {
                let admission = admission.clone();
                let barrier = barrier.clone();
                tokio::spawn(async move {
                    barrier.wait().await;
                    admission.spawn("racing", |_| {
                        Box::pin(async {
                            Ok(CallToolResult::success(vec![ContentBlock::text("done")]))
                        })
                    })
                })
            };
            barrier.wait().await;
            admission.shutdown();
            let _ = spawning.await.expect("spawner joins");

            assert!(matches!(
                admission.spawn("after shutdown", |_| Box::pin(async { unreachable!() })),
                Err(AdmissionError::Closed)
            ));
            assert_eq!(admission.retained_count(), 0);
        }
    }

    #[tokio::test]
    async fn retained_limit_tracks_sdk_entries_until_the_sdk_evicts_them() {
        let admission = TaskAdmission {
            manager: TaskManager::new(),
            active: Arc::new(Semaphore::new(2)),
            state: Arc::new(Mutex::new(TaskState::default())),
            max_retained: 1,
            ttl_ms: 50,
        };
        let (release, wait) = tokio::sync::oneshot::channel::<()>();
        let task = admission
            .spawn("long-running", move |_| {
                Box::pin(async move {
                    let _ = wait.await;
                    Ok(CallToolResult::success(vec![ContentBlock::text("done")]))
                })
            })
            .expect("admit long-running task");

        tokio::time::sleep(Duration::from_millis(10)).await;
        assert_eq!(admission.retained_count(), 1);
        assert!(matches!(
            admission.spawn("still retained", |_| Box::pin(async { unreachable!() })),
            Err(AdmissionError::RetainedLimit)
        ));

        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(
            admission
                .get(&task.task_id)
                .expect("expired task remains retained")
                .status(),
            TaskStatus::Failed
        );
        assert_eq!(admission.retained_count(), 1);
        tokio::time::sleep(Duration::from_millis(55)).await;
        assert_eq!(admission.retained_count(), 0);
        let _ = release.send(());
    }
}
