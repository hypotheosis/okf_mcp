#!/usr/bin/env bash
# Shell tests for tools/agentic. Run with `just test-agentic`, or directly.
#
# A stub `gh` (stub/gh) sits first on PATH and answers from per-test fixture
# files, watch-review.sh runs on a fake clock (stub/fake-now, stub/fake-sleep),
# and every git repository is a throwaway one under a temporary directory, so
# nothing touches GitHub, the real checkout or the wall clock (the gate and
# watchdog tests wait about a second each).
#
# Usage: run.sh [<name-substring>...]
#   With arguments, only the tests whose names contain one of them run.
#
# Parallelism (#1216). Most of the time goes on real sleeps, so up to
# $AGENTIC_TEST_JOBS tests run at once: 4 by default, and never more than 4 (a
# larger value is clamped to 4 with a note on stderr); 1 runs them one at a time.
# Each test is its own `run.sh --one <name>` process in its own process group, with
# its own temporary directory, git repositories and fixtures, so tests share no
# files, locks, branches or process scans. When a test ends, anything it left in
# its group is stopped before the result is reported. The tests in
# $serial_tests assert upper bounds on wall-clock time; they run alone after
# the others, so machine load from the other tests can't trip them. Results
# are printed in declaration order whatever order they finished in.
# Set AGENTIC_TEST_JOBS=1 to reproduce a failure without parallelism.
#
# Exit status: 0 when every test passes, 1 otherwise.
set -u
tests_dir=$(cd "$(dirname "$0")" && pwd)
tools=$(dirname "$tests_dir")
script=$tests_dir/$(basename "$0")
one=""
if [ "${1:-}" = --one ]; then
    one=$2
    shift 2
fi
work=$(mktemp -d)
work=$(cd "$work" && pwd -P)
trap 'rm -rf "$work"' EXIT

# The on-the-clock tests: they bound real elapsed time tightly, so they run
# serially, after the parallel ones.
serial_tests=(
    test_merge_refuses_on_hung_call
    test_merge_state_wait_is_bounded_with_a_slow_lookup
    test_watch_stalled_gh_still_ends_at_deadline
    test_a_stalled_call_is_stopped_by_the_deadline_not_after_it
    test_gate_returns_the_gate_exit_status
    test_fake_gate_wait_fails_loudly_when_its_marker_never_appears
    test_captured_call_returns_in_time_when_a_setsid_child_holds_stdout
    test_captured_call_returns_at_once_when_the_leader_leaves_a_holder_behind
    test_watchdog_ignores_a_reader_that_uses_cpu
)

export PATH="$tests_dir/stub:$PATH"
export AGENTIC_REPO=owner/repo AGENTIC_RETRIGGER=/prrb
# The scripts' defaults are under test, so none of these come from the caller.
unset AGENTIC_REVIEWER AGENTIC_MAX_REVIEW_ROUNDS AGENTIC_NOW_CMD AGENTIC_SLEEP_CMD
# gate.sh's build-dir seeding would run a machine-local tool; tests that cover
# it name a stub.
export AGENTIC_SEED_CMD=
# The scripts under test poll every 0.1 s by default; a test that makes many
# calls on a fake clock would pay that per call. Tests of the tick itself set
# their own value.
export AGENTIC_TICKS_PER_SEC=100

bot='brooke-reviews[bot]'
head_a=aaaaaaa1111111111111111111111111111111111
head_b=bbbbbbb2222222222222222222222222222222222

git_quiet() {
    git -c user.name=test -c user.email=test@example.invalid \
        -c commit.gpgsign=false -c core.hooksPath=/dev/null "$@"
}

# A main checkout with an origin, and a linked worktree OUTSIDE the main
# checkout (a sibling directory), as `git worktree add ../wt` would make. Built
# by the first new_fixtures of a test, in the test's own $work.
repo=$work/repo
build_world() {
    [ ! -d "$repo" ] || return 0
    git init -q -b main "$repo"
    git_quiet -C "$repo" commit -q --allow-empty -m init
    git clone -q --bare "$repo" "$work/origin.git"
    git -C "$repo" remote add origin "$work/origin.git"
    git -C "$repo" fetch -q origin
    git -C "$repo" worktree add -q -b wt "$work/wt"
    mkdir "$work/wt2" "$work/elsewhere"
}

# --- helpers (each test runs in its own subshell, so `exit 1` fails just it) ---

new_fixtures() {
    build_world
    fx=$(mktemp -d "$work/fx.XXXXXX")
    mkdir "$fx/api"
    : >"$fx/calls.log"
    export GH_FIXTURES=$fx
}

# A user whose login shares the bot's prefix but is not the bot.
impostor='brooke-example'

# old_reviews <n>: n bot reviews, all of an earlier head than the PR's.
old_reviews() {
    jq -n --arg bot "$bot" --argjson n "$1" '[range($n) | {user: {login: $bot}, commit_id: "0ld"}]' \
        >"$fx/api/pulls_7_reviews.1.json"
}

# good_pr <head>: a PR whose every merge check passes for <head>.
good_pr() {
    printf '{"headRefOid":"%s","mergeStateStatus":"CLEAN","baseRefName":"main"}\n' "$1" \
        >"$fx/pr-view.json"
    printf '[{"user":{"login":"%s"},"commit_id":"%s"}]\n' "$bot" "$1" \
        >"$fx/api/pulls_7_reviews.1.json"
    printf '[{"id":1,"in_reply_to_id":null,"user":{"login":"%s"}},
             {"id":2,"in_reply_to_id":1,"user":{"login":"author"}}]\n' "$bot" \
        >"$fx/api/pulls_7_comments.1.json"
}

# A full first page of 30 bot reviews of an older commit; the current-head
# review then has to be on page two. The round cap is raised so that the 30
# old reviews alone can't satisfy the scripts.
old_reviews_page() {
    export AGENTIC_MAX_REVIEW_ROUNDS=100
    jq -n --arg bot "$bot" '[range(30) | {user: {login: $bot}, commit_id: "0ld"}]' \
        >"$fx/api/pulls_7_reviews.1.json"
    printf '[{"user":{"login":"%s"},"commit_id":"%s"}]\n' "$bot" "$head_a" \
        >"$fx/api/pulls_7_reviews.2.json"
}

die() {
    echo "  $*"
    printf '  output:\n%s\n' "${out:-}" | sed 's/^/    /'
    exit 1
}
want_rc() { [ "$rc" = "$1" ] || die "exit $rc, want $1"; }
want_out() { grep -qF -- "$1" <<<"$out" || die "output lacks '$1'"; }
want_no_out() { ! grep -qF -- "$1" <<<"$out" || die "output has '$1'"; }
want_not_merged() { [ ! -e "$fx/merged" ] || die "merged $(cat "$fx/merged")"; }

# The real-time timeout only stops a broken script from hanging the suite.
run_merge() {
    out=$(cd "$repo" && timeout 15 "$tools/merge-reviewed.sh" 7 2>&1)
    rc=$?
}

# block_gh: every gh call hangs from now on; the EXIT trap kills whatever the
# stub started, in case a script under test leaves it behind.
block_gh() {
    touch "$fx/block"
    trap '[ -s "$fx/blocked.pids" ] && xargs kill -KILL <"$fx/blocked.pids" 2>/dev/null' EXIT
}

# want_no_blocked_left: every process the blocking stub started is gone.
want_no_blocked_left() {
    [ -s "$fx/blocked.pids" ] || die "the stub was never called"
    local pid
    while read -r pid; do
        ! kill -0 "$pid" 2>/dev/null || die "stub process $pid outlived the script"
    done <"$fx/blocked.pids"
}

# The watcher on a fake clock starting at 1000 s. The real-time timeout only
# stops a broken watcher from hanging the suite; it is never reached when the
# watcher works.
run_watch() {
    echo 1000 >"$fx/clock"
    out=$(cd "$repo" && AGENTIC_NOW_CMD=fake-now AGENTIC_SLEEP_CMD=fake-sleep \
        timeout 15 "$tools/watch-review.sh" 7 2>&1)
    rc=$?
    clock=$(cat "$fx/clock")
}

# --- merge-reviewed.sh ---

# thread <id> <resolved: true|false> <author> [<path>]: one reviewThreads node.
thread() {
    jq -n --arg id "$1" --argjson r "$2" --arg who "$3" --arg path "${4:-src/lib.rs}" \
        '{id: $id, isResolved: $r, path: $path, comments: {nodes: [{author: {login: $who}}]}}'
}

# threads_page <page> <thread json>...: a GraphQL reviewThreads page.
threads_page() {
    local page=$1
    shift
    printf '%s\n' "$@" | jq -s '{data: {repository: {pullRequest: {reviewThreads: {nodes: .}}}}}' \
        >"$fx/graphql-threads.$page.json"
}

# blocked_pr <head>: good_pr, but GitHub reports BLOCKED until threads are resolved.
blocked_pr() {
    good_pr "$1"
    printf '{"headRefOid":"%s","mergeStateStatus":"BLOCKED","baseRefName":"main"}\n' "$1" \
        >"$fx/pr-view.json"
}

# run_merge_fake_clock: run_merge with the fake clock, so waits take no real time.
run_merge_fake_clock() {
    echo 1000 >"$fx/clock"
    out=$(cd "$repo" && AGENTIC_NOW_CMD=fake-now AGENTIC_SLEEP_CMD=fake-sleep \
        timeout 15 "$tools/merge-reviewed.sh" 7 2>&1)
    rc=$?
}

want_resolved() { [ "$(cat "$fx/resolved" 2>/dev/null | paste -sd,)" = "$1" ] || die "resolved '$(paste -sd, "$fx/resolved" 2>/dev/null)', want '$1'"; }
want_none_resolved() { [ ! -e "$fx/resolved" ] || die "resolved $(paste -sd, "$fx/resolved")"; }

test_merge_refuses_unreviewed_head() {
    new_fixtures; good_pr "$head_a"
    printf '[{"user":{"login":"%s"},"commit_id":"%s"}]\n' "$bot" "$head_b" \
        >"$fx/api/pulls_7_reviews.1.json"
    run_merge
    want_rc 1; want_out "REFUSE: no brooke-reviews[bot] review"; want_not_merged
}

test_merge_refuses_unreplied_thread() {
    new_fixtures; good_pr "$head_a"
    printf '[{"id":1,"in_reply_to_id":null,"user":{"login":"%s"}}]\n' "$bot" \
        >"$fx/api/pulls_7_comments.1.json"
    run_merge
    want_rc 1; want_out "without a reply: [1]"; want_not_merged
}

test_merge_refuses_unclean_state() {
    new_fixtures; good_pr "$head_a"
    printf '{"headRefOid":"%s","mergeStateStatus":"BEHIND","baseRefName":"main"}\n' "$head_a" >"$fx/pr-view.json"
    run_merge
    want_rc 1; want_out "mergeStateStatus=BEHIND"; want_not_merged
}

test_merge_refuses_on_api_error() {
    new_fixtures; good_pr "$head_a"
    touch "$fx/api/pulls_7_reviews.fail"
    run_merge
    want_rc 1; want_out "REFUSE: could not read the reviews"; want_not_merged
}

test_merge_refuses_on_pr_view_error() {
    new_fixtures; good_pr "$head_a"; touch "$fx/pr-view.fail"
    run_merge
    want_rc 1; want_out "REFUSE: could not read PR 7"; want_not_merged
}

test_merge_refuses_on_comments_api_error() {
    new_fixtures; good_pr "$head_a"; touch "$fx/api/pulls_7_comments.fail"
    run_merge
    want_rc 1; want_out "REFUSE: could not read the inline comments"; want_not_merged
}

test_merge_ignores_prefix_sharing_reviewer() {
    new_fixtures; good_pr "$head_a"
    # Three reviews of the current head by someone who is not the bot: they
    # must neither count as the head's review nor use up rounds.
    jq -n --arg who "$impostor" --arg head "$head_a" \
        '[range(3) | {user: {login: $who}, commit_id: $head}]' >"$fx/api/pulls_7_reviews.1.json"
    run_merge
    want_rc 1; want_out "(0 of 3 rounds used)"; want_not_merged
}

test_merge_ignores_prefix_sharing_thread() {
    new_fixtures; good_pr "$head_a"
    printf '[{"id":5,"in_reply_to_id":null,"user":{"login":"%s"}}]\n' "$impostor" \
        >"$fx/api/pulls_7_comments.1.json"
    run_merge
    want_rc 0; want_out "MERGED PR 7"
}

test_merge_matches_bot_login_without_suffix() {
    new_fixtures; good_pr "$head_a"
    out=$(cd "$repo" && AGENTIC_REVIEWER=brooke-reviews "$tools/merge-reviewed.sh" 7 2>&1)
    rc=$?
    want_rc 0; want_out "MERGED PR 7"
}

test_merge_refuses_on_hung_call() {
    new_fixtures; good_pr "$head_a"; block_gh
    started=$SECONDS
    AGENTIC_CALL_TIMEOUT=1 run_merge
    want_rc 1; want_out "REFUSE: could not read PR 7"; want_not_merged
    [ $((SECONDS - started)) -le 4 ] || die "took $((SECONDS - started)) s"
    want_no_blocked_left
}

test_merge_refuses_head_changed_before_merge() {
    new_fixtures; good_pr "$head_a"
    echo "$head_b" >"$fx/merge-head"
    run_merge
    want_rc 1; want_out "REFUSE: the merge of aaaaaaa failed"; want_not_merged
}

test_merge_finds_review_on_page_two() {
    new_fixtures; good_pr "$head_a"; old_reviews_page
    run_merge
    want_rc 0; want_out "MERGED PR 7"; want_no_out "round cap"
}

test_merge_below_cap_requires_current_head_review() {
    new_fixtures; good_pr "$head_a"; old_reviews 2
    run_merge
    want_rc 1; want_out "REFUSE: no brooke-reviews[bot] review on head aaaaaaa (2 of 3 rounds used)"
    want_not_merged
}

test_merge_at_cap_merges_newer_head() {
    new_fixtures; good_pr "$head_a"; old_reviews 3
    run_merge
    want_rc 0; want_out "under the review round cap (3 of 3 rounds"; want_out "MERGED PR 7"
    grep -qF -- "--match-head-commit $head_a" "$fx/calls.log" || die "merge not pinned"
}

test_merge_at_cap_refuses_unreplied_thread() {
    new_fixtures; good_pr "$head_a"; old_reviews 3
    printf '[{"id":1,"in_reply_to_id":null,"user":{"login":"%s"}}]\n' "$bot" \
        >"$fx/api/pulls_7_comments.1.json"
    run_merge
    want_rc 1; want_out "without a reply: [1]"; want_not_merged
}

test_merge_at_cap_refuses_unclean_state() {
    new_fixtures; good_pr "$head_a"; old_reviews 3
    printf '{"headRefOid":"%s","mergeStateStatus":"BLOCKED","baseRefName":"main"}\n' "$head_a" >"$fx/pr-view.json"
    run_merge
    want_rc 1; want_out "mergeStateStatus=BLOCKED"; want_not_merged
}

test_merge_merges_when_all_checks_pass() {
    new_fixtures; good_pr "$head_a"
    run_merge
    want_rc 0; want_out "MERGED PR 7 -> main"
    [ "$(cat "$fx/merged")" = "$head_a" ] || die "merged the wrong head"
    grep -qF -- "pr merge 7 -R owner/repo --merge --match-head-commit $head_a" "$fx/calls.log" \
        || die "merge not pinned to the head"
}

# base_pr <head> <base> [<merge commit>]: good_pr, but with a PR base other than main.
base_pr() {
    good_pr "$1"
    jq -n --arg h "$1" --arg b "$2" --arg m "${3:-}" \
        '{headRefOid: $h, mergeStateStatus: "CLEAN", baseRefName: $b}
         + (if $m == "" then {} else {mergeCommit: {oid: $m}} end)' >"$fx/pr-view.json"
}

test_merge_reports_the_base_and_merge_commit() {
    new_fixtures; base_pr "$head_a" milestone/9-x ccccccc3333333333333333333333333333333333
    run_merge
    want_rc 0; want_out "MERGED PR 7 -> milestone/9-x ccccccc"; want_no_out "-> main"
}

test_merge_falls_back_to_the_base_tip() {
    new_fixtures; base_pr "$head_a" milestone/9-x
    # Put the base on origin one commit ahead of main, so its tip is not main's.
    git_quiet -C "$repo" branch -f ms main
    git_quiet -C "$repo" worktree add -q --detach "$work/ms" ms
    git_quiet -C "$work/ms" commit -q --allow-empty -m ms
    git_quiet -C "$work/ms" push -q origin HEAD:refs/heads/milestone/9-x
    tip=$(git -C "$work/ms" rev-parse --short HEAD)
    git -C "$repo" worktree remove --force "$work/ms"
    git -C "$repo" branch -D ms -q
    run_merge
    want_rc 0; want_out "MERGED PR 7 -> milestone/9-x $tip"
}

test_merge_refuses_without_a_base_branch() {
    new_fixtures; good_pr "$head_a"
    # A PR with no baseRefName at all, then one whose baseRefName is null.
    printf '{"headRefOid":"%s","mergeStateStatus":"CLEAN"}\n' "$head_a" >"$fx/pr-view.json"
    run_merge
    want_rc 1; want_out "REFUSE: could not read the base branch of PR 7"; want_not_merged
    printf '{"headRefOid":"%s","mergeStateStatus":"CLEAN","baseRefName":null}\n' "$head_a" \
        >"$fx/pr-view.json"
    run_merge
    want_rc 1; want_out "REFUSE: could not read the base branch of PR 7"; want_not_merged
}

test_merge_keeps_the_sha_when_origin_is_unavailable() {
    new_fixtures; base_pr "$head_a" milestone/9-x ccccccc3333333333333333333333333333333333
    # The merge has happened; a fetch that fails must not hide it. Put origin
    # back however the test ends (the repository is shared by every test).
    trap 'git -C "$repo" remote set-url origin "$work/origin.git"' EXIT
    git -C "$repo" remote set-url origin "$work/no-such-origin.git"
    run_merge
    want_rc 0; want_out "MERGED PR 7 -> milestone/9-x ccccccc"
    want_out "warning: could not fetch origin afterwards"
    [ "$(cat "$fx/merged")" = "$head_a" ] || die "merged the wrong head"
}

test_merge_warns_when_no_merge_commit_can_be_found() {
    # No mergeCommit from gh, and origin has no such base branch.
    new_fixtures; base_pr "$head_a" milestone/absent
    run_merge
    want_rc 0
    want_out "MERGED PR 7 -> milestone/absent; warning: could not find the merge commit"
}

test_merge_resolves_answered_bot_threads_then_merges() {
    new_fixtures; blocked_pr "$head_a"
    threads_page 1 "$(thread T1 false "$bot")" "$(thread T2 true "$bot")" \
        "$(thread T3 false brooke-reviews)"
    echo CLEAN >"$fx/merge-state-seq"
    run_merge_fake_clock
    want_rc 0; want_out "MERGED PR 7 -> main"; want_resolved "T1,T3"
}

