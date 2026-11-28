#!/usr/bin/env bash
# watchdog.sh [--once]
#
# Coordinator-side hang detector. Run it in the background for the whole epic.
# Every 5 minutes it looks for test or gate processes, and idle stdin readers,
# whose working directory is inside this repository (the main checkout or any
# registered worktree, wherever it lives), and exits 3, which notifies the
# coordinator, when one has run longer than its limit:
#   test binaries                             AGENTIC_WATCHDOG_TEST   default 1200 s
#     (.../debug/deps/<name>-<hash>, or .../debug/build/<package>/<hash>/out/
#     <name>-<hash> in the newer build-dir layout, which cargo 1.100 uses)
#   cargo test|nextest|clippy|build, just test|lint   AGENTIC_WATCHDOG_GATE   default 4800 s
#   idle stdin reader                         AGENTIC_WATCHDOG_IDLE   default 600 s
#     A cat, tee, head or tail (without -f) whose fd 0 is a pipe or socket and
#     which has used at most one second of CPU. A background command's stdin
#     never closes, so a stray `cat` there hangs forever with no output and
#     the gate behind it never starts (#780). A bash blocked in `read` is not
#     checked: /proc cannot tell it from a bash idling for another reason (a
#     shell waiting for commands on its stdin looks the same), so flagging it
#     would raise false alarms. sleep, `gh pr checks --watch` and wait loops
#     don't read stdin and are not flagged.
# A reading is only trusted when it is plausible: ps sometimes reports a
# wrapped etimes (about 4.1e9 s) for a process that is starting or exiting
# (#371, #534). So a reading above the host's uptime is ignored, and a flagged
# process is read once more (bounded) and must still exist, still be over its
# limit and still be no older than the host.
# A third check covers an agent that stops without any process left to see
# (#850): `expect.sh start <worktree>` records that the coordinator waits for
# that agent's report. A pending expectation alerts when its worktree is idle:
# no test or gate process works there (a cargo, rustc, just or flock process,
# a gate.sh, or a test binary with its cwd in the worktree; a gate waiting for
# the shared gate lock counts as working) and the newest activity is older than
# AGENTIC_WATCHDOG_QUIET (default 1200 s). Activity is the newest of: any mtime
# under <worktree>/target/i*-logs, the worktree's HEAD reflog time and the
# expectation's start time (a time in the future counts as now). An expectation
# whose worktree is gone is dropped with a note. The alert names the worktree's
# basename, the label and the idle seconds. After an alert, `expect.sh done`
# or `expect.sh start` (which restarts the quiet clock) the expectation, or a
# restarted watchdog alerts again at once while it is still idle.
# It reports each process's pid, elapsed time, comm (the executable's name)
# and directory, never its command line: command lines can carry credentials.
# (Matching still reads the command line internally.)
# Every external command that reads the process table, another process's
# memory, git or the worktrees' logs runs under bounded_capture (#675): the
# three `ps -eo` scans, the per-process `ps -p` re-reads, the `tail`
# command-line read, `git rev-parse`, `git worktree list`, `git reflog` and
# `find`. Each gets AGENTIC_CALL_TIMEOUT (default 60 s). Then, whether it timed
# out or exited by itself, whatever is left of its process group gets TERM,
# then KILL (decided by the group's liveness, not the command's), so no
# descendant outlives the call; a process that survives even KILL (an
# uninterruptible kernel wait) is abandoned, never waited for, so none of them
# can freeze the watchdog. A
# timeout warns on stderr (naming the command, never its output) and is never
# an alarm: a timed-out or failed `ps -eo` scan skips the whole pass, a
# timed-out re-read drops that process, a timed-out `find` or `reflog` skips
# that expectation, and a timed-out `worktree list` leaves only the main
# checkout for that pass; the next pass tries again, and with --once it still
# exits 0. When the start-up `git rev-parse` fails or times out, it exits 1.
# Remaining limit: the rest is not supervised. Those are bash's own reads of
# /proc/<pid>/comm, /proc/<pid>/stat and /proc/uptime, `readlink` of
# /proc/<pid>/cwd and /proc/<pid>/fd/0 (none of them reads the process's
# memory, which is what stalls ps), `dirname` and `getconf` at start-up,
# `mktemp`, `date`, `sleep`, `rm`, `sed` and `head` on expectation files, and
# `grep`, `sed`, `awk`, `sort` and `tail` on text already captured. A filesystem that hangs every access (the
# one holding $TMPDIR or git's common directory) can still block those.
# After handling an alert, start it again. With --once it checks a single time
# and exits 0 when nothing is over its limit.
#
# It catches what a PR-review watcher cannot: an agent stuck before it has
# opened a PR. (A timeout-less race test once stalled a task for 12 hours.)
set -u
here=$(cd "$(dirname "$0")" && pwd)
# shellcheck source-path=SCRIPTDIR source=lib.sh
source "$here/lib.sh"

