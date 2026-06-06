use serde_json::Value;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AgentSpawnedProjection {
    pub(crate) agent_id: String,
    pub(crate) description: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AgentTerminatedProjection {
    pub(crate) agent_id: String,
    pub(crate) run_id: String,
    pub(crate) status: String,
    pub(crate) turns_completed: u64,
}

pub(crate) fn project_agent_spawned(metadata: Option<&Value>) -> AgentSpawnedProjection {
    AgentSpawnedProjection {
        agent_id: metadata_string(metadata, "agent_id").unwrap_or_else(|| "?".to_string()),
        description: metadata_string(metadata, "description").unwrap_or_default(),
    }
}

pub(crate) fn project_agent_terminated(metadata: Option<&Value>) -> AgentTerminatedProjection {
    AgentTerminatedProjection {
        agent_id: metadata_string(metadata, "agent_id").unwrap_or_else(|| "?".to_string()),
        run_id: metadata_string(metadata, "run_id").unwrap_or_else(|| "?".to_string()),
        status: metadata_string(metadata, "status").unwrap_or_else(|| "?".to_string()),
        turns_completed: metadata_u64(metadata, "turns_completed").unwrap_or(0),
    }
}

fn metadata_string(metadata: Option<&Value>, key: &str) -> Option<String> {
    metadata
        .and_then(|m| m.get(key))
        .and_then(Value::as_str)
        .map(str::to_string)
}

fn metadata_u64(metadata: Option<&Value>, key: &str) -> Option<u64> {
    metadata.and_then(|m| m.get(key)).and_then(Value::as_u64)
}

#[cfg(test)]
mod tests {
    use super::{project_agent_spawned, project_agent_terminated};
    use serde_json::json;

    #[test]
    fn agent_spawned_projection_defaults_missing_fields() {
        let projection = project_agent_spawned(Some(&json!({})));
        assert_eq!(projection.agent_id, "?");
        assert!(projection.description.is_empty());
    }

    #[test]
    fn agent_terminated_projection_preserves_status_and_turns() {
        let projection = project_agent_terminated(Some(&json!({
            "agent_id": "reviewer@abc",
            "run_id": "run-1",
            "status": "interrupted",
            "turns_completed": 3
        })));
        assert_eq!(projection.agent_id, "reviewer@abc");
        assert_eq!(projection.run_id, "run-1");
        assert_eq!(projection.status, "interrupted");
        assert_eq!(projection.turns_completed, 3);
    }
}
