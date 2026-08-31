# Release readiness observations

Recorded on 2026-08-31 without mutating any public service:

- `https://crates.io/api/v1/crates/mcp-rust-coder` returned HTTP 404. The crate
  name appeared unclaimed at observation time and must be checked again before
  publishing.
- `https://api.github.com/repos/ugur-murat-alt/mcp-rust-coder` returned HTTP
  404. No GitHub repository was created.
- No tag, GitHub release, crates.io publish, paid benchmark, pull request, or
  remote push was performed.

The first Rust release tag is reserved as `mcp-rust-coder-v0.1.0`. Publication
must verify the crate name again, use a clean reviewed commit, and bind the
packed `.cargo_vcs_info.json`, peeled tag SHA, checksums, binary `--version`,
and MCP `serverInfo.version` to that same commit and crate version.
