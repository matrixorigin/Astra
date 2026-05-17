use async_trait::async_trait;
use axum::{Json, http::StatusCode};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::{MySql, QueryBuilder, Row};
use uuid::Uuid;

use astra_core::{ErrorResponse, SharedPool, error_response, internal_error};

use crate::personal_skills::{
    CreateUserSkillSource, DatabasePersonalSkillStore, SubmitUserSkillVersion,
};

const SKILLIFY_HARNESS_ID: &str = "skillify";
const SKILLIFY_VERSION_ID: &str = "skillify.v1";
const SKILLIFY_TEMPLATE_ID: &str = "skillify.v1";
const MAX_SKILLIFY_SESSIONS: usize = 20;
const MAX_SKILLIFY_EVENTS: i64 = 2_000;
const MAX_SKILLIFY_CANDIDATES: usize = 40;

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
    pub status: String,
    pub confidence: Option<f64>,
    pub assigned_to: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SkillifyRunRequest {
    pub session_ids: Vec<String>,
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
}

impl DatabaseHarnessService {
    pub fn new(pool: SharedPool) -> Self {
        Self { pool }
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
            harness_run_id: row.try_get("harness_run_id").map_err(internal_error)?,
            harness_id: row.try_get("harness_id").map_err(internal_error)?,
            version_id: row.try_get("version_id").map_err(internal_error)?,
            user_id: row.try_get("user_id").map_err(internal_error)?,
            session_id: row.try_get("session_id").ok(),
            status: row.try_get("status").map_err(internal_error)?,
            input_json: parse_json_cell(&row, "input_json"),
            output_json: parse_json_cell(&row, "output_json"),
            error: row.try_get("error").ok(),
            created_at: row.try_get("created_at").unwrap_or_default(),
            updated_at: row.try_get("updated_at").unwrap_or_default(),
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
        Ok(item_from_row(row))
    }

