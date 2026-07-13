use serde_json::Value;

pub const AGENT_RUNTIME_TOOL_NAMES: &[&str] = &["agent", "agent_fanout"];
pub const AGENT_ACTIONS: &[&str] = &["spawn", "get_result", "run_chain", "send_message"];
pub const AGENT_ACTIONS_DISPLAY: &str = "spawn, get_result, run_chain, send_message";

pub const AGENT_FANOUT_ACTIONS: &[&str] = &["start", "get_results", "stop_slot"];
pub const AGENT_FANOUT_ACTIONS_DISPLAY: &str = "start, get_results, stop_slot";

pub fn is_agent_runtime_tool(name: &str) -> bool {
    AGENT_RUNTIME_TOOL_NAMES.contains(&name)
}

/// A provider delivered a tool call whose argument envelope could not be
/// decoded. This is deliberately distinct from a valid object that fails the
/// tool schema: the runtime must not infer executable intent from corrupted
/// text.
pub fn has_malformed_tool_args(args: &Value) -> bool {
    args.get("_parse_error").is_some()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentAction {
    Spawn,
    GetResult,
    RunChain,
    SendMessage,
}

impl AgentAction {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Spawn => "spawn",
            Self::GetResult => "get_result",
            Self::RunChain => "run_chain",
            Self::SendMessage => "send_message",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentFanoutAction {
    Start,
    GetResults,
    StopSlot,
}

impl AgentFanoutAction {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Start => "start",
            Self::GetResults => "get_results",
            Self::StopSlot => "stop_slot",
        }
    }
}

pub fn agent_missing_action_message() -> String {
    format!(
        "missing required parameter `action` for `agent`. Provide a JSON object with `action` set to one of {AGENT_ACTIONS_DISPLAY}. For a child agent use {{\"action\":\"spawn\",\"description\":\"…\",\"prompt\":\"…\"}}; do not wrap arguments under `spawn` or pass `agents:[...]`."
    )
}

pub fn agent_action_type_message() -> &'static str {
    "field `action` for `agent` must be a string"
}

pub fn agent_unknown_action_message(action: &str) -> String {
    format!("unknown `agent` action '{action}'. Use one of: {AGENT_ACTIONS_DISPLAY}.")
}

pub fn invalid_agent_spawn_wrapper_message() -> &'static str {
    "invalid agent call shape. Use a top-level JSON `action` field, not a `spawn` wrapper key. For example: {\"action\":\"spawn\",\"description\":\"…\",\"prompt\":\"…\"}. For parallel fan-out, use the `agent_fanout` JSON schema; do not pass `agents:[...]`."
}

pub fn invalid_agent_agents_payload_message() -> &'static str {
    "unsupported `agents` batch payload for `agent`. Each `agent` call launches exactly one child. Use the `agent_fanout` JSON schema for atomic parallel fan-out."
}

pub fn agent_action_from_args(args: &Value) -> Result<AgentAction, String> {
    reject_malformed_tool_args("agent", args)?;
    match args.get("action") {
        Some(Value::String(action)) if !action.trim().is_empty() => match action.as_str() {
            "spawn" => Ok(AgentAction::Spawn),
            "get_result" => Ok(AgentAction::GetResult),
            "run_chain" => Ok(AgentAction::RunChain),
            "send_message" => Ok(AgentAction::SendMessage),
            other => Err(agent_unknown_action_message(other)),
        },
        Some(Value::String(_)) | None if args.get("spawn").is_some() => {
            Err(invalid_agent_spawn_wrapper_message().to_string())
        }
        Some(Value::String(_)) | None if args.get("agents").is_some() => {
            Err(invalid_agent_agents_payload_message().to_string())
        }
        Some(Value::String(_)) | None => Err(agent_missing_action_message()),
        Some(_) => Err(agent_action_type_message().to_string()),
    }
}

pub fn agent_fanout_missing_action_message() -> String {
    format!(
        "missing required parameter `action` for `agent_fanout`. Provide one JSON object with `action` set to one of {AGENT_FANOUT_ACTIONS_DISPLAY}; follow the advertised schema exactly."
    )
}

pub fn agent_fanout_action_type_message() -> &'static str {
    "field `action` for `agent_fanout` must be a string"
}

pub fn agent_fanout_unknown_action_message(action: &str) -> String {
    format!("unknown `agent_fanout` action '{action}'. Use one of: {AGENT_FANOUT_ACTIONS_DISPLAY}.")
}

