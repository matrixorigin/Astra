//! Unified Routing Decision — merges 3 intent systems into ONE analysis pass.
//!
//! Previously, intent was analyzed by three separate systems:
//! 1. `ConversationState` — boolean signal extraction (is_fetch, is_github, etc.)
//! 2. `classify_task()` — simple "code"/"reasoning"/None hinter
//! 3. `IntentDisambiguation` — conflict detection between signals
//!
//! `RoutingDecision` replaces these with a single analysis that produces:
//! - Typed task classification (7 types vs. 3)
//! - Memory-augmented domain detection
//! - Unified confidence score (signals + task clarity + memory hints)
//! - Tool filter strategy (Wide / Domain / Minimal)
//! - Round budget estimation from task complexity
//!
//! # Usage
//!
//! ```rust,ignore
//! let decision = RoutingEngine::analyze(
//!     "我关注matrixorigin",
//!     1,
//!     &[],
//!     &["matrixorigin = GitHub org"],
//! );
//! assert_eq!(decision.task_type, TaskType::Memory);
//! assert!(decision.confidence > 0.3);
//! ```

use crate::tool_registry::state::ConversationState;
use crate::turn::routing_metrics::{DisambiguationAction, IntentDisambiguation};

// ─── Task Type ───────────────────────────────────────────────────────────────

/// Enriched task classification — 7 types vs. the old 3 (code/reasoning/None).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum TaskType {
    /// Code editing, generation, or file manipulation.
    Code,
    /// Analysis, explanation, comparison.
    Reasoning,
    /// Read-only data retrieval (list PRs, show status, etc.)
    Fetch,
    /// Create/update/delete operations.
    Mutate,
    /// Store or retrieve user preferences (关注/跟踪/bookmark).
    Memory,
    /// Greeting, chit-chat, simple questions.
    Conversational,
    /// Multiple task types combined (e.g., "show me PRs and fix the failing one").
    Compound,
    /// Cannot determine task type.
    Unknown,
}

// ─── Domain Hint ─────────────────────────────────────────────────────────────

/// Domain extracted from signals + memory hints.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum DomainHint {
    GitHub,
    Git,
    Code,
    Memory,
    Web,
    System,
    Database,
}

// ─── Tool Filter ─────────────────────────────────────────────────────────────

/// Recommended tool selection strategy based on routing analysis.
#[derive(Debug, Clone, PartialEq)]
pub enum ToolFilter {
    /// Low confidence → include all tools, let LLM decide.
    Wide,
    /// Domain-focused → filter to specific tool categories.
    Domain(Vec<String>),
    /// Conversational → minimal tools (only pinned).
    Minimal,
}

// ─── RoutingDecision ─────────────────────────────────────────────────────────

/// Unified routing decision — single analysis pass, shared by all consumers.
///
/// Embeds `ConversationState` for backward compatibility with scoring.rs
/// while adding richer classification, memory integration, and strategy hints.
#[derive(Debug, Clone)]
pub struct RoutingDecision {
    /// Legacy signal extraction (backward-compatible with scoring.rs).
    pub conversation_state: ConversationState,

    /// Enriched task classification.
    pub task_type: TaskType,

    /// Memory-derived hints (e.g., "matrixorigin = GitHub org").
    pub memory_hints: Vec<String>,

    /// Domain detected from signals + memory.
    pub domain_hint: Option<DomainHint>,

    /// Memory-derived boost terms for TF-IDF scoring.
    pub boost_terms: Vec<String>,

    /// Unified confidence (0.0 = completely uncertain, 1.0 = very confident).
    pub confidence: f64,

    /// Recommended tool selection strategy.
    pub tool_filter: ToolFilter,

    /// Estimated round budget based on task complexity.
    pub estimated_rounds: u32,

    /// Disambiguation result (from embedded IntentDisambiguation).
    pub disambiguation: IntentDisambiguation,
}

// ─── Preference Detection ────────────────────────────────────────────────────

/// Chinese preference/interest verbs.
const ZH_PREFERENCE: &[&str] = &["关注", "跟踪", "留意", "记住", "记一下", "收藏"];

/// English preference/interest patterns.
const EN_PREFERENCE: &[&str] = &[
    "follow",
    "track",
    "remember",
    "bookmark",
    "interested in",
    "keep an eye on",
    "subscribe",
];

