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
| `profile` | Cargo/rustc | Runs one bounded Cargo target with `--timings` and stores its bounded HTML report under server-owned evidence | Observed rebuild report, separated admission/preflight/Cargo phases, observed/reasoned-hypothesis/unknown explanations, and baseline/candidate comparison. |
| `audit` | Advisory scanner | Reads authorized Rust files | Bounded findings and skipped-file reasons. |
| `crate_lookup` | crates.io | Bounded HTTPS request | `FOUND`, `NOT_FOUND`, `VERSION_MISMATCH`, or `UNAVAILABLE`. |
| `docs` | rustdoc/docs.rs | May use cache, network, or local `cargo doc` | Exact-version excerpt and provenance or typed unavailability. |
| `context` | Rust Analyzer, workspace source, cargo metadata | Never writes source | Revision-bound capsule of definitions, consumers, tests, signatures, dependency/feature evidence, and bounded excerpts with per-item reasons. |
| `explain` | rustc/Cargo plus advisory Rust Analyzer | Runs a bounded Cargo check for `macro`/`trait`; metadata-only for `cfg` | Provenance-labelled fragments; missing expansion mapping is `unknown`, never guessed. |
| `symbol` | Rust Analyzer | Depends on workspace-code policy | Hover text and selected location. |
| `references` | Rust Analyzer | Depends on workspace-code policy | Bounded reference locations. |
| `definition` | Rust Analyzer | Depends on workspace-code policy | Selected definition location. |
| `symbols` | Rust Analyzer | Depends on workspace-code policy | Bounded document symbols. |
| `implementations` | Rust Analyzer | Depends on workspace-code policy | Bounded implementation locations. |
| `hierarchy` | Rust Analyzer | Depends on workspace-code policy | Bounded incoming/outgoing call graph. |
| `rename` | Rust Analyzer | Never writes source | Verified `old_string`/`new_string` edit package. |
| `refactor` | Rust Analyzer | Never writes source | Verified write-free refactor package. |
| `change` | Server-owned scratch + Cargo/rustc for candidate validation | Never writes the workspace; compiles only the candidate copy | Revision-bound change record with candidate hashes, validation evidence (bounded diagnostics and write-free suggestions for a fresh `FAIL`), and a verified/unverified export package. |
| `repair` | Server-owned scratch + Cargo/rustc for candidate validation and bounded minimization | Never writes the workspace; creates and compiles only temporary candidate copies | Grouped diagnostics with reasoned root-cause hypotheses and source-backed ownership evidence, per-candidate measured compile/test results, behavior/performance guards, a measured selection with residual risks, and an export-verified minimized reproducer with pinned configuration. |

`check` targets are `check`, `clippy`, `test`, `doc`, `fmt`, and `all`. Formatting
uses check-only behavior. A completed explicit validation is never reused as
authority for a later request; only an active identical job may be joined.

`profile` runs exactly one of `check`, `clippy`, `test`, or `doc`. It separates
protocol admission, scheduler queue, metadata/identity preflight, Cargo process
time, and finalization; parallel unit durations are never summed as wall time.
The stable Cargo `--timings` HTML report is stored as a bounded server-owned
artifact, and its embedded unit data is extracted only with the exact
version-bound shape. A missing, oversized, or malformed report produces a typed
`unavailable` result instead of guessed numbers, and missing Cargo telemetry is
never reported as cache hits. `buildAnalyze` returns one sample; `buildCompare`
records toolchain, hardware class, configuration, cache state, sample counts, and
the source-change binding, and returns `INCONCLUSIVE` for single-run, noisy,
mixed warm/cold, or insufficient samples. Warm and cold experiments are never
merged, CPU/I/O bottleneck types are not asserted without observation, and any
suggested feature/dependency/profile change remains a proposal that is never
applied automatically. Debug assertions and test scope are never disabled as a
hidden speedup.