test_merge_resolves_bot_threads_on_every_page() {
    new_fixtures; blocked_pr "$head_a"
    threads_page 1 "$(thread T1 false "$bot")"
    threads_page 2 "$(thread T2 false "$bot")"
    echo CLEAN >"$fx/merge-state-seq"
    run_merge_fake_clock
    want_rc 0; want_out "MERGED PR 7"; want_resolved "T1,T2"
}

test_merge_resolves_nothing_when_a_bot_thread_is_unanswered() {
    new_fixtures; blocked_pr "$head_a"
    printf '[{"id":1,"in_reply_to_id":null,"user":{"login":"%s"}}]\n' "$bot" \
        >"$fx/api/pulls_7_comments.1.json"
    threads_page 1 "$(thread T1 false "$bot")"
    run_merge
    want_rc 1; want_out "without a reply: [1]"; want_none_resolved; want_not_merged
}

test_merge_refuses_a_persons_unresolved_thread_and_leaves_it() {
    new_fixtures; blocked_pr "$head_a"
    threads_page 1 "$(thread T1 false "$bot")" "$(thread T2 false alice docs/a.md)"
    run_merge_fake_clock
    want_rc 1; want_out "unresolved review threads started by a person"
    want_out "docs/a.md by alice"; want_resolved "T1"; want_not_merged
}

test_merge_waits_for_an_unknown_state_then_merges() {
    new_fixtures; good_pr "$head_a"
    printf '{"headRefOid":"%s","mergeStateStatus":"UNKNOWN","baseRefName":"main"}\n' "$head_a" \
        >"$fx/pr-view.json"
    printf 'UNKNOWN\nCLEAN\n' >"$fx/merge-state-seq"
    run_merge_fake_clock
    want_rc 0; want_out "MERGED PR 7"; want_none_resolved
}

test_merge_refuses_an_unknown_state_that_never_resolves() {
    new_fixtures; good_pr "$head_a"
    printf '{"headRefOid":"%s","mergeStateStatus":"UNKNOWN","baseRefName":"main"}\n' "$head_a" \
        >"$fx/pr-view.json"
    run_merge_fake_clock
    want_rc 1; want_out "mergeStateStatus=UNKNOWN after waiting 51 s"; want_not_merged
    # The 10 s cleanup reserve ends the polling early (1061 - 10 = 1051), and the
    # message reports that, not the 60 s of AGENTIC_STATE_WAIT (#1169).
    [ "$(cat "$fx/clock")" -le 1100 ] || die "waited past the bound"
}

test_merge_state_wait_is_bounded_with_a_slow_lookup() {
    new_fixtures; good_pr "$head_a"
    printf '{"headRefOid":"%s","mergeStateStatus":"UNKNOWN","baseRefName":"main"}\n' "$head_a" \
        >"$fx/pr-view.json"
    # Each state lookup would take 20 s and ignores TERM, the worst cleanup that
    # can be stopped. The documented bound (await_merge_state) is
    # AGENTIC_STATE_WAIT + 2 = 4 s, cleanup included (#1169); the test allows
    # 2 s more for the script's own start-up and the stub calls before the wait
    # (a loaded machine).
    echo 20 >"$fx/lookup-sleep"
    local bound_ms=4000 tolerance_ms=2000 started_ms elapsed_ms pgid
    started_ms=$(($(date +%s%N) / 1000000))
    out=$(cd "$repo" && AGENTIC_STATE_WAIT=2 agentic_kill_after=1 agentic_reap_after=1 \
        timeout 30 "$tools/merge-reviewed.sh" 7 2>&1)
    rc=$?
    elapsed_ms=$(($(date +%s%N) / 1000000 - started_ms))
    want_rc 1; want_out "mergeStateStatus=UNKNOWN after waiting"; want_not_merged
    [ "$elapsed_ms" -le $((bound_ms + tolerance_ms)) ] \
        || die "took $elapsed_ms ms; the bound is $bound_ms ms (+$tolerance_ms ms tolerance)"
    # The lookup was cut off, not left to finish, and none of it is left running.
    [ ! -e "$fx/lookup-finished" ] || die "the slow lookup ran to its end"
    pgid=$(cat "$fx/lookup-pgid") || die "the slow lookup never ran"
    ! kill -0 -- "-$pgid" 2>/dev/null || die "the lookup's process group $pgid is still running"
}

test_merge_says_what_it_checked_when_blocked_for_another_reason() {
    new_fixtures; blocked_pr "$head_a"
    threads_page 1 "$(thread T1 true "$bot")"
    run_merge_fake_clock
    want_rc 1; want_out "mergeStateStatus=BLOCKED; checked the review threads (none open)"
    want_none_resolved; want_not_merged
}

test_merge_names_the_wait_when_still_blocked_after_resolving() {
    new_fixtures; blocked_pr "$head_a"
    threads_page 1 "$(thread T1 false "$bot")"
    run_merge_fake_clock
    want_rc 1; want_out "still 51 s after resolving 1"; want_resolved "T1"
    want_not_merged
}

test_merge_refuses_when_the_threads_cannot_be_read() {
    new_fixtures; blocked_pr "$head_a"; touch "$fx/graphql-threads.fail"
    run_merge
    want_rc 1; want_out "REFUSE: could not read the review threads"; want_not_merged
}

test_merge_refuses_when_a_thread_cannot_be_resolved() {
    new_fixtures; blocked_pr "$head_a"; touch "$fx/graphql-resolve.fail"
    threads_page 1 "$(thread T1 false "$bot")"
    run_merge
    want_rc 1; want_out "REFUSE: could not resolve review thread T1"; want_not_merged
}

# --- watch-review.sh ---

test_watch_succeeds_on_current_head_review() {
    new_fixtures; good_pr "$head_a"
    run_watch
    want_rc 0; want_out "reviewed PR 7 at aaaaaaa"
}

test_watch_finds_review_on_page_two() {
    new_fixtures; good_pr "$head_a"; old_reviews_page
    run_watch
    want_rc 0; want_out "reviewed PR 7"
}

test_watch_at_cap_exits_4_without_retrigger() {
    new_fixtures; good_pr "$head_a"; old_reviews 3
    run_watch
    want_rc 4; want_out "review round limit reached (3)"
    [ "$clock" = 1000 ] || die "waited until $clock instead of exiting at once"
    ! grep -q '^pr comment' "$fx/calls.log" || die "re-triggered at the cap"
}

test_watch_gives_up_when_pr_view_keeps_failing() {
    # 9 passes of 100 s instead of 30 of 30 s: still several past the 90 s and
    # 300 s re-trigger thresholds, and the deadline is the same.
    export AGENTIC_WATCH_POLL=100
    new_fixtures; good_pr "$head_a"; touch "$fx/pr-view.fail"
    printf '[{"user":{"login":"%s"},"body":"skipped: out of date with main"}]\n' "$bot" \
        >"$fx/api/issues_7_comments.1.json"
    run_watch
    want_rc 2; want_out "TIMEOUT"; want_out "out of date with main"
    [ "$clock" -ge 1900 ] && [ "$clock" -le 1930 ] || die "gave up at $clock, want 1900..1930"
}

test_watch_retrigger_keeps_overall_deadline() {
    new_fixtures; good_pr "$head_a"
    echo '[]' >"$fx/api/pulls_7_reviews.1.json"
    run_watch
    want_rc 2; want_out "posted /prrb on PR 7"; want_out "TIMEOUT"
    [ "$(grep -c '^pr comment 7 .*--body /prrb' "$fx/calls.log")" = 1 ] || die "re-triggered more than once"
    [ "$clock" -ge 1900 ] && [ "$clock" -le 1930 ] || die "gave up at $clock, want 1900..1930"
}

# --- watchdog.sh ---

test_watch_reviews_api_error_never_retriggers() {
    # 9 passes of 100 s instead of 30 of 30 s: still several past the 90 s and
    # 300 s re-trigger thresholds, and the deadline is the same.
    export AGENTIC_WATCH_POLL=100
    new_fixtures; good_pr "$head_a"; touch "$fx/api/pulls_7_reviews.fail"
    run_watch
    want_rc 2; want_out "TIMEOUT"; want_out "could not read the reviews"
    ! grep -q '^pr comment' "$fx/calls.log" || die "re-triggered with the round count unknown"
    [ "$clock" -ge 1900 ] && [ "$clock" -le 1930 ] || die "gave up at $clock, want 1900..1930"
}

test_watch_reactions_api_error_never_retriggers() {
    # 9 passes of 100 s instead of 30 of 30 s: still several past the 90 s and
    # 300 s re-trigger thresholds, and the deadline is the same.
    export AGENTIC_WATCH_POLL=100
    new_fixtures; good_pr "$head_a"
    echo '[]' >"$fx/api/pulls_7_reviews.1.json"
    touch "$fx/api/issues_7_reactions.fail"
    run_watch
    want_rc 2
    ! grep -q '^pr comment' "$fx/calls.log" || die "re-triggered with the reactions unknown"
}

test_watch_ignores_prefix_sharing_reaction() {
    new_fixtures; good_pr "$head_a"
    echo '[]' >"$fx/api/pulls_7_reviews.1.json"
    printf '[{"user":{"login":"%s"},"content":"eyes"}]\n' "$impostor" \
        >"$fx/api/issues_7_reactions.1.json"
    run_watch
    # The bot hasn't reacted, so the re-trigger is due after ~90 s (the first
    # poll past 90 s is at 1120), not the ~5 min it waits after a bot reaction.
    [ "$(cat "$fx/comment-times" 2>/dev/null)" = 1120 ] \
        || die "re-triggered at $(cat "$fx/comment-times" 2>/dev/null), want 1120"
}

test_watch_bot_reaction_delays_retrigger() {
    new_fixtures; good_pr "$head_a"
    echo '[]' >"$fx/api/pulls_7_reviews.1.json"
    # Reacted at 1000 s, as the watcher starts: this push's reaction.
    printf '[{"user":{"login":"%s"},"content":"eyes","created_at":"1970-01-01T00:16:40Z"}]\n' \
        "$bot" >"$fx/api/issues_7_reactions.1.json"
    run_watch
    [ "$(cat "$fx/comment-times" 2>/dev/null)" = 1330 ] \
        || die "re-triggered at $(cat "$fx/comment-times" 2>/dev/null), want 1330"
}

test_watch_earlier_rounds_reaction_does_not_delay_retrigger() {
    new_fixtures; good_pr "$head_a"
    echo '[]' >"$fx/api/pulls_7_reviews.1.json"
    # Reacted at 600 s, before the watcher started at 1000 s: an earlier
    # round's, so the re-trigger is due after ~90 s as if there were none.
    printf '[{"user":{"login":"%s"},"content":"eyes","created_at":"1970-01-01T00:10:00Z"}]\n' \
        "$bot" >"$fx/api/issues_7_reactions.1.json"
    run_watch
    [ "$(cat "$fx/comment-times" 2>/dev/null)" = 1120 ] \
        || die "re-triggered at $(cat "$fx/comment-times" 2>/dev/null), want 1120"
}

test_watch_merge_state_error_never_retriggers() {
    # 9 passes of 100 s instead of 30 of 30 s: still several past the 90 s and
    # 300 s re-trigger thresholds, and the deadline is the same.
    export AGENTIC_WATCH_POLL=100
    new_fixtures; good_pr "$head_a"
    echo '[]' >"$fx/api/pulls_7_reviews.1.json"
    touch "$fx/merge-state.fail"
    run_watch
    want_rc 2; want_out "could not read the merge state"
    ! grep -q '^pr comment' "$fx/calls.log" || die "re-triggered with the merge state unknown"
}

test_watch_ignores_an_earlier_rounds_out_of_date_notice() {
    new_fixtures; good_pr "$head_a"
    echo '[]' >"$fx/api/pulls_7_reviews.1.json"
    # Posted at 600 s, before the watcher started at 1000 s.
    printf '[{"user":{"login":"%s"},"created_at":"1970-01-01T00:10:00Z","body":"This PR is out of date with main"}]\n' \
        "$bot" >"$fx/api/issues_7_comments.1.json"
    run_watch
    want_rc 2; want_out "TIMEOUT"; want_no_out "the bot says PR 7 is out of date"
}

test_watch_comments_api_error_is_retried_not_a_skip() {
    new_fixtures; good_pr "$head_a"
    echo '[]' >"$fx/api/pulls_7_reviews.1.json"
    touch "$fx/api/issues_7_comments.fail"
    run_watch
    want_rc 2; want_out "could not read the comments on PR 7"; want_no_out "the bot says PR 7 is out of date"
    [ "$(grep -c '^pr comment 7 .*--body /prrb' "$fx/calls.log")" = 1 ] || die "re-triggered other than once"
}

test_watch_exits_5_at_once_when_conflicting() {
    new_fixtures; good_pr "$head_a"
    echo '[]' >"$fx/api/pulls_7_reviews.1.json"
    printf '{"headRefOid":"%s","mergeStateStatus":"DIRTY"}\n' "$head_a" >"$fx/pr-view.json"
    run_watch
    want_rc 5; want_out "DIRTY against main"
    [ "$clock" = 1000 ] || die "waited until $clock instead of exiting at once"
    ! grep -q '^pr comment' "$fx/calls.log" || die "re-triggered a conflicting PR"
}

test_watch_exits_5_on_out_of_date_notice_after_retrigger() {
    new_fixtures; good_pr "$head_a"
    echo '[]' >"$fx/api/pulls_7_reviews.1.json"
    # The bot answers the re-trigger (posted at 1120) with a skip notice.
    printf '[{"user":{"login":"%s"},"created_at":"1970-01-01T00:20:00Z","body":"This PR is out of date with main"}]\n' \
        "$bot" >"$fx/api/issues_7_comments.1.json"
    run_watch
    want_rc 5; want_out "out of date with main"
    [ "$clock" -le 1180 ] || die "kept waiting until $clock after the notice"
}

test_watch_never_retriggers_while_behind_main() {
    new_fixtures; good_pr "$head_a"
    echo '[]' >"$fx/api/pulls_7_reviews.1.json"
    printf '{"headRefOid":"%s","mergeStateStatus":"BEHIND"}\n' "$head_a" >"$fx/pr-view.json"
    run_watch
    want_rc 5; want_out "BEHIND against main: merge origin/main"
    [ "$clock" = 1000 ] || die "waited until $clock instead of exiting at once"
    ! grep -q '^pr comment' "$fx/calls.log" || die "re-triggered while behind main"
}

test_watch_stalled_gh_still_ends_at_deadline() {
    new_fixtures; good_pr "$head_a"; block_gh
    # Real clock with a 4 s deadline and a 2 s cleanup reserve (1 + 1): a
    # stalled gh must not hold the watcher past the deadline, and the
    # diagnostics get only their own window, 3 s plus the reserve (#1169).
    started=$SECONDS
    out=$(cd "$repo" && AGENTIC_WATCH_TIMEOUT=4 agentic_kill_after=1 agentic_reap_after=1 \
        timeout 20 "$tools/watch-review.sh" 7 2>&1)
    rc=$?
    want_rc 2; want_out "TIMEOUT"
    # 4 + (3 + 2) = 9 s at most, plus a little for the shell.
    [ $((SECONDS - started)) -le 11 ] || die "took $((SECONDS - started)) s, want at most 9"
    want_no_blocked_left
}

# stubborn_gh: a gh that ignores TERM and stays up until it is KILLed, the
# worst call to stop. Its pid goes to $fx/stubborn.pid; the EXIT trap kills it
# in case a script under test leaves it behind.
stubborn_gh() {
    mkdir -p "$fx/bin"
    cat >"$fx/bin/gh" <<STUB
#!/usr/bin/env bash
trap '' TERM
echo \$\$ >>"$fx/stubborn.pid"
while :; do sleep 0.2; done
STUB
    chmod +x "$fx/bin/gh"
    trap '[ -s "$fx/stubborn.pid" ] && xargs kill -KILL <"$fx/stubborn.pid" 2>/dev/null' EXIT
}

test_call_budget_reserves_the_cleanup_time_before_the_deadline() {
    new_fixtures; echo 1000 >"$fx/clock"
    # budget <deadline> [VAR=value...]: agentic_call_budget on the fake clock.
    budget() {
        local deadline=$1
        shift
        env "$@" AGENTIC_NOW_CMD=fake-now bash -c \
            'source "$1"; agentic_deadline=$2; agentic_call_budget' _ "$tools/lib.sh" "$deadline"
    }
    [ "$(budget 1030 X=1)" = 20 ] || die "30 s left, 10 s reserve: want 20"
    [ "$(budget 1100 X=1)" = 60 ] || die "the per-call cap still applies"
    [ "$(budget 1100 AGENTIC_CALL_TIMEOUT=7)" = 7 ] || die "AGENTIC_CALL_TIMEOUT still caps"
    [ "$(budget 1010 X=1)" = 0 ] || die "left equal to the reserve: no call"
    [ "$(budget 1005 X=1)" = 0 ] || die "left below the reserve: no call"
    [ "$(budget 1030 agentic_kill_after=1 agentic_reap_after=2)" = 27 ] \
        || die "the reserve is kill_after + reap_after"
    [ "$(env AGENTIC_NOW_CMD=fake-now AGENTIC_CALL_TIMEOUT=9 bash -c \
        'source "$1"; agentic_call_budget' _ "$tools/lib.sh")" = 9 ] \
        || die "without a deadline there is no reserve"
}

