//! Decision Explainer — provides human-readable explanations for agent decisions.
//!
//! This module helps answer "why" questions:
//! - Why were these tools selected?
//! - Why was this history compressed?
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
    /// Tool surface decision.
    ToolSurface {
        visible_tools: Vec<String>,
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
            DecisionType::ToolSurface { .. } => "tool surface",
            DecisionType::HistoryCompression { .. } => "History Compression",
            DecisionType::MemoryRetrieval { .. } => "Memory Retrieval",
            DecisionType::StrategyChoice { .. } => "Strategy Choice",
            DecisionType::ModelRouting { .. } => "Model Routing",
        }
    }
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_decision_explanation_builder() {
        let exp = DecisionExplanation::new(
            DecisionType::ToolSurface {
                visible_tools: vec!["grep".to_string(), "read_file".to_string()],
                total_available: 50,
            },
            "Surfaced grep and read_file based on the recorded tool-surface decision.",
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
    fn test_human_readable_explanation() {
        let exp = DecisionExplanation::new(
            DecisionType::ToolSurface {
                visible_tools: vec!["grep".to_string()],
                total_available: 10,
            },
            "Grep surfaced for code search.",
        )
        .with_confidence(0.9);

        let text = exp.to_human_readable();
        assert!(text.contains("tool surface"));
        assert!(text.contains("90%"));
    }
}
