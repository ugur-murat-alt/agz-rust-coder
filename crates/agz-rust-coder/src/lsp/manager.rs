//! Per-workspace rust-analyzer lifecycle management.

use std::{
    collections::BTreeMap,
    ffi::OsString,
    fmt,
    future::Future,
    io,
    path::{Path, PathBuf},
    pin::Pin,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

use super::{
    client::{
        self, CloseHandler, DocumentSyncGuard, DocumentSyncOptions, LspError, NotificationHandler,
        ServerRequestHandler,
    },
    normalize::{
        self, BinaryConfigSchema, NormalizeError, SchemaError, configuration_value,
        document_sync_options, fixed_environment, path_to_file_uri, resolve_binary_path,
    },
};
use crate::{
    config::{RustAnalyzerConfig, WorkspaceCode},
    process::{CommandSpec, ProcessRunOptions, ProcessRunResult, ProcessSupervisor, root_bound},
    workspace::{AuthorizedRoot, RootGuard},
};
use serde_json::{Value, json};
use thiserror::Error;
use tokio::{
    io::{AsyncRead, AsyncReadExt},
    process::Command,
    sync::Notify,
    task::JoinHandle,
    time,
};
use tokio_util::sync::CancellationToken;

pub type ClientRef = Arc<dyn LspClientLike>;
pub type ClientFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T, LspError>> + Send + 'a>>;
pub type ProbeFuture<'a> =
    Pin<Box<dyn Future<Output = Result<BinaryConfigSchema, ProbeError>> + Send + 'a>>;

#[derive(Debug, Clone, Error)]
pub enum ProbeError {
    #[error("schema probe was cancelled")]
    Cancelled,
    #[error("schema probe I/O failed: {0}")]
    Io(String),
    #[error("schema probe timed out")]
    TimedOut,
    #[error("schema probe output exceeded its bound")]
    OutputTooLarge,
    #[error("schema probe process failed: {0}")]
    Process(String),
    #[error("schema probe returned an invalid schema: {0}")]
    Schema(#[from] SchemaError),
}

#[derive(Debug, Clone, Error)]
pub enum ManagerError {
    #[error("rust-analyzer manager is closing")]
    Closing,
    #[error("rust-analyzer operation was cancelled")]
    Cancelled,
    #[error("workspace path is unsafe: {0}")]
    Workspace(#[from] NormalizeError),
    #[error("workspace root authority is invalid: {0}")]
    RootAuthority(String),
    #[error("rust-analyzer is unavailable: {0}")]
    Unavailable(String),
    #[error("rust-analyzer startup failed: {0}")]
    Startup(String),
    #[error("rust-analyzer client error: {0}")]
    Client(#[from] LspError),
    #[error("timed out waiting for rust-analyzer availability")]
    WaitTimeout,
    #[error("rust-analyzer manager state was poisoned")]
    Poisoned,
}

impl ManagerError {
    pub fn is_unavailable(&self) -> bool {
        matches!(self, Self::Unavailable(_))
    }
}

#[derive(Clone)]
pub struct ClientCallbacks {
    pub server_request_handler: Option<ServerRequestHandler>,
    pub notification_handler: Option<NotificationHandler>,
    pub close_handler: Option<CloseHandler>,
}

impl Default for ClientCallbacks {
    fn default() -> Self {
        Self {
            server_request_handler: None,
            notification_handler: None,
            close_handler: None,
        }
    }
}

impl fmt::Debug for ClientCallbacks {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ClientCallbacks")
            .field(
                "server_request_handler",
                &self.server_request_handler.is_some(),
            )
            .field("notification_handler", &self.notification_handler.is_some())
            .field("close_handler", &self.close_handler.is_some())
            .finish()
    }
}

/// Minimal client surface required by the manager.  Tests can provide a
/// deterministic implementation without spawning a child process.
pub trait LspClientLike: Send + Sync {
    fn set_callbacks(&self, callbacks: ClientCallbacks) -> ClientFuture<'_, ()>;

    fn set_callbacks_with_cancellation(
        &self,
        callbacks: ClientCallbacks,
        cancellation: Option<CancellationToken>,
    ) -> ClientFuture<'_, ()> {
        cancellable_client_future(self.set_callbacks(callbacks), cancellation)
    }

    fn set_document_sync(&self, options: DocumentSyncOptions) -> ClientFuture<'_, ()>;

    fn set_document_sync_with_cancellation(
        &self,
        options: DocumentSyncOptions,
        cancellation: Option<CancellationToken>,
    ) -> ClientFuture<'_, ()> {
        cancellable_client_future(self.set_document_sync(options), cancellation)
    }

    fn begin_document(
        &self,
        _uri: &str,
        _language_id: &str,
        _text: &str,
    ) -> ClientFuture<'_, DocumentSyncGuard> {
        Box::pin(async { Ok(DocumentSyncGuard::default()) })
    }

    fn begin_document_with_cancellation(
        &self,
        uri: &str,
        language_id: &str,
        text: &str,
        cancellation: Option<CancellationToken>,
    ) -> ClientFuture<'_, DocumentSyncGuard> {
        cancellable_client_future(self.begin_document(uri, language_id, text), cancellation)
    }

    fn document_version(&self, _uri: &str) -> ClientFuture<'_, Option<i64>> {
        Box::pin(async { Ok(None) })
    }

    fn request(&self, method: &str, params: Value, timeout: Duration) -> ClientFuture<'_, Value>;

    fn request_with_cancellation(
        &self,
        method: &str,
        params: Value,
        timeout: Duration,
        cancellation: Option<CancellationToken>,
    ) -> ClientFuture<'_, Value> {
        cancellable_client_future(self.request(method, params, timeout), cancellation)
    }

    fn notify_with_cancellation(
        &self,
        method: &str,
        params: Value,
        cancellation: Option<CancellationToken>,
    ) -> ClientFuture<'_, ()> {
        cancellable_client_future(self.notify(method, params), cancellation)
    }

    fn notify(&self, method: &str, params: Value) -> ClientFuture<'_, ()>;

    fn shutdown(&self, timeout: Duration) -> ClientFuture<'_, ()>;

    fn is_closed(&self) -> bool;
}

pub trait LspClientFactory: Send + Sync + 'static {
    fn spawn<'a>(
        &'a self,
        spec: CommandSpec,
        default_timeout: Duration,
        max_frame_bytes: usize,
    ) -> ClientFuture<'a, ClientRef>;

    fn spawn_with_cancellation<'a>(
        &'a self,
        spec: CommandSpec,
        default_timeout: Duration,
        max_frame_bytes: usize,
        cancellation: Option<CancellationToken>,
    ) -> ClientFuture<'a, ClientRef> {
        cancellable_client_future(
            self.spawn(spec, default_timeout, max_frame_bytes),
            cancellation,
        )
    }

    fn spawn_authorized<'a>(
        &'a self,
        spec: CommandSpec,
        default_timeout: Duration,
        max_frame_bytes: usize,
        cancellation: Option<CancellationToken>,
        authority: Arc<crate::workspace::AuthorizedRoot>,
    ) -> ClientFuture<'a, ClientRef> {
        let _ = authority;
        self.spawn_with_cancellation(spec, default_timeout, max_frame_bytes, cancellation)
    }

    /// Returns the root URI path exposed to this factory's language server.
    /// Test and alternate factories retain the lexical root by default.
    fn protocol_root(&self, lexical: &Path) -> PathBuf {
        lexical.to_owned()
    }
}

pub trait BinarySchemaProbe: Send + Sync + 'static {
    fn probe<'a>(&'a self, binary: &'a Path, timeout: Duration) -> ProbeFuture<'a>;

    fn probe_with_cancellation<'a>(
        &'a self,
        binary: &'a Path,
        timeout: Duration,
        cancellation: Option<CancellationToken>,
    ) -> ProbeFuture<'a> {
        cancellable_probe_future(self.probe(binary, timeout), cancellation)
    }

    fn probe_authorized_with_cancellation<'a>(
        &'a self,
        binary: &'a Path,
        timeout: Duration,
        cancellation: Option<CancellationToken>,
        authority: Arc<AuthorizedRoot>,
    ) -> ProbeFuture<'a> {
        let _ = authority;
        self.probe_with_cancellation(binary, timeout, cancellation)
    }

    /// Runs a root-authorized probe with the manager's process lifecycle owner.
    /// Alternate probes retain the existing authorized behavior unless they need
    /// the supervisor's process-tree cleanup guarantees.
    fn probe_authorized_with_supervisor<'a>(
        &'a self,
        binary: &'a Path,
        timeout: Duration,
        cancellation: Option<CancellationToken>,
        authority: Arc<AuthorizedRoot>,
        supervisor: ProcessSupervisor,
    ) -> ProbeFuture<'a> {
        let _ = supervisor;
        self.probe_authorized_with_cancellation(binary, timeout, cancellation, authority)
    }
}

fn cancellable_client_future<'a, T>(
    future: ClientFuture<'a, T>,
    cancellation: Option<CancellationToken>,
) -> ClientFuture<'a, T>
where
    T: Send + 'a,
{
    Box::pin(async move {
        tokio::select! {
            result = future => result,
            () = cancellation_cancelled(cancellation), if cancellation.is_some() => {
                Err(LspError::Cancelled)
            },
        }
    })
}

fn cancellable_probe_future<'a>(
    future: ProbeFuture<'a>,
    cancellation: Option<CancellationToken>,
) -> ProbeFuture<'a> {
    Box::pin(async move {
        tokio::select! {
            result = future => result,
            () = cancellation_cancelled(cancellation), if cancellation.is_some() => {
                Err(ProbeError::Cancelled)
            },
        }
    })
}

