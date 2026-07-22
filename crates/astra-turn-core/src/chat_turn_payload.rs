//! Top-level `/chat` (streaming) JSON body skeleton before `edge_tools`, `tool_results`, and selector hints.

use std::collections::HashSet;
use std::path::Path;

use serde_json::{Value, json};

use crate::chat_turn_edge_profile::build_base_edge_profile_value;
use crate::chat_turn_explain_wire::chat_turn_explain_field_json;
use crate::edge_prompt_context::detect_workspace_context;
use crate::tool::schema::prune::filter_tool_schemas_by_excluded_names;

/// Inputs for [`chat_turn_base_payload`] (keeps the arity aligned with the JSON body without a 9-arg function).
pub struct ChatTurnBasePayloadInput<'a> {
    pub messages: &'a [Value],
    pub user_intent: Option<&'a str>,
    pub session_id: Option<&'a str>,
    pub agent_id: Option<&'a str>,
    pub inference_purpose: astra_turn_types::InferencePurpose,
    /// Producer-owned index of the current model invocation within the
    /// agentic turn. Durable inference admission uses this coordinate to
    /// distinguish adjacent tool rounds while keeping transport retries of
    /// the same round idempotent.
    pub round_index: u32,
    pub offering_id: Option<&'a str>,
    pub interaction_mode: Option<&'a str>,
    pub explain_verbose: bool,
    pub explain_on: bool,
    pub edge_executor_id: &'a str,
    pub capabilities: Vec<String>,
    pub project_root: &'a Path,
    pub git_branch: Option<String>,
    /// Thinking/reasoning configuration for extended thinking models.
    pub thinking: crate::thinking_config::ThinkingConfig,
}

/// Invariant fields for `POST /chat/stream` (or equivalent) before dynamic tool schemas and callbacks.
///
/// `edge_executor_id` and `capabilities` are fields so server-side builders are not tied to CLI env.
#[must_use]
pub fn chat_turn_base_payload(input: ChatTurnBasePayloadInput<'_>) -> Value {
    let ChatTurnBasePayloadInput {
        messages,
        user_intent,
        session_id,
        agent_id,
        inference_purpose,
        round_index,
        offering_id,
        interaction_mode,
        explain_verbose,
        explain_on,
        edge_executor_id,
        capabilities,
        project_root,
        git_branch,
        thinking,
    } = input;
    let mut payload = json!({
        "messages": messages,
        "user_intent": user_intent,
        "session_id": session_id,
        "agent_id": agent_id,
        "inference_purpose": inference_purpose,
        "round_index": round_index,
        "interaction_mode": interaction_mode,
        "explain": chat_turn_explain_field_json(explain_verbose, explain_on),
        "edge_executor_id": edge_executor_id,
        "capabilities": capabilities,
        "edge_profile": build_base_edge_profile_value(
            project_root.to_string_lossy().as_ref(),
            git_branch,
            detect_workspace_context(project_root),
        ),
    });
    if let Some(offering_id) = offering_id
        && let Some(obj) = payload.as_object_mut()
    {
        obj.insert(
            "model_selection".to_string(),
            json!({"offering_id": offering_id}),
        );
    }
    if thinking.is_enabled() {
        if let Some(obj) = payload.as_object_mut() {
            obj.insert("thinking".to_string(), thinking.to_payload_value());
        }
    }
    payload
}

/// Project producer-owned system skill identities into `edge_profile.active_skills`.
pub fn merge_active_skills_into_edge_profile(payload: &mut Value, active_skills: &[String]) {
    if active_skills.is_empty() {
        return;
    }
    if let Some(root) = payload.as_object_mut()
        && let Some(ep) = root.get_mut("edge_profile")
        && let Some(ep_obj) = ep.as_object_mut()
    {
        ep_obj.insert("active_skills".to_string(), json!(active_skills));
    }
}

/// Deduped registry skill names that affected this `/chat` request: selector-chosen
/// skills and skills whose instruction bodies were merged successfully.
pub fn merge_invoked_skills_into_edge_profile(payload: &mut Value, invoked_skills: &[String]) {
    if invoked_skills.is_empty() {
        return;
    }
    if let Some(root) = payload.as_object_mut()
        && let Some(ep) = root.get_mut("edge_profile")
        && let Some(ep_obj) = ep.as_object_mut()
    {
        ep_obj.insert("invoked_skills".to_string(), json!(invoked_skills));
    }
}