# The main checkout is the parent of git's common dir, which also holds the
# expectation ledger (lib.sh: agentic_main_checkout, agentic_expect_dir). Read
# once, through the supervisor below; without it the watchdog can't work.
git_common=""
start_up() {
    bounded_capture git_common git rev-parse --path-format=absolute --git-common-dir \
        && [ -n "$git_common" ] && return 0
    echo "watchdog: error: could not find this repository's git directory" >&2
    exit 1
}
once=0
[ "${1:-}" = --once ] && once=1
test_limit=${AGENTIC_WATCHDOG_TEST:-1200}
gate_limit=${AGENTIC_WATCHDOG_GATE:-4800}
idle_limit=${AGENTIC_WATCHDOG_IDLE:-600}
interval=${AGENTIC_WATCHDOG_INTERVAL:-300}
quiet_limit=${AGENTIC_WATCHDOG_QUIET:-1200}
# Up to this much CPU (utime + stime, in clock ticks; default one second) still
# counts as idle.
idle_cpu_ticks=${AGENTIC_WATCHDOG_IDLE_CPU:-$(getconf CLK_TCK 2>/dev/null || echo 100)}

# True when $1 is one of the checkouts or below one (lib.sh).
in_checkout() { agentic_path_inside "$1" "${checkouts[@]}"; }

# True when $1 (seconds) is no more than the host's uptime. With the uptime
# unknown, nothing is ruled out.
plausible() {
    [ -z "${uptime:-}" ] || [ "$1" -le "$uptime" ]
}

# Sets log_time to the newest mtime (whole seconds) of everything under $1's
# target/i*-logs directories, or to nothing when there are none. Returns 124
# when a find timed out, so the activity is unknown.
newest_log_time() {
    local dir newest=0 time times
    log_time=""
    for dir in "$1"/target/i*-logs; do
        [ -d "$dir" ] || continue
        times=""
        # A partly failed find (a file deleted meanwhile) still counts.
        bounded_capture -q times find "$dir" -printf '%T@\n'
        [ "$?" = 124 ] && return 124
        time=$(sort -n <<<"$times" | tail -n 1)
        time=${time%%.*}
        case "$time" in '' | *[!0-9]*) continue ;; esac
        [ "$time" -gt "$newest" ] && newest=$time
    done
    [ "$newest" -gt 0 ] && log_time=$newest
    return 0
}

# bounded_capture [-q] [-i <file>] <variable> <cmd...>: runs cmd and stores its
# output in <variable>; every external command a pass needs goes through it.
# The supervision is lib.sh's agentic_capture_supervise, which agentic_gh and
# agentic_git share through agentic_run_captured: cmd writes to a private temp
# file (not a pipe), reads <file> (default /dev/null) on stdin, runs in its own
# session and process group, and is polled for up to agentic_call_budget
# seconds; however it ends, its whole group is stopped (TERM, KILL, and
# abandoned only if it survives KILL), and the temp file is deleted. So neither
# the child nor a descendant holding its output open can stall the watchdog
# (#675). Here stderr is discarded. A timeout warns on stderr, naming only the
# command (never its output or arguments), and returns 124. A command that fails
# returns its status with its output stored; it warns too unless -q is given
# (for calls whose failure is expected, such as a recheck of a process that has
# gone). INT and TERM follow lib.sh's "Signals" invariant from the first step
# (the budget included): 130 or 143 when the caller's own handler returned,
# with nothing launched or stored after the signal.
bounded_capture() {
    local bc_exit=0
    agentic_cap_begin
    bounded_capture_steps "$@" || bc_exit=$?
    agentic_cap_finish "$bc_exit"
}

