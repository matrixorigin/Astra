//! Session Memory Protocol v1 — types, parsing, validation, and compression.
//!
//! Implements the L0/L1 layers from `docs/design/session-memory-protocol.md`.

use std::fmt;

use serde_json::Value;

use astra_turn_types::continuity::{
    AttentionManifest, ContinuityState, GoalState, TodoItem, TodoState, TodoStatus,
    narrative_task_contradicts_facts, redact_sensitive,
};

use astra_turn_types::session_facts::SessionFacts;

// ── L0: Session Anchor ──────────────────────────────────────────────────────

const ANCHOR_PREFIX: &str = "[session-anchor] ";
const MAX_TASK_WORDS: usize = 20;

/// Structured form of the L0 session anchor.
///
/// Represented as an enum-of-variants so that the two historical textual
/// layouts (facts-based vs legacy L1) each carry only the fields they can
/// actually emit. Illegal combinations — e.g. a `LegacyL1` anchor with
/// `Plan`/`ActiveFile` progress that only `Facts` can produce — are
/// structurally unrepresentable, and `Display` / `is_trivial` don't need
/// any "degrade gracefully" fallback arms.
///
/// Adding a new inner state variant is a compile error everywhere that
/// must react to it — closing the shape-drift hole that caused the
/// `69657ca7` bug where `is_trivial_anchor` string-parsing only knew the
/// legacy shape and let every facts-based turn-1 anchor through.
///
/// [`Display`] is the **single source of truth** for the wire format. All
/// tests and production code render through it so any future change flows
/// through one place and the cached prefix cannot silently drift.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Anchor {
    /// Facts-based: `[session-anchor] Goal: <task>. State: <state>.[ Last error: ….][ Avoid: ….]`
    ///
    /// Produced by [`extract_anchor_from_facts`] when `SessionFacts` is
    /// available. Only this variant carries `last_error` / `blocked_tools`
    /// — those are derived from system facts and have no parallel in the
    /// L1 narrative path.
    Facts {
        task: String,
        state: FactsState,
        last_error: Option<String>,
        blocked_tools: Vec<String>,
    },
    /// Legacy L1: `[session-anchor] <task>. Currently: <current>. <done>/<total> steps.`
    ///
    /// Produced by [`extract_anchor`] — fresh first-turn anchor or L1
    /// narrative derived. No constraints fields: the legacy shape pre-dates
    /// the facts pipeline and never emitted `Last error:` / `Avoid:`.
    LegacyL1 { task: String, state: LegacyState },
}

/// State variants that Facts-based anchors can carry.
///
/// Compiler-enforced: emitters of this shape cannot emit `Narrative` state,
/// and the legacy emitter cannot emit `Plan`/`ActiveFile`. The
/// pre-refactor `Display` had four "degrade gracefully" arms for these
/// illegal combinations; they disappear when shape is a variant rather
/// than a runtime flag.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FactsState {
    /// Fresh session, no plan / active file. Emits `starting`.
    ///
    /// This is the *only* state that can make a Facts anchor trivial, and
    /// only when no `last_error` / `blocked_tools` are attached. See
    /// [`Anchor::is_trivial`].
    Starting,
    /// Facts-derived plan progress. Emits `<done>/<total> subtasks, current: <subtask>`.
    Plan {
        done: u32,
        total: u32,
        current: String,
    },
    /// Facts-derived last-touched file. Emits `<action> <path> (t<turn>)`.
    ActiveFile {
        action: String,
        path: String,
        turn: u32,
    },
}

/// State variants that Legacy-L1 anchors can carry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LegacyState {
    /// Fresh session, no L1 narrative. Emits `starting` + `0/0 steps.`.
    Starting,
    /// L1-narrative derived state with progress counter. Emits
    /// `<current>` + `<done>/<total> steps.`.
    Narrative {
        current: String,
        done: usize,
        total: usize,
    },
}

impl Anchor {
    /// Task string shared by both variants.
    #[must_use]
    pub fn task(&self) -> &str {
        match self {
            Anchor::Facts { task, .. } | Anchor::LegacyL1 { task, .. } => task,
        }
    }

    /// True when the anchor adds no information beyond what the LLM already
    /// sees in the message stream and therefore should NOT be injected into
    /// the dynamic system prompt.
    ///
    /// Trivial iff:
    ///
    /// 1. Inner state is the `Starting` variant of whichever shape is in use
    ///    (no progress, no active file, no narrative).
    /// 2. No `Facts::last_error` / `Facts::blocked_tools` attached.
    /// 3. `task` is a near-verbatim truncation of `current_user_msg` — see
    ///    [`anchor_task_matches_message`].
    ///
    /// Any other combination carries real signal and must be emitted.
    #[must_use]
    pub fn is_trivial(&self, current_user_msg: &str) -> bool {
        match self {
            Anchor::Facts {
                task,
                state,
                last_error,
                blocked_tools,
            } => {
                let state_is_trivial = match state {
                    FactsState::Starting => true,
                    FactsState::Plan { .. } | FactsState::ActiveFile { .. } => false,
                };
                state_is_trivial
                    && last_error.is_none()
                    && blocked_tools.is_empty()
                    && anchor_task_matches_message(task, current_user_msg)
            }
            Anchor::LegacyL1 { task, state } => {
                let state_is_trivial = match state {
                    LegacyState::Starting => true,
                    LegacyState::Narrative { .. } => false,
                };
                state_is_trivial && anchor_task_matches_message(task, current_user_msg)
            }
        }
    }
}

impl fmt::Display for Anchor {
    /// Emit the wire format. **Must stay byte-exact** with the pre-refactor
    /// strings — the cached prefix layout depends on it.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Anchor::Facts {
                task,
                state,
                last_error,
                blocked_tools,
            } => {
                let state_str = match state {
                    FactsState::Starting => "starting".to_string(),
                    FactsState::Plan {
                        done,
                        total,
                        current,
                    } => format!("{done}/{total} subtasks, current: {current}"),
                    FactsState::ActiveFile { action, path, turn } => {
                        format!("{action} {path} (t{turn})")
                    }
                };
                write!(f, "{ANCHOR_PREFIX}Goal: {task}. State: {state_str}.")?;
                if let Some(err) = last_error {
                    write!(f, " Last error: {err}.")?;
                }
                if !blocked_tools.is_empty() {
                    write!(f, " Avoid: {}.", blocked_tools.join(", "))?;
                }
            }
            Anchor::LegacyL1 { task, state } => {
                let (current, done, total) = match state {
                    LegacyState::Starting => ("starting".to_string(), 0usize, 0usize),
                    LegacyState::Narrative {
                        current,
                        done,
                        total,
                    } => (current.clone(), *done, *total),
                };
                write!(
                    f,
                    "{ANCHOR_PREFIX}{task}. Currently: {current}. {done}/{total} steps."
                )?;
            }
        }
        Ok(())
    }
}

