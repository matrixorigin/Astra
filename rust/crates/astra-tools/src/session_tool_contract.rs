use serde_json::Value;

/// Public actions exposed by the model-facing `session` schema.
///
/// Keep this list in schema order. Plan lifecycle, rollback, compression, and
/// user prompts are top-level tools when their providers expose them; they are
/// not hidden `session` sub-actions.
pub const SESSION_ACTIONS: &[&str] = &[
    "config",
    "sleep",
    "history_page",
    "history_search",
    "history_around",
];

pub const SESSION_ACTIONS_DISPLAY: &str =
    "config, sleep, history_page, history_search, history_around";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionAction {
    Config,
    Sleep,
    HistoryPage,
    HistorySearch,
    HistoryAround,
}

impl SessionAction {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Config => "config",
            Self::Sleep => "sleep",
            Self::HistoryPage => "history_page",
            Self::HistorySearch => "history_search",
            Self::HistoryAround => "history_around",
        }
    }
}

pub fn session_missing_action_message() -> String {
    format!(
        "missing required parameter `action` for `session`. Retry the same `session` tool with action set to one of: {SESSION_ACTIONS_DISPLAY}."
    )
}

pub fn session_action_type_message() -> &'static str {
    "field `action` for `session` must be a string"
}

pub fn session_unknown_action_message(action: &str) -> String {
    match action {
        "enter_plan" => {
            "unknown `session` action 'enter_plan'. Plan lifecycle is a top-level tool flow; call `enter_plan_mode` only when it is visible in the current tool list.".to_string()
        }
        "exit_plan" => {
            "unknown `session` action 'exit_plan'. Plan lifecycle is a top-level tool flow; call `exit_plan_mode` only when it is visible in the current tool list.".to_string()
        }
        _ => format!(
            "unknown `session` action '{action}'. Use one of: {SESSION_ACTIONS_DISPLAY}. Do not wrap plan, rollback, compression, or user-prompt tools inside `session`; call a top-level tool only when it is visible in the current tool list."
        ),
    }
}

pub fn session_action_from_args(args: &Value) -> Result<SessionAction, String> {
    match args.get("action") {
        Some(Value::String(action)) if !action.trim().is_empty() => match action.as_str() {
            "config" => Ok(SessionAction::Config),
            "sleep" => Ok(SessionAction::Sleep),
            "history_page" => Ok(SessionAction::HistoryPage),
            "history_search" => Ok(SessionAction::HistorySearch),
            "history_around" => Ok(SessionAction::HistoryAround),
            other => Err(session_unknown_action_message(other)),
        },
        Some(Value::String(_)) | None => Err(session_missing_action_message()),
        Some(_) => Err(session_action_type_message().to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn action_contract_is_schema_surface_only() {
        assert_eq!(
            SESSION_ACTIONS,
            &[
                "config",
                "sleep",
                "history_page",
                "history_search",
                "history_around"
            ]
        );
        for hidden_action in [
            "enter_plan",
            "exit_plan",
            "rollback",
            "compress",
            "ask_user",
        ] {
            assert!(
                !SESSION_ACTIONS.contains(&hidden_action),
                "{hidden_action} must remain a top-level tool/provider decision, not a session sub-action"
            );
        }
        let parsed_actions = SESSION_ACTIONS
            .iter()
            .copied()
            .map(|action| {
                session_action_from_args(&json!({"action": action}))
                    .expect("schema action must parse")
                    .as_str()
            })
            .collect::<Vec<_>>();
        assert_eq!(
            parsed_actions, SESSION_ACTIONS,
            "parser enum and schema action list must stay byte-order aligned"
        );
    }

    #[test]
    fn action_parser_rejects_missing_blank_and_wrong_type() {
        let missing = session_action_from_args(&json!({})).expect_err("missing action must fail");
        assert!(missing.contains("missing required parameter `action`"));
        assert!(missing.contains(SESSION_ACTIONS_DISPLAY));

        let blank =
            session_action_from_args(&json!({"action": ""})).expect_err("blank action must fail");
        assert_eq!(blank, missing);

        let wrong_type = session_action_from_args(&json!({"action": 7}))
            .expect_err("non-string action must fail");
        assert_eq!(wrong_type, session_action_type_message());
    }

    #[test]
    fn stale_plan_actions_redirect_without_reopening_session_contract() {
        let enter = session_unknown_action_message("enter_plan");
        assert!(enter.contains("unknown `session` action 'enter_plan'"));
        assert!(enter.contains("enter_plan_mode"));

        let exit = session_unknown_action_message("exit_plan");
        assert!(exit.contains("unknown `session` action 'exit_plan'"));
        assert!(exit.contains("exit_plan_mode"));
    }
}
