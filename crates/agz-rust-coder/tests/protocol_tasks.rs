use agz_rust_coder::{Config, RustCoderServer};
use anyhow::{Context, Result};
use rmcp::{
    ClientLifecycleMode, ClientServiceExt, ServiceExt,
    model::{
        CallToolRequestParams, CallToolResponse, CallToolResult, CancelTaskParams,
        ClientCapabilities, ClientInfo, GetTaskParams, Implementation, ProtocolVersion,
        TaskPayload, TaskStatus,
    },
};
use serde_json::{Map, Value};
use std::{
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

static NEXT_STATE_ID: AtomicU64 = AtomicU64::new(0);

struct IsolatedState(PathBuf);

impl Drop for IsolatedState {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn isolated_config(root: PathBuf) -> (Config, IsolatedState) {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock is after the Unix epoch")
        .as_nanos();
    let state = std::fs::canonicalize(std::env::temp_dir())
        .expect("canonical temp directory")
        .join(format!(
            "agz-rust-coder-task-state-{}-{stamp}-{}",
            std::process::id(),
            NEXT_STATE_ID.fetch_add(1, Ordering::Relaxed)
        ));
    let mut config = Config::defaults_at(root);
    config.gate.cache_dir = state.join("gate");
    config.gate.lease_dir = state.join("leases");
    config.docs.cache_dir = state.join("docs");
    config.telemetry.enabled = false;
    config.telemetry.path = state.join("activity.jsonl");
    (config, IsolatedState(state))
}

fn spawn_server_for(
    fixture: &'static str,
) -> (tokio::io::DuplexStream, tokio::task::JoinHandle<Result<()>>) {
    let root = std::fs::canonicalize(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../tests/fixtures/stage7")
            .join(fixture),
    )
    .expect("canonical task fixture");
    let (config, state) = isolated_config(root);
    spawn_configured_server(config, std::sync::Arc::new(state))
}

fn spawn_configured_server(
    config: Config,
    state: std::sync::Arc<IsolatedState>,
) -> (tokio::io::DuplexStream, tokio::task::JoinHandle<Result<()>>) {
    let (server_transport, client_transport) = tokio::io::duplex(1 << 20);
    let task = tokio::spawn(async move {
        let _state = state;
        let service = RustCoderServer::new(config)?
            .serve(server_transport)
            .await?;
        service.waiting().await?;
        Ok(())
    });
    (client_transport, task)
}

fn spawn_local_docs_server(
    root: std::path::PathBuf,
    cache: std::path::PathBuf,
) -> (tokio::io::DuplexStream, tokio::task::JoinHandle<Result<()>>) {
    let (server_transport, client_transport) = tokio::io::duplex(1 << 20);
    let task = tokio::spawn(async move {
        let (mut config, state) = isolated_config(root);
        let _state = state;
        config.docs.fallback = agz_rust_coder::config::DocsFallback::Local;
        config.docs.cache_dir = cache;
        config.docs.timeout_ms = 60_000;
        let service = RustCoderServer::new(config)?
            .serve(server_transport)
            .await?;
        service.waiting().await?;
        Ok(())
    });
    (client_transport, task)
}

fn client_info(capabilities: ClientCapabilities) -> ClientInfo {
    ClientInfo::new(
        capabilities,
        Implementation::new("agz-rust-coder-task-test", "0.1.0"),
    )
}

#[tokio::test]
async fn task_result_matches_the_capability_free_sync_result() -> Result<()> {
    let root = std::fs::canonicalize(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/stage7/clean"),
    )?;
    let (config, state) = isolated_config(root);
    let state = std::sync::Arc::new(state);
    let (task_transport, task_server) = spawn_configured_server(config.clone(), state.clone());
    let task_client = client_info(ClientCapabilities::builder().enable_tasks().build())
        .serve_with_lifecycle(
            task_transport,
            ClientLifecycleMode::Discover {
                preferred_versions: vec![ProtocolVersion::V_2026_07_28],
            },
        )
        .await?;

    let task = match task_client
        .call_tool_once(CallToolRequestParams::new("check"))
        .await?
    {
        CallToolResponse::Task(task) => task,
        other => anyhow::bail!("expected task response, got {other:?}"),
    };
    let task_result = loop {
        let current = task_client
            .peer()
            .get_task(GetTaskParams::new(task.task.task_id.clone()))
            .await?;
        match current.task.payload {
            TaskPayload::Completed { result } => {
                break serde_json::from_value::<CallToolResult>(serde_json::Value::Object(result))?;
            }
            TaskPayload::Failed { error } => anyhow::bail!("task failed: {error:?}"),
            TaskPayload::Cancelled => anyhow::bail!("task was cancelled"),
            TaskPayload::Working | TaskPayload::InputRequired { .. } => {
                tokio::time::sleep(std::time::Duration::from_millis(2)).await;
            }
            _ => anyhow::bail!("task returned an unsupported future payload"),
        }
    };

    let (sync_transport, sync_server) = spawn_configured_server(config, state);
    let sync_client = client_info(ClientCapabilities::default())
        .serve(sync_transport)
        .await?;
    let CallToolResponse::Complete(sync_result) = sync_client
        .call_tool_once(CallToolRequestParams::new("check"))
        .await?
    else {
        anyhow::bail!("capability-free client unexpectedly received a task");
    };

    let task_structured = task_result
        .structured_content
        .as_ref()
        .context("missing task structured result")?;
    let sync_structured = sync_result
        .structured_content
        .as_ref()
        .context("missing sync structured result")?;
    for pointer in [
        "/schemaVersion",
        "/tool",
        "/status",
        "/data/target",
        "/data/authority",
        "/data/inputHash",
        "/data/commandHash",
        "/data/environmentHash",
        "/data/cacheMode",
        "/data/scope",
        "/data/steps/0/target",
        "/data/steps/0/exitCode",
        "/data/steps/0/timedOut",
        "/data/steps/0/cancelled",
        "/data/steps/0/diagnostics",
        "/data/steps/0/drainComplete",
        "/data/steps/0/cleanupComplete",
        "/untrustedData",
    ] {
        assert_eq!(
            task_structured.pointer(pointer),
            sync_structured.pointer(pointer),
            "task/sync mismatch at {pointer}"
        );
    }
    assert_eq!(task_result.is_error, sync_result.is_error);
    assert_eq!(
        task_result
            .structured_content
            .as_ref()
            .and_then(|value| value.get("status"))
            .and_then(serde_json::Value::as_str)
            .context("missing task status")?,
        "FAST_PASS"
    );

    task_client.cancel().await?;
    sync_client.cancel().await?;
    task_server.await??;
    sync_server.await??;
    Ok(())
}

#[tokio::test]
async fn typed_tool_error_completes_the_task_instead_of_failing_it() -> Result<()> {
    let root = std::fs::canonicalize(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/stage7/clean"),
    )?;
    let (mut config, state) = isolated_config(root.clone());
    config.cargo.path = Some(root.join("missing-cargo"));
    let (transport, server) = spawn_configured_server(config, std::sync::Arc::new(state));
    let client = client_info(ClientCapabilities::builder().enable_tasks().build())
        .serve_with_lifecycle(
            transport,
            ClientLifecycleMode::Discover {
                preferred_versions: vec![ProtocolVersion::V_2026_07_28],
            },
        )
        .await?;
    let task = match client
        .call_tool_once(CallToolRequestParams::new("check"))
        .await?
    {
        CallToolResponse::Task(task) => task,
        other => anyhow::bail!("expected task response, got {other:?}"),
    };

    loop {
        let current = client
            .peer()
            .get_task(GetTaskParams::new(task.task.task_id.clone()))
            .await?;
        match current.task.payload {
            TaskPayload::Completed { result } => {
                let result = serde_json::from_value::<CallToolResult>(Value::Object(result))?;
                assert_eq!(result.is_error, Some(true));
                assert_eq!(
                    result
                        .structured_content
                        .as_ref()
                        .and_then(|value| value.get("status"))
                        .and_then(Value::as_str),
                    Some("INCONCLUSIVE")
                );
                break;
            }
            TaskPayload::Failed { error } => anyhow::bail!("typed result failed task: {error:?}"),
            TaskPayload::Cancelled => anyhow::bail!("typed result cancelled task"),
            TaskPayload::Working | TaskPayload::InputRequired { .. } => {
                tokio::time::sleep(std::time::Duration::from_millis(2)).await;
            }
            _ => anyhow::bail!("task returned an unsupported future payload"),
        }
    }

    client.cancel().await?;
    server.await??;
    Ok(())
}

#[tokio::test]
async fn compiler_failure_completes_with_is_error_false() -> Result<()> {
    let (transport, server) = spawn_server_for("broken");
    let client = client_info(ClientCapabilities::builder().enable_tasks().build())
        .serve_with_lifecycle(
            transport,
            ClientLifecycleMode::Discover {
                preferred_versions: vec![ProtocolVersion::V_2026_07_28],
            },
        )
        .await?;
    let task = match client
        .call_tool_once(CallToolRequestParams::new("check"))
        .await?
    {
        CallToolResponse::Task(task) => task,
        other => anyhow::bail!("expected task response, got {other:?}"),
    };

    loop {
        let current = client
            .peer()
            .get_task(GetTaskParams::new(task.task.task_id.clone()))
            .await?;
        match current.task.payload {
            TaskPayload::Completed { result } => {
                let result = serde_json::from_value::<CallToolResult>(Value::Object(result))?;
                assert_eq!(result.is_error, Some(false));
                assert_eq!(
                    result
                        .structured_content
                        .as_ref()
                        .and_then(|value| value.get("status"))
                        .and_then(Value::as_str),
                    Some("FAIL")
                );
                break;
            }
            TaskPayload::Failed { error } => {
                anyhow::bail!("compiler failure failed task: {error:?}")
            }
            TaskPayload::Cancelled => anyhow::bail!("compiler failure cancelled task"),
            TaskPayload::Working | TaskPayload::InputRequired { .. } => {
                tokio::time::sleep(std::time::Duration::from_millis(2)).await;
            }
            _ => anyhow::bail!("task returned an unsupported future payload"),
        }
    }

    let (sync_transport, sync_server) = spawn_server_for("broken");
    let sync_client = client_info(ClientCapabilities::default())
        .serve(sync_transport)
        .await?;
    let CallToolResponse::Complete(sync_result) = sync_client
        .call_tool_once(CallToolRequestParams::new("check"))
        .await?
    else {
        anyhow::bail!("capability-free client unexpectedly received a task");
    };
    assert_eq!(sync_result.is_error, Some(false));
    assert_eq!(
        sync_result
            .structured_content
            .as_ref()
            .and_then(|value| value.get("status"))
            .and_then(Value::as_str),
        Some("FAIL")
    );

    client.cancel().await?;
    sync_client.cancel().await?;
    server.await??;
    sync_server.await??;
    Ok(())
}

#[tokio::test]
async fn cancelling_a_docs_task_stops_the_real_local_cargo_process() -> Result<()> {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_nanos();
    let temp = std::fs::canonicalize(std::env::temp_dir())?;
    // Preserve the deep canonical cache path: this also regresses MSVC
    // rejection of a verbatim Cargo --target-dir argument.
    let root = temp.join(format!(
        "agz-rust-coder-docs-task-{}-{stamp}",
        std::process::id()
    ));
    let cache = temp.join(format!(
        "agz-rust-coder-docs-task-cache-{}-{stamp}",
        std::process::id()
    ));
    std::fs::create_dir_all(root.join("src"))?;
    std::fs::write(
        root.join("Cargo.toml"),
        "[package]\nname = \"cancel-docs\"\nversion = \"0.1.0\"\nedition = \"2024\"\nbuild = \"build.rs\"\n\n[workspace]\n",
    )?;
    std::fs::write(
        root.join("Cargo.lock"),
        "version = 4\n\n[[package]]\nname = \"cancel-docs\"\nversion = \"0.1.0\"\n",
    )?;
    std::fs::write(root.join("src/lib.rs"), "pub struct CancelDocs;\n")?;
    let pid_file = root.join("build-script.pid");
    std::fs::write(
        root.join("build.rs"),
        format!(
            "fn main() {{ std::fs::write({pid_file:?}, std::process::id().to_string()).unwrap(); std::thread::sleep(std::time::Duration::from_secs(60)); }}\n"
        ),
    )?;
    let (transport, server) = spawn_local_docs_server(root.clone(), cache.clone());
    let client = client_info(ClientCapabilities::builder().enable_tasks().build())
        .serve_with_lifecycle(
            transport,
            ClientLifecycleMode::Discover {
                preferred_versions: vec![ProtocolVersion::V_2026_07_28],
            },
        )
        .await?;
    let mut arguments = Map::new();
    arguments.insert("crate".to_owned(), Value::String("cancel-docs".to_owned()));
    arguments.insert("expensiveFallback".to_owned(), Value::Bool(true));
    let task = match client
        .call_tool_once(CallToolRequestParams::new("docs").with_arguments(arguments))
        .await?
    {
        CallToolResponse::Task(task) => task,
        other => anyhow::bail!("expected docs task response, got {other:?}"),
    };
    let mut build_pid = None;
    let startup_deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    while std::time::Instant::now() < startup_deadline {
        if let Ok(text) = std::fs::read_to_string(&pid_file) {
            build_pid = text.trim().parse::<u32>().ok();
            if build_pid.is_some() {
                break;
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    let current = client
        .peer()
        .get_task(GetTaskParams::new(task.task.task_id.clone()))
        .await?;
    let Some(build_pid) = build_pid else {
        client.cancel().await?;
        server.await??;
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&cache);
        anyhow::bail!("local cargo doc did not reach its build barrier; task={current:?}");
    };
    #[cfg(not(target_os = "linux"))]
    let _ = build_pid;
    let cancelled_at = std::time::Instant::now();
    client
        .peer()
        .cancel_task(CancelTaskParams::new(task.task.task_id.clone()))
        .await?;

    let mut terminal = None;
    for _ in 0..500 {
        let current = client
            .peer()
            .get_task(GetTaskParams::new(task.task.task_id.clone()))
            .await?;
        if current.task.status().is_terminal() {
            terminal = Some(current.task.status());
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert_eq!(terminal, Some(TaskStatus::Cancelled));
    assert!(cancelled_at.elapsed() < std::time::Duration::from_secs(5));
    #[cfg(target_os = "linux")]
    assert!(
        !std::fs::read_to_string(format!("/proc/{build_pid}/stat"))
            .ok()
            .is_some_and(|stat| {
                stat.rsplit_once(')')
                    .is_some_and(|(_, fields)| fields.split_whitespace().next() != Some("Z"))
            }),
        "cancelled build-script process {build_pid} is still alive"
    );

    client.cancel().await?;
    server.await??;
    let _ = std::fs::remove_dir_all(root);
    let _ = std::fs::remove_dir_all(cache);
    Ok(())
}
