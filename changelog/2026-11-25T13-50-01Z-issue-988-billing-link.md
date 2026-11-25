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
- The destination is the start page of an external billing service, which should
  sign the person in through this server's OAuth (as a registered client) and
  send them to their own billing. It is never a customer-specific session URL or
  a secret: it is public in `/ui/brand.json`, and no credentials or personal
  information are passed in it. This server holds no billing-provider code.
  Choosing the deployment's destination is outside this repository.