async fn cancellation_cancelled(token: Option<CancellationToken>) {
    if let Some(token) = token {
        token.cancelled().await;
    } else {
        std::future::pending::<()>().await;
    }
}

#[derive(Debug, Clone)]
pub struct ManagerOptions {
    pub binary: Option<PathBuf>,
    pub timeout: Duration,
    pub idle: Duration,
    pub max_instances: usize,
    pub workspace_code: WorkspaceCode,
    pub max_frame_bytes: usize,
    pub shutdown_timeout: Duration,
    pub wait_timeout: Duration,
    pub probe_timeout: Duration,
    pub max_probe_output_bytes: usize,
}

impl Default for ManagerOptions {
    fn default() -> Self {
        Self {
            binary: None,
            timeout: Duration::from_secs(30),
            idle: Duration::from_secs(900),
            max_instances: 2,
            workspace_code: WorkspaceCode::Deny,
            max_frame_bytes: client::DEFAULT_MAX_FRAME_BYTES,
            shutdown_timeout: Duration::from_secs(2),
            wait_timeout: Duration::from_secs(30),
            probe_timeout: Duration::from_secs(10),
            max_probe_output_bytes: 8 * 1024 * 1024,
        }
    }
}

impl ManagerOptions {
    pub fn from_config(config: &RustAnalyzerConfig) -> Self {
        Self {
            binary: config.path.clone(),
            timeout: Duration::from_millis(config.timeout_ms),
            idle: Duration::from_millis(config.idle_ms),
            max_instances: usize::try_from(config.max_instances).unwrap_or(usize::MAX),
            workspace_code: config.workspace_code,
            ..Self::default()
        }
    }

    pub fn with_binary(mut self, binary: impl Into<PathBuf>) -> Self {
        self.binary = Some(binary.into());
        self
    }

    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    pub fn with_idle(mut self, idle: Duration) -> Self {
        self.idle = idle;
        self
    }

    pub fn with_max_instances(mut self, max_instances: usize) -> Self {
        self.max_instances = max_instances;
        self
    }

    pub fn with_workspace_code(mut self, workspace_code: WorkspaceCode) -> Self {
        self.workspace_code = workspace_code;
        self
    }

    pub fn with_shutdown_timeout(mut self, timeout: Duration) -> Self {
        self.shutdown_timeout = timeout;
        self
    }

    pub fn with_wait_timeout(mut self, timeout: Duration) -> Self {
        self.wait_timeout = timeout;
        self
    }

    fn normalized(mut self) -> Result<Self, ManagerError> {
        if self.max_instances == 0 {
            return Err(ManagerError::Startup(
                "max_instances must be greater than zero".to_owned(),
            ));
        }
        self.max_instances = self.max_instances.min(2);
        self.timeout = nonzero_duration(self.timeout);
        self.idle = nonzero_duration(self.idle);
        self.shutdown_timeout = nonzero_duration(self.shutdown_timeout);
        self.wait_timeout = nonzero_duration(self.wait_timeout);
        self.probe_timeout = nonzero_duration(self.probe_timeout);
        self.max_frame_bytes = self.max_frame_bytes.max(1);
        self.max_probe_output_bytes = self.max_probe_output_bytes.max(1);
        Ok(self)
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RustAnalyzerCapabilities {
    pub expand_macro: bool,
    pub related_tests: bool,
    pub failed_obligations: bool,
    pub open_docs: bool,
}

pub fn internal_capabilities(initialized: &Value) -> RustAnalyzerCapabilities {
    let Some(experimental) = initialized
        .get("capabilities")
        .and_then(|value| value.get("experimental"))
        .and_then(Value::as_object)
    else {
        return RustAnalyzerCapabilities::default();
    };
    let nested = experimental.get("rust-analyzer").and_then(Value::as_object);
    let supported = |camel: &str, method: &str| {
        experimental.get(camel).and_then(Value::as_bool) == Some(true)
            || experimental.get(method).and_then(Value::as_bool) == Some(true)
            || nested
                .and_then(|object| object.get(camel))
                .and_then(Value::as_bool)
                == Some(true)
            || nested
                .and_then(|object| object.get(method))
                .and_then(Value::as_bool)
                == Some(true)
    };
    RustAnalyzerCapabilities {
        expand_macro: supported("expandMacro", "rust-analyzer/expandMacro"),
        related_tests: supported("relatedTests", "rust-analyzer/relatedTests"),
        failed_obligations: supported("failedObligations", "rust-analyzer/getFailedObligations"),
        open_docs: supported("openDocs", "rust-analyzer/openDocs"),
    }
}

#[derive(Debug, Clone)]
pub struct ConcreteClientAdapter {
    inner: Arc<client::LspClient>,
}

impl ConcreteClientAdapter {
    pub fn new(inner: Arc<client::LspClient>) -> Self {
        Self { inner }
    }

    pub fn client(&self) -> &Arc<client::LspClient> {
        &self.inner
    }
}

impl LspClientLike for ConcreteClientAdapter {
    fn set_callbacks(&self, callbacks: ClientCallbacks) -> ClientFuture<'_, ()> {
        let ClientCallbacks {
            server_request_handler,
            notification_handler,
            close_handler,
        } = callbacks;
        Box::pin(async move {
            if let Some(handler) = server_request_handler {
                self.inner
                    .set_server_request_handler(move |method, params, cancellation| {
                        let handler = handler.clone();
                        async move { handler(method, params, cancellation).await }
                    })
                    .await;
            }
            if let Some(handler) = notification_handler {
                self.inner
                    .set_notification_handler(move |method, params| handler(method, params))
                    .await;
            }
            if let Some(handler) = close_handler {
                self.inner.set_close_handler(move || handler()).await;
            }
            Ok(())
        })
    }

    fn set_document_sync(&self, options: DocumentSyncOptions) -> ClientFuture<'_, ()> {
        Box::pin(async move {
            self.inner.set_document_sync(options).await;
            Ok(())
        })
    }

    fn set_document_sync_with_cancellation(
        &self,
        options: DocumentSyncOptions,
        cancellation: Option<CancellationToken>,
    ) -> ClientFuture<'_, ()> {
        let inner = Arc::clone(&self.inner);
        Box::pin(async move {
            inner
                .set_document_sync_with_cancellation(options, cancellation)
                .await
        })
    }

    fn begin_document(
        &self,
        uri: &str,
        language_id: &str,
        text: &str,
    ) -> ClientFuture<'_, DocumentSyncGuard> {
        let inner = Arc::clone(&self.inner);
        let uri = uri.to_owned();
        let language_id = language_id.to_owned();
        let text = text.to_owned();
        Box::pin(async move { inner.begin_document(&uri, &language_id, &text).await })
    }

    fn begin_document_with_cancellation(
        &self,
        uri: &str,
        language_id: &str,
        text: &str,
        cancellation: Option<CancellationToken>,
    ) -> ClientFuture<'_, DocumentSyncGuard> {
        let inner = Arc::clone(&self.inner);
        let uri = uri.to_owned();
        let language_id = language_id.to_owned();
        let text = text.to_owned();
        Box::pin(async move {
            inner
                .begin_document_with_cancellation(&uri, &language_id, &text, cancellation)
                .await
        })
    }

    fn document_version(&self, uri: &str) -> ClientFuture<'_, Option<i64>> {
        let inner = Arc::clone(&self.inner);
        let uri = uri.to_owned();
        Box::pin(async move { Ok(inner.document_version(&uri).await) })
    }

    fn request(&self, method: &str, params: Value, timeout: Duration) -> ClientFuture<'_, Value> {
        let method = method.to_owned();
        Box::pin(async move {
            self.inner
                .request_with_timeout(&method, params, timeout)
                .await
        })
    }

    fn request_with_cancellation(
        &self,
        method: &str,
        params: Value,
        timeout: Duration,
        cancellation: Option<CancellationToken>,
    ) -> ClientFuture<'_, Value> {
        let method = method.to_owned();
        Box::pin(async move {
            self.inner
                .request_with_cancellation(&method, params, timeout, cancellation)
                .await
        })
    }

    fn notify(&self, method: &str, params: Value) -> ClientFuture<'_, ()> {
        let method = method.to_owned();
        Box::pin(async move { self.inner.notify(&method, params).await })
    }

    fn notify_with_cancellation(
        &self,
        method: &str,
        params: Value,
        cancellation: Option<CancellationToken>,
    ) -> ClientFuture<'_, ()> {
        let inner = Arc::clone(&self.inner);
        let method = method.to_owned();
        Box::pin(async move {
            inner
                .notify_with_cancellation(&method, params, cancellation)
                .await
        })
    }

    fn shutdown(&self, timeout: Duration) -> ClientFuture<'_, ()> {
        Box::pin(async move {
            self.inner.shutdown(timeout).await?;
            Ok(())
        })
    }

    fn is_closed(&self) -> bool {
        self.inner.is_closed()
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct ConcreteClientFactory;

impl LspClientFactory for ConcreteClientFactory {
    fn spawn<'a>(
        &'a self,
        spec: CommandSpec,
        default_timeout: Duration,
        max_frame_bytes: usize,
    ) -> ClientFuture<'a, ClientRef> {
        Box::pin(async move {
            let client = client::LspClient::spawn(spec, default_timeout, max_frame_bytes).await?;
            Ok(Arc::new(ConcreteClientAdapter::new(client)) as ClientRef)
        })
    }

    fn spawn_with_cancellation<'a>(
        &'a self,
        spec: CommandSpec,
        default_timeout: Duration,
        max_frame_bytes: usize,
        cancellation: Option<CancellationToken>,
    ) -> ClientFuture<'a, ClientRef> {
        Box::pin(async move {
            let client = client::LspClient::spawn_with_cancellation(
                spec,
                default_timeout,
                max_frame_bytes,
                cancellation,
            )
            .await?;
            Ok(Arc::new(ConcreteClientAdapter::new(client)) as ClientRef)
        })
    }

    fn spawn_authorized<'a>(
        &'a self,
        spec: CommandSpec,
        default_timeout: Duration,
        max_frame_bytes: usize,
        cancellation: Option<CancellationToken>,
        authority: Arc<crate::workspace::AuthorizedRoot>,
    ) -> ClientFuture<'a, ClientRef> {
        Box::pin(async move {
            let client = client::LspClient::spawn_authorized(
                spec,
                default_timeout,
                max_frame_bytes,
                cancellation,
                authority,
            )
            .await?;
            Ok(Arc::new(ConcreteClientAdapter::new(client)) as ClientRef)
        })
    }

    fn protocol_root(&self, lexical: &Path) -> PathBuf {
        root_bound::lsp_protocol_root(lexical)
    }
}

