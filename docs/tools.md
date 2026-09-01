# Tool And Configuration Reference

This document defines the public tool and configuration surface of
`agz-rust-coder` `0.1.0`.

## Tool Catalog

| Tool | Authority | Side effects | Result |
| --- | --- | --- | --- |
| `check` | Cargo/rustc | May build in a bounded target directory | Validation status, command evidence, diagnostics, and timing data. |
| `audit` | Advisory scanner | Reads authorized Rust files | Bounded findings and skipped-file reasons. |
| `crate_lookup` | crates.io | Bounded HTTPS request | `FOUND`, `NOT_FOUND`, `VERSION_MISMATCH`, or `UNAVAILABLE`. |
| `docs` | rustdoc/docs.rs | May use cache, network, or local `cargo doc` | Exact-version excerpt and provenance or typed unavailability. |
| `symbol` | Rust Analyzer | Depends on workspace-code policy | Hover text and selected location. |
| `references` | Rust Analyzer | Depends on workspace-code policy | Bounded reference locations. |
| `definition` | Rust Analyzer | Depends on workspace-code policy | Selected definition location. |
| `symbols` | Rust Analyzer | Depends on workspace-code policy | Bounded document symbols. |
| `implementations` | Rust Analyzer | Depends on workspace-code policy | Bounded implementation locations. |
| `hierarchy` | Rust Analyzer | Depends on workspace-code policy | Bounded incoming/outgoing call graph. |
| `rename` | Rust Analyzer | Never writes source | Verified `old_string`/`new_string` edit package. |
| `refactor` | Rust Analyzer | Never writes source | Verified write-free refactor package. |

`check` targets are `check`, `clippy`, `test`, `doc`, `fmt`, and `all`. Formatting
uses check-only behavior. A completed explicit validation is never reused as
authority for a later request; only an active identical job may be joined.

All tools return equivalent structured and text representations within
`limits.tool_output_bytes`. Remote bodies and excerpts are bounded before
parsing. External content is emitted under `untrustedData` and is never added to
server instructions.

## Result Semantics

Expected domain outcomes are successful MCP calls with typed status:

- compiler or test failure: `FAIL`;
- crate absence, mismatch, or registry outage: `NOT_FOUND`,
  `VERSION_MISMATCH`, or `UNAVAILABLE`;
- missing or ambiguous symbols: `NOT_FOUND` or `AMBIGUOUS`;
- documentation fallback exhaustion: typed unavailable data.

Invalid arguments, unauthorized paths, resource limits, timeouts, and
unavailable semantic infrastructure use `isError=true`. Text and structured
status must agree.

## Tasks And Cancellation

`check` and `docs` support MCP tasks when negotiated. The server emits progress,
accepts `tasks/cancel`, propagates request, root-epoch, and shutdown cancellation,
and removes terminal task state after bounded retention. Synchronous fallback
remains available for clients without task support.

## Configuration Sources

The precedence order is CLI, `AGZ_RUST_CODER_*` environment, explicit TOML, and
defaults. Lists replace lower-priority values instead of appending. Unknown TOML
or environment keys fail startup.

Environment variables uppercase the field and use `__` between sections:
`gate.hard_timeout_ms` becomes `AGZ_RUST_CODER_GATE__HARD_TIMEOUT_MS`. Root lists
use the platform path-list separator.

## Configuration Reference

