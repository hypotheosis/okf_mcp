#!/usr/bin/env bash
# merge-reviewed.sh <pr>
#
# Merges a PR (merge commit) only when all of these hold, and refuses otherwise:
#   1. the review bot has reviewed the PR's current head commit, or it has
#      already given the maximum number of review rounds (agentic_max_rounds;
#      the final round's non-blocking findings go to follow-up issues);
#   2. every top-level inline comment from the bot has a reply in its thread;
#   3. GitHub reports the PR as CLEAN (mergeable, up to date as required). When
#      it is BLOCKED, the script first resolves the unresolved review threads the
#      bot started (all of them answered, by 2.): a ruleset such as the default
#      branch's requires resolved threads, and an answered thread is not a
#      resolved one. Threads a person started are never resolved; one that is
#      still open is named in the refusal. After resolving, and while GitHub
#      reports UNKNOWN, it re-reads the state for up to $AGENTIC_STATE_WAIT
#      seconds (default 60, counted from the last resolve). That is a polling
#      budget, not a hard limit: see await_merge_state for the total bound.
# The merge is pinned to the inspected head, so a concurrent push makes it fail.
# Every gh and git call is bounded ($AGENTIC_CALL_TIMEOUT, default 60 s); a call
# that runs out of time is a refusal.
# Then fetches origin so the next worktree starts from the new base, and prints
# the PR's base branch and the merge commit.
set -euo pipefail
here=$(cd "$(dirname "$0")" && pwd)
# shellcheck source-path=SCRIPTDIR source=lib.sh
source "$here/lib.sh"

pr=${1:?usage: merge-reviewed.sh <pr>}
repo=$(agentic_repo) || { echo "REFUSE: could not resolve the repository"; exit 1; }
bot=$(agentic_reviewer)

info=$(agentic_gh pr view "$pr" -R "$repo" --json headRefOid,mergeStateStatus,baseRefName) \
    || { echo "REFUSE: could not read PR $pr"; exit 1; }
head=$(jq -r .headRefOid <<<"$info")
state=$(jq -r .mergeStateStatus <<<"$info")
base=$(jq -r '.baseRefName // empty' <<<"$info")
[ -n "$base" ] || { echo "REFUSE: could not read the base branch of PR $pr"; exit 1; }

counts=$(agentic_review_counts "$repo" "$pr" "$bot" "$head") \
    || { echo "REFUSE: could not read the reviews of PR $pr"; exit 1; }
read -r on_head rounds <<<"$counts"
max_rounds=$(agentic_max_rounds)
capped=0
if [ "$on_head" = 0 ]; then
    if [ "$rounds" -lt "$max_rounds" ]; then
        echo "REFUSE: no $bot review on head ${head:0:7} ($rounds of $max_rounds rounds used)"
        exit 1
    fi
    capped=1
fi

comments=$(agentic_api_list "repos/$repo/pulls/$pr/comments") \
    || { echo "REFUSE: could not read the inline comments of PR $pr"; exit 1; }
