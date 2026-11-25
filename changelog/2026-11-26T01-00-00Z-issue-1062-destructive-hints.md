# Issue 1062: destructiveHint follows the MCP spec

The ChatGPT directory review rejected 11 tools marked `destructiveHint: false`
that overwrite or remove existing content. The spec says `false` means the tool
performs only additive updates; being staged in a changeset (undone by
`discard`) does not change that.

`src/mcp/annotations.rs` splits `STAGED` into `ADDITIVE` (9 tools, stay `false`)
and `STAGED_DESTRUCTIVE` (12 tools, now `true`): `edit_document`, `replace_body`,
`set_frontmatter`, `remove_source`, `unquestion`, `revert`, `review`, `stamp`,
`index`, `repair_indexes`, `move_concept`, `split_concept`. `send_feedback`
(`EXTERNAL`) is now `destructiveHint: true` as well. `metrics::tool_label` chains
the new lists, so every tool still has a label.

The `#[tool(...)]` hints are changed to match. `edit_document`, `replace_body`
and `set_frontmatter` now say they are staged and that `discard` drops the edit.
The generated kind in `docs/reference/tools.md` is renamed `staged` to
`additive`, and `destructive` is defined as "may overwrite or remove existing
content, staged or not". The README counts are rewritten from the code (20 reads,
9 additive, 17 destructive, `send_feedback`).

Not done here: re-submitting to the ChatGPT directory.
