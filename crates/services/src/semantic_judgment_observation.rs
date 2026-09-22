//! Bounded C3 semantic observations over the existing trace journal.
//!
//! This is not an execution/usage ledger. Successful queries never establish
//! complete history: trace buffering, ingestion and retention may lose facts.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use astra_core::SharedPool;
use astra_turn_types::{
    SEMANTIC_JUDGMENT_ID_MAX_BYTES, SEMANTIC_JUDGMENT_TRACE_ATTR, SemanticJudgmentInvocationV1,
    SemanticJudgmentObservationV1, SemanticJudgmentValidationError,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sqlx::Row;

use crate::cancellation_safe_db::CancellationSafePoolConnection;
use crate::session_journal::TraceSpanBuilder;
use crate::{ServiceError, ServiceResult};

pub const SEMANTIC_JUDGMENT_TRACE_NAME: &str = "semantic_judgment";
pub const MAX_SEMANTIC_JUDGMENT_CANDIDATES: usize = 512;
// Includes the escaped 8-KiB typed attribute and the ordinary trace envelope.
pub const MAX_SEMANTIC_JUDGMENT_TRACE_BYTES: usize = 32_768;
const MAX_SEMANTIC_JUDGMENT_EXECUTION_ATTEMPTS: usize = 8;

/// Redacted projection of the canonical parser result, never a second classifier.
/// Runtime control diagnostics remain with their existing owner. In particular,
/// raw malformed text and free-form error details never cross this boundary.
pub fn request_judgment_result(
    result: &Result<crate::WorkAdmissionClassification, crate::TurnIntentJudgeError>,
) -> astra_turn_types::RequestJudgmentResultV1 {
    checked_request_judgment_result(result).unwrap_or(
        astra_turn_types::RequestJudgmentResultV1::Invalid {
            reason: astra_turn_types::SemanticJudgmentInvalidV1::InvalidContract,
        },
    )
}

/// Project the decision that crossed the runtime admission boundary. This is
/// intentionally separate from the model classification observation: a
/// successful provider response is not execution authority until this result
/// is reconciled and adopted by the host.
pub fn accepted_request_judgment_result(
    decision: &crate::WorkAdmissionDecision,
) -> astra_turn_types::RequestJudgmentResultV1 {
    let classification = crate::WorkAdmissionClassification {
        work_lifecycle: decision.turn_intent().work_lifecycle,
        activation: decision.activation(),
        domain: decision.domain(),
        workspace_mutation: decision.workspace_mutation(),
        mutation_completion_scope: decision.mutation_completion_scope(),
        execution_topology: decision.execution_topology(),
        required_capabilities: decision.required_capabilities().to_vec(),
    };
    request_judgment_result(&Ok(classification))
}

fn checked_request_judgment_result(
    result: &Result<crate::WorkAdmissionClassification, crate::TurnIntentJudgeError>,
) -> Result<astra_turn_types::RequestJudgmentResultV1, SemanticJudgmentValidationError> {
    use crate::TurnIntentJudgeError as Error;
    use astra_config::user_profile::{
        MutationCompletionScope as Scope, TurnIntentDomain as Domain, WorkLifecycleIntent,
        WorkspaceMutationIntent as Mutation,
    };
    use astra_turn_types::*;
    let invalid = SemanticJudgmentValidationError::InvalidContract;
    let field = |id: &str| RequestJudgmentFieldV1::from_question_id(id).ok_or(invalid);
    let projected = match result {
        Ok(c) if c.required_capabilities.len() > 2 => return Err(invalid),
        Ok(c) => RequestJudgmentResultV1::Decided {
            classification: RequestJudgmentClassificationV1 {
                work_required: match c.work_lifecycle {
                    WorkLifecycleIntent::Required => true,
                    WorkLifecycleIntent::NotRequired => false,
                    WorkLifecycleIntent::Unknown => return Err(invalid),
                },
                activation_deferred: c.activation == crate::WorkAdmissionActivation::Defer,
                domain: c.domain.map(|d| match d {
                    Domain::GitHub => RequestJudgmentDomainV1::Github,
                    Domain::Git => RequestJudgmentDomainV1::Git,
                    Domain::Code => RequestJudgmentDomainV1::Code,
                    Domain::Memory => RequestJudgmentDomainV1::Memory,
                    Domain::Web => RequestJudgmentDomainV1::Web,
                    Domain::System => RequestJudgmentDomainV1::System,
                    Domain::Database => RequestJudgmentDomainV1::Database,
                }),
                mutation: match c.workspace_mutation {
                    Mutation::ReadOnly => RequestJudgmentMutationV1::ReadOnly,
                    Mutation::MayMutate => RequestJudgmentMutationV1::MayMutate,
                    Mutation::MustMutate => RequestJudgmentMutationV1::MustMutate,
                    Mutation::Unknown => RequestJudgmentMutationV1::Unknown,
                },
                scope: match c.mutation_completion_scope {
                    Scope::Unknown => RequestJudgmentScopeV1::Unknown,
                    Scope::Workspace => RequestJudgmentScopeV1::Workspace,
                    Scope::External => RequestJudgmentScopeV1::External,
                    Scope::Mixed => RequestJudgmentScopeV1::Mixed,
                },
                parallel_subruns: c.execution_topology
                    == crate::WorkExecutionTopology::ParallelSubruns,
                capabilities: c
                    .required_capabilities
                    .iter()
                    .map(|c| match c {
                        crate::WorkAdmissionCapability::Web => RequestJudgmentCapabilityV1::Web,
                        crate::WorkAdmissionCapability::AgentSpawner => {
                            RequestJudgmentCapabilityV1::AgentSpawner
                        }
                    })
                    .collect(),
            },
        },
        Err(Error::Uncertain { diagnostics }) => {
            // Check cardinality before copying any parser-owned diagnostic.
            if diagnostics.evidence.len() > REQUEST_JUDGMENT_MAX_FIELDS
                || diagnostics.uncertain_fields.len() > REQUEST_JUDGMENT_MAX_FIELDS
            {
                return Err(invalid);
            }
            RequestJudgmentResultV1::Abstained {
                uncertain_fields: diagnostics
                    .uncertain_fields
                    .iter()
                    .map(|id| field(id))
                    .collect::<Result<_, _>>()?,
                assessment: RequestJudgmentAssessmentV1 {
                    provenance: diagnostics.provenance,
                    fields: diagnostics
                        .evidence
                        .iter()
                        .map(|(id, evidence)| {
                            Ok(RequestJudgmentFieldAssessmentV1 {
                                field: field(id)?,
                                score: SemanticJudgmentScoreV1::from_f64(evidence.value)?,
                            })
                        })
                        .collect::<Result<_, SemanticJudgmentValidationError>>()?,
                },
            }
        }
        Err(Error::Conflicting { fields, .. }) => {
            if fields.len() > REQUEST_JUDGMENT_MAX_FIELDS {
                return Err(invalid);
            }
            RequestJudgmentResultV1::Conflicting {
                fields: fields
                    .iter()
                    .map(|id| field(id))
                    .collect::<Result<_, _>>()?,
            }
        }
        // This error currently erases the codec's typed cause. Do not infer
        // malformed JSON versus schema errors by parsing its raw/detail text.
        Err(Error::Malformed { .. }) => RequestJudgmentResultV1::Invalid {
            reason: SemanticJudgmentInvalidV1::InvalidContract,
        },
        Err(Error::UnsupportedCombination(_) | Error::TrustedWorkflowTopologyConflict(_)) => {
            RequestJudgmentResultV1::Invalid {
                reason: SemanticJudgmentInvalidV1::UnsupportedCombination,
            }
        }
        Err(Error::Rejected(_)) => RequestJudgmentResultV1::Unavailable {
            reason: SemanticJudgmentUnavailableReasonV1::UnexpectedFinish,
            delivery: SemanticJudgmentDeliveryV1::ResponseReceived,
        },
        Err(Error::Inference(error)) => RequestJudgmentResultV1::Unavailable {
            reason: match error.kind {
                astra_core::ErrorKind::Cancelled => SemanticJudgmentUnavailableReasonV1::Cancelled,
                astra_core::ErrorKind::ProviderDeadline => {
                    SemanticJudgmentUnavailableReasonV1::Deadline
                }
                _ => SemanticJudgmentUnavailableReasonV1::ExecutionError,
            },
            delivery: SemanticJudgmentDeliveryV1::Unresolved,
        },
    };
    projected.validate()?;
    Ok(projected)
}

fn valid_span_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= SEMANTIC_JUDGMENT_ID_MAX_BYTES
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_-.:".contains(&b))
}

