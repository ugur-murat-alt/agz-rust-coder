# Rust correctness and efficiency: six-part delivery

Baseline: `feb7f1b5fe9022ea09870bc375cded075a27439e` (0.1.1).
One proposed PR; no merge, tag, version bump or publication is part of this work.
See [usage](tools.md#explicit-validation-options) and [verification](rust-efficiency-evidence.md).

## 1. Streaming compiler evidence

Cargo stdout is decoded as bounded line-oriented JSON during execution, before
terminal sanitation and independently of the human-output tail. Diagnostic
storage is capped at 128 entries / 1 MiB, and a line at 256 KiB. Errors can replace
warnings when the budget is exhausted. Separate counters identify duplicates,
record omissions, malformed lines, oversized lines, and Cargo build completion.
The first complete diagnostic produces a bounded, explicitly provisional and
untrusted progress message. No early progress grants validation authority.

## 2. Avoidable identity work

Maintain workspace/external file counters rather than re-counting the entire
ordered set for every insertion. Preserve file de-duplication, authorization,
file and byte budgets, and content-based hashes. Do not reuse a completed check
as authority for a later request. The read-only `identity_measure` example and
`benchmark/identity_compare.py` compare the original and changed implementation
using the exact same fixture paths, command identity, files and hashes.

## 3. Explicit validation scope and configuration

`check.options` carries feature selection, built-in target triple, test substring
and runner. Existing defaults remain Cargo/default features/current host.
Affected/shadow scope extends to check, Clippy, tests and documentation. Full
validation always uses workspace scope. Empty/unknown changes, Cargo inputs,
build scripts, external path dependencies, procedural macros and explicit
feature/platform selection widen conservatively. Package reachability is not
claimed to be a complete test-impact oracle. `all` means all stages in the
recorded configuration, not every platform or feature combination.

## 4. Diagnostic-centered source context

Opt-in context joins the compiler's package/target provenance with authorized
source excerpts and exact resolved direct-dependency versions. At most 24
contexts, four source files of at most 1 MiB each, seven lines and 240 characters
per line are retained. Missing/oversized/unauthorized sources have explicit
reasons. `sourceHash` binds the returned excerpt. `input-identity-matched` means
pre/post input hashes agree, not an atomic filesystem snapshot or an instruction
to trust source text. The coding agent still makes the repair decision; no new
LLM or parallel semantic engine is added. Existing navigation/docs tools remain
available for deeper follow-up.

## 5. Independent correctness and performance evidence

Property tests exercise arbitrary Unicode and arbitrary stdout chunk boundaries.
A small Loom model validates register-before-check notification ordering, with a
negative control that detects the original ordering bug. Real-process and
protocol tests complement this limited model. Benchmark comparisons reject
mismatched input hashes, configurations, sample counts, NaN and zero durations.
Measurements concern input identity, not model quality, token use or total build
speed. Sources and reproducible commands accompany the raw samples.

## 6. Explicit optional acceleration

Nextest 0.9.143 is opt-in; missing/wrong tooling returns unavailability rather
than silently changing the requested runner. Test filtering has no full-pass
claim, and `all` retains a separate Cargo doctest stage. Sccache 0.17.0 requires
an explicitly configured absolute `RUSTC_WRAPPER`. The MCP owns a foreground
Unix-socket cache server and keeps compilation client-side in the supervised
process tree. It selects a local bounded cache, excludes remote/distributed
configuration and leaves incremental settings unchanged. The supervised mode
currently requires Unix; default Cargo remains portable. Real optional-tool
regressions and a checksum-pinned Linux CI job are included.

## Incidental defects corrected during review

Register completion notifications before checking completion in both process
shutdown and job subscriptions. Bound the supervision loop even when a killed
leader cannot be reaped; do not spin against an already-expired operation timer.
Reject pre-cancelled/expired work before spawning and reject duration overflow.
Read suggestion sources through retained capabilities with per-file and total
budgets; interpret rustc columns as Unicode scalar values, not byte counts.
Revalidate failed compilations before returning edit/context evidence; clear
edit packages after cancellation, staleness or incomplete cleanup. Correct
request-level first-diagnostic timing, timestamps, completed-stage count,
failed-test tails, doctestable workspace selection and additive schema fixtures.

## Delivery boundary

Implementation and local validation are recorded in the evidence report. The
GitHub connector exposed only read actions in the final review session, so a
remote PR and remote CI cannot be described as completed. The delivery bundle
preserves the full source, exact Git tree, patch, logs and guarded publication
script. The two temporary bootstrap workflows are not part of the final tree.

Final audit: register process/job/LSP completion and capacity notifications before checking state. Preserve pre-spawn cancellation/timeout status through Git, metadata and analyzer probes. The capacity wait is boxed once at process startup to bound nested navigation future size.