#[derive(Debug, Clone, Copy)]
pub struct ConcreteBinarySchemaProbe {
    max_output_bytes: usize,
}

impl Default for ConcreteBinarySchemaProbe {
    fn default() -> Self {
        Self {
            max_output_bytes: 8 * 1024 * 1024,
        }
    }
}

impl ConcreteBinarySchemaProbe {
    pub fn new(max_output_bytes: usize) -> Self {
        Self {
            max_output_bytes: max_output_bytes.max(1),
        }
    }
}

impl BinarySchemaProbe for ConcreteBinarySchemaProbe {
    fn probe<'a>(&'a self, binary: &'a Path, timeout: Duration) -> ProbeFuture<'a> {
        Box::pin(probe_binary_schema(
            binary.to_owned(),
            timeout,
            self.max_output_bytes,
            None,
        ))
    }

    fn probe_with_cancellation<'a>(
        &'a self,
        binary: &'a Path,
        timeout: Duration,
        cancellation: Option<CancellationToken>,
    ) -> ProbeFuture<'a> {
        Box::pin(probe_binary_schema(
            binary.to_owned(),
            timeout,
            self.max_output_bytes,
            cancellation,
        ))
    }

    fn probe_authorized_with_cancellation<'a>(
        &'a self,
        binary: &'a Path,
        timeout: Duration,
        cancellation: Option<CancellationToken>,
        authority: Arc<AuthorizedRoot>,
    ) -> ProbeFuture<'a> {
        self.probe_authorized_with_supervisor(
            binary,
            timeout,
            cancellation,
            authority,
            ProcessSupervisor::without_journal(),
        )
    }

    fn probe_authorized_with_supervisor<'a>(
        &'a self,
        binary: &'a Path,
        timeout: Duration,
        cancellation: Option<CancellationToken>,
        authority: Arc<AuthorizedRoot>,
        supervisor: ProcessSupervisor,
    ) -> ProbeFuture<'a> {
        let binary = binary.to_owned();
        let max_output_bytes = self.max_output_bytes;
        Box::pin(async move {
            if cancellation
                .as_ref()
                .is_some_and(CancellationToken::is_cancelled)
            {
                return Err(ProbeError::Cancelled);
            }
            let mut options = ProcessRunOptions::new(authority.path())
                .with_environment(fixed_environment())
                .with_timeout(timeout)
                .with_max_output_bytes(max_output_bytes);
            if let Some(cancellation) = cancellation {
                options = options.with_cancellation(cancellation);
            }
            let result = supervisor
                .run_authorized(
                    binary,
                    [OsString::from("--print-config-schema")],
                    options,
                    authority,
                )
                .await
                .map_err(|error| match error {
                    crate::process::ProcessError::Cancelled => ProbeError::Cancelled,
                    crate::process::ProcessError::TimedOut => ProbeError::TimedOut,
                    error => ProbeError::Io(error.to_string()),
                })?;
            supervised_schema_result(result)
        })
    }
}

fn supervised_schema_result(result: ProcessRunResult) -> Result<BinaryConfigSchema, ProbeError> {
    if !result.drain_complete || !result.cleanup_complete {
        return Err(ProbeError::Process(format!(
            "schema probe cleanup was incomplete (drain_complete={}, cleanup_complete={}): {}",
            result.drain_complete,
            result.cleanup_complete,
            result.warnings.join("; ")
        )));
    }
    if result.cancelled {
        return Err(ProbeError::Cancelled);
    }
    if result.timed_out {
        return Err(ProbeError::TimedOut);
    }
    if result.output_truncated {
        return Err(ProbeError::OutputTooLarge);
    }
    if result.exit_code != 0 || result.signal.is_some() {
        return Err(ProbeError::Process(format!(
            "schema probe exited with code {}{}: {}",
            result.exit_code,
            result
                .signal
                .map(|signal| format!(" (signal {signal})"))
                .unwrap_or_default(),
            result.stderr
        )));
    }
    BinaryConfigSchema::from_bytes(result.stdout.as_bytes()).map_err(ProbeError::Schema)
}

#[derive(Debug)]
struct BoundedOutput {
    bytes: Vec<u8>,
    truncated: bool,
}

async fn probe_binary_schema(
    binary: PathBuf,
    timeout: Duration,
    max_output_bytes: usize,
    cancellation: Option<CancellationToken>,
) -> Result<BinaryConfigSchema, ProbeError> {
    probe_binary_schema_command(
        binary,
        vec![OsString::from("--print-config-schema")],
        fixed_environment(),
        timeout,
        max_output_bytes,
        cancellation,
    )
    .await
}

async fn probe_binary_schema_command(
    binary: PathBuf,
    arguments: Vec<OsString>,
    environment: BTreeMap<OsString, OsString>,
    timeout: Duration,
    max_output_bytes: usize,
    cancellation: Option<CancellationToken>,
) -> Result<BinaryConfigSchema, ProbeError> {
    if cancellation
        .as_ref()
        .is_some_and(CancellationToken::is_cancelled)
    {
        return Err(ProbeError::Cancelled);
    }
    let mut command = Command::new(&binary);
    command
        .args(arguments)
        .env_clear()
        .envs(environment)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let mut child = command
        .spawn()
        .map_err(|error| ProbeError::Io(error.to_string()))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| ProbeError::Io("schema probe did not expose stdout".to_owned()))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| ProbeError::Io("schema probe did not expose stderr".to_owned()))?;
    let stdout_task = tokio::spawn(read_bounded(stdout, max_output_bytes));
    let stderr_task = tokio::spawn(read_bounded(stderr, max_output_bytes));

    let status = tokio::select! {
        result = time::timeout(nonzero_duration(timeout), child.wait()) => {
            match result {
                Ok(result) => result.map_err(|error| ProbeError::Io(error.to_string()))?,
                Err(_) => {
                    let _ = child.kill().await;
                    let _ = time::timeout(Duration::from_millis(100), child.wait()).await;
                    stdout_task.abort();
                    stderr_task.abort();
                    return Err(ProbeError::TimedOut);
                }
            }
        }
        () = cancellation_cancelled(cancellation.clone()), if cancellation.is_some() => {
            let _ = child.kill().await;
            let _ = time::timeout(Duration::from_millis(100), child.wait()).await;
            stdout_task.abort();
            stderr_task.abort();
            return Err(ProbeError::Cancelled);
        }
    };
    let stdout = join_probe_output(stdout_task).await?;
    let stderr = join_probe_output(stderr_task).await?;
    if stdout.truncated || stderr.truncated {
        return Err(ProbeError::OutputTooLarge);
    }
    if !status.success() {
        let message = String::from_utf8_lossy(&stderr.bytes);
        return Err(ProbeError::Process(format!(
            "schema probe exited with {status}: {message}"
        )));
    }
    BinaryConfigSchema::from_bytes(&stdout.bytes).map_err(ProbeError::Schema)
}

async fn read_bounded<R>(mut reader: R, max_bytes: usize) -> io::Result<BoundedOutput>
where
    R: AsyncRead + Unpin,
{
    let mut bytes = Vec::with_capacity(max_bytes.min(64 * 1024));
    let mut buffer = [0u8; 8 * 1024];
    let mut truncated = false;
    loop {
        let read = reader.read(&mut buffer).await?;
        if read == 0 {
            break;
        }
        if bytes.len() < max_bytes {
            let keep = (max_bytes - bytes.len()).min(read);
            bytes.extend_from_slice(&buffer[..keep]);
            if keep < read {
                truncated = true;
            }
        } else {
            truncated = true;
        }
    }
    Ok(BoundedOutput { bytes, truncated })
}

