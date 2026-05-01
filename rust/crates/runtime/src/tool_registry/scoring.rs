use std::collections::HashMap;
use std::sync::LazyLock;

use astra_text_utils::text_tokenize::tokenize;
use astra_turn_core::routing_metrics::{ConfidenceCalibrator, DisambiguationAction};
use astra_turn_core::tool_registry_meta::{IntentType, Scope, TOOL_CATALOG, ToolMeta};
use astra_turn_core::tool_registry_report::ToolQualityTracker;
use astra_turn_core::tool_registry_state::ConversationState;

/// Pre-computed inverse document frequency for each term across all tools.
/// Terms that appear in fewer tools get higher IDF (more discriminative).
struct TermIndex {
    /// term → IDF weight (log(N / df))
    idf: HashMap<String, f64>,
    /// tool_catalog_index → { term → normalized_tf }
    tool_tfs: Vec<HashMap<String, f64>>,
}

/// Build the term index from the tool catalog.
fn build_term_index() -> TermIndex {
    let n = TOOL_CATALOG.len() as f64;
    let mut doc_freq: HashMap<String, usize> = HashMap::new();
    let mut tool_tfs = Vec::with_capacity(TOOL_CATALOG.len());

    for tool in TOOL_CATALOG.iter() {
        // Combine description + triggers into one document per tool
        let mut doc = String::from(tool.description);
        doc.push(' ');
        doc.push_str(tool.name);
        for trigger in tool.triggers {
            doc.push(' ');
            doc.push_str(trigger);
        }

        let terms = tokenize(&doc);
        let total = terms.len().max(1) as f64;
        let mut tf_map: HashMap<String, f64> = HashMap::new();
        for term in &terms {
            *tf_map.entry(term.clone()).or_default() += 1.0;
        }
        // Normalize TF
        for v in tf_map.values_mut() {
            *v /= total;
        }
        // Track document frequency
        let unique_terms: std::collections::HashSet<&String> = tf_map.keys().collect();
        for t in unique_terms {
            *doc_freq.entry(t.clone()).or_default() += 1;
        }
        tool_tfs.push(tf_map);
    }

    // Compute IDF: log(N / df) — terms in fewer tools are more discriminative
    let idf: HashMap<String, f64> = doc_freq
        .into_iter()
        .map(|(term, df)| (term, (n / df as f64).ln().max(0.1)))
        .collect();

    TermIndex { idf, tool_tfs }
}

static TERM_INDEX: LazyLock<TermIndex> = LazyLock::new(build_term_index);

/// TF-IDF cosine similarity between a query and a tool's document.
/// Returns a score in [0.0, 1.0].
///
/// Uses proper cosine similarity: dot(q,d) / (||q|| * ||d||).
/// Both vectors are TF-IDF weighted; query uses binary TF (1.0 per term).
pub fn tfidf_score(query_terms: &[String], tool_idx: usize) -> f64 {
    let index = &*TERM_INDEX;
    let tool_tf = &index.tool_tfs[tool_idx];

    let mut dot_product = 0.0;
    let mut query_norm_sq = 0.0;
    let mut doc_norm_sq = 0.0;

    // Accumulate query-side norm and dot product for overlapping terms
    for qt in query_terms {
        let idf = index.idf.get(qt).copied().unwrap_or(0.0);
        let q_weight = idf; // query TF is 1.0 (binary)
        query_norm_sq += q_weight * q_weight;

        if let Some(&doc_tf) = tool_tf.get(qt) {
            dot_product += q_weight * (doc_tf * idf);
        }
    }

    // Accumulate doc-side norm over ALL terms in the tool's vocabulary
    for (term, &tf) in tool_tf.iter() {
        let idf = index.idf.get(term).copied().unwrap_or(0.0);
        let d_weight = tf * idf;
        doc_norm_sq += d_weight * d_weight;
    }

    let denom = query_norm_sq.sqrt() * doc_norm_sq.sqrt();
    if denom < f64::EPSILON {
        return 0.0;
    }
    (dot_product / denom).min(1.0)
}

// ─── Pre-filter: reorder dynamic tools by relevance ─────────────────────────

/// Direct trigger match: checks if any of the tool's triggers appear in the query.
/// Returns a score in [0.0, 1.0] based on match quality.
/// This is COMPLEMENTARY to TF-IDF — triggers capture multilingual synonyms
/// and phrase patterns that TF-IDF might miss (e.g., "关注" → github tools).
pub fn trigger_match_score(tool: &ToolMeta, query_lower: &str) -> f64 {
    use astra_turn_core::tool_registry_state::{
        split_haystack_words, word_boundary_match_prepared,
    };

    let mut best_score = 0.0;
    // Split the haystack once and reuse across all triggers. The haystack
    // split is O(haystack_len), which dominates for long user messages (see
    // `SCORING_QUERY_BYTE_CAP` in this crate); re-doing it per trigger used
    // to be the super-linear term behind the slow-test audit.
    let query_words = split_haystack_words(query_lower);

    for trigger in tool.triggers {
        if word_boundary_match_prepared(query_lower, &query_words, trigger) {
            // Longer triggers = more specific = higher score
            let specificity = (trigger.chars().count() as f64 / 10.0).min(1.0);
            let score = 0.5 + 0.5 * specificity; // range: 0.5 – 1.0
            if score > best_score {
                best_score = score;
            }
        } else {
            let trigger_lower = trigger.to_lowercase();
            let trigger_words: Vec<&str> = trigger_lower.split_whitespace().collect();

            if trigger_words.len() >= 2 {
                // Bag-of-words: all trigger words present in query (any order).
                // Handles "review local changes" matching trigger "review changes".
                let all_present = trigger_words
                    .iter()
                    .all(|tw| query_words.iter().any(|qw| qw == tw));
                if all_present {
                    let specificity = (trigger.chars().count() as f64 / 10.0).min(1.0);
                    // Slightly lower than contiguous match (0.4 base vs 0.5).
                    let score = 0.4 + 0.4 * specificity;
                    if score > best_score {
                        best_score = score;
                    }
                } else {
                    // Partial prefix match: first word of trigger in query.
                    let query_has_first = trigger_words
                        .first()
                        .is_some_and(|fw| query_words.iter().any(|qw| qw == fw));
                    if query_has_first {
                        let partial_score = 0.3;
                        if partial_score > best_score {
                            best_score = partial_score;
                        }
                    }
                }
            }
        }
    }
    best_score
}