test_a_stalled_call_is_stopped_by_the_deadline_not_after_it() {
    new_fixtures; stubborn_gh
    # A deadline 4 s away, reserve 1 + 1: the call runs 2 s, gets TERM (ignored),
    # is KILLed 1 s later and is gone by the deadline. Without the reserve it
    # would run 4 s and need 1 s more.
    local started_ms elapsed_ms pid
    started_ms=$(($(date +%s%N) / 1000000))
    out=$(PATH="$fx/bin:$PATH" agentic_kill_after=1 agentic_reap_after=1 timeout 20 bash -c '
        source "$1"
        agentic_deadline=$(($(agentic_now) + 4))
        agentic_gh pr view 7
        echo "rc=$?"' _ "$tools/lib.sh" 2>&1)
    elapsed_ms=$(($(date +%s%N) / 1000000 - started_ms))
    want_out "rc=124"
    [ "$elapsed_ms" -le 4500 ] || die "took $elapsed_ms ms; the deadline is 4000 ms (+500 ms for the shell)"
    [ "$elapsed_ms" -ge 1000 ] || die "took only $elapsed_ms ms: the call was never started"
    pid=$(head -n1 "$fx/stubborn.pid") || die "the stalled call never ran"
    ! kill -0 "$pid" 2>/dev/null || die "the stalled call $pid is still running"
}

test_watchdog_covers_worktrees_outside_main_checkout() {
    new_fixtures
    # Not local: the EXIT trap runs after the function returns. The fakes'
    # output goes to /dev/null so they don't hold the runner's pipe open.
    fake_pids=()
    trap 'kill "${fake_pids[@]}" 2>/dev/null' EXIT
    local inside outside_sibling outside_prefix
    # Fake test binaries: argv[0] names a test binary, the process is a sleep.
    (cd "$work/wt" && exec -a "$work/wt/target/debug/deps/fake_test" sleep 30) >/dev/null 2>&1 &
    inside=$!; fake_pids+=("$inside")
    (cd "$work/wt2" && exec -a "$work/wt2/target/debug/deps/fake_test" sleep 30) >/dev/null 2>&1 &
    outside_prefix=$!; fake_pids+=("$outside_prefix")
    (cd "$work/elsewhere" && exec -a "$work/elsewhere/target/debug/deps/fake_test" sleep 30) >/dev/null 2>&1 &
    outside_sibling=$!; fake_pids+=("$outside_sibling")
    sleep 1.2 # so every fake has run longer than the 0 s limit
    out=$(cd "$work/wt" && AGENTIC_WATCHDOG_TEST=0 AGENTIC_WATCHDOG_GATE=0 \
        timeout 15 "$tools/watchdog.sh" --once 2>&1)
    rc=$?
    want_rc 3; want_out "pid=$inside "
    grep -qE "pid=($outside_prefix|$outside_sibling) " <<<"$out" && die "reported a process outside"
    kill "$inside"; wait "$inside" 2>/dev/null
    out=$(cd "$work/wt" && AGENTIC_WATCHDOG_TEST=0 AGENTIC_WATCHDOG_GATE=0 \
        timeout 15 "$tools/watchdog.sh" --once 2>&1)
    rc=$?
    want_rc 0
}

# watchdog_with_ps <first etimes> <re-read etimes|gone> [uptime]: runs the
# watchdog once, with a stub `ps` that lists one test process in the checkout
# (a real sleep, so /proc/<pid>/cwd resolves) with the first etimes, then
# answers a re-read with the second (or fails when "gone"). Limits are 1200 s.
watchdog_with_ps() {
    new_fixtures
    fake_pids=()
    trap 'kill "${fake_pids[@]}" 2>/dev/null' EXIT
    (cd "$work/wt" && exec -a "$work/wt/target/debug/deps/fake_test" sleep 30) >/dev/null 2>&1 &
    ps_pid=$!; fake_pids+=("$ps_pid")
    sleep 0.3
    mkdir -p "$fx/bin"
    printf '%s\n' "${3:-693000.50} 1.00" >"$fx/uptime"
    cat >"$fx/bin/ps" <<STUB
#!/usr/bin/env bash
case "\$*" in
    *"-p "*) [ "$2" = gone ] && exit 1; echo "  $2" ;;
    *) echo "$ps_pid $1 $work/wt/target/debug/deps/fake_test" ;;
esac
STUB
    chmod +x "$fx/bin/ps"
    out=$(cd "$work/wt" && PATH="$fx/bin:$PATH" AGENTIC_UPTIME_FILE="$fx/uptime" \
        timeout 15 "$tools/watchdog.sh" --once 2>&1)
    rc=$?
}

# hanging_ps: a stub `ps` that never answers in time, first on PATH.
hanging_ps() {
    new_fixtures
    mkdir -p "$fx/bin"
    printf '%s\n' '#!/usr/bin/env bash' 'exec sleep 30' >"$fx/bin/ps"
    chmod +x "$fx/bin/ps"
}

# watch_until_warnings <pattern> <count> [env...]: runs the looping watchdog from
# $work/wt in the background, polls (up to 30 s) until its output holds <count>
# lines matching <pattern>, then stops it explicitly. Sets still_running (1 when
# it had to be stopped, so it was running until then) and out.
watch_until_warnings() {
    local pattern=$1 count=$2 tick; shift 2
    cd "$work/wt" || return 1
    env "$@" "$tools/watchdog.sh" >"$fx/watchdog.out" 2>&1 &
    wd_pid=$!
    trap 'kill "$wd_pid" ${fake_pids[@]+"${fake_pids[@]}"} 2>/dev/null' EXIT
    for ((tick = 0; tick < 300; tick++)); do
        [ "$(grep -c "$pattern" "$fx/watchdog.out")" -ge "$count" ] && break
        kill -0 "$wd_pid" 2>/dev/null || break
        sleep 0.1
    done
    still_running=0
    if kill -0 "$wd_pid" 2>/dev/null; then still_running=1; kill -TERM "$wd_pid"; fi
    # A frozen watchdog (one blocked in a $(...)) defers TERM: KILL it after 10 s.
    for ((tick = 0; tick < 100; tick++)); do
        kill -0 "$wd_pid" 2>/dev/null || break
        sleep 0.1
    done
    kill -KILL "$wd_pid" 2>/dev/null
    wait "$wd_pid" 2>/dev/null
    out=$(<"$fx/watchdog.out")
}

test_watchdog_skips_a_pass_when_the_ps_scan_hangs() {
    # #675: an unbounded ps would freeze the watchdog after at most one
    # warning. Bounded, every scan is skipped with a warning and the next
    # pass runs: wait for the second warning, then the watchdog must still be
    # running (it is stopped by the test, not by itself).
    hanging_ps
    watch_until_warnings "warning: ps call " 2 PATH="$fx/bin:$PATH" AGENTIC_CALL_TIMEOUT=1 \
        AGENTIC_WATCHDOG_INTERVAL=0
    [ "$still_running" = 1 ] || die "the watchdog exited by itself"
    [ "$(grep -c "warning: ps call " <<<"$out")" -ge 2 ] \
        || die "the watchdog did not run a second pass"
    want_no_out WATCHDOG
}

test_watchdog_once_exits_0_when_the_ps_scan_hangs() {
    hanging_ps
    out=$(cd "$work/wt" && PATH="$fx/bin:$PATH" AGENTIC_CALL_TIMEOUT=1 \
        timeout 10 "$tools/watchdog.sh" --once 2>&1)
    rc=$?
    want_rc 0; want_out "warning: ps call "; want_no_out WATCHDOG
}

# abandoning_stub <command> <glob> [body]: a stub <command> first on PATH
# ($fx/bin). When its arguments match <glob> it ignores TERM (only KILL stops
# it), leaves a detached grandchild (in its own session, out of reach of the
# watchdog's group kill) holding its stdout open, records that grandchild's pid
# in $fx/grandchildren and never answers. Otherwise it runs <body>, by default
# the real <command>. A process stuck in an uninterruptible kernel wait can't be
# made in a test; this one defeats everything else a supervisor can wait on: a
# TERM, the child and the output closing.
abandoning_stub() {
    local real body
    real=$(command -v "$1")
    if [ $# -ge 3 ]; then body=$3; else body="exec \"$real\" \"\$@\""; fi
    mkdir -p "$fx/bin" "$fx/tmp"
    cat >"$fx/bin/$1" <<STUB
#!/usr/bin/env bash
case "\$*" in
    $2)
        trap '' TERM
        setsid sleep 60 </dev/null >&1 2>/dev/null &
        echo \$! >>"$fx/grandchildren"
        while :; do sleep 1; done ;;
esac
$body
STUB
    chmod +x "$fx/bin/$1"
}

# quiet_ps: a stub `ps` that finds nothing, so a pass runs every later check.
quiet_ps() {
    mkdir -p "$fx/bin"
    printf '%s\n' '#!/usr/bin/env bash' 'exit 0' >"$fx/bin/ps"
    chmod +x "$fx/bin/ps"
}

# reap_grandchildren: KILLs every grandchild an abandoning stub started and sets
# gone=1 once none of their pids is left (checked by pid).
reap_grandchildren() {
    local gc tick
    gone=1
    [ -s "$fx/grandchildren" ] || return 0
    while read -r gc; do kill -KILL "$gc" 2>/dev/null; done <"$fx/grandchildren"
    for ((tick = 0; tick < 30; tick++)); do
        gone=1
        while read -r gc; do [ -e "/proc/$gc" ] && gone=0; done <"$fx/grandchildren"
        [ "$gone" = 1 ] && break
        sleep 0.1
    done
}

# watch_abandoning <command>: runs the looping watchdog with 1 s calls until it
# has warned twice about <command>, then checks that it never froze: it was
# still running, the hung <command> was started again by a later pass (two
# grandchildren), nothing alarmed, no temp file is left and every stub
# grandchild is gone. The caller sets up the stubs and fixtures first.
watch_abandoning() {
    watch_until_warnings "warning: $1 call " 2 PATH="$fx/bin:$PATH" TMPDIR="$fx/tmp" \
        AGENTIC_UPTIME_FILE="$fx/uptime" AGENTIC_CALL_TIMEOUT=1 agentic_kill_after=1 \
        AGENTIC_WATCHDOG_INTERVAL=0
    local started
    started=$(wc -l <"$fx/grandchildren" 2>/dev/null || echo 0)
    reap_grandchildren
    [ "$still_running" = 1 ] || die "the watchdog exited by itself"
    [ "$started" -ge 2 ] || die "no later pass started $1 again (the watchdog froze)"
    [ "$(grep -c "warning: $1 call " <<<"$out")" -ge 2 ] || die "fewer than two warnings about $1"
    want_no_out WATCHDOG
    [ -z "$(ls -A "$fx/tmp")" ] || die "a temp file was left behind"
    [ "$gone" = 1 ] || die "a stub grandchild is still running"
}

test_watchdog_abandons_a_ps_scan_it_cannot_stop() {
    # #675, Brooke's second review: a ps that ignores TERM (killed only by KILL)
    # and whose detached grandchild keeps the output open must neither stall the
    # watchdog nor leave the scan's temp file behind. Waiting for the child or
    # for the output to close would hang forever on a ps stuck in an
    # uninterruptible /proc read; the old pipe capture hangs here the same way
    # (the grandchild holds the pipe).
    new_fixtures; printf '%s\n' "693000.50 1.00" >"$fx/uptime"
    abandoning_stub ps '*'
    watch_abandoning ps
}

test_watchdog_abandons_a_ps_reread_it_cannot_stop() {
    # #675, Brooke's third review: the per-process `ps -p` re-read goes through
    # the same supervisor. Only the re-read hangs; the scans list one test
    # process far over its limit (a real sleep, so /proc/<pid>/cwd resolves).
    new_fixtures; fake_pids=()
    trap 'kill "${fake_pids[@]}" 2>/dev/null' EXIT
    (cd "$work/wt" && exec -a "$work/wt/target/debug/deps/fake_test" sleep 30) >/dev/null 2>&1 &
    fake_pids+=("$!")
    printf '%s\n' "693000.50 1.00" >"$fx/uptime"
    abandoning_stub ps '*"-p "*' \
        "case \"\$*\" in *pid=,etimes=,args=*) echo \"${fake_pids[0]} 5000 $work/wt/target/debug/deps/fake_test\" ;; esac"
    watch_abandoning ps
}

test_watchdog_abandons_a_cmdline_read_it_cannot_stop() {
    # Reading an idle `tail`'s command line (to tell `tail -f` apart) reads its
    # memory, which can block like ps; it is supervised too (`tr` reads it).
    new_fixtures; fake_pids=()
    trap 'kill "${fake_pids[@]}" 2>/dev/null' EXIT
    idle_reader tail
    printf '%s\n' "693000.50 1.00" >"$fx/uptime"
    mkdir -p "$fx/bin"
    cat >"$fx/bin/ps" <<STUB
#!/usr/bin/env bash
case "\$*" in
    *"-p "*) echo "  5000" ;;
    *pid=,etimes=,comm=*) echo "$reader_pid 5000 tail" ;;
esac
STUB
    chmod +x "$fx/bin/ps"
    abandoning_stub tr '*'
    watch_abandoning tr
}

test_watchdog_abandons_a_git_worktree_list_it_cannot_stop() {
    new_fixtures; printf '%s\n' "693000.50 1.00" >"$fx/uptime"
    quiet_ps
    abandoning_stub git '*"worktree list"*'
    watch_abandoning git
}

test_watchdog_abandons_a_git_reflog_it_cannot_stop() {
    # A pending expectation for an idle worktree makes the pass read its reflog.
    new_fixtures; expect_wt ex-reflog
    expect_start "$work/ex-reflog" "task 9"
    printf '%s\n' "693000.50 1.00" >"$fx/uptime"
    quiet_ps
    abandoning_stub git '*reflog*'
    watch_abandoning git
}

test_watchdog_abandons_a_find_it_cannot_stop() {
    # ...and the mtimes under its target/i*-logs.
    new_fixtures; expect_wt ex-find
    expect_start "$work/ex-find" "task 9"
    mkdir -p "$work/ex-find/target/i9-logs"
    printf '%s\n' "693000.50 1.00" >"$fx/uptime"
    quiet_ps
    abandoning_stub find '*'
    watch_abandoning find
}

# slow_mktemp_stub: a stub `mktemp` that records that it started in
# $fx/mktemp-started, sleeps 1 s and then runs the real mktemp, so a signal can
# be sent while it runs (#1193). Sets up $fx/tmp, the TMPDIR of the test.
slow_mktemp_stub() {
    local real
    real=$(command -v mktemp)
    mkdir -p "$fx/bin" "$fx/tmp"
    printf '%s\n' '#!/usr/bin/env bash' ": >\"$fx/mktemp-started\"" 'sleep 1' \
        "exec $real \"\$@\"" >"$fx/bin/mktemp"
    chmod +x "$fx/bin/mktemp"
}

# term_during_mktemp <pid>: waits (up to 10 s) for the stub mktemp to start, sends
# TERM to <pid> while it sleeps, and waits (up to 10 s) for <pid> to end. The
# stub's marker file is the signal: no fixed sleep decides when TERM is sent.
term_during_mktemp() {
    local tick
    for ((tick = 0; tick < 100; tick++)); do
        [ -e "$fx/mktemp-started" ] && break
        sleep 0.1
    done
    [ -e "$fx/mktemp-started" ] || die "the stub mktemp never started"
    kill -TERM "$1"
    for ((tick = 0; tick < 100; tick++)); do
        kill -0 "$1" 2>/dev/null || break
        sleep 0.1
    done
    kill -0 "$1" 2>/dev/null && { kill -KILL "$1"; die "TERM did not stop it"; }
    wait "$1" 2>/dev/null
    return 0
}

test_watchdog_leaves_no_temp_file_when_termed_during_mktemp() {
    # #1193: a TERM that arrives while a call's `mktemp` runs must not leave the
    # temp file behind (the file was created but not yet registered for cleanup).
    new_fixtures; printf '%s\n' "693000.50 1.00" >"$fx/uptime"
    slow_mktemp_stub
    (cd "$work/wt" && PATH="$fx/bin:$PATH" TMPDIR="$fx/tmp" AGENTIC_UPTIME_FILE="$fx/uptime" \
        exec "$tools/watchdog.sh" >"$fx/watchdog.out" 2>&1) &
    term_during_mktemp "$!"
    [ -z "$(ls -A "$fx/tmp")" ] || die "a temp file was left behind: $(ls -A "$fx/tmp")"
}

test_run_captured_leaves_no_temp_file_when_termed_during_mktemp() {
    # #1193: the same for agentic_run_captured, which makes two temp files.
    new_fixtures
    slow_mktemp_stub
    (PATH="$fx/bin:$PATH" TMPDIR="$fx/tmp" exec bash -c \
        'source "$1/lib.sh"; agentic_run_captured 5 true' _ "$tools") >/dev/null 2>&1 &
    term_during_mktemp "$!"
    [ -z "$(ls -A "$fx/tmp")" ] || die "a temp file was left behind: $(ls -A "$fx/tmp")"
}

