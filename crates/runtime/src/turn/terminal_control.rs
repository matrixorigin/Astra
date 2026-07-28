use std::collections::HashMap;

use serde_json::{Map, Value, json};
use thiserror::Error;
use uuid::Uuid;

pub(crate) const TERMINAL_HANDOFF_CONTROL_KIND: &str = "moi.control.handoff.v1";

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RuntimeControlToolDescriptor {
    pub public_name: String,
    pub kind: String,
    pub target: String,
    pub fixed_action: Option<String>,
    pub terminal: bool,
    pub policy_id: String,
    pub ui_visibility: Option<String>,
}

impl RuntimeControlToolDescriptor {
    pub(crate) fn from_metadata(
        public_name: &str,
        metadata: Option<&Value>,
    ) -> Result<Option<Self>, RuntimeControlDescriptorError> {
        let Some(metadata) = metadata else {
            return Ok(None);
        };
        let metadata =
            metadata
                .as_object()
                .ok_or_else(|| RuntimeControlDescriptorError::InvalidMetadata {
                    tool: public_name.to_string(),
                    detail: "metadata must be a JSON object".to_string(),
                })?;
        let Some(control) = metadata.get("control") else {
            return Ok(None);
        };
        let control =
            control
                .as_object()
                .ok_or_else(|| RuntimeControlDescriptorError::InvalidMetadata {
                    tool: public_name.to_string(),
                    detail: "metadata.control must be a JSON object".to_string(),
                })?;
        let kind = required_string(control, "kind", public_name)?;
        if kind != TERMINAL_HANDOFF_CONTROL_KIND {
            return Err(RuntimeControlDescriptorError::UnsupportedKind {
                tool: public_name.to_string(),
                kind,
            });
        }
        let terminal = control
            .get("terminal")
            .and_then(Value::as_bool)
            .ok_or_else(|| RuntimeControlDescriptorError::InvalidMetadata {
                tool: public_name.to_string(),
                detail: "metadata.control.terminal must be a boolean".to_string(),
            })?;
        if !terminal {
            return Err(RuntimeControlDescriptorError::NonTerminalHandoff {
                tool: public_name.to_string(),
            });
        }
        let target = required_string(control, "target", public_name)?;
        let fixed_action = optional_string(control, "action", public_name)?;
        let policy_id = required_string(control, "policy_id", public_name)?;
        let ui_visibility = optional_string(control, "ui_visibility", public_name)?;

        Ok(Some(Self {
            public_name: public_name.to_string(),
            kind,
            target,
            fixed_action,
            terminal,
            policy_id,
            ui_visibility,
        }))
    }
}

fn required_string(
    object: &Map<String, Value>,
    field: &str,
    tool: &str,
) -> Result<String, RuntimeControlDescriptorError> {
    let value = object.get(field).and_then(Value::as_str).ok_or_else(|| {
        RuntimeControlDescriptorError::InvalidMetadata {
            tool: tool.to_string(),
            detail: format!("metadata.control.{field} must be a string"),
        }
    })?;
    validate_unpadded_non_empty(value, field, tool)
}

fn optional_string(
    object: &Map<String, Value>,
    field: &str,
    tool: &str,
) -> Result<Option<String>, RuntimeControlDescriptorError> {
    let Some(value) = object.get(field) else {
        return Ok(None);
    };
    let value = value
        .as_str()
        .ok_or_else(|| RuntimeControlDescriptorError::InvalidMetadata {
            tool: tool.to_string(),
            detail: format!("metadata.control.{field} must be a string"),
        })?;
    validate_unpadded_non_empty(value, field, tool).map(Some)
}

