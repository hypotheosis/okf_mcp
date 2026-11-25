# Issue 955: navigation lists hide catalogs an admin may not read yet

With `require_unlock` on, an admin's reader picker and console catalog switcher
listed every catalog the key reaches, including other teams' catalogs that need
an unlock before their contents can be read.

- `GET /api/v1/catalogs` and `GET /api/v1/whoami` take `?readable=true`. The
  listing is the usual reach listing followed by `Principal::admission` for a
  read, decided on the server clock. The check is pure, so listing records no
  read. `CatalogDirectory::readable_page` scans the reach list forward until it
  has `limit + 1` readable rows, so pages stay full and `next` is exact even
  across a run of locked catalogs.
- The plain listings are unchanged (additive flag). The console's Catalogs page
  and the Unlock form's picker keep using them, so a locked catalog can still
  be found and unlocked; `whoami`'s admin fields do not depend on the flag.
- The reader (`/read/` picker and known-catalog list) and the console switcher
  (`refreshIdentity`) ask for `readable=true`. A successful unlock and an
  ended unlock refresh the identity, so the switcher follows at once and an
  ineligible selection is replaced.
- With `require_unlock` off the readable listing equals the plain one.
- Hiding is not the boundary: content routes still enforce the unlock.
- Tests: directory paging over locked pages, expiry by the server clock, and
  `require_unlock` off; HTTP tests for locked, unlocked, expired, ended and
  normally readable catalogs, paging, and scoped keys; two browser journeys
  (switcher and reader picker) that also use the admin unlock discovery.
- Docs: `docs/operations.md`, "Catalog listings are paginated".
