#![allow(
    clippy::missing_errors_doc,
    clippy::struct_excessive_bools,
    clippy::too_many_arguments,
    clippy::too_many_lines
)]

use std::{
    collections::{BTreeMap, HashMap},
    ffi::{OsStr, OsString},
    fmt, fs, io,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

use thiserror::Error;
use tokio::{
    sync::{Notify, mpsc, watch},
    task::JoinHandle,
    time,
};
use tokio_util::sync::CancellationToken;

use process_wrap::tokio::{ChildWrapper, CommandWrap, KillOnDrop};

#[cfg(windows)]
use process_wrap::tokio::JobObject;
#[cfg(unix)]
use process_wrap::tokio::ProcessGroup;

use super::{
    identity::{self, command_hash},
    journal::{JournalRecord, ProcessGroupIdentity, ProcessJournal},
    output::{
        DiagnosticCallback, OutputCollector, OutputEvent, OutputSnapshot, StreamKind,
        spawn_stderr_reader, spawn_stdout_reader,
    },
    root_bound::RootBoundCommand,
};
use crate::workspace::AuthorizedRoot;

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(600);
const DEFAULT_KILL_GRACE: Duration = Duration::from_millis(500);
const DEFAULT_CLEANUP_TIMEOUT: Duration = Duration::from_secs(2);
const DEFAULT_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(2);
const DEFAULT_MAX_OUTPUT_BYTES: usize = 8 * 1024 * 1024;
const POLL_INTERVAL: Duration = Duration::from_millis(10);
const SIGTERM: i32 = 15;
#[cfg(unix)]
const ESRCH: i32 = 3;

#[derive(Debug, Error)]
pub enum ProcessError {
    #[error("process request was cancelled before spawn")]
    Cancelled,
    #[error("process deadline elapsed before spawn")]
    TimedOut,
    #[error("process supervisor is closing")]
    Closing,
    #[error("invalid process specification: {0}")]
    InvalidSpecification(String),
    #[error("failed to spawn {executable}: {source}")]
    Spawn {
        executable: PathBuf,
        source: io::Error,
    },
    #[error("spawned process did not expose a PID")]
    MissingPid,
    #[error("root-bound process launch failed: {0}")]
    RootBinding(String),
}

#[derive(Clone)]
pub struct ProcessRunOptions {
    pub cwd: PathBuf,
    pub env: BTreeMap<OsString, OsString>,
    pub timeout: Duration,
    pub deadline: Option<Instant>,
    pub cancel: Option<CancellationToken>,
    pub max_output_bytes: usize,
    pub kill_grace: Duration,
    pub cleanup_timeout: Duration,
    pub diagnostic_callback: Option<DiagnosticCallback>,
    /// Retain an unsanitized, bounded stdout prefix for machine-readable protocols.
    /// This is opt-in; human/tool output remains sanitized.
    pub capture_raw_stdout: bool,
    pub stdout_callback: Option<super::output::StdoutCallback>,
}

impl fmt::Debug for ProcessRunOptions {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ProcessRunOptions")
            .field("cwd", &self.cwd)
            .field("env_entries", &self.env.len())
            .field("timeout", &self.timeout)
            .field("deadline", &self.deadline)
            .field(
                "cancelled",
                &self
                    .cancel
                    .as_ref()
                    .is_some_and(CancellationToken::is_cancelled),
            )
            .field("max_output_bytes", &self.max_output_bytes)
            .field("kill_grace", &self.kill_grace)
            .field("cleanup_timeout", &self.cleanup_timeout)
            .field("diagnostic_callback", &self.diagnostic_callback.is_some())
            .finish()
    }
}

impl ProcessRunOptions {
    #[must_use]
    pub fn on_stdout<F>(mut self, callback: F) -> Self
    where
        F: Fn(&[u8]) -> bool + Send + Sync + 'static,
    {
        self.stdout_callback = Some(Arc::new(callback));
        self
    }

    pub fn new(cwd: impl Into<PathBuf>) -> Self {
        Self {
            cwd: cwd.into(),
            env: BTreeMap::new(),
            timeout: DEFAULT_TIMEOUT,
            deadline: None,
            cancel: None,
            max_output_bytes: DEFAULT_MAX_OUTPUT_BYTES,
            kill_grace: DEFAULT_KILL_GRACE,
            cleanup_timeout: DEFAULT_CLEANUP_TIMEOUT,
            diagnostic_callback: None,
            capture_raw_stdout: false,
            stdout_callback: None,
        }
    }

    #[must_use]
    pub fn with_raw_stdout(mut self) -> Self {
        self.capture_raw_stdout = true;
        self
    }

    #[must_use]
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    #[must_use]
    pub fn with_timeout_ms(mut self, timeout_ms: u64) -> Self {
        self.timeout = Duration::from_millis(timeout_ms);
        self
    }

