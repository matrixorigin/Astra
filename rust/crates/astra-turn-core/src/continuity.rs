//! Runtime-owned continuity state for multi-turn agent work.
//!
//! This module is intentionally pure and deterministic: it can be updated from
//! runtime facts/tool evidence without asking the model to remember progress.

use std::fmt::Write;

use serde::{Deserialize, Serialize};

use crate::cloud_session_facts::{FileEntry, PlanFact, SessionFacts};

const ATTENTION_PREFIX: &str = "[attention:v1]";
const DEFAULT_ATTENTION_CHAR_CAP: usize = 4_000;
const MAX_MANIFEST_FILES: usize = 12;
const MAX_MANIFEST_CORRECTIONS: usize = 5;
const MAX_SECRET_VALUE_CHARS: usize = 160;

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ContinuityState {
    pub goal: GoalState,
    pub todos: TodoState,
    pub facts: SessionFacts,
    pub user_corrections: Vec<UserCorrection>,
    pub verification: VerificationState,
}

impl ContinuityState {
    pub fn new(goal: impl Into<String>) -> Self {
        Self {
            goal: GoalState {
                text: goal.into(),
                ..Default::default()
            },
            ..Default::default()
        }
    }

    pub fn with_facts(mut self, facts: SessionFacts) -> Self {
        self.facts = facts;
        self
    }

    pub fn sync_facts(&mut self, facts: SessionFacts) {
        let plan_state = self.todos.to_plan_fact(&self.goal.text);
        self.facts = facts;
        if plan_state.is_some() {
            self.facts.set_plan_state(plan_state);
        }
    }

    pub fn ensure_goal(&mut self, goal: impl Into<String>) {
        if self.goal.text.trim().is_empty() {
            self.goal.text = goal.into();
        }
    }

    pub fn ensure_tracked_goal(&mut self, goal: impl Into<String>) -> Option<&TodoItem> {
        let goal = goal.into();
        self.ensure_goal(goal.clone());
        if !should_track_request(&goal) || self.todos.has_items() {
            return self.todos.active_or_next();
        }

        let title = first_sentence(&goal);
        self.todos.add_item(TodoItem {
            id: "runtime-goal".to_string(),
            title: truncate_clean(&title, 120),
            description: truncate_clean(&goal, 320),
            status: TodoStatus::Pending,
            evidence: Vec::new(),
            blocked_reason: None,
        });
        self.todos.active_or_next()
    }

    pub fn add_user_correction(&mut self, correction: impl Into<String>, turn: u32) {
        let text = correction.into();
        if text.trim().is_empty() {
            return;
        }
        self.user_corrections.push(UserCorrection {
            text: redact_sensitive(&text),
            turn,
        });
        if self.user_corrections.len() > MAX_MANIFEST_CORRECTIONS {
            self.user_corrections.remove(0);
        }
    }