# shellcheck disable=SC2016 # a jq program: $vars are jq's, not the shell's
unreplied=$(agentic_jq_bot "$bot" '
    (map(select(.in_reply_to_id != null) | .in_reply_to_id) | unique) as $replied
    | map(select((.user.login | is_bot)
                 and .in_reply_to_id == null
                 and ((.id as $i | $replied | index($i)) == null)))
    | map(.id)' -c <<<"$comments")
if [ "$unreplied" != "[]" ]; then
    echo "REFUSE: $bot threads without a reply: $unreplied"
    exit 1
fi

# await_merge_state <also_blocked>: re-reads mergeStateStatus into $state until
# it is no longer UNKNOWN (and, when <also_blocked> is 1, no longer BLOCKED:
# GitHub may still hold the pre-resolve state), for at most $AGENTIC_STATE_WAIT
# seconds from now: no sleep and no new lookup starts after that. Each lookup
# and each sleep is capped by the time left (agentic_deadline, scoped to this
# call); a lookup that runs out of time ends the wait with the state as it was.
# The total is a hard bound: the wait ends within AGENTIC_STATE_WAIT + 2 s (1 s
# of grace, plus up to 1 s from the whole-second clock), and stopping a stalled
# lookup is inside that, because agentic_call_budget reserves
# agentic_cleanup_reserve ($agentic_kill_after + $agentic_reap_after, 10 s by
# default) from the time left. So the last lookup starts that much earlier: the
# state is polled for about AGENTIC_STATE_WAIT - reserve s (about 50 s by
# default), not the whole wait. A lookup's processes are never left running,
# except one that survives KILL, which lib.sh abandons with a warning.
# Sets $waited to the seconds spent.
await_merge_state() {
    local started deadline left rc latest
    started=$(agentic_now)
    deadline=$((started + ${AGENTIC_STATE_WAIT:-60}))
    # One second of grace, so the read after the last sleep still gets a budget.
    agentic_deadline=$((deadline + 1))
    waited=0
    while :; do
        rc=0
        latest=$(agentic_gh pr view "$pr" -R "$repo" --json mergeStateStatus \
            --jq .mergeStateStatus) || rc=$?
        if [ "$rc" = 124 ]; then
            waited=$(($(agentic_now) - started))
            break
        fi
        [ "$rc" = 0 ] || { echo "REFUSE: could not re-read the merge state of PR $pr"; exit 1; }
        state=$latest
        waited=$(($(agentic_now) - started))
        if [ "$state" != UNKNOWN ] && { [ "$1" != 1 ] || [ "$state" != BLOCKED ]; }; then
            break
        fi
        left=$((deadline - $(agentic_now)))
        [ "$left" -gt 0 ] || break
        agentic_sleep "$((left < 3 ? left : 3))"
    done
    agentic_deadline=""
}

# GitHub computes mergeStateStatus lazily; UNKNOWN means it has not finished.
waited=0
[ "$state" != UNKNOWN ] || await_merge_state 0

resolved=0
person_threads=""
if [ "$state" = BLOCKED ]; then
    # shellcheck disable=SC2016 # a GraphQL query: $vars are GraphQL's, not the shell's
    pages=$(agentic_gh api graphql --paginate -f owner="${repo%%/*}" -f name="${repo#*/}" \
        -F pr="$pr" -f query='
        query($owner: String!, $name: String!, $pr: Int!, $endCursor: String) {
          repository(owner: $owner, name: $name) {
            pullRequest(number: $pr) {
              reviewThreads(first: 100, after: $endCursor) {
                pageInfo { hasNextPage endCursor }
                nodes { id isResolved path comments(first: 1) { nodes { author { login } } } }
              }
            }
          }
        }') || { echo "REFUSE: could not read the review threads of PR $pr"; exit 1; }
    threads=$(jq -s -e '
            if length > 0 and all(.[]; (.data.repository.pullRequest.reviewThreads.nodes | type) == "array")
            then [.[].data.repository.pullRequest.reviewThreads.nodes[]] else error end' \
            <<<"$pages" 2>/dev/null) \
        || { echo "REFUSE: could not read the review threads of PR $pr"; exit 1; }
    # shellcheck disable=SC2016 # a jq program: $vars are jq's, not the shell's
    bot_threads=$(agentic_jq_bot "$bot" '
        .[] | select((.isResolved | not) and ((.comments.nodes[0].author.login // "") | is_bot))
        | .id' -r <<<"$threads")
    # shellcheck disable=SC2016
    person_threads=$(agentic_jq_bot "$bot" '
        .[] | select((.isResolved | not) and (((.comments.nodes[0].author.login // "") | is_bot) | not))
        | "\(.path // "(no file)") by \(.comments.nodes[0].author.login // "an unknown user")"' \
        -r <<<"$threads" | paste -sd, - | sed 's/,/, /g')
    for thread_id in $bot_threads; do
        # shellcheck disable=SC2016
        agentic_gh api graphql -f id="$thread_id" -f query='
            mutation($id: ID!) { resolveReviewThread(input: {threadId: $id}) { thread { isResolved } } }' \
            >/dev/null \
            || { echo "REFUSE: could not resolve review thread $thread_id ($resolved resolved" \
                "before it); resolve the threads by hand or retry"; exit 1; }
        resolved=$((resolved + 1))
    done
    [ "$resolved" = 0 ] || await_merge_state 1
fi

if [ "$state" != CLEAN ]; then
    if [ "$state" = BLOCKED ]; then
        if [ -n "$person_threads" ]; then
            echo "REFUSE: mergeStateStatus=$state; unresolved review threads started by a person" \
                "(never resolved by this script): $person_threads"
        elif [ "$resolved" != 0 ]; then
            echo "REFUSE: mergeStateStatus=$state still $waited s after resolving $resolved $bot" \
                "review thread(s); no thread by a person is open, so the block is something else" \
                "(a required check or approval, or a ruleset): see the PR's merge box"
        else
            echo "REFUSE: mergeStateStatus=$state; checked the review threads (none open) and found" \
                "nothing for this script to resolve, so the block is something else (a required" \
                "check or approval, or a ruleset): see the PR's merge box"
        fi
    elif [ "$state" = UNKNOWN ]; then
        echo "REFUSE: mergeStateStatus=UNKNOWN after waiting $waited s for GitHub to compute it"
    else
        echo "REFUSE: mergeStateStatus=$state"
    fi
    exit 1
fi

if [ "$capped" = 1 ]; then
    echo "merging ${head:0:7} under the review round cap ($rounds of $max_rounds rounds;" \
        "the latest $bot review is of an earlier head)"
fi

# --match-head-commit makes GitHub refuse the merge if someone pushed after the
# checks above, so only the reviewed commit can ever be merged.
agentic_gh pr merge "$pr" -R "$repo" --merge --match-head-commit "$head" \
    || { echo "REFUSE: the merge of ${head:0:7} failed: GitHub rejected it (did the head" \
        "change?) or the call timed out (check the PR before retrying)"; exit 1; }
# The merge is done; a failure from here on only means the local view is stale.
# Name the PR's real base (a milestone branch, not always main) and the merge
# commit GitHub made on it; without that commit, use the base's tip after a fetch.
merge_sha=$(agentic_gh pr view "$pr" -R "$repo" --json mergeCommit --jq '.mergeCommit.oid // empty' \
    2>/dev/null) || merge_sha=""
if main_checkout=$(agentic_main_checkout) \
    && agentic_git -C "$main_checkout" fetch -q origin; then
    if [ -z "$merge_sha" ]; then
        merge_sha=$(agentic_git -C "$main_checkout" rev-parse "origin/$base" 2>/dev/null) \
            || merge_sha=""
    fi
    fetched=1
else
    fetched=0
fi
if [ -n "$merge_sha" ]; then
    echo "MERGED PR $pr -> $base ${merge_sha:0:7}"
else
    echo "MERGED PR $pr -> $base; warning: could not find the merge commit"
fi
[ "$fetched" = 1 ] || echo "warning: could not fetch origin afterwards"
echo "next: tools/agentic/remove-worktree.sh <worktree> --branch (frees its Cargo build output)"
