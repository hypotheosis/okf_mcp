# Issue 943: My keys keeps the newest read's account snapshot

The My keys list read destructured `/me` straight into the page-level
`account` before the `latest()` guard ran, so an older read that answered
late still replaced the account the New key form uses (expiry options,
ceiling checks).

- `src/http/ui/js/pages/my-keys.js`: `load` reads into a local `fresh` and
  assigns `account` only after `if (!current()) return;`.
- `tests/ui_e2e/self_service.rs`: the held-answer journey for My keys now
  changes `max_key_lifetime` between the older and newer reads, releases the
  older answer, and asserts the New key form offers the newer account's
  expiry options (7 days, 30 days the longest) and not the older one's.
- The other list pages that use `latest()` (My grants, catalogs,
  changesets, teams, users, runs, settings, account) assign page state only
  after their guard; none needed a change.
