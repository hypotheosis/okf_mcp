#!/usr/bin/env bash
# Shared helpers for the agentic-development scripts in this directory.
# Sourced, not run: `source "$(dirname "$0")/lib.sh"`.

# --- Bounded commands -------------------------------------------------------
# Every external command the tools run goes through agentic_run_bounded (output
# streams: gates) or agentic_run_captured (output captured: gh and git), so a
# stalled `gh`, `git` or test can't block past its budget or outlive it.
#
# Signals (#1193). agentic_run_bounded, agentic_run_captured (and agentic_gh /
# agentic_git) and watchdog.sh's bounded_capture each own INT and TERM from the
# moment their traps go in until their one exit, and hold this invariant:
#   - While they own the signals, a signal is only recorded (the first one
#     wins); the trap tears nothing down and returns. So no step can find its
#     files removed or the caller's traps back in the middle of its work.
#   - After a signal is recorded, nothing new starts. Every statement of a
#     helper's body that creates a file, launches a process (an external
#     command, a pipeline, a `$(...)` or a background job, the budget's clock
#     included) or writes output is either the statement directly after a
#     check of the record, with nothing between them, or a call to a helper
#     whose own first statement is that check and whose second is the act
#     (agentic_cap_mktemp, agentic_cap_say, agentic_bounded_warn, and
#     agentic_capture_supervise for its launch). The check is agentic_cap_live
#     in a capture and [ "$agentic_bounded_signal" = 0 ] in the bounded run;
#     a bounded run inside a capture (remove-worktree.sh) takes over, as its
#     traps go in, a signal the capture already recorded, so its own first
#     check stops it (agentic_signals_begin). Statements that create, start
#     and write nothing (tests, plain assignments, kill -0, wait) need no
#     check of their own. So a signal that
#     lands between a check and the statement after it lets exactly that one
#     statement run (with what it runs itself, such as the subshell of its
#     `$(...)`), and nothing after it. What that statement made is undone: a
#     temp file is registered by the statement that creates it and removed at
#     the exit; a launched group is stopped right after the launch, by the
#     captured call's first poll or by agentic_run_bounded's wait, which the
#     signal's trap ends by ending the timer (agentic_stop_group waits for a
#     group whose leader has not yet called setsid()). The undoing is the one
#     thing that runs after a signal, by design: agentic_stop_group (its kill,
#     wait and sleep), the bounded run's kill and wait of its timer, and the
#     exit's rm. tests/run.sh checks this by sending TERM before each
#     statement of each helper in turn (tests/signal-sweep.sh).
#   - The exit runs exactly once on every path, a caller's `set -e` included
#     (the body's status is taken with `|| status=$?`): it removes the temp files, puts
#     the caller's INT and TERM traps back (one statement per signal, so there
#     is never a moment with neither trap), and re-raises a recorded signal
#     exactly once. If the caller's handler returns, the helper returns 130
#     (INT) or 143 (TERM), whatever its command did.
#   - The long waits stop promptly on a recorded signal: the trap interrupts
#     agentic_run_bounded's `wait`, and the captured calls' poll checks the
#     record every 0.1 s; either then stops the command's group.
# A signal before a helper's traps are in, or after its exit put the caller's
# back, is the caller's own.

# How long agentic_stop_group waits between TERM and KILL, in seconds.
agentic_kill_after=${agentic_kill_after:-5}
# How long agentic_stop_group waits after KILL for the dead to be reaped, in
# seconds. What is still there after that survived KILL.
agentic_reap_after=${agentic_reap_after:-5}
# Set by agentic_run_bounded: 1 when its last command ran out of time.
agentic_timed_out=0

# How often the bounded and captured calls look at their command, in checks
# per second (AGENTIC_TICKS_PER_SEC, 1..1000; the default 10 is a 0.1 s tick).
# Every call costs at least one tick, so on a fake clock, where a watcher makes
# hundreds of calls, the tests raise it to 100. The limits stay in whole
# seconds: a loop ends after limit * ticks-per-second ticks or when the clock
# ($EPOCHREALTIME) says the limit has passed, whichever comes first, so the
# time a tick's own work takes never stretches a limit at fine ticks.
agentic_ticks_per_sec=${AGENTIC_TICKS_PER_SEC:-10}
case "$agentic_ticks_per_sec" in
    [1-9] | [1-9][0-9] | [1-9][0-9][0-9] | 1000) ;;
    *) agentic_ticks_per_sec=10 ;;
esac
printf -v agentic_tick '0.%03d' "$((1000 / agentic_ticks_per_sec))"

