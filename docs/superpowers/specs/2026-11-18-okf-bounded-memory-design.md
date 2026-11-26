# okf-mcp bounded memory: design

Status: designed 2026-11-18; to be delivered in milestone 18. Folds in
issues #458 (HA-tolerant mail worker) and #356 (cleanup loops on apalis).

## Goal

A replica's memory, startup time and background database load stop
growing with the total number of users and catalogs. The design target is
about 1M users and 1M catalogs, of which a small fraction is active at
once, served by N identical replicas behind one Service with no session
affinity required.

Concretely:

- **Catalog snapshots are loaded on demand** into a bounded cache, and
  evicted when unused. Startup loads none.
- **Credentials and users are loaded on demand** into bounded caches. A
  change reloads only what it touched, never the whole key cache.
- **Listing catalogs is a query**, not a scan of an in-memory map.
- **Changes reach every replica through targeted invalidations**, sent
  with Postgres `NOTIFY` inside the transaction that makes the change,
  with a TTL backstop on every entry.
- **Cluster-wide housekeeping runs once per cluster** as apalis jobs, not
  on every replica's poll loop.
- **Metric cardinality is bounded**: no series per catalog.

The poll loop is removed.

## Why things grow today

| State | Where | Grows with | Reloaded |
|---|---|---|---|
| Every catalog's snapshot | `App::catalogs` (`src/app.rs`), built by `App::poll_pass` | all catalogs' content | the changed ones, every `poll_interval`; all of them at startup |
| The catalog list | the same map, scanned by `list_catalogs`, `GET /api/catalogs`, `/api/search`, identity, labels | all catalogs | every pass, one row per catalog per replica |
| The key cache | `KeyCache::read` (`src/auth.rs`): every key, grant, user, ceiling, label, suspension and gate | all users and keys | whole, on every `admin_generation` bump anywhere |
| Per-catalog metrics | `okf.catalog.concepts`, `okf.catalog.generation`, `okf.changesets.open`, `okf_catalog_refreshes_total` | all catalogs | every scrape |
| Housekeeping | `prune_*`, `count_open_changesets`, `mail::queue::record_depth` in `App::poll_pass` | replicas | every pass on every replica |

With three replicas this already shows as three times the poll passes and
catalog refresh counts. At 1M catalogs a replica would need the whole
corpus in RAM, and a key created anywhere would reload every user's
credentials on every replica.

## Non-goals

- **Partitioning catalogs across replicas** (routing each catalog to an
  owning replica). Memory per replica is bounded by the active set, not
  divided by N. Partitioning can be layered on later if the active set
  outgrows one replica's budget; the cache metrics below show when.
- **Requiring session affinity.** Everything is correct without it.
  Hashing on `Mcp-Session-Id` at the ingress is documented as an optional
  optimisation (fewer cold loads, roughly 1/N of the active set per
  replica).
- **Moving snapshots or search into Redis or Postgres.** Reads stay on an
  in-process snapshot, and every `okf` analysis is unchanged.
