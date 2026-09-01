#![forbid(unsafe_code)]

mod benchmark;
mod child_process;
mod evidence;
mod mcp;
mod opencode;

use anyhow::{Result, bail};

#[tokio::main]
async fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let Some(command) = args.next() else {
        print_help();
        bail!("a smoke command is required")
    };
    let extra: Vec<_> = args.collect();
    let root = evidence::repo_root()?;

    match command.as_str() {
        "protocol-smoke" if extra.is_empty() => protocol_smoke(&root).await,
        "opencode-smoke" if extra.is_empty() => opencode_smoke(&root).await,
        "benchmark-smoke" => benchmark::run(&root, extra).await,
        "--help" | "-h" if extra.is_empty() => {
            print_help();
            Ok(())
        }
        other => bail!("unknown or invalid xtask command: {other}"),
    }
}

async fn protocol_smoke(root: &std::path::Path) -> Result<()> {
    let evidence = mcp::protocol_smoke(root).await?;
    let run = serde_json::json!({
        "schema_version": 1,
        "run_id": "stage7-protocol-smoke",
        "mode": "provider-free",
        "transport": "stdio",
        "provider": null,
        "model": null,
        "variant": null,
        "fixture": "stage7/protocol/expectations.json",
        "protocol_versions": ["2025-11-25", "2026-07-28"],
    });
    let results = serde_json::to_value(evidence)?;
    let report = "# Stage 7 protocol smoke\n\nPASS: RMCP stdio initialize, discovery, tool output, task, and synchronous fallback checks passed without a provider or model request.\n";
    let output = evidence::publish("protocol-smoke", &run, &results, report)?;
    println!("protocol-smoke: PASS ({})", output.display());
    Ok(())
}

async fn opencode_smoke(root: &std::path::Path) -> Result<()> {
    let evidence = opencode::run(root).await?;
    let run = serde_json::json!({
        "schema_version": 1,
        "run_id": "stage7-opencode-smoke",
        "mode": "provider-free",
        "provider": "loopback-fake",
        "model": "stage7-fake-model",
        "variant": null,
        "fixture_set": "stage7/opencode",
        "tool_call_policy": "model requests use only the loopback fake provider; no external model endpoint is configured",
    });
    let results = serde_json::to_value(evidence)?;
    let report = "# Stage 7 OpenCode smoke\n\nPASS: direct and grouped local MCP configurations were validated by the pinned OpenCode host. Model requests were served only by the loopback fake provider; no paid or external request was permitted.\n";
    let output = evidence::publish("opencode-smoke", &run, &results, report)?;
    println!("opencode-smoke: PASS ({})", output.display());
    Ok(())
}

fn print_help() {
    println!("Usage: xtask <protocol-smoke|opencode-smoke|benchmark-smoke [--live]>");
}