    #[must_use]
    pub fn with_deadline(mut self, deadline: Instant) -> Self {
        self.deadline = Some(deadline);
        self
    }

    #[must_use]
    pub fn with_cancellation(mut self, cancel: CancellationToken) -> Self {
        self.cancel = Some(cancel);
        self
    }

    #[must_use]
    pub fn with_env<K, V>(mut self, key: K, value: V) -> Self
    where
        K: Into<OsString>,
        V: Into<OsString>,
    {
        self.env.insert(key.into(), value.into());
        self
    }

    #[must_use]
    pub fn with_environment(
        mut self,
        environment: impl IntoIterator<Item = (OsString, OsString)>,
    ) -> Self {
        self.env.extend(environment);
        self
    }

    #[must_use]
    pub fn with_max_output_bytes(mut self, max_output_bytes: usize) -> Self {
        self.max_output_bytes = max_output_bytes;
        self
    }

    #[must_use]
    pub fn with_kill_grace(mut self, kill_grace: Duration) -> Self {
        self.kill_grace = kill_grace;
        self
    }

    #[must_use]
    pub fn with_cleanup_timeout(mut self, cleanup_timeout: Duration) -> Self {
        self.cleanup_timeout = cleanup_timeout;
        self
    }

    #[must_use]
    pub fn on_diagnostic<F>(mut self, callback: F) -> Self
    where
        F: Fn(&str) -> bool + Send + Sync + 'static,
    {
        self.diagnostic_callback = Some(Arc::new(callback));
        self
    }
}

