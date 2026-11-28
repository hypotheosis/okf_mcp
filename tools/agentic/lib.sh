#!/usr/bin/env bash
# Shared helpers for the agentic-development scripts in this directory.
# Sourced, not run: `source "$(dirname "$0")/lib.sh"`.

# --- Bounded commands -------------------------------------------------------
# Every external command the tools run goes through agentic_run_bounded (output
# streams: gates) or agentic_run_captured (output captured: gh and git), so a
# stalled `gh`, `git` or test can't block past its budget or outlive it.

# How long agentic_stop_group waits between TERM and KILL, in seconds.
agentic_kill_after=${agentic_kill_after:-5}
# How long agentic_stop_group waits after KILL for the dead to be reaped, in
# seconds. What is still there after that survived KILL.
agentic_reap_after=${agentic_reap_after:-5}
# Set by agentic_run_bounded: 1 when its last command ran out of time.
agentic_timed_out=0

# agentic_stop_group <pgid>: TERM what is left of the process group, KILL it
# after $agentic_kill_after seconds, and return once it is empty (at once when
# it already is) or, when something survives KILL, $agentic_reap_after seconds
# after the KILL. A process that survives KILL is in an uninterruptible kernel
# wait (D state, for example reading a hung mount); callers must check the
# group again and never wait for it.
agentic_stop_group() {
    local tick
    kill -TERM -- "-$1" 2>/dev/null || return 0
    for ((tick = 0; tick < agentic_kill_after * 10; tick++)); do
        kill -0 -- "-$1" 2>/dev/null || return 0
        sleep 0.1
    done
    kill -KILL -- "-$1" 2>/dev/null || return 0
    # KILL can't be ignored; allow a few seconds for the dead to be reaped.
    for ((tick = 0; tick < agentic_reap_after * 10; tick++)); do
        kill -0 -- "-$1" 2>/dev/null || return 0
        sleep 0.1
    done
}

# agentic_traps_restore <saved>: puts the caller's INT and TERM traps back. <saved>
# is the output of `trap -p INT TERM` taken earlier (empty when there were none).
agentic_traps_restore() {
    trap - INT TERM
    [ -z "$1" ] || eval "$1"
}

# The caller's INT and TERM traps while agentic_run_bounded runs, and the status
# it aborts with (130 INT, 143 TERM) when a signal came and the caller's own
# handler returned; 0 otherwise.
agentic_bounded_saved_traps=""
agentic_bounded_signalled=0

# agentic_run_bounded <seconds> <cmd...>: runs cmd in its own session and
# process group with at most <seconds> to finish. When time runs out the group
# is stopped (agentic_stop_group). However cmd ends, anything still left in its
# group is stopped too, with a warning on stderr, and this returns once the
# group is empty. The one exception (#948): a process that survives KILL (an
# uninterruptible kernel wait) is ABANDONED, never waited for, so it can't
# block the caller; a warning on stderr says so (naming the command, never its
# output). A leader that survives KILL after a timeout is disowned. The status
# is unchanged by an abandoned leftover. An INT or TERM meanwhile stops the group
# and then kills this shell with the same signal.
# Returns cmd's status, or 124 when it ran out of time (agentic_timed_out=1). A
# signal whose caller handler returns makes it return 130 (INT) or 143 (TERM).
agentic_run_bounded() {
    agentic_run_bounded_steps "$@"
    local status=$?
    # The one way out (#1193): a signal recorded at ANY step, including the
    # stops after the wait, wins over the command's own status.
    [ "$agentic_bounded_signalled" = 0 ] || status=$agentic_bounded_signalled
    return "$status"
}

