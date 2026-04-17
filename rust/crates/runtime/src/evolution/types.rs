//! Core types for the evolution engine.

use crate::pipeline::routing::{DomainHint, TaskType};
use crate::turn::action_compensation::FailureCategory;

// Re-export CalibrationAxis from pipeline::routing (defined in astra-pipeline)
pub use crate::pipeline::routing::CalibrationAxis;

// ── Signals ──

/// A typed evolution signal detected from runtime events.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub enum EvolutionSignal {
    /// A tool returned an error result.
    ToolFailure {
        tool_name: String,
        error_snippet: String,
        failure_category: Option<FailureCategory>,
        skill_context: Option<String>,
        turn_id: String,
    },
    /// The user corrected the agent's previous response.
    UserCorrection {
        correction_text: String,
        prior_assistant_text: String,
        skill_context: Option<String>,
        turn_id: String,
    },
    /// A tool chain pattern's recent success rate dropped significantly.
    PatternDrift {
        pattern_signature: String,
        task_type: TaskType,
        domain: Option<DomainHint>,
        historical_rate: f64,
        recent_rate: f64,
    },
    /// The same tool signature repeated across multiple rounds (stall).
    RepeatedStall {
        tool_chain: Vec<String>,
        stall_count: u32,
        turn_id: String,
    },
    /// An LLM-driven reflection generated evolution proposals.
    LlmReflection { context_id: String },
}

impl EvolutionSignal {
    /// Deduplication key: discriminant + identifying fields.
    pub fn dedup_key(&self) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        std::mem::discriminant(self).hash(&mut h);
        match self {
            Self::ToolFailure {
                tool_name,
                error_snippet,
                ..
            } => {
                tool_name.hash(&mut h);
                // First 100 chars of error for dedup (same as Jiuwenclaw, but typed).
                error_snippet
                    .get(..100)
                    .unwrap_or(error_snippet)
                    .hash(&mut h);
            }
            Self::UserCorrection {
                correction_text, ..
            } => {
                correction_text
                    .get(..100)
                    .unwrap_or(correction_text)
                    .hash(&mut h);
            }
            Self::PatternDrift {
                pattern_signature, ..
            } => {
                pattern_signature.hash(&mut h);
            }
            Self::RepeatedStall { tool_chain, .. } => {
                tool_chain.hash(&mut h);
            }
            Self::LlmReflection { context_id } => {
                context_id.hash(&mut h);
            }
        }
        h.finish()
    }
}

// ── Proposals ──

