use serde_json::Value;

/// Public actions exposed by the model-facing `memory` schema.
pub const MEMORY_ACTIONS: &[&str] = &[
    "remember",
    "recall",
    "inventory",
    "expand",
    "forget",
    "update",
    "focus",
    "reflect",
    "profile",
    "feedback",
];

pub const MEMORY_ACTIONS_DISPLAY: &str =
    "remember, recall, inventory, expand, forget, update, focus, reflect, profile, feedback";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemoryAction {
    Remember,
    Recall,
    Inventory,
    Expand,
    Forget,
    Update,
    Focus,
    Reflect,
    Profile,
    Feedback,
}

impl MemoryAction {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Remember => "remember",
            Self::Recall => "recall",
            Self::Inventory => "inventory",
            Self::Expand => "expand",
            Self::Forget => "forget",
            Self::Update => "update",
            Self::Focus => "focus",
            Self::Reflect => "reflect",
            Self::Profile => "profile",
            Self::Feedback => "feedback",
        }
    }
}

pub fn memory_missing_action_message() -> String {
    format!(
        "missing required parameter `action` for `memory`. Retry the same `memory` tool with action set to one of: {MEMORY_ACTIONS_DISPLAY}."
    )
}

pub fn memory_action_type_message() -> &'static str {
    "field `action` for `memory` must be a string"
}

pub fn memory_unknown_action_message(action: &str) -> String {
    format!("unknown `memory` action '{action}'. Use one of: {MEMORY_ACTIONS_DISPLAY}.")
}

pub fn memory_action_from_args(args: &Value) -> Result<MemoryAction, String> {
    match args.get("action") {
        Some(Value::String(action)) if !action.trim().is_empty() => match action.as_str() {
            "remember" => Ok(MemoryAction::Remember),
            "recall" => Ok(MemoryAction::Recall),
            "inventory" => Ok(MemoryAction::Inventory),
            "expand" => Ok(MemoryAction::Expand),
            "forget" => Ok(MemoryAction::Forget),
            "update" => Ok(MemoryAction::Update),
            "focus" => Ok(MemoryAction::Focus),
            "reflect" => Ok(MemoryAction::Reflect),
            "profile" => Ok(MemoryAction::Profile),
            "feedback" => Ok(MemoryAction::Feedback),
            other => Err(memory_unknown_action_message(other)),
        },
        Some(Value::String(_)) | None => Err(memory_missing_action_message()),
        Some(_) => Err(memory_action_type_message().to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn action_contract_matches_schema_order() {
        let parsed_actions = MEMORY_ACTIONS
            .iter()
            .copied()
            .map(|action| {
                memory_action_from_args(&json!({"action": action}))
                    .expect("schema action must parse")
                    .as_str()
            })
            .collect::<Vec<_>>();
        assert_eq!(parsed_actions, MEMORY_ACTIONS);
    }

    #[test]
    fn action_parser_rejects_missing_blank_wrong_type_and_unknown() {
        let missing = memory_action_from_args(&json!({})).expect_err("missing action must fail");
        assert!(missing.contains("missing required parameter `action`"));
        assert!(missing.contains(MEMORY_ACTIONS_DISPLAY));

        let blank =
            memory_action_from_args(&json!({"action": ""})).expect_err("blank action must fail");
        assert_eq!(blank, missing);

        let wrong_type =
            memory_action_from_args(&json!({"action": 7})).expect_err("wrong type must fail");
        assert_eq!(wrong_type, memory_action_type_message());

        let unknown =
            memory_action_from_args(&json!({"action": "store"})).expect_err("unknown must fail");
        assert!(unknown.contains("unknown `memory` action 'store'"));
    }
}
