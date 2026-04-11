//! Signal detection and collection from runtime events.
//!
//! Pure computation — no LLM calls, no IO. Extracts [`EvolutionSignal`]s from
//! tool results, user messages, and turn summaries, with session-scoped dedup.

use std::collections::HashSet;

use super::types::{EvolutionSignal, ToolResultContext, TurnSummary};

/// Maximum buffered signals before oldest are evicted.
const MAX_BUFFERED_SIGNALS: usize = 200;

/// Maximum error snippet length stored in signals.
const MAX_SNIPPET_LEN: usize = 300;

/// Correction keywords (lowercase). Checked via `contains` on the user message.
pub static CORRECTION_KEYWORDS: &[&str] = &[
    // Chinese
    "不对",
    "错了",
    "应该是",
    "重新来",
    "纠正",
    "搞错了",
    "不是这样",
    "改一下",
    // English
    "that's wrong",
    "should be",
    "actually,",
    "no, wait",
    "try again",
    "that is wrong",
    "not correct",
    "you're wrong",
    "incorrect",
];

/// Collects evolution signals from runtime events with session-scoped dedup.
pub struct SignalCollector {
    signals: Vec<EvolutionSignal>,
    seen_keys: HashSet<u64>,
}

impl SignalCollector {
    pub fn new() -> Self {
        Self {
            signals: Vec::new(),
            seen_keys: HashSet::new(),
        }
    }

    /// Signals collected so far.
    pub fn signals(&self) -> &[EvolutionSignal] {
        &self.signals
    }

    /// Drain all buffered signals, resetting the buffer but keeping dedup keys.
    pub fn drain(&mut self) -> Vec<EvolutionSignal> {
        std::mem::take(&mut self.signals)
    }

    /// Clear dedup keys (e.g. at conversation boundary).
    pub fn clear_dedup(&mut self) {
        self.seen_keys.clear();
    }

    /// Process a tool result and extract signals.
    pub fn on_tool_result(&mut self, ctx: &ToolResultContext<'_>) {
        if !ctx.is_error {
            return;
        }
        let snippet = truncate(ctx.result, MAX_SNIPPET_LEN);
        let signal = EvolutionSignal::ToolFailure {
            tool_name: ctx.tool_name.to_string(),
            error_snippet: snippet,
            skill_context: ctx.active_skill.map(String::from),
            turn_id: ctx.turn_id.to_string(),
        };
        self.push(signal);
    }

    /// Process a user message and detect correction intent.
    pub fn on_user_message(
        &mut self,
        msg: &str,
        prior_assistant: Option<&str>,
        active_skill: Option<&str>,
        turn_id: &str,
    ) {
        if msg.is_empty() {
            return;
        }
        let lower = msg.to_lowercase();
        let is_correction = CORRECTION_KEYWORDS.iter().any(|kw| lower.contains(kw));
        if !is_correction {
            return;
        }
        let signal = EvolutionSignal::UserCorrection {
            correction_text: truncate(msg, MAX_SNIPPET_LEN),
            prior_assistant_text: truncate(prior_assistant.unwrap_or(""), MAX_SNIPPET_LEN),
            skill_context: active_skill.map(String::from),
            turn_id: turn_id.to_string(),
        };
        self.push(signal);
    }

    /// Process turn-end summary and detect stalls.
    pub fn on_turn_end(&mut self, summary: &TurnSummary<'_>) {
        // Placeholder: stall detection is delegated to TurnState::detect_stall.
        // Future turn-level signals (e.g. latency spikes, context overflow) go here.
        let _ = summary;
    }

    /// Add a pre-built signal (e.g. PatternDrift from PatternLibrary).
    pub fn add_signal(&mut self, signal: EvolutionSignal) {
        self.push(signal);
    }

