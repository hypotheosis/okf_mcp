# Issue #1044: the shared test server of a killed run is swept

## Changed
- `scripts/test-pg.sh`: `up NAME [OWNER_PID]` takes an optional owner pid. With
  one, the container carries `okf-mcp.test-start=<pid>-0` and
  `okf-mcp.test-owner-scope=<boot id>/<pid namespace>`, the labels the sweep
  in `tests/support/pg.rs` reads. `run` passes its own shell's pid, so a run
  killed with KILL (which skips the EXIT trap, as `gate.sh` does after a
  timeout) no longer leaks its server: the next test process's sweep removes
  it once that shell is gone.
- Without an owner pid (`up NAME` for a server kept across runs), or where the
  boot id or pid namespace cannot be read, the label stays `server-<name>`
  with no scope, so the sweep never removes it.
- `tests/support/pg.rs`: the `owner_pid` doc names the unowned server as the
  `server-<name>` case.

## Notes
- This supersedes the line in the #784 changelog that says the shared
  server's label is never swept: it is never swept only when it has no owner.
- The sweep's rules are unchanged: only `ESRCH` from `kill(pid, 0)` means dead.

## Tests
- `the_shared_server_of_a_dead_run_is_swept` (`src/db.rs`) starts three real
  servers through the script (dead owner, live owner, no owner), checks their
  labels, runs the sweep with a liveness stub and asserts only the dead
  owner's server is removed.
