# Account page: number the /me reads (issue #1108)

## Added
- `a_late_older_account_read_does_not_redraw_over_a_newer_one` in `tests/ui_e2e/account.rs`: a password change starts a `/me` read whose answer is held, a rename draws the new name, and the held older answer must not bring the old name back.

## Changed
- `drawAccount` in `src/http/ui/js/pages/account.js` takes a number from `latest(main)` before its read and draws nothing when a newer read has started (or the page was left), as `drawSessions`, `drawPasskeys` and `drawIdentities` do (#778).

## Notes
- The reauth status read (`checkReauth`, `GET /me/identities/<provider>/reauth`, added by #1099) runs once per page load, guarded by the provider it started for, so it cannot overlap itself and needs no number.
- Callers of `drawAccount`: the first load, the display-name save, a team rename and a password change or set.
