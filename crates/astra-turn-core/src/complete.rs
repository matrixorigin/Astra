use serde_json::{Map, Value};

use crate::stall::DivergenceStatus;

#[derive(Debug, Clone, PartialEq)]
pub struct TurnCompletionFacts {
    pub stall_detected: bool,
    pub divergence_status: DivergenceStatus,
}

impl Default for TurnCompletionFacts {
    fn default() -> Self {
        Self {
            stall_detected: false,
            divergence_status: DivergenceStatus::Healthy,
        }
    }
}

impl TurnCompletionFacts {
    pub fn from_tool_signatures(tool_signatures: &[std::collections::BTreeSet<String>]) -> Self {
        let stall_detected =
            crate::stall::detect_server_stall(tool_signatures, crate::stall::SERVER_STALL_WINDOW)
                .unwrap_or(false);
        let divergence_status =
            crate::stall::detect_divergence(tool_signatures).unwrap_or(DivergenceStatus::Healthy);
        Self {
            stall_detected,
            divergence_status,
        }
    }
}

pub fn build_turn_complete_event(
    has_tool_calls: bool,
    facts: &TurnCompletionFacts,
    execution_state: Option<Value>,
    assistant_text: Option<&str>,
) -> Map<String, Value> {
    let mut event = Map::from_iter([
        (
            "type".to_string(),
            Value::String("turn_complete".to_string()),
        ),
        ("has_tool_calls".to_string(), Value::Bool(has_tool_calls)),
    ]);
    if facts.stall_detected {
        event.insert("stall_detected".to_string(), Value::Bool(true));
    }
    if let DivergenceStatus::Diverging(rounds) = &facts.divergence_status {
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
    fn complete_basic_preserves_observed_facts() {
        let event = build_turn_complete_event(true, &TurnCompletionFacts::default(), None, None);
        assert_eq!(event["type"].as_str().unwrap(), "turn_complete");
        assert!(event["has_tool_calls"].as_bool().unwrap());
        assert!(event.get("stall_detected").is_none());
        assert!(event.get("divergence_detected").is_none());
    }

    #[test]
    fn complete_stall_preserves_actual_tool_call_fact() {
        let event = build_turn_complete_event(
            true,
            &TurnCompletionFacts {
                stall_detected: true,
                divergence_status: DivergenceStatus::Healthy,
            },
            None,
            None,
        );
        assert!(event["has_tool_calls"].as_bool().unwrap());
        assert!(event["stall_detected"].as_bool().unwrap());
    }

    #[test]
    fn complete_diverging_preserves_actual_tool_call_fact() {
        let event = build_turn_complete_event(
            true,
            &TurnCompletionFacts {
                stall_detected: false,
                divergence_status: DivergenceStatus::Diverging(3),
            },
            None,
            None,
        );
        assert!(event["has_tool_calls"].as_bool().unwrap());
        assert!(event["divergence_detected"].as_bool().unwrap());
        assert_eq!(event["exploration_rounds"].as_u64().unwrap(), 3);
    }

    #[test]
    fn complete_exploring_preserves_observed_facts() {
        let event = build_turn_complete_event(
            true,
            &TurnCompletionFacts {
                stall_detected: false,
                divergence_status: DivergenceStatus::Exploring(2),
            },
            None,
            None,
        );
        assert!(event["has_tool_calls"].as_bool().unwrap());
        assert!(event.get("divergence_detected").is_none());
    }

    #[test]
    fn complete_with_execution_state() {
        let state = json!({"round": 3});
        let event =
            build_turn_complete_event(false, &TurnCompletionFacts::default(), Some(state), None);
        assert_eq!(event["execution_state"]["round"].as_i64().unwrap(), 3);
    }

    #[test]
    fn complete_no_execution_state() {
        let event = build_turn_complete_event(false, &TurnCompletionFacts::default(), None, None);
        assert!(event.get("execution_state").is_none());
    }

    #[test]
    fn complete_with_assistant_text() {
        let event = build_turn_complete_event(
            false,
            &TurnCompletionFacts::default(),
            None,
            Some("final reconciled answer"),
        );
        assert_eq!(
            event.get("assistant_text"),
            Some(&json!("final reconciled answer"))
        );
    }
}
