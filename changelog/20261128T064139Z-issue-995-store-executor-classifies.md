# Store reads classify an unreachable store by construction (#995)

Generated: 2026-11-28T06:41:39Z

Follow-up from the review of #994. `CatalogDb::begin` mapped a failure to
get a connection to `StoreUnavailable`, but a query run through
`CatalogDb::executor()` returned the driver's error and `?` turned it into
`OkfError::Database`: an exhausted or lost store answered HTTP 500 and an
internal MCP error instead of `StoreUnavailable`. The history tool and route
did so once the catalog's state was cached.

- **Design.** `CatalogDb::executor()` now returns `stores::StoreExecutor`,
  a `Copy` wrapper that implements `sqlx::Executor` over the store's pool
  (`src/stores/executor.rs`). A driver error that
  `classify_store_error` reads as the store being unreachable (`PoolTimedOut`,
  `PoolClosed`, `Io`, `Tls`, `WorkerCrashed`, a refused login) leaves it as
  `sqlx::Error::Configuration` holding the `OkfError::StoreUnavailable`, and
  `From<sqlx::Error> for OkfError` gives that back. Every other error, a SQL
  error, a constraint violation, `RowNotFound`, is untouched, so `Database`,
  `Busy` and the unique-violation checks behave as before. Mid-query `Io` and
  `Tls` count as unreachable, as they do in `CatalogDb::classify`.
- **Why a wrapper.** Store-layer functions take `impl PgExecutor`, and about
  thirty non-test call sites pass `CatalogDb::executor()`. The wrapper covers
  every one, and every future one, with no change at the call sites. Mapping
  at the tool and route boundaries instead would need a change in each of
  the reads and would leave a new one unclassified. The account pool is
  untouched (`AccountDb::executor()` still returns `&PgPool`).
- **Raw pool.** What an executor cannot do (`begin`, `acquire`, `close`,
  `options`) moved to `CatalogDb::pool()`; its callers outside tests are the
  registry, which has its own handling, and two reads that now classify with
  `CatalogDb::classify` (`catalog::build`'s transaction and the evidence read).
- **No new noise.** The error is logged once, as before, when it is made.
- Tests: `stores::executor::tests` (every query kind on a closed pool is
  `StoreUnavailable`; a SQL error and `RowNotFound` stay `Database`; the
  account pool is not classified) and, in `mcp::unavailable_tests`, the history
  tool and the history route on a store that goes away after the state loaded.
  The last two failed before the change with `Database { PoolTimedOut }`.
- Docs: `docs/operations.md` no longer lists the gap.