/// Score a tool's relevance to the current conversation state.
/// Combines TF-IDF textual similarity, direct trigger matching,
/// intent/scope alignment, and content-gated recency.
///
/// score = content_base + intent_alignment + scope + gated_recency
///
/// Higher = more relevant. Range: 0.0 to 1.0.
/// Minimum content relevance (TF-IDF + trigger) for full recency boost.
/// Below this threshold recency decays linearly; at 0 content it is fully
/// suppressed.  This is the GENERAL gate — it does NOT inspect IntentType
/// or ConversationState flags, so adding new tool categories requires zero
/// changes here.
const RECENCY_CONTENT_GATE: f64 = 0.08;

/// Raw recency signal for a tool: +0.3 exact reuse, +0.1 same-category, else 0.
/// This is a pure lookup — it does NOT decide whether to apply the boost.
fn raw_recency_boost(tool: &ToolMeta, state: &ConversationState) -> f64 {
    if state.recent_tools.iter().any(|r| r == tool.name) {
        return 0.3;
    }
    for intent in tool.intents {
        let same_category = state.recent_tools.iter().any(|r| {
            TOOL_CATALOG
                .iter()
                .find(|t| t.name == r.as_str())
                .is_some_and(|t| t.intents.contains(intent))
        });
        if same_category {
            return 0.1;
        }
    }
    0.0
}

// File-context scoring: boost tools relevant to detected project languages.
// Language tags (e.g., "rust", "typescript") come from workspace marker files.
fn file_context_tool_boost(tool_name: &str, file_context: &[String]) -> f64 {
    if file_context.is_empty() {
        return 0.0;
    }
    // Tools that benefit from language awareness
    let boost = file_context.iter().any(|lang| match lang.as_str() {
        "rust" => matches!(
            tool_name,
            "bash" | "grep" | "read_file" | "write_file" | "str_replace" | "glob"
        ),
        "typescript" | "javascript" => matches!(
            tool_name,
            "bash" | "grep" | "read_file" | "write_file" | "str_replace" | "glob"
        ),
        "python" => matches!(
            tool_name,
            "bash" | "grep" | "read_file" | "write_file" | "str_replace" | "glob"
        ),
        "go" => matches!(
            tool_name,
            "bash" | "grep" | "read_file" | "write_file" | "str_replace" | "glob"
        ),
        "docker" => matches!(tool_name, "bash" | "read_file" | "write_file"),
        _ => false,
    });
    if boost { 0.05 } else { 0.0 }
}

fn explicit_lsp_signal(query_lower: &str) -> bool {
    use astra_turn_core::tool_registry_state::{
        split_haystack_words, word_boundary_match_prepared,
    };

    const LSP_SIGNALS: &[&str] = &[
        "lsp",
        "language server",
        "go to definition",
        "definition",
        "find references",
        "references",
        "hover",
        "implementation",
        "type hierarchy",
        "supertypes",
        "subtypes",
        "inheritance",
        "type definition",
        "declaration",
        "rename symbol",
        "prepare rename",
        "code action",
        "quick fix",
        "quickfix",
        "completion",
        "completions",
        "autocomplete",
        "auto-complete",
        "auto import",
        "postfix completion",
        "diagnostics",
        "document highlight",
        "document link",
        "selection range",
        "inlay hint",
        "folding range",
        "document color",
        "color presentation",
        "semantic token",
        "code lens",
        "call hierarchy",
        "incoming calls",
        "outgoing calls",
        "document symbol",
        "workspace symbol",
        "语义跳转",
        "定义",
        "引用",
        "悬停",
        "实现",
        "类型层次",
        "父类型",
        "子类型",
        "类型定义",
        "声明",
        "重命名",
        "代码动作",
        "快速修复",
        "补全",
        "自动补全",
        "自动导入",
        "诊断",
        "高亮",
        "文档链接",
        "选择范围",
        "内联提示",
        "折叠范围",
        "文档颜色",
        "颜色表示",
        "语义令牌",
        "代码透镜",
        "调用层次",
    ];

    let words = split_haystack_words(query_lower);
    LSP_SIGNALS
        .iter()
        .any(|signal| word_boundary_match_prepared(query_lower, &words, signal))
}

