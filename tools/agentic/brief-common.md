# Shared brief for implementation agents

The coordinator gives each agent a short task brief (the issue, its worktree and branch, the plan task, and what runs in parallel) and points it at this file. Everything here applies in full.

## Project rules
- **The plan is authoritative.** The milestone's plan under `docs/superpowers/plans/` governs, with the spec it names. Read its shared sections first (from the top through "Task dependencies"), then only your own task: `grep -n '^### Task' <plan>` finds it.
- **The plan's code has not been compiled.** Where it disagrees with `main` or with earlier merged tasks, `main` wins. Fix the plan in the same PR, inline or with a dated `> **Corrected (issue #N).**` note, then grep later tasks for the changed symbol and fix them too.
- **Read every comment on your issue before starting.** Earlier tasks leave heads-ups there.
- **Read `tools/agentic/lessons.md`** and follow every rule that applies. Each one was a real bug found in review.
- **Code rules:** follow `.claude/CLAUDE.md`:
  - doc comments with Arguments, Returns and Errors;
  - comments explaining Rust-specific constructs for a Python-expert reader;
  - no `.unwrap()` outside tests;
  - `thiserror` errors;
  - no emoji.
- **Changelog:** one entry per task, `changelog/<UTC ts>-m<N>-task-<NN>-<slug>.md` (letter tasks use `07a`), with `## Added`, `## Changed` and `## Notes`. Get the timestamp from `date -u +%Y-%m-%dT%H-%M-%SZ`.
- **Test databases:** each DB test starts a throwaway Postgres container, so Docker is needed. Re-run a failing test alone before concluding the failure is yours.
- **Builds:** build and test with `SQLX_OFFLINE=true`, which the justfile exports. Never set `DATABASE_URL` while building.
- **Browser tests** (`just test-ui`) use rustwright. Leave `RUSTWRIGHT_CHROMIUM` unset.
- **No real infrastructure details anywhere:** no real hostnames, domains, cluster names, IPs, personal names or emails in files, tests, commits or issues. Use placeholders such as `alice`, `example.com` and `example.internal`.

## Gates (all must pass before you push)
Run every gate through `tools/agentic/gate.sh`. It takes the cross-worktree lock for heavy gates and applies a hard timeout:

```
tools/agentic/gate.sh fmt
tools/agentic/gate.sh lint
tools/agentic/gate.sh test
tools/agentic/gate.sh test-ui              # if you touch the console or tests/ui_e2e/
tools/agentic/gate.sh dylint               # required before every push (the lint is at deny)
tools/agentic/gate.sh mcpjam               # required before every push (the MCPJam scenarios, tests/mcpjam/)
tools/agentic/gate.sh build-release       # before every push that touches src/, Cargo.toml, Cargo.lock, build.rs, migrations/ or the Dockerfile (the image's release build)
tools/agentic/gate.sh worker               # the reader's Node tests (tests/read_worker/), while iterating
tools/agentic/gate.sh narrow --lib -- <filter>   # while iterating
```

