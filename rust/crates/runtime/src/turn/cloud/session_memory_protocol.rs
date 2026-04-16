//! Session Memory Protocol v1 — types, parsing, validation, and compression.
//!
//! Implements the L0/L1 layers from `docs/design/session-memory-protocol.md`.

use serde_json::Value;

use super::session_facts::SessionFacts;

// ── L0: Session Anchor ──────────────────────────────────────────────────────

const ANCHOR_PREFIX: &str = "[session-anchor] ";
const MAX_TASK_WORDS: usize = 20;

/// Build an L0 anchor from SessionFacts (ground truth) + optional narrative.
/// Preferred over `extract_anchor` when facts are available.
pub fn extract_anchor_from_facts(
    first_user_msg: &str,
    facts: &SessionFacts,
    narrative: Option<&SessionMemory>,
) -> String {
    // Task: from narrative if available (LLM good at summarizing), fallback to first user msg
    let task = narrative
        .and_then(|n| n.section("Task Specification"))
        .map(|s| first_sentence(s).to_string())
        .unwrap_or_else(|| truncate_words(first_user_msg, MAX_TASK_WORDS));

    // State: from system facts (ground truth)
    let state = if let Some(plan) = &facts.plan_state {
        let sub = plan.current_subtask.as_deref().unwrap_or("unknown");
        format!("{}/{} subtasks, current: {sub}", plan.completed, plan.total)
    } else if let Some(f) = facts.active_files.last() {
        format!("{} {} (t{})", f.last_action, f.path, f.turn)
    } else {
        "starting".to_string()
    };

    let mut anchor = format!("{ANCHOR_PREFIX}Goal: {task}. State: {state}.");

    // Constraints from system facts
    if let Some(err) = &facts.error_state.last_error {
        let short = truncate_words(err, 10);
        anchor.push_str(&format!(" Last error: {short}."));
    }
    if !facts.blocked_tools.is_empty() {
        anchor.push_str(&format!(" Avoid: {}.", facts.blocked_tools.join(", ")));
    }

    anchor
}

/// Build an L0 anchor line from the first user message or from a parsed L1.
/// Legacy path — used when SessionFacts is not available.
pub fn extract_anchor(first_user_msg: &str, l1: Option<&SessionMemory>) -> String {
    if let Some(l1) = l1 {
        let task = first_sentence(l1.section("Task Specification").unwrap_or(""));
        let current = first_sentence(l1.section("Current State").unwrap_or(""));
        let (done, total) = count_progress_markers(l1.section("Progress").unwrap_or(""));
        format!("{ANCHOR_PREFIX}{task}. Currently: {current}. {done}/{total} steps.")
    } else {
        let task = truncate_words(first_user_msg, MAX_TASK_WORDS);
        format!("{ANCHOR_PREFIX}{task}. Currently: starting. 0/0 steps.")
    }
}

fn first_sentence(text: &str) -> &str {
    let text = text.trim();
    let sentence = text
        .match_indices(['.', '。', '\n'])
        .next()
        .map(|(i, s)| text[..i + s.len()].trim_end_matches('\n'))
        .unwrap_or(text);
    // Guarantee single-line output
    sentence.lines().next().unwrap_or("")
}

fn truncate_words(text: &str, max_words: usize) -> String {
    // CJK-aware: count CJK characters as individual "words" since they
    // lack whitespace boundaries. Mixed text uses a blended count.
    let mut result = String::new();
    let mut count = 0;
    let mut in_word = false;
    for ch in text.chars() {
        if ch.is_whitespace() {
            if in_word {
                // End of an ASCII word - count it
                count += 1;
                result.push(' ');
                in_word = false;
            }
            continue;
        }
        if count >= max_words {
            break;
        }
        // CJK characters each count as one "word" unit
        if is_cjk_char(ch) {
            if in_word {
                // End of an ASCII word before CJK - count it
                count += 1;
                in_word = false;
            }
            if !result.is_empty() && !result.ends_with(' ') {
                result.push(' ');
            }
            result.push(ch);
            count += 1;
        } else {
            // Accumulate ASCII/Latin into word groups
            result.push(ch);
            in_word = true;
        }
    }
    result
}

fn is_cjk_char(ch: char) -> bool {
    matches!(ch,
        '\u{4E00}'..='\u{9FFF}' |   // CJK Unified Ideographs
        '\u{3400}'..='\u{4DBF}' |   // CJK Extension A
        '\u{3000}'..='\u{303F}' |   // CJK Symbols and Punctuation
        '\u{FF00}'..='\u{FFEF}' |   // Halfwidth and Fullwidth Forms
        '\u{2E80}'..='\u{2EFF}' |   // CJK Radicals Supplement
        '\u{AC00}'..='\u{D7AF}'     // Hangul Syllables
    )
}

/// Truncate text to fit within a token budget (~4 chars/token).
fn truncate_to_token_budget(text: &str, max_tokens: usize) -> String {
    let max_chars = max_tokens * 4;
    if text.len() <= max_chars {
        return text.to_string();
    }
    // Find a clean break point (word boundary) near the limit
    let truncated = &text[..text.floor_char_boundary(max_chars)];
    truncated
        .rsplit_once(char::is_whitespace)
        .map(|(left, _)| left)
        .unwrap_or(truncated)
        .to_string()
}

fn count_progress_markers(text: &str) -> (usize, usize) {
    let mut done = 0;
    let mut total = 0;
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("✅") || trimmed.starts_with("🔄") || trimmed.starts_with("⏳") {
            total += 1;
            if trimmed.starts_with("✅") {
                done += 1;
            }
        }
    }
    (done, total)
}

// ── L1: Session Memory ──────────────────────────────────────────────────────

pub const SESSION_MEMORY_PREFIX: &str = "[session-memory:v1]";

const REQUIRED_SECTIONS: &[&str] = &["Task Specification", "Current State", "User Messages"];

#[cfg(test)]
const SECTION_NAMES: &[&str] = &[
    "Session Title",
    "Task Specification",
    "Current State",
    "Key Files",
    "Progress",
    "Errors & Corrections",
    "Decisions",
    "User Messages",
    "Worklog",
    "Context",
];

/// Per-section token budgets for the stored version (≤4000 total).
pub const STORED_SECTION_BUDGETS: &[(&str, usize)] = &[
    ("Session Title", 20),
    ("Task Specification", 200),
    ("Current State", 400),
    ("Key Files", 500),
    ("Progress", 400),
    ("Errors & Corrections", 500),
    ("Decisions", 400),
    ("User Messages", 800),
    ("Worklog", 700),
    ("Context", 50),
];

pub const STORED_TOTAL_BUDGET: usize = 4000;
pub const INJECTION_TOTAL_BUDGET: usize = 2000;

/// Parsed session memory with section access.
#[derive(Debug, Clone)]
pub struct SessionMemory {
    pub raw: String,
    sections: Vec<(String, String)>, // (name, content)
}

impl SessionMemory {
    /// Parse a `[session-memory:v1]` markdown string into sections.
    pub fn parse(raw: &str) -> Option<Self> {
        let trimmed = raw.trim();
        if !trimmed.starts_with(SESSION_MEMORY_PREFIX) {
            return None;
        }
        let mut sections = Vec::new();
        let mut current_name: Option<String> = None;
        let mut current_content = String::new();

        for line in trimmed.lines() {
            if let Some(name) = line.strip_prefix("# ") {
                if let Some(prev_name) = current_name.take() {
                    sections.push((prev_name, current_content.trim().to_string()));
                    current_content.clear();
                }
                current_name = Some(name.trim().to_string());
            } else if current_name.is_some() {
                current_content.push_str(line);
                current_content.push('\n');
            }
        }
        if let Some(name) = current_name {
            sections.push((name, current_content.trim().to_string()));
        }

        Some(Self {
            raw: raw.to_string(),
            sections,
        })
    }

