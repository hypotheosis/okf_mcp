# Debug builds keep line tables only

Generated: 2026-11-28T10:57:57Z

Debug and test builds wrote gigabytes on every relink, which kept the
workstation's NVMe busy while several worktrees built and tested at once
(`/proc/pressure/io` showed tasks fully stalled on I/O about 35% of the time
over five minutes). Most of those bytes were full debug info in the test
binaries.

## Changed

- `Cargo.toml` sets `[profile.dev] debug = "line-tables-only"` and
  `[profile.dev.package."*"] debug = false`. Backtraces and panics still name
  file and line in this crate; dependencies carry symbol names only. For a
  debugger session that needs variables, build with
  `CARGO_PROFILE_DEV_DEBUG=full`.

## Measured

One worktree on pgrace-dt, `cargo nextest run --no-run`, under the gate lock,
other agents' gates running. "Written" is the build's own bytes written
(getrusage), not the whole disk.

| Step | Before | After |
|---|---|---|
| Crate from cold (dependencies built) | 146 s, 17.7 GB written | 121 s, 5.9 GB written |
| Rebuild after touching `src/lib.rs` | 20 s, 12.7 GB written | 18 s, 4.6 GB written |
| Build dir | 21 GB (6.9 GB incremental) | 7.2 GB (3.4 GB incremental) |
| `it` test binary | 1.31 GB | 0.35 GB |
| Lib unit-test binary | 1.46 GB | 0.42 GB |

The `same`-mode nextest pass took about the same time (145 s before, 148 s
after) and gave the same result: 3854 passed, and
`ui_e2e::read::a_session_check_as_someone_else_alone_hides_the_first_accounts_copies_offline`
failed both times (the known flake #1183).

## Tried and rejected

`CARGO_INCREMENTAL=0` in `tools/agentic/gate.sh` cut a cold crate build to
104 s and 2.5 GB written, but a rebuild after an edit went from 18 s to 84 s.
Agents rebuild with the `narrow` gate many times per issue, so it stays on.

## After merging

The warm build of main (`cargo-warm`) has dependencies built with full debug
info, so every worktree's first build recompiles its dependencies (mostly
sccache hits) until `cargo-warm refresh` builds main with the new profile.
