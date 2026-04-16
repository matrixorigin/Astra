use serde::{Deserialize, Serialize};

use crate::counter::count_persisted_turn_events;

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plan_prefers_llm_response_id() {
        let plan =
            build_session_activity_update_plan(false, 0, 0, 0, false, Some("p1"), Some("l1"));
        assert_eq!(plan.last_event_id.as_deref(), Some("l1"));
    }

    #[test]
    fn plan_falls_back_to_parent_id() {
        let plan = build_session_activity_update_plan(false, 0, 0, 0, false, Some("p1"), None);
        assert_eq!(plan.last_event_id.as_deref(), Some("p1"));
    }

    #[test]
    fn plan_no_ids() {
        let plan = build_session_activity_update_plan(false, 0, 0, 0, false, None, None);
        assert!(plan.last_event_id.is_none());
    }

    #[test]
    fn plan_event_count_delegates_to_counter() {
        let plan = build_session_activity_update_plan(true, 2, 3, 1, true, None, None);
        assert_eq!(plan.event_count_increment, 8);
    }
}
