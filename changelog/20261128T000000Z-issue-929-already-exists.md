# Issue 929: a taken path is `AlreadyExists`, not `InvalidPath`

`create_concept` on a path that already holds a concept used to answer `InvalidPath` ("already
exists"), which a client branching on `error` could not tell from a malformed path. It now answers
`AlreadyExists`, carrying `path` and a message that names the recovery: `edit_document` or
`replace_body` changes the concept, `get_concept` reads it, or choose another path.

BREAKING for clients: a client that matched `InvalidPath` for this case must match `AlreadyExists`.
`InvalidPath` stays for malformed, escaping, reserved and machinery paths. Over REST the variant is
409 (`AppError::status`); no REST route creates concepts, so it surfaces there only through a route
that returns an `OkfError` from a write path.

Decision: the other taken-target sites move too, since they share `new_concept_path` or the same
check: `move_concept` (and `Workspace::rename`) and `split_concept`'s `to`. The one message covers
them ("to put another concept there, choose another path or delete or move the existing one
first").

- `OkfError::AlreadyExists { path }` in `src/error.rs` (kind, JSON body with a `/`-prefixed `path`).
- `new_concept_path` (`src/okf/paths.rs`) and `MemWorkspace::rename` (`src/okf/workspace.rs`) return it.
- `create_concept`'s description says `path` must not exist yet and names the edit tools; its
  31-character opening is unchanged.
- `docs/reference/tools.md` regenerated; `docs/operations.md` troubleshooting table and `README.md`
  name the new code.

Tests: `error::tests::a_taken_path_is_already_exists_not_invalid_path_and_names_the_recovery`,
`http::tests::a_taken_path_is_a_conflict_and_a_malformed_one_a_bad_request`,
`mcp::writes::tests::create_concept_on_an_existing_path_is_already_exists_and_a_bad_path_is_not`,
the `move_concept` and `MemWorkspace::rename` taken-target tests, the description assertions in
`the_write_tools_have_object_output_schemas...`, and an mcpjam scenario in
`careful-fact-curator.test.ts`.

Review round 1 (Brooke): `Workspace::exists` is true for a directory too, so a directory named like a
concept (`target.md/child.md`) answered `AlreadyExists` and advised editing a concept that does not
exist. `new_concept_path` now answers `AlreadyExists` only for a file (`is_file`); a directory is
`InvalidPath` "is a directory", the same wording `check_placement` and `MemWorkspace::rename` use for
it (before this PR `new_concept_path` said "already exists" for both). `rename` and `split_concept`'s
`to` were already right or go through the helper. Tests with a `.md` directory: `a_directory_named_like_a_concept_is_invalid_path_not_already_exists`
(paths and moves), `create_concept_refusals`, `a_split_target_taken_is_already_exists_and_a_directory_is_invalid_path`;
all four fail with `exists` restored.