/// Detect preference/tracking intent in a query.
///
/// Returns true only when the preference verb is the PRIMARY action,
/// not a secondary usage (e.g., "watch the test output" is NOT preference).
fn is_preference_intent(query: &str, signals: &ConversationState) -> bool {
    let lower = query.to_lowercase();

    let has_zh = ZH_PREFERENCE.iter().any(|kw| lower.contains(kw));
    let has_en = EN_PREFERENCE.iter().any(|kw| lower.contains(kw));

    if !has_zh && !has_en {
        return false;
    }

    // If there are strong action signals (fetch/mutate/analytical), preference
    // is likely secondary. Only return true when preference is the primary intent.
    // Exception: "关注" with a proper noun (entity name) is almost always preference.
    if has_zh {
        // Chinese preference verbs are less ambiguous — usually primary intent
        return true;
    }

    // English: "follow", "watch" etc. are ambiguous. Only treat as preference
    // when no other strong action signals fire.
    let action_signals = [signals.is_fetch, signals.is_mutate, signals.is_analytical]
        .iter()
        .filter(|&&x| x)
        .count();

    action_signals == 0
}

// ─── Code Detection ──────────────────────────────────────────────────────────

/// Detect code-related intent (enriched from old classify_task).
fn is_code_intent(query: &str) -> bool {
    let lower = query.to_lowercase();
    if lower.contains("```") {
        return true;
    }
    // File extension patterns
    let extensions = [
        ".py", ".go", ".ts", ".js", ".rs", ".java", ".cpp", ".rb", ".c", ".h",
    ];
    if extensions.iter().any(|ext| lower.contains(ext)) {
        return true;
    }
    // Code-specific verbs
    let code_verbs = [
        "compile",
        "build",
        "debug",
        "refactor",
        "implement",
        "编译",
        "调试",
        "重构",
    ];
    code_verbs.iter().any(|v| lower.contains(v))
}

// ─── Task Classification ─────────────────────────────────────────────────────

/// Classify the task type from query + signals + memory.
fn classify_task_type(
    query: &str,
    signals: &ConversationState,
    memory_hints: &[String],
) -> TaskType {
    // Priority 1: Preference/tracking intent
    if is_preference_intent(query, signals) {
        return TaskType::Memory;
    }

    // Priority 2: Memory query intent (asking about stored memories)
    if signals.is_memory && (signals.is_fetch || signals.is_followup) {
        return TaskType::Memory;
    }

    // Priority 3: Code intent
    if is_code_intent(query) {
        return TaskType::Code;
    }

    // Priority 4: Compound intent (fetch + mutate)
    if signals.is_fetch && signals.is_mutate {
        return TaskType::Compound;
    }

    // Priority 5: Single-signal mapping
    if signals.is_mutate {
        return TaskType::Mutate;
    }
    if signals.is_fetch {
        return TaskType::Fetch;
    }
    if signals.is_analytical {
        return TaskType::Reasoning;
    }
    if signals.is_conversational && signals.signal_count() == 0 {
        return TaskType::Conversational;
    }

    // Priority 6: Follow-up inherits domain from recent tools → Fetch
    // "pr呢？" after github_ci_status → Fetch (not Unknown)
    if signals.is_followup && !signals.recent_tools.is_empty() {
        let domain = infer_domain_from_tools(&signals.recent_tools);
        if domain.is_some() {
            return TaskType::Fetch;
        }
    }

    // Priority 7: Memory hints suggest domain
    if !memory_hints.is_empty() {
        let hint_text = memory_hints.join(" ").to_lowercase();
        if hint_text.contains("github") || hint_text.contains("repository") {
            return TaskType::Fetch;
        }
    }

    // Priority 8: Reasoning keywords (from old classify_task)
    let lower = query.to_lowercase();
    let reasoning_kw = [
        "explain",
        "analyze",
        "reason",
        "compare",
        "why",
        "为什么",
        "分析",
    ];
    if reasoning_kw.iter().any(|k| lower.contains(k)) {
        return TaskType::Reasoning;
    }

    TaskType::Unknown
}

// ─── Domain Extraction ───────────────────────────────────────────────────────