    /// Get content of a named section.
    pub fn section(&self, name: &str) -> Option<&str> {
        self.sections
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, c)| c.as_str())
    }

    /// List all section names present.
    pub fn section_names(&self) -> Vec<&str> {
        self.sections.iter().map(|(n, _)| n.as_str()).collect()
    }

    /// Validate that required sections are present and non-empty.
    pub fn validate(&self) -> Result<(), Vec<String>> {
        let mut errors = Vec::new();
        for &name in REQUIRED_SECTIONS {
            match self.section(name) {
                None => errors.push(format!("missing section: {name}")),
                Some(c) if c.trim().is_empty() => errors.push(format!("empty section: {name}")),
                _ => {}
            }
        }
        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors)
        }
    }

    /// Estimate token count (~4 chars per token).
    pub fn estimate_tokens(&self) -> usize {
        self.raw.len() / 4
    }

    /// Estimate tokens for a single section.
    pub fn section_tokens(&self, name: &str) -> usize {
        self.section(name).map(|c| c.len() / 4).unwrap_or(0)
    }

    /// Check which sections exceed their stored-version budget.
    pub fn over_budget_sections(&self) -> Vec<(&str, usize, usize)> {
        let mut result = Vec::new();
        for &(name, budget) in STORED_SECTION_BUDGETS {
            let tokens = self.section_tokens(name);
            if tokens > budget {
                result.push((name, tokens, budget));
            }
        }
        result
    }
}

/// Compress a stored L1 into the injection version (≤2000 tokens), zero LLM.
pub fn compress_to_injection(l1: &SessionMemory) -> String {
    let mut out = String::from(SESSION_MEMORY_PREFIX);
    out.push('\n');

    // Task Specification — full text
    if let Some(c) = l1.section("Task Specification") {
        out.push_str("# Task Specification\n");
        out.push_str(c);
        out.push('\n');
    }

    // Current State — full text
    if let Some(c) = l1.section("Current State") {
        out.push_str("# Current State\n");
        out.push_str(c);
        out.push('\n');
    }

    // Key Files — file names only
    if let Some(c) = l1.section("Key Files") {
        out.push_str("# Key Files\n");
        for line in c.lines() {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            // "path — description" → "path"
            let name = trimmed.split(" — ").next().unwrap_or(trimmed);
            let name = name.split(" - ").next().unwrap_or(name);
            out.push_str(name.trim());
            out.push('\n');
        }
    }

    // Progress — only 🔄 and ⏳
    if let Some(c) = l1.section("Progress") {
        let pending: Vec<&str> = c
            .lines()
            .filter(|l| {
                let t = l.trim();
                t.starts_with("🔄") || t.starts_with("⏳")
            })
            .collect();
        if !pending.is_empty() {
            out.push_str("# Progress\n");
            for line in pending {
                out.push_str(line.trim());
                out.push('\n');
            }
        }
    }

    // Errors & Corrections — unresolved + user corrections
    if let Some(c) = l1.section("Errors & Corrections") {
        let kept: Vec<&str> = c
            .lines()
            .filter(|l| {
                let t = l.trim();
                !t.is_empty()
                    && (t.contains("unresolved")
                        || t.contains("UNRESOLVED")
                        || t.contains("user correction")
                        || t.contains("USER CORRECTION")
                        || t.starts_with("- ❌")
                        || t.starts_with("- 🔧"))
            })
            .collect();
        if !kept.is_empty() {
            out.push_str("# Errors & Corrections\n");
            for line in kept {
                out.push_str(line.trim());
                out.push('\n');
            }
        }
    }

    // Decisions — last 2, truncated
    if let Some(c) = l1.section("Decisions") {
        let entries: Vec<&str> = c.lines().filter(|l| l.trim().starts_with("- ")).collect();
        let last_two: Vec<&str> = entries.iter().rev().take(2).rev().copied().collect();
        if !last_two.is_empty() {
            out.push_str("# Decisions\n");
            for line in last_two {
                let words: Vec<&str> = line.split_whitespace().collect();
                let truncated: String = words.into_iter().take(15).collect::<Vec<_>>().join(" ");
                out.push_str(&truncated);
                out.push('\n');
            }
        }
    }

    // User Messages — last 3
    if let Some(c) = l1.section("User Messages") {
        let msgs: Vec<&str> = c.split("\n\n").filter(|s| !s.trim().is_empty()).collect();
        let last_three: Vec<&str> = msgs.iter().rev().take(3).rev().copied().collect();
        if !last_three.is_empty() {
            out.push_str("# User Messages\n");
            out.push_str(&last_three.join("\n\n"));
            out.push('\n');
        }
    }

    // Worklog — omitted
    // Context — omitted

    out
}

/// Build facts-first injection: L1a (system facts) + L1b (narrative) with cross-validation.
/// Returns ~650 tokens at normal pressure. See design doc Section 4.4.
pub fn build_facts_first_injection(
    facts: &SessionFacts,
    narrative: Option<&SessionMemory>,
) -> String {
    let mut out = String::from("[session-memory]\n");

    // ── Layer 1: System Facts (ground truth, ~150t) ──
    out.push_str(&facts.to_injection());

    // Track which narrative sections to skip due to cross-validation
    let mut skip_task = false;

    // ── Layer 2: Cross-validation (detect contradictions BEFORE injecting narrative) ──
    if facts.error_state.total_errors > 0 && facts.error_state.last_error.is_some() {
        if let Some(plan) = &facts.plan_state {
            if plan.completed == plan.total && plan.total > 0 {
                skip_task = true;
                out.push_str(
                    "⚠️ Plan complete but unresolved errors — narrative Task section omitted\n",
                );
            }
        }
    }

    // ── Layer 3: LLM Narrative (supplement, ≤500t) ──
    if let Some(n) = narrative {
        if !skip_task {
            if let Some(task) = n.section("Task Specification") {
                out.push_str("# Task\n");
                out.push_str(&truncate_to_token_budget(task.trim(), 200));
                out.push('\n');
            }
        }
        if let Some(corrections) = n.section("User Corrections") {
            let trimmed = corrections.trim();
            if !trimmed.is_empty() {
                out.push_str("# User Corrections\n");
                out.push_str(&truncate_to_token_budget(trimmed, 150));
                out.push('\n');
            }
        }
        if let Some(learnings) = n.section("Learnings") {
            let entries: Vec<&str> = learnings
                .lines()
                .filter(|l| l.trim().starts_with("- "))
                .collect();
            let last_three: Vec<&str> = entries.iter().rev().take(3).rev().copied().collect();
            if !last_three.is_empty() {
                out.push_str("# Learnings\n");
                for line in &last_three {
                    out.push_str(line.trim());
                    out.push('\n');
                }
            }
        }
        if let Some(decisions) = n.section("Decisions") {
            let entries: Vec<&str> = decisions
                .lines()
                .filter(|l| l.trim().starts_with("- "))
                .collect();
            if let Some(recent) = entries.last() {
                out.push_str("# Last Decision\n");
                out.push_str(recent.trim());
                out.push('\n');
            }
        }
    }

    out
}

