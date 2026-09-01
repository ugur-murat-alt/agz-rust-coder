use std::{
    fs,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use agz_rust_coder::{Config, RustCoderServer, lsp::path_to_file_uri};
use anyhow::{Context, Result};
use rmcp::{
    ClientHandler, ClientLifecycleMode, ClientServiceExt, ErrorData as McpError, ServiceExt,
    model::{
        CallToolRequestParams, CallToolResponse, ClientCapabilities, ClientInfo, Implementation,
        ProtocolVersion,
    },
    service::{MaybeSendFuture, Peer, RequestContext, RoleClient},
};

static NEXT_STATE_ID: AtomicU64 = AtomicU64::new(0);

struct IsolatedState(PathBuf);

impl Drop for IsolatedState {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[derive(Clone)]
struct MockClient {
    info: ClientInfo,
    roots: Arc<Mutex<MockRoots>>,
}

struct MockRoots {
    uris: Vec<String>,
    error: bool,
    delay: Duration,
    calls: usize,
}

impl MockClient {
    fn new(info: ClientInfo, paths: Vec<PathBuf>) -> Self {
        Self {
            info,
            roots: Arc::new(Mutex::new(MockRoots {
                uris: paths.into_iter().map(|path| file_uri(&path)).collect(),
                error: false,
                delay: Duration::ZERO,
                calls: 0,
            })),
        }
    }

    fn set_roots(&self, paths: Vec<PathBuf>) {
        let mut roots = self.roots.lock().expect("mock roots lock");
        roots.uris = paths.into_iter().map(|path| file_uri(&path)).collect();
        roots.error = false;
    }

    fn set_uris(&self, uris: Vec<String>) {
        let mut roots = self.roots.lock().expect("mock roots lock");
        roots.uris = uris;
        roots.error = false;
    }

    fn set_error(&self) {
        self.roots.lock().expect("mock roots lock").error = true;
    }

    fn set_delay(&self, delay: Duration) {
        self.roots.lock().expect("mock roots lock").delay = delay;
    }

    fn calls(&self) -> usize {
        self.roots.lock().expect("mock roots lock").calls
    }
}

impl ClientHandler for MockClient {
    #[allow(deprecated)]
    fn list_roots(
        &self,
        _context: RequestContext<RoleClient>,
    ) -> impl std::future::Future<Output = Result<rmcp::model::ListRootsResult, McpError>>
    + MaybeSendFuture
    + '_ {
        let (uris, error, delay) = {
            let mut roots = self.roots.lock().expect("mock roots lock");
            roots.calls += 1;
            (roots.uris.clone(), roots.error, roots.delay)
        };
        async move {
            if !delay.is_zero() {
                tokio::time::sleep(delay).await;
            }
            if error {
                return Err(McpError::internal_error("mock roots failure", None));
            }
            Ok(rmcp::model::ListRootsResult::new(
                uris.into_iter().map(rmcp::model::Root::new).collect(),
            ))
        }
    }

    fn get_info(&self) -> ClientInfo {
        self.info.clone()
    }
}

fn file_uri(path: &Path) -> String {
    path_to_file_uri(path).expect("local file URI")
}

fn fixture_root() -> PathBuf {
    fs::canonicalize(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/stage7/clean"),
    )
    .expect("canonical stage7 clean fixture")
}

fn isolated_config(root: PathBuf) -> (Config, IsolatedState) {
    let state_id = NEXT_STATE_ID.fetch_add(1, Ordering::Relaxed);
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock is after the Unix epoch")
        .as_nanos();
    let state = fs::canonicalize(std::env::temp_dir())
        .expect("canonical temp directory")
        .join(format!(
            "agz-rust-coder-roots-state-{}-{stamp}-{state_id}",
            std::process::id()
        ));
    let mut config = Config::defaults_at(root);
    config.gate.cache_dir = state.join("gate");
    config.gate.lease_dir = state.join("leases");
    config.docs.cache_dir = state.join("docs");
    config.telemetry.enabled = false;
    config.telemetry.path = state.join("activity.jsonl");
    (config, IsolatedState(state))
}

fn client_info(capabilities: ClientCapabilities) -> ClientInfo {
    ClientInfo::new(
        capabilities,
        Implementation::new("agz-rust-coder-roots-test", "0.1.0"),
    )
}

#[allow(deprecated)]
fn advertised_client_info() -> ClientInfo {
    client_info(
        ClientCapabilities::builder()
            .enable_roots()
            .enable_roots_list_changed()
            .build(),
    )
}

fn spawn_server(config: Config) -> (tokio::io::DuplexStream, tokio::task::JoinHandle<Result<()>>) {
    let (server_transport, client_transport) = tokio::io::duplex(1 << 20);
    let task = tokio::spawn(async move {
        let service = RustCoderServer::new(config)?
            .serve(server_transport)
            .await?;
        service.waiting().await?;
        Ok(())
    });
    (client_transport, task)
}

async fn call_audit(peer: &Peer<RoleClient>) -> Result<rmcp::model::CallToolResult> {
    let result = peer
        .call_tool_once(CallToolRequestParams::new("audit"))
        .await?;
    match result {
        CallToolResponse::Complete(result) => Ok(result),
        other => anyhow::bail!("expected complete audit response, got {other:?}"),
    }
}

fn result_status(result: &rmcp::model::CallToolResult) -> Option<&str> {
    result
        .structured_content
        .as_ref()
        .and_then(|value| value.get("status"))
        .and_then(serde_json::Value::as_str)
}

#[tokio::test]
async fn unsupported_client_roots_use_the_configured_allowlist() -> Result<()> {
    let root = fixture_root();
    let mock = MockClient::new(
        client_info(ClientCapabilities::default()),
        vec![root.clone()],
    );
    let (config, _state) = isolated_config(root.clone());
    let (transport, server) = spawn_server(config);
    let client = mock
        .clone()
        .serve_with_lifecycle(
            transport,
            ClientLifecycleMode::Discover {
                preferred_versions: vec![ProtocolVersion::V_2026_07_28],
            },
        )
        .await?;

    let result = call_audit(client.peer()).await?;
    assert_eq!(result_status(&result), Some("CLEAN"));
    assert_eq!(mock.calls(), 0);

    client.cancel().await?;
    server.await??;
    Ok(())
}

#[tokio::test]
async fn advertised_empty_roots_fail_closed_without_configured_fallback() -> Result<()> {
    let root = fixture_root();
    let mock = MockClient::new(advertised_client_info(), Vec::new());
    let (config, _state) = isolated_config(root);
    let (transport, server) = spawn_server(config);
    let client = mock
        .clone()
        .serve_with_lifecycle(
            transport,
            ClientLifecycleMode::Discover {
                preferred_versions: vec![ProtocolVersion::V_2026_07_28],
            },
        )
        .await?;

    let result = call_audit(client.peer()).await?;
    assert_eq!(result_status(&result), Some("INCONCLUSIVE"));
    assert_eq!(result.is_error, Some(true));
    assert_eq!(mock.calls(), 1);

    client.cancel().await?;
    server.await??;
    Ok(())
}

#[tokio::test]
async fn advertised_root_errors_fail_closed() -> Result<()> {
    let root = fixture_root();
    let mock = MockClient::new(advertised_client_info(), vec![root.clone()]);
    mock.set_error();
    let (config, _state) = isolated_config(root);
    let (transport, server) = spawn_server(config);
    let client = mock
        .clone()
        .serve_with_lifecycle(
            transport,
            ClientLifecycleMode::Discover {
                preferred_versions: vec![ProtocolVersion::V_2026_07_28],
            },
        )
        .await?;

    let result = call_audit(client.peer()).await?;
    assert_eq!(result_status(&result), Some("INCONCLUSIVE"));
    assert_eq!(result.is_error, Some(true));
    assert_eq!(mock.calls(), 1);

    client.cancel().await?;
    server.await??;
    Ok(())
}

#[tokio::test]
async fn advertised_invalid_root_uri_fails_closed() -> Result<()> {
    let root = fixture_root();
    let mock = MockClient::new(advertised_client_info(), vec![root.clone()]);
    mock.set_uris(vec!["https://example.invalid/workspace".to_owned()]);
    let (config, _state) = isolated_config(root);
    let (transport, server) = spawn_server(config);
    let client = mock
        .clone()
        .serve_with_lifecycle(
            transport,
            ClientLifecycleMode::Discover {
                preferred_versions: vec![ProtocolVersion::V_2026_07_28],
            },
        )
        .await?;

    let result = call_audit(client.peer()).await?;
    assert_eq!(result_status(&result), Some("INCONCLUSIVE"));
    assert_eq!(result.is_error, Some(true));
    assert_eq!(mock.calls(), 1);

    client.cancel().await?;
    server.await??;
    Ok(())
}

#[tokio::test]
async fn advertised_root_timeout_fails_closed() -> Result<()> {
    let root = fixture_root();
    let mock = MockClient::new(advertised_client_info(), vec![root.clone()]);
    mock.set_delay(Duration::from_secs(6));
    let (config, _state) = isolated_config(root);
    let (transport, server) = spawn_server(config);
    let client = mock
        .clone()
        .serve_with_lifecycle(
            transport,
            ClientLifecycleMode::Discover {
                preferred_versions: vec![ProtocolVersion::V_2026_07_28],
            },
        )
        .await?;

    let result = call_audit(client.peer()).await?;
    assert_eq!(result_status(&result), Some("INCONCLUSIVE"));
    assert_eq!(result.is_error, Some(true));
    assert_eq!(mock.calls(), 1);

    client.cancel().await?;
    server.await??;
    Ok(())
}

#[tokio::test]
async fn advertised_roots_narrow_the_configured_allowlist_and_singleflight() -> Result<()> {
    let root = fixture_root();
    let narrowed = root.join("src");
    let mock = MockClient::new(advertised_client_info(), vec![narrowed.clone()]);
    mock.set_delay(Duration::from_millis(25));
    let (config, _state) = isolated_config(root);
    let (transport, server) = spawn_server(config);
    let client = mock
        .clone()
        .serve_with_lifecycle(
            transport,
            ClientLifecycleMode::Discover {
                preferred_versions: vec![ProtocolVersion::V_2026_07_28],
            },
        )
        .await?;

    let (first, second) = tokio::join!(call_audit(client.peer()), call_audit(client.peer()));
    let first = first?;
    let second = second?;
    for result in [&first, &second] {
        assert_eq!(result_status(result), Some("CLEAN"));
        let workspace = result
            .structured_content
            .as_ref()
            .and_then(|value| value.get("workspace"))
            .context("missing workspace information")?;
        assert_eq!(
            workspace
                .get("workspaceRoot")
                .and_then(|value| value.as_str()),
            narrowed.to_str()
        );
    }
    assert_eq!(mock.calls(), 1);

    client.cancel().await?;
    server.await??;
    Ok(())
}

#[tokio::test]
async fn roots_changed_invalidates_the_epoch_cache() -> Result<()> {
    let root = fixture_root();
    let narrowed = root.join("src");
    let mock = MockClient::new(advertised_client_info(), vec![narrowed.clone()]);
    let (config, _state) = isolated_config(root.clone());
    let (transport, server) = spawn_server(config);
    let client = mock
        .clone()
        .serve_with_lifecycle(
            transport,
            ClientLifecycleMode::Discover {
                preferred_versions: vec![ProtocolVersion::V_2026_07_28],
            },
        )
        .await?;

    let first = call_audit(client.peer()).await?;
    assert_eq!(result_status(&first), Some("CLEAN"));
    mock.set_roots(vec![root.clone()]);
    client.peer().notify_roots_list_changed().await?;
    let second = call_audit(client.peer()).await?;
    assert_eq!(result_status(&second), Some("CLEAN"));
    let workspace = second
        .structured_content
        .as_ref()
        .and_then(|value| value.get("workspace"))
        .context("missing workspace information")?;
    let workspace_root = workspace
        .get("workspaceRoot")
        .and_then(|value| value.as_str())
        .context("workspace root is not a string")?;
    assert_eq!(Path::new(workspace_root).canonicalize()?, root);
    assert_eq!(mock.calls(), 2);

    client.cancel().await?;
    server.await??;
    Ok(())
}

struct SlowProject {
    root: PathBuf,
    base: PathBuf,
}

impl SlowProject {
    #[allow(clippy::unnecessary_debug_formatting)]
    fn new() -> Self {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock is after the Unix epoch")
            .as_nanos();
        let base = fs::canonicalize(std::env::temp_dir())
            .expect("canonical temp directory")
            .join(format!(
                "agz-rust-coder-roots-cancel-{}-{stamp}",
                std::process::id()
            ));
        let root = base.join("workspace");
        fs::create_dir_all(root.join("src")).expect("create slow project");
        fs::write(
            root.join("Cargo.toml"),
            "[package]\nname = \"roots-cancel\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[workspace]\n",
        )
        .expect("write slow manifest");
        fs::write(
            root.join("Cargo.lock"),
            "version = 4\n\n[[package]]\nname = \"roots-cancel\"\nversion = \"0.1.0\"\n",
        )
        .expect("write slow lockfile");
        fs::write(root.join("src/lib.rs"), "pub struct RootsCancel;\n").expect("write slow source");
        let pid_file = root.join("build-script.pid");
        fs::write(
            root.join("build.rs"),
            format!(
                "fn main() {{ std::fs::write({pid_file:?}, std::process::id().to_string()).unwrap(); std::thread::sleep(std::time::Duration::from_secs(10)); }}\n"
            ),
        )
        .expect("write slow build script");
        Self { root, base }
    }

    fn pid_file(&self) -> PathBuf {
        self.root.join("build-script.pid")
    }
}

impl Drop for SlowProject {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.base);
    }
}

