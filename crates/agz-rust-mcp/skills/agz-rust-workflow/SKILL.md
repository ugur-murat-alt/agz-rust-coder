---
name: agz-rust-workflow
description: Implement Rust changes with AGZ Rust MCP using compiler diagnostics, exact dependency APIs, bounded context and focused validation.
---

# Rust implementation

Use the connected AGZ Rust MCP. Client prefixes vary; discover its available
tools once. Pass the active checkout's absolute `dir` to workspace tools.

`dir` may be the repository root, a package/source subdirectory, or another
authorized worktree. A branch name is not a directory restriction. Configure
the common project directories with `--allow-root` and shared path-dependency
directories with `--allow-dependency-root`; client roots can only narrow them.
Use a separate target for each checkout. Do not redirect an operation to a
different checkout just to bypass a missing-root error.

Read the owning project rules and Cargo manifests/lockfile before choosing the
implementation. Use `context(action="prepare")` with a concrete purpose and a
file/symbol anchor to gather a bounded capsule. Expand only an unresolved item;
use `context(action="delta")` after source changes instead of reloading context.
For a single semantic question, use `definition`, `references` or `symbol`.

Verify new dependencies with `crate_lookup`. For an existing dependency, use
`docs` with its resolved version and the specific symbol; keep expensive local
generation opt-in. `api(action="probe")` can type-check an uncertain snippet in
the workspace configuration. Read live schemas for action-specific fields.

For a small edit, implement directly and call `check(target="check",
options={"context":true})`. Let the compiler diagnostic guide the next edit.
For candidate isolation, `change` creates/stages/validates/exports a changeset;
the caller reviews and applies its write-free patch. `work` additionally tracks
an explicit scope, behavior contract, acceptance gates and repair budget. Use it
when that lifecycle is needed; keep simple edits simple. A work handoff is
single-use and bound to its revision; inspect state before resuming after doubt.

Choose validation for the changed behavior. Select exact workspace members with
`check.options.packages=["member"]`. For one Cargo target, also set
`cargoTarget={"kind":"test","name":"integration_name"}`; kinds are `lib`, `bin`,
`test`, `example`, and `bench` (`lib` has no name). A Cargo target requires
explicit packages and applies to check/build/clippy/test. Paths and globs are rejected.
The response records `scope.strategy="explicit"`, selected packages and options;
a selected pass is not full-workspace validation.
`verify` test actions accept these selectors under `configuration`. Test plans
and runs honor the selection and report subset coverage. Complete Cargo runs
without mappings or substring filters use one configuration-wide command in
`testPlan.executionGroups`. The `items` list is the advisory target inventory;
`testRun.items` records execution groups, not independent per-package passes.
Keep packages and features together so Cargo preserves dependency feature
unification. Never auto-enable `required-features`; conditional targets only
participate when Cargo enables them for the requested configuration.
Manifest `test=true` includes examples and benches; explicit named selectors
override `test=false`. Default Cargo suites include doctests and compile ordinary
examples without counting those builds as tests. Separate compilation scopes
report `COMPILED`, with zero tests. Nextest retains separate Cargo doctest scopes.
Empty test summaries and opaque harness output cannot borrow another scope's
pass. `configuration.testFilter` selects an ordinary-test subset with an explicit
doctest gap; use it or exact test mappings, not both.
If workspace inputs change between planning and completion, `test_run` returns
`STALE` and retains individual results without granting combined success.
New or removed Cargo target files refresh metadata in the same MCP session;
ordinary source-body edits retain metadata reuse. If discovery reports changed
inputs, obtain fresh evidence after the inputs settle.

`options` also supports features, a platform `targetTriple`, and a test-name
substring. `testFilter` requires `target="test"`; selected/filtered tests without
executed-test evidence are inconclusive. Use `target="build"` for compilation
and linking without executing a custom harness. Check/clippy provide earlier
compiler feedback. Use `verify` for an explicit test or feature matrix. Run the
project's required delivery gate; `target="all"` and `fmt` cannot be narrowed by
package/target selection. `all` covers every stage in the recorded configuration,
not every feature/platform combination.

Treat `STALE`, `INCONCLUSIVE`, `UNAVAILABLE`, missing tools and truncated evidence
as gaps. Revalidate after the relevant input changes; do not repeatedly retry an
unchanged failure. Cargo/rustc is authoritative; semantic and audit results are
advisory. Keep warnings and omission counts visible in the report. Distinguish
local checks, real adapter evidence, CI and release acceptance.
