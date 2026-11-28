# OAuth token span: say how a refused resource differed, keep okf.grant unscrubbed (#1011)

Generated: 2026-11-28T07:57:26Z

A client was refused with `400 invalid_target` and the trace could not say
what it had sent. On code exchanges, `okf.grant` also reached the trace
backend as `[Filtered]`.

## Added

- `okf.resource` on the `okf.oauth.token` span, and on the `authorize` span,
  when `invalid_target` comes from a `resource` that is not the server's
  (`<public_url>/mcp`). A closed set from `oauth::resource::classify`:
  `trailing_slash`, `origin`, `other_path`, `other_host`, `unparseable`,
  `other_form`. Never the sent value, in the span, the logs or the error
  body. Decisions: a different scheme or port counts as `other_host` (the
  origin differs); letter case in the host, an explicit default port, a
  query, fragment or user info are `other_form`; an empty value is
  `unparseable`.
- The refusals are unchanged: `invalid_target` is still returned for every
  one of them, including a trailing slash.

## Changed (telemetry-breaking)

- The `okf.grant` span values are now `code` and `refresh` (were
  `authorization_code` and `refresh_token`). Both old values held `auth` or
  `token`, which trace backends' default scrubbers hide. Anyone with a saved
  query, dashboard or alert on `okf.grant:authorization_code` or
  `okf.grant:refresh_token` must change it. The `grant` label of
  `okf_oauth_tokens_issued_total` is not changed. The `okf.refresh` values
  (`rotated`, `grace`, `reused`, `unknown`) hold none of the scrubbed
  substrings and are unchanged.

## Not done

- Accepting equivalent spellings (item 3 of the issue) waits on what
  `okf.resource` shows clients send.
