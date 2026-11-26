# Issue 757: one team-level invalidation for a suspension

`teams::suspend` and `unsuspend` used to read every catalog the team owns and
send one `catalog_meta` NOTIFY each, under the team lock. They now send two
messages whatever the team owns: `team` and the new `team_catalogs { team_id }`.

- Each replica drops every cached directory row whose owner team id matches,
  with one `invalidate_where`, which also fences loads in flight. The writing
  replica's `apply_own_writes` takes the same path. Credential and label
  caches hold no suspension state, so `team_catalogs` leaves them alone
  (the `team` message beside it drops the team's users and label).
- Rolling upgrade: a previous-release replica fails to parse the unknown kind,
  counts it unreadable and runs `apply_reconnect`, which marks every cache
  stale. It therefore refuses the suspended team on its next request.
  Nothing needs a two-release switch.
- Tests: two notifications for a team owning 5,000 catalogs (counted),
  directory drop by owner, lift reopens access, the unreadable-kind path,
  and the multi-replica suspend and lift.
