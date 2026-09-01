//! A bounded JSON-RPC/LSP client for a rust-analyzer stdio process.
//!
//! The client deliberately does not use an LSP crate.  Rust Analyzer output is
//! untrusted input, so framing and message validation happen before any value is
//! dispatched to a caller.

use std::{
    collections::HashMap,
    pin::Pin,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
    },
    time::Duration,
};

#[cfg(windows)]
use process_wrap::tokio::JobObject;
#[cfg(unix)]
use process_wrap::tokio::ProcessGroup;
use process_wrap::tokio::{ChildWrapper, CommandWrap, KillOnDrop};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Number, Value, json};
use thiserror::Error;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    process::{ChildStderr, ChildStdin, ChildStdout},
    sync::{Mutex, Notify, OwnedMutexGuard, RwLock, mpsc, oneshot},
    time,
};
use tokio_util::sync::CancellationToken;

use crate::process::CommandSpec;

pub const DEFAULT_MAX_FRAME_BYTES: usize = 16 * 1024 * 1024;
pub const DEFAULT_MAX_HEADER_BYTES: usize = 8 * 1024;
pub const DEFAULT_MAX_FRAMES_PER_READ: usize = 256;
pub const DEFAULT_MAX_STDERR_BYTES: usize = 2 * 1024;
pub const DEFAULT_MAX_DOCUMENTS: usize = 512;
pub const DEFAULT_MAX_PENDING_REQUESTS: usize = 1_024;
pub const DEFAULT_MAX_SERVER_REQUESTS: usize = 256;
pub const CONTENT_MODIFIED_CODE: i64 = -32801;
pub const REQUEST_CANCELLED_CODE: i64 = -32800;
pub const METHOD_NOT_FOUND_CODE: i64 = -32601;

#[derive(Debug, Error, Clone)]
pub enum LspError {
    #[error("invalid LSP input: {0}")]
    InvalidInput(String),
    #[error("semantic symbol was not found: {0}")]
    NotFound(String),
    #[error("semantic symbol is ambiguous: {0}")]
    Ambiguous(String),
    #[error("LSP frame error: {0}")]
    Frame(String),
    #[error("failed to spawn rust-analyzer: {0}")]
    Spawn(String),
    #[error("rust-analyzer initialize failed: {0}")]
    Initialize(String),
    #[error("rust-analyzer request '{method}' timed out after {timeout_ms}ms")]
    Timeout { method: String, timeout_ms: u64 },
    #[error("rust-analyzer request was cancelled")]
    Cancelled,
    #[error("rust-analyzer process is closed: {0}")]
    Closed(String),
    #[error("rust-analyzer response error {code:?}: {message}")]
    Response {
        code: Option<i64>,
        message: String,
        data: Option<Value>,
    },
    #[error("rust-analyzer schema probe failed: {0}")]
    SchemaProbe(String),
    #[error("LSP I/O failed: {0}")]
    Io(String),
}

impl LspError {
    pub fn code(&self) -> Option<i64> {
        match self {
            Self::Response { code, .. } => *code,
            Self::Cancelled => Some(REQUEST_CANCELLED_CODE),
            _ => None,
        }
    }

    pub fn is_content_modified(&self) -> bool {
        self.code() == Some(CONTENT_MODIFIED_CODE)
    }
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum FrameError {
    #[error("frame header exceeded the configured bound")]
    HeaderTooLarge,
    #[error("frame buffer exceeded the configured bound")]
    BufferTooLarge,
    #[error("frame is missing Content-Length")]
    MissingContentLength,
    #[error("invalid Content-Length")]
    InvalidContentLength,
    #[error("frame body exceeded the configured bound")]
    BodyTooLarge,
    #[error("frame body is not valid UTF-8")]
    InvalidUtf8,
    #[error("frame body is not valid JSON: {0}")]
    InvalidJson(String),
}

#[derive(Debug, Clone)]
pub struct FrameDecoder {
    buffer: Vec<u8>,
    discard_body: Option<usize>,
    max_body_bytes: usize,
    max_header_bytes: usize,
    max_buffer_bytes: usize,
    max_frames_per_push: usize,
}

impl Default for FrameDecoder {
    fn default() -> Self {
        Self::new(DEFAULT_MAX_FRAME_BYTES)
    }
}

impl FrameDecoder {
    pub fn new(max_body_bytes: usize) -> Self {
        let max_body_bytes = max_body_bytes.max(1);
        Self {
            buffer: Vec::new(),
            discard_body: None,
            max_body_bytes,
            max_header_bytes: DEFAULT_MAX_HEADER_BYTES,
            max_buffer_bytes: max_body_bytes.saturating_add(DEFAULT_MAX_HEADER_BYTES),
            max_frames_per_push: DEFAULT_MAX_FRAMES_PER_READ,
        }
    }

    pub fn with_limits(
        max_body_bytes: usize,
        max_header_bytes: usize,
        max_buffer_bytes: usize,
        max_frames_per_push: usize,
    ) -> Self {
        let max_body_bytes = max_body_bytes.max(1);
        let max_header_bytes = max_header_bytes.max(1);
        let max_buffer_bytes = max_buffer_bytes.max(max_header_bytes.saturating_add(1));
        Self {
            buffer: Vec::new(),
            discard_body: None,
            max_body_bytes,
            max_header_bytes,
            max_buffer_bytes,
            max_frames_per_push: max_frames_per_push.max(1),
        }
    }

    pub fn buffered_len(&self) -> usize {
        self.buffer.len()
    }

    pub fn push(&mut self, chunk: &[u8]) -> Vec<Result<Value, FrameError>> {
        let mut frames = Vec::new();
        let mut offset = 0;
        while offset < chunk.len() {
            self.decode_available(&mut frames);
            if frames.len() >= self.max_frames_per_push {
                let remaining = &chunk[offset..];
                let available = self.max_buffer_bytes.saturating_sub(self.buffer.len());
                if remaining.len() > available {
                    self.buffer.clear();
                    frames.push(Err(FrameError::BufferTooLarge));
                } else {
                    self.buffer.extend_from_slice(remaining);
                }
                return frames;
            }

            let available = self.max_buffer_bytes.saturating_sub(self.buffer.len());
            if available == 0 {
                if self.decode_available(&mut frames) {
                    continue;
                }
                self.buffer.clear();
                frames.push(Err(FrameError::BufferTooLarge));
                return frames;
            }

            let take = available.min(chunk.len() - offset);
            self.buffer.extend_from_slice(&chunk[offset..offset + take]);
            offset += take;
        }
        self.decode_available(&mut frames);
        frames
    }

