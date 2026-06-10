pub mod agent_journal_event_surface;
pub mod delegation_event_surface;
pub mod health_status_surface;
pub mod run_status_surface;
pub mod self_surface;
pub mod session_source_surface;
pub mod session_task_surface;
pub mod session_workspace_status_surface;
pub mod skill_install_status_surface;
pub mod task_checkpoint_surface;
pub mod task_result_surface;

use serde_json::Value;

pub(crate) fn metadata_string(metadata: Option<&Value>, key: &str) -> Option<String> {
    metadata
        .and_then(|m| m.get(key))
        .and_then(Value::as_str)
        .map(str::to_string)
}

pub(crate) fn metadata_u64(metadata: Option<&Value>, key: &str) -> Option<u64> {
    metadata.and_then(|m| m.get(key)).and_then(Value::as_u64)
}
