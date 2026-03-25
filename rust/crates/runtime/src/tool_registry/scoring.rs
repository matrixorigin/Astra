use std::collections::HashMap;
use std::sync::LazyLock;

use super::meta::{IntentType, Scope, TOOL_CATALOG, ToolMeta};
use super::state::ConversationState;

/// Pre-computed inverse document frequency for each term across all tools.
/// Terms that appear in fewer tools get higher IDF (more discriminative).
struct TermIndex {
    /// term → IDF weight (log(N / df))
    idf: HashMap<String, f64>,
    /// tool_catalog_index → { term → normalized_tf }
    tool_tfs: Vec<HashMap<String, f64>>,
}

/// Tokenize text into lowercase terms, handling both CJK and ASCII.
/// CJK: emits both unigrams and bigrams for better phrase matching
/// (e.g., "记忆" → ["记", "忆", "记忆"]).
pub(crate) fn tokenize(text: &str) -> Vec<String> {
    let mut terms = Vec::new();
    let lower = text.to_lowercase();
    let mut ascii_buf = String::new();
    let mut cjk_chars: Vec<char> = Vec::new();

    for ch in lower.chars() {
        if ('\u{4e00}'..='\u{9fff}').contains(&ch) {
            // Flush ASCII buffer before CJK
            if !ascii_buf.is_empty() {
                for word in ascii_buf.split(|c: char| !c.is_alphanumeric() && c != '_') {
                    let w = word.trim();
                    if w.len() >= 2 {
                        terms.push(w.to_string());
                    }
                }
                ascii_buf.clear();
            }
            // Emit unigram
            terms.push(ch.to_string());
            // Emit bigram with previous CJK char
            if let Some(&prev) = cjk_chars.last() {
                terms.push(format!("{}{}", prev, ch));
            }
            cjk_chars.push(ch);
        } else {
            cjk_chars.clear(); // non-CJK breaks bigram chain
            ascii_buf.push(ch);
        }
    }
    // Flush remaining ASCII
    if !ascii_buf.is_empty() {
        for word in ascii_buf.split(|c: char| !c.is_alphanumeric() && c != '_') {
            let w = word.trim();
            if w.len() >= 2 {
                terms.push(w.to_string());
            }
        }
    }
    terms
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
/// Returns a score in [0.0, 1.0] — normalized by max possible score.
pub(crate) fn tfidf_score(query_terms: &[String], tool_idx: usize) -> f64 {
    let index = &*TERM_INDEX;
    let tool_tf = &index.tool_tfs[tool_idx];

    let mut dot_product = 0.0;
    let mut query_norm_sq = 0.0;

    for qt in query_terms {
        let idf = index.idf.get(qt).copied().unwrap_or(0.0);
        let q_weight = idf; // query TF is 1.0 (binary)
        query_norm_sq += q_weight * q_weight;

        if let Some(&doc_tf) = tool_tf.get(qt) {
            dot_product += q_weight * (doc_tf * idf);
        }
    }

    if query_norm_sq < f64::EPSILON {
        return 0.0;
    }
    // Normalize to [0, 1] range — use sqrt(query_norm) only for simplicity
    (dot_product / query_norm_sq.sqrt()).min(1.0)
}

// ─── Pre-filter: reorder dynamic tools by relevance ─────────────────────────

/// Score a tool's relevance to the current conversation state.
/// Combines TF-IDF textual similarity with intent/scope alignment.
/// Higher = more relevant. Range: 0.0 to 1.0.
fn tool_relevance_score(
    tool: &ToolMeta,
    tool_idx: usize,
    state: &ConversationState,
    query_terms: &[String],
) -> f64 {
    let mut score = 0.0;

    // TF-IDF textual similarity — replaces hardcoded trigger substring matching.
    // Uses pre-computed index over tool descriptions + triggers + names.
    let text_score = tfidf_score(query_terms, tool_idx);
    score += text_score * 0.6; // weight: 60% textual

    // Intent alignment with conversation state — 30% structural
    for intent in tool.intents {
        match intent {
            IntentType::GitHub if state.is_github => score += 0.25,
            IntentType::GitHub if state.is_fetch => score += 0.15,
            IntentType::Git if state.is_git => score += 0.25,
            IntentType::Git if state.is_fetch || state.references_history => score += 0.15,
            // Memory bonus: always apply when TF-IDF already scored this tool well
            // (i.e., query contains memory-related terms). Don't gate on state flags —
            // "我有哪些记忆？" is a memory query even without "之前" or "分析".
            IntentType::Memory if text_score > 0.05 => score += 0.2,
            IntentType::Memory if state.references_history || state.is_analytical => score += 0.15,
            IntentType::CodeEdit if state.is_mutate => score += 0.15,
            IntentType::CodeRead if state.is_fetch || state.is_analytical => score += 0.1,
            IntentType::Introspect if state.is_analytical => score += 0.15,
            _ => {}
        }
    }

    // Scope alignment
    match tool.scope {
        Scope::External if state.is_fetch && !state.is_mutate => score += 0.1,
        Scope::CrossSession if state.references_history => score += 0.1,
        _ => {}
    }

    // Recency boost: if this tool was used in a recent turn, it's likely still relevant.
    // This captures follow-up queries like "matrixone呢？" after a github_list_prs call.
    if state.recent_tools.iter().any(|r| r == tool.name) {
        score += 0.3;
    } else {
        // Same-category recency: if any recent tool shares an intent, boost slightly.
        for intent in tool.intents {
            let same_category = state.recent_tools.iter().any(|r| {
                TOOL_CATALOG
                    .iter()
                    .find(|t| t.name == r.as_str())
                    .is_some_and(|t| t.intents.contains(intent))
            });
            if same_category {
                score += 0.1;
                break;
            }
        }
    }

    score.min(1.0)
}

/// Pre-filter: rank dynamic tools by relevance and filter by minimum score threshold.
/// Returns (catalog_index, score) pairs for dynamic tools with score >= MIN_SCORE_THRESHOLD,
/// sorted by descending score. Falls back to top-3 by score if nothing clears the threshold.
pub fn pre_filter_dynamic(state: &ConversationState, query: &str) -> Vec<(usize, f64)> {
    // Short-circuit: pure conversational queries ("hi", "thanks") don't need dynamic tools.
    // The 7 pinned tools (bash, read_file, etc.) are always sent regardless.
    if state.is_conversational && !state.is_fetch && !state.is_mutate && !state.is_analytical {
        return vec![];
    }

    let query_terms = tokenize(query);
    let mut scored: Vec<(usize, f64)> = TOOL_CATALOG
        .iter()
        .enumerate()
        .filter(|(_, t)| !t.pinned)
        .map(|(idx, tool)| {
            let score = tool_relevance_score(tool, idx, state, &query_terms);
            (idx, score)
        })
        .collect();

    // Sort by descending score, then by catalog order for ties
    scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));

    // Apply minimum score threshold, with fallback to top-3 if nothing qualifies.
    let above_threshold: Vec<_> = scored
        .iter()
        .filter(|(_, s)| *s >= MIN_SCORE_THRESHOLD)
        .copied()
        .collect();

    if above_threshold.is_empty() {
        // Fallback: always offer the 3 highest-scoring tools even below threshold.
        scored.truncate(3);
        scored
    } else {
        above_threshold
    }
}

// ─── Budget gate ────────────────────────────────────────────────────────────

/// Default token budget for tool schemas in the context window.
/// Sized to select ~4-6 dynamic tools on a typical query; forces real scoring.
pub const DEFAULT_TOOL_BUDGET_TOKENS: u32 = 800;

/// Minimum relevance score a dynamic tool must exceed to be considered.
/// Tools scoring below this threshold are excluded even if budget allows.
const MIN_SCORE_THRESHOLD: f64 = 0.05;