# agentic_run_bounded_steps: the body of agentic_run_bounded. It may return
# early; agentic_run_bounded alone decides the status the caller sees.
agentic_run_bounded_steps() {
    local pgid timer finished="" status=0
    agentic_timed_out=0
    # A background job of a non-interactive shell is not a group leader, so
    # setsid makes the job itself the leader of a new session: pid = group id.
    setsid "${@:2}" &
    pgid=$!
    # The timer is a plain sleep beside the command; whichever ends first wins.
    sleep "$1" &
    timer=$!
    agentic_bounded_saved_traps=$(trap -p INT TERM)
    agentic_bounded_signalled=0
    trap 'kill "$timer" 2>/dev/null; agentic_stop_group "$pgid"
        agentic_bounded_signalled=130; agentic_traps_restore "$agentic_bounded_saved_traps"
        kill -INT "$BASHPID"' INT
    trap 'kill "$timer" 2>/dev/null; agentic_stop_group "$pgid"
        agentic_bounded_signalled=143; agentic_traps_restore "$agentic_bounded_saved_traps"
        kill -TERM "$BASHPID"' TERM
    wait -n -p finished "$pgid" "$timer" || status=$?
    # A caller's own handler that returned after the signal: the group is
    # stopped, so abort with the signal's status instead of going on as if the
    # command had ended (#1193).
    [ "$agentic_bounded_signalled" = 0 ] || return "$agentic_bounded_signalled"
    if [ "$finished" = "$pgid" ]; then
        kill "$timer" 2>/dev/null || true
        wait "$timer" 2>/dev/null || true
        # The leader is reaped, so a live group is made of leftovers.
        if kill -0 -- "-$pgid" 2>/dev/null; then
            agentic_stop_group "$pgid"
            if kill -0 -- "-$pgid" 2>/dev/null; then
                echo "agentic: warning: '$2' exited (status $status) leaving processes" \
                    "behind; some survived KILL and were abandoned" >&2
            else
                echo "agentic: warning: '$2' exited (status $status) leaving processes" \
                    "behind; stopped them" >&2
            fi
        fi
    else
        # shellcheck disable=SC2034 # read by the scripts that source this file
        agentic_timed_out=1
        agentic_stop_group "$pgid"
        status=124
        if kill -0 "$pgid" 2>/dev/null; then
            # The leader survived KILL: stop tracking it, never wait for it
            # (bash still reaps it whenever it dies).
            disown "$pgid" 2>/dev/null
            echo "agentic: warning: '$2' timed out and its process survived KILL" \
                "(uninterruptible wait?); abandoned, not waited for" >&2
        else
            # Dead, and bash has reaped it, so this returns at once.
            wait "$pgid" 2>/dev/null || true
            if kill -0 -- "-$pgid" 2>/dev/null; then
                echo "agentic: warning: '$2' timed out and processes of its group survived" \
                    "KILL (uninterruptible wait?); abandoned, not waited for" >&2
            fi
        fi
    fi
    agentic_traps_restore "$agentic_bounded_saved_traps"
    return "$status"
}

# The most stopping a call's process group can take, in seconds: TERM, then
# KILL after $agentic_kill_after, then $agentic_reap_after for the dead to be
# reaped (agentic_stop_group). Both runners spend it after the call's budget.
agentic_cleanup_reserve() {
    printf '%s\n' "$((agentic_kill_after + agentic_reap_after))"
}

# The time a single gh or git call may run: $AGENTIC_CALL_TIMEOUT (default
# 60 s), and, when the caller has set $agentic_deadline (epoch seconds on
# agentic_now's clock), never more than the time left before it minus
# agentic_cleanup_reserve, so that stopping a stalled call is over by the
# deadline too (#1169). Without a deadline the reserve is extra, on top of the
# cap. 0 means no call can start: the time left is not more than the reserve.
agentic_call_budget() {
    local cap=${AGENTIC_CALL_TIMEOUT:-60} left
    if [ -n "${agentic_deadline:-}" ]; then
        left=$((agentic_deadline - $(agentic_now) - $(agentic_cleanup_reserve)))
        [ "$left" -lt "$cap" ] && cap=$left
    fi
    [ "$cap" -gt 0 ] || cap=0
    printf '%s\n' "$cap"
}

