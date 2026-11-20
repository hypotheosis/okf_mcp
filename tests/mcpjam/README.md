# MCPJam swarm scenarios, without inference

The MCPJam swarm for this server runs 15 goals: an LLM plays a persona, another
model drives the assistant, and a third model grades the result. That costs
inference on every run, and a red result can come from the simulated user or
the grader as easily as from the server.

This suite replays the same goals as fixed sequences of MCP tool calls through
the [MCPJam SDK](https://www.npmjs.com/package/@mcpjam/sdk) (`MCPClientManager`).
It asserts on what the server stores and answers. No model is involved, so a
red test is a server behaviour change.

| File | Swarm persona | Goals |
|---|---|---|
| `scenarios/careful-fact-curator.test.ts` | Careful Fact Curator | Create and source a fact; Review and publish a draft; Add context with sources; Organize facts into folders; Verify and stamp a fact |
| `scenarios/skeptical-recall-prober.test.ts` | Skeptical Recall Prober | Create and retrieve a stored fact; Feed scattered facts and audit retention; Test source attribution accuracy; Provoke and inspect edit conflicts; Catch hallucinated recall with a search audit |
| `scenarios/playful-memory-tester.test.ts` | Playful Memory Tester | Quiz the assistant on facts; Build a personal facts knowledge base; Test memory with surprise questions; Create indexed memory archive; Quick memory capture interview |
| `scenarios/concurrency.test.ts` | none | The shared-catalog races the 2026-11-19 wave hit, across 8 concurrent MCP sessions in one catalog |
| `scenarios/admin-guard.test.ts` | none | The delete guard on temporary catalogs |
| `scenarios/swarm.test.ts` | none | The live-swarm provisioner (see "Live swarm"), against a fake REST API plus one real round trip |

Each `describe` is named after its MCPJam goal, verbatim.

## Running

Local, on a throwaway Postgres (needs Docker and Node 22+), from the repo root:

    just test-mcpjam

Against a remote server, such as staging:

    export OKF_MCP_URL=https://<staging host>/mcp
    export OKF_MCP_KEY=...          # see "Isolation" for which key
    just test-mcpjam

Keep the key in your environment, never in a file.

| Variable | Meaning |
|---|---|
| `OKF_MCP_URL` | The MCP endpoint. Set: remote mode. Unset: local mode. |
| `OKF_MCP_KEY` | Remote mode: the API key. |
| `OKF_MCP_ISOLATION` | `auto` (default), `catalog` or `folder`; see below. |
| `OKF_MCP_CATALOG` | Folder isolation: the scratch catalog every scenario shares. |
| `OKF_MCP_API_URL` | The REST admin API, if not `<origin of OKF_MCP_URL>/api/v1`. |
| `OKF_MCP_KEEP=1` | Keep the run's catalogs (or concepts) instead of deleting them. |
| `DATABASE_URL` / `TEST_PG_URL` | Local mode: the Postgres to use (`just test-mcpjam` provides one). |
| `OKF_MCP_BIN` | Local mode: the `okf-mcp` binary (default `target/debug/okf-mcp`). |
| `RUST_LOG` | Local mode: the server's log filter (default `warn`). |

## Isolation

**Catalog isolation** (`OKF_MCP_ISOLATION=catalog`, and what `auto` picks when
the key allows it): every scenario gets a temporary catalog of its own,
`mcpjam-<time>-<random>-<scenario>`. The global setup creates them through the
REST admin API (`POST /api/v1/catalogs`), owned by the key's user, and seeds
them. The teardown deletes them with their history (`DELETE ...?force=true`).
This needs an admin key that reaches every catalog:

    okf-mcp key create mcpjam-tests --access admin --all --user <you>

Such a key can delete any catalog on the server, so `src/admin.ts` refuses to
delete a catalog unless this run made its name *and* created it. Its unit tests
are in `scenarios/admin-guard.test.ts`. Use this key on staging only.

**Folder isolation** (`folder`, and the `auto` fallback when the key may not
create catalogs): every scenario shares `OKF_MCP_CATALOG`, each under
`/swarm-<time>-<random>/<scenario>/`, so it works with a plain write key. The
teardown deletes the run's concepts. The server never deletes `index.md`
files, so empty folder indexes and `log.md` lines stay behind; use a scratch
catalog.

Either way, each scenario's seeds are published before any test runs. That
includes `infra/ntp`, which "Verify and stamp a fact" assumes. Local mode
creates its own user and admin key with the `okf-mcp` CLI, so it runs catalog
isolation by default, the same path staging takes.

## Concurrency

`concurrency.test.ts` collides on purpose. Against a multi-replica staging, its
sessions may also reach different replicas:

- **Dependent calls in parallel** (`create_concept` with `add_source`, the swarm's
  false "add_source NotFound" finding). Either order must give a valid answer,
  and a sequential retry must succeed.
- **Fan-out:** 8 sessions write to one changeset at once, and no write is lost.
- **Concurrent publishes into one folder,** existing or brand new (the Quiz
  session's Conflict). Index bookkeeping is replayed, so every publish lands.
- **Racing edits of one line:** exactly one publish wins; the others get `Conflict`.
- **Cross-session visibility:** a publish is readable by every other session as
  soon as it returns.

## Known gaps

The test `search by source id finds the concepts citing it (#761)` is a
`test.fails`: it passes while the gap exists. When #761 lands it goes red;
then change it to a plain `test`.

## Live swarm

The live MCPJam swarm is LLM-driven: goals and environments live in MCPJam, not
in this repository. All sessions of one swarm share one catalog unless each
session gets its own, which is what produced the 2026-11-19 cross-talk. This
script provisions the okf-mcp side of that isolation (issue #759):

    export OKF_MCP_URL=https://<host>/mcp
    export OKF_MCP_KEY=...      # an admin key reaching every catalog; setup and teardown only
    just mcpjam-swarm <goalId> [--sessions N] [--keep] [--dry-run]

What it does, in order:

1. Creates N catalogs `mcpjam-<run>-<i>` (`POST /api/v1/catalogs`).
2. Seeds each with the fixtures of every swarm goal, at the catalog root:
   `/infra/ntp.md` for "Verify and stamp a fact", the footbridge concept, and a
   `/<goal>/readme` per goal.
3. Creates N write keys (`POST /api/v1/keys`), each reaching only its own
   catalog, with it as the default.
4. Verifies each key: it reads `/infra/ntp.md` in its own catalog and cannot
   read another session's catalog.
5. Tears down, always (success, failure, Ctrl-C): revokes the keys, then deletes
   the catalogs. Only what this run created is touched (`src/admin.ts` and the
   same guard in `src/swarm.ts`). A failure at any step tears down whatever
   exists and stops. A step that fails per resource (a rejected request, an
   HTTP error) does not stop the others, and every leftover is named in the output.
   Only confirmed creates are deleted or revoked: a catalog needs a 201 whose
   body names it, a key needs a 200 or 201 with a usable body (non-empty key id,
   name and secret). A create with no usable answer (lost response, 5xx,
   timeout, malformed or wrongly shaped body) may or may not have happened, so
   it is never deleted or revoked by name: the run reports `unresolved: catalog
   <name>` or `unresolved: key <name>` and exits non-zero for a human to check.

`--keep` skips the normal-path teardown and prints the catalogs and key ids left
behind. `--dry-run` prints the plan and calls nothing, neither okf-mcp nor
MCPJam, and needs no environment. The admin key is read from the environment
only. Output holds key names and ids, never a secret, and the session keys'
secrets exist only in the process's memory, so a `--keep` run cannot hand them
over: mint fresh keys on the kept catalogs with `okf-mcp key create`.

### What is not automated: the MCPJam side

The decision on #759 asked the script to create one MCPJam environment per
session, carrying that session's URL and key, then launch the goal. MCPJam's
preview REST API (`https://app.mcpjam.com/api/v1`, bearer `MCPJAM_API_KEY`,
checked 2026-11-20) does not support that cleanly, so this step is manual for
now:

- An environment carries no URL or header. Its fields are `hostId`,
  `serverAttachmentId` (a server group), model, skill and secret selections
  ([show one environment](https://docs.mcpjam.com/api-reference/environments/show-one-environment.md)).
  Server URLs and credentials belong to saved project servers.
- A saved server can be created with a URL and encrypted `headers`
  (`POST /projects/{projectId}/servers`, [save a server](https://docs.mcpjam.com/api-reference/servers/save-a-server-into-a-project.md)),
  and deleted (`DELETE .../servers/{serverId}`). The docs do not show an
  `Authorization` header in the examples.
- Server groups are create-only: there is no update or delete route
  ([snapshot servers](https://docs.mcpjam.com/api-reference/environments/snapshot-servers-into-a-server-group.md)).
- A named environment cannot be created through the API. `POST
  .../environments/ensure-adhoc` get-or-creates an unnamed one for a host plus a
  server group
  ([ensure-adhoc](https://docs.mcpjam.com/api-reference/environments/get-or-create-an-unnamed-environment-for-a-composed-stack.md)),
  and the only documented removal is `POST .../environments/{id}/archive`, which
  is documented for named environments.
- Launching and polling are documented
  ([launch](https://docs.mcpjam.com/api-reference/swarm-runs/launch-a-journey.md):
  `environmentIds`, `Idempotency-Key`, `iterations`; poll `GET
  /projects/{projectId}/goal-runs/{runId}`).

So a full create-and-remove chain through the API leaves server groups (and
possibly ad-hoc environments) behind. Wiring MCPJam by hand, per session:

1. Run with `--keep` and note the catalogs.
2. For each catalog, mint a key: `okf-mcp key create swarm-<i> --access write
   --catalogs <catalog> --default <catalog> --user <you>`.
3. In MCPJam, give each session its own server (the MCP URL plus that key as a
   bearer header), group and environment, and launch the goal with those
   `environmentIds` and `iterations: 1`.
4. Afterwards revoke the keys and `okf-mcp catalog delete <catalog> --force`.

The `onReady` hook in `src/swarm.ts` is where an automated launch plugs in, and
`KeyAdmin.secretOf` is how it would read a session's secret.

### Cost and limits (for when the launch is automated)

- Each session is billed: a launch fans out into `targets x iterations` chat
  sessions, each running a model on your organization's account. Keep
  `--sessions` small.
- The MCPJam API is in preview and may change. Its rate limit is 60 requests
  per minute with bursts of 10, an organization defaults to 2 concurrent runs
  (`CONCURRENT_RUN_LIMIT`), and there are daily launch caps, so a swarm of
  more than 2 environments per goal may be queued or refused.
