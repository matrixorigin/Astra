use serde::{Deserialize, Serialize};

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