/// The caller mints a distinct observation identity, stable across replay.
/// `evaluation_span_id` correlates stages; it is not each stage's identity.
/// Session/turn binding and persistence remain owned by TurnEventBuffer.
pub fn semantic_judgment_trace(
    observation_span_id: &str,
    observation: &SemanticJudgmentObservationV1,
    observed_at_us: u64,
) -> Result<TraceSpanBuilder, SemanticJudgmentValidationError> {
    if !valid_span_id(observation_span_id) {
        return Err(SemanticJudgmentValidationError::InvalidContract);
    }
    let attrs = HashMap::from([(
        SEMANTIC_JUDGMENT_TRACE_ATTR.to_owned(),
        observation.to_json()?,
    )]);
    Ok(TraceSpanBuilder::default()
        .span_id(observation_span_id.to_owned())
        .name(SEMANTIC_JUDGMENT_TRACE_NAME.to_owned())
        .trace_id(Some(observation.correlation.run_id.clone()))
        .turn(Some(observation.correlation.turn))
        .start_us(observed_at_us)
        .end_us(observed_at_us)
        .attrs(Some(&attrs)))
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SemanticJudgmentTraceObservation {
    pub observation_span_id: String,
    pub observation: SemanticJudgmentObservationV1,
}

/// Closed, payload-free reasons. Never return raw metadata or serde/SQL errors.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SemanticJudgmentCaptureGap {
    TraceMayBeDropped,
    SourceUnavailable,
    CandidateLimit,
    ObservationLimit,
    OversizedMetadata,
    InvalidObservation,
    ScopeMismatch,
    ConflictingIdentity,
    ConflictingEvaluation,
    JournalEvictionObserved,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SemanticJudgmentCapture {
    pub available: bool,
    /// Always true, including a successful empty query. No complete-history claim.
    pub capture_incomplete: bool,
    /// Query/projection truncation, distinct from potential upstream trace loss.
    pub truncated: bool,
    pub candidates_scanned: usize,
    pub duplicate_observations: usize,
    /// Exact omissions from valid, nonconflicting facts inside this candidate window.
    pub omitted_observations: usize,
    pub observations: Vec<SemanticJudgmentTraceObservation>,
    pub gaps: Vec<SemanticJudgmentCaptureGap>,
}

impl SemanticJudgmentCapture {
    pub fn unavailable() -> Self {
        Self {
            available: false,
            gaps: vec![
                SemanticJudgmentCaptureGap::TraceMayBeDropped,
                SemanticJudgmentCaptureGap::SourceUnavailable,
            ],
            ..Self::empty()
        }
    }

    fn empty() -> Self {
        Self {
            available: true,
            capture_incomplete: true,
            truncated: false,
            candidates_scanned: 0,
            duplicate_observations: 0,
            omitted_observations: 0,
            observations: Vec::new(),
            gaps: vec![SemanticJudgmentCaptureGap::TraceMayBeDropped],
        }
    }
}

/// Bounded row carrier, also usable by journal/replay tests. Deliberately no
/// Debug/Serialize: raw stored metadata must not escape into diagnostics.
pub struct SemanticJudgmentTraceRow {
    pub user_id: String,
    pub session_id: String,
    pub metadata_json: Option<String>,
    /// SQL suppressed an oversized document before transferring it to Rust.
    pub metadata_oversized: bool,
}

fn parse_metadata(raw: &str) -> Result<Value, SemanticJudgmentCaptureGap> {
    if raw.len() > MAX_SEMANTIC_JUDGMENT_TRACE_BYTES {
        return Err(SemanticJudgmentCaptureGap::OversizedMetadata);
    }
    let value: Value =
        serde_json::from_str(raw).map_err(|_| SemanticJudgmentCaptureGap::InvalidObservation)?;
    if !value.is_object() {
        return Err(SemanticJudgmentCaptureGap::InvalidObservation);
    }
    Ok(value)
}

fn decode_metadata(
    metadata: &Value,
) -> Result<Option<SemanticJudgmentTraceObservation>, SemanticJudgmentCaptureGap> {
    if metadata.get("name").and_then(Value::as_str) != Some(SEMANTIC_JUDGMENT_TRACE_NAME) {
        return Ok(None);
    }
    let invalid = SemanticJudgmentCaptureGap::InvalidObservation;
    let span = metadata
        .get("span_id")
        .and_then(Value::as_str)
        .filter(|id| valid_span_id(id))
        .ok_or(invalid)?;
    let raw = metadata
        .get("attrs")
        .and_then(|a| a.get(SEMANTIC_JUDGMENT_TRACE_ATTR))
        .and_then(Value::as_str)
        .ok_or(invalid)?;
    let observation =
        SemanticJudgmentObservationV1::from_json(raw).map_err(|error| match error {
            SemanticJudgmentValidationError::Oversized => {
                SemanticJudgmentCaptureGap::OversizedMetadata
            }
            _ => invalid,
        })?;
    if metadata.get("trace_id").and_then(Value::as_str) != Some(&observation.correlation.run_id) {
        return Err(SemanticJudgmentCaptureGap::ScopeMismatch);
    }
    Ok(Some(SemanticJudgmentTraceObservation {
        observation_span_id: span.to_owned(),
        observation,
    }))
}

/// Decode only the exact typed trace attribute; unrelated traces are ignored.
pub fn decode_semantic_judgment_trace(
    metadata_json: &str,
) -> Result<Option<SemanticJudgmentTraceObservation>, SemanticJudgmentCaptureGap> {
    decode_metadata(&parse_metadata(metadata_json)?)
}

fn terminal_evaluation_key(
    observation: &SemanticJudgmentObservationV1,
) -> (
    String,
    u32,
    String,
    astra_turn_types::RequestJudgmentStageV1,
) {
    (
        observation.correlation.run_id.clone(),
        observation.correlation.turn,
        observation.correlation.evaluation_span_id.clone(),
        observation.fact.stage,
    )
}

/// Rows must be newest-first in canonical DB order (created_at, event_id).
/// Both work and retained memory are capped, including duplicate/conflict state.
/// One extra candidate is inspected only for overflow, never decoded.
pub fn project_semantic_judgment_observations(
    rows: impl IntoIterator<Item = SemanticJudgmentTraceRow>,
    user_id: &str,
    session_id: &str,
    max_candidates: usize,
    max_observations: usize,
) -> SemanticJudgmentCapture {
    use SemanticJudgmentCaptureGap as Gap;
    let limit = max_candidates.min(MAX_SEMANTIC_JUDGMENT_CANDIDATES);
    let output_limit = max_observations.min(MAX_SEMANTIC_JUDGMENT_CANDIDATES);
    let mut capture = SemanticJudgmentCapture::empty();
    let mut gaps = BTreeSet::from([Gap::TraceMayBeDropped]);
    let mut facts = BTreeMap::<String, (usize, SemanticJudgmentTraceObservation)>::new();
    let mut conflicts = BTreeSet::new();
    let mut terminal_facts = BTreeMap::new();
    let mut terminal_conflicts = BTreeSet::new();
    for (index, row) in rows.into_iter().take(limit + 1).enumerate() {
        if index == limit {
            capture.truncated = true;
            gaps.insert(Gap::CandidateLimit);
            break;
        }
        capture.candidates_scanned += 1;
        if row.user_id != user_id || row.session_id != session_id {
            gaps.insert(Gap::ScopeMismatch);
            continue;
        }
        if row.metadata_oversized {
            gaps.insert(Gap::OversizedMetadata);
            continue;
        }
        let metadata = match row
            .metadata_json
            .as_deref()
            .ok_or(Gap::InvalidObservation)
            .and_then(parse_metadata)
        {
            Ok(value) => value,
            Err(gap) => {
                gaps.insert(gap);
                continue;
            }
        };
        if metadata
            .get("dropped_events_before")
            .and_then(Value::as_u64)
            .is_some_and(|n| n > 0)
        {
            gaps.insert(Gap::JournalEvictionObserved);
        }
        let fact = match decode_metadata(&metadata) {
            Ok(Some(fact)) => fact,
            Ok(None) => continue,
            Err(gap) => {
                gaps.insert(gap);
                // A malformed copy of an identifiable semantic observation
                // cannot leave another copy falsely looking uncontested.
                if metadata.get("name").and_then(Value::as_str)
                    == Some(SEMANTIC_JUDGMENT_TRACE_NAME)
                    && let Some(id) = metadata
                        .get("span_id")
                        .and_then(Value::as_str)
                        .filter(|id| valid_span_id(id))
                {
                    if facts.remove(id).is_some() || conflicts.contains(id) {
                        gaps.insert(Gap::ConflictingIdentity);
                    }
                    conflicts.insert(id.to_owned());
                }
                continue;
            }
        };
        // Preserve terminal contradictions even if span-level reconciliation
        // subsequently removes either copy. All state shares the candidate cap.
        {
            let key = terminal_evaluation_key(&fact.observation);
            if let Some(previous) = terminal_facts.get(&key) {
                if previous != &fact.observation {
                    terminal_conflicts.insert(key);
                    gaps.insert(Gap::ConflictingEvaluation);
                }
            } else {
                terminal_facts.insert(key, fact.observation.clone());
            }
        }
        let id = &fact.observation_span_id;
        if conflicts.contains(id) {
            gaps.insert(Gap::ConflictingIdentity);
        } else if let Some((_, previous)) = facts.get(id) {
            if previous == &fact {
                capture.duplicate_observations += 1;
            } else {
                facts.remove(id);
                conflicts.insert(id.clone());
                gaps.insert(Gap::ConflictingIdentity);
            }
        } else {
            facts.insert(id.clone(), (index, fact));
        }
    }
    let mut ordered: Vec<_> = facts.into_values().collect();
    ordered.sort_by_key(|(index, _)| *index);
    // A terminal evaluation is one logical fact even if transport/replay
    // minted another observation span. Initial and clarification stay distinct.
    // Validate the whole bounded window before applying the output limit.
    let mut evaluations = BTreeSet::new();
    ordered.retain(|(_, fact)| {
        let key = terminal_evaluation_key(&fact.observation);
        if terminal_conflicts.contains(&key) {
            return false;
        }
        if !evaluations.insert(key) {
            capture.duplicate_observations += 1;
            return false;
        }
        true
    });
    capture.omitted_observations = ordered.len().saturating_sub(output_limit);
    if capture.omitted_observations > 0 {
        capture.truncated = true;
        gaps.insert(Gap::ObservationLimit);
    }
    capture.observations = ordered
        .into_iter()
        .take(output_limit)
        .map(|(_, fact)| fact)
        .collect();
    capture.gaps = gaps.into_iter().collect();
    capture
}

// Bound candidate rows using the existing owner/session/type index. Do not
// search indefinitely through unrelated traces to fill the observation budget.
// CASE bounds bytes transferred/parsed; it does not claim bounded DB JSON work.
const LOAD_SQL: &str = "SELECT user_id, session_id, \
    CASE WHEN OCTET_LENGTH(CAST(metadata AS CHAR)) <= ? THEN CAST(metadata AS CHAR) ELSE NULL END AS metadata_json, \
    CASE WHEN OCTET_LENGTH(CAST(metadata AS CHAR)) > ? THEN 1 ELSE 0 END AS metadata_oversized \
    FROM agent_events WHERE user_id = ? AND session_id = ? AND event_type = 'trace_span' \
    ORDER BY created_at DESC, event_id DESC LIMIT ?";

/// Authenticated session-scoped read. No payload-controlled run/owner filters.
/// Callers supply their normal query deadline and map errors to unavailable().
pub async fn load_semantic_judgment_observations(
    pool: &SharedPool,
    user_id: &str,
    session_id: &str,
    max_candidates: usize,
    max_observations: usize,
) -> ServiceResult<SemanticJudgmentCapture> {
    if user_id.trim().is_empty()
        || user_id.len() > 128
        || session_id.trim().is_empty()
        || session_id.len() > 64
    {
        return Err(ServiceError::invalid(
            "invalid semantic observation subject",
        ));
    }
    let mut connection = CancellationSafePoolConnection::acquire(pool.get())
        .await
        .map_err(|_| ServiceError::persistence("semantic observation connection unavailable"))?;
    let owned = crate::storage::agent_session_exists_for_user(
        connection.connection_mut(),
        session_id,
        user_id,
    )
    .await
    .map_err(|_| ServiceError::persistence("semantic observation scope unavailable"))?;
    if !owned {
        connection.release();
        return Err(ServiceError::not_found("session not found"));
    }
    let limit = max_candidates.min(MAX_SEMANTIC_JUDGMENT_CANDIDATES);
    let rows = sqlx::query(LOAD_SQL)
        .bind(MAX_SEMANTIC_JUDGMENT_TRACE_BYTES as i64)
        .bind(MAX_SEMANTIC_JUDGMENT_TRACE_BYTES as i64)
        .bind(user_id)
        .bind(session_id)
        .bind((limit + 1) as i64)
        .fetch_all(connection.connection_mut())
        .await
        .map_err(|_| ServiceError::persistence("semantic observations unavailable"))?;
    connection.release();
    let decode_error = || ServiceError::persistence("semantic observation row unavailable");
    let rows = rows
        .into_iter()
        .map(|row| {
            Ok(SemanticJudgmentTraceRow {
                user_id: row.try_get("user_id").map_err(|_| decode_error())?,
                session_id: row.try_get("session_id").map_err(|_| decode_error())?,
                metadata_json: row.try_get("metadata_json").map_err(|_| decode_error())?,
                metadata_oversized: row
                    .try_get::<i64, _>("metadata_oversized")
                    .map_err(|_| decode_error())?
                    != 0,
            })
        })
        .collect::<ServiceResult<Vec<_>>>()?;
    Ok(project_semantic_judgment_observations(
        rows,
        user_id,
        session_id,
        limit,
        max_observations,
    ))
}

/// Shared consumer coverage. Missing/excluded sources never become zero counts.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SemanticJudgmentCoverage {
    #[default]
    NotObserved,
    CaptureIncomplete,
    SourceExcluded,
    NoPool,
    Timeout,
    QueryFailed,
    SourceUnavailable,
}

/// Coverage of the separate physical-execution lookup. This is deliberately
/// independent from semantic trace capture: a captured judgment can have no
/// usable invocation identity, and a ledger lookup can fail without erasing
/// the captured judgment itself.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SemanticJudgmentExecutionCoverage {
    #[default]
    NotObserved,
    /// The current observation depth intentionally did not request a ledger
    /// lookup. This is not a claim that no physical execution exists.
    Deferred,
    LookupComplete,
    LookupIncomplete,
    SourceExcluded,
    NoPool,
    Timeout,
    QueryFailed,
    SourceUnavailable,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SemanticJudgmentExecutionAssociation {
    /// The invocation and at least one physical provider attempt agree with
    /// the semantic observation's immutable scope.
    Matched,
    /// No exact invocation row was found for the captured identity.
    NotCaptured,
    /// The bounded lookup was cut off before this identity could be resolved.
    LookupIncomplete,
    /// The identity or route/attempt facts disagree, so no model is trusted.
    Conflicting,
    /// The exact ledger lookup was unavailable.
    Unavailable,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SemanticJudgmentExecutionAttempt {
    pub attempt_id: String,
    pub attempt_index: u32,
    pub provider: String,
    pub model_name: String,
    pub protocol: String,
    pub status: String,
}

/// Physical execution evidence attached to one semantic stage. This carries
/// identity and bounded attempt status only; token accounting remains owned by
/// the canonical usage projection.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SemanticJudgmentExecutionExplanation {
    pub run_id: String,
    pub turn: u32,
    pub round: u32,
    pub evaluation_span_id: String,
    pub stage: astra_turn_types::RequestJudgmentStageV1,
    pub invocation_id: Option<String>,
    pub association: SemanticJudgmentExecutionAssociation,
    pub provider: Option<String>,
    pub model_name: Option<String>,
    pub attempts: Vec<SemanticJudgmentExecutionAttempt>,
    pub attempts_truncated: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SemanticJudgmentStageCounts {
    pub evaluated: usize,
    pub decisions: usize,
    pub abstained: usize,
    pub invalid: usize,
    pub not_dispatched: usize,
    pub evaluation_unavailable: usize,
    pub conflicting: usize,
    pub initial: usize,
    pub clarification: usize,
}

/// A typed, bounded projection of a captured decided result.  This is derived
/// from the already authenticated observations; it is not a second decision
/// record and it does not assert that the runtime adopted the classification.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SemanticJudgmentDecisionSummary {
    pub run_id: String,
    pub turn: u32,
    pub round: u32,
    pub evaluation_span_id: String,
    pub stage: astra_turn_types::RequestJudgmentStageV1,
    pub classification: astra_turn_types::RequestJudgmentClassificationV1,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SemanticJudgmentScope {
    SessionTraceAtRead,
    LocalJournalAtRead,
}

/// Project an owner-authorized journal window through the canonical trace decoder.
/// Local identity comes from the selected artifact store, never from trace attributes.
pub fn project_local_semantic_judgments(
    window: &crate::session_journal::JournalObservationWindow,
    owner: &crate::OwnerScope,
    session_id: &str,
    depth: astra_core::ObservationDepth,
) -> SemanticJudgmentView {
    use crate::session_journal::JournalEventType;
    let rows = window
        .events
        .iter()
        .rev()
        .filter(|event| event.event_type == JournalEventType::TraceSpan)
        .map(|event| {
            let metadata_json = event.metadata.as_ref().map(Value::to_string);
            // Reuse the canonical decoder, including its size/strict-schema checks.
            // Missing or conflicting journal turns cannot authenticate a typed fact.
            let turn_mismatch = metadata_json
                .as_deref()
                .and_then(|raw| decode_semantic_judgment_trace(raw).ok().flatten())
                .is_some_and(|fact| event.turn != Some(fact.observation.correlation.turn));
            SemanticJudgmentTraceRow {
                user_id: owner.id().to_string(),
                session_id: if turn_mismatch {
                    String::new()
                } else {
                    event.session_id.clone().unwrap_or_default()
                },
                metadata_json,
                metadata_oversized: false,
            }
        });
    let mut capture = if window.available {
        project_semantic_judgment_observations(rows, owner.id(), session_id, 512, 32)
    } else {
        SemanticJudgmentCapture::unavailable()
    };
    if window.truncated {
        capture.truncated = true;
        capture
            .gaps
            .push(SemanticJudgmentCaptureGap::CandidateLimit);
    }
    if window.malformed_records > 0 {
        capture
            .gaps
            .push(SemanticJudgmentCaptureGap::InvalidObservation);
    }
    capture.gaps.sort();
    capture.gaps.dedup();
    let mut view = SemanticJudgmentView::from_capture(capture);
    view.mark_execution_unavailable();
    let mut view = view.bounded(depth);
    view.scope = SemanticJudgmentScope::LocalJournalAtRead;
    view
}

/// The same bounded semantic view for introspection and reflection. Counts
/// describe captured stage observations, NEVER physical inference attempts.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SemanticJudgmentView {
    pub scope: SemanticJudgmentScope,
    pub coverage: SemanticJudgmentCoverage,
    #[serde(default)]
    pub execution_coverage: SemanticJudgmentExecutionCoverage,
    pub capture_incomplete: bool,
    pub capture_truncated: bool,
    pub counts: Option<SemanticJudgmentStageCounts>,
    pub capture_gaps: Vec<SemanticJudgmentCaptureGap>,
    pub capture_omitted_observations: usize,
    pub omitted_details: usize,
    #[serde(default)]
    pub execution_omitted: usize,
    pub observations: Vec<SemanticJudgmentTraceObservation>,
    #[serde(default)]
    pub execution_explanations: Vec<SemanticJudgmentExecutionExplanation>,
}