async fn join_probe_output(
    task: JoinHandle<io::Result<BoundedOutput>>,
) -> Result<BoundedOutput, ProbeError> {
    task.await
        .map_err(|error| ProbeError::Io(error.to_string()))?
        .map_err(|error| ProbeError::Io(error.to_string()))
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CloseReport {
    pub requested: usize,
    pub completed: usize,
    pub remaining: usize,
}

#[derive(Debug)]
struct StopState {
    complete: AtomicBool,
    notify: Notify,
}

impl StopState {
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

    async fn wait_until(&self, deadline: Instant) -> bool {
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

    async fn wait_until_with_cancellation(
        &self,
        deadline: Instant,
        cancellation: Option<&CancellationToken>,
    ) -> Result<bool, ManagerError> {
        loop {
            let notified = self.notify.notified();
            if self.complete.load(Ordering::Acquire) {
                return Ok(true);
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Ok(self.complete.load(Ordering::Acquire));
            }
            if let Some(cancellation) = cancellation {
                tokio::select! {
                    result = time::timeout(remaining, notified) => {
                        if result.is_err() {
                            return Ok(self.complete.load(Ordering::Acquire));
                        }
                    }
                    () = cancellation.cancelled() => return Err(ManagerError::Cancelled),
                }
            } else if time::timeout(remaining, notified).await.is_err() {
                return Ok(self.complete.load(Ordering::Acquire));
            }
        }
    }
}

struct Instance {
    root: PathBuf,
    root_authority: Arc<AuthorizedRoot>,
    client: ClientRef,
    token: Arc<()>,
    capabilities: RustAnalyzerCapabilities,
    active: AtomicUsize,
    last_used: Mutex<Instant>,
    stopping: AtomicBool,
    stop: Arc<StopState>,
}

impl fmt::Debug for Instance {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Instance")
            .field("root", &self.root)
            .field("capabilities", &self.capabilities)
            .field("active", &self.active.load(Ordering::Acquire))
            .field("stopping", &self.stopping.load(Ordering::Acquire))
            .finish_non_exhaustive()
    }
}

impl Instance {
    fn new(
        root: PathBuf,
        root_authority: Arc<AuthorizedRoot>,
        client: ClientRef,
        token: Arc<()>,
        capabilities: RustAnalyzerCapabilities,
    ) -> Self {
        Self {
            root,
            root_authority,
            client,
            token,
            capabilities,
            active: AtomicUsize::new(0),
            last_used: Mutex::new(Instant::now()),
            stopping: AtomicBool::new(false),
            stop: Arc::new(StopState::new()),
        }
    }

    fn touch(&self) {
        if let Ok(mut last_used) = self.last_used.lock() {
            *last_used = Instant::now();
        }
    }

    fn last_used(&self) -> Instant {
        self.last_used
            .lock()
            .map(|last_used| *last_used)
            .unwrap_or_else(|_| Instant::now())
    }
}

#[derive(Debug)]
struct StartFlight {
    result: Mutex<Option<Result<Arc<Instance>, ManagerError>>>,
    notify: Notify,
}

impl StartFlight {
    fn new() -> Self {
        Self {
            result: Mutex::new(None),
            notify: Notify::new(),
        }
    }
}

#[derive(Debug)]
struct SchemaFlight {
    result: Mutex<Option<Result<(), ManagerError>>>,
    notify: Notify,
}

impl SchemaFlight {
    fn new() -> Self {
        Self {
            result: Mutex::new(None),
            notify: Notify::new(),
        }
    }
}

#[derive(Debug, Default)]
struct ManagerState {
    instances: BTreeMap<PathBuf, Arc<Instance>>,
    starts: BTreeMap<PathBuf, Arc<StartFlight>>,
    closing: bool,
}

struct ManagerInner<P, F> {
    options: ManagerOptions,
    authorized_execution: bool,
    processes: ProcessSupervisor,
    probe: Arc<P>,
    factory: Arc<F>,
    state: Mutex<ManagerState>,
    availability: Arc<Notify>,
    resolved_binary: Mutex<Option<Result<PathBuf, ManagerError>>>,
    schema: Mutex<Option<(PathBuf, Result<(), ManagerError>)>>,
    schema_flight: Mutex<Option<(PathBuf, Arc<SchemaFlight>)>>,
    sweep_task: Mutex<Option<JoinHandle<()>>>,
}

impl<P, F> fmt::Debug for ManagerInner<P, F> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ManagerInner")
            .field("options", &self.options)
            .field("state", &self.state)
            .finish_non_exhaustive()
    }
}

pub struct RustAnalyzerManager<P = ConcreteBinarySchemaProbe, F = ConcreteClientFactory>
where
    P: BinarySchemaProbe,
    F: LspClientFactory,
{
    inner: Arc<ManagerInner<P, F>>,
}

impl<P, F> Clone for RustAnalyzerManager<P, F>
where
    P: BinarySchemaProbe,
    F: LspClientFactory,
{
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
        }
    }
}

impl<P, F> fmt::Debug for RustAnalyzerManager<P, F>
where
    P: BinarySchemaProbe,
    F: LspClientFactory,
{
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RustAnalyzerManager")
            .field("options", &self.inner.options)
            .field("state", &self.inner.state)
            .finish_non_exhaustive()
    }
}

impl RustAnalyzerManager<ConcreteBinarySchemaProbe, ConcreteClientFactory> {
    pub fn new(options: ManagerOptions) -> Result<Self, ManagerError> {
        Self::with_adapters(
            options,
            ConcreteBinarySchemaProbe::default(),
            ConcreteClientFactory,
        )
    }

    pub fn new_authorized(options: ManagerOptions) -> Result<Self, ManagerError> {
        Self::new_authorized_with_supervisor(options, ProcessSupervisor::without_journal())
    }

    pub(crate) fn new_authorized_with_supervisor(
        options: ManagerOptions,
        processes: ProcessSupervisor,
    ) -> Result<Self, ManagerError> {
        Self::with_adapters_mode(
            options,
            ConcreteBinarySchemaProbe::default(),
            ConcreteClientFactory,
            true,
            processes,
        )
    }

    pub fn from_config(config: &RustAnalyzerConfig) -> Result<Self, ManagerError> {
        Self::new(ManagerOptions::from_config(config))
    }

    pub(crate) fn from_config_authorized(
        config: &RustAnalyzerConfig,
        processes: ProcessSupervisor,
    ) -> Result<Self, ManagerError> {
        Self::new_authorized_with_supervisor(ManagerOptions::from_config(config), processes)
    }
}