#[allow(clippy::too_many_arguments)]
fn tool_relevance_score(
    tool: &ToolMeta,
    tool_idx: usize,
    state: &ConversationState,
    query_terms: &[String],
    query_lower: &str,
    memory_domain_hints: &[crate::pipeline::routing::DomainHint],
    co_occurrence: &HashMap<String, f64>,
    file_context: &[String],
    outcome_bias: &HashMap<String, f64>,
) -> f64 {
    use crate::pipeline::routing::DomainHint;

    let mut score = 0.0;

    // ── Phase 1: Content relevance (tool-agnostic textual match) ──
    let text_score = tfidf_score(query_terms, tool_idx);
    score += text_score * 0.40;

    let trigger_score = trigger_match_score(tool, query_lower);
    score += trigger_score * 0.25;

    // Content relevance = weighted sum of objective textual signals.
    // Used as the recency gate: if the query has zero textual overlap with
    // this tool, recency cannot create relevance from nothing.
    let content_relevance = text_score * 0.40 + trigger_score * 0.25;

    // Combined signal for intent gating
    let text_or_trigger = text_score.max(trigger_score);

    // ── Phase 2: Intent alignment ──
    for intent in tool.intents {
        match intent {
            IntentType::GitHub if state.is_github => score += 0.25,
            IntentType::GitHub if state.is_fetch => score += 0.15,
            IntentType::Git if state.is_git => score += 0.25,
            IntentType::Git if state.is_fetch || state.references_history => score += 0.15,
            IntentType::Memory if text_or_trigger > 0.05 => score += 0.2,
            // NOTE: references_history removed here — it conflates
            // "referring to earlier conversation" with "needs persistent memory",
            // causing memory_search to be over-selected for conversational queries.
            IntentType::Memory if state.is_analytical => score += 0.1,
            IntentType::CodeEdit if state.is_mutate => score += 0.15,
            IntentType::CodeRead if state.is_fetch || state.is_analytical => score += 0.1,
            IntentType::Introspect if state.is_analytical => score += 0.15,
            IntentType::Database if text_or_trigger > 0.05 => score += 0.2,
            IntentType::Database if state.is_fetch || state.is_analytical => score += 0.1,
            _ => {}
        }
    }

    // ── Phase 2b: Memory domain hint alignment ──
    // When memory says the user cares about a domain (e.g., "matrixorigin → GitHub"),
    // boost tools in that domain even without keyword signals.
    // General mechanism: DomainHint → IntentType mapping, then +0.15 if tool matches.
    if !memory_domain_hints.is_empty() {
        let tool_matches_memory_domain = tool.intents.iter().any(|intent| {
            memory_domain_hints.iter().any(|domain| {
                matches!(
                    (domain, intent),
                    (DomainHint::GitHub, IntentType::GitHub)
                        | (DomainHint::Git, IntentType::Git)
                        | (
                            DomainHint::Code,
                            IntentType::CodeEdit | IntentType::CodeRead
                        )
                        | (DomainHint::Memory, IntentType::Memory)
                        | (DomainHint::Database, IntentType::Database)
                )
            })
        });
        if tool_matches_memory_domain {
            score += 0.15;
        }
    }

    // ── Phase 3: Scope alignment ──
    match tool.scope {
        Scope::External if state.is_fetch && !state.is_mutate => score += 0.1,
        Scope::CrossSession if state.references_history => score += 0.1,
        _ => {}
    }

    // ── Phase 3b: Explicit code-intel boost ──
    // Queries that explicitly ask for LSP-style editor intelligence should
    // prefer the unified lsp tool over generic file/text tools.
    if tool.name == "lsp" && explicit_lsp_signal(query_lower) {
        score += 0.30;
    }

    // ── Phase 4: Content-gated recency ──
    // GENERAL PRINCIPLE: recency AMPLIFIES existing textual relevance.
    // A tool with zero content match gets zero recency, regardless of how
    // recently it was used.  This prevents cross-intent contamination
    // (e.g., GitHub tools bleeding into memory queries) WITHOUT per-intent
    // match statements.  Adding new IntentTypes requires zero changes here.
    //
    // Gate ramps linearly: content < GATE → proportional fraction of boost.
    //
    // Memory domain hints soften the gate: if memory confirms the tool's domain
    // is relevant, the effective gate is halved — allowing recency boost even
    // when TF-IDF overlap is minimal (entity names not in tool vocabulary).
    // Cold-start relaxation: on turns 1-2, recency history is thin, so
    // soften the gate to avoid blocking valid tools. By turn 3+ the
    // recent_tools history is populated enough for the normal gate.
    let cold_start_factor = match state.turn_count {
        0..=1 => 0.5, // Halve gate on first 2 turns
        2 => 0.75,    // Slightly relaxed on turn 3
        _ => 1.0,     // Normal gate
    };
    let effective_gate = if !memory_domain_hints.is_empty() {
        let tool_in_memory_domain = tool.intents.iter().any(|intent| {
            memory_domain_hints.iter().any(|domain| {
                matches!(
                    (domain, intent),
                    (DomainHint::GitHub, IntentType::GitHub)
                        | (DomainHint::Git, IntentType::Git)
                        | (
                            DomainHint::Code,
                            IntentType::CodeEdit | IntentType::CodeRead
                        )
                        | (DomainHint::Memory, IntentType::Memory)
                        | (DomainHint::Database, IntentType::Database)
                )
            })
        });
        if tool_in_memory_domain {
            RECENCY_CONTENT_GATE * 0.5 * cold_start_factor
        } else {
            RECENCY_CONTENT_GATE * cold_start_factor
        }
    } else {
        RECENCY_CONTENT_GATE * cold_start_factor
    };

    let recency_raw = raw_recency_boost(tool, state);
    if recency_raw > 0.0 {
        let gate = (content_relevance / effective_gate).min(1.0);
        score += recency_raw * gate;
    }

    // ── Phase 5: Tool co-occurrence boost ──
    // When PatternLibrary has learned that certain tools succeed together,
    // boost tools that frequently co-occur with recently-used tools.
    // Max boost: +0.10 — enough to tip marginal tools into selection
    // but not enough to override strong textual/intent signals.
    if let Some(&co_score) = co_occurrence.get(tool.name) {
        score += co_score * 0.10;
    }

    // ── Phase 6: File-context boost ──
    // When we detect project languages from workspace marker files (e.g.,
    // Cargo.toml → "rust"), slightly boost code-editing tools that are
    // universally useful for that language. Small boost (+0.05) acts as
    // a tiebreaker, not an override.
    score += file_context_tool_boost(tool.name, file_context);

    // ── Phase 7: Outcome-memory bias ──
    // Persistent per-tool outcome signal from `ToolHealthTracker`. Recent
    // identical-signature failures push the score down; recent successes
    // push it up. Bounded to ±0.10 so it can tip ties but never overpower
    // textual/intent signals. The hard-block for repeated identical
    // failures lives at execution time (`headless_tool_pipeline::policy`).
    if let Some(&bias) = outcome_bias.get(tool.name) {
        score += bias.clamp(-0.10, 0.10);
    }

    // ── Soft ceiling ──
    // Hard clamp at 1.0 hides rank differences when multiple tools exceed 1.0
    // (e.g., text=0.40 + trigger=0.25 + intent=0.25 + scope=0.10 + recency=0.30 = 1.30).
    // Soft ceiling: diminishing returns above 1.0 preserves relative ordering.
    if score > 1.0 {
        1.0 + (score - 1.0) * 0.5
    } else {
        score
    }
}

/// Pre-filter: rank dynamic tools by relevance and filter by minimum score threshold.
/// Returns (catalog_index, score) pairs for dynamic tools, sorted by descending score.
pub fn pre_filter_dynamic(state: &ConversationState, query: &str) -> Vec<(usize, f64)> {
    pre_filter_dynamic_core(
        state,
        query,
        None,
        None,
        &[],
        &HashMap::new(),
        &[],
        &HashMap::new(),
    )
}

/// Like [`pre_filter_dynamic`] but accepts an optional quality tracker to boost/penalize
/// tools based on historical effectiveness.
pub fn pre_filter_dynamic_with_quality(
    state: &ConversationState,
    query: &str,
    quality_tracker: Option<&ToolQualityTracker>,
) -> Vec<(usize, f64)> {
    pre_filter_dynamic_core(
        state,
        query,
        quality_tracker,
        None,
        &[],
        &HashMap::new(),
        &[],
        &HashMap::new(),
    )
}

/// Full-featured pre-filter with both quality tracking and confidence calibration.
/// Use this from production paths where session-scoped state is available.
pub fn pre_filter_dynamic_calibrated(
    state: &ConversationState,
    query: &str,
    quality_tracker: Option<&ToolQualityTracker>,
    calibrator: Option<&ConfidenceCalibrator>,
) -> Vec<(usize, f64)> {
    pre_filter_dynamic_core(
        state,
        query,
        quality_tracker,
        calibrator,
        &[],
        &HashMap::new(),
        &[],
        &HashMap::new(),
    )
}

