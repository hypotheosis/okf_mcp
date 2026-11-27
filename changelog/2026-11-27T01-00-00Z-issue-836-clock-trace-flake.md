# Issue 836: clock-trace test flake (diagnostics, unreproduced)

`app::tests::each_clock_check_is_its_own_trace` failed once in a full run
and never again. The failure output was not kept, and three plain runs of
the narrow filter did not reproduce it.

Ruled out: the test's capture is thread-local (`Observer`, through
`test_tracing::set_default` and its anchor subscriber), so other tests'
spans can't reach it. A first guess was a stale per-callsite interest cache
from another thread. A new test,
`a_callsite_another_thread_reached_first_still_records`, has another thread
register one callsite before an `Observer` starts and a second, distinct
callsite after it starts; the observing thread then reaches both and both
spans are captured, with and without an extra
`rebuild_interest_cache()` call in `Observer::start` (installing the
subscriber already rebuilds the cache). So no fix is claimed.

Changed: the clock test's assertions now check the `caller` and
`okf.clock.check` span counts and print every captured span name, so a
recurrence shows what was seen. The new test pins the anchor's guarantee.
The issue stays open until it recurs or is closed as unreproduced.
