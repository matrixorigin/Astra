use super::*;
use futures_util::StreamExt;

// ═══════════════════════════════════════════════════════════════ Spinner ══

const SPINNER_FRAMES: &[char] = &['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];

/// A spinner that runs in a background thread.
pub(super) struct Spinner {
    stop: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl Spinner {
    /// Start a spinner with the given prefix text (e.g., "  🔧 read_file").
    pub(super) fn start(prefix: String) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let stop2 = stop.clone();
        let handle = std::thread::spawn(move || {
            let mut idx = 0usize;
            while !stop2.load(Ordering::Relaxed) {
                let frame = SPINNER_FRAMES[idx % SPINNER_FRAMES.len()];
                eprint!(
                    "\r  {} {}",
                    prefix.as_str().cyan(),
                    format!("{frame}").yellow()
                );
                let _ = io::stderr().flush();
                idx += 1;
                std::thread::sleep(std::time::Duration::from_millis(80));
            }
        });
        Self {
            stop,
            handle: Some(handle),
        }
    }

    /// Stop the spinner and clear its line. Returns nothing.
    pub(super) fn stop_clear(mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
        // Clear the spinner line
        eprint!("\r{}\r", " ".repeat(80));
        let _ = io::stderr().flush();
    }
}

impl Drop for Spinner {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

// ─── Turn result from one /chat/turn SSE stream ───────────────────────────────

pub(super) struct TurnResult {
    pub(super) session_id: Option<String>,
    pub(super) run_id: Option<String>,
    pub(super) full_text: String,
    pub(super) reasoning_content: String, // thinking/reasoning captured from LLM (for thinking models)
    pub(super) tool_calls: Vec<serde_json::Value>, // raw tool_call objects from server
    pub(super) explain_turns: Vec<serde_json::Value>,
    pub(super) has_tool_calls: bool,
    pub(super) prompt_tokens: u64,
    pub(super) completion_tokens: u64,
    pub(super) has_usage: bool,
    pub(super) error_message: Option<String>,
}

impl TurnResult {
    pub(super) fn new() -> Self {
        Self {
            session_id: None,
            run_id: None,
            full_text: String::new(),
            reasoning_content: String::new(),
            tool_calls: Vec::new(),
            explain_turns: Vec::new(),
            has_tool_calls: false,
            prompt_tokens: 0,
            completion_tokens: 0,
            has_usage: false,
            error_message: None,
        }
    }
}

/// Live rendering state tracked across SSE chunks within one turn.
pub(super) struct StreamRenderState {
    thinking_start: Option<Instant>,
    thinking_spinner: Option<Spinner>,
}

impl StreamRenderState {
    pub(super) fn new() -> Self {
        Self {
            thinking_start: None,
            thinking_spinner: None,
        }
    }

    fn start_thinking(&mut self) {
        if self.thinking_spinner.is_none() {
            self.thinking_start = Some(Instant::now());
            self.thinking_spinner = Some(Spinner::start("  ● Thinking".to_string()));
        }
    }

    fn stop_thinking(&mut self) {
        if let Some(spinner) = self.thinking_spinner.take() {
            spinner.stop_clear();
            if let Some(start) = self.thinking_start.take() {
                let elapsed = start.elapsed().as_secs_f64();
                eprintln!("{}", format!("  ● Thought for {elapsed:.1}s").dim());
            }
        }
    }
}

/// Consume one /chat/turn SSE stream, render text deltas, collect tool_calls.
/// When `quiet` is true, all terminal output is suppressed but result.full_text is still captured.
pub(super) async fn consume_turn_sse(
    resp: reqwest::Response,
    render_md: bool,
    term_width: usize,
    quiet: bool,
) -> TurnResult {
    let mut result = TurnResult::new();
    let mut render = StreamRenderState::new();
    let mut stream = resp.bytes_stream();
    let mut buffer = String::new();

    while let Some(chunk) = stream.next().await {
        let Ok(chunk) = chunk else { break };
        buffer.push_str(&String::from_utf8_lossy(&chunk));
        while let Some(event_end) = buffer.find("\n\n") {
            let event_str = buffer[..event_end].to_string();
            buffer = buffer[event_end + 2..].to_string();
            dispatch_turn_event_block(&event_str, &mut result, &mut render, quiet);
        }
    }
    if !buffer.trim().is_empty() {
        dispatch_turn_event_block(&buffer, &mut result, &mut render, quiet);
    }

    // Ensure thinking spinner is cleaned up
    render.stop_thinking();

    if quiet {
        // In quiet mode: text is captured in result.full_text, no terminal output
        return result;
    }

    // Clear raw streamed text and re-render cleanly
    if !result.full_text.is_empty() {
        // Calculate how many visual lines the raw streamed text occupies
        let tw = term_width.max(1);
        let mut visual_lines = 0usize;
        let mut col = 0usize;
        for ch in result.full_text.chars() {
            if ch == '\n' {
                visual_lines += 1;
                col = 0;
            } else {
                col += 1;
                if col >= tw {
                    visual_lines += 1;
                    col = 0;
                }
            }
        }
        if visual_lines > 0 {
            execute!(
                io::stdout(),
                cursor::MoveUp(visual_lines as u16),
                cursor::MoveToColumn(0),
                terminal::Clear(terminal::ClearType::FromCursorDown)
            )
            .ok();
        } else {
            // Single-line text: just clear the current line
            execute!(
                io::stdout(),
                cursor::MoveToColumn(0),
                terminal::Clear(terminal::ClearType::CurrentLine)
            )
            .ok();
        }

        if result.has_tool_calls {
            // Before tool calls: print trimmed text with exactly one trailing newline
            let trimmed = result.full_text.trim();
            if !trimmed.is_empty() {
                if render_md {
                    print_markdown(trimmed);
                } else {
                    println!("{trimmed}");
                }
            }
        } else if render_md {
            print_markdown(&result.full_text);
        } else {
            println!("{}", result.full_text.trim_end());
        }
    }
    // If no text at all (pure tool calls on subsequent turns), print nothing

    result
}

pub(super) fn dispatch_turn_event_block(
    block: &str,
    result: &mut TurnResult,
    render: &mut StreamRenderState,
    quiet: bool,
) {
    for line in block.lines() {
        let Some(data) = line.strip_prefix("data: ") else {
            continue;
        };
        if data == "[DONE]" {
            continue;
        }
        let Ok(event) = serde_json::from_str::<serde_json::Value>(data) else {
            continue;
        };
        let etype = event.get("type").and_then(|v| v.as_str()).unwrap_or("");
        match etype {
            "text_delta" => {
                if !quiet {
                    render.stop_thinking();
                }
                if let Some(content) = event.get("content").and_then(|v| v.as_str()) {
                    if !quiet {
                        print!("{content}");
                        let _ = io::stdout().flush();
                    }
                    result.full_text.push_str(content);
                }
            }
            "text_done" => {
                if result.full_text.is_empty()
                    && let Some(ft) = event.get("full_text").and_then(|v| v.as_str())
                {
                    result.full_text = ft.to_string();
                }
            }
            "thinking_delta" | "reasoning_delta" => {
                // Capture thinking/reasoning content for inclusion in assistant messages.
                if !quiet {
                    render.start_thinking();
                }
                if let Some(chunk) = event.get("content").and_then(|v| v.as_str()) {
                    result.reasoning_content.push_str(chunk);
                }
            }
            "thinking_done" | "reasoning_done" => {
                if !quiet {
                    render.stop_thinking();
                }
            }
            "tool_call_start" => {
                if !quiet {
                    render.stop_thinking();
                }
            }
            "tool_call" => {
                result.tool_calls.push(event.clone());
            }
            "explain" => {
                result.explain_turns.push(event.clone());
            }
            "turn_complete" | "turn_done" => {
                result.has_tool_calls = event
                    .get("has_tool_calls")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
            }
            "session_info" => {
                if let Some(sid) = event.get("session_id").and_then(|v| v.as_str()) {
                    result.session_id = Some(sid.to_string());
                }
            }
            "usage" => {
                result.prompt_tokens = event
                    .get("prompt_tokens")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0);
                result.completion_tokens = event
                    .get("completion_tokens")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0);
                result.has_usage = true;
            }
            "error" => {
                let msg = event
                    .get("message")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown error");
                result.error_message = Some(format!("Error: {msg}"));
            }
            "run_started" => {
                if let Some(rid) = event.get("run_id").and_then(|v| v.as_str()) {
                    result.run_id = Some(rid.to_string());
                }
            }
            _ => {
                if let Some(rid) = event.get("run_id").and_then(|v| v.as_str()) {
                    result.run_id = Some(rid.to_string());
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sse(event_type: &str, extra: &str) -> String {
        format!("data: {{\"type\":\"{event_type}\"{extra}}}\n\n")
    }

    #[test]
    fn text_delta_accumulates() {
        let mut r = TurnResult::new();
        let mut s = StreamRenderState::new();
        let block = format!(
            "{}{}",
            sse("text_delta", ",\"content\":\"hello \""),
            sse("text_delta", ",\"content\":\"world\""),
        );
        dispatch_turn_event_block(&block, &mut r, &mut s, true);
        assert_eq!(r.full_text, "hello world");
    }

    #[test]
    fn session_info_captured() {
        let mut r = TurnResult::new();
        let mut s = StreamRenderState::new();
        dispatch_turn_event_block(
            &sse("session_info", ",\"session_id\":\"abc-123\""),
            &mut r,
            &mut s,
            true,
        );
        assert_eq!(r.session_id.as_deref(), Some("abc-123"));
    }

    #[test]
    fn usage_captured() {
        let mut r = TurnResult::new();
        let mut s = StreamRenderState::new();
        dispatch_turn_event_block(
            &sse("usage", ",\"prompt_tokens\":100,\"completion_tokens\":50"),
            &mut r,
            &mut s,
            true,
        );
        assert!(r.has_usage);
        assert_eq!(r.prompt_tokens, 100);
        assert_eq!(r.completion_tokens, 50);
    }

    #[test]
    fn tool_call_collected() {
        let mut r = TurnResult::new();
        let mut s = StreamRenderState::new();
        dispatch_turn_event_block(
            &sse("tool_call", ",\"function\":{\"name\":\"bash\"}"),
            &mut r,
            &mut s,
            true,
        );
        assert_eq!(r.tool_calls.len(), 1);
        assert_eq!(r.tool_calls[0]["function"]["name"].as_str(), Some("bash"));
    }

    #[test]
    fn turn_complete_sets_has_tool_calls() {
        let mut r = TurnResult::new();
        let mut s = StreamRenderState::new();
        dispatch_turn_event_block(
            &sse("turn_complete", ",\"has_tool_calls\":true"),
            &mut r,
            &mut s,
            true,
        );
        assert!(r.has_tool_calls);
    }

    #[test]
    fn error_captured() {
        let mut r = TurnResult::new();
        let mut s = StreamRenderState::new();
        dispatch_turn_event_block(
            &sse("error", ",\"message\":\"rate limited\""),
            &mut r,
            &mut s,
            true,
        );
        assert_eq!(r.error_message.as_deref(), Some("Error: rate limited"));
    }

    #[test]
    fn run_started_captures_run_id() {
        let mut r = TurnResult::new();
        let mut s = StreamRenderState::new();
        dispatch_turn_event_block(
            &sse("run_started", ",\"run_id\":\"run-42\""),
            &mut r,
            &mut s,
            true,
        );
        assert_eq!(r.run_id.as_deref(), Some("run-42"));
    }

    #[test]
    fn done_marker_ignored() {
        let mut r = TurnResult::new();
        let mut s = StreamRenderState::new();
        dispatch_turn_event_block("data: [DONE]\n\n", &mut r, &mut s, true);
        assert!(r.full_text.is_empty());
    }

    #[test]
    fn invalid_json_ignored() {
        let mut r = TurnResult::new();
        let mut s = StreamRenderState::new();
        dispatch_turn_event_block("data: {invalid json}\n\n", &mut r, &mut s, true);
        assert!(r.full_text.is_empty());
    }

    #[test]
    fn text_done_fallback_when_no_deltas() {
        let mut r = TurnResult::new();
        let mut s = StreamRenderState::new();
        dispatch_turn_event_block(
            &sse("text_done", ",\"full_text\":\"complete answer\""),
            &mut r,
            &mut s,
            true,
        );
        assert_eq!(r.full_text, "complete answer");
    }

    #[test]
    fn thinking_delta_captures_reasoning() {
        let mut r = TurnResult::new();
        let mut s = StreamRenderState::new();
        let block = format!(
            "{}{}",
            sse("thinking_delta", ",\"content\":\"step 1\""),
            sse("thinking_delta", ",\"content\":\" step 2\""),
        );
        dispatch_turn_event_block(&block, &mut r, &mut s, true);
        assert_eq!(r.reasoning_content, "step 1 step 2");
    }
}