impl<P, F> RustAnalyzerManager<P, F>
where
    P: BinarySchemaProbe,
    F: LspClientFactory,
{
    pub fn with_adapters(
        options: ManagerOptions,
        probe: P,
        factory: F,
    ) -> Result<Self, ManagerError> {
        Self::with_adapters_mode(
            options,
            probe,
            factory,
            false,
            ProcessSupervisor::without_journal(),
        )
    }

    pub fn with_authorized_adapters(
        options: ManagerOptions,
        probe: P,
        factory: F,
    ) -> Result<Self, ManagerError> {
        Self::with_adapters_mode(
            options,
            probe,
            factory,
            true,
            ProcessSupervisor::without_journal(),
        )
    }

    fn with_adapters_mode(
        options: ManagerOptions,
        probe: P,
        factory: F,
        authorized_execution: bool,
        processes: ProcessSupervisor,
    ) -> Result<Self, ManagerError> {
        let options = options.normalized()?;
        Ok(Self {
            inner: Arc::new(ManagerInner {
                options,
                authorized_execution,
                processes,
                probe: Arc::new(probe),
                factory: Arc::new(factory),
                state: Mutex::new(ManagerState::default()),
                availability: Arc::new(Notify::new()),
                resolved_binary: Mutex::new(None),
                schema: Mutex::new(None),
                schema_flight: Mutex::new(None),
                sweep_task: Mutex::new(None),
            }),
        })
    }

    pub fn new_with_adapters(
        options: ManagerOptions,
        probe: P,
        factory: F,
    ) -> Result<Self, ManagerError> {
        Self::with_adapters(options, probe, factory)
    }

    pub fn is_closing(&self) -> bool {
        self.inner
            .state
            .lock()
            .map(|state| state.closing)
            .unwrap_or(true)
    }

    pub fn instance_count(&self) -> usize {
        self.inner
            .state
            .lock()
            .map(|state| state.instances.len())
            .unwrap_or(0)
    }

    pub fn active_lease_count(&self) -> usize {
        self.inner
            .state
            .lock()
            .map(|state| {
                state
                    .instances
                    .values()
                    .map(|instance| instance.active.load(Ordering::Acquire))
                    .sum()
            })
            .unwrap_or(0)
    }

    pub async fn acquire(&self, root: impl AsRef<Path>) -> Result<ClientRef, ManagerError> {
        self.acquire_optional(root, None).await
    }

    pub async fn acquire_with_cancellation(
        &self,
        root: impl AsRef<Path>,
        cancellation: CancellationToken,
    ) -> Result<ClientRef, ManagerError> {
        self.acquire_optional(root, Some(cancellation)).await
    }

    async fn acquire_optional(
        &self,
        root: impl AsRef<Path>,
        cancellation: Option<CancellationToken>,
    ) -> Result<ClientRef, ManagerError> {
        let (root, authority) = self.authorize_path_root(root.as_ref())?;
        self.acquire_authorized_optional(root, authority, cancellation)
            .await
    }

    /// Acquires a client using an already-authorized workspace capability.
    /// The exact requested directory is reopened through that capability so a
    /// lexical root cannot be rebound to a replacement directory mid-request.
    pub async fn acquire_authorized(
        &self,
        authority: Arc<AuthorizedRoot>,
        root: impl AsRef<Path>,
    ) -> Result<ClientRef, ManagerError> {
        self.require_authorized_execution()?;
        let (root, authority) = authorize_exact_root(&authority, root.as_ref())?;
        self.acquire_authorized_optional(root, authority, None)
            .await
    }

    pub async fn acquire_authorized_with_cancellation(
        &self,
        authority: Arc<AuthorizedRoot>,
        root: impl AsRef<Path>,
        cancellation: CancellationToken,
    ) -> Result<ClientRef, ManagerError> {
        self.require_authorized_execution()?;
        let (root, authority) = authorize_exact_root(&authority, root.as_ref())?;
        self.acquire_authorized_optional(root, authority, Some(cancellation))
            .await
    }

    async fn acquire_authorized_optional(
        &self,
        root: PathBuf,
        authority: Arc<AuthorizedRoot>,
        cancellation: Option<CancellationToken>,
    ) -> Result<ClientRef, ManagerError> {
        self.ensure_sweeper();
        check_cancellation(cancellation.as_ref())?;
        self.acquire_canonical(
            root,
            authority,
            Instant::now() + self.inner.options.wait_timeout,
            cancellation,
        )
        .await
    }

    fn authorize_path_root(
        &self,
        root: &Path,
    ) -> Result<(PathBuf, Arc<AuthorizedRoot>), ManagerError> {
        let root = normalize::canonical_workspace_path(root)?;
        let guard = RootGuard::new([root.clone()], std::iter::empty())
            .map_err(|error| ManagerError::RootAuthority(error.to_string()))?;
        let authority = guard
            .configured_roots()
            .first()
            .cloned()
            .ok_or_else(|| ManagerError::RootAuthority("workspace root is empty".to_owned()))?;
        authorize_exact_root(&authority, &root)
    }

    pub async fn acquire_lease(&self, root: impl AsRef<Path>) -> Result<ClientLease, ManagerError> {
        self.acquire_lease_optional(root, None).await
    }

    pub async fn acquire_lease_with_cancellation(
        &self,
        root: impl AsRef<Path>,
        cancellation: CancellationToken,
    ) -> Result<ClientLease, ManagerError> {
        self.acquire_lease_optional(root, Some(cancellation)).await
    }

    async fn acquire_lease_optional(
        &self,
        root: impl AsRef<Path>,
        cancellation: Option<CancellationToken>,
    ) -> Result<ClientLease, ManagerError> {
        let (root, authority) = self.authorize_path_root(root.as_ref())?;
        self.acquire_lease_authorized_optional(root, authority, cancellation)
            .await
    }

    pub async fn acquire_lease_authorized(
        &self,
        authority: Arc<AuthorizedRoot>,
        root: impl AsRef<Path>,
    ) -> Result<ClientLease, ManagerError> {
        self.require_authorized_execution()?;
        let (root, authority) = authorize_exact_root(&authority, root.as_ref())?;
        self.acquire_lease_authorized_optional(root, authority, None)
            .await
    }

    pub async fn acquire_lease_authorized_with_cancellation(
        &self,
        authority: Arc<AuthorizedRoot>,
        root: impl AsRef<Path>,
        cancellation: CancellationToken,
    ) -> Result<ClientLease, ManagerError> {
        self.require_authorized_execution()?;
        let (root, authority) = authorize_exact_root(&authority, root.as_ref())?;
        self.acquire_lease_authorized_optional(root, authority, Some(cancellation))
            .await
    }

    async fn acquire_lease_authorized_optional(
        &self,
        root: PathBuf,
        authority: Arc<AuthorizedRoot>,
        cancellation: Option<CancellationToken>,
    ) -> Result<ClientLease, ManagerError> {
        self.ensure_sweeper();
        check_cancellation(cancellation.as_ref())?;
        let deadline = Instant::now() + self.inner.options.wait_timeout;
        loop {
            check_cancellation(cancellation.as_ref())?;
            if let Some(lease) = self.try_lease(&root, &authority, cancellation.as_ref())? {
                return Ok(lease);
            }
            let _ = self
                .acquire_canonical(
                    root.clone(),
                    authority.clone(),
                    deadline,
                    cancellation.clone(),
                )
                .await?;
        }
    }

    pub async fn with_client<T, O, Fut>(
        &self,
        root: impl AsRef<Path>,
        operation: O,
    ) -> Result<T, ManagerError>
    where
        O: FnOnce(ClientRef) -> Fut + Send,
        Fut: Future<Output = Result<T, LspError>> + Send,
        T: Send,
    {
        self.with_client_optional(root, None, operation).await
    }

    pub async fn with_client_with_cancellation<T, O, Fut>(
        &self,
        root: impl AsRef<Path>,
        cancellation: CancellationToken,
        operation: O,
    ) -> Result<T, ManagerError>
    where
        O: FnOnce(ClientRef) -> Fut + Send,
        Fut: Future<Output = Result<T, LspError>> + Send,
        T: Send,
    {
        self.with_client_optional(root, Some(cancellation), operation)
            .await
    }

    async fn with_client_optional<T, O, Fut>(
        &self,
        root: impl AsRef<Path>,
        cancellation: Option<CancellationToken>,
        operation: O,
    ) -> Result<T, ManagerError>
    where
        O: FnOnce(ClientRef) -> Fut + Send,
        Fut: Future<Output = Result<T, LspError>> + Send,
        T: Send,
    {
        let lease = self.acquire_lease_optional(root, cancellation).await?;
        let result = operation(lease.client())
            .await
            .map_err(manager_client_error);
        drop(lease);
        result
    }

    pub async fn with_client_authorized<T, O, Fut>(
        &self,
        authority: Arc<AuthorizedRoot>,
        root: impl AsRef<Path>,
        operation: O,
    ) -> Result<T, ManagerError>
    where
        O: FnOnce(ClientRef) -> Fut + Send,
        Fut: Future<Output = Result<T, LspError>> + Send,
        T: Send,
    {
        self.with_client_authorized_optional(authority, root.as_ref(), None, operation)
            .await
    }

    pub async fn with_client_authorized_with_cancellation<T, O, Fut>(
        &self,
        authority: Arc<AuthorizedRoot>,
        root: impl AsRef<Path>,
        cancellation: CancellationToken,
        operation: O,
    ) -> Result<T, ManagerError>
    where
        O: FnOnce(ClientRef) -> Fut + Send,
        Fut: Future<Output = Result<T, LspError>> + Send,
        T: Send,
    {
        self.with_client_authorized_optional(
            authority,
            root.as_ref(),
            Some(cancellation),
            operation,
        )
        .await
    }

    async fn with_client_authorized_optional<T, O, Fut>(
        &self,
        authority: Arc<AuthorizedRoot>,
        root: &Path,
        cancellation: Option<CancellationToken>,
        operation: O,
    ) -> Result<T, ManagerError>
    where
        O: FnOnce(ClientRef) -> Fut + Send,
        Fut: Future<Output = Result<T, LspError>> + Send,
        T: Send,
    {
        self.require_authorized_execution()?;
        let lease = match cancellation.clone() {
            Some(cancellation) => {
                self.acquire_lease_authorized_with_cancellation(authority, root, cancellation)
                    .await?
            }
            None => self.acquire_lease_authorized(authority, root).await?,
        };
        let result = operation(lease.client())
            .await
            .map_err(manager_client_error);
        drop(lease);
        result
    }

    pub async fn supports_internal(
        &self,
        root: impl AsRef<Path>,
        capability: &str,
    ) -> Result<bool, ManagerError> {
        self.supports_internal_optional(root, capability, None)
            .await
    }

    pub async fn supports_internal_with_cancellation(
        &self,
        root: impl AsRef<Path>,
        capability: &str,
        cancellation: CancellationToken,
    ) -> Result<bool, ManagerError> {
        self.supports_internal_optional(root, capability, Some(cancellation))
            .await
    }

    async fn supports_internal_optional(
        &self,
        root: impl AsRef<Path>,
        capability: &str,
        cancellation: Option<CancellationToken>,
    ) -> Result<bool, ManagerError> {
        check_cancellation(cancellation.as_ref())?;
        let root = normalize::canonical_workspace_path(root.as_ref())?;
        let lease = self
            .acquire_lease_optional(&root, cancellation.clone())
            .await?;
        let state = self
            .inner
            .state
            .lock()
            .map_err(|_| ManagerError::Poisoned)?;
        let supported = state.instances.get(&root).is_some_and(|instance| {
            Arc::ptr_eq(instance, &lease.instance)
                && capability_enabled(instance.capabilities, capability)
        });
        drop(state);
        drop(lease);
        check_cancellation(cancellation.as_ref())?;
        Ok(supported)
    }

    pub async fn close_all(&self) -> CloseReport {
        self.ensure_sweeper();
        let requested = {
            let Ok(mut state) = self.inner.state.lock() else {
                return CloseReport::default();
            };
            state.closing = true;
            state.instances.len() + state.starts.len()
        };
        self.inner.availability.notify_waiters();
        if let Ok(mut task) = self.inner.sweep_task.lock() {
            if let Some(task) = task.take() {
                task.abort();
            }
        }

        let deadline = Instant::now() + self.close_budget();
        loop {
            let notified = self.inner.availability.notified();
            let starts = self
                .inner
                .state
                .lock()
                .map(|state| state.starts.values().cloned().collect::<Vec<_>>())
                .unwrap_or_default();
            for flight in starts {
                let _ = wait_for_start(&flight, deadline, None).await;
            }

            let instances = self
                .inner
                .state
                .lock()
                .map(|state| state.instances.values().cloned().collect::<Vec<_>>())
                .unwrap_or_default();
            for instance in instances {
                let lease_deadline = deadline
                    .checked_sub(self.shutdown_budget())
                    .unwrap_or(deadline);
                let force = Instant::now() >= lease_deadline;
                if instance.active.load(Ordering::Acquire) == 0 || force {
                    let _ = self.stop_instance_until(instance, deadline, force).await;
                }
            }
            let remaining = self
                .inner
                .state
                .lock()
                .map(|state| state.instances.len() + state.starts.len())
                .unwrap_or(requested);
            if remaining == 0 || Instant::now() >= deadline {
                return CloseReport {
                    requested,
                    completed: requested.saturating_sub(remaining),
                    remaining,
                };
            }
            let _ = self
                .wait_for_availability_with_cancellation(deadline, None, notified)
                .await;
        }
    }

    async fn acquire_canonical(
        &self,
        root: PathBuf,
        authority: Arc<AuthorizedRoot>,
        deadline: Instant,
        cancellation: Option<CancellationToken>,
    ) -> Result<ClientRef, ManagerError> {
        loop {
            check_cancellation(cancellation.as_ref())?;
            enum Action {
                Start(Arc<StartFlight>),
                WaitStart(Arc<StartFlight>),
                WaitStop(Arc<StopState>),
                Stop(Arc<Instance>),
                Return(ClientRef),
            }

            let action = {
                let mut state = self
                    .inner
                    .state
                    .lock()
                    .map_err(|_| ManagerError::Poisoned)?;
                if state.closing {
                    return Err(ManagerError::Closing);
                }
                if let Some(instance) = state.instances.get(&root).cloned() {
                    if instance.stopping.load(Ordering::Acquire) {
                        Action::WaitStop(instance.stop.clone())
                    } else if instance.client.is_closed() {
                        Action::Stop(instance)
                    } else if !authorities_match(&instance.root_authority, &authority)? {
                        return Err(ManagerError::RootAuthority(
                            "a different physical directory now occupies the requested root"
                                .to_owned(),
                        ));
                    } else {
                        instance.touch();
                        Action::Return(instance.client.clone())
                    }
                } else if let Some(flight) = state.starts.get(&root).cloned() {
                    Action::WaitStart(flight)
                } else {
                    let flight = Arc::new(StartFlight::new());
                    state.starts.insert(root.clone(), flight.clone());
                    Action::Start(flight)
                }
            };

            match action {
                Action::Return(client) => return Ok(client),
                Action::WaitStop(stop) => {
                    if !stop
                        .wait_until_with_cancellation(deadline, cancellation.as_ref())
                        .await?
                    {
                        return Err(ManagerError::WaitTimeout);
                    }
                }
                Action::Stop(instance) => {
                    let _ = self.stop_instance(instance).await;
                }
                Action::WaitStart(flight) => {
                    match wait_for_start(&flight, deadline, cancellation.as_ref()).await {
                        Ok(result) => {
                            if result.client.is_closed() {
                                continue;
                            }
                            if !authorities_match(&result.root_authority, &authority)? {
                                return Err(ManagerError::RootAuthority(
                                    "the active startup belongs to a different physical directory"
                                        .to_owned(),
                                ));
                            }
                            return Ok(result.client.clone());
                        }
                        Err(ManagerError::Cancelled) => {
                            // A cancelled owner must not cancel an unrelated waiter.
                            check_cancellation(cancellation.as_ref())?;
                            continue;
                        }
                        Err(error) => return Err(error),
                    }
                }
                Action::Start(flight) => {
                    let result = self
                        .start_owner(
                            root.clone(),
                            authority.clone(),
                            flight,
                            cancellation.clone(),
                        )
                        .await?;
                    if result.client.is_closed() {
                        continue;
                    }
                    if !authorities_match(&result.root_authority, &authority)? {
                        return Err(ManagerError::RootAuthority(
                            "the startup belongs to a different physical directory".to_owned(),
                        ));
                    }
                    return Ok(result.client.clone());
                }
            }
        }
    }

    fn try_lease(
        &self,
        root: &Path,
        authority: &Arc<AuthorizedRoot>,
        cancellation: Option<&CancellationToken>,
    ) -> Result<Option<ClientLease>, ManagerError> {
        check_cancellation(cancellation)?;
        let state = self
            .inner
            .state
            .lock()
            .map_err(|_| ManagerError::Poisoned)?;
        if state.closing {
            return Err(ManagerError::Closing);
        }
        let Some(instance) = state.instances.get(root).cloned() else {
            return Ok(None);
        };
        if instance.stopping.load(Ordering::Acquire) || instance.client.is_closed() {
            return Ok(None);
        }
        if !authorities_match(&instance.root_authority, authority)? {
            return Err(ManagerError::RootAuthority(
                "a different physical directory now occupies the requested root".to_owned(),
            ));
        }
        instance.active.fetch_add(1, Ordering::AcqRel);
        instance.touch();
        if let Err(error) = check_cancellation(cancellation) {
            instance.active.fetch_sub(1, Ordering::AcqRel);
            instance.touch();
            self.inner.availability.notify_waiters();
            return Err(error);
        }
        Ok(Some(ClientLease {
            instance,
            availability: self.inner.availability.clone(),
        }))
    }

    async fn start_owner(
        &self,
        root: PathBuf,
        authority: Arc<AuthorizedRoot>,
        flight: Arc<StartFlight>,
        cancellation: Option<CancellationToken>,
    ) -> Result<Arc<Instance>, ManagerError> {
        let result = self.start_instance(&root, authority, cancellation).await;
        self.complete_start(&root, &flight, result.clone());
        result
    }

    async fn start_instance(
        &self,
        root: &Path,
        authority: Arc<AuthorizedRoot>,
        cancellation: Option<CancellationToken>,
    ) -> Result<Arc<Instance>, ManagerError> {
        check_cancellation(cancellation.as_ref())?;
        let binary = self.resolved_binary()?;
        if self.inner.options.workspace_code == WorkspaceCode::Deny {
            self.ensure_schema(&binary, authority.clone(), cancellation.clone())
                .await?;
        }
        // Keep the capacity wait state out of every nested navigation future.
        Box::pin(self.ensure_capacity(root, cancellation.clone())).await?;
        check_cancellation(cancellation.as_ref())?;
        let protocol_root = self.protocol_root_for(root);
        let spec = CommandSpec {
            executable: binary,
            args: Vec::new(),
            cwd: root.to_owned(),
            env: fixed_environment(),
        };
        let client = if self.inner.authorized_execution {
            self.inner
                .factory
                .spawn_authorized(
                    spec,
                    self.inner.options.timeout,
                    self.inner.options.max_frame_bytes,
                    cancellation.clone(),
                    authority.clone(),
                )
                .await
        } else {
            self.inner
                .factory
                .spawn_with_cancellation(
                    spec,
                    self.inner.options.timeout,
                    self.inner.options.max_frame_bytes,
                    cancellation.clone(),
                )
                .await
        }
        .map_err(manager_client_error)?;
        if let Err(error) = check_cancellation(cancellation.as_ref()) {
            let _ = client.shutdown(self.inner.options.shutdown_timeout).await;
            return Err(error);
        }
        let token = Arc::new(());
        let callbacks = self.callbacks(root, &protocol_root, token.clone());
        if let Err(error) = client
            .set_callbacks_with_cancellation(callbacks, cancellation.clone())
            .await
        {
            let _ = client.shutdown(self.inner.options.shutdown_timeout).await;
            return Err(manager_client_error(error));
        }
        check_cancellation_or_shutdown(&cancellation, &client, self.inner.options.shutdown_timeout)
            .await?;
        let initialized = match client
            .request_with_cancellation(
                "initialize",
                initialize_params(&protocol_root, self.inner.options.workspace_code)?,
                self.inner.options.timeout,
                cancellation.clone(),
            )
            .await
        {
            Ok(value) => value,
            Err(error) => {
                let _ = client.shutdown(self.inner.options.shutdown_timeout).await;
                return Err(manager_client_error(error));
            }
        };
        if let Err(error) = check_cancellation(cancellation.as_ref()) {
            let _ = client.shutdown(self.inner.options.shutdown_timeout).await;
            return Err(error);
        }
        if client.is_closed() {
            let _ = client.shutdown(self.inner.options.shutdown_timeout).await;
            return Err(ManagerError::Client(LspError::Closed(
                "rust-analyzer closed during initialize".to_owned(),
            )));
        }
        if let Err(error) = client
            .set_document_sync_with_cancellation(
                document_sync_options(&initialized),
                cancellation.clone(),
            )
            .await
        {
            let _ = client.shutdown(self.inner.options.shutdown_timeout).await;
            return Err(manager_client_error(error));
        }
        if let Err(error) = client
            .notify_with_cancellation("initialized", json!({}), cancellation.clone())
            .await
        {
            let _ = client.shutdown(self.inner.options.shutdown_timeout).await;
            return Err(manager_client_error(error));
        }
        check_cancellation_or_shutdown(&cancellation, &client, self.inner.options.shutdown_timeout)
            .await?;
        Ok(Arc::new(Instance::new(
            root.to_owned(),
            authority,
            client,
            token,
            internal_capabilities(&initialized),
        )))
    }

    fn complete_start(
        &self,
        root: &Path,
        flight: &Arc<StartFlight>,
        result: Result<Arc<Instance>, ManagerError>,
    ) {
        let mut stop_after_insert = None;
        if let Ok(mut state) = self.inner.state.lock() {
            if let Ok(instance) = &result {
                state.instances.insert(root.to_owned(), instance.clone());
                if state.closing {
                    stop_after_insert = Some(instance.clone());
                }
            }
            if state
                .starts
                .get(root)
                .is_some_and(|current| Arc::ptr_eq(current, flight))
            {
                state.starts.remove(root);
            }
        }
        if let Ok(mut slot) = flight.result.lock() {
            *slot = Some(result);
        }
        flight.notify.notify_waiters();
        self.inner.availability.notify_waiters();
        if let Some(instance) = stop_after_insert {
            let manager = self.clone();
            tokio::spawn(async move {
                manager.stop_instance(instance).await;
            });
        }
    }

    fn resolved_binary(&self) -> Result<PathBuf, ManagerError> {
        let mut cache = self
            .inner
            .resolved_binary
            .lock()
            .map_err(|_| ManagerError::Poisoned)?;
        if let Some(result) = &*cache {
            return result.clone();
        }
        let result = resolve_binary_path(self.inner.options.binary.as_deref())
            .map_err(ManagerError::Workspace);
        *cache = Some(result.clone());
        result
    }

    async fn ensure_schema(
        &self,
        binary: &Path,
        authority: Arc<AuthorizedRoot>,
        cancellation: Option<CancellationToken>,
    ) -> Result<(), ManagerError> {
        loop {
            check_cancellation(cancellation.as_ref())?;
            let binary = binary.to_owned();
            let (flight, owner) = {
                let cache = self
                    .inner
                    .schema
                    .lock()
                    .map_err(|_| ManagerError::Poisoned)?;
                if let Some((cached_binary, result)) = &*cache {
                    if cached_binary == &binary {
                        return result.clone();
                    }
                }
                let mut flight_slot = self
                    .inner
                    .schema_flight
                    .lock()
                    .map_err(|_| ManagerError::Poisoned)?;
                if let Some((active_binary, flight)) = &*flight_slot {
                    if active_binary == &binary {
                        (flight.clone(), false)
                    } else {
                        let flight = Arc::new(SchemaFlight::new());
                        *flight_slot = Some((binary.clone(), flight.clone()));
                        (flight, true)
                    }
                } else {
                    let flight = Arc::new(SchemaFlight::new());
                    *flight_slot = Some((binary.clone(), flight.clone()));
                    (flight, true)
                }
            };

            if !owner {
                match wait_for_schema(
                    &flight,
                    Instant::now() + self.inner.options.wait_timeout,
                    cancellation.as_ref(),
                )
                .await
                {
                    Ok(()) => return Ok(()),
                    Err(ManagerError::Cancelled) => {
                        // A request-local probe cancellation should not poison waiters.
                        check_cancellation(cancellation.as_ref())?;
                        continue;
                    }
                    Err(error) => return Err(error),
                }
            }

            let result = if self.inner.authorized_execution {
                self.inner
                    .probe
                    .probe_authorized_with_supervisor(
                        &binary,
                        self.inner.options.probe_timeout,
                        cancellation.clone(),
                        authority,
                        self.inner.processes.clone(),
                    )
                    .await
            } else {
                self.inner
                    .probe
                    .probe_with_cancellation(
                        &binary,
                        self.inner.options.probe_timeout,
                        cancellation.clone(),
                    )
                    .await
            }
                .map_err(|error| match error {
                    ProbeError::Cancelled => ManagerError::Cancelled,
                    error => ManagerError::Unavailable(error.to_string()),
                })
                .and_then(|schema| {
                    if schema.supports_workspace_code_deny() {
                        Ok(())
                    } else {
                        Err(ManagerError::Unavailable(
                            "schema did not verify build-script, procedural-macro, and check-on-save disable keys"
                                .to_owned(),
                        ))
                    }
                });
            if !matches!(&result, Err(ManagerError::Cancelled)) {
                if let Ok(mut cache) = self.inner.schema.lock() {
                    *cache = Some((binary.clone(), result.clone()));
                }
            }
            if let Ok(mut slot) = self.inner.schema_flight.lock() {
                if slot
                    .as_ref()
                    .is_some_and(|(active_binary, _)| active_binary == &binary)
                {
                    *slot = None;
                }
            }
            if let Ok(mut slot) = flight.result.lock() {
                *slot = Some(result.clone());
            }
            flight.notify.notify_waiters();
            return result;
        }
    }

    async fn ensure_capacity(
        &self,
        root: &Path,
        cancellation: Option<CancellationToken>,
    ) -> Result<(), ManagerError> {
        let deadline = Instant::now() + self.inner.options.wait_timeout;
        loop {
            let notified = self.inner.availability.notified();
            check_cancellation(cancellation.as_ref())?;
            enum CapacityAction {
                Ready,
                Stop(Arc<Instance>),
                Wait,
            }

            let action = {
                let state = self
                    .inner
                    .state
                    .lock()
                    .map_err(|_| ManagerError::Poisoned)?;
                if state.closing {
                    return Err(ManagerError::Closing);
                }
                let reserved_elsewhere = state
                    .instances
                    .len()
                    .saturating_add(state.starts.len())
                    .saturating_sub(1);
                let start_rank = state.starts.keys().position(|candidate| candidate == root);
                let startup_slot = state.instances.is_empty()
                    && start_rank.is_some_and(|rank| rank < self.inner.options.max_instances);
                if reserved_elsewhere < self.inner.options.max_instances || startup_slot {
                    CapacityAction::Ready
                } else {
                    let candidate = state
                        .instances
                        .iter()
                        .filter(|(candidate_root, instance)| {
                            *candidate_root != root
                                && instance.active.load(Ordering::Acquire) == 0
                                && !instance.stopping.load(Ordering::Acquire)
                        })
                        .min_by_key(|(_, instance)| instance.last_used())
                        .map(|(_, instance)| instance.clone());
                    candidate.map_or(CapacityAction::Wait, CapacityAction::Stop)
                }
            };
            match action {
                CapacityAction::Ready => return Ok(()),
                CapacityAction::Stop(instance) => {
                    self.stop_instance_until(instance, deadline, false).await;
                }
                CapacityAction::Wait => {
                    if !self
                        .wait_for_availability_with_cancellation(
                            deadline,
                            cancellation.as_ref(),
                            notified,
                        )
                        .await?
                    {
                        return Err(ManagerError::WaitTimeout);
                    }
                }
            }
        }
    }

    async fn stop_instance(&self, instance: Arc<Instance>) {
        let _ = self
            .stop_instance_until(instance, Instant::now() + self.shutdown_budget(), false)
            .await;
    }

    async fn stop_instance_until(
        &self,
        instance: Arc<Instance>,
        deadline: Instant,
        force: bool,
    ) -> bool {
        if self.mark_stopping(&instance, force) {
            self.stop_marked_until(instance, deadline).await
        } else {
            instance.stop.wait_until(deadline).await
        }
    }

    fn mark_stopping(&self, instance: &Arc<Instance>, force: bool) -> bool {
        let Ok(state) = self.inner.state.lock() else {
            return false;
        };
        let Some(current) = state.instances.get(&instance.root) else {
            return false;
        };
        if !Arc::ptr_eq(current, instance)
            || current.stopping.load(Ordering::Acquire)
            || (!force && current.active.load(Ordering::Acquire) != 0)
        {
            return false;
        }
        current.stopping.store(true, Ordering::Release);
        true
    }

    async fn stop_marked_until(&self, instance: Arc<Instance>, deadline: Instant) -> bool {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return false;
        }
        let completed = match time::timeout(
            remaining,
            instance
                .client
                .shutdown(self.inner.options.shutdown_timeout),
        )
        .await
        {
            Ok(Ok(())) => true,
            Ok(Err(_)) => instance.client.is_closed(),
            Err(_) => false,
        };
        if !completed {
            return false;
        }
        if let Ok(mut state) = self.inner.state.lock() {
            if state
                .instances
                .get(&instance.root)
                .is_some_and(|current| Arc::ptr_eq(current, &instance))
            {
                state.instances.remove(&instance.root);
            }
        }
        instance.stop.finish();
        self.inner.availability.notify_waiters();
        true
    }

    async fn sweep_idle(&self) {
        let now = Instant::now();
        let candidates = {
            let Ok(state) = self.inner.state.lock() else {
                return;
            };
            if state.closing {
                return;
            }
            state
                .instances
                .values()
                .filter_map(|instance| {
                    let eligible = instance.active.load(Ordering::Acquire) == 0
                        && !instance.stopping.load(Ordering::Acquire)
                        && now.saturating_duration_since(instance.last_used())
                            >= self.inner.options.idle;
                    eligible.then(|| instance.clone())
                })
                .collect::<Vec<_>>()
        };
        for instance in candidates {
            let manager = RustAnalyzerManager {
                inner: Arc::clone(&self.inner),
            };
            tokio::spawn(async move {
                manager
                    .stop_instance_until(
                        instance,
                        Instant::now() + manager.shutdown_budget(),
                        false,
                    )
                    .await;
            });
        }
    }

    pub(crate) fn protocol_root_for(&self, lexical: &Path) -> PathBuf {
        if self.inner.authorized_execution {
            self.inner.factory.protocol_root(lexical)
        } else {
            lexical.to_owned()
        }
    }

    fn require_authorized_execution(&self) -> Result<(), ManagerError> {
        if self.inner.authorized_execution {
            Ok(())
        } else {
            Err(ManagerError::RootAuthority(
                "this manager was constructed for direct public execution".to_owned(),
            ))
        }
    }

    fn callbacks(&self, root: &Path, protocol_root: &Path, token: Arc<()>) -> ClientCallbacks {
        let root_for_request = protocol_root.to_owned();
        let deny = self.inner.options.workspace_code == WorkspaceCode::Deny;
        let server_request_handler: ServerRequestHandler =
            Arc::new(move |method, params, _cancel| {
                let root = root_for_request.clone();
                Box::pin(async move {
                    if method == "workspace/workspaceFolders" {
                        let uri = path_to_file_uri(&root)
                            .map_err(|error| LspError::SchemaProbe(error.to_string()))?;
                        return Ok(json!([{"uri": uri, "name": "workspace"}]));
                    }
                    if method == "workspace/configuration" {
                        return Ok(configuration_value(&method, &params, deny));
                    }
                    client::default_server_request(&method, &params)
                })
            });
        let weak_inner = Arc::downgrade(&self.inner);
        let root_for_close = root.to_owned();
        let close_handler: CloseHandler = Arc::new(move || {
            let Some(inner) = weak_inner.upgrade() else {
                return;
            };
            let Ok(mut state) = inner.state.lock() else {
                return;
            };
            let removed = state
                .instances
                .get(&root_for_close)
                .filter(|instance| Arc::ptr_eq(&instance.token, &token))
                .cloned();
            if let Some(instance) = removed {
                state.instances.remove(&root_for_close);
                instance.stop.finish();
                inner.availability.notify_waiters();
            }
        });
        ClientCallbacks {
            server_request_handler: Some(server_request_handler),
            notification_handler: None,
            close_handler: Some(close_handler),
        }
    }

    fn ensure_sweeper(&self) {
        let Ok(mut task_slot) = self.inner.sweep_task.lock() else {
            return;
        };
        if task_slot.is_some() {
            return;
        }
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let weak = Arc::downgrade(&self.inner);
        let interval = self
            .inner
            .options
            .idle
            .checked_div(2)
            .unwrap_or_else(|| Duration::from_millis(1))
            .max(Duration::from_millis(1))
            .min(Duration::from_secs(60));
        *task_slot = Some(handle.spawn(async move {
            let mut ticker = time::interval(interval);
            loop {
                ticker.tick().await;
                let Some(inner) = weak.upgrade() else {
                    return;
                };
                let manager = RustAnalyzerManager { inner };
                manager.sweep_idle().await;
                if manager.is_closing() {
                    return;
                }
            }
        }));
    }

    async fn wait_for_availability_with_cancellation(
        &self,
        deadline: Instant,
        cancellation: Option<&CancellationToken>,
        notified: tokio::sync::futures::Notified<'_>,
    ) -> Result<bool, ManagerError> {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Ok(false);
        }
        if let Some(cancellation) = cancellation {
            tokio::select! {
                result = time::timeout(remaining, notified) => Ok(result.is_ok()),
                () = cancellation.cancelled() => Err(ManagerError::Cancelled),
            }
        } else {
            Ok(time::timeout(remaining, notified).await.is_ok())
        }
    }

    fn shutdown_budget(&self) -> Duration {
        self.inner
            .options
            .shutdown_timeout
            .checked_mul(4)
            .unwrap_or(Duration::MAX)
    }

    fn close_budget(&self) -> Duration {
        self.inner
            .options
            .wait_timeout
            .checked_add(self.shutdown_budget())
            .unwrap_or(Duration::MAX)
    }
}

