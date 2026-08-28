use crate::db_row::RowExt as ContextManifestDbRow;
use astra_core::SharedPool;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;
use uuid::Uuid;

pub const BUDGET_V1_8K_TOTAL_CAP: u32 = 7_300;
pub const BUDGET_V1_8K_PROMPT_CAP: u32 = 8_000;
pub const DELEGATION_ZONE_CAP: u32 = 1_500;
pub const DELEGATION_BLOCKER_ZONE_CAP: u32 = DELEGATION_ZONE_CAP * 2;
pub const DELEGATION_CHILD_FLOOR: u32 = 200;
pub const RECENT_TAIL_BLOCKER_FLOOR: u32 = 1_600;
pub const BENCHMARK_TOOL_PREVIEW_BUDGET: u32 = 2_500;
pub const RECENT_TAIL_BENCHMARK_FLOOR: u32 = 1_600;
pub const SYSTEM_TOOL_SCHEMAS_MAX: u32 = 3_400;
pub const TURN_INTENT_BENCHMARK_COMPARISON: &str = "benchmark_comparison";
pub const DELEGATION_MAX_RENDERED_CHILDREN: usize =
    (DELEGATION_ZONE_CAP / DELEGATION_CHILD_FLOOR) as usize;
pub const SESSION_ARTIFACT_STATUS_EXPIRED: &str = "expired";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionArtifactStatusKind {
    Expired,
    Other,
}

pub fn session_artifact_status_kind(status: &str) -> SessionArtifactStatusKind {
    match status {
        SESSION_ARTIFACT_STATUS_EXPIRED => SessionArtifactStatusKind::Expired,
        _ => SessionArtifactStatusKind::Other,
    }
}

pub fn session_artifact_raw_payload_is_available(status: &str) -> bool {
    session_artifact_status_kind(status) != SessionArtifactStatusKind::Expired
}

pub const CONTEXT_MANIFEST_REASONS: &[(&str, &str, Option<&str>)] = &[
    ("initial_turn", "lifecycle", Some("session_anchor")),
    ("normal_turn", "lifecycle", Some("recent_tail")),
    ("post_compaction", "compaction", Some("summary")),
    (
        "history_recall_structured",
        "retrieval",
        Some("retrieved_facts"),
    ),
    ("history_recall_fts", "retrieval", Some("retrieved_facts")),
    (
        "history_recall_vector",
        "retrieval",
        Some("retrieved_facts"),
    ),
    ("large_tool_output_gated", "artifact", Some("tool_previews")),
    ("plan_subtree_query", "plan", Some("plan_todo")),
    ("tree_structured_report", "plan", Some("plan_todo")),
    ("workspace_switch", "workspace", Some("workspace")),
    ("approval_resume", "approval", Some("safety_approvals")),
    ("cross_session_recall", "retrieval", Some("retrieved_facts")),
    ("delegation_poll", "delegation", Some("delegation_state")),
    (
        "partial_blocker_review",
        "delegation",
        Some("delegation_state"),
    ),
    (
        "delegation_aggregate",
        "delegation",
        Some("delegation_state"),
    ),
    ("cross_skill_alignment", "skills", Some("skills")),
    ("skill_quality_review", "skills", Some("skills")),
    ("final_delivery_summary", "lifecycle", Some("summary")),
    ("ambiguity_clarification", "next_action", Some("plan_todo")),
    (
        "execute_after_clarification",
        "next_action",
        Some("plan_todo"),
    ),
    ("user_memory_promote", "memory", Some("session_anchor")),
    ("user_memory_archive", "memory", Some("session_anchor")),
    ("user_memory_revise", "memory", Some("session_anchor")),
    (
        "user_memory_loaded_on_init",
        "memory",
        Some("session_anchor"),
    ),
    ("progressive_loading", "budget", None),
    (
        "intent_driven_preview_expand",
        "budget",
        Some("tool_previews"),
    ),
    ("other", "fallback", None),
];

pub const BASELINE_PREVIEW_TEMPLATES: &[(&str, u32, &str)] = &[
    ("bash", 1200, "shell_v1"),
    ("run_script", 1200, "python_stdout_v1"),
    ("read_file", 1000, "text_v1"),
    ("write_file", 1000, "text_v1"),
    ("str_replace", 1000, "diff_v1"),
    ("list_dir", 1200, "text_v1"),
    ("glob", 1200, "text_v1"),
    ("grep", 1200, "text_v1"),
    ("symbols", 1200, "rust_v1"),
    ("web_search", 1000, "search_v1"),
    ("web_fetch", 1200, "html_v1"),
    ("tool_search", 1000, "text_v1"),
    ("session", 1200, "text_v1"),
    ("task_board", 1200, "text_v1"),
    ("agent", 1200, "text_v1"),
    ("agent_fanout", 1600, "text_v1"),
    ("memory", 1200, "text_v1"),
    ("mo_query", 1200, "sql_v1"),
    ("pg_dump", 1000, "sql_v1"),
    ("fetch_url", 1000, "html_v1"),
    ("parse_pdf", 1000, "pdf_v1"),
    ("SKILL.md", 1200, "skill_md_v1"),
    ("cargo", 1200, "rust_v1"),
    ("rustc", 1200, "rust_v1"),
    ("clippy", 1200, "rust_v1"),
    ("pg_schema_structurize", 1200, "sql_v1"),
    ("slow_query_analyzer", 1200, "sql_v1"),
    ("curl", 1000, "text_v1"),
    ("git", 1200, "diff_v1"),
    ("docker_logs", 1200, "text_v1"),
    ("kubectl", 1200, "text_v1"),
    ("python_stdout", 1200, "text_v1"),
    ("npm_build", 1200, "js_v1"),
    ("csv_head", 1200, "csv_v1"),
    ("json_preview", 1200, "json_v1"),
    ("markdown_preview", 1200, "markdown_v1"),
];

