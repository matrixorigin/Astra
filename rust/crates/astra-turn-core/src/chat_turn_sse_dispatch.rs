//! JSON `type` dispatch for astra `/chat/turn` SSE event blocks (blank-line framed).
//!
//! Shared between the CLI stream consumer and any future headless client: updates a structured
//! accumulator and returns terminal UI hints. [`ChatTurnSseFramer`] turns arbitrary byte chunks
//! into complete event blocks via [`super::sse_blocks`] and records time-to-first-token.

use astra_thin_client::ApprovalKind;
use serde_json::Value;
use std::time::Instant;

use super::sse_blocks::SseBlankLineUtf8Buf;

/// State collected from one `/chat/turn` SSE stream (excluding edge executor bookkeeping).
#[derive(Debug, Clone, Default)]
pub struct ChatTurnSseAccum {
    pub session_id: Option<String>,
    pub run_id: Option<String>,
    pub full_text: String,
    /// Thinking / reasoning chunks (for models that stream reasoning separately).
    pub reasoning_content: String,
    pub tool_calls: Vec<Value>,
    /// Index from tool_call id -> position in `tool_calls` for O(1) merges.
    pub tool_call_id_index: std::collections::HashMap<String, usize>,
    pub explain_turns: Vec<Value>,
    pub has_tool_calls: bool,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_creation_tokens: u64,
    pub has_usage: bool,
    pub error_message: Option<String>,
    /// System prompt token estimate from runtime (via `context_meta` SSE event).
    pub system_prompt_tokens: Option<u32>,
    /// Detailed system prompt breakdown from runtime (via `context_meta` SSE event).
    pub system_prompt_breakdown: Option<Value>,
}

/// Deferred edge work from `tool_request` / `approval_required` events.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EdgeApprovalRequest {
    pub request_id: String,
    pub tool: String,
    pub approval_kind: ApprovalKind,
    pub detail: Option<String>,
}

/// Deferred edge work from `tool_request` / approval SSE events.
#[derive(Debug, Clone)]
pub enum ChatTurnEdgePending {
    ToolRequest {
        request_id: String,
        tool: String,
        args: Value,
    },
    ApprovalRequired {
        request_id: String,
        tool: String,
        approval_kind: ApprovalKind,
        detail: Option<String>,
    },
    ApprovalBatchRequired {
        requests: Vec<EdgeApprovalRequest>,
    },
}

/// Hints for the CLI live renderer (no-op when the consumer sets `quiet`).
#[derive(Debug)]
pub enum SseRenderEffect {
    StopThinkingSpinner,
    StartThinkingSpinner,
    /// Incremental reasoning chunk for a compact terminal preview (CLI).
    ThinkingPreviewChunk(String),
    StreamText(String),
}

fn normalize_tool_call_for_accum(event: &Value) -> Option<Value> {
    // Prefer the top-level `id` / `call_id`; providers that wrap the tool
    // call under `function` (nested) set `function.id` / `function.call_id`
    // instead. Fall back to those before minting a synthetic UUID so
    // downstream `tool_call_id` matching keeps working with providers that
    // only surface the id under `function`.
    let nested_id = event.get("function").and_then(|f| {
        f.get("id")
            .or_else(|| f.get("call_id"))
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    });
    let call_id = event
        .get("id")
        .or_else(|| event.get("call_id"))
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .or(nested_id)
        .unwrap_or_else(|| uuid::Uuid::now_v7().to_string());

    if let Some(function) = event.get("function").and_then(Value::as_object) {
        let name = function.get("name").and_then(Value::as_str).unwrap_or("");
        let arguments = function
            .get("arguments")
            .cloned()
            .unwrap_or_else(|| Value::String("{}".to_string()));
        if !name.is_empty() {
            return Some(serde_json::json!({
                "id": call_id,
                "type": "function",
                "function": {
                    "name": name,
                    "arguments": arguments,
                }
            }));
        }
    }

    let name = event
        .get("name")
        .or_else(|| event.get("tool"))
        .and_then(Value::as_str)
        .unwrap_or("");
    if name.is_empty() {
        return None;
    }

    let raw_arguments = event
        .get("arguments")
        .or_else(|| event.get("args"))
        .cloned()
        .unwrap_or_else(|| Value::Object(Default::default()));
    let arguments = match raw_arguments {
        Value::String(text) => Value::String(text),
        other => Value::String(serde_json::to_string(&other).unwrap_or_else(|_| "{}".to_string())),
    };

    Some(serde_json::json!({
        "id": call_id,
        "type": "function",
        "function": {
            "name": name,
            "arguments": arguments,
        }
    }))
}

fn approval_kind_from_event(event: &Value) -> ApprovalKind {
    event
        .get("approval_kind")
        .cloned()
        .and_then(|value| serde_json::from_value::<ApprovalKind>(value).ok())
        .unwrap_or(ApprovalKind::Explicit)
}

fn approval_request_from_event(event: &Value) -> Option<EdgeApprovalRequest> {
    let request_id = event
        .get("request_id")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let tool = event
        .get("tool")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let approval_kind = approval_kind_from_event(event);
    let detail = event
        .get("detail")
        .and_then(|v| v.as_str())
        .map(std::string::ToString::to_string)
        .or_else(|| {
            event
                .get("path")
                .and_then(|v| v.as_str())
                .map(std::string::ToString::to_string)
        });
    if request_id.is_empty() || tool.is_empty() {
        return None;
    }
    Some(EdgeApprovalRequest {
        request_id,
        tool,
        approval_kind,
        detail,
    })
}

