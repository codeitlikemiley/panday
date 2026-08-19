//! The checked-in Grafana board is tested against the code (M21.2).
//!
//! docs/21 asks for "Grafana dashboards checked into `deploy/`". A checked-in
//! board rots in a specific, silent way: a metric gets renamed, the panel keeps
//! rendering, and it shows "No data" — which looks exactly like a healthy system
//! with no traffic. So every metric a panel queries must be a metric the code
//! can actually emit.

use std::collections::BTreeSet;

const DASHBOARD: &str = include_str!("../../../deploy/grafana-panday.json");

/// Metric names referenced by any panel query.
fn referenced() -> BTreeSet<String> {
    let board: serde_json::Value = serde_json::from_str(DASHBOARD).expect("dashboard is JSON");
    let mut out = BTreeSet::new();
    for panel in board["panels"].as_array().expect("panels") {
        for target in panel["targets"].as_array().expect("targets") {
            let expr = target["expr"].as_str().expect("expr");
            // PromQL identifiers: our metrics all start `panday_`, and nothing
            // else in an expression does.
            let mut rest = expr;
            while let Some(at) = rest.find("panday_") {
                rest = &rest[at..];
                let end = rest
                    .find(|c: char| !c.is_ascii_alphanumeric() && c != '_')
                    .unwrap_or(rest.len());
                out.insert(rest[..end].to_string());
                rest = &rest[end..];
            }
        }
    }
    out
}

/// Histogram families expose `_bucket`/`_sum`/`_count` series derived from the
/// family name; a query legitimately names those.
fn base_name(series: &str) -> &str {
    for suffix in ["_bucket", "_sum", "_count"] {
        if let Some(stripped) = series.strip_suffix(suffix) {
            return stripped;
        }
    }
    series
}

#[test]
fn every_metric_the_dashboard_queries_exists_in_the_code() {
    let known: BTreeSet<&str> = panday_sdk::metrics::metrics()
        .names()
        .into_iter()
        // Rendered by `render()` itself rather than being a family.
        .chain(["panday_metrics_series_dropped_total"])
        .collect();

    let missing: Vec<String> = referenced()
        .into_iter()
        .filter(|s| !known.contains(base_name(s)))
        .collect();

    assert!(
        missing.is_empty(),
        "the dashboard queries metrics that no longer exist: {missing:?}\nknown: {known:?}"
    );
}

#[test]
fn the_dashboard_is_not_empty_and_names_its_panels() {
    let board: serde_json::Value = serde_json::from_str(DASHBOARD).unwrap();
    let panels = board["panels"].as_array().unwrap();
    assert!(panels.len() >= 8, "{} panels", panels.len());
    for p in panels {
        assert!(p["title"].as_str().is_some_and(|t| !t.is_empty()));
        // Every panel says what it means. A cost board read by someone who did
        // not build it is the point of the board.
        assert!(
            p["description"].as_str().is_some_and(|d| d.len() > 30),
            "panel {:?} has no description",
            p["title"]
        );
    }
    assert_eq!(board["uid"].as_str(), Some("panday-cost-cache"));
}

#[test]
fn the_two_headline_rows_of_the_docs_table_are_on_the_board() {
    // docs/21 M21.2: "first Grafana board (cost + cache ratio)".
    let refs = referenced();
    assert!(
        refs.iter().any(|r| r.starts_with("panday_cost_usd")),
        "{refs:?}"
    );
    assert!(
        refs.iter()
            .any(|r| r.starts_with("panday_cache_read_ratio")),
        "{refs:?}"
    );
}
