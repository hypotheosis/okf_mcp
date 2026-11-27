# Issue 1174: front-load tool descriptions and add a `guide` tool

Clients that defer tools (the Claude app) show the model only `name - <first 31 characters of
the description>` until it loads a tool, and drop the server instructions.

- New read-only tool `guide`: returns the server's instructions. Its description opens "Read
  first: how to use <product>" (the `product_name` setting, as `send_feedback` does). The text is
  `compose_instructions` in `src/mcp/mod.rs`, called once per session; `get_info` and `guide` both
  read that one stored string. 47 tools now (21 read-only).
- Every tool description now opens with a plain-words sentence of at most 31 characters saying
  what the tool is for, before any mechanics, for example `create_concept` "Save what the person
  wants kept", `search` "Recall what the person told you", `open_changeset` "Step 1 of saving to
  memory", `publish` "Final step of saving to memory". The old text follows unchanged except for
  `create_concept`, `search` and `send_feedback`, whose first sentences were reordered or merged
  so "save", "remember", "recall" and "notes" come early.
- Test rule (`src/mcp/openings_tests.rs`): every tool has an approved opening in a list (a new
  tool fails until it adds one), the description starts with it, it fits 31 characters, ends
  at a sentence or word boundary, and the shown characters hold no code, path or parameter
  marks. The save and recall openings are pinned exactly.
- `tests/mcpjam/scenarios/tool-descriptions.test.ts` builds the deferred view (`name - first 31
  characters`) from the live tool list and checks it. The MCPJam gate runs no model, so it cannot
  show a model reaching a write tool; that is a manual Claude app run.
- `docs/reference/tools.md` regenerated; README tool counts updated.