test_run_captured_aborts_when_the_callers_term_handler_returns() {
    # #1193 (review): a caller whose TERM handler returns must get 143 back, with
    # the command never launched, no temp file left and its trap still installed.
    new_fixtures
    slow_mktemp_stub
    (PATH="$fx/bin:$PATH" TMPDIR="$fx/tmp" exec bash -c 'set -u; source "$1/lib.sh"
        trap "echo handled" TERM
        agentic_run_captured 5 touch "$2"; echo "rc=$?"; trap -p TERM' \
        _ "$tools" "$fx/launched" >"$fx/rc.out" 2>&1) &
    term_during_mktemp "$!"
    out=$(<"$fx/rc.out")
    [ -z "$(ls -A "$fx/tmp")" ] || die "a temp file was left behind"
    [ ! -e "$fx/launched" ] || die "the command was launched"
    want_out "handled"; want_out "rc=143"; want_out "trap -- 'echo handled' SIGTERM"
}

test_run_captured_aborts_when_a_signal_arrives_during_the_copy_out() {
    # #1193 (re-audit): a TERM while the captured output is being copied out
    # (a stub `cat` that waits for the test) is not lost: 143, handler ran, the
    # caller's trap kept.
    new_fixtures; mkdir -p "$fx/bin" "$fx/tmp"
    local real; real=$(command -v cat)
    printf '%s\n' '#!/usr/bin/env bash' ": >\"$fx/cat-started\"" \
        "i=0; while [ ! -e \"$fx/release\" ] && [ \$i -lt 100 ]; do sleep 0.1; i=\$((i + 1)); done" \
        "exec $real \"\$@\"" >"$fx/bin/cat"
    chmod +x "$fx/bin/cat"
    (PATH="$fx/bin:$PATH" TMPDIR="$fx/tmp" exec bash -c 'set -u; source "$1/lib.sh"
        trap "echo handled" TERM
        agentic_run_captured 5 true; echo "rc=$?"; trap -p TERM' _ "$tools" \
        >"$fx/rc.out" 2>&1) &
    local pid=$! tick
    for ((tick = 0; tick < 100; tick++)); do
        [ -e "$fx/cat-started" ] && break
        sleep 0.1
    done
    [ -e "$fx/cat-started" ] || { kill -KILL "$pid"; die "the copy-out never started"; }
    kill -TERM "$pid"
    : >"$fx/release"
    wait "$pid" 2>/dev/null
    out=$(<"$fx/rc.out")
    [ -z "$(ls -A "$fx/tmp")" ] || die "a temp file was left behind"
    want_out "handled"; want_out "rc=143"; want_out "trap -- 'echo handled' SIGTERM"
}

# term_after_stop_begins <script>: runs <script> (bash -c; $1=tools, $2 and $3
# files) in the background. Its command leaves a TERM-ignoring child behind, so
# the stop after the wait takes a while. Waits until the command's leader is gone,
# i.e. until that stop has begun, then sends TERM, and stops the leftover by its
# recorded pid. Sets out from the script's output.
term_after_stop_begins() {
    local pid tick leader
    (exec bash -c "$1" _ "$tools" "$fx/leftover" "$fx/leader.pid" "${2:-}" >"$fx/rc.out" 2>&1) &
    pid=$!
    for ((tick = 0; tick < 100; tick++)); do
        [ -s "$fx/leader.pid" ] && [ -s "$fx/leftover" ] && break
        sleep 0.1
    done
    [ -s "$fx/leftover" ] || { kill -KILL "$pid"; die "the command never started"; }
    leader=$(<"$fx/leader.pid")
    for ((tick = 0; tick < 100; tick++)); do
        [ -e "/proc/$leader" ] || break
        sleep 0.05
    done
    kill -TERM "$pid"
    wait "$pid" 2>/dev/null
    kill -KILL "$(<"$fx/leftover")" 2>/dev/null
    out=$(<"$fx/rc.out")
}

# The command: starts a child that ignores TERM and outlives it, records the
# leader's and the child's pids, and ends at once.
leftover_cmd='bash -c '"'"'echo $$ >"$2"; (trap "" TERM; echo $BASHPID >"$1"; exec sleep 30) & exit 0'"'"' _'

test_run_bounded_aborts_when_term_arrives_while_stopping_leftovers() {
    # #1193 (review round 2): the stop after the wait is a window too.
    new_fixtures
    term_after_stop_begins 'set -u; source "$1/lib.sh"; agentic_kill_after=2
        trap "echo handled" TERM
        agentic_run_bounded 30 '"$leftover_cmd"' "$2" "$3"; echo "rc=$?"; trap -p TERM'
    want_out "handled"; want_out "rc=143"; want_out "trap -- 'echo handled' SIGTERM"
}

test_bounded_capture_aborts_when_the_callers_term_handler_returns() {
    # #1193 (review): the watchdog's bounded_capture (its function is cut out of
    # watchdog.sh, which can't be sourced) returns 143 the same way.
    new_fixtures
    slow_mktemp_stub
    (PATH="$fx/bin:$PATH" TMPDIR="$fx/tmp" exec bash -c 'set -u; source "$1/lib.sh"
        source <(sed -n "/^bounded_capture\(_steps\)\{0,1\}() {/,/^}/p" "$1/watchdog.sh")
        trap "echo handled" TERM
        bounded_capture result touch "$2"; echo "rc=$?"; trap -p TERM' \
        _ "$tools" "$fx/launched" >"$fx/rc.out" 2>&1) &
    term_during_mktemp "$!"
    out=$(<"$fx/rc.out")
    [ -z "$(ls -A "$fx/tmp")" ] || die "a temp file was left behind"
    [ ! -e "$fx/launched" ] || die "the command was launched"
    want_out "handled"; want_out "rc=143"; want_out "trap -- 'echo handled' SIGTERM"
}

# block_stub <path> <marker> <command>: an executable <path> that creates
# $fx/<marker>, waits (up to 10 s) for $fx/release and then runs <command>, so
# a test can send a signal at a known step (#1193).
block_stub() {
    mkdir -p "$(dirname "$1")"
    printf '%s\n' '#!/usr/bin/env bash' ": >\"$fx/$2\"" \
        "i=0; while [ ! -e \"$fx/release\" ] && [ \$i -lt 100 ]; do sleep 0.1; i=\$((i + 1)); done" \
        "$3" >"$1"
    chmod +x "$1"
}

# term_at <ready> <call> [INT]: runs <call> in a caller shell (set -u, lib.sh
# and watchdog.sh's bounded_capture sourced, a TERM (or INT) handler that
# returns, $fx/bin first on PATH, TMPDIR=$fx/tmp; $2 is $fx in <call>). Once the
# test-side condition <ready> holds (up to 10 s), sends the signal, releases the
# stubs and waits (up to 10 s) for the shell. Sets out to its output and checks
# 143 (130), the handler ran, the trap is kept and no temp file is left.
term_at() {
    local pid tick sig=${3:-TERM} want=143
    [ "$sig" = TERM ] || want=130
    mkdir -p "$fx/bin" "$fx/tmp"
    # With job control on (set -m) the background shell does not start with INT
    # ignored, so its INT trap can be installed.
    [ "$sig" = TERM ] || set -m
    (PATH="$fx/bin:$PATH" TMPDIR="$fx/tmp" exec bash -c 'set -u; source "$1/lib.sh"
        source <(sed -n "/^bounded_capture\(_steps\)\{0,1\}() {/,/^}/p" "$1/watchdog.sh")
        trap "echo handled" '"$sig"'
        '"$2"'; echo "rc=$?"; trap -p '"$sig" _ "$tools" "$fx" >"$fx/rc.out" 2>&1) &
    pid=$!
    set +m
    for ((tick = 0; tick < 100; tick++)); do
        eval "$1" && break
        sleep 0.1
    done
    eval "$1" || { kill -KILL "$pid"; die "never reached the step: $1"; }
    kill "-$sig" "$pid"
    : >"$fx/release"
    for ((tick = 0; tick < 100; tick++)); do
        kill -0 "$pid" 2>/dev/null || break
        sleep 0.1
    done
    kill -0 "$pid" 2>/dev/null && { kill -KILL "$pid"; die "$sig did not stop it"; }
    wait "$pid" 2>/dev/null
    out=$(<"$fx/rc.out")
    [ -z "$(ls -A "$fx/tmp")" ] || die "a temp file was left behind: $(ls -A "$fx/tmp")"
    want_out "handled"; want_out "rc=$want"; want_out "trap -- 'echo handled' SIG$sig"
}

# term_during_budget <helper>: TERM while <helper> (bounded_capture result, or
# agentic_bounded_call, behind agentic_gh) works out its budget on a blocked
# clock. Nothing is launched.
term_during_budget() {
    new_fixtures
    block_stub "$fx/bin/clock" clock-started 'echo 1000'
    term_at '[ -e "$fx/clock-started" ]' 'export AGENTIC_NOW_CMD="$2/bin/clock"
        agentic_deadline=2000; '"$1"' touch "$2/launched"'
    [ ! -e "$fx/launched" ] || die "the command was launched"
}

test_bounded_capture_aborts_when_termed_during_the_budget() {
    # #1193 (review round 3): a TERM during $(agentic_call_budget), after the
    # capture's traps are in, used to recreate the removed temp file and launch.
    term_during_budget "bounded_capture result"
}

test_a_term_at_each_capture_step_launches_and_leaks_nothing() {
    # #1193: the invariant across the steps the other tests don't cover. A TERM
    # with a returning caller handler gives 143, keeps the trap, leaves no temp
    # file; before the launch nothing starts, after it the group is stopped.
    local helper pid
    term_during_budget agentic_bounded_call
    for helper in "bounded_capture result" "agentic_run_captured 30"; do
        # While the command runs.
        new_fixtures
        term_at '[ -s "$fx/cmd.pid" ]' "$helper"' bash -c "echo \$\$ >\"\$1/cmd.pid\"
            exec sleep 30" _ "$2"'
        pid=$(<"$fx/cmd.pid")
        [ ! -e "/proc/$pid" ] || { kill -KILL "$pid"; die "$helper left the command running"; }
        # While its leftover is being stopped: the leader is gone, a TERM-ignoring
        # child is left.
        new_fixtures
        term_at '[ -s "$fx/leftover" ] && [ -s "$fx/cmd.pid" ] && [ ! -e "/proc/$(<"$fx/cmd.pid")" ]' \
            'agentic_kill_after=1; '"$helper"' bash -c "echo \$\$ >\"\$1/cmd.pid\"
            (trap \"\" TERM; echo \$BASHPID >\"\$1/leftover\"; exec sleep 30) & exit 0" _ "$2"'
        pid=$(<"$fx/leftover")
        [ ! -e "/proc/$pid" ] || { kill -KILL "$pid"; die "$helper left the leftover running"; }
    done
}

test_a_term_before_the_group_exists_stops_the_command() {
    # #1193 (audit): a TERM that lands just after the launch, before the leader
    # has called setsid() (a stub setsid that waits), used to find no group to
    # stop and leave the command running. Each helper stops it once it starts.
    local helper pid real
    real=$(command -v setsid)
    for helper in "bounded_capture result" "agentic_run_captured 30" "agentic_run_bounded 30"; do
        new_fixtures
        block_stub "$fx/bin/setsid" setsid-started "exec $real \"\$@\""
        term_at '[ -e "$fx/setsid-started" ]' "$helper"' bash -c "echo \$\$ >\"\$1/cmd.pid\"
            exec sleep 30" _ "$2"'
        [ -s "$fx/cmd.pid" ] || continue
        pid=$(<"$fx/cmd.pid")
        [ ! -e "/proc/$pid" ] || { kill -KILL "$pid"; die "$helper left the command running"; }
    done
}

test_the_helpers_clean_up_under_a_callers_set_e() {
    # #1193 (audit): under `set -e` a failing command must still go through the
    # one exit: temp files removed, the caller's trap back, then the shell ends.
    local call
    for call in "agentic_run_captured 5" "agentic_bounded_call" "bounded_capture result" \
        "agentic_run_bounded 5"; do
        new_fixtures; mkdir -p "$fx/tmp"
        rc=0
        out=$(TMPDIR="$fx/tmp" bash -c 'set -euo pipefail; source "$1/lib.sh"
            source <(sed -n "/^bounded_capture\(_steps\)\{0,1\}() {/,/^}/p" "$1/watchdog.sh")
            trap "echo mine" TERM; trap "trap -p TERM" EXIT
            '"$call"' bash -c "exit 3"; echo not-reached' _ "$tools" 2>&1) || rc=$?
        want_rc 3; want_no_out "not-reached"; want_out "trap -- 'echo mine' SIGTERM"
        [ -z "$(ls -A "$fx/tmp")" ] || die "$call left a temp file: $(ls -A "$fx/tmp")"
    done
}

test_an_int_with_a_returning_handler_gives_130() {
    # #1193: INT takes the same path as TERM: before the launch (the budget) and
    # while the command runs, for a capture and for agentic_run_bounded.
    local helper pid
    new_fixtures
    block_stub "$fx/bin/clock" clock-started 'echo 1000'
    term_at '[ -e "$fx/clock-started" ]' 'export AGENTIC_NOW_CMD="$2/bin/clock"
        agentic_deadline=2000; agentic_bounded_call touch "$2/launched"' INT
    [ ! -e "$fx/launched" ] || die "the command was launched"
    for helper in "agentic_run_captured 30" "agentic_run_bounded 30"; do
        new_fixtures
        term_at '[ -s "$fx/cmd.pid" ]' "$helper"' bash -c "echo \$\$ >\"\$1/cmd.pid\"
            exec sleep 30" _ "$2"' INT
        pid=$(<"$fx/cmd.pid")
        [ ! -e "/proc/$pid" ] || { kill -KILL "$pid"; die "$helper left the command running"; }
    done
}

# sweep <region> <points> <call>: the signal sweep of lib.sh's "Signals"
# invariant (#1193) over one helper, through tests/signal-sweep.sh: a first
# run of <call> lists the statements of the functions in <points> that run,
# then one run per statement sends TERM right before it. Each run must give
# 143 with the caller's handler run and kept, leave no temp file, and run no
# wrapped command (an external one, or echo; as a statement or from one) after
# the statement the signal landed before, except the undoing
# (agentic_stop_group, agentic_cap_finish).
# A statement a run does not reach (the timing changed the path) is skipped.
# What ran after a signal is added to sweep_bad, so one run lists every gap.
sweep() {
    local point log hit=0 bad
    mkdir -p "$fx/bin" "$fx/tmp"
    sweep_run() {
        PATH="$fx/bin:$PATH" TMPDIR="$fx/tmp" AGENTIC_TICKS_PER_SEC=10 \
            bash "$tools/tests/signal-sweep.sh" \
            "$tools" "$1" "$region" "$points" "$call" "${2:-}" 2>&1
    }
    local region=$1 points=$2 call=$3
    out=$(sweep_run "$fx/points")
    [ -s "$fx/points" ] || die "the sweep of $region listed no statements"
    for point in $(awk '!seen[$0]++' "$fx/points"); do
        log="$fx/sweep.log"
        : >"$log"
        out=$(sweep_run "$log" "$point")
        grep -q '^INJECT' "$log" || continue
        hit=$((hit + 1))
        want_out "handled"; want_out "rc=143"; want_out "trap -- 'printf \"handled\\n\"' SIGTERM"
        [ -z "$(ls -A "$fx/tmp")" ] || die "TERM before $point left $(ls -A "$fx/tmp")"
        bad=$(awk '/^INJECT/ { n = $2; next }
            n != "" && $1 > n && $2 ~ /^(cat|sed|tail|mktemp|rm|date|setsid|sleep|echo)$/ \
                && $3 != "agentic_stop_group" && $3 != "agentic_cap_finish"' "$log")
        [ -z "$bad" ] || sweep_bad+="TERM before $point ($(grep '^INJECT' "$log")) let these run:
$bad
"
    done
    [ "$hit" -gt 0 ] || die "the sweep of $region reached no statement"
}

test_a_term_before_any_statement_lets_only_that_statement_run() {
    # #1193 (review round 4): the invariant statement by statement, for each
    # helper on each path that writes: a leftover and a failure, a timeout, the
    # budget's clock, and remove-worktree.sh's clean, failed and not.
    local left='bash -c "(trap \"\" TERM; exec sleep 5) & exit 3"'
    local bounded="agentic_run_bounded agentic_run_bounded_steps agentic_bounded_warn" clean
    local capture="agentic_cap_live agentic_cap_mktemp agentic_cap_say agentic_capture_supervise"
    sweep_bad=""
    new_fixtures
    sweep agentic_run_captured_steps "agentic_run_captured_steps $capture" \
        "agentic_run_captured 5 $left"
    new_fixtures
    sweep agentic_run_captured_steps agentic_run_captured_steps "agentic_run_captured 1 sleep 5"
    new_fixtures
    sweep bounded_capture_steps bounded_capture_steps "bounded_capture result $left"
    new_fixtures
    sweep bounded_capture_steps bounded_capture_steps \
        "AGENTIC_CALL_TIMEOUT=1 bounded_capture result sleep 5"
    new_fixtures
    sweep agentic_bounded_call_steps agentic_bounded_call_steps \
        "agentic_deadline=\$((\$(date +%s) + 100)); agentic_bounded_call $left"
    new_fixtures
    sweep agentic_run_bounded_steps "$bounded" "agentic_run_bounded 30 $left"
    for clean in 'echo "     Removed 3 files"' 'echo broken; exit 2'; do
        new_fixtures
        mkdir -p "$fx/bin"
        printf '%s\n' '#!/usr/bin/env bash' "$clean" >"$fx/bin/cargo"
        chmod +x "$fx/bin/cargo"
        sweep clean_build_output_steps "clean_build_output_steps $bounded" \
            "wt=$fx; clean_build_output"
    done
    out=""
    [ -z "$sweep_bad" ] || die "$sweep_bad"
}

test_run_bounded_aborts_when_the_callers_term_handler_returns() {
    # #1193 (review): the same for agentic_run_bounded, which must stop the group,
    # return 143 and keep the caller's trap.
    new_fixtures
    (PATH="$fx/bin:$PATH" exec bash -c 'set -u; source "$1/lib.sh"
        trap "echo handled" TERM
        agentic_run_bounded 30 bash -c ": >\"\$1\"; exec sleep 30" _ "$2"
        echo "rc=$?"; trap -p TERM' _ "$tools" "$fx/started" >"$fx/rc.out" 2>&1) &
    local rb_pid=$! tick
    for ((tick = 0; tick < 100; tick++)); do
        [ -e "$fx/started" ] && break
        sleep 0.1
    done
    [ -e "$fx/started" ] || { kill -KILL "$rb_pid"; die "the command never started"; }
    kill -TERM "$rb_pid"
    wait "$rb_pid" 2>/dev/null
    out=$(<"$fx/rc.out")
    want_out "handled"; want_out "rc=143"; want_out "trap -- 'echo handled' SIGTERM"
}

test_run_captured_puts_the_callers_traps_back() {
    # #1193: agentic_run_captured used to clear the caller's INT and TERM traps.
    new_fixtures; mkdir -p "$fx/tmp"
    local trapped
    trapped=$(TMPDIR="$fx/tmp" bash -c \
        'source "$1/lib.sh"; trap "echo mine" TERM; agentic_run_captured 5 true; trap -p TERM' \
        _ "$tools")
    [ "$trapped" = "trap -- 'echo mine' SIGTERM" ] || die "the caller's TERM trap was lost: $trapped"
}

# group_stub <leader body>: a stub `ps` whose call first starts a member of its
# own process group (no setsid) that ignores TERM and holds the call's output
# open, recording its pid in $fx/members, and then runs <leader body>.
group_stub() {
    mkdir -p "$fx/bin" "$fx/tmp"
    cat >"$fx/bin/ps" <<STUB
#!/usr/bin/env bash
(trap '' TERM; exec sleep 60) </dev/null >&1 2>/dev/null &
echo \$! >>"$fx/members"
$1
STUB
    chmod +x "$fx/bin/ps"
}

# want_members_gone: every recorded group member is gone once the watchdog has
# returned (checked by pid, without waiting); the test's trap stops any left.
want_members_gone() {
    local member
    [ -s "$fx/members" ] || die "the stub never started a group member"
    while read -r member; do
        [ ! -e "/proc/$member" ] || die "group member $member outlived the call"
    done <"$fx/members"
}

test_watchdog_kills_a_group_member_that_ignores_term_after_a_timeout() {
    # Brooke's fourth review: the leader dies on TERM, a member of its group
    # ignores TERM. Escalating by the leader's liveness alone never sent KILL.
    new_fixtures
    trap '[ -s "$fx/members" ] && xargs kill -KILL <"$fx/members" 2>/dev/null' EXIT
    group_stub 'while :; do sleep 1; done'
    out=$(cd "$work/wt" && PATH="$fx/bin:$PATH" TMPDIR="$fx/tmp" AGENTIC_CALL_TIMEOUT=1 \
        agentic_kill_after=1 timeout 30 "$tools/watchdog.sh" --once 2>&1)
    rc=$?
    want_rc 0; want_out "warning: ps call timed out"; want_no_out WATCHDOG
    want_members_gone
    [ -z "$(ls -A "$fx/tmp")" ] || die "a temp file was left behind"
}

test_watchdog_stops_what_a_call_leaves_behind_on_a_normal_exit() {
    # A leader that exits at once, leaving a TERM-ignoring member of its group
    # holding the (unlinked) output file: it must not pile up across passes.
    new_fixtures
    trap '[ -s "$fx/members" ] && xargs kill -KILL <"$fx/members" 2>/dev/null' EXIT
    group_stub 'exit 0'
    out=$(cd "$work/wt" && PATH="$fx/bin:$PATH" TMPDIR="$fx/tmp" AGENTIC_CALL_TIMEOUT=5 \
        agentic_kill_after=1 timeout 60 "$tools/watchdog.sh" --once 2>&1)
    rc=$?
    want_rc 0; want_no_out WATCHDOG
    [ "$(wc -l <"$fx/members")" -ge 3 ] || die "the three scans did not all run"
    want_members_gone
    want_out "warning: ps call exited leaving processes behind"
    [ -z "$(ls -A "$fx/tmp")" ] || die "a temp file was left behind"
}

test_watchdog_exits_1_when_its_start_up_git_call_hangs() {
    new_fixtures
    abandoning_stub git '*rev-parse*'
    out=$(cd "$work/wt" && PATH="$fx/bin:$PATH" TMPDIR="$fx/tmp" AGENTIC_CALL_TIMEOUT=1 \
        agentic_kill_after=1 timeout 15 "$tools/watchdog.sh" --once 2>&1)
    rc=$?
    reap_grandchildren
    want_rc 1; want_out "warning: git call timed out"; want_out "could not find"
    [ -z "$(ls -A "$fx/tmp")" ] || die "a temp file was left behind"
    [ "$gone" = 1 ] || die "a stub grandchild is still running"
}

# third_scan_fails_ps <worktree name>: a stub `ps` that lists one test process in the checkout
# far over its limit (a real sleep, so /proc/<pid>/cwd resolves), an empty idle
# reader list, and hangs only on the quiet-agent scan (pid=,comm=,args=). A
# pending expectation makes the watchdog reach that scan.
third_scan_fails_ps() {
    new_fixtures; expect_wt "$1"
    expect_start "$work/$1" "task 9"
    fake_pids=()
    trap 'kill "${fake_pids[@]}" 2>/dev/null' EXIT
    (cd "$work/wt" && exec -a "$work/wt/target/debug/deps/fake_test" sleep 30) >/dev/null 2>&1 &
    ps_pid=$!; fake_pids+=("$ps_pid")
    sleep 0.3
    mkdir -p "$fx/bin"
    printf '%s\n' "693000.50 1.00" >"$fx/uptime"
    cat >"$fx/bin/ps" <<STUB
#!/usr/bin/env bash
case "\$*" in
    *"-p "*) echo "  5001" ;;
    *pid=,comm=,args=*) exec sleep 30 ;;
    *pid=,etimes=,comm=*) ;;
    *) echo "$ps_pid 5000 $work/wt/target/debug/deps/fake_test" ;;
esac
STUB
    chmod +x "$fx/bin/ps"
}

test_watchdog_skips_the_pass_when_only_the_third_scan_fails() {
    # An alert found by the first scan must not survive a failed later scan.
    third_scan_fails_ps ex-third-once
    out=$(cd "$work/wt" && PATH="$fx/bin:$PATH" AGENTIC_UPTIME_FILE="$fx/uptime" \
        AGENTIC_CALL_TIMEOUT=1 timeout 15 "$tools/watchdog.sh" --once 2>&1)
    rc=$?
    want_rc 0; want_out "warning: ps call "; want_no_out WATCHDOG
}

test_watchdog_loop_skips_the_pass_when_only_the_third_scan_fails() {
    third_scan_fails_ps ex-third-loop
    watch_until_warnings "warning: ps call " 2 PATH="$fx/bin:$PATH" \
        AGENTIC_UPTIME_FILE="$fx/uptime" AGENTIC_CALL_TIMEOUT=1 AGENTIC_WATCHDOG_INTERVAL=0
    [ "$still_running" = 1 ] || die "the watchdog exited by itself"
    want_no_out WATCHDOG
}

test_watchdog_ignores_a_wrapped_etimes_reading() {
    # The 2026-11-18 sighting: 2^32 - 4123168608 = 171798688.
    watchdog_with_ps 4123168608 5001
    want_rc 0
    ! grep -q WATCHDOG <<<"$out" || die "alarmed on a reading above the uptime"
}

test_watchdog_ignores_a_process_gone_on_reread() {
    watchdog_with_ps 5000 gone
    want_rc 0
    ! grep -q WATCHDOG <<<"$out" || die "alarmed on a process that exited"
}

test_watchdog_ignores_an_implausible_reread() {
    watchdog_with_ps 5000 4123168608
    want_rc 0
}

test_watchdog_alarms_on_a_genuinely_long_process() {
    watchdog_with_ps 5000 5001
    want_rc 3; want_out "pid=$ps_pid elapsed=5000s comm=sleep cwd=$work/wt"
    # Alerts carry no command line: argv[0] here is a path to the fake binary.
    ! grep -q "fake_test" <<<"$out" || die "the alert includes command-line text"
}

test_watchdog_sees_a_test_binary_in_the_build_dir_layout() {
    # Issue #738: cargo 1.100 runs test binaries from
    # <build-dir>/debug/build/<package>/<hash>/out/, with no /debug/deps/ in
    # the path, and the watchdog missed a lib test binary hung for 50 min.
    # The gate limit is out of reach, so only the test-binary limit can fire.
    new_fixtures
    fake_pids=()
    trap 'kill "${fake_pids[@]}" 2>/dev/null' EXIT
    local binary=$work/build-dir/debug/build/okf_mcp/0e3f1e14f080a04c/out/okf_mcp-0e3f1e14f080a04c
    (cd "$work/wt" && exec -a "$binary" sleep 30) >/dev/null 2>&1 &
    local hung=$!
    fake_pids+=("$hung")
    sleep 1.2 # so the fake has run longer than the 0 s limit
    out=$(cd "$work/wt" && AGENTIC_WATCHDOG_TEST=0 AGENTIC_WATCHDOG_GATE=999999 \
        timeout 15 "$tools/watchdog.sh" --once 2>&1)
    rc=$?
    want_rc 3; want_out "pid=$hung "
    ! grep -q "okf_mcp-0e3f1e14f080a04c" <<<"$out" || die "the alert includes command-line text"
    kill "$hung"; wait "$hung" 2>/dev/null || true
}

# --- idle stdin readers (issue #780) ---

# idle_reader <comm> [args...]: starts the real <comm> in $work/wt with a pipe
# on stdin that nothing ever writes to or closes (a 20 s sleep holds it open,
# so the test is bounded), and sets reader_pid. Output goes to /dev/null so the
# fake doesn't hold the runner's pipe open.
idle_reader() {
    (cd "$work/wt" && exec "$@" < <(sleep 20)) >/dev/null 2>&1 &
    reader_pid=$!; fake_pids+=("$reader_pid")
}

# idle_watchdog [env...]: one watchdog pass from $work/wt with the test and gate
# limits out of reach, so only the idle check can fire.
idle_watchdog() {
    out=$(cd "$work/wt" && env AGENTIC_WATCHDOG_TEST=999999 AGENTIC_WATCHDOG_GATE=999999 "$@" \
        timeout 15 "$tools/watchdog.sh" --once 2>&1)
    rc=$?
}

test_watchdog_flags_an_idle_cat_on_a_pipe_without_argument_text() {
    new_fixtures; fake_pids=(); trap 'kill "${fake_pids[@]}" 2>/dev/null' EXIT
    # tee reads stdin like cat and takes a distinctive argument to look for.
    idle_reader tee "$work/wt/distinct-secret-arg.out"
    local tee_pid=$reader_pid
    idle_reader cat
    sleep 1.2 # so both have run longer than the 0 s limit
    idle_watchdog AGENTIC_WATCHDOG_IDLE=0
    want_rc 3; want_out "pid=$reader_pid "; want_out "comm=cat cwd=$work/wt"
    want_out "pid=$tee_pid "; want_out "comm=tee "
    ! grep -q "distinct-secret-arg" <<<"$out" || die "the alert includes argument text"
    ! grep -q "cmd=" <<<"$out" || die "the alert has a cmd field"
}

test_watchdog_ignores_a_sleep_and_a_cat_on_a_file() {
    new_fixtures; fake_pids=()
    # A stopped process ignores SIGTERM until it is continued, so the cleanup
    # continues every fake first, whether the test passes or dies.
    trap 'kill -CONT "${fake_pids[@]}" 2>/dev/null; kill "${fake_pids[@]}" 2>/dev/null' EXIT
    (cd "$work/wt" && exec sleep 20) >/dev/null 2>&1 &
    fake_pids+=("$!")
    # A cat whose stdin is a regular file: fd 0 is not a pipe or socket. It must
    # still be alive during the scan, or the test proves nothing (a cat on an
    # empty file exits at once). A 1 MiB file and a stdout pipe nobody reads
    # keep it blocked in a write, and a SIGSTOP holds it in state T.
    head -c 1048576 /dev/zero >"$work/regular-input"
    (cd "$work/wt" && exec cat < "$work/regular-input" > >(sleep 20)) >/dev/null 2>&1 &
    local file_cat=$!
    fake_pids+=("$file_cat")
    # Stop it only once it has exec'd cat: until then it is the subshell.
    local tries=0 state=
    while [ "$(cat "/proc/$file_cat/comm" 2>/dev/null)" != cat ] && [ "$tries" -lt 50 ]; do
        tries=$((tries + 1)); sleep 0.1
    done
    kill -STOP "$file_cat"
    tries=0
    while [ "$tries" -lt 50 ]; do
        state=$(awk '/^State:/ {print $2}' "/proc/$file_cat/status" 2>/dev/null)
        [ "$state" = T ] && break
        tries=$((tries + 1)); sleep 0.1
    done
    [ "$state" = T ] || die "the cat on a file is not alive and stopped (state: ${state:-gone})"
    [ "$(readlink "/proc/$file_cat/fd/0")" = "$work/regular-input" ] || die "fd 0 is not the regular file"
    # tail -f is a follower, not a stuck reader.
    idle_reader tail -f /dev/stdin
    sleep 1.2
    idle_watchdog AGENTIC_WATCHDOG_IDLE=0
    want_rc 0
    kill -0 "$file_cat" 2>/dev/null || die "the cat on a file exited before the scan"
    kill -CONT "$file_cat"; kill "$file_cat"; wait "$file_cat" 2>/dev/null || true
    [ ! -d "/proc/$file_cat" ] || die "the cat on a file outlived the test"
}

test_watchdog_ignores_an_idle_reader_outside_a_checkout() {
    new_fixtures; fake_pids=(); trap 'kill "${fake_pids[@]}" 2>/dev/null' EXIT
    # Two identical readers on pipes, one inside a checkout and one outside
    # every checkout: only the directory differs, so only the checkout filter
    # can tell them apart.
    idle_reader cat
    local inside=$reader_pid outside
    (cd "$work/elsewhere" && exec cat < <(sleep 20)) >/dev/null 2>&1 &
    outside=$!; fake_pids+=("$outside")
    sleep 1.2
    [ "$(readlink "/proc/$outside/fd/0" | cut -d: -f1)" = pipe ] || die "the outside reader is not on a pipe"
    idle_watchdog AGENTIC_WATCHDOG_IDLE=0
    want_rc 3; want_out "pid=$inside "
    ! grep -q "pid=$outside " <<<"$out" || die "reported a reader outside every checkout"
}

test_watchdog_ignores_an_idle_reader_under_the_limit() {
    new_fixtures; fake_pids=(); trap 'kill "${fake_pids[@]}" 2>/dev/null' EXIT
    idle_reader cat
    sleep 1.2 # older than 0 s, far under the 3600 s limit
    idle_watchdog AGENTIC_WATCHDOG_IDLE=3600
    want_rc 0
}

test_watchdog_ignores_a_reader_that_uses_cpu() {
    new_fixtures; fake_pids=(); trap 'kill "${fake_pids[@]}" 2>/dev/null' EXIT
    # cat copying from a pipe that never stops producing is busy, not stuck.
    (cd "$work/wt" && exec cat < <(timeout 4 yes)) >/dev/null 2>&1 &
    fake_pids+=("$!")
    sleep 3
    # A zero-tick CPU allowance, so any CPU at all counts as busy.
    idle_watchdog AGENTIC_WATCHDOG_IDLE=0 AGENTIC_WATCHDOG_IDLE_CPU=0
    want_rc 0
    sleep 2 # let the 4 s producer end so no cat outlives the test
}

# idle_watchdog_with_ps <etimes>: a stub `ps` lists one real idle cat in the
# checkout with the given etimes (and answers the re-read with it), and the
# idle limit is left unset, so the default applies. The caller sets up the
# fixtures and the cleanup trap first, so the helper can run twice in one test.
idle_watchdog_with_ps() {
    idle_reader cat
    sleep 0.3
    mkdir -p "$fx/bin"
    printf '%s\n' "693000.50 1.00" >"$fx/uptime"
    cat >"$fx/bin/ps" <<STUB
#!/usr/bin/env bash
case "\$*" in
    *"-p "*) echo "  $1" ;;
    *args=*) ;;
    *) echo "$reader_pid $1 cat" ;;
esac
STUB
    chmod +x "$fx/bin/ps"
    idle_watchdog PATH="$fx/bin:$PATH" AGENTIC_UPTIME_FILE="$fx/uptime"
}

test_watchdog_default_idle_limit_is_600_seconds() {
    new_fixtures; fake_pids=(); trap 'kill "${fake_pids[@]}" 2>/dev/null' EXIT
    unset AGENTIC_WATCHDOG_IDLE
    idle_watchdog_with_ps 601
    want_rc 3; want_out "pid=$reader_pid elapsed=601s comm=cat"
    idle_watchdog_with_ps 599
    want_rc 0
}

test_watchdog_ignores_an_idle_reader_with_a_wrapped_etimes() {
    new_fixtures; fake_pids=(); trap 'kill "${fake_pids[@]}" 2>/dev/null' EXIT
    idle_watchdog_with_ps 4123168608
    want_rc 0
}

# --- gate.sh ---

test_gate_timeout_returns_124() {
    new_fixtures; fake_just 'exec sleep 30'
    out=$(cd "$repo" && PATH="$fx/bin:$PATH" AGENTIC_TIMEOUT_FMT=1s \
        "$tools/gate.sh" fmt 2>&1)
    rc=$?
    want_rc 124; want_out "Treat this as a HANG"
}

# fake_just <body>: puts a `just` running <body> first on PATH for this test.
fake_just() {
    mkdir -p "$fx/bin"
    printf '#!/usr/bin/env bash\n%s\n' "$1" >"$fx/bin/just"
    chmod +x "$fx/bin/just"
}

test_gate_returns_the_gate_exit_status() {
    new_fixtures; fake_just 'exit 3'
    out=$(cd "$repo" && PATH="$fx/bin:$PATH" "$tools/gate.sh" lint 2>&1)
    rc=$?
    want_rc 3; want_no_out "HANG"
}

# fake_gate_wait_for <marker> [ticks]: shell text for a fake gate that waits
# (at most <ticks> x 0.1 s, default 100) for the marker file a helper writes
# once it is set up, so the test synchronizes on observed state, not on a delay.
# <marker> is shell text the fake gate expands itself (pass "$GH_FIXTURES/x"
# with the dollar sign kept, no backslash). A marker that never appears fails
# the fake gate with exit 97 and a message, so the test fails; it never goes on.
fake_gate_wait_for() {
    # shellcheck disable=SC2016 # the text is for the fake gate's shell to expand
    printf 'for _ in $(seq %s); do [ -e "%s" ] && break; sleep 0.1; done\n[ -e "%s" ] || { echo "marker %s never appeared" >&2; exit 97; }\n' "${2:-100}" "$1" "$1" "$1"
}

# leaky_gate <then>: a fake gate that starts a child which ignores TERM and
# runs in the gate's process group, then runs <then> (wait, exit 0, kill itself...).
# The child's output goes to /dev/null so it can't hold the test's pipe open.
leaky_gate() {
    # The child writes child.ready after it installs its trap; the leader waits
    # for it, so no TERM can arrive before the trap exists.
    # shellcheck disable=SC2016 # the marker path is expanded by the fake just
    fake_just "bash -c 'trap \"\" TERM; : >\"\$GH_FIXTURES/child.ready\"; exec sleep 30' >/dev/null 2>&1 &
echo \$! >\"\$GH_FIXTURES/child.pid\"
$(fake_gate_wait_for '$GH_FIXTURES/child.ready')
$1"
    trap '[ -s "$fx/child.pid" ] && kill -KILL "$(cat "$fx/child.pid")" 2>/dev/null' EXIT
}

# run_leaky_gate: runs the lint gate with a 1 s limit and 1 s grace.
run_leaky_gate() {
    started=$SECONDS
    (cd "$repo" && PATH="$fx/bin:$PATH" AGENTIC_TIMEOUT_LINT=1s AGENTIC_GATE_KILL_AFTER=1 \
        "$tools/gate.sh" lint) >"$fx/out" 2>&1
    rc=$?
    out=$(cat "$fx/out")
}

# want_nothing_left: gate.sh returned quickly, its TERM-ignoring child is gone
# and the lock is free.
want_nothing_left() {
    [ $((SECONDS - started)) -le 6 ] || die "took $((SECONDS - started)) s"
    [ -s "$fx/child.pid" ] || die "the fake gate didn't start its child"
    ! kill -0 "$(cat "$fx/child.pid")" 2>/dev/null || die "the TERM-ignoring child outlived the gate"
    flock -n "$repo/.git/agentic/gates.lock" true || die "gates.lock is still held"
}

test_gate_timeout_stops_term_ignoring_child_and_frees_lock() {
    new_fixtures; leaky_gate 'wait'
    run_leaky_gate
    want_rc 124; want_out "Treat this as a HANG"; want_nothing_left
}

test_gate_killed_leader_stops_child_and_frees_lock() {
    new_fixtures; leaky_gate 'kill -KILL $$'
    run_leaky_gate
    want_rc 124; want_out "Treat this as a HANG"; want_nothing_left
}

test_gate_passing_leader_stops_leftover_child_and_frees_lock() {
    new_fixtures; leaky_gate 'exit 0'
    run_leaky_gate
    want_rc 0; want_out "leaving processes behind; stopped them"; want_no_out "HANG"; want_nothing_left
}

test_fake_gate_wait_fails_loudly_when_its_marker_never_appears() {
    new_fixtures
    # shellcheck disable=SC2016 # the marker path is expanded by the fake just
    fake_just "$(fake_gate_wait_for '$GH_FIXTURES/never.ready' 5)
exit 0"
    started=$SECONDS
    out=$(cd "$repo" && PATH="$fx/bin:$PATH" timeout 30 "$tools/gate.sh" lint 2>&1)
    rc=$?
    want_rc 97; want_out "never.ready never appeared"
    [ $((SECONDS - started)) -le 5 ] || die "took $((SECONDS - started)) s"
}

test_fake_gate_wait_returns_as_soon_as_the_marker_exists() {
    new_fixtures
    # shellcheck disable=SC2016 # the marker path is expanded by the fake just
    fake_just ': >"$GH_FIXTURES/up.ready"
'"$(fake_gate_wait_for '$GH_FIXTURES/up.ready' 5)"'
exit 0'
    out=$(cd "$repo" && PATH="$fx/bin:$PATH" timeout 30 "$tools/gate.sh" lint 2>&1)
    rc=$?
    want_rc 0; want_no_out "never appeared"
}

# A daemon the gate starts (sccache's server, say) leaves the gate's process
# group with setsid, so the group cleanup never stops it. It must not take the
# lock with it: flock -o keeps the lock's fd out of the gate's children.
test_gate_daemon_that_escapes_the_group_does_not_keep_the_lock() {
    new_fixtures
    # The daemon's shell runs only after setsid has left the group, and writes
    # daemon.ready then; the leader waits for it before it ends.
    # shellcheck disable=SC2016 # $! and $GH_FIXTURES are expanded by the fake just
    fake_just 'setsid bash -c ": >\"$GH_FIXTURES/daemon.ready\"; exec sleep 30" </dev/null >/dev/null 2>&1 &
echo $! >"$GH_FIXTURES/daemon.pid"
'"$(fake_gate_wait_for '$GH_FIXTURES/daemon.ready')"'
exit 0'
    trap '[ -s "$fx/daemon.pid" ] && kill -KILL "$(cat "$fx/daemon.pid")" 2>/dev/null' EXIT
    out=$(cd "$repo" && PATH="$fx/bin:$PATH" "$tools/gate.sh" lint 2>&1)
    rc=$?
    want_rc 0
    [ -s "$fx/daemon.pid" ] || die "the fake gate didn't start its daemon"
    kill -0 "$(cat "$fx/daemon.pid")" 2>/dev/null || die "the daemon didn't outlive the gate"
    flock -n "$repo/.git/agentic/gates.lock" true || die "the daemon still holds gates.lock"
}

# The lock outlives the gate's own command: while gate.sh is still stopping a
# TERM-ignoring leftover, no other gate may take it.
test_gate_holds_the_lock_until_its_cleanup_ends() {
    new_fixtures; leaky_gate 'exit 0'
    (cd "$repo" && PATH="$fx/bin:$PATH" AGENTIC_GATE_KILL_AFTER=3 \
        "$tools/gate.sh" lint) >"$fx/out" 2>&1 &
    gate_pid=$!
    for ((tick = 0; tick < 50; tick++)); do
        [ -s "$fx/child.pid" ] && break
        sleep 0.1
    done
    [ -s "$fx/child.pid" ] || die "the fake gate didn't start its child"
    sleep 1 # the leader has exited; cleanup is waiting out its 3 s grace
    kill -0 "$(cat "$fx/child.pid")" 2>/dev/null || die "the leftover is already gone"
    ! flock -n "$repo/.git/agentic/gates.lock" true || die "the lock is free during cleanup"
    wait "$gate_pid"
    rc=$?
    out=$(cat "$fx/out")
    want_rc 0; want_out "leaving processes behind; stopped them"
    ! kill -0 "$(cat "$fx/child.pid")" 2>/dev/null || die "the leftover outlived the gate"
    flock -n "$repo/.git/agentic/gates.lock" true || die "gates.lock is still held"
}

test_gate_lock_wait_is_bounded() {
    new_fixtures; fake_just 'exit 0'
    mkdir -p "$repo/.git/agentic"
    # One process takes the lock and becomes the sleep, so killing $holder
    # releases the lock.
    # shellcheck disable=SC2016 # $1 is expanded by the inner bash
    bash -c 'exec 9>"$1"; flock 9; exec sleep 30' _ "$repo/.git/agentic/gates.lock" \
        >/dev/null 2>&1 &
    lock_holder=$! # not local: the EXIT trap runs after the function returns
    trap 'kill "$lock_holder" 2>/dev/null' EXIT
    sleep 0.2
    out=$(cd "$repo" && PATH="$fx/bin:$PATH" AGENTIC_TIMEOUT_LINT=1s "$tools/gate.sh" lint 2>&1)
    rc=$?
    kill "$lock_holder"; wait "$lock_holder" 2>/dev/null
    want_rc 124; want_out "Treat this as a HANG"
}

test_gate_dylint_runs_the_just_recipe() {
    new_fixtures; fake_just 'printf "%s\n" "$*" >"$GH_FIXTURES/just_args"; exit 0'
    out=$(cd "$repo" && PATH="$fx/bin:$PATH" "$tools/gate.sh" dylint 2>&1)
    rc=$?
    want_rc 0
    [ "$(cat "$fx/just_args")" = dylint ] \
        || die "just got '$(cat "$fx/just_args")', want 'dylint'"
}

test_gate_dylint_timeout_returns_124() {
    new_fixtures; fake_just 'exec sleep 30'
    out=$(cd "$repo" && PATH="$fx/bin:$PATH" AGENTIC_TIMEOUT_DYLINT=1s \
        "$tools/gate.sh" dylint 2>&1)
    rc=$?
    want_rc 124; want_out "Treat this as a HANG"
}

test_gate_worker_runs_the_just_recipe() {
    new_fixtures; fake_just 'printf "%s\n" "$*" >"$GH_FIXTURES/just_args"; exit 0'
    out=$(cd "$repo" && PATH="$fx/bin:$PATH" "$tools/gate.sh" worker 2>&1)
    rc=$?
    want_rc 0
    [ "$(cat "$fx/just_args")" = test-worker ] \
        || die "just got '$(cat "$fx/just_args")', want 'test-worker'"
}

test_gate_worker_timeout_returns_124() {
    new_fixtures; fake_just 'exec sleep 30'
    out=$(cd "$repo" && PATH="$fx/bin:$PATH" AGENTIC_TIMEOUT_WORKER=1s \
        "$tools/gate.sh" worker 2>&1)
    rc=$?
    want_rc 124; want_out "Treat this as a HANG"
}

test_gate_mcpjam_runs_the_just_recipe() {
    new_fixtures; fake_just 'printf "%s\n" "$*" >"$GH_FIXTURES/just_args"; exit 0'
    out=$(cd "$repo" && PATH="$fx/bin:$PATH" "$tools/gate.sh" mcpjam 2>&1)
    rc=$?
    want_rc 0
    [ "$(cat "$fx/just_args")" = test-mcpjam ] \
        || die "just got '$(cat "$fx/just_args")', want 'test-mcpjam'"
}

test_gate_mcpjam_never_reaches_a_remote_target_or_real_database() {
    new_fixtures
    # A fake just that records which of the steering variables it was given.
    # shellcheck disable=SC2016 # expanded by the fake just, not here
    fake_just 'env | grep -E "^(OKF_MCP_[A-Z_]+|DATABASE_URL|TEST_PG_URL)=" | sort \
        >"$GH_FIXTURES/just_env"; exit 0'
    out=$(cd "$repo" && PATH="$fx/bin:$PATH" OKF_MCP_URL=https://example.com/mcp \
        OKF_MCP_KEY=k OKF_MCP_API_URL=https://example.com/api/v1 OKF_MCP_ISOLATION=folder \
        OKF_MCP_CATALOG=scratch OKF_MCP_BIN=/elsewhere/okf-mcp \
        DATABASE_URL=postgres://example.internal/real TEST_PG_URL=postgres://127.0.0.1/test \
        OKF_MCP_KEEP=1 "$tools/gate.sh" mcpjam 2>&1)
    rc=$?
    want_rc 0
    local want=$'OKF_MCP_KEEP=1\nTEST_PG_URL=postgres://127.0.0.1/test'
    [ "$(cat "$fx/just_env")" = "$want" ] \
        || die "just got the environment '$(cat "$fx/just_env")', want '$want'"
}

test_gate_mcpjam_timeout_returns_124() {
    new_fixtures; fake_just 'exec sleep 30'
    out=$(cd "$repo" && PATH="$fx/bin:$PATH" AGENTIC_TIMEOUT_MCPJAM=1s \
        "$tools/gate.sh" mcpjam 2>&1)
    rc=$?
    want_rc 124; want_out "Treat this as a HANG"
}

test_gate_mcpjam_waits_for_the_gate_lock() {
    new_fixtures; fake_just 'exit 0'
    mkdir -p "$repo/.git/agentic"
    # shellcheck disable=SC2016 # $1 is expanded by the inner bash
    bash -c 'exec 9>"$1"; flock 9; exec sleep 30' _ "$repo/.git/agentic/gates.lock" \
        >/dev/null 2>&1 &
    lock_holder=$! # not local: the EXIT trap runs after the function returns
    trap 'kill "$lock_holder" 2>/dev/null' EXIT
    sleep 0.2
    out=$(cd "$repo" && PATH="$fx/bin:$PATH" AGENTIC_TIMEOUT_MCPJAM=1s \
        "$tools/gate.sh" mcpjam 2>&1)
    rc=$?
    kill "$lock_holder"; wait "$lock_holder" 2>/dev/null
    want_rc 124; want_out "for the gate lock"
}

test_gate_narrow_builds_offline() {
    new_fixtures
    mkdir -p "$fx/bin"
    # A fake cargo that records the SQLX_OFFLINE it was given.
    cat >"$fx/bin/cargo" <<'EOF'
#!/usr/bin/env bash
printf '%s\n' "${SQLX_OFFLINE-unset}" >"$GH_FIXTURES/sqlx_offline"
EOF
    chmod +x "$fx/bin/cargo"
    out=$(cd "$repo" && env -u SQLX_OFFLINE DATABASE_URL=postgres://db.example.invalid/x \
        PATH="$fx/bin:$PATH" "$tools/gate.sh" narrow --lib 2>&1)
    rc=$?
    want_rc 0
    [ "$(cat "$fx/sqlx_offline")" = true ] \
        || die "cargo got SQLX_OFFLINE=$(cat "$fx/sqlx_offline"), want true"
}

# fake_seed [body]: a seeding command on PATH that logs its args and cwd, then
# runs <body> (default: succeed). Tests name it with AGENTIC_SEED_CMD=fake-seed.
fake_seed() {
    mkdir -p "$fx/bin"
    printf '#!/usr/bin/env bash\nprintf "%%s %%s\\n" "$PWD" "$*" >>"$GH_FIXTURES/seed.log"\n%s\n' \
        "${1:-exit 0}" >"$fx/bin/fake-seed"
    chmod +x "$fx/bin/fake-seed"
}

test_gate_seeds_the_build_dir_before_a_building_gate() {
    new_fixtures; fake_seed; fake_just 'printf "%s\n" ran >>"$GH_FIXTURES/just.log"'
    out=$(cd "$repo" && PATH="$fx/bin:$PATH" AGENTIC_SEED_CMD=fake-seed \
        "$tools/gate.sh" lint 2>&1)
    rc=$?
    want_rc 0
    [ "$(cat "$fx/seed.log")" = "$repo seed ." ] || die "seed log: $(cat "$fx/seed.log")"
    [ -s "$fx/just.log" ] || die "the gate did not run"
}

test_gate_fmt_does_not_seed() {
    new_fixtures; fake_seed; fake_just 'exit 0'
    out=$(cd "$repo" && PATH="$fx/bin:$PATH" AGENTIC_SEED_CMD=fake-seed \
        "$tools/gate.sh" fmt 2>&1)
    rc=$?
    want_rc 0
    [ ! -e "$fx/seed.log" ] || die "fmt seeded: $(cat "$fx/seed.log")"
}

test_gate_builds_from_scratch_when_seeding_fails() {
    new_fixtures; fake_seed 'exit 7'; fake_just 'exit 3'
    out=$(cd "$repo" && PATH="$fx/bin:$PATH" AGENTIC_SEED_CMD=fake-seed \
        "$tools/gate.sh" lint 2>&1)
    rc=$?
    want_rc 3; want_out "'fake-seed seed' failed; building from scratch"
}

test_gate_skips_seeding_when_the_command_is_missing() {
    new_fixtures; fake_just 'exit 0'
    out=$(cd "$repo" && PATH="$fx/bin:$PATH" AGENTIC_SEED_CMD=no-such-seeder \
        "$tools/gate.sh" lint 2>&1)
    rc=$?
    want_rc 0; want_no_out "seed"
}

# --- gate.sh timing line (#1190) ---

# want_timing <regex>: the output has exactly one gate.sh timing line, and it
# matches <regex> (an extended regex for the part after "gate.sh: ").
want_timing() {
    local n
    n=$(grep -c '^gate\.sh: [a-z-]* \(waited\|seed\|ran\)' <<<"$out" || true)
    [ "$n" = 1 ] || die "want one timing line, found $n in: $out"
    grep -Eq -- "^gate\.sh: $1\$" <<<"$out" || die "no timing line matching '$1' in: $out"
}

# hold_gate_lock <seconds>: another process holds the gate lock for <seconds>.
hold_gate_lock() {
    mkdir -p "$repo/.git/agentic"
    # shellcheck disable=SC2016 # $1 and $2 are expanded by the inner bash
    bash -c 'exec 9>"$1"; flock 9; exec sleep "$2"' _ "$repo/.git/agentic/gates.lock" "$1" \
        >/dev/null 2>&1 &
    lock_holder=$! # not local: the EXIT trap runs after the function returns
    trap 'kill "$lock_holder" 2>/dev/null' EXIT
    sleep 0.2
}

test_gate_timing_line_on_success() {
    new_fixtures; fake_just 'echo gate output'
    out=$(cd "$repo" && PATH="$fx/bin:$PATH" "$tools/gate.sh" lint 2>&1)
    rc=$?
    want_rc 0
    want_timing 'lint waited [0-9]+ s for the lock, ran [0-9]+ s \(exit 0\)'
    [ "$(tail -n1 <<<"$out")" = "$(grep '^gate\.sh: lint' <<<"$out")" ] \
        || die "the timing line is not last: $out"
}

test_gate_timing_line_on_failure() {
    new_fixtures; fake_just 'exit 3'
    out=$(cd "$repo" && PATH="$fx/bin:$PATH" "$tools/gate.sh" lint 2>&1)
    rc=$?
    want_rc 3; want_timing 'lint waited [0-9]+ s for the lock, ran [0-9]+ s \(exit 3\)'
}

test_gate_timing_line_for_an_unlocked_gate_has_no_wait() {
    new_fixtures; fake_just 'exit 0'
    out=$(cd "$repo" && PATH="$fx/bin:$PATH" "$tools/gate.sh" fmt 2>&1)
    rc=$?
    want_rc 0; want_timing 'fmt ran [0-9]+ s \(exit 0\)'
    want_no_out "lock"
}

test_gate_timing_line_on_the_gates_own_timeout() {
    new_fixtures; fake_just 'exec sleep 30'
    out=$(cd "$repo" && PATH="$fx/bin:$PATH" AGENTIC_TIMEOUT_LINT=1s "$tools/gate.sh" lint 2>&1)
    rc=$?
    want_rc 124; want_out "Treat this as a HANG"
    want_timing 'lint waited [0-9]+ s for the lock, ran [0-9]+ s \(exit 124\)'
}

test_gate_timing_counts_the_wait_for_another_holder() {
    new_fixtures; fake_just 'exit 0'
    hold_gate_lock 2
    out=$(cd "$repo" && PATH="$fx/bin:$PATH" "$tools/gate.sh" lint 2>&1)
    rc=$?
    kill "$lock_holder" 2>/dev/null; wait "$lock_holder" 2>/dev/null
    want_rc 0; want_timing 'lint waited [0-9]+ s for the lock, ran [0-9]+ s \(exit 0\)'
    waited=$(sed -n 's/^gate\.sh: lint waited \([0-9]*\) s.*/\1/p' <<<"$out")
    [ "$waited" -ge 1 ] || die "waited $waited s behind a 2 s holder"
}

test_gate_timing_line_on_a_lock_timeout_says_it_waited_the_limit() {
    new_fixtures; fake_just 'exit 0'
    hold_gate_lock 30
    out=$(cd "$repo" && PATH="$fx/bin:$PATH" AGENTIC_TIMEOUT_LINT=1s "$tools/gate.sh" lint 2>&1)
    rc=$?
    kill "$lock_holder"; wait "$lock_holder" 2>/dev/null
    want_rc 124; want_out "for the gate lock"
    want_timing 'lint waited [1-9][0-9]* s for the lock \(exit 124\)'
}

test_gate_timing_line_includes_the_seed_time_when_seeding_runs() {
    new_fixtures; fake_seed 'sleep 1.2'; fake_just 'exit 0'
    out=$(cd "$repo" && PATH="$fx/bin:$PATH" AGENTIC_SEED_CMD=fake-seed \
        "$tools/gate.sh" lint 2>&1)
    rc=$?
    want_rc 0
    want_timing 'lint waited [0-9]+ s for the lock, seed [1-9][0-9]* s, ran [0-9]+ s \(exit 0\)'
}

test_gate_timing_line_has_no_seed_part_when_nothing_seeds() {
    new_fixtures; fake_seed; fake_just 'exit 0'
    out=$(cd "$repo" && PATH="$fx/bin:$PATH" "$tools/gate.sh" lint 2>&1)
    rc=$?
    want_rc 0; want_timing 'lint waited [0-9]+ s for the lock, ran [0-9]+ s \(exit 0\)'
    want_no_out "seed"
}

# --- remove-worktree.sh ---

# A worktree of the throwaway repo on its own branch. With <cargo> it carries a
# committed Cargo.toml (so git sees it as clean) and gets a stub `cargo` that
# logs its directory and whether the worktree existed, and prints cargo's
# summary line; `cargo.fail` in the fixtures makes it fail.
rw_worktree() {
    rw_branch=rw-$RANDOM$RANDOM
    rw_wt=$work/$rw_branch
    git -C "$repo" worktree add -q -b "$rw_branch" "$rw_wt" main
    if [ "${1:-}" = cargo ]; then
        echo '[package]' >"$rw_wt/Cargo.toml"
        git -C "$rw_wt" add Cargo.toml
        git_quiet -C "$rw_wt" commit -q -m cargo
    fi
    mkdir -p "$fx/bin"
    cat >"$fx/bin/cargo" <<'EOS'
#!/usr/bin/env bash
printf '%s exists=%s\n' "$PWD" "$([ -d "$PWD" ] && echo yes)" >>"$GH_FIXTURES/cargo.log"
[ ! -e "$GH_FIXTURES/cargo.fail" ] || { echo "error: clean failed" >&2; exit 101; }
[ ! -e "$GH_FIXTURES/cargo.hang" ] || exec sleep 30
echo "     Removed 12 files, 1.5GiB total" >&2
EOS
    chmod +x "$fx/bin/cargo"
}

run_remove() {
    out=$(cd "$repo" && PATH="$fx/bin:$PATH" AGENTIC_BASE_REF=main timeout 20 \
        "$tools/remove-worktree.sh" "$@" 2>&1)
    rc=$?
}

test_remove_refuses_the_main_checkout() {
    new_fixtures; rw_worktree
    run_remove "$repo"
    want_rc 1; want_out "REFUSE: $repo is the main checkout"
}

test_remove_refuses_an_unregistered_path() {
    new_fixtures; rw_worktree
    run_remove "$work/elsewhere"
    want_rc 1; want_out "is not a registered worktree"
    [ -d "$work/elsewhere" ] || die "removed it"
}

test_remove_refuses_with_a_process_inside() {
    new_fixtures; rw_worktree cargo
    (cd "$rw_wt" && exec sleep 30) >/dev/null 2>&1 &
    rw_pid=$!
    trap 'kill "$rw_pid" 2>/dev/null' EXIT
    sleep 0.3
    run_remove "$rw_wt"
    want_rc 1; want_out "processes are running inside"; want_out "$rw_pid $rw_wt sleep"
    case "$out" in *"sleep 30"*) die "printed the command line: $out" ;; esac
    [ -d "$rw_wt" ] || die "removed it"
    [ ! -e "$fx/cargo.log" ] || die "ran cargo clean"
}

