use std::{
    env, fs,
    path::{Path, PathBuf},
    process::Command,
    sync::{Arc, Mutex, OnceLock},
    time::Duration,
};

use agz_rust_coder::lsp::client::default_server_request;
use agz_rust_coder::{
    lsp::{DocumentSyncOptions, FrameDecoder, FrameError, LspClient, LspError},
    process::CommandSpec,
};
use serde_json::{Value, json};
use tokio::time::sleep;
use tokio_util::sync::CancellationToken;

static MOCK_BINARY: OnceLock<PathBuf> = OnceLock::new();

fn temp_dir() -> PathBuf {
    fs::canonicalize(env::temp_dir()).expect("canonical temp directory")
}

fn mock_binary() -> &'static PathBuf {
    MOCK_BINARY.get_or_init(|| {
        let source =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/lsp/mock_ra.rs");
        let output_dir = temp_dir().join(format!(
            "agz-rust-coder-mock-fixture-{}",
            std::process::id()
        ));
        fs::create_dir_all(&output_dir).expect("create mock fixture directory");
        let output = output_dir.join(format!("mock-ra{}", env::consts::EXE_SUFFIX));
        let rustc = [
            env::var_os("RUSTC").map(PathBuf::from),
            Some(PathBuf::from("rustc")),
        ]
        .into_iter()
        .flatten()
        .find(|candidate| Command::new(candidate).arg("--version").output().is_ok())
        .expect("find rustc for mock fixture");
        let status = Command::new(rustc)
            .args(["--edition=2024"])
            .arg(source)
            .arg("-o")
            .arg(&output)
            .status()
            .expect("compile mock fixture");
        assert!(status.success(), "mock fixture compilation failed");
        output
    })
}

