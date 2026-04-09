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
use std::time::SystemTime;

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

/// Possible causes of focus drift.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type")]
pub enum DriftCause {
    /// Important context was compressed away.
    HistoryCompression {
        lost_context: Vec<String>,
        compression_turn: u32,
    },

    /// Expected memories were not retrieved.
    MemoryMiss {
        expected_but_not_retrieved: Vec<String>,
        query_used: String,
    },

    /// Topic shifted without explicit transition.
    TopicShift {
        original_topic: String,
        new_topic: String,
        shift_turn: u32,
    },

    /// Token budget pressure forced premature decisions.
    TokenBudgetPressure {
        budget_available: u32,
        budget_needed: u32,
        sacrificed_context: Vec<String>,
    },

    /// User provided ambiguous instructions.
    AmbiguousInstruction {
        instruction: String,
        interpretations: Vec<String>,
    },

    /// No clear cause identified.
    Unknown,
}

/// Evidence supporting drift analysis.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DriftEvidence {
    /// Turn number where evidence was found.
    pub turn: u32,

    /// Type of evidence.
    pub evidence_type: EvidenceType,

    /// Description of the evidence.
    pub description: String,

    /// Confidence in this evidence (0.0-1.0).
    pub confidence: f64,
}

/// Types of evidence for drift detection.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum EvidenceType {
    /// Tool calls changed topic.
    ToolCallTopicChange,
    /// User corrected the agent.
    UserCorrection,
    /// Agent asked clarifying question about original task.
    ClarificationRequest,
    /// Key term disappeared from context.
    TermDisappearance,
    /// Compression removed relevant turn.
    CompressionLoss,
    /// Memory query returned irrelevant results.
    MemoryMismatch,
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
            confidence: confidence.clamp(0.0, 1.0),
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
                    "- Turn {}: [{}] {} ({:.0}% confidence)",
                    ev.turn,
                    format!("{:?}", ev.evidence_type),
                    ev.description,
                    ev.confidence * 100.0
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
                confidence: 0.8,
            });
            severity += 0.4;
        }

        // Check for topic divergence (basic keyword overlap)
        let original_keywords = extract_keywords(original_query);
        for (i, query) in recent_queries.iter().enumerate() {
            let query_keywords = extract_keywords(query);
            let overlap = keyword_overlap(&original_keywords, &query_keywords);
            if overlap < 0.2 {
                evidence.push(DriftEvidence {
                    turn: i as u32,
                    evidence_type: EvidenceType::ToolCallTopicChange,
                    description: format!(
                        "Low keyword overlap ({:.0}%) with original query",
                        overlap * 100.0
                    ),
                    confidence: 0.6,
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
                confidence: 0.5,
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
}

/// Extract simple keywords from text (lowercase, 4+ chars).
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
}
