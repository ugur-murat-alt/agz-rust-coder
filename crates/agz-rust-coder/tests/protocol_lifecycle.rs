use std::{
    fs,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use agz_rust_coder::{Config, RustCoderServer};
use anyhow::{Context, Result};
use rmcp::{
    ClientLifecycleMode, ClientServiceExt, ServiceError, ServiceExt,
    model::{
        CallToolRequestParams, CallToolResponse, ClientCapabilities, ClientInfo, ErrorCode,
        GetPromptRequestParams, GetPromptResponse, Implementation, ProtocolVersion,
        ReadResourceRequestParams, ReadResourceResponse,
    },
};

static NEXT_STATE_ID: AtomicU64 = AtomicU64::new(0);

struct IsolatedState(PathBuf);

impl Drop for IsolatedState {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn fixture_config() -> (Config, IsolatedState) {
    let root = std::fs::canonicalize(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/stage7/clean"),
    )
    .expect("canonical stage7 clean fixture");
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock is after the Unix epoch")
        .as_nanos();
    let state = fs::canonicalize(std::env::temp_dir())
        .expect("canonical temp directory")
        .join(format!(
            "agz-rust-coder-lifecycle-state-{}-{stamp}-{}",
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

fn client_info(capabilities: ClientCapabilities) -> ClientInfo {
    ClientInfo::new(
        capabilities,
        Implementation::new("agz-rust-coder-test", "0.1.0"),
    )
}

#[tokio::test]
async fn initialize_lists_the_static_surface_and_guidance() -> Result<()> {
    let (config, _state) = fixture_config();
    let (transport, server_task) = spawn_server(config);
    let client = client_info(ClientCapabilities::default())
        .serve(transport)
        .await?;

    let peer_info = client.peer_info().context("missing server peer info")?;
    assert_eq!(peer_info.protocol_version, ProtocolVersion::V_2025_11_25);
    assert_eq!(
        peer_info
            .server_info
            .as_ref()
            .map(|info| info.name.as_str()),
        Some("agz-rust-coder")
    );
    assert!(
        peer_info
            .instructions
            .as_deref()
            .is_some_and(|text| text.contains("write-free"))
    );

    let tools = client.peer().list_tools(None).await?;
    let names: Vec<_> = tools.tools.iter().map(|tool| tool.name.as_ref()).collect();
    assert_eq!(
        names,
        [
            "check",
            "audit",
            "crate_lookup",
            "docs",
            "symbol",
            "references",
            "definition",
            "symbols",
            "implementations",
            "hierarchy",
            "rename",
            "refactor",
        ]
    );
    assert!(tools.tools.iter().all(|tool| tool.output_schema.is_some()));

    let prompts = client.peer().list_prompts(None).await?;
    assert_eq!(prompts.prompts.len(), 1);
    let prompt = client
        .peer()
        .get_prompt_once(GetPromptRequestParams::new("workflow"))
        .await?;
    assert!(matches!(prompt, GetPromptResponse::Complete(_)));

    let resources = client.peer().list_resources(None).await?;
    assert_eq!(resources.resources.len(), 4);
    let resource = client
        .peer()
        .read_resource_once(ReadResourceRequestParams::new("rust-coder://workflow"))
        .await?;
    assert!(matches!(resource, ReadResourceResponse::Complete(_)));

    let result = client
        .call_tool_once(CallToolRequestParams::new("check"))
        .await?;
    let CallToolResponse::Complete(result) = result else {
        anyhow::bail!("capability-free client unexpectedly received a task");
    };
    let structured = result
        .structured_content
        .as_ref()
        .context("missing structured result")?;
    assert_eq!(result.is_error, Some(false));
    let text = result
        .content
        .first()
        .and_then(|content| content.as_text())
        .context("missing text fallback")?;
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&text.text)?,
        *structured
    );

    client.cancel().await?;
    server_task.await??;
    Ok(())
}

#[tokio::test]
async fn discover_selects_the_requested_2026_version() -> Result<()> {
    let (config, _state) = fixture_config();
    let (transport, server_task) = spawn_server(config);
    let client = client_info(ClientCapabilities::default())
        .serve_with_lifecycle(
            transport,
            ClientLifecycleMode::Discover {
                preferred_versions: vec![ProtocolVersion::V_2026_07_28],
            },
        )
        .await?;

    assert_eq!(
        client
            .peer_info()
            .context("missing server peer info")?
            .protocol_version,
        ProtocolVersion::V_2026_07_28
    );

    client.cancel().await?;
    server_task.await??;
    Ok(())
}

#[tokio::test]
async fn missing_resource_code_tracks_the_negotiated_protocol() -> Result<()> {
    let (legacy_config, _legacy_state) = fixture_config();
    let (legacy_transport, legacy_server) = spawn_server(legacy_config);
    let legacy = client_info(ClientCapabilities::default())
        .serve(legacy_transport)
        .await?;
    let legacy_error = legacy
        .peer()
        .read_resource(ReadResourceRequestParams::new("rust-coder://missing"))
        .await
        .expect_err("missing legacy resource");
    assert_eq!(mcp_error_code(legacy_error), ErrorCode::RESOURCE_NOT_FOUND);
    legacy.cancel().await?;
    legacy_server.await??;

    let (modern_config, _modern_state) = fixture_config();
    let (modern_transport, modern_server) = spawn_server(modern_config);
    let modern = client_info(ClientCapabilities::default())
        .serve_with_lifecycle(
            modern_transport,
            ClientLifecycleMode::Discover {
                preferred_versions: vec![ProtocolVersion::V_2026_07_28],
            },
        )
        .await?;
    let modern_error = modern
        .peer()
        .read_resource(ReadResourceRequestParams::new("rust-coder://missing"))
        .await
        .expect_err("missing modern resource");
    assert_eq!(mcp_error_code(modern_error), ErrorCode::INVALID_PARAMS);
    modern.cancel().await?;
    modern_server.await??;
    Ok(())
}

fn mcp_error_code(error: ServiceError) -> ErrorCode {
    match error {
        ServiceError::McpError(data) => data.code,
        other => panic!("expected MCP error, got {other:?}"),
    }
}

#[tokio::test]
async fn shutdown_continues_after_a_waiter_is_cancelled_and_is_reusable() -> Result<()> {
    let (config, _state) = fixture_config();
    let server = RustCoderServer::new(config)?;
    let state = Arc::clone(server.state());
    let first_state = Arc::clone(&state);
    let first = tokio::spawn(async move { first_state.shutdown_async().await });
    tokio::task::yield_now().await;
    first.abort();
    let _ = first.await;

    tokio::time::timeout(Duration::from_secs(5), state.shutdown_async())
        .await
        .context("shutdown retry timed out")??;
    state.shutdown_async().await?;
    assert!(state.is_shutting_down());
    Ok(())
}