# agentic_capture_supervise <seconds> <stdin-file> <stdout-file> <stderr-file>
# <cmd...>: the supervisor behind every captured call (agentic_run_captured,
# watchdog.sh's bounded_capture). It runs cmd in its own session and process
# group with stdin from <stdin-file> and its output in the two files, never a
# pipe, and polls for at most <seconds>. However the leader ends (on its own or
# timed out), its whole group is then stopped by agentic_stop_group, which
# decides by the group's liveness, not the leader's. So neither the child nor a
# descendant that holds its output open (a daemon, a process that ignores TERM)
# can make a caller wait for end-of-file: nothing is read from a pipe. Anything
# that survives KILL (an uninterruptible kernel wait) is abandoned, never waited
# for. Returns cmd's status, or 124 when it ran out of time or had none to run
# with (agentic_timed_out=1). Sets agentic_cap_pid (the group's leader while the
# command runs, for the callers' signal traps), agentic_cap_reason ("timed out",
# "had no time left" or "failed", plus ", child left behind" or ", processes
# left behind") and agentic_cap_left ("stopped them" or "some survived KILL and
# were left behind" when the command exited leaving processes behind).
agentic_cap_pid=""
agentic_cap_reason=""
agentic_cap_left=""
agentic_capture_supervise() {
    local budget=$1 in=$2 out=$3 err=$4 tick status=0
    shift 4
    agentic_timed_out=0
    agentic_cap_pid="" agentic_cap_reason="" agentic_cap_left=""
    if [ "$budget" -le 0 ]; then
        # shellcheck disable=SC2034 # read by the scripts that source this file
        agentic_timed_out=1
        agentic_cap_reason="had no time left"
        return 124
    fi
    # stderr before stdin, so a failed <stdin-file> redirect lands in <stderr-file>.
    setsid "$@" >"$out" 2>"$err" <"$in" &
    agentic_cap_pid=$!
    for ((tick = 0; tick < budget * 10; tick++)); do
        kill -0 "$agentic_cap_pid" 2>/dev/null || break
        sleep 0.1
    done
    if kill -0 "$agentic_cap_pid" 2>/dev/null; then
        agentic_timed_out=1
        status=124
        agentic_cap_reason="timed out"
    else
        # It has exited, so this returns at once. Reaping it first means a
        # live group below is made of descendants, not of its zombie.
        wait "$agentic_cap_pid" || status=$?
        agentic_cap_reason="failed"
        kill -0 -- "-$agentic_cap_pid" 2>/dev/null && agentic_cap_left=1
    fi
    # Every path ends here: stop whatever is left of the group, by group.
    agentic_stop_group "$agentic_cap_pid"
    if kill -0 "$agentic_cap_pid" 2>/dev/null; then
        # The leader survived KILL: stop tracking it, never wait for it
        # (bash still reaps it whenever it dies).
        disown "$agentic_cap_pid" 2>/dev/null
        agentic_cap_reason="$agentic_cap_reason, child left behind"
    elif kill -0 -- "-$agentic_cap_pid" 2>/dev/null; then
        agentic_cap_reason="$agentic_cap_reason, processes left behind"
    fi
    if [ -n "$agentic_cap_left" ]; then
        agentic_cap_left="stopped them"
        kill -0 -- "-$agentic_cap_pid" 2>/dev/null \
            && agentic_cap_left="some survived KILL and were left behind"
    fi
    return "$status"
}

# The temp files of the capture in progress, removed on every path out.
agentic_cap_files=()
# The caller's INT and TERM traps, as `trap -p` printed them, put back by
# agentic_cap_cleanup.
agentic_cap_saved_traps=""
# 0, or 130 (INT) / 143 (TERM) once a signal was handled: the caller's own
# handler returned, so the capture must stop (see agentic_cap_cleanup).
agentic_cap_status=0

# agentic_cap_begin: starts a capture. Remembers the caller's INT and TERM traps
# and installs the ones that clean up. Call it before agentic_cap_mktemp.
agentic_cap_begin() {
    agentic_cap_files=()
    agentic_cap_status=0
    agentic_cap_saved_traps=$(trap -p INT TERM)
    trap 'agentic_cap_cleanup INT' INT
    trap 'agentic_cap_cleanup TERM' TERM
}

# agentic_cap_mktemp <var> <template>: creates a temp file, registers it for
# agentic_cap_cleanup and stores its path in <var>. Returns 1 (after cleaning
# up) when mktemp fails, and $agentic_cap_status when a signal was handled. The file is registered by the same statement that
# creates it (#1193): bash runs a trap only after a whole statement, so a
# signal that arrives while mktemp runs finds the file already registered.
# (`f=$(mktemp ...)` and then `files+=("$f")` leaves a gap between the two.)
agentic_cap_mktemp() {
    agentic_cap_files+=("$(mktemp "$2")") || { agentic_cap_cleanup; return 1; }
    [ "$agentic_cap_status" = 0 ] || return "$agentic_cap_status"
    printf -v "$1" '%s' "${agentic_cap_files[-1]}"
}

# agentic_cap_cleanup [signal]: kills what the capture left running (on a
# signal), removes its temp files, puts the caller's INT and TERM traps back
# and, on a signal, kills this shell with it (so the caller's trap sees it).
# If that handler returns, execution resumes after the kill with everything
# torn down: agentic_cap_status then holds 130 or 143 and the capture's callers
# must stop and return it, never go on (#1193).
agentic_cap_cleanup() {
    # Copied first: a signal that lands in this function runs a nested cleanup
    # that empties the global, and the traps below must still be the caller's.
    local saved_traps=$agentic_cap_saved_traps
    if [ -n "${1:-}" ]; then
        [ -z "$agentic_cap_pid" ] || kill -KILL -- "-$agentic_cap_pid" 2>/dev/null
    fi
    [ "${#agentic_cap_files[@]}" -eq 0 ] || rm -f -- "${agentic_cap_files[@]}"
    agentic_cap_files=()
    agentic_cap_pid=""
    # After a signal the caller's traps are already back (and agentic_cap_status
    # is set): a later plain cleanup must not clear them again.
    [ -n "${1:-}" ] || [ "$agentic_cap_status" = 0 ] || return 0
    agentic_traps_restore "$saved_traps"
    agentic_cap_saved_traps=""
    [ -z "${1:-}" ] && return 0
    case "$1" in INT) agentic_cap_status=130 ;; *) agentic_cap_status=143 ;; esac
    kill "-$1" "$BASHPID"
}

