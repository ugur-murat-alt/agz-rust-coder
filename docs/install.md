# Install And Client Setup

**An AGZ Yazılım product.** This is step 1 of the
[documentation reading path](README.md). It covers every install method,
per-OS notes, MCP client configuration, verification, and troubleshooting.
The next step is the [tool and configuration reference](tools.md).

`agz-rust-mcp` is a standalone stdio MCP server: one executable, launched by
your MCP client, speaking the Model Context Protocol on stdin/stdout. Every
method below installs the same `agz-rust-mcp` executable.

## Bundled skills (current source)

Version `0.4.0` bundles four versioned `SKILL.md` files inside the same
executable. They work offline and need no separate skill package or MCP server.
`prompts/list` discovers `workflow`, `repair`, `refactor`, and `performance`;
`prompts/get` accepts an optional `task` string (bounded to 4,000 sanitized
characters). `resources/list` exposes the same content under
`agz-rust-mcp://skills/<skill-name>`. The legacy workflow resource remains an alias.
Clients decide whether to discover/invoke prompts or filesystem skills; MCP
resource availability alone does not enable automatic skill selection.

For clients that discover filesystem skills, use a source-built binary:

```bash
agz-rust-mcp skills list
agz-rust-mcp skills show agz-rust-workflow
# Existing trusted parent; destination must NOT exist:
agz-rust-mcp skills export --dir /path/to/new-skills
```

The destination contains `agz-rust-workflow`, `agz-rust-repair`,
`agz-rust-refactor`, and `agz-rust-performance`, each with one `SKILL.md`.
Choose the client's supported skill directory as the new destination, or copy
the selected skill folders into its existing directory after review. Export
refuses existing destinations, never replaces custom skills, and reports any
partial output on failure. These CLI commands do not start MCP/Cargo, read server
configuration or change client settings. With no subcommand the binary remains
the normal stdio server. Skills use the connected server's discovered tools;
Rust/Cargo and optional adapters remain local executable prerequisites.

## Requirements

- Linux, macOS, or Windows on x86_64, or macOS on arm64.
- An MCP-capable client (OpenCode2, Codex, or another stdio client).
- Rust `1.88.0` or newer only for the `cargo install` and source methods.
- Node.js only for the npm wrapper.

## 1. npm Wrapper

```bash
npx -y @agz-yazilim/agz-rust-mcp --version
```

The wrapper is the managed option: it resolves and caches the release artifact
matching your platform and forwards stdio to it, so the server itself is not
built on your machine. Pin an exact wrapper version in client configuration
instead of relying on the floating tag, so upgrades stay explicit. Requires
Node.js and network access on first use.

## 2. Installer Script (`install.sh`)

The installer downloads a release archive, verifies its published SHA-256
checksum before extraction, refuses symlink destinations, verifies the staged
binary, and installs atomically. It currently supports **Linux x86_64 only**;
other platforms get a clear error and should use method 3, 4, or 5.

Download the installer and checksum manifest from the release page, verify the
script itself, then run it:

```bash
curl -fsSL -O \
  https://github.com/ugur-murat-alt/agz-rust-mcp/releases/latest/download/install.sh
curl -fsSL -O \
  https://github.com/ugur-murat-alt/agz-rust-mcp/releases/latest/download/SHA256SUMS
grep ' install.sh$' SHA256SUMS | sha256sum --check
# macOS: grep ' install.sh$' SHA256SUMS | shasum -a 256 -c -

bash install.sh
```

Do not run the installer if the script checksum fails; re-download from the
release page instead. Environment overrides:

| Variable | Default | Meaning |
| --- | --- | --- |
| `AGZ_RUST_MCP_VERSION` | latest release | Exact version to install. |
| `AGZ_RUST_MCP_INSTALL_DIR` | `$HOME/.local/bin` | Absolute install directory. |

```bash
AGZ_RUST_MCP_VERSION=0.4.0 AGZ_RUST_MCP_INSTALL_DIR="$HOME/.local/bin" bash install.sh
```

The installer also verifies the archive layout and the extracted binary's
`--version` before replacing an existing file. A `SHA256SUMS` file covers the
crate, source bundle, and installer; each archive ships its own `.tar.gz.sha256`.

## 3. Prebuilt Archives

