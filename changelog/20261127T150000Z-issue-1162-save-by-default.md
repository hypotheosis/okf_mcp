# Issue 1162: steer clients to save to the catalog by default

- Server instructions: the opening no longer lists "systems, projects, people and decisions"; it
  says catalogs keep whatever the person may want again in a later conversation. The remember
  rule moved to the top and now reads save-then-tell: save what the person shares without asking
  first and say what was saved. "Don't save what they didn't ask you to keep." is gone.
- `search`, `create_concept` and `open_changeset` descriptions say the catalog is where to
  remember things. "Visible to others" became "nothing is published until `publish`; a published
  concept is visible only to those with access to the catalog (a personal catalog's owner unless
  they share it, a team catalog's team)". "Those with access" is the general clause: a
  catalog is reached through a user, a team or a key's grant.
- `get_concept` on an `index.md` names `read_document`. `history` says an old revision of an
  `index.md` has no direct reader (`revert` into a changeset shows it as a draft).
- A `NotFound` from the content and bookkeeping write tools (those in `src/mcp/writes.rs`) adds: retry once a parallel call that creates the path returns.
- `docs/reference/tools.md` regenerated. New unit tests and `tests/mcpjam/scenarios/tool-descriptions.test.ts`.