impl Default for SemanticJudgmentView {
    fn default() -> Self {
        Self::unavailable(SemanticJudgmentCoverage::NotObserved)
    }
}

impl SemanticJudgmentView {
    pub fn unavailable(coverage: SemanticJudgmentCoverage) -> Self {
        let execution_coverage = match coverage {
            SemanticJudgmentCoverage::SourceExcluded => {
                SemanticJudgmentExecutionCoverage::SourceExcluded
            }
            SemanticJudgmentCoverage::NoPool => SemanticJudgmentExecutionCoverage::NoPool,
            SemanticJudgmentCoverage::Timeout => SemanticJudgmentExecutionCoverage::Timeout,
            SemanticJudgmentCoverage::QueryFailed => SemanticJudgmentExecutionCoverage::QueryFailed,
            SemanticJudgmentCoverage::SourceUnavailable => {
                SemanticJudgmentExecutionCoverage::SourceUnavailable
            }
            SemanticJudgmentCoverage::NotObserved | SemanticJudgmentCoverage::CaptureIncomplete => {
                SemanticJudgmentExecutionCoverage::NotObserved
            }
        };
        Self {
            scope: SemanticJudgmentScope::SessionTraceAtRead,
            coverage,
            execution_coverage,
            capture_incomplete: true,
            capture_truncated: false,
            counts: None,
            capture_gaps: vec![],
            capture_omitted_observations: 0,
            omitted_details: 0,
            execution_omitted: 0,
            observations: vec![],
            execution_explanations: vec![],
        }
    }

    pub fn from_capture(capture: SemanticJudgmentCapture) -> Self {
        use astra_turn_types::{
            RequestJudgmentResultV1 as Result, RequestJudgmentStageV1 as Stage,
        };
        if !capture.available {
            return Self::unavailable(SemanticJudgmentCoverage::SourceUnavailable);
        }
        let mut counts = SemanticJudgmentStageCounts::default();
        for observation in &capture.observations {
            match observation.observation.fact.stage {
                Stage::Initial => counts.initial += 1,
                Stage::Clarification => counts.clarification += 1,
            }
            match &observation.observation.fact.result {
                Result::Decided { .. } => {
                    counts.evaluated += 1;
                    counts.decisions += 1;
                }
                Result::Abstained { .. } => {
                    counts.evaluated += 1;
                    counts.abstained += 1;
                }
                Result::Conflicting { .. } => {
                    counts.evaluated += 1;
                    counts.conflicting += 1;
                }
                Result::Invalid { .. } => {
                    counts.evaluated += 1;
                    counts.invalid += 1;
                }
                Result::NotDispatched { .. } => counts.not_dispatched += 1,
                Result::Unavailable { .. } => counts.evaluation_unavailable += 1,
            }
        }
        Self {
            scope: SemanticJudgmentScope::SessionTraceAtRead,
            coverage: SemanticJudgmentCoverage::CaptureIncomplete,
            execution_coverage: SemanticJudgmentExecutionCoverage::NotObserved,
            capture_incomplete: true,
            capture_truncated: capture.truncated,
            counts: Some(counts),
            capture_gaps: capture.gaps,
            capture_omitted_observations: capture.omitted_observations,
            omitted_details: 0,
            execution_omitted: 0,
            observations: capture.observations,
            execution_explanations: vec![],
        }
    }

    /// Local journal facts may retain the typed invocation identity but do not
    /// authorize a server-ledger lookup. Preserve that distinction explicitly.
    fn mark_execution_unavailable(&mut self) {
        mark_execution_lookup_unavailable(
            self,
            SemanticJudgmentExecutionCoverage::SourceUnavailable,
        );
    }

    fn defer_execution_lookup(&mut self) {
        if self.observations.iter().any(|observation| {
            matches!(
                &observation.observation.correlation.invocation,
                SemanticJudgmentInvocationV1::Known { .. }
            )
        }) {
            self.execution_coverage = SemanticJudgmentExecutionCoverage::Deferred;
            self.execution_explanations.clear();
        }
    }

    /// Count all captured observations before limiting detail. Idempotent for
    /// the same depth; exact display omissions are not upstream loss counts.
    pub fn bounded(mut self, depth: astra_core::ObservationDepth) -> Self {
        use astra_core::ObservationDepth;
        let limit = match depth {
            ObservationDepth::Hint => 2,
            ObservationDepth::Summary => 8,
            ObservationDepth::Diagnostic => 16,
            ObservationDepth::Forensic => 32,
        };
        self.omitted_details += self.observations.len().saturating_sub(limit);
        let visible_keys = self
            .observations
            .iter()
            .take(limit)
            .map(|observation| {
                (
                    observation.observation.correlation.run_id.clone(),
                    observation.observation.correlation.turn,
                    observation.observation.correlation.round,
                    observation
                        .observation
                        .correlation
                        .evaluation_span_id
                        .clone(),
                    observation.observation.fact.stage,
                )
            })
            .collect::<BTreeSet<_>>();
        self.observations.truncate(limit);
        let before = self.execution_explanations.len();
        self.execution_explanations.retain(|explanation| {
            visible_keys.contains(&(
                explanation.run_id.clone(),
                explanation.turn,
                explanation.round,
                explanation.evaluation_span_id.clone(),
                explanation.stage,
            ))
        });
        self.execution_omitted += before.saturating_sub(self.execution_explanations.len());
        self
    }

    /// Return only the decided classifications that are present in the
    /// bounded observation window.  Missing or omitted observations are not
    /// reconstructed from the aggregate counts.
    pub fn decision_summaries(&self) -> Vec<SemanticJudgmentDecisionSummary> {
        self.observations
            .iter()
            .filter_map(|observation| {
                let astra_turn_types::RequestJudgmentResultV1::Decided { classification } =
                    &observation.observation.fact.result
                else {
                    return None;
                };
                Some(SemanticJudgmentDecisionSummary {
                    run_id: observation.observation.correlation.run_id.clone(),
                    turn: observation.observation.correlation.turn,
                    round: observation.observation.correlation.round,
                    evaluation_span_id: observation
                        .observation
                        .correlation
                        .evaluation_span_id
                        .clone(),
                    stage: observation.observation.fact.stage,
                    classification: classification.clone(),
                })
            })
            .collect()
    }

    fn captured_evaluation_count(&self) -> usize {
        self.observations
            .iter()
            .map(|observation| {
                (
                    observation.observation.correlation.run_id.as_str(),
                    observation.observation.correlation.turn,
                    observation
                        .observation
                        .correlation
                        .evaluation_span_id
                        .as_str(),
                )
            })
            .collect::<BTreeSet<_>>()
            .len()
    }

    fn captured_result_labels(&self, limit: usize, detailed: bool) -> Vec<String> {
        self.observations
            .iter()
            .take(limit)
            .map(|observation| {
                let result = &observation.observation.fact.result;
                let mut label = format!(
                    "{}: {}",
                    stage_label(observation.observation.fact.stage),
                    if detailed {
                        result.detail_label()
                    } else {
                        result.presentation_label()
                    }
                );
                if let Some(execution) = self.execution_explanation(observation) {
                    label.push_str(" · ");
                    label.push_str(&execution.render_label());
                }
                bounded_render_label(label)
            })
            .collect()
    }

    fn execution_explanation(
        &self,
        observation: &SemanticJudgmentTraceObservation,
    ) -> Option<&SemanticJudgmentExecutionExplanation> {
        self.execution_explanations.iter().find(|explanation| {
            explanation.run_id == observation.observation.correlation.run_id
                && explanation.turn == observation.observation.correlation.turn
                && explanation.round == observation.observation.correlation.round
                && explanation.evaluation_span_id
                    == observation.observation.correlation.evaluation_span_id
                && explanation.stage == observation.observation.fact.stage
        })
    }

