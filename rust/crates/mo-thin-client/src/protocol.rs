//! JSON bodies and classified SSE payloads for the thin client protocol.
//!
//! Aligns with `runtime` `ChatRequest` / `http_helpers::sse_json_response` and design doc §5.5
//! (`edge_executor_id`, `capabilities`).

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// `POST /chat/stream` body — superset of server `ChatRequest` plus optional edge fields.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ChatStreamRequest {
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context: Option<serde_json::Map<String, Value>>,
    #[serde(default = "default_max_candidates")]
    pub max_candidates: u32,
    #[serde(default)]
    pub explain: bool,
    /// Design §5.5 — identifies which edge executor should run tool callbacks.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub edge_executor_id: Option<String>,
    /// Tool names this edge instance can run (bash, fs, git, …).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub capabilities: Vec<String>,
}

fn default_max_candidates() -> u32 {
    8
}

impl ChatStreamRequest {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            session_id: None,
            agent_id: None,
            model: None,
            context: None,
            max_candidates: default_max_candidates(),
            explain: false,
            edge_executor_id: None,
            capabilities: Vec::new(),
        }
    }
}

/// `POST /sessions` (matches `SessionCreateRequest` on server).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct SessionCreateRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<serde_json::Map<String, Value>>,
}

/// `PUT /sessions/{id}` body subset.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct SessionUpdateRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<serde_json::Map<String, Value>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
}

/// `POST /tools/result` (§5.5 — forward-compatible).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ToolResultRequest {
    pub request_id: String,
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
}

/// `POST /approval/respond` (§5.5).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ApprovalRespondRequest {
    pub request_id: String,
    pub decision: ApprovalDecision,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalDecision {
    Allow,
    Deny,
    AllowSession,
}

/// `POST /agents/edge` — matches server `EdgeRegisterRequest` (Phase 3 registry).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct EdgeRegisterRequest {
    pub edge_agent_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hostname: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worktree_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capabilities: Option<Value>,
}

impl EdgeRegisterRequest {
    pub fn new(edge_agent_id: impl Into<String>) -> Self {
        Self {
            edge_agent_id: edge_agent_id.into(),
            hostname: None,
            worktree_path: None,
            capabilities: None,
        }
    }
}

/// `POST /agents/edge/heartbeat`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct EdgeHeartbeatRequest {
    pub edge_agent_id: String,
}

/// `POST /tasks/{id}/lease/{claim,release,renew}` — matches server lease handlers.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct TaskLeaseMutationRequest {
    pub edge_agent_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ttl_sec: Option<i64>,
}

/// Classified SSE JSON line (`data: …` payload). Unknown `type` values are preserved as [`StreamEvent::Other`].
#[derive(Debug, Clone, PartialEq)]
pub enum StreamEvent {
    SessionInfo {
        session_id: String,
        run_id: String,
    },
    TextDelta {
        content: Value,
    },
    TextDone {
        full_text: Value,
    },
    ReasoningMessageContent {
        content: Value,
    },
    ThinkingDelta {
        content: Value,
    },
    ThinkingDone,
    ToolCallStart {
        tool: Value,
        call_id: Value,
    },
    ToolResult {
        call_id: Value,
        result: Value,
    },
    /// §5.5 — cloud asks edge to run a tool (forward-compatible).
    ToolRequest {
        request_id: String,
        tool: String,
        args: Value,
    },
    PlanCreated {
        plan: Value,
    },
    PlanStepStart {
        step: Value,
    },
    PlanStepDone {
        step: Value,
        result: Value,
    },
    PlanRevised {
        plan: Value,
    },
    /// §5.5 — subtask / plan progress (generic bucket).
    PlanUpdate {
        raw: Value,
    },
    AgentDelegated {
        agent_id: Value,
        task: Value,
    },
    AgentProgress {
        agent_id: Value,
        progress: Value,
    },
    AgentCompleted {
        agent_id: Value,
        result: Value,
    },
    RunStarted,
    RunFinished,
    Ping,
    Done {
        tokens_used: Option<u64>,
        raw: Value,
    },
    /// §5.5 — approval gate.
    ApprovalRequired {
        request_id: String,
        tool: String,
        path: Option<String>,
        raw: Value,
    },
    Error {
        message: String,
        code: Option<String>,
        retryable: bool,
        raw: Value,
    },
    /// Server sent a `type` we do not model yet.
    Other {
        event_type: String,
        raw: Value,
    },
}

