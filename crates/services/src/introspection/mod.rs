//! Introspection API — cloud-side data for `get_agent_info` tool.
//!
//! Design principle: all numeric reasoning happens in Rust functions.
//! Callers (LLM) receive conclusions, not raw data.

pub mod database;
pub mod scoring;

use async_trait::async_trait;
use axum::{Json, http::StatusCode};
pub use database::DatabaseIntrospectionService;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use astra_core::{ErrorResponse, internal_error};

// ── Response types ───────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize)]
pub struct SkillInfo {
    pub name: String,
    pub version: String,
    pub description: String,
    pub category: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct SkillsIntrospectionResponse {
    pub installed: Vec<SkillInfo>,
    pub cloud: Vec<SkillInfo>,
}

/// The only durable event type accepted as an intent-drift assessment.
///
/// Generic `drift_detected` events have ambiguous provenance and are not part
/// of this contract. A producer must persist this exact event type together
/// with [`IntentDriftAssessmentV1`] metadata.
pub const INTENT_DRIFT_ASSESSMENT_EVENT_TYPE: &str = "intent_drift_assessment";
pub const INTENT_DRIFT_ASSESSMENT_SCHEMA_VERSION: u32 = 1;
pub const INTENT_DRIFT_ASSESSMENT_MAX_EVIDENCE_ITEMS: usize = 32;
pub const INTENT_DRIFT_ASSESSMENT_MAX_EVIDENCE_ITEM_BYTES: usize = 1_000;
pub const INTENT_DRIFT_CHECK_SCHEMA_VERSION: u32 = 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IntentDriftVerdict {
    Aligned,
    Drifting,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IntentDriftLevel {
    Aligned,
    Mild,
    Moderate,
    High,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IntentDriftAssessmentProvenanceKind {
    LlmJudge,
}

/// Durable provenance for a model-produced intent-drift verdict.
///
/// `invocation_id` joins the verdict to the inference ledger. Provider and
/// model are repeated here so the introspection projection is self-describing;
/// they are identities, not client-selectable execution inputs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IntentDriftAssessmentProvenance {
    pub kind: IntentDriftAssessmentProvenanceKind,
    pub invocation_id: String,
    pub provider: String,
    pub model: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_response_id: Option<String>,
}

/// Versioned metadata contract for `intent_drift_assessment` events.
///
/// Every semantic field is owned by the LLM judge. Consumers may validate and
/// project it, but must not derive a verdict from message text or manufacture
/// an aligned result when no event exists.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IntentDriftAssessmentV1 {
    pub schema_version: u32,
    pub provenance: IntentDriftAssessmentProvenance,
    pub verdict: IntentDriftVerdict,
    pub score: f64,
    pub level: IntentDriftLevel,
    #[serde(default)]
    pub evidence: Vec<String>,
    pub turn: u32,
    pub round: u32,
}

impl IntentDriftAssessmentV1 {
    pub fn validate(&self) -> Result<(), String> {
        if self.schema_version != INTENT_DRIFT_ASSESSMENT_SCHEMA_VERSION {
            return Err(format!(
                "unsupported intent-drift assessment schema version {}",
                self.schema_version
            ));
        }
        if !self.score.is_finite() || !(0.0..=1.0).contains(&self.score) {
            return Err(format!(
                "intent-drift assessment score must be finite and in 0..=1, got {}",
                self.score
            ));
        }
        match (self.verdict, self.level) {
            (IntentDriftVerdict::Aligned, IntentDriftLevel::Aligned)
            | (
                IntentDriftVerdict::Drifting,
                IntentDriftLevel::Mild | IntentDriftLevel::Moderate | IntentDriftLevel::High,
            ) => {}
            _ => {
                return Err(
                    "intent-drift assessment verdict and level are structurally inconsistent"
                        .to_string(),
                );
            }
        }
        for (field, value) in [
            ("invocation_id", self.provenance.invocation_id.as_str()),
            ("provider", self.provenance.provider.as_str()),
            ("model", self.provenance.model.as_str()),
        ] {
            validate_assessment_identity(field, value)?;
        }
        if let Some(provider_response_id) = self.provenance.provider_response_id.as_deref() {
            validate_assessment_identity("provider_response_id", provider_response_id)?;
        }
        if self.evidence.len() > INTENT_DRIFT_ASSESSMENT_MAX_EVIDENCE_ITEMS {
            return Err(format!(
                "intent-drift assessment evidence contains {} items; maximum is {}",
                self.evidence.len(),
                INTENT_DRIFT_ASSESSMENT_MAX_EVIDENCE_ITEMS
            ));
        }
        for item in &self.evidence {
            if item.is_empty()
                || item.trim() != item
                || item.len() > INTENT_DRIFT_ASSESSMENT_MAX_EVIDENCE_ITEM_BYTES
                || item.chars().any(char::is_control)
            {
                return Err(format!(
                    "intent-drift assessment evidence must contain exact non-empty text of at most {} bytes",
                    INTENT_DRIFT_ASSESSMENT_MAX_EVIDENCE_ITEM_BYTES
                ));
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IntentDriftAssessmentStatus {
    Assessed,
    Unavailable,
}

/// Strict projection returned by `GET /introspection/drift-check`.
///
/// The nullable fields intentionally preserve the distinction between a
/// model-produced `aligned` verdict and the absence of any assessment.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct IntentDriftCheckResponseV2 {
    pub schema_version: u32,
    pub user_id: String,
    pub session_id: String,
    pub assessment_status: IntentDriftAssessmentStatus,
    pub verdict: Option<IntentDriftVerdict>,
    pub score: Option<f64>,
    pub level: Option<IntentDriftLevel>,
    pub evidence: Vec<String>,
    pub provenance: Option<IntentDriftAssessmentProvenance>,
    pub turn: Option<u32>,
    pub round: Option<u32>,
    pub source_event_id: Option<String>,
    pub assessed_at: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct IntentDriftCheckResponseV2Wire {
    schema_version: u32,
    user_id: String,
    session_id: String,
    assessment_status: IntentDriftAssessmentStatus,
    verdict: Option<IntentDriftVerdict>,
    score: Option<f64>,
    level: Option<IntentDriftLevel>,
    evidence: Vec<String>,
    provenance: Option<IntentDriftAssessmentProvenance>,
    turn: Option<u32>,
    round: Option<u32>,
    source_event_id: Option<String>,
    assessed_at: Option<String>,
}

impl<'de> Deserialize<'de> for IntentDriftCheckResponseV2 {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let wire = IntentDriftCheckResponseV2Wire::deserialize(deserializer)?;
        let response = Self {
            schema_version: wire.schema_version,
            user_id: wire.user_id,
            session_id: wire.session_id,
            assessment_status: wire.assessment_status,
            verdict: wire.verdict,
            score: wire.score,
            level: wire.level,
            evidence: wire.evidence,
            provenance: wire.provenance,
            turn: wire.turn,
            round: wire.round,
            source_event_id: wire.source_event_id,
            assessed_at: wire.assessed_at,
        };
        response.validate().map_err(serde::de::Error::custom)?;
        Ok(response)
    }
}

impl IntentDriftCheckResponseV2 {
    pub fn unavailable(
        user_id: impl Into<String>,
        session_id: impl Into<String>,
    ) -> Result<Self, String> {
        let response = Self {
            schema_version: INTENT_DRIFT_CHECK_SCHEMA_VERSION,
            user_id: user_id.into(),
            session_id: session_id.into(),
            assessment_status: IntentDriftAssessmentStatus::Unavailable,
            verdict: None,
            score: None,
            level: None,
            evidence: Vec::new(),
            provenance: None,
            turn: None,
            round: None,
            source_event_id: None,
            assessed_at: None,
        };
        response.validate()?;
        Ok(response)
    }

    pub fn assessed(
        user_id: impl Into<String>,
        session_id: impl Into<String>,
        assessment: IntentDriftAssessmentV1,
        source_event_id: impl Into<String>,
        assessed_at: impl Into<String>,
    ) -> Result<Self, String> {
        assessment.validate()?;
        let response = Self {
            schema_version: INTENT_DRIFT_CHECK_SCHEMA_VERSION,
            user_id: user_id.into(),
            session_id: session_id.into(),
            assessment_status: IntentDriftAssessmentStatus::Assessed,
            verdict: Some(assessment.verdict),
            score: Some(assessment.score),
            level: Some(assessment.level),
            evidence: assessment.evidence,
            provenance: Some(assessment.provenance),
            turn: Some(assessment.turn),
            round: Some(assessment.round),
            source_event_id: Some(source_event_id.into()),
            assessed_at: Some(assessed_at.into()),
        };
        response.validate()?;
        Ok(response)
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.schema_version != INTENT_DRIFT_CHECK_SCHEMA_VERSION {
            return Err(format!(
                "unsupported intent-drift check schema version {}",
                self.schema_version
            ));
        }
        validate_assessment_identity("user_id", &self.user_id)?;
        validate_assessment_identity("session_id", &self.session_id)?;

        match self.assessment_status {
            IntentDriftAssessmentStatus::Unavailable => {
                if self.verdict.is_some()
                    || self.score.is_some()
                    || self.level.is_some()
                    || !self.evidence.is_empty()
                    || self.provenance.is_some()
                    || self.turn.is_some()
                    || self.round.is_some()
                    || self.source_event_id.is_some()
                    || self.assessed_at.is_some()
                {
                    return Err(
                        "unavailable intent-drift checks must not contain assessment fields"
                            .to_string(),
                    );
                }
            }
            IntentDriftAssessmentStatus::Assessed => {
                let assessment = IntentDriftAssessmentV1 {
                    schema_version: INTENT_DRIFT_ASSESSMENT_SCHEMA_VERSION,
                    provenance: self.provenance.clone().ok_or_else(|| {
                        "assessed intent-drift check is missing provenance".to_string()
                    })?,
                    verdict: self.verdict.ok_or_else(|| {
                        "assessed intent-drift check is missing verdict".to_string()
                    })?,
                    score: self.score.ok_or_else(|| {
                        "assessed intent-drift check is missing score".to_string()
                    })?,
                    level: self.level.ok_or_else(|| {
                        "assessed intent-drift check is missing level".to_string()
                    })?,
                    evidence: self.evidence.clone(),
                    turn: self
                        .turn
                        .ok_or_else(|| "assessed intent-drift check is missing turn".to_string())?,
                    round: self.round.ok_or_else(|| {
                        "assessed intent-drift check is missing round".to_string()
                    })?,
                };
                assessment.validate()?;
                validate_assessment_identity(
                    "source_event_id",
                    self.source_event_id.as_deref().ok_or_else(|| {
                        "assessed intent-drift check is missing source_event_id".to_string()
                    })?,
                )?;
                validate_assessment_identity(
                    "assessed_at",
                    self.assessed_at.as_deref().ok_or_else(|| {
                        "assessed intent-drift check is missing assessed_at".to_string()
                    })?,
                )?;
            }
        }
        Ok(())
    }
}

fn validate_assessment_identity(field: &str, value: &str) -> Result<(), String> {
    if value.is_empty()
        || value.trim() != value
        || value.len() > 255
        || value.chars().any(char::is_control)
    {
        return Err(format!(
            "intent-drift assessment {field} must be an exact non-empty identifier of at most 255 bytes"
        ));
    }
    Ok(())
}

// ── Trait ─────────────────────────────────────────────────────────────────────

pub type ServiceResult<T> = Result<T, (StatusCode, Json<ErrorResponse>)>;

#[async_trait]
pub trait IntrospectionService: Send + Sync {
    async fn get_skills_introspection(
        &self,
        user_id: &str,
    ) -> ServiceResult<SkillsIntrospectionResponse>;

    async fn get_context_trend(
        &self,
        user_id: &str,
        session_id: &str,
        turns: i32,
        context_window: i64,
    ) -> ServiceResult<Value>;

    async fn get_context_snapshot(
        &self,
        user_id: &str,
        session_id: &str,
        turn_index: Option<i32>,
        detail: bool,
        raw: bool,
        raw_token_budget: i32,
    ) -> ServiceResult<Value>;

    async fn get_retrieval_quality(
        &self,
        user_id: &str,
        session_id: &str,
        turns: i32,
    ) -> ServiceResult<Value>;

    /// Recent decisions for a session — who picked what, why, and when.
    /// Exposed to the LLM so it can read its own decision trace in-turn
    /// instead of waiting for a human-facing dashboard.
    ///
    /// Response shape (schema_version = 1):
    /// ```json
    /// {
    ///   "schema_version": 1,
    ///   "session_id": "...",
    ///   "user_id": "...",
    ///   "last_n": 20,
    ///   "decisions": [
    ///     {"decision_id": "...", "decision_type": "tool_surface",
    ///      "created_at": "...", "output": {...}},
    ///     ...
    ///   ]
    /// }
    /// ```
    async fn get_decision_trace(
        &self,
        user_id: &str,
        session_id: &str,
        last_n: i32,
    ) -> ServiceResult<Value>;

    /// Per-tool history across the caller's sessions within a rolling
    /// `window_hours` (default 24).
    ///
    /// Response shape (schema_version = 1):
    /// ```json
    /// {
    ///   "schema_version": 1,
    ///   "user_id": "...", "tool": "...", "window_hours": 24,
    ///   "total_calls": N, "ok_count": N, "fail_count": N,
    ///   "success_rate": 0.0..=1.0,
    ///   "recent_failures": [
    ///     {"session_id": "...", "error_preview": "...", "created_at": "..."},
    ///     ...
    ///   ]
    /// }
    /// ```
    async fn get_tool_history(
        &self,
        user_id: &str,
        tool: &str,
        window_hours: i32,
    ) -> ServiceResult<Value>;

    /// Latest durable LLM intent-drift assessment for the active session.
    ///
    /// This endpoint is projection-only. It never compares message text and
    /// never treats absence of an assessment as evidence of alignment.
    ///
    /// Response shape (schema_version = 2):
    /// ```json
    /// {
    ///   "schema_version": 2,
    ///   "user_id": "...", "session_id": "...",
    ///   "assessment_status": "assessed" | "unavailable",
    ///   "verdict": "aligned" | "drifting" | null,
    ///   "score": 0.0..=1.0 | null,
    ///   "level": "aligned" | "mild" | "moderate" | "high" | null,
    ///   "evidence": [],
    ///   "provenance": {...} | null,
    ///   "turn": 3 | null, "round": 1 | null,
    ///   "source_event_id": "..." | null,
    ///   "assessed_at": "..." | null
    /// }
    /// ```
    async fn get_drift_check(
        &self,
        user_id: &str,
        session_id: &str,
    ) -> ServiceResult<IntentDriftCheckResponseV2>;
}

// ── Unconfigured implementation ──────────────────────────────────────────────

pub struct UnconfiguredIntrospectionService;

#[async_trait]
impl IntrospectionService for UnconfiguredIntrospectionService {
    async fn get_skills_introspection(
        &self,
        _: &str,
    ) -> ServiceResult<SkillsIntrospectionResponse> {
        Err(internal_error("introspection service not configured"))
    }
    async fn get_context_trend(&self, _: &str, _: &str, _: i32, _: i64) -> ServiceResult<Value> {
        Err(internal_error("introspection service not configured"))
    }
    async fn get_context_snapshot(
        &self,
        _: &str,
        _: &str,
        _: Option<i32>,
        _: bool,
        _: bool,
        _: i32,
    ) -> ServiceResult<Value> {
        Err(internal_error("introspection service not configured"))
    }
    async fn get_retrieval_quality(&self, _: &str, _: &str, _: i32) -> ServiceResult<Value> {
        Err(internal_error("introspection service not configured"))
    }
    async fn get_decision_trace(&self, _: &str, _: &str, _: i32) -> ServiceResult<Value> {
        Err(internal_error("introspection service not configured"))
    }
    async fn get_tool_history(&self, _: &str, _: &str, _: i32) -> ServiceResult<Value> {
        Err(internal_error("introspection service not configured"))
    }
    async fn get_drift_check(&self, _: &str, _: &str) -> ServiceResult<IntentDriftCheckResponseV2> {
        Err(internal_error("introspection service not configured"))
    }
}

// ── Query parameter types ────────────────────────────────────────────────────

fn default_turns() -> i32 {
    10
}
fn default_context_window() -> i64 {
    200000
}
fn default_retrieval_turns() -> i32 {
    5
}
fn default_raw_token_budget() -> i32 {
    2000
}

#[derive(Deserialize)]
pub struct ContextTrendQuery {
    pub session_id: String,
    #[serde(default = "default_turns")]
    pub turns: i32,
    #[serde(default = "default_context_window")]
    pub context_window: i64,
}

#[derive(Deserialize)]
pub struct ContextSnapshotQuery {
    pub session_id: String,
    pub turn_index: Option<i32>,
    #[serde(default)]
    pub detail: bool,
    #[serde(default)]
    pub raw: bool,
    #[serde(default = "default_raw_token_budget")]
    pub raw_token_budget: i32,
}

#[derive(Deserialize)]
pub struct RetrievalQualityQuery {
    pub session_id: String,
    #[serde(default = "default_retrieval_turns")]
    pub turns: i32,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn context_trend_query_defaults() {
        let json = r#"{"session_id": "s1"}"#;
        let q: ContextTrendQuery = serde_json::from_str(json).unwrap();
        assert_eq!(q.turns, 10);
        assert_eq!(q.context_window, 200000);
    }

    #[test]
    fn context_snapshot_query_defaults() {
        let json = r#"{"session_id": "s1"}"#;
        let q: ContextSnapshotQuery = serde_json::from_str(json).unwrap();
        assert_eq!(q.raw_token_budget, 2000);
        assert!(!q.detail);
        assert!(!q.raw);
        assert!(q.turn_index.is_none());
    }

    #[test]
    fn retrieval_quality_query_defaults() {
        let json = r#"{"session_id": "s1"}"#;
        let q: RetrievalQualityQuery = serde_json::from_str(json).unwrap();
        assert_eq!(q.turns, 5);
    }

    #[test]
    fn intent_drift_contract_accepts_only_explicit_llm_provenance() {
        let valid = serde_json::json!({
            "schema_version": 1,
            "provenance": {
                "kind": "llm_judge",
                "invocation_id": "invocation-1",
                "provider": "provider-1",
                "model": "model-1"
            },
            "verdict": "aligned",
            "score": 0.1,
            "level": "aligned",
            "evidence": [],
            "turn": 2,
            "round": 1
        });
        let assessment: IntentDriftAssessmentV1 =
            serde_json::from_value(valid.clone()).expect("typed LLM assessment");
        assessment.validate().expect("valid assessment");

        let mut non_llm = valid;
        non_llm["provenance"]["kind"] = serde_json::json!("heuristic");
        assert!(serde_json::from_value::<IntentDriftAssessmentV1>(non_llm).is_err());
    }

    #[test]
    fn intent_drift_contract_rejects_semantically_inconsistent_fields() {
        let mut assessment = IntentDriftAssessmentV1 {
            schema_version: INTENT_DRIFT_ASSESSMENT_SCHEMA_VERSION,
            provenance: IntentDriftAssessmentProvenance {
                kind: IntentDriftAssessmentProvenanceKind::LlmJudge,
                invocation_id: "invocation-1".into(),
                provider: "provider-1".into(),
                model: "model-1".into(),
                provider_response_id: None,
            },
            verdict: IntentDriftVerdict::Aligned,
            score: 0.9,
            level: IntentDriftLevel::High,
            evidence: Vec::new(),
            turn: 2,
            round: 1,
        };

        assert!(assessment.validate().is_err());

        assessment.verdict = IntentDriftVerdict::Drifting;
        assessment.evidence =
            vec!["bounded evidence".into(); INTENT_DRIFT_ASSESSMENT_MAX_EVIDENCE_ITEMS + 1];
        assert!(assessment.validate().is_err());

        assessment.evidence = vec!["x".repeat(INTENT_DRIFT_ASSESSMENT_MAX_EVIDENCE_ITEM_BYTES + 1)];
        assert!(assessment.validate().is_err());
    }

    #[test]
    fn intent_drift_check_distinguishes_unavailable_from_aligned() {
        let unavailable = IntentDriftCheckResponseV2::unavailable("owner-1", "session-1").unwrap();
        let wire = serde_json::to_value(&unavailable).unwrap();

        assert_eq!(wire["assessment_status"], "unavailable");
        assert!(wire["verdict"].is_null());
        assert!(wire["score"].is_null());
        assert!(wire["level"].is_null());
        assert_eq!(wire["evidence"], serde_json::json!([]));
        serde_json::from_value::<IntentDriftCheckResponseV2>(wire)
            .expect("canonical unavailable projection round trips");
    }

    #[test]
    fn intent_drift_check_deserialization_enforces_structure_and_llm_provenance() {
        let valid = serde_json::json!({
            "schema_version": 2,
            "user_id": "owner-1",
            "session_id": "session-1",
            "assessment_status": "assessed",
            "verdict": "drifting",
            "score": 0.75,
            "level": "high",
            "evidence": ["judge evidence"],
            "provenance": {
                "kind": "llm_judge",
                "invocation_id": "invocation-1",
                "provider": "provider-1",
                "model": "model-1"
            },
            "turn": 4,
            "round": 2,
            "source_event_id": "event-1",
            "assessed_at": "2026-07-30T12:00:00Z"
        });
        serde_json::from_value::<IntentDriftCheckResponseV2>(valid.clone())
            .expect("valid assessed projection");

        let mut unavailable_with_verdict = valid.clone();
        unavailable_with_verdict["assessment_status"] = serde_json::json!("unavailable");
        assert!(
            serde_json::from_value::<IntentDriftCheckResponseV2>(unavailable_with_verdict).is_err()
        );

        let mut non_llm = valid.clone();
        non_llm["provenance"]["kind"] = serde_json::json!("heuristic");
        assert!(serde_json::from_value::<IntentDriftCheckResponseV2>(non_llm).is_err());

        let mut inconsistent = valid.clone();
        inconsistent["verdict"] = serde_json::json!("aligned");
        assert!(serde_json::from_value::<IntentDriftCheckResponseV2>(inconsistent).is_err());

        let mut unknown = valid;
        unknown["locally_inferred"] = serde_json::json!(true);
        assert!(serde_json::from_value::<IntentDriftCheckResponseV2>(unknown).is_err());
    }
}