    fn execution_coverage_note(&self) -> Option<&'static str> {
        match self.execution_coverage {
            SemanticJudgmentExecutionCoverage::LookupIncomplete => {
                Some("execution record is partial")
            }
            SemanticJudgmentExecutionCoverage::SourceExcluded => {
                Some("execution record is excluded by the selected source")
            }
            SemanticJudgmentExecutionCoverage::NoPool => Some("execution record unavailable"),
            SemanticJudgmentExecutionCoverage::Timeout => {
                Some("loading execution details timed out")
            }
            SemanticJudgmentExecutionCoverage::QueryFailed => {
                Some("execution record failed to load")
            }
            SemanticJudgmentExecutionCoverage::SourceUnavailable => {
                Some("execution identity is unavailable from this source")
            }
            SemanticJudgmentExecutionCoverage::Deferred => {
                Some("model details not loaded at this depth")
            }
            SemanticJudgmentExecutionCoverage::NotObserved
            | SemanticJudgmentExecutionCoverage::LookupComplete => None,
        }
    }

    pub fn render(&self) -> String {
        let source = match self.scope {
            SemanticJudgmentScope::SessionTraceAtRead => "session trace",
            SemanticJudgmentScope::LocalJournalAtRead => "local journal (not server history)",
        };
        let Some(c) = &self.counts else {
            return format!(
                "Request classification · details unavailable from {source} ({:?}); this does not mean no classification ran.",
                self.coverage
            );
        };
        let mut parts = vec![format!("{} decided", c.decisions)];
        if c.abstained > 0 {
            parts.push(format!("{} uncertain", c.abstained));
        }
        if c.conflicting > 0 || c.invalid > 0 {
            parts.push(format!(
                "{} rejected",
                c.conflicting.saturating_add(c.invalid)
            ));
        }
        if c.not_dispatched > 0 {
            parts.push(format!("{} skipped", c.not_dispatched));
        }
        if c.evaluation_unavailable > 0 {
            parts.push(format!("{} unavailable", c.evaluation_unavailable));
        }
        let mut rendered = format!(
            "Request classification · captured {} · {source}",
            parts.join(" · ")
        );
        if self.capture_truncated {
            rendered.push_str(" · capture truncated");
        } else if self.capture_incomplete {
            rendered.push_str(" · bounded capture; missing stages are possible");
        }
        if self.capture_omitted_observations > 0 {
            rendered.push_str(&format!(
                " · {} observation(s) not counted",
                self.capture_omitted_observations
            ));
        }
        if self.omitted_details > 0 {
            rendered.push_str(&format!(
                " · {} detail(s) hidden but included in counts",
                self.omitted_details
            ));
        }
        if self.execution_omitted > 0 {
            rendered.push_str(&format!(
                " · {} execution detail(s) hidden",
                self.execution_omitted
            ));
        }
        if let Some(note) = self.execution_coverage_note() {
            rendered.push_str(" · ");
            rendered.push_str(note);
        }
        let captured = self.captured_result_labels(3, true);
        if !captured.is_empty() {
            rendered.push_str(". Captured result(s): ");
            rendered.push_str(&captured.join("; "));
            let omitted = self.observations.len().saturating_sub(captured.len());
            if omitted > 0 {
                rendered.push_str(&format!("; {omitted} more detail(s) hidden"));
            }
        }
        rendered.push_str(
            ". Classification informs preparation; it does not prove the agent followed it. Model and token usage are reported separately.",
        );
        rendered
    }

    /// Short projection for the default introspect/reflect view. A semantic
    /// result is not a physical attempt and does not prove that the runtime
    /// adopted it, so the compact form keeps that boundary explicit.
    pub fn render_compact(&self) -> String {
        let Some(c) = &self.counts else {
            let source = match self.coverage {
                SemanticJudgmentCoverage::SourceExcluded => "excluded by source policy",
                SemanticJudgmentCoverage::SourceUnavailable => "source unavailable",
                // `NoPool` means the observation storage pool is absent. It
                // says nothing about whether a judgment/provider was
                // available to the run.
                SemanticJudgmentCoverage::NoPool => "observation storage unavailable",
                SemanticJudgmentCoverage::Timeout => "timed out",
                SemanticJudgmentCoverage::QueryFailed => "query failed",
                SemanticJudgmentCoverage::NotObserved => "not observed",
                SemanticJudgmentCoverage::CaptureIncomplete => "capture incomplete",
            };
            return format!(
                "Request classification · {source} in {}; no result is available here",
                match self.scope {
                    SemanticJudgmentScope::SessionTraceAtRead => "session trace",
                    SemanticJudgmentScope::LocalJournalAtRead => "local journal",
                }
            );
        };
        let mut parts = Vec::new();
        let captured = self.captured_result_labels(2, false);
        if !captured.is_empty() {
            parts.push(format!("captured: {}", captured.join("; ")));
        }
        let evaluation_count = self.captured_evaluation_count();
        if evaluation_count > 0 {
            let label = if evaluation_count == 1 {
                "evaluation"
            } else {
                "evaluations"
            };
            parts.push(format!("{evaluation_count} {label}"));
        }
        if captured.is_empty() || self.observations.len() > captured.len() {
            if c.abstained > 0 {
                parts.push(format!("{} uncertain", c.abstained));
            }
            if c.conflicting > 0 {
                parts.push(format!("{} conflicting", c.conflicting));
            }
            if c.invalid > 0 {
                parts.push(format!("{} invalid", c.invalid));
            }
            if c.not_dispatched > 0 {
                parts.push(format!("{} skipped", c.not_dispatched));
            }
            if c.evaluation_unavailable > 0 {
                parts.push(format!("{} unavailable", c.evaluation_unavailable));
            }
        }
        if parts.is_empty() {
            parts.push("no result captured".into());
        }
        let mut line = format!("Request classification · {}", parts.join(" · "));
        // This view contains classification and (when available) physical
        // execution evidence. It intentionally does not contain an
        // application receipt, so state that boundary without implying that
        // a missing record means the classification was ignored.
        if c.evaluated == 0 && c.evaluation_unavailable == 0 && c.not_dispatched > 0 {
            let skipped = if c.not_dispatched == 1 {
                "captured classification was skipped".to_owned()
            } else {
                format!("{} captured classifications were skipped", c.not_dispatched)
            };
            line.push_str(" · ");
            line.push_str(&skipped);
        } else {
            line.push_str(" · whether it guided the run is not recorded");
        }
        if self.capture_truncated {
            line.push_str(" · history truncated");
        } else if self.capture_incomplete {
            line.push_str(" · history may be incomplete");
        }
        if self.capture_omitted_observations > 0 || self.omitted_details > 0 {
            line.push_str(" · more detail omitted");
        }
        if self.execution_omitted > 0 {
            line.push_str(" · more execution detail omitted");
        }
        if let Some(note) = self.execution_coverage_note() {
            line.push_str(" · ");
            line.push_str(note);
        }
        line
    }

    pub fn render_for_depth(&self, depth: astra_core::ObservationDepth) -> String {
        match depth {
            astra_core::ObservationDepth::Hint | astra_core::ObservationDepth::Summary => {
                self.render_compact()
            }
            astra_core::ObservationDepth::Diagnostic | astra_core::ObservationDepth::Forensic => {
                self.render()
            }
        }
    }
}

fn stage_label(stage: astra_turn_types::RequestJudgmentStageV1) -> &'static str {
    match stage {
        astra_turn_types::RequestJudgmentStageV1::Initial => "initial",
        astra_turn_types::RequestJudgmentStageV1::Clarification => "clarification",
    }
}

fn bounded_render_label(label: String) -> String {
    const MAX_BYTES: usize = 320;
    if label.len() <= MAX_BYTES {
        return label;
    }
    let suffix = "...";
    let mut end = MAX_BYTES - suffix.len();
    while !label.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}{}", &label[..end], suffix)
}

impl SemanticJudgmentExecutionExplanation {
    fn render_label(&self) -> String {
        match self.association {
            SemanticJudgmentExecutionAssociation::Matched => {
                let mut label = crate::judgment_presentation::provider_model_label(
                    self.provider.as_deref(),
                    self.model_name.as_deref(),
                )
                .map(|identity| format!("execution via {identity}"))
                .unwrap_or_else(|| "execution matched".to_owned());
                if self.attempts.len() > 1 {
                    label.push_str(&format!(" · {} attempts", self.attempts.len()));
                }
                if self.attempts_truncated {
                    label.push_str(" · attempts incomplete");
                }
                label
            }
            SemanticJudgmentExecutionAssociation::NotCaptured => {
                "execution not captured in this lookup".to_owned()
            }
            SemanticJudgmentExecutionAssociation::LookupIncomplete => {
                "execution evidence incomplete".to_owned()
            }
            SemanticJudgmentExecutionAssociation::Conflicting => {
                "execution evidence conflicts".to_owned()
            }
            SemanticJudgmentExecutionAssociation::Unavailable => {
                "execution association unavailable".to_owned()
            }
        }
    }
}

pub fn semantic_judgment_facet_enabled(facet: astra_core::ObservationFacet) -> bool {
    use astra_core::ObservationFacet;
    matches!(
        facet,
        ObservationFacet::Session
            | ObservationFacet::Overview
            | ObservationFacet::Recent
            | ObservationFacet::Trace
    )
}

fn execution_model_name(resolved: &str, upstream: &str) -> Option<String> {
    [resolved, upstream]
        .into_iter()
        .find(|name| !name.is_empty())
        .map(str::to_owned)
}

/// Reuse a caller's bounded physical-attempt read for detailed semantic
/// explanations. This is a pure projection: it cannot acquire a pool
/// connection or issue a second ledger query.
pub fn apply_semantic_execution_capture(
    view: &mut SemanticJudgmentView,
    capture: &crate::inference_execution::SessionAuxiliaryUsageCapture,
    session_id: &str,
) {
    let has_known_invocation = view.observations.iter().any(|observation| {
        match &observation.observation.correlation.invocation {
            SemanticJudgmentInvocationV1::Known { .. } => true,
            SemanticJudgmentInvocationV1::Unavailable => false,
        }
    });
    if !has_known_invocation {
        view.execution_coverage = SemanticJudgmentExecutionCoverage::NotObserved;
        view.execution_explanations.clear();
        return;
    }
    let mut facts_by_invocation =
        BTreeMap::<String, Vec<&crate::inference_execution::AuxiliaryExecutionAttemptFact>>::new();
    for fact in &capture.execution {
        facts_by_invocation
            .entry(fact.invocation_id.clone())
            .or_default()
            .push(fact);
    }
    view.execution_coverage = if capture.facts.truncated {
        SemanticJudgmentExecutionCoverage::LookupIncomplete
    } else {
        SemanticJudgmentExecutionCoverage::LookupComplete
    };
    view.execution_explanations = view
        .observations
        .iter()
        .filter_map(|observation| {
            let SemanticJudgmentInvocationV1::Known { invocation_id } =
                &observation.observation.correlation.invocation
            else {
                return None;
            };
            let Some(facts) = facts_by_invocation.get(invocation_id) else {
                return unavailable_execution_explanation(
                    observation,
                    if capture.facts.truncated {
                        SemanticJudgmentExecutionAssociation::LookupIncomplete
                    } else {
                        SemanticJudgmentExecutionAssociation::NotCaptured
                    },
                );
            };
            let first = facts.first()?;
            let correlation = &observation.observation.correlation;
            let scope_conflict = facts.iter().any(|fact| {
                fact.invocation_run_id.as_deref() != Some(correlation.run_id.as_str())
                    || fact.invocation_turn != Some(i64::from(correlation.turn))
                    || fact.invocation_round != Some(i64::from(correlation.round))
            });
            let route_provider = first.route_provider.clone();
            let model_name =
                execution_model_name(&first.resolved_model_name, &first.upstream_model_name);
            let attempt_invalid = facts.iter().any(|fact| {
                u32::try_from(fact.attempt_index).is_err()
                    || fact.attempt_id.is_empty()
                    || fact.attempt_provider.is_empty()
                    || fact.attempt_protocol.is_empty()
                    || fact.attempt_status.is_empty()
            });
            let attempts = facts
                .iter()
                .filter_map(|fact| {
                    Some(SemanticJudgmentExecutionAttempt {
                        attempt_id: (!fact.attempt_id.is_empty())
                            .then(|| fact.attempt_id.clone())?,
                        attempt_index: u32::try_from(fact.attempt_index).ok()?,
                        provider: (!fact.attempt_provider.is_empty())
                            .then(|| fact.attempt_provider.clone())?,
                        model_name: execution_model_name(
                            &fact.resolved_model_name,
                            &fact.upstream_model_name,
                        )?,
                        protocol: (!fact.attempt_protocol.is_empty())
                            .then(|| fact.attempt_protocol.clone())?,
                        status: (!fact.attempt_status.is_empty())
                            .then(|| fact.attempt_status.clone())?,
                    })
                })
                .take(MAX_SEMANTIC_JUDGMENT_EXECUTION_ATTEMPTS)
                .collect::<Vec<_>>();
            let route_conflict = facts.iter().any(|fact| {
                fact.attempt_provider != first.route_provider.as_deref().unwrap_or_default()
                    || execution_model_name(&fact.resolved_model_name, &fact.upstream_model_name)
                        != model_name
            });
            let identity_conflict = facts.iter().any(|fact| {
                fact.invocation_session_id.as_deref() != Some(session_id)
                    || fact.route_session_id.as_deref() != Some(session_id)
                    || fact.route_run_id != fact.invocation_run_id
                    || fact.attempt_session_id.as_deref() != Some(session_id)
                    || fact.attempt_run_id != fact.invocation_run_id
            }) || route_provider.as_deref().is_none_or(str::is_empty)
                || model_name.is_none();
            let attempts_truncated = capture.facts.truncated
                || facts.len() > MAX_SEMANTIC_JUDGMENT_EXECUTION_ATTEMPTS
                || attempt_invalid;
            let association =
                if scope_conflict || identity_conflict || route_conflict || attempt_invalid {
                    SemanticJudgmentExecutionAssociation::Conflicting
                } else {
                    SemanticJudgmentExecutionAssociation::Matched
                };
            let attempts = if association == SemanticJudgmentExecutionAssociation::Conflicting {
                // A conflicting row does not prove that its physical details
                // belong to this observation. Do not leak them through the
                // structured projection.
                Vec::new()
            } else {
                attempts
            };
            Some(SemanticJudgmentExecutionExplanation {
                run_id: correlation.run_id.clone(),
                turn: correlation.turn,
                round: correlation.round,
                evaluation_span_id: correlation.evaluation_span_id.clone(),
                stage: observation.observation.fact.stage,
                invocation_id: Some(invocation_id.clone()),
                association,
                provider: (!scope_conflict
                    && !identity_conflict
                    && !route_conflict
                    && !attempt_invalid)
                    .then_some(route_provider.clone())
                    .flatten(),
                model_name: (!scope_conflict
                    && !identity_conflict
                    && !route_conflict
                    && !attempt_invalid)
                    .then_some(model_name.clone())
                    .flatten(),
                attempts,
                attempts_truncated,
            })
        })
        .collect();
}

