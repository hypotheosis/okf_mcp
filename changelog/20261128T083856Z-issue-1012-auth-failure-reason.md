# Record why a bearer credential was refused (okf.auth.failure) (#1012)

Generated: 2026-11-28T08:38:56Z

Production showed `DELETE /mcp` answering 401 and nothing said whether the
client sent no credential or an access token that had expired.

## Added

- `okf.auth.failure` on the HTTP request span for every 401 from
  `require_key` (MCP and archive routes) and `require_principal`
  (`/api/v1`). A closed set, `AuthFailure`: `missing`, `scheme`, `unknown`,
  `expired`, `revoked`, `wrong_resource`. The request span is an `info` span,
  so it is exported under the default OTLP filter (`info`); a test runs the
  cases at `info` and at `trace`.
- Mapping: no header, or an empty `Bearer` token, or no session cookie is
  `missing`; another scheme is `scheme`; an access token past its expiry
  `expired`; one revoked, or whose grant is not live, `revoked`; one for
  another resource `wrong_resource`; every other unmatched token, a revoked
  or expired API key and a dead console session cookie `unknown`.
- Never recorded: the token, a prefix, its hash, the key id, the cookie. A
  test sends marker tokens and checks no span, log, stderr or body holds them.
- The 401 is unchanged for every reason (a test compares status, headers and
  body across all of them): the verifiers now return `Result<Arc<Principal>,
  AuthFailure>` inside, and the response is still `unauthorized()`.

## Changed

- `tokens::find_access` reads the access token row by hash whatever its
  state and returns `AccessState` (`Live`, `Revoked`, `Expired`; `None` for
  unknown). It is the same single primary-key read as before, with the
  revoked and expiry tests moved from the `WHERE` into Rust, so no refusal
  adds or drops a query and the cache behaviour is unchanged (only live
  tokens are cached).

## Decisions and not done

- An API key that is revoked or expired is `unknown`: the credential
  cache's single-flight load shares only "found or not", and carrying the
  reason would need a wider change.
- A suspended user or team needs no value: suspension is a 403 at the
  catalog, never a 401 in these layers.
- No metric label: there is no 401 counter (only the request duration
  histogram by status).
