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

## Review rounds
- Round 1: the test also sends two sign-ins (wrong and right password) under a
  second caller trace id in the same observer, and asserts that second
  caller's own spans (one `audit`, one `start_in`) were captured, so the
  exclusion is exercised on every run.
- Round 2: removed an equality on the unfiltered `audit` count (it counted
  every span in the observer, so a foreign span would fail it, which is the
  original flake). Every assertion now reads spans selected by one of the two
  caller trace ids, and the test tolerates unrelated traces in the observer.
