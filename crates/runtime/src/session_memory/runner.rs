use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use astra_prompts::memory_proto::{MemoryEntry, NS_SESSION, ST_ACTIVE};
use astra_services::SessionArtifactStore;
use astra_services::session_journal::{
    SessionMemoryExtractionErrorReason, SessionMemoryExtractionSource,
};
use astra_turn_core::cloud_session_memory_extract::{
    SESSION_MEMORY_TEMPLATE, build_extraction_prompt, extract_section,
};
use astra_turn_types::{
    InferencePurpose, is_runtime_owned_message, session_facts::SessionFacts,
};

use crate::memory_hooks::relevance::LlmConnParams;
use crate::turn::cloud::memoria_compact::{MemoriaMemory, MemoriaPort};
use crate::turn::llm::client::{
    LlmCall, call_llm_nonstream, global_llm_client, redact_provider_secrets,
};

pub const SESSION_MEMORY_PREFIX: &str = "[@session/active]";
pub const SESSION_MEMORY_MEMORIA_TYPE: &str = "working";
const SESSION_MEMORY_SCHEMA_VERSION: u16 = 2;
const CURRENT_SESSION_MEMORY_RETRIEVE_TOP_K: usize = 64;
const LOCAL_SESSION_MEMORY_METADATA_FILE: &str = "session-memory.meta.json";

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionNarrative {
    #[serde(default)]
    pub session_title: String,
    #[serde(default)]
    pub task_spec: String,
    #[serde(default)]
    pub current_state: Vec<String>,
    #[serde(default)]
    pub active_goals: Vec<String>,
    #[serde(default)]
    pub pending_todos: Vec<String>,
    #[serde(default)]
    pub corrections: Vec<String>,
    #[serde(default)]
    pub learnings: Vec<String>,
}

/// Sparse semantic update returned by the extraction model. Missing fields
/// preserve the current narrative; an explicitly empty string/list clears a
/// field. This keeps the model focused on facts that changed this turn instead
/// of regenerating a large markdown document and accidentally reviving stale
/// closed-loop history.
#[derive(Debug, Clone, Default, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct SessionNarrativePatch {
    #[serde(default)]
    session_title: Option<String>,
    #[serde(default)]
    task_spec: Option<String>,
    #[serde(default)]
    current_state: Option<Vec<String>>,
    #[serde(default)]
    active_goals: Option<Vec<String>>,
    #[serde(default)]
    pending_todos: Option<Vec<String>>,
    #[serde(default)]
    corrections: Option<Vec<String>>,
    #[serde(default)]
    learnings: Option<Vec<String>>,
}

impl SessionNarrativePatch {
    fn is_empty(&self) -> bool {
        self.session_title.is_none()
            && self.task_spec.is_none()
            && self.current_state.is_none()
            && self.active_goals.is_none()
            && self.pending_todos.is_none()
            && self.corrections.is_none()
            && self.learnings.is_none()
    }
}

