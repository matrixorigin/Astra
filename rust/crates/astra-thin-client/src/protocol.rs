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
    /// Forwarded into server `context` for stop-hooks (`when: task_completed`) on cloud runs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan_subtask_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub is_plan_subtask: Option<bool>,
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
            plan_subtask_id: None,
            is_plan_subtask: None,
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
        run_id: Option<String>,
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
    ReasoningDelta {
        content: Value,
    },
    ThinkingDelta {
        content: Value,
    },
    ThinkingDone,
    ReasoningDone,
    ToolCallStart {
        tool: Value,
        call_id: Value,
        arguments: Option<Value>,
    },
    ToolCallEnd {
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
    AgentSpawned {
        agent_id: String,
        run_id: String,
        parent_run_id: String,
        agent_type: String,
        description: String,
        timestamp: Option<u64>,
        raw: Value,
    },
    AgentProgress {
        agent_id: String,
        status: Option<String>,
        raw: Value,
    },
    AgentCompleted {
        agent_id: String,
        status: Option<String>,
        raw: Value,
    },
    RunStarted {
        run_id: Option<String>,
        session_id: Option<String>,
    },
    RunPaused {
        run_id: Option<String>,
    },
    RunResumed {
        run_id: Option<String>,
    },
    RunCancelled {
        run_id: Option<String>,
    },
    RunFinished {
        run_id: Option<String>,
        status: Option<String>,
        error: Option<String>,
    },
    Usage {
        prompt_tokens: Option<u64>,
        completion_tokens: Option<u64>,
        tool_call_count: Option<u64>,
        cache_creation_tokens: Option<u64>,
        cache_read_tokens: Option<u64>,
        cache_creation_input_tokens: Option<u64>,
        cache_read_input_tokens: Option<u64>,
        raw: Value,
    },
    TurnComplete {
        followup_suggestion: Option<String>,
        raw: Value,
    },
    Warning {
        message: String,
        claims_failed: Option<u64>,
        raw: Value,
    },
    Explain {
        content: String,
        raw: Value,
    },
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
        detail: Option<String>,
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
            run_id: obj
                .get("run_id")
                .and_then(|v| v.as_str())
                .map(std::string::ToString::to_string),
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
        "reasoning_delta" => StreamEvent::ReasoningDelta {
            content: obj.get("content").cloned().unwrap_or(Value::Null),
        },
        "thinking_delta" => StreamEvent::ThinkingDelta {
            content: obj.get("content").cloned().unwrap_or(Value::Null),
        },
        "thinking_done" => StreamEvent::ThinkingDone,
        "reasoning_done" => StreamEvent::ReasoningDone,
        "tool_call_start" => StreamEvent::ToolCallStart {
            tool: obj.get("tool").cloned().unwrap_or(Value::Null),
            call_id: obj.get("call_id").cloned().unwrap_or(Value::Null),
            arguments: obj.get("arguments").cloned(),
        },
        "tool_call_end" | "tool_result" => StreamEvent::ToolCallEnd {
            call_id: obj.get("call_id").cloned().unwrap_or(Value::Null),
            result: obj.get("result").cloned().unwrap_or(Value::Null),
        },
        "tool_request" => StreamEvent::ToolRequest {
            request_id: get_str(&obj, "request_id"),
            tool: get_str(&obj, "tool"),
            args: obj
                .get("args")
                .cloned()
                .unwrap_or_else(|| Value::Object(Default::default())),
        },
        "plan_created" => StreamEvent::PlanCreated {
            plan: obj
                .get("plan")
                .cloned()
                .unwrap_or_else(|| Value::Object(Default::default())),
        },
        "plan_step_start" => StreamEvent::PlanStepStart {
            step: obj.get("step").cloned().unwrap_or(Value::Null),
        },
        "plan_step_done" => StreamEvent::PlanStepDone {
            step: obj.get("step").cloned().unwrap_or(Value::Null),
            result: obj.get("result").cloned().unwrap_or(Value::Null),
        },
        "plan_revised" => StreamEvent::PlanRevised {
            plan: obj
                .get("plan")
                .cloned()
                .unwrap_or_else(|| Value::Object(Default::default())),
        },
        "plan_update" => StreamEvent::PlanUpdate { raw },
        "agent_delegated" => StreamEvent::AgentDelegated {
            agent_id: obj.get("agent_id").cloned().unwrap_or(Value::Null),
            task: obj.get("task").cloned().unwrap_or(Value::Null),
        },
        "agent_spawned" => StreamEvent::AgentSpawned {
            agent_id: get_str(&obj, "agent_id"),
            run_id: get_str(&obj, "run_id"),
            parent_run_id: get_str(&obj, "parent_run_id"),
            agent_type: get_str(&obj, "agent_type"),
            description: get_str(&obj, "description"),
            timestamp: obj.get("timestamp").and_then(|v| v.as_u64()),
            raw,
        },
        "agent_progress" => StreamEvent::AgentProgress {
            agent_id: get_str(&obj, "agent_id"),
            status: optional_str(&obj, "status"),
            raw,
        },
        "agent_completed" => StreamEvent::AgentCompleted {
            agent_id: get_str(&obj, "agent_id"),
            status: optional_str(&obj, "status"),
            raw,
        },
        "run_started" => StreamEvent::RunStarted {
            run_id: optional_str(&obj, "run_id"),
            session_id: optional_str(&obj, "session_id"),
        },
        "run_paused" => StreamEvent::RunPaused {
            run_id: optional_str(&obj, "run_id"),
        },
        "run_resumed" => StreamEvent::RunResumed {
            run_id: optional_str(&obj, "run_id"),
        },
        "run_cancelled" => StreamEvent::RunCancelled {
            run_id: optional_str(&obj, "run_id"),
        },
        "run_finished" => StreamEvent::RunFinished {
            run_id: optional_str(&obj, "run_id"),
            status: optional_str(&obj, "status"),
            error: optional_str(&obj, "error"),
        },
        "usage" => StreamEvent::Usage {
            prompt_tokens: obj.get("prompt_tokens").and_then(|v| v.as_u64()),
            completion_tokens: obj.get("completion_tokens").and_then(|v| v.as_u64()),
            tool_call_count: obj.get("tool_call_count").and_then(|v| v.as_u64()),
            cache_creation_tokens: obj.get("cache_creation_tokens").and_then(|v| v.as_u64()),
            cache_read_tokens: obj.get("cache_read_tokens").and_then(|v| v.as_u64()),
            cache_creation_input_tokens: obj
                .get("cache_creation_input_tokens")
                .and_then(|v| v.as_u64()),
            cache_read_input_tokens: obj.get("cache_read_input_tokens").and_then(|v| v.as_u64()),
            raw,
        },
        "turn_complete" => StreamEvent::TurnComplete {
            followup_suggestion: optional_str(&obj, "followup_suggestion"),
            raw,
        },
        "warning" => StreamEvent::Warning {
            message: get_str(&obj, "message"),
            claims_failed: obj.get("claims_failed").and_then(|v| v.as_u64()),
            raw,
        },
        "explain" => StreamEvent::Explain {
            content: get_str(&obj, "content"),
            raw,
        },
        "ping" => StreamEvent::Ping,
        "done" => StreamEvent::Done {
            tokens_used: obj.get("tokens_used").and_then(|v| v.as_u64()).or_else(|| {
                obj.get("tokens_used")
                    .and_then(|v| v.as_i64())
                    .map(|i| i as u64)
            }),
            raw,
        },
        "approval_required" => StreamEvent::ApprovalRequired {
            request_id: get_str(&obj, "request_id"),
            tool: get_str(&obj, "tool"),
            path: obj
                .get("path")
                .and_then(|v| v.as_str())
                .map(std::string::ToString::to_string),
            detail: obj
                .get("detail")
                .and_then(|v| v.as_str())
                .map(std::string::ToString::to_string)
                .or_else(|| {
                    obj.get("path")
                        .and_then(|v| v.as_str())
                        .map(std::string::ToString::to_string)
                }),
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

fn optional_str(obj: &serde_json::Map<String, Value>, key: &str) -> Option<String> {
    obj.get(key)
        .and_then(|v| v.as_str())
        .map(std::string::ToString::to_string)
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
            plan_subtask_id: Some("t1".into()),
            is_plan_subtask: Some(true),
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
                assert_eq!(run_id.as_deref(), Some("b"));
            }
            e => panic!("unexpected {e:?}"),
        }
    }

    #[test]
    fn classify_session_info_without_run_id() {
        let v = serde_json::json!({"type":"session_info","session_id":"a"});
        match classify_stream_event(v).unwrap() {
            StreamEvent::SessionInfo { session_id, run_id } => {
                assert_eq!(session_id, "a");
                assert_eq!(run_id, None);
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
    fn classify_tool_call_start_preserves_arguments() {
        let value = serde_json::json!({
            "type":"tool_call_start",
            "tool":"bash",
            "call_id":"c1",
            "arguments":"{\"command\":\"ls\"}"
        });
        match classify_stream_event(value).unwrap() {
            StreamEvent::ToolCallStart {
                tool,
                call_id,
                arguments,
            } => {
                assert_eq!(tool, "bash");
                assert_eq!(call_id, "c1");
                assert_eq!(
                    arguments,
                    Some(Value::String("{\"command\":\"ls\"}".to_string()))
                );
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn classify_tool_call_end_and_legacy_tool_result() {
        for value in [
            serde_json::json!({"type":"tool_call_end","call_id":"c1","result":"ok"}),
            serde_json::json!({"type":"tool_result","call_id":"c2","result":"legacy"}),
        ] {
            match classify_stream_event(value).unwrap() {
                StreamEvent::ToolCallEnd { call_id, result } => {
                    assert!(call_id == "c1" || call_id == "c2");
                    assert!(result == "ok" || result == "legacy");
                }
                other => panic!("unexpected {other:?}"),
            }
        }
    }

    #[test]
    fn classify_reasoning_done() {
        match classify_stream_event(serde_json::json!({"type":"reasoning_done"})).unwrap() {
            StreamEvent::ReasoningDone => {}
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn classify_reasoning_delta() {
        match classify_stream_event(serde_json::json!({
            "type":"reasoning_delta",
            "content":"thinking"
        }))
        .unwrap()
        {
            StreamEvent::ReasoningDelta { content } => assert_eq!(content, "thinking"),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn classify_agent_events() {
        let spawned = serde_json::json!({
            "type": "agent_spawned",
            "agent_id": "agent-1",
            "run_id": "run-1",
            "parent_run_id": "root-1",
            "agent_type": "worker",
            "description": "Investigate",
            "timestamp": 123
        });
        match classify_stream_event(spawned).unwrap() {
            StreamEvent::AgentSpawned {
                agent_id,
                run_id,
                parent_run_id,
                agent_type,
                description,
                timestamp,
                raw,
            } => {
                assert_eq!(agent_id, "agent-1");
                assert_eq!(run_id, "run-1");
                assert_eq!(parent_run_id, "root-1");
                assert_eq!(agent_type, "worker");
                assert_eq!(description, "Investigate");
                assert_eq!(timestamp, Some(123));
                assert_eq!(raw["type"], "agent_spawned");
            }
            other => panic!("unexpected {other:?}"),
        }

        let progress = serde_json::json!({
            "type": "agent_progress",
            "agent_id": "agent-1",
            "status": "started",
            "description": "Reading files",
            "timestamp": 456
        });
        match classify_stream_event(progress).unwrap() {
            StreamEvent::AgentProgress {
                agent_id,
                status,
                raw,
            } => {
                assert_eq!(agent_id, "agent-1");
                assert_eq!(status.as_deref(), Some("started"));
                assert_eq!(raw["description"], "Reading files");
                assert_eq!(raw["timestamp"], 456);
            }
            other => panic!("unexpected {other:?}"),
        }

        let completed = serde_json::json!({
            "type": "agent_completed",
            "agent_id": "agent-1",
            "status": "failed",
            "error": "boom",
            "timestamp": 789
        });
        match classify_stream_event(completed).unwrap() {
            StreamEvent::AgentCompleted {
                agent_id,
                status,
                raw,
            } => {
                assert_eq!(agent_id, "agent-1");
                assert_eq!(status.as_deref(), Some("failed"));
                assert_eq!(raw["error"], "boom");
                assert_eq!(raw["timestamp"], 789);
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn classify_usage() {
        let value = serde_json::json!({
            "type": "usage",
            "prompt_tokens": 10,
            "completion_tokens": 4,
            "tool_call_count": 2,
            "cache_creation_tokens": 3,
            "cache_read_tokens": 1
        });
        match classify_stream_event(value).unwrap() {
            StreamEvent::Usage {
                prompt_tokens,
                completion_tokens,
                tool_call_count,
                cache_creation_tokens,
                cache_read_tokens,
                raw,
                ..
            } => {
                assert_eq!(prompt_tokens, Some(10));
                assert_eq!(completion_tokens, Some(4));
                assert_eq!(tool_call_count, Some(2));
                assert_eq!(cache_creation_tokens, Some(3));
                assert_eq!(cache_read_tokens, Some(1));
                assert_eq!(raw["type"], "usage");
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn classify_turn_complete_warning_and_explain() {
        match classify_stream_event(serde_json::json!({
            "type": "turn_complete",
            "followup_suggestion": "Try /plan"
        }))
        .unwrap()
        {
            StreamEvent::TurnComplete {
                followup_suggestion,
                raw,
            } => {
                assert_eq!(followup_suggestion.as_deref(), Some("Try /plan"));
                assert_eq!(raw["type"], "turn_complete");
            }
            other => panic!("unexpected {other:?}"),
        }

        match classify_stream_event(serde_json::json!({
            "type": "warning",
            "message": "approaching limit",
            "claims_failed": 2
        }))
        .unwrap()
        {
            StreamEvent::Warning {
                message,
                claims_failed,
                raw,
            } => {
                assert_eq!(message, "approaching limit");
                assert_eq!(claims_failed, Some(2));
                assert_eq!(raw["type"], "warning");
            }
            other => panic!("unexpected {other:?}"),
        }

        match classify_stream_event(serde_json::json!({
            "type": "explain",
            "content": "why this happened"
        }))
        .unwrap()
        {
            StreamEvent::Explain { content, raw } => {
                assert_eq!(content, "why this happened");
                assert_eq!(raw["type"], "explain");
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn classify_run_lifecycle_events() {
        let started = serde_json::json!({
            "type": "run_started",
            "run_id": "run-1",
            "session_id": "sess-1"
        });
        match classify_stream_event(started).unwrap() {
            StreamEvent::RunStarted { run_id, session_id } => {
                assert_eq!(run_id.as_deref(), Some("run-1"));
                assert_eq!(session_id.as_deref(), Some("sess-1"));
            }
            other => panic!("unexpected {other:?}"),
        }

        let finished = serde_json::json!({
            "type": "run_finished",
            "run_id": "run-1",
            "status": "failed",
            "error": "boom"
        });
        match classify_stream_event(finished).unwrap() {
            StreamEvent::RunFinished {
                run_id,
                status,
                error,
            } => {
                assert_eq!(run_id.as_deref(), Some("run-1"));
                assert_eq!(status.as_deref(), Some("failed"));
                assert_eq!(error.as_deref(), Some("boom"));
            }
            other => panic!("unexpected {other:?}"),
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
