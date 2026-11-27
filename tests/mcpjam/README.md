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

Each `describe` is named after its MCPJam goal, verbatim.

## Reads after a publish

Against a server with several replicas (staging, say), a `publish` returns
before the other replicas have refreshed, so a read sent straight after it can
land on a replica that still shows the previous revision for a moment (usually
well under a second). Reads through the changeset and `pending_changes` are
always current for the changeset's own edits. A scenario that checks published
text right after a `publish` should read through its changeset before
publishing, or retry the read briefly. The guarantee is in
[docs/operations.md](../../docs/operations.md) ("Catalog reads are eventually
consistent across replicas").

## Running

Local, on a throwaway Postgres (needs Docker and Node 22+), from the repo root:

    just test-mcpjam

Before a PR is pushed, agents run it as a gate, `tools/agentic/gate.sh mcpjam`
(shared gate lock, 20-minute limit; see `tools/agentic/README.md`). The gate is
always local mode: it drops `OKF_MCP_URL`, `OKF_MCP_KEY`, `OKF_MCP_BIN`,
`DATABASE_URL` and the other target variables, so it tests the checkout's own
build.

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

Some checks need a catalog of the scenario's own: the unfiltered `cited_by`
listing returns at most 200 sources for the whole catalog, so a shared folder
catalog could push this run's sources out of it whatever the server does. Those
tests are skipped under folder isolation (`test.skipIf`, with the reason in
the test file) and run under catalog isolation, which local mode always uses.

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