Release pages provide `.tar.gz` archives with a matching `.sha256` file:

| Platform | Archive (0.3.0 and later) |
| --- | --- |
| Linux x86_64 | `agz-rust-mcp-linux-x86_64.tar.gz` |
| macOS arm64 | `agz-rust-mcp-macos-arm64.tar.gz` |
| Windows x86_64 | `agz-rust-mcp-windows-x86_64.tar.gz` |

**Legacy 0.1.0-0.2.0 assets.** Releases up to and including `0.2.0` were
published under the former product name and use the old asset and executable
names. The files remain available and `install.sh` maps them automatically:

| Release line | Archive name | Archived executable |
| --- | --- | --- |
| `0.1.0`-`0.2.0` | `agz-rust-coder-<platform>-<arch>.tar.gz` | `agz-rust-coder` |
| `0.3.0` and later | `agz-rust-mcp-<platform>-<arch>.tar.gz` | `agz-rust-mcp` |

Whatever the archive name, the installed executable is always `agz-rust-mcp`;
`install.sh` renames a legacy archive executable during installation.

Linux and macOS:

```bash
sha256sum --check agz-rust-mcp-macos-arm64.tar.gz.sha256   # Linux
shasum -a 256 -c agz-rust-mcp-macos-arm64.tar.gz.sha256    # macOS
tar -xzf agz-rust-mcp-macos-arm64.tar.gz
install -m 0755 agz-rust-mcp "$HOME/.local/bin/agz-rust-mcp"
```

Windows PowerShell:

```powershell
Get-FileHash .\agz-rust-mcp-windows-x86_64.tar.gz -Algorithm SHA256
tar -xzf .\agz-rust-mcp-windows-x86_64.tar.gz
# Compare Get-FileHash output with the .sha256 file, then add the folder to PATH.
```

Compare the printed hash with the `.sha256` file before extracting. Never run
an archive whose checksum does not match.

## 4. crates.io (`cargo install`)

Requires Rust `1.88.0` or newer:

```bash
rustup toolchain install 1.88.0
cargo install agz-rust-mcp --locked
agz-rust-mcp --version
```

`--locked` builds with the exact dependency versions recorded in `Cargo.lock`.
Pin a release with `cargo install agz-rust-mcp --version 0.4.0 --locked`.
Cargo installs to `$HOME/.cargo/bin` (`%USERPROFILE%\.cargo\bin` on Windows);
ensure that directory is on `PATH`.

## 5. From Source

```bash
git clone https://github.com/ugur-murat-alt/agz-rust-mcp
cd agz-rust-mcp
cargo +1.88.0 build --release -p agz-rust-mcp --locked
./target/release/agz-rust-mcp --version
```

MSRV is Rust `1.88.0` with edition 2024; `rust-toolchain.toml` pins the toolchain
used by CI. Copy `target/release/agz-rust-mcp` (`.exe` on Windows) to a directory
on `PATH`, or point the client at its absolute path. See
[CONTRIBUTING.md](../CONTRIBUTING.md) for the full development gate.

## Per-OS Notes

- **Linux.** `install.sh` is the shortest verified path (x86_64 only). Add
  `$HOME/.local/bin` to `PATH` when the installer reports it is missing.
- **macOS.** Use the `macos-arm64` archive or `cargo install`. Intel Macs have
  no prebuilt archive in this release; build from source. If the OS blocks the
  unsigned binary, allow it under System Settings -> Privacy & Security.
- **Windows.** Use the `windows-x86_64` archive or `cargo install`; `install.sh`
  does not support Windows. Add the extraction directory to `PATH`.

## MCP Client Setup

### Multiple checkouts, worktrees and package subdirectories

