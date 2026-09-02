//! Portable wire contract for a session's durable run tree.
//!
//! The server owns lifecycle truth. Clients consume this snapshot as a
//! projection and must not infer control availability from display text.

use serde::{Deserialize, Serialize};

pub const SESSION_RUN_TREE_SCHEMA_VERSION: u32 = 5;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionRunLifecycleStatus {
    Running,
    Waiting,
    Paused,
    Completed,
    Delegated,
    Interrupted,
    Failed,
    Cancelled,
}

impl SessionRunLifecycleStatus {
    #[must_use]
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Completed | Self::Delegated | Self::Interrupted | Self::Failed | Self::Cancelled
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionRunAction {
    Pause,
    Resume,
    ContinueSession,
    Cancel,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionRunPermissionFacts {
    pub has_issues: bool,
    pub requests: u32,
    pub approved: u32,
    pub tools_blocked: u32,
}

/// Portable facts needed to explain where and under which capability envelope
/// an agent run executes. Every field is evidence-backed; absent facts stay
/// absent instead of being inferred from display names or deployment mode.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionRunRuntimeFacts {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime_profile: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub offering_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_binding_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_binding_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_binding_schema_version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub background: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permission: Option<SessionRunPermissionFacts>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionRunNode {
    pub run_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_run_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub root_run_id: Option<String>,
    pub depth: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_name: Option<String>,
    pub status: SessionRunLifecycleStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub waiting_for: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_code: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_message: Option<String>,
    pub run_event_high_watermark: i64,
    pub total_tool_calls: u32,
    #[serde(default)]
    pub runtime: SessionRunRuntimeFacts,
    pub available_actions: Vec<SessionRunAction>,
    pub created_at: String,
    pub updated_at: String,
}

impl SessionRunNode {
    /// Agent monitor membership is explicit run metadata, not inferred from
    /// parentage, depth, display names, or runtime profile labels.
    #[must_use]
    pub fn is_agent_run(&self) -> bool {
        self.agent_id
            .as_deref()
            .is_some_and(|agent_id| !agent_id.trim().is_empty())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionRunTreeSnapshot {
    pub schema_version: u32,
    pub session_id: String,
    /// Hash of the canonical node payload and truncation metadata. It is stable
    /// when only `observed_at` changes, so clients can cheaply skip reductions.
    pub snapshot_revision: String,
    pub observed_at: String,
    pub node_limit: u32,
    pub truncated: bool,
    pub runs: Vec<SessionRunNode>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_round_trip_preserves_typed_lifecycle_and_actions() {
        let snapshot = SessionRunTreeSnapshot {
            schema_version: SESSION_RUN_TREE_SCHEMA_VERSION,
            session_id: "session-1".into(),
            snapshot_revision: "sha256:abc".into(),
            observed_at: "2026-07-11T00:00:00Z".into(),
            node_limit: 200,
            truncated: false,
            runs: vec![SessionRunNode {
                run_id: "child-1".into(),
                parent_run_id: Some("root-1".into()),
                root_run_id: Some("root-1".into()),
                depth: 1,
                agent_id: Some("reviewer".into()),
                agent_name: Some("Reviewer".into()),
                status: SessionRunLifecycleStatus::Paused,
                waiting_for: Some("user_resume".into()),
                error_code: None,
                error_message: None,
                run_event_high_watermark: 7,
                total_tool_calls: 3,
                runtime: SessionRunRuntimeFacts {
                    runtime_profile: Some("edge".into()),
                    offering_id: Some("offer-gpt-5".into()),
                    model_name: Some("gpt-5".into()),
                    ..Default::default()
                },
                available_actions: vec![SessionRunAction::Resume, SessionRunAction::Cancel],
                created_at: "2026-07-11T00:00:00Z".into(),
                updated_at: "2026-07-11T00:01:00Z".into(),
            }],
        };

        let payload = serde_json::to_value(&snapshot).unwrap();
        assert_eq!(payload["runs"][0]["status"], "paused");
        assert_eq!(
            payload["runs"][0]["available_actions"],
            serde_json::json!(["resume", "cancel"])
        );
        assert_eq!(payload["runs"][0]["runtime"]["offering_id"], "offer-gpt-5");
        assert_eq!(payload["runs"][0]["runtime"]["model_name"], "gpt-5");
        assert!(payload["runs"][0]["runtime"].get("model_gateway").is_none());
        assert_eq!(
            serde_json::from_value::<SessionRunTreeSnapshot>(payload).unwrap(),
            snapshot
        );
    }

    #[test]
    fn delegated_is_a_distinct_terminal_wire_status() {
        let payload = serde_json::to_value(SessionRunLifecycleStatus::Delegated).unwrap();
        assert_eq!(payload, serde_json::json!("delegated"));
        assert!(SessionRunLifecycleStatus::Delegated.is_terminal());
        assert_eq!(
            serde_json::from_value::<SessionRunLifecycleStatus>(payload).unwrap(),
            SessionRunLifecycleStatus::Delegated
        );
    }

    #[test]
    fn interrupted_is_a_distinct_terminal_wire_status() {
        let payload = serde_json::to_value(SessionRunLifecycleStatus::Interrupted).unwrap();
        assert_eq!(payload, serde_json::json!("interrupted"));
        assert!(SessionRunLifecycleStatus::Interrupted.is_terminal());
        assert_eq!(
            serde_json::from_value::<SessionRunLifecycleStatus>(payload).unwrap(),
            SessionRunLifecycleStatus::Interrupted
        );
    }

    #[test]
    fn agent_membership_uses_typed_identity_not_tree_position() {
        let mut node = SessionRunNode {
            run_id: "nested-conversation-run".into(),
            parent_run_id: Some("root-run".into()),
            root_run_id: Some("root-run".into()),
            depth: 3,
            agent_id: None,
            agent_name: Some("misleading display name".into()),
            status: SessionRunLifecycleStatus::Running,
            waiting_for: None,
            error_code: None,
            error_message: None,
            run_event_high_watermark: 0,
            total_tool_calls: 0,
            runtime: SessionRunRuntimeFacts {
                runtime_profile: Some("agent_binding_registry".into()),
                ..Default::default()
            },
            available_actions: vec![SessionRunAction::Cancel],
            created_at: "2026-07-11T00:00:00Z".into(),
            updated_at: "2026-07-11T00:00:01Z".into(),
        };
        assert!(!node.is_agent_run());

        node.parent_run_id = None;
        node.depth = 0;
        node.agent_id = Some("team-orchestrator".into());
        assert!(node.is_agent_run());
    }
}
