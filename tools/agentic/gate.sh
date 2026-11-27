#!/usr/bin/env bash
# gate.sh <gate> [extra args...]
#
# Runs one project gate under the cross-worktree lock and a hard timeout.
# Several agents share one machine, so heavy gates must not run at once, and a
# hung test must never stall a task: a timeout exit (124) means "a test hung",
# never "pass" or an ordinary failure.
#
# Gates and their limits (override a limit with AGENTIC_TIMEOUT_<GATE>, e.g.
# AGENTIC_TIMEOUT_TEST=90m):
#   fmt      just fmt                     5m   (no lock)
#   lint     just lint                    30m
#   test     just test                    75m  (nextest and doctests twice: OKF_TEST_STORE=same, then =separate)
#   test-ui  just test-ui                 60m
#   narrow   cargo test <args...>         15m  (no lock; for iterating)
#   dylint   just dylint                  45m  (the first run builds the lint library)
#   worker   just test-worker             5m   (no lock; the reader's Node tests, also run by test)
#   release  just test-release            5m   (no lock; scripts/test_release.py, the release tooling's tests)
#   mcpjam   just test-mcpjam             20m  (the MCPJam scenarios against a local server, tests/mcpjam/)
#
# Before a gate that builds, an installed seeding command (AGENTIC_SEED_CMD,
# default cargo-warm) may fill an empty cargo build dir; see below.
#
# The limit counts time spent waiting for the lock (flock -w), so a gate stuck
# behind a hung holder also fails instead of waiting forever.
#
# The gate runs in its own session (setsid, via lib.sh's agentic_run_bounded),
# so its whole process group can be stopped. On timeout the group gets TERM,
# then KILL after a grace period (AGENTIC_GATE_KILL_AFTER, default 60 s). On
# every other exit too (pass, failure, a killed gate, or INT/TERM to gate.sh),
# whatever is left of the group is stopped the same way. gate.sh returns only once the group is empty,
# so nothing it started (a test that ignores TERM, a stray background job)
# outlives it. gate.sh holds the lock until then, and the gate's children
# never inherit it, so a daemon that leaves the group (setsid) cannot keep it.
# One exception to "the group is empty" (#948): a process that survives KILL (an
# uninterruptible kernel wait) is abandoned 5 s after KILL,
# with a warning on stderr, so it can't hang gate.sh; the lock is
# still freed.
#
# Exit codes:
#   the gate's own, when it exits by itself; if it left processes behind they
#     are stopped and a warning on stderr says so;
#   124 on timeout, or when the gate was killed (137), with a hint on stderr;
#   130 or 143 when gate.sh itself gets INT or TERM (it stops the gate, then
#     dies of the same signal).
set -euo pipefail
here=$(cd "$(dirname "$0")" && pwd)
# shellcheck source-path=SCRIPTDIR source=lib.sh
source "$here/lib.sh"

gate=${1:?usage: gate.sh fmt|lint|test|test-ui|dylint|worker|release|mcpjam|narrow [args...]}
shift

# Build against the committed sqlx query data, as the justfile does, so a
# DATABASE_URL in the caller's environment never steers a gate (the narrow
# gate runs cargo directly and would otherwise miss the justfile's setting).
# A value the caller set explicitly is kept.
export SQLX_OFFLINE="${SQLX_OFFLINE-true}"

lock="$(agentic_state_dir)/gates.lock"
kill_after=${AGENTIC_GATE_KILL_AFTER:-60}

case "$gate" in
    fmt)     limit=${AGENTIC_TIMEOUT_FMT:-5m};     locked=0; cmd=(just fmt "$@") ;;
    lint)    limit=${AGENTIC_TIMEOUT_LINT:-30m};   locked=1; cmd=(just lint "$@") ;;
    test)    limit=${AGENTIC_TIMEOUT_TEST:-75m};   locked=1; cmd=(just test "$@") ;;
    test-ui) limit=${AGENTIC_TIMEOUT_TEST_UI:-60m}; locked=1; cmd=(just test-ui "$@") ;;
    dylint)  limit=${AGENTIC_TIMEOUT_DYLINT:-45m};  locked=1; cmd=(just dylint "$@") ;;
    worker)  limit=${AGENTIC_TIMEOUT_WORKER:-5m};  locked=0; cmd=(just test-worker "$@") ;;
    release) limit=${AGENTIC_TIMEOUT_RELEASE:-5m}; locked=0; cmd=(just test-release "$@") ;;
    # Always the local server built from this checkout, on a throwaway
    # Postgres: a remote target (OKF_MCP_URL) would test what is deployed,
    # not this change; OKF_MCP_BIN would test another build; and the suite
    # prefers DATABASE_URL over the test server's TEST_PG_URL, so a real
    # database must never reach it.
    mcpjam)  limit=${AGENTIC_TIMEOUT_MCPJAM:-20m}; locked=1
             cmd=(env -u OKF_MCP_URL -u OKF_MCP_KEY -u OKF_MCP_API_URL -u OKF_MCP_ISOLATION
                  -u OKF_MCP_CATALOG -u OKF_MCP_BIN -u DATABASE_URL just test-mcpjam "$@") ;;
    narrow)  limit=${AGENTIC_TIMEOUT_NARROW:-15m}; locked=0; cmd=(cargo test "$@") ;;
    *) echo "gate.sh: unknown gate '$gate'" >&2; exit 2 ;;
