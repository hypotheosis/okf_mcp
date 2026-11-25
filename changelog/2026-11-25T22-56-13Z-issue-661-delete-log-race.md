# Issue #661: document the delete/log race, fail hard on impossible states

Decision (issue #661): keep the behavior (option a). `delete_concept` records
a `log.md` unlink only when its own snapshot has a link. When a `log` entry
linking the concept publishes from another changeset first, the delete's
publish is refused (`LintFailed`, rule `link-into-deleted`), and the same
delete in a fresh changeset succeeds.

- `delete_concept`'s tool description and `docs/reference/tools.md` now say so
  and tell the agent to run the delete again in a fresh changeset.
- `unlink_logs`: the two "unreachable" `continue` branches are now internal
  errors (`OkfError::Io` through `invariant_broken`). Only the "file with no
  link" branch is testable (a hand-built `logged` list); the empty-run branch
  cannot be reached, because `chunk_by` never yields an empty run.
- New test `a_delete_refused_by_a_racing_log_link_succeeds_when_retried`
  (`tests/restructure.rs`) and a unit test in `src/okf/ops/delete.rs`.