    fn push(&mut self, signal: EvolutionSignal) {
        let key = signal.dedup_key();
        if !self.seen_keys.insert(key) {
            return; // duplicate
        }
        if self.signals.len() >= MAX_BUFFERED_SIGNALS {
            let evicted = self.signals.remove(0);
            // Prune the dedup key so the same signal pattern can be re-detected.
            self.seen_keys.remove(&evicted.dedup_key());
        }
        self.signals.push(signal);
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        // Find the last char boundary that fits within `max` bytes.
        let end = s.floor_char_boundary(max);
        s[..end].to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::evolution::types::ToolResultContext;

    fn make_error_ctx<'a>(
        tool: &'a str,
        result: &'a str,
        turn_id: &'a str,
    ) -> ToolResultContext<'a> {
        ToolResultContext {
            tool_name: tool,
            tool_args: "{}",
            result,
            is_error: true,
            duration_ms: 100,
            active_skill: None,
            turn_id,
        }
    }

    #[test]
    fn collects_tool_failure_on_error() {
        let mut c = SignalCollector::new();
        c.on_tool_result(&make_error_ctx("bash", "Error: command not found", "t1"));
        assert_eq!(c.signals().len(), 1);
        match &c.signals()[0] {
            EvolutionSignal::ToolFailure { tool_name, .. } => {
                assert_eq!(tool_name, "bash");
            }
            _ => panic!("expected ToolFailure"),
        }
    }

    #[test]
    fn skips_successful_tool_result() {
        let mut c = SignalCollector::new();
        let ctx = ToolResultContext {
            tool_name: "bash",
            tool_args: "{}",
            result: "ok",
            is_error: false,
            duration_ms: 50,
            active_skill: None,
            turn_id: "t1",
        };
        c.on_tool_result(&ctx);
        assert!(c.signals().is_empty());
    }

    #[test]
    fn dedup_same_tool_failure() {
        let mut c = SignalCollector::new();
        c.on_tool_result(&make_error_ctx("bash", "Error: not found", "t1"));
        c.on_tool_result(&make_error_ctx("bash", "Error: not found", "t2"));
        assert_eq!(c.signals().len(), 1, "duplicate should be deduped");
    }

    #[test]
    fn different_errors_not_deduped() {
        let mut c = SignalCollector::new();
        c.on_tool_result(&make_error_ctx("bash", "Error: not found", "t1"));
        c.on_tool_result(&make_error_ctx("bash", "Error: permission denied", "t2"));
        assert_eq!(c.signals().len(), 2);
    }

    #[test]
    fn detects_chinese_correction() {
        let mut c = SignalCollector::new();
        c.on_user_message(
            "不对，应该用另一个方法",
            Some("I used method A"),
            None,
            "t1",
        );
        assert_eq!(c.signals().len(), 1);
        match &c.signals()[0] {
            EvolutionSignal::UserCorrection {
                correction_text, ..
            } => {
                assert!(correction_text.contains("不对"));
            }
            _ => panic!("expected UserCorrection"),
        }
    }

    #[test]
    fn detects_english_correction() {
        let mut c = SignalCollector::new();
        c.on_user_message(
            "that's wrong, should be the other way",
            Some("I did X"),
            None,
            "t1",
        );
        assert_eq!(c.signals().len(), 1);
    }

    #[test]
    fn no_correction_on_normal_message() {
        let mut c = SignalCollector::new();
        c.on_user_message("please read the file", None, None, "t1");
        assert!(c.signals().is_empty());
    }

    #[test]
    fn empty_message_ignored() {
        let mut c = SignalCollector::new();
        c.on_user_message("", None, None, "t1");
        assert!(c.signals().is_empty());
    }

    #[test]
    fn drain_returns_and_clears_signals() {
        let mut c = SignalCollector::new();
        c.on_tool_result(&make_error_ctx("bash", "Error: fail", "t1"));
        let drained = c.drain();
        assert_eq!(drained.len(), 1);
        assert!(c.signals().is_empty());
    }

    #[test]
    fn drain_preserves_dedup_keys() {
        let mut c = SignalCollector::new();
        c.on_tool_result(&make_error_ctx("bash", "Error: fail", "t1"));
        c.drain();
        // Same signal again — should still be deduped
        c.on_tool_result(&make_error_ctx("bash", "Error: fail", "t2"));
        assert!(c.signals().is_empty());
    }

    #[test]
    fn clear_dedup_allows_re_detection() {
        let mut c = SignalCollector::new();
        c.on_tool_result(&make_error_ctx("bash", "Error: fail", "t1"));
        c.drain();
        c.clear_dedup();
        c.on_tool_result(&make_error_ctx("bash", "Error: fail", "t2"));
        assert_eq!(c.signals().len(), 1);
    }

    #[test]
    fn evicts_oldest_when_buffer_full() {
        let mut c = SignalCollector::new();
        for i in 0..MAX_BUFFERED_SIGNALS + 5 {
            // Each has unique error to avoid dedup
            c.on_tool_result(&make_error_ctx(
                "bash",
                &format!("Error: unique_{i}"),
                &format!("t{i}"),
            ));
        }
        assert_eq!(c.signals().len(), MAX_BUFFERED_SIGNALS);
        // First signal should have been evicted; last should be present
        match &c.signals()[c.signals().len() - 1] {
            EvolutionSignal::ToolFailure { error_snippet, .. } => {
                let expected_idx = MAX_BUFFERED_SIGNALS + 4;
                assert!(
                    error_snippet.contains(&format!("unique_{expected_idx}")),
                    "last signal should be the most recent"
                );
            }
            _ => panic!("expected ToolFailure"),
        }
    }

    #[test]
    fn truncates_long_error_snippet() {
        let mut c = SignalCollector::new();
        let long_error = "E".repeat(1000);
        c.on_tool_result(&make_error_ctx("bash", &long_error, "t1"));
        match &c.signals()[0] {
            EvolutionSignal::ToolFailure { error_snippet, .. } => {
                assert!(error_snippet.len() <= MAX_SNIPPET_LEN);
            }
            _ => panic!("expected ToolFailure"),
        }
    }

    #[test]
    fn truncate_respects_char_boundaries() {
        // Multi-byte: "你好世界" is 12 bytes, 4 chars
        let s = "你好世界abcdef";
        let result = super::truncate(s, 7); // 7 bytes — should not split a CJK char
        assert!(result.len() <= 7);
        assert!(result.is_char_boundary(result.len()));
    }

    #[test]
    fn add_signal_deduplicates() {
        let mut c = SignalCollector::new();
        let s = EvolutionSignal::PatternDrift {
            pattern_signature: "bash|read_file".into(),
            task_type: crate::pipeline::routing::TaskType::Code,
            domain: None,
            historical_rate: 0.8,
            recent_rate: 0.3,
        };
        c.add_signal(s.clone());
        c.add_signal(s);
        assert_eq!(c.signals().len(), 1);
    }

    #[test]
    fn skill_context_preserved_in_tool_failure() {
        let mut c = SignalCollector::new();
        let ctx = ToolResultContext {
            tool_name: "bash",
            tool_args: "{}",
            result: "Error: fail",
            is_error: true,
            duration_ms: 100,
            active_skill: Some("review_changes"),
            turn_id: "t1",
        };
        c.on_tool_result(&ctx);
        match &c.signals()[0] {
            EvolutionSignal::ToolFailure { skill_context, .. } => {
                assert_eq!(skill_context.as_deref(), Some("review_changes"));
            }
            _ => panic!("expected ToolFailure"),
        }
    }

    #[test]
    fn correction_with_skill_context() {
        let mut c = SignalCollector::new();
        c.on_user_message(
            "不对，应该这样做",
            Some("I did X"),
            Some("review_code"),
            "t1",
        );
        match &c.signals()[0] {
            EvolutionSignal::UserCorrection { skill_context, .. } => {
                assert_eq!(skill_context.as_deref(), Some("review_code"));
            }
            _ => panic!("expected UserCorrection"),
        }
    }
}
