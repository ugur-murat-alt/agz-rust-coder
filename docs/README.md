# AGZ Rust MCP Documentation

English | [Türkçe](README.tr.md)

**An AGZ Yazılım product.** `agz-rust-mcp` is a standalone stdio MCP server for
bounded Rust correctness grounded in Cargo and rustc.

This index defines the reading path. Follow it in order the first time; each
step links to its Turkish counterpart and to the previous and next documents.

## Reading Path

| Step | Document | What it covers |
| --- | --- | --- |
| 1 | [Install and client setup](install.md) | Every install method, per-OS notes, OpenCode2 and Codex configuration, verification, and troubleshooting. |
| 2 | [Tools and configuration](tools.md) | Tool catalog, actions, result semantics, and the complete configuration reference. |
| 3 | [Architecture](architecture.md) | Process model, data flow, protocol lifecycle, authority model, and residual risk. |
| 4 | [Validation and benchmark protocol](benchmark.md) | Provider-free smokes, task benchmark gates, evidence layout, and the release gate. |
| 5 | [Security policy](../SECURITY.md) and [Contributing](../CONTRIBUTING.md) | Security boundary and private reporting; change rules and validation commands. |

## Reference And Background

- [CHANGELOG.md](../CHANGELOG.md) - release history.
- [CODE_OF_CONDUCT.md](../CODE_OF_CONDUCT.md) - project conduct policy.
- [Rust correctness and efficiency plan](rust-efficiency-plan.md) - six-part
  correctness/efficiency work with [verification evidence](rust-efficiency-evidence.md).
- [ADR 0001: source apply capability](adr/0001-source-apply-capability.md) -
  why the server returns edit packages instead of applying them.
- [Root README](../README.md) - product summary and machine-readable tool and
  configuration tables.

Every English document in the path has a paired Turkish file: install,
tool, architecture, and benchmark guides link to each other directly.
