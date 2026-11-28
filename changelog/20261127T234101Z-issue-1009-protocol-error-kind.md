# Issue 1009: protocol errors carry an error kind, and every failed tool span sets `error.type`

A tool call that failed before the handler ran (arguments that don't parse, an unknown tool) left
`okf.error.kind` empty on its `mcp <tool>` span, and no span set the OpenTelemetry attribute
`error.type`, which Sentry and other backends show as the error type.

- `call_tool` declares `error.type` and, for a failed call that never reached `tool_error`, records
  a kind on both `okf.error.kind` and `error.type`: `InvalidParams` (`INVALID_PARAMS`),
  `UnknownTool` (`METHOD_NOT_FOUND`), `Internal` (`INTERNAL_ERROR`), else `Protocol`
  (`kind_of_code`). `tool_error` records `error.type` beside `okf.error.kind`.
- Divergence from the issue, from rmcp 3.8.0's source: an unknown tool is `INVALID_PARAMS`
  ("tool not found"), not `METHOD_NOT_FOUND`, so it is `InvalidParams`. Arguments that don't
  deserialize come back as an `Ok` `isError` result (text "failed to deserialize parameters:"),
  not an `Err`; `protocol_error_kind` recognises that text and records `InvalidParams`.
- The error message is never recorded: serde's messages echo argument values.
- Metrics are unchanged: `tool_call` already counted these calls as errors.
- Other spans: no other span sets `okf.error.kind`.
