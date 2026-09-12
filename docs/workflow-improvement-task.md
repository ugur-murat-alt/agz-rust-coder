# Rust workflow and efficiency work

Started 2026-09-12 from `main@10a4d8cb11c017efe30c87625c9d01c27410ef55` in
`agz-rust-mcp`; the former `agz-rust-coder` directory is empty. The checkout
was clean. Implementation branch: `codex/rust-workflow-efficiency`.

## Acceptance

- Ship focused coding, repair, refactoring, and performance skills inside the
  same executable and Cargo package, available through MCP prompts/resources.
- Allow clients with filesystem skill discovery to export the bundled skills
  without installing a second MCP, overwriting custom skills, or running Cargo.
- Reduce redundant compiler payload in compact checks while preserving status,
  diagnostics, omissions, source suggestions, and useful failing-test output.
- Measure the before/after response on the same compiler-error fixture; keep
  transport savings separate from build speed or agent-quality claims.
- Validate affected behavior, protocol compatibility, package inclusion, and
  the repository's required local delivery gates. Preserve release identities.
- User follow-up: support authorized main checkouts, nested Git worktrees and
  member/source subdirectories without incorrect dependency preflight failures;
  preserve isolated targets and source identity across sibling checkouts.

## Initial evidence

The installed MCP reached Cargo, but the existing project target contains Rust
1.98 artifacts incompatible with pinned 1.88 (`E0514`). No source regression is
inferred from that run. Validation uses a task-specific external target via
`rust-balanced`; existing target artifacts remain intact.

The failing compact response retained 5 diagnostics, reported 335 omitted
diagnostics, and repeated raw Cargo JSON in stdout, tail, and reason until the
wire cap truncated the result. This is the concrete payload regression target.

## Progress

- Initial checkout, ownership rules, public tool surface and prior identity
  rename verified.
- Baseline workspace/all-target/all-feature check passed with Rust 1.88 using
  the isolated target.
- Four bundled skills, offline CLI export, and compact human-output handling
  implemented. CLI, streaming and MCP lifecycle focused checks passed (13 tests).
- Same 40-error fixture: median wire bytes 46,827 -> 8,519 (81.8% smaller), with
  the same five E0308 diagnostics and omission counts. Truncation true -> false.
- Two worktree regressions reproduced before fixes: broad parent roots hid
  external dependencies; member subdirectories omitted the owning workspace's
  inherited dependencies. Shared metadata corrections implemented; real-worktree
  integration passed, including independent checkouts, shared-dependency source
  changes and inherited workspace dependencies.

- No-Git source-subdirectory checks now fingerprint the complete resolved Cargo
  workspace, including build.rs outside the requested src directory.
- Audit reads and walks use the requested directory; unreadable/omitted inputs
  return INCONCLUSIVE instead of a misleading CLEAN response.
- The same configured-parent/selected-child defect was reproduced in context
  (missing definition/consumer source evidence) and API (INVALID source read).
  Their source reads now use the retained requested-directory capability; both
  regression cases passed after the fix.
- Codex Rust root arguments now include Projects, Documents/Codex and managed
  Codex worktrees, with corresponding explicit shared-dependency roots. The
  original config is backed up; source-built runtime smoke passed across those roots.
- Upstream latest remains agz-rust-mcp-v0.3.0 (published 2026-09-11); this work is
  an unreleased local change. No dependency/toolchain upgrade is included.

## Final local validation

- Formatting, workspace/all-target/all-feature Clippy with warnings denied,
  workspace tests, explicit Rust 1.88 check and optimized release build passed.
  The standard suite leaves seven opt-in tests ignored.
- All three required xtask smoke commands passed against the release binary.
  OpenCode used the repository-pinned beta-18743 in an isolated temporary
  installation; provider traffic stayed on its local fake provider.
- A single real MCP process checked a main repository under Projects and its
  linked worktree under Codex worktrees, including the worktree's src directory.
  Audit scanned the requested file; context verified the source hash; API resolve
  and real Rust Analyzer symbol lookup succeeded. An intentionally broken
  worktree did not poison the main checkout; changing the external shared
  dependency correctly failed the main check.
