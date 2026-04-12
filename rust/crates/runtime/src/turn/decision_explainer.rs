//! Decision Explainer — provides human-readable explanations for agent decisions.
//!
//! This module helps answer "why" questions:
//! - Why were these tools selected?
//! - Why was this history compressed?
//! - Why did the agent lose focus?
//!
//! Each explanation includes:
//! - Inputs that led to the decision
//! - The reasoning process
//! - Alternatives that were considered
//! - Confidence level

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::time::SystemTime;

use astra_core::{ConfidenceInterval, DriftCause, DriftEvidence, EvidenceType};

use super::context_assembly_trace::{MemoryRetrievalTrace, TokenBudgetTrace};

// ─── Decision Explanation ────────────────────────────────────────────────────

/// A structured explanation for an agent decision.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DecisionExplanation {
    /// Unique ID for this explanation.
    pub id: String,

    /// When this decision was made.
    pub timestamp: SystemTime,

    /// Type of decision being explained.
    pub decision_type: DecisionType,

    /// Inputs that influenced this decision.
    pub inputs: Vec<ExplainableInput>,

    /// Human-readable reasoning.
    pub reasoning: String,

    /// Alternative options that were considered.
    pub alternatives: Vec<Alternative>,

    /// Confidence in this decision (0.0-1.0).
    pub confidence: f64,
}

/// Types of decisions that can be explained.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type")]
pub enum DecisionType {
    /// Tool selection decision.
    ToolSelection {
        selected_tools: Vec<String>,
        total_available: u32,
    },

    /// History compression decision.
    HistoryCompression {
        turns_compressed: Vec<u32>,
        turns_retained: Vec<u32>,
        compression_ratio: f64,
    },

    /// Memory retrieval decision.
    MemoryRetrieval {
        memories_selected: Vec<String>,
        total_candidates: u32,
    },

    /// Strategy choice (e.g., compression strategy).
    StrategyChoice {
        strategy: String,
        available_strategies: Vec<String>,
    },

    /// Model routing decision.
    ModelRouting {
        selected_model: String,
        routing_reason: String,
    },
}

/// An input that influenced a decision.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExplainableInput {
    /// Name of this input factor.
    pub name: String,

    /// Value of this input.
    pub value: String,

    /// How much this input influenced the decision (0.0-1.0).
    pub influence: f64,

    /// Brief explanation of why this matters.
    pub explanation: Option<String>,
}

/// An alternative that was considered but not chosen.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Alternative {
    /// Description of the alternative.
    pub description: String,

    /// Why it was not chosen.
    pub rejection_reason: String,

    /// Score or ranking of this alternative.
    pub score: Option<f64>,
}

impl DecisionExplanation {
    /// Create a new decision explanation.
    pub fn new(decision_type: DecisionType, reasoning: impl Into<String>) -> Self {
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            timestamp: SystemTime::now(),
            decision_type,
            inputs: Vec::new(),
            reasoning: reasoning.into(),
            alternatives: Vec::new(),
            confidence: 0.5,
        }
    }

    /// Add an input factor.
    pub fn with_input(
        mut self,
        name: impl Into<String>,
        value: impl Into<String>,
        influence: f64,
    ) -> Self {
        self.inputs.push(ExplainableInput {
            name: name.into(),
            value: value.into(),
            influence,
            explanation: None,
        });
        self
    }

    /// Add an input factor with explanation.
    pub fn with_input_explained(
        mut self,
        name: impl Into<String>,
        value: impl Into<String>,
        influence: f64,
        explanation: impl Into<String>,
    ) -> Self {
        self.inputs.push(ExplainableInput {
            name: name.into(),
            value: value.into(),
            influence,
            explanation: Some(explanation.into()),
        });
        self
    }

    /// Add an alternative that was considered.
    pub fn with_alternative(
        mut self,
        description: impl Into<String>,
        rejection_reason: impl Into<String>,
        score: Option<f64>,
    ) -> Self {
        self.alternatives.push(Alternative {
            description: description.into(),
            rejection_reason: rejection_reason.into(),
            score,
        });
        self
    }

    /// Set confidence level.
    pub fn with_confidence(mut self, confidence: f64) -> Self {
        self.confidence = confidence.clamp(0.0, 1.0);
        self
    }

    /// Format as human-readable text.
    pub fn to_human_readable(&self) -> String {
        let mut lines = Vec::new();

        // Header
        lines.push(format!("## Decision: {}", self.decision_type_name()));
        lines.push(format!("Confidence: {:.0}%", self.confidence * 100.0));
        lines.push(String::new());

        // Reasoning
        lines.push("### Reasoning".to_string());
        lines.push(self.reasoning.clone());
        lines.push(String::new());

        // Key inputs
        if !self.inputs.is_empty() {
            lines.push("### Key Factors".to_string());
            for input in &self.inputs {
                let influence = format!("({:.0}% influence)", input.influence * 100.0);
                lines.push(format!(
                    "- **{}**: {} {}",
                    input.name, input.value, influence
                ));
                if let Some(ref exp) = input.explanation {
                    lines.push(format!("  → {}", exp));
                }
            }
            lines.push(String::new());
        }

        // Alternatives
        if !self.alternatives.is_empty() {
            lines.push("### Alternatives Considered".to_string());
            for alt in &self.alternatives {
                let score = alt
                    .score
                    .map(|s| format!(" (score: {:.2})", s))
                    .unwrap_or_default();
                lines.push(format!("- **{}**{}", alt.description, score));
                lines.push(format!("  → Rejected: {}", alt.rejection_reason));
            }
        }

        lines.join("\n")
    }

    fn decision_type_name(&self) -> &str {
        match &self.decision_type {
            DecisionType::ToolSelection { .. } => "Tool Selection",
            DecisionType::HistoryCompression { .. } => "History Compression",
            DecisionType::MemoryRetrieval { .. } => "Memory Retrieval",
            DecisionType::StrategyChoice { .. } => "Strategy Choice",
            DecisionType::ModelRouting { .. } => "Model Routing",
        }
    }
}