test_remove_cleans_then_removes() {
    new_fixtures; rw_worktree cargo
    run_remove "$rw_wt"
    want_rc 0; want_out "REMOVED $rw_wt (cargo clean: removed 12 files, 1.5GiB total)"
    [ "$(cat "$fx/cargo.log")" = "$rw_wt exists=yes" ] || die "cargo ran: $(cat "$fx/cargo.log")"
    [ ! -e "$rw_wt" ] || die "worktree still there"
    git -C "$repo" rev-parse -q --verify "refs/heads/$rw_branch" >/dev/null || die "branch gone"
}

test_remove_keeps_the_worktree_when_clean_fails() {
    new_fixtures; rw_worktree cargo; touch "$fx/cargo.fail"
    run_remove "$rw_wt"
    want_rc 1; want_out "REFUSE: cargo clean failed (exit 101)"; want_out "error: clean failed"
    [ -d "$rw_wt" ] || die "removed it"
}

test_remove_keeps_the_worktree_when_clean_times_out() {
    new_fixtures; rw_worktree cargo; touch "$fx/cargo.hang"
    out=$(cd "$repo" && PATH="$fx/bin:$PATH" AGENTIC_BASE_REF=main AGENTIC_CLEAN_TIMEOUT=1 \
        timeout 20 "$tools/remove-worktree.sh" "$rw_wt" 2>&1)
    rc=$?
    want_rc 1; want_out "timed out after 1 s"
    [ -d "$rw_wt" ] || die "removed it"
}

