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
