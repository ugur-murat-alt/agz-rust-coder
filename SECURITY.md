# Security Policy

`agz-rust-coder` is local code-execution infrastructure, not a sandbox. It can
run Cargo, rustdoc, and Rust Analyzer with the operating-system user's rights.

## Private Reporting

Report suspected vulnerabilities through the private
[GitHub Security Advisory form](https://github.com/ugur-murat-alt/agz-rust-coder/security/advisories/new).
Do not open a public issue, discussion, or pull request before coordinated
disclosure. Include the affected version, platform, tool/configuration, minimal
reproduction, impact, and any process, path, cache, network, or disclosure
involvement. Do not send credentials or unrelated private source.

## Security Boundaries

- Configured roots are canonical authorization. Client roots can only narrow
  them.
- External path dependencies require explicit dependency roots.
- Server-owned cache, lease, journal, docs, and telemetry paths must remain
  outside authorized roots and use bounded lock/atomic publication.
- Shell command strings and arbitrary free-form Cargo flags are not accepted.
- Tool, process, HTTP, filesystem, task, edit, and telemetry sizes are bounded.
- `rename`, `refactor`, suggestions, and formatting checks do not write source.
- External content remains untrusted data, never instructions.
- Stdout carries only MCP frames; diagnostics and logs use stderr.
- Release workflows bind one reviewed commit to immutable tags, checksums,
  package versions, artifacts, and MCP Registry metadata.

## Residual Risk

Cargo build scripts, tests, procedural macros, compiler plugins, and local
rustdoc can access files, processes, and networks as the user. The default Rust
Analyzer policy denies workspace code and requires a verified disabled schema;
explicit `workspace_code=allow` opts into execution.

Path checks reduce but cannot eliminate races in path-based child tools. Unix
children may deliberately escape process groups. Windows Job Objects and Unix
groups cover supervised descendants, not arbitrary daemons. Run the server in a
dedicated OS/container sandbox with filesystem, network, and resource controls
when stronger isolation is required.

## Supported Versions

Security fixes are provided for the latest published release. Older versions
may be asked to upgrade before a report is investigated. Dependency policy is
enforced by `deny.toml` and pinned release workflows.