impl Default for ProcessRunOptions {
    fn default() -> Self {
        Self::new(PathBuf::from("."))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandSpec {
    pub executable: PathBuf,
    pub args: Vec<OsString>,
    pub cwd: PathBuf,
    pub env: BTreeMap<OsString, OsString>,
}

impl CommandSpec {
    pub fn new(executable: impl Into<PathBuf>, cwd: impl Into<PathBuf>) -> Self {
        Self {
            executable: executable.into(),
            args: Vec::new(),
            cwd: cwd.into(),
            env: BTreeMap::new(),
        }
    }

    #[must_use]
    pub fn arg(mut self, argument: impl Into<OsString>) -> Self {
        self.args.push(argument.into());
        self
    }

    #[must_use]
    pub fn args(mut self, arguments: impl IntoIterator<Item = impl Into<OsString>>) -> Self {
        self.args.extend(arguments.into_iter().map(Into::into));
        self
    }

    #[must_use]
    pub fn env<K, V>(mut self, key: K, value: V) -> Self
    where
        K: Into<OsString>,
        V: Into<OsString>,
    {
        self.env.insert(key.into(), value.into());
        self
    }

    fn normalized(&self) -> Result<Self, ProcessError> {
        validate_path(&self.executable, "executable")?;
        validate_path(&self.cwd, "cwd")?;
        let executable = fs::canonicalize(&self.executable).map_err(|error| {
            ProcessError::InvalidSpecification(format!(
                "cannot resolve executable {}: {error}",
                self.executable.display()
            ))
        })?;
        let cwd = fs::canonicalize(&self.cwd).map_err(|error| {
            ProcessError::InvalidSpecification(format!(
                "cannot resolve cwd {}: {error}",
                self.cwd.display()
            ))
        })?;
        if !cwd.is_dir() {
            return Err(ProcessError::InvalidSpecification(format!(
                "cwd is not a directory: {}",
                cwd.display()
            )));
        }
        if !executable.is_file() {
            return Err(ProcessError::InvalidSpecification(format!(
                "executable is not a regular file: {}",
                executable.display()
            )));
        }
        for argument in &self.args {
            validate_os_string(argument, "argument")?;
        }
        for (key, value) in &self.env {
            validate_environment_key(key)?;
            validate_os_string(value, "environment value")?;
        }
        Ok(Self {
            executable,
            args: self.args.clone(),
            cwd,
            env: self.env.clone(),
        })
    }
}

#[derive(Debug, Clone)]
pub struct ProcessRunResult {
    pub command: String,
    pub exit_code: i32,
    pub signal: Option<i32>,
    pub timed_out: bool,
    pub cancelled: bool,
    pub duration_ms: u64,
    pub first_diagnostic_ms: Option<u64>,
    pub output_truncated: bool,
    pub output: String,
    pub stdout: String,
    pub stderr: String,
    pub raw_stdout: Option<Vec<u8>>,
    pub drain_complete: bool,
    pub cleanup_complete: bool,
    pub token: String,
    pub child_pid: u32,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ShutdownReport {
    pub requested: usize,
    pub completed: usize,
    pub remaining: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StopReason {
    Cancelled,
    TimedOut,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StopCommand {
    None,
    Cancel,
}

#[derive(Debug)]
struct ActiveRun {
    child_pid: Option<u32>,
    stop: watch::Sender<StopCommand>,
    completion: Arc<Completion>,
}

#[derive(Debug, Default)]
struct SupervisorState {
    closing: bool,
    active: HashMap<String, ActiveRun>,
}

#[derive(Debug)]
struct Completion {
    complete: AtomicBool,
    notify: Notify,
}

impl Completion {
    fn new() -> Self {
        Self {
            complete: AtomicBool::new(false),
            notify: Notify::new(),
        }
    }

    fn finish(&self) {
        if !self.complete.swap(true, Ordering::Release) {
            self.notify.notify_waiters();
        }
    }

    async fn wait(&self, deadline: Instant) -> bool {
        loop {
            let notified = self.notify.notified();
            if self.complete.load(Ordering::Acquire) {
                return true;
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return self.complete.load(Ordering::Acquire);
            }
            if time::timeout(remaining, notified).await.is_err() {
                return self.complete.load(Ordering::Acquire);
            }
        }
    }
}

#[derive(Debug)]
struct SupervisorInner {
    state: Mutex<SupervisorState>,
    active_count: AtomicUsize,
    journal: Option<Arc<ProcessJournal>>,
    shutdown_timeout: Duration,
}

impl SupervisorInner {
    fn reserve(self: &Arc<Self>, token: String) -> Result<RunRegistration, ProcessError> {
        let (stop, stop_receiver) = watch::channel(StopCommand::None);
        let completion = Arc::new(Completion::new());
        let mut state = lock_unpoisoned(&self.state);
        if state.closing {
            return Err(ProcessError::Closing);
        }
        state.active.insert(
            token.clone(),
            ActiveRun {
                child_pid: None,
                stop,
                completion: Arc::clone(&completion),
            },
        );
        self.active_count.fetch_add(1, Ordering::AcqRel);
        Ok(RunRegistration {
            inner: Arc::clone(self),
            token,
            completion,
            stop_receiver,
            finished: false,
            retained: false,
        })
    }

    fn set_child_pid(&self, token: &str, child_pid: u32) {
        let mut state = lock_unpoisoned(&self.state);
        if let Some(active) = state.active.get_mut(token) {
            active.child_pid = Some(child_pid);
        }
    }

    fn finish(&self, token: &str, completion: &Completion) {
        let removed = lock_unpoisoned(&self.state).active.remove(token).is_some();
        if removed {
            self.active_count.fetch_sub(1, Ordering::AcqRel);
        }
        completion.finish();
    }

    fn snapshot_for_shutdown(&self) -> Vec<(watch::Sender<StopCommand>, Arc<Completion>)> {
        let mut state = lock_unpoisoned(&self.state);
        state.closing = true;
        state
            .active
            .values()
            .map(|active| (active.stop.clone(), Arc::clone(&active.completion)))
            .collect()
    }
}

#[derive(Debug)]
struct RunRegistration {
    inner: Arc<SupervisorInner>,
    token: String,
    completion: Arc<Completion>,
    stop_receiver: watch::Receiver<StopCommand>,
    finished: bool,
    retained: bool,
}

impl RunRegistration {
    fn set_child_pid(&self, child_pid: u32) {
        self.inner.set_child_pid(&self.token, child_pid);
    }

    fn finish(mut self) {
        self.finished = true;
        self.inner.finish(&self.token, &self.completion);
    }

    fn retain(mut self) {
        self.retained = true;
    }
}

impl Drop for RunRegistration {
    fn drop(&mut self) {
        if !self.finished && !self.retained {
            let _ = self.stop_receiver.borrow();
            self.inner.finish(&self.token, &self.completion);
        }
    }
}

#[derive(Clone)]
pub struct ProcessSupervisor {
    inner: Arc<SupervisorInner>,
}

impl fmt::Debug for ProcessSupervisor {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ProcessSupervisor")
            .field("active_count", &self.active_count())
            .field("closing", &self.is_closing())
            .finish()
    }
}

pub type AsyncProcessRunner = ProcessSupervisor;

impl ProcessSupervisor {
    pub fn new(journal: Option<Arc<ProcessJournal>>) -> Self {
        Self {
            inner: Arc::new(SupervisorInner {
                state: Mutex::new(SupervisorState::default()),
                active_count: AtomicUsize::new(0),
                journal,
                shutdown_timeout: DEFAULT_SHUTDOWN_TIMEOUT,
            }),
        }
    }

    pub fn without_journal() -> Self {
        Self::new(None)
    }

    pub fn with_journal(journal: ProcessJournal) -> Self {
        Self::new(Some(Arc::new(journal)))
    }

    #[must_use]
    pub fn with_shutdown_timeout(mut self, shutdown_timeout: Duration) -> Self {
        if let Some(inner) = Arc::get_mut(&mut self.inner) {
            inner.shutdown_timeout = shutdown_timeout;
        }
        self
    }

    pub fn active_count(&self) -> usize {
        self.inner.active_count.load(Ordering::Acquire)
    }

    pub fn is_closing(&self) -> bool {
        lock_unpoisoned(&self.inner.state).closing
    }

    pub async fn close(&self) -> ShutdownReport {
        let active = self.inner.snapshot_for_shutdown();
        let requested = active.len();
        let deadline = Instant::now() + self.inner.shutdown_timeout;
        for (stop, _) in &active {
            let _ = stop.send(StopCommand::Cancel);
        }
        let mut completed = 0;
        for (_, completion) in active {
            if completion.wait(deadline).await {
                completed += 1;
            }
        }
        ShutdownReport {
            requested,
            completed,
            remaining: self.active_count(),
        }
    }

    pub async fn run<I, A>(
        &self,
        executable: impl Into<PathBuf>,
        args: I,
        options: ProcessRunOptions,
    ) -> Result<ProcessRunResult, ProcessError>
    where
        I: IntoIterator<Item = A>,
        A: Into<OsString>,
    {
        let mut spec = CommandSpec::new(executable, options.cwd.clone());
        spec.args = args.into_iter().map(Into::into).collect();
        spec.env = options.env.clone();
        self.run_spec(spec, options).await
    }

    /// Starts an exact-root process through the internal guard.  Unlike `run`,
    /// the requested cwd is never supplied to the outer spawn operation.
    pub async fn run_authorized<I, A>(
        &self,
        executable: impl Into<PathBuf>,
        args: I,
        options: ProcessRunOptions,
        authority: Arc<AuthorizedRoot>,
    ) -> Result<ProcessRunResult, ProcessError>
    where
        I: IntoIterator<Item = A>,
        A: Into<OsString>,
    {
        let mut logical = CommandSpec::new(executable, options.cwd.clone());
        logical.args = args.into_iter().map(Into::into).collect();
        logical.env = options.env.clone();
        let logical = logical.normalized()?;
        let bound = RootBoundCommand::new(
            &authority,
            &logical.cwd,
            &logical.executable,
            &logical.args,
            &logical.env,
        )
        .map_err(|error| ProcessError::RootBinding(error.to_string()))?;
        let ambient = std::env::current_dir().map_err(|error| {
            ProcessError::RootBinding(format!(
                "cannot retain ambient cwd for guard spawn: {error}"
            ))
        })?;
        let mut guard_options = options;
        guard_options.cwd = ambient;
        guard_options.env = bound.environment.clone();
        let guard_spec = CommandSpec {
            executable: bound.executable,
            args: bound.args,
            cwd: guard_options.cwd.clone(),
            env: bound.environment.clone(),
        };
        let result = self
            .run_spec_inner(
                guard_spec,
                guard_options,
                None,
                Some(logical),
                Some(bound.handoff_args),
            )
            .await;
        // On Windows cap-std deliberately opens directories without
        // FILE_SHARE_DELETE. Retain that exact handle until the guard and its
        // child are fully reaped so the lexical target cannot be replaced.
        drop(authority);
        result
    }

    pub async fn run_spec(
        &self,
        spec: CommandSpec,
        options: ProcessRunOptions,
    ) -> Result<ProcessRunResult, ProcessError> {
        let spawn_cwd = options.cwd.clone();
        self.run_spec_inner(spec, options, Some(spawn_cwd), None, None)
            .await
    }

    async fn run_spec_inner(
        &self,
        spec: CommandSpec,
        options: ProcessRunOptions,
        spawn_cwd: Option<PathBuf>,
        logical_spec: Option<CommandSpec>,
        handoff_args: Option<Vec<OsString>>,
    ) -> Result<ProcessRunResult, ProcessError> {
        validate_options(&options)?;
        let token = make_token();
        let registration = self.inner.reserve(token.clone())?;
        let started = Instant::now();
        let normalized = match spec.normalized() {
            Ok(spec) => spec,
            Err(error) => {
                registration.finish();
                return Err(error);
            }
        };
        if options
            .cancel
            .as_ref()
            .is_some_and(CancellationToken::is_cancelled)
            || *registration.stop_receiver.borrow() == StopCommand::Cancel
        {
            return Err(ProcessError::Cancelled);
        }
        if options.timeout.is_zero()
            || options
                .deadline
                .is_some_and(|deadline| Instant::now() >= deadline)
        {
            return Err(ProcessError::TimedOut);
        }
        let evidence_spec = logical_spec.as_ref().unwrap_or(&normalized);
        let mut command = CommandWrap::with_new(&normalized.executable, |command| {
            command
                .args(&normalized.args)
                .env_clear()
                .envs(&normalized.env)
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped());
            if let Some(cwd) = &spawn_cwd {
                command.current_dir(cwd);
            }
        });
        command.wrap(KillOnDrop);
        #[cfg(unix)]
        command.wrap(ProcessGroup::leader());
        #[cfg(windows)]
        command.wrap(JobObject);

        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(source) => {
                registration.finish();
                return Err(ProcessError::Spawn {
                    executable: normalized.executable,
                    source,
                });
            }
        };
        let Some(child_pid) = child.id() else {
            registration.finish();
            return Err(ProcessError::MissingPid);
        };
        registration.set_child_pid(child_pid);

        let child_start_time = identity::read_process_identity(child_pid)
            .ok()
            .flatten()
            .map(|identity| identity.start_time);
        let group = group_identity(child_pid);
        #[cfg(target_os = "linux")]
        let journal_executable = PathBuf::from(format!("/proc/{child_pid}/exe"));
        // Windows retains the guard as the job-owned leader; platforms without
        // Linux process identity retain the prior journal identity contract.
        #[cfg(windows)]
        let journal_executable = normalized.executable.clone();
        #[cfg(all(not(target_os = "linux"), not(windows)))]
        let journal_executable = evidence_spec.executable.clone();
        let record = JournalRecord::new(
            token.clone(),
            child_pid,
            child_start_time,
            journal_executable.clone(),
            group,
            command_hash(&journal_executable, &normalized.args),
        );
        let alternate_command_hash = handoff_args
            .as_deref()
            .map(|args| command_hash(&journal_executable, args));
        let mut warnings = Vec::new();
        let journal_recorded = if let Some(journal) = &self.inner.journal {
            match journal.record_with_alternate(&record, alternate_command_hash.as_deref()) {
                Ok(()) => true,
                Err(error) => {
                    push_warning(
                        &mut warnings,
                        format!("journal record unavailable: {error}"),
                    );
                    false
                }
            }
        } else {
            false
        };

        let stdout = child.stdout().take();
        let stderr = child.stderr().take();
        let expected_streams = usize::from(stdout.is_some()) + usize::from(stderr.is_some());
        let (sender, mut events) = mpsc::channel(64);
        let mut readers = Vec::with_capacity(expected_streams);
        if let Some(stdout) = stdout {
            readers.push(spawn_stdout_reader(stdout, sender.clone()));
        }
        if let Some(stderr) = stderr {
            readers.push(spawn_stderr_reader(stderr, sender.clone()));
        }
        drop(sender);

        let mut collector = OutputCollector::new(
            started,
            options.max_output_bytes,
            options.diagnostic_callback.clone(),
        );
        collector.set_stdout_callback(options.stdout_callback.clone());
        if options.capture_raw_stdout {
            collector.capture_raw_stdout();
        }
        let mut eof = [false; 2];
        let mut stop_reason = None;
        let mut force_at = None;
        let mut cleanup_deadline = None;
        let mut force_sent = false;
        let mut force_succeeded = false;
        let mut status = None;
        let mut events_open = true;
        let timeout_deadline = started + options.timeout;
        let deadline = options
            .deadline
            .map_or(timeout_deadline, |deadline| deadline.min(timeout_deadline));
        let cancel = options.cancel.clone();
        let mut stop_receiver = registration.stop_receiver.clone();

        loop {
            let now = Instant::now();
            if stop_reason.is_none() && now >= deadline {
                request_stop(
                    &mut child,
                    StopReason::TimedOut,
                    now,
                    &options,
                    &mut stop_reason,
                    &mut force_at,
                    &mut cleanup_deadline,
                    &mut force_sent,
                    &mut force_succeeded,
                    &mut warnings,
                );
            }
            if stop_reason.is_some()
                && !force_sent
                && force_at.is_some_and(|force_at| now >= force_at)
            {
                force_kill(
                    &mut child,
                    &mut force_sent,
                    &mut force_succeeded,
                    &mut warnings,
                );
            }
            if cleanup_deadline.is_some_and(|bound| now >= bound) {
                push_warning(
                    &mut warnings,
                    "process leader did not exit within the cleanup bound".to_owned(),
                );
                break;
            }
            match child.try_wait() {
                Ok(maybe_status) => status = maybe_status,
                Err(error) => {
                    push_warning(
                        &mut warnings,
                        format!("process status check failed: {error}"),
                    );
                    force_kill(
                        &mut child,
                        &mut force_sent,
                        &mut force_succeeded,
                        &mut warnings,
                    );
                }
            }
            if status.is_some() {
                if stop_reason.is_none() {
                    force_kill(
                        &mut child,
                        &mut force_sent,
                        &mut force_succeeded,
                        &mut warnings,
                    );
                    break;
                }
                if force_sent {
                    break;
                }
            }

            let wake_at = next_wake(
                now,
                if stop_reason.is_some() {
                    now + POLL_INTERVAL
                } else {
                    deadline
                },
                force_at.filter(|_| !force_sent),
            );
            let sleep = time::sleep(wake_at.saturating_duration_since(now));
            tokio::pin!(sleep);
            tokio::select! {
                event = events.recv(), if events_open => {
                    match event {
                        Some(event) => handle_event(event, &mut collector, &mut eof),
                        None => events_open = false,
                    }
                }
                changed = stop_receiver.changed(), if stop_reason.is_none() => {
                    if changed.is_ok() && *stop_receiver.borrow() == StopCommand::Cancel {
                        request_stop(
                            &mut child,
                            StopReason::Cancelled,
                            Instant::now(),
                            &options,
                            &mut stop_reason,
                            &mut force_at,
                            &mut cleanup_deadline,
                            &mut force_sent,
                            &mut force_succeeded,
                            &mut warnings,
                        );
                    }
                }
                () = async {
                    if let Some(cancel) = &cancel {
                        cancel.cancelled().await;
                    }
                }, if cancel.is_some() && stop_reason.is_none() => {
                    request_stop(
                        &mut child,
                        StopReason::Cancelled,
                        Instant::now(),
                        &options,
                        &mut stop_reason,
                        &mut force_at,
                        &mut cleanup_deadline,
                        &mut force_sent,
                        &mut force_succeeded,
                        &mut warnings,
                    );
                }
                () = &mut sleep => {}
            }
        }

        let cleanup_deadline =
            cleanup_deadline.unwrap_or_else(|| Instant::now() + options.cleanup_timeout);
        while count_eof(eof) < expected_streams {
            let now = Instant::now();
            if now >= cleanup_deadline {
                break;
            }
            if stop_reason.is_some()
                && !force_sent
                && force_at.is_some_and(|force_at| now >= force_at)
            {
                force_kill(
                    &mut child,
                    &mut force_sent,
                    &mut force_succeeded,
                    &mut warnings,
                );
            }
            let sleep = time::sleep(cleanup_deadline.saturating_duration_since(now));
            tokio::pin!(sleep);
            tokio::select! {
                event = events.recv() => {
                    if let Some(event) = event {
                        handle_event(event, &mut collector, &mut eof);
                    } else {
                        break;
                    }
                }
                () = &mut sleep => break,
            }
        }
        let drain_complete = count_eof(eof) == expected_streams;
        if !drain_complete {
            push_warning(
                &mut warnings,
                "child output drain reached its bound".to_owned(),
            );
        }
        drop(events);
        let readers_complete = join_readers(&mut readers, !drain_complete, cleanup_deadline).await;
        if !readers_complete {
            push_warning(
                &mut warnings,
                "output reader shutdown was bounded".to_owned(),
            );
        }

        let mut cleanup_complete = false;
        if let Some(waited) = wait_child(&mut child, cleanup_deadline).await {
            match waited {
                Ok(waited_status) => {
                    status = Some(waited_status);
                    cleanup_complete = true;
                }
                Err(error) => {
                    push_warning(&mut warnings, format!("process wait failed: {error}"));
                    force_kill(
                        &mut child,
                        &mut force_sent,
                        &mut force_succeeded,
                        &mut warnings,
                    );
                    if let Some(waited) = wait_child(&mut child, cleanup_deadline).await {
                        match waited {
                            Ok(waited_status) => {
                                status = Some(waited_status);
                                cleanup_complete = true;
                            }
                            Err(error) => push_warning(
                                &mut warnings,
                                format!("forced process wait failed: {error}"),
                            ),
                        }
                    }
                }
            }
        } else {
            force_kill(
                &mut child,
                &mut force_sent,
                &mut force_succeeded,
                &mut warnings,
            );
            if let Some(waited) = wait_child(&mut child, cleanup_deadline).await {
                match waited {
                    Ok(waited_status) => {
                        status = Some(waited_status);
                        cleanup_complete = true;
                    }
                    Err(error) => {
                        push_warning(
                            &mut warnings,
                            format!("forced process wait failed: {error}"),
                        );
                    }
                }
            } else {
                push_warning(
                    &mut warnings,
                    "process cleanup reached its bound".to_owned(),
                );
            }
        }

        cleanup_complete &= force_succeeded;
        let snapshot = collector.finish();
        append_snapshot_warnings(&mut warnings, &snapshot);
        if cleanup_complete {
            if journal_recorded
                && let Some(journal) = &self.inner.journal
                && let Err(error) = journal.remove(&token)
            {
                push_warning(&mut warnings, format!("journal cleanup failed: {error}"));
            }
        } else if journal_recorded {
            push_warning(
                &mut warnings,
                "process cleanup is incomplete; journal record retained for startup recovery"
                    .to_owned(),
            );
        }
        let result = make_result(
            evidence_spec,
            token,
            child_pid,
            started,
            status,
            stop_reason,
            snapshot,
            drain_complete && readers_complete,
            cleanup_complete,
            warnings,
        );
        if cleanup_complete {
            registration.finish();
        } else {
            registration.retain();
        }
        Ok(result)
    }
}

fn lock_unpoisoned<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn validate_options(options: &ProcessRunOptions) -> Result<(), ProcessError> {
    let now = Instant::now();
    if now.checked_add(options.timeout).is_none()
        || now
            .checked_add(options.kill_grace)
            .and_then(|time| time.checked_add(options.cleanup_timeout))
            .is_none()
    {
        return Err(ProcessError::InvalidSpecification(
            "process durations exceed the monotonic clock range".into(),
        ));
    }
    if options.max_output_bytes == 0 {
        return Err(ProcessError::InvalidSpecification(
            "max_output_bytes must be greater than zero".to_owned(),
        ));
    }
    Ok(())
}

fn validate_path(path: &Path, name: &str) -> Result<(), ProcessError> {
    if !path.is_absolute() {
        return Err(ProcessError::InvalidSpecification(format!(
            "{name} must be an absolute path"
        )));
    }
    validate_os_string(path.as_os_str(), name)
}

fn validate_os_string(value: &OsStr, name: &str) -> Result<(), ProcessError> {
    #[cfg(unix)]
    let contains_nul = {
        use std::os::unix::ffi::OsStrExt;
        value.as_bytes().contains(&0)
    };
    #[cfg(windows)]
    let contains_nul = std::os::windows::ffi::OsStrExt::encode_wide(value).any(|unit| unit == 0);
    #[cfg(not(any(unix, windows)))]
    let contains_nul = value.to_string_lossy().contains('\0');
    if contains_nul {
        return Err(ProcessError::InvalidSpecification(format!(
            "{name} contains a NUL byte"
        )));
    }
    Ok(())
}

fn validate_environment_key(key: &OsStr) -> Result<(), ProcessError> {
    validate_os_string(key, "environment key")?;
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        if key.as_bytes().contains(&b'=') || key.is_empty() {
            return Err(ProcessError::InvalidSpecification(
                "environment key must be non-empty and cannot contain '='".to_owned(),
            ));
        }
    }
    #[cfg(windows)]
    if key.is_empty() || key.to_string_lossy().contains('=') {
        return Err(ProcessError::InvalidSpecification(
            "environment key must be non-empty and cannot contain '='".to_owned(),
        ));
    }
    Ok(())
}

fn make_token() -> String {
    static SEQUENCE: AtomicUsize = AtomicUsize::new(0);
    let sequence = SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let now = Instant::now();
    format!(
        "{}-{:x}-{:x}",
        std::process::id(),
        now.elapsed().as_nanos(),
        sequence
    )
}

fn render_command(spec: &CommandSpec) -> String {
    std::iter::once(spec.executable.to_string_lossy().into_owned())
        .chain(
            spec.args
                .iter()
                .map(|argument| argument.to_string_lossy().into_owned()),
        )
        .collect::<Vec<_>>()
        .join(" ")
}

fn group_identity(child_pid: u32) -> ProcessGroupIdentity {
    #[cfg(unix)]
    {
        ProcessGroupIdentity::Unix { pgid: child_pid }
    }
    #[cfg(windows)]
    {
        ProcessGroupIdentity::WindowsJob {
            process_id: child_pid,
        }
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = child_pid;
        ProcessGroupIdentity::Unmanaged
    }
}

fn request_stop(
    child: &mut Box<dyn ChildWrapper>,
    reason: StopReason,
    now: Instant,
    options: &ProcessRunOptions,
    stop_reason: &mut Option<StopReason>,
    force_at: &mut Option<Instant>,
    cleanup_deadline: &mut Option<Instant>,
    force_sent: &mut bool,
    force_succeeded: &mut bool,
    warnings: &mut Vec<String>,
) {
    if stop_reason.is_some() {
        return;
    }
    *stop_reason = Some(reason);
    #[cfg(unix)]
    {
        if let Err(error) = child.signal(SIGTERM) {
            push_warning(
                warnings,
                format!("graceful process-group stop failed: {error}"),
            );
            force_kill(child, force_sent, force_succeeded, warnings);
        }
    }
    #[cfg(not(unix))]
    {
        force_kill(child, force_sent, force_succeeded, warnings);
    }
    let force_deadline = now + options.kill_grace;
    *force_at = Some(force_deadline);
    *cleanup_deadline = Some(force_deadline + options.cleanup_timeout);
}

fn force_kill(
    child: &mut Box<dyn ChildWrapper>,
    force_sent: &mut bool,
    force_succeeded: &mut bool,
    warnings: &mut Vec<String>,
) {
    if *force_sent {
        return;
    }
    match child.start_kill() {
        Ok(()) => *force_succeeded = true,
        #[cfg(unix)]
        Err(error) if error.raw_os_error() == Some(ESRCH) => {
            *force_succeeded = true;
        }
        Err(error) => {
            push_warning(warnings, format!("force process-tree stop failed: {error}"));
        }
    }
    *force_sent = true;
}

fn next_wake(now: Instant, deadline: Instant, force_at: Option<Instant>) -> Instant {
    let mut wake = now + POLL_INTERVAL;
    if deadline < wake {
        wake = deadline;
    }
    if let Some(force_at) = force_at
        && force_at < wake
    {
        wake = force_at;
    }
    wake
}

fn handle_event(event: OutputEvent, collector: &mut OutputCollector, eof: &mut [bool; 2]) {
    match event {
        OutputEvent::Chunk(stream, bytes) => collector.push(stream, &bytes),
        OutputEvent::Eof(stream) => {
            collector.finish_stream(stream);
            eof[stream_index(stream)] = true;
        }
        OutputEvent::Error(stream, error) => collector.push_error(stream, &error),
    }
}

fn stream_index(stream: StreamKind) -> usize {
    match stream {
        StreamKind::Stdout => 0,
        StreamKind::Stderr => 1,
    }
}

fn count_eof(eof: [bool; 2]) -> usize {
    usize::from(eof[0]) + usize::from(eof[1])
}

async fn join_readers(readers: &mut [JoinHandle<()>], abort: bool, deadline: Instant) -> bool {
    let mut complete = true;
    for reader in readers {
        if abort || Instant::now() >= deadline {
            reader.abort();
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            complete = false;
            continue;
        }
        match time::timeout(remaining, &mut *reader).await {
            Ok(Ok(())) => {}
            Ok(Err(_)) | Err(_) => {
                reader.abort();
                complete = false;
            }
        }
    }
    complete
}

async fn wait_child(
    child: &mut Box<dyn ChildWrapper>,
    deadline: Instant,
) -> Option<io::Result<std::process::ExitStatus>> {
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        return None;
    }
    time::timeout(remaining, child.wait()).await.ok()
}