fn validate_unpadded_non_empty(
    value: &str,
    field: &str,
    tool: &str,
) -> Result<String, RuntimeControlDescriptorError> {
    if value.is_empty() {
        return Err(RuntimeControlDescriptorError::InvalidMetadata {
            tool: tool.to_string(),
            detail: format!("metadata.control.{field} must not be empty"),
        });
    }
    if value.trim() != value {
        return Err(RuntimeControlDescriptorError::InvalidMetadata {
            tool: tool.to_string(),
            detail: format!("metadata.control.{field} must not contain surrounding whitespace"),
        });
    }
    Ok(value.to_string())
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub(crate) enum RuntimeControlDescriptorError {
    #[error("terminal control descriptor for tool '{tool}' is invalid: {detail}")]
    InvalidMetadata { tool: String, detail: String },
    #[error("terminal control descriptor for tool '{tool}' uses unsupported kind '{kind}'")]
    UnsupportedKind { tool: String, kind: String },
    #[error("terminal handoff descriptor for tool '{tool}' must set terminal=true")]
    NonTerminalHandoff { tool: String },
}

impl RuntimeControlDescriptorError {
    pub(crate) fn error_code(&self) -> &'static str {
        match self {
            Self::UnsupportedKind { .. } => "terminal_handoff_unsupported",
            Self::InvalidMetadata { .. } | Self::NonTerminalHandoff { .. } => {
                "terminal_handoff_contract_violation"
            }
        }
    }
}

#[derive(Debug, Clone, Default)]
pub(crate) struct RuntimeControlToolSnapshot {
    by_public_name: HashMap<String, RuntimeControlToolDescriptor>,
}

impl RuntimeControlToolSnapshot {
    pub(crate) fn new(descriptors: Vec<RuntimeControlToolDescriptor>) -> Self {
        Self {
            by_public_name: descriptors
                .into_iter()
                .map(|descriptor| (descriptor.public_name.clone(), descriptor))
                .collect(),
        }
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.by_public_name.is_empty()
    }

    pub(crate) fn descriptor(&self, public_name: &str) -> Option<&RuntimeControlToolDescriptor> {
        self.by_public_name.get(public_name)
    }

