#!/usr/bin/env bash
# One Redis and one one-node Valkey cluster for a whole test run (issue
# #1045), the way scripts/test-pg.sh gives a run one Postgres. Tests use them
# through TEST_REDIS_URL and TEST_REDIS_CLUSTER_URL (tests/support/redis.rs),
# each under a key prefix of its own, instead of starting a container per test.
# A test that needs a server to itself (an outage, a change to the default
# user, killing every pub/sub connection) still starts its own container.
#
#   scripts/test-redis.sh up NAME [OWNER_PID]
#                                  start container NAME (Redis) and
#                                  NAME-cluster (Valkey); print
#                                  `TEST_REDIS_URL=...` and
#                                  `TEST_REDIS_CLUSTER_URL=...` lines, ready
#                                  for `eval`. With OWNER_PID the containers
#                                  are labelled as owned by that process, so
#                                  the stale-container sweep
#                                  (tests/support/pg.rs) removes them once it
#                                  is gone; without it, nothing sweeps them
#   scripts/test-redis.sh down NAME
#                                  remove both containers (fine if gone)
#   scripts/test-redis.sh run CMD...
#                                  run CMD with both variables set: reuse a
#                                  server already named by its variable, else
#                                  start it and remove it when CMD exits,
#                                  even on failure
#
# To keep the servers across runs:
#   eval "$(scripts/test-redis.sh up okf-mcp-test-redis)"
#   export TEST_REDIS_URL TEST_REDIS_CLUSTER_URL
#
# Their data lives in memory only (no snapshots, no append-only file): a test
# server's data is worth nothing after the run.
set -euo pipefail

# redis:7.4-alpine, not testcontainers-modules' default tag (5.0 lacks GETDEL),
# and the Valkey image tests/support/redis.rs starts per test.
readonly REDIS_IMAGE=redis:7.4-alpine
readonly VALKEY_IMAGE=valkey/valkey:8.1-alpine
readonly READY_TIMEOUT_SECONDS=60
# shellcheck source=scripts/test-server-lib.sh
source "$(dirname "${BASH_SOURCE[0]}")/test-server-lib.sh"

usage() {
    echo "usage: $0 up NAME [OWNER_PID] | down NAME | run CMD..." >&2
    exit 2
}

# Waits until `CLI ping` in container $1 (CLI is redis-cli or valkey-cli) answers PONG.
wait_ready() {
    local name=$1 cli=$2 waited=0
    until [[ $(docker exec "$name" "$cli" ping 2>/dev/null) == PONG ]]; do
        if ((waited >= READY_TIMEOUT_SECONDS)); then
            echo "$cli in $name was not ready after ${READY_TIMEOUT_SECONDS}s" >&2
            docker logs "$name" >&2 || true
            return 1
        fi
        sleep 1
        waited=$((waited + 1))
    done
}

# Prints the host port container $1 publishes for 6379.
published_port() {
    docker port "$1" 6379/tcp | head -n 1 | sed 's/.*://'
}

up() {
    local name=$1 owner=${2:-}
    local -a labels
    mapfile -t labels < <(owner_labels "$name" "$owner")
    local cluster="$name-cluster"

    docker run --detach --name "$name" "${labels[@]}" \
        --publish 127.0.0.1::6379 \
        "$REDIS_IMAGE" redis-server --save "" --appendonly no >/dev/null
    docker run --detach --name "$cluster" "${labels[@]}" \
        --publish 127.0.0.1::6379 \
        "$VALKEY_IMAGE" valkey-server --cluster-enabled yes \
        --cluster-config-file nodes.conf --appendonly no --save "" >/dev/null
    wait_ready "$name" redis-cli
    wait_ready "$cluster" valkey-cli

    # A cluster client on the host follows the topology back to the address
    # the node announces, so it must be the published host port (the same
    # steps as tests/support/redis.rs's form_cluster).
    local port
    port=$(published_port "$cluster")
    docker exec "$cluster" valkey-cli config set cluster-announce-ip 127.0.0.1 >/dev/null
    docker exec "$cluster" valkey-cli config set cluster-announce-port "$port" >/dev/null
    docker exec "$cluster" valkey-cli cluster addslotsrange 0 16383 >/dev/null
    local waited=0
    until docker exec "$cluster" valkey-cli cluster info | grep -q cluster_state:ok; do
        if ((waited >= READY_TIMEOUT_SECONDS)); then
            echo "the cluster in $cluster did not form after ${READY_TIMEOUT_SECONDS}s" >&2
            docker logs "$cluster" >&2 || true
            return 1
        fi
        sleep 1
        waited=$((waited + 1))
    done

    echo "TEST_REDIS_URL=redis://127.0.0.1:$(published_port "$name")/0"
    echo "TEST_REDIS_CLUSTER_URL=redis://127.0.0.1:$port/0"
}

# Removes container $1 (fine if it is gone).
remove() {
    if docker container inspect "$1" >/dev/null 2>&1; then
        docker rm --force --volumes "$1" >/dev/null
    fi
}

down() {
    remove "$1"
    remove "$1-cluster"
}

run() {
    if [[ -n ${TEST_REDIS_URL:-} && -n ${TEST_REDIS_CLUSTER_URL:-} ]]; then
        exec "$@"
    fi
    local name="okf-mcp-test-redis-$$"
    # Set before `up`, so servers that fail to become ready are removed too.
    trap "down '$name'" EXIT
    # Both servers are started; a variable that is already set wins.
    local started
    started=$(up "$name" "$$")
    local line
    while IFS= read -r line; do
        local variable=${line%%=*}
        if [[ -z ${!variable:-} ]]; then
            export "$line"
        fi
    done <<<"$started"
    # As in test-pg.sh: the command runs in the background so a signal sent to
    # this script alone is handled at once, and `stop` ends it with TERM.
    child=
    trap 'stop 130' INT
    trap 'stop 143' TERM
    "$@" &
    child=$!
    wait "$child"
}

stop() {
    if [[ -n $child ]]; then
        kill -TERM "$child" 2>/dev/null || true
        wait "$child" 2>/dev/null || true
    fi
    exit "$1"
}

(($# >= 1)) || usage
command=$1
shift
case $command in
up)
    (($# == 1 || $# == 2)) || usage
    up "$@"
    ;;
down)
    (($# == 1)) || usage
    down "$1"
    ;;
run)
    (($# >= 1)) || usage
    run "$@"
    ;;
*) usage ;;
esac
