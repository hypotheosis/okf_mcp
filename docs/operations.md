# Running okf-mcp

This guide is for anyone running the server: what it is made of, how to
configure it, a complete Kubernetes example, the first run, health, upgrades,
backups and troubleshooting. Every name in it is a placeholder: the host is
`okf-mcp.example.com`, the namespace and database are `okf-mcp`, and "the
site's object store" is whatever S3-compatible store your backups go to.

The design (`docs/superpowers/specs/2026-10-26-okf-mcp-server-design.md`) says
why the server is built this way; the README describes the tools, the
changesets and the routes in detail.

## 1. What runs

- **One process**, `okf-mcp serve`. Postgres holds the durable state:
  catalogs, revisions, changesets, API keys, grants and accounts. The pod needs
  no persistent volume. **One replica by default; several with `[redis]`.**
- **Why one replica, without `[redis]`.** Publishing is not the reason: it is
  safe across processes. A publish takes an in-process mutex, which orders
  publishes within one process, and a per-catalog Postgres advisory lock inside
  the publish transaction, which orders publishes, imports and deletes of a
  catalog across every process. What breaks with a second replica is the
  short-lived state each process keeps in its own memory:

  | State | What fails on another replica |
  |---|---|
  | MCP sessions, and the key each is bound to | A request naming a session another replica opened gets 404; a restored session would have no binding for the key check |
  | OAuth consent pages in progress, and authorization codes | The consent POST or the token exchange lands elsewhere: an unknown request or code |
  | OIDC sign-in and link flows | The callback lands elsewhere and the flow is unknown |
  | Passkey registration and sign-in ceremonies | Finishing on another replica fails |
  | The OAuth access-token cache | A token revoked on one replica still works on another until it expires |
  | Rate-limit buckets | Counted per replica, so each replica allows the whole budget (with `[redis]` they are shared) |

  The credential, user, settings and catalog caches are not in the table: since
  milestone 18 they follow Postgres change messages on every replica, with or
  without Redis.

- **With `[redis]`, several replicas work.** Redis holds the first four rows'
  short-lived state and the rate-limit buckets, and two caches also learn of changes on every
  replica promptly through Redis: the catalog snapshots (best-effort pub/sub
  announcements of each catalog's new generation) and the access-token cache
  (a revocation counter). Credentials, users, settings and catalog rows learn
  of changes through the Postgres change feed, with or without Redis.
  Everything else stays as it was. The Redis section below says what to run, what it holds and what
  happens when it fails; "Several replicas: what stays per replica" lists the
  caveats. Without the table nothing changes: no Redis code runs, no
  connection is made, and one replica is still required.
- **Routes:**

  | Route | Access | Does |
  |---|---|---|
  | `GET /healthz` | none | The database's state, the number of catalogs this replica holds and the last clock check (section 7) |
  | `POST/GET/DELETE /mcp` | any key (`read`, `write` or `admin`) | MCP streamable HTTP; each tool checks its own level and the key's catalog grants |
  | `GET /catalogs/{name}/export` | `read` on the catalog | The catalog as a `.tar.gz` |
  | `POST /catalogs/{name}/import[?replace=true&only_machinery=true&expect_generation=N]` | `write` on the catalog | Imports an uploaded `.tar.gz` |

  Keys are bearer tokens (`Authorization: Bearer <key>`). Only `/healthz` is
  open, and it names no catalog, host or secret.
- **The command line** (`okf-mcp catalog`, `key`, `import`, `export`,
  `migrate`) talks to the database directly, not to the server. Each write
  sends a change message (Postgres `NOTIFY`) from its own transaction, so
  every running replica normally sees a new, changed, imported or deleted
  catalog within a fraction of a second. If a message is missed while the
  listener stays connected, a replica rereads a catalog's row after
  `catalog_ttl` at the latest (10 seconds by default); after a reconnect it
  rereads every row on next use. Key, user, team and settings changes travel
  the same way (a lost message is bounded by `credential_ttl`, 10 seconds by
  default). With `[redis]` the command line also announces imports and
  `catalog repair-indexes` to the catalog subscribers (best effort).

## 2. The image

- **Name:** `ghcr.io/hypotheosis/okf-mcp:<version>`, for example `:0.34.0`.
  Every build is also tagged `:sha-<short commit>`. There is no `:latest`:
  manifests pin a version.
- **Built by** `.github/workflows/image.yml` when a `v*` tag is pushed (`v0.34.0`
  gives `:0.34.0`), and on a manual dispatch (which publishes `:sha-<short>`
  only). The workflow runs no tests; they run before each merge.
- **Contents:** Debian bookworm-slim with `ca-certificates`, and the binary at
  `/usr/local/bin/okf-mcp`. No git. The binary uses rustls and links no
  OpenSSL.
- **User:** uid `10001`, non-root (`USER 10001`, numeric, so
  `runAsNonRoot` can check it).
- **Entrypoint and command:** `ENTRYPOINT ["/usr/local/bin/okf-mcp"]`,
  `CMD ["serve"]`. Override the command to run a subcommand
  (`["migrate"]`), or pass flags before it (`["--config", "/etc/okf-mcp/config.toml", "serve"]`).
  The image exposes port 8080.
- **Base images float on purpose.** The Dockerfile names `rust:1.99.1-slim-bookworm`
  and `debian:bookworm-slim` by tag, not digest, so a new build picks up
  Debian's security fixes. A pushed release keeps its digest. Never rebuild an
  old tag: to ship a security fix, release a new version.

Build locally (BuildKit, which recent Docker uses by default):

```sh
docker build -t okf-mcp:dev .
docker run --rm okf-mcp:dev --version
```

## 3. Configuration

### The configuration file

TOML, named by `--config` or `OKF_MCP_CONFIG`. Every key is optional, and
unknown keys are ignored, so one file serves every version.

| Key | Default | Meaning |
|---|---|---|
| `listen` | `"0.0.0.0:8080"` | The address `serve` listens on. `--listen` or `OKF_MCP_LISTEN` overrides it |
| `allowed_hosts` | `["localhost", "127.0.0.1", "::1"]` | The `Host` names (optionally `host:port`) that `/mcp` accepts: rmcp's DNS-rebinding check. Behind an ingress, the public host name |
| `default_catalog` | none | The catalog a key with no default of its own reads and writes |
| `session_idle_timeout` | `"2h"` | An idle MCP session is closed after this; an open changeset with no change within it is `idle` in `list_changesets` |
| `maintenance_review_intervals` | `{}` (30 days per catalog) | Per-catalog review durations, e.g. `{ pg = "30d" }`; each must be at least one day |
| `max_import_bytes` | `"64MiB"` | The largest uncompressed import (`MiB` is binary, `MB` decimal, or a number of bytes). The import route caps the request body at this plus 0.1% and 64 KiB of gzip slack |
| `time_server` | none | The NTP server for the clock check (`host`, `host:port` or an address). Without it there is no check |
| `concept_outline_chars` | `20000` | `get_concept` returns an outline of the headings, not the body, for a body longer than this many characters, unless the call passes `full` or `section` |
| `database_max_connections` | `16` | The most database connections one process opens (`serve` and every subcommand). Writers queued on one changeset wait without one; the writer whose turn it is, a publish, and a read that builds an overlay each hold one |
| `store_max_connections` | `8` | The most connections one process opens to each catalog store, beside `database_max_connections` (the account database). Every store, `main` included, has a pool of its own, opened on its first use. A store on its own database also holds one more connection for its change-feed listener, outside this limit |
| `store_idle_timeout` | `"10m"` | How long an idle connection to a catalog store is kept open before it is closed. A store's pool holds no connection while unused |
| `store_connect_timeout` | `"5s"` | How long opening a store's pool, or waiting for a connection to begin a transaction on it, may take. Past it the request fails with `StoreUnavailable`, which never names the store. Loading a catalog from a store may take 6 times this before the store is reported unavailable |
| `overlay_cache_bytes` | `"256MiB"` | The most memory the changeset overlays (an open changeset's view of its catalog) may take, estimated like a catalog snapshot (a fixed cost per file plus a multiple of the stored size, over the overlay's whole workspace, so many empty files count). Past it, an overlay is dropped and rebuilt on its next read; one larger than the whole budget is served but not kept (a budget above 4 GiB is counted in coarser, rounded-up units, as for `catalog_cache_bytes`). An overlay unused for `session_idle_timeout` is dropped too |
| `public_url` | none | The server's public origin, such as `"https://okf-mcp.example.com"` (no path, query or user info; a trailing `/` is dropped). Setting it turns on OAuth ("OAuth connectors") and console sign-in by account ("Signing in and self-service"); without it the OAuth and sign-in routes are 404. It must be `https`, except on a loopback host (such as `localhost`, `127.0.0.1` or `::1`) for local testing, and its host must be in `allowed_hosts` |
| `oauth.access_token_ttl` | `"24h"` | How long an OAuth access token works. A revoked one stops working at once whatever its lifetime, so the long default costs little; each refresh is a chance for a client's processes to race (`oauth.refresh_reuse_grace`) |
| `oauth.refresh_token_ttl` | `"30d"` | How long a refresh token works. Each refresh issues a new one with a fresh lifetime, so a connection used within this stays signed in |
| `oauth.refresh_reuse_grace` | `"60s"` | How long after its first use a refresh token presented again by the same client is rotated again instead of ending the connection ([Concurrent refreshes](#concurrent-refreshes)). `"0s"` turns it off; at most 10 minutes |
| `oauth.idle_grant_revoke_after` | `"30d"` | The hourly `prune_oauth` housekeeping job revokes an OAuth grant that has had no token issued for this long and holds no live token ([Idle grants](#idle-grants)). At most 10 years; zero is refused |
| `oauth.code_max_expiry` | `"7d"` | The longest expiry a sign-in code may be given (`oauth code create --expires`, the console's expiry) |
| `oauth.dcr_enabled` | `true` | Accept dynamic client registration (RFC 7591) at `/oauth/register`. Clients that publish a Client ID Metadata Document work either way |
| `oauth.cimd_allow_private` | `false` | Fetch Client ID Metadata Documents from loopback and private addresses. For test rigs only: it opens the fetcher to the internal network |
| `auth.password_sign_in` | `true` | Offer email and password sign-in on `/signin`. The default for the `password_sign_in` setting ([Signing in and self-service](#signing-in-and-self-service)) |
| `auth.max_keys_per_user` | `20` | The most live self-issued keys a user may hold, `0` to `1000` (`max_keys_per_user` setting) |
| `auth.max_key_lifetime` | none | The longest expiry a self-issued key may have, such as `"90d"`; unset means no limit (`max_key_lifetime` setting) |
| `auth.console_session_idle` | `"12h"` | A console session unused this long ends (`console_session_idle` setting). Not `session_idle_timeout`, which is MCP's |
| `auth.console_session_absolute` | `"7d"` | A console session ends this long after sign-in, used or not (`console_session_absolute` setting) |
| `auth.setup_link_ttl` | `"24h"` | How long a setup link works (`setup_link_ttl` setting) |
| `auth.reset_link_ttl` | `"1h"` | How long a password reset link works (`reset_link_ttl` setting) |
| `auth.trusted_proxy_header` | none | The header whose last address is the client's, such as `"x-forwarded-for"`, behind an ingress that appends it. Unset: the TCP peer's address, and no header is read. The sign-in limits and the sign-in history use it. Config only, not a setting |
| `[[auth.oidc]]` | none | One table per OIDC provider: `id` (1 to 32 of `a-z`, `0-9` and `-`; stable, since each linked identity stores it), `label` (the button text), `issuer`, `client_id`, `client_secret_env` (the environment variable holding the secret) and `scopes` (default `["openid", "email", "profile"]`). Needs `public_url` |
| `mail.provider` | `"log"` | `"smtp"`, `"postmark"`, `"ses"` or `"log"`. **`log` is for development only:** it writes each whole message, setup and reset links included, to the log at `info`, and delivers nothing |
| `mail.from` | `"okf <okf@localhost>"` | The From address, such as `"okf <okf@example.com>"`; it must parse as one mailbox. Set it for `smtp`, `postmark` and `ses` (for `ses`, an identity verified in SES) |
| `catalog_cache_bytes` | `"1GiB"` | The most memory this process's catalog snapshots may take, estimated from their stored size (`"512MiB"`, or a number of bytes). Catalogs load on first use, not at start-up; past the budget the least recently used one is dropped and loaded again when next read. A catalog larger than the whole budget is served but not kept |
| `catalog_directory_entries` | `100000` | The most catalog rows the catalog directory keeps in memory, by name. It answers "does this catalog exist, and who owns it" without loading content |
| `catalog_ttl` | `"10s"` | How long the directory serves a catalog row before reading it again, and so the longest a replica serves an older snapshot of a catalog whose change message it missed (each use refreshes the snapshot to its row). A change drops the row at once; this covers a change message that is lost while the listener stays connected (a reconnect marks every row stale). A name that does not exist is remembered for 5 seconds regardless |
| `credential_cache_entries` | `100000` | The most API keys and OAuth grants each credential cache keeps in memory (one by key hash, one by grant id). A key or grant is read from the database the first time it is used; an unknown one is never cached |
| `user_cache_entries` | `100000` | The most users the user cache keeps in memory: each one's admin flag, teams, entitlements and live unlocks, read on first use (a ceiling is resolved per catalog from these and the catalog's row, so an entry does not grow with the catalogs a user's teams own) |
| `credential_ttl` | `"10s"` | How long a cached key, grant or user is served before it is read again. A change (a revocation, a team or admin change) drops it on every replica at once; this bounds how long one survives a change message that is lost |
| `search_fanout_max` | `25` | The most catalogs one search may cover, from 1 to 999. `GET /api/v1/search` with no `catalogs=` searches every catalog the key reaches and answers 400 when it reaches more. A request that names more than this many distinct catalogs (in `catalogs=` or in MCP `search`'s `catalogs` argument) also gets a 400; a name repeated counts once. The console reads the cap from `GET /whoami` (`search_fanout_max`): a key that reaches no more searches them all, and one that reaches more is shown a chooser limited to the cap |
| `worker_shutdown_grace` | `"30s"` | How long `serve` waits, once told to stop, for the mail worker to finish the mail in hand. Set the pod's `terminationGracePeriodSeconds` a few seconds above it |
| `mail.smtp.host`, `mail.smtp.port`, `mail.smtp.starttls`, `mail.smtp.username`, `mail.smtp.password_env` | none, `587`, `true`, none, none | The SMTP relay. `starttls = true` requires STARTTLS; `false` is TLS from the first byte (port 465); there is no plain-text mode. `username` and `password_env` (the environment variable holding the password) go together |
| `mail.postmark.token_env`, `mail.postmark.message_stream` | none, `"outbound"` | Postmark's server token (by environment variable) and message stream |
| `mail.ses.region` | none | The AWS region SES sends in. Unset, the AWS SDK's own resolution decides (`AWS_REGION`, then the profile, then instance metadata). The `[mail.ses]` table is optional; there is no credential key ([Mail](#mail)) |
| `feedback.forminit_form_id` | none | The `[feedback]` table turns on the `send_feedback` tool and the console's feedback form (the [Feedback](#feedback) section); without it neither is offered and nothing is sent anywhere. The id of the Forminit form feedback is posted to, `https://forminit.com/f/<forminit_form_id>`: 1 to 64 of `A-Z`, `a-z`, `0-9`, `-` and `_`. Not a secret. Unknown keys in this table are an error |
| `feedback.api_key_env` | none | The environment variable holding the form's API key, sent as `X-API-KEY`. Optional, but without a key Forminit takes one submission per 5 seconds from the server's address (5 per second with one), and a protected form needs it |
| `feedback.worker` | `true` | Run the feedback worker in `serve`. With several replicas, turn it on for one only. Its own pool opens up to 3 connections, on top of `database_max_connections` |
| `feedback.max_per_hour` | `10` | Submissions one credential (API key or OAuth grant) may make in any hour, 1 to 1000. Counted in Postgres, so it holds across replicas |
| `feedback.retention` | `"30d"` | How long forwarded feedback is kept before the hourly prune deletes it, 1 day to 10 years. Failed feedback is pruned after the same time, counted from when it failed |
| `redis.url_env` | none | The `[redis]` table turns on shared state for several replicas (the [Redis](#redis) section). `url_env` is required: the name of the environment variable holding the URL, `redis://` or `rediss://`, which may carry a password (so it never goes in the file). Unknown keys in this table are an error |
| `redis.key_prefix` | `"okf:"` | Starts every key and channel, so one Redis can serve several deployments. 1 to 64 bytes, with no whitespace, control character or brace: it goes inside the hash tag `{<key_prefix>}` |
| `redis.mode` | `"standalone"` | `"standalone"` for one Redis or Valkey primary (or a managed primary endpoint), `"cluster"` for a cluster-aware client: ElastiCache Serverless, which runs only in cluster mode, and other clusters |
| `telemetry.service_name` | `"okf-mcp"` | The `service.name` that OTLP reports |
| `telemetry.environment` | none | The `deployment.environment.name` that OTLP reports, such as `"production"` |
| `telemetry.stderr.format` | `"text"` | stderr's line format: `"text"` (today's lines) or `"json"` (one object per line, with `trace_id` and `span_id`) |
| `telemetry.stderr.filter` | `"warn,okf_mcp=info,tower_http=info"` | stderr's filter, a `tracing` directive string. `RUST_LOG` overrides it |
| `telemetry.otlp.endpoint`, `telemetry.otlp.headers_env` | none | The receiver's base URL (`/v1/traces`, `/v1/logs` and `/v1/metrics` are appended; HTTP with protobuf, not gRPC) and the environment variable holding its headers as `name=value,name=value`, or `none`. Set together. OTLP is off without the `[telemetry.otlp]` table |
| `telemetry.otlp.sentry_dsn_env` | none | The environment variable holding a Sentry DSN, instead of `endpoint` and `headers_env` |
| `telemetry.otlp.signals` | all three with `endpoint`; `["traces", "logs"]` with `sentry_dsn_env` | Which signals are pushed: any non-empty subset of `"traces"`, `"logs"` and `"metrics"`. A signal that is off gets no exporter, layer or reader; its filter and interval keys still parse, and its `okf_telemetry_export_failures_total` series stay at 0. `"metrics"` is refused with `sentry_dsn_env` |
| `telemetry.otlp.traces_filter`, `telemetry.otlp.logs_filter` | `"info"`, `"info"` | What the span and log exporters take (this crate's own targets only) |
| `telemetry.otlp.sample_ratio` | none | The share of new traces kept, `0.0` to `1.0`. Unset: `OTEL_TRACES_SAMPLER` decides, else every trace |
| `telemetry.otlp.export_interval` | `"60s"` | How often metrics are pushed over OTLP |
| `telemetry.prometheus.listen` | none | The address of the separate, unauthenticated `/metrics` listener, such as `"[::]:9464"`. No listener without it. It must not equal `listen` or share its port when either is unspecified |

The server refuses to start (and every subcommand refuses to run) with a zero
`session_idle_timeout`, `worker_shutdown_grace`,
`database_max_connections`, `store_max_connections`, `store_idle_timeout`, `store_connect_timeout`, `overlay_cache_bytes`, `catalog_cache_bytes`, `catalog_directory_entries`, `catalog_ttl`,
`credential_cache_entries`, `user_cache_entries` or `credential_ttl`, a `search_fanout_max` outside 1 to 999, an empty `allowed_hosts` or a blank
entry in it, or a `time_server` that is blank or has surrounding whitespace.
`maintenance_review_intervals` also rejects invalid catalog names, malformed
durations, durations below 86,400 seconds and durations beyond the supported
range. Omitted catalogs use 30 days. It also refuses a zero `oauth.*`
lifetime, and a `public_url` that is not a URL, has a path, query or user
info, is `http` on a host other than a loopback one, or whose host no
`allowed_hosts` entry admits (an entry with a port must match the URL's
port).

For `[redis]` it refuses a `url_env` that is empty or holds a character
other than a letter, digit or `_`, and a `key_prefix` that breaks the rule in
the table. Unlike the rest of the file, the table refuses a key it does not
know, so an inline `url` is an error. `serve` then refuses to start when the
variable is unset or blank, its value is not a Redis URL, or Redis does not
answer `PING`.

For `[auth]` it refuses a zero duration or one over ten years,
`max_keys_per_user` over 1000, and a `trusted_proxy_header` that is not a
header name. An `[[auth.oidc]]` table needs `public_url`, and each needs
every key but `scopes`. Unlike the rest of the file, it refuses a key it does
not know, so a misspelt key or an inline `client_secret` is an error. It
also refuses an `id` that is malformed or listed twice, an empty `label`,
`client_id` or `client_secret_env`, and an `issuer` that is not `https`
(`http` only on a loopback host) or has a query or fragment. For `[mail]`
it refuses a `from` that is not one address, an `smtp` provider without a
`[mail.smtp]` table, with a blank host, port 0, or `username` without
`password_env` (or the reverse), and a `postmark` provider without a
`[mail.postmark]` table or with a blank `token_env` or `message_stream`,
and an `ses` provider with a blank `region`. For `[feedback]` it refuses a
`forminit_form_id` that breaks the rule in the table (so it can never change
the URL's host or path), an `api_key_env` that is not a variable name, and
`max_per_hour` or `retention` out of range; like `[redis]`, the table
refuses a key it does not know, so an inline `api_key` is an error.
`serve` also refuses to start when a variable named by `client_secret_env`,
`password_env`, `token_env` or `feedback.api_key_env` is unset or empty,
naming the variable (never a value), and when the `ses` provider finds no
AWS region. The other
subcommands never read those variables nor look for AWS credentials:
`user invite` and `mail test` only queue mail, which a running server sends.

Like `[auth]`, the `[telemetry]` tables refuse a key they do not know. They
also refuse an `[telemetry.otlp]` table with none of `endpoint` with
`headers_env`, or `sentry_dsn_env`; one with both forms, or with only one of
`endpoint` and `headers_env`; an `endpoint` that is not an `http` or `https`
URL with a host, or that has a user name, password, query or fragment; a
`signals` list that is empty, repeats a name, names an unknown signal, or has `metrics` with `sentry_dsn_env`; a
`sample_ratio` outside `0` to `1`; a zero `export_interval`; a filter that does
not parse; and a `telemetry.prometheus.listen` that overlaps `listen`.
`serve` also refuses to start when a variable named by `headers_env` or
`sentry_dsn_env` is unset or blank, naming the variable (never a value). See
[Telemetry](#telemetry).

```toml
listen = "[::]:8080"
allowed_hosts = ["okf-mcp.example.com"]
default_catalog = "notes"
time_server = "time.example.com"
session_idle_timeout = "2h"
max_import_bytes = "64MiB"
maintenance_review_intervals = { pg = "30d" }
concept_outline_chars = 20000
database_max_connections = 16
overlay_cache_bytes = "256MiB"
public_url = "https://okf-mcp.example.com"   # turns on OAuth; see "OAuth connectors"

[oauth]
access_token_ttl = "24h"
refresh_token_ttl = "30d"
refresh_reuse_grace = "60s"
idle_grant_revoke_after = "30d"
code_max_expiry = "7d"
dcr_enabled = true
cimd_allow_private = false

[auth]
password_sign_in = true
max_keys_per_user = 20
max_key_lifetime = "90d"
console_session_idle = "12h"
console_session_absolute = "7d"
trusted_proxy_header = "x-forwarded-for"   # behind an ingress only

[[auth.oidc]]
id = "example"
label = "Example ID"
issuer = "https://id.example.com"
client_id = "okf-console"
client_secret_env = "OKF_OIDC_EXAMPLE_SECRET"

[mail]
provider = "smtp"
from = "okf <okf@example.com>"

[mail.smtp]
host = "smtp.example.com"
port = 587
starttls = true
username = "okf"
password_env = "OKF_SMTP_PASSWORD"

[telemetry]
service_name = "okf-mcp"
environment = "production"

[telemetry.stderr]
format = "json"
filter = "warn,okf_mcp=info"

[telemetry.otlp]
endpoint = "https://otlp.example.com"
headers_env = "OKF_OTLP_HEADERS"
export_interval = "60s"

[telemetry.prometheus]
listen = "[::]:9464"
```

`[oauth]`, `[auth]`, `[mail]` and `[telemetry]` are TOML tables, so they come after every
top-level key: a top-level key written below one would land inside it.

- **IPv6:** on an IPv6 or dual-stack cluster, listen on `[::]:8080`. Linux
  binds it dual-stack, so IPv4 still works. With the default `0.0.0.0:8080`,
  probes and traffic that reach the pod's IPv6 address are refused, and the
  pod never becomes ready.
- **Behind an ingress:** `allowed_hosts` must name the public host. With the
  default loopback list, every `/mcp` request through the ingress gets 403.

### The environment

| Variable | Meaning |
|---|---|
| `DATABASE_URL` | The Postgres URL (or `--database-url`). Never logged; `--help` hides its value |
| `OKF_MCP_CONFIG` | The configuration file (or `--config`) |
| `OKF_MCP_LISTEN` | Overrides `listen` (or `serve --listen`) |
| `PGSSLMODE` | TLS to Postgres; `verify-full` in production (section 4) |
| `PGSSLROOTCERT` | The CA certificate file that `verify-full` checks the database's certificate against |
| `RUST_LOG` | The stderr filter only, and it overrides `[telemetry.stderr] filter`; it does not change what is exported. **Leave it unset:** the default is `warn,okf_mcp=info,tower_http=info`, which logs the server's own events and one line per request. Setting `info` everywhere adds noise from every dependency |
| `OKF_OTLP_HEADERS` (any name) | The OTLP receiver's headers as `name=value,name=value`, or `none`, named by `telemetry.otlp.headers_env`. Keep it in a Secret |
| `OKF_SENTRY_DSN` (any name) | A Sentry DSN, named by `telemetry.otlp.sentry_dsn_env` |
| `OTEL_SDK_DISABLED` | `true` turns OTLP export off without editing the file |
| `OTEL_RESOURCE_ATTRIBUTES`, `OTEL_TRACES_SAMPLER`, `OTEL_EXPORTER_OTLP_HEADERS`, `OTEL_BSP_*` | Read by the OpenTelemetry SDK as it defines them ([Telemetry](#telemetry)). `OTEL_EXPORTER_OTLP_HEADERS` is merged over `headers_env`'s value |
| `OKF_REDIS_URL` (any name) | The Redis URL, named by `redis.url_env`: `redis://` or `rediss://`, possibly with a password (`rediss://:password@redis.example.internal:6379`). It can hold a credential, so keep it in a Secret. It is never logged, recorded in a span or put in an error: errors name the variable and the Redis error's kind |
| `SSL_CERT_FILE` | A CA bundle for `rediss://` to a Redis whose certificate a private CA signed (the system roots are read otherwise) |
| `OKF_OIDC_EXAMPLE_SECRET` (any name) | An OIDC client secret, named by its provider's `client_secret_env` |
| `OKF_SMTP_PASSWORD` (any name) | The SMTP password, named by `mail.smtp.password_env` |
| `OKF_POSTMARK_TOKEN` (any name) | The Postmark server token, named by `mail.postmark.token_env` |
| `AWS_REGION`, `AWS_CONTAINER_CREDENTIALS_FULL_URI`, ... | Read by the AWS SDK for the `ses` provider only: the region when `mail.ses.region` is unset, and credentials through the SDK's standard chain. EKS Pod Identity sets the credential variables itself; never set static keys for okf ([Mail](#mail)) |

Logs go to stderr. They never carry the database URL, an API key or a
document's text, nor a password, session id, CSRF token, setup or reset
link, or client secret. Each request's line names the method and the matched
route (`/api/v1/users/{name}/sessions/{id_prefix}`), never the literal path
or the query, so neither an id in a path nor an emailed link's token reaches
the log. The `log` mail provider is the one exception: it logs whole
messages, links included, which is why it is for development only. What
leaves the pod over OTLP is much narrower than stderr; see
[Telemetry](#telemetry) for what is never exported and why stderr stays
sensitive.

Set `OKF_MCP_CONFIG` in the container's environment rather than passing
`--config` as an argument: then `kubectl exec ... -- okf-mcp <subcommand>`
reads the same file as the server, with no extra flags.

## 4. Postgres

- **One database and its owner role**, for example `okf_mcp` owned by
  `okf_mcp`. The server needs no superuser. The migrations create three
  extensions, `citext` (okf's) and `pgcrypto` and `hstore` (apalis', for the
  mail queue). All three are trusted extensions, so the database's owner can
  create them (Postgres 13 or later). **Connections per replica:**
  up to `database_max_connections` (16 by default) for requests, plus 1 for
  the change-feed listener (it holds one connection for the life of the
  process, on its own one-connection pool, so it never takes a request slot),
  plus `store_max_connections` for the pool of the store `main`, which is in
  the account database, plus up to 3 for the worker pool that the mail and housekeeping workers
  share (the feedback worker, if on, has its own 3). For N replicas that is
  `N x (database_max_connections + store_max_connections + 4)`, plus `3 x N`
  with the feedback worker on, plus `N x (store_max_connections + 1)` on the
  database of each separate store (its pool and its change-feed listener, held
  while the pool is open and outside `store_max_connections`; a store inside
  the account database has no listener, the account listener hears it). See
  [Catalog stores](#catalog-stores). Keep the database's `max_connections` above that, with
  room for the command line, `okf-mcp migrate` and your own tools.
- **Two schemas.** okf's tables are in `public`; the mail queue is apalis'
  `apalis` schema, which `migrate` sets up after okf's migrations, with its
  own migration table (`apalis._sqlx_migrations`).
- **Migrations** are embedded in the binary and run on every start, by `serve`
  and by every subcommand. `okf-mcp migrate` applies them and exits, for a
  separate deploy step if you want one. They are forward-only: a binary older
  than the schema refuses to start.
- **A migration never waits long for a lock.** While it runs, `migrate` (and
  so `serve`, and `okf-mcp stores migrate` for a store) sets the connection's
  `lock_timeout` to 10 seconds, so an `ALTER TABLE` queued behind a long
  transaction cannot make every query on that table queue behind it without
  limit. When the wait runs out the start fails with `busy: a migration waited
  more than 10000 ms for a lock another transaction holds and was rolled back
  ...; retry` (Postgres' own report, `canceling statement due to lock
  timeout`, is in the log). **A retry is safe:** an ordinary migration is
  rolled back with its history row, and an index built `CONCURRENTLY` that
  was cut short leaves an INVALID index that the next start drops and builds
  again. Find the long transaction (`pg_stat_activity`, oldest `xact_start`),
  let it end or end it, and start again; a replica that fails this way
  restarts under its orchestrator on its own. The bound does not apply to the
  build's own run time, only to waits for locks, and it is put back when the
  migration ends.

### TLS

Turn on verified TLS with two environment variables, which sqlx reads
beneath the URL:

```sh
PGSSLMODE=verify-full
PGSSLROOTCERT=/etc/okf-mcp/pg-ca/ca.crt   # the database's CA certificate, mounted as a file
```

- `verify-full` checks the certificate against the CA and the URL's host name
  against the certificate's names.
- **The URL must not name `sslmode` or `sslrootcert`.** Parameters in the URL
  override the environment, so `?sslmode=prefer` in the URL would silently
  turn verification off. Check the URL for those parameter names without
  printing it, since it carries the password.
- **Spell it exactly.** sqlx ignores a `PGSSLMODE` it cannot parse (say,
  `verify_full`) and falls back to the default below, without a warning.
- **Without `PGSSLMODE`**, sqlx's default is `sslmode=prefer`: it tries TLS
  without verifying the certificate, and falls back to plain text if the
  server has no TLS. That is fine on a laptop, not in production.
- **Developers:** with `PGSSLMODE=verify-full` exported in your shell, the
  plain database tests fail, because their throwaway Postgres has no TLS and
  sqlx reads the variable beneath their URLs. Unset it, or set
  `PGSSLMODE=prefer`, before `cargo test`.

### CloudNativePG

With a CloudNativePG `Cluster` named `okf-mcp-db`:

- `DATABASE_URL` is the `okf-mcp-db-app` secret's `uri` key. Its host is
  `okf-mcp-db-rw`, which the server certificate names, so `verify-full` holds
  inside the namespace.
- `PGSSLROOTCERT` is the `okf-mcp-db-ca` secret's `ca.crt` key, mounted as a
  file. Mount only that item: the secret also holds the CA's private key,
  which the server never needs.
- CloudNativePG's default `pg_hba` also accepts plain connections, so TLS is
  the client's choice: set `PGSSLMODE`.

### Backups

Backups belong to the database, not the server. With CloudNativePG, use the
barman-cloud plugin: WAL archiving and base backups to the site's object
store (section 9).

## Catalog stores

A **store** is a named place where catalogs keep their data. The account
database holds everything about people and access (users, teams, keys,
grants, sessions, settings, the registry of catalogs and stores, and the
audit of account actions). A store holds the catalogs' own data in a schema
named `catalog`: documents and their revisions, changesets, maintenance
runs, each catalog's generation and the catalog audit
(`catalog.catalog_audit_entries`).

- **`main`** is the store every deployment has. Its schema is inside the
  account database, so a deployment that adds no store keeps one database.
  Every existing catalog is on `main`.
- A store on **its own database** (location `host`) keeps a team's catalog
  data apart, for example on another Postgres cluster. Add one with
  `POST /api/v1/stores` or `okf-mcp stores add` ([Adding a store](#adding-a-store)).
- Each catalog is on exactly one store, chosen when it is created (see
  [Placement](#placement)) and never changed. Nothing moves a catalog between
  stores.
- Every store, `main` included, has a connection pool of its own, opened on
  its first use, and each pool's `search_path` is fixed at connect (`catalog`
  for a store, `public` for the account database). A query for account tables
  sent to a store, or the reverse, fails with "relation does not exist", even
  when the store is inside the account database.

### Adding a store

`okf-mcp stores add NAME --host H [--port 5432] --database D
--credential-kind KIND --credential-ref REF`, or `POST /api/v1/stores`
([Catalog stores API](#catalog-stores-api)), needs an admin that reaches every
catalog. Create the database and its login role first; the server creates the
`catalog` schema and its tables itself. The add:

1. refuses a name, or a host, port and database, that already has a row,
   before the new database is touched (a refused request claims nothing);
2. connects to the new database (a store that cannot be reached is refused,
   and the reply names no host, port or credential; the log has the cause);
3. refuses a database that holds the account tables (the account database
   under another host name) or whose `catalog.store_identity` names another
   registered store (an identity naming a store with no row is left by an
   add that failed after step 4 and is replaced, so a retry works; adds of
   one physical database take turns, whatever host name or store name they
   use): each physical database answers to one store name, whatever host
   name reached it, so adding the same database twice under two names is
   refused;
4. migrates it (`migrations/catalog/`: the baseline, then the identity row)
   and stores the row. Every replica then drops any pool it had for that name.

The stores table never holds a password. `credential_ref` says where to get
it:

| `credential_kind` | `credential_ref` | The login |
|---|---|---|
| `env` | The name of an environment variable holding the password. Set it on every replica and on the host that runs `okf-mcp stores migrate` | Host, port and database come from the row and the password from the variable; the user, TLS settings and other connection options are the account database's own (the account database's password is never sent to another host) |
| `secrets-manager` | A secret's ARN, for example `arn:aws:secretsmanager:eu-west-1:111122223333:secret:okf/acme-AbCdEf`. The secret's value is JSON with `username` and `password` (the format RDS-managed secrets use) | The secret's `username` and `password`. Read when the pool opens, and again when the database refuses a login |
| `iam` | The database user the token is signed for (the user must be allowed to log in with IAM) | The server signs a short-lived RDS IAM token with the AWS credentials it runs with (the same chain as the mail sender's SES client) and replaces the pool's token every 10 minutes and when a login is refused. The token is signed for the region in an RDS host name (`name.id.REGION.rds.amazonaws.com`); for any other host name (a proxy, a CNAME) the server's own region is used, and there is no per-store region setting |

When a connection is refused its login (the secret rotated, the token
expired), the server fetches a fresh login and tries once more, so the
request that met the refusal succeeds. It waits for the refresh, at most
`store_connect_timeout`, and acquires one more time; requests refused
together wait on the same refresh, which fetches the login once. Only a
refused login is retried: a timeout or a network failure is not, and a login
still refused after the refresh ends in `StoreUnavailable`. The refresh runs
at most once every 10 seconds, so a second rotation inside that gap can fail
requests until the next one. The retry attempt has its own bound, the pool's
connect timeout, so a request that waited for a refresh can take up to twice
`store_connect_timeout` in all. A store using `env` has no refresh and no retry.
The AWS credentials are used only for `secrets-manager` and `iam` stores.

### Placement

A new catalog goes on its owner team's pin if the team has one, else on the
**default store** (the `default_store` setting, `main` until changed).
`okf-mcp stores set-default STORE`, `stores pin TEAM STORE` and `stores unpin
TEAM` change that, and each change is audited. Changing the default or a pin
affects catalogs created afterwards only: existing catalogs stay where they
are. `stores list` shows each store, which one is the default, its credential
kind and reference, and how many teams are pinned to it. There is no command
or route to remove a store.

### Migrating stores

A store's schema migrations (`migrations/catalog/`, history in
`catalog._sqlx_migrations`) run automatically the first time a process opens
that store's pool, under an advisory lock, so replicas starting together
migrate once. A store whose migration fails, or whose identity names another
store, is `StoreUnavailable` for that process (logged with the store's name);
nothing is cached, so the next request tries again, and no other store is
affected.

`okf-mcp stores migrate` migrates every store now, in name order, trying all
of them. It prints one line per store: the name, a tab, then `up to date`,
`<n> applied`, `adopted, <n> applied` or `failed: <error> (the log has the
cause)`. If any failed, it exits with an error ending `<n> of <m> stores could
not be migrated`. Run it as a deploy step beside `okf-mcp migrate` (which
migrates the account database and adopts `main`) when you want a failure to
stop a rollout instead of showing up on the first request.

### Connections

A process holds, at most:

- on the account database: `database_max_connections` for requests, 1 for the
  change-feed listener, up to 3 for the mail and housekeeping workers (3 more
  for the feedback worker, when on), and **`store_max_connections` for the
  pool of the store `main`**, which is inside it;
- on each separate store's database it has opened: `store_max_connections`,
  and 1 for that store's change-feed listener, which is outside that limit.

For N replicas, each database must allow at least:

| Database | Connections |
|---|---|
| Account | `N x (database_max_connections + store_max_connections + 4)`, plus `3 x N` with the feedback worker |
| Each separate store | `N x (store_max_connections + 1)` |

With the defaults (16 and 8) and 3 replicas, the account database needs 84 and
a separate store 27. These are upper bounds: a store's pool opens on first
use and closes a connection idle for `store_idle_timeout`. Leave room for the
command line, `okf-mcp migrate`, `okf-mcp stores migrate` and your own tools.

### Upgrading to the release with separate stores

The release that moves the catalog tables from `public` to the `catalog`
schema (account migrations `0041_catalog_schema` and `0042_catalog_audit`)
removes tables and columns that older replicas still query. **It is not a
rolling upgrade**; use a maintenance window:

1. **Snapshot the database** (section 9). Migrations are forward-only; the
   snapshot is the way back.
2. **Scale `serve` to zero**, every replica, so no older binary runs against
   the database.
3. **Run `okf-mcp migrate`** with the new image. It applies the account
   migrations, then adopts `main`: it records the catalog baseline as already
   applied, because the moved tables exist, and writes `main`'s identity row.
   Then run `okf-mcp stores migrate`; it should report `main` as `up to date`.
4. **Start the new release.**

Catalog audit entries written before the upgrade stay in the account audit
table, and the audit log reads them and the newer ones as one list, each
exactly once ([Audit API](#audit-api)). The earlier releases of the milestone
(registry, placement) upgrade rolling as usual, because every catalog is still
on `main`. To go back, restore the snapshot.

### When a store cannot be reached

A request that needs a store that does not answer within
`store_connect_timeout` fails with `StoreUnavailable` (HTTP 503, MCP error
`StoreUnavailable`: "the catalog's data is temporarily unavailable; retry
later"). The message never names the store; the log does. Catalogs on other
stores are unaffected. A call remembers a store it found down and does not
spend another timeout on it (with the gaps below). Loading a catalog from a
store may take up to 6 times the timeout before the store is reported
unavailable.

- **Writes** to a catalog on a lost store fail with `StoreUnavailable`.
- **A changeset handle** does not say which store holds it, so a lookup asks
  the stores in turn, `main` first, and stops at the one that has it. A handle
  found before a lost store is reached still works. The lookup fails with
  `StoreUnavailable` only when it meets a lost store before it finds the
  handle, which includes a well-formed handle no store holds: it cannot be
  ruled out while a store it must ask is down. A malformed handle is refused
  without asking any store.
- **Merging reads** return what they could and name the rest: `search`,
  `list_catalogs`, `list_changesets` without `catalog`, `GET /api/v1/search`,
  `GET /api/v1/catalogs`, `GET /api/v1/changesets` and the console's search
  add `unavailable`, a sorted list of catalog ids (only catalogs the caller
  reaches), omitted when every store answered.
- **First-match reads** (`get_concept`, `read_document` and `history`
  without `catalog`) walk the connection's selection in order. They answer
  from a catalog before the first lost store; if none of those has the
  document, they fail with `StoreUnavailable` instead of `NotFound` or an
  answer from a later catalog, because the catalog on the lost store might
  have held it. Catalogs after the lost one are not consulted.
- **`cited_by`** fails with `StoreUnavailable` if any selected catalog's store
  is lost; it does not merge.
- A catalog in memory whose refresh finds its store lost is unavailable too;
  it is not served from its stale copy.
- **Housekeeping** runs per store, so a lost store fails only its own jobs;
  `okf_changesets_open` carries its last count. `GET /healthz` checks the
  account database only, so a lost store does not make a replica unready.

A query run through the store's executor (`CatalogDb::executor()`, which
every store-layer read takes) classifies its own driver errors (issue #995):
a pool timeout, a closed pool, a transport or TLS failure and a refused login
are `StoreUnavailable` wherever in the query they happen, while a constraint
violation or a bad statement stays a `Database` error (HTTP 500). The
exception is a statement on a connection taken from the pool (`acquire`) or
inside a transaction (`CatalogDb::begin`): the code that takes the connection
classifies its errors itself, and a path that does not can still report a
lost store as `Database`. Known gaps, each tracked as an issue: two paths
probe a store already found down again within one call (#1017).

### What a separate store does and does not separate

A catalog's content, history and catalog audit (paths, commit messages) are
only in its store's database. That is all a store separates. These stay
shared, so a store is not a secrecy boundary against whoever runs the server
or the account database:

- the **account database** holds users' names and emails, credential labels,
  team and catalog **names**, who may reach them, and the audit of account
  actions, which names catalogs and teams;
- the **job queue** is in the account database; a housekeeping job names a
  store and a slot, never a catalog's content;
- every replica's **in-memory caches** hold the content of the catalogs it
  has read, from whichever store;
- **logs** name catalogs and stores (the local log names the store of a
  `StoreUnavailable`); [Telemetry](#telemetry) says what is exported.

Say which of these a deployment shares when you describe a store as
segregated.

## Redis

Redis is optional. It is what lets `okf-mcp serve` run as several replicas
behind one Service (section 1 says what it holds and why). Turn it on with a
`[redis]` table (section 3); without the table none of this applies.

```toml
[redis]
url_env = "OKF_REDIS_URL"   # the variable holds redis://... or rediss://...
key_prefix = "okf:"         # the default; names this deployment's keys
mode = "standalone"         # or "cluster"
```

- **What to run.** Redis 6.2 or later (the server uses `GETDEL` and Lua
  scripts), or Valkey. Either:
  - `mode = "standalone"`: one primary, or a managed service's single
    endpoint. The client reconnects by itself.
  - `mode = "cluster"`: a cluster-aware client that follows the cluster's
    topology. **ElastiCache Serverless** (for Valkey) is the
    example: it runs only in cluster mode and requires TLS, so use
    `mode = "cluster"` and a `rediss://` URL, such as
    `rediss://redis.example.internal:6379`. A self-run cluster works the same
    way.
  - Sentinel is not supported. Neither is spreading the keys over several
    cluster slots: see "The hash tag" below.
- **The hash tag.** Every key and channel is named `{<key_prefix>}<rest>`, for
  example `{okf:}oauth:code:<hash>`, in both modes. The braces are a Redis
  hash tag: Redis hashes only the text inside them, so every key the server
  uses lands on one cluster slot. That is what lets a script that touches
  several keys (the session binding scripts, the swap of a per-session index,
  the compare-and-set) run on a cluster, where a command over keys in
  different slots fails with `CROSSSLOT`. The cost is that all of okf-mcp's
  keys sit on one shard. That is fine for short-lived sign-in state of a few
  kilobytes per entry, and ElastiCache Serverless scales a slot's capacity
  itself. It is also why `key_prefix` cannot contain a brace. A standalone
  deployment has the same names.
- **Start-up.** `serve` connects and sends `PING` before it listens. It
  refuses to start when the variable is unset or blank, the value is not a
  Redis URL, or Redis does not answer: the connection gives up after 5
  seconds, and each command waits at most 2. The message names the variable
  and the Redis error's kind, never the URL.

- **The Redis user's permissions.** A deployment whose Redis user is
  restricted by an ACL must grant it every command the server runs, on every
  key under `{<key_prefix>}` and, for the announcements, on the channels under
  it:
  - the connection: `PING`, `HELLO`, `AUTH` (`+@connection`);
  - keys: `GET`, `MGET`, `SET`, `GETDEL`, `DEL`, `EXISTS`, `PEXPIRE`, `PTTL`, `INCR`
    (`+@read +@write +@keyspace`);
  - hashes, for the rate-limit buckets: `HMGET`, `HSET`;
  - scripts: `EVALSHA`, `EVAL`, `SCRIPT LOAD` (`+@scripting`). **Redis checks
    every command a script calls against the same ACL**, so the user needs
    the commands above for the scripts too;
  - pub/sub: `PUBLISH`, `SUBSCRIBE` (`+@pubsub`);
  - with `mode = "cluster"`, also `CLUSTER SLOTS`, which the client sends to
    learn the topology (`+cluster|slots`, not in any category above);
  - **`TIME`**, which the rate limiter's script calls so that every replica
    counts on Redis's one clock. `TIME` is in none of the categories above
    (it is `@fast`), so a user granted those and denied `-@dangerous` must be
    given `+time` by name. Without it the script fails with `ACL failure in
    script: User ... has no permissions to run the 'time' command` on every
    call, and every rate limit falls back to each replica's own bucket.

  A working access string is `on >password ~{okf:}* &{okf:}* -@all +@read
  +@write +@keyspace +@connection +@transaction +@scripting +@pubsub +time
  -@dangerous` (with your `key_prefix`; add `+cluster|slots` for
  `mode = "cluster"`). The server checks this at start-up:
  it runs the limiter's script once against a throwaway key and, if Redis
  refuses it, logs an **error** ("the shared rate limiter's script cannot run
  on Redis") and reports `"rate_limits": "degraded"` on `/healthz` (section 7).
  The server still starts; limits are decided per replica until the ACL is
  fixed. Fallbacks are counted per limiter in `okf_ratelimit_fallbacks_total`
  and logged as a warning at most once a minute per limiter. In exported logs
  the limiter `oauth_token` is named `oauth-tok`, because Sentry's scrubbing was seen
  replacing the value `oauth_token` with `[Filtered]`; the metric labels
  keep their names.
- **Why the script uses `TIME`.** One clock for every replica means no
  replica's clock skew refills a bucket early. Passing the caller's clock
  instead would drop the `+time` requirement but give that up, so the script
  keeps `TIME`.

### What Redis holds

`{p}` stands for `{<key_prefix>}`, and `h(x)` for the hex SHA-256 of `x`. A
session id, a flow cookie and a pending authorization id are credentials, so
none appears in a key name as the client sent it. Neither does a raw console
session id: the keys and values that refer to one hold its hash.

| Key | Holds | TTL |
|---|---|---|
| `{p}mcp:state:h(session)` | What a replica needs to restore the MCP session (rmcp's session state) | `session_idle_timeout`, renewed on every admitted request |
| `{p}mcp:bind:h(session)` | The id of the API key that opened the session | The same, renewed with it |
| `{p}oauth:pending:h(id)` | A consent page in progress | 10 minutes, fixed |
| `{p}oauth:code:h(h(code))` (the server's own hash of the code, hashed again by the store) | An authorization code | 60 seconds, fixed. A used code stays until then, so a second use is seen as a replay and the token family is revoked |
| `{p}oauth:access-revocations` | A counter of access-token revocations | None |
| `{p}oidc:flow:h(cookie)` | An OIDC sign-in, link or re-authentication flow | 10 minutes, fixed |
| `{p}oidc:reauth:h(<hash of the console session id>)` | A finished OIDC re-authentication waiting for `DELETE /api/v1/me` | 5 minutes, fixed; taken once |
| `{p}oidc:link:<hash of the console session id>` | Which flow that console session has in progress | 10 minutes, renewed on use |
| `{p}passkey:ceremony:h(cookie)` | A passkey registration or sign-in ceremony | 5 minutes, fixed |
| `{p}passkey:session:<hash of the console session id>` | Which ceremony that console session has in progress | 5 minutes, renewed on use |
| `{p}ratelimit:<limiter>:h(id)` | A rate limit's token bucket: the tokens left and when they were counted. `<limiter>` is the `limiter` label of `okf_ratelimit_rejections_total`; `id` is the address key, the normalized email or the user id, or `global` for the OAuth limits | The time an empty bucket takes to refill (15 minutes for sign-in, an hour for the reset limits, 10 minutes for link starts, 60, 30 and 20 seconds for OAuth registration, token and consent), renewed on every request |
| channel `{p}catalog-generation` | Announcements that a catalog's snapshot is out of date: `<catalog id> <generation>` | Not stored |

- **Values** are JSON with a version. A value of another version, or one that
  does not decode, counts as absent and is counted as a `decode` error. After
  a rolling upgrade that changes a value's shape, a flow in progress starts
  again; nothing is misread. A rate-limit bucket is the exception: a Redis
  hash of two numbers (`t`, the tokens left, and `at`, Redis's clock in
  milliseconds when they were counted), read and written only by the
  limit's Lua script.
- **Redis is a secret store.** Values hold what a flow needs for at most ten
  minutes: PKCE verifiers, nonces, CSRF tokens and passkey ceremony state. It
  has the same trust as Postgres: password-protected, on a private network,
  with TLS wherever it crosses hosts.
- **Single use across replicas.** A flow or ceremony is read and deleted in
  one command, so it finishes once whichever replica takes it. A code or
  pending authorization is changed by a compare-and-set that retries up to 8
  times and then answers 503, so two replicas racing for one code produce one
  exchange and one replay.
- **Administrative changes are not announced through Redis.** Keys, users,
  teams, grants and settings reach every replica through the Postgres change
  feed. A replica's settings are also checked against `admin_generation`
  (a counter every administrative change bumps) once they are older than
  `credential_ttl`. The `admin-generation` channel of releases up to 0.29.0 is
  gone; nothing publishes or subscribes to it.
- **Catalog snapshots** are announced, not stored. After a publish, an HTTP
  import, or (from the command line) an import or `catalog repair-indexes`
  commits, the writer publishes the catalog's id and new generation, and every
  replica refreshes that catalog's snapshot when the announcement arrives.
  This is best-effort, eventually consistent propagation, not
  read-your-writes: the write's answer does not wait for the announcement or
  for any other replica's refresh, so a client whose next request lands on
  another replica can still read the old text for a moment (normally well
  under a second). The same write also sends a change message through
  Postgres, which reaches every replica without Redis; if both are lost, the
  replica rereads the catalog's row after `catalog_ttl` at the latest and
  refreshes the snapshot on its next use. A replica that already reached the
  generation (its own publish) does nothing; one that has not cached the
  catalog, or has deleted it, does nothing either, so an announcement never
  loads a catalog (catalogs load on first use). Refreshes of one catalog from
  an announcement, a change message, a request and a local publish wait for
  one another and build each generation once; a snapshot never moves back. A
  replica's `pending_changes`, and its reads through a changeset, do give a
  guarantee for the changeset's own entries: they refresh first when an entry
  was made on a newer revision than the replica's snapshot holds, so a
  changeset is never diffed or read against text older than its entries'
  bases, and `pending_changes` never reports `current_revision` lower than
  `base_revision`.
- **The access-token cache** checks the revocation counter on every lookup. A
  revocation increments it, and a replica that sees a new value clears its
  cache. A cached token is also trusted for at most 60 seconds, as a backstop
  for a crash between the commit and the increment.

### When Redis fails

After start-up, the flows that depend on Redis **fail closed**. Nothing falls
back to memory, because that would split the state between replicas.

| What | The answer |
|---|---|
| An MCP request (the binding check, a restore, `initialize`'s binding, a `DELETE`) | 503. The client retries, or initializes again |
| The consent page and its form | 503, the busy page ("The server is busy. Start again from your client in a minute."), on a failed read, update or removal alike. A Deny or Approve whose removal of the pending entry fails answers 503: no denial is forwarded and no code is minted |
| The token endpoint | 503 `server_error` (body "internal error"). If Redis fails after a code was marked used, the token family is revoked and the client starts again |
| An OIDC start or callback, or a link | 503 ("Sign-in is unavailable right now; try again shortly." on the callback). The flow cookie is kept, so a reload once Redis answers can finish the flow |
| A passkey step | 503 |
| A rate limit | Decided by the replica's own in-memory bucket, as without `[redis]`: the same budget, counted per replica until Redis answers again (fails open, never unlimited) |
| `GET /healthz` | Still 200: `"redis": "unavailable"` (section 7) |
| A catalog read or write | Unaffected: Postgres alone serves them |

- **Subscriber reconnects.** The client resubscribes by itself after a
  reconnect; a subscription that stays disconnected for 10 seconds is replaced
  by a new one, retried with a wait that grows from 1 to 30 seconds.
- **Subscriber log lines.** The catalog subscriber (`subscriber="catalog"`)
  logs at `info` every time it is subscribed (`resubscribe`
  is `true` after the first, also when a replaced subscription is followed
  by a new one; `disconnected_ms` says how long the outage lasted, counted
  from the first loss, so it covers a replaced subscription and failed
  subscribes, and is absent on the first subscribe), and at `warn` when the connection is lost, when a lost
  connection outlasts the 10-second grace and the subscription is replaced,
  when the subscription ends and when a subscribe fails. A replica with
  `warn` lines and no later `info` line is not hearing announcements. No
  line carries the Redis URL, a channel payload or a key. There is no
  gauge: `okf_redis_errors_total{operation="subscribe"}` counts the losses.
- **Catalog announcements** fail the same way: a failed publish is counted and
  logged. When a replica's catalog subscriber (re)subscribes, with the same
  reconnect and backoff, it marks every cached catalog row stale, so each
  catalog is checked against the database on its next use (nothing is
  reloaded at once). A lost announcement is normally covered by the Postgres
  change message, and at worst costs `catalog_ttl` before that replica's reads
  show the write; even a delivered one is applied asynchronously.
- **The access-token cache** skips the cache on a Redis error: the lookup goes
  to Postgres and caches nothing.
- **Rate limits fail open to the replica's own bucket.** Each limiter keeps an
  in-memory bucket per key as well, asked only when its Redis call fails. A
  request during an outage is limited as it was before limits were shared:
  each replica allows the configured budget, so the effective limit is that
  budget times the number of replicas, never unlimited. Failing closed would
  turn a Redis outage into refusing every password sign-in, reset request and
  OAuth token refresh, which need only Postgres; the durable account lockout
  (10 wrong passwords, Postgres) still holds. Each failure is counted as
  `limit` and logged at `warn` with the limiter's name. A call to an
  unreachable Redis waits up to the 2-second command timeout first, once per
  limit the request checks (a reset request checks two, so up to 4 seconds).
- **An entry Redis lost** (evicted, or Redis restarted without persistence)
  fails closed too: a 404 for a session, an unknown code, an expired flow. The
  person starts again. A lost rate-limit bucket starts full again: an idle
  one is dropped only once it has refilled anyway, but one evicted early, or
  lost in a restart, grants up to one budget sooner than it would have.
- **Counting.** Every failure is counted in `okf_redis_errors_total` by
  `operation` (the metric reference lists them) and logged with the operation
  and the error's kind. The error text stays in the local log only; no URL,
  key name or value is logged. Alert on the counter rising, not on
  `/healthz`.

### Sizing

An entry is about 1 to 4 KB (an MCP session about 1 KB). Redis holds only live
entries, which expire in minutes, apart from MCP sessions, which last until
idle for `session_idle_timeout` (2 hours by default). Count one session per
connected client and a few flows or ceremonies per person signing in. The
anonymous entries are bounded by the per-address start limits times the number
of replicas, and by Redis's own `maxmemory`: the in-memory caps on flows,
ceremonies, codes and pending authorizations apply without `[redis]` only. Each person or
console session holds at most one link flow and one passkey ceremony: starting
another replaces the first. A rate-limit bucket is a few hundred bytes, one per
address, email or user that made a limited request within its window; the
in-memory cap of 100,000 keys per limiter applies without `[redis]` only.

### Eviction

Set `maxmemory-policy` to `noeviction` or a `volatile-*` policy
(`volatile-lru`, for example), **not** an `allkeys-*` policy. The
revocation counter has no TTL, so an `allkeys-*` policy may evict it. The
server recreates a missing counter from a random seed, so a lost counter never
repeats a value a replica has seen and every cache is cleared at its next
lookup. Losing it costs a cache refill, not a missed revocation, but there is
no reason to allow it. **On ElastiCache Serverless AWS manages
`maxmemory` and eviction;** there is nothing to set. On a cluster or primary
you run, set the policy yourself and leave headroom so `noeviction` does not
start refusing writes (the server then answers 503 as above).

### TLS and AUTH

- Use `rediss://` wherever Redis is not on the pod's own host. It is required
  for ElastiCache Serverless. The server reads the system's root certificates;
  for a Redis whose certificate a private CA signed, point `SSL_CERT_FILE` at
  the CA bundle (mount it from a ConfigMap or Secret).
- Put the password in the URL (`rediss://:password@host:6379`) and the URL in
  a Secret named by `url_env`. Do not put it in the config file.
- Restrict the network: allow Redis from the okf-mcp pods only.

### Several replicas: what stays per replica

With `[redis]`, run as many replicas as you need (section 5's variant uses
two). What `[redis]` does not share:

- **`GET /mcp` streams.** A server-sent stream belongs to the replica that
  opened it. A client that loses it reconnects and may land elsewhere; the
  session carries on through Redis, but the stream does not resume.
- **Catalog reads are eventually consistent across replicas.** A publish or
  import on one replica normally shows on another within a fraction of a
  second, through its announcement ("Catalog snapshots" in the Redis
  section), but not before the write's answer returns: a client's next read
  on another replica may still see the old text. There is no read-your-writes
  guarantee across replicas. Clients and test authors: read through the
  changeset (or `pending_changes`, which is always current for its own edits)
  before publishing, or retry a read briefly after `publish`; the `publish`
  tool description says the same, and eval or MCPJam scenarios that fire calls
  rapid-fire are the likeliest to notice (`tests/mcpjam/README.md`). A lost announcement (Redis down, or the
  subscriber reconnecting) is covered by the Postgres change message the same
  write sends; if that is missed too, by the catalog row's lifetime,
  `catalog_ttl` (10 seconds by default). A catalog created or deleted on one
  replica shows on the others the same way. Within a
  changeset, `pending_changes` and reads through it refresh a replica that is
  behind the changeset's own entries.
- **Rate limits** (per address, per email, OIDC link starts, OAuth) are
  shared: every replica takes from the same bucket in Redis, so the configured
  limit holds however many replicas run, and a restart does not reset it.
  Only while Redis fails does each replica count on its own (the Redis
  section, "When Redis fails").
- **The mail worker** runs on every replica, with no per-pod setting.
  apalis takes each job with `FOR UPDATE SKIP LOCKED`, so two replicas never
  send the same mail at once. Each process opens up to 3 connections for
  the worker pool the mail and housekeeping workers share, so N replicas add
  `3 * N` to Postgres's `max_connections`, beside the request pools, plus
  `3 * N` more with the feedback worker on (its pool is its own). A replica that
  dies loses its mail in hand to another replica after about one minute (a
  heartbeat every 10 seconds, six missed); the mail shows as `sending` and
  cannot be deleted until then. Delivery is at least once: a replica killed
  after the provider accepted a mail but before the job was acknowledged
  (SIGKILL, out of memory, a grace period shorter than a send) causes a
  second send. On SIGTERM a replica starts no new send and finishes the one
  in hand, for at most `worker_shutdown_grace` (30 seconds by default, the
  longest a provider may take); keep `terminationGracePeriodSeconds` above it.
  The hourly `prune_workers` housekeeping job deletes the `apalis.workers`
  rows of stopped replicas.
- **The feedback worker** runs on every replica with `[feedback]` unless
  `[feedback] worker = false`, which you can set on all but one replica.
  Fetching a job is exclusive in normal operation, so
  two workers do not normally post the same job, but delivery is
  at-least-once (see [Feedback](#feedback)), and one worker is enough. The
  feedback rate limit is counted in Postgres, so unlike the
  limits above it holds across replicas.
- **Housekeeping** runs once per cluster as apalis jobs on queue
  `housekeeping`: `prune_changesets` (empty changesets a day old, every 5
  minutes), `prune_oauth` (idle OAuth grants, expired tokens and codes,
  unbound registered clients, sign-in events, emailed-link tokens, console
  sessions, mail jobs, finished housekeeping jobs and forwarded feedback,
  hourly), `prune_audit` (the audit log, daily), `prune_workers` (hourly),
  `cluster_stats` (every minute), `provision_catalogs` (every minute; see
  [Half-created catalogs](#half-created-catalogs)) and `cleanup_stores` (every
  minute; see [Store cleanup](#store-cleanup)). Every replica's ticker pushes each
  task's current slot once a minute; a unique key per slot makes the other
  replicas' pushes no-ops, so each slot runs once, on whichever replica's
  worker takes it. The work on catalog data runs as one job per catalog
  store, keyed `<task>:<store>:<slot>` (cluster jobs are `<task>:<slot>`):
  `prune_changesets`, the store half of `prune_audit` (each store's
  catalog events, past the same `audit_retention_days`) and the open
  changeset count of `cluster_stats`. The ticker reads the `stores` table on
  every push, so a store added while the replicas run gets its jobs within a
  minute. A store that is down fails only its own jobs, which are retried.
  A store job's own prune, count and write statements run under a 20 s
  `lock_timeout` and `statement_timeout`, so a held lock doesn't keep a
  worker waiting. Each step of a job (opening the store, the prune, the
  count, the account write) also has its own 30 s `tokio` backstop, so a
  job of several steps can run longer than 30 s in all, but no one step
  can. Finding and opening
  the store is bounded by `store_connect_timeout` instead, a store's
  first-open migrations by the backstop alone, since a migration can
  legitimately run long, and `prune_audit`'s read of the retention setting
  by its own timeout, outside the step backstops. A failed job is retried twice with a backoff; a slot
  missed while every replica was down is not caught up, and the next slot
  does the same work. `okf_housekeeping_runs_total` counts the runs. A
  replica looks a slot up before pushing it, so a slot already scheduled
  costs one indexed read and leaves nothing in the Postgres log. Only when
  two replicas push a new slot at the same moment does the loser hit the
  unique index: the Postgres log then shows one `duplicate key value
  violates unique constraint "idx_jobs_idempotency_key"` error, which the
  server ignores.
- <a id="half-created-catalogs"></a>**Half-created catalogs.** Creating a
  catalog takes three steps: the registry row is inserted with
  `provisioned_at` empty, the store side (the starter files) is written under
  the catalog's lock on its store, and `provisioned_at` is set. Until the
  last step the catalog does not exist for any lookup, listing or
  permission check, and its name is taken. A create that stops between the
  steps (a crash, a failed store connection) leaves a row that the
  `provision_catalogs` task, which looks at rows older than a minute, takes
  the same lock for: if the store side is there it sets `provisioned_at`; if
  not it records `catalog.deleted` with `failed_create: true` in the details
  and removes the row, freeing the name. A create that was only slow is never
  removed, because both sides take the lock, not because of the age: the
  create either commits first (and the task finishes the catalog) or waits
  behind the task, finds its row gone and fails with a retryable `Busy`.
  Each recovery attempt is bounded: its transactions run with
  `lock_timeout` and `statement_timeout` set to `store_connect_timeout`, and
  the whole attempt, including the store's first open and migration, is
  dropped after six times that (30 s by default). A store that stalls, or a
  catalog lock another connection holds, fails that attempt; the task skips
  that store's other rows for the rest of the run, keeps the first failure
  as its result (so the job is retried), and goes on to the catalogs on
  other stores. The row stays pending. There is no separate setting.
  The first release that writes unprovisioned rows must run only after every
  replica runs one that hides them (plan decision P11). Rows stuck for long
  show up as `SELECT id, name, created_at FROM catalogs WHERE provisioned_at
  IS NULL`; a failed job is logged as `recovering a half-created catalog
  failed`.
- **Cluster gauges.** `okf_changesets_open` and `okf_mail_queue_depth` come
  from the `cluster_stats` row, which every replica reads once a minute, so
  all replicas report the same value: use `max()` across replicas.
  `okf_changesets_open` is the sum of each store's last count
  (`cluster_store_stats`), so a store that cannot be reached contributes the
  count it had when it was last reached.
- **`okf_mcp_sessions_active`** counts the sessions each replica holds. A
  session restored on a second replica counts there too, so the sum over
  replicas can exceed the number of live sessions.
- **No session affinity** is needed on the Service or the Ingress.

## Caches and change propagation

Since milestone 18 the server holds a bounded amount of state, however many
catalogs, keys and users the database has. Nothing is loaded at start-up:
`serve` applies the migrations and starts serving, and each cache fills on
first use.

| Cache | Bounded by | Holds | Metrics (`cache` label) |
|---|---|---|---|
| Catalog snapshots | `catalog_cache_bytes` | A catalog's parsed files, built on first read | `catalog` |
| Changeset overlays | `overlay_cache_bytes` | A changeset's view of its catalog | `overlay` |
| Catalog directory | `catalog_directory_entries` | The catalog rows (id, name, owner, suspension, maintenance), by name, with no content | `directory` |
| Credentials | `credential_cache_entries` each | API keys by hash, OAuth grants by id | `credential`, `grant` |
| Users | `user_cache_entries` | Each user's admin flag, teams, entitlements and unlocks (the ceiling is resolved per catalog from these) | `user` |
| Labels | 100,000 entries (fixed) | The display labels of the users, teams and catalogs a response shows, read for just those names | `labels` |

- **A request pins what it uses.** A catalog evicted while a request reads it
  stays alive for that request; the next request loads it again.
- **Sizing `catalog_cache_bytes`.** A snapshot's weight is
  `1024 + 1024 x files + 8 x stored bytes` per catalog (the method is in the
  milestone 18 design, decision 2), so a count of catalogs and a stored size
  give the figure. Any budget is accepted: above 4 GiB the cache counts in
  coarser units (a power of two of bytes, rounded up, so a catalog is never
  undercounted), and `okf_cache_bytes` still reports bytes. Watch
  `okf_cache_bytes{cache="catalog"}` against the
  setting, and the hit ratio of `okf_cache_requests_total` for the same
  cache. A ratio that stays low with the cache at its limit, and
  `okf_cache_evictions_total{cause="size"}` rising, mean the working set is
  larger than the budget: raise it (and the pod's memory limit above it, with
  room for the overlays, the other caches and the process). A catalog larger
  than the whole budget is built for the request that wants it and not kept,
  with a warning naming its size.
- **Change propagation.** Every administrative write and publish queues a
  change message (Postgres `NOTIFY`, channel `okf_invalidate`) from its own
  transaction, so it is delivered only if the transaction commits. Each
  replica's listener applies it: it drops the named cache entries (a
  catalog row and snapshot, a credential, a user, a team's members, the
  settings). The replica that took the request applies its own messages
  before it answers, so the writer reads its own change. Delivery needs
  neither Redis nor an announcement, so any number of replicas may run.
  `okf_invalidations_lag_seconds` is the time from send to application.
  Suspending a team or lifting it sends two messages whatever the team
  owns: `team` (its label and members) and `team_catalogs` (every directory
  row it owns), so a team with a million catalogs costs two `NOTIFY`s, not
  a million. A replica of the previous release cannot read `team_catalogs`
  (an unknown kind counts in `okf_invalidations_unreadable_total`), and
  treats it as a lost message: it marks every cache stale, so it too refuses
  the suspended team's catalogs on its next request. That makes a rolling
  upgrade safe; the old replica rereads more than it needs to while it
  lasts.
- **What bounds a lost message.** While a listener is disconnected it loses
  messages, so when it reconnects every cached entry is marked stale (not
  dropped): each is read again on its next use, nothing is reloaded at once.
  A message lost while the listener stays connected is bounded by the TTLs:
  `credential_ttl` for credentials, users, labels and settings, and
  `catalog_ttl` for catalog rows (and so snapshots). Both are 10 seconds by
  default; a name that does not exist is remembered for 5 seconds. There is no
  background sweep.
- **Lists are queries.** `GET /api/v1/catalogs`, `/whoami`, `/labels` and MCP
  `list_catalogs` page through the directory by name (`after`, `limit`),
  rather than scanning memory; `GET /api/v1/search` refuses to search more
  than `search_fanout_max` catalogs unless it is told which, and refuses to be told more than that many. See the
  [API reference](#api-reference).
- **Optional: ingress hashing on `Mcp-Session-Id`.** A session works on any
  replica, so nothing depends on this. Routing each session to one replica
  keeps the catalogs it reads in that replica's cache, so there are fewer
  cold loads and each replica's cache holds a smaller share. With nginx:
  `hash $http_mcp_session_id consistent;` in the upstream block. A replica
  that leaves moves only its share of sessions.
- **Dashboards.** The database-wide gauges (`okf_changesets_open`,
  `okf_mail_queue_depth`) are the same on every replica: use `max()` across
  replicas, not `sum()`. The per-catalog gauges `okf_catalog_concepts` and
  `okf_catalog_generation` are gone (a series per catalog does not scale);
  `okf_cache_entries` and `okf_cache_bytes` say how many catalogs this
  replica holds, and `GET /healthz`'s `cached_catalogs` reports the same
  number.

## 5. Kubernetes

A complete, generic example. Apply it with `kubectl apply -n okf-mcp -f`, or
put it in your GitOps repository. It assumes:

- the CloudNativePG operator and the barman-cloud plugin are installed;
- ingress-nginx is the ingress controller, and cert-manager issues
  certificates through a `ClusterIssuer` named `letsencrypt`;
- a secret `okf-mcp-backup-creds` in the namespace holds the object store's
  `ACCESS_KEY_ID`, `SECRET_ACCESS_KEY` and `REGION` (create it however your site
  manages secrets).

```yaml
apiVersion: postgresql.cnpg.io/v1
kind: Cluster
metadata:
  name: okf-mcp-db
spec:
  instances: 3
  bootstrap:
    initdb:
      database: okf_mcp
      owner: okf_mcp
  storage:
    size: 10Gi
  plugins:
    - name: barman-cloud.cloudnative-pg.io
      isWALArchiver: true
      parameters:
        barmanObjectName: okf-mcp-backup
        serverName: okf-mcp-db
---
apiVersion: v1
kind: ConfigMap
metadata:
  name: okf-mcp-config
data:
  config.toml: |
    listen = "[::]:8080"
    allowed_hosts = ["okf-mcp.example.com"]
    default_catalog = "notes"
    time_server = "time.example.com"
    public_url = "https://okf-mcp.example.com"

    [telemetry]
    environment = "production"

    [telemetry.otlp]
    endpoint = "http://otel-collector.observability.svc.cluster.local:4318"
    headers_env = "OKF_OTLP_HEADERS"

    [telemetry.prometheus]
    listen = "[::]:9464"
---
apiVersion: apps/v1
kind: Deployment
metadata:
  name: okf-mcp
spec:
  # One replica because, without [redis], MCP sessions, consent pages, sign-in
  # flows live in this process's memory (section 1). Recreate
  # keeps two servers from running at once during an upgrade.
  replicas: 1
  strategy:
    type: Recreate
  selector:
    matchLabels:
      app: okf-mcp
  template:
    metadata:
      labels:
        app: okf-mcp
    spec:
      securityContext:
        runAsNonRoot: true
        runAsUser: 10001
        runAsGroup: 10001
        seccompProfile:
          type: RuntimeDefault
      containers:
        - name: okf-mcp
          image: ghcr.io/hypotheosis/okf-mcp:0.34.0
          args: ["serve"]
          ports:
            - name: http
              containerPort: 8080
            - name: metrics
              containerPort: 9464
          env:
            - name: OKF_MCP_CONFIG
              value: /etc/okf-mcp/config.toml
            - name: POD_NAME
              valueFrom:
                fieldRef:
                  fieldPath: metadata.name
            - name: POD_NAMESPACE
              valueFrom:
                fieldRef:
                  fieldPath: metadata.namespace
            # Kubernetes expands $(POD_NAME) and $(POD_NAMESPACE) because
            # they are defined above this entry.
            - name: OTEL_RESOURCE_ATTRIBUTES
              value: k8s.pod.name=$(POD_NAME),k8s.namespace.name=$(POD_NAMESPACE)
            - name: OKF_OTLP_HEADERS
              valueFrom:
                secretKeyRef:
                  name: okf-mcp-otlp
                  key: headers
            - name: DATABASE_URL
              valueFrom:
                secretKeyRef:
                  name: okf-mcp-db-app
                  key: uri
            - name: PGSSLMODE
              value: verify-full
            - name: PGSSLROOTCERT
              value: /etc/okf-mcp/pg-ca/ca.crt
          startupProbe:
            tcpSocket:
              port: http
            periodSeconds: 5
            failureThreshold: 60
          readinessProbe:
            httpGet:
              path: /healthz
              port: http
            periodSeconds: 10
            timeoutSeconds: 5
          livenessProbe:
            tcpSocket:
              port: http
            periodSeconds: 20
          resources:
            requests:
              cpu: 100m
              memory: 256Mi
            limits:
              memory: 1Gi
          securityContext:
            allowPrivilegeEscalation: false
            readOnlyRootFilesystem: true
            capabilities:
              drop: ["ALL"]
          volumeMounts:
            - name: config
              mountPath: /etc/okf-mcp/config.toml
              subPath: config.toml
              readOnly: true
            - name: pg-ca
              mountPath: /etc/okf-mcp/pg-ca
              readOnly: true
            - name: tmp
              mountPath: /tmp
      volumes:
        - name: config
          configMap:
            name: okf-mcp-config
        - name: pg-ca
          secret:
            secretName: okf-mcp-db-ca
            items:
              - key: ca.crt
                path: ca.crt
        - name: tmp
          emptyDir: {}
---
apiVersion: v1
kind: Service
metadata:
  name: okf-mcp
spec:
  selector:
    app: okf-mcp
  ports:
    - name: http
      port: 8080
      targetPort: http
---
apiVersion: networking.k8s.io/v1
kind: Ingress
metadata:
  name: okf-mcp
  annotations:
    cert-manager.io/cluster-issuer: letsencrypt
    nginx.ingress.kubernetes.io/proxy-buffering: "off"
    nginx.ingress.kubernetes.io/proxy-read-timeout: "3600"
    nginx.ingress.kubernetes.io/proxy-send-timeout: "3600"
    nginx.ingress.kubernetes.io/proxy-body-size: "80m"
spec:
  ingressClassName: nginx
  tls:
    - hosts: ["okf-mcp.example.com"]
      secretName: okf-mcp-tls
  rules:
    - host: okf-mcp.example.com
      http:
        paths:
          - path: /
            pathType: Prefix
            backend:
              service:
                name: okf-mcp
                port:
                  name: http
```

The objects for backups (`ObjectStore`, `ScheduledBackup`) are in section 9.

**The Deployment, and why:**

- **One replica and `Recreate`:** without `[redis]`, the short-lived state is
  in the process's memory, so a second replica would not find a session, a
  consent page or a flow that another one holds (section 1). The old pod stops
  before the new one starts, which also means an upgrade has a short outage.
  Publishing is not the reason: it is safe across processes.
- **Several replicas:** with `[redis]`, use the variant below. The Service
  needs no session affinity, and the Ingress needs no sticky annotation.
- **Startup probe:** TCP on 8080, for up to 5 minutes (60 x 5 s). The server
  opens its port only after it has run the migrations and loaded its keys and
  settings; catalogs load on first use, so start-up does not grow with their
  number.
- **Readiness probe:** `GET /healthz`, which answers 503 while the database
  does not answer within 2 seconds, so the pod leaves the Service during a
  database outage. The clock check never changes it.
- **Liveness probe:** TCP only, so a database failover never restarts the
  server.
- **`securityContext`:** non-root, a read-only root filesystem and every
  capability dropped. The server writes nothing to disk; the `emptyDir` on
  `/tmp` is where an archive copied into the pod for `okf-mcp import`, or
  written by `okf-mcp export`, goes (section 6).
- **No `RUST_LOG`:** the binary's default is the intended level (section 3).
- **Telemetry:** the `metrics` port (9464) is the unauthenticated `/metrics`
  listener. The Service above does not expose it and the Ingress must not
  route to it; scrape it through
  [`examples/kubernetes/metrics-service.yaml`](../examples/kubernetes/metrics-service.yaml)
  and [`examples/kubernetes/servicemonitor.yaml`](../examples/kubernetes/servicemonitor.yaml).
  `OKF_OTLP_HEADERS` comes from a Secret (`okf-mcp-otlp`, key `headers`, value
  `none` for a receiver without auth), and the downward API fills
  `OTEL_RESOURCE_ATTRIBUTES` so every signal names its pod and namespace. Leave
  all of it out, with the `[telemetry]` table, to run as before.
- The `kubectl cp` in section 6 needs `tar` in the container, which the image
  has.

**The variant with `[redis]`** (several replicas, a rolling upgrade). Add to the
ConfigMap's `config.toml`:

```toml
[redis]
url_env = "OKF_REDIS_URL"
mode = "cluster"        # ElastiCache Serverless; "standalone" for one primary
```

`[redis]` needs the release that includes multi-replica serve (milestone 16) or
a later one: an older image, such as the 0.20.0 above, does not support it.
Set that image tag in the variant below, and change the Deployment (the Secret `okf-mcp-redis` holds the URL under the
key `url`, for example `rediss://redis.example.internal:6379`; create it however
your site manages secrets):

```yaml
spec:
  replicas: 2
  strategy:
    type: RollingUpdate
    rollingUpdate:
      maxUnavailable: 0
      maxSurge: 1
  template:
    spec:
      containers:
        - name: okf-mcp
          # The first release with multi-replica serve (milestone 16), not the
          # 0.20.0 of the example above.
          image: ghcr.io/hypotheosis/okf-mcp:<release with multi-replica serve>
          env:
            # ... the entries above, and:
            - name: OKF_REDIS_URL
              valueFrom:
                secretKeyRef:
                  name: okf-mcp-redis
                  key: url
```

The mail worker runs on every replica: see "Several replicas: what stays per
replica", and set `terminationGracePeriodSeconds` above `worker_shutdown_grace`. With
`[feedback]` so does the feedback worker unless `[feedback] worker = false`.

**The Service:** it sets no `ipFamilyPolicy`. On a dual-stack cluster the
server listens on both families (`listen = "[::]:8080"`), so any policy works,
and a cluster may enforce its own (one that makes every Service
`RequireDualStack` leaves a manifest asking for `PreferDualStack` permanently
out of sync in a GitOps tool). Leaving the field out lets the cluster decide.

**The Ingress annotations, and why:**

- `proxy-buffering: "off"`: `/mcp` answers with server-sent event streams.
  With buffering on, nginx holds events back until its buffer fills, and
  clients see nothing.
- `proxy-read-timeout` and `proxy-send-timeout` `"3600"`: a client's `GET /mcp`
  stream stays open for as long as the session lives. nginx's default of 60 s
  would cut it.
- `proxy-body-size: "80m"`: nginx's default of 1 MiB would refuse almost every
  import. The import route accepts up to `max_import_bytes` plus gzip slack
  (a little over 64 MiB with the default), so the ingress must allow more
  than that. Raise both together.

## Telemetry

The server reports what it is doing three ways: spans and log records pushed
over OTLP to a receiver, metrics pushed over OTLP and served on a separate
`/metrics` port for Prometheus, and the stderr log it has always written.
All of it is off unless configured: with no `[telemetry]` table the server
writes today's stderr lines and nothing else. It applies to `okf-mcp serve`
only. The other subcommands keep plain stderr logging, and `okf-maintenance`
exports nothing (OTLP for the runner is a follow-up).

The design is `docs/superpowers/specs/2026-11-15-okf-observability-design.md`;
the keys are in the table in section 3.

### Sending to a receiver

OTLP goes over HTTP with protobuf bodies (not gRPC), to `endpoint` with
`/v1/traces`, `/v1/logs` and `/v1/metrics` appended, so point it at a
receiver's HTTP port (4318 for an OpenTelemetry Collector). Each export times
out after 10 seconds. `headers_env` names the variable that holds the request
headers as `name=value,name=value`, so a token never appears in the file;
set the variable to `none` for a receiver that takes no headers. A named
variable that is unset or blank stops the server starting, and the message
names the variable, never its value.

```toml
[telemetry]
service_name = "okf-mcp"
environment = "production"

[telemetry.otlp]
endpoint = "https://otlp.example.com"
headers_env = "OKF_OTLP_HEADERS"
sample_ratio = 0.25
```

```sh
OKF_OTLP_HEADERS='authorization=Bearer <token>'   # from a Secret, never the file
```

- **Sampling:** `sample_ratio` keeps that share of new traces. Without it,
  `OTEL_TRACES_SAMPLER` decides, and without that every trace is kept. An
  incoming `traceparent` is attached to the request span as a link, never
  adopted, so a caller cannot choose to have the server record (or skip) a
  trace. The server does send a `traceparent` on the requests it makes to an
  OIDC provider, to Postmark and to a client's metadata document, so those
  services can join the trace.
- **What is exported:** spans and log records from this crate's own code
  (`okf_mcp` targets) at `info` or above, by default. `traces_filter` and
  `logs_filter` take a tracing filter such as `info,okf_mcp::store=debug`.
  Raising them adds the `debug` and `trace` spans of the store and the
  handlers. Events from dependencies (sqlx, rmcp, hyper, the AWS SDK) are
  never exported, whatever the filter says, and neither are events with the
  `okf_mcp::local_only` target (below).
- **Metrics over OTLP** use the same endpoint, every `export_interval`
  (default 60 seconds), and are independent of the `/metrics` listener.
- **A failing receiver does not stop the server serving.** Failed exports are counted
  in `okf_telemetry_export_failures_total`, by signal and reason, and the
  server carries on.
- **Shutdown:** on SIGTERM the server drains requests, then flushes the last
  batch of each signal, waiting at most 4 seconds. That fits inside
  Kubernetes' default 30-second grace period.
- **The standard `OTEL_*` variables still apply:** `OTEL_RESOURCE_ATTRIBUTES`
  adds resource attributes (the Kubernetes example fills `k8s.pod.name` and
  `k8s.namespace.name`), `OTEL_EXPORTER_OTLP_HEADERS` is merged over the
  headers from `headers_env` (a header in both takes the environment's
  value), `OTEL_TRACES_SAMPLER` applies when `sample_ratio` is unset, and the
  batch settings `OTEL_BSP_*` are read by the SDK (the metric push interval
  is `export_interval` alone: the file's value, default 60 seconds, always
  wins over `OTEL_METRIC_EXPORT_INTERVAL`). `OTEL_SDK_DISABLED=true` turns OTLP
  off without editing the file;
  the `/metrics` listener and stderr are not affected, and the variables named
  by `headers_env` and `sentry_dsn_env` are then not needed.

### Sentry

Sentry takes OTLP at its own endpoint. Put the project's DSN in a variable and
name it instead of `endpoint` and `headers_env`:

```toml
[telemetry.otlp]
sentry_dsn_env = "OKF_SENTRY_DSN"
```

The server derives the signal URLs from the DSN
(`https://<host>/api/<project>/integration/otlp/v1/<signal>`, keeping any path
prefix) and sends the public key as the `x-sentry-auth` header. `sentry_dsn_env`
cannot be combined with `endpoint` or `headers_env`. The DSN is read at start-up,
never logged, and an invalid one is refused with a message that does not repeat it.
The Sentry SDK itself is not used.

Sentry's OTLP intake takes traces and logs only; it has no metrics endpoint
([docs.sentry.io/concepts/otlp](https://docs.sentry.io/concepts/otlp/)), so a
metrics push would be answered with a 404 every `export_interval`. With
`sentry_dsn_env`, `signals` therefore defaults to `["traces", "logs"]`, and
`"metrics"` is refused at start-up. Metrics are served by
the Prometheus `/metrics` listener (`[telemetry.prometheus]`) instead. The same
key turns off any signal a receiver does not take. At start-up the server logs
`OTLP export is on` with the `signals` it exports.

### Through a collector

A collector in the cluster keeps the vendor's credentials out of the okf-mcp
pod and lets you fan out. Point `endpoint` at the collector's HTTP receiver
and have it export onward:

```yaml
receivers:
  otlp:
    protocols:
      http:
        endpoint: 0.0.0.0:4318
exporters:
  otlphttp:
    endpoint: https://otlp.example.com
service:
  pipelines:
    traces:  { receivers: [otlp], exporters: [otlphttp] }
    logs:    { receivers: [otlp], exporters: [otlphttp] }
    metrics: { receivers: [otlp], exporters: [otlphttp] }
```

```toml
[telemetry.otlp]
endpoint = "http://otel-collector.observability.svc.cluster.local:4318"
headers_env = "OKF_OTLP_HEADERS"     # the variable may hold: none
```

For a quick look at what the server sends, give the collector a `debug`
exporter with `verbosity: detailed`: it prints every span, log record and
metric. The milestone changelog records such a run.

### The metrics listener

```toml
[telemetry.prometheus]
listen = "[::]:9464"
```

`GET /metrics` on this port returns the Prometheus text exposition. It is a
separate listener on purpose: it has **no authentication**, so it must be
reachable only through the cluster network. The Ingress must not route to it
(the example in section 5 does not), and the main port never serves
`/metrics`. Every other path on it is a 404. `listen` must not equal the main
`listen` address, nor share its port when either is unspecified (`0.0.0.0`
or `::`); the server refuses to start otherwise. A scrape reads state the
server already holds (the snapshot cache, the pool's counters, the
housekeeping job's last mail count), so it never queries Postgres.

Add a port named `metrics` to the Deployment and scrape it with a
ServiceMonitor. [`examples/kubernetes/metrics-service.yaml`](../examples/kubernetes/metrics-service.yaml)
is a ClusterIP Service for the port and
[`examples/kubernetes/servicemonitor.yaml`](../examples/kubernetes/servicemonitor.yaml)
selects it (path `/metrics`, every 30 seconds; add the label your Prometheus
selects ServiceMonitors by, often `release: <name>`). A NetworkPolicy that
admits only the Prometheus pods to port 9464 is a sensible addition.
[`examples/grafana/okf-mcp.json`](../examples/grafana/okf-mcp.json) is an
example Grafana dashboard (import it, and pick the Prometheus data source;
it needs Grafana 10.4 or later). It has a panel for every family the server
exports, grouped in rows (overview, HTTP, MCP tools, publish, catalogs and
change propagation, auth, mail, archives, platform), and shows each of the six duration
histograms as a heatmap of the server's own buckets, next to p50/p95/p99
lines. The `catalog` and `tool` variables narrow the panels that carry those
labels; the tool-duration heatmap follows `tool`.
Every query is scoped with `{job=~"$job"}`, because names such as
`http_server_request_duration_seconds` are standard and other services (or
another okf-mcp installation) in the same Prometheus would otherwise be merged
in. The `job` variable lists the values of the `job` label on `okf_build_info`,
which the ServiceMonitor sets (by default to the Service's name,
`okf-mcp-metrics`); choose your installation there. If you relabel `job`, the
dashboard follows it.
`tests/grafana_dashboard.rs` checks that it names only metrics the server
exports, that it shows every one of them, and that each histogram's buckets
are drawn as a heatmap. Each replica serves its own `/metrics`; the metrics Service's endpoints are every pod, so the ServiceMonitor gets one target per replica.

### stderr

stderr is unchanged by default: text lines, filtered by
`warn,okf_mcp=info,tower_http=info`. Two things can change:

- `[telemetry.stderr] format = "json"` writes one JSON object per line, with
  `trace_id` and `span_id` on every line written inside a span. The ids are the
  ones the receiver shows, so a line in `kubectl logs` leads to its trace.
- `[telemetry.stderr] filter` sets the filter in the file. `RUST_LOG`
  overrides it, and a blank or invalid `RUST_LOG` is ignored in favour of the
  file's value. `RUST_LOG` now means stderr only: it has no effect on what is
  exported, which `traces_filter` and `logs_filter` control. stderr always caps
  `rmcp` at `info`, because at `debug` that crate logs whole JSON-RPC bodies.

With a receiver reading the same events, stderr can be quieter, for example
`filter = "warn"`, and you read the receiver instead. Keep `warn` and `error`
on stderr either way: they are what `kubectl logs` shows when the receiver is
down.

### What is never exported

Nothing below reaches an OTLP span, event, log record or metric, or
`/metrics`:

- URIs, query strings and headers. A request span carries the method and the
  matched route template (`/api/v1/users/{name}/sessions/{id_prefix}`), never
  the path or the query.
- Request and response bodies, document and concept text, and concept paths.
- SQL and bind values.
- Error messages. An error is exported as its kind, a closed set of words.
- Passwords, tokens, codes, cookies, setup and reset links, and email
  addresses.
- Names: users, teams, keys and display names. For the principal, spans carry
  ids only: `enduser.id` (the user's id) and `okf.key.id` (the key's id). For a
  console session, `okf.key.id` is the word `session`, not the session's id and
  not the user's name; the user is identified by `enduser.id` alone.
- Dependency events (sqlx statements, rmcp's JSON-RPC, the AWS SDK).

**What is exported is not anonymous.** The ids above are stable and can be
resolved to a user or key by whoever can read your database or the audit log,
so choose a receiver you trust with them. Catalog names are exported too, as
the `okf.catalog` span attribute, but not on any metric (see below).

**Catalog names are not metric labels (issue #1157).** No metric carries a
`catalog` label, on `/metrics` or in OTLP. Decision: the dimension is removed
rather than replaced by the catalog's database row id. A catalog name is
operator- or user-supplied text that can identify a person or project, and one
series per catalog is a cardinality the server cannot bound (a row id would
hide the text but not the series count). Per-catalog figures are available
from the `okf.catalog` span attribute, the audit log and the database, which
are not time-series stores. The affected instruments are
`okf_mcp_tool_duration_seconds`, `okf_publish_duration_seconds`,
`okf_publish_lock_wait_seconds`, `okf_lint_rejections_total` and
`okf_archive_size_bytes_total`; operations on every catalog now land in the same
series, with the remaining labels unchanged.

**Upgrading: the `catalog` metric label is gone.** Dashboards, recording rules
and alerts that filter (`catalog="pg"`), match (`catalog=~"$catalog"`) or group
(`sum by (catalog)`) on it must drop that matcher or grouping. PromQL reads a
missing label as empty, so a matcher that needs a value (`catalog="pg"`,
`catalog=~"pg|other"`) matches no new series, while `catalog=~".*"` still
matches all of them; a `sum by (catalog)` now yields one series with an empty
label, and a `label_values(..., catalog)` variable lists nothing for new
samples. The example Grafana dashboard no longer
has a `catalog` variable or per-catalog panels. **Samples already stored in
Prometheus (or any OTLP backend) are not scrubbed:** series recorded before the
upgrade keep their `catalog` label and its values until your retention drops
them, or until you delete them with the TSDB admin API (`delete_series`) or
your backend's equivalent. Old and new samples of one metric are different
series, so a query that spans the upgrade sees both until the old ones age out.

**Upgrading: `(none)` and `(multiple)` (span only).** A tool call that
resolved no catalog used to be labelled `none`, and one that reached several
`multiple`, which a catalog of that name could be mistaken for. They are now
`(none)` and `(multiple)`, which no catalog name can match, in the
`okf.catalog` span attribute. Searches and alerts on spans that match
`okf.catalog:none` must change to `(none)` (and `multiple` to `(multiple)`).
Before issue #1157 the tool metric carried the same values as a `catalog`
label; that label no longer exists.

`mcp.session.id` and `okf.changeset` are digests, never the raw values: a
session id or a changeset handle lets whoever holds it act on the session or
changeset (with the owning key), so neither leaves the server. Each is the
first 16 hex digits of `SHA-256(kind, 0x00, value)`, unkeyed so every replica
gives the same token: filter on `mcp.session.id` to see one session's calls,
or on `okf.changeset` to see the calls on one changeset. To find the token
for a session or handle you hold, hash it the same way. Both ids are random
UUIDs, so the token cannot be turned back into the value.

Metric labels come from closed sets only: route templates, registered tool
names (anything else is `other`), and fixed outcome, method,
rule, reason and status words. No metric has a series per catalog, so a catalog created and deleted under many
names adds none.

**stderr is the one exception, and it is sensitive.** Events with the target
`okf_mcp::local_only` reach stderr only (both OTLP layers turn them off), and
they may carry caller-supplied text, an error's full text and the client
address. With the `log` mail provider they carry whole messages, setup and
reset links included. That is the same as stderr's content before telemetry
existed. So:

- keep stderr inside the cluster, as you keep the server's own log;
- if a log pipeline ships stderr to a third party, filter out
  `okf_mcp::local_only` first (with the JSON format, the `target` field);
- never use the `log` mail provider outside development.

For each such event, stderr also gets an ordinary twin that is exported, with a
closed value in place of the text (for example `sign-in attempt` goes to the
receiver, and `sign-in attempt (detail)` with the address stays on stderr).

### Metric reference

Names are as `/metrics` shows them. Dots become underscores in names and in
label names (`http.route` is `http_route`), durations are in seconds, and a
counter ends in `_total`. A histogram is three series (`_bucket`, `_sum`,
`_count`) with buckets from 1 ms to 60 s. Every family except
`okf_build_info`, `okf_telemetry_export_failures_total`, `okf_mail_queue_depth`,
`okf_changesets_open` and the gauges that read live state
(`db_client_connection_count`, and `okf_cache_entries` and `okf_cache_bytes`
while their cache exists) has no series until its first event, so a scrape of an idle server lists
fewer families than this table. Over OTLP the names are
the dotted originals.

| Family | Type | Labels | Meaning |
|---|---|---|---|
| `http_server_request_duration_seconds` | histogram | `http_request_method`, `http_route`, `http_response_status_code` | Every HTTP request on the main port |
| `http_server_active_requests` | gauge | `http_request_method`, `http_route` | Requests being served |
| `okf_mcp_tool_duration_seconds` | histogram | `tool`, `outcome` | MCP tool calls. `tool` is a registered tool or `other`; `outcome` is `ok` or `error` |
| `okf_mcp_sessions_active` | gauge | none | Open MCP sessions |
| `okf_publish_duration_seconds` | histogram | `outcome` | A publish, from the first lock to the refreshed snapshot |
| `okf_publish_lock_wait_seconds` | histogram | none | Time a publish waited for the catalog's locks |
| `okf_lint_rejections_total` | counter | `rule` | Lint errors that refused a publish. `rule` is the lint's rule id, `secret` for any secret rule, or `other` |
| `okf_changesets_open` | gauge | none | Open changesets in the cluster, as of the last `cluster_stats` housekeeping job (the same on every replica) |
| `okf_catalog_announcements_total` | counter | `result` | Catalog announcements handled, counting also each catalog found behind in the catch-up after a (re)subscribe: `applied` (rebuilt), `current` (already there), `unknown` (not loaded here, or deleted) or `failed`. Only with `[redis]` |
| `okf_invalidations_queued_total` | counter | `kind` | Change-feed messages queued by this replica's write transactions; Postgres delivers one only if its transaction commits, so a rollback leaves this ahead of `received` (`catalog`, `catalog_meta`, `credential`, `user`, `team`, `team_catalogs`, `settings`) |
| `okf_invalidations_received_total` | counter | `kind` | Change-feed messages received and applied |
| `okf_invalidations_unreadable_total` | counter | | Change-feed payloads this build could not read (each reloads the caches) |
| `okf_invalidations_lag_seconds` | histogram | | Time from a message's send to its application on this replica |
| `okf_cache_requests_total` | counter | `cache`, `result` | Lookups in the bounded caches. `cache` is `catalog`, `overlay`, `directory`, `credential`, `grant`, `user` or `labels`; `result` is `hit`, `miss` or `stale` (served but past its TTL or marked stale, so read again) |
| `okf_cache_evictions_total` | counter | `cache`, `cause` | Entries removed from a bounded cache. `cause` is `size` (the cache was full), `ttl` or `invalidated` (a change message dropped it) |
| `okf_cache_load_duration_seconds` | histogram | `cache`, `outcome` | Loads that fill a bounded cache (a catalog build, a database read); `outcome` is `ok` or `error` |
| `okf_cache_entries` | gauge | `cache` | Entries a bounded cache holds now |
| `okf_cache_bytes` | gauge | `cache` | Estimated bytes held, for the caches limited by bytes (`catalog` and `overlay`) |
| `okf_keycache_reloads_total` | counter | `outcome` | Settings and label reloads (named for the key cache they used to include; keys and users load on demand since milestone 18) |
| `okf_redis_errors_total` | counter | `operation` | Failed Redis calls. `operation` is `connect`, `get`, `set`, `take`, `update`, `remove`, `index`, `admit`, `bind`, `publish`, `subscribe`, `counter`, `limit` (a shared rate-limit bucket; the request was decided by the replica's own bucket instead) or `decode` (a stored value that would not decode). A failed or late `/healthz` `PING` counts as `get`. Only with `[redis]` |
| `okf_auth_attempts_total` | counter | `method`, `outcome` | Sign-in attempts. `method` is `password`, `passkey`, `oidc` or `setup`; `outcome` is `ok`, `failed` or `locked` |
| `okf_oauth_tokens_issued_total` | counter | `grant` | Token pairs issued: `authorization_code` or `refresh_token` |
| `okf_oauth_refreshes_total` | counter | `result` | Refresh-token presentations, by what the token turned out to be: `rotated` (first use), `grace` (a used token rotated again within `oauth.refresh_reuse_grace`), `reused` (presented past the window; the connection's tokens are revoked) or `unknown` (unknown, expired or revoked). The same values as the `okf.refresh` span attribute, from one source. All four series exist from the first scrape. [Alert on `reused`](#concurrent-refreshes) |
| `okf_ratelimit_rejections_total` | counter | `limiter` | Requests a limiter refused: `sign_in_per_ip`, `forgot_per_email`, `forgot_per_ip`, `link_starts`, `oauth_register`, `oauth_token` or `oauth_consent` |
| `okf_ratelimit_fallbacks_total` | counter | `limiter` | Decisions this replica's own bucket made because the shared (Redis) bucket failed, by the same `limiter` values. Any rate above 0 means the effective limit is the configured one times the replica count; alert on it. Only with `[redis]` |
| `okf_mail_jobs_total` | counter | `provider`, `outcome` | Mail tries. `provider` is the configured one; `outcome` is `sent`, `retry` or `failed` |
| `okf_mail_queue_depth` | gauge | `status` | Mail not yet sent, as of the last `cluster_stats` housekeeping job (the same on every replica): `pending`, `sending`, `retrying` or `failed` |
| `okf_housekeeping_runs_total` | counter | `task`, `outcome` | Housekeeping jobs run on this replica. `task` is `prune_changesets`, `prune_oauth`, `prune_audit`, `prune_workers`, `cluster_stats`, `provision_catalogs` or `cleanup_stores`; `outcome` is `ok` or `error` (an error is retried) |
| `okf_housekeeping_duration_seconds` | histogram | `task` | Housekeeping job duration |
| `db_client_connection_count` | gauge | `state` | Postgres pool connections: `idle` and `used` sum to the pool's size |
| `okf_clock_checks_total` | counter | `outcome` | Clock checks against `time_server` |
| `okf_clock_offset_seconds` | gauge | none | The time server's clock minus the server's, at the last good check |
| `okf_archive_size_bytes_total` | counter | `direction` | Archive bytes moved; `direction` is `export` or `import` |
| `okf_archive_duration_seconds` | histogram | `direction`, `outcome` | Archive exports and imports |
| `okf_telemetry_export_failures_total` | counter | `signal`, `reason` | Failed OTLP exports. `signal` is `traces`, `logs` or `metrics`; `reason` is `timeout`, `transport`, `rejected` or `other`. All twelve series exist from the first scrape |
| `okf_build_info` | gauge | `version` | Always 1; the label is the running version |

### Span reference

Spans are exported with `info` as the default filter. A request span is a
root, and a clock check, a housekeeping job and a mail job are roots too, since no
request started them. A tool call's `mcp <tool>` span is a child of its
`POST /mcp` request span, in the same trace (rmcp runs the call on its own
task, so the HTTP layer hands the request span over in the request's
extensions). One trace shows the request, the tool call and, with a raised
`traces_filter`, the tool's own spans. The request span stays a root: the
caller's `traceparent` is a link only. The request span's exported duration can
cover the tool call. `initialize`, `tools/list` and notifications never reach a
tool, so at the default `info` filter their traces hold the request span alone.

| Span | Attributes |
|---|---|
| `<METHOD> <route>`, such as `POST /signin` (server). No span for an unmatched route | `http.request.method` (`_OTHER` for an unknown method), `http.route`, `http.response.status_code`, `enduser.id`, `okf.key.id`, `okf.auth.failure` (on a 401 from a bearer credential check only: why it was refused, a closed set, see [Why a credential was refused](#why-a-credential-was-refused)). The status is `Error` for a 5xx only. The caller's `traceparent`, if any, is a link |
| `mcp <tool>` (server): one per tool call; `other` for an unregistered name | `okf.tool`, `okf.catalog` (the catalog the tool resolved: an existing catalog's name, from the `catalog` argument, the changeset or the connection's selection; `(multiple)` when a call reached several, `(none)` when it resolved none; the parentheses keep both from ever being a catalog's name), `mcp.session.id` (a digest of the `Mcp-Session-Id`, only when the request has one), `okf.changeset` (a digest of the `changeset` argument, only when the call names one), `okf.outcome` (`ok` or `error`), `okf.error.kind` on failure and `error.type` (the same value, the OpenTelemetry name that Sentry and other backends show as the error type; never the message), `enduser.id`, `okf.key.id`. The kind is the tool error's name for an `isError` result; for a call that fails before the tool runs it is `InvalidParams` (arguments that don't parse, or an unknown tool name: rmcp 3.8 answers it with `INVALID_PARAMS`), `UnknownTool` (`METHOD_NOT_FOUND`), `Internal` (`INTERNAL_ERROR`) or `Protocol` (any other code) |
| `okf.publish`, with children `okf.publish.lock`, `okf.publish.lint` and `okf.publish.write` | `okf.catalog`, `okf.changeset.entries` |
| `okf.catalog.announcement`: a root per catalog announcement handled | `okf.catalog` (a catalog cached on this replica only), `okf.announcement.result` (as `okf_catalog_announcements_total`'s `result`) |
| `okf.clock.check` | `okf.outcome`, `okf.clock.offset` (seconds) |
| `okf.archive.export` and `okf.archive.import` | `okf.catalog` (only for a catalog that exists), `okf.archive.size` (bytes, on success), `okf.outcome` |
| `okf.mail.job` | `okf.mail.template`, `okf.mail.provider`, `okf.outcome`. Never the address |
| `okf.oauth.token` | `okf.grant` (`code` or `refresh`), `outcome` (`ok` or `failed`), `okf.error` (the RFC 6749 error code, on failure), `okf.refresh` for a refresh (`rotated`, `grace`, `reused` or `unknown`; [Concurrent refreshes](#concurrent-refreshes)). `okf.resource` when `invalid_target` came from a `resource` that was not this server's (see below). No key or value holds `auth` or `token`, which trace backends' default scrubbers hide. Never a code, token or client id |
| Redis calls: `connect` and `subscribe` (at `info`), then `publish`, `admits`, `bind`, `release`, `load_state`, `store_state`, `delete_state`, `run_counter`, the rate limits' `take` (operation `limit`) and the stored-state operations (at `debug`, off by default) | `db.system.name` (`redis`), `db.operation.name` (the same set as `okf_redis_errors_total`'s `operation`, except `decode`, which is a metric label only). Never a key name, a session or flow id, a value or the URL |
| Everything else (store, handlers) | Function spans at `debug` and `trace`, off by default; they carry safe ids only |

An event logged inside a span is exported as a log record carrying the span's
`trace_id` and `span_id`, and also appears as an event on the span.

## 6. First run

The subcommands run inside the pod, where `DATABASE_URL`, the TLS variables
and `OKF_MCP_CONFIG` are already set.

**Create a user** for each person who will hold a credential. Every key,
sign-in code and catalog needs one: a key or code belongs to a user, and a
catalog to a team. Creating user `alice` also creates her personal team
`alice` ([Users and teams](#users-and-teams) has the rest):

```sh
kubectl -n okf-mcp exec deploy/okf-mcp -- okf-mcp user create alice --display-name "Alice Example"
```

**Create a catalog**, owned by a team (a user's name names that user's
personal team):

```sh
kubectl -n okf-mcp exec deploy/okf-mcp -- \
    okf-mcp catalog create notes --description "Team notes" --owner alice
```

A new catalog is not empty: it starts with a root `index.md` (headed with
its name, its description as the first paragraph) and a `log.md` with one
`Creation` entry, so agents can list concepts and log changes at once.
Importing into it before anything is published replaces them without
`--replace`.

**Repair catalogs without indexes.** A catalog created before starter files
existed (or one imported without indexes) has no root `index.md`, so agents
cannot list concepts in it. Repair every such catalog from the command line,
checking first with `--dry-run`:

```sh
kubectl -n okf-mcp exec deploy/okf-mcp -- \
    okf-mcp catalog repair-indexes --missing-only --dry-run
kubectl -n okf-mcp exec deploy/okf-mcp -- \
    okf-mcp catalog repair-indexes --missing-only
```

or name catalogs (`okf-mcp catalog repair-indexes notes recipes`). Each
repair is published at once, under the catalog's lock as an import is: a
missing root `index.md` is the one a new catalog gets (its name and
description), then a `log.md`, folder indexes, and folder and concept
listings are added where missing, and `log.md` gets a line recording the
repair. Existing index text (headings, prose, listings, summaries) is
kept, so a second run writes nothing, with one exception: an old raw
(unencoded) concept or folder listing written before percent-encoding, such
as `* [A](a b.md) - x` or `* [My docs](my docs/) - x`, whose link never
resolves, has its destination
encoded in place (label and summary kept), or is removed when a working
listing sits beside it; the output quotes each one. A file or folder no
markdown link can list (a label that cannot be written, such as a folder named `a]`) is not listed; the output
names it. With names, `--missing-only` says
which of them it skipped (those with a root `index.md`, or misspelled). Listings of concepts
without a title or description get `TODO` placeholders; the output names
each one. A changeset someone left open on an affected index replays its
bookkeeping onto the repaired text or conflicts at publish, as after any
publish. The command's change message reaches every running replica at once
(at worst within `catalog_ttl`); with `[redis]` it also announces the change
(best effort).

An agent with write access can do the same for one catalog through a
changeset, to review before publishing: `open_changeset`, `repair_indexes`,
`pending_changes`, then `publish`. The maintenance runner does neither; it
only reports.

**Change a catalog's description** (what `list_catalogs` tells agents, for
example when a catalog becomes the source of truth). It prints the old and
new text; an unknown catalog is refused, and an empty description is allowed:

```sh
kubectl -n okf-mcp exec deploy/okf-mcp -- \
    okf-mcp catalog update notes --description "Team notes; the source of truth"
```

**Create keys.** Access is `read`, `write` or `admin`; grant named catalogs
with `--catalogs a,b` or every catalog, present and future, with `--all`.
`--user` names the user the key belongs to, which must exist; it changes
nothing about what the key reaches. Reporting is independently opt-in via
`key create --maintenance-report`: it requires read access to the catalog and
never grants write access; write/admin do not enable reporting by themselves:

```sh
kubectl -n okf-mcp exec deploy/okf-mcp -- \
    okf-mcp key create claude --access write --catalogs notes --default notes --actor claude-code-tui --user alice
kubectl -n okf-mcp exec deploy/okf-mcp -- \
    okf-mcp key create claude --access write --catalogs notes --default notes --actor claude-ai-web --user alice
kubectl -n okf-mcp exec deploy/okf-mcp -- \
    okf-mcp key create ops-admin --access admin --all --user alice
```

The positional value is a grouping name, so the two `claude` credentials are
listed together while retaining separate actors, grants, defaults and
revocation. `key create` prints the key once and its immutable administration
id on stderr; only the key's SHA-256 is stored. Put it straight
into a password manager or a secret store. Never paste it into logs, tickets,
chat or shell history (a leading space keeps a command out of history in most
shells). `key list` shows name, actor, id, access, state, grants, `default
X`, `maintenance_report true|false`, `provisioner yes|no`, `user U` (`-` for a key from before
users existed that nobody has assigned), `kind key|oauth`, `client NAME` (the OAuth client's name for a grant, `-` for a key) and `last_activity T` (when a grant's newest token was issued, `-` for a key or a grant with none on record; see "Idle grants" below), grouped by name. OAuth grants appear in the list too and may share a name and actor with a key, so check `kind`. Use
`okf-mcp key maintenance-report ID --enabled true|false [--actor ACTOR]` to
change the reporting flag. `revoke`, `grant`, `ungrant` and `maintenance-report`
accept an id, an unambiguous live name, or `NAME --actor ACTOR` of an API key;
they never match an OAuth grant, whose name may repeat a key's (see "Mint a
sign-in code" below); for example,
`okf-mcp key grant claude work --actor
claude-ai-web`. A revoked name/actor pair can be created again for rotation;
the old row and its changeset history remain attached to the old id. A revoked
key stops working on every replica within a fraction of a second (the change
message; a lost one is bounded by `credential_ttl`).

**Mint a sign-in code** for a person connecting an OAuth client. Codes are
redeemed on the OAuth consent page, which exists only once OAuth is on
(`public_url` is set). The code is printed once. It is single-use and expires
unused after 24 hours unless `--expires` says otherwise, up to
`oauth.code_max_expiry` (7 days by default). Access is `read` or `write`;
`admin` is refused. `--user` (required) names the user the code and its grant
belong to; `--actor` defaults to the code's name. The person enters the code
on the consent page, which turns it into an OAuth grant: a key row named
`<code name>.<client host>-<8 characters of the code id>`, with the code's
catalogs, access, default, actor and user. `okf-mcp key ...` never acts on a grant,
except `key assign`, which gives a key or a grant its user
([Users and teams](#users-and-teams)); `okf-mcp oauth grant revoke ID` revokes one (see
[Managing codes and grants](#managing-codes-and-grants) for every
subcommand, and [OAuth connectors](#oauth-connectors) for the whole flow):

```sh
kubectl -n okf-mcp exec deploy/okf-mcp -- \
    okf-mcp oauth code create laptop --actor example-actor --access write --catalogs notes --default notes --user alice
```

**Import a catalog**, from a `git archive` tarball (exactly the tracked files)
or any `.tar.gz` that follows the import rules. Either copy it into the pod's
`/tmp`:

```sh
git -C ~/notes archive --format=tar.gz -o notes.tar.gz HEAD
kubectl -n okf-mcp cp notes.tar.gz "$(kubectl -n okf-mcp get pod -l app=okf-mcp -o name | cut -d/ -f2)":/tmp/notes.tar.gz
kubectl -n okf-mcp exec deploy/okf-mcp -- okf-mcp import notes /tmp/notes.tar.gz
kubectl -n okf-mcp exec deploy/okf-mcp -- rm /tmp/notes.tar.gz
```

or stream it on stdin (`-`):

```sh
git -C ~/notes archive --format=tar.gz HEAD |
    kubectl -n okf-mcp exec -i deploy/okf-mcp -- okf-mcp import notes -
```

or post it to the HTTP route with an `admin` key (`?replace=true` to replace
a non-empty catalog, which a new catalog's starter files do not make it, `?only_machinery=true` for `--only-machinery` and
`?expect_generation=N` for `--expect-generation N`, both described below):

```sh
git -C ~/notes archive --format=tar.gz HEAD |
    curl -sf -H "Authorization: Bearer $OKF_MCP_KEY" --data-binary @- \
        https://okf-mcp.example.com/catalogs/notes/import
```

The server picks up a catalog created, updated or imported from the command
line as soon as the change message reaches it (each catalog loads on its
first read; a description change does not rebuild the catalog's snapshot);
an import through the HTTP route is visible at once.

`okf-mcp export` inside the pod must write under `/tmp` (or to `-`, stdout),
the only writable path:

```sh
kubectl -n okf-mcp exec deploy/okf-mcp -- okf-mcp export notes - > notes.tar.gz
```

It reports the generation the archive holds on stderr (`exported catalog
"notes" at generation 42`). The import report (JSON on stdout, or the HTTP
route's body) lists the `added`, `updated` and `deleted` paths and counts the
`unchanged` ones.

**Update machinery** (`AGENTS.md`, `tools/`, `install/` and the rest that the
write tools refuse) with `--replace --only-machinery`. Only the source's
machinery is compared and written; its other files are ignored and the
catalog's concepts stay as they are, so a concept published meanwhile is
kept. The source can be the catalog's template checkout or an export with
new machinery overlaid:

```sh
git -C ~/notes-template archive --format=tar.gz HEAD |
    kubectl -n okf-mcp exec -i deploy/okf-mcp -- \
        okf-mcp import notes - --replace --only-machinery
```

A full `--replace` from an edited export reverts whatever was published
after the export. Pass the generation the export reported as
`--expect-generation 42`, and the import is refused, writing nothing, if the
catalog has moved since; export again and redo the edit. Over HTTP, the
same guarded machinery-only import is:

```sh
curl -sf -H "Authorization: Bearer $OKF_MCP_KEY" --data-binary @notes.tar.gz \
    "https://okf-mcp.example.com/api/v1/catalogs/notes/import?replace=true&only_machinery=true&expect_generation=12"
```

**Connect a harness.** The MCP URL is `https://okf-mcp.example.com/mcp`, with
the key in `Authorization: Bearer <key>`. For Claude Code:

```sh
claude mcp add --transport http okf https://okf-mcp.example.com/mcp \
    --header "Authorization: Bearer $OKF_MCP_KEY"
```

`X-OKF-Catalogs` (or its alias `X-OKF-Catalog`, or `?catalogs=a,b`) picks
the connection's catalogs, and bounds them. Each value is a catalog's id
(its `name`, which `list_catalogs` returns beside its `label`), never its
label: the session sees no other
catalog, whatever the key's grants or default. A request with both header
spellings reads them as one list, the `X-OKF-Catalogs` names first. No
header names a changeset: an agent calls `open_changeset` and passes the
handle it returns as `changeset` to every write. The README's "Connecting a
client" and "Changesets" sections describe both.

**Protocol versions:** the server accepts MCP `2024-11-05`, `2025-03-26`,
`2025-06-18`, `2025-11-25` and `2026-07-28` (rmcp 3.8's known versions). A
client on a version before `2026-07-28` gets a stateful session
(`Mcp-Session-Id`); one on `2026-07-28` is served statelessly.

## Users and teams

Every API key, OAuth grant and sign-in code belongs to a **user**, and every
catalog to a **team**:

- Creating a user also creates its **personal team**, of the same name, with
  the user as its owner. A personal team may take more members.
- A **team** has members, each an `owner` or a `member`. The role governs
  only the team's own membership.
- A catalog is owned by exactly one team, and a credential belongs to exactly
  one user (never a team). `actor` stays as each credential's provenance
  label.
- **Ownership grants no access.** A credential reaches exactly the catalogs
  it is granted; owning a catalog, or being in the team that owns it, adds
  nothing.

**Running okf-mcp for other people.** An operator who runs the server for
its staff or its customers has four more tools, all off or neutral until
used: the product name and support contact people see in email and on the
server's pages ([Settings](#settings), `product_name` and
`support_contact`); a [provisioner key](#provisioner-keys) for the
operator's own signup and offboarding automation, which manages accounts,
teams and catalogs but reads no catalog; a stored, append-only
[audit log](#audit-log) of administrative changes, catalog writes, exports
and operator reads, which admins read in full and each team's owners read
for their team; and [unlock before access](#unlock-before-access), which,
once `require_unlock` is on, keeps operator staff out of a customer's
catalog until they unlock it with a reason and a time limit. Finding a
user by email (`user list --email`) and deleting a user with everything
only theirs ([Deletion rules](#deletion-rules)) complete the lifecycle.

User and team names are `[a-z0-9-]+` and are unique across both: `user
create alice` is refused while a team `alice` exists, and the other way
round. Every command below needs the database, as every subcommand does; the
REST routes and console controls for each are in the
[parity table](#cli-api-and-console-parity). A running server picks up each
change through the change feed within a fraction of a second (a lost message
is bounded by `catalog_ttl` or `credential_ttl`, 10 seconds by default); the
same change through the API or the console takes effect at once on the
replica that took the request.

| Command | Does |
|---|---|
| `okf-mcp user create NAME [--display-name TEXT] [--email ADDRESS]` | Creates the user and its personal team `NAME`, with the email when given: set in the same transaction, so an address another user has creates nothing |
| `okf-mcp user update NAME --display-name TEXT` | Sets (or, with `""`, clears) the user's display name; its personal team's label follows. Prints `updated user NAME display name: "old" -> "new"` |
| `okf-mcp user list [--email ADDRESS]` | One tab-separated line per user: name, `credentials N` (keys, grants and codes naming it, revoked ones included), `email E` (`-` when unset), `admin yes\|no`, `teams a,b`, then the display name. `--email` prints only the user with that email, in any letter case, and nothing when no user has it |
| `okf-mcp user delete NAME` | Deletes a user with its credentials, sessions and personal team, discarding its open changesets; catalogs and history stay. Prints what went (see below) |
| `okf-mcp user set-admin NAME --on\|--off` | Gives or takes the admin flag: an admin user's console session acts as an admin that reaches every catalog. A change signs the user out of every console session. `--off` for the last admin user is allowed here (operator access), refused over REST |
| `okf-mcp user set-email NAME EMAIL` (`--clear`) | Sets the email a user signs in and gets mail with (unique, case-insensitive), or clears it. A change ends the user's outstanding emailed links |
| `okf-mcp user invite NAME` | Queues a setup email with a single-use link (valid `setup_link_ttl`) to the user's email; an older setup link stops working. Needs `public_url`; prints the address, never the link. It only queues: a running server's mail worker sends it, so the command needs no mail secret |
| `okf-mcp user entitle NAME CATALOG --access read\|write` | Adds a catalog to the user's ceiling, or changes its level |
| `okf-mcp user unentitle NAME CATALOG` | Removes an entitlement |
| `okf-mcp user sessions NAME` | The user's console sessions, most recently used first, one per line: id prefix (never the id), `created T`, `seen T`, `ip A`, then the browser |
| `okf-mcp user revoke-sessions NAME` | Signs the user out of every console session; keys and grants keep working. Prints `revoked N sessions of NAME` |
| `okf-mcp user sign-ins NAME [--limit N]` | The user's recent sign-in attempts (20 by default), newest first: time, method, outcome, `ip A` |
| `okf-mcp team create NAME [--display-name TEXT] --owner USER` | Creates a shared team with `USER` as its first owner |
| `okf-mcp team update NAME --display-name TEXT` | Sets (or clears) a shared team's display name; a personal team is refused (change its user's) |
| `okf-mcp team list` | One line per team: name, `personal U` (`-` for a shared team), `members u(role),...`, `catalogs c,...` (`-` for none), `suspended TIME` (`-` when active), then the display name |
| `okf-mcp team add-member TEAM USER [--role owner\|member]` | Adds a member (`member` by default), or gives a member that role |
| `okf-mcp team set-role TEAM USER --role owner\|member` | Changes a member's role, adding the user if it is not a member |
| `okf-mcp team remove-member TEAM USER` | Removes a member |
| `okf-mcp team delete NAME` | Deletes a shared team that owns no catalog; its memberships go with it |
| `okf-mcp team suspend NAME [--notice TEXT]` | Suspends the team: its catalogs refuse every non-admin credential except export (see [Suspending a team](#suspending-a-team)). Prints `suspended team NAME`. Repeating it replaces the notice (none when `--notice` is left out) and keeps the first suspension time |
| `okf-mcp team unsuspend NAME` | Lifts the suspension. Prints `lifted the suspension of team NAME`; no change when the team is active |
| `okf-mcp catalog create NAME --owner TEAM [--display-name TEXT] [--description TEXT]` | `--owner` is required |
| `okf-mcp catalog update NAME [--display-name TEXT] [--description TEXT]` | At least one flag is required; prints one `updated catalog NAME <field>: "old" -> "new"` line per flag given |
| `okf-mcp catalog repair-indexes [NAME...] [--missing-only] [--dry-run] [--actor ACTOR]` | Adds the root `index.md`, `log.md`, folder indexes and listings the named catalogs are missing, rewrites old raw (unencoded) concept and folder listings, and publishes them at once. `--missing-only` keeps only catalogs without a root `index.md` (every such catalog when none is named); `--dry-run` writes nothing. Prints, per catalog, its summary, the files written and a note per created file and `TODO` placeholder, then `repaired N of M catalog(s)`. It visits every store and repairs each catalog on the store that holds it. A failing catalog or store does not stop the others; the command then exits non-zero naming it. Revisions record `ACTOR` (default `okf-mcp/repair-indexes`) and the message `repair indexes` |
| `okf-mcp catalog set-owner NAME --team TEAM` | Moves a catalog to another team, or gives a legacy catalog its first owner. Prints `catalog C: owner OLD -> NEW` (`(none)` for a legacy catalog) |
| `okf-mcp catalog set-member-access NAME --access none\|read\|write` | What members of the owner team reach through their own keys and grants (default `write`). Prints `catalog C: member access OLD -> NEW` |
| `okf-mcp catalog set-maintenance NAME on\|off` | Lets the maintenance runner check the catalog (`on`, the default) or keeps it out (`off`); see [The maintenance runner](#unlock-before-access). Prints `catalog C: maintenance OLD -> NEW` |
| `okf-mcp key create NAME --user USER [--provisioner] ...` | `--user` is required; `--actor` still defaults to `NAME`. `--provisioner` takes no `--access`, `--all`, `--catalogs`, `--default` or `--maintenance-report` |
| `okf-mcp key assign ID --user USER [--actor ACTOR]` | Gives a key or an OAuth grant a user, or changes it; access is unchanged. `ID` is an id, or an unambiguous live name (`--actor` narrows it). The only `key` subcommand that also acts on grants. Prints `key ID now belongs to USER` |
| `okf-mcp oauth code create NAME --user USER ...` | `--user` is required; `--actor` defaults to `NAME` |
| `okf-mcp oauth code assign ID --user USER` | Gives a sign-in code, in any state, a user. Prints `sign-in code ID now belongs to USER` |
| `okf-mcp unassigned` | Every key, grant, sign-in code and catalog with no user or owner, one per line (`key NAME (id ID, actor ACTOR)`, `grant NAME (id ID)`, `code NAME (id ID)`, `catalog NAME`), or `nothing is unassigned` |

`catalog list` prints name, `generation N`, `owner T` (`-` when unassigned),
then the description. `key list`, `oauth grant list` and `oauth code list`
end with `user U` (`-` when unassigned); `key list` and `oauth grant list` then add `kind` and `client`. Every listing is tab-separated with
no header row, so `cut -f` works on it.

### Provisioner keys

Create an automation key with `okf-mcp key create signup --provisioner --user alice`,
or as an every-catalog admin with `POST /api/v1/keys` and
`{"name":"signup","user":"alice","provisioner":true}`. The console has a
Provisioner switch in API keys > New key; it disables the catalog-access controls.
`key list` includes `provisioner yes|no`. The flag is fixed at creation, its
grant list is empty, and the schema refuses grants on it. It is never self-issued
or an OAuth grant. Scoped admins cannot manage a provisioner key.

Protect it like an admin key: it can take over any non-admin account by changing
its email and sending a setup link. It cannot act on admin users: lookup leaves
them out, detail refuses them, and mutations check the live flag under the user
row lock. User detail never shows key, grant or sign-in-code lists to a
provisioner; the user view retains their aggregate count. Every state change
it makes, every email lookup and every refused request is audited with its key
as actor; its other reads (`GET /users/{name}`, `GET /teams`, `GET /teams/{name}`,
`GET /audit`) and its requests that change nothing appear in request traces and
metrics, not in the audit log (see the `user.looked_up` and `provisioner.refused`
rows of the [audit log](#audit-log) table; #423). Team and catalog
management remains server-wide, as the allow-list specifies; the admin-target
refusal applies to named user operations and membership/initial-owner targets.

The explicit allow-list has these 20 method/template pairs (adding a new route
does not add permission):

| Method | Route |
|---|---|
| GET | `/api/v1/users?email=ADDRESS` (email required) |
| GET | `/api/v1/users/{name}` |
| POST | `/api/v1/users` |
| PUT | `/api/v1/users/{name}/email` |
| POST | `/api/v1/users/{name}/invite` |
| PATCH | `/api/v1/users/{name}` (display name) |
| DELETE | `/api/v1/users/{name}` |
| GET | `/api/v1/teams` (every team, as an admin sees it; for reconciling its records) |
| GET | `/api/v1/teams/{name}` |
| POST | `/api/v1/teams` |
| PUT | `/api/v1/teams/{name}/members/{user}` |
| DELETE | `/api/v1/teams/{name}/members/{user}` |
| PATCH | `/api/v1/teams/{name}` (display name) |
| DELETE | `/api/v1/teams/{name}` |
| PUT | `/api/v1/teams/{name}/suspension` |
| POST | `/api/v1/catalogs` |
| PATCH | `/api/v1/catalogs/{catalog}` (display name, description) |
| DELETE | `/api/v1/catalogs/{catalog}` |
| PUT | `/api/v1/catalogs/{catalog}/member-access` |
| GET | `/api/v1/audit` (only the entries it wrote; not the export) |

Every other API route is refused, including `GET /api/v1/labels`, listing all users, managing
keys or grants, changing settings or admin flags, entitlements and session
revocation. It reaches no catalog contents: no reads, writes, imports or exports.
It is not an MCP or archive credential, nor a console sign-in credential.

A shared catalog belongs to a team with several members:

```sh
okf-mcp user create alice --display-name "Alice Example"
okf-mcp user create bob
okf-mcp team create family --display-name "The family" --owner alice
okf-mcp team add-member family bob
okf-mcp catalog create recipes --owner family
okf-mcp key create bob-laptop --access write --catalogs recipes --default recipes --user bob
okf-mcp team list
```

### Migrating an existing deployment

Upgrading to a release with users and teams (migration `0009`) creates no
user and assigns nothing: every key, grant, sign-in code and catalog from
before it stays **unassigned** and keeps working exactly as before. Assign
them as follows (inside the pod, with `kubectl exec` as in
[First run](#6-first-run)):

1. List what is unassigned:

   ```sh
   okf-mcp unassigned
   ```

2. Create a user for each person who holds a credential, and a team for each
   group that shares a catalog:

   ```sh
   okf-mcp user create alice
   okf-mcp team create family --owner alice
   ```

3. Give each key and grant a user (by id, or by an unambiguous live name),
   each sign-in code a user, and each catalog an owner:

   ```sh
   okf-mcp key assign claude --actor claude-code-tui --user alice
   okf-mcp key assign GRANT_ID --user alice     # a grant, by its id
   okf-mcp oauth code assign CODE_ID --user alice
   okf-mcp catalog set-owner notes --team family
   ```

4. Run `okf-mcp unassigned` again, and repeat until it prints `nothing is
   unassigned`.

The console does the same at Users → "What is unassigned"
(`/ui/#/users/unassigned`), with a user or team picker and Assign on each
row; the Users link shows the count while anything is unassigned. Nothing
forces the migration to finish: assigning is bookkeeping, and an unassigned
credential authenticates as before (`whoami` shows `user: null`). Making the
columns `NOT NULL` once nothing is unassigned is future work (#296).

### Store cleanup

A catalog or key deletion commits on the account database first and owes the
rest to the store its data lives on: one row in `store_cleanups`, written in
the same transaction. The `cleanup_stores` job (every minute, once per
cluster) drains the rows store by store, up to 100 per store per run. For a
deleted catalog it removes the store's rows (the cascade from
`catalog_state`; the catalog's audit entries stay) and then the `catalogs`
row; until then the name stays reserved and no reader sees the catalog. Until
then, deleting the team or user that owns the catalog is refused (409) with
`catalog "<name>" is still being deleted; try again shortly`; retry after the
next run. For a
deleted key it discards the key's open changesets (a `catalog.changeset_discarded`
entry with `reason` `key_deleted` each) and clears the key from the store's
changeset and maintenance-run history.

A store that is down, or a cleanup that fails, is retried every run: the row
counts its `attempts`, and a store that fails never holds up the others or the
deletion itself, and a row that keeps failing is tried after the rows that
have failed less, so it cannot starve the rest of its store. Past 20 attempts the job logs `a store cleanup keeps failing` at
error level on every run and keeps the row; look at `store_cleanups` for the
store, kind and subject, and fix the store (its credential, its reachability).
If the store's commit fails after the account's, the catalog is gone from
every lookup and its data stays unmarked until the cleanup removes it all the
same.

Each attempt is bounded, at the database first. Every statement the job
issues, on the account database and on a store, runs in a transaction that
sets `lock_timeout` and `statement_timeout` to 20 seconds (`LIMITS.wait`, a
constant, not a setting), so PostgreSQL itself gives up on a row held under a
lock (a changeset another session has `FOR UPDATE`, a catalog's `catalog_state`
row, a default key's row) and rolls the transaction back. A `tokio` timeout of
30 seconds per row (`LIMITS.attempt`) wraps the attempt as a backstop for what
the database cannot bound, such as a store that stops answering; dropping a
future does not cancel its statement, which is why the database bound comes
first. An attempt that fails either way is counted as a failed try like any
other, and the run goes on to the next row and store. A catalog's cleanup
removes the store's data and commits that before it takes the account half, so
no attempt holds a store connection while it waits for an account one, or the
reverse. A user's keys are deleted with their `maintenance_runs` and
changeset references cleared by the cleanup; a maintenance claim and a
changeset open each hold the key's row `FOR KEY SHARE` until their store
commit, so the deletion waits for them and the cleanup it owes then finds
what they wrote.

One failure never ends the run. The only error that stops it is the first
read of which stores have rows (the account database cannot be reached).
Everything else is tied to one store or one row and stays there: a store
that cannot be opened, a store whose rows cannot be read, a row that fails,
and even counting a failed try (say the row itself is locked by another
session) are logged with the store and the row's `id` and the run goes on to
the next row and store. A try that could not be counted is logged at error
level as `a store cleanup failed and its try could not be counted`; the row
keeps its old `attempts` and is tried on the next run.

### Deletion rules

Deleting a user takes everything that is only theirs, in one transaction:
its keys and OAuth grants (revoked ones too), sign-in codes, console
sessions, passwords, passkeys, sign-in identities, emailed links, sign-in
events, entitlements, team memberships and personal team. The open
changesets of its keys and grants are discarded a little later, each with a
`catalog.changeset_discarded` audit entry, by the `cleanup_stores` job
([Store cleanup](#store-cleanup)): the deletion owes it in the same
transaction, so it is never lost. Catalogs, their history (whose
`actor` text stays as written) and every audit entry are kept; the deletion
is recorded as `user.deleted`, and from then on the log names the user
`deleted user #N` and its credentials `credential #N`.

What a deletion would orphan is refused instead: each refusal is a 409 over
REST, and a non-zero exit naming the cause on the command line.

- `user delete` is refused while its personal team owns a catalog (delete
  the catalog or move it with `catalog set-owner` first), while it is the
  only owner of a shared team (add another owner first), and, over REST and
  in the console, for the last admin user. The command line has operator
  access and may delete the last admin user, as it may clear the last admin
  flag. Over REST, `DELETE /api/v1/users/{name}?unless_shared_member=true`
  adds a precondition, checked in the same transaction as the delete: a
  user who is a member (owner or member, suspended team or not) of any team
  but its own personal one is refused with a 409 whose message starts `the
  user is a member of shared team(s): ` and lists the teams, and nothing is
  deleted. That refusal is checked first, so it wins over every other one,
  the last-owner refusal included. Without the parameter the rules below apply
  unchanged. A user being a plain member of a shared team, or the owner of one
  with other owners, does not stop the deletion: the membership goes with
  it.
- A person deleting their own account from the Account page
  (`DELETE /api/v1/me`, [Deleting your own account](#deleting-your-own-account))
  goes through the same deletion, with the same refusals as REST: the last
  admin user, catalogs its personal team owns and shared teams it is the
  only owner of are refused, with the reason shown on the page. The account
  is never disabled instead.
- `team delete` is refused for a personal team (it goes with its user) and
  for a team that owns a catalog.
- `team remove-member` and `team set-role` never remove or demote a team's
  last owner, and `remove-member` never removes a personal team's user.

An unknown user or team is a 404 (`{"error": "UnknownName", "message": ...,
"noun": "user", "name": ...}`) on every route that names one, including the `user` and
`owner` of a create body; on the command line it is a non-zero exit naming
it. A create body without its required `user` or `owner` is a 422; on the
command line a missing `--user` or `--owner` is clap's usage error (exit 2).

### Display names and labels

Users, teams and catalogs each have an id (`name`, a slug that never
changes) and an optional display name. A signup provisioner may generate
the ids at random, so the server shows people a `label` instead, never the
id. A user's is its display name, else its email's local part, else
"Unnamed user". A shared team's is its display name, else "Unnamed team";
a personal team's is "<its user's label>'s team". A catalog's is its display
name, else its description (cut to 80 characters), else "Untitled catalog".

- **Rules.** A display name is trimmed, at most 80 characters (the server
  counts Unicode scalar values, so the console inputs have no `maxlength`),
  with no control characters or line breaks. Empty clears it. Migration
  `0019` cleans existing values and adds the CHECK constraints; it locks
  `users` and `teams` while it runs.
- **Who edits what.** A user's display name: an admin that reaches every
  catalog, a provisioner (not for an admin user), or the user on its
  account page. A shared team's: an admin, a provisioner or an owner of the
  team; a personal team follows its user. A catalog's: an admin on the
  catalog, an owner of its owner team or a provisioner.
- **Ids stay ids.** Ids appear in URLs, key and grant records, client
  config and MCP arguments. Tools return both `name` and `label`: say the
  label to people, pass the id in calls. Audit details record only the
  length of a display name, never the text.
- **Where labels show.** Views gain `label` fields, MCP results gain
  `catalog_label` (and `actor_label`, the label of a user actor; `user_label` for the person behind a revision, see [Who made a revision](#who-made-a-revision)), and the console resolves ids
  through `GET /api/v1/labels` (its own user and teams, and the catalogs
  it reaches) and, for the other users and teams a view shows, `GET
  /api/v1/labels/named`; pickers label users and teams from their own
  rows. Where two catalogs or teams share a label,
  the console pickers add the owner team where that tells them apart, then
  the slug, repeatedly, until the option texts are distinct. A new catalog's root
  `index.md` heading is its display name, else `Catalog`. An export is
  named `<stem>-<id>.tar.gz`, where the stem is the label with ASCII
  letters and digits lowercased, every other run of characters replaced by
  one `-`, ends trimmed and cut at 40 bytes ("Work Notes" gives
  `work-notes-<id>.tar.gz`). A label with no ASCII letter or digit leaves an
  empty stem, and the file is `<id>.tar.gz`. The console downloads it under
  the server's name.
- **Audit log.** An entry's `person_label` comes from the audit person
  record (a deleted or unnamed person shows "Unnamed user"). Its
  `target_label`, set for a catalog or shared team, is looked up at read
  time: a team's by its immutable team id, a catalog's by `created_at` (the
  log keeps no catalog id), so a catalog recreated under an old name does
  not lend its label to older entries.

### Suspending a team

Suspending a team stops every catalog it owns from answering its members,
through MCP, the REST API and the console, until the suspension is lifted.
Nothing is deleted or changed: documents, history, open changesets, keys,
grants and memberships all stay, and lifting the suspension restores every
credential exactly as it was. okf-mcp records no reason; the optional
**notice** (at most 2000 characters) is what members are told.

```sh
okf-mcp team suspend family --notice "Paused while the account is reviewed."
okf-mcp team list             # family ... suspended 2026-11-13T12:00:00Z ...
okf-mcp team unsuspend family
```

Over REST it is `PUT /api/v1/teams/{name}/suspension` with
`{"suspended": true, "notice": "..."}` or `{"suspended": false}` (the
[API reference](#users-and-teams-api) has the details); in the console,
Teams → a team → Suspension → Suspend, or Lift suspension. The route and the
console need an admin that reaches every catalog, as the other team writes
do; the command line needs only the database, as every subcommand does.

- **What it refuses.** Every read and write of the team's catalogs by a
  non-admin credential (API keys, OAuth grants and console sessions, whoever
  issued them): every MCP tool but `list_catalogs`, changeset writes and
  publish, the
  maintenance tools, and the `/api/v1` browse, search, history, health,
  report, changeset and maintenance routes. The answer is 403 with
  `{"error": "Suspended", "message", "catalog", "team", "notice"}`
  (`notice` is `null` when there is none); the message names the catalog
  and the team and ends with the notice.
- **What stays open.** **Export** (`GET /catalogs/{name}/export` and
  `GET /api/v1/catalogs/{name}/export`) for any credential that reaches the
  catalog, so members can take a copy. Console sign-in. **Listing:**
  `list_catalogs` and `GET /api/v1/catalogs` still show the catalog, with
  `suspended` set to `{"team", "notice"}` (null for an active catalog), and
  `GET /api/v1/me`'s ceiling is unchanged. In the console the team and its
  catalogs carry a "suspended" pill, Browse shows the notice with a link to
  Import & export, and the 403 shows its own message.
- **Admins are exempt.** An `admin` key, or an admin user's console session,
  is not affected, so the operator can still inspect, export and repair the
  catalogs. Import is a write, so only an admin can import into a
  suspended catalog.
- **Only credentials that reach the catalog learn of it.** Reach is checked
  first: a credential that doesn't reach the catalog still gets
  `CatalogNotSelected` (404), and no listing shows it the catalog. A
  credential that reaches it below the level a route needs gets `Forbidden`
  from routes that check the level first, and `Suspended` from tools that
  select their catalog first (`open_changeset`, the maintenance tools).
- **Connection lists drop, named catalogs refuse.** A call that names a
  suspended catalog (an argument, the key's default or the server's
  `default_catalog`, or the only catalog a key is granted, which it selects
  when nothing else does) is refused with `Suspended`. A call that falls back to
  the connection's list (`X-OKF-Catalogs`) leaves suspended catalogs out and
  reads the rest, so one suspended team doesn't break a connection that also
  reads other teams' catalogs; only a list whose every catalog is suspended
  is refused. `GET /api/v1/search` without `catalogs`, and `list_changesets`
  (and `GET /api/v1/changesets`) without `catalog`, leave suspended catalogs
  out the same way.
- **It follows ownership.** The suspension is the owner team's: a catalog
  moved to a suspended team (`catalog set-owner`), or created for one, is
  suspended too, and one moved away is not.
- **Repeating and lifting.** Suspending a suspended team keeps its first
  `suspended_at` and replaces the notice (a missing or blank notice clears
  it). Lifting a team that is not suspended changes nothing. An unknown team
  is a 404 (`UnknownName`); a notice over 2000 characters or holding a NUL
  is refused (409 `Refused`, a non-zero exit on the command line), leaving
  the team as it was. The console can't change a suspended team's notice
  without lifting it (#361); the CLI and REST can.
- **Timing.** On the replica that took the request, the REST route (which
  the console uses) applies the write's own change messages before
  answering, so the next request there sees the change. Every other replica
  follows when the change message reaches it, normally within a fraction of
  a second, with or without `[redis]`. A message lost while a replica's
  listener stays connected is covered by the caches' TTLs (`catalog_ttl` for
  the suspension on a catalog's row, `credential_ttl` for users and
  labels); a reconnect rereads everything. The command line writes the
  database and sends the same message from its transaction, so a running
  server sees it as it sees an API change.

### CLI, API and console parity

Every admin command for users, teams, assignment, sign-in, settings, mail,
the audit log and unlocks has a REST route and a console control.
`tests/admin_parity.rs` fails when a `user`, `team`, `settings`, `mail`,
`audit` or `unlock` subcommand, or one of the other commands below, has no
row here or its row names a route the API reference does not document. A
row may name a subcommand's flag (`user list --email`);
the test checks the flag exists.

| CLI | REST | Console |
|---|---|---|
| `okf-mcp user create` | `POST /api/v1/users` | Users → New user |
| `okf-mcp user update` | `PATCH /api/v1/users/{name}` | Users → user → Display name |
| `okf-mcp user list` | `GET /api/v1/users` | Users |
| `okf-mcp user list --email` | `GET /api/v1/users` | Users → the filter box |
| `okf-mcp user delete` | `DELETE /api/v1/users/{name}` | Users → user → Delete user |
| `okf-mcp team create` | `POST /api/v1/teams` | Teams → New team |
| `okf-mcp team update` | `PATCH /api/v1/teams/{name}` | Teams → team → Display name |
| `okf-mcp team list` | `GET /api/v1/teams` | Teams |
| `okf-mcp team add-member` | `PUT /api/v1/teams/{name}/members/{user}` | Teams → team → Add member |
| `okf-mcp team set-role` | `PUT /api/v1/teams/{name}/members/{user}` | Teams → team → a member's role |
| `okf-mcp team remove-member` | `DELETE /api/v1/teams/{name}/members/{user}` | Teams → team → Remove |
| `okf-mcp team delete` | `DELETE /api/v1/teams/{name}` | Teams → team → Delete team |
| `okf-mcp team suspend` | `PUT /api/v1/teams/{name}/suspension` | Teams → team → Suspension → Suspend |
| `okf-mcp team unsuspend` | `PUT /api/v1/teams/{name}/suspension` | Teams → team → Suspension → Lift suspension |
| `okf-mcp catalog set-owner` | `PUT /api/v1/catalogs/{catalog}/owner` | Catalogs → catalog → Change owner (Assign when unassigned); Users → What is unassigned |
| `okf-mcp key create --provisioner` | `POST /api/v1/keys` | API keys > New key > Provisioner |
| `okf-mcp key assign` | `PUT /api/v1/keys/{id}/user` | API keys → key or grant → Assign / Change user; Users → What is unassigned |
| `okf-mcp oauth code assign` | `PUT /api/v1/oauth/codes/{id}/user` | API keys → sign-in code → Assign / Change user; Users → What is unassigned |
| `okf-mcp unassigned` | `GET /api/v1/unassigned` | Users → What is unassigned (`/ui/#/users/unassigned`), and the count on the Users link |
| `okf-mcp user set-admin` | `PUT /api/v1/users/{name}/admin` | Users → user → Admin switch |
| `okf-mcp user set-email` | `PUT /api/v1/users/{name}/email` | Users → user → Email (Save, Clear) |
| `okf-mcp user invite` | `POST /api/v1/users/{name}/invite` | Users → user → Send setup email |
| `okf-mcp user entitle` | `PUT /api/v1/users/{name}/entitlements/{catalog}` | Users → user → Entitlements → Entitle, or a level; an owner of the catalog's shared team: Catalogs → catalog → Entitlements → Entitle |
| `okf-mcp user unentitle` | `DELETE /api/v1/users/{name}/entitlements/{catalog}` | Users → user → Entitlements → Remove; an owner: Catalogs → catalog → Entitlements → Remove |
| `okf-mcp user sessions` | `GET /api/v1/users/{name}/sessions` | Users → user → Console sessions |
| `okf-mcp user revoke-sessions` | `DELETE /api/v1/users/{name}/sessions` | Users → user → Sign out everywhere |
| `okf-mcp user sign-ins` | `GET /api/v1/users/{name}/sign-ins` | Users → user → Recent sign-ins |
| `okf-mcp catalog set-member-access` | `PUT /api/v1/catalogs/{catalog}/member-access` | Catalogs → catalog → Team member access (an owner of the catalog's shared team too, in a console session) |
| `okf-mcp catalog set-maintenance` | `PUT /api/v1/catalogs/{catalog}/maintenance` | Catalogs → catalog → Maintenance switch |
| `okf-mcp settings list` | `GET /api/v1/settings` | Settings → Policy |
| `okf-mcp settings set` | `PUT /api/v1/settings/{key}` | Settings → Policy → a value's Save |
| `okf-mcp settings reset` | `DELETE /api/v1/settings/{key}` | Settings → Policy → Reset |
| `okf-mcp mail test` | `POST /api/v1/mail/test` | Settings → Mail → Send test email |
| `okf-mcp mail queue` | `GET /api/v1/mail/queue` | Settings → Mail queue |
| `okf-mcp mail retry` | `POST /api/v1/mail/queue/{id}/retry` | Settings → Mail queue → Retry |
| `okf-mcp mail delete` | `DELETE /api/v1/mail/queue/{id}` | Settings → Mail queue → Delete |
| `okf-mcp unlock create` | `POST /api/v1/unlocks` | Unlock (on a locked catalog), or Unlocks → Unlock a catalog |
| `okf-mcp unlock list` | `GET /api/v1/unlocks` | Unlocks |
| `okf-mcp unlock end` | `DELETE /api/v1/unlocks/{id}` | Unlocks → End |
| `okf-mcp audit list` | `GET /api/v1/audit` | Audit |
| `okf-mcp audit export` | `GET /api/v1/audit/export` | Audit → Export |

`GET /api/v1/users/{name}` and `GET /api/v1/teams/{name}` (a user's or a
team's drawer in the console) have no command of their own: `user list` and
`team list` show the same facts. Nor do `DELETE
/api/v1/users/{name}/sessions/{id_prefix}` (one session's Sign out in the
drawer; `user revoke-sessions` ends them all) and `GET
/api/v1/catalogs/{catalog}/member-access` and `GET
/api/v1/catalogs/{catalog}/entitlements` (`catalog set-member-access`
prints the previous level; the owner's entitlement section reads the second). The self-service routes under `/api/v1/me` are
not admin actions and have no command. The users, teams and unassigned writes need
an admin key that reaches every catalog, as `POST /api/v1/catalogs` does;
reading users and teams needs any admin key, so a scoped admin can pick the
user of a key it creates. For any other admin the console shows these
controls disabled, or the Unassigned view a note, saying why.

### Who made a revision

Every revision keeps two names for who made it. `actor` is the credential's
actor: a key's actor, the OAuth client's name (a self-issued grant's actor is
the client's name, so every Claude Code user's edits share one), or a console
session's user name. It never changes. `user` is the person: the name (the id
the API uses, as `whoami` shows) of the user whose credential published the
revision. What is stored, at publish time, is that user's `users.id`
(`revisions.user_id`, a catalog-store column, null when no person is known),
and each read resolves it to the name, so a renamed or reused name never
misattributes an old revision.

- **Recorded once.** A publish records the publishing credential's user: a
  key's or grant's user, or the signed-in person of a console session. It is
  the publisher, not the opener of the changeset, who is recorded. An import,
  `catalog repair-indexes` and a catalog's starter files record none. Reassigning a
  key to another user afterwards changes nothing already recorded.
- **Deleting a user keeps it.** There is no foreign key: deleting a user
  leaves `revisions.user_id` as it was, so the revision still names that user.
  The user's name and label no longer resolve, so history shows
  `deleted:<users.id>` (the stored uuid) as `user` and `null` as `user_label`.
  Passing that exact value as the `user` filter finds their revisions; their
  old name does not. The prefix keeps names and stored ids apart: a name is
  `[a-z0-9-]+`, so no name has a colon, and a bare `user` is always a name. A
  user created later with a name equal to a deleted user's uuid (a uuid is a
  valid name) is that new user for the filter, and `deleted:<uuid>` is still
  the deleted one. A bare value no user has as a name matches nothing. This
  may be revisited for privacy.
- **Existing revisions.** Catalog migration 0004 fills `user_id` from the
  changeset's key (`changesets.key_id` to `api_keys.user_id`) where that
  resolves, on the account database's own store (`main`) only: another store's
  database has no `api_keys` table, so its older revisions stay null. A
  console session's older revisions stay null too (their changeset names
  `session:<user>`, no key). The migration is two files, so the table lock
  is short: 0003 adds the nullable column (a brief ACCESS EXCLUSIVE lock, no
  rewrite), 0004 backfills with one UPDATE that takes only row locks (reads
  and publishes continue; it makes dead rows for autovacuum), and 0005 builds
  the partial index `revisions_catalog_user_recent` with `CREATE INDEX
  CONCURRENTLY`, so publishes continue while it builds. A build that failed
  part-way is dropped and built again by the next start.
- **Where it shows.** `history` and `recent_changes` (MCP) and `GET
  /api/v1/catalogs/{catalog}/history/{path}` and `.../recent-changes` (HTTP)
  give each revision `user` and `user_label` beside `actor` and
  `actor_label`. `actor_label` is the label of a user *actor* (a console
  session): for a key or an OAuth grant it is null, and the person is
  `user_label`. `list_changesets` and `pending_changes` show the person who
  opened a changeset the same way, read from its key now (there is no
  revision yet). Anyone who may read a catalog's history sees these fields,
  the same readers who already see its actors; the person's display name and
  user id reach them even when the credential is an OAuth grant or a key.
- **Filters.** `history` and the HTTP history route take `user` (a user's
  id, or `deleted:<id>` as a revision shows a deleted user) and `since` (an RFC 3339
  time; a revision made at that time counts). A path whose revisions the
  filters exclude answers an empty list. `recent_changes` and the HTTP
  `recent-changes` route list a catalog's revisions across every path, newest
  first, with the same filters, and page by `after` (the previous page's
  `next_after`); a page holds `limit` revisions (default 50, at most 200).
  The unfiltered and `since` pages read `revisions_catalog_recent` and a
  `user` page reads `revisions_catalog_user_recent`, backwards, so a page
  reads about one page of index entries whatever the catalog's size.

### Audit log

Every change below writes one entry in the change's own transaction: the
change and its entry commit together, and a change whose entry cannot be
written fails. Operator reads (`catalog.admin_read`), which change nothing,
are counted in the background instead (below). A catalog entry names the catalog and belongs to its owner
team; a user entry names the user only through its audit person, which
reads `deleted user #N` once the user is deleted; a settings entry names
the setting and belongs to no team. The groups are the design's (Part 5
of `docs/superpowers/specs/2026-11-13-okf-operator-api-design.md`).

| Action | Recorded when | Details |
|---|---|---|
| **Users** | | |
| `user.deleted` | A user is deleted (`user delete`, `DELETE /api/v1/users/{name}`, the console), or deletes their own account (`DELETE /api/v1/me`, with the person as the actor). A refused deletion records nothing | `keys` (keys and grants deleted), `codes` (sign-in codes deleted) |
| `user.created` | A user is created (`user create`, `POST /api/v1/users`, the console). A refused create records nothing. A create with an email also records `user.email_changed` | none |
| `user.email_changed` | A user's email is set or cleared (`user set-email`, `PUT /api/v1/users/{name}/email`). A change of letter case only, or clearing an email that is not set, records nothing. A create with an email records it right after `user.created` | `cleared` (never the address) |
| `user.display_name_changed` | A user's display name changes. A provisioner is refused an admin user; the same text again, and a refused change, record nothing | `length` (in characters; never the text) |
| `user.admin_changed` | A user's admin flag changes (`user set-admin`, `PUT /api/v1/users/{name}/admin`). Setting the flag it already has records nothing | `from`, `to` |
| `user.invited` | A setup email is queued (`user invite`, `POST /api/v1/users/{name}/invite`). A refused invite (no email, no `public_url`) records nothing | none |
| `user.entitlement_set` | A user is given a catalog, or its level changes (`user entitle`, `PUT /api/v1/users/{name}/entitlements/{catalog}`). The entry belongs to the catalog's owner team. The level it already has records nothing | `catalog`, `access`, `previous` (`read`, `write`, or `null` for a new one) |
| `user.entitlement_removed` | An entitlement is removed (`user unentitle`, `DELETE /api/v1/users/{name}/entitlements/{catalog}`). The entry belongs to the catalog's owner team. Removing one not given records nothing | `catalog` |
| `user.sessions_revoked` | An admin ends a user's console sessions (`user revoke-sessions`, `DELETE /api/v1/users/{name}/sessions`, or one session with `DELETE /api/v1/users/{name}/sessions/{id_prefix}`). Ending none records nothing; a user ending their own session, and the sessions an admin-flag change ends, record no entry of this kind | `sessions` (how many ended) |
| **Teams** | | |
| `team.created` | A shared team is created (`team create`, `POST /api/v1/teams`); its person is the first owner | none |
| `team.member_added` | A user joins a team (`team add-member`, `PUT /api/v1/teams/{name}/members/{user}`); its person is the user | `role`, `from` (`null`) |
| `team.role_changed` | A member's role changes (`team set-role`, the same `PUT`). Setting the role a member already has records nothing | `role`, `from` |
| `team.member_removed` | A member leaves a team (`team remove-member`, `DELETE /api/v1/teams/{name}/members/{user}`); its person is the user. A refused removal records nothing | none |
| `team.deleted` | A shared team is deleted (`team delete`, `DELETE /api/v1/teams/{name}`), recorded before the row goes. A refused deletion records nothing | none |
| `team.suspended` | A team is suspended or its notice changes (`team suspend`, `PUT /api/v1/teams/{name}/suspension`). Suspending a suspended team with the same notice, a refused notice and an unknown or malformed name record nothing | `notice` (whether there is one; never its text), `already` (it was suspended before) |
| `team.display_name_changed` | A shared team's display name changes, by an admin, a provisioner or an owner of the team. A personal team is refused (its label follows its user); the same text again, and a refused change, record nothing | `length` (in characters; never the text) |
| `team.unsuspended` | A suspension is lifted (`team unsuspend`, the same `PUT`). Lifting an active team's suspension records nothing | none |
| **Catalogs** | | |
| `catalog.created` | A catalog is created (`catalog create`, `POST /api/v1/catalogs`) | none |
| `catalog.owner_changed` | A catalog gets another owner team (`catalog set-owner`, `PUT /api/v1/catalogs/{catalog}/owner`); the entry belongs to both teams. Giving it to the team that owns it records nothing | `from` (the old team's audit id, `null` for a catalog that had no owner), `to` |
| `catalog.description_changed` | A catalog's description changes (`catalog update`, `PATCH /api/v1/catalogs/{catalog}`). The same text again records nothing | `length` (in characters; never the text) |
| `catalog.display_name_changed` | A catalog's display name changes, by an admin on the catalog or an owner of its team. The same text again, and a refused change, record nothing | `length` (in characters; never the text) |
| `catalog.member_access_changed` | A catalog's member access changes (`catalog set-member-access`, `PUT /api/v1/catalogs/{catalog}/member-access`). The same value again records nothing | `from`, `to` (`none`, `read` or `write`) |
| `catalog.maintenance_changed` | A catalog's maintenance flag changes (`catalog set-maintenance`, `PUT /api/v1/catalogs/{catalog}/maintenance`), by an admin that reaches every catalog or an owner of its team signed in to the console. The same value again, and a refused change, record nothing | `from`, `to` (`on` or `off`) |
| `catalog.deleted` | A catalog is deleted (`catalog delete`, `DELETE /api/v1/catalogs/{catalog}`), recorded in the deleting transaction, which also marks the catalog deleted and owes the store cleanup ([Store cleanup](#store-cleanup)). A refused deletion records nothing | `forced` (unpublished changes were deleted with it), `generation` |
| `catalog.repaired` | `okf-mcp catalog repair-indexes` writes at least one file (a dry run, or a catalog with nothing missing, records nothing) | `files` (the paths written), `generation` (after the repair) |
| `catalog.imported` | An import writes or deletes at least one file (`okf-mcp import`, `POST /catalogs/{name}/import` and its `/api/v1` twin, the console). An import that changes nothing, or is refused, records nothing | `replace`, `scope` (`everything` or `machinery`), `written` (files added or changed), `deleted`, `generation` (after the import) |
| **Credentials** | | |
| `key.created` | An admin or signed-in user creates a key (CLI, API, console, self-service); provisioner creation also records this action | `access`, `all_catalogs`, `catalogs` (count), `maintenance_report`, `self_issued`, `expires` (boolean); a provisioner records only `provisioner: true` |
| `key.revoked`, `oauth.grant_revoked` | An API key or OAuth grant is revoked, including self-service. An already revoked or unknown target records nothing | none |
| `key.catalog_granted`, `key.catalog_ungranted` | A key's or OAuth grant's catalog grant is added or removed (the entry's target is a `key` or a `grant`); unchanged grants record nothing. Scoped to that catalog's owner team | `catalog` |
| `key.user_assigned` | A key, grant or sign-in code gets a different user (including revoked credentials); the entry's person is the new user. Assigning the existing user records nothing | none |
| `key.report_flag_changed` | A key's reporting flag changes; an unchanged or refused flag records nothing | `from`, `to` |
| `oauth.code_created`, `oauth.code_revoked` | A sign-in code is created or revoked; refused operations record nothing | Creation: `access`, `all_catalogs`, `catalogs` (count); revocation: none |
| `oauth.grant_created` | A sign-in code is redeemed (system actor `oauth`), or a signed-in user approves consent (their console-session actor) | `client` (host only), plus `code` (the immutable code id, not its secret) for redemption |
| **Settings** | | |
| `setting.changed` | `settings set` or `PUT /api/v1/settings/{key}` changes a setting ([Settings](#settings)). Setting the value already stored records nothing | `from` (the stored value before, `null` without an override) and `to`; for `support_contact` only `set` (whether a contact is set now), never the address |
| `setting.cleared` | `settings reset` or `DELETE /api/v1/settings/{key}` removes an override (with none, nothing is recorded) | `from`; nothing for `support_contact` |
| **Catalog stores** | | |
| `store.added` | `stores add` or `POST /api/v1/stores` adds a store (target kind `store`, named by the store). Refused additions record nothing | `location` (`host`) and `credential_kind`; never the host, port or credential reference |
| `setting.default_store_changed` | `stores set-default` or `PUT /api/v1/settings/default_store` changes the default store (target: the setting `default_store`). The same store again records nothing | `from`, `to` (store names) |
| `team.store_pinned`, `team.store_unpinned` | `stores pin` / `stores unpin` or `PUT` / `DELETE /api/v1/teams/{team}/store` changes a team's pin. An unchanged pin records nothing | `from`, `to` (store names, `null` for no pin) |
| **Catalog writes** | | |
| `catalog.published` | A publish (the `publish` tool, `POST /api/v1/changesets/{handle}/publish`, the console's Publish) writes at least one revision (one that changes nothing writes none and records nothing). Moves, splits and concept deletes are publishes and are recorded as such | `changeset` (the handle; the diff stays in the catalog's history), `files` (revisions written), `generation_from`, `generation_to` |
| `catalog.reverted` | A `revert` succeeds: a document's old revision (or its deletion) goes into a changeset. A refused revert records nothing | `path`, `revision`, `changeset` |
| `catalog.changeset_discarded` | An admin closes another key's changeset (`discard` without `paths`, or `DELETE /api/v1/changesets/{handle}`), or the store cleanup after a user deletion discards an open changeset of one of the deleted user's keys (`reason` `key_deleted`; the entry's person is the system). A key discarding its own changeset records nothing | `changeset`, `files` (entries dropped), `reason` (`admin` or `key_deleted`) |
| **Data access** | | |
| `catalog.exported` | Every export, whoever asks: `okf-mcp export` (source `cli`), `GET /catalogs/{name}/export` and its `/api/v1` twin, the console. Written before the archive is compressed; an export whose entry cannot be written fails. A refused export records nothing | `generation` (the one the archive holds), `files`, `catalog_id`; the entry belongs to the team that owned the catalog when it was read |
| `catalog.admin_read` | An admin credential reads a catalog's contents (MCP read tools, a changeset read, the console's and the API's browse, history and search routes) and its user is not in the catalog's owner team; a key with no user always counts. One entry per credential (or session user), catalog identity, owning team and clock hour, with an exact request count; members' reads, writes (recorded as publishes) and paths that read no contents (listing catalogs or changesets, opening a changeset, the connection's catalog list) record nothing. Only the catalogs a call actually reads count: a read that answers from the first catalog of a connection's list records only that one | `hour` (its start), `catalog_id` (the catalog's immutable id), and the request count |
| **Provisioner use** | | |
| `user.looked_up` | A provisioner key looks a user up by email (`GET /api/v1/users?email=`). One entry per key, outcome, matched user and clock hour, with an exact request count. A found user is the entry's person; no match, and an admin user (which a provisioner is never shown), is `not_found` with the route as the target (`GET /api/v1/users`). The address is never stored, not even hashed. No team's owners see it: it belongs to no team, and its actor is an operator. The provisioner's other reads and its requests that change nothing record nothing | `outcome` (`found` or `not_found`), `hour` (its start), and the request count; `overflow` for the hour's overflow entry (below) |
| `provisioner.refused` | A request by a provisioner key is refused: a route outside its allow-list (a 403 from the API's gate, or from MCP and the archive routes, `/mcp` and `/catalogs/{name}/export` and `/import`, which never take a provisioner key), or an allow-listed route its handler or a store check refuses (any other 403: a listing with no `email`, an admin user as the target). One entry per key, method, route template, reason and clock hour, with an exact request count. The target is the route (`GET /api/v1/keys`); no team's owners see it | `method` (a standard method, else `OTHER`), `route` (the matched template, never the literal path, query or body), `reason` (`route_not_allowed` or `forbidden`), `hour`, and the request count; `overflow` for the hour's overflow entry (below) |
| **Unlocks and maintenance passes** | | |
| `catalog.unlocked` | An admin unlocks a catalog (`unlock create`, `POST /api/v1/unlocks`, the console's Unlock). The entry's person is the unlocked admin, marked operator, so owners see neither who it was nor the reason. A refused unlock records nothing | `unlock` (its id), `category`, `reason` (removed for owners), `starts`, `expires` |
| `catalog.unlock_ended` | An admin ends a live unlock early (`unlock end`, `DELETE /api/v1/unlocks/{id}`, Unlocks → End); its person is the unlock's admin, as above. An unlock that simply expires records nothing more, and ending one already ended or expired is refused and records nothing | `unlock` |
| `catalog.maintenance_run` | A maintenance run ends, however it ends (closed by the runner or an operator, completed, or failed by recovery once its deadline passes), or a claim finds nothing to do. Source `system`, actor label `system:maintenance`, naming the run's credential; an admin-level key or a key of an admin user shows to owners as an operator. The runner's individual reads are not recorded | `run` (its id), `state` (`completed`, `failed` or `cancelled`), `findings` (reported in that run) |

Credential events belong to the owner teams of their granted catalogs
(all-catalogs credentials belong to no team); a code assignment belongs to
no team. Web changes retain the request's actor, source and client address;
the CLI records its operating-system actor. Secrets and hashes never enter
entry details. New credentials get their audit reference in the creation
transaction. An older credential's first reference resolution can wait on
another first resolver; existing-reference refreshes skip locked rows.
These credential-change entries do not record every provisioner request:
its lookups and refusals are recorded as above, and its other reads and
no-op requests stay in request traces and metrics.

Provisioner lookups and refusals are written off the request path too,
with the key (its `audit_credentials` id), source and address fixed when the
request is let in, and bounded so a scripted flood cannot fill the log:

- Repeats collapse. Within a clock hour one entry per key and kind of event
  (a lookup's outcome and matched user; a refusal's method, route template
  and reason) counts every request exactly, in the way `catalog.admin_read`
  does.
- A key opens at most 50 entries an hour. Further kinds that hour count in
  one overflow entry per action, target `*`, with `details.overflow` set;
  the entry for a kind already opened keeps counting in its own entry. The
  budget's counter rows are removed by the daily `prune_audit` job once
  their hour ended over an hour before (a grace for a lagging replica
  clock), not kept for the audit retention period: they gate only their
  own hour, and the entries hold the counts.
- At most 32 such writes are pending on a replica; a request that finds them
  all busy loses its count (a debug event says so) rather than queueing
  without bound. A failed write is logged and never fails the request.

An email lookup stores no part of the address, not even a hash. A found
lookup already names the user it matched, and a not-found one has no user to
name, so nothing needs to correlate repeated lookups of one address. A keyed
hash (under `server_keys`) would only add a value that links lookups of the
same unknown address, with nothing in the log to use it for, and a key to
rotate.
A team's owners never see these entries: they belong to no team, and the
actor is an operator, whom owners see only as "an operator". Admins see
them, and a provisioner key reading `GET /api/v1/audit` sees the ones it
wrote.

Admin reads are counted off the request path: the server notes each
catalog at most once per request and writes the count in the background,
and a stopping server waits (up to 30 seconds) for the counts in flight.
Who read and which catalog and team the read belongs to are fixed when the
request is let in, so a count written after the reader's user was deleted
names nobody, a read before a catalog's transfer stays in the old owner
team's log, and a read of a catalog deleted or replaced under the same
name keeps the original catalog's id. While the server is reloading a
catalog that was deleted and created again (or created outside it), an
admin credential's read of it is refused with `Busy` (retry) rather than
recorded against the wrong catalog. A server that crashes between a
request and its write can lose that one count.

The log is append-only: the database refuses any change to an entry, and
any delete of one younger than `audit_retention_days` (at least 30 days,
whatever the stored value). The daily `prune_audit` housekeeping job (once per cluster)
deletes the entries older than
`audit_retention_days` plus an hour (a margin for clock skew between the
server and the database); nothing else deletes them. Lowering the setting
takes effect at the next daily pass; raising it keeps the entries not yet
deleted.

**Not recorded.** Ordinary reads by a catalog's customers (members of the
owner team, entitled non-admins); sign-ins (they are in
`okf-mcp user sign-ins`); the maintenance runner's individual reads and
reports (in their own tables; each pass writes one
`catalog.maintenance_run`); reads under an unlock (they are tallied on the
unlock instead, shown by `unlock list`); and a provisioner's metadata
reads, unchanged requests and refusals (#423). A change that changes
nothing, and a refused change, record nothing.

**People, teams and credentials in entries.** No column of an entry holds a
person's name, an email address or a secret. An entry names people through
`audit_people`, teams through `audit_teams` and keys, grants and sign-in
codes through `audit_credentials`; it keeps no foreign key to users, teams,
keys or catalogs, so deleting one never changes an entry. Deleting a user
renames its `audit_people` row to `deleted user #N` and clears its
credentials' ids and labels (`credential #N`), in the deletion's own
transaction. A team is its immutable identity: a team deleted and created
again under the same name is a new `audit_teams` row, so its owners never
see the old team's entries. The actor label is only a kind (`console
session`, `api key`, `oauth grant`), `cli:<os user>` or `system:<what>`;
a key's own label lives in `audit_credentials`. Client IPs of web requests
are kept for the retention period.

**Who reads it.** An admin that reaches every catalog (or an admin user's
console session) reads every entry in full; the command line reads the
same. A team's owner, signed in to the console, reads the entries that
belong to that team, with operator staff shown as "an operator" (no
person, credential, label or IP), other people's IPs left out, an unlock's
reason removed and only their own team in `teams`. A provisioner reads the
entries it wrote. Nobody else reads it; the full rules are in the
[Audit API](#audit-api).

**Where.** `okf-mcp audit list` and `okf-mcp audit export` on the command
line, `GET /api/v1/audit` and `GET /api/v1/audit/export` over REST (both
in the [Audit API](#audit-api)), and the console's Audit page (Records →
Audit; an owner picks one of their teams), with the same filters and an
Export button that downloads `audit.jsonl`.

A `catalog.unlocked` entry, as an admin reads it (`GET /api/v1/audit`):

```json
{
  "id": 412,
  "at": "2026-11-13T12:00:00Z",
  "action": "catalog.unlocked",
  "source": "console",
  "actor": {"kind": "known", "person": "staff", "credential": null,
            "label": "console session", "operator": true},
  "ip": "192.0.2.10",
  "target": {"kind": "catalog", "name": "main", "person": "staff"},
  "teams": ["family"],
  "details": {"unlock": 1, "category": "support", "reason": "ticket 12",
              "starts": "2026-11-13T12:00:00Z",
              "expires": "2026-11-13T14:00:00Z"},
  "requests": null
}
```

The owner of `family` reads the same entry as `"actor": {"kind":
"operator"}`, `"ip": null`, `"target": {"kind": "catalog", "name": "main",
"person": null}` and `details` without `reason`.

### Unlock before access

With `require_unlock` on ([Settings](#settings)), operator staff open a
customer catalog's contents only under a live unlock: one admin user, one
catalog, a reason category (`support`, `deletion_request`,
`export_request`, `security_investigation`, `legal` or `other`), a
free-text reason (a ticket number) and at most `unlock_max_hours`. It
applies to that admin's console session and own keys, and ends at its
expiry or when an admin ends it; it cannot be extended.

```sh
okf-mcp unlock create --catalog main --user staff --category support \
    --reason "ticket 12" --hours 2
# unlocked main for staff until 2026-11-13T14:00:00Z (unlock 1)
okf-mcp unlock list [--catalog main] [--live]
# 1  main  staff  support  live  reads 0  writes 0  exports 0  until 2026-11-13T14:00:00Z
okf-mcp unlock end 1
# ended unlock 1
```

`--hours` defaults to `unlock_max_hours`; a longer one, 0, a blank or
over-long reason, a user who is not an admin (or does not exist) and an
unknown catalog are refused. `unlock list` shows every unlock, newest
first, tab-separated: its state (`live`, `expired` or `ended`), the reads,
writes and exports made under it, and when it ends (an early end, else
its expiry). The reason is in the audit log's `catalog.unlocked` entry.
The same over REST is in the [Unlocks API](#unlocks-api); a catalog's
owners read its unlocks there redacted.

**Who needs one.** With `require_unlock` on, every credential that is not
a customer of the catalog: an admin key, an admin user's console session,
any other key that reaches the catalog without its user being in the
owner team (an admin-issued key outside its teams), and a key with no
user (which cannot be unlocked: give it one). A customer is a member of
the catalog's owner team, or a user who is not an admin and is entitled
to the catalog; neither ever needs an unlock, whatever their key's level.
An admin user's own entitlement does not count. An unlock lets in exactly
its user, through any of that user's credentials, to exactly its catalog,
until it expires or is ended. With `require_unlock` off nothing changes:
operator reads are recorded as hourly `catalog.admin_read` entries.

**What "contents" covers.** Every read or write of a catalog's documents:
the MCP read and write tools (a changeset handle included, so an open
changeset stops working when its unlock ends), opening a changeset,
`list_changesets` (a locked catalog's changesets are left out, and naming
it is refused), the browse routes (tree, documents, concepts, history,
revisions), search (an unnamed search leaves a locked catalog out),
export, import, and every maintenance tool (each selects its catalog
first). Metadata and administration need none: listing catalogs, a
catalog's description, owner and member access, users, teams, keys,
settings, the audit log and unlocks themselves. The command line works on
the database directly and needs none (its export and import are audited
with source `cli`).

**The refusal.** 403 with `"error": "UnlockRequired"` and the `catalog`;
its message says how to get an unlock, or, for a key with no user, that
it needs one first. Reach, level and suspension are checked first, so a
credential that does not reach the catalog still gets 404, and one below
the level a route needs gets `Forbidden`. Tools that select their catalog
before checking their own level (`open_changeset`, `maintenance_dispose`,
the maintenance work tools) answer `UnlockRequired` first, as they do for
a suspended catalog. A connection list leaves a locked catalog out, and
is refused only when every catalog in it is locked. Each request made
under an unlock adds one to its reads, writes (a changeset write, an
import) or exports, shown by `unlock list`; those counts replace the
hourly admin-read entries.

**The maintenance runner.** A reporting key that is not an admin key
needs no unlock to read and report (claims, renewals, submissions), on
any server. Its writes get no exemption: a reporting key with write
access needs an unlock to write like any other operator credential. A
catalog whose `maintenance` flag is off keeps the runner out whatever its
grants and whatever `require_unlock`: it is refused with 403 `Forbidden`
and `"maintenance": "off"`.

The flag is `on` for every catalog until someone turns it off. An admin
that reaches every catalog, or an owner of the catalog's owner team signed
in to the console, sets it: `okf-mcp catalog set-maintenance main off` (or
`on`), `PUT /api/v1/catalogs/{catalog}/maintenance` with
`{"maintenance": "off"}`. A plain member of the team, a scoped admin key and
a provisioner cannot. The change takes effect at once, and is recorded as
`catalog.maintenance_changed`; `GET /api/v1/catalogs` shows each catalog's
`maintenance`. A runner refused on its claim skips the catalog: it claims
nothing, exits 0 with reason `maintenance_off`, and replays the same claim
next time (see [Report-only runner](#report-only-runner)). Every pass that
ends writes one `catalog.maintenance_run` entry, which the team's owners
see in their audit log.

## Signing in and self-service

People sign in to the console as themselves, with any of three methods: a
password, a passkey, or a linked account at an OIDC provider. Signed in,
they manage their own API keys and OAuth grants within a **ceiling** an admin
sets, and approve OAuth consent as themselves. Pasting an API key into the
console still works, for automation and for admins without a user.

Signing in needs `public_url` (section 3). It gives the cookies' origin, the
passkey relying party (its host) and the OIDC redirect URIs. Without it,
`GET /signin` offers only "Use an API key instead", and every other sign-in
route answers 404 (`POST /signin` included).

### Signing in

`GET /signin` offers:

- **Email and password**, while the `password_sign_in` setting is on (when
  it is off, the form is gone and `POST /signin` answers 404);
- **Sign in with a passkey**, while passkeys are on (below): the browser
  offers the passkeys it holds for this server, so no email is typed first;
- **one button per `[[auth.oidc]]` provider**;
- **Use an API key instead**: the console's pasted-key card.

It takes `return_to`, a same-origin path starting `/ui/`, `/read/` (the
mobile reader) or `/oauth/authorize?`; anything else becomes `/ui/`. A browser with a live session is not shown the form: the page answers 303 to that same checked `return_to`. (A session that is expired, idle, revoked or cannot be checked gets the form.) The console's card links
to it with "Sign in with your account", carrying the page you were on.

- **One refusal for everything.** Every refused password sign-in (a wrong
  password, an unknown email, a locked account, a malformed form) answers
  401 "Email or password is incorrect", and costs the same: an unknown
  email is checked against a dummy hash.
- **Lockout.** After 10 consecutive failures an account locks for 15
  minutes. The lock is stored, so a restart does not lift it.
- **Per-address limit.** Each client address may try 30 times per 15
  minutes (a token bucket: a burst of 30, refilled over 15 minutes), shared
  by password sign-in, passkey sign-in and OIDC starts; past it, 429. IPv6
  addresses count by their `/64`. Without `[redis]` the limit is in memory,
  per process, and a restart resets it; with `[redis]` it is kept in Redis,
  so every replica counts against one budget (falling back to each
  replica's own count only while Redis fails).
  Behind an ingress, set `auth.trusted_proxy_header`, or every request seems
  to come from the ingress and all of them share one limit.
- **Other sites' forms are refused.** Every sign-in, forgot, setup and reset
  form POST, and every open passkey route, answers 403 when the browser's
  `Sec-Fetch-Site` header is neither `same-origin` nor `none` (login CSRF).
  A request without the header, such as curl's, is let through.

Passwords are hashed with argon2id (19 MiB, 2 passes, 1 lane). A password
has 12 to 1024 characters and is not in the bundled list of common
passwords; there are no composition rules.

Every attempt writes a sign-in event (method `password`, `passkey`, `oidc`
or `setup`; outcome `ok`, `failed` or `locked`; the client address), which
the user sees on their Account page and an admin in the user's drawer or
with `okf-mcp user sign-ins NAME`, and the same facts as an `info` log line
with the user id. Events older than 90 days are pruned.

### Sessions

A sign-in starts a **console session**. The `okf_session` cookie holds 256
random bits (`HttpOnly; SameSite=Lax; Path=/`, and `Secure` when
`public_url` is `https`); the server stores only its SHA-256, and a CSRF
token derived from it, stored hashed too.

- **Lifetime.** A session ends after `console_session_idle` unused (default
  12 hours) or `console_session_absolute` after sign-in (default 7 days),
  whichever comes first. Use is recorded at most once per
  `min(1 minute, idle / 4)`, so an idle session ends between `idle` and
  `idle` plus that interval after its last use.
- **CSRF.** Every write the cookie authenticates must carry `X-OKF-CSRF`
  with the session's token (`GET /api/v1/session` gives it); otherwise it
  gets 403. The console does this itself.
- **One credential per request.** A request with a bearer key or token is
  that credential's, whatever cookie it carries. `/mcp` and
  `/catalogs/{name}/export|import` take only bearer credentials. The
  `/api/v1` copies of export and import take a session, at its levels.
- **What a session reaches.** An admin user's session is an admin that
  reaches every catalog, as an every-catalog admin key is. A member's
  session reaches exactly its ceiling, at the ceiling's levels. The admin
  flag, teams and entitlements are read from the user cache on every
  request, and the ceiling on a catalog is resolved from them and the
  catalog's directory row (owner team, member access) at the check.
- **No changesets.** A session never owns a changeset, so MCP writes still
  need a key or a grant. An admin user's session may still publish or
  discard any key's changeset from the Changesets page, recorded as that
  user.
- **Ending sessions.**
  - Sign-out (the console's Sign out, `DELETE /api/v1/session`) deletes the
    session.
  - Changing a user's admin flag signs them out of every session, and they
    sign in again with the new flag.
  - Changing your password, adding a passkey or linking an account from the
    Account page signs your other sessions out, unless "Keep me signed in
    elsewhere" is ticked.
  - A password reset signs every session of the user out.
  - The Account page signs out one session at a time, this browser's
    included.
  - Deleting your own account ends every session of yours.
  - An admin signs a user out everywhere with
    `okf-mcp user revoke-sessions NAME` or the user drawer's "Sign out
    everywhere", or ends one session from the drawer.

### Deleting your own account

A signed-in person deletes their own account from the Account page's
"Delete your account" section (`DELETE /api/v1/me`, issue #355). It asks
again that it is them, in the same request:

- with the current password, checked as a sign-in is (a wrong one counts
  toward the 10-failure lockout, and a locked account is refused); or
- with a fresh assertion from one of their own passkeys
  (`POST /api/v1/me/passkeys/confirm/start` for the challenge, which only
  the account's passkeys can answer; the ceremony is single use, bound to
  the 5-minute `okf_passkey` cookie and to this session).

- with a fresh re-authentication at a linked OIDC provider, for a person
  whose only sign-in method is a linked account (issue #775). The page's
  "Re-authenticate with <provider>" button runs
  `POST /api/v1/me/identities/{provider}/reauth`, which sends the browser to
  the provider with `prompt=login` and `max_age=0`. The callback (the same
  `/auth/oidc/{provider}/callback` route) verifies the ID token as a
  sign-in does (signature, issuer, audience, nonce, expiry), and the
  token must also carry an **`auth_time` claim no more than 5 minutes
  before the callback** (and at most a minute ahead of the server's clock,
  for skew); a token with no `auth_time`, or an older one, is not accepted
  as a proof (see below for what the person is told). So a
  provider that ignores `max_age` is accepted only if the person really did
  authenticate within those 5 minutes; one that never sends `auth_time`
  cannot be used for deletion at all. The window is fixed in the server
  (`REAUTH_AUTH_TIME_WINDOW`), not configurable. The token's subject must be
  the one the person linked at that provider when the flow started. The
  callback creates no session, links nothing and records no sign-in: it only
  keeps a proof for this console session, valid for 5 minutes and taken
  once by `DELETE /api/v1/me` with `{"oidc_reauth": true}`. A sign-in or
  link callback never makes one, and a callback finished in another
  browser or session is refused (400). The flow shares the link starts'
  allowance (10 in a burst, then one a minute). With Redis the proof lives
  in the `oidc:reauth` key space, so any replica can finish and spend it.
- **A provider that did not vouch for a fresh sign-in.** When the token is
  genuine (it passed every other check) but its `auth_time` is missing, too
  old or in the future, the callback still returns to the Account page and
  keeps a marker for this session instead of a proof. The next
  `DELETE /api/v1/me` with `{"oidc_reauth": true}` then answers **403 with
  `"error": "ReauthStale"`** (a kind of its own, distinct from `Forbidden`),
  spends the marker, and says: "`<label>` did not confirm a fresh sign-in, so
  the account cannot be deleted yet; sign out of `<label>`, sign back in,
  then try again." `<label>` is the provider's configured display label,
  never the issuer, the subject or token details. The Account page shows
  that text with a "Try again" button. Every other refusal (another
  identity, a used or expired proof, another session's proof) stays generic
  and never says this.
- **Telling the person on their return, before any delete attempt** (issue
  #1099). When the Account page loads after the return from the provider, it
  asks `GET /api/v1/me/identities/{provider}/reauth`, which answers
  `{"state": "none" | "ready" | "stale"}` and nothing else (no subject,
  issuer or time): `ready` when a fresh proof waits for this session,
  `stale` when the marker above waits, `none` when nothing waits (never
  started, expired, already used, made for another provider, or another
  session's, which a session cannot see). On `stale` the page shows the
  sign-out/sign-in text and the "Try again" button at once and offers no
  delete button; on `ready` it offers the delete button. The call is
  read-only: it does not take the proof, write anything or extend its 5
  minutes, so a delete after `ready` still succeeds, and it applies the same
  rule as the deletion (one function judges the stored outcome for both). It
  needs a console session (403 for a key, a grant or no session; 401
  without credentials) and answers 404 for an unknown provider or with OIDC
  off. If the call fails, the page falls back to offering the delete
  button, and the deletion answers as before.

The page offers each way the account has. An account with no password, no
passkey and no linked account at a configured provider cannot confirm, and
the page says to set a password, add a passkey or link an account first. The
page draws no deletion control until it has read the account, since the
confirmation asks you to type its ID. A key or OAuth token is refused (403):
only a console session may delete its account.

The deletion is the same one an admin's `DELETE /api/v1/users/{name}` runs
([Deletion rules](#deletion-rules)), in one transaction under the user's
row lock: keys and OAuth grants (revoked ones too), sign-in codes, every
console session, passkeys, linked identities, emailed links, entitlements,
team memberships and the personal team all go, or nothing does. Under that
lock it also checks that this session is still in force, and that the
password is still the one typed, the passkey is still the account's or the
re-authenticated identity is still linked to it, so a reset, password
change, passkey removal, unlink or sign-out that commits first refuses the
deletion. A refused deletion still uses the proof up; re-authenticate again. It is recorded as `user.deleted`, with the person as
the actor (the log then names them `deleted user #N`, as for any deleted
user). The refusals are the deletion rules': the last admin user, a
personal team that owns a catalog, and the only owner of a shared team are
refused with a 409 whose reason the page shows; nothing is deleted or
disabled. A deletion clears the session cookie (and any passkey ceremony
cookie), and the console shows the
sign-in card with "Your account was deleted."

### Setting up an account, and resetting a password

No admin ever sees a password. An admin gives the user an email and sends a
setup link:

```sh
okf-mcp user set-email alice alice@example.com
okf-mcp user invite alice
```

The link (`{public_url}/account/setup?token=...`) opens a page where the
person chooses a password, registers a passkey, or both, and is signed in.
It works once, for `setup_link_ttl` (24 hours by default), and a newer setup
link replaces it. Changing the user's email ends every link they have not
used yet.

On `/signin/forgot` anyone may ask for a reset link. The answer is always
"If an account exists, we sent a link.", except a 403 for another site's
form and a 429 past the per-address limit (20 requests per hour). Each email
gets at most 3 links per hour; past that the answer is the same and nothing
is sent. A reset link works once, for
`reset_link_ttl` (1 hour by default), and opens a set-password page. The
reset signs every session of the user out, mails a "sign-in method added"
notice, signs nobody in, and sends the browser to `/signin`. An expired,
used or unknown link answers 410 "This link has expired. Ask for a new
one."

### Passkeys

Passkeys are on when `public_url`'s host is a domain name (a relying party
id cannot be an address, so a `public_url` such as `https://192.0.2.10` turns
them off, and `/auth/passkey/*` answers 404). They are registered on the
setup page or the Account page, and always need user verification (a PIN or
a biometric). The relying party is `public_url`'s host, so a passkey works
only at that host: changing it strands every registered passkey (people then
sign in another way and add a new one). Sign-in uses discoverable
credentials. The server stores public keys only, and checks and advances each
passkey's signature counter: a counter that did not move is logged at `warn`
and refused as a failed sign-in. A ceremony waits at most 5 minutes, in
memory, keyed by the `okf_passkey` cookie (`SameSite=Strict`), so a restart
ends the ones in progress.

### OIDC providers

A provider is an `[[auth.oidc]]` table in the config file, with its client
secret in an environment variable. This example uses a placeholder provider
at `https://id.example.com`:

```toml
[[auth.oidc]]
id = "example"                          # stable: stored with each linked identity
label = "Example ID"                    # the button text
issuer = "https://id.example.com"
client_id = "okf-console"
client_secret_env = "OKF_OIDC_EXAMPLE_SECRET"
scopes = ["openid", "email", "profile"]
```

1. Register a confidential web client at the provider, with the redirect
   URI `{public_url}/auth/oidc/<id>/callback` (here
   `https://okf-mcp.example.com/auth/oidc/example/callback`). Note its
   client id and secret.
2. Add the table above, and put the secret in the server's environment as
   `OKF_OIDC_EXAMPLE_SECRET` (a Kubernetes secret, as for `DATABASE_URL`).
3. Restart the server. `/signin` shows "Example ID", and the pod needs
   outbound HTTPS to the issuer.

The flow is the authorization code flow with PKCE (S256), `state` and
`nonce`, bound to the browser by a 10-minute, single-use `okf_oidc` cookie.
The provider's discovery document (`<issuer>/.well-known/openid-configuration`)
is cached for an hour; a failed refresh keeps the cached one and is tried
again after a minute, and with nothing cached the sign-in answers 503. The
`/auth/oidc/*` routes exist only while a provider is configured; an unknown
provider is 404.

After the ID token checks out (issuer, audience, nonce, expiry):

1. an identity already linked (by provider and subject) signs its user in;
2. otherwise, if the token's email is verified and matches a user's email
   (in any letter case), the identity is linked, the user signs in, and they
   get a "sign-in method added" email;
3. otherwise sign-in is refused (403) with "No account for this email. Ask
   an admin to invite you." There is no self-signup.

A bad `state`, nonce or token is 400 "Sign-in failed; try again". A
signed-in person can also link a provider from the Account page, whatever
the provider's email says: the callback links it only for the browser still
holding the session that started the link, and returns to `/ui/#/account`.
Each user may start 10 links in a burst, then one a minute. A user links
at most one account per provider, and an account linked to another user
cannot be linked (409). Unlinking is refused for the last sign-in method
(409). A person whose only sign-in method is a linked account can
re-authenticate at the provider to delete their account
([Deleting your own account](#deleting-your-own-account)): the same callback,
a flow of its own kind that signs nobody in and requires an `auth_time` no
more than 5 minutes old. The Settings page lists the providers (id, label,
issuer), never a secret. Changing a provider's `id` orphans its linked identities.

### Mail

Mail goes through `[mail]`'s provider:

- `smtp`: any relay, over STARTTLS or TLS from the first byte;
- `postmark`: Postmark's HTTP API;
- `ses`: Amazon SES, through the AWS SDK's SES v2 `SendEmail` (not SES
  SMTP), with the SDK's standard credentials (below);
- `log`: **for development only.** It logs each whole message at `info`,
  recipient, subject and body, setup and reset links included, and
  delivers nothing. Anyone who can read the log can use those links. Never
  run it in production; it is the default only so that a server without
  `[mail]` starts.

The templates are a setup link, a reset link, "a sign-in method was added
to your account" (after a password change or reset, a new passkey or a
linked identity) and the Settings page's test email.

A request never sends mail itself. It queues a `SendMail` job in the
server's own Postgres database (the `apalis` schema, section 4); a setup
link and a "method added" notice are queued in the same transaction as the
change they report. A worker in every `serve` process
sends it, on the workers' shared pool; `user invite` and `mail test`
on the command line only queue. Two workers never send the same mail.

- A provider error that may pass is retried: a network error, Postmark 401,
  429 or 5xx, an SMTP 4xx, and for SES a timeout, throttling or a sending
  quota, any 408, 429 or 5xx, a paused or suspended account, and any
  credential or authorization failure (no credentials found, any 401 or
  403, an expired token, `AccessDenied`). The first retry is 30 seconds later, doubling up to an
  hour, for 24 hours from when the mail was queued.
- A refusal fails the mail at once: an SMTP 5xx, any other Postmark error
  (422 among them), and for SES `MessageRejected`, an unverified MAIL FROM
  domain, a bad request (an invalid address among them) or any other 4xx.
  An SES error is sorted by its error type, then by its HTTP status.
- Failed mail keeps its last error. Sent mail is deleted after 7 days and
  failed mail after 30, by the server's hourly prune.
- A job holds the rendered message, link included, until it is pruned: the
  queue listings never show a body.

```sh
okf-mcp mail test alice@example.com    # queues the test email
okf-mcp mail queue                     # id, status, attempts, to, template, run_at, error
okf-mcp mail retry ID                  # failed mail: send again now, with a new 24-hour window
okf-mcp mail delete ID
```

The queue lists every mail not yet sent, as `pending` (waiting for a
worker), `sending` (a worker holds it), `retrying` (failed, with tries
left) or `failed` (refused, or out of time). Retry takes `retrying` and
`failed` mail; Delete takes anything but `sending`. The Settings page shows
the same queue with Retry and Delete. A setup or reset link in a deleted
mail stays valid until it expires; send a new one to replace it.

#### Amazon SES

```toml
[mail]
provider = "ses"
from = "okf <okf@example.com>"

[mail.ses]
region = "us-east-1"   # optional; otherwise AWS_REGION and the SDK's chain
```

- **Credentials** come only from the AWS SDK's standard chain:
  environment, profile, web identity (IRSA), then the container provider,
  which is what **EKS Pod Identity** uses. okf has no access-key setting:
  give the pod an identity instead. Credentials are fetched on the first
  send and refreshed by the SDK, not at start-up, so a missing or refused
  identity shows up as a retrying mail rather than a failed start.
- **Region:** `mail.ses.region` wins; otherwise `AWS_REGION`, the
  profile, then instance metadata. With none, `serve` stops at start-up
  with "found no AWS region". Pod Identity does not set a region, so set
  `mail.ses.region` (or `AWS_REGION`) on EKS.
- **Permission:** the role needs `ses:SendEmail` on the From identity
  (and on the configuration set, if the identity has a default one), for
  example `arn:aws:ses:us-east-1:111122223333:identity/example.com`.
- **Identity:** `mail.from`'s address or its domain must be a verified
  SES identity in that region.
- **Sandbox:** an account in the SES sandbox sends only to verified
  recipients, and SES answers `MessageRejected` for any other, which
  fails the mail at once. Request production access before inviting
  users.
- **EKS Pod Identity prerequisites:** the `eks-pod-identity-agent` add-on
  runs on the cluster, and a Pod Identity association maps the server's
  namespace and service account to an IAM role with the permission above.
  The pod must run under that service account; associations apply to pods
  started after they exist, so restart the server after creating one.
- **Each send is one SES request,** bounded at 30 seconds (10 to connect),
  with no SDK retries: the queue's own backoff retries it.

Smoke test after deploying (needs a recipient SES will deliver to, which
in the sandbox means a verified one):

```sh
okf-mcp mail test alice@example.com    # queue it; the running server sends it
okf-mcp mail queue                     # empty once sent; else its status and last error
```

A sent test mail leaves the queue and the server logs `mail sent`; it
arrives from `mail.from`. A `retrying` entry whose error names
`AccessDenied` or a credential failure points at the Pod Identity
association or the role's policy; `MessageRejected` (failed at once)
usually means an unverified identity or a sandbox recipient.

### Feedback

With a `[feedback]` table, the `send_feedback` tool lets an agent, or a user
through their agent, send the operator feedback about using the server:
something confusing, broken, missing or good, and the console's "Send
feedback" button lets someone signed in with an account send it from a
form. It is posted to a
[Forminit](https://forminit.com) form. Without the table the tool is not in
`tools/list`, the server instructions do not mention it, the console draws
no form (`POST /api/v1/feedback` answers 404), and nothing is sent.

The console's form shows the same third-party disclosure as the tool's
description ("The feedback goes to <product>'s team through a third-party
form service (Forminit)."), what is sent, and asks the person to leave out
anything they don't want to share. It posts to `POST /api/v1/feedback`
(console session and `X-OKF-CSRF` only), which checks and queues the
submission exactly as the tool does: the same limits, the same queue, with
`from` always `user`, no `tool`, `client` `web console` and
`credential_kind` `session`. Its hourly limit counts each user's console
submissions together, whichever session sent them. The Forminit key never
reaches the browser.

```toml
[feedback]
forminit_form_id = "<form id>"            # not a secret
api_key_env = "OKF_MCP_FORMINIT_API_KEY"  # optional: the variable holding the X-API-KEY
# worker = true                           # set false on the replicas that must not run it
# max_per_hour = 10                       # per credential
# retention = "30d"                       # how long sent and failed feedback is kept
```

What a submission holds, and nothing else: text blocks `message`, `kind`
(`bug`, `confusing`, `missing`, `praise` or `other`), `from` (`user` or
`agent`), `tool` (when given), `server_version`, `client` (the MCP client's
name from `initialize`, or `web console`), `credential_kind` (`api_key`,
`oauth`, or `session` for the console) and
`feedback_id` (the feedback's own random id, the one the tool returns); a
`rating` block (1 to 5) when given; and a `sender` block whose `userId` is
the credential's user slug (`unassigned` for a legacy key no user owns).
No email, display name, key name, token or catalog content is sent unless
it is in the message. The tool's description tells agents so, and to ask
the user before including catalog content or personal details.

When a tool fails with an internal error (`Database`, `Migrate`, `Io` or
`Config`), the client gets only the generic message ("internal error; the
server log has the details"), and the detail stays in the server's log. The
error body also carries two fields, neither of which reveals the detail:

- `trace_id`: the tool span's OpenTelemetry trace id, the same id the
  exported `tool failed` event and the stderr line carry (with
  `telemetry.stderr.format = "json"`), so an operator can go from a feedback
  entry to the failing span and its stderr detail. It is sent with or without
  `[feedback]`, and left out when the span has no valid trace (telemetry off).
- `report`: one optional sentence ("If this keeps happening, you can report
  it with send_feedback (kind: bug, tool: <tool>), quoting trace_id.").
  Sent only when `[feedback]` is set, so it never names a tool the client does
  not have, and not for a passing database outage (a pool timeout, a lost
  connection, a Postgres shutdown), which would otherwise make every
  connected agent send a report of the same incident.

No other error carries either field. The HTTP API's internal errors carry `trace_id` too
(see the `/api/v1` error shape in the Web console section), but never `report`.

Any credential can send, `read` included, and no catalog is needed. A
message is at most 4,000 characters, and each credential may send
`max_per_hour` (10) in any hour; both are refused with `LimitExceeded`
(`limit` `feedback_message` or `feedback_rate`).

The tool never calls Forminit. It stores a `feedback` row (`pending`) and,
in the same transaction, a `send_feedback` job in the `apalis` queue the
mail uses (the job holds only the row's id), and answers with the id. The
feedback worker posts it:

- a 2xx marks the row `sent` and stores Forminit's `submission.hashId`
  (`submission_id`);
- a 429, a 5xx or a network error is retried, 30 seconds later and then
  doubling up to an hour, for 10 tries (about three hours), then the row is
  `failed`;
- any other 4xx (`FORM_NOT_FOUND`, `FORM_DISABLED`, a validation error) marks
  it `failed` at once with Forminit's error code in `last_error`, never
  retried.

Delivery is at-least-once. A worker takes a job exclusively in normal
operation (`FOR UPDATE SKIP LOCKED`), but a retry after an unconfirmed or
unrecorded acceptance can post a duplicate: a timeout after Forminit took
the post, or a failure to mark the row `sent` afterwards, sends it again on
the next try, possibly from another replica. A duplicate carries the same
`feedback_id`, so de-duplicate on that field in Forminit.

The hourly prune deletes `sent` and `failed` rows `retention` (30 days)
after they were sent or given up on. Until then the operator sees and
retries failed rows from the command line (a running server's worker does
the sending):

```sh
okf-mcp feedback list         # failed rows: id, user, kind, attempts, failed at, error
okf-mcp feedback retry ID     # back to pending with no tries, and queued again
```

A `failed` row with `FORM_NOT_FOUND` or `VALIDATION_ERROR` fails every
submission until the form id or the rating block is fixed, so check
`[feedback]` before retrying. The worker logs a warning for each row it
marks `failed` (`feedback not delivered; marked failed`, with the error
code as `reason`). Deleting a user deletes that user's `pending` and
`failed` feedback in the same transaction, and a submission racing the
deletion either lands before it (and goes with it) or is refused because
the user is gone; `sent` rows, already delivered,
are left for the prune. No try that starts after the deletion commits
sends the deleted rows. A post the worker had already started is not
cancelled and can still reach Forminit with the user's name (the deletion
does not wait up to 30 seconds for it); its answer then updates nothing,
so it is not sent again. For anything else, read the table:

```sql
SELECT id, status, attempts, last_error, created_at
FROM feedback WHERE status <> 'sent' ORDER BY created_at;
```

### Settings

Policy and branding settings live in the database, so the Settings page
changes them without a restart; the config file's `[auth]` table gives each
policy and branding setting's default. `audit_retention_days` and the three
unlock settings have no config key: their defaults are `400`, `false`, `4`
and `true`.

A deployment that rebrands the server sets the branding in `[auth]`, so a
fresh database shows it with no Settings change (`support_contact`,
`feedback_url`, `docs_url`, `billing_url` and `onboarding_url` are left out for none):

```toml
[auth]
product_name = "Acme Notes"
support_contact = "help@example.com"
feedback_url = "https://example.com/feedback"
docs_url = "https://docs.example.com"
billing_url = "https://billing.example.com/login"
onboarding_url = "https://docs.example.com/start/first-15-minutes/"
mcp_server_name = "acme-notes"
```

`docs_url` and `onboarding_url` are separate settings, both off by default:
the documentation link names a site, while the onboarding redirect names
one page on it, and a server whose documentation has no getting-started
page (or another operator's) can show the link without sending new users
to a page that is not there.

MCP clients see the product name as the server's display title
(`serverInfo.title` in `initialize`), and in `send_feedback`'s description
and the instructions line that offers it, as it stands when each session
starts. `mcp_server_name` (config-only, default `okf_mcp`) is the
identifier the server reports as `serverInfo.name`: 1 to 64 ASCII letters,
digits, `-`, `_` and `.`. Changing it breaks no client: clients key a server,
and prefix its tool names, by the name the person gave it when adding it,
not by `serverInfo.name`.

The file's values are checked as `settings set` checks them, and a bad one
stops the server at start-up, naming the key. The passkey prompt a browser
or password manager shows ("Save a passkey for ...") names the config
file's `product_name`, never a value saved in Settings: WebAuthn fixes that
name when the server starts.

| Key | Default | Meaning |
|---|---|---|
| `password_sign_in` | `true` | Offer email and password sign-in |
| `max_keys_per_user` | `20` | Live self-issued keys per user, `0` to `1000` |
| `max_key_lifetime` | `none` | Longest self-issued key expiry; `none` means no limit |
| `console_session_idle` | `12h` | Console session idle timeout |
| `console_session_absolute` | `7d` | Console session absolute timeout |
| `setup_link_ttl` | `24h` | Setup link lifetime |
| `reset_link_ttl` | `1h` | Password reset link lifetime |
| `product_name` | `okf` | The product name people see: in every email (subjects and text), the console (sidebar, login card and tab title), the sign-in, forgot, setup, reset and OAuth consent pages, the [mobile reader](reference/reader.md#name) (its install name "<product> Reader", home-screen label and tab title) and (from the config file only) the passkey prompt. 1 to 60 characters with no control characters; surrounding spaces are trimmed |
| `support_contact` | `none` | An email address or an `https://` URL (with a host). When set, every email ends with "Questions? Contact ..." and the server pages show the same line (a URL as a link); `none` shows no line |
| `feedback_url` | `none` | An `https://` URL (with a host, at most 500 characters, no whitespace or control characters; an email address is refused). When set, the console's sidebar footer shows a "Send feedback" link to it, opening in a new tab; `none` shows no link. With a `[feedback]` table, someone signed in with an account sees the in-console feedback form instead ([Feedback](#feedback)), never both; a key still sees this link. Nothing else changes when it is unset |
| `docs_url` | `none` | An `https://` URL, checked as `feedback_url` is. When set, the console's sidebar shows a "Documentation" link to it on every page, for everyone signed in (by key or account, admin or not), opening in a new tab; `none` shows no link |
| `billing_url` | `none` | An `https://` URL, checked as `feedback_url` is: the start page of an external billing service, which should sign the person in through this server's OAuth (as a registered client, authorization code with PKCE) and send them to their own billing. Because they are already signed in to the console, they enter no password or email. Never a customer-specific session URL or a secret: it is public in `/ui/brand.json`, and no credentials or personal information are passed in it. This server holds no billing-provider code or customer mapping. When set, the console's sidebar shows a "Billing" link under "Your account" to it, for people signed in with an account (admin or not), opening in a new tab with `rel="noopener noreferrer"`; people signed in with an API key alone never see it. `none` (the default) shows no link, and saving or disabling it updates an open console without a reload. Which service a deployment points it at is the operator's to configure; nothing in this repository names one |
| `onboarding_url` | `none` | An `https://` URL, checked as `feedback_url` is: where a successful first password setup (`POST /account/setup`) sends the person, still signed in, instead of the console. The setup answers with a short welcome page ("Your account is ready", signed in) that has a button linking to the URL and a link to the console, not a 303 and not a timed refresh: browsers apply the console CSP's `form-action 'self'` to redirects after a form POST and would block one to another origin, and a link click is not a form submission. The person clicks to continue; nothing moves on its own. A setup that continues an OAuth authorization (`return_to=/oauth/authorize?...`) resumes it instead; sign-in, password reset and a passkey setup are unchanged. `none` goes to the console as before |
| `audit_retention_days` | `400` | How many days [audit log](#audit-log) entries are kept, `30` to `3650`. A daily pass deletes older ones |
| `require_unlock` | `false` | Whether operator credentials (admin keys, admin users' sessions) need an unlock, with a reason and a time limit, before they open a customer catalog's contents |
| `unlock_max_hours` | `4` | The longest an unlock lasts, in hours, `1` to `72`; a longer one is refused (422, `limit: "unlock_max_hours"`) |
| `unlock_notify_owners` | `true` | Whether the owners of a catalog's owner team (those with an email address) are emailed when it is unlocked: the catalog, the category and the end, never who or the reason |

```sh
okf-mcp settings list                      # key, value, then "default" or "set by WHO at T"
okf-mcp settings set max_key_lifetime 90d
okf-mcp settings reset max_key_lifetime    # back to the config file's default
okf-mcp settings set product_name "Acme Notes"
okf-mcp settings set support_contact https://example.com/help
okf-mcp settings set feedback_url https://example.com/feedback
okf-mcp settings set docs_url https://docs.example.com
okf-mcp settings set billing_url https://billing.example.com/login
okf-mcp settings set onboarding_url https://docs.example.com/start/first-15-minutes/
okf-mcp settings set audit_retention_days 730
```

A value is checked as its config key is: `true`/`false` (or `on`/`off`), a
whole number, or a duration like `12h` above zero and at most ten years
(`none` for `max_key_lifetime`); `product_name`, `support_contact`,
`feedback_url`, `docs_url`, `billing_url` and `onboarding_url` as in the table. A bad one is refused, naming the key: 422
over REST, a non-zero exit on the command line, and an unknown key is clap's
list of the keys. Each change records who made it (`cli:<os user>` for the
command line, the admin's actor over REST and in the console), writes an
[audit log](#audit-log) entry, and reaches every server through
the change feed (a lost message is bounded by `credential_ttl`). Setting the value already stored changes and records
nothing. A stored value this server cannot read (from
a newer version, or a hand edit) is logged at `warn` and the default used.
Mail is rendered when it is queued, so a queued mail keeps the brand it was
queued with; `mail.from` (the sender) stays configuration.

### Self-service keys and grants

A signed-in person's **My keys** page (`/ui/#/my-keys`) lists, creates,
renames and revokes the keys they made themselves. A self-issued key:

- names its catalogs, each `read` or `write`, all within the ceiling; never
  "all catalogs", never `admin`, never a maintenance reporter;
- counts against `max_keys_per_user` while it is live;
- expires no later than `max_key_lifetime` allows (and at that limit when
  no expiry is given), or never if that is `none`;
- belongs to the signed-in user, with its name as the default actor.

Creating one above the ceiling, past the key limit or beyond the lifetime
limit answers 422 naming the limit (`ceiling`, `max_keys_per_user` or
`max_key_lifetime`). An expired key stops working at once. **My grants**
(`/ui/#/my-grants`) lists and revokes the user's OAuth grants, with when
each was last active ([Idle grants](#idle-grants)). My keys also
lists the keys an admin made for the user: those are admin-issued, so not
capped by the ceiling, and an admin manages them on the API keys page.
**Account** (`/ui/#/account`) changes the password, adds, renames and
removes passkeys, links and unlinks providers, lists and ends sessions, and
shows recent sign-ins, teams and the ceiling. It refuses to remove the last
sign-in method (409).

### The ceiling and member access

A user's **ceiling** is what their self-issued keys and grants may reach:

- the catalogs owned by every team the user belongs to, at each catalog's
  **member access** (`none`, `read` or `write`; `write` by default);
- plus the user's **entitlements**, catalogs an admin adds one at a time,
  each `read` or `write`;
- where both give a catalog, the higher level.

A self-issued key or grant reaches, on each catalog, the lower of its own
level and the ceiling's; a catalog outside the ceiling is unreachable. Every
change that can move a ceiling (team membership, a catalog's owner or member
access, entitlements, the admin flag) reaches running servers through the change
feed, at once on the server that made it, so lowering a ceiling
lowers live keys and grants. An admin user's session is uncapped, but their
self-issued keys and grants are capped like anyone's: the ceiling comes from
teams and entitlements, never from the admin flag. Admin-issued keys and
grants from sign-in codes are never capped.

```sh
okf-mcp catalog set-member-access recipes --access read   # members read only
okf-mcp user entitle bob notes --access read             # one more catalog for bob
okf-mcp user unentitle bob notes
```

Admins change member access and entitlements, and changing them through a
key needs an admin that reaches every catalog (the server's check is
`store::ceiling::may_set_member_access`). An **owner of a shared team**
signed in to the console may also do both for the catalogs their team owns
(issue #352), from the catalog's drawer (Catalogs, then the catalog: Team
member access, and Entitlements). The rules:

- **Console session only.** An owner's API keys and OAuth grants cannot
  change member access or entitlements, as with the maintenance switch, the
  audit viewer and the unlock list: they keep the admin-only refusal (403).
- **Shared teams only.** The owner of a personal team gets none of these
  controls on its catalogs.
- **Entitlements go to members of the owning team only**, at `read` or
  `write` (never `admin`): the use is raising one member above the team's
  member access. An unknown user and a user outside the team get the same
  403, with the same body, so nothing about other tenants can be probed; an
  unknown catalog and one the caller does not own are refused alike. The
  answer carries only the entitlement on that catalog, never the user's
  entitlements elsewhere (an admin still gets all of them). An entitlement
  an admin gave to someone who has since left the team is not shown to the
  owner and cannot be removed by one.
- **Checked under locks.** The owner's membership row (and, for an
  entitlement, the user's and the catalog's) is locked in the transaction
  that writes, so a demotion, a removal or a change of the catalog's owner
  team that commits meanwhile is refused or waits.
- **Same effects as an admin's change:** a ceiling write bumps
  `admin_generation` and re-caps running keys and grants at once, and the
  change is audited (`catalog.member_access_changed`,
  `user.entitlement_set`, `user.entitlement_removed`) with the owner as
  actor.

### Rollout

Migration `0010` only adds. Until an admin acts nothing changes: every key,
grant, sign-in code and catalog works as before, every existing key is
admin-issued (uncapped), every catalog's member access is `write`, and
pasting a key into the console still works. After deploying:

1. Set `public_url` (section 3) if it is not set yet; it turns on OAuth too
   ("OAuth connectors").
2. Configure `[mail]` (until then the `log` provider writes each setup link
   to the server log, which is acceptable on a test server only) and, if
   wanted, `[[auth.oidc]]`. Put their secrets in the environment, and
   restart.
3. Give yourself an email, the admin flag and a setup link (inside the pod,
   with `kubectl exec` as in [First run](#6-first-run)):

   ```sh
   okf-mcp user set-email alice alice@example.com
   okf-mcp user set-admin alice --on
   okf-mcp user invite alice
   okf-mcp mail queue          # the setup mail, until the worker sends it
   ```

4. Open the link, choose a password or register a passkey, and you are
   signed in. Settings (`/ui/#/settings`) shows the policy, the providers
   and the mail queue; send yourself a test email from it.
5. Team catalogs start at `member_access = write`: before members start
   creating their own keys, review shared catalogs and set `read` (or
   `none`) where intended.
6. Invite the others (`user set-email`, then `user invite`, or the user
   drawer's Email and "Send setup email").

## Report-only maintenance

### Permissions and lifecycle

Maintenance reports are separate database records, not canonical documents.
There is no automatic canonical editing, merging, splitting, deletion,
verification stamping or changeset publication. No scheduler, model service,
inference, external fetching, embeddings, cross-catalog synthesis, new UI or
report search indexing exists in this slice. The housekeeping jobs
are not a maintenance scheduler. External URLs are lexical attributions only;
the server never fetches them. Treat pinned source prose as untrusted evidence,
not instructions.

Grant reporting independently of ordinary access. These examples use a synthetic
`demo` catalog; `DATABASE_URL` and any client bearer keys should come from your
secret environment, not report text:

```sh
okf-mcp key create investigator --access read --catalogs demo --actor synthetic-investigator --user alice --maintenance-report
okf-mcp key list
okf-mcp key maintenance-report REPORTER_ID --enabled false
okf-mcp key maintenance-report REPORTER_ID --enabled true
# For an ambiguous grouping name, select the actor instead of using its ID:
okf-mcp key maintenance-report investigator --actor synthetic-investigator --enabled true
```

Replace `REPORTER_ID` with the immutable ID printed on creation/listing.
`key list` includes `maintenance_report true|false`. Changes reach a running
server through the change feed (a lost message is bounded by
`credential_ttl`). Read, Write and Admin keys
all default to reporting disabled. Reporting plus Read permits claim, renew,
submit and closing one's own runs; it grants no canonical write access. Plain
Read permits pinned reads and all report get/list operations. Ordinary Write
permits dispositions and closing another credential's run without reporting;
even a writer closing its **own** run must have reporting enabled. Catalog
grants still apply. Actor attribution comes from the authenticated immutable
credential, never an `actor` parameter in an MCP request.

### Tool reference

Every call requires `catalog`; connection defaults never select it. Requests
reject unknown fields, blank IDs, invalid enums and invalid revision pins.
Parameters below are required unless marked optional. Each structured JSON
response repeats `catalog` beside the result fields (not under a `result` key).
Report get/list responses contain no lease tokens.

| Tool | Parameters besides `catalog` | Result / permission |
|---|---|---|
| `maintenance_claim` | `idempotency_key`, optional `batch_size` | `run_id`, `leases`; Read + reporting |
| `maintenance_renew` | `work_id`, `token`, `idempotency_key` | Lease with `run_id`, `work_id`, `token`, `pin`, `reasons`, `expires_at`; Read + reporting, owner only |
| `maintenance_read_revision` | `path`, `revision` | `pin`, exact UTF-8 `content`, `outdated`; Read |
| `maintenance_submit` | `work_id`, `token`, `idempotency_key`, `submission` | Receipt: `run_id`, `work_id`, `outcome`, `finding_ids`, `outdated`; Read + reporting, owner only |
| `maintenance_close` | `run_id`, `state`, `rationale`, `idempotency_key` | Run summary; own run requires reporting, another credential's requires Write |
| `maintenance_list_runs` | optional `after`, `limit`, `order` (`asc` default, or `desc` for newest first) | `items`, `next_after`; Read |
| `maintenance_get_run` | `run_id` | `run` summary and `work` array with original evidence/finding IDs; Read |
| `maintenance_list_findings` | optional `after`, `limit` | `items`, `next_after`; Read |
| `maintenance_get_finding` | `finding_id` | Finding input, actor, time, disposition/history and read-time `outdated`; Read |
| `maintenance_dispose` | `finding_id`, `disposition`, `rationale`, `idempotency_key` | Finding with appended disposition history; Write, reporting alone insufficient |

`maintenance_close.state` accepts `completed`, `failed` or `cancelled`, not
`running`. Explicit completion requires every item terminal; the last terminal
item also completes a running run automatically. Closing failed/cancelled
invalidates outstanding leases and releases work for a later run. Empty claims
complete immediately. History and completed work survive closure/recovery.
An authorized `state: failed` close against an already terminal run acknowledges
its actual `completed`, `cancelled` or `failed` state idempotently; it does not
rewrite the state, terminal time/reason, or work history. This also covers another
caller terminalizing the run between lookup and close. Ownership and reporting/Write
checks still apply before replay. The acknowledgment is not a submission receipt.

### Limits, evidence and retries

| Boundary | Implemented limit |
|---|---|
| Claim batch / primary concepts per run | Default 10; 1–50 inclusive; failed attempts count too; a new batch is a new run |
| Lease | 900 seconds; renew to `min(now + 900 seconds, run deadline)` |
| Run deadline | Creation + 3,600 seconds; exhaustion fails the run as `budget_exhausted` |
| Expiry | Expired at `now >= expires_at`; recovery records `lease_expired`, eligible immediately with a new attempt/token |
| Explicit failed/inconclusive retry | 86,400-second cooldown |
| Aging review interval | Default 30 days; `maintenance_review_intervals = { pg = "30d" }`; minimum one day |
| Findings | At most 20 per primary work submission |
| Evidence entries | At most 20 submission-level local entries; at most 20 combined local/external entries per finding |
| Excerpt | Nonempty; at most 2,048 UTF-8 bytes (2 KiB), local or external |
| Submission | At most 262,144 bytes (256 KiB) of serialized `Submission` JSON, not the surrounding MCP request |
| Rationale | At most 4,096 UTF-8 bytes for close/disposition |
| `reported_tokens` | Optional nonnegative integer, at most 9,223,372,036,854,775,807 (storage limit) |
| Idempotency key | 1–128 ASCII characters; no whitespace or control characters; secret-scanned |
| Revision pin | Positive revision, concept `.md` path of at most 256 bytes; no traversal or machinery paths; MCP uses `/...md` |
| Report page | Default 20; 1–200 inclusive |

All timestamps come from the injected server clock; none is a caller parameter.
All mutations require an idempotency key, scoped by catalog, immutable credential
and operation. Retry the same normalized request with the same key to get the
original response without renewing a lease or changing counts/times. Reusing a
key with different content is `Refused`. Authorization runs before replay;
revoked/ungranted/disabled reporters cannot replay successful mutations. A
successful submit replay remains valid after lease expiry. Never share lease
tokens; a same-name replacement credential is not the original owner.

Local excerpts must be exact substrings of persisted UTF-8 revision bytes,
including line endings, not normalized snapshot text. Completed submissions
must cite the exact primary pin in `submission.evidence`; every finding needs
local evidence even with external observations. `suggested_link`,
`possible_duplicate` and `conflicting_assertions` require two distinct concept
paths. Other categories are `potentially_outdated_information` and
`unverifiable_claim`. `relationship` is `direct_statement`, `inference` or
`external_observation`. External observations require literal `http://` or
`https://` resources without credentials, whitespace or percent-encoded
authorities, a nonempty excerpt and nonblank attribution. Reports cannot serve
as evidence inputs.

Caller-supplied `submission.outcome` is `completed`, `inconclusive` or `failed`.
`superseded` is a server-assigned outcome, rejected as submission input.
Completed work may assess `supported_current`, `outdated` or `not_assessed`;
aging work must use one of the first two. Inconclusive work may assess
`unverifiable` or `not_assessed`; failed submissions use `not_assessed`.
Failed/inconclusive work need not supply evidence. Reported support is an
attributed investigator assessment, never human/server verification; absence of
evidence never means current. Completed aging work schedules another review
after the interval whether current or outdated. A newer revision is uncovered
and invalidates effective support; source edits/deletion can supersede work and
make historical reports `outdated`. Legacy `verified` stamps only hint at
scheduling (malformed/future stamps are ignored). Maintenance never writes
canonical `verified`, `reviewed` or `generated` stamps.

The existing secret scanner rejects detected patterns in report text and
idempotency keys with masked `SecretDetected` errors. It is **heuristic**, not a
proof that text contains no secret. Keep secrets out of summaries, excerpts,
URLs, rationales and exported reports even if validation accepts them.

### Investigate and record follow-up

These are synthetic MCP `tools/call` parameter objects, not new HTTP endpoints.
Use your client's normal MCP connection with a reporter key from its secret
environment. Suppose the synthetic catalog has `/service.md` revision 1 whose
exact content includes `The service port is 8080.`. Do not assume a real catalog
returns this pin or these IDs: substitute the actual lease and pinned bytes.

1. Claim one item with the reporting credential:

```json
{"name":"maintenance_claim","arguments":{"catalog":"demo","batch_size":1,"idempotency_key":"demo-claim-1"}}
```

Save returned `run_id` and `leases[0]` (if `leases` is empty, there is no work).
Replace `WORK_ID`, `LEASE_TOKEN`, `RUN_ID`, path and revision below with those
returned values; do not invent them. Each lease also supplies `reasons` and
`expires_at`.

2. Read the lease's exact revision, not the latest canonical document:

```json
{"name":"maintenance_read_revision","arguments":{"catalog":"demo","path":"/service.md","revision":1}}
```

3. Submit an inconclusive investigation with an attributed finding. This example
cannot establish whether the recorded port is current and does not claim it can:

```json
{
  "name": "maintenance_submit",
  "arguments": {
    "catalog": "demo",
    "work_id": "WORK_ID",
    "token": "LEASE_TOKEN",
    "idempotency_key": "demo-submit-1",
    "submission": {
      "outcome": "inconclusive",
      "assessment": "unverifiable",
      "summary": "The recorded port needs independent confirmation.",
      "evidence": [{"pin":{"path":"/service.md","revision":1},"excerpt":"The service port is 8080."}],
      "findings": [{
        "category": "unverifiable_claim",
        "assertion": "The document states a service port without confirmation of current runtime state.",
        "follow_up": "Ask the service owner to confirm the current port.",
        "uncertainty": "No runtime observation was available in this investigation.",
        "relationship": "direct_statement",
        "evidence": [{"pin":{"path":"/service.md","revision":1},"excerpt":"The service port is 8080."}],
        "external": []
      }],
      "reported_tokens": null
    }
  }
}
```

`reported_tokens` is optional investigator accounting, not measured billing or
proof a model ran. Save the returned receipt's `finding_ids[0]` as `FINDING_ID`.
If work takes longer, renew before expiry using
`{"catalog":"demo","work_id":"WORK_ID","token":"LEASE_TOKEN","idempotency_key":"demo-renew-1"}`
as `maintenance_renew` arguments. To stop unfinished work, `maintenance_close`
arguments are
`{"catalog":"demo","run_id":"RUN_ID","state":"cancelled","rationale":"Stop synthetic investigation.","idempotency_key":"demo-close-1"}`.

4. Inspect and export report JSON with any catalog reader:

```json
{"name":"maintenance_get_run","arguments":{"catalog":"demo","run_id":"RUN_ID"}}
{"name":"maintenance_list_runs","arguments":{"catalog":"demo","limit":20}}
{"name":"maintenance_list_findings","arguments":{"catalog":"demo","limit":20}}
{"name":"maintenance_get_finding","arguments":{"catalog":"demo","finding_id":"FINDING_ID"}}
```

The block contains four separate calls, not one JSON document. List results are
`{"catalog":"demo","items":[...],"next_after":"LAST_ROW_ID"}` when more
rows exist. Continue the same list tool with
`{"catalog":"demo","limit":20,"after":"LAST_ROW_ID"}`; stop when
`next_after` is `null`. Pages ascend by `(created_at, id)`. Cursors are row IDs,
not offsets/timestamps, and belong to that catalog and list type. Foreign or
missing cursors/records return `NotFound`. To export reports, save these JSON
results, paginate both lists, and retrieve run/finding detail by ID. There is no
separate report-export tool or report bundle endpoint.

5. Switch to an ordinary writer credential and record a decision:

```json
{"name":"maintenance_dispose","arguments":{"catalog":"demo","finding_id":"FINDING_ID","disposition":"accepted_for_follow_up","rationale":"Owner confirmation is needed before any canonical edit.","idempotency_key":"demo-dispose-1"}}
```

Other dispositions are `dismissed`, `needs_input` and `open` (reopen). History is
append-only. Acceptance **does not apply changes**. Any later canonical edit or
publication remains a separate, explicitly authorized changeset workflow.

### Export, retention and backup boundaries

Reports do not enter normal search, lint/oldest/links reports, canonical history
or canonical bundle exports (`okf-mcp export` and `GET /catalogs/{name}/export`).
A canonical bundle is not a maintenance backup; importing one does not restore
report history. Database backups include maintenance tables, leases, receipts,
findings and dispositions along with canonical data. Treat database backups as
sensitive: public report get/list output omits tokens, database backups do not.
Deleting a concept preserves historical reports but makes affected pins stale.
Deleting a catalog removes its maintenance records under existing cascade
semantics; recreating the same name is a new identity and cannot replay old
receipts. No automatic report pruning is introduced here.

## Report-only runner

`okf-maintenance` is a separate binary, not a server poller or resident scheduler.
`cargo run` preserves the legacy `okf-mcp` default. Use `cargo run --bin
okf-maintenance -- ...` for development, or the explicitly provisioned runner
executable for operations. It needs no database credentials or catalog checkout.
The controller can use only `search`, `get_concept`, `history`,
`maintenance_claim`, `maintenance_renew`, `maintenance_read_revision`,
`maintenance_submit`, `maintenance_close` and `maintenance_get_run`. It cannot
publish, change canonical content, dispose findings or fetch source URLs.

A catalog whose owners turned maintenance off (`catalog set-maintenance C off`)
refuses its claim with `Forbidden` and `"maintenance": "off"`. The runner then
claims nothing and exits 0 with reason `maintenance_off`, recording no stop: the
pending claim replays exactly on the next run, which works normally once the flag
is on again. Refused after it claimed work (the flag turned off mid-run, even
after every submission, before the close), it stops with reason `maintenance_off`
and exit 4, like any interrupted run; the pending request replays on the next run.

### Prerequisites and commands

1. Provision a dedicated ordinary **Read + maintenance_report** key restricted to
   one catalog, and a separate provider API key/account/quota. These are operator
   actions, not effects of building this repository. An interactive Claude
   subscription/OAuth login does **not** supply this adapter's authentication:
   `--bare` uses `ANTHROPIC_API_KEY`, never interactive OAuth/keychain state.
2. Choose an exact provider model ID and install a trusted, pinned Claude
   executable outside user homes (the service uses `ProtectHome=true`). The
   inspected real CLI was **`2.1.319 (Claude Code)`**, version/help only; no paid
   output has been verified. Synthetic envelopes target that contract but do not
   prove it works against a provider. Do not silently update the executable or
   expected version when a check fails.
3. Adapt [config.toml](../examples/maintenance/config.toml): replace the `.invalid`
   HTTPS endpoint, catalog/model placeholders and absolute paths. Certificates
   must verify; redirects and inherited proxies are disabled. Set **both**
   `allow_model_upload` and `acknowledge_inflight_budget_overshoot` true only with
   authorization. Both default false, including in the example. Network preflight
   also requires both flags under the existing client contract, though it does
   not upload source documents or run inference. `require_hard_billing_cap=true`
   is rejected as unsupported. Unknown config fields and invalid values fail
   closed without echoing them.
4. Supply `OKF_MAINTENANCE_TOKEN` and `ANTHROPIC_API_KEY` via the environment or a
   protected service `EnvironmentFile`; never flags or TOML. Avoid shell history,
   `set -x`, environment dumps and world-readable files. The controller uses one
   loaded credential object for the connection, journal binding and provider
   adapter. Only the provider key reaches the model child, not the MCP token,
   database credentials, inherited user configuration or proxy overrides.

With prerequisites and explicit operator authorization (these commands are not
run by installation scripts):

```sh
/opt/okf-maintenance/okf-maintenance preflight --config /etc/okf-maintenance/config.toml
/opt/okf-maintenance/okf-maintenance run-once --config /etc/okf-maintenance/config.toml
```

`preflight` reads configuration, credential presence, exact executable version,
required help flags and MCP tool schemas. It creates no journal/lock, claims no
work, performs no report mutation and runs no inference. It does not certify the
key's reporting grant or the paid result envelope. Success is exit 0 with exactly
a zero-count `RunSummary`: `run_id:null`, `reported:0`, `inconclusive:0`, `failed:0`,
`skipped:0`, `invocations:0`, `accounting_known:true`, `reason:"preflight_passed"`.
Here known accounting means no model invocation occurred, not provider validation.

`run-once` locks private state before connecting and either recovers the saved
run or performs one bounded batch. A later invocation may start a fresh budget
window only after the prior run is durably terminal. SIGINT/Ctrl-C and SIGTERM
request cancellation and **await supervisor completion**, including owned inference
cleanup and any permitted report/close acknowledgements. The CLI does not abort
its supervisor. Forced kill/OOM can still interrupt durability and require recovery.

Normal stdout is one JSON summary (or evaluation object), never logs. Run fields
are `run_id`, `reported`, `inconclusive`, `failed`, `skipped`, `invocations`,
`accounting_known`, `reason`. `reported` includes completed and superseded receipts;
`skipped` counts work without receipts. Fixed reasons describe operational state,
not source content. Errors use fixed redacted stderr messages, no raw clap values,
TOML diagnostics, remote bodies, tokens or child stderr.

| Exit | Meaning |
|---|---|
| 0 | Preflight passed; offline evaluation completed; or all selected work acknowledged without runtime failure (including empty batches and inconclusive aging). Evaluation 0 is not a quality threshold. |
| 2 | Invalid CLI/configuration or incompatible readiness contract/version/schema. |
| 3 | Another process holds this local state lock. No new claim or model invocation. |
| 4 | Runtime/permission/transport/I/O/journal/cancellation failure, partial execution or unknown accounting. Runtime protocol failure summaries also exit 4, not 2. |

**Exit 0 never verifies present-day facts.** Catalog consistency is not independent
real-world evidence. Aging always maps to `Inconclusive` / `Unverifiable`; changed-only
work can be `NotAssessed`. The runner never submits `SupportedCurrent`, renews a
canonical verification stamp or counts copied assertions as independent validation.

### Privacy, limits and accounting

Selected primary/neighbor catalog text is uploaded to the configured provider.
Review data classification and provider retention before consent. The secret
scanner is heuristic: passing it is not proof that a document is safe to upload.
Prompt injection remains untrusted data, not permission for shell/browser/MCP tools.
The child gets a private temporary working directory/HOME, an allowlisted
environment and tool-restricting CLI flags. These are **not an OS sandbox or
network egress allowlist**. Systemd filesystem hardening does not change that.

| Control | Default / hard local bound |
|---|---|
| Batch / invocations | Batch 10 (1..50); invocations default `min(10,batch_size)`, 1..batch_size and at most 50 |
| Run duration | Standalone 1,800 seconds, accepted 1..3,000; service example 1,740 |
| Child duration / concurrency | 90 seconds, exactly one model process at a time |
| Primary / neighbor / serialized prompt | 65,536 / 16,384 / 131,072 bytes; at most 4 neighbors from 20 discovered paths |
| Child stdout / stderr | 1,048,576 / 65,536 bytes, bounded and discarded after processing |
| MCP | 10-second RPC timeout; at most 3 transient attempts, delays 1 then 2 seconds; exact mutation payload/key replay |
| Heartbeat / safety margin | Every 60 seconds (earlier sweeps when needed); 30 seconds before lease/server deadline |
| Reservation | `invocation_microusd=100000` (USD 0.10); `run_microusd=1000000` (USD 1.00) |
| Observed token stop | 100,000; input, output, cache creation and cache read all included |
| Model retries | 0; no model-based repair or fallback |

The CLI uses one monotonic `max_run_seconds` window starting before configuration
and connection setup. Connection, paginated tool discovery, readiness probes and
supervision share that deadline; setup never grants supervision a fresh full
window. Preflight uses the same bounded setup window without claims or inference.
SIGINT/SIGTERM cancel network-only setup promptly. Owned readiness/model processes
are cancelled and awaited through reaping, not dropped; blocking local I/O and
process reaping prevent an unconditional OS-level return-by-deadline guarantee.

The MCP raw SSE event cap is 1,048,576 bytes. **JSON responses may already be
buffered and decoded by reqwest/rmcp before the serialized-size check**: it is not
a hard raw-HTTP allocation limit. Use controller `MemoryMax=512M` as illustrated.
An OOM kill is a recoverable crash, not a clean report or proof of zero spending.

Reservations are saved before invocation; every started call consumes a call slot.
Missing/invalid cost or any token class means unknown accounting, not zero. Nonzero
child exit and unknown accounting stop further inference; valid already-submitted
reports remain. A proposal that fails a check fails only its own item: its failed
report's summary reads `Proposal rejected: <site>: <check>` (for example
`Proposal rejected: finding 1 evidence 1: excerpt is not a substring of s1`), so
`maintenance_get_run` shows why without cluster access. The same reason is kept in
the journal's `rejection` field and logged once to stderr at warn level. It names
only indices, packet source ids and the check, never excerpt or source text.
The runner matches a model excerpt with whitespace tolerance: each run of ASCII
whitespace in it matches any nonempty run in the source, so a quote joined across
a hard line wrap still matches. The report stores the exact source span, so the
server's exact-substring rule above still holds.
Ambiguous/failed reservations are not refunded.
The CLI budget flag, USD reservations and observed token totals are stopping
thresholds, **not hard in-flight billing/token guarantees**. A request may overshoot;
provider-enforced quotas or a separately approved enforcing gateway are required
for strict ceilings. A new completed-run window is not a daily/account-wide cap.

### Private state, recovery and diagnosis

The Linux journal supports x86/x86_64/aarch64; other platforms fail closed. Use an
absolute, operator-owned state directory (0700), files 0600, no symlink components,
and trusted ancestors owned by root or that operator. Non-sticky group/world-writable
ancestors are rejected. The stable `lock` inode must never be unlinked or replaced
while a runner may hold it. A local lock is not distributed exclusion; server leases
remain authoritative across machines.

`journal.json` includes lease tokens, pinned evidence, pending reports, accounting
and exact mutation keys. It is sensitive despite containing only a SHA-256
fingerprint of the MCP credential. Keep encrypted/access-controlled backups of
private state while the service is stopped; preserve ownership and modes. A
canonical export does not back up this local journal. Do not copy state to another
endpoint/catalog/key or edit it to discard an inconvenient pending operation.

Recovery binds to the full normalized HTTPS endpoint (including path/query),
origin, catalog and credential fingerprint. Changed credentials/binding, corruption
or insecure permissions refuse recovery. Resolve deliberately with the operator
and server authority, not by deleting the journal and paying again. A pending claim,
renewal, submit or close replays its exact normalized payload and idempotency key.
Crash after `invocation_started` means potentially spent money: recover reports and
close without another model call in that run. Invocation count, reservations and
known usage survive restart; the recovery process has a fresh bounded local wall
window. New work/submission remains subject to the original server deadline/lease
authority. After exact pending replay, an authoritative terminal read abandons
unreported local work without inventing receipts. Expired unfinished running work
uses a journaled failure-close acknowledgment, not renewal or inference; that
recovery close does not require a live lease or unexpired server deadline. Only a
durable terminal run permits a later fresh run's spending window. Do not infer
permission to retry from an apparently idle process or a missing response.
The first stop reason remains immutable for reporting (for example, `transport`
and exit 4). Successful exact replay, run lookup and authority checks allow
acknowledged terminal cleanup without any new inference even after that historical
transport stop. A current permission, protocol or transport failure still blocks
cleanup; unresolved requests are never discarded or acknowledged locally by guess.

The owned CLI installs a global tracing subscriber **before runtime/SDK
activity**, regardless of `RUST_LOG`, with a fixed filter that passes only the
`okf_maintenance::rejection` warn line described above; everything else is dropped.
This removes the inherited rmcp logging hazard:
rmcp can trace full protocol payloads, source bodies and tokens. There is no verbose
dependency override. Library embedders must establish their own safe logging policy;
the library does not globally reconfigure its host. Never enable prompt/protocol
logs to diagnose a fixed error.

Use exit status, safe summary counts/reason, file ownership/modes, pinned version/help,
and separately authorized server-side permission/run inspection. Check `Config` /
`Contract` against documented fields, version and schemas; `Busy` against the owned
service; `Permission` against credentials/grants and private paths; runtime failures
against the durable recovery state. Inspect state only in a protected environment,
never paste it or child output into tickets. Fix cause before rerunning. A
transport/permission/protocol loss may deliberately leave work for recovery/expiry,
without remote cleanup or guessed lease authority.

### Offline evaluation and mandatory live gates

```sh
cargo run --bin okf-maintenance -- evaluate --cases tests/fixtures/investigator/corpus/evaluation.json
```

Evaluation requires no config, credentials, network or model subprocess. Input is
one version-1 JSON corpus, at most 16 MiB / 10,000 cases; see the
[fixture format and metric definitions](../tests/fixtures/investigator/corpus/README.md).
Each case has `id`, `provenance`, `annotation_author`, `review`, `packet`, raw
`proposal`, `expected` relation keys and per-key `admissibility`. Provider envelopes
are not corpus proposals. Unknown/duplicate DTO fields and invalid corpus shapes
fail closed. Predictions require the whole production proposal gate and exact
citations; relation keys are category plus sorted distinct evidence paths, not
assertion wording or revision numbers.

Output fields: `cases`, `true_positive`, `false_positive`, `false_negative`,
`rejected_citations`, `false_verification`, `rejected_verification`,
`excluded_unreviewed`, `precision`, `recall`. Precision is TP/(TP+FP), recall
TP/(TP+FN); undefined denominators are JSON null. Ambiguous, unreviewed or
incompletely annotated cases are excluded from both denominators. Rejected
proposals yield no predictions and leave expected keys unmatched. Citation and
forbidden-verification diagnostics include excluded cases. `false_verification`
counts accepted structured `SupportedCurrent` results (must remain zero), not a
natural-language detector. `rejected_verification` counts attempted forbidden
assessment/outcome fields.

The generated synthetic fixture has 11 cases, TP=4, FP=2, FN=3, one rejected
citation, zero false verification, two rejected verification attempts and two
excluded cases; precision 4/6, recall 4/7. These are parser/mapping specifications,
**not model-quality measurements or independent human review**. No real paid
response has been captured or verified by this implementation.

Before any scheduling, require **both separate gates**:

1. Independent human review of the synthetic corpus and its labels, covering
   valid/invalid links, similar-name distinct entities, copies, history versus
   contradictions, unverifiable aging and injected instructions. Generated
   `synthetic_specification` labels or merely setting `human_reviewed` do not
   satisfy this review. Choose and document acceptable quality criteria.
2. A **separately authorized one-primary live smoke run** against the exact pinned
   CLI/model, with both consents true, `batch_size=1`, `max_invocations=1`,
   `invocation_microusd=100000` and `run_microusd=100000`. Use approved sanitized
   source data and a fresh authorized state binding; do not overwrite a recoverable
   production journal. Arrange an explicitly approved sanitized envelope capture
   without enabling dependency logs or storing credentials/private source bodies.
   Confirm the target result has `type:"result"`, `subtype:"success"`,
   `is_error:false`, object `structured_output`, nonnegative `total_cost_usd`,
   and explicit integer input/output/cache-creation/cache-read usage fields.
   Human-review evidence, accounting, privacy and absence of canonical writes.
   The ordinary CLI intentionally does not export raw envelopes; any capture
   procedure is a separate operator authorization, not a debug flag here.

Version/help preflight, synthetic parser tests and generated labels satisfy neither
live gate. Leave the timer disabled when smoke evidence is absent or failed.
Accounts, keys, quotas and paid smoke remain operator work, not test-suite actions.

### Disabled systemd examples

[Service](../examples/maintenance/okf-maintenance.service) and
[timer](../examples/maintenance/okf-maintenance.timer) are repository examples only.
**Nothing installs or enables them.** They contain no `[Install]` targets and no
installation scripts. Both consent flags in the matching config remain false.

After authorization and both gates, the operator may provision a dedicated
`okf-maintenance` user/group, root-owned pinned executables under
`/opt/okf-maintenance`, and `/etc/okf-maintenance` owned by
`root:okf-maintenance` (0750). Keep `config.toml` owned by
`root:okf-maintenance` (0640) and `credentials.env` root-owned 0600, containing
only the two required environment assignments. The system service manager reads the protected
EnvironmentFile. Do not put the pinned model executable under a user home or rely
on the developer's PATH, repository tree or interactive credentials.

The service uses `UMask=0077`, `StateDirectory=okf-maintenance`,
`StateDirectoryMode=0700`, explicit `/var/lib/okf-maintenance` writable state,
`PrivateTmp`, `NoNewPrivileges`, `ProtectSystem=strict`, `ProtectHome=true`, and
`MemoryMax=512M`. For `Type=oneshot`, `TimeoutStartSec=1800` is the execution cap
(not `RuntimeMaxSec`); matching `max_run_seconds=1740` leaves 60 seconds for orderly
shutdown. `KillMode=control-group` handles the service's remaining descendants on
forced stop. This is defense in depth, not an egress firewall or billing guarantee.

The timer is daily with a randomized delay of up to one hour, persistent catch-up,
and `Unit=okf-maintenance.service`. Persistent catch-up may run soon after boot;
authorize that behavior too. An operator who elects to install the reviewed files
can reload systemd and explicitly start the timer. To persist activation across
boots, they must deliberately add an appropriate install target or wants-link;
this repository does not do so. Do not start it merely because full tests passed.

## Web console

The server also serves a browser console and the JSON API it runs on, on
the same port as `/mcp`:

| Route | Key | Does |
|---|---|---|
| `GET /` | none | Redirects (303) to `/ui/` |
| `GET /ui`, `GET /ui/` | none | `/ui` redirects (308) to `/ui/`, which serves the console's shell |
| `GET /ui/*` | none | The console's static files, embedded in the binary at build time; an unknown file is 404 |
| `GET /ui/manifest.json` | none | `{"pages": [...]}`: the console pages this build has |
| `GET /ui/brand.json` | none | `{"product_name", "support_contact", "feedback_url", "docs_url", "billing_url", "feedback_form"}`: the [branding settings](#settings) (`null` when unset), which the console shows before anyone signs in, and `feedback_form`, `{"disclosure", "max_message_chars", "kinds"}` with a `[feedback]` table, else `null` (never the form id or the API key). `onboarding_url` is not in it |
| `GET /read`, `GET /read/` | none | The [mobile reader](reference/reader.md), an installable read-only app for phones: `/read` redirects (308) to `/read/`. `/read/` and `/read/manifest.webmanifest` are rendered per request with the current `product_name`; signed out, the page stays and shows a Sign in button rather than redirecting. Its files (`GET /read/*`, `GET /read/sw.js`, and `GET /read/shell.json`, the worker's precache listing with a SHA-256 per file) are embedded in the binary, carry the console's security headers and hold no catalog data; the app reads `/api/v1` with the console's session cookie |
| `/api/v1/*` | bearer key or console session | The JSON API; each endpoint checks the level it needs |
| `GET /signin`, `POST /signin` | none | The sign-in page and its password form ([Signing in and self-service](#signing-in-and-self-service)). Without `public_url` the page offers only the pasted key and the POST is 404; with `password_sign_in` off the POST is 404 |
| `GET /signin/forgot`, `POST /signin/forgot` | none | Ask for a reset link; only with `public_url` set |
| `GET /account/setup`, `POST /account/setup` | setup link (`?token=`) | Choose a password (or, with the page's script, register a passkey); signs in, then shows a welcome page linking to `onboarding_url` when it is set and the setup does not continue an OAuth authorization, else redirects (303) to `return_to` |
| `GET /account/reset`, `POST /account/reset` | reset link (`?token=`) | Set a new password; signs every session out |
| `POST /auth/passkey/signin/start`, `POST /auth/passkey/signin/finish` | none | Passkey sign-in (discoverable credentials); only while passkeys are on. The finish answers `{redirect}` with the session cookie, or 401 |
| `POST /auth/passkey/setup/start`, `POST /auth/passkey/setup/finish` | setup link (in the body) | Register a passkey from the setup page, use the link and sign in; 422 for a credential that does not verify |
| `GET /auth/oidc/{provider}/start`, `GET /auth/oidc/{provider}/callback` | none | OIDC sign-in, and the end of linking an account; only while a provider is configured |

The console's files hold no catalog data, so they need no key. Every
`/ui/` response carries:

- `Content-Security-Policy: default-src 'self'; script-src 'self'; style-src 'self' https://fonts.googleapis.com; font-src 'self' https://fonts.gstatic.com; img-src 'self' data:; connect-src 'self'; frame-ancestors 'none'; base-uri 'none'; form-action 'self'`
- `X-Content-Type-Options: nosniff`
- `Referrer-Policy: no-referrer`
- `Cache-Control: no-cache` on each file (the browser revalidates on every load)

Google Fonts (`fonts.googleapis.com` for the stylesheet, `fonts.gstatic.com`
for the fonts) is the only third-party origin the console loads from. The
reader at `/read/` serves its own fonts, hence `font-src 'self'`. Behind
a proxy that adds its own CSP, keep these sources or the fonts fall back to
the system ones.

Every response the HTTP server sends, whatever its status (pages, the API,
`/mcp`, redirects, 401s, 404s, static files), also carries
`X-Robots-Tag: noindex, nofollow`, set by one layer on the whole router, so
no page of the server is indexed by a search engine. There is no `robots.txt`
`Disallow`: a crawler must fetch a page to see it is marked. The `/metrics`
listener is separate, internal and never routed by the ingress, so it does not
send it.

Every `/api/v1` response carries `Cache-Control: no-store` and
`X-Content-Type-Options: nosniff`. The API uses the same bearer keys and the
same errors as the other routes: 401 without a valid key (plain text, with
`WWW-Authenticate: Bearer`), and otherwise `{"error": ..., "message": ...}`
with the statuses in the README's "Routes". An unknown `/api/v1` path is 404,
key or not.

An internal error (`Database`, `Migrate`, `Io` or `Config`: every 500 from a
route) answers with the generic message ("internal error; the server log has
the details") and one more field, `trace_id`: the OpenTelemetry trace id of the
request's span, the one the exported `request failed` event and the stderr line
carry (with `telemetry.stderr.format = "json"`). A user can quote it in a
report and the operator can open that trace. It is 32 hex digits, left out
when the request has no valid trace (telemetry off, or the span filtered out)
and never sent as zeros. The failed credential lookup of the bearer-key middleware (MCP and archive
routes) answers the same way. The OAuth routes' 500s carry it too, in their own
shapes (see the OAuth section). No other error carries it, and no error carries the
detail. The console shows it in its error toast.

Every response of a routed request also carries `X-Trace-Id`, the same 32 hex
digits, so a user can quote the id of a slow or wrong-but-200 request, not
only a failed one; on an error answer it equals the body's `trace_id`. Like
the body field it is left off, never zeros, when the request has no valid
trace (telemetry off, or the span filtered out). It is left off embedded
static files (the console's and reader's scripts, styles, fonts, the
reader's pages and worker), whose bytes are the same for every caller, so a
cache or CDN cannot pin one caller's id on them, and off a request that
matches no route (an unknown path), which has no request span. The OAuth
discovery documents, the only cross-origin readable responses, send
`Access-Control-Expose-Headers: x-trace-id`, so a browser client's script can
read it; the console is same-origin and needs nothing.

Every answer a route gives a signed-in caller also carries `X-OKF-Account`:
16 hex digits of SHA-256 of a prefixed id (the user's id for a console
session, the key's id for a bearer key or grant). It is stable per account,
differs between accounts, and names no user, key or secret; the mobile
reader's service worker uses it to keep its copies to one account. The
refusals given before a route runs (the 401 for a missing or bad
credential, the 403 for a console write without `X-OKF-CSRF`, a 500 from
the credential lookup) and the 404 for an unknown path do not carry it.

The sign-in pages use the console's CSP (their scripts are static files
under `/ui/js/signin/`), with `Cache-Control: no-store`. Sign-out is
`DELETE /api/v1/session`, in the identity table below; there is no sign-out
page.

You sign in to the console with your account ("Sign in with your account";
see [Signing in and self-service](#signing-in-and-self-service)), or by
pasting an API key. A pasted key is kept in that browser's `localStorage` (in
memory only, where storage is blocked) until you sign out, so sign out on a
shared machine; anyone who can run script in that browser profile can read
it. An account session lives in an `HttpOnly` cookie no script can read. At
start the console asks for a session first: a live session wins over a
stored key, which is then dropped, so the console uses one credential at a
time. A refused key (revoked, say) or an ended session signs the browser out
at its next request. Signed in with an account, the console shows "Your
account" (My keys, My grants, Account), and Administer only to an admin
user; hiding it is a convenience, since the API refuses a member's session on
admin routes (403).

The sidebar begins with a search box (Enter shows the results in the main
panel, at the linkable `#/search/<query>`; a blank box sends nothing), then
the groups **Catalogs** (Browse, Recent changes, Import & export, and the
catalog selector), **Records**, Administer and Your account. **Records**
holds Findings, Runs and Changesets, then Audit, for an admin that reaches
every catalog and for anyone signed in with an account (every user owns
at least its personal team; an owner picks one of the teams they own), and
Unlocks, for those and for any admin credential (a scoped admin sees the
unlocks of the catalogs it administers). **Your account** also holds the
Catalogs link, which shows to an admin credential and to anyone signed in
with an account: it lists the catalogs the user reaches, and a catalog's
drawer for a member holds only the Maintenance switch, enabled for an owner
of the catalog's owner team. Where a
locked catalog is refused (`UnlockRequired`), the console offers Unlock,
to an admin credential whose user is an admin user (`whoami`'s
`user_admin`), with a short form for the category,
reason and hours.

### API reference

The API has 95 endpoints (a method and path each). The identity endpoints
are below (3), and each area's endpoints are in its own subsection:
[API keys](#api-keys) (7),
[OAuth sign-in codes](#oauth-sign-in-codes) (4),
[Users and teams](#users-and-teams-api) (21),
[Your account](#your-account-api) (19),
[Settings and mail](#settings-and-mail-api) (7),
[Audit](#audit-api) (2), [Unlocks](#unlocks-api) (3),
[Health](#health) (2), [Browse and search](#browse-and-search) (8),
[Catalogs and import/export](#catalogs-and-importexport) (10),
[Findings and runs](#findings-and-runs) (6) and [Changesets](#changesets) (3).
Each area's tests but Your account's check every endpoint against the
access matrix (no key, read, write, admin, and an admin key that does not
reach the catalog); Your account's check that every route refuses a bearer
key and acts only on the signed-in user's own rows. Every endpoint also takes a console session, at the signed-in user's levels
([Sessions](#sessions)), with `X-OKF-CSRF` on writes.

| Method and path | Key | Does |
|---|---|---|
| `GET /api/v1/whoami` | any | The calling credential: `key_id`, `name`, `actor`, `access`, `maintenance_report`, `all_catalogs`, `catalogs` (one page of the catalogs it reaches, sorted; see [Catalog listings are paginated](#catalog-listings-are-paginated)) with `next`, `search_fanout_max` (the most catalogs one search may name), `levels` (its level on each catalog of the page, `{catalog: "read"\|"write"\|"admin"}`: for a console session the ceiling's level there, resolved per catalog), `default_catalog`, `kind` (`key`, `oauth` or `session`), `client_name` (a grant's client), `user` (the user it belongs to, `null` when unassigned), `teams` (that user's teams, sorted) and `user_admin` (whether that user is an admin user, `false` without one; the console offers Unlock only when it is true). A session's `key_id` is `session:<user>`, its `name` `null` and its `actor` the user's name. Never a key, token or hash |
| `GET /api/v1/session` | console session | `{user, email, admin, csrf, expires_at}`: the signed-in user's name, email and admin flag, the CSRF token writes must send as `X-OKF-CSRF`, and when the session ends at the latest. It holds no ceiling (issue #736): the session's level on each catalog is in `GET /api/v1/whoami`'s `levels`, and `GET /api/v1/me/ceiling` pages the ceiling. 401 without a usable session (a dead cookie is cleared); a bearer credential gets 403 |
| `DELETE /api/v1/session` | console session | Signs out: deletes the session and clears the cookie; 204. Needs `X-OKF-CSRF` (403 without); a bearer credential gets 403 |

```sh
curl -s -H "Authorization: Bearer $OKF_MCP_KEY" https://okf-mcp.example.com/api/v1/whoami
```

### API keys

The API keys page (`/ui/#/keys`) and its API manage keys without a shell on
the server, as `okf-mcp key ...` does. Both are for admin keys only: a read
or write key gets 403 from the API, and the console hides the page.

An admin key manages only the keys inside its reach. One that reaches every
catalog manages every key. One scoped to catalogs manages the keys whose
every catalog it reaches, and no all-catalogs key; any other key answers 404,
as an unknown id does. A new key or a grant may name only catalogs the
caller reaches, and `all` needs a caller that reaches every catalog. Paths
take a key's immutable id, never its name.

The secret is in the `POST /api/v1/keys` response only: it is not stored and
cannot be shown again, and no other response holds a secret or a hash. A
revoke, grant, ungrant or reporting change takes effect on the key's next
request on the replica that took the change, and on the others when the
change message reaches them (a lost message is bounded by `credential_ttl`). Key changes made here record
no audit actor, as with the CLI.

Every key belongs to a user ([Users and teams](#users-and-teams)). New key
has a required user picker, the list has a User column (`unassigned` for a
key from before users existed), and each key's or grant's drawer, revoked
ones included, has Assign (unassigned) or Change user.

| Method and path | Key | Does |
|---|---|---|
| `GET /api/v1/keys` | admin | The keys the caller manages, revoked ones included: `id`, `name`, `actor`, `access`, `kind` (`key` or `oauth`), `client_name`, `maintenance_report`, `all_catalogs`, `catalogs`, `catalog_access` (each catalog's level), `default_catalog`, `created_at`, `revoked_at`, `expires_at` (`null`: never), `self_issued`, `user` (`null` when unassigned). Never a secret or hash |
| `POST /api/v1/keys` | admin | Body `{name, user, access: "read"\|"write"\|"admin", all, catalogs, default_catalog, actor, maintenance_report, provisioner}` (`user` required; `actor` defaults to `name`). `access` is required unless `provisioner: true`; a provisioner needs an every-catalog admin and refuses any supplied access, all, catalogs, default or reporting option (409). The returned key includes `provisioner`. Answers `{key, secret}`. 400 for a bad `access`, 403 for `all` from a scoped caller, 404 for an unreached catalog or an unknown user, 409 for a bad or taken name, no catalogs, or `all` with `catalogs`, 422 for an unknown field or a missing `user` |
| `PATCH /api/v1/keys/{id}` | admin | Body `{maintenance_report: bool}`. 409 for a revoked key, or for enabling reporting on an OAuth grant |
| `PUT /api/v1/keys/{id}/user` | admin | Body `{user}`. Gives the key or OAuth grant (live or revoked) that user, or changes it; access is unchanged. Answers its view, and its next request runs as that user. 404 for an unmanaged key or an unknown user, 422 without `user` |
| `POST /api/v1/keys/{id}/revoke` | admin | Refused from the key's next request on. 409 if already revoked |
| `PUT /api/v1/keys/{id}/catalogs/{catalog}` | admin | Grants a catalog. 404 for an unreached catalog or an unmanaged key; 409 for a revoked or all-catalogs key |
| `DELETE /api/v1/keys/{id}/catalogs/{catalog}` | admin | Removes a grant, and the default if it was that catalog. Errors as for the grant |

```sh
curl -fsS -H "Authorization: Bearer $OKF_MCP_KEY" -H 'Content-Type: application/json' \
    -d '{"name":"ci-reader","access":"read","catalogs":["main"],"default_catalog":"main"}' \
    https://okf-mcp.example.com/api/v1/keys
```

### OAuth sign-in codes

`okf-mcp oauth ...` and `/api/v1/oauth/codes` mint and manage the one-time
codes the OAuth consent page asks for. The reach rule is the one for keys:
a scoped admin manages only codes whose every catalog it reaches, and no
all-catalogs code. The code itself is in the create response (and the CLI's
stdout) only; only its SHA-256 is stored. Codes are read from the database on
every use, so minting or revoking one takes effect at once. Grants, the keys
codes become, are listed and revoked with the other keys: their views carry
`kind: "oauth"` and `client_name`.

The API keys page (`/ui/#/keys`) has a "Sign-in codes" section below the
keys: "New sign-in code" mints one and shows it once, the list shows live
codes (tick "Show redeemed, expired and revoked codes" for the rest), and a
code's drawer revokes it until someone redeems it. In the key list, an OAuth
grant is marked `OAuth` with its client's name. Its drawer revokes it or
changes its catalogs, and has no reporting switch: a grant never reports.

The `okf-mcp oauth` commands are in "OAuth connectors"
([Managing codes and grants](#managing-codes-and-grants)), with how to turn
OAuth on and connect a client.

| Method and path | Key | Does |
|---|---|---|
| `GET /api/v1/oauth/codes` | admin | The codes the caller manages, newest first: `id`, `name`, `actor`, `access`, `all_catalogs`, `catalogs`, `default_catalog`, `state`, `created_at`, `expires_at`, `redeemed_at`, `grant_id`, `revoked_at`, `user` (`null` when unassigned). Never a code or hash |
| `POST /api/v1/oauth/codes` | admin | Body `{name, user, actor, access: "read"\|"write", all, catalogs, default_catalog, expires_in}` (`user` required; `actor` optional, and absent or blank means `name`; `expires_in` like `2h` or `3d`, default 24h). Answers `{code, secret}`. 400 for a bad `access` or `expires_in`, 403 for `all` from a scoped caller, 404 for an unreached catalog or an unknown user, 409 for `admin`, a bad name, no catalogs, `all` with `catalogs` or too long an expiry, 422 for an unknown field or a missing `user` |
| `DELETE /api/v1/oauth/codes/{id}` | admin | Revokes an unredeemed code; answers its view. 404 for an unmanaged code, 409 if redeemed or already revoked |
| `PUT /api/v1/oauth/codes/{id}/user` | admin | Body `{user}`. Gives the code (in any state) that user; answers its view. A grant the code already became keeps its own user (`PUT /api/v1/keys/{id}/user`). 404 for an unmanaged code or an unknown user, 422 without `user` |

```sh
curl -fsS -H "Authorization: Bearer $OKF_MCP_KEY" -H 'Content-Type: application/json' \
    -d '{"name":"laptop","user":"alice","access":"read","catalogs":["main"]}' \
    https://okf-mcp.example.com/api/v1/oauth/codes
```

### Users and teams API

The routes behind `okf-mcp user ...`, `team ...` and `unassigned`
([Users and teams](#users-and-teams) has the model and the
[parity table](#cli-api-and-console-parity)). Writes and `GET /unassigned`
need an admin key that reaches every catalog (403 for any other key), since
users and teams are not catalog-scoped; reads need any admin key (403 below
admin). Every change takes effect at once. A refusal under the
[deletion rules](#deletion-rules) is 409, and an unknown user or team 404
(`UnknownName`).

| Method and path | Key | Does |
|---|---|---|
| `GET /api/v1/users` | admin | `?email=` (optional) answers only the user with that email, in any letter case: zero or one, in the same view; text that cannot be an address finds none, and an unknown parameter is 400. `{users}`, by name: `name`, `display_name`, `label` (what people are shown: the display name, else the email's local part, else "Unnamed user"), `created_at`, `credentials` (keys, grants and codes naming it, whoever manages them), `teams`, `email` (`null` when unset), `is_admin`, `has_password` (never the password or its hash) |
| `POST /api/v1/users` | admin, every catalog, or a provisioner | Body `{name, display_name, email}` (`display_name` optional: trimmed, at most 80 characters, no control characters or line breaks; the personal team gets it too; `email` optional, `null` or absent leaves it unset). Creates the user and its personal team, and sets the email in the same transaction, so a refused address creates nothing; 201 with the user. 409 for an invalid or taken name, or `Another user has this email` for an address another user has (in any letter case): the same status and body whoever holds it, an admin included, and it names no account. 422 without `name`, for a refused `display_name` or for an address that is not one. The email needs no more than `PUT /api/v1/users/{name}/email` does |
| `GET /api/v1/users/{name}` | admin | `{user, keys, codes, entitlements}`: the user, the keys, grants and codes naming it that the caller manages, and its entitlements (`[{catalog, access, created_at}]`, by catalog) |
| `PATCH /api/v1/users/{name}` | admin, every catalog | Body `{display_name}` (`deny_unknown_fields`; empty clears it). Answers the user. 422 (`limit: "display_name"`) for one over 80 characters or holding a control character or line break. The CLI is `user update` |
| `DELETE /api/v1/users/{name}` | admin, every catalog | Deletes the user with its credentials, sessions and personal team (see "Deletion rules"); 204. Optional query `?unless_shared_member=true`: delete nothing and answer 409 while the user is a member, in any role, of any team other than its own personal team (suspended teams included); the check runs under the delete's own locks, so a membership committed concurrently is never removed silently. A client tells this 409 from the others by the `message`: it starts `the user is a member of shared team(s): ` and then the quoted team names, while every other deletion refusal starts `user "<name>" cannot be deleted`. It wins over every other refusal, the last-owner one included. Optional query `?unless_has_email=true`: delete nothing and answer 409 while the user has an email address; the message is `the user has an email address`, it names no address or other user, and the check runs under the user's row lock, which `PUT /api/v1/users/{name}/email` takes too, so an email set concurrently is never lost with the account. The shared-team refusal still comes first when both apply, and this one wins over the others. `false` or no parameter: unchanged. 400 for another query parameter or a non-boolean value. Otherwise 409 for a personal team that owns catalogs, the last owner of a shared team, or the last admin user |
| `PUT /api/v1/users/{name}/admin` | admin, every catalog | Body `{admin: bool}`. Answers `{name, previous, admin}`. A change signs the user out of every console session (they sign in again with the new flag). 409 for clearing the last admin user (only the CLI may), 422 without `admin` |
| `PUT /api/v1/users/{name}/email` | admin, every catalog | Body `{email}` (`null` clears; the field is required). Answers the user. A change ends the user's outstanding emailed links. 409 "Another user has this email" (in any letter case), 422 for an address that is not one or without `email` |
| `POST /api/v1/users/{name}/invite` | admin, every catalog | Queues a setup email with a single-use link (replacing any older one) to the user's email; 202 `{name, email}`. 409 without an email or without `public_url` |
| `PUT /api/v1/users/{name}/entitlements/{catalog}` | admin, every catalog; or an owner of the catalog's shared team, signed in to the console, for a member of that team | Body `{access: "read"\|"write"}`. Adds the entitlement or changes its level; answers `{name, entitlements}` (for an owner, only the entitlement on this catalog). 400 for another level, 404 for an unknown user or catalog asked by an admin. 403 for an owner's key or grant, a non-owner, a personal team's owner, and, for an owner, one identical refusal for an unknown user and a user outside the team |
| `DELETE /api/v1/users/{name}/entitlements/{catalog}` | admin, every catalog; or an owner, as `PUT` | Removes it (none given: no change); answers `{name, entitlements}` (an owner: this catalog's only). The same refusals as `PUT` |
| `GET /api/v1/users/{name}/sessions` | admin | `{sessions}`, most recently used first: `id_prefix`, `created_at`, `last_seen_at`, `ip`, `user_agent`, `current` (always `false` here). Never a session id, token or hash |
| `DELETE /api/v1/users/{name}/sessions` | admin, every catalog | Signs the user out of every console session (keys and grants keep working); answers `{name, revoked}` |
| `DELETE /api/v1/users/{name}/sessions/{id_prefix}` | admin, every catalog | Signs one session out; 204. 404 for a malformed prefix or one that matches none of the user's sessions, 409 for one that matches several (none is ended) |
| `GET /api/v1/users/{name}/sign-ins` | admin | `{sign_ins}`, newest first (at most 50): `method`, `outcome`, `ip`, `at` |
| `GET /api/v1/teams` | admin; a provisioner | `{teams}`, by name: `name`, `display_name`, `label` (the display name, else "Unnamed team"; a personal team's is "<its user's label>'s team"), `personal_of` (the user whose personal team it is, or `null`), `created_at`, `members` (`[{user, role}]`, by user), `catalogs` (the names it owns), `suspended_at` (when it was suspended, `null` while active) and `suspension_notice` (`null` when there is none; see [Suspending a team](#suspending-a-team)) |
| `POST /api/v1/teams` | admin, every catalog | Body `{name, display_name, owner}` (`owner` required: the first owner's user name; `display_name` optional, checked as for users). 201 with the team. 404 for an unknown owner, 409 for an invalid or taken name, 422 without `owner` or for a refused `display_name` |
| `GET /api/v1/teams/{name}` | admin | The team, as in the list |
| `PATCH /api/v1/teams/{name}` | admin, every catalog; an owner of the team signed in to the console | Body `{display_name}`. Answers the team. 409 for a personal team (its label follows its user). The CLI is `team update` |
| `DELETE /api/v1/teams/{name}` | admin, every catalog | Deletes a shared team that owns no catalog; 204. 409 for a personal team or one that owns a catalog |
| `PUT /api/v1/teams/{name}/members/{user}` | admin, every catalog | Body `{role: "owner"\|"member"}`. Adds the user, or gives a member that role; answers `{change: "added"\|"role_changed"\|"unchanged", team}`. 409 for demoting the last owner, 422 for a missing or unknown `role` |
| `DELETE /api/v1/teams/{name}/members/{user}` | admin, every catalog | Removes a member; answers the team. 409 for a non-member, the last owner, or a personal team's user |
| `PUT /api/v1/teams/{name}/suspension` | admin, every catalog | Body `{suspended: bool, notice?}`. `true` suspends the team, or replaces the notice of a suspended one (its `suspended_at` is kept); `false` lifts it (no change when active). Answers the team. Its catalogs then refuse every non-admin credential with 403 `Suspended`, except export. 404 for an unknown or malformed team name, 409 for a notice over 2000 characters or holding a NUL, 422 without `suspended` or with an unknown field |
| `GET /api/v1/unassigned` | admin, every catalog | `{keys, codes, catalogs}`: the keys and grants (live and revoked) and codes with no user, as in their own lists, and the names of the catalogs with no owner |

```sh
curl -fsS -H "Authorization: Bearer $OKF_MCP_KEY" -H 'Content-Type: application/json' \
    -d '{"name":"family","owner":"alice"}' \
    https://okf-mcp.example.com/api/v1/teams
```

### Your account API

The routes behind My keys, My grants and Account, for a person signed in
with an account (a console session). A bearer key or OAuth token gets 403
from every one but `GET /api/v1/me`, since a key must not mint keys for
itself. Each acts only on the signed-in user's own keys, grants, passkeys,
identities and sessions: anyone else's id answers 404, as an unknown one
does. Limits are in [Signing in and self-service](#signing-in-and-self-service).

| Method and path | Key | Does |
|---|---|---|
| `GET /api/v1/me` | console session, or a key that belongs to a user | `{user, display_name, email, admin, has_password, teams, owned_teams, ceiling_access, session, password_sign_in, limits}`: `owned_teams` are the teams the user owns, sorted, its personal team included (whose audit log its console session can read); `ceiling_access` is the ceiling's highest level (`read` or `write`, `null` when it reaches no catalog; the ceiling itself is paged by `GET /api/v1/me/ceiling`), `session` says whether a console session asked, and `limits` is `{max_keys_per_user, max_key_lifetime_secs}` (`null` for no lifetime limit). 404 for a key without a user |
| `GET /api/v1/me/ceiling` | console session, or a key that belongs to a user | `?prefix=&after=&limit=`: one page of the user's ceiling, `{ceiling: [{catalog, access}], next}`, by catalog name in byte order; `prefix` (trimmed, lowercased) keeps the names that start with it, and one no name can start with finds nothing; `next` is present when more follow (send it back as `after`); `limit` is 100 by default, at most 1000. Each page is one bounded read: no request reads the whole ceiling. 400 for a malformed `after`, 404 for a key without a user |
| `GET /api/v1/me/keys` | console session | `{keys, ceiling}`: the user's keys, revoked and expired ones included, as `GET /api/v1/keys` shows them (with `catalog_access`, `expires_at`, `self_issued`), and the ceiling on the catalogs its live self-issued keys name (`{catalog: "read"\|"write"}`; a catalog left out is outside it) |
| `POST /api/v1/me/keys` | console session | Body `{name, catalogs: [{catalog, access}], default_catalog, actor, expires_at}` (`actor` defaults to `name`; `expires_at` RFC 3339, and absent means the lifetime limit, or none). Answers `{key, secret}`, the secret once. 400 for no catalogs, a repeated catalog, an unknown level, or a malformed or past expiry; 409 for a bad or taken name or a default that is not one of `catalogs`; 422 above the ceiling (`admin` included), past the key limit or the lifetime limit (`limit` is `ceiling`, `max_keys_per_user` or `max_key_lifetime`), or for an unknown field such as `all`, `user` or `maintenance_report` |
| `DELETE /api/v1/me` | console session | Deletes your own account ([Deleting your own account](#deleting-your-own-account)). Body `{password}`, `{passkey}` (the browser's answer to `POST /api/v1/me/passkeys/confirm/start`) or `{oidc_reauth: true}` (the provider re-authentication `POST /api/v1/me/identities/{provider}/reauth` and its callback finished for this session, single use, 5 minutes; its ID token needed an `auth_time` within 5 minutes of the callback), exactly one. 204, clearing the session cookie and any `okf_passkey` ceremony cookie, whichever proof was used. 400 for none or more than one; 403 for a key or token, a wrong password, a locked account, a passkey that does not verify, another session's ceremony or re-authentication, `"error": "ReauthStale"` when the provider's `auth_time` was missing or not fresh (the message tells the person to sign out of the provider, sign back in and try again), or a password, passkey or linked identity changed meanwhile (never 401, which the console reads as signed out); 401 once this session has ended; 404 for a passkey while passkeys are off, or a re-authentication while OIDC is off; 409 for a refusal of the deletion rules (the last admin user, a personal team owning a catalog, the only owner of a shared team); 410 for an expired or used passkey ceremony, or a provider re-authentication that was never finished, expired or was used |
| `PATCH /api/v1/me/keys/{id}` | console session | Body `{name}`. Renames a live key; answers its view. 409 for a bad or taken name or a revoked key |
| `DELETE /api/v1/me/keys/{id}` | console session | Revokes it (the row stays, revoked); answers its view. 409 if already revoked |
| `GET /api/v1/me/grants` | console session | `{grants, ceiling}`: the user's OAuth grants, revoked ones included, as key views (`client_name`, `catalog_access`, `self_issued`, `revoked_at`, ...), and the ceiling on the catalogs its live self-issued grants name, as for `/me/keys` |
| `DELETE /api/v1/me/grants/{id}` | console session | Revokes the grant and every token it holds; its client is refused from its next request. Answers its view; 409 if already revoked |
| `PUT /api/v1/me/password` | console session | Body `{current_password, new_password, keep_sessions}` (`current_password` required while the user has a password). 204. 403 for a wrong current password, a locked account or a password changed meanwhile (never 401, which the console would read as signed out); 401 once this session has ended; 422 for the policy (`limit: "password"`). Signs the other sessions out unless `keep_sessions`, and queues the notice, in the same transaction |
| `GET /api/v1/me/passkeys` | console session | `{passkeys}`, oldest first: `id` (unpadded base64url), `name`, `created_at`, `last_used_at` |
| `POST /api/v1/me/passkeys/register/start` | console session | Body `{name, keep_sessions}`. The WebAuthn creation options; the ceremony is bound to a 5-minute `okf_passkey` cookie and to this session. 404 while passkeys are off, 422 for a bad name |
| `POST /api/v1/me/passkeys/register/finish` | console session | Body: the browser's credential. Stores the passkey; 201 `{id, name}`. Signs the other sessions out unless the start said `keep_sessions`. 403 for another session's ceremony, 409 for a passkey already registered, 410 for an expired ceremony, 422 for a credential that does not verify (`limit: "passkey"`), 401 only once the session has ended |
| `POST /api/v1/me/passkeys/confirm/start` | console session | The WebAuthn request options for re-authenticating with one of your own passkeys before `DELETE /api/v1/me`; the ceremony is bound to a 5-minute `okf_passkey` cookie and to this session, and replaces any other ceremony the session holds. 404 while passkeys are off, 409 when you have no passkey |
| `PATCH /api/v1/me/passkeys/{id}` | console session | Body `{name}`. Renames it; 204. 422 for an empty, over-64-character or control-character name (`limit: "passkey_name"`) |
| `DELETE /api/v1/me/passkeys/{id}` | console session | Removes it; 204. 409 for the last sign-in method |
| `GET /api/v1/me/identities` | console session | `{identities, providers}`: the linked identities (`provider`, `email_at_link`, `created_at`, `last_used_at`; never the subject) and the configured providers (`id`, `label`) |
| `POST /api/v1/me/identities/{provider}/link` | console session | Body `{keep_sessions}`. Starts linking: answers `{url}`, the provider's sign-in URL (the flow cookie is set); the callback links the identity and returns to `/ui/#/account`. 404 for an unknown provider or with no providers, 429 past the per-user allowance, 503 when the provider cannot be reached |
| `POST /api/v1/me/identities/{provider}/reauth` | console session | Starts re-authenticating at the provider where you have an identity linked, to delete your account: answers `{url}`, the provider's sign-in URL with `prompt=login` and `max_age=0` (the flow cookie is set). The callback verifies the ID token (it must carry an `auth_time` within 5 minutes, and the subject you linked), keeps a single-use proof for this session for 5 minutes and returns to `/ui/#/account`; it never signs in or links. 404 for an unknown provider, with OIDC off, or when you have no identity linked there; 429 past the per-user allowance shared with link starts, 503 when the provider cannot be reached |
| `GET /api/v1/me/identities/{provider}/reauth` | console session | Read-only status of this session's provider re-authentication for deleting your account: `{"state": "none"}`, `{"state": "ready"}` (a fresh proof waits) or `{"state": "stale"}` (the provider did not vouch for a fresh sign-in), and nothing else. Never takes or extends the proof; the same rule as `DELETE /api/v1/me`. 403 for a key, a grant or no console session; 404 for an unknown provider or with OIDC off |
| `DELETE /api/v1/me/identities/{provider}` | console session | Unlinks it; 204. 404 when none is linked there, 409 for the last sign-in method |
| `GET /api/v1/me/sessions` | console session | `{sessions}`, most recently used first: `id_prefix` (12 hex characters, never the id), `created_at`, `last_seen_at`, `ip`, `user_agent`, `current` (this browser's) |
| `DELETE /api/v1/me/sessions/{id_prefix}` | console session | Signs one session out, this browser's included; 204. 404 for a malformed prefix or one none of the user's sessions has; 409 for one several share (none is ended) |
| `GET /api/v1/me/sign-ins` | console session | `{sign_ins}`, newest first (at most 50): `method`, `outcome`, `ip`, `at` |
| `POST /api/v1/feedback` | console session | The console's feedback form ([Feedback](#feedback)). Body `{message, kind, rating}` (`kind` defaults to `other`, `rating` 1 to 5 or absent; `from`, `tool` or any other field is a 422). 201 `{sent: true, id}`. 404 without a `[feedback]` table; 403 for a key or token; 409 (`Refused`) for a blank message, a NUL, an unknown kind or a rating outside 1 to 5; 422 for a message over 4,000 characters (`limit: "feedback_message"`) or past `max_per_hour` for this user (`limit: "feedback_rate"`) |

### Settings and mail API

The routes behind `okf-mcp settings ...` and `okf-mcp mail ...`. Reads need
any admin key; changes need an admin key that reaches every catalog (403 for
any other key). An admin user's console session counts as an admin that
reaches every catalog.

| Method and path | Key | Does |
|---|---|---|
| `GET /api/v1/settings` | admin | `{settings, oidc, mail}`: each setting's `key`, `value` (the effective value, as text), `overridden`, `updated_at`, `updated_by`; the OIDC providers' `id`, `label`, `issuer`; the mail `provider` and `from`. Never a secret |
| `PUT /api/v1/settings/{key}` | admin, every catalog | Body `{value}` (a string, as on the command line). Answers the setting. 404 for an unknown key, 422 for a value the setting refuses or without `value` |
| `DELETE /api/v1/settings/{key}` | admin, every catalog | Drops the override; answers the setting at its default. 404 for an unknown key |
| `POST /api/v1/mail/test` | admin, every catalog | Body `{to}`. Queues the test email; 202 `{to}`. 422 for an address that is not one |
| `GET /api/v1/mail/queue` | admin | `{mail}`: every mail not yet sent, oldest due first: `id`, `to`, `template`, `status` (`pending`, `sending`, `retrying` or `failed`), `attempts`, `last_error`, `run_at`. Never a body |
| `POST /api/v1/mail/queue/{id}/retry` | admin, every catalog | Sends failed mail again now, with a new 24-hour retry window; 204. 404 for an unknown id, 409 for mail that has not failed |
| `DELETE /api/v1/mail/queue/{id}` | admin, every catalog | Deletes mail that is not being sent right now; 204. 404 for an unknown id, 409 while a worker is sending it |

### Catalog stores API

The routes behind `okf-mcp stores ...`. Reading needs any admin key; every
change needs an admin key that reaches every catalog (403 for any other key,
a provisioner key included). A change is recorded in the [audit
log](#audit-log) in its own transaction and sent to every replica, which
drops its pool for the changed store. None of these moves a catalog: a
catalog's store is chosen when it is created (its owner team's pin, else the
default store) and never changes.

| Method and path | Key | Does |
|---|---|---|
| `GET /api/v1/stores` | admin | `{stores}` in name order: `name`, `location` (`account-database` or `host`), `host`, `port`, `database`, `credential_kind`, `credential_ref` (a variable name, secret ARN or IAM identity, never the credential; all `null` for `main`), `created_at`, `is_default`, `pinned_teams` (a count) |
| `POST /api/v1/stores` | admin, every catalog | Body `{name, host, port, database, credential_kind, credential_ref}` (`credential_kind` is `env`, `secrets-manager` or `iam`). Opens a connection to the new store first and answers 201 with the store only if that works. 409 when it cannot be reached (the reply names no host, port or credential; the cause is in the server log), when a name or a host, port and database is already used (refused before the store's database is touched), or when the store's schema cannot be migrated (a retry is safe: a database left claimed by an add that failed is re-claimed by the next add); 422 for a malformed body |
| `PUT /api/v1/settings/default_store` | admin, every catalog | Body `{value}`, a store's name. Answers `{key, value}`. 404 for an unknown store |
| `PUT /api/v1/teams/{team}/store` | admin, every catalog | Body `{store}`. Pins the team: catalogs it owns are created there from now on. Answers `{team, store, changed}`. 404 for an unknown team or store |
| `DELETE /api/v1/teams/{team}/store` | admin, every catalog | Removes the pin; answers `{team, store: null, changed}`. 404 for an unknown team |

The command line has the same operations: `stores list`, `stores add NAME
--host H [--port 5432] --database D --credential-kind env --credential-ref
VAR`, `stores set-default STORE`, `stores pin TEAM STORE` and `stores unpin
TEAM`. They have no console control yet, so `stores` is not in the parity
table above. There is no command or route to remove a store.

### Audit API

The routes behind reading the [audit log](#audit-log). Who reads what:

- **An admin key that reaches every catalog** (or an admin user's console
  session) reads every entry, unredacted.
- **A team's owner**, through a signed-in console session only and naming
  the team (`?team=NAME`), reads the entries that belong to that team. The
  scope is the team's immutable identity, never its name: a team created
  again under an old name sees none of the old team's entries. An owner's
  keys, a plain member, a team the user does not own, a missing `team` and
  a malformed name get 403. `GET /api/v1/me`'s `owned_teams` lists the
  teams a user may name.
- **A provisioner key** reads only the entries it wrote (`GET /api/v1/audit`
  only; the export is 403 for it).
- Anyone else gets 403.

What an owner sees is redacted when it is read (the stored entry never
changes): an entry whose actor is an admin credential, a provisioner, or
any credential of an admin user (a self-issued key, a grant) shows the
actor as `{"kind": "operator"}`, with no person, credential, label or IP;
client IPs show only for the team's own members; a target person who
was an admin user when the entry was written (such as the admin an unlock
is for) is hidden, and so is a key, grant or code of theirs; an unlock's
free-text `reason` leaves `details`; and `teams` lists only the owner's own
team. For an owner the `actor` filter never matches an operator's entry and
the `target` filter never matches a hidden person or credential, so neither
reveals who operator staff are. A deleted user reads as `deleted user #N`
and their credentials as `credential #N` in every view.

Filters (all optional; an empty value is no filter; an unknown one is 400):
`team` (an admin's by name, spanning every team that had it, a personal
team by its user's name), `action` (one action name; 400 for an unknown
one), `actor` (a person, credential id or actor label), `target` (a target
name, a target person, or a credential id), `since` and `until` (RFC 3339;
`since` inclusive, `until` exclusive; 400 otherwise), `after` (the cursor:
entries older than this position; 400 when it is not a cursor, a bare entry
id included, or for an owner not a cursor issued for that team) and `limit`
(1 to 500, default 100). A filter that can name nothing (a malformed team,
a NUL) matches no entry.

Where entries live: the six catalog events (`catalog.published`,
`catalog.imported`, `catalog.repaired`, `catalog.reverted`,
`catalog.changeset_discarded`, `catalog.maintenance_run`) are written in
the store of the catalog they are about, in the transaction that makes the
change, so an entry exists exactly when its change does. Every other action
is written to the account database, and so are catalog events written
before stores were split. The log reads the account database and every
store together and merges them by time, then store, then id, newest
first, so an entry appears once and a page never skips or repeats one. Ids
are per source: an entry's `id` is only unique together with its `store`.
Entries stay readable after their catalog is deleted or moved to another
team, and names are read from the account database when the page is built,
so deleting a user clears their name in store entries too. A store that
cannot be reached fails the request (503) instead of returning a page
without its entries.

An entry is `{id, store, at, action, source, actor, ip, target, teams,
details, requests}` (`store` is the name of the store the entry is on, left
out for the account database; a team owner's has neither `id` nor `store`,
below): `actor` is `{"kind": "operator"}` or `{"kind": "known", person,
credential, label, operator}`; `ip` is `null` when not shown; `target` is
`{kind, name, person}` (`name` is `null` for a user); `teams` are names,
a personal team shown as its user; `requests` is a `catalog.admin_read`
entry's request count, else `null`.

A team owner never sees an entry id (issue #430). Ids are one sequence
across the whole server, so the gaps between two of a team's entries would
show how much every other team did in between. An owner's entries, on the
page and in the export, have no `id` key, and an owner's `next_after` is an
opaque string: the next entry's position (time, store and id) and the owner's team, encrypted
(ChaCha20-Poly1305, a fresh random nonce each time, so two cursors for the
same place look unrelated) under a server key. Pass it back unchanged as
`?after=`. It is the only cursor an owner's request accepts: a readable
cursor, an altered cursor, or one issued for another team the user owns is
400 (`InvalidPath`, the same message whatever is wrong) and shows no
entries. Admins and provisioners keep the integer `id` and get a readable
`next_after`, `<microseconds>:<store>:<id>` (the store is empty for the
account database); treat it as opaque and pass it back as `?after=`.

The key is 32 random bytes the server creates the first time an owner
reads the log, in the `server_keys` table (row `audit_cursor`), so every
replica reads the same key and cursors survive restarts; nothing needs
configuring. Rotating it is deleting the row
(`DELETE FROM server_keys WHERE name = 'audit_cursor'`): the next owner read
creates a new key, every replica uses it at once (none caches it), and a
cursor sealed under the old key is refused with 400. An owner scrolling the
console's audit page then sees that refusal on Load more and starts again
from the first page with Refresh; no entry is lost or shown to anyone else.

| Method and path | Key | Does |
|---|---|---|
| `GET /api/v1/audit` | admin, every catalog; a team owner's console session with `?team=`; a provisioner | `{entries, next_after, actions}`: entries newest first, at most `limit`; `next_after` is the last entry's cursor when the page is full (readable for an admin or a provisioner, sealed for an owner; pass it back as `?after=`; a full last page is followed by an empty one), else `null`; `actions` lists every action name |
| `GET /api/v1/audit/export` | as `GET /api/v1/audit`, not a provisioner | Every entry the filters select (`after` starts it, an owner's opaque cursor for an owner; `limit` is ignored), newest first, as JSON Lines (`application/x-ndjson`, one entry per line, downloaded as `audit.jsonl`) |

```sh
curl -s -H "Authorization: Bearer $OKF_MCP_KEY" \
    "https://okf-mcp.example.com/api/v1/audit?action=user.created&limit=20"
```

The CLI reads the same log with operator access (every entry, unredacted,
as an every-catalog admin), taking the same filters as flags (`--team`,
`--action`, `--actor`, `--target`, `--since`, `--until`, `--after`,
`--limit`) and refusing a bad one the same way:

```sh
# Newest first, one tab-separated line per entry: cursor, time, action, actor
# (`person NAME`, else `credential ID`, else the actor label), target
# (`user NAME`, or `KIND NAME`, then `person NAME` when the entry names one),
# teams (comma-separated, `-` for none) and the details as JSON. Control
# characters in names and labels are escaped. Pass a full page's last cursor
# (the first column) as --after for the next page.
okf-mcp audit list --action user.created --limit 20

# Every matching entry as JSON Lines, the records `GET /api/v1/audit/export`
# answers (`--after` starts it; `--limit` is ignored).
okf-mcp audit export --team family --since 2026-01-01T00:00:00Z > audit.jsonl
```

### Unlocks API

The routes behind `okf-mcp unlock ...` ([Unlock before
access](#unlock-before-access)). Who may do what:

- **Creating** needs an admin credential at `admin` on the catalog (a
  scoped admin naming a catalog it does not administer gets 404, as for
  an unknown one). The unlock is for the caller's own user; naming another
  `user` needs an admin that reaches every catalog. A key with no user gets
  403 ("give it a user") whether or not it names a `user`: it may not
  unlock for anyone. *(Decided 2026-11-15, issue #447.)* The credential's
  own user must be an admin user: an admin key issued to anyone else gets
  403 ("not an admin user"), whoever it names, before the target is looked
  at, and nothing is recorded, so the person granting an unlock is always
  operator staff. A demotion that commits while the unlock waits is
  refused the same way. A named `user` who is not an admin gets 409. The
  CLI (`okf-mcp unlock create`) acts as the operator and is unchanged.
- **Listing**: an admin reads full records (a scoped admin only those of
  the catalogs it administers). A team's owner, through a signed-in console
  session only and naming the team (`?team=NAME`), reads the unlocks of the
  catalogs that team owns now, redacted: `{id, catalog, category, starts,
  ends, state, reads, writes, exports, by}`, with `by` always `"an
  operator"` and never the reason or who it was. Anyone else gets 403, a
  malformed team name included.
- **Ending** needs an admin (any admin, not only the unlock's); a scoped
  admin ends only the unlocks of the catalogs it administers (any other id
  is 404). Ending does not need an admin user (decided 2026-11-15, issue
  #447): it only takes access away, so any admin credential on the catalog
  may close an unlock.
- A provisioner key gets 403 on every route.

An admin's record is `{id, catalog, owner_team, user, category, reason,
starts, expires, ended, state, reads, writes, exports}`: `user` is the
admin's name, or `deleted user #N`; `ended` is `null` unless it was ended
early; `state` is `live`, `expired` or `ended`.

| Method and path | Key | Does |
|---|---|---|
| `POST /api/v1/unlocks` | admin on the catalog, of an admin user; another `user` needs every catalog | Body `{catalog, category, reason, hours?, user?}` (`hours` defaults to `unlock_max_hours`). Records `catalog.unlocked`, emails the owners when `unlock_notify_owners` is on, and answers 201 with the record. 403 for a credential with no user or whose user is not an admin user, 404 for an unknown catalog or user, 409 for a user who is not an admin, 422 for `hours` outside 1 to `unlock_max_hours` (`LimitExceeded`, `limit: "unlock_max_hours"`), a blank, over-500-character or control-character reason (`limit: "reason"`), an unknown category or field, or an `hours` that is negative or not a whole number (a body error, not `LimitExceeded`) |
| `GET /api/v1/unlocks` | admin; a team owner's console session with `?team=` | `{unlocks}`, newest first. Filters: `catalog`, `team` (the catalog's owner team now) and `live=true` (neither ended nor expired); an unknown one is 400, and a malformed name matches nothing |
| `DELETE /api/v1/unlocks/{id}` | admin | Ends a live unlock now; records `catalog.unlock_ended` and answers 200 with the record (`state: "ended"`). 404 for an unknown id, 409 for one already ended or expired |

```sh
curl -s -X POST -H "Authorization: Bearer $OKF_MCP_KEY" -H 'content-type: application/json' \
    -d '{"catalog": "main", "category": "support", "reason": "ticket 12", "hours": 2}' \
    https://okf-mcp.example.com/api/v1/unlocks
```

### Health

| Method and path | Key | Does |
|---|---|---|
| `GET /api/v1/catalogs/{catalog}/health` | read | `{catalog, lint_errors, lint_warnings, sprawl, orphans, broken_links, oldest, open_questions}`. `sprawl` counts `lint`'s `sprawl` warnings; `oldest` is the stalest concept's `oldest` row (its newest `generated`, `verified` or `reviewed` stamp; `null` without concepts); links are counted without mentions |
| `GET /api/v1/catalogs/{catalog}/reports/{report}` | read | `report` is `lint`, `sprawl`, `links`, `oldest` or `questions`; the body is that tool's result unchanged. `?n=` for `oldest` and `sprawl` (default 20, clamped to 1..=200; the MCP tools default to 5), `?mentions=false` for `links`. Another name is 404 `NotFound` |

A catalog the key does not reach, or that does not exist, is 404 on both.

```sh
curl -s -H "Authorization: Bearer $OKF_MCP_KEY" \
    "https://okf-mcp.example.com/api/v1/catalogs/main/reports/oldest?n=10"
```

Health is the top section of Recent changes (`#/health` still works and
lands there); the console opens on Browse. Its five tiles are these
counts for the selected catalog (lint errors with the warning count, sprawling
concepts, orphans with the broken-link count, the oldest stamp, open
questions). The switcher below them shows one report at a time as a sortable
table, and a row opens its concept in Browse. Refresh reloads the counts and
the reports.

### Browse and search

Browse (`#/browse/<catalog>/<path>[?s=<section>]`) shows a catalog's tree beside one
document, with tabs Content, Links, History and Raw. A history row opens
the revision's diff in a drawer. Choosing another catalog in the switcher
while browsing opens that catalog's root `index.md`. Search
(`#/search/<query>`) searches every catalog the key reaches.

Recent changes (`#/recent`) lists the revisions published in the selected
catalog, newest first: every document's, one row per revision (a path
changed three times has three rows), creates, updates, deletions and
imports alike, with the path, revision, operation, actor, message and time.
Open changesets never show; only what was published. A row opens that
revision's diff and text in the same drawer as Browse's History tab, a
deleted document's included. Refresh reads the newest page again, and Load
older continues from the last row shown. Choosing another catalog in the
switcher shows that catalog's changes.

Concepts are rendered to HTML on the server (`src/render.rs`):

- raw HTML in a document is shown as text, never run;
- links may use only `http`, `https` and `mailto`, or point at a document
  of the same catalog, which opens in the console; every other link
  (`javascript:`, `data:`, a path leaving the catalog, a link to a file
  that is not markdown) is shown as plain text;
- an outside image is shown as a link to it, because the console's CSP
  loads no outside images;
- a link to a missing document is struck through.

A body over `concept_outline_chars` is shown as its outline until you
choose "Load the whole body" or follow a section link, which loads the
whole body and scrolls to that heading.

| Method and path | Key | Does |
|---|---|---|
| `GET /api/v1/catalogs/{catalog}/tree` | read | Every concept, `index.md` and `log.md`, and the root `AGENTS.md`: `path`, `kind`, `title`, `description` (the frontmatter's, when it is a string; else null) |
| `GET /api/v1/catalogs/{catalog}/concepts/{path}?full=` | read | `get_concept` with `raw`, plus `body_html` and the whole `outline`. Over `concept_outline_chars` without `full=true`: no `body`, `raw` or `body_html` |
| `GET /api/v1/catalogs/{catalog}/links/{path}` | read | Outbound `links` and `backlinks`, as `get_concept` with `links` |
| `GET /api/v1/catalogs/{catalog}/history/{path}?limit=&before=&user=&since=` | read | The `history` tool, with its `user` and `since` filters ([Who made a revision](#who-made-a-revision)). `before` is a revision number: only revisions below it (strict), newest first. The answer's `next` is its last revision when an older matching one exists, else `null`; pass it as `before` for the next page. A `before` at or below the oldest revision gives an empty page with `next: null`; a `before` that is not an integer is a client error (4xx) |
| `GET /api/v1/catalogs/{catalog}/revisions/{n}/{path}` | read | `text` at revision `n` (null for a deletion or non-text bytes) and a unified `diff` from revision `n-1` |
| `GET /api/v1/catalogs/{catalog}/documents/{path}` | read | `read_document` (any `index.md` or `log.md`, the root `AGENTS.md`) plus `body_html` |
| `GET /api/v1/catalogs/{catalog}/recent-changes?limit=&after=&user=&since=` | read | The catalog's published revisions, newest first: `{catalog, catalog_label, changes, next_after}`, each change `{path, title, description, revision, op, actor, actor_label, user, user_label, message, at}` (`title` and `description` as the catalog has the document now; null for a deletion or a document gone since), content left out. Revisions sharing a time come by path, then revision, both descending. `limit` 1..=200 (default 50; outside is 409); `after` is the previous page's `next_after` (an unknown or malformed one is 404); `user` and `since` filter as for `history` (a `since` that is not RFC 3339 is 409); `next_after` is `null` on the last page. The MCP `recent_changes` tool is the same query without titles |
| `GET /api/v1/search?q=&catalogs=&limit=` | read | The `search` tool. `catalogs` is comma-separated and defaults to every catalog the key reaches, if it reaches at most `search_fanout_max` (25); past that, leaving `catalogs` out is a 400 (`InvalidPath`, "name the catalogs to search (at most 25 are searched by default)"). Naming more than `search_fanout_max` distinct catalogs is also a 400 (`InvalidPath`, "name at most 25 catalogs to search at once"); a repeated name counts once. A named catalog it does not reach is a 404. Each hit also has `snippet_text`: the text of the snippet's block from its line on, without markdown syntax, cut to 160 characters with an ellipsis (null without a snippet). `unavailable` lists the catalogs left out because their store could not be reached (absent when empty); the others still answer |

A catalog the key does not reach, or that does not exist, is 404 on all
eight.

The per-document routes put their kind before the path (`/links/{path}`,
not `/concepts/{path}/links`), because the path is a wildcard and must come
last. For example:

```sh
curl -s -H "Authorization: Bearer $OKF_MCP_KEY" \
    https://okf-mcp.example.com/api/v1/catalogs/main/revisions/2/systems/alpha.md
```

### Catalogs and import/export

| Method and path | Key | Does |
|---|---|---|
| `GET /api/v1/catalogs?after=&limit=` | any | One page of the catalogs the key reaches, by name (see [Catalog listings are paginated](#catalog-listings-are-paginated)), as `{catalogs, next}`, each catalog with `name`, `display_name`, `label` (the display name, else the description, else "Untitled catalog"), `description`, `owner_team` (`null` when unassigned), `owner_team_label`, `files`, `concepts`, `generation` (of the loaded snapshot, what `expect_generation` compares against), `last_change` (`null` when empty), `suspended` (`{team, notice}` while its owner team is suspended, otherwise `null`: a suspended catalog is listed, never hidden; see [Suspending a team](#suspending-a-team)) and `maintenance` (`on` or `off`). When a store cannot be reached, `unavailable` lists the ids of this page's catalogs left out because of it ([When a store cannot be reached](#when-a-store-cannot-be-reached)); it is absent otherwise |
| `POST /api/v1/catalogs` | admin, every catalog | `{name, display_name, description, owner}` (`display_name` and `description` optional, `display_name` checked as for users; `owner` required, a team's name). 201 with the new catalog's summary; 403 for a key granted named catalogs, since it would not reach the new one; 404 for an unknown owner; 409 for an invalid or taken name; 422 without `owner` or for a refused `display_name` |
| `PATCH /api/v1/catalogs/{catalog}` | admin on the catalog; an owner of its owner team signed in to the console; a provisioner | `{display_name?, description?}`, at least one (`deny_unknown_fields`). Answers `{name, previous, description, previous_display_name, display_name, label}`. 422 for a refused `display_name`. The CLI is `catalog update` |
| `GET /api/v1/labels?after=&limit=` | any credential; not a provisioner | `{users, teams, catalogs, next}`, each of the first three a map from id to label. `catalogs` is one page of the catalogs the caller reaches (paginated as `GET /api/v1/catalogs`; labels shared within a page are told apart). Every caller, an admin that reaches every catalog included, gets its own user, its teams and the owner teams of the catalogs on its page. The console reads it to show labels where an answer names something by id (see [Display names and labels](#display-names-and-labels)) |
| `GET /api/v1/labels/named?users=&teams=` | any credential; not a provisioner | `{users, teams}`, maps from id to label for the comma-separated ids named, at most 100 of each (more, or an unknown parameter, is 400). An admin key or session (which may list every user and team with their labels, `GET /api/v1/users` and `/teams`) gets every one that exists; anyone else only its own user and its own teams. An id that does not exist, or that the caller may not see, is absent. The console reads the labels of the users and teams a view shows with it |
| `PUT /api/v1/catalogs/{catalog}/owner` | admin, every catalog | `{team}`. Makes the team the owner; answers `{name, previous, owner_team}` (`previous` is `null` for a legacy catalog). 404 for an unknown catalog or team, 422 without `team`. Ownership grants no access |
| `GET /api/v1/catalogs/{catalog}/member-access` | admin; or an owner of the catalog's shared team, signed in to the console | `{name, member_access}` (`none`, `read` or `write`): what members of the owner team reach through their own keys and grants. 403 for an owner's key or grant, a non-owner and a personal team's owner (an unknown catalog too, for a session) |
| `GET /api/v1/catalogs/{catalog}/entitlements?members_after=&entitlements_after=&limit=&list=` | admin; or an owner, as `member-access` | `{name, members, members_next, entitlements, entitlements_next}`: one page of the owner team's members (`[{user, label, role}]`, the only users an owner may entitle) and of the catalog's entitlements (`[{user, label, access}]`; an owner sees its members' only), each in byte order of the user name. `limit` is the size of each list (default 100, at most 1000). A `*_next` that is not `null` goes back as the matching `*_after` for the next page; `list=members` or `list=entitlements` reads only that list (a continuation), the other coming back empty. 400 for another `list`. The same refusals |
| `PUT /api/v1/catalogs/{catalog}/member-access` | admin, every catalog; or an owner of the catalog's shared team, signed in to the console | Body `{access: "none"\|"read"\|"write"}`. Sets it, and every member's own keys and grants are capped again at once; answers `{name, previous, member_access}`. 400 for another value, 404 for an unknown catalog asked by an admin, 422 without `access`. 403 as for the reads |
| `PUT /api/v1/catalogs/{catalog}/maintenance` | admin, every catalog; or an owner of the catalog's team, signed in | Body `{maintenance: "on"\|"off"}`. Lets the maintenance runner in or keeps it out; answers `{name, previous, maintenance}`. 403 for anyone else (a non-owner's session also for an unknown catalog), 404 for an unknown catalog asked by an admin, 422 for another value or without `maintenance` |
| `DELETE /api/v1/catalogs/{catalog}[?force=true]` | admin | Deletes the catalog with its documents, history, changesets and grants; answers `{name}` once the deletion has committed. The answer does not wait for the data to go: the request marks the catalog deleted and owes the store cleanup ([Store cleanup](#store-cleanup)), so its time does not grow with the catalog's documents and history. 409 while an open changeset holds unpublished changes, unless `force=true` |
| `GET /api/v1/catalogs/{catalog}/export` | read | The same handler and 120 s timeout as `/catalogs/{name}/export` |
| `POST /api/v1/catalogs/{catalog}/import[?replace=true&only_machinery=true&expect_generation=N]` | write | The same handler, body cap and upload deadline as `/catalogs/{name}/import` |

#### Catalog listings are paginated

`GET /api/v1/catalogs`, `GET /api/v1/whoami` (its `catalogs`), `GET
/api/v1/labels` (its `catalogs`) and the MCP `list_catalogs` tool answer one
page at a time, by name:

- `limit` is the page size: 100 when absent or 0, at most 1000 (a larger
  one is cut to 1000). `list_catalogs` always serves 100.
- `next` is set while more catalogs follow and absent on the last page.
  Send it back as `after` (as it came; it is opaque) for the next page.
- A malformed `after` is a 400 (`InvalidPath`, naming `after`). A cursor only
  moves the start of the list: it never shows a catalog the key does not
  reach, whatever it names.
- A catalog this replica has not loaded yet is left out of its page, so a
  page can hold fewer than `limit`; keep following `next`.

The console follows `next`: the catalog switcher, the labels and the
pickers read every page, and the Catalogs page shows Load more.

**`?readable=true`** (`GET /api/v1/catalogs` and `GET /api/v1/whoami`) lists
only the catalogs the caller may read now: those it reaches, then admitted as
a content read is admitted, on the server clock. With `require_unlock` on, an
admin user's every-catalog key therefore leaves out a catalog of another team
until it has a live unlock, and drops it again once the unlock ends or
expires; a team member's catalogs stay. With `require_unlock` off the answer is
the plain one. The check records nothing (no read, no hourly entry). Paging
stays exact: the server skips catalogs left out, so a page is full while more
readable catalogs follow, `next` is set only when one does, and a page of
locked catalogs is never returned empty with a cursor. The plain listings
(without `readable`) are unchanged, so the console's Catalogs page and Unlock
form still find a locked catalog to unlock; `whoami`'s `all_catalogs`,
`access` and `user_admin` do not depend on the flag. The console's switcher
and the reader's catalog picker use `readable=true`. It is not a boundary: the
content routes keep refusing a locked catalog.

A catalog the key does not reach, or that does not exist, is 404 on every
route but the list, the create, the owner change, the member-access
change and the maintenance switch (which reach every catalog).

- Catalog changes take effect at once: the handler applies the change's own
  messages before answering, so the next request (and the console's catalog switcher) sees it.
- Catalog rows record no actor, as with `okf-mcp catalog`; the server's log
  line for each change names the key id and its actor.
- `only_machinery=true` is `--only-machinery`: only the archive's machinery
  is compared and written. On a non-empty catalog past generation 0 (one
  that has had a publish or import) it needs `replace=true`,
  as on the command line.
- A stale `expect_generation` is 409, with a message that says to export
  again; nothing is imported. Both import paths take these options.

```sh
curl -s -H "Authorization: Bearer $OKF_MCP_KEY" https://okf-mcp.example.com/api/v1/catalogs
```

The Catalogs page lists the catalogs with their owner team and counts. An
admin opens a row to change its description or delete it (typing its name to
confirm, with a force box for unpublished changes); an admin key that reaches
every catalog also gets New catalog (with a required owner team) and the
drawer's Change owner (Assign for an unassigned catalog). The Import & export
page sits in the main navigation, outside Administer, for anyone signed in.
Its picker lists the catalogs the credential reaches; it exports the picked
catalog as a `.tar.gz` download for `read` or more, and, for `write` or more
on that catalog, uploads one with the import options, a progress bar and the
import report (or the refusal and its problems). While the catalog's owner team is
suspended, only an admin can import: anyone else sees the panel disabled, with a note
saying so, and export still works. It pre-fills the expected generation from `GET /api/v1/catalogs`,
so an import is refused if the catalog moved since the page loaded; clear the
field to skip the check.

### Findings and runs

The Findings page (`/ui/#/findings`) lists the selected catalog's
maintenance findings, open ones first, with a filter for each other
disposition and one for all. A finding's drawer shows its claim, its evidence
at the pinned revisions and its decision history, and a write key records a
decision there with a rationale; the console makes the retry key. A decision
is recorded only: it never edits the catalog. The Runs page (`/ui/#/runs`)
lists runs newest first, one page at a time with a Load older button, and a run's drawer lists its work items with links
to their concepts and findings. The nav badge shows the selected catalog's
open findings.

| Method and path | Key | Does |
|---|---|---|
| `GET /api/v1/catalogs/{catalog}/maintenance/runs?after=&limit=&order=` | read | As `maintenance_list_runs`: oldest first by default, newest first with `order=desc` (another value is 400; `after` continues in that order); `limit` 1..=200 (default 20; outside is 409); `after` is the previous page's `next_after` (an unknown one is 404) |
| `GET /api/v1/catalogs/{catalog}/maintenance/runs/{run}` | read | As `maintenance_get_run`: the run summary and up to 50 work items, with their finding ids |
| `GET /api/v1/catalogs/{catalog}/maintenance/findings?after=&limit=&disposition=` | read | As `maintenance_list_findings`, optionally narrowed to one current disposition (`open`, `accepted_for_follow_up`, `dismissed`, `needs_input`; another value is 400). `limit` and `next_after` count matching findings only |
| `GET /api/v1/catalogs/{catalog}/maintenance/findings/{finding}` | read | As `maintenance_get_finding`: claim, evidence with pinned revisions, decision history, `outdated` |
| `GET /api/v1/catalogs/{catalog}/maintenance/counts` | read | `{catalog, open, accepted_for_follow_up, dismissed, needs_input}` by current disposition (the nav badge) |
| `POST /api/v1/catalogs/{catalog}/maintenance/findings/{finding}/disposition` | write | Body `{disposition, rationale, idempotency_key}` (an unknown field is 422). As `maintenance_dispose`: appends a decision and never edits the catalog. A retry with the same key and body returns the original response; the same key with a different body is 409. The reporting flag grants nothing here: a read-level reporter key gets 403 |

A catalog the key does not reach, or that does not exist, is 404 on all six.

```sh
curl -fsS -H "Authorization: Bearer $OKF_MCP_KEY" -H 'Content-Type: application/json' \
    -d '{"disposition":"dismissed","rationale":"Covered elsewhere.","idempotency_key":"dismiss-1"}' \
    https://okf-mcp.example.com/api/v1/catalogs/main/maintenance/findings/FINDING_ID/disposition
```

### Changesets

The Changesets page (`/ui/#/changesets`) lists the selected catalog's open
changesets: a key sees its own, an admin every key's. It marks the idle ones
(no change within `session_idle_timeout`), and a row opens the changeset's
pending diff in a drawer, with Publish and Discard. These are the
`list_changesets`, `pending_changes`, `publish` and `discard` tools over
HTTP, with the same rules: the diff is for the key that opened the changeset
or an admin (a read key opens none, so it sees none), publishing and
discarding are for that key (at write) or an admin, and another key's handle
answers 404, as an unknown one does, never 403. A suspended or locked
catalog, and the maintenance runner on a catalog with maintenance off, are
refused as the tools refuse them.

A console session owns no changeset, so a member's session sees none here;
an admin user's session sees every key's, and may publish or discard them.
To publish your own key's changeset from the console as a member, sign in
to the console with that key.

**Publishing from the console.** This is the manual way to finish what an
agent staged, for instance when its own `publish` call keeps failing.
Review the diff, type a revision message (it is required, and recorded on
every revision the publish writes), click Publish and confirm. Cancel sends
nothing. While the publish runs, Publish and Discard stay disabled, even in
a drawer opened again meanwhile.
- **Published.** The drawer closes, the handle leaves the list, and a note
  above the list shows the revisions written, with any lint warnings and
  replayed bookkeeping paths. The revisions and the `catalog.published`
  audit entry name whoever published: an admin publishing an agent's
  changeset is recorded as that admin (its key, or its console session),
  never as the key that opened the changeset. A changeset whose edits all
  match the published text closes with no revisions.
- **Nothing to publish.** A changeset with no pending files is not
  published and stays open (discard it, or let the 24-hour prune of empty
  changesets close it).
- **Refused.** A conflict (a document changed since the changeset read it)
  lists the conflicting files; a lint failure lists the errors. Nothing is
  published or discarded and the message stays in the form: fix the
  changeset through the agent or another MCP client, then publish again.

| Method and path | Key | Does |
|---|---|---|
| `GET /api/v1/changesets?catalog=` | read | The key's own open changesets; an admin's: every key's. `catalog` is optional (absent or empty: every catalog the key reaches; an unreached one is 404). As `list_changesets`: `changeset`, `catalog`, `key`, `key_name`, `actor`, `created`, `last_change`, `idle`, `paths`, plus `unavailable` (catalog ids on a store that could not be reached, absent when empty) |
| `GET /api/v1/changesets/{handle}/diff` | owner or admin | As `pending_changes`: `{catalog, changeset, files}`, each file with its unified `diff`. 404 for another key's handle; 409 once published or discarded |
| `POST /api/v1/changesets/{handle}/publish` | write (owner) or admin | As `publish`, with the body `{"message": "..."}`: `{catalog, catalog_label, changeset, published, revisions, warnings, replayed}`, each revision `{path, revision, op}`. `published: false` means the changeset held no entries and stays open; `published: true` with no revisions means every edit matched the published text, and it is closed. The revisions and the `catalog.published` entry are the caller's (an admin's own, when it publishes another key's changeset). 404 for another key's handle or an unknown one; 403 for a suspended or locked catalog, or the maintenance runner where maintenance is off; 409 for a blank message (`Refused`), a published or discarded handle (`Refused`) or a conflict (`Conflict`, with `files`: `path`, `base_revision`, `current_revision`); 422 for lint errors in the changed files (`LintFailed`, with `errors` and `warnings`) or a body without `message`. A refused publish leaves the changeset open and unchanged |
| `DELETE /api/v1/changesets/{handle}` | write (owner) or admin | As `discard` without `paths`: drops every entry and closes the handle for good (`{catalog, changeset, removed, closed}`). An admin closing another key's changeset is recorded in the [audit log](#audit-log). 404 for another key's handle; 409 once published or discarded |

```sh
curl -s -X POST -H "Authorization: Bearer $OKF_MCP_KEY" \
    -H 'Content-Type: application/json' -d '{"message": "Reviewed the agent edits"}' \
    https://okf-mcp.example.com/api/v1/changesets/$HANDLE/publish
curl -s -X DELETE -H "Authorization: Bearer $OKF_MCP_KEY" \
    https://okf-mcp.example.com/api/v1/changesets/$HANDLE
```

### Testing the console

`just test-ui` runs the browser journeys (`tests/ui_e2e`) against a real
server in headless Chromium, driven by `rustwright`. They need Docker (for
the throwaway Postgres) and Chromium. `rustwright` finds Chromium through
`RUSTWRIGHT_CHROMIUM` (the binary's path), `CHROME`, `CHROMIUM`, the
Playwright cache, then `PATH`; without one, each journey prints a skip line
and passes. A journey fails on any script error, unhandled rejection or CSP
violation in the page. `tests/ui_e2e/smoke.rs` visits every page as an admin;
each page also has its own journeys.

On some distributions `chromium-browser` on `PATH` is a stub that installs a
snap package and cannot launch headless. If journeys fail at launch, leave
`RUSTWRIGHT_CHROMIUM` unset and install a Playwright Chromium (the
Playwright cache comes before `PATH`), or point the variable at a real
Chromium binary, never at the stub.

## OAuth connectors

Some MCP clients can only sign in with OAuth: ChatGPT's custom connectors,
Claude.ai's custom connectors, and the Claude and ChatGPT connector
directories. For them the server is also an OAuth 2.1 authorization server,
following the MCP authorization specification (2026-07-28). Clients that send
an API key (`Authorization: Bearer okf_...`) are not affected: keys and OAuth
work side by side on the same `/mcp` URL.

A person connecting a client signs in on the consent page with their
account: `/oauth/authorize` sends them to `/signin` first, after checking the
client and its redirect URI. Under **Advanced Options** they tick catalogs,
choose `read` or `write` for each within their ceiling, pick a default and
approve. The page offers their ceiling 50 catalogs at a time: **Find** shows
the catalogs whose name starts with what they type, and **Next catalogs** the
50 after those shown; both keep what is ticked (at most 50 catalogs), at the
ceiling's level now. With several catalogs to offer, Advanced Options starts open and
nothing is preselected. With exactly one catalog in their whole ceiling it starts closed, with that catalog
ticked, at `write` if their ceiling allows it (else `read`), and as the
default; the page says so above the form, so approving as shown grants
exactly that. Opening Advanced Options changes it. The grant is
**self-issued**: it belongs to them, its actor is the client's name, and it
is capped at their ceiling and lowered when the ceiling is
([Signing in and self-service](#signing-in-and-self-service)). The approval
re-reads the ceiling, so a level lowered while the page was open is
refused, and each page approves at most once.

A redirect URI must match one the client registered exactly, with one
exception: a loopback `http` URI (`localhost`, `127.0.0.0/8` or `[::1]`)
matches on any port (RFC 8252 section 7.3), since native clients such as
Claude Code register `http://localhost/callback` and listen on a port the
operating system picks. Scheme, host as written, path and query must still
match, and the token request must repeat the port the authorization used.

Alternatively ("Use a sign-in code instead" on the consent page) an admin
mints a **sign-in code** that says what the connection may do: its
catalogs, `read` or `write` access, the default catalog, and the actor its
writes are recorded as. The person connecting types the code into the
server's consent page. Redeeming a code creates an **OAuth grant**, which
works like an API key with exactly the code's access: it is listed with the
keys and revoked like one. A grant from a sign-in code is admin-issued: it
keeps the code's access, uncapped, as before. A code works once, and expires
unused after 24 hours unless it was minted with another expiry.

OAuth grants are never `admin` and never submit maintenance reports (the
database refuses both). The console and the maintenance runner keep using API
keys.

### Turning it on

1. Set `public_url` to the origin clients reach the server at, and make sure
   its host is in `allowed_hosts` (section 3):

   ```toml
   allowed_hosts = ["okf-mcp.example.com"]
   public_url = "https://okf-mcp.example.com"
   ```

   The server refuses to start if the host is missing from `allowed_hosts`:
   the MCP `Host` check would otherwise refuse every OAuth client.

2. Restart the server. These now answer, with no key:

   ```sh
   curl -s https://okf-mcp.example.com/.well-known/oauth-protected-resource
   curl -s https://okf-mcp.example.com/.well-known/oauth-authorization-server
   ```

   The first names the resource, `https://okf-mcp.example.com/mcp`, and its
   authorization server, `https://okf-mcp.example.com`. A request to `/mcp`
   without a token now gets a 401 whose `WWW-Authenticate` header points
   clients at that document
   (`Bearer resource_metadata="https://okf-mcp.example.com/.well-known/oauth-protected-resource"`).

**Routing.** The example ingress (section 5) routes every path, so it needs
no change. A proxy that routes only some paths must also pass
`/.well-known/oauth-protected-resource` (and the same path with `/mcp`
appended), `/.well-known/oauth-authorization-server`, `/oauth/` and `/ui/`
(the consent page's stylesheets), and must let the server's own
`Content-Security-Policy` through on `/oauth/`.

**Egress.** A client that identifies itself by a Client ID Metadata Document
(an `https://` `client_id`) makes the server fetch that document from the
client's host over HTTPS. The fetch ignores `HTTP_PROXY` and the other proxy
variables, so a network policy that blocks the pod's direct outbound HTTPS
breaks such clients. A fetched document is cached (5 minutes to 24 hours, from
its `Cache-Control`). If a refetch fails in transport (timeout, refused
connection, no DNS answer, a non-200 status other than 404 and 410), the
cached copy keeps working for up to 24 hours after it was fetched. If the
host answers 404 Not Found or 410 Gone, the document has been deleted, and
that authorization fails at once. If the refetch delivers an invalid
document (not JSON, too large, a changed `client_id`, invalid
`redirect_uris`), that authorization fails at once too. The cached copy is left
in place, not replaced or evicted (also after a 404 or 410), so a later refetch that fails in transport
can still use it within the 24 hours, and the next valid document replaces it. Clients that register
themselves (dynamic client registration) need no egress.

**State.** Without `[redis]` the server stays single-replica. Consent pages in
progress and authorization codes not yet exchanged live in memory, so a
restart interrupts at most a sign-in in progress. With `[redis]` they live in
Redis, so any replica can take the next step and a restart interrupts nothing;
when Redis fails the consent page and the token endpoint answer 503 and the
person starts again from the client. The person starts again from the client, with
the same code if they had not yet chosen Approve (a code is spent only by
Approve). Grants, tokens, codes and clients are in the database.

### Connecting a client

The steps are the same for ChatGPT, Claude.ai and any other client that
supports remote MCP servers with OAuth. The client's own screens differ and
change over time, so they are described generically here.

A person with an account skips step 1: in step 3 they sign in (if their
browser has no console session yet), choose catalogs and levels within their
ceiling, and approve. The steps below are the sign-in code path, for anyone
without an account.

1. **Mint a sign-in code** for the connection, on the server:

   ```sh
   kubectl -n okf-mcp exec deploy/okf-mcp -- \
       okf-mcp oauth code create chatgpt --actor example-actor \
       --access write --catalogs notes --default notes --user alice
   ```

   The command prints the code (`okfsc_...`) once on stdout, and its id and
   expiry on stderr. Only its SHA-256 is stored. Give it to the person
   connecting over a private channel. An admin can also mint one in the web
   console: API keys page, "Sign-in codes", "New sign-in code".

2. **Add the connector** in the client (usually in its settings, where custom
   connectors or remote MCP servers are added). Give it the URL
   `https://okf-mcp.example.com/mcp`, that is `<public_url>/mcp`, and choose
   OAuth if the client asks how to authenticate. Leave any client ID and
   client secret fields empty: the server accepts clients that identify
   themselves by a Client ID Metadata Document and clients that register
   themselves, and needs no secret from either.

3. **Sign in.** The client opens the server's consent page in a browser. It
   names the client and the host it will send the browser back to. A client
   that registered itself, rather than publishing a metadata document, is
   marked "unverified": its name is whatever it said. Enter the sign-in code
   and choose Continue. The next page states what will be granted (access,
   catalogs, actor and default catalog). Choose Approve, and the browser goes
   back to the client, which is now connected. Deny sends the client an
   `access_denied` error and leaves the code unused.

The sign-in must finish within 10 minutes of the client opening the page, and
five wrong codes end it; either way the client has to start again. The page
gives the same message for a mistyped, expired, revoked or already used code,
on purpose.

The client renews its access itself: access tokens last
`oauth.access_token_ttl` and are renewed with a refresh token, which lasts
`oauth.refresh_token_ttl` from its last use. A connection used at least that
often stays connected.

#### Concurrent refreshes

Each refresh token can be used once: a refresh issues a new pair, and a used
token presented again is taken for a copied one, which ends the connection
(every token of the sign-in is revoked). A client that runs several
processes on one stored credential, such as Claude Code with parallel
sessions, sends the same refresh token from each process when the access
token expires, and only one can win. Within `oauth.refresh_reuse_grace` of
a token's first use (60 seconds by default), a second use by the same
client is rotated again instead: each process gets a pair of its own, and
the connection stays. The window starts at the first use and does not
slide. It never revives a revoked connection, and another client presenting
the token is refused as before. Past the window, a reuse ends the
connection as before.

`/oauth/token` answers a `resource` other than this server's MCP endpoint
(`<public_url>/mcp`, compared byte for byte) with `400 invalid_target`.
`/oauth/authorize` answers it with a `303` redirect to the client carrying
`error=invalid_target`, not a 400. The `okf.oauth.token` span (at
authorize, the `debug` span `authorize`) then carries `okf.resource`, how
the sent value differed, never the value: `trailing_slash` (the endpoint with a `/` added),
`origin` (the bare origin), `other_path` (this host, another path),
`other_host` (another host, scheme or port), `unparseable` (empty, relative or
malformed) or `other_form` (the endpoint written another way: letter case in
the host, an explicit default port, a query or a fragment). It is still
refused; the value tells you which spelling the client uses.

### Why a credential was refused

A 401 from `/mcp`, an archive route or `/api/v1` is one answer for every
cause: the same status, body and `WWW-Authenticate` header, with no `error`
or description, so a caller learns nothing about which credentials exist. The
request span (exported at the default `info` filter) carries the cause as
`okf.auth.failure`, a closed set. It never holds the token, a prefix of it,
its hash or its key id.

| `okf.auth.failure` | Meaning |
|---|---|
| `missing` | No `Authorization` header, a `Bearer` header with an empty token, or (on `/api/v1`) no session cookie either. A `DELETE /mcp` with this value is a client closing a session without a credential |
| `scheme` | An `Authorization` header that is not `Bearer <token>` (another scheme, no space, or bytes that are not text) |
| `unknown` | The token or cookie matches no live credential: never issued, an `okfat_` token with OAuth off, a refresh token's value, an API key that is revoked or expired (an API key's load answers only "not live"), a console session that is dead or whose user is gone |
| `expired` | An OAuth access token past its expiry (normal for a client that closes its session after the 24 h access-token lifetime) |
| `revoked` | An OAuth access token that was revoked, or whose grant is no longer live (revoked, or past its own expiry) |
| `wrong_resource` | An OAuth access token issued for another resource (RFC 8707) |

A suspended user or team never gets a 401 here: suspension refuses at the
catalog with a 403, so it has no value. A provisioner key used on `/mcp` is a
403 as before. The value is absent on a request that was admitted.

The verifier tells `unknown`, `revoked` and `expired` apart from the one
read of `oauth_tokens` by hash that it already made (the row's `revoked_at`
and `expires_at`, checked after the read instead of in the `WHERE`), so an
unknown token costs the same query as a revoked one. A token cached while
live and expired since is `expired` with no query at all, as before.

The `okf.oauth.token` span records each refresh's `okf.refresh`: `rotated`,
`grace` (rotated again within the window), `reused` (the connection ended)
or `unknown`, and `okf.error` on failure.

`okf_oauth_refreshes_total{result}` counts the same four values, so a
reused token can alert before anyone reports being signed out. `grace`
rising steadily is normal for a client with several processes; `reused` is
the one to watch:

```
increase(okf_oauth_refreshes_total{result="reused"}[1h]) > 0
```

A `reused` result means a connection was ended: a copied token, or a client
process presenting a refresh token after the window. The counter is on the
Prometheus listener; a deployment that exports only traces and logs over
OTLP sees the result on the span only.

#### Idle grants

A grant's **last activity** is when its newest token was issued: the
client's first token exchange after sign-in, and every refresh since (each
refresh issues a new pair). Requests made with an access token are not
recorded one by one, but a client that keeps working refreshes at least
every `oauth.access_token_ttl`, so the time lags real use by at most that
much. `okf-mcp oauth grant list` and `key list` show it as `last_activity T`,
the console's API keys page and My grants as Last active. `-` means no token
is on record: the grant was never used, or its tokens expired more than a
week ago and were pruned.

Once an hour the `prune_oauth` housekeeping job revokes every grant whose last activity (or, with
no token on record, its creation) is older than
`oauth.idle_grant_revoke_after` (30 days by default) and that holds no live
token, unrevoked and unexpired. Such a grant can never be used again (only a
live refresh token gets a new one), so the revocation only tidies the list.
With a `refresh_token_ttl` longer than the idle period, a grant is kept
until its last refresh token expires. Static API keys are never touched.

The revocation is the one `oauth grant revoke` makes: the grant shows as
revoked, and the audit log records `oauth.grant_revoked` with the actor
`system:idle-grant-revocation` and the details `{"reason": "idle",
"last_activity": "..."}`. With several replicas, each runs the pass; a grant
is re-checked under its row lock, so it is revoked and audited once. The
client reconnects with a new sign-in, as after any revoke.

Each connection needs its own code. Connecting ChatGPT and Claude.ai, two
people, or the same client again after a revoke, takes a new code each time.

### Managing codes and grants

| Command | Does |
|---|---|
| `okf-mcp oauth code create NAME --user USER [--actor ACTOR] --access read\|write (--catalogs C,C... \| --all) [--default C] [--expires 24h]` | Mints a single-use code. Prints the `okfsc_` code once (stdout) and its id and expiry (stderr). `NAME` is a label in `[A-Za-z0-9._-]+`; `--user` names the user the code and its grant belong to (it must exist); `--actor` defaults to `NAME`; `--catalogs` is comma-separated, as for `key create`; `--default` must be granted. `--access admin` is refused. `--expires` (`24h`, `3d`) defaults to 24 hours and may not exceed `oauth.code_max_expiry` |
| `okf-mcp oauth code list` | Every code, never the code itself. Tab-separated: name, actor, id, access, state (`live`, `expired`, `redeemed` or `revoked`), catalogs (`*` for all), `default D`, `expires T`, `grant G` (the grant a redeemed code became), `user U`; an empty cell is `-` |
| `okf-mcp oauth code revoke ID` | Revokes a code nobody has redeemed. Refuses a redeemed or already revoked one |
| `okf-mcp oauth code assign ID --user USER` | Gives a code, in any state, a user. The grant a redeemed code became keeps its own user: assign that with `okf-mcp key assign` ([Users and teams](#users-and-teams)) |
| `okf-mcp oauth grant list` | The OAuth grants, revoked ones included, with `key list`'s columns (ending in `user U`, `kind oauth`, `client <name>`, `last_activity T`; see [Idle grants](#idle-grants)) |
| `okf-mcp oauth grant revoke ID` | Revokes a grant by its exact id, and with it every token it holds. Refuses a plain key's id |

Each takes `--database-url` and `--config` like every subcommand; inside the
pod, run them with `kubectl exec` as in step 1.

A grant is named `<code name>.<client host>-<first 8 characters of the code's
id>`, so a code named `chatgpt` redeemed by a client at `client.example.com`
becomes a grant like `chatgpt.client.example.com-1a2b3c4d`. The client host is
the metadata document's host, or a registered client's first redirect URI's. The suffix tells
apart grants made from codes of the same name. `okf-mcp key revoke`,
`grant`, `ungrant` and `maintenance-report` act on API keys only: they never
match a grant, even one whose name and actor equal a key's. Reach a grant by
its id, with `okf-mcp oauth grant revoke` or the `/api/v1/keys/{id}` routes
("API keys"). The one `key` subcommand that does reach grants is
`okf-mcp key assign`, which changes only a grant's user.

**Revoking.**

- **A connection**: revoke its grant. Made with the CLI, the revoke reaches
  every replica through the change feed within a fraction of a second (a
  lost message is bounded by `credential_ttl`); made in the console, at once
  on the replica that took the request. The client
  then gets 401 and must be connected again with a new code.
- **A code** that was never redeemed: `okf-mcp oauth code revoke`, or its
  drawer in the console. It takes effect at once (codes are read from the
  database on every use). A redeemed code cannot be revoked; revoke the grant
  it became (its `grant` column).
- **From the client**: disconnecting in the client may call `/oauth/revoke`.
  Revoking an access token ends only that token: the refresh token keeps
  working and mints a new access token, so the client stays connected.
  Revoking a refresh token ends its whole family, which disconnects the
  client; the grant then stays listed, with no working tokens, until an
  admin revokes it. Only a refresh-token revocation or revoking the grant
  disconnects a client.

Unused registered clients are deleted 24 hours after registration, and tokens
and unredeemed codes 7 days after they expire, by the hourly `prune_oauth`
housekeeping job.

### What an operator sees in the console

The API keys page (`/ui/#/keys`):

- **Sign-in codes**, below the keys. "New sign-in code" mints one (name,
  user, an optional actor, `read` or `write`, catalogs, default, an expiry of
  1 hour to 7 days)
  and shows it once, in a dialog that keeps it only until "I have stored it"
  is clicked. The list shows live codes; "Show redeemed, expired and revoked
  codes" shows the rest. A live or expired code's drawer revokes it, and
  every code's drawer has Assign or Change user.
- **OAuth grants**, in the key list, marked `OAuth` with their client's name.
  A grant's drawer shows its kind ("OAuth grant") and client, revokes it, and
  changes its catalogs. It has no maintenance-reporting switch: a grant never
  reports.

The Users page (`/ui/#/users`) has a filter box that narrows the list, as
you type, to users whose name, display name or email contains the text, in
any letter case. Its user drawer has the sign-in controls: the
Admin switch, Email (Save, Clear), "Send setup email", Entitlements, the
console sessions with Sign out and "Sign out everywhere", and Recent
sign-ins. A catalog's drawer (`/ui/#/catalogs`) has Team member access. The
Settings page (`/ui/#/settings`) edits the policy settings, lists the OIDC
providers and the mail provider read-only, sends a test email and shows the
mail queue with Retry and Delete. A scoped admin sees these controls
disabled, with the reason.

`GET /api/v1/whoami` and the key list carry each credential's `kind` (`key`
or `oauth`) and `client_name`. The console's codes JSON API is under "Web
console" ([OAuth sign-in codes](#oauth-sign-in-codes)).

### What the server checks

- **Clients.**
  - A `client_id` that is an `https://` URL is fetched as a Client ID
    Metadata Document: over `https`, with no redirects and no proxy, within 5
    seconds and 16 KiB, and only from a public address (loopback, private,
    link-local and unique-local addresses are refused, unless
    `cimd_allow_private`). The connection goes only to the addresses checked,
    so DNS rebinding cannot swap them. The document must be a JSON object
    whose `client_id` equals its URL, listing 1 to 32 redirect URIs. It is
    cached for its `Cache-Control` max-age, clamped between 5 minutes and 24
    hours; if a refetch fails, the cached copy is used for up to a day after
    it was fetched.
  - Any other `client_id` must have been registered at `/oauth/register`:
    public clients only, 1 to 10 redirect URIs, each `https` or `http` on a
    loopback host.
  - Either way the client must be able to authenticate with `none`: its
    `token_endpoint_auth_method` is absent or `none`, or its
    `token_endpoint_auth_methods_supported` lists `none` (as ChatGPT's
    document does). Grant types other than `authorization_code` and
    `refresh_token` are ignored. A refused client gets a generic "not known"
    page; the server logs the reason at `warn` (`refused an OAuth client`,
    with the `client_id`).
- **Redirects.** The `redirect_uri` must equal one the client declared,
  character for character. An error found before that check (an unknown
  client, a redirect URI it did not declare) is shown on the server's own
  error page, never sent to an unchecked address. Later errors go back to
  the client with `error`, `state` and `iss`.
- **PKCE.** S256 only, on every authorization.
- **Audience.** `resource` must be `<public_url>/mcp` (RFC 8707). It is
  required at the code exchange. At authorize it may be left out, which
  means `<public_url>/mcp`, because some clients (Crush) send it only to the
  token endpoint; any other value is refused there with `invalid_target`.
  Every token is issued for it and refused with any other.
- **Replay.**
  - An authorization code works once, within 60 seconds. Presenting it again
    revokes the tokens issued from it, even if the two requests race. A
    failed exchange (a wrong verifier, client, redirect URI or resource)
    also spends it.
  - A refresh token works once. Presenting a used one revokes the whole
    connection's tokens, and the client must be connected again with a new
    code. A refresh by another client, or for another `resource`, is refused
    and changes nothing.
- **Rate limits and caps.** Registration (a burst of 10, then one every 6
  seconds), the token and revoke endpoints (60, then 2 a second, shared) and
  the consent form (20, then 1 a second) are limited globally, not per
  address; with `[redis]` each is one bucket shared by every replica. Over the limit, the JSON endpoints answer 429 `slow_down` and the
  consent form a 429 page. Registration is also refused (429 `slow_down`)
  while 500 registered clients have no grant. At most 10,000 consent pages
  and 10,000 unexchanged authorization codes are held in memory; past that a
  new sign-in gets a 503 page.

Tokens and codes start with `okfat_` (access), `okfrt_` (refresh), `okfac_`
(authorization code) and `okfsc_` (sign-in code), so secret scanners can
find them. The server stores only their SHA-256 and never logs them.

### OAuth routes

| Route | Key | Does |
|---|---|---|
| `GET /.well-known/oauth-protected-resource`, `GET /.well-known/oauth-protected-resource/mcp` | none | Protected Resource Metadata (RFC 9728) |
| `GET /.well-known/oauth-authorization-server` | none | Authorization server metadata (RFC 8414). `registration_endpoint` is absent when `dcr_enabled = false` |
| `POST /oauth/register` | none | Dynamic client registration (RFC 7591), public clients only: 201 with the client's metadata and `client_id`. 404 when `dcr_enabled = false` |
| `GET /oauth/authorize`, `POST /oauth/authorize` | none (a console session, or a sign-in code on the form) | The consent page; without a session it redirects to `/signin` first (`signin=code` takes the sign-in code path instead). Approve and Deny answer with a page that forwards the browser to the client (a zero-delay refresh and a Continue link), so a client whose redirect URI redirects again still gets the answer |
| `POST /oauth/token` | none (PKCE, or a refresh token) | `grant_type=authorization_code` or `refresh_token`: answers `access_token`, `token_type: Bearer`, `expires_in` and `refresh_token` |
| `POST /oauth/revoke` | none | Revokes a token (RFC 7009): 200 for any token, an unknown one included; 400 `invalid_request` without `token` |

All seven paths exist only while `public_url` is set; without it they are 404.
Every response is `Cache-Control: no-store` (the discovery documents also
carry `Access-Control-Allow-Origin: *`), and every JSON error is
`{"error": ..., "error_description": ...}` with the OAuth error code:
`invalid_client` is 401, `slow_down` 429, `server_error` 500 (503 when Redis
fails: [Redis](#redis)) and the rest 400.

A `server_error` that is a 500 (not the 503) also carries `trace_id`, the
OpenTelemetry trace id of the request's span, as an extra member (RFC 6749
section 5.2 allows them), with the same generic `error_description`. The
authorize and consent pages show it on a 500 page ("Quote this reference when
you report it"). It is 32 hex digits, left out when the request has no valid
trace and never sent as zeros. No 4xx answer carries it, and no answer
carries the detail (issue #1104).

## 7. Health and time

`GET /healthz` needs no key:

```json
{"database": "ok", "cached_catalogs": 2, "clock": {"ok": true, "checked_at": "2026-11-03T12:00:00Z", "offset_ms": 3, "stratum": 2}, "redis": "ok", "rate_limits": "shared"}
```

- **Status:** 200 when the database answers `SELECT 1` within 2 seconds;
  otherwise 503 with `"database": "unreachable"`. Nothing else changes it.
- **`cached_catalogs`:** how many catalogs this replica holds in its catalog
  cache now (a count, never names). Catalogs load on first use, so it starts
  at 0 and moves with traffic; it is not the number of catalogs that exist.
- **`redis`:** `"ok"` when Redis answers `PING` within 2 seconds, `"unavailable"`
  when it fails or is late, and `null` without `[redis]`. **It never changes
  the status code.** A Redis outage would otherwise mark every replica unready
  at once and empty the Service, when the replicas can still serve catalog
  reads and writes. A failed probe is counted in `okf_redis_errors_total`
  (as `get`). The body never names the Redis host or URL. Alert on `"redis":
  "unavailable"` or on that counter, not on the status.
- **`rate_limits`:** `"shared"` while the shared rate limiter's Redis calls
  work, `"degraded"` once the last one failed (each replica is then deciding
  from its own buckets, so the effective limit is the configured one times the
  replica count), and `null` without `[redis]`. The usual cause is a Redis user
  whose ACL lacks a command the limiter's script calls (see "The Redis user's
  permissions" in [Redis](#redis)). **It never changes the status code.** At
  start-up the server runs the script once and logs an error naming the
  problem; alert on `"rate_limits": "degraded"` or on
  `okf_ratelimit_fallbacks_total`.
- **`clock`:** the last clock check, or `null` without a `time_server` (or
  before the first check). It is coarse on purpose, since the route is
  public:
  - `ok`: whether the time server gave a usable reply;
  - `checked_at`: when the check ran, by the server's clock;
  - `offset_ms`: the time server's clock minus ours, in milliseconds (positive
    means ours is behind); absent when `ok` is false;
  - `stratum`: the time server's stratum; absent when `ok` is false.

  The server's name and any error text appear only in the logs.

**The clock check** queries `time_server` over SNTP at startup and every 15
minutes, with a 5-second limit that includes the DNS lookup. Every timestamp
the server writes (`generated`, `reviewed`, log dates, revisions) comes from
its own clock, so the check is there to catch a drifting node:

- an offset over 2 seconds is logged at `error`: "the clock is off by more
  than 2 seconds", with the `server` and `offset_ms` fields. Fix the node's
  time synchronisation; the server doesn't adjust its clock;
- a failed query (no answer, a refused reply, a name that doesn't resolve) is
  logged at `warn`: "the clock check failed", with the error, which names the
  server;
- a good check is logged only at `debug`.

**A failing clock check never fails readiness**: a time-server outage must not
take the server out of service. Alert on the `error` log line, not on
`/healthz`'s status.

## 8. Upgrades

1. Release a new version with `just release-prepare <patchlevel>` and
   `just release-ship` (below). The image workflow publishes `:<version>`.
2. Change the image tag in the Deployment (in your GitOps repository, if you
   use one).
3. The Deployment recreates the pod: the old one stops, the new one runs any
   new migrations and becomes ready (catalogs load on first use). With `[redis]` and
   `RollingUpdate` (section 5), the new pod starts beside the old ones and
   each pod is replaced once the new one is ready.

- **The release that adds separate catalog stores is not a rolling
  upgrade.** It moves tables between schemas; follow [Upgrading to the
  release with separate stores](#upgrading-to-the-release-with-separate-stores)
  (snapshot, scale to zero, migrate, start).
- **Migrations are forward-only.** Once a new version has migrated the
  database, an older binary refuses to start against it. To roll back past a
  migration, restore the database from a backup taken before the upgrade
  (section 9), or fix forward with a new version.
- **Release notes** are the `changelog/` directory: one dated entry per change,
  and one per milestone.
- **Changesets survive** an upgrade: they are in the database. Open MCP
  sessions do not without `[redis]`; clients reconnect and carry on with the
  same handles. With `[redis]` they do: another replica restores the session
  from Redis, though a `GET /mcp` stream is lost and reopens.
- **0.12.0 (web console)** adds no migration and no setting. It serves
  `/ui/` and `/api/v1/` on the existing port, and `/` now redirects to
  `/ui/` (it was 404). The ingress above routes them with no change. A proxy
  that sets its own `Content-Security-Policy` must keep the console's sources
  ("Web console").
- **0.13.0 (OAuth)** adds migration `0008_oauth.sql`: existing keys become
  keys of kind `key` and work as before, and an older binary refuses the
  migrated database. It adds the `public_url` setting and the `[oauth]`
  table. Without `public_url` nothing else changes: the OAuth routes are 404
  and a 401 still carries a plain `WWW-Authenticate: Bearer`. With it, see
  "OAuth connectors" (and allow the pod outbound HTTPS if clients use
  metadata documents). Every tool now carries a title and behaviour hints
  (`readOnlyHint`, `destructiveHint`, `idempotentHint`, `openWorldHint`),
  which clients may show; no tool's arguments or results changed.
- **User login (the first release after 0.14.0)** adds migration
  `0010_user_login.sql` and apalis' `apalis` schema, and an older binary
  refuses the migrated database. Before upgrading:
  - check that the database owner can create the trusted extensions
    `citext`, `pgcrypto` and `hstore` (Postgres 13 or later), and that the
    database's `max_connections` leaves room for the mail worker's 4 extra
    connections per server process (section 4);
  - decide on `[mail]`: without it the `log` provider logs every message,
    links included, so set `smtp` or `postmark` (and its secret) before you
    send anyone a setup link on a shared server;
  - add the `[auth]` and `[[auth.oidc]]` tables if wanted (section 3). A
    secret variable a table names must be set, or `serve` refuses to start.

  The migration only adds: existing keys, grants, codes and catalogs work as
  before, every existing key is admin-issued (uncapped), every catalog's
  member access is `write`, and the pasted-key console sign-in is unchanged.
  `/api/v1` now also takes a console session cookie, and `whoami` may answer
  `kind: "session"`. The new routes are `/signin`, `/account/*`,
  `/auth/passkey/*` and `/auth/oidc/*`; a proxy that routes only some paths
  must pass them. Then follow
  [Rollout](#rollout) to give the first admin a sign-in.
- **Team suspension (the first release after 0.15.0)** adds migration
  `0011_team_suspension.sql`, which only adds two nullable columns to
  `teams` (`suspended_at`, `suspension_notice`): every team stays active and
  nothing else changes until an admin suspends one. An older binary refuses
  the migrated database. `GET /api/v1/teams`, `GET /api/v1/catalogs` and
  `list_catalogs` gain fields, and `okf-mcp team list` a `suspended` cell
  before the display name, so a script that reads that listing by position
  (`cut -f`) takes the display name from field 6, not 5
  ([Suspending a team](#suspending-a-team)).
- **The operator API (milestone 14; also after 0.15.0)** adds migrations
  `0012_audit.sql` to `0017_maintenance_audit.sql`, and an older binary
  refuses the migrated database. One changes behaviour:
  `0013_user_deletion.sql` turns the user references of `api_keys`,
  `sign_in_codes` and `team_members` into `ON DELETE CASCADE`, so deleting
  a user now deletes its keys, grants, sign-in codes and memberships with
  it (see [Deletion rules](#deletion-rules); the refusals that remain are
  the code's). The others only add: the audit tables and their append-only
  trigger (`0012`); an `audit_people` row for every user and an
  `audit_teams` row for every team (`0014`, a backfill); the `provisioner`
  key flag (`0015`, `false` for every existing key); the `unlocks` table
  and `catalogs.maintenance` (`0016`, `on` for every catalog); and the
  trigger that records each maintenance pass (`0017`). Nothing is locked
  after the upgrade: `require_unlock` stays off until an operator turns it
  on, and the branding settings default to `okf` and no contact. The log
  starts empty, and from the upgrade on every recorded change, export and
  operator read writes an entry. `team list` keeps its columns, but
  `okf-mcp key list` gains a `provisioner yes|no` cell before its last
  `user U` cell, so a script that reads the user by position (`cut -f`)
  takes it from field 10, not 9. `user list` gains `--email`, and
  `GET /api/v1/catalogs` a `maintenance` field.

- **Bounded memory (milestone 18; 0.23.0)** adds migrations
  `0023_audit_backfill.sql`, `0024_cluster_stats.sql`,
  `0025_catalogs_member_reach.sql`, `0026_catalogs_member_write.sql` and
  `0027_catalogs_member_reach_bytes.sql`, and an older
  binary refuses the migrated database. The migrations run before the new
  binary serves. Old and new replicas can run together
  during a rolling deploy (a milestone 18 server bridged both ways until
  0.29.0; see the entry after this one). What changes for operators:
  - **Three indexes are built concurrently.** `0025`, `0026` and `0027` add
    partial indexes on `catalogs` (a console session's catalogs, listed a
    page at a time per owner team; its owner teams' writable catalogs; and
    the same catalogs by name in byte order, which `GET /api/v1/me/ceiling`
    and the consent page page and find by a name prefix) with
    `CREATE INDEX CONCURRENTLY`, so the replicas still serving keep writing
    catalogs while they build; on a large database the build takes a while
    and waits for transactions already open on `catalogs`. If a build fails
    part-way (a lost connection, a cancelled pod), Postgres leaves the index
    INVALID and the migration unrecorded: the next start drops the invalid
    index and builds it again, under a lock that keeps two starting replicas
    from doing it at once. Check with
    `SELECT indexrelid::regclass, indisvalid FROM pg_index WHERE indexrelid::regclass::text LIKE 'catalogs_member_%'`.
  - **Settings added:** `catalog_cache_bytes` (`"1GiB"`),
    `catalog_directory_entries` (100000), `catalog_ttl` (`"10s"`),
    `credential_cache_entries` (100000), `user_cache_entries` (100000),
    `credential_ttl` (`"10s"`), `overlay_cache_bytes` (`"256MiB"`),
    `search_fanout_max` (25) and `worker_shutdown_grace` (`"30s"`). Size the
    pod's memory limit above `catalog_cache_bytes` plus
    `overlay_cache_bytes`, and the database's `max_connections` for
    `N x (database_max_connections + 4)` ([Postgres](#4-postgres)).
  - **Settings deprecated** (removed after 0.29.0, see the entry after this one):
    `poll_interval` (nothing polls any more), `overlay_cache_size` (use
    `overlay_cache_bytes`) and `mail.worker` (every replica runs the worker).
  - **Behaviour:** catalogs load on first use, not at start-up; changes
    reach every replica through Postgres `NOTIFY` with or without Redis;
    every replica runs the mail and housekeeping workers (the pruning runs
    once per cluster); `/healthz` reports `cached_catalogs` instead of
    `catalogs`.
  - **Response shapes:** `GET /api/v1/catalogs`, `/whoami`, `/labels` and
    MCP `list_catalogs` return one page (`next` is present when more follow;
    send it back as `after`), and `GET /api/v1/search` answers 400 when no
    catalogs are named and the key reaches more than `search_fanout_max`,
    or when more than that many distinct catalogs are named (MCP `search` too).
    `GET /whoami` lists every reachable name without loading any catalog;
    `GET /catalogs` and `list_catalogs` leave out a catalog whose data does
    not build.
  - **No answer holds a whole ceiling (issue #736).** A user's ceiling grows
    with the catalogs their teams own, so it is paged now:
    - `GET /api/v1/session` no longer has `ceiling`. A console session's
      level on a catalog is in `GET /api/v1/whoami`'s new `levels`
      (`{catalog: "read"|"write"|"admin"}` for the catalogs on that page).
    - `GET /api/v1/me` no longer has `ceiling` (it was `[{catalog,
      access}]`); `ceiling_access` (`"read"`, `"write"`, or `null` for an
      empty ceiling) names its highest level. The new `GET
      /api/v1/me/ceiling?prefix=&after=&limit=` pages it: `{ceiling:
      [{catalog, access}], next}`, by name in byte order, `prefix` for the
      names starting with it.
    - `GET /api/v1/me/keys` and `GET /api/v1/me/grants` gain `ceiling`: the
      ceiling on the catalogs their live self-issued credentials name, for
      cap notes.
    - The OAuth consent page offers 50 catalogs at a time, with Find (by
      the start of a catalog's name) and Next catalogs, which keep what is
      ticked (at most 50 ticked catalogs carry over).

    What clients must change: a script that read `ceiling` from `/session`
    or `/me` reads `GET /me/ceiling` page by page (follow `next` as
    `after`), or `whoami`'s `levels` for the catalogs it lists. The console
    shipped with the server is already changed.
  - **Labels are read for what is shown (issue #749).** `GET
    /api/v1/labels` no longer gives an admin that reaches every catalog
    every user and team: like anyone else, it gets its own user, its teams
    and the owner teams of the catalogs on its page. The labels of other
    users and teams come from the new `GET
    /api/v1/labels/named?users=&teams=` (comma-separated ids, at most 100
    of each; `{users, teams}`), which answers any admin for any user or
    team (as `GET /users` and `GET /teams` already did) and anyone else for
    its own user and teams only. The
    views that list users and teams (`GET /users`, `GET /teams`) already
    carry each row's `label`. What clients must change: a script that
    looked up an arbitrary user's or team's label in `/labels` asks
    `/labels/named` for it, or reads the `label` of the user or team view.
    The console shipped with the server is already changed.
  - **Metrics removed:** `okf_poll_duration_seconds`,
    `okf_catalog_refreshes_total`, `okf_catalog_concepts` and
    `okf_catalog_generation` (and the `okf.poll` span). Added: the
    `okf_cache_*`, `okf_invalidations_*` and `okf_housekeeping_*` families. The Grafana example
    follows.

- **The milestone 16 bridge and three settings are removed (the release
  after 0.29.0; issue #727).**
  - **Upgrade from milestone 16 only through 0.23.0 or later.** The
    `admin-generation` Redis channel is gone: nothing publishes `notified`
    on it and nothing subscribes to it. A milestone 16 replica (0.22 or
    older) running beside this release no longer hears of administrative
    changes through Redis. It sees them only on its own poll. Finish any
    rolling upgrade from 0.22 or older onto 0.23.0 or later before taking
    this release. `server_state.admin_generation` stays: the settings check
    still reads it, and no migration changes it.
  - **Removed settings:** `poll_interval`, `overlay_cache_size` (use
    `overlay_cache_bytes`) and `mail.worker`. A configuration that still
    sets them is **not** refused: neither the top level nor `[mail]` denies
    unknown keys, so the server starts, ignores the keys, and no longer logs
    the start-up warning it logged from 0.23.0 to 0.29.0. Delete the keys at
    your leisure. A typo elsewhere in those two sections is equally silent.
  - **The command line** no longer announces anything on Redis after a
    key, user, team or settings command (only catalog imports and
    `catalog repair-indexes` announce, as before).

### Preparing and shipping a release

Prerequisites: Bash, Python 3.11+ (standard library only), Git, just, the usual
Rust/build/test prerequisites, `cargo install cargo-release --version 0.25.18
--locked`, and authenticated GitHub CLI (`gh auth login`) with permission to
push a branch and merge its PR. `origin` must name this repository on github.com
(HTTPS or SSH), with exactly one resolved fetch URL and one matching resolved
push URL; extra URLs are rejected before any release push. Use a full-history
clone with the previous release tags fetched; notes are computed from the
reachable stable tags and committed history. Do not run releases concurrently.

```sh
git switch main
git pull --ff-only
just release-prepare patch   # required: patch, minor, major, or exact 1.2.3
just release-ship             # add --no-checks to skip fmt, lint and test
```

Preparation requires a clean `main` exactly equal to fetched `origin/main`.
It rejects non-stable or non-increasing versions and existing local or remote
version tags before changing versioned files. cargo-release bumps `Cargo.toml`
and `Cargo.lock`; its hook updates the four pinned image version mentions in
this document, writes `changelog/<UTC-timestamp>-release-v<version>.md` from
committed history since the latest reachable stable tag. It runs no checks:
preparation is local and publishes nothing, and shipping runs them on the same
tree.
One local `chore: Release v<version>` commit and a marked annotated version tag
are created. Nothing is pushed or published to crates.io.

Shipping checks that `HEAD` is that prepared, single-parent commit and its
annotated tag matches the package version. A published tag is always refused.
Before opening or reusing `release/v<version>`'s PR, it runs `just fmt`,
`just lint`, and `just test`, the only checks a release gets (they respect
`RUST_TEST_THREADS` if set). `just release-ship --no-checks` skips them, for a
`main` whose changes were each tested as they merged; the release PR's body
then says the checks were skipped. **There is no PR CI configured:** this is local validation, not a claim
that absent CI passed. The script does not wait for nonexistent checks. A
conflicting remote release branch, ambiguous/closed PR, API failure, or main
moving before merge stops shipping without cleanup. Branch creation uses an
empty expected-ref lease: it can create a missing branch only, never overwrite
an existing one, even if another actor creates it concurrently.

The PR is merged with its expected head commit guarded by `gh`. Before any tag
publication, the script fetches main and verifies the merge's parents are the
original base and prepared commit, its tree equals the locally validated tree,
and it is on main. A concurrent main change incorporated into the merge is
therefore refused, even if it would leave the same tree. Only then may the
unpublished preparation tag be moved locally to the actual merge commit and
pushed (no remote ref is force-updated). Both pushes explicitly disable
follow-tags, even when `push.followTags=true`; unrelated local tags stay local.
The tag push specifies its full source and destination, so `remote.origin.push`
mappings cannot redirect publication. No branches are automatically deleted.
The existing tag workflow builds and publishes the **server image only**; it
does not package the maintenance runner, create a GitHub release, update a
`latest` image, or deploy anything. Check the image workflow separately after
shipping; tag-push success does not prove image publication succeeded.

#### Failures and retries

There is no destructive rollback. Inspect `git status`, `git diff`, local tags,
remote refs, and the PR before taking further action. Preparation failures can
leave changed manifests/examples, new notes, staged files, or a local release
commit/tag if a later step failed. Do not rerun preparation on that dirty or
ahead-of-origin state. Preserve any wanted work and manually clean up only the
release artifacts you have identified; return main to origin/main before
preparing afresh. Never delete or replace a published tag.

If shipping stops after a branch push or PR creation, rerun `just release-ship`;
it reuses the matching branch/PR. If the merge succeeded but its API response or
the tag push failed, rerun the same command: it validates the already merged PR
and retries the unpublished tag, including when the local tag was already moved.
Later commits on main do not invalidate that verified merge. Local main stays
at the prepared commit throughout, so retries retain the original identity.
After successful publication, fast-forward it manually with `git fetch origin
main` and `git merge --ff-only origin/main`. An already published tag causes a
retry to refuse, not to retag or republish.

If main moved before the release merged, or the merged tree/parents differ,
manual reconciliation is required; no tag is published. Review the PR and
coordinate a fresh release rather than forcing a tag or branch. The scripts
never close a PR, delete a branch, reset main, or discard changes for you.

The release integration matrix uses throwaway repositories, synthetic bare
origins, actual just dispatch and cargo-release, and fake GitHub/check commands:

```sh
python3 -B -m unittest discover -s scripts -p 'test_release.py' -v
```

## 9. Backup and restore

**What to back up:** the database, and only the database. The pod holds no
state.

### CloudNativePG with the barman-cloud plugin

The `Cluster` in section 5 already names the plugin as its WAL archiver
(`plugins`), so every WAL segment goes to the object store. Add the object
store and a daily base backup:

```yaml
apiVersion: barmancloud.cnpg.io/v1
kind: ObjectStore
metadata:
  name: okf-mcp-backup
spec:
  retentionPolicy: "30d"
  configuration:
    destinationPath: s3://okf-mcp-backups/
    endpointURL: https://s3.example.com
    s3Credentials:
      accessKeyId:
        name: okf-mcp-backup-creds
        key: ACCESS_KEY_ID
      secretAccessKey:
        name: okf-mcp-backup-creds
        key: SECRET_ACCESS_KEY
      region:
        name: okf-mcp-backup-creds
        key: REGION
    wal:
      compression: gzip
    data:
      compression: gzip
---
apiVersion: postgresql.cnpg.io/v1
kind: ScheduledBackup
metadata:
  name: okf-mcp-db-daily
spec:
  schedule: "0 30 3 * * *"
  backupOwnerReference: self
  cluster:
    name: okf-mcp-db
  method: plugin
  pluginConfiguration:
    name: barman-cloud.cloudnative-pg.io
```

The schedule has six fields, seconds first: this one runs at 03:30 every day.

**Set the region.** barman-cloud signs its requests for a region, the AWS default
when none is given. An S3-compatible store with a region of its own refuses
requests signed for any other one, often with a bare `400 Bad Request` on the
bucket check, and the cluster's `ContinuousArchiving` condition reports
`ContinuousArchivingFailing`. `s3Credentials.region` takes the region from a
secret key, as above; the value itself is not secret.

Give the backup credentials access to this bucket only.

**Check it works.** The cluster's `ContinuousArchiving` condition should be
`True`. Then take an on-demand backup with the
[cnpg kubectl plugin](https://cloudnative-pg.io/documentation/current/kubectl-plugin/)
and wait for it to reach `completed`:

```sh
kubectl cnpg backup okf-mcp-db -n okf-mcp \
    --method plugin --plugin-name barman-cloud.cloudnative-pg.io
kubectl -n okf-mcp get backups
```

kubectl refuses flags placed before a plugin's name (`kubectl -n okf-mcp cnpg
...` fails with "flags cannot be placed before plugin name"), so they go after
`cnpg backup`.

Don't trust the schedule alone: check that a recent backup completed.

### Restore

Restore into a **new** `Cluster`, bootstrapped from the object store. It
replays the newest base backup and then the archived WAL to its end:

```yaml
apiVersion: postgresql.cnpg.io/v1
kind: Cluster
metadata:
  name: okf-mcp-db-restore
spec:
  instances: 1
  storage:
    size: 10Gi
  bootstrap:
    recovery:
      source: origin
      database: okf_mcp
      owner: okf_mcp
  externalClusters:
    - name: origin
      plugin:
        name: barman-cloud.cloudnative-pg.io
        parameters:
          barmanObjectName: okf-mcp-backup
          serverName: okf-mcp-db
```

- **`database` and `owner` name the source's database and owner.** Without
  them CloudNativePG names both `app`, and the restored cluster's `-app`
  secret points at a database and user that hold none of the restored data.
- **No `plugins` section:** the restored cluster must not archive into the
  source's path (`serverName: okf-mcp-db`), or it would write over the
  backups it came from. If it is to become the live database, give it its
  own `plugins` entry with a new `serverName` once it is up.
- **The image:** if your cluster needs `imageName` on every `Cluster` (an
  image mirror, say), give the restored one the same `imageName` as the source.
- **Before the backup,** hold every write: publishes, imports (the subcommand
  and the route) and `catalog` changes. The simplest way is to scale the
  Deployment to 0 for the window
  (`kubectl -n okf-mcp scale deploy/okf-mcp --replicas=0`, then back to 1).
  Then record the live database's numbers with the query below. These are the
  baseline.
- **Compare it** with the live database when it is ready, querying both at the
  same moment with writes still held, so the two must be equal. Counts catch
  missing rows, and one checksum over every document (catalog, path, revision,
  execute bit and content) catches changed documents. It does not cover the
  revision history's content or changesets; the counts and `last_revision`
  stand in for those:

  ```sh
  kubectl -n okf-mcp exec -i <instance> -c postgres -- psql -d okf_mcp <<'SQL'
  SELECT (SELECT count(*) FROM catalogs) AS catalogs,
         (SELECT count(*) FROM documents) AS documents,
         (SELECT count(*) FROM revisions) AS revisions,
         (SELECT max(created_at) FROM revisions) AS last_revision,
         (SELECT encode(sha256(convert_to(string_agg(
                    catalog_id || '/' || path || '@' || revision || ':' || executable
                      || ':' || encode(sha256(content), 'hex'),
                    ',' ORDER BY catalog_id, path), 'UTF8')), 'hex')
            FROM documents) AS content_checksum;
  SQL
  ```

  Run it on each cluster's primary. `catalog_id` is kept by a physical
  restore, so the checksum compares like with like, even for catalogs that share
  a path. `convert_to` keeps backslashes in paths from being read as `bytea`
  escapes.
- **If the live database moved anyway,** the checksum can no longer match.
  Require each count to be at least the baseline and at most the live value,
  and compare one document you know didn't change by its own
  `encode(sha256(content), 'hex')`. The bound assumes rows only grow: a
  catalog deleted in the window (which cascades through `documents` and
  `revisions`) breaks it even on a good restore, so a window that may include a
  deletion needs writes held, not bounds.
- **Then** either point `DATABASE_URL` and `PGSSLROOTCERT` at its `-app` and
  `-ca` secrets, or delete it (`kubectl -n okf-mcp delete cluster
  okf-mcp-db-restore`; CloudNativePG removes its volumes).
- **Tested:** the restore `Cluster` above was followed as written in milestone
  7's restore drill. A small catalog was ready about 3 minutes after the
  `Cluster` was created, and matched the live database exactly. The comparison
  query was revised after the drill (the drill's version ordered by `path`
  only), and the revised query was then run as written on the live database.

### An extra copy of each catalog

The export route gives a catalog's published files as a `.tar.gz` (a
`read` key is enough), and `okf-mcp import` or the import route restores one.
This keeps the files, not the revision history or open changesets, so it
complements the database backups rather than replacing them:

```sh
curl -sf -H "Authorization: Bearer $OKF_MCP_KEY" -o notes.tar.gz \
    https://okf-mcp.example.com/catalogs/notes/export
```

## 10. Troubleshooting

| Symptom | Cause and fix |
|---|---|
| 401 on `/mcp` or an archive route | No `Authorization: Bearer` header, or the key or OAuth access token is unknown, expired or revoked. The body is plain text, with `WWW-Authenticate: Bearer` (and `resource_metadata="..."` when OAuth is on). The request span's `okf.auth.failure` says which ([Why a credential was refused](#why-a-credential-was-refused)) |
| 403 on every `/mcp` request through the ingress | The `Host` is not in `allowed_hosts` (rmcp's DNS-rebinding check). Add the public host name and restart the pod |
| 404 on `/mcp` with "Session not found" | The `Mcp-Session-Id` was opened by another key, idled out past `session_idle_timeout`, or predates a restart. Reconnect with the key that opened it |
| `create_concept`, `move_concept` or `split_concept` returns `AlreadyExists` (a REST route that returns it answers 409) | A concept is already at the target path (a valid path that is taken). It is not `InvalidPath`, which stays for malformed, escaping, reserved and machinery paths (a change from before issue #929: a client that matched `InvalidPath` for this case must match `AlreadyExists`). The message names the recovery: `edit_document` or `replace_body` changes the existing concept (`get_concept` reads it), or choose another path, or delete or move the existing concept first. The body's `path` is the taken path |
| A tool returns `UnknownChangeset`, or `Refused` naming a closed changeset ("is published", "is discarded", "was published or discarded while this write waited", "is no longer open") | `UnknownChangeset`: the handle is mistyped or belongs to another key (only its key, and an `admin` that reaches its catalog, may use it). `Refused`: its changeset is closed, by `publish`, by `discard`, or by the prune of changesets left empty for 24 hours, possibly while the call waited for it. `list_changesets` lists the open handles the key may use; otherwise call `open_changeset` |
| A tool or `/api/v1` route returns 403 `Suspended` | The catalog's owner team is suspended (the body names the `catalog`, the `team` and the operator's `notice`). Export still works. An admin lifts it with `okf-mcp team unsuspend TEAM` ([Suspending a team](#suspending-a-team)) |
| A tool or `/api/v1` route returns 403 `UnlockRequired` | `require_unlock` is on and an operator credential opened a customer catalog's contents without a live unlock (the body names the `catalog`). Unlock it with `okf-mcp unlock create`, `POST /api/v1/unlocks` or Unlock in the console; a key with no user cannot be unlocked: give it one (`okf-mcp key assign`) |
| The maintenance runner gets 403 `Forbidden` with `"maintenance": "off"`, or reports reason `maintenance_off` | The catalog's owners turned maintenance off (the body names the `catalog`). Refused at its claim, the runner skips the catalog and exits 0 with reason `maintenance_off`, keeping the claim to replay; refused after it claimed work, it stops with that reason and exit 4. Either way nothing is lost: an owner of its team (or an every-catalog admin) turns it back on with `okf-mcp catalog set-maintenance C on` or the catalog's Maintenance switch, and the next run carries on ([Report-only runner](#report-only-runner)) |
| A team owner gets 403 from `GET /api/v1/audit` or `GET /api/v1/unlocks` | Owners read their team's entries and unlocks through a signed-in console session only, naming the team (`?team=NAME`), never through their keys. A plain member, a team the user does not own and a missing or malformed `team` get 403 too ([Audit API](#audit-api)) |
| `user delete` is refused (409) | Its personal team owns a catalog, it is the last owner of a shared team, or (over REST and in the console) it is the last admin user. The message names the cause ([Deletion rules](#deletion-rules)) |
| A tool returns `CatalogNotSelected` or `Forbidden` | The key is not granted that catalog, or its access level is too low (a `read` key calling a write tool). `okf-mcp key grant`, or a key with more access |
| 404 on an archive route | The catalog doesn't exist or the key isn't granted it (never 403, so a key can't probe which catalogs exist) |
| 403 on an archive route | The key reaches the catalog but its level is too low: import needs `write` (export needs only `read`). A `Suspended` 403 on import means the catalog's owner team is suspended: only an admin imports then |
| 413 on import | The body is over `max_import_bytes` plus slack (a plain-text axum rejection), or, if the answer comes from nginx, over the ingress's `proxy-body-size`. Raise both together |
| 409 on import | The catalog is not empty: add `?replace=true` (or `--replace` on the command line). Or, with `?expect_generation=N`, the catalog has moved since that export: export it again |
| `/healthz` is 503 and the pod is not ready | The database did not answer within 2 seconds: check the `Cluster`'s state, the `okf-mcp-db-app` secret and network policies |
| The pod never becomes ready on an IPv6 cluster | `listen` is `0.0.0.0:8080`; set `"[::]:8080"` |
| The server exits with a configuration error | The file failed validation; the message names the file and the key |
| The server exits naming a migration it does not know | The database was migrated by a newer version: deploy that version or newer |
| TLS errors at startup (`UnknownIssuer`, a name mismatch) | `PGSSLROOTCERT` names the wrong CA, or the URL's host is not one the certificate names (with CloudNativePG, use the `-rw` service from the `-app` secret's `uri`). Check that the URL names no `sslmode` or `sslrootcert` |
| `clock.ok` is false | Read the pod's `warn` line, "the clock check failed": often the name doesn't resolve from the pod, or UDP port 123 is blocked. `time_server` can be an address |
| Database tests fail locally with TLS errors | `PGSSLMODE=verify-full` is exported in your shell; unset it |
| `just test-ui` fails to launch Chromium | `RUSTWRIGHT_CHROMIUM` (or `CHROME`, `CHROMIUM`, or `PATH`'s `chromium-browser`) names a snap stub that cannot run headless. Unset it so rustwright uses the Playwright cache, or name a real Chromium ("Testing the console") |
| The console's fonts fall back, or the console logs CSP violations behind a proxy | The proxy adds its own `Content-Security-Policy`. Keep the console's sources ("Web console") |
| `/.well-known/oauth-protected-resource` is 404 | `public_url` is not set (OAuth is off), or a proxy does not route `/.well-known/`. Set it, restart, and check the proxy ("OAuth connectors") |
| The server exits naming `public_url` | It is not an `https` origin (a path, query or user info, or `http` off loopback), or its host is not in `allowed_hosts`. Give the bare origin and add its host to `allowed_hosts` |
| A client says it cannot register, or the consent page says "This client is not known to the server." | With `dcr_enabled = false`, the client must publish a Client ID Metadata Document. Otherwise its document could not be fetched or was refused: it must be `https`, on a public address, a JSON object under 16 KiB naming its own URL as `client_id`, and answer within 5 seconds without a redirect. Check that the pod can make outbound HTTPS connections |
| The consent page says the redirect URI is not one the client registered | The client sent a `redirect_uri` that is not, character for character, one of those it declared. Fix the client's registration or metadata document |
| The consent page refuses a sign-in code | The code is mistyped, expired, already redeemed or revoked (the page does not say which, on purpose). `okf-mcp oauth code list` shows its state; mint a new one |
| The consent page says "This sign-in request has expired or is not valid." or "Too many wrong sign-in codes." | The sign-in took longer than 10 minutes, the server restarted, or a code was refused five times. Start the connection again in the client |
| An OAuth client was connected but now gets 401 and asks to sign in | Its grant was revoked (by an operator, or by the housekeeping job once idle: the audit actor is then `system:idle-grant-revocation`), it was unused for longer than `oauth.refresh_token_ttl`, the client revoked its refresh token, or a used refresh token was presented again past `oauth.refresh_reuse_grace` (a sign of a copied token, or a client process holding a stale one), which ends the connection's tokens: its `okf.oauth.token` span shows `okf.refresh` `reused`. Mint a new code and reconnect |
| Approve shows "Returning to your client" but the client never connects | The browser did not follow the refresh. Click Continue on the page; if that also fails, the client is not listening on its redirect URI (a native client's loopback port closed) |
| `/oauth/register` or `/oauth/token` answers 429 `slow_down` | The global rate limit, or 500 registered clients without a grant (each is deleted 24 hours after registration). Wait and retry; look for a client registering in a loop |