impl SessionNarrative {
    fn apply_patch(&mut self, patch: SessionNarrativePatch) {
        if let Some(value) = patch.session_title {
            self.session_title = value;
        }
        if let Some(value) = patch.task_spec {
            self.task_spec = value;
        }
        if let Some(value) = patch.current_state {
            self.current_state = value;
        }
        if let Some(value) = patch.active_goals {
            self.active_goals = value;
        }
        if let Some(value) = patch.pending_todos {
            self.pending_todos = value;
        }
        if let Some(value) = patch.corrections {
            self.corrections = value;
        }
        if let Some(value) = patch.learnings {
            self.learnings = value;
        }
        normalize_narrative(self);
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionMemorySnapshot {
    pub schema_version: u16,
    pub session_id: String,
    pub updated_turn: u32,
    #[serde(default)]
    pub facts: SessionFacts,
    pub narrative: SessionNarrative,
}

/// Prompt-facing current-session snapshot with its real freshness boundary.
/// `updated_turn` is optional only for a local artifact whose metadata is
/// missing; callers must not replace an unknown snapshot turn with the current
/// turn, because that would make stale state appear freshly verified.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadedSessionMemory {
    pub content: String,
    pub updated_turn: Option<u32>,
}

fn default_stable_memory_epoch() -> u32 {
    1
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionMemoryArtifactMetadata {
    pub session_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_local_refresh_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_snapshot_source: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_extracted_turn: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_extraction_source: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_remote_sync_status: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_remote_sync_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_remote_sync_detail: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_selector_model: Option<String>,
    #[serde(default = "default_stable_memory_epoch")]
    pub stable_memory_epoch: u32,
}

impl Default for SessionMemoryArtifactMetadata {
    fn default() -> Self {
        Self {
            session_id: String::new(),
            last_local_refresh_at: None,
            current_snapshot_source: None,
            last_extracted_turn: None,
            last_extraction_source: None,
            last_remote_sync_status: None,
            last_remote_sync_at: None,
            last_remote_sync_detail: None,
            last_selector_model: None,
            stable_memory_epoch: default_stable_memory_epoch(),
        }
    }
}

impl SessionMemorySnapshot {
    fn from_markdown(
        session_id: &str,
        content: &str,
        updated_turn: u32,
        facts: SessionFacts,
    ) -> Self {
        let mut narrative = SessionNarrative {
            session_title: extract_scalar_section(content, "Session Title"),
            task_spec: extract_scalar_section(content, "Task Specification"),
            current_state: extract_list_section(content, "Current State"),
            active_goals: extract_list_section(content, "Active Goals"),
            pending_todos: extract_list_section(content, "Pending Todos"),
            corrections: extract_list_section(content, "Errors & Corrections"),
            learnings: extract_list_section(content, "Learnings"),
        };
        if narrative_is_empty(&narrative) {
            let fallback = single_line(&fallback_body_text(content));
            if !fallback.is_empty() {
                narrative.current_state.push(fallback);
            }
        }
        normalize_narrative(&mut narrative);
        Self {
            schema_version: SESSION_MEMORY_SCHEMA_VERSION,
            session_id: session_id.to_string(),
            updated_turn,
            facts,
            narrative,
        }
    }

    fn abstract_line(&self) -> String {
        let primary = first_non_empty([
            self.narrative.current_state.first().map(String::as_str),
            self.narrative.active_goals.first().map(String::as_str),
            (!self.narrative.task_spec.is_empty()).then_some(self.narrative.task_spec.as_str()),
            (!self.narrative.session_title.is_empty())
                .then_some(self.narrative.session_title.as_str()),
        ])
        .unwrap_or("Active session state");
        let mut abstract_line = single_line(primary);
        if abstract_line.len() < 24 {
            abstract_line = format!("Session {}: {abstract_line}", self.session_id);
        }
        truncate_chars(&abstract_line, 150)
    }

    fn overview(&self) -> String {
        let mut sections = Vec::new();
        if !self.narrative.task_spec.is_empty() {
            sections.push(format!(
                "Task specification: {}",
                single_line(&self.narrative.task_spec)
            ));
        }
        push_capped_section(
            &mut sections,
            "Current state",
            &self.narrative.current_state,
            3,
        );
        push_capped_section(
            &mut sections,
            "Open loops",
            &self.narrative.pending_todos,
            4,
        );
        push_capped_section(&mut sections, "Corrections", &self.narrative.corrections, 4);
        push_capped_section(&mut sections, "Learnings", &self.narrative.learnings, 3);
        let facts = self.facts.to_injection();
        if !facts.trim().is_empty() {
            sections.push(facts.trim().to_string());
        }
        sections.join("\n")
    }

    fn overview_without_facts(&self) -> String {
        let mut sections = Vec::new();
        if !self.narrative.task_spec.is_empty() {
            sections.push(format!(
                "Task specification: {}",
                single_line(&self.narrative.task_spec)
            ));
        }
        push_capped_section(
            &mut sections,
            "Current state",
            &self.narrative.current_state,
            3,
        );
        push_capped_section(
            &mut sections,
            "Open loops",
            &self.narrative.pending_todos,
            4,
        );
        push_capped_section(&mut sections, "Corrections", &self.narrative.corrections, 4);
        push_capped_section(&mut sections, "Learnings", &self.narrative.learnings, 3);
        sections.join("\n")
    }

    fn to_memory_entry(&self) -> MemoryEntry {
        let detail = serde_json::to_string_pretty(self)
            .unwrap_or_else(|_| "{\"error\":\"session-memory-encode\"}".to_string());
        MemoryEntry::new_layered(
            NS_SESSION,
            ST_ACTIVE,
            &self.abstract_line(),
            Some(&self.overview()),
            Some(&detail),
        )
    }

    fn to_markdown(&self) -> String {
        let files_from_facts = self
            .facts
            .active_files
            .iter()
            .rev()
            .take(10)
            .map(|f| format!("{} {} (t{})", f.last_action, f.path, f.turn))
            .collect::<Vec<_>>();
        let corrections = if let Some(last_error) = &self.facts.error_state.last_error {
            let mut out = self.narrative.corrections.clone();
            if !out.iter().any(|line| line.contains(last_error)) {
                out.push(format!(
                    "System observed {} errors; latest: {}",
                    self.facts.error_state.total_errors, last_error
                ));
            }
            out
        } else {
            self.narrative.corrections.clone()
        };
        let mut sections = vec!["# Session Memory".to_string()];
        push_scalar_markdown_section(
            &mut sections,
            "Session Title",
            &self.narrative.session_title,
        );
        push_list_markdown_section(&mut sections, "Active Goals", &self.narrative.active_goals);
        push_list_markdown_section(
            &mut sections,
            "Pending Todos",
            &self.narrative.pending_todos,
        );
        push_list_markdown_section(
            &mut sections,
            "Current State",
            &self.narrative.current_state,
        );
        push_scalar_markdown_section(
            &mut sections,
            "Task Specification",
            &self.narrative.task_spec,
        );
        push_list_markdown_section(&mut sections, "Files and Functions", &files_from_facts);
        push_list_markdown_section(&mut sections, "Errors & Corrections", &corrections);
        push_list_markdown_section(&mut sections, "Learnings", &self.narrative.learnings);
        sections.join("\n\n")
    }
}

/// What the worker produced.
pub(crate) enum ExtractionArtifacts {
    Persisted {
        source: SessionMemoryExtractionSource,
        bytes_written: u64,
        store_attempt: u32,
        content: String,
        selector_model: Option<String>,
        failed_candidates: Vec<LlmCandidateFailure>,
    },
    LlmFailedPersistedFallback {
        error_reason: SessionMemoryExtractionErrorReason,
        error_detail: Option<String>,
        bytes_written: u64,
        store_attempt: u32,
        content: String,
        selector_model: Option<String>,
        failed_candidates: Vec<LlmCandidateFailure>,
    },
    PersistFailed {
        error_reason: SessionMemoryExtractionErrorReason,
        persist_error_detail: Option<String>,
        llm_error_reason: Option<SessionMemoryExtractionErrorReason>,
        llm_error_detail: Option<String>,
        selector_model: Option<String>,
        failed_candidates: Vec<LlmCandidateFailure>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct LlmExtractionFailure {
    reason: SessionMemoryExtractionErrorReason,
    detail: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct StoreSessionMemoryFailure {
    reason: SessionMemoryExtractionErrorReason,
    detail: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LlmCandidateFailure {
    pub(crate) model_name: String,
    pub(crate) reason: SessionMemoryExtractionErrorReason,
    pub(crate) detail: Option<String>,
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn run_extraction(
    memoria: &Arc<dyn MemoriaPort>,
    session_id: &str,
    messages: &[Value],
    turn_number: usize,
    current_memory: &str,
    session_facts: &SessionFacts,
    memory_model_params: &[LlmConnParams],
    llm_timeout: Duration,
    max_output_tokens: usize,
) -> ExtractionArtifacts {
    let filtered_messages = session_memory_extraction_messages(messages);
    let messages = filtered_messages.as_slice();
    let base_memory = if current_memory.trim().is_empty() {
        SESSION_MEMORY_TEMPLATE.to_string()
    } else {
        current_memory.to_string()
    };
    let fallback = build_rule_fallback_memory(&base_memory, messages, turn_number);

    if memory_model_params.is_empty() {
        let fallback = canonicalize_session_memory_markdown(
            session_id,
            &fallback,
            turn_number as u32,
            session_facts,
        );
        return match store_session_memory(
            memoria,
            session_id,
            turn_number as u32,
            session_facts,
            &fallback,
        )
        .await
        {
            Ok((bytes_written, store_attempt)) => ExtractionArtifacts::Persisted {
                source: SessionMemoryExtractionSource::RuleFallback,
                bytes_written,
                store_attempt,
                content: fallback,
                selector_model: None,
                failed_candidates: Vec::new(),
            },
            Err(error) => ExtractionArtifacts::PersistFailed {
                error_reason: error.reason,
                persist_error_detail: error.detail,
                llm_error_reason: None,
                llm_error_detail: None,
                selector_model: None,
                failed_candidates: Vec::new(),
            },
        };
    }

    let mut failed_candidates = Vec::new();
    for params in memory_model_params {
        match update_memory_with_llm(
            &base_memory,
            messages,
            params,
            llm_timeout,
            max_output_tokens,
        )
        .await
        {
            Ok(updated) => {
                let updated = canonicalize_session_memory_markdown(
                    session_id,
                    &updated,
                    turn_number as u32,
                    session_facts,
                );
                match store_session_memory(
                    memoria,
                    session_id,
                    turn_number as u32,
                    session_facts,
                    &updated,
                )
                .await
                {
                    Ok((bytes_written, store_attempt)) => {
                        return ExtractionArtifacts::Persisted {
                            source: SessionMemoryExtractionSource::Llm,
                            bytes_written,
                            store_attempt,
                            content: updated,
                            selector_model: Some(params.model_name.clone()),
                            failed_candidates,
                        };
                    }
                    Err(error) => {
                        return ExtractionArtifacts::PersistFailed {
                            error_reason: error.reason,
                            persist_error_detail: error.detail,
                            llm_error_reason: None,
                            llm_error_detail: None,
                            selector_model: Some(params.model_name.clone()),
                            failed_candidates,
                        };
                    }
                }
            }
            Err(error) => failed_candidates.push(LlmCandidateFailure {
                model_name: params.model_name.clone(),
                reason: error.reason,
                detail: error.detail,
            }),
        }
    }

    let last_failure = failed_candidates.last();
    let fallback = canonicalize_session_memory_markdown(
        session_id,
        &fallback,
        turn_number as u32,
        session_facts,
    );
    match store_session_memory(
        memoria,
        session_id,
        turn_number as u32,
        session_facts,
        &fallback,
    )
    .await
    {
        Ok((bytes_written, store_attempt)) => ExtractionArtifacts::LlmFailedPersistedFallback {
            error_reason: last_failure
                .map_or(SessionMemoryExtractionErrorReason::LlmError, |f| f.reason),
            error_detail: last_failure.and_then(|f| f.detail.clone()),
            bytes_written,
            store_attempt,
            content: fallback,
            selector_model: last_failure.map(|f| f.model_name.clone()),
            failed_candidates,
        },
        Err(store_error) => ExtractionArtifacts::PersistFailed {
            error_reason: store_error.reason,
            persist_error_detail: store_error.detail,
            llm_error_reason: last_failure.map(|f| f.reason),
            llm_error_detail: last_failure.and_then(|f| f.detail.clone()),
            selector_model: last_failure.map(|f| f.model_name.clone()),
            failed_candidates,
        },
    }
}

fn session_memory_extraction_messages(messages: &[Value]) -> Vec<Value> {
    astra_turn_core::prompt_facing::sanitize_prompt_facing_messages(messages.to_vec())
        .into_iter()
        .filter(|msg| !is_ephemeral_message_for_session_memory(msg))
        .collect()
}

fn is_ephemeral_message_for_session_memory(msg: &Value) -> bool {
    is_runtime_owned_message(msg)
}

pub fn encode_session_memory_entry(session_id: &str, content: &str) -> String {
    SessionMemorySnapshot::from_markdown(session_id, content, 0, SessionFacts::default())
        .to_memory_entry()
        .encode()
}

pub fn decode_session_memory_entry(raw: &str, session_id: &str) -> Option<String> {
    decode_session_memory_snapshot(raw, session_id).map(|snapshot| snapshot.to_markdown())
}

pub fn canonicalize_session_memory_markdown(
    session_id: &str,
    content: &str,
    updated_turn: u32,
    session_facts: &SessionFacts,
) -> String {
    SessionMemorySnapshot::from_markdown(session_id, content, updated_turn, session_facts.clone())
        .to_markdown()
}

pub async fn load_current_session_memory(
    memoria: &dyn MemoriaPort,
    session_id: &str,
) -> Option<String> {
    load_current_session_memory_with_freshness(memoria, session_id)
        .await
        .map(|loaded| loaded.content)
}

pub async fn load_current_session_memory_with_freshness(
    memoria: &dyn MemoriaPort,
    session_id: &str,
) -> Option<LoadedSessionMemory> {
    load_current_session_memory_snapshot(memoria, session_id)
        .await
        .map(|snapshot| LoadedSessionMemory {
            content: snapshot.to_markdown(),
            updated_turn: Some(snapshot.updated_turn),
        })
}

pub(crate) async fn load_current_session_memory_snapshot(
    memoria: &dyn MemoriaPort,
    session_id: &str,
) -> Option<SessionMemorySnapshot> {
    let session_id = session_id.trim();
    if session_id.is_empty() {
        return None;
    }
    let query = format!("{SESSION_MEMORY_PREFIX} {session_id} session memory");

    match memoria
        .retrieve_scoped_typed(
            &query,
            session_id,
            CURRENT_SESSION_MEMORY_RETRIEVE_TOP_K,
            &[SESSION_MEMORY_MEMORIA_TYPE],
        )
        .await
    {
        Ok(memories) => select_latest_session_memory_snapshot(&memories, session_id),
        Err(e) => {
            tracing::warn!(
                session_id = %session_id,
                error = %e,
                "session_memory typed retrieval failed"
            );
            None
        }
    }
}

pub fn load_local_session_memory_artifact(session_id: &str) -> Option<String> {
    let session_id = session_id.trim();
    if session_id.is_empty() {
        return None;
    }
    let path = astra_services::local_session_artifact_store()
        .session_path(session_id, "session-memory.md")
        .ok()?;
    let body = std::fs::read_to_string(path).ok()?;
    let trimmed = body.trim();
    if trimmed.is_empty() {
        return None;
    }
    Some(trimmed.to_string())
}

pub fn load_local_session_memory_metadata(
    session_id: &str,
) -> Option<SessionMemoryArtifactMetadata> {
    let session_id = session_id.trim();
    if session_id.is_empty() {
        return None;
    }
    let path = astra_services::local_session_artifact_store()
        .session_path(session_id, LOCAL_SESSION_MEMORY_METADATA_FILE)
        .ok()?;
    let raw = std::fs::read_to_string(path).ok()?;
    let mut metadata = serde_json::from_str::<SessionMemoryArtifactMetadata>(&raw).ok()?;
    if metadata.session_id.trim().is_empty() {
        metadata.session_id = session_id.to_string();
    }
    // Older or manually edited metadata files may explicitly store `0`,
    // which bypasses serde's defaulting. Normalize it here so the rest of
    // the read path can treat the epoch as a total invariant.
    if metadata.stable_memory_epoch == 0 {
        metadata.stable_memory_epoch = default_stable_memory_epoch();
    }
    Some(metadata)
}

pub async fn load_current_session_memory_preferring_local(
    memoria: &dyn MemoriaPort,
    session_id: &str,
) -> Option<String> {
    load_current_session_memory_preferring_local_with_freshness(memoria, session_id)
        .await
        .map(|loaded| loaded.content)
}

pub async fn load_current_session_memory_preferring_local_with_freshness(
    memoria: &dyn MemoriaPort,
    session_id: &str,
) -> Option<LoadedSessionMemory> {
    let local = load_local_session_memory_artifact(session_id);
    let local_turn = load_local_session_memory_metadata(session_id)
        .and_then(|metadata| metadata.last_extracted_turn);
    let remote = load_current_session_memory_snapshot(memoria, session_id).await;

    match (local, local_turn, remote) {
        (Some(local), Some(local_turn), Some(remote)) if local_turn >= remote.updated_turn => {
            Some(LoadedSessionMemory {
                content: local,
                updated_turn: Some(local_turn),
            })
        }
        (_, _, Some(remote)) => Some(LoadedSessionMemory {
            content: remote.to_markdown(),
            updated_turn: Some(remote.updated_turn),
        }),
        (Some(local), local_turn, None) => Some(LoadedSessionMemory {
            content: local,
            updated_turn: local_turn,
        }),
        (None, _, None) => None,
    }
}

fn select_latest_session_memory_snapshot(
    memories: &[MemoriaMemory],
    session_id: &str,
) -> Option<SessionMemorySnapshot> {
    let mut latest_active: Option<SessionMemorySnapshot> = None;
    let mut latest_key: Option<(u32, &str, &str)> = None;
    for memory in memories {
        if let Some(snapshot) = decode_session_memory_snapshot(&memory.content, session_id) {
            // Multiple pods may legitimately persist the same turn before a
            // distributed claim is available. Use a stable total order so all
            // readers converge regardless of backend response ordering.
            let key = (
                snapshot.updated_turn,
                memory.memory_id.as_str(),
                memory.content.as_str(),
            );
            if latest_key.is_none_or(|best| key > best) {
                latest_key = Some(key);
                latest_active = Some(snapshot);
            }
        }
    }
    latest_active
}

pub fn decode_session_memory_overview(raw: &str, session_id: &str) -> Option<String> {
    decode_session_memory_snapshot(raw, session_id)?;
    let entry = MemoryEntry::parse(raw.trim())?;
    Some(format!("Session state:\n{}", entry.overview_view()))
}

pub fn decode_session_memory_prompt(
    raw: &str,
    session_id: &str,
    facts_override: Option<&SessionFacts>,
    include_overview: bool,
) -> Option<String> {
    let snapshot = decode_session_memory_snapshot(raw, session_id)?;
    let entry = MemoryEntry::parse(raw.trim())?;
    let facts = facts_override.unwrap_or(&snapshot.facts);
    let mut parts = vec![format!(
        "## Session State\nSnapshot provenance: updated through session turn {}.\nLatest state: {}",
        snapshot.updated_turn,
        entry.compact_view(),
    )];
    let facts_text = facts.to_injection();
    if !facts_text.trim().is_empty() {
        parts.push(facts_text.trim().to_string());
    }
    if include_overview {
        let overview = snapshot.overview_without_facts();
        if !overview.is_empty() {
            parts.push(overview);
        }
    }
    Some(parts.join("\n"))
}

fn decode_session_memory_snapshot(raw: &str, session_id: &str) -> Option<SessionMemorySnapshot> {
    let entry = MemoryEntry::parse(raw.trim())?;
    if entry.ns != NS_SESSION || entry.status != ST_ACTIVE {
        return None;
    }
    let detail = entry.detail_layer()?;
    let snapshot: SessionMemorySnapshot = serde_json::from_str(detail).ok()?;
    (snapshot.schema_version == SESSION_MEMORY_SCHEMA_VERSION && snapshot.session_id == session_id)
        .then_some(snapshot)
}

fn extract_scalar_section(content: &str, name: &str) -> String {
    extract_section(content, name)
        .map(|section| single_line(&section))
        .filter(|section| !section.is_empty())
        .unwrap_or_default()
}

fn extract_list_section(content: &str, name: &str) -> Vec<String> {
    extract_section(content, name)
        .map(|section| parse_section_lines(&section))
        .unwrap_or_default()
}

fn parse_section_lines(section: &str) -> Vec<String> {
    let mut out = Vec::new();
    for line in section.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with("<!--") {
            continue;
        }
        let item = trimmed
            .trim_start_matches("- ")
            .trim_start_matches("* ")
            .trim_start_matches(|c: char| c.is_ascii_digit() || c == '.' || c == ')')
            .trim();
        if !item.is_empty() {
            out.push(item.to_string());
        }
    }
    if out.is_empty() {
        let trimmed = section.trim();
        if !trimmed.is_empty() {
            out.push(single_line(trimmed));
        }
    }
    dedup_preserve_order(&mut out);
    out
}

fn seed_active_goal(narrative: &SessionNarrative) -> Option<String> {
    [
        narrative.task_spec.as_str(),
        narrative.session_title.as_str(),
    ]
    .into_iter()
    .map(single_line)
    .find(|candidate| !candidate.is_empty())
}

fn normalize_narrative(narrative: &mut SessionNarrative) {
    narrative.session_title = truncate_chars(&single_line(&narrative.session_title), 200);
    narrative.task_spec = truncate_chars(&single_line(&narrative.task_spec), 1_000);
    narrative
        .active_goals
        .retain(|goal| !goal.trim().is_empty());
    narrative
        .pending_todos
        .retain(|pending| !pending.trim().is_empty());
    narrative
        .current_state
        .retain(|state| !state.trim().is_empty());
    dedup_preserve_order(&mut narrative.active_goals);
    if narrative.active_goals.is_empty()
        && let Some(goal) = seed_active_goal(narrative)
    {
        narrative.active_goals.push(goal);
    }
    dedup_preserve_order(&mut narrative.pending_todos);
    dedup_preserve_order(&mut narrative.corrections);
    dedup_preserve_order(&mut narrative.learnings);
    narrative.current_state.truncate(8);
    narrative.active_goals.truncate(8);
    narrative.pending_todos.truncate(8);
    narrative.corrections.truncate(8);
    narrative.learnings.truncate(8);
}

fn narrative_is_empty(narrative: &SessionNarrative) -> bool {
    narrative.session_title.is_empty()
        && narrative.task_spec.is_empty()
        && narrative.current_state.is_empty()
        && narrative.active_goals.is_empty()
        && narrative.pending_todos.is_empty()
        && narrative.corrections.is_empty()
        && narrative.learnings.is_empty()
}

fn fallback_body_text(content: &str) -> String {
    content
        .trim()
        .strip_prefix("# Session Memory")
        .unwrap_or(content.trim())
        .trim()
        .to_string()
}

fn dedup_preserve_order(items: &mut Vec<String>) {
    let mut seen = std::collections::HashSet::new();
    items.retain(|item| {
        let key = single_line(item).to_lowercase();
        !key.is_empty() && seen.insert(key)
    });
}

fn push_capped_section(out: &mut Vec<String>, label: &str, items: &[String], cap: usize) {
    if items.is_empty() {
        return;
    }
    let rendered = items
        .iter()
        .take(cap)
        .map(|item| single_line(item))
        .collect::<Vec<_>>()
        .join("; ");
    let suffix = if items.len() > cap {
        format!(" (+{} more)", items.len() - cap)
    } else {
        String::new()
    };
    out.push(format!("{label}: {rendered}{suffix}"));
}

fn push_scalar_markdown_section(out: &mut Vec<String>, name: &str, value: &str) {
    let value = value.trim();
    if !value.is_empty() {
        out.push(format!("## {name}\n{value}"));
    }
}

fn push_list_markdown_section(out: &mut Vec<String>, name: &str, items: &[String]) {
    if items.is_empty() {
        return;
    }
    let body = items
        .iter()
        .map(|item| format!("- {}", item.trim()))
        .collect::<Vec<_>>()
        .join("\n");
    out.push(format!("## {name}\n{body}"));
}

fn single_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn first_non_empty<'a>(candidates: impl IntoIterator<Item = Option<&'a str>>) -> Option<&'a str> {
    candidates
        .into_iter()
        .flatten()
        .find(|candidate| !candidate.trim().is_empty())
}

fn truncate_chars(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    text.chars()
        .take(max_chars.saturating_sub(1))
        .collect::<String>()
        + "…"
}

async fn update_memory_with_llm(
    current_memory: &str,
    messages: &[Value],
    params: &LlmConnParams,
    llm_timeout: Duration,
    max_output_tokens: usize,
) -> Result<String, LlmExtractionFailure> {
    let prompt = build_extraction_prompt(current_memory, messages);
    let call = call_llm_nonstream(
        global_llm_client(),
        LlmCall {
            purpose: InferencePurpose::MemoryExtraction,
            messages: &prompt,
            tools: &[],
            route: params.execution_route(),
            max_output_tokens: Some(max_output_tokens),
            temperature: Some(0.0),
            has_fallback: false,
            thinking: &astra_turn_core::thinking_config::ThinkingConfig::Off,
        },
        llm_timeout.saturating_add(Duration::from_secs(1)),
    );
    let parsed = match tokio::time::timeout(llm_timeout, call).await {
        Err(_) => {
            return Err(LlmExtractionFailure {
                reason: SessionMemoryExtractionErrorReason::LlmTimeout,
                detail: None,
            });
        }
        Ok(Err(error)) => {
            return Err(LlmExtractionFailure {
                reason: SessionMemoryExtractionErrorReason::LlmError,
                detail: Some(summarize_llm_detail(&redact_provider_secrets(
                    &error.message,
                ))),
            });
        }
        Ok(Ok(result)) => result,
    };
    let content = parsed.full_text.trim();
    if content.is_empty() {
        return Err(LlmExtractionFailure {
            reason: SessionMemoryExtractionErrorReason::EmptyResponse,
            detail: None,
        });
    }
    let (patch, normalized_scalar_lists) =
        parse_session_narrative_patch(content).map_err(|error| LlmExtractionFailure {
            reason: SessionMemoryExtractionErrorReason::LlmError,
            detail: Some(summarize_llm_detail(&format!(
                "invalid session-memory patch: {error}"
            ))),
        })?;
    if !normalized_scalar_lists.is_empty() {
        tracing::warn!(
            target: "astra_runtime::session_memory",
            selector_model = %params.model_name,
            fields = %normalized_scalar_lists.join(","),
            "normalized scalar session-memory fields to singleton lists"
        );
    }
    if patch.is_empty() {
        return Err(LlmExtractionFailure {
            reason: SessionMemoryExtractionErrorReason::EmptyResponse,
            detail: Some("session-memory patch contained no canonical fields".to_string()),
        });
    }
    let mut snapshot = SessionMemorySnapshot::from_markdown(
        "__llm_update__",
        current_memory,
        0,
        SessionFacts::default(),
    );
    snapshot.narrative.apply_patch(patch);
    Ok(snapshot.to_markdown())
}

fn parse_session_narrative_patch(
    raw: &str,
) -> Result<(SessionNarrativePatch, Vec<&'static str>), serde_json::Error> {
    let trimmed = raw.trim();
    let json = trimmed
        .strip_prefix("```json")
        .or_else(|| trimmed.strip_prefix("```"))
        .and_then(|body| body.strip_suffix("```"))
        .map(str::trim)
        .unwrap_or(trimmed);
    let mut value = serde_json::from_str::<Value>(json)?;
    let mut normalized_scalar_lists = Vec::new();
    if let Some(object) = value.as_object_mut() {
        for field in [
            "current_state",
            "active_goals",
            "pending_todos",
            "corrections",
            "learnings",
        ] {
            if object.get(field).is_some_and(Value::is_string)
                && let Some(scalar) = object.remove(field)
            {
                object.insert(field.to_string(), Value::Array(vec![scalar]));
                normalized_scalar_lists.push(field);
            }
        }
    }
    serde_json::from_value(value).map(|patch| (patch, normalized_scalar_lists))
}

fn summarize_llm_detail(text: &str) -> String {
    truncate_chars(&text.split_whitespace().collect::<Vec<_>>().join(" "), 220)
}

async fn store_session_memory(
    memoria: &Arc<dyn MemoriaPort>,
    session_id: &str,
    turn_number: u32,
    session_facts: &SessionFacts,
    content: &str,
) -> Result<(u64, u32), StoreSessionMemoryFailure> {
    let encoded = SessionMemorySnapshot::from_markdown(
        session_id,
        content,
        turn_number,
        session_facts.clone(),
    )
    .to_memory_entry()
    .encode();
    let mut last_detail = None;

    for attempt in 1..=2 {
        match memoria
            .store(
                &encoded,
                SESSION_MEMORY_MEMORIA_TYPE,
                Some(session_id),
                Some("T3"),
            )
            .await
        {
            Ok(memory_id) => {
                if let Err(reason) =
                    cleanup_prior_session_memory_entries(memoria, session_id, &memory_id, &encoded)
                        .await
                {
                    tracing::warn!(
                        session_id = %session_id,
                        ?reason,
                        "new session-memory snapshot is durable but stale snapshot cleanup was incomplete"
                    );
                }
                return Ok((encoded.len() as u64, attempt));
            }
            Err(error) => {
                last_detail = Some(summarize_llm_detail(&error));
                if attempt == 2 {
                    return Err(StoreSessionMemoryFailure {
                        reason: SessionMemoryExtractionErrorReason::WriteFailed,
                        detail: last_detail,
                    });
                }
            }
        }
    }

    Err(StoreSessionMemoryFailure {
        reason: SessionMemoryExtractionErrorReason::WriteFailed,
        detail: last_detail,
    })
}

pub fn persist_local_session_memory_artifact(
    session_id: &str,
    content: &str,
) -> Result<(), String> {
    let path = astra_services::local_session_artifact_store()
        .session_path(session_id, "session-memory.md")?;
    let Some(parent) = path.parent() else {
        return Err(format!(
            "session-memory path has no parent: {}",
            path.display()
        ));
    };
    std::fs::create_dir_all(parent)
        .map_err(|error| format!("create session-memory dir {}: {error}", parent.display()))?;
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| format!("invalid session-memory file name: {}", path.display()))?;
    let tmp_path = path.with_file_name(format!("{file_name}.tmp"));
    std::fs::write(&tmp_path, content)
        .map_err(|error| format!("write session-memory tmp {}: {error}", tmp_path.display()))?;
    if let Err(error) = std::fs::rename(&tmp_path, &path) {
        let _ = std::fs::remove_file(&tmp_path);
        return Err(format!(
            "rename session-memory tmp {} -> {}: {error}",
            tmp_path.display(),
            path.display()
        ));
    }
    let mut metadata = load_local_session_memory_metadata(session_id).unwrap_or_default();
    metadata.session_id = session_id.to_string();
    metadata.last_local_refresh_at = Some(chrono::Utc::now().to_rfc3339());
    persist_local_session_memory_metadata(session_id, &metadata)?;
    Ok(())
}

pub fn persist_local_session_memory_metadata(
    session_id: &str,
    metadata: &SessionMemoryArtifactMetadata,
) -> Result<(), String> {
    let path = astra_services::local_session_artifact_store()
        .session_path(session_id, LOCAL_SESSION_MEMORY_METADATA_FILE)?;
    let Some(parent) = path.parent() else {
        return Err(format!(
            "session-memory metadata path has no parent: {}",
            path.display()
        ));
    };
    std::fs::create_dir_all(parent).map_err(|error| {
        format!(
            "create session-memory metadata dir {}: {error}",
            parent.display()
        )
    })?;
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| {
            format!(
                "invalid session-memory metadata file name: {}",
                path.display()
            )
        })?;
    let tmp_path = path.with_file_name(format!("{file_name}.tmp"));
    let raw = serde_json::to_string_pretty(metadata).map_err(|error| {
        format!(
            "serialize session-memory metadata {}: {error}",
            path.display()
        )
    })?;
    std::fs::write(&tmp_path, raw).map_err(|error| {
        format!(
            "write session-memory metadata tmp {}: {error}",
            tmp_path.display()
        )
    })?;
    if let Err(error) = std::fs::rename(&tmp_path, &path) {
        let _ = std::fs::remove_file(&tmp_path);
        return Err(format!(
            "rename session-memory metadata tmp {} -> {}: {error}",
            tmp_path.display(),
            path.display()
        ));
    }
    Ok(())
}

async fn cleanup_prior_session_memory_entries(
    memoria: &Arc<dyn MemoriaPort>,
    session_id: &str,
    current_memory_id: &str,
    current_encoded: &str,
) -> Result<(), SessionMemoryExtractionErrorReason> {
    // The new snapshot is already durable. Cleanup is deliberately
    // best-effort: duplicate stale snapshots are safe because readers select
    // the highest `updated_turn`; deleting the only valid snapshot is not.
    // Loop until no stale decodable entries remain, with a cap for backends
    // whose delete endpoint silently does nothing.
    const PAGE_SIZE: usize = 16;
    const MAX_PAGES: usize = 8;
    let current_turn = decode_session_memory_snapshot(current_encoded, session_id)
        .map(|snapshot| snapshot.updated_turn)
        .ok_or(SessionMemoryExtractionErrorReason::WriteFailed)?;
    let mut seen_memory_ids = std::collections::HashSet::new();
    for _ in 0..MAX_PAGES {
        let memories = retrieve_prior_session_memory_page(memoria, session_id, PAGE_SIZE).await?;

        let stale_candidates: Vec<&MemoriaMemory> = memories
            .iter()
            .filter(|memory| {
                decode_session_memory_snapshot(&memory.content, session_id)
                    .is_some_and(|snapshot| snapshot.updated_turn < current_turn)
            })
            .filter(|memory| {
                if !current_memory_id.is_empty() {
                    memory.memory_id != current_memory_id
                } else {
                    memory.content != current_encoded
                }
            })
            .collect();
        if stale_candidates.is_empty() {
            return Ok(());
        }

        let to_delete: Vec<String> = stale_candidates
            .into_iter()
            .filter_map(|memory| {
                let id = memory.memory_id.as_str();
                if id.is_empty() {
                    return None;
                }
                if !seen_memory_ids.insert(id.to_string()) {
                    return None;
                }
                Some(id.to_string())
            })
            .collect();

        if to_delete.is_empty() {
            return Err(SessionMemoryExtractionErrorReason::PurgeFailed);
        }
        for id in &to_delete {
            memoria
                .delete(id)
                .await
                .map_err(|_| SessionMemoryExtractionErrorReason::PurgeFailed)?;
        }
    }
    let remaining = retrieve_prior_session_memory_page(memoria, session_id, PAGE_SIZE).await?;
    if !remaining
        .iter()
        .filter(|memory| {
            decode_session_memory_snapshot(&memory.content, session_id)
                .is_some_and(|snapshot| snapshot.updated_turn < current_turn)
        })
        .any(|memory| {
            if !current_memory_id.is_empty() {
                memory.memory_id != current_memory_id
            } else {
                memory.content != current_encoded
            }
        })
    {
        return Ok(());
    }
    tracing::warn!(
        session_id = %session_id,
        page_size = PAGE_SIZE,
        max_pages = MAX_PAGES,
        unique_candidates = seen_memory_ids.len(),
        "session memory purge hit page cap before exhausting prior entries"
    );
    Err(SessionMemoryExtractionErrorReason::PurgeFailed)
}

async fn retrieve_prior_session_memory_page(
    memoria: &Arc<dyn MemoriaPort>,
    session_id: &str,
    page_size: usize,
) -> Result<Vec<MemoriaMemory>, SessionMemoryExtractionErrorReason> {
    memoria
        .retrieve_scoped_typed(
            &format!("{SESSION_MEMORY_PREFIX} {session_id} session memory"),
            session_id,
            page_size,
            &[SESSION_MEMORY_MEMORIA_TYPE],
        )
        .await
        .map_err(|_| SessionMemoryExtractionErrorReason::PurgeFailed)
}

fn build_rule_fallback_memory(
    current_memory: &str,
    messages: &[Value],
    turn_number: usize,
) -> String {
    let first_user = first_user_message(messages).unwrap_or("Current session");
    let errors = collect_error_lines(messages);
    let mut snapshot = SessionMemorySnapshot::from_markdown(
        "__fallback__",
        current_memory,
        turn_number as u32,
        SessionFacts::default(),
    );
    if snapshot.narrative.session_title.is_empty() {
        snapshot.narrative.session_title = truncate(first_user, 180).to_string();
    }
    if snapshot.narrative.task_spec.is_empty() {
        snapshot.narrative.task_spec = truncate(first_user, 400).to_string();
    }
    // A deterministic fallback has no semantic authority to reinterpret later
    // user text as a goal, correction, acknowledgement, or status change.
    // Preserve the canonical state selected previously; initialize it from the
    // original task only when no canonical state exists yet. A later successful
    // selector can advance it from typed evidence.
    if snapshot.narrative.current_state.is_empty() {
        snapshot.narrative.current_state =
            vec![format!("Active task: {}", truncate(first_user, 300))];
    }
    snapshot.narrative.corrections.extend(errors);
    normalize_narrative(&mut snapshot.narrative);
    snapshot.to_markdown()
}

fn first_user_message(messages: &[Value]) -> Option<&str> {
    messages.iter().find_map(|msg| {
        (msg.get("role").and_then(Value::as_str) == Some("user")
            && !is_ephemeral_message_for_session_memory(msg))
        .then(|| message_text(msg))
        .flatten()
    })
}

fn collect_error_lines(messages: &[Value]) -> Vec<String> {
    messages
        .iter()
        .filter_map(|msg| {
            if is_ephemeral_message_for_session_memory(msg) {
                return None;
            }
            structured_error_text(msg).map(|text| truncate(&text, 200).to_string())
        })
        .collect()
}

fn structured_error_text(msg: &Value) -> Option<String> {
    if msg.get("is_error").and_then(Value::as_bool) == Some(true) {
        return message_text_or_summary(msg).or_else(|| Some("structured error".to_string()));
    }

    if matches!(
        msg.get("status").and_then(Value::as_str),
        Some("error" | "failed")
    ) {
        return message_text_or_summary(msg)
            .or_else(|| msg.get("error").map(compact_json_value))
            .or_else(|| Some("structured error status".to_string()));
    }

    if let Some(error) = msg.get("error") {
        return message_text_or_summary(msg).or_else(|| Some(compact_json_value(error)));
    }

    let blocks = msg.get("content").and_then(Value::as_array)?;
    let mut parts = Vec::new();
    for block in blocks {
        let is_error_block = block.get("is_error").and_then(Value::as_bool) == Some(true)
            || block.get("type").and_then(Value::as_str) == Some("error");
        if !is_error_block {
            continue;
        }
        if let Some(text) = block.get("text").and_then(Value::as_str) {
            parts.push(text.trim().to_string());
        } else if let Some(content) = block.get("content").and_then(Value::as_str) {
            parts.push(content.trim().to_string());
        } else {
            parts.push(compact_json_value(block));
        }
    }
    (!parts.is_empty()).then(|| parts.join("\n"))
}

fn compact_json_value(value: &Value) -> String {
    value
        .as_str()
        .map(str::to_string)
        .unwrap_or_else(|| value.to_string())
}

fn message_text(msg: &Value) -> Option<&str> {
    msg.get("content").and_then(Value::as_str)
}

fn assistant_tool_call_summary(msg: &Value) -> Option<String> {
    let tool_calls = msg.get("tool_calls").and_then(Value::as_array)?;
    let names: Vec<&str> = tool_calls
        .iter()
        .filter_map(|tool_call| {
            tool_call
                .get("function")
                .and_then(|function| function.get("name"))
                .and_then(Value::as_str)
        })
        .collect();
    if names.is_empty() {
        None
    } else {
        Some(format!("[called: {}]", names.join(", ")))
    }
}

fn message_text_or_summary(msg: &Value) -> Option<String> {
    let role = msg.get("role").and_then(Value::as_str)?;
    if let Some(text) = message_text(msg)
        .map(str::trim)
        .filter(|text| !text.is_empty())
    {
        return Some(text.to_string());
    }
    match role {
        "assistant" => assistant_tool_call_summary(msg),
        _ => None,
    }
}

fn truncate(text: &str, max_chars: usize) -> &str {
    if text.chars().count() <= max_chars {
        return text;
    }
    let mut end = 0usize;
    for (count, (idx, ch)) in text.char_indices().enumerate() {
        if count == max_chars {
            break;
        }
        end = idx + ch.len_utf8();
    }
    &text[..end]
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    use crate::turn::cloud::memoria_compact::MemoriaMemory;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[derive(Default)]
    struct CapturingMemoria {
        stored: Mutex<Vec<(String, String, Option<String>)>>,
        retrieve_results: Mutex<Vec<MemoriaMemory>>,
        deleted: Mutex<Vec<String>>,
        operations: Mutex<Vec<String>>,
    }

    #[derive(Default)]
    struct FailingMemoria;

    struct DeleteFailMemoria {
        retrieve_results: Vec<MemoriaMemory>,
    }

    struct OverflowingMemoria {
        next_id: Mutex<usize>,
    }

    #[derive(Default)]
    struct FiniteDeleteMemoria {
        remaining: Mutex<usize>,
        stored: Mutex<Vec<String>>,
    }

    #[derive(Default)]
    struct TopKMemoria {
        retrieve_results: Vec<MemoriaMemory>,
        requested_top_k: Mutex<Vec<usize>>,
    }

    #[async_trait::async_trait]
    impl MemoriaPort for CapturingMemoria {
        async fn retrieve_ext(
            &self,
            _query: &str,
            _session_id: Option<&str>,
            _top_k: usize,
            _filter_session: bool,
        ) -> Result<Vec<MemoriaMemory>, String> {
            Ok(self.retrieve_results.lock().unwrap().clone())
        }

        async fn store(
            &self,
            content: &str,
            memory_type: &str,
            session_id: Option<&str>,
            _trust_tier: Option<&str>,
        ) -> Result<String, String> {
            self.operations.lock().unwrap().push("store".to_string());
            self.stored.lock().unwrap().push((
                content.to_string(),
                memory_type.to_string(),
                session_id.map(str::to_string),
            ));
            Ok("mem-1".to_string())
        }

        async fn purge_working(&self, _session_id: &str) -> Result<u64, String> {
            Ok(0)
        }

        async fn delete(&self, memory_id: &str) -> Result<(), String> {
            self.operations
                .lock()
                .unwrap()
                .push(format!("delete:{memory_id}"));
            self.deleted.lock().unwrap().push(memory_id.to_string());
            Ok(())
        }
    }

    #[async_trait::async_trait]
    impl MemoriaPort for FailingMemoria {
        async fn retrieve_ext(
            &self,
            _query: &str,
            _session_id: Option<&str>,
            _top_k: usize,
            _filter_session: bool,
        ) -> Result<Vec<MemoriaMemory>, String> {
            Ok(Vec::new())
        }

        async fn store(
            &self,
            _content: &str,
            _memory_type: &str,
            _session_id: Option<&str>,
            _trust_tier: Option<&str>,
        ) -> Result<String, String> {
            Err("write failed".to_string())
        }

        async fn purge_working(&self, _session_id: &str) -> Result<u64, String> {
            Ok(0)
        }

        async fn delete(&self, _memory_id: &str) -> Result<(), String> {
            Err("delete failed".to_string())
        }
    }

    #[async_trait::async_trait]
    impl MemoriaPort for DeleteFailMemoria {
        async fn retrieve_ext(
            &self,
            _query: &str,
            _session_id: Option<&str>,
            _top_k: usize,
            _filter_session: bool,
        ) -> Result<Vec<MemoriaMemory>, String> {
            Ok(self.retrieve_results.clone())
        }

        async fn store(
            &self,
            _content: &str,
            _memory_type: &str,
            _session_id: Option<&str>,
            _trust_tier: Option<&str>,
        ) -> Result<String, String> {
            Ok("mem-1".to_string())
        }

        async fn purge_working(&self, _session_id: &str) -> Result<u64, String> {
            Ok(0)
        }

        async fn delete(&self, _memory_id: &str) -> Result<(), String> {
            Err("delete failed".to_string())
        }
    }

    #[async_trait::async_trait]
    impl MemoriaPort for OverflowingMemoria {
        async fn retrieve_ext(
            &self,
            _query: &str,
            _session_id: Option<&str>,
            _top_k: usize,
            _filter_session: bool,
        ) -> Result<Vec<MemoriaMemory>, String> {
            let mut next_id = self.next_id.lock().unwrap();
            let memory = MemoriaMemory {
                memory_id: format!("mem-overflow-{}", *next_id),
                content: encode_session_memory_entry(
                    "sess-overflow",
                    "# Session Memory\n\noverflow",
                ),
                memory_type: SESSION_MEMORY_MEMORIA_TYPE.to_string(),
                session_id: Some("sess-overflow".to_string()),
                ..Default::default()
            };
            *next_id += 1;
            Ok(vec![memory])
        }

        async fn store(
            &self,
            _content: &str,
            _memory_type: &str,
            _session_id: Option<&str>,
            _trust_tier: Option<&str>,
        ) -> Result<String, String> {
            Ok("mem-new".to_string())
        }

        async fn purge_working(&self, _session_id: &str) -> Result<u64, String> {
            Ok(0)
        }

        async fn delete(&self, _memory_id: &str) -> Result<(), String> {
            Ok(())
        }
    }

    #[async_trait::async_trait]
    impl MemoriaPort for FiniteDeleteMemoria {
        async fn retrieve_ext(
            &self,
            _query: &str,
            _session_id: Option<&str>,
            _top_k: usize,
            _filter_session: bool,
        ) -> Result<Vec<MemoriaMemory>, String> {
            let remaining = *self.remaining.lock().unwrap();
            if remaining == 0 {
                return Ok(Vec::new());
            }
            Ok(vec![MemoriaMemory {
                memory_id: format!("mem-bounded-{remaining}"),
                content: encode_session_memory_entry("sess-bounded", "# Session Memory\n\nbounded"),
                memory_type: SESSION_MEMORY_MEMORIA_TYPE.to_string(),
                session_id: Some("sess-bounded".to_string()),
                ..Default::default()
            }])
        }

        async fn store(
            &self,
            content: &str,
            _memory_type: &str,
            _session_id: Option<&str>,
            _trust_tier: Option<&str>,
        ) -> Result<String, String> {
            self.stored.lock().unwrap().push(content.to_string());
            Ok("mem-bounded-new".to_string())
        }

        async fn purge_working(&self, _session_id: &str) -> Result<u64, String> {
            Ok(0)
        }

        async fn delete(&self, _memory_id: &str) -> Result<(), String> {
            let mut remaining = self.remaining.lock().unwrap();
            if *remaining > 0 {
                *remaining -= 1;
            }
            Ok(())
        }
    }

    #[async_trait::async_trait]
    impl MemoriaPort for TopKMemoria {
        async fn retrieve_ext(
            &self,
            _query: &str,
            _session_id: Option<&str>,
            top_k: usize,
            _filter_session: bool,
        ) -> Result<Vec<MemoriaMemory>, String> {
            self.requested_top_k.lock().unwrap().push(top_k);
            Ok(self.retrieve_results.iter().take(top_k).cloned().collect())
        }

        async fn store(
            &self,
            _content: &str,
            _memory_type: &str,
            _session_id: Option<&str>,
            _trust_tier: Option<&str>,
        ) -> Result<String, String> {
            Ok("mem-topk".to_string())
        }

        async fn purge_working(&self, _session_id: &str) -> Result<u64, String> {
            Ok(0)
        }
    }

    async fn spawn_json_server(
        assert_request: Arc<dyn Fn(&str) + Send + Sync>,
        body: Value,
    ) -> (String, tokio::task::JoinHandle<()>) {
        spawn_json_server_with_status(assert_request, 200, "OK", body).await
    }

    async fn spawn_json_server_with_status(
        assert_request: Arc<dyn Fn(&str) + Send + Sync>,
        status_code: u16,
        reason_phrase: &str,
        body: Value,
    ) -> (String, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let body_text = body.to_string();
        let reason_phrase = reason_phrase.to_string();
        let handle = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 32 * 1024];
            let n = socket.read(&mut buf).await.unwrap_or(0);
            let request = String::from_utf8_lossy(&buf[..n]).into_owned();
            assert_request(&request);
            let response = format!(
                "HTTP/1.1 {status_code} {reason_phrase}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                body_text.len(),
                body_text
            );
            let _ = socket.write_all(response.as_bytes()).await;
        });
        (format!("http://{addr}"), handle)
    }

    fn sample_messages() -> Vec<Value> {
        vec![
            json!({"role": "user", "content": "Fix crates/runtime/src/session_memory/runner.rs"}),
            json!({"role": "assistant", "content": "Investigating the session memory runner."}),
        ]
    }

    #[test]
    fn rule_fallback_records_only_live_resumable_state() {
        let content = build_rule_fallback_memory("", &sample_messages(), 3);

        assert!(content.contains("Active task: Fix crates/runtime/src/session_memory/runner.rs"));
        assert!(
            !content.contains("Persisted a deterministic session-memory fallback snapshot"),
            "fallback Completed must not claim an implementation detail as user-visible progress: {content}"
        );
        assert!(
            !content.contains("Keep session memory synchronized when the session grows"),
            "fallback Learnings must not invent generic advice: {content}"
        );
        assert!(!content.contains("## Completed"));
        assert!(!content.contains("## Worklog"));
    }

    #[test]
    fn rule_fallback_preserves_canonical_state_without_classifying_later_text() {
        let messages = vec![
            json!({"role": "user", "content": "Implement durable session memory refresh"}),
            json!({"role": "assistant", "content": "Working on it."}),
            json!({"role": "user", "content": "This later message may be a correction, a new goal, or an acknowledgement."}),
        ];
        let existing = "# Session Memory\n\n## Task Specification\nImplement durable session memory refresh\n\n## Current State\n- Canonical producer state\n";

        let content = build_rule_fallback_memory(existing, &messages, 4);

        assert!(content.contains("Canonical producer state"));
        assert!(!content.contains("This later message may be"));
    }

    #[test]
    fn rule_fallback_records_only_structured_error_evidence() {
        let messages = vec![
            json!({"role": "user", "content": "Keep long-running sessions healthy"}),
            json!({"role": "assistant", "content": "Ordinary prose that happens to describe a failure"}),
            json!({"role": "tool", "is_error": true, "content": "typed failure evidence"}),
        ];

        let content = build_rule_fallback_memory("", &messages, 6);

        assert!(content.contains("Keep long-running sessions healthy"));
        assert!(content.contains("typed failure evidence"));
        assert!(!content.contains("Ordinary prose that happens to describe a failure"));
    }

    #[tokio::test]
    async fn run_extraction_without_selector_persists_rule_fallback() {
        let memoria = Arc::new(CapturingMemoria::default()) as Arc<dyn MemoriaPort>;
        let artifacts = run_extraction(
            &memoria,
            "sess-1",
            &sample_messages(),
            3,
            "",
            &SessionFacts::default(),
            &[],
            Duration::from_secs(3),
            256,
        )
        .await;

        match artifacts {
            ExtractionArtifacts::Persisted {
                source, content, ..
            } => {
                assert_eq!(source, SessionMemoryExtractionSource::RuleFallback);
                assert!(content.contains("# Session Memory"));
            }
            _ => panic!("expected fallback persistence"),
        }
    }

    #[tokio::test]
    async fn run_extraction_openai_selector_persists_llm_content() {
        let (server_url, server_handle) = spawn_json_server(
            Arc::new(|request: &str| {
                assert!(request.starts_with("POST /v1/chat/completions HTTP/1.1"));
                assert!(request.contains("authorization: Bearer test-key"));
            }),
            json!({
                "choices": [{
                    "message": {
                        "content": "{\"session_title\":\"LLM Result\"}"
                    }
                }]
            }),
        )
        .await;

        let memoria = Arc::new(CapturingMemoria::default()) as Arc<dyn MemoriaPort>;
        let params = LlmConnParams {
            base_url: format!("{server_url}/v1"),
            api_key: "test-key".to_string(),
            model_name: "selector-openai".to_string(),
            wire_model_name: None,
            provider: "openai".to_string(),
            request_body_overrides: None,
            header_overrides: std::collections::HashMap::new(),
            completions_url_override: None,
            request_timeout: None,
        };
        let artifacts = run_extraction(
            &memoria,
            "sess-openai",
            &sample_messages(),
            1,
            "",
            &SessionFacts::default(),
            std::slice::from_ref(&params),
            Duration::from_secs(3),
            512,
        )
        .await;

        match artifacts {
            ExtractionArtifacts::Persisted {
                source, content, ..
            } => {
                assert_eq!(source, SessionMemoryExtractionSource::Llm);
                assert!(content.contains("LLM Result"));
            }
            _ => panic!("expected llm persistence"),
        }
        server_handle.await.unwrap();
    }

    #[tokio::test]
    async fn run_extraction_normalizes_realistic_scalar_list_response_without_fallback() {
        let (server_url, server_handle) = spawn_json_server(
            Arc::new(|request: &str| {
                assert!(request.starts_with("POST /v1/chat/completions HTTP/1.1"));
            }),
            json!({
                "choices": [{
                    "message": {
                        "content": "{\"current_state\":\"review complete; awaiting verification\"}"
                    }
                }]
            }),
        )
        .await;

        let memoria = Arc::new(CapturingMemoria::default()) as Arc<dyn MemoriaPort>;
        let params = LlmConnParams {
            base_url: format!("{server_url}/v1"),
            api_key: "test-key".to_string(),
            model_name: "selector-openai".to_string(),
            wire_model_name: None,
            provider: "openai".to_string(),
            request_body_overrides: None,
            header_overrides: std::collections::HashMap::new(),
            completions_url_override: None,
            request_timeout: None,
        };
        let artifacts = run_extraction(
            &memoria,
            "sess-scalar-selector",
            &sample_messages(),
            1,
            "",
            &SessionFacts::default(),
            std::slice::from_ref(&params),
            Duration::from_secs(3),
            512,
        )
        .await;

        match artifacts {
            ExtractionArtifacts::Persisted {
                source, content, ..
            } => {
                assert_eq!(source, SessionMemoryExtractionSource::Llm);
                assert!(content.contains("review complete; awaiting verification"));
            }
            _ => panic!("expected normalized LLM persistence without rule fallback"),
        }
        server_handle.await.unwrap();
    }

    #[tokio::test]
    async fn run_extraction_selector_prompt_keeps_user_corrections_as_evidence() {
        let (server_url, server_handle) = spawn_json_server(
            Arc::new(|request: &str| {
                assert!(
                    request.contains("durable fix, not a workaround"),
                    "user correction must remain visible to the selector: {request}"
                );
                assert!(
                    request.contains("never use mocks in integration tests"),
                    "concrete directive should remain visible to selector prompt: {request}"
                );
            }),
            json!({
                "choices": [{
                    "message": {
                        "content": "{\"session_title\":\"Filtered LLM Result\"}"
                    }
                }]
            }),
        )
        .await;

        let messages = vec![
            json!({"role": "user", "content": "Improve long running session memory"}),
            json!({"role": "assistant", "content": "I will patch the immediate case."}),
            json!({"role": "user", "content": "What I asked for is a durable fix, not a workaround"}),
            json!({"role": "user", "content": "wrong, never use mocks in integration tests"}),
        ];
        let memoria = Arc::new(CapturingMemoria::default()) as Arc<dyn MemoriaPort>;
        let params = LlmConnParams {
            base_url: format!("{server_url}/v1"),
            api_key: "test-key".to_string(),
            model_name: "selector-openai".to_string(),
            wire_model_name: None,
            provider: "openai".to_string(),
            request_body_overrides: None,
            header_overrides: std::collections::HashMap::new(),
            completions_url_override: None,
            request_timeout: None,
        };

        let artifacts = run_extraction(
            &memoria,
            "sess-filtered-selector",
            &messages,
            4,
            "",
            &SessionFacts::default(),
            std::slice::from_ref(&params),
            Duration::from_secs(3),
            512,
        )
        .await;

        match artifacts {
            ExtractionArtifacts::Persisted {
                source, content, ..
            } => {
                assert_eq!(source, SessionMemoryExtractionSource::Llm);
                assert!(content.contains("Filtered LLM Result"));
            }
            _ => panic!("expected llm persistence"),
        }
        server_handle.await.unwrap();
    }

    #[tokio::test]
    async fn run_extraction_selector_prompt_excludes_typed_runtime_messages() {
        let (server_url, server_handle) = spawn_json_server(
            Arc::new(|request: &str| {
                assert!(
                    !request.contains("owned-selector-only"),
                    "runtime-owned content must not reach selector prompt: {request}"
                );
                assert!(
                    request.contains("Improve long-running memory hygiene"),
                    "real user task should remain visible to selector prompt: {request}"
                );
            }),
            json!({
                "choices": [{
                    "message": {
                        "content": "{\"session_title\":\"Scaffolding Filtered\"}"
                    }
                }]
            }),
        )
        .await;

        let messages = vec![
            json!({"role": "user", "content": "Improve long-running memory hygiene"}),
            astra_turn_types::runtime_owned_message(
                "assistant",
                "owned-selector-only one",
                astra_turn_types::RuntimeMessageDelivery::EphemeralControl,
            ),
            astra_turn_types::runtime_owned_message(
                "user",
                "owned-selector-only two",
                astra_turn_types::RuntimeMessageDelivery::RequiredContext,
            ),
        ];
        let memoria = Arc::new(CapturingMemoria::default()) as Arc<dyn MemoriaPort>;
        let params = LlmConnParams {
            base_url: format!("{server_url}/v1"),
            api_key: "test-key".to_string(),
            model_name: "selector-openai".to_string(),
            wire_model_name: None,
            provider: "openai".to_string(),
            request_body_overrides: None,
            header_overrides: std::collections::HashMap::new(),
            completions_url_override: None,
            request_timeout: None,
        };

        let artifacts = run_extraction(
            &memoria,
            "sess-filtered-scaffolding",
            &messages,
            4,
            "",
            &SessionFacts::default(),
            std::slice::from_ref(&params),
            Duration::from_secs(3),
            512,
        )
        .await;

        match artifacts {
            ExtractionArtifacts::Persisted {
                source, content, ..
            } => {
                assert_eq!(source, SessionMemoryExtractionSource::Llm);
                assert!(content.contains("Scaffolding Filtered"));
            }
            _ => panic!("expected llm persistence"),
        }
        server_handle.await.unwrap();
    }

    #[tokio::test]
    async fn run_extraction_selector_prompt_preserves_unowned_conversation_text() {
        let (server_url, server_handle) = spawn_json_server(
            Arc::new(|request: &str| {
                assert!(
                    request.contains("Read failed before returning output"),
                    "unowned assistant text must remain available to the selector: {request}"
                );
                assert!(
                    request.contains("web_search timed out"),
                    "runtime must not classify prose by keywords: {request}"
                );
                assert!(
                    request.contains("Sensitive path requires explicit opt-in"),
                    "runtime must not classify prose by keywords: {request}"
                );
                assert!(
                    request.contains("Improve session cleanup nudges"),
                    "real task should remain visible to selector prompt: {request}"
                );
            }),
            json!({
                "choices": [{
                    "message": {
                        "content": "{\"session_title\":\"Unowned Preserved\"}"
                    }
                }]
            }),
        )
        .await;

        let messages = vec![
            json!({"role": "user", "content": "Improve session cleanup nudges"}),
            json!({"role": "assistant", "content": "Read failed before returning output: Reading: .../tool-results/call.txt"}),
            json!({"role": "assistant", "content": "Tool web_search timed out while waiting for the server"}),
            json!({"role": "assistant", "content": "Sensitive path requires explicit opt-in in Auto mode"}),
        ];
        let memoria = Arc::new(CapturingMemoria::default()) as Arc<dyn MemoriaPort>;
        let params = LlmConnParams {
            base_url: format!("{server_url}/v1"),
            api_key: "test-key".to_string(),
            model_name: "selector-openai".to_string(),
            wire_model_name: None,
            provider: "openai".to_string(),
            request_body_overrides: None,
            header_overrides: std::collections::HashMap::new(),
            completions_url_override: None,
            request_timeout: None,
        };

        let artifacts = run_extraction(
            &memoria,
            "sess-filtered-transient-status",
            &messages,
            4,
            "",
            &SessionFacts::default(),
            std::slice::from_ref(&params),
            Duration::from_secs(3),
            512,
        )
        .await;

        match artifacts {
            ExtractionArtifacts::Persisted {
                source, content, ..
            } => {
                assert_eq!(source, SessionMemoryExtractionSource::Llm);
                assert!(content.contains("Unowned Preserved"));
            }
            _ => panic!("expected llm persistence"),
        }
        server_handle.await.unwrap();
    }

    #[tokio::test]
    async fn run_extraction_anthropic_selector_uses_native_endpoint() {
        let (server_url, server_handle) = spawn_json_server(
            Arc::new(|request: &str| {
                assert!(request.starts_with("POST /v1/messages HTTP/1.1"));
                assert!(request.contains("x-api-key: anthropic-key"));
                assert!(request.contains("anthropic-version: 2023-06-01"));
                assert!(request.contains("\"model\":\"deepseek-v4-flash\""));
            }),
            json!({
                "content": [
                    { "type": "text", "text": "{\"session_title\":\"Anthropic Result\"}" }
                ]
            }),
        )
        .await;

        let memoria = Arc::new(CapturingMemoria::default()) as Arc<dyn MemoriaPort>;
        let params = LlmConnParams {
            base_url: server_url,
            api_key: "anthropic-key".to_string(),
            model_name: "deepseek-v4-flash-anthropic".to_string(),
            wire_model_name: Some("deepseek-v4-flash".to_string()),
            provider: "anthropic".to_string(),
            request_body_overrides: None,
            header_overrides: std::collections::HashMap::new(),
            completions_url_override: None,
            request_timeout: None,
        };
        let artifacts = run_extraction(
            &memoria,
            "sess-anthropic",
            &sample_messages(),
            1,
            "",
            &SessionFacts::default(),
            std::slice::from_ref(&params),
            Duration::from_secs(3),
            512,
        )
        .await;

        match artifacts {
            ExtractionArtifacts::Persisted { content, .. } => {
                assert!(content.contains("Anthropic Result"));
            }
            _ => panic!("expected anthropic llm persistence"),
        }
        server_handle.await.unwrap();
    }

    #[tokio::test]
    async fn run_extraction_bedrock_selector_uses_converse_endpoint() {
        let (server_url, server_handle) = spawn_json_server(
            Arc::new(|request: &str| {
                assert!(request.starts_with("POST /model/anthropic.claude/converse HTTP/1.1"));
                assert!(request.contains("authorization: Bearer bedrock-key"));
            }),
            json!({
                "output": {
                    "message": {
                        "content": [
                            { "text": "{\"session_title\":\"Bedrock Result\"}" }
                        ]
                    }
                }
            }),
        )
        .await;

        let memoria = Arc::new(CapturingMemoria::default()) as Arc<dyn MemoriaPort>;
        let params = LlmConnParams {
            base_url: server_url,
            api_key: "bedrock-key".to_string(),
            model_name: "anthropic.claude".to_string(),
            wire_model_name: None,
            provider: "bedrock".to_string(),
            request_body_overrides: None,
            header_overrides: std::collections::HashMap::new(),
            completions_url_override: None,
            request_timeout: None,
        };
        let artifacts = run_extraction(
            &memoria,
            "sess-bedrock",
            &sample_messages(),
            1,
            "",
            &SessionFacts::default(),
            std::slice::from_ref(&params),
            Duration::from_secs(3),
            512,
        )
        .await;

        match artifacts {
            ExtractionArtifacts::Persisted { content, .. } => {
                assert!(content.contains("Bedrock Result"));
            }
            _ => panic!("expected bedrock llm persistence"),
        }
        server_handle.await.unwrap();
    }

    #[tokio::test]
    async fn run_extraction_preserves_llm_reason_when_fallback_store_also_fails() {
        let (server_url, server_handle) = spawn_json_server(
            Arc::new(|request: &str| {
                assert!(request.starts_with("POST /v1/chat/completions HTTP/1.1"));
            }),
            json!({
                "choices": [{
                    "message": {
                        "content": ""
                    }
                }]
            }),
        )
        .await;

        let memoria = Arc::new(FailingMemoria) as Arc<dyn MemoriaPort>;
        let params = LlmConnParams {
            base_url: format!("{server_url}/v1"),
            api_key: "test-key".to_string(),
            model_name: "selector-openai".to_string(),
            wire_model_name: None,
            provider: "openai".to_string(),
            request_body_overrides: None,
            header_overrides: std::collections::HashMap::new(),
            completions_url_override: None,
            request_timeout: None,
        };
        let artifacts = run_extraction(
            &memoria,
            "sess-double-fail",
            &sample_messages(),
            1,
            "",
            &SessionFacts::default(),
            std::slice::from_ref(&params),
            Duration::from_secs(3),
            512,
        )
        .await;

        match artifacts {
            ExtractionArtifacts::PersistFailed {
                error_reason,
                llm_error_reason,
                llm_error_detail,
                ..
            } => {
                assert_eq!(
                    error_reason,
                    SessionMemoryExtractionErrorReason::WriteFailed
                );
                assert_eq!(
                    llm_error_reason,
                    Some(SessionMemoryExtractionErrorReason::EmptyResponse)
                );
                assert_eq!(llm_error_detail, None);
            }
            _ => panic!("expected double-failure persist error"),
        }
        server_handle.await.unwrap();
    }

    #[tokio::test]
    async fn run_extraction_captures_http_error_detail_on_fallback() {
        let (server_url, server_handle) = spawn_json_server_with_status(
            Arc::new(|request: &str| {
                assert!(request.starts_with("POST /v1/chat/completions HTTP/1.1"));
            }),
            502,
            "Bad Gateway",
            json!({
                "error": {
                    "message": "upstream model gateway timed out"
                }
            }),
        )
        .await;

        let memoria = Arc::new(CapturingMemoria::default()) as Arc<dyn MemoriaPort>;
        let params = LlmConnParams {
            base_url: format!("{server_url}/v1"),
            api_key: "test-key".to_string(),
            model_name: "selector-openai".to_string(),
            wire_model_name: None,
            provider: "openai".to_string(),
            request_body_overrides: None,
            header_overrides: std::collections::HashMap::new(),
            completions_url_override: None,
            request_timeout: None,
        };
        let artifacts = run_extraction(
            &memoria,
            "sess-http-detail",
            &sample_messages(),
            1,
            "",
            &SessionFacts::default(),
            std::slice::from_ref(&params),
            Duration::from_secs(3),
            512,
        )
        .await;

        match artifacts {
            ExtractionArtifacts::LlmFailedPersistedFallback {
                error_reason,
                error_detail,
                ..
            } => {
                assert_eq!(error_reason, SessionMemoryExtractionErrorReason::LlmError);
                assert_eq!(
                    error_detail,
                    Some("http 502: upstream model gateway timed out".to_string())
                );
            }
            _ => panic!("expected llm-failed fallback persistence"),
        }
        server_handle.await.unwrap();
    }

    #[tokio::test]
    async fn run_extraction_retries_later_selector_candidate_after_llm_failure() {
        let (failing_url, failing_handle) = spawn_json_server_with_status(
            Arc::new(|request: &str| {
                assert!(request.starts_with("POST /v1/chat/completions HTTP/1.1"));
            }),
            502,
            "Bad Gateway",
            json!({
                "error": {
                    "message": "selector one timed out"
                }
            }),
        )
        .await;
        let (success_url, success_handle) = spawn_json_server(
            Arc::new(|request: &str| {
                assert!(request.starts_with("POST /v1/chat/completions HTTP/1.1"));
            }),
            json!({
                "choices": [{
                    "message": {
                        "content": "{\"session_title\":\"Recovered on candidate two\"}"
                    }
                }]
            }),
        )
        .await;

        let memoria = Arc::new(CapturingMemoria::default()) as Arc<dyn MemoriaPort>;
        let first = LlmConnParams {
            base_url: format!("{failing_url}/v1"),
            api_key: "test-key".to_string(),
            model_name: "selector-openai-1".to_string(),
            wire_model_name: None,
            provider: "openai".to_string(),
            request_body_overrides: None,
            header_overrides: std::collections::HashMap::new(),
            completions_url_override: None,
            request_timeout: None,
        };
        let second = LlmConnParams {
            base_url: format!("{success_url}/v1"),
            api_key: "test-key".to_string(),
            model_name: "selector-openai-2".to_string(),
            wire_model_name: None,
            provider: "openai".to_string(),
            request_body_overrides: None,
            header_overrides: std::collections::HashMap::new(),
            completions_url_override: None,
            request_timeout: None,
        };
        let artifacts = run_extraction(
            &memoria,
            "sess-retry",
            &sample_messages(),
            1,
            "",
            &SessionFacts::default(),
            &[first.clone(), second.clone()],
            Duration::from_secs(3),
            512,
        )
        .await;

        match artifacts {
            ExtractionArtifacts::Persisted {
                source,
                content,
                selector_model,
                failed_candidates,
                ..
            } => {
                assert_eq!(source, SessionMemoryExtractionSource::Llm);
                assert_eq!(selector_model.as_deref(), Some(second.model_name.as_str()));
                assert!(content.contains("Recovered on candidate two"));
                assert_eq!(failed_candidates.len(), 1);
                assert_eq!(failed_candidates[0].model_name, first.model_name);
                assert_eq!(
                    failed_candidates[0].reason,
                    SessionMemoryExtractionErrorReason::LlmError
                );
            }
            _ => panic!("expected second selector candidate to recover"),
        }
        failing_handle.await.unwrap();
        success_handle.await.unwrap();
    }

    #[tokio::test]
    async fn run_extraction_stores_new_snapshot_before_cleaning_prior_entries() {
        let memoria = Arc::new(CapturingMemoria::default());
        memoria.retrieve_results.lock().unwrap().extend([
            MemoriaMemory {
                memory_id: "mem-old-session".to_string(),
                content: encode_session_memory_entry("sess-1", "# Session Memory\n\nold"),
                memory_type: SESSION_MEMORY_MEMORIA_TYPE.to_string(),
                session_id: Some("sess-1".to_string()),
                ..Default::default()
            },
            MemoriaMemory {
                memory_id: "mem-working".to_string(),
                content: "User: keep general working memory".to_string(),
                memory_type: "working".to_string(),
                session_id: Some("sess-1".to_string()),
                ..Default::default()
            },
        ]);
        let memoria_dyn = Arc::clone(&memoria) as Arc<dyn MemoriaPort>;

        let artifacts = run_extraction(
            &memoria_dyn,
            "sess-1",
            &sample_messages(),
            3,
            "",
            &SessionFacts::default(),
            &[],
            Duration::from_secs(3),
            256,
        )
        .await;

        match artifacts {
            ExtractionArtifacts::Persisted { .. } => {}
            _ => panic!("expected persisted extraction"),
        }
        assert_eq!(
            memoria.deleted.lock().unwrap().as_slice(),
            ["mem-old-session"],
            "only prior session-memory entries should be deleted"
        );
        assert_eq!(
            memoria.stored.lock().unwrap().len(),
            1,
            "new session memory should still be stored after purge"
        );
        assert_eq!(
            memoria.stored.lock().unwrap()[0].1,
            SESSION_MEMORY_MEMORIA_TYPE,
            "authoritative session memory should use the supported working memoria type"
        );
        assert_eq!(
            memoria.operations.lock().unwrap().as_slice(),
            ["store", "delete:mem-old-session"],
            "a cleanup failure must never erase the last valid snapshot before replacement"
        );
    }

    #[tokio::test]
    async fn cleanup_never_deletes_competing_same_or_newer_turn_snapshots() {
        let encode = |turn, state: &str| {
            SessionMemorySnapshot::from_markdown(
                "sess-race",
                &format!("# Session Memory\n\n## Current State\n- {state}\n"),
                turn,
                SessionFacts::default(),
            )
            .to_memory_entry()
            .encode()
        };
        let current = encode(3, "writer-a");
        let memoria = Arc::new(CapturingMemoria::default());
        memoria.retrieve_results.lock().unwrap().extend([
            MemoriaMemory {
                memory_id: "writer-b-turn-3".to_string(),
                content: encode(3, "writer-b"),
                memory_type: SESSION_MEMORY_MEMORIA_TYPE.to_string(),
                session_id: Some("sess-race".to_string()),
                ..Default::default()
            },
            MemoriaMemory {
                memory_id: "writer-newer-turn-4".to_string(),
                content: encode(4, "writer-newer"),
                memory_type: SESSION_MEMORY_MEMORIA_TYPE.to_string(),
                session_id: Some("sess-race".to_string()),
                ..Default::default()
            },
            MemoriaMemory {
                memory_id: "stale-turn-2".to_string(),
                content: encode(2, "stale"),
                memory_type: SESSION_MEMORY_MEMORIA_TYPE.to_string(),
                session_id: Some("sess-race".to_string()),
                ..Default::default()
            },
        ]);
        let memoria_dyn = Arc::clone(&memoria) as Arc<dyn MemoriaPort>;

        let _ = cleanup_prior_session_memory_entries(
            &memoria_dyn,
            "sess-race",
            "writer-a-turn-3",
            &current,
        )
        .await;

        assert_eq!(
            memoria.deleted.lock().unwrap().as_slice(),
            ["stale-turn-2"],
            "concurrent writers may only collect versions older than their own"
        );
    }

    #[test]
    fn equal_turn_snapshot_selection_is_independent_of_backend_order() {
        let memory = |memory_id: &str, state: &str| MemoriaMemory {
            memory_id: memory_id.to_string(),
            content: SessionMemorySnapshot::from_markdown(
                "sess-tie",
                &format!("# Session Memory\n\n## Current State\n- {state}\n"),
                5,
                SessionFacts::default(),
            )
            .to_memory_entry()
            .encode(),
            memory_type: SESSION_MEMORY_MEMORIA_TYPE.to_string(),
            session_id: Some("sess-tie".to_string()),
            ..Default::default()
        };
        let lower = memory("memory-a", "writer-a");
        let higher = memory("memory-b", "writer-b");

        let forward =
            select_latest_session_memory_snapshot(&[lower.clone(), higher.clone()], "sess-tie")
                .unwrap();
        let reverse = select_latest_session_memory_snapshot(&[higher, lower], "sess-tie").unwrap();

        assert_eq!(forward, reverse);
        assert_eq!(forward.updated_turn, 5);
        assert_eq!(forward.narrative.current_state, vec!["writer-b"]);
    }

    #[tokio::test]
    async fn run_extraction_keeps_success_when_stale_snapshot_cleanup_fails() {
        let memoria = Arc::new(DeleteFailMemoria {
            retrieve_results: vec![MemoriaMemory {
                memory_id: "mem-old-session".to_string(),
                content: encode_session_memory_entry("sess-1", "# Session Memory\n\nold"),
                memory_type: SESSION_MEMORY_MEMORIA_TYPE.to_string(),
                session_id: Some("sess-1".to_string()),
                ..Default::default()
            }],
        }) as Arc<dyn MemoriaPort>;
        let artifacts = run_extraction(
            &memoria,
            "sess-1",
            &sample_messages(),
            3,
            "",
            &SessionFacts::default(),
            &[],
            Duration::from_secs(3),
            256,
        )
        .await;

        assert!(
            matches!(artifacts, ExtractionArtifacts::Persisted { .. }),
            "the new durable snapshot remains successful even if stale cleanup fails"
        );
    }

    #[tokio::test]
    async fn run_extraction_keeps_success_when_stale_cleanup_hits_page_cap() {
        let memoria = Arc::new(OverflowingMemoria {
            next_id: Mutex::new(0),
        }) as Arc<dyn MemoriaPort>;
        let artifacts = run_extraction(
            &memoria,
            "sess-overflow",
            &sample_messages(),
            3,
            "",
            &SessionFacts::default(),
            &[],
            Duration::from_secs(3),
            256,
        )
        .await;

        assert!(
            matches!(artifacts, ExtractionArtifacts::Persisted { .. }),
            "page-capped cleanup must not roll back the new snapshot"
        );
    }

    #[tokio::test]
    async fn run_extraction_persists_when_final_probe_after_page_cap_is_empty() {
        let memoria = Arc::new(FiniteDeleteMemoria {
            remaining: Mutex::new(8),
            stored: Mutex::new(Vec::new()),
        });
        let memoria_dyn = Arc::clone(&memoria) as Arc<dyn MemoriaPort>;
        let artifacts = run_extraction(
            &memoria_dyn,
            "sess-bounded",
            &sample_messages(),
            3,
            "",
            &SessionFacts::default(),
            &[],
            Duration::from_secs(3),
            256,
        )
        .await;

        match artifacts {
            ExtractionArtifacts::Persisted { .. } => {}
            _ => panic!("expected persisted extraction"),
        }
        assert_eq!(
            *memoria.remaining.lock().unwrap(),
            0,
            "all prior entries should eventually be cleaned after store"
        );
        assert_eq!(
            memoria.stored.lock().unwrap().len(),
            1,
            "new session memory should still be stored after the final empty probe"
        );
    }

    #[test]
    fn session_memory_entry_roundtrips() {
        let encoded = encode_session_memory_entry("sess-42", "# Session Memory\n\nhello");
        let decoded = decode_session_memory_entry(&encoded, "sess-42").unwrap();
        assert!(encoded.starts_with(SESSION_MEMORY_PREFIX));
        assert!(decoded.contains("## Current State"));
        assert!(decoded.contains("- hello"));
        assert!(decode_session_memory_entry(&encoded, "other").is_none());
    }

    #[test]
    fn sparse_narrative_patch_preserves_omitted_fields_and_clears_explicit_empty_lists() {
        let mut snapshot = SessionMemorySnapshot::from_markdown(
            "sess-patch",
            "# Session Memory\n\n## Task Specification\nPreserve typed runtime context\n\n## Current State\n- old state\n\n## Pending Todos\n- stale todo\n",
            3,
            SessionFacts::default(),
        );
        let patch = parse_session_narrative_patch(
            r#"{"current_state":["typed lane is wired"],"pending_todos":[]}"#,
        )
        .expect("valid sparse patch")
        .0;

        snapshot.narrative.apply_patch(patch);
        let canonical = snapshot.to_markdown();

        assert!(canonical.contains("Preserve typed runtime context"));
        assert!(canonical.contains("typed lane is wired"));
        assert!(!canonical.contains("stale todo"));
        assert!(!canonical.contains("## Pending Todos"));
    }

    #[test]
    fn narrative_patch_rejects_unknown_fields_and_distinguishes_empty_patch() {
        assert!(parse_session_narrative_patch(r#"{"workflow":["legacy"]}"#).is_err());
        assert!(
            parse_session_narrative_patch("{}")
                .expect("empty JSON object is structurally valid")
                .0
                .is_empty()
        );
    }

    #[test]
    fn narrative_patch_normalizes_scalar_list_fields_but_keeps_strict_types() {
        let (patch, normalized) = parse_session_narrative_patch(
            r#"{"current_state":"review complete","pending_todos":"verify online"}"#,
        )
        .expect("scalar strings are losslessly normalizable");

        assert_eq!(
            patch.current_state,
            Some(vec!["review complete".to_string()])
        );
        assert_eq!(patch.pending_todos, Some(vec!["verify online".to_string()]));
        assert_eq!(normalized, vec!["current_state", "pending_todos"]);
        assert!(parse_session_narrative_patch(r#"{"current_state":42}"#).is_err());
        assert!(parse_session_narrative_patch(r#"{"current_state":["valid",42]}"#).is_err());
    }

    #[test]
    fn session_memory_error_extraction_ignores_plain_text_keywords() {
        let messages = vec![
            json!({"role": "user", "content": "The fail-safe design is working now."}),
            json!({"role": "assistant", "content": "No panic remains; the error handling path is documented."}),
        ];

        assert!(collect_error_lines(&messages).is_empty());
    }

    #[test]
    fn session_memory_error_extraction_uses_structured_error_signals() {
        let messages = vec![
            json!({"role": "tool", "content": "command failed with exit code 1", "is_error": true}),
            json!({"role": "assistant", "status": "failed", "error": "timeout waiting for edge executor"}),
            json!({"role": "user", "content": [{"type": "error", "text": "malformed stream event cursor"}]}),
        ];

        let errors = collect_error_lines(&messages);
        assert_eq!(errors.len(), 3);
        assert!(errors.iter().any(|line| line.contains("exit code 1")));
        assert!(errors.iter().any(|line| line.contains("timeout")));
        assert!(errors.iter().any(|line| line.contains("malformed stream")));
    }

    #[tokio::test]
    async fn load_current_session_memory_fails_closed_on_mixed_session_response() {
        let memoria = CapturingMemoria {
            retrieve_results: Mutex::new(vec![
                MemoriaMemory {
                    content: encode_session_memory_entry("other", "# Session Memory\n\nignore me"),
                    memory_type: SESSION_MEMORY_MEMORIA_TYPE.to_string(),
                    session_id: Some("other".to_string()),
                    ..Default::default()
                },
                MemoriaMemory {
                    content: encode_session_memory_entry("sess-42", "# Session Memory\n\nhello"),
                    memory_type: SESSION_MEMORY_MEMORIA_TYPE.to_string(),
                    session_id: Some("sess-42".to_string()),
                    ..Default::default()
                },
            ]),
            ..Default::default()
        };

        assert!(
            load_current_session_memory(&memoria, "sess-42")
                .await
                .is_none(),
            "a strict response containing any foreign session must be rejected as a whole"
        );
        memoria.retrieve_results.lock().unwrap().remove(0);
        let loaded = load_current_session_memory(&memoria, "sess-42")
            .await
            .expect("matching session memory should load");
        assert!(loaded.contains("## Current State"));
        assert!(loaded.contains("- hello"));
        assert!(
            load_current_session_memory(&memoria, "missing")
                .await
                .is_none()
        );
        assert!(load_current_session_memory(&memoria, "   ").await.is_none());
    }

    #[tokio::test]
    async fn load_current_session_memory_requires_typed_snapshot_schema() {
        let memoria = CapturingMemoria {
            retrieve_results: Mutex::new(vec![
                MemoriaMemory {
                    content: encode_session_memory_entry(
                        "sess-42",
                        "# Session Memory\n\nwrong type",
                    ),
                    memory_type: "reference".to_string(),
                    session_id: Some("sess-42".to_string()),
                    ..Default::default()
                },
                MemoriaMemory {
                    content:
                        "[@session/memory]\nsession_id=sess-42\n# Session Memory\n\nlegacy body"
                            .to_string(),
                    memory_type: SESSION_MEMORY_MEMORIA_TYPE.to_string(),
                    session_id: Some("sess-42".to_string()),
                    ..Default::default()
                },
            ]),
            ..Default::default()
        };

        assert!(
            load_current_session_memory(&memoria, "sess-42")
                .await
                .is_none()
        );
    }

    #[tokio::test]
    async fn load_current_session_memory_searches_beyond_small_noise_window() {
        let mut retrieve_results = (0..8)
            .map(|idx| MemoriaMemory {
                content: format!("[session:sess-42] Recent conversation chunk {idx}"),
                memory_type: "working".to_string(),
                session_id: Some("sess-42".to_string()),
                ..Default::default()
            })
            .collect::<Vec<_>>();
        retrieve_results.push(MemoriaMemory {
            content: encode_session_memory_entry("sess-42", "# Session Memory\n\nneedle"),
            memory_type: SESSION_MEMORY_MEMORIA_TYPE.to_string(),
            session_id: Some("sess-42".to_string()),
            ..Default::default()
        });
        let memoria = TopKMemoria {
            retrieve_results,
            ..Default::default()
        };

        let loaded = load_current_session_memory(&memoria, "sess-42")
            .await
            .expect("session memory beyond the old top_k window should still load");

        assert!(loaded.contains("- needle"));
        assert_eq!(
            memoria.requested_top_k.lock().unwrap().as_slice(),
            &[CURRENT_SESSION_MEMORY_RETRIEVE_TOP_K]
        );
    }

    #[test]
    fn persist_local_session_memory_artifact_refreshes_metadata_sidecar() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let _guard = astra_services::session_journal::JournalDirGuard::new(tmp.path());
        let session_id = format!(
            "sess-meta-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        );
        persist_local_session_memory_artifact(
            &session_id,
            "# Session Memory\n\n## Current State\nmetadata sidecar should refresh\n",
        )
        .expect("persist local artifact");
        let metadata = load_local_session_memory_metadata(&session_id).expect("metadata");
        assert_eq!(metadata.session_id, session_id);
        assert!(metadata.last_local_refresh_at.is_some());
        assert_eq!(metadata.stable_memory_epoch, 1);
    }

    #[tokio::test]
    async fn load_current_session_memory_prefers_latest_snapshot_turn() {
        let older = SessionMemorySnapshot::from_markdown(
            "sess-42",
            "# Session Memory\n\nolder",
            3,
            SessionFacts::default(),
        )
        .to_memory_entry()
        .encode();
        let newer = SessionMemorySnapshot::from_markdown(
            "sess-42",
            "# Session Memory\n\nnewer",
            9,
            SessionFacts::default(),
        )
        .to_memory_entry()
        .encode();
        let memoria = CapturingMemoria {
            retrieve_results: Mutex::new(vec![
                MemoriaMemory {
                    content: older,
                    memory_type: SESSION_MEMORY_MEMORIA_TYPE.to_string(),
                    session_id: Some("sess-42".to_string()),
                    ..Default::default()
                },
                MemoriaMemory {
                    content: newer,
                    memory_type: SESSION_MEMORY_MEMORIA_TYPE.to_string(),
                    session_id: Some("sess-42".to_string()),
                    ..Default::default()
                },
            ]),
            ..Default::default()
        };

        let loaded = load_current_session_memory(&memoria, "sess-42")
            .await
            .expect("latest session snapshot should load");

        assert!(loaded.contains("- newer"));
        assert!(!loaded.contains("- older"));
    }

    #[test]
    fn canonicalize_session_memory_markdown_does_not_classify_narrative_wording() {
        let canonical = canonicalize_session_memory_markdown(
            "sess-42",
            "# Session Memory\n\n## Session Title\nReview uncommitted changes in the session memory feature.\n\n## Active Goals\nNone.\n\n## Pending Todos\nNone.\n\n## Completed\n- Ran tests\n\n## Current State\nThe user's request is complete. No issues remain. The session is idle.\n",
            4,
            &SessionFacts::default(),
        );

        assert!(canonical.contains("## Session Title"));
        assert!(canonical.contains("Review uncommitted changes in the session memory feature."));
        assert!(canonical.contains("## Active Goals"));
        assert!(canonical.contains("- None."));
        assert!(canonical.contains("## Pending Todos"));
        assert!(canonical.contains("The user's request is complete"));
        assert!(canonical.contains("session is idle"));
        assert!(canonical.contains("No issues remain"));
    }

    #[test]
    fn canonicalize_session_memory_does_not_promote_resolved_errors() {
        let facts = SessionFacts {
            error_state: astra_turn_types::session_facts::ErrorFact {
                total_errors: 7,
                last_error: None,
                last_error_turn: None,
            },
            ..Default::default()
        };

        let canonical = canonicalize_session_memory_markdown(
            "sess-42",
            "# Session Memory\n\n## Current State\n- Continuing verified fix\n",
            12,
            &facts,
        );

        assert!(!canonical.contains("System observed"));
        assert!(!canonical.contains("7 errors"));
        assert!(!canonical.contains("## Errors & Corrections"));
    }

    #[tokio::test]
    async fn load_current_session_memory_preferring_local_chooses_the_newest_snapshot() {
        use astra_services::SessionArtifactStore;

        let tmp = tempfile::TempDir::new().unwrap();
        let _dir_guard = astra_services::session_journal::JournalDirGuard::new(tmp.path());
        let session_id = "sess-local-first";
        let path = astra_services::local_session_artifact_store()
            .session_path(session_id, "session-memory.md")
            .unwrap();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            "# Session Memory\n\n## Current State\n- local snapshot wins\n",
        )
        .unwrap();
        persist_local_session_memory_metadata(
            session_id,
            &SessionMemoryArtifactMetadata {
                session_id: session_id.to_string(),
                last_extracted_turn: Some(2),
                ..Default::default()
            },
        )
        .unwrap();

        let memoria = CapturingMemoria {
            retrieve_results: Mutex::new(vec![MemoriaMemory {
                memory_type: SESSION_MEMORY_MEMORIA_TYPE.to_string(),
                session_id: Some(session_id.to_string()),
                content: SessionMemorySnapshot::from_markdown(
                    session_id,
                    "# Session Memory\n\n## Current State\n- remote snapshot",
                    1,
                    SessionFacts::default(),
                )
                .to_memory_entry()
                .encode(),
                ..Default::default()
            }]),
            ..Default::default()
        };

        let loaded =
            load_current_session_memory_preferring_local_with_freshness(&memoria, session_id)
                .await
                .expect("local session-memory artifact should load");

        assert!(loaded.content.contains("- local snapshot wins"));
        assert!(!loaded.content.contains("- remote snapshot"));
        assert_eq!(loaded.updated_turn, Some(2));

        memoria.retrieve_results.lock().unwrap()[0].content = SessionMemorySnapshot::from_markdown(
            session_id,
            "# Session Memory\n\n## Current State\n- newer remote snapshot wins",
            3,
            SessionFacts::default(),
        )
        .to_memory_entry()
        .encode();
        let loaded =
            load_current_session_memory_preferring_local_with_freshness(&memoria, session_id)
                .await
                .expect("newer remote snapshot");
        assert!(loaded.content.contains("- newer remote snapshot wins"));
        assert!(!loaded.content.contains("- local snapshot wins"));
        assert_eq!(loaded.updated_turn, Some(3));
    }

    #[test]
    fn from_markdown_does_not_infer_goal_semantics_from_wording() {
        let snapshot = SessionMemorySnapshot::from_markdown(
            "sess-42",
            "# Session Memory

## Session Title
review uncommitted changes

## Active Goals
- (None explicitly stated)

## Task Specification
review uncommitted changes
",
            2,
            SessionFacts::default(),
        );

        assert_eq!(
            snapshot.narrative.active_goals,
            vec!["(None explicitly stated)".to_string()]
        );
    }

    #[test]
    fn current_state_preserves_explicit_completion_facts() {
        let snapshot = SessionMemorySnapshot::from_markdown(
            "sess-42",
            "# Session Memory\n\n## Current State\n- task completed\n",
            2,
            SessionFacts::default(),
        );
        assert_eq!(
            snapshot.narrative.current_state,
            vec!["task completed".to_string()]
        );
    }

    #[test]
    fn session_memory_storage_type_stays_supported_and_transient() {
        assert_eq!(SESSION_MEMORY_MEMORIA_TYPE, "working");
        assert!(astra_prompts::memory_types::is_supported_memoria_type(
            SESSION_MEMORY_MEMORIA_TYPE
        ));
        assert!(!astra_turn_types::is_persistent_memory_type(
            SESSION_MEMORY_MEMORIA_TYPE
        ));
    }

    #[test]
    fn session_memory_extraction_uses_prompt_facing_history_boundary() {
        let messages = vec![
            json!({"role": "user", "content": "我说过的所有话，还有回复"}),
            astra_turn_types::runtime_owned_message(
                "user",
                "arbitrary resume context",
                astra_turn_types::RuntimeMessageDelivery::RequiredContext,
            ),
            json!({
                "role": "assistant",
                "content": serde_json::Value::Null,
                "tool_calls": [{
                    "id": "skill-auto-route-analyze-session",
                    "function": {"name": "skill", "arguments": "{}"}
                }]
            }),
            json!({"role": "tool", "tool_call_id": "skill-auto-route-analyze-session", "content": "<skill-loaded name=\"analyze-session\"/>"}),
            json!({"role": "assistant", "content": "你问过我总结这段会话。"}),
        ];

        let filtered = session_memory_extraction_messages(&messages);
        assert_eq!(
            filtered,
            vec![
                json!({"role": "user", "content": "我说过的所有话，还有回复"}),
                json!({"role": "assistant", "content": "你问过我总结这段会话。"}),
            ]
        );
    }
}