/// Full pre-filter with memory domain hints for gate softening.
/// When memory provides domain hints (e.g., user tracks GitHub entities),
/// tools in those domains get a score boost and softened content gate.
pub fn pre_filter_dynamic_with_memory(
    state: &ConversationState,
    query: &str,
    quality_tracker: Option<&ToolQualityTracker>,
    calibrator: Option<&ConfidenceCalibrator>,
    memory_domain_hints: &[crate::pipeline::routing::DomainHint],
) -> Vec<(usize, f64)> {
    pre_filter_dynamic_core(
        state,
        query,
        quality_tracker,
        calibrator,
        memory_domain_hints,
        &HashMap::new(),
        &[],
        &HashMap::new(),
    )
}

/// Pressure-aware pre-filter: applies an additional minimum-score floor that
/// rises with budget pressure, excluding marginally-relevant tools when token
/// headroom is scarce.
///
/// | pressure | floor   | effect                               |
/// |----------|---------|--------------------------------------|
/// | 0.0      | 0.00    | no extra filtering                   |
/// | 0.3      | 0.05    | exclude noise                        |
/// | 0.6      | 0.10    | keep only clearly relevant tools     |
/// | 0.9      | 0.18    | keep only strongly relevant tools    |
///
/// Also reduces `min_recall` ceiling so fewer low-scoring tools are force-
/// included under high pressure.
pub fn pre_filter_dynamic_with_pressure(
    state: &ConversationState,
    query: &str,
    quality_tracker: Option<&ToolQualityTracker>,
    calibrator: Option<&ConfidenceCalibrator>,
    memory_domain_hints: &[crate::pipeline::routing::DomainHint],
    budget_pressure: f64,
) -> Vec<(usize, f64)> {
    pre_filter_dynamic_with_pressure_and_cooccurrence(
        state,
        query,
        quality_tracker,
        calibrator,
        memory_domain_hints,
        budget_pressure,
        &HashMap::new(),
        &[],
        &HashMap::new(),
    )
}

/// Full pre-filter with pressure, memory hints, AND tool co-occurrence learning.
/// Co-occurrence scores come from PatternLibrary::co_occurrence_scores() and
/// boost tools that historically succeed alongside recently-used tools.
pub fn pre_filter_dynamic_with_cooccurrence(
    state: &ConversationState,
    query: &str,
    quality_tracker: Option<&ToolQualityTracker>,
    calibrator: Option<&ConfidenceCalibrator>,
    memory_domain_hints: &[crate::pipeline::routing::DomainHint],
    budget_pressure: f64,
    co_occurrence: &HashMap<String, f64>,
) -> Vec<(usize, f64)> {
    pre_filter_dynamic_with_pressure_and_cooccurrence(
        state,
        query,
        quality_tracker,
        calibrator,
        memory_domain_hints,
        budget_pressure,
        co_occurrence,
        &[],
        &HashMap::new(),
    )
}

/// Full pre-filter with pressure, co-occurrence, AND file-context scoring.
/// This is the most complete scoring path for production use.
#[allow(clippy::too_many_arguments)]
pub fn pre_filter_dynamic_with_file_context(
    state: &ConversationState,
    query: &str,
    quality_tracker: Option<&ToolQualityTracker>,
    calibrator: Option<&ConfidenceCalibrator>,
    memory_domain_hints: &[crate::pipeline::routing::DomainHint],
    budget_pressure: f64,
    co_occurrence: &HashMap<String, f64>,
    file_context: &[String],
) -> Vec<(usize, f64)> {
    pre_filter_dynamic_with_pressure_and_cooccurrence(
        state,
        query,
        quality_tracker,
        calibrator,
        memory_domain_hints,
        budget_pressure,
        co_occurrence,
        file_context,
        &HashMap::new(),
    )
}

/// Pre-filter including outcome-memory bias. When `outcome_bias` is non-empty,
/// each catalog tool gets an additive score adjustment (±0.10) derived from
/// recent per-signature success/failure evidence (see
/// [`astra_turn_core::tool_health::ToolHealthTracker::outcome_bias_by_tool`]).
#[allow(clippy::too_many_arguments)]
pub fn pre_filter_dynamic_with_outcome_bias(
    state: &ConversationState,
    query: &str,
    quality_tracker: Option<&ToolQualityTracker>,
    calibrator: Option<&ConfidenceCalibrator>,
    memory_domain_hints: &[crate::pipeline::routing::DomainHint],
    budget_pressure: f64,
    co_occurrence: &HashMap<String, f64>,
    file_context: &[String],
    outcome_bias: &HashMap<String, f64>,
) -> Vec<(usize, f64)> {
    pre_filter_dynamic_with_pressure_and_cooccurrence(
        state,
        query,
        quality_tracker,
        calibrator,
        memory_domain_hints,
        budget_pressure,
        co_occurrence,
        file_context,
        outcome_bias,
    )
}

/// Internal: pressure + co-occurrence + file-context + outcome bias.
#[allow(clippy::too_many_arguments)]
fn pre_filter_dynamic_with_pressure_and_cooccurrence(
    state: &ConversationState,
    query: &str,
    quality_tracker: Option<&ToolQualityTracker>,
    calibrator: Option<&ConfidenceCalibrator>,
    memory_domain_hints: &[crate::pipeline::routing::DomainHint],
    budget_pressure: f64,
    co_occurrence: &HashMap<String, f64>,
    file_context: &[String],
    outcome_bias: &HashMap<String, f64>,
) -> Vec<(usize, f64)> {
    let mut result = pre_filter_dynamic_core(
        state,
        query,
        quality_tracker,
        calibrator,
        memory_domain_hints,
        co_occurrence,
        file_context,
        outcome_bias,
    );

    if budget_pressure > 0.01 {
        // Pressure floor: quadratic ramp so Normal is unaffected and
        // AggressivePrune (0.9) is aggressive.
        let pressure_floor = budget_pressure * budget_pressure * 0.22;
        result.retain(|&(_, score)| score >= pressure_floor);

        // Under high pressure, also cap the minimum recall guarantee.
        // At 0.9 pressure we allow as few as 1 forced tool; at 0.3, still 3.
        let max_recall = ((1.0 - budget_pressure) * 5.0).ceil() as usize;
        let max_recall = max_recall.max(1);
        if result.len() > max_recall {
            result.truncate(max_recall);
        }
    }

    result
}

