# Account journeys cannot skip the loaded wait (issue #785)

## Added
- `tests/ui_e2e/account.rs`: a guard test that fails on any raw `ui.open("/account")` not preceded by a `// raw-open:` comment giving the reason. A new journey now goes through `open_account` (which waits for `.view[data-loaded=true]`) or says why it must not.

## Changed
- The four journeys that open the page raw on purpose (two held reads, one failed read, a key's refused session) carry the `// raw-open:` comment.

## Notes
- The races in the issue were fixed on `main` by #500 (`open_account` in every account journey, the harness wait raised to 60 s, `localhost` mapped to IPv4). Before this change, 10 repeats of the 23 `account::` journeys alone passed 10 of 10; the #431 journey included.
- The OIDC discovery stub (`src/oidc/testing.rs`) answers 503 at once and has no timeout of its own; the 10 s that mattered was the harness wait, now 60 s. No stub change was needed.
- A `Detached` click on `#confirm-ok` cannot come from a page redraw: `confirmDialog` appends its dialog to `body`, outside `#main`. The `Detached` of #431 was the click on the passkey row's remove button, which `open_account` covers. No harness retry was added.
- `signin::an_account_sign_in_replaces_a_stored_key_and_sign_out_ends_both` is a different cause (a `localStorage` read before the console drops the key) and stays with #885.
