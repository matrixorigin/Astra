//! Session restore: canonical Server summaries and explicit CLI-local recovery.
//! Server execution state is materialized by the Session Context coordinator.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use astra_core::canonical_names::{append_unique_names, normalize_name_list};

pub const COMPOSITE_SNAPSHOT_INDEX_ARTIFACT_KIND: &str = "composite_snapshot_index";
fn is_zero_u64(v: &u64) -> bool {
    *v == 0
}

trait SessionRestoreRow {
    fn string_column(&self, column: &str) -> Result<String, sqlx::Error>;
    fn optional_string_column(&self, column: &str) -> Result<Option<String>, sqlx::Error>;
    fn i64_column(&self, column: &str) -> Result<i64, sqlx::Error>;
}

impl SessionRestoreRow for sqlx::mysql::MySqlRow {
    fn string_column(&self, column: &str) -> Result<String, sqlx::Error> {
        use sqlx::Row;
        self.try_get::<String, _>(column)
    }

    fn optional_string_column(&self, column: &str) -> Result<Option<String>, sqlx::Error> {
        use sqlx::Row;
        self.try_get::<Option<String>, _>(column)
    }

    fn i64_column(&self, column: &str) -> Result<i64, sqlx::Error> {
        use sqlx::Row;
        self.try_get::<i64, _>(column)
    }
}

fn mysql_string(
    row: &impl SessionRestoreRow,
    context: &str,
    column: &str,
) -> Result<String, String> {
    row.string_column(column)
        .map_err(|e| format!("{context}: decode column `{column}`: {e}"))
}

fn mysql_optional_string(
    row: &impl SessionRestoreRow,
    context: &str,
    column: &str,
) -> Result<Option<String>, String> {
    row.optional_string_column(column)
        .map_err(|e| format!("{context}: decode column `{column}`: {e}"))
}

fn mysql_i64(row: &impl SessionRestoreRow, context: &str, column: &str) -> Result<i64, String> {
    row.i64_column(column)
        .map_err(|e| format!("{context}: decode column `{column}`: {e}"))
}

fn metadata_json_state(metadata_str: Option<&str>) -> Result<SessionMetadataState, String> {
    match metadata_str.map(str::trim).filter(|m| !m.is_empty()) {
        Some(metadata) => extract_session_state_from_metadata(metadata),
        None => Ok(SessionMetadataState::default()),
    }
}

fn non_negative_i64_to_u64(value: i64, context: &str, column: &str) -> Result<u64, String> {
    u64::try_from(value)
        .map_err(|_| format!("{context}: column `{column}` expected u64 range, got {value}"))
}

fn non_negative_i64_to_u32(value: i64, context: &str, column: &str) -> Result<u32, String> {
    u32::try_from(value)
        .map_err(|_| format!("{context}: column `{column}` expected u32 range, got {value}"))
}

fn cache_token_counts_from_token_usage_json(
    raw: &str,
    context: &str,
) -> Result<(Option<i64>, Option<i64>), String> {
    let value: Value = serde_json::from_str(raw)
        .map_err(|error| format!("{context}: token_usage JSON decode failed: {error}"))?;
    let usage = astra_turn_types::CanonicalTokenUsage::from_json(&value)
        .map_err(|error| format!("{context}: {error}"))?;
    Ok((
        usage.cached_input_tokens().map(|value| value as i64),
        usage.cache_creation_tokens().map(|value| value as i64),
    ))
}

fn apply_restore_cache_token_usage(
    raw: &str,
    event_id: &str,
    cache_read_total: &mut i64,
    cache_creation_total: &mut i64,
) -> bool {
    let (cache_read, cache_creation) = match cache_token_counts_from_token_usage_json(
        raw,
        "restore_cloud_session.cache_tokens",
    ) {
        Ok(counts) => counts,
        Err(error) => {
            tracing::warn!(
                target: "astra_services::session_restore",
                event_id = event_id,
                error = %error,
                "invalid token_usage while restoring cache totals; skipping event token counters"
            );
            return false;
        }
    };
    // These are observed subtotals, not complete request measurements. A missing
    // lane leaves its subtotal unchanged; it does not establish measured zero.
    let Some(next_cache_read_total) = cache_read_total.checked_add(cache_read.unwrap_or(0)) else {
        tracing::warn!(
            target: "astra_services::session_restore",
            event_id = event_id,
            current_total = *cache_read_total,
            delta = ?cache_read,
            "cache read token total overflow while restoring session; skipping event token counters"
        );
        return false;
    };
    let Some(next_cache_creation_total) =
        cache_creation_total.checked_add(cache_creation.unwrap_or(0))
    else {
        tracing::warn!(
            target: "astra_services::session_restore",
            event_id = event_id,
            current_total = *cache_creation_total,
            delta = ?cache_creation,
            "cache creation token total overflow while restoring session; skipping event token counters"
        );
        return false;
    };

    *cache_read_total = next_cache_read_total;
    *cache_creation_total = next_cache_creation_total;
    true
}

// ─── Restored Session State ─────────────────────────────────────────────────

/// Reconstructed Session state. The resumable directory returns only its summary
/// fields; execution requires the separate selected-Session restore and admission.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RestoredSession {
    /// Session ID being restored.
    pub session_id: String,
    /// Turn number to continue from.
    pub turn_count: u32,
    /// Total input tokens consumed so far.
    pub total_tokens_in: u64,
    /// Total output tokens consumed so far.
    pub total_tokens_out: u64,
    /// Observed prompt-cache read subtotal; not proof of complete usage.
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    pub total_cache_read_tokens: u64,
    /// Observed prompt-cache creation subtotal; not proof of complete usage.
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    pub total_cache_creation_tokens: u64,
    /// Recently used tools (for context carry-forward).
    pub recent_tools: Vec<String>,
    /// Number of checkpoints available.
    pub checkpoint_count: u32,
    /// Status of the session when last active.
    pub last_status: String,
    /// Git branch from workspace metadata.
    pub git_branch: Option<String>,
    /// Model used in the session.
    pub model: Option<String>,
    /// Permission mode used when the session was last active.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permission_mode: Option<String>,
    /// Session title (if set).
    pub title: Option<String>,
    /// Whether restoration was from cloud (true) or local only (false).
    pub restored_from_cloud: bool,
    /// Conversation messages from Step Protocol heavy checkpoint (for LLM resume).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub conversation_messages: Vec<serde_json::Value>,
    /// One causally selected conversation generation. New clients consume
    /// this instead of independently comparing turn counts and message arrays.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resume_bundle: Option<astra_turn_types::ResumeBundleV1>,
    /// Schema-addressed activation evidence; this is the only form that can
    /// authorize a deferred carrier call after restore.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub deferred_tool_activations: Vec<astra_turn_types::DeferredToolActivation>,
    /// Blocked/health-avoidance tools from checkpoint.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub blocked_tools: Vec<String>,
    /// Serialized approval overrides restored from a heavy checkpoint.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approval_overrides: Option<serde_json::Value>,
    /// Structured interruption payload restored from a heavy checkpoint.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interruption: Option<serde_json::Value>,
    /// Serialized compaction-state payload restored from a heavy checkpoint.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compaction_state: Option<serde_json::Value>,
    /// Serialized context pipeline state (stats + latches + recovery).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pipeline_state: Option<serde_json::Value>,
    /// Latest structured context-trace signal.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_context_trace: Option<super::session_workspace::ContextTraceSignal>,
    /// Authoritative workspace metadata snapshot when available.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<super::session_workspace::WorkspaceMetadata>,
}

