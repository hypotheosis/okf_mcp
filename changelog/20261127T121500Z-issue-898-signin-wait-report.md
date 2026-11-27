# ui_e2e: the password sign-in waits report the page (issue #898, refs)

## Added
- `SIGN_IN_REPORT` in `tests/ui_e2e/harness.rs`: a JS report of the page's address (no query or fragment), the HTTP status of the page's own navigation (`performance` navigation entry `responseStatus`: the sign-in answer when the page is the one the POST returned), the ready state, whether `#app` and `#whoami` exist, `#app`'s `hidden` flag, the `#whoami` text, the sign-in page's `#signin-error` text and the status of `GET /api/v1/session` (bounded to two seconds). Never a cookie, key, token or the typed password.
- A journey, `signin::the_sign_in_report_names_the_refusal_the_page_shows`, that pins the report on a refused sign-in (401, the error line, no `#app`, session 401) and that the typed password is not in it.

## Changed
- `Ui::sign_in_password` waits for `#app` and for `#whoami` containing `" - "` through `poll_reporting`, so a timeout panics with the report. The waits and their 60 s bound are unchanged.

## Notes
- The cause of the "timed out waiting for `#app`" failures after the password submit, seen only in full `gate.sh test` runs, is not proven. This change only makes the next recurrence explain itself (a refused sign-in shows as status 401 or 429 with an error line; a slow console as 200 with `#app` present but hidden; a stalled server as a session status of `error: AbortError`).
- Checked and ruled out from the code: the per-address sign-in limit (30 per 15 minutes) is per server and in memory in the journeys, and every journey has its own server and database; argon2 runs on tokio's blocking pool (512 threads) and the `argon2` package is built at opt-level 3 in dev; the sign-in transaction takes only the user's own row.
- Evidence for the likely trigger: `just test` runs `cargo nextest run` with one process per test and the default thread count (the machine's CPU count), so about 300 browser journeys run alongside every other test, each with its own Chromium; `just test-ui` caps them at `--test-threads=2`. `.config/nextest.toml` has no test group for `ui_e2e`. A concurrency cap there is a candidate follow-up, not changed here.
- #1101 (Chromium picks its own debugging port) is on main (`launch_options`, `--remote-debugging-port=0`).
- `sign_in_password`'s `" - "` match is also met by a key identity ("name - level"); #1151 did the exact match where a journey needs it. Not changed here.