    pub(crate) fn contains(&self, public_name: &str) -> bool {
        self.by_public_name.contains_key(public_name)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TerminalHandoffWindow {
    Open,
    Closed,
    Delegated,
}

impl Default for TerminalHandoffWindow {
    fn default() -> Self {
        Self::Closed
    }
}

impl TerminalHandoffWindow {
    pub(crate) fn for_snapshot(snapshot: &RuntimeControlToolSnapshot) -> Self {
        if snapshot.is_empty() {
            Self::Closed
        } else {
            Self::Open
        }
    }

    pub(crate) fn is_open(self) -> bool {
        self == Self::Open
    }

    pub(crate) fn close(&mut self) {
        if *self == Self::Open {
            *self = Self::Closed;
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StreamedFirstToolAction {
    Ordinary,
    TerminalCandidate,
}

/// Classify the first streamed tool action only once both its public name and
/// complete JSON-object arguments are available. Acceptance remains a
/// whole-response decision because a terminal action must also be the sole
/// action and must not follow user-visible text.
pub(crate) fn classify_complete_streamed_first_tool(
    snapshot: &RuntimeControlToolSnapshot,
    tool_call: &Value,
) -> Option<StreamedFirstToolAction> {
    let name = tool_call_name(tool_call)?;
    tool_call_arguments(tool_call)?;
    Some(if snapshot.contains(name) {
        StreamedFirstToolAction::TerminalCandidate
    } else {
        StreamedFirstToolAction::Ordinary
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TerminalHandoffRequest {
    pub handoff_id: String,
    pub kind: String,
    pub target: String,
    pub action: String,
    pub terminal: bool,
    pub tool_call_id: String,
}

impl TerminalHandoffRequest {
    pub(crate) fn event(&self) -> Value {
        json!({
            "type": "runtime.control.handoff.requested",
            "handoff_id": self.handoff_id,
            "kind": self.kind,
            "target": self.target,
            "action": self.action,
            "terminal": self.terminal,
            "tool_call_id": self.tool_call_id,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TerminalControlRejection {
    pub code: &'static str,
    pub message: String,
    pub tool_call_id: Option<String>,
}

impl TerminalControlRejection {
    pub(crate) fn event(&self) -> Value {
        json!({
            "type": "runtime.control.handoff.rejected",
            "error_code": self.code,
            "message": self.message,
            "tool_call_id": self.tool_call_id,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TerminalControlOutcome {
    Passthrough,
    Requested(TerminalHandoffRequest),
    Rejected(TerminalControlRejection),
}

pub(crate) fn evaluate_terminal_control_actions(
    window: &mut TerminalHandoffWindow,
    snapshot: &RuntimeControlToolSnapshot,
    full_text: &str,
    tool_calls: &[Value],
) -> TerminalControlOutcome {
    if snapshot.is_empty() {
        return TerminalControlOutcome::Passthrough;
    }

    let terminal_calls = tool_calls
        .iter()
        .filter_map(|call| {
            let name = tool_call_name(call)?;
            snapshot.contains(name).then_some(call)
        })
        .collect::<Vec<_>>();

    if *window == TerminalHandoffWindow::Delegated {
        return terminal_calls
            .first()
            .map_or(TerminalControlOutcome::Passthrough, |call| {
                TerminalControlOutcome::Rejected(rejection(
                    "terminal_handoff_contract_violation",
                    "terminal handoff was requested after control had already been delegated",
                    Some(call),
                ))
            });
    }

    if *window == TerminalHandoffWindow::Closed {
        return terminal_calls
            .first()
            .map_or(TerminalControlOutcome::Passthrough, |call| {
                TerminalControlOutcome::Rejected(rejection(
                    "terminal_handoff_window_closed",
                    "terminal handoff must be the source run's first agent action",
                    Some(call),
                ))
            });
    }

    if !full_text.is_empty() {
        *window = TerminalHandoffWindow::Closed;
        return terminal_calls
            .first()
            .map_or(TerminalControlOutcome::Passthrough, |call| {
                TerminalControlOutcome::Rejected(rejection(
                    "terminal_handoff_window_closed",
                    "terminal handoff followed assistant text and was not the first agent action",
                    Some(call),
                ))
            });
    }

    let Some(first_call) = tool_calls.first() else {
        return TerminalControlOutcome::Passthrough;
    };
    let Some(first_name) = tool_call_name(first_call) else {
        *window = TerminalHandoffWindow::Closed;
        return TerminalControlOutcome::Passthrough;
    };
    let Some(descriptor) = snapshot.descriptor(first_name) else {
        *window = TerminalHandoffWindow::Closed;
        return terminal_calls.first().map_or(
            TerminalControlOutcome::Passthrough,
            |call| {
                TerminalControlOutcome::Rejected(rejection(
                    "terminal_handoff_window_closed",
                    "terminal handoff followed an ordinary tool call and was not the first agent action",
                    Some(call),
                ))
            },
        );
    };

    if tool_calls.len() != 1 {
        *window = TerminalHandoffWindow::Closed;
        return TerminalControlOutcome::Rejected(rejection(
            "terminal_handoff_contract_violation",
            "terminal handoff must be the only tool call in its action batch",
            Some(first_call),
        ));
    }

    let request = match terminal_handoff_request(descriptor, first_call) {
        Ok(request) => request,
        Err(rejection) => {
            *window = TerminalHandoffWindow::Closed;
            return TerminalControlOutcome::Rejected(rejection);
        }
    };
    *window = TerminalHandoffWindow::Delegated;
    TerminalControlOutcome::Requested(request)
}

fn terminal_handoff_request(
    descriptor: &RuntimeControlToolDescriptor,
    tool_call: &Value,
) -> Result<TerminalHandoffRequest, TerminalControlRejection> {
    let tool_call_id = tool_call.get("id").and_then(Value::as_str).ok_or_else(|| {
        rejection(
            "terminal_handoff_contract_violation",
            "terminal handoff tool call must include a non-empty id",
            Some(tool_call),
        )
    })?;
    let trimmed_tool_call_id = tool_call_id.trim();
    if trimmed_tool_call_id.is_empty() {
        return Err(rejection(
            "terminal_handoff_contract_violation",
            "terminal handoff tool call must include a non-empty id",
            Some(tool_call),
        ));
    }
    if trimmed_tool_call_id != tool_call_id {
        return Err(rejection(
            "terminal_handoff_contract_violation",
            "terminal handoff tool call id must not contain surrounding whitespace",
            Some(tool_call),
        ));
    }
    let arguments = tool_call_arguments(tool_call).ok_or_else(|| {
        rejection(
            "terminal_handoff_contract_violation",
            "terminal handoff arguments must be a JSON object",
            Some(tool_call),
        )
    })?;
    let action = if let Some(action) = descriptor.fixed_action.as_deref() {
        if !arguments.is_empty() {
            return Err(rejection(
                "terminal_handoff_contract_violation",
                "fixed-action terminal handoff arguments must be an empty object",
                Some(tool_call),
            ));
        }
        action
    } else {
        let action = arguments.get("action").ok_or_else(|| {
            rejection(
                "terminal_handoff_contract_violation",
                "terminal handoff action is required",
                Some(tool_call),
            )
        })?;
        if arguments.len() != 1 {
            return Err(rejection(
                "terminal_handoff_contract_violation",
                "terminal handoff arguments must contain only action",
                Some(tool_call),
            ));
        }
        let action = action.as_str().ok_or_else(|| {
            rejection(
                "terminal_handoff_contract_violation",
                "terminal handoff action must be a non-empty string",
                Some(tool_call),
            )
        })?;
        let trimmed_action = action.trim();
        if trimmed_action.is_empty() {
            return Err(rejection(
                "terminal_handoff_contract_violation",
                "terminal handoff action must be a non-empty string",
                Some(tool_call),
            ));
        }
        if trimmed_action != action {
            return Err(rejection(
                "terminal_handoff_contract_violation",
                "terminal handoff action must not contain surrounding whitespace",
                Some(tool_call),
            ));
        }
        action
    };

    Ok(TerminalHandoffRequest {
        handoff_id: format!("handoff_{}", Uuid::now_v7()),
        kind: descriptor.kind.clone(),
        target: descriptor.target.clone(),
        action: action.to_string(),
        terminal: descriptor.terminal,
        tool_call_id: tool_call_id.to_string(),
    })
}

fn tool_call_name(tool_call: &Value) -> Option<&str> {
    tool_call
        .get("function")
        .and_then(Value::as_object)
        .and_then(|function| function.get("name"))
        .or_else(|| tool_call.get("name"))
        .and_then(Value::as_str)
        .filter(|name| !name.is_empty())
}

fn tool_call_arguments(tool_call: &Value) -> Option<Map<String, Value>> {
    let raw = tool_call
        .get("function")
        .and_then(Value::as_object)
        .and_then(|function| function.get("arguments"))
        .or_else(|| tool_call.get("arguments"))?;
    match raw {
        Value::Object(object) => Some(object.clone()),
        Value::String(text) => serde_json::from_str::<Map<String, Value>>(text).ok(),
        _ => None,
    }
}

fn rejection(
    code: &'static str,
    message: impl Into<String>,
    tool_call: Option<&Value>,
) -> TerminalControlRejection {
    TerminalControlRejection {
        code,
        message: message.into(),
        tool_call_id: tool_call
            .and_then(|call| call.get("id"))
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty() && value.trim() == *value)
            .map(str::to_string),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn descriptor() -> RuntimeControlToolDescriptor {
        RuntimeControlToolDescriptor {
            public_name: "mcp__provider__handoff".to_string(),
            kind: TERMINAL_HANDOFF_CONTROL_KIND.to_string(),
            target: "agent_authoring".to_string(),
            fixed_action: None,
            terminal: true,
            policy_id: "provider.handoff.v1".to_string(),
            ui_visibility: Some("hidden".to_string()),
        }
    }

    fn snapshot() -> RuntimeControlToolSnapshot {
        RuntimeControlToolSnapshot::new(vec![descriptor()])
    }

    fn tool_call(name: &str, id: &str) -> Value {
        json!({
            "id": id,
            "type": "function",
            "function": {
                "name": name,
                "arguments": r#"{"action":"revise_current_agent"}"#,
            },
        })
    }

    fn terminal_tool_call(id: Option<Value>, arguments: Value) -> Value {
        let mut call = json!({
            "type": "function",
            "function": {
                "name": "mcp__provider__handoff",
                "arguments": arguments,
            },
        });
        if let Some(id) = id {
            call["id"] = id;
        }
        call
    }

    #[test]
    fn descriptor_parses_control_from_metadata_without_tool_name_rules() {
        let metadata = json!({
            "control": {
                "kind": TERMINAL_HANDOFF_CONTROL_KIND,
                "target": "agent_authoring",
                "terminal": true,
                "policy_id": "provider.handoff.v1",
                "ui_visibility": "hidden",
            }
        });

        let parsed = RuntimeControlToolDescriptor::from_metadata(
            "mcp__different_provider__arbitrary_name",
            Some(&metadata),
        )
        .unwrap()
        .unwrap();

        assert_eq!(parsed.kind, TERMINAL_HANDOFF_CONTROL_KIND);
        assert_eq!(parsed.target, "agent_authoring");
        assert_eq!(parsed.fixed_action, None);
        assert!(parsed.terminal);
        assert_eq!(parsed.policy_id, "provider.handoff.v1");
    }

    #[test]
    fn unsupported_control_kind_fails_explicitly() {
        let metadata = json!({
            "control": {
                "kind": "moi.control.handoff.v2",
                "target": "agent_authoring",
                "terminal": true,
                "policy_id": "provider.handoff.v2",
            }
        });

        let error =
            RuntimeControlToolDescriptor::from_metadata("tool", Some(&metadata)).unwrap_err();

        assert_eq!(error.error_code(), "terminal_handoff_unsupported");
    }

    #[test]
    fn first_terminal_tool_is_accepted_without_interpreting_action() {
        let snapshot = snapshot();
        let mut window = TerminalHandoffWindow::for_snapshot(&snapshot);

        let outcome = evaluate_terminal_control_actions(
            &mut window,
            &snapshot,
            "",
            &[tool_call("mcp__provider__handoff", "call-1")],
        );

        let TerminalControlOutcome::Requested(request) = outcome else {
            panic!("expected terminal handoff request");
        };
        assert_eq!(request.action, "revise_current_agent");
        assert_eq!(request.target, "agent_authoring");
        assert_eq!(window, TerminalHandoffWindow::Delegated);
    }

    #[test]
    fn fixed_action_is_read_from_descriptor_and_requires_empty_arguments() {
        let mut descriptor = descriptor();
        descriptor.target = "workflow_authoring".to_string();
        descriptor.fixed_action = Some("handle_request".to_string());
        let snapshot = RuntimeControlToolSnapshot::new(vec![descriptor]);
        let mut window = TerminalHandoffWindow::for_snapshot(&snapshot);
        let call = terminal_tool_call(Some(json!("call-workflow")), json!("{}"));

        let outcome = evaluate_terminal_control_actions(&mut window, &snapshot, "", &[call]);

        let TerminalControlOutcome::Requested(request) = outcome else {
            panic!("expected terminal handoff request");
        };
        assert_eq!(request.target, "workflow_authoring");
        assert_eq!(request.action, "handle_request");
    }

    #[test]
    fn text_first_closes_window_and_rejects_terminal_call() {
        let snapshot = snapshot();
        let mut window = TerminalHandoffWindow::for_snapshot(&snapshot);

        let outcome = evaluate_terminal_control_actions(
            &mut window,
            &snapshot,
            "visible text",
            &[tool_call("mcp__provider__handoff", "call-2")],
        );

        let TerminalControlOutcome::Rejected(rejection) = outcome else {
            panic!("expected rejection");
        };
        assert_eq!(rejection.code, "terminal_handoff_window_closed");
        assert_eq!(window, TerminalHandoffWindow::Closed);
    }

    #[test]
    fn ordinary_tool_first_closes_window_and_late_handoff_is_rejected() {
        let snapshot = snapshot();
        let mut window = TerminalHandoffWindow::for_snapshot(&snapshot);

        assert_eq!(
            evaluate_terminal_control_actions(
                &mut window,
                &snapshot,
                "",
                &[tool_call("ordinary_tool", "call-ordinary")],
            ),
            TerminalControlOutcome::Passthrough
        );
        assert_eq!(window, TerminalHandoffWindow::Closed);

        let outcome = evaluate_terminal_control_actions(
            &mut window,
            &snapshot,
            "",
            &[tool_call("mcp__provider__handoff", "call-late")],
        );
        let TerminalControlOutcome::Rejected(rejection) = outcome else {
            panic!("expected late handoff rejection");
        };
        assert_eq!(rejection.code, "terminal_handoff_window_closed");
    }

    #[test]
    fn handoff_must_be_the_only_tool_call() {
        let snapshot = snapshot();
        let mut window = TerminalHandoffWindow::for_snapshot(&snapshot);

        let outcome = evaluate_terminal_control_actions(
            &mut window,
            &snapshot,
            "",
            &[
                tool_call("mcp__provider__handoff", "call-handoff"),
                tool_call("ordinary_tool", "call-extra"),
            ],
        );
        let TerminalControlOutcome::Rejected(rejection) = outcome else {
            panic!("expected contract rejection");
        };
        assert_eq!(rejection.code, "terminal_handoff_contract_violation");
    }

    #[test]
    fn repeated_handoff_after_delegation_is_rejected() {
        let snapshot = snapshot();
        let mut window = TerminalHandoffWindow::for_snapshot(&snapshot);
        assert!(matches!(
            evaluate_terminal_control_actions(
                &mut window,
                &snapshot,
                "",
                &[tool_call("mcp__provider__handoff", "call-first")],
            ),
            TerminalControlOutcome::Requested(_)
        ));

        let outcome = evaluate_terminal_control_actions(
            &mut window,
            &snapshot,
            "",
            &[tool_call("mcp__provider__handoff", "call-repeat")],
        );
        let TerminalControlOutcome::Rejected(rejection) = outcome else {
            panic!("expected repeated handoff rejection");
        };
        assert_eq!(rejection.code, "terminal_handoff_contract_violation");
        assert_eq!(window, TerminalHandoffWindow::Delegated);
    }

    #[test]
    fn terminal_handoff_rejects_invalid_id_and_argument_shapes() {
        let cases = [
            (
                "missing tool call id",
                terminal_tool_call(None, json!(r#"{"action":"x"}"#)),
                "must include a non-empty id",
                None,
            ),
            (
                "non-string tool call id",
                terminal_tool_call(Some(json!(7)), json!(r#"{"action":"x"}"#)),
                "must include a non-empty id",
                None,
            ),
            (
                "empty tool call id",
                terminal_tool_call(Some(json!("")), json!(r#"{"action":"x"}"#)),
                "must include a non-empty id",
                None,
            ),
            (
                "whitespace-only tool call id",
                terminal_tool_call(Some(json!(" \t")), json!(r#"{"action":"x"}"#)),
                "must include a non-empty id",
                None,
            ),
            (
                "leading whitespace tool call id",
                terminal_tool_call(Some(json!(" call-1")), json!(r#"{"action":"x"}"#)),
                "must not contain surrounding whitespace",
                None,
            ),
            (
                "trailing whitespace tool call id",
                terminal_tool_call(Some(json!("call-1 ")), json!(r#"{"action":"x"}"#)),
                "must not contain surrounding whitespace",
                None,
            ),
            (
                "arguments are not an object",
                terminal_tool_call(Some(json!("call-1")), json!("[1]")),
                "arguments must be a JSON object",
                Some("call-1"),
            ),
            (
                "arguments contain invalid JSON",
                terminal_tool_call(Some(json!("call-1")), json!("{")),
                "arguments must be a JSON object",
                Some("call-1"),
            ),
            (
                "missing action",
                terminal_tool_call(Some(json!("call-1")), json!("{}")),
                "action is required",
                Some("call-1"),
            ),
            (
                "extra argument field",
                terminal_tool_call(
                    Some(json!("call-1")),
                    json!(r#"{"action":"x","extra":true}"#),
                ),
                "arguments must contain only action",
                Some("call-1"),
            ),
            (
                "non-string action",
                terminal_tool_call(Some(json!("call-1")), json!(r#"{"action":1}"#)),
                "action must be a non-empty string",
                Some("call-1"),
            ),
            (
                "empty action",
                terminal_tool_call(Some(json!("call-1")), json!(r#"{"action":""}"#)),
                "action must be a non-empty string",
                Some("call-1"),
            ),
            (
                "whitespace-only action",
                terminal_tool_call(Some(json!("call-1")), json!(r#"{"action":" \t"}"#)),
                "action must be a non-empty string",
                Some("call-1"),
            ),
            (
                "leading whitespace action",
                terminal_tool_call(Some(json!("call-1")), json!(r#"{"action":" x"}"#)),
                "action must not contain surrounding whitespace",
                Some("call-1"),
            ),
            (
                "trailing whitespace action",
                terminal_tool_call(Some(json!("call-1")), json!(r#"{"action":"x "}"#)),
                "action must not contain surrounding whitespace",
                Some("call-1"),
            ),
        ];

        for (name, call, expected_message, expected_tool_call_id) in cases {
            let snapshot = snapshot();
            let mut window = TerminalHandoffWindow::for_snapshot(&snapshot);
            let outcome = evaluate_terminal_control_actions(&mut window, &snapshot, "", &[call]);
            let TerminalControlOutcome::Rejected(rejection) = outcome else {
                panic!("{name}: expected terminal contract rejection");
            };
            assert_eq!(
                rejection.code, "terminal_handoff_contract_violation",
                "{name}"
            );
            assert!(
                rejection.message.contains(expected_message),
                "{name}: unexpected rejection message: {}",
                rejection.message
            );
            assert_eq!(
                rejection.tool_call_id.as_deref(),
                expected_tool_call_id,
                "{name}: invalid ids must not be propagated"
            );
            assert_eq!(window, TerminalHandoffWindow::Closed, "{name}");
        }
    }

    #[test]
    fn streamed_tool_classification_waits_for_complete_arguments() {
        let snapshot = snapshot();
        let incomplete = json!({
            "id": "call-1",
            "type": "function",
            "function": {
                "name": "ordinary_tool",
                "arguments": r#"{"action":"#,
            }
        });

        assert_eq!(
            classify_complete_streamed_first_tool(&snapshot, &incomplete),
            None
        );
        assert_eq!(
            classify_complete_streamed_first_tool(
                &snapshot,
                &tool_call("ordinary_tool", "call-ordinary")
            ),
            Some(StreamedFirstToolAction::Ordinary)
        );
        assert_eq!(
            classify_complete_streamed_first_tool(
                &snapshot,
                &tool_call("mcp__provider__handoff", "call-terminal")
            ),
            Some(StreamedFirstToolAction::TerminalCandidate)
        );
    }
}
