# Issue #778: the Account page numbers its list reads

## Changed
- `drawSessions`, `drawPasskeys` and `drawIdentities` take a number from the shared `latest()` helper before they read, and draw nothing when a newer read of the same list has started, so a late older answer no longer redraws over a newer one.

## Added
- ui_e2e journeys that hold the older `/me/sessions` and `/me/passkeys` answers and release them after a newer write's read has drawn.

## Notes
- No journey for `drawIdentities`: it uses the same helper, and a journey needs two providers linked to hold one unlink's read across another; not written here.
- `drawAccount` is also reloaded from several paths without numbering; left for a follow-up.