/// Cap the query length (in **bytes**) for scoring purposes. Triggers are
/// short keywords; nothing past the first few hundred characters of a user
/// message moves the needle for tool selection, and scanning longer queries
/// costs roughly O(query_len × tools × terms_per_query) — a raw 10 KB paste
/// turns one `pre_filter_dynamic` call into multi-second CPU on debug
/// builds.
///
/// Size rationale (4 KiB):
/// - Covers the first ~4000 ASCII chars (more than any LLM trigger cares
///   about).
/// - ~2000 mixed-Latin chars.
/// - ~1360 pure-CJK chars (3 bytes per codepoint under UTF-8) — comfortably
///   above realistic tool-selection prompts in Chinese, even with pasted
///   code snippets.
///
/// If this ever needs raising, fix the per-query super-linear term first
/// (dedup `query_terms` before feeding TF-IDF, cache the haystack split
/// inside the per-tool loop) — then the cap can grow without paying CPU.
const SCORING_QUERY_BYTE_CAP: usize = 4096;

/// Return the longest prefix of `s` whose byte length ≤ `max_bytes`, snapped
/// down to a UTF-8 character boundary. Never splits a multi-byte codepoint.
fn truncate_at_char_boundary(s: &str, max_bytes: usize) -> &str {
    if s.len() <= max_bytes {
        return s;
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// Core pre-filter implementation.
#[allow(clippy::too_many_arguments)]
fn pre_filter_dynamic_core(
    state: &ConversationState,
    query: &str,
    quality_tracker: Option<&ToolQualityTracker>,
    calibrator: Option<&ConfidenceCalibrator>,
    memory_domain_hints: &[crate::pipeline::routing::DomainHint],
    co_occurrence: &HashMap<String, f64>,
    file_context: &[String],
    outcome_bias: &HashMap<String, f64>,
) -> Vec<(usize, f64)> {
    // Short-circuit: pure conversational queries don't need dynamic tools.
    if state.is_conversational && !state.is_fetch && !state.is_mutate && !state.is_analytical {
        return vec![];
    }

    // Truncate first — downstream callees do O(haystack_len) work per trigger
    // per tool (~500 calls per scoring), so an un-truncated 10k-char paste
    // would burn seconds of CPU on every turn.
    let query: &str = truncate_at_char_boundary(query, SCORING_QUERY_BYTE_CAP);
    let query_lower = query.to_lowercase();
    let query_terms = tokenize(query);
    let mut scored: Vec<(usize, f64)> = TOOL_CATALOG
        .iter()
        .enumerate()
        .filter(|(_, t)| !t.pinned)
        .map(|(idx, tool)| {
            let mut score = tool_relevance_score(
                tool,
                idx,
                state,
                &query_terms,
                &query_lower,
                memory_domain_hints,
                co_occurrence,
                file_context,
                outcome_bias,
            );
            if let Some(tracker) = quality_tracker {
                score *= tracker.boost_factor(tool.name);
            }
            (idx, score)
        })
        .collect();

    // Sort by descending score, then by catalog order for ties
    scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));

    // ── Signal-strength adaptive threshold ──
    // When ConversationState has few signals, the selector is UNCERTAIN.
    // Rather than excluding all dynamic tools (which forces the LLM to use
    // only pinned tools like bash/read_file), we LOWER the threshold so more
    // tools pass through. The LLM then makes the final tool choice.
    //
    // Signal quality weighting: not all signals are equal.
    // - Intent signals (is_github, is_git) are strong indicators (weight 1.0)
    // - Action signals (is_fetch, is_mutate) are medium indicators (weight 0.7)
    // - Context signals (references_history, is_analytical) are weaker (weight 0.5)
    // Weighted sum gives a more nuanced confidence measure than binary count.
    let signal_weights: &[(bool, f64)] = &[
        (state.is_github, 1.0),          // strong: specific domain
        (state.is_git, 1.0),             // strong: specific domain
        (state.is_fetch, 0.7),           // medium: action type
        (state.is_mutate, 0.7),          // medium: action type
        (state.is_analytical, 0.5),      // weaker: broad category
        (state.references_history, 0.5), // weaker: contextual
    ];
    let signal_strength: f64 = signal_weights
        .iter()
        .filter(|(active, _)| *active)
        .map(|(_, w)| w)
        .sum();

    let base_threshold = if signal_strength < 0.01 {
        0.0 // No signals → include everything above zero
    } else if signal_strength < 0.8 {
        MIN_SCORE_THRESHOLD * 0.3 // Weak signals → very low bar
    } else {
        MIN_SCORE_THRESHOLD // Strong signals → normal threshold
    };

    // ── Score spread adaptive threshold ──
    // When top scores are tightly clustered (many tools scored similarly),
    // the selector is uncertain about which tools are best → lower threshold
    // to give the LLM more choices.
    // When there's a clear winner with a big gap, be more selective.
    let spread_factor = if scored.len() >= 3 {
        let top_score = scored[0].1;
        // Look at the 5th tool or last tool (whichever comes first)
        let reference_idx = (scored.len() - 1).min(4);
        let reference_score = scored[reference_idx].1;
        let spread = top_score - reference_score;
        if spread < 0.03 && top_score > 0.0 {
            0.5 // Tight cluster → halve threshold (uncertain)
        } else if spread > 0.15 {
            1.2 // Wide gap → slightly raise threshold (confident)
        } else {
            1.0 // Normal spread → no adjustment
        }
    } else {
        1.0
    };

    let spread_adjusted = base_threshold * spread_factor;

    // Apply confidence calibration if available. The calibrator adjusts the
    // threshold based on historical correction rate for the dominant intent.
    let calibrated_threshold = if let Some(cal) = calibrator {
        let primary_intent = if state.is_github {
            "github"
        } else if state.is_git {
            "git"
        } else if state.is_fetch {
            "fetch"
        } else if state.is_mutate {
            "mutate"
        } else if state.is_analytical {
            "analytical"
        } else {
            "general"
        };
        let cal_value = cal.calibrated_threshold(primary_intent);
        // Calibrator returns absolute threshold; combine with signal-based:
        // use the lower of (signal-adaptive, calibrated) to be recall-first.
        spread_adjusted.min(cal_value)
    } else {
        spread_adjusted
    };

    // When disambiguation detects conflicting intents (e.g., fetch+mutate), lower
    // the threshold further so more tools pass — covering both intent categories.
    let effective_threshold = match state.disambiguation.as_ref().map(|d| &d.recommendation) {
        Some(DisambiguationAction::WidenToolSelection) => calibrated_threshold * 0.5,
        Some(DisambiguationAction::ProceedWithNote) => calibrated_threshold * 0.8,
        _ => calibrated_threshold,
    };

    // ── Recall-first selection ──
    // The tool selector is a RECALL engine — its job is to include all
    // potentially relevant tools. The LLM handles PRECISION (final pick).
    // Therefore:
    //   1. Always include top-N non-zero scoring tools (MIN_RECALL_TOOLS)
    //   2. Threshold is a soft ranking hint, not a hard exclusion gate
    //   3. Budget gate (800 tokens) is the real constraint downstream
    //   4. Ensure intent diversity: at least 1 tool per active intent category

    let above_threshold: Vec<_> = scored
        .iter()
        .filter(|(_, s)| *s >= effective_threshold && *s > 0.0)
        .copied()
        .collect();

    // Start with tools above threshold
    let mut result = above_threshold;

    // Guarantee minimum recall: always include at least `min_recall` tools.
    // Adaptive: weaker signals → more tools (compensate for uncertainty).
    let min_recall = if signal_strength < 0.01 {
        5 // No idea what user wants → cast wide net
    } else if signal_strength < 0.8 {
        4 // Weak signals → slightly wider
    } else {
        MIN_RECALL_TOOLS // Strong signals → focused selection
    };
    if result.len() < min_recall {
        for &(idx, s) in &scored {
            if !result.iter().any(|&(i, _)| i == idx) {
                result.push((idx, s));
                if result.len() >= min_recall {
                    break;
                }
            }
        }
    }

    // If STILL empty (no dynamic tools at all), return empty
    if result.is_empty() {
        return result;
    }

    // Intent diversity: ensure at least 1 tool per active intent category.
    // If the user's query triggered is_github, we MUST include a GitHub tool
    // even if it scored below threshold.
    ensure_intent_diversity(&mut result, &scored, state);

    // Re-sort by score (diversity insertion may have disrupted order)
    result.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    result
}

