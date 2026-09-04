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
- Kept Git identity probes within the check request's deadline and cancellation
  lifecycle, with bounded raw output and supervised process-tree cleanup.
- Honored the earlier of the per-process timeout and the request deadline.
- Moved post-validation identity work off the async executor and avoided extra
  Git probes after failed or cancelled Cargo runs.
- Preserved result status and trust markers at the minimum output budget and
  removed complete terminal escape sequences from structured tool results.
- Fixed Linux installation in paths containing spaces, rejected failed archive
  listings, and stopped interrupted installs without replacing the old binary.

### Changed

- Updated the SHA-pinned checkout, upload-artifact, and download-artifact
  actions to 7.0.1, 7.0.1, and 8.0.1 respectively, retaining explicit artifact
  checksum verification and disabled credential persistence.
- Updated reqwest to 0.13.4, TOML to 1.1, SHA-2 to 0.11.0, and process-wrap to
  10.0.0. Kept cargo-platform pinned to 0.3.2 to preserve Rust 1.88 support;
  the proposed 0.3.3 upgrade requires Rust 1.91 and was reverted.
- Added explicit `release/<Cargo version>` branch publication alongside manual
  workflow dispatch. The branch must match the package version. Publication
  still requires all existing validation jobs and the `release` environment;
  releases are serialized across the repository.

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
