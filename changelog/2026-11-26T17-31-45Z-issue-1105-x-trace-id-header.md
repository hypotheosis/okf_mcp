# Issue #1105: X-Trace-Id response header

## Added
- `request_telemetry` sets `X-Trace-Id` (the request span's trace id) on every response of a routed request, omitted when the span has no valid trace context.
- `http::StaticFile` response marker and `ui::static_file`: embedded static files (console and reader assets, the reader's rendered pages and worker) carry no header.
- `Access-Control-Expose-Headers: x-trace-id` on the OAuth discovery documents.
- Tests: header matches the exported span on 200s, equals the body `trace_id` on a 500, absent without a trace, absent on static files.

## Changed
- docs/operations.md: replaces the "no X-Trace-Id header" text.

## Notes
- Unmatched routes have no request span, so no header.
