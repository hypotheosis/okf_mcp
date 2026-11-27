# Issue 826: whoami row-count test flake

`whoami_reads_the_callers_key_not_every_key` failed once in a narrow
`http::` run (502 passed, 1 failed) and never again; the failure message
was not captured.

## Diagnosis

The runtime flavor and the thread-local subscriber are not the cause: the
request future is polled on the test's own thread (`block_on`), nothing in
the whoami path spawns a task, and `join!` stays on that thread. One run
of `http::` at the default thread count passed (583 tests), so the failure
is unreproduced.

The remaining race is the `tracing` callsite interest cache (issue 481).
A thread that reaches the `sqlx::query` callsite while no subscriber of
this test is alive can cache "never"; the test installed its subscriber
without recomputing the cache, unlike `http::tests` which calls
`rebuild_interest_cache`. The symptom would be `statements == 0`.

## Change

- `rows_returned::counting` now calls `tracing::callsite::rebuild_interest_cache()`
  after installing its subscriber, for every caller.
- The whoami test uses `counting` instead of its own copy of the setup.
