# release-ship --no-checks

`just release-ship --no-checks` ships a prepared release without running `just fmt`,
`just lint` and `just test` first. Use it when `main` was tested as each change merged and a
second full run would only load the machine. The release PR's body says the checks were
skipped. Without the flag, shipping runs them as before.

- `scripts/release.py`: `ship` takes `--no-checks`; any other argument is still refused.
- `justfile`: `release-ship` passes its arguments through as positional arguments, so no
  argument is ever interpreted by the shell.
- `scripts/test_release.py`: the fake `gh` records the PR body. New tests check that the
  default body says the checks ran, that `--no-checks` ships past a failing check and says it
  skipped them, and that extra or shell-like arguments are refused.
- `docs/operations.md`: "Preparing and shipping a release" documents the flag.
