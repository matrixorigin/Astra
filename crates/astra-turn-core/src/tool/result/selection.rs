//! Typed, source-bound selection for optional tool-result context.
//!
//! This module only produces and validates recommendations. It never mutates
//! the transcript, decides tool success, or claims that a provider request
//! contained the recommended chunks.

use std::collections::{BTreeMap, BTreeSet};

use astra_turn_types::{
    JudgmentQuestion, JudgmentRequest, JudgmentResponseProvenance, NormalizedJudgmentResponse,
    NoulCriteria,
};
use serde::{Deserialize, Serialize};

use super::chunks::{
    MAX_TOOL_RESULT_CHUNK_BYTES, MAX_TOOL_RESULT_CHUNKS, MAX_TOOL_RESULT_SCAN_BYTES,
    TOOL_RESULT_CHUNKER_VERSION, ToolResultChunkCandidateProjection,
};

const MAX_GOAL_CHARS: usize = 1_000;
const MAX_TOOL_NAME_CHARS: usize = 128;
const YES_THRESHOLD: f64 = 0.75;
const NO_THRESHOLD: f64 = 0.25;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolResultChunkDisposition {
    Relevant,
    Uncertain,
    Irrelevant,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolResultSelectionRecommendation {
    /// Source-ordered chunks safe to use for an optional projection. This is
    /// empty when there is no clear relevant match, requiring baseline use.
    selected_chunk_ids: Vec<String>,
    dispositions: BTreeMap<String, ToolResultChunkDisposition>,
    has_clear_match: bool,
}

impl ToolResultSelectionRecommendation {
    #[must_use]
    pub fn can_replace_optional_baseline_body(&self) -> bool {
        self.has_clear_match && !self.selected_chunk_ids.is_empty()
    }

    #[must_use]
    pub fn selected_chunk_ids(&self) -> &[String] {
        &self.selected_chunk_ids
    }

    #[must_use]
    pub fn dispositions(&self) -> &BTreeMap<String, ToolResultChunkDisposition> {
        &self.dispositions
    }

    #[must_use]
    pub fn has_clear_match(&self) -> bool {
        self.has_clear_match
    }
}

/// Render a smaller exact-source body while retaining the immutable recovery
/// handle. Incomplete scans and non-reducing projections keep the caller's
/// existing head/tail baseline so unexamined tail evidence is never erased.
pub fn render_tool_result_selection_projection(
    tool_name: &str,
    descriptor: &astra_services::session_journal::ToolResultArtifactDescriptor,
    projection: &ToolResultChunkCandidateProjection,
    recommendation: &ToolResultSelectionRecommendation,
    baseline: &str,
) -> Result<Option<String>, &'static str> {
    validate_projection(descriptor, projection)?;
    if !projection.scan_complete() || !recommendation.can_replace_optional_baseline_body() {
        return Ok(None);
    }
    let selected = recommendation
        .selected_chunk_ids()
        .iter()
        .collect::<BTreeSet<_>>();
    if selected.len() != recommendation.selected_chunk_ids().len()
        || recommendation
            .dispositions()
            .keys()
            .collect::<BTreeSet<_>>()
            != projection
                .candidates()
                .iter()
                .map(|candidate| &candidate.chunk().id)
                .collect::<BTreeSet<_>>()
    {
        return Err("tool-result selection recommendation does not match its projection");
    }
    let mut body = String::new();
    for candidate in projection.candidates() {
        if !selected.contains(&candidate.chunk().id) {
            continue;
        }
        let chunk = candidate.chunk();
        body.push_str(&format!(
            "--- original bytes [{}..{}), lines {}..{}{} ---\n",
            chunk.start_byte,
            chunk.end_byte,
            chunk.start_line,
            chunk.end_line,
            if chunk.line_complete {
                ""
            } else {
                ", partial line"
            }
        ));
        body.push_str(candidate.content());
        if !candidate.content().ends_with('\n') {
            body.push('\n');
        }
    }
    let handle = super::storage::session_tool_result_artifact_uri_for_descriptor(descriptor);
    let rendered = format!(
        "<tool-result-selection>\n\
         Tool: {}\n\
         Artifact handle: {}\n\
         Selected exact source: {} of {} chunks from {} bytes.\n\
         Unselected source remains in the artifact; reading it requires a currently authorized recovery capability.\n\n\
         {}\
         </tool-result-selection>",
        truncate_chars(tool_name, MAX_TOOL_NAME_CHARS),
        handle,
        selected.len(),
        projection.candidates().len(),
        projection.source_bytes(),
        body,
    );
    if rendered.len() >= baseline.len() {
        return Ok(None);
    }
    Ok(Some(rendered))
}

