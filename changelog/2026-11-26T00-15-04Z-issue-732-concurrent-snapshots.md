# Issue 732: listings load their page's snapshots concurrently

`GET /api/v1/catalogs`, the `list_catalogs` MCP tool and the unnamed
console search (`every_reachable`) loaded the snapshots of a page's
catalogs one after another, so a cold first page of 100 catalogs cost about
87 ms at a million catalogs.

## Changed

- `ToolContext::listed_states` loads a page's rows through
  `ToolContext::listed_state`, at most `LISTING_LOAD_CONCURRENCY` (8) at a
  time, with `futures_util`'s `buffered`, so results keep the page's order.
  The first row of each store loads in a first wave and the rest in a second,
  so a store that is lost is probed once (its first row marks it down), as
  before. `CatalogCache::get` is untouched: single-flight and broken-generation
  handling are as before.
- The three callers filter out rows they do not load (a store the page found
  down, a closed catalog) first, then handle each result exactly as the old
  loop did. One difference: a non-store error from a load now surfaces after
  the page's other loads were started, not before; the first error in page
  order is still the one returned.
- `tests/scale.rs`: the cold first-page bound is back to the plan's 50 ms
  (was 200 ms).

## Tests

Unit tests of the ordering helper: order kept when loads finish out of
order, the bound respected (a counting loader), errors returned in place,
and a zero limit treated as one.