- The final release binary measured a median 8,519 response bytes versus 46,827
  in the baseline, an 81.81% reduction across three samples per binary. Compiler
  diagnostics, omission counts and evidence counters were identical; wire
  truncation changed from true to false. This is transport volume only.
- The benchmark script accepts --baseline-report and refuses to report gains
  when fixture hashes, sample counts, diagnostics or compiler evidence differ.
- Structured results: [workflow-improvement-evidence.json](workflow-improvement-evidence.json).
  Raw local transcripts are retained in .state/workflow-efficiency-20260912.
- One protocol run correctly reported STALE while this task was editing a
  fingerprinted benchmark file. The subsequent run used a stable source snapshot
  and passed; the stale protection was not weakened.

- Cargo package verification passed: the archive contains all four SKILL.md
  files, and the extracted crate compiled successfully.
- The tested release binary was atomically installed in the user's existing
  local MCP location after backing up the previous binary. Four previously
  absent Codex skill directories were installed from the binary's export.
  The exact configured managed launcher was then started from the empty legacy
  project directory: 20 tools, 4 prompts, 8 resources and a selected-project
  single-file audit passed. The installed workflow file matched the MCP prompt.
- Existing client connections continue to use their running process until
  reconnection or app restart. No existing process was stopped. Installation
  provenance and rollback paths are retained under .state/workflow-efficiency-20260912.
- No commit, push, CI run or published release was performed.

## Exact package and Cargo-target selection

- Added optional `packages` and typed `cargoTarget` to the existing validation
  options. Names resolve against actual workspace metadata before Cargo starts.
  Unknown names, globs, flag/path inputs and attempts to narrow all/fmt fail
  closed; doc supports package selection while retaining its doctest command.
- Explicit packages remain selected with feature/platform options. The command,
  package IDs, options and `scope.strategy=explicit` identify the precise run.
- Verify test plans honor the selectors and bind each selected inventory item to
  its actual package/target. Conflicting mappings never launch a broad fallback.
  Explicit selection remains TESTED_SUBSET, even after a global changed input.
- Selected/filtered tests with no executed-test evidence are INCONCLUSIVE.
  Regression fixtures cover a selected test succeeding beside an unrelated
  compiler failure, invalid selection, mapped targets and exact command identity.
- Optional schema changes were reviewed for six affected tools; the 20-tool
  catalog, annotations and legacy input compatibility are preserved. Benchmark
  scripts now share one bounded stdio client instead of duplicating it.

### Current measurements and validation

Three cold and three warm runs per binary used the same eight-package fixture,
Rust 1.88, isolated targets, no sccache/incremental, and exactly one executed test.
The selected build prepares 3 units instead of 40. Cold median Cargo time is
1,270 -> 352 ms (72.28% lower); total MCP latency is 1,899.06 -> 991.95 ms.
Warm Cargo time is 128 -> 103 ms, while total latency is 649.15 -> 633.18 ms.
These measurements validate reduced work for the selected test, not equivalence
to whole-workspace correctness or gains on every project.

The new release binary retains the compact-response gain: 46,827 -> 8,517 median
bytes (81.81%), with identical diagnostics, omissions and compiler evidence.
Formatting, workspace/all-target/all-feature Clippy with denied warnings, all
workspace tests, Rust 1.88 check and optimized build passed. Seven opt-in tests
remain ignored. All three xtask smokes passed. The package archive contains all
four skills and the new selection module, and the packaged crate compiled.

The real-worktree runtime smoke passed again with the new binary, now including
explicit package/lib checks from main, linked worktree and src. Shared-dependency
changes still invalidate the result; a broken worktree remains independent.

Binary e10ce04b0d809ef15816b5b6d3975d81c580575e21f28da9d53482a37c27fd15
was installed with a backup of the previous local build. Only the workflow skill
needed an update; all installed files matched the prior manifest before editing.
The exact managed launcher, started from the empty legacy directory, ran one
selected integration test from a managed-worktree src path containing spaces
and Unicode. Its schema, four prompts, eight resources and installed skill text
matched. Existing client processes need reconnection to use the new binary.