impl RestoredSession {
    /// Return the one selected conversation payload.
    ///
    /// Only a causally selected, versioned bundle may drive continuation.
    /// The standalone message field is informational and never executable
    /// resume authority.
    pub fn resume_messages(&self) -> &[serde_json::Value] {
        self.resume_bundle
            .as_ref()
            .map(|bundle| bundle.conversation_messages.as_slice())
            .unwrap_or_default()
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ResumableSessionsResponse {
    pub sessions: Vec<RestoredSession>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct SessionMetadataState {
    pub git_branch: Option<String>,
    pub model: Option<String>,
    pub permission_mode: Option<String>,
}

/// A restored checkpoint entry (lightweight, for listing).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RestoredCheckpoint {
    pub number: u32,
    pub turn: u32,
    pub title: String,
    pub summary: String,
    pub total_tokens: u64,
}

async fn restore_cloud_cache_token_totals(
    pool: &sqlx::Pool<sqlx::MySql>,
    user_id: &str,
    session_id: &str,
) -> Result<(i64, i64), String> {
    use futures_util::TryStreamExt;
    let mut connection = crate::CancellationSafePoolConnection::acquire(pool)
        .await
        .map_err(|e| format!("restore_cloud_session.cache_tokens: {e}"))?;
    let mut rows = sqlx::query(
        "SELECT event_id, IFNULL(CAST(token_usage AS CHAR), '{}') AS token_usage \
         FROM agent_events \
         WHERE session_id = ? AND user_id = ? \
           AND event_type IN ('user_query', 'llm_response') AND token_usage IS NOT NULL",
    )
    .bind(session_id)
    .bind(user_id)
    .fetch(connection.connection_mut());

    let mut cache_read_total = 0i64;
    let mut cache_creation_total = 0i64;
    while let Some(row) = rows
        .try_next()
        .await
        .map_err(|e| format!("restore_cloud_session.cache_tokens: {e}"))?
    {
        let event_id = mysql_string(&row, "restore_cloud_session.cache_tokens", "event_id")
            .unwrap_or_else(|error| {
                tracing::warn!(
                    target: "astra_services::session_restore",
                    error = %error,
                    "could not decode event_id while restoring cache totals"
                );
                "<unknown>".to_string()
            });
        let raw = mysql_string(&row, "restore_cloud_session.cache_tokens", "token_usage")?;
        apply_restore_cache_token_usage(
            &raw,
            &event_id,
            &mut cache_read_total,
            &mut cache_creation_total,
        );
    }
    drop(rows);
    connection.release();
    Ok((cache_read_total, cache_creation_total))
}

// ─── Restore Implementation ─────────────────────────────────────────────

/// Explicit local recovery and owner-scoped cloud diagnostic summaries.
pub struct SessionRestoreService {
    pool: Option<sqlx::Pool<sqlx::MySql>>,
}

impl SessionRestoreService {
    /// Create with a MatrixOne pool for cloud summaries.
    pub fn new(pool: sqlx::Pool<sqlx::MySql>) -> Self {
        Self { pool: Some(pool) }
    }

    /// Create for local filesystem recovery.
    pub fn local_only() -> Self {
        Self { pool: None }
    }

    /// Restore only local filesystem state. This intentionally never reads MatrixOne.
    pub async fn restore_local_session(
        &self,
        session_id: &str,
    ) -> Result<Option<RestoredSession>, String> {
        let journal = summarize_local_journal(session_id)?;
        let workspace = match super::session_workspace::read_workspace(session_id) {
            Ok(workspace) => Some(workspace),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) if journal.is_none() => {
                return Err(format!("read local workspace for {session_id}: {error}"));
            }
            Err(_) => None,
        };
        let Some(workspace) = workspace else {
            return journal
                .map(|summary| {
                    Ok(RestoredSession {
                        session_id: session_id.into(),
                        turn_count: summary.turn_count,
                        total_tokens_in: summary.total_tokens_in,
                        total_tokens_out: summary.total_tokens_out,
                        total_cache_read_tokens: summary.total_cache_read_tokens,
                        total_cache_creation_tokens: summary.total_cache_creation_tokens,
                        recent_tools: normalize_name_list(summary.recent_tools),
                        checkpoint_count: local_checkpoint_count(
                            session_id,
                            "restore_local_session",
                        )?,
                        last_status: summary.last_status,
                        model: summary.model,
                        permission_mode: summary.permission_mode,
                        ..Default::default()
                    })
                })
                .transpose();
        };
        let mut recent_tools =
            recent_tools_from_context_trace(workspace.last_context_trace.as_ref());
        if recent_tools.is_empty()
            && let Some(summary) = journal.as_ref()
        {
            recent_tools = normalize_name_list(summary.recent_tools.iter().map(String::as_str));
        }
        Ok(Some(restored_session_from_workspace(
            workspace,
            journal.as_ref(),
            recent_tools,
            local_checkpoint_count(session_id, "restore_local_session")?,
            false,
        )))
    }

    /// List only local filesystem checkpoints. This intentionally never reads MatrixOne.
    pub async fn list_local_checkpoints(
        &self,
        session_id: &str,
    ) -> Result<Vec<RestoredCheckpoint>, String> {
        let entries = super::session_checkpoint::read_checkpoint_index(session_id)
            .map_err(|error| format!("list_local_checkpoints: failed to read local checkpoint index for {session_id}: {error}"))?;
        Ok(parse_local_checkpoint_entries(&entries))
    }

    /// Try restoring workspace metadata from remote session artifacts.
    async fn restore_cloud_workspace(
        &self,
        user_id: &str,
        session_id: &str,
    ) -> Result<Option<super::session_workspace::WorkspaceMetadata>, String> {
        let pool = match self.pool.as_ref() {
            Some(pool) => pool,
            None => return Ok(None),
        };
        let artifact = crate::session_artifact_store::load_json_artifact_from_pool(
            pool,
            user_id,
            session_id,
            super::session_workspace::WORKSPACE_METADATA_PROJECTION_ID,
        )
        .await
        .map_err(|e| format!("restore_cloud_workspace: {e}"))?;

        let Some(artifact) = artifact else {
            return Ok(None);
        };

        let metadata =
            serde_json::from_value::<super::session_workspace::WorkspaceMetadata>(artifact.content)
                .map_err(|e| format!("restore_cloud_workspace: {e}"))?;

        Ok(Some(metadata))
    }

    /// Load owner-scoped diagnostic summaries. This does not authorize continuation.
    pub async fn load_cloud_summary(
        &self,
        user_id: &str,
        session_id: &str,
    ) -> Result<Option<RestoredSession>, String> {
        let pool = match &self.pool {
            Some(p) => p,
            None => return Ok(None),
        };

        let row = sqlx::query(
            "SELECT s.title, s.status, CAST(s.metadata AS CHAR) AS metadata_json, \
             COALESCE(event_summary.turn_count, 0) AS turn_count, \
             COALESCE(event_summary.total_tokens_in, 0) AS total_tokens_in, \
             COALESCE(event_summary.total_tokens_out, 0) AS total_tokens_out \
              FROM agent_sessions s \
              LEFT JOIN ( \
                SELECT user_id, session_id, \
                       COALESCE(MAX(CASE WHEN parent_run_id IS NULL THEN turn_seq END), 0) AS turn_count, \
                       CAST(COALESCE(SUM(CASE WHEN event_type IN ('user_query', 'llm_response') AND token_usage IS NOT NULL \
                         THEN COALESCE(token_input, 0) ELSE 0 END), 0) AS SIGNED) AS total_tokens_in, \
                       CAST(COALESCE(SUM(CASE WHEN event_type IN ('user_query', 'llm_response') AND token_usage IS NOT NULL \
                         THEN COALESCE(token_output, 0) ELSE 0 END), 0) AS SIGNED) AS total_tokens_out \
                FROM agent_events \
                WHERE user_id = ? AND session_id = ? \
                GROUP BY user_id, session_id \
              ) event_summary \
                ON event_summary.user_id = s.user_id AND event_summary.session_id = s.session_id \
              WHERE s.session_id = ? AND s.user_id = ?",
        )
        .bind(user_id)
        .bind(session_id)
        .bind(session_id)
        .bind(user_id)
        .fetch_optional(pool)
        .await
        .map_err(|e| format!("restore_cloud_session: {e}"))?;

        match row {
            Some(row) => {
                let status = mysql_string(&row, "restore_cloud_session", "status")?;
                let title = mysql_optional_string(&row, "restore_cloud_session", "title")?;
                let turn_count = mysql_i64(&row, "restore_cloud_session", "turn_count")?;
                let total_tokens_in = mysql_i64(&row, "restore_cloud_session", "total_tokens_in")?;
                let total_tokens_out =
                    mysql_i64(&row, "restore_cloud_session", "total_tokens_out")?;
                let (total_cache_read_tokens, total_cache_creation_tokens) =
                    restore_cloud_cache_token_totals(pool, user_id, session_id).await?;

                // Extract plan state from metadata JSON
                let metadata_str =
                    mysql_optional_string(&row, "restore_cloud_session", "metadata_json")?;
                let metadata_state = metadata_json_state(metadata_str.as_deref())?;

                let completed_turn =
                    non_negative_i64_to_u32(turn_count, "restore_cloud_session", "turn_count")?;
                let mut restored = RestoredSession {
                    session_id: session_id.to_string(),
                    turn_count: completed_turn,
                    total_tokens_in: non_negative_i64_to_u64(
                        total_tokens_in,
                        "restore_cloud_session",
                        "total_tokens_in",
                    )?,
                    total_tokens_out: non_negative_i64_to_u64(
                        total_tokens_out,
                        "restore_cloud_session",
                        "total_tokens_out",
                    )?,
                    total_cache_read_tokens: non_negative_i64_to_u64(
                        total_cache_read_tokens,
                        "restore_cloud_session",
                        "total_cache_read_tokens",
                    )?,
                    total_cache_creation_tokens: non_negative_i64_to_u64(
                        total_cache_creation_tokens,
                        "restore_cloud_session",
                        "total_cache_creation_tokens",
                    )?,
                    last_status: status,
                    title,
                    restored_from_cloud: true,
                    git_branch: metadata_state.git_branch.clone(),
                    // Versioned restore owns the potentially large history in
                    // exactly one response field.
                    conversation_messages: Vec::new(),
                    ..Default::default()
                };
                if let Some(workspace) = self.restore_cloud_workspace(user_id, session_id).await? {
                    // These are observed diagnostic subtotals. Only canonical
                    // Context may supply the executable conversation and cursor.
                    restored.total_tokens_in =
                        restored.total_tokens_in.max(workspace.total_tokens_in);
                    restored.total_tokens_out =
                        restored.total_tokens_out.max(workspace.total_tokens_out);
                    restored.total_cache_read_tokens = restored
                        .total_cache_read_tokens
                        .max(workspace.total_cache_read_tokens);
                    restored.total_cache_creation_tokens = restored
                        .total_cache_creation_tokens
                        .max(workspace.total_cache_creation_tokens);
                    restored.git_branch = restored.git_branch.or(workspace.git_branch);
                }
                Ok(Some(restored))
            }
            None => Ok(None),
        }
    }
}

fn apply_selected_resume_bundle(session: &mut RestoredSession) {
    let Some(bundle) = session.resume_bundle.as_ref() else {
        session.deferred_tool_activations.clear();
        session.recent_tools.clear();
        session.blocked_tools.clear();
        session.approval_overrides = None;
        session.interruption = None;
        session.compaction_state = None;
        session.pipeline_state = None;
        session.model = None;
        session.permission_mode = None;
        session.last_context_trace = None;
        session.workspace = None;
        return;
    };
    let cursor = bundle.cursor.clone();
    let checkpoint = bundle.projections.checkpoint_at(&cursor).cloned();
    let provider = bundle.projections.provider_at(&cursor).cloned();
    let deferred_tool_activations = bundle.deferred_tool_activations().to_vec();

    session.deferred_tool_activations = deferred_tool_activations;
    session.recent_tools = checkpoint
        .as_ref()
        .map(|projection| projection.recent_tools.clone())
        .unwrap_or_default();
    session.blocked_tools = checkpoint
        .as_ref()
        .map(|projection| projection.blocked_tools.clone())
        .unwrap_or_default();
    session.approval_overrides = checkpoint
        .as_ref()
        .and_then(|projection| projection.approval_overrides.clone());
    session.interruption = checkpoint
        .as_ref()
        .and_then(|projection| projection.interruption.clone());
    session.compaction_state = checkpoint
        .as_ref()
        .and_then(|projection| projection.compaction_state.clone());
    session.pipeline_state = checkpoint
        .as_ref()
        .and_then(|projection| projection.pipeline_state.clone());
    session.model = provider
        .as_ref()
        .and_then(|projection| projection.model.clone());
    session.permission_mode = provider.and_then(|projection| projection.permission_mode);

    // These runtime-bearing legacy projections have no cursor envelope.
    session.last_context_trace = None;
    session.workspace = None;
}

/// Replace the resumable conversation with a coordinator-verified canonical
/// bundle. Runtime-bearing side projections are re-admitted only when their
/// own envelope proves the same causal boundary.
pub fn install_canonical_resume_bundle(
    session: &mut RestoredSession,
    bundle: astra_turn_types::ResumeBundleV1,
) -> Result<(), String> {
    if bundle.cursor.session_id != session.session_id
        || bundle.source != astra_turn_types::ResumeSourceV1::CanonicalJournal
        || !bundle.validates_root()
    {
        return Err("canonical resume bundle does not validate its materialized content".into());
    }
    session.turn_count = bundle.cursor.completed_turn;
    session.conversation_messages.clear();
    session.resume_bundle = Some(bundle);
    apply_selected_resume_bundle(session);
    Ok(())
}

fn local_checkpoint_count(session_id: &str, context: &str) -> Result<u32, String> {
    let count = super::session_checkpoint::read_checkpoint_index(session_id)
        .map_err(|error| {
            format!("{context}: failed to read local checkpoint index for {session_id}: {error}")
        })?
        .len();
    u32::try_from(count).map_err(|_| {
        format!(
            "{context}: local checkpoint index for {session_id} has too many entries: {}",
            count
        )
    })
}

fn recent_tools_from_context_trace(
    trace: Option<&super::session_workspace::ContextTraceSignal>,
) -> Vec<String> {
    let mut tools = Vec::new();
    if let Some(visible_tools) = trace
        .and_then(|signal| signal.tool_surface.as_ref())
        .map(|selection| selection.visible_tools.iter().map(String::as_str))
    {
        append_unique_names(&mut tools, visible_tools);
    }
    tools
}

fn parse_local_checkpoint_entries(local_entries: &[String]) -> Vec<RestoredCheckpoint> {
    local_entries
        .iter()
        .filter_map(|entry| {
            let parts: Vec<&str> = entry.splitn(3, " - ").collect();
            if parts.len() >= 3 {
                let number: u32 = parts[0].trim().parse().ok()?;
                let turn_str = parts[1].trim().strip_prefix("Turn ")?.trim();
                let turn: u32 = turn_str.parse().ok()?;
                let title = parts[2].trim().to_string();
                Some(RestoredCheckpoint {
                    number,
                    turn,
                    title,
                    summary: String::new(),
                    total_tokens: 0,
                })
            } else {
                None
            }
        })
        .collect()
}

#[derive(Debug, Clone, Default)]
struct LocalJournalSummary {
    turn_count: u32,
    total_tokens_in: u64,
    total_tokens_out: u64,
    total_cache_read_tokens: u64,
    total_cache_creation_tokens: u64,
    recent_tools: Vec<String>,
    model: Option<String>,
    permission_mode: Option<String>,
    last_status: String,
}

fn summarize_local_journal(session_id: &str) -> Result<Option<LocalJournalSummary>, String> {
    let digest = crate::session_journal::read_journal_for_digest(session_id);
    let (events, _, _) = match digest {
        Ok(digest) => digest,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(format!(
                "failed to read session journal for {session_id}: {error}"
            ));
        }
    };
    if events.is_empty() {
        return Ok(None);
    }

