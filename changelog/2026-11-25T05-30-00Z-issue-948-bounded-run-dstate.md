# Issue #948: agentic_run_bounded no longer blocks on a process that survives KILL

## Changed
- `tools/agentic/lib.sh`: after a timeout and KILL, `agentic_run_bounded`
  checks the leader with `kill -0`. A leader that survived (an uninterruptible
  kernel wait, D state) is disowned, never waited for, and the call returns 124
  with a warning on stderr. This is the behaviour `watchdog.sh`'s
  `bounded_capture` got in #675.
- The leftover check no longer runs `pgrep -c -g`, which scans `/proc` like
  `ps` and can stall the same way. It is a `kill -0 -- -pgid` liveness check;
  the warning therefore no longer says how many processes were left.
- New `agentic_reap_after` (default 5 s): how long `agentic_stop_group` waits
  after KILL. Behaviour is unchanged; tests shorten it.
- A process that survives KILL after a normal exit is also reported
  ("some survived KILL and were abandoned"); the command's own status is kept.

## Docs
- `tools/agentic/README.md` and `gate.sh` header: the promise that `gate.sh`
  returns only once every process the gate started is gone now states its one
  exception (an abandoned D-state process) and that the lock is still freed.

## Tests
- Three stub tests in `tools/agentic/tests/run.sh`: a leader that survives KILL
  after a timeout, a leftover that survives KILL after a normal exit (`kill`
  is shadowed so the signals have no effect), and no `pgrep` call.

## Not changed
- `watchdog.sh` keeps its own `bounded_capture`: it needs a temp-file output,
  stdin and its own messages and statuses, so sharing it is not a clean swap.