Raw reports, command logs, tool schemas and installation provenance are retained
in `.state/scope-selection-20260912`. No commit, push, CI or published release
was performed.

## Default verify inventory and metadata freshness

The default runner repeated a broad workspace test command for every inventory
item. A two-package fixture with six distinct tests recorded twenty executions,
and an empty target could borrow a sibling target's passing evidence. Every
inventory item now executes its resolved package and Cargo target; doctests
retain a separate per-package command. Mapping ownership, the configured runner,
feature selection and filters are preserved. Unknown/conflicting mappings fail
before execution; filtered coverage remains a subset.

Verify now captures workspace identity before planning and after execution. A
new failing target added between items reproduces the previous false full-suite
result and now returns STALE. This is pre/post evidence, not an atomic snapshot.

The first installation smoke exposed another real defect: adding a target
between requests reused cached Cargo metadata and omitted the new failure. That
candidate was automatically rolled back. The final correction fingerprints the
bounded conventional target layout (including build.rs), preserves metadata hits
for source-body edits, and refuses to cache inputs that change during discovery.
Addition/removal and mid-discovery mutation regression tests pass.

### Measurement and final local verification

The identical six-test fixture now executes six cases instead of twenty, with
three cold and three warm samples per binary. Cold median MCP latency changed
5,138.70 -> 5,073.15 ms (1.28%; no meaningful cold speedup claim). Warm median
changed 4,797.64 -> 4,271.75 ms (10.96% lower). Scope identities and source hashes
match; raw reports retain all commands and test evidence.

Formatting, denied-warning Clippy, all-target/all-feature workspace tests,
explicit Rust 1.88 check, optimized build, all three xtask smokes and Cargo package
verification passed. The standard test run passed 595 tests and left seven
opt-in tests ignored. One earlier concurrent metadata test had a temporary-path
failure; its isolated rerun and this final full suite passed without weakened
assertions. Its cause is not established, and the logs are retained.

The final binary SHA is 33c19fc14eca308e998cb054726c4bb5d76929eeb599de8f1d6631dfb08ed24a. It was installed with a backup of the prior
local build, together with the updated workflow skill. The exact configured
managed launcher started from the empty legacy directory. A real main checkout
and linked worktree, both called from src, each executed three distinct tests.
Adding a failing target to the worktree returned FAIL on the next request while
the main checkout remained successful. Spaces/Unicode, twenty tools, four prompts,
eight resources and equality of the installed skill and MCP prompt all passed.
Existing clients need reconnection to pick up the new process.

Raw evidence, rejected-candidate/rollback details and source provenance live in
`.state/verify-inventory-20260912`. Historical measurements above identify their
own binaries; this package did not remeasure the compact-response or explicit
selection benchmark. No commit, push, CI or published release was performed.

## Scheduler latency and concurrent validation profiles

The fixed 500 ms wait previously restarted for every Cargo gate, even when the
source had already settled in the same process. A controlled diagnostic arm on
the previous binary changed only debounce from 500 to zero: warm median total
latency was 635.95 vs 135.54 ms while Cargo was 118 vs 119 ms. queueMs includes
admission/preflight and scheduler waiting; those components are not falsely
reported as independent measurements.

A separate real-Cargo barrier regression reproduced concurrent check/test on
unchanged source incorrectly cancelling the first command as SUPERSEDED. The
scheduler now uses a source fingerprint independent of the command/profile;
full validation and singleflight identities still include command and environment.
Both profiles pass with separate jobs, while a genuine source change continues
to supersede the older job.

The actual MCP stdio comparison confirms the same defect and fix: two concurrent
check/test calls on the prior binary returned one FAST_PASS and one SUPERSEDED;
the installed candidate returned two FAST_PASS results, separate job/command
identities and one executed test. This supplements the real-Cargo service test.

The configured quiet-input window is now shared per observed workspace source
state. A first/new source waits normally; unchanged later commands use the
remaining window without restarting it. History is bounded to 256 recently used
roots, with conservative waiting after eviction. Dirty notification rearms the
window. Completed results are never reused for later explicit requests, and
existing lease/cancellation/deadline checks remain in force.