// ─── Focus Drift Analysis ────────────────────────────────────────────────────

/// Analysis of focus drift (when the agent loses track of the original task).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FocusDriftAnalysis {
    /// Whether drift was detected.
    pub drift_detected: bool,

    /// Turn where drift likely started.
    pub drift_turn: Option<u32>,

    /// Severity of the drift (0.0-1.0).
    pub drift_severity: f64,

    /// Most likely cause of the drift.
    pub likely_cause: DriftCause,

    /// Context that was affected.
    pub affected_context: Vec<String>,

    /// Suggested recovery action.
    pub recovery_suggestion: String,

    /// Evidence supporting this analysis.
    pub evidence: Vec<DriftEvidence>,
}

impl FocusDriftAnalysis {
    /// Create a "no drift" analysis.
    pub fn no_drift() -> Self {
        Self {
            drift_detected: false,
            drift_turn: None,
            drift_severity: 0.0,
            likely_cause: DriftCause::Unknown,
            affected_context: Vec::new(),
            recovery_suggestion: "No recovery needed - conversation is on track.".to_string(),
            evidence: Vec::new(),
        }
    }

    /// Create a drift analysis with detected cause.
    pub fn detected(cause: DriftCause, severity: f64, turn: u32) -> Self {
        let recovery_suggestion = suggest_recovery(&cause);
        Self {
            drift_detected: true,
            drift_turn: Some(turn),
            drift_severity: severity.clamp(0.0, 1.0),
            likely_cause: cause,
            affected_context: Vec::new(),
            recovery_suggestion,
            evidence: Vec::new(),
        }
    }

    /// Add evidence.
    pub fn with_evidence(
        mut self,
        turn: u32,
        evidence_type: EvidenceType,
        description: impl Into<String>,
        confidence: f64,
    ) -> Self {
        self.evidence.push(DriftEvidence {
            turn,
            evidence_type,
            description: description.into(),
            confidence: ConfidenceInterval::exact(confidence),
        });
        self
    }

    /// Add affected context.
    pub fn with_affected_context(mut self, context: Vec<String>) -> Self {
        self.affected_context = context;
        self
    }

    /// Format as human-readable text.
    pub fn to_human_readable(&self) -> String {
        let mut lines = Vec::new();

        if !self.drift_detected {
            lines.push("✅ No focus drift detected.".to_string());
            return lines.join("\n");
        }

        lines.push(format!(
            "⚠️ Focus Drift Detected (severity: {:.0}%)",
            self.drift_severity * 100.0
        ));
        if let Some(turn) = self.drift_turn {
            lines.push(format!("Started at turn: {}", turn));
        }
        lines.push(String::new());

        lines.push("### Likely Cause".to_string());
        lines.push(format_cause(&self.likely_cause));
        lines.push(String::new());

        if !self.affected_context.is_empty() {
            lines.push("### Affected Context".to_string());
            for ctx in &self.affected_context {
                lines.push(format!("- {}", ctx));
            }
            lines.push(String::new());
        }

        if !self.evidence.is_empty() {
            lines.push("### Evidence".to_string());
            for ev in &self.evidence {
                lines.push(format!(
                    "- Turn {}: [{:?}] {} ({:.0}% confidence)",
                    ev.turn,
                    ev.evidence_type,
                    ev.description,
                    ev.confidence.point * 100.0
                ));
            }
            lines.push(String::new());
        }

        lines.push("### Recovery Suggestion".to_string());
        lines.push(self.recovery_suggestion.clone());

        lines.join("\n")
    }
}

fn suggest_recovery(cause: &DriftCause) -> String {
    match cause {
        DriftCause::HistoryCompression { .. } => {
            "Consider asking the user to restate their original goal, or review earlier turns for context.".to_string()
        }
        DriftCause::MemoryMiss { .. } => {
            "Try a broader memory search query, or ask the user about relevant prior context.".to_string()
        }
        DriftCause::TopicShift { original_topic, .. } => {
            format!("Return focus to: '{}'. Ask user if topic change was intentional.", original_topic)
        }
        DriftCause::TokenBudgetPressure { .. } => {
            "Increase token budget or use more aggressive summarization to preserve key context.".to_string()
        }
        DriftCause::AmbiguousInstruction { .. } => {
            "Ask clarifying questions to disambiguate the user's intent.".to_string()
        }
        DriftCause::Unknown => {
            "Review recent turns for context clues, or ask user to clarify their current goal.".to_string()
        }
    }
}

