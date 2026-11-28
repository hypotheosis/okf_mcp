# Issue 949: history pages past the newest 200 revisions

`history` returned at most the newest 200 revisions of a document and had no
way to ask for older ones. It now pages by revision number.

## Added

- `history` takes `before` (a revision number): only revisions below it
  (strict), newest first. Its result gains `next`: the page's last revision
  number while an older matching revision exists, else null. Pass `next` as
  `before` for the next page. A `before` at or below the oldest revision is an
  empty page with `next: null`, not "not found". `recent_changes` pages with
  an opaque `after`/`next_after` cursor over a time-ordered feed; a revision
  number is the natural cursor here, so `history` names its own pair.
- `GET /api/v1/catalogs/{catalog}/history/{path}` accepts `?before=` and
  answers `next`.
- The console's History tab has a Load older button that follows `next`,
  appends the rows, and goes away when `next` is null. It fetches one page per
  click (a second click while a read is in flight does nothing), and an answer
  that arrives after the tab was left draws nothing.
- `store::history::revisions` and `revisions_by` take `before: Option<i32>`;
  the query adds `revision < COALESCE($6, 2147483647)` and still reads the
  primary key `(catalog_id, path, revision)` backwards without a sort (a test
  `EXPLAIN`s it).

## Changed

- `history` reads one row more than `limit` to decide `next`, so exactly
  `limit` revisions left give `next: null`, never a pointer to an empty page.
- With a `before` or a filter, a page with no rows is still an empty list when
  the path has any revision; only a path with none is "not found".
- `docs/reference/tools.md#history`, the browse API row in
  `docs/operations.md` and `src/investigator/contract_tools.json` were
  regenerated or updated. The contract recapture also refreshed other tools'
  descriptions that had drifted from the openings of #1174.

## Notes

- Tests: `src/mcp/history_paging_tests.rs` (261 revisions listed to revision
  1, the exact boundary, the strict bound, the empty page past the oldest, a
  filtered walk, the `EXPLAIN`), a browse API round trip, a ui_e2e journey and
  an MCPJam scenario.
- The tool description's first 31 characters are unchanged (#1174).
