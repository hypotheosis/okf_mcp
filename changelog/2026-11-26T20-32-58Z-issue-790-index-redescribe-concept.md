# Issue 790: index re-describes an already-listed concept

## Added
- `index` on a concept that is already listed, with a non-empty summary that
  differs from its bullet's, now sets that summary and records the existing
  `ReplayOp::SetListingSummary` (target: the concept), so publish redoes it on
  newer index text instead of conflicting like a free edit of the index.
  Tests: `two_concepts_re_described_in_one_folder_index_both_publish`,
  `a_concept_summary_whose_listing_another_changeset_removed_fails_at_publish`,
  `a_free_edit_after_index_on_a_concept_is_stale_checked` (`src/mcp/writes.rs`),
  and unit tests in `src/okf/ops/log_index.rs`.

## Changed
- The `index` description, the `summary` parameter and `docs/reference/tools.md`.
- The note `repair_indexes` writes for a concept with no description now says
  to describe it with `index` on the concept's path.
- `lost_listing` names a concept target as itself (it trimmed a trailing
  `index.md` from any name, e.g. `my-index.md`).
- The parity script's `index` of `analysis/stamps.md` passes the summary the
  fixture already has, so okf.py and the server still agree on "already lists".

## Notes
- A raw (unencoded, non-resolving) listing is unchanged: no-op with the
  existing note pointing at `repair_indexes`. `create_concept`'s
  `index_summary` never re-describes. Title and section are ignored for a
  listed concept, as before.
