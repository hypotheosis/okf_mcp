#!/usr/bin/env bash
# watch-review.sh <pr>
#
# Waits until the review bot has reviewed the PR's CURRENT head commit, then
# exits 0. Meant to run in the background while a coordinator waits.
#
# - Posts the bot's re-trigger command ($AGENTIC_RETRIGGER, default "/prrb")
#   once if the bot has not reacted since the watcher started after ~90 s, or
#   reacted but not reviewed after ~5 min. A reaction from before the watcher
#   started (an earlier round's) does not count. Never re-triggers once the
#   bot has given the maximum number of review rounds (agentic_max_rounds).
# - Exits 5 at once when the PR is behind main or conflicts with it
#   (mergeStateStatus BEHIND or DIRTY), or when the bot answers with an
#   "out of date" notice: the bot skips such a PR, so waiting is pointless.
#   Merge origin/main into the branch, push, and run the watcher again.
# - Exits 4 at once when the current head has no review and the round cap is
#   reached: merge with merge-reviewed.sh instead of asking again.
# - A failed lookup (PR, reviews, reactions) decides nothing on that pass: no
#   re-trigger and no exit; the next poll retries.
# - Exits 2 once 15 min ($AGENTIC_WATCH_TIMEOUT seconds) have passed since it
#   started, whatever happened in between (re-trigger, API failures, a stalled
#   gh), and prints the bot's latest PR comments: the usual cause is a skip
#   notice such as "out of date with main" (merge origin/main into the branch
#   and push again).
# - Every gh call, including stopping a stalled one, is over by that deadline:
#   its run time is capped by the time left minus the cleanup reserve
#   ($agentic_kill_after + $agentic_reap_after, 10 s by default; lib.sh), and by
#   a per-call cap ($AGENTIC_CALL_TIMEOUT, default 60 s). The final diagnostics
#   get a window of their own, 3 s of run time plus that reserve, so the whole
#   run ends within AGENTIC_WATCH_TIMEOUT + 3 + reserve s (plus 1 s of clock).
set -u
here=$(cd "$(dirname "$0")" && pwd)
# shellcheck source-path=SCRIPTDIR source=lib.sh
source "$here/lib.sh"

pr=${1:?usage: watch-review.sh <pr>}
started=$(agentic_now) # for re-trigger timing
readonly deadline=$((started + ${AGENTIC_WATCH_TIMEOUT:-900})) # overall limit; never moves
agentic_deadline=$deadline # bounds every gh call (lib.sh: agentic_call_budget)
diagnostics_budget=$((3 + $(agentic_cleanup_reserve)))
repo=$(agentic_repo) || { echo "watch-review.sh: could not resolve the repository" >&2; exit 1; }
bot=$(agentic_reviewer)
retrigger=${AGENTIC_RETRIGGER:-/prrb}
poll=30
max_rounds=$(agentic_max_rounds)
retriggered=0
head=""

# Exits 5 when the bot cannot review the PR as it stands: GitHub reports it
# BEHIND or DIRTY (conflicting) against main, or the bot has answered since
# the watcher started with an "out of date" notice. Returns 1 when a lookup
# failed: the PR's state is then unknown, so the caller must not re-trigger
# on this pass.
exit_if_out_of_date() {
    local merge_state notices
    if merge_state=$(agentic_gh pr view "$pr" -R "$repo" --json mergeStateStatus \
        --jq .mergeStateStatus); then
        case "$merge_state" in
            BEHIND | DIRTY)
                echo "PR $pr is $merge_state against main: merge origin/main, resolve any" \
                    "conflicts, push, then watch again; the bot skips it as it is"
                exit 5
                ;;
        esac
    else
        echo "watch-review.sh: could not read the merge state of PR $pr; retrying" >&2
        return 1
    fi
    [ "$retriggered" = 1 ] || return 0
    if ! notices=$(agentic_api_list "repos/$repo/issues/$pr/comments"); then
        echo "watch-review.sh: could not read the comments on PR $pr; retrying" >&2
        return 1
    else
        # shellcheck disable=SC2016 # $started is a jq variable
        notices=$(agentic_jq_bot "$bot" '[.[] | select(.user.login | is_bot)
            | select((.created_at // null) == null or (.created_at | fromdateiso8601) >= $started)
            | select(.body | test("out of date"; "i"))] | length' \
            --argjson started "$started" <<<"$notices")
        if [ "${notices:-0}" != 0 ]; then
            echo "the bot says PR $pr is out of date with main: merge origin/main, push," \
                "then watch again"
            exit 5
        fi
    fi
}

retrigger() {
    if agentic_gh pr comment "$pr" -R "$repo" --body "$retrigger" >/dev/null; then
        retriggered=1
        echo "posted $retrigger on PR $pr"
    fi
}

while :; do
    # Any lookup that fails leaves its answer unknown: no decision is taken on
    # this pass (no re-trigger, no exit), the deadline stays, and the next
    # poll retries.
    if ! head=$(agentic_gh pr view "$pr" -R "$repo" --json headRefOid --jq .headRefOid); then
        echo "watch-review.sh: could not read PR $pr; retrying" >&2
    elif ! counts=$(agentic_review_counts "$repo" "$pr" "$bot" "$head"); then
        echo "watch-review.sh: could not read the reviews of PR $pr; retrying" >&2
    else
        read -r on_head rounds <<<"$counts"
        if [ "$on_head" != 0 ]; then
            echo "reviewed PR $pr at ${head:0:7}"
            exit 0
        fi
        if [ "$rounds" -ge "$max_rounds" ]; then
            echo "review round limit reached ($max_rounds); merge with merge-reviewed.sh" \
                "once the gates pass and every thread has a reply"
            exit 4
        fi
        if ! exit_if_out_of_date; then
            : # state unknown: no re-trigger on this pass
        elif [ "$retriggered" = 0 ]; then
            waited=$(($(agentic_now) - started))
            if ! reactions=$(agentic_api_list "repos/$repo/issues/$pr/reactions"); then
                echo "watch-review.sh: could not read the reactions on PR $pr; retrying" >&2
            else
                # Only a reaction since the watcher started shows the bot saw
                # this push; an earlier round's reaction stays on the PR.
                # shellcheck disable=SC2016 # $started is a jq variable
                reacted=$(agentic_jq_bot "$bot" '[.[] | select(.user.login | is_bot)
                    | select((.created_at // null) == null
                        or (.created_at | fromdateiso8601) >= $started)] | length' \
                    --argjson started "$started" <<<"$reactions")
                if { [ "$reacted" = 0 ] && [ "$waited" -gt 90 ]; } || [ "$waited" -gt 300 ]; then
                    retrigger
                fi
            fi
        fi
    fi
    if [ "$(agentic_now)" -ge "$deadline" ]; then
        echo "TIMEOUT waiting for a review of PR $pr at ${head:-unknown head}"
        agentic_deadline=$(($(agentic_now) + diagnostics_budget))
        agentic_api_list "repos/$repo/issues/$pr/comments" 2>/dev/null \
            | agentic_jq_bot "$bot" '.[] | select(.user.login | is_bot) | .body' -r \
            | tail -5
        exit 2
    fi
    left=$((deadline - $(agentic_now)))
    agentic_sleep "$((left < poll ? left : poll))"
done