fn unavailable_execution_explanation(
    observation: &SemanticJudgmentTraceObservation,
    association: SemanticJudgmentExecutionAssociation,
) -> Option<SemanticJudgmentExecutionExplanation> {
    let SemanticJudgmentInvocationV1::Known { invocation_id } =
        &observation.observation.correlation.invocation
    else {
        return None;
    };
    Some(SemanticJudgmentExecutionExplanation {
        run_id: observation.observation.correlation.run_id.clone(),
        turn: observation.observation.correlation.turn,
        round: observation.observation.correlation.round,
        evaluation_span_id: observation
            .observation
            .correlation
            .evaluation_span_id
            .clone(),
        stage: observation.observation.fact.stage,
        invocation_id: Some(invocation_id.clone()),
        association,
        provider: None,
        model_name: None,
        attempts: vec![],
        attempts_truncated: false,
    })
}

pub fn mark_execution_lookup_unavailable(
    view: &mut SemanticJudgmentView,
    coverage: SemanticJudgmentExecutionCoverage,
) {
    view.execution_coverage = coverage;
    view.execution_explanations = view
        .observations
        .iter()
        .filter_map(|observation| {
            unavailable_execution_explanation(
                observation,
                SemanticJudgmentExecutionAssociation::Unavailable,
            )
        })
        .collect();
}

/// One shared optional read policy for all consumers. This loads semantic
/// trace evidence only; callers that already read the auxiliary physical
/// capture may apply that capture through the pure projection above.
pub async fn load_semantic_judgment_view(
    pool: Option<&SharedPool>,
    user_id: &str,
    session_id: &str,
    source: astra_core::SourcePolicy,
    depth: astra_core::ObservationDepth,
) -> SemanticJudgmentView {
    if matches!(
        source,
        astra_core::SourcePolicy::LiveOnly | astra_core::SourcePolicy::LocalOnly
    ) {
        return SemanticJudgmentView::unavailable(SemanticJudgmentCoverage::SourceExcluded);
    }
    let Some(pool) = pool else {
        return SemanticJudgmentView::unavailable(SemanticJudgmentCoverage::NoPool);
    };
    let capture = match tokio::time::timeout(
        std::time::Duration::from_secs(2),
        load_semantic_judgment_observations(pool, user_id, session_id, 128, 128),
    )
    .await
    {
        Ok(Ok(capture)) => capture,
        Ok(Err(_)) => {
            return SemanticJudgmentView::unavailable(SemanticJudgmentCoverage::QueryFailed);
        }
        Err(_) => return SemanticJudgmentView::unavailable(SemanticJudgmentCoverage::Timeout),
    };
    let view = SemanticJudgmentView::from_capture(capture).bounded(depth);
    // Physical execution is deliberately deferred here. The introspect and
    // reflect callers already perform one bounded auxiliary read; they attach
    // its request-local detail without opening a second database path.
    let mut view = view;
    view.defer_execution_lookup();
    view
}

#[cfg(test)]
mod tests {
    use super::*;
    use astra_turn_types::*;
    use serde_json::json;

    fn observation() -> SemanticJudgmentObservationV1 {
        SemanticJudgmentObservationV1 {
            schema_version: 1,
            correlation: SemanticJudgmentCorrelationV1 {
                run_id: "run-1".into(),
                turn: 2,
                round: 3,
                owner_generation: Some(1),
                evaluation_span_id: "evaluation-1".into(),
                invocation: SemanticJudgmentInvocationV1::Unavailable,
            },
            fact: SemanticJudgmentFactV1 {
                stage: RequestJudgmentStageV1::Initial,
                result: RequestJudgmentResultV1::NotDispatched {
                    reason: SemanticJudgmentPreDispatchReasonV1::NoOffering,
                },
            },
        }
    }

    fn known_observation(
        stage: RequestJudgmentStageV1,
        evaluation_span_id: &str,
        invocation_id: &str,
    ) -> SemanticJudgmentTraceObservation {
        let mut observation = observation();
        observation.correlation.evaluation_span_id = evaluation_span_id.into();
        observation.correlation.invocation = SemanticJudgmentInvocationV1::Known {
            invocation_id: invocation_id.into(),
        };
        observation.fact.stage = stage;
        SemanticJudgmentTraceObservation {
            observation_span_id: format!("span-{evaluation_span_id}-{invocation_id}"),
            observation,
        }
    }

    fn semantic_view(observations: Vec<SemanticJudgmentTraceObservation>) -> SemanticJudgmentView {
        SemanticJudgmentView::from_capture(SemanticJudgmentCapture {
            available: true,
            capture_incomplete: true,
            truncated: false,
            candidates_scanned: observations.len(),
            duplicate_observations: 0,
            omitted_observations: 0,
            observations,
            gaps: vec![SemanticJudgmentCaptureGap::TraceMayBeDropped],
        })
    }

    fn execution_fact(
        invocation_id: &str,
        run_id: &str,
        turn: u32,
        round: u32,
        attempt: (&str, u32, &str, &str, &str),
    ) -> crate::inference_execution::AuxiliaryExecutionAttemptFact {
        let (attempt_id, attempt_index, provider, protocol, status) = attempt;
        crate::inference_execution::AuxiliaryExecutionAttemptFact {
            invocation_id: invocation_id.into(),
            invocation_session_id: Some("session-1".into()),
            invocation_run_id: Some(run_id.into()),
            invocation_turn: Some(i64::from(turn)),
            invocation_round: Some(i64::from(round)),
            route_session_id: Some("session-1".into()),
            route_run_id: Some(run_id.into()),
            route_provider: Some("deepseek".into()),
            attempt_id: attempt_id.into(),
            attempt_index: i64::from(attempt_index),
            attempt_session_id: Some("session-1".into()),
            attempt_run_id: Some(run_id.into()),
            attempt_provider: provider.into(),
            attempt_protocol: protocol.into(),
            attempt_status: status.into(),
            resolved_model_name: "deepseek-flash".into(),
            upstream_model_name: "deepseek-chat".into(),
        }
    }

    #[test]
    fn semantic_execution_projection_requires_exact_scope_and_keeps_status_distinct() {
        let initial = known_observation(RequestJudgmentStageV1::Initial, "eval-1", "inv-1");
        let missing = known_observation(RequestJudgmentStageV1::Initial, "eval-2", "inv-missing");
        let conflict = known_observation(RequestJudgmentStageV1::Initial, "eval-3", "inv-2");
        let mut view = semantic_view(vec![initial, missing, conflict]);
        let capture = crate::inference_execution::SessionAuxiliaryUsageCapture {
            facts: ExplainAnalyzeAuxiliaryUsageV1 {
                available: true,
                truncated: false,
                attempts: Vec::new(),
            },
            execution: vec![
                execution_fact(
                    "inv-1",
                    "run-1",
                    2,
                    3,
                    ("attempt-1", 0, "deepseek", "openai_compatible", "succeeded"),
                ),
                // Same opaque invocation id is not accepted when its
                // immutable run scope disagrees with the observation.
                execution_fact(
                    "inv-2",
                    "other-run",
                    2,
                    3,
                    ("attempt-2", 0, "deepseek", "openai_compatible", "succeeded"),
                ),
            ],
        };
        apply_semantic_execution_capture(&mut view, &capture, "session-1");

        assert_eq!(
            view.execution_coverage,
            SemanticJudgmentExecutionCoverage::LookupComplete
        );
        let matched = view
            .execution_explanations
            .iter()
            .find(|explanation| explanation.invocation_id.as_deref() == Some("inv-1"))
            .unwrap();
        assert_eq!(
            matched.association,
            SemanticJudgmentExecutionAssociation::Matched
        );
        assert_eq!(matched.model_name.as_deref(), Some("deepseek-flash"));
        assert_eq!(matched.provider.as_deref(), Some("deepseek"));
        assert_eq!(matched.attempts[0].status, "succeeded");

        let conflicting = view
            .execution_explanations
            .iter()
            .find(|explanation| explanation.invocation_id.as_deref() == Some("inv-2"))
            .unwrap();
        assert_eq!(
            conflicting.association,
            SemanticJudgmentExecutionAssociation::Conflicting
        );
        assert!(conflicting.model_name.is_none());
        assert!(conflicting.attempts.is_empty());
        let serialized = serde_json::to_string(conflicting).unwrap();
        assert!(!serialized.contains("attempt-2"));
        assert!(!serialized.contains("deepseek"));

        let not_captured = view
            .execution_explanations
            .iter()
            .find(|explanation| explanation.invocation_id.as_deref() == Some("inv-missing"))
            .unwrap();
        assert_eq!(
            not_captured.association,
            SemanticJudgmentExecutionAssociation::NotCaptured
        );
        assert!(
            view.render()
                .contains("execution via deepseek · deepseek-flash")
        );
        assert!(
            view.render_compact()
                .contains("via deepseek · deepseek-flash")
        );
        assert!(
            view.captured_result_labels(4, true)
                .join("; ")
                .contains("execution evidence conflicts")
        );
    }

    #[test]
    fn semantic_execution_projection_does_not_claim_complete_attempt_history() {
        let observation = known_observation(RequestJudgmentStageV1::Initial, "eval-1", "inv-1");
        let mut view = semantic_view(vec![observation]);
        let capture = crate::inference_execution::SessionAuxiliaryUsageCapture {
            facts: ExplainAnalyzeAuxiliaryUsageV1 {
                available: true,
                truncated: true,
                attempts: Vec::new(),
            },
            execution: vec![execution_fact(
                "inv-1",
                "run-1",
                2,
                3,
                ("attempt-1", 0, "deepseek", "openai_compatible", "succeeded"),
            )],
        };
        apply_semantic_execution_capture(&mut view, &capture, "session-1");
        assert_eq!(
            view.execution_coverage,
            SemanticJudgmentExecutionCoverage::LookupIncomplete
        );
        assert!(view.execution_explanations[0].attempts_truncated);
        assert!(
            view.render_compact()
                .contains("execution record is partial")
        );
    }

    #[test]
    fn semantic_execution_capture_projects_the_canonical_auxiliary_read() {
        let observation = known_observation(RequestJudgmentStageV1::Initial, "eval-1", "inv-1");
        let mut view = semantic_view(vec![observation]);
        let capture = crate::inference_execution::SessionAuxiliaryUsageCapture {
            facts: ExplainAnalyzeAuxiliaryUsageV1 {
                available: true,
                truncated: false,
                attempts: Vec::new(),
            },
            execution: vec![crate::inference_execution::AuxiliaryExecutionAttemptFact {
                invocation_id: "inv-1".into(),
                invocation_session_id: Some("session-1".into()),
                invocation_run_id: Some("run-1".into()),
                invocation_turn: Some(2),
                invocation_round: Some(3),
                route_session_id: Some("session-1".into()),
                route_run_id: Some("run-1".into()),
                route_provider: Some("deepseek".into()),
                attempt_id: "attempt-1".into(),
                attempt_index: 0,
                attempt_session_id: Some("session-1".into()),
                attempt_run_id: Some("run-1".into()),
                attempt_provider: "deepseek".into(),
                attempt_protocol: "openai_compatible".into(),
                attempt_status: "succeeded".into(),
                resolved_model_name: "deepseek-flash".into(),
                upstream_model_name: "deepseek-chat".into(),
            }],
        };
        apply_semantic_execution_capture(&mut view, &capture, "session-1");

        assert_eq!(
            view.execution_coverage,
            SemanticJudgmentExecutionCoverage::LookupComplete
        );
        let explanation = &view.execution_explanations[0];
        assert_eq!(
            explanation.association,
            SemanticJudgmentExecutionAssociation::Matched
        );
        assert_eq!(explanation.provider.as_deref(), Some("deepseek"));
        assert_eq!(explanation.model_name.as_deref(), Some("deepseek-flash"));
        assert_eq!(explanation.attempts[0].model_name, "deepseek-flash");
    }

    #[test]
    fn semantic_execution_capture_rejects_out_of_range_attempt_index() {
        let observation = known_observation(RequestJudgmentStageV1::Initial, "eval-1", "inv-1");
        let mut fact = execution_fact(
            "inv-1",
            "run-1",
            2,
            3,
            ("attempt-1", 0, "deepseek", "openai_compatible", "succeeded"),
        );
        fact.attempt_index = i64::from(u32::MAX) + 1;
        let mut view = semantic_view(vec![observation]);
        let capture = crate::inference_execution::SessionAuxiliaryUsageCapture {
            facts: ExplainAnalyzeAuxiliaryUsageV1 {
                available: true,
                truncated: false,
                attempts: Vec::new(),
            },
            execution: vec![fact],
        };

        apply_semantic_execution_capture(&mut view, &capture, "session-1");

        let explanation = &view.execution_explanations[0];
        assert_eq!(
            explanation.association,
            SemanticJudgmentExecutionAssociation::Conflicting
        );
        assert!(explanation.attempts.is_empty());
        assert!(explanation.attempts_truncated);
    }

