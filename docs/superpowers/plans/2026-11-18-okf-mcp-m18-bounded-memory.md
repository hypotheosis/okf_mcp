# OKF MCP Server, Milestone 18 (bounded memory): Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking. In this project each task is one GitHub sub-issue and one PR (`github-issue-to-pr`).

> **Builds on `main` at `2d4ca10`** (PR #643, after v0.22.0).

**Goal:** A replica's memory, startup time and background database load stop growing with the total number of users and catalogs (target: about 1M of each, a small fraction active). Catalogs, credentials, users and labels load on demand into bounded caches; changes reach every replica through `pg_notify` sent by the transaction that makes them; listing is a query; housekeeping runs once per cluster as apalis jobs; the poll loop and per-catalog metric series go.

**Architecture:**
- **`src/invalidate.rs`** owns the change feed: the `Invalidation` enum, `invalidate(conn, &[Invalidation])` (bumps `admin_generation` and runs `pg_notify('okf_invalidate', ..)` in the caller's transaction), and the listener task that applies each message to the caches. It replaces the ~37 `bump_admin_generation(conn)` calls.
- **`src/cache/`** holds the caches, all built on one wrapper, `cache::Bounded<K, V>` (moka `future::Cache` plus per-key invalidation epochs, stale marking and the `okf.cache.*` metrics):
  - `cache::catalogs::CatalogCache`: snapshots by catalog id, weighed in bytes;
  - `cache::directory::CatalogDirectory`: catalog rows by name, with access facts;
  - `cache::credentials`: credential, user and label caches, and principal assembly.
- **`src/housekeeping.rs`** runs the pruning and the cluster gauges as one apalis job type, scheduled by slot-keyed idempotency keys.
- **`App`** holds the caches instead of `catalogs`, `keys`, `polling`, `broken`, `open_changesets`, `oauth_pruned` and `audit_pruned`.

**Tech Stack:** Rust 2021 with the existing axum 0.8 / sqlx 0.9 / rmcp 3.8 / tokio / apalis 1.1 stack. New dependency: `moka = { version = "0.12.15", features = ["future"] }`. sqlx's `PgListener` is already available through `sqlx` (feature `postgres`).

**Spec:** `docs/superpowers/specs/2026-11-18-okf-bounded-memory-design.md` (Decisions 1-20; Decisions 10, 12-15 corrected during planning).

---

## Conventions for every task

- **Branches and PRs.** Branch per task from `main`, one PR per task, linked to its sub-issue. Follow `AGENTS.md` (review rounds, keeping branches current, `tools/agentic/`) and `tools/agentic/brief-common.md`.
- **TDD.** Write the failing test, run it, implement, run it, commit.
- **Gates** run only through `tools/agentic/gate.sh`: `fmt`, `lint`, `test` and `dylint` before every push. While iterating, use `gate.sh narrow <cargo test args>` (one test-name filter per run).
- **Code style.** Comments explain Rust-specific constructs for a Python-expert, Rust-novice reader, as in the existing code.
  - Every public item gets a doc comment with `# Arguments`, `# Returns` and `# Errors`.
  - No `.unwrap()` outside tests.
  - Errors are existing `OkfError` variants: `Database` (503) for a failed load, `Config` at start-up, `BadRequest`-class variants already used by the API for the fan-out cap.
  - Every new function has `#[tracing::instrument(.., skip_all)]` or a reason-bearing `uninstrumented_fn` allow (the `hyp_tracing` lint is at deny).
- **Privacy rules** (review checklist items for every task):
  - an invalidation payload carries ids and generations only: never a key hash, a name of a person, an email, a label or catalog content;
  - never log a credential, a key hash or a session id; errors go through `split_event!` with the full text in the local-only half;
  - metric labels never carry a catalog, user or key: only `cache`, `kind`, `task`, `result`, `cause`, `outcome`.
- **Behaviour is preserved unless the spec says otherwise.** Existing tests keep passing, apart from `.await`s, constructor arguments and the response-shape changes of Decision 7 (Task 10). Where a test asserted poll-loop behaviour, it is rewritten to assert the same outcome through `NOTIFY` and says so in its comment.
- **Two-replica tests** use `TestServer::start_shared` / `sibling` (`tests/support/server.rs:189`, `:209`). From Task 2 on, the harness sets `catalog_ttl` and `credential_ttl` to one hour (until Task 16 removes it, also `poll_interval`), so a pass proves `NOTIFY` carried the change. Every wait is bounded with `tokio::time::timeout` (`lessons.md`).
- **Changelog.** One entry per task, `changelog/<UTC ts>-m18-task-<NN>-<slug>.md`, with `## Added`, `## Changed` and `## Notes`. Take the timestamp from `date -u +%Y-%m-%dT%H-%M-%SZ`. Task 18 also writes the milestone changelog.
- **The code in this plan has not been compiled.** If an implementation has to differ, fix this plan in the same PR (inline, or with a dated `> **Corrected (issue #N).**` note), then grep later tasks for the changed symbol.
- **Placeholders only** in tests and docs: `example.com`, `db.example.internal`, `alice`. No real hostnames, registries, names or keys.
- **Commit messages** end with the attribution line the coordinator gives.

## Verified facts the code relies on

### apalis 1.1.1 / apalis-postgres 1.1.0 (as locked)

- `apalis_postgres::queries::push_tasks(conn: &mut E, queue: &str, Vec<PgTask>)` inserts with no `ON CONFLICT`. Migration `20260508093314` adds `UNIQUE INDEX idx_jobs_idempotency_key ON apalis.jobs(job_type, idempotency_key)`: a duplicate key is a unique violation (SQLSTATE `23505`) and aborts the caller's transaction, so push each housekeeping job in its own statement on the pool, not inside a larger transaction. NULL keys never collide.
- `TaskBuilder::idempotency_key(S)`, `run_at_timestamp(u64)`, `max_attempts(usize)`, `build()`.
- One worker serves one `PostgresStorage<Args>` and one queue: housekeeping gets its own worker. `PostgresStorageFactory` shares one LISTEN connection but `unwrap`s on connect, so it is not used.
- `apalis_postgres::Config::default()` sets `missed_heartbeats = 10` (the doc comment says 2; the code says 10). With `HEARTBEAT_INTERVAL = 30s` a dead worker's jobs return after 300 s today.
- `Config::heartbeat_interval(Duration)`, `missed_heartbeats(usize)`, `queue(..)`.
- `Worker::run_until(signal)` stops fetching on the signal and waits for in-flight tasks; fetched-but-unstarted tasks are re-enqueued (`attempts + 1`).
- `apalis.workers`: `id`, `worker_type`, `storage_name`, `layers`, `last_seen`, `started_at`. `apalis.jobs.lock_by` references `apalis.workers(id)`.
- Status values: `Pending`, `Queued`, `Running`, `Done`, `Failed`, `Killed`.

### moka 0.12.15 (in the registry, not yet in the lock)

> **Corrected (issue #682, 2026-11-18).** The requirement `0.12.15` resolved to 0.12.16 in `Cargo.lock` (the newest compatible release); the API below is unchanged. Moka's clock is the real one: a TTL cannot be tested by pausing tokio's time (Task 4's test sleeps 300 ms against a 100 ms TTL). `Cache::invalidate` and `insert` are async, so `Bounded::invalidate_where` and `reweigh` are async too.

- Needs `features = ["future"]`.
- `moka::future::Cache::builder().max_capacity(u64).weigher(|&K, &V| -> u32).time_to_live(Duration).eviction_listener(|Arc<K>, V, RemovalCause|).build()`.
- `try_get_with(key, init_future) -> Result<V, Arc<E>>`: concurrent callers for one key share one init.
- `get(&k)`, `insert(k, v)`, `invalidate(&k)`, `invalidate_all()`, `iter()` (sync, `(Arc<K>, V)`), `entry_count()`, `weighted_size()`, `run_pending_tasks()`.
- `RemovalCause::{Expired, Explicit, Replaced, Size}`.
- The weigher given to moka returns `u32`.
  > **Corrected (issue #756).** `Bounded::with_weigher` now takes a `u64` byte weight. Moka gets it rounded up to units of `1 << shift` bytes, where the shift is 0 for a budget up to `u32::MAX` bytes and otherwise one that keeps the budget to at most `2^31` units (`bounded::unit_shift`); `max_capacity` is scaled by the same unit and the `okf_cache_bytes` gauge is scaled back to bytes. `too_heavy` compares bytes.

### sqlx 0.9 `PgListener`

- `PgListener::connect_with(&PgPool)`, `listen(channel)`, `try_recv() -> Result<Option<PgNotification>, Error>`.
- `try_recv` returns `Ok(None)` when the connection was lost; with `eager_reconnect` (default `true`) it has already reconnected and re-`LISTEN`ed. Notifications sent while it was disconnected are lost. `Ok(None)` is the "mark everything stale" signal (spec Decision 12).
- `PgNotification::payload() -> &str`.

### okf-mcp on `2d4ca10`

- `App::catalogs()` (`src/app.rs:626`) and `App::catalog(name)` (`:644`) are sync. `load_with_redis` loads every catalog (`:250-261`). `poll_pass` is `:699-789`.
- `ToolContext` (`src/mcp/context.rs:64`) copies the map once at construction (`:93`). `select` (`:295`), `states` (`:320`), `read_set` (`:341`), `one` (`:366`), `report_catalog` (`:656`) are sync; `state` (`:764`) `expect`s a present name; `resolve_changeset` (`:716`) scans the map by id (`:726`).
- Callers of the sync resolvers: `src/mcp/changesets.rs:129, 258`; `src/mcp/maintenance/work.rs:117, 140, 162, 186, 207`; `src/mcp/maintenance/reports.rs:145, 163, 206, 232, 261, 272`; `src/http/api/browse.rs:182, 233, 364, 417`; `src/mcp/search.rs:115`; `src/mcp/documents.rs:384`; `src/mcp/history.rs:124`; `src/mcp/reports.rs:367`.
- Iterations of the map: `src/mcp/catalogs.rs:91`, `src/mcp/changesets.rs:133`, `src/http/api/catalogs.rs:246`, `src/http/api/search.rs:91`, `src/http/api/identity.rs:171`, `src/http/api/labels.rs:122`, `src/telemetry/metrics.rs:493, 510, 525`, `src/http/mod.rs:229` (`/healthz` count), `src/mcp/mod.rs:114` (`catalog_label` existence).
- `CatalogState` (`src/catalog.rs:155`): `publishing` is locked at `src/store/publish.rs:201` and `src/mcp/changesets.rs:592`; `rebuilding` only in `refresh_to` (`src/catalog.rs:415`). Refresh callers: `src/store/publish.rs:303, 368`, `src/http/archives.rs:577`, `src/app.rs:744`.
- `KeyedLocks` (`src/keyed_lock.rs`): `lock(&self, key: i64) -> KeyedGuard<'_>`, cancel-safe, entries removed when unused.
- `Overlays` (`src/overlay.rs:257`): hand-rolled LRU by count (`overlay_cache_size`, default 64, `src/config.rs:48`) before Task 9 (issue #687); now a `Bounded` weighed in bytes.
- `catalogs.name` is `UNIQUE` (`migrations/0001_schema.sql:14`).
- `bump_admin_generation(conn)` (`src/store/catalogs.rs:1036`) has 37 non-test call sites, all inside the write's transaction; no SQL trigger bumps it. The per-function list is in Task 2.
- `KeyCache::read` (`src/auth.rs:954`) runs `key_rows::list`, `ceiling::ceilings`, `users::auth_by_id`, `teams::suspended_catalogs`, `Labels::load`, then `gates::load` (`src/store/gates.rs:37`), which first **writes** missing audit rows (`ensure_audit_rows`, `:157`). *(Task 3, issue #681: no longer; `gates::load` writes nothing.)*
- `settle(app)` (`src/http/api/mod.rs:306`) is `app.poll()`; it is called after every administrative HTTP write (41 sites) and `src/oauth/authorize.rs:1141`.
- `serve` (`src/http/mod.rs:651`) spawns the poll loop (`:657`), the M16 announce subscriber (`:660`), the clock check (`:664`) and the mail worker (`:667`, gated by `mail.worker`).
- The latest migration is `0021_oauth_tokens_grant_activity.sql`. *(Since then: `0022_feedback.sql` from main's #697, then Task 3's `0023_audit_backfill.sql`; issue #681.)*

## Decisions taken while planning (2026-11-18)

1. **Milestone 18.** Labels: `epic`, `milestone-18`. Folds in #458 and #356.
2. **Invalidations ride Postgres `NOTIFY`, not Redis** (the maintainer, 2026-11-18). Spec Decisions 10, 12-14 corrected. `invalidate(conn, ..)` replaces `bump_admin_generation(conn)` in every store function, so the CLI is covered with no extra step and nothing is sent on rollback.
3. **Housekeeping uses apalis's idempotency index** instead of a schedule table and repair sweep (spec Decision 15 corrected).
4. **Audit rows are created with their entity**, plus a backfill migration (spec Decision 20, added during planning).
5. **The first task keeps today's caches** and only changes how they learn of changes: the listener's first `apply` reloads the key cache or refreshes one catalog. Each later task swaps in a bounded cache behind the same `apply`. So every PR ships working software and the risky swaps are isolated.
6. **`ToolContext`'s resolution goes async in its own mechanical task** (Task 7) while still backed by the map, so Task 8's cache swap does not also touch 25 call sites.

## Scope

| Task | Delivers |
|---|---|
| 1 | `src/invalidate.rs`: `Invalidation`, JSON payload, `invalidate(conn, ..)`, the listener task, `apply` onto today's caches, reconnect = mark stale; `okf.invalidations.*` metrics |
| 2 | Every `bump_admin_generation` call becomes `invalidate(conn, &[..])` with its kinds; `catalog` invalidations from publish, import and repair; per-function coverage tests; two-replica tests with TTLs and poll at 1h |
| 3 | Audit rows created by the create paths; migration `0023_audit_backfill.sql`; `gates::load` stops writing |
| 4 | `moka`; `cache::Bounded<K, V>` (epochs, stale marking, single-flight, metrics `okf.cache.*`) |
| 5 | `cache::directory::CatalogDirectory` and the store queries behind it (row by name with access facts, list by names, paginated list) |
| 6 | Publish and rebuild locks move from `CatalogState` to `KeyedLocks` on `App` |
| 7 | `ToolContext` resolution async (`select`, `one`, `states`, `read_set`, `report_catalog`, `changeset`), still over the map; all callers `.await` |
| 8 | `cache::catalogs::CatalogCache` replaces the `Catalogs` map: nothing loaded at start-up, per-request pinning of the snapshot, negative cache, weight factor measured; description and owner from the directory; the per-catalog gauges and `/healthz`'s catalog count removed with the map |
| 9 | Overlays weighed in bytes (`overlay_cache_bytes`); `overlay_cache_size` deprecated |
| 10 | Paginated catalog listings (`GET /api/v1/catalogs`, identity, labels, MCP `list_catalogs`), `list_changesets` without a scan, the `/api/search` fan-out cap, the console following `next` |
| 11 | `cache::credentials`: credential and user caches replace `KeyCache` for authentication, sessions and OAuth grants; suspension and gates checked against the directory |
| 12 | Labels on demand (`LabelLookup`), replacing `keys().labels()` |
| 13 | Settings on invalidation; `admin_generation` reads reduced to settings; the M16 announcement bridge (spec Decision 14) |
| 14 | #458: worker pool shared and bounded, heartbeat 10 s x 6, `mail.worker` deprecated, `prune_workers`, shutdown grace |
| 15 | `src/housekeeping.rs`: the `housekeeping` queue, the slot ticker, the five tasks, `cluster_stats` table and gauges |
| 16 | The poll loop, `settle`, `poll_interval`, `okf.poll` and `okf_catalog_refreshes_total` removed |
| 17 | Docs: `docs/operations.md`, `docs/reference/`, the server design note, release notes draft |
| 18 | Scale test (ignored by default), smoke test, README, milestone changelog |

**Deliberately deferred**

| Item | Where it goes |
|---|---|
| Partitioning catalogs across replicas | not planned; the cache metrics show when it is needed |
| Shared rate limits | #357, unchanged |
| A shared snapshot store | not planned (spec Non-goals) |

## File map

| File | Tasks |
|---|---|
| `Cargo.toml`, `Cargo.lock` | 4 (`moka`) |
| `src/lib.rs` | 1 (`pub mod invalidate;`), 4 (`pub mod cache;`), 15 (`pub mod housekeeping;`) |
| `src/invalidate.rs` | 1 (create), 8, 11, 12, 13 (each moves `apply` onto its cache) |
| `src/cache/mod.rs`, `src/cache/bounded.rs` | 4 (create) |
| `src/cache/directory.rs` | 5 (create), 11 (access facts read by principals) |
| `src/cache/catalogs.rs` | 8 (create) |
| `src/cache/credentials.rs` | 11 (create), 12 |
| `src/cache/labels.rs` | 12 (create) |
| `src/housekeeping.rs` | 15 (create) |
| `src/store/catalogs.rs` | 2 (`bump_admin_generation` moves into `src/invalidate.rs` as a private function; Task 1 left it `pub(crate)` because the 37 callers remain), 5 (`row_by_name`, `rows_by_names`, `page`) |
| `src/store/{keys,users,teams,ceiling,settings,unlocks,import,repair,publish}.rs`, `src/store/oauth/{codes,grants}.rs` | 2 |
| `src/store/{users,keys,teams}.rs`, `src/store/oauth/{codes,grants}.rs`, `src/store/gates.rs`, `src/store/audit.rs` | 3 |
| `migrations/0023_audit_backfill.sql` | 3 |
| `migrations/0024_cluster_stats.sql` | 15 |
| `src/catalog.rs` | 6 (locks out), 8 (description and owner out) |
| `src/keyed_lock.rs` | 6 (doc only) |
| `src/store/publish.rs`, `src/mcp/changesets.rs` | 6, 8 |
| `src/mcp/context.rs` | 7, 8, 11 |
| `src/mcp/*.rs`, `src/mcp/maintenance/*.rs`, `src/http/api/browse.rs` | 7 (`.await`s) |
| `src/overlay.rs` | 9 |
| `src/http/api/{catalogs,identity,labels,search}.rs`, `src/mcp/catalogs.rs` | 10 |
| `src/http/ui/js/**` (console views that list catalogs) | 10 |
| `src/auth.rs` | 11 (`Keys`/`KeyCache` replaced), 12 |
| `src/http/console_session.rs`, `src/oauth/{access,token,authorize}.rs` | 11 |
| `src/http/api/{audit,people}.rs`, `src/http/archives.rs`, `src/labels/directory.rs` | 12 |
| `src/shared/announce.rs` | 13 (bridge), 16 |
| `src/mail/queue.rs` | 14, 15 (shared worker pool) |
| `src/config.rs` | 8, 9, 10, 11, 14, 16 |
| `src/app.rs` | 1, 6, 8, 11, 13, 15, 16 |
| `src/http/mod.rs` | 1 (spawn listener), 14, 15, 16 |
| `src/http/api/mod.rs` | 16 (`settle` removed) |
| `src/telemetry/metrics.rs` | 1, 4, 15, 16 |
| `src/cli.rs`, `src/cli/*.rs` | 13 (bridge), 16 |
| `tests/support/server.rs` | 2 (TTLs in `serve_app`), 14 (worker on siblings) |
| `tests/multi_replica.rs` | 2, 8, 11, 13, 15 |
| `tests/scale.rs` | 18 (create) |
| `docs/operations.md`, `docs/reference/*.md`, `docs/superpowers/specs/2026-10-26-okf-mcp-server-design.md` | 17 |
| `docs/superpowers/specs/2026-11-18-okf-bounded-memory-design.md` | 8 (weight factor), any task that corrects it |
| `README.md`, `changelog/*-m18-milestone.md` | 18 |
| `changelog/` | every task |

## Task dependencies

Each line is `task (the tasks it needs)`. A task starts when every task in its parentheses has merged. Tasks on one line can run in parallel.

```
wave 1:  1 ()     3 ()     4 ()     6 ()     7 ()     14 ()
wave 2:  2 (1)    5 (1, 4)    15 (14)
wave 3:  10 (5, 7)    11 (2, 3, 5)
wave 4:  8 (2, 5, 6, 7, 10)    12 (11)    13 (11)
wave 5:  9 (8)    16 (8, 10, 11, 12, 13, 15)
wave 6:  17 (all feature tasks)
wave 7:  18 (all)
```

Why each edge, and the shared files to merge with care:
- **2 needs 1** (`invalidate`). 2 is the wide one: 37 call sites in 14 store files. Nothing else in waves 1-2 edits those store files except 3, which touches the *create* functions of users, keys, teams and grants: 3 adds audit inserts near the top of each, 2 swaps the bump at the end. Keep both.
- **5 needs 1 and 4**: the directory is a `Bounded` cache and registers with `invalidate::apply`.
- **8 needs 2, 5, 6, 7 and 10**: the cache is invalidated by `catalog` messages (2), resolves rows through the directory (5), relies on locks outside the entry (6) and on async resolution (7). 8 removes `App::catalogs()` and `ToolContext::catalogs()`, so every caller that enumerates the map must already be gone: 10 converts the listings, and 8 itself takes the rest (the per-catalog gauges, the `/healthz` count, tests). 7 and 8 both edit `src/mcp/context.rs`.
- **10 needs 5 and 7** (it used to need 8): listings page through the directory (5) with async resolution (7), and read the snapshot fields of the listed page through today's `App::catalog(name)`; 8 later swaps that one call for `catalog_state`. 10 and 11 run together and both touch `src/mcp/context.rs` (listing callers vs `admission_to`): keep both.
- **9 needs 8**: overlays use 8's measured weigher.
- **11 needs 2, 3 and 5**: credential and user messages (2), audit ids read with the entity (3), suspension and gates from the directory (5). 11 and 8 both edit `src/app.rs` (different fields) and `src/mcp/context.rs` (`admission_to` vs resolution): keep both.
- **12 and 13 need 11**: both remove what is left of `KeyCache`.
- **15 needs 14**: the housekeeping worker uses 14's shared worker pool and timing.
- **16 needs every task that stops reading the poll's output.** 17 documents what shipped; 18 is last.

---

### Task 1: The invalidation feed (`src/invalidate.rs`)

Spec Decisions 10, 12. This task changes no write path (Task 2 does) and no cache (later tasks do): `invalidate(conn, ..)` exists and is tested, and the listener applies messages to today's caches.

**Files:**
- Create: `src/invalidate.rs`.
- Modify: `src/lib.rs`, `src/app.rs`, `src/http/mod.rs`, `src/telemetry/metrics.rs`, `src/shared/announce.rs` (uses `Backoff`); also create `src/backoff.rs`.

> **Corrected (issue #679, 2026-11-19).** `src/store/catalogs.rs` is not touched: `bump_admin_generation` stays `pub(crate)` until Task 2 replaces its 37 callers.

- [ ] **Step 1: Write the failing tests** (`src/invalidate.rs`, `#[cfg(test)] mod tests`)

```rust
#[test]
fn every_kind_round_trips_through_its_payload() {
    let all = [
        Invalidation::Catalog { id: 42, generation: 17 },
        Invalidation::CatalogMeta { id: 42, name: "notes".into() },
        Invalidation::Credential { key_id: "k_1".to_owned() },
        Invalidation::User { id: "u_1".to_owned() },
        Invalidation::Team { id: "t_1".to_owned() },
        Invalidation::Settings,
    ];
    for invalidation in all {
        let payload = invalidation.to_payload(1_700_000_000_000);
        assert_eq!(Message::parse(&payload).expect("parses").invalidation, invalidation);
    }
}

#[test]
fn a_payload_of_an_unknown_version_or_kind_does_not_parse() {
    assert!(Message::parse(r#"{"v":2,"at":1,"kind":"catalog","id":1,"generation":1}"#).is_none());
    assert!(Message::parse(r#"{"v":1,"at":1,"kind":"nope"}"#).is_none());
    assert!(Message::parse("42").is_none());
}

#[tokio::test]
async fn a_committed_invalidation_is_delivered_and_a_rolled_back_one_is_not() {
    let db = TestDb::start_migrated().await;
    let mut listener = PgListener::connect_with(&db.pool).await.unwrap();
    listener.listen(CHANNEL).await.unwrap();

    let mut tx = db.pool.begin().await.unwrap();
    invalidate(&mut tx, &[Invalidation::Settings]).await.unwrap();
    tx.rollback().await.unwrap();
    let mut tx = db.pool.begin().await.unwrap();
    invalidate(&mut tx, &[Invalidation::CatalogMeta { id: 3, name: "c".into() }]).await.unwrap();
    tx.commit().await.unwrap();

    let got = timeout(WAIT, listener.recv()).await.expect("in time").unwrap();
    let message = Message::parse(got.payload()).expect("parses");
    assert_eq!(message.invalidation, Invalidation::CatalogMeta { id: 3, name: "c".into() });
}

#[tokio::test]
async fn invalidate_bumps_admin_generation_once_per_call() {
    let db = TestDb::start_migrated().await;
    let before = catalogs::admin_generation(&db.pool).await.unwrap();
    let mut tx = db.pool.begin().await.unwrap();
    invalidate(&mut tx, &[Invalidation::Settings, Invalidation::User { id: "u".into() }])
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(catalogs::admin_generation(&db.pool).await.unwrap(), before + 1);
}
```

And in `src/app.rs` tests:
- `a_catalog_invalidation_refreshes_that_catalog`: publish through the store on the pool directly (not through `App`), call `app.apply(&Message { invalidation: Catalog { id, generation }, .. })`, assert `app.catalog(name).current().generation() == generation`.
- `a_credential_invalidation_reloads_the_key_cache`: revoke a key through the store, `apply` a `Credential`, assert `app.keys().find(&key).is_none()`.
- `a_lost_connection_marks_everything_stale`: `app.apply_reconnect().await` reloads keys and refreshes every loaded catalog (today's caches have no "stale" flag; reloading is today's equivalent).

Run: `tools/agentic/gate.sh narrow invalidate` — expected: fails to compile (`invalidate` not defined).

- [ ] **Step 2: Implement `src/invalidate.rs`**

```rust
//! The change feed (spec Decisions 10 and 12). A store function that
//! changes what a replica caches calls [`invalidate`] inside its own
//! transaction; Postgres delivers the `NOTIFY` to every listening replica
//! when, and only if, the transaction commits. [`listen`] runs on every
//! replica and applies each message to that replica's caches.

/// The `LISTEN` channel.
pub const CHANNEL: &str = "okf_invalidate";

/// The payload version this build writes and reads.
const VERSION: u8 = 1;

/// What changed. Ids only: never a key hash, a person's name or content.
// `serde(tag = "kind")` writes the variant as a `"kind"` field next to its
// own fields, e.g. `{"kind":"catalog","id":1,"generation":2}`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Invalidation {
    /// A publish, import or repair moved the catalog to `generation`.
    Catalog { id: i64, generation: i64 },
    /// The catalog row or its access facts changed (create, delete,
    /// details, owner, maintenance, member access, suspension). `name`
    /// (the catalog's slug, an id, not a person's data) lets a replica drop
    /// a cached "no such catalog" entry on create: the id is new, so no
    /// cache can map it back to the name.
    CatalogMeta { id: i64, name: String },
    /// A key or grant (`api_keys.id`) changed.
    Credential { key_id: String },
    /// A user (`users.id`) changed: profile, admin flag, entitlements,
    /// unlocks, or anything that moves their ceiling.
    User { id: String },
    /// A team (`teams.id`, text) changed: members, roles, label.
    Team { id: String },
    /// A server setting changed.
    Settings,
}

/// One received message: what changed, and when it was sent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Message {
    /// What changed.
    pub invalidation: Invalidation,
    /// The sender's clock at `invalidate`, in Unix milliseconds, for
    /// `okf.invalidations.lag`.
    pub sent_at_ms: i64,
}

// The wire form: the version and send time beside the flattened variant.
#[derive(serde::Serialize, serde::Deserialize)]
struct Wire {
    v: u8,
    at: i64,
    #[serde(flatten)]
    invalidation: Invalidation,
}

impl Invalidation {
    /// This invalidation's `NOTIFY` payload.
    ///
    /// # Arguments
    /// * `sent_at_ms` - the sender's clock, Unix milliseconds.
    ///
    /// # Returns
    /// The JSON payload.
    pub fn to_payload(&self, sent_at_ms: i64) -> String {
        serde_json::to_string(&Wire { v: VERSION, at: sent_at_ms, invalidation: self.clone() })
            .expect("an Invalidation always serializes")
    }
}

impl Message {
    /// Parses a payload; `None` for another version, an unknown kind or
    /// anything else this build cannot read.
    pub fn parse(payload: &str) -> Option<Self> {
        let wire: Wire = serde_json::from_str(payload).ok()?;
        (wire.v == VERSION).then_some(Self { invalidation: wire.invalidation, sent_at_ms: wire.at })
    }
}

/// Records `invalidations` in the caller's transaction: bumps
/// `admin_generation` once and sends one `NOTIFY` per invalidation, which
/// Postgres delivers at commit and drops on rollback.
///
/// # Arguments
/// * `conn` - the writer's transaction (`&mut tx`).
/// * `invalidations` - what the transaction changed; empty does nothing.
///
/// # Errors
/// [`OkfError::Database`]; the caller's transaction then fails as it would
/// for any other statement.
#[tracing::instrument(level = "debug", skip_all)]
pub async fn invalidate(conn: &mut PgConnection, invalidations: &[Invalidation]) -> Result<(), OkfError> {
    if invalidations.is_empty() {
        return Ok(());
    }
    catalogs::bump_admin_generation(&mut *conn).await?;
    let now = Utc::now().timestamp_millis();
    for invalidation in invalidations {
        sqlx::query("SELECT pg_notify($1, $2)")
            .bind(CHANNEL)
            .bind(invalidation.to_payload(now))
            .execute(&mut *conn)
            .await?;
        Metrics::global().invalidation_queued(invalidation.kind());
    }
    Ok(())
}
```

`Invalidation::kind(&self) -> &'static str` returns the serde tag (`"catalog"`, ...), for metric labels.

The listener:

```rust
/// Listens on [`CHANNEL`] until `shutdown`, applying each message to
/// `app`. A lost connection (`try_recv` -> `Ok(None)`, already reconnected)
/// marks every cache stale, since messages sent meanwhile are lost (spec
/// Decision 12). A connect failure backs off from 1 s to 30 s.
#[cfg_attr(dylint_lib = "hyp_tracing", allow(uninstrumented_fn, reason = "long-lived loop; each message gets its own root span"))]
pub async fn listen(app: Arc<App>, shutdown: CancellationToken) {
    let mut backoff = Backoff::new(Duration::from_secs(1), Duration::from_secs(30));
    loop {
        let mut listener = tokio::select! {
            () = shutdown.cancelled() => return,
            connected = connect(app.pool()) => match connected {
                Ok(listener) => listener,
                Err(error) => { /* split_event!(warn, ...), sleep backoff.next_delay() or return on cancel */ continue; }
            },
        };
        // A fresh connection: anything sent while we had none is lost.
        app.apply_reconnect().await;
        backoff.reset();
        loop {
            tokio::select! {
                () = shutdown.cancelled() => return,
                received = listener.try_recv() => match received {
                    Ok(Some(notification)) => apply_payload(&app, notification.payload()).await,
                    // Lost and already re-established by sqlx.
                    Ok(None) => app.apply_reconnect().await,
                    Err(_) => break, // log, then reconnect from the outer loop
                },
            }
        }
    }
}

async fn apply_payload(app: &App, payload: &str) {
    match Message::parse(payload) {
        Some(message) => {
            Metrics::global().invalidation_received(message.invalidation.kind(), message.lag(Utc::now().timestamp_millis()));
            app.apply(&message).instrument(tracing::info_span!(parent: None, "okf.invalidate.apply", okf.invalidation.kind = message.invalidation.kind())).await;
        }
        // Unreadable: treat as a reconnect, for safety (spec "Error handling").
        None => { Metrics::global().invalidation_unreadable(); app.apply_reconnect().await; }
    }
}
```

`Backoff` is new: `src/shared/announce.rs` has its 1 s to 30 s backoff inline. Extract it to `src/backoff.rs` (`Backoff::new(first, max)`, `next_delay() -> Duration` doubling to `max`, `reset()`) in this task and use it from both.

- [ ] **Step 3: `App::apply` and `App::apply_reconnect` over today's caches** (`src/app.rs`)

> **Corrected (issue #679, 2026-11-19).** As built, `apply` sends a `Catalog` message through the existing `refresh_announced` (which already finds the loaded state by id, skips a generation already reached and maps a deleted catalog to `Unknown`), and `apply_reconnect` is `reload_keys_now(false)` plus the existing `catch_up_catalogs` (one list read, refreshing only the catalogs behind). The sketch below is the idea, not the code: read `src/app.rs`. Later tasks that move `apply` onto a cache replace these bodies; they do not need `refresh_catalog_by_id`, which was never created.

```rust
/// Applies one change-feed message to this replica's caches.
pub async fn apply(&self, message: &invalidate::Message) {
    match &message.invalidation {
        Invalidation::Catalog { id, .. } => self.refresh_catalog_by_id(*id).await,
        Invalidation::CatalogMeta { .. }
        | Invalidation::Credential { .. }
        | Invalidation::User { .. }
        | Invalidation::Team { .. }
        | Invalidation::Settings => self.reload_keys_now(false).await,
    }
}

/// After a lost connection: every cache is suspect.
pub async fn apply_reconnect(&self) {
    self.reload_keys_now(false).await;
    for state in self.catalogs().values() {
        if let Err(error) = state.refresh(&self.pool, &self.rebuild_locks).await { /* split_event!(warn ...) */ }
    }
}
```

`refresh_catalog_by_id` finds the loaded state by id and calls `refresh`; a catalog not loaded is left to the poll (until Task 8). `CatalogMeta` reloading keys is enough today: description and owner are copied in by the poll, which still runs.

> **Corrected (issue #679, 2026-11-19).** The counter is `okf.invalidations.queued` (Prometheus `okf_invalidations_queued_total`), not `sent`: `invalidate` counts inside the transaction, before the commit decides delivery, so "sent" would count rolled-back messages.

- [ ] **Step 4: Metrics** (`src/telemetry/metrics.rs`; Prometheus names `okf_invalidations_queued_total`, `okf_invalidations_received_total`, `okf_invalidations_unreadable_total`, `okf_invalidations_lag_seconds`): `okf.invalidations.queued{kind}` and `okf.invalidations.received{kind}` (counters), `okf.invalidations.unreadable` (counter), `okf.invalidations.lag` (seconds histogram, send to apply; a negative lag from clock skew records 0). Add the new families to `Metrics::FAMILIES`.

- [ ] **Step 5: Spawn the listener** in `serve` (`src/http/mod.rs`, next to the M16 subscriber at `:660`), and await it in the shutdown sequence like the others. It holds its connection for the life of the process, so it gets its own one-connection pool (`PgPoolOptions::new().max_connections(1)`, same URL and options as the main pool) and `PgListener::connect_with(&listener_pool)`. sqlx's `connect_with` keeps a connection from the pool it is given, so using the main pool would take one request slot away for good. The pool also gives the listener its reconnects. `docs/operations.md`'s per-replica count grows by one in Task 17 (spec Decision 17).

- [ ] **Step 6: Run** `tools/agentic/gate.sh narrow invalidate` and `gate.sh narrow app::tests::a_` — expected PASS. Then the full gates.

- [ ] **Step 7: Commit** — `feat: a change feed over Postgres NOTIFY (M18 task 1)`.

---

### Task 2: Every administrative write and publish sends its invalidations

Spec Decision 10's table. After this task every change reaches every replica within one `NOTIFY` round trip, with or without Redis.

**Files:**
- Modify: `src/store/catalogs.rs`, `src/store/ceiling.rs`, `src/store/settings.rs`, `src/store/keys.rs`, `src/store/users.rs`, `src/store/teams.rs`, `src/store/unlocks.rs`, `src/store/oauth/codes.rs`, `src/store/oauth/grants.rs`, `src/store/publish.rs`, `src/store/import.rs`, `src/store/repair.rs`, `tests/support/server.rs`, `tests/multi_replica.rs`.

- [ ] **Step 1: The mapping.** Replace each `bump_admin_generation(&mut *tx)` with `invalidate(&mut tx, &[..])`:

| Store function (current bump line) | Invalidations |
|---|---|
| `catalogs::create` (`catalogs.rs:178`) | `CatalogMeta { id, name }`, `Team { id: owner }` when it has an owner |
| `catalogs::set_owner` (`:325`) | `CatalogMeta { id, name }`, `Team` for the old and the new owner |
| `catalogs::update_details` (`:493`) | `CatalogMeta { id, name }` |
| `catalogs::set_maintenance` (`:730`) | `CatalogMeta { id, name }` |
| `catalogs::delete` (`:882`) | `CatalogMeta { id, name }`, `Team { id: owner }` |
| `ceiling::set_member_access` (`ceiling.rs:345`) | `CatalogMeta { id, name }`, `Team { id: owner }` |
| `ceiling::entitle` (`:446`), `unentitle` (`:502`) | `User { id }` |
| `settings::set` (`settings.rs:821`), `reset` (`:894`) | `Settings` |
| `oauth::codes::redeem` (`codes.rs:672`) | `Credential { key_id: grant id }` |
| `oauth::grants::create_self_issued` (`grants.rs:312`) | `Credential { key_id: grant id }` |
| `keys::create_provisioner` (`keys.rs:468`), `create_with_id` (`:527`), `create_self_issued` (`:725`) | `Credential { key_id }` |
| `keys::set_maintenance_report_with` (`:1103`) | `Credential { key_id }` |
| `keys::revoke_with` (`:1193`) | `Credential { key_id }` |
| `keys::revoke_if_idle` (`:1341`), called by `revoke_idle_grants` | `Credential { key_id }` (new: today it bumps nothing, and the poll's key reload picks the revocation up) |
| `keys::grant_with` (`:1312`), `ungrant_with` (`:1433`) | `Credential { key_id }` |
| `keys::set_user` (`:1582`) | `Credential { key_id }` |
| `users::create` (`users.rs:253`) | `User { id }`, `Team { id: personal team }` |
| `users::set_email` (`:690`), `set_display_name` (`:757`), `set_admin` (`:850`) | `User { id }` |
| `users::delete` (`:1227`) | `User { id }`, `Credential` for each of the user's keys and grants (the function already selects them), `Team` for each team they were in |
| `unlocks::create` (`unlocks.rs:509`), `end` (`:669`) | `User { id: the unlock's user }` |
| `teams::create` (`teams.rs:292`), `set_display_name` (`:715`), `delete` (`:785`) | `Team { id }` |
| `teams::set_member` (`:515`), `remove_member` (`:585`) | `Team { id }`, `User { id: member }` |
| `teams::suspend` (`:897`), `unsuspend` (`:956`) | `Team { id }`, `CatalogMeta { id, name }` for each owned catalog |
| `publish::publish_locked` (`publish.rs:271`, the generation update) | `Catalog { id, generation: new }` |
| `import` (`import.rs:282`), `repair` (`repair.rs:143`) | `Catalog { id, generation: new }` |

Where a function bumps conditionally (`unentitle`, `set_member_access`, `set_maintenance_report_with`, `set_email`, `unsuspend`), it invalidates under the same condition. `bump_admin_generation` becomes `pub(crate)` used only by `invalidate`; grep must show no other caller.

> **Corrected (issue #680, 2026-11-19).** As built:
> - `bump_admin_generation` moved into `src/invalidate.rs` as a private function; the test-only callers (`src/http/api/{access_tests,changesets}.rs`, `src/mcp/changesets.rs`, `src/store/oauth/testing.rs`, `src/store/settings.rs`) call `invalidate` with a fitting kind.
> - **A list of only `Catalog` invalidations does not bump `admin_generation`** (`invalidate`'s doc comment). Publish, import and repair never bumped it; the poll loop sees them through `catalogs.generation`. Bumping would make every publish take the one `server_state` row lock until commit (serializing all publishes, and adding a lock to publish's order) and make every replica's poll reload the whole key cache after each publish. Spec Decision 10's "admin_generation is a counter of administrative changes" still holds; Decision 14's bridge is unaffected, since old replicas see publishes by catalog generation.
> - `keys::revoke_if_idle` already bumped (through `record_revocation`, which every revocation shares), so the "new" in the table is wrong: `record_revocation` now invalidates the credential, covering `revoke_with` and `revoke_if_idle` at once.
> - `keys::set_user` is reached through `assign_user_by_id` and `assign_user_target`.
> - `unlocks::end` invalidates `User` only while the unlock still names a user (`unlocks.user_id` is `ON DELETE SET NULL`); an unlock of a deleted user reaches nobody, and that path no longer bumps.
> - `users::delete` reads the user's `team_members.team_id`s (no lock: a membership added concurrently waits on the user row it holds) before the cascade; it invalidates the user, each key and grant, then each team, in id order.
> - `catalogs::create` names its owner team by id (`teams::id_of`); `set_owner`, `delete` and `ceiling::set_member_access` read the owner's `teams.id` in their existing `UPDATE`/`SELECT ... FOR UPDATE` (`RETURNING`), adding no lock. `teams::suspend`/`unsuspend` read the owned catalogs with a plain `SELECT` after the team's lock (`suspension_invalidations`).
> - Every `invalidate` call sits exactly where its `bump_admin_generation` was, so each writer's lock order is unchanged. `Invalidation::credential`, `user` and `team` are small constructors from `&str`.

- [ ] **Step 2: Write the coverage tests.** One test per row, in the store module's tests, through a shared helper:

```rust
/// Listens on the change feed, runs `write`, and returns every message
/// delivered within 2 s of its commit (empty if it rolled back).
pub(crate) async fn invalidations_of(pool: &PgPool, write: impl Future<Output = ()>) -> Vec<Invalidation>
```

(in `src/invalidate.rs` under `#[cfg(test)]`). Example:

```rust
#[tokio::test]
async fn revoking_a_key_invalidates_that_credential() {
    let db = TestDb::start_migrated().await;
    let key = create_test_key(&db.pool).await;
    let got = invalidations_of(&db.pool, async {
        keys::revoke_by_id(&db.pool, &key.id, &actor()).await.unwrap();
    }).await;
    assert_eq!(got, [Invalidation::Credential { key_id: key.id.clone() }]);
}
```

Plus `a_refused_write_sends_nothing` for one representative failing write (e.g. revoking an unknown key) asserting an empty list.

> **Corrected (issue #680, 2026-11-19).** `invalidations_of` lives in `crate::invalidate::testing` and takes the write as a future (an `async { .. }` block), not a closure returning one: a closure's `async` block cannot borrow the test's locals past the closure call. It waits for an end marker it sends after the write (Postgres delivers notifications in commit order), not for a fixed 2 s. The per-function tests are in one module, `src/store/invalidation_tests.rs` (`gate.sh narrow --lib store::invalidation_tests`), with shared fixtures; the publish test is `mcp::changesets::publish_tests::a_publish_invalidates_its_catalog_at_the_new_generation` (publishing needs an `App`).

- [ ] **Step 3: Run each new test** (`gate.sh narrow invalidations_of` won't match; run per module, e.g. `gate.sh narrow store::keys::tests::`) — expected FAIL before Step 1's edits, PASS after.

- [ ] **Step 4: Two-replica tests** (`tests/multi_replica.rs`). `serve_app` gains `catalog_ttl` / `credential_ttl` forced to 1 h (the fields arrive in Tasks 8 and 11; until then only `poll_interval = 1h`, already set). New tests, each ending in `eventually(..)` on B:
  - `a_publish_on_one_replica_is_served_on_the_other`: open a changeset and publish on A via MCP; B's `get_concept` returns the new text.
  - `a_key_revoked_through_the_store_is_refused_on_the_other_replica`: revoke with `keys::revoke_by_id` on the pool, as the CLI does (no `settle`, no Redis announcement): B refuses the key. This is the case the M16 design could not cover without the poll.
  - `a_killed_listener_connection_reloads_on_reconnect`: `SELECT pg_terminate_backend(pid) FROM pg_stat_activity WHERE query LIKE 'LISTEN%'` for B's listener, revoke a key on A, assert B refuses it.

> **Corrected (issue #680, 2026-11-19).** `serve_app` needed no change: `catalog_ttl` and `credential_ttl` do not exist yet, and `poll_interval` is already one hour for every `start_shared`/`sibling` replica. A publish through MCP on A also announces over Redis, so it cannot show that `NOTIFY` carried it (and `a_publish_on_one_replica_propagates_to_the_other_promptly`, issue #659, already covers it); the catalog test is `a_catalog_imported_through_the_store_is_served_on_both_replicas`, an import on the pool as `okf-mcp import` does. The revocation in the killed-listener test is made through the store as well (an API revoke would announce over Redis), and the test ends every `okf_invalidate` `LISTEN` backend of the database, both replicas'.

- [ ] **Step 5: Full gates; commit** — `feat: every administrative write and publish sends invalidations (M18 task 2)`.

---

### Task 3: Audit rows are created with their entity (spec Decision 20)

**Files:**
- Create: `migrations/0023_audit_backfill.sql`.
- Modify: `src/store/users.rs` (`create`), `src/store/keys.rs` (`create_provisioner`, `create_with_id`, `create_self_issued`), `src/store/oauth/codes.rs` (`redeem`), `src/store/oauth/grants.rs` (`create_self_issued`), `src/store/teams.rs` (`create`), `src/store/catalogs.rs` (`set_owner`, for the owner team's audit row), `src/store/gates.rs`, `src/store/audit.rs`.

- [ ] **Step 1: Failing tests** (`src/store/gates.rs` tests):
  - `every_new_user_key_grant_and_team_has_its_audit_row`: create one of each through the store functions; assert `audit_people`, `audit_credentials` and `audit_teams` each have the matching row **before** any `gates::load`.
  - `gates_load_writes_nothing`: snapshot row counts of the three audit tables, call `gates::load`, assert unchanged.
  - `the_backfill_creates_missing_rows_and_is_idempotent` (in `src/db.rs` tests, alongside the migration tests): delete a user's audit row, run the migration's SQL, assert it is back; run again, assert no duplicates.

- [ ] **Step 2: The migration** (`migrations/0023_audit_backfill.sql`). Read `src/store/gates.rs:157` (`ensure_audit_rows`) and `src/store/audit.rs` (`audit_person`, `audit_credential`, `audit_team_ref`) first and reproduce exactly the rows they would create, as `INSERT ... SELECT ... WHERE NOT EXISTS` statements, one per table. Copy the column list and any derived value (for example a label snapshot) from those functions; do not invent columns.

- [ ] **Step 3: Create paths.** Each create function calls the matching existing helper (`audit::audit_person(&mut tx, ..)`, `audit_credential`, `audit_team_ref`) in its own transaction, after the entity insert.

- [ ] **Step 4: `gates::load`** drops `ensure_audit_rows` (and the function itself if nothing else calls it).

- [ ] **Step 5: Run, full gates, commit** — `feat: create audit rows with their entity, backfill the rest (M18 task 3)`.

> **Corrected (issue #681, 2026-11-19).** The migration is `0023_audit_backfill.sql`, not `0022`: main's `0022_feedback.sql` (#697) took that number, so Task 15's `cluster_stats` migration moves to `0024`. What shipped differs from Steps 1-4 in these ways too; read `migrations/0023_audit_backfill.sql`, `src/store/audit.rs` (`credential_created`) and `src/store/gates.rs`.
> - There are no Rust helpers `audit::audit_person` or `audit::audit_credential`: those are SQL functions (`migrations/0014_audit_row_locks.sql`). The one new helper is `audit::credential_created(conn, credential_id, label)`, which runs `audit_credential($1, NULL, $2)`. `keys::create_provisioner`, `keys::record_created` (shared by `create_with_id` and `create_self_issued`), `oauth::codes::redeem` and `oauth::grants::create_self_issued` call it after the insert and before `audit::record`, with the credential's `actor` as its label. That is the label `ensure_audit_rows` gave a key and `audit::record` gives a credential when it acts. Before this change, `key.created` and `oauth.grant_created` already made the row through their target, but with no label.
> - `users::create`, `teams::create` and `catalogs::set_owner` are unchanged. Since `0014`, users and teams have had their row from the transaction that creates them (`users::create` through its `user.created` entry and `audit::team_ref` for the personal team, `teams::create` through `record_team`), and `set_owner` already resolves the new owner through `audit::team_ref`. Catalog owners are always teams, so there was nothing left to add.
> - The backfill covers every team, as `0014` did, not only the catalog owners that `ensure_audit_rows` covered. It locks the owners as the resolvers do: every user `FOR KEY SHARE` first, in the users' lock order, then each owner again in the credential insert. This way a user being deleted during the rolling deploy is waited for and skipped, not re-created after `forget_person` (`db::tests::the_backfill_waits_for_a_deletion_and_recreates_nothing_it_forgot`). It also uses `ON CONFLICT DO NOTHING` beside `WHERE NOT EXISTS`, so that a row an old replica makes during the rolling deploy is neither duplicated nor touched. Revoked keys get no row, as before.
> - Tests: `every_new_user_key_grant_and_team_has_its_audit_row` (in `src/store/gates.rs`) covers a user, a team and the three key paths. The two grant paths are asserted in their existing tests, `codes::tests::redeem_turns_the_code_into_an_oauth_grant_once` and `grants::tests::a_self_issued_grant_records_its_user_levels_and_client`, because their fixtures (clients, sessions, ceilings) live there. `gates_load_writes_nothing` replaces `a_key_from_before_the_audit_log_gets_its_credential_id_at_load`: a key inserted without its row now has no credential id after a load. `the_backfill_creates_missing_rows_and_is_idempotent` is in `src/db.rs`.

---

### Task 4: `cache::Bounded<K, V>`

Spec Decisions 2 (single flight, weight), 11 (epochs), 12 (stale), 19 (`okf.cache.*`).

**Files:**
- Create: `src/cache/mod.rs`, `src/cache/bounded.rs`.
- Modify: `Cargo.toml` (`moka = { version = "0.12.15", features = ["future"] }`, with a comment like the others), `src/lib.rs`, `src/telemetry/metrics.rs`.

- [ ] **Step 1: Failing tests** (`src/cache/bounded.rs`):

```rust
#[tokio::test]
async fn concurrent_misses_load_once() {
    let cache = Bounded::<i64, Arc<str>>::new(CacheName::Directory, Limit::Entries(10), TTL);
    let loads = AtomicUsize::new(0);
    let load = || async { loads.fetch_add(1, SeqCst); tokio::time::sleep(MS_50).await; Ok::<_, OkfError>(Arc::from("v")) };
    let (a, b) = tokio::join!(cache.get_or_load(1, load), cache.get_or_load(1, load));
    assert_eq!((a.unwrap(), b.unwrap()), (Arc::from("v"), Arc::from("v")));
    assert_eq!(loads.load(SeqCst), 1);
}

#[tokio::test]
async fn a_load_overtaken_by_an_invalidation_is_served_but_not_kept() {
    let cache = Bounded::<i64, i64>::new(CacheName::Directory, Limit::Entries(10), TTL);
    let (release, wait) = tokio::sync::oneshot::channel::<()>();
    let pending = cache.get_or_load(1, || async { wait.await.ok(); Ok::<_, OkfError>(16) });
    let invalidate = async { cache.invalidate(&1).await; release.send(()).ok(); };
    let (served, ()) = tokio::join!(pending, invalidate);
    assert_eq!(served.unwrap(), 16, "the caller still gets what it loaded");
    assert_eq!(cache.peek(&1).await, None, "but it is not cached");
}

#[tokio::test]
async fn invalidating_keys_that_are_not_cached_keeps_no_state() {
    let cache: Bounded<i64, u32> = Bounded::new(CacheName::User, Limit::Entries(10), TTL);
    for key in 0..100_000 {
        cache.invalidate(&key).await;
    }
    assert_eq!(cache.tracked_epochs(), 0, "no load in flight, so no epoch kept");
}

#[tokio::test]
async fn an_epoch_outlives_the_entry_until_the_older_load_finishes() {
    // Load A starts; the key is invalidated; load A is still running when a
    // second invalidation arrives. A must still not be cached when it ends,
    // and the epoch entry is gone once it has.
    /* hold load A on a oneshot, invalidate twice, release, assert peek is None
       and tracked_epochs() == 0 */
}

#[tokio::test]
async fn an_indirect_invalidation_fences_a_load_in_flight() {
    // A load of key 1 is held; `invalidate_where` matches nothing resident
    // (the cache is empty), yet the held load must not be cached.
    /* hold get_or_load(1) on a oneshot, invalidate_where(|_, _| true), release:
       the caller gets the value, peek(&1) is None */
}

#[tokio::test]
async fn a_stale_entry_is_revalidated_on_next_use() { /* insert, mark_all_stale(), get_or_revalidate calls `check` once and keeps the value when `check` says it is current */ }

#[tokio::test]
async fn an_entry_heavier_than_the_whole_budget_is_served_and_not_kept() { /* Limit::Bytes(100), weigher says 1000 */ }

#[tokio::test]
async fn entries_expire_after_the_ttl() { /* tokio::time::pause(); advance past TTL; next get reloads */ }
```

- [ ] **Step 2: Implement.** The public surface:

```rust
/// Which cache, for metric labels and logs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CacheName { Catalog, Overlay, Directory, Credential, User, Labels }

/// A cache's capacity: by entry count, or by estimated bytes through a weigher.
#[derive(Debug, Clone, Copy)]
pub enum Limit { Entries(u64), Bytes(u64) }

/// A bounded async cache with single-flight loads, per-key invalidation
/// epochs (a load overtaken by an invalidation is not kept) and stale
/// marking (spec Decisions 2, 11, 12).
pub struct Bounded<K, V> { /* moka::future::Cache<K, Entry<V>>, epochs: std::sync::Mutex<HashMap<K, u64>>, stale_before: AtomicU64, name, limit */ }

impl<K: Hash + Eq + Clone + Send + Sync + 'static, V: Clone + Send + Sync + 'static> Bounded<K, V> {
    pub fn new(name: CacheName, limit: Limit, ttl: Duration) -> Self;
    /// With `Limit::Bytes`: how much one value weighs.
    pub fn with_weigher(self, weigher: impl Fn(&K, &V) -> u32 + Send + Sync + 'static) -> Self;
    /// The cached value, loading it once however many callers miss together.
    pub async fn get_or_load<F, Fut>(&self, key: K, load: F) -> Result<V, OkfError>
    where F: FnOnce() -> Fut, Fut: Future<Output = Result<V, OkfError>>;
    /// As `get_or_load`, and a cached entry marked stale (or older than the
    /// TTL) is first passed to `check`, which returns `Ok(None)` to keep it
    /// or `Ok(Some(new))` to replace it.
    pub async fn get_or_revalidate<F, Fut, C, CFut>(&self, key: K, load: F, check: C) -> Result<V, OkfError>;
    /// The cached value without loading or counting a request (tests, listings).
    pub async fn peek(&self, key: &K) -> Option<V>;
    /// Drops `key` and bumps its epoch.
    pub async fn invalidate(&self, key: &K);
    /// Drops every entry `predicate` selects, bumping their epochs.
    pub fn invalidate_where(&self, predicate: impl Fn(&K, &V) -> bool);
    /// Marks every current entry stale (Decision 12).
    pub fn mark_all_stale(&self);
    /// Re-inserts `key`'s cached value so the weigher runs again: moka
    /// weighs on insert and replace only, not when data behind an `Arc`
    /// changes. Respects epochs like a load (a re-weigh overtaken by an
    /// invalidation is dropped). No-op if `key` is not cached.
    pub async fn reweigh(&self, key: &K);
    /// Keys with a load in flight (tests: the epoch map's size).
    #[cfg(test)]
    pub fn tracked_epochs(&self) -> usize;
}
```

Implementation notes for the engineer:
- Each `Entry<V>` stores `value`, `loaded_at: Instant` and `stamp: u64` (the `stale_before` counter at load). `mark_all_stale` increments `stale_before`; an entry with `stamp < stale_before` is stale.
- Epochs exist only for keys with a load in flight. The map is `inflight: std::sync::Mutex<HashMap<K, InFlight>>`, `InFlight { loads: u32, epoch: u64 }`. Before calling `load`, a caller registers (`loads += 1`, inserting `{ loads: 1, epoch: 0 }` if absent) and records the epoch it saw; a guard deregisters it when the load finishes, errors or is cancelled (`Drop`), removing the entry when `loads` reaches 0. `invalidate(key)` drops the cache entry and, **only if `inflight` has the key**, increments its epoch; an invalidation of a key that is neither cached nor loading leaves no state at all. Use moka's `try_get_with` for single flight, with the init future returning the value and the epoch it started at; after it resolves, if the key's epoch moved, call `cache.invalidate(&key)` and return the value to the callers. The map is therefore bounded by the number of concurrent loads, not by the cache or by the change history. An epoch is never reset while an older load is still registered, because the entry outlives every registered load.
- `Bounded` has no eviction listener for epochs (the listener only counts evictions for metrics).
- Minimum generations (Decision 11, used by Task 8) follow the same rule: `InFlight` also carries `min_generation: Option<i64>`, raised by a `Catalog` message only while a load is in flight. A load that starts after the message was applied reads after the writer's commit (Postgres delivers `NOTIFY` at commit), so it needs no minimum.
- `invalidate_where` uses `iter()` (sync) and calls `invalidate` for each match; the cache is bounded, so the scan is bounded. Do not enable moka's `support_invalidation_closures` (it keeps predicates around).
- **Indirect invalidations fence every in-flight load.** A predicate sees only resident entries, so it cannot reach a load whose identity is known only from its result (a `by_hash` load of a key being revoked by id; a user load overtaken by a `Team` message). `Bounded` therefore keeps one cache-wide `fence: AtomicU64`. Each load records it at start, next to its key's epoch. `invalidate_where` increments the fence *before* scanning, and a load inserts only if both its key's epoch and the fence are unchanged; otherwise it serves its result uncached, exactly like a direct overtake. The cost is that an indirect invalidation leaves the loads in flight at that moment uncached (they are still served, and the next request loads again). At administrative write rates, tens per second, that is negligible. The state is one counter, so memory stays bounded.
- Over-budget values: with `Limit::Bytes(n)`, a value whose weight exceeds `n` is returned to the callers of that load and then invalidated, so it is never kept. Concurrent callers still share the one load (moka's single flight); a later request loads it again. Log a warn with the weight, at most once per key per hour.
- Metrics (`src/telemetry/metrics.rs`): `okf.cache.requests{cache,result=hit|miss|stale}`, `okf.cache.evictions{cache,cause=size|ttl|invalidated}` from the eviction listener (`RemovalCause::Size` -> `size`, `Expired` -> `ttl`, `Explicit` -> `invalidated`, `Replaced` not counted), `okf.cache.entries{cache}` and `okf.cache.bytes{cache}` as observable gauges over `entry_count()`/`weighted_size()`, `okf.cache.load.duration{cache,outcome}`.

- [ ] **Step 3: Run, full gates, commit** — `feat: a bounded cache with epochs and stale marking (M18 task 4)`.

> **Corrected (issue #682, 2026-11-18).** What shipped in `src/cache/bounded.rs` differs from the surface below in these ways; read the file for the code.
> - `with_metrics(Metrics)` (call it last) turns on `okf.cache.*` and registers the size gauges (`Metrics::observe_cache`, holding a `Weak` probe); `with_weigher` goes before it. Types `Lookup` (`hit|miss|stale`), `Eviction` (`size|ttl|invalidated`) and the trait `CacheSize` live in `cache::bounded`. `okf.cache.bytes` has no unit (a `By` unit renders as `okf_cache_bytes_bytes`) and reports only for `Limit::Bytes`.
> - `invalidate_where` and `reweigh` are `async`.
> - `get_or_load` treats a stale-marked entry as a miss (it reloads). `get_or_revalidate`'s `check` runs **only** for stale-marked entries, never for a fresh one: moka's `time_to_live` is the only age limit, so "older than the TTL" is an ordinary miss. Anything that needs a shorter lifetime for some entries (Task 5's absent-name entries, 5 s) must not rely on `check` seeing them: use a second `Bounded` with its own TTL, or put the age in the value and treat an old one as a miss in the caller.
> - A caller that shared another caller's failed load gets `OkfError::Busy` (`OkfError` is not `Clone`); the caller that ran the loader gets the original error.
> - The over-budget warning is rate-limited once an hour **per cache**, not per key: per-key state would be unbounded.
> - Minimum generations (`InFlight.min_generation`, Decision 11) are **not** in Task 4: `Bounded` cannot read a generation out of `V`. Task 8 adds them (a `generation_of` function given to the cache, or checked in its `check`/load closures) with `CatalogCache`.
> - `with_idle` (named in the self-review's list of names) was never part of Task 4 and does not exist.


---

### Task 5: The catalog directory (spec Decisions 6, 7 store side)

**Files:**
- Create: `src/cache/directory.rs`.
- Modify: `src/store/catalogs.rs`, `src/app.rs` (field and constructor only), `src/invalidate.rs` (`CatalogMeta` and `Catalog` also invalidate the directory), `src/config.rs` (`catalog_directory_entries`, `catalog_ttl`).

- [ ] **Step 1: Store queries** (`src/store/catalogs.rs`). One row type with everything a request needs about a catalog without its content:

```rust
/// A catalog's row with its access facts (spec Decisions 6 and 9).
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct CatalogRow {
    pub id: i64,
    pub name: String,
    pub display_name: String,
    pub description: String,
    pub generation: i64,
    pub owner_team: Option<String>,
    pub owner_team_id: Option<String>,
    /// The owner team's suspension, if suspended (`teams.suspended_at`).
    pub suspended: Option<SuspendedRow>,
    /// `NOT catalogs.maintenance`.
    pub maintenance_off: bool,
    /// The owner team's `audit_teams.id` (`CatalogScope::team`).
    pub audit_team: Option<i64>,
}

pub async fn row_by_name(executor: impl PgExecutor<'_>, name: &str) -> Result<Option<CatalogRow>, OkfError>;
pub async fn rows_by_names(executor: impl PgExecutor<'_>, names: &[String]) -> Result<Vec<CatalogRow>, OkfError>;
/// One page, by name, after `after` (exclusive).
pub async fn page(executor: impl PgExecutor<'_>, after: Option<&str>, limit: i64) -> Result<Vec<CatalogRow>, OkfError>;
```

Build the SELECT once in a macro next to `select_catalogs!`, joining `teams` for the owner and suspension and `audit_teams` for the audit id, copying the column expressions from `teams::suspended_catalogs` (`src/store/teams.rs:974`) and `gates::load`'s catalogs query (`src/store/gates.rs`) so the values are identical to today's. Tests: each function against fixtures with a suspended owner, a maintenance-off catalog and an unowned one; `page` walks three pages of five catalogs in name order.

- [ ] **Step 2: Failing tests** (`src/cache/directory.rs`):
  - `resolve_reads_once_then_serves_from_cache` (count queries with a test hook or by checking `okf.cache.requests` through a test meter).
  - `an_unknown_name_is_remembered_briefly_and_forgotten_by_create`: `resolve("x")` is `None`; create `x` through the store (which sends `CatalogMeta { id, name: "x" }`); after `apply`, `resolve("x")` is `Some`. The directory drops by `name` (rows and absent entries are keyed by name) and by `id` (a cached row with that id, for a delete or an update).
  - `a_catalog_meta_invalidation_drops_the_row` and `a_catalog_invalidation_raises_the_minimum_generation`: after `Catalog { id, generation: 9 }`, a row read at generation 8 is not cached (Decision 11).

- [ ] **Step 3: Implement.**

```rust
/// Catalog rows by name, with absent names remembered for 5 s (spec
/// Decision 6). The generation in a row is what [`CatalogCache`] checks
/// its snapshots against.
pub struct CatalogDirectory {
    rows: Bounded<String, Option<Arc<CatalogRow>>>,
    // id -> name, so an invalidation by id finds the row. Bounded with `rows`.
    names: std::sync::Mutex<HashMap<i64, String>>,
    // id -> the lowest generation a row may have (Decision 11).
    minimum: std::sync::Mutex<HashMap<i64, i64>>,
    pool: PgPool,
}

impl CatalogDirectory {
    pub fn new(pool: PgPool, entries: u64, ttl: Duration) -> Self;
    pub async fn resolve(&self, name: &str) -> Result<Option<Arc<CatalogRow>>, OkfError>;
    /// Rows for `names`, from the cache where present, the rest in one query.
    pub async fn resolve_many(&self, names: &[String]) -> Result<Vec<Arc<CatalogRow>>, OkfError>;
    pub async fn apply(&self, invalidation: &Invalidation);
    pub fn mark_all_stale(&self);
}
```

> **Corrected (issue #682).** `Bounded`'s `check` runs only for stale-marked entries, so it cannot expire an absent entry after 5 s. Keep absent names in a second `Bounded<String, ()>` (negative cache) with a 5 s TTL, consulted before the row cache.

Absent entries: a negative cache with a 5 s lifetime (see the note above); present ones follow `catalog_ttl`.

> **Corrected (issue #683, 2026-11-19).** As shipped (read `src/cache/directory.rs`): `rows: Bounded<String, Option<Arc<CatalogRow>>>` plus `absent: Bounded<String, ()>` (5 s), `new(pool, entries, ttl, metrics)`. There are no `names` or `minimum` maps: a `CatalogMeta` message names the catalog, so it drops `rows` and `absent` by name; a `Catalog` message calls `invalidate_where` for a row of that id behind the generation, and the cache's fence keeps loads in flight uncached (Decision 11) without per-id state. `mark_all_stale` and `apply` are `async` (they clear `absent` / `invalidate_where`). `resolve_many` serves cached rows and reads the rest in one query without caching them. (A cached row it visits counts as recently used: moka has no value lookup without an access.) `Bounded::peek_fresh` was added for that. `CatalogRow.suspended` is `Option<SuspendedRow { team, notice }>`; labels are not in the row. The wiring is in `App::apply` / `apply_reconnect` (not `invalidate.rs`). Task 8's "minimum generation" for `CatalogCache` still needs its own handling; Task 11 reads suspension and `audit_team` from `CatalogRow`.

- [ ] **Step 4: Wire into `App`** (`App::directory() -> &CatalogDirectory`) and into `App::apply` / `apply_reconnect`. Nothing reads it yet except tests.

- [ ] **Step 5: Run, full gates, commit** — `feat: the catalog directory (M18 task 5)`.

---

### Task 6: Per-catalog locks outside the snapshot entry (spec Decision 4)

**Files:**
- Modify: `src/catalog.rs`, `src/app.rs`, `src/store/publish.rs`, `src/mcp/changesets.rs`.

- [ ] **Step 1: Failing test** (`src/store/publish.rs` tests): `publishes_of_one_catalog_serialize_across_two_states`: build two separate `CatalogState`s for the same catalog (as an eviction and reload would), start two publishes through each, and assert they never overlap (record entry and exit in a shared `Mutex<Vec<_>>`). Today this fails: each state has its own `publishing` mutex.

- [ ] **Step 2: Implement.** `App` gains `publish_locks: KeyedLocks` and `rebuild_locks: KeyedLocks`. `CatalogState::publish_lock()` is removed; callers use `app.publish_lock(state.id()).await` (returns `KeyedGuard<'_>`). `refresh_to` takes the rebuild lock from a `&KeyedLocks` argument passed by its callers (`refresh`, `refresh_locked`), so `CatalogState` holds no mutex. Update the doc comments at `src/catalog.rs:177` and `src/overlay.rs` that describe the in-process half of the publish lock.

> **Corrected (issue #684, 2026-11-19).** As shipped:
> - `CatalogState::refresh(pool, rebuilds: &KeyedLocks)`, `refresh_locked(conn, rebuilds)` and `current_covering(pool, rebuilds, bases)` all take the rebuild locks, since `current_covering` refreshes too; so does `Overlays::get(pool, rebuilds, state, changeset)`, which calls it. Callers pass `app.rebuild_locks()` (`&KeyedLocks`). `App::publish_lock(catalog_id) -> KeyedGuard<'_>` is as planned.
> - `src/overlay.rs` had no comment about the publish lock; its only change is the `rebuilds` argument. `src/keyed_lock.rs`'s module doc now names catalog ids too.
> - The test cannot see two publishes overlap from outside: their transactions serialize on the advisory lock anyway, and the in-process lock's job is the refresh after commit. So `publishes_of_one_catalog_serialize_across_two_states` holds `app.publish_lock(id)` as a publish through the first state would, publishes through a second `CatalogState::load` of the catalog, and asserts it waits (changeset still open) until the guard drops, then publishes. `catalog::tests::refreshes_of_two_states_share_the_rebuild_lock` does the same for the rebuild lock.

- [ ] **Step 3: Run** `gate.sh narrow publish` and `gate.sh narrow lock_order` (`src/mcp/changesets/lock_order_tests.rs` must pass unchanged); full gates; commit — `refactor: per-catalog locks outside the snapshot (M18 task 6)`.

---

### Task 7: `ToolContext` resolves catalogs asynchronously

Mechanical: behaviour unchanged, still over the copied map. It moves the `.await`s now so Task 8 changes only the source.

**Files:**
- Modify: `src/mcp/context.rs`, `src/selection.rs` (only if `resolve`'s `exists` closure must become a precomputed set), and every caller listed in Verified facts.

- [ ] **Step 1:** Change the signatures:

```rust
pub async fn select(&self, argument: Option<&[String]>) -> Result<Selection, OkfError>;
pub async fn check_connection(&self) -> Result<(), OkfError>;
pub async fn states(&self, argument: Option<&[String]>) -> Result<Vec<Arc<CatalogState>>, OkfError>;
pub async fn read_set(&self, catalog: Option<&str>) -> Result<Vec<Arc<CatalogState>>, OkfError>;
pub async fn one(&self, catalog: Option<&str>) -> Result<Arc<CatalogState>, OkfError>;
pub async fn report_catalog(&self, catalog: Option<&str>, pinned: Option<&Pinned>) -> Result<Arc<CatalogState>, OkfError>;
async fn state(&self, name: &str) -> Result<Arc<CatalogState>, OkfError>; // no more `expect`
```

`selection::resolve` keeps a sync `exists` closure: `select` first collects the candidate names (connection list, argument, key default, server default), checks their existence in one step (here: the map; in Task 8: `directory.resolve_many`), then passes a `HashSet` to `resolve`. `state` returns `CatalogNotSelected` instead of panicking for a missing name.

> **Corrected (issue #685, 2026-11-19).** Task 7 shipped with `select` still passing `resolve` a sync closure over the copied map (`self.catalogs.contains_key(name)`), not a precomputed `HashSet`: with the map as the source the two are equivalent, and building the set needs the candidate names, which only Task 8's `directory.resolve_many` makes worth collecting. Task 8 changes `select` (in `src/mcp/context.rs`) to collect the candidates, resolve them in one step and pass the set. Also, `mcp::catalogs::list` (the `list_catalogs` tool's body, `src/mcp/catalogs.rs`) became `async` because it calls `select`; Task 10 rewrites it as a query and must keep it `async`. `state` returns `CatalogNotSelected { catalog }` (the variant's only field).

- [ ] **Step 2:** Add `.await` at every caller. `gate.sh lint` lists them; no other change.

- [ ] **Step 3:** Full gates (the existing tests are the test); commit — `refactor: async catalog resolution in ToolContext (M18 task 7)`.

---

### Task 8: `CatalogCache` replaces the catalog map (spec Decisions 2, 3)

**Files:**
- Create: `src/cache/catalogs.rs`.
- Modify: `src/app.rs`, `src/catalog.rs`, `src/mcp/context.rs`, `src/mcp/mod.rs`, `src/store/publish.rs`, `src/http/archives.rs`, `src/invalidate.rs`, `src/config.rs`, `src/telemetry/metrics.rs`, `src/http/mod.rs` (`/healthz`), `src/http/api/access_tests.rs`, `tests/multi_replica.rs`, the spec (weight factor).

- [ ] **Step 1: Measure the weight factor.** Measure, don't guess. In an `#[ignore]` test `measure_snapshot_overhead`, build snapshots of (a) a 500-document synthetic catalog with links and frontmatter, (b) a 5,000-document one, and measure the allocator delta with a counting `#[global_allocator]` wrapper in that test binary only (`tests/snapshot_weight.rs`, its own binary so the allocator affects nothing else). Report `resident / stored_bytes` for each; pick the larger, round up, and record method and numbers in spec Decision 2 ("Corrected (issue #N)"). The production weigher is `stored_bytes * FACTOR`, clamped to `u32::MAX`.

- [ ] **Step 2: Failing tests** (`src/cache/catalogs.rs`, `src/app.rs`):
  - `start_up_loads_no_catalog`: `App::load` with three catalogs; `okf.cache.entries{cache=catalog}` is 0.
  - `the_first_read_loads_and_the_second_hits`.
  - `a_catalog_invalidation_refreshes_in_place`: readers that pinned the old `Arc<Loaded>` keep it; new reads get the new generation.
  - `a_broken_generation_is_not_rebuilt_on_every_request`: store rows that fail to build; two reads, one build attempt; after a new generation, one more attempt.
  - `a_request_keeps_its_snapshot_while_the_entry_is_evicted` (budget of one catalog, two catalogs read in one `ToolContext`).
  - `a_request_sees_one_snapshot_when_the_catalog_refreshes_between_reads`: in one `ToolContext`, read catalog `a`, publish `a` through the store and `apply` its `Catalog` message (an in-place refresh), read `a` again: both reads return the first generation; a new `ToolContext` sees the second.
  - `a_resident_catalog_that_grows_past_the_budget_is_not_kept`: budget of 10 KiB, load a 2 KiB catalog, publish documents that take it past 10 KiB, refresh it (once through `apply`, once through the publisher's direct refresh): `okf.cache.bytes{cache=catalog}` reflects the new weight and the entry is gone (over budget), not reported at its original size.
  - In `tests/multi_replica.rs`: `a_catalog_created_on_one_replica_is_readable_on_the_other` (no poll: directory invalidation plus load on demand).

- [ ] **Step 3: Implement.**

```rust
/// Catalog snapshots by id, bounded in estimated bytes (spec Decision 2).
pub struct CatalogCache {
    states: Bounded<i64, Arc<CatalogState>>,
    // (id, generation) pairs whose stored data would not build.
    broken: Bounded<(i64, i64), Arc<OkfError>>,
    pool: PgPool,
}

impl CatalogCache {
    pub fn new(pool: PgPool, budget_bytes: u64, ttl: Duration) -> Self;
    /// The state of `row`'s catalog at `row.generation` or newer: cached,
    /// refreshed in place, or loaded.
    pub async fn get(&self, row: &CatalogRow, rebuild_locks: &KeyedLocks) -> Result<Arc<CatalogState>, OkfError>;
    pub async fn apply(&self, invalidation: &Invalidation, rebuild_locks: &KeyedLocks);
    pub fn mark_all_stale(&self);
}
```

`get`: `states.get_or_revalidate(row.id, load, check)`, where `check` compares `state.current().generation()` with `row.generation` and refreshes in place when behind, then returns `Ok(Some(Arc::clone(&state)))`, so `Bounded` replaces the entry with the same `Arc` and the weigher runs on the new snapshot. Every other in-place refresh (the `Catalog` message in `apply`, the publisher's and the import's direct refreshes) ends with `states.reweigh(&id)`, through one `CatalogCache::refreshed(id)` helper so no refresh path can skip it. A delete (`CatalogMeta` whose directory row is now absent) invalidates the id. `CatalogState` loses `description`, `owner_team`, `set_description`, `set_owner_team`: readers take them from the `CatalogRow`. `CatalogState::load` takes `(pool, &CatalogRow)` instead of `(pool, name)`.

`App`:
- `catalogs`, `catalogs()`, `catalog()` and the `Broken` map are removed. New: `App::catalog_row(name) -> Result<Option<Arc<CatalogRow>>>` (directory) and `App::catalog_state(&CatalogRow) -> Result<Arc<CatalogState>>` (cache).
- `load_with_redis` no longer lists or loads catalogs; it checks `default_catalog` with one `directory.resolve`.
- `poll_pass` drops the catalog loop (the rest of the poll stays until Task 16).
- The remaining map enumerations go here, since the map does: the `okf.catalog.concepts` and `okf.catalog.generation` gauges (`src/telemetry/metrics.rs:493`, `:510`, `:525`) and `/healthz`'s `catalogs` count (`src/http/mod.rs:229`; replace it with `"cached_catalogs"` from the cache's entry count and update the healthz shape tests). Task 16 then only removes the poll's own metrics. Listing callers were converted in Task 10; `rg 'catalogs\(\)' src` must find none outside tests before this task's PR opens.

`ToolContext`: drops the `catalogs` field; adds `pinned: Mutex<HashMap<String, Pinned>>`, `Pinned { row: Arc<CatalogRow>, state: Arc<CatalogState>, snapshot: Arc<Loaded> }`. Pinning a `CatalogState` alone does not pin a snapshot: `current` is a `RwLock<Arc<Loaded>>` swapped by every in-place refresh. On first use of a catalog, `ToolContext` resolves the row and state and takes `state.current()` once, as `snapshot`. Request reads go through `ToolContext::snapshot(name) -> Arc<Loaded>`; `state(name)` remains for writers that must see the latest (publish under its lock, overlay builds keyed by changeset, which validate against the current generation themselves). Convert the read sites that call `.current()` on a context's state (`grep -rn '\.current()' src/mcp src/http`, about 60) to `snapshot`; any that must stay on `state().current()` gets a one-line comment saying why. `resolve_changeset` finds the catalog by `row.catalog_id` through a new `catalogs::row_by_id` (add to Task 5's queries here) instead of scanning. `ToolContext::catalogs()` is removed; Task 10 already converted its listing callers (and left `App::loaded_catalog(row)` for this task to swap for `catalog_state`; issue #688), and its test callers (`src/http/api/access_tests.rs`) read `ctx.state(name)` instead.

`src/mcp/mod.rs:114` (`catalog_label` existence for telemetry): use `directory.resolve`.

Publish (`src/store/publish.rs:303`) and HTTP import (`src/http/archives.rs:576`) keep refreshing the publisher's own entry directly (its `NOTIFY` arrives a moment later and is then a no-op).

> **Corrected (issue #686, 2026-11-19).** As shipped (read `src/cache/catalogs.rs`, `src/app.rs`, `src/mcp/context.rs`):
> - **The weight is `1024 + 1024 * files + 8 * stored_bytes`** (`cache::catalogs::weight(stored_bytes, files)` and `weight_of(&Loaded)`, saturating at `u64::MAX`, issue #756; constants `PER_CATALOG_BYTES`, `PER_FILE_BYTES`, `WEIGHT_FACTOR`). Content alone would let empty catalogs and empty files weigh nothing (review of PR #714). `Loaded::stored_bytes()` is new. Method and numbers are in spec Decision 2; the measurement is `tests/snapshot_weight.rs` (`--ignored`).
> - `CatalogCache::new(pool, budget_bytes, metrics)`: no TTL argument. Freshness is not the cache's: `get(row, rebuilds)` uses `get_or_load` and then compares the snapshot's generation with the row's, refreshing in place when behind (`get_or_revalidate`'s `check` runs only for stale-marked entries, so it could not do this). The snapshot cache's moka TTL is a fixed one-day backstop. There is **no `CatalogCache::mark_all_stale`**: a reconnect marks the directory stale (`App::apply_reconnect`, and `App::catch_up_catalogs`, which the M16 subscriber calls on a resubscribe, is now just that), and each use then rereads the row and catches the snapshot up.
> - `apply` is `catch_up(id, generation, rebuilds) -> Result<CatchUp, OkfError>` (`NotResident | Current | Applied`): used by the change feed (`App::apply`), M16 announcements (`refresh_announced`) and the HTTP import. **Minimum generations (Decision 11)** are the cache's epoch fence: a `Catalog` message that finds the entry not resident calls `Bounded::invalidate(id)`, which moves the epoch of a load in flight (only then: no state otherwise), so that load is served but not kept. It is conservative (a load that already read the new generation is not kept either) and needs no per-id state.
> - Re-weighing: `refreshed(id)` (every refresh inside the cache) and `refreshed_if_moved(state, before)` (a publish, on every outcome, since `current_base` can refresh before a merge or lint refusal; an overlay build and `pending_changes`, which can refresh through `current_covering`).
> - Read-your-writes on the writing replica (review of PR #714): `settle` no longer refreshes catalogs, so every HTTP route (a layer on the top-level router: the API, the archive routes, the console forms, OAuth and MCP) runs inside `invalidate::capturing`, which collects the invalidations the request's writes queued (a task-local filled by `invalidate`), and `App::apply_own_writes` applies them to the directory before the response goes out (in-process drops only, no I/O after the commit). **Task 16**: when `settle` goes, the same capture can serve the key and credential caches.
> - The broken map is `Bounded<(id, generation), ()>` (10,000 entries, one hour). A request for a broken generation gets the older snapshot if one is cached, else `CatalogNotSelected` (what an unloaded catalog answered before). A failed in-place refresh keeps serving the old snapshot.
> - `CatalogMeta`: `App::forget_if_deleted(id, name)` drops a cached catalog whose name no longer resolves to that id (one directory read, only when it is cached); a description, owner or maintenance change keeps the snapshot.
> - `App` keeps an async `catalog(name) -> Result<Option<Arc<CatalogState>>>` (row, then state; `None` for absent or unbuildable) for the archive routes and tests, and adds `listed_state(&row)` (`catalog_state` with `CatalogNotSelected` as `None`), which replaced `loaded_catalog`. Listings (`GET /catalogs`, MCP `list_catalogs`) load the page's catalogs one after another and leave out one that does not build; `/api/search` loads the catalogs it would search (it reads them anyway); **`GET /whoami` lists every reachable name without loading** (a behaviour change: before, a catalog this replica had not loaded was left out).
> - `ToolContext` has no `Pinned` for catalogs (that name is the changeset's): `held: Mutex<HashMap<String, Held { state, snapshot }>>` plus `rows` (the rows `select` read, so `state` needs no second read). `select` looks up each candidate with the cached `directory.resolve`, not `resolve_many` (whose bulk reads are not cached, so every request would read the rows again; Task 7's note expected `resolve_many`). Reads use `ToolContext::snapshot(&state)` (not `snapshot(name)`): `view` and the three browse routes were the only production read sites; the rest of the `.current()` calls are writers (publish, `current_covering`) or tests. `state` is `pub(crate)`. `resolve_changeset` uses `catalogs::row_by_id` (added, with a test in `src/store/catalogs_rows_tests.rs`).
> - `App::poll_pass` keeps only the key reload; `okf_catalog_refreshes_total` is still declared but no longer recorded (**Task 16** deletes it with `RefreshResult`). The `okf.catalog.refresh` span went with the loop. The Grafana example lost its two per-catalog panels, and its `$catalog` variable reads `okf_publish_duration_seconds_count`.
> - `/healthz`'s field is `cached_catalogs` (a `u64`, `Bounded::entry_count`, added). `catalog_cache_bytes` is a `ByteSize` (`"1GiB"`).
> - `tests/support/server.rs` now also forces `catalog_ttl = 1h` for `start_shared`/`sibling`, as the conventions ask, so a multi-replica pass proves `NOTIFY` carried a catalog change.

- [ ] **Step 4: Run, full gates; update the spec's Decision 2 with the factor; commit** — `feat: catalogs load on demand into a bounded cache (M18 task 8)`.

---

### Task 9: Overlays weighed in bytes (spec Decision 5; needs Task 8's weigher)

**Files:** Modify `src/overlay.rs`, `src/config.rs`, `src/app.rs`.

- [ ] **Step 1: Failing test:** `overlays_are_evicted_by_bytes_not_count`: `overlay_cache_bytes` fits two overlays of a test catalog; build three; the least recently used is gone.
- [ ] **Step 2: Implement.** Replace the hand-rolled `HashMap` and `capacity` with `Bounded<i64 /* changeset id */, Arc<Overlay>>` (`CacheName::Overlay`, `Limit::Bytes`, `time_to_idle = session_idle_timeout` through a `Bounded::with_idle(Duration)` builder method added here), weighed with Task 8's weigher on the overlay's workspace (as shipped in issue #686: `cache::catalogs::weight(stored_bytes, files)`, where a snapshot's are `Loaded::stored_bytes()` and `Loaded::files()`; an overlay must count its own files and their sizes the same way). Keep the validity check (`built_at`) and `builds: KeyedLocks` as they are. Config: `overlay_cache_bytes` (default 256 MiB); `overlay_cache_size` still parses and logs `warn!("overlay_cache_size is deprecated and ignored; set overlay_cache_bytes")` at start-up.
> **Corrected (issue #687, 2026-11-19).** As shipped (read `src/overlay.rs`):
> - `Overlays::new(idle, budget_bytes, metrics)`; the cache is `Bounded<i64, Arc<Overlay>>` with `Limit::Bytes`, a one-day `time_to_live` backstop and `time_to_idle = session_idle_timeout` through the new `Bounded::with_idle`. `Bounded::get_if` and `Bounded::build` are new too (the caller does its own single flight through `builds`).
> - The weight is `cache::catalogs::weight(stored_bytes, files)` over the overlay's whole workspace (the base's files copied plus the pending entries, so an overlay costs about a snapshot of its own), through the new `MemWorkspace::totals()`, computed once at build (an `Overlay` is immutable, so nothing needs re-weighing; `Overlay::weight`). The shared `Arc<Loaded>` base itself is not charged again; the catalog cache weighs it. An overlay of many empty files is charged `PER_FILE_BYTES` each.
> - `Overlays::forget` and `lookup` are `async` (there is no `store`: rebuilds call `Bounded::build` directly); `forget`'s three callers (`write.rs`, `mcp/changesets.rs`, `store/publish.rs`) `.await` it. `forget` is `Bounded::invalidate`. Builds go through the new `Bounded::build` (fence registered before the build, load duration and outcome counted) and lookups through `Bounded::get_if` (one request counted, a miss when the entry is not usable), so a `forget` during a build keeps its result out of the cache and the overlay cache emits the `okf.cache.requests` and `okf.cache.load.*` series. The second lookup under the build lock is uncounted.
> - The idle sweep and the least-recently-used scan are moka's: which overlay goes first when the budget is exceeded is moka's admission policy, not strict LRU, so the test asserts the count and the byte gauge.
> - `Bounded::build` checks for a newer cached build (`supersedes`) with a peek, then keeps: two steps, not atomic. Two builds finishing together can leave the older; `lookup` drops an entry older than the row it is asked for, so the next read rebuilds.
> - Config: `overlay_cache_bytes: ByteSize` (default 256 MiB, must be positive); `overlay_cache_size` is `Option<usize>`, ignored, and `Config::deprecation_warnings` reports it (the existing start-up warning path, not a new `warn!`).

- [ ] **Step 3: Run, gates, commit** — `feat: overlays bounded in bytes (M18 task 9)`.

---

### Task 10: Listings are queries; the search fan-out cap (spec Decisions 7, 8)

Runs before Task 8 and removes every enumeration of the catalog map from the listing paths, so Task 8 can delete the map. Summary fields come from the `CatalogRow` (description, owner, label); the snapshot fields (`files`, `concepts`, `last_change`) of the page being returned come from today's `App::catalog(name)`, one per listed catalog, which Task 8 replaces with `catalog_state`. Callers that still need the map after this task: the per-catalog gauges and `/healthz` (Task 8 removes them) and tests.

**Files:** Modify `src/http/api/catalogs.rs`, `src/http/api/identity.rs`, `src/http/api/labels.rs`, `src/http/api/search.rs`, `src/mcp/catalogs.rs`, `src/mcp/changesets.rs`, `src/mcp/context.rs` (`ToolContext::catalogs()` loses its listing callers), `src/config.rs`, the console views that list catalogs (find them with `grep -rnE "api\.get\('/catalogs'|/catalogs\?" src/http/ui/js`; for example `src/http/ui/js/components/unlock.js:77`), `docs/reference/` (Task 17 finishes the prose).

- [ ] **Step 1: One helper** (`src/cache/directory.rs`):

```rust
/// The catalogs `principal` reaches, one page at a time (spec Decision 7).
/// `Reach::Catalogs` returns them all in one page (bounded by the grants);
/// `Reach::All` pages through every catalog by name.
pub async fn reachable_page(&self, principal: &Principal, after: Option<&str>, limit: u32) -> Result<Page<Arc<CatalogRow>>, OkfError>;

pub struct Page<T> { pub items: Vec<T>, pub next: Option<String> }
```

`next` is the last name of the page when a row follows it, opaque to clients (base64url of the name, so a client cannot depend on it being a name). *(As shipped, `limit` and `after` apply to `Reach::Catalogs` too, and a connection list goes through `offered_page`; see the correction after Step 5.)*

- [ ] **Step 2: Failing tests**, one per endpoint: with an all-reach admin key and 7 catalogs, `?limit=3` returns 3, 3, 1 with `next` set, set, absent; a `Reach::Catalogs` key gets its catalogs in one page and no `next`. `CatalogSummary` fields are unchanged; `files`, `concepts` and `last_change` (which need a snapshot) are read through `App::loaded_catalog(row)` (today's `App::catalog(name)`, checked against the row's `catalogs.id`) for each listed catalog (Task 8 changes this to `catalog_state`, which loads at most `limit` catalogs). `/api/search` with 26 reachable catalogs and none named answers 400 with `"name the catalogs to search (at most 25 are searched by default)"`; with 25 it searches all.
- [ ] **Step 3: Implement** the endpoints over `reachable_page`: `GET /api/v1/catalogs?after=&limit=` (default 100, max 1000), identity's `catalogs` and the labels endpoint's catalog list (same parameters, `next` beside the list), MCP `list_catalogs` (an optional `after` argument and a `next` field in its result). `list_changesets` without `catalog` filters changesets by the principal in SQL (`changesets::open_for_catalogs(names)` over the reachable names; for `Reach::All`, no catalog filter) instead of scanning the map. Config: `search_fanout_max` (default 25, from 1 to 999: the cap is checked with one page of `search_fanout_max + 1` rows).
- [ ] **Step 4: The console** follows `next` where it lists catalogs (a "Load more" control, vanilla JS, as the rest of the console).
- [ ] **Step 5: Run, gates, commit** — `feat: paginated catalog listings and a search fan-out cap (M18 task 10)`.

> **Corrected (issue #688, 2026-11-19).** As shipped (read `src/cache/directory.rs`, `src/http/api/{catalogs,identity,labels,search}.rs`, `src/mcp/{catalogs,changesets}.rs`):
> - `reachable_page(principal, after, limit)` takes the client's cursor as sent and parses it itself (`parse_cursor`: trimmed, empty is none; anything not canonical unpadded base64url of a valid catalog name is `InvalidPath { path: "after" }`, a 400, before any read). It calls `offered_page(principal, connection, after, limit)`, which also narrows to an MCP connection's `X-OKF-Catalogs` list. `limit` and `after` apply to `Reach::Catalogs` (and to a connection list) too: the bounded set is read with `resolve_many`, then cut, so `?limit=` means the same for every key; with the default 100 a granted key still gets one page and no `next`. `Reach::All` reads `limit + 1` rows, so `next` is set only when a row follows (never on a last page that happens to be full). `page_size`, `DEFAULT_PAGE` (100), `MAX_PAGE` (1000) and `Page<T> { items, next }` live there too; `next` is absent (`skip_serializing_if`), not null, on the last page.
> - Snapshot fields come through `App::loaded_catalog(&CatalogRow)`, which is `App::catalog(name)` filtered to the row's `catalogs.id`: a catalog this replica has not loaded, or another incarnation under its name, is left out of its page (pages may be short; clients follow `next`). `GET /whoami` and `/api/search` use it as the "loaded" filter they had. **Task 8** replaces `loaded_catalog` (not `App::catalog(name)`) with `catalog_state`, and decides whether a catalog that does not build is listed.
> - `GET /labels` pages its `catalogs` for every caller, the every-catalog admin included (its users and teams are still whole, until Task 12); labels are told apart within a page only. *(Since Task 24, issue #749, its users and teams are scoped like everyone else's.)* `Labels::catalog_slugs` had no caller left and was removed (**Task 12**: build `Wanted` from the page's rows).
> - `list_changesets` without `catalog`: `changesets::open_for_catalogs(pool, names: Option<&[String]>, key)` (names: the connection's list, else the grants; `None` for `Reach::All`; malformed names dropped), then each row is kept only if `offers`, `check_not_suspended` and `ToolContext::check_contents_by_id(name, catalog_id, Read)` pass. `check_contents_by_id` is `admission_to` for a name and `catalogs.id` with no state (`admission_to` now calls `admission_by_id`): **Task 11** edits that body, not `admission_to`'s. `list_open` and the new function share one query.
> - The search cap counts what the key reaches (one directory page of `search_fanout_max + 1`), not what is open to it now, so suspensions and unlocks do not move it; the 400 is `InvalidPath { path: "catalogs" }`. `search_fanout_max` must be more than zero.
> - MCP `list_catalogs` takes `ListCatalogsParams { after }` and always serves `DEFAULT_PAGE`; its result has `next`.
> - The console: `api.pages(path, field)` (`core/api.js`) follows `next` with `limit=1000` for the views that need a whole list (the shell's `/whoami` and `/labels`, the unlock and entitlement pickers, import/export); the catalogs page has Load more (`#catalogs-more`, `#catalogs-stale`, committed cursor), and its maintenance drawer reads the pages until one holds the catalog.

---

### Task 11: Credential and user caches replace `KeyCache` (spec Decision 9)

The largest task. Keep `Principal`'s public shape; change where its parts come from.

**Files:**
- Create: `src/cache/credentials.rs`.
- Modify: `src/auth.rs`, `src/app.rs`, `src/http/console_session.rs`, `src/oauth/access.rs`, `src/oauth/token.rs`, `src/mcp/context.rs` (`admission_to`, whose body is now `admission_by_id(name, catalogs.id, access)`, issue #688), `src/http/api/catalogs.rs` (`create_catalog`'s suspension read), `src/http/api/unlocks.rs` (`gates.admins`), `src/http/archives.rs` (`gates.catalogs`), `src/store/keys.rs` (`key_rows::by_hash`, `by_id`), `src/store/ceiling.rs` (`ceiling_of` by id), `src/store/users.rs`, `src/store/gates.rs`, `src/invalidate.rs`, `src/config.rs`, `tests/multi_replica.rs`.

- [ ] **Step 1: Store reads by one id** (each a single query, with tests):
  - `keys::record_by_hash(pool, &[u8; 32]) -> Option<KeyRecord>` and `record_by_id(pool, &str)`, reusing `key_rows::list`'s SELECT with a `WHERE` (`src/store/keys.rs:911`).
  - `ceiling::ceiling_by_user_id(pool, &str) -> Ceiling` (`CEILING_ROWS` filtered on the user).
  - `gates::for_user(pool, user_id, now) -> UserGates { member_of: HashSet<String>, entitled: HashSet<String>, is_admin: bool, unlocks: Vec<LiveUnlock>, audit_person: Option<i64> }` and `gates::audit_credential_id(pool, key_id) -> Option<i64>`, from the same queries `gates::load_with` runs, filtered.
  - `teams::ids_of_user(pool, user_id) -> Vec<String>` (for `team` invalidations; `teams.id` is text).

- [ ] **Step 2: Failing tests** (`src/cache/credentials.rs`):
  - `a_key_is_found_with_one_query_and_then_from_cache`.
  - `a_revoked_key_is_refused_after_its_credential_invalidation`.
  - `a_self_issued_key_is_capped_by_its_users_current_ceiling`: change the user's team membership; after the `Team` invalidation the key's reach shrinks.
  - `a_key_revoked_while_its_hash_lookup_is_loading_is_not_cached`: hold a `find` (the `by_hash` load) with a test hook, revoke the key and `apply` its `Credential { key_id }` while it is held, release: that request may use the result it loaded, but the next `find` reloads and refuses the key.
  - `a_user_load_overtaken_by_a_team_invalidation_is_not_cached`: hold a `user` load, remove the user from a team and `apply` the `Team` message, release: the next `user` reads the new memberships.
  - `a_suspension_closes_the_catalog_for_every_cached_principal`: suspend the owner team; after `CatalogMeta`, `principal.require(Read, catalog)` is `Suspended` (read through the directory).
  - `an_unknown_credential_is_not_cached` (two lookups, two queries).
  - `entries_expire_after_credential_ttl` (`tokio::time::pause`).
  - Every existing test in `src/auth.rs` keeps passing.
  - `tests/multi_replica.rs`: the M16 revocation tests (`a_key_revoked_on_one_replica_is_refused_on_the_other_at_once`, the user delete, team suspension and grant revocation ones) pass unchanged with `poll_interval` and `credential_ttl` at 1 h.

- [ ] **Step 3: Implement.**

```rust
/// What a credential row says, before the user's ceiling caps it.
struct CredentialEntry { record: KeyRecord, expires_at: Option<DateTime<Utc>>, audit_credential: Option<i64> }

/// What a user row and its memberships say.
struct UserEntry { auth: UserAuth, ceiling: Ceiling, gates: UserGates, teams: Vec<String> }

/// Credentials and users on demand (spec Decision 9).
pub struct Credentials {
    by_hash: Bounded<[u8; 32], Arc<CredentialEntry>>,
    by_id: Bounded<String, Arc<CredentialEntry>>,
    users: Bounded<String, Arc<UserEntry>>,
    pool: PgPool,
    clock: Arc<dyn Clock>,
}

impl Credentials {
    pub async fn find(&self, presented: &str) -> Result<Option<Arc<Principal>>, OkfError>;
    pub async fn find_grant(&self, grant_id: &str) -> Result<Option<Arc<Principal>>, OkfError>;
    pub async fn user(&self, user_id: &str) -> Result<Option<Arc<UserEntry>>, OkfError>;
    pub async fn session_principal(&self, user_id: &str) -> Result<Option<Principal>, OkfError>;
    pub async fn apply(&self, invalidation: &Invalidation);
    pub fn mark_all_stale(&self);
}
```

- `find` and `find_grant` build the principal with today's `principal_of` logic (cap a self-issued reach by `UserEntry::ceiling`), and **not** cached as a whole: entries are cached, the cheap assembly runs per request, so a user change needs only the user entry dropped.
- `Principal`'s `suspended: Arc<Suspensions>` and `gates: Arc<AccessGates>` are replaced by `access: Arc<AccessFacts>`, where `AccessFacts` holds the user's `UserGates`, the credential's audit id, the `require_unlock` setting at assembly time, and a handle to the directory. `suspension(catalog)`, `check_not_suspended`, `admission`, `scope_of` and `access_record` read the catalog's facts through the directory row of the catalog being checked. Because the directory is async and these methods are sync today, `ToolContext` and the HTTP extractors resolve the catalog's row first (through the directory (Task 5), pinning it in `ToolContext` for the request; Task 8 later adds the state and snapshot to the same pinned entry) and pass `&CatalogRow` in: `principal.check_open(&row, access, now)`, `principal.require(access, &row)`. Change each call site listed in Verified facts accordingly; the compiler finds them.
- `apply`: `Credential { key_id }` invalidates `by_id[key_id]` and every `by_hash` entry whose `record.id == key_id` (`invalidate_where`); `User { id }` invalidates `users[id]`; `Team { id }` invalidates every user whose `teams` contains `id` (both predicate invalidations also fence in-flight loads of their cache, Task 4); `CatalogMeta` and `Settings` touch nothing here (facts are read per request from the directory and settings).
- `Keys` and `KeyCache` are deleted, along with `App::keys()`, `reload_keys_now`, `reload_moved_keys`'s key reload and `catch_up_keys`. `BearerVerifier for Keys` becomes `for Credentials`. Settings stay on the `admin_generation` reload until Task 13. Labels stay on a `Labels::load` held by `App` and reloaded on any `User`, `Team` or `CatalogMeta` until Task 12 (a temporary, explicitly bounded regression: note it in the PR).
- `session_user` and `session_principal` (`src/http/console_session.rs:333`, `:376`) lose their cache-then-database fallback: the cache loads on miss.
- `src/oauth/access.rs` `find_grant` (`:327`) calls `credentials.find_grant` once; the catch-up retry goes.
- Config: `credential_ttl` (10 s), `credential_cache_entries`, `user_cache_entries` (100,000 each).

> **Corrected (issue #689, 2026-11-19).** What shipped differs from Steps 1-3 in these ways; read `src/cache/credentials.rs`, `src/auth.rs`, `src/store/gates.rs` and `src/mcp/context.rs` for the code.
> - **Store reads.** `keys::record_by_hash(pool, &[u8; 32])` and `record_by_id(pool, &str)` return `Option<CredentialRecord { record: KeyRecord, audit_credential: Option<i64> }>`: the audit id comes in the same statement, so there is no separate `gates::audit_credential_id` call on the load path (the function exists, for tests and callers outside the cache). There is no `ceiling::ceiling_by_user_id`: the user load reads `ceiling::ceiling_in(conn, user_id)` inside its one read-only `REPEATABLE READ` transaction. `ceiling::ceilings`, `teams::suspended_catalogs`, `users::auth_by_id` and `gates::load`/`load_with` (whole-table reads) are deleted; `users::find_auth` takes any executor. There is no `teams::ids_of_user`: the teams are part of `UserGates`.
> - **`UserGates`** (`src/store/gates.rs`, `gates::for_user(conn, user_id, is_admin, now)`) names catalogs by `catalogs.id` and the owner by `teams.id`, never by name: `{ is_admin, teams: HashSet<String> (team ids), entitled: HashSet<i64> (catalog ids), unlocks: Vec<UserUnlock { id, catalog_id, expires_at }>, person: Option<i64> }`. A customer is a member of the row's `owner_team_id` or entitled to the row's `id`; an unlock matches the row's `id`. So an entitlement, membership or unlock of a catalog deleted and created again under its name never admits to the new one, without per-name invalidation. There is no `member_of` set.
> - **`UserEntry`** is `{ auth: UserAuth, ceiling: Ceiling, gates: Arc<UserGates> }` (the teams are `gates.teams`). The cache value for credentials is `Arc<CredentialRecord>` (there is no `CredentialEntry`; `expires_at` is the record's). `Credentials::new(pool, clock, CredentialLimits { credentials, users, ttl }, metrics)`; the grant cache reports as `cache="grant"` (`CacheName::Grant`), the key cache as `credential`.
> - **Assembly needs settings and labels**, which `Credentials` does not hold: `find(presented, &AccessView)`, `find_grant(grant_id, &AccessView)` and `session_principal(user_id, &AccessView)` take `AccessView { require_unlock, labels: Arc<Labels> }`, built per request by `App::access_view()`. Callers use `App::find_key`, `App::find_grant` and `App::session_principal`. `BearerVerifier` is implemented by `auth::KeyVerifier { app }`, not by `Credentials`. Task 13 makes `App::access_view` call `fresh_settings`.
> - **`Bounded::get_or_load_present`** (new in `src/cache/bounded.rs`): a loader returning `Ok(None)` is shared by the callers that missed together but never cached. That is how unknown, revoked and expired credentials, and unknown users, stay out of the caches.
> - **`Principal.facts: Arc<AccessFacts>`** (not `access`, which is the level field): `AccessFacts { user: Option<Arc<UserGates>>, credential: Option<i64>, require_unlock: bool, labels: Arc<Labels> }`. It holds labels, not a directory handle: `suspension(&row)` names the catalog and team with them (the row has no labels). Task 12 must replace `AccessFacts::labels` too. The methods take the row: `require(access, &row)`, `check_not_suspended(&row)`, `suspension(&row) -> Option<SuspendedCatalog>` (owned), `admission(&row, access, now)`, `check_open(&row, access, now)`, `require_contents(access, &row, now)`, `require_maintenance_report(&row)`; `require_level(access, name)` is public; `scope_of` is gone (`CatalogScope::of(&row)`). Two async helpers resolve the row: `principal.require_in(directory, access, name)` (reads the row only after reach and level pass, and only below admin; a missing row is not suspended, as before) and `principal.reached_row(directory, access, name)` (reach and level, then the row or `CatalogNotSelected`). `ToolContext` gained `row(&state)` (the directory row of that very incarnation: `CatalogNotSelected` when it is gone, `Busy` when the name is now another catalog), `require`, `require_contents` and `require_maintenance_report` on a state; `admit`, `admit_all`, `check_contents` and `admission_to` are `async`; `select` resolves the rows of its candidate names (argument, connection, defaults) and `selection::resolve` takes `rows: impl Fn(&str) -> Option<&CatalogRow>`. Nothing is pinned per request beyond that (Task 8).
> - **`apply`** also handles `CatalogMeta { name }`: it drops every cached credential whose reach or default names the catalog and every cached user whose ceiling does (the grants and ceilings are by name, and a delete-and-recreate sends only `CatalogMeta` and the owner's `Team`). Every indirect drop raises the cache's fence.
> - **`App`.** `reload_keys_now` is `reload_settings_now(announce)` and reloads the settings and the labels (`App::labels()`, which replaces `keys().labels()`); every non-`Catalog` message calls it (a `Credential` message too, so this replica's `admin_generation` keeps up with the M16 announcements it hears). `apply_m16_announcement()` exists from this task: the M16 subscriber marks the credential caches **and the catalog directory** stale and reloads settings and labels on an announcement it has not caught up with (an older replica's suspension, owner or maintenance change sends no message). **The poll pass, while it exists, marks the credential caches and the directory stale whenever `admin_generation` moved since its last sweep** (its own counter, `swept_generation`, so a reload by a change message or an announcement cannot hide a change no message described): its old full key reload, bounded; lib tests (which run no listener) rely on it after `app.poll()`. The metric and span keep their `okf.keycache.*` names.
> - **Task 10 (PR #712) overlap, merged.** `admission_by_id` and `check_contents_by_id` are `async` and decide by the directory row of that name and id (`ToolContext::row_by_id`, which `row(&state)` calls); `list_changesets` without a catalog reads each listed row's catalog row for the suspension check, then `check_contents_by_id`.
> - **Superseded (issue #728).** `UserEntry` no longer holds a `ceiling`, `ceiling_in` takes the catalog names, and a session's reach is `Reach::Ceiling`; see Task 19.
> - **Tests.** `tests/support/server.rs` sets `credential_ttl` to one hour beside `poll_interval` for every Redis-backed (`start_shared`/`sibling`) replica. The lib tests are in `src/cache/credentials_tests.rs`. `entries_expire_after_credential_ttl` sleeps against a 100 ms TTL (moka's real clock; Task 4's note). Pure `Principal` tests in `src/auth.rs` take rows.

- [ ] **Step 4: Run** `gate.sh narrow auth`, `gate.sh narrow console_session`, `gate.sh narrow oauth`, `gate.sh narrow --test multi_replica`; full gates; commit — `feat: credentials and users load on demand (M18 task 11)`.

---

### Task 12: Labels on demand

> **Corrected (issue #689, 2026-11-19).** The callers now read `app.labels()` (the `keys().labels()` calls were renamed in place, so the line numbers below are approximate), and `Principal::suspension` reads `principal.facts.labels` (`AccessFacts::labels`, filled from `App::access_view`): replace that too, for example by resolving the two labels where the `Suspended` error and the listing's `SuspensionInfo` are built.

> **Corrected (issue #690, 2026-11-19).** As shipped (read `src/cache/labels.rs`, `src/labels/directory.rs`, `src/cache/bounded.rs` (`get_or_load_many`)):
> - `Wanted` lives in `src/labels/directory.rs` (`user`, `team`, `catalog`, `is_empty`), `LabelLookup` in `src/cache/labels.rs`: one `Bounded<LabelKey, LabelValue>` shared by users, teams and catalogs (`DEFAULT_LABEL_CACHE_ENTRIES` = 100,000, a constant, no config key; TTL is `credential_ttl`), reached as `App::labels() -> &LabelLookup`. **`labels_for(&Wanted)` returns exactly the slugs asked for**; `labels_with_owners(&Wanted)` also adds the owner teams of the catalogs (what `catalog_choices` and the owner-team labels need). Both read every miss in one query per kind, together, through the new `Bounded::get_or_load_many(keys: Vec<K>, loader)` (a batch load kept per key unless an invalidation overtook it; absent slugs never cached). `Labels::load`, `teams_of`, `memberships` and the `from_rows` memberships argument are gone; `Labels::users()`/`teams()` remain for `LabelLookup::everyone()` (a whole, uncached read for `GET /labels` of an every-catalog admin only, whose users and teams are whole by design) and `LabelLookup::teams_of(user)` is a small uncached query (the labels endpoint's own teams and `refuse_suspended_owner`).
> - Cached values remember the row id, because messages carry ids: a `User` message drops that user's label and its personal team's, a `Team` message that team's, a `CatalogMeta` message that catalog's. `mark_all_stale` runs on reconnect, on an M16 announcement and in the poll's sweep; `App::reload_moved_settings` no longer loads labels (Task 13: settings only).
> - **`AccessFacts::labels` and `AccessView::labels` are gone.** `Principal::suspension(&row)` builds both labels from the row: `catalog_label(&row.display_name, &row.description)` and the new `SuspendedRow::team_label`, which the directory query computes (it joins the team and its personal owner). The directory drops a suspended row on a `Team` message for its owner team and on any `User` message. Nothing resolves labels at the `Suspended` error site.
> - `audit::list`/`export_lines` take `&LabelLookup` and read their page's catalog and team targets together; the CLI builds a throwaway lookup. `ToolContext::labels()` is `labels_for(&Wanted)` and `ctx.catalog_label(slug)` is `async` and fallible (every call site adds `.await?`); `TeamView::many/one` read the users of the records shown; the consent pages read the catalogs they offer.
> - The tests are `cache::labels::tests` and `bounded::tests::get_or_load_many_*`; `an_audit_page_reads_its_target_labels_once` is in `store::audit::tests`.

> **Corrected (issue #749, 2026-11-19).** `LabelLookup::everyone()` and `Labels::users()`/`teams()` are gone: `GET /labels` answers every caller, the every-catalog admin included, with its own user and teams and the page's catalogs with their owner teams, and the console reads other users' and teams' labels on demand through `GET /labels/named` (Task 24).

**Files:** Create `src/cache/labels.rs`. Modify `src/labels/directory.rs`, every `keys().labels()` caller (Verified facts list them: `src/http/api/audit.rs:142, 170`, `src/http/archives.rs:407`, `src/http/api/catalogs.rs:247, 322, 355`, `src/http/api/labels.rs:128`, `src/http/api/identity.rs:185`, `src/http/api/people.rs:943, 979, 995, 1046, 1070, 1104, 1181`, `src/oauth/authorize.rs:756, 1075, 1242, 1265`, `src/mcp/context.rs:210`), `src/invalidate.rs`, `src/app.rs`.

- [ ] **Step 1: Failing tests:** `labels_for_returns_only_the_slugs_asked_for` (one query for users, teams and catalogs together); `a_user_invalidation_drops_that_users_label`; `an_audit_page_of_50_entries_reads_labels_once`.
- [ ] **Step 2: Implement.** `Labels` keeps its accessor API (`user`, `team`, `catalog`, `catalog_display_name`, `actor`, `catalog_owner`, `teams_of`, `catalog_choices`) but becomes a view over the slugs one response needs:

```rust
/// The slugs a response will show.
#[derive(Debug, Default)]
pub struct Wanted { pub users: BTreeSet<String>, pub teams: BTreeSet<String>, pub catalogs: BTreeSet<String> }

impl LabelLookup {
    /// Labels for `wanted`: cached ones, the rest in one round trip.
    pub async fn labels_for(&self, wanted: &Wanted) -> Result<Labels, OkfError>;
    pub async fn apply(&self, invalidation: &Invalidation);
}
```

Each caller builds its `Wanted` from what it is about to render (the audit page's actors and catalogs, the team view's members, ...) and calls `labels_for` once. `Labels::users()`/`teams()` iteration callers (labels endpoint, `catalog_choices`) iterate the `Wanted` they built from the principal's reachable page (Task 10; `catalog_slugs()` is already gone, issue #688: the labels endpoint's catalogs are the page's rows). `Labels::load` (all rows) stays only for the CLI (`src/cli/audit.rs:38, 46`), which runs once per command.

- [ ] **Step 3: Run, gates, commit** — `feat: labels on demand (M18 task 12)`.

---

### Task 13: Settings on invalidation, and the M16 bridge (spec Decisions 9, 14)

> **Removed (issue #727, 2026-11-25).** The M16 bridge this task describes
> (`App::bridge_to_m16`, `announce::{BRIDGE_PAYLOAD, publish_bridge, listen}`,
> `App::apply_m16_announcement`, the `admin-generation` channel) is deleted. The
> `admin_generation` counter and its bumps stay: the settings check
> (`App::revalidate_settings`) reads it. Read the corrections below as history.

> **Corrected (issue #689, 2026-11-19).** Task 11 already renamed `reload_keys_now` to `reload_settings_now` (settings and labels), deleted `reload_moved_keys`' key reload (it is `reload_moved_settings`) and added `App::apply_m16_announcement` (marks the credential caches stale, then `reload_settings_now(false)`), which the `admin_generation` subscriber already calls. Principal assembly reads settings in one place, `App::access_view()`: make it use `fresh_settings`. The settings test should assemble through `app.find_key`, not `credentials.find`.

**Files:** Modify `src/app.rs`, `src/invalidate.rs`, `src/shared/announce.rs`, `src/http/api/mod.rs`, `src/cache/credentials.rs` and the request extractors (`fresh_settings` per request), `tests/multi_replica.rs`.

- [ ] **Step 1: Failing tests:**
  - `a_settings_invalidation_reloads_the_settings` (lib).
  - `settings_are_revalidated_after_the_ttl_without_a_notification` (lib): build the `App` with the listener disabled and a test clock, change `require_unlock` through the store directly (its `NOTIFY` reaches no one), advance the clock past `credential_ttl`, then assemble a principal (`credentials.find`): it carries the new `require_unlock`, and `app.settings()` returns the new value.
  - `concurrent_requests_past_the_ttl_reload_settings_once` (lib): ten `fresh_settings()` calls past the TTL run one `settings::load`.
  - `an_m16_announcement_marks_credentials_and_settings_stale` (lib, through `announce::run` with the test `Source`): the next `find` re-reads the key.
  - `an_administrative_http_write_publishes_the_m16_announcement` (`tests/multi_replica.rs`, Redis on): subscribe to the M16 channel and assert a message after `POST /api/v1/keys/{id}/revoke`.
- [ ] **Step 2: Implement.**
  - **Settings get the same TTL backstop as credentials (spec Decision 9).** `App.settings` becomes `RwLock<Arc<LoadedSettings>>`, `LoadedSettings { settings: Arc<Settings>, validated_at: Instant }`. New `App::fresh_settings(&self) -> Result<Arc<Settings>, OkfError>`: if `validated_at` is older than `credential_ttl`, or the settings are marked stale (reconnect, M16 announcement), reload them through a `tokio::sync::Mutex<()>` so concurrent callers run one `settings::load` (re-check the age after taking the lock), then return them. Principal assembly (`Credentials::find`, `find_grant`, `session_principal`) and the HTTP request extractors call `fresh_settings` once per request; the ~40 sync `App::settings()` readers keep reading the loaded copy, which is therefore never older than the TTL plus one request. A load failure keeps serving the old settings, logs at `warn`, and leaves `validated_at` unchanged so the next request retries.
  - `App::apply(Settings)` reloads `settings::load`. `reload_moved_keys` and the `admin_generation`/`announced_generation` atomics go; `settle` (`src/http/api/mod.rs:306`) becomes `bridge(app)`: when `[redis]` is set, `announce::publish(redis, None)` so M16 replicas reload; otherwise nothing. Every `settle` call site keeps calling it, renamed. `src/oauth/authorize.rs:1141`'s `app.poll()` becomes `bridge(app)` too.
  - `announce::listen` stays subscribed; any message calls `app.apply_m16_announcement()`: `credentials.mark_all_stale()`, `labels.mark_all_stale()` (Task 12) and the settings reload. It no longer calls `reload_keys_now` (gone in Task 11).
  - Each bridge item carries `// M16 bridge (spec Decision 14): remove in the release after 0.23.` so Task 18's follow-up issue can grep them.
> **Corrected (issue #691, 2026-11-19).** What shipped differs from Step 2 in these ways; read `src/app.rs` (`fresh_settings`, `revalidate_settings`, `bridge_to_m16`, `apply_m16_announcement`), `src/shared/announce.rs` and `src/http/mod.rs` (`check_settings`).
> - **`admin_generation` is the settings version.** `App.settings` is `RwLock<LoadedSettings { settings: Arc<Settings>, generation: i64, validated_at: DateTime<Utc> }>` (by the app's clock, so a test can move it; an age that is negative, a clock stepped back, counts as expired). `fresh_settings(&self) -> Arc<Settings>` returns no `Result`: a failed check keeps the loaded settings by design, so there is nothing to return. A check reads `admin_generation` and runs `settings::load` only when it moved (every settings write bumps it). A stale mark (`settings_marks`, raised by `App::reload_settings()` on a `Settings` message, a reconnect, an M16 announcement or a poll pass) keeps the settings stale for every caller until a check that began after it succeeds (`satisfied_marks`, written only on success): a request during an in-flight check waits for it, and a failed or cancelled check satisfies nothing. A check is bounded by `SETTINGS_CHECK_TIMEOUT` (5 s), since the middleware runs outside the routes' timeouts; past it the check counts as failed and the loaded settings serve.
> - **Per request:** one middleware on every route but `/healthz` (`check_settings`) calls `fresh_settings`, instead of each extractor; `access_view` is `async` and calls it too. `src/cache/credentials.rs` is unchanged.
> - **Labels** are Task 12's `LabelLookup` (merged first): `App::apply` passes every message to `labels.apply`, and `apply_m16_announcement`, `apply_reconnect` and the poll's sweep call `labels.mark_all_stale()`. `reload_settings_now` is gone; the `okf.keycache.reload` span and `okf_keycache_reloads_total` now count settings loads (a check that finds `admin_generation` unmoved loads nothing and counts nothing).
> - **A failed check is shared** by the callers queued behind it, whatever started it (TTL or a stale mark): stale marks are counted (`settings_marks`), and a waiter runs its own check only when a mark landed after the finished check began (`checked_marks`). A request that arrives after the failure retries.
> - **`settle` keeps its name and its poll** (the writer's read-your-writes until Task 16) and then calls `App::bridge_to_m16()`, which spawns `announce::publish_bridge(redis)` (detached, like `announce_catalog`). `src/oauth/authorize.rs`' `reload_settings_now(true)` became `app.bridge_to_m16()`. The poll pass no longer announces.
> - **The bridge payload is `announce::BRIDGE_PAYLOAD` (`"notified"`), not the empty one.** An M16 replica reads any non-number as "check now", as it reads the M16 command line's empty payload; the new command line sends the bridge payload too. A new replica ignores the bridge payload (that change also went through `NOTIFY`) and answers **every** other payload with `apply_m16_announcement`, whatever generation it names: without the `admin_generation` atomic there is nothing to compare with, and comparing could hide an M16 change behind a generation a message's reload already reached (the hole `swept_generation` closed in Task 11). `apply_m16_announcement` marks the credential and user caches and the catalog directory stale, checks the settings and reloads the labels.
> - **Tests** also cover both directions of a mixed deploy in `tests/multi_replica.rs` (`an_m16_replicas_write_reaches_a_new_replica_through_its_announcement`), the bridge payload being ignored (`the_bridge_payload_marks_nothing_stale`) and the middleware (`a_request_checks_settings_older_than_the_credential_ttl`).

- [ ] **Step 3: Run, gates, commit** — `feat: settings on invalidation; bridge to M16 replicas (M18 task 13)`.

---

### Task 14: Every replica runs a worker (#458, spec Decision 17)

**Files:** Modify `src/mail/queue.rs`, `src/config.rs`, `src/http/mod.rs`, `tests/support/server.rs`, `src/mcp/testing.rs`.

- [ ] **Step 1: Failing tests** (`src/mail/queue.rs`):
  - `a_dead_workers_job_returns_within_a_minute`: start a worker with production timing except `heartbeat = 1s` (keeping `missed_heartbeats = 6`), take a job, kill the worker task without shutdown, assert another worker sends it within 6-8 s (scaled from 60 s).
  - `stale_worker_rows_are_pruned_and_live_ones_kept`: insert an `apalis.workers` row with `last_seen` an hour ago and one referenced by a `Running` job; `prune_workers` deletes only the first.
  - `mail_worker_false_parses_with_a_warning_and_is_ignored` (`src/config.rs`).
  - The existing `shutdown_finishes_the_mail_in_hand` and `two_workers_never_send_the_same_mail_twice` pass unchanged.
- [ ] **Step 2: Implement.**
  - `HEARTBEAT_INTERVAL = 10s`, `QueueConfig::...heartbeat_interval(10s).missed_heartbeats(6)`.
  - One worker pool per process, `WORKER_POOL_SIZE = 3` (mail's LISTEN, mail's fetch/ack/heartbeat and handler writes, housekeeping's fetch/ack/heartbeat; housekeeping polls without LISTEN), exposed as `queue::WorkerPool` and passed to Task 15's worker. Update the module docs' connection arithmetic.
  - `pub async fn prune_workers(pool: &PgPool, now: DateTime<Utc>) -> Result<u64, OkfError>`: `DELETE FROM apalis.workers w WHERE w.last_seen < $1 AND NOT EXISTS (SELECT 1 FROM apalis.jobs j WHERE j.lock_by = w.id AND j.status IN ('Queued','Running'))` with `$1 = now - 10 x orphan timeout`. Called from the poll's hourly prune for now; Task 15 moves it.
  - `mail.worker`: still deserializes; `Config::validate` logs `warn!("mail.worker is deprecated and ignored: every serve runs the worker")`. `serve` always spawns the worker; tests stop it through a new `App`/`serve` constructor flag (`ServeOptions { worker: bool }`), which `tests/support/server.rs` (`serve_app`) and `src/mcp/testing.rs` set.
  - Shutdown: `worker_shutdown_grace` (default 30 s): `serve` waits for the worker's `run_until` at most that long, then logs and returns.

> **Corrected (issue #692, 2026-11-18).** What shipped differs from the bullets above in four places; read `src/mail/queue.rs`, `src/http/mod.rs` and `src/config.rs`.
> - `prune_workers` keeps every worker row that **any** job still names (`NOT EXISTS (SELECT 1 FROM apalis.jobs j WHERE j.lock_by = w.id)`), not only `Queued`/`Running` ones: `apalis.jobs.lock_by` is a foreign key with no `ON DELETE`, so deleting the row of a worker that finished jobs (`Done`, `Killed`) fails. apalis clears `lock_by` when it re-enqueues a dead worker's job, so a dead worker's row is prunable at once; the row of a worker that sent mail goes after `queue::prune` deletes those jobs (7 days). `App::prune_oauth` calls it right after `queue::prune`.
> - `WorkerPool` is `Clone` and owned by `serve`, which creates it (`WorkerPool::new(app.pool())`), passes `&WorkerPool` to `spawn_worker(app, &pool, shutdown)` and calls `WorkerPool::close().await` after the workers stop. `run_worker` no longer closes the pool, so Task 15's `spawn_worker` must take `&WorkerPool` the same way and must not close it. `housekeeping::spawn_worker(app, pool: WorkerPool, ..)` below should read `pool: &WorkerPool`.
> - The deprecation warning is not logged by `Config::validate` (it runs before the log subscriber exists). `mail.worker` is now `Option<bool>`, `Config::deprecation_warnings()` returns the message, and `serve_with` logs it at `warn`.
> - `http::serve` stays (it calls `serve_with` with `ServeOptions::default()`, `worker: true`); `http::serve_with(app, listener, shutdown, ServeOptions)` takes the flag. `ServeOptions::NO_WORKER` is what the unit-test servers use, and `tests/support/server.rs` passes `ServeOptions { worker: mail_worker, .. }`. `src/mcp/testing.rs` no longer sets `mail.worker`.
- [ ] **Step 3: Run, gates, commit** — `feat: an HA-tolerant worker on every replica (M18 task 14, closes #458)`.

---

### Task 15: Housekeeping as apalis jobs (#356, spec Decisions 15, 16)

**Files:**
- Create: `src/housekeeping.rs`, `migrations/0024_cluster_stats.sql`.
- Modify: `src/app.rs` (prune functions move), `src/http/mod.rs`, `src/telemetry/metrics.rs`, `src/mail/queue.rs` (`record_depth` returns counts instead of recording), `tests/multi_replica.rs`.

- [ ] **Step 1: Migration.**

```sql
-- One row: the cluster-wide counts the cluster_stats job computes, read by
-- every replica's gauges (spec Decision 16).
CREATE TABLE cluster_stats (
    id              boolean PRIMARY KEY DEFAULT true CHECK (id),
    open_changesets bigint NOT NULL,
    mail_queue      jsonb  NOT NULL,
    computed_at     timestamptz NOT NULL
);
```

- [ ] **Step 2: Failing tests** (`src/housekeeping.rs`):

```rust
#[tokio::test]
async fn two_replicas_scheduling_one_slot_create_one_job() {
    let db = TestDb::start_migrated().await;
    let slot = Task::PruneOauth.slot_at(fixed_now());
    schedule(&db.pool, Task::PruneOauth, slot).await.unwrap();
    schedule(&db.pool, Task::PruneOauth, slot).await.unwrap(); // the duplicate is ignored, not an error
    let jobs: i64 = sqlx::query_scalar("SELECT count(*) FROM apalis.jobs WHERE job_type = $1")
        .bind(QUEUE).fetch_one(&db.pool).await.unwrap();
    assert_eq!(jobs, 1);
}

#[test]
fn slots_start_at_their_cadence_boundary() {
    // 2026-11-18T19:52:10Z -> hourly slot 19:00:00, 5-minute slot 19:50:00, daily slot 00:00:00.
}
```

Plus one test per task that runs `run(task, &context)` against fixtures and asserts its rows are gone (moving today's `App` prune tests here), and `cluster_stats_are_reported_by_every_replica` (two `App`s, one row, both gauges equal).

- [ ] **Step 3: Implement.**

```rust
/// The apalis queue (`apalis.jobs.job_type`).
pub const QUEUE: &str = "housekeeping";

/// One housekeeping task (spec Decision 15).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Task { PruneChangesets, PruneOauth, PruneAudit, PruneWorkers, ClusterStats }

impl Task {
    pub const ALL: [Task; 5] = [ /* ... */ ];
    pub fn cadence(self) -> Duration; // 5 min, 1 h, 24 h, 1 h, 60 s
    /// The start of the cadence window containing `now`, in Unix seconds.
    pub fn slot_at(self, now: DateTime<Utc>) -> i64;
}

/// The job: which task, for which slot.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Housekeeping { pub task: Task, pub slot: i64 }

/// Pushes `task`'s job for `slot` unless one exists: the idempotency key
/// `"<task>:<slot>"` and apalis's unique index make a second push a unique
/// violation, which is ignored. On the pool, in its own statement, so the
/// violation aborts nothing else.
pub async fn schedule(pool: &PgPool, task: Task, slot: i64) -> Result<(), OkfError>;

/// Every minute until `shutdown`: schedules the current slot of every task.
pub async fn tick_until(app: Arc<App>, shutdown: CancellationToken);

/// The worker (Task 14's pool), and the handler that runs one job.
pub fn spawn_worker(app: Arc<App>, pool: &WorkerPool, shutdown: CancellationToken) -> JoinHandle<()>;
async fn run(job: Housekeeping, context: Data<HousekeepingContext>) -> Result<(), BoxDynError>;
```

- `schedule` recognizes the unique violation as `sqlx::Error::Database(e) if e.code().as_deref() == Some("23505")` and returns `Ok(())`.
- The tasks are today's functions moved from `App`: `prune_empty_changesets`, `prune_oauth` (minus its in-process once-an-hour gate, now the cadence; it keeps calling `revoke_idle_grants` before the token prune), `prune_audit` (minus its gate), `queue::prune_workers` (Task 14), and `cluster_stats`: `changesets::open_counts` summed, `queue::record_depth`'s grouped count, written to the row with `INSERT ... ON CONFLICT (id) DO UPDATE`. `prune_oauth` also deletes `Done` housekeeping jobs older than a day.
- `max_attempts(3)`; a failure returns a plain error so apalis retries.
- Each replica's gauges (`okf.changesets.open` without the `catalog` label, `okf_mail_queue_depth`) read an in-memory copy of the row that `tick_until` refreshes each minute.
- Metrics: `okf.housekeeping.runs{task,outcome}`, `okf.housekeeping.duration{task}`.
- `serve` spawns `tick_until` and the housekeeping worker and awaits both at shutdown. `poll_pass` drops the prune and count calls.

> **Corrected (issue #693, 2026-11-19).** What shipped differs from Steps 1-3 in these places; read `src/housekeeping.rs`, `src/http/mod.rs` (`serve_with`) and `src/telemetry/metrics.rs`.
> - The migration is `0024_cluster_stats.sql` (Task 3 took `0023`).
> - `schedule(pool, task, slot) -> Result<bool, OkfError>`: `true` when this push created the job, `false` when the slot already had one. It first runs `SELECT EXISTS (.. apalis.jobs WHERE job_type = $1 AND idempotency_key = $2)` and returns `false` without an insert when the slot is scheduled, so the every-minute repeats leave no ERROR lines in the Postgres log (coordinator, PR #709 review). Only two replicas racing for a new slot reach the insert together; the loser's unique violation on apalis's `idx_jobs_idempotency_key` (code `23505` and that constraint name) is ignored, and any other error is returned.
> - `run(task: Task, app: &App) -> Result<(), OkfError>` runs one task in-process (the tests call it); the apalis handler is the private `run_job(job, Data<HousekeepingContext>, TaskId)`, which records `okf.housekeeping.runs{task,outcome}` and `okf.housekeeping.duration{task}` and, on a failure, moves `run_at` back with `mail::queue::reschedule_job` (30 s doubling) before returning the plain error, so the three attempts are not spent at three consecutive 10 s polls. A task with several steps (`prune_oauth`) runs them all, logs each failure and returns the first.
> - `cluster_stats` counts with a new `changesets::open_total(pool) -> Result<i64, OkfError>` (`count(*)` over the `changesets_open` partial index), which replaces `changesets::open_counts`; nothing else used the per-catalog map. `queue::record_depth(pool, metrics)` became `queue::depth(pool) -> Result<Vec<(MailStatus, i64)>, OkfError>`.
> - The in-memory copy of the row lives in `Metrics`, not `App`: `refresh_gauges(app)` (called by each tick, after scheduling) reads the row with `read_stats(pool) -> Result<Option<ClusterStats>, OkfError>` and stores it with `Metrics::set_open_changesets` and `Metrics::set_mail_queue_depth`. `okf.changesets.open` (no labels) is registered in `Metrics::new`, so `okf_changesets_open` moved into `Metrics::FAMILIES` (present from the first scrape, zero until the first row is read). `App` lost `open_changesets`, `open_changesets()`, `count_open_changesets`, `oauth_pruned`, `audit_pruned` and the prune functions; `poll_pass` no longer prunes or counts.
> - `prune_oauth` also deletes housekeeping jobs that gave up (`Killed`, or `Failed` at `max_attempts`) 30 days after they finished, as `queue::prune` does for mail; `Done` ones go after a day.
> - `serve_with` starts the housekeeping worker and `tick_until` only with `ServeOptions.worker` (test servers that set it to `false` neither run nor schedule housekeeping). Both workers run under `supervise_worker`, which gained a `queue: &'static str` argument for its log lines; the ticker is joined, then both supervisors, then the pool is closed.
> - The worker's name is `okf-housekeeping-<host>-<pid>`, with `.2`, `.3`, ... for later workers in the same process: apalis holds an advisory lock on each registered name, and several test servers in one process would otherwise share one.
> - The housekeeping worker polls every 10 s (`housekeeping::POLL_INTERVAL`) with `WorkerTiming::SERVE`'s heartbeat and restart delays.

- [ ] **Step 4: Run, gates, commit** — `feat: housekeeping runs once per cluster as apalis jobs (M18 task 15, closes #356)`.

---

### Task 16: Remove the poll loop and per-catalog series (spec Decisions 18, 19)

**Files:** Modify `src/app.rs`, `src/http/mod.rs`, `src/config.rs`, `src/telemetry/metrics.rs`, `src/cli.rs`, `tests/support/server.rs`, any test that calls `app.poll()`.

- [ ] **Step 1:** Delete `App::poll`, `poll_pass`, `poll_until`, the `polling` mutex, the `okf.poll` span and `poll_duration` metric, `catalog_refreshes` / `okf_catalog_refreshes_total` and `RefreshResult` (the per-catalog gauges went in Task 8; since issue #686 nothing records `okf_catalog_refreshes_total`, and `poll_pass` only reloads the key cache). `settle`'s remaining job, the writer's own key-cache reload, can move onto `invalidate::capturing` and `App::apply_own_writes` (issue #686), which already give `/api/v1` writes read-your-writes for the catalog directory. Remove them from `Metrics::FAMILIES`.
- [ ] **Step 2:** `poll_interval` still parses and logs `warn!("poll_interval is deprecated and ignored")`.
> **Corrected (issue #689, 2026-11-19).** Since Task 11 the poll pass also marks the credential caches and the directory stale whenever `admin_generation` moved since its last sweep (`swept_generation`): that goes with the poll (the sweep marks the label cache stale too since issue #690), and so must the backstop for older replicas' silent changes (Task 13's M16 bridge marks the directory and credentials stale on their announcements). Lib tests run no listener, so a test that polled to see a credential, user or catalog-row change must apply the write's messages instead (`src/cache/credentials_tests.rs`'s `apply_messages_of` shows how, with `invalidate::testing::invalidations_of`).

> **Corrected (issue #691, 2026-11-19).** The poll pass also checks the settings (`reload_settings()`) and reads `admin_generation` itself for the labels and the sweep. `settle` (`src/http/api/mod.rs`) still polls and then calls `App::bridge_to_m16()`: Task 16 removes only the poll from it, keeping the bridge call at every call site until the bridge goes. Lib tests that poll to see a settings change (`src/http/api/me.rs`, `src/app.rs`) must call `app.reload_settings()` or apply a `Settings` message instead. Readers of `App::settings()` outside an HTTP request get no per-request check; `housekeeping::prune_audit` already calls `fresh_settings`, and any other background reader added later must too, since without the poll nothing else refreshes them for it.

- [ ] **Step 3:** Every test that called `app.poll().await` to make a change visible now relies on `NOTIFY`: replace with a bounded wait helper `app.until_applied(|app| ..)` (lib) or `eventually` (integration). `grep -rn "\.poll()" src tests` must find nothing.
- [ ] **Step 4:** Full gates; commit — `feat: remove the poll loop (M18 task 16)`.

> **Corrected (issue #694, 2026-11-19).** As shipped:
> - `settle` is deleted, not kept: every call site calls `app.bridge_to_m16()` directly (each marked `// M16 bridge (spec Decision 14)`, so Task 18's grep finds them). The writer's read-your-writes was already `invalidate::capturing` + `App::apply_own_writes` on every HTTP route (issue #686), and it covers the credential, user and label caches and the settings mark too, so nothing replaced the poll there.
> - The `swept_generation` sweep is not replaced by another sweep: a lost message is bounded by each cache's TTL (`credential_ttl` for credentials, users, labels and settings; `catalog_ttl` for directory rows, and so for snapshots), as spec Decision 13 says. `src/app.rs`'s `a_change_no_message_described_is_read_after_the_ttls` proves it with raw SQL changes and no message.
> - `poll_interval` is `Option<Duration>` (default unset); any value, zero included, parses, and `Config::deprecation_warnings` names it (`serve` logs it), as for `mail.worker`.
> - There is no `app.until_applied` or `eventually` helper. Lib tests (no listener) use `invalidate::testing::applied(app, write)`, which collects the write's messages through `NOTIFY` (`messages_of`) and applies them with `App::apply`. Integration tests use `tests/support/server.rs`'s `applied(app, write)`, which runs the write under `invalidate::capturing` and calls `App::apply_own_writes` (deterministic; the server's listener applies the same messages later). A poll after creating a key, user or grant was dropped (they load on first use, and unknowns are never cached); a raw-SQL change, which sends no message, is followed by `App::apply_reconnect()`.
> - The Grafana example's three poll panels went, and the refresh query left the announcements panel.

---

### Task 17: Docs

**Files:** `docs/operations.md`, `docs/reference/` (the API and tool references for the paginated listings, the search cap, the new settings and the removed ones), `docs/superpowers/specs/2026-10-26-okf-mcp-server-design.md` (the "In-memory snapshot" section points to the M18 spec), `changelog/`.

> **Corrected (issue #691, 2026-11-19).** `docs/operations.md`'s "The key cache is announced" paragraph (and the command line's "empty message") is out of date since Task 13: the `admin-generation` channel now carries only the M16 bridge (`notified` from milestone 18 binaries), and settings are checked after `credential_ttl`.

- [ ] **Step 1:** In `docs/operations.md`:
  - a "Caches" section: each cache, its setting, what bounds it, its metrics, and how to size `catalog_cache_bytes` from `okf.cache.bytes` and hit ratios;
  - "Change propagation": the change feed, the 10 s TTL bound, what a listener reconnect does;
  - replace "Pruning" with "Housekeeping": the five tasks, cadences, one run per slot, `okf.housekeeping.runs`;
  - "Postgres connections": per replica `database_max_connections` (requests) + 1 (the invalidation listener's own pool) + 3 (the worker pool); total for N replicas `N × (database_max_connections + 4)`, as in spec Decision 17;
  - the mail worker: every replica, at-least-once delivery, orphan recovery about 1 minute, `worker_shutdown_grace` matched to `terminationGracePeriodSeconds`;
  - optional ingress hashing on `Mcp-Session-Id` (nginx `hash $http_mcp_session_id consistent;`), what it buys, and that nothing depends on it;
  - Grafana notes: database-wide gauges with `max()`, the removed per-catalog series and their replacements.
- [ ] **Step 2:** A release-notes draft in the changelog entry: removed metrics, paginated responses (`next`), the search fan-out cap, deprecated settings (`poll_interval`, `overlay_cache_size`, `mail.worker`), the new settings with defaults.
- [ ] **Step 3:** Commit — `docs: bounded memory operations and reference (M18 task 17)`.

> **Corrected (issue #695, 2026-11-19).** As shipped: `docs/operations.md` has a "Caches and change propagation" section (the sizing text, the change feed, the lost-message bounds, ingress hashing and the Grafana notes), the connection formula in "4. Postgres", the cache rows of the metric reference and an "Upgrades" entry for the release; "Housekeeping" and the mail worker text were already written by Tasks 14 and 15. The poll prose was rewritten for the change feed and the TTL backstop (the heads-ups on #695 listed the passages). `docs/reference/tools.md` was regenerated by Tasks 10 and 13 and needed no change. The release-notes draft is in `changelog/2026-11-19T13-41-51Z-m18-task-17-docs.md`.


---

### Task 18: Scale test, smoke test, milestone changelog

**Files:** Create `tests/scale.rs`. Modify `README.md`, `changelog/<ts>-m18-milestone.md`.

- [ ] **Step 1: `tests/scale.rs`** (`#[ignore]`, run in the close-out with `gate.sh narrow --test scale -- --ignored`):
  - seed 1,000,000 catalogs (one tiny document each) and 1,000,000 users with one key each, by `COPY`/`generate_series` SQL, not the store functions;
  - assert `App::load` completes in under 5 s and holds no catalog;
  - with `catalog_cache_bytes` set to fit 1,000 tiny catalogs, read 5,000 distinct catalogs; assert `okf.cache.bytes{cache=catalog}` never exceeds the budget and the process's RSS growth (from `/proc/self/statm`) stays under budget plus 200 MiB;
  - authenticate 10,000 distinct keys; p99 under 20 ms on the test machine; `credential` cache entries bounded by `credential_cache_entries`;
  - `GET /api/v1/catalogs` for an admin key: first page under 50 ms.
  Record the measured numbers in the milestone changelog.
- [ ] **Step 2: Smoke test** (by hand, documented in the changelog): two replicas against one Postgres, publish on one, read on the other; revoke via the CLI, refused on both; kill one replica's listener connection and repeat.
- [ ] **Step 3:** README feature list; the milestone changelog; a follow-up issue "Remove the M16 bridge and deprecated settings" listing every `M16 bridge` comment.
- [ ] **Step 4:** Commit — `docs: milestone 18 changelog and scale results (M18 task 18)`.

> **Corrected (issue #696, 2026-11-19).** As shipped (read `tests/scale.rs` and the milestone changelog `changelog/2026-11-19T14-34-24Z-m18-milestone.md`):
> - The test seeds with `INSERT ... SELECT ... generate_series`, gives the million catalogs to a team nobody is in, and runs best in release mode: `gate.sh narrow --release --test scale -- --ignored --nocapture`. A user in a team that owns a million catalogs loads a million-entry ceiling (about 820 ms for the first `authenticate`; issue #728), which would have measured the ceiling, not the caches.
> - The cache gauge names are `okf_cache_bytes{cache="catalog"}`, `okf_cache_entries{cache="catalog"}` and `okf_cache_entries{cache="credential"}`; the weight is 1 KiB per catalog, 1 KiB per file and 8 per stored byte, so the budget for 1,000 tiny catalogs is `weight(len, 1) * 1000`. `App::load` is `load_with_metrics` here, so the gauges can be read.
> - The listing bound is split. The first `GET /api/v1/catalogs` of a default page (100 catalogs) loads 100 snapshots one after another and takes about 90 ms; the plan's 50 ms holds for the page once loaded (measured 1.5 ms). The test asserts 200 ms cold and 50 ms warm. Loading the page's snapshots concurrently would bring the cold figure down; it is not done here.
> - Credential entries are asserted at or under `credential_cache_entries` (set to 5,000 for the run, below the 10,000 keys read).
> - The smoke test needs no Redis: the change feed carries it.
> - The bridge follow-up is issue #727.
> - Maintainer accepted the revised bound on 2026-11-19. Loading a page's snapshots concurrently is #732.

> **Corrected (issue #728, 2026-11-19).** Since Task 19 the million catalogs belong to the administrator's own team (the personal team of `support::server::OWNER`, the admin key's user), which is what the test now measures: the administrator's first authentication is asserted under 20 ms (measured 3.20 ms in a release run on 2026-11-19, 15 µs again, against about 820 ms before; the same run gave 10,000 keys a p99 of 0.55 ms with 5,000 credential entries at the 5,000 limit). The measured `App` sets `credential_ttl` to one hour, so no entry expires during the run, and the credential entries are asserted in the limit region (at least 90 % of `credential_cache_entries`) as well as at or under it: the bound, not the TTL, holds the count.

---

### Task 19: A user's ceiling is resolved per catalog (issue #728, 2026-11-19)

Found by Task 18's scale test: `Credentials::load_user` read the user's whole ceiling (`ceiling::ceiling_in`, one row per catalog the user's teams own or are entitled to) and `UserEntry` kept it, so the first `authenticate` of a user whose team owns a million catalogs took about 820 ms and cached a million-entry map (the user cache bounds entries, not bytes). As shipped (read `src/store/ceiling.rs`, `src/auth.rs`, `src/cache/credentials.rs`, `src/cache/directory.rs` and `src/store/catalogs.rs`):

- **Per catalog.** `CatalogRow` carries `member_access`; `UserGates::entitled` is `HashMap<i64, Access>` (catalog id to the entitlement's level). `ceiling::level_on(&row, &gates)` is the ceiling on one catalog: `member_access.level()` when the row's `owner_team_id` is in `gates.teams`, merged with `gates.entitled[row.id]`, the higher winning: what `CEILING_ROWS` answers for that catalog. `ceiling::ceiling_over(rows, gates)` is that over a set of rows.
- **`UserEntry` is `{ auth, gates }`**: no `ceiling`. The user load is the user row and the gates' four queries, each bounded by the user's teams, entitlements and unlocks. A `CatalogMeta` message no longer drops users (a `member_access` or owner change reaches the check through the directory row, which that message drops; membership and entitlement changes send `Team` and `User` as before).
- **Self-issued credentials** are capped at assembly as before (`principal_of` and `cap` unchanged), by `ceiling_over` of the rows of the credential's own catalogs (`CatalogDirectory::resolve_many`): `Credentials::new(pool, directory, clock, limits, metrics)` takes the application's `Arc<CatalogDirectory>` (`App::directory` is now an `Arc`).
- **Sessions.** A non-admin console session's reach is `Reach::Ceiling` (no payload; resolved from `facts.user`), and its `access` is `Read` (it grants nothing by itself; `GET /whoami` shows `ceiling::highest`, one `EXISTS` row). `Principal::for_session(user, facts)` lost its ceiling argument. Name-only checks cannot decide a ceiling, so they fail closed for it (`level`, `reaches`, `require_level`), and every check a session can reach takes the row: `level_at`, `reaches_row`, `require_level_at`, `require(access, &row)`, `require_export(&row)`, `require_in` and `reached_row` (which resolve the row first for a ceiling); `precheck_level` is the by-name check run before a row's lookup (a ceiling passes it, then `require` decides); `excludes(name)` is true only for a by-name reach without the name. `ToolContext::offers` takes a `&CatalogRow`; `candidate_rows`, `selection::resolve`'s `available` and `list_changesets` check rows. `only_reachable` (the single-catalog fallback) is `None` for a ceiling: every HTTP route a session calls names its catalog.
- **Listings.** `offered_page` for `Reach::Ceiling` reads `catalogs::ceiling_page(teams, entitled, after, limit + 1)`: per owner team, a `LATERAL` range scan of the new partial index `catalogs_member_reach (owner_team_id, name) WHERE member_access <> 'none'` (migration `0025_catalogs_member_reach.sql`), plus the entitled ids, ordered and cut to the page. `ceiling::highest` seeks writable catalogs through a second partial index, `catalogs_member_write (owner_team_id) WHERE member_access = 'write'` (migration `0026`). Both are built `CREATE INDEX CONCURRENTLY IF NOT EXISTS` in `-- no-transaction` migrations, one statement each (a multi-statement query is an implicit transaction); `db::migrate` drops either one left INVALID by a failed build before the migrations run, under an advisory lock (`CONCURRENT_INDEXES`, `drop_invalid_indexes`). A connection list's names are checked on their rows.
- **Named reads.** `ceiling::ceiling_in(conn, user_id, names)` reads only the named catalogs (`NAMED_CEILING_ROWS`): `grants::create_self_issued` checks a consent with it under the user's lock, and `POST /me/keys` checks the new key's catalogs with it. `ceiling::ceiling_of(pool, user)` (whole) remains for the pages that show the whole ceiling, read on request and never cached: `GET /me`, `GET /session` (which no longer reads the principal's reach or the cache) and the consent page. Those answers still grow with the ceiling; paging them is #736.

  > **Corrected (issue #736, 2026-11-19).** Since Task 21 no request reads `ceiling_of`: it is test-only (`#[cfg(test)]`), the reference the bounded reads are checked against. `GET /me`, `GET /session` and the consent page read a page of the ceiling instead (Task 21).
- **Tests.** `cache::credentials::tests::the_per_catalog_ceiling_answers_what_the_whole_ceiling_did` (owner team at each `member_access`, entitlements below, at and above it, an admin user, a user with neither; session levels, self-issued caps, `ceiling_in`, `highest` and the paged session listing against `ceiling_of`) and `a_user_whose_team_owns_many_catalogs_loads_a_small_entry` (50,000 catalogs seeded by SQL: the load's statements return a handful of rows, counted from `sqlx::query` events; the entry's `Debug` text stays small). `a_cached_session_follows_member_access_and_owner_changes` (a warmed session and row; `member_access` write, read, none, write, then the owner moved to a team the user is not in, each applied through its change messages), `store::ceiling::tests::highest_examines_no_read_only_catalog` (`EXPLAIN ANALYZE` of `HIGHEST_SQL` over 50,000 read-only catalogs examines under 100 rows) and `db::tests::a_failed_concurrent_build_is_rebuilt_by_the_next_migrate`. `tests/scale.rs`: see the Task 18 note above.

### Task 20: `GET /whoami` reads the caller's key row by id (issue #737, 2026-11-19)

Found while fixing #728: `whoami` read every key (`keys::list`) and `find`-ed the caller's, so its cost and its transient memory grew with the number of keys and grants. As shipped (read `src/http/api/identity.rs`): for a key or grant the handler reads the one row with `keys::record_by_id(pool, principal.key_id)` (the same statement the credential cache loads a grant with). The answer is unchanged, including a row that is gone (no name, no user) and a row revoked since the credential was cached (still named, as `record_by_id` returns revoked rows like `list` did). The user still comes from that row, not the cached principal. A session has no key row and reads none.

- **Test.** `identity::tests::whoami_reads_the_callers_key_not_every_key` seeds 50,000 other keys by SQL and asserts the warmed request's statements return under 50 rows (`RowsReturned`, now shared in `src/rows_returned.rs`, counts `sqlx::query` events' `rows_returned`). The existing whoami tests cover the answer for a key, a key with a user, an unassigned key, a grant (`oauth::access`) and a session (`console_session`).
- **Other whole-table reads.** `me::own` (`/me/keys` and `/me/grants` writes), `keys::managed` (admin key writes), `keys::list_keys`, `people::user_detail`'s key list and `me::owned` read every key. They are explicit listing or per-action admin paths, not the per-request hot path; filed as a follow-up.

### Task 21: `/me`, `/session` and the consent page page the ceiling (issue #736, 2026-11-19)

Found while fixing #728: three pages still read `ceiling_of` and returned or rendered the whole ceiling, so their answers (and a consent's pending authorization) grew with the catalogs a user's teams own. As shipped (read `src/store/ceiling.rs`, `src/http/api/me.rs`, `src/http/api/identity.rs`, `src/oauth/authorize.rs`, `src/oauth/consent.rs` and `src/http/ui/js/components/ceiling.js`):

- **One bounded read.** `ceiling::page(executor, user_id, prefix, after, limit) -> CeilingPage { entries, next }`: the ceiling's catalogs whose names start with `prefix`, after `after`, in byte order, at most `limit`, each at `ceiling_of`'s level. `PAGE_SQL` is one `LATERAL` range scan per owner team of the new partial index `catalogs_member_reach_bytes (owner_team_id, name COLLATE "C") WHERE member_access <> 'none'` (migration `0027`, concurrent like `0025`; added to `db::CONCURRENT_INDEXES`), plus the user's entitlements, cut to the page. Byte order makes a prefix a range (`[prefix, prefix || '~')`) whatever the database's collation. `ceiling::top_level(pool, user_id) -> Option<Access>` is a one-entry page plus `highest`. `ceiling_of`, `ceiling_rows` and `CEILING_ROWS` are test-only.
- **API.** `GET /me` drops `ceiling` for `ceiling_access` (`read`, `write` or `null`). New `GET /me/ceiling?prefix=&after=&limit=` answers `{ceiling: [{catalog, access}], next}` (cursor as `cache::directory::cursor_of`, page size as `page_size`). `GET /session` drops `ceiling`. `GET /whoami` gains `levels` (`principal.level_at(row)` for each row of its page). `GET /me/keys` and `GET /me/grants` gain `ceiling`, the ceiling on their live self-issued credentials' catalogs (`ceiling_in`).
- **Consent.** `UserConsent` gains `prefix`, `next` and `whole` (`serde(default)`, so an entry from before reads as a first page that may have more). The first page is `ceiling::page(.., CONSENT_PAGE = 50)`. Two new actions, `find` (the `find` box's prefix, trimmed and lowercased) and `more` (the page after the one shown), spend and replace the nonce like `consent`, carry the ticked catalogs (`ticked_levels`, checked against the offer; at most `MAX_CARRIED = 50`, else the page again with `TOO_MANY_CARRIED`), re-read the carried catalogs with `ceiling_in` (dropped when no longer reached, lowered with the ceiling), read the next page, and store the new offer in one `Ephemeral::update` that checks the replacement nonce is still current (`browse_offer`, `browse_to`, `consent_browse`). The offer holds at most `CONSENT_PAGE + MAX_CARRIED` catalogs. A lone catalog is preselected only when the offer is the whole ceiling (`whole`). Approve still checks the choice against the offer (`chosen_levels`), and `grants::create_self_issued` still decides under the user's lock.
- **Console.** `store.levelOn` reads `whoami.levels`; `freshAccount` no longer touches the session. `components/ceiling.js` (`ceilingPager`) pages `/me/ceiling` with a Find box and Load more (committed rows, prefix and cursor; numbered reads; a failed read keeps the cursor and marks the list out of date). The Account page's ceiling table and New key's picker use it; New key keeps its ticks in a map across finds, its Default offers the chosen catalogs, and its create is one pending write. My keys and My grants cap notes read their answers' `ceiling`.
- **Tests.** `oauth::authorize::tests::walking_the_consent_pages_offers_exactly_the_whole_ceiling` (equivalence with `ceiling_of`, by Next catalogs and by Find), `a_find_carries_the_ticked_catalogs_into_one_grant`, `a_find_rechecks_what_it_carries_and_spends_the_page`, `a_find_carries_at_most_max_carried_catalogs`, `a_team_owning_many_catalogs_keeps_each_page_small` (50,000 catalogs: `GET /me`, `GET /session`, `GET /me/ceiling`, the consent page and a Find each return under 1,000 rows, counted with `rows_returned::counting`), `store::ceiling::tests::a_ceiling_page_examines_a_page_not_the_ceiling` (`EXPLAIN ANALYZE` of `PAGE_SQL`: first, continued, by prefix, and a prefix nothing starts with), `me::tests::the_ceiling_is_paged_and_found_by_prefix`, `state::tests::a_consent_stored_before_paging_reads_back`; browser journeys `account::the_ceiling_is_paged_with_a_find_box`, `self_service::a_member_picks_key_catalogs_across_finds`, `self_service::a_ticked_catalog_lowered_meanwhile_is_created_at_its_new_level` (a ticked level the ceiling lowered meanwhile is lowered in New key's map too, so Create sends what the select shows; review of PR #743) and `consent::a_person_finds_another_catalog_and_approves_both`.

### Task 23: The housekeeping ticker stops promptly at shutdown (issue #748, 2026-11-19)

`housekeeping::tick_until` raced only `ticks.tick()` against shutdown, so a tick blocked on a lock (`schedule` on `apalis.jobs`, or the gauge read) was never dropped and `serve_with`'s await of the ticker could outlive `worker_shutdown_grace`. The tick is now wrapped in `shutdown.run_until_cancelled(...)` and the ticker returns when it is cancelled. Dropping a tick midway is safe: scheduling is idempotent per slot and the gauge refresh only reads. Test: `the_ticker_stops_at_shutdown_while_a_tick_is_blocked`.

### Task 24: Admin label reads are scoped to the entities shown (issue #749, 2026-11-19)

Found by the review bot on milestone PR #747: for an every-catalog admin, `GET /labels` read every user and team (`LabelLookup::everyone()`, uncached) on every catalog page, and the console's `refreshIdentity()` follows every page, so one identity refresh with 1M catalogs made about 1,000 whole-population reads. As shipped (read `src/http/api/labels.rs`, `src/http/ui/js/core/labels.js`, `src/http/ui/js/components/picker.js`):

- **`GET /labels`** answers every caller the same way: its own user and teams (`LabelLookup::teams_of`), and the page's catalogs with their owner teams (`labels_with_owners`). `LabelLookup::everyone()` and `Labels::users()`/`teams()` are removed.
- **`GET /labels/named?users=&teams=`** (new; comma-separated slugs, at most `NAMED_MAX` = 100 of each, `deny_unknown_fields`): `{users, teams}` through `labels_for`. An admin (`require_reader`, the check of `GET /users` and `GET /teams`, which already answer every user's and team's label) gets every one that exists; anyone else only its own user and its own teams, so no caller receives a label it could not read before.
- **Console.** `core/labels.js` keeps the shell's table and an on-demand table; `ensureLabels({users, teams})` reads what neither holds (or what was read before the last refresh) in requests of at most 100 of each, never rejects (a failed read leaves the slugs), and `forgetLabels()` empties both on sign-out. The keys and sign-in code tables and the admin Unlocks table call it with their rows' users before drawing. Pickers label users and teams from their own rows (`userChoices(rows)`, `teamKindText` reads the row's `label` and the personal owner's member label); `namesOf` is gone. The user drawer's messages use the drawer's `user.label`. The account page's teams and Browse's owner team still come from the shell's table.
- **Tests.** `labels_tests::labels_scope` (the admin's page holds its own user and teams), `labels_named_answers_an_admin_any_and_others_only_their_own`, `labels_named_refuses_a_typo_and_too_many_slugs`, `an_admin_labels_page_reads_a_bounded_number_of_rows` (50,000 users and personal teams seeded by SQL; the cold admin page returns under 100 rows, a full named request under 250, counted with `rows_returned::counting`); browser journeys `keys::the_keys_table_reads_the_labels_of_the_users_it_shows_only`, `keys::a_failed_labels_read_still_draws_the_keys_by_slug`, `keys::a_late_labels_answer_never_replaces_a_newer_one` (each entry keeps the generation it was read in, and an older answer never replaces a newer entry; review of PR #754), `unlocks::an_admin_reads_another_staff_members_unlock_by_label`, and the existing picker and keys label journeys.
- **Not changed.** `GET /users` and `GET /teams` (whole admin listings, with labels) and the shell following every `/whoami` and `/labels` page are as before: each page is now bounded, but a refresh still reads every page.

## Self-review

- **Spec coverage.**
  - Decision 1 (one milestone): Scope.
  - Decision 2 (catalog cache): Tasks 4, 8 (weight factor measured in 8).
  - Decision 3 (pinning): Task 8.
  - Decision 4 (locks): Task 6.
  - Decision 5 (overlays): Task 9.
  - Decision 6 (directory): Task 5.
  - Decision 7 (listing): Tasks 5 (queries), 10 (endpoints).
  - Decision 8 (fan-out cap): Task 10.
  - Decision 9 (credential caches): Tasks 11, 12, 13.
  - Decision 10 (change feed): Tasks 1, 2.
  - Decision 11 (epochs): Tasks 4, 5.
  - Decision 12 (reconnect): Tasks 1, 4 (`mark_all_stale`), each cache's `apply_reconnect`.
  - Decision 13 (TTLs): Tasks 5, 8, 11.
  - Decision 14 (mixed versions): Task 13.
  - Decision 15 (housekeeping, #356): Task 15.
  - Decision 16 (gauges): Task 15.
  - Decision 17 (#458): Task 14.
  - Decision 18 (poll removed): Task 16.
  - Decision 19 (metrics): Tasks 1, 4, 15, 16.
  - Decision 20 (audit rows): Task 3.
  - Testing section: each task's tests; the scale test in Task 18.
  - Docs section: Task 17.
- **Names used across tasks:**
  - `invalidate::{Invalidation, Message, CHANNEL, invalidate, listen}`, `App::{apply, apply_reconnect}`;
  - `cache::{Bounded, CacheName, Limit}`, `Bounded::{get_or_load, get_or_revalidate, peek, invalidate, invalidate_where, mark_all_stale, reweigh, with_weigher, with_metrics}`;
  - `cache::directory::{CatalogDirectory, Page}`, `CatalogDirectory::{resolve, resolve_many, reachable_page, apply}`; `store::catalogs::{CatalogRow, row_by_name, row_by_id, rows_by_names, page}`;
  - `cache::catalogs::CatalogCache::{get, catch_up, refreshed}` (issue #686: `apply` became `catch_up`); `App::{catalog_row, catalog_state, publish_lock, directory}`;
  - `cache::credentials::{Credentials, CredentialEntry, UserEntry, AccessFacts}`; `cache::labels::{LabelLookup, Wanted}`;
  - `housekeeping::{QUEUE, Task, Housekeeping, schedule, tick_until, spawn_worker}`; `mail::queue::{WorkerPool, prune_workers}`;
  - `App::bridge_to_m16`, `announce::{BRIDGE_PAYLOAD, publish_bridge}`, `App::apply_m16_announcement`, and every line marked `M16 bridge (spec Decision 14)` (Task 13; corrected in issue #691).
- **Known risk.** Task 11 changes the signatures of `Principal::require` and `check_open` to take a `&CatalogRow`, touching about 40 call sites across HTTP and MCP. It is the largest PR; if review shows it is too big, split it as 11a (credential and user caches, principals still carrying a suspension snapshot built per request from the directory) and 11b (the `&CatalogRow` signatures).

## Open questions for the maintainer

None blocking. To confirm when filing the epic: the defaults for `catalog_cache_bytes` (1 GiB) and `search_fanout_max` (25), which Task 18's scale test will inform.