/// Extract text content from a message, handling both string and Anthropic content blocks.
pub fn extract_message_text(msg: &Value) -> Option<String> {
    msg.get("content").and_then(|c| {
        c.as_str().map(String::from).or_else(|| {
            c.as_array().and_then(|blocks| {
                let texts: Vec<&str> = blocks
                    .iter()
                    .filter_map(|b| b.get("text").and_then(Value::as_str))
                    .collect();
                if texts.is_empty() {
                    None
                } else {
                    Some(texts.join("\n"))
                }
            })
        })
    })
}

/// Find the end index (exclusive) of the first user message block after `start`.
/// Returns `start` if no user message found.
pub fn first_user_end(messages: &[Value], start: usize) -> usize {
    messages[start..]
        .iter()
        .position(|m| m.get("role").and_then(|v| v.as_str()) == Some("user"))
        .map(|i| start + i + 1)
        .unwrap_or(start)
}

// ── First User Message Preservation ─────────────────────────────────────────

/// Find the index of the first `role: "user"` message in the array.
pub fn first_user_message_index(messages: &[Value]) -> Option<usize> {
    messages.iter().position(|m| {
        m.get("role")
            .and_then(Value::as_str)
            .map(|r| r == "user")
            .unwrap_or(false)
    })
}

/// Check if a message has compact_metadata (is a compaction boundary).
pub fn is_compaction_boundary(msg: &Value) -> bool {
    msg.get("compact_metadata").is_some()
}

// ── Pressure-Adaptive Injection ─────────────────────────────────────────────

/// Determine what to inject based on context pressure.
#[derive(Debug, Clone, PartialEq)]
pub enum InjectionLevel {
    /// L1 injection version (full compressed), ≤2000 tokens
    L1Full,
    /// L1 minimal: Task + Current State + Progress only, ~800 tokens
    L1Minimal,
    /// L0 anchor only, ~50 tokens
    L0Only,
}

/// Pressure thresholds for injection level selection.
pub const DEFAULT_L1_FULL_THRESHOLD: f64 = 0.75;
pub const DEFAULT_L1_MINIMAL_THRESHOLD: f64 = 0.85;

pub fn injection_level_for_pressure(pressure: f64) -> InjectionLevel {
    injection_level_for_pressure_with_thresholds(
        pressure,
        DEFAULT_L1_FULL_THRESHOLD,
        DEFAULT_L1_MINIMAL_THRESHOLD,
    )
}

pub fn injection_level_for_pressure_with_thresholds(
    pressure: f64,
    l1_full_max: f64,
    l1_minimal_max: f64,
) -> InjectionLevel {
    if pressure < l1_full_max {
        InjectionLevel::L1Full
    } else if pressure < l1_minimal_max {
        InjectionLevel::L1Minimal
    } else {
        InjectionLevel::L0Only
    }
}

// ── P3: Persist L1 to Memoria ────────────────────────────────────────────────

/// Purge old L1 for this session, then store the new one with one retry.
/// Extracted from the tokio::spawn body so it can be tested with a mock client.
pub async fn persist_l1(
    client: &dyn crate::turn::cloud::memoria_compact::MemoriaClient,
    l1_content: &str,
    session_id: &str,
) -> Result<String, String> {
    // Best-effort purge of old L1 for this session
    let _ = client.purge_working(session_id).await;

    // Store with one retry
    match client
        .store(l1_content, "working", Some(session_id), Some("T2"))
        .await
    {
        Ok(id) => Ok(id),
        Err(e) => {
            tracing::warn!(session_id = %session_id, attempt = 1, error = %e, "L1 store failed, retrying");
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            client
                .store(l1_content, "working", Some(session_id), Some("T2"))
                .await
                .map_err(|e2| {
                    tracing::warn!(session_id = %session_id, attempt = 2, error = %e2, "L1 store failed, giving up");
                    e2
                })
        }
    }
}

// ── P3: Build L1 from conversation ──────────────────────────────────────────