- **`dylint`** runs the `uninstrumented_fn` policy lint (`docs/contributing/policy-lints.md`). The lint is at deny: a new function needs `#[instrument]` or a reason-bearing exemption, and the gate must exit 0 before you push. The first run builds the lint library (minutes).
- **`mcpjam`** replays the MCPJam swarm goals as fixed MCP tool calls (`tests/mcpjam/README.md`) against a server built from your checkout, on a throwaway Postgres. It needs Docker and Node 22+, and must exit 0 before you push. A red scenario is a change in what the server stores or answers: fix the server, or, if the change is intended, the scenario.
- **`build-release`** runs the Dockerfile's build step, `cargo build --release --locked --bin okf-mcp`, with the rustc the Dockerfile pins (`cargo +<version>`). The dev and test profiles do not show every compile error: rustc's query depth limit on a deep async future appeared only in the release build, and no image built for a day (#1227). `cargo check --release` does not show it, and a newer rustc did not either. It takes 2 to 5 minutes warm and needs the pinned toolchain installed (`rustup toolchain install <version>`). Run it for every change that touches `src/`, `Cargo.toml`, `Cargo.lock`, `build.rs`, `migrations/` or the `Dockerfile`; skip it for docs, `tests/` only, `tools/`, `scripts/` and the reader.
- **Exit 124 means a test hung.** It is neither a pass nor an ordinary failure, so find the stuck test before anything else (the script prints how).
- **Flaky tests retry once inside the run** (`retries = 1`, `.config/nextest.toml`). A test that passes on its second try shows as `TRY 2 PASS` and in the summary's flaky count. It is not a clean pass:
  - paste every FLAKY line into your report;
  - a flaky test that already has an open issue (search `gh issue list --search "<test name>"`) needs nothing more;
  - a flaky test with no open issue may be a race your change introduced. Run it narrowly a few times (plain repeats, no stress loops), and either fix it or report it as a candidate follow-up with what you found.
- **One green full `test` run on the head you push is enough.** Don't re-run a green gate to collect more evidence unless your brief asks for it. A review round that changes only tests needs the narrowed module (`gate.sh narrow`) plus one full run; a round that changes only the PR description or `claims.json` needs no gate run.
- **Wait for a long gate before you hand back.** Wait with one bounded wait (Monitor, or a single `timeout`-bounded `until` loop whose PID you record), never by handing back mid-gate.
- **Bound every wait.** Never run tests outside `gate.sh`, and never wait on a background job without a bound.
- **Tests you write that hold a lock or wait on another task** wrap the wait in `tokio::time::timeout`.

## Shared machine
- Write logs only inside your worktree, for example `target/i<N>-logs/`, and take test counts from your own logs.
- Never kill processes by pattern (`pkill -f`, `killall`). Kill only PIDs you started.
- Don't start extra parallel builds.

## When main moves
Run `git merge origin/main`. Never rebase or force-push. Keep both sides of a conflict, then re-run the gates before pushing. The review bot skips a PR that is behind `main`.

**Before asking for a review or a re-review** (`/prrb`), always bring the PR up to date with `main` first: merge `origin/main`, re-run the gates, push, and only then ask. The bot skips a PR that is behind `main`, so a request made before that is wasted.

## Design-only start
A coordinator may start you while the previous issue's PR is still in review. Your brief then says **design-only**. Until the coordinator says go:
- read the issue, the code and the plan;
- write your design;
- you may draft code and tests in your worktree;
- run no `gate.sh` gates, no cargo builds or tests, and no Docker;
- don't push or open a PR.

Report the design (approach, files, tests, open questions) in one hand-back and wait. When the coordinator says go, merge `origin/main` first, because the previous PR has just merged, then build, gate and push as usual.

## The agentic test suite
`tools/agentic/tests/run.sh` runs 4 tests at a time (`AGENTIC_TEST_JOBS=1` for one at a time) and takes a name-substring filter (`run.sh watch_stalled`); keep the suite well under its 300 s cap (about a minute), and give a new test's waits configurable, small values.

## Review replies
When you address a review, reply in each inline thread (as a reply to that comment), not only in a top-level comment. `merge-reviewed.sh` refuses to merge while any reviewer thread has no reply. Keep the PR description and its claims in step with the final code after every round.

## Review rounds
A PR gets at most 3 review rounds from the bot (`AGENTIC_MAX_REVIEW_ROUNDS`).
- In rounds 1 and 2, address all comments.
- In the final round (round 3), fix only correctness, security and data-loss findings. File everything else (style, extra tests, wording, refactors) as a follow-up issue labelled `review-followup`, and reply in each thread with the issue link.
- Then the coordinator merges with no further review request.

## When done
- Don't merge.
- End commit messages with the attribution line the coordinator gives you, and use `Closes #<N>` in the PR body.
- Report:
  - the PR URL and head SHA;
  - gate results, with test counts from your own logs;
  - plan divergences, and which plan sections you corrected;
  - what you verified by reverting a fix to see its test fail.
- **Settle every risk you name before the review sees it.** If your report or PR names a residual risk, a gap, an untested case or a claim stronger than its evidence, either fix it in this PR or say in the PR body why it is out of scope (and list it as a candidate follow-up). The review bot reliably flags a risk the PR names but leaves open, and a review round costs 15-40 minutes (#1183 took two such rounds). Word claims no stronger than the tests behind them.
- Keep your context: the coordinator will message you when the review lands.
- Don't hand back while your own background work is still running: finish, push, then report.
