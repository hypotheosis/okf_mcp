# The second-account reader journeys no longer lose their sign-in to a late 401

Generated: 2026-11-28T12:01:57Z

`signing_in_as_someone_else_in_another_tab_never_mixes_their_copies` and
`a_session_check_as_someone_else_alone_hides_the_first_accounts_copies_offline`
(`tests/ui_e2e/read.rs`) timed out after 60-65 s waiting for `#account-user`
to name `second`, in full test gates only.

## What was slow

Nothing was slow: the wait was for something that never happened. With the
wait's new report, a failing gate run (the second full run on this branch)
said:

    the page: {"href":"/read/#/me","account_user":null,"reader_hidden":false,
    "controlled":true,"session":{"status":401,...}}; the sign-in POST:
    {"status":200,"redirected":true,"path":"/ui/"}; cookies: <none>

The POST had succeeded (a failed one answers 401, never a redirect) and a
minute later `/api/v1/session` answered 401 with no session cookie left. The
reader showed its signed-out screen, which has no `#account-user`.

The cause is in the server's answer to an ended session: the `/api/v1`
middleware answers a request whose session cookie no longer works with 401
and a `Set-Cookie` that clears it (`unauthorized_clearing`,
`src/http/console_session.rs`). Signing in as `second` ends the session the
browser held (rotation), so a request the first account had in flight, whose
401 arrives after the sign-in's `Set-Cookie`, clears the new session's cookie.
The helper opened the console in the second tab first, and the console loads
its identity and pages as the first account, so under load those requests
were still in flight when the POST landed. Alone, they finish long before.

It is a test bug in how the helper is built, on top of a product race that a
user can hit too (two tabs, one signing in while the other has a request in
flight); that race is not changed here.

## Changed

- `tests/ui_e2e/read.rs`: `sign_in_second_in_another_tab` posts the sign-in
  from `/healthz` (no script, so no requests of the first account), then reads
  `/api/v1/session` back in the same tab and signs in again, up to three
  times, if it does not name `second`. It returns what each attempt answered.
  The pointless reload of the console as `second` is gone (the tab is closed
  right after). `only_the_session_check_as_second` waits with
  `Ui::wait_text_is_noting`, which on a timeout reports the page's address,
  `#account-user`, whether the worker controls the page, what
  `/api/v1/session` answers (status and user, never its csrf token), the
  sign-in attempts' answers and the cookie names (never values).
- `tests/ui_e2e/harness.rs`: `Ui::wait_text_is_noting` (via
  `poll_reporting_with`) and `Ui::cookie_names`.
- `tools/agentic/lessons.md`: the rule for two-tab sign-in journeys.

## Review round (PR #1211)

Reading `second` back does not prove the reader tab's own first-account
requests have finished: one answered 401 after the read-back would clear the
cookie again, and the wait would run 60 s on a signed-out reader. Retrying
after that point would also be wrong, since the reader's 401 path
(`signIn()`) wipes the first account's copies and the journeys would then
pass without testing the hand-over.

- `Ui::track_fetches` wraps the page's `fetch` to count its pending requests
  (a request is pending until its answer reaches the page, the worker's work
  included; the headers, `Set-Cookie` too, are applied before that), and
  `Ui::fetches_settle_within` / `Ui::wait_fetches_settled` wait until none is
  pending on two reads in a row.
- Both journeys call `track_fetches` right after the worker is ready, and
  `sign_in_second_in_another_tab` waits for the reader to be quiet before it
  signs in. The reader tab stays open and untouched, so what each journey
  tests is unchanged.
- `the_second_sign_in_waits_for_the_readers_pending_requests` holds a request
  in the reader and asserts the wait does not return while it is held and does
  once it is released. With the wait made a no-op the test fails at once
  ("the wait returned with a request of the page pending").
- Not covered: a request the worker starts on its own (not for a page
  request). The worker's background work is cache writes and the `sw.js`
  update check, none of which is an `/api/v1` read. The product race itself
  is left for a follow-up.

No concurrency cap was added: the failure is a race, not slowness, and a cap
would slow every full gate.