/// Which axis an evolution proposal targets.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub enum EvolutionAxis {
    /// Modify a skill's SKILL.md content.
    Skill {
        skill_name: String,
        section: SkillSection,
        diff: SkillDiff,
    },
    /// Adjust a tool chain pattern's standing.
    Pattern {
        signature: String,
        action: PatternAction,
    },
    /// Nudge a calibration axis.
    Calibration {
        axis: CalibrationAxis,
        adjustment: f64,
    },
    /// Update entity knowledge.
    Entity {
        entity: String,
        action: EntityAction,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum SkillSection {
    Instructions,
    Examples,
    Troubleshooting,
}

impl SkillSection {
    pub fn heading(&self) -> &'static str {
        match self {
            Self::Instructions => "Instructions",
            Self::Examples => "Examples",
            Self::Troubleshooting => "Troubleshooting",
        }
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub enum SkillDiff {
    /// Append content to the end of a section.
    Append { content: String },
    /// Replace content matching a marker string.
    Replace {
        old_marker: String,
        new_content: String,
    },
    /// Remove content matching a marker string.
    Remove { marker: String },
}

pub use astra_pipeline::PatternAction;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub enum EntityAction {
    AddTool(String),
    RemoveTool(String),
    SetDomain(DomainHint),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum ProposalPromotionRecommendation {
    Promote,
    Canary,
    Hold,
}

impl ProposalPromotionRecommendation {
    pub fn priority(self) -> u8 {
        match self {
            Self::Promote => 0,
            Self::Canary => 1,
            Self::Hold => 2,
        }
    }
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ProposalPromotionVerdict {
    pub recommendation: ProposalPromotionRecommendation,
    pub confidence_score: f64,
    pub support_score: f64,
    pub safety_score: f64,
    pub overall_score: f64,
    pub evidence: Vec<String>,
    pub blockers: Vec<String>,
    pub rollback_hint: Option<String>,
}

/// A proposed evolution with metadata.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct EvolutionProposal {
    pub id: String,
    pub signal: EvolutionSignal,
    pub axis: EvolutionAxis,
    pub confidence: f64,
    pub reasoning: String,
    pub created_at: u64,
    pub status: ApprovalStatus,
    pub promotion_verdict: Option<ProposalPromotionVerdict>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum ApprovalStatus {
    Pending,
    CanaryActive,
    CanaryPromoted,
    CanaryRolledBack,
    Approved,
    Rejected,
    AutoApplied,
}

// ── Rail trait ──

/// Context passed to evolution rail after a tool execution.
#[derive(Debug)]
pub struct ToolResultContext<'a> {
    pub tool_name: &'a str,
    pub tool_args: &'a str,
    pub result: &'a str,
    pub is_error: bool,
    pub failure_category: Option<FailureCategory>,
    pub duration_ms: u64,
    pub active_skill: Option<&'a str>,
    pub turn_id: &'a str,
}

/// Summary passed to evolution rail at turn end.
#[derive(Debug)]
pub struct TurnSummary<'a> {
    pub turn_id: &'a str,
    pub tools_used: &'a [String],
    pub had_errors: bool,
    pub user_query: &'a str,
    pub active_skill: Option<&'a str>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dedup_key_same_tool_failure_same_key() {
        let s1 = EvolutionSignal::ToolFailure {
            tool_name: "bash".into(),
            error_snippet: "command not found".into(),
            failure_category: None,
            skill_context: None,
            turn_id: "t1".into(),
        };
        let s2 = EvolutionSignal::ToolFailure {
            tool_name: "bash".into(),
            error_snippet: "command not found".into(),
            failure_category: None,
            skill_context: Some("review_changes".into()),
            turn_id: "t2".into(),
        };
        assert_eq!(s1.dedup_key(), s2.dedup_key());
    }

    #[test]
    fn dedup_key_different_tool_different_key() {
        let s1 = EvolutionSignal::ToolFailure {
            tool_name: "bash".into(),
            error_snippet: "command not found".into(),
            failure_category: None,
            skill_context: None,
            turn_id: "t1".into(),
        };
        let s2 = EvolutionSignal::ToolFailure {
            tool_name: "read_file".into(),
            error_snippet: "command not found".into(),
            failure_category: None,
            skill_context: None,
            turn_id: "t1".into(),
        };
        assert_ne!(s1.dedup_key(), s2.dedup_key());
    }

    #[test]
    fn dedup_key_different_variant_different_key() {
        let s1 = EvolutionSignal::ToolFailure {
            tool_name: "bash".into(),
            error_snippet: "error".into(),
            failure_category: None,
            skill_context: None,
            turn_id: "t1".into(),
        };
        let s2 = EvolutionSignal::UserCorrection {
            correction_text: "error".into(),
            prior_assistant_text: "".into(),
            skill_context: None,
            turn_id: "t1".into(),
        };
        assert_ne!(s1.dedup_key(), s2.dedup_key());
    }

    #[test]
    fn dedup_key_long_snippet_truncated() {
        let long = "x".repeat(500);
        let s1 = EvolutionSignal::ToolFailure {
            tool_name: "bash".into(),
            error_snippet: long.clone(),
            failure_category: None,
            skill_context: None,
            turn_id: "t1".into(),
        };
        let s2 = EvolutionSignal::ToolFailure {
            tool_name: "bash".into(),
            error_snippet: format!("{long}EXTRA_SUFFIX"),
            failure_category: None,
            skill_context: None,
            turn_id: "t1".into(),
        };
        // First 100 chars are the same → same key
        assert_eq!(s1.dedup_key(), s2.dedup_key());
    }

    #[test]
    fn skill_section_headings() {
        assert_eq!(SkillSection::Instructions.heading(), "Instructions");
        assert_eq!(SkillSection::Examples.heading(), "Examples");
        assert_eq!(SkillSection::Troubleshooting.heading(), "Troubleshooting");
    }

    #[test]
    fn approval_status_default_is_pending() {
        let p = EvolutionProposal {
            id: "ev_test".into(),
            signal: EvolutionSignal::ToolFailure {
                tool_name: "bash".into(),
                error_snippet: "err".into(),
                failure_category: None,
                skill_context: None,
                turn_id: "t1".into(),
            },
            axis: EvolutionAxis::Pattern {
                signature: "bash".into(),
                action: PatternAction::Demote,
            },
            confidence: 0.8,
            reasoning: "test".into(),
            created_at: 0,
            status: ApprovalStatus::Pending,
            promotion_verdict: None,
        };
        assert_eq!(p.status, ApprovalStatus::Pending);
    }

    #[test]
    fn promotion_recommendation_priorities_sort_promote_first() {
        assert!(
            ProposalPromotionRecommendation::Promote.priority()
                < ProposalPromotionRecommendation::Canary.priority()
        );
        assert!(
            ProposalPromotionRecommendation::Canary.priority()
                < ProposalPromotionRecommendation::Hold.priority()
        );
    }
}