test_remove_leaves_no_log_when_termed_during_the_clean() {
    # #1193: a TERM during the bounded cargo clean must not leave its log behind.
    new_fixtures; rw_worktree cargo; touch "$fx/cargo.hang"; mkdir -p "$fx/tmp"
    (cd "$repo" && PATH="$fx/bin:$PATH" TMPDIR="$fx/tmp" AGENTIC_BASE_REF=main \
        exec "$tools/remove-worktree.sh" "$rw_wt" >"$fx/rw.out" 2>&1) &
    local rw_pid=$! tick
    for ((tick = 0; tick < 100; tick++)); do
        [ -e "$fx/cargo.log" ] && break
        sleep 0.1
    done
    [ -e "$fx/cargo.log" ] || { kill -KILL "$rw_pid"; die "the stub cargo never started"; }
    kill -TERM "$rw_pid"
    for ((tick = 0; tick < 100; tick++)); do
        kill -0 "$rw_pid" 2>/dev/null || break
        sleep 0.1
    done
    kill -0 "$rw_pid" 2>/dev/null && { kill -KILL "$rw_pid"; die "TERM did not stop it"; }
    wait "$rw_pid" 2>/dev/null
    [ -z "$(ls -A "$fx/tmp")" ] || die "a temp file was left behind: $(ls -A "$fx/tmp")"
    [ -d "$rw_wt" ] || die "removed the worktree"
}

