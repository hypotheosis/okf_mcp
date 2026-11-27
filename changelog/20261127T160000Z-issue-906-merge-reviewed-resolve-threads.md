# Issue #906: merge-reviewed.sh resolves answered bot review threads

## Changed
- `tools/agentic/merge-reviewed.sh`: when the PR is `BLOCKED` and every bot thread has a reply, it lists the review threads (GraphQL `reviewThreads`, all pages) and resolves the unresolved ones whose first comment is the bot's (`resolveReviewThread`). It then re-reads `mergeStateStatus` until it is no longer `BLOCKED` or `UNKNOWN`, for at most `AGENTIC_STATE_WAIT` seconds (default 60, counted from the last resolve). An `UNKNOWN` state is waited out the same way.
- Refusals say what was checked: a person's unresolved thread is named (file and author) and never resolved; a `BLOCKED` state with no open thread says the block is something else; a state that stayed `BLOCKED` after resolving names the wait.
- The wait is bounded, but `AGENTIC_STATE_WAIT` is a polling budget, not a hard limit: no sleep or new lookup starts after it, and each lookup and sleep is capped by the time left. The last lookup may run up to 2 s past it (1 s of grace, plus up to 1 s from the whole-second clock), and stopping that lookup's process group takes up to `agentic_kill_after` + `agentic_reap_after` (5 + 5 s) more. So the wait ends within `AGENTIC_STATE_WAIT` + 12 s (72 s by default). Process-group cleanup is unchanged: a lookup that runs out of time is stopped with its whole group (a process that survives KILL is abandoned with a warning, as everywhere in `lib.sh`).
- `tools/agentic/tests/stub/gh`: `api graphql` (thread listing and resolving) and a `merge-state-seq` fixture for a state that changes between reads.
- `tools/agentic/README.md`: the merge steps and the new variable.

## Added
- Eleven cases in `tools/agentic/tests/run.sh` (resolve and merge, paging, unanswered thread resolves nothing, a person's thread, `UNKNOWN` then `CLEAN`, `UNKNOWN` forever, a slow state lookup that ignores TERM ends within the documented bound, 6 s for a 2 s wait with 1 s kill and reap settings, plus a stated 2 s tolerance, and is cut off with no process left, blocked for another reason, blocked after resolving, thread read failure, resolve failure). With `merge-reviewed.sh` reverted to `origin/main`, 10 of the 11 fail; the unanswered-thread case passes on both.

## Notes
- Threads are only listed when the state is `BLOCKED`, so a `CLEAN` PR (a milestone branch) is untouched and `merge-if-ready.sh` is unchanged.
