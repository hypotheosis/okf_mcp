# Issue 1203: okf.resource at authorize reaches the default trace export

The `/oauth/authorize` handler's span was `authorize` at `debug`, so the
default OTLP traces filter (`info`) dropped the `okf.resource` field that
issue 1011 added. It is now `okf.oauth.authorize` at `info`, like
`okf.oauth.token`. It carries only `okf.resource` (a closed set, never the
value), and only when the request named a `resource` that was not this
server's.

- `src/oauth/authorize.rs`: the span's name and level; the test now runs under
  `DEFAULT_OTLP_FILTER`, and a second test shows the field stays unrecorded for
  a missing or accepted `resource`.
- `src/http/telemetry_tests.rs`: `okf.resource` joins the scan's floor set.
- `docs/operations.md`: the span table and the invalid_target note.

The old `authorize` debug span no longer exists under that name; it was off by
default and nothing in the repository used it.