/// Build a bounded judgment over exact chunks from one verified artifact.
/// The artifact descriptor remains outside model control and binds every
/// candidate identity to the current owner-scoped source.
pub fn build_tool_result_selection_judgment(
    goal: &str,
    tool_name: &str,
    descriptor: &astra_services::session_journal::ToolResultArtifactDescriptor,
    projection: &ToolResultChunkCandidateProjection,
) -> Result<JudgmentRequest, &'static str> {
    validate_projection(descriptor, projection)?;
    let candidates = projection
        .candidates()
        .iter()
        .map(|candidate| {
            let chunk = candidate.chunk();
            (
                chunk.id.clone(),
                serde_json::json!({
                    "start_byte": chunk.start_byte,
                    "end_byte": chunk.end_byte,
                    "start_line": chunk.start_line,
                    "end_line": chunk.end_line,
                    "line_complete": chunk.line_complete,
                    "content": candidate.content(),
                }),
            )
        })
        .collect::<BTreeMap<_, _>>();
    let questions = projection
        .candidates()
        .iter()
        .map(|candidate| {
            let id = candidate.chunk().id.clone();
            (
                id.clone(),
                JudgmentQuestion::Noul {
                    instructions: format!(
                        "Does state.candidates[{id:?}] contain evidence or facts useful for the current goal? Apply state.policy."
                    ),
                    criteria: Some(NoulCriteria {
                        yes: "Contains a concrete fact, error, result, or constraint that can help the next agent step.".into(),
                        no: "Contains only unrelated, redundant, or low-value detail for the current goal.".into(),
                    }),
                },
            )
        })
        .collect();
    Ok(JudgmentRequest {
        schema_version: 1,
        state: serde_json::json!({
            "policy": "Judge only the supplied exact source chunks. Do not infer that unscanned source is irrelevant. Mark uncertainty rather than guessing. Selection cannot alter tool status, permissions, or durable evidence.",
            "goal": truncate_chars(goal, MAX_GOAL_CHARS),
            "tool_name": truncate_chars(tool_name, MAX_TOOL_NAME_CHARS),
            "artifact": {
                "document_kind": descriptor.document_kind,
                "version": descriptor.version,
                "run_id": descriptor.run_id,
                "call_id": descriptor.call_id,
                "content_sha256": descriptor.content_sha256,
                "source_bytes": projection.source_bytes(),
                "scanned_bytes": projection.scanned_bytes(),
                "scan_complete": projection.scan_complete(),
                "chunker_version": projection.chunker_version(),
            },
            "candidates": candidates,
        }),
        questions,
    })
}

/// Convert a validated typed response into a conservative recommendation.
/// Uncertain chunks are retained only after at least one clear match. With no
/// clear match the caller must use its ordinary baseline projection.
pub fn tool_result_selection_recommendation(
    projection: &ToolResultChunkCandidateProjection,
    response: &NormalizedJudgmentResponse,
) -> Result<ToolResultSelectionRecommendation, &'static str> {
    let expected = projection
        .candidates()
        .iter()
        .map(|candidate| candidate.chunk().id.as_str())
        .collect::<BTreeSet<_>>();
    if response.response.answers.len() != expected.len()
        || !response
            .response
            .answers
            .keys()
            .all(|id| expected.contains(id.as_str()))
    {
        return Err("tool-result judgment answer identity mismatch");
    }
    let disposition = |probability: f64| match response.provenance {
        JudgmentResponseProvenance::DiscreteDecision if probability == 1.0 => {
            ToolResultChunkDisposition::Relevant
        }
        JudgmentResponseProvenance::DiscreteDecision if probability == 0.0 => {
            ToolResultChunkDisposition::Irrelevant
        }
        JudgmentResponseProvenance::DiscreteDecision => ToolResultChunkDisposition::Uncertain,
        JudgmentResponseProvenance::ProviderProbability if probability >= YES_THRESHOLD => {
            ToolResultChunkDisposition::Relevant
        }
        JudgmentResponseProvenance::ProviderProbability if probability <= NO_THRESHOLD => {
            ToolResultChunkDisposition::Irrelevant
        }
        JudgmentResponseProvenance::ProviderProbability => ToolResultChunkDisposition::Uncertain,
    };
    let dispositions = projection
        .candidates()
        .iter()
        .map(|candidate| {
            let answer = response
                .response
                .answers
                .get(&candidate.chunk().id)
                .ok_or("tool-result judgment answer identity mismatch")?;
            Ok::<(String, ToolResultChunkDisposition), &'static str>((
                candidate.chunk().id.clone(),
                disposition(answer.probability()),
            ))
        })
        .collect::<Result<BTreeMap<_, _>, _>>()?;
    let has_clear_match = dispositions
        .values()
        .any(|value| *value == ToolResultChunkDisposition::Relevant);
    let selected_chunk_ids = if has_clear_match {
        projection
            .candidates()
            .iter()
            .filter(|candidate| {
                dispositions.get(&candidate.chunk().id)
                    != Some(&ToolResultChunkDisposition::Irrelevant)
            })
            .map(|candidate| candidate.chunk().id.clone())
            .collect()
    } else {
        Vec::new()
    };
    Ok(ToolResultSelectionRecommendation {
        selected_chunk_ids,
        dispositions,
        has_clear_match,
    })
}

