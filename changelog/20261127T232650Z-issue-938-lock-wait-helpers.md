# Issue 938: the remaining lock-wait test helpers read pg_locks, not query text

#639 (PR #937) replaced two test waits that matched `pg_stat_activity.query`, whose text can be a
stale snapshot while `wait_event_type` is live. Five more waits did the same and could pass without
the code under test reaching the lock the test means. They now use `src/test_locks.rs`, which reads
`pg_locks` and `pg_blocking_pids` and matches the lock's table and the holder's pid, each bounded to
10 s (inside the callers' 30 s deadlines).

| Test | Old match | Now | Lock |
|---|---|---|---|
| `idle_tests::a_grant_used_or_revoked_while_the_pass_waits_is_left_alone` | `%FROM api_keys WHERE id = $1 FOR UPDATE%` | `expect_row_lock_waiter(holder, "api_keys")` | row, `FOR UPDATE` |
| `idle_tests::a_refresh_admitted_before_expiry_is_never_undone_by_the_idle_pass` | same, or the pass finished | `expect_row_lock_waiter(rotation, "api_keys")` | row, `FOR UPDATE` behind the rotation's `FOR SHARE` |
| `idle_tests::a_rotation_after_a_revocation_is_refused` | `%FOR SHARE%` | `expect_row_lock_waiter(revocation, "api_keys")` | row, `FOR SHARE` behind `FOR UPDATE` |
| `catalogs_tests::a_load_overtaken_by_a_catalog_message_is_served_but_not_kept` | `%documents%` | `wait_waiter(hold)`, asserts a `relation` lock | table, `ACCESS EXCLUSIVE` on `catalog.documents` |
| `oauth::codes::a_racing_double_redeem_yields_one_grant` (not in the issue) | `%FROM sign_in_codes%FOR UPDATE%` | `wait_row_lock_waiters(blocker, "sign_in_codes", 2)` | row, `FOR UPDATE` |

- The second idle test used to accept "the pass finished" as well; it now requires the pass to block
  on the rotation's row, which is the race it tests.
- `test_locks::wait_row_lock_waiters` is new (test-only): it waits for `count` sessions on one row.
  The second session after a row queues for the first one's `tuple` lock rather than the holder's
  transaction, so it is counted only when `pg_blocking_pids` names a session that waits for the
  holder itself; a queue on another row of the same table, behind another holder, is not (review of
  PR #1184, regression test `a_queue_on_another_row_of_the_table_is_not_counted_for_the_holder`).
  `wait_row_lock_waiter` is `wait_row_lock_waiters` with a count of one.
- The `query LIKE 'LISTEN%'` checks (`invalidate.rs`, `invalidate/store_feed_tests.rs`,
  `mail/queue.rs`) find idle LISTEN connections, not lock waits, and are unchanged.
- No production code changed.