/// Build an L0 anchor from SessionFacts (ground truth) + optional narrative.
/// Preferred over `extract_anchor` when facts are available.
///
/// Returns a structured [`Anchor`]; call [`Anchor::to_string`] /
/// `format!("{anchor}")` to render the wire form.
pub fn extract_anchor_from_facts(
    first_user_msg: &str,
    facts: &SessionFacts,
    narrative: Option<&SessionMemory>,
) -> Anchor {
    // Task: from narrative if available (LLM good at summarizing), fallback to first user msg
    let task = narrative
        .and_then(|n| n.section("Task Specification"))
        .map(|s| first_sentence(s).to_string())
        .unwrap_or_else(|| truncate_words(first_user_msg, MAX_TASK_WORDS));

    // State: from system facts (ground truth)
    let state = if let Some(plan) = &facts.plan_state {
        FactsState::Plan {
            done: plan.completed,
            total: plan.total,
            current: plan
                .current_subtask
                .clone()
                .unwrap_or_else(|| "unknown".to_string()),
        }
    } else if let Some(f) = facts.active_files.last() {
        FactsState::ActiveFile {
            action: f.last_action.clone(),
            path: f.path.clone(),
            turn: f.turn,
        }
    } else {
        FactsState::Starting
    };

    let last_error = facts
        .error_state
        .last_error
        .as_ref()
        .map(|err| truncate_words(err, 10));
    let blocked_tools = facts.blocked_tools.clone();

    Anchor::Facts {
        task,
        state,
        last_error,
        blocked_tools,
    }
}

/// Build an L0 anchor from the first user message or from a parsed L1.
/// Legacy path — used when SessionFacts is not available.
///
/// Returns a structured [`Anchor`] in the legacy shape.
pub fn extract_anchor(first_user_msg: &str, l1: Option<&SessionMemory>) -> Anchor {
    if let Some(l1) = l1 {
        let task = first_sentence(l1.section("Task Specification").unwrap_or("")).to_string();
        let current = first_sentence(l1.section("Current State").unwrap_or("")).to_string();
        let (done, total) = count_progress_markers(l1.section("Progress").unwrap_or(""));
        Anchor::LegacyL1 {
            task,
            state: LegacyState::Narrative {
                current,
                done,
                total,
            },
        }
    } else {
        Anchor::LegacyL1 {
            task: truncate_words(first_user_msg, MAX_TASK_WORDS),
            state: LegacyState::Starting,
        }
    }
}

fn anchor_task_matches_message(anchor_task: &str, current_user_msg: &str) -> bool {
    let a = anchor_task.split_whitespace().collect::<Vec<_>>().join(" ");
    let u = current_user_msg
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    if a.is_empty() || u.is_empty() {
        return false;
    }
    // `anchor_task` is the word-bounded truncation of the first user
    // message at MAX_TASK_WORDS. We consider it trivial when the current
    // user message starts with the same prefix (case-insensitive) — i.e.
    // nothing meaningful diverged. Short messages like "hi" will match
    // directly; longer first messages on turn 1 will also match because
    // current_user_msg == first_user_msg.
    let a_lower = a.to_lowercase();
    let u_lower = u.to_lowercase();
    u_lower == a_lower || u_lower.starts_with(&a_lower)
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

/// Given a list of Memoria memories (already filtered by `session_id`),
/// return the single L1 row that readers should treat as authoritative.
///
/// Returns `None` when no row starts with [`SESSION_MEMORY_PREFIX`].
///
/// When multiple L1 rows exist for one session — possible when a prior
/// [`persist_l1`] invocation saw a transient purge failure and the
/// next successful store left a stale row behind — callers MUST use
/// this helper rather than `.find()` directly. The helper picks the
/// highest `retrieval_score` deterministically and emits a single
/// warning so operators can spot the split. `find()` on a list that
/// happens to be unsorted would silently pick an older row.
pub fn pick_latest_l1(
    memories: &[crate::turn::cloud::memoria_compact::MemoriaMemory],
) -> Option<&crate::turn::cloud::memoria_compact::MemoriaMemory> {
    let mut matching = memories
        .iter()
        .filter(|m| m.content.starts_with(SESSION_MEMORY_PREFIX));
    let first = matching.next()?;
    // Common case: exactly one L1 row. Avoid scanning the rest.
    let remaining: Vec<_> = matching.collect();
    if remaining.is_empty() {
        return Some(first);
    }
    tracing::warn!(
        target: "astra_runtime::session_memory::protocol",
        stale_count = remaining.len(),
        "multiple session-memory L1 rows for one session; picking highest retrieval_score"
    );
    let mut best = first;
    let mut best_score = first.retrieval_score.unwrap_or(f64::NEG_INFINITY);
    for m in remaining {
        let s = m.retrieval_score.unwrap_or(f64::NEG_INFINITY);
        if s > best_score {
            best_score = s;
            best = m;
        }
    }
    Some(best)
}

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
///
/// Returns a pressure-adapted injection per design doc §4.8:
/// * `L1Full`    → facts + full validated narrative (~650t)
/// * `L1Minimal` → facts only, narrative skipped (~150t)
/// * `L0Only`    → empty string — caller is responsible for falling back
///   to the L0 anchor which already lives in the dynamic system prompt.
pub fn build_facts_first_injection(
    facts: &SessionFacts,
    narrative: Option<&SessionMemory>,
    level: InjectionLevel,
) -> String {
    if level == InjectionLevel::L0Only {
        // At ≥85% pressure the L0 anchor lives in the dynamic system
        // prompt already (CacheScope::None); returning empty here keeps
        // the compaction injection out of the way so it doesn't
        // double-up.
        return String::new();
    }

    let mut out = String::from("[session-memory]\n");

    // ── Layer 1: System Facts (ground truth, ~150t) ──
    if facts_have_attention_value(facts) {
        let attention =
            AttentionManifest::from_state(&continuity_from_facts(facts), 4_000).into_string();
        out.push_str(&attention);
        if !out.ends_with('\n') {
            out.push('\n');
        }
    }
    out.push_str(&facts.to_injection());

    // ── Layer 2 (narrative) only at L1Full. At L1Minimal the budget
    //     doesn't justify ~500t of prose — protocol §4.8.
    if level == InjectionLevel::L1Full {
        append_validated_narrative(&mut out, facts, narrative);
    }

    out
}

/// Build continuity-first injection: runtime-owned attention/todo state first,
/// then SessionFacts, then LLM narrative only as validated supplement.
/// Pressure-adaptive mirror of [`build_facts_first_injection`].
pub fn build_continuity_first_injection(
    continuity: &ContinuityState,
    narrative: Option<&SessionMemory>,
    level: InjectionLevel,
) -> String {
    if level == InjectionLevel::L0Only {
        return String::new();
    }

    let mut out = String::from("[session-memory]\n");
    let attention = AttentionManifest::from_state(continuity, 4_000).into_string();
    out.push_str(&attention);
    if !out.ends_with('\n') {
        out.push('\n');
    }
    out.push_str(&continuity.facts.to_injection());
    if level == InjectionLevel::L1Full {
        append_validated_narrative(&mut out, &continuity.facts, narrative);
    }
    out
}

fn facts_have_attention_value(facts: &SessionFacts) -> bool {
    facts.plan_state.is_some()
        || !facts.active_files.is_empty()
        || facts.error_state.total_errors > 0
        || !facts.blocked_tools.is_empty()
}

fn continuity_from_facts(facts: &SessionFacts) -> ContinuityState {
    let goal = facts
        .plan_state
        .as_ref()
        .map(|plan| plan.goal.clone())
        .unwrap_or_default();
    let todos = facts
        .plan_state
        .as_ref()
        .and_then(|plan| {
            plan.current_subtask.as_ref().map(|subtask| TodoState {
                items: vec![TodoItem {
                    id: "session-plan-current".to_string(),
                    title: subtask.clone(),
                    description: subtask.clone(),
                    status: TodoStatus::InProgress,
                    evidence: Vec::new(),
                    blocked_reason: None,
                }],
            })
        })
        .unwrap_or_default();

    ContinuityState {
        goal: GoalState {
            text: goal,
            source_turn: None,
        },
        todos,
        facts: facts.clone(),
        user_corrections: Vec::new(),
        verification: Default::default(),
    }
}

/// Inspect facts + narrative for cross-validation signals that the
/// narrative is stale and a fresh L1b extraction would help. Purely a
/// read — nothing is mutated. The caller decides how to act (typically
/// by requesting a re-extraction with `had_error=true` semantics or
/// emitting an operator-visible event). See design doc §4.4.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct NarrativeStaleness {
    /// Plan is complete but there are unresolved errors — the
    /// narrative's Task Specification may claim "done" contradicting
    /// facts. Consumers of injection already skip the Task section;
    /// this flag surfaces the same signal to other call sites.
    pub task_contradicted: bool,
    /// 3+ errors recorded in facts but the narrative's User Corrections
    /// section is empty. Suggests the LLM missed real user correction
    /// events that should be captured next cycle.
    pub missing_corrections: bool,
}