    let mut summary = LocalJournalSummary {
        last_status: "local".to_string(),
        ..Default::default()
    };
    let mut latest_turn_index: Option<usize> = None;

    for (idx, event) in events.iter().enumerate() {
        if let Some(mode) = event
            .edge_policy
            .as_ref()
            .and_then(|policy| policy.permission_mode.clone())
        {
            summary.permission_mode = Some(mode);
        } else if event.event_type == crate::session_journal::JournalEventType::PermissionAudit
            && event
                .metadata
                .as_ref()
                .and_then(|metadata| metadata.get("kind"))
                .and_then(|kind| kind.as_str())
                == Some("permission_mode_changed")
            && let Some(mode) = event
                .metadata
                .as_ref()
                .and_then(|metadata| metadata.get("to_mode"))
                .and_then(|mode| mode.as_str())
        {
            summary.permission_mode = Some(mode.to_string());
        }

        match event.event_type {
            crate::session_journal::JournalEventType::Turn => {
                summary.turn_count += 1;
                summary.total_tokens_in += event.tokens_in.unwrap_or(0);
                summary.total_tokens_out += event.tokens_out.unwrap_or(0);
                summary.total_cache_read_tokens += event.cache_read_tokens.unwrap_or(0);
                summary.total_cache_creation_tokens += event.cache_creation_tokens.unwrap_or(0);
                if event.model.is_some() {
                    summary.model = event.model.clone();
                }
                latest_turn_index = Some(idx);
            }
            crate::session_journal::JournalEventType::SessionStart if summary.model.is_none() => {
                summary.model = event.model.clone();
            }
            crate::session_journal::JournalEventType::SessionEnd => {
                summary.last_status = "completed".to_string();
            }
            _ => {}
        }
    }

