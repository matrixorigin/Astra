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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plan_all_some() {
        let plan = build_snapshot_link_plan(Some("cap1"), Some("req1"), Some("resp1")).unwrap();
        assert_eq!(plan.context_capture_id, "cap1");
        assert_eq!(plan.llm_request_id, "req1");
        assert_eq!(plan.llm_response_id.as_deref(), Some("resp1"));
    }

    #[test]
    fn plan_no_response_id() {
        let plan = build_snapshot_link_plan(Some("cap1"), Some("req1"), None).unwrap();
        assert!(plan.llm_response_id.is_none());
    }

    #[test]
    fn plan_no_capture_id_returns_none() {
        assert!(build_snapshot_link_plan(None, Some("req1"), Some("resp1")).is_none());
    }

    #[test]
    fn plan_no_parent_id_returns_none() {
        assert!(build_snapshot_link_plan(Some("cap1"), None, Some("resp1")).is_none());
    }

    #[test]
    fn plan_all_none() {
        assert!(build_snapshot_link_plan(None, None, None).is_none());
    }

    #[test]
    fn plan_serializes() {
        let plan = build_snapshot_link_plan(Some("c"), Some("r"), None).unwrap();
        let json = serde_json::to_string(&plan).unwrap();
        assert!(json.contains("context_capture_id"));
        let roundtrip: SnapshotLinkPlan = serde_json::from_str(&json).unwrap();
        assert_eq!(roundtrip, plan);
    }
}
