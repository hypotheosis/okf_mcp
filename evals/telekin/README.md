# Telekin agent eval

A model works through 17 scripted conversations against an okf-mcp server built from this checkout. Each case passes or fails on which tools the agent called and what it finally said.

The cases are the 15 goals of an MCPJam swarm run (three personas: a skeptical recall prober, a careful fact curator and a playful memory tester), written as fixed prompts, plus two cases for server behavior that swarm exposed. Swarm goals let a persona improvise; these cases don't, so a change in the result means a change in the server or the model, not in the conversation.

It is not a merge gate. It spends model credits, so the workflow runs on demand and on pull requests that change the eval.

## Files

| File | What it is |
|---|---|
| `suite.yaml` | The cases, in MCPJam's suite file format (`schemaVersion: "2"`). |
| `catalog/` | The `eval-sandbox` catalog every run starts from: a time server, a build server, a retired DNS resolver and an owner. |
| `setup.sh` | Migrates the database, creates the owner, the catalog (imported from `catalog/`) and a write key, then starts `okf-mcp serve` in the background. |
| `run.sh` | Runs the suite with the pinned MCPJam CLI (`mcpjam test`) and writes a JUnit report. |
| `mcp.json` | Binds the suite's `telekin` server to `$OKF_EVAL_URL`, sending the key from `$OKF_EVAL_KEY`. |

## The setup it reproduces

- **One shared catalog.** Every case reads and writes `eval-sandbox`, as a team would. Cases store different facts, so their order doesn't matter, but index and log lines from earlier cases are there for later ones.
- **A key with no default catalog**, reaching only `eval-sandbox`. That is how an OAuth connection made through a client such as MCPJam looks. A tool called without `catalog` is refused with "no catalog selected" until issue #663 lands.
- **No system prompt.** The agent knows only what the server's instructions and tool descriptions tell it. A prompt that wants something stored says so ("save this in my catalog").

## Running it

In CI: the `telekin-eval` workflow (Actions, then "Run workflow"; optionally one case id). It needs the `MCPJAM_API_KEY` repository secret, an `sk_` key from <https://app.mcpjam.com/settings/api-keys>. MCPJam's hosted models serve the agent, so no provider key is needed.

Locally, with Docker for Postgres:

```sh
cargo build --locked --bin okf-mcp
export DATABASE_URL=$(scripts/test-pg.sh up okf-eval)
evals/telekin/setup.sh okf-eval-key
OKF_EVAL_KEY_FILE=okf-eval-key evals/telekin/run.sh                 # every case
OKF_EVAL_KEY_FILE=okf-eval-key evals/telekin/run.sh --case verify-and-stamp
kill "$(cat okf-eval-server.pid)"; scripts/test-pg.sh down okf-eval
```

`run.sh` uses `--inference auto`: `ANTHROPIC_API_KEY` for your own key, else `MCPJAM_API_KEY`, else a stored `mcpjam cloud login`. Start from a fresh database for a clean run; a second run on the same one finds the first run's facts.

Exit codes are the MCPJam CLI's: `0` every case passed, `1` a case failed, `2` a bad suite file or flag, `3` credentials, `4` setup (the server could not be reached), `5` inconclusive.

## What the checks can see

A local run (`mcpjam test`) records each tool call and the agent's messages, but not tool results: `toolResultContains` and `toolResultMatches` are refused locally. So a case that needs to know what the catalog holds asks the agent to search for it, or to quote what it read, and checks the reply.

Two cases track open issues:

- `bare-search-without-selection` is expected to fail until issue #663 lands: a search without `catalog` is refused.
- `organize-into-folders` passes only if the agent replaces the "TODO: describe this folder" line a new folder gets in the root index. Today that takes a free edit of the index (issue #668), which can conflict when others write at the same time.

The first local run (2026-11-19, `anthropic/claude-haiku-5.5`) passed 16 of 17, failing only `bare-search-without-selection`.

Each case runs once, so a single run can fail on how the model words an answer rather than on what it did. Phrase a check on the final message so that only a wrong answer can fail it. For example, ask for a source's URL "exactly as the catalog records it" rather than "where it comes from". If a case still varies, raise its `iterations` and lower `passThreshold`.

## Changing it

- Check a suite file offline before pushing: `npx -y @mcpjam/cli@6.3.2 cloud eval validate --file evals/telekin/suite.yaml`.
- The CLI version is pinned in `run.sh`. The suite format follows the CLI, so bump the two together and validate.
- Keep `catalog/` free of real hosts and people; use `example.com` names.
