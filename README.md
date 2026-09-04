# agz-rust-coder

[![CI](https://github.com/ugur-murat-alt/agz-rust-coder/actions/workflows/ci.yml/badge.svg)](https://github.com/ugur-murat-alt/agz-rust-coder/actions/workflows/ci.yml)
[![crates.io](https://img.shields.io/crates/v/agz-rust-coder.svg)](https://crates.io/crates/agz-rust-coder)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

English | [Turkce](README.tr.md)

`agz-rust-coder` is a standalone stdio MCP server for compiler-grounded Rust
work. It runs bounded Cargo validation, audits source, resolves exact-version
crate documentation, provides Rust Analyzer navigation, and returns write-free
rename/refactor packages.

## Identity

| Contract | Value |
| --- | --- |
| Crate, binary, server | `agz-rust-coder` |
| MCP Registry | `io.github.ugur-murat-alt/agz-rust-coder` |
| Current release | `0.1.1` |
| First release | `0.1.0` |
| Release tag | `agz-rust-coder-v<version>` |
| Rust edition / MSRV | `2024` / `1.88.0` |
| Rust MCP SDK | `rmcp` `3.1.4` |
| Default / discovered protocol | `2025-11-25` / `2026-07-28` |

MCP package ownership marker: `mcp-name: io.github.ugur-murat-alt/agz-rust-coder`.

## Install

```bash
cargo install agz-rust-coder --locked
agz-rust-coder --version
```

The package is source-distributed through crates.io. Release pages also provide
prebuilt archives and SHA-256 checksums.

## OpenCode

Add the installed binary to `opencode.jsonc`:

```jsonc
{
  "$schema": "https://opencode.ai/config.json",
  "mcp": {
    "servers": {
      "rust": {
        "type": "local",
        "command": ["agz-rust-coder"],
        "cwd": ".",
        "codemode": false,
        "timeout": {
          "startup": 30000,
          "catalog": 30000,
          "execution": 720000
        }
      }
    }
  }
}
```

The canonical current directory is the default authorized root. Add explicit
roots with repeated `--allow-root` arguments when the client starts elsewhere.
Client-provided MCP roots may narrow configured access but never widen it.

## Tools

OpenCode commonly exposes grouped MCP tools as `rust_*`.

| MCP tool | OpenCode direct name | Default | Purpose |
| --- | --- | --- | --- |
| `check` | `rust_check` | `enabled` | Run bounded Cargo check, Clippy, tests, docs, or the full gate. |
| `audit` | `rust_audit` | `enabled` | Scan Rust source for bounded static findings. |
| `crate_lookup` | `rust_crate_lookup` | `enabled` | Verify a crate and optional exact version on crates.io. |
| `docs` | `rust_docs` | `enabled` | Resolve exact-version docs from cache, local sources, or docs.rs. |
| `symbol` | `rust_symbol` | `enabled` | Read Rust Analyzer hover data for one symbol. |
| `references` | `rust_references` | `enabled` | Find bounded references. |
| `definition` | `rust_definition` | `enabled` | Find the selected definition. |
| `symbols` | `rust_symbols` | `enabled` | List symbols in one Rust file. |
| `implementations` | `rust_implementations` | `enabled` | Find implementations. |
| `hierarchy` | `rust_hierarchy` | `enabled` | Trace a bounded call hierarchy. |
| `rename` | `rust_rename` | `enabled` | Produce a verified rename edit package without applying it. |
| `refactor` | `rust_refactor` | `enabled` | Produce a verified refactor edit package without applying it. |

Every tool returns deterministic structured data plus an equivalent bounded text
fallback. External data stays under `untrustedData`. Expected domain outcomes
such as compiler failure, missing crates, or unavailable docs are typed results;
invalid input, authorization failure, resource exhaustion, and unavailable
semantic infrastructure are protocol errors.

## Configuration

Precedence is CLI, then `AGZ_RUST_CODER_*` environment variables, then the
explicit `--config` TOML file, then defaults. Environment keys use `__` between
sections, for example `AGZ_RUST_CODER_GATE__HARD_TIMEOUT_MS=600000`.

| Key | Default | Meaning |
| --- | --- | --- |
| `server.allow_roots` | canonical CWD | Workspace read/command boundary. |
| `server.allow_dependency_roots` | empty | Explicit external path-dependency roots. |
| `gate.hard_timeout_ms` | `600000` | Cargo operation deadline. |
| `gate.scope` | `shadow` | Validation target: `workspace`, `shadow`, or `affected`. |
| `gate.cache` | `auto` | Cache policy: `auto`, `project`, or `isolated`. |
| `rust_analyzer.workspace_code` | `deny` | Reject RA startup unless workspace code is disabled. |
| `docs.fallback` | `auto` | Documentation source policy. |
| `limits.tool_output_bytes` | `49152` | Maximum serialized tool result size. |
| `telemetry.enabled` | `true` | Bounded local activity records without prompts or source. |

Run `agz-rust-coder --help` for every CLI field. The complete behavior and
default table is in [docs/tools.md](docs/tools.md).

## Security

The server does not modify workspace source, but it is not an operating-system
sandbox. Cargo build scripts, tests, procedural macros, local rustdoc, and
opted-in Rust Analyzer workspace code execute with the server user's authority.
Use a container or OS sandbox when that boundary is required.

- Workspace and dependency paths are canonicalized and fail closed.
- Cache, lease, journal, docs, and telemetry paths cannot overlap authorized
  roots.
- Child output, HTTP bodies, directory walks, edits, tasks, and telemetry are
  bounded.
- `rename`, `refactor`, and formatting checks return data only.
- Stdout is reserved for MCP framing; logs and panic output use stderr.

Report vulnerabilities privately as described in [SECURITY.md](SECURITY.md).

## Development

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
cargo test --workspace --all-targets --all-features --locked --no-fail-fast
cargo +1.88.0 check --workspace --all-targets --all-features --locked
cargo build --release --locked
cargo run -p xtask -- protocol-smoke
cargo run -p xtask -- opencode-smoke
cargo run -p xtask -- benchmark-smoke
```

See [CONTRIBUTING.md](CONTRIBUTING.md), [architecture](docs/architecture.md),
[tool reference](docs/tools.md), [benchmark protocol](docs/benchmark.md), and
[CHANGELOG.md](CHANGELOG.md).

## Canonical Links

- Repository: https://github.com/ugur-murat-alt/agz-rust-coder
- Crate: https://crates.io/crates/agz-rust-coder
- SDK docs: https://docs.rs/rmcp/3.1.4/rmcp/
- MCP `2025-11-25`: https://modelcontextprotocol.io/specification/2025-11-25
- MCP `2026-07-28`: https://modelcontextprotocol.io/specification/2026-07-28

## License

[MIT](LICENSE), Copyright (c) 2026 Ugur Murat Altintas.
