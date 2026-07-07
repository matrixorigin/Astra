use astra_turn_core::pipeline_metrics::MetricsRegistry;

pub(crate) const METRIC_APPROVAL_JOURNAL_LOOKUP_TOTAL: &str =
    "astra_interaction_approval_journal_lookup_total";
pub(crate) const METRIC_APPROVAL_JOURNAL_WRITE_TOTAL: &str =
    "astra_interaction_approval_journal_write_total";
pub(crate) const METRIC_APPROVAL_LEDGER_INSERT_TOTAL: &str =
    "astra_interaction_approval_ledger_insert_total";
pub(crate) const METRIC_ASK_USER_JOURNAL_WRITE_TOTAL: &str =
    "astra_interaction_ask_user_journal_write_total";
pub(crate) const METRIC_ASK_USER_LEDGER_INSERT_TOTAL: &str =
    "astra_interaction_ask_user_ledger_insert_total";

pub(crate) fn register_interaction_metrics(registry: &MetricsRegistry) {
    registry.register_counter(
        METRIC_APPROVAL_JOURNAL_LOOKUP_TOTAL,
        "Approval durable journal lookups by event type and low-cardinality outcome.",
    );
    registry.register_counter(
        METRIC_APPROVAL_JOURNAL_WRITE_TOTAL,
        "Approval durable journal writes by low-cardinality outcome.",
    );
    registry.register_counter(
        METRIC_APPROVAL_LEDGER_INSERT_TOTAL,
        "Approval callback ledger inserts by low-cardinality outcome.",
    );
    registry.register_counter(
        METRIC_ASK_USER_JOURNAL_WRITE_TOTAL,
        "ask_user durable journal writes by event type and low-cardinality outcome.",
    );
    registry.register_counter(
        METRIC_ASK_USER_LEDGER_INSERT_TOTAL,
        "ask_user callback ledger inserts by low-cardinality outcome.",
    );
    astra_turn_core::ws_user_prompt_gate::register_ws_user_prompt_metrics(registry);
}

pub(crate) fn record_approval_journal_lookup(
    registry: &MetricsRegistry,
    event: &'static str,
    outcome: &'static str,
) {
    register_interaction_metrics(registry);
    registry.increment_counter(
        METRIC_APPROVAL_JOURNAL_LOOKUP_TOTAL,
        &[("event", event), ("outcome", outcome)],
        1,
    );
}

pub(crate) fn record_approval_journal_write(registry: &MetricsRegistry, outcome: &'static str) {
    register_interaction_metrics(registry);
    registry.increment_counter(
        METRIC_APPROVAL_JOURNAL_WRITE_TOTAL,
        &[("outcome", outcome)],
        1,
    );
}

pub(crate) fn record_approval_ledger_insert(registry: &MetricsRegistry, outcome: &'static str) {
    register_interaction_metrics(registry);
    registry.increment_counter(
        METRIC_APPROVAL_LEDGER_INSERT_TOTAL,
        &[("outcome", outcome)],
        1,
    );
}

pub(crate) fn record_ask_user_journal_write(
    registry: &MetricsRegistry,
    event: &'static str,
    outcome: &'static str,
) {
    register_interaction_metrics(registry);
    registry.increment_counter(
        METRIC_ASK_USER_JOURNAL_WRITE_TOTAL,
        &[("event", event), ("outcome", outcome)],
        1,
    );
}

pub(crate) fn record_ask_user_ledger_insert(registry: &MetricsRegistry, outcome: &'static str) {
    register_interaction_metrics(registry);
    registry.increment_counter(
        METRIC_ASK_USER_LEDGER_INSERT_TOTAL,
        &[("outcome", outcome)],
        1,
    );
}
