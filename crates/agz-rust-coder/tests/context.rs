//! Protocol-level tests for the `context` tool: registration, bounded output,
//! paginated expand, typed NOT_FOUND, and offline rust-analyzer degradation.

use std::{
    fs,
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

use agz_rust_coder::{Config, RustCoderServer};
use anyhow::{Context, Result, bail};
use rmcp::{
    ServiceExt,
    model::{
        CallToolRequestParams, CallToolResponse, ClientCapabilities, ClientInfo, Implementation,
    },
    service::{RoleClient, RunningService},
};
use serde_json::{Map, Value, json};

static NEXT_STATE_ID: AtomicU64 = AtomicU64::new(0);

struct IsolatedState(PathBuf);

impl Drop for IsolatedState {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn fixture_config() -> (Config, IsolatedState) {
    let root = fs::canonicalize(
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
            "agz-rust-coder-context-protocol-{}-{stamp}-{}",
            std::process::id(),
            NEXT_STATE_ID.fetch_add(1, Ordering::Relaxed)
        ));
    let mut config = Config::defaults_at(root);
    config.gate.cache_dir = state.join("gate");
    config.gate.lease_dir = state.join("leases");
    config.docs.cache_dir = state.join("docs");
    config.telemetry.enabled = false;
    config.telemetry.path = state.join("activity.jsonl");
    // Force the offline path so the test never starts a real analyzer.
    config.rust_analyzer.path = Some(state.join("missing-rust-analyzer"));
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

fn client_info() -> ClientInfo {
    ClientInfo::new(
        ClientCapabilities::default(),
        Implementation::new("agz-rust-coder-context-test", "0.1.0"),
    )
}

fn arguments(entries: &[(&str, Value)]) -> Map<String, Value> {
    entries
        .iter()
        .map(|(key, value)| ((*key).to_owned(), value.clone()))
        .collect()
}

fn call(
    client: &RunningService<RoleClient, ClientInfo>,
    entries: &[(&str, Value)],
) -> impl std::future::Future<Output = Result<CallToolResponse, rmcp::ServiceError>> {
    client.call_tool_once(CallToolRequestParams::new("context").with_arguments(arguments(entries)))
}

async fn call_complete(
    client: &RunningService<RoleClient, ClientInfo>,
    entries: &[(&str, Value)],
) -> Result<rmcp::model::CallToolResult> {
    match call(client, entries).await? {
        CallToolResponse::Complete(result) => Ok(result),
        _ => bail!("context unexpectedly returned a task"),
    }
}

fn structured(result: &rmcp::model::CallToolResult) -> Result<&Value> {
    let structured = result
        .structured_content
        .as_ref()
        .context("missing structured content")?;
    let text = result
        .content
        .first()
        .and_then(|content| content.as_text())
        .context("missing text fallback")?;
    let parsed: Value = serde_json::from_str(&text.text)?;
    assert_eq!(&parsed, structured, "text and structured payloads differ");
    Ok(structured)
}

fn file_anchor() -> Value {
    json!([{"kind": "file", "file": "src/lib.rs", "range": {"startLine": 1, "endLine": 4}}])
}

#[tokio::test]
async fn prepare_expand_and_delta_are_bounded_and_typed() -> Result<()> {
    let (config, _state) = fixture_config();
    let (transport, server_task) = spawn_server(config);
    let client = client_info().serve(transport).await?;

    let tools = client.peer().list_tools(None).await?;
    assert!(
        tools.tools.iter().any(|tool| tool.name == "context"),
        "context tool must be registered"
    );

    let prepared = call_complete(
        &client,
        &[
            ("action", json!("prepare")),
            ("purpose", json!("api change fixture")),
            ("changeId", json!("change-1")),
            ("byteBudget", json!(4096)),
            (
                "anchors",
                json!([
                    {"kind": "file", "file": "src/lib.rs", "range": {"startLine": 1, "endLine": 4}},
                    {"kind": "symbol", "symbol": "answer", "file": "src/lib.rs"}
                ]),
            ),
        ],
    )
    .await?;
    assert_ne!(prepared.is_error, Some(true), "prepare must not fail");
    let prepared = structured(&prepared)?;
    assert_eq!(prepared["status"], "OK");
    assert_eq!(prepared["untrustedData"], true);
    assert_eq!(prepared["data"]["action"], "prepare");
    let capsule_id = prepared["data"]["capsuleId"]
        .as_str()
        .context("capsuleId")?
        .to_owned();
    assert!(
        prepared["data"]["items"]
            .as_array()
            .is_some_and(|items| !items.is_empty()),
        "prepare must select items"
    );
    assert!(
        prepared["data"]["omitted"]
            .as_array()
            .is_some_and(|omitted| !omitted.is_empty()),
        "offline anchors must produce explicit omissions"
    );
    assert!(
        prepared["data"]["items"]
            .as_array()
            .is_some_and(|items| items.iter().any(|item| item["kind"] == "package")),
        "cargo metadata package evidence must be selected for the fixture"
    );
    assert!(
        prepared["data"]["identity"]["sourceHashes"]
            .as_object()
            .is_some_and(|hashes| !hashes.is_empty()),
        "capsule identity must bind source hashes"
    );
    assert!(
        prepared["data"]["sizes"]["bytes"]
            .as_u64()
            .is_some_and(|bytes| bytes <= 4096),
        "byte budget must bound the payload"
    );
    assert_eq!(
        prepared["data"]["sizes"]["tokenizerAvailable"],
        Value::Bool(false)
    );

    let expanded = call_complete(
        &client,
        &[
            ("action", json!("expand")),
            ("capsuleId", json!(capsule_id)),
            ("cursor", json!(0)),
            ("pageSize", json!(1)),
            ("byteBudget", json!(4096)),
        ],
    )
    .await?;
    assert_ne!(expanded.is_error, Some(true));
    let expanded = structured(&expanded)?;
    assert_eq!(expanded["status"], "OK");
    assert_eq!(expanded["data"]["page"]["limit"], 1);
    assert_eq!(
        expanded["data"]["items"]
            .as_array()
            .map(Vec::len)
            .unwrap_or_default(),
        1
    );

    let delta = call_complete(
        &client,
        &[
            ("action", json!("delta")),
            ("previousCapsuleId", json!(prepared["data"]["capsuleId"])),
            ("anchors", file_anchor()),
            ("byteBudget", json!(4096)),
        ],
    )
    .await?;
    assert_ne!(delta.is_error, Some(true));
    let delta = structured(&delta)?;
    assert_eq!(delta["status"], "OK");
    assert!(delta["data"]["delta"].is_object());
    assert!(
        delta["data"]["items"].as_array().is_some_and(Vec::is_empty),
        "delta returns only the delta report"
    );

    let missing = call_complete(
        &client,
        &[
            ("action", json!("expand")),
            ("capsuleId", json!("no-such-capsule")),
        ],
    )
    .await?;
    assert_eq!(missing.is_error, Some(true));
    let missing = structured(&missing)?;
    assert_eq!(missing["status"], "NOT_FOUND");
    assert!(missing["data"]["delta"].is_null());

    client.cancel().await?;
    server_task.await??;
    Ok(())
}

#[tokio::test]
async fn invalid_context_arguments_are_rejected_without_echoing_input() -> Result<()> {
    let (config, _state) = fixture_config();
    let (transport, server_task) = spawn_server(config);
    let client = client_info().serve(transport).await?;

    let missing_capsule = call(&client, &[("action", json!("expand"))]).await;
    assert!(missing_capsule.is_err(), "expand requires capsuleId");

    let empty_anchors = call(&client, &[("action", json!("prepare"))]).await;
    assert!(empty_anchors.is_err(), "prepare requires anchors");

    let tiny_budget = call(
        &client,
        &[
            ("action", json!("prepare")),
            ("anchors", file_anchor()),
            ("byteBudget", json!(100)),
        ],
    )
    .await;
    assert!(
        tiny_budget.is_err(),
        "byteBudget below the floor is invalid"
    );

    let too_many = call(
        &client,
        &[
            ("action", json!("prepare")),
            (
                "anchors",
                Value::Array(vec![json!({"kind": "file", "file": "src/lib.rs"}); 9]),
            ),
        ],
    )
    .await;
    let error = too_many.expect_err("anchor cap must be enforced");
    let message = format!("{error:?}");
    assert!(!message.contains("src/lib.rs"), "input must not be echoed");
    if let rmcp::ServiceError::McpError(data) = error {
        assert_eq!(data.message, "anchors accepts at most 8 items");
    } else {
        bail!("expected an MCP invalid-params error");
    }

    client.cancel().await?;
    server_task.await??;
    Ok(())
}

#[tokio::test]
async fn context_tool_can_be_disabled_by_configuration() -> Result<()> {
    let (mut config, _state) = fixture_config();
    config.tools.context = false;
    let (transport, server_task) = spawn_server(config);
    let client = client_info().serve(transport).await?;

    let tools = client.peer().list_tools(None).await?;
    assert!(
        !tools.tools.iter().any(|tool| tool.name == "context"),
        "disabled context tool must not be registered"
    );

    client.cancel().await?;
    server_task.await??;
    Ok(())
}

#[tokio::test]
async fn capability_free_client_gets_a_synchronous_result() -> Result<()> {
    let (config, _state) = fixture_config();
    let (transport, server_task) = spawn_server(config);
    let client = client_info().serve(transport).await?;

    let result = client
        .call_tool(
            CallToolRequestParams::new("context").with_arguments(arguments(&[
                ("action", json!("prepare")),
                ("anchors", file_anchor()),
            ])),
        )
        .await?;
    let structured = structured(&result)?;
    assert_eq!(structured["status"], "OK");

    client.cancel().await?;
    server_task.await??;
    Ok(())
}
