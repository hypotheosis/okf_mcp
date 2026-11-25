# Issue #848: the sign-in span test reads only its own trace

## Added
- `Observed::spans_of_requests_from(caller_trace)` (src/telemetry/subscriber.rs,
  test capture): every exported span in a trace whose request span links to
  that trace id.
- `Browser::traceparent` (src/http/api/testing.rs): a `traceparent` header sent
  on every request the test browser makes.

## Changed
- `sign_in_spans_carry_method_and_outcome_and_no_secret` (src/http/signin.rs)
  sends a `traceparent` of its own with both sign-ins. A request span links,
  never adopts, a caller's trace, so the test counts the `audit` and `start_in`
  spans, and scans for secrets, only in the traces that link to that id. The
  exact assertions stay; the test stays parallel (no `serial_test`).

## Notes
- The flake (two red runs on 2026-11-20, PRs #840 and #847) did not reproduce
  in plain repeat runs, so the cause of the foreign spans is not proven. The
  scope makes the test independent of it.
- No other test was changed. Candidates not shown to be affected:
  `src/store/sign_in_events.rs` asserts exactly one span named `record`, and
  `src/http/telemetry_tests.rs` asserts exact counts of `GET /healthz` spans.

## Review round 1
- The test now also sends two sign-ins (wrong and right password) under a second
  caller trace id in the same observer, asserts those spans were captured
  (two `audit` spans in all, one `audit` and one `start_in` for the decoy), and
  only then asserts the exact lists for its own requests. Without the
  filter, the exact `audit` list would hold two entries and fail, so the
  isolation is exercised on every run.