impl NarrativeStaleness {
    pub fn any(&self) -> bool {
        self.task_contradicted || self.missing_corrections
    }
}

/// Compute staleness signals without building the full injection.
/// Zero-allocation on the happy path.
pub fn narrative_staleness(
    facts: &SessionFacts,
    narrative: Option<&SessionMemory>,
) -> NarrativeStaleness {
    let task_contradicted = narrative_task_contradicts_facts(facts);
    let missing_corrections = facts.error_state.total_errors >= 3
        && narrative
            .and_then(|n| n.section("User Corrections"))
            .map(|s| s.trim().is_empty())
            .unwrap_or(true);
    NarrativeStaleness {
        task_contradicted,
        missing_corrections,
    }
}

fn append_validated_narrative(
    out: &mut String,
    facts: &SessionFacts,
    narrative: Option<&SessionMemory>,
) {
    let skip_task = narrative_task_contradicts_facts(facts);

    // ── Layer 3: LLM Narrative (supplement, ≤500t) ──
    if let Some(n) = narrative {
        if !skip_task {
            if let Some(task) = n.section("Task Specification") {
                out.push_str("# Task\n");
                out.push_str(&truncate_to_token_budget(
                    redact_sensitive(task.trim()).as_str(),
                    200,
                ));
                out.push('\n');
            }
        }
        if let Some(corrections) = n.section("User Corrections") {
            let redacted = redact_sensitive(corrections.trim());
            let trimmed = redacted.trim();
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
                    out.push_str(&redact_sensitive(line.trim()));
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
                out.push_str(&redact_sensitive(recent.trim()));
                out.push('\n');
            }
        }
    }
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
    // NaN degrades safely to the most-informative level (fail-open): a
    // caller whose pressure computation div-by-zero'd should not silently
    // lose all context. +∞ falls through the `<` comparisons naturally
    // and correctly lands in L0Only. Negative values also fail-open —
    // they indicate a computation bug, not genuinely low pressure, but
    // L1Full is the safer default than L0Only when uncertain.
    if pressure.is_nan() || pressure < l1_full_max {
        InjectionLevel::L1Full
    } else if pressure < l1_minimal_max {
        InjectionLevel::L1Minimal
    } else {
        InjectionLevel::L0Only
    }
}

#[cfg(test)]
mod injection_level_threshold_tests {
    use super::*;

    #[test]
    fn below_l1_full_threshold_returns_full() {
        assert_eq!(injection_level_for_pressure(0.0), InjectionLevel::L1Full);
        assert_eq!(injection_level_for_pressure(0.5), InjectionLevel::L1Full);
        assert_eq!(injection_level_for_pressure(0.7499), InjectionLevel::L1Full);
    }

    #[test]
    fn at_l1_full_threshold_boundary_returns_minimal() {
        // Strict `<` comparison: 0.75 exactly is NOT L1Full.
        assert_eq!(
            injection_level_for_pressure(DEFAULT_L1_FULL_THRESHOLD),
            InjectionLevel::L1Minimal
        );
    }

    #[test]
    fn between_thresholds_returns_minimal() {
        assert_eq!(
            injection_level_for_pressure(0.80),
            InjectionLevel::L1Minimal
        );
        assert_eq!(
            injection_level_for_pressure(0.8499),
            InjectionLevel::L1Minimal
        );
    }

    #[test]
    fn at_l1_minimal_threshold_boundary_returns_l0() {
        // Strict `<` comparison: 0.85 exactly is NOT L1Minimal.
        assert_eq!(
            injection_level_for_pressure(DEFAULT_L1_MINIMAL_THRESHOLD),
            InjectionLevel::L0Only
        );
    }

    #[test]
    fn above_l1_minimal_threshold_returns_l0() {
        assert_eq!(injection_level_for_pressure(0.90), InjectionLevel::L0Only);
        assert_eq!(injection_level_for_pressure(1.50), InjectionLevel::L0Only);
    }

    #[test]
    fn non_finite_pressure_degrades_to_full_not_l0() {
        // A miscomputed pressure (NaN from div-by-zero, -∞ from bug) must
        // not silently strip all context — fail open to L1Full.
        assert_eq!(
            injection_level_for_pressure(f64::NAN),
            InjectionLevel::L1Full
        );
        assert_eq!(
            injection_level_for_pressure(f64::NEG_INFINITY),
            InjectionLevel::L1Full
        );
        assert_eq!(
            injection_level_for_pressure(f64::INFINITY),
            InjectionLevel::L0Only
        );
        assert_eq!(injection_level_for_pressure(-0.1), InjectionLevel::L1Full);
    }

    #[test]
    fn custom_thresholds_respected() {
        assert_eq!(
            injection_level_for_pressure_with_thresholds(0.50, 0.40, 0.60),
            InjectionLevel::L1Minimal
        );
        assert_eq!(
            injection_level_for_pressure_with_thresholds(0.30, 0.40, 0.60),
            InjectionLevel::L1Full
        );
        assert_eq!(
            injection_level_for_pressure_with_thresholds(0.70, 0.40, 0.60),
            InjectionLevel::L0Only
        );
    }
}

#[cfg(test)]
mod narrative_staleness_tests {
    use super::*;

    fn facts_with_errors(n: u32) -> SessionFacts {
        let mut f = SessionFacts::default();
        f.error_state.total_errors = n;
        f
    }

