//! L1a: System-tracked session facts (ground truth, zero LLM).
//!
//! Updated every turn from journal events and checkpoint state.
//! See `docs/design/session-memory-protocol.md` Section 4.1.

use astra_services::session_journal::{JournalEvent, ToolCallRecord};
use serde::{Deserialize, Serialize};
use std::fmt::Write;

// ── Types ────────────────────────────────────────────────────────────────────

/// Ground truth session state. Never hallucinated.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileEntry {
    pub path: String,
    /// "read", "write", or "create"; "write" covers all non-create mutations, including deletes.
    pub last_action: String,
    pub turn: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolFact {
    pub name: String,
    pub ok: bool,
    pub turn: u32,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PlanFact {
    pub goal: String,
    pub completed: u32,
    pub total: u32,
    pub current_subtask: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ErrorFact {
    pub total_errors: u32,
    pub last_error: Option<String>,
    pub last_error_turn: Option<u32>,
}

const MAX_ACTIVE_FILES: usize = 20;
const MAX_RECENT_TOOLS: usize = 10;

// ── Update ───────────────────────────────────────────────────────────────────

impl SessionFacts {
    /// Incremental update from a single turn's journal event.
    pub fn update_from_journal_event(&mut self, event: &JournalEvent) {
        if let Some(t) = event.turn {
            self.turn = t;
        }
        if let Some(tokens) = event.tokens_in {
            self.estimated_tokens += tokens;
        }

        // Extract file paths and tool outcomes from tool_calls
        if let Some(tool_calls) = &event.tool_calls {
            for tc in tool_calls {
                if tc.is_synthetic_placeholder() {
                    continue;
                }
                // File tracking
                if let Some(path) = extract_file_path(tc) {
                    let action = action_for_tool(&tc.name);
                    self.upsert_file(path, action, self.turn);
                }
                // Tool outcome tracking
                self.recent_tool_calls.push(ToolFact {
                    name: tc.name.clone(),
                    ok: tc.ok,
                    turn: self.turn,
                });
                if self.recent_tool_calls.len() > MAX_RECENT_TOOLS {
                    self.recent_tool_calls.remove(0);
                }
            }
        }

        // Error tracking
        if let Some(err) = &event.error {
            self.error_state.total_errors += 1;
            self.error_state.last_error = Some(truncate(err, 200));
            self.error_state.last_error_turn = Some(self.turn);
        }
    }

    /// Set blocked tools from checkpoint state.
    pub fn set_blocked_tools(&mut self, blocked: Vec<String>) {
        self.blocked_tools = blocked;
    }

    /// Set plan state from checkpoint's `executing_plan_json`.
    pub fn set_plan_state(&mut self, plan: Option<PlanFact>) {
        self.plan_state = plan;
    }

    fn upsert_file(&mut self, path: String, action: String, turn: u32) {
        // Update existing entry or append
        if let Some(entry) = self.active_files.iter_mut().find(|f| f.path == path) {
            entry.last_action = action;
            entry.turn = turn;
        } else {
            self.active_files.push(FileEntry {
                path,
                last_action: action,
                turn,
            });
            if self.active_files.len() > MAX_ACTIVE_FILES {
                self.active_files.remove(0);
            }
        }
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

/// Extract file path from a ToolCallRecord.
/// Uses `file_path` field if available, falls back to parsing `args_full` (untruncated)
/// and finally `args_preview` (which may be truncated mid-path).
fn extract_file_path(tc: &ToolCallRecord) -> Option<String> {
    // Prefer the dedicated field (populated at tool execution time)
    if let Some(fp) = &tc.file_path {
        if !fp.is_empty() {
            return Some(fp.clone());
        }
    }
    // Next-best: parse the untruncated args_full. Reliable for str_replace and
    // any record where `args_preview` would have been cut off mid-path.
    if let Some(full) = tc.args_full.as_deref() {
        if let Some(path) = parse_path_from_json_preview(full) {
            return Some(path);
        }
    }
    // Last-resort: parse args_preview. Best-effort for legacy records.
    let preview = tc.args_preview.as_deref()?;
    parse_path_from_json_preview(preview)
}

/// Best-effort extraction of "path" field from a truncated JSON preview.
fn parse_path_from_json_preview(preview: &str) -> Option<String> {
    // Look for "path":"..." or "path": "..."
    let idx = preview.find("\"path\"")?;
    let rest = &preview[idx + 6..];
    let colon = rest.find(':')?;
    let after_colon = rest[colon + 1..].trim_start();
    if !after_colon.starts_with('"') {
        return None;
    }
    let content = &after_colon[1..];
    let end = content.find('"')?;
    let path = &content[..end];
    if path.is_empty() || path.contains("\\n") {
        return None;
    }
    Some(path.to_string())
}

fn action_for_tool(tool_name: &str) -> String {
    match tool_name {
        "create_file" => "create".to_string(),
        "write_to_file" | "write_file" => "write".to_string(),
        "edit_file" | "str_replace" | "str_replace_editor" | "multi_edit" | "delete_file" => {
            "write".to_string()
        }
        "insert_content" | "append_to_file" => "write".to_string(),
        _ => "read".to_string(),
    }
}

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

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use astra_services::session_journal::{JournalEvent, JournalEventType, ToolCallRecord};

    fn make_tc(
        name: &str,
        ok: bool,
        file_path: Option<&str>,
        args: Option<&str>,
    ) -> ToolCallRecord {
        ToolCallRecord {
            name: name.to_string(),
            ok,
            ms: 100,
            error: None,
            input_bytes: None,
            output_bytes: None,
            args_preview: args.map(|s| s.to_string()),
            result_preview: None,
            file_path: file_path.map(|s| s.to_string()),
            surgically_removed: None,
            original_tool_name: None,
            ..Default::default()
        }
    }

    fn make_event(turn: u32, tool_calls: Vec<ToolCallRecord>) -> JournalEvent {
        let mut event = JournalEvent::base_public(JournalEventType::Turn, None);
        event.turn = Some(turn);
        event.tokens_in = Some(1000);
        event.tool_calls = Some(tool_calls);
        event
    }

    #[test]
    fn update_tracks_files_from_file_path_field() {
        let mut facts = SessionFacts::default();
        let event = make_event(
            1,
            vec![
                make_tc("read_file", true, Some("src/main.rs"), None),
                make_tc("str_replace", true, Some("src/lib.rs"), None),
            ],
        );
        facts.update_from_journal_event(&event);

        assert_eq!(facts.active_files.len(), 2);
        assert_eq!(facts.active_files[0].path, "src/main.rs");
        assert_eq!(facts.active_files[0].last_action, "read");
        assert_eq!(facts.active_files[1].path, "src/lib.rs");
        assert_eq!(facts.active_files[1].last_action, "write");
    }

    #[test]
    fn update_tracks_create_and_recent_file_mutation_actions() {
        let mut facts = SessionFacts::default();
        let event = make_event(
            1,
            vec![
                make_tc("create_file", true, Some("new.rs"), None),
                make_tc("write_file", true, Some("existing.rs"), None),
                make_tc("multi_edit", true, Some("batch.rs"), None),
                make_tc("delete_file", true, Some("gone.rs"), None),
            ],
        );
        facts.update_from_journal_event(&event);

        assert_eq!(facts.active_files.len(), 4);
        assert_eq!(facts.active_files[0].last_action, "create");
        assert_eq!(facts.active_files[1].last_action, "write");
        assert_eq!(facts.active_files[2].last_action, "write");
        assert_eq!(facts.active_files[3].last_action, "write");
    }

    #[test]
    fn update_falls_back_to_args_preview() {
        let mut facts = SessionFacts::default();
        let event = make_event(
            1,
            vec![make_tc(
                "read_file",
                true,
                None,
                Some(r#"{"path":"src/foo.rs"}"#),
            )],
        );
        facts.update_from_journal_event(&event);

        assert_eq!(facts.active_files.len(), 1);
        assert_eq!(facts.active_files[0].path, "src/foo.rs");
    }

    #[test]
    fn update_prefers_args_full_over_truncated_args_preview() {
        // Repro of a real journal hazard: when `file_path` is missing
        // (legacy/older record) and `args_preview` was truncated mid-path,
        // the fallback parser used to return a wrong/partial path. Records
        // that carry the untruncated `args_full` must use that instead.
        let mut facts = SessionFacts::default();
        let truncated_preview = r#"{"path":"rust/crates/astra-cli/src/edge_tools/file_state_legac"#;
        let full = r#"{"path":"rust/crates/astra-cli/src/edge_tools/file_state_legacy_helpers.rs","old_str":"foo","new_str":"bar"}"#;
        let mut tc = make_tc("str_replace", true, None, Some(truncated_preview));
        tc.args_full = Some(full.to_string());
        let event = make_event(1, vec![tc]);
        facts.update_from_journal_event(&event);

        assert_eq!(facts.active_files.len(), 1);
        assert_eq!(
            facts.active_files[0].path,
            "rust/crates/astra-cli/src/edge_tools/file_state_legacy_helpers.rs",
            "extractor must read the untruncated args_full, not the truncated preview"
        );
    }

    #[test]
    fn upsert_updates_existing_file() {
        let mut facts = SessionFacts::default();
        let e1 = make_event(1, vec![make_tc("read_file", true, Some("a.rs"), None)]);
        let e2 = make_event(2, vec![make_tc("str_replace", true, Some("a.rs"), None)]);
        facts.update_from_journal_event(&e1);
        facts.update_from_journal_event(&e2);

        assert_eq!(facts.active_files.len(), 1);
        assert_eq!(facts.active_files[0].last_action, "write");
        assert_eq!(facts.active_files[0].turn, 2);
    }

    #[test]
    fn caps_active_files_at_max() {
        let mut facts = SessionFacts::default();
        for i in 0..25 {
            let event = make_event(
                i,
                vec![make_tc(
                    "read_file",
                    true,
                    Some(&format!("file_{i}.rs")),
                    None,
                )],
            );
            facts.update_from_journal_event(&event);
        }
        assert_eq!(facts.active_files.len(), MAX_ACTIVE_FILES);
        // Oldest should be dropped
        assert_eq!(facts.active_files[0].path, "file_5.rs");
    }

    #[test]
    fn tracks_errors() {
        let mut facts = SessionFacts::default();
        let mut event = make_event(3, vec![]);
        event.error = Some("sqlx migration failed".to_string());
        facts.update_from_journal_event(&event);

        assert_eq!(facts.error_state.total_errors, 1);
        assert_eq!(
            facts.error_state.last_error.as_deref(),
            Some("sqlx migration failed")
        );
        assert_eq!(facts.error_state.last_error_turn, Some(3));
    }

    #[test]
    fn tracks_tool_outcomes() {
        let mut facts = SessionFacts::default();
        let event = make_event(
            1,
            vec![
                make_tc("read_file", true, None, None),
                make_tc("bash", false, None, None),
            ],
        );
        facts.update_from_journal_event(&event);

        assert_eq!(facts.recent_tool_calls.len(), 2);
        assert!(facts.recent_tool_calls[0].ok);
        assert!(!facts.recent_tool_calls[1].ok);
    }

    #[test]
    fn caps_recent_tools_at_max() {
        let mut facts = SessionFacts::default();
        for i in 0..15 {
            let event = make_event(i, vec![make_tc("bash", true, None, None)]);
            facts.update_from_journal_event(&event);
        }
        assert_eq!(facts.recent_tool_calls.len(), MAX_RECENT_TOOLS);
    }

    #[test]
    fn is_active_file_respects_recency() {
        let mut facts = SessionFacts::default();
        let e1 = make_event(1, vec![make_tc("read_file", true, Some("old.rs"), None)]);
        let e2 = make_event(10, vec![make_tc("read_file", true, Some("new.rs"), None)]);
        facts.update_from_journal_event(&e1);
        facts.update_from_journal_event(&e2);

        assert!(facts.is_active_file("new.rs", 5));
        assert!(!facts.is_active_file("old.rs", 5));
        assert!(facts.is_active_file("old.rs", 20)); // wider window
    }

    #[test]
    fn pending_relevant_file_matches_current_subtask() {
        let facts = SessionFacts {
            plan_state: Some(PlanFact {
                goal: "fix compaction".to_string(),
                completed: 1,
                total: 3,
                current_subtask: Some(
                    "preserve rust/crates/runtime/src/server/run_lifecycle.rs while validating"
                        .to_string(),
                ),
            }),
            ..Default::default()
        };

        assert!(facts.is_pending_relevant_file("rust/crates/runtime/src/server/run_lifecycle.rs"));
        assert!(!facts.is_pending_relevant_file("rust/crates/runtime/src/other.rs"));
    }

    #[test]
    fn to_injection_format() {
        let facts = SessionFacts {
            turn: 5,
            estimated_tokens: 25000,
            active_files: vec![FileEntry {
                path: "src/main.rs".to_string(),
                last_action: "write".to_string(),
                turn: 5,
            }],
            error_state: ErrorFact {
                total_errors: 1,
                last_error: Some("compile error".to_string()),
                ..Default::default()
            },
            blocked_tools: vec!["web_fetch".to_string()],
            ..Default::default()
        };

        let injection = facts.to_injection();
        assert!(injection.contains("Turn 5, ~25K tokens"));
        assert!(injection.contains("write src/main.rs (t5)"));
        assert!(injection.contains("Errors: 1 total, last: compile error"));
        assert!(injection.contains("Blocked tools: web_fetch"));
    }

    #[test]
    fn working_set_injection_has_stable_order_and_preserves_key_facts() {
        let facts = SessionFacts {
            turn: 5,
            active_files: vec![
                FileEntry {
                    path: "src/z.rs".to_string(),
                    last_action: "read".to_string(),
                    turn: 4,
                },
                FileEntry {
                    path: "src/a.rs".to_string(),
                    last_action: "write".to_string(),
                    turn: 5,
                },
            ],
            recent_tool_calls: vec![
                ToolFact {
                    name: "read_file".to_string(),
                    ok: true,
                    turn: 4,
                },
                ToolFact {
                    name: "str_replace".to_string(),
                    ok: false,
                    turn: 5,
                },
            ],
            plan_state: Some(PlanFact {
                goal: "fix context continuity".to_string(),
                completed: 1,
                total: 3,
                current_subtask: Some("add canonical working set".to_string()),
            }),
            blocked_tools: vec!["str_replace".to_string(), "web_fetch".to_string()],
            error_state: ErrorFact {
                total_errors: 2,
                last_error: Some("old_str not found".to_string()),
                last_error_turn: Some(5),
            },
            estimated_tokens: 0,
        };

        let injection = facts.to_working_set_injection("fallback goal");
        assert!(injection.starts_with("[working-set:v1]\n"));
        assert!(injection.contains("goal: fix context continuity\n"));
        assert!(injection.contains("pending_work: add canonical working set\n"));
        assert!(
            injection.find("- src/a.rs").unwrap() < injection.find("- src/z.rs").unwrap(),
            "active files should be sorted for deterministic rendering: {injection}"
        );
        assert!(injection.contains("- read_file [ok t4]"));
        assert!(injection.contains("- str_replace [error t5]"));
        assert!(injection.contains("- blocked: str_replace, web_fetch"));
        assert!(injection.contains("- errors: 2 total, last: old_str not found"));
    }

    #[test]
    fn working_set_injection_uses_none_placeholders_for_empty_sections() {
        let facts = SessionFacts::default();
        let injection = facts.to_working_set_injection("");
        assert!(injection.contains("goal: none\n"));
        assert!(injection.contains("pending_work: none\n"));
        assert!(injection.contains("active_files:\n- none\n"));
        assert!(injection.contains("recent_tools:\n- none\n"));
        assert!(injection.contains("tool_risks:\n- none\n"));
    }

    #[test]
    fn parse_path_from_json_preview_works() {
        assert_eq!(
            parse_path_from_json_preview(r#"{"path":"src/lib.rs","old_str":"fn main"}"#),
            Some("src/lib.rs".to_string())
        );
        assert_eq!(
            parse_path_from_json_preview(r#"{"path": "a/b.rs"}"#),
            Some("a/b.rs".to_string())
        );
        // Truncated — no closing quote
        assert_eq!(
            parse_path_from_json_preview(r#"{"path":"src/very/long/path/that/gets/trun"#),
            None
        );
        // No path field
        assert_eq!(parse_path_from_json_preview(r#"{"query":"test"}"#), None);
    }

    #[test]
    fn accumulates_tokens() {
        let mut facts = SessionFacts::default();
        let e1 = make_event(1, vec![]);
        let e2 = make_event(2, vec![]);
        facts.update_from_journal_event(&e1);
        facts.update_from_journal_event(&e2);
        assert_eq!(facts.estimated_tokens, 2000);
    }
}
