#!/usr/bin/env bash
# remove-worktree.sh <worktree> [--branch] [--force-branch] [--base <ref>]
#
# Removes a worktree after its PR has merged, and frees its Cargo build output,
# which `git worktree remove` leaves behind: with `build-dir` set outside the
# checkout, each worktree builds into its own hashed directory, and a worktree
# can leave tens of GiB there. Refuses (exit 1, nothing changed) unless:
#   1. the path is a registered worktree of this repository, not the main
#      checkout;
#   2. no process has its working directory inside it (a clean would break a
#      running build or test; the same /proc check watchdog.sh uses);
#   3. with --branch, the worktree's branch is merged into the base: --base
#      <ref> (for example origin/milestone/<epic>-<slug>, for a PR merged into a
#      milestone branch), else $AGENTIC_BASE_REF, else origin/main; unless
#      --force-branch is also given.
# Then, in order: `cargo clean` in the worktree when it has a Cargo.toml,
# bounded by $AGENTIC_CLEAN_TIMEOUT (default 600 s); a failed or timed-out
# clean stops here and keeps the worktree. Then `git worktree remove` (never
# forced, so uncommitted work keeps the worktree), then with --branch or
# --force-branch `git branch -D`. Prints one REMOVED line.
# Every git and cargo call is bounded.
set -euo pipefail
here=$(cd "$(dirname "$0")" && pwd)
# shellcheck source-path=SCRIPTDIR source=lib.sh
source "$here/lib.sh"

usage="usage: remove-worktree.sh <worktree> [--branch] [--force-branch] [--base <ref>]"
target=""
delete_branch=0
force_branch=0
base=${AGENTIC_BASE_REF:-origin/main}
while [ $# -gt 0 ]; do
    case "$1" in
        --branch) delete_branch=1 ;;
        --force-branch) delete_branch=1 force_branch=1 ;;
        --base)
            [ $# -ge 2 ] && [ -n "$2" ] || { echo "$usage" >&2; exit 2; }
            base=$2; shift ;;
        -*) echo "$usage" >&2; exit 2 ;;
        *) [ -z "$target" ] || { echo "$usage" >&2; exit 2; }; target=$1 ;;
    esac
    shift
done
[ -n "$target" ] || { echo "$usage" >&2; exit 2; }

refuse() { echo "REFUSE: $*"; exit 1; }

main=$(agentic_main_checkout) || refuse "could not find the main checkout"
[ -d "$target" ] || refuse "$target is not a directory"
wt=$(cd "$target" && pwd -P)
[ "$wt" != "$main" ] || refuse "$wt is the main checkout"

# The registered worktrees: "worktree <path>", then "branch refs/heads/<name>"
# (absent when detached), one blank-line-separated record each.
listing=$(agentic_git -C "$main" worktree list --porcelain) \
    || refuse "could not list the worktrees"
registered=0
branch=""
while read -r key value; do
    case "$key" in
        worktree)
            [ "$registered" = 0 ] || break # the next record: ours is complete
            [ "$value" != "$wt" ] || registered=1
            ;;
        branch) [ "$registered" = 1 ] && branch=${value#refs/heads/} ;;
    esac
done <<<"$listing"
[ "$registered" = 1 ] || refuse "$wt is not a registered worktree of this repository"

busy=$(agentic_procs_inside "$wt")
[ -z "$busy" ] || refuse "processes are running inside $wt (pid, cwd, name):"$'\n'"$busy"

if [ "$delete_branch" = 1 ]; then
    [ -n "$branch" ] || refuse "$wt has no branch (detached HEAD) to delete"
    if [ "$force_branch" = 0 ]; then
        merged=0
        agentic_git -C "$main" merge-base --is-ancestor "refs/heads/$branch" "$base" && merged=1
        [ "$merged" = 1 ] \
            || refuse "branch $branch is not merged into $base (use --force-branch to delete it anyway)"
    fi
fi

# clean_build_output_steps: the bounded `cargo clean` of the worktree, between
# agentic_cap_begin and agentic_cap_finish, so its log follows lib.sh's
# "Signals" invariant (#1193): an INT or TERM during the clean (which
# agentic_run_bounded passes on to the capture's recording traps) leaves no
# log behind, and nothing runs or prints after it. Sets cleaned. Returns 1,
# with a REFUSE line printed, when the clean failed or timed out.
clean_build_output_steps() {
    local log clean_status=0 why summary
    agentic_cap_mktemp log "${TMPDIR:-/tmp}/remove-worktree.XXXXXX" || {
        agentic_cap_live && echo "REFUSE: could not create a temp file for the cargo clean output"
        return 1
    }
    agentic_cap_live || return
    # cargo runs inside the worktree so its config and build dir resolve there.
    # shellcheck disable=SC2016 # $1 is for the inner bash, not this shell
    agentic_run_bounded "${AGENTIC_CLEAN_TIMEOUT:-600}" \
        bash -c 'cd "$1" && exec cargo clean' _ "$wt" >"$log" 2>&1 || clean_status=$?
    agentic_cap_live || return
    if [ "$clean_status" != 0 ]; then
        why="failed (exit $clean_status)"
        [ "$agentic_timed_out" = 1 ] && why="timed out after ${AGENTIC_CLEAN_TIMEOUT:-600} s"
        agentic_cap_live || return
        echo "REFUSE: cargo clean $why in $wt; the worktree is kept. Output:"
        agentic_cap_live || return
        tail -n 20 "$log"
        return 1
    fi
    agentic_cap_live || return
    summary=$(sed -n 's/^.*Removed \(.*\)$/\1/p' "$log" | tail -n 1)
    cleaned="cargo clean: removed ${summary:-nothing reported}"
}

cleaned="no Cargo.toml, cargo clean skipped"
if [ -f "$wt/Cargo.toml" ]; then
    clean_status=0
    agentic_cap_begin
    clean_build_output_steps || clean_status=$?
    # A recorded signal is re-raised here, after the log is removed.
    agentic_cap_finish "$clean_status" || exit 1
fi

agentic_git -C "$main" worktree remove "$wt" \
    || refuse "git could not remove $wt (uncommitted changes? the build output is already cleaned)"
extra=""
if [ "$delete_branch" = 1 ]; then
    agentic_git -C "$main" branch -D "$branch" >/dev/null \
        || refuse "removed $wt, but could not delete branch $branch"
    extra="; deleted branch $branch"
fi
echo "REMOVED $wt ($cleaned)$extra"