- **Shared rate limits (#357).** Unchanged.
- **Changing the mail retry window or backoff** (the mail queue's
  Decision 11).

## Decisions

### 1. One milestone for catalogs, credentials, metrics and housekeeping

All five sources of growth are fixed together, so the milestone ends with
no per-replica state proportional to users or catalogs.

### 2. Catalog snapshots live in a bounded, weighted cache

`CatalogCache` replaces the `Catalogs` map. It is a moka async `Cache`
keyed by catalog id, whose values are today's `Arc<CatalogState>`,
unchanged inside.

- **Weight** is an estimate of the entry's resident bytes: the sum of its
  stored file sizes times a factor measured during implementation (Task:
  measure on the real catalog and a synthetic large one, record the factor
  and the method in this spec). The cache's capacity is
  `catalog_cache_bytes`.
  > **Corrected (issue #686, 2026-11-19).** The weight is
  > `1024 + 1024 * files + 8 * stored_bytes`, saturating at `u64::MAX` (moka is given it in coarser units for a budget past 4 GiB, issue #756)
  > (`cache::catalogs::{PER_CATALOG_BYTES, PER_FILE_BYTES, WEIGHT_FACTOR,
  > weight}`). Content alone would charge nothing for an empty catalog or
  > for many empty files, whose paths, revisions, commit times and map
  > entries still take memory (review of PR #714). Method
  > (`tests/snapshot_weight.rs`, `--ignored`): a counting
  > `#[global_allocator]` in its own test binary; catalogs imported into
  > Postgres; after a warm-up load of a small catalog, the bytes allocated
  > minus freed across one `CatalogState::load` whose result is still held.
  > Measured: one empty document, 1,065 B; 5,000 empty documents at paths
  > like `notes/topic-1/concept-00001.md`, 4,926,196 B (985 B per file, so
  > about 80 B per catalog besides); 500 synthetic concepts (frontmatter,
  > headings, prose, links, about 850 B each), 3,870,753 B for 424,010 B of
  > content; 5,000 of them, 38,696,461 B for 4,285,010 B. With 985 B per
  > file taken out, content costs 7.97 and 7.88 B per byte. Rounded up:
  > 8 per byte, 1 KiB per file, and 1 KiB per catalog (generous, to include
  > the cache entry's own bookkeeping). No real catalog checkout was
  > available to the run.

  > **Corrected (issue #745, 2026-11-19).** The catalog cache has no TTL of
  > its own (issue #686): `CatalogCache::new` takes the pool, the budget
  > and the metrics, and no TTL. Freshness is the directory row's: a use compares the
  > snapshot's generation with the row's and refreshes in place when
  > behind. The moka TTL on snapshots is a fixed one-day backstop.

- **Loads are single-flight** (`try_get_with`): concurrent misses on one
  catalog build it once, through today's `CatalogState::load`.
- **A stale hit refreshes in place** with today's `refresh_to`. Readers
  keep serving the old snapshot until the new one is swapped in. moka
  weighs an entry only when it is inserted or replaced, so every in-place
  refresh (stale hit, `catalog` message, the publisher's own refresh)
  re-inserts the entry to re-weigh it; a catalog that grows past the
  budget is then evicted instead of keeping its old weight.
- **A catalog larger than the whole budget** is loaded, served to the
  requests waiting on it, and not retained; a warning names its size.
- **A catalog whose stored data will not build** is recorded in a small
  negative cache keyed by `(id, generation)`, so that generation is not
  rebuilt on every request. This replaces the poll's `broken` map. The
  request gets today's error.
- **A database error on a miss** is returned to that request and not
  cached.

### 3. A request pins the catalogs it uses

`ToolContext` no longer copies the whole map at construction. It resolves
each catalog the first time the request touches it and keeps the
snapshot (`Arc<Loaded>`, not only the `Arc<CatalogState>` whose current
snapshot an in-place refresh swaps) for the rest of the request, so one
request sees one snapshot per catalog even if the entry is evicted or
refreshed meanwhile. Writers that must see the latest still use the state.

### 4. Per-catalog locks move out of the cache entry

The in-process publish mutex (`publishing`) and the rebuild mutex
(`rebuilding`) live in `CatalogState` today. If an entry were evicted and
loaded again, two publishes could hold two different mutexes. Both move to
`KeyedLocks` (`src/keyed_lock.rs`), keyed by catalog id, independent of
cache residency. The advisory lock in the publish transaction is
unchanged.

### 5. Overlays get a byte budget

Overlays (`src/overlay.rs`) each hold a full snapshot. They keep their
own cache, now weighed by the same estimate and bounded by
`overlay_cache_bytes` in place of `overlay_cache_size`, which parses with
a deprecation warning for one release and is then removed.

### 6. The catalog directory answers "exists?" and "which?" without content

`CatalogDirectory` is a bounded TTL cache of catalog rows, keyed by name,
holding what `select_catalogs!` reads (`id`, `name`, `display_name`,
`description`, `generation`, `owner_team`) plus the access facts of
Decision 9 (owner team suspended, maintenance opt-out).

- `resolve(name)` serves the cached row if it is younger than
  `catalog_ttl` (default 10s) and not invalidated, else reads it with one
  indexed query. Its `generation` is what `CatalogCache` checks entries
  against, so freshness has one point of control.
- A missing name is cached as absent for 5s. Creating a catalog
  invalidates the absent entry.
- `description` and `owner_team` are read from the directory, not copied
  into `CatalogState` (`set_description` and `set_owner_team` go away).
- Capacity: `catalog_directory_entries` (default 100,000).

### 7. Listing catalogs is a query driven by the principal's reach

Every place that scans the map and filters by `principal.reaches` becomes
a query:

- `Reach::Catalogs(map)`: `WHERE name = ANY($1)`, bounded by the key's
  grants.
- `Reach::All`: paginated, `ORDER BY name` with an opaque cursor and a
  limit (default 100, max 1000).

Paginated responses gain a `next` cursor (absent on the last page):
`GET /api/catalogs`, the identity endpoint's catalog list, the labels
endpoint's catalog list, and the MCP `list_catalogs` tool. The console
follows the cursor. This is a response shape change and is called out in
the release notes.

> **Corrected (issue #745, 2026-11-19).** Task 21 (issue #736) extends the
> paging to the pages that showed a user's whole ceiling. `GET /me` drops
> `ceiling` for `ceiling_access` (the highest level, or `null`) and carries
> no `next`; `GET /session` drops it and has no paging. The new
> `GET /me/ceiling?prefix=&after=&limit=` answers `{ceiling, next}`: the
> catalogs whose names start with `prefix`, in byte order, at most `limit`
> (`ceiling::page`). `GET /whoami` gains `levels`, and the consent page
> shows one page at a time (Find, Next catalogs). A client that read the
> whole ceiling from `/me` must page `/me/ceiling` and follow `next`.
> Listings leave out catalogs that do not build.

### 8. Search fan-out is capped

`GET /api/search` with no catalogs named searches every reachable catalog
today, which for a `Reach::All` key would be every catalog in the system.
It now searches at most `search_fanout_max` catalogs (default 25). A
request that would exceed it is refused with a 400 asking the caller to
name the catalogs. The cap applies to every key.

> **Corrected (issue #745, 2026-11-19).** The cap applies only when the
> request names no catalogs (issue #725). An explicit list is not capped:
> the console names every reachable catalog, so capping a list would have
> broken it. The cap still applies to every key.

> **Corrected (issue #725, 2026-11-23).** An explicit list is capped too: a
> request may name at most `search_fanout_max` distinct catalogs (a repeated
> name counts once), in `GET /api/v1/search` and in MCP `search`, with the
> same 400. The console no longer names every catalog: a key that reaches no
> more than the cap names none, and one that reaches more is shown a chooser
> limited to the cap, which it learns from `GET /whoami`'s `search_fanout_max`.

### 9. The key cache is split by what each change touches

| Cache | Key | Holds | Invalidated by |
|---|---|---|---|
| Credential | key hash (API key), grant id (OAuth) | the key or grant row: reach, level, default catalog, flags, user id | `credential` |
| User | `users.id` | admin flag, name, email, ceiling, team ids, entitlements, live unlocks | `user`, `team` (each cached user listing that team) |
| Catalog access facts | catalog name | owner team, suspended, maintenance opt-out | `catalog_meta` (part of the directory row, Decision 6) |
| Labels | slug | display labels | `user`, `team`, `catalog_meta` |

A request builds its principal from the credential entry and the user
entry, capping reach by the ceiling as `principal_of` does today.
Suspension and gate checks (`Principal::require`, `check_open`,
`admission`) look up the catalog being acted on in the directory at the
moment of the check, instead of a map shared by every principal. Unlock
expiry is still checked on every request.

- **TTL backstop:** `credential_ttl`, default 10s, the same bound
  `poll_interval` gives a lost announcement today. Revocation is never
  weaker than now. Settings, which principal assembly reads (for example
  `require_unlock`), get the same backstop: the first request after the TTL
  reloads them once, so a lost `settings` message is also bounded by
  `credential_ttl`.
- **Unknown credentials are not cached.** A bad token costs one indexed
  query; the existing rate limiters guard guessing. This removes the
  window in which a newly created key is refused on another replica, so
  `App::catch_up_keys` goes away.
- **The OAuth `AccessTokens` cache** (`src/oauth/access.rs`) stays, and
  resolves its grant through the credential cache.
- **Capacity:** `credential_cache_entries` and `user_cache_entries`
  (default 100,000 each). Label lookups share a bounded cache of 100,000
  slugs.
- **Settings** are global and small: loaded once per replica, reloaded on
  a `settings` invalidation or after `credential_ttl`. `admin_generation`
  remains only as the settings version.

> **Corrected (issue #745, 2026-11-19).** As shipped (issues #689, #728 and
> #736, PR #739): the user cache holds `{ auth, gates }`, not the ceiling.
> A user's ceiling grew with the catalogs their teams own, so it is
> resolved per catalog from the directory row (owner team, member access,
> entitlements) when a request needs it, and a session's reach is
> `Reach::Ceiling`. The pages that listed a whole ceiling no longer do
> (issue #736): `GET /me` returns `ceiling_access` instead, `GET /session`
> drops the ceiling, `GET /me/ceiling?prefix=&after=&limit=` pages it
> (`{ceiling, next}`) and the consent page shows one page at a time (see
> Decision 7). Listings (`GET /catalogs`, `/whoami`, `/labels`, MCP `list_catalogs`)
> page the same way (Decision 7).

### 10. Invalidations are sent by the transaction that makes the change

> **Corrected (2026-11-18, planning).** The design first sent invalidations
> on the Redis announce channel after commit. Planning found about 35 store
> functions that change access state, called from HTTP handlers, the OAuth
> consent flow and about 30 CLI commands, each bumping `admin_generation`
> inside its own transaction. Publishing from every caller after commit
> would be easy to forget, would lose the message on a crash between
> commit and publish, and would leave CLI changes to the TTL. The
> maintainer chose Postgres `NOTIFY` instead.

Each store function that today calls `bump_admin_generation(conn)` calls
`invalidate(conn, &[Invalidation])` instead, inside the same transaction.
It runs `SELECT pg_notify('okf_invalidate', $1)` once per invalidation,
so:

- the message is delivered only if the transaction commits, and never on
  rollback;
- the CLI, the server and any other writer are covered with no separate
  step;
- it works with or without Redis.

The payload is versioned JSON, well under `NOTIFY`'s 8000-byte limit:

```json
{"v":1,"kind":"catalog","id":42,"generation":17}
```

Kinds and fields:

| Kind | Fields | Sent by |
|---|---|---|
| `catalog` | `id`, `generation` | publish, import, repair |
| `catalog_meta` | `id`, `name` (the slug, so a cached "no such catalog" entry is dropped on create) | create, delete, `update_details`, `set_owner`, `set_maintenance`, `set_member_access`, team suspend and unsuspend (one per owned catalog) |
| `credential` | `key_id` | key or grant create, revoke, grant, ungrant, `set_user`, `set_maintenance_report`, OAuth code redeem, self-issued grant create |
| `user` | `id` | user create, delete, `set_email`, `set_display_name`, `set_admin`, entitle, unentitle, unlock create and end |
| `team` | `id` | team create, delete, `set_member`, `remove_member`, `set_display_name`, `set_member_access` (for the owner team) |
| `settings` | none | settings set and reset |

`admin_generation` is still bumped by `invalidate`, so it remains a single
counter of administrative changes; only the settings reload reads it.
A `credential` message carries the key id, never the key hash.

Every replica runs one listener task on its own connection
(`PgListener`, `LISTEN okf_invalidate`) that applies each message to its
caches. A replica applies its own transactions' messages the same way: the
writer does not apply them by hand.

`NOTIFY` serializes the commits of notifying transactions. At
administrative and publish rates (tens per second) this costs nothing
measurable; it is recorded so a much higher write rate is designed for.

### 11. Loads cannot resurrect stale data

Each cache keeps a per-key invalidation epoch, but only for keys with a
load in flight: the counter is created when a load starts and removed when
the last load of that key finishes. A load records the epoch when it starts
and inserts its result only if the epoch has not moved; otherwise it serves
the result to its own request and does not cache it. An invalidation of a
key that is neither cached nor loading keeps no state, so the map is bounded
by concurrent loads, not by the population or the change history. A
`catalog` message also raises an in-flight load's minimum generation, so a
load that read an older generation is never cached; a load that starts
after the message reads after the writer's commit and needs none.

Some invalidations name an entry indirectly: a `credential` message names
a key id, but the hash cache is keyed by hash; a `team` message drops
every user in the team. These scan the resident entries, so a load still
in flight, whose key is known only from its result, cannot be matched.
Each cache therefore also keeps one cache-wide fence counter. An indirect
invalidation raises it, and a load that started before the raise serves
its result to its own request but does not cache it. That costs one
counter, and the loads in flight at such a moment go uncached.

### 12. Reconnects mark everything stale

`NOTIFY` is not delivered to a listener while it is disconnected. When
the listener reconnects, every cached entry is marked stale (not dropped):
each is revalidated against Postgres on next use. Content stays warm, and
no message lost during the gap is trusted.

### 13. TTLs are the backstop, not the mechanism

Entries are revalidated after `catalog_ttl` and `credential_ttl` (10s
each) even with no message, covering a listener that is up but missed a
message for a reason not covered by Decision 12. With or without Redis,
any number of replicas may run: the invalidation path no longer depends
on Redis. Redis is still needed for multi-replica serve for what
milestone 16 put there (MCP sessions, OAuth and sign-in state).

### 14. Rolling deploys with mixed versions

During the deploy, old replicas run the M16 poll loop and the
`admin_generation` announcement over Redis. For one release, new replicas
bridge both ways:

- **New writes reach old replicas.** `invalidate` still bumps
  `admin_generation` (Decision 10), so an old replica's poll sees every
  change made by a new one, and a new replica that made a server-side
  administrative change also publishes the M16 empty announcement when
  `[redis]` is set, so old replicas reload at once.
- **Old writes reach new replicas.** Old replicas send no `NOTIFY`. New
  replicas keep subscribing to the M16 channel for one release and treat
  any announcement as "mark credential, user and label caches and settings
  stale". Catalog publishes on an old replica reach new replicas through
  `catalog_ttl` (10s), the same bound the poll gives today.

The bridge is removed one release later.

> **Corrected (issue #745, 2026-11-19).** The bridge payload is the string
> `"notified"` (`announce::BRIDGE_PAYLOAD`, issue #691), not the empty
> message. An M16 replica reads any non-number as "check now", as it reads
> the M16 command line's empty payload. A new replica ignores the bridge
> payload (that change also went through `NOTIFY`) and answers every other
> payload as an M16 announcement.

### 15. Housekeeping runs as apalis jobs, once per cluster (#356)

> **Corrected (2026-11-18, planning).** The design first used a
> `housekeeping_schedule` table, a self-rescheduling job chain and a repair
> sweep. apalis-postgres 1.1 has a unique index on
> `(job_type, idempotency_key)`, which makes all three unnecessary.

Queue `housekeeping`, one job type `Housekeeping { task, slot }`:

| Task | Cadence | Work |
|---|---|---|
| `prune_changesets` | 5 min | empty changesets unchanged for 24h |
| `prune_oauth` | hourly | idle OAuth grants revoked first, then expired tokens and codes, unbound clients, old sign-in events, expired emailed links, console sessions, old mail jobs, done housekeeping jobs older than a day |
| `prune_audit` | daily | entries past `audit_retention_days` plus the margin |
| `prune_workers` | hourly | dead `apalis.workers` rows (#458) |
| `cluster_stats` | 60s | open changesets total, mail queue depth by status |

Scheduling: a ticker on every replica, once a minute, pushes for each task
the job of the current slot (`slot` = the start of the current cadence
window, as a Unix second) with idempotency key `"<task>:<slot>"`. Each push
runs in its own statement with `ON CONFLICT DO NOTHING` semantics: a
unique violation on the idempotency index means another replica already
scheduled that slot and is ignored. So each slot runs once in the cluster,
whichever replica's worker takes it, and a replica that is down simply
does not push. A slot missed because every replica was down is not caught
up; the next slot does the same work.

Failures are retried by apalis (3 attempts), then logged; the next slot
does the work again.

### 16. Database-wide gauges are reported identically by every replica

`cluster_stats` writes its counts to a one-row `cluster_stats` table
(`open_changesets bigint`, `mail_queue jsonb`, `computed_at`). Each
replica refreshes an in-memory copy from that row once a minute and its
gauge callbacks serve the copy, so every replica reports the same value
and dashboards use `max()`. `okf.changesets.open` loses its `catalog`
label and becomes one cluster total.

### 17. Every replica runs a worker (#458)

- `mail.worker` is deprecated: it parses, logs a warning, and is ignored.
  Every `serve` runs one apalis worker for `send_mail` and
  `housekeeping`. Tests disable it with a constructor flag, not config.
- The worker's dedicated pool shrinks from 4 connections to 3, shared by
  both queues: mail's `LISTEN`, mail's fetch, ack, heartbeat and handler
  writes, and housekeeping's fetch, ack and heartbeat (housekeeping polls
  without `LISTEN`).
- The invalidation listener (Decision 10) holds one connection for the
  life of the process, from its own one-connection pool rather than the
  main pool, so it never takes a request slot.
- `docs/operations.md` gives the Postgres connection total per replica as
  `database_max_connections` + 1 (listener) + 3 (worker), and for N
  replicas `N × (database_max_connections + 4)`.
- Orphan recovery: heartbeat 10s, 6 missed heartbeats, so a dead pod's
  job returns after about 1 minute. Documented, with the `sending` state
  it implies for mail.
- On SIGTERM the worker stops fetching and waits for its in-flight job up
  to `worker_shutdown_grace` (default 30s), which the docs match to
  `terminationGracePeriodSeconds`. Mail delivery is documented as
  at least once, with the duplicate case spelled out.
- `prune_workers` deletes `apalis.workers` rows whose `last_seen` is older
  than 10 orphan timeouts and that no `Running` job references.

> **Corrected (issue #745, 2026-11-19).** `prune_workers` keeps every
> worker row that any job still names, not only `Queued` or `Running` ones
> (issue #692): `apalis.jobs.lock_by` is a foreign key with no `ON DELETE`,
> so deleting the row of a worker that finished jobs fails. A dead worker's
> row is prunable at once (apalis clears `lock_by` when it re-enqueues its
> job); a sender's row goes after the jobs are pruned (7 days).

### 18. The poll loop is removed

`App::poll`, `poll_pass`, `poll_until`, the `polling` mutex and
`poll_interval` go. `poll_interval` parses with a deprecation warning for
one release. What remains on each replica is the once-a-minute housekeeping
ticker (Decision 15), the invalidation listener (Decision 10) and the
`cluster_stats` refresh (Decision 16).

### 19. Metrics have no per-catalog series

Removed: `okf.poll` (span and duration metric), `okf_catalog_refreshes_total`,
`okf.catalog.concepts`, `okf.catalog.generation`, and the `catalog` label
on `okf.changesets.open`.

Added, labelled by `cache` (`catalog`, `overlay`, `directory`,
`credential`, `user`, `labels`) where it applies:

- `okf.cache.requests{cache,result}` with `result` = `hit`, `miss`,
  `stale`;
- `okf.cache.evictions{cache,cause}` (`size`, `ttl`, `invalidated`);
- `okf.cache.entries{cache}` and `okf.cache.bytes{cache}` (the weighed
  caches);
- `okf.cache.load.duration{cache,outcome}` (histogram);
- `okf.invalidations.queued{kind}`, `okf.invalidations.received{kind}`,
  and `okf.invalidations.lag` (histogram, send to apply, from a send
  timestamp in the payload);
- `okf.housekeeping.runs{task,outcome}` and
  `okf.housekeeping.duration{task}`.

A large catalog's load is still visible per catalog in traces: the
`okf.catalog.load` span keeps its `okf.catalog` attribute.

### 20. Audit rows are created with their entity

`gates::load` creates any missing `audit_people`, `audit_credentials` and
`audit_teams` row for every user, key and owner team on every reload.
Per-request loading cannot do that. Instead:

- the store functions that create a user, key, grant or team create its
  audit row in the same transaction;
- one migration backfills a row for every existing entity that lacks one;
- the credential and user caches read the audit ids with the entity
  (`AccessGates::people` and `credentials` become fields of the cached
  entries), and the directory row carries the catalog's audit team id
  (`CatalogScope`).

An entity found without its audit row at request time (which the
migration and the create paths rule out) is refused with
`OkfError::Database`'s 503 and logged, rather than recorded without one.

> **Corrected (issue #745, 2026-11-19).** An entity found without its audit
> row at request time is recorded with no credential id, not refused with
> 503. Rows are created with their entity and backfilled by migration
> `0023_audit_backfill.sql` (issue #681). The maintainer kept this
> behaviour (2026-11-19). Reading a user's gates (`gates::for_user`,
> `gates::audit_credential_id`) no longer creates rows
> (`reading_a_users_gates_writes_nothing`).

## Architecture

```
request ──► auth: CredentialCache ─► UserCache ─► Principal
              │                                     │
              ▼                                     ▼
        CatalogDirectory (rows, access facts) ◄── selection / listing (SQL)
              │ generation
              ▼
        CatalogCache (weighed LRU, single-flight) ──► Arc<CatalogState> pinned per request
              ▲
  LISTEN okf_invalidate ──► Invalidations::apply (epoch bump, drop / mark stale)
              ▲ delivered at commit
  store functions (HTTP, MCP, CLI) ──► invalidate(conn, ..) = pg_notify in the tx

apalis worker (every replica): send_mail, housekeeping
  ticker on every replica ─► push "<task>:<slot>" (idempotency key) ─► one run per slot
  cluster_stats job ─► cluster_stats row ─► gauges on every replica
```

New modules:

| Module | Responsibility |
|---|---|
| `cache::catalogs` | `CatalogCache`: weighed LRU, single-flight load, stale refresh, negative cache |
| `cache::directory` | `CatalogDirectory`: rows by name, absent entries, reach-driven listing queries |
| `cache::credentials` | credential, user and label caches; principal assembly |
| `cache::epoch` | per-key invalidation epochs and minimum generations for in-flight loads, shared by every cache |
| `invalidate` | the `Invalidation` enum, its JSON form, `invalidate(conn, ..)`, the listener and `apply` |
| `housekeeping` | the job type, the ticker, the tasks, `cluster_stats` |

`App` holds the caches instead of `catalogs`, `keys`, `polling`,
`broken`, `open_changesets`, `oauth_pruned` and `audit_pruned`.

## Configuration

| Setting | Default | Replaces |
|---|---|---|
| `catalog_cache_bytes` | 1 GiB | (load everything) |
| `overlay_cache_bytes` | 256 MiB | `overlay_cache_size` |
| `catalog_directory_entries` | 100,000 | |
| `credential_cache_entries` | 100,000 | |
| `user_cache_entries` | 100,000 | |
| `catalog_ttl` | 10s | `poll_interval` |
| `credential_ttl` | 10s | `poll_interval` |
| `search_fanout_max` | 25 | |
| `worker_shutdown_grace` | 30s | |

Deprecated (parse with a warning for one release): `poll_interval`,
`overlay_cache_size`, `mail.worker`.

## Error handling

- A cache load that fails with `OkfError::Database` is returned to the
  request (503 or the tool error it maps to today) and not cached.
- A catalog that fails to build is cached negatively per generation
  (Decision 2) and returns today's error.
- `invalidate` runs in the writer's transaction: if `pg_notify` fails, the
  transaction fails, as any other statement in it would.
- A listener that loses its connection reconnects with backoff and marks
  every cache stale (Decision 12); the TTLs bound staleness meanwhile.
- An unparseable invalidation payload is counted, logged at warn, and
  treated as a reconnect (Decision 12) for safety.
- A housekeeping job failure is retried by apalis, then logged; the next
  slot does the work again (Decision 15).

## Privacy and security

- Revocation bound is unchanged at 10s on a lost message, immediate
  otherwise, on every replica.
- Invalidation payloads carry ids only: no key hashes, names of people,
  emails or catalog content.
- The search fan-out cap (Decision 8) also limits how much work one
  request can cause.
- Not caching unknown credentials keeps a flood of bad tokens out of the
  cache; it is bounded by the rate limiters.

## Testing

- **Unit tests** for each cache: hit, miss, stale refresh, TTL expiry,
  eviction by weight, the over-budget catalog, negative caching, and
  single-flight (many concurrent misses, one load).
- **The epoch race** (Decision 11): a test holds a load open, delivers an
  invalidation, releases the load, and asserts the stale result is served
  once and not cached. A property test (hegel) interleaves loads and
  invalidations and checks the cache never holds a generation below the
  announced minimum.
- **Invalidation coverage:** for every store function in the table of
  Decision 10, a test listens on `okf_invalidate` and asserts the right
  `Invalidation` arrives after commit and nothing arrives on rollback.
- **Two-replica integration tests** (two `App`s, one Postgres, with TTLs
  set to an hour so a pass proves `NOTIFY` carried the change): publish on
  A is served on B; revoke on A is refused on B; a change made through the
  CLI's store functions reaches both; a listener whose connection is
  killed (`pg_terminate_backend`) marks entries stale on reconnect.
- **Mixed-version tests** (Decision 14): an M16 announcement marks a new
  replica's credential caches stale; a new replica's administrative change
  publishes the M16 announcement when `[redis]` is set.
- **Housekeeping:** two tickers pushing the same slot create one job;
  each task's job does its work; `prune_workers` removes a stale row and
  keeps a live one; SIGTERM waits for an in-flight job.
- **Scale test** (ignored by default, run in the milestone close-out):
  seed 1M catalog rows (tiny catalogs) and 1M users with keys; assert that
  startup loads no catalog, resident memory stays within the configured
  budgets after touching a working set larger than the budget, and
  listing and auth latencies stay within stated bounds.

> **Corrected (issue #745, 2026-11-19).** The hegel property test of the
> epoch race was not written in M18; it is deferred to issue #744. The
> unit and integration tests above cover the race with a held load. The
> scale test ran with a small budget (2,456,000 bytes, 1,000 catalogs), not the
> default 1 GiB; the default settings (1 GiB catalog cache, fan-out cap 25)
> were kept. Its listing bound is split: 200 ms for the cold first page, 50
> ms for a repeated one (the cold page loads 100 snapshots one after
> another, issue #732).

## Rollout

1. Ship with the new defaults. Old and new replicas coexist during the
   rolling deploy (Decision 14). The audit backfill migration (Decision 20) runs before
   the new binary serves.
2. Watch `okf.cache.requests` hit ratios, `okf.cache.bytes`,
   `okf.cache.load.duration` and `okf.invalidations.lag`.
3. Optionally enable ingress hashing on `Mcp-Session-Id` and compare cold
   loads.
4. One release later: remove the M16 announcement bridge and the
   deprecated settings.

## Docs

- `docs/operations.md`: the caches and their settings, sizing for memory
  and Postgres connections with N replicas, optional session hashing, the
  worker and its shutdown grace, at-least-once mail.
- `docs/reference/`: the paginated responses and the search fan-out cap.
- Release notes: removed metrics and their replacements, response shape
  changes, deprecated settings.
