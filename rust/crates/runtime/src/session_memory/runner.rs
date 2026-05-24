use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use astra_prompts::memory_proto::{MemoryEntry, NS_SESSION, ST_ACTIVE};
use astra_services::session_journal::{
    SessionMemoryExtractionErrorReason, SessionMemoryExtractionSource,
};
use astra_turn_core::cloud_session_memory_extract::{
    SESSION_MEMORY_TEMPLATE, build_extraction_prompt, extract_section,
};
use astra_turn_types::session_facts::SessionFacts;

use crate::memory_hooks::relevance::LlmConnParams;
use crate::turn::cloud::memoria_compact::MemoriaClient;
use crate::turn::llm::client::{
    apply_provider_auth, build_provider_request_body_with_overrides, global_llm_client,
    llm_request_url_for_provider, parse_nonstream_response_for_provider,
};

pub const SESSION_MEMORY_PREFIX: &str = "[@session/active]";
const LEGACY_SESSION_MEMORY_PREFIX: &str = "[@session/memory]";
const SESSION_MEMORY_SCHEMA_VERSION: u16 = 2;

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
    pub completed: Vec<String>,
    #[serde(default)]
    pub files_and_functions: Vec<String>,
    #[serde(default)]
    pub workflow: Vec<String>,
    #[serde(default)]
    pub corrections: Vec<String>,
    #[serde(default)]
    pub learnings: Vec<String>,
    #[serde(default)]
    pub worklog: Vec<String>,
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
            completed: extract_list_section(content, "Completed"),
            files_and_functions: extract_list_section(content, "Files and Functions"),
            workflow: extract_list_section(content, "Workflow"),
            corrections: extract_list_section(content, "Errors & Corrections"),
            learnings: extract_list_section(content, "Learnings"),
            worklog: extract_list_section(content, "Worklog"),
        };
        if narrative_is_empty(&narrative) {
            let fallback = single_line(&fallback_body_text(content));
            if !fallback.is_empty() {
                narrative.current_state.push(fallback.clone());
                narrative.worklog.push(fallback);
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
        push_capped_section(&mut sections, "Completed", &self.narrative.completed, 3);
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
        push_capped_section(&mut sections, "Completed", &self.narrative.completed, 3);
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
        let files_from_facts = if self.facts.active_files.is_empty() {
            self.narrative.files_and_functions.clone()
        } else {
            self.facts
                .active_files
                .iter()
                .rev()
                .take(10)
                .map(|f| format!("{} {} (t{})", f.last_action, f.path, f.turn))
                .collect()
        };
        let corrections = if self.facts.error_state.total_errors == 0 {
            self.narrative.corrections.clone()
        } else {
            let mut out = self.narrative.corrections.clone();
            if let Some(last_error) = &self.facts.error_state.last_error {
                if !out.iter().any(|line| line.contains(last_error)) {
                    out.push(format!(
                        "System observed {} errors; latest: {}",
                        self.facts.error_state.total_errors, last_error
                    ));
                }
            }
            out
        };
        format!(
            "# Session Memory\n\n## Session Title\n{title}\n\n## Active Goals\n{active_goals}\n\n## Pending Todos\n{pending_todos}\n\n## Completed\n{completed}\n\n## Current State\n{current_state}\n\n## Task Specification\n{task_spec}\n\n## Files and Functions\n{files_and_functions}\n\n## Workflow\n{workflow}\n\n## Errors & Corrections\n{corrections}\n\n## Learnings\n{learnings}\n\n## Worklog\n{worklog}",
            title = render_scalar(&self.narrative.session_title, "Current session"),
            active_goals = render_list(
                &self.narrative.active_goals,
                "- No explicit active goals captured."
            ),
            pending_todos = render_list(&self.narrative.pending_todos, "- No open loops recorded."),
            completed = render_list(&self.narrative.completed, "- No completed work recorded."),
            current_state = render_list(
                &self.narrative.current_state,
                "- No current state recorded."
            ),
            task_spec = render_scalar(&self.narrative.task_spec, "No task specification recorded."),
            files_and_functions = render_list(&files_from_facts, "- No active files captured."),
            workflow = render_list(&self.narrative.workflow, "- No workflow recorded."),
            corrections = render_list(&corrections, "- No corrections recorded."),
            learnings = render_list(&self.narrative.learnings, "- No learnings recorded."),
            worklog = render_list(&self.narrative.worklog, "- No worklog recorded."),
        )
    }
}

