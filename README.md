# AGZ Rust MCP

[![CI](https://github.com/ugur-murat-alt/agz-rust-mcp/actions/workflows/ci.yml/badge.svg)](https://github.com/ugur-murat-alt/agz-rust-mcp/actions/workflows/ci.yml)
[![crates.io](https://img.shields.io/crates/v/agz-rust-mcp.svg)](https://crates.io/crates/agz-rust-mcp)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

English | [Türkçe](README.tr.md)

**An AGZ Yazılım product.** `agz-rust-mcp` is a standalone stdio MCP server for
bounded Rust correctness grounded in Cargo and rustc. It runs bounded Cargo
validation, audits source, resolves exact-version crate documentation, provides
Rust Analyzer navigation, and returns write-free rename/refactor packages.

## Install

```bash
# npm wrapper (requires Node.js; resolves the matching release binary)
npx -y @agz-yazilim/agz-rust-mcp --version

# installer script (Linux x86_64): download install.sh and SHA256SUMS from the
# latest release, verify the script, then run it
bash install.sh

# crates.io (requires Rust 1.88.0 or newer)
cargo install agz-rust-mcp --locked
agz-rust-mcp --version
```

Prebuilt `.tar.gz` archives with `.sha256` files are available for Linux
x86_64, macOS arm64, and Windows x86_64. Releases up to `0.2.0` use the legacy
`agz-rust-coder-*` asset names; from `0.3.0` onward the names are
`agz-rust-mcp-*`.

**Step-by-step instructions, per-OS notes, checksum verification, and
troubleshooting: [docs/install.md](docs/install.md) ·
[Türkçe](docs/install.tr.md).**

## MCP Client Setup

The current source also bundles four offline skills: `agz-rust-workflow`,
`agz-rust-repair`, `agz-rust-refactor`, and `agz-rust-performance`. One executable
serves the tools and their workflows through MCP prompts/resources; a second MCP
is unnecessary. Filesystem-based clients can use `agz-rust-mcp skills export`.
See [bundled skills](docs/install.md#bundled-skills-current-source).
Bundled skills are available from `0.4.0`; `0.3.0` binaries do not include them.

Minimal OpenCode2 configuration (`opencode.jsonc`):

```jsonc
{
  "$schema": "https://opencode.ai/config.json",
  "mcp": {
    "servers": {
      "rust": {
        "type": "local",
        "command": ["agz-rust-mcp"],
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

Minimal Codex configuration (`~/.codex/config.toml`):

```toml
[mcp_servers.rust]
command = "agz-rust-mcp"
args = []
```

The canonical current directory is the default authorized root. Add explicit
roots with repeated `--allow-root` arguments when the client starts elsewhere;
client-provided MCP roots may narrow configured access but never widen it. See
[docs/install.md](docs/install.md#mcp-client-setup) for wrapper-managed client
variants.

## Documentation

Read the documentation in this order:

1. [Install and client setup](docs/install.md) - every install method and
   client configuration.
2. [Tool and configuration reference](docs/tools.md) - tools, actions, result
   semantics, and all configuration keys.
3. [Architecture](docs/architecture.md) - process model, protocol lifecycle,
   and authority boundaries.
4. [Validation and benchmark protocol](docs/benchmark.md) - smokes, gates, and
   evidence.
5. [Security policy](SECURITY.md) and [Contributing](CONTRIBUTING.md).

The bilingual index with the full reading path is
[docs/README.md](docs/README.md) / [docs/README.tr.md](docs/README.tr.md).

## Identity

| Contract | Value |
| --- | --- |
| Crate, binary, server | `agz-rust-mcp` |
| MCP Registry | `io.github.ugur-murat-alt/agz-rust-mcp` |
| Source version | `0.4.0` |
| First release | `0.1.0` |
| Release tag | `agz-rust-mcp-v<version>` |
| Rust edition / MSRV | `2024` / `1.88.0` |
| Rust MCP SDK | `rmcp` `3.1.4` |
| Default / discovered protocol | `2025-11-25` / `2026-07-28` |

MCP package ownership marker: `mcp-name: io.github.ugur-murat-alt/agz-rust-mcp`.

## Tools

OpenCode2 commonly exposes grouped MCP tools as `rust_*`.

| MCP tool | OpenCode2 direct name | Default | Purpose |
| --- | --- | --- | --- |
| `check` | `rust_check` | `enabled` | Run bounded Cargo check, Clippy, tests, docs, or the full gate. |
| `profile` | `rust_profile` | `enabled` | Analyze observed Cargo rebuild behavior and compare bounded build evidence without claiming unmeasured speedups. |
| `audit` | `rust_audit` | `enabled` | Scan Rust source for bounded static findings. |
| `crate_lookup` | `rust_crate_lookup` | `enabled` | Verify a crate and optional exact version on crates.io. |
| `docs` | `rust_docs` | `enabled` | Resolve exact-version docs from cache, local sources, or docs.rs. |
| `context` | `rust_context` | `enabled` | Prepare, expand, or delta a revision-bound semantic context capsule with per-item reasons. |
| `api` | `rust_api` | `enabled` | Resolve an API signature from bounded analyzer evidence or type-check a candidate snippet in an isolated copy of the workspace configuration. |
| `explain` | `rust_explain` | `enabled` | Explain macro expansion provenance, trait obligations, or cfg enablement. |
| `verify` | `rust_verify` | `enabled` | Plan or run a bounded feature, target, toolchain, and stage matrix. |
| `symbol` | `rust_symbol` | `enabled` | Read Rust Analyzer hover data for one symbol. |
| `references` | `rust_references` | `enabled` | Find bounded references. |
| `definition` | `rust_definition` | `enabled` | Find the selected definition. |
| `symbols` | `rust_symbols` | `enabled` | List symbols in one Rust file. |
| `implementations` | `rust_implementations` | `enabled` | Find implementations. |
| `hierarchy` | `rust_hierarchy` | `enabled` | Trace a bounded call hierarchy. |
| `rename` | `rust_rename` | `enabled` | Produce a verified rename edit package without applying it. |
| `refactor` | `rust_refactor` | `enabled` | Produce a verified refactor edit package without applying it. |
| `change` | `rust_change` | `enabled` | Create, stage, migrate, and validate a revision-bound changeset in server-owned scratch without writing the workspace. |
| `repair` | `rust_repair` | `enabled` | Analyze, try, compare, and minimize compiler-driven repair candidates for a failing change revision without writing the workspace. |
| `work` | `rust_work` | `enabled` | Drive a typed intent through change/validate with explicit gates and budgets, returning honest gate evidence or a bounded single-use handoff. |

Every tool returns deterministic structured data plus an equivalent bounded text
fallback. External data stays under `untrustedData`. Expected domain outcomes
such as compiler failure, missing crates, or unavailable docs are typed results;
invalid input, authorization failure, resource exhaustion, and unavailable
semantic infrastructure are protocol errors.

## Configuration

Precedence is CLI, then `AGZ_RUST_MCP_*` environment variables, then the
explicit `--config` TOML file, then defaults. Environment keys use `__` between
sections, for example `AGZ_RUST_MCP_GATE__HARD_TIMEOUT_MS=600000`.

| Key | Default | Meaning |
| --- | --- | --- |
| `server.allow_roots` | canonical CWD | Workspace read/command boundary. |
| `server.allow_dependency_roots` | empty | Explicit external path-dependency roots. |
| `gate.hard_timeout_ms` | `600000` | Cargo operation deadline. |
| `gate.scope` | `shadow` | Validation target: `workspace`, `shadow`, or `affected`. |
| `gate.cache` | `auto` | Cache policy: `auto`, `project`, or `isolated`. |
| `rust_analyzer.workspace_code` | `deny` | Reject RA startup unless workspace code is disabled. |
| `docs.fallback` | `auto` | Documentation source policy. |
| `profile.max_report_bytes` | `4194304` | Bounded read/store cap for one Cargo timing artifact. |
| `profile.max_runs` | `4` | Fresh Cargo runs available to one `profile` call. |
| `profile.compare_samples` | `3` | Required samples per side before any speed claim. |
| `limits.tool_output_bytes` | `49152` | Maximum serialized tool result size. |
| `change.max_bytes` | `268435456` | Maximum captured candidate bytes per changeset. |
| `repair.max_candidates` | `4` | Candidates one `repair` action may try. |
| `repair.max_compiles` | `4` | Cargo validations one `repair` action may run. |
| `repair.wall_time_ms` | `120000` | Wall-clock budget for one `repair` action. |
| `repair.minimize_max_candidates` | `32` | Compile-evaluated reduction attempts one `repair(action=minimize)` may try. |
| `repair.minimize_max_compiles` | `16` | Cargo runs one `repair(action=minimize)` may execute, including reproduction and export verification. |
| `work.max_candidates` | `4` | Host candidate revisions one work item may stage. |
| `work.max_compiles` | `12` | Gate validations one work item may run. |
| `work.wall_time_ms` | `600000` | Wall-time budget for one work item. |
| `telemetry.enabled` | `true` | Bounded local activity records without prompts or source. |

`profile` evidence is bounded: the latest 64 records stay in memory, and
persisted timing artifacts under the server-owned `profile-evidence` directory
are pruned after 24 hours or beyond 64 files. The `profile` result reports this
retention policy, so expired evidence is visible instead of silently reused.

Run `agz-rust-mcp --help` for every CLI field. The complete behavior and
default table is in [docs/tools.md](docs/tools.md#configuration-reference).

## Security

The server does not modify workspace source, but it is not an operating-system
sandbox. Cargo build scripts, tests, procedural macros, local rustdoc, and
opted-in Rust Analyzer workspace code execute with the server user's authority.
Use a container or OS sandbox when that boundary is required. Workspace,
dependency, cache, lease, journal, docs, and telemetry paths are canonicalized,
bounded, and fail closed, and stdout is reserved for MCP framing. Report
vulnerabilities privately as described in [SECURITY.md](SECURITY.md).

## Links

- Repository: https://github.com/ugur-murat-alt/agz-rust-mcp
- Crate: https://crates.io/crates/agz-rust-mcp
- SDK docs: https://docs.rs/rmcp/3.1.4/rmcp/
- MCP `2025-11-25`: https://modelcontextprotocol.io/specification/2025-11-25
- MCP `2026-07-28`: https://modelcontextprotocol.io/specification/2026-07-28

## License

[MIT](LICENSE), Copyright (c) 2026 Ugur Murat Altintas.
