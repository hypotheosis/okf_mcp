//! The example Grafana dashboard (`examples/grafana/okf-mcp.json`) names only
//! metrics the server exports, so renaming an instrument breaks this test
//! rather than a dashboard nobody opens until an incident.
//!
//! The list of exported families is `Metrics::ALL_FAMILIES`, which a unit
//! test in `src/telemetry/metrics.rs` keeps equal to what the instruments
//! render.

use std::collections::BTreeSet;

use okf_mcp::telemetry::metrics::Metrics;
use regex::Regex;
use serde_json::Value;

/// The dashboard file, read at compile time so the test needs no working
/// directory.
const DASHBOARD: &str = include_str!("../examples/grafana/okf-mcp.json");

/// PromQL words that can stand where a metric name could: grouping and
/// matching keywords and the set operators. (Function names are skipped by
/// being followed by `(`.)
const KEYWORDS: [&str; 11] = [
    "by",
    "without",
    "on",
    "ignoring",
    "group_left",
    "group_right",
    "bool",
    "offset",
    "and",
    "or",
    "unless",
];

/// Every `expr` in the dashboard, panels and nested rows alike.
fn expressions(node: &Value, found: &mut Vec<String>) {
    match node {
        Value::Object(map) => {
            for (key, value) in map {
                match (key.as_str(), value) {
                    ("expr", Value::String(text)) => found.push(text.clone()),
                    _ => expressions(value, found),
                }
            }
        }
        Value::Array(items) => items.iter().for_each(|item| expressions(item, found)),
        _ => {}
    }
}