/// Infer domain from recently-used tool names.
///
/// This enables context carry-forward: if the previous turn used `github_ci_status`,
/// a follow-up like "pr呢？" inherits the GitHub domain even though "pr" alone is
/// a weak signal.
fn infer_domain_from_tools(tools: &[String]) -> Option<DomainHint> {
    let github_prefixes = ["github_", "github"];
    let git_tools = [
        "git_status",
        "git_diff",
        "git_log",
        "git_blame",
        "git_file_history",
        "git_contributors",
        "git_log_search",
    ];
    let memory_tools = [
        "memory_store",
        "memory_search",
        "memory_profile",
        "memory_correct",
        "memory_purge",
    ];
    let db_tools = ["mo_query", "mo_snapshot", "mo_branch"];

    let has_github = tools
        .iter()
        .any(|t| github_prefixes.iter().any(|p| t.starts_with(p)));
    let has_git = tools.iter().any(|t| git_tools.contains(&t.as_str()));
    let has_memory = tools.iter().any(|t| memory_tools.contains(&t.as_str()));
    let has_db = tools.iter().any(|t| db_tools.contains(&t.as_str()));

    if has_github {
        return Some(DomainHint::GitHub);
    }
    if has_git {
        return Some(DomainHint::Git);
    }
    if has_memory {
        return Some(DomainHint::Memory);
    }
    if has_db {
        return Some(DomainHint::Database);
    }
    None
}

/// Extract domain hint from signals + memory hints + follow-up context.
fn extract_domain_hint(signals: &ConversationState, memory_hints: &[String]) -> Option<DomainHint> {
    // Signals take priority
    if signals.is_github {
        return Some(DomainHint::GitHub);
    }
    if signals.is_git {
        return Some(DomainHint::Git);
    }
    if signals.is_memory {
        return Some(DomainHint::Memory);
    }

    // Follow-up: infer domain from recent tools (context carry-forward)
    if signals.is_followup && !signals.recent_tools.is_empty() {
        let domain = infer_domain_from_tools(&signals.recent_tools);
        if domain.is_some() {
            return domain;
        }
    }

    // Memory hints
    if !memory_hints.is_empty() {
        let text = memory_hints.join(" ").to_lowercase();
        if text.contains("github") || text.contains("repository") || text.contains("pull request") {
            return Some(DomainHint::GitHub);
        }
        if text.contains("git") && !text.contains("github") {
            return Some(DomainHint::Git);
        }
    }

    None
}

// ─── Confidence Computation ──────────────────────────────────────────────────

/// Compute unified routing confidence from all available signals.
fn compute_routing_confidence(
    signals: &ConversationState,
    task_type: &TaskType,
    memory_hint_count: usize,
    disambiguation: &IntentDisambiguation,
) -> f64 {
    let mut conf: f64 = 0.0;

    // Signal strength (max 0.5)
    conf += match signals.signal_count() {
        0 => 0.0,
        1 => 0.2,
        2 => 0.4,
        _ => 0.5,
    };

    // Task type clarity (max 0.3)
    conf += match task_type {
        TaskType::Unknown => 0.0,
        TaskType::Compound => 0.1,
        _ => 0.3,
    };

    // Memory context bonus (max 0.2)
    if memory_hint_count > 0 {
        conf += 0.2_f64.min(memory_hint_count as f64 * 0.1);
    }

    // Disambiguation penalty
    if disambiguation.conflict_score > 0.5 {
        conf -= 0.1;
    }

    conf.clamp(0.0, 1.0)
}

// ─── Tool Filter ─────────────────────────────────────────────────────────────

/// Determine tool selection strategy from routing analysis.
fn determine_tool_filter(
    signals: &ConversationState,
    disambiguation: &DisambiguationAction,
    domain_hint: &Option<DomainHint>,
    confidence: f64,
) -> ToolFilter {
    // Low confidence → wide selection
    if confidence < 0.3 {
        return ToolFilter::Wide;
    }

    // Strong conflict → wide
    if *disambiguation == DisambiguationAction::WidenToolSelection {
        return ToolFilter::Wide;
    }

    // Conversational with no action signals → minimal
    if signals.is_conversational && signals.signal_count() == 0 {
        return ToolFilter::Minimal;
    }

    // Domain hint → focused selection
    if let Some(domain) = domain_hint {
        let categories = match domain {
            DomainHint::GitHub => vec!["github".into(), "git".into()],
            DomainHint::Git => vec!["git".into()],
            DomainHint::Code => vec!["code".into(), "file".into()],
            DomainHint::Memory => vec!["memory".into()],
            DomainHint::Web => vec!["web".into()],
            DomainHint::System => vec!["system".into()],
            DomainHint::Database => vec!["database".into(), "sql".into(), "matrixone".into()],
        };
        return ToolFilter::Domain(categories);
    }

    // Signal-based domains
    let mut domains = Vec::new();
    if signals.is_github {
        domains.push("github".into());
    }
    if signals.is_git {
        domains.push("git".into());
    }

    if domains.is_empty() {
        ToolFilter::Wide
    } else {
        ToolFilter::Domain(domains)
    }
}