/// What the worker produced.
pub enum ExtractionArtifacts {
    Persisted {
        source: SessionMemoryExtractionSource,
        bytes_written: u64,
        store_attempt: u32,
        content: String,
    },
    LlmFailedPersistedFallback {
        error_reason: SessionMemoryExtractionErrorReason,
        error_detail: Option<String>,
        bytes_written: u64,
        store_attempt: u32,
        content: String,
    },
    PersistFailed {
        error_reason: SessionMemoryExtractionErrorReason,
        llm_error_reason: Option<SessionMemoryExtractionErrorReason>,
        llm_error_detail: Option<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct LlmExtractionFailure {
    reason: SessionMemoryExtractionErrorReason,
    detail: Option<String>,
}

#[allow(clippy::too_many_arguments)]
pub async fn run_extraction(
    memoria: &Arc<dyn MemoriaClient>,
    session_id: &str,
    messages: &[Value],
    turn_number: usize,
    current_tokens: usize,
    current_memory: &str,
    session_facts: &SessionFacts,
    memory_model_params: Option<&LlmConnParams>,
    llm_timeout: Duration,
    max_output_tokens: usize,
) -> ExtractionArtifacts {
    let base_memory = if current_memory.trim().is_empty() {
        SESSION_MEMORY_TEMPLATE.to_string()
    } else {
        current_memory.to_string()
    };
    let fallback = build_rule_fallback_memory(&base_memory, messages, turn_number, current_tokens);

    let Some(params) = memory_model_params else {
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
            },
            Err(error_reason) => ExtractionArtifacts::PersistFailed {
                error_reason,
                llm_error_reason: None,
                llm_error_detail: None,
            },
        };
    };

    match update_memory_with_llm(
        &base_memory,
        messages,
        params,
        llm_timeout,
        max_output_tokens,
    )
    .await
    {
        Ok(updated) => match store_session_memory(
            memoria,
            session_id,
            turn_number as u32,
            session_facts,
            &updated,
        )
        .await
        {
            Ok((bytes_written, store_attempt)) => ExtractionArtifacts::Persisted {
                source: SessionMemoryExtractionSource::Llm,
                bytes_written,
                store_attempt,
                content: updated,
            },
            Err(error_reason) => ExtractionArtifacts::PersistFailed {
                error_reason,
                llm_error_reason: None,
                llm_error_detail: None,
            },
        },
        Err(error_reason) => match store_session_memory(
            memoria,
            session_id,
            turn_number as u32,
            session_facts,
            &fallback,
        )
        .await
        {
            Ok((bytes_written, store_attempt)) => ExtractionArtifacts::LlmFailedPersistedFallback {
                error_reason: error_reason.reason,
                error_detail: error_reason.detail,
                bytes_written,
                store_attempt,
                content: fallback,
            },
            Err(store_error) => ExtractionArtifacts::PersistFailed {
                error_reason: store_error,
                llm_error_reason: Some(error_reason.reason),
                llm_error_detail: error_reason.detail,
            },
        },
    }
}

pub fn encode_session_memory_entry(session_id: &str, content: &str) -> String {
    SessionMemorySnapshot::from_markdown(session_id, content, 0, SessionFacts::default())
        .to_memory_entry()
        .encode()
}

pub fn decode_session_memory_entry(raw: &str, session_id: &str) -> Option<String> {
    decode_session_memory_snapshot(raw, session_id)
        .map(|snapshot| snapshot.to_markdown())
        .or_else(|| decode_legacy_session_memory_entry(raw, session_id))
}