    fn decode_available(&mut self, frames: &mut Vec<Result<Value, FrameError>>) -> bool {
        let mut progressed = false;
        while frames.len() < self.max_frames_per_push {
            if let Some(remaining) = self.discard_body {
                let discarded = remaining.min(self.buffer.len());
                if discarded != 0 {
                    self.buffer.drain(..discarded);
                    self.discard_body = Some(remaining - discarded);
                    progressed = true;
                }
                if self.discard_body.is_some_and(|remaining| remaining != 0) {
                    break;
                }
                self.discard_body = None;
                continue;
            }
            let Some(header_end) = find_header_end(&self.buffer) else {
                if self.buffer.len() > self.max_header_bytes {
                    self.buffer.clear();
                    frames.push(Err(FrameError::HeaderTooLarge));
                    progressed = true;
                }
                break;
            };
            let body_start = header_end + 4;
            if header_end > self.max_header_bytes {
                self.buffer.drain(..body_start);
                resync_header(&mut self.buffer);
                frames.push(Err(FrameError::HeaderTooLarge));
                progressed = true;
                continue;
            }

            let length = match parse_content_length(&self.buffer[..header_end]) {
                Ok(length) => length,
                Err(error) => {
                    self.buffer.drain(..body_start);
                    resync_header(&mut self.buffer);
                    frames.push(Err(error));
                    progressed = true;
                    continue;
                }
            };
            if length > self.max_body_bytes {
                let available_body = self.buffer.len().saturating_sub(body_start);
                if available_body >= length {
                    if let Some(body_end) = body_start.checked_add(length) {
                        self.buffer.drain(..body_end);
                    } else {
                        self.buffer.clear();
                    }
                    self.discard_body = None;
                } else {
                    self.buffer.clear();
                    self.discard_body = Some(length.saturating_sub(available_body));
                }
                frames.push(Err(FrameError::BodyTooLarge));
                progressed = true;
                continue;
            }
            let Some(body_end) = body_start.checked_add(length) else {
                self.buffer.clear();
                frames.push(Err(FrameError::BodyTooLarge));
                progressed = true;
                continue;
            };
            if self.buffer.len() < body_end {
                break;
            }

            let body = self.buffer[body_start..body_end].to_vec();
            self.buffer.drain(..body_end);
            let body = match std::str::from_utf8(&body) {
                Ok(body) => body,
                Err(_) => {
                    frames.push(Err(FrameError::InvalidUtf8));
                    progressed = true;
                    continue;
                }
            };
            frames.push(
                serde_json::from_str(body)
                    .map_err(|error| FrameError::InvalidJson(error.to_string())),
            );
            progressed = true;
        }
        progressed
    }
}

pub fn parse_frames(chunk: &[u8], decoder: &mut FrameDecoder) -> Vec<Result<Value, FrameError>> {
    decoder.push(chunk)
}

fn find_header_end(buffer: &[u8]) -> Option<usize> {
    buffer.windows(4).position(|window| window == b"\r\n\r\n")
}

fn resync_header(buffer: &mut Vec<u8>) {
    let Some(index) = find_content_length_header(buffer) else {
        return;
    };
    if index > 0 {
        buffer.drain(..index);
    }
}

fn find_content_length_header(buffer: &[u8]) -> Option<usize> {
    let needle = b"content-length:";
    buffer
        .windows(needle.len())
        .position(|window| window.eq_ignore_ascii_case(needle))
}

fn parse_content_length(header: &[u8]) -> Result<usize, FrameError> {
    let header = std::str::from_utf8(header).map_err(|_| FrameError::InvalidUtf8)?;
    let mut found = None;
    for line in header.split("\r\n") {
        let Some((name, value)) = line.split_once(':') else {
            return Err(FrameError::MissingContentLength);
        };
        if !name.eq_ignore_ascii_case("content-length") {
            continue;
        }
        if found.is_some() {
            return Err(FrameError::InvalidContentLength);
        }
        let value = value.trim();
        if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
            return Err(FrameError::InvalidContentLength);
        }
        found = value.parse::<usize>().ok();
        if found.is_none() {
            return Err(FrameError::InvalidContentLength);
        }
    }
    found.ok_or(FrameError::MissingContentLength)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DocumentSyncOptions {
    pub open_close: bool,
    /// LSP `TextDocumentSyncKind`: 0 none, 1 full, 2 incremental.
    pub change: u8,
}

impl Default for DocumentSyncOptions {
    fn default() -> Self {
        Self {
            open_close: false,
            change: 0,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DocumentSnapshot {
    pub version: i64,
    pub text: String,
}

/// Holds a per-document lock while a synchronized semantic operation runs.
/// The guard is intentionally opaque so callers cannot bypass synchronization.
pub struct DocumentSyncGuard {
    _lock: Option<OwnedMutexGuard<()>>,
}

impl std::fmt::Debug for DocumentSyncGuard {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("DocumentSyncGuard(..)")
    }
}

impl Default for DocumentSyncGuard {
    fn default() -> Self {
        Self { _lock: None }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Position {
    pub line: u32,
    pub character: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Range {
    pub start: Position,
    pub end: Position,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TextDocumentChange {
    pub range: Range,
    pub text: String,
}

#[derive(Debug)]
struct PendingRequest {
    sender: oneshot::Sender<Result<Value, LspError>>,
    method: String,
}

pub type ServerRequestFuture =
    Pin<Box<dyn std::future::Future<Output = Result<Value, LspError>> + Send>>;
pub type ServerRequestHandler =
    Arc<dyn Fn(String, Value, CancellationToken) -> ServerRequestFuture + Send + Sync>;
pub type NotificationHandler = Arc<dyn Fn(String, Value) + Send + Sync>;
pub type CloseHandler = Arc<dyn Fn() + Send + Sync>;

pub struct LspClient {
    writer: Mutex<Option<ChildStdin>>,
    child: Arc<Mutex<Box<dyn ChildWrapper>>>,
    kill_sender: mpsc::UnboundedSender<()>,
    pending: Arc<Mutex<HashMap<String, PendingRequest>>>,
    server_requests: Arc<Mutex<HashMap<String, CancellationToken>>>,
    next_id: AtomicU64,
    closed: AtomicBool,
    process_exited: AtomicBool,
    shutdown_started: AtomicBool,
    closed_notify: Notify,
    process_exit_notify: Notify,
    document_sync: RwLock<DocumentSyncOptions>,
    documents: Mutex<HashMap<String, DocumentSnapshot>>,
    document_locks: Mutex<HashMap<String, Arc<Mutex<()>>>>,
    stderr_tail: Mutex<String>,
    stderr_overflowed: AtomicBool,
    protocol_errors: AtomicUsize,
    max_documents: usize,
    max_stderr_bytes: usize,
    max_frame_bytes: usize,
    default_timeout: Duration,
    server_request_handler: RwLock<Option<ServerRequestHandler>>,
    notification_handler: RwLock<Option<NotificationHandler>>,
    close_handler: RwLock<Option<CloseHandler>>,
}

impl std::fmt::Debug for LspClient {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("LspClient")
            .field("closed", &self.is_closed())
            .field(
                "protocol_errors",
                &self.protocol_errors.load(Ordering::Acquire),
            )
            .finish_non_exhaustive()
    }
}

impl Drop for LspClient {
    fn drop(&mut self) {
        let _ = self.kill_sender.send(());
    }
}

impl LspClient {
    pub async fn spawn(
        spec: CommandSpec,
        default_timeout: Duration,
        max_frame_bytes: usize,
    ) -> Result<Arc<Self>, LspError> {
        Self::spawn_with_cancellation(spec, default_timeout, max_frame_bytes, None).await
    }

    pub async fn spawn_with_cancellation(
        spec: CommandSpec,
        default_timeout: Duration,
        max_frame_bytes: usize,
        cancellation: Option<CancellationToken>,
    ) -> Result<Arc<Self>, LspError> {
        if cancellation
            .as_ref()
            .is_some_and(CancellationToken::is_cancelled)
        {
            return Err(LspError::Cancelled);
        }
        let spec = normalize_command_spec(spec)?;
        let mut command = CommandWrap::with_new(&spec.executable, |command| {
            command
                .args(&spec.args)
                .current_dir(&spec.cwd)
                .env_clear()
                .envs(&spec.env)
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped());
        });
        command.wrap(KillOnDrop);
        #[cfg(unix)]
        command.wrap(ProcessGroup::leader());
        #[cfg(windows)]
        command.wrap(JobObject);

        let mut child = command
            .spawn()
            .map_err(|error| LspError::Spawn(error.to_string()))?;
        if cancellation
            .as_ref()
            .is_some_and(CancellationToken::is_cancelled)
        {
            let _ = child.start_kill();
            return Err(LspError::Cancelled);
        }
        if child.id().is_none() {
            let _ = child.start_kill();
            return Err(LspError::Spawn(
                "spawned process did not expose a PID".to_owned(),
            ));
        }
        let stdin = child
            .stdin()
            .take()
            .ok_or_else(|| LspError::Spawn("rust-analyzer did not expose stdin".to_owned()))?;
        let stdout = child
            .stdout()
            .take()
            .ok_or_else(|| LspError::Spawn("rust-analyzer did not expose stdout".to_owned()))?;
        let stderr = child
            .stderr()
            .take()
            .ok_or_else(|| LspError::Spawn("rust-analyzer did not expose stderr".to_owned()))?;
        let (kill_sender, kill_receiver) = mpsc::unbounded_channel();

        let client = Arc::new(Self {
            writer: Mutex::new(Some(stdin)),
            child: Arc::new(Mutex::new(child)),
            kill_sender,
            pending: Arc::new(Mutex::new(HashMap::new())),
            server_requests: Arc::new(Mutex::new(HashMap::new())),
            next_id: AtomicU64::new(1),
            closed: AtomicBool::new(false),
            process_exited: AtomicBool::new(false),
            shutdown_started: AtomicBool::new(false),
            closed_notify: Notify::new(),
            process_exit_notify: Notify::new(),
            document_sync: RwLock::new(DocumentSyncOptions::default()),
            documents: Mutex::new(HashMap::new()),
            document_locks: Mutex::new(HashMap::new()),
            stderr_tail: Mutex::new(String::new()),
            stderr_overflowed: AtomicBool::new(false),
            protocol_errors: AtomicUsize::new(0),
            max_documents: DEFAULT_MAX_DOCUMENTS,
            max_stderr_bytes: DEFAULT_MAX_STDERR_BYTES,
            max_frame_bytes: max_frame_bytes.max(1),
            default_timeout,
            server_request_handler: RwLock::new(None),
            notification_handler: RwLock::new(None),
            close_handler: RwLock::new(None),
        });

        let weak = Arc::downgrade(&client);
        tokio::spawn(read_stdout(stdout, weak.clone(), max_frame_bytes.max(1)));
        tokio::spawn(read_stderr(stderr, weak.clone()));
        let child = Arc::clone(&client.child);
        tokio::spawn(async move {
            let result = wait_for_child(child, kill_receiver).await;
            if let Some(client) = weak.upgrade() {
                match result {
                    Ok(status) => {
                        client
                            .mark_process_exited(format!("rust-analyzer exited with {status}"))
                            .await;
                    }
                    Err(error) => {
                        client
                            .mark_closed(format!("rust-analyzer wait failed: {error}"))
                            .await;
                    }
                }
            }
        });
        Ok(client)
    }

    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Acquire)
    }

    pub fn protocol_error_count(&self) -> usize {
        self.protocol_errors.load(Ordering::Acquire)
    }

    pub async fn stderr_tail(&self) -> String {
        self.stderr_tail.lock().await.clone()
    }

    pub fn stderr_overflowed(&self) -> bool {
        self.stderr_overflowed.load(Ordering::Acquire)
    }

    pub async fn set_document_sync(&self, options: DocumentSyncOptions) {
        let _ = self
            .set_document_sync_with_cancellation(options, None)
            .await;
    }

    pub async fn set_document_sync_with_cancellation(
        &self,
        options: DocumentSyncOptions,
        cancellation: Option<CancellationToken>,
    ) -> Result<(), LspError> {
        if cancellation
            .as_ref()
            .is_some_and(CancellationToken::is_cancelled)
        {
            return Err(LspError::Cancelled);
        }
        let mut document_sync = tokio::select! {
            document_sync = self.document_sync.write() => document_sync,
            () = cancellation_cancelled(cancellation.clone()), if cancellation.is_some() => {
                return Err(LspError::Cancelled);
            },
        };
        *document_sync = DocumentSyncOptions {
            open_close: options.open_close,
            change: if options.change <= 2 {
                options.change
            } else {
                0
            },
        };
        Ok(())
    }

    pub async fn document_sync(&self) -> DocumentSyncOptions {
        *self.document_sync.read().await
    }

    async fn document_sync_with_cancellation(
        &self,
        cancellation: Option<CancellationToken>,
    ) -> Result<DocumentSyncOptions, LspError> {
        Ok(tokio::select! {
            document_sync = self.document_sync.read() => *document_sync,
            () = cancellation_cancelled(cancellation), if cancellation.is_some() => {
                return Err(LspError::Cancelled);
            },
        })
    }

    pub async fn set_server_request_handler<F, Fut>(&self, handler: F)
    where
        F: Fn(String, Value, CancellationToken) -> Fut + Send + Sync + 'static,
        Fut: std::future::Future<Output = Result<Value, LspError>> + Send + 'static,
    {
        let handler: ServerRequestHandler =
            Arc::new(move |method, params, cancel| Box::pin(handler(method, params, cancel)));
        *self.server_request_handler.write().await = Some(handler);
    }

    pub async fn set_notification_handler<F>(&self, handler: F)
    where
        F: Fn(String, Value) + Send + Sync + 'static,
    {
        *self.notification_handler.write().await = Some(Arc::new(handler));
    }

    pub async fn set_close_handler<F>(&self, handler: F)
    where
        F: Fn() + Send + Sync + 'static,
    {
        *self.close_handler.write().await = Some(Arc::new(handler));
    }

    pub async fn request(&self, method: &str, params: Value) -> Result<Value, LspError> {
        self.request_with_cancellation(method, params, self.default_timeout, None)
            .await
    }

    pub async fn request_with_timeout(
        &self,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> Result<Value, LspError> {
        self.request_with_cancellation(method, params, timeout, None)
            .await
    }

    pub async fn request_with_cancellation(
        &self,
        method: &str,
        params: Value,
        timeout: Duration,
        cancellation: Option<CancellationToken>,
    ) -> Result<Value, LspError> {
        if method.trim().is_empty() {
            return Err(LspError::InvalidInput(
                "LSP method cannot be empty".to_owned(),
            ));
        }
        if cancellation
            .as_ref()
            .is_some_and(CancellationToken::is_cancelled)
        {
            return Err(LspError::Cancelled);
        }
        if self.is_closed() {
            return Err(LspError::Closed(
                "request attempted after process exit".to_owned(),
            ));
        }
        let id = Number::from(self.next_id.fetch_add(1, Ordering::Relaxed));
        let id_value = Value::Number(id);
        let id_key = rpc_id_key(&id_value)
            .ok_or_else(|| LspError::InvalidInput("generated request ID was invalid".to_owned()))?;
        let (sender, receiver) = oneshot::channel();
        {
            let mut pending = self.pending.lock().await;
            if pending.len() >= DEFAULT_MAX_PENDING_REQUESTS {
                return Err(LspError::InvalidInput(
                    "pending request limit exceeded".to_owned(),
                ));
            }
            pending.insert(
                id_key.clone(),
                PendingRequest {
                    sender,
                    method: method.to_owned(),
                },
            );
        }
        if self.is_closed() {
            self.pending.lock().await.remove(&id_key);
            return Err(LspError::Closed(
                "process exited before request was written".to_owned(),
            ));
        }

        let message = json!({"jsonrpc": "2.0", "id": id_value, "method": method, "params": params});
        if let Err(error) = self.write_value(message).await {
            self.pending.lock().await.remove(&id_key);
            return Err(error);
        }

        let timeout = nonzero_duration(timeout);
        tokio::pin!(receiver);
        tokio::select! {
            result = &mut receiver => result.unwrap_or_else(|_| Err(LspError::Closed("request channel closed".to_owned()))),
            _ = time::sleep(timeout) => {
                self.cancel_pending(&id_key, id_value, method, timeout, false).await
            }
            _ = cancellation_cancelled(cancellation.clone()), if cancellation.is_some() => {
                self.cancel_pending(&id_key, id_value, method, timeout, true).await
            }
        }
    }

    pub async fn request_retry(
        &self,
        method: &str,
        params: Value,
        timeout: Duration,
        attempts: usize,
    ) -> Result<Value, LspError> {
        let attempts = attempts.clamp(1, 3);
        let mut last = None;
        for attempt in 0..attempts {
            match self
                .request_with_timeout(method, params.clone(), timeout)
                .await
            {
                Ok(value) => return Ok(value),
                Err(error) if error.is_content_modified() && attempt + 1 < attempts => {
                    last = Some(error);
                    time::sleep(Duration::from_millis(20)).await;
                }
                Err(error) => return Err(error),
            }
        }
        Err(last.unwrap_or_else(|| LspError::Closed("retry did not produce a result".to_owned())))
    }

    pub async fn notify(&self, method: &str, params: Value) -> Result<(), LspError> {
        self.notify_with_cancellation(method, params, None).await
    }

    pub async fn notify_with_cancellation(
        &self,
        method: &str,
        params: Value,
        cancellation: Option<CancellationToken>,
    ) -> Result<(), LspError> {
        if method.trim().is_empty() {
            return Err(LspError::InvalidInput(
                "LSP method cannot be empty".to_owned(),
            ));
        }
        if cancellation
            .as_ref()
            .is_some_and(CancellationToken::is_cancelled)
        {
            return Err(LspError::Cancelled);
        }
        self.write_value_with_cancellation(
            json!({"jsonrpc": "2.0", "method": method, "params": params}),
            cancellation,
        )
        .await
    }

    pub async fn synchronize_document(
        &self,
        uri: &str,
        language_id: &str,
        text: &str,
    ) -> Result<i64, LspError> {
        self.synchronize_document_with_cancellation(uri, language_id, text, None)
            .await
    }

    pub async fn synchronize_document_with_cancellation(
        &self,
        uri: &str,
        language_id: &str,
        text: &str,
        cancellation: Option<CancellationToken>,
    ) -> Result<i64, LspError> {
        let lock = self
            .document_lock_with_cancellation(uri, cancellation.clone())
            .await?;
        let _guard = tokio::select! {
            guard = lock.lock() => guard,
            () = cancellation_cancelled(cancellation.clone()), if cancellation.is_some() => {
                return Err(LspError::Cancelled);
            },
        };
        self.synchronize_document_locked(uri, language_id, text, cancellation)
            .await
    }

    pub async fn begin_document(
        &self,
        uri: &str,
        language_id: &str,
        text: &str,
    ) -> Result<DocumentSyncGuard, LspError> {
        self.begin_document_with_cancellation(uri, language_id, text, None)
            .await
    }

    pub async fn begin_document_with_cancellation(
        &self,
        uri: &str,
        language_id: &str,
        text: &str,
        cancellation: Option<CancellationToken>,
    ) -> Result<DocumentSyncGuard, LspError> {
        let lock = self
            .document_lock_with_cancellation(uri, cancellation.clone())
            .await?;
        let guard = tokio::select! {
            guard = lock.lock_owned() => guard,
            () = cancellation_cancelled(cancellation.clone()), if cancellation.is_some() => {
                return Err(LspError::Cancelled);
            },
        };
        self.synchronize_document_locked(uri, language_id, text, cancellation)
            .await?;
        Ok(DocumentSyncGuard { _lock: Some(guard) })
    }

    pub async fn with_document<'a, T, F>(
        &'a self,
        uri: &str,
        language_id: &str,
        text: &str,
        operation: F,
    ) -> Result<T, LspError>
    where
        F: FnOnce(
            &'a Self,
        )
            -> Pin<Box<dyn std::future::Future<Output = Result<T, LspError>> + Send + 'a>>,
    {
        let _guard = self.begin_document(uri, language_id, text).await?;
        operation(self).await
    }

    pub async fn with_document_with_cancellation<'a, T, F>(
        &'a self,
        uri: &str,
        language_id: &str,
        text: &str,
        cancellation: Option<CancellationToken>,
        operation: F,
    ) -> Result<T, LspError>
    where
        F: FnOnce(
            &'a Self,
        )
            -> Pin<Box<dyn std::future::Future<Output = Result<T, LspError>> + Send + 'a>>,
    {
        let _guard = self
            .begin_document_with_cancellation(uri, language_id, text, cancellation)
            .await?;
        operation(self).await
    }

    pub async fn close_document(&self, uri: &str) -> Result<(), LspError> {
        let lock = self.document_lock(uri).await?;
        {
            let _guard = lock.lock().await;
            let sync = self.document_sync().await;
            if sync.open_close && self.documents.lock().await.contains_key(uri) {
                self.notify(
                    "textDocument/didClose",
                    json!({"textDocument": {"uri": uri}}),
                )
                .await?;
            }
            self.documents.lock().await.remove(uri);
        }
        let mut locks = self.document_locks.lock().await;
        if locks
            .get(uri)
            .is_some_and(|current| Arc::ptr_eq(current, &lock) && Arc::strong_count(current) == 2)
        {
            locks.remove(uri);
        }
        Ok(())
    }

    pub async fn document_snapshot(&self, uri: &str) -> Option<DocumentSnapshot> {
        self.documents.lock().await.get(uri).cloned()
    }

    pub async fn document_version(&self, uri: &str) -> Option<i64> {
        self.documents
            .lock()
            .await
            .get(uri)
            .map(|snapshot| snapshot.version)
    }

    pub async fn shutdown(&self, timeout: Duration) -> Result<(), LspError> {
        let timeout = nonzero_duration(timeout);
        let first_shutdown = !self.shutdown_started.swap(true, Ordering::AcqRel);
        if first_shutdown && !self.is_closed() {
            let _ = self
                .request_with_timeout("shutdown", Value::Null, timeout)
                .await;
            let _ = self.notify("exit", Value::Null).await;
        }
        if self.wait_process_exited(timeout).await {
            return Ok(());
        }

        #[cfg(unix)]
        {
            let child = self.child.lock().await;
            let _ = child.signal(15);
        }
        #[cfg(not(unix))]
        {
            let mut child = self.child.lock().await;
            let _ = child.start_kill();
        }
        if self.wait_process_exited(timeout).await {
            return Ok(());
        }

        {
            let mut child = self.child.lock().await;
            let _ = child.start_kill();
        }
        if self.wait_process_exited(timeout).await {
            return Ok(());
        }

        self.mark_closed("rust-analyzer did not exit after bounded kill escalation".to_owned())
            .await;
        Err(LspError::Timeout {
            method: "shutdown".to_owned(),
            timeout_ms: timeout.as_millis().min(u128::from(u64::MAX)) as u64,
        })
    }

    pub async fn wait_process_exited(&self, timeout: Duration) -> bool {
        let deadline = time::Instant::now() + nonzero_duration(timeout);
        loop {
            if self.process_exited.load(Ordering::Acquire) {
                return true;
            }
            let remaining = deadline.saturating_duration_since(time::Instant::now());
            if remaining.is_zero() {
                return self.process_exited.load(Ordering::Acquire);
            }
            let notified = self.process_exit_notify.notified();
            if time::timeout(remaining, notified).await.is_err() {
                return self.process_exited.load(Ordering::Acquire);
            }
        }
    }

    pub async fn wait_closed(&self, timeout: Duration) -> bool {
        let deadline = time::Instant::now() + nonzero_duration(timeout);
        loop {
            if self.is_closed() {
                return true;
            }
            let remaining = deadline.saturating_duration_since(time::Instant::now());
            if remaining.is_zero() {
                return self.is_closed();
            }
            let notified = self.closed_notify.notified();
            if time::timeout(remaining, notified).await.is_err() {
                return self.is_closed();
            }
        }
    }

    async fn write_value(&self, message: Value) -> Result<(), LspError> {
        self.write_value_with_cancellation(message, None).await
    }

    async fn write_value_with_cancellation(
        &self,
        message: Value,
        cancellation: Option<CancellationToken>,
    ) -> Result<(), LspError> {
        if self.is_closed() {
            return Err(LspError::Closed("process is closed".to_owned()));
        }
        let body =
            serde_json::to_vec(&message).map_err(|error| LspError::Frame(error.to_string()))?;
        if body.len() > self.max_frame_bytes {
            return Err(LspError::Frame(
                "outbound frame exceeded the configured bound".to_owned(),
            ));
        }
        let mut frame = format!("Content-Length: {}\r\n\r\n", body.len()).into_bytes();
        frame.extend_from_slice(&body);
        let mut writer = tokio::select! {
            writer = self.writer.lock() => writer,
            () = cancellation_cancelled(cancellation.clone()), if cancellation.is_some() => {
                return Err(LspError::Cancelled);
            },
        };
        let Some(writer) = writer.as_mut() else {
            return Err(LspError::Closed("stdin is closed".to_owned()));
        };
        let result = writer
            .write_all(&frame)
            .await
            .map_err(|error| LspError::Io(error.to_string()));
        if result.is_ok()
            && cancellation
                .as_ref()
                .is_some_and(CancellationToken::is_cancelled)
        {
            return Err(LspError::Cancelled);
        }
        result
    }

    async fn cancel_pending(
        &self,
        id_key: &str,
        id: Value,
        method: &str,
        timeout: Duration,
        cancelled: bool,
    ) -> Result<Value, LspError> {
        let pending = self.pending.lock().await.remove(id_key);
        if pending.is_none() {
            return if cancelled {
                Err(LspError::Cancelled)
            } else {
                Err(LspError::Timeout {
                    method: method.to_owned(),
                    timeout_ms: timeout.as_millis().min(u128::from(u64::MAX)) as u64,
                })
            };
        }
        let _ = self.notify("$/cancelRequest", json!({"id": id})).await;
        if cancelled {
            return Err(LspError::Cancelled);
        }
        Err(LspError::Timeout {
            method: method.to_owned(),
            timeout_ms: timeout.as_millis().min(u128::from(u64::MAX)) as u64,
        })
    }

    async fn document_lock(&self, uri: &str) -> Result<Arc<Mutex<()>>, LspError> {
        self.document_lock_with_cancellation(uri, None).await
    }

    async fn document_lock_with_cancellation(
        &self,
        uri: &str,
        cancellation: Option<CancellationToken>,
    ) -> Result<Arc<Mutex<()>>, LspError> {
        if uri.trim().is_empty() {
            return Err(LspError::InvalidInput(
                "document URI cannot be empty".to_owned(),
            ));
        }
        let mut locks = tokio::select! {
            locks = self.document_locks.lock() => locks,
            () = cancellation_cancelled(cancellation), if cancellation.is_some() => {
                return Err(LspError::Cancelled);
            },
        };
        if let Some(lock) = locks.get(uri) {
            return Ok(Arc::clone(lock));
        }
        if locks.len() >= self.max_documents {
            return Err(LspError::InvalidInput(
                "document lock limit exceeded".to_owned(),
            ));
        }
        let lock = Arc::new(Mutex::new(()));
        locks.insert(uri.to_owned(), Arc::clone(&lock));
        Ok(lock)
    }

    async fn synchronize_document_locked(
        &self,
        uri: &str,
        language_id: &str,
        text: &str,
        cancellation: Option<CancellationToken>,
    ) -> Result<i64, LspError> {
        let sync = self
            .document_sync_with_cancellation(cancellation.clone())
            .await?;
        let previous = tokio::select! {
            documents = self.documents.lock() => documents.get(uri).cloned(),
            () = cancellation_cancelled(cancellation.clone()), if cancellation.is_some() => {
                return Err(LspError::Cancelled);
            },
        };
        let changed = previous
            .as_ref()
            .is_none_or(|snapshot| snapshot.text != text);
        let version = if changed {
            previous
                .as_ref()
                .map_or(1, |snapshot| snapshot.version.saturating_add(1))
        } else {
            previous.as_ref().map_or(1, |snapshot| snapshot.version)
        };
        if previous.is_none() {
            if sync.open_close {
                self.notify_with_cancellation(
                    "textDocument/didOpen",
                    json!({"textDocument": {"uri": uri, "languageId": language_id, "version": version, "text": text}}),
                    cancellation.clone(),
                )
                .await?;
            }
        } else if changed && sync.change != 0 {
            let content_changes = if sync.change == 2 {
                let previous_text = previous
                    .as_ref()
                    .map_or("", |snapshot| snapshot.text.as_str());
                let change = incremental_change(previous_text, text);
                json!([{"range": range_value(&change.range), "text": change.text}])
            } else {
                json!([{"text": text}])
            };
            self.notify_with_cancellation(
                "textDocument/didChange",
                json!({"textDocument": {"uri": uri, "version": version}, "contentChanges": content_changes}),
                cancellation.clone(),
            )
            .await?;
        }
        let mut documents = tokio::select! {
            documents = self.documents.lock() => documents,
            () = cancellation_cancelled(cancellation), if cancellation.is_some() => {
                return Err(LspError::Cancelled);
            },
        };
        documents.insert(
            uri.to_owned(),
            DocumentSnapshot {
                version,
                text: text.to_owned(),
            },
        );
        Ok(version)
    }

    async fn handle_message(self: &Arc<Self>, message: Value) {
        let message = match validate_message(message) {
            Ok(message) => message,
            Err(error) => {
                self.record_protocol_error(&error);
                return;
            }
        };
        match message {
            IncomingMessage::Response { id, result, error } => {
                let Some(key) = rpc_id_key(&id) else {
                    self.record_protocol_error("response contained an invalid ID");
                    return;
                };
                let pending = self.pending.lock().await.remove(&key);
                let Some(pending) = pending else {
                    return;
                };
                let value = match error {
                    Some(error) => Err(LspError::Response {
                        code: error.code,
                        message: error.message,
                        data: error.data,
                    }),
                    None => Ok(result.unwrap_or(Value::Null)),
                };
                let _ = pending.sender.send(value);
            }
            IncomingMessage::Notification { method, params } => {
                if method == "$/cancelRequest" {
                    self.cancel_server_request(&params).await;
                }
                if let Some(handler) = self.notification_handler.read().await.clone() {
                    handler(method, params);
                }
            }
            IncomingMessage::Request { id, method, params } => {
                let Some(key) = rpc_id_key(&id) else {
                    self.record_protocol_error("server request contained an invalid ID");
                    return;
                };
                let cancellation = CancellationToken::new();
                let accepted = {
                    let mut server_requests = self.server_requests.lock().await;
                    if server_requests.len() >= DEFAULT_MAX_SERVER_REQUESTS {
                        false
                    } else {
                        server_requests.insert(key.clone(), cancellation.clone());
                        true
                    }
                };
                if !accepted {
                    let _ = self
                        .write_value(json!({
                            "jsonrpc": "2.0",
                            "id": id,
                            "error": {"code": -32000, "message": "server request limit exceeded"}
                        }))
                        .await;
                    return;
                }
                let handler = self.server_request_handler.read().await.clone();
                let client = Arc::downgrade(self);
                let server_requests = Arc::clone(&self.server_requests);
                tokio::spawn(run_server_request(
                    client,
                    server_requests,
                    handler,
                    key,
                    id,
                    method,
                    params,
                    cancellation,
                ));
            }
        }
    }

    async fn cancel_server_request(&self, params: &Value) {
        let Some(params) = params.as_object() else {
            self.record_protocol_error("server request cancellation params must be an object");
            return;
        };
        let Some(id) = params.get("id") else {
            self.record_protocol_error("server request cancellation did not contain an ID");
            return;
        };
        let Some(key) = rpc_id_key(id) else {
            self.record_protocol_error("server request cancellation contained an invalid ID");
            return;
        };
        if let Some(token) = self.server_requests.lock().await.get(&key).cloned() {
            token.cancel();
        }
    }

    fn record_protocol_error(&self, error: &str) {
        self.protocol_errors.fetch_add(1, Ordering::AcqRel);
        tracing::warn!(error, "ignoring malformed rust-analyzer JSON-RPC message");
    }

    fn request_kill(&self) {
        let _ = self.kill_sender.send(());
    }

    async fn mark_closed(&self, reason: String) {
        if self.closed.swap(true, Ordering::AcqRel) {
            return;
        }
        self.writer.lock().await.take();
        let mut pending = self.pending.lock().await;
        for (_, request) in pending.drain() {
            let _ = request.sender.send(Err(LspError::Closed(format!(
                "{reason} (request: {})",
                request.method
            ))));
        }
        drop(pending);
        for token in self.server_requests.lock().await.values() {
            token.cancel();
        }
        self.server_requests.lock().await.clear();
        self.closed_notify.notify_waiters();
    }

    async fn mark_process_exited(&self, reason: String) {
        let first = !self.process_exited.swap(true, Ordering::AcqRel);
        if !first {
            return;
        }
        self.process_exit_notify.notify_waiters();
        self.mark_closed(reason).await;
        if let Some(handler) = self.close_handler.read().await.clone() {
            handler();
        }
    }
}

async fn run_server_request(
    client: std::sync::Weak<LspClient>,
    server_requests: Arc<Mutex<HashMap<String, CancellationToken>>>,
    handler: Option<ServerRequestHandler>,
    key: String,
    id: Value,
    method: String,
    params: Value,
    cancellation: CancellationToken,
) {
    let result = if let Some(handler) = handler {
        handler(method, params, cancellation.clone()).await
    } else {
        default_server_request(&method, &params)
    };
    server_requests.lock().await.remove(&key);
    let Some(client) = client.upgrade() else {
        return;
    };
    if client.is_closed() {
        return;
    }
    let response = match result {
        Ok(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
        Err(error) => {
            let (code, message, data) = match error {
                LspError::Response {
                    code,
                    message,
                    data,
                } => (code.unwrap_or(METHOD_NOT_FOUND_CODE), message, data),
                error => (
                    error.code().unwrap_or(METHOD_NOT_FOUND_CODE),
                    error.to_string(),
                    None,
                ),
            };
            json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": object_without_nulls([
                    ("code", json!(code)),
                    ("message", json!(message)),
                    ("data", data.unwrap_or(Value::Null)),
                ]),
            })
        }
    };
    let _ = client.write_value(response).await;
}

