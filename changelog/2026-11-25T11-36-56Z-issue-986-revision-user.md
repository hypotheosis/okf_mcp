# Issue 986: record the person behind each revision, and filter history by person and time

Every Claude Code user's edits showed the same actor (the OAuth client's name),
and `actor_label` was null for it, so nothing said which person made a change.

- **Catalog migrations 0003 to 0005.** 0003 adds the nullable
  `revisions.user_id` (metadata-only, a brief ACCESS EXCLUSIVE lock, alone in
  its file). 0004 backfills it through changeset, key and `api_keys.user_id`
  with one UPDATE that takes row locks only; it runs only where
  `public.api_keys` exists (the account database's `main` store). 0005 builds
  the partial index `revisions_catalog_user_recent` `CONCURRENTLY`. There is no
  foreign key: deleting a user leaves the column (decision of 2026-11-23).
  `db::drop_invalid_indexes` now takes a schema and names, and
  `migrate_store_locked` drops an invalid catalog index before the migrator
  runs, as `migrate` already did for the account ones.
- **Recorded at publish.** `publish` records `AuditActor::user_id` (the
  publisher's user) on every revision; `actor` is unchanged. An import,
  `repair-indexes` and starter files record none.
- **Showing it.** `history`, the new `recent_changes` tool and the HTTP history
  and recent-changes routes give `user` (the user's name; the stored id for a
  deleted user) and `user_label`. `list_changesets` and `pending_changes` give
  the opener's `user` and `user_label`, read from the key now. The `actor_label`
  descriptions now say it labels a user actor.
- **Filters.** `history` and `recent_changes` take `user` and `since`. Recent
  changes is a separate tool (not `history` with an optional `path`): it needs
  a cursor and a catalog, returns a path per row, and the store function and HTTP
  route already existed. `store::history::recent_by` has four statements so
  that a `user` page uses the partial index and no page sorts.
- **Stored id, shown name.** `revisions.user_id` holds `users.id` (a uuid),
  which a name reused after a deletion cannot take over; readers resolve it to
  the name on each page with one primary-key query.
- Tests: publish records the grant or key's user; reassigning the key and
  deleting the user keep the record; filters and paging; the migration's
  backfill; the failed-build cleanup of the new index; planner checks for the
  new statements.
- Docs: `docs/operations.md` ("Who made a revision"), `docs/reference/tools.md`.
- **Migration lock polling.** The advisory lock around `migrate` and
  `migrate_store` is now taken by polling `pg_try_advisory_lock`. A replica
  blocked in `pg_advisory_lock` is an open transaction, and the holder's
  `CREATE INDEX CONCURRENTLY` (0005) waits for it, so the two deadlocked and
  the build was aborted when several starts met a store that needed 0005.
