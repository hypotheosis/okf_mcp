#!/usr/bin/env bash
# signal-sweep.sh <tools> <log> <region> <points> <call> [<target>]: the driver of
# run.sh's signal sweep (#1193). Checks lib.sh's "Signals" invariant, that a
# signal lets at most the one statement it lands before run, by sending TERM
# before one statement of a helper and recording every external command and
# every echo that runs after it.
#
# It sources lib.sh, watchdog.sh's bounded_capture and remove-worktree.sh's
# clean_build_output_steps, puts wrappers in front of the external commands
# the helpers run (and echo), and runs <call> with a TERM handler that returns.
# A DEBUG trap (bash runs it before every simple command; `set -T` extends it
# into functions) numbers each statement of this shell that runs inside the
# function <region>, the helper's body.
#   - Without <target>, each statement (function:line) of the functions named
#     in <points> (space-separated) that runs is written to <log>, one per
#     line: the points to send TERM before.
#   - With <target>, TERM is sent to this shell right before the first
#     statement at <target>, and <log> gets "INJECT <n> <statement>", then one
#     line "<n> <command> <calling function>" per wrapped command, where <n> is
#     the number of the statement that ran it (a subshell inherits the number
#     of the statement that started it); after the signal, each statement of
#     <region> is added the same way ("<n> <first word> <function>"), as a
#     background job can be ended before its wrapper's line is written. What
#     ran after the signal has an <n>
#     above the injected one; run.sh allows only the undoing (agentic_stop_group
#     and agentic_cap_finish).
# Ends with "rc=<status of call>" and the caller's TERM trap on stdout.
set -u
sweep_tools=$1 sweep_log=$2 sweep_region=$3 sweep_points=$4 sweep_call=$5
sweep_target=${6:-}
# shellcheck source=/dev/null # the files are found (and cut) at run time
source "$sweep_tools/lib.sh"
# shellcheck source=/dev/null
source <(sed -n "/^bounded_capture\(_steps\)\{0,1\}() {/,/^}/p" "$sweep_tools/watchdog.sh")
# shellcheck source=/dev/null
source <(sed -n "/^clean_build_output_steps() {/,/^}/p" "$sweep_tools/remove-worktree.sh")
# shellcheck disable=SC2034 # read by lib.sh
agentic_kill_after=1 agentic_reap_after=1

# clean_build_output: remove-worktree.sh's top level around its steps.
clean_build_output() {
    local status=0
    agentic_cap_begin
    clean_build_output_steps || status=$?
    agentic_cap_finish "$status"
}

sweep_seq=0 sweep_done=0 sweep_main=$BASHPID
sweep_note() {
    [ -n "$sweep_target" ] || return 0
    printf '%s %s %s\n' "$sweep_seq" "$1" "${FUNCNAME[2]:-main}" >>"$sweep_log"
}
# The wrappers. setsid and the timer's sleep always run in the background
# (`... &`), so they exec: the job's pid stays the command's.
for sweep_cmd in cat sed tail mktemp rm date; do
    eval "$sweep_cmd() { sweep_note $sweep_cmd; command $sweep_cmd \"\$@\"; }"
done
setsid() { sweep_note setsid; exec setsid "$@"; }
sleep() {
    sweep_note sleep
    case "$1" in 0.*) command sleep 0.01 ;; *) exec sleep "$@" ;; esac
}
echo() { sweep_note echo; builtin echo "$@"; }

# sweep_debug <line> <functions> <statement>: the DEBUG trap's work.
sweep_debug() {
    [ "$BASHPID" = "$sweep_main" ] || return 0
    case " $2 " in *" $sweep_region "*) ;; *) return 0 ;; esac
    case "${2%% *}" in
        cat | sed | tail | mktemp | rm | date | setsid | sleep | echo | sweep_note) return 0 ;;
    esac
    sweep_seq=$((sweep_seq + 1))
    # After the signal, each statement too: a background job (`... &`) can be
    # ended before its wrapper writes its line.
    [ "$sweep_done" = 0 ] \
        || printf '%s %s %s\n' "$sweep_seq" "${3%% *}" "${2%% *}" >>"$sweep_log"
    if [ -z "$sweep_target" ]; then
        case " $sweep_points " in
            *" ${2%% *} "*) printf '%s\n' "${2%% *}:$1" >>"$sweep_log" ;;
        esac
    elif [ "$sweep_done" = 0 ] && [ "${2%% *}:$1" = "$sweep_target" ]; then
        sweep_done=1
        printf 'INJECT %s %s\n' "$sweep_seq" "$3" >>"$sweep_log"
        kill -TERM "$BASHPID"
    fi
}

trap 'printf "handled\n"' TERM
set -T
trap 'sweep_debug "$LINENO" "${FUNCNAME[*]:-}" "$BASH_COMMAND"' DEBUG
eval "$sweep_call"
sweep_rc=$?
trap - DEBUG
printf 'rc=%s\n' "$sweep_rc"
trap -p TERM
