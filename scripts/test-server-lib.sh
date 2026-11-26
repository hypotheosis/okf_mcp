# Shared by scripts/test-pg.sh and scripts/test-redis.sh (source it; it starts
# nothing). Labels a test-server container so the stale-container sweep
# (tests/support/pg.rs) knows who owns it.

# The labels every test container carries (tests/support/pg.rs START_LABEL and
# OWNER_SCOPE_LABEL), so `docker ps --filter label=okf-mcp.test-start` counts
# these too.
readonly LABEL=okf-mcp.test-start
readonly SCOPE_LABEL=okf-mcp.test-owner-scope

# Prints the `--label` arguments, one per line, that mark a container as owned
# by process $2, in the form the sweep reads (tests/support/pg.rs): the start
# label `<pid>-<counter>` and the owner scope `<boot id>/<pid namespace>`.
# Without an owner, or where the scope cannot be read, the start label is
# `server-<name>` and no scope is set, so the sweep never removes it.
owner_labels() {
    local name=$1 owner=${2:-}
    local boot_id namespace
    if [[ -n $owner ]] &&
        boot_id=$(cat /proc/sys/kernel/random/boot_id 2>/dev/null) &&
        namespace=$(readlink /proc/self/ns/pid 2>/dev/null); then
        echo "--label"
        echo "$LABEL=$owner-0"
        echo "--label"
        echo "$SCOPE_LABEL=$boot_id/$namespace"
    else
        echo "--label"
        echo "$LABEL=server-$name"
    fi
}