# bounded_capture_steps: the body of bounded_capture, between agentic_cap_begin
# and agentic_cap_finish (lib.sh). Returns early, to that exit, at the first
# check that finds a signal recorded.
bounded_capture_steps() {
    local bc_quiet=0 bc_in=/dev/null bc_status=0 bc_budget bc_file
    while :; do
        case "$1" in
            -q) bc_quiet=1; shift ;;
            -i) bc_in=$2; shift 2 ;;
            *) break ;;
        esac
    done
    agentic_cap_live || return
    bc_budget=$(agentic_call_budget)
    agentic_cap_mktemp bc_file "${TMPDIR:-/tmp}/watchdog-call.XXXXXX" || return
    agentic_capture_supervise "$bc_budget" "$bc_in" "$bc_file" /dev/null "${@:2}" \
        || bc_status=$?
    agentic_cap_live || return
    [ -z "$agentic_cap_left" ] \
        || echo "watchdog: warning: $2 call exited leaving processes behind; $agentic_cap_left" >&2
    if [ "$bc_status" -eq 124 ]; then
        echo "watchdog: warning: $2 call $agentic_cap_reason (status 124); skipping it" >&2
        return 124
    fi
    agentic_cap_live || return
    printf -v "$1" '%s' "$(<"$bc_file")"
    agentic_cap_live || return
    if [ "$bc_status" -ne 0 ] && [ "$bc_quiet" = 0 ]; then
        echo "watchdog: warning: $2 call $agentic_cap_reason (status $bc_status); skipping it" >&2
    fi
    return "$bc_status"
}

