# Ignore Python bytecode (issue #1076)

`.gitignore` now ignores `__pycache__/` and `*.py[cod]`. Running
`scripts/test_release.py`, or importing any helper under `scripts/`, writes
`scripts/__pycache__/*.pyc`, and a plain `git add -A` used to commit it (PR #1074
had to remove one). No tracked file matched either pattern, so nothing is untracked
by this change.
