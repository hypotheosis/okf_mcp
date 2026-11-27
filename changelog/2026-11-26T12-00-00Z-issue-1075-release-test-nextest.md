# Issue 1075: release tooling tests stub nextest

`scripts/test_release.py` was red (9 failures, 7 errors of 35) since `just test` moved to
`cargo nextest run` (#1069): the fixture's fake cargo stubbed only `fmt`, `clippy`, `doc` and
`test`, so `cargo nextest run` reached the real cargo-nextest.

- The fake cargo now treats `nextest` as the `test` check, so `FAIL_CHECK=test` fails both the
  nextest run and the `cargo test --doc` step. `cargo nextest --version` (the probe in
  `just test`) succeeds even then.
- New recipe `just test-release` and gate `tools/agentic/gate.sh release` (5m, no lock) run the
  suite, so a justfile change that breaks it is caught.
