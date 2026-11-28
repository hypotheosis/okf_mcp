# gate.sh prints a timing line (#1190)

Generated: 2026-11-28T03:21:35Z

`tools/agentic/gate.sh` now ends every run with one stderr line, after the
gate's own output: `gate.sh: <gate> waited <N> s for the lock, seed <N> s, ran <N> s (exit <code>)`.
`waited` is only for gates that take the lock, `seed` only when the seeding
command ran, `ran` only when the gate started. A lock timeout reports the whole
limit as waited. Exit codes, limits and lock behaviour are unchanged.

This separates lock wait from run time, so gate durations compare across runs
and the effect of the build-dir seed (#1185) can be measured.

Tests: `tools/agentic/tests/run.sh` (success, failure, unlocked gate, own
timeout, wait behind another holder, lock timeout, seed present and absent).
Docs: `tools/agentic/README.md`.
