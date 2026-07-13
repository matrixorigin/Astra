use astra_turn_core::pipeline_metrics::MetricsRegistry;

pub(crate) const METRIC_APPROVAL_INTERACTION_LOOKUP_TOTAL: &str =
    "astra_interaction_approval_lookup_total";
pub(crate) const METRIC_APPROVAL_INTERACTION_RESOLUTION_TOTAL: &str =
    "astra_interaction_approval_resolution_total";

pub(crate) fn register_interaction_metrics(registry: &MetricsRegistry) {
    registry.register_counter(
        METRIC_APPROVAL_INTERACTION_LOOKUP_TOTAL,
        "Durable approval interaction lookups by event type and low-cardinality outcome.",
    );
    registry.register_counter(
        METRIC_APPROVAL_INTERACTION_RESOLUTION_TOTAL,
        "Durable approval interaction resolutions by low-cardinality outcome.",
    );
    astra_turn_core::ws_user_prompt_gate::register_ws_user_prompt_metrics(registry);
}

pub(crate) fn record_approval_interaction_lookup(
    registry: &MetricsRegistry,
    event: &'static str,
    outcome: &'static str,
) {
    register_interaction_metrics(registry);
    registry.increment_counter(
        METRIC_APPROVAL_INTERACTION_LOOKUP_TOTAL,
        &[("event", event), ("outcome", outcome)],
        1,
    );
}

pub(crate) fn record_approval_interaction_resolution(
    registry: &MetricsRegistry,
    outcome: &'static str,
) {
    register_interaction_metrics(registry);
    registry.increment_counter(
        METRIC_APPROVAL_INTERACTION_RESOLUTION_TOTAL,
        &[("outcome", outcome)],
        1,
    );
}
