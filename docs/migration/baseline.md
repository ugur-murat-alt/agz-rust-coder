# TypeScript reference baseline

This migration baseline is pinned to the read-only reference repository at
`/home/ugur/Projects/opencode-rust-coder`.

- Branch: `main`
- Commit: `0672c255dd7e7098dda54f04d9ac001cf164e199`
- Recorded: 2026-08-31
- Bun: `1.3.14`
- Command: `bun run typecheck && bun test && bun run build`
- Result: PASS
- Tests: 282 passed, 0 failed, 1,043 assertions across 33 files

No paid model benchmark was run. Public release, publish, or pull-request
operations are outside this baseline and require explicit user approval.

## Frozen contracts

- The reviewed TypeScript tool schema fixture is preserved semantically in
  `tests/reference/tool-schemas.json` (snapshot SHA-256
  `8ad100a4acddd04c20bb18ed55cfc6797056827549328082e381af4746c15d8d`).
  The source fixture byte checksum is
  `03223f990d0cd680d3b1427c4a8e390f8a578c11c1fe5abb70d014ccf4643d5c`.
- `tests/reference/source-tests.sha256` records the 33 behavior-owner test
  files and both legacy fixtures.
- `tests/reference/security-invariants.md` maps the source security fixtures to
  the Rust contract suites that replace them.
- Golden output behavior is frozen by the diagnostics, docs, gate, LSP, and
  write-free edit expectations named in the security invariant inventory.

The Rust protocol intentionally changes only the workspace `dir` field from
required to optional, with a default when exactly one root is authorized.