// ─── Round Estimation ────────────────────────────────────────────────────────

/// Estimate round budget from task complexity.
fn estimate_rounds(task_type: &TaskType, confidence: f64) -> u32 {
    let base = match task_type {
        TaskType::Conversational => 1,
        TaskType::Memory => 2,
        TaskType::Fetch => 3,
        TaskType::Reasoning => 5,
        TaskType::Mutate => 5,
        TaskType::Code => 8,
        TaskType::Compound => 8,
        TaskType::Unknown => 5,
    };
    // Low confidence → more exploratory rounds
    if confidence < 0.3 {
        ((base as f64) * 1.5).ceil() as u32
    } else {
        base
    }
}

// ─── Routing Engine ──────────────────────────────────────────────────────────

/// One function to rule them all.
///
/// Replaces:
/// - `ConversationState::from_message_with_context()`
/// - `classify_task()`
/// - `disambiguate_intents()`
///
/// With a single analysis pass that shares data between steps.
pub struct RoutingEngine;

impl RoutingEngine {
    /// Analyze a user query and produce a unified routing decision.
    ///
    /// # Arguments
    /// - `query`: The user's message
    /// - `turn_count`: Current conversation turn
    /// - `recent_tools`: Tools used in recent turns
    /// - `memory_hints`: Domain hints from memory service (e.g., "matrixorigin = GitHub org")
    /// - `boost_terms`: Pre-extracted boost terms from history + memory
    pub fn analyze(
        query: &str,
        turn_count: u32,
        recent_tools: &[String],
        memory_hints: &[String],
        boost_terms: Vec<String>,
    ) -> RoutingDecision {
        // 1. Build ConversationState (reuses existing signal extraction)
        let conversation_state =
            ConversationState::from_message_with_context(query, turn_count, recent_tools);

        // 2. Classify task type (enriched: 7 types vs. old 3)
        let task_type = classify_task_type(query, &conversation_state, memory_hints);

        // 3. Get disambiguation (already computed inside ConversationState)
        let disambiguation = conversation_state
            .disambiguation
            .clone()
            .unwrap_or_else(|| IntentDisambiguation {
                primary_intent: "unknown".into(),
                secondary_intent: None,
                conflict_score: 0.0,
                recommendation: DisambiguationAction::Proceed,
            });

        // 4. Extract domain hint from signals + memory
        let domain_hint = extract_domain_hint(&conversation_state, memory_hints);

        // 5. Compute unified confidence
        let confidence = compute_routing_confidence(
            &conversation_state,
            &task_type,
            memory_hints.len(),
            &disambiguation,
        );

        // 6. Determine tool filter
        let tool_filter = determine_tool_filter(
            &conversation_state,
            &disambiguation.recommendation,
            &domain_hint,
            confidence,
        );

        // 7. Estimate rounds
        let estimated_rounds = estimate_rounds(&task_type, confidence);

        RoutingDecision {
            conversation_state,
            task_type,
            memory_hints: memory_hints.to_vec(),
            domain_hint,
            boost_terms,
            confidence,
            tool_filter,
            estimated_rounds,
            disambiguation,
        }
    }
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn analyze(query: &str) -> RoutingDecision {
        RoutingEngine::analyze(query, 1, &[], &[], vec![])
    }

    fn analyze_with_recent(query: &str, turn_count: u32, recent_tools: &[&str]) -> RoutingDecision {
        let recent_tools: Vec<String> = recent_tools.iter().map(|s| s.to_string()).collect();
        RoutingEngine::analyze(query, turn_count, &recent_tools, &[], vec![])
    }

    fn analyze_with_memory(query: &str, hints: &[&str]) -> RoutingDecision {
        let hints: Vec<String> = hints.iter().map(|s| s.to_string()).collect();
        RoutingEngine::analyze(query, 1, &[], &hints, vec![])
    }

    // ── Task Type Classification ─────────────────────────────────────────

