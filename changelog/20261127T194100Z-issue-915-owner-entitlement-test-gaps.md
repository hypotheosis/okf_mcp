# Issue #915: owner entitlement tests, a catalog moved mid-request and an exact refusal

Tests only; no production code changed.

## Added
- `src/store/ceiling.rs`: `an_entitlement_on_a_catalog_moved_to_another_team_while_it_waits_is_refused`. It holds `server_state` so `catalogs::set_owner` updates the catalog row and waits uncommitted, proves the row is locked (`FOR UPDATE NOWAIT` fails), then starts the owner's entitlement, which passes the owner and membership checks on the old state and waits on the catalog row's share lock. Releasing the hold lets the move commit; the entitlement is then refused with `Forbidden` and no `user_catalogs` row exists. Ordering comes from lock-waiter counts, as in the neighbouring demotion and member-removal tests, with no sleeps. It runs in both `OKF_TEST_STORE` modes.

## Changed
- `src/http/api/owner_access_tests.rs`: an owner's API key and OAuth grant on the owner routes must answer exactly 403 with `"error": "Forbidden"`, as docs/operations.md says, not `403 | 404`.

## Checked
- Removing the owner-team comparison in `begin_entitlement` makes the new test fail (the entitlement succeeds).
- Making the owner routes answer 404 for a key makes the tightened assertion fail.