    if let Some(idx) = latest_turn_index
        && let Some(event) = events.get(idx)
    {
        if let Some(tools_used) = event.tools_used.as_ref() {
            append_unique_names(
                &mut summary.recent_tools,
                tools_used.iter().map(String::as_str),
            );
        }
        if summary.recent_tools.is_empty()
            && let Some(tool_calls) = event.tool_calls.as_ref()
        {
            append_unique_names(
                &mut summary.recent_tools,
                tool_calls.iter().map(|call| call.name.as_str()),
            );
        }
    }

    Ok(Some(summary))
}

fn restored_session_from_workspace(
    ws: super::session_workspace::WorkspaceMetadata,
    local_journal: Option<&LocalJournalSummary>,
    recent_tools: Vec<String>,
    checkpoint_count: u32,
    restored_from_cloud: bool,
) -> RestoredSession {
    let workspace = ws.clone();
    let turn_count = local_journal
        .map(|summary| ws.turn_count.max(summary.turn_count))
        .unwrap_or(ws.turn_count);
    let total_tokens_in = local_journal
        .map(|summary| ws.total_tokens_in.max(summary.total_tokens_in))
        .unwrap_or(ws.total_tokens_in);
    let total_tokens_out = local_journal
        .map(|summary| ws.total_tokens_out.max(summary.total_tokens_out))
        .unwrap_or(ws.total_tokens_out);
    let total_cache_read_tokens = local_journal
        .map(|summary| {
            ws.total_cache_read_tokens
                .max(summary.total_cache_read_tokens)
        })
        .unwrap_or(ws.total_cache_read_tokens);
    let total_cache_creation_tokens = local_journal
        .map(|summary| {
            ws.total_cache_creation_tokens
                .max(summary.total_cache_creation_tokens)
        })
        .unwrap_or(ws.total_cache_creation_tokens);
    let permission_mode = ws
        .permission_mode
        .clone()
        .or_else(|| local_journal.and_then(|summary| summary.permission_mode.clone()));
    let model = ws
        .model
        .as_deref()
        .and_then(|model| astra_core::model_override::normalize_model_override(Some(model)))
        .map(str::to_string)
        .or_else(|| local_journal.and_then(|summary| summary.model.clone()));

    RestoredSession {
        session_id: ws.session_id.clone(),
        turn_count,
        total_tokens_in,
        total_tokens_out,
        total_cache_read_tokens,
        total_cache_creation_tokens,
        recent_tools,
        checkpoint_count,
        last_status: ws.status.clone(),
        git_branch: ws.git_branch.clone(),
        model,
        permission_mode,
        title: None,
        restored_from_cloud,
        last_context_trace: ws.last_context_trace.clone(),
        workspace: Some(workspace),
        ..Default::default()
    }
}

impl SessionRestoreService {
    pub async fn list_resumable_sessions(
        &self,
        user_id: &str,
    ) -> Result<Vec<RestoredSession>, String> {
        let pool = match &self.pool {
            Some(p) => p,
            None => return Ok(Vec::new()),
        };

        // A directory item selects a Session; it is not a hydrated resume bundle.
        // Detail recovery and its evidence checks run only after that selection.
        let mut select = sqlx::QueryBuilder::<sqlx::MySql>::new(
            "SELECT s.session_id, s.title, s.status, CAST(s.metadata AS CHAR) AS metadata_json, \
             s.context_completed_turn AS turn_count FROM ",
        );
        crate::auth::session::push_session_activity_source(&mut select, user_id);
        select.push(
            " WHERE s.status IN ('active', 'paused') \
              ORDER BY s.activity_at DESC, s.session_id DESC LIMIT 20",
        );
        let rows = select
            .build()
            .fetch_all(pool)
            .await
            .map_err(|e| format!("list_resumable: {e}"))?;

        let mut sessions = Vec::with_capacity(rows.len());
        for row in &rows {
            let metadata_json =
                mysql_optional_string(row, "list_resumable_sessions", "metadata_json")?;
            let metadata = metadata_json_state(metadata_json.as_deref())?;
            sessions.push(RestoredSession {
                session_id: mysql_string(row, "list_resumable_sessions", "session_id")?,
                title: mysql_optional_string(row, "list_resumable_sessions", "title")?,
                last_status: mysql_string(row, "list_resumable_sessions", "status")?,
                turn_count: non_negative_i64_to_u32(
                    mysql_i64(row, "list_resumable_sessions", "turn_count")?,
                    "list_resumable_sessions",
                    "turn_count",
                )?,
                git_branch: metadata.git_branch,
                model: metadata.model,
                permission_mode: metadata.permission_mode,
                restored_from_cloud: true,
                ..Default::default()
            });
        }
        Ok(sessions)
    }
}

// ─── Session Metadata Decoding ──────────────────────────────────────────────

pub fn extract_session_state_from_metadata(
    metadata_json: &str,
) -> Result<SessionMetadataState, String> {
    // Defense: reject excessively large metadata to prevent DoS
    const MAX_METADATA_SIZE: usize = 512 * 1024; // 512 KB
    if metadata_json.len() > MAX_METADATA_SIZE {
        return Err(format!(
            "session metadata JSON exceeds maximum size: {} > {MAX_METADATA_SIZE}",
            metadata_json.len()
        ));
    }
    let parsed: serde_json::Value = match serde_json::from_str(metadata_json) {
        Ok(v) => v,
        Err(e) => {
            let prefix = &metadata_json[..metadata_json.len().min(200)];
            return Err(format!(
                "session metadata JSON parse failed: {e}; payload_prefix={prefix:?}"
            ));
        }
    };
    let obj = match parsed.as_object() {
        Some(o) => o,
        None => {
            return Err("session metadata JSON must be an object".to_string());
        }
    };
    Ok(SessionMetadataState {
        git_branch: obj
            .get("git_branch")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string()),
        model: obj
            .get("model")
            .and_then(|v| v.as_str())
            .and_then(|s| astra_core::model_override::normalize_model_override(Some(s)))
            .map(str::to_string),
        permission_mode: obj
            .get("permission_mode")
            .and_then(|v| v.as_str())
            .map(str::to_string),
    })
}

