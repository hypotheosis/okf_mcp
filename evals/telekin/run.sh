#!/usr/bin/env bash
# Runs the Telekin agent eval (evals/telekin/README.md) against a server that
# evals/telekin/setup.sh started.
#
#   OKF_EVAL_KEY_FILE=... evals/telekin/run.sh [--case ID ...]
#
# Inference: ANTHROPIC_API_KEY for your own key, or MCPJAM_API_KEY (an sk_
# key) for MCPJam-hosted inference; `--inference auto` picks whichever is set.
# Reports land in $OKF_EVAL_REPORT_DIR (default eval-report/). Exit codes are
# mcpjam's: 0 passed, 1 a case failed, 2 bad file or flags, 3 credentials,
# 4 setup, 5 inconclusive.
set -euo pipefail

readonly HERE=$(cd "$(dirname "$0")" && pwd)
# The CLI version is pinned: the suite file's schema follows it.
readonly MCPJAM_CLI=@mcpjam/cli@6.3.2
readonly REPORT_DIR=${OKF_EVAL_REPORT_DIR:-eval-report}
: "${OKF_EVAL_KEY_FILE:?OKF_EVAL_KEY_FILE must name the key file setup.sh wrote}"

# Read by mcp.json's ${VAR} substitution, so the key never sits on a command
# line or in the file.
OKF_EVAL_KEY=$(<"$OKF_EVAL_KEY_FILE")
export OKF_EVAL_KEY
export OKF_EVAL_URL=${OKF_EVAL_URL:-http://127.0.0.1:8080/mcp}

mkdir -p "$REPORT_DIR"
# Publish flows take a dozen tool calls, more than the CLI's default of 10
# model steps per prompt.
npx -y "$MCPJAM_CLI" --no-telemetry test "$HERE/suite.yaml" \
    --mcp-config "$HERE/mcp.json" \
    --inference auto \
    --max-steps 30 \
    --iteration-timeout 300000 \
    --reporter junit-xml --out "$REPORT_DIR/junit.xml" \
    "$@"
