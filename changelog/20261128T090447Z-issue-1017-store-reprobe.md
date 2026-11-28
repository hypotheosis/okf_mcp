# A call probes a lost store once on two more paths (#1017)

Generated: 2026-11-28T09:04:47Z

Two paths spent a second connect timeout on a store the same call had
already found down, so the call could run past the request deadline.

## Fixed

- `list_catalogs` with a key granted several catalogs and no default: the
  selection fallback read the granted catalogs with `resolve_many`, which
  probed the store the listing page had just found down. It now passes the
  call's down set (`ToolContext::down_stores`) to
  `CatalogDirectory::resolve_many_skipping`, which skips the generation read
  for those stores and still returns their account rows.
- `list_changesets` with a connection list: `open_in_every_store` re-read the
  store `check_connection` had found down. It now reports a store in the
  call's down set as down without connecting.

The result is unchanged: the same partial answer and the same `unavailable`
entries. The down set is per call (`ToolContext`), never kept across
requests. `CatalogDirectory::resolve_many` (the credential cache's caller)
is unchanged and takes no down set.

## Tests

- `list_catalogs_with_two_granted_catalogs_probes_a_lost_store_once`
- `list_changesets_with_a_connection_list_probes_a_lost_store_once`

Both counted two probes before the fix and one after.