pub struct ClientLease {
    instance: Arc<Instance>,
    availability: Arc<Notify>,
}

impl fmt::Debug for ClientLease {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ClientLease")
            .field("root", &self.instance.root)
            .field("active", &self.instance.active.load(Ordering::Acquire))
            .finish()
    }
}

impl ClientLease {
    pub fn client(&self) -> ClientRef {
        self.instance.client.clone()
    }

    pub fn root(&self) -> &Path {
        &self.instance.root
    }
}

impl Drop for ClientLease {
    fn drop(&mut self) {
        let previous =
            self.instance
                .active
                .fetch_update(Ordering::AcqRel, Ordering::Acquire, |active| {
                    active.checked_sub(1)
                });
        if previous.is_ok() {
            self.instance.touch();
            self.availability.notify_waiters();
        }
    }
}

fn check_cancellation(cancellation: Option<&CancellationToken>) -> Result<(), ManagerError> {
    if cancellation.is_some_and(CancellationToken::is_cancelled) {
        return Err(ManagerError::Cancelled);
    }
    Ok(())
}

fn manager_client_error(error: LspError) -> ManagerError {
    if matches!(error, LspError::Cancelled) {
        ManagerError::Cancelled
    } else {
        ManagerError::Client(error)
    }
}

fn authorize_exact_root(
    authority: &Arc<AuthorizedRoot>,
    root: &Path,
) -> Result<(PathBuf, Arc<AuthorizedRoot>), ManagerError> {
    let exact = authority
        .authorize_dir(root)
        .map_err(|error| ManagerError::RootAuthority(error.to_string()))?;
    Ok((exact.path().to_owned(), exact))
}