fn log_path(label: &str) -> PathBuf {
    let path = temp_dir().join(format!(
        "agz-rust-coder-lsp-client-{label}-{}-{}.log",
        std::process::id(),
        NEXT_LOG.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    let _ = fs::remove_file(&path);
    path
}

static NEXT_LOG: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

async fn spawn_client(mode: &str, log: Option<&Path>) -> Arc<LspClient> {
    let mut spec = CommandSpec::new(mock_binary().clone(), temp_dir())
        .arg("--mode")
        .arg(mode);
    if let Some(log) = log {
        spec = spec.arg("--log").arg(log.as_os_str());
    }
    LspClient::spawn(spec, Duration::from_secs(2), 64 * 1024)
        .await
        .expect("spawn mock client")
}

#[test]
fn client_module_compiles_with_the_process_boundary() {
    let mut decoder = FrameDecoder::new(1024);
    let frames = decoder.push(b"Content-Length: 2\r\n\r\n{}");
    assert_eq!(frames.len(), 1);
}

#[test]
fn oversized_frame_discards_only_its_body_before_the_next_frame() {
    let mut decoder = FrameDecoder::with_limits(4, 128, 128, 1);
    let first = decoder.push(b"Content-Length: 8\r\n\r\n1234");
    assert!(matches!(first.as_slice(), [Err(FrameError::BodyTooLarge)]));

    let second = decoder.push(b"5678Content-Length: 2\r\n\r\n{}");
    assert_eq!(second, vec![Ok(json!({}))]);
}

#[tokio::test]
async fn accepts_partial_and_concatenated_frames() {
    let partial = spawn_client("partial", None).await;
    assert_eq!(
        partial
            .request("echo", Value::Null)
            .await
            .expect("partial response"),
        json!({"ok": true})
    );
    partial
        .shutdown(Duration::from_millis(200))
        .await
        .expect("shutdown partial client");

    let concat = spawn_client("concat", None).await;
    assert_eq!(
        concat
            .request("echo", Value::Null)
            .await
            .expect("concatenated response"),
        json!({"ok": true})
    );
    concat
        .shutdown(Duration::from_millis(200))
        .await
        .expect("shutdown concatenated client");
}

#[tokio::test]
async fn rejects_malformed_protocol_values_without_closing_the_client() {
    let client = spawn_client("malformed", None).await;

    let result = client
        .request("echo", Value::Null)
        .await
        .expect("valid response after malformed frame");
    assert_eq!(result, json!({"ok": true}));
    assert!(!client.is_closed());
    assert!(client.protocol_error_count() >= 1);
    client
        .shutdown(Duration::from_millis(200))
        .await
        .expect("shutdown mock client");
}

#[tokio::test]
async fn preserves_response_codes_and_retries_content_modified_by_code() {
    let client = spawn_client("error", None).await;
    let error = client
        .request("error", Value::Null)
        .await
        .expect_err("mock error response");
    match error {
        LspError::Response { code, data, .. } => {
            assert_eq!(code, Some(-32_042));
            assert_eq!(data, Some(json!({"marker": true})));
        }
        other => panic!("unexpected error: {other:?}"),
    }
    client
        .shutdown(Duration::from_millis(200))
        .await
        .expect("shutdown error client");

    let retry = spawn_client("retry", None).await;
    let result = retry
        .request_retry("retry", Value::Null, Duration::from_secs(1), 2)
        .await
        .expect("retry response");
    assert_eq!(result, json!({"ok": true}));
    retry
        .shutdown(Duration::from_millis(200))
        .await
        .expect("shutdown retry client");
}

#[tokio::test]
async fn timeout_sends_cancel_request_with_the_matching_request() {
    let log = log_path("timeout");
    let client = spawn_client("slow", Some(&log)).await;

    let error = client
        .request_with_timeout("slow", Value::Null, Duration::from_millis(50))
        .await
        .expect_err("slow request timeout");
    assert!(matches!(error, LspError::Timeout { .. }));
    sleep(Duration::from_millis(50)).await;
    let log_text = fs::read_to_string(&log).expect("read timeout log");
    assert!(log_text.contains("$/cancelRequest"));
    client
        .shutdown(Duration::from_millis(200))
        .await
        .expect("shutdown slow client");
    let _ = fs::remove_file(log);
}

#[tokio::test]
async fn request_cancellation_sends_cancel_request_and_returns_distinct_error() {
    let log = log_path("request-cancel");
    let client = spawn_client("slow", Some(&log)).await;
    let cancellation = CancellationToken::new();
    let request_client = Arc::clone(&client);
    let request_cancellation = cancellation.clone();
    let request = tokio::spawn(async move {
        request_client
            .request_with_cancellation(
                "slow",
                Value::Null,
                Duration::from_secs(2),
                Some(request_cancellation),
            )
            .await
    });

    sleep(Duration::from_millis(50)).await;
    cancellation.cancel();
    assert!(matches!(
        request.await.expect("cancelled request"),
        Err(LspError::Cancelled)
    ));
    for _ in 0..20 {
        if fs::read_to_string(&log).is_ok_and(|contents| contents.contains("$/cancelRequest")) {
            break;
        }
        sleep(Duration::from_millis(5)).await;
    }
    let log_text = fs::read_to_string(&log).expect("read cancellation log");
    assert!(log_text.contains("$/cancelRequest"));
    client
        .shutdown(Duration::from_millis(200))
        .await
        .expect("shutdown cancelled client");
    let _ = fs::remove_file(log);
}

#[tokio::test]
async fn already_cancelled_request_does_not_write_a_request_frame() {
    let log = log_path("request-pre-cancel");
    let client = spawn_client("slow", Some(&log)).await;
    let cancellation = CancellationToken::new();
    cancellation.cancel();

    assert!(matches!(
        client
            .request_with_cancellation(
                "slow",
                Value::Null,
                Duration::from_secs(2),
                Some(cancellation),
            )
            .await,
        Err(LspError::Cancelled)
    ));
    assert!(fs::read_to_string(&log).is_err());
    client
        .shutdown(Duration::from_millis(200))
        .await
        .expect("shutdown pre-cancelled client");
    let _ = fs::remove_file(log);
}

#[tokio::test]
async fn dispatches_notifications_and_server_requests_without_blocking_responses() {
    let client = spawn_client("server", None).await;
    let notification = Arc::new(Mutex::new(None::<(String, Value)>));
    let notification_observer = Arc::clone(&notification);
    client
        .set_notification_handler(move |method, params| {
            if let Ok(mut observed) = notification_observer.lock() {
                *observed = Some((method, params));
            }
        })
        .await;
    let handled = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let handled_observer = Arc::clone(&handled);
    client
        .set_server_request_handler(move |method, _params, _cancel| {
            let handled = Arc::clone(&handled_observer);
            async move {
                assert_eq!(method, "mock/custom");
                handled.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                Ok(json!({"handled": true}))
            }
        })
        .await;

    assert_eq!(
        client
            .request("trigger", Value::Null)
            .await
            .expect("trigger response"),
        Value::Null
    );
    for _ in 0..20 {
        if handled.load(std::sync::atomic::Ordering::Relaxed) == 1 {
            break;
        }
        sleep(Duration::from_millis(5)).await;
    }
    assert_eq!(handled.load(std::sync::atomic::Ordering::Relaxed), 1);
    assert_eq!(
        notification.lock().expect("notification mutex").as_ref(),
        Some(&("mock/notification".to_owned(), json!({"value": 7})))
    );
    client
        .shutdown(Duration::from_millis(200))
        .await
        .expect("shutdown server client");
}

#[tokio::test]
async fn cancels_a_server_request_and_answers_the_server_after_handler_cancellation() {
    let client = spawn_client("server-cancel", None).await;
    let cancelled = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let cancelled_observer = Arc::clone(&cancelled);
    client
        .set_server_request_handler(move |method, _params, cancellation| {
            let cancelled = Arc::clone(&cancelled_observer);
            async move {
                assert_eq!(method, "mock/slowServerRequest");
                cancellation.cancelled().await;
                cancelled.store(true, std::sync::atomic::Ordering::Release);
                Err(LspError::Cancelled)
            }
        })
        .await;

    assert_eq!(
        client
            .request("trigger", Value::Null)
            .await
            .expect("trigger response"),
        json!({"cancelled": true})
    );
    assert!(cancelled.load(std::sync::atomic::Ordering::Acquire));
    client
        .shutdown(Duration::from_millis(200))
        .await
        .expect("shutdown cancelled server client");
}

#[test]
fn default_server_requests_are_answered_or_rejected_explicitly() {
    assert_eq!(
        default_server_request(
            "workspace/configuration",
            &json!({"items": [{"section": "one"}, {"section": "two"}]}),
        )
        .expect("configuration response"),
        json!([null, null])
    );
    let error =
        default_server_request("mock/unsupported", &Value::Null).expect_err("unsupported request");
    assert!(matches!(
        error,
        LspError::Response {
            code: Some(-32601),
            ..
        }
    ));
}

#[tokio::test]
async fn bounds_stderr_and_tracks_incremental_document_state() {
    let stderr_client = spawn_client("stderr", None).await;
    let _ = stderr_client.request("echo", Value::Null).await;
    sleep(Duration::from_millis(25)).await;
    assert!(stderr_client.stderr_overflowed());
    assert!(stderr_client.stderr_tail().await.ends_with("FINAL-STDERR"));
    stderr_client
        .shutdown(Duration::from_millis(200))
        .await
        .expect("shutdown stderr client");

    let log = log_path("sync");
    let client = spawn_client("echo", Some(&log)).await;
    client
        .set_document_sync(DocumentSyncOptions {
            open_close: true,
            change: 2,
        })
        .await;
    let uri = "file:///mock/lib.rs";
    assert_eq!(
        client
            .synchronize_document(uri, "rust", "fn main() {}\n")
            .await
            .expect("open document"),
        1
    );
    assert_eq!(
        client
            .synchronize_document(uri, "rust", "fn main() { 1 }\n")
            .await
            .expect("change document"),
        2
    );
    assert_eq!(
        client
            .document_snapshot(uri)
            .await
            .expect("document snapshot")
            .text,
        "fn main() { 1 }\n"
    );
    client.close_document(uri).await.expect("close document");
    sleep(Duration::from_millis(25)).await;
    let log_text = fs::read_to_string(&log).expect("read sync log");
    assert!(log_text.contains("textDocument/didOpen"));
    assert!(log_text.contains("textDocument/didChange"));
    assert!(log_text.contains("textDocument/didClose"));
    client
        .shutdown(Duration::from_millis(200))
        .await
        .expect("shutdown sync client");
    let _ = fs::remove_file(log);
}

#[tokio::test]
async fn document_cancellation_unblocks_while_waiting_for_the_same_document() {
    let client = spawn_client("echo", None).await;
    let uri = "file:///mock/blocked.rs";
    let guard = client
        .begin_document(uri, "rust", "fn main() {}\n")
        .await
        .expect("open document");
    let cancellation = CancellationToken::new();
    let waiting_client = Arc::clone(&client);
    let waiting_cancellation = cancellation.clone();
    let waiting = tokio::spawn(async move {
        waiting_client
            .synchronize_document_with_cancellation(
                uri,
                "rust",
                "fn main() { 1 }\n",
                Some(waiting_cancellation),
            )
            .await
    });

    sleep(Duration::from_millis(20)).await;
    cancellation.cancel();
    assert!(matches!(
        waiting.await.expect("document task"),
        Err(LspError::Cancelled)
    ));
    assert_eq!(client.document_version(uri).await, Some(1));
    drop(guard);
    client
        .shutdown(Duration::from_millis(200))
        .await
        .expect("shutdown document client");
}

#[tokio::test]
async fn closed_documents_release_their_per_document_lock_budget() {
    let client = spawn_client("echo", None).await;
    for index in 0..(agz_rust_coder::lsp::client::DEFAULT_MAX_DOCUMENTS + 8) {
        let uri = format!("file:///mock/document-{index}.rs");
        client
            .synchronize_document(&uri, "rust", "fn main() {}\n")
            .await
            .expect("open document");
        client.close_document(&uri).await.expect("close document");
    }
    client
        .shutdown(Duration::from_millis(200))
        .await
        .expect("shutdown document client");
}

#[tokio::test]
async fn unsupported_server_requests_receive_a_protocol_error_response() {
    let log = log_path("server-unsupported");
    let client = spawn_client("server-unsupported", Some(&log)).await;
    assert_eq!(
        client
            .request("trigger", Value::Null)
            .await
            .expect("trigger response"),
        Value::Null
    );
    for _ in 0..20 {
        if fs::read_to_string(&log).is_ok_and(|contents| contents.contains("\"code\":-32601")) {
            break;
        }
        sleep(Duration::from_millis(5)).await;
    }
    let log_text = fs::read_to_string(&log).expect("read unsupported request log");
    assert!(log_text.contains("\"code\":-32601"), "{log_text}");
    client
        .shutdown(Duration::from_millis(200))
        .await
        .expect("shutdown unsupported request client");
    let _ = fs::remove_file(log);
}

#[tokio::test]
async fn graceful_and_forced_shutdowns_remain_bounded() {
    let graceful = spawn_client("graceful", None).await;
    graceful
        .shutdown(Duration::from_millis(200))
        .await
        .expect("graceful shutdown");
    assert!(graceful.wait_closed(Duration::from_millis(200)).await);

    let forced = spawn_client("ignore", None).await;
    forced
        .shutdown(Duration::from_millis(40))
        .await
        .expect("forced shutdown");
    assert!(forced.wait_closed(Duration::from_millis(200)).await);
}

#[cfg(unix)]
#[tokio::test]
async fn rejects_a_symlinked_executable_before_spawning() {
    use std::os::unix::fs::symlink;

    let root = temp_dir().join(format!(
        "agz-rust-coder-lsp-client-path-{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).expect("create path safety fixture");
    let link = root.join("rust-analyzer");
    symlink(mock_binary(), &link).expect("create executable symlink");
    let result = LspClient::spawn(
        CommandSpec::new(link, temp_dir()),
        Duration::from_secs(1),
        64 * 1024,
    )
    .await;
    assert!(matches!(result, Err(LspError::Spawn(message)) if message.contains("symlink")));
    let _ = fs::remove_dir_all(root);
}
