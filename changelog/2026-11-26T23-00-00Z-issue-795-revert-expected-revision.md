# Issue 795: `revert` takes an expected revision

`revert` gains an optional `expected_revision`: the revision the caller saw
published when it read `history`. If the path's published revision differs when
the revert runs, it is refused with `Conflict` (the existing shape: `base_revision`
is the value passed, `current_revision` the revision now) and nothing is
written to the changeset. Without it, behaviour is unchanged.

`history` returns `current_revision` (the live revision, null for a deleted
path, independent of `limit`, `user` and `since`) to pass back.

The check runs in `write::apply_audited` against the snapshot the entry's base
comes from (new `OpOutcome::expected`), so no publish can slip between check
and base. A deleted path has no live revision, so the check cannot be used to
guard a revert onto a deleted path; omit it there.
