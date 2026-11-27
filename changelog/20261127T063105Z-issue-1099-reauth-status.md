# Account deletion: the stale-reauth guidance shows on return from the provider (issue #1099)

## Added
- `GET /api/v1/me/identities/{provider}/reauth`: a read-only, console-session-bound status of this session's provider re-authentication. Answers `{"state": "none" | "ready" | "stale"}` and nothing else. It never takes the proof, writes nothing and does not extend its lifetime. 403 for a key, a grant or no console session; 404 for an unknown provider or with OIDC off.
- `Ephemeral::peek` (and `ExpiringMap::peek`, `Oidc::peek_reauth_proof`): a read that neither removes nor renews an entry (a plain `GET` in Redis).
- The Account page asks the status after the return from the provider: `stale` shows the sign-out/sign-in guidance with "Try again" at once and no delete button; `ready` shows the delete button; while the answer is pending a "Checking" line replaces the button.
- Tests: the status never spends the proof (a delete after `ready` succeeds); `stale` for no, old and future `auth_time` with the marker intact for the delete; another session sees `none`, no session is refused, an unknown provider is 404; `peek` in memory and Redis (standalone and cluster) leaves the value and its lifetime alone.

## Changed
- `take_reauth` and the new status share one function, `judge_reauth`, for the session binding and the stale check, so the status cannot disagree with the deletion. The `auth_time` window is still judged once, at the callback (`Oidc::identify_fresh`); its verdict is the proof's `stale` flag.
- `reauth_start` and the status share `reauth_session` for the console-session check.
- The ui_e2e journey for a provider with no `auth_time` now expects the guidance before any delete attempt (it used to click through the confirmation first).
- `docs/operations.md` documents the endpoint and the page's behaviour.

## Notes
- The page keeps the old path as a fallback: if the status call fails, it offers the delete button and the deletion answers `ReauthStale` as before.
- A proof stored for another provider than the one in the path reads as `none`.