fn make_result(
    spec: &CommandSpec,
    token: String,
    child_pid: u32,
    started: Instant,
    status: Option<std::process::ExitStatus>,
    stop_reason: Option<StopReason>,
    snapshot: OutputSnapshot,
    drain_complete: bool,
    cleanup_complete: bool,
    warnings: Vec<String>,
) -> ProcessRunResult {
    let timed_out = stop_reason == Some(StopReason::TimedOut);
    let cancelled = stop_reason == Some(StopReason::Cancelled);
    let status_code = status.as_ref().and_then(std::process::ExitStatus::code);
    let signal = status.as_ref().copied().and_then(exit_signal);
    let exit_code = status_code.unwrap_or(if timed_out {
        124
    } else if cancelled {
        130
    } else {
        -1
    });
    ProcessRunResult {
        command: render_command(spec),
        exit_code,
        signal,
        timed_out,
        cancelled,
        duration_ms: duration_millis(started.elapsed()),
        first_diagnostic_ms: snapshot.first_diagnostic.map(duration_millis),
        output_truncated: snapshot.output_truncated,
        output: snapshot.output,
        stdout: snapshot.stdout,
        stderr: snapshot.stderr,
        raw_stdout: snapshot.raw_stdout,
        drain_complete,
        cleanup_complete,
        token,
        child_pid,
        warnings,
    }
}