/// Parse the JSON object from one SSE `data:` line into a [`StreamEvent`].
pub fn classify_stream_event(value: Value) -> Result<StreamEvent, crate::error::ThinClientError> {
    let obj = value
        .as_object()
        .cloned()
        .ok_or_else(|| crate::error::ThinClientError::InvalidSseJson(value.clone()))?;

    let ty = obj
        .get("type")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    let raw = Value::Object(obj.clone());

    Ok(match ty.as_str() {
        "session_info" => StreamEvent::SessionInfo {
            session_id: get_str(&obj, "session_id"),
            run_id: get_str(&obj, "run_id"),
        },
        "text_delta" => StreamEvent::TextDelta {
            content: obj.get("content").cloned().unwrap_or(Value::Null),
        },
        "text_done" => StreamEvent::TextDone {
            full_text: obj.get("full_text").cloned().unwrap_or(Value::Null),
        },
        "reasoning_message_content" => StreamEvent::ReasoningMessageContent {
            content: obj.get("content").cloned().unwrap_or(Value::Null),
        },
        "thinking_delta" => StreamEvent::ThinkingDelta {
            content: obj.get("content").cloned().unwrap_or(Value::Null),
        },
        "thinking_done" => StreamEvent::ThinkingDone,
        "tool_call_start" => StreamEvent::ToolCallStart {
            tool: obj.get("tool").cloned().unwrap_or(Value::Null),
            call_id: obj.get("call_id").cloned().unwrap_or(Value::Null),
        },
        "tool_result" => StreamEvent::ToolResult {
            call_id: obj.get("call_id").cloned().unwrap_or(Value::Null),
            result: obj.get("result").cloned().unwrap_or(Value::Null),
        },
        "tool_request" => StreamEvent::ToolRequest {
            request_id: get_str(&obj, "request_id"),
            tool: get_str(&obj, "tool"),
            args: obj.get("args").cloned().unwrap_or_else(|| Value::Object(Default::default())),
        },
        "plan_created" => StreamEvent::PlanCreated {
            plan: obj.get("plan").cloned().unwrap_or_else(|| Value::Object(Default::default())),
        },
        "plan_step_start" => StreamEvent::PlanStepStart {
            step: obj.get("step").cloned().unwrap_or(Value::Null),
        },
        "plan_step_done" => StreamEvent::PlanStepDone {
            step: obj.get("step").cloned().unwrap_or(Value::Null),
            result: obj.get("result").cloned().unwrap_or(Value::Null),
        },
        "plan_revised" => StreamEvent::PlanRevised {
            plan: obj.get("plan").cloned().unwrap_or_else(|| Value::Object(Default::default())),
        },
        "plan_update" => StreamEvent::PlanUpdate { raw },
        "agent_delegated" => StreamEvent::AgentDelegated {
            agent_id: obj.get("agent_id").cloned().unwrap_or(Value::Null),
            task: obj.get("task").cloned().unwrap_or(Value::Null),
        },
        "agent_progress" => StreamEvent::AgentProgress {
            agent_id: obj.get("agent_id").cloned().unwrap_or(Value::Null),
            progress: obj.get("progress").cloned().unwrap_or(Value::Null),
        },
        "agent_completed" => StreamEvent::AgentCompleted {
            agent_id: obj.get("agent_id").cloned().unwrap_or(Value::Null),
            result: obj.get("result").cloned().unwrap_or(Value::Null),
        },
        "run_started" => StreamEvent::RunStarted,
        "run_finished" => StreamEvent::RunFinished,
        "ping" => StreamEvent::Ping,
        "done" => StreamEvent::Done {
            tokens_used: obj
                .get("tokens_used")
                .and_then(|v| v.as_u64())
                .or_else(|| obj.get("tokens_used").and_then(|v| v.as_i64()).map(|i| i as u64)),
            raw,
        },
        "approval_required" => StreamEvent::ApprovalRequired {
            request_id: get_str(&obj, "request_id"),
            tool: get_str(&obj, "tool"),
            path: obj
                .get("path")
                .and_then(|v| v.as_str())
                .map(std::string::ToString::to_string),
            raw,
        },
        "error" => StreamEvent::Error {
            message: obj
                .get("message")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            code: obj
                .get("code")
                .and_then(|v| v.as_str())
                .map(std::string::ToString::to_string),
            retryable: obj
                .get("retryable")
                .and_then(|v| v.as_bool())
                .unwrap_or(false),
            raw,
        },
        "" => StreamEvent::Other {
            event_type: String::new(),
            raw,
        },
        _ => StreamEvent::Other {
            event_type: ty,
            raw,
        },
    })
}

