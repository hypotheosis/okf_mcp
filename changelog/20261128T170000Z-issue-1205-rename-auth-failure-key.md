# Rename okf.auth.failure to okf.refusal (#1205)

- The HTTP request span's refusal reason is now `okf.refusal`. Its values
  (`missing`, `scheme`, `unknown`, `expired`, `revoked`, `wrong_resource`) are
  unchanged. A key holding `auth` is scrubbed by trace backends (Sentry's
  default list), as #884 found for `okf.oauth.grant`, so the old name showed
  as `[Filtered]`. The key only existed since #1012.
- `docs/operations.md`: the span table, "Why a credential was refused" and the
  401 troubleshooting row use the new key.
- Test `no_okf_span_key_holds_a_substring_a_scrubber_hides` scans `src` for
  every `okf.*` span attribute key and checks it against Sentry's documented
  default scrub list plus `cookie`, `session` and `csrf`.
- Not part of this change: confirming against a real Sentry event after
  deploy (issue step 4).
