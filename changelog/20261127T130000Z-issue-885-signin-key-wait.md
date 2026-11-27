# Issue #885: the sign-in journey waits for the account session, not a substring of the key's

- Cause: the journey `an_account_sign_in_replaces_a_stored_key_and_sign_out_ends_both` waited for `#whoami` to contain `carol - read`. The stored key's identity (`console-carol - read`) contains that text, so the wait could be met by a console that had booted on the key, and the read of `localStorage` `okf.key` then still saw the key.
- Console unchanged: `boot()` in `src/http/ui/js/main.js` sets `store.key = null` (which removes `okf.key` synchronously) before `enterOrRecover()` draws `#whoami`, so the visible state and storage already agree. No order inconsistency, so no product fix.
- Test fix: a new `Ui::wait_text_is` (exact text) in `tests/ui_e2e/harness.rs`; the journey uses it for `carol - read`, and asserts first that the key's text is a substring match but not an exact one.
- Other reads in the file that assume an async effect: checked, none. Sign-out reads follow `wait_visible("#login")`, which the console shows only after the key is cleared and the session ended.
