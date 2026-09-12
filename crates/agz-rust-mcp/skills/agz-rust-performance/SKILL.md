---
name: agz-rust-performance
description: Measure and improve Rust build or runtime performance with AGZ Rust MCP using comparable baselines and explicit correctness gates.
---

# Measured Rust performance

Use the connected AGZ Rust MCP with the active checkout's absolute `dir`.
Choose the metric before changing code: build latency, runtime latency/CPU,
memory, or response bytes. Record source revision, workload, configuration,
toolchain, platform and warm/cold cache conditions for both sides.

For build latency, `profile(action="build_analyze")` records fresh Cargo evidence
and identifies observed rebuild behavior. Keep its evidence IDs and use
`build_compare` with matching configuration and enough samples. Expired evidence,
partial timing data or mismatched identities cannot justify a speed claim.
Resolve avoidable scope/rebuild work before enabling optional accelerators.

For runtime work, capture a baseline with `change`, stage the candidate, then
use `profile(action="runtime_compare")`. Declare a hypothesis, supported benchmark
adapter/workload, warmups, samples and positive thresholdPercent before running.
Include a correctnessGate for the behavior at risk. Read the tool's comparability
decision and stop reason; an unavailable adapter is not a measurement. A custom
benchmark through the project's approved runner is appropriate when the MCP
does not support the workload.

Apply one coherent optimization, validate its behavior, then compare under the
same inputs. Use distributions and raw samples rather than a single best run.
Separate identity/preflight time, Cargo time, queue time and response time.
`check.queueMs` includes admission/preflight and scheduler waiting; do not add
those components twice. Record whether the MCP process already observed the
same source: its debounce window is shared across unchanged-source requests,
while Cargo still runs afresh. A new source state starts a new window.
Transport byte savings do not establish token savings, build speed or agent
quality. Local Linux measurements do not establish other-platform gains.

Nextest and sccache are explicit optional accelerators, not required extra MCPs.
Inspect local support and trusted configuration first. Do not silently change
runner, features, incremental settings or the required doctest gate. Keep
workspace output directories isolated and avoid concurrent duplicate builds.
Report regressions and unmeasured boundaries alongside measured improvements.