    pub fn attention_manifest(&self) -> AttentionManifest {
        AttentionManifest::from_state(self, DEFAULT_ATTENTION_CHAR_CAP)
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct GoalState {
    pub text: String,
    pub source_turn: Option<u32>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct TodoState {
    pub items: Vec<TodoItem>,
}

impl TodoState {
    pub fn add_item(&mut self, item: TodoItem) {
        if self.items.iter().any(|existing| existing.id == item.id) {
            return;
        }
        self.items.push(item);
    }

    pub fn has_items(&self) -> bool {
        !self.items.is_empty()
    }

    pub fn active_or_next(&self) -> Option<&TodoItem> {
        self.items
            .iter()
            .find(|item| item.status == TodoStatus::InProgress)
            .or_else(|| {
                self.items
                    .iter()
                    .find(|item| item.status == TodoStatus::Pending)
            })
    }

    pub fn begin_next_ready(&mut self) -> Option<&TodoItem> {
        let index = self
            .items
            .iter()
            .position(|item| item.status == TodoStatus::InProgress)
            .or_else(|| {
                self.items
                    .iter()
                    .position(|item| item.status == TodoStatus::Pending)
            })?;
        self.items[index].status = TodoStatus::InProgress;
        Some(&self.items[index])
    }

    pub fn add_evidence(&mut self, id: &str, evidence: impl Into<String>) -> bool {
        let Some(item) = self.items.iter_mut().find(|item| item.id == id) else {
            return false;
        };
        let evidence = evidence.into();
        if !evidence.trim().is_empty() {
            item.evidence.push(redact_sensitive(&evidence));
        }
        true
    }

    pub fn mark_done(&mut self, id: &str, evidence: impl Into<String>) -> bool {
        let Some(item) = self.items.iter_mut().find(|item| item.id == id) else {
            return false;
        };
        let evidence = evidence.into();
        if !evidence.trim().is_empty() {
            item.evidence.push(redact_sensitive(&evidence));
        }
        item.status = TodoStatus::Done;
        item.blocked_reason = None;
        true
    }

    pub fn mark_blocked(&mut self, id: &str, reason: impl Into<String>) -> bool {
        let Some(item) = self.items.iter_mut().find(|item| item.id == id) else {
            return false;
        };
        item.status = TodoStatus::Blocked;
        item.blocked_reason = Some(redact_sensitive(&reason.into()));
        true
    }

    pub fn to_plan_fact(&self, goal: &str) -> Option<PlanFact> {
        if self.items.is_empty() {
            return None;
        }
        let completed = self
            .items
            .iter()
            .filter(|item| item.status == TodoStatus::Done)
            .count() as u32;
        let current_subtask = self
            .active_or_next()
            .map(|item| format!("{}: {}", item.id, item.title));
        Some(PlanFact {
            goal: goal.to_string(),
            completed,
            total: self.items.len() as u32,
            current_subtask,
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TodoItem {
    pub id: String,
    pub title: String,
    pub description: String,
    pub status: TodoStatus,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub evidence: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blocked_reason: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TodoStatus {
    Pending,
    InProgress,
    Done,
    Blocked,
}

impl TodoStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::InProgress => "in_progress",
            Self::Done => "done",
            Self::Blocked => "blocked",
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct UserCorrection {
    pub text: String,
    pub turn: u32,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct VerificationState {
    pub last_status: Option<VerificationStatus>,
    pub last_evidence: Option<String>,
    pub last_turn: Option<u32>,
}

impl VerificationState {
    pub fn set(&mut self, status: VerificationStatus, evidence: impl Into<String>, turn: u32) {
        self.last_status = Some(status);
        self.last_evidence = Some(redact_sensitive(&evidence.into()));
        self.last_turn = Some(turn);
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum VerificationStatus {
    Unknown,
    Passed,
    Failed,
    Blocked,
}

impl VerificationStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Unknown => "unknown",
            Self::Passed => "passed",
            Self::Failed => "failed",
            Self::Blocked => "blocked",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttentionManifest {
    content: String,
}

impl AttentionManifest {
    pub fn from_state(state: &ContinuityState, max_chars: usize) -> Self {
        let mut out = String::new();
        writeln!(out, "{ATTENTION_PREFIX}").ok();
        writeln!(
            out,
            "goal: {}",
            none_if_empty(&redact_sensitive(&truncate_clean(&state.goal.text, 300)))
        )
        .ok();

        let current = state.todos.active_or_next();
        writeln!(
            out,
            "current_todo: {}",
            current
                .map(format_todo_line)
                .unwrap_or_else(|| "none".to_string())
        )
        .ok();

        out.push_str("ready_next:\n");
        let ready: Vec<&TodoItem> = state
            .todos
            .items
            .iter()
            .filter(|item| item.status == TodoStatus::Pending)
            .take(5)
            .collect();
        if ready.is_empty() {
            out.push_str("- none\n");
        } else {
            for item in ready {
                writeln!(out, "- {}", format_todo_line(item)).ok();
            }
        }

        out.push_str("active_files:\n");
        let mut files: Vec<&FileEntry> = state.facts.active_files.iter().collect();
        files.sort_by(|a, b| a.path.cmp(&b.path));
        if files.is_empty() {
            out.push_str("- none\n");
        } else {
            for file in files.into_iter().take(MAX_MANIFEST_FILES) {
                writeln!(
                    out,
                    "- {} [{} t{}]",
                    redact_sensitive(&truncate_clean(&file.path, 180)),
                    file.last_action,
                    file.turn
                )
                .ok();
            }
        }

        out.push_str("last_error:\n");
        if let Some(err) = state.facts.error_state.last_error.as_deref() {
            writeln!(
                out,
                "- t{}: {}",
                state.facts.error_state.last_error_turn.unwrap_or_default(),
                redact_sensitive(&truncate_clean(err, 240))
            )
            .ok();
        } else {
            out.push_str("- none\n");
        }

        out.push_str("blocked_tools:\n");
        if state.facts.blocked_tools.is_empty() {
            out.push_str("- none\n");
        } else {
            let mut blocked = state.facts.blocked_tools.clone();
            blocked.sort();
            for tool in blocked {
                writeln!(out, "- {}", truncate_clean(&tool, 80)).ok();
            }
        }

        out.push_str("user_corrections:\n");
        if state.user_corrections.is_empty() {
            out.push_str("- none\n");
        } else {
            for correction in state
                .user_corrections
                .iter()
                .rev()
                .take(MAX_MANIFEST_CORRECTIONS)
                .rev()
            {
                writeln!(
                    out,
                    "- t{}: {}",
                    correction.turn,
                    truncate_clean(&correction.text, 240)
                )
                .ok();
            }
        }

        out.push_str("verification:\n");
        match state.verification.last_status {
            Some(status) => {
                let evidence = state
                    .verification
                    .last_evidence
                    .as_deref()
                    .unwrap_or("none");
                writeln!(
                    out,
                    "- {} t{}: {}",
                    status.as_str(),
                    state.verification.last_turn.unwrap_or_default(),
                    truncate_clean(evidence, 240)
                )
                .ok();
            }
            None => out.push_str("- none\n"),
        }

        Self {
            content: truncate_clean(&out, max_chars.max(ATTENTION_PREFIX.len())),
        }
    }

    pub fn as_str(&self) -> &str {
        &self.content
    }

    pub fn into_string(self) -> String {
        self.content
    }
}

pub fn should_track_request(message: &str) -> bool {
    let trimmed = message.trim();
    if trimmed.is_empty() {
        return false;
    }
    let lower = trimmed.to_ascii_lowercase();
    let action_words = [
        "implement",
        "fix",
        "add",
        "update",
        "refactor",
        "test",
        "debug",
        "plan",
        "validate",
        "run",
        "修改",
        "实现",
        "修复",
        "验证",
        "测试",
        "制定",
        "方案",
    ];
    let has_action = action_words
        .iter()
        .any(|word| lower.contains(word) || trimmed.contains(word));
    let is_multistep = trimmed.contains('\n')
        || trimmed.contains(" and ")
        || trimmed.contains("然后")
        || trimmed.contains("并且")
        || trimmed.split_whitespace().count() >= 8
        || trimmed.chars().count() >= 24;
    has_action && is_multistep
}

pub fn narrative_task_contradicts_facts(facts: &SessionFacts) -> bool {
    facts.error_state.total_errors > 0
        && facts.error_state.last_error.is_some()
        && facts
            .plan_state
            .as_ref()
            .is_some_and(|plan| plan.total > 0 && plan.completed >= plan.total)
}

pub fn strip_attention_manifest_messages(messages: &mut Vec<serde_json::Value>) {
    messages.retain(|message| {
        message
            .get("content")
            .and_then(|content| content.as_str())
            .is_none_or(|content| !content.trim_start().starts_with(ATTENTION_PREFIX))
    });
}

pub fn append_attention_manifest_message(
    messages: &mut Vec<serde_json::Value>,
    state: &ContinuityState,
    max_chars: usize,
) -> bool {
    strip_attention_manifest_messages(messages);
    let manifest = AttentionManifest::from_state(state, max_chars).into_string();
    if manifest_is_empty(&manifest) {
        return false;
    }
    messages.push(serde_json::json!({
        "role": "user",
        "content": manifest,
        "metadata": {
            "volatile": true,
            "kind": "attention_manifest"
        }
    }));
    true
}

fn manifest_is_empty(manifest: &str) -> bool {
    manifest.contains("goal: none\n")
        && manifest.contains("current_todo: none\n")
        && manifest.contains("active_files:\n- none\n")
        && manifest.contains("last_error:\n- none\n")
        && manifest.contains("user_corrections:\n- none\n")
        && manifest.contains("verification:\n- none\n")
}

fn format_todo_line(item: &TodoItem) -> String {
    let mut line = format!(
        "{} [{}]: {}",
        item.id,
        item.status.as_str(),
        truncate_clean(&redact_sensitive(&item.title), 160)
    );
    if let Some(reason) = item.blocked_reason.as_deref() {
        line.push_str(" blocked: ");
        line.push_str(&truncate_clean(reason, 160));
    }
    line
}

fn first_sentence(text: &str) -> String {
    text.trim()
        .match_indices(['.', '。', '\n'])
        .next()
        .map(|(i, s)| text[..i + s.len()].trim().to_string())
        .unwrap_or_else(|| text.trim().to_string())
}

fn none_if_empty(text: &str) -> &str {
    if text.trim().is_empty() { "none" } else { text }
}

fn truncate_clean(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    let truncated: String = text.chars().take(max_chars.saturating_sub(1)).collect();
    format!("{truncated}…")
}

pub fn redact_sensitive(text: &str) -> String {
    text.split_whitespace()
        .map(redact_token)
        .collect::<Vec<_>>()
        .join(" ")
}

fn redact_token(token: &str) -> String {
    let lower = token.to_ascii_lowercase();
    if lower.starts_with("bearer ") {
        return "Bearer [REDACTED]".to_string();
    }
    if is_secret_assignment(&lower) || looks_like_secret_value(token) {
        let prefix = token
            .find(['=', ':'])
            .map(|idx| &token[..=idx])
            .unwrap_or("");
        return format!("{prefix}[REDACTED]");
    }
    token.chars().take(MAX_SECRET_VALUE_CHARS).collect()
}

fn is_secret_assignment(lower: &str) -> bool {
    let Some(separator) = lower.find(['=', ':']) else {
        return false;
    };
    let key = &lower[..separator];
    [
        "token", "secret", "password", "passwd", "api_key", "apikey", "auth",
    ]
    .iter()
    .any(|needle| key.contains(needle))
}

fn looks_like_secret_value(token: &str) -> bool {
    let lower = token.to_ascii_lowercase();
    lower.starts_with("ghp_")
        || lower.starts_with("github_pat_")
        || lower.starts_with("sk-")
        || lower.starts_with("xoxb-")
        || (token.len() >= 32
            && token
                .chars()
                .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-' | '.')))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cloud_session_facts::{ErrorFact, FileEntry};

    fn state_with_todo() -> ContinuityState {
        let mut state = ContinuityState::new("Implement durable continuity without losing context");
        state.todos.add_item(TodoItem {
            id: "runtime-todo".to_string(),
            title: "Wire todo policy into turn loop".to_string(),
            description: "Runtime should track todo without model task tool calls".to_string(),
            status: TodoStatus::InProgress,
            evidence: vec![],
            blocked_reason: None,
        });
        state
    }

    #[test]
    fn continuity_state_builds_attention_manifest_from_facts_and_todos() {
        let mut state = state_with_todo();
        state.facts.active_files = vec![FileEntry {
            path: "rust/crates/runtime/src/turn/agentic_loop_host.rs".to_string(),
            last_action: "write".to_string(),
            turn: 3,
        }];
        state.facts.error_state = ErrorFact {
            total_errors: 1,
            last_error: Some("cargo test failed".to_string()),
            last_error_turn: Some(4),
        };
        state.add_user_correction("用户要求不能牺牲 cache", 2);
        state
            .verification
            .set(VerificationStatus::Failed, "cargo test failed", 4);

        let manifest = state.attention_manifest().into_string();
        assert!(manifest.starts_with("[attention:v1]\n"));
        assert!(manifest.contains("goal: Implement durable continuity"));
        assert!(manifest.contains("current_todo: runtime-todo [in_progress]: Wire todo policy"));
        assert!(manifest.contains("- rust/crates/runtime/src/turn/agentic_loop_host.rs"));
        assert!(manifest.contains("- t4: cargo test failed"));
        assert!(manifest.contains("- failed t4: cargo test failed"));
        assert!(manifest.contains("用户要求不能牺牲 cache"));
    }

    #[test]
    fn attention_manifest_has_stable_field_order() {
        let state = state_with_todo();
        let manifest = state.attention_manifest().into_string();
        let fields = [
            "goal:",
            "current_todo:",
            "ready_next:",
            "active_files:",
            "last_error:",
            "blocked_tools:",
            "user_corrections:",
            "verification:",
        ];
        let mut previous = 0;
        for field in fields {
            let index = manifest
                .find(field)
                .unwrap_or_else(|| panic!("missing {field}"));
            assert!(
                index >= previous,
                "{field} rendered out of order: {manifest}"
            );
            previous = index;
        }
    }

    #[test]
    fn attention_manifest_respects_char_cap() {
        let mut state = ContinuityState::new("x".repeat(10_000));
        state.facts.active_files = (0..50)
            .map(|i| FileEntry {
                path: format!("very/long/path/{i}/{}", "x".repeat(300)),
                last_action: "read".to_string(),
                turn: i,
            })
            .collect();
        let manifest = AttentionManifest::from_state(&state, 700).into_string();
        assert!(manifest.chars().count() <= 700);
        assert!(manifest.starts_with("[attention:v1]\n"));
    }

    #[test]
    fn attention_manifest_redacts_secret_like_values() {
        let mut state = ContinuityState::new("Use token=ghp_super_secret_value");
        state.facts.error_state = ErrorFact {
            total_errors: 1,
            last_error: Some("failed with password:hunter2 and sk-1234567890abcdef".to_string()),
            last_error_turn: Some(2),
        };
        state.add_user_correction("api_key=abc123", 1);

        let manifest = state.attention_manifest().into_string();
        assert!(manifest.contains("token=[REDACTED]"));
        assert!(manifest.contains("password:[REDACTED]"));
        assert!(manifest.contains("api_key=[REDACTED]"));
        assert!(!manifest.contains("hunter2"));
        assert!(!manifest.contains("abc123"));
        assert!(!manifest.contains("sk-1234567890abcdef"));
    }

    #[test]
    fn narrative_cannot_override_failed_fact_state() {
        let facts = SessionFacts {
            plan_state: Some(PlanFact {
                goal: "Build API".to_string(),
                completed: 3,
                total: 3,
                current_subtask: None,
            }),
            error_state: ErrorFact {
                total_errors: 1,
                last_error: Some("test failure".to_string()),
                last_error_turn: Some(5),
            },
            ..Default::default()
        };

        assert!(narrative_task_contradicts_facts(&facts));
    }

    #[test]
    fn todo_policy_creates_task_for_multistep_request_without_model_task_tool_call() {
        let mut state = ContinuityState::default();
        let active = state
            .ensure_tracked_goal(
                "实现 runtime continuity，并且添加测试验证多轮之后不会忘记 active todo",
            )
            .expect("tracked request should create a runtime todo");

        assert_eq!(active.id, "runtime-goal");
        assert_eq!(active.status, TodoStatus::Pending);
        assert_eq!(state.todos.items.len(), 1);
    }

    #[test]
    fn todo_policy_does_not_track_trivial_single_answer_request() {
        let mut state = ContinuityState::default();
        assert!(state.ensure_tracked_goal("什么是 Rust?").is_none());
        assert!(state.todos.items.is_empty());
    }

    #[test]
    fn todo_policy_marks_ready_item_in_progress_and_blocks_on_failed_verification() {
        let mut state = ContinuityState::default();
        state.ensure_tracked_goal("Implement tests and validate runtime continuity behavior");
        let active = state.todos.begin_next_ready().unwrap();
        assert_eq!(active.status, TodoStatus::InProgress);

        assert!(
            state
                .todos
                .mark_blocked("runtime-goal", "cargo test failed with token=secret-value")
        );
        let item = state
            .todos
            .items
            .iter()
            .find(|item| item.id == "runtime-goal")
            .unwrap();
        assert_eq!(item.status, TodoStatus::Blocked);
        assert_eq!(
            item.blocked_reason.as_deref(),
            Some("cargo test failed with token=[REDACTED]")
        );
    }

    #[test]
    fn attention_manifest_replaces_prior_manifest_message() {
        let state = state_with_todo();
        let mut messages = vec![
            serde_json::json!({"role": "user", "content": "hello"}),
            serde_json::json!({"role": "user", "content": "[attention:v1]\ngoal: stale"}),
        ];

        assert!(append_attention_manifest_message(
            &mut messages,
            &state,
            2_000
        ));
        assert_eq!(
            messages
                .iter()
                .filter(|m| m
                    .get("content")
                    .and_then(|c| c.as_str())
                    .is_some_and(|content| content.starts_with("[attention:v1]")))
                .count(),
            1
        );
        assert!(
            messages
                .last()
                .unwrap()
                .get("content")
                .unwrap()
                .as_str()
                .unwrap()
                .contains("runtime-todo")
        );
    }
}