`explain` actions are `macro`, `trait`, and `cfg`. Every fragment carries
`observedCompiler`, `advisoryAnalyzer`, `inferred`, or `unknown` provenance.
`macro` combines rustc expansion provenance retained in diagnostics with a
negotiated `rust-analyzer/expandMacro` response when the capability exists;
unsupported capabilities return `UNSUPPORTED_CAPABILITY`. `trait` reports
compiler expected/found text and failed bounds with a bounded source selection;
Rust Analyzer failed obligations are advisory and disagreements keep the
compiler side authoritative. `cfg` evaluates the source `#[cfg]` condition
against Cargo metadata features for the recorded selection and answers `unknown`
for target predicates that were not probed. Unsupported configurations are
never presented as verified, and proc-macro/build-script policy is never
elevated.

All tools return equivalent structured and text representations within
`limits.tool_output_bytes`. Remote bodies and excerpts are bounded before
parsing. External content is emitted under `untrustedData` and is never added to
server instructions.

## Changeset Scratch

`change` copies the complete authorized working tree, including dirty tracked
and untracked files, into a server-owned scratch directory; Git is not required.
The original workspace is never written. Captures report a bounded `excluded`
list (for example `.git`, the Cargo target directory, and server scratch), so an
input that depends on an excluded directory is visible rather than silently
assumed complete. `stage` validates every `oldString`/`newString` patch and new
file before applying anything, then publishes a durable applying marker before
the first candidate write and records the read-back hashes of every staged file
(exact single match, UTF-8, CRLF-strict bytes, relative in-candidate paths,
overlap rejection). `validate` requires `expectedRevision` and `baseIdentity`
(revision 0 is valid), re-hashes the recorded candidate files before starting
any Cargo process, and runs the same Cargo targets as `check` against the
candidate with a dedicated root guard and an isolated target directory; only a
current-revision, non-cancelled, identity-matched PASS/FAIL is fresh evidence.
`export` requires a current authorization epoch and the same hash check, returns
a revision-bound package with an honest `verified` flag, and `discard` removes
the scratch without following symlinks. Symlinks, special file types, or limit
overflows fail the capture closed as `INCOMPLETE_INPUTS`; a workspace with
relative path dependencies outside the captured tree also fails `create` closed
with `INCOMPLETE_INPUTS` and lists them, because the candidate copy cannot
reproduce their relative `path = "..."` references. A mid-apply I/O failure, a
crash between the applying marker and the final publish, or candidate bytes
that no longer match the recorded revision mark the change
`FAILED_INCONSISTENT` and refuse further stage/validate/export requests.

`validate`, and a fresh `inspect`/`export` evidence row, also return bounded
candidate compiler feedback: at most five, twelve, or twenty-four diagnostics
(for `compact`, `standard`, or `full` detail) carrying `code`, `level`,
candidate-relative `file`, `line`, and a truncated `message`, plus
`diagnosticsTotal`/`diagnosticsOmitted` counters and the parsed Cargo/test
`stats` (`testsExecuted`, `buildSuccess`). A fresh `FAIL` row additionally
carries a write-free `suggestionPackage` built from machine-applicable compiler
suggestions through the same helper as `check`, verified against the candidate
bytes and never written to any workspace; `skipped`, `unsupported`, `*Total`,
and `truncated` keep omissions visible. Only current-revision, non-cancelled,
byte-verified rows carry diagnostics or suggestions: a later `stage` supersedes
the earlier row and drops that feedback. Compiler text stays untrusted evidence
and the whole result stays inside `limits.tool_output_bytes` with visible
truncation.

## Repair Candidates

`repair` acts on the current-revision fresh `FAIL` evidence of an existing
`change`. `analyze` groups bounded diagnostics by code and file with primary and
secondary spans, labels root-cause links as reasoned hypotheses (never proven
facts), and explains ownership and trait obligations with source excerpts read
from the revision-bound candidate copy. Candidate sources are the
machine-applicable suggestion package (replayed as one atomic candidate because
the flattened package does not preserve per-suggestion grouping), verifiable
Rust Analyzer quick fixes, and a few explicit mechanical transforms
(clone-at-move, derive-for-local-type, known standard-library import). Every
candidate is a write-free `oldString`/`newString` package.

