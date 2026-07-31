//! CLI-owned accounting helpers for full-history work.
//!
//! The helpers in this module deliberately keep measurement behind the
//! process-wide instrumentation gate. Default production execution therefore
//! performs only the history work that the call site already required.

use astra_core::history_work::{
    HistoryWorkSite, QueueBytesReservation, instrumentation_enabled, record_operation,
};
use serde_json::Value;

fn value_payload_bytes(value: &Value) -> u64 {
    match value {
        Value::Null => 0,
        Value::Bool(_) => 1,
        Value::Number(_) => {
            u64::try_from(std::mem::size_of::<serde_json::Number>()).unwrap_or(u64::MAX)
        }
        Value::String(value) => value.len().try_into().unwrap_or(u64::MAX),
        Value::Array(values) => values.iter().fold(0_u64, |bytes, value| {
            bytes.saturating_add(value_payload_bytes(value))
        }),
        Value::Object(values) => values.iter().fold(0_u64, |bytes, (key, value)| {
            bytes
                .saturating_add(key.len().try_into().unwrap_or(u64::MAX))
                .saturating_add(value_payload_bytes(value))
        }),
    }
}

fn json_history_work(messages: &[Value]) -> (u64, u64) {
    (
        messages.iter().fold(0_u64, |bytes, message| {
            bytes.saturating_add(value_payload_bytes(message))
        }),
        messages.len().try_into().unwrap_or(u64::MAX),
    )
}

fn pair_history_work(history: &[(String, String)]) -> (u64, u64) {
    (
        history.iter().fold(0_u64, |bytes, (user, assistant)| {
            bytes
                .saturating_add(user.len().try_into().unwrap_or(u64::MAX))
                .saturating_add(assistant.len().try_into().unwrap_or(u64::MAX))
        }),
        history.len().try_into().unwrap_or(u64::MAX),
    )
}

fn measure_when<T>(enabled: bool, measure: impl FnOnce() -> T) -> Option<T> {
    enabled.then(measure)
}

fn existing_buffer_work(bytes: &[u8], rows: usize) -> (u64, u64) {
    (
        bytes.len().try_into().unwrap_or(u64::MAX),
        rows.try_into().unwrap_or(u64::MAX),
    )
}

pub(crate) fn record_json_history(site: HistoryWorkSite, messages: &[Value]) {
    let Some((bytes, rows)) =
        measure_when(instrumentation_enabled(), || json_history_work(messages))
    else {
        return;
    };
    record_operation(site, bytes, rows, 0);
}

pub(crate) fn clone_json_history(site: HistoryWorkSite, messages: &[Value]) -> Vec<Value> {
    record_json_history(site, messages);
    messages.to_vec()
}

pub(crate) fn record_pair_history(site: HistoryWorkSite, history: &[(String, String)]) {
    let Some((bytes, rows)) =
        measure_when(instrumentation_enabled(), || pair_history_work(history))
    else {
        return;
    };
    record_operation(site, bytes, rows, 0);
}

pub(crate) fn clone_pair_history(
    site: HistoryWorkSite,
    history: &[(String, String)],
) -> Vec<(String, String)> {
    record_pair_history(site, history);
    history.to_vec()
}

/// Record a serialization or hash that consumed an already-materialized
/// buffer. This never reserializes or traverses the source history.
pub(crate) fn record_existing_buffer(site: HistoryWorkSite, bytes: &[u8], rows: usize) {
    if !instrumentation_enabled() {
        return;
    }
    let (bytes, rows) = existing_buffer_work(bytes, rows);
    record_operation(site, bytes, rows, 0);
}

pub(crate) fn record_measured_work(site: HistoryWorkSite, bytes: u64, rows: usize) {
    if !instrumentation_enabled() {
        return;
    }
    record_operation(site, bytes, rows.try_into().unwrap_or(u64::MAX), 0);
}

pub(crate) fn record_text_payload<'a>(
    site: HistoryWorkSite,
    row_count: usize,
    texts: impl IntoIterator<Item = &'a str>,
) {
    let Some(bytes) = measure_when(instrumentation_enabled(), || {
        texts.into_iter().fold(0_u64, |bytes, text| {
            bytes.saturating_add(text.len().try_into().unwrap_or(u64::MAX))
        })
    }) else {
        return;
    };
    record_operation(site, bytes, row_count.try_into().unwrap_or(u64::MAX), 0);
}

pub(crate) fn record_fork_tool_schema_serialization(
    site: HistoryWorkSite,
    entries: &[astra_turn_core::fork_prefix::ToolSchemaEntry],
) {
    let Some(bytes) = measure_when(instrumentation_enabled(), || {
        entries.iter().fold(0_u64, |bytes, entry| {
            bytes.saturating_add(entry.canonical_bytes.len().try_into().unwrap_or(u64::MAX))
        })
    }) else {
        return;
    };
    record_operation(site, bytes, entries.len().try_into().unwrap_or(u64::MAX), 0);
}

pub(crate) fn reserve_json_history_queue(
    site: HistoryWorkSite,
    messages: &[Value],
) -> Option<QueueBytesReservation> {
    measure_when(instrumentation_enabled(), || {
        let (bytes, _) = json_history_work(messages);
        QueueBytesReservation::for_site(site, bytes)
    })
}

pub(crate) fn reserve_pair_history_queue(
    site: HistoryWorkSite,
    history: &[(String, String)],
) -> Option<QueueBytesReservation> {
    measure_when(instrumentation_enabled(), || {
        let (bytes, _) = pair_history_work(history);
        QueueBytesReservation::for_site(site, bytes)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    #[test]
    fn disabled_measurement_gate_does_not_touch_history() {
        let traversed = Cell::new(false);
        let measured = measure_when(false, || {
            traversed.set(true);
            (u64::MAX, u64::MAX)
        });

        assert_eq!(measured, None);
        assert!(!traversed.get());
    }

    #[test]
    fn existing_buffer_measurement_uses_materialized_length() {
        let encoded = br#"[{"role":"user","content":"hello"}]"#;
        // This is the pure function called by `record_existing_buffer`. Its
        // API has only the materialized bytes, so it cannot reserialize or
        // revisit the source history.
        let measured = existing_buffer_work(encoded, 1);
        assert_eq!(measured, (35, 1));
    }

    #[test]
    fn structural_measurements_count_owned_payload() {
        let json = vec![serde_json::json!({
            "role": "user",
            "content": "hello",
            "metadata": {"attempt": 1}
        })];
        let pairs = vec![("hello".to_string(), "world".to_string())];

        let (json_bytes, json_rows) = json_history_work(&json);
        let (pair_bytes, pair_rows) = pair_history_work(&pairs);

        assert!(json_bytes >= 5);
        assert_eq!(json_rows, 1);
        assert_eq!(pair_bytes, 10);
        assert_eq!(pair_rows, 1);
    }
}