    async fn validate_session_ownership(
        &self,
        user_id: &str,
        session_ids: &[String],
    ) -> Result<(), (StatusCode, Json<ErrorResponse>)> {
        if session_ids.is_empty() {
            return Err(error_response(
                StatusCode::BAD_REQUEST,
                "session_ids is required",
            ));
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

        Ok(rows
            .into_iter()
            .map(|row| SkillifyEvent {
                event_id: row.try_get("event_id").unwrap_or_default(),
                session_id: row.try_get("session_id").unwrap_or_default(),
                event_type: row.try_get("event_type").unwrap_or_default(),
                content: row.try_get("content").unwrap_or_default(),
            })
            .collect())
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
        self.validate_session_ownership(&user_id, &session_ids)
            .await?;
        let events = self.load_skillify_events(&user_id, &session_ids).await?;
        if events.is_empty() {
            return Err(error_response(
                StatusCode::BAD_REQUEST,
                "selected sessions do not contain readable events",
            ));
        }

        let candidates = extract_skillify_candidates(&events, MAX_SKILLIFY_CANDIDATES);
        let harness_run_id = format!("harness-run-{}", Uuid::new_v4());
        let input_json = json!({
            "template_id": SKILLIFY_TEMPLATE_ID,
            "session_ids": session_ids,
            "skill_name": request.skill_name,
            "topic": request.topic,
            "target_scope": request.target_scope.clone().unwrap_or_else(|| "personal".to_string())
        });
        let output_json = json!({
            "candidate_count": candidates.len(),
            "approved_count": 0,
            "draft_version_id": null
        });
        let status = if candidates.is_empty() {
            "completed"
        } else {
            "waiting_for_review"
        };

        let mut tx = self.pool.get().begin().await.map_err(internal_error)?;
        sqlx::query(
            "INSERT INTO harness_runs
             (harness_run_id, harness_id, version_id, user_id, session_id, status,
              input_json, output_json, created_at, updated_at)
             VALUES (?, ?, ?, ?, NULL, ?, ?, ?, NOW(6), NOW(6))",
        )
        .bind(&harness_run_id)
        .bind(SKILLIFY_HARNESS_ID)
        .bind(SKILLIFY_VERSION_ID)
        .bind(&user_id)
        .bind(status)
        .bind(input_json.to_string())
        .bind(output_json.to_string())
        .execute(&mut *tx)
        .await
        .map_err(internal_error)?;

        for session_id in input_json["session_ids"].as_array().into_iter().flatten() {
            let Some(session_id) = session_id.as_str() else {
                continue;
            };
            sqlx::query(
                "INSERT INTO harness_sources
                 (source_id, harness_run_id, source_type, source_ref, snapshot_ref, metadata_json, status, created_at)
                 VALUES (?, ?, 'sessions', ?, ?, ?, 'ready', NOW(6))",
            )
            .bind(format!("harness-source-{}", Uuid::new_v4()))
            .bind(&harness_run_id)
            .bind(session_id)
            .bind(session_id)
            .bind(json!({"session_id": session_id}).to_string())
            .execute(&mut *tx)
            .await
            .map_err(internal_error)?;
        }

        for (index, candidate) in candidates.iter().enumerate() {
            let item_id = format!("harness-item-{}", Uuid::new_v4());
            let citation_id = format!("harness-citation-{}", Uuid::new_v4());
            let locator = json!({
                "session_id": candidate.session_id,
                "event_id": candidate.event_id,
                "event_type": candidate.event_type,
                "candidate_index": index
            });
            let proposed = json!({
                "kind": candidate.kind,
                "statement": candidate.statement,
                "source_excerpt": candidate.source_excerpt,
                "citations": [citation_id]
            });
            sqlx::query(
                "INSERT INTO harness_items
                 (item_id, harness_run_id, item_type, locator_json, input_json,
                  proposed_output_json, final_output_json, status, confidence, created_at, updated_at)
                 VALUES (?, ?, 'skill_candidate', ?, ?, ?, '{}', 'pending_review', ?, NOW(6), NOW(6))",
            )
            .bind(&item_id)
            .bind(&harness_run_id)
            .bind(locator.to_string())
            .bind(json!({"source_excerpt": candidate.source_excerpt}).to_string())
            .bind(proposed.to_string())
            .bind(candidate.confidence)
            .execute(&mut *tx)
            .await
            .map_err(internal_error)?;

            sqlx::query(
                "INSERT INTO harness_citations
                 (citation_id, harness_run_id, item_id, source_id, source_locator_json,
                  quote_hash, evidence_text_preview, relevance_score, created_by_node_id, created_at)
                 VALUES (?, ?, ?, NULL, ?, ?, ?, ?, 'agent.extract_skill_candidates', NOW(6))",
            )
            .bind(citation_id)
            .bind(&harness_run_id)
            .bind(&item_id)
            .bind(locator.to_string())
            .bind(stable_hash(&candidate.source_excerpt))
            .bind(&candidate.source_excerpt)
            .bind(candidate.confidence)
            .execute(&mut *tx)
            .await
            .map_err(internal_error)?;
        }

        tx.commit().await.map_err(internal_error)?;
        self.load_run(&harness_run_id).await
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
        Ok(rows.into_iter().map(item_from_row).collect())
    }

    async fn decide_item(
        &self,
        user_id: String,
        harness_run_id: String,
        item_id: String,
        request: HarnessDecisionRequest,
    ) -> Result<HarnessItemRecord, (StatusCode, Json<ErrorResponse>)> {
        self.ensure_run_owner(&user_id, &harness_run_id).await?;
        let current = self.load_item(&harness_run_id, &item_id).await?;
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
        let decision_id = format!("harness-decision-{}", Uuid::new_v4());
        let idempotency_key = request
            .idempotency_key
            .unwrap_or_else(|| format!("{}:{}", item_id, decision_id));
        let mut tx = self.pool.get().begin().await.map_err(internal_error)?;
        sqlx::query(
            "INSERT INTO harness_decisions
             (decision_id, harness_run_id, item_id, reviewer_user_id, decision,
              before_json, after_json, reason, idempotency_key, created_at)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, NOW(6))",
        )
        .bind(&decision_id)
        .bind(&harness_run_id)
        .bind(&item_id)
        .bind(&user_id)
        .bind(decision)
        .bind(current.proposed_output_json.to_string())
        .bind(final_output.to_string())
        .bind(request.reason.unwrap_or_default())
        .bind(idempotency_key)
        .execute(&mut *tx)
        .await
        .map_err(internal_error)?;
        sqlx::query(
            "UPDATE harness_items
             SET status = ?, final_output_json = ?, updated_at = NOW(6)
             WHERE harness_run_id = ? AND item_id = ?",
        )
        .bind(status)
        .bind(final_output.to_string())
        .bind(&harness_run_id)
        .bind(&item_id)
        .execute(&mut *tx)
        .await
        .map_err(internal_error)?;
        update_skillify_run_counts(&mut tx, &harness_run_id).await?;
        tx.commit().await.map_err(internal_error)?;
        self.load_item(&harness_run_id, &item_id).await
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
            .filter(|item| item.status == "approved")
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
    event_type: String,
    content: String,
}

#[derive(Clone, Debug, PartialEq)]
struct SkillifyCandidate {
    event_id: String,
    session_id: String,
    event_type: String,
    kind: String,
    statement: String,
    source_excerpt: String,
    confidence: f64,
}

fn skillify_template() -> HarnessTemplateRecord {
    HarnessTemplateRecord {
        template_id: SKILLIFY_TEMPLATE_ID.to_string(),
        name: "Skillify from sessions".to_string(),
        description: "Extract reviewed, reusable personal skill rules from selected chat sessions."
            .to_string(),
        built_in: true,
        input_schema_json: json!({
            "type": "object",
            "required": ["session_ids"],
            "properties": {
                "session_ids": {"type": "array", "items": {"type": "string"}, "minItems": 1},
                "skill_name": {"type": "string"},
                "topic": {"type": "string"},
                "target_scope": {"type": "string", "enum": ["personal", "project"]}
            }
        }),
        workflow_json: json!({
            "nodes": [
                "source.snapshot_sessions",
                "agent.extract_skill_candidates",
                "validate.skill_candidates",
                "human.review",
                "skill.draft_from_sessions",
                "skill.validate_draft",
                "human.publish_decision"
            ]
        }),
    }
}

fn node_catalog() -> Vec<HarnessNodeCatalogRecord> {
    vec![
        node(
            "source.snapshot_sessions",
            "Snapshot selected session events",
        ),
        node(
            "agent.extract_skill_candidates",
            "Extract candidate skill rules from sessions",
        ),
        node(
            "validate.skill_candidates",
            "Validate candidate schema and source citation",
        ),
        node("human.review", "Review and approve candidate rules"),
        node(
            "skill.draft_from_sessions",
            "Create a draft user skill from approved rules",
        ),
        node(
            "skill.validate_draft",
            "Validate generated skill manifest and markdown",
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

fn extract_skillify_candidates(
    events: &[SkillifyEvent],
    max_candidates: usize,
) -> Vec<SkillifyCandidate> {
    let mut out = Vec::new();
    for event in events {
        if out.len() >= max_candidates {
            break;
        }
        if !is_user_like_event(&event.event_type) {
            continue;
        }
        for statement in candidate_statements(&event.content) {
            if out.len() >= max_candidates {
                break;
            }
            let kind = classify_candidate(&statement);
            out.push(SkillifyCandidate {
                event_id: event.event_id.clone(),
                session_id: event.session_id.clone(),
                event_type: event.event_type.clone(),
                kind,
                statement: normalize_candidate_statement(&statement),
                source_excerpt: excerpt(&event.content),
                confidence: 0.72,
            });
        }
    }
    dedupe_candidates(out)
}

fn is_user_like_event(event_type: &str) -> bool {
    let event_type = event_type.to_ascii_lowercase();
    event_type.contains("user") || event_type.contains("query") || event_type.contains("message")
}

fn candidate_statements(content: &str) -> Vec<String> {
    let lower = content.to_lowercase();
    let patterns = [
        "我喜欢",
        "我不喜欢",
        "我希望",
        "我更喜欢",
        "不要",
        "别",
        "以后",
        "下次",
        "偏好",
        "风格",
        "习惯",
        "优先",
        "i prefer",
        "i like",
        "i don't like",
        "don't",
        "always",
        "never",
        "next time",
        "please use",
        "style",
        "prefer",
    ];
    if !patterns.iter().any(|pattern| lower.contains(pattern)) {
        return Vec::new();
    }
    split_sentences(content)
        .into_iter()
        .filter(|sentence| {
            let lower = sentence.to_lowercase();
            patterns.iter().any(|pattern| lower.contains(pattern))
        })
        .take(3)
        .collect()
}

fn split_sentences(content: &str) -> Vec<String> {
    content
        .split(['\n', '。', '！', '？', '.', '!', '?'])
        .map(str::trim)
        .filter(|line| line.chars().count() >= 6)
        .map(|line| line.chars().take(240).collect::<String>())
        .collect()
}

fn classify_candidate(statement: &str) -> String {
    let lower = statement.to_lowercase();
    if lower.contains("不要")
        || lower.contains("别")
        || lower.contains("don't")
        || lower.contains("never")
        || lower.contains("不喜欢")
    {
        "negative_preference".to_string()
    } else if lower.contains("风格") || lower.contains("style") {
        "style_preference".to_string()
    } else {
        "workflow_preference".to_string()
    }
}

fn normalize_candidate_statement(statement: &str) -> String {
    statement.trim().replace(char::is_whitespace, " ")
}

fn excerpt(content: &str) -> String {
    content.trim().chars().take(360).collect()
}

fn dedupe_candidates(candidates: Vec<SkillifyCandidate>) -> Vec<SkillifyCandidate> {
    let mut out = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for candidate in candidates {
        let key = candidate.statement.to_lowercase();
        if seen.insert(key) {
            out.push(candidate);
        }
    }
    out
}

fn item_from_row(row: sqlx::mysql::MySqlRow) -> HarnessItemRecord {
    HarnessItemRecord {
        item_id: row.try_get("item_id").unwrap_or_default(),
        harness_run_id: row.try_get("harness_run_id").unwrap_or_default(),
        item_type: row.try_get("item_type").unwrap_or_default(),
        locator_json: parse_json_cell(&row, "locator_json"),
        input_json: parse_json_cell(&row, "input_json"),
        proposed_output_json: parse_json_cell(&row, "proposed_output_json"),
        final_output_json: parse_json_cell(&row, "final_output_json"),
        status: row.try_get("status").unwrap_or_default(),
        confidence: row.try_get("confidence").ok(),
        assigned_to: row.try_get("assigned_to").ok(),
        created_at: row.try_get("created_at").unwrap_or_default(),
        updated_at: row.try_get("updated_at").unwrap_or_default(),
    }
}

fn parse_json_cell(row: &sqlx::mysql::MySqlRow, column: &str) -> Value {
    let text: String = row.try_get(column).unwrap_or_else(|_| "{}".to_string());
    serde_json::from_str(&text).unwrap_or_else(|_| json!({}))
}

async fn update_skillify_run_counts(
    tx: &mut sqlx::Transaction<'_, MySql>,
    harness_run_id: &str,
) -> Result<(), (StatusCode, Json<ErrorResponse>)> {
    let row = sqlx::query(
        "SELECT
            SUM(CASE WHEN status = 'approved' THEN 1 ELSE 0 END) AS approved_count,
            SUM(CASE WHEN status = 'pending_review' THEN 1 ELSE 0 END) AS pending_count,
            COUNT(*) AS candidate_count
         FROM harness_items
         WHERE harness_run_id = ?",
    )
    .bind(harness_run_id)
    .fetch_one(&mut **tx)
    .await
    .map_err(internal_error)?;
    let approved_count: i64 = row.try_get("approved_count").unwrap_or(0);
    let pending_count: i64 = row.try_get("pending_count").unwrap_or(0);
    let candidate_count: i64 = row.try_get("candidate_count").unwrap_or(0);
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

fn stable_hash(text: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(text.as_bytes());
    format!("sha256:{:x}", hasher.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(content: &str) -> SkillifyEvent {
        SkillifyEvent {
            event_id: "evt-1".into(),
            session_id: "session-1".into(),
            event_type: "user_query".into(),
            content: content.into(),
        }
    }

    #[test]
    fn extracts_explicit_user_preferences() {
        let candidates = extract_skillify_candidates(
            &[event("我不喜欢太长的解释。以后回答先给结论，再给理由。")],
            10,
        );
        assert_eq!(candidates.len(), 2);
        assert_eq!(candidates[0].kind, "negative_preference");
        assert!(candidates[1].statement.contains("以后回答先给结论"));
    }

    #[test]
    fn ignores_content_without_preference_signal() {
        let candidates = extract_skillify_candidates(&[event("帮我总结一下这篇论文的贡献。")], 10);
        assert!(candidates.is_empty());
    }

    #[test]
    fn validates_skill_name() {
        assert!(validate_skill_name("my-skill_1").is_ok());
        assert!(validate_skill_name("bad skill").is_err());
    }
}