/// The metric names a PromQL expression selects: its identifiers once label
/// matchers, grouping clauses, strings and durations are removed, minus
/// function names and keywords.
fn metric_names(expression: &str) -> Vec<String> {
    let strings = Regex::new(r#""[^"]*""#).expect("pattern");
    let braces = Regex::new(r"\{[^}]*\}").expect("pattern");
    let grouping = Regex::new(r"\b(?:by|without|on|ignoring|group_left|group_right)\s*\([^)]*\)")
        .expect("pattern");
    let ranges = Regex::new(r"\[[^\]]*\]").expect("pattern");
    let identifier = Regex::new(r"[A-Za-z_:][A-Za-z0-9_:]*(\s*\()?").expect("pattern");

    let cleaned = strings.replace_all(expression, "");
    let cleaned = braces.replace_all(&cleaned, "");
    let cleaned = grouping.replace_all(&cleaned, "");
    let cleaned = ranges.replace_all(&cleaned, "");
    identifier
        .captures_iter(&cleaned)
        .filter(|capture| capture.get(1).is_none())
        .map(|capture| capture[0].to_string())
        .filter(|name| !KEYWORDS.contains(&name.as_str()))
        .collect()
}

/// The family a series belongs to: a histogram's `_bucket`, `_sum` and
/// `_count` series belong to the histogram's family.
fn family_of(series: &str) -> &str {
    if Metrics::ALL_FAMILIES.contains(&series) {
        return series;
    }
    ["_bucket", "_sum", "_count"]
        .iter()
        .find_map(|suffix| series.strip_suffix(suffix))
        .filter(|base| Metrics::ALL_FAMILIES.contains(base))
        .unwrap_or(series)
}

#[test]
fn every_metric_in_the_dashboard_is_one_the_server_exports() {
    let dashboard: Value = serde_json::from_str(DASHBOARD).expect("the dashboard is JSON");
    let mut expressions_found = Vec::new();
    expressions(&dashboard, &mut expressions_found);
    assert!(expressions_found.len() >= 10, "found {expressions_found:?}");

    let mut used = BTreeSet::new();
    for expression in &expressions_found {
        let names = metric_names(expression);
        assert!(!names.is_empty(), "no metric in {expression}");
        for name in names {
            let family = family_of(&name);
            assert!(
                Metrics::ALL_FAMILIES.contains(&family),
                "{name} in `{expression}` is not an exported metric"
            );
            used.insert(family.to_string());
        }
    }
    // The dashboard shows every family the server exports, so a new
    // instrument fails this test until it has a panel.
    let missing: Vec<_> = Metrics::ALL_FAMILIES
        .iter()
        .filter(|family| !used.contains(**family))
        .collect();
    assert!(missing.is_empty(), "not on the dashboard: {missing:?}");
}

/// Every panel in the dashboard, rows' nested panels included.
fn panels(dashboard: &Value) -> Vec<&Value> {
    let top = dashboard["panels"].as_array().expect("panels");
    top.iter()
        .chain(
            top.iter()
                .filter_map(|panel| panel["panels"].as_array())
                .flatten(),
        )
        .collect()
}

/// The histogram families a panel reads through raw `_bucket` series (not
/// through `histogram_quantile`, which reduces them to a line).
fn bucket_families(panel: &Value) -> BTreeSet<String> {
    let mut found = Vec::new();
    expressions(&panel["targets"], &mut found);
    found
        .iter()
        .filter(|expression| !expression.contains("histogram_quantile"))
        .flat_map(|expression| metric_names(expression))
        .filter_map(|name| name.strip_suffix("_bucket").map(str::to_string))
        .collect()
}

#[test]
fn every_histogram_has_a_heatmap_of_its_buckets() {
    let dashboard: Value = serde_json::from_str(DASHBOARD).expect("the dashboard is JSON");
    let all = panels(&dashboard);

    // A raw bucket query belongs only in a heatmap that reads Prometheus's
    // buckets as they are (`format: heatmap`, no re-bucketing).
    for panel in &all {
        if bucket_families(panel).is_empty() {
            continue;
        }
        assert_eq!(panel["type"], "heatmap", "{}", panel["title"]);
        assert_eq!(panel["options"]["calculate"], false, "{}", panel["title"]);
        for target in panel["targets"].as_array().expect("targets") {
            assert_eq!(target["format"], "heatmap", "{}", panel["title"]);
        }
    }

    // Every histogram the server exports (a family the dashboard reads
    // `_bucket` series of anywhere) has a heatmap.
    let mut histograms = Vec::new();
    expressions(&dashboard, &mut histograms);
    let histograms: BTreeSet<String> = histograms
        .iter()
        .flat_map(|expression| metric_names(expression))
        .filter_map(|name| name.strip_suffix("_bucket").map(str::to_string))
        .collect();
    let in_heatmaps: BTreeSet<String> = all
        .iter()
        .flat_map(|panel| bucket_families(panel))
        .collect();
    assert_eq!(histograms, in_heatmaps);
    assert_eq!(in_heatmaps.len(), 8, "{in_heatmaps:?}");

    // And each one also has a quantile view beside its heatmap.
    let mut quantile_expressions = Vec::new();
    expressions(&dashboard, &mut quantile_expressions);
    let with_quantiles: BTreeSet<String> = quantile_expressions
        .iter()
        .filter(|expression| expression.contains("histogram_quantile"))
        .flat_map(|expression| metric_names(expression))
        .filter_map(|name| name.strip_suffix("_bucket").map(str::to_string))
        .collect();
    assert_eq!(with_quantiles, in_heatmaps);
}

/// Whether a selector picks only failures: an `outcome="error"` or a 5xx
/// status matcher. Such series exist only once a failure has happened.
fn selects_failures(expression: &str) -> bool {
    expression.contains(r#"outcome="error""#)
        || expression.contains(r#"http_response_status_code=~"5..""#)
}

#[test]
fn every_error_ratio_reads_zero_when_nothing_failed() {
    // A failure series has no samples until the first failure, so a bare
    // `failures / total` is empty ("No data") on a healthy server. The
    // numerator must fall back to `0 * total` over the same labels, which
    // keeps "no traffic" (an empty denominator) as "No data".
    let dashboard: Value = serde_json::from_str(DASHBOARD).expect("the dashboard is JSON");
    let mut found = Vec::new();
    expressions(&dashboard, &mut found);
    let ratios: Vec<_> = found
        .iter()
        .filter(|expression| selects_failures(expression) && expression.contains(" / "))
        .collect();
    assert!(ratios.len() >= 2, "found {ratios:?}");
    for ratio in ratios {
        let (numerator, _) = ratio.split_once(") / ").expect("a parenthesised numerator");
        assert!(
            numerator.contains(" or on(") && numerator.contains("(0 * "),
            "`{ratio}` is empty, not 0, when nothing has failed"
        );
    }
}

#[test]
fn the_extractor_skips_set_operators() {
    let names = metric_names(
        r#"(sum by (tool) (rate(a_total{outcome="error"}[5m])) or on(tool) (0 * sum by (tool) (rate(a_total[5m])))) / sum by (tool) (rate(a_total[5m]))"#,
    );
    assert_eq!(names, ["a_total", "a_total", "a_total"]);
}

/// Every selector of a metric in `expression`, matchers included
/// (`name{...}`), or the bare name when it has none.
fn selectors(expression: &str) -> Vec<String> {
    let pattern =
        Regex::new(r"\b(?:okf_|http_server_|db_client_)[a-z_]+(?:\{[^}]*\})?").expect("pattern");
    pattern
        .find_iter(expression)
        .map(|found| found.as_str().to_string())
        .collect()
}

#[test]
fn every_selector_is_scoped_by_the_job_variable() {
    // Standard OTel names (`http_server_request_duration_seconds`) are shared
    // by other services, so an unscoped query would merge them in.
    let dashboard: Value = serde_json::from_str(DASHBOARD).expect("the dashboard is JSON");
    let mut found = Vec::new();
    expressions(&dashboard, &mut found);
    for expression in &found {
        let chosen = selectors(expression);
        assert!(!chosen.is_empty(), "no selector in {expression}");
        for selector in chosen {
            assert!(
                selector.contains(r#"job=~"$job""#),
                "{selector} in `{expression}` is not scoped by $job"
            );
        }
    }
    let variables = dashboard["templating"]["list"]
        .as_array()
        .expect("template variables");
    assert!(variables.iter().any(|variable| variable["name"] == "job"));
}

#[test]
fn the_extractor_reads_labelled_selectors_with_variables() {
    let expression = r#"sum by (le) (rate(okf_invalidations_lag_seconds_bucket{job=~"$job",outcome="error"}[$__rate_interval])) / okf_clock_offset_seconds{job=~"$job"}"#;
    assert_eq!(
        metric_names(expression),
        [
            "okf_invalidations_lag_seconds_bucket",
            "okf_clock_offset_seconds"
        ]
    );
    assert_eq!(selectors(expression).len(), 2);
    assert!(selectors("okf_mail_queue_depth")[0].contains("okf_mail_queue_depth"));
}

#[test]
fn the_extractor_reads_promql_the_way_the_dashboard_uses_it() {
    let names = metric_names(
        r#"histogram_quantile(0.95, sum by (le, tool) (rate(okf_mcp_tool_duration_seconds_bucket{outcome="ok"}[5m])))"#,
    );
    assert_eq!(names, ["okf_mcp_tool_duration_seconds_bucket"]);
    assert_eq!(
        family_of("okf_mcp_tool_duration_seconds_bucket"),
        "okf_mcp_tool_duration_seconds"
    );
    assert_eq!(
        family_of("okf_no_such_metric_total"),
        "okf_no_such_metric_total"
    );
    let two = metric_names("sum(a_total) / sum(b_total) > 0.5");
    assert_eq!(two, ["a_total", "b_total"]);
}

#[test]
fn a_renamed_metric_would_fail_the_check() {
    let renamed = DASHBOARD.replace("okf_auth_attempts_total", "okf_sign_ins_total");
    let dashboard: Value = serde_json::from_str(&renamed).expect("JSON");
    let mut found = Vec::new();
    expressions(&dashboard, &mut found);
    let unknown = found
        .iter()
        .flat_map(|expression| metric_names(expression))
        .filter(|name| !Metrics::ALL_FAMILIES.contains(&family_of(name)))
        .collect::<Vec<_>>();
    assert_eq!(unknown, ["okf_sign_ins_total"]);
}

/// Every label name the server's metrics export, plus `job` (added by
/// Prometheus) and `le` (the histogram bucket bound). A dashboard query may
/// match, group or label by these and nothing else. Issue #1157 removed
/// `catalog` from the metrics; a query still using it would show an empty
/// panel, and a variable built from it would list nothing. When a metric gains
/// a label, add it here.
const KNOWN_LABELS: [&str; 24] = [
    "cache",
    "cause",
    "direction",
    "grant",
    "http_request_method",
    "http_response_status_code",
    "http_route",
    "job",
    "kind",
    "le",
    "limiter",
    "method",
    "operation",
    "outcome",
    "provider",
    "reason",
    "result",
    "rule",
    "signal",
    "state",
    "status",
    "task",
    "tool",
    "version",
];

/// The label names a PromQL expression, a `label_values(...)` variable query
/// or a legend format uses: matcher names inside `{...}`, grouping and
/// matching clauses (`by`, `without`, `on`, `ignoring`, `group_left`,
/// `group_right`), the last argument of `label_values`, and `{{name}}`
/// legend placeholders.
fn label_names(text: &str) -> BTreeSet<String> {
    let matcher = Regex::new(r"([A-Za-z_][A-Za-z0-9_]*)\s*(?:=~|!~|!=|=)").expect("pattern");
    let selector = Regex::new(r"\{([^{}]*)\}").expect("pattern");
    let grouping = Regex::new(r"\b(?:by|without|on|ignoring|group_left|group_right)\s*\(([^)]*)\)")
        .expect("pattern");
    let values = Regex::new(r",\s*([A-Za-z_][A-Za-z0-9_]*)\s*\)\s*$").expect("pattern");
    let legend = Regex::new(r"\{\{\s*([A-Za-z_][A-Za-z0-9_]*)\s*\}\}").expect("pattern");
    let strings = Regex::new(r#""[^"]*""#).expect("pattern");

    let mut found = BTreeSet::new();
    for braces in selector.captures_iter(text) {
        // Matcher values are strings that may hold `=` or `,`: drop them.
        let without_strings = strings.replace_all(&braces[1], "\"\"").into_owned();
        found.extend(
            matcher
                .captures_iter(&without_strings)
                .map(|c| c[1].to_string()),
        );
    }
    for clause in grouping.captures_iter(text) {
        found.extend(
            clause[1]
                .split(',')
                .map(str::trim)
                .filter(|label| !label.is_empty())
                .map(str::to_string),
        );
    }
    if text.trim_start().starts_with("label_values(") {
        found.extend(values.captures_iter(text).map(|c| c[1].to_string()));
    }
    found.extend(legend.captures_iter(text).map(|c| c[1].to_string()));
    found
}

/// Every string under the keys the dashboard evaluates or displays:
/// `expr`, `legendFormat` and the variables' `definition`.
fn label_bearing_strings(node: &Value, found: &mut Vec<String>) {
    match node {
        Value::Object(map) => {
            for (key, value) in map {
                match (key.as_str(), value) {
                    ("expr" | "legendFormat" | "definition", Value::String(text)) => {
                        found.push(text.clone());
                    }
                    _ => label_bearing_strings(value, found),
                }
            }
        }
        Value::Array(items) => items
            .iter()
            .for_each(|item| label_bearing_strings(item, found)),
        _ => {}
    }
}

/// The labels in use that the metrics do not have, by query.
fn unknown_labels(dashboard: &Value) -> Vec<(String, String)> {
    let mut strings = Vec::new();
    label_bearing_strings(dashboard, &mut strings);
    strings
        .iter()
        .flat_map(|text| {
            label_names(text)
                .into_iter()
                .filter(|label| !KNOWN_LABELS.contains(&label.as_str()))
                .map(|label| (label, text.clone()))
                .collect::<Vec<_>>()
        })
        .collect()
}

#[test]
fn every_label_in_the_dashboard_is_one_the_metrics_have() {
    let dashboard: Value = serde_json::from_str(DASHBOARD).expect("the dashboard is JSON");
    let unknown = unknown_labels(&dashboard);
    assert!(unknown.is_empty(), "labels the metrics lack: {unknown:?}");
    // The extractor sees labels at all (a vacuous pass would hide a broken one).
    let mut strings = Vec::new();
    label_bearing_strings(&dashboard, &mut strings);
    let seen: BTreeSet<String> = strings.iter().flat_map(|text| label_names(text)).collect();
    assert!(
        seen.contains("outcome") && seen.contains("tool"),
        "{seen:?}"
    );
}

#[test]
fn a_query_on_the_removed_catalog_label_would_fail_the_check() {
    // Every place the old dashboard used the label: a selector, a grouping,
    // a legend and the variable's `label_values`.
    for text in [
        r#"sum(rate(okf_publish_duration_seconds_count{job=~"$job",catalog=~"$catalog"}[5m]))"#,
        "sum by (catalog, outcome) (rate(okf_publish_duration_seconds_count[5m]))",
        "{{catalog}} {{outcome}}",
        r#"label_values(okf_publish_duration_seconds_count{job=~"$job"}, catalog)"#,
    ] {
        assert!(label_names(text).contains("catalog"), "{text}");
    }
    let old = DASHBOARD.replace(
        r#"sum by (tool, outcome) (rate(okf_mcp_tool_duration_seconds_count{job=~\"$job\"}"#,
        r#"sum by (tool, outcome) (rate(okf_mcp_tool_duration_seconds_count{job=~\"$job\",catalog=~\"$catalog\"}"#,
    );
    assert_ne!(old, DASHBOARD, "the probe expression moved");
    let dashboard: Value = serde_json::from_str(&old).expect("JSON");
    let unknown = unknown_labels(&dashboard);
    assert!(
        unknown.iter().any(|(label, _)| label == "catalog"),
        "{unknown:?}"
    );
    assert!(!DASHBOARD.contains("catalog=~"));
}
