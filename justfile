# Common local validation commands. Keep these recipes as the stable interface
# used by contributors and issue-to-PR automation.

# apalis-postgres' query macros check a live database whenever DATABASE_URL
# is set at build time (the CLI reads that variable too). Build from its
# shipped offline cache instead.
export SQLX_OFFLINE := "true"

fmt:
    cargo fmt --all --check

lint:
    cargo clippy --all-targets -- -D warnings
    RUSTDOCFLAGS="-D warnings" cargo doc --no-deps

# One Postgres server for the whole run (scripts/test-pg.sh); set
# TEST_PG_URL to reuse one already running.
test:
    scripts/test-pg.sh run cargo test

# Browser end-to-end tests of the web console (needs Docker and Chromium;
# RUSTWRIGHT_CHROMIUM names the binary if it is not on PATH).
test-ui:
    scripts/test-pg.sh run cargo test --test ui_e2e -- --test-threads=2

# The dylint policy lints (docs/contributing/policy-lints.md). Needs
# `cargo install --locked cargo-dylint dylint-link`; the first run builds the
# lint library on its own pinned nightly (minutes). `--all-targets` compiles
# test code too, so it is excluded properly. The lint is at deny (`-D`);
# changing the level needs a touch of the crate roots, see the policy doc.
dylint:
    DYLINT_RUSTFLAGS="-D uninstrumented_fn" cargo dylint --all -- --all-targets

# Deterministic replays of the MCPJam swarm goals, no LLM (tests/mcpjam/README.md).
# A local server on a throwaway Postgres by default; set OKF_MCP_URL and
# OKF_MCP_KEY to run against a remote one, such as staging. An admin key with
# --all gives each scenario a temporary catalog; with a plain write key, also
# set OKF_MCP_CATALOG to a scratch catalog they share.
test-mcpjam:
    #!/usr/bin/env bash
    set -euo pipefail
    (cd tests/mcpjam && npm ci --ignore-scripts --no-audit --no-fund)
    if [ -n "${OKF_MCP_URL:-}" ]; then
        cd tests/mcpjam && npx vitest run
    else
        cargo build --bin okf-mcp
        scripts/test-pg.sh run bash -c 'cd tests/mcpjam && npx vitest run'
    fi

# Live MCPJam swarm, okf-mcp side: one seeded catalog and one catalog-scoped
# write key per session, verified, then torn down (tests/mcpjam/README.md,
# "Live swarm"). Needs OKF_MCP_URL and OKF_MCP_KEY (an admin key reaching every
# catalog); `--dry-run` needs neither. Usage: just mcpjam-swarm <goalId> [--sessions N] [--keep] [--dry-run]
mcpjam-swarm *args:
    (cd tests/mcpjam && npm ci --ignore-scripts --no-audit --no-fund)
    cd tests/mcpjam && npx tsx src/swarm-cli.ts {{args}}

# Shell tests for tools/agentic (stub gh, fake clock; no Docker or network).
test-agentic:
    timeout 120 tools/agentic/tests/run.sh

# Prepare locally; ship through a PR, then publish the merged commit's tag.
release-prepare patchlevel:
    bash scripts/release-prepare.sh {{quote(patchlevel)}}

release-ship:
    bash scripts/release-ship.sh
