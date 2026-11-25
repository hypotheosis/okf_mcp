# Issue #784: test containers left by killed test processes are swept

## Changed
- `tests/support/pg.rs`: the first container start in each test process
  (`start_labelled`, which the Postgres, Redis and Valkey starts of the test
  support go through) removes the `okf-mcp.test-start` containers whose owner process no
  longer exists, and prints how many it removed. A failed sweep is only logged.
- Every container started through `start_labelled` also carries
  `okf-mcp.test-owner-scope`, `<boot id>/<pid namespace>` of the process that
  started it, where Linux lets it be read.

## Safety
The sweep removes a container only when all of these hold; any doubt keeps it:
- it has a `okf-mcp.test-owner-scope` label equal to the sweeper's own (a pid
  from another namespace, such as a container, or from another machine or boot
  sharing the Docker daemon, means nothing here: hosts report the same
  namespace id, the boot id tells them apart);
- its start label is `<pid>-<counter>` (the shared server's `server-<name>`
  label is never swept);
- that pid does not exist in `/proc`;
- `/proc` is usable (boot id and namespace readable, pid 1 visible), so a platform without
  it or a `hidepid` mount sweeps nothing.
A process whose `/proc` belongs to another pid namespace than its own (an
unusual sandbox) is the one setup this does not cover.
A dead pid reused by another process keeps its container until that process
exits.
Containers started before this change have no scope label and are not
swept.

## Tests
- Unit tests in `src/db.rs` for the decision (`stale_container_ids`) with a
  stubbed liveness check (including the same namespace on another machine), the
  label parser, the scope and the `/proc` liveness check.
- A Docker-backed test creates containers labelled with a scope of its own
  (dead owner, live owner, no scope, shared-server label, other machine), runs
  `sweep_stale_containers_of` with that scope and asserts only the dead owner's
  is removed; it removes everything it created.

## Not changed
- One Redis/Valkey server per run (like #304) is a follow-up candidate.
