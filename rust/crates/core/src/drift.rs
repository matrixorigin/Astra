use serde::{Deserialize, Serialize};

use crate::confidence::ConfidenceInterval;

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
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DriftEvidence {
    /// Turn number where evidence was found.
    pub turn: u32,

    /// Type of evidence.
    pub evidence_type: EvidenceType,

    /// Description of the evidence.
    pub description: String,

    /// Confidence in this evidence with explicit uncertainty bounds.
    pub confidence: ConfidenceInterval,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drift_evidence_serializes_confidence_interval() {
        let evidence = DriftEvidence {
            turn: 3,
            evidence_type: EvidenceType::MemoryMismatch,
            description: "wrong memory cluster".into(),
            confidence: ConfidenceInterval::symmetric(0.8, 0.1),
        };

        let json = serde_json::to_value(&evidence).unwrap();
        let point = json["confidence"]["point"].as_f64().unwrap();
        let lower = json["confidence"]["lower"].as_f64().unwrap();
        let upper = json["confidence"]["upper"].as_f64().unwrap();
        assert!((point - 0.8).abs() < f64::EPSILON);
        assert!((lower - 0.7).abs() < 1e-9);
        assert!((upper - 0.9).abs() < f64::EPSILON);
    }
}
