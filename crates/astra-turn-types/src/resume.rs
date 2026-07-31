use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

use crate::{
    CONVERSATION_PROJECTION_SCHEMA_VERSION, DEFAULT_CONVERSATION_BRANCH_ID,
    SEGMENTED_CONVERSATION_PROJECTION_SCHEMA_VERSION, SESSION_CURSOR_SCHEMA_VERSION,
    SessionCursorV1, canonical_conversation_root,
};

pub const CAUSAL_PROJECTION_ENVELOPE_SCHEMA_VERSION: u32 = 1;
pub const RESUME_BUNDLE_SCHEMA_VERSION: u32 = 1;

/// Causal relationship between two conversation cursors.
///
/// `BehindUnverified` deliberately does not claim ancestry. A lower sequence
/// is only a possible prefix until journal replay or another durable proof
/// verifies it.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CursorRelationV1 {
    Exact,
    BehindUnverified,
    Ahead,
    Divergent,
    DifferentIdentity,
    LegacyUnknown,
}

pub fn cursor_relation(candidate: &SessionCursorV1, head: &SessionCursorV1) -> CursorRelationV1 {
    if candidate.owner_id != head.owner_id
        || candidate.session_id != head.session_id
        || candidate.branch_id != head.branch_id
    {
        return CursorRelationV1::DifferentIdentity;
    }
    if candidate.schema_version == 0 || head.schema_version == 0 {
        return CursorRelationV1::LegacyUnknown;
    }
    if candidate == head {
        return CursorRelationV1::Exact;
    }

    let candidate_clock = (
        candidate.journal_event_seq,
        candidate.conversation_seq,
        candidate.completed_turn,
    );
    let head_clock = (
        head.journal_event_seq,
        head.conversation_seq,
        head.completed_turn,
    );
    if candidate_clock.0 <= head_clock.0
        && candidate_clock.1 <= head_clock.1
        && candidate_clock.2 <= head_clock.2
        && candidate_clock != head_clock
    {
        return CursorRelationV1::BehindUnverified;
    }
    if candidate_clock.0 >= head_clock.0
        && candidate_clock.1 >= head_clock.1
        && candidate_clock.2 >= head_clock.2
        && candidate_clock != head_clock
    {
        return CursorRelationV1::Ahead;
    }
    CursorRelationV1::Divergent
}

/// Versioned envelope for checkpoint/task/provider/activation projections.
///
/// A projection older than the selected conversation is admissible only when
/// replay has verified it through that exact selected root. Merely having a
/// lower turn or sequence is not ancestry proof.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CausalProjectionEnvelopeV1<T> {
    pub schema_version: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_cursor: Option<SessionCursorV1>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verified_through_root: Option<String>,
    pub payload: T,
}

impl<T> CausalProjectionEnvelopeV1<T> {
    pub fn unversioned(payload: T) -> Self {
        Self {
            schema_version: CAUSAL_PROJECTION_ENVELOPE_SCHEMA_VERSION,
            source_cursor: None,
            verified_through_root: None,
            payload,
        }
    }

    pub fn at_cursor(cursor: SessionCursorV1, payload: T) -> Self {
        Self {
            schema_version: CAUSAL_PROJECTION_ENVELOPE_SCHEMA_VERSION,
            source_cursor: Some(cursor),
            verified_through_root: None,
            payload,
        }
    }

