use serde_json::{Map, Value};

use super::stall::DivergenceStatus;

pub fn build_turn_complete_event(
    has_tool_calls: bool,
    stall_detected: bool,
    divergence_status: &DivergenceStatus,
    execution_state: Option<Value>,
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
    event
}
