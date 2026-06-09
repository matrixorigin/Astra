use super::test_executor;
use astra_tools::task_mgmt::{SessionTask, TaskManager, TaskStore};
use async_trait::async_trait;
use serde_json::{Value, json};
use std::sync::Arc;

// ── Config tool tests ─────────────────────────────────────────────────────

#[test]
fn config_list_settings() {
    let exe = test_executor();
    let result = exe.config_tool(&json!({ "setting": "list" }));
    let parsed: Value = serde_json::from_str(&result).unwrap();

    let settings = parsed["available_settings"]
        .as_array()
        .expect("available_settings must be an array");
    assert!(!settings.is_empty(), "must expose at least one setting");

    // Settings are objects carrying at least a `setting` key (canonical name)
    // plus a human-readable `description`. Require that shape so the UI can
    // always render each entry uniformly.
    for (i, s) in settings.iter().enumerate() {
        let name = s
            .get("setting")
            .and_then(|v| v.as_str())
            .unwrap_or_else(|| panic!("settings[{i}].setting must be a string — got: {s}"));
        assert!(
            !name.is_empty(),
            "settings[{i}].setting must be non-empty — got: {s}"
        );
    }

    // Every canonical setting the config_tool supports must be represented.
    // Protects against accidental list regression.
    let surface: Vec<String> = settings
        .iter()
        .filter_map(|s| {
            s.get("setting")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
        })
        .collect();
    for required in [
        "output_limit",
        "tool_output_limit",
        "auto_approve",
        "turn_limit",
    ] {
        assert!(
            surface.iter().any(|s| s == required),
            "expected canonical setting `{required}` in list — got: {surface:?}"
        );
    }
}

#[test]
fn config_get_output_limit() {
    let exe = test_executor();
    let result = exe.config_tool(&json!({ "setting": "output_limit" }));
    let parsed: Value = serde_json::from_str(&result).unwrap();

    assert_eq!(parsed.get("setting").unwrap(), "output_limit");
    assert!(parsed.get("value").is_some());
}

#[test]
fn config_unknown_setting() {
    let exe = test_executor();
    let result = exe.config_tool(&json!({ "setting": "unknown_setting_xyz" }));

    assert!(result.contains("error"));
    assert!(result.contains("Unknown setting"));
}

#[test]
fn config_output_limit() {
    let exe = test_executor();
    let result = exe.config_tool(&json!({ "setting": "output_limit" }));
    let parsed: Value = serde_json::from_str(&result).unwrap();

    assert!(parsed.get("value").is_some());
    let value = parsed.get("value").unwrap().as_u64().unwrap();
    assert!(value > 0);
}

#[tokio::test]
async fn brief_includes_session_state() {
    let exe = test_executor();
    let result = exe.brief(&json!({})).await;
    let parsed: Value = serde_json::from_str(&result).unwrap();

    assert!(parsed.get("effective_project_root").is_some());
    assert!(parsed.get("session").is_some());
    assert!(parsed.get("git").is_some());
    assert!(parsed.get("tasks").is_some());
    assert!(parsed.get("files").is_some());
}

#[tokio::test]
async fn brief_reports_created_tasks() {
    let exe = test_executor();
    exe.task_action_create(&json!({"title": "Implement thing"}))
        .await;
    let result = exe.brief(&json!({"focus": "tasks"})).await;
    let parsed: Value = serde_json::from_str(&result).unwrap();

    assert_eq!(parsed["tasks"]["count"], 1);
    assert_eq!(parsed["tasks"]["open_work_count"], 1);
    assert_eq!(parsed["tasks"]["items"][0]["title"], "Implement thing");
}

#[tokio::test]
async fn brief_prioritizes_paused_open_work_over_completed_history() {
    let exe = test_executor();
    exe.task_action_create(&json!({"title": "Already done"}))
        .await;
    exe.task_action_update(&json!({"task_id": "task-1", "new_status": "completed"}))
        .await;
    exe.task_action_create(&json!({"title": "Waiting on operator"}))
        .await;
    exe.task_action_update(&json!({"task_id": "task-2", "new_status": "paused"}))
        .await;

    let result = exe.brief(&json!({"focus": "tasks", "max_items": 1})).await;
    let parsed: Value = serde_json::from_str(&result).unwrap();

    assert_eq!(parsed["tasks"]["count"], 2);
    assert_eq!(parsed["tasks"]["open_work_count"], 1);
    assert_eq!(parsed["tasks"]["items"][0]["title"], "Waiting on operator");
    assert_eq!(parsed["tasks"]["items"][0]["status"], "paused");
    assert_eq!(parsed["tasks"]["items"][1]["more"], 1);
}

#[tokio::test]
async fn brief_tasks_surfaces_task_board_load_failure() {
    struct FailingTaskStore;

    #[async_trait]
    impl TaskStore for FailingTaskStore {
        async fn load(&self, _session_id: &str) -> Result<Vec<SessionTask>, String> {
            Err("simulated brief task-board outage".to_string())
        }

        async fn save(&self, _session_id: &str, _tasks: Vec<SessionTask>) -> Result<(), String> {
            Ok(())
        }

        async fn next_task_id(&self, _session_id: &str) -> Result<u32, String> {
            Ok(1)
        }

        async fn peek_next_task_id(&self, _session_id: &str) -> Result<u32, String> {
            Ok(1)
        }
    }

    let exe = test_executor().with_shared_task_manager(Arc::new(TaskManager::new(
        "brief-fail",
        Arc::new(FailingTaskStore),
    )));

    let result = exe.brief(&json!({"focus": "tasks"})).await;
    let parsed: Value = serde_json::from_str(&result).unwrap();

    assert_eq!(parsed["tasks"]["available"], false, "{parsed}");
    assert!(
        parsed["tasks"]["error"]
            .as_str()
            .unwrap_or_default()
            .contains("simulated brief task-board outage"),
        "brief(focus=tasks) must surface task-board load failure: {parsed}"
    );
    assert!(
        parsed["tasks"].get("count").is_none(),
        "brief(focus=tasks) must not report count=0 when task board is unreadable: {parsed}"
    );
}
