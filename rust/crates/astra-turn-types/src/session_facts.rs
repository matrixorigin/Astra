//! L1a: System-tracked session facts (ground truth, zero LLM).
//!
//! Pure data model and deterministic prompt rendering for session facts.
//! Journal/tool ingestion lives in `astra-turn-core` so these shared turn types
//! stay independent of service-layer journal records.

use serde::{Deserialize, Serialize};
use std::fmt::Write;

// ── Types ────────────────────────────────────────────────────────────────────

/// Ground truth session state. Never hallucinated.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionFacts {
    /// Files touched this session, most recent last. Capped at 20.
    pub active_files: Vec<FileEntry>,
    /// Last N tool calls with outcomes. Capped at 10.
    pub recent_tool_calls: Vec<ToolFact>,
    /// Blocked/unhealthy tools (from checkpoint).
    pub blocked_tools: Vec<String>,
    /// Error accumulator.
    pub error_state: ErrorFact,
    /// Current turn number.
    pub turn: u32,
    /// Cumulative prompt tokens.
    pub estimated_tokens: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FileEntry {
    pub path: String,
    /// "read", "write", or "create"; "write" covers all non-create mutations, including deletes.
    pub last_action: String,
    pub turn: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ToolFact {
    pub name: String,
    pub ok: bool,
    pub turn: u32,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ErrorFact {
    pub total_errors: u32,
    pub last_error: Option<String>,
    pub last_error_turn: Option<u32>,
}

// ── Update ───────────────────────────────────────────────────────────────────

impl SessionFacts {
    /// Set blocked tools from checkpoint state.
    pub fn set_blocked_tools(&mut self, blocked: Vec<String>) {
        self.blocked_tools = blocked;
    }
}

// ── Injection ────────────────────────────────────────────────────────────────

impl SessionFacts {
    /// Serialize to injection format (~150 tokens).
    pub fn to_injection(&self) -> String {
        let mut out = String::from("# System State\n");
        writeln!(
            out,
            "Turn {}, ~{}K tokens",
            self.turn,
            self.estimated_tokens / 1000
        )
        .ok();

        if !self.active_files.is_empty() {
            out.push_str("Active files:\n");
            for f in self.active_files.iter().rev().take(10) {
                writeln!(out, "  {} {} (t{})", f.last_action, f.path, f.turn).ok();
            }
        }

        if self.error_state.total_errors > 0 {
            write!(out, "Errors: {} total", self.error_state.total_errors).ok();
            if let Some(err) = &self.error_state.last_error {
                write!(out, ", last: {err}").ok();
            }
            out.push('\n');
        }

        if !self.blocked_tools.is_empty() {
            writeln!(out, "Blocked tools: {}", self.blocked_tools.join(", ")).ok();
        }

        out
    }

    /// Check if a file path is in the active set (for compaction pin list).
    pub fn is_active_file(&self, path: &str, recent_turns: u32) -> bool {
        let cutoff = self.turn.saturating_sub(recent_turns);
        self.active_files
            .iter()
            .any(|f| f.path == path && f.turn >= cutoff)
    }
}
