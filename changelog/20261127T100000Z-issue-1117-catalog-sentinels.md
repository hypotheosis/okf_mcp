# Issue #1117: telemetry catalog sentinels can't be catalog names

- The MCP tool span's `okf.catalog` attribute and the tool metrics' `catalog` label now report `(none)` and `(multiple)` instead of `none` and `multiple`. `[a-z0-9-]+` (`is_valid_name`) can never produce a parenthesis, so no catalog's traffic can be mistaken for the sentinels. No catalog-name rule changed.
- Defined once as `CATALOG_LABEL_NONE` and `CATALOG_LABEL_MULTIPLE` in `src/store/catalogs.rs`; `CatalogSeen::label` uses them.
- **Breaking for dashboards and alerts:** anything matching `okf.catalog:none` or `catalog="none"` must change to `(none)`, and `multiple` to `(multiple)`. `docs/operations.md` ("Telemetry") says so. The example Grafana dashboard filters with `catalog=~"$catalog"` (`.*`), so it needs no change.
- Tests: the sentinels fail `is_valid_name`; a no-catalog call and a two-catalog call (`list_catalogs`) record `(none)` and `(multiple)` on the span and the metric label (existing tests updated).
