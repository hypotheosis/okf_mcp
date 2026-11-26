# `just test` under cargo-nextest, and `Stores::forget_all` drops its stores

## Why

A benchmark of `cargo test` against `cargo nextest run` on main 25eaa83
(v0.30.0, a 32-core workstation, one shared test Postgres, builds excluded,
one run each) measured:

| Mode | cargo test | nextest + `cargo test --doc` |
|---|---|---|
| `same` | 252s | 147s + 15s |
| `separate` | 242s | 196s + 15s |
| Both | 494s | about 373s |

`cargo test` runs the 24 test binaries one after another, and the library's
unit tests alone took about 125s of each run. nextest runs every binary's
tests side by side.

The nextest runs also found a bug. `forgetting_every_store_ends_their_credential_tasks`
failed whenever it ran alone (in nextest's process per test, or as
`cargo test --exact`), after its 30-second timeout. It passed only inside the
full `cargo test` run.

## Fixed: `Stores::forget_all` (`src/stores/registry.rs`)

`forget_all` called moka's `invalidate_all` and then `run_pending_tasks`.
moka only marks entries stale on `invalidate_all` and drops them during
later housekeeping, so every open store's pool and IAM credential task lived
on after the call. That happens when the change feed reconnects
(`App`), in the CLI and in store cleanups. `forget_all` now invalidates each
open store by name, as `forget` does, before settling moka's bookkeeping, so
the entries and their tasks are gone when it returns. The test now passes on
its own in 0.17s.

## Changed: `just test`

- Each store mode runs `cargo nextest run`, then `cargo test --doc`.
  nextest does not run doctests. Both modes still run when one fails.
- The recipe stops with an install hint when `cargo-nextest` is missing
  (`cargo install --locked cargo-nextest`).
- `.config/nextest.toml`: no fail-fast. A test running past 60s is reported
  as SLOW and is killed as TIMEOUT after 5 minutes, so a hung test names
  itself instead of running to gate.sh's 75-minute limit. Failures are
  printed again at the end.
- `tools/agentic/watchdog.sh` also counts `cargo nextest` as a gate process.
  The gate.sh and README notes now mention nextest.

`just test-ui` and `gate.sh narrow` still use `cargo test`.