# agentic_run_captured <seconds> <cmd...>: like agentic_run_bounded, but for
# callers that capture the output (`x=$(agentic_gh ...)`) (#1191). The command's
# stdout and stderr go to private temp files (mode 600) and stdin is /dev/null;
# after the command has ended and its group is stopped, the files are copied to
# this function's stdout and stderr, partial output included after a timeout,
# and removed. Nothing waits for end-of-file on a pipe a leftover could hold.
# Returns cmd's status, or 124 on a timeout (or with no time left), or 130 (INT)
# or 143 (TERM) when a signal came and the caller's own handler returned.
agentic_run_captured() {
    agentic_run_captured_steps "$@"
    agentic_cap_finish $?
}

# agentic_cap_finish <status>: the one way out of a capture (#1193). Returns
# 130 (INT) or 143 (TERM) when a signal was recorded at ANY step (create,
# launch, wait, stop, copy-out, cleanup), whatever <status> says, and <status>
# otherwise. The steps functions may return early; none returns to a caller
# without going through here.
agentic_cap_finish() {
    [ "$agentic_cap_status" = 0 ] || return "$agentic_cap_status"
    return "$1"
}

# agentic_run_captured_steps: the body of agentic_run_captured.
agentic_run_captured_steps() {
    local budget=$1 cap_out cap_err status=0
    shift
    agentic_cap_begin
    agentic_cap_mktemp cap_out "${TMPDIR:-/tmp}/agentic-call.XXXXXX" || return $?
    agentic_cap_mktemp cap_err "${TMPDIR:-/tmp}/agentic-call.XXXXXX" || return $?
    agentic_capture_supervise "$budget" /dev/null "$cap_out" "$cap_err" "$@" || status=$?
    [ "$agentic_cap_status" = 0 ] || return "$agentic_cap_status"
    cat -- "$cap_out"
    [ "$agentic_cap_status" = 0 ] || return "$agentic_cap_status"
    cat -- "$cap_err" >&2
    [ "$agentic_cap_status" = 0 ] || return "$agentic_cap_status"
    case "$agentic_cap_reason" in
        "timed out"* | "had no time left"*)
            echo "agentic: warning: '$1' $agentic_cap_reason (status 124)" >&2 ;;
    esac
    [ -z "$agentic_cap_left" ] \
        || echo "agentic: warning: '$1' exited (status $status) leaving processes behind;" \
            "$agentic_cap_left" >&2
    agentic_cap_cleanup
    [ "$agentic_cap_status" = 0 ] || return "$agentic_cap_status"
    return "$status"
}

# agentic_gh <args...> / agentic_git <args...>: gh or git, bounded by
# agentic_call_budget and captured (agentic_run_captured). Return 124 at once
# when no time is left.
agentic_gh() { agentic_bounded_call gh "$@"; }
agentic_git() { agentic_bounded_call git "$@"; }
agentic_bounded_call() {
    local budget
    budget=$(agentic_call_budget)
    [ "$budget" -gt 0 ] || return 124
    agentic_run_captured "$budget" "$@"
}

# --- Repository, reviewer and GitHub helpers --------------------------------

# The GitHub repository (owner/name) of the current checkout, or $AGENTIC_REPO.
agentic_repo() {
    if [ -n "${AGENTIC_REPO:-}" ]; then
        printf '%s\n' "$AGENTIC_REPO"
    else
        agentic_gh repo view --json nameWithOwner --jq .nameWithOwner
    fi
}

# The review bot's full login. Override with $AGENTIC_REVIEWER.
agentic_reviewer() {
    printf '%s\n' "${AGENTIC_REVIEWER:-brooke-reviews[bot]}"
}

# agentic_jq_bot <bot> <filter> [jq options...]: runs jq with `is_bot`
# defined, true when the input login IS the bot: an exact comparison after
# dropping an optional trailing "[bot]" from both sides, so "brooke-reviews"
# and "brooke-reviews[bot]" match but "brooke-example" never does.
agentic_jq_bot() {
    jq "${@:3}" --arg bot "$1" \
        "def is_bot: rtrimstr(\"[bot]\") == (\$bot | rtrimstr(\"[bot]\")); $2"
}