async fn read_stdout(
    mut stdout: ChildStdout,
    client: std::sync::Weak<LspClient>,
    max_frame_bytes: usize,
) {
    let mut decoder = FrameDecoder::new(max_frame_bytes);
    let mut buffer = [0u8; 16 * 1024];
    loop {
        let result = stdout.read(&mut buffer).await;
        let read = match result {
            Ok(read) => read,
            Err(error) => {
                if let Some(client) = client.upgrade() {
                    client
                        .mark_closed(format!("stdout read failed: {error}"))
                        .await;
                    client.request_kill();
                }
                return;
            }
        };
        if read == 0 {
            if let Some(client) = client.upgrade() {
                client
                    .mark_closed("rust-analyzer stdout closed".to_owned())
                    .await;
                client.request_kill();
            }
            return;
        }
        let Some(client_ref) = client.upgrade() else {
            return;
        };
        for frame in decoder.push(&buffer[..read]) {
            match frame {
                Ok(value) => client_ref.handle_message(value).await,
                Err(error) => client_ref.record_protocol_error(&error.to_string()),
            }
        }
    }
}

async fn wait_for_child(
    child: Arc<Mutex<Box<dyn ChildWrapper>>>,
    mut kill_receiver: mpsc::UnboundedReceiver<()>,
) -> Result<std::process::ExitStatus, std::io::Error> {
    let mut kill_requested = false;
    loop {
        let result = {
            let mut child = child.lock().await;
            child.try_wait()
        };
        match result {
            Ok(Some(status)) => return Ok(status),
            Err(error) => return Err(error),
            Ok(None) => {}
        }

        if kill_requested {
            time::sleep(Duration::from_millis(10)).await;
            continue;
        }

        tokio::select! {
            _ = kill_receiver.recv() => {
                let mut child = child.lock().await;
                match child.start_kill() {
                    Ok(()) => kill_requested = true,
                    Err(kill_error) => match child.try_wait() {
                        Ok(Some(status)) => return Ok(status),
                        Ok(None) | Err(_) => return Err(kill_error),
                    },
                }
            }
            _ = time::sleep(Duration::from_millis(10)) => {}
        }
    }
}

