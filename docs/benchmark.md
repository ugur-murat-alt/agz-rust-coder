# Validation And Benchmark Protocol

Release claims use deterministic local gates first. Live model benchmarks are
optional measurements and never replace compiler, protocol, package, or
security checks.

## Provider-Free Smokes

```bash
cargo run -p xtask -- protocol-smoke
cargo run -p xtask -- opencode-smoke
cargo run -p xtask -- benchmark-smoke
cargo run -p xtask -- task-benchmark-smoke
```

`protocol-smoke` starts the real stdio binary and checks initialization, tool
discovery, structured/text parity, `2026-07-28` task creation, progress,
cancellation, terminal state, synchronous fallback, and fixture cleanup.

`opencode-smoke` runs the pinned OpenCode host against direct and grouped local
MCP configurations. A loopback fake provider returns deterministic tool calls;
no paid or external model endpoint is used.

`benchmark-smoke` runs frozen clean and broken Rust fixtures against an oracle.
It verifies that status and `passed` fields agree and compares current behavior
to the preserved benchmark contract.

`task-benchmark-smoke` extends that benchmark harness with the frozen
`rust-agent-tasks-v1` corpus. It replays eight Rust task classes over three
paired and position-balanced repetitions with three distinct order seeds:
A) ordinary shell/file work, B) the preserved `0.2.0` MCP surface, and C) the
Rust Change Engine contract. The task request contains only public task data;
independent oracle observations and mutation guards decide success. The replay
retains failed, timeout, and cancelled trials instead of filtering them.

The provider-free replay is a harness fixture, not measured model performance.
Its timing, Cargo-call, recompile, and host-turn numbers exist to exercise
aggregation and gate logic. Input/output/cache/schema token counts and cost are
recorded as `unknown`, never as zero. Real provider/model measurements remain
explicit opt-in work.

The frozen v1 corpus covers missing trait implementation, borrow/move repair,
exact crate API use, multi-crate signature migration, feature-only breakage,
regression-test addition, dependency upgrade, and behavior-preserving
performance work. `xtask/tests/task_benchmark.rs`, which is part of the normal
workspace CI test command, verifies corpus hashes, replay completeness,
independent scoring, unknown usage semantics, negative controls, and balanced
ordering. CI also invokes `task-benchmark-smoke` directly on the platform matrix
so transcript replay and evidence publication are exercised as a product path.

## Predeclared Task Benchmark Gates

Before Change Engine implementation, the corpus fixes these comparison rules:

- quality: C may not fall below B; the `non_inferiority_margin` is 0 percentage
  points for v1;
- efficiency: C targets at least 20% fewer host turns and 15% fewer Cargo calls
  than B;
- wall time: C may regress by at most 10% versus B.

Every arm reports sample count, success rate with a Wilson 95% interval, wall
time dispersion, CPU time, snapshot preparation, Cargo/recompile counts,
host turns, cold/warm cache strata, token fields, cost, timeout and
cancellation outcomes. Provider-free replay can prove the harness evaluates
these gates, but it can never make Change Engine default-on; comparable opt-in
live runs are required for that decision.

Raw replay observations live in
`benchmark/task-corpus/provider-free-replay.json`. The frozen task manifest,
embedded `fixtures.json` workspace snapshots, and independent oracle catalog
live beside it. Fixture and settings hashes bind results to the exact corpus.
The recorded provenance includes provider, model, harness version, MCP SHA,
fixture hash, toolchain, OS/hardware, cache state, and settings hash.

## Evidence Layout

Each run is atomically published under `benchmark/results/stage7/` with:

- `run.json`: `run_id`, mode, fixture, protocol and adapter metadata;
- `results.json`: observations and pass/fail assertions;
- `report.md`: human-readable bounded summary;
- `provenance.json`: `source_commit`, `source_checksum`, dirty state, and command
  identity.

No report contains prompts, absolute workspace paths, session IDs, credentials,
or private source. Concurrent publishers use a lock and unique final directory.

## Live Mode

Live mode requires an explicitly reviewed adapter and an explicit operator
decision because it may incur cost:

```bash
AGZ_RUST_MCP_LIVE_ADAPTER=/absolute/path/to/reviewed-adapter \
  cargo run -p xtask -- benchmark-smoke --live
```

The manifest records `provider` / `model` / `variant`, repetitions, fixtures,
cost when available, and the `non_inferiority_margin`. Adapter output is rejected
if its boolean pass field contradicts its typed status. Results from different
source or adapter checksums are not pooled.

## Release Gate

The minimum local release gate is:

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
cargo test --workspace --all-targets --all-features --locked --no-fail-fast
cargo +1.88.0 check --workspace --all-targets --all-features --locked
cargo build --release --locked
cargo package -p agz-rust-mcp --locked
cargo publish -p agz-rust-mcp --dry-run --locked
```

The provider-free smokes, real pinned Rust Analyzer/doc adapters,
`cargo deny check`, workflow lint, and secret/vulnerability scans complete the
release evidence. Platform CI supplies macOS and Windows process and path
coverage unavailable on a Linux workstation.

## Input-identity comparisons (unreleased)

Build `crates/agz-rust-mcp/examples/identity_measure.rs` against the baseline and
candidate using the same toolchain/profile. Keep both binaries, then run
`python3 benchmark/identity_compare.py BASELINE CANDIDATE --output comparison.json`.
The script generates identical fixtures, alternates measurement order, warms
each case three times and records 15 samples by default. Source edits and manifest
edits are separate scenarios. Unequal input hashes invalidate a comparison.
Run `python3 -m unittest discover -s benchmark -p test_identity_compare.py` to test
comparison rejection. No LLM, network or Cargo execution is involved in the
measured stage. See [recorded evidence](rust-efficiency-evidence.md).