fn duration_millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

#[cfg(unix)]
fn exit_signal(status: std::process::ExitStatus) -> Option<i32> {
    use std::os::unix::process::ExitStatusExt;
    status.signal()
}

#[cfg(not(unix))]
fn exit_signal(_status: std::process::ExitStatus) -> Option<i32> {
    None
}

fn append_snapshot_warnings(warnings: &mut Vec<String>, snapshot: &OutputSnapshot) {
    for error in &snapshot.read_errors {
        push_warning(warnings, format!("output reader: {error}"));
    }
}

fn push_warning(warnings: &mut Vec<String>, warning: String) {
    if warnings.len() < 16 {
        warnings.push(warning);
    }
}

#[cfg(test)]
mod tests {
    use super::super::output::OutputCollector;

    #[test]
    fn token_is_non_empty_and_journal_safe() {
        let token = super::make_token();
        assert!(!token.is_empty());
        assert!(
            token
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
        );
        let _ = OutputCollector::new(std::time::Instant::now(), 64, None);
    }

    #[tokio::test]
    async fn reader_join_obeys_the_absolute_cleanup_deadline() {
        let mut readers = vec![tokio::spawn(async {
            tokio::time::sleep(std::time::Duration::from_secs(60)).await;
        })];
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(20);

        assert!(!super::join_readers(&mut readers, false, deadline).await);
        tokio::task::yield_now().await;
        assert!(readers[0].is_finished());
    }
}
