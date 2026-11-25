# Issue #784: test containers left by killed test processes are swept

## Changed
- `tests/support/pg.rs`: the first container start in each test process
  (`start_labelled`, which the Postgres, Redis and Valkey starts of the test
  support go through) removes the `okf-mcp.test-start` containers whose owner process no
  longer exists, and prints how many it removed. A failed sweep is only logged.
- Every container started through `start_labelled` also carries
  `okf-mcp.test-pidns`, the pid namespace of the process that started it, where
  Linux lets it be read.

## Safety
The sweep removes a container only when all of these hold; any doubt keeps it:
- it has a `okf-mcp.test-pidns` label equal to the sweeper's own namespace (a
  pid from another namespace, such as a container, means nothing here);
- its start label is `<pid>-<counter>` (the shared server's `server-<name>`
  label is never swept);
- that pid does not exist in `/proc`;
- `/proc` is usable (namespace readable, pid 1 visible), so a platform without
  it or a `hidepid` mount sweeps nothing.
A process whose `/proc` belongs to another pid namespace than its own (an
unusual sandbox) is the one setup this does not cover.
A dead pid reused by another process only keeps a leak for one more run.
Containers started before this change have no namespace label and are not
swept.

## Tests
- Unit tests in `src/db.rs` for the decision (`stale_container_ids`) with a
  stubbed liveness check, the label parser and the `/proc` liveness check.

## Not changed
- One Redis/Valkey server per run (like #304) is a follow-up candidate.