### Comparable measurements and installation

At the unchanged 500 ms setting, the same one-test workload's warm median MCP
latency changed 635.95 -> 137.94 ms
(78.31% lower). Cold
median was 939.19 -> 935.46 ms; the
first source observation still includes the normal quiet-input window. Each
binary/setting/cache condition has three samples, with matching fixture hashes,
commands and test evidence, and different cold/warm job IDs.

The full six-test inventory stayed unchanged: cold median
5073.15 -> 2544.59 ms;
warm 4271.75 -> 1233.07 ms.
Every run executed the same six cases exactly once. Raw per-item evidence is
retained, and the previous binary's metrics are not relabelled as this binary's.

Format, denied-warning workspace Clippy, all-target/all-feature workspace tests,
Rust 1.88 check, optimized build, all three xtask smokes and Cargo package
verification passed. Seven opt-in tests remain ignored. The configured managed
launcher passed main/src and real linked-worktree/src, including a newly added
failing worktree target without poisoning main. Twenty tools, four prompts and
eight resources remain; all installed skill texts match their MCP prompts.

Binary 094a0b69a018f95cc287fd00964defd55db28d2f214e6f72ba5e33c2f68c873d was installed with a backup of the prior
build. The performance skill now explains the shared window and inclusive queue
metric. Existing client processes need reconnection. Source/provenance/reproduction
logs are under `.state/scheduler-latency-20260912`. No commit, push, CI or public
release was performed.

## Cargo target coverage and actual build validation

Five controlled Rust 1.88 fixtures were checked with real Cargo and the previous
installed MCP. Cargo failed all five. Verify incorrectly reported
FULL_REQUESTED_SUITE for an enabled example test, ordinary example compile error,
enabled bench test and proc-macro doctest. A failing cdylib unit test was omitted
with INCONCLUSIVE. The candidate now returns FAIL for all five. Original Cargo
logs and MCP responses are retained. Fixture hashes were recorded after the
baseline and before the candidate, using the unchanged controlled files; this
is not represented as a pre-baseline hash capture.

Inventory now honors target.test, explicit named selectors overriding test=false,
proc-macro doctests and native library unit targets. Ordinary examples compile
without cfg(test) using Cargo build. Their COMPILED outcome has zero executed
tests and cannot alone establish a full suite. Requested omissions prevent full
status. Custom harnesses run, but an opaque successful exit without recognized
test summaries remains incomplete evidence. Non-host matrix execution remains
unsupported rather than implying cross-platform proof.

The existing check tool accepts target=build, including exact package/target
selection. Profile, change and host verify matrix can also use this gate.
Regression fixtures demonstrate successful cargo check followed by a real link
failure from cargo build. Tools/list still exposes 20 tools; only four tool
schemas gained additive build values and compile-scope descriptions. Existing
fields, enum ordering and annotations were checked against the prior binary.

Focused tests, formatting, denied-warning workspace Clippy, all-target/all-feature
tests, MSRV 1.88 check, optimized build, all three xtask smokes and Cargo package
verification passed. Seven opt-in tests remain ignored. The first whole-suite
Clippy attempt found a missing new enum arm in a test helper; it was fixed before
the successful delivery run. A later compatibility test also caught a missing
compile-only phrase in the non-host warning; the explanation was restored.
Package contents match the source and four skills.

Installed development binary: cc3c758e7c605d5b943de1dca18d447203d8349fa61950f26ebc3db142bd2c1d.
The prior binary and changed skill were backed up. The configured launcher,
started from the empty legacy directory, passed main/src, a real linked worktree
with Unicode/spaces, normal example compilation and explicit example build.
Adding a new failing worktree target failed only that checkout; main still passed.
All four installed skills match their MCP prompts. Existing clients need a
reconnect. No commit, push, public release or CI claim is made. This package makes
no new performance claim; previous measurements retain their original hashes.

## Cargo feature semantics and configuration-wide suites