    #[test]
    fn classify_preference_zh() {
        let d = analyze("我关注matrixorigin");
        assert_eq!(d.task_type, TaskType::Memory);
    }

    #[test]
    fn classify_preference_zh_track() {
        let d = analyze("帮我跟踪这个项目");
        assert_eq!(d.task_type, TaskType::Memory);
    }

    #[test]
    fn classify_preference_en() {
        let d = analyze("I want to follow the matrixorigin project");
        assert_eq!(d.task_type, TaskType::Memory);
    }

    #[test]
    fn classify_fetch() {
        let d = analyze("show me the latest PRs");
        assert_eq!(d.task_type, TaskType::Fetch);
    }

    #[test]
    fn classify_mutate() {
        let d = analyze("create a new branch for the fix");
        assert_eq!(d.task_type, TaskType::Mutate);
    }

    #[test]
    fn classify_code() {
        let d = analyze("implement the parser in main.rs");
        assert_eq!(d.task_type, TaskType::Code);
    }

    #[test]
    fn classify_code_backticks() {
        let d = analyze("fix this code: ```python\nprint('hello')```");
        assert_eq!(d.task_type, TaskType::Code);
    }

    #[test]
    fn classify_reasoning() {
        let d = analyze("explain why the test fails");
        assert_eq!(d.task_type, TaskType::Reasoning);
    }

    #[test]
    fn classify_conversational() {
        let d = analyze("hello");
        assert_eq!(d.task_type, TaskType::Conversational);
    }

    #[test]
    fn classify_compound_fetch_mutate() {
        let d = analyze("show me the PR and update the description");
        assert_eq!(d.task_type, TaskType::Compound);
    }

    #[test]
    fn classify_unknown() {
        let d = analyze("matrixorigin");
        assert_eq!(d.task_type, TaskType::Unknown);
    }

    #[test]
    fn classify_memory_query_zh() {
        let d = analyze("我有哪些记忆？");
        assert_eq!(d.task_type, TaskType::Memory);
        assert_eq!(d.domain_hint, Some(DomainHint::Memory));
    }

    #[test]
    fn classify_followup_fetch_from_recent_github_tools() {
        let d = analyze_with_recent("pr呢？", 2, &["github_ci_status"]);
        assert_eq!(d.task_type, TaskType::Fetch);
        assert_eq!(d.domain_hint, Some(DomainHint::GitHub));
        match &d.tool_filter {
            ToolFilter::Domain(domains) => {
                assert!(domains.contains(&"github".to_string()));
                assert!(domains.contains(&"git".to_string()));
            }
            other => panic!("Expected Domain filter, got {:?}", other),
        }
    }

    #[test]
    fn classify_latest_followup_from_recent_github_tools() {
        let d = analyze_with_recent("最新的", 2, &["github_ci_status"]);
        assert!(d.conversation_state.is_followup);
        assert_eq!(d.task_type, TaskType::Fetch);
        assert_eq!(d.domain_hint, Some(DomainHint::GitHub));
    }

    #[test]
    fn first_turn_short_question_is_not_followup() {
        let d = analyze("pr呢？");
        assert!(!d.conversation_state.is_followup);
        assert_eq!(d.domain_hint, Some(DomainHint::GitHub));
    }

    // ── Memory Hints Improve Classification ──────────────────────────────

    #[test]
    fn memory_hints_improve_unknown_to_fetch() {
        // Without memory: "matrixorigin" is Unknown
        let d1 = analyze("matrixorigin");
        assert_eq!(d1.task_type, TaskType::Unknown);

        // With memory: domain hint from memory → Fetch
        let d2 = analyze_with_memory("matrixorigin", &["matrixorigin = GitHub org"]);
        assert_eq!(d2.task_type, TaskType::Fetch);
    }

    #[test]
    fn memory_hints_boost_confidence() {
        let d1 = analyze("matrixorigin");
        let d2 = analyze_with_memory("matrixorigin", &["matrixorigin = GitHub org"]);
        assert!(
            d2.confidence > d1.confidence,
            "Memory should boost confidence: {} > {}",
            d2.confidence,
            d1.confidence,
        );
    }

    // ── Domain Detection ─────────────────────────────────────────────────

    #[test]
    fn domain_github_from_signals() {
        let d = analyze("list the open PRs on GitHub");
        assert_eq!(d.domain_hint, Some(DomainHint::GitHub));
    }

