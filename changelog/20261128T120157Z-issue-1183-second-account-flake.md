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

No concurrency cap was added: the failure is a race, not slowness, and a cap
would slow every full gate.