test_remove_skips_clean_without_cargo_toml() {
    new_fixtures; rw_worktree
    run_remove "$rw_wt"
    want_rc 0; want_out "no Cargo.toml, cargo clean skipped"
    [ ! -e "$fx/cargo.log" ] || die "ran cargo"
}

test_remove_deletes_a_merged_branch_with_branch() {
    new_fixtures; rw_worktree
    run_remove "$rw_wt" --branch
    want_rc 0; want_out "deleted branch $rw_branch"
    ! git -C "$repo" rev-parse -q --verify "refs/heads/$rw_branch" >/dev/null || die "branch kept"
}

test_remove_refuses_an_unmerged_branch_unless_forced() {
    new_fixtures; rw_worktree cargo
    run_remove "$rw_wt" --branch
    want_rc 1; want_out "is not merged into main"
    [ -d "$rw_wt" ] && [ ! -e "$fx/cargo.log" ] || die "changed something before refusing"
    run_remove "$rw_wt" --force-branch
    want_rc 0; want_out "deleted branch $rw_branch"
    ! git -C "$repo" rev-parse -q --verify "refs/heads/$rw_branch" >/dev/null || die "branch kept"
}

# A sub-issue branch merged into a milestone branch, not main: --base names it.
test_remove_deletes_a_branch_merged_into_the_given_base() {
    new_fixtures; rw_worktree cargo
    git -C "$repo" branch -q "milestone-$rw_branch" "$rw_branch"
    run_remove "$rw_wt" --branch
    want_rc 1; want_out "is not merged into main"
    run_remove "$rw_wt" --branch --base "milestone-$rw_branch"
    want_rc 0; want_out "deleted branch $rw_branch"
    ! git -C "$repo" rev-parse -q --verify "refs/heads/$rw_branch" >/dev/null || die "branch kept"
}

test_remove_wants_a_ref_after_base() {
    new_fixtures; rw_worktree
    run_remove "$rw_wt" --branch --base
    want_rc 2; want_out "usage: remove-worktree.sh"
    [ -d "$rw_wt" ] || die "removed it"
}


# --- expect.sh and the watchdog's quiet-agent check (#850) ---

# expect_wt <name>: a fresh linked worktree $work/<name> and the real time in
# $t0. The fake clock starts at $t0, so the worktree's HEAD reflog is not newer
# than the expectation.
expect_wt() {
    rm -rf "$repo/.git/agentic-expect" # every test starts with an empty ledger
    git -C "$repo" worktree add -q -b "$1" "$work/$1"
    t0=$(date +%s)
    echo "$t0" >"$fx/clock"
}

# expect_watchdog <seconds after t0>: one watchdog pass on the fake clock, with
# the process limits out of reach so only the quiet-agent check can fire.
expect_watchdog() {
    echo $((t0 + $1)) >"$fx/clock"
    out=$(cd "$repo" && env AGENTIC_NOW_CMD=fake-now AGENTIC_WATCHDOG_TEST=999999 \
        AGENTIC_WATCHDOG_GATE=999999 timeout 15 "$tools/watchdog.sh" --once 2>&1)
    rc=$?
}

# expect_watchdog_at <epoch>: like expect_watchdog, with the fake clock at <epoch>.
expect_watchdog_at() {
    echo "$1" >"$fx/clock"
    out=$(cd "$repo" && env AGENTIC_NOW_CMD=fake-now AGENTIC_WATCHDOG_TEST=999999 \
        AGENTIC_WATCHDOG_GATE=999999 timeout 15 "$tools/watchdog.sh" --once 2>&1)
    rc=$?
}

expect_start() {
    (cd "$repo" && AGENTIC_NOW_CMD=fake-now "$tools/expect.sh" start "$@") >/dev/null
}

test_expect_start_list_done() {
    new_fixtures; expect_wt ex-list
    expect_start "$work/ex-list" "task 5"
    echo $((t0 + 90)) >"$fx/clock"
    out=$(cd "$repo" && AGENTIC_NOW_CMD=fake-now "$tools/expect.sh" list 2>&1); rc=$?
    want_rc 0; want_out "ex-list waiting 90s: task 5"
    out=$(cd "$repo" && "$tools/expect.sh" done "$work/ex-list" 2>&1); rc=$?
    want_rc 0
    out=$(cd "$repo" && "$tools/expect.sh" list 2>&1)
    want_no_out "ex-list"
}

test_expect_rejects_a_missing_worktree_and_bad_usage() {
    new_fixtures
    out=$(cd "$repo" && "$tools/expect.sh" start "$work/nowhere" 2>&1); rc=$?
    want_rc 2
    out=$(cd "$repo" && "$tools/expect.sh" 2>&1); rc=$?
    want_rc 2; want_out "usage:"
}

test_watchdog_flags_an_idle_agent_with_a_report_pending() {
    new_fixtures; expect_wt ex-idle
    expect_start "$work/ex-idle" "task 7 review"
    expect_watchdog 1300
    want_rc 3; want_out "agent at ex-idle quiet for 1300s"; want_out "task 7 review"
    # A restarted watchdog alerts again while the agent is still idle.
    expect_watchdog 1400
    want_rc 3; want_out "quiet for 1400s"
}

test_watchdog_ignores_an_agent_under_the_quiet_limit() {
    new_fixtures; expect_wt ex-young
    expect_start "$work/ex-young" "task 7"
    expect_watchdog 1100
    want_rc 0
    # The limit is configurable.
    echo $((t0 + 100)) >"$fx/clock"
    out=$(cd "$repo" && env AGENTIC_NOW_CMD=fake-now AGENTIC_WATCHDOG_QUIET=50 \
        timeout 15 "$tools/watchdog.sh" --once 2>&1); rc=$?
    want_rc 3; want_out "quiet for 100s"
}

test_watchdog_ignores_an_agent_with_a_fresh_log() {
    new_fixtures; expect_wt ex-log
    expect_start "$work/ex-log" "task 7"
    mkdir -p "$work/ex-log/target/i7-logs/deep"
    touch -d "@$((t0 + 1000))" "$work/ex-log/target/i7-logs/deep/gate.log"
    expect_watchdog 1300
    want_rc 0
    expect_watchdog 2300
    want_rc 3; want_out "quiet for 1300s"
}

test_watchdog_counts_a_new_commit_as_activity() {
    new_fixtures; expect_wt ex-commit
    # The expectation is older than the quiet limit before the commit.
    echo $((t0 - 5000)) >"$fx/clock"
    expect_start "$work/ex-commit" "task 7"
    git_quiet -C "$work/ex-commit" commit -q --allow-empty -m work
    now=$(date +%s)
    echo "$now" >"$fx/clock"
    expect_watchdog_at "$now"
    want_rc 0
    # Without the commit the same expectation is idle.
    expect_watchdog_at $((now + 1300))
    want_rc 3
}

test_watchdog_counts_a_fresh_start_as_activity() {
    new_fixtures; expect_wt ex-rearm
    # The worktree's last commit and logs are older than the limit; the agent
    # was sent back (expect.sh start) 100 s ago.
    mkdir -p "$work/ex-rearm/target/i7-logs"
    touch -d "@$((t0 - 9000))" "$work/ex-rearm/target/i7-logs/gate.log"
    echo $((t0 + 3000)) >"$fx/clock"
    expect_start "$work/ex-rearm" "task 7 round 2"
    expect_watchdog 3100
    want_rc 0
    expect_watchdog 4400
    want_rc 3; want_out "quiet for 1400s"
}

test_watchdog_counts_a_checkout_of_an_old_commit_as_activity() {
    new_fixtures; expect_wt ex-old
    # An old commit: its committer date is long before the quiet limit.
    GIT_COMMITTER_DATE="@$((t0 - 90000)) +0000" GIT_AUTHOR_DATE="@$((t0 - 90000)) +0000" \
        git_quiet -C "$work/ex-old" commit -q --allow-empty -m old
    git -C "$work/ex-old" branch old-tip
    git_quiet -C "$work/ex-old" commit -q --allow-empty -m newer
    echo $((t0 - 5000)) >"$fx/clock"
    expect_start "$work/ex-old" "task 7"
    # A fresh reflog event pointing at the OLD commit.
    git_quiet -C "$work/ex-old" checkout -q --detach old-tip
    now=$(date +%s)
    expect_watchdog_at "$now"
    want_rc 0
}

# A fake working process with its cwd in the worktree: start_busy <dir> <cmd...>.
start_busy() {
    fake_pids=()
    trap 'kill "${fake_pids[@]}" 2>/dev/null' EXIT
    (cd "$1" && shift && exec "$@") >/dev/null 2>&1 &
    fake_pids+=("$!")
    sleep 0.3
}

test_watchdog_ignores_an_agent_with_a_gate_waiting_for_the_lock() {
    new_fixtures; expect_wt ex-gate
    expect_start "$work/ex-gate" "task 7"
    # gate.sh waiting on the shared lock is a bash running gate.sh (then flock).
    printf '#!/usr/bin/env bash\nsleep 30\n' >"$fx/gate.sh"
    start_busy "$work/ex-gate" bash "$fx/gate.sh" lint
    expect_watchdog 3000
    want_rc 0
}

test_watchdog_ignores_an_agent_with_a_cargo_build_running() {
    new_fixtures; expect_wt ex-cargo
    expect_start "$work/ex-cargo" "task 7"
    mkdir -p "$fx/bin"; cp "$(command -v sleep)" "$fx/bin/cargo"
    mkdir -p "$work/ex-cargo/target"
    start_busy "$work/ex-cargo/target" "$fx/bin/cargo" 30
    expect_watchdog 3000
    want_rc 0
}

test_watchdog_alert_leaves_out_command_lines() {
    new_fixtures; expect_wt ex-secret
    expect_start "$work/ex-secret" "task 7"
    # An unrelated process in the worktree does not count as work, and its
    # command line (here a fake credential) never reaches the alert.
    start_busy "$work/ex-secret" bash -c 'exec -a "--token=SECRETVALUE" sleep 30'
    expect_watchdog 1300
    want_rc 3; want_out "agent at ex-secret"; want_no_out "SECRETVALUE"
}

test_watchdog_drops_an_expectation_whose_worktree_is_gone() {
    new_fixtures; expect_wt ex-gone
    expect_start "$work/ex-gone" "task 7"
    git -C "$repo" worktree remove --force "$work/ex-gone"
    expect_watchdog 5000
    want_rc 0; want_out "dropped the expectation for ex-gone"
    [ ! -e "$repo/.git/agentic-expect/ex-gone" ] || die "the entry is still there"
}

test_watchdog_stops_alerting_once_the_report_is_done() {
    new_fixtures; expect_wt ex-done
    expect_start "$work/ex-done" "task 7"
    expect_watchdog 1300
    want_rc 3
    (cd "$repo" && "$tools/expect.sh" done "$work/ex-done") >/dev/null
    expect_watchdog 1400
    want_rc 0
}

# --- agentic_run_bounded: the caller's traps (#1193) ---

test_run_bounded_puts_the_callers_traps_back() {
    new_fixtures
    local trapped
    trapped=$(bash -c 'source "$1/lib.sh"; trap "echo mine" TERM; agentic_run_bounded 5 true
        trap -p TERM' _ "$tools")
    [ "$trapped" = "trap -- 'echo mine' SIGTERM" ] || die "the caller's TERM trap was lost: $trapped"
}

test_run_bounded_hands_a_term_to_the_callers_trap() {
    # The signal stops the group, then reaches the caller's own handler.
    new_fixtures
    (exec bash -c 'source "$1/lib.sh"; trap "echo caller-trap; exit 7" TERM
        agentic_run_bounded 30 bash -c ": >\"\$1\"; exec sleep 30" _ "$2"' \
        _ "$tools" "$fx/started" >"$fx/rb.out" 2>&1) &
    local rb_pid=$! tick
    for ((tick = 0; tick < 100; tick++)); do
        [ -e "$fx/started" ] && break
        sleep 0.1
    done
    [ -e "$fx/started" ] || { kill -KILL "$rb_pid"; die "the command never started"; }
    kill -TERM "$rb_pid"
    rc=0; wait "$rb_pid" || rc=$?
    out=$(<"$fx/rb.out")
    want_rc 7; want_out "caller-trap"
}

# --- agentic_run_bounded: processes that survive KILL (#948) ---
# A real process in an uninterruptible wait can't be made in a test, so the
# harness shadows `kill` with a function that swallows TERM and KILL aimed at
# the command's group (its leader's pid, which the command records) (the process then stays alive, as one in D
# state would) and passes every other call, `kill -0` included, to the builtin.

# write_kill_shim: writes $fx/shim.sh, which shadows `kill`. TERM or KILL aimed
# at the group whose leader pid is in $fx/leader.pid has no effect, except that
# when $fx/leader_dies exists the leader itself is really killed. Everything
# else, `kill -0` included, goes to the builtin.
write_kill_shim() {
    cat >"$fx/shim.sh" <<'SHIM'
kill() {
    local immortal
    immortal=$(cat "$GH_FIXTURES/leader.pid" 2>/dev/null)
    case "$1" in
        -TERM | -KILL)
            case "${3:-$2}" in
                "-$immortal" | "$immortal")
                    [ ! -e "$GH_FIXTURES/leader_dies" ] || builtin kill -KILL "$immortal" 2>/dev/null
                    return 0 ;;
            esac ;;
    esac
    builtin kill "$@"
}
SHIM
}

# immortal_harness <body>: writes $fx/harness.sh, which sources lib.sh and the
# shim and then runs <body>.
immortal_harness() {
    write_kill_shim
    cat >"$fx/harness.sh" <<HARNESS
source "$tools/lib.sh"
source "$fx/shim.sh"
agentic_kill_after=1
agentic_reap_after=1
$1
HARNESS
}

# stop_survivors: stop what the harness made immortal (the test's own cleanup).
stop_survivors() {
    cat "$fx/leader.pid" "$fx/member.pid" 2>/dev/null | xargs -r kill -KILL 2>/dev/null
}

test_run_bounded_abandons_a_leader_that_survives_kill_after_a_timeout() {
    new_fixtures
    trap stop_survivors EXIT
    # shellcheck disable=SC2016 # expanded by the harness, not here
    immortal_harness 'agentic_run_bounded 1 bash -c "echo \$\$ >\"$GH_FIXTURES/leader.pid\"; exec sleep 60 >/dev/null 2>&1"
echo "rc=$? timed_out=$agentic_timed_out"'
    started=$SECONDS
    out=$(cd "$work" && timeout 30 bash "$fx/harness.sh" 2>&1)
    rc=$?
    want_rc 0; want_out "rc=124 timed_out=1"; want_out "survived KILL"; want_out "abandoned"
    [ $((SECONDS - started)) -le 10 ] || die "took $((SECONDS - started)) s"
    kill -0 "$(cat "$fx/leader.pid")" 2>/dev/null || die "the leader was not left running"
}

test_run_bounded_abandons_a_leftover_that_survives_kill_after_a_normal_exit() {
    new_fixtures
    trap stop_survivors EXIT
    # shellcheck disable=SC2016 # expanded by the harness, not here
    immortal_harness 'cmd='\''(exec sleep 60) </dev/null >/dev/null 2>&1 & echo $! >"$GH_FIXTURES/member.pid"; echo $$ >"$GH_FIXTURES/leader.pid"; exit 3'\''
agentic_run_bounded 5 bash -c "$cmd"
echo "rc=$?"'
    started=$SECONDS
    out=$(cd "$work" && timeout 30 bash "$fx/harness.sh" 2>&1)
    rc=$?
    want_rc 0; want_out "rc=3"; want_out "some survived KILL and were abandoned"
    [ $((SECONDS - started)) -le 10 ] || die "took $((SECONDS - started)) s"
    kill -0 "$(cat "$fx/member.pid")" 2>/dev/null || die "the leftover was not left running"
}

test_run_bounded_abandons_a_group_member_that_survives_kill_after_a_timeout() {
    # The leader dies (bash reaps it); a member of its group survives KILL.
    new_fixtures
    trap stop_survivors EXIT
    touch "$fx/leader_dies"
    # shellcheck disable=SC2016 # expanded by the harness, not here
    immortal_harness 'agentic_run_bounded 1 bash -c "(exec sleep 60 </dev/null >/dev/null 2>&1) & echo \$! >\"$GH_FIXTURES/member.pid\"; echo \$\$ >\"$GH_FIXTURES/leader.pid\"; exec sleep 60 >/dev/null 2>&1"
echo "rc=$?"'
    started=$SECONDS
    out=$(cd "$work" && timeout 30 bash "$fx/harness.sh" 2>&1)
    rc=$?
    want_rc 0; want_out "rc=124"; want_out "processes of its group survived KILL"
    [ $((SECONDS - started)) -le 10 ] || die "took $((SECONDS - started)) s"
    kill -0 "$(cat "$fx/member.pid")" 2>/dev/null || die "the member was not left running"
    ! kill -0 "$(cat "$fx/leader.pid")" 2>/dev/null || die "the leader should be dead"
}

