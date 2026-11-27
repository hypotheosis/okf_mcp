# Issue 925: revoke the grant when the authorization code cannot be stored or generated

`finish` in `src/oauth/authorize.rs` now calls `revoke_unused_grant` on the two remaining paths
where no code is minted after the grant exists (#589 handled the failed pending removal):

- the code or family id cannot be generated (`server_error`);
- the code is not stored (full map or Redis failure, the 503 busy page).

Only the grant this request created is revoked, by its id (`redeem` and `create_self_issued` both
insert a new row), so a grant from an earlier consent is untouched. Responses are unchanged. The
code takes a clone of the id so the id remains for the revoke. The comment above the insert no
longer says the grant stays.

Test: `a_refused_code_insert_revokes_the_new_grant` (standalone and cluster Redis, `SET` refused
with `NOPERM`) asserts the busy page, no code, and that only the earlier grant stays live. The
earlier-consent setup of `a_failed_removal_answers_the_busy_page` moved into shared helpers.

Review round 1: the generation failure now has tests. `mint_ids` in `src/oauth/authorize.rs` generates
the code and the family id (same order and errors as before); a test build lets
`OAuth::force_generation_failure(ForcedGenerationFailure::Code | FamilyId)` fail the next call, a
per-`OAuth` atomic like `SessionStore::fail_next_store`, compiled only under `cfg(test)`, so production
code and its performance are unchanged. `a_failed_code_generation_revokes_the_new_grant` and
`a_failed_family_id_generation_revokes_the_new_grant` assert the 500 page, no code, one newly revoked
grant and the earlier grant still live.
