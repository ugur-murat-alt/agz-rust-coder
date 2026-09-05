# Rust evidence and efficiency: review evidence

Review date: 2026-09-05. Baseline: `feb7f1b5fe9022ea09870bc375cded075a27439e`.
Scope: the six-part plan, related process/LSP lifecycle consumers, public MCP
contracts, tests, documentation and CI configuration. This is a bounded review,
not a claim that every possible defect or interleaving has been eliminated.

## Source audit and corrective changes

The tracked-tree inventory was scanned for conflict markers, unwanted generated
files, symlinks and unsafe-code blocks. Production changes were inspected in
context, including their callers and error mappings. Unsafe-code text in the
audit test fixtures is intentional input to the scanner, not new production
unsafe code. Temporary dependency/bootstrap workflows are absent from the final
tree. Release identity, source-write-free edit behavior and retained root
capabilities are preserved.

| Finding | Correction | Evidence |
| --- | --- | --- |
| Compiler errors disappeared behind the bounded tail; first-error timing was not connected | Stream raw Cargo stdout before terminal sanitation; retain errors independently; record request/step timing and omission/corruption counters | `streaming_evidence.rs`, real `check_service.rs` failure/context test |
| Repeated file-set counting grew quadratically | Maintain independent workspace/external counters without changing content hashes or budgets | Existing identity tests; nine same-input comparisons below |
| Scope settings covered only `check`; validation configuration was implicit | Typed feature/target/filter/runner options, conservative widening and recorded actual profile | Profile unit tests, affected Clippy/test and build-script regressions |
| A test-name filter matching nothing could report a pass | Require evidence of executed libtest cases; no/custom/ignored cases are inconclusive; nextest uses `--no-tests=fail` | Filtered Cargo and real Nextest tests |
| Edit/context evidence could outlive a failed compilation's source state | Revalidate failed compilation inputs, clear usable edits after stale/cancel/timeout/unclean results; expose context freshness explicitly | Check integration, source replacement and existing stale-input tests |
| Suggestion reads were unbounded and columns treated as bytes | Capability-based per-file/total read bounds and Unicode scalar-value coordinates, including CRLF | Sparse oversized-source rejection; Unicode property and fixture tests |
| Completion/startup/capacity notifications could be missed between checking and subscribing | Register before checking process/job/LSP state; box the cold capacity wait to constrain nested future size | Existing real lifecycle/manager tests plus Loom positive and negative controls |
| The supervisor could start pre-cancelled work or spin on an expired deadline | Pre-spawn rejection, duration-overflow validation and bounded cleanup loop | Process deadline tests and repeated Git cancellation regression |
| Pre-spawn cancellation was converted into generic Git/metadata/analyzer error | Preserve typed cancellation and timeout through each process-result boundary | Typed mapping unit test; failing regression followed by 20/20 successful repetitions |
| Test configuration comparison and zombie detection produced misleading failures | Compare task/sync calls with the same configuration; distinguish live processes from unreaped zombies | Protocol tasks and existing process-tree smoke coverage |

The final typed-mapping defect was found by rerunning the whole suite, not by
reasoning alone. Its original failure transcript is retained in the delivery
package. A separate smoke run correctly returned `STALE` when review documents
were modified while Cargo ran; subsequent validation uses an unchanged tree.
That was a changing-input rejection, not permission to suppress freshness checks.

## Local verification and reproducibility

Linux x86-64, Rust/Cargo 1.88.0. The final code-level gate produced **273 passing
workspace tests, zero failures, six explicitly ignored integration tests**.
Real Nextest, Sccache, Rust Analyzer and local rustdoc tests were additionally
executed, rather than claiming ignored tests passed. The two remaining ignored
adapters require live crates.io/docs.rs access. Two Python comparison tests
reject invalid measurements. The pre-spawn cancellation case passed 20 repeated
runs after its mapping correction.