    #[test]
    fn semantic_execution_projection_keeps_full_observation_identity() {
        let first = known_observation(RequestJudgmentStageV1::Initial, "shared-eval", "inv-1");
        let mut second = known_observation(RequestJudgmentStageV1::Initial, "shared-eval", "inv-2");
        second.observation.correlation.run_id = "run-2".into();
        second.observation.correlation.turn = 7;
        second.observation.correlation.round = 8;
        let mut view = semantic_view(vec![first.clone(), second.clone()]);
        let capture = crate::inference_execution::SessionAuxiliaryUsageCapture {
            facts: ExplainAnalyzeAuxiliaryUsageV1 {
                available: true,
                truncated: false,
                attempts: Vec::new(),
            },
            execution: vec![
                execution_fact(
                    "inv-1",
                    "run-1",
                    2,
                    3,
                    ("attempt-1", 0, "deepseek", "openai_compatible", "succeeded"),
                ),
                execution_fact(
                    "inv-2",
                    "run-2",
                    7,
                    8,
                    ("attempt-2", 0, "deepseek", "openai_compatible", "succeeded"),
                ),
            ],
        };
        apply_semantic_execution_capture(&mut view, &capture, "session-1");
        assert_eq!(view.execution_explanations.len(), 2);
        assert_eq!(
            view.execution_explanation(&first)
                .and_then(|explanation| explanation.invocation_id.as_deref()),
            Some("inv-1")
        );
        assert_eq!(
            view.execution_explanation(&second)
                .and_then(|explanation| explanation.invocation_id.as_deref()),
            Some("inv-2")
        );
    }

    #[test]
    fn semantic_execution_lookup_legacy_view_defaults_to_unobserved() {
        let legacy = json!({
            "scope": "session_trace_at_read",
            "coverage": "capture_incomplete",
            "capture_incomplete": true,
            "capture_truncated": false,
            "counts": null,
            "capture_gaps": [],
            "capture_omitted_observations": 0,
            "omitted_details": 0,
            "observations": []
        });
        let view: SemanticJudgmentView = serde_json::from_value(legacy).unwrap();
        assert_eq!(
            view.execution_coverage,
            SemanticJudgmentExecutionCoverage::NotObserved
        );
        assert!(view.execution_explanations.is_empty());
        assert_eq!(view.execution_omitted, 0);
    }

    #[test]
    fn semantic_execution_lookup_can_be_deferred_without_claiming_zero_calls() {
        let mut view = semantic_view(vec![known_observation(
            RequestJudgmentStageV1::Initial,
            "eval-1",
            "inv-1",
        )]);
        view.defer_execution_lookup();
        assert_eq!(
            view.execution_coverage,
            SemanticJudgmentExecutionCoverage::Deferred
        );
        assert!(view.execution_explanations.is_empty());
        assert!(
            view.render_compact()
                .contains("model details not loaded at this depth")
        );
    }

    fn decided() -> RequestJudgmentResultV1 {
        let request =
            crate::work_admission_classification_request(&crate::TurnIntentJudgeContext::default());
        request_judgment_result(&crate::parse_work_admission_classification(
            &request,
            r#"{"true":["mutation.read_only"],"uncertain":[]}"#,
        ))
    }

    fn abstained() -> RequestJudgmentResultV1 {
        let request =
            crate::work_admission_classification_request(&crate::TurnIntentJudgeContext::default());
        request_judgment_result(&crate::parse_work_admission_classification(
            &request,
            r#"{"true":["mutation.read_only"],"uncertain":["required"]}"#,
        ))
    }

    #[test]
    fn request_judgment_adapter_preserves_canonical_decision_and_uncertainty() {
        let RequestJudgmentResultV1::Decided { classification } = decided() else {
            panic!("decision");
        };
        assert!(!classification.work_required);
        assert_eq!(classification.mutation, RequestJudgmentMutationV1::ReadOnly);
        assert_eq!(classification.scope, RequestJudgmentScopeV1::Unknown);
        assert!(classification.domain.is_none());
        let RequestJudgmentResultV1::Abstained {
            uncertain_fields,
            assessment,
        } = abstained()
        else {
            panic!("abstention");
        };
        assert_eq!(uncertain_fields, vec![RequestJudgmentFieldV1::Required]);
        assert_eq!(
            assessment.provenance,
            JudgmentResponseProvenance::DiscreteDecision
        );
        assert_eq!(assessment.fields.len(), REQUEST_JUDGMENT_MAX_FIELDS);
        assert_eq!(
            assessment
                .fields
                .iter()
                .find(|f| f.field == RequestJudgmentFieldV1::Required)
                .unwrap()
                .score
                .as_number()
                .as_f64(),
            Some(0.5)
        );
    }

    #[test]
    fn request_judgment_adapter_does_not_expose_error_text_or_invent_codec_cause() {
        use crate::TurnIntentJudgeError as Error;
        for error in [
            Error::Malformed {
                raw: "PRIVATE_PAYLOAD".into(),
                detail: "PRIVATE_PAYLOAD".into(),
            },
            Error::Conflicting {
                fields: vec!["PRIVATE_PAYLOAD".into()],
                detail: "PRIVATE_PAYLOAD".into(),
            },
        ] {
            let projected = request_judgment_result(&Err(error));
            assert_eq!(
                projected,
                RequestJudgmentResultV1::Invalid {
                    reason: SemanticJudgmentInvalidV1::InvalidContract
                }
            );
            assert!(!format!("{projected:?}").contains("PRIVATE_PAYLOAD"));
            assert!(
                !serde_json::to_string(&projected)
                    .unwrap()
                    .contains("PRIVATE_PAYLOAD")
            );
        }
        let conflict = request_judgment_result(&Err(Error::Conflicting {
            fields: vec!["scope.workspace".into(), "scope.external".into()],
            detail: "PRIVATE_PAYLOAD".into(),
        }));
        assert_eq!(
            conflict,
            RequestJudgmentResultV1::Conflicting {
                fields: vec![
                    RequestJudgmentFieldV1::ScopeWorkspace,
                    RequestJudgmentFieldV1::ScopeExternal
                ],
            }
        );
        assert_eq!(
            request_judgment_result(&Err(Error::UnsupportedCombination(
                "PRIVATE_PAYLOAD".into()
            ))),
            RequestJudgmentResultV1::Invalid {
                reason: SemanticJudgmentInvalidV1::UnsupportedCombination
            }
        );
    }

    #[test]
    fn request_judgment_adapter_preserves_native_score_without_rounding() {
        let request =
            crate::work_admission_classification_request(&crate::TurnIntentJudgeContext::default());
        let score = 0.512_345_678_901_234_5;
        let response = JudgmentResponse {
            schema_version: 1,
            model: "offline".into(),
            answers: request
                .questions
                .keys()
                .map(|id| {
                    (
                        id.clone(),
                        JudgmentAnswer::Noul {
                            noul: if id == "required" {
                                score
                            } else if id == "mutation.read_only" {
                                1.0
                            } else {
                                0.0
                            },
                        },
                    )
                })
                .collect(),
        };
        let result = crate::parse_work_admission_classification(
            &request,
            &serde_json::to_string(&response).unwrap(),
        );
        let RequestJudgmentResultV1::Abstained { assessment, .. } =
            request_judgment_result(&result)
        else {
            panic!("uncertain");
        };
        assert_eq!(
            assessment.provenance,
            JudgmentResponseProvenance::ProviderProbability
        );
        assert_eq!(
            assessment
                .fields
                .iter()
                .find(|f| f.field == RequestJudgmentFieldV1::Required)
                .unwrap()
                .score
                .as_number()
                .as_f64(),
            Some(score)
        );
    }

    #[test]
    fn request_judgment_adapter_keeps_delivery_unknown_and_does_not_map_errors_to_abstention() {
        for (kind, reason) in [
            (
                astra_core::ErrorKind::Cancelled,
                SemanticJudgmentUnavailableReasonV1::Cancelled,
            ),
            (
                astra_core::ErrorKind::ProviderDeadline,
                SemanticJudgmentUnavailableReasonV1::Deadline,
            ),
            (
                astra_core::ErrorKind::DatabaseError,
                SemanticJudgmentUnavailableReasonV1::ExecutionError,
            ),
        ] {
            assert_eq!(
                request_judgment_result(&Err(crate::TurnIntentJudgeError::Inference(
                    astra_core::ClassifiedError::new(kind, "PRIVATE_PAYLOAD"),
                ))),
                RequestJudgmentResultV1::Unavailable {
                    reason,
                    delivery: SemanticJudgmentDeliveryV1::Unresolved
                }
            );
        }
    }

    #[test]
    fn request_judgment_adapter_rejects_unprojectable_diagnostics_without_affecting_control_result()
    {
        let request =
            crate::work_admission_classification_request(&crate::TurnIntentJudgeContext::default());
        let mut result = crate::parse_work_admission_classification(
            &request,
            r#"{"true":["mutation.read_only"],"uncertain":["required"]}"#,
        );
        let Err(crate::TurnIntentJudgeError::Uncertain { diagnostics }) = &mut result else {
            panic!("uncertain");
        };
        diagnostics.evidence.get_mut("required").unwrap().value = f64::NAN;
        assert_eq!(
            request_judgment_result(&result),
            RequestJudgmentResultV1::Invalid {
                reason: SemanticJudgmentInvalidV1::InvalidContract
            }
        );
        assert!(matches!(
            result,
            Err(crate::TurnIntentJudgeError::Uncertain { .. })
        ));
    }

    fn metadata(id: &str, observation: &SemanticJudgmentObservationV1) -> Value {
        semantic_judgment_trace(id, observation, 100)
            .unwrap()
            .session_id(Some("session-1"))
            .build()
            .metadata
            .unwrap()
    }

    fn row(metadata: Value) -> SemanticJudgmentTraceRow {
        SemanticJudgmentTraceRow {
            user_id: "owner".into(),
            session_id: "session-1".into(),
            metadata_json: Some(metadata.to_string()),
            metadata_oversized: false,
        }
    }

    #[test]
    fn local_journal_uses_canonical_projection_and_exposes_window_loss() {
        let fact = observation();
        let event = semantic_judgment_trace("local", &fact, 100)
            .unwrap()
            .session_id(Some("session-1"))
            .build();
        let expected = project(vec![row(event.metadata.clone().unwrap())], 512, 32);
        let owner = crate::OwnerScope::user("owner").unwrap();
        let mut window = crate::session_journal::JournalObservationWindow {
            events: vec![event.clone(), event],
            available: true,
            truncated: true,
            malformed_records: 1,
        };
        let view = project_local_semantic_judgments(
            &window,
            &owner,
            "session-1",
            astra_core::ObservationDepth::Diagnostic,
        );
        assert_eq!(view.observations, expected.observations);
        assert_eq!(view.scope, SemanticJudgmentScope::LocalJournalAtRead);
        assert!(view.capture_truncated && view.capture_incomplete);
        assert!(
            view.capture_gaps
                .contains(&SemanticJudgmentCaptureGap::InvalidObservation)
        );
        assert!(view.render().contains("not server history"));
        let other = project_local_semantic_judgments(
            &window,
            &owner,
            "another-session",
            astra_core::ObservationDepth::Diagnostic,
        );
        assert!(other.observations.is_empty());
        assert!(
            other
                .capture_gaps
                .contains(&SemanticJudgmentCaptureGap::ScopeMismatch)
        );
        for turn in [None, Some(fact.correlation.turn + 1)] {
            for event in &mut window.events {
                event.turn = turn;
            }
            let inconsistent = project_local_semantic_judgments(
                &window,
                &owner,
                "session-1",
                astra_core::ObservationDepth::Diagnostic,
            );
            assert!(inconsistent.observations.is_empty());
            assert!(
                inconsistent
                    .capture_gaps
                    .contains(&SemanticJudgmentCaptureGap::ScopeMismatch)
            );
        }
        window.available = false;
        let missing = project_local_semantic_judgments(
            &window,
            &owner,
            "session-1",
            astra_core::ObservationDepth::Diagnostic,
        );
        assert!(missing.counts.is_none());
        assert_eq!(
            missing.coverage,
            SemanticJudgmentCoverage::SourceUnavailable
        );
    }

    fn project(
        rows: Vec<SemanticJudgmentTraceRow>,
        candidates: usize,
        observations: usize,
    ) -> SemanticJudgmentCapture {
        project_semantic_judgment_observations(rows, "owner", "session-1", candidates, observations)
    }

    #[test]
    fn semantic_judgment_not_dispatched_conflicts_with_decision_in_both_orders() {
        let not_dispatched = observation();
        let mut decision = not_dispatched.clone();
        decision.fact.result = decided();
        for reverse in [false, true] {
            let mut rows = vec![
                row(metadata("a", &not_dispatched)),
                row(metadata("b", &decision)),
            ];
            if reverse {
                rows.reverse();
            }
            let capture = project(rows, 32, 32);
            assert!(capture.observations.is_empty());
            assert!(
                capture
                    .gaps
                    .contains(&SemanticJudgmentCaptureGap::ConflictingEvaluation)
            );
        }
        let copies = project(
            vec![
                row(metadata("a", &not_dispatched)),
                row(metadata("b", &not_dispatched)),
            ],
            32,
            32,
        );
        assert_eq!(copies.duplicate_observations, 1);
        let view = SemanticJudgmentView::from_capture(copies);
        let counts = view.counts.as_ref().unwrap();
        assert_eq!(counts.not_dispatched, 1);
        assert_eq!(counts.evaluated, 0);
        let compact = view.render_compact();
        assert!(
            compact.contains("captured classification was skipped"),
            "{compact}"
        );
        assert!(
            !compact.contains("whether it guided the run is not recorded"),
            "{compact}"
        );
    }

