# Provision an isolated, seeded catalog per live MCPJam swarm session (okf side)

## Added

- `just mcpjam-swarm <goalId> [--sessions N] [--keep] [--dry-run]`
  (`tests/mcpjam/src/swarm.ts`, `swarm-cli.ts`). It creates N temporary catalogs
  with the REST admin API, seeds each with the fixtures of every swarm goal
  (including `/infra/ntp.md`), creates N write keys that each reach only their
  own catalog (as default), verifies that isolation over MCP, and tears down:
  keys revoked, then catalogs deleted. Teardown runs on success, failure and
  Ctrl-C; a partial setup failure tears down what exists. Both clients refuse to
  delete or revoke anything this run did not create.
- `--dry-run` calls nothing. `--keep` skips the normal-path teardown and prints
  what was left (names and key ids only).
- `tests/mcpjam/scenarios/swarm.test.ts`: setup order, teardown after a failure
  at each step, `--keep`, `--dry-run` making no HTTP call, the guard, no secret in
  output, and a real round trip against the suite's server.
- A "Live swarm" section in `tests/mcpjam/README.md`.
- `tsx` 4.20.6 as a dev dependency of `tests/mcpjam`, to run the script.

## Changed

- Nothing in the server or the deterministic suite.

## Notes

- The MCPJam side (environments, launch, polling) is NOT built. MCPJam's
  preview API gives an environment no URL or header; per-session servers would
  need saved servers, server groups (create-only, no delete) and ad-hoc
  environments (no documented removal), so a full automated teardown is not
  possible. The README records the findings with doc URLs and the manual wiring.
  `Hooks.onReady` and `KeyAdmin.secretOf` are the extension points.
- The session keys' secrets are never printed or written, so a run cannot hand
  them to a human; that is why manual wiring mints fresh keys on kept catalogs.

Related: #759.