# agentic_stop_group <pgid>: TERM what is left of the process group, KILL it
# after $agentic_kill_after seconds, and return once it is empty (at once when
# it already is) or, when something survives KILL, $agentic_reap_after seconds
# after the KILL. A process that survives KILL is in an uninterruptible kernel
# wait (D state, for example reading a hung mount); callers must check the
# group again and never wait for it.
agentic_stop_group() {
    local tick end
    # Just after the launch the leader may not have called setsid() yet: it is
    # alive but its group does not exist. Wait (up to 1 s) for the group, and
    # KILL the leader by pid if it never appears (#1193).
    for ((tick = 0; tick < 100; tick++)); do
        kill -TERM -- "-$1" 2>/dev/null && break
        kill -0 "$1" 2>/dev/null || return 0
        sleep 0.01
    done
    [ "$tick" -lt 100 ] || { kill -KILL "$1" 2>/dev/null; return 0; }
    end=$((${EPOCHREALTIME/[.,]/} + agentic_kill_after * 1000000))
    for ((tick = 0; tick < agentic_kill_after * agentic_ticks_per_sec; tick++)); do
        [ "${EPOCHREALTIME/[.,]/}" -lt "$end" ] || break
        kill -0 -- "-$1" 2>/dev/null || return 0
        sleep "$agentic_tick"
    done
    kill -KILL -- "-$1" 2>/dev/null || return 0
    # KILL can't be ignored; allow a few seconds for the dead to be reaped.
    end=$((${EPOCHREALTIME/[.,]/} + agentic_reap_after * 1000000))
    for ((tick = 0; tick < agentic_reap_after * agentic_ticks_per_sec; tick++)); do
        [ "${EPOCHREALTIME/[.,]/}" -lt "$end" ] || break
        kill -0 -- "-$1" 2>/dev/null || return 0
        sleep "$agentic_tick"
    done
}

# agentic_signals_begin <prefix>: takes INT and TERM over for a helper (see
# "Signals" above). Saves the caller's traps, as `trap -p` prints them, in
# <prefix>_saved_int and <prefix>_saved_term, sets <prefix>_signal to 0, and
# installs traps that only record 130 (INT) or 143 (TERM) there, keeping the
# first, and then run [on-signal], a command that must not create or launch
# anything (agentic_run_bounded's ends its timer, so its wait returns). Each
# helper has its own <prefix>, so one can run inside another (remove-worktree.sh
# runs agentic_run_bounded inside a capture): the inner exit re-raises into the
# outer's recording trap. agentic_signals_owner names the innermost owner
# (empty when none), so an inner helper takes over a signal the outer one
# has already recorded.
agentic_signals_owner=""
agentic_signals_begin() {
    local outer=${1}_outer outer_signal
    printf -v "${1}_saved_int" '%s' "$(trap -p INT)"
    printf -v "${1}_saved_term" '%s' "$(trap -p TERM)"
    printf -v "${1}_outer" '%s' "$agentic_signals_owner"
    agentic_signals_owner=$1
    printf -v "${1}_signal" '%s' 0
    # shellcheck disable=SC2064 # the prefix is expanded now, the record at the signal
    trap "[ \"\$${1}_signal\" != 0 ] || ${1}_signal=130; ${2:-:}" INT
    # shellcheck disable=SC2064
    trap "[ \"\$${1}_signal\" != 0 ] || ${1}_signal=143; ${2:-:}" TERM
    # A signal the outer owner's trap recorded (before these traps went in, or
    # before this helper was called) is this helper's too, so its body starts
    # nothing. Read after the traps are in: a later signal is recorded here.
    outer_signal=${!outer:+${!outer}_signal}
    [ -z "$outer_signal" ] || [ "${!outer_signal}" = 0 ] \
        || printf -v "${1}_signal" '%s' "${!outer_signal}"
}

# agentic_signals_end <prefix> <status>: gives INT and TERM back: restores the
# traps agentic_signals_begin saved, one statement per signal, then re-raises a
# recorded signal at this shell, once. The record is read after the restore, so
# a signal that arrived during it is not lost. Returns 130 or 143 when a signal
# was recorded (the caller's handler returned), <status> otherwise.
agentic_signals_end() {
    local saved_int=${1}_saved_int saved_term=${1}_saved_term signal=${1}_signal
    local outer=${1}_outer
    # ${!name} is bash's indirect expansion: the value of the variable named
    # by $name.
    agentic_signals_owner=${!outer}
    if [ -n "${!saved_int}" ]; then eval "${!saved_int}"; else trap - INT; fi
    if [ -n "${!saved_term}" ]; then eval "${!saved_term}"; else trap - TERM; fi
    case "${!signal}" in
        130) kill -INT "$BASHPID"; return 130 ;;
        143) kill -TERM "$BASHPID"; return 143 ;;
    esac
    return "$2"
}