// Preserve the shared fallible interface on platforms with a no-op implementation.
#[cfg_attr(not(unix), allow(clippy::unnecessary_wraps))]
fn authorities_match(left: &AuthorizedRoot, right: &AuthorizedRoot) -> Result<bool, ManagerError> {
    if left.path() != right.path() {
        return Ok(false);
    }
    #[cfg(unix)]
    {
        use cap_std::fs::MetadataExt;

        let left = left
            .dir()
            .dir_metadata()
            .map_err(|error| ManagerError::RootAuthority(error.to_string()))?;
        let right = right
            .dir()
            .dir_metadata()
            .map_err(|error| ManagerError::RootAuthority(error.to_string()))?;
        Ok(left.dev() == right.dev() && left.ino() == right.ino())
    }
    #[cfg(not(unix))]
    {
        // The capability directory handle remains live for every instance. On
        // Windows that open handle prevents a replacement at this lexical path;
        // rejecting a path mismatch above is therefore fail-closed.
        Ok(true)
    }
}

async fn check_cancellation_or_shutdown(
    cancellation: &Option<CancellationToken>,
    client: &ClientRef,
    timeout: Duration,
) -> Result<(), ManagerError> {
    if let Err(error) = check_cancellation(cancellation.as_ref()) {
        let _ = client.shutdown(timeout).await;
        return Err(error);
    }
    Ok(())
}