/// Shallow-merge top-level keys from `extensions` into `edge_profile` (cloud–edge audit / lineage).
///
/// `extensions` must be a JSON object. Non-object values are ignored.
pub fn merge_edge_profile_extensions(payload: &mut Value, extensions: &Value) {
    let Some(ext_obj) = extensions.as_object() else {
        return;
    };
    if ext_obj.is_empty() {
        return;
    }
    if let Some(root) = payload.as_object_mut()
        && let Some(ep) = root.get_mut("edge_profile")
        && let Some(ep_obj) = ep.as_object_mut()
    {
        for (k, v) in ext_obj {
            ep_obj.insert(k.clone(), v.clone());
        }
    }
}

/// Dynamic tool schemas for this turn (`edge_tools`).
pub fn set_payload_edge_tools(payload: &mut Value, schemas: Vec<Value>) {
    if let Some(obj) = payload.as_object_mut() {
        obj.insert("edge_tools".to_string(), Value::Array(schemas));
    }
}

/// Drop schemas whose `function.name` is in `restricted_tools`, then set `edge_tools` on the payload.
pub fn attach_filtered_edge_tools(
    payload: &mut Value,
    turn_schemas: Vec<Value>,
    restricted_tools: &HashSet<String>,
) {
    let final_schemas = filter_tool_schemas_by_excluded_names(turn_schemas, restricted_tools);
    set_payload_edge_tools(payload, final_schemas);
}

fn canonical_tool_result_wire_row(row: &Value) -> Value {
    let Some(row) = row.as_object() else {
        return json!({
            "request_id": null,
            "status": "failed",
            "output": "invalid non-object tool result",
        });
    };

    let mut wire = row.clone();
    let request_id = wire
        .remove("request_id")
        .or_else(|| wire.remove("tool_call_id"))
        .unwrap_or(Value::Null);
    let output = wire
        .remove("output")
        .or_else(|| wire.remove("result"))
        .unwrap_or(Value::Null);
    let explicit_error = wire.get("error").is_some_and(|error| {
        !error.is_null() && error.as_str().is_none_or(|text| !text.is_empty())
    }) || wire.remove("is_error").and_then(|value| value.as_bool())
        == Some(true);
    let status = if explicit_error {
        "failed"
    } else {
        match wire.remove("status") {
            Some(Value::String(status)) => match status.trim().to_ascii_lowercase().as_str() {
                "completed" => "completed",
                "failed" => "failed",
                "skipped" => "skipped",
                _ => "failed",
            },
            Some(_) => "failed",
            None if !output.is_string() => "failed",
            None => crate::tool_result_semantics::cloud_tool_result_status_label(
                output.as_str().expect("string checked above"),
            ),
        }
    };
    wire.insert("request_id".to_string(), request_id);
    wire.insert("status".to_string(), Value::String(status.to_string()));
    wire.insert("output".to_string(), output);
    Value::Object(wire)
}

