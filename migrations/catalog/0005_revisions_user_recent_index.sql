-- no-transaction
-- A user's recent changes in one catalog (`history::recent` with a `user`
-- filter): the catalog's revisions by one user, newest first, ordered by
-- (created_at, path, revision), all descending, as `revisions_catalog_recent`
-- serves the unfiltered page. A partial index: a revision with no person
-- needs no entry, and the filter never asks for one.
--
-- Built CONCURRENTLY, as accounts 0025 does, so publishes keep writing
-- `revisions` while it builds (a plain CREATE INDEX holds a SHARE lock for
-- the whole build). That cannot run in a transaction, hence
-- `no-transaction` above (it must be the file's first line), and the file
-- holds this one statement only. A build that failed part-way leaves an
-- INVALID index; `db::migrate_store` drops it before the migrations run, so
-- the retry builds it again (`IF NOT EXISTS` would otherwise keep it).
CREATE INDEX CONCURRENTLY IF NOT EXISTS revisions_catalog_user_recent
    ON catalog.revisions (catalog_id, user_id, created_at, path, revision)
    WHERE user_id IS NOT NULL;
