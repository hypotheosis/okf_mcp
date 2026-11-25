# Issue 988: a configurable Billing link in the console

## Added
- `billing_url` setting (config `[auth] billing_url`, `okf-mcp settings`, the
  Settings page): an optional `https://` URL, off by default, checked and
  stored exactly as `docs_url` is (`none` clears it, reset returns to the
  config default).
- `billing_url` in `Brand` and `GET /ui/brand.json` (`null` when unset).
- A "Billing" link under "Your account" in the console sidebar, shown to people
  signed in with an account (admin or not), never for an API key alone. It opens
  in a new tab with `rel="noopener noreferrer"`; the URL is set as a DOM
  property. Saving, disabling or resetting the setting redraws the shell.
- Tests: config and settings validation, update, `none` and reset; brand
  serialization; browser journeys for member, key and signed-out visibility,
  link attributes, and the shell following a save and a disable.

## Changed
- The router no longer hides a `.nav-link` without a `data-nav` (an external
  link); `drawHelp` shares one `setLink` helper across its links.
- `brand_in` reads the `billing_url` row too, so a queued mail's brand matches.

## Notes
- The destination is a reusable portal login, not a customer-specific session
  URL; it is public. Choosing the production destination is a deployment step
  outside this repository.
