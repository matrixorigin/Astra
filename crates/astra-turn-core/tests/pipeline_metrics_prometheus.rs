//! TDD tests for Phase 13 Prometheus metrics registry.
//!
//! The registry must:
//! * Expose counters and gauges with optional labels
//! * Render in Prometheus text exposition format (0.0.4)
//! * Be cheap to update from hot paths (atomic, no locks on increment)
//! * Produce stable, sorted output so scrapers can diff safely

use astra_turn_core::pipeline_metrics::{MetricKind, MetricsRegistry};

#[test]
fn empty_registry_renders_empty_output() {
    let reg = MetricsRegistry::new();
    assert_eq!(reg.render_prometheus(), "");
}

#[test]
fn counter_increment_is_visible_in_output() {
    let reg = MetricsRegistry::new();
    reg.register_counter(
        "astra_pipeline_turns_total",
        "Total pipeline turns executed",
    );
    reg.increment_counter("astra_pipeline_turns_total", &[], 1);
    reg.increment_counter("astra_pipeline_turns_total", &[], 4);

    let out = reg.render_prometheus();
    assert!(
        out.contains("# HELP astra_pipeline_turns_total Total pipeline turns executed"),
        "missing HELP line:\n{out}"
    );
    assert!(
        out.contains("# TYPE astra_pipeline_turns_total counter"),
        "missing TYPE line:\n{out}"
    );
    assert!(
        out.contains("astra_pipeline_turns_total 5"),
        "missing value line:\n{out}"
    );
}

#[test]
fn gauge_set_and_adjust() {
    let reg = MetricsRegistry::new();
    reg.register_gauge("astra_context_pressure_ratio", "Current pressure ratio");
    reg.set_gauge("astra_context_pressure_ratio", &[], 0.42);
    let out = reg.render_prometheus();
    assert!(out.contains("# TYPE astra_context_pressure_ratio gauge"));
    assert!(
        out.contains("astra_context_pressure_ratio 0.42"),
        "gauge value missing:\n{out}"
    );

    reg.set_gauge("astra_context_pressure_ratio", &[], 0.91);
    let out = reg.render_prometheus();
    assert!(
        out.contains("astra_context_pressure_ratio 0.91"),
        "gauge did not update:\n{out}"
    );
}

#[test]
fn labels_are_sorted_and_escaped() {
    let reg = MetricsRegistry::new();
    reg.register_counter("astra_tool_invocations_total", "Tool invocations");
    reg.increment_counter(
        "astra_tool_invocations_total",
        &[("tool", "bash"), ("outcome", "ok")],
        1,
    );
    reg.increment_counter(
        "astra_tool_invocations_total",
        &[("tool", "bash"), ("outcome", "error")],
        2,
    );

    let out = reg.render_prometheus();
    // labels should appear alphabetically sorted inside {}
    assert!(
        out.contains(r#"astra_tool_invocations_total{outcome="ok",tool="bash"} 1"#),
        "expected ok series missing:\n{out}"
    );
    assert!(
        out.contains(r#"astra_tool_invocations_total{outcome="error",tool="bash"} 2"#),
        "expected error series missing:\n{out}"
    );
}

#[test]
fn label_value_with_special_chars_is_escaped() {
    let reg = MetricsRegistry::new();
    reg.register_counter("astra_alerts_fired_total", "Alerts fired");
    reg.increment_counter(
        "astra_alerts_fired_total",
        &[("rule", "line \"break\"\nbad")],
        1,
    );
    let out = reg.render_prometheus();
    assert!(
        out.contains(r#"rule="line \"break\"\nbad""#),
        "label not escaped per prometheus text format:\n{out}"
    );
}

#[test]
fn unknown_metric_increment_is_silently_ignored() {
    // Incrementing an unregistered metric must not panic —
    // hot paths should never take down the pipeline over a typo.
    let reg = MetricsRegistry::new();
    reg.increment_counter("does_not_exist", &[], 1);
    reg.set_gauge("also_missing", &[], 1.0);
    assert_eq!(reg.render_prometheus(), "");
}

#[test]
fn metric_kind_can_be_queried() {
    let reg = MetricsRegistry::new();
    reg.register_counter("c", "counter help");
    reg.register_gauge("g", "gauge help");
    assert_eq!(reg.kind_of("c"), Some(MetricKind::Counter));
    assert_eq!(reg.kind_of("g"), Some(MetricKind::Gauge));
    assert_eq!(reg.kind_of("missing"), None);
}

#[test]
fn output_is_deterministic_across_renders() {
    let reg = MetricsRegistry::new();
    reg.register_counter("z_metric", "z help");
    reg.register_counter("a_metric", "a help");
    reg.increment_counter("z_metric", &[("k", "v")], 1);
    reg.increment_counter("a_metric", &[("k", "v")], 1);

    let a = reg.render_prometheus();
    let b = reg.render_prometheus();
    assert_eq!(a, b, "rendering must be deterministic");
    // and metric families must be sorted alphabetically
    let a_pos = a.find("a_metric").expect("a_metric missing");
    let z_pos = a.find("z_metric").expect("z_metric missing");
    assert!(a_pos < z_pos, "metric families must be sorted:\n{a}");
}
