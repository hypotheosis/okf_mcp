#!/usr/bin/env bash
# Starts an okf-mcp server for the Telekin agent eval (evals/telekin/README.md)
# and seeds it: a user, the eval-sandbox catalog imported from
# evals/telekin/catalog, and a write key reaching only that catalog.
#
#   DATABASE_URL=postgres://... evals/telekin/setup.sh KEY_FILE
#
# KEY_FILE receives the key (mode 600); it is never printed. The server keeps
# running in the background on $OKF_EVAL_LISTEN (default 127.0.0.1:8080), its
# log in $OKF_EVAL_LOG and its pid in $OKF_EVAL_PID_FILE. The binary is
# $OKF_MCP_BIN (default target/debug/okf-mcp).
#
# The key has no default catalog, like an OAuth connection made through a
# client such as MCPJam: tools called without `catalog` meet the server's
# "no catalog selected" path (issue #663) exactly as the swarm runs did.
set -euo pipefail

readonly HERE=$(cd "$(dirname "$0")" && pwd)
readonly BIN=${OKF_MCP_BIN:-target/debug/okf-mcp}
readonly LISTEN=${OKF_EVAL_LISTEN:-127.0.0.1:8080}
readonly LOG=${OKF_EVAL_LOG:-okf-eval-server.log}
readonly PID_FILE=${OKF_EVAL_PID_FILE:-okf-eval-server.pid}
readonly CATALOG=eval-sandbox
readonly OWNER=eval-owner
readonly READY_TIMEOUT_SECONDS=60

if [[ $# -ne 1 ]]; then
    echo "usage: DATABASE_URL=... $0 KEY_FILE" >&2
    exit 2
fi
readonly KEY_FILE=$1
: "${DATABASE_URL:?DATABASE_URL must name the eval database}"

"$BIN" migrate
"$BIN" user create "$OWNER" --display-name "Eval Owner"
"$BIN" catalog create "$CATALOG" --description "Eval sandbox" --owner "$OWNER"
"$BIN" import "$CATALOG" "$HERE/catalog" --actor seed

# `key create` prints the key alone on stdout, once. umask keeps the file
# private from the moment it exists.
(umask 077 && "$BIN" key create mcpjam-eval --access write --catalogs "$CATALOG" \
    --actor mcpjam-eval --user "$OWNER" >"$KEY_FILE")

"$BIN" serve --listen "$LISTEN" >"$LOG" 2>&1 &
echo $! >"$PID_FILE"

for _ in $(seq "$READY_TIMEOUT_SECONDS"); do
    if curl -sf "http://$LISTEN/healthz" >/dev/null; then
        echo "okf-mcp is ready on http://$LISTEN/mcp"
        exit 0
    fi
    if ! kill -0 "$(cat "$PID_FILE")" 2>/dev/null; then
        echo "okf-mcp exited before it was ready; its log:" >&2
        cat "$LOG" >&2
        exit 1
    fi
    sleep 1
done
echo "okf-mcp was not ready within ${READY_TIMEOUT_SECONDS}s; its log:" >&2
cat "$LOG" >&2
exit 1