    #[test]
    fn no_errors_no_narrative_is_fresh() {
        let facts = SessionFacts::default();
        let s = narrative_staleness(&facts, None);
        assert!(!s.any());
        assert!(!s.missing_corrections);
    }

    #[test]
    fn three_errors_no_narrative_flags_missing_corrections() {
        // Regression guard for the "narrative=None" call site: when the
        // caller can't fetch L1, staleness must still trigger so the next
        // extraction runs. Two errors should NOT trigger (threshold is ≥3).
        assert!(!narrative_staleness(&facts_with_errors(2), None).missing_corrections);
        assert!(narrative_staleness(&facts_with_errors(3), None).missing_corrections);
        assert!(narrative_staleness(&facts_with_errors(10), None).missing_corrections);
    }

    #[test]
    fn three_errors_with_populated_corrections_is_fresh() {
        let facts = facts_with_errors(3);
        let narrative = SessionMemory::parse(
            "[session-memory:v1]\n# User Corrections\n- user said no use X\n\n# Task Specification\n- foo\n",
        )
        .expect("parse test narrative");
        let s = narrative_staleness(&facts, Some(&narrative));
        assert!(!s.missing_corrections);
    }

    #[test]
    fn three_errors_with_empty_corrections_section_flags_stale() {
        let facts = facts_with_errors(3);
        let narrative = SessionMemory::parse(
            "[session-memory:v1]\n# User Corrections\n\n# Task Specification\n- foo\n",
        )
        .expect("parse test narrative");
        assert!(narrative_staleness(&facts, Some(&narrative)).missing_corrections);
    }

    #[test]
    fn any_aggregates_both_signals() {
        let s = NarrativeStaleness {
            task_contradicted: true,
            missing_corrections: false,
        };
        assert!(s.any());
        let s = NarrativeStaleness {
            task_contradicted: false,
            missing_corrections: true,
        };
        assert!(s.any());
        let s = NarrativeStaleness::default();
        assert!(!s.any());
    }
}

// ── P3: Persist L1 to Memoria ────────────────────────────────────────────────

/// Outcome of one [`persist_l1`] call. The `PurgeFailed` variant is
/// distinct from `StoreFailed` because the caller's downstream
/// decisions differ: after a failed purge we abort *before* storing,
/// so no state change happened and the next turn can retry cleanly;
/// after a failed store we tried twice and need a different mitigation.
#[derive(Debug)]
pub enum PersistL1Error {
    /// Pre-store purge of the previous L1 exhausted retries. We did
    /// NOT attempt the new store — retrying would risk two L1 rows
    /// coexisting, making prefix-based retrieval non-deterministic.
    PurgeFailed(String),
    /// Purge succeeded (or there was nothing to purge) but the new
    /// store failed even after one retry.
    StoreFailed(String),
}

impl std::fmt::Display for PersistL1Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::PurgeFailed(m) => write!(f, "L1 purge failed: {m}"),
            Self::StoreFailed(m) => write!(f, "L1 store failed: {m}"),
        }
    }
}

/// Successful [`persist_l1`] return — carries attempt-count breadcrumbs
/// so callers can surface `attempt=1|2` to operational events without
/// reproducing the retry logic.
#[derive(Debug, Clone)]
pub struct PersistL1Success {
    pub memory_id: String,
    /// 1 on first-try success, 2 when the store had to retry.
    pub store_attempt: u32,
}

