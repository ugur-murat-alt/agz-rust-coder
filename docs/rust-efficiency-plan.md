# Rust correctness and efficiency — single-PR delivery

Baseline: `feb7f1b5fe9022ea09870bc375cded075a27439e` (0.1.1).
Requested outcome: fewer incorrect Rust fixes and shorter evidence-driven
iteration, without weakening Cargo authority or workspace authorization.

## 1. Streaming, complete compiler evidence

Decode bounded Cargo JSON while stdout arrives. Preserve early diagnostics
independently of the display tail; measure first useful evidence and distinguish
log truncation, omitted diagnostics, malformed input and partial telemetry.
Early evidence is never a completed pass. Test chunk boundaries, long lines,
large output, cancellation, deduplication and real compiler failures.

## 2. Remove avoidable identity work

Replace repeated whole-set recounting with maintained counters while retaining
identical file budgets, deduplication and root boundaries. Measure no-change,
single-file and manifest changes separately; do not substitute metadata-only
freshness or reuse completed checks as later authority.

## 3. Explicit, scoped validation profiles

Extend existing scope selection to relevant development checks, retaining broad
final validation. Represent feature selection, target and test filters as typed,
validated arguments; record the actual scope and configuration in evidence.
Widen on global/build-script/proc-macro/unknown changes. Never describe one
configuration as every feature combination or every platform.

## 4. Diagnostic-centered code context

Assemble bounded compiler evidence, authorized source ranges, package/target
identity and resolved dependency versions without introducing another LLM.
Distinguish verified source, advisory navigation and documentation follow-ups.
Missing or stale context must remain explicit; edit packages never write source.

## 5. Independent correctness and performance evidence

Add property-based boundary tests and a small concurrency model for a real
production invariant. Extend the existing benchmark workflow with comparable
measurements and machine-readable before/after evidence. Reject incomparable
runs and avoid unsupported speed, token or model-quality claims.

## 6. Opt-in nextest and sccache

Add explicit, bounded optional acceleration with unchanged Cargo defaults.
Keep doctests separate; distinguish unavailable tooling from test failure.
Never silently disable incremental compilation or replace a user compiler
wrapper. Exercise real optional tooling where the runner supports it and retain
hermetic regressions for unavailable/conflicting configurations.

## Delivery and verification

One PR; no release or merge is implied by this task. Keep Rust 1.88, unsafe-code
prohibition, source-write-free tools, cancellation, root and checksum protections.
Validate fmt, strict Clippy, all workspace targets/features, MSRV, release,
protocol/OpenCode/benchmark smoke and real Rust Analyzer. Store exact commands,
commit identities, measured samples and limitations in a companion evidence
report. Temporary bootstrap tooling is removed from the final tree.

Status: implementation and baseline validation in progress.
