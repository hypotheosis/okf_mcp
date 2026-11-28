# Captured gh and git calls in tools/agentic stay bounded

Generated: 2026-11-28T13:13:25Z

`x=$(agentic_gh ...)` waited for end-of-file on its pipe, not only for the call
to return. A daemonised child, or a process that survives KILL, kept the pipe
open, so the substitution outlived `AGENTIC_CALL_TIMEOUT` (issue #1191).

## Change

- `lib.sh`: new `agentic_capture_supervise` (the supervisor, moved out of
  `watchdog.sh`'s `bounded_capture`) and `agentic_run_captured`: stdout and
  stderr go to private mode-600 temp files, stdin is `/dev/null`, the process
  group is stopped as before, then the files are copied out (partial output kept
  after a timeout) and removed on every path, including INT and TERM. Status is
  the command's, or 124 on a timeout.
- `agentic_gh` and `agentic_git` use it. `agentic_run_bounded` still streams
  (`gate.sh`, `remove-worktree.sh`'s clean).
- `watchdog.sh`'s `bounded_capture` now calls the shared supervisor, so there is
  one capture implementation. Its warnings and statuses are unchanged.
- Callers of `agentic_gh`/`agentic_git` (watch-review.sh, merge-reviewed.sh,
  remove-worktree.sh, lib.sh) take stdout and the status only, or pass stderr
  through; none reads stdin. The one visible difference: stderr arrives when the
  call ends instead of while it runs.

## Tests

`tests/run.sh`: a `setsid` child that ignores TERM and holds stdout for 30 s
(returns in about 1 s, partial output kept, killed afterwards by its recorded
pid), a leader that exits leaving a holder, output/stderr/status/stdin
pass-through, no time left, and temp-file removal on TERM. Without the fix the
first waits for the holder.
