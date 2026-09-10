# agz-rust-mcp

_An AGZ Yazılım product._

`agz-rust-mcp` is a bounded, source-write-free Rust correctness MCP server.
Cargo and rustc remain the authority; Rust Analyzer and static audit results are
advisory.

- MCP Registry name: `mcp-name: io.github.ugur-murat-alt/agz-rust-mcp`
- Version: `0.3.0`
- MSRV: Rust `1.88.0`
- Transport: stdio

```bash
cargo install agz-rust-mcp --locked
agz-rust-mcp --version
```

The server exposes `check`, `profile`, `audit`, `crate_lookup`, `docs`,
`context`, `api`, `explain`, `verify`, `symbol`, `references`, `definition`,
`symbols`, `implementations`, `hierarchy`, `rename`, `refactor`, and `change`.
Edit tools return bounded packages and never modify source. The `context` tool
builds revision-bound capsules with per-item reasons, bounded pagination, and
stale/delta tracking. The `api` tool resolves signatures from bounded analyzer
evidence and type-checks candidate snippets only in a discarded candidate copy.
The `change` tool keeps a revision-bound candidate copy with validation evidence
and never writes workspace source.

See the [project repository](https://github.com/ugur-murat-alt/agz-rust-mcp)
for client configuration, security boundaries, and release artifacts.

[MIT](LICENSE)