async fn read_stderr(mut stderr: ChildStderr, client: std::sync::Weak<LspClient>) {
    let mut buffer = [0u8; 2 * 1024];
    loop {
        let read = match stderr.read(&mut buffer).await {
            Ok(read) => read,
            Err(_) => return,
        };
        if read == 0 {
            return;
        }
        let Some(client) = client.upgrade() else {
            return;
        };
        let text = String::from_utf8_lossy(&buffer[..read]);
        let mut tail = client.stderr_tail.lock().await;
        tail.push_str(&text);
        if tail.len() > client.max_stderr_bytes {
            client.stderr_overflowed.store(true, Ordering::Release);
            let remove = tail.len() - client.max_stderr_bytes;
            let boundary = tail
                .char_indices()
                .find(|(index, _)| *index >= remove)
                .map_or(tail.len(), |(index, _)| index);
            tail.drain(..boundary);
        }
    }
}

fn normalize_command_spec(spec: CommandSpec) -> Result<CommandSpec, LspError> {
    if !spec.executable.is_absolute() || !spec.cwd.is_absolute() {
        return Err(LspError::Spawn(
            "executable and cwd must be absolute".to_owned(),
        ));
    }
    let executable = super::normalize::canonical_binary_path(&spec.executable)
        .map_err(|error| LspError::Spawn(error.to_string()))?;
    let cwd = super::normalize::canonical_workspace_path(&spec.cwd)
        .map_err(|error| LspError::Spawn(error.to_string()))?;
    for argument in &spec.args {
        validate_os_string(argument, "argument")?;
    }
    for (key, value) in &spec.env {
        validate_os_string(key, "environment key")?;
        validate_os_string(value, "environment value")?;
    }
    Ok(CommandSpec {
        executable,
        args: spec.args,
        cwd,
        env: spec.env,
    })
}

