# Issue 1193: no temp file left when a signal arrives during mktemp

- `lib.sh`: new `agentic_cap_begin`, `agentic_cap_mktemp` and `agentic_traps_restore`.
  A temp file is registered for cleanup by the statement that creates it, so a
  signal that arrives while `mktemp` runs finds it registered (a trap runs only
  after a whole statement). `agentic_cap_cleanup` and `agentic_run_bounded` now
  put the caller's INT and TERM traps back instead of clearing them, and a
  signal is re-raised so the caller's handler sees it.
- `watchdog.sh` (`bounded_capture`), `agentic_run_captured` and `remove-worktree.sh`
  (its `cargo clean` log) use them.
- Not changed: `expect.sh start` (its `.new.*` dotfile is skipped by every reader
  and the window is microseconds).
- Tests: TERM during a 1 s stub `mktemp` (watchdog, `agentic_run_captured`) and
  during a hung `cargo clean` leaves no file; the caller's traps survive both
  `agentic_run_captured` and `agentic_run_bounded`, and a TERM reaches the caller's trap.
- `lessons.md`: one line.

## Review round 1
- When a caller's own TERM or INT handler returns after `agentic_cap_cleanup` or
  `agentic_run_bounded` re-raised the signal, execution used to resume in a torn-down
  capture. They now record 130 (INT) or 143 (TERM): `agentic_cap_mktemp`,
  `agentic_run_captured`, `bounded_capture` and `agentic_run_bounded` stop and return
  it, launching nothing; the caller's trap stays installed.
- Re-audit: a signal during the copy-out (after the first status check) was lost: the
  final plain cleanup cleared the traps already restored. `agentic_cap_cleanup` now
  leaves them alone once a signal was handled, and `agentic_run_captured` and
  `bounded_capture` re-check the status after the copy-out and the final cleanup.
- Review round 2: a TERM during the stop after the wait of `agentic_run_bounded` was
  also lost. The three functions now have one way out each: `agentic_run_bounded`
  and the new `agentic_cap_finish` (used by `agentic_run_captured` and
  `bounded_capture`) return 130 or 143 when a signal was recorded at any step, whatever
  the step's own status; the bodies (`*_steps`) may return early. `agentic_cap_cleanup`
  copies the saved traps before its guard, closing the one-builtin gap.

## Review round 3: signals deferred, one invariant
- Review round 3 found a fourth window: a TERM during `$(agentic_call_budget)` in
  `bounded_capture` ran the cleanup, and the supervisor then recreated the removed
  file and launched the command. Instead of a fifth spot fix, the helpers now defer
  signals. Between `agentic_cap_begin` (or the start of `agentic_run_bounded`) and the
  one exit, the INT and TERM traps only record 130 or 143; nothing is torn down
  mid-flight. Every step that creates a file, launches a process or writes output
  checks the record first (`agentic_cap_live`), the supervisor's poll stops the group
  on a recorded signal, and the exit (`agentic_cap_finish`, `agentic_signals_end`)
  removes the files, restores the caller's traps one statement per signal and
  re-raises once. The invariant is written at the top of `lib.sh`'s bounded-commands
  section.
- `agentic_cap_cleanup`, `agentic_traps_restore`, `agentic_cap_status` and
  `agentic_bounded_signalled` are gone; new `agentic_signals_begin`,
  `agentic_signals_end` and `agentic_cap_live`. `agentic_gh` / `agentic_git` and
  `bounded_capture` work out their budget under the capture's traps.
  `remove-worktree.sh`'s clean is a steps function with the same single exit.
- Tests: a TERM during a blocked budget clock (`bounded_capture`, `agentic_gh`'s
  path), while the command runs and while its leftover is stopped, each with a
  returning caller handler: 143, trap kept, no temp file, nothing launched before
  the launch and the group stopped after it.
- Independent re-audit of the deferral found four gaps, now fixed: a TERM
  right after a launch, before the leader's `setsid()`, found no group to stop
  (`agentic_stop_group` now waits up to 1 s for the group, then KILLs the
  leader); a TERM just before `wait -n` waited out the whole budget (the trap
  now ends the bounded run's timer); the budget's clock and the bounded run's
  warnings ran after a recorded signal (now checked); and under a caller's
  `set -e` a failing command skipped the exit (each wrapper takes the body's
  status with `|| status=$?`). Tests: a stub `setsid` that blocks before the
  group exists, and every helper under `set -euo pipefail`.
