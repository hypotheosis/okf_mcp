# Issue 1193: no temp file left when TERM arrives during mktemp

Draft (design phase; the timestamp is set when the work is finalised).

- `lib.sh`: new `agentic_cap_begin` and `agentic_cap_mktemp`. The temp file is
  registered for cleanup by the same statement that creates it, so a signal
  that arrives while `mktemp` runs finds it registered. `agentic_cap_cleanup`
  now puts the caller's INT and TERM traps back instead of clearing them.
- `watchdog.sh` (`bounded_capture`) and `agentic_run_captured` use them.
- Tests: TERM during a 1 s stub `mktemp` leaves no file, for both callers; the
  caller's TERM trap survives `agentic_run_captured`.
