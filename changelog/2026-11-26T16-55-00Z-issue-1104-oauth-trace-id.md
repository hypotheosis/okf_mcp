# Issue #1104: OAuth 500s carry a trace id

## Added
- `http::request_trace_id()`: the request span's trace id (from the `REQUEST_SPAN` task-local), or `None` without a valid trace context. `AppError` and the OAuth answers share it; a response header can reuse it (#1105).
- An OAuth `server_error` answered with 500 carries `trace_id` in its JSON body; the authorize and consent error pages show it on a 500.
- Tests: OAuth token 500, authorize page 500, no-trace omission, and the auth middleware's failed credential lookup.

## Changed
- `consent::error_page_html` takes the optional trace id; `consent::error_page` is now instrumented.

## Notes
- The auth middleware (`require_key`) already answered through `AppError`, so its credential-lookup 500 carried the id since #1093; a test now pins that.
- 4xx and the busy 503 carry no id. Details stay server-side.
