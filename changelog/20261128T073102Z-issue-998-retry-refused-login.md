# Retry the acquisition once after a refused login (#998)

Generated: 2026-11-28T07:31:02Z

Follow-up from the review of #996. A store pool that needed a new physical
connection after the secret rotated or the token expired was refused its
login; the triggering request failed (`StoreUnavailable`), the credential
task refreshed in the background, and only the next request worked.

## Added

- `CatalogDb::retrying` (`src/stores/mod.rs`): runs an acquisition and, when
  it is refused its login (SQLSTATE class 28, `is_auth_failure`), tells the
  credential task, waits for its refresh count to pass the value read before
  the first attempt, and runs the acquisition once more.
- `CatalogDb::acquire` and `CatalogDb::begin_with`, classified like `begin`.
- Tests: `stores::tests` (retry once; no third attempt; timeout; ended task;
  no retry of a timeout, closed pool, I/O, worker or SQL error; eight callers
  released by one refresh; a refresh during the attempt) and
  `stores::registry::tests` (a read, `begin`, `acquire` and `begin_with`
  succeed on the first try after a rotation on an open pool; a login that
  stays refused is `StoreUnavailable` within `store_connect_timeout` after one
  fetch; concurrent reads share one refresh).

## Changed

- `StoreExecutor` takes its connection itself (as sqlx's pool executor does)
  so the acquisition can be retried: a query cannot be run twice, getting
  its connection can. `fetch_many` takes it inside the stream (new direct
  dependency `async-stream`, already in the lock through sqlx).
- `catalog::build` and the evidence read use `CatalogDb::begin_with` and
  `acquire` instead of the raw pool.
- The `Keeper` exposes its refresh count outside tests; a store with a credential
  task holds it in its handle.
- Docs: the Task 9 correction note, "Adding a store" in `docs/operations.md`.

## Notes

- The wait is a `tokio::sync::watch` on the refresh count, never a sleep or a
  poll, bounded by `store_connect_timeout` counted from the first attempt.
- Coalescing: every refused caller signals; the task drains what it has heard
  before one fetch, bumps the count once, and all waiters see it. A refresh
  that lands while the attempt is in flight counts too.
- Not retried: timeouts, closed pools, transport errors, stores with no
  credential task (`env`, the account database). A login still refused after the
  refresh is returned; there is no loop.
- The task's 10 second minimum gap between refreshes is unchanged, so a second
  rotation inside it can still fail until the next refresh.
- The log line for a retry (debug) carries the store name in the local sink only.