    #[test]
    fn semantic_judgment_terminal_correlation_deduplicates_and_rejects_conflicts_in_both_orders() {
        let mut evaluated = observation();
        evaluated.fact.result = abstained();
        let mut conflict = evaluated.clone();
        conflict.fact.result = decided();
        let mut clarified = conflict.clone();
        clarified.fact.stage = RequestJudgmentStageV1::Clarification;
        for reverse in [false, true] {
            let mut rows = vec![
                row(metadata("eval-a", &evaluated)),
                row(metadata("eval-b", &conflict)),
                row(metadata("eval-c", &evaluated)),
                row(metadata("clarify-a", &clarified)),
                row(metadata("clarify-b", &clarified)),
            ];
            if reverse {
                rows.reverse();
            }
            let capture = project(rows, 32, 32);
            assert!(
                capture
                    .gaps
                    .contains(&SemanticJudgmentCaptureGap::ConflictingEvaluation)
            );
            let counts = SemanticJudgmentView::from_capture(capture).counts.unwrap();
            assert_eq!(counts.evaluated, 1);
            assert_eq!(counts.initial, 0);
            assert_eq!(counts.clarification, 1);
            let mut collided = vec![
                row(metadata("same", &evaluated)),
                row(metadata("same", &conflict)),
                row(metadata("other", &evaluated)),
            ];
            if reverse {
                collided.reverse();
            }
            assert!(project(collided, 32, 32).observations.is_empty());
        }
    }

    fn live_settings() -> astra_core::MatrixOneSettings {
        let _ = dotenvy::dotenv();
        assert_eq!(
            std::env::var("ASTRA_TEST_DB_IT").as_deref(),
            Ok("1"),
            "set ASTRA_TEST_DB_IT=1"
        );
        let mut settings = astra_core::MatrixOneSettings::from_env();
        settings.db_pool_max_connections = 1;
        settings.db_pool_min_connections = 0;
        settings
    }

    #[tokio::test]
    #[ignore = "requires live MatrixOne: ASTRA_TEST_DB_IT=1; existing schema"]
    async fn semantic_judgment_live_public_loader_synthetic_sessions_are_owner_scoped() {
        let pool = SharedPool::new(&live_settings())
            .await
            .expect("test database connection");
        let user = format!("semantic-test-{}", uuid::Uuid::new_v4());
        let other_user = format!("semantic-other-{}", uuid::Uuid::new_v4());
        // Same-owner/different-session and different-owner decoys must not leak
        // into the requested capture. All identities are unique to this test.
        let fixtures = [&user, &user, &other_user].map(|owner| {
            let session = uuid::Uuid::new_v4().to_string();
            let event = uuid::Uuid::new_v4().to_string();
            let mut fact = observation();
            fact.correlation.run_id = format!("run-{session}");
            let trace = semantic_judgment_trace(&event, &fact, 100)
                .unwrap()
                .session_id(Some(&session))
                .build();
            (owner, session, event, fact, trace.metadata.unwrap())
        });
        // Public loaders acquire their own connection. Commit the complete
        // fixture first; an uncommitted transaction cannot exercise that path.
        let mut tx = pool.get().begin().await.unwrap();
        for (owner, session, event, _, metadata) in &fixtures {
            sqlx::query("INSERT INTO agent_sessions (session_id,user_id,status,event_count,project_retention_policy,created_at,updated_at,last_active_at) VALUES (?,?,'active',1,'session',NOW(6),NOW(6),NOW(6))")
                .bind(session).bind(*owner).execute(&mut *tx).await.unwrap();
            sqlx::query("INSERT INTO agent_events (event_id,user_id,session_id,event_type,metadata,payload_hash,ingestion_write_id,created_at) VALUES (?,?,?,'trace_span',?,?,?,NOW(6))")
                .bind(event).bind(*owner).bind(session).bind(metadata.to_string())
                .bind(crate::observation_capture::canonical_observation_payload_hash(
                    crate::observation_capture::ObservationPayloadDomain::AgentEvent,
                    &json!({"event_id": event, "user_id": owner, "session_id": session, "event_type": "trace_span", "metadata": metadata}),
                ))
                .bind(uuid::Uuid::new_v4().to_string())
                .execute(&mut *tx).await.unwrap();
        }
        tx.commit().await.unwrap();

        // Retain Results until after cleanup, including unexpected loader
        // errors: a failed assertion must not leave committed fixtures behind.
        let mut captures = Vec::new();
        for (owner, session, _, _, _) in &fixtures {
            captures.push(load_semantic_judgment_observations(&pool, owner, session, 32, 8).await);
        }
        let wrong_owner =
            load_semantic_judgment_observations(&pool, &other_user, &fixtures[0].1, 32, 8).await;

        let mut cleanup = pool.get().begin().await.unwrap();
        for (owner, session, event, _, _) in &fixtures {
            sqlx::query(
                "DELETE FROM agent_events WHERE user_id = ? AND session_id = ? AND event_id = ?",
            )
            .bind(*owner)
            .bind(session)
            .bind(event)
            .execute(&mut *cleanup)
            .await
            .unwrap();
            sqlx::query("DELETE FROM agent_sessions WHERE user_id = ? AND session_id = ?")
                .bind(*owner)
                .bind(session)
                .execute(&mut *cleanup)
                .await
                .unwrap();
        }
        cleanup.commit().await.unwrap();

        for (result, (_, _, event, fact, _)) in captures.into_iter().zip(&fixtures) {
            let capture = result.expect("owner-scoped public loader must succeed");
            assert!(capture.available && capture.capture_incomplete);
            assert!(!capture.truncated);
            assert_eq!(capture.candidates_scanned, 1);
            assert_eq!(capture.observations.len(), 1);
            assert_eq!(capture.observations[0].observation_span_id, *event);
            assert_eq!(capture.observations[0].observation, *fact);
            assert!(
                capture
                    .gaps
                    .contains(&SemanticJudgmentCaptureGap::TraceMayBeDropped)
            );
        }
        match wrong_owner {
            Err(error) => assert_eq!(error.kind, crate::ServiceErrorKind::NotFound),
            Ok(_) => panic!("wrong owner must receive not found, never observations"),
        }
    }

    #[tokio::test]
    #[ignore = "requires live MatrixOne: ASTRA_TEST_DB_IT=1; existing schema"]
    async fn semantic_judgment_live_bounded_sql_null_oversize_and_equal_timestamp() {
        let pool = SharedPool::new(&live_settings())
            .await
            .expect("test database connection");
        let user = format!("semantic-test-{}", uuid::Uuid::new_v4());
        let session = uuid::Uuid::new_v4().to_string();
        // Transaction rollback isolates all synthetic fixtures, including panic.
        let mut tx = pool.get().begin().await.unwrap();
        sqlx::query("INSERT INTO agent_sessions (session_id,user_id,status,event_count,project_retention_policy,created_at,updated_at,last_active_at) VALUES (?,?,'active',0,'session',NOW(6),NOW(6),NOW(6))")
            .bind(&session).bind(&user).execute(&mut *tx).await.unwrap();
        assert!(
            crate::storage::agent_session_exists_for_user(&mut *tx, &session, &user)
                .await
                .unwrap()
        );
        assert!(
            !crate::storage::agent_session_exists_for_user(&mut *tx, &session, "other-owner")
                .await
                .unwrap()
        );
        let normal = metadata("normal", &observation()).to_string();
        let oversized =
            json!({"padding":"x".repeat(MAX_SEMANTIC_JUDGMENT_TRACE_BYTES)}).to_string();
        for (suffix, data) in [("a", None), ("b", Some(normal)), ("c", Some(oversized))] {
            let hash = crate::observation_capture::canonical_observation_payload_hash(
                crate::observation_capture::ObservationPayloadDomain::AgentEvent,
                &json!({"event_id": format!("{session}-{suffix}"), "user_id": user, "session_id": session, "event_type": "trace_span", "metadata": data.as_deref().map(|value| serde_json::from_str::<serde_json::Value>(value).unwrap()), "created_at": "2026-01-01 00:00:00.000000"}),
            );
            sqlx::query("INSERT INTO agent_events (event_id,user_id,session_id,event_type,metadata,payload_hash,ingestion_write_id,created_at) VALUES (?,?,?,'trace_span',?,?,?,'2026-01-01 00:00:00.000000')")
                .bind(format!("{session}-{suffix}")).bind(&user).bind(&session).bind(data)
                .bind(hash).bind(uuid::Uuid::new_v4().to_string())
                .execute(&mut *tx).await.unwrap();
        }
        let rows = sqlx::query(LOAD_SQL)
            .bind(MAX_SEMANTIC_JUDGMENT_TRACE_BYTES as i64)
            .bind(MAX_SEMANTIC_JUDGMENT_TRACE_BYTES as i64)
            .bind(&user)
            .bind(&session)
            .bind(3_i64)
            .fetch_all(&mut *tx)
            .await
            .unwrap();
        assert_eq!(rows.len(), 3);
        assert!(rows[0].get::<Option<String>, _>("metadata_json").is_none());
        assert_eq!(rows[0].get::<i64, _>("metadata_oversized"), 1);
        assert!(
            decode_semantic_judgment_trace(&rows[1].get::<String, _>("metadata_json"))
                .unwrap()
                .is_some()
        );
        assert_eq!(rows[1].get::<i64, _>("metadata_oversized"), 0);
        assert!(rows[2].get::<Option<String>, _>("metadata_json").is_none());
        assert_eq!(rows[2].get::<i64, _>("metadata_oversized"), 0);
        tx.rollback().await.unwrap();
    }