fn format_cause(cause: &DriftCause) -> String {
    match cause {
        DriftCause::HistoryCompression {
            lost_context,
            compression_turn,
        } => {
            format!(
                "History compression at turn {} removed important context: {}",
                compression_turn,
                lost_context.join(", ")
            )
        }
        DriftCause::MemoryMiss {
            expected_but_not_retrieved,
            query_used,
        } => {
            format!(
                "Memory query '{}' failed to retrieve expected memories: {}",
                query_used,
                expected_but_not_retrieved.join(", ")
            )
        }
        DriftCause::TopicShift {
            original_topic,
            new_topic,
            shift_turn,
        } => {
            format!(
                "Topic shifted at turn {} from '{}' to '{}'",
                shift_turn, original_topic, new_topic
            )
        }
        DriftCause::TokenBudgetPressure {
            budget_available,
            budget_needed,
            sacrificed_context,
        } => {
            format!(
                "Token budget pressure (had {}, needed {}) forced sacrifice of: {}",
                budget_available,
                budget_needed,
                sacrificed_context.join(", ")
            )
        }
        DriftCause::AmbiguousInstruction {
            instruction,
            interpretations,
        } => {
            format!(
                "Ambiguous instruction '{}' could mean: {}",
                instruction,
                interpretations.join(" OR ")
            )
        }
        DriftCause::Unknown => "Unable to determine specific cause.".to_string(),
    }
}

// ─── Drift Detector ──────────────────────────────────────────────────────────

/// Detects focus drift by analyzing conversation patterns.
pub struct DriftDetector {
    /// Minimum severity to report drift.
    pub min_severity_threshold: f64,
    /// Number of recent turns to analyze.
    pub analysis_window: u32,
}

impl Default for DriftDetector {
    fn default() -> Self {
        Self {
            min_severity_threshold: 0.3,
            analysis_window: 10,
        }
    }
}

impl DriftDetector {
    /// Analyze a conversation for drift.
    ///
    /// This is a simplified heuristic-based detector.
    /// A more sophisticated version would use embeddings and semantic similarity.
    pub fn analyze(
        &self,
        original_query: &str,
        recent_queries: &[String],
        compressed_turns: &[u32],
        user_corrections: &[u32],
    ) -> FocusDriftAnalysis {
        let mut evidence = Vec::new();
        let mut severity = 0.0;

        // Check for user corrections (strong signal)
        if !user_corrections.is_empty() {
            let last_correction = user_corrections.last().copied().unwrap_or(0);
            evidence.push(DriftEvidence {
                turn: last_correction,
                evidence_type: EvidenceType::UserCorrection,
                description: "User provided correction/redirection".to_string(),
                confidence: ConfidenceInterval::exact(0.8),
            });
            severity += 0.4;
        }

        // Check for topic divergence (TF-IDF cosine similarity)
        let original_keywords = extract_keywords(original_query);
        for (i, query) in recent_queries.iter().enumerate() {
            let sim = query_similarity(original_query, query);
            if sim < 0.15 {
                let query_keywords = extract_keywords(query);
                let overlap = keyword_overlap(&original_keywords, &query_keywords);
                evidence.push(DriftEvidence {
                    turn: i as u32,
                    evidence_type: EvidenceType::ToolCallTopicChange,
                    description: format!(
                        "Low similarity ({:.0}%) with original query (keyword overlap: {:.0}%)",
                        sim * 100.0,
                        overlap * 100.0
                    ),
                    confidence: ConfidenceInterval::exact(0.7),
                });
                severity += 0.2;
            }
        }

        // Check if compression removed many turns
        if compressed_turns.len() > 5 {
            evidence.push(DriftEvidence {
                turn: compressed_turns[0],
                evidence_type: EvidenceType::CompressionLoss,
                description: format!("{} turns were compressed", compressed_turns.len()),
                confidence: ConfidenceInterval::exact(0.5),
            });
            severity += 0.15;
        }

        // Determine if drift is significant
        let drift_detected = severity >= self.min_severity_threshold;

        if drift_detected {
            // Determine most likely cause
            let likely_cause = if !user_corrections.is_empty() {
                DriftCause::TopicShift {
                    original_topic: original_query.chars().take(100).collect(),
                    new_topic: recent_queries.last().cloned().unwrap_or_default(),
                    shift_turn: user_corrections.last().copied().unwrap_or(0),
                }
            } else if compressed_turns.len() > 5 {
                DriftCause::HistoryCompression {
                    lost_context: vec!["Multiple turns compressed".to_string()],
                    compression_turn: compressed_turns[0],
                }
            } else {
                DriftCause::Unknown
            };

            let recovery_suggestion = suggest_recovery(&likely_cause);
            FocusDriftAnalysis {
                drift_detected: true,
                drift_turn: evidence.first().map(|e| e.turn),
                drift_severity: severity.min(1.0),
                likely_cause,
                affected_context: Vec::new(),
                recovery_suggestion,
                evidence,
            }
        } else {
            FocusDriftAnalysis::no_drift()
        }
    }