Four disagreements were reproduced on installed binary
cc3c758e7c605d5b943de1dca18d447203d8349fa61950f26ebc3db142bd2c1d, with fixture hashes captured before the
baseline. Cargo passed a default run with a disabled conditional example, while
verify auto-enabled its required feature and failed. Cargo also passed workspace
feature selections `extra` and `a/extra`; splitting them across per-package
commands caused verify failures. Most seriously, a dependency enabled a workspace
member's feature and broke its test in the combined Cargo run, while verify's
separate commands incorrectly returned FULL_REQUESTED_SUITE.

Complete Cargo configurations without explicit mappings or substring filters now
execute one canonical test command. Workspace requests use `cargo test --workspace`;
package/target selections preserve their original flags together and remain subset
evidence. `testPlan.items` stays the advisory target inventory and
`testPlan.executionGroups` records actual execution scopes. `testRun.items` never
pretends the combined command produced independent package results. Conditional
required features are no longer auto-enabled; Cargo resolves eligibility and
feature unification. Default Cargo suites include doctests and ordinary example
compilation, without counting example builds as tests.

Stream counters retain test binary counts, summary counts and empty-summary counts
independently of truncated human output. A positive group requires nonempty test
evidence and enough summaries for the executables and doctest scopes. Empty or
opaque custom harness scopes cannot borrow another scope's success. Mapped and
impact scopes still retain their exact targets and whole-run freshness checks.
The four original disagreements now match their Cargo oracle outcomes.

The same six tests in two packages still execute exactly once. Cargo invocations
per verify changed 6 -> 1. Across three samples per binary/cache state, cold median
was 2531.70 -> 1550.92 ms and
warm 1225.62 -> 709.32 ms
(42.13% lower warm latency). The fixture hashes and requested
inventory match. This measures this controlled workload, not general build speed.
Earlier performance results keep their original binary hashes.

Focused regression tests, formatting, workspace Clippy with warnings denied,
all-target/all-feature tests, Rust 1.88 check, release build, all three xtask smokes
and Cargo package verification passed. Seven optional tests remain ignored.
Twenty tools, all input schemas and annotations remain unchanged. Only additive
output fields for execution groups and streaming counters changed. Packaged
source/four skills match the checkout. Installed binary:
f030f54eedb1e475992206260e8c4a776abbfb983e8a14da405c671278eae154.
The prior binary and changed skill were backed up. The configured launcher passed
main/src and a real linked worktree with Unicode/spaces; explicit example build
works, and a new worktree-only target failure does not poison the main checkout.
Existing client connections need reconnection. No commit, push or release occurred.

## Closure and delivery — 2026-09-13

The user requested that this goal be finalized and delivered. Open-ended work
stops here. The final review reproduced a remaining false full-suite claim:
path-only mappings plus a widened inventory ran packages separately, returned
FULL_REQUESTED_SUITE, and missed a feature-unification failure in Cargo's combined
workspace run. Full success now also requires the canonical Cargo configuration
group. The same mapped run correctly returns TESTED_SUBSET and the combined run
returns FAIL. A real two-package regression and binary-level reproduction passed;
EN/TR tool documentation explains the inventory/result distinction.

The final local gates passed: formatting, warnings-denied workspace Clippy,
all-target/all-feature tests (610 passed; 7 opt-in ignored), Rust 1.88
check, release build, all three xtask smokes and Cargo package verification.
The final guard leaves tool schemas, descriptions and annotations unchanged.
The tested binary was installed with a drift check and backup; configured-launcher
checks passed on main/src, a real Unicode/space worktree/src and example build.
A worktree-only compile failure leaves main valid. All four installed skills
match their bundled prompts. Existing clients need reconnection.

Final binary SHA-256: 6df532c0c75bcffe84301511a59d26143aea66f82c2b0f6669cc7f90f5ce196a.
Historical benchmark measurements retain their original binary hashes and were
not remeasured for this closing correctness guard. Source changes, all new files,
the binary, four skills, verified crate, raw evidence and checksums are delivered
locally. No commit, push, PR, CI or published release was performed.

See [the comprehensive delivery report](DELIVERY-2026-09-13.md) and
[structured evidence](workflow-improvement-evidence.json). Optional runner work
is documented as a limit, not pending required work. No further improvement cycle
is scheduled by this task.