    #[test]
    fn domain_git_from_signals() {
        let d = analyze("show me the git diff");
        assert_eq!(d.domain_hint, Some(DomainHint::Git));
    }

    #[test]
    fn domain_github_from_memory() {
        let d = analyze_with_memory("check matrixorigin", &["matrixorigin = GitHub repository"]);
        assert_eq!(d.domain_hint, Some(DomainHint::GitHub));
    }

    // ── Tool Filter Strategy ─────────────────────────────────────────────

    #[test]
    fn filter_wide_for_low_confidence() {
        let d = analyze("matrixorigin");
        assert_eq!(d.tool_filter, ToolFilter::Wide);
    }

    #[test]
    fn filter_domain_for_github() {
        let d = analyze("list the open PRs on GitHub");
        match &d.tool_filter {
            ToolFilter::Domain(domains) => {
                assert!(domains.contains(&"github".to_string()));
            }
            other => panic!("Expected Domain filter, got {:?}", other),
        }
    }

    #[test]
    fn filter_minimal_for_greeting() {
        let d = analyze("hello");
        assert_eq!(d.tool_filter, ToolFilter::Minimal);
    }

    // ── Round Estimation ─────────────────────────────────────────────────

    #[test]
    fn rounds_low_for_conversational() {
        let d = analyze("hello");
        assert!(d.estimated_rounds <= 2);
    }

    #[test]
    fn rounds_high_for_code() {
        let d = analyze("implement the parser in main.rs");
        assert!(d.estimated_rounds >= 5);
    }

    #[test]
    fn rounds_inflated_for_low_confidence() {
        let d = analyze("matrixorigin");
        // Unknown task + low confidence → base 5 * 1.5 = 8
        assert!(
            d.estimated_rounds > 5,
            "Low confidence should inflate rounds: {}",
            d.estimated_rounds,
        );
    }

    // ── Confidence ───────────────────────────────────────────────────────

    #[test]
    fn confidence_higher_with_signals() {
        let d_vague = analyze("something");
        let d_clear = analyze("list the open PRs on GitHub");
        assert!(
            d_clear.confidence > d_vague.confidence,
            "Clear query should have higher confidence: {} > {}",
            d_clear.confidence,
            d_vague.confidence,
        );
    }

    #[test]
    fn confidence_capped_at_1() {
        // Even with many signals, confidence should not exceed 1.0
        let d = analyze("analyze the GitHub PR diff and explain the git changes");
        assert!(d.confidence <= 1.0);
    }

    // ── Backward Compatibility ───────────────────────────────────────────

    #[test]
    fn conversation_state_matches_standalone() {
        let query = "show me the latest PRs on GitHub";
        let d = analyze(query);
        let standalone = ConversationState::from_message_with_context(query, 1, &[]);

        assert_eq!(d.conversation_state.is_fetch, standalone.is_fetch,);
        assert_eq!(d.conversation_state.is_github, standalone.is_github,);
        assert_eq!(
            d.conversation_state.signal_count(),
            standalone.signal_count(),
        );
    }

    // ── Preference Detection Edge Cases ──────────────────────────────────

    #[test]
    fn preference_zh_takes_priority_over_fetch() {
        // "关注" is preference even if the entity triggers is_fetch
        let d = analyze("我关注这个PR的状态");
        assert_eq!(d.task_type, TaskType::Memory);
    }

    #[test]
    fn en_watch_not_preference() {
        // "watch" is intentionally excluded from EN_PREFERENCE (too ambiguous)
        let d = analyze("watch the test output carefully");
        assert_ne!(d.task_type, TaskType::Memory);
    }

    #[test]
    fn en_follow_alone_is_preference() {
        let d = analyze("I want to follow matrixorigin");
        assert_eq!(d.task_type, TaskType::Memory);
    }

    // ── Disambiguation ───────────────────────────────────────────────────

    #[test]
    fn disambiguation_fetch_mutate_conflict() {
        let d = analyze("show me the PR and update the description");
        assert!(d.disambiguation.conflict_score > 0.5);
        assert_eq!(
            d.disambiguation.recommendation,
            DisambiguationAction::WidenToolSelection,
        );
    }

    #[test]
    fn disambiguation_single_intent_no_conflict() {
        let d = analyze("list the open PRs");
        assert!(d.disambiguation.conflict_score < 0.5);
    }
}