# The directory shared by every worktree of this repository (git's common dir),
# where cross-worktree state such as the gate lock lives.
agentic_state_dir() {
    local common
    common=$(agentic_git rev-parse --path-format=absolute --git-common-dir) || return 1
    mkdir -p "$common/agentic"
    printf '%s\n' "$common/agentic"
}

# The directory of pending-report expectations (expect.sh), shared by every
# worktree. Printed, not created: expect.sh start creates it.
agentic_expect_dir() {
    local common
    common=$(agentic_git rev-parse --path-format=absolute --git-common-dir) || return 1
    printf '%s\n' "$common/agentic-expect"
}

# agentic_expect_field <file> <key>: the value of a `key=value` line of an
# expectation file. The file is only ever read as text, never sourced.
agentic_expect_field() {
    sed -n "s/^$2=//p" "$1" | head -n 1
}

# The top of the main checkout (the parent of the common .git directory).
agentic_main_checkout() {
    local common
    common=$(agentic_git rev-parse --path-format=absolute --git-common-dir) || return 1
    dirname "$common"
}

# agentic_path_inside <path> <dir...>: true when <path> is one of the
# directories or below one, on a directory boundary (so /x/wt2 is not inside
# /x/wt).
agentic_path_inside() {
    local dir
    for dir in "${@:2}"; do
        case "$1" in "$dir" | "$dir"/*) return 0 ;; esac
    done
    return 1
}

# agentic_procs_inside <dir...>: prints "<pid> <cwd> <name>" for every
# process whose working directory is inside one of the directories. Reads
# /proc, so a process of another user whose cwd can't be read is not listed.
# The name is /proc/<pid>/comm, never cmdline: reading cmdline reads the
# process's memory and can stall indefinitely, and its arguments can hold
# credentials.
agentic_procs_inside() {
    local entry pid cwd name
    for entry in /proc/[0-9]*; do
        pid=${entry#/proc/}
        cwd=$(readlink "$entry/cwd" 2>/dev/null) || continue
        agentic_path_inside "$cwd" "$@" || continue
        name='?'
        read -r name <"$entry/comm" 2>/dev/null || true
        printf '%s %s %s\n' "$pid" "$cwd" "$name"
    done
}

# Every item of a paginated GitHub REST list endpoint, as one JSON array.
# `gh api --paginate` prints one array per page; `jq -s 'add'` joins them.
# Fails when any page cannot be fetched.
agentic_api_list() {
    local pages
    pages=$(agentic_gh api --paginate "$1") || return 1
    jq -s 'add // []' <<<"$pages"
}

# The most review rounds a PR gets: one round is one review by the bot, on any
# head. Asking again past this mostly produces nitpicks. Override with
# $AGENTIC_MAX_REVIEW_ROUNDS.
agentic_max_rounds() {
    printf '%s\n' "${AGENTIC_MAX_REVIEW_ROUNDS:-3}"
}

# agentic_review_counts <repo> <pr> <bot> <sha>: prints "<on_sha> <rounds>",
# the bot's reviews of that commit and of any commit, across every page.
# Fails when the reviews can't be read.
agentic_review_counts() {
    local reviews
    reviews=$(agentic_api_list "repos/$1/pulls/$2/reviews") || return 1
    # shellcheck disable=SC2016 # a jq program: $vars are jq's, not the shell's
    agentic_jq_bot "$3" '
        map(select(.user.login | is_bot))
        | "\(map(select(.commit_id == $head)) | length) \(length)"' -r --arg head "$4" \
        <<<"$reviews"
}

# The clock and sleep the watchers use, in seconds. The tests swap in a fake
# clock with $AGENTIC_NOW_CMD and $AGENTIC_SLEEP_CMD so nothing really waits.
agentic_now() {
    if [ -n "${AGENTIC_NOW_CMD:-}" ]; then "$AGENTIC_NOW_CMD"; else date +%s; fi
}
agentic_sleep() {
    "${AGENTIC_SLEEP_CMD:-sleep}" "$1"
}

# The host's uptime in whole seconds ($AGENTIC_UPTIME_FILE, default
# /proc/uptime; the tests point it at a fixture), or nothing when unreadable.
agentic_uptime() {
    local up
    read -r up _ <"${AGENTIC_UPTIME_FILE:-/proc/uptime}" 2>/dev/null || return 0
    printf '%s\n' "${up%%.*}"
}