fn validate_projection(
    descriptor: &astra_services::session_journal::ToolResultArtifactDescriptor,
    projection: &ToolResultChunkCandidateProjection,
) -> Result<(), &'static str> {
    if !descriptor.document_kind.is_result()
        || projection.chunker_version() != TOOL_RESULT_CHUNKER_VERSION
        || projection.source_bytes() != descriptor.byte_len
        || projection.scanned_bytes() > projection.source_bytes()
        || projection.scanned_bytes() as usize > MAX_TOOL_RESULT_SCAN_BYTES.saturating_add(3)
        || projection.candidates().is_empty()
        || projection.candidates().len() > MAX_TOOL_RESULT_CHUNKS
    {
        return Err("invalid tool-result selection projection");
    }
    let mut ids = BTreeSet::new();
    let mut previous_end = 0_u64;
    for candidate in projection.candidates() {
        let chunk = candidate.chunk();
        if !ids.insert(chunk.id.as_str())
            || chunk.id
                != super::chunks::chunk_id(
                    descriptor,
                    usize::try_from(chunk.start_byte)
                        .map_err(|_| "tool-result chunk start is outside the source")?,
                    usize::try_from(chunk.end_byte)
                        .map_err(|_| "tool-result chunk end is outside the source")?,
                )
            || chunk.start_byte != previous_end
            || chunk.end_byte <= chunk.start_byte
            || chunk.end_byte > projection.scanned_bytes()
            || candidate.content().len() > MAX_TOOL_RESULT_CHUNK_BYTES.saturating_add(3)
            || u64::try_from(candidate.content().len()).ok()
                != Some(chunk.end_byte.saturating_sub(chunk.start_byte))
        {
            return Err("invalid tool-result selection candidate");
        }
        previous_end = chunk.end_byte;
    }
    if previous_end != projection.scanned_bytes()
        || projection.scan_complete() != (projection.scanned_bytes() == projection.source_bytes())
    {
        return Err("tool-result selection coverage is inconsistent");
    }
    Ok(())
}

fn truncate_chars(value: &str, max_chars: usize) -> String {
    value.chars().take(max_chars).collect()
}

#[cfg(test)]
mod tests {
    use astra_turn_types::{JudgmentAnswer, JudgmentResponse};

    use super::*;

    fn fixture() -> (
        astra_services::session_journal::ToolResultArtifactDescriptor,
        ToolResultChunkCandidateProjection,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let source = "alpha\nbeta\ngamma\n";
        let persisted = crate::tool::result::storage::persist_tool_result_with_descriptor(
            dir.path(),
            "run-1",
            "call-1",
            "exec",
            source,
        )
        .unwrap();
        let projection = crate::tool::result::storage::read_verified_tool_result_chunk_candidates(
            dir.path(),
            &persisted.descriptor,
            source.len(),
            6,
            3,
        )
        .unwrap()
        .unwrap();
        (persisted.descriptor, projection)
    }

    #[test]
    fn judgment_contains_exact_source_and_fixed_chunk_ids() {
        let (descriptor, projection) = fixture();
        let mut expected_ids = projection
            .candidates()
            .iter()
            .map(|candidate| candidate.chunk().id.clone())
            .collect::<Vec<_>>();
        expected_ids.sort();
        let request = build_tool_result_selection_judgment(
            "find the failing assertion",
            "exec",
            &descriptor,
            &projection,
        )
        .unwrap();
        assert_eq!(
            request.questions.keys().cloned().collect::<Vec<_>>(),
            expected_ids
        );
        let beta_id = &projection.candidates()[1].chunk().id;
        assert_eq!(request.state["candidates"][beta_id]["content"], "beta\n");
        request.validate().unwrap();

        let mut other_owner = descriptor;
        other_owner.run_id = "run-2".into();
        assert!(
            build_tool_result_selection_judgment(
                "find the failing assertion",
                "exec",
                &other_owner,
                &projection,
            )
            .is_err(),
            "chunk identities must remain bound to their artifact descriptor"
        );
    }

