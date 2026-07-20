use async_trait::async_trait;
use axum::{Json, http::StatusCode};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::{MySql, QueryBuilder, Row};
use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};
use uuid::Uuid;

use astra_core::{ErrorResponse, SharedPool, error_response, internal_error};

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
enum HarnessItemStatusKind {
    Approved,
    Other,
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

fn harness_item_status_kind(status: &str) -> HarnessItemStatusKind {
    match status {
        "approved" => HarnessItemStatusKind::Approved,
        _ => HarnessItemStatusKind::Other,
    }
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
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct HarnessDecisionRequest {
    pub decision: String,
    pub after_json: Option<Value>,
    pub reason: Option<String>,
    pub idempotency_key: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SkillifyDraftRequest {
    pub skill_name: Option<String>,
    pub version: Option<String>,
    pub description: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SkillifyDraftRecord {
    pub harness_run_id: String,
    pub skill_name: String,
    pub version_id: String,
    pub content_markdown: String,
    pub approved_item_count: usize,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SkillifyPublishRequest {
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

    async fn create_skillify_draft(
        &self,
        user_id: String,
        harness_run_id: String,
        request: SkillifyDraftRequest,
    ) -> Result<SkillifyDraftRecord, (StatusCode, Json<ErrorResponse>)>;
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
                    IFNULL(CAST(input_json AS CHAR), '{}') AS input_json,
                    IFNULL(CAST(output_json AS CHAR), '{}') AS output_json,
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
                    IFNULL(CAST(locator_json AS CHAR), '{}') AS locator_json,
                    IFNULL(CAST(input_json AS CHAR), '{}') AS input_json,
                    IFNULL(CAST(proposed_output_json AS CHAR), '{}') AS proposed_output_json,
                    IFNULL(CAST(final_output_json AS CHAR), '{}') AS final_output_json,
                    IFNULL(CAST(decision_history_json AS CHAR), '[]') AS decision_history_json,
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
                    IFNULL(CAST(source_summary_json AS CHAR), '{}') AS source_summary_json,
                    IFNULL(CAST(decision_history_json AS CHAR), '[]') AS decision_history_json,
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

    async fn load_skill_rules(
        &self,
        harness_run_id: &str,
        skill_draft_id: &str,
    ) -> Result<Vec<HarnessSkillRuleRecord>, (StatusCode, Json<ErrorResponse>)> {
        let rows = sqlx::query(
            "SELECT skill_rule_id, skill_draft_id, harness_run_id, rule_type, statement,
                    rationale, IFNULL(CAST(decision_history_json AS CHAR), '[]') AS decision_history_json,
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
                    source_id, IFNULL(CAST(source_locator_json AS CHAR), '{}') AS source_locator_json,
                    source_snapshot_ref, source_content_hash,
                    IFNULL(CAST(source_metadata_json AS CHAR), '{}') AS source_metadata_json,
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
                    IFNULL(CAST(locator_json AS CHAR), '{}') AS locator_json,
                    IFNULL(CAST(input_json AS CHAR), '{}') AS input_json,
                    IFNULL(CAST(proposed_output_json AS CHAR), '{}') AS proposed_output_json,
                    IFNULL(CAST(final_output_json AS CHAR), '{}') AS final_output_json,
                    IFNULL(CAST(decision_history_json AS CHAR), '[]') AS decision_history_json,
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
                    rationale, IFNULL(CAST(decision_history_json AS CHAR), '[]') AS decision_history_json,
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
                    IFNULL(CAST(source_summary_json AS CHAR), '{}') AS source_summary_json,
                    IFNULL(CAST(decision_history_json AS CHAR), '[]') AS decision_history_json,
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

    async fn load_skill_rule_locked(
        &self,
        tx: &mut sqlx::Transaction<'_, MySql>,
        harness_run_id: &str,
        skill_draft_id: &str,
        skill_rule_id: &str,
    ) -> Result<HarnessSkillRuleRecord, (StatusCode, Json<ErrorResponse>)> {
        let row = sqlx::query(
            "SELECT skill_rule_id, skill_draft_id, harness_run_id, rule_type, statement,
                    rationale, IFNULL(CAST(decision_history_json AS CHAR), '[]') AS decision_history_json,
                    status, confidence, source_count, created_by_node_id,
                    CAST(created_at AS CHAR) AS created_at,
                    CAST(updated_at AS CHAR) AS updated_at
             FROM harness_skill_rules
             WHERE harness_run_id = ? AND skill_draft_id = ? AND skill_rule_id = ?
             LIMIT 1 FOR UPDATE",
        )
        .bind(harness_run_id)
        .bind(skill_draft_id)
        .bind(skill_rule_id)
        .fetch_optional(&mut **tx)
        .await
        .map_err(internal_error)?;
        let row =
            row.ok_or_else(|| error_response(StatusCode::NOT_FOUND, "skill rule not found"))?;
        skill_rule_from_row(row)
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
    ) -> Result<Vec<SkillifyEvent>, (StatusCode, Json<ErrorResponse>)> {
        let mut builder = QueryBuilder::<MySql>::new(
            "SELECT event_id, session_id, event_type, content
             FROM agent_events
             WHERE user_id = ",
        );
        builder.push_bind(user_id);
        builder.push(" AND content IS NOT NULL AND content != '' AND session_id IN (");
        let mut separated = builder.separated(", ");
        for session_id in session_ids {
            separated.push_bind(session_id);
        }
        separated.push_unseparated(")");
        builder.push(" ORDER BY session_id ASC, created_at ASC LIMIT ");
        builder.push_bind(MAX_SKILLIFY_EVENTS);

        let rows = builder
            .build()
            .fetch_all(self.pool.get())
            .await
            .map_err(internal_error)?;

        rows.into_iter()
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
            .collect::<HarnessResult<Vec<_>>>()
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
        let session_ids = normalize_session_ids(request.session_ids);
        let source_files = self.normalize_source_files(request.source_files.clone())?;
        if session_ids.is_empty() && source_files.is_empty() {
            return Err(error_response(
                StatusCode::BAD_REQUEST,
                "select at least one session or source file",
            ));
        }
        self.validate_session_ownership(&user_id, &session_ids)
            .await?;
        let mut events = if session_ids.is_empty() {
            Vec::new()
        } else {
            self.load_skillify_events(&user_id, &session_ids).await?
        };
        events.extend(source_files);
        if events.is_empty() {
            return Err(error_response(
                StatusCode::BAD_REQUEST,
                "selected sources do not contain readable content",
            ));
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
        let harness_run_id = format!("harness-run-{}", Uuid::new_v4());
        let input_json = json!({
            "template_id": SKILLIFY_TEMPLATE_ID,
            "session_ids": &session_ids,
            "source_file_count": request.source_files.as_ref().map(Vec::len).unwrap_or(0),
            "skill_name": &request.skill_name,
            "topic": &request.topic,
            "target_scope": &target_scope
        });
        sqlx::query(
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
        .await
        .map_err(internal_error)?;

        let source_packets = events
            .iter()
            .map(skillify_source_packet_from_event)
            .collect::<Vec<_>>();
        let source_packet_index = source_packets
            .iter()
            .map(|packet| (packet.source_id.clone(), packet.clone()))
            .collect::<HashMap<_, _>>();
        let agent_output = match executor
            .synthesize_skill_drafts(SkillifyAgentRequest {
                user_id: user_id.clone(),
                harness_run_id: harness_run_id.clone(),
                skill_name: request.skill_name.clone(),
                topic: request.topic.clone(),
                target_scope: target_scope.clone(),
                source_packets,
            })
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
        if let Err(failure) = validate_skillify_agent_output(&agent_output, &events) {
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
        let output_json = json!({
            "extractor": agent_output.extractor,
            "subagent_strategy": agent_output.subagent_strategy,
            "skill_draft_count": agent_output.drafts.len(),
            "rule_count": rule_count,
            "approved_rule_count": 0,
            "draft_version_id": null
        });
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
                    let locator = merge_citation_locator(
                        citation.source_locator_json.clone(),
                        &citation.source_id,
                        index,
                    );
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
                    IFNULL(CAST(locator_json AS CHAR), '{}') AS locator_json,
                    IFNULL(CAST(input_json AS CHAR), '{}') AS input_json,
                    IFNULL(CAST(proposed_output_json AS CHAR), '{}') AS proposed_output_json,
                    IFNULL(CAST(final_output_json AS CHAR), '{}') AS final_output_json,
                    IFNULL(CAST(decision_history_json AS CHAR), '[]') AS decision_history_json,
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
        let skill_rule_link = skill_rule_review_item_ids(&current)?;
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

        if let Some((skill_draft_id, skill_rule_id)) = skill_rule_link {
            let current_rule = self
                .load_skill_rule_locked(&mut tx, &harness_run_id, &skill_draft_id, &skill_rule_id)
                .await?;
            let rule_status = skill_rule_status_for_item_decision(decision);
            let statement = final_output
                .get("statement")
                .and_then(Value::as_str)
                .unwrap_or(&current_rule.statement)
                .trim()
                .to_string();
            if statement.is_empty() {
                return Err(error_response(
                    StatusCode::BAD_REQUEST,
                    "rule statement must not be empty",
                ));
            }
            let rationale = final_output
                .get("rationale")
                .and_then(Value::as_str)
                .unwrap_or(&current_rule.rationale)
                .trim()
                .to_string();
            let rule_before_json = json!({
                "status": current_rule.status.clone(),
                "statement": current_rule.statement.clone(),
                "rationale": current_rule.rationale.clone(),
            });
            let rule_after_json = json!({
                "status": rule_status,
                "statement": statement.clone(),
                "rationale": rationale.clone(),
                "payload": final_output.clone(),
            });
            let rule_decision_history_json = append_decision_history(
                &current_rule.decision_history_json,
                decision_history_entry(
                    decision,
                    &user_id,
                    request.reason.as_deref(),
                    request.idempotency_key.as_deref(),
                    rule_before_json,
                    rule_after_json,
                ),
            );
            sqlx::query(
                "UPDATE harness_skill_rules
                 SET status = ?, statement = ?, rationale = ?, decision_history_json = ?, updated_at = NOW(6)
                 WHERE harness_run_id = ? AND skill_draft_id = ? AND skill_rule_id = ?",
            )
            .bind(rule_status)
            .bind(statement)
            .bind(rationale)
            .bind(rule_decision_history_json.to_string())
            .bind(&harness_run_id)
            .bind(&skill_draft_id)
            .bind(&skill_rule_id)
            .execute(&mut *tx)
            .await
            .map_err(internal_error)?;
            refresh_skill_draft_after_rule_decision(&mut tx, &harness_run_id, &skill_draft_id)
                .await?;
        }
        update_skillify_run_counts(&mut tx, &harness_run_id).await?;
        if skill_rule_review_item_ids(&current)?.is_some() {
            update_skillify_draft_counts(&mut tx, &harness_run_id).await?;
        }
        tx.commit().await.map_err(internal_error)?;
        self.load_item(&harness_run_id, &item_id).await
    }

    async fn list_skill_drafts(
        &self,
        user_id: String,
        harness_run_id: String,
    ) -> Result<Vec<HarnessSkillDraftRecord>, (StatusCode, Json<ErrorResponse>)> {
        self.ensure_run_owner(&user_id, &harness_run_id).await?;
        let rows = sqlx::query(
            "SELECT skill_draft_id, harness_run_id, candidate_name, description,
                    target_scope, publish_visibility, content_markdown,
                    IFNULL(CAST(source_summary_json AS CHAR), '{}') AS source_summary_json,
                    IFNULL(CAST(decision_history_json AS CHAR), '[]') AS decision_history_json,
                    status, confidence, created_by_node_id, revision, published_version_id,
                    CAST(created_at AS CHAR) AS created_at,
                    CAST(updated_at AS CHAR) AS updated_at
             FROM harness_skill_drafts
             WHERE harness_run_id = ?
             ORDER BY created_at ASC",
        )
        .bind(&harness_run_id)
        .fetch_all(self.pool.get())
        .await
        .map_err(internal_error)?;

        let mut drafts = Vec::with_capacity(rows.len());
        for row in rows {
            let mut draft = skill_draft_from_row(row)?;
            draft.rules = self
                .load_skill_rules(&harness_run_id, &draft.skill_draft_id)
                .await?;
            drafts.push(draft);
        }
        Ok(drafts)
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
            "revision": current.revision + 1,
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
             SET status = ?, content_markdown = ?, decision_history_json = ?, revision = revision + 1, updated_at = NOW(6)
             WHERE harness_run_id = ? AND skill_draft_id = ?",
        )
        .bind(status)
        .bind(content_markdown)
        .bind(decision_history_json.to_string())
        .bind(&harness_run_id)
        .bind(&skill_draft_id)
        .execute(&mut *tx)
        .await
        .map_err(internal_error)?;
        update_skillify_draft_counts(&mut tx, &harness_run_id).await?;
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
        let current = self
            .load_skill_rule_locked(&mut tx, &harness_run_id, &skill_draft_id, &skill_rule_id)
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
        refresh_skill_draft_after_rule_decision(&mut tx, &harness_run_id, &skill_draft_id).await?;
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
        let draft = self
            .load_skill_draft(&harness_run_id, &skill_draft_id)
            .await?;
        if !harness_skill_draft_is_publishable(&draft.status) {
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
        let version = request.version.unwrap_or_else(|| "0.1.0".to_string());
        let description = request
            .description
            .unwrap_or_else(|| draft.description.clone());
        let manifest = json!({
            "name": draft.candidate_name,
            "description": description,
            "version": version
        });
        let store = DatabasePersonalSkillStore::new(self.pool.clone());
        store
            .create_source(
                &user_id,
                CreateUserSkillSource {
                    skill_name: draft.candidate_name.clone(),
                    visibility: Some(visibility.clone()),
                },
            )
            .await
            .map_err(|e| error_response(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
        let version_record = store
            .submit_version(
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
            .map_err(|e| error_response(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

        let mut tx = self.pool.get().begin().await.map_err(internal_error)?;
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

    async fn create_skillify_draft(
        &self,
        user_id: String,
        harness_run_id: String,
        request: SkillifyDraftRequest,
    ) -> Result<SkillifyDraftRecord, (StatusCode, Json<ErrorResponse>)> {
        let run = self.ensure_run_owner(&user_id, &harness_run_id).await?;
        if run.harness_id != SKILLIFY_HARNESS_ID {
            return Err(error_response(
                StatusCode::BAD_REQUEST,
                "skill draft can only be created from a skillify harness run",
            ));
        }
        let items = self
            .list_run_items(user_id.clone(), harness_run_id.clone())
            .await?;
        let approved: Vec<_> = items
            .into_iter()
            .filter(|item| {
                harness_item_status_kind(&item.status) == HarnessItemStatusKind::Approved
            })
            .collect();
        if approved.is_empty() {
            return Err(error_response(
                StatusCode::BAD_REQUEST,
                "at least one approved skill candidate is required",
            ));
        }
        let skill_name = request
            .skill_name
            .or_else(|| {
                run.input_json
                    .get("skill_name")
                    .and_then(Value::as_str)
                    .map(ToString::to_string)
            })
            .unwrap_or_else(|| default_skill_name(&harness_run_id));
        validate_skill_name(&skill_name)?;
        let version = request.version.unwrap_or_else(|| "0.1.0".to_string());
        let description = request.description.unwrap_or_else(|| {
            "Draft skill generated from reviewed Skillify harness candidates.".to_string()
        });
        let content_markdown = render_skill_markdown(&skill_name, &description, &approved);
        let manifest = json!({
            "name": skill_name,
            "description": description,
            "version": version
        });
        let store = DatabasePersonalSkillStore::new(self.pool.clone());
        store
            .create_source(
                &user_id,
                CreateUserSkillSource {
                    skill_name: skill_name.clone(),
                    visibility: Some("private".to_string()),
                },
            )
            .await
            .map_err(|e| error_response(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
        let version_record = store
            .submit_version(
                &user_id,
                &skill_name,
                SubmitUserSkillVersion {
                    version,
                    manifest_json: manifest,
                    content_markdown: content_markdown.clone(),
                    status: Some("draft".to_string()),
                },
            )
            .await
            .map_err(|e| error_response(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

        let output_json = json!({
            "candidate_count": run.output_json.get("candidate_count").and_then(Value::as_u64).unwrap_or(approved.len() as u64),
            "approved_count": approved.len(),
            "draft_skill_name": skill_name,
            "draft_version_id": version_record.version_id
        });
        sqlx::query(
            "UPDATE harness_runs SET status = 'completed', output_json = ?, updated_at = NOW(6)
             WHERE harness_run_id = ?",
        )
        .bind(output_json.to_string())
        .bind(&harness_run_id)
        .execute(self.pool.get())
        .await
        .map_err(internal_error)?;

        Ok(SkillifyDraftRecord {
            harness_run_id,
            skill_name,
            version_id: version_record.version_id,
            content_markdown,
            approved_item_count: approved.len(),
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

    async fn create_skillify_draft(
        &self,
        _user_id: String,
        _harness_run_id: String,
        _request: SkillifyDraftRequest,
    ) -> Result<SkillifyDraftRecord, (StatusCode, Json<ErrorResponse>)> {
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

fn skill_rule_status_for_item_decision(decision: &str) -> &'static str {
    match decision {
        "approve" => "approved",
        "reject" => "rejected",
        "edit" => "edited",
        "request_revision" => "needs_revision",
        _ => "proposed",
    }
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

    let known_source_ids = events
        .iter()
        .map(|event| event.source_id.as_str())
        .collect::<HashSet<_>>();

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
                if !known_source_ids.contains(citation.source_id.as_str()) {
                    return Err(invalid_skillify_agent_output(format!(
                        "unknown citation source_id {}",
                        citation.source_id
                    )));
                }
                if citation.source_excerpt.trim().is_empty() {
                    return Err(invalid_skillify_agent_output(
                        "citation source_excerpt is empty",
                    ));
                }
            }
        }
    }

    Ok(())
}

fn unique_rule_source_count(rule: &SkillifyAgentRule) -> i64 {
    rule.citations
        .iter()
        .map(|citation| citation.source_id.as_str())
        .collect::<HashSet<_>>()
        .len() as i64
}

fn merge_citation_locator(locator: Value, source_id: &str, rule_index: usize) -> Value {
    let mut obj = locator.as_object().cloned().unwrap_or_default();
    obj.entry("source_id".to_string())
        .or_insert_with(|| json!(source_id));
    obj.insert("rule_index".to_string(), json!(rule_index));
    Value::Object(obj)
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
    let output_json = json!({
        "candidate_count": candidate_count,
        "approved_count": approved_count,
        "draft_version_id": null
    });
    sqlx::query(
        "UPDATE harness_runs SET status = ?, output_json = ?, updated_at = NOW(6)
         WHERE harness_run_id = ?",
    )
    .bind(status)
    .bind(output_json.to_string())
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
    let current_output_json: String = sqlx::query_scalar(
        "SELECT IFNULL(CAST(output_json AS CHAR), '{}') FROM harness_runs WHERE harness_run_id = ?",
    )
    .bind(harness_run_id)
    .fetch_one(&mut **tx)
    .await
    .map_err(internal_error)?;
    let mut output_json: Value =
        serde_json::from_str(&current_output_json).map_err(internal_error)?;
    let output = output_json.as_object_mut().ok_or_else(|| {
        internal_error("harness run output_json must be an object before updating Skillify counts")
    })?;
    output.insert("skill_draft_count".to_string(), json!(skill_draft_count));
    output.insert("rule_count".to_string(), json!(rule_count));
    output.insert(
        "approved_rule_count".to_string(),
        json!(approved_rule_count),
    );
    output.insert("published_count".to_string(), json!(published_count));
    sqlx::query(
        "UPDATE harness_runs SET status = ?, output_json = ?, updated_at = NOW(6)
         WHERE harness_run_id = ?",
    )
    .bind(status)
    .bind(output_json.to_string())
    .bind(harness_run_id)
    .execute(&mut **tx)
    .await
    .map_err(internal_error)?;
    Ok(())
}

async fn refresh_skill_draft_after_rule_decision(
    tx: &mut sqlx::Transaction<'_, MySql>,
    harness_run_id: &str,
    skill_draft_id: &str,
) -> Result<(), (StatusCode, Json<ErrorResponse>)> {
    let draft_row = sqlx::query(
        "SELECT candidate_name, description
         FROM harness_skill_drafts
         WHERE harness_run_id = ? AND skill_draft_id = ?
         LIMIT 1",
    )
    .bind(harness_run_id)
    .bind(skill_draft_id)
    .fetch_one(&mut **tx)
    .await
    .map_err(internal_error)?;
    let candidate_name =
        required_harness_string(&draft_row, "harness_skill_drafts", "candidate_name")?;
    let description = required_harness_string(&draft_row, "harness_skill_drafts", "description")?;
    let rule_rows = sqlx::query(
        "SELECT skill_rule_id, skill_draft_id, harness_run_id, rule_type, statement,
                rationale, IFNULL(CAST(decision_history_json AS CHAR), '[]') AS decision_history_json,
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
    let rules = rule_rows
        .into_iter()
        .map(skill_rule_from_row)
        .collect::<HarnessResult<Vec<_>>>()?;
    let status = derive_harness_skill_draft_status(&rules);
    let content_markdown = render_skill_markdown_from_rules(&candidate_name, &description, &rules);
    sqlx::query(
        "UPDATE harness_skill_drafts
         SET status = ?, content_markdown = ?, revision = revision + 1, updated_at = NOW(6)
         WHERE harness_run_id = ? AND skill_draft_id = ?",
    )
    .bind(status)
    .bind(content_markdown)
    .bind(harness_run_id)
    .bind(skill_draft_id)
    .execute(&mut **tx)
    .await
    .map_err(internal_error)?;
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

fn default_skill_name(harness_run_id: &str) -> String {
    let suffix = harness_run_id
        .rsplit('-')
        .next()
        .unwrap_or(harness_run_id)
        .chars()
        .take(8)
        .collect::<String>();
    format!("skillify-{suffix}")
}

fn render_skill_markdown(
    skill_name: &str,
    description: &str,
    approved: &[HarnessItemRecord],
) -> String {
    let mut lines = vec![
        "---".to_string(),
        format!("name: {skill_name}"),
        format!("description: {description}"),
        "version: \"0.1.0\"".to_string(),
        "---".to_string(),
        String::new(),
        "# Skill Instructions".to_string(),
        String::new(),
        "Use these reviewed preferences when they are relevant to the user's current task."
            .to_string(),
        String::new(),
    ];
    for item in approved {
        if let Some(statement) = item
            .final_output_json
            .get("statement")
            .and_then(Value::as_str)
            .or_else(|| {
                item.proposed_output_json
                    .get("statement")
                    .and_then(Value::as_str)
            })
        {
            lines.push(format!("- {}", statement.trim()));
        }
    }
    lines.push(String::new());
    lines.push("Do not apply these preferences when they conflict with explicit user instructions in the current conversation.".to_string());
    lines.join("\n")
}

fn render_skill_markdown_from_rules(
    skill_name: &str,
    description: &str,
    rules: &[HarnessSkillRuleRecord],
) -> String {
    let mut lines = vec![
        "---".to_string(),
        format!("name: {skill_name}"),
        format!("description: {description}"),
        "version: \"0.1.0\"".to_string(),
        "---".to_string(),
        String::new(),
        "# Skill Instructions".to_string(),
        String::new(),
        description.to_string(),
        String::new(),
        "## Rules".to_string(),
        String::new(),
    ];
    for rule in rules.iter().filter(|rule| {
        !matches!(
            harness_skill_rule_status_kind(&rule.status),
            HarnessSkillRuleStatusKind::Rejected | HarnessSkillRuleStatusKind::NeedsRevision
        )
    }) {
        let statement = rule.statement.trim();
        if !statement.is_empty() {
            lines.push(format!("- {statement}"));
        }
    }
    lines.push(String::new());
    lines.push("## Guardrails".to_string());
    lines.push(String::new());
    lines.push("- Apply these rules only when they are relevant to the current task.".to_string());
    lines.push(
        "- Do not apply these rules when they conflict with explicit current user instructions."
            .to_string(),
    );
    lines.join("\n")
}

fn stable_hash(text: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(text.as_bytes());
    format!("sha256:{:x}", hasher.finalize())
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
