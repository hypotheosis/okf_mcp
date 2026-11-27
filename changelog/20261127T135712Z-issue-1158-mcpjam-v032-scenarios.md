# Issue #1158: mcpjam scenarios pin the v0.32.0 tool behaviour

## Added
- `tests/mcpjam/scenarios/skeptical-recall-prober.test.ts`, "Test source attribution accuracy": `cited_by` by `resource` (case-sensitive, exact), alone and with `source_id` (both must match one entry); the unfiltered listing with a source cited by two concepts and twice in one (`cited_by: 2`, `hits` empty) and its order (count descending, then id, then resource); `limit` cutting the listing, and (a separate test) the hits of a filtered call, with `truncated: true`.
- Same file, "Provoke and inspect edit conflicts": `history`'s `current_revision` and `latest_revision` for a live document; `revert` with `expected_revision` succeeding; a stale `expected_revision` refused with `Conflict` naming the current revision and leaving the changeset empty; a deleted document (`current_revision` null, `latest_revision` the delete's revision), restorable with that revision until a re-create makes the same call a `Conflict`.
- `tests/mcpjam/scenarios/careful-fact-curator.test.ts`, "Organize facts into folders": `index` re-describing a listed concept (only that bullet changes), the unchanged summary being a no-op ("already lists"), and the re-describe replaying at publish after another changeset published the same `index.md`.

## Notes
- Every id, resource and path carries the run's folder. The two unfiltered `cited_by` listing checks (counts and order, cut) need a catalog of the scenario's own, since the listing is capped at 200 sources catalog-wide: they run under catalog isolation (local mode, or an admin key) and are skipped under folder isolation. The resource-filter check and the filtered `limit` checks hold in a shared catalog. README, "Isolation", says so.
- Reads of published state after a `publish` go through `vi.waitFor` (README, "Reads after a publish"); the body read before the revert's publish goes through the changeset.
- No product code changed. The staging run is left to the owner.
