//! JSON `type` dispatch for astra `/chat/turn` SSE event blocks (blank-line framed).
//!
//! Shared between the CLI stream consumer and any future headless client: updates a structured
//! accumulator and returns terminal UI hints. [`ChatTurnSseFramer`] turns arbitrary byte chunks
//! into complete event blocks via [`super::sse_blocks`] and records time-to-first-token.

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
    pub explain_turns: Vec<Value>,
    pub has_tool_calls: bool,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_creation_tokens: u64,
    pub has_usage: bool,
    pub error_message: Option<String>,
}

/// Deferred edge work from `tool_request` / `approval_required` events.
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
        path: Option<String>,
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
        "thinking_delta" | "reasoning_delta" => {
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
        }
        "tool_call" => {
            accum.tool_calls.push(event.clone());
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
            let path = event
                .get("path")
                .and_then(|v| v.as_str())
                .map(std::string::ToString::to_string);
            if !request_id.is_empty() {
                edge_pending.push(ChatTurnEdgePending::ApprovalRequired {
                    request_id,
                    tool,
                    path,
                });
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
        }
        "usage" => {
            let prompt = event.get("prompt_tokens").and_then(|v| v.as_u64());
            let completion = event.get("completion_tokens").and_then(|v| v.as_u64());
            if prompt.is_none() && completion.is_none() {
                if accum.error_message.is_none() {
                    accum.error_message = Some("Error: invalid usage payload".to_string());
                }
                return;
            }
            accum.prompt_tokens = prompt.unwrap_or(0);
            accum.completion_tokens = completion.unwrap_or(0);
            accum.cache_read_tokens = event
                .get("cache_read_tokens")
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
            accum.cache_creation_tokens = event
                .get("cache_creation_tokens")
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
            accum.has_usage = true;
        }
        "error" => {
            let msg = event
                .get("message")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown error");
            accum.error_message = Some(format!("Error: {msg}"));
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
/// records [`ChatTurnSseFramer::ttft_ms`] on the first `text_delta` / `content_block_delta` payload.
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
        if event_block.contains("\"text_delta\"") || event_block.contains("\"content_block_delta\"")
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
            &sse("session_info", ",\"session_id\":\"abc-123\""),
            &mut a,
            &mut vec![],
        );
        assert_eq!(a.session_id.as_deref(), Some("abc-123"));
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
        let block = "data: {\"type\":\"approval_required\",\"request_id\":\"ap-1\",\"tool\":\"write_file\",\"path\":\"src/x.rs\"}\n\n";
        dispatch_chat_turn_sse_event_block(block, &mut a, &mut pending);
        assert_eq!(pending.len(), 1);
        match &pending[0] {
            ChatTurnEdgePending::ApprovalRequired {
                request_id,
                tool,
                path,
            } => {
                assert_eq!(request_id, "ap-1");
                assert_eq!(tool, "write_file");
                assert_eq!(path.as_deref(), Some("src/x.rs"));
            }
            _ => panic!("expected ApprovalRequired"),
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
}
