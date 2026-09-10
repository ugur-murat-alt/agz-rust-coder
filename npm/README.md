# @agz-yazilim/agz-rust-mcp

Standalone stdio [MCP](https://modelcontextprotocol.io) server for **Rust
correctness**, by **AGZ Yazılım**. It gives MCP clients compiler-backed tools
for Rust work: bounded Cargo `check`/Clippy/tests/docs, static audit, crate and
docs lookup, and Rust Analyzer-backed symbols, references, definitions,
hierarchies, and write-free rename/refactor edit packages. Cargo/rustc output
is authoritative; analyzer and audit output is advisory.

The npm package is a thin, zero-dependency Node launcher. It runs the pinned,
checksum-verified release binary; it does not bundle or modify it.

> Published as `@agz-yazilim/agz-rust-mcp`. If the `@agz-yazilim` npm scope is
> unavailable at publish time, the same wrapper is published unscoped as
> `agz-rust-mcp`; substitute that name in the commands below.

## Quick start

```sh
npx -y @agz-yazilim/agz-rust-mcp
```

The first run resolves the server binary, verifies it, and caches it under
`~/.cache/agz-rust-mcp/npm/<version>-<platform>/`. Later runs reuse the cache.
Print the wrapper version without downloading anything:

```sh
npx -y @agz-yazilim/agz-rust-mcp --agz-npm-version
# or
AGZ_RUST_MCP_NPM_VERSION=1 npx -y @agz-yazilim/agz-rust-mcp
```

## OpenCode2

`opencode.jsonc`:

```jsonc
{
  "$schema": "https://opencode.ai/config.json",
  "mcp": {
    "servers": {
      "rust": {
        "type": "local",
        "command": ["npx", "-y", "@agz-yazilim/agz-rust-mcp"],
        "cwd": ".",
        "codemode": false,
        "timeout": {
          "startup": 30000,
          "catalog": 30000,
          "execution": 720000
        }
      }
    }
  }
}
```

## Codex

`~/.codex/config.toml`:

```toml
[mcp_servers.agz-rust-mcp]
command = "npx"
args = ["-y", "@agz-yazilim/agz-rust-mcp"]
```

To skip `npx` startup on every launch, install the wrapper once and call the
installed command directly:

```sh
npm install --global @agz-yazilim/agz-rust-mcp
```

```toml
[mcp_servers.agz-rust-mcp]
command = "agz-rust-mcp"
```

## Binary resolution and integrity

Resolution order:

1. `AGZ_RUST_MCP_BIN` — absolute or relative path to an existing server binary.
   If set but missing, the launcher fails closed instead of downloading.
2. `agz-rust-mcp` on `PATH` (this launcher's own path is skipped).
3. Pinned GitHub release download from `ugur-murat-alt/agz-rust-mcp`.

Download safety: HTTPS only (non-HTTPS redirects are refused), each download is
bounded to 64 MiB, the `.sha256` asset is checked before extraction, the archive
is extracted with the system `tar` using an argv array (no shell), the extracted
binary must be a regular file and is set to `0755`, its `--version` must report
the wrapper version, and it is atomically renamed into the cache. A single-flight
lock prevents concurrent launches from double-downloading.

Release mapping: version `0.2.x` maps to the already published
`agz-rust-coder-v0.2.0` release (`agz-rust-coder-<platform>.tar.gz`); version
`0.3.0` and later use `agz-rust-mcp-v<version>` and
`agz-rust-mcp-<platform>.tar.gz`. Platform keys are `linux-x86_64`,
`macos-arm64`, and `windows-x86_64`.

| Variable | Purpose |
| --- | --- |
| `AGZ_RUST_MCP_BIN` | Use an existing server binary instead of downloading. |
| `AGZ_RUST_MCP_NPM_VERSION` | Truthy value prints the wrapper version and exits without downloading. |

## Fallback installation

If the wrapper cannot run on your platform (other architectures, offline hosts,
or restricted CI), install the server directly:

- Release installer: `./install.sh` from
  <https://github.com/ugur-murat-alt/agz-rust-mcp>
- crates.io: `cargo install agz-rust-mcp`
- Manual release archive: download `agz-rust-mcp-<platform>.tar.gz` and its
  `.sha256` from
  <https://github.com/ugur-murat-alt/agz-rust-mcp/releases>
- From source: `cargo build --release -p agz-rust-mcp --locked`

## License

MIT. Repository and documentation:
<https://github.com/ugur-murat-alt/agz-rust-mcp>. AGZ Yazılım product.
