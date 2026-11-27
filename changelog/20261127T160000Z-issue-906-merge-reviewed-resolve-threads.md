# Issue #906: merge-reviewed.sh resolves answered bot review threads

## Changed
- `tools/agentic/merge-reviewed.sh`: when the PR is `BLOCKED` and every bot thread has a reply, it lists the review threads (GraphQL `reviewThreads`, all pages) and resolves the unresolved ones whose first comment is the bot's (`resolveReviewThread`). It then re-reads `mergeStateStatus` until it is no longer `BLOCKED` or `UNKNOWN`, for at most `AGENTIC_STATE_WAIT` seconds (default 60, counted from the last resolve). An `UNKNOWN` state is waited out the same way.
- Refusals say what was checked: a person's unresolved thread is named (file and author) and never resolved; a `BLOCKED` state with no open thread says the block is something else; a state that stayed `BLOCKED` after resolving names the wait.
- The wait is bounded: each lookup and sleep is capped by the time left, so it never overruns `AGENTIC_STATE_WAIT`.
- `tools/agentic/tests/stub/gh`: `api graphql` (thread listing and resolving) and a `merge-state-seq` fixture for a state that changes between reads.
- `tools/agentic/README.md`: the merge steps and the new variable.

## Added
- Eleven cases in `tools/agentic/tests/run.sh` (resolve and merge, paging, unanswered thread resolves nothing, a person's thread, `UNKNOWN` then `CLEAN`, `UNKNOWN` forever, a slow state lookup does not overrun the wait, blocked for another reason, blocked after resolving, thread read failure, resolve failure). With `merge-reviewed.sh` reverted to `origin/main`, 10 of the 11 fail; the unanswered-thread case passes on both.

## Notes
- Threads are only listed when the state is `BLOCKED`, so a `CLEAN` PR (a milestone branch) is untouched and `merge-if-ready.sh` is unchanged.
