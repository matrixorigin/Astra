use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SnapshotLinkPlan {
    pub context_capture_id: String,
    pub llm_request_id: String,
    pub llm_response_id: Option<String>,
}

pub fn build_snapshot_link_plan(
    context_capture_id: Option<&str>,
    parent_event_id: Option<&str>,
    llm_response_event_id: Option<&str>,
) -> Option<SnapshotLinkPlan> {
    Some(SnapshotLinkPlan {
        context_capture_id: context_capture_id?.to_string(),
        llm_request_id: parent_event_id?.to_string(),
        llm_response_id: llm_response_event_id.map(ToString::to_string),
    })
}
