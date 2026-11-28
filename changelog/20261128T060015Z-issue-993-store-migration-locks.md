# Bound migration lock waits; index the store columns (#993)

Generated: 2026-11-28T06:00:15Z

Follow-up from the review of #992 (catalog stores, epic #958).

- **Lock waits.** No migration set `lock_timeout`, so an `ALTER` could wait
  behind a long transaction while every query on the table queued behind it.
  Migrations 0036-0043 have run on real databases, and sqlx refuses a changed
  file (checksum), so none could be edited. Instead the migrator sets it:
  `db::migrate` and `db::migrate_store` set the connection's `lock_timeout` to
  `MIGRATION_LOCK_TIMEOUT` (10 s) after taking the migration lock and put the
  previous value back before the connection returns to its pool. It is a
  session setting, so `no-transaction` migrations (`CREATE INDEX
  CONCURRENTLY`), which run on the same connection, are bound too, and so is
  every future migration, account or catalog. A migration cut off by it fails
  with `OkfError::Busy` ("a migration waited more than N ms ... start it again")
  and the Postgres report goes to the log. A retry is safe (see
  `docs/operations.md`, Postgres). The value is a constant: `migrate` and
  `migrate_store` are called from about twenty places with no config in reach, so a setting is a
  follow-up.
- **Indexes.** `0044_teams_store_index.sql` and
  `0045_catalogs_store_index.sql` build `teams_store` and `catalogs_store`
  (`CREATE INDEX CONCURRENTLY IF NOT EXISTS`, one `no-transaction` file each,
  as 0025-0027) on exactly the foreign keys' columns, and both are listed in
  `CONCURRENT_INDEXES` so a failed build is dropped and redone. The need is
  real now: `store::stores::summaries` counts `teams WHERE store = ...` for
  every store listed, the `ON DELETE RESTRICT` check reads both tables when a
  store is removed, and housekeeping filters catalogs by store.
- Tests (`src/db.rs`): a migration blocked by another transaction's
  `ACCESS EXCLUSIVE` lock fails at a 500 ms bound with `Busy`, the pooled
  connection keeps no bound, and the retry builds a valid index; EXPLAIN of the
  foreign-key lookup uses each new index.
