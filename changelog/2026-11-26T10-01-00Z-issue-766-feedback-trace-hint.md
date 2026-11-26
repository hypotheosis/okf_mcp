# Issue 766: internal tool errors carry a trace id and a send_feedback hint

## Added
- `tool_error` (`src/mcp/mod.rs`) adds `trace_id` to an internal error's body (`Database`,
  `Migrate`, `Io`, `Config`): the tool span's OpenTelemetry trace id, only when the span has a
  valid trace context.
- With `[feedback]` set, it also adds `report`, one optional sentence naming `send_feedback` and
  the failing tool. `call_tool` scopes a `REPORT_TOOL` task-local only when `send_feedback` is
  registered, so the hint never names a tool the client lacks.
- `OkfError::is_transient`: a database error that looks like an outage (pool timeout, closed
  pool, reset connection, TLS failure, crashed pool worker, SQLSTATE class 08, 57P01 to 57P03).
- Tests for each case, including the trace id matching the exported tool span and an end-to-end
  call through the served MCP endpoint.

## Changed
- `docs/reference/tools.md` (via its generator) and `docs/operations.md` describe the two keys.

## Notes
- `trace_id` is sent without `[feedback]` too: it reveals nothing about the catalog and lets a
  user quote it in a bug report.
- `report` is left off for transient database failures (an outage hits every agent at once and
  the feedback rate limit is per credential); `trace_id` is still sent.
- `OkfError::to_json()` is unchanged. The HTTP API (`src/http/mod.rs`) has its own error path
  and is unchanged; it has no `send_feedback` tool to point at.
