# AGENTS.md - mcp-rust-coder

## Workflow

- Rust edition 2024, MSRV 1.88.0, `#![forbid(unsafe_code)]`.
- Focused checks must be followed by the delivery gate:
  `cargo fmt --all --check`,
  `cargo clippy --workspace --all-targets --all-features --locked -- -D warnings`,
  `cargo test --workspace --all-features --locked`,
  `cargo +1.88.0 check --workspace --all-targets --all-features --locked`, and
  `cargo build --release --locked`.
- After `xtask` exists, also run `protocol-smoke`, `opencode-smoke`, and
  `benchmark-smoke` before release claims.
- Do not run live/model benchmarks, publish, create a release, push, or open a
  pull request without explicit user approval.

## Invariants

- The executable, crate, and MCP server name is `mcp-rust-coder`.
- The stdio transport owns stdout. Logs, panic diagnostics, and child output go
  to stderr or bounded typed results.
- Cargo/rustc output is authoritative. Rust Analyzer and static audit output is
  advisory.
- `rename`, `refactor`, and compiler suggestions return write-free edit
  packages and never modify workspace source files.
- Workspace, dependency, cache, process, and LSP paths fail closed on escape or
  unverifiable symlink state.
- External crates.io/docs.rs failures return bounded typed unavailable states;
  they do not crash the server.
- Completed validation PASS results are never reused as authority by a later
  explicit `check` call. Only an active identical job may be joined.
- Every tool result keeps deterministic structured JSON and equivalent text
  fallback within the configured wire-byte limit.

## Reference

The previous TypeScript implementation at
`/home/ugur/Projects/opencode-rust-coder` is read-only migration evidence. Its
checksums and invariant mapping are under `tests/reference/`. Never edit,
format, or delete files in that repository from this project.