`try` recreates its own change for every candidate (`create`, replay of the
previous patches, then the candidate patches as one atomic stage) and validates
it through the same isolated Cargo path as `change`. A stale or overlapping part
rejects the whole candidate without partial application, identical candidate
hashes are never retried, and `maxCandidates`, `maxCompiles`, and `wallTimeMs`
stop with an explicit stop reason. `compare` adds measured compile status,
diagnostic delta, changed lines, public API delta, and the optional
`constraints.testTarget` gate result. A candidate that failed to compile, adds a
behavior/performance impact (test deletion or `#[ignore]`, assertion removal,
lint disabling, new `todo!`/`unimplemented!`/`panic!`/`unwrap`, unnecessary
clone, `unsafe`, error-swallowing conversion, block deletion, or public API
change), or fails the requested test gate is never selected; without a test gate
the comparison is `compileVerified` only. Pre-existing impacts are reported
separately from candidate-added impacts. Cancelled, timed-out, incomplete, or
cleanup-failed runs publish no usable repair evidence.

## Failure Minimization

`repair(action=minimize)` reduces a failing change revision to a small,
portable reproducer. It first builds a failure predicate from a real diagnostic
of the current-revision fresh `FAIL` evidence: the error code plus the
normalized message structure, including trait and type names. The error code
alone is never a predicate, and `failurePredicate` may only narrow the identity
(code, `messageContains`, `file`). The unchanged candidate snapshot must
reproduce the predicate on a first fresh Cargo run and again on a second
unchanged confirmation run; a run that does not match, times out, or exceeds
the compile budget before confirmation stops visibly as `NOT_REPRODUCED`
instead of being minimized.

The reduction search then tries permitted `reductionScope` axes (`files`,
`items`, `modules`, or all) on the revision-bound candidate copy: whole
unreferenced files, syntactically complete items and `use` items, `mod`
declarations with their module files, and inline `mod` blocks. Names that are
still referenced elsewhere and module paths that are still used (`name::`) are
not offered, so a reduction cannot silently delete code the failure depends on.
Each trial is a real Cargo run; a reduction is only accepted when the same
predicate is produced and the trial introduces no new error signature. An
irrelevant diagnostic with the same error code, a wrong syntax error, a newly
missing dependency, and a timeout are all rejected and recorded with their
failure match and reason. Accepted reductions are recorded with their selection
reason; `remainingRisks` states the unsearched scope.

The result is a proof package: minimum source with SHA-256 content hashes and
inline content, pinned toolchain file, edition, target, features, Cargo.toml
and Cargo.lock hashes, the reproduction command, and the verification evidence.
Before `REPRODUCED` is claimed the exact minimized source is materialized in a
clean temporary directory and compiled again; only a matching predicate with no
new signatures there sets `exportVerified: true`. A budget stop publishes
`BEST_KNOWN_REPRODUCER` with `verification: trialVerified` and the remaining
scope instead. No global minimality is claimed, no automatic upload or issue
creation happens, and the original workspace is never written. The export never
copies Cargo home, credentials, environment secrets, version-control data, or
unrelated repository files; only Rust sources, manifests, the lockfile,
toolchain pins, `.cargo` configuration, and provenance files are captured, and
anything left out is listed in `omitted`. Candidate items are also capped per
file, plan, snapshot byte total, and wall clock, and cancellation is forwarded
to every Cargo child.

## Context Capsules

`context` works from typed anchors only: `{kind:"file",file,range?}` and
`{kind:"symbol",symbol,file?,line?}`. There is no free-text or natural-language
interpretation. `prepare` selects a bounded capsule with definitions,
implementations, workspace consumers, related test candidates, hover
signatures, cargo-metadata dependency/feature evidence, and source excerpts.
Every item carries a selection reason and provenance, and unavailable, ambiguous,
or budget-omitted items stay visible in an `omitted` list.

`capsuleId` is a sha256 over the root epoch, toolchain/analyzer identity, source
hashes, typed anchor set, purpose, change label, feature selection, and byte
budget. A changed source therefore produces a new identity, and `expand` marks
items `stale` when the current file hash differs from the stored one; old symbol
handles never silently apply to a new revision. `expand` re-reads authorized
files, re-hashes them, and pages items with `cursor`/`pageSize`. `delta` returns
only added, changed, and removed items versus a stored previous capsule and
answers `NOT_FOUND` or `EXPIRED` instead of a fake empty delta.

