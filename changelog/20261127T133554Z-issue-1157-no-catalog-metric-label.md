# Issue #1157: catalog names are no longer metric labels

## Changed
- The `catalog` attribute is removed from the five instruments that carried it: `okf_publish_duration_seconds` (`outcome` remains), `okf_publish_lock_wait_seconds` (no labels), `okf_lint_rejections_total` (`rule` remains), `okf_mcp_tool_duration_seconds` (`tool`, `outcome` remain) and `okf_archive_size_bytes_total` (`direction` remains). `Metrics::publish`, `publish_lock_wait`, `lint_rejection`, `tool_call` and `archive` lose their name parameter; the callers in `src/store/publish.rs`, `src/mcp/mod.rs` and `src/http/archives.rs` follow. The label is removed at the instrument, so `/metrics` and OTLP metrics both lose it.
- `examples/grafana/okf-mcp.json`: the `catalog` variable, every `catalog=~"$catalog"` matcher, the `catalog` grouping and legends are gone; "Publishes by catalog and outcome" is "Publishes by outcome".
- `docs/operations.md` and `README.md` say catalog names are span attributes only, document the label-schema change, and fix the #1117 upgrade note (the `(none)`/`(multiple)` values now apply to the span only).

## Decision
Remove the dimension rather than swap names for row ids. A catalog name is free text that can identify a person or project, and a series per catalog is cardinality the server cannot bound; a row id hides the text but not the series count, and nothing needed per-catalog metrics (per-catalog gauges were already removed). Per-catalog detail stays available in the `okf.catalog` span attribute (unchanged, with the `(none)`/`(multiple)` sentinels from #1117), the audit log and the database. Trace and log disclosure policy is out of scope.

## Added
- `tests/telemetry_privacy.rs`: `no_catalog_name_is_a_metric_attribute` drives an MCP call, a successful and a refused publish (lock wait, lint rejection) and an archive export on two catalogs with synthetic names, then asserts no name or `catalog` key on `/metrics` or in any OTLP metric attribute, and that both catalogs land in the same series (counts 2 and 2, 4 lock waits).
- `tests/grafana_dashboard.rs`: every label a dashboard query matches, groups by, shows in a legend or lists in a variable must be a label the metrics have (`KNOWN_LABELS`); a probe re-adding `catalog` fails.

## Notes
- Already-stored Prometheus samples are not scrubbed; operators delete them or wait out retention (`docs/operations.md`).
- Dashboards, recording rules and alerts that filter or group by `catalog` must change.
- Other metric labels reviewed: `http.route` is a route template, `tool` a closed set (`other` for the rest), `cache`, `rule`, `outcome`, `direction`, `kind` and the rest are fixed words. No other catalog name or user-derived text is a label.