# The bounded run's signal state (agentic_signals_begin agentic_bounded):
# the caller's traps, and 0 or the recorded 130 / 143.
# shellcheck disable=SC2034 # set and read through the agentic_bounded prefix
agentic_bounded_saved_int="" agentic_bounded_saved_term="" agentic_bounded_outer=""
agentic_bounded_signal=0
# The timer of the bounded run in progress, which a recorded signal ends.
agentic_bounded_timer=""

# agentic_bounded_warn <text...>: a warning on stderr, unless a signal is
# recorded (then nothing more is written).
agentic_bounded_warn() {
    [ "$agentic_bounded_signal" = 0 ] || return 0
    echo "agentic: warning: $*" >&2
}

# agentic_run_bounded <seconds> <cmd...>: runs cmd in its own session and
# process group with at most <seconds> to finish. When time runs out the group
# is stopped (agentic_stop_group). However cmd ends, anything still left in its
# group is stopped too, with a warning on stderr, and this returns once the
# group is empty. The one exception (#948): a process that survives KILL (an
# uninterruptible kernel wait) is ABANDONED, never waited for, so it can't
# block the caller; a warning on stderr says so (naming the command, never its
# output). A leader that survives KILL after a timeout is disowned. The status
# is unchanged by an abandoned leftover. An INT or TERM meanwhile stops the
# group, then reaches the caller's trap (or kills this shell): see "Signals"
# above; cmd is not launched when the signal came first.
# Returns cmd's status, or 124 when it ran out of time (agentic_timed_out=1). A
# signal whose caller handler returns makes it return 130 (INT) or 143 (TERM).
agentic_run_bounded() {
    local status=0
    agentic_bounded_timer=""
    # shellcheck disable=SC2016 # the trap expands it when the signal comes
    agentic_signals_begin agentic_bounded \
        '[ -z "$agentic_bounded_timer" ] || kill "$agentic_bounded_timer" 2>/dev/null'
    # `|| status=$?`, not `$?` after the call: under a caller's `set -e` a
    # failing body would otherwise end the shell before the exit runs.
    agentic_run_bounded_steps "$@" || status=$?
    agentic_signals_end agentic_bounded "$status"
}

