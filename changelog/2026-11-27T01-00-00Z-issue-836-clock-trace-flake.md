# Issue 836: clock-trace test flake

`app::tests::each_clock_check_is_its_own_trace` failed once in a full run
and never again. The failure output was not kept, and three plain runs of
the narrow filter did not reproduce it.

Cause (from reading the code, unreproduced): the test uses `Observer`, which
installs a thread-local subscriber, so other tests' spans are not visible to
it. What can differ between runs is the per-callsite interest cache: the
`caller` and `okf.clock.check` callsites are shared with other tests on other
threads, and one that registered first can leave a cached "never" that hides
the span from this test's subscriber (issues #481, #826).

Fix: `Observer::start` now calls `tracing::callsite::rebuild_interest_cache()`
after installing the subscriber, as `rows_returned::counting` already does,
so every capturing test gets it. The test's assertions now name the spans
they captured, so a recurrence is diagnosable.
