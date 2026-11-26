# cited_by lists the cited sources when given no filter (issue #1114)

## Added
- `cited_by` with neither `source_id` nor `resource` returns `sources`: each cited source
  (grouped by id, case-insensitively, and resource) with `cited_by`, the number of concepts
  citing it. Ordered by count descending, then id, then resource; capped by `limit` with
  `truncated`, as `hits` is. `SourceTally` in `okf::search` counts in one pass.
- A mcpjam scenario step for the unfiltered call.

## Changed
- `{}` no longer fails with `Refused`; `hits` is empty and `sources` is set. A filtered call
  is unchanged and has no `sources` key. An empty-string `source_id` or `resource` (with no
  other non-empty one) is still `Refused`: it is a malformed filter, and widening it to the
  listing would hide a client bug.
- The tool description gains one clause; `docs/reference/tools.md` regenerated.

## Notes
- The tool's error rate no longer counts the MCPJam harness's argument-free probe as a failure.