/// Purge old L1 for this session (with retry), then store the new one.
///
/// The write side of session memory MUST keep Memoria's prefix index
/// single-valued per session: two concurrent `SESSION_MEMORY_PREFIX`
/// rows would make retrieval non-deterministic. If purge fails after
/// retries we abort — leaving the stale row in place is safer than
/// racing a new one next to it.
pub async fn persist_l1(
    client: &dyn crate::turn::cloud::memoria_compact::MemoriaClient,
    l1_content: &str,
    session_id: &str,
) -> Result<PersistL1Success, PersistL1Error> {
    // Purge with one retry. A permanent failure here blocks the store.
    if let Err(first_err) = client.purge_working(session_id).await {
        tracing::warn!(
            session_id = %session_id,
            attempt = 1,
            error = %first_err,
            "L1 purge failed, retrying"
        );
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        if let Err(second_err) = client.purge_working(session_id).await {
            tracing::warn!(
                session_id = %session_id,
                attempt = 2,
                error = %second_err,
                "L1 purge failed, aborting store to avoid duplicate L1"
            );
            return Err(PersistL1Error::PurgeFailed(second_err));
        }
    }

    // Store with one retry.
    match client
        .store(l1_content, "working", Some(session_id), Some("T2"))
        .await
    {
        Ok(id) => Ok(PersistL1Success {
            memory_id: id,
            store_attempt: 1,
        }),
        Err(e) => {
            tracing::warn!(
                session_id = %session_id,
                attempt = 1,
                error = %e,
                "L1 store failed, retrying"
            );
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            client
                .store(l1_content, "working", Some(session_id), Some("T2"))
                .await
                .map(|id| PersistL1Success {
                    memory_id: id,
                    store_attempt: 2,
                })
                .map_err(|e2| {
                    tracing::warn!(
                        session_id = %session_id,
                        attempt = 2,
                        error = %e2,
                        "L1 store failed, giving up"
                    );
                    PersistL1Error::StoreFailed(e2)
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
        let rendered = anchor.to_string();
        assert!(rendered.starts_with("[session-anchor] "));
        assert!(rendered.contains("Add OAuth support"));
        assert!(rendered.contains("Currently: starting"));
        assert!(rendered.contains("0/0 steps"));
        assert!(matches!(
            anchor,
            Anchor::LegacyL1 {
                state: LegacyState::Starting,
                ..
            }
        ));
    }

    #[test]
    fn anchor_from_l1() {
        let l1 = SessionMemory::parse(sample_l1()).unwrap();
        let anchor = extract_anchor("ignored", Some(&l1));
        let rendered = anchor.to_string();
        assert!(rendered.starts_with("[session-anchor] "));
        assert!(rendered.contains("OAuth"));
        assert!(rendered.contains("token refresh"));
        assert!(rendered.contains("3/6 steps"));
        assert!(matches!(
            anchor,
            Anchor::LegacyL1 {
                state: LegacyState::Narrative {
                    done: 3,
                    total: 6,
                    ..
                },
                ..
            }
        ));
    }

    #[test]
    fn anchor_truncates_long_user_message() {
        let long_msg = (0..50)
            .map(|i| format!("word{i}"))
            .collect::<Vec<_>>()
            .join(" ");
        let anchor = extract_anchor(&long_msg, None);
        let words: Vec<&str> = anchor.task().split_whitespace().collect();
        assert!(words.len() <= MAX_TASK_WORDS);
    }

    // ── L0 Anchor from Facts Tests ──────────────────────────────────────

    #[test]
    fn facts_anchor_with_plan_state() {
        use astra_turn_types::session_facts::{PlanFact, SessionFacts};
        let mut facts = SessionFacts::default();
        facts.plan_state = Some(PlanFact {
            goal: "Implement OAuth".to_string(),
            completed: 3,
            total: 5,
            current_subtask: Some("token refresh".to_string()),
        });
        let anchor = extract_anchor_from_facts("Add OAuth support", &facts, None);
        let rendered = anchor.to_string();
        assert!(
            rendered.starts_with("[session-anchor] Goal:"),
            "anchor: {rendered}"
        );
        assert!(rendered.contains("OAuth"), "anchor: {rendered}");
        assert!(rendered.contains("3/5 subtasks"), "anchor: {rendered}");
        assert!(
            rendered.contains("current: token refresh"),
            "anchor: {rendered}"
        );
    }

    #[test]
    fn facts_anchor_with_active_file_no_plan() {
        use astra_turn_types::session_facts::{FileEntry, SessionFacts};
        let mut facts = SessionFacts::default();
        facts.active_files.push(FileEntry {
            path: "src/auth.rs".to_string(),
            last_action: "write".to_string(),
            turn: 7,
        });
        let anchor = extract_anchor_from_facts("Fix auth bug", &facts, None);
        assert!(anchor.to_string().contains("State: write src/auth.rs (t7)"));
    }

    #[test]
    fn facts_anchor_empty_facts_shows_starting() {
        use astra_turn_types::session_facts::SessionFacts;
        let facts = SessionFacts::default();
        let anchor = extract_anchor_from_facts("Build something", &facts, None);
        assert!(anchor.to_string().contains("State: starting"));
        assert!(matches!(
            anchor,
            Anchor::Facts {
                state: FactsState::Starting,
                ..
            }
        ));
    }

    #[test]
    fn facts_anchor_includes_last_error() {
        use astra_turn_types::session_facts::{ErrorFact, SessionFacts};
        let mut facts = SessionFacts::default();
        facts.error_state = ErrorFact {
            total_errors: 2,
            last_error: Some("sqlx migration column exists".to_string()),
            last_error_turn: Some(5),
        };
        let anchor = extract_anchor_from_facts("Fix DB", &facts, None);
        let rendered = anchor.to_string();
        assert!(rendered.contains("Last error:"), "anchor: {rendered}");
        assert!(rendered.contains("sqlx"), "anchor: {rendered}");
    }

    #[test]
    fn facts_anchor_includes_blocked_tools() {
        use astra_turn_types::session_facts::SessionFacts;
        let mut facts = SessionFacts::default();
        facts.blocked_tools = vec!["web_fetch".to_string(), "rm".to_string()];
        let anchor = extract_anchor_from_facts("Do stuff", &facts, None);
        assert!(anchor.to_string().contains("Avoid: web_fetch, rm"));
    }

    #[test]
    fn facts_anchor_prefers_narrative_task_spec() {
        use astra_turn_types::session_facts::SessionFacts;
        let facts = SessionFacts::default();
        let l1 = SessionMemory::parse(sample_l1()).unwrap();
        let anchor = extract_anchor_from_facts("raw user msg ignored", &facts, Some(&l1));
        let rendered = anchor.to_string();
        // Should use Task Specification from narrative, not the raw user msg
        assert!(rendered.contains("OAuth"));
        assert!(!rendered.contains("raw user msg"));
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
        use astra_turn_types::session_facts::{FileEntry, SessionFacts};
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
        let injection =
            build_facts_first_injection(&facts, Some(&narrative), InjectionLevel::L1Full);
        // System State must come before Task
        let facts_pos = injection.find("# System State").unwrap();
        let task_pos = injection.find("# Task").unwrap();
        assert!(facts_pos < task_pos, "facts must come before narrative");
    }

    #[test]
    fn injection_includes_narrative_sections() {
        use astra_turn_types::session_facts::SessionFacts;
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
        let injection =
            build_facts_first_injection(&facts, Some(&narrative), InjectionLevel::L1Full);
        assert!(injection.contains("# Task\nImplement OAuth"));
        assert!(injection.contains("# User Corrections\nUse RS256 not HS256"));
        assert!(injection.contains("# Learnings"));
        assert!(injection.contains("# Last Decision"));
        assert!(injection.contains("Use sqlx")); // last decision
    }

    #[test]
    fn injection_without_narrative() {
        use astra_turn_types::session_facts::{FileEntry, SessionFacts};
        let mut facts = SessionFacts::default();
        facts.turn = 3;
        facts.estimated_tokens = 10000;
        facts.active_files.push(FileEntry {
            path: "a.rs".to_string(),
            last_action: "read".to_string(),
            turn: 3,
        });
        let injection = build_facts_first_injection(&facts, None, InjectionLevel::L1Full);
        assert!(injection.contains("# System State"));
        assert!(injection.contains("Turn 3"));
        assert!(!injection.contains("# Task")); // no narrative
    }

    #[test]
    fn facts_first_injection_includes_attention_manifest_from_plan_facts() {
        use astra_turn_types::session_facts::{PlanFact, SessionFacts};
        let facts = SessionFacts {
            plan_state: Some(PlanFact {
                goal: "Implement runtime continuity".to_string(),
                completed: 1,
                total: 3,
                current_subtask: Some("wire attention into compaction".to_string()),
            }),
            ..Default::default()
        };

        let injection = build_facts_first_injection(&facts, None, InjectionLevel::L1Full);
        let attention_pos = injection.find("[attention:v1]").unwrap();
        let facts_pos = injection.find("# System State").unwrap();
        assert!(attention_pos < facts_pos);
        assert!(injection.contains("goal: Implement runtime continuity"));
        assert!(
            injection.contains(
                "current_todo: session-plan-current [in_progress]: wire attention into compaction"
            ),
            "{injection}"
        );
    }

    #[test]
    fn injection_cross_validation_skips_task_on_contradiction() {
        use astra_turn_types::session_facts::{ErrorFact, PlanFact, SessionFacts};
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
        let injection =
            build_facts_first_injection(&facts, Some(&narrative), InjectionLevel::L1Full);
        // Task should be SKIPPED due to contradiction
        assert!(
            !injection.contains("# Task"),
            "contradicted Task should be skipped"
        );
        assert!(
            !injection.contains("⚠️"),
            "contradicted narrative should be omitted without prompt warning noise"
        );
        // But User Corrections should still be present
        assert!(injection.contains("# User Corrections"));
        assert!(injection.contains("RS256"));
    }

    #[test]
    fn continuity_first_injection_puts_attention_before_facts_and_narrative() {
        use astra_turn_types::continuity::{
            ContinuityState, TodoItem, TodoState, TodoStatus, VerificationState, VerificationStatus,
        };
        use astra_turn_types::session_facts::{FileEntry, SessionFacts};

        let facts = SessionFacts {
            turn: 7,
            active_files: vec![FileEntry {
                path: "rust/crates/runtime/src/turn/agentic_loop_execution_phase.rs".to_string(),
                last_action: "write".to_string(),
                turn: 7,
            }],
            ..Default::default()
        };
        let mut verification = VerificationState::default();
        verification.set(VerificationStatus::Failed, "cargo test failed", 7);
        let continuity = ContinuityState {
            goal: astra_turn_types::continuity::GoalState {
                text: "Implement runtime-owned continuity".to_string(),
                source_turn: Some(1),
            },
            todos: TodoState {
                items: vec![TodoItem {
                    id: "attention-injection".to_string(),
                    title: "Inject attention manifest".to_string(),
                    description: "turn start injection".to_string(),
                    status: TodoStatus::InProgress,
                    evidence: vec![],
                    blocked_reason: None,
                }],
            },
            facts,
            user_corrections: Vec::new(),
            verification,
        };
        let narrative = narrative_with_sections(&[("Task Specification", "LLM narrative")]);

        let injection =
            build_continuity_first_injection(&continuity, Some(&narrative), InjectionLevel::L1Full);
        let attention_pos = injection.find("[attention:v1]").unwrap();
        let facts_pos = injection.find("# System State").unwrap();
        let task_pos = injection.find("# Task").unwrap();
        assert!(attention_pos < facts_pos);
        assert!(facts_pos < task_pos);
        assert!(injection.contains("current_todo: attention-injection [in_progress]"));
        assert!(injection.contains("- failed t7: cargo test failed"));
    }

    #[test]
    fn facts_first_narrative_sections_are_redacted() {
        use astra_turn_types::session_facts::SessionFacts;
        let facts = SessionFacts::default();
        let narrative = narrative_with_sections(&[
            ("Task Specification", "Use token=ghp_secret"),
            ("User Corrections", "password:hunter2"),
            ("Decisions", "- Use api_key=abc123"),
        ]);

        let injection =
            build_facts_first_injection(&facts, Some(&narrative), InjectionLevel::L1Full);
        assert!(injection.contains("token=[REDACTED]"));
        assert!(injection.contains("password:[REDACTED]"));
        assert!(injection.contains("api_key=[REDACTED]"));
        assert!(!injection.contains("hunter2"));
        assert!(!injection.contains("abc123"));
        assert!(!injection.contains("ghp_secret"));
    }

    #[test]
    fn injection_no_cross_validation_when_no_errors() {
        use astra_turn_types::session_facts::{PlanFact, SessionFacts};
        let mut facts = SessionFacts::default();
        facts.plan_state = Some(PlanFact {
            goal: "Build API".to_string(),
            completed: 3,
            total: 3,
            current_subtask: None,
        });
        // No errors — no contradiction
        let narrative = narrative_with_sections(&[("Task Specification", "Build API")]);
        let injection =
            build_facts_first_injection(&facts, Some(&narrative), InjectionLevel::L1Full);
        assert!(injection.contains("# Task\nBuild API")); // Task NOT skipped
        assert!(!injection.contains("⚠️"));
    }

    #[test]
    fn injection_learnings_last_three_only() {
        use astra_turn_types::session_facts::SessionFacts;
        let facts = SessionFacts::default();
        let narrative = narrative_with_sections(&[(
            "Learnings",
            "- first\n- second\n- third\n- fourth\n- fifth",
        )]);
        let injection =
            build_facts_first_injection(&facts, Some(&narrative), InjectionLevel::L1Full);
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
        let rendered = anchor.to_string();
        assert!(rendered.starts_with("[session-anchor] "));
        assert!(rendered.contains("Currently: starting"));
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

    // ── Pressure-adaptive injection contract (§4.8) ─────────────────────

    /// Helper producing a facts+narrative fixture that all three levels
    /// can exercise. Facts carry plan/files/errors; narrative carries
    /// Task Spec + Learnings + Decisions. The shape difference between
    /// levels should be clean and contract-level, not pattern-fragile.
    fn pressure_fixture() -> (SessionFacts, SessionMemory) {
        use astra_turn_types::session_facts::{FileEntry, PlanFact};
        let mut facts = SessionFacts {
            turn: 5,
            estimated_tokens: 40_000,
            active_files: vec![FileEntry {
                path: "src/auth.rs".to_string(),
                last_action: "write".to_string(),
                turn: 4,
            }],
            ..Default::default()
        };
        facts.set_plan_state(Some(PlanFact {
            goal: "Add OAuth".to_string(),
            completed: 3,
            total: 5,
            current_subtask: Some("token refresh".to_string()),
        }));
        let narrative_text = format!(
            "{SESSION_MEMORY_PREFIX}\n\
             # Task Specification\nAdd OAuth with JWT tokens to the API.\n\
             # Current State\n3/5 subtasks done.\n\
             # User Corrections\n- Use PKCE, not implicit flow.\n\
             # Learnings\n- JWT refresh rotation is stateful.\n\
             # Decisions\n- Picked reqwest over hyper.\n\
             # User Messages\nAdd OAuth please.\n",
        );
        let narrative =
            SessionMemory::parse(&narrative_text).expect("fixture narrative must parse");
        (facts, narrative)
    }

    #[test]
    fn full_level_includes_narrative() {
        let (facts, narrative) = pressure_fixture();
        let out = build_facts_first_injection(&facts, Some(&narrative), InjectionLevel::L1Full);
        assert!(
            out.contains("OAuth"),
            "L1Full must carry narrative Task Spec; got: {out}"
        );
        assert!(
            out.contains("Last Decision") || out.contains("reqwest"),
            "L1Full must carry the last Decision; got: {out}"
        );
        assert!(
            out.contains("Learnings"),
            "L1Full must carry Learnings section; got: {out}"
        );
    }

    #[test]
    fn minimal_level_drops_narrative_keeps_facts() {
        let (facts, narrative) = pressure_fixture();
        let out = build_facts_first_injection(&facts, Some(&narrative), InjectionLevel::L1Minimal);
        // Facts survive (plan progress is facts-derived).
        assert!(!out.is_empty(), "L1Minimal still emits facts; got empty");
        // Narrative-only content is gone.
        assert!(
            !out.contains("reqwest"),
            "L1Minimal must drop narrative Decisions; got: {out}"
        );
        assert!(
            !out.contains("JWT refresh rotation"),
            "L1Minimal must drop narrative Learnings; got: {out}"
        );
        assert!(
            !out.contains("PKCE"),
            "L1Minimal must drop narrative User Corrections; got: {out}"
        );
        // But the facts-level Task (from plan.goal) may still appear
        // via the attention manifest; that's fine — it's ground truth,
        // not narrative.
    }

    #[test]
    fn l0_only_returns_empty() {
        let (facts, narrative) = pressure_fixture();
        let out = build_facts_first_injection(&facts, Some(&narrative), InjectionLevel::L0Only);
        assert_eq!(
            out, "",
            "L0Only defers to the L0 anchor in the dynamic system prompt; injection must be empty"
        );
    }

    // ── Cross-validation staleness signals (§4.4) ───────────────────────

    #[test]
    fn staleness_clean_when_facts_and_narrative_agree() {
        let (facts, narrative) = pressure_fixture();
        let s = narrative_staleness(&facts, Some(&narrative));
        assert!(!s.task_contradicted);
        assert!(!s.missing_corrections);
        assert!(!s.any());
    }

    #[test]
    fn staleness_detects_plan_done_with_errors() {
        use astra_turn_types::session_facts::{ErrorFact, PlanFact};
        let mut facts = SessionFacts {
            turn: 5,
            ..Default::default()
        };
        facts.set_plan_state(Some(PlanFact {
            goal: "done goal".to_string(),
            completed: 5,
            total: 5,
            current_subtask: None,
        }));
        facts.error_state = ErrorFact {
            total_errors: 1,
            last_error: Some("panic in auth".to_string()),
            last_error_turn: Some(5),
        };
        let s = narrative_staleness(&facts, None);
        assert!(
            s.task_contradicted,
            "plan complete + unresolved errors must flag as contradicted"
        );
    }

    #[test]
    fn staleness_detects_missing_corrections_under_error_pressure() {
        use astra_turn_types::session_facts::ErrorFact;
        let facts = SessionFacts {
            turn: 10,
            error_state: ErrorFact {
                total_errors: 4,
                last_error: Some("last one".to_string()),
                last_error_turn: Some(10),
            },
            ..Default::default()
        };
        // No narrative → corrections section is effectively empty.
        let s = narrative_staleness(&facts, None);
        assert!(
            s.missing_corrections,
            "≥3 errors + empty corrections must flag for re-extraction"
        );
    }

    #[test]
    fn staleness_quiet_when_corrections_recorded() {
        use astra_turn_types::session_facts::ErrorFact;
        let facts = SessionFacts {
            turn: 10,
            error_state: ErrorFact {
                total_errors: 4,
                last_error: Some("last one".to_string()),
                last_error_turn: Some(10),
            },
            ..Default::default()
        };
        let narrative_text = format!(
            "{SESSION_MEMORY_PREFIX}\n\
             # Task Specification\nWork on auth.\n\
             # Current State\nstuck on cookie handling.\n\
             # User Corrections\n- Use secure cookies only.\n\
             # User Messages\nPlease fix cookies.\n",
        );
        let narrative = SessionMemory::parse(&narrative_text).unwrap();
        let s = narrative_staleness(&facts, Some(&narrative));
        assert!(
            !s.missing_corrections,
            "non-empty corrections must suppress the re-extraction flag"
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
            let result = persist_l1(&*mock, "L1 content", "sess-1").await.unwrap();
            assert_eq!(
                result.store_attempt, 1,
                "first-try success must report attempt=1"
            );
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
            let result = persist_l1(&*mock, "L1 retry", "sess-2").await.unwrap();
            assert_eq!(
                result.store_attempt, 2,
                "retry success must report attempt=2 so events can carry it"
            );
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
        async fn persist_l1_aborts_store_when_purge_exhausts_retries() {
            // Permanent purge failure must abort the store: leaving a
            // stale L1 alongside a new one would make prefix-based
            // retrieval non-deterministic.
            use std::sync::atomic::{AtomicUsize, Ordering};
            struct PermanentPurgeFailMock {
                purge_attempts: AtomicUsize,
                store_attempts: AtomicUsize,
            }
            #[async_trait::async_trait]
            impl MemoriaClient for PermanentPurgeFailMock {
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
                    self.store_attempts.fetch_add(1, Ordering::Relaxed);
                    Ok("would-be-duplicate".into())
                }
                async fn purge_working(&self, _: &str) -> Result<u64, String> {
                    self.purge_attempts.fetch_add(1, Ordering::Relaxed);
                    Err("purge broken".into())
                }
                async fn delete(&self, _: &str) -> Result<(), String> {
                    Ok(())
                }
            }
            let mock = PermanentPurgeFailMock {
                purge_attempts: AtomicUsize::new(0),
                store_attempts: AtomicUsize::new(0),
            };
            let result = persist_l1(&mock, "L1", "s").await;
            assert!(
                matches!(result, Err(PersistL1Error::PurgeFailed(_))),
                "permanent purge failure must return PurgeFailed, got {result:?}"
            );
            assert_eq!(
                mock.purge_attempts.load(Ordering::Relaxed),
                2,
                "purge should be retried once"
            );
            assert_eq!(
                mock.store_attempts.load(Ordering::Relaxed),
                0,
                "store MUST NOT be attempted after exhausted purge retries"
            );
        }

        #[tokio::test]
        async fn persist_l1_retries_purge_once_then_proceeds() {
            // Transient purge failure recovers on retry; store then runs.
            use std::sync::atomic::{AtomicUsize, Ordering};
            struct TransientPurgeMock {
                purge_attempts: AtomicUsize,
            }
            #[async_trait::async_trait]
            impl MemoriaClient for TransientPurgeMock {
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
                    Ok("mem-1".into())
                }
                async fn purge_working(&self, _: &str) -> Result<u64, String> {
                    let n = self.purge_attempts.fetch_add(1, Ordering::Relaxed);
                    if n == 0 {
                        Err("transient".into())
                    } else {
                        Ok(1)
                    }
                }
                async fn delete(&self, _: &str) -> Result<(), String> {
                    Ok(())
                }
            }
            let mock = TransientPurgeMock {
                purge_attempts: AtomicUsize::new(0),
            };
            let result = persist_l1(&mock, "L1", "s").await.unwrap();
            assert_eq!(result.memory_id, "mem-1");
            assert_eq!(result.store_attempt, 1);
            assert_eq!(mock.purge_attempts.load(Ordering::Relaxed), 2);
        }
    }

    // ── Anchor::is_trivial ──────────────────────────────────────────────
    //
    // Triviality is now a structural property of the `Anchor` enum rather
    // than a string-parsed check on the rendered form. Each test either
    // constructs an `Anchor` through the public constructors
    // (`extract_anchor` / `extract_anchor_from_facts`) or — for explicit
    // shape coverage — via a small `mk_*` helper below.

    #[test]
    fn trivial_anchor_turn_one_hi() {
        // User: "hi" on turn 1 with no facts → legacy Starting shape whose
        // task is a truncation of the current message → trivial.
        let anchor = extract_anchor("hi", None);
        assert!(
            anchor.is_trivial("hi"),
            "bootstrap anchor echoing the user msg must be flagged trivial, got: {anchor}"
        );
    }

    #[test]
    fn trivial_anchor_turn_one_short_task() {
        let anchor = extract_anchor("refactor the prompt builder", None);
        assert!(anchor.is_trivial("refactor the prompt builder"));
    }

    #[test]
    fn non_trivial_anchor_when_l1_present() {
        // L1 narrative → anchor state is Narrative, not Starting.
        let l1 = SessionMemory::parse(sample_l1()).expect("sample_l1 parses");
        let anchor = extract_anchor("fix OAuth", Some(&l1));
        assert!(
            !anchor.is_trivial("fix OAuth"),
            "anchor enriched by L1 must not be flagged trivial, got: {anchor}"
        );
    }

    #[test]
    fn non_trivial_anchor_when_user_drifted() {
        // Same bootstrap shape, but current user msg diverges from the
        // anchored task — anchor restores "what we were actually doing".
        let anchor = extract_anchor("refactor the prompt builder", None);
        assert!(
            !anchor.is_trivial("wait, let's talk about logging"),
            "when user drifts, anchor should be kept, got: {anchor}"
        );
    }

    #[test]
    fn non_trivial_anchor_when_progress_nonzero() {
        // Narrative state with real progress is structurally non-trivial.
        let anchor = Anchor::LegacyL1 {
            task: "fix bug".into(),
            state: LegacyState::Narrative {
                current: "patching module".into(),
                done: 2,
                total: 3,
            },
        };
        assert!(!anchor.is_trivial("fix bug"));
    }

    #[test]
    fn trivial_anchor_case_insensitive_match() {
        let anchor = extract_anchor("Fix Timeout Bug", None);
        assert!(anchor.is_trivial("fix timeout bug"));
    }

    #[test]
    fn trivial_anchor_long_user_message_matching_prefix() {
        // First message longer than MAX_TASK_WORDS → anchor truncates.
        // Current user msg is still the same string → starts-with match
        // still holds, so trivial.
        let long = "refactor the prompt builder to use the volatile lane and drop the ancient typed field which has been unused for months";
        let anchor = extract_anchor(long, None);
        assert!(
            anchor.is_trivial(long),
            "same long message echoes anchor prefix → trivial, got: {anchor}"
        );
    }

    #[test]
    fn non_trivial_anchor_empty_user_msg() {
        // Defensive: empty current message → treat as non-trivial (keep
        // anchor so the LLM still sees context).
        let anchor = extract_anchor("refactor prompt builder", None);
        assert!(!anchor.is_trivial(""));
    }

    // ── Facts-shape triviality ──────────────────────────────────────────
    // Structural regression coverage for the `69657ca7` bug: before the
    // `Anchor` refactor, `is_trivial_anchor` parsed rendered strings and
    // recognized only the legacy shape. The facts-based turn-1 anchor
    // slipped through and bloated the volatile lane. The new enum-of-
    // variants `Anchor` carries per-shape state, and `is_trivial` matches
    // on each shape's own `Starting` sentinel — so the bug is structurally
    // impossible to reintroduce.

    #[test]
    fn trivial_anchor_facts_shape_turn_one_hi() {
        use astra_turn_types::session_facts::SessionFacts;
        let anchor = extract_anchor_from_facts("hi", &SessionFacts::default(), None);
        assert_eq!(
            anchor.to_string(),
            "[session-anchor] Goal: hi. State: starting."
        );
        assert!(
            anchor.is_trivial("hi"),
            "facts-shape bootstrap anchor must be flagged trivial, got: {anchor}"
        );
    }

    #[test]
    fn trivial_anchor_facts_shape_short_task() {
        use astra_turn_types::session_facts::SessionFacts;
        let anchor = extract_anchor_from_facts(
            "refactor the prompt builder",
            &SessionFacts::default(),
            None,
        );
        assert!(anchor.is_trivial("refactor the prompt builder"));
    }

    #[test]
    fn non_trivial_anchor_facts_shape_when_state_not_starting() {
        use astra_turn_types::session_facts::{PlanFact, SessionFacts};
        let mut facts = SessionFacts::default();
        facts.plan_state = Some(PlanFact {
            goal: "Fix bug".into(),
            completed: 2,
            total: 5,
            current_subtask: Some("patch".into()),
        });
        let anchor = extract_anchor_from_facts("fix bug", &facts, None);
        assert!(
            !anchor.is_trivial("fix bug"),
            "facts-based anchor with real state must NOT be trivial, got: {anchor}"
        );
    }

    #[test]
    fn non_trivial_anchor_facts_shape_when_constraints_appended() {
        // last_error attached → non-trivial even with Starting state.
        use astra_turn_types::session_facts::{ErrorFact, SessionFacts};
        let mut facts = SessionFacts::default();
        facts.error_state = ErrorFact {
            total_errors: 1,
            last_error: Some("timeout".into()),
            last_error_turn: Some(1),
        };
        let anchor = extract_anchor_from_facts("hi", &facts, None);
        assert!(
            !anchor.is_trivial("hi"),
            "anchor carrying Last error must NOT be trivial, got: {anchor}"
        );
    }

    #[test]
    fn non_trivial_anchor_facts_shape_when_user_drifted() {
        use astra_turn_types::session_facts::SessionFacts;
        let anchor = extract_anchor_from_facts(
            "refactor the prompt builder",
            &SessionFacts::default(),
            None,
        );
        assert!(
            !anchor.is_trivial("wait, let's talk about logging"),
            "facts-shape anchor must re-anchor after user drift, got: {anchor}"
        );
    }

    // ── pick_latest_l1 ──────────────────────────────────────────────────

    mod pick_latest_l1_tests {
        use super::super::pick_latest_l1;
        use crate::turn::cloud::memoria_compact::MemoriaMemory;

        fn mem(id: &str, content: &str, score: Option<f64>) -> MemoriaMemory {
            MemoriaMemory {
                memory_id: id.to_string(),
                content: content.to_string(),
                memory_type: "working".to_string(),
                retrieval_score: score,
            }
        }

        #[test]
        fn empty_returns_none() {
            assert!(pick_latest_l1(&[]).is_none());
        }

        #[test]
        fn no_prefix_match_returns_none() {
            let list = vec![
                mem("a", "random note", Some(0.9)),
                mem("b", "[other-prefix] text", Some(0.8)),
            ];
            assert!(pick_latest_l1(&list).is_none());
        }

        #[test]
        fn single_match_is_returned() {
            let list = vec![
                mem("a", "random note", Some(0.9)),
                mem("b", "[session-memory:v1]\nfoo", Some(0.5)),
            ];
            let picked = pick_latest_l1(&list).unwrap();
            assert_eq!(picked.memory_id, "b");
        }

        #[test]
        fn multiple_matches_pick_highest_score() {
            // Critical unhappy path: two L1 rows exist because a prior
            // purge failure left a stale one in place. Readers MUST
            // converge on the same "latest" regardless of input order.
            let list = vec![
                mem("old", "[session-memory:v1]\nold content", Some(0.4)),
                mem("new", "[session-memory:v1]\nnew content", Some(0.9)),
            ];
            let picked = pick_latest_l1(&list).unwrap();
            assert_eq!(picked.memory_id, "new");

            // Reversed input → same answer.
            let reversed = vec![list[1].clone(), list[0].clone()];
            let picked_rev = pick_latest_l1(&reversed).unwrap();
            assert_eq!(
                picked_rev.memory_id, "new",
                "pick_latest_l1 must be deterministic regardless of input order"
            );
        }

        #[test]
        fn missing_scores_use_neg_infinity_so_scored_wins() {
            let list = vec![
                mem("scored", "[session-memory:v1]\nA", Some(0.1)),
                mem("unscored", "[session-memory:v1]\nB", None),
            ];
            assert_eq!(pick_latest_l1(&list).unwrap().memory_id, "scored");
        }

        #[test]
        fn all_unscored_returns_first_match() {
            let list = vec![
                mem("first", "[session-memory:v1]\nA", None),
                mem("second", "[session-memory:v1]\nB", None),
            ];
            // With all scores absent, both compare equal and `first`
            // wins by iteration order. Documented in the fn doc.
            assert_eq!(pick_latest_l1(&list).unwrap().memory_id, "first");
        }
    }
}
