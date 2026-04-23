use super::*;

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
    for required in ["model", "api_key", "output_limit"] {
        assert!(
            surface.iter().any(|s| s == required),
            "expected canonical setting `{required}` in list — got: {surface:?}"
        );
    }
}

#[test]
fn config_get_model() {
    let exe = test_executor();
    let result = exe.config_tool(&json!({ "setting": "model" }));
    let parsed: Value = serde_json::from_str(&result).unwrap();

    assert_eq!(parsed.get("setting").unwrap(), "model");
    assert!(parsed.get("value").is_some());
}

#[test]
fn config_get_api_key_status() {
    let exe = test_executor();
    let result = exe.config_tool(&json!({ "setting": "api_key" }));
    let parsed: Value = serde_json::from_str(&result).unwrap();

    // Security: must never leak actual key material in any form.
    assert!(!result.contains("sk-"), "must not leak OpenAI-style key prefix");
    assert!(
        !result.to_lowercase().contains("bearer "),
        "must not emit bearer-token shape"
    );

    assert_eq!(parsed["setting"], "api_key");
    let status = parsed["status"]
        .as_object()
        .expect("status must be an object mapping provider env var → state");

    // Must cover every canonical provider env var so the UI can render a full
    // matrix, not just the one that happens to be set.
    for key in ["MO_API_KEY", "OPENAI_API_KEY", "ANTHROPIC_API_KEY"] {
        let v = status
            .get(key)
            .and_then(|v| v.as_str())
            .unwrap_or_else(|| panic!("status.{key} must be a string state — got: {parsed}"));
        assert!(
            v == "set" || v == "not set",
            "status.{key} must be a canonical state ('set'/'not set') — got: {v:?}"
        );
    }
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

#[test]
fn brief_includes_session_state() {
    let exe = test_executor();
    let result = exe.brief(&json!({}));
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
    exe.task_create(&json!({"title": "Implement thing"})).await;
    let result = exe.brief(&json!({"focus": "tasks"}));
    let parsed: Value = serde_json::from_str(&result).unwrap();

    assert_eq!(parsed["tasks"]["count"], 1);
    assert_eq!(parsed["tasks"]["items"][0]["title"], "Implement thing");
}