    /// Analyze with additional context from trace data.
    ///
    /// Extends `analyze()` with three additional heuristics that consume
    /// `ContextAssemblyTrace` fields:
    ///
    /// - **MemoryMiss**: memory retrieval returned 0 results or all scores < 0.3
    /// - **TokenBudgetPressure**: budget_pressure > 0.85
    /// - **AmbiguousInstruction**: consecutive corrections on similar queries
    pub fn analyze_with_context(
        &self,
        original_query: &str,
        recent_queries: &[String],
        compressed_turns: &[u32],
        user_corrections: &[u32],
        memory_traces: &[MemoryRetrievalTrace],
        budget_traces: &[TokenBudgetTrace],
    ) -> FocusDriftAnalysis {
        // Start with the base analysis
        let mut base = self.analyze(
            original_query,
            recent_queries,
            compressed_turns,
            user_corrections,
        );

        // Even if base says "no drift", the additional signals can push it over
        let mut extra_severity = 0.0_f64;
        let mut extra_evidence = Vec::new();
        let mut memory_miss_detected = false;
        let mut budget_pressure_detected = false;

        // ── MemoryMiss: check if memory retrieval failed ────────────────
        for (i, trace) in memory_traces.iter().enumerate() {
            let no_candidates = trace.candidates_considered == 0;
            let all_low_relevance = !trace.memories_selected.is_empty()
                && trace
                    .memories_selected
                    .iter()
                    .all(|m| m.relevance_score < 0.3);
            let empty_selection =
                trace.candidates_considered > 0 && trace.memories_selected.is_empty();

            if no_candidates || all_low_relevance || empty_selection {
                let desc = if no_candidates {
                    format!(
                        "Memory retrieval returned 0 candidates for \"{}\"",
                        ellipsize_str(&trace.query, 40)
                    )
                } else if all_low_relevance {
                    let max_score = trace
                        .memories_selected
                        .iter()
                        .map(|m| m.relevance_score)
                        .fold(0.0_f64, f64::max);
                    format!(
                        "All retrieved memories have low relevance (max {:.0}%)",
                        max_score * 100.0
                    )
                } else {
                    format!(
                        "{} candidates considered, none selected",
                        trace.candidates_considered
                    )
                };

                extra_evidence.push(DriftEvidence {
                    turn: i as u32,
                    evidence_type: EvidenceType::MemoryMismatch,
                    description: desc,
                    confidence: ConfidenceInterval::exact(0.6),
                });
                if !memory_miss_detected {
                    extra_severity += 0.2;
                    memory_miss_detected = true;
                }
            }
        }

        // ── TokenBudgetPressure: check if budget is critically tight ────
        for (i, trace) in budget_traces.iter().enumerate() {
            if trace.budget_pressure > 0.85 {
                let sacrificed = if trace.compression_triggered {
                    "history compressed"
                } else {
                    "context may be truncated"
                };
                extra_evidence.push(DriftEvidence {
                    turn: i as u32,
                    evidence_type: EvidenceType::TermDisappearance,
                    description: format!(
                        "Token budget pressure {:.0}% — {sacrificed}",
                        trace.budget_pressure * 100.0
                    ),
                    confidence: ConfidenceInterval::exact(0.7),
                });
                if !budget_pressure_detected {
                    extra_severity += 0.25;
                    budget_pressure_detected = true;
                }
            }
        }

        // ── AmbiguousInstruction: consecutive corrections on similar queries ─
        if user_corrections.len() >= 2 {
            let corrections_sorted = {
                let mut v = user_corrections.to_vec();
                v.sort();
                v
            };
            for window in corrections_sorted.windows(2) {
                let (t1, t2) = (window[0], window[1]);
                // Consecutive or near-consecutive corrections
                if t2 - t1 <= 2 {
                    let q1 = recent_queries.get(t1 as usize);
                    let q2 = recent_queries.get(t2 as usize);
                    if let (Some(q1), Some(q2)) = (q1, q2) {
                        let sim = query_similarity(q1, q2);
                        if sim > 0.4 {
                            extra_evidence.push(DriftEvidence {
                                turn: t2,
                                evidence_type: EvidenceType::ClarificationRequest,
                                description: format!(
                                    "Repeated corrections on similar queries (similarity {:.0}%)",
                                    sim * 100.0
                                ),
                                confidence: ConfidenceInterval::exact(0.75),
                            });
                            extra_severity += 0.3;
                            break; // One ambiguity signal is enough
                        }
                    }
                }
            }
        }

        // Merge extra evidence into base analysis
        if !extra_evidence.is_empty() {
            let total_severity = (base.drift_severity + extra_severity).min(1.0);
            base.evidence.extend(extra_evidence);
            base.drift_severity = total_severity;

            // Re-evaluate drift detection with augmented severity
            if !base.drift_detected && total_severity >= self.min_severity_threshold {
                base.drift_detected = true;
                base.drift_turn = base.evidence.first().map(|e| e.turn);

                // Determine likely cause from new evidence
                base.likely_cause = if memory_miss_detected {
                    let query = memory_traces
                        .iter()
                        .find(|t| t.candidates_considered == 0 || t.memories_selected.is_empty())
                        .map(|t| t.query.clone())
                        .unwrap_or_default();
                    DriftCause::MemoryMiss {
                        expected_but_not_retrieved: vec![ellipsize_str(&query, 80)],
                        query_used: query,
                    }
                } else if budget_pressure_detected {
                    if let Some(trace) = budget_traces.iter().find(|t| t.budget_pressure > 0.85) {
                        DriftCause::TokenBudgetPressure {
                            budget_available: trace.max_tokens.saturating_sub(trace.total_used),
                            budget_needed: trace.total_used,
                            sacrificed_context: if trace.compression_triggered {
                                vec!["History compressed under budget pressure".to_string()]
                            } else {
                                vec![]
                            },
                        }
                    } else {
                        DriftCause::AmbiguousInstruction {
                            instruction: original_query.chars().take(80).collect(),
                            interpretations: vec![
                                "User corrected the same intent multiple times".to_string(),
                            ],
                        }
                    }
                } else {
                    DriftCause::AmbiguousInstruction {
                        instruction: original_query.chars().take(80).collect(),
                        interpretations: vec![
                            "User corrected the same intent multiple times".to_string(),
                        ],
                    }
                };
                base.recovery_suggestion = suggest_recovery(&base.likely_cause);
            }
        }

        base
    }
}