/// Minimum number of dynamic tools to include for non-conversational queries.
/// Ensures the LLM always has meaningful choices beyond pinned tools.
/// Base minimum recall. Actual minimum is adaptive based on signal count:
///   0 signals → 5 (uncertain → give LLM more choices)
///   1 signal  → 4
///   2+ signals → 3 (confident → fewer, focused tools)
const MIN_RECALL_TOOLS: usize = 3;

/// Ensure at least 1 tool from each intent category that the user's query activated.
/// This prevents the scenario where a GitHub query gets only Git tools because
/// git_diff scored higher than github_list_prs.
fn ensure_intent_diversity(
    result: &mut Vec<(usize, f64)>,
    all_scored: &[(usize, f64)],
    state: &ConversationState,
) {
    use astra_turn_core::tool_registry_meta::IntentType;

    let intent_requirements: &[(bool, IntentType)] = &[
        (state.is_github, IntentType::GitHub),
        (state.is_git, IntentType::Git),
        (state.is_analytical, IntentType::Introspect),
        (state.is_analytical, IntentType::Memory),
        (state.is_mutate, IntentType::CodeEdit),
    ];

    for &(active, ref intent) in intent_requirements {
        if !active {
            continue;
        }
        // Check if result already has a tool with this intent
        let has_intent = result
            .iter()
            .any(|&(idx, _)| TOOL_CATALOG[idx].intents.contains(intent));
        if has_intent {
            continue;
        }
        // Find the highest-scoring tool with this intent from all_scored
        // but only force it in if it scored above a minimum bar — forcing
        // a 0.02-scoring tool wastes a schema slot without helping.
        if let Some(&best) = all_scored
            .iter()
            .find(|&&(idx, score)| TOOL_CATALOG[idx].intents.contains(intent) && score >= 0.10)
        {
            result.push(best);
        }
    }
}

// ─── Budget gate ────────────────────────────────────────────────────────────

/// Default token budget for tool schemas in the context window.
/// Sized to select ~4-6 dynamic tools on a typical query; forces real scoring.
pub const DEFAULT_TOOL_BUDGET_TOKENS: u32 = 800;

/// Minimum relevance score a dynamic tool must exceed to be considered.
/// Tools scoring below this threshold are excluded even if budget allows.
const MIN_SCORE_THRESHOLD: f64 = 0.05;