fn validate_os_string(value: &std::ffi::OsStr, name: &str) -> Result<(), LspError> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        if value.as_bytes().contains(&0) {
            return Err(LspError::Spawn(format!("{name} contains a NUL byte")));
        }
    }
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        if value.encode_wide().any(|unit| unit == 0) {
            return Err(LspError::Spawn(format!("{name} contains a NUL byte")));
        }
    }
    Ok(())
}

fn nonzero_duration(duration: Duration) -> Duration {
    if duration.is_zero() {
        Duration::from_millis(1)
    } else {
        duration
    }
}

async fn cancellation_cancelled(token: Option<CancellationToken>) {
    if let Some(token) = token {
        token.cancelled().await;
    } else {
        std::future::pending::<()>().await;
    }
}

#[derive(Debug)]
enum IncomingMessage {
    Response {
        id: Value,
        result: Option<Value>,
        error: Option<RpcResponseError>,
    },
    Request {
        id: Value,
        method: String,
        params: Value,
    },
    Notification {
        method: String,
        params: Value,
    },
}

#[derive(Debug)]
struct RpcResponseError {
    code: Option<i64>,
    message: String,
    data: Option<Value>,
}

fn validate_message(value: Value) -> Result<IncomingMessage, String> {
    let Value::Object(object) = value else {
        return Err("JSON-RPC message must be an object".to_owned());
    };
    if object.get("jsonrpc") != Some(&Value::String("2.0".to_owned())) {
        return Err("JSON-RPC message must use jsonrpc=2.0".to_owned());
    }
    if object.contains_key("method") {
        let Some(method) = object.get("method").and_then(Value::as_str) else {
            return Err("JSON-RPC method must be a string".to_owned());
        };
        if method.is_empty() {
            return Err("JSON-RPC method cannot be empty".to_owned());
        }
        if object.contains_key("result") || object.contains_key("error") {
            return Err("JSON-RPC request cannot contain result or error".to_owned());
        }
        let params = object.get("params").cloned().unwrap_or(Value::Null);
        if !matches!(params, Value::Null | Value::Object(_) | Value::Array(_)) {
            return Err("JSON-RPC params must be structured".to_owned());
        }
        if let Some(id) = object.get("id") {
            if rpc_id_key(id).is_none() {
                return Err("JSON-RPC request ID must be a string or number".to_owned());
            }
            return Ok(IncomingMessage::Request {
                id: id.clone(),
                method: method.to_owned(),
                params,
            });
        }
        return Ok(IncomingMessage::Notification {
            method: method.to_owned(),
            params,
        });
    }

    let Some(id) = object.get("id").cloned() else {
        return Err("JSON-RPC response must contain an ID".to_owned());
    };
    if object.contains_key("params") {
        return Err("JSON-RPC response cannot contain params".to_owned());
    }
    if !matches!(id, Value::Null) && rpc_id_key(&id).is_none() {
        return Err("JSON-RPC response ID must be null, a string, or a number".to_owned());
    }
    let has_result = object.contains_key("result");
    let has_error = object.contains_key("error");
    if has_result == has_error {
        return Err("JSON-RPC response must contain exactly one of result or error".to_owned());
    }
    if has_error {
        let Value::Object(error) = object.get("error").cloned().unwrap_or(Value::Null) else {
            return Err("JSON-RPC error must be an object".to_owned());
        };
        let message = error
            .get("message")
            .and_then(Value::as_str)
            .ok_or_else(|| "JSON-RPC error message must be a string".to_owned())?;
        let code = error.get("code").and_then(Value::as_i64);
        if code.is_none() {
            return Err("JSON-RPC error code must be an integer".to_owned());
        }
        return Ok(IncomingMessage::Response {
            id,
            result: None,
            error: Some(RpcResponseError {
                code,
                message: message.to_owned(),
                data: error.get("data").cloned(),
            }),
        });
    }
    Ok(IncomingMessage::Response {
        id,
        result: object.get("result").cloned(),
        error: None,
    })
}

