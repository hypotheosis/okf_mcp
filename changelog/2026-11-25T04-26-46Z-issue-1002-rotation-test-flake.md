# Issue #1002: store credential rotation tests no longer race a stray refresh

## Added
- `a_refresh_is_applied` and `until_ok` test helpers in src/stores/registry.rs:
  a bounded (60 s) wait for the credential task's refresh counter, and a
  bounded retry of a read until it connects under the refreshed login.

## Changed
- Four rotation tests (`a_refused_login_on_an_open_pool_fetches_a_fresh_one_...`,
  `a_secrets_manager_store_opens_and_follows_a_rotated_secret`, and the shared
  body of `a_plain_read_refused_after_a_rotation_...`,
  `a_role_with_a_quotation_mark_...`, `a_role_with_two_adjacent_...`) now
  rotate in a fixed order: the source (secret or clock) moves to the new
  login first, then the role takes a password that matches neither login, the
  refused read is asserted, the refresh is awaited, and only then does the role
  take the new password. A final bounded retry confirms the next read works.
- `secrets.calls` and `signer.calls` are asserted `>= 2` instead of `== 2`.

## Why it flaked
The refusal channel (`LOGINS_REFUSED`) is process-wide, and a refusal whose
message names no single role (the quotation-mark tests make one, as does any
message without a quoted name) wakes every credential task. A stray refresh
between the test's `set_password` and its `secrets.set` fetched the old
secret, so the refresh counter reached 1 early and the next read still used
the old login (28P01, registry.rs:1836 and 1958). A stray refresh between the
clock moving and `set_password` gave the pool the new token before the
database wanted it, so the `begin` that should have been refused succeeded
(registry.rs:1781).

## Notes
- Cost: none in the normal path. A refresh is immediate unless one just ran,
  in which case the keeper's 10 s `MIN_REFRESH_GAP` applies; the 60 s bound
  covers it.
- Lost strictness: "the refused attempt and nothing else caused the second
  signature" (`== 2`) cannot hold when a stray refresh may also sign.
- Not changed: production retry behaviour (#998).
