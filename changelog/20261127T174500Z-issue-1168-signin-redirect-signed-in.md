# Issue #1168: GET /signin sends a signed-in browser on to return_to

## Changed
- `src/http/signin.rs` (`sign_in_page`): when the request carries a live console session (`console_session::signed_in_user`, the check `/oauth/authorize` uses: the same `touch` as the `/api/v1` middleware, then a user that still exists), answer `303 See Other` to `safe_return_to(return_to)` with `Cache-Control: no-store`, instead of the form. `?notice=reset` does not change that. No cookie, an unknown, expired, idle or revoked session still gets the 200 form. A database error while checking is logged at `warn` (the error only, never the cookie) and draws the form, so a store outage does not lock people out.
- Docs: `docs/reference/reader.md` and the sign-in page section of `docs/operations.md`.

## Why
A home-screen icon or bookmark for `/signin?return_to=/read/` showed the password form on every launch, which looked like being signed out.

## No redirect loop
The reader on a 401 shows its own signed-out screen with a **Sign in** link, and the console shows its login card; neither navigates to `/signin` by itself. A session `/signin` accepts is the one `/api/v1` accepts, so the page it redirects to does not bounce back.

## Added
- Unit tests in `src/http/signin.rs`: redirect with `no-store` (including `notice=reset` and the default), no open redirect for hostile `return_to`, form for no/unknown/idle/expired/revoked sessions, form on a store error then redirect once healed.
- Journey `a_signed_in_browser_opening_the_sign_in_link_lands_in_the_reader` in `tests/ui_e2e/read.rs`.