/// Build an L1 session memory string from the current conversation messages.
/// This is called at turn end to persist session state to Memoria.
pub fn build_l1_from_messages(
    messages: &[Value],
    turn_number: usize,
    estimated_tokens: usize,
) -> String {
    let first_user = messages
        .iter()
        .find(|m| m.get("role").and_then(Value::as_str) == Some("user"))
        .and_then(|m| extract_message_text(m))
        .unwrap_or_default();

    // Collect user messages (deduplicated, last N)
    let mut seen_user_msgs = std::collections::HashSet::new();
    let user_msgs: Vec<String> = messages
        .iter()
        .filter_map(|m| {
            if m.get("role").and_then(Value::as_str) == Some("user") {
                extract_message_text(m).filter(|t| seen_user_msgs.insert(t.to_lowercase()))
            } else {
                None
            }
        })
        .collect();

    // Collect tool names used
    let mut tool_names: Vec<String> = Vec::new();
    let mut seen_tools = std::collections::HashSet::new();
    for m in messages {
        if let Some(calls) = m.get("tool_calls").and_then(Value::as_array) {
            for tc in calls {
                if let Some(name) = tc
                    .get("function")
                    .and_then(|f| f.get("name"))
                    .and_then(Value::as_str)
                {
                    if seen_tools.insert(name.to_string()) {
                        tool_names.push(name.to_string());
                    }
                }
            }
        }
    }

    // Collect file paths from tool calls (read_file, fs_read, etc.)
    let mut files: Vec<String> = Vec::new();
    let mut seen_files = std::collections::HashSet::new();
    for m in messages {
        if let Some(calls) = m.get("tool_calls").and_then(Value::as_array) {
            for tc in calls {
                if let Some(args) = tc
                    .get("function")
                    .and_then(|f| f.get("arguments"))
                    .and_then(Value::as_str)
                {
                    if let Ok(parsed) = serde_json::from_str::<Value>(args) {
                        if let Some(path) = parsed.get("path").and_then(Value::as_str) {
                            if seen_files.insert(path.to_string()) {
                                files.push(path.to_string());
                            }
                        }
                    }
                }
            }
        }
    }

    // Build the L1 markdown
    let task = truncate_to_token_budget(&first_user, 200); // match STORED_SECTION_BUDGETS
    let user_section: String = user_msgs
        .iter()
        .rev()
        .take(10)
        .rev()
        .map(|s| truncate_words(s, 30))
        .collect::<Vec<_>>()
        .join("\n");
    let files_section = files
        .iter()
        .take(20)
        .map(|f| f.as_str())
        .collect::<Vec<_>>()
        .join("\n");

    // Derive current state from last assistant message
    let last_action = messages
        .iter()
        .rev()
        .find(|m| m.get("role").and_then(Value::as_str) == Some("assistant"))
        .and_then(|m| extract_message_text(m))
        .map(|t| truncate_words(&t, 15))
        .unwrap_or_default();
    let current_state = if last_action.is_empty() {
        format!("Turn {turn_number}, active")
    } else {
        format!("Turn {turn_number}, active. {last_action}")
    };

    // Derive progress from tool call count
    let tool_call_count: usize = messages
        .iter()
        .filter_map(|m| m.get("tool_calls").and_then(Value::as_array))
        .map(|a| a.len())
        .sum();
    let progress = if tool_call_count == 0 {
        "🔄 In progress".to_string()
    } else {
        format!("✅ {tool_call_count} tool calls completed\n🔄 Turn {turn_number} in progress")
    };

    format!(
        "{SESSION_MEMORY_PREFIX}\n\
         # Session Title\n{title}\n\
         # Task Specification\n{task}\n\
         # Current State\n{current_state}\n\
         # Key Files\n{files}\n\
         # Progress\n{progress}\n\
         # Errors & Corrections\nNone\n\
         # Decisions\nTools used: {tools}\n\
         # User Messages\n{users}\n\
         # Worklog\nTurn {turn_number}\n\
         # Context\nTurn {turn_number}, ~{tokens}K tokens",
        title = truncate_words(&first_user, 10),
        task = task,
        files = if files_section.is_empty() {
            "None".to_string()
        } else {
            files_section
        },
        tools = if tool_names.is_empty() {
            "none".to_string()
        } else {
            tool_names.join(", ")
        },
        users = user_section,
        tokens = estimated_tokens / 1000,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // ── Helpers ──────────────────────────────────────────────────────────

    fn sample_l1() -> &'static str {
        "[session-memory:v1]\n\
         # Session Title\n\
         OAuth API Implementation\n\
         # Task Specification\n\
         Add OAuth support to API with JWT tokens per RFC 6749.\n\
         # Current State\n\
         Implementing token refresh logic in src/auth/refresh.rs.\n\
         # Key Files\n\
         src/auth/mod.rs — added OAuthConfig struct\n\
         src/routes/oauth.rs — authorization endpoints\n\
         src/auth/refresh.rs — token refresh handler\n\
         # Progress\n\
         ✅ OAuth client registration\n\
         ✅ Authorization code flow\n\
         ✅ JWT signing (RS256)\n\
         🔄 Token refresh (in progress)\n\
         ⏳ PKCE support\n\
         ⏳ Integration tests\n\
         # Errors & Corrections\n\
         - ❌ sqlx migration error: column already exists — UNRESOLVED\n\
         - 🔧 user correction: use RS256 not HS256\n\
         - ✅ JWT panic on empty kid — fixed by defaulting to first key\n\
         # Decisions\n\
         - RS256 over HS256 for key rotation support\n\
         - Separate oauth_tokens table to avoid polluting sessions\n\
         - 5min refresh buffer to prevent race condition\n\
         # User Messages\n\
         Add OAuth support to the API with JWT tokens\n\n\
         Use RS256 instead of HS256\n\n\
         Also add PKCE support\n\n\
         Make sure the refresh token has a 5 minute buffer\n\
         # Worklog\n\
         Turn 1 — scaffolded OAuth routes\n\
         Turn 3 — implemented JWT signing\n\
         Turn 5 — started token refresh\n\
         # Context\n\
         Turn 8, ~45K tokens, pressure 65%"
    }

    fn sample_l1_missing_sections() -> &'static str {
        "[session-memory:v1]\n\
         # Session Title\n\
         Test Session\n\
         # Current State\n\
         Working on something\n\
         # Key Files\n\
         foo.rs"
    }

    fn sample_l1_empty_required() -> &'static str {
        "[session-memory:v1]\n\
         # Session Title\n\
         Test\n\
         # Task Specification\n\
         \n\
         # Current State\n\
         Working\n\
         # User Messages\n\
         hello"
    }

    // ── L0 Anchor Tests ─────────────────────────────────────────────────

    #[test]
    fn anchor_from_first_user_message() {
        let anchor = extract_anchor("Add OAuth support to the API with JWT tokens", None);
        assert!(anchor.starts_with("[session-anchor] "));
        assert!(anchor.contains("Add OAuth support"));
        assert!(anchor.contains("Currently: starting"));
        assert!(anchor.contains("0/0 steps"));
    }

    #[test]
    fn anchor_from_l1() {
        let l1 = SessionMemory::parse(sample_l1()).unwrap();
        let anchor = extract_anchor("ignored", Some(&l1));
        assert!(anchor.starts_with("[session-anchor] "));
        assert!(anchor.contains("OAuth"));
        assert!(anchor.contains("token refresh"));
        assert!(anchor.contains("3/6 steps"));
    }

    #[test]
    fn anchor_truncates_long_user_message() {
        let long_msg = (0..50)
            .map(|i| format!("word{i}"))
            .collect::<Vec<_>>()
            .join(" ");
        let anchor = extract_anchor(&long_msg, None);
        let words: Vec<&str> = anchor
            .strip_prefix("[session-anchor] ")
            .unwrap()
            .split(". Currently:")
            .next()
            .unwrap()
            .split_whitespace()
            .collect();
        assert!(words.len() <= MAX_TASK_WORDS);
    }

    // ── L0 Anchor from Facts Tests ──────────────────────────────────────

    #[test]
    fn facts_anchor_with_plan_state() {
        use super::super::session_facts::{PlanFact, SessionFacts};
        let mut facts = SessionFacts::default();
        facts.plan_state = Some(PlanFact {
            goal: "Implement OAuth".to_string(),
            completed: 3,
            total: 5,
            current_subtask: Some("token refresh".to_string()),
        });
        let anchor = extract_anchor_from_facts("Add OAuth support", &facts, None);
        assert!(
            anchor.starts_with("[session-anchor] Goal:"),
            "anchor: {anchor}"
        );
        assert!(anchor.contains("OAuth"), "anchor: {anchor}");
        assert!(anchor.contains("3/5 subtasks"), "anchor: {anchor}");
        assert!(
            anchor.contains("current: token refresh"),
            "anchor: {anchor}"
        );
    }

    #[test]
    fn facts_anchor_with_active_file_no_plan() {
        use super::super::session_facts::{FileEntry, SessionFacts};
        let mut facts = SessionFacts::default();
        facts.active_files.push(FileEntry {
            path: "src/auth.rs".to_string(),
            last_action: "write".to_string(),
            turn: 7,
        });
        let anchor = extract_anchor_from_facts("Fix auth bug", &facts, None);
        assert!(anchor.contains("State: write src/auth.rs (t7)"));
    }

    #[test]
    fn facts_anchor_empty_facts_shows_starting() {
        use super::super::session_facts::SessionFacts;
        let facts = SessionFacts::default();
        let anchor = extract_anchor_from_facts("Build something", &facts, None);
        assert!(anchor.contains("State: starting"));
    }

    #[test]
    fn facts_anchor_includes_last_error() {
        use super::super::session_facts::{ErrorFact, SessionFacts};
        let mut facts = SessionFacts::default();
        facts.error_state = ErrorFact {
            total_errors: 2,
            last_error: Some("sqlx migration column exists".to_string()),
            last_error_turn: Some(5),
        };
        let anchor = extract_anchor_from_facts("Fix DB", &facts, None);
        assert!(anchor.contains("Last error:"), "anchor: {anchor}");
        assert!(anchor.contains("sqlx"), "anchor: {anchor}");
    }

    #[test]
    fn facts_anchor_includes_blocked_tools() {
        use super::super::session_facts::SessionFacts;
        let mut facts = SessionFacts::default();
        facts.blocked_tools = vec!["web_fetch".to_string(), "rm".to_string()];
        let anchor = extract_anchor_from_facts("Do stuff", &facts, None);
        assert!(anchor.contains("Avoid: web_fetch, rm"));
    }

    #[test]
    fn facts_anchor_prefers_narrative_task_spec() {
        use super::super::session_facts::SessionFacts;
        let facts = SessionFacts::default();
        let l1 = SessionMemory::parse(sample_l1()).unwrap();
        let anchor = extract_anchor_from_facts("raw user msg ignored", &facts, Some(&l1));
        // Should use Task Specification from narrative, not the raw user msg
        assert!(anchor.contains("OAuth"));
        assert!(!anchor.contains("raw user msg"));
    }

    // ── Facts-First Injection Tests ────────────────────────────────────

    fn narrative_with_sections(sections: &[(&str, &str)]) -> SessionMemory {
        let mut text = String::from("[session-memory:v1]\n");
        for (name, content) in sections {
            text.push_str(&format!("# {name}\n{content}\n"));
        }
        SessionMemory::parse(&text).unwrap()
    }

    #[test]
    fn injection_facts_before_narrative() {
        use super::super::session_facts::{FileEntry, SessionFacts};
        let mut facts = SessionFacts::default();
        facts.turn = 5;
        facts.estimated_tokens = 20000;
        facts.active_files.push(FileEntry {
            path: "src/main.rs".to_string(),
            last_action: "write".to_string(),
            turn: 5,
        });
        let narrative = narrative_with_sections(&[
            ("Task Specification", "Build a web server"),
            ("Decisions", "- Use axum framework"),
        ]);
        let injection = build_facts_first_injection(&facts, Some(&narrative));
        // System State must come before Task
        let facts_pos = injection.find("# System State").unwrap();
        let task_pos = injection.find("# Task").unwrap();
        assert!(facts_pos < task_pos, "facts must come before narrative");
    }

    #[test]
    fn injection_includes_narrative_sections() {
        use super::super::session_facts::SessionFacts;
        let facts = SessionFacts::default();
        let narrative = narrative_with_sections(&[
            ("Task Specification", "Implement OAuth"),
            ("User Corrections", "Use RS256 not HS256"),
            (
                "Learnings",
                "- CJK needs special handling\n- Use char_indices",
            ),
            ("Decisions", "- Use axum\n- Use sqlx"),
        ]);
        let injection = build_facts_first_injection(&facts, Some(&narrative));
        assert!(injection.contains("# Task\nImplement OAuth"));
        assert!(injection.contains("# User Corrections\nUse RS256 not HS256"));
        assert!(injection.contains("# Learnings"));
        assert!(injection.contains("# Last Decision"));
        assert!(injection.contains("Use sqlx")); // last decision
    }

    #[test]
    fn injection_without_narrative() {
        use super::super::session_facts::{FileEntry, SessionFacts};
        let mut facts = SessionFacts::default();
        facts.turn = 3;
        facts.estimated_tokens = 10000;
        facts.active_files.push(FileEntry {
            path: "a.rs".to_string(),
            last_action: "read".to_string(),
            turn: 3,
        });
        let injection = build_facts_first_injection(&facts, None);
        assert!(injection.contains("# System State"));
        assert!(injection.contains("Turn 3"));
        assert!(!injection.contains("# Task")); // no narrative
    }

    #[test]
    fn injection_cross_validation_skips_task_on_contradiction() {
        use super::super::session_facts::{ErrorFact, PlanFact, SessionFacts};
        let mut facts = SessionFacts::default();
        facts.plan_state = Some(PlanFact {
            goal: "Build API".to_string(),
            completed: 3,
            total: 3, // all done
            current_subtask: None,
        });
        facts.error_state = ErrorFact {
            total_errors: 1,
            last_error: Some("test failure".to_string()),
            last_error_turn: Some(5),
        };
        let narrative = narrative_with_sections(&[
            ("Task Specification", "Build API — completed successfully"),
            ("User Corrections", "Use RS256"),
        ]);
        let injection = build_facts_first_injection(&facts, Some(&narrative));
        // Task should be SKIPPED due to contradiction
        assert!(
            !injection.contains("# Task"),
            "contradicted Task should be skipped"
        );
        assert!(injection.contains("⚠️"), "should have warning");
        // But User Corrections should still be present
        assert!(injection.contains("# User Corrections"));
        assert!(injection.contains("RS256"));
    }

    #[test]
    fn injection_no_cross_validation_when_no_errors() {
        use super::super::session_facts::{PlanFact, SessionFacts};
        let mut facts = SessionFacts::default();
        facts.plan_state = Some(PlanFact {
            goal: "Build API".to_string(),
            completed: 3,
            total: 3,
            current_subtask: None,
        });
        // No errors — no contradiction
        let narrative = narrative_with_sections(&[("Task Specification", "Build API")]);
        let injection = build_facts_first_injection(&facts, Some(&narrative));
        assert!(injection.contains("# Task\nBuild API")); // Task NOT skipped
        assert!(!injection.contains("⚠️"));
    }

    #[test]
    fn injection_learnings_last_three_only() {
        use super::super::session_facts::SessionFacts;
        let facts = SessionFacts::default();
        let narrative = narrative_with_sections(&[(
            "Learnings",
            "- first\n- second\n- third\n- fourth\n- fifth",
        )]);
        let injection = build_facts_first_injection(&facts, Some(&narrative));
        assert!(!injection.contains("- first"));
        assert!(!injection.contains("- second"));
        assert!(injection.contains("- third"));
        assert!(injection.contains("- fourth"));
        assert!(injection.contains("- fifth"));
    }

    // ── L1 Parsing Tests ────────────────────────────────────────────────

    #[test]
    fn parse_valid_l1() {
        let l1 = SessionMemory::parse(sample_l1()).unwrap();
        assert_eq!(
            l1.section("Session Title"),
            Some("OAuth API Implementation")
        );
        assert!(l1.section("Task Specification").unwrap().contains("OAuth"));
        assert!(l1.section("Current State").unwrap().contains("refresh"));
        assert_eq!(l1.section_names().len(), 10);
    }

    #[test]
    fn parse_rejects_wrong_prefix() {
        assert!(SessionMemory::parse("# Just a markdown file").is_none());
        assert!(SessionMemory::parse("[session-memory:v2]\n# Title\nfoo").is_none());
        assert!(SessionMemory::parse("").is_none());
    }

    #[test]
    fn parse_handles_whitespace_prefix() {
        let with_space = format!("  {}\n# Session Title\nTest", SESSION_MEMORY_PREFIX);
        let l1 = SessionMemory::parse(&with_space).unwrap();
        assert_eq!(l1.section("Session Title"), Some("Test"));
    }

    // ── L1 Validation Tests ─────────────────────────────────────────────

    #[test]
    fn validate_complete_l1() {
        let l1 = SessionMemory::parse(sample_l1()).unwrap();
        assert!(l1.validate().is_ok());
    }

    #[test]
    fn validate_missing_required_sections() {
        let l1 = SessionMemory::parse(sample_l1_missing_sections()).unwrap();
        let errors = l1.validate().unwrap_err();
        assert!(errors.iter().any(|e| e.contains("Task Specification")));
        assert!(errors.iter().any(|e| e.contains("User Messages")));
        assert!(!errors.iter().any(|e| e.contains("Current State"))); // present
    }

    #[test]
    fn validate_empty_required_section() {
        let l1 = SessionMemory::parse(sample_l1_empty_required()).unwrap();
        let errors = l1.validate().unwrap_err();
        assert!(
            errors
                .iter()
                .any(|e| e.contains("empty section: Task Specification"))
        );
    }

    // ── L1 Size Governance Tests ────────────────────────────────────────

    #[test]
    fn budget_constants_sum_correctly() {
        let total: usize = STORED_SECTION_BUDGETS.iter().map(|(_, b)| b).sum();
        assert!(
            total <= STORED_TOTAL_BUDGET,
            "section budgets sum to {total}, exceeds stored total {STORED_TOTAL_BUDGET}"
        );
    }

    #[test]
    fn over_budget_detection() {
        // Build an L1 with an oversized Worklog section
        let big_worklog = "x ".repeat(4000); // ~1000 tokens
        let raw = format!(
            "{SESSION_MEMORY_PREFIX}\n\
             # Session Title\nTest\n\
             # Task Specification\nDo something\n\
             # Current State\nWorking\n\
             # Key Files\nfoo.rs\n\
             # Progress\n✅ step1\n\
             # Errors & Corrections\nNone\n\
             # Decisions\n- decision1\n\
             # User Messages\nHello\n\
             # Worklog\n{big_worklog}\n\
             # Context\nTurn 1"
        );
        let l1 = SessionMemory::parse(&raw).unwrap();
        let over = l1.over_budget_sections();
        assert!(
            over.iter().any(|(name, _, _)| *name == "Worklog"),
            "Worklog should be over budget"
        );
    }

    #[test]
    fn normal_l1_within_budget() {
        let l1 = SessionMemory::parse(sample_l1()).unwrap();
        let over = l1.over_budget_sections();
        assert!(
            over.is_empty(),
            "sample L1 should be within budget: {over:?}"
        );
    }

    // ── L1 Injection Compression Tests ──────────────────────────────────

    #[test]
    fn injection_contains_required_sections() {
        let l1 = SessionMemory::parse(sample_l1()).unwrap();
        let injected = compress_to_injection(&l1);
        assert!(injected.starts_with(SESSION_MEMORY_PREFIX));
        assert!(injected.contains("# Task Specification"));
        assert!(injected.contains("# Current State"));
    }

    #[test]
    fn injection_omits_worklog_and_context() {
        let l1 = SessionMemory::parse(sample_l1()).unwrap();
        let injected = compress_to_injection(&l1);
        assert!(!injected.contains("# Worklog"));
        assert!(!injected.contains("# Context"));
    }

    #[test]
    fn injection_filters_completed_progress() {
        let l1 = SessionMemory::parse(sample_l1()).unwrap();
        let injected = compress_to_injection(&l1);
        // Should NOT contain completed items
        assert!(!injected.contains("✅"));
        // Should contain in-progress and pending
        assert!(injected.contains("🔄"));
        assert!(injected.contains("⏳"));
    }

    #[test]
    fn injection_strips_file_descriptions() {
        let l1 = SessionMemory::parse(sample_l1()).unwrap();
        let injected = compress_to_injection(&l1);
        // Should have file names but not descriptions
        assert!(injected.contains("src/auth/mod.rs"));
        assert!(!injected.contains("added OAuthConfig struct"));
    }

    #[test]
    fn injection_keeps_only_last_3_user_messages() {
        let l1 = SessionMemory::parse(sample_l1()).unwrap();
        let injected = compress_to_injection(&l1);
        // Original has 4 user messages, injection should have last 3
        assert!(!injected.contains("Add OAuth support to the API with JWT tokens"));
        assert!(injected.contains("Use RS256 instead of HS256"));
        assert!(injected.contains("Also add PKCE support"));
        assert!(injected.contains("5 minute buffer"));
    }

    #[test]
    fn injection_keeps_only_last_2_decisions() {
        let l1 = SessionMemory::parse(sample_l1()).unwrap();
        let injected = compress_to_injection(&l1);
        // Original has 3 decisions, injection should have last 2
        assert!(!injected.contains("RS256 over HS256"));
        assert!(injected.contains("oauth_tokens table"));
        assert!(injected.contains("refresh buffer"));
    }

    #[test]
    fn injection_keeps_unresolved_errors_and_user_corrections() {
        let l1 = SessionMemory::parse(sample_l1()).unwrap();
        let injected = compress_to_injection(&l1);
        assert!(injected.contains("UNRESOLVED"));
        assert!(injected.contains("user correction"));
        // Resolved error should be filtered
        assert!(!injected.contains("fixed by defaulting"));
    }

    #[test]
    fn injection_smaller_than_stored() {
        let l1 = SessionMemory::parse(sample_l1()).unwrap();
        let injected = compress_to_injection(&l1);
        let injection_tokens = injected.len() / 4;
        let stored_tokens = l1.estimate_tokens();
        assert!(
            injection_tokens < stored_tokens,
            "injection ({injection_tokens}t) should be smaller than stored ({stored_tokens}t)"
        );
    }

    // ── First User Message Preservation Tests ───────────────────────────

    #[test]
    fn find_first_user_message() {
        let msgs = vec![
            json!({"role": "system", "content": "you are helpful"}),
            json!({"role": "user", "content": "do the thing"}),
            json!({"role": "assistant", "content": "ok"}),
        ];
        assert_eq!(first_user_message_index(&msgs), Some(1));
    }

    #[test]
    fn no_user_message() {
        let msgs = vec![
            json!({"role": "system", "content": "system"}),
            json!({"role": "assistant", "content": "hello"}),
        ];
        assert_eq!(first_user_message_index(&msgs), None);
    }

    // ── Compaction Boundary Detection ───────────────────────────────────

    #[test]
    fn detect_compaction_boundary() {
        let boundary_msg = json!({
            "role": "system",
            "content": "[Context compacted]",
            "compact_metadata": {"tier": "compact_history"}
        });
        assert!(is_compaction_boundary(&boundary_msg));

        let normal_msg = json!({"role": "user", "content": "hello"});
        assert!(!is_compaction_boundary(&normal_msg));
    }

    // ── Pressure-Adaptive Injection Tests ───────────────────────────────

    #[test]
    fn injection_level_low_pressure() {
        assert_eq!(injection_level_for_pressure(0.5), InjectionLevel::L1Full);
        assert_eq!(injection_level_for_pressure(0.74), InjectionLevel::L1Full);
    }

    #[test]
    fn injection_level_medium_pressure() {
        assert_eq!(
            injection_level_for_pressure(0.75),
            InjectionLevel::L1Minimal
        );
        assert_eq!(
            injection_level_for_pressure(0.84),
            InjectionLevel::L1Minimal
        );
    }

    #[test]
    fn injection_level_high_pressure() {
        assert_eq!(injection_level_for_pressure(0.85), InjectionLevel::L0Only);
        assert_eq!(injection_level_for_pressure(0.95), InjectionLevel::L0Only);
        assert_eq!(injection_level_for_pressure(1.0), InjectionLevel::L0Only);
    }

    #[test]
    fn injection_level_post_compaction() {
        // Post-compaction pressure is typically low → L1Full
        assert_eq!(injection_level_for_pressure(0.3), InjectionLevel::L1Full);
    }

    // ── Progress Counting ───────────────────────────────────────────────

    #[test]
    fn count_progress_empty() {
        assert_eq!(count_progress_markers(""), (0, 0));
    }

    #[test]
    fn count_progress_mixed() {
        let text = "✅ done1\n✅ done2\n🔄 wip\n⏳ pending\nsome other line";
        assert_eq!(count_progress_markers(text), (2, 4));
    }

    // ── Edge Cases ──────────────────────────────────────────────────────

    #[test]
    fn anchor_from_empty_message() {
        let anchor = extract_anchor("", None);
        assert!(anchor.starts_with("[session-anchor] "));
        assert!(anchor.contains("Currently: starting"));
    }

    #[test]
    fn compress_minimal_l1() {
        let raw = format!(
            "{SESSION_MEMORY_PREFIX}\n\
             # Task Specification\nDo X\n\
             # Current State\nDoing X\n\
             # User Messages\nDo X"
        );
        let l1 = SessionMemory::parse(&raw).unwrap();
        assert!(l1.validate().is_ok());
        let injected = compress_to_injection(&l1);
        assert!(injected.contains("Do X"));
        assert!(injected.contains("Doing X"));
    }

    #[test]
    fn section_names_match_protocol() {
        let l1 = SessionMemory::parse(sample_l1()).unwrap();
        for &name in SECTION_NAMES {
            assert!(
                l1.section(name).is_some(),
                "sample L1 missing section: {name}"
            );
        }
    }

    #[test]
    fn build_l1_from_messages_produces_valid_l1() {
        let messages = vec![
            json!({"role": "system", "content": "You are helpful."}),
            json!({"role": "user", "content": "Build a rate limiter using Redis"}),
            json!({"role": "assistant", "content": "I'll start by reading the code.", "tool_calls": [
                {"id": "c1", "type": "function", "function": {"name": "read_file", "arguments": "{\"path\": \"src/main.rs\"}"}}
            ]}),
            json!({"role": "tool", "content": "fn main() {}", "tool_call_id": "c1"}),
            json!({"role": "assistant", "content": "Done with step 1."}),
            json!({"role": "user", "content": "Now add Redis connection"}),
        ];
        let l1_text = build_l1_from_messages(&messages, 2, 50000);
        let l1 = SessionMemory::parse(&l1_text).expect("should parse");
        assert!(
            l1.validate().is_ok(),
            "should be valid: {:?}",
            l1.validate()
        );
        assert!(
            l1.section("Task Specification")
                .unwrap()
                .contains("rate limiter")
        );
        assert!(l1.section("Key Files").unwrap().contains("src/main.rs"));
        assert!(l1.section("Decisions").unwrap().contains("read_file"));
        assert!(
            l1.section("User Messages")
                .unwrap()
                .contains("Redis connection")
        );
        assert!(l1.section("Context").unwrap().contains("50K"));
    }

    #[test]
    fn build_l1_within_budget() {
        // Large conversation
        let mut messages = vec![
            json!({"role": "system", "content": "sys"}),
            json!({"role": "user", "content": "Implement a very complex distributed system with many requirements"}),
        ];
        for i in 0..50 {
            messages.push(json!({"role": "assistant", "content": format!("Step {i} done")}));
            messages
                .push(json!({"role": "user", "content": format!("Continue with step {}", i+1)}));
        }
        let l1_text = build_l1_from_messages(&messages, 50, 100000);
        let tokens = l1_text.len() / 4;
        assert!(
            tokens <= STORED_TOTAL_BUDGET,
            "L1 should be ≤{STORED_TOTAL_BUDGET} tokens, got {tokens}"
        );
    }

    #[test]
    fn first_sentence_handles_cjk_period() {
        // '。' is 3 bytes in UTF-8 — must not slice into the middle of it
        let text = "这是第一句话。这是第二句话。";
        let result = first_sentence(text);
        assert_eq!(result, "这是第一句话。");
    }

    #[test]
    fn first_sentence_handles_ascii_period() {
        assert_eq!(first_sentence("Hello world. More text."), "Hello world.");
    }

    #[test]
    fn build_l1_context_shows_nonzero_tokens() {
        let messages = vec![
            json!({"role": "system", "content": "sys"}),
            json!({"role": "user", "content": "Build something"}),
            json!({"role": "assistant", "content": "Done."}),
        ];
        let l1_text = build_l1_from_messages(&messages, 5, 45000);
        let l1 = SessionMemory::parse(&l1_text).unwrap();
        let ctx = l1.section("Context").unwrap();
        assert!(
            ctx.contains("45K"),
            "Context should show ~45K tokens, got: {ctx}"
        );
    }

    // ── Fix #9: first_sentence strips trailing newline ──────────────────

    #[test]
    fn first_sentence_strips_trailing_newline() {
        let text = "First line\nSecond line";
        let result = first_sentence(text);
        assert_eq!(result, "First line", "anchor must be single-line");
        assert!(!result.contains('\n'));
    }

    #[test]
    fn first_sentence_no_delimiter_still_single_line() {
        // Text with no period/newline — unwrap_or returns full text,
        // but .lines().next() guarantees single-line
        let text = "Very long text with no delimiter at all";
        let result = first_sentence(text);
        assert!(!result.contains('\n'));
        assert_eq!(result, text);
    }

    #[test]
    fn first_sentence_embedded_newline_in_fallback() {
        // Edge case: text has embedded newlines but no sentence-ending punctuation
        // before the first newline — should still return single line
        let text = "Line one\nLine two\nLine three";
        let result = first_sentence(text);
        assert_eq!(result, "Line one");
    }

    // ── Fix #2: Anthropic content blocks in build_l1 ────────────────────

    #[test]
    fn build_l1_handles_anthropic_content_blocks() {
        let messages = vec![
            json!({"role": "system", "content": "sys"}),
            json!({"role": "user", "content": [
                {"type": "text", "text": "Build a distributed cache with LRU eviction"}
            ]}),
            json!({"role": "assistant", "content": "Starting."}),
        ];
        let l1_text = build_l1_from_messages(&messages, 1, 10000);
        let l1 = SessionMemory::parse(&l1_text).unwrap();
        assert!(
            l1.section("Task Specification")
                .unwrap()
                .contains("distributed cache"),
            "Should extract text from Anthropic content blocks"
        );
        assert!(
            l1.section("User Messages")
                .unwrap()
                .contains("distributed cache"),
            "User messages should include Anthropic block content"
        );
    }

    // ── Fix #8: user message deduplication ──────────────────────────────

    #[test]
    fn build_l1_deduplicates_user_messages() {
        let messages = vec![
            json!({"role": "system", "content": "sys"}),
            json!({"role": "user", "content": "continue"}),
            json!({"role": "assistant", "content": "ok"}),
            json!({"role": "user", "content": "continue"}),
            json!({"role": "assistant", "content": "ok"}),
            json!({"role": "user", "content": "continue"}),
            json!({"role": "assistant", "content": "done"}),
        ];
        let l1_text = build_l1_from_messages(&messages, 3, 5000);
        let l1 = SessionMemory::parse(&l1_text).unwrap();
        let user_section = l1.section("User Messages").unwrap();
        let count = user_section.matches("continue").count();
        assert_eq!(
            count, 1,
            "duplicate 'continue' should appear only once, got {count}"
        );
    }

    // ── Fix #5: shared first_user_end helper ────────────────────────────

    #[test]
    fn first_user_end_finds_user_after_system() {
        let msgs = vec![
            json!({"role": "system", "content": "sys"}),
            json!({"role": "user", "content": "task"}),
            json!({"role": "assistant", "content": "ok"}),
        ];
        assert_eq!(first_user_end(&msgs, 1), 2); // end is exclusive: index 1 is user, end = 2
    }

    #[test]
    fn first_user_end_no_user_returns_start() {
        let msgs = vec![
            json!({"role": "system", "content": "sys"}),
            json!({"role": "assistant", "content": "ok"}),
        ];
        assert_eq!(first_user_end(&msgs, 1), 1); // no user found, returns start
    }

    #[test]
    fn first_user_end_skips_tool_before_user() {
        let msgs = vec![
            json!({"role": "system", "content": "sys"}),
            json!({"role": "tool", "content": "stale", "tool_call_id": "x"}),
            json!({"role": "user", "content": "THE TASK"}),
            json!({"role": "assistant", "content": "ok"}),
        ];
        assert_eq!(first_user_end(&msgs, 1), 3); // tool at 1, user at 2, end = 3
    }

    // ── extract_message_text ────────────────────────────────────────────

    #[test]
    fn extract_message_text_string_content() {
        let msg = json!({"role": "user", "content": "hello"});
        assert_eq!(extract_message_text(&msg).unwrap(), "hello");
    }

    #[test]
    fn extract_message_text_anthropic_blocks() {
        let msg = json!({"role": "user", "content": [
            {"type": "text", "text": "first"},
            {"type": "image", "source": {}},
            {"type": "text", "text": "second"}
        ]});
        assert_eq!(extract_message_text(&msg).unwrap(), "first\nsecond");
    }

    #[test]
    fn extract_message_text_empty_blocks() {
        let msg = json!({"role": "user", "content": [{"type": "image", "source": {}}]});
        assert!(extract_message_text(&msg).is_none());
    }

    // ── Fix #10: token-based truncation ─────────────────────────────────

    #[test]
    fn truncate_to_token_budget_short_text() {
        let result = truncate_to_token_budget("short text", 200);
        assert_eq!(result, "short text");
    }

    #[test]
    fn truncate_to_token_budget_long_text() {
        let long = "word ".repeat(500); // ~500 words, ~125 tokens per 100 words
        let result = truncate_to_token_budget(&long, 50); // 50 tokens = ~200 chars
        assert!(
            result.len() <= 200,
            "should be ≤200 chars, got {}",
            result.len()
        );
        assert!(!result.ends_with(' '), "should break at word boundary");
    }

    #[test]
    fn build_l1_task_within_stored_budget() {
        // Very long first user message — Task Specification must stay within budget
        let long_task = "implement ".repeat(200); // ~200 words
        let messages = vec![
            json!({"role": "system", "content": "sys"}),
            json!({"role": "user", "content": long_task}),
            json!({"role": "assistant", "content": "ok"}),
        ];
        let l1_text = build_l1_from_messages(&messages, 1, 10000);
        let l1 = SessionMemory::parse(&l1_text).unwrap();
        let task_tokens = l1.section_tokens("Task Specification");
        let budget = STORED_SECTION_BUDGETS
            .iter()
            .find(|(n, _)| *n == "Task Specification")
            .map(|(_, b)| *b)
            .unwrap();
        assert!(
            task_tokens <= budget + 10, // small margin for overhead
            "Task Specification should be ≤{budget} tokens, got {task_tokens}"
        );
    }

    // ── Fix #11: configurable injection thresholds ──────────────────────

    #[test]
    fn injection_level_custom_thresholds() {
        // Tighter thresholds
        assert_eq!(
            injection_level_for_pressure_with_thresholds(0.5, 0.6, 0.7),
            InjectionLevel::L1Full
        );
        assert_eq!(
            injection_level_for_pressure_with_thresholds(0.65, 0.6, 0.7),
            InjectionLevel::L1Minimal
        );
        assert_eq!(
            injection_level_for_pressure_with_thresholds(0.75, 0.6, 0.7),
            InjectionLevel::L0Only
        );
    }

    #[test]
    fn injection_level_default_matches_constants() {
        // Verify the convenience function uses the documented constants
        assert_eq!(
            injection_level_for_pressure(DEFAULT_L1_FULL_THRESHOLD - 0.01),
            InjectionLevel::L1Full
        );
        assert_eq!(
            injection_level_for_pressure(DEFAULT_L1_FULL_THRESHOLD),
            InjectionLevel::L1Minimal
        );
        assert_eq!(
            injection_level_for_pressure(DEFAULT_L1_MINIMAL_THRESHOLD),
            InjectionLevel::L0Only
        );
    }

    // ── #3/#7: persist_l1 — purge + store + retry ───────────────────────

    mod persist_l1_tests {
        use super::*;
        use crate::turn::cloud::memoria_compact::{MemoriaClient, MemoriaMemory};
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};

        /// Mock that tracks calls and can fail N times before succeeding.
        struct MockMemoria {
            store_calls: AtomicUsize,
            purge_calls: AtomicUsize,
            fail_store_times: AtomicUsize,
            stored: tokio::sync::Mutex<Vec<(String, String)>>, // (content, session_id)
        }

        impl MockMemoria {
            fn new(fail_store_times: usize) -> Self {
                Self {
                    store_calls: AtomicUsize::new(0),
                    purge_calls: AtomicUsize::new(0),
                    fail_store_times: AtomicUsize::new(fail_store_times),
                    stored: tokio::sync::Mutex::new(Vec::new()),
                }
            }
        }

        #[async_trait::async_trait]
        impl MemoriaClient for MockMemoria {
            async fn retrieve_ext(
                &self,
                _q: &str,
                _sid: Option<&str>,
                _k: usize,
                _filter: bool,
            ) -> Result<Vec<MemoriaMemory>, String> {
                Ok(vec![])
            }
            async fn store(
                &self,
                content: &str,
                _mt: &str,
                sid: Option<&str>,
                _tt: Option<&str>,
            ) -> Result<String, String> {
                let n = self.store_calls.fetch_add(1, Ordering::SeqCst);
                let remaining = self.fail_store_times.load(Ordering::SeqCst);
                if n < remaining {
                    return Err(format!("mock store failure #{}", n + 1));
                }
                self.stored
                    .lock()
                    .await
                    .push((content.to_string(), sid.unwrap_or("").to_string()));
                Ok(format!("mem-{n}"))
            }
            async fn purge_working(&self, _sid: &str) -> Result<u64, String> {
                self.purge_calls.fetch_add(1, Ordering::SeqCst);
                Ok(0)
            }
            async fn delete(&self, _id: &str) -> Result<(), String> {
                Ok(())
            }
        }

        #[tokio::test]
        async fn persist_l1_purges_then_stores() {
            let mock = Arc::new(MockMemoria::new(0));
            let result = persist_l1(&*mock, "L1 content", "sess-1").await;
            assert!(result.is_ok());
            assert_eq!(
                mock.purge_calls.load(Ordering::SeqCst),
                1,
                "should purge once"
            );
            assert_eq!(
                mock.store_calls.load(Ordering::SeqCst),
                1,
                "should store once on success"
            );
            let stored = mock.stored.lock().await;
            assert_eq!(stored[0].0, "L1 content");
            assert_eq!(stored[0].1, "sess-1");
        }

        #[tokio::test]
        async fn persist_l1_retries_on_first_failure() {
            let mock = Arc::new(MockMemoria::new(1)); // fail first store, succeed second
            let result = persist_l1(&*mock, "L1 retry", "sess-2").await;
            assert!(result.is_ok(), "should succeed on retry");
            assert_eq!(
                mock.store_calls.load(Ordering::SeqCst),
                2,
                "should call store twice"
            );
            assert_eq!(
                mock.purge_calls.load(Ordering::SeqCst),
                1,
                "purge only once"
            );
        }

        #[tokio::test]
        async fn persist_l1_gives_up_after_two_failures() {
            let mock = Arc::new(MockMemoria::new(2)); // fail both attempts
            let result = persist_l1(&*mock, "L1 fail", "sess-3").await;
            assert!(result.is_err(), "should fail after 2 attempts");
            assert_eq!(
                mock.store_calls.load(Ordering::SeqCst),
                2,
                "should attempt exactly twice"
            );
            assert!(
                mock.stored.lock().await.is_empty(),
                "nothing should be stored"
            );
        }

        #[tokio::test]
        async fn persist_l1_purge_failure_does_not_block_store() {
            // Mock that fails purge but succeeds store
            struct PurgeFailMock;
            #[async_trait::async_trait]
            impl MemoriaClient for PurgeFailMock {
                async fn retrieve_ext(
                    &self,
                    _: &str,
                    _: Option<&str>,
                    _: usize,
                    _: bool,
                ) -> Result<Vec<MemoriaMemory>, String> {
                    Ok(vec![])
                }
                async fn store(
                    &self,
                    _: &str,
                    _: &str,
                    _: Option<&str>,
                    _: Option<&str>,
                ) -> Result<String, String> {
                    Ok("ok".into())
                }
                async fn purge_working(&self, _: &str) -> Result<u64, String> {
                    Err("purge broken".into())
                }
                async fn delete(&self, _: &str) -> Result<(), String> {
                    Ok(())
                }
            }
            let result = persist_l1(&PurgeFailMock, "L1", "s").await;
            assert!(result.is_ok(), "purge failure should not prevent store");
        }
    }
}