pub async fn load_current_session_memory(
    memoria: &dyn MemoriaClient,
    session_id: &str,
) -> Option<String> {
    let session_id = session_id.trim();
    if session_id.is_empty() {
        return None;
    }
    let query = format!("{SESSION_MEMORY_PREFIX} {session_id} session memory");
    let memories = memoria
        .retrieve_ext(&query, Some(session_id), 5, true)
        .await
        .ok()?;
    memories
        .iter()
        .find_map(|memory| decode_session_memory_entry(&memory.content, session_id))
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
        "## Session State\nLatest state: {}",
        entry.compact_view()
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

fn decode_legacy_session_memory_entry(raw: &str, session_id: &str) -> Option<String> {
    let trimmed = raw.trim();
    if !trimmed.starts_with(LEGACY_SESSION_MEMORY_PREFIX) {
        return None;
    }
    let rest = trimmed[LEGACY_SESSION_MEMORY_PREFIX.len()..].trim_start();
    let sid_line = rest.lines().next()?.trim();
    let encoded_sid = sid_line.strip_prefix("session_id=")?.trim();
    if encoded_sid != session_id {
        return None;
    }
    let content = rest[sid_line.len()..].trim();
    (!content.is_empty()).then(|| content.to_string())
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

fn normalize_narrative(narrative: &mut SessionNarrative) {
    dedup_preserve_order(&mut narrative.active_goals);
    dedup_preserve_order(&mut narrative.pending_todos);
    dedup_preserve_order(&mut narrative.completed);
    dedup_preserve_order(&mut narrative.files_and_functions);
    dedup_preserve_order(&mut narrative.workflow);
    dedup_preserve_order(&mut narrative.corrections);
    dedup_preserve_order(&mut narrative.learnings);
    dedup_preserve_order(&mut narrative.worklog);
    narrative.pending_todos.retain(|pending| {
        !narrative.completed.iter().any(|done| {
            done.eq_ignore_ascii_case(pending) || single_line(done) == single_line(pending)
        })
    });
}

fn narrative_is_empty(narrative: &SessionNarrative) -> bool {
    narrative.session_title.is_empty()
        && narrative.task_spec.is_empty()
        && narrative.current_state.is_empty()
        && narrative.active_goals.is_empty()
        && narrative.pending_todos.is_empty()
        && narrative.completed.is_empty()
        && narrative.files_and_functions.is_empty()
        && narrative.workflow.is_empty()
        && narrative.corrections.is_empty()
        && narrative.learnings.is_empty()
        && narrative.worklog.is_empty()
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

fn render_scalar(value: &str, fallback: &str) -> String {
    if value.trim().is_empty() {
        fallback.to_string()
    } else {
        value.trim().to_string()
    }
}

fn render_list(items: &[String], fallback: &str) -> String {
    if items.is_empty() {
        fallback.to_string()
    } else {
        items
            .iter()
            .map(|item| format!("- {}", item.trim()))
            .collect::<Vec<_>>()
            .join("\n")
    }
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
    let body = build_provider_request_body_with_overrides(
        &prompt,
        &[],
        &params.model_name,
        &params.provider,
        Some(max_output_tokens),
        Some(0.0),
        false,
        &astra_turn_core::thinking_config::ThinkingConfig::Off,
        params.request_body_overrides.as_ref(),
    );
    let url = llm_request_url_for_provider(
        &params.base_url,
        &params.provider,
        &params.model_name,
        false,
    );
    let request = global_llm_client()
        .post(url)
        .timeout(llm_timeout)
        .header("content-type", "application/json");
    let request = apply_provider_auth(request, &params.provider, &params.api_key, None).json(&body);

    let response = match tokio::time::timeout(llm_timeout, request.send()).await {
        Ok(Ok(resp)) => resp,
        Ok(Err(error)) if error.is_timeout() => {
            return Err(LlmExtractionFailure {
                reason: SessionMemoryExtractionErrorReason::LlmTimeout,
                detail: None,
            });
        }
        Ok(Err(error)) => {
            return Err(LlmExtractionFailure {
                reason: SessionMemoryExtractionErrorReason::LlmError,
                detail: Some(summarize_llm_detail(&error.to_string())),
            });
        }
        Err(_) => {
            return Err(LlmExtractionFailure {
                reason: SessionMemoryExtractionErrorReason::LlmTimeout,
                detail: None,
            });
        }
    };

    let status = response.status();
    let body_text = response
        .text()
        .await
        .map_err(|error| LlmExtractionFailure {
            reason: SessionMemoryExtractionErrorReason::LlmError,
            detail: Some(summarize_llm_detail(&format!(
                "http {}: failed to read body: {error}",
                status.as_u16()
            ))),
        })?;
    let payload: Value =
        serde_json::from_str(&body_text).map_err(|error| LlmExtractionFailure {
            reason: SessionMemoryExtractionErrorReason::LlmError,
            detail: Some(summarize_llm_detail(&format!(
                "http {}: invalid json body ({error}): {}",
                status.as_u16(),
                extract_llm_error_detail(&body_text)
            ))),
        })?;
    if !status.is_success() {
        return Err(LlmExtractionFailure {
            reason: SessionMemoryExtractionErrorReason::LlmError,
            detail: Some(summarize_llm_detail(&format!(
                "http {}: {}",
                status.as_u16(),
                extract_llm_error_detail_from_json(&payload)
            ))),
        });
    }

    let parsed = parse_nonstream_response_for_provider(
        &payload,
        &params.provider,
        &params.model_name,
        Instant::now(),
    );
    let content = parsed.full_text.trim();
    if content.is_empty() {
        return Err(LlmExtractionFailure {
            reason: SessionMemoryExtractionErrorReason::EmptyResponse,
            detail: None,
        });
    }
    Ok(content.to_string())
}

fn summarize_llm_detail(text: &str) -> String {
    truncate_chars(&text.split_whitespace().collect::<Vec<_>>().join(" "), 220)
}

fn extract_llm_error_detail(body_text: &str) -> String {
    match serde_json::from_str::<Value>(body_text) {
        Ok(payload) => extract_llm_error_detail_from_json(&payload),
        Err(_) => summarize_llm_detail(body_text),
    }
}

fn extract_llm_error_detail_from_json(payload: &Value) -> String {
    payload
        .pointer("/error/message")
        .and_then(Value::as_str)
        .or_else(|| payload.pointer("/message").and_then(Value::as_str))
        .or_else(|| payload.pointer("/error/type").and_then(Value::as_str))
        .map(summarize_llm_detail)
        .unwrap_or_else(|| summarize_llm_detail(&payload.to_string()))
}

async fn store_session_memory(
    memoria: &Arc<dyn MemoriaClient>,
    session_id: &str,
    turn_number: u32,
    session_facts: &SessionFacts,
    content: &str,
) -> Result<(u64, u32), SessionMemoryExtractionErrorReason> {
    purge_prior_session_memory_entries(memoria, session_id).await?;
    let encoded = SessionMemorySnapshot::from_markdown(
        session_id,
        content,
        turn_number,
        session_facts.clone(),
    )
    .to_memory_entry()
    .encode();
    for attempt in 1..=2 {
        match memoria
            .store(&encoded, "working", Some(session_id), Some("T3"))
            .await
        {
            Ok(_) => return Ok((encoded.len() as u64, attempt)),
            Err(_) if attempt == 1 => continue,
            Err(_) => return Err(SessionMemoryExtractionErrorReason::WriteFailed),
        }
    }
    Err(SessionMemoryExtractionErrorReason::WriteFailed)
}

async fn purge_prior_session_memory_entries(
    memoria: &Arc<dyn MemoriaClient>,
    session_id: &str,
) -> Result<(), SessionMemoryExtractionErrorReason> {
    // Loop until no decodable session-memory entries remain. A single
    // top_k=16 query can leave duplicates behind when retries or scoring
    // edge-cases push extras past the page boundary; the next write would
    // see them rank ahead of the fresh entry. Cap iterations to avoid an
    // infinite loop if `delete` is silently a no-op upstream.
    const PAGE_SIZE: usize = 16;
    const MAX_PAGES: usize = 8;
    let mut seen_memory_ids = std::collections::HashSet::new();
    for _ in 0..MAX_PAGES {
        let mut memories = memoria
            .retrieve_ext(
                &format!("{SESSION_MEMORY_PREFIX} {session_id} session memory"),
                Some(session_id),
                PAGE_SIZE,
                true,
            )
            .await
            .map_err(|_| SessionMemoryExtractionErrorReason::PurgeFailed)?;
        let legacy_memories = memoria
            .retrieve_ext(
                &format!("{LEGACY_SESSION_MEMORY_PREFIX} {session_id} session memory"),
                Some(session_id),
                PAGE_SIZE,
                true,
            )
            .await
            .map_err(|_| SessionMemoryExtractionErrorReason::PurgeFailed)?;
        memories.extend(legacy_memories);

        let to_delete: Vec<String> = memories
            .iter()
            .filter(|memory| {
                decode_session_memory_snapshot(&memory.content, session_id).is_some()
                    || decode_legacy_session_memory_entry(&memory.content, session_id).is_some()
            })
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
            return Ok(());
        }
        for id in &to_delete {
            memoria
                .delete(id)
                .await
                .map_err(|_| SessionMemoryExtractionErrorReason::PurgeFailed)?;
        }
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

fn build_rule_fallback_memory(
    current_memory: &str,
    messages: &[Value],
    turn_number: usize,
    current_tokens: usize,
) -> String {
    let recent = render_recent_messages(messages, 8, 240);
    let first_user = first_user_message(messages).unwrap_or("Current session");
    let last_user = last_user_message(messages).unwrap_or(first_user);
    let last_assistant =
        last_assistant_message(messages).unwrap_or_else(|| "No assistant summary yet.".to_string());
    let errors = collect_error_lines(messages);
    format!(
        "# Session Memory\n\n## Session Title\n{first_user}\n\n## Active Goals\n- {last_user}\n\n## Pending Todos\n- Continue the current session task.\n\n## Completed\n- Session memory updated through rule fallback.\n\n## Current State\n- Turn {turn_number}\n- Approximate context size: {current_tokens} tokens\n- Latest assistant state: {last_assistant}\n\n## Task Specification\n{first_user}\n\n## Files and Functions\n{files}\n\n## Workflow\n{workflow}\n\n## Errors & Corrections\n{errors}\n\n## Learnings\n- Keep session memory synchronized when the session grows.\n\n## Worklog\n{worklog}",
        files = detect_file_mentions(messages),
        workflow = if recent.is_empty() {
            "- No recent conversation captured.".to_string()
        } else {
            recent
                .iter()
                .map(|line| format!("- {line}"))
                .collect::<Vec<_>>()
                .join("\n")
        },
        errors = if errors.is_empty() {
            "- No explicit errors captured.".to_string()
        } else {
            errors
                .iter()
                .map(|line| format!("- {line}"))
                .collect::<Vec<_>>()
                .join("\n")
        },
        worklog = if current_memory.trim().is_empty() {
            "- Initialized session memory document.".to_string()
        } else {
            format!(
                "- Refreshed existing session memory.\n- {}",
                recent
                    .first()
                    .cloned()
                    .unwrap_or_else(|| "No recent conversation captured.".to_string())
            )
        },
    )
}

fn render_recent_messages(messages: &[Value], take: usize, max_len: usize) -> Vec<String> {
    messages
        .iter()
        .rev()
        .filter_map(|msg| {
            let role = msg.get("role").and_then(Value::as_str)?;
            match role {
                "user" | "assistant" | "tool" => {
                    let text = message_text_or_summary(msg)?;
                    Some(format!("{role}: {}", truncate(&text, max_len)))
                }
                _ => None,
            }
        })
        .take(take)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect()
}

fn first_user_message(messages: &[Value]) -> Option<&str> {
    messages.iter().find_map(|msg| {
        (msg.get("role").and_then(Value::as_str) == Some("user"))
            .then(|| message_text(msg))
            .flatten()
    })
}

fn last_user_message(messages: &[Value]) -> Option<&str> {
    messages.iter().rev().find_map(|msg| {
        (msg.get("role").and_then(Value::as_str) == Some("user"))
            .then(|| message_text(msg))
            .flatten()
    })
}

fn last_assistant_message(messages: &[Value]) -> Option<String> {
    messages.iter().rev().find_map(|msg| {
        (msg.get("role").and_then(Value::as_str) == Some("assistant"))
            .then(|| message_text_or_summary(msg))
            .flatten()
    })
}

fn collect_error_lines(messages: &[Value]) -> Vec<String> {
    messages
        .iter()
        .filter_map(|msg| {
            let text = message_text_or_summary(msg)?;
            let lower = text.to_ascii_lowercase();
            (lower.contains("error") || lower.contains("fail") || lower.contains("panic"))
                .then(|| truncate(&text, 200).to_string())
        })
        .collect()
}

fn detect_file_mentions(messages: &[Value]) -> String {
    let mut seen = std::collections::BTreeSet::new();
    for msg in messages {
        let Some(text) = message_text_or_summary(msg) else {
            continue;
        };
        for token in text.split_whitespace() {
            let cleaned = token.trim_matches(|c: char| {
                matches!(c, ',' | '.' | ':' | ';' | '(' | ')' | '[' | ']' | '{' | '}')
            });
            if cleaned.contains('/')
                || cleaned.ends_with(".rs")
                || cleaned.ends_with(".ts")
                || cleaned.ends_with(".tsx")
                || cleaned.ends_with(".py")
            {
                seen.insert(cleaned.to_string());
            }
        }
    }
    if seen.is_empty() {
        "- No file paths referenced.".to_string()
    } else {
        seen.into_iter()
            .take(8)
            .map(|item| format!("- {item}"))
            .collect::<Vec<_>>()
            .join("\n")
    }
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
    }

    #[derive(Default)]
    struct FailingMemoria;

    struct DeleteFailMemoria {
        retrieve_results: Vec<MemoriaMemory>,
    }

    struct OverflowingMemoria {
        next_id: Mutex<usize>,
    }

    #[async_trait::async_trait]
    impl MemoriaClient for CapturingMemoria {
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
            self.deleted.lock().unwrap().push(memory_id.to_string());
            Ok(())
        }
    }

    #[async_trait::async_trait]
    impl MemoriaClient for FailingMemoria {
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
    impl MemoriaClient for DeleteFailMemoria {
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
    impl MemoriaClient for OverflowingMemoria {
        async fn retrieve_ext(
            &self,
            query: &str,
            _session_id: Option<&str>,
            _top_k: usize,
            _filter_session: bool,
        ) -> Result<Vec<MemoriaMemory>, String> {
            if query.starts_with(LEGACY_SESSION_MEMORY_PREFIX) {
                return Ok(Vec::new());
            }
            let mut next_id = self.next_id.lock().unwrap();
            let memory = MemoriaMemory {
                memory_id: format!("mem-overflow-{}", *next_id),
                content: encode_session_memory_entry(
                    "sess-overflow",
                    "# Session Memory\n\noverflow",
                ),
                memory_type: "working".to_string(),
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
            let request = String::from_utf8_lossy(&buf[..n]).to_string();
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
            json!({"role": "user", "content": "Fix rust/crates/runtime/src/session_memory/runner.rs"}),
            json!({"role": "assistant", "content": "Investigating the session memory runner."}),
        ]
    }

    #[tokio::test]
    async fn run_extraction_without_selector_persists_rule_fallback() {
        let memoria = Arc::new(CapturingMemoria::default()) as Arc<dyn MemoriaClient>;
        let artifacts = run_extraction(
            &memoria,
            "sess-1",
            &sample_messages(),
            3,
            12_345,
            "",
            &SessionFacts::default(),
            None,
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
                        "content": "# Session Memory\n\n## Session Title\nLLM Result"
                    }
                }]
            }),
        )
        .await;

        let memoria = Arc::new(CapturingMemoria::default()) as Arc<dyn MemoriaClient>;
        let params = LlmConnParams {
            base_url: format!("{server_url}/v1"),
            api_key: "test-key".to_string(),
            model_name: "selector-openai".to_string(),
            provider: "openai".to_string(),
            request_body_overrides: None,
            thinking_capability: None,
        };
        let artifacts = run_extraction(
            &memoria,
            "sess-openai",
            &sample_messages(),
            1,
            20_000,
            "",
            &SessionFacts::default(),
            Some(&params),
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
    async fn run_extraction_anthropic_selector_uses_native_endpoint() {
        let (server_url, server_handle) = spawn_json_server(
            Arc::new(|request: &str| {
                assert!(request.starts_with("POST /v1/messages HTTP/1.1"));
                assert!(request.contains("x-api-key: anthropic-key"));
                assert!(request.contains("anthropic-version: 2023-06-01"));
            }),
            json!({
                "content": [
                    { "type": "text", "text": "# Session Memory\n\n## Session Title\nAnthropic Result" }
                ]
            }),
        )
        .await;

        let memoria = Arc::new(CapturingMemoria::default()) as Arc<dyn MemoriaClient>;
        let params = LlmConnParams {
            base_url: server_url,
            api_key: "anthropic-key".to_string(),
            model_name: "selector-anthropic".to_string(),
            provider: "anthropic".to_string(),
            request_body_overrides: None,
            thinking_capability: None,
        };
        let artifacts = run_extraction(
            &memoria,
            "sess-anthropic",
            &sample_messages(),
            1,
            20_000,
            "",
            &SessionFacts::default(),
            Some(&params),
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
                            { "text": "# Session Memory\n\n## Session Title\nBedrock Result" }
                        ]
                    }
                }
            }),
        )
        .await;

        let memoria = Arc::new(CapturingMemoria::default()) as Arc<dyn MemoriaClient>;
        let params = LlmConnParams {
            base_url: server_url,
            api_key: "bedrock-key".to_string(),
            model_name: "anthropic.claude".to_string(),
            provider: "bedrock".to_string(),
            request_body_overrides: None,
            thinking_capability: None,
        };
        let artifacts = run_extraction(
            &memoria,
            "sess-bedrock",
            &sample_messages(),
            1,
            20_000,
            "",
            &SessionFacts::default(),
            Some(&params),
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

        let memoria = Arc::new(FailingMemoria) as Arc<dyn MemoriaClient>;
        let params = LlmConnParams {
            base_url: format!("{server_url}/v1"),
            api_key: "test-key".to_string(),
            model_name: "selector-openai".to_string(),
            provider: "openai".to_string(),
            request_body_overrides: None,
            thinking_capability: None,
        };
        let artifacts = run_extraction(
            &memoria,
            "sess-double-fail",
            &sample_messages(),
            1,
            20_000,
            "",
            &SessionFacts::default(),
            Some(&params),
            Duration::from_secs(3),
            512,
        )
        .await;

        match artifacts {
            ExtractionArtifacts::PersistFailed {
                error_reason,
                llm_error_reason,
                llm_error_detail,
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

        let memoria = Arc::new(CapturingMemoria::default()) as Arc<dyn MemoriaClient>;
        let params = LlmConnParams {
            base_url: format!("{server_url}/v1"),
            api_key: "test-key".to_string(),
            model_name: "selector-openai".to_string(),
            provider: "openai".to_string(),
            request_body_overrides: None,
            thinking_capability: None,
        };
        let artifacts = run_extraction(
            &memoria,
            "sess-http-detail",
            &sample_messages(),
            1,
            20_000,
            "",
            &SessionFacts::default(),
            Some(&params),
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
    async fn run_extraction_deletes_prior_session_memory_entries_before_store() {
        let memoria = Arc::new(CapturingMemoria::default());
        memoria.retrieve_results.lock().unwrap().extend([
            MemoriaMemory {
                memory_id: "mem-old-session".to_string(),
                content: encode_session_memory_entry("sess-1", "# Session Memory\n\nold"),
                memory_type: "working".to_string(),
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
        let memoria_dyn = Arc::clone(&memoria) as Arc<dyn MemoriaClient>;

        let artifacts = run_extraction(
            &memoria_dyn,
            "sess-1",
            &sample_messages(),
            3,
            12_345,
            "",
            &SessionFacts::default(),
            None,
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
    }

    #[tokio::test]
    async fn run_extraction_returns_purge_failed_when_prior_delete_fails() {
        let memoria = Arc::new(DeleteFailMemoria {
            retrieve_results: vec![MemoriaMemory {
                memory_id: "mem-old-session".to_string(),
                content: encode_session_memory_entry("sess-1", "# Session Memory\n\nold"),
                memory_type: "working".to_string(),
                session_id: Some("sess-1".to_string()),
                ..Default::default()
            }],
        }) as Arc<dyn MemoriaClient>;
        let artifacts = run_extraction(
            &memoria,
            "sess-1",
            &sample_messages(),
            3,
            12_345,
            "",
            &SessionFacts::default(),
            None,
            Duration::from_secs(3),
            256,
        )
        .await;

        match artifacts {
            ExtractionArtifacts::PersistFailed {
                error_reason,
                llm_error_reason,
                llm_error_detail,
            } => {
                assert_eq!(
                    error_reason,
                    SessionMemoryExtractionErrorReason::PurgeFailed
                );
                assert_eq!(llm_error_reason, None);
                assert_eq!(llm_error_detail, None);
            }
            _ => panic!("expected purge failure"),
        }
    }

    #[tokio::test]
    async fn run_extraction_returns_purge_failed_when_prior_entries_exceed_page_cap() {
        let memoria = Arc::new(OverflowingMemoria {
            next_id: Mutex::new(0),
        }) as Arc<dyn MemoriaClient>;
        let artifacts = run_extraction(
            &memoria,
            "sess-overflow",
            &sample_messages(),
            3,
            12_345,
            "",
            &SessionFacts::default(),
            None,
            Duration::from_secs(3),
            256,
        )
        .await;

        match artifacts {
            ExtractionArtifacts::PersistFailed {
                error_reason,
                llm_error_reason,
                llm_error_detail,
            } => {
                assert_eq!(
                    error_reason,
                    SessionMemoryExtractionErrorReason::PurgeFailed
                );
                assert_eq!(llm_error_reason, None);
                assert_eq!(llm_error_detail, None);
            }
            _ => panic!("expected purge failure when page cap is exceeded"),
        }
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

    #[tokio::test]
    async fn load_current_session_memory_returns_only_matching_active_entry() {
        let memoria = CapturingMemoria {
            retrieve_results: Mutex::new(vec![
                MemoriaMemory {
                    content: encode_session_memory_entry("other", "# Session Memory\n\nignore me"),
                    ..Default::default()
                },
                MemoriaMemory {
                    content: encode_session_memory_entry("sess-42", "# Session Memory\n\nhello"),
                    ..Default::default()
                },
            ]),
            ..Default::default()
        };

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

    #[test]
    fn render_recent_messages_includes_tool_heavy_web_agent_rounds() {
        let messages = vec![
            json!({"role": "user", "content": "open the homepage"}),
            json!({
                "role": "assistant",
                "content": serde_json::Value::Null,
                "tool_calls": [{
                    "function": {"name": "web_fetch"}
                }]
            }),
            json!({"role": "tool", "content": "Fetched https://example.com", "tool_call_id": "c1"}),
        ];

        assert_eq!(
            render_recent_messages(&messages, 8, 240),
            vec![
                "user: open the homepage".to_string(),
                "assistant: [called: web_fetch]".to_string(),
                "tool: Fetched https://example.com".to_string(),
            ]
        );
    }

    #[test]
    fn last_assistant_message_synthesizes_tool_call_summary() {
        let messages = vec![json!({
            "role": "assistant",
            "content": serde_json::Value::Null,
            "tool_calls": [
                {"function": {"name": "web_fetch"}},
                {"function": {"name": "bash"}}
            ]
        })];

        assert_eq!(
            last_assistant_message(&messages).as_deref(),
            Some("[called: web_fetch, bash]")
        );
    }
}
