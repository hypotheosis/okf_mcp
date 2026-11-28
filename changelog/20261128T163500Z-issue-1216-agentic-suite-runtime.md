# Issue 1216: the agentic suite runs in about a minute, not 5

- `tests/run.sh` runs 4 tests at a time (`AGENTIC_TEST_JOBS`, 1 for serial). Each
  test is its own `run.sh --one <name>` in its own session, with its own temporary
  directory, git repositories (built by its first `new_fixtures`) and fixtures, so
  tests share no lock, branch or process scan. Anything a test leaves in its
  session is stopped before its result is printed. Results print in declaration
  order. `run.sh <name-substring>...` runs a subset. Tests that bound real elapsed
  time tightly (`serial_tests`) run alone, after the parallel ones.
- `lib.sh`: `AGENTIC_TICKS_PER_SEC` (default 10, the old 0.1 s tick; 1..1000) sets
  how often a bounded or captured call looks at its command. The limits are now
  measured on `$EPOCHREALTIME`, not by counting ticks, so finer ticks never
  stretch them. The tests use 100: a call on a fake clock no longer costs 100 ms.
- `watch-review.sh`: `AGENTIC_WATCH_POLL` (default 30 s). `merge-reviewed.sh`:
  `AGENTIC_STATE_POLL` (default 3 s). Four watch tests that only assert "never
  re-triggers by the deadline" poll every 100 s on the fake clock.
- Tests: tick default and fallback, finer ticks notice a finished command sooner
  and keep the 1 s limit, the default watch poll is still 30 s.
- `lessons.md`, `brief-common.md`, `README.md`: one entry each.

## Measured (164 tests, `timeout 600`)
- Before: 324.8 s serial (one watch test: 13.5 s).
- After: 62 s at 4 jobs (two runs, 62.3 s and 62.0 s), 179.6 s at 1 job (one watch test: 2.4 s).

## Review round 1
- `lib.sh`: the tick is formatted from whole microseconds with its whole-second part
  (`AGENTIC_TICKS_PER_SEC=1` is `1.000000`, not `0.1000`; 7 is `0.142857`).
- `run.sh`: `AGENTIC_TEST_JOBS` above 4 is clamped to 4 with a note on stderr (0 and
  non-numbers still exit 2).
- `run.sh`: after each wake-up the pool collects every tracked test that has ended
  (`kill -0`), once each, so tests that end while another's group is being stopped are
  not missed; a pass that collects nothing 50 times in a row fails loudly.
  `AGENTIC_TEST_EXTRA_FILE` lets run.sh's own tests add fixture tests to a nested run.
- Tests: rate 1 and rate 7 ticks and budgets, the job cap, and a pool run in which three
  tests end while a fourth's group takes 2 s to stop. That last one also passes on the
  old pool: bash 5.3's `wait -n` returns children that ended earlier, so the loss could
  not be reproduced; the new collection does not depend on that.