    #[tokio::test]
    #[ignore = "requires live MatrixOne: ASTRA_TEST_DB_IT=1"]
    async fn semantic_judgment_live_cancelled_exchange_closes_connection() {
        let pool = SharedPool::new(&live_settings())
            .await
            .expect("test database connection");
        let mut connection = CancellationSafePoolConnection::acquire(pool.get())
            .await
            .unwrap();
        let before: u64 = sqlx::query_scalar("SELECT CONNECTION_ID()")
            .fetch_one(connection.connection_mut())
            .await
            .unwrap();
        let timed_out = tokio::time::timeout(std::time::Duration::from_millis(100), async move {
            let result = sqlx::query("SELECT SLEEP(2)")
                .execute(connection.connection_mut())
                .await;
            connection.release();
            result
        })
        .await;
        assert!(
            timed_out.is_err(),
            "must cancel an in-flight database exchange"
        );
        let after: u64 = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            sqlx::query_scalar("SELECT CONNECTION_ID()").fetch_one(pool.get()),
        )
        .await
        .unwrap()
        .unwrap();
        assert_ne!(before, after, "cancelled connection must not be reused");
        let value: i64 = sqlx::query_scalar("SELECT 17")
            .fetch_one(pool.get())
            .await
            .unwrap();
        assert_eq!(value, 17);
    }

    #[test]
    fn semantic_judgment_trace_roundtrips_through_canonical_ingestion() {
        let fact = observation();
        let event = semantic_judgment_trace("observation-1", &fact, 100)
            .unwrap()
            .session_id(Some("session-1"))
            .build();
        let ingested =
            crate::event_ingestion::IngestionEvent::from_journal_event(&event, "owner").unwrap();
        assert_eq!(ingested.event_type, "trace_span");
        assert!(ingested.content.is_none());
        assert!(ingested.token_usage.is_none());
        let meta = ingested.metadata.unwrap();
        assert_eq!(meta["duration_us"], 0);
        let decoded = decode_semantic_judgment_trace(&meta.to_string())
            .unwrap()
            .unwrap();
        assert_eq!(decoded.observation_span_id, "observation-1");
        assert_eq!(decoded.observation, fact);
        assert!(semantic_judgment_trace("bad id", &fact, 0).is_err());
    }

    #[test]
    fn semantic_judgment_capture_never_claims_complete_history() {
        let empty = project(vec![], 10, 10);
        assert!(empty.available && empty.capture_incomplete && !empty.truncated);
        assert_eq!(empty.gaps, [SemanticJudgmentCaptureGap::TraceMayBeDropped]);
        let unavailable = SemanticJudgmentCapture::unavailable();
        assert!(!unavailable.available && unavailable.capture_incomplete && !unavailable.truncated);
    }

    #[test]
    fn semantic_judgment_view_counts_stages_before_bounding_not_physical_calls() {
        let mut initial = observation();
        initial.fact.result = abstained();
        let mut clarified = initial.clone();
        clarified.fact.stage = RequestJudgmentStageV1::Clarification;
        clarified.fact.result = decided();
        let mut another = observation();
        another.correlation.evaluation_span_id = "another-preflight".into();
        let view = SemanticJudgmentView::from_capture(project(
            vec![
                row(metadata("initial", &initial)),
                row(metadata("clarified", &clarified)),
                row(metadata("another", &another)),
            ],
            3,
            3,
        ))
        .bounded(astra_core::ObservationDepth::Hint);
        let counts = view.counts.as_ref().unwrap();
        assert_eq!(counts.evaluated, 2);
        assert_eq!(counts.abstained, 1);
        assert_eq!(counts.decisions, 1);
        assert_eq!(counts.not_dispatched, 1);
        assert_eq!(counts.initial, 2);
        assert_eq!(counts.clarification, 1);
        assert_eq!(view.omitted_details, 1);
        assert_eq!(view.observations.len(), 2);
        assert_eq!(view.decision_summaries().len(), 1);
        let compact = view.render_compact();
        assert!(
            compact.contains(
                "captured: initial: uncertain fields=required; clarification: Work not required · read-only · scope=unknown"
            ),
            "{compact}"
        );
        assert!(compact.contains("scope=unknown"), "{compact}");
        assert!(compact.contains("1 evaluation"), "{compact}");
        assert!(
            !compact.contains("other captured result(s) hidden"),
            "{compact}"
        );
        assert!(
            compact.contains("whether it guided the run is not recorded"),
            "{compact}"
        );
        assert!(view.capture_incomplete);
        assert!(
            view.render()
                .contains("Model and token usage are reported separately")
        );
        assert!(
            view.render()
                .contains("1 detail(s) hidden but included in counts")
        );
        let detailed = view.render();
        assert!(detailed.contains("work=not_required"), "{detailed}");
        assert!(detailed.contains("activation=not_deferred"), "{detailed}");
        assert!(!view.render().contains("observation(s) not counted"));
        assert!(!view.render().contains("0.5"));
        assert!(serde_json::to_string(&view).unwrap().contains("0.5"));
        assert_eq!(
            view.clone().bounded(astra_core::ObservationDepth::Hint),
            view
        );
        assert!(!serde_json::to_string(&view).unwrap().contains("tokens"));
    }

    #[test]
    fn semantic_judgment_compact_preserves_captured_result_and_evaluation_boundaries() {
        let mut older = observation();
        older.fact.result = decided();
        older.correlation.evaluation_span_id = "evaluation-old".into();

        let mut newer = observation();
        newer.correlation.run_id = "run-new".into();
        newer.correlation.turn = 4;
        newer.correlation.evaluation_span_id = "evaluation-new".into();
        newer.fact.stage = RequestJudgmentStageV1::Clarification;
        newer.fact.result = RequestJudgmentResultV1::Unavailable {
            reason: SemanticJudgmentUnavailableReasonV1::Deadline,
            delivery: SemanticJudgmentDeliveryV1::Unresolved,
        };

        let view = SemanticJudgmentView::from_capture(project(
            vec![
                row(metadata("newer", &newer)),
                row(metadata("older", &older)),
            ],
            4,
            4,
        ));
        let compact = view.render_compact();
        assert!(
            compact
                .contains("captured: clarification: unavailable (deadline, delivery unresolved)"),
            "{compact}"
        );
        assert!(compact.contains("2 evaluations"), "{compact}");
        assert!(compact.contains("Work not required"), "{compact}");
        assert!(
            !compact.contains("other captured result(s) hidden"),
            "{compact}"
        );
        assert_eq!(view.decision_summaries().len(), 1);
        assert_eq!(
            view.decision_summaries()[0].evaluation_span_id,
            "evaluation-old"
        );
        assert_eq!(view.decision_summaries()[0].run_id, "run-1");
    }

    #[test]
    fn semantic_judgment_view_distinguishes_capture_loss_from_hidden_details() {
        let mut view = SemanticJudgmentView::from_capture(project(
            vec![row(metadata("one", &observation()))],
            1,
            1,
        ));
        view.capture_omitted_observations = 3;
        view.capture_truncated = true;
        let rendered = view.render();
        assert!(rendered.contains("capture truncated"));
        assert!(rendered.contains("3 observation(s) not counted"));
        assert!(!rendered.contains("detail(s) hidden"));

        view.omitted_details = 1;
        let rendered = view.render();
        assert!(rendered.contains("3 observation(s) not counted"));
        assert!(rendered.contains("1 detail(s) hidden but included in counts"));
    }

    #[tokio::test]
    async fn semantic_judgment_view_distinguishes_excluded_missing_failed_and_empty() {
        use astra_core::{ObservationDepth as Depth, SourcePolicy};
        for source in [SourcePolicy::LiveOnly, SourcePolicy::LocalOnly] {
            let view =
                load_semantic_judgment_view(None, "owner", "session-1", source, Depth::Summary)
                    .await;
            assert_eq!(view.coverage, SemanticJudgmentCoverage::SourceExcluded);
            assert!(view.counts.is_none());
        }
        let absent = load_semantic_judgment_view(
            None,
            "owner",
            "session-1",
            SourcePolicy::Auto,
            Depth::Summary,
        )
        .await;
        assert_eq!(absent.coverage, SemanticJudgmentCoverage::NoPool);
        assert!(
            absent
                .render_compact()
                .contains("observation storage unavailable")
        );
        assert!(!absent.render_compact().contains("no provider pool"));
        let failed = SemanticJudgmentView::unavailable(SemanticJudgmentCoverage::QueryFailed);
        assert_eq!(failed.coverage, SemanticJudgmentCoverage::QueryFailed);
        assert!(!failed.render().contains("PRIVATE"));
        let empty =
            SemanticJudgmentView::from_capture(project(vec![], 128, 128)).bounded(Depth::Summary);
        assert_eq!(empty.coverage, SemanticJudgmentCoverage::CaptureIncomplete);
        assert!(empty.counts.is_some() && empty.capture_incomplete);
        assert_ne!(empty, absent);
    }

    #[test]
    fn semantic_execution_lookup_failure_preserves_captured_classification() {
        let mut view = semantic_view(vec![known_observation(
            RequestJudgmentStageV1::Initial,
            "eval-1",
            "inv-1",
        )]);
        let counts = view.counts.clone();
        mark_execution_lookup_unavailable(&mut view, SemanticJudgmentExecutionCoverage::Timeout);
        assert_eq!(view.counts, counts);
        assert_eq!(view.observations.len(), 1);
        assert_eq!(
            view.execution_coverage,
            SemanticJudgmentExecutionCoverage::Timeout
        );
        assert!(
            view.render()
                .contains("loading execution details timed out")
        );
    }

    #[test]
    fn semantic_judgment_stages_share_evaluation_but_not_observation_identity() {
        let mut initial = observation();
        initial.fact.result = abstained();
        let mut clarified = initial.clone();
        clarified.fact.stage = RequestJudgmentStageV1::Clarification;
        clarified.fact.result = RequestJudgmentResultV1::Unavailable {
            reason: SemanticJudgmentUnavailableReasonV1::Deadline,
            delivery: SemanticJudgmentDeliveryV1::Unresolved,
        };
        let capture = project(
            vec![
                row(metadata("clarification", &clarified)),
                row(metadata("initial", &initial)),
            ],
            3,
            3,
        );
        assert_eq!(capture.observations.len(), 2);
        assert_eq!(capture.duplicate_observations, 0);
        assert!(
            capture
                .observations
                .iter()
                .all(|fact| fact.observation.correlation.evaluation_span_id == "evaluation-1")
        );
        let counts = SemanticJudgmentView::from_capture(capture).counts.unwrap();
        assert_eq!(counts.abstained, 1);
        assert_eq!(counts.evaluation_unavailable, 1);
    }

    #[test]
    fn semantic_judgment_candidate_cap_does_not_walk_unbounded_input() {
        let rows = (0..).map(|index| {
            assert!(index <= MAX_SEMANTIC_JUDGMENT_CANDIDATES);
            row(json!({"name": "other"}))
        });
        let capture = project_semantic_judgment_observations(
            rows,
            "owner",
            "session-1",
            usize::MAX,
            usize::MAX,
        );
        assert_eq!(capture.candidates_scanned, MAX_SEMANTIC_JUDGMENT_CANDIDATES);
        assert!(capture.truncated);
    }

    #[test]
    fn semantic_judgment_capture_bounds_candidates_output_and_zero_limits() {
        let rows = || {
            (0..4)
                .map(|n| {
                    let mut independent = observation();
                    independent.correlation.evaluation_span_id = format!("evaluation-{n}");
                    row(metadata(&format!("span-{n}"), &independent))
                })
                .collect()
        };
        let capped = project(rows(), 3, 1);
        assert_eq!(capped.candidates_scanned, 3);
        assert_eq!(capped.observations.len(), 1);
        assert_eq!(capped.omitted_observations, 2);
        assert!(
            capped
                .gaps
                .contains(&SemanticJudgmentCaptureGap::CandidateLimit)
        );
        assert!(
            capped
                .gaps
                .contains(&SemanticJudgmentCaptureGap::ObservationLimit)
        );
        let zero = project(rows(), 0, 0);
        assert_eq!(zero.candidates_scanned, 0);
        assert!(zero.truncated && zero.observations.is_empty());
        assert!(!project(vec![], 0, 0).truncated);
        let display_zero = project(rows(), 4, 0);
        assert_eq!(display_zero.omitted_observations, 4);
    }

    #[test]
    fn semantic_judgment_capture_deduplicates_and_excludes_all_conflicting_copies() {
        let first = metadata("one", &observation());
        let mut different = observation();
        different.correlation.round += 1;
        different.correlation.evaluation_span_id = "evaluation-2".into();
        let conflict = metadata("one", &different);
        let result = project(
            vec![
                row(first.clone()),
                row(first.clone()),
                row(metadata("two", &different)),
                row(conflict),
                row(first),
            ],
            8,
            8,
        );
        assert_eq!(result.duplicate_observations, 1);
        assert_eq!(result.observations.len(), 1);
        assert_eq!(result.observations[0].observation_span_id, "two");
        assert!(
            result
                .gaps
                .contains(&SemanticJudgmentCaptureGap::ConflictingIdentity)
        );
    }

    #[test]
    fn semantic_judgment_capture_rejects_wrong_subject_and_payload_run() {
        let meta = metadata("one", &observation());
        let mut wrong_owner = row(meta.clone());
        wrong_owner.user_id = "other".into();
        let mut wrong_session = row(meta.clone());
        wrong_session.session_id = "other".into();
        let mut wrong_run = meta;
        wrong_run["trace_id"] = json!("other-run");
        let result = project(vec![wrong_owner, wrong_session, row(wrong_run)], 3, 3);
        assert!(result.observations.is_empty());
        assert!(
            result
                .gaps
                .contains(&SemanticJudgmentCaptureGap::ScopeMismatch)
        );
    }

    #[test]
    fn semantic_judgment_decoder_is_bounded_strict_and_payload_free() {
        assert_eq!(
            decode_semantic_judgment_trace(&"x".repeat(MAX_SEMANTIC_JUDGMENT_TRACE_BYTES + 1)),
            Err(SemanticJudgmentCaptureGap::OversizedMetadata)
        );
        let mut meta = metadata("one", &observation());
        let raw = meta["attrs"][SEMANTIC_JUDGMENT_TRACE_ATTR]
            .as_str()
            .unwrap();
        let mut payload: Value = serde_json::from_str(raw).unwrap();
        payload["private"] = json!("PRIVATE_SECRET");
        meta["attrs"][SEMANTIC_JUDGMENT_TRACE_ATTR] = json!(payload.to_string());
        let result = project(vec![row(metadata("one", &observation())), row(meta)], 2, 2);
        assert!(result.observations.is_empty());
        assert!(
            !serde_json::to_string(&result)
                .unwrap()
                .contains("PRIVATE_SECRET")
        );
        let result = project(
            vec![row(json!({"name":"other", "dropped_events_before": 2}))],
            2,
            2,
        );
        assert!(
            result
                .gaps
                .contains(&SemanticJudgmentCaptureGap::JournalEvictionObserved)
        );
        assert!(result.observations.is_empty());
    }

    #[test]
    fn semantic_judgment_capture_does_not_refill_after_unrelated_or_oversized_rows() {
        let mut oversized = row(Value::Null);
        oversized.metadata_oversized = true;
        let result = project(
            vec![
                row(json!({"name":"other"})),
                oversized,
                row(metadata("late", &observation())),
            ],
            2,
            2,
        );
        assert_eq!(result.candidates_scanned, 2);
        assert!(result.observations.is_empty() && result.truncated);
        assert!(
            result
                .gaps
                .contains(&SemanticJudgmentCaptureGap::OversizedMetadata)
        );
    }
}
