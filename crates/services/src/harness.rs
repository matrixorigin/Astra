use async_trait::async_trait;
use axum::{Json, http::StatusCode};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::{MySql, QueryBuilder, Row};
use std::{
    collections::{BTreeSet, HashMap, HashSet},
    sync::Arc,
};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use astra_core::{ErrorResponse, SharedPool, error_response, internal_error};

use crate::evaluation::api::{
    EvaluationExperimentPrepareResponse, EvaluationPrepareCase, EvaluationPrepareRevision,
    EvaluationPrepareTarget,
};
use crate::evaluation::{EvaluationTargetKind, NO_SKILL_REVISION_ID};
use crate::models::parse_pricing_snapshot;
use crate::personal_skills::{
    CreateUserSkillSource, DatabasePersonalSkillStore, SubmitUserSkillVersion,
};

type HarnessResult<T> = Result<T, (StatusCode, Json<ErrorResponse>)>;
type SkillRuleReviewIds = (String, String);

const SKILLIFY_HARNESS_ID: &str = "skillify";
const SKILLIFY_VERSION_ID: &str = "skillify.v1";
const SKILLIFY_TEMPLATE_ID: &str = "skillify.v1";
const MAX_SKILLIFY_SESSIONS: usize = 20;
const MAX_SKILLIFY_EVENTS: i64 = 2_000;
const MAX_SKILLIFY_SOURCE_FILES: usize = 10;
const MAX_SKILLIFY_SOURCE_FILE_CHARS: usize = 200_000;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct HarnessTemplateRecord {
    pub template_id: String,
    pub name: String,
    pub description: String,
    pub built_in: bool,
    pub input_schema_json: Value,
    pub workflow_json: Value,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct HarnessNodeCatalogRecord {
    pub node_type: String,
    pub description: String,
    pub input_schema_json: Value,
    pub output_schema_json: Value,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct HarnessRunRecord {
    pub harness_run_id: String,
    pub harness_id: String,
    pub version_id: String,
    pub user_id: String,
    pub session_id: Option<String>,
    pub status: String,
    pub input_json: Value,
    pub output_json: Value,
    pub error: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct HarnessItemRecord {
    pub item_id: String,
    pub harness_run_id: String,
    pub item_type: String,
    pub locator_json: Value,
    pub input_json: Value,
    pub proposed_output_json: Value,
    pub final_output_json: Value,
    pub decision_history_json: Value,
    pub status: String,
    pub confidence: Option<f64>,
    pub assigned_to: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct HarnessCitationRecord {
    pub citation_id: String,
    pub harness_run_id: String,
    pub item_id: String,
    pub skill_draft_id: Option<String>,
    pub skill_rule_id: Option<String>,
    pub source_id: Option<String>,
    pub source_locator_json: Value,
    pub source_snapshot_ref: Option<String>,
    pub source_content_hash: Option<String>,
    pub source_metadata_json: Value,
    pub artifact_id: Option<String>,
    pub quote_hash: Option<String>,
    pub evidence_text_preview: Option<String>,
    pub relevance_score: Option<f64>,
    pub created_by_node_id: Option<String>,
    pub created_at: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct HarnessSkillRuleRecord {
    pub skill_rule_id: String,
    pub skill_draft_id: String,
    pub harness_run_id: String,
    pub rule_type: String,
    pub statement: String,
    pub rationale: String,
    pub decision_history_json: Value,
    pub status: String,
    pub confidence: Option<f64>,
    pub source_count: i64,
    pub created_by_node_id: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    pub citations: Vec<HarnessCitationRecord>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct HarnessSkillDraftRecord {
    pub skill_draft_id: String,
    pub harness_run_id: String,
    pub candidate_name: String,
    pub description: String,
    pub target_scope: String,
    pub publish_visibility: String,
    pub content_markdown: String,
    pub source_summary_json: Value,
    pub decision_history_json: Value,
    pub status: String,
    pub confidence: Option<f64>,
    pub created_by_node_id: Option<String>,
    pub revision: i64,
    pub published_version_id: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    pub rules: Vec<HarnessSkillRuleRecord>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum HarnessSkillRuleStatusKind {
    Proposed,
    Conflicted,
    NeedsRevision,
    Approved,
    Edited,
    Rejected,
    Other,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum HarnessSkillDraftStatusKind {
    ReadyToPublish,
    Approved,
    PendingRuleReview,
    NeedsRevision,
    Rejected,
    Other,
}

fn harness_skill_rule_status_kind(status: &str) -> HarnessSkillRuleStatusKind {
    match status {
        "proposed" => HarnessSkillRuleStatusKind::Proposed,
        "conflicted" => HarnessSkillRuleStatusKind::Conflicted,
        "needs_revision" => HarnessSkillRuleStatusKind::NeedsRevision,
        "approved" => HarnessSkillRuleStatusKind::Approved,
        "edited" => HarnessSkillRuleStatusKind::Edited,
        "rejected" => HarnessSkillRuleStatusKind::Rejected,
        _ => HarnessSkillRuleStatusKind::Other,
    }
}

fn harness_skill_draft_status_kind(status: &str) -> HarnessSkillDraftStatusKind {
    match status {
        "ready_to_publish" => HarnessSkillDraftStatusKind::ReadyToPublish,
        "approved" => HarnessSkillDraftStatusKind::Approved,
        "pending_rule_review" => HarnessSkillDraftStatusKind::PendingRuleReview,
        "needs_revision" => HarnessSkillDraftStatusKind::NeedsRevision,
        "rejected" => HarnessSkillDraftStatusKind::Rejected,
        _ => HarnessSkillDraftStatusKind::Other,
    }
}

fn harness_skill_rule_blocks_draft_approval(status: &str) -> bool {
    matches!(
        harness_skill_rule_status_kind(status),
        HarnessSkillRuleStatusKind::Conflicted
            | HarnessSkillRuleStatusKind::NeedsRevision
            | HarnessSkillRuleStatusKind::Rejected
    )
}

fn harness_skill_rule_is_unresolved(status: &str) -> bool {
    matches!(
        harness_skill_rule_status_kind(status),
        HarnessSkillRuleStatusKind::Proposed
            | HarnessSkillRuleStatusKind::Conflicted
            | HarnessSkillRuleStatusKind::NeedsRevision
    )
}

fn harness_skill_rule_is_approved(status: &str) -> bool {
    matches!(
        harness_skill_rule_status_kind(status),
        HarnessSkillRuleStatusKind::Approved | HarnessSkillRuleStatusKind::Edited
    )
}

fn harness_skill_draft_is_publishable(status: &str) -> bool {
    matches!(
        harness_skill_draft_status_kind(status),
        HarnessSkillDraftStatusKind::ReadyToPublish | HarnessSkillDraftStatusKind::Approved
    )
}

fn derive_harness_skill_draft_status(rules: &[HarnessSkillRuleRecord]) -> &'static str {
    let unresolved = rules
        .iter()
        .any(|rule| harness_skill_rule_is_unresolved(&rule.status));
    let approved = rules
        .iter()
        .any(|rule| harness_skill_rule_is_approved(&rule.status));

    if unresolved {
        "pending_rule_review"
    } else if approved {
        "ready_to_publish"
    } else {
        "rejected"
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SkillifySourceFile {
    pub file_name: String,
    pub mime_type: Option<String>,
    pub content: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SkillifyRunRequest {
    pub session_ids: Vec<String>,
    pub source_files: Option<Vec<SkillifySourceFile>>,
    pub skill_name: Option<String>,
    pub topic: Option<String>,
    pub target_scope: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idempotency_key: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct HarnessDecisionRequest {
    pub expected_revision: Option<i64>,
    pub decision: String,
    pub after_json: Option<Value>,
    pub reason: Option<String>,
    pub idempotency_key: Option<String>,
}

/// Natural-language authoring input. The caller does not choose a Harness,
/// model, verifier, or source projection.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuthoringIntentRequest {
    pub goal: String,
    #[serde(default)]
    pub create_new: bool,
    pub target_skill: Option<AuthoringSkillTarget>,
    pub validation_task: Option<AuthoringValidationTask>,
    /// Replay one operation with the same key; omit it to start a fresh attempt.
    pub idempotency_key: Option<String>,
}

impl AuthoringIntentRequest {
    fn operation_key(&self, session_id: &str) -> Option<String> {
        self.idempotency_key.as_ref().map(|key| {
            format!(
                "authoring:{}",
                stable_hash(
                    &json!([
                        session_id.trim(),
                        self.goal.trim(),
                        self.target_skill,
                        self.create_new,
                        key
                    ])
                    .to_string()
                )
            )
        })
    }

    /// A transport can retain this reference even when it stops waiting.
    pub fn harness_run_id(&self, user_id: &str, session_id: &str) -> Option<String> {
        self.operation_key(session_id)
            .map(|key| skillify_harness_run_id(user_id, Some(&key)))
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuthoringSkillTarget {
    pub skill_name: String,
    pub version_id: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuthoringValidationTask {
    pub source_id: String,
    pub expected_result: Value,
}

struct AuthoringGenerationContext {
    baseline: Option<crate::personal_skills::ActivePersonalSkillRecord>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthoringEvaluationSummary {
    pub status: String,
    pub reason: String,
    pub experiment_id: Option<String>,
}

/// Server-resolved input for the shared Evaluation prepare boundary. It is
/// skipped from the HTTP response because clients submit only the natural
/// language goal; the runtime handler consumes it after authoring has
/// materialized the candidate.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AuthoringEvaluationInput {
    pub submission_idempotency_key: String,
    pub target: EvaluationPrepareTarget,
    pub case: EvaluationPrepareCase,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AuthoringInferenceEvidence {
    pub schema_version: u32,
    pub invocation_count: usize,
    pub physical_attempt_count: usize,
    pub priced_attempt_count: usize,
    pub exact_usage_attempt_count: usize,
    pub complete: bool,
    pub settlement_pending: bool,
    pub usage_status: String,
    pub providers: Vec<String>,
    pub models: Vec<String>,
    pub offering_ids: Vec<String>,
    pub operations: Vec<String>,
    pub prompt_tokens: Option<u64>,
    pub completion_tokens: Option<u64>,
    pub cache_read_tokens: Option<u64>,
    pub cache_creation_tokens: Option<u64>,
    pub estimated_cost_usd: Option<f64>,
    pub completeness_reasons: Vec<String>,
    pub evidence_fingerprint: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AuthoringIntentRecord {
    pub target: String,
    pub operation: String,
    pub resolution_source: String,
    pub goal: String,
    pub harness_run: HarnessRunRecord,
    pub skill_drafts: Vec<HarnessSkillDraftRecord>,
    pub evaluation: AuthoringEvaluationSummary,
    pub inference: AuthoringInferenceEvidence,
    #[serde(default)]
    pub evaluation_plan: Option<EvaluationExperimentPrepareResponse>,
    #[serde(skip)]
    pub evaluation_input: Option<AuthoringEvaluationInput>,
}

impl AuthoringIntentRecord {
    /// The shared conversational continuation contract for Server and CLI.
    pub fn tool_output(&self) -> Value {
        json!({
            "status": "candidate_ready", "target": self.target, "operation": self.operation,
            "harness_run_id": self.harness_run.harness_run_id,
            "result_url": format!("/authoring?runId={}", self.harness_run.harness_run_id),
            "candidates": self.skill_drafts.iter().map(|draft| json!({
                "skill_draft_id": draft.skill_draft_id,
                "review_url": format!("/harnesses?runId={}&draftId={}", self.harness_run.harness_run_id, draft.skill_draft_id),
                "name": draft.candidate_name, "description": draft.description,
                "content_markdown": draft.content_markdown, "status": draft.status,
            })).collect::<Vec<_>>(),
            "evaluation": self.evaluation,
            "source_coverage": self.harness_run.input_json.get("source_coverage"),
            "evidence": {
                "inference_complete": self.inference.complete,
                "usage_status": self.inference.usage_status,
                "estimated_cost_usd": self.inference.estimated_cost_usd,
            },
            "next_step": "The candidate is private and inactive. Follow result_url to inspect or continue this same evaluation, review evidence, save and explicitly use this candidate. Prepared means trials have not yet been evaluated.",
        })
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SkillifyPublishRequest {
    pub expected_revision: i64,
    pub visibility: Option<String>,
    pub version: Option<String>,
    pub description: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SkillifyPublishRecord {
    pub harness_run_id: String,
    pub skill_draft_id: String,
    pub skill_name: String,
    pub version_id: String,
    pub visibility: String,
    pub content_markdown: String,
    pub approved_rule_count: usize,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SkillifySourcePacket {
    pub event_id: String,
    pub session_id: String,
    pub source_id: String,
    pub source_type: String,
    pub title: String,
    pub event_type: String,
    pub content: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SkillifyAgentRequest {
    pub user_id: String,
    pub harness_run_id: String,
    pub skill_name: Option<String>,
    pub topic: Option<String>,
    pub target_scope: String,
    pub source_packets: Vec<SkillifySourcePacket>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SkillifyAgentCitation {
    pub source_id: String,
    pub source_excerpt: String,
    pub source_locator_json: Value,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SkillifyAgentRule {
    pub rule_type: String,
    pub statement: String,
    pub rationale: String,
    pub confidence: Option<f64>,
    pub citations: Vec<SkillifyAgentCitation>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SkillifyAgentDraft {
    pub candidate_name: String,
    pub description: String,
    pub target_scope: String,
    pub publish_visibility: String,
    pub content_markdown: String,
    pub source_summary_json: Value,
    pub confidence: Option<f64>,
    pub rules: Vec<SkillifyAgentRule>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SkillifyAgentOutput {
    pub extractor: String,
    pub subagent_strategy: Value,
    pub drafts: Vec<SkillifyAgentDraft>,
}

#[async_trait]
pub trait SkillifyAgentExecutor: Send + Sync {
    async fn synthesize_skill_drafts(
        &self,
        request: SkillifyAgentRequest,
        cancel_token: Option<CancellationToken>,
    ) -> Result<SkillifyAgentOutput, String>;
}

#[async_trait]
pub trait HarnessService: Send + Sync {
    async fn list_templates(
        &self,
    ) -> Result<Vec<HarnessTemplateRecord>, (StatusCode, Json<ErrorResponse>)>;

    async fn list_node_catalog(
        &self,
    ) -> Result<Vec<HarnessNodeCatalogRecord>, (StatusCode, Json<ErrorResponse>)>;

    async fn create_skillify_run(
        &self,
        user_id: String,
        request: SkillifyRunRequest,
    ) -> Result<HarnessRunRecord, (StatusCode, Json<ErrorResponse>)>;

    async fn create_authoring_intent(
        &self,
        user_id: String,
        session_id: String,
        request: AuthoringIntentRequest,
        cancel_token: Option<CancellationToken>,
    ) -> Result<AuthoringIntentRecord, (StatusCode, Json<ErrorResponse>)>;

    async fn persist_authoring_evaluation(
        &self,
        user_id: String,
        harness_run_id: String,
        expected_candidate_revision_id: String,
        evaluation: AuthoringEvaluationSummary,
        plan: Option<EvaluationExperimentPrepareResponse>,
    ) -> Result<HarnessRunRecord, (StatusCode, Json<ErrorResponse>)>;

    async fn get_run(
        &self,
        user_id: String,
        harness_run_id: String,
    ) -> Result<HarnessRunRecord, (StatusCode, Json<ErrorResponse>)>;

    async fn list_run_items(
        &self,
        user_id: String,
        harness_run_id: String,
    ) -> Result<Vec<HarnessItemRecord>, (StatusCode, Json<ErrorResponse>)>;

    async fn decide_item(
        &self,
        user_id: String,
        harness_run_id: String,
        item_id: String,
        request: HarnessDecisionRequest,
    ) -> Result<HarnessItemRecord, (StatusCode, Json<ErrorResponse>)>;

    async fn list_skill_drafts(
        &self,
        user_id: String,
        harness_run_id: String,
    ) -> Result<Vec<HarnessSkillDraftRecord>, (StatusCode, Json<ErrorResponse>)>;

    async fn get_skill_draft(
        &self,
        user_id: String,
        harness_run_id: String,
        skill_draft_id: String,
    ) -> Result<HarnessSkillDraftRecord, (StatusCode, Json<ErrorResponse>)>;

    async fn decide_skill_draft(
        &self,
        user_id: String,
        harness_run_id: String,
        skill_draft_id: String,
        request: HarnessDecisionRequest,
    ) -> Result<HarnessSkillDraftRecord, (StatusCode, Json<ErrorResponse>)>;

    async fn decide_skill_rule(
        &self,
        user_id: String,
        harness_run_id: String,
        skill_draft_id: String,
        skill_rule_id: String,
        request: HarnessDecisionRequest,
    ) -> Result<HarnessSkillDraftRecord, (StatusCode, Json<ErrorResponse>)>;

    async fn publish_skill_draft(
        &self,
        user_id: String,
        harness_run_id: String,
        skill_draft_id: String,
        request: SkillifyPublishRequest,
    ) -> Result<SkillifyPublishRecord, (StatusCode, Json<ErrorResponse>)>;
}

#[derive(Clone)]
pub struct DatabaseHarnessService {
    pool: SharedPool,
    skillify_agent_executor: Option<Arc<dyn SkillifyAgentExecutor>>,
}

impl DatabaseHarnessService {
    pub fn new(pool: SharedPool) -> Self {
        Self {
            pool,
            skillify_agent_executor: None,
        }
    }

    pub fn with_skillify_agent_executor(
        mut self,
        executor: Arc<dyn SkillifyAgentExecutor>,
    ) -> Self {
        self.skillify_agent_executor = Some(executor);
        self
    }

    async fn ensure_run_owner(
        &self,
        user_id: &str,
        harness_run_id: &str,
    ) -> Result<HarnessRunRecord, (StatusCode, Json<ErrorResponse>)> {
        let run = self.load_run(harness_run_id).await?;
        if run.user_id != user_id {
            return Err(error_response(
                StatusCode::NOT_FOUND,
                "harness run not found",
            ));
        }
        Ok(run)
    }

    async fn load_run(
        &self,
        harness_run_id: &str,
    ) -> Result<HarnessRunRecord, (StatusCode, Json<ErrorResponse>)> {
        let row = sqlx::query(
            "SELECT harness_run_id, harness_id, version_id, user_id, session_id, status,
                    IFNULL(input_json, '{}') AS input_json,
                    IFNULL(output_json, '{}') AS output_json,
                    error,
                    CAST(created_at AS CHAR) AS created_at,
                    CAST(updated_at AS CHAR) AS updated_at
             FROM harness_runs
             WHERE harness_run_id = ?
             LIMIT 1",
        )
        .bind(harness_run_id)
        .fetch_optional(self.pool.get())
        .await
        .map_err(internal_error)?;

        let row =
            row.ok_or_else(|| error_response(StatusCode::NOT_FOUND, "harness run not found"))?;
        Ok(HarnessRunRecord {
            harness_run_id: required_harness_string(&row, "harness_runs", "harness_run_id")?,
            harness_id: required_harness_string(&row, "harness_runs", "harness_id")?,
            version_id: required_harness_string(&row, "harness_runs", "version_id")?,
            user_id: required_harness_string(&row, "harness_runs", "user_id")?,
            session_id: optional_harness_string(&row, "harness_runs", "session_id")?,
            status: required_harness_string(&row, "harness_runs", "status")?,
            input_json: parse_json_cell(&row, "harness_runs", "input_json")?,
            output_json: parse_json_cell(&row, "harness_runs", "output_json")?,
            error: optional_harness_string(&row, "harness_runs", "error")?,
            created_at: required_harness_string(&row, "harness_runs", "created_at")?,
            updated_at: required_harness_string(&row, "harness_runs", "updated_at")?,
        })
    }

    async fn load_item(
        &self,
        harness_run_id: &str,
        item_id: &str,
    ) -> Result<HarnessItemRecord, (StatusCode, Json<ErrorResponse>)> {
        let row = sqlx::query(
            "SELECT item_id, harness_run_id, item_type,
                    IFNULL(locator_json, '{}') AS locator_json,
                    IFNULL(input_json, '{}') AS input_json,
                    IFNULL(proposed_output_json, '{}') AS proposed_output_json,
                    IFNULL(final_output_json, '{}') AS final_output_json,
                    IFNULL(decision_history_json, '[]') AS decision_history_json,
                    status, confidence, assigned_to,
                    CAST(created_at AS CHAR) AS created_at,
                    CAST(updated_at AS CHAR) AS updated_at
             FROM harness_items
             WHERE harness_run_id = ? AND item_id = ?
             LIMIT 1",
        )
        .bind(harness_run_id)
        .bind(item_id)
        .fetch_optional(self.pool.get())
        .await
        .map_err(internal_error)?;

        let row =
            row.ok_or_else(|| error_response(StatusCode::NOT_FOUND, "harness item not found"))?;
        item_from_row(row)
    }

    async fn load_skill_draft(
        &self,
        harness_run_id: &str,
        skill_draft_id: &str,
    ) -> Result<HarnessSkillDraftRecord, (StatusCode, Json<ErrorResponse>)> {
        let row = sqlx::query(
            "SELECT skill_draft_id, harness_run_id, candidate_name, description,
                    target_scope, publish_visibility, content_markdown,
                    IFNULL(source_summary_json, '{}') AS source_summary_json,
                    IFNULL(decision_history_json, '[]') AS decision_history_json,
                    status, confidence, created_by_node_id, revision, published_version_id,
                    CAST(created_at AS CHAR) AS created_at,
                    CAST(updated_at AS CHAR) AS updated_at
             FROM harness_skill_drafts
             WHERE harness_run_id = ? AND skill_draft_id = ?
             LIMIT 1",
        )
        .bind(harness_run_id)
        .bind(skill_draft_id)
        .fetch_optional(self.pool.get())
        .await
        .map_err(internal_error)?;

        let row =
            row.ok_or_else(|| error_response(StatusCode::NOT_FOUND, "skill draft not found"))?;
        let mut draft = skill_draft_from_row(row)?;
        draft.rules = self
            .load_skill_rules(harness_run_id, skill_draft_id)
            .await?;
        Ok(draft)
    }

    async fn load_skill_drafts_for_run(
        &self,
        harness_run_id: &str,
    ) -> Result<Vec<HarnessSkillDraftRecord>, (StatusCode, Json<ErrorResponse>)> {
        let rows = sqlx::query(
            "SELECT skill_draft_id, harness_run_id, candidate_name, description,
                    target_scope, publish_visibility, content_markdown,
                    IFNULL(source_summary_json, '{}') AS source_summary_json,
                    IFNULL(decision_history_json, '[]') AS decision_history_json,
                    status, confidence, created_by_node_id, revision, published_version_id,
                    CAST(created_at AS CHAR) AS created_at,
                    CAST(updated_at AS CHAR) AS updated_at
             FROM harness_skill_drafts
             WHERE harness_run_id = ?
             ORDER BY created_at ASC",
        )
        .bind(harness_run_id)
        .fetch_all(self.pool.get())
        .await
        .map_err(internal_error)?;

        let mut drafts = Vec::with_capacity(rows.len());
        for row in rows {
            drafts.push(skill_draft_from_row(row)?);
        }
        if drafts.is_empty() {
            return Ok(drafts);
        }

        let rule_rows = sqlx::query(
            "SELECT skill_rule_id, skill_draft_id, harness_run_id, rule_type, statement,
                    rationale, IFNULL(decision_history_json, '[]') AS decision_history_json,
                    status, confidence, source_count, created_by_node_id,
                    CAST(created_at AS CHAR) AS created_at,
                    CAST(updated_at AS CHAR) AS updated_at
             FROM harness_skill_rules
             WHERE harness_run_id = ?
             ORDER BY created_at ASC",
        )
        .bind(harness_run_id)
        .fetch_all(self.pool.get())
        .await
        .map_err(internal_error)?;
        let mut rules_by_draft: HashMap<String, Vec<HarnessSkillRuleRecord>> = HashMap::new();
        for row in rule_rows {
            let rule = skill_rule_from_row(row)?;
            rules_by_draft
                .entry(rule.skill_draft_id.clone())
                .or_default()
                .push(rule);
        }

        let citation_rows = sqlx::query(
            "SELECT citation_id, harness_run_id, item_id, skill_draft_id, skill_rule_id,
                    source_id, IFNULL(source_locator_json, '{}') AS source_locator_json,
                    source_snapshot_ref, source_content_hash,
                    IFNULL(source_metadata_json, '{}') AS source_metadata_json,
                    artifact_id, quote_hash, evidence_text_preview, relevance_score,
                    created_by_node_id, CAST(created_at AS CHAR) AS created_at
             FROM harness_citations
             WHERE harness_run_id = ? AND skill_rule_id IS NOT NULL
             ORDER BY created_at ASC",
        )
        .bind(harness_run_id)
        .fetch_all(self.pool.get())
        .await
        .map_err(internal_error)?;
        let mut citations_by_rule: HashMap<String, Vec<HarnessCitationRecord>> = HashMap::new();
        for row in citation_rows {
            let citation = citation_from_row(row)?;
            if let Some(skill_rule_id) = citation.skill_rule_id.clone() {
                citations_by_rule
                    .entry(skill_rule_id)
                    .or_default()
                    .push(citation);
            }
        }

        for draft in &mut drafts {
            let mut rules = rules_by_draft
                .remove(&draft.skill_draft_id)
                .unwrap_or_default();
            for rule in &mut rules {
                rule.citations = citations_by_rule
                    .remove(&rule.skill_rule_id)
                    .unwrap_or_default();
            }
            draft.rules = rules;
        }
        Ok(drafts)
    }

    async fn load_skill_rules(
        &self,
        harness_run_id: &str,
        skill_draft_id: &str,
    ) -> Result<Vec<HarnessSkillRuleRecord>, (StatusCode, Json<ErrorResponse>)> {
        let rows = sqlx::query(
            "SELECT skill_rule_id, skill_draft_id, harness_run_id, rule_type, statement,
                    rationale, IFNULL(decision_history_json, '[]') AS decision_history_json,
                    status, confidence, source_count, created_by_node_id,
                    CAST(created_at AS CHAR) AS created_at,
                    CAST(updated_at AS CHAR) AS updated_at
             FROM harness_skill_rules
             WHERE harness_run_id = ? AND skill_draft_id = ?
             ORDER BY created_at ASC",
        )
        .bind(harness_run_id)
        .bind(skill_draft_id)
        .fetch_all(self.pool.get())
        .await
        .map_err(internal_error)?;
        let mut rules = rows
            .into_iter()
            .map(skill_rule_from_row)
            .collect::<HarnessResult<Vec<_>>>()?;
        for rule in &mut rules {
            rule.citations = self
                .load_skill_rule_citations(harness_run_id, &rule.skill_rule_id)
                .await?;
        }
        Ok(rules)
    }

    async fn load_skill_rule_citations(
        &self,
        harness_run_id: &str,
        skill_rule_id: &str,
    ) -> Result<Vec<HarnessCitationRecord>, (StatusCode, Json<ErrorResponse>)> {
        let rows = sqlx::query(
            "SELECT citation_id, harness_run_id, item_id, skill_draft_id, skill_rule_id,
                    source_id, IFNULL(source_locator_json, '{}') AS source_locator_json,
                    source_snapshot_ref, source_content_hash,
                    IFNULL(source_metadata_json, '{}') AS source_metadata_json,
                    artifact_id, quote_hash, evidence_text_preview, relevance_score,
                    created_by_node_id, CAST(created_at AS CHAR) AS created_at
             FROM harness_citations
             WHERE harness_run_id = ? AND skill_rule_id = ?
             ORDER BY created_at ASC",
        )
        .bind(harness_run_id)
        .bind(skill_rule_id)
        .fetch_all(self.pool.get())
        .await
        .map_err(internal_error)?;
        rows.into_iter().map(citation_from_row).collect()
    }

    async fn load_item_locked(
        &self,
        tx: &mut sqlx::Transaction<'_, MySql>,
        harness_run_id: &str,
        item_id: &str,
    ) -> Result<HarnessItemRecord, (StatusCode, Json<ErrorResponse>)> {
        let row = sqlx::query(
            "SELECT item_id, harness_run_id, item_type,
                    IFNULL(locator_json, '{}') AS locator_json,
                    IFNULL(input_json, '{}') AS input_json,
                    IFNULL(proposed_output_json, '{}') AS proposed_output_json,
                    IFNULL(final_output_json, '{}') AS final_output_json,
                    IFNULL(decision_history_json, '[]') AS decision_history_json,
                    status, confidence, assigned_to,
                    CAST(created_at AS CHAR) AS created_at,
                    CAST(updated_at AS CHAR) AS updated_at
             FROM harness_items
             WHERE harness_run_id = ? AND item_id = ?
             LIMIT 1 FOR UPDATE",
        )
        .bind(harness_run_id)
        .bind(item_id)
        .fetch_optional(&mut **tx)
        .await
        .map_err(internal_error)?;
        let row =
            row.ok_or_else(|| error_response(StatusCode::NOT_FOUND, "harness item not found"))?;
        item_from_row(row)
    }

    async fn load_skill_rules_locked(
        &self,
        tx: &mut sqlx::Transaction<'_, MySql>,
        harness_run_id: &str,
        skill_draft_id: &str,
    ) -> Result<Vec<HarnessSkillRuleRecord>, (StatusCode, Json<ErrorResponse>)> {
        let rows = sqlx::query(
            "SELECT skill_rule_id, skill_draft_id, harness_run_id, rule_type, statement,
                    rationale, IFNULL(decision_history_json, '[]') AS decision_history_json,
                    status, confidence, source_count, created_by_node_id,
                    CAST(created_at AS CHAR) AS created_at,
                    CAST(updated_at AS CHAR) AS updated_at
             FROM harness_skill_rules
             WHERE harness_run_id = ? AND skill_draft_id = ?
             ORDER BY created_at ASC",
        )
        .bind(harness_run_id)
        .bind(skill_draft_id)
        .fetch_all(&mut **tx)
        .await
        .map_err(internal_error)?;
        rows.into_iter().map(skill_rule_from_row).collect()
    }

    async fn load_skill_draft_locked(
        &self,
        tx: &mut sqlx::Transaction<'_, MySql>,
        harness_run_id: &str,
        skill_draft_id: &str,
    ) -> Result<HarnessSkillDraftRecord, (StatusCode, Json<ErrorResponse>)> {
        let row = sqlx::query(
            "SELECT skill_draft_id, harness_run_id, candidate_name, description,
                    target_scope, publish_visibility, content_markdown,
                    IFNULL(source_summary_json, '{}') AS source_summary_json,
                    IFNULL(decision_history_json, '[]') AS decision_history_json,
                    status, confidence, created_by_node_id, revision, published_version_id,
                    CAST(created_at AS CHAR) AS created_at,
                    CAST(updated_at AS CHAR) AS updated_at
             FROM harness_skill_drafts
             WHERE harness_run_id = ? AND skill_draft_id = ?
             LIMIT 1 FOR UPDATE",
        )
        .bind(harness_run_id)
        .bind(skill_draft_id)
        .fetch_optional(&mut **tx)
        .await
        .map_err(internal_error)?;
        let row =
            row.ok_or_else(|| error_response(StatusCode::NOT_FOUND, "skill draft not found"))?;
        let mut draft = skill_draft_from_row(row)?;
        draft.rules = self
            .load_skill_rules_locked(tx, harness_run_id, skill_draft_id)
            .await?;
        Ok(draft)
    }

    async fn validate_session_ownership(
        &self,
        user_id: &str,
        session_ids: &[String],
    ) -> Result<(), (StatusCode, Json<ErrorResponse>)> {
        if session_ids.is_empty() {
            return Ok(());
        }
        if session_ids.len() > MAX_SKILLIFY_SESSIONS {
            return Err(error_response(
                StatusCode::BAD_REQUEST,
                format!("skillify supports at most {MAX_SKILLIFY_SESSIONS} sessions per run"),
            ));
        }
        let mut builder =
            QueryBuilder::<MySql>::new("SELECT session_id FROM agent_sessions WHERE user_id = ");
        builder.push_bind(user_id);
        builder.push(" AND session_id IN (");
        let mut separated = builder.separated(", ");
        for session_id in session_ids {
            separated.push_bind(session_id);
        }
        separated.push_unseparated(")");
        let rows = builder
            .build()
            .fetch_all(self.pool.get())
            .await
            .map_err(internal_error)?;
        if rows.len() != session_ids.len() {
            return Err(error_response(
                StatusCode::NOT_FOUND,
                "one or more selected sessions were not found",
            ));
        }
        Ok(())
    }

    async fn load_skillify_events(
        &self,
        user_id: &str,
        session_ids: &[String],
    ) -> Result<(Vec<SkillifyEvent>, bool), (StatusCode, Json<ErrorResponse>)> {
        let mut builder = QueryBuilder::<MySql>::new(
            "SELECT event_id, session_id, event_type, content
             FROM agent_events
             WHERE user_id = ",
        );
        builder.push_bind(user_id);
        builder.push(
            " AND event_type <> 'evaluation_case' AND content IS NOT NULL AND content != '' AND session_id IN (",
        );
        let mut separated = builder.separated(", ");
        for session_id in session_ids {
            separated.push_bind(session_id);
        }
        separated.push_unseparated(")");
        builder.push(" ORDER BY created_at DESC, event_id DESC LIMIT ");
        builder.push_bind(MAX_SKILLIFY_EVENTS + 1);

        let mut rows = builder
            .build()
            .fetch_all(self.pool.get())
            .await
            .map_err(internal_error)?;

        let truncated = rows.len() > MAX_SKILLIFY_EVENTS as usize;
        rows.truncate(MAX_SKILLIFY_EVENTS as usize);
        rows.reverse();
        let events = rows
            .into_iter()
            .map(|row| {
                let event_id = required_harness_string(&row, "agent_events", "event_id")?;
                let session_id = required_harness_string(&row, "agent_events", "session_id")?;
                let event_type = required_harness_string(&row, "agent_events", "event_type")?;
                let content = required_harness_string(&row, "agent_events", "content")?;
                Ok(SkillifyEvent {
                    source_id: event_id.clone(),
                    source_type: "session_event".to_string(),
                    title: skillify_session_event_title(&session_id, &event_type),
                    event_id,
                    session_id,
                    event_type,
                    content,
                })
            })
            .collect::<HarnessResult<Vec<_>>>()?;
        Ok((events, truncated))
    }

    fn normalize_source_files(
        &self,
        files: Option<Vec<SkillifySourceFile>>,
    ) -> Result<Vec<SkillifyEvent>, (StatusCode, Json<ErrorResponse>)> {
        let files = files.unwrap_or_default();
        if files.len() > MAX_SKILLIFY_SOURCE_FILES {
            return Err(error_response(
                StatusCode::BAD_REQUEST,
                format!(
                    "skillify supports at most {MAX_SKILLIFY_SOURCE_FILES} source files per run"
                ),
            ));
        }
        let mut out = Vec::new();
        for (index, file) in files.into_iter().enumerate() {
            let file_name = file.file_name.trim();
            let content = file.content.trim();
            if file_name.is_empty() || content.is_empty() {
                continue;
            }
            if content.chars().count() > MAX_SKILLIFY_SOURCE_FILE_CHARS {
                return Err(error_response(
                    StatusCode::BAD_REQUEST,
                    format!(
                        "source file {file_name} exceeds the {MAX_SKILLIFY_SOURCE_FILE_CHARS} character limit"
                    ),
                ));
            }
            let source_id = format!("uploaded-file-{index}-{}", stable_hash(file_name));
            out.push(SkillifyEvent {
                event_id: source_id.clone(),
                session_id: String::new(),
                source_id,
                source_type: "upload".to_string(),
                title: file_name.to_string(),
                event_type: file.mime_type.unwrap_or_else(|| "text/plain".to_string()),
                content: content.to_string(),
            });
        }
        Ok(out)
    }

    async fn record_skillify_run_failure(
        &self,
        user_id: &str,
        harness_run_id: &str,
        mut failure: (StatusCode, Json<ErrorResponse>),
    ) -> (StatusCode, Json<ErrorResponse>) {
        let persisted_error = failure.1.detail.chars().take(4_000).collect::<String>();
        let update = sqlx::query(
            "UPDATE harness_runs
             SET status = 'failed', error = ?, updated_at = NOW(6)
             WHERE user_id = ? AND harness_run_id = ? AND status = 'running'",
        )
        .bind(persisted_error)
        .bind(user_id)
        .bind(harness_run_id)
        .execute(self.pool.get())
        .await;

        match update {
            Ok(result) if result.rows_affected() == 1 => {
                failure.1.metadata = Some(json!({
                    "harness_run_id": harness_run_id,
                    "status": "failed"
                }));
                failure
            }
            Ok(_) => (
                StatusCode::CONFLICT,
                Json(
                    ErrorResponse::new(
                        "Skillify run failed, but its durable state was no longer running",
                    )
                    .with_error_code("harness_terminal_conflict")
                    .with_metadata(json!({
                        "harness_run_id": harness_run_id,
                        "status": "unknown"
                    })),
                ),
            ),
            Err(error) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(
                    ErrorResponse::new(format!(
                        "Skillify run failed and its terminal state could not be persisted: {error}"
                    ))
                    .with_error_code("harness_terminal_persistence_failed")
                    .with_metadata(json!({
                        "harness_run_id": harness_run_id,
                        "status": "unknown"
                    })),
                ),
            ),
        }
    }
}

impl DatabaseHarnessService {
    async fn load_authoring_evaluation_case(
        &self,
        user_id: &str,
        session_id: &str,
    ) -> HarnessResult<Option<EvaluationPrepareCase>> {
        if session_id.trim().is_empty() {
            return Ok(None);
        }
        let rows = sqlx::query(
            "SELECT content FROM agent_events
             WHERE user_id = ? AND session_id = ? AND event_type = 'evaluation_case'
               AND JSON_UNQUOTE(JSON_EXTRACT(metadata, '$.astra_ingestion_source')) = 'server'
               AND content IS NOT NULL AND content != ''
             ORDER BY created_at ASC, event_id ASC LIMIT 2",
        )
        .bind(user_id)
        .bind(session_id)
        .fetch_all(self.pool.get())
        .await
        .map_err(internal_error)?;
        if rows.len() > 1 {
            return Ok(None);
        }
        let Some(row) = rows.into_iter().next() else {
            return Ok(None);
        };
        let Ok(content) = row.try_get::<String, _>("content") else {
            return Ok(None);
        };
        Ok(serde_json::from_str(&content).ok())
    }

    async fn materialize_authoring_candidate(
        &self,
        user_id: &str,
        harness_run: &HarnessRunRecord,
        draft: &HarnessSkillDraftRecord,
    ) -> HarnessResult<String> {
        validate_skill_name(&draft.candidate_name)?;
        let candidate_identity = stable_hash(&format!(
            "authoring-candidate:v2:{}:{}:{}:{}:{}",
            harness_run.harness_run_id,
            draft.skill_draft_id,
            draft.candidate_name,
            draft.description,
            draft.content_markdown,
        ));
        let candidate_identity = candidate_identity
            .strip_prefix("sha256:")
            .unwrap_or(&candidate_identity);
        // user_skill_versions.version is VARCHAR(64). Keep the readable
        // authoring prefix and enough content identity for concurrent retries
        // to converge without exceeding the product schema.
        let version = format!(
            "evaluation.{}",
            &candidate_identity[..candidate_identity.len().min(48)]
        );
        let manifest = json!({
            "name": draft.candidate_name,
            "description": draft.description,
            "version": &version,
        });
        let expected_content_hash =
            crate::personal_skills::skill_md_content_hash(&manifest, &draft.content_markdown);
        let matches_current_draft = |candidate: &crate::personal_skills::UserSkillVersionRecord| {
            candidate.status == "draft"
                && candidate.version == version
                && candidate.manifest_json == manifest
                && candidate.content_markdown == draft.content_markdown
                && candidate.content_hash == expected_content_hash
                && candidate.content_hash
                    == crate::personal_skills::skill_md_content_hash(
                        &candidate.manifest_json,
                        &candidate.content_markdown,
                    )
        };

        let store = DatabasePersonalSkillStore::new(self.pool.clone());
        if let Some(version_id) = harness_run
            .output_json
            .pointer("/authoring/candidate_revision_id")
            .and_then(Value::as_str)
            && let Some(version) = store
                .load_version(user_id, &draft.candidate_name, version_id)
                .await
                .map_err(|error| {
                    error_response(StatusCode::SERVICE_UNAVAILABLE, error.to_string())
                })?
            && matches_current_draft(&version)
        {
            return Ok(version.version_id);
        }

        if let Some(version_record) = store
            .load_version_by_version(user_id, &draft.candidate_name, &version)
            .await
            .map_err(|error| error_response(StatusCode::SERVICE_UNAVAILABLE, error.to_string()))?
        {
            if !matches_current_draft(&version_record) {
                return Err(error_response(
                    StatusCode::CONFLICT,
                    format!(
                        "authoring candidate version identity conflicts with existing content: {}",
                        version_record.version_id
                    ),
                ));
            }
            return Ok(version_record.version_id);
        }

        store
            .ensure_source(
                user_id,
                CreateUserSkillSource {
                    skill_name: draft.candidate_name.clone(),
                    visibility: Some("private".to_string()),
                },
            )
            .await
            .map_err(|error| error_response(StatusCode::SERVICE_UNAVAILABLE, error.to_string()))?;
        match store
            .submit_version(
                user_id,
                &draft.candidate_name,
                SubmitUserSkillVersion {
                    version: version.clone(),
                    manifest_json: manifest.clone(),
                    content_markdown: draft.content_markdown.clone(),
                    status: Some("draft".to_string()),
                },
            )
            .await
        {
            Ok(version_record) => Ok(version_record.version_id),
            Err(error) => {
                // A concurrent retry may have won the unique source/version
                // insert. Re-read the immutable identity before surfacing the
                // write failure so the durable candidate reference converges.
                if let Some(version_record) = store
                    .load_version_by_version(user_id, &draft.candidate_name, &version)
                    .await
                    .map_err(|load_error| {
                        error_response(StatusCode::SERVICE_UNAVAILABLE, load_error.to_string())
                    })?
                    && matches_current_draft(&version_record)
                {
                    return Ok(version_record.version_id);
                }
                Err(error_response(
                    StatusCode::SERVICE_UNAVAILABLE,
                    error.to_string(),
                ))
            }
        }
    }

    async fn resolve_authoring_evaluation_input(
        &self,
        user_id: &str,
        session_id: &str,
        operation: &str,
        harness_run: &HarnessRunRecord,
        drafts: &[HarnessSkillDraftRecord],
        validation_task: Option<&AuthoringValidationTask>,
    ) -> HarnessResult<Option<AuthoringEvaluationInput>> {
        let case = if let Some(task) = validation_task {
            let source = harness_run.input_json["source_packets"]
                .as_array()
                .and_then(|sources| {
                    sources.iter().find(|source| {
                        source["source_id"].as_str() == Some(task.source_id.as_str())
                            && source["event_type"].as_str() == Some("user_query")
                    })
                })
                .ok_or_else(|| {
                    error_response(
                        StatusCode::BAD_REQUEST,
                        "select an original user task from the frozen sources",
                    )
                })?;
            EvaluationPrepareCase {
                case_id: task.source_id.clone(),
                message: source["content"].as_str().unwrap_or_default().to_string(),
                holdout: false,
                verifier_config:
                    crate::evaluation::task_verifier::TaskVerifierConfig::JsonValueEquals {
                        expected: task.expected_result.clone(),
                    },
            }
        } else {
            let Some(case) = self
                .load_authoring_evaluation_case(user_id, session_id)
                .await?
            else {
                return Ok(None);
            };
            case
        };
        if drafts.len() != 1 {
            return Ok(None);
        }
        let draft = &drafts[0];
        let Some(candidate_revision_id) = harness_run
            .output_json
            .pointer("/authoring/candidate_revision_id")
            .and_then(Value::as_str)
            .filter(|value| !value.trim().is_empty())
            .map(str::to_string)
        else {
            return Ok(None);
        };
        let (skill_name, baseline) = match operation {
            "create" => (
                draft.candidate_name.clone(),
                EvaluationPrepareRevision {
                    revision_id: NO_SKILL_REVISION_ID.to_string(),
                    content: None,
                },
            ),
            "improve" => {
                let baseline = &harness_run.output_json["authoring"]["baseline"];
                let Some(version_id) = baseline["version_id"].as_str() else {
                    return Ok(None);
                };
                (
                    draft.candidate_name.clone(),
                    EvaluationPrepareRevision {
                        revision_id: version_id.to_string(),
                        content: None,
                    },
                )
            }
            _ => return Ok(None),
        };
        Ok(Some(AuthoringEvaluationInput {
            submission_idempotency_key: format!(
                "authoring-evaluation:v1:{}",
                stable_hash(
                    &json!([
                        harness_run.harness_run_id,
                        draft.skill_draft_id,
                        candidate_revision_id
                    ])
                    .to_string()
                )
            ),
            target: EvaluationPrepareTarget {
                kind: EvaluationTargetKind::Skill,
                baseline,
                candidate: EvaluationPrepareRevision {
                    revision_id: candidate_revision_id,
                    content: None,
                },
                skill_name: Some(skill_name),
            },
            case,
        }))
    }

    async fn load_authoring_inference_evidence(
        &self,
        user_id: &str,
        harness_run_id: &str,
    ) -> HarnessResult<AuthoringInferenceEvidence> {
        const MAX_FACTS: i64 = 32_768;
        let invocations = sqlx::query(
            "SELECT i.invocation_id, i.operation_id, i.status, i.usage_status,
                    i.provider_delivery_state,
                    r.offering_id, r.resolved_model_name AS model_name,
                    r.provider, CAST(r.pricing_json AS CHAR) AS pricing_json
             FROM inference_invocations AS i
             LEFT JOIN inference_routes AS r
               ON r.user_id = i.user_id AND r.route_id = i.route_id
             WHERE i.user_id = ? AND i.harness_run_id = ?
             ORDER BY i.created_at ASC, i.invocation_id ASC
             LIMIT ?",
        )
        .bind(user_id)
        .bind(harness_run_id)
        .bind(MAX_FACTS + 1)
        .fetch_all(self.pool.get())
        .await
        .map_err(internal_error)?;
        let attempts = sqlx::query(
            "SELECT a.attempt_id, a.invocation_id, a.provider, a.status,
                    a.usage_status, a.input_tokens, a.output_tokens,
                    a.cache_read_tokens, a.cache_creation_tokens,
                    r.offering_id, r.resolved_model_name AS model_name,
                    r.provider AS route_provider,
                    CAST(r.pricing_json AS CHAR) AS pricing_json
             FROM inference_provider_attempts AS a
             INNER JOIN inference_invocations AS i
               ON i.user_id = a.user_id AND i.invocation_id = a.invocation_id
             LEFT JOIN inference_routes AS r
               ON r.user_id = i.user_id AND r.route_id = i.route_id
             WHERE a.user_id = ? AND a.harness_run_id = ?
             ORDER BY a.invocation_id ASC, a.attempt_index ASC, a.attempt_id ASC
             LIMIT ?",
        )
        .bind(user_id)
        .bind(harness_run_id)
        .bind(MAX_FACTS + 1)
        .fetch_all(self.pool.get())
        .await
        .map_err(internal_error)?;

        let mut providers = BTreeSet::new();
        let mut models = BTreeSet::new();
        let mut offering_ids = BTreeSet::new();
        let mut operations = BTreeSet::new();
        let mut invocation_ids = HashSet::new();
        let mut attempt_counts = HashMap::<String, usize>::new();
        let mut fingerprint_parts = vec![
            "authoring-inference-v1".to_string(),
            user_id.to_string(),
            harness_run_id.to_string(),
        ];
        let invocations_truncated = invocations.len() > MAX_FACTS as usize;
        let attempts_truncated = attempts.len() > MAX_FACTS as usize;
        let mut settlement_pending = invocations_truncated || attempts_truncated;
        let mut topology_complete = !invocations.is_empty() && !settlement_pending;
        let mut usage_complete = !invocations.is_empty() && !settlement_pending;
        let mut pricing_complete = !invocations.is_empty() && !settlement_pending;
        let mut completeness_reasons = Vec::new();
        let mut seen_reasons = HashSet::new();
        let mut prompt_tokens = 0_u64;
        let mut completion_tokens = 0_u64;
        let mut cache_read_tokens = 0_u64;
        let mut cache_creation_tokens = 0_u64;
        let mut exact_usage_attempt_count = 0_usize;
        let mut priced_attempt_count = 0_usize;
        let mut estimated_cost_usd = 0_f64;

        let add_reason = |reasons: &mut Vec<String>, seen: &mut HashSet<String>, reason: String| {
            if seen.insert(reason.clone()) {
                reasons.push(reason);
            }
        };
        let non_negative = |value: i64, field: &str| -> HarnessResult<u64> {
            u64::try_from(value).map_err(|_| {
                error_response(
                    StatusCode::CONFLICT,
                    format!("authoring inference evidence contains a negative {field}"),
                )
            })
        };
        let add_total = |total: &mut u64, value: u64| {
            if let Some(next) = total.checked_add(value) {
                *total = next;
                true
            } else {
                false
            }
        };

        for row in invocations.iter().take(MAX_FACTS as usize) {
            let invocation_id: String = row.try_get("invocation_id").map_err(internal_error)?;
            let operation_id: String = row.try_get("operation_id").map_err(internal_error)?;
            let status: String = row.try_get("status").map_err(internal_error)?;
            let usage_status: String = row.try_get("usage_status").map_err(internal_error)?;
            let delivery_state: String = row
                .try_get("provider_delivery_state")
                .map_err(internal_error)?;
            let provider: Option<String> = row.try_get("provider").map_err(internal_error)?;
            let model: Option<String> = row.try_get("model_name").map_err(internal_error)?;
            let offering_id: Option<String> = row.try_get("offering_id").map_err(internal_error)?;
            invocation_ids.insert(invocation_id.clone());
            operations.insert(operation_id.clone());
            if let Some(value) = provider.as_deref() {
                providers.insert(value.to_string());
            }
            if let Some(value) = model.as_deref() {
                models.insert(value.to_string());
            }
            if let Some(value) = offering_id.as_deref() {
                offering_ids.insert(value.to_string());
            }
            if status == "admitted" || delivery_state == "unknown" {
                settlement_pending = true;
                topology_complete = false;
                add_reason(
                    &mut completeness_reasons,
                    &mut seen_reasons,
                    format!("invocation_not_terminal:{invocation_id}"),
                );
            }
            if provider.is_none() || model.is_none() || offering_id.is_none() {
                topology_complete = false;
                add_reason(
                    &mut completeness_reasons,
                    &mut seen_reasons,
                    format!("route_missing:{invocation_id}"),
                );
            }
            if status != "succeeded" {
                usage_complete = false;
                add_reason(
                    &mut completeness_reasons,
                    &mut seen_reasons,
                    format!("invocation_status:{status}:{invocation_id}"),
                );
            }
            fingerprint_parts.push(format!(
                "invocation:{invocation_id}:{operation_id}:{status}:{usage_status}:{delivery_state}:{}:{}:{}",
                offering_id.as_deref().unwrap_or(""),
                model.as_deref().unwrap_or(""),
                provider.as_deref().unwrap_or(""),
            ));
        }

        let mut attempts_by_invocation = HashSet::new();
        for row in attempts.iter().take(MAX_FACTS as usize) {
            let attempt_id: String = row.try_get("attempt_id").map_err(internal_error)?;
            let invocation_id: String = row.try_get("invocation_id").map_err(internal_error)?;
            let provider: String = row.try_get("provider").map_err(internal_error)?;
            let status: String = row.try_get("status").map_err(internal_error)?;
            let usage_status: String = row.try_get("usage_status").map_err(internal_error)?;
            let route_provider: Option<String> =
                row.try_get("route_provider").map_err(internal_error)?;
            let input_tokens = non_negative(
                row.try_get("input_tokens").map_err(internal_error)?,
                "input_tokens",
            )?;
            let output_tokens = non_negative(
                row.try_get("output_tokens").map_err(internal_error)?,
                "output_tokens",
            )?;
            let cache_read = non_negative(
                row.try_get("cache_read_tokens").map_err(internal_error)?,
                "cache_read_tokens",
            )?;
            let cache_creation = non_negative(
                row.try_get("cache_creation_tokens")
                    .map_err(internal_error)?,
                "cache_creation_tokens",
            )?;
            let pricing_json: Option<String> =
                row.try_get("pricing_json").map_err(internal_error)?;
            attempts_by_invocation.insert(invocation_id.clone());
            *attempt_counts.entry(invocation_id.clone()).or_default() += 1;
            providers.insert(provider.clone());
            if let Some(route_provider) = route_provider.as_deref()
                && provider != route_provider
            {
                pricing_complete = false;
                add_reason(
                    &mut completeness_reasons,
                    &mut seen_reasons,
                    format!("provider_fallback_pricing_unbound:{attempt_id}"),
                );
            }
            if status == "started" || status == "delivery_unknown" {
                settlement_pending = true;
                topology_complete = false;
                add_reason(
                    &mut completeness_reasons,
                    &mut seen_reasons,
                    format!("attempt_not_terminal:{attempt_id}"),
                );
            }
            if usage_status == "provider_exact" {
                exact_usage_attempt_count = exact_usage_attempt_count.saturating_add(1);
                let totals_ok = add_total(&mut prompt_tokens, input_tokens)
                    && add_total(&mut completion_tokens, output_tokens)
                    && add_total(&mut cache_read_tokens, cache_read)
                    && add_total(&mut cache_creation_tokens, cache_creation);
                if !totals_ok {
                    usage_complete = false;
                    add_reason(
                        &mut completeness_reasons,
                        &mut seen_reasons,
                        "inference_token_total_overflow".to_string(),
                    );
                }
            } else {
                usage_complete = false;
                add_reason(
                    &mut completeness_reasons,
                    &mut seen_reasons,
                    format!("usage_{usage_status}:{attempt_id}"),
                );
            }
            let pricing = pricing_json
                .as_deref()
                .map(parse_pricing_snapshot)
                .transpose()
                .map_err(|error| {
                    error_response(
                        StatusCode::CONFLICT,
                        format!(
                            "invalid pricing snapshot for authoring attempt {attempt_id}: {error}"
                        ),
                    )
                })?
                .flatten();
            let cost = (route_provider.as_deref() == Some(provider.as_str()))
                .then(|| {
                    pricing.as_ref()?.estimated_cost_usd(
                        input_tokens,
                        output_tokens,
                        cache_read,
                        cache_creation,
                    )
                })
                .flatten();
            if let Some(cost) = cost {
                priced_attempt_count = priced_attempt_count.saturating_add(1);
                estimated_cost_usd += cost;
                if !estimated_cost_usd.is_finite() {
                    pricing_complete = false;
                    add_reason(
                        &mut completeness_reasons,
                        &mut seen_reasons,
                        "inference_cost_overflow".to_string(),
                    );
                }
            } else {
                pricing_complete = false;
                add_reason(
                    &mut completeness_reasons,
                    &mut seen_reasons,
                    format!("pricing_unavailable:{attempt_id}"),
                );
            }
            fingerprint_parts.push(format!(
                "attempt:{attempt_id}:{invocation_id}:{provider}:{status}:{usage_status}:{input_tokens}:{output_tokens}:{cache_read}:{cache_creation}",
            ));
        }

        if invocations.is_empty() {
            add_reason(
                &mut completeness_reasons,
                &mut seen_reasons,
                "inference_ledger_empty".to_string(),
            );
        }
        if attempts.is_empty() {
            topology_complete = false;
            usage_complete = false;
            pricing_complete = false;
            add_reason(
                &mut completeness_reasons,
                &mut seen_reasons,
                "inference_attempts_empty".to_string(),
            );
        }
        for invocation_id in &invocation_ids {
            if !attempts_by_invocation.contains(invocation_id) {
                topology_complete = false;
                usage_complete = false;
                pricing_complete = false;
                add_reason(
                    &mut completeness_reasons,
                    &mut seen_reasons,
                    format!("terminal_attempt_missing:{invocation_id}"),
                );
            }
        }
        if priced_attempt_count != attempt_counts.values().sum::<usize>() {
            pricing_complete = false;
        }
        if exact_usage_attempt_count != attempt_counts.values().sum::<usize>() {
            usage_complete = false;
        }
        let physical_attempt_count = attempt_counts.values().sum::<usize>();
        let invocation_count = invocation_ids.len();
        let complete = topology_complete
            && usage_complete
            && pricing_complete
            && !settlement_pending
            && physical_attempt_count > 0;
        let usage_status = if complete
            || (physical_attempt_count > 0
                && exact_usage_attempt_count == physical_attempt_count
                && !settlement_pending)
        {
            "provider_exact"
        } else if exact_usage_attempt_count > 0 {
            "provider_partial"
        } else {
            "unavailable"
        };
        let evidence_fingerprint = stable_hash(&fingerprint_parts.join("\n"));
        Ok(AuthoringInferenceEvidence {
            schema_version: 1,
            invocation_count,
            physical_attempt_count,
            priced_attempt_count,
            exact_usage_attempt_count,
            complete,
            settlement_pending,
            usage_status: usage_status.to_string(),
            providers: providers.into_iter().collect(),
            models: models.into_iter().collect(),
            offering_ids: offering_ids.into_iter().collect(),
            operations: operations.into_iter().collect(),
            prompt_tokens: (usage_complete && topology_complete && !settlement_pending)
                .then_some(prompt_tokens),
            completion_tokens: (usage_complete && topology_complete && !settlement_pending)
                .then_some(completion_tokens),
            cache_read_tokens: (usage_complete && topology_complete && !settlement_pending)
                .then_some(cache_read_tokens),
            cache_creation_tokens: (usage_complete && topology_complete && !settlement_pending)
                .then_some(cache_creation_tokens),
            estimated_cost_usd: complete.then_some(estimated_cost_usd),
            completeness_reasons,
            evidence_fingerprint,
        })
    }

    async fn create_skillify_run_internal(
        &self,
        user_id: String,
        request: SkillifyRunRequest,
        authoring: Option<&AuthoringGenerationContext>,
        cancel_token: Option<CancellationToken>,
    ) -> Result<HarnessRunRecord, (StatusCode, Json<ErrorResponse>)> {
        let session_ids = normalize_session_ids(request.session_ids);
        let source_files = self.normalize_source_files(request.source_files.clone())?;
        let harness_run_id = skillify_harness_run_id(&user_id, request.idempotency_key.as_deref());
        if request.idempotency_key.is_some() {
            match self.ensure_run_owner(&user_id, &harness_run_id).await {
                Ok(existing) => return Ok(existing),
                Err((status, _)) if status == StatusCode::NOT_FOUND => {}
                Err(error) => return Err(error),
            }
        }
        if session_ids.is_empty() && source_files.is_empty() {
            return Err(error_response(
                StatusCode::BAD_REQUEST,
                "select at least one session or source file",
            ));
        }
        self.validate_session_ownership(&user_id, &session_ids)
            .await?;
        let (mut events, truncated) = if session_ids.is_empty() {
            (Vec::new(), false)
        } else {
            self.load_skillify_events(&user_id, &session_ids).await?
        };
        let source_coverage = json!({
            "selection": "most_recent",
            "event_limit": MAX_SKILLIFY_EVENTS,
            "selected_event_count": events.len(),
            "older_events_omitted": truncated,
            "first_event_id": events.first().map(|event| &event.event_id),
            "last_event_id": events.last().map(|event| &event.event_id),
        });
        events.extend(source_files);
        if events.is_empty() {
            return Err(error_response(
                StatusCode::BAD_REQUEST,
                "selected sources do not contain readable content",
            ));
        }

        if let Some(baseline) = authoring.and_then(|context| context.baseline.as_ref()) {
            events.push(SkillifyEvent {
                event_id: baseline.version_id.clone(),
                session_id: String::new(),
                source_id: format!("skill-version:{}", baseline.version_id),
                source_type: "skill_version".into(),
                title: baseline.skill_name.clone(),
                event_type: "skill_baseline".into(),
                content: baseline.content_markdown.clone(),
            });
        }
        let target_scope = request
            .target_scope
            .clone()
            .unwrap_or_else(|| "personal".to_string());
        validate_skillify_target_scope(&target_scope)?;
        let executor = self.skillify_agent_executor.as_ref().ok_or_else(|| {
            error_response(
                StatusCode::SERVICE_UNAVAILABLE,
                "Skillify agent executor is not configured",
            )
        })?;
        let source_packets = events
            .iter()
            .map(skillify_source_packet_from_event)
            .collect::<Vec<_>>();
        let input_json = json!({
            "source_packets": &source_packets,
            "source_coverage": source_coverage,
            "template_id": SKILLIFY_TEMPLATE_ID,
            "session_ids": &session_ids,
            "source_file_count": request.source_files.as_ref().map(Vec::len).unwrap_or(0),
            "skill_name": &request.skill_name,
            "topic": &request.topic,
            "target_scope": &target_scope,
            "idempotency_key": &request.idempotency_key
        });
        let insert_result = sqlx::query(
            "INSERT INTO harness_runs
             (harness_run_id, harness_id, version_id, user_id, session_id, status,
              input_json, output_json, created_at, updated_at)
             VALUES (?, ?, ?, ?, NULL, 'running', ?, '{\"stage\":\"model_execution\"}', NOW(6), NOW(6))",
        )
        .bind(&harness_run_id)
        .bind(SKILLIFY_HARNESS_ID)
        .bind(SKILLIFY_VERSION_ID)
        .bind(&user_id)
        .bind(input_json.to_string())
        .execute(self.pool.get())
        .await;
        match insert_result {
            Ok(_) => {}
            Err(error) if request.idempotency_key.is_some() && is_duplicate_key(&error) => {
                return self.ensure_run_owner(&user_id, &harness_run_id).await;
            }
            Err(error) => return Err(internal_error(error)),
        }

        // Freeze identity and old content before provider I/O. Never infer an
        // improvement from the name invented by the model after generation.
        let (topic, authoring_metadata) = if let Some(context) = authoring {
            let topic = if context.baseline.is_some() {
                "Improve exactly the pinned Skill. Preserve its name and unaffected capabilities. Use its full old body and new evidence; explain every change in the cited rule rationales."
            } else {
                "Generate one reusable Skill from the user's goal and available context."
            };
            let metadata = json!({
                "target": "skill",
                "operation": if context.baseline.is_some() { "improve" } else { "create" },
                "resolution_source": "frozen_before_generation",
                "baseline": context.baseline.as_ref().map(|baseline| json!({
                    "skill_name": baseline.skill_name, "version_id": baseline.version_id,
                    "content_hash": baseline.content_hash, "content_markdown": baseline.content_markdown,
                })),
            });
            let updated = sqlx::query(
                "UPDATE harness_runs SET output_json = ?, updated_at = NOW(6)
                 WHERE user_id = ? AND harness_run_id = ? AND status = 'running'",
            )
            .bind(json!({"stage": "model_execution", "authoring": &metadata}).to_string())
            .bind(&user_id)
            .bind(&harness_run_id)
            .execute(self.pool.get())
            .await
            .map_err(internal_error)?;
            if updated.rows_affected() != 1 {
                return Err(error_response(
                    StatusCode::CONFLICT,
                    "authoring harness run is no longer in running state",
                ));
            }
            (Some(topic.to_string()), Some(metadata))
        } else {
            (request.topic.clone(), None)
        };

        let source_packet_index = source_packets
            .iter()
            .map(|packet| (packet.source_id.clone(), packet.clone()))
            .collect::<HashMap<_, _>>();
        let agent_output = match executor
            .synthesize_skill_drafts(
                SkillifyAgentRequest {
                    user_id: user_id.clone(),
                    harness_run_id: harness_run_id.clone(),
                    skill_name: request.skill_name.clone(),
                    topic: topic.clone(),
                    target_scope: target_scope.clone(),
                    source_packets,
                },
                cancel_token,
            )
            .await
        {
            Ok(output) => output,
            Err(error) => {
                let failure = error_response(
                    StatusCode::BAD_GATEWAY,
                    format!("Skillify agent failed: {error}"),
                );
                return Err(self
                    .record_skillify_run_failure(&user_id, &harness_run_id, failure)
                    .await);
            }
        };
        let validation = validate_skillify_agent_output(&agent_output, &events).and_then(|()| {
            if let Some(baseline) = authoring.and_then(|context| context.baseline.as_ref())
                && (agent_output.drafts.len() != 1
                    || agent_output.drafts[0].candidate_name != baseline.skill_name)
            {
                return Err(invalid_skillify_agent_output(
                    "improvement must preserve the pinned Skill identity",
                ));
            }
            Ok(())
        });
        if let Err(failure) = validation {
            return Err(self
                .record_skillify_run_failure(&user_id, &harness_run_id, failure)
                .await);
        }

        let persistence_result: HarnessResult<()> = async {
        let rule_count: usize = agent_output
            .drafts
            .iter()
            .map(|draft| draft.rules.len())
            .sum();
        let mut output_json = json!({
            "extractor": agent_output.extractor,
            "subagent_strategy": agent_output.subagent_strategy,
            "skill_draft_count": agent_output.drafts.len(),
            "rule_count": rule_count,
            "approved_rule_count": 0,
            "draft_version_id": null
        });
        if let Some(authoring) = authoring_metadata.as_ref() {
            output_json["authoring"] = authoring.clone();
        }
        let status = if agent_output.drafts.is_empty() {
            "completed"
        } else {
            "waiting_for_review"
        };

        let mut tx = self.pool.get().begin().await.map_err(internal_error)?;
        let transitioned = sqlx::query(
            "UPDATE harness_runs
             SET status = ?, output_json = ?, error = NULL, updated_at = NOW(6)
             WHERE user_id = ? AND harness_run_id = ? AND status = 'running'",
        )
        .bind(status)
        .bind(output_json.to_string())
        .bind(&user_id)
        .bind(&harness_run_id)
        .execute(&mut *tx)
        .await
        .map_err(internal_error)?;
        if transitioned.rows_affected() != 1 {
            return Err(error_response(
                StatusCode::CONFLICT,
                "Skillify run is no longer in running state",
            ));
        }

        for draft in &agent_output.drafts {
            let skill_draft_id = format!("harness-skill-draft-{}", Uuid::new_v4());
            sqlx::query(
                "INSERT INTO harness_skill_drafts
                 (skill_draft_id, harness_run_id, candidate_name, description, target_scope,
                  publish_visibility, content_markdown, source_summary_json, status, confidence,
                  created_by_node_id, revision, created_at, updated_at)
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?, 'pending_rule_review', ?, 'agent.synthesize_skill_drafts', 1, NOW(6), NOW(6))",
            )
            .bind(&skill_draft_id)
            .bind(&harness_run_id)
            .bind(&draft.candidate_name)
            .bind(&draft.description)
            .bind(&draft.target_scope)
            .bind(&draft.publish_visibility)
            .bind(&draft.content_markdown)
            .bind(draft.source_summary_json.to_string())
            .bind(draft.confidence)
            .execute(&mut *tx)
            .await
            .map_err(internal_error)?;

            for (index, rule) in draft.rules.iter().enumerate() {
                let skill_rule_id = format!("harness-skill-rule-{}", Uuid::new_v4());
                let review_item_id = skill_rule_review_item_id(&skill_rule_id);
                let source_count = unique_rule_source_count(rule);
                sqlx::query(
                    "INSERT INTO harness_skill_rules
                     (skill_rule_id, skill_draft_id, harness_run_id, rule_type, statement,
                      rationale, status, confidence, source_count, created_by_node_id,
                      created_at, updated_at)
                     VALUES (?, ?, ?, ?, ?, ?, 'proposed', ?, ?, 'agent.extract_skill_signals', NOW(6), NOW(6))",
                )
                .bind(&skill_rule_id)
                .bind(&skill_draft_id)
                .bind(&harness_run_id)
                .bind(&rule.rule_type)
                .bind(&rule.statement)
                .bind(&rule.rationale)
                .bind(rule.confidence)
                .bind(source_count)
                .execute(&mut *tx)
                .await
                .map_err(internal_error)?;

                let item_locator_json = json!({
                    "type": "skill_rule",
                    "skill_draft_id": &skill_draft_id,
                    "skill_rule_id": &skill_rule_id,
                });
                let item_input_json = json!({
                    "skill_draft_id": &skill_draft_id,
                    "candidate_name": &draft.candidate_name,
                    "rule_index": index,
                    "citation_count": rule.citations.len(),
                });
                let item_proposed_output_json = skill_rule_review_payload(
                    &rule.rule_type,
                    &rule.statement,
                    &rule.rationale,
                    rule.confidence,
                    source_count,
                );
                sqlx::query(
                    "INSERT INTO harness_items
                     (item_id, harness_run_id, parent_item_id, item_type, locator_json,
                      input_json, proposed_output_json, final_output_json, status, confidence,
                      created_at, updated_at)
                     VALUES (?, ?, ?, 'skill_rule', ?, ?, ?, '{}', 'pending_review', ?, NOW(6), NOW(6))",
                )
                .bind(&review_item_id)
                .bind(&harness_run_id)
                .bind(&skill_draft_id)
                .bind(item_locator_json.to_string())
                .bind(item_input_json.to_string())
                .bind(item_proposed_output_json.to_string())
                .bind(rule.confidence)
                .execute(&mut *tx)
                .await
                .map_err(internal_error)?;

                for citation in &rule.citations {
                    let source_packet = source_packet_index.get(&citation.source_id).ok_or_else(|| {
                        error_response(
                            StatusCode::BAD_GATEWAY,
                            format!(
                                "skillify agent produced citation source_id {} without a backing source packet",
                                citation.source_id
                            ),
                        )
                    })?;
                    let citation_id = format!("harness-citation-{}", Uuid::new_v4());
                    let (start_byte, end_byte) = locate_source_excerpt(&source_packet.content, &citation.source_excerpt)?;
                    let locator = json!({
                        "source_id": &source_packet.source_id,
                        "event_id": &source_packet.event_id,
                        "session_id": &source_packet.session_id,
                        "title": &source_packet.title,
                        "rule_index": index,
                        "start_byte": start_byte,
                        "end_byte": end_byte,
                        "validation": "exact_source_match",
                    });
                    let source_metadata_json = citation_source_metadata_json(source_packet);
                    sqlx::query(
                        "INSERT INTO harness_citations
                         (citation_id, harness_run_id, item_id, skill_draft_id, skill_rule_id, source_id,
                          source_locator_json, source_snapshot_ref, source_content_hash, source_metadata_json,
                          quote_hash, evidence_text_preview, relevance_score, created_by_node_id, created_at)
                         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 'agent.extract_skill_signals', NOW(6))",
                    )
                    .bind(citation_id)
                    .bind(&harness_run_id)
                    .bind(&review_item_id)
                    .bind(&skill_draft_id)
                    .bind(&skill_rule_id)
                    .bind(&citation.source_id)
                    .bind(locator.to_string())
                    .bind(&source_packet.event_id)
                    .bind(stable_hash(&source_packet.content))
                    .bind(source_metadata_json.to_string())
                    .bind(stable_hash(&citation.source_excerpt))
                    .bind(&citation.source_excerpt)
                    .bind(rule.confidence)
                    .execute(&mut *tx)
                    .await
                    .map_err(internal_error)?;
                }
            }
        }

        tx.commit().await.map_err(internal_error)?;
        Ok(())
        }
        .await;

        match persistence_result {
            Ok(()) => self.load_run(&harness_run_id).await,
            Err(failure) => Err(self
                .record_skillify_run_failure(&user_id, &harness_run_id, failure)
                .await),
        }
    }
}

#[async_trait]
impl HarnessService for DatabaseHarnessService {
    async fn list_templates(
        &self,
    ) -> Result<Vec<HarnessTemplateRecord>, (StatusCode, Json<ErrorResponse>)> {
        Ok(vec![skillify_template()])
    }

    async fn list_node_catalog(
        &self,
    ) -> Result<Vec<HarnessNodeCatalogRecord>, (StatusCode, Json<ErrorResponse>)> {
        Ok(node_catalog())
    }

    async fn create_skillify_run(
        &self,
        user_id: String,
        request: SkillifyRunRequest,
    ) -> Result<HarnessRunRecord, (StatusCode, Json<ErrorResponse>)> {
        self.create_skillify_run_internal(user_id, request, None, None)
            .await
    }
    async fn create_authoring_intent(
        &self,
        user_id: String,
        session_id: String,
        mut request: AuthoringIntentRequest,
        cancel_token: Option<CancellationToken>,
    ) -> Result<AuthoringIntentRecord, (StatusCode, Json<ErrorResponse>)> {
        let goal = request.goal.trim().to_string();
        if goal.is_empty() {
            return Err(error_response(
                StatusCode::BAD_REQUEST,
                "authoring goal must not be empty",
            ));
        }
        if goal.chars().count() > MAX_SKILLIFY_SOURCE_FILE_CHARS {
            return Err(error_response(
                StatusCode::BAD_REQUEST,
                format!(
                    "authoring goal exceeds the {MAX_SKILLIFY_SOURCE_FILE_CHARS} character limit"
                ),
            ));
        }

        request
            .idempotency_key
            .get_or_insert_with(|| Uuid::new_v4().to_string());
        let idempotency_key = request.operation_key(&session_id);
        let replay = if let Some(key) = idempotency_key.as_deref() {
            match self
                .ensure_run_owner(&user_id, &skillify_harness_run_id(&user_id, Some(key)))
                .await
            {
                Ok(_) => true,
                Err((StatusCode::NOT_FOUND, _)) => false,
                Err(error) => return Err(error),
            }
        } else {
            false
        };
        let store = DatabasePersonalSkillStore::new(self.pool.clone());
        let baseline = if replay || request.create_new {
            None
        } else if let Some(target) = request.target_skill.as_ref() {
            let version = store
                .load_version(&user_id, &target.skill_name, &target.version_id)
                .await
                .map_err(|error| {
                    error_response(StatusCode::SERVICE_UNAVAILABLE, error.to_string())
                })?
                .ok_or_else(|| {
                    error_response(StatusCode::NOT_FOUND, "target Skill version was not found")
                })?;
            Some(crate::personal_skills::ActivePersonalSkillRecord {
                skill_name: version.skill_name,
                version_id: version.version_id,
                version: version.version,
                content_hash: version.content_hash,
                content_markdown: version.content_markdown,
            })
        } else if !session_id.trim().is_empty() {
            self.validate_session_ownership(&user_id, std::slice::from_ref(&session_id))
                .await?;
            let active = store
                .load_active_for_session(&user_id, &session_id)
                .await
                .map_err(|error| {
                    error_response(StatusCode::SERVICE_UNAVAILABLE, error.to_string())
                })?;
            if active.len() > 1 {
                return Err(error_response(
                    StatusCode::CONFLICT,
                    format!(
                        "Which Skill should be improved? Specify target_skill with its version: {}",
                        active
                            .iter()
                            .map(|skill| format!("{}@{}", skill.skill_name, skill.version_id))
                            .collect::<Vec<_>>()
                            .join(", ")
                    ),
                ));
            }
            active.into_iter().next()
        } else {
            None
        };
        let context = AuthoringGenerationContext { baseline };
        let harness_run = self
            .create_skillify_run_internal(
                user_id.clone(),
                SkillifyRunRequest {
                    session_ids: session_id
                        .trim()
                        .is_empty()
                        .then(Vec::new)
                        .unwrap_or_else(|| vec![session_id.clone()]),
                    source_files: Some(vec![SkillifySourceFile {
                        file_name: "authoring-intent.txt".to_string(),
                        mime_type: Some("text/plain".to_string()),
                        content: goal.clone(),
                    }]),
                    skill_name: context
                        .baseline
                        .as_ref()
                        .map(|baseline| baseline.skill_name.clone()),
                    topic: None,
                    target_scope: Some("personal".to_string()),
                    idempotency_key,
                },
                Some(&context),
                cancel_token,
            )
            .await?;
        if matches!(harness_run.status.as_str(), "running" | "failed") {
            return Err(error_response(
                StatusCode::CONFLICT,
                format!(
                    "authoring run {} is {}; use a new idempotency key for a fresh attempt after failure",
                    harness_run.harness_run_id, harness_run.status
                ),
            ));
        }
        let skill_drafts = self
            .load_skill_drafts_for_run(&harness_run.harness_run_id)
            .await?;
        let operation = harness_run
            .output_json
            .pointer("/authoring/operation")
            .and_then(Value::as_str)
            .unwrap_or("create")
            .to_string();
        let resolution_source = "frozen_before_generation".to_string();
        let inference = self
            .load_authoring_inference_evidence(&user_id, &harness_run.harness_run_id)
            .await?;
        let mut persisted_evaluation = harness_run
            .output_json
            .pointer("/authoring/evaluation")
            .cloned()
            .and_then(|value| serde_json::from_value::<AuthoringEvaluationSummary>(value).ok());
        let mut persisted_plan = harness_run
            .output_json
            .pointer("/authoring/evaluation_plan")
            .cloned()
            .and_then(|value| {
                serde_json::from_value::<EvaluationExperimentPrepareResponse>(value).ok()
            });
        let mut resolution_run = harness_run.clone();
        let previous_candidate_revision_id = harness_run
            .output_json
            .pointer("/authoring/candidate_revision_id")
            .and_then(Value::as_str)
            .map(str::to_string);
        let candidate_revision_id = if skill_drafts.len() == 1 {
            let version_id = self
                .materialize_authoring_candidate(&user_id, &harness_run, &skill_drafts[0])
                .await?;
            resolution_run.output_json["authoring"]["candidate_revision_id"] =
                Value::String(version_id.clone());
            Some(version_id)
        } else {
            None
        };

        let candidate_changed = previous_candidate_revision_id != candidate_revision_id;
        if let Some(authoring) = resolution_run
            .output_json
            .as_object_mut()
            .and_then(|output| output.get_mut("authoring"))
            .and_then(Value::as_object_mut)
        {
            authoring.insert(
                "candidate_revision_id".to_string(),
                candidate_revision_id
                    .clone()
                    .map_or(Value::Null, Value::String),
            );
            if candidate_changed {
                // Evaluation evidence belongs to one immutable candidate.
                // Clear it in the same conditional write that installs a new
                // candidate reference so an interrupted retry cannot expose
                // a report for an older draft.
                authoring.remove("evaluation");
                authoring.remove("evaluation_plan");
                authoring.remove("evaluation_input");
            }
        }
        if candidate_changed {
            // Persist the candidate reference before resolving optional
            // evaluation evidence. A replay-case or provider lookup failure
            // must leave the private candidate recoverable on retry. The
            // candidate predicate prevents a concurrent authoring request
            // from overwriting a newer candidate.
            let update = if let Some(previous_candidate_revision_id) =
                previous_candidate_revision_id.as_deref()
            {
                sqlx::query(
                    "UPDATE harness_runs SET output_json = ?, updated_at = NOW(6)
                     WHERE user_id = ? AND harness_run_id = ?
                       AND JSON_UNQUOTE(JSON_EXTRACT(output_json, '$.authoring.candidate_revision_id')) = ?
                       AND CAST(updated_at AS CHAR) = ?",
                )
                .bind(resolution_run.output_json.to_string())
                .bind(&user_id)
                .bind(&harness_run.harness_run_id)
                .bind(previous_candidate_revision_id)
                .bind(&harness_run.updated_at)
                .execute(self.pool.get())
                .await
            } else {
                sqlx::query(
                    "UPDATE harness_runs SET output_json = ?, updated_at = NOW(6)
                     WHERE user_id = ? AND harness_run_id = ?
                       AND (JSON_UNQUOTE(JSON_EXTRACT(output_json, '$.authoring.candidate_revision_id')) IS NULL
                            OR JSON_UNQUOTE(JSON_EXTRACT(output_json, '$.authoring.candidate_revision_id')) = 'null')
                       AND CAST(updated_at AS CHAR) = ?",
                )
                .bind(resolution_run.output_json.to_string())
                .bind(&user_id)
                .bind(&harness_run.harness_run_id)
                .bind(&harness_run.updated_at)
                .execute(self.pool.get())
                .await
            }
            .map_err(internal_error)?;
            if update.rows_affected() != 1 {
                return Err(error_response(
                    StatusCode::CONFLICT,
                    "authoring candidate changed while the request was resolving; retry the request",
                ));
            }
        }

        if candidate_changed {
            persisted_evaluation = None;
            persisted_plan = None;
        }
        let mut expected_updated_at = harness_run.updated_at.clone();
        if candidate_changed {
            // The candidate update may have raced a same-candidate Evaluation
            // persistence. Adopt the current row before constructing the
            // final authoring snapshot so a prepared plan is never lost just
            // because this request started from an older read.
            let current = self
                .get_run(user_id.clone(), harness_run.harness_run_id.clone())
                .await?;
            let current_candidate_revision_id = current
                .output_json
                .pointer("/authoring/candidate_revision_id")
                .and_then(Value::as_str)
                .map(str::to_string);
            if current_candidate_revision_id != candidate_revision_id {
                return Err(error_response(
                    StatusCode::CONFLICT,
                    "authoring candidate changed while the request was resolving; retry the request",
                ));
            }
            resolution_run.output_json = current.output_json.clone();
            expected_updated_at = current.updated_at;
            persisted_evaluation = current
                .output_json
                .pointer("/authoring/evaluation")
                .cloned()
                .and_then(|value| serde_json::from_value(value).ok());
            persisted_plan = current
                .output_json
                .pointer("/authoring/evaluation_plan")
                .cloned()
                .and_then(|value| serde_json::from_value(value).ok());
        }
        let persisted_plan = persisted_plan.filter(|plan| {
            candidate_revision_id
                .as_deref()
                .is_some_and(|candidate_id| {
                    plan.experiment.spec.target.candidate.revision_id == candidate_id
                })
        });
        let frozen_task: Option<AuthoringValidationTask> = resolution_run
            .output_json
            .pointer("/authoring/validation_task")
            .filter(|value| !value.is_null())
            .cloned()
            .map(serde_json::from_value)
            .transpose()
            .map_err(internal_error)?;
        let frozen_input: Option<AuthoringEvaluationInput> = resolution_run
            .output_json
            .pointer("/authoring/evaluation_input")
            .filter(|value| !value.is_null())
            .cloned()
            .map(serde_json::from_value)
            .transpose()
            .map_err(internal_error)?;
        if (persisted_plan.is_some() || frozen_task.is_some() || frozen_input.is_some())
            && request.validation_task.is_some()
            && resolution_run
                .output_json
                .pointer("/authoring/validation_task")
                != Some(&json!(request.validation_task))
        {
            return Err(error_response(
                StatusCode::CONFLICT,
                "this candidate already has a frozen evaluation; reuse its existing report",
            ));
        }
        let validation_task = request.validation_task.as_ref().or(frozen_task.as_ref());
        let evaluation_input = if persisted_plan.is_some() {
            None
        } else if frozen_input.is_some() {
            frozen_input
        } else {
            match self
                .resolve_authoring_evaluation_input(
                    &user_id,
                    &session_id,
                    &operation,
                    &resolution_run,
                    &skill_drafts,
                    validation_task,
                )
                .await
            {
                Ok(input) => input,
                Err(failure) if request.validation_task.is_some() => return Err(failure),
                Err((status, error)) => {
                    tracing::warn!(
                        user_id = %user_id,
                        session_id = %session_id,
                        status = %status,
                        error = ?error,
                        "optional authoring evaluation resolution unavailable"
                    );
                    None
                }
            }
        };
        let evaluation = persisted_evaluation.unwrap_or_else(|| {
            if evaluation_input.is_some() {
                AuthoringEvaluationSummary {
                    status: "ready".to_string(),
                    reason: "candidate and server-owned replay case are ready for the shared Evaluation".to_string(),
                    experiment_id: None,
                }
            } else {
                AuthoringEvaluationSummary {
                    status: "unavailable".to_string(),
                    reason: "no server-owned replay case and verifier were resolved from the authoring context".to_string(),
                    experiment_id: None,
                }
            }
        });
        let mut output_json = resolution_run.output_json.clone();
        output_json["authoring"] = json!({
            "target": "skill",
            "operation": &operation,
            "resolution_source": &resolution_source,
            "request": &request,
            "baseline": resolution_run.output_json["authoring"]["baseline"].clone(),
            "validation_task": request.validation_task.as_ref().map(|task| json!(task)).unwrap_or_else(|| resolution_run.output_json["authoring"]["validation_task"].clone()),
            "evaluated_draft_revision": skill_drafts.first().map(|draft| draft.revision),
            "evaluation": &evaluation,
            "evaluation_plan": persisted_plan,
            "evaluation_input": evaluation_input.as_ref().map(|input| json!(input)).unwrap_or_else(|| resolution_run.output_json["authoring"]["evaluation_input"].clone()),
            "candidate_revision_id": candidate_revision_id,
            "skill_draft_count": skill_drafts.len(),
            "inference": &inference,
        });
        let update = if let Some(candidate_revision_id) = candidate_revision_id.as_deref() {
            sqlx::query(
                "UPDATE harness_runs SET output_json = ?, updated_at = NOW(6)
                 WHERE user_id = ? AND harness_run_id = ?
                   AND JSON_UNQUOTE(JSON_EXTRACT(output_json, '$.authoring.candidate_revision_id')) = ?
                   AND CAST(updated_at AS CHAR) = ?",
            )
            .bind(output_json.to_string())
            .bind(&user_id)
            .bind(&harness_run.harness_run_id)
            .bind(candidate_revision_id)
            .bind(&expected_updated_at)
            .execute(self.pool.get())
            .await
        } else {
            sqlx::query(
                "UPDATE harness_runs SET output_json = ?, updated_at = NOW(6)
                 WHERE user_id = ? AND harness_run_id = ?
                   AND (JSON_UNQUOTE(JSON_EXTRACT(output_json, '$.authoring.candidate_revision_id')) IS NULL
                        OR JSON_UNQUOTE(JSON_EXTRACT(output_json, '$.authoring.candidate_revision_id')) = 'null')
                   AND CAST(updated_at AS CHAR) = ?",
            )
            .bind(output_json.to_string())
            .bind(&user_id)
            .bind(&harness_run.harness_run_id)
            .bind(&expected_updated_at)
            .execute(self.pool.get())
            .await
        }
        .map_err(internal_error)?;
        if update.rows_affected() != 1 {
            return Err(error_response(
                StatusCode::CONFLICT,
                "authoring candidate changed while the request was resolving; retry the request",
            ));
        }
        let harness_run = self
            .get_run(user_id, harness_run.harness_run_id.clone())
            .await?;
        let durable_candidate_revision_id = harness_run
            .output_json
            .pointer("/authoring/candidate_revision_id")
            .and_then(Value::as_str)
            .map(str::to_string);
        if durable_candidate_revision_id != candidate_revision_id {
            return Err(error_response(
                StatusCode::CONFLICT,
                "authoring candidate changed while the result was being read; retry the request",
            ));
        }
        let durable_evaluation = harness_run
            .output_json
            .pointer("/authoring/evaluation")
            .cloned()
            .and_then(|value| serde_json::from_value::<AuthoringEvaluationSummary>(value).ok())
            .unwrap_or_else(|| AuthoringEvaluationSummary {
                status: "unavailable".to_string(),
                reason: "the durable authoring Evaluation result was missing after persistence"
                    .to_string(),
                experiment_id: None,
            });
        let durable_plan = harness_run
            .output_json
            .pointer("/authoring/evaluation_plan")
            .cloned()
            .and_then(|value| {
                serde_json::from_value::<EvaluationExperimentPrepareResponse>(value).ok()
            });
        let evaluation_input = if durable_plan.is_some() {
            None
        } else {
            evaluation_input
        };

        Ok(AuthoringIntentRecord {
            target: "skill".to_string(),
            operation,
            resolution_source,
            goal,
            harness_run,
            skill_drafts,
            evaluation: durable_evaluation,
            inference,
            evaluation_plan: durable_plan,
            evaluation_input,
        })
    }

    async fn persist_authoring_evaluation(
        &self,
        user_id: String,
        harness_run_id: String,
        expected_candidate_revision_id: String,
        evaluation: AuthoringEvaluationSummary,
        plan: Option<EvaluationExperimentPrepareResponse>,
    ) -> Result<HarnessRunRecord, (StatusCode, Json<ErrorResponse>)> {
        let run = self.ensure_run_owner(&user_id, &harness_run_id).await?;
        if run
            .output_json
            .pointer("/authoring/candidate_revision_id")
            .and_then(Value::as_str)
            != Some(expected_candidate_revision_id.as_str())
        {
            return Err(error_response(
                StatusCode::CONFLICT,
                "authoring content changed while Evaluation was being prepared; retry the request",
            ));
        }
        let existing_plan = run
            .output_json
            .pointer("/authoring/evaluation_plan")
            .cloned()
            .and_then(|value| {
                serde_json::from_value::<EvaluationExperimentPrepareResponse>(value).ok()
            });
        if plan.is_none()
            && existing_plan.as_ref().is_some_and(|plan| {
                plan.experiment.spec.target.candidate.revision_id == expected_candidate_revision_id
            })
        {
            // A late preparation failure must not erase a prepared result for
            // the same immutable candidate. Candidate changes invalidate the
            // plan before this method is reached.
            return Ok(run);
        }
        let expected_updated_at = run.updated_at.clone();
        let mut output_json = run.output_json;
        let authoring = output_json
            .as_object_mut()
            .and_then(|output| output.get_mut("authoring"))
            .and_then(Value::as_object_mut)
            .ok_or_else(|| {
                internal_error("authoring harness output is missing its authoring object")
            })?;
        authoring.insert(
            "evaluation".to_string(),
            serde_json::to_value(evaluation).map_err(internal_error)?,
        );
        match plan {
            Some(plan) => authoring.insert(
                "evaluation_plan".to_string(),
                serde_json::to_value(plan).map_err(internal_error)?,
            ),
            None => authoring.remove("evaluation_plan"),
        };
        let update = sqlx::query(
            "UPDATE harness_runs SET output_json = ?, updated_at = NOW(6)
             WHERE user_id = ? AND harness_run_id = ?
               AND JSON_UNQUOTE(JSON_EXTRACT(output_json, '$.authoring.candidate_revision_id')) = ?
               AND CAST(updated_at AS CHAR) = ?",
        )
        .bind(output_json.to_string())
        .bind(&user_id)
        .bind(&harness_run_id)
        .bind(&expected_candidate_revision_id)
        .bind(&expected_updated_at)
        .execute(self.pool.get())
        .await
        .map_err(internal_error)?;
        if update.rows_affected() != 1 {
            return Err(error_response(
                StatusCode::CONFLICT,
                "authoring candidate changed while Evaluation was being prepared; retry the request",
            ));
        }
        self.get_run(user_id, harness_run_id).await
    }

    async fn get_run(
        &self,
        user_id: String,
        harness_run_id: String,
    ) -> Result<HarnessRunRecord, (StatusCode, Json<ErrorResponse>)> {
        self.ensure_run_owner(&user_id, &harness_run_id).await
    }

    async fn list_run_items(
        &self,
        user_id: String,
        harness_run_id: String,
    ) -> Result<Vec<HarnessItemRecord>, (StatusCode, Json<ErrorResponse>)> {
        self.ensure_run_owner(&user_id, &harness_run_id).await?;
        let rows = sqlx::query(
            "SELECT item_id, harness_run_id, item_type,
                    IFNULL(locator_json, '{}') AS locator_json,
                    IFNULL(input_json, '{}') AS input_json,
                    IFNULL(proposed_output_json, '{}') AS proposed_output_json,
                    IFNULL(final_output_json, '{}') AS final_output_json,
                    IFNULL(decision_history_json, '[]') AS decision_history_json,
                    status, confidence, assigned_to,
                    CAST(created_at AS CHAR) AS created_at,
                    CAST(updated_at AS CHAR) AS updated_at
             FROM harness_items
             WHERE harness_run_id = ?
             ORDER BY created_at ASC",
        )
        .bind(&harness_run_id)
        .fetch_all(self.pool.get())
        .await
        .map_err(internal_error)?;
        rows.into_iter().map(item_from_row).collect()
    }

    async fn decide_item(
        &self,
        user_id: String,
        harness_run_id: String,
        item_id: String,
        request: HarnessDecisionRequest,
    ) -> Result<HarnessItemRecord, (StatusCode, Json<ErrorResponse>)> {
        self.ensure_run_owner(&user_id, &harness_run_id).await?;
        let item = self.load_item(&harness_run_id, &item_id).await?;
        if let Some((draft_id, rule_id)) = skill_rule_review_item_ids(&item)? {
            self.decide_skill_rule(user_id, harness_run_id.clone(), draft_id, rule_id, request)
                .await?;
            return self.load_item(&harness_run_id, &item_id).await;
        }
        let mut tx = self.pool.get().begin().await.map_err(internal_error)?;
        let current = self
            .load_item_locked(&mut tx, &harness_run_id, &item_id)
            .await?;
        if let Some(idempotency_key) = request.idempotency_key.as_deref()
            && decision_history_contains_idempotency(
                &current.decision_history_json,
                idempotency_key,
            )
        {
            tx.commit().await.map_err(internal_error)?;
            return Ok(current);
        }
        let decision = request.decision.trim();
        let (status, final_output) = match decision {
            "approve" => ("approved", current.proposed_output_json.clone()),
            "reject" => ("rejected", json!({})),
            "edit" => {
                let after = request.after_json.clone().ok_or_else(|| {
                    error_response(StatusCode::BAD_REQUEST, "after_json is required for edit")
                })?;
                ("approved", after)
            }
            "request_revision" => ("needs_revision", json!({})),
            _ => {
                return Err(error_response(
                    StatusCode::BAD_REQUEST,
                    "decision must be approve, reject, edit, or request_revision",
                ));
            }
        };
        let before_json = json!({
            "status": current.status.clone(),
            "proposed_output_json": current.proposed_output_json.clone(),
            "final_output_json": current.final_output_json.clone(),
        });
        let after_json = json!({
            "status": status,
            "final_output_json": final_output.clone(),
        });
        let decision_history_json = append_decision_history(
            &current.decision_history_json,
            decision_history_entry(
                decision,
                &user_id,
                request.reason.as_deref(),
                request.idempotency_key.as_deref(),
                before_json,
                after_json,
            ),
        );
        sqlx::query(
            "UPDATE harness_items
             SET status = ?, final_output_json = ?, decision_history_json = ?, updated_at = NOW(6)
             WHERE harness_run_id = ? AND item_id = ?",
        )
        .bind(status)
        .bind(final_output.to_string())
        .bind(decision_history_json.to_string())
        .bind(&harness_run_id)
        .bind(&item_id)
        .execute(&mut *tx)
        .await
        .map_err(internal_error)?;

        update_skillify_run_counts(&mut tx, &harness_run_id).await?;
        tx.commit().await.map_err(internal_error)?;
        self.load_item(&harness_run_id, &item_id).await
    }

    async fn list_skill_drafts(
        &self,
        user_id: String,
        harness_run_id: String,
    ) -> Result<Vec<HarnessSkillDraftRecord>, (StatusCode, Json<ErrorResponse>)> {
        self.ensure_run_owner(&user_id, &harness_run_id).await?;
        self.load_skill_drafts_for_run(&harness_run_id).await
    }

    async fn get_skill_draft(
        &self,
        user_id: String,
        harness_run_id: String,
        skill_draft_id: String,
    ) -> Result<HarnessSkillDraftRecord, (StatusCode, Json<ErrorResponse>)> {
        self.ensure_run_owner(&user_id, &harness_run_id).await?;
        self.load_skill_draft(&harness_run_id, &skill_draft_id)
            .await
    }

    async fn decide_skill_draft(
        &self,
        user_id: String,
        harness_run_id: String,
        skill_draft_id: String,
        request: HarnessDecisionRequest,
    ) -> Result<HarnessSkillDraftRecord, (StatusCode, Json<ErrorResponse>)> {
        self.ensure_run_owner(&user_id, &harness_run_id).await?;
        let mut tx = self.pool.get().begin().await.map_err(internal_error)?;
        let current = self
            .load_skill_draft_locked(&mut tx, &harness_run_id, &skill_draft_id)
            .await?;

        if let Some(idempotency_key) = request.idempotency_key.as_deref()
            && decision_history_contains_idempotency(
                &current.decision_history_json,
                idempotency_key,
            )
        {
            tx.commit().await.map_err(internal_error)?;
            return self
                .load_skill_draft(&harness_run_id, &skill_draft_id)
                .await;
        }
        validate_reviewed_revision(&current, request.expected_revision)?;
        if current.published_version_id.is_some() {
            return Err(error_response(
                StatusCode::CONFLICT,
                "published drafts are immutable; create an improvement instead",
            ));
        }
        let decision = request.decision.trim();
        let (status, content_markdown, decision_after_json) = match decision {
            "approve" => {
                if current
                    .rules
                    .iter()
                    .any(|rule| harness_skill_rule_blocks_draft_approval(&rule.status))
                {
                    return Err(error_response(
                        StatusCode::CONFLICT,
                        "resolve rejected, conflicted, or revision-needed rules before approving the skill",
                    ));
                }
                sqlx::query(
                    "UPDATE harness_skill_rules
                     SET status = 'approved', updated_at = NOW(6)
                     WHERE harness_run_id = ? AND skill_draft_id = ? AND status = 'proposed'",
                )
                .bind(&harness_run_id)
                .bind(&skill_draft_id)
                .execute(&mut *tx)
                .await
                .map_err(internal_error)?;
                (
                    "ready_to_publish",
                    current.content_markdown.clone(),
                    json!({"decision": "approve"}),
                )
            }
            "reject" => {
                sqlx::query(
                    "UPDATE harness_skill_rules
                     SET status = 'rejected', updated_at = NOW(6)
                     WHERE harness_run_id = ? AND skill_draft_id = ? AND status IN ('proposed', 'needs_revision', 'conflicted')",
                )
                .bind(&harness_run_id)
                .bind(&skill_draft_id)
                .execute(&mut *tx)
                .await
                .map_err(internal_error)?;
                (
                    "rejected",
                    current.content_markdown.clone(),
                    json!({"decision": "reject"}),
                )
            }
            "edit" => {
                let after = request.after_json.clone().ok_or_else(|| {
                    error_response(StatusCode::BAD_REQUEST, "after_json is required for edit")
                })?;
                let markdown = after
                    .get("content_markdown")
                    .and_then(Value::as_str)
                    .ok_or_else(|| {
                        error_response(
                            StatusCode::BAD_REQUEST,
                            "after_json.content_markdown is required for skill draft edit",
                        )
                    })?
                    .trim()
                    .to_string();
                if markdown.is_empty() {
                    return Err(error_response(
                        StatusCode::BAD_REQUEST,
                        "content_markdown must not be empty",
                    ));
                }
                ("pending_rule_review", markdown, after)
            }
            "request_revision" => (
                "needs_revision",
                current.content_markdown.clone(),
                json!({"decision": "request_revision"}),
            ),
            _ => {
                return Err(error_response(
                    StatusCode::BAD_REQUEST,
                    "decision must be approve, reject, edit, or request_revision",
                ));
            }
        };
        let before_json = json!({
            "status": current.status.clone(),
            "content_markdown": current.content_markdown.clone(),
            "revision": current.revision,
        });
        let after_json = json!({
            "status": status,
            "content_markdown": content_markdown.clone(),
            "revision": current.revision + i64::from(content_markdown != current.content_markdown),
            "payload": decision_after_json,
        });
        let decision_history_json = append_decision_history(
            &current.decision_history_json,
            decision_history_entry(
                decision,
                &user_id,
                request.reason.as_deref(),
                request.idempotency_key.as_deref(),
                before_json,
                after_json,
            ),
        );
        sqlx::query(
            "UPDATE harness_skill_drafts
             SET status = ?, content_markdown = ?, decision_history_json = ?, revision = revision + ?, updated_at = NOW(6)
             WHERE harness_run_id = ? AND skill_draft_id = ?",
        )
        .bind(status)
        .bind(&content_markdown)
        .bind(decision_history_json.to_string())
        .bind(i32::from(content_markdown != current.content_markdown))
        .bind(&harness_run_id)
        .bind(&skill_draft_id)
        .execute(&mut *tx)
        .await
        .map_err(internal_error)?;
        update_skillify_draft_counts(&mut tx, &harness_run_id).await?;
        if content_markdown != current.content_markdown {
            mark_authoring_evaluation_stale(&mut tx, &harness_run_id).await?;
        }
        tx.commit().await.map_err(internal_error)?;
        self.load_skill_draft(&harness_run_id, &skill_draft_id)
            .await
    }

    async fn decide_skill_rule(
        &self,
        user_id: String,
        harness_run_id: String,
        skill_draft_id: String,
        skill_rule_id: String,
        request: HarnessDecisionRequest,
    ) -> Result<HarnessSkillDraftRecord, (StatusCode, Json<ErrorResponse>)> {
        self.ensure_run_owner(&user_id, &harness_run_id).await?;
        let mut tx = self.pool.get().begin().await.map_err(internal_error)?;
        let mut draft = self
            .load_skill_draft_locked(&mut tx, &harness_run_id, &skill_draft_id)
            .await?;

        let current = draft
            .rules
            .iter()
            .find(|rule| rule.skill_rule_id == skill_rule_id)
            .ok_or_else(|| error_response(StatusCode::NOT_FOUND, "skill rule not found"))?;
        if let Some(idempotency_key) = request.idempotency_key.as_deref()
            && decision_history_contains_idempotency(
                &current.decision_history_json,
                idempotency_key,
            )
        {
            tx.commit().await.map_err(internal_error)?;
            return self
                .load_skill_draft(&harness_run_id, &skill_draft_id)
                .await;
        }
        validate_reviewed_revision(&draft, request.expected_revision)?;
        if draft.published_version_id.is_some() {
            return Err(error_response(
                StatusCode::CONFLICT,
                "published drafts are immutable; create an improvement instead",
            ));
        }
        let decision = request.decision.trim();
        let (status, statement, rationale, decision_after_json) = match decision {
            "approve" => (
                "approved",
                current.statement.clone(),
                current.rationale.clone(),
                json!({
                    "statement": current.statement.clone(),
                    "rationale": current.rationale.clone()
                }),
            ),
            "reject" => (
                "rejected",
                current.statement.clone(),
                current.rationale.clone(),
                json!({}),
            ),
            "edit" => {
                let after = request.after_json.clone().ok_or_else(|| {
                    error_response(StatusCode::BAD_REQUEST, "after_json is required for edit")
                })?;
                let statement = after
                    .get("statement")
                    .and_then(Value::as_str)
                    .unwrap_or(&current.statement)
                    .trim()
                    .to_string();
                if statement.is_empty() {
                    return Err(error_response(
                        StatusCode::BAD_REQUEST,
                        "rule statement must not be empty",
                    ));
                }
                let rationale = after
                    .get("rationale")
                    .and_then(Value::as_str)
                    .unwrap_or(&current.rationale)
                    .trim()
                    .to_string();
                ("edited", statement, rationale, after)
            }
            "request_revision" => (
                "needs_revision",
                current.statement.clone(),
                current.rationale.clone(),
                json!({"decision": "request_revision"}),
            ),
            _ => {
                return Err(error_response(
                    StatusCode::BAD_REQUEST,
                    "decision must be approve, reject, edit, or request_revision",
                ));
            }
        };
        let before_json = json!({
            "status": current.status.clone(),
            "statement": current.statement.clone(),
            "rationale": current.rationale.clone(),
        });
        let after_json = json!({
            "status": status,
            "statement": statement.clone(),
            "rationale": rationale.clone(),
            "payload": decision_after_json,
        });
        let decision_history_json = append_decision_history(
            &current.decision_history_json,
            decision_history_entry(
                decision,
                &user_id,
                request.reason.as_deref(),
                request.idempotency_key.as_deref(),
                before_json,
                after_json,
            ),
        );
        sqlx::query(
            "UPDATE harness_skill_rules
             SET status = ?, statement = ?, rationale = ?, decision_history_json = ?, updated_at = NOW(6)
             WHERE harness_run_id = ? AND skill_draft_id = ? AND skill_rule_id = ?",
        )
        .bind(status)
        .bind(&statement)
        .bind(&rationale)
        .bind(decision_history_json.to_string())
        .bind(&harness_run_id)
        .bind(&skill_draft_id)
        .bind(&skill_rule_id)
        .execute(&mut *tx)
        .await
        .map_err(internal_error)?;
        let review_item_id = skill_rule_review_item_id(&skill_rule_id);
        let review_item = self
            .load_item_locked(&mut tx, &harness_run_id, &review_item_id)
            .await?;
        let item_status = item_status_for_skill_rule_status(status);
        let item_final_output = if item_status == "approved" {
            skill_rule_review_payload(
                &current.rule_type,
                &statement,
                &rationale,
                current.confidence,
                current.source_count,
            )
        } else {
            json!({})
        };
        let item_before_json = json!({
            "status": review_item.status.clone(),
            "proposed_output_json": review_item.proposed_output_json.clone(),
            "final_output_json": review_item.final_output_json.clone(),
        });
        let item_after_json = json!({
            "status": item_status,
            "final_output_json": item_final_output.clone(),
        });
        let item_decision_history_json = append_decision_history(
            &review_item.decision_history_json,
            decision_history_entry(
                decision,
                &user_id,
                request.reason.as_deref(),
                request.idempotency_key.as_deref(),
                item_before_json,
                item_after_json,
            ),
        );
        sqlx::query(
            "UPDATE harness_items
             SET status = ?, final_output_json = ?, decision_history_json = ?, updated_at = NOW(6)
             WHERE harness_run_id = ? AND item_id = ?",
        )
        .bind(item_status)
        .bind(item_final_output.to_string())
        .bind(item_decision_history_json.to_string())
        .bind(&harness_run_id)
        .bind(&review_item_id)
        .execute(&mut *tx)
        .await
        .map_err(internal_error)?;
        for rule in &mut draft.rules {
            if rule.skill_rule_id == skill_rule_id {
                rule.status = status.to_string();
            }
        }
        refresh_skill_draft_after_rule_decision(
            &mut tx,
            &draft,
            decision,
            request.after_json.as_ref(),
        )
        .await?;
        update_skillify_run_counts(&mut tx, &harness_run_id).await?;
        update_skillify_draft_counts(&mut tx, &harness_run_id).await?;
        tx.commit().await.map_err(internal_error)?;
        self.load_skill_draft(&harness_run_id, &skill_draft_id)
            .await
    }

    async fn publish_skill_draft(
        &self,
        user_id: String,
        harness_run_id: String,
        skill_draft_id: String,
        request: SkillifyPublishRequest,
    ) -> Result<SkillifyPublishRecord, (StatusCode, Json<ErrorResponse>)> {
        let run = self.ensure_run_owner(&user_id, &harness_run_id).await?;
        if run.harness_id != SKILLIFY_HARNESS_ID {
            return Err(error_response(
                StatusCode::BAD_REQUEST,
                "only skillify harness drafts can be published as skills",
            ));
        }
        let mut tx = self.pool.get().begin().await.map_err(internal_error)?;
        let draft = self
            .load_skill_draft_locked(&mut tx, &harness_run_id, &skill_draft_id)
            .await?;
        validate_reviewed_revision(&draft, Some(request.expected_revision))?;
        if draft.status != "published" && !harness_skill_draft_is_publishable(&draft.status) {
            return Err(error_response(
                StatusCode::CONFLICT,
                "skill draft must be approved before publishing",
            ));
        }
        if draft
            .rules
            .iter()
            .any(|rule| harness_skill_rule_is_unresolved(&rule.status))
        {
            return Err(error_response(
                StatusCode::CONFLICT,
                "resolve all skill rules before publishing",
            ));
        }
        let approved_rule_count = draft
            .rules
            .iter()
            .filter(|rule| harness_skill_rule_is_approved(&rule.status))
            .count();
        if approved_rule_count == 0 {
            return Err(error_response(
                StatusCode::BAD_REQUEST,
                "at least one approved rule is required",
            ));
        }
        let visibility = request
            .visibility
            .unwrap_or_else(|| draft.publish_visibility.clone());
        validate_publish_visibility(&visibility)?;
        validate_skill_name(&draft.candidate_name)?;
        // One reviewed draft owns one default publication identity. Different
        // improvements never compete for a fixed 0.1.0 label; retries converge.
        let version = request.version.unwrap_or_else(|| {
            let identity = stable_hash(&skill_draft_id);
            format!("0.1.0+{}", &identity[7..55])
        });
        let description = request
            .description
            .unwrap_or_else(|| draft.description.clone());
        let manifest = json!({
            "name": draft.candidate_name,
            "description": description,
            "version": version
        });
        let existing = DatabasePersonalSkillStore::load_version_by_version_on(
            &mut tx,
            &user_id,
            &draft.candidate_name,
            &version,
        )
        .await
        .map_err(|e| error_response(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
        let version_record = if let Some(existing) = existing {
            if existing.status != "published"
                || existing.manifest_json != manifest
                || existing.content_markdown != draft.content_markdown
            {
                return Err(error_response(
                    StatusCode::CONFLICT,
                    "publication version already belongs to different content",
                ));
            }
            existing
        } else {
            if draft.published_version_id.is_some() {
                return Err(error_response(
                    StatusCode::CONFLICT,
                    "this draft has already been published with another version",
                ));
            }
            DatabasePersonalSkillStore::submit_version_on(
                &mut tx,
                &user_id,
                &draft.candidate_name,
                SubmitUserSkillVersion {
                    version,
                    manifest_json: manifest,
                    content_markdown: draft.content_markdown.clone(),
                    status: Some("published".to_string()),
                },
            )
            .await
            .map_err(|e| error_response(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        };

        if draft
            .published_version_id
            .as_ref()
            .is_some_and(|id| id != &version_record.version_id)
        {
            return Err(error_response(
                StatusCode::CONFLICT,
                "this draft has already been published with another version",
            ));
        }
        if draft.status != "published" || draft.publish_visibility != visibility {
            DatabasePersonalSkillStore::create_source_on(
                &mut tx,
                &user_id,
                CreateUserSkillSource {
                    skill_name: draft.candidate_name.clone(),
                    visibility: Some(visibility.clone()),
                },
            )
            .await
            .map_err(|e| error_response(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
            sqlx::query(
                "UPDATE harness_skill_drafts
                 SET status = 'published', published_version_id = ?, publish_visibility = ?, updated_at = NOW(6)
                 WHERE harness_run_id = ? AND skill_draft_id = ?",
            )
            .bind(&version_record.version_id)
            .bind(&visibility)
            .bind(&harness_run_id)
            .bind(&skill_draft_id)
            .execute(&mut *tx)
            .await
            .map_err(internal_error)?;
            update_skillify_draft_counts(&mut tx, &harness_run_id).await?;
        }
        tx.commit().await.map_err(internal_error)?;

        Ok(SkillifyPublishRecord {
            harness_run_id,
            skill_draft_id,
            skill_name: draft.candidate_name,
            version_id: version_record.version_id,
            visibility,
            content_markdown: draft.content_markdown,
            approved_rule_count,
        })
    }
}

pub struct UnconfiguredHarnessService;

#[async_trait]
impl HarnessService for UnconfiguredHarnessService {
    async fn list_templates(
        &self,
    ) -> Result<Vec<HarnessTemplateRecord>, (StatusCode, Json<ErrorResponse>)> {
        Err(internal_error("harness service not configured"))
    }

    async fn list_node_catalog(
        &self,
    ) -> Result<Vec<HarnessNodeCatalogRecord>, (StatusCode, Json<ErrorResponse>)> {
        Err(internal_error("harness service not configured"))
    }

    async fn create_skillify_run(
        &self,
        _user_id: String,
        _request: SkillifyRunRequest,
    ) -> Result<HarnessRunRecord, (StatusCode, Json<ErrorResponse>)> {
        Err(internal_error("harness service not configured"))
    }

    async fn create_authoring_intent(
        &self,
        _user_id: String,
        _session_id: String,
        _request: AuthoringIntentRequest,
        _cancel_token: Option<CancellationToken>,
    ) -> Result<AuthoringIntentRecord, (StatusCode, Json<ErrorResponse>)> {
        Err(internal_error("harness service not configured"))
    }

    async fn persist_authoring_evaluation(
        &self,
        _user_id: String,
        _harness_run_id: String,
        _expected_candidate_revision_id: String,
        _evaluation: AuthoringEvaluationSummary,
        _plan: Option<EvaluationExperimentPrepareResponse>,
    ) -> Result<HarnessRunRecord, (StatusCode, Json<ErrorResponse>)> {
        Err(internal_error("harness service not configured"))
    }

    async fn get_run(
        &self,
        _user_id: String,
        _harness_run_id: String,
    ) -> Result<HarnessRunRecord, (StatusCode, Json<ErrorResponse>)> {
        Err(internal_error("harness service not configured"))
    }

    async fn list_run_items(
        &self,
        _user_id: String,
        _harness_run_id: String,
    ) -> Result<Vec<HarnessItemRecord>, (StatusCode, Json<ErrorResponse>)> {
        Err(internal_error("harness service not configured"))
    }

    async fn decide_item(
        &self,
        _user_id: String,
        _harness_run_id: String,
        _item_id: String,
        _request: HarnessDecisionRequest,
    ) -> Result<HarnessItemRecord, (StatusCode, Json<ErrorResponse>)> {
        Err(internal_error("harness service not configured"))
    }

    async fn list_skill_drafts(
        &self,
        _user_id: String,
        _harness_run_id: String,
    ) -> Result<Vec<HarnessSkillDraftRecord>, (StatusCode, Json<ErrorResponse>)> {
        Err(internal_error("harness service not configured"))
    }

    async fn get_skill_draft(
        &self,
        _user_id: String,
        _harness_run_id: String,
        _skill_draft_id: String,
    ) -> Result<HarnessSkillDraftRecord, (StatusCode, Json<ErrorResponse>)> {
        Err(internal_error("harness service not configured"))
    }

    async fn decide_skill_draft(
        &self,
        _user_id: String,
        _harness_run_id: String,
        _skill_draft_id: String,
        _request: HarnessDecisionRequest,
    ) -> Result<HarnessSkillDraftRecord, (StatusCode, Json<ErrorResponse>)> {
        Err(internal_error("harness service not configured"))
    }

    async fn decide_skill_rule(
        &self,
        _user_id: String,
        _harness_run_id: String,
        _skill_draft_id: String,
        _skill_rule_id: String,
        _request: HarnessDecisionRequest,
    ) -> Result<HarnessSkillDraftRecord, (StatusCode, Json<ErrorResponse>)> {
        Err(internal_error("harness service not configured"))
    }

    async fn publish_skill_draft(
        &self,
        _user_id: String,
        _harness_run_id: String,
        _skill_draft_id: String,
        _request: SkillifyPublishRequest,
    ) -> Result<SkillifyPublishRecord, (StatusCode, Json<ErrorResponse>)> {
        Err(internal_error("harness service not configured"))
    }
}

#[derive(Clone, Debug)]
struct SkillifyEvent {
    event_id: String,
    session_id: String,
    source_id: String,
    source_type: String,
    title: String,
    event_type: String,
    content: String,
}

fn required_harness_string(
    row: &sqlx::mysql::MySqlRow,
    table: &'static str,
    column: &'static str,
) -> HarnessResult<String> {
    let value: String = row
        .try_get(column)
        .map_err(|err| internal_error(format!("invalid {table}.{column}: {err}")))?;
    if value.trim().is_empty() {
        return Err(internal_error(format!(
            "invalid {table}.{column}: value is empty"
        )));
    }
    Ok(value)
}

fn optional_harness_string(
    row: &sqlx::mysql::MySqlRow,
    table: &'static str,
    column: &'static str,
) -> HarnessResult<Option<String>> {
    let value: Option<String> = row
        .try_get(column)
        .map_err(|err| internal_error(format!("invalid {table}.{column}: {err}")))?;
    if value
        .as_deref()
        .is_some_and(|value| value.trim().is_empty())
    {
        return Err(internal_error(format!(
            "invalid {table}.{column}: value is empty"
        )));
    }
    Ok(value)
}

fn skillify_session_event_title(session_id: &str, event_type: &str) -> String {
    format!("{event_type} ({session_id})")
}

fn skill_rule_review_item_id(skill_rule_id: &str) -> String {
    format!("harness-item-{skill_rule_id}")
}

fn skill_rule_review_item_ids(
    item: &HarnessItemRecord,
) -> HarnessResult<Option<SkillRuleReviewIds>> {
    if item.item_type != "skill_rule" {
        return Ok(None);
    }
    let skill_draft_id = item
        .locator_json
        .get("skill_draft_id")
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| internal_error("skill_rule review item missing locator.skill_draft_id"))?;
    let skill_rule_id = item
        .locator_json
        .get("skill_rule_id")
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| internal_error("skill_rule review item missing locator.skill_rule_id"))?;
    Ok(Some((
        skill_draft_id.to_string(),
        skill_rule_id.to_string(),
    )))
}

fn item_status_for_skill_rule_status(status: &str) -> &'static str {
    match harness_skill_rule_status_kind(status) {
        HarnessSkillRuleStatusKind::Approved | HarnessSkillRuleStatusKind::Edited => "approved",
        HarnessSkillRuleStatusKind::Rejected => "rejected",
        HarnessSkillRuleStatusKind::NeedsRevision => "needs_revision",
        _ => "pending_review",
    }
}

fn skill_rule_review_payload(
    rule_type: &str,
    statement: &str,
    rationale: &str,
    confidence: Option<f64>,
    source_count: i64,
) -> Value {
    json!({
        "rule_type": rule_type,
        "statement": statement,
        "rationale": rationale,
        "confidence": confidence,
        "source_count": source_count,
    })
}

fn skillify_source_packet_from_event(event: &SkillifyEvent) -> SkillifySourcePacket {
    SkillifySourcePacket {
        event_id: event.event_id.clone(),
        session_id: event.session_id.clone(),
        source_id: event.source_id.clone(),
        source_type: event.source_type.clone(),
        title: event.title.clone(),
        event_type: event.event_type.clone(),
        content: event.content.clone(),
    }
}

fn validate_skillify_target_scope(
    target_scope: &str,
) -> Result<(), (StatusCode, Json<ErrorResponse>)> {
    match target_scope {
        "personal" | "project" => Ok(()),
        _ => Err(error_response(
            StatusCode::BAD_REQUEST,
            "target_scope must be personal or project",
        )),
    }
}

fn invalid_skillify_agent_output(message: impl Into<String>) -> (StatusCode, Json<ErrorResponse>) {
    error_response(
        StatusCode::BAD_GATEWAY,
        format!("Skillify agent returned invalid output: {}", message.into()),
    )
}

fn validate_skillify_agent_output(
    output: &SkillifyAgentOutput,
    events: &[SkillifyEvent],
) -> Result<(), (StatusCode, Json<ErrorResponse>)> {
    if output.extractor.trim().is_empty() {
        return Err(invalid_skillify_agent_output("missing extractor"));
    }

    let sources = events
        .iter()
        .map(|event| (event.source_id.as_str(), event.content.as_str()))
        .collect::<HashMap<_, _>>();

    for draft in &output.drafts {
        validate_skill_name(&draft.candidate_name)
            .map_err(|_| invalid_skillify_agent_output("invalid candidate_name"))?;
        validate_skillify_target_scope(&draft.target_scope)
            .map_err(|_| invalid_skillify_agent_output("invalid target_scope"))?;
        validate_publish_visibility(&draft.publish_visibility)
            .map_err(|_| invalid_skillify_agent_output("invalid publish_visibility"))?;
        if draft.description.trim().is_empty() {
            return Err(invalid_skillify_agent_output("draft description is empty"));
        }
        if draft.content_markdown.trim().is_empty() {
            return Err(invalid_skillify_agent_output(
                "draft content_markdown is empty",
            ));
        }
        if !draft.source_summary_json.is_object() {
            return Err(invalid_skillify_agent_output(
                "draft source_summary_json must be an object",
            ));
        }
        if draft.rules.is_empty() {
            return Err(invalid_skillify_agent_output(
                "draft must contain at least one cited rule",
            ));
        }

        for rule in &draft.rules {
            if rule.statement.trim().is_empty() {
                return Err(invalid_skillify_agent_output("rule statement is empty"));
            }
            if rule.rationale.trim().is_empty() {
                return Err(invalid_skillify_agent_output("rule rationale is empty"));
            }
            if rule.citations.is_empty() {
                return Err(invalid_skillify_agent_output(
                    "every rule must include at least one citation",
                ));
            }
            for citation in &rule.citations {
                if !sources.contains_key(citation.source_id.as_str()) {
                    return Err(invalid_skillify_agent_output(format!(
                        "unknown citation source_id {}",
                        citation.source_id
                    )));
                }
                locate_source_excerpt(
                    sources[citation.source_id.as_str()],
                    &citation.source_excerpt,
                )?;
            }
        }
    }

    Ok(())
}

fn locate_source_excerpt(content: &str, excerpt: &str) -> HarnessResult<(usize, usize)> {
    if excerpt.trim().is_empty() {
        return Err(invalid_skillify_agent_output(
            "citation source_excerpt is empty",
        ));
    }
    let start = content.find(excerpt).ok_or_else(|| {
        invalid_skillify_agent_output("citation source_excerpt is not present in its frozen source")
    })?;
    Ok((start, start + excerpt.len()))
}

fn unique_rule_source_count(rule: &SkillifyAgentRule) -> i64 {
    rule.citations
        .iter()
        .map(|citation| citation.source_id.as_str())
        .collect::<HashSet<_>>()
        .len() as i64
}

fn skillify_template() -> HarnessTemplateRecord {
    HarnessTemplateRecord {
        template_id: SKILLIFY_TEMPLATE_ID.to_string(),
        name: "Skillify from sources".to_string(),
        description: "Create reviewed draft skills from selected sessions and text files."
            .to_string(),
        built_in: true,
        input_schema_json: json!({
            "type": "object",
            "properties": {
                "session_ids": {"type": "array", "items": {"type": "string"}, "minItems": 1},
                "source_files": {"type": "array", "items": {"type": "object"}},
                "skill_name": {"type": "string"},
                "topic": {"type": "string"},
                "target_scope": {"type": "string", "enum": ["personal", "project"]}
            }
        }),
        workflow_json: json!({
            "nodes": [
                "source.snapshot_sessions_and_files",
                "source.normalize_skillify_inputs",
                "agent.extract_skill_signals",
                "agent.synthesize_skill_drafts",
                "validate.skill_drafts",
                "human.review_skill_drafts",
                "skill.render_approved_draft",
                "skill.validate_draft",
                "skill.queue_publish_candidate",
                "human.publish_decision"
            ]
        }),
    }
}

fn node_catalog() -> Vec<HarnessNodeCatalogRecord> {
    vec![
        node(
            "source.snapshot_sessions_and_files",
            "Snapshot selected session events and uploaded text files",
        ),
        node(
            "source.normalize_skillify_inputs",
            "Normalize Skillify source packets for agent extraction",
        ),
        node(
            "agent.extract_skill_signals",
            "Extract evidence-backed skill signals from sources",
        ),
        node(
            "agent.synthesize_skill_drafts",
            "Synthesize coherent draft skills from extracted signals",
        ),
        node(
            "validate.skill_drafts",
            "Validate draft skill schema, rules, and source citations",
        ),
        node("human.review_skill_drafts", "Review skill drafts and rules"),
        node(
            "skill.render_approved_draft",
            "Render approved rules into a final draft skill revision",
        ),
        node(
            "skill.validate_draft",
            "Validate generated skill manifest and markdown",
        ),
        node(
            "skill.queue_publish_candidate",
            "Queue approved skill drafts for private or public publication",
        ),
        node(
            "human.publish_decision",
            "Keep activation as a human-owned action",
        ),
    ]
}

fn node(node_type: &str, description: &str) -> HarnessNodeCatalogRecord {
    HarnessNodeCatalogRecord {
        node_type: node_type.to_string(),
        description: description.to_string(),
        input_schema_json: json!({"type": "object"}),
        output_schema_json: json!({"type": "object"}),
    }
}

fn normalize_session_ids(session_ids: Vec<String>) -> Vec<String> {
    let mut out = Vec::new();
    for session_id in session_ids {
        let session_id = session_id.trim();
        if !session_id.is_empty() && !out.iter().any(|existing| existing == session_id) {
            out.push(session_id.to_string());
        }
    }
    out
}

fn item_from_row(row: sqlx::mysql::MySqlRow) -> HarnessResult<HarnessItemRecord> {
    Ok(HarnessItemRecord {
        item_id: required_harness_string(&row, "harness_items", "item_id")?,
        harness_run_id: required_harness_string(&row, "harness_items", "harness_run_id")?,
        item_type: required_harness_string(&row, "harness_items", "item_type")?,
        locator_json: parse_json_cell(&row, "harness_items", "locator_json")?,
        input_json: parse_json_cell(&row, "harness_items", "input_json")?,
        proposed_output_json: parse_json_cell(&row, "harness_items", "proposed_output_json")?,
        final_output_json: parse_json_cell(&row, "harness_items", "final_output_json")?,
        decision_history_json: parse_json_cell(&row, "harness_items", "decision_history_json")?,
        status: required_harness_string(&row, "harness_items", "status")?,
        confidence: row
            .try_get("confidence")
            .map_err(|err| internal_error(format!("invalid harness_items.confidence: {err}")))?,
        assigned_to: optional_harness_string(&row, "harness_items", "assigned_to")?,
        created_at: required_harness_string(&row, "harness_items", "created_at")?,
        updated_at: required_harness_string(&row, "harness_items", "updated_at")?,
    })
}

fn skill_rule_from_row(row: sqlx::mysql::MySqlRow) -> HarnessResult<HarnessSkillRuleRecord> {
    Ok(HarnessSkillRuleRecord {
        skill_rule_id: required_harness_string(&row, "harness_skill_rules", "skill_rule_id")?,
        skill_draft_id: required_harness_string(&row, "harness_skill_rules", "skill_draft_id")?,
        harness_run_id: required_harness_string(&row, "harness_skill_rules", "harness_run_id")?,
        rule_type: required_harness_string(&row, "harness_skill_rules", "rule_type")?,
        statement: required_harness_string(&row, "harness_skill_rules", "statement")?,
        rationale: required_harness_string(&row, "harness_skill_rules", "rationale")?,
        decision_history_json: parse_json_cell(
            &row,
            "harness_skill_rules",
            "decision_history_json",
        )?,
        status: required_harness_string(&row, "harness_skill_rules", "status")?,
        confidence: row.try_get("confidence").map_err(|err| {
            internal_error(format!("invalid harness_skill_rules.confidence: {err}"))
        })?,
        source_count: row.try_get("source_count").map_err(|err| {
            internal_error(format!("invalid harness_skill_rules.source_count: {err}"))
        })?,
        created_by_node_id: optional_harness_string(
            &row,
            "harness_skill_rules",
            "created_by_node_id",
        )?,
        created_at: required_harness_string(&row, "harness_skill_rules", "created_at")?,
        updated_at: required_harness_string(&row, "harness_skill_rules", "updated_at")?,
        citations: Vec::new(),
    })
}

fn citation_from_row(row: sqlx::mysql::MySqlRow) -> HarnessResult<HarnessCitationRecord> {
    Ok(HarnessCitationRecord {
        citation_id: required_harness_string(&row, "harness_citations", "citation_id")?,
        harness_run_id: required_harness_string(&row, "harness_citations", "harness_run_id")?,
        item_id: required_harness_string(&row, "harness_citations", "item_id")?,
        skill_draft_id: optional_harness_string(&row, "harness_citations", "skill_draft_id")?,
        skill_rule_id: optional_harness_string(&row, "harness_citations", "skill_rule_id")?,
        source_id: optional_harness_string(&row, "harness_citations", "source_id")?,
        source_locator_json: parse_json_cell(&row, "harness_citations", "source_locator_json")?,
        source_snapshot_ref: optional_harness_string(
            &row,
            "harness_citations",
            "source_snapshot_ref",
        )?,
        source_content_hash: optional_harness_string(
            &row,
            "harness_citations",
            "source_content_hash",
        )?,
        source_metadata_json: parse_json_cell(&row, "harness_citations", "source_metadata_json")?,
        artifact_id: optional_harness_string(&row, "harness_citations", "artifact_id")?,
        quote_hash: optional_harness_string(&row, "harness_citations", "quote_hash")?,
        evidence_text_preview: optional_harness_string(
            &row,
            "harness_citations",
            "evidence_text_preview",
        )?,
        relevance_score: row.try_get("relevance_score").map_err(|err| {
            internal_error(format!("invalid harness_citations.relevance_score: {err}"))
        })?,
        created_by_node_id: optional_harness_string(
            &row,
            "harness_citations",
            "created_by_node_id",
        )?,
        created_at: required_harness_string(&row, "harness_citations", "created_at")?,
    })
}

fn skill_draft_from_row(row: sqlx::mysql::MySqlRow) -> HarnessResult<HarnessSkillDraftRecord> {
    Ok(HarnessSkillDraftRecord {
        skill_draft_id: required_harness_string(&row, "harness_skill_drafts", "skill_draft_id")?,
        harness_run_id: required_harness_string(&row, "harness_skill_drafts", "harness_run_id")?,
        candidate_name: required_harness_string(&row, "harness_skill_drafts", "candidate_name")?,
        description: required_harness_string(&row, "harness_skill_drafts", "description")?,
        target_scope: required_harness_string(&row, "harness_skill_drafts", "target_scope")?,
        publish_visibility: required_harness_string(
            &row,
            "harness_skill_drafts",
            "publish_visibility",
        )?,
        content_markdown: required_harness_string(
            &row,
            "harness_skill_drafts",
            "content_markdown",
        )?,
        source_summary_json: parse_json_cell(&row, "harness_skill_drafts", "source_summary_json")?,
        decision_history_json: parse_json_cell(
            &row,
            "harness_skill_drafts",
            "decision_history_json",
        )?,
        status: required_harness_string(&row, "harness_skill_drafts", "status")?,
        confidence: row.try_get("confidence").map_err(|err| {
            internal_error(format!("invalid harness_skill_drafts.confidence: {err}"))
        })?,
        created_by_node_id: optional_harness_string(
            &row,
            "harness_skill_drafts",
            "created_by_node_id",
        )?,
        revision: row.try_get("revision").map_err(|err| {
            internal_error(format!("invalid harness_skill_drafts.revision: {err}"))
        })?,
        published_version_id: optional_harness_string(
            &row,
            "harness_skill_drafts",
            "published_version_id",
        )?,
        created_at: required_harness_string(&row, "harness_skill_drafts", "created_at")?,
        updated_at: required_harness_string(&row, "harness_skill_drafts", "updated_at")?,
        rules: Vec::new(),
    })
}

fn parse_json_cell(
    row: &sqlx::mysql::MySqlRow,
    table: &'static str,
    column: &'static str,
) -> HarnessResult<Value> {
    let text = required_harness_string(row, table, column)?;
    serde_json::from_str(&text)
        .map_err(|err| internal_error(format!("invalid {table}.{column}: {err}")))
}

fn decision_history_entry(
    decision: &str,
    actor_user_id: &str,
    reason: Option<&str>,
    idempotency_key: Option<&str>,
    before_json: Value,
    after_json: Value,
) -> Value {
    json!({
        "decision": decision,
        "actor_user_id": actor_user_id,
        "reason": reason,
        "idempotency_key": idempotency_key,
        "decided_at": chrono::Utc::now().to_rfc3339(),
        "before_json": before_json,
        "after_json": after_json,
    })
}

fn append_decision_history(history: &Value, entry: Value) -> Value {
    match history {
        Value::Array(entries) => {
            let mut next = entries.clone();
            next.push(entry);
            Value::Array(next)
        }
        _ => Value::Array(vec![entry]),
    }
}

fn decision_history_contains_idempotency(history: &Value, idempotency_key: &str) -> bool {
    match history {
        Value::Array(entries) => entries.iter().any(|entry| {
            entry
                .get("idempotency_key")
                .and_then(Value::as_str)
                .map(|seen| seen == idempotency_key)
                .unwrap_or(false)
        }),
        _ => false,
    }
}

fn citation_source_metadata_json(source_packet: &SkillifySourcePacket) -> Value {
    json!({
        "event_id": &source_packet.event_id,
        "session_id": &source_packet.session_id,
        "source_type": &source_packet.source_type,
        "title": &source_packet.title,
        "event_type": &source_packet.event_type,
        "content_chars": source_packet.content.chars().count(),
        "evidence_kind": match source_packet.event_type.as_str() {
            "user_query" | "user_message" => "user_statement",
            "tool_result" | "tool_execution_result" => "execution_result",
            _ if source_packet.title == "authoring-intent.txt" => "user_goal",
            _ => "source_material",
        },
    })
}

async fn update_skillify_run_counts(
    tx: &mut sqlx::Transaction<'_, MySql>,
    harness_run_id: &str,
) -> Result<(), (StatusCode, Json<ErrorResponse>)> {
    let row = sqlx::query(
        "SELECT
            CAST(COALESCE(SUM(CASE WHEN status = 'approved' THEN 1 ELSE 0 END), 0) AS SIGNED) AS approved_count,
            CAST(COALESCE(SUM(CASE WHEN status = 'pending_review' THEN 1 ELSE 0 END), 0) AS SIGNED) AS pending_count,
            COUNT(*) AS candidate_count
         FROM harness_items
         WHERE harness_run_id = ?",
    )
    .bind(harness_run_id)
    .fetch_one(&mut **tx)
    .await
    .map_err(internal_error)?;
    let approved_count: i64 = row.try_get("approved_count").map_err(internal_error)?;
    let pending_count: i64 = row.try_get("pending_count").map_err(internal_error)?;
    let candidate_count: i64 = row.try_get("candidate_count").map_err(internal_error)?;
    let status = if pending_count > 0 {
        "waiting_for_review"
    } else {
        "reviewed"
    };
    sqlx::query(
        "UPDATE harness_runs SET status = ?, output_json = JSON_SET(output_json,
         '$.candidate_count', ?, '$.approved_count', ?), updated_at = NOW(6)
         WHERE harness_run_id = ?",
    )
    .bind(status)
    .bind(candidate_count)
    .bind(approved_count)
    .bind(harness_run_id)
    .execute(&mut **tx)
    .await
    .map_err(internal_error)?;
    Ok(())
}

async fn update_skillify_draft_counts(
    tx: &mut sqlx::Transaction<'_, MySql>,
    harness_run_id: &str,
) -> Result<(), (StatusCode, Json<ErrorResponse>)> {
    let row = sqlx::query(
        "SELECT
            CAST(COALESCE(SUM(CASE WHEN status = 'published' THEN 1 ELSE 0 END), 0) AS SIGNED) AS published_count,
            CAST(COALESCE(SUM(CASE WHEN status IN ('ready_to_publish', 'approved') THEN 1 ELSE 0 END), 0) AS SIGNED) AS ready_count,
            CAST(COALESCE(SUM(CASE WHEN status IN ('pending_rule_review', 'pending_skill_review', 'needs_revision') THEN 1 ELSE 0 END), 0) AS SIGNED) AS pending_count,
            COUNT(*) AS skill_draft_count
         FROM harness_skill_drafts
         WHERE harness_run_id = ?",
    )
    .bind(harness_run_id)
    .fetch_one(&mut **tx)
    .await
    .map_err(internal_error)?;
    let published_count: i64 = row.try_get("published_count").map_err(internal_error)?;
    let ready_count: i64 = row.try_get("ready_count").map_err(internal_error)?;
    let pending_count: i64 = row.try_get("pending_count").map_err(internal_error)?;
    let skill_draft_count: i64 = row.try_get("skill_draft_count").map_err(internal_error)?;
    let status = if skill_draft_count > 0 && published_count == skill_draft_count {
        "completed"
    } else if pending_count > 0 {
        "waiting_for_review"
    } else if ready_count > 0 {
        "reviewed"
    } else {
        "completed"
    };
    let rule_row = sqlx::query(
        "SELECT
            CAST(COALESCE(SUM(CASE WHEN status IN ('approved', 'edited') THEN 1 ELSE 0 END), 0) AS SIGNED) AS approved_rule_count,
            COUNT(*) AS rule_count
         FROM harness_skill_rules
         WHERE harness_run_id = ?",
    )
    .bind(harness_run_id)
    .fetch_one(&mut **tx)
    .await
    .map_err(internal_error)?;
    let approved_rule_count: i64 = rule_row
        .try_get("approved_rule_count")
        .map_err(internal_error)?;
    let rule_count: i64 = rule_row.try_get("rule_count").map_err(internal_error)?;
    sqlx::query(
        "UPDATE harness_runs SET status = ?, output_json = JSON_SET(output_json,
         '$.skill_draft_count', ?, '$.rule_count', ?, '$.approved_rule_count', ?,
         '$.published_count', ?), updated_at = NOW(6) WHERE harness_run_id = ?",
    )
    .bind(status)
    .bind(skill_draft_count)
    .bind(rule_count)
    .bind(approved_rule_count)
    .bind(published_count)
    .bind(harness_run_id)
    .execute(&mut **tx)
    .await
    .map_err(internal_error)?;
    Ok(())
}

async fn mark_authoring_evaluation_stale(
    tx: &mut sqlx::Transaction<'_, MySql>,
    harness_run_id: &str,
) -> HarnessResult<()> {
    sqlx::query("UPDATE harness_runs SET output_json = JSON_SET(output_json,
        '$.authoring.candidate_revision_id', NULL,
        '$.authoring.evaluation.status', 'stale',
        '$.authoring.evaluation.reason', 'The content changed during review; previous evaluation applies only to the earlier candidate.'), updated_at = NOW(6)
        WHERE harness_run_id = ? AND JSON_EXTRACT(output_json, '$.authoring') IS NOT NULL")
        .bind(harness_run_id).execute(&mut **tx).await.map_err(internal_error)?;
    Ok(())
}

async fn refresh_skill_draft_after_rule_decision(
    tx: &mut sqlx::Transaction<'_, MySql>,
    draft: &HarnessSkillDraftRecord,
    decision: &str,
    after: Option<&Value>,
) -> Result<(), (StatusCode, Json<ErrorResponse>)> {
    let harness_run_id = &draft.harness_run_id;
    let skill_draft_id = &draft.skill_draft_id;
    let edits_body = matches!(decision, "edit" | "reject");
    let previous = &draft.content_markdown;
    let content_markdown =
        if edits_body {
            after.and_then(|value| value.get("content_markdown")).and_then(Value::as_str)
            .filter(|value| !value.trim().is_empty())
            .ok_or_else(|| error_response(StatusCode::BAD_REQUEST,
                "rule edit or rejection requires the explicitly reviewed full content_markdown"))?
        } else {
            previous
        };
    let changed = content_markdown != previous.as_str();
    if decision == "reject" && !changed {
        return Err(error_response(
            StatusCode::BAD_REQUEST,
            "revise the full Skill body to remove the rejected rule before saving",
        ));
    }
    let status = derive_harness_skill_draft_status(&draft.rules);
    sqlx::query(
        "UPDATE harness_skill_drafts
         SET status = ?, content_markdown = IF(?, ?, content_markdown), revision = revision + ?, updated_at = NOW(6)
         WHERE harness_run_id = ? AND skill_draft_id = ?",
    )
    .bind(status)
    .bind(edits_body)
    .bind(content_markdown)
    .bind(i32::from(changed))
    .bind(harness_run_id)
    .bind(skill_draft_id)
    .execute(&mut **tx)
    .await
    .map_err(internal_error)?;
    if changed {
        mark_authoring_evaluation_stale(tx, harness_run_id).await?;
    }
    Ok(())
}

fn validate_skill_name(skill_name: &str) -> Result<(), (StatusCode, Json<ErrorResponse>)> {
    let valid = !skill_name.is_empty()
        && skill_name.len() <= 80
        && skill_name
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '-' || ch == '_');
    if valid {
        Ok(())
    } else {
        Err(error_response(
            StatusCode::BAD_REQUEST,
            "skill_name must contain only letters, numbers, hyphen, or underscore",
        ))
    }
}

fn validate_publish_visibility(visibility: &str) -> Result<(), (StatusCode, Json<ErrorResponse>)> {
    match visibility {
        "private" | "public" => Ok(()),
        _ => Err(error_response(
            StatusCode::BAD_REQUEST,
            "visibility must be private or public",
        )),
    }
}

fn validate_reviewed_revision(
    draft: &HarnessSkillDraftRecord,
    expected: Option<i64>,
) -> HarnessResult<()> {
    if expected != Some(draft.revision) {
        return Err(error_response(
            StatusCode::CONFLICT,
            "skill draft changed or expected_revision is missing; reload the draft and review its current revision",
        ));
    }
    Ok(())
}

fn stable_hash(text: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(text.as_bytes());
    format!("sha256:{:x}", hasher.finalize())
}

fn is_duplicate_key(error: &sqlx::Error) -> bool {
    matches!(
        error,
        sqlx::Error::Database(database_error)
            if database_error.code().as_deref() == Some("1062")
    )
}

fn skillify_harness_run_id(user_id: &str, idempotency_key: Option<&str>) -> String {
    idempotency_key
        .map(|key| format!("harness-run-{}", stable_hash(&format!("{user_id}:{key}"))))
        .unwrap_or_else(|| format!("harness-run-{}", Uuid::new_v4()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event() -> SkillifyEvent {
        SkillifyEvent {
            event_id: "evt-1".into(),
            session_id: "session-1".into(),
            source_id: "session-1".into(),
            source_type: "session".into(),
            title: "Test session".into(),
            event_type: "user_query".into(),
            content: "我更喜欢先给结论，再解释理由。".into(),
        }
    }

    fn agent_output(source_id: &str) -> SkillifyAgentOutput {
        SkillifyAgentOutput {
            extractor: "test-agent".into(),
            subagent_strategy: json!({"enabled": false}),
            drafts: vec![SkillifyAgentDraft {
                candidate_name: "test-skill".into(),
                description: "Test skill".into(),
                target_scope: "personal".into(),
                publish_visibility: "private".into(),
                content_markdown: "# Test Skill\n\n- Prefer concise answers.".into(),
                source_summary_json: json!({"source_count": 1}),
                confidence: Some(0.8),
                rules: vec![SkillifyAgentRule {
                    rule_type: "preference".into(),
                    statement: "Answer with the conclusion first.".into(),
                    rationale: "The source states this as a stable user preference.".into(),
                    confidence: Some(0.8),
                    citations: vec![SkillifyAgentCitation {
                        source_id: source_id.into(),
                        source_excerpt: "我更喜欢先给结论".into(),
                        source_locator_json: json!({"event_id": "evt-1"}),
                    }],
                }],
            }],
        }
    }

    #[test]
    fn validates_skill_name() {
        assert!(validate_skill_name("my-skill_1").is_ok());
        assert!(validate_skill_name("bad skill").is_err());
    }

    #[test]
    fn validates_agent_output_with_known_citation_source() {
        assert!(validate_skillify_agent_output(&agent_output("session-1"), &[event()]).is_ok());
    }

    #[test]
    fn rejects_agent_output_with_unknown_citation_source() {
        assert!(
            validate_skillify_agent_output(&agent_output("missing-source"), &[event()]).is_err()
        );
    }

    #[test]
    fn citation_must_match_original_text_and_offsets_are_utf8_bytes() {
        let mut output = agent_output("session-1");
        output.drafts[0].rules[0].citations[0].source_excerpt = "原文中不存在的成功结果".into();
        assert!(validate_skillify_agent_output(&output, &[event()]).is_err());
        let content = "中文前缀：exact quote。";
        let (start, end) = locate_source_excerpt(content, "exact quote").unwrap();
        assert_eq!(&content[start..end], "exact quote");
        assert_eq!(start, "中文前缀：".len());
        assert!(locate_source_excerpt(content, " ").is_err());
    }

    #[test]
    fn detects_existing_idempotency_key_in_decision_history() {
        let history = json!([
            {
                "decision": "approve",
                "idempotency_key": "idem-1"
            }
        ]);
        assert!(decision_history_contains_idempotency(&history, "idem-1"));
        assert!(!decision_history_contains_idempotency(&history, "idem-2"));
    }

    #[test]
    fn harness_skill_rule_status_helpers_keep_unresolved_and_approved_sets_distinct() {
        assert!(harness_skill_rule_blocks_draft_approval("conflicted"));
        assert!(harness_skill_rule_blocks_draft_approval("needs_revision"));
        assert!(harness_skill_rule_blocks_draft_approval("rejected"));
        assert!(!harness_skill_rule_blocks_draft_approval("proposed"));
        assert!(!harness_skill_rule_blocks_draft_approval("approved"));

        assert!(harness_skill_rule_is_unresolved("proposed"));
        assert!(harness_skill_rule_is_unresolved("conflicted"));
        assert!(harness_skill_rule_is_unresolved("needs_revision"));
        assert!(!harness_skill_rule_is_unresolved("approved"));
        assert!(!harness_skill_rule_is_unresolved("edited"));

        assert!(harness_skill_rule_is_approved("approved"));
        assert!(harness_skill_rule_is_approved("edited"));
        assert!(!harness_skill_rule_is_approved("rejected"));
    }

    #[test]
    fn derive_harness_skill_draft_status_prefers_unresolved_then_approved_then_rejected() {
        let mut rule = HarnessSkillRuleRecord {
            skill_rule_id: "rule-1".into(),
            skill_draft_id: "draft-1".into(),
            harness_run_id: "run-1".into(),
            rule_type: "preference".into(),
            statement: "lead with conclusion".into(),
            rationale: "stable preference".into(),
            decision_history_json: json!([]),
            status: "approved".into(),
            confidence: Some(0.8),
            source_count: 1,
            created_by_node_id: None,
            created_at: "now".into(),
            updated_at: "now".into(),
            citations: vec![],
        };

        assert_eq!(
            derive_harness_skill_draft_status(&[rule.clone()]),
            "ready_to_publish"
        );

        rule.status = "conflicted".into();
        assert_eq!(
            derive_harness_skill_draft_status(&[rule.clone()]),
            "pending_rule_review"
        );

        rule.status = "rejected".into();
        assert_eq!(derive_harness_skill_draft_status(&[rule]), "rejected");
        assert!(harness_skill_draft_is_publishable("ready_to_publish"));
        assert!(harness_skill_draft_is_publishable("approved"));
        assert!(!harness_skill_draft_is_publishable("pending_rule_review"));
    }
}
