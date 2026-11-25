# Issue 955 review: a readable listing probes a stalled store once

`CatalogDirectory::readable_page` reads the reach list one scan page at a time.
Each scan page probed every store it listed (the generations query, bounded by
`store_connect_timeout`), so a stalled store cost one bound per scan page, for
example on console boot (`/whoami?readable=true`) with many locked catalogs.

- The scan now keeps the stores it found unreachable and skips them in later
  scan pages (`offered_page_skipping`, `resolve_many_skipping` and
  `fill_generations` take the request's known-down set). A stalled store is
  waited on once per request; its catalogs are still named in `unavailable`.
  `offered_page` and `resolve_many` keep their signatures and behaviour.
- Test: `a_readable_listing_over_several_scan_pages_spends_one_load_bound_on_a_stalled_store`
  locks `catalog_state` on a second store and reads four one-catalog scan pages;
  it fails (1.2 s against a 300 ms bound) when the set is not carried through.
