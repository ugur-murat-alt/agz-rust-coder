# Tool And Configuration Reference

This document defines the public tool and configuration surface of
`agz-rust-coder` `0.2.0`.

Request deadlines and cancellation also cover Git probes and input-identity
collection before and after Cargo. Git subprocesses use the shared process
supervisor; NUL-delimited paths are read from a bounded raw stdout prefix, not
from sanitized display text. Cancelled or timed-out Cargo runs do not launch post-validation Git probes.
Failed compilations are revalidated before offering edit/context evidence. Truncated tool envelopes retain their original
`status`, error flag, and `untrustedData` marker.

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

## Explicit validation options

The following additive `check` fields are available starting with **0.2.0**.
Omitting `options` retains the existing Cargo behavior. Examples are MCP
argument objects, not shell strings:

```json
{"target":"check","options":{"noDefaultFeatures":true,"features":["serde"],"context":true}}
```

```json
{"target":"test","options":{"runner":"nextest","testFilter":"parses_empty_input"}}
```

`options` accepts `features` (at most 64 names, each at most 128 bytes),
`allFeatures`, `noDefaultFeatures`, `targetTriple` (built-in target, not a JSON
file), `testFilter` (bounded test-name substring), `runner` (`cargo` or `nextest`),
`sccache`, and `context`. Unknown options, leading flags, control characters,
conflicting feature choices, or a test filter on `target=all` are rejected.
`allFeatures` enables one combined selection; it does not test all combinations.
A non-host target must already be installed. Cross-target test execution needs
a working Cargo runner configured by the operator; check success alone is not
an execution test. No toolchain is downloaded automatically.

`gate.scope` now applies to check, Clippy, test and doc development stages.
`all` always executes the workspace stages in the requested configuration.
Global/ambiguous input changes and explicit feature/platform choices widen
scope conservatively. `FULL_PASS` therefore means the recorded stages and
options passed, not every possible Rust configuration. A filtered Cargo test
without evidence of at least one executed libtest case is `INCONCLUSIVE`, even
if Cargo exits zero. Custom harness output that cannot establish this is also
inconclusive; it is not silently treated as test success.
Nextest rejects zero matching tests with `--no-tests=fail`.

Step evidence includes `evidence`, `diagnosticsOmitted`, `contexts`, and existing
output/cleanup flags. `firstDiagnosticMs` in a step is process-relative; the
request-level value includes preflight and queue time. A truncated log does not
necessarily mean lost compiler diagnostics. Conversely, malformed/oversized
records and omitted diagnostics are explicit. Provisional progress messages
are untrusted compiler text and never final results.

Context excerpts carry source hashes and exact resolved direct dependencies,
not speculative repair advice. `input-identity-matched` means complete pre/post
input identities agree. Files are not atomically snapshotted: recheck source
hashes or `old_string` before applying an edit. Failed compilations are also
revalidated before returning suggestions/context. Cancelled/timed-out/unclean
work never publishes usable edits. Source budgets and missing-context reasons
remain visible. The MCP still never applies an edit to workspace source.

Nextest must report 0.9.143 from a trusted absolute PATH directory outside every
configured workspace/dependency root. No silent runner fallback is performed.
For supervised Sccache, configure an absolute `RUSTC_WRAPPER` for version 0.17.0
and request `sccache=true`. This mode currently requires Unix; it owns a private
foreground local cache server and socket, uses client-side compilation, and
cleans the process tree before returning. It excludes remote/distributed cache
configuration, preserves incremental settings, and limits its local disk cache
to 256 MiB beneath `gate.lease_dir`. A long Unix socket path returns an actionable
error: choose a shorter `gate.lease_dir`. This is an opt-in safety-constrained
mode, not transparent support for arbitrary Sccache configurations.

The Rust library adds fields to public request/evidence structs. Consumers using
struct literals may need adjustments; prefer `GateRequest::new(...).with_options(...)`.
The MCP's prior input fields and default behavior are preserved and schema-tested.

See [the six-part plan](rust-efficiency-plan.md) and
[verification evidence](rust-efficiency-evidence.md).

On macOS and Windows, host leases with an unverifiable foreign PID are retained
rather than reclaimed. After a confirmed owner crash, an operator may need to
remove that stale lease while no validation process is using it. Linux retains
verified absent-PID recovery. With opt-in Sccache, metadata probes bypass only
RUSTC_WRAPPER; compilation still uses the owned, validated cache session.
