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
    /// Plan progress (from checkpoint, not journal).
    pub plan_state: Option<PlanFact>,
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
pub struct PlanFact {
    pub goal: String,
    pub completed: u32,
    pub total: u32,
    pub current_subtask: Option<String>,
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

    /// Set plan state from checkpoint's `executing_plan_json`.
    pub fn set_plan_state(&mut self, plan: Option<PlanFact>) {
        self.plan_state = plan;
    }
}

// ── Injection ────────────────────────────────────────────────────────────────

impl SessionFacts {
    /// Deterministic working-set injection for cross-turn continuity.
    ///
    /// Field order is stable by design so prefix-cache providers can reuse the
    /// surrounding prompt while still preserving the facts the model needs to
    /// stay oriented after compaction.
    pub fn to_working_set_injection(&self, current_goal: &str) -> String {
        let mut out = String::from("[working-set:v1]\n");
        let goal = if let Some(plan) = &self.plan_state {
            plan.goal.trim()
        } else {
            current_goal.trim()
        };
        writeln!(out, "goal: {}", truncate_or_none(goal, 240)).ok();

        let pending = self
            .plan_state
            .as_ref()
            .and_then(|plan| plan.current_subtask.as_deref())
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(|value| truncate(value, 200))
            .unwrap_or_else(|| "none".to_string());
        writeln!(out, "pending_work: {pending}").ok();

        out.push_str("active_files:\n");
        if self.active_files.is_empty() {
            out.push_str("- none\n");
        } else {
            let mut files: Vec<&FileEntry> = self.active_files.iter().collect();
            files.sort_by(|a, b| a.path.cmp(&b.path));
            for file in files.into_iter().take(12) {
                writeln!(
                    out,
                    "- {} [{} t{}]",
                    truncate(&file.path, 160),
                    file.last_action,
                    file.turn
                )
                .ok();
            }
        }

        out.push_str("recent_tools:\n");
        if self.recent_tool_calls.is_empty() {
            out.push_str("- none\n");
        } else {
            for tool in self.recent_tool_calls.iter().rev().take(6).rev() {
                writeln!(
                    out,
                    "- {} [{} t{}]",
                    tool.name,
                    if tool.ok { "ok" } else { "error" },
                    tool.turn
                )
                .ok();
            }
        }

        out.push_str("tool_risks:\n");
        if self.blocked_tools.is_empty() && self.error_state.total_errors == 0 {
            out.push_str("- none\n");
        } else {
            if !self.blocked_tools.is_empty() {
                let mut blocked = self.blocked_tools.clone();
                blocked.sort();
                writeln!(out, "- blocked: {}", blocked.join(", ")).ok();
            }
            if self.error_state.total_errors > 0 {
                let last = self
                    .error_state
                    .last_error
                    .as_deref()
                    .map(|err| truncate(err, 180))
                    .unwrap_or_else(|| "unknown".to_string());
                writeln!(
                    out,
                    "- errors: {} total, last: {}",
                    self.error_state.total_errors, last
                )
                .ok();
            }
        }

        out
    }

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

        if let Some(plan) = &self.plan_state {
            write!(
                out,
                "Plan: {} ({}/{})",
                plan.goal, plan.completed, plan.total
            )
            .ok();
            if let Some(sub) = &plan.current_subtask {
                write!(out, ", current: {sub}").ok();
            }
            out.push('\n');
        }

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

    /// Check whether a file path is explicitly referenced by pending plan work.
    pub fn is_pending_relevant_file(&self, path: &str) -> bool {
        let Some(plan) = &self.plan_state else {
            return false;
        };
        let Some(subtask) = plan.current_subtask.as_deref() else {
            return false;
        };
        if path.is_empty() {
            return false;
        }
        subtask.contains(path)
    }
}

// ── Helpers ──────────────────────────────────────────────────────────────────

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        let boundary = s.floor_char_boundary(max);
        format!("{}…", &s[..boundary])
    }
}

fn truncate_or_none(s: &str, max: usize) -> String {
    if s.is_empty() {
        "none".to_string()
    } else {
        truncate(s, max)
    }
}