test_gate_frees_the_lock_when_it_abandons_a_process_that_survives_kill() {
    new_fixtures
    trap stop_survivors EXIT
    write_kill_shim
    # shellcheck disable=SC2016 # $$ and $GH_FIXTURES are expanded by the fake just
    fake_just 'echo $$ >"$GH_FIXTURES/leader.pid"; exec sleep 60 >/dev/null 2>&1'
    started=$SECONDS
    out=$(cd "$repo" && PATH="$fx/bin:$PATH" BASH_ENV="$fx/shim.sh" AGENTIC_TIMEOUT_LINT=1s \
        AGENTIC_GATE_KILL_AFTER=1 timeout 30 "$tools/gate.sh" lint 2>&1)
    rc=$?
    want_rc 124; want_out "survived KILL"; want_out "abandoned"
    [ $((SECONDS - started)) -le 15 ] || die "took $((SECONDS - started)) s"
    kill -0 "$(cat "$fx/leader.pid")" 2>/dev/null || die "the leader was not left running"
    flock -n "$repo/.git/agentic/gates.lock" true || die "gates.lock is still held"
}

test_run_bounded_does_not_use_pgrep_for_the_leftover_check() {
    # pgrep scans /proc like ps and can stall the same way, so it must not run.
    new_fixtures
    mkdir "$fx/bin"
    printf '#!/bin/sh\ntouch "%s/pgrep.called"\nexec sleep 60\n' "$fx" >"$fx/bin/pgrep"
    chmod +x "$fx/bin/pgrep"
    # shellcheck disable=SC2016 # expanded by the harness, not here
    immortal_harness 'agentic_run_bounded 5 bash -c "(exec sleep 60) </dev/null >/dev/null 2>&1 & exit 0"
echo "rc=$?"'
    out=$(cd "$work" && PATH="$fx/bin:$PATH" timeout 30 bash "$fx/harness.sh" 2>&1)
    rc=$?
    want_rc 0; want_out "rc=0"; want_out "leaving processes behind; stopped them"
    [ ! -e "$fx/pgrep.called" ] || die "pgrep was called"
}

# --- agentic_run_captured: captured calls stay bounded (#1191) ---
# A `$(agentic_gh ...)` waits for end-of-file on its pipe. A child in its own
# session that ignores TERM and holds stdout open must not stretch the call.

# captured_harness <body>: writes $fx/harness.sh, which sources lib.sh, keeps
# every temp file in $fx/tmp and then runs <body>.
captured_harness() {
    mkdir -p "$fx/tmp"
    cat >"$fx/harness.sh" <<HARNESS
export TMPDIR="$fx/tmp"
source "$tools/lib.sh"
agentic_kill_after=1
agentic_reap_after=1
$1
HARNESS
}

want_no_temp_files() {
    [ -z "$(ls -A "$fx/tmp")" ] || die "temp files left: $(ls -A "$fx/tmp")"
}

test_captured_call_returns_in_time_when_a_setsid_child_holds_stdout() {
    new_fixtures
    trap 'xargs -r kill -KILL <"$fx/holder.pid" 2>/dev/null' EXIT
    # The holder leaves the leader's group, ignores TERM and keeps stdout and
    # stderr open for 30 s; the leader prints, then hangs past the limit.
    # shellcheck disable=SC2016 # expanded by the harness, not here
    captured_harness 'start=$(date +%s%N)
out=$(agentic_run_captured 1 bash -c "echo partial; echo partial-err >&2; setsid bash -c \"trap \\\"\\\" TERM; echo \\\$\\\$ >\\\"$GH_FIXTURES/holder.pid\\\"; exec sleep 30\" & sleep 30")
echo "rc=$? ms=$(( ($(date +%s%N) - start) / 1000000 )) out=$out"'
    out=$(cd "$work" && timeout 20 bash "$fx/harness.sh" 2>&1)
    rc=$?
    want_rc 0; want_out "rc=124"; want_out "out=partial"; want_out "partial-err"
    ms=${out#*ms=}; ms=${ms%% *}
    [ "$ms" -lt 4000 ] || die "returned after $ms ms, want under 4000"
    holder=$(cat "$fx/holder.pid") || die "no holder pid"
    kill -0 "$holder" 2>/dev/null || die "holder $holder is not running (test set-up)"
    kill -KILL "$holder"
    want_no_temp_files
}

test_captured_call_returns_at_once_when_the_leader_leaves_a_holder_behind() {
    new_fixtures
    trap 'xargs -r kill -KILL <"$fx/holder.pid" 2>/dev/null' EXIT
    # shellcheck disable=SC2016 # expanded by the harness, not here
    captured_harness 'start=$(date +%s%N)
out=$(agentic_run_captured 10 bash -c "echo done; setsid bash -c \"trap \\\"\\\" TERM; echo \\\$\\\$ >\\\"$GH_FIXTURES/holder.pid\\\"; exec sleep 30\" & sleep 0.3; exit 3")
echo "rc=$? ms=$(( ($(date +%s%N) - start) / 1000000 )) out=$out"'
    out=$(cd "$work" && timeout 20 bash "$fx/harness.sh" 2>&1)
    rc=$?
    want_rc 0; want_out "rc=3"; want_out "out=done"
    ms=${out#*ms=}; ms=${ms%% *}
    [ "$ms" -lt 4000 ] || die "returned after $ms ms, want under 4000"
    kill -KILL "$(cat "$fx/holder.pid")" || die "holder not running"
    want_no_temp_files
}

test_captured_call_passes_output_stderr_and_status_through() {
    new_fixtures
    # shellcheck disable=SC2016 # expanded by the harness, not here
    captured_harness 'out=$(agentic_run_captured 10 bash -c "echo out-line; echo err-line >&2; read -r x; echo stdin=[\$x]; exit 7" 2>"$GH_FIXTURES/err")
echo "rc=$?"
echo "out=$out"
echo "err=$(cat "$GH_FIXTURES/err")"'
    out=$(cd "$work" && timeout 20 bash "$fx/harness.sh" 2>&1 </dev/null)
    rc=$?
    want_rc 0; want_out "rc=7"; want_out "out=out-line"; want_out "stdin=[]"
    want_out "err=err-line"
    want_no_out "warning"
    want_no_temp_files
}

test_captured_call_with_no_time_left_returns_124_without_running() {
    new_fixtures
    captured_harness 'out=$(agentic_run_captured 0 touch "$GH_FIXTURES/ran"); echo "rc=$?"'
    out=$(cd "$work" && timeout 20 bash "$fx/harness.sh" 2>&1)
    rc=$?
    want_rc 0; want_out "rc=124"
    [ ! -e "$fx/ran" ] || die "the command ran"
    want_no_temp_files
}

test_captured_call_removes_its_files_when_it_is_terminated() {
    new_fixtures
    captured_harness 'agentic_run_captured 30 sleep 30'
    (cd "$work" && exec bash "$fx/harness.sh" >/dev/null 2>&1) &
    harness_pid=$!
    trap 'kill -KILL "$harness_pid" 2>/dev/null' EXIT
    # shellcheck disable=SC2016 # the inner shell expands them
    timeout 5 bash -c 'until [ "$(ls "$1" | wc -l)" -ge 2 ]; do sleep 0.1; done' _ "$fx/tmp" \
        || die "no temp files appeared"
    kill -TERM "$harness_pid"
    # shellcheck disable=SC2016 # the inner shell expands them
    timeout 10 bash -c 'while kill -0 "$1" 2>/dev/null; do sleep 0.1; done' _ "$harness_pid" \
        || die "the harness did not stop on TERM"
    want_no_temp_files
}

# --- poll and tick overrides (#1216) ---

# ticks_run <budget> <ticks> <cmd...>: agentic_run_captured with
# AGENTIC_TICKS_PER_SEC=<ticks>; prints "rc=<status> ms=<elapsed>".
ticks_run() {
    local budget=$1 ticks=$2
    shift 2
    AGENTIC_TICKS_PER_SEC=$ticks bash -c '
        source "$1"; budget=$2; shift 2
        start=$(date +%s%N)
        agentic_run_captured "$budget" "$@" >/dev/null 2>&1
        echo "rc=$? ms=$((($(date +%s%N) - start) / 1000000))"' _ "$tools/lib.sh" "$budget" "$@"
}

test_ticks_default_to_a_tenth_of_a_second_and_bad_values_fall_back() {
    local want
    for want in "" 0 abc 1001 -5 2.5; do
        [ "$(AGENTIC_TICKS_PER_SEC=$want bash -c 'source "$1"; echo "$agentic_ticks_per_sec $agentic_tick"' _ "$tools/lib.sh")" = "10 0.100000" ] \
            || die "'$want' did not fall back to 10 ticks (0.100000 s)"
    done
    [ "$(env -u AGENTIC_TICKS_PER_SEC bash -c 'source "$1"; echo "$agentic_ticks_per_sec $agentic_tick"' _ "$tools/lib.sh")" = "10 0.100000" ] \
        || die "the default is not 10 ticks a second"
    [ "$(AGENTIC_TICKS_PER_SEC=100 bash -c 'source "$1"; echo "$agentic_tick"' _ "$tools/lib.sh")" = 0.010000 ] \
        || die "100 ticks a second is not a 0.010000 s tick"
}

test_finer_ticks_notice_a_finished_command_sooner() {
    new_fixtures
    local line ms
    line=$(ticks_run 5 100 sleep 0.3); ms=${line##*ms=}
    [ "${line%% *}" = rc=0 ] || die "a 0.3 s command under a 5 s budget: $line"
    [ "$ms" -lt 1500 ] || die "took $ms ms: the 0.3 s command was not noticed at once"
}

test_finer_ticks_do_not_stretch_the_time_limit() {
    new_fixtures
    local line ms
    # The limit is measured on the clock, not by counting ticks: 1 s stays 1 s
    # (plus the cleanup, which has nothing to stop but the sleep itself).
    line=$(agentic_kill_after=1 agentic_reap_after=1 ticks_run 1 1000 sleep 30); ms=${line##*ms=}
    [ "${line%% *}" = rc=124 ] || die "a 30 s command under a 1 s budget: $line"
    [ "$ms" -ge 1000 ] || die "stopped after $ms ms, before its 1 s budget"
    [ "$ms" -le 4000 ] || die "stopped after $ms ms: finer ticks stretched the 1 s budget"
}

test_watch_polls_every_30_seconds_by_default() {
    new_fixtures; good_pr "$head_a"
    echo '[]' >"$fx/api/pulls_7_reviews.1.json"
    unset AGENTIC_WATCH_POLL
    run_watch
    # 900 s at 30 s: a pass at 1000 and every 30 s to 1870 is 30 passes that call gh
    # (the pass at the 1900 deadline has no time left for a call).
    [ "$(grep -c '^pr view 7 .*headRefOid' "$fx/calls.log")" = 30 ] \
        || die "$(grep -c '^pr view 7 .*headRefOid' "$fx/calls.log") passes, want 30"
    AGENTIC_WATCH_POLL=100 run_watch
    [ "$(grep -c '^pr view 7 .*headRefOid' "$fx/calls.log")" = 39 ] \
        || die "a 100 s poll did not add 9 passes (1000..1800): $(grep -c '^pr view 7 .*headRefOid' "$fx/calls.log") in all"
}

test_a_tick_is_a_whole_reciprocal_of_the_rate() {
    local rate want
    for rate in "1 1.000000" "2 0.500000" "7 0.142857" "10 0.100000" "1000 0.001000"; do
        set -- $rate
        want=$2
        [ "$(AGENTIC_TICKS_PER_SEC=$1 bash -c 'source "$1"; echo "$agentic_tick"' _ "$tools/lib.sh")" = "$want" ] \
            || die "rate $1: tick is not $want"
    done
}

test_a_slow_tick_does_not_cut_a_budget_short() {
    new_fixtures
    local line rate
    # A 1.2 s command under a 2 s budget must finish, not time out, at a rate of
    # 1 tick a second (a 0.1000 tick used to make that budget 0.2 s) and at one
    # that does not divide 1000 (7).
    for rate in 1 7; do
        line=$(ticks_run 2 "$rate" sleep 1.2)
        [ "${line%% *}" = rc=0 ] || die "rate $rate: a 1.2 s command under a 2 s budget: $line"
    done
}

test_the_job_limit_is_four() {
    local out rc
    out=$(AGENTIC_TEST_JOBS=99 "$tests_dir/run.sh" test_ticks_default_to 2>&1); rc=$?
    [ "$rc" = 0 ] || die "rc=$rc: $out"
    want_out "above the limit of 4; using 4"; want_out "(jobs: 4)"
    out=$(AGENTIC_TEST_JOBS=4 "$tests_dir/run.sh" test_ticks_default_to 2>&1)
    case "$out" in *"above the limit"*) die "4 was clamped: $out" ;; esac
    want_out "(jobs: 4)" # (from the 4 run)
    out=$(AGENTIC_TEST_JOBS=0 "$tests_dir/run.sh" test_ticks_default_to 2>&1); rc=$?
    [ "$rc" = 2 ] || die "0 jobs: rc=$rc: $out"
}

test_the_pool_collects_tests_that_end_while_another_is_cleaned_up() {
    new_fixtures
    # The first test leaves a TERM-ignoring process in its group, so stopping
    # its group takes 2 s; the three after it end meanwhile. A pool that
    # counted on `wait -n` to name each would lose them and spin.
    cat >"$fx/extra.sh" <<'EXTRA'
test_zz_leaves_a_stubborn_process() {
    (trap '' TERM; exec sleep 30) </dev/null >/dev/null 2>&1 &
}
test_zz_fast_a() { :; }
test_zz_fast_b() { :; }
test_zz_fast_c() { :; }
EXTRA
    out=$(AGENTIC_TEST_EXTRA_FILE=$fx/extra.sh AGENTIC_TEST_JOBS=4 timeout 30 \
        "$tests_dir/run.sh" test_zz_ 2>&1); rc=$?
    [ "$rc" = 0 ] || die "rc=$rc: $out"
    want_out "4 passed, 0 failed"
}

# --- runner ---

# Tests of run.sh itself add fixture tests this way (see the pool test).
# shellcheck source=/dev/null
[ -z "${AGENTIC_TEST_EXTRA_FILE:-}" ] || source "$AGENTIC_TEST_EXTRA_FILE"
all_tests=$(declare -F | awk '$3 ~ /^test_/ {print $3}')

# One test, in this process (`run.sh --one <name>`): its output and its
# "<status> <ms>" line go to $AGENTIC_TEST_RESULTS for the parent to print.
if [ -n "$one" ]; then
    t0=$(date +%s%N)
    if result=$("$one" 2>&1); then status=0; else status=1; fi
    printf '%s\n' "$result" >"$AGENTIC_TEST_RESULTS/$one.out"
    echo "$status $((($(date +%s%N) - t0) / 1000000))" >"$AGENTIC_TEST_RESULTS/$one.res"
    exit "$status"
fi

jobs=${AGENTIC_TEST_JOBS:-4}
case "$jobs" in
    [1-9] | [1-9][0-9] | [1-9][0-9][0-9]) ;;
    *) echo "run.sh: AGENTIC_TEST_JOBS must be a whole number from 1, got '$jobs'" >&2; exit 2 ;;
esac
if [ "$jobs" -gt 4 ]; then
    echo "run.sh: AGENTIC_TEST_JOBS=$jobs is above the limit of 4; using 4" >&2
    jobs=4
fi

selected=""
for test_name in $all_tests; do
    if [ $# = 0 ]; then
        selected="$selected $test_name"
        continue
    fi
    for filter in "$@"; do
        case "$test_name" in *"$filter"*) selected="$selected $test_name"; break ;; esac
    done
done
[ -n "$selected" ] || { echo "run.sh: no test matches: $*" >&2; exit 1; }

results=$work/results
mkdir "$results"
export AGENTIC_TEST_RESULTS=$results

# stop_session <pid>: stop whatever is left in a finished test's group
# (its pid is its group id): TERM, KILL after 2 s.
stop_session() {
    local tick
    kill -TERM -- "-$1" 2>/dev/null || return 0
    for ((tick = 0; tick < 20; tick++)); do
        kill -0 -- "-$1" 2>/dev/null || return 0
        sleep 0.1
    done
    kill -KILL -- "-$1" 2>/dev/null || true
}

declare -A running_test=()
# reap_one: waits until a test process ends, then collects EVERY tracked one
# that has ended (stop_session can take seconds, in which time others end, and
# a later `wait -n` would then return at once with nothing to report), once
# each. A pass that collects nothing means the bookkeeping is wrong; after a
# few of those it fails loudly instead of spinning.
reap_idle=0
reap_one() {
    local pid collected=0
    wait -n 2>/dev/null || true
    for pid in "${!running_test[@]}"; do
        kill -0 "$pid" 2>/dev/null && continue
        stop_session "$pid"
        unset 'running_test[$pid]'
        collected=1
    done
    if [ "$collected" = 1 ]; then
        reap_idle=0
        return 0
    fi
    reap_idle=$((reap_idle + 1))
    if [ "$reap_idle" -ge 50 ]; then
        echo "run.sh: lost track of ${#running_test[@]} running test(s): ${running_test[*]}" >&2
        stop_all 1
    fi
    sleep 0.1
}

# On INT or TERM, stop every running test's session, then end.
stop_all() {
    local pid
    for pid in "${!running_test[@]}"; do stop_session "$pid"; done
    exit "${1:-130}"
}
trap stop_all INT TERM

# run_pool <jobs> <test>...: runs the tests, at most <jobs> at once.
run_pool() {
    local pool_jobs=$1 name pid
    shift
    # Monitor mode: a job gets its own process group (its pid) and normal INT
    # handling; an async job of a plain script has INT ignored, which the
    # tests that send themselves an INT would inherit.
    set -m
    for name in "$@"; do
        while [ "${#running_test[@]}" -ge "$pool_jobs" ]; do reap_one; done
        "$BASH" "$script" --one "$name" </dev/null >/dev/null 2>&1 &
        pid=$!
        running_test[$pid]=$name
    done
    while [ "${#running_test[@]}" -gt 0 ]; do reap_one; done
    set +m
}

parallel_tests=()
tail_tests=()
for test_name in $selected; do
    case " ${serial_tests[*]} " in
        *" $test_name "*) tail_tests+=("$test_name") ;;
        *) parallel_tests+=("$test_name") ;;
    esac
done
[ "${#parallel_tests[@]}" = 0 ] || run_pool "$jobs" "${parallel_tests[@]}"
[ "${#tail_tests[@]}" = 0 ] || run_pool 1 "${tail_tests[@]}"

passed=0
failed=0
for test_name in $selected; do
    if [ -s "$results/$test_name.res" ]; then
        read -r status ms <"$results/$test_name.res"
    else
        status=1 ms=0
        echo "no result was written (the test process died)" >"$results/$test_name.out"
    fi
    if [ "$status" = 0 ]; then
        passed=$((passed + 1))
        echo "ok   $test_name ($ms ms)"
    else
        failed=$((failed + 1))
        echo "FAIL $test_name"
        cat "$results/$test_name.out"
    fi
done
echo "$passed passed, $failed failed (jobs: $jobs)"
[ "$failed" = 0 ]