| Key | Default | Notes |
| --- | --- | --- |
| `server.allow_roots` | canonical CWD | Primary authorized workspace roots. |
| `server.allow_dependency_roots` | empty | External path-dependency roots. |
| `tools.check` | `true` | Register `check`. |
| `tools.audit` | `true` | Register `audit`. |
| `tools.crate_lookup` | `true` | Register `crate_lookup`. |
| `tools.docs` | `true` | Register `docs`. |
| `tools.lsp` | `true` | Register semantic navigation tools. |
| `tools.rename` | `true` | Register `rename` when LSP is enabled. |
| `tools.refactor` | `true` | Register `refactor` when LSP is enabled. |
| `cargo.path` | PATH `cargo` | Optional Cargo executable override. |
| `gate.hard_timeout_ms` | `600000` | One Cargo operation deadline. |
| `gate.debounce_ms` | `500` | Stable-input debounce. |
| `gate.host_concurrency` | `1` | Host-wide Cargo permits. |
| `gate.scope` | `shadow` | `workspace`, `shadow`, or `affected`. |
| `gate.cache` | `auto` | `auto`, `project`, or `isolated`. |
| `gate.min_free_disk_mb` | `1024` | Preflight disk floor. |
| `gate.min_available_memory_mb` | `512` | Preflight memory floor when the host exposes a reliable available-memory measurement (currently Linux). |
| `gate.cache_dir` | platform `agz-rust-coder/state/gate` | Server-owned Cargo cache. |
| `gate.lease_dir` | platform `agz-rust-coder/state/leases` | Host leases and process journal. |
| `rust_analyzer.path` | PATH or rustup | Optional binary override. |
| `rust_analyzer.timeout_ms` | `30000` | Semantic request deadline. |
| `rust_analyzer.idle_ms` | `900000` | Idle process lifetime. |
| `rust_analyzer.max_instances` | `2` | Concurrent workspace processes. |
| `rust_analyzer.check_hint` | `false` | Allow RA check hints. |
| `rust_analyzer.workspace_code` | `deny` | `deny` or explicit `allow`. |
| `docs.timeout_ms` | `300000` | Documentation resolution deadline. |
| `docs.fallback` | `auto` | `auto`, `local`, `network`, or `off`. |
| `docs.cache_dir` | platform `agz-rust-coder/docs` | Server-owned docs cache. |
| `limits.max_rename_edits` | `200` | Rename edit cap. |
| `limits.max_refactor_edits` | `200` | Refactor edit cap. |
| `limits.process_output_bytes` | `8388608` | Combined child-output cap. |
| `limits.tool_output_bytes` | `49152` | MCP tool-result cap. |
| `limits.max_in_flight_tools` | `32` | Concurrent tool admission. |
| `limits.max_active_tasks` | `16` | Running task cap. |
| `limits.max_retained_tasks` | `128` | Terminal task cap. |
| `limits.identity_files` | `20000` | Input identity file cap. |
| `limits.identity_file_bytes` | `33554432` | Per identity file cap. |
| `limits.identity_total_bytes` | `268435456` | Total identity byte cap. |
| `limits.external_files` | `5000` | External dependency file cap. |
| `limits.external_bytes` | `67108864` | External dependency byte cap. |
| `limits.git_output_bytes` | `8388608` | Git evidence cap. |
| `limits.audit_files` | `10000` | Audit file cap. |
| `limits.audit_file_bytes` | `2097152` | Per audit file cap. |
| `limits.audit_total_bytes` | `67108864` | Total audit byte cap. |
| `limits.audit_findings` | `200` | Audit finding cap. |
| `telemetry.enabled` | `true` | Enable local activity records. |
| `telemetry.path` | platform `agz-rust-coder/state/activity.jsonl` | Server-owned JSONL path. |
| `telemetry.retention_bytes` | `8388608` | Rotation threshold. |
| `telemetry.retention_days` | `7` | Age retention. |
| `telemetry.max_archives` | `3` | Archive cap. |

Server-owned paths must not overlap an authorized workspace or dependency root.
Telemetry records bounded operation metadata and never raw prompts, private
source, tool arguments, raw paths, or session identifiers.

## Rust Analyzer Policy

The default `rust_analyzer.workspace_code=deny` profile probes the running
server schema and disables build scripts, procedural macros, and check-on-save.
If that cannot be verified, semantic tools return unavailable without starting
the process. `allow` is an explicit opt-in to workspace code execution.

## Related Documents

- [README](../README.md)
- [Architecture](architecture.md)
- [Benchmark protocol](benchmark.md)
- [Security policy](../SECURITY.md)
