# Issue 1186: the integration tests build as one test binary

`tests/` built 25 integration-test binaries (`tests/*.rs` plus `tests/ui_e2e/`), each linking the
whole `okf_mcp` lib and every dependency and each compiling its own copy of `tests/support/`. They
are now one binary, `it` (`tests/it.rs`), with a module per former test file.

## Changed

- `Cargo.toml`: `autotests = false`; `[[test]] it`.
- `tests/it.rs`: declares `support` once, every former binary as a `#[path]` module and
  `ui_e2e`; carries the one `recursion_limit = "256"`.
- Test files: `mod support;` became `use crate::support;`; `tls.rs`'s and `ui_e2e`'s `#[path]`
  support modules and `crate::<module>` paths inside `ui_e2e/` follow.
- `justfile` `test-ui`: `cargo test --test it ui_e2e:: -- --test-threads=2`.
- `README.md` and `skills/README.md`: every `--test <name>` command is now `--test it <name>::`.

## Notes

- `telemetry_install`, `telemetry_sdk_disabled` and `cimd_no_proxy` stay their own `[[test]]`
  targets. They set the global tracing subscriber, the global meter provider and environment
  variables. nextest gives every test its own process, but plain `cargo test` runs a binary's tests
  as threads of one process: run in `it` together with `telemetry_privacy`, six of the nine tests
  failed. The reason is recorded in `tests/it.rs`.
- A libtest filter is a substring match: `parity::` also matches `admin_parity::` and `smoke::`
  matches `ui_e2e::smoke::`. The README's `--ignored` commands are unaffected.
- `cargo nextest list`: 3775 tests before, 3760 after. The only difference is
  `support::redis::tests::random_prefixes_have_the_documented_shape_and_never_repeat`: 17 integration-test
  copies before, 2 after (`it`, and `telemetry_install`, which keeps its own `support` because it
  is a separate target); the lib's own, different test of that name is unchanged. That is 15
  fewer, not the 16 the issue expected.
- Tests that re-run the test binary (`current_exe` with `--exact`) name their child by its module
  path now (`investigator::journal_process::...`, `test_database::template_race_child`).
  Unprefixed, the child matches no test under `it`; the journal tests and the template race then
  failed, and `investigator_config_secrets_environment_isolated` would have passed without running
  its child.
- Timings: the issue's benchmark table stands; local timings were skipped, the machine was
  saturated.