    pub fn is_admissible_at(&self, selected: &SessionCursorV1) -> bool {
        if self.schema_version != CAUSAL_PROJECTION_ENVELOPE_SCHEMA_VERSION {
            return false;
        }
        let Some(source) = self.source_cursor.as_ref() else {
            return false;
        };
        match cursor_relation(source, selected) {
            CursorRelationV1::Exact => true,
            CursorRelationV1::BehindUnverified => {
                self.verified_through_root.as_deref() == Some(selected.canonical_root_hash.as_str())
            }
            CursorRelationV1::Ahead
            | CursorRelationV1::Divergent
            | CursorRelationV1::DifferentIdentity
            | CursorRelationV1::LegacyUnknown => false,
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ResumeSourceV1 {
    CanonicalJournal,
    CslProjection,
    Checkpoint,
    JournalDisplayProjection,
    TranscriptProjection,
}

impl ResumeSourceV1 {
    fn reliability_rank(self) -> u8 {
        match self {
            Self::CanonicalJournal => 5,
            Self::CslProjection => 4,
            Self::Checkpoint => 3,
            Self::JournalDisplayProjection => 2,
            Self::TranscriptProjection => 1,
        }
    }

    pub fn is_degraded(self) -> bool {
        matches!(
            self,
            Self::Checkpoint | Self::JournalDisplayProjection | Self::TranscriptProjection
        )
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ResumeDegradedReasonV1 {
    LegacyCursorUnknown,
    ProjectionCursorMissing,
    ProjectionCorrupt,
    ProjectionBehind,
    ProjectionDivergent,
    CheckpointFallback,
    DisplayPairsOnly,
    TranscriptOnly,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ResumeRepairActionV1 {
    RebuildProjectionFromJournal,
    DiscardCorruptProjection,
    InspectCanonicalJournal,
    ForkDivergentState,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ResumeCheckpointProjectionV1 {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub blocked_tools: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub recent_tools: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approval_overrides: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interruption: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compaction_state: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pipeline_state: Option<Value>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ResumeActivationProjectionV1 {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub deferred_tool_names: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ResumeTaskProjectionV1 {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub executing_plan_json: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan_goal: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan_config_json: Option<String>,
    #[serde(default)]
    pub plan_execution_rounds: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub contract_json: Option<String>,
}

impl ResumeTaskProjectionV1 {
    pub fn is_empty(&self) -> bool {
        self.executing_plan_json.is_none()
            && self.plan_goal.is_none()
            && self.plan_config_json.is_none()
            && self.plan_execution_rounds == 0
            && self.contract_json.is_none()
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ResumeProviderProjectionV1 {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permission_mode: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config_version_id: Option<String>,
}

impl ResumeProviderProjectionV1 {
    pub fn is_empty(&self) -> bool {
        self.model.is_none() && self.permission_mode.is_none() && self.config_version_id.is_none()
    }
}

/// Independently persisted resume projections. Every payload carries its own
/// causal envelope; selecting a conversation never makes an unrelated side
/// projection current.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ResumeProjectionSetV1 {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checkpoint: Option<CausalProjectionEnvelopeV1<ResumeCheckpointProjectionV1>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task: Option<CausalProjectionEnvelopeV1<ResumeTaskProjectionV1>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<CausalProjectionEnvelopeV1<ResumeProviderProjectionV1>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub activation: Option<CausalProjectionEnvelopeV1<ResumeActivationProjectionV1>>,
}

impl ResumeProjectionSetV1 {
    pub fn admit_at(self, selected: &SessionCursorV1) -> Self {
        Self {
            checkpoint: self
                .checkpoint
                .filter(|projection| projection.is_admissible_at(selected)),
            task: self
                .task
                .filter(|projection| projection.is_admissible_at(selected)),
            provider: self
                .provider
                .filter(|projection| projection.is_admissible_at(selected)),
            activation: self
                .activation
                .filter(|projection| projection.is_admissible_at(selected)),
        }
    }

    pub fn checkpoint_at(
        &self,
        selected: &SessionCursorV1,
    ) -> Option<&ResumeCheckpointProjectionV1> {
        self.checkpoint
            .as_ref()
            .filter(|projection| projection.is_admissible_at(selected))
            .map(|projection| &projection.payload)
    }

    pub fn task_at(&self, selected: &SessionCursorV1) -> Option<&ResumeTaskProjectionV1> {
        self.task
            .as_ref()
            .filter(|projection| projection.is_admissible_at(selected))
            .map(|projection| &projection.payload)
    }

    pub fn provider_at(&self, selected: &SessionCursorV1) -> Option<&ResumeProviderProjectionV1> {
        self.provider
            .as_ref()
            .filter(|projection| projection.is_admissible_at(selected))
            .map(|projection| &projection.payload)
    }

    pub fn activation_at(
        &self,
        selected: &SessionCursorV1,
    ) -> Option<&ResumeActivationProjectionV1> {
        self.activation
            .as_ref()
            .filter(|projection| projection.is_admissible_at(selected))
            .map(|projection| &projection.payload)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ResumeCandidateV1 {
    pub source: ResumeSourceV1,
    pub cursor: SessionCursorV1,
    pub conversation_messages: Vec<Value>,
    /// Content identity for projections whose cursor root identifies a
    /// manifest rather than the flattened message vector.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub materialized_conversation_root_hash: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub degraded_reasons: Vec<ResumeDegradedReasonV1>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub repair_actions: Vec<ResumeRepairActionV1>,
    #[serde(default)]
    pub projections: ResumeProjectionSetV1,
}

impl ResumeCandidateV1 {
    pub fn validates_root(&self) -> bool {
        validates_materialized_root(
            self.source,
            &self.cursor,
            &self.conversation_messages,
            self.materialized_conversation_root_hash.as_deref(),
        )
    }

    pub fn descriptor(&self) -> ResumeDescriptorV1 {
        ResumeDescriptorV1 {
            source: self.source,
            cursor: self.cursor.clone(),
            degraded_reasons: self.degraded_reasons.clone(),
            repair_actions: self.repair_actions.clone(),
        }
    }
}

/// Causal resume metadata without the potentially very large conversation
/// payload.
///
/// Keep this separate from [`ResumeBundleV1`] so selectors and live session
/// state do not clone or retain whole histories merely to compare cursors.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ResumeDescriptorV1 {
    pub source: ResumeSourceV1,
    pub cursor: SessionCursorV1,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub degraded_reasons: Vec<ResumeDegradedReasonV1>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub repair_actions: Vec<ResumeRepairActionV1>,
}

impl ResumeDescriptorV1 {
    pub fn is_degraded(&self) -> bool {
        self.source.is_degraded() || !self.degraded_reasons.is_empty()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ResumeBundleV1 {
    pub schema_version: u32,
    pub cursor: SessionCursorV1,
    pub source: ResumeSourceV1,
    pub conversation_messages: Vec<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub materialized_conversation_root_hash: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub degraded_reasons: Vec<ResumeDegradedReasonV1>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub repair_actions: Vec<ResumeRepairActionV1>,
    #[serde(default)]
    pub projections: ResumeProjectionSetV1,
}

impl ResumeBundleV1 {
    pub fn is_degraded(&self) -> bool {
        self.source.is_degraded() || !self.degraded_reasons.is_empty()
    }

    pub fn validates_root(&self) -> bool {
        validates_materialized_root(
            self.source,
            &self.cursor,
            &self.conversation_messages,
            self.materialized_conversation_root_hash.as_deref(),
        )
    }

    pub fn descriptor(&self) -> ResumeDescriptorV1 {
        ResumeDescriptorV1 {
            source: self.source,
            cursor: self.cursor.clone(),
            degraded_reasons: self.degraded_reasons.clone(),
            repair_actions: self.repair_actions.clone(),
        }
    }

    pub fn activated_deferred_tool_names(&self) -> &[String] {
        self.projections
            .activation_at(&self.cursor)
            .map(|projection| projection.deferred_tool_names.as_slice())
            .unwrap_or_default()
    }
}

#[derive(Debug, Clone, Error, PartialEq, Eq)]
pub enum ResumeSelectionError {
    #[error("no valid resume candidate is available")]
    NoCandidate,
    #[error("no resume candidate materializes the canonical head")]
    CanonicalHeadUnavailable,
    #[error("resume candidates at one causal boundary have different roots")]
    DivergentCandidates,
    #[error("resume candidates belong to different owners, sessions, or branches")]
    DifferentIdentity,
    #[error("resume candidate owner, session, or branch identity is empty")]
    InvalidIdentity,
    #[error("canonical journal candidates do not share one causal lineage")]
    DivergentCanonicalHeads,
    #[error("resume cursor schema {0} is unsupported")]
    UnsupportedCursorSchema(u32),
    #[error("resume projection schema {0} is unsupported")]
    UnsupportedProjectionSchema(u32),
    #[error("legacy resume cursor carries versioned sequence or projection metadata")]
    InvalidLegacyCursor,
}

fn validate_cursor_schema(cursor: &SessionCursorV1) -> Result<(), ResumeSelectionError> {
    if cursor.schema_version == 0 {
        if cursor.projection_schema != 0
            || cursor.journal_event_seq != 0
            || cursor.conversation_seq != 0
            || cursor.compaction_generation != 0
        {
            return Err(ResumeSelectionError::InvalidLegacyCursor);
        }
        return Ok(());
    }
    if cursor.schema_version != SESSION_CURSOR_SCHEMA_VERSION {
        return Err(ResumeSelectionError::UnsupportedCursorSchema(
            cursor.schema_version,
        ));
    }
    if cursor.projection_schema != CONVERSATION_PROJECTION_SCHEMA_VERSION
        && cursor.projection_schema != SEGMENTED_CONVERSATION_PROJECTION_SCHEMA_VERSION
    {
        return Err(ResumeSelectionError::UnsupportedProjectionSchema(
            cursor.projection_schema,
        ));
    }
    Ok(())
}

/// Select a candidate by causal metadata only.
///
/// The returned index refers to `candidates`. Callers can then move the
/// selected payload without cloning it. Payload roots must be validated before
/// calling this function; [`select_resume_bundle`] does that for owned
/// candidates.
pub fn select_resume_candidate_index(
    canonical_head: Option<&SessionCursorV1>,
    candidates: &[ResumeDescriptorV1],
) -> Result<usize, ResumeSelectionError> {
    let Some(identity) = candidates.first().map(|candidate| {
        (
            candidate.cursor.owner_id.as_str(),
            candidate.cursor.session_id.as_str(),
            candidate.cursor.branch_id.as_str(),
        )
    }) else {
        return Err(ResumeSelectionError::NoCandidate);
    };
    if identity.0.trim().is_empty() || identity.1.trim().is_empty() || identity.2.trim().is_empty()
    {
        return Err(ResumeSelectionError::InvalidIdentity);
    }
    for candidate in candidates {
        validate_cursor_schema(&candidate.cursor)?;
    }
    if candidates.iter().any(|candidate| {
        (
            candidate.cursor.owner_id.as_str(),
            candidate.cursor.session_id.as_str(),
            candidate.cursor.branch_id.as_str(),
        ) != identity
    }) {
        return Err(ResumeSelectionError::DifferentIdentity);
    }

    let inferred_canonical_head = if canonical_head.is_none() {
        let mut head = None::<&SessionCursorV1>;
        for candidate in candidates
            .iter()
            .filter(|candidate| candidate.source == ResumeSourceV1::CanonicalJournal)
        {
            head = Some(match head {
                None => &candidate.cursor,
                Some(current) => match cursor_relation(&candidate.cursor, current) {
                    CursorRelationV1::Exact | CursorRelationV1::BehindUnverified => current,
                    CursorRelationV1::Ahead => &candidate.cursor,
                    CursorRelationV1::Divergent
                    | CursorRelationV1::DifferentIdentity
                    | CursorRelationV1::LegacyUnknown => {
                        return Err(ResumeSelectionError::DivergentCanonicalHeads);
                    }
                },
            });
        }
        head
    } else {
        None
    };
    let canonical_head = canonical_head.or(inferred_canonical_head);

    if let Some(head) = canonical_head {
        validate_cursor_schema(head)?;
        if (
            head.owner_id.as_str(),
            head.session_id.as_str(),
            head.branch_id.as_str(),
        ) != identity
        {
            return Err(ResumeSelectionError::DifferentIdentity);
        }
        return candidates
            .iter()
            .enumerate()
            .filter(|(_, candidate)| {
                cursor_relation(&candidate.cursor, head) == CursorRelationV1::Exact
            })
            .max_by_key(|(_, candidate)| candidate.source.reliability_rank())
            .map(|(index, _)| index)
            .ok_or(ResumeSelectionError::CanonicalHeadUnavailable);
    }

    let mut indices = (0..candidates.len()).collect::<Vec<_>>();
    indices.sort_by_key(|&index| {
        let candidate = &candidates[index];
        (
            std::cmp::Reverse(candidate.cursor.schema_version),
            std::cmp::Reverse(candidate.cursor.journal_event_seq),
            std::cmp::Reverse(candidate.cursor.conversation_seq),
            std::cmp::Reverse(candidate.source.reliability_rank()),
        )
    });
    let selected_index = indices[0];
    let selected = &candidates[selected_index];
    if selected.cursor.schema_version > 0
        && indices[1..].iter().any(|&index| {
            let candidate = &candidates[index];
            candidate.cursor.schema_version > 0
                && candidate.cursor.journal_event_seq == selected.cursor.journal_event_seq
                && candidate.cursor.conversation_seq == selected.cursor.conversation_seq
                && candidate.cursor.canonical_root_hash != selected.cursor.canonical_root_hash
        })
    {
        return Err(ResumeSelectionError::DivergentCandidates);
    }
    Ok(selected_index)
}

/// Select one conversation generation without merging independently persisted
/// candidates. Side projections are admitted separately through
/// [`CausalProjectionEnvelopeV1::is_admissible_at`].
pub fn select_resume_bundle(
    canonical_head: Option<&SessionCursorV1>,
    candidates: impl IntoIterator<Item = ResumeCandidateV1>,
) -> Result<ResumeBundleV1, ResumeSelectionError> {
    let mut candidates = candidates
        .into_iter()
        .filter(ResumeCandidateV1::validates_root)
        .collect::<Vec<_>>();
    if candidates.is_empty() {
        return Err(ResumeSelectionError::NoCandidate);
    }
    let descriptors = candidates
        .iter()
        .map(ResumeCandidateV1::descriptor)
        .collect::<Vec<_>>();
    let selected_index = select_resume_candidate_index(canonical_head, &descriptors)?;
    let selected = candidates.swap_remove(selected_index);

    let cursor = selected.cursor;
    Ok(ResumeBundleV1 {
        schema_version: RESUME_BUNDLE_SCHEMA_VERSION,
        cursor,
        source: selected.source,
        conversation_messages: selected.conversation_messages,
        materialized_conversation_root_hash: selected.materialized_conversation_root_hash,
        degraded_reasons: selected.degraded_reasons,
        repair_actions: selected.repair_actions,
        projections: selected.projections,
    })
}

fn validates_materialized_root(
    source: ResumeSourceV1,
    cursor: &SessionCursorV1,
    messages: &[Value],
    materialized_root: Option<&str>,
) -> bool {
    let root = canonical_conversation_root(messages);
    match cursor.projection_schema {
        0 | CONVERSATION_PROJECTION_SCHEMA_VERSION => root == cursor.canonical_root_hash,
        SEGMENTED_CONVERSATION_PROJECTION_SCHEMA_VERSION => {
            source == ResumeSourceV1::CanonicalJournal && materialized_root == Some(root.as_str())
        }
        _ => false,
    }
}

pub fn legacy_resume_cursor(
    owner_id: &str,
    session_id: &str,
    completed_turn: u32,
    messages: &[Value],
) -> SessionCursorV1 {
    SessionCursorV1 {
        schema_version: 0,
        owner_id: owner_id.to_string(),
        session_id: session_id.to_string(),
        branch_id: DEFAULT_CONVERSATION_BRANCH_ID.to_string(),
        completed_turn,
        journal_event_seq: 0,
        conversation_seq: 0,
        canonical_root_hash: canonical_conversation_root(messages),
        projection_schema: 0,
        compaction_generation: 0,
        config_version_id: None,
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn cursor(seq: u64, root: &str) -> SessionCursorV1 {
        SessionCursorV1 {
            schema_version: 1,
            owner_id: "owner".into(),
            session_id: "session".into(),
            branch_id: "main".into(),
            completed_turn: seq as u32,
            journal_event_seq: seq,
            conversation_seq: seq,
            canonical_root_hash: root.into(),
            projection_schema: 1,
            compaction_generation: 0,
            config_version_id: None,
        }
    }

    fn candidate(source: ResumeSourceV1, seq: u64, text: &str) -> ResumeCandidateV1 {
        let messages = vec![json!({"role": "user", "content": text})];
        ResumeCandidateV1 {
            source,
            cursor: cursor(seq, &canonical_conversation_root(&messages)),
            conversation_messages: messages,
            materialized_conversation_root_hash: None,
            degraded_reasons: Vec::new(),
            repair_actions: Vec::new(),
            projections: ResumeProjectionSetV1::default(),
        }
    }

    #[test]
    fn canonical_head_rejects_longer_but_unrelated_projection() {
        let canonical = candidate(ResumeSourceV1::CanonicalJournal, 2, "canonical");
        let head = canonical.cursor.clone();
        let unrelated = candidate(ResumeSourceV1::CslProjection, 3, "longer");
        let selected = select_resume_bundle(Some(&head), [unrelated, canonical]).unwrap();
        assert_eq!(selected.source, ResumeSourceV1::CanonicalJournal);
        assert_eq!(selected.cursor, head);
    }

    #[test]
    fn segmented_canonical_resume_verifies_materialized_content_separately_from_manifest_identity()
    {
        let messages = vec![json!({"role": "user", "content": "canonical"})];
        let content_root = canonical_conversation_root(&messages);
        let mut manifest_cursor = cursor(2, &"a".repeat(64));
        manifest_cursor.projection_schema = SEGMENTED_CONVERSATION_PROJECTION_SCHEMA_VERSION;
        let candidate = ResumeCandidateV1 {
            source: ResumeSourceV1::CanonicalJournal,
            cursor: manifest_cursor.clone(),
            conversation_messages: messages.clone(),
            materialized_conversation_root_hash: Some(content_root),
            degraded_reasons: Vec::new(),
            repair_actions: Vec::new(),
            projections: ResumeProjectionSetV1::default(),
        };

        let selected = select_resume_bundle(Some(&manifest_cursor), [candidate.clone()]).unwrap();
        assert!(selected.validates_root());

        let mut missing_proof = candidate;
        missing_proof.materialized_conversation_root_hash = None;
        assert_eq!(
            select_resume_bundle(Some(&manifest_cursor), [missing_proof]),
            Err(ResumeSelectionError::NoCandidate)
        );
    }

    #[test]
    fn equal_sequence_different_roots_is_a_typed_conflict() {
        let left = candidate(ResumeSourceV1::CslProjection, 2, "left");
        let right = candidate(ResumeSourceV1::Checkpoint, 2, "right");
        assert_eq!(
            select_resume_bundle(None, [left, right]),
            Err(ResumeSelectionError::DivergentCandidates)
        );
    }

    #[test]
    fn lower_cursor_needs_explicit_replay_proof_for_side_projection() {
        let messages = vec![json!({"role": "user", "content": "head"})];
        let head = cursor(3, &canonical_conversation_root(&messages));
        let mut projection =
            CausalProjectionEnvelopeV1::at_cursor(cursor(2, "older-root"), json!({"tasks": []}));
        assert!(!projection.is_admissible_at(&head));
        projection.verified_through_root = Some(head.canonical_root_hash.clone());
        assert!(projection.is_admissible_at(&head));
    }

    #[test]
    fn equal_turn_count_does_not_promote_a_lower_sequence_to_exact() {
        let head = cursor(4, "head");
        let mut inconsistent = cursor(3, "candidate");
        inconsistent.completed_turn = 4;
        assert_eq!(
            cursor_relation(&inconsistent, &head),
            CursorRelationV1::BehindUnverified
        );
    }

    #[test]
    fn legacy_checkpoint_is_explicitly_degraded() {
        let messages = vec![json!({"role": "user", "content": "legacy"})];
        let candidate = ResumeCandidateV1 {
            source: ResumeSourceV1::Checkpoint,
            cursor: legacy_resume_cursor("owner", "session", 7, &messages),
            conversation_messages: messages,
            materialized_conversation_root_hash: None,
            degraded_reasons: vec![
                ResumeDegradedReasonV1::LegacyCursorUnknown,
                ResumeDegradedReasonV1::CheckpointFallback,
            ],
            repair_actions: vec![ResumeRepairActionV1::InspectCanonicalJournal],
            projections: ResumeProjectionSetV1::default(),
        };
        let selected = select_resume_bundle(None, [candidate]).unwrap();
        assert!(selected.is_degraded());
        assert_eq!(selected.cursor.schema_version, 0);
    }

    #[test]
    fn equal_legacy_cursors_never_admit_a_side_projection_as_exact() {
        let messages = vec![json!({"role": "user", "content": "legacy"})];
        let cursor = legacy_resume_cursor("owner", "session", 7, &messages);
        let envelope = CausalProjectionEnvelopeV1::at_cursor(cursor.clone(), json!({"state": 1}));

        assert_eq!(
            cursor_relation(&cursor, &cursor),
            CursorRelationV1::LegacyUnknown
        );
        assert!(!envelope.is_admissible_at(&cursor));
    }

    #[test]
    fn legacy_cursor_cannot_smuggle_a_versioned_ordering_clock() {
        let mut descriptor = candidate(ResumeSourceV1::Checkpoint, 9, "candidate").descriptor();
        descriptor.cursor.schema_version = 0;
        descriptor.cursor.projection_schema = 0;

        assert_eq!(
            select_resume_candidate_index(None, &[descriptor]),
            Err(ResumeSelectionError::InvalidLegacyCursor)
        );
    }

    #[test]
    fn metadata_only_selection_handles_long_session_cursors_without_history_payloads() {
        let candidates = [
            candidate(ResumeSourceV1::CslProjection, 40_000, "older").descriptor(),
            candidate(ResumeSourceV1::Checkpoint, 40_001, "newer").descriptor(),
        ];
        assert_eq!(
            select_resume_candidate_index(None, &candidates),
            Ok(1),
            "causal selection must not require cloning or scanning message payloads"
        );
    }

    #[test]
    fn candidates_from_different_owners_are_rejected_before_clock_comparison() {
        let left = candidate(ResumeSourceV1::Checkpoint, 1, "left").descriptor();
        let mut right = candidate(ResumeSourceV1::Checkpoint, 99, "right").descriptor();
        right.cursor.owner_id = "other-owner".into();
        assert_eq!(
            select_resume_candidate_index(None, &[left, right]),
            Err(ResumeSelectionError::DifferentIdentity)
        );
    }

    #[test]
    fn divergent_canonical_journal_heads_are_not_resolved_by_source_order() {
        let left = candidate(ResumeSourceV1::CanonicalJournal, 7, "left").descriptor();
        let right = candidate(ResumeSourceV1::CanonicalJournal, 7, "right").descriptor();
        assert_eq!(
            select_resume_candidate_index(None, &[left, right]),
            Err(ResumeSelectionError::DivergentCanonicalHeads)
        );
    }

    #[test]
    fn side_projections_are_admitted_independently_at_the_selected_cursor() {
        let mut candidate = candidate(ResumeSourceV1::Checkpoint, 8, "conversation");
        let selected_cursor = candidate.cursor.clone();
        candidate.projections = ResumeProjectionSetV1 {
            checkpoint: Some(CausalProjectionEnvelopeV1::at_cursor(
                cursor(7, "older-root"),
                ResumeCheckpointProjectionV1 {
                    blocked_tools: vec!["stale-tool".into()],
                    ..Default::default()
                },
            )),
            task: Some(CausalProjectionEnvelopeV1::unversioned(
                ResumeTaskProjectionV1 {
                    plan_goal: Some("unversioned task".into()),
                    ..Default::default()
                },
            )),
            provider: None,
            activation: Some(CausalProjectionEnvelopeV1::at_cursor(
                selected_cursor.clone(),
                ResumeActivationProjectionV1 {
                    deferred_tool_names: vec!["github".into()],
                },
            )),
        };

        let selected = select_resume_bundle(Some(&selected_cursor), [candidate]).unwrap();
        assert_eq!(selected.activated_deferred_tool_names(), ["github"]);
        assert!(
            selected
                .projections
                .checkpoint_at(&selected.cursor)
                .is_none(),
            "an older checkpoint is not ancestry proof"
        );
        assert!(
            selected.projections.task_at(&selected.cursor).is_none(),
            "an unversioned task projection must not resume into a typed generation"
        );
        assert!(
            selected
                .projections
                .activation_at(&selected.cursor)
                .is_some()
        );
    }
}