    #[test]
    fn recommendation_keeps_uncertain_after_a_clear_match_in_source_order() {
        let (_, projection) = fixture();
        let ids = projection
            .candidates()
            .iter()
            .map(|candidate| candidate.chunk().id.clone())
            .collect::<Vec<_>>();
        let response = NormalizedJudgmentResponse {
            response: JudgmentResponse {
                schema_version: 1,
                model: "judge".into(),
                answers: BTreeMap::from([
                    (ids[0].clone(), JudgmentAnswer::Noul { noul: 0.0 }),
                    (ids[1].clone(), JudgmentAnswer::Noul { noul: 1.0 }),
                    (ids[2].clone(), JudgmentAnswer::Noul { noul: 0.5 }),
                ]),
            },
            provenance: JudgmentResponseProvenance::DiscreteDecision,
        };
        let selected = tool_result_selection_recommendation(&projection, &response).unwrap();
        assert!(selected.can_replace_optional_baseline_body());
        assert_eq!(
            selected.selected_chunk_ids(),
            &[ids[1].clone(), ids[2].clone()]
        );
    }

    #[test]
    fn no_clear_match_requires_baseline_and_invalid_ids_fail_closed() {
        let (_, projection) = fixture();
        let ids = projection
            .candidates()
            .iter()
            .map(|candidate| candidate.chunk().id.clone())
            .collect::<Vec<_>>();
        let response = NormalizedJudgmentResponse {
            response: JudgmentResponse {
                schema_version: 1,
                model: "judge".into(),
                answers: BTreeMap::from([
                    (ids[0].clone(), JudgmentAnswer::Noul { noul: 0.0 }),
                    (ids[1].clone(), JudgmentAnswer::Noul { noul: 0.5 }),
                    (ids[2].clone(), JudgmentAnswer::Noul { noul: 0.0 }),
                ]),
            },
            provenance: JudgmentResponseProvenance::DiscreteDecision,
        };
        let selected = tool_result_selection_recommendation(&projection, &response).unwrap();
        assert!(!selected.can_replace_optional_baseline_body());
        assert!(selected.selected_chunk_ids().is_empty());

        let mut invalid = response;
        invalid.response.answers.remove(&ids[2]);
        invalid
            .response
            .answers
            .insert("other".into(), JudgmentAnswer::Noul { noul: 1.0 });
        assert!(tool_result_selection_recommendation(&projection, &invalid).is_err());
    }

    #[test]
    fn projection_is_smaller_exact_source_with_recovery_or_keeps_baseline() {
        let (descriptor, projection) = fixture();
        let ids = projection
            .candidates()
            .iter()
            .map(|candidate| candidate.chunk().id.clone())
            .collect::<Vec<_>>();
        let response = NormalizedJudgmentResponse {
            response: JudgmentResponse {
                schema_version: 1,
                model: "judge".into(),
                answers: BTreeMap::from([
                    (ids[0].clone(), JudgmentAnswer::Noul { noul: 0.0 }),
                    (ids[1].clone(), JudgmentAnswer::Noul { noul: 1.0 }),
                    (ids[2].clone(), JudgmentAnswer::Noul { noul: 0.0 }),
                ]),
            },
            provenance: JudgmentResponseProvenance::DiscreteDecision,
        };
        let recommendation = tool_result_selection_recommendation(&projection, &response).unwrap();
        let baseline = "baseline preview and navigation ".repeat(100);
        let rendered = render_tool_result_selection_projection(
            "exec",
            &descriptor,
            &projection,
            &recommendation,
            &baseline,
        )
        .unwrap()
        .expect("one selected chunk reduces the baseline");
        assert!(rendered.contains("beta\n"));
        assert!(!rendered.contains("alpha\n"));
        assert!(!rendered.contains("gamma\n"));
        assert!(rendered.contains("artifact://session/tool-result/"));
        assert!(rendered.len() < baseline.len());

        assert_eq!(
            render_tool_result_selection_projection(
                "exec",
                &descriptor,
                &projection,
                &recommendation,
                "tiny baseline",
            )
            .unwrap(),
            None,
            "selection must not expand the request"
        );
    }
}