pub fn preview_template_fts_field_weights(normalize_version: &str) -> &'static str {
    match normalize_version {
        "sql_v1" => r#"{"statement":2.0,"object_name":1.5,"error":2.0,"preview_text":1.0}"#,
        "rust_v1" => r#"{"diagnostic":2.0,"crate":1.4,"file":1.3,"preview_text":1.0}"#,
        "skill_md_v1" => r#"{"name":2.0,"description":1.6,"trigger":1.4,"preview_text":1.0}"#,
        "json_v1" => r#"{"path":1.8,"key":1.5,"value":1.0,"preview_text":1.0}"#,
        "csv_v1" => r#"{"header":1.8,"sample":1.2,"preview_text":1.0}"#,
        "diff_v1" => r#"{"path":1.7,"symbol":1.4,"hunk":1.2,"preview_text":1.0}"#,
        "html_v1" => r#"{"title":1.8,"heading":1.5,"url":1.2,"preview_text":1.0}"#,
        "pdf_v1" => r#"{"title":1.8,"section":1.5,"preview_text":1.0}"#,
        "js_v1" => r#"{"package":1.6,"script":1.4,"error":2.0,"preview_text":1.0}"#,
        "markdown_v1" => r#"{"heading":1.7,"link":1.2,"preview_text":1.0}"#,
        "artifact_file_v1" => {
            r#"{"title":2.0,"filename":1.8,"content_type":1.3,"preview_text":1.0}"#
        }
        "shell_v1" => r#"{"command":1.6,"stderr":2.0,"stdout":1.0,"preview_text":1.0}"#,
        "python_stdout_v1" => r#"{"script":1.5,"stdout":1.0,"stderr":2.0,"preview_text":1.0}"#,
        "search_v1" => r#"{"query":2.0,"title":1.5,"snippet":1.2,"preview_text":1.0}"#,
        _ => r#"{"preview_text":1.0,"tool_name":1.2,"error":1.8}"#,
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BudgetV1_8k {
    pub anchor: u32,
    pub plan_todo: u32,
    pub recent_tail: u32,
    pub summary: u32,
    pub retrieved: u32,
    pub tool_previews: u32,
    pub system_tool_schemas: u32,
    pub reserved_output: u32,
    pub safety_buffer: u32,
}

impl BudgetV1_8k {
    pub fn standard() -> Self {
        Self {
            anchor: 200,
            plan_todo: 400,
            recent_tail: 2000,
            summary: 500,
            retrieved: 1000,
            tool_previews: 500,
            system_tool_schemas: SYSTEM_TOOL_SCHEMAS_MAX,
            reserved_output: 500,
            safety_buffer: 200,
        }
    }

    pub fn prompt_cap(&self) -> u32 {
        self.anchor
            + self.plan_todo
            + self.recent_tail
            + self.summary
            + self.retrieved
            + self.tool_previews
            + self.system_tool_schemas
    }

    pub fn input_context_cap(&self) -> u32 {
        self.prompt_cap()
            .saturating_sub(self.reserved_output + self.safety_buffer)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TurnIntentBudgetAllocation {
    pub budget: BudgetV1_8k,
    pub borrowed_from_recent_tail: u32,
    pub flex_applied: bool,
}

pub fn budget_for_turn_intent(turn_intent: Option<&str>) -> TurnIntentBudgetAllocation {
    let mut budget = BudgetV1_8k::standard();
    if turn_intent == Some(TURN_INTENT_BENCHMARK_COMPARISON) {
        let borrowed = budget
            .recent_tail
            .saturating_sub(RECENT_TAIL_BENCHMARK_FLOOR);
        budget.recent_tail = RECENT_TAIL_BENCHMARK_FLOOR;
        budget.tool_previews = BENCHMARK_TOOL_PREVIEW_BUDGET;
        let overflow = budget.prompt_cap().saturating_sub(BUDGET_V1_8K_PROMPT_CAP);
        budget.system_tool_schemas = budget.system_tool_schemas.saturating_sub(overflow);
        return TurnIntentBudgetAllocation {
            budget,
            borrowed_from_recent_tail: borrowed,
            flex_applied: true,
        };
    }

    TurnIntentBudgetAllocation {
        budget,
        borrowed_from_recent_tail: 0,
        flex_applied: false,
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DelegationBudget {
    pub active_children: usize,
    pub rendered_children: usize,
    pub overflow_children: usize,
    pub per_child_budget: u32,
    pub rendered_total: u32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DelegationBudgetAllocation {
    pub child_budget: DelegationBudget,
    pub requested_delegation_zone_budget: u32,
    pub delegation_zone_budget: u32,
    pub recent_tail_budget: u32,
    pub borrowed_from_recent_tail: u32,
    pub unfunded_blocker_tokens: u32,
    pub blocker_active: bool,
}

pub fn delegation_budget(active_children: usize) -> DelegationBudget {
    if active_children == 0 {
        return DelegationBudget {
            active_children,
            rendered_children: 0,
            overflow_children: 0,
            per_child_budget: 0,
            rendered_total: 0,
        };
    }
    let rendered_children = active_children.min(DELEGATION_MAX_RENDERED_CHILDREN);
    let per_child_budget =
        DELEGATION_CHILD_FLOOR.max(DELEGATION_ZONE_CAP / rendered_children as u32);
    DelegationBudget {
        active_children,
        rendered_children,
        overflow_children: active_children.saturating_sub(rendered_children),
        per_child_budget,
        rendered_total: per_child_budget * rendered_children as u32,
    }
}

pub fn delegation_budget_allocation(
    active_children: usize,
    blocker_children: usize,
) -> DelegationBudgetAllocation {
    let base = BudgetV1_8k::standard();
    let blocker_active = blocker_children > 0;
    let requested = if blocker_active {
        DELEGATION_BLOCKER_ZONE_CAP
    } else {
        DELEGATION_ZONE_CAP
    };
    let borrowable = base.recent_tail.saturating_sub(RECENT_TAIL_BLOCKER_FLOOR);
    let needed = requested.saturating_sub(DELEGATION_ZONE_CAP);
    let borrowed = if blocker_active {
        borrowable.min(needed)
    } else {
        0
    };
    let effective_cap = DELEGATION_ZONE_CAP + borrowed;
    let rendered_children = if active_children == 0 {
        0
    } else {
        active_children.min((effective_cap / DELEGATION_CHILD_FLOOR) as usize)
    };
    let child_budget = if rendered_children == 0 {
        DelegationBudget {
            active_children,
            rendered_children: 0,
            overflow_children: active_children,
            per_child_budget: 0,
            rendered_total: 0,
        }
    } else {
        let per_child_budget = DELEGATION_CHILD_FLOOR.max(effective_cap / rendered_children as u32);
        DelegationBudget {
            active_children,
            rendered_children,
            overflow_children: active_children.saturating_sub(rendered_children),
            per_child_budget,
            rendered_total: per_child_budget * rendered_children as u32,
        }
    };
    DelegationBudgetAllocation {
        child_budget,
        requested_delegation_zone_budget: requested,
        delegation_zone_budget: effective_cap,
        recent_tail_budget: base.recent_tail.saturating_sub(borrowed),
        borrowed_from_recent_tail: borrowed,
        unfunded_blocker_tokens: requested.saturating_sub(effective_cap),
        blocker_active,
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConfidenceAction {
    AutoAccept,
    AskUser,
    Reject,
}

pub fn next_action_confidence_action(
    confidence: f32,
    ask_user_count_1h: u32,
    source: &str,
    provenance_event_id: Option<&str>,
) -> ConfidenceAction {
    if source == "small_model" && provenance_event_id.is_none() {
        return ConfidenceAction::AskUser;
    }
    let fatigue_downgrade_allowed = matches!(source, "structured_event" | "rule");
    let adjusted = if fatigue_downgrade_allowed && ask_user_count_1h >= 3 {
        confidence - 0.1
    } else {
        confidence
    };
    if adjusted >= 0.8 {
        ConfidenceAction::AutoAccept
    } else if adjusted >= 0.5 {
        ConfidenceAction::AskUser
    } else {
        ConfidenceAction::Reject
    }
}

pub fn suggested_next_action_expires_at(kind: &str, now: chrono::DateTime<chrono::Utc>) -> String {
    let expires = match kind {
        "approval" => now + chrono::Duration::hours(24),
        "todo" => now + chrono::Duration::days(7),
        _ => now + chrono::Duration::hours(1),
    };
    expires.to_rfc3339()
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RetrievalStage {
    Structured,
    Fts,
    Vector,
}

impl RetrievalStage {
    pub fn timeout_ms(&self) -> u64 {
        match self {
            RetrievalStage::Structured => 50,
            RetrievalStage::Fts => 200,
            RetrievalStage::Vector => 500,
        }
    }

    pub fn event_type(&self, reason: &str) -> String {
        match self {
            RetrievalStage::Structured => format!("retrieval.structured_{reason}"),
            RetrievalStage::Fts => format!("retrieval.fts_{reason}"),
            RetrievalStage::Vector => format!("retrieval.vector_{reason}"),
        }
    }

    pub fn next_stage(&self) -> Option<RetrievalStage> {
        match self {
            RetrievalStage::Structured => Some(RetrievalStage::Fts),
            RetrievalStage::Fts => Some(RetrievalStage::Vector),
            RetrievalStage::Vector => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RenderMode {
    PlainText,
    Markdown,
    CodeBlockPreserved,
    ToolPreview,
    Summary,
    ReferenceOnly,
}

impl RenderMode {
    pub fn as_str(&self) -> &'static str {
        match self {
            RenderMode::PlainText => "plain_text",
            RenderMode::Markdown => "markdown",
            RenderMode::CodeBlockPreserved => "code_block_preserved",
            RenderMode::ToolPreview => "tool_preview",
            RenderMode::Summary => "summary",
            RenderMode::ReferenceOnly => "reference_only",
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ContextManifestWrite {
    pub manifest_id: String,
    pub user_id: String,
    pub session_id: String,
    pub run_id: Option<String>,
    pub turn_id: String,
    pub model_provider: String,
    pub model_name: String,
    pub context_window_tokens: u32,
    pub max_output_tokens: u32,
    pub total_estimated_tokens: u32,
    pub policy_version: String,
    pub tokenizer_id: Option<String>,
    pub budget_template_id: Option<String>,
    pub turn_intent: Option<String>,
    pub reason: String,
    pub manifest_json: serde_json::Value,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ContextManifestItemWrite {
    pub session_id: String,
    pub item_order: i32,
    pub zone: String,
    pub source_table: String,
    pub source_id: String,
    pub source_hash: Option<String>,
    pub included: bool,
    pub token_estimate: u32,
    pub budget_tokens: u32,
    pub reason: String,
    pub render_mode: String,
    pub raw_ref: Option<String>,
}

#[derive(Debug, Error)]
pub enum ContextManifestError {
    #[error("database operation failed: operation={operation}, entity={entity}, source={source}")]
    Database {
        operation: &'static str,
        entity: String,
        #[source]
        source: sqlx::Error,
    },
    #[error("json serialization failed: operation={operation}, entity={entity}, source={source}")]
    Json {
        operation: &'static str,
        entity: String,
        #[source]
        source: serde_json::Error,
    },
    #[error("cross-session retrieval missing user_id filter")]
    CrossSessionAuthMissing,
    #[error("unsupported raw_ref scheme: {scheme}")]
    UnsupportedRawRefScheme { scheme: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SessionArtifactManifestRow {
    status: String,
    content_json: String,
    metadata: Option<String>,
}

fn context_manifest_decode_error(
    operation: &'static str,
    entity: &str,
    column: &str,
    source: sqlx::Error,
) -> ContextManifestError {
    ContextManifestError::Database {
        operation,
        entity: format!("{entity}.{column}"),
        source,
    }
}

fn context_manifest_invalid_value_error(
    operation: &'static str,
    entity: &str,
    column: &str,
    message: impl Into<String>,
) -> ContextManifestError {
    let source = sqlx::Error::Decode(Box::new(std::io::Error::new(
        std::io::ErrorKind::InvalidData,
        message.into(),
    )));
    context_manifest_decode_error(operation, entity, column, source)
}

fn context_manifest_row_string(
    row: &impl ContextManifestDbRow,
    operation: &'static str,
    entity: &str,
    column: &str,
) -> Result<String, ContextManifestError> {
    row.string_column(column)
        .map_err(|source| context_manifest_decode_error(operation, entity, column, source))
}

fn context_manifest_row_optional_string(
    row: &impl ContextManifestDbRow,
    operation: &'static str,
    entity: &str,
    column: &str,
) -> Result<Option<String>, ContextManifestError> {
    row.optional_string_column(column)
        .map_err(|source| context_manifest_decode_error(operation, entity, column, source))
}

fn context_manifest_row_u32_at_least(
    row: &impl ContextManifestDbRow,
    operation: &'static str,
    entity: &str,
    column: &str,
    min: i64,
) -> Result<u32, ContextManifestError> {
    let value = row
        .i64_column(column)
        .map_err(|source| context_manifest_decode_error(operation, entity, column, source))?;
    if value < min {
        return Err(context_manifest_invalid_value_error(
            operation,
            entity,
            column,
            format!("invalid {entity}.{column}: {value}; expected >= {min}"),
        ));
    }
    u32::try_from(value).map_err(|_| {
        context_manifest_invalid_value_error(
            operation,
            entity,
            column,
            format!(
                "invalid {entity}.{column}: {value}; expected <= {}",
                u32::MAX
            ),
        )
    })
}

fn decode_preview_template_budget_row(
    row: &impl ContextManifestDbRow,
    tool_name: &str,
) -> Result<u32, ContextManifestError> {
    context_manifest_row_u32_at_least(
        row,
        "preview_template_lookup_decode",
        tool_name,
        "max_preview_bytes",
        1,
    )
}

fn decode_session_artifact_manifest_row(
    row: &impl ContextManifestDbRow,
    artifact_id: &str,
) -> Result<SessionArtifactManifestRow, ContextManifestError> {
    let operation = "render_manifest_artifact_decode";
    Ok(SessionArtifactManifestRow {
        status: context_manifest_row_string(row, operation, artifact_id, "status")?,
        content_json: context_manifest_row_string(row, operation, artifact_id, "content_json")?,
        metadata: context_manifest_row_optional_string(row, operation, artifact_id, "metadata")?,
    })
}

#[derive(Clone)]
pub struct DatabaseContextManifestStore {
    pool: SharedPool,
}

struct SessionEventInsert<'a> {
    user_id: &'a str,
    session_id: &'a str,
    event_type: &'a str,
    content: &'a str,
    metadata: serde_json::Value,
    operation: &'static str,
    entity: &'a str,
}

impl DatabaseContextManifestStore {
    pub fn new(pool: SharedPool) -> Self {
        Self { pool }
    }

    async fn insert_session_event_and_bump_count(
        &self,
        event: SessionEventInsert<'_>,
    ) -> Result<String, ContextManifestError> {
        let event_id = Uuid::new_v4().to_string();
        let mut tx =
            self.pool
                .get()
                .begin()
                .await
                .map_err(|source| ContextManifestError::Database {
                    operation: event.operation,
                    entity: event.entity.to_string(),
                    source,
                })?;
        let insert_result = sqlx::query(
            "INSERT INTO agent_events
             (event_id, session_id, user_id, event_type, content, metadata, created_at)
             VALUES (?, ?, ?, ?, ?, ?, NOW(6))",
        )
        .bind(&event_id)
        .bind(event.session_id)
        .bind(event.user_id)
        .bind(event.event_type)
        .bind(event.content)
        .bind(event.metadata.to_string())
        .execute(&mut *tx)
        .await
        .map_err(|source| ContextManifestError::Database {
            operation: event.operation,
            entity: event.entity.to_string(),
            source,
        })?;
        let inserted_events =
            crate::storage::rows_affected_to_i64(insert_result.rows_affected(), event.operation)
                .map_err(|source| ContextManifestError::Database {
                    operation: event.operation,
                    entity: event.entity.to_string(),
                    source,
                })?;
        if inserted_events <= 0 {
            return Err(ContextManifestError::Database {
                operation: event.operation,
                entity: event.entity.to_string(),
                source: sqlx::Error::Protocol("session event insert affected no rows".into()),
            });
        }
        crate::storage::add_agent_session_event_count_or_create(
            &mut tx,
            event.session_id,
            event.user_id,
            inserted_events,
            Some(&event_id),
        )
        .await
        .map_err(|source| ContextManifestError::Database {
            operation: event.operation,
            entity: event.entity.to_string(),
            source,
        })?;
        tx.commit()
            .await
            .map_err(|source| ContextManifestError::Database {
                operation: event.operation,
                entity: event.entity.to_string(),
                source,
            })?;
        Ok(event_id)
    }

    pub async fn normalize_reason(
        &self,
        user_id: &str,
        proposed_reason: &str,
        session_id: &str,
        run_id: Option<&str>,
        turn_id: &str,
        component: &str,
    ) -> Result<String, ContextManifestError> {
        let known = CONTEXT_MANIFEST_REASONS
            .iter()
            .any(|(reason, _, _)| *reason == proposed_reason);
        if known {
            return Ok(proposed_reason.to_string());
        }
        self.insert_session_event_and_bump_count(SessionEventInsert {
            user_id,
            session_id,
            event_type: "manifest.reason_unknown",
            content: proposed_reason,
            metadata: serde_json::json!({
                "proposed_reason": proposed_reason,
                "turn_id": turn_id,
                "run_id": run_id,
                "component": component,
            }),
            operation: "manifest_reason_unknown_event",
            entity: session_id,
        })
        .await
        .map(|_| ())?;
        Ok("other".to_string())
    }

    pub async fn save_manifest(
        &self,
        manifest: ContextManifestWrite,
        items: Vec<ContextManifestItemWrite>,
    ) -> Result<(), ContextManifestError> {
        let reason = self
            .normalize_reason(
                &manifest.user_id,
                &manifest.reason,
                &manifest.session_id,
                manifest.run_id.as_deref(),
                &manifest.turn_id,
                "context_manifest_store",
            )
            .await?;
        let dropped_count = items.iter().filter(|item| !item.included).count() as i64;
        let manifest_json = serde_json::to_string(&manifest.manifest_json).map_err(|source| {
            ContextManifestError::Json {
                operation: "serialize_context_manifest",
                entity: manifest.manifest_id.clone(),
                source,
            }
        })?;
        let mut tx =
            self.pool
                .get()
                .begin()
                .await
                .map_err(|source| ContextManifestError::Database {
                    operation: "begin_context_manifest",
                    entity: manifest.manifest_id.clone(),
                    source,
                })?;
        sqlx::query(
            "INSERT INTO context_manifests
             (manifest_id, user_id, session_id, run_id, turn_id, model_provider, model_name,
              context_window_tokens, max_output_tokens, total_estimated_tokens, policy_version,
              tokenizer_id, budget_template_id, turn_intent, reason, dropped_count, manifest_json, created_at)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, NOW(6))",
        )
        .bind(&manifest.manifest_id)
        .bind(&manifest.user_id)
        .bind(&manifest.session_id)
        .bind(&manifest.run_id)
        .bind(&manifest.turn_id)
        .bind(&manifest.model_provider)
        .bind(&manifest.model_name)
        .bind(i64::from(manifest.context_window_tokens))
        .bind(i64::from(manifest.max_output_tokens))
        .bind(i64::from(manifest.total_estimated_tokens))
        .bind(&manifest.policy_version)
        .bind(&manifest.tokenizer_id)
        .bind(&manifest.budget_template_id)
        .bind(&manifest.turn_intent)
        .bind(&reason)
        .bind(dropped_count)
        .bind(manifest_json)
        .execute(&mut *tx)
        .await
        .map_err(|source| ContextManifestError::Database {
            operation: "insert_context_manifest",
            entity: manifest.manifest_id.clone(),
            source,
        })?;
        for item in items {
            let referenced_artifact_id = referenced_artifact_id_from_manifest_item(&item);
            sqlx::query(
                "INSERT INTO context_manifest_items
                 (manifest_id, session_id, item_order, zone, source_table, source_id, source_hash,
                  included, token_estimate, budget_tokens, reason, render_mode, raw_ref, created_at)
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, NOW(6))",
            )
            .bind(&manifest.manifest_id)
            .bind(&item.session_id)
            .bind(item.item_order)
            .bind(&item.zone)
            .bind(&item.source_table)
            .bind(&item.source_id)
            .bind(&item.source_hash)
            .bind(if item.included { 1_i8 } else { 0_i8 })
            .bind(i64::from(item.token_estimate))
            .bind(i64::from(item.budget_tokens))
            .bind(&item.reason)
            .bind(&item.render_mode)
            .bind(&item.raw_ref)
            .execute(&mut *tx)
            .await
            .map_err(|source| ContextManifestError::Database {
                operation: "insert_context_manifest_item",
                entity: manifest.manifest_id.clone(),
                source,
            })?;
            if let Some(artifact_id) = referenced_artifact_id {
                sqlx::query(
                    "UPDATE session_artifacts
	                     SET referenced_by_manifest_count = referenced_by_manifest_count + 1,
	                         updated_at = NOW(6)
	                     WHERE user_id = ? AND session_id = ? AND artifact_id = ?",
                )
                .bind(&manifest.user_id)
                .bind(&item.session_id)
                .bind(&artifact_id)
                .execute(&mut *tx)
                .await
                .map_err(|source| ContextManifestError::Database {
                    operation: "increment_manifest_artifact_ref",
                    entity: artifact_id,
                    source,
                })?;
            }
        }
        tx.commit()
            .await
            .map_err(|source| ContextManifestError::Database {
                operation: "commit_context_manifest",
                entity: manifest.manifest_id,
                source,
            })
    }

    pub async fn validate_raw_ref(&self, raw_ref: &str) -> Result<(), ContextManifestError> {
        let Some((scheme, _rest)) = raw_ref.split_once("://") else {
            return Err(ContextManifestError::UnsupportedRawRefScheme {
                scheme: String::new(),
            });
        };
        let exists = sqlx::query(
            "SELECT scheme FROM raw_ref_scheme_registry WHERE scheme = ? AND is_active = 1",
        )
        .bind(scheme)
        .fetch_optional(self.pool.get())
        .await
        .map_err(|source| ContextManifestError::Database {
            operation: "raw_ref_scheme_lookup",
            entity: raw_ref.to_string(),
            source,
        })?
        .is_some();
        if exists {
            Ok(())
        } else {
            Err(ContextManifestError::UnsupportedRawRefScheme {
                scheme: scheme.to_string(),
            })
        }
    }

    pub async fn record_retrieval_degrade_event(
        &self,
        user_id: &str,
        session_id: &str,
        run_id: Option<&str>,
        stage: RetrievalStage,
        reason: &str,
        elapsed_ms: u64,
    ) -> Result<Option<RetrievalStage>, ContextManifestError> {
        let event_type = stage.event_type(reason);
        let next_stage = stage.next_stage();
        self.insert_session_event_and_bump_count(SessionEventInsert {
            user_id,
            session_id,
            event_type: &event_type,
            content: reason,
            metadata: serde_json::json!({
                "run_id": run_id,
                "stage": format!("{stage:?}"),
                "reason": reason,
                "elapsed_ms": elapsed_ms,
                "sla_ms": stage.timeout_ms(),
                "next_stage": next_stage.as_ref().map(|stage| format!("{stage:?}")),
            }),
            operation: "insert_retrieval_degrade_event",
            entity: session_id,
        })
        .await
        .map(|_| ())?;
        Ok(next_stage)
    }

    pub async fn preview_template_budget_or_fallback(
        &self,
        user_id: &str,
        session_id: &str,
        run_id: Option<&str>,
        tool_name: &str,
    ) -> Result<u32, ContextManifestError> {
        let row = sqlx::query(
            "SELECT max_preview_bytes FROM preview_template_registry
             WHERE tool_name = ? AND status = 'active'
             ORDER BY updated_at DESC
             LIMIT 1",
        )
        .bind(tool_name)
        .fetch_optional(self.pool.get())
        .await
        .map_err(|source| ContextManifestError::Database {
            operation: "preview_template_lookup",
            entity: tool_name.to_string(),
            source,
        })?;
        if let Some(row) = row {
            return decode_preview_template_budget_row(&row, tool_name);
        }

        self.insert_session_event_and_bump_count(SessionEventInsert {
            user_id,
            session_id,
            event_type: "preview_template_missing",
            content: tool_name,
            metadata: serde_json::json!({
                "run_id": run_id,
                "tool_name": tool_name,
                "fallback_max_preview_bytes": 400,
            }),
            operation: "preview_template_missing_event",
            entity: tool_name,
        })
        .await
        .map(|_| ())?;
        Ok(400)
    }

    pub async fn render_artifact_manifest_item(
        &self,
        user_id: &str,
        session_id: &str,
        artifact_id: &str,
        summary_hint: Option<&str>,
    ) -> Result<String, ContextManifestError> {
        let row = sqlx::query(
            "SELECT status, content_json, CAST(metadata AS CHAR) AS metadata
             FROM session_artifacts
             WHERE user_id = ? AND session_id = ? AND artifact_id = ?
             LIMIT 1",
        )
        .bind(user_id)
        .bind(session_id)
        .bind(artifact_id)
        .fetch_optional(self.pool.get())
        .await
        .map_err(|source| ContextManifestError::Database {
            operation: "render_manifest_artifact_lookup",
            entity: artifact_id.to_string(),
            source,
        })?;

        let Some(row) = row else {
            return Ok(expired_artifact_placeholder(artifact_id, summary_hint));
        };

        let artifact_row = decode_session_artifact_manifest_row(&row, artifact_id)?;
        let summary = artifact_summary_for_placeholder(
            summary_hint,
            artifact_row.metadata.as_deref(),
            artifact_row.content_json.as_str(),
        );

        if !session_artifact_raw_payload_is_available(&artifact_row.status) {
            return Ok(expired_artifact_placeholder(
                artifact_id,
                summary.as_deref(),
            ));
        }
        Ok(summary.unwrap_or(artifact_row.content_json))
    }
}

pub fn content_hash_with_normalize_version(
    content_hash: &str,
    normalize_version: Option<&str>,
) -> String {
    let version = normalize_version.unwrap_or("raw_v1");
    let digest = Sha256::digest(format!("{content_hash}|{version}").as_bytes());
    format!("sha256:{digest:x}")
}

pub fn expired_artifact_placeholder(artifact_id: &str, summary: Option<&str>) -> String {
    match summary.filter(|value| !value.trim().is_empty()) {
        Some(summary) => format!(
            "artifact {artifact_id}: historical, raw no longer available, summary preserved: {summary}"
        ),
        None => format!(
            "artifact {artifact_id}: historical, raw no longer available, summary preserved"
        ),
    }
}

fn artifact_summary_for_placeholder(
    summary_hint: Option<&str>,
    metadata_json: Option<&str>,
    content_json: &str,
) -> Option<String> {
    if let Some(summary) = summary_hint
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        return Some(summary.to_string());
    }
    for source in [metadata_json, Some(content_json)].into_iter().flatten() {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(source) else {
            continue;
        };
        if let Some(summary) = value
            .get("summary")
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            return Some(summary.to_string());
        }
        if let Some(preview) = value
            .get("preview_text")
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            return Some(preview.chars().take(240).collect());
        }
    }
    None
}

pub fn artifact_id_from_raw_ref(raw_ref: &str) -> Option<String> {
    let rest = raw_ref.strip_prefix("artifact://")?;
    let path = rest.split('@').next().unwrap_or(rest);
    path.trim_matches('/')
        .split('/')
        .rfind(|part| !part.is_empty())
        .map(ToString::to_string)
}

fn referenced_artifact_id_from_manifest_item(item: &ContextManifestItemWrite) -> Option<String> {
    if item.source_table == "session_artifacts" {
        return Some(item.source_id.clone());
    }
    item.raw_ref.as_deref().and_then(artifact_id_from_raw_ref)
}

pub fn cross_session_retrieval_requires_user_filter(
    user_id_filter: Option<&str>,
) -> Result<(), ContextManifestError> {
    if user_id_filter.is_some_and(|value| !value.trim().is_empty()) {
        Ok(())
    } else {
        Err(ContextManifestError::CrossSessionAuthMissing)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Clone)]
    struct FakeContextManifestRow {
        failed_column: Option<&'static str>,
        i64_overrides: Vec<(&'static str, i64)>,
        metadata: Option<&'static str>,
    }

    impl FakeContextManifestRow {
        fn complete() -> Self {
            Self {
                failed_column: None,
                i64_overrides: Vec::new(),
                metadata: Some(r#"{"summary":"metadata summary"}"#),
            }
        }

        fn fail_on(column: &'static str) -> Self {
            Self {
                failed_column: Some(column),
                ..Self::complete()
            }
        }

        fn with_i64(column: &'static str, value: i64) -> Self {
            Self {
                i64_overrides: vec![(column, value)],
                ..Self::complete()
            }
        }

        fn with_metadata(metadata: Option<&'static str>) -> Self {
            Self {
                metadata,
                ..Self::complete()
            }
        }

        fn fail_if_needed(&self, column: &str) -> Result<(), sqlx::Error> {
            if self.failed_column == Some(column) {
                Err(sqlx::Error::ColumnNotFound(column.to_string()))
            } else {
                Ok(())
            }
        }
    }

    impl ContextManifestDbRow for FakeContextManifestRow {
        fn string_column(&self, column: &str) -> Result<String, sqlx::Error> {
            self.fail_if_needed(column)?;
            Ok(match column {
                "status" => "active",
                "content_json" => r#"{"summary":"content summary","body":"payload"}"#,
                _ => return Err(sqlx::Error::ColumnNotFound(column.to_string())),
            }
            .to_string())
        }

        fn optional_string_column(&self, column: &str) -> Result<Option<String>, sqlx::Error> {
            self.fail_if_needed(column)?;
            Ok(match column {
                "metadata" => self.metadata.map(ToString::to_string),
                _ => return Err(sqlx::Error::ColumnNotFound(column.to_string())),
            })
        }

        fn i64_column(&self, column: &str) -> Result<i64, sqlx::Error> {
            self.fail_if_needed(column)?;
            if let Some((_, value)) = self
                .i64_overrides
                .iter()
                .find(|(candidate, _)| *candidate == column)
            {
                return Ok(*value);
            }
            Ok(match column {
                "max_preview_bytes" => 1024,
                _ => return Err(sqlx::Error::ColumnNotFound(column.to_string())),
            })
        }
    }

    fn assert_context_manifest_db_error_mentions(
        result: Result<impl std::fmt::Debug, ContextManifestError>,
        needle: &str,
    ) {
        let error = result.expect_err("decode should fail");
        match error {
            ContextManifestError::Database { entity, source, .. } => {
                assert!(
                    entity.contains(needle) || source.to_string().contains(needle),
                    "error should identify `{needle}`, got entity={entity}, source={source}"
                );
            }
            other => panic!("expected database decode error, got {other:?}"),
        }
    }

    #[test]
    fn session_artifact_status_helpers_treat_expired_as_non_downloadable() {
        assert_eq!(
            session_artifact_status_kind(SESSION_ARTIFACT_STATUS_EXPIRED),
            SessionArtifactStatusKind::Expired
        );
        assert!(!session_artifact_raw_payload_is_available(
            SESSION_ARTIFACT_STATUS_EXPIRED
        ));
        assert!(session_artifact_raw_payload_is_available("active"));
    }

    #[test]
    fn expired_artifact_placeholder_preserves_summary_hint() {
        let rendered =
            expired_artifact_placeholder("artifact-1", Some("important preserved summary"));
        assert!(rendered.contains("historical, raw no longer available"));
        assert!(rendered.contains("important preserved summary"));
    }

    #[test]
    fn preview_template_budget_decode_preserves_values_and_fails_loudly() {
        assert_eq!(
            decode_preview_template_budget_row(&FakeContextManifestRow::complete(), "bash")
                .unwrap(),
            1024
        );

        assert_context_manifest_db_error_mentions(
            decode_preview_template_budget_row(
                &FakeContextManifestRow::fail_on("max_preview_bytes"),
                "bash",
            ),
            "max_preview_bytes",
        );
        assert_context_manifest_db_error_mentions(
            decode_preview_template_budget_row(
                &FakeContextManifestRow::with_i64("max_preview_bytes", 0),
                "bash",
            ),
            "max_preview_bytes",
        );
        assert_context_manifest_db_error_mentions(
            decode_preview_template_budget_row(
                &FakeContextManifestRow::with_i64("max_preview_bytes", i64::from(u32::MAX) + 1),
                "bash",
            ),
            "max_preview_bytes",
        );
    }

    #[test]
    fn session_artifact_manifest_row_decode_preserves_values_and_fails_loudly() {
        let row =
            decode_session_artifact_manifest_row(&FakeContextManifestRow::complete(), "artifact-1")
                .expect("artifact manifest row decodes");
        assert_eq!(row.status, "active");
        assert_eq!(
            row.metadata.as_deref(),
            Some(r#"{"summary":"metadata summary"}"#)
        );
        assert_eq!(
            artifact_summary_for_placeholder(None, row.metadata.as_deref(), &row.content_json)
                .as_deref(),
            Some("metadata summary")
        );

        let row = decode_session_artifact_manifest_row(
            &FakeContextManifestRow::with_metadata(None),
            "artifact-1",
        )
        .expect("NULL metadata is valid");
        assert_eq!(row.metadata, None);
        assert_eq!(
            artifact_summary_for_placeholder(None, row.metadata.as_deref(), &row.content_json)
                .as_deref(),
            Some("content summary")
        );

        for column in ["status", "content_json", "metadata"] {
            assert_context_manifest_db_error_mentions(
                decode_session_artifact_manifest_row(
                    &FakeContextManifestRow::fail_on(column),
                    "artifact-1",
                ),
                column,
            );
        }
    }
}
