//! Best-effort tool-result selection observations in the existing trace lane.
//!
//! The trace explains evaluation. Provider-wire application remains owned by
//! the durable projection decision/receipt stored with model-request facts.

use std::collections::HashMap;

use crate::session_journal::TraceSpanBuilder;

pub const TOOL_RESULT_SELECTION_TRACE_NAME: &str = "tool_result_selection";

pub fn tool_result_selection_trace(
    observation_span_id: &str,
    observation: &astra_turn_types::ToolResultSelectionObservationV1,
    observed_at_us: u64,
) -> Result<TraceSpanBuilder, &'static str> {
    observation.validate()?;
    if observation_span_id.trim().is_empty()
        || observation_span_id.len() > 512
        || !observation_span_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"_-.:".contains(&byte))
    {
        return Err("invalid tool-result selection trace identity");
    }
    let encoded = serde_json::to_string(observation)
        .map_err(|_| "serialize tool-result selection observation")?;
    let attrs = HashMap::from([(
        astra_turn_types::TOOL_RESULT_SELECTION_TRACE_ATTR.to_owned(),
        encoded,
    )]);
    Ok(TraceSpanBuilder::default()
        .span_id(observation_span_id.to_owned())
        .name(TOOL_RESULT_SELECTION_TRACE_NAME.to_owned())
        .trace_id(Some(observation.correlation.run_id.clone()))
        .turn(Some(observation.correlation.turn))
        .start_us(observed_at_us)
        .end_us(observed_at_us)
        .attrs(Some(&attrs)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trace_contains_only_the_typed_bounded_observation() {
        let observation = astra_turn_types::ToolResultSelectionObservationV1 {
            schema_version: astra_turn_types::TOOL_RESULT_SELECTION_OBSERVATION_SCHEMA_VERSION,
            correlation: astra_turn_types::ToolResultSelectionCorrelationV1 {
                run_id: "run-1".into(),
                turn: 1,
                round: 2,
                owner_generation: Some(3),
                evaluation_id: "evaluation-1".into(),
            },
            coverage: astra_turn_types::ToolResultSelectionCoverageV1 {
                source_bytes: 100,
                scanned_bytes: 100,
                candidate_chunks: 2,
                source_complete: true,
                goal_complete: true,
            },
            outcome: astra_turn_types::ToolResultSelectionOutcomeV1::NotDispatched {
                reason: astra_turn_types::ToolResultSelectionNotDispatchedReasonV1::NoOffering,
            },
        };
        let event = tool_result_selection_trace("selection-1", &observation, 10)
            .unwrap()
            .session_id(Some("session-1"))
            .build();
        let metadata = event.metadata.unwrap();
        assert_eq!(metadata["name"], TOOL_RESULT_SELECTION_TRACE_NAME);
        let decoded: astra_turn_types::ToolResultSelectionObservationV1 = serde_json::from_str(
            metadata["attrs"][astra_turn_types::TOOL_RESULT_SELECTION_TRACE_ATTR]
                .as_str()
                .unwrap(),
        )
        .unwrap();
        assert_eq!(decoded, observation);
    }
}
