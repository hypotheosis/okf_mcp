# Agentic workflow: shorter time to merge

## Changed
- `.config/nextest.toml`: every test gets one retry inside the run (`retries = 1`). A transient failure now costs one test's time instead of a full re-run of the test gate (about 4.5 minutes, plus an agent turn). A green run costs nothing extra. A pass on the second try is reported as FLAKY, and a hung test now takes up to two slow-timeouts (10 minutes) to fail.
- `tools/agentic/brief-common.md`:
  - FLAKY results are reported, and a flaky test with no open issue is investigated as a possible race the change introduced.
  - One green full test run on the pushed head is enough. A round that changes only tests needs a narrowed run plus one full run, and a round that changes only the description or claims needs no gate run.
  - Wait for a long gate before handing back.
  - A risk the report or PR names is fixed or explicitly scoped out before review, and claims are worded no stronger than their tests.
  - New "Design-only start" section.
- `AGENTS.md`: a coordinator may start the next issue design-only while the current PR is in review, with no gates, builds or Docker until it merges. Coordinators ask for one full test run per round, not repeated runs.

## Notes
- Measured over the 24 hours to 2026-11-28T13:00Z: about 47 minutes from one merge to the next. Full-gate re-runs for flakes, and review rounds, were the largest costs after the cargo-warm seeding (#1185) and line-tables-only debug info (#1209).
