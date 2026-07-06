use serde_json::Value;

/// Public checklist actions exposed by the model-facing `task` schema.
///
/// Keep this list in schema order. Executors must not accept extra aliases:
/// if the schema did not advertise an action, accepting it in one runtime
/// creates a hidden server/CLI semantic split.
pub const TASK_ACTIONS: &[&str] = &[
    "create",
    "update",
    "list",
    "get",
    "stop",
    "list_user",
    "adopt",
    "archive",
];

pub const TASK_ACTIONS_DISPLAY: &str = "create, update, list, get, stop, list_user, adopt, archive";

pub fn task_missing_action_message() -> String {
    format!(
        "missing required parameter `action` for `task`. Retry the same `task` tool with action set to one of: {TASK_ACTIONS_DISPLAY}."
    )
}

pub fn task_action_type_message() -> &'static str {
    "field 'action' for `task` must be a string"
}

pub fn task_unknown_action_message(action: &str) -> String {
    format!("unknown `task` action '{action}'. Use one of: {TASK_ACTIONS_DISPLAY}.")
}

pub fn task_action_from_args(args: &Value) -> Result<&str, String> {
    match args.get("action") {
        Some(Value::String(action)) => Ok(action.as_str()),
        Some(_) => Err(task_action_type_message().to_string()),
        None => Err(task_missing_action_message()),
    }
}

pub fn task_action_allowed_fields(action: &str) -> Option<&'static [&'static str]> {
    match action {
        "create" => Some(&[
            "action",
            "title",
            "description",
            "subtasks",
            "active_form",
            "owner",
            "metadata",
            "add_blocks",
            "add_blocked_by",
        ]),
        "update" => Some(&[
            "action",
            "task_id",
            "new_status",
            "title",
            "description",
            "subtask_id",
            "active_form",
            "owner",
            "metadata",
            "add_blocks",
            "add_blocked_by",
            "remove_blocks",
            "remove_blocked_by",
            "reason",
            "error_message",
        ]),
        "list" => Some(&["action", "status_filter"]),
        "get" => Some(&["action", "task_id"]),
        "stop" => Some(&["action", "task_id", "reason"]),
        "list_user" => Some(&["action", "user_status"]),
        "adopt" => Some(&["action", "source_session_id", "task_id"]),
        "archive" => Some(&["action", "task_id", "older_than_days", "reason"]),
        _ => None,
    }
}

pub fn task_actions_allowing_field(field: &str, current_action: &str) -> Vec<&'static str> {
    TASK_ACTIONS
        .iter()
        .copied()
        .filter(|action| *action != current_action)
        .filter(|action| {
            task_action_allowed_fields(action).is_some_and(|allowed| allowed.contains(&field))
        })
        .collect()
}

pub fn unknown_task_field_message(action: &str, key: &str, allowed: &[&str]) -> String {
    let other_actions = task_actions_allowing_field(key, action);
    let action_hint = if other_actions.is_empty() {
        String::new()
    } else {
        format!(
            "; field is valid for: {}",
            other_actions
                .iter()
                .map(|action| format!("task.{action}"))
                .collect::<Vec<_>>()
                .join(", ")
        )
    };
    format!(
        "unknown field '{key}' for task.{action} (valid: {}{})",
        allowed.join(", "),
        action_hint
    )
}

fn validate_task_tool_args_for_action_impl(
    action: &str,
    args: &Value,
    allow_runtime_private_fields: bool,
) -> Result<(), String> {
    let Some(allowed) = task_action_allowed_fields(action) else {
        return Ok(());
    };
    let Some(obj) = args.as_object() else {
        return Err(format!("task.{action} arguments must be an object"));
    };
    for key in obj.keys() {
        if allow_runtime_private_fields && key.starts_with('_') {
            continue;
        }
        if !allowed.contains(&key.as_str()) {
            return Err(unknown_task_field_message(action, key, allowed));
        }
    }
    if let Some(action_value) = obj.get("action")
        && !action_value.is_string()
    {
        return Err(task_action_type_message().to_string());
    }
    Ok(())
}

pub fn validate_public_task_tool_args_for_action(action: &str, args: &Value) -> Result<(), String> {
    validate_task_tool_args_for_action_impl(action, args, false)
}

pub fn validate_runtime_task_tool_args_for_action(
    action: &str,
    args: &Value,
) -> Result<(), String> {
    validate_task_tool_args_for_action_impl(action, args, true)
}

pub fn strip_runtime_private_task_fields(args: &Value) -> Value {
    let Some(obj) = args.as_object() else {
        return args.clone();
    };
    let mut public = obj.clone();
    public.retain(|key, _| !key.starts_with('_'));
    Value::Object(public)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn action_contract_is_schema_surface_only() {
        assert_eq!(
            TASK_ACTIONS,
            &[
                "create",
                "update",
                "list",
                "get",
                "stop",
                "list_user",
                "adopt",
                "archive"
            ]
        );
        assert!(
            !TASK_ACTIONS.contains(&"cancel"),
            "task cancellation is expressed by action=stop, not a hidden action alias"
        );
    }

    #[test]
    fn missing_action_message_is_retryable_invalid_args_contract() {
        let err = task_action_from_args(&json!({})).expect_err("missing action must fail");
        assert!(err.contains("missing required parameter `action`"));
        assert!(err.contains("Retry the same `task` tool"));
        assert!(err.contains(TASK_ACTIONS_DISPLAY));
    }

    #[test]
    fn validates_unknown_fields_with_action_owner_hint() {
        let err = validate_public_task_tool_args_for_action(
            "update",
            &json!({"action": "update", "task_id": "task-1", "subtasks": []}),
        )
        .expect_err("create-only field must fail on update");
        assert!(err.contains("unknown field 'subtasks' for task.update"));
        assert!(err.contains("field is valid for: task.create"), "{err}");
    }

    #[test]
    fn rejects_status_alias_consistently() {
        let err = validate_public_task_tool_args_for_action(
            "update",
            &json!({"action": "update", "task_id": "task-1", "status": "completed"}),
        )
        .expect_err("old status alias must not be accepted");
        assert!(err.contains("unknown field 'status'"));
        assert!(err.contains("new_status"), "{err}");
    }

    #[test]
    fn runtime_private_fields_are_transport_only() {
        let args = json!({
            "action": "create",
            "title": "ship",
            "_run_id": "run-1",
            "_tool_call_id": "call-1"
        });
        validate_runtime_task_tool_args_for_action("create", &args)
            .expect("executor may carry runtime-private metadata");
        assert!(
            validate_public_task_tool_args_for_action("create", &args).is_err(),
            "business-layer task args stay public-only"
        );
        let stripped = strip_runtime_private_task_fields(&args);
        assert!(stripped.get("_run_id").is_none());
        assert_eq!(stripped["title"], "ship");
    }
}
