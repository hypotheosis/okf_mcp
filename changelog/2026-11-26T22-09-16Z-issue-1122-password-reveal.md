# Show/hide buttons on password-setting fields (issue #1122)

## Added
- `src/http/ui/js/core/reveal.js`: `addReveal(input, field)` puts a non-submit
  Show/Hide button beside an existing password input and toggles its `type`
  between `password` and `text`. The input is moved into a `.reveal-field`
  wrapper, never replaced, so its value, name and constraints are untouched.
  The button carries `aria-label` ("Show new password" / "Hide new password")
  and `aria-pressed`; the input keeps its visible label as its own name.
- `src/http/ui/js/signin/reveal.js`: the same-origin module script of the
  server-rendered setup and reset pages. It adds a button to every input
  marked `data-reveal="<field name>"`.
- Browser tests in `tests/ui_e2e/account.rs` and `tests/ui_e2e/signin.rs`.

## Changed
- The Account page's password form gives the current, new and confirmation
  fields a button each (all start masked).
- `setup_html` and `reset_html` mark their inputs with `data-reveal`; the
  setup and reset responses load `reveal.js` (the reset page now goes through
  `pages::document`, like setup).
- `styles.css`: `.reveal-field` layout, using the console's tokens.

## Notes
- The buttons are added by the script, not rendered in the HTML: without
  JavaScript there is nothing inert on the page and the forms submit exactly
  as before. The CSP is unchanged (no inline script or handler).
- Revealed values stay in the existing input; nothing stores or logs them.
