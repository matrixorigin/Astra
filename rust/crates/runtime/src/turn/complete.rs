use serde_json::{Map, Value};

pub fn build_turn_complete_event(
    has_tool_calls: bool,
    stall_detected: bool,
    execution_state: Option<Value>,
) -> Map<String, Value> {
    let mut event = Map::from_iter([
        (
            "type".to_string(),
            Value::String("turn_complete".to_string()),
        ),
        (
            "has_tool_calls".to_string(),
            Value::Bool(if stall_detected {
                false
            } else {
                has_tool_calls
            }),
        ),
    ]);
    if stall_detected {
        event.insert("stall_detected".to_string(), Value::Bool(true));
    }
    if let Some(execution_state) = execution_state {
        event.insert("execution_state".to_string(), execution_state);
    }
    event
}