The in-memory capsule store is bounded by `context.max_capsules` and
`context.capsule_ttl_ms`; a root-epoch change invalidates stored capsules.
Analyzer, workspace, and metadata text stays untrusted, provenance-tagged
evidence. Capsules are not exposed as MCP resources in this version; `expand`
pagination is the documented fallback. Sizes are exact UTF-8 byte and character
counts only; no tokenizer exists, and no token counts are reported.

## Result Semantics

Expected domain outcomes are successful MCP calls with typed status:

- compiler or test failure: `FAIL`;
- crate absence, mismatch, or registry outage: `NOT_FOUND`,
  `VERSION_MISMATCH`, or `UNAVAILABLE`;
- missing or ambiguous symbols: `NOT_FOUND` or `AMBIGUOUS`;
- documentation fallback exhaustion: typed unavailable data;
- capsule handles that are unknown or past TTL/root-epoch invalidation:
  `NOT_FOUND` or `EXPIRED`.

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
| `tools.profile` | `true` | Register `profile`. |
| `tools.audit` | `true` | Register `audit`. |
| `tools.crate_lookup` | `true` | Register `crate_lookup`. |
| `tools.docs` | `true` | Register `docs`. |
| `tools.context` | `true` | Register `context`. |
| `tools.explain` | `true` | Register `explain`. |
| `tools.lsp` | `true` | Register semantic navigation tools. |
| `tools.rename` | `true` | Register `rename` when LSP is enabled. |
| `tools.refactor` | `true` | Register `refactor` when LSP is enabled. |
| `tools.change` | `true` | Register `change`. |
| `tools.repair` | `true` | Register `repair` when `change` is enabled. |
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
| `profile.max_report_bytes` | `4194304` | Bounded read/store cap for one Cargo timing artifact. |
| `profile.max_runs` | `4` | Fresh Cargo runs available to one `profile` call. |
| `profile.compare_samples` | `3` | Required samples per side before any speed claim. |
| `rust_analyzer.path` | PATH or rustup | Optional binary override. |
| `rust_analyzer.timeout_ms` | `30000` | Semantic request deadline. |
| `rust_analyzer.idle_ms` | `900000` | Idle process lifetime. |
| `rust_analyzer.max_instances` | `2` | Concurrent workspace processes. |
| `rust_analyzer.check_hint` | `false` | Allow RA check hints. |
| `rust_analyzer.workspace_code` | `deny` | `deny` or explicit `allow`. |
| `docs.timeout_ms` | `300000` | Documentation resolution deadline. |
| `docs.fallback` | `auto` | `auto`, `local`, `network`, or `off`. |
| `docs.cache_dir` | platform `agz-rust-coder/docs` | Server-owned docs cache. |
| `change.scratch_dir` | platform `agz-rust-coder/state/change` | Server-owned changeset scratch outside authorized roots. |
| `change.max_active` | `4` | Concurrent active changes per server. |
| `change.max_files` | `20000` | Captured files per change. |
| `change.max_bytes` | `268435456` | Captured candidate bytes per change. |
| `change.ttl_ms` | `86400000` | Orphan and discarded scratch retention before the startup sweep. |
| `change.max_revisions` | `32` | Stage revisions per change. |
| `repair.max_candidates` | `4` | Candidate attempts per `repair` action; requests may only narrow. |
| `repair.max_compiles` | `4` | Cargo validations per `repair` action; requests may only narrow. |
| `repair.wall_time_ms` | `120000` | Wall-clock budget per `repair` action; requests may only narrow. |
| `repair.minimize_max_candidates` | `32` | Compile-evaluated reduction attempts per `repair(action=minimize)`; requests may only narrow. |
| `repair.minimize_max_compiles` | `16` | Cargo runs per `repair(action=minimize)`, including reproduction and export verification; requests may only narrow. |
| `context.max_capsules` | `32` | In-memory capsule ring capacity. |
| `context.capsule_ttl_ms` | `900000` | Capsule TTL; root-epoch changes also invalidate. |
| `context.max_items` | `64` | Items selected into one capsule. |
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
