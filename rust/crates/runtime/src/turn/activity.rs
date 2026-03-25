use serde::{Deserialize, Serialize};

use crate::count_persisted_turn_events;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SessionActivityUpdatePlan {
    pub event_count_increment: usize,
    pub last_event_id: Option<String>,
}

pub fn build_session_activity_update_plan(
    has_user_content: bool,
    tool_results_len: usize,
    tool_calls_len: usize,
    cloud_tool_results_len: usize,
    has_full_text: bool,
    parent_event_id: Option<&str>,
    llm_response_event_id: Option<&str>,
) -> SessionActivityUpdatePlan {
    SessionActivityUpdatePlan {
        event_count_increment: count_persisted_turn_events(
            has_user_content,
            tool_results_len,
            tool_calls_len,
            cloud_tool_results_len,
            has_full_text,
        ),
        last_event_id: llm_response_event_id
            .map(ToString::to_string)
            .or_else(|| parent_event_id.map(ToString::to_string)),
    }
}
