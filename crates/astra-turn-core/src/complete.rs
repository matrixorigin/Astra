use serde_json::{Map, Value};

use crate::stall::DivergenceStatus;

pub fn build_turn_complete_event(
    has_tool_calls: bool,
    stall_detected: bool,
    divergence_status: &DivergenceStatus,
    execution_state: Option<Value>,
    assistant_text: Option<&str>,
) -> Map<String, Value> {
    let force_stop = stall_detected || matches!(divergence_status, DivergenceStatus::Diverging(_));
    let mut event = Map::from_iter([
        (
            "type".to_string(),
            Value::String("turn_complete".to_string()),
        ),
        (
            "has_tool_calls".to_string(),
            Value::Bool(if force_stop { false } else { has_tool_calls }),
        ),
    ]);
    if stall_detected {
        event.insert("stall_detected".to_string(), Value::Bool(true));
    }
    if let DivergenceStatus::Diverging(rounds) = divergence_status {
        event.insert("divergence_detected".to_string(), Value::Bool(true));
        event.insert(
            "exploration_rounds".to_string(),
            Value::Number((*rounds as u64).into()),
        );
    }
    if let Some(execution_state) = execution_state {
        event.insert("execution_state".to_string(), execution_state);
    }
    if let Some(assistant_text) = assistant_text
        && !assistant_text.is_empty()
    {
        event.insert(
            "assistant_text".to_string(),
            Value::String(assistant_text.to_string()),
        );
    }
    event
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn complete_basic_no_force_stop() {
        let event = build_turn_complete_event(true, false, &DivergenceStatus::Healthy, None, None);
        assert_eq!(event["type"].as_str().unwrap(), "turn_complete");
        assert!(event["has_tool_calls"].as_bool().unwrap());
        assert!(event.get("stall_detected").is_none());
        assert!(event.get("divergence_detected").is_none());
    }

    #[test]
    fn complete_stall_forces_no_tool_calls() {
        let event = build_turn_complete_event(true, true, &DivergenceStatus::Healthy, None, None);
        assert!(!event["has_tool_calls"].as_bool().unwrap());
        assert!(event["stall_detected"].as_bool().unwrap());
    }

    #[test]
    fn complete_diverging_forces_no_tool_calls() {
        let event =
            build_turn_complete_event(true, false, &DivergenceStatus::Diverging(3), None, None);
        assert!(!event["has_tool_calls"].as_bool().unwrap());
        assert!(event["divergence_detected"].as_bool().unwrap());
        assert_eq!(event["exploration_rounds"].as_u64().unwrap(), 3);
    }

    #[test]
    fn complete_exploring_no_force_stop() {
        let event =
            build_turn_complete_event(true, false, &DivergenceStatus::Exploring(2), None, None);
        assert!(event["has_tool_calls"].as_bool().unwrap());
        assert!(event.get("divergence_detected").is_none());
    }

    #[test]
    fn complete_with_execution_state() {
        let state = json!({"round": 3});
        let event =
            build_turn_complete_event(false, false, &DivergenceStatus::Healthy, Some(state), None);
        assert_eq!(event["execution_state"]["round"].as_i64().unwrap(), 3);
    }

    #[test]
    fn complete_no_execution_state() {
        let event = build_turn_complete_event(false, false, &DivergenceStatus::Healthy, None, None);
        assert!(event.get("execution_state").is_none());
    }

    #[test]
    fn complete_with_assistant_text() {
        let event = build_turn_complete_event(
            false,
            false,
            &DivergenceStatus::Healthy,
            None,
            Some("final reconciled answer"),
        );
        assert_eq!(
            event.get("assistant_text"),
            Some(&json!("final reconciled answer"))
        );
    }
}
