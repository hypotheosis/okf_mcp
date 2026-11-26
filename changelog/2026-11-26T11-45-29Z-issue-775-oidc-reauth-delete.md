# Issue 775: delete your own account with an OIDC re-authentication

## Added
- `POST /api/v1/me/identities/{provider}/reauth`: starts a fresh round trip
  to the provider the user has linked (`prompt=login`, `max_age=0`), as a
  flow of its own kind (`Flow::reauth`, a `ReauthRequest` bound to the
  console session's hash and to the subject linked at that provider).
- The same callback route finishes it, but only to keep a `ReauthProof`
  (`oidc:reauth` key space, keyed by the session's hash, 5 minutes,
  single use). It creates no session, links nothing and records no sign-in.
- `DELETE /api/v1/me` accepts `{"oidc_reauth": true}` as a third proof;
  `users::Proof::Oidc { provider, subject }` is rechecked under the user's
  row lock by `users::delete_own` (the identity must still be linked).
- The ID token must carry an `auth_time` within
  `oidc::REAUTH_AUTH_TIME_WINDOW` (5 minutes) of the callback and at most a
  minute ahead of the server clock; checked inside the crate's own ID-token
  verification (`set_auth_time_verifier_fn`), after the signature, issuer,
  audience, nonce and expiry checks. A missing `auth_time` is refused.
- Account page: the delete section offers "Re-authenticate with <provider>"
  for a linked account, then "Delete my account" after the return, instead
  of the "set a password or add a passkey first" note.
- The mock provider (`oidc/testing.rs`) can set `auth_time` and records its
  authorize requests.
- `identities::subject_of`.

## Changed
- `OidcProviders::authorize_url` takes a `fresh` flag; `identify` and the
  new `identify_fresh` share one verification.
- `Flow` has a `reauth` field (`#[serde(default)]`, so an older stored flow
  reads as a sign-in or link). A session holds one link or re-authentication
  flow at a time (the link index is shared).
- `docs/operations.md`: "Deleting your own account", the OIDC section, the
  Redis key table and the API table.

## Notes
- Rolling deploy: a replica from before this change that receives a
  re-authentication callback ignores the unknown `reauth` field and treats
  the flow as a sign-in, which signs the person in as themselves (no deletion
  proof is made). Deploy all replicas before relying on the button.
- A refused deletion (409, 403 after a concurrent change) still consumes the
  proof, as a passkey ceremony does; the person re-authenticates again.
- The provider id stands for the issuer: identities are keyed by
  `(provider id, sub)` and a provider's issuer is fixed by its config entry.

## Added (maintainer requirement, same issue)
- `OkfError::ReauthStale { provider }` (kind `ReauthStale`, HTTP 403): the
  re-authentication's ID token was genuine but its `auth_time` was missing,
  too old or in the future (`OidcError::AuthTimeStale`). The callback keeps
  a `stale` marker (no subject) under the session; `DELETE /api/v1/me`
  spends it and answers with the provider's display label and the
  instruction to sign out of the provider, sign back in and try again.
  Issuer, subject and token details never appear.
- Account page: that message in `#account-delete-reason` (`data-kind`
  `ReauthStale`) and a "Try again" re-authenticate button; other refusals
  stay generic.
