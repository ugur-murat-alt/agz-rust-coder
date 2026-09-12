---
name: agz-rust-repair
description: Diagnose and repair Rust compiler failures with AGZ Rust MCP, preserving ownership contracts and validating candidate fixes.
---

# Compiler-driven repair

Use the connected AGZ Rust MCP with the active checkout's absolute `dir`.
Start from the failed check's code, primary span, message and recorded options.
Use `check(options={"context":true})` when fresh diagnostics or source context
are missing. Prefer compact output; request more detail for an omitted relevant
diagnostic. A full response is still bounded.

Separate infrastructure from source failures. A missing lockfile, unauthorized
dependency root or unavailable analyzer has not established a Rust code defect.
For E0514, confirm toolchain and artifact provenance, then use a workspace- and
toolchain-specific target directory through the approved Cargo runner. Preserve
existing build artifacts and dependencies; do not clean or upgrade by reflex.

For ownership/lifetime errors, inspect the producer and consumer contract before
adding clones, allocation or synchronization. Use `explain` for a specific trait,
macro or cfg question and `docs` for the exact resolved dependency API. Analyzer
unavailability must not be reported as evidence that a symbol/API is absent.

For isolated alternatives, create/stage/validate with `change`, then use
`repair(action="analyze")` against that failing revision. `repair(action="try")`
or `compare` evaluates bounded host-supplied or compiler-suggested candidates.
Declare a test gate when behavioral correctness matters: compileVerified alone
does not establish behavior. Review the patch and new/remaining diagnostics;
export/apply only the chosen candidate. `repair(action="minimize")` is useful
for a reproducible bug report; inspect failure identity and export verification.

Stop a repair attempt when its budget is exhausted or a required input is
missing; report the concrete blocker. Never conceal failure by dropping tests,
weakening a contract or changing unrelated files. After applying a fix, run the
smallest relevant regression check and the project's required delivery gates.
