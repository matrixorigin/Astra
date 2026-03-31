//! Top-level `/chat` (streaming) JSON body skeleton before `edge_tools`, `tool_results`, and selector hints.

use std::path::Path;

use serde_json::{Value, json};

use super::chat_turn_edge_profile::build_base_edge_profile_value;
use super::chat_turn_explain_wire::chat_turn_explain_field_json;
use super::edge_prompt_context::detect_workspace_context;

/// Inputs for [`chat_turn_base_payload`] (keeps the arity aligned with the JSON body without a 9-arg function).
pub struct ChatTurnBasePayloadInput<'a> {
    pub messages: &'a [Value],
    pub session_id: Option<&'a str>,
    pub model: Option<&'a str>,
    pub explain_verbose: bool,
    pub explain_on: bool,
    pub edge_executor_id: &'a str,
    pub capabilities: Vec<String>,
    pub project_root: &'a Path,
    pub git_branch: Option<String>,
}

/// Invariant fields for `POST /chat/stream` (or equivalent) before dynamic tool schemas and callbacks.
///
/// `edge_executor_id` and `capabilities` are fields so server-side builders are not tied to CLI env.
#[must_use]
pub fn chat_turn_base_payload(input: ChatTurnBasePayloadInput<'_>) -> Value {
    let ChatTurnBasePayloadInput {
        messages,
        session_id,
        model,
        explain_verbose,
        explain_on,
        edge_executor_id,
        capabilities,
        project_root,
        git_branch,
    } = input;
    json!({
        "messages": messages,
        "session_id": session_id,
        "model": model,
        "explain": chat_turn_explain_field_json(explain_verbose, explain_on),
        "edge_executor_id": edge_executor_id,
        "capabilities": capabilities,
        "edge_profile": build_base_edge_profile_value(
            project_root.to_string_lossy().as_ref(),
            git_branch,
            detect_workspace_context(project_root),
        ),
    })
}

/// Set `edge_profile.active_skills` when the edge detected system skill hints in the user message.
pub fn merge_active_skills_into_edge_profile(payload: &mut Value, active_skills: &[&str]) {
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn base_payload_core_fields() {
        let msgs = vec![json!({"role": "user", "content": "hi"})];
        let p = chat_turn_base_payload(ChatTurnBasePayloadInput {
            messages: &msgs,
            session_id: None,
            model: Some("gpt-test"),
            explain_verbose: false,
            explain_on: true,
            edge_executor_id: "edge-unit",
            capabilities: vec!["bash".into(), "fs".into()],
            project_root: Path::new("/tmp"),
            git_branch: Some("main".into()),
        });
        assert_eq!(p["messages"], json!(msgs));
        assert_eq!(p["session_id"], Value::Null);
        assert_eq!(p["model"], "gpt-test");
        assert_eq!(p["explain"], json!(true));
        assert_eq!(p["edge_executor_id"], "edge-unit");
        let caps = p["capabilities"].as_array().unwrap();
        assert!(caps.contains(&json!("bash")));
        assert_eq!(p["edge_profile"]["cwd"], "/tmp");
        assert_eq!(p["edge_profile"]["git_branch"], "main");
        assert!(p["edge_profile"].get("memoria_url").is_some());
    }

    #[test]
    fn base_payload_session_id_some() {
        let p = chat_turn_base_payload(ChatTurnBasePayloadInput {
            messages: &[],
            session_id: Some("sess-1"),
            model: Some("m"),
            explain_verbose: true,
            explain_on: false,
            edge_executor_id: "e",
            capabilities: vec![],
            project_root: Path::new("/"),
            git_branch: None,
        });
        assert_eq!(p["session_id"], "sess-1");
        assert_eq!(p["explain"], json!("verbose"));
    }

    #[test]
    fn merge_active_skills_into_edge_profile_inserts_array() {
        let mut p = json!({ "edge_profile": {} });
        merge_active_skills_into_edge_profile(&mut p, &["markdown", "concise"]);
        assert_eq!(p["edge_profile"]["active_skills"], json!(["markdown", "concise"]));
    }

    #[test]
    fn merge_active_skills_no_op_when_empty() {
        let mut p = json!({ "edge_profile": {} });
        merge_active_skills_into_edge_profile(&mut p, &[]);
        assert!(p["edge_profile"].as_object().unwrap().get("active_skills").is_none());
    }
}