pub fn agent_fanout_action_from_args(args: &Value) -> Result<AgentFanoutAction, String> {
    reject_malformed_tool_args("agent_fanout", args)?;
    match args.get("action") {
        Some(Value::String(action)) if !action.trim().is_empty() => match action.as_str() {
            "start" => Ok(AgentFanoutAction::Start),
            "get_results" => Ok(AgentFanoutAction::GetResults),
            "stop_slot" => Ok(AgentFanoutAction::StopSlot),
            other => Err(agent_fanout_unknown_action_message(other)),
        },
        Some(Value::String(_)) | None => Err(agent_fanout_missing_action_message()),
        Some(_) => Err(agent_fanout_action_type_message().to_string()),
    }
}

fn reject_malformed_tool_args(tool_name: &str, args: &Value) -> Result<(), String> {
    if !has_malformed_tool_args(args) {
        return Ok(());
    }
    Err(format!(
        "`{tool_name}` arguments were not valid JSON, so the call was not executed. Emit one complete JSON object matching the tool schema; do not serialize arguments as text or include markup."
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn agent_action_contract_matches_schema_order() {
        let parsed_actions = AGENT_ACTIONS
            .iter()
            .copied()
            .map(|action| {
                agent_action_from_args(&json!({"action": action}))
                    .expect("schema action must parse")
                    .as_str()
            })
            .collect::<Vec<_>>();
        assert_eq!(parsed_actions, AGENT_ACTIONS);
    }

    #[test]
    fn agent_fanout_parse_error_is_not_reported_as_missing_action() {
        let raw_payload = "{\"action\":\"start\"";
        let err = agent_fanout_action_from_args(&json!({
            "_parse_error": {"kind": "invalid_json", "executed": false}
        }))
        .unwrap_err();
        assert!(err.contains("agent_fanout"));
        assert!(!err.contains(raw_payload));
    }

    #[test]
    fn agent_runtime_tool_names_are_explicit_contract() {
        assert_eq!(AGENT_RUNTIME_TOOL_NAMES, &["agent", "agent_fanout"]);
        assert!(is_agent_runtime_tool("agent"));
        assert!(is_agent_runtime_tool("agent_fanout"));
        assert!(!is_agent_runtime_tool("task_board"));
    }

    #[test]
    fn agent_fanout_action_contract_matches_schema_order() {
        let parsed_actions = AGENT_FANOUT_ACTIONS
            .iter()
            .copied()
            .map(|action| {
                agent_fanout_action_from_args(&json!({"action": action}))
                    .expect("schema action must parse")
                    .as_str()
            })
            .collect::<Vec<_>>();
        assert_eq!(parsed_actions, AGENT_FANOUT_ACTIONS);
    }

    #[test]
    fn agent_parser_rejects_missing_wrong_type_unknown_and_legacy_shapes() {
        let missing = agent_action_from_args(&json!({})).expect_err("missing action must fail");
        assert!(missing.contains("missing required parameter `action`"));
        assert!(missing.contains(AGENT_ACTIONS_DISPLAY));

        let wrong_type =
            agent_action_from_args(&json!({"action": 7})).expect_err("wrong type must fail");
        assert_eq!(wrong_type, agent_action_type_message());

        let unknown =
            agent_action_from_args(&json!({"action": "batch"})).expect_err("unknown must fail");
        assert!(unknown.contains("unknown `agent` action 'batch'"));

        let wrapper = agent_action_from_args(&json!({"spawn": {"prompt": "x"}}))
            .expect_err("wrapper shape must fail");
        assert!(wrapper.contains("top-level `action='spawn'`"));

        let agents =
            agent_action_from_args(&json!({"agents": []})).expect_err("agents payload must fail");
        assert!(agents.contains("unsupported `agents` batch payload"));
    }

    #[test]
    fn agent_fanout_parser_rejects_missing_wrong_type_and_unknown() {
        let missing =
            agent_fanout_action_from_args(&json!({})).expect_err("missing action must fail");
        assert!(missing.contains("missing required parameter `action`"));
        assert!(missing.contains(AGENT_FANOUT_ACTIONS_DISPLAY));

        let wrong_type =
            agent_fanout_action_from_args(&json!({"action": 7})).expect_err("wrong type must fail");
        assert_eq!(wrong_type, agent_fanout_action_type_message());

        let unknown = agent_fanout_action_from_args(&json!({"action": "spawn"}))
            .expect_err("unknown must fail");
        assert!(unknown.contains("unknown `agent_fanout` action 'spawn'"));
    }
}
