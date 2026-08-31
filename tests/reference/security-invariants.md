# Frozen security and behavior inventory

The source repository remains the behavior owner until the Rust parity suite is
green. Each row names the legacy fixture and the Rust contract that must retain
its invariant.

| Area | Legacy evidence | Rust contract |
|---|---|---|
| Tool schemas | `test/schema.contract.test.ts`, `test/fixtures/tool-schemas.json` | 12 deterministic input/output schemas, unknown fields rejected, optional `dir` as the sole intentional input change |
| Root selection | `test/rustdetect.test.ts`, `test/tools.test.ts` | linked-worktree boundary, sole nested project, same-depth ambiguity, no parent escape |
| Symlink and path safety | `test/lsp.symbol.test.ts`, `test/lsp.edits.test.ts`, `test/docs.test.ts` | lexical escape, external symlink, symlink swap, external URI/edit rejection |
| Source write-free edits | `test/edit-adapter.test.ts`, `test/compiler.diagnostics.test.ts`, `test/lsp.edits.test.ts` | snapshot/version checks, overlap rejection, byte-identical files after rename/refactor/suggestions |
| Gate authority | `test/gate.service.test.ts`, `test/gate.identity-scope.test.ts` | FAST/FULL distinction, incomplete identity no authority, pre/post stale, active-only singleflight, terminal PASS non-reuse |
| Process cleanup | `test/gate.process.test.ts` | bounded split streams, complete drain, timeout/cancel process-group termination |
| Atomic artifacts | `test/cache.retention.test.ts`, `test/benchmark.ablation.test.ts` | lock, create-new temporary file, same-directory publish, no symlink following |
| Diagnostics | `test/compiler.diagnostics.test.ts`, `test/tools.test.ts` | Cargo JSON, child/macro spans, ANSI stripping, complete MachineApplicable suggestions only |
| Audit | `test/tools.test.ts`, `test/knowledge.test.ts` | bounded scan, generated sources skipped, comments/strings masked, path escape rejected |
| Docs | `test/docs.test.ts` | exact lock version/source, ambiguity preserved, partial cache rejected, external target, no workspace write, offline fail-open |
| LSP framing | `test/lsp.client.test.ts` | partial/concatenated frames, timeout cancellation, server requests, malformed values, bounded shutdown escalation |
| LSP lifecycle | `test/lsp.manager.test.ts` | max instances, active-request protection, idle cleanup, crash restart, graceful shutdown |
| Semantic tools | `test/lsp.symbol.test.ts`, `test/lsp.navigation.test.ts` | UTF-16, bounded results/content, advisory-only results, workspace-relative locations |
| Protocol/public surface | `test/plugin.integration.test.ts` | twelve tools, bounded guidance, disabled groups omitted, compiler remains authoritative |
| Bilingual/release drift | `test/release-docs.test.ts` | version/tool/config/link parity and negative drift checks |

## Golden result rules

- Every tool returns schema version 1, the tool name, status, summary, typed
  data, warnings, `untrustedData`, and `truncated`.
- `structuredContent` and text content serialize the same deterministic JSON.
- Cargo/test failures are typed outcomes with `isError=false`; boundary,
  timeout, missing binary, and resource admission failures use `isError=true`.
- External source strings stay under `data`; they never become instructions,
  summaries, or interpolated warnings.
- The total serialized result stays within 49,152 bytes without slicing JSON.
- Rust Analyzer output is advisory and cannot create validation authority.