/// Attach canonical callback-style `tool_results` to the outbound `/chat/turn` payload.
///
/// Agentic-loop state stores model-facing `tool_call_id`/`result` rows. The
/// transport boundary rewrites those rows to the sole wire contract:
/// `request_id`/`status`/`output`.
pub fn set_payload_tool_results_if_non_empty(payload: &mut Value, rows: &[Value]) {
    if rows.is_empty() {
        return;
    }
    if let Some(obj) = payload.as_object_mut() {
        obj.insert(
            "tool_results".to_string(),
            Value::Array(rows.iter().map(canonical_tool_result_wire_row).collect()),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;
    use std::path::Path;

    #[test]
    fn base_payload_core_fields() {
        let msgs = vec![json!({"role": "user", "content": "hi"})];
        let p = chat_turn_base_payload(ChatTurnBasePayloadInput {
            messages: &msgs,
            user_intent: Some("hi"),
            session_id: None,
            agent_id: Some("test-agent"),
            inference_purpose: astra_turn_types::InferencePurpose::SubAgent,
            round_index: 7,
            offering_id: Some("offer-gpt-test"),
            interaction_mode: Some("auto"),
            explain_verbose: false,
            explain_on: true,
            edge_executor_id: "edge-unit",
            capabilities: vec!["bash".into(), "fs".into()],
            project_root: Path::new("/tmp"),
            git_branch: Some("main".into()),
            thinking: crate::thinking_config::ThinkingConfig::Off,
        });
        assert_eq!(p["messages"], json!(msgs));
        assert_eq!(p["user_intent"], "hi");
        assert_eq!(p["session_id"], Value::Null);
        assert_eq!(p["agent_id"], "test-agent");
        assert_eq!(p["inference_purpose"], "sub_agent");
        assert_eq!(p["round_index"], 7);
        assert!(p.get("model").is_none());
        assert_eq!(p["model_selection"]["offering_id"], "offer-gpt-test");
        assert_eq!(p["interaction_mode"], "auto");
        assert_eq!(p["explain"], json!(true));
        assert_eq!(p["edge_executor_id"], "edge-unit");
        let caps = p["capabilities"].as_array().unwrap();
        assert!(caps.contains(&json!("bash")));
        assert_eq!(p["edge_profile"]["cwd"], "/tmp");
        assert_eq!(p["edge_profile"]["git_branch"], "main");
        assert!(p["edge_profile"].get("memoria_url").is_none());
        assert!(
            p.get("runtime_bindings").is_none(),
            "request payloads must not carry execution endpoints or credentials"
        );
        // thinking = Off → field absent
        assert!(p.get("thinking").is_none());
    }

    #[test]
    fn base_payload_session_id_some() {
        let p = chat_turn_base_payload(ChatTurnBasePayloadInput {
            messages: &[],
            user_intent: None,
            session_id: Some("sess-1"),
            agent_id: None,
            inference_purpose: astra_turn_types::InferencePurpose::PrimaryAgent,
            round_index: 0,
            offering_id: None,
            interaction_mode: None,
            explain_verbose: true,
            explain_on: false,
            edge_executor_id: "e",
            capabilities: vec![],
            project_root: Path::new("/"),
            git_branch: None,
            thinking: crate::thinking_config::ThinkingConfig::Off,
        });
        assert_eq!(p["session_id"], "sess-1");
        assert_eq!(p["agent_id"], Value::Null);
        assert_eq!(p["interaction_mode"], Value::Null);
        assert_eq!(p["explain"], json!("verbose"));
    }

    #[test]
    fn base_payload_thinking_included_when_enabled() {
        let p = chat_turn_base_payload(ChatTurnBasePayloadInput {
            messages: &[],
            user_intent: None,
            session_id: None,
            agent_id: None,
            inference_purpose: astra_turn_types::InferencePurpose::PrimaryAgent,
            round_index: 0,
            offering_id: None,
            interaction_mode: Some("non_interactive"),
            explain_verbose: false,
            explain_on: false,
            edge_executor_id: "e",
            capabilities: vec![],
            project_root: Path::new("/"),
            git_branch: None,
            thinking: crate::thinking_config::ThinkingConfig::Enabled {
                budget_tokens: 10000,
            },
        });
        assert_eq!(p["thinking"]["mode"], "enabled");
        assert_eq!(p["thinking"]["budget_tokens"], 10000);
    }

    #[test]
    fn base_payload_thinking_absent_when_off() {
        let p = chat_turn_base_payload(ChatTurnBasePayloadInput {
            messages: &[],
            user_intent: None,
            session_id: None,
            agent_id: None,
            inference_purpose: astra_turn_types::InferencePurpose::PrimaryAgent,
            round_index: 0,
            offering_id: None,
            interaction_mode: None,
            explain_verbose: false,
            explain_on: false,
            edge_executor_id: "e",
            capabilities: vec![],
            project_root: Path::new("/"),
            git_branch: None,
            thinking: crate::thinking_config::ThinkingConfig::Off,
        });
        assert!(p.get("thinking").is_none());
    }

    #[test]
    fn merge_active_skills_into_edge_profile_inserts_array() {
        let mut p = json!({ "edge_profile": {} });
        merge_active_skills_into_edge_profile(
            &mut p,
            &["markdown".to_string(), "concise".to_string()],
        );
        assert_eq!(
            p["edge_profile"]["active_skills"],
            json!(["markdown", "concise"])
        );
    }

    #[test]
    fn merge_active_skills_no_op_when_empty() {
        let mut p = json!({ "edge_profile": {} });
        merge_active_skills_into_edge_profile(&mut p, &[]);
        assert!(
            p["edge_profile"]
                .as_object()
                .unwrap()
                .get("active_skills")
                .is_none()
        );
    }

    #[test]
    fn merge_invoked_skills_into_edge_profile_inserts_array() {
        let mut p = json!({ "edge_profile": {} });
        merge_invoked_skills_into_edge_profile(&mut p, &["markdown".into(), "bash".into()]);
        assert_eq!(
            p["edge_profile"]["invoked_skills"],
            json!(["markdown", "bash"])
        );
    }

    #[test]
    fn merge_invoked_skills_no_op_when_empty() {
        let mut p = json!({ "edge_profile": {} });
        merge_invoked_skills_into_edge_profile(&mut p, &[]);
        assert!(
            p["edge_profile"]
                .as_object()
                .unwrap()
                .get("invoked_skills")
                .is_none()
        );
    }

    #[test]
    fn merge_edge_profile_extensions_merges_objects() {
        let mut p = json!({ "edge_profile": { "cwd": "/tmp", "k": 1 } });
        merge_edge_profile_extensions(
            &mut p,
            &json!({
                "session_lineage": { "parent_session_id": "abc" },
                "edge_policy": { "permission_mode": "prompt" }
            }),
        );
        assert_eq!(p["edge_profile"]["cwd"], "/tmp");
        assert_eq!(p["edge_profile"]["k"], 1);
        assert_eq!(
            p["edge_profile"]["session_lineage"]["parent_session_id"],
            "abc"
        );
        assert_eq!(
            p["edge_profile"]["edge_policy"]["permission_mode"],
            "prompt"
        );
    }

    #[test]
    fn set_payload_edge_tools_and_tool_results() {
        let mut p = json!({});
        set_payload_edge_tools(&mut p, vec![json!({"fn": "t1"})]);
        assert_eq!(p["edge_tools"], json!([{"fn": "t1"}]));
        set_payload_tool_results_if_non_empty(
            &mut p,
            &[json!({
                "tool_call_id": "1",
                "name": "read_file",
                "result": "contents",
            })],
        );
        assert_eq!(
            p["tool_results"],
            json!([{
                "request_id": "1",
                "name": "read_file",
                "status": "completed",
                "output": "contents",
            }])
        );
    }

    #[test]
    fn tool_result_wire_conversion_fails_closed_on_alias_statuses_and_bad_rows() {
        let mut payload = json!({});
        set_payload_tool_results_if_non_empty(
            &mut payload,
            &[
                json!({"request_id": "canonical", "status": "skipped", "output": "deduped"}),
                json!({"tool_call_id": "legacy-status", "status": "success", "result": "ok"}),
                json!({"tool_call_id": "error", "result": "Error: denied"}),
                json!({"request_id": "conflict", "status": "completed", "output": "ok", "error": "denied"}),
                json!({"request_id": "object-output", "output": {"ok": true}}),
                Value::String("bad".to_string()),
            ],
        );
        assert_eq!(payload["tool_results"][0]["status"], "skipped");
        assert_eq!(payload["tool_results"][1]["status"], "failed");
        assert_eq!(payload["tool_results"][2]["status"], "failed");
        assert_eq!(payload["tool_results"][3]["status"], "failed");
        assert_eq!(payload["tool_results"][4]["status"], "failed");
        assert_eq!(payload["tool_results"][5]["status"], "failed");
    }

    #[test]
    fn set_payload_tool_results_no_op_when_empty() {
        let mut p = json!({});
        set_payload_tool_results_if_non_empty(&mut p, &[]);
        assert!(p.get("tool_results").is_none());
    }

    #[test]
    fn attach_filtered_edge_tools_excludes_by_name() {
        let mut p = json!({});
        let schemas = vec![
            json!({"function": {"name": "bash"}}),
            json!({"function": {"name": "danger"}}),
        ];
        let mut r = HashSet::new();
        r.insert("danger".into());
        attach_filtered_edge_tools(&mut p, schemas, &r);
        let arr = p["edge_tools"].as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["function"]["name"], "bash");
    }
}