Commands used (ordinary `cargo` resolved to the pinned 1.88.0 toolchain):

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
cargo test --workspace --all-targets --all-features --locked --no-fail-fast
cargo check --workspace --all-targets --all-features --locked
cargo build --release --workspace --all-features --locked
python3 -m unittest discover -s benchmark -p test_identity_compare.py
cargo test -p agz-rust-coder --test lsp_real --locked -- --ignored
cargo test -p agz-rust-coder --test docs --locked real_local_generator_writes_only_to_the_external_cache -- --ignored
cargo run --locked -p xtask -- protocol-smoke
cargo run --locked -p xtask -- opencode-smoke
cargo run --locked -p xtask -- benchmark-smoke
cargo package --locked -p agz-rust-coder --allow-dirty
cargo publish --locked -p agz-rust-coder --allow-dirty --dry-run
```

For optional integration, the compiled `check_service` test executable is run
with `real_nextest --ignored` and separately `real_sccache --ignored`, the latter
with `RUSTC_WRAPPER` set only for that executable. This avoids wrapping the build
of the test harness. Pinned Nextest 0.9.143 and Sccache 0.17.0 download checksums
are in CI. Real Rust Analyzer uses the 1.88.0 component. OpenCode smoke uses
`@opencode-ai/cli@0.0.0-beta-18743`. The final command transcripts and explicit
step outcomes are shipped in `evidence/` and `verification-summary.json` in the
delivery package. Formatting, strict Clippy, the workspace/real-tool tests,
release build, all three smoke commands and package verification completed.
`cargo publish --dry-run` was also attempted: it re-packaged and compiled the
crate, then stopped with `attempting to make an HTTP request, but --offline was
specified`. Its network-dependent registry validation is therefore **not passed**.
This is not a complete remote release gate, and no actual registry upload was
attempted.

The local container has a 4 GiB memory limit; validation used two build jobs.
Compiler/public registry inputs came from checksum-verified workflow artifacts,
not credentials. All local compilation was offline. No tests or quality gates
were disabled to obtain success.

## Measured identity-stage effect

Raw samples: [identity-comparison.json](evidence/identity-comparison.json).
Both binaries used the same 1.88.0 toolchain and release profile, three warmups
and 15 measurements per scenario. Invocation order alternated between scenarios.
Each pair used the same absolute fixture, command/environment identity and files.
Every sampled hash agreed between baseline and candidate; differing hashes would
have stopped the comparison. Counts below exclude the additional Cargo.toml file.

| Rust files | Scenario | Baseline median, ms | Candidate median, ms | Reduction |
| --- | --- | ---: | ---: | ---: |
| 1,000 | unchanged | 10.03 | 8.37 | 16.5% |
| 1,000 | one Rust file changed | 10.01 | 8.32 | 16.8% |
| 1,000 | manifest changed | 9.63 | 9.03 | 6.3% |
| 5,000 | unchanged | 90.24 | 48.19 | 46.6% |
| 5,000 | one Rust file changed | 91.01 | 49.04 | 46.1% |
| 5,000 | manifest changed | 90.23 | 49.17 | 45.5% |
| 10,000 | unchanged | 266.59 | 102.80 | 61.4% |
| 10,000 | one Rust file changed | 263.66 | 104.99 | 60.2% |
| 10,000 | manifest changed | 263.82 | 97.69 | 63.0% |

This is a **single-host, synthetic no-Git, warm-filesystem-cache identity-stage
measurement**. It does not measure cold Git discovery, total Cargo build/test
speed, agent success rate, LLM latency or token consumption. Final incidental
lifecycle changes did not alter the identity implementation used for measurement;
binary and source SHA-256 provenance is retained with the delivery evidence.

## Unverified boundaries and remote delivery

Windows/macOS compilation, live crates.io/docs.rs integration and a fresh
cargo-deny vulnerability-database check were not executable in this container.
Their CI gates are retained, with a pinned Linux optional-tools job added.
Cached manifest/license inspection is not a substitute for cargo-deny.
Sccache's supervised implementation explicitly requires Unix local sockets.
The Loom model covers notification-ordering logic, not all Tokio internals.
Pre/post input-hash agreement is not an atomic filesystem snapshot.

The GitHub connector in this session exposes read operations only. No source
push, PR creation, merge, version bump or release is claimed. The accompanying
checksum-bound Git bundle, patch, source ZIP, PR body and guarded `publish_pr.py`
can submit the exact reviewed tree to the existing feature branch and open or
reuse one PR. It refuses a moved baseline/preparation branch, never force-pushes,
and leaves `main` and release tags unchanged. Remote CI must run on that PR.

## Primary behavior references

- rustc JSON coordinates: https://doc.rust-lang.org/rustc/json.html
- Tokio notification behavior: https://docs.rs/tokio/1.53.1/tokio/sync/struct.Notify.html
- Nextest options and doctest boundary: https://nexte.st/docs/running-tests/
- Pinned Sccache modes: https://github.com/mozilla/sccache/blob/v0.17.0/docs/Configuration.md
