mod admission;
mod client_roots;
mod handler;
mod progress;
mod response;
mod tasks;

pub use handler::{
    AuditData, AuditInput, AuditOutput, CheckData, CheckDetail, CheckInput, CheckOutput,
    CheckTarget, CrateLookupData, CrateLookupInput, CrateLookupOutput, DocsData, DocsInput,
    DocsOutput, EditData, EditOutput, HierarchyDirection, HierarchyInput, ImplementationsInput,
    RefactorInput, RenameInput, RustCoderServer, SemanticData, SemanticInput, SemanticOutput,
    SymbolInput, SymbolsInput, tool_definitions,
};
pub use progress::ProgressReporter;
pub use response::{ToolData, ToolOutput, WorkspaceInfo};
pub use tasks::{AdmissionError, TaskAdmission};

use std::{
    fmt,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use futures::future::{BoxFuture, FutureExt, Shared};
use tokio::sync::{Mutex, OwnedSemaphorePermit};
use tokio_util::sync::CancellationToken;

use crate::{
    config::{Config, ConfigError},
    docs::DocsResolver,
    lsp::RustAnalyzerManager,
    process::{ProcessJournal, ProcessSupervisor},
    telemetry::ActivityLog,
    tools::{AuditLimits, AuditService, CheckService},
    workspace::{AuthorizedRoot, RootGuard},
};
use admission::AdmissionController;
use client_roots::ClientRootsCoordinator;

type SharedShutdown = Shared<BoxFuture<'static, Result<(), ShutdownError>>>;

pub struct AppState {
    config: Config,
    roots: Arc<RootGuard>,
    client_roots: ClientRootsCoordinator,
    processes: ProcessSupervisor,
    check: Arc<CheckService>,
    audit: AuditService,
    docs: Arc<DocsResolver>,
    cargo_home: Option<Arc<AuthorizedRoot>>,
    lsp: Option<Arc<RustAnalyzerManager>>,
    admission: AdmissionController,
    tasks: TaskAdmission,
    telemetry: Arc<ActivityLog>,
    shutdown: CancellationToken,
    shutting_down: Arc<AtomicBool>,
    shutdown_run: Arc<Mutex<Option<SharedShutdown>>>,
}

impl fmt::Debug for AppState {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AppState")
            .field("config", &self.config)
            .field("roots", &self.roots)
            .field("client_roots", &self.client_roots)
            .field("processes", &self.processes)
            .field("check", &self.check)
            .field("lsp_available", &self.lsp.is_some())
            .field("tasks", &self.tasks)
            .field("shutting_down", &self.is_shutting_down())
            .finish_non_exhaustive()
    }
}