fn ellipsize_str(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let truncated: String = s.chars().take(max.saturating_sub(1)).collect();
        format!("{truncated}…")
    }
}
///
/// Uses the shared CJK-aware tokenizer (`text_tokenize`) which handles both
/// English (with stemming) and Chinese (unigrams + bigrams).  Unlike the older
/// `keyword_overlap`, this captures semantic proximity rather than exact keyword
/// matches, and works correctly for non-Latin scripts.
///
/// Returns 0.0-1.0.  Two identical queries return 1.0; completely disjoint
/// vocabularies return 0.0.
fn query_similarity(a: &str, b: &str) -> f64 {
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }
    if a == b {
        return 1.0;
    }
    let tokens_a = crate::text_tokenize::tokenize(a);
    let tokens_b = crate::text_tokenize::tokenize(b);
    if tokens_a.is_empty() || tokens_b.is_empty() {
        return 0.0;
    }
    let tf_a = crate::text_tokenize::build_tf(&tokens_a);
    let tf_b = crate::text_tokenize::build_tf(&tokens_b);
    cosine_sim(&tf_a, &tf_b)
}

/// Cosine similarity between two TF vectors.
fn cosine_sim(a: &HashMap<String, f64>, b: &HashMap<String, f64>) -> f64 {
    let mut dot = 0.0_f64;
    let mut norm_a = 0.0_f64;
    let mut norm_b = 0.0_f64;
    for (term, &c1) in a {
        norm_a += c1 * c1;
        if let Some(&c2) = b.get(term) {
            dot += c1 * c2;
        }
    }
    for &c2 in b.values() {
        norm_b += c2 * c2;
    }
    let denom = norm_a.sqrt() * norm_b.sqrt();
    if denom < 1e-9 {
        0.0
    } else {
        (dot / denom).min(1.0)
    }
}

/// Extract simple keywords from text (lowercase, 4+ chars).
/// Kept for backward compatibility and evidence description.
fn extract_keywords(text: &str) -> Vec<String> {
    text.split_whitespace()
        .map(|w| {
            w.to_lowercase()
                .chars()
                .filter(|c| c.is_alphanumeric())
                .collect::<String>()
        })
        .filter(|w| w.len() >= 4)
        .collect()
}

