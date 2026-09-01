# Validation And Benchmark Protocol

Release claims use deterministic local gates first. Live model benchmarks are
optional measurements and never replace compiler, protocol, package, or
security checks.

## Provider-Free Smokes

```bash
cargo run -p xtask -- protocol-smoke
cargo run -p xtask -- opencode-smoke
cargo run -p xtask -- benchmark-smoke
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
AGZ_RUST_CODER_LIVE_ADAPTER=/absolute/path/to/reviewed-adapter \
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
cargo package -p agz-rust-coder --locked
cargo publish -p agz-rust-coder --dry-run --locked
```

The three provider-free smokes, real pinned Rust Analyzer/doc adapters,
`cargo deny check`, workflow lint, and secret/vulnerability scans complete the
release evidence. Platform CI supplies macOS and Windows process and path
coverage unavailable on a Linux workstation.
