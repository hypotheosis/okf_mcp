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