fn apply_one_event(
    event: &Value,
    accum: &mut ChatTurnSseAccum,
    edge_pending: &mut Vec<ChatTurnEdgePending>,
    effects: &mut Vec<SseRenderEffect>,
) {
    let etype = event.get("type").and_then(|v| v.as_str()).unwrap_or("");
    match etype {
        "text_delta" => {
            effects.push(SseRenderEffect::StopThinkingSpinner);
            if let Some(content) = event.get("content").and_then(|v| v.as_str()) {
                accum.full_text.push_str(content);
                effects.push(SseRenderEffect::StreamText(content.to_string()));
            }
        }
        "text_done" => {
            if accum.full_text.is_empty()
                && let Some(ft) = event.get("full_text").and_then(|v| v.as_str())
            {
                accum.full_text = ft.to_string();
            }
        }
        "thinking_delta" | "reasoning_delta" | "reasoning_message_content" => {
            effects.push(SseRenderEffect::StartThinkingSpinner);
            if let Some(chunk) = event.get("content").and_then(|v| v.as_str()) {
                accum.reasoning_content.push_str(chunk);
                if !chunk.is_empty() {
                    effects.push(SseRenderEffect::ThinkingPreviewChunk(chunk.to_string()));
                }
            }
        }
        "thinking_done" | "reasoning_done" => {
            effects.push(SseRenderEffect::StopThinkingSpinner);
        }
        "tool_call_start" => {
            effects.push(SseRenderEffect::StopThinkingSpinner);
            if let Some(tool_call) = normalize_tool_call_for_accum(event) {
                let id = tool_call
                    .get("id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let idx = accum.tool_calls.len();
                accum.tool_calls.push(tool_call);
                if !id.is_empty() {
                    accum.tool_call_id_index.insert(id, idx);
                }
            }
        }
        "tool_call" => {
            if let Some(tool_call) = normalize_tool_call_for_accum(event) {
                let tc_id = tool_call.get("id").and_then(|v| v.as_str()).unwrap_or("");
                if !tc_id.is_empty() {
                    if let Some(&idx) = accum.tool_call_id_index.get(tc_id) {
                        accum.tool_calls[idx] = tool_call;
                    } else {
                        let idx = accum.tool_calls.len();
                        accum.tool_call_id_index.insert(tc_id.to_string(), idx);
                        accum.tool_calls.push(tool_call);
                    }
                } else {
                    accum.tool_calls.push(tool_call);
                }
            }
        }
        "tool_request" => {
            let request_id = event
                .get("request_id")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let tool = event
                .get("tool")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let args = event
                .get("args")
                .cloned()
                .unwrap_or_else(|| Value::Object(Default::default()));
            if !request_id.is_empty() && !tool.is_empty() {
                edge_pending.push(ChatTurnEdgePending::ToolRequest {
                    request_id,
                    tool,
                    args,
                });
            }
        }
        "approval_required" => {
            if let Some(request) = approval_request_from_event(event) {
                edge_pending.push(ChatTurnEdgePending::ApprovalRequired {
                    request_id: request.request_id,
                    tool: request.tool,
                    approval_kind: request.approval_kind,
                    detail: request.detail,
                });
            }
        }
        "approval_batch_required" => {
            let requests = event
                .get("requests")
                .and_then(Value::as_array)
                .map(|items| {
                    items
                        .iter()
                        .filter_map(approval_request_from_event)
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            if !requests.is_empty() {
                edge_pending.push(ChatTurnEdgePending::ApprovalBatchRequired { requests });
            }
        }
        "explain" => {
            accum.explain_turns.push(event.clone());
        }
        "turn_complete" | "turn_done" => {
            accum.has_tool_calls = event
                .get("has_tool_calls")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
        }
        "session_info" => {
            if let Some(sid) = event.get("session_id").and_then(|v| v.as_str()) {
                accum.session_id = Some(sid.to_string());
            }
            if let Some(rid) = event.get("run_id").and_then(|v| v.as_str()) {
                accum.run_id = Some(rid.to_string());
            }
        }
        "usage" => {
            // Providers either expose usage fields flat on the event or wrap
            // them inside a nested `"usage": {...}` object. Flat wins if
            // present; nested is consulted only as a fallback so a provider
            // that switches shape mid-stream never silently zeroes these
            // counters. All four fields (prompt/completion/cache_read/
            // cache_creation) share this precedence.
            let nested = event.get("usage");
            let read_u64 = |field: &str| -> Option<u64> {
                event
                    .get(field)
                    .and_then(|v| v.as_u64())
                    .or_else(|| nested.and_then(|u| u.get(field)).and_then(|v| v.as_u64()))
            };
            let prompt = read_u64("prompt_tokens");
            let completion = read_u64("completion_tokens");
            if prompt.is_none() && completion.is_none() {
                if accum.error_message.is_none() {
                    accum.error_message = Some("Error: invalid usage payload".to_string());
                }
                return;
            }
            accum.prompt_tokens = prompt.unwrap_or(0);
            accum.completion_tokens = completion.unwrap_or(0);
            accum.cache_read_tokens = read_u64("cache_read_tokens").unwrap_or(0);
            accum.cache_creation_tokens = read_u64("cache_creation_tokens").unwrap_or(0);
            accum.has_usage = true;
        }
        "error" => {
            let msg = event
                .get("message")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown error");
            accum.error_message = Some(format!("Error: {msg}"));
        }
        "context_meta" => {
            if let Some(t) = event.get("system_prompt_tokens").and_then(|v| v.as_u64()) {
                accum.system_prompt_tokens = Some(t as u32);
            }
            if let Some(b) = event.get("system_prompt_breakdown") {
                accum.system_prompt_breakdown = Some(b.clone());
            }
        }
        "run_started" => {
            if let Some(rid) = event.get("run_id").and_then(|v| v.as_str()) {
                accum.run_id = Some(rid.to_string());
            }
        }
        _ => {
            if let Some(rid) = event.get("run_id").and_then(|v| v.as_str()) {
                accum.run_id = Some(rid.to_string());
            }
        }
    }
}

/// Parse one SSE event `block` (may contain multiple `data:` lines), update `accum`, append edge work.
pub fn dispatch_chat_turn_sse_event_block(
    block: &str,
    accum: &mut ChatTurnSseAccum,
    edge_pending: &mut Vec<ChatTurnEdgePending>,
) -> Vec<SseRenderEffect> {
    let mut effects = Vec::new();
    for line in block.lines() {
        let Some(data) = line.strip_prefix("data: ") else {
            continue;
        };
        if data == "[DONE]" {
            continue;
        }
        let Ok(event) = serde_json::from_str::<Value>(data) else {
            // Synthetic error: protocol parse error should be visible, not silently ignored.
            effects.push(SseRenderEffect::StopThinkingSpinner);
            if accum.error_message.is_none() {
                accum.error_message = Some("Error: invalid JSON in SSE data".to_string());
            }
            continue;
        };
        apply_one_event(&event, accum, edge_pending, &mut effects);
    }
    effects
}

/// Buffers lossy UTF-8 from a `/chat/turn` body stream, yields complete blank-line SSE blocks, and
/// records [`ChatTurnSseFramer::ttft_ms`] on the first text or reasoning payload
/// (`text_delta`, `content_block_delta`, `thinking_delta`, `reasoning_delta`,
/// `reasoning_message_content`).
#[derive(Debug)]
pub struct ChatTurnSseFramer {
    sse: SseBlankLineUtf8Buf,
    stream_start: Instant,
    pub ttft_ms: Option<u64>,
    first_token_recorded: bool,
}

impl ChatTurnSseFramer {
    pub fn new() -> Self {
        Self {
            sse: SseBlankLineUtf8Buf::new(),
            stream_start: Instant::now(),
            ttft_ms: None,
            first_token_recorded: false,
        }
    }

    fn note_ttft_from_raw_event_text(&mut self, event_block: &str) {
        if self.first_token_recorded {
            return;
        }
        if event_block.contains("\"text_delta\"")
            || event_block.contains("\"content_block_delta\"")
            || event_block.contains("\"thinking_delta\"")
            || event_block.contains("\"reasoning_delta\"")
            || event_block.contains("\"reasoning_message_content\"")
            || event_block.contains("\"tool_call_start\"")
            || event_block.contains("\"tool_call\"")
        {
            self.ttft_ms = Some(self.stream_start.elapsed().as_millis() as u64);
            self.first_token_recorded = true;
        }
    }

    /// Append one HTTP chunk; returns every **complete** SSE event block (may be empty).
    pub fn push_lossy_bytes(&mut self, bytes: &[u8]) -> Vec<String> {
        let blocks = self.sse.push_lossy_bytes(bytes);
        for b in &blocks {
            self.note_ttft_from_raw_event_text(b);
        }
        blocks
    }

    /// After the byte stream ends: run TTFT detection on any trailing bytes, then take the buffer
    /// for a final [`dispatch_chat_turn_sse_event_block`] pass (partial event without `\n\n` yet).
    pub fn take_trailing_dispatch_blob(&mut self) -> String {
        let tail = self.sse.take_buf();
        self.note_ttft_from_raw_event_text(&tail);
        tail
    }
}

impl Default for ChatTurnSseFramer {
    fn default() -> Self {
        Self::new()
    }
}

/// Result of parsing a full UTF-8 `/chat/turn`-style SSE body in one shot (tests, fixtures, future headless clients).
#[derive(Debug)]
pub struct ParsedChatTurnSseBody {
    pub accum: ChatTurnSseAccum,
    pub edge_pending: Vec<ChatTurnEdgePending>,
    pub render_effects: Vec<SseRenderEffect>,
    pub ttft_ms: Option<u64>,
}

/// Parse an entire response body as UTF-8 (valid `str` — use [`String::from_utf8_lossy`] at the boundary if needed).
pub fn parse_chat_turn_sse_utf8_body(body: &str) -> ParsedChatTurnSseBody {
    let mut framer = ChatTurnSseFramer::new();
    let mut accum = ChatTurnSseAccum::default();
    let mut pending = Vec::new();
    let mut render_effects = Vec::new();
    for block in framer.push_lossy_bytes(body.as_bytes()) {
        render_effects.extend(dispatch_chat_turn_sse_event_block(
            &block,
            &mut accum,
            &mut pending,
        ));
    }
    let tail = framer.take_trailing_dispatch_blob();
    if !tail.trim().is_empty() {
        render_effects.extend(dispatch_chat_turn_sse_event_block(
            &tail,
            &mut accum,
            &mut pending,
        ));
    }
    ParsedChatTurnSseBody {
        accum,
        edge_pending: pending,
        render_effects,
        ttft_ms: framer.ttft_ms,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_utf8_body_roundtrip_text() {
        let body = format!(
            "{}{}",
            sse("text_delta", ",\"content\":\"hi\""),
            sse("text_delta", ",\"content\":\" there\"")
        );
        let p = parse_chat_turn_sse_utf8_body(&body);
        assert_eq!(p.accum.full_text, "hi there");
        assert!(p.edge_pending.is_empty());
    }

    fn sse(event_type: &str, extra: &str) -> String {
        format!("data: {{\"type\":\"{event_type}\"{extra}}}\n\n")
    }

    #[test]
    fn text_delta_accumulates() {
        let mut a = ChatTurnSseAccum::default();
        let block = format!(
            "{}{}",
            sse("text_delta", ",\"content\":\"hello \""),
            sse("text_delta", ",\"content\":\"world\""),
        );
        let efx = dispatch_chat_turn_sse_event_block(&block, &mut a, &mut vec![]);
        assert_eq!(a.full_text, "hello world");
        assert!(!efx.is_empty());
    }

    #[test]
    fn reasoning_delta_emits_preview_chunks() {
        let mut a = ChatTurnSseAccum::default();
        let block = format!(
            "{}{}",
            sse("reasoning_delta", ",\"content\":\"hello\""),
            sse("reasoning_delta", ",\"content\":\" z\"")
        );
        let efx = dispatch_chat_turn_sse_event_block(&block, &mut a, &mut vec![]);
        assert_eq!(a.reasoning_content, "hello z");
        let chunks: Vec<&str> = efx
            .iter()
            .filter_map(|e| match e {
                SseRenderEffect::ThinkingPreviewChunk(s) => Some(s.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(chunks, vec!["hello", " z"]);
    }

    #[test]
    fn session_info_captured() {
        let mut a = ChatTurnSseAccum::default();
        dispatch_chat_turn_sse_event_block(
            &sse(
                "session_info",
                ",\"session_id\":\"abc-123\",\"run_id\":\"run-123\"",
            ),
            &mut a,
            &mut vec![],
        );
        assert_eq!(a.session_id.as_deref(), Some("abc-123"));
        assert_eq!(a.run_id.as_deref(), Some("run-123"));
    }

    #[test]
    fn usage_captured() {
        let mut a = ChatTurnSseAccum::default();
        dispatch_chat_turn_sse_event_block(
            &sse("usage", ",\"prompt_tokens\":100,\"completion_tokens\":50"),
            &mut a,
            &mut vec![],
        );
        assert!(a.has_usage);
        assert_eq!(a.prompt_tokens, 100);
        assert_eq!(a.completion_tokens, 50);
    }

    #[test]
    fn tool_call_collected() {
        let mut a = ChatTurnSseAccum::default();
        dispatch_chat_turn_sse_event_block(
            &sse("tool_call", ",\"function\":{\"name\":\"bash\"}"),
            &mut a,
            &mut vec![],
        );
        assert_eq!(a.tool_calls.len(), 1);
        assert_eq!(a.tool_calls[0]["function"]["name"].as_str(), Some("bash"));
    }

    #[test]
    fn tool_call_start_collected_in_canonical_shape() {
        let mut a = ChatTurnSseAccum::default();
        dispatch_chat_turn_sse_event_block(
            &sse(
                "tool_call_start",
                ",\"call_id\":\"tc-1\",\"tool\":\"bash\",\"arguments\":\"{\\\"command\\\":\\\"ls\\\"}\"",
            ),
            &mut a,
            &mut vec![],
        );
        assert_eq!(a.tool_calls.len(), 1);
        assert_eq!(a.tool_calls[0]["id"].as_str(), Some("tc-1"));
        assert_eq!(a.tool_calls[0]["function"]["name"].as_str(), Some("bash"));
        assert_eq!(
            a.tool_calls[0]["function"]["arguments"].as_str(),
            Some("{\"command\":\"ls\"}")
        );
    }

    #[test]
    fn turn_complete_sets_has_tool_calls() {
        let mut a = ChatTurnSseAccum::default();
        dispatch_chat_turn_sse_event_block(
            &sse("turn_complete", ",\"has_tool_calls\":true"),
            &mut a,
            &mut vec![],
        );
        assert!(a.has_tool_calls);
    }

    #[test]
    fn error_captured() {
        let mut a = ChatTurnSseAccum::default();
        dispatch_chat_turn_sse_event_block(
            &sse("error", ",\"message\":\"rate limited\""),
            &mut a,
            &mut vec![],
        );
        assert_eq!(a.error_message.as_deref(), Some("Error: rate limited"));
    }

    #[test]
    fn run_started_captures_run_id() {
        let mut a = ChatTurnSseAccum::default();
        dispatch_chat_turn_sse_event_block(
            &sse("run_started", ",\"run_id\":\"run-42\""),
            &mut a,
            &mut vec![],
        );
        assert_eq!(a.run_id.as_deref(), Some("run-42"));
    }

    #[test]
    fn done_marker_ignored() {
        let mut a = ChatTurnSseAccum::default();
        dispatch_chat_turn_sse_event_block("data: [DONE]\n\n", &mut a, &mut vec![]);
        assert!(a.full_text.is_empty());
    }

    #[test]
    fn invalid_json_ignored() {
        let mut a = ChatTurnSseAccum::default();
        dispatch_chat_turn_sse_event_block("data: {invalid json}\n\n", &mut a, &mut vec![]);
        assert_eq!(
            a.error_message.as_deref(),
            Some("Error: invalid JSON in SSE data")
        );
    }

    #[test]
    fn text_done_fallback_when_no_deltas() {
        let mut a = ChatTurnSseAccum::default();
        dispatch_chat_turn_sse_event_block(
            &sse("text_done", ",\"full_text\":\"complete answer\""),
            &mut a,
            &mut vec![],
        );
        assert_eq!(a.full_text, "complete answer");
    }

    #[test]
    fn thinking_delta_captures_reasoning() {
        let mut a = ChatTurnSseAccum::default();
        let block = format!(
            "{}{}",
            sse("thinking_delta", ",\"content\":\"step 1\""),
            sse("thinking_delta", ",\"content\":\" step 2\""),
        );
        dispatch_chat_turn_sse_event_block(&block, &mut a, &mut vec![]);
        assert_eq!(a.reasoning_content, "step 1 step 2");
    }

    #[test]
    fn reasoning_message_content_captures_reasoning() {
        let mut a = ChatTurnSseAccum::default();
        let block = format!(
            "{}{}",
            sse("reasoning_message_content", ",\"content\":\"step 1\""),
            sse("reasoning_message_content", ",\"content\":\" step 2\""),
        );
        let efx = dispatch_chat_turn_sse_event_block(&block, &mut a, &mut vec![]);
        assert_eq!(a.reasoning_content, "step 1 step 2");
        let chunks: Vec<&str> = efx
            .iter()
            .filter_map(|e| match e {
                SseRenderEffect::ThinkingPreviewChunk(s) => Some(s.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(chunks, vec!["step 1", " step 2"]);
    }

    #[test]
    fn tool_request_enqueues_pending() {
        let mut a = ChatTurnSseAccum::default();
        let mut pending = Vec::new();
        let block = "data: {\"type\":\"tool_request\",\"request_id\":\"tr-1\",\"tool\":\"bash\",\"args\":{\"command\":\"echo x\"}}\n\n";
        dispatch_chat_turn_sse_event_block(block, &mut a, &mut pending);
        assert_eq!(pending.len(), 1);
        match &pending[0] {
            ChatTurnEdgePending::ToolRequest {
                request_id,
                tool,
                args,
            } => {
                assert_eq!(request_id, "tr-1");
                assert_eq!(tool, "bash");
                assert_eq!(args["command"], "echo x");
            }
            _ => panic!("expected ToolRequest"),
        }
    }

    #[test]
    fn approval_required_enqueues_pending() {
        let mut a = ChatTurnSseAccum::default();
        let mut pending = Vec::new();
        let block = "data: {\"type\":\"approval_required\",\"request_id\":\"ap-1\",\"tool\":\"write_file\",\"approval_kind\":\"standard\",\"path\":\"src/x.rs\",\"detail\":\"src/x.rs\"}\n\n";
        dispatch_chat_turn_sse_event_block(block, &mut a, &mut pending);
        assert_eq!(pending.len(), 1);
        match &pending[0] {
            ChatTurnEdgePending::ApprovalRequired {
                request_id,
                tool,
                approval_kind,
                detail,
            } => {
                assert_eq!(request_id, "ap-1");
                assert_eq!(tool, "write_file");
                assert_eq!(*approval_kind, ApprovalKind::Standard);
                assert_eq!(detail.as_deref(), Some("src/x.rs"));
            }
            _ => panic!("expected ApprovalRequired"),
        }
    }

    #[test]
    fn approval_required_without_kind_defaults_to_explicit() {
        let mut a = ChatTurnSseAccum::default();
        let mut pending = Vec::new();
        let block = "data: {\"type\":\"approval_required\",\"request_id\":\"ap-1\",\"tool\":\"bash\",\"detail\":\"rm -rf tmp\"}\n\n";
        dispatch_chat_turn_sse_event_block(block, &mut a, &mut pending);
        match &pending[0] {
            ChatTurnEdgePending::ApprovalRequired { approval_kind, .. } => {
                assert_eq!(*approval_kind, ApprovalKind::Explicit);
            }
            other => panic!("expected ApprovalRequired, got {other:?}"),
        }
    }

    #[test]
    fn approval_batch_required_enqueues_pending() {
        let mut a = ChatTurnSseAccum::default();
        let mut pending = Vec::new();
        let block = "data: {\"type\":\"approval_batch_required\",\"requests\":[{\"request_id\":\"ap-1\",\"tool\":\"write_file\",\"approval_kind\":\"standard\",\"detail\":\"src/a.rs\"},{\"request_id\":\"ap-2\",\"tool\":\"write_file\",\"approval_kind\":\"standard\",\"detail\":\"src/b.rs\"}]}\n\n";
        dispatch_chat_turn_sse_event_block(block, &mut a, &mut pending);
        assert_eq!(pending.len(), 1);
        match &pending[0] {
            ChatTurnEdgePending::ApprovalBatchRequired { requests } => {
                assert_eq!(requests.len(), 2);
                assert_eq!(requests[0].request_id, "ap-1");
                assert_eq!(requests[1].detail.as_deref(), Some("src/b.rs"));
                assert_eq!(requests[0].approval_kind, ApprovalKind::Standard);
            }
            other => panic!("expected ApprovalBatchRequired, got {other:?}"),
        }
    }

    #[test]
    fn framer_splits_event_across_chunks() {
        let ev = sse("session_info", ",\"session_id\":\"split-id\"");
        let mid = ev.find("session").unwrap();
        let mut f = ChatTurnSseFramer::new();
        assert!(f.push_lossy_bytes(&ev.as_bytes()[..mid]).is_empty());
        let blocks = f.push_lossy_bytes(&ev.as_bytes()[mid..]);
        assert_eq!(blocks.len(), 1);
        let mut a = ChatTurnSseAccum::default();
        dispatch_chat_turn_sse_event_block(&blocks[0], &mut a, &mut vec![]);
        assert_eq!(a.session_id.as_deref(), Some("split-id"));
    }

    #[test]
    fn invalid_usage_payload_sets_error() {
        let mut a = ChatTurnSseAccum::default();
        dispatch_chat_turn_sse_event_block("data: {\"type\":\"usage\"}\n\n", &mut a, &mut vec![]);
        assert_eq!(
            a.error_message.as_deref(),
            Some("Error: invalid usage payload")
        );
        assert!(!a.has_usage);
    }

    #[test]
    fn framer_ttft_on_text_delta_block() {
        let block = sse("text_delta", ",\"content\":\"x\"");
        let mut f = ChatTurnSseFramer::new();
        let _ = f.push_lossy_bytes(block.as_bytes());
        assert!(f.ttft_ms.is_some());
    }

    #[test]
    fn framer_ttft_on_reasoning_delta_block() {
        let block = sse("reasoning_delta", ",\"content\":\"thinking...\"");
        let mut f = ChatTurnSseFramer::new();
        let _ = f.push_lossy_bytes(block.as_bytes());
        assert!(f.ttft_ms.is_some());
    }

    #[test]
    fn framer_ttft_on_reasoning_message_content_block() {
        let block = sse("reasoning_message_content", ",\"content\":\"thinking...\"");
        let mut f = ChatTurnSseFramer::new();
        let _ = f.push_lossy_bytes(block.as_bytes());
        assert!(f.ttft_ms.is_some());
    }

    #[test]
    fn framer_ttft_on_tool_call_start_block() {
        // LLM responds with only tool calls (no text) — ttft must still be recorded.
        let block = sse("tool_call_start", ",\"id\":\"call-1\",\"name\":\"bash\"");
        let mut f = ChatTurnSseFramer::new();
        let _ = f.push_lossy_bytes(block.as_bytes());
        assert!(
            f.ttft_ms.is_some(),
            "ttft must be set when first SSE event is tool_call_start"
        );
    }

    #[test]
    fn framer_ttft_on_tool_call_block() {
        let block = sse(
            "tool_call",
            ",\"id\":\"call-1\",\"name\":\"bash\",\"arguments\":\"{}\"}",
        );
        let mut f = ChatTurnSseFramer::new();
        let _ = f.push_lossy_bytes(block.as_bytes());
        assert!(
            f.ttft_ms.is_some(),
            "ttft must be set when first SSE event is tool_call"
        );
    }

    #[test]
    fn framer_ttft_not_set_on_usage_only() {
        // usage events alone should not trigger ttft
        let block = sse("usage", ",\"prompt_tokens\":100,\"completion_tokens\":5");
        let mut f = ChatTurnSseFramer::new();
        let _ = f.push_lossy_bytes(block.as_bytes());
        assert!(f.ttft_ms.is_none(), "usage-only event must not set ttft");
    }

    // ── Cache token tests ────────────────────────────────────────────────

    #[test]
    fn usage_with_cache_tokens_parsed() {
        let mut a = ChatTurnSseAccum::default();
        dispatch_chat_turn_sse_event_block(
            &sse(
                "usage",
                ",\"prompt_tokens\":100,\"completion_tokens\":50,\"cache_read_tokens\":25,\"cache_creation_tokens\":10",
            ),
            &mut a,
            &mut vec![],
        );
        assert!(a.has_usage);
        assert_eq!(a.prompt_tokens, 100);
        assert_eq!(a.completion_tokens, 50);
        assert_eq!(a.cache_read_tokens, 25);
        assert_eq!(a.cache_creation_tokens, 10);
    }

    #[test]
    fn usage_cache_tokens_default_to_zero_when_missing() {
        let mut a = ChatTurnSseAccum::default();
        dispatch_chat_turn_sse_event_block(
            &sse("usage", ",\"prompt_tokens\":100,\"completion_tokens\":50"),
            &mut a,
            &mut vec![],
        );
        assert!(a.has_usage);
        assert_eq!(a.cache_read_tokens, 0);
        assert_eq!(a.cache_creation_tokens, 0);
    }

    #[test]
    fn usage_cache_tokens_null_treated_as_zero() {
        let mut a = ChatTurnSseAccum::default();
        dispatch_chat_turn_sse_event_block(
            &sse(
                "usage",
                ",\"prompt_tokens\":100,\"completion_tokens\":50,\"cache_read_tokens\":null,\"cache_creation_tokens\":null",
            ),
            &mut a,
            &mut vec![],
        );
        assert_eq!(a.cache_read_tokens, 0);
        assert_eq!(a.cache_creation_tokens, 0);
    }

    #[test]
    fn usage_without_prompt_or_completion_is_error_and_ignores_cache() {
        let mut a = ChatTurnSseAccum::default();
        dispatch_chat_turn_sse_event_block(
            &sse(
                "usage",
                ",\"cache_read_tokens\":500,\"cache_creation_tokens\":100",
            ),
            &mut a,
            &mut vec![],
        );
        // Early return: no prompt/completion → error, cache tokens not parsed
        assert!(!a.has_usage);
        assert_eq!(a.cache_read_tokens, 0);
        assert_eq!(a.cache_creation_tokens, 0);
        assert!(a.error_message.is_some());
    }

    #[test]
    fn usage_second_event_overwrites_cache_tokens() {
        let mut a = ChatTurnSseAccum::default();
        let block = format!(
            "{}{}",
            sse(
                "usage",
                ",\"prompt_tokens\":100,\"completion_tokens\":50,\"cache_read_tokens\":30,\"cache_creation_tokens\":10"
            ),
            sse(
                "usage",
                ",\"prompt_tokens\":200,\"completion_tokens\":80,\"cache_read_tokens\":60,\"cache_creation_tokens\":0"
            ),
        );
        dispatch_chat_turn_sse_event_block(&block, &mut a, &mut vec![]);
        assert_eq!(a.prompt_tokens, 200);
        assert_eq!(a.completion_tokens, 80);
        assert_eq!(a.cache_read_tokens, 60);
        assert_eq!(a.cache_creation_tokens, 0);
    }

    // -----------------------------------------------------------------------
    // Unhappy-path / edge-case tests
    // -----------------------------------------------------------------------

    #[test]
    fn text_delta_missing_content_field_no_panic() {
        let mut a = ChatTurnSseAccum::default();
        dispatch_chat_turn_sse_event_block(&sse("text_delta", ""), &mut a, &mut vec![]);
        assert_eq!(a.full_text, "");
    }

    #[test]
    fn text_delta_null_content_ignored() {
        let mut a = ChatTurnSseAccum::default();
        dispatch_chat_turn_sse_event_block(
            &sse("text_delta", ",\"content\":null"),
            &mut a,
            &mut vec![],
        );
        assert_eq!(a.full_text, "");
    }

    #[test]
    fn text_delta_numeric_content_ignored() {
        let mut a = ChatTurnSseAccum::default();
        dispatch_chat_turn_sse_event_block(
            &sse("text_delta", ",\"content\":42"),
            &mut a,
            &mut vec![],
        );
        assert_eq!(a.full_text, "");
    }

    #[test]
    fn tool_request_missing_id_not_pushed() {
        let mut a = ChatTurnSseAccum::default();
        let mut pending = vec![];
        dispatch_chat_turn_sse_event_block(
            &sse("tool_request", ",\"tool\":\"read_file\",\"args\":{}"),
            &mut a,
            &mut pending,
        );
        // Empty request_id → not pushed
        assert!(pending.is_empty());
    }

    #[test]
    fn tool_request_missing_tool_not_pushed() {
        let mut a = ChatTurnSseAccum::default();
        let mut pending = vec![];
        dispatch_chat_turn_sse_event_block(
            &sse("tool_request", ",\"request_id\":\"r1\",\"args\":{}"),
            &mut a,
            &mut pending,
        );
        // Empty tool → not pushed
        assert!(pending.is_empty());
    }

    #[test]
    fn tool_request_missing_args_defaults_to_empty_object() {
        let mut a = ChatTurnSseAccum::default();
        let mut pending = vec![];
        dispatch_chat_turn_sse_event_block(
            &sse("tool_request", ",\"request_id\":\"r1\",\"tool\":\"bash\""),
            &mut a,
            &mut pending,
        );
        assert_eq!(pending.len(), 1);
        if let ChatTurnEdgePending::ToolRequest { args, .. } = &pending[0] {
            assert!(args.is_object());
            assert!(args.as_object().unwrap().is_empty());
        } else {
            panic!("expected ToolRequest");
        }
    }

    #[test]
    fn approval_required_missing_id_not_pushed() {
        let mut a = ChatTurnSseAccum::default();
        let mut pending = vec![];
        dispatch_chat_turn_sse_event_block(
            &sse("approval_required", ",\"tool\":\"write_file\""),
            &mut a,
            &mut pending,
        );
        assert!(pending.is_empty());
    }

    #[test]
    fn approval_required_missing_tool_not_pushed() {
        let mut a = ChatTurnSseAccum::default();
        let mut pending = vec![];
        dispatch_chat_turn_sse_event_block(
            &sse("approval_required", ",\"request_id\":\"r1\""),
            &mut a,
            &mut pending,
        );
        assert!(pending.is_empty());
    }

    #[test]
    fn usage_negative_tokens_treated_as_missing() {
        let mut a = ChatTurnSseAccum::default();
        // as_u64() returns None for negative values
        dispatch_chat_turn_sse_event_block(
            &sse("usage", ",\"prompt_tokens\":-1,\"completion_tokens\":-5"),
            &mut a,
            &mut vec![],
        );
        // Negative i64 fails as_u64() → both None → error
        assert!(a.error_message.is_some());
        assert!(!a.has_usage);
    }

    #[test]
    fn usage_float_tokens_treated_as_zero() {
        let mut a = ChatTurnSseAccum::default();
        dispatch_chat_turn_sse_event_block(
            &sse("usage", ",\"prompt_tokens\":1.5,\"completion_tokens\":2.7"),
            &mut a,
            &mut vec![],
        );
        // as_u64() returns None for floats → falls through to unwrap_or(0)
        // But at least one must be present as integer for has_usage
        assert!(a.error_message.is_some());
    }

    #[test]
    fn usage_missing_cache_tokens_default_to_zero() {
        let mut a = ChatTurnSseAccum::default();
        dispatch_chat_turn_sse_event_block(
            &sse("usage", ",\"prompt_tokens\":100,\"completion_tokens\":50"),
            &mut a,
            &mut vec![],
        );
        assert!(a.has_usage);
        assert_eq!(a.cache_read_tokens, 0);
        assert_eq!(a.cache_creation_tokens, 0);
    }

    #[test]
    fn multiple_errors_last_wins() {
        let mut a = ChatTurnSseAccum::default();
        let block = format!(
            "{}{}",
            sse("error", ",\"message\":\"rate limited\""),
            sse("error", ",\"message\":\"server error\""),
        );
        dispatch_chat_turn_sse_event_block(&block, &mut a, &mut vec![]);
        // Each error overwrites the previous — last error wins.
        assert!(a.error_message.as_ref().unwrap().contains("server error"));
    }

    #[test]
    fn error_event_missing_message_says_unknown() {
        let mut a = ChatTurnSseAccum::default();
        dispatch_chat_turn_sse_event_block(&sse("error", ""), &mut a, &mut vec![]);
        assert_eq!(a.error_message.as_deref(), Some("Error: unknown error"));
    }

    #[test]
    fn unknown_event_type_extracts_run_id() {
        let mut a = ChatTurnSseAccum::default();
        dispatch_chat_turn_sse_event_block(
            &sse("some_future_event", ",\"run_id\":\"run-42\""),
            &mut a,
            &mut vec![],
        );
        assert_eq!(a.run_id.as_deref(), Some("run-42"));
    }

    #[test]
    fn unknown_event_without_run_id_is_noop() {
        let mut a = ChatTurnSseAccum::default();
        dispatch_chat_turn_sse_event_block(
            &sse("some_future_event", ",\"data\":123"),
            &mut a,
            &mut vec![],
        );
        assert!(a.run_id.is_none());
        assert!(a.error_message.is_none());
    }

    #[test]
    fn empty_block_is_noop() {
        let mut a = ChatTurnSseAccum::default();
        let efx = dispatch_chat_turn_sse_event_block("", &mut a, &mut vec![]);
        assert!(efx.is_empty());
        assert_eq!(a.full_text, "");
        assert!(a.error_message.is_none());
    }

    #[test]
    fn whitespace_only_block_is_noop() {
        let mut a = ChatTurnSseAccum::default();
        let efx = dispatch_chat_turn_sse_event_block("  \n\n  \n", &mut a, &mut vec![]);
        assert!(efx.is_empty());
    }

    #[test]
    fn done_only_block_is_noop() {
        let mut a = ChatTurnSseAccum::default();
        let efx = dispatch_chat_turn_sse_event_block("data: [DONE]\n\n", &mut a, &mut vec![]);
        assert!(efx.is_empty());
        assert!(!a.has_usage);
    }

    #[test]
    fn invalid_json_sets_error() {
        let mut a = ChatTurnSseAccum::default();
        let efx =
            dispatch_chat_turn_sse_event_block("data: {not valid json}\n\n", &mut a, &mut vec![]);
        assert!(a.error_message.as_ref().unwrap().contains("invalid JSON"));
        // Should also emit StopThinkingSpinner
        assert!(
            efx.iter()
                .any(|e| matches!(e, SseRenderEffect::StopThinkingSpinner))
        );
    }

    #[test]
    fn invalid_json_then_valid_event_still_works() {
        let mut a = ChatTurnSseAccum::default();
        let block = format!(
            "data: {{bad json}}\n\n{}",
            sse("text_delta", ",\"content\":\"ok\""),
        );
        dispatch_chat_turn_sse_event_block(&block, &mut a, &mut vec![]);
        assert_eq!(a.full_text, "ok");
        // Error from first event preserved
        assert!(a.error_message.is_some());
    }

    #[test]
    fn event_missing_type_field_treated_as_unknown() {
        let mut a = ChatTurnSseAccum::default();
        dispatch_chat_turn_sse_event_block("data: {\"run_id\":\"r1\"}\n\n", &mut a, &mut vec![]);
        // Missing "type" → unwrap_or("") → falls into _ arm → extracts run_id
        assert_eq!(a.run_id.as_deref(), Some("r1"));
    }

    #[test]
    fn framer_handles_empty_bytes() {
        let mut f = ChatTurnSseFramer::new();
        let blocks = f.push_lossy_bytes(&[]);
        assert!(blocks.is_empty());
        assert!(f.ttft_ms.is_none());
    }

    #[test]
    fn framer_trailing_blob_on_empty_returns_empty_string() {
        let mut f = ChatTurnSseFramer::new();
        let tail = f.take_trailing_dispatch_blob();
        assert_eq!(tail, "");
    }

    #[test]
    fn framer_invalid_utf8_lossy_converts() {
        let mut f = ChatTurnSseFramer::new();
        // Invalid UTF-8 sequence: 0xFF is never valid
        let data = b"data: {\"type\":\"text_delta\",\"content\":\"hi\xff\"}\n\n";
        let blocks = f.push_lossy_bytes(data);
        assert_eq!(blocks.len(), 1);
        // Lossy conversion replaces invalid bytes with U+FFFD
        assert!(blocks[0].contains('\u{FFFD}') || blocks[0].contains("hi"));
    }

    #[test]
    fn session_info_missing_session_id_is_noop() {
        let mut a = ChatTurnSseAccum::default();
        dispatch_chat_turn_sse_event_block(
            &sse("session_info", ",\"other_field\":\"val\""),
            &mut a,
            &mut vec![],
        );
        assert!(a.session_id.is_none());
    }

    #[test]
    fn turn_complete_missing_has_tool_calls_defaults_false() {
        let mut a = ChatTurnSseAccum::default();
        dispatch_chat_turn_sse_event_block(&sse("turn_complete", ""), &mut a, &mut vec![]);
        assert!(!a.has_tool_calls);
    }

    #[test]
    fn thinking_delta_missing_content_no_panic() {
        let mut a = ChatTurnSseAccum::default();
        let efx =
            dispatch_chat_turn_sse_event_block(&sse("thinking_delta", ""), &mut a, &mut vec![]);
        assert_eq!(a.reasoning_content, "");
        assert!(
            efx.iter()
                .any(|e| matches!(e, SseRenderEffect::StartThinkingSpinner))
        );
    }

    #[test]
    fn thinking_delta_empty_string_no_preview_chunk() {
        let mut a = ChatTurnSseAccum::default();
        let efx = dispatch_chat_turn_sse_event_block(
            &sse("thinking_delta", ",\"content\":\"\""),
            &mut a,
            &mut vec![],
        );
        // Empty content: spinner started but no ThinkingPreviewChunk emitted
        assert!(
            efx.iter()
                .any(|e| matches!(e, SseRenderEffect::StartThinkingSpinner))
        );
        assert!(
            !efx.iter()
                .any(|e| matches!(e, SseRenderEffect::ThinkingPreviewChunk(_)))
        );
    }

    #[test]
    fn text_done_only_fills_when_full_text_empty() {
        let a_default = ChatTurnSseAccum::default();
        let mut a = ChatTurnSseAccum {
            full_text: "already set".to_string(),
            ..a_default
        };
        dispatch_chat_turn_sse_event_block(
            &sse("text_done", ",\"full_text\":\"should not overwrite\""),
            &mut a,
            &mut vec![],
        );
        assert_eq!(a.full_text, "already set");
    }

    #[test]
    fn text_done_fills_empty_full_text() {
        let mut a = ChatTurnSseAccum::default();
        dispatch_chat_turn_sse_event_block(
            &sse("text_done", ",\"full_text\":\"complete response\""),
            &mut a,
            &mut vec![],
        );
        assert_eq!(a.full_text, "complete response");
    }

    // ── Additional edge-case tests ─────────────────────────────────────────

    #[test]
    fn usage_negative_tokens_treated_as_invalid() {
        let mut a = ChatTurnSseAccum::default();
        dispatch_chat_turn_sse_event_block(
            &sse("usage", ",\"prompt_tokens\":-5,\"completion_tokens\":-10"),
            &mut a,
            &mut vec![],
        );
        // Negative values fail as_u64() → both None → error branch
        assert!(!a.has_usage);
        assert!(a.error_message.is_some());
        assert!(a.error_message.as_ref().unwrap().contains("invalid usage"));
    }

    #[test]
    fn usage_float_tokens_treated_as_error() {
        let mut a = ChatTurnSseAccum::default();
        // Float values cannot be parsed as i64 by serde, so as_i64() returns None.
        dispatch_chat_turn_sse_event_block(
            "data: {\"type\":\"usage\",\"prompt_tokens\":3.14,\"completion_tokens\":2.71}\n\n",
            &mut a,
            &mut vec![],
        );
        // The parser falls through to the "neither prompt nor completion" branch
        // and sets an error, OR it just stores 0. Either way, no panic.
        assert!(a.has_usage || a.error_message.is_some());
    }

    #[test]
    fn empty_block_produces_no_effects() {
        let mut a = ChatTurnSseAccum::default();
        let efx = dispatch_chat_turn_sse_event_block("", &mut a, &mut vec![]);
        assert!(efx.is_empty());
        assert!(a.full_text.is_empty());
    }

    #[test]
    fn whitespace_only_block_produces_no_effects() {
        let mut a = ChatTurnSseAccum::default();
        let efx = dispatch_chat_turn_sse_event_block("   \n\n  \n", &mut a, &mut vec![]);
        assert!(efx.is_empty());
    }

    #[test]
    fn done_marker_only_produces_no_effects() {
        let mut a = ChatTurnSseAccum::default();
        let efx = dispatch_chat_turn_sse_event_block("data: [DONE]\n\n", &mut a, &mut vec![]);
        assert!(efx.is_empty());
    }

    #[test]
    fn invalid_json_in_data_line_sets_error() {
        let mut a = ChatTurnSseAccum::default();
        dispatch_chat_turn_sse_event_block("data: {not valid json}\n\n", &mut a, &mut vec![]);
        assert!(a.error_message.is_some());
    }

    #[test]
    fn session_info_captures_id() {
        let mut a = ChatTurnSseAccum::default();
        dispatch_chat_turn_sse_event_block(
            &sse("session_info", ",\"session_id\":\"sess-abc\""),
            &mut a,
            &mut vec![],
        );
        assert_eq!(a.session_id.as_deref(), Some("sess-abc"));
    }

    #[test]
    fn turn_complete_with_tool_calls_true() {
        let mut a = ChatTurnSseAccum::default();
        dispatch_chat_turn_sse_event_block(
            &sse("turn_complete", ",\"has_tool_calls\":true"),
            &mut a,
            &mut vec![],
        );
        assert!(a.has_tool_calls);
    }

    #[test]
    fn turn_complete_without_tool_calls_stays_false() {
        let mut a = ChatTurnSseAccum::default();
        dispatch_chat_turn_sse_event_block(
            &sse("turn_complete", ",\"has_tool_calls\":false"),
            &mut a,
            &mut vec![],
        );
        assert!(!a.has_tool_calls);
    }

    #[test]
    fn explain_event_collected() {
        let mut a = ChatTurnSseAccum::default();
        dispatch_chat_turn_sse_event_block(
            &sse("explain", ",\"detail\":\"selection took 5ms\""),
            &mut a,
            &mut vec![],
        );
        assert_eq!(a.explain_turns.len(), 1);
    }

    #[test]
    fn tool_call_with_empty_id_gets_synthetic_uuid() {
        let mut a = ChatTurnSseAccum::default();
        // Simulate a model returning a tool_call with empty id
        dispatch_chat_turn_sse_event_block(
            &sse(
                "tool_call_start",
                ",\"id\":\"\",\"name\":\"bash\",\"arguments\":\"{\\\"command\\\":\\\"ls\\\"}\"",
            ),
            &mut a,
            &mut vec![],
        );
        assert_eq!(a.tool_calls.len(), 1);
        let id = a.tool_calls[0]["id"].as_str().unwrap();
        assert!(
            !id.is_empty(),
            "empty tool_call id must be replaced with a synthetic UUID"
        );
    }

    #[test]
    fn tool_call_with_missing_id_gets_synthetic_uuid() {
        let mut a = ChatTurnSseAccum::default();
        // No id field at all
        dispatch_chat_turn_sse_event_block(
            &sse("tool_call_start", ",\"name\":\"grep\",\"arguments\":\"{}\""),
            &mut a,
            &mut vec![],
        );
        assert_eq!(a.tool_calls.len(), 1);
        let id = a.tool_calls[0]["id"].as_str().unwrap();
        assert!(
            !id.is_empty(),
            "missing tool_call id must be replaced with a synthetic UUID"
        );
    }

    /// tool_call event with same id as prior tool_call_start must merge (update),
    /// not duplicate. Qwen-plus sends tool_call_start with partial args, then
    /// tool_call with complete args for the same call_id.
    #[test]
    fn tool_call_merges_into_existing_tool_call_start() {
        let mut a = ChatTurnSseAccum::default();
        let mut p = vec![];
        // tool_call_start arrives first with partial arguments
        dispatch_chat_turn_sse_event_block(
            &sse(
                "tool_call_start",
                ",\"call_id\":\"tc-1\",\"tool\":\"git_log\",\"arguments\":\"{\\\"n\"",
            ),
            &mut a,
            &mut p,
        );
        assert_eq!(a.tool_calls.len(), 1);
        assert_eq!(a.tool_call_id_index.get("tc-1"), Some(&0));
        // tool_call arrives with complete arguments — same id
        dispatch_chat_turn_sse_event_block(
            &sse(
                "tool_call",
                ",\"id\":\"tc-1\",\"name\":\"git_log\",\"arguments\":{\"n\":5}",
            ),
            &mut a,
            &mut p,
        );
        // Must still be 1 entry, not 2
        assert_eq!(
            a.tool_calls.len(),
            1,
            "tool_call should merge, not duplicate"
        );
        assert_eq!(a.tool_calls[0]["id"].as_str(), Some("tc-1"));
        assert_eq!(
            a.tool_calls[0]["function"]["arguments"].as_str(),
            Some("{\"n\":5}")
        );
        assert_eq!(a.tool_call_id_index.get("tc-1"), Some(&0));
    }

    /// tool_call with a new id (no prior tool_call_start) appends normally.
    #[test]
    fn tool_call_without_prior_start_appends() {
        let mut a = ChatTurnSseAccum::default();
        let mut p = vec![];
        dispatch_chat_turn_sse_event_block(
            &sse(
                "tool_call",
                ",\"id\":\"tc-new\",\"name\":\"bash\",\"arguments\":{}",
            ),
            &mut a,
            &mut p,
        );
        assert_eq!(a.tool_calls.len(), 1);
        assert_eq!(a.tool_calls[0]["id"].as_str(), Some("tc-new"));
        assert_eq!(a.tool_call_id_index.get("tc-new"), Some(&0));
    }

    // ── Phase-R adversarial regression: usage nested fallback (Bug B) ──

    /// Regression: a provider that nests usage counters under
    /// `"usage": {...}` must still be decoded correctly. Before the fix,
    /// these counters silently zeroed out.
    #[test]
    fn usage_nested_fallback_captures_all_four_counters() {
        let mut a = ChatTurnSseAccum::default();
        dispatch_chat_turn_sse_event_block(
            "data: {\"type\":\"usage\",\"usage\":{\"prompt_tokens\":101,\"completion_tokens\":42,\"cache_read_tokens\":7,\"cache_creation_tokens\":13}}\n\n",
            &mut a,
            &mut vec![],
        );
        assert!(a.has_usage);
        assert_eq!(a.prompt_tokens, 101);
        assert_eq!(a.completion_tokens, 42);
        assert_eq!(a.cache_read_tokens, 7);
        assert_eq!(a.cache_creation_tokens, 13);
        assert!(
            a.error_message.is_none(),
            "no invalid-usage error for nested shape"
        );
    }

    /// Contract pin: when BOTH flat and nested are present, flat wins.
    #[test]
    fn usage_flat_wins_over_nested() {
        let mut a = ChatTurnSseAccum::default();
        dispatch_chat_turn_sse_event_block(
            "data: {\"type\":\"usage\",\"prompt_tokens\":1,\"completion_tokens\":2,\"cache_read_tokens\":3,\"cache_creation_tokens\":4,\"usage\":{\"prompt_tokens\":999,\"completion_tokens\":999,\"cache_read_tokens\":999,\"cache_creation_tokens\":999}}\n\n",
            &mut a,
            &mut vec![],
        );
        assert_eq!(a.prompt_tokens, 1);
        assert_eq!(a.completion_tokens, 2);
        assert_eq!(a.cache_read_tokens, 3);
        assert_eq!(a.cache_creation_tokens, 4);
    }

    /// Mixed shape: flat prompt/completion, nested cache_* still decoded.
    #[test]
    fn usage_mixed_flat_and_nested_per_field() {
        let mut a = ChatTurnSseAccum::default();
        dispatch_chat_turn_sse_event_block(
            "data: {\"type\":\"usage\",\"prompt_tokens\":50,\"completion_tokens\":10,\"usage\":{\"cache_read_tokens\":11,\"cache_creation_tokens\":22}}\n\n",
            &mut a,
            &mut vec![],
        );
        assert_eq!(a.prompt_tokens, 50);
        assert_eq!(a.completion_tokens, 10);
        assert_eq!(a.cache_read_tokens, 11);
        assert_eq!(a.cache_creation_tokens, 22);
    }

    // ── Phase-R adversarial regression: nested function.id (Bug C) ──

    /// Regression: `tool_call` with only nested `function.id` (no top-level
    /// id/call_id) must preserve that id instead of minting a UUID.
    /// Downstream tool_call_id matching breaks if a UUID is synthesized.
    #[test]
    fn tool_call_uses_nested_function_id_when_top_level_missing() {
        let mut a = ChatTurnSseAccum::default();
        dispatch_chat_turn_sse_event_block(
            "data: {\"type\":\"tool_call\",\"function\":{\"id\":\"real-id-42\",\"name\":\"bash\",\"arguments\":\"{}\"}}\n\n",
            &mut a,
            &mut vec![],
        );
        assert_eq!(a.tool_calls.len(), 1);
        assert_eq!(
            a.tool_calls[0]["id"].as_str(),
            Some("real-id-42"),
            "nested function.id must be preserved, not replaced by a UUID"
        );
        assert_eq!(a.tool_call_id_index.get("real-id-42"), Some(&0));
    }

    #[test]
    fn tool_call_uses_nested_function_call_id_when_top_level_missing() {
        let mut a = ChatTurnSseAccum::default();
        dispatch_chat_turn_sse_event_block(
            "data: {\"type\":\"tool_call\",\"function\":{\"call_id\":\"real-call-7\",\"name\":\"bash\",\"arguments\":\"{}\"}}\n\n",
            &mut a,
            &mut vec![],
        );
        assert_eq!(a.tool_calls.len(), 1);
        assert_eq!(a.tool_calls[0]["id"].as_str(), Some("real-call-7"));
    }

    /// Contract pin: top-level `id` still wins over nested function.id —
    /// guarding against a regression introduced by the Bug-C fix.
    #[test]
    fn tool_call_top_level_id_wins_over_nested_function_id() {
        let mut a = ChatTurnSseAccum::default();
        dispatch_chat_turn_sse_event_block(
            "data: {\"type\":\"tool_call\",\"id\":\"top-id\",\"function\":{\"id\":\"nested-id\",\"name\":\"bash\",\"arguments\":\"{}\"}}\n\n",
            &mut a,
            &mut vec![],
        );
        assert_eq!(a.tool_calls.len(), 1);
        assert_eq!(a.tool_calls[0]["id"].as_str(), Some("top-id"));
    }

    /// Contract pin: no id anywhere → UUID is minted (not left empty /
    /// not rejected). Preserves pre-fix behavior for purely id-less events.
    #[test]
    fn tool_call_with_no_id_anywhere_still_mints_uuid() {
        let mut a = ChatTurnSseAccum::default();
        dispatch_chat_turn_sse_event_block(
            "data: {\"type\":\"tool_call\",\"function\":{\"name\":\"bash\",\"arguments\":\"{}\"}}\n\n",
            &mut a,
            &mut vec![],
        );
        assert_eq!(a.tool_calls.len(), 1);
        let id = a.tool_calls[0]["id"].as_str().expect("id present");
        assert!(!id.is_empty(), "id must be minted, not empty");
        // UUID v7 is 36 chars (8-4-4-4-12) with dashes.
        assert_eq!(id.len(), 36, "minted id should be a UUID string");
    }

    // ── SSE dispatch contract pins ─────────────────────────────────────

    /// Current contract: `[DONE]` is NOT terminal — subsequent `data:`
    /// lines in the same block are still processed. This is arguably a
    /// bug (a real `[DONE]` should stop parsing) but is preserved here
    /// deliberately for now; changing it is out of scope for this PR.
    /// TODO(astra-stream-protocol): decide whether `[DONE]` should stop
    /// dispatch of subsequent lines in the same block.
    #[test]
    fn done_marker_is_non_terminal_subsequent_lines_still_processed() {
        let mut a = ChatTurnSseAccum::default();
        let block = "data: [DONE]\ndata: {\"type\":\"text_delta\",\"content\":\"after-done\"}\n\n";
        dispatch_chat_turn_sse_event_block(block, &mut a, &mut vec![]);
        assert_eq!(
            a.full_text, "after-done",
            "data lines after [DONE] in the same block are currently still processed"
        );
    }

    /// Contract pin: only the FIRST malformed JSON line sets
    /// `error_message`. Later malformed lines must NOT overwrite the
    /// earlier message — the consumer surfaces the first symptom.
    #[test]
    fn malformed_json_only_first_sets_error_message() {
        let mut a = ChatTurnSseAccum::default();
        let block = "data: {not json one}\ndata: {not json two}\n\n";
        dispatch_chat_turn_sse_event_block(block, &mut a, &mut vec![]);
        assert_eq!(
            a.error_message.as_deref(),
            Some("Error: invalid JSON in SSE data"),
            "first malformed line sets the canonical error"
        );
        // Now simulate a subsequent block with another malformed line and
        // verify the prior error_message is preserved (not overwritten).
        let before = a.error_message.clone();
        dispatch_chat_turn_sse_event_block("data: {still not}\n\n", &mut a, &mut vec![]);
        assert_eq!(a.error_message, before);
    }

    /// Contract pin: a `tool_call` event that supplies only the top-level
    /// `id` (no `function.id`, no `call_id`) still works — not regressed
    /// by the Bug-C nested fallback.
    #[test]
    fn tool_call_with_only_top_level_id_still_works() {
        let mut a = ChatTurnSseAccum::default();
        dispatch_chat_turn_sse_event_block(
            "data: {\"type\":\"tool_call\",\"id\":\"classic-1\",\"function\":{\"name\":\"bash\",\"arguments\":\"{}\"}}\n\n",
            &mut a,
            &mut vec![],
        );
        assert_eq!(a.tool_calls.len(), 1);
        assert_eq!(a.tool_calls[0]["id"].as_str(), Some("classic-1"));
        assert_eq!(a.tool_calls[0]["function"]["name"].as_str(), Some("bash"));
        assert_eq!(a.tool_call_id_index.get("classic-1"), Some(&0));
    }
}
