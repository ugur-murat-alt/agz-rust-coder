use std::{collections::BTreeMap, fs, path::Path};

const ENGLISH: &str = include_str!("../../../README.md");
const TURKISH: &str = include_str!("../../../README.tr.md");
const TOOLS_ENGLISH: &str = include_str!("../../../docs/tools.md");
const TOOLS_TURKISH: &str = include_str!("../../../docs/tools.tr.md");
const ARCHITECTURE_ENGLISH: &str = include_str!("../../../docs/architecture.md");
const ARCHITECTURE_TURKISH: &str = include_str!("../../../docs/architecture.tr.md");
const BENCHMARK_ENGLISH: &str = include_str!("../../../docs/benchmark.md");
const BENCHMARK_TURKISH: &str = include_str!("../../../docs/benchmark.tr.md");

#[test]
fn bilingual_public_contract_is_in_sync() {
    assert_contract(ENGLISH, TURKISH).expect("bilingual README contract");

    for relative in [
        "README.md",
        "README.tr.md",
        "docs/tools.md",
        "docs/tools.tr.md",
        "docs/architecture.md",
        "docs/architecture.tr.md",
        "docs/benchmark.md",
        "docs/benchmark.tr.md",
        "CHANGELOG.md",
        "CONTRIBUTING.md",
        "CODE_OF_CONDUCT.md",
        "CODE_OF_CONDUCT.tr.md",
        "SECURITY.md",
        "LICENSE",
        "server.json",
    ] {
        assert!(
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../..")
                .join(relative)
                .is_file(),
            "missing public document {relative}"
        );
    }
}

#[test]
fn drift_validator_rejects_version_config_tool_and_link_changes() {
    let wrong_version = TURKISH.replacen("`0.1.0`", "`9.9.9`", 1);
    assert!(assert_contract(ENGLISH, &wrong_version).is_err());

    let wrong_config = TURKISH.replacen("`49152`", "`49151`", 1);
    assert!(assert_contract(ENGLISH, &wrong_config).is_err());

    let wrong_tool = TURKISH.replacen("`rust_check`", "`rust_validate`", 1);
    assert!(assert_contract(ENGLISH, &wrong_tool).is_err());

    let wrong_link = TURKISH.replacen(
        "https://docs.rs/rmcp/3.1.4/rmcp/",
        "https://docs.rs/rmcp/latest/rmcp/",
        1,
    );
    assert!(assert_contract(ENGLISH, &wrong_link).is_err());
}

#[test]
fn paired_public_docs_preserve_machine_readable_contracts() {
    assert_shared_markers(
        TOOLS_ENGLISH,
        TOOLS_TURKISH,
        &[
            "`check`",
            "`audit`",
            "`crate_lookup`",
            "`docs`",
            "`symbol`",
            "`references`",
            "`definition`",
            "`symbols`",
            "`implementations`",
            "`hierarchy`",
            "`rename`",
            "`refactor`",
            "`gate.scope`",
            "`rust_analyzer.workspace_code`",
            "`telemetry.enabled`",
        ],
    );
    assert_shared_markers(
        ARCHITECTURE_ENGLISH,
        ARCHITECTURE_TURKISH,
        &["`CreateTaskResult`", "`2025-11-25`", "`2026-07-28`"],
    );
    assert_shared_markers(
        BENCHMARK_ENGLISH,
        BENCHMARK_TURKISH,
        &[
            "cargo run -p xtask -- protocol-smoke",
            "cargo run -p xtask -- opencode-smoke",
            "cargo run -p xtask -- benchmark-smoke",
            "`run_id`",
            "`source_commit`",
            "`source_checksum`",
            "`provider` / `model` / `variant`",
            "`non_inferiority_margin`",
        ],
    );
}

fn assert_shared_markers(english: &str, turkish: &str, markers: &[&str]) {
    for marker in markers {
        assert!(
            english.contains(marker),
            "English document missing {marker}"
        );
        assert!(
            turkish.contains(marker),
            "Turkish document missing {marker}"
        );
    }
}