# agentic_run_bounded_steps: the body of agentic_run_bounded, which alone
# decides the status the caller sees.
agentic_run_bounded_steps() {
    local pgid timer finished="" status=0 what="timed out"
    agentic_timed_out=0
    [ "$agentic_bounded_signal" = 0 ] || return
    # A background job of a non-interactive shell is not a group leader, so
    # setsid makes the job itself the leader of a new session: pid = group id.
    setsid "${@:2}" &
    pgid=$!
    # The timer is a plain sleep beside the command; whichever ends first wins.
    # A signal recorded during the launch starts no timer.
    timer=""
    if [ "$agentic_bounded_signal" = 0 ]; then
        sleep "$1" &
        timer=$!
    fi
    agentic_bounded_timer=$timer
    # A recorded signal interrupts this wait. One recorded before it (after the
    # check above) skips it, or, landing after this check, has ended the timer,
    # so the wait returns at once. Either way the group is stopped below.
    if [ "$agentic_bounded_signal" = 0 ] && [ -n "$timer" ]; then
        wait -n -p finished "$pgid" "$timer" || status=$?
    fi
    if [ -n "$timer" ]; then
        kill "$timer" 2>/dev/null || true
        wait "$timer" 2>/dev/null || true
    fi
    # shellcheck disable=SC2034 # read by the trap's on-signal command
    agentic_bounded_timer=""
    # `wait -p` unsets finished when a signal interrupts it.
    if [ "${finished:-}" = "$pgid" ]; then
        # The leader is reaped, so a live group is made of leftovers.
        if kill -0 -- "-$pgid" 2>/dev/null; then
            agentic_stop_group "$pgid"
            if kill -0 -- "-$pgid" 2>/dev/null; then
                agentic_bounded_warn "'$2' exited (status $status) leaving processes" \
                    "behind; some survived KILL and were abandoned"
            else
                agentic_bounded_warn "'$2' exited (status $status) leaving processes" \
                    "behind; stopped them"
            fi
        fi
        return "$status"
    fi
    # The timer ended, or a signal was recorded.
    if [ "$agentic_bounded_signal" = 0 ]; then
        # shellcheck disable=SC2034 # read by the scripts that source this file
        agentic_timed_out=1
        status=124
    else
        what="was interrupted"
    fi
    agentic_stop_group "$pgid"
    if kill -0 "$pgid" 2>/dev/null; then
        # The leader survived KILL: stop tracking it, never wait for it
        # (bash still reaps it whenever it dies).
        disown "$pgid" 2>/dev/null
        agentic_bounded_warn "'$2' $what and its process survived KILL" \
            "(uninterruptible wait?); abandoned, not waited for"
    else
        # Dead, and bash has reaped it, so this returns at once.
        wait "$pgid" 2>/dev/null || true
        if kill -0 -- "-$pgid" 2>/dev/null; then
            agentic_bounded_warn "'$2' $what and processes of its group survived" \
                "KILL (uninterruptible wait?); abandoned, not waited for"
        fi
    fi
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

# The capture in progress (agentic_cap_begin to agentic_cap_finish): its temp
# files, removed by the exit on every path, and its signal state
# (agentic_signals_begin agentic_cap): the caller's traps, and 0 or the
# recorded 130 / 143.
agentic_cap_files=()
# shellcheck disable=SC2034 # set and read through the agentic_cap prefix
agentic_cap_saved_int="" agentic_cap_saved_term=""
agentic_cap_signal=0

# agentic_cap_begin: starts a capture and takes INT and TERM over (see
# "Signals" above). Everything up to agentic_cap_finish, the budget included,
# runs under its recording traps.
agentic_cap_begin() {
    agentic_cap_files=()
    agentic_cap_pid=""
    agentic_signals_begin agentic_cap
}

# agentic_cap_live: true while no signal is recorded. The one check a capture
# makes before each step that creates a file, launches a process or writes
# output; a step that finds it false returns at once, to agentic_cap_finish.
agentic_cap_live() {
    [ "$agentic_cap_signal" = 0 ]
}

# agentic_cap_say <text...>: writes <text> on stderr, unless a signal is
# recorded; then it writes nothing and returns 1, so `agentic_cap_say ... ||
# return` goes to the exit.
agentic_cap_say() {
    agentic_cap_live || return 1
    echo "$*" >&2
}

# agentic_cap_mktemp <var> <template>: creates a temp file, registers it for
# agentic_cap_finish and stores its path in <var>. Returns 1 when a signal is
# recorded (nothing created) or mktemp fails. The file is registered by the
# same statement that creates it: bash runs a trap only after a whole
# statement, so a signal that arrives while mktemp runs finds the file already
# registered. (`f=$(mktemp ...)` and then `files+=("$f")` leaves a gap.)
agentic_cap_mktemp() {
    agentic_cap_live || return 1
    agentic_cap_files+=("$(mktemp "$2")") || {
        unset 'agentic_cap_files[-1]'
        return 1
    }
    printf -v "$1" '%s' "${agentic_cap_files[-1]}"
}

# agentic_cap_finish <status>: the one way out of a capture. Removes its temp
# files, gives INT and TERM back and re-raises a recorded signal
# (agentic_signals_end). Returns 130 (INT) or 143 (TERM) when a signal was
# recorded at any step, whatever <status> says, and <status> otherwise.
agentic_cap_finish() {
    [ "${#agentic_cap_files[@]}" -eq 0 ] || rm -f -- "${agentic_cap_files[@]}"
    agentic_cap_files=()
    agentic_cap_pid=""
    agentic_signals_end agentic_cap "$1"
}

# agentic_capture_supervise <seconds> <stdin-file> <stdout-file> <stderr-file>
# <cmd...>: the supervisor behind every captured call (agentic_run_captured,
# watchdog.sh's bounded_capture), run between agentic_cap_begin and
# agentic_cap_finish. It runs cmd in its own session and process group with
# stdin from <stdin-file> and its output in the two files, never a pipe, and
# polls for at most <seconds>. However the leader ends (on its own, timed out,
# or on a recorded signal), its whole group is then stopped by
# agentic_stop_group, which decides by the group's liveness, not the leader's.
# So neither the child nor a descendant that holds its output open (a daemon, a
# process that ignores TERM) can make a caller wait for end-of-file: nothing is
# read from a pipe. Anything that survives KILL (an uninterruptible kernel
# wait) is abandoned, never waited for. Launches nothing when a signal is
# recorded. Returns cmd's status, 124 when it ran out of time or had none to
# run with (agentic_timed_out=1), or 1 on a recorded signal (the capture's
# exit returns 130 or 143). Sets agentic_cap_pid (the group's leader),
# agentic_cap_reason ("timed out", "had no time left", "was interrupted" or
# "failed", plus ", child left behind" or ", processes left behind") and
# agentic_cap_left ("stopped them" or "some survived KILL and were left
# behind" when the command exited leaving processes behind).
agentic_cap_pid=""
agentic_cap_reason=""
agentic_cap_left=""
agentic_capture_supervise() {
    local budget=$1 in=$2 out=$3 err=$4 tick end status=0
    shift 4
    agentic_timed_out=0
    agentic_cap_pid="" agentic_cap_reason="" agentic_cap_left=""
    if [ "$budget" -le 0 ]; then
        # shellcheck disable=SC2034 # read by the scripts that source this file
        agentic_timed_out=1
        agentic_cap_reason="had no time left"
        return 124
    fi
    agentic_cap_live || return 1
    # stderr before stdin, so a failed <stdin-file> redirect lands in <stderr-file>.
    setsid "$@" >"$out" 2>"$err" <"$in" &
    agentic_cap_pid=$!
    # The record is checked first, so a signal that landed just before the
    # launch stops the group at once, and again directly before each sleep.
    end=$((${EPOCHREALTIME/[.,]/} + budget * 1000000))
    for ((tick = 0; tick < budget * agentic_ticks_per_sec; tick++)); do
        [ "${EPOCHREALTIME/[.,]/}" -lt "$end" ] || break
        agentic_cap_live || break
        kill -0 "$agentic_cap_pid" 2>/dev/null || break
        agentic_cap_live || break
        sleep "$agentic_tick"
    done
    if ! agentic_cap_live; then
        status=1
        agentic_cap_reason="was interrupted"
    elif kill -0 "$agentic_cap_pid" 2>/dev/null; then
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

# agentic_run_captured <seconds> <cmd...>: like agentic_run_bounded, but for
# callers that capture the output (`x=$(agentic_gh ...)`) (#1191). The command's
# stdout and stderr go to private temp files (mode 600) and stdin is /dev/null;
# after the command has ended and its group is stopped, the files are copied to
# this function's stdout and stderr, partial output included after a timeout,
# and removed. Nothing waits for end-of-file on a pipe a leftover could hold.
# Returns cmd's status, or 124 on a timeout (or with no time left), or 130 (INT)
# or 143 (TERM) when a signal came and the caller's own handler returned.
agentic_run_captured() {
    local status=0
    agentic_cap_begin
    agentic_run_captured_steps "$@" || status=$?
    agentic_cap_finish "$status"
}

# agentic_run_captured_steps <seconds> <cmd...>: the body of a captured call,
# between agentic_cap_begin and agentic_cap_finish. Returns early (to the exit)
# at the first check that finds a signal recorded.
agentic_run_captured_steps() {
    local budget=$1 cap_out cap_err status=0
    shift
    agentic_cap_mktemp cap_out "${TMPDIR:-/tmp}/agentic-call.XXXXXX" || return
    agentic_cap_mktemp cap_err "${TMPDIR:-/tmp}/agentic-call.XXXXXX" || return
    agentic_capture_supervise "$budget" /dev/null "$cap_out" "$cap_err" "$@" || status=$?
    agentic_cap_live || return
    cat -- "$cap_out"
    agentic_cap_live || return
    cat -- "$cap_err" >&2
    agentic_cap_live || return
    case "$agentic_cap_reason" in
        "timed out"* | "had no time left"*)
            agentic_cap_say "agentic: warning: '$1' $agentic_cap_reason (status 124)" \
                || return ;;
    esac
    [ -z "$agentic_cap_left" ] \
        || agentic_cap_say "agentic: warning: '$1' exited (status $status) leaving" \
            "processes behind; $agentic_cap_left" || return
    return "$status"
}

# agentic_gh <args...> / agentic_git <args...>: gh or git, bounded by
# agentic_call_budget and captured (agentic_run_captured). Return 124 at once
# when no time is left. The budget is worked out under the capture's traps, so
# a signal meanwhile launches nothing.
agentic_gh() { agentic_bounded_call gh "$@"; }
agentic_git() { agentic_bounded_call git "$@"; }
agentic_bounded_call() {
    local status=0
    agentic_cap_begin
    agentic_bounded_call_steps "$@" || status=$?
    agentic_cap_finish "$status"
}
agentic_bounded_call_steps() {
    local budget
    agentic_cap_live || return
    budget=$(agentic_call_budget)
    [ "$budget" -gt 0 ] || return 124
    agentic_run_captured_steps "$budget" "$@"
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
