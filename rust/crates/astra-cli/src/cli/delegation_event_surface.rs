use serde_json::Value;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DelegationStartedProjection {
    pub(crate) run_id: Option<String>,
    pub(crate) parent_run_id: Option<String>,
    pub(crate) pattern: String,
    pub(crate) agent_ids: Vec<String>,
    pub(crate) agent_count: usize,
    pub(crate) agent_type: Option<String>,
    pub(crate) task: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DelegationSubRunStartedProjection {
    pub(crate) run_id: Option<String>,
    pub(crate) sub_run_id: String,
    pub(crate) agent_id: String,
    pub(crate) status: String,
    pub(crate) retry_of: Option<String>,
    pub(crate) agent_type: Option<String>,
    pub(crate) task: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DelegationSubRunCompletedProjection {
    pub(crate) run_id: Option<String>,
    pub(crate) sub_run_id: String,
    pub(crate) agent_id: String,
    pub(crate) status: String,
    pub(crate) error: Option<String>,
    pub(crate) output_preview: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DelegationRetryProjection {
    pub(crate) original_run_id: String,
    pub(crate) retry_run_id: String,
    pub(crate) agent_id: String,
    pub(crate) attempt: u32,
    pub(crate) reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DelegationCompletedProjection {
    pub(crate) pattern: String,
    pub(crate) total_sub_runs: usize,
    pub(crate) succeeded: usize,
    pub(crate) failed: usize,
    pub(crate) aggregated_status: String,
    pub(crate) aggregated_output_preview: Option<String>,
}

pub(crate) fn project_delegation_started(metadata: Option<&Value>) -> DelegationStartedProjection {
    DelegationStartedProjection {
        run_id: metadata_string(metadata, "run_id"),
        parent_run_id: metadata_string(metadata, "parent_run_id"),
        pattern: metadata_string(metadata, "pattern").unwrap_or_else(|| "?".to_string()),
        agent_ids: metadata
            .and_then(|m| m.get("agent_ids"))
            .and_then(Value::as_array)
            .map(|ids| {
                ids.iter()
                    .filter_map(|value| value.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default(),
        agent_count: metadata_u64(metadata, "agent_count").unwrap_or(0) as usize,
        agent_type: metadata_string(metadata, "agent_type"),
        task: metadata_string(metadata, "task"),
    }
}

pub(crate) fn delegation_event_id(metadata: Option<&Value>) -> Option<String> {
    metadata_string(metadata, "delegation_id")
}

pub(crate) fn project_delegation_sub_run_started(
    metadata: Option<&Value>,
) -> DelegationSubRunStartedProjection {
    DelegationSubRunStartedProjection {
        run_id: metadata_string(metadata, "run_id")
            .or_else(|| metadata_string(metadata, "sub_run_id")),
        sub_run_id: metadata_string(metadata, "sub_run_id").unwrap_or_default(),
        agent_id: metadata_string(metadata, "agent_id").unwrap_or_else(|| "?".to_string()),
        status: metadata_string(metadata, "status").unwrap_or_else(|| "running".to_string()),
        retry_of: metadata_string(metadata, "retry_of"),
        agent_type: metadata_string(metadata, "agent_type"),
        task: metadata_string(metadata, "task"),
    }
}

pub(crate) fn project_delegation_sub_run_completed(
    metadata: Option<&Value>,
) -> DelegationSubRunCompletedProjection {
    DelegationSubRunCompletedProjection {
        run_id: metadata_string(metadata, "run_id")
            .or_else(|| metadata_string(metadata, "sub_run_id")),
        sub_run_id: metadata_string(metadata, "sub_run_id").unwrap_or_default(),
        agent_id: metadata_string(metadata, "agent_id").unwrap_or_else(|| "?".to_string()),
        status: metadata_string(metadata, "status").unwrap_or_else(|| "?".to_string()),
        error: metadata_string(metadata, "error"),
        output_preview: metadata_string(metadata, "output_preview"),
    }
}

pub(crate) fn project_delegation_retry(metadata: Option<&Value>) -> DelegationRetryProjection {
    DelegationRetryProjection {
        original_run_id: metadata_string(metadata, "original_run_id").unwrap_or_default(),
        retry_run_id: metadata_string(metadata, "retry_run_id").unwrap_or_default(),
        agent_id: metadata_string(metadata, "agent_id").unwrap_or_else(|| "?".to_string()),
        attempt: metadata_u64(metadata, "attempt").unwrap_or(2) as u32,
        reason: metadata_string(metadata, "reason").unwrap_or_default(),
    }
}

pub(crate) fn project_delegation_completed(
    metadata: Option<&Value>,
) -> DelegationCompletedProjection {
    DelegationCompletedProjection {
        pattern: metadata_string(metadata, "pattern").unwrap_or_else(|| "?".to_string()),
        total_sub_runs: metadata_u64(metadata, "total_sub_runs").unwrap_or(0) as usize,
        succeeded: metadata_u64(metadata, "succeeded").unwrap_or(0) as usize,
        failed: metadata_u64(metadata, "failed").unwrap_or(0) as usize,
        aggregated_status: metadata_string(metadata, "aggregated_status")
            .unwrap_or_else(|| "?".to_string()),
        aggregated_output_preview: metadata_string(metadata, "aggregated_output_preview"),
    }
}

pub(crate) fn delegation_sub_run_detail(
    projection: &DelegationSubRunCompletedProjection,
) -> Option<&str> {
    projection
        .error
        .as_deref()
        .or(projection.output_preview.as_deref())
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
    use super::*;
    use serde_json::json;

    #[test]
    fn retry_projection_defaults_attempt_to_two() {
        let projection = project_delegation_retry(Some(&json!({
            "original_run_id": "run-a",
            "retry_run_id": "run-b",
            "agent_id": "reviewer",
            "reason": "needs another pass"
        })));
        assert_eq!(projection.attempt, 2);
        assert_eq!(projection.reason, "needs another pass");
    }

    #[test]
    fn completed_sub_run_detail_prefers_error_then_preview() {
        let projection = project_delegation_sub_run_completed(Some(&json!({
            "sub_run_id": "run-a",
            "agent_id": "reviewer",
            "status": "failed",
            "error": "permission denied",
            "output_preview": "partial output"
        })));
        assert_eq!(
            delegation_sub_run_detail(&projection),
            Some("permission denied")
        );

        let preview_only = project_delegation_sub_run_completed(Some(&json!({
            "sub_run_id": "run-b",
            "agent_id": "coder",
            "status": "completed",
            "output_preview": "all good"
        })));
        assert_eq!(delegation_sub_run_detail(&preview_only), Some("all good"));
    }

    #[test]
    fn started_projection_collects_agent_ids_and_parent() {
        let projection = project_delegation_started(Some(&json!({
            "parent_run_id": "root-run",
            "pattern": "fan_out",
            "agent_count": 2,
            "agent_ids": ["coder", "reviewer"]
        })));
        assert_eq!(projection.parent_run_id.as_deref(), Some("root-run"));
        assert_eq!(projection.pattern, "fan_out");
        assert_eq!(projection.agent_count, 2);
        assert_eq!(projection.agent_ids, vec!["coder", "reviewer"]);
    }

    #[test]
    fn sub_run_started_projection_falls_back_to_sub_run_id_for_run_id() {
        let projection = project_delegation_sub_run_started(Some(&json!({
            "sub_run_id": "run-child-1",
            "agent_id": "reviewer"
        })));
        assert_eq!(projection.run_id.as_deref(), Some("run-child-1"));
        assert_eq!(projection.sub_run_id, "run-child-1");
    }

    #[test]
    fn delegation_event_id_reads_common_identifier() {
        assert_eq!(
            delegation_event_id(Some(&json!({"delegation_id": "deleg-123"}))).as_deref(),
            Some("deleg-123")
        );
        assert_eq!(delegation_event_id(Some(&json!({}))), None);
    }
}