// ─── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session_journal::JournalDirGuard;
    use crate::session_workspace;
    use serde_json::json;

    const REAL_SESSION_0AC769_FIXTURE: &str =
        include_str!("../fixtures/real_session_0ac769_min.jsonl");

    fn typed_resume_bundle_from_messages(
        owner_id: &str,
        session_id: &str,
        sequence: u64,
        messages: Vec<serde_json::Value>,
    ) -> astra_turn_types::ResumeBundleV1 {
        let cursor = astra_turn_types::SessionCursorV1 {
            schema_version: astra_turn_types::SESSION_CURSOR_SCHEMA_VERSION,
            owner_id: owner_id.into(),
            session_id: session_id.into(),
            branch_id: astra_turn_types::DEFAULT_CONVERSATION_BRANCH_ID.into(),
            completed_turn: sequence as u32,
            journal_event_seq: sequence,
            conversation_seq: sequence,
            canonical_root_hash: astra_turn_types::canonical_conversation_root(&messages),
            projection_schema: astra_turn_types::CONVERSATION_PROJECTION_SCHEMA_VERSION,
            compaction_generation: 0,
            config_version_id: None,
        };
        astra_turn_types::select_resume_bundle(
            None,
            [astra_turn_types::ResumeCandidateV1 {
                source: astra_turn_types::ResumeSourceV1::Checkpoint,
                cursor,
                conversation_messages: messages,
                materialized_conversation_root_hash: None,
                degraded_reasons: Vec::new(),
                repair_actions: Vec::new(),
                projections: astra_turn_types::ResumeProjectionSetV1::default(),
            }],
        )
        .unwrap()
    }

    struct FakeSessionRestoreRow {
        failed_column: Option<&'static str>,
    }

    impl FakeSessionRestoreRow {
        fn complete() -> Self {
            Self {
                failed_column: None,
            }
        }

        fn fail_on(column: &'static str) -> Self {
            Self {
                failed_column: Some(column),
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

    impl SessionRestoreRow for FakeSessionRestoreRow {
        fn string_column(&self, column: &str) -> Result<String, sqlx::Error> {
            self.fail_if_needed(column)?;
            Ok(match column {
                "status" => "active",
                "session_id" => "sess-1",
                "title" => "Checkpoint",
                _ => unreachable!("unexpected string column: {column}"),
            }
            .to_string())
        }

        fn optional_string_column(&self, column: &str) -> Result<Option<String>, sqlx::Error> {
            self.fail_if_needed(column)?;
            Ok(match column {
                "title" => Some("Resume work".to_string()),
                _ => unreachable!("unexpected optional string column: {column}"),
            })
        }

        fn i64_column(&self, column: &str) -> Result<i64, sqlx::Error> {
            self.fail_if_needed(column)?;
            Ok(match column {
                "turn_count" => 42,
                "total_tokens_in" => 1000,
                _ => unreachable!("unexpected i64 column: {column}"),
            })
        }
    }

    #[test]
    fn cloud_summary_decoding_preserves_values_and_rejects_corrupt_columns() {
        let row = FakeSessionRestoreRow::complete();
        assert_eq!(mysql_string(&row, "summary", "status").unwrap(), "active");
        assert_eq!(
            mysql_optional_string(&row, "summary", "title")
                .unwrap()
                .as_deref(),
            Some("Resume work")
        );
        assert_eq!(mysql_i64(&row, "summary", "turn_count").unwrap(), 42);
        for column in ["status", "title", "turn_count"] {
            let row = FakeSessionRestoreRow::fail_on(column);
            let error = match column {
                "status" => mysql_string(&row, "summary", column).unwrap_err(),
                "title" => mysql_optional_string(&row, "summary", column).unwrap_err(),
                _ => mysql_i64(&row, "summary", column).unwrap_err(),
            };
            assert!(
                error.contains("summary") && error.contains(column),
                "{error}"
            );
        }
    }

    #[test]
    fn canonical_install_rejects_foreign_or_corrupt_bundles_without_mutation() {
        let messages = vec![json!({"role":"assistant", "content":"retained"})];
        let mut bundle = typed_resume_bundle_from_messages("owner", "session", 2, messages.clone());
        bundle.source = astra_turn_types::ResumeSourceV1::CanonicalJournal;
        bundle.materialized_conversation_root_hash =
            Some(astra_turn_types::canonical_conversation_root(&messages));
        let mut restored = RestoredSession {
            session_id: "session".into(),
            ..Default::default()
        };
        restored.total_tokens_in = 17;
        restored.total_cache_read_tokens = 3;
        restored.model = Some("stale-model".into());
        restored.permission_mode = Some("allow".into());
        restored.recent_tools = vec!["stale-tool".into()];
        restored.blocked_tools = vec!["stale-block".into()];
        restored.approval_overrides = Some(json!({"write": "allow"}));
        restored.interruption = Some(json!({"reason": "old"}));
        restored.compaction_state = Some(json!({"old": true}));
        restored.pipeline_state = Some(json!({"old": true}));
        restored.workspace = Some(crate::session_workspace::WorkspaceMetadata::new(
            "session", "/tmp/old",
        ));
        install_canonical_resume_bundle(&mut restored, bundle.clone()).unwrap();
        assert!(restored.model.is_none() && restored.permission_mode.is_none());
        assert!(restored.recent_tools.is_empty() && restored.blocked_tools.is_empty());
        assert!(restored.approval_overrides.is_none() && restored.interruption.is_none());
        assert!(restored.compaction_state.is_none() && restored.pipeline_state.is_none());
        assert!(restored.workspace.is_none() && restored.last_context_trace.is_none());
        assert_eq!(restored.resume_messages(), messages);
        assert_eq!(
            (restored.total_tokens_in, restored.total_cache_read_tokens),
            (17, 3)
        );
        let original = serde_json::to_value(&restored).unwrap();
        let mut corrupt = bundle.clone();
        corrupt.conversation_messages.clear();
        let mut foreign = bundle.clone();
        foreign.cursor.session_id = "foreign".into();
        let mut checkpoint = bundle;
        checkpoint.source = astra_turn_types::ResumeSourceV1::Checkpoint;
        for rejected in [corrupt, foreign, checkpoint] {
            assert!(install_canonical_resume_bundle(&mut restored, rejected).is_err());
            assert_eq!(serde_json::to_value(&restored).unwrap(), original);
        }
    }

    #[test]
    fn cache_token_counts_from_token_usage_json_preserves_cache_split() {
        let raw = json!({
            "input_tokens": 10,
            "cached_input_tokens": 4,
            "cache_creation_tokens": 3,
            "output_tokens": 2,
            "total_tokens": 19,
        })
        .to_string();
        assert_eq!(
            cache_token_counts_from_token_usage_json(&raw, "test").expect("cache split"),
            (Some(4), Some(3))
        );
        assert_eq!(
            cache_token_counts_from_token_usage_json("{}", "test").expect("unknown sample"),
            (None, None)
        );
        let error = cache_token_counts_from_token_usage_json(
            r#"{"cached_input_tokens":-1,"cache_creation_tokens":0}"#,
            "test",
        )
        .expect_err("negative cache counts must fail");
        assert!(error.contains("non-negative"));
    }

    #[test]
    fn restore_cache_subtotals_keep_known_lanes_and_reject_contradictions() {
        assert_eq!(
            cache_token_counts_from_token_usage_json(r#"{"cached_input_tokens":0}"#, "test")
                .unwrap(),
            (Some(0), None)
        );
        let mut read = 4;
        let mut creation = 3;
        for raw in [
            r#"{"output_tokens":7}"#,
            "{}",
            r#"{"cached_input_tokens":null}"#,
        ] {
            assert!(apply_restore_cache_token_usage(
                raw,
                "partial",
                &mut read,
                &mut creation
            ));
            assert_eq!((read, creation), (4, 3));
        }
        assert!(apply_restore_cache_token_usage(
            r#"{"cache_creation_tokens":2}"#,
            "known",
            &mut read,
            &mut creation
        ));
        assert_eq!((read, creation), (4, 5));
        assert!(!apply_restore_cache_token_usage(
            r#"{"input_tokens":10,"cached_input_tokens":2,"cache_creation_tokens":0,"output_tokens":3,"total_tokens":14}"#,
            "contradictory",
            &mut read,
            &mut creation,
        ));
        assert_eq!((read, creation), (4, 5));
    }

    #[test]
    fn restore_cache_token_totals_skip_invalid_rows() {
        let mut cache_read_total = 0;
        let mut cache_creation_total = 0;
        let valid = json!({
            "cached_input_tokens": 4,
            "cache_creation_tokens": 3,
        })
        .to_string();

        assert!(apply_restore_cache_token_usage(
            &valid,
            "event-ok",
            &mut cache_read_total,
            &mut cache_creation_total,
        ));
        assert!(!apply_restore_cache_token_usage(
            "{not-json",
            "event-bad-json",
            &mut cache_read_total,
            &mut cache_creation_total,
        ));
        assert!(!apply_restore_cache_token_usage(
            r#"{"cached_input_tokens":-1,"cache_creation_tokens":0}"#,
            "event-negative",
            &mut cache_read_total,
            &mut cache_creation_total,
        ));

        assert_eq!((cache_read_total, cache_creation_total), (4, 3));
    }

    #[test]
    fn restore_cache_token_totals_skip_overflowing_rows_atomically() {
        let mut cache_read_total = i64::MAX - 1;
        let mut cache_creation_total = 10;
        let overflowing = json!({
            "cached_input_tokens": 2,
            "cache_creation_tokens": 5,
        })
        .to_string();

        assert!(!apply_restore_cache_token_usage(
            &overflowing,
            "event-overflow",
            &mut cache_read_total,
            &mut cache_creation_total,
        ));
        assert_eq!((cache_read_total, cache_creation_total), (i64::MAX - 1, 10));
    }

    #[test]
    fn metadata_json_state_defaults_only_for_absent_or_empty_metadata() {
        assert_eq!(
            metadata_json_state(None).unwrap(),
            SessionMetadataState::default()
        );
        assert_eq!(
            metadata_json_state(Some("")).unwrap(),
            SessionMetadataState::default()
        );

        let state = metadata_json_state(Some(
            r#"{"model":"gpt-5","permission_mode":"accept_edits"}"#,
        ))
        .unwrap();
        assert_eq!(state.model.as_deref(), Some("gpt-5"));
        assert_eq!(state.permission_mode.as_deref(), Some("accept_edits"));
    }

    #[test]
    fn metadata_json_state_fails_loudly_on_corrupt_present_metadata() {
        for (metadata, expected) in [("not json", "parse failed"), ("[]", "must be an object")] {
            let error = metadata_json_state(Some(metadata)).expect_err("metadata should fail");
            assert!(
                error.contains(expected),
                "metadata error should identify `{expected}`: {error}"
            );
        }
    }

    #[test]
    fn non_negative_numeric_conversions_fail_loudly_on_invalid_values() {
        assert_eq!(
            non_negative_i64_to_u64(42, "restore_cloud_session", "total_tokens_in").unwrap(),
            42
        );
        assert_eq!(
            non_negative_i64_to_u32(42, "restore_cloud_session", "turn_count").unwrap(),
            42
        );

        for err in [
            non_negative_i64_to_u64(-1, "restore_cloud_session", "total_tokens_in").unwrap_err(),
            non_negative_i64_to_u32(-1, "restore_cloud_session", "turn_count").unwrap_err(),
            non_negative_i64_to_u32(i64::MAX, "restore_cloud_session", "turn_count").unwrap_err(),
        ] {
            assert!(
                err.contains("expected") && err.contains("column"),
                "error should identify invalid numeric restore value: {err}"
            );
        }
    }

    // ── RestoredSession ──

    #[test]
    fn restored_session_defaults() {
        let s = RestoredSession::default();
        assert_eq!(s.turn_count, 0);
        assert!(s.recent_tools.is_empty());
        assert!(!s.restored_from_cloud);
        assert!(s.last_context_trace.is_none());
    }

    #[test]
    fn restored_session_json_roundtrip() {
        let s = RestoredSession {
            session_id: "sess-1".into(),
            turn_count: 15,
            total_tokens_in: 5000,
            total_tokens_out: 3000,
            recent_tools: vec!["git".into(), "grep".into()],
            checkpoint_count: 3,
            last_status: "active".into(),
            git_branch: Some("main".into()),
            model: Some("gpt-4".into()),
            title: Some("Refactor session".into()),
            restored_from_cloud: true,
            ..Default::default()
        };
        let json = serde_json::to_string(&s).unwrap();
        let loaded: RestoredSession = serde_json::from_str(&json).unwrap();
        assert_eq!(loaded.session_id, "sess-1");
        assert_eq!(loaded.turn_count, 15);
        assert_eq!(loaded.recent_tools.len(), 2);
        assert!(loaded.restored_from_cloud);
    }

    // ── RestoredCheckpoint ──

    #[test]
    fn restored_checkpoint_json_roundtrip() {
        let c = RestoredCheckpoint {
            number: 3,
            turn: 15,
            title: "Phase A complete".into(),
            summary: "Finished token efficiency work".into(),
            total_tokens: 50000,
        };
        let json = serde_json::to_string(&c).unwrap();
        let loaded: RestoredCheckpoint = serde_json::from_str(&json).unwrap();
        assert_eq!(loaded.number, 3);
        assert_eq!(loaded.total_tokens, 50000);
    }

    // ── SessionRestoreService (local-only mode) ──

    #[tokio::test]
    async fn local_only_restore_nonexistent_returns_none() {
        let svc = SessionRestoreService::local_only();
        let result = svc
            .restore_local_session("nonexistent-session")
            .await
            .unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn local_only_list_checkpoints_empty() {
        let svc = SessionRestoreService::local_only();
        let ckpts = svc
            .list_local_checkpoints("nonexistent-session")
            .await
            .unwrap();
        assert!(ckpts.is_empty());
    }

    #[tokio::test]
    async fn local_only_list_checkpoints_unreadable_index_fails_loudly() {
        let tmp = tempfile::tempdir().unwrap();
        let _guard = JournalDirGuard::new(tmp.path());
        let sid = uuid::Uuid::new_v4().to_string();
        let checkpoint_index = session_workspace::workspace_dir_for(&sid)
            .join("checkpoints")
            .join("index.md");
        std::fs::create_dir_all(&checkpoint_index).unwrap();

        let svc = SessionRestoreService::local_only();
        let error = svc
            .list_local_checkpoints(&sid)
            .await
            .expect_err("unreadable checkpoint index must fail checkpoint listing");

        assert!(
            error.contains("failed to read local checkpoint index"),
            "checkpoint listing error should identify local index failure: {error}"
        );
        assert!(
            error.contains("list_local_checkpoints"),
            "checkpoint listing error should include context: {error}"
        );
    }

    #[tokio::test]
    async fn local_only_list_resumable_empty() {
        let svc = SessionRestoreService::local_only();
        let sessions = svc.list_resumable_sessions("user1").await.unwrap();
        assert!(sessions.is_empty());
    }

    // ── Integration-style test with real workspace ──

    #[tokio::test]
    async fn restore_from_local_workspace() {
        // This test depends on whether workspace files exist
        // Just verify the function doesn't panic and returns a valid type
        let svc = SessionRestoreService::local_only();

        // Use a UUID that doesn't exist → should return None
        let result = svc
            .restore_local_session("00000000-0000-0000-0000-000000000000")
            .await
            .unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn local_only_restore_falls_back_to_real_session_journal_without_workspace() {
        let tmp = tempfile::tempdir().unwrap();
        let _guard = JournalDirGuard::new(tmp.path());
        let sid = "0ac7696c-8a67-4e9f-b7bb-88b3bf7b59a0";
        let path = crate::session_journal::journal_file_path(sid);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, REAL_SESSION_0AC769_FIXTURE).unwrap();

        let svc = SessionRestoreService::local_only();
        let restored = svc
            .restore_local_session(sid)
            .await
            .unwrap()
            .expect("journal-only session should restore");

        assert_eq!(restored.session_id, sid);
        assert_eq!(restored.turn_count, 1);
        assert_eq!(restored.total_tokens_in, 33_659);
        assert_eq!(restored.total_tokens_out, 2_855);
        assert_eq!(restored.recent_tools, vec!["git", "read_file", "grep"]);
        assert_eq!(restored.model.as_deref(), Some("glm-5.1"));
        assert_eq!(restored.last_status, "completed");
        assert!(!restored.restored_from_cloud);
    }

    #[tokio::test]
    async fn local_restore_uses_real_session_journal_tools_when_workspace_has_no_trace() {
        let tmp = tempfile::tempdir().unwrap();
        let _guard = JournalDirGuard::new(tmp.path());
        let sid = "0ac7696c-8a67-4e9f-b7bb-88b3bf7b59a0";
        let path = crate::session_journal::journal_file_path(sid);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, REAL_SESSION_0AC769_FIXTURE).unwrap();

        let ws = session_workspace::WorkspaceMetadata::with_context(sid, "glm-5.1", "/repo", None);
        session_workspace::write_workspace(&ws).unwrap();

        let svc = SessionRestoreService::local_only();
        let restored = svc
            .restore_local_session(sid)
            .await
            .unwrap()
            .expect("workspace-backed session should restore");

        assert_eq!(restored.recent_tools, vec!["git", "read_file", "grep"]);
        assert_eq!(restored.turn_count, 1);
        assert_eq!(restored.total_tokens_in, 33_659);
        assert_eq!(restored.total_tokens_out, 2_855);
    }

    #[tokio::test]
    async fn local_restore_corrupt_workspace_falls_back_to_journal() {
        let tmp = tempfile::tempdir().unwrap();
        let _guard = JournalDirGuard::new(tmp.path());
        let sid = uuid::Uuid::new_v4().to_string();

        let writer = crate::session_journal::JournalWriter::new(&sid).unwrap();
        writer
            .append(&crate::session_journal::JournalEvent::session_start(
                Some(&sid),
                Some("test-model"),
            ))
            .unwrap();
        writer
            .append(&crate::session_journal::JournalEvent::turn(
                Some(&sid),
                1,
                Some("test-model"),
                "continue",
                "restored",
                0,
                10,
                5,
                5,
            ))
            .unwrap();

        let workspace_dir = session_workspace::workspace_dir_for(&sid);
        std::fs::create_dir_all(&workspace_dir).unwrap();
        std::fs::write(workspace_dir.join("workspace.yaml"), ":\nnot-valid-yaml").unwrap();

        let svc = SessionRestoreService::local_only();
        let restored = svc
            .restore_local_session(&sid)
            .await
            .unwrap()
            .expect("corrupt workspace should still restore from journal");
        assert_eq!(restored.session_id, sid);
        assert_eq!(restored.turn_count, 1);
        assert_eq!(restored.model.as_deref(), Some("test-model"));
        assert_eq!(restored.last_status, "local");
        assert!(!restored.restored_from_cloud);
    }

    #[tokio::test]
    async fn local_restore_unreadable_checkpoint_index_fails_loudly() {
        let tmp = tempfile::tempdir().unwrap();
        let _guard = JournalDirGuard::new(tmp.path());
        let sid = uuid::Uuid::new_v4().to_string();

        let ws = session_workspace::WorkspaceMetadata::with_context(&sid, "gpt-5", "/repo", None);
        session_workspace::write_workspace(&ws).unwrap();
        let checkpoint_index = session_workspace::workspace_dir_for(&sid)
            .join("checkpoints")
            .join("index.md");
        std::fs::create_dir_all(&checkpoint_index).unwrap();

        let svc = SessionRestoreService::local_only();
        let error = svc
            .restore_local_session(&sid)
            .await
            .expect_err("unreadable checkpoint index must fail restore");

        assert!(
            error.contains("failed to read local checkpoint index"),
            "restore error should identify checkpoint index failure: {error}"
        );
        assert!(
            error.contains("restore_local_session"),
            "restore error should include context: {error}"
        );
    }

    #[tokio::test]
    async fn local_restore_recovers_permission_mode_from_journal() {
        let tmp = tempfile::tempdir().unwrap();
        let _guard = JournalDirGuard::new(tmp.path());
        let sid = uuid::Uuid::new_v4().to_string();

        let writer = crate::session_journal::JournalWriter::new(&sid).unwrap();
        let mut start =
            crate::session_journal::JournalEvent::session_start(Some(&sid), Some("test-model"));
        start.edge_policy = Some(crate::session_journal::EdgePolicySnapshot {
            permission_mode: Some("accept_edits".into()),
            cloud_policy_version: None,
            rules_fingerprint: None,
        });
        writer.append(&start).unwrap();
        writer
            .append(&crate::session_journal::JournalEvent::permission_audit(
                Some(&sid),
                Some(1),
                serde_json::json!({
                    "kind": "permission_mode_changed",
                    "from_mode": "accept_edits",
                    "to_mode": "plan",
                    "source": "test",
                    "changed": true
                }),
            ))
            .unwrap();
        writer
            .append(&crate::session_journal::JournalEvent::turn(
                Some(&sid),
                1,
                Some("test-model"),
                "continue",
                "restored",
                0,
                10,
                5,
                5,
            ))
            .unwrap();

        let svc = SessionRestoreService::local_only();
        let restored = svc
            .restore_local_session(&sid)
            .await
            .unwrap()
            .expect("journal-only session should restore");

        assert_eq!(restored.permission_mode.as_deref(), Some("plan"));
    }

    #[tokio::test]
    async fn local_restore_uses_journal_model_when_workspace_model_is_symbolic_default() {
        let tmp = tempfile::tempdir().unwrap();
        let _guard = JournalDirGuard::new(tmp.path());
        let sid = uuid::Uuid::new_v4().to_string();

        let writer = crate::session_journal::JournalWriter::new(&sid).unwrap();
        writer
            .append(&crate::session_journal::JournalEvent::session_start(
                Some(&sid),
                Some("gpt-5"),
            ))
            .unwrap();
        writer
            .append(&crate::session_journal::JournalEvent::turn(
                Some(&sid),
                1,
                Some("gpt-5"),
                "continue",
                "restored",
                0,
                10,
                5,
                5,
            ))
            .unwrap();
        let mut ws = session_workspace::WorkspaceMetadata::new(&sid, "default");
        ws.turn_count = 1;
        session_workspace::write_workspace(&ws).unwrap();

        let svc = SessionRestoreService::local_only();
        let restored = svc
            .restore_local_session(&sid)
            .await
            .unwrap()
            .expect("workspace-backed session should restore");

        assert_eq!(restored.model.as_deref(), Some("gpt-5"));
    }

    #[tokio::test]
    async fn local_restore_unreadable_journal_returns_error() {
        let tmp = tempfile::tempdir().unwrap();
        let _guard = JournalDirGuard::new(tmp.path());
        let sid = uuid::Uuid::new_v4().to_string();

        let path = crate::session_journal::journal_file_path(&sid);
        std::fs::create_dir_all(&path).unwrap();

        let svc = SessionRestoreService::local_only();
        let error = svc
            .restore_local_session(&sid)
            .await
            .expect_err("unreadable journal should fail local restore");

        assert!(error.contains("failed to read session journal"), "{error}");
    }

    // ── Restored session field coverage ──

    #[test]
    fn restored_session_cloud_flag_distinguishes_source() {
        let local = RestoredSession {
            restored_from_cloud: false,
            ..Default::default()
        };
        let cloud = RestoredSession {
            restored_from_cloud: true,
            ..Default::default()
        };
        assert!(!local.restored_from_cloud);
        assert!(cloud.restored_from_cloud);
    }

    #[test]
    fn typed_restore_response_serializes_long_history_once() {
        let messages = (0..2_048)
            .map(|index| serde_json::json!({"role": "user", "content": format!("m-{index}")}))
            .collect::<Vec<_>>();
        let session = RestoredSession {
            session_id: "long-session".into(),
            resume_bundle: Some(typed_resume_bundle_from_messages(
                "owner-1",
                "long-session",
                1_024,
                messages,
            )),
            ..Default::default()
        };

        let encoded = serde_json::to_value(&session).unwrap();
        assert!(
            encoded.get("conversation_messages").is_none(),
            "the generation-zero field must stay absent from a typed response"
        );
        assert_eq!(
            encoded["resume_bundle"]["conversation_messages"]
                .as_array()
                .unwrap()
                .len(),
            2_048
        );
    }

    #[test]
    fn restored_checkpoint_ordering() {
        let ckpts = [
            RestoredCheckpoint {
                number: 1,
                turn: 5,
                title: "First".into(),
                summary: String::new(),
                total_tokens: 1000,
            },
            RestoredCheckpoint {
                number: 2,
                turn: 10,
                title: "Second".into(),
                summary: String::new(),
                total_tokens: 3000,
            },
        ];
        assert!(ckpts[0].turn < ckpts[1].turn);
        assert!(ckpts[0].total_tokens < ckpts[1].total_tokens);
    }

    #[test]
    fn parse_local_checkpoint_entries_reads_index_format() {
        let parsed = parse_local_checkpoint_entries(&[
            "001 - Turn 5 - First checkpoint".to_string(),
            "002 - Turn 9 - Second checkpoint".to_string(),
        ]);
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0].number, 1);
        assert_eq!(parsed[0].turn, 5);
        assert_eq!(parsed[0].title, "First checkpoint");
    }

    // ── Restore field completeness ──

    #[test]
    fn restored_session_all_fields_populated() {
        let s = RestoredSession {
            session_id: "s-full".into(),
            turn_count: 42,
            total_tokens_in: 100_000,
            total_tokens_out: 80_000,
            recent_tools: vec!["bash".into(), "grep".into(), "read_file".into()],
            checkpoint_count: 5,
            last_status: "active".into(),
            git_branch: Some("feature/resume".into()),
            model: Some("claude-3".into()),
            title: Some("Implement session resume".into()),
            restored_from_cloud: false,
            ..Default::default()
        };
        // Verify every field survives serialization
        let json = serde_json::to_string(&s).unwrap();
        let loaded: RestoredSession = serde_json::from_str(&json).unwrap();
        assert_eq!(loaded.session_id, "s-full");
        assert_eq!(loaded.turn_count, 42);
        assert_eq!(loaded.total_tokens_in, 100_000);
        assert_eq!(loaded.total_tokens_out, 80_000);
        assert_eq!(loaded.recent_tools.len(), 3);
        assert_eq!(loaded.checkpoint_count, 5);
        assert_eq!(loaded.last_status, "active");
        assert_eq!(loaded.git_branch.as_deref(), Some("feature/resume"));
        assert_eq!(loaded.model.as_deref(), Some("claude-3"));
        assert_eq!(loaded.title.as_deref(), Some("Implement session resume"));
        assert!(!loaded.restored_from_cloud);
    }

    #[test]
    fn restored_session_partial_fields_default_safely() {
        // Simulate a cloud restore with minimal data
        let s = RestoredSession {
            session_id: "s-partial".into(),
            turn_count: 3,
            last_status: "active".into(),
            restored_from_cloud: true,
            ..Default::default()
        };
        assert!(s.recent_tools.is_empty());
        assert!(s.git_branch.is_none());
        assert!(s.model.is_none());
        assert!(s.title.is_none());
        assert_eq!(s.total_tokens_in, 0);
        assert_eq!(s.total_tokens_out, 0);
        assert_eq!(s.checkpoint_count, 0);
    }

    #[test]
    fn restored_checkpoint_covers_listing_fields() {
        let ckpt = RestoredCheckpoint {
            number: 7,
            turn: 35,
            title: "Checkpoint after refactor".into(),
            summary: "All tests passing after auth refactor".into(),
            total_tokens: 120_000,
        };
        // Keep the local checkpoint listing wire fields.
        assert_eq!(ckpt.number, 7);
        assert_eq!(ckpt.turn, 35);
        assert!(!ckpt.title.is_empty());
        assert!(!ckpt.summary.is_empty());
        assert!(ckpt.total_tokens > 0);
    }

    #[test]
    fn extract_session_state_from_metadata_ignores_non_plan_trace_fields() {
        let metadata = r#"{
            "executing_plan": "{\"subtasks\":[]}",
            "git_branch": "feature/cloud-sync",
            "model": "gpt-5.4",
            "last_context_trace": {
                "turn_id": "turn-9",
                "visible_tools": ["lsp", "view"],
                "memory_query": "resume trace persistence",
                "memories_selected": 2,
                "compressed_turns": 1,
                "compression_ratio": 0.72,
                "budget_pressure": 0.88,
                "total_tokens_used": 12345
            }
        }"#;
        let state = extract_session_state_from_metadata(metadata).unwrap();
        assert_eq!(state.git_branch.as_deref(), Some("feature/cloud-sync"));
        assert_eq!(state.model.as_deref(), Some("gpt-5.4"));
    }

    #[test]
    fn extract_session_state_from_metadata_drops_symbolic_default_model() {
        let state = extract_session_state_from_metadata(r#"{"model":"default"}"#).unwrap();
        assert_eq!(state.model, None);
    }

    #[test]
    fn restored_session_minimal_json() {
        // Required fields without serde(default)
        let json = r#"{
            "session_id":"s1","last_status":"active","turn_count":0,
            "total_tokens_in":0,"total_tokens_out":0,"recent_tools":[],
            "checkpoint_count":0,"restored_from_cloud":false
        }"#;
        let s: RestoredSession = serde_json::from_str(json).unwrap();
        assert_eq!(s.session_id, "s1");
        assert_eq!(s.turn_count, 0);
        assert!(s.conversation_messages.is_empty());
        assert!(s.blocked_tools.is_empty());
    }

    fn minimal_session_json(extra: &str) -> String {
        format!(
            r#"{{"session_id":"s1","last_status":"x","turn_count":0,
            "total_tokens_in":0,"total_tokens_out":0,"recent_tools":[],
            "checkpoint_count":0,"restored_from_cloud":false{}}}"#,
            if extra.is_empty() {
                "".to_string()
            } else {
                format!(",{extra}")
            }
        )
    }

    #[test]
    fn restored_session_with_extra_fields_ignored() {
        let json = minimal_session_json(r#""unknown_field":"value","another":42"#);
        let s: RestoredSession = serde_json::from_str(&json).unwrap();
        assert_eq!(s.session_id, "s1");
    }

    #[test]
    fn restored_session_null_optional_fields() {
        let json = minimal_session_json(r#""git_branch":null,"model":null,"title":null"#);
        let s: RestoredSession = serde_json::from_str(&json).unwrap();
        assert!(s.git_branch.is_none());
        assert!(s.model.is_none());
        assert!(s.title.is_none());
    }

    #[test]
    fn restored_checkpoint_serialization_roundtrip() {
        let ckpt = RestoredCheckpoint {
            number: 5,
            turn: 10,
            title: "Phase 1 complete".into(),
            summary: "Implemented auth module".into(),
            total_tokens: 50000,
        };
        let json = serde_json::to_string(&ckpt).unwrap();
        let loaded: RestoredCheckpoint = serde_json::from_str(&json).unwrap();
        assert_eq!(loaded.number, 5);
        assert_eq!(loaded.turn, 10);
        assert_eq!(loaded.title, "Phase 1 complete");
    }

    #[test]
    fn restored_session_conversation_messages_preserved() {
        let s = RestoredSession {
            session_id: "s1".into(),
            conversation_messages: vec![
                serde_json::json!({"role": "user", "content": "hi"}),
                serde_json::json!({"role": "assistant", "content": "hello"}),
            ],
            ..Default::default()
        };
        let json = serde_json::to_string(&s).unwrap();
        let loaded: RestoredSession = serde_json::from_str(&json).unwrap();
        assert_eq!(loaded.conversation_messages.len(), 2);
        assert_eq!(loaded.conversation_messages[0]["role"], "user");
    }

    #[test]
    fn restored_session_blocked_tools_roundtrip() {
        let s = RestoredSession {
            session_id: "s1".into(),
            blocked_tools: vec!["dangerous_tool".into()],
            ..Default::default()
        };
        let json = serde_json::to_string(&s).unwrap();
        let loaded: RestoredSession = serde_json::from_str(&json).unwrap();
        assert_eq!(loaded.blocked_tools, vec!["dangerous_tool"]);
    }

    #[test]
    fn restored_session_skip_serializing_empty_collections() {
        let s = RestoredSession::default();
        let json = serde_json::to_string(&s).unwrap();
        // Empty vecs should not appear in serialized JSON
        assert!(!json.contains("conversation_messages"));
        assert!(!json.contains("blocked_tools"));
    }

    #[test]
    fn recent_tools_from_context_trace_uses_visible_tools() {
        let trace = session_workspace::ContextTraceSignal {
            turn_id: "turn-7".into(),
            captured_at: None,
            tool_surface: Some(session_workspace::ContextTraceToolSurface {
                tools_available: 8,
                visible_tools: vec!["bash".into(), "grep".into(), "bash".into()],
                surface_scope: "latest_round".into(),
                latency_ms: 12,
            }),
            memory: None,
            history: None,
            budget: None,
            timing: None,
        };

        assert_eq!(
            recent_tools_from_context_trace(Some(&trace)),
            vec!["bash".to_string(), "grep".to_string()]
        );
        assert!(recent_tools_from_context_trace(None).is_empty());
    }

    #[test]
    fn recent_tools_from_context_trace_drops_blank_tool_names() {
        let trace = session_workspace::ContextTraceSignal {
            turn_id: "turn-blank-tools".into(),
            captured_at: None,
            tool_surface: Some(session_workspace::ContextTraceToolSurface {
                tools_available: 5,
                visible_tools: vec![
                    "".into(),
                    "  ".into(),
                    " rg ".into(),
                    "rg".into(),
                    " bash".into(),
                ],
                surface_scope: "latest_round".into(),
                latency_ms: 3,
            }),
            memory: None,
            history: None,
            budget: None,
            timing: None,
        };

        assert_eq!(
            recent_tools_from_context_trace(Some(&trace)),
            vec!["rg".to_string(), "bash".to_string()],
            "resume recent_tools must contain canonical non-empty tool names only"
        );
    }
}
