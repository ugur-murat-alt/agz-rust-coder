# agz-rust-coder

`agz-rust-coder` is a bounded, source-write-free Rust correctness MCP server.
Cargo and rustc remain the authority; Rust Analyzer and static audit results are
advisory.

- MCP Registry name: `mcp-name: io.github.ugur-murat-alt/agz-rust-coder`
- Version: `0.1.1`
- MSRV: Rust `1.88.0`
- Transport: stdio

```bash
cargo install agz-rust-coder --locked
agz-rust-coder --version
```

The server exposes `check`, `audit`, `crate_lookup`, `docs`, `symbol`,
`references`, `definition`, `symbols`, `implementations`, `hierarchy`, `rename`,
and `refactor`. Edit tools return bounded packages and never modify source.

See the [project repository](https://github.com/ugur-murat-alt/agz-rust-coder)
for client configuration, security boundaries, and release artifacts.

[MIT](LICENSE)
