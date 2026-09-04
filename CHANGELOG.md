# Changelog

All notable changes to `agz-rust-coder` are documented here. The project uses
Semantic Versioning and follows the Keep a Changelog structure.

## [Unreleased]

## [0.1.1] - 2026-09-04

### Security

- Bound authorized Cargo, Git, documentation, and Rust Analyzer subprocesses to
  the exact directory capabilities captured during workspace selection, with
  process-tree cleanup for cancelled schema probes.

### Fixed

- Made metadata ownership, followers, Cargo execution, cancellation, and
  deadlines bounded without publishing late or request-local results to the
  shared cache.
- Made crates.io lookup cancellation-aware while preserving bounded response
  streaming, fixed-host redirects, and immediate admission-permit release.

## [0.1.0] - 2026-09-01

### Added

- Initial public Rust release with 12 bounded MCP tools for Cargo validation,
  static auditing, crate verification, exact-version documentation, semantic
  navigation, and write-free edit packages.
- MCP protocol support for `2025-11-25` and `2026-07-28`, including client
  roots, tasks, progress, and cancellation.
- Cross-platform child-process supervision, bounded caches and telemetry, and
  provider-free protocol, OpenCode, and benchmark smoke suites.
- Cargo distribution through crates.io and discovery metadata for the official
  MCP Registry.

[Unreleased]: https://github.com/ugur-murat-alt/agz-rust-coder/compare/agz-rust-coder-v0.1.1...HEAD
[0.1.1]: https://github.com/ugur-murat-alt/agz-rust-coder/compare/agz-rust-coder-v0.1.0...agz-rust-coder-v0.1.1
[0.1.0]: https://github.com/ugur-murat-alt/agz-rust-coder/releases/tag/agz-rust-coder-v0.1.0