fn rpc_id_key(value: &Value) -> Option<String> {
    match value {
        Value::String(_) | Value::Number(_) => serde_json::to_string(value).ok(),
        _ => None,
    }
}

pub fn default_server_request(method: &str, params: &Value) -> Result<Value, LspError> {
    match method {
        "workspace/configuration" => {
            let items = params.get("items").and_then(Value::as_array);
            Ok(Value::Array(items.map_or_else(Vec::new, |items| {
                vec![Value::Null; items.len()]
            })))
        }
        "window/workDoneProgress/create"
        | "client/registerCapability"
        | "client/unregisterCapability"
        | "window/showMessageRequest" => Ok(Value::Null),
        _ => Err(LspError::Response {
            code: Some(METHOD_NOT_FOUND_CODE),
            message: format!("unsupported server request: {method}"),
            data: None,
        }),
    }
}

pub fn range_value(range: &Range) -> Value {
    json!({
        "start": {"line": range.start.line, "character": range.start.character},
        "end": {"line": range.end.line, "character": range.end.character}
    })
}

pub fn incremental_change(previous: &str, next: &str) -> TextDocumentChange {
    let old: Vec<char> = previous.chars().collect();
    let new: Vec<char> = next.chars().collect();
    let mut prefix = 0usize;
    while prefix < old.len() && prefix < new.len() && old[prefix] == new[prefix] {
        prefix += 1;
    }
    let mut old_end = old.len();
    let mut new_end = new.len();
    while old_end > prefix && new_end > prefix && old[old_end - 1] == new[new_end - 1] {
        old_end -= 1;
        new_end -= 1;
    }
    let old_start_byte = char_index_to_byte(previous, prefix);
    let old_end_byte = char_index_to_byte(previous, old_end);
    let new_start_byte = char_index_to_byte(next, prefix);
    let new_end_byte = char_index_to_byte(next, new_end);
    TextDocumentChange {
        range: Range {
            start: position_at_byte(previous, old_start_byte),
            end: position_at_byte(previous, old_end_byte),
        },
        text: next[new_start_byte..new_end_byte].to_owned(),
    }
}

