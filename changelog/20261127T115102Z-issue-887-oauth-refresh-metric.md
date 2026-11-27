# Issue #887: count refresh results in `okf_oauth_refreshes_total{result}`

## Added
- Counter `okf.oauth.refreshes` (`okf_oauth_refreshes_total{result}`), `result` one of `rotated`, `grace`, `reused`, `unknown`. All four series are pre-registered at zero in `Metrics::new`, so `increase(...)` has a series from the first scrape.
- `RefreshResult` in `src/telemetry/metrics.rs`, and `Metrics::oauth_refresh`.
- A Grafana panel "OAuth refreshes by result" in the Auth row of `examples/grafana/okf-mcp.json` (the dashboard test requires every exported family on it); the panels below it move down one row.
- `docs/operations.md`: the metrics-table row and the example alert `increase(okf_oauth_refreshes_total{result="reused"}[1h]) > 0`.
- Tests in `src/oauth/token.rs`: one per result, and one for the four series before any refresh.

## Changed
- `refresh_label` became `refresh_result`, returning a `RefreshResult`. `refresh` records the span's `okf.refresh` and the counter from that one value, so they cannot drift.

## Notes
- The repository ships no alert rules; the alert is documented in `docs/operations.md` only.
- Other closed-set counters are not pre-registered (they appear on first record); this one is, as the issue asks.