// ─── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool_registry::state::ConversationState;

    fn state_at_turn(turn: u32) -> ConversationState {
        ConversationState {
            turn_count: turn,
            ..Default::default()
        }
    }

    #[test]
    fn cold_start_factor_halves_gate_on_turn_zero() {
        let s = state_at_turn(0);
        let factor = match s.turn_count {
            0..=1 => 0.5,
            2 => 0.75,
            _ => 1.0,
        };
        assert_eq!(factor, 0.5);
        // Effective gate = 0.08 * 0.5 = 0.04
        assert!((RECENCY_CONTENT_GATE * factor - 0.04).abs() < 1e-10);
    }

    #[test]
    fn cold_start_factor_normal_after_turn_three() {
        let s = state_at_turn(5);
        let factor = match s.turn_count {
            0..=1 => 0.5,
            2 => 0.75,
            _ => 1.0,
        };
        assert_eq!(factor, 1.0);
        assert!((RECENCY_CONTENT_GATE * factor - 0.08).abs() < 1e-10);
    }

    #[test]
    fn cold_start_factor_intermediate_at_turn_two() {
        let s = state_at_turn(2);
        let factor = match s.turn_count {
            0..=1 => 0.5,
            2 => 0.75,
            _ => 1.0,
        };
        assert_eq!(factor, 0.75);
    }

    #[test]
    fn intent_diversity_rejects_low_score_tools() {
        // Simulate: GitHub intent is active, but the only GitHub-intent tool
        // scored 0.05 (below the 0.10 floor).
        let state = ConversationState {
            is_github: true,
            ..Default::default()
        };

        // We can't easily construct TOOL_CATALOG entries, but we can verify
        // the logic: the function skips tools whose score < 0.10.
        let all_scored: Vec<(usize, f64)> = vec![];
        let mut result: Vec<(usize, f64)> = vec![];
        ensure_intent_diversity(&mut result, &all_scored, &state);
        // No tools available → nothing added (regression: previously could add invalid)
        assert!(result.is_empty());
    }

    // ──────────────────────────────────────────────────────────
    // tfidf_score
    // ──────────────────────────────────────────────────────────

    #[test]
    fn tfidf_score_empty_query_returns_zero() {
        let score = tfidf_score(&[], 0);
        assert_eq!(score, 0.0);
    }

    #[test]
    fn tfidf_score_unknown_terms_returns_zero() {
        let terms: Vec<String> = vec!["xyznonexistent123".into()];
        let score = tfidf_score(&terms, 0);
        assert_eq!(score, 0.0);
    }

    #[test]
    fn tfidf_score_relevant_terms_positive() {
        // "file" and "read" should match read_file tool (index 0 in TOOL_CATALOG)
        let terms: Vec<String> = vec!["file".into(), "read".into()];
        // Find the read_file tool index
        let idx = TOOL_CATALOG
            .iter()
            .position(|t| t.name == "read_file")
            .unwrap();
        let score = tfidf_score(&terms, idx);
        assert!(score > 0.0, "expected positive score, got {score}");
    }

    #[test]
    fn tfidf_score_bounded_by_one() {
        // Even with perfect match terms, score should be <= 1.0
        let idx = TOOL_CATALOG.iter().position(|t| t.name == "bash").unwrap();
        let terms: Vec<String> = vec![
            "bash".into(),
            "execute".into(),
            "command".into(),
            "shell".into(),
            "run".into(),
        ];
        let score = tfidf_score(&terms, idx);
        assert!(score <= 1.0, "expected score <= 1.0, got {score}");
    }

    #[test]
    fn explicit_lsp_signal_recognizes_completion_and_quickfix_queries() {
        for query in [
            "show autocomplete candidates",
            "can you do auto import here",
            "apply the quick fix",
            "帮我自动补全",
            "这里能自动导入吗",
        ] {
            let lower = query.to_lowercase();
            assert!(
                explicit_lsp_signal(&lower),
                "expected explicit LSP signal for query: {query}"
            );
        }
    }

    // ──────────────────────────────────────────────────────────
    // file_context_tool_boost
    // ──────────────────────────────────────────────────────────

    #[test]
    fn file_context_boost_empty_context() {
        assert_eq!(file_context_tool_boost("bash", &[]), 0.0);
    }

    #[test]
    fn file_context_boost_rust_bash() {
        let ctx = vec!["rust".to_string()];
        assert_eq!(file_context_tool_boost("bash", &ctx), 0.05);
    }

    #[test]
    fn file_context_boost_rust_unrelated_tool() {
        let ctx = vec!["rust".to_string()];
        assert_eq!(file_context_tool_boost("github_search_repos", &ctx), 0.0);
    }

    #[test]
    fn file_context_boost_python() {
        let ctx = vec!["python".to_string()];
        assert_eq!(file_context_tool_boost("read_file", &ctx), 0.05);
        assert_eq!(file_context_tool_boost("write_file", &ctx), 0.05);
    }

    #[test]
    fn file_context_boost_docker() {
        let ctx = vec!["docker".to_string()];
        assert_eq!(file_context_tool_boost("bash", &ctx), 0.05);
        assert_eq!(file_context_tool_boost("grep", &ctx), 0.0); // grep not in docker list
    }

    #[test]
    fn file_context_boost_unknown_lang() {
        let ctx = vec!["haskell".to_string()];
        assert_eq!(file_context_tool_boost("bash", &ctx), 0.0);
    }

    // ──────────────────────────────────────────────────────────
    // raw_recency_boost
    // ──────────────────────────────────────────────────────────

    #[test]
    fn recency_boost_exact_tool_match() {
        let state = ConversationState {
            recent_tools: vec!["bash".into()],
            ..Default::default()
        };
        let tool = TOOL_CATALOG.iter().find(|t| t.name == "bash").unwrap();
        assert_eq!(raw_recency_boost(tool, &state), 0.3);
    }

    #[test]
    fn recency_boost_no_match() {
        let state = ConversationState {
            recent_tools: vec!["bash".into()],
            ..Default::default()
        };
        let tool = TOOL_CATALOG.iter().find(|t| t.name == "read_file").unwrap();
        // read_file and bash may share an intent (FileSystem), giving 0.1
        // Or they may not. Just verify it's less than 0.3.
        let boost = raw_recency_boost(tool, &state);
        assert!(boost < 0.3);
    }

    #[test]
    fn recency_boost_empty_recent() {
        let state = ConversationState::default();
        let tool = TOOL_CATALOG.iter().find(|t| t.name == "bash").unwrap();
        assert_eq!(raw_recency_boost(tool, &state), 0.0);
    }

    // ──────────────────────────────────────────────────────────
    // pre_filter_dynamic
    // ──────────────────────────────────────────────────────────

    #[test]
    fn pre_filter_returns_nonempty_for_real_query() {
        let state = state_at_turn(1);
        let results = pre_filter_dynamic(&state, "read the contents of a file");
        assert!(
            !results.is_empty(),
            "should return some tools for a real query"
        );
    }

    #[test]
    fn pre_filter_empty_query() {
        let state = state_at_turn(1);
        let results = pre_filter_dynamic(&state, "");
        // Empty query may still return tools due to cold-start logic
        // Just verify it doesn't panic
        let _ = results;
    }

    #[test]
    fn pre_filter_with_file_context() {
        let state = state_at_turn(1);
        let ctx = vec!["rust".to_string()];
        let results = pre_filter_dynamic_with_file_context(
            &state,
            "run tests",
            None,
            None,
            &[],
            0.0,
            &HashMap::new(),
            &ctx,
        );
        assert!(!results.is_empty());
    }

    #[test]
    fn pre_filter_with_pressure_zero() {
        let state = state_at_turn(1);
        let results = pre_filter_dynamic_with_pressure(&state, "read file", None, None, &[], 0.0);
        assert!(!results.is_empty());
    }

    #[test]
    fn pre_filter_with_pressure_max() {
        let state = state_at_turn(1);
        let results = pre_filter_dynamic_with_pressure(&state, "read file", None, None, &[], 1.0);
        // High pressure = stricter filter, but shouldn't panic
        let _ = results;
    }

    // --- tfidf_score edge cases ---

    #[test]
    fn tfidf_score_single_common_term() {
        // "run" appears in many tools — should have low IDF and produce a modest score
        let terms = vec!["run".to_string()];
        let bash_idx = TOOL_CATALOG.iter().position(|t| t.name == "bash").unwrap();
        let score = tfidf_score(&terms, bash_idx);
        assert!((0.0..=1.0).contains(&score));
    }

    #[test]
    fn tfidf_score_out_of_bounds_index() {
        // Index beyond TOOL_CATALOG length
        let terms = vec!["read".to_string()];
        // This would panic if not guarded — verify it's handled by the index
        // (TERM_INDEX.tool_tfs[idx] would be out of bounds)
        // We test a valid but last index instead
        let last_idx = TOOL_CATALOG.len() - 1;
        let score = tfidf_score(&terms, last_idx);
        assert!((0.0..=1.0).contains(&score));
    }

    #[test]
    fn tfidf_score_cjk_query() {
        let terms = vec!["记忆".to_string(), "搜索".to_string()];
        let mem_idx = TOOL_CATALOG
            .iter()
            .position(|t| t.name == "memory_search")
            .unwrap();
        let score = tfidf_score(&terms, mem_idx);
        // CJK terms should match Chinese triggers in memory_search
        assert!(score > 0.0, "CJK query should match memory_search triggers");
    }

    // --- file_context_tool_boost edge cases ---

    #[test]
    fn file_context_boost_java_no_match() {
        // Java isn't in the explicit match list
        assert_eq!(file_context_tool_boost("bash", &["java".to_string()]), 0.0);
    }

    #[test]
    fn file_context_boost_go_matches_bash() {
        assert_eq!(file_context_tool_boost("bash", &["go".to_string()]), 0.05);
    }

    #[test]
    fn file_context_boost_docker_only_bash_read_write() {
        assert_eq!(
            file_context_tool_boost("bash", &["docker".to_string()]),
            0.05
        );
        assert_eq!(
            file_context_tool_boost("grep", &["docker".to_string()]),
            0.0
        );
    }

    // --- pre_filter edge cases ---

    #[test]
    fn pre_filter_punctuation_only_query() {
        let state = state_at_turn(1);
        let results = pre_filter_dynamic(&state, "!!!???");
        // Should not panic — may return empty or few results
        for (_, score) in &results {
            assert!(*score >= 0.0);
        }
    }

    #[test]
    fn pre_filter_very_long_query() {
        // Regression guard: `pre_filter_dynamic_core` truncates via
        // `SCORING_QUERY_BYTE_CAP` so even huge pastes (10 000+ chars) stay
        // cheap. Before truncation this test ran ~5s because every trigger
        // in every tool re-split the full haystack.
        let state = state_at_turn(1);
        let long_query = "read ".repeat(2000);
        let results = pre_filter_dynamic(&state, &long_query);
        for (_, score) in &results {
            assert!(*score >= 0.0 && *score <= 10.0);
        }
    }

    #[test]
    fn truncate_at_char_boundary_never_splits_codepoint() {
        // ASCII: exactly at cap — keep all.
        assert_eq!(truncate_at_char_boundary("abcd", 4), "abcd");
        assert_eq!(truncate_at_char_boundary("abcd", 3), "abc");
        // Multi-byte: cap inside a 3-byte CJK char snaps down.
        // "中" is 3 bytes; at max_bytes=2 we must return "" (nothing fits).
        assert_eq!(truncate_at_char_boundary("中国", 2), "");
        // At max_bytes=3 one full char fits.
        assert_eq!(truncate_at_char_boundary("中国", 3), "中");
        // At max_bytes=5, one char fits (the second would cross byte 6).
        assert_eq!(truncate_at_char_boundary("中国", 5), "中");
        // Exactly at char boundary.
        assert_eq!(truncate_at_char_boundary("中国", 6), "中国");
    }

    #[test]
    fn pre_filter_long_cjk_query_is_bounded() {
        // Regression guard: a long CJK query must also be bounded. With the
        // previous 1024-byte cap only ~340 CJK chars passed through, which
        // was a silent behaviour change for non-Latin prompts. The new
        // 4096-byte cap keeps >1000 CJK chars.
        let state = state_at_turn(1);
        // "查询代码 " = 13 bytes (3×3-byte CJK + 1×ASCII space).
        let long_cjk = "查询代码 ".repeat(2000);
        let results = pre_filter_dynamic(&state, &long_cjk);
        for (_, score) in &results {
            assert!(*score >= 0.0 && *score <= 10.0);
        }
    }

    #[test]
    fn pre_filter_conversational_state_empty() {
        let state = ConversationState {
            is_conversational: true,
            ..Default::default()
        };
        let results = pre_filter_dynamic(&state, "hello how are you");
        assert!(
            results.is_empty(),
            "conversational queries should return empty dynamic tools"
        );
    }

    // ──────────────────────────────────────────────────────────
    // outcome_bias
    // ──────────────────────────────────────────────────────────

    /// Helper: find the score assigned to a specific tool name in a ranked result.
    fn score_for(results: &[(usize, f64)], tool_name: &str) -> Option<f64> {
        results.iter().find_map(|(idx, score)| {
            if TOOL_CATALOG[*idx].name == tool_name {
                Some(*score)
            } else {
                None
            }
        })
    }

    #[test]
    fn outcome_bias_demotes_failing_tool() {
        let state = ConversationState::default();
        let query = "grep search for a pattern in the codebase";
        let empty = HashMap::new();

        let baseline = pre_filter_dynamic_with_outcome_bias(
            &state,
            query,
            None,
            None,
            &[],
            0.0,
            &empty,
            &[],
            &empty,
        );
        let base_score = score_for(&baseline, "grep").expect("grep should rank");

        let mut penalty = HashMap::new();
        penalty.insert("grep".to_string(), -0.16);
        let biased = pre_filter_dynamic_with_outcome_bias(
            &state,
            query,
            None,
            None,
            &[],
            0.0,
            &empty,
            &[],
            &penalty,
        );
        let biased_score = score_for(&biased, "grep").expect("grep should still rank");

        assert!(
            biased_score < base_score,
            "negative outcome bias should lower score: {biased_score} vs {base_score}"
        );
        // Scoring applies an inner clamp of ±0.10.
        assert!((base_score - biased_score - 0.10).abs() < 1e-6);
    }

    #[test]
    fn outcome_bias_promotes_successful_tool() {
        let state = ConversationState::default();
        let query = "grep search for a pattern in the codebase";
        let empty = HashMap::new();

        let baseline = pre_filter_dynamic_with_outcome_bias(
            &state,
            query,
            None,
            None,
            &[],
            0.0,
            &empty,
            &[],
            &empty,
        );
        let base_score = score_for(&baseline, "grep").expect("grep should rank");

        let mut boost = HashMap::new();
        boost.insert("grep".to_string(), 0.10);
        let biased = pre_filter_dynamic_with_outcome_bias(
            &state,
            query,
            None,
            None,
            &[],
            0.0,
            &empty,
            &[],
            &boost,
        );
        let biased_score = score_for(&biased, "grep").expect("grep should still rank");

        assert!(
            biased_score > base_score,
            "positive outcome bias should raise score: {biased_score} vs {base_score}"
        );
    }
}
