# Issue #903: Account re-reads the session each time it opens

## Changed
- `src/http/read/js/main.js`: the session read and its offline classification moved out of `boot()` into `checkSession()` (answered, signed out, or offline). Boot and Account share it; the context hands it to screens.
- `src/http/read/js/screens/me.js`: Account calls `checkSession()` on every open. An answer updates `state.session` and draws the full screen; an offline or failed read draws the reduced view (message, Appearance, Delete offline copies) and leaves `state.session` as it was; a 401 goes to sign-in. A screen left while the read is out is not drawn over (`ctx.live()`).
- `docs/reference/reader.md`: Account checks the connection each time it opens.

## Added
- `read::account_opened_offline_after_an_online_start_shows_the_reduced_view`: online start, connection cut, Account shows the reduced view, connection restored, Account shows the full screen again.
- `read::a_late_session_answer_does_not_draw_account_over_another_screen`.

## Notes
- The worker never stores `/session`, so an offline re-read fails as boot's does. A successful answer goes through the worker's `admit` as at boot.
