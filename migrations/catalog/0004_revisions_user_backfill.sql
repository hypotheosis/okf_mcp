-- Backfills `revisions.user_id` (0003) for the revisions already published:
-- revision -> changeset_id -> changesets.key_id -> api_keys.user_id, where
-- each link still resolves. A revision with no changeset (an import), a
-- changeset whose key is gone (its user's deletion deletes the key), or a
-- key no user owns keeps a null. A console session's changeset names no key
-- row (`session:<name>`), so it keeps a null too.
--
-- `api_keys` is an account table. Only the account database holds it, so
-- this runs only where `public.api_keys` exists (the `main` store); another
-- store's database has no account table to join, and its revisions stay null.
--
-- Lock: ROW EXCLUSIVE on `revisions` and `changesets` (what any UPDATE
-- takes), which blocks neither reads nor inserts, so publishes continue.
-- It is one statement and one transaction, so a large table makes it take
-- time and leaves dead row versions for autovacuum; it holds no lock that
-- waits for a reader or a writer. It sits in a file of its own, apart from
-- the ADD COLUMN (0003), so that file's ACCESS EXCLUSIVE lock is not held
-- while this runs. A rerun changes nothing (`user_id IS NULL`).
DO $backfill$
BEGIN
    IF to_regclass('public.api_keys') IS NOT NULL THEN
        UPDATE catalog.revisions r
           SET user_id = k.user_id
          FROM catalog.changesets c
          JOIN public.api_keys k ON k.id = c.key_id
         WHERE r.changeset_id = c.id
           AND r.user_id IS NULL
           AND k.user_id IS NOT NULL;
    END IF;
END
$backfill$;