The server is not tied to one Git branch. Supply an absolute `dir` for the
repository root, package/source subdirectory or linked worktree in each call.
Configure their common project parent once using `--allow-root`, plus any other
worktree location (for example a client's dedicated worktrees directory).
Shared path dependencies outside the selected worktree need an explicit
`--allow-dependency-root`. Example server arguments, with your own paths:

```text
--allow-root /projects --allow-root /client-worktrees --allow-dependency-root /projects
```

These are explicit local trust boundaries; MCP client roots may narrow them.
The current source discovers inherited workspace dependencies from member/source
directories and binds external dependency inputs to the selected worktree, not
the broad configured parent. Different checkouts retain separate cache keys.
In auto cache mode an unsafe/external project target falls back to an isolated
target; project-only mode still requires a safe in-workspace target. A missing
directory or inaccessible dependency produces a bounded reason, not a pass.

`agz-rust-mcp` is a standalone stdio server; any MCP-capable client can run it.
The canonical current directory is the default authorized root. Add explicit
roots with repeated `--allow-root <path>` arguments when the client starts
elsewhere. Client-provided MCP roots may narrow configured access but never
widen it.

### OpenCode2 (`opencode.jsonc`)

```jsonc
{
  "$schema": "https://opencode.ai/config.json",
  "mcp": {
    "servers": {
      "rust": {
        "type": "local",
        "command": ["agz-rust-mcp"],
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

Wrapper-managed variant:

```jsonc
"command": ["npx", "-y", "@agz-yazilim/agz-rust-mcp"],
```

### Codex (`~/.codex/config.toml`)

```toml
[mcp_servers.rust]
command = "agz-rust-mcp"
args = []
```

Wrapper-managed variant:

```toml
[mcp_servers.rust]
command = "npx"
args = ["-y", "@agz-yazilim/agz-rust-mcp"]
```

Managed wrapper note: with the npm wrapper the client config never changes when
the underlying binary is updated; pin the wrapper version for reproducibility.
With a direct binary, use an absolute path if the client's `PATH` differs from
your shell's. Restart or reload the client after editing its configuration.

Configuration precedence is CLI flags, then `AGZ_RUST_MCP_*` environment
variables, then `--config` TOML, then defaults; the complete key reference is in
[docs/tools.md](tools.md#configuration-reference).

## Verification

1. Check the executable:

   ```bash
   agz-rust-mcp --version
   agz-rust-mcp --help
   ```

2. Check the MCP handshake and tool catalog over stdio (newline-delimited
   JSON-RPC):

   ```bash
   {
     printf '%s\n' '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"verify","version":"1.0.0"}}}'
     printf '%s\n' '{"jsonrpc":"2.0","method":"notifications/initialized"}'
     printf '%s\n' '{"jsonrpc":"2.0","id":2,"method":"tools/list"}'
   } | agz-rust-mcp
   ```

   Expect an `initialize` result naming the negotiated protocol and a
   `tools/list` result containing the `check`, `profile`, `audit`,
   `crate_lookup`, `docs`, `context`, `api`, `explain`, `verify`, `symbol`,
   `references`, `definition`, `symbols`, `implementations`, `hierarchy`,
   `rename`, `refactor`, `change`, `repair`, and `work` tools (tools disabled by
   configuration are omitted). Stdout is MCP framing only; diagnostics go to
   stderr.

3. From a source checkout, run the repository protocol smoke:

   ```bash
   cargo run -p xtask -- protocol-smoke
   ```

## Troubleshooting

| Symptom | Check and fix |
| --- | --- |
| `agz-rust-mcp: command not found` | Add the install directory (`$HOME/.local/bin` or `$HOME/.cargo/bin`) to `PATH`, or use an absolute path in the client config. |
| Checksum mismatch | Re-download the archive and its `.sha256` from the release page; do not bypass verification. |
| Installer rejects the OS or architecture | `install.sh` supports Linux x86_64 only; use a prebuilt archive, `cargo install`, or a source build. |
| `npx` starts an unexpected version | Pin the wrapper version in the client config and clear stale npm cache entries. |
| Client shows no tools | Verify the client's `PATH`, use an absolute binary path, and allow enough startup timeout. |
| Path or root authorization errors | Start the client in the workspace or pass repeated `--allow-root <path>` flags. |
| Semantic tools return unavailable | Install the pinned Rust Analyzer (`rustup component add rust-analyzer --toolchain 1.88.0`) and review the workspace-code policy in [docs/tools.md](tools.md#rust-analyzer-policy). |
| `cargo install` fails on an old toolchain | Install Rust `1.88.0` or newer with `rustup toolchain install 1.88.0`. |

Related: [documentation index](README.md) - [tool reference](tools.md) -
[architecture](architecture.md) - [security policy](../SECURITY.md).
