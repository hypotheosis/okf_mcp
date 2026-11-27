# lessons.md: checklist for a new OkfError variant (issue #1100)

`tools/agentic/lessons.md` ("Inputs and errors") now lists what a new `OkfError` variant needs.

- **Compiler-enforced, because none has a `_` arm:** an arm in `OkfError::kind`, in `OkfError::to_json` and in `AppError::status`.
- **Checked by hand:**
  - the doc comment and `#[error]` message;
  - whether `is_internal` should list it;
  - a `src/error.rs` test of its kind and JSON body;
  - an HTTP test of its exact status;
  - `docs/operations.md`, plus `docs/reference/tools.md` when an MCP tool can return it;
  - any console code that branches on the error kind.

Why: wiring `ReauthStale` (#775, PR #1097) meant finding these sites one at a time.

Documentation only; no code changed. The issue's optional automatic test (every variant appears in the docs) was not added. The docs name variants in prose, by route, not in one table, so there is nothing to check them against yet.
