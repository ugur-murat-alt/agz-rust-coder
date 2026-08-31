# RMCP protocol decisions

These decisions are pinned to `rmcp 3.1.4` and the official
`rmcp-v3.1.4` source tag.

- Session initialization defaults to MCP `2025-11-25`.
- Stateless discovery explicitly requests preferred version `2026-07-28`.
- Only `check` and `docs` are task-eligible, and only when the client declares
  `io.modelcontextprotocol/tasks` support. Other tools remain synchronous.
- The server selects task mode with `CallToolResponse::Task`; capability-free
  clients receive `CallToolResponse::Complete` for the same typed result.
- A task is observable by `tasks/get` before its handle is returned. Terminal
  tool results are stored inline in `tasks/get.result`.
- A tool result with `isError=true` still completes the task. JSON-RPC failures
  fail it; cancellation produces the cancelled terminal state.
- Request cancellation suppresses a late synchronous response but still runs
  process, lease, and telemetry cleanup.
- `Tool.output_schema` is populated through the exact SDK model and not through
  a host-specific extension.
- `RunningService::waiting(self)` consumes the service. Shutdown code must clone
  its cancellation token before entering a `select!`, then move the service
  into exactly one waiting branch.
- Progress is sent only when a request carries a progress token. Missing-token
  calls remain valid and do not fabricate notifications.

The handler is a protocol adapter. Workspace, process, gate, documentation, and
Rust Analyzer services do not depend on RMCP model types.
