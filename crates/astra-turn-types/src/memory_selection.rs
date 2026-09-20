//! Bounded memory decision facts. Candidate indices are local to this batch;
//! private memory text and raw provider responses do not enter Explain.
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MemorySelectionOperation {
    Relevance,
    Dismissal,
    Reuse,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wire_report_rejects_contradictory_or_content_bearing_facts() {
        let report = MemorySelectionReport {
            session_id: "s".into(),
            turn: 1,
            operation: MemorySelectionOperation::Relevance,
            method: MemorySelectionMethod::Model,
            reason: MemorySelectionReason::Completed,
            model: Some("jev-test".into()),
            selection_order: vec![0],
            elapsed_ms: 12,
            candidates: vec![MemoryCandidateDecision {
                index: 0,
                selected: true,
                probability_bps: Some(9000),
            }],
            candidate_coverage: None,
            prompt_projection: None,
        };
        assert!(report.is_valid());
        let mut invalid = report.clone();
        invalid.reason = MemorySelectionReason::NoCandidates;
        assert!(!invalid.is_valid());
        invalid = report.clone();
        invalid.method = MemorySelectionMethod::Lexical;
        assert!(!invalid.is_valid());
        invalid = report.clone();
        invalid.candidates[0].probability_bps = Some(10001);
        assert!(!invalid.is_valid());
        invalid = report.clone();
        invalid.candidates[0].index = 1;
        assert!(!invalid.is_valid());
        invalid = report.clone();
        invalid.session_id.clear();
        assert!(!invalid.is_valid());
        invalid = report.clone();
        invalid.turn = 0;
        assert!(!invalid.is_valid());
        invalid = report.clone();
        invalid.prompt_projection = Some(MemoryPromptProjection {
            selected_candidates: 1,
            included_candidates: Some(2),
        });
        assert!(!invalid.is_valid());
        invalid = report.clone();
        invalid.candidate_coverage = Some(MemoryCandidateCoverage {
            source_items: 8,
            evaluated_candidates: 2,
            truncated: true,
        });
        assert!(!invalid.is_valid());
        let mut projected = report.clone();
        projected.prompt_projection = Some(MemoryPromptProjection {
            selected_candidates: 1,
            included_candidates: Some(1),
        });
        assert!(projected.is_valid());
        assert!(projected.summary().contains("1/1 entered request"));
        projected.candidate_coverage = Some(MemoryCandidateCoverage {
            source_items: 8,
            evaluated_candidates: 1,
            truncated: true,
        });
        assert!(projected.is_valid());
        assert!(projected.summary().contains("bounded 8 source items"));
        let mut value = serde_json::to_value(report).unwrap();
        value["raw_response"] = serde_json::json!("private");
        assert!(serde_json::from_value::<MemorySelectionReport>(value).is_err());
    }
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MemorySelectionMethod {
    Model,
    Lexical,
    None,
    Reuse,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MemorySelectionReason {
    Completed,
    NoCandidates,
    NoSelector,
    CallUnavailable,
    InvalidResponse,
    RetrievalUnavailable,
    RetrievalTimeout,
    Reused,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct MemoryCandidateDecision {
    pub index: u32,
    pub selected: bool,
    /// Provider probability rounded to basis points, not a calibrated accuracy.
    pub probability_bps: Option<u16>,
}

/// Evidence produced by the final context serializer, after selection and
/// context optimization. It is deliberately separate from the selector's
/// recommendation: a selected candidate is not proof that the model saw it.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct MemoryPromptProjection {
    pub selected_candidates: u32,
    /// `None` means the final serializer could not prove inclusion or
    /// exclusion (for example, only a spill reference was visible).
    #[serde(deserialize_with = "crate::deserialize_required_option")]
    pub included_candidates: Option<u32>,
}

/// Coverage of the bounded retrieval-to-judgment candidate projection.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct MemoryCandidateCoverage {
    pub source_items: u32,
    pub evaluated_candidates: u32,
    pub truncated: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct MemorySelectionReport {
    pub session_id: String,
    pub turn: u32,
    pub operation: MemorySelectionOperation,
    pub method: MemorySelectionMethod,
    pub reason: MemorySelectionReason,
    pub model: Option<String>,
    pub candidates: Vec<MemoryCandidateDecision>,
    /// Selected candidate indices in the selector's ranking order.
    pub selection_order: Vec<u32>,
    /// Measured at the selection owner; not part of the server clock domain.
    pub elapsed_ms: u64,
    /// Present when the retrieval owner can report how a provider response was
    /// bounded before any rule/model judgment saw it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub candidate_coverage: Option<MemoryCandidateCoverage>,
    /// Present only when the final serialized request could verify whether
    /// the selected lesson payload entered the model-visible context.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_projection: Option<MemoryPromptProjection>,
}

impl MemorySelectionReport {
    pub fn is_valid(&self) -> bool {
        !self.session_id.is_empty()
            && self.session_id.len() <= 512
            && !self.session_id.chars().any(char::is_control)
            && self.turn > 0
            && self.candidates.len() <= 256
            && self.selection_order.len() == self.candidates.iter().filter(|c| c.selected).count()
            && self.selection_order.iter().enumerate().all(|(i, index)| {
                !self.selection_order[..i].contains(index)
                    && self
                        .candidates
                        .get(*index as usize)
                        .is_some_and(|c| c.selected)
            })
            && self.semantics_valid()
            && self.elapsed_ms <= crate::EXPLAIN_ANALYZE_MAX_SAFE_INTEGER
            && self.candidate_coverage.as_ref().is_none_or(|coverage| {
                coverage.evaluated_candidates as usize == self.candidates.len()
                    && coverage.source_items >= coverage.evaluated_candidates
            })
            && self.prompt_projection.as_ref().is_none_or(|projection| {
                self.operation != MemorySelectionOperation::Dismissal
                    && projection.selected_candidates as usize == self.selected_indices().len()
                    && projection
                        .included_candidates
                        .is_none_or(|included| included <= projection.selected_candidates)
            })
            && self.model.as_ref().is_none_or(|m| {
                !m.trim().is_empty() && m.len() <= 160 && !m.chars().any(char::is_control)
            })
            && self.candidates.iter().enumerate().all(|(i, c)| {
                c.index as usize == i && c.probability_bps.is_none_or(|p| p <= 10_000)
            })
    }

    fn semantics_valid(&self) -> bool {
        use MemorySelectionMethod as M;
        use MemorySelectionOperation as O;
        use MemorySelectionReason as R;
        let scores_absent = self.candidates.iter().all(|c| c.probability_bps.is_none());
        match self.reason {
            R::Completed => {
                self.method == M::Model
                    && self.operation != O::Reuse
                    && self.model.is_some()
                    && !self.candidates.is_empty()
            }
            R::NoCandidates => {
                self.method == M::None
                    && self.operation != O::Reuse
                    && self.model.is_none()
                    && self.candidates.is_empty()
            }
            R::RetrievalUnavailable | R::RetrievalTimeout => {
                self.method == M::None
                    && self.operation == O::Relevance
                    && self.model.is_none()
                    && self.candidates.is_empty()
            }
            R::Reused => {
                self.method == M::Reuse
                    && self.operation == O::Reuse
                    && self.model.is_none()
                    && scores_absent
                    && self.candidates.iter().all(|c| c.selected)
            }
            R::NoSelector | R::CallUnavailable | R::InvalidResponse => {
                scores_absent
                    && !self.candidates.is_empty()
                    && match self.operation {
                        O::Relevance => self.method == M::Lexical,
                        O::Dismissal => {
                            self.method == M::None && self.candidates.iter().all(|c| !c.selected)
                        }
                        O::Reuse => false,
                    }
            }
        }
    }

    pub fn selected_indices(&self) -> Vec<usize> {
        self.selection_order.iter().map(|i| *i as usize).collect()
    }

    pub fn summary(&self) -> String {
        use MemorySelectionReason as R;
        if matches!(self.reason, R::RetrievalUnavailable | R::RetrievalTimeout) {
            return format!("Memory retrieval unavailable · {}", self.reason_label());
        }
        let action = match self.operation {
            MemorySelectionOperation::Relevance => "selected",
            MemorySelectionOperation::Dismissal => "dismissed",
            MemorySelectionOperation::Reuse => "reused",
        };
        let method = match self.method {
            MemorySelectionMethod::Model => self.model.as_deref().unwrap_or("model"),
            MemorySelectionMethod::Lexical => "local keyword matching",
            MemorySelectionMethod::None => "not run",
            MemorySelectionMethod::Reuse => "session cache",
        };
        let projection = self
            .prompt_projection
            .as_ref()
            .map(|projection| {
                if projection.selected_candidates == 0 {
                    " · no memory entered request".to_string()
                } else if let Some(included) = projection.included_candidates {
                    format!(
                        " · {included}/{} entered request",
                        projection.selected_candidates
                    )
                } else {
                    " · request inclusion unknown".to_string()
                }
            })
            .unwrap_or_default();
        let coverage = self
            .candidate_coverage
            .as_ref()
            .filter(|coverage| coverage.truncated)
            .map(|coverage| {
                format!(
                    " · bounded {} source items to {} candidates",
                    coverage.source_items, coverage.evaluated_candidates
                )
            })
            .unwrap_or_default();
        format!(
            "Memory selection · {method} · {} candidates → {} {action}{coverage}{projection} · {}ms · {}",
            self.candidates.len(),
            self.selected_indices().len(),
            self.elapsed_ms,
            self.reason_label()
        )
    }

    pub fn detail_lines(&self) -> Vec<String> {
        let projection = if self.prompt_projection.is_some() {
            "final request projection measured"
        } else {
            "final request projection unavailable"
        };
        let mut lines = vec![format!(
            "Reported by CLI/Edge · turn {} · same decision across request rounds · {projection}",
            self.turn
        )];
        for candidate in &self.candidates {
            let decision = match (self.operation, candidate.selected) {
                (MemorySelectionOperation::Dismissal, true) => "dismissed",
                (MemorySelectionOperation::Dismissal, false) => "kept",
                (_, true) => "selected",
                (_, false) => "not selected",
            };
            let score = candidate
                .probability_bps
                .map(|p| format!(" · model score {:.2}%", f64::from(p) / 100.0))
                .unwrap_or_default();
            lines.push(format!(
                "Candidate {} · {decision}{score}",
                candidate.index + 1
            ));
        }
        lines
    }

    pub fn reason_label(&self) -> &'static str {
        match self.reason {
            MemorySelectionReason::Completed => "completed",
            MemorySelectionReason::NoCandidates => "no candidates",
            MemorySelectionReason::NoSelector => {
                if self.operation == MemorySelectionOperation::Dismissal {
                    "no selector available; memories kept"
                } else {
                    "no selector available; ranked locally, candidates kept"
                }
            }
            MemorySelectionReason::CallUnavailable => {
                if self.operation == MemorySelectionOperation::Dismissal {
                    "selector unavailable; memories kept"
                } else {
                    "selector unavailable; ranked locally, candidates kept"
                }
            }
            MemorySelectionReason::InvalidResponse => {
                if self.operation == MemorySelectionOperation::Dismissal {
                    "invalid selector response; memories kept"
                } else {
                    "invalid selector response; ranked locally, candidates kept"
                }
            }
            MemorySelectionReason::RetrievalUnavailable => "retrieval failed",
            MemorySelectionReason::RetrievalTimeout => "retrieval timed out",
            MemorySelectionReason::Reused => "no new relevance check",
        }
    }
}