fn get_str(obj: &serde_json::Map<String, Value>, key: &str) -> String {
    obj.get(key)
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chat_stream_request_serde_roundtrip() {
        let r = ChatStreamRequest {
            message: "hi".into(),
            session_id: Some("s-1".into()),
            agent_id: None,
            model: Some("m".into()),
            context: None,
            max_candidates: 3,
            explain: true,
            edge_executor_id: Some("edge-1".into()),
            capabilities: vec!["bash".into(), "fs".into()],
        };
        let j = serde_json::to_value(&r).unwrap();
        let back: ChatStreamRequest = serde_json::from_value(j).unwrap();
        assert_eq!(r, back);
    }

    #[test]
    fn chat_stream_request_default_max_candidates() {
        let j = serde_json::json!({"message":"x"});
        let r: ChatStreamRequest = serde_json::from_value(j).unwrap();
        assert_eq!(r.max_candidates, 8);
    }

    #[test]
    fn classify_session_info() {
        let v = serde_json::json!({"type":"session_info","session_id":"a","run_id":"b"});
        match classify_stream_event(v).unwrap() {
            StreamEvent::SessionInfo { session_id, run_id } => {
                assert_eq!(session_id, "a");
                assert_eq!(run_id, "b");
            }
            e => panic!("unexpected {e:?}"),
        }
    }

    #[test]
    fn classify_tool_request_design_shape() {
        let v = serde_json::json!({
            "type": "tool_request",
            "request_id": "tr-1",
            "tool": "bash",
            "args": {"command": "ls"}
        });
        match classify_stream_event(v).unwrap() {
            StreamEvent::ToolRequest {
                request_id,
                tool,
                args,
            } => {
                assert_eq!(request_id, "tr-1");
                assert_eq!(tool, "bash");
                assert_eq!(args["command"], "ls");
            }
            e => panic!("unexpected {e:?}"),
        }
    }

    #[test]
    fn classify_error_event() {
        let v = serde_json::json!({
            "type": "error",
            "message": "nope",
            "code": "AUTH_ERROR",
            "retryable": false
        });
        match classify_stream_event(v).unwrap() {
            StreamEvent::Error {
                message,
                code,
                retryable,
                ..
            } => {
                assert_eq!(message, "nope");
                assert_eq!(code.as_deref(), Some("AUTH_ERROR"));
                assert!(!retryable);
            }
            e => panic!("unexpected {e:?}"),
        }
    }

    #[test]
    fn classify_unknown_type_preserved() {
        let v = serde_json::json!({"type":"future_event","foo": 1});
        match classify_stream_event(v).unwrap() {
            StreamEvent::Other { event_type, raw } => {
                assert_eq!(event_type, "future_event");
                assert_eq!(raw["foo"], 1);
            }
            e => panic!("unexpected {e:?}"),
        }
    }
}