/// Calculate keyword overlap ratio.
fn keyword_overlap(a: &[String], b: &[String]) -> f64 {
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }
    let matches = a.iter().filter(|kw| b.contains(kw)).count();
    matches as f64 / a.len().max(b.len()) as f64
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_decision_explanation_builder() {
        let exp = DecisionExplanation::new(
            DecisionType::ToolSelection {
                selected_tools: vec!["grep".to_string(), "view".to_string()],
                total_available: 50,
            },
            "Selected grep and view based on query pattern matching code search.",
        )
        .with_input("query", "find the auth module", 0.8)
        .with_input_explained("file_type", "*.rs", 0.3, "Rust files targeted")
        .with_alternative("ripgrep CLI", "grep tool handles this case", Some(0.6))
        .with_confidence(0.85);

        assert_eq!(exp.confidence, 0.85);
        assert_eq!(exp.inputs.len(), 2);
        assert_eq!(exp.alternatives.len(), 1);
    }

    #[test]
    fn test_drift_analysis_no_drift() {
        let analysis = FocusDriftAnalysis::no_drift();
        assert!(!analysis.drift_detected);
        assert!(analysis.drift_turn.is_none());
    }

    #[test]
    fn test_drift_analysis_detected() {
        let analysis = FocusDriftAnalysis::detected(
            DriftCause::HistoryCompression {
                lost_context: vec!["user goal".to_string()],
                compression_turn: 5,
            },
            0.7,
            5,
        )
        .with_evidence(
            5,
            EvidenceType::CompressionLoss,
            "Turns 1-4 compressed",
            0.8,
        )
        .with_affected_context(vec!["Original task description".to_string()]);

        assert!(analysis.drift_detected);
        assert_eq!(analysis.drift_turn, Some(5));
        assert_eq!(analysis.evidence.len(), 1);
    }

    #[test]
    fn test_drift_detector_no_drift() {
        let detector = DriftDetector::default();
        let analysis = detector.analyze(
            "implement user authentication",
            &[
                "implement auth module".to_string(),
                "add login function".to_string(),
            ],
            &[],
            &[],
        );
        assert!(!analysis.drift_detected);
    }

    #[test]
    fn test_drift_detector_with_correction() {
        let detector = DriftDetector::default();
        let analysis = detector.analyze(
            "implement user authentication",
            &["configure database".to_string()],
            &[],
            &[3], // User correction at turn 3
        );
        assert!(analysis.drift_detected);
        assert!(analysis.drift_severity >= 0.3);
    }

    #[test]
    fn test_keyword_extraction() {
        let keywords = extract_keywords("The quick brown fox jumps");
        assert!(keywords.contains(&"quick".to_string()));
        assert!(keywords.contains(&"brown".to_string()));
        assert!(keywords.contains(&"jumps".to_string()));
        assert!(!keywords.contains(&"the".to_string())); // too short
    }

    #[test]
    fn test_keyword_overlap() {
        let a = vec![
            "quick".to_string(),
            "brown".to_string(),
            "jumps".to_string(),
        ];
        let b = vec!["quick".to_string(), "lazy".to_string(), "dogs".to_string()];
        let overlap = keyword_overlap(&a, &b);
        assert!(overlap > 0.0 && overlap < 1.0);
    }

    #[test]
    fn test_human_readable_explanation() {
        let exp = DecisionExplanation::new(
            DecisionType::ToolSelection {
                selected_tools: vec!["grep".to_string()],
                total_available: 10,
            },
            "Grep selected for code search.",
        )
        .with_confidence(0.9);

        let text = exp.to_human_readable();
        assert!(text.contains("Tool Selection"));
        assert!(text.contains("90%"));
    }

    #[test]
    fn test_human_readable_drift() {
        let analysis = FocusDriftAnalysis::detected(
            DriftCause::TopicShift {
                original_topic: "auth".to_string(),
                new_topic: "database".to_string(),
                shift_turn: 5,
            },
            0.6,
            5,
        );

        let text = analysis.to_human_readable();
        assert!(text.contains("Focus Drift Detected"));
        assert!(text.contains("60%"));
    }

    #[test]
    fn test_query_similarity_identical() {
        let sim = query_similarity("implement user auth", "implement user auth");
        assert!((sim - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_query_similarity_related_queries() {
        // Related queries should have high similarity
        let sim = query_similarity("implement user authentication", "implement auth module");
        assert!(sim > 0.3, "related queries should be similar: {sim}");
    }

    #[test]
    fn test_query_similarity_unrelated_queries() {
        // Completely different topics should have low similarity
        let sim = query_similarity(
            "implement user authentication",
            "configure kubernetes deployment",
        );
        assert!(
            sim < 0.15,
            "unrelated queries should have low similarity: {sim}"
        );
    }

    #[test]
    fn test_query_similarity_chinese() {
        // CJK queries should work via bigram tokenization
        let sim = query_similarity("实现用户认证功能", "实现用户登录");
        assert!(
            sim > 0.2,
            "related Chinese queries should be similar: {sim}"
        );

        let sim2 = query_similarity("实现用户认证", "配置数据库连接");
        assert!(sim2 < sim, "unrelated Chinese should be less similar");
    }

    #[test]
    fn test_query_similarity_empty() {
        assert_eq!(query_similarity("", "something"), 0.0);
        assert_eq!(query_similarity("something", ""), 0.0);
        assert_eq!(query_similarity("", ""), 0.0);
    }

    #[test]
    fn test_drift_detector_topic_divergence() {
        // Multiple unrelated queries (2 × 0.2 = 0.4 severity → above 0.3 threshold)
        let detector = DriftDetector::default();
        let original = "implement user authentication";
        let analysis = detector.analyze(
            original,
            &[
                "configure kubernetes deployment pipeline".to_string(),
                "setup monitoring with prometheus".to_string(),
            ],
            &[],
            &[],
        );
        assert!(
            analysis.drift_detected,
            "two unrelated queries should trigger drift"
        );
        let topic_changes = analysis
            .evidence
            .iter()
            .filter(|e| matches!(e.evidence_type, EvidenceType::ToolCallTopicChange))
            .count();
        assert!(
            topic_changes >= 2,
            "should have evidence for both divergent queries, got {topic_changes}"
        );
    }

    // ── analyze_with_context() tests ────────────────────────────────────

    fn make_memory_trace(query: &str, candidates: u32, scores: &[f64]) -> MemoryRetrievalTrace {
        use super::super::context_assembly_trace::*;
        MemoryRetrievalTrace {
            query: query.to_string(),
            candidates_considered: candidates,
            memories_selected: scores
                .iter()
                .map(|&s| MemorySelection {
                    memory_id: "m1".to_string(),
                    memory_type: "semantic".to_string(),
                    content_preview: "test".to_string(),
                    relevance_score: s,
                    tokens: 50,
                    source: MemorySource::Memoria,
                })
                .collect(),
            memories_rejected: vec![],
            total_tokens: 50 * scores.len() as u32,
            retrieval_latency_ms: 10,
        }
    }

    fn make_budget_trace(pressure: f64, compressed: bool) -> TokenBudgetTrace {
        TokenBudgetTrace {
            max_tokens: 100_000,
            system_prompt_tokens: 5_000,
            history_tokens: 20_000,
            memory_tokens: 5_000,
            tool_schema_tokens: 10_000,
            user_message_tokens: 1_000,
            total_used: (100_000.0 * pressure) as u32,
            budget_pressure: pressure,
            compression_triggered: compressed,
        }
    }

    #[test]
    fn test_analyze_with_context_memory_miss_no_candidates() {
        let detector = DriftDetector::default();
        // A related query (no base drift) + empty memory retrieval
        let analysis = detector.analyze_with_context(
            "implement user auth",
            &["implement login flow".to_string()],
            &[],
            &[],
            &[make_memory_trace("user auth", 0, &[])],
            &[],
        );
        // MemoryMiss alone adds 0.2 severity — below 0.3 threshold
        assert!(
            !analysis.drift_detected,
            "memory miss alone (0.2) should not cross 0.3 threshold"
        );
        // But the evidence should be present
        let mem_evidence = analysis
            .evidence
            .iter()
            .any(|e| matches!(e.evidence_type, EvidenceType::MemoryMismatch));
        assert!(mem_evidence, "should have MemoryMismatch evidence");
    }

    #[test]
    fn test_analyze_with_context_memory_miss_low_relevance() {
        let detector = DriftDetector::default();
        let analysis = detector.analyze_with_context(
            "implement user auth",
            &["implement login flow".to_string()],
            &[],
            &[],
            &[make_memory_trace("user auth", 5, &[0.1, 0.2])], // all < 0.3
            &[],
        );
        let mem_evidence = analysis
            .evidence
            .iter()
            .any(|e| matches!(e.evidence_type, EvidenceType::MemoryMismatch));
        assert!(
            mem_evidence,
            "low relevance scores should trigger MemoryMismatch"
        );
    }

    #[test]
    fn test_analyze_with_context_memory_hit_no_evidence() {
        let detector = DriftDetector::default();
        let analysis = detector.analyze_with_context(
            "implement user auth",
            &["implement login flow".to_string()],
            &[],
            &[],
            &[make_memory_trace("user auth", 5, &[0.8, 0.6])], // above 0.3
            &[],
        );
        let mem_evidence = analysis
            .evidence
            .iter()
            .any(|e| matches!(e.evidence_type, EvidenceType::MemoryMismatch));
        assert!(
            !mem_evidence,
            "good relevance should not trigger MemoryMismatch"
        );
    }

    #[test]
    fn test_analyze_with_context_budget_pressure() {
        let detector = DriftDetector::default();
        let analysis = detector.analyze_with_context(
            "implement user auth",
            &["implement login flow".to_string()],
            &[],
            &[],
            &[],
            &[make_budget_trace(0.92, true)], // 92% pressure, compression triggered
        );
        // Budget pressure alone adds 0.25 severity — below 0.3 threshold
        assert!(
            !analysis.drift_detected,
            "budget pressure alone (0.25) should not cross 0.3 threshold"
        );
        let budget_evidence = analysis
            .evidence
            .iter()
            .any(|e| e.description.contains("Token budget pressure"));
        assert!(budget_evidence, "should have budget pressure evidence");
    }

    #[test]
    fn test_analyze_with_context_budget_normal_no_evidence() {
        let detector = DriftDetector::default();
        let analysis = detector.analyze_with_context(
            "implement user auth",
            &["implement login flow".to_string()],
            &[],
            &[],
            &[],
            &[make_budget_trace(0.6, false)], // 60% — fine
        );
        let budget_evidence = analysis
            .evidence
            .iter()
            .any(|e| e.description.contains("Token budget pressure"));
        assert!(
            !budget_evidence,
            "normal budget should not trigger evidence"
        );
    }

    #[test]
    fn test_analyze_with_context_memory_miss_plus_budget_triggers_drift() {
        let detector = DriftDetector::default();
        // memory miss (0.2) + budget pressure (0.25) = 0.45 → above threshold
        let analysis = detector.analyze_with_context(
            "implement user auth",
            &["implement login flow".to_string()],
            &[],
            &[],
            &[make_memory_trace("user auth", 0, &[])],
            &[make_budget_trace(0.92, true)],
        );
        assert!(
            analysis.drift_detected,
            "memory miss + budget pressure should trigger drift (0.45 > 0.3)"
        );
        assert!(
            analysis.drift_severity >= 0.4,
            "combined severity should be >= 0.4, got {:.2}",
            analysis.drift_severity
        );
        // Cause should be MemoryMiss (detected first)
        assert!(
            matches!(analysis.likely_cause, DriftCause::MemoryMiss { .. }),
            "likely cause should be MemoryMiss when both are present"
        );
    }

    #[test]
    fn test_analyze_with_context_budget_pressure_carries_remaining_budget_values() {
        let detector = DriftDetector {
            min_severity_threshold: 0.2,
            ..DriftDetector::default()
        };
        let analysis = detector.analyze_with_context(
            "implement user auth",
            &[],
            &[],
            &[],
            &[],
            &[make_budget_trace(0.92, true)],
        );
        assert!(analysis.drift_detected);
        assert!(matches!(
            analysis.likely_cause,
            DriftCause::TokenBudgetPressure {
                budget_available: 8_000,
                budget_needed: 92_000,
                ..
            }
        ));
    }

    #[test]
    fn test_analyze_with_context_ambiguous_instruction() {
        let detector = DriftDetector::default();
        // Two corrections on similar queries (consecutive turns)
        let analysis = detector.analyze_with_context(
            "fix the build error",
            &[
                "fix the build error in main.rs".to_string(),
                "fix the build error in lib.rs".to_string(),
                "fix the build error please".to_string(),
            ],
            &[],
            &[0, 1], // corrections at turns 0 and 1
            &[],
            &[],
        );
        // user corrections (0.4) + ambiguous instruction (0.3) = 0.7
        assert!(
            analysis.drift_detected,
            "ambiguous instruction should be detected"
        );
        let clarification = analysis
            .evidence
            .iter()
            .any(|e| matches!(e.evidence_type, EvidenceType::ClarificationRequest));
        assert!(
            clarification,
            "should have ClarificationRequest evidence for ambiguity"
        );
    }

    #[test]
    fn test_analyze_with_context_non_consecutive_corrections_no_ambiguity() {
        let detector = DriftDetector::default();
        // Two corrections but far apart (turns 0 and 5)
        let analysis = detector.analyze_with_context(
            "fix the build error",
            &[
                "fix the build error".to_string(),
                "something".to_string(),
                "something".to_string(),
                "something".to_string(),
                "something".to_string(),
                "fix the build error".to_string(),
            ],
            &[],
            &[0, 5], // not consecutive
            &[],
            &[],
        );
        // Has user correction evidence (0.4) but NOT ambiguity
        let clarification = analysis
            .evidence
            .iter()
            .any(|e| matches!(e.evidence_type, EvidenceType::ClarificationRequest));
        assert!(
            !clarification,
            "non-consecutive corrections should not trigger ambiguity"
        );
    }

    #[test]
    fn test_analyze_with_context_dissimilar_corrections_no_ambiguity() {
        let detector = DriftDetector::default();
        // Consecutive corrections but on completely different topics
        let analysis = detector.analyze_with_context(
            "help me code",
            &[
                "deploy kubernetes cluster".to_string(),
                "write a haiku poem".to_string(),
            ],
            &[],
            &[0, 1], // consecutive corrections
            &[],
            &[],
        );
        let clarification = analysis
            .evidence
            .iter()
            .any(|e| matches!(e.evidence_type, EvidenceType::ClarificationRequest));
        assert!(
            !clarification,
            "dissimilar corrections should not trigger ambiguity (different topics)"
        );
    }

    #[test]
    fn test_analyze_with_context_base_drift_augmented() {
        let detector = DriftDetector::default();
        // Base: 2 unrelated queries (0.4) + extra: budget pressure (0.25) = 0.65
        let analysis = detector.analyze_with_context(
            "implement user auth",
            &[
                "configure kubernetes".to_string(),
                "setup monitoring".to_string(),
            ],
            &[],
            &[],
            &[],
            &[make_budget_trace(0.95, true)],
        );
        assert!(analysis.drift_detected);
        assert!(
            analysis.drift_severity >= 0.6,
            "severity should be >= 0.6, got {:.2}",
            analysis.drift_severity
        );
    }

    #[test]
    fn test_analyze_with_context_empty_traces() {
        let detector = DriftDetector::default();
        // No traces — should behave exactly like base analyze()
        let base = detector.analyze(
            "implement user auth",
            &["implement login flow".to_string()],
            &[],
            &[],
        );
        let enhanced = detector.analyze_with_context(
            "implement user auth",
            &["implement login flow".to_string()],
            &[],
            &[],
            &[],
            &[],
        );
        assert_eq!(base.drift_detected, enhanced.drift_detected);
        assert!((base.drift_severity - enhanced.drift_severity).abs() < f64::EPSILON);
        assert_eq!(base.evidence.len(), enhanced.evidence.len());
    }

    #[test]
    fn test_ellipsize_str() {
        assert_eq!(ellipsize_str("short", 10), "short");
        assert_eq!(ellipsize_str("hello world this is long", 10), "hello wor…");
        assert_eq!(ellipsize_str("", 5), "");
        // CJK characters
        assert_eq!(ellipsize_str("你好世界测试", 4), "你好世…");
    }
}