fn assert_contract(english: &str, turkish: &str) -> Result<(), String> {
    for value in [
        env!("CARGO_PKG_VERSION"),
        "3.1.4",
        "2025-11-25",
        "2026-07-28",
        "1.88.0",
        "agz-rust-coder-v<version>",
    ] {
        let marker = format!("`{value}`");
        if english.matches(&marker).count() != turkish.matches(&marker).count() {
            return Err(format!("version/protocol marker drift: {marker}"));
        }
    }

    let english_tools = keyed_table(
        english,
        &[
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
        ],
    )?;
    let turkish_tools = keyed_table(
        turkish,
        &[
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
        ],
    )?;
    for (tool, english_row) in &english_tools {
        let turkish_row = turkish_tools
            .get(tool)
            .ok_or_else(|| format!("missing Turkish tool {tool}"))?;
        if english_row.get(..3) != turkish_row.get(..3) {
            return Err(format!("tool mapping drift: {tool}"));
        }
    }
    for direct_name in [
        "rust_check",
        "rust_audit",
        "rust_crate_lookup",
        "rust_docs",
        "rust_symbol",
        "rust_references",
        "rust_definition",
        "rust_symbols",
        "rust_implementations",
        "rust_hierarchy",
        "rust_rename",
        "rust_refactor",
    ] {
        if english.matches(direct_name).count() != turkish.matches(direct_name).count() {
            return Err(format!("OpenCode direct tool drift: {direct_name}"));
        }
    }

    let config_keys = [
        "server.allow_roots",
        "server.allow_dependency_roots",
        "gate.hard_timeout_ms",
        "gate.scope",
        "gate.cache",
        "rust_analyzer.workspace_code",
        "docs.fallback",
        "limits.tool_output_bytes",
        "telemetry.enabled",
    ];
    let english_config = keyed_table(english, &config_keys)?;
    let turkish_config = keyed_table(turkish, &config_keys)?;
    for key in config_keys {
        let english_row = english_config
            .get(key)
            .ok_or_else(|| format!("missing English config {key}"))?;
        let turkish_row = turkish_config
            .get(key)
            .ok_or_else(|| format!("missing Turkish config {key}"))?;
        if english_row.get(..2) != turkish_row.get(..2) {
            return Err(format!("config default drift: {key}"));
        }
    }

    for link in [
        "https://github.com/ugur-murat-alt/agz-rust-coder",
        "https://docs.rs/rmcp/3.1.4/rmcp/",
        "https://modelcontextprotocol.io/specification/2025-11-25",
        "https://modelcontextprotocol.io/specification/2026-07-28",
    ] {
        if !english.contains(link) || !turkish.contains(link) {
            return Err(format!("canonical link drift: {link}"));
        }
    }
    Ok(())
}

fn keyed_table(document: &str, keys: &[&str]) -> Result<BTreeMap<String, Vec<String>>, String> {
    let mut rows = BTreeMap::new();
    for line in document.lines().filter(|line| line.starts_with('|')) {
        let columns = line
            .trim_matches('|')
            .split('|')
            .map(|column| column.trim().trim_matches('`').to_owned())
            .collect::<Vec<_>>();
        let Some(key) = columns.first() else {
            continue;
        };
        if keys.contains(&key.as_str()) {
            rows.insert(key.clone(), columns);
        }
    }
    for key in keys {
        if !rows.contains_key(*key) {
            return Err(format!("missing table row {key}"));
        }
    }
    Ok(rows)
}

#[test]
fn package_readme_and_license_are_in_the_publish_allowlist() {
    let manifest = fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml"))
        .expect("read package manifest");
    assert!(manifest.contains("README.md"));
    assert!(manifest.contains("LICENSE"));
    assert!(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("README.md")
            .is_file()
    );
    assert!(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("LICENSE")
            .is_file()
    );

    let package_readme =
        fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("README.md"))
            .expect("read package README");
    assert!(package_readme.contains("mcp-name: io.github.ugur-murat-alt/agz-rust-coder"));
}

#[test]
fn mcp_registry_metadata_matches_the_cargo_package() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let metadata: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(root.join("server.json")).expect("read server.json"),
    )
    .expect("parse server.json");

    let expected = serde_json::json!({
        "$schema": "https://static.modelcontextprotocol.io/schemas/2025-12-11/server.schema.json",
        "name": "io.github.ugur-murat-alt/agz-rust-coder",
        "title": "AGZ Rust Coder",
        "description": "Bounded, source-write-free Rust correctness tools grounded in Cargo and rustc output.",
        "repository": {
            "url": "https://github.com/ugur-murat-alt/agz-rust-coder",
            "source": "github"
        },
        "version": env!("CARGO_PKG_VERSION"),
        "packages": [{
            "registryType": "cargo",
            "registryBaseUrl": "https://crates.io",
            "identifier": env!("CARGO_PKG_NAME"),
            "version": env!("CARGO_PKG_VERSION"),
            "transport": { "type": "stdio" }
        }]
    });

    assert_eq!(metadata, expected);

    let mut drifted = metadata;
    drifted["description"] = "different immutable metadata".into();
    assert_ne!(drifted, expected);
}
