//! Transport envelope decoding shared by live and durable Explain consumers.
use serde_json::Value;

use crate::{EXPLAIN_ANALYZE_EVENT_TYPE, EXPLAIN_ANALYZE_MAX_SAFE_INTEGER, ExplainAnalyzeEventV1};

/// A durable cursor identifies delivery, not the runtime fact. Validate it before
/// removing it so live and replay copies retain identical fact identity.
pub fn decode_explain_analyze_wire(event: &Value) -> Result<ExplainAnalyzeEventV1, &'static str> {
    if event.get("type").and_then(Value::as_str) != Some(EXPLAIN_ANALYZE_EVENT_TYPE) {
        return Err("unexpected event type");
    }
    if let Some(index) = event.get("index")
        && !index
            .as_u64()
            .is_some_and(|value| value <= EXPLAIN_ANALYZE_MAX_SAFE_INTEGER)
    {
        return Err("invalid Explain Analyze replay index");
    }
    let mut payload = event
        .as_object()
        .cloned()
        .ok_or("event must be an object")?;
    payload.remove("type");
    payload.remove("index");
    let fact: ExplainAnalyzeEventV1 = serde_json::from_value(Value::Object(payload))
        .map_err(|_| "event does not match the Explain Analyze schema")?;
    if !fact.is_valid() {
        return Err("event failed Explain Analyze schema validation");
    }
    Ok(fact)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event() -> Value {
        serde_json::json!({"type":"explain_analyze","schema_version":1,"event_id":"e",
            "run_id":"r","turn_id":"t","node_id":"n","producer_id":"p",
            "clock_domain_id":"c","kind":"admission","label":"Admission",
            "transition":"started","elapsed_ms":0})
    }

    #[test]
    fn explain_wire_live_and_replay_have_identical_facts() {
        let live = event();
        for index in [0, 7, EXPLAIN_ANALYZE_MAX_SAFE_INTEGER] {
            let mut replay = live.clone();
            replay["index"] = index.into();
            assert_eq!(
                decode_explain_analyze_wire(&live),
                decode_explain_analyze_wire(&replay)
            );
        }
    }

    #[test]
    fn explain_wire_rejects_invalid_cursor_and_unknown_fields() {
        for index in [
            serde_json::json!(-1),
            serde_json::json!(0.5),
            serde_json::json!("1"),
            Value::Null,
            serde_json::json!(true),
            serde_json::json!(EXPLAIN_ANALYZE_MAX_SAFE_INTEGER + 1),
        ] {
            let mut replay = event();
            replay["index"] = index;
            assert!(decode_explain_analyze_wire(&replay).is_err());
        }
        let mut extended = event();
        extended["raw_trace"] = true.into();
        assert!(decode_explain_analyze_wire(&extended).is_err());
    }
}