impl AppState {
    /// Creates shared state after validating the server configuration.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError`] when configuration validation fails.
    pub fn new(config: Config) -> Result<Self, ConfigError> {
        config.validate()?;
        let tasks = TaskAdmission::new(&config);
        let max_in_flight =
            usize::try_from(config.limits.max_in_flight_tools).unwrap_or(usize::MAX);
        let admission = AdmissionController::new(max_in_flight);
        let roots = Arc::new(
            RootGuard::new(
                config.server.allow_roots.clone(),
                config.server.allow_dependency_roots.clone(),
            )
            .map_err(|error| ConfigError::InvalidField {
                field: "server.allow_roots",
                message: error.to_string(),
            })?,
        );
        let client_roots = ClientRootsCoordinator::new(Arc::clone(&roots));
        let telemetry = Arc::new(ActivityLog::new(&config.telemetry).map_err(|error| {
            ConfigError::InvalidField {
                field: "telemetry.path",
                message: error.to_string(),
            }
        })?);
        let journal = ProcessJournal::new(config.gate.lease_dir.join("process-journal")).map_err(
            |error| ConfigError::InvalidField {
                field: "gate.lease_dir",
                message: format!("process journal unavailable: {error}"),
            },
        )?;
        let recovery = journal.recover_orphans();
        if recovery.truncated || recovery.skipped > 0 {
            tracing::warn!(
                inspected = recovery.inspected,
                killed = recovery.killed,
                skipped = recovery.skipped,
                truncated = recovery.truncated,
                "process journal recovery left entries that were not safe to kill"
            );
        }
        let processes = ProcessSupervisor::with_journal(journal);
        let check = Arc::new(CheckService::with_shared_supervisor(
            config.clone(),
            Arc::clone(&roots),
            processes.clone(),
        ));
        let audit = AuditService::new(AuditLimits::from_u64(
            config.limits.audit_files,
            config.limits.audit_file_bytes,
            config.limits.audit_total_bytes,
            config.limits.audit_findings,
        ));
        let lsp =
            RustAnalyzerManager::from_config_authorized(&config.rust_analyzer, processes.clone())
                .ok()
                .map(Arc::new);
        let cargo_home = std::env::var_os("CARGO_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".cargo")))
            .and_then(|path| RootGuard::new([path], std::iter::empty()).ok())
            .and_then(|guard| guard.configured_roots().first().cloned());
        Ok(Self {
            config,
            roots,
            client_roots,
            processes: processes.clone(),
            check,
            audit,
            docs: Arc::new(DocsResolver::with_authorized_supervisor(processes)),
            cargo_home,
            lsp,
            admission,
            tasks,
            telemetry,
            shutdown: CancellationToken::new(),
            shutting_down: Arc::new(AtomicBool::new(false)),
            shutdown_run: Arc::new(Mutex::new(None)),
        })
    }

    pub fn config(&self) -> &Config {
        &self.config
    }

    pub fn shutdown_token(&self) -> CancellationToken {
        self.shutdown.clone()
    }

    pub fn is_shutting_down(&self) -> bool {
        self.shutting_down.load(Ordering::Acquire)
    }

    /// Stop accepting new work without aborting existing task futures yet.
    pub fn begin_shutdown(&self) {
        self.shutting_down.store(true, Ordering::Release);
        self.admission.close();
    }

    pub async fn shutdown_async(&self) -> Result<(), ShutdownError> {
        self.begin_shutdown();
        let run = {
            let mut slot = self.shutdown_run.lock().await;
            if let Some(run) = slot.as_ref() {
                run.clone()
            } else {
                self.shutdown.cancel();
                let check = Arc::clone(&self.check);
                let processes = self.processes.clone();
                let lsp = self.lsp.clone();
                let tasks = self.tasks.clone();
                let telemetry = Arc::clone(&self.telemetry);
                let worker = tokio::spawn(async move {
                    let check = check.close();
                    let processes = processes.close();
                    let lsp = async {
                        if let Some(manager) = lsp {
                            manager.close_all().await
                        } else {
                            Default::default()
                        }
                    };
                    let (_, process_report, lsp_report) = tokio::join!(check, processes, lsp);
                    tasks.shutdown();
                    telemetry
                        .flush()
                        .map_err(|error| ShutdownError::Telemetry(error.to_string()))?;
                    if process_report.remaining != 0
                        || process_report.completed != process_report.requested
                    {
                        return Err(ShutdownError::Processes {
                            requested: process_report.requested,
                            completed: process_report.completed,
                            remaining: process_report.remaining,
                        });
                    }
                    if lsp_report.remaining != 0 || lsp_report.completed != lsp_report.requested {
                        return Err(ShutdownError::RustAnalyzer {
                            requested: lsp_report.requested,
                            completed: lsp_report.completed,
                            remaining: lsp_report.remaining,
                        });
                    }
                    Ok(())
                });
                let run = async move {
                    worker
                        .await
                        .map_err(|error| ShutdownError::Worker(error.to_string()))?
                }
                .boxed()
                .shared();
                *slot = Some(run.clone());
                run
            }
        };
        run.await
    }

    pub(crate) fn tasks(&self) -> &TaskAdmission {
        &self.tasks
    }

    pub(crate) fn roots(&self) -> &Arc<RootGuard> {
        &self.roots
    }

    pub(crate) fn client_roots(&self) -> &ClientRootsCoordinator {
        &self.client_roots
    }

    pub(crate) fn record_activity(
        &self,
        event: &str,
        tool: Option<&str>,
        status: Option<&str>,
        root: Option<&std::path::Path>,
        request: Option<&str>,
    ) {
        if let Err(error) = self.telemetry.record(event, tool, status, root, request) {
            tracing::warn!(%error, "could not write bounded activity telemetry");
        }
    }

    pub(crate) fn check_service(&self) -> &Arc<CheckService> {
        &self.check
    }

    pub(crate) fn audit_service(&self) -> &AuditService {
        &self.audit
    }

    pub(crate) fn docs_service(&self) -> &Arc<DocsResolver> {
        &self.docs
    }

    pub(crate) fn cargo_home(&self) -> Option<&Arc<AuthorizedRoot>> {
        self.cargo_home.as_ref()
    }

    pub(crate) fn lsp_manager(&self) -> Option<&Arc<RustAnalyzerManager>> {
        self.lsp.as_ref()
    }

    pub(crate) fn try_admit(&self) -> Result<OwnedSemaphorePermit, admission::ToolAdmissionError> {
        self.admission.try_acquire()
    }

    pub(crate) fn max_output_bytes(&self) -> u64 {
        self.config.limits.tool_output_bytes
    }

    pub(crate) fn tool_enabled(&self, name: &str) -> bool {
        self.config
            .enabled_tool_names()
            .into_iter()
            .any(|enabled| enabled == name)
    }
}

#[derive(Debug, Clone, thiserror::Error)]
pub enum ShutdownError {
    #[error(
        "process shutdown incomplete: requested={requested} completed={completed} remaining={remaining}"
    )]
    Processes {
        requested: usize,
        completed: usize,
        remaining: usize,
    },
    #[error(
        "rust-analyzer shutdown incomplete: requested={requested} completed={completed} remaining={remaining}"
    )]
    RustAnalyzer {
        requested: usize,
        completed: usize,
        remaining: usize,
    },
    #[error("telemetry flush failed: {0}")]
    Telemetry(String),
    #[error("shutdown worker failed: {0}")]
    Worker(String),
}
