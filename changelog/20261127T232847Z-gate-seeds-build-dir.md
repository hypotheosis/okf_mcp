# gate.sh seeds an empty cargo build dir before building gates

A new worktree starts with an empty cargo build dir, so its first gate compiles every dependency
(about 413 s cold for `cargo test --no-run`). `gate.sh` now runs an optional seeding command,
`AGENTIC_SEED_CMD` (default `cargo-warm`), as `<cmd> seed .` before every gate that builds
(everything but `fmt`, `worker` and `release`). The command is machine-local and not part of this
repository: on the maintainer's workstation it reflink-copies a warm build of `main` into the
worktree's build dir when it has none, so cargo finds every dependency Fresh and compiles only
`okf_mcp` itself.

- Optional and best effort: when the command is not on `PATH` nothing happens; when it fails,
  `gate.sh` prints `'<cmd> seed' failed; building from scratch` and the gate runs as before. The
  seed step is bounded at 300 s in its own process group (`agentic_run_bounded`).
- `AGENTIC_SEED_CMD=` (empty) turns it off; `tools/agentic/tests/run.sh` sets it empty so the suite
  never runs a machine-local tool.
- Documented in `tools/agentic/README.md` (environment table) and `gate.sh`'s header.

Measured on the workstation (fresh worktree at `e1149dae`, seeded by `gate.sh narrow --no-run`):
495 crates Fresh, only `okf_mcp` compiled, 307 s under concurrent load; an earlier hand-seeded
trial took 170 s, against 413 s cold.

Tests (`just test-agentic`): `test_gate_seeds_the_build_dir_before_a_building_gate`,
`test_gate_fmt_does_not_seed`, `test_gate_builds_from_scratch_when_seeding_fails`,
`test_gate_skips_seeding_when_the_command_is_missing`. The first and third fail with the hook
disabled.
