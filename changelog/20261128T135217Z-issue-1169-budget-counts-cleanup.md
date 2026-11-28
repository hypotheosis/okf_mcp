# agentic deadlines now count the cleanup of a stalled call

Generated: 2026-11-28T13:52:17Z

`agentic_call_budget` capped a call by the time left before the script's
deadline, but stopping a stalled call (TERM, KILL after `agentic_kill_after`,
reaping within `agentic_reap_after`) came on top, so every script ran up to
10 s past its deadline (issue #1169).

## Change

- `lib.sh`: new `agentic_cleanup_reserve` (kill + reap, 10 s by default);
  `agentic_call_budget` subtracts it from the time left when a deadline is
  set. Both runners (`agentic_run_bounded`, `agentic_run_captured` and its
  supervisor) spend at most budget + reserve, so a call ends by the deadline.
  No deadline: unchanged, the reserve is extra on top of `AGENTIC_CALL_TIMEOUT`.
- `watch-review.sh`: the diagnostics window is 3 s plus the reserve, so the
  run ends within `AGENTIC_WATCH_TIMEOUT + 3 + reserve`.
- `merge-reviewed.sh`: the state wait ends within `AGENTIC_STATE_WAIT + 2`
  (was + 12); lookups stop starting a reserve early, so it polls about 50 s of
  the default 60, and its message reports the time actually spent.
- README, lessons and the script headers state the new bounds.

## Tests

`tests/run.sh`: budget arithmetic (reserve, cap, no deadline); a stalled gh
that ignores TERM ends by a 4 s deadline; the watch and merge bound tests are
retuned; the fake-clock merge tests expect 51 s.
