# Issue 1127: guard a revert onto a deleted path

`history` gains `latest_revision`: the path's newest revision number, a
`delete` revision included. `current_revision` keeps its meaning (the live
revision, null for a deleted path).

`revert`'s `expected_revision` is now compared against the path's latest
revision. For a live path that is the live revision, as before. For a deleted
path the caller passes `latest_revision` (the deletion's number); a re-create
or any publish in between is refused with `Conflict` and nothing is written.

The comparison is made in `write::apply_audited`, inside the write's own
transaction, after `lock_catalog` and the changeset row lock (the same lock
order as the live-path guard): the live revision comes from the snapshot the
entry's base comes from, and with no live document the newest stored revision
is read in that transaction (`store::history::latest_revision`).

Docs: `docs/reference/tools.md` and the tool descriptions for `history` and
`revert`; the investigator contract fixture is regenerated.