# Checks every pending expectation (expect.sh), appends alerts to $found.
check_expectations() {
    local dir="$git_common/agentic-expect" file started label wt name now newest time idle busy cwd
    [ -d "$dir" ] || return 0
    # Working processes: the cwd of each cargo, rustc, just or flock, of a
    # gate.sh (a bash, so by its argument) and of a test binary.
    local busy_cwds=()
    while read -r pid comm args; do
        case "$comm" in
            cargo | cargo-* | rustc | just | flock) ;;
            *) case "$args" in */gate.sh* | */debug/deps/* | */debug/build/*/out/*) ;; *) continue ;; esac ;;
        esac
        cwd=$(readlink "/proc/$pid/cwd" 2>/dev/null) && busy_cwds+=("$cwd")
    done < <(grep -v -E 'grep|watchdog\.sh' <<<"$busy_procs")
    now=$(agentic_now)
    for file in "$dir"/*; do
        [ -f "$file" ] || continue
        wt=$(agentic_expect_field "$file" path)
        label=$(agentic_expect_field "$file" label)
        started=$(agentic_expect_field "$file" start)
        name=${file##*/}
        if [ ! -d "$wt" ]; then
            rm -f "$file"
            echo "watchdog: dropped the expectation for $name: its worktree is gone"
            continue
        fi
        case "$started" in '' | *[!0-9]*) started=$now ;; esac
        busy=0
        for cwd in "${busy_cwds[@]}"; do
            case "$cwd" in "$wt" | "$wt"/*) busy=1 ;; esac
        done
        [ "$busy" = 0 ] || continue
        newest=$started
        # A call that timed out leaves the activity unknown: no alert this pass.
        newest_log_time "$wt" || continue
        [ -n "$log_time" ] && [ "$log_time" -gt "$newest" ] && newest=$log_time
        # The reflog event time (HEAD@{<epoch>}), not the commit's own date: a
        # checkout or reset to an old commit is fresh activity.
        time=""
        bounded_capture -q time git -C "$wt" reflog -1 --date=unix --format=%gd HEAD
        [ "$?" = 124 ] && continue
        time=${time#HEAD@\{}
        time=${time%\}}
        case "$time" in '' | *[!0-9]*) ;; *) [ "$time" -gt "$newest" ] && newest=$time ;; esac
        idle=$((now - newest))
        [ "$idle" -gt "$quiet_limit" ] || continue
        found+="agent at $name quiet for ${idle}s with a report pending: ${label:-no label}"$'\n'
    done
}

start_up
root=${git_common%/*}
while :; do
    # Re-read every round: worktrees come and go during an epic. The call is
    # bounded (bounded_capture), so a stuck git can't stall the watchdog; when
    # it fails or times out, this pass covers the main checkout only.
    checkouts=("$root")
    worktrees=""
    bounded_capture -q worktrees git -C "$root" worktree list --porcelain
    while read -r path; do [ -n "$path" ] && checkouts+=("$path"); done \
        < <(sed -n 's/^worktree //p' <<<"$worktrees")
    found=""
    uptime=$(agentic_uptime)
    # Every scan is taken before any alert is raised, and each is bounded. When
    # one times out or fails the whole pass is skipped (with a warning from
    # bounded_capture, never an alarm, even if an earlier scan found something);
    # the next pass tries again. busy_procs feeds check_expectations.
    all_procs="" readers="" busy_procs=""
    if ! { bounded_capture all_procs ps -eo pid=,etimes=,args= \
        && bounded_capture readers ps -eo pid=,etimes=,comm= \
        && bounded_capture busy_procs ps -eo pid=,comm=,args=; }; then
        [ "$once" = 1 ] && exit 0
        sleep "$interval"
        continue
    fi
    while read -r pid elapsed args; do
        cwd=$(readlink "/proc/$pid/cwd" 2>/dev/null) || continue
        in_checkout "$cwd" || continue
        limit=$gate_limit
        case "$args" in */debug/deps/* | */debug/build/*/out/*) limit=$test_limit ;; esac
        if [ "$elapsed" -gt "$limit" ]; then
            plausible "$elapsed" || continue
            # Re-read: the first reading may belong to a process that has gone
            # (ps then fails, quietly). A timed-out re-read drops the process
            # for this pass, with a warning; the next pass checks it again.
            bounded_capture -q again ps -o etimes= -p "$pid" || continue
            again=${again//[[:space:]]/}
            case "$again" in '' | *[!0-9]*) continue ;; esac
            [ "$again" -gt "$limit" ] && plausible "$again" || continue
            read -r comm <"/proc/$pid/comm" 2>/dev/null || comm=unknown
            found+="pid=$pid elapsed=${elapsed}s comm=$comm cwd=$cwd"$'\n'
        fi
    done < <(grep -E '/debug/deps/|/debug/build/[^/ ]+/[^/ ]+/out/|cargo (test|nextest|clippy|build)|just (test|lint|test-ui)' <<<"$all_procs" \
        | grep -v -E 'grep|watchdog\.sh')
    # Idle stdin readers (#780). The list is by comm only; the command line is
    # read here just to tell `tail -f` (a legitimate follower) from `tail`.
    while read -r pid elapsed comm; do
        [ "$elapsed" -gt "$idle_limit" ] || continue
        plausible "$elapsed" || continue
        cwd=$(readlink "/proc/$pid/cwd" 2>/dev/null) || continue
        in_checkout "$cwd" || continue
        case "$(readlink "/proc/$pid/fd/0" 2>/dev/null)" in pipe:* | socket:*) ;; *) continue ;; esac
        if [ "$comm" = tail ]; then
            # Reading another process's cmdline reads its memory, which can
            # block like ps does, so it is supervised too.
            bounded_capture -q -i "/proc/$pid/cmdline" args tr '\0' ' ' || continue
            [[ " $args" =~ [[:space:]](-[A-Za-z0-9]*[fF][A-Za-z0-9]*|--follow(=[a-z]+)?)[[:space:]] ]] && continue
        fi
        # utime and stime are fields 14 and 15; the fields after ") " start at
        # the state (field 3), so they are the 12th and 13th words.
        read -r stat <"/proc/$pid/stat" 2>/dev/null || continue
        # shellcheck disable=SC2086 # split on purpose
        set -- ${stat##*) }
        [ $((${12:-0} + ${13:-0})) -le "$idle_cpu_ticks" ] || continue
        bounded_capture -q again ps -o etimes= -p "$pid" || continue
        again=${again//[[:space:]]/}
        case "$again" in '' | *[!0-9]*) continue ;; esac
        [ "$again" -gt "$idle_limit" ] && plausible "$again" || continue
        found+="pid=$pid elapsed=${elapsed}s comm=$comm cwd=$cwd idle-stdin-reader"$'\n'
    done < <(awk '$3=="cat"||$3=="tee"||$3=="head"||$3=="tail"' <<<"$readers")
    check_expectations
    if [ -n "$found" ]; then
        echo "WATCHDOG $(date -u +%FT%TZ): long-running or idle processes in $root:"
        printf '%s' "$found"
        exit 3
    fi
    [ "$once" = 1 ] && exit 0
    sleep "$interval"
done
