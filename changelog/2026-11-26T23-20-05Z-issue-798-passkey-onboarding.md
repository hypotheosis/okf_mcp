# Issue 798: passkey setup follows the onboarding guide; feedback limit wording

## Added
- `AfterSetup::target`, so a script-driven caller can take the destination as one string.
- `feedback::Caller` (`Credential` or `User`) on `Sender` and `NewFeedback`.

## Changed
- `POST /auth/passkey/setup/finish` answers its `redirect` through `signin::after_setup`, the rule a password setup uses: the `onboarding_url` when set, an OAuth `/oauth/authorize?` continuation always winning, else the validated `return_to`.
- The feedback rate-limit refusal says "you have sent ..." for the console endpoint and keeps "this credential has sent ..." for the MCP tool.

## Notes
- The redirect target is the operator's own `https://` setting (validated when stored); `return_to` still passes `safe_return_to` at the ceremony start. `passkey.js` navigates with `location.assign`, which the setup form's `form-action 'self'` does not govern.
