# Dependency and MSRV evidence

Critical dependency versions were queried with Cargo 1.88.0 on 2026-08-31.
Unknown published `rust-version` metadata is not treated as compatibility
evidence; the locked graph must still pass the Rust 1.88 delivery gate.

| Crate | Pinned/selected version | Published `rust-version` |
|---|---:|---:|
| `rmcp` | `3.1.4` | `1.88` |
| `process-wrap` | `9.0.0` | `1.86.0` |
| `cap-std` | `4.0.3` | unknown |
| `fs4` | `1.1.0` | `1.75.0` |
| `cargo_metadata` | `0.23.1` | `1.86.0` |
| `cargo-lock` | `11.1.0` | `1.85` |
| `lsp-types` | `0.97.0` | unknown |
| `reqwest` | `0.13.2` | `1.64.0` |
| `scraper` | `0.24.0` | unknown |
| `sysinfo` | `0.37.2` | `1.88` |
| `regex` | `1.11.2` | `1.65` |
| `url` | `2.5.7` | `1.63` |
| `tempfile` | `3.20.0` | `1.63` |

`process-wrap 9.0.0` was also checked for the default process-group,
JobObject, kill-on-drop, and creation-flag features plus its `tokio1` frontend.
The RMCP API remains pinned to annotated tag `rmcp-v3.1.4`, peeled commit
`4a738b9dd99eaca418b614afa433a0cbdaf8d056`.