async fn wait_for_start(
    flight: &StartFlight,
    deadline: Instant,
    cancellation: Option<&CancellationToken>,
) -> Result<Arc<Instance>, ManagerError> {
    loop {
        let notified = flight.notify.notified();
        if let Some(result) = flight
            .result
            .lock()
            .map_err(|_| ManagerError::Poisoned)?
            .clone()
        {
            return result;
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(ManagerError::WaitTimeout);
        }
        if let Some(cancellation) = cancellation {
            tokio::select! {
                result = time::timeout(remaining, notified) => {
                    if result.is_err() {
                        return Err(ManagerError::WaitTimeout);
                    }
                }
                () = cancellation.cancelled() => return Err(ManagerError::Cancelled),
            }
        } else if time::timeout(remaining, notified).await.is_err() {
            return Err(ManagerError::WaitTimeout);
        }
    }
}

async fn wait_for_schema(
    flight: &SchemaFlight,
    deadline: Instant,
    cancellation: Option<&CancellationToken>,
) -> Result<(), ManagerError> {
    loop {
        let notified = flight.notify.notified();
        if let Some(result) = flight
            .result
            .lock()
            .map_err(|_| ManagerError::Poisoned)?
            .clone()
        {
            return result;
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(ManagerError::WaitTimeout);
        }
        if let Some(cancellation) = cancellation {
            tokio::select! {
                result = time::timeout(remaining, notified) => {
                    if result.is_err() {
                        return Err(ManagerError::WaitTimeout);
                    }
                }
                () = cancellation.cancelled() => return Err(ManagerError::Cancelled),
            }
        } else if time::timeout(remaining, notified).await.is_err() {
            return Err(ManagerError::WaitTimeout);
        }
    }
}

fn initialize_params(root: &Path, workspace_code: WorkspaceCode) -> Result<Value, ManagerError> {
    let uri = path_to_file_uri(root).map_err(ManagerError::Workspace)?;
    let initialization_options = if workspace_code == WorkspaceCode::Deny {
        json!({
            "checkOnSave": false,
            "cargo": {"buildScripts": {"enable": false}},
            "procMacro": {"enable": false}
        })
    } else {
        json!({"checkOnSave": false})
    };
    Ok(json!({
        "processId": std::process::id(),
        "rootUri": uri,
        "initializationOptions": initialization_options,
        "capabilities": {
            "general": {"positionEncodings": ["utf-16"]},
            "textDocument": {
                "documentSymbol": {"hierarchicalDocumentSymbolSupport": true},
                "hover": {"contentFormat": ["markdown", "plaintext"]},
                "references": {"linkSupport": false},
                "definition": {"linkSupport": true},
                "implementation": {"linkSupport": true},
                "callHierarchy": {},
                "rename": {"prepareSupport": true, "prepareSupportDefaultBehavior": 1},
                "codeAction": {
                    "codeActionLiteralSupport": {
                        "codeActionKind": {"valueSet": [
                            "", "quickfix", "refactor", "refactor.extract",
                            "refactor.inline", "refactor.rewrite", "source",
                            "source.organizeImports", "source.fixAll"
                        ]}
                    },
                    "isPreferredSupport": true,
                    "disabledSupport": true
                }
            },
            "workspace": {"workspaceFolders": true, "workspaceEdit": {"documentChanges": true}}
        },
        "workspaceFolders": [{"uri": uri, "name": "workspace"}]
    }))
}

fn capability_enabled(capabilities: RustAnalyzerCapabilities, name: &str) -> bool {
    match name {
        "expandMacro" | "rust-analyzer/expandMacro" => capabilities.expand_macro,
        "relatedTests" | "rust-analyzer/relatedTests" => capabilities.related_tests,
        "failedObligations" | "rust-analyzer/getFailedObligations" => {
            capabilities.failed_obligations
        }
        "openDocs" | "rust-analyzer/openDocs" => capabilities.open_docs,
        _ => false,
    }
}

fn nonzero_duration(duration: Duration) -> Duration {
    if duration.is_zero() {
        Duration::from_millis(1)
    } else {
        duration
    }
}