#[tokio::test]
async fn roots_changed_cancels_an_active_workspace_check() -> Result<()> {
    let project = SlowProject::new();
    let (mut config, _state) = isolated_config(project.root.clone());
    config.gate.cache_dir = project.base.join("cache");
    config.gate.lease_dir = project.base.join("leases");
    config.gate.hard_timeout_ms = 30_000;
    let journal_jobs = config.gate.lease_dir.join("process-journal/jobs");
    let mock = MockClient::new(advertised_client_info(), vec![project.root.clone()]);
    let (transport, server) = spawn_server(config);
    let client = mock
        .serve_with_lifecycle(
            transport,
            ClientLifecycleMode::Discover {
                preferred_versions: vec![ProtocolVersion::V_2026_07_28],
            },
        )
        .await?;
    let peer = client.peer().clone();
    let check = tokio::spawn(async move {
        peer.call_tool_once(CallToolRequestParams::new("check"))
            .await
    });

    for _ in 0..500 {
        if project.pid_file().is_file() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(
        project.pid_file().is_file(),
        "slow Cargo build did not start"
    );
    assert!(
        fs::read_dir(&journal_jobs)
            .expect("read active process journal")
            .filter_map(Result::ok)
            .any(|entry| entry.path().extension().and_then(|value| value.to_str()) == Some("json")),
        "active Cargo process was not journaled"
    );
    client.peer().notify_roots_list_changed().await?;
    let response = check.await??;
    let CallToolResponse::Complete(result) = response else {
        anyhow::bail!("expected complete check response, got {response:?}");
    };
    assert_eq!(result_status(&result), Some("CANCELLED"));
    assert_eq!(result.is_error, Some(true));
    for _ in 0..100 {
        let active = fs::read_dir(&journal_jobs)
            .expect("read completed process journal")
            .filter_map(Result::ok)
            .any(|entry| entry.path().extension().and_then(|value| value.to_str()) == Some("json"));
        if !active {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(
        fs::read_dir(&journal_jobs)
            .expect("read cleaned process journal")
            .filter_map(Result::ok)
            .all(|entry| entry.path().extension().and_then(|value| value.to_str()) != Some("json")),
        "completed Cargo process remained journaled"
    );

    client.cancel().await?;
    server.await??;
    Ok(())
}
