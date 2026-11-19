## Added
- `evals/telekin/`: the Telekin agent eval. A model works through 17 scripted conversations against an okf-mcp server built from the checkout, with deterministic checks on the tools it called and what it said. 15 cases are the goals of the 2026-11-18 MCPJam swarm run; two track server behavior that run exposed (issues #663 and #668).
- `evals/telekin/catalog/`, the `eval-sandbox` seed catalog (placeholder hosts only), and `setup.sh`, which migrates, creates the owner, catalog and key, and starts the server.
- `evals/telekin/run.sh`, which runs the suite with the pinned MCPJam CLI (`@mcpjam/cli@6.3.2`, `mcpjam test`) and writes a JUnit report.
- The `telekin-eval` workflow: builds okf-mcp, starts Postgres and the server, runs the eval and uploads the report and server log. It runs on demand (optionally one case) and on pull requests that change the eval. It needs the `MCPJAM_API_KEY` repository secret.

## Notes
- Not a merge gate: it spends model credits, and gates stay local.
- The key has no default catalog, like an OAuth connection made through MCPJam, so the eval meets the same "no catalog selected" path the swarm did.
- Local MCPJam runs do not capture tool results, so cases that must know what the catalog holds ask the agent to search or quote what it read, and check the reply.