esac

# A new worktree has no cargo build dir, so its first build compiles every
# dependency. If a seeding command is installed (AGENTIC_SEED_CMD, default
# `cargo-warm`: a machine-local tool, not part of this repository), gates that
# build run `<cmd> seed .` first, which copies a warm build of main into the
# build dir when there is none yet. It is optional and best effort: when the
# command is missing nothing happens, and when it fails the gate builds from
# scratch as before. An empty AGENTIC_SEED_CMD turns it off.
seed_cmd=${AGENTIC_SEED_CMD-cargo-warm}
case "$gate" in
    fmt | worker | release) ;;
    *)
        if [ -n "$seed_cmd" ] && command -v "$seed_cmd" >/dev/null 2>&1; then
            agentic_kill_after=5
            agentic_run_bounded 300 "$seed_cmd" seed . \
                || echo "gate.sh: '$seed_cmd seed' failed; building from scratch" >&2
        fi
        ;;
esac

# A limit such as 90, 90s, 30m or 2h, in seconds.
case "$limit" in
    *[0-9]s | *[0-9]) unit=1 ;;
    *[0-9]m) unit=60 ;;
    *[0-9]h) unit=3600 ;;
    *) echo "gate.sh: bad limit '$limit' (use N, Ns, Nm or Nh)" >&2; exit 2 ;;
esac
number=${limit%[smh]}
[[ "$number" =~ ^[0-9]+$ ]] || { echo "gate.sh: bad limit '$limit'" >&2; exit 2; }
limit_s=$((number * unit))

# lib.sh's agentic_run_bounded runs the gate in its own process group and
# empties that group on every exit; the grace period here is the gate's.
agentic_kill_after=$kill_after
status=0
agentic_timed_out=0
lock_timed_out=0
if [ "$locked" = 1 ]; then
    # gate.sh holds the lock itself, on fd 9, from before the gate starts until
    # its process group is empty, cleanup included. The gate never inherits
    # it: `9>&-` below closes fd 9 for everything agentic_run_bounded starts
    # (bash keeps its own copy close-on-exec), so a daemon the gate starts,
    # which leaves the group with setsid and escapes the cleanup (sccache's
    # server), cannot keep the lock after gate.sh returns.
    exec 9>>"$lock"
    waited_from=$SECONDS
    # A lock wait that runs out is a hang too: the holder is stuck.
    if flock -w "$limit_s" 9; then
        # The limit counts the wait.
        limit_s=$((limit_s - (SECONDS - waited_from)))
        [ "$limit_s" -ge 1 ] || limit_s=1
        agentic_run_bounded "$limit_s" "${cmd[@]}" 9>&- || status=$?
    else
        lock_timed_out=1
        status=124
    fi
else
    agentic_run_bounded "$limit_s" "${cmd[@]}" || status=$?
fi
if [ "$status" = 124 ] || [ "$status" = 137 ]; then
    if [ "$lock_timed_out" = 1 ]; then
        reason="waited its ${limit} limit for the gate lock (another gate is stuck holding it)"
    elif [ "$agentic_timed_out" = 1 ]; then
        reason="hit its ${limit} limit"
    else
        reason="was killed or timed out (exit $status)"
    fi
    cat >&2 <<EOF
gate.sh: '$gate' $reason. Treat this as a HANG, not a failure:
  - run the suspect tests alone with: tools/agentic/gate.sh narrow <filter> -- --test-threads=1
  - in the test's Postgres container, check lock waits:
      SELECT pid, state, wait_event_type, left(query, 120) FROM pg_stat_activity;
  - any test that holds a lock or waits on another task must wrap the wait in
    tokio::time::timeout and fail with a message naming what it waited for.
EOF
    exit 124
fi
exit "$status"