fn char_index_to_byte(text: &str, index: usize) -> usize {
    text.char_indices()
        .nth(index)
        .map_or(text.len(), |(byte, _)| byte)
}

pub fn position_at_byte(text: &str, byte: usize) -> Position {
    let mut line = 0u32;
    let mut character = 0u32;
    let boundary = if byte >= text.len() {
        text.len()
    } else {
        text.char_indices()
            .take_while(|(index, _)| *index <= byte)
            .last()
            .map_or(0, |(index, _)| index)
    };
    for value in text[..boundary].chars() {
        if value == '\n' {
            line = line.saturating_add(1);
            character = 0;
        } else {
            character = character.saturating_add(value.len_utf16() as u32);
        }
    }
    Position { line, character }
}

pub fn utf16_len(text: &str) -> u32 {
    text.chars()
        .map(|character| character.len_utf16() as u32)
        .fold(0, u32::saturating_add)
}

pub fn value_position(value: &Value) -> Option<Position> {
    let object = value.as_object()?;
    Some(Position {
        line: object.get("line")?.as_u64()?.try_into().ok()?,
        character: object.get("character")?.as_u64()?.try_into().ok()?,
    })
}

pub fn value_range(value: &Value) -> Option<Range> {
    let object = value.as_object()?;
    Some(Range {
        start: value_position(object.get("start")?)?,
        end: value_position(object.get("end")?)?,
    })
}

pub fn object_without_nulls(entries: impl IntoIterator<Item = (&'static str, Value)>) -> Value {
    let mut object = Map::new();
    for (key, value) in entries {
        if !value.is_null() {
            object.insert(key.to_owned(), value);
        }
    }
    Value::Object(object)
}
