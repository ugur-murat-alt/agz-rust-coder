# AGENTS.md - agz-rust-coder

## Delivery

- Use Rust 2024 with MSRV 1.88.0 and keep `#![forbid(unsafe_code)]`.
- Run formatting, workspace Clippy with `-D warnings`, all-target tests, MSRV
  check, release build, and all three `xtask` smoke commands before a release.
- Keep actions and release tooling pinned. Do not weaken artifact, checksum,
  tag, package, or registry identity checks.

## Product Boundaries

- Crate, library, executable, and MCP identities are `agz-rust-coder` and
  `agz_rust_coder`; configuration uses `AGZ_RUST_CODER_*`.
- Stdout belongs exclusively to MCP stdio framing. Diagnostics and logs go to
  stderr or bounded tool results.
- Cargo/rustc output is authoritative. Audit and Rust Analyzer output is
  advisory.
- `rename`, `refactor`, and compiler suggestions return edit packages but never
  write workspace source.
- Workspace and dependency roots fail closed on path escape or unverifiable
  symlink state. Client roots may narrow configured access but never widen it.
- Completed checks are not reused as authority for a later explicit request.
- External registry or documentation failures return typed bounded states.
