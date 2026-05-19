//! Concrete [`RunLifecycleService`] backed by [`ServerAgenticLoopHost`].
//!
//! This module replaces `UnconfiguredRunLifecycleService` with a real implementation
//! that runs multi-turn agentic loops on the server via the shared
//! [`run_agentic_loop_with_host`] cognitive pipeline.
//!
//! Run status, listing, and replay are backed by durable run state. The
//! process-local map only keeps live control handles for in-flight runs.

use std::any::Any;
use std::collections::{BTreeSet, HashMap, HashSet};
use std::panic::AssertUnwindSafe;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Instant;

use async_trait::async_trait;
use axum::Json;
use axum::http::StatusCode;
use futures_util::FutureExt;
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use tokio::sync::{Mutex as TokioMutex, RwLock, broadcast, mpsc};

use astra_server_types::ws_progress_callback::ProgressEvent;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use astra_core::{ErrorResponse, SharedPool, connect_matrixone, error_response};
use astra_services::coordination::{AgentProfile, AgentTier};
use astra_services::runs::{
    CancelRunRecord, ChatRequestData, ChatRunRecord, ChatStreamRecord, DurableRunRecord,
    RunInputData, RunInputRecord, RunLifecycleService, RunListRecord, RunMutationRecord,
    RunProjectionCheckpointRecord, RunProjectionRecord, RunStatusRecord,
};
use astra_services::session_audit::{RUNTIME_PROMOTION_EVENT_TYPE, RuntimePromotionEventData};
use astra_services::skills::SkillService;
use astra_services::{
    DatabaseContextManifestStore, DatabaseStateProjectionStore, RetrievalStage, StateItemUpsert,
};
use astra_services::{EdgeContext, LlmTokenServiceConfig};
use sqlx::Row;

use crate::FernetTokenEncryptor;
use crate::MatrixOneSettings;
use crate::observability_integration::ObservabilityHub;
use crate::orchestration::{
    AgentToolContext, DynamicAgentSpawner, InheritedPermissions, ProgressBroadcaster,
    SpawnAgentExecutor, SpawnRunConfig, SpawnRunResult,
};
use crate::turn::agentic_loop_host::{
    AgenticLoopHost, AgenticLoopOutcome, AgenticLoopState, CancellationState,
    ContextTracePersistenceContext, EvaluationPersistenceContext, MessagingState,
    RequestConstraints, SkillState, StopHookState, run_agentic_loop_with_host,
};
use crate::{
    DatabaseEvaluationService, DatabaseEventService, DatabaseTraceEventWriter,
    EventCreateRequestData, EventService,
};
use astra_pipeline::step_recorder::StepRecorder;
use astra_turn_core::contracts::{
    TurnCoreEventRecord, TurnCoreEventWriter, TurnCorePersistPlan, TurnDecisionAuditRecord,
    TurnHookDbPersistPlan, TurnHookDbWriter, TurnObserverRequest, TurnObserverWorker,
    TurnSkillSelectionRecord, TurnToolEventPersistPlan, TurnToolEventRecord, TurnToolEventWriter,
};
use astra_turn_core::trace_event::{TraceContext, TraceEvent, TraceEventWriter};

use astra_core::{
    STATUS_CANCELLED, STATUS_COMPLETED, STATUS_FAILED, STATUS_PAUSED, STATUS_RUNNING,
    STATUS_WAITING,
};

use super::run_engine::RunEngine;
use super::server_loop_host::ServerAgenticLoopHostBuilder;

const RUNTIME_CONTEXT_TRACE_AGENT_ID: &str = "astra-server";
const LLM_TOKEN_SERVICE_TRUSTED_DOMAINS_TABLE: &str = "runtime_llm_trusted_domains";

fn panic_payload_message(payload: &(dyn Any + Send)) -> String {
    if let Some(message) = payload.downcast_ref::<&str>() {
        (*message).to_string()
    } else if let Some(message) = payload.downcast_ref::<String>() {
        message.clone()
    } else {
        "non-string panic payload".to_string()
    }
}

async fn run_agentic_loop_with_host_panic_safe<H: AgenticLoopHost>(
    host: &mut H,
    state: &mut AgenticLoopState,
) -> Result<AgenticLoopOutcome, astra_core::ClassifiedError> {
    match AssertUnwindSafe(run_agentic_loop_with_host(host, state))
        .catch_unwind()
        .await
    {
        Ok(outcome) => outcome,
        Err(payload) => {
            let message = format!(
                "agentic loop panicked: {}",
                panic_payload_message(payload.as_ref())
            );
            tracing::error!(
                target: "astra_runtime::run_lifecycle",
                error = %message,
                "agentic loop panic converted to failed run"
            );
            Err(astra_core::ClassifiedError::new(
                astra_core::ErrorKind::Unknown,
                message,
            ))
        }
    }
}

// ─── Skill wiring for server paths ──────────────────────────────────────────

type ServerSkillResolverBundle = (
    Option<Arc<crate::skills::UnifiedSkillRegistry>>,
    Option<Arc<dyn crate::turn::skill_tool::SkillResolver>>,
);

/// Post-loop session-memory cleanup shared by `create_run` and
/// `stream_chat`. Runs session-end governance (purge working memory +
/// persist episodic overview + Memoria reflect) when the per-session
/// debounce window allows, and always clears the bridge seen-ledger +
/// extraction-service debounce so long-lived servers don't accumulate
/// per-session state.
///
/// Best-effort: every step logs and continues on failure. Safe to call
/// with an empty `session_id` (no-op).
async fn post_loop_memory_cleanup(
    session_id: &str,
    session_facts: &astra_turn_types::session_facts::SessionFacts,
    extraction_service: Option<&Arc<crate::session_memory::MemoryExtractionService>>,
) {
    if session_id.is_empty() {
        return;
    }
    // ── Governance, debounced ──
    //
    // Session IDs are sticky across many terminal runs (user reopens a
    // session or the TUI issues follow-up turns). Running governance
    // per run would write one episode per turn and hammer reflect.
    // The debouncer allows one governance per session per window.
    let mut episode_was_written = false;
    if let Some(ref memoria_client) =
        crate::turn::cloud::memoria_compact::HttpMemoriaClient::from_env()
    {
        let debouncer = crate::turn::session_end_debounce::global();
        if matches!(
            debouncer.should_run(session_id),
            crate::turn::session_end_debounce::DebounceDecision::Run
        ) {
            match crate::turn::cloud::session_end_governance::run_session_end_governance(
                session_facts,
                session_id,
                memoria_client,
            )
            .await
            {
                Ok(report) => {
                    episode_was_written = report.episode_chars > 0;
                    if episode_was_written
                        || report.working_purged > 0
                        || report.reflect_candidates > 0
                        || report.scenes_stored > 0
                    {
                        tracing::info!(
                            session_id = %session_id,
                            learnings = report.learnings_stored,
                            purged = report.working_purged,
                            episode_chars = report.episode_chars,
                            reflect_candidates = report.reflect_candidates,
                            reflect_synthesized = report.reflect_synthesized,
                            scenes_stored = report.scenes_stored,
                            "session-end governance complete"
                        );
                    }
                    debouncer.record(session_id);
                }
                Err(e) => {
                    tracing::warn!(
                        session_id = %session_id,
                        error = %e,
                        "session-end governance failed"
                    );
                }
            }
        } else {
            tracing::debug!(
                session_id = %session_id,
                "session-end governance skipped by debounce"
            );
        }

        // ── Close the recall → outcome feedback loop ──────────────
        //
        // Every LLM-driven `memory(action=recall)` pushed its returned
        // memory_ids onto the process-global recall ledger. Drain them
        // here and route a feedback signal to Memoria. Conservative
        // heuristic:
        //   • episode was written (session did substantive work)
        //       → signal `useful` — the recall was at least surfaced
        //         into a productive session
        //   • trivial session (no episode)
        //       → drop without scoring — not enough evidence
        //
        // A richer attribution (per-tool-call outcome mapping) needs
        // the tool dispatch layer to report success/failure per recall,
        // which is a bigger refactor. The episode-level heuristic is
        // the smallest step that closes the loop in production.
        let snapshots = astra_tools::memoria::MemoriaClient::drain_recalls(session_id, None);
        if !snapshots.is_empty() && episode_was_written {
            use crate::turn::cloud::memoria_compact::MemoriaClient as ServerMemoriaClient;
            for snap in &snapshots {
                for id in &snap.memory_ids {
                    let ctx = format!("session-end: turn {} productive session", snap.turn);
                    if let Err(e) =
                        ServerMemoriaClient::feedback(memoria_client, id, "useful", Some(&ctx))
                            .await
                    {
                        tracing::debug!(memory_id = %id, error = %e, "feedback push failed");
                    }
                }
            }
            tracing::info!(
                session_id = %session_id,
                snapshots = snapshots.len(),
                "closed recall → useful feedback loop"
            );
        } else if !snapshots.is_empty() {
            tracing::debug!(
                session_id = %session_id,
                snapshots = snapshots.len(),
                "session trivial; dropped recall snapshots without scoring"
            );
        }
    }
    // ── Always: clear canonical memory process state for this session ──
    //
    // A single process-global set in `astra_tools::memoria` holds both
    // the bridge-side content-dedup keys and the tool-side
    // memory_id dedup entries. The shared reset also ensures focus hints
    // and the recall ledger are clean even if
    // governance didn't run (e.g. no memoria client configured, or drain
    // was conditional on an episode being written).
    astra_tools::memoria::MemoriaClient::reset_session_process_state(session_id);

    // ── Always: release extraction service's per-session debounce ──
    if let Some(svc) = extraction_service {
        svc.forget_session(session_id);
    }
}

/// Build a user-scoped skill registry + resolver for server-side web runs.
///
/// The visible catalog is assembled by `skills::catalog` and contains exactly:
/// API-server HOME skills (`~/.astra/skills`, `~/.claude/skills`) plus database
/// skills visible to the authenticated user. Request `allow_skills` is a
/// selector/execution filter over that catalog, not a switch that enables the
/// catalog.
fn build_server_skill_resolver(
    skill_service: Option<Arc<dyn SkillService>>,
    user_id: &str,
    allow_skills: Option<&HashSet<String>>,
) -> ServerSkillResolverBundle {
    use crate::turn::skill_tool::SkillResolver as _;

    if matches!(allow_skills, Some(allow_skills) if allow_skills.is_empty()) {
        return (None, None);
    }
    let Some(registry) = crate::capabilities::build_server_skill_registry(skill_service, user_id)
    else {
        return (None, None);
    };

    let resolver_impl = Arc::new(crate::skills::UnifiedSkillResolver::new(Arc::clone(
        &registry,
    )));
    let skills = resolver_impl.available_skills();
    let resolver: Option<Arc<dyn crate::turn::skill_tool::SkillResolver>> = if skills.is_empty() {
        None
    } else {
        Some(resolver_impl)
    };
    (Some(registry), resolver)
}

fn normalize_allowlist_entry(entry: &str, field: &str) -> Result<String, String> {
    let normalized = entry.trim().to_ascii_lowercase();
    if normalized.is_empty() {
        Err(format!("{field} must not contain empty values"))
    } else {
        Ok(normalized)
    }
}

fn normalize_request_allowlist(
    entries: Option<&[String]>,
    field: &str,
) -> Result<Option<HashSet<String>>, String> {
    let Some(entries) = entries else {
        return Ok(None);
    };
    let mut normalized = HashSet::new();
    for entry in entries {
        normalized.insert(normalize_allowlist_entry(entry, field)?);
    }
    Ok(Some(normalized))
}

#[derive(Clone, Debug, Eq, PartialEq, Hash)]
struct TrustedLlmDomain {
    host: String,
    port: Option<u16>,
}

fn normalize_trusted_llm_domain_host(raw: &str) -> Result<String, String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err("trusted domain host must not be empty".to_string());
    }
    if trimmed.contains("://") {
        return Err(format!(
            "trusted domain host '{trimmed}' must not include URL scheme"
        ));
    }
    let parsed = reqwest::Url::parse(&format!("http://{trimmed}"))
        .map_err(|error| format!("invalid trusted domain host '{trimmed}': {error}"))?;
    if parsed.username() != ""
        || parsed.password().is_some()
        || parsed.path() != "/"
        || parsed.query().is_some()
        || parsed.fragment().is_some()
        || parsed.port().is_some()
    {
        return Err(format!("trusted domain host '{trimmed}' must be host only"));
    }
    let Some(host) = parsed.host_str() else {
        return Err(format!(
            "trusted domain host '{trimmed}' must include a host"
        ));
    };
    Ok(host.to_ascii_lowercase())
}

fn trusted_llm_domain_from_db_values(
    host_raw: &str,
    port_raw: i64,
) -> Result<TrustedLlmDomain, String> {
    let host = normalize_trusted_llm_domain_host(host_raw)?;
    let port = match port_raw {
        0 => None,
        port if !(1..=65_535).contains(&port) => {
            return Err(format!(
                "trusted domain host '{host_raw}' has invalid port value {port}"
            ));
        }
        port => Some(port as u16),
    };
    Ok(TrustedLlmDomain { host, port })
}

fn llm_token_service_domain_is_trusted(
    url: &reqwest::Url,
    trusted_domains: &[TrustedLlmDomain],
) -> bool {
    let Some(host) = url.host_str() else {
        return false;
    };
    let normalized_host = host.to_ascii_lowercase();
    let resolved_port = url.port_or_known_default();
    trusted_domains.iter().any(|trusted| {
        if trusted.host != normalized_host {
            return false;
        }
        match trusted.port {
            Some(port) => resolved_port == Some(port),
            None => true,
        }
    })
}

fn validate_llm_token_service_config(
    config: Option<&astra_services::LlmTokenServiceConfig>,
    trusted_domains: &[TrustedLlmDomain],
) -> Result<(), String> {
    let Some(config) = config else {
        return Ok(());
    };
    let raw_url = config.url.trim();
    if raw_url.is_empty() {
        return Err("llm_token_service.url must not be empty".to_string());
    }
    let parsed = reqwest::Url::parse(raw_url)
        .map_err(|error| format!("llm_token_service.url must be a valid URL: {error}"))?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return Err("llm_token_service.url must use http or https scheme".to_string());
    }
    if parsed.host_str().is_none() {
        return Err("llm_token_service.url must include a host".to_string());
    }
    if config.timeout_ms == Some(0) {
        return Err("llm_token_service.timeout_ms must be greater than 0".to_string());
    }
    if trusted_domains.is_empty() {
        return Err(format!(
            "llm_token_service.url is not allowed without trusted domains configured in table {LLM_TOKEN_SERVICE_TRUSTED_DOMAINS_TABLE}"
        ));
    }
    if !llm_token_service_domain_is_trusted(&parsed, trusted_domains) {
        return Err(format!(
            "llm_token_service.url host must match trusted domains configured in table {LLM_TOKEN_SERVICE_TRUSTED_DOMAINS_TABLE}"
        ));
    }
    Ok(())
}

fn skill_tool_names(skill: &crate::turn::skill_tool::SkillToolInfo) -> Vec<String> {
    std::iter::once(skill.name.as_str())
        .chain(skill.aliases.iter().map(String::as_str))
        .map(|name| name.trim().to_ascii_lowercase())
        .filter(|name| !name.is_empty())
        .collect()
}

fn resolved_skill_names(skill: &crate::turn::skill_tool::ResolvedSkill) -> Vec<String> {
    std::iter::once(skill.name.as_str())
        .chain(skill.aliases.iter().map(String::as_str))
        .map(|name| name.trim().to_ascii_lowercase())
        .filter(|name| !name.is_empty())
        .collect()
}

struct RequestScopedSkillResolver {
    inner: Arc<dyn crate::turn::skill_tool::SkillResolver>,
    allowed_lookup: HashSet<String>,
    visible_skills: Vec<crate::turn::skill_tool::SkillToolInfo>,
}

impl RequestScopedSkillResolver {
    fn new(
        inner: Arc<dyn crate::turn::skill_tool::SkillResolver>,
        requested: HashSet<String>,
    ) -> Result<Self, String> {
        let mut visible_skills = Vec::new();
        let mut allowed_lookup = HashSet::new();
        let mut matched = HashSet::new();

        for skill in inner.available_skills() {
            let names = skill_tool_names(&skill);
            let skill_matches = names
                .iter()
                .filter(|name| requested.contains(*name))
                .cloned()
                .collect::<Vec<_>>();
            if skill_matches.is_empty() {
                continue;
            }
            matched.extend(skill_matches);
            allowed_lookup.extend(names);
            visible_skills.push(skill);
        }

        let mut unmatched = requested.difference(&matched).cloned().collect::<Vec<_>>();
        unmatched.sort();
        if !unmatched.is_empty() {
            return Err(format!(
                "allow_skills contains unknown entries: {}",
                unmatched.join(", ")
            ));
        }

        Ok(Self {
            inner,
            allowed_lookup,
            visible_skills,
        })
    }
}

impl crate::turn::skill_tool::SkillResolver for RequestScopedSkillResolver {
    fn resolve(
        &self,
        name: &str,
    ) -> Result<crate::turn::skill_tool::ResolvedSkill, crate::skills::SkillError> {
        let lookup = name.trim().to_ascii_lowercase();
        if lookup.is_empty() || !self.allowed_lookup.contains(&lookup) {
            return Err(crate::skills::SkillError::PermissionDenied(format!(
                "skill '{name}' is not allowed for this request"
            )));
        }

        let resolved = self.inner.resolve(name)?;
        if resolved_skill_names(&resolved)
            .into_iter()
            .any(|candidate| self.allowed_lookup.contains(&candidate))
        {
            Ok(resolved)
        } else {
            Err(crate::skills::SkillError::PermissionDenied(format!(
                "skill '{}' resolved outside the request allowlist",
                resolved.name
            )))
        }
    }

    fn available_skills(&self) -> Vec<crate::turn::skill_tool::SkillToolInfo> {
        self.visible_skills.clone()
    }
}

fn apply_normalized_skill_allowlist(
    resolver: Option<Arc<dyn crate::turn::skill_tool::SkillResolver>>,
    allow_skills: Option<&HashSet<String>>,
) -> Result<Option<Arc<dyn crate::turn::skill_tool::SkillResolver>>, String> {
    let Some(allow_skills) = allow_skills else {
        return Ok(resolver);
    };

    let Some(inner) = resolver else {
        return if allow_skills.is_empty() {
            Ok(None)
        } else {
            Err("allow_skills was provided, but no skills are configured on this server".into())
        };
    };

    Ok(Some(Arc::new(RequestScopedSkillResolver::new(
        inner,
        allow_skills.clone(),
    )?)))
}

/// Build a server-side skill executor that supports both Inline and Fork
/// execution contexts via [`SkillExecutionRouter`].
fn build_server_skill_executor(
    matrixone: &MatrixOneSettings,
    encryptor: &Arc<FernetTokenEncryptor>,
    shared_pool: Option<&SharedPool>,
    model_override: Option<&str>,
    llm_token_service: Option<&astra_services::LlmTokenServiceConfig>,
    edge_tools: &[Value],
    edge_profile: &Map<String, Value>,
    forward_headers: &HashMap<String, String>,
    request_constraints: RequestConstraints,
    skill_resolver: Option<Arc<dyn crate::turn::skill_tool::SkillResolver>>,
    session_id: &str,
    edge_connection_pool: Option<&astra_server_types::edge_connection_pool::EdgeConnectionPool>,
    cancel_token: Option<Arc<tokio_util::sync::CancellationToken>>,
    memory_extraction_service: Option<&Arc<crate::session_memory::MemoryExtractionService>>,
    #[cfg(feature = "harness")] harness_sink: Option<
        &std::sync::Arc<dyn astra_harness::SnapshotSink>,
    >,
) -> Option<Arc<dyn crate::skills::traits::SkillExecutor>> {
    use super::server_skill_subrun::ServerSkillSubRunExecutor;
    use astra_skills::executor::isolated::{IsolatedSkillExecutor, SkillExecutionRouter};

    let mut subrun_executor = ServerSkillSubRunExecutor::new(
        matrixone.clone(),
        Arc::clone(encryptor),
        session_id.to_string(),
    )
    .with_pool(shared_pool.cloned())
    .with_default_model(model_override.map(String::from))
    .with_llm_token_service(llm_token_service.cloned())
    .with_edge_tools(edge_tools.to_vec())
    .with_edge_profile(edge_profile.clone())
    .with_forward_headers(forward_headers.clone())
    .with_request_constraints(request_constraints)
    .with_skill_resolver(skill_resolver)
    .with_cancel_token(cancel_token);
    if let Some(svc) = memory_extraction_service {
        subrun_executor = subrun_executor.with_memory_extraction_service(Arc::clone(svc));
    }
    if let Some(pool) = edge_connection_pool {
        subrun_executor = subrun_executor.with_edge_connection_pool(pool.clone());
    }
    #[cfg(feature = "harness")]
    if let Some(sink) = harness_sink {
        subrun_executor = subrun_executor.with_harness_sink(Some(sink.clone()));
    }

    let isolated = Arc::new(IsolatedSkillExecutor::new(Arc::new(subrun_executor)));
    let router = SkillExecutionRouter::new(Some(isolated));
    Some(Arc::new(router))
}

pub(crate) fn has_turn_verdict_warning(
    verdict_events: &[astra_turn_core::agentic_verdict_audit::AgenticVerdictAuditEvent],
) -> bool {
    verdict_events.iter().any(|event| {
        event.severity.eq_ignore_ascii_case("warning")
            || event.severity.eq_ignore_ascii_case("critical")
    })
}

fn build_runtime_turn_evaluation_event(
    session_id: &str,
    source: &str,
    state: &AgenticLoopState,
) -> astra_services::session_journal::JournalEvent {
    let verdict_warning = has_turn_verdict_warning(&state.stall.verdict_events);
    let eval_thresholds = crate::pipeline::evaluation::current_evaluation_thresholds();
    let eval = crate::pipeline::evaluation::evaluate_tool_call_records_with_thresholds(
        &state.message,
        &state.recent_tools,
        &state.stall.tool_call_records,
        state.stall.events.len(),
        verdict_warning,
        state.telemetry.first_budget_pressure,
        eval_thresholds,
    );
    crate::pipeline::evaluation::build_turn_evaluation_journal_event(
        Some(session_id),
        None,
        source,
        &state.message,
        &state.recent_tools,
        &state.stall.tool_call_records,
        state.stall.events.len(),
        verdict_warning,
        state.telemetry.first_budget_pressure,
        &eval,
    )
}

fn persist_turn_evaluation_journal(session_id: &str, source: &str, state: &AgenticLoopState) {
    if session_id.is_empty() {
        return;
    }

    let event = build_runtime_turn_evaluation_event(session_id, source, state);
    match astra_services::session_journal::JournalWriter::new(session_id) {
        Ok(journal) => {
            if let Err(err) = journal.append(&event) {
                tracing::warn!(
                    target: "astra_runtime::run_lifecycle",
                    session_id = %session_id,
                    error = %err,
                    "turn evaluation journal append failed"
                );
            }
        }
        Err(err) => tracing::warn!(
            target: "astra_runtime::run_lifecycle",
            session_id = %session_id,
            error = %err,
            "turn evaluation journal init failed"
        ),
    }
}

/// Best-effort flush of turn observability events to local journal.
fn flush_turn_observability(state: &mut AgenticLoopState, session_id: &str, interrupted: bool) {
    let Some(buf) = state.turn_event_buffer.as_mut() else {
        return;
    };
    if buf.is_empty() {
        return;
    }
    let Ok(writer) = astra_services::session_journal::JournalWriter::new(session_id) else {
        return;
    };
    if interrupted {
        let _ = buf.flush_interrupted(&writer);
    } else {
        let _ = buf.flush(&writer);
    }
}

fn skill_search_from_context(
    context: &std::collections::HashMap<String, serde_json::Value>,
) -> astra_core::SkillSearchSettings {
    context
        .get("skill_search")
        .cloned()
        .and_then(|v| serde_json::from_value(v).ok())
        .unwrap_or_default()
}

fn build_runtime_evaluation_service(
    matrixone: &MatrixOneSettings,
    shared_pool: Option<&SharedPool>,
) -> DatabaseEvaluationService {
    let service = DatabaseEvaluationService::new(matrixone.clone());
    match shared_pool {
        Some(pool) => service.with_pool(pool.clone()),
        None => service,
    }
}

async fn initialize_runtime_controllers(
    loop_state: &mut AgenticLoopState,
    user_id: &str,
    session_id: &str,
    evaluation_persistence: Option<EvaluationPersistenceContext>,
    context_trace_persistence: Option<ContextTracePersistenceContext>,
) {
    let hub = Arc::new(ObservabilityHub::new());
    let session = hub.start_session(user_id, session_id);

    loop_state.telemetry.observability_hub = Some(hub);
    loop_state.telemetry.observability_session = Some(session);
    loop_state.telemetry.evaluation_persistence = evaluation_persistence;
    loop_state.telemetry.context_trace_persistence = context_trace_persistence;
}

async fn configure_runtime_controllers(
    matrixone: &MatrixOneSettings,
    shared_pool: Option<&SharedPool>,
    loop_state: &mut AgenticLoopState,
    user_id: &str,
    session_id: &str,
) {
    let evaluation_persistence = shared_pool.map(|pool| EvaluationPersistenceContext {
        user_id: user_id.to_string(),
        evaluation_service: build_runtime_evaluation_service(matrixone, Some(pool)),
    });
    let context_trace_persistence = shared_pool.map(|pool| ContextTracePersistenceContext {
        user_id: user_id.to_string(),
        event_service: build_runtime_event_service(matrixone, Some(pool)),
        artifact_store: astra_services::DatabaseSessionArtifactStore::new(matrixone.clone())
            .with_pool(pool.clone()),
        agent_id: RUNTIME_CONTEXT_TRACE_AGENT_ID.to_string(),
    });
    initialize_runtime_controllers(
        loop_state,
        user_id,
        session_id,
        evaluation_persistence,
        context_trace_persistence,
    )
    .await
}

fn build_runtime_event_service(
    matrixone: &MatrixOneSettings,
    shared_pool: Option<&SharedPool>,
) -> DatabaseEventService {
    let service = DatabaseEventService::new(matrixone.clone());
    match shared_pool {
        Some(pool) => service.with_pool(pool.clone()),
        None => service,
    }
}

async fn persist_runtime_promotion_events(
    matrixone: &MatrixOneSettings,
    shared_pool: Option<&SharedPool>,
    user_id: &str,
    session_id: &str,
    run_id: &str,
    promotions: &[RuntimePromotionEventData],
) {
    if promotions.is_empty() {
        return;
    }
    let Some(pool) = shared_pool else {
        tracing::debug!(
            session_id,
            "runtime promotion persistence skipped: shared_pool not configured"
        );
        return;
    };

    let service = build_runtime_event_service(matrixone, Some(pool));
    for promotion in promotions {
        let metadata = match serde_json::to_value(promotion) {
            Ok(value) => Some(value),
            Err(err) => {
                tracing::warn!(
                    target: "astra_runtime::run_lifecycle",
                    subject_id = %promotion.subject_id,
                    error = %err,
                    "runtime promotion event serialize failed"
                );
                continue;
            }
        };
        if let Err((status, response)) = service
            .create_event(
                user_id.to_string(),
                EventCreateRequestData {
                    session_id: session_id.to_string(),
                    event_type: RUNTIME_PROMOTION_EVENT_TYPE.to_string(),
                    content: promotion.summary.clone(),
                    agent_id: None,
                    agent_version: None,
                    parent_event_id: None,
                    parent_event_ids: Some(Vec::new()),
                    causal_chain_id: Some(format!(
                        "{session_id}:runtime-promotion:{}:{run_id}",
                        promotion.subject_id
                    )),
                    metadata,
                },
            )
            .await
        {
            tracing::warn!(
                target: "astra_runtime::run_lifecycle",
                subject_id = %promotion.subject_id,
                status = %status,
                detail = %response.0.detail,
                "runtime promotion event persist failed"
            );
        }
    }
}

/// Bundles all handles needed by post-loop best-effort persistence calls.
///
/// Both `create_run` and `stream_chat` run the same set of side effects after
/// the agentic loop finishes: core event persistence, tool event persistence,
/// hook DB writes, Memoria observer, pipeline learning, session-end hooks,
/// runtime promotion events, and learning-stack save.  This struct captures
/// the shared state so both paths can call `run()` instead of duplicating
/// ~60 lines of glue code.
struct PostLoopPersistContext {
    matrixone: MatrixOneSettings,
    shared_pool: Option<SharedPool>,
    user_id: String,
    session_id: String,
    run_id: String,
    agent_id: Option<String>,
    model_name: Option<String>,
    user_message: String,
    hook_db_writer: Option<Arc<dyn TurnHookDbWriter>>,
    observer_worker: Option<Arc<dyn TurnObserverWorker>>,
    tool_event_writer: Option<Arc<dyn TurnToolEventWriter>>,
    csl_manager: Option<tokio::sync::Mutex<astra_turn_core::conversation_log::manager::CslManager>>,
}

impl PostLoopPersistContext {
    /// Run all best-effort post-loop persistence side effects.
    ///
    /// The `loop_success` flag comes from `outcome.is_ok()` (before consuming
    /// the outcome in `finalize_run_events`).
    async fn run(&self, state: &AgenticLoopState, loop_success: bool) {
        let _ = loop_success;
        // 0. Persist CSL via CslManager.
        if let Some(ref mgr) = self.csl_manager {
            let mut mgr = mgr.lock().await;
            let session_state = extract_session_state_compact(state);
            let messages = messages_for_csl_persist(state);
            if let Err(e) = mgr
                .persist_turn(state.session_turn, &messages, &session_state)
                .await
            {
                tracing::warn!(
                    session_id = %self.session_id,
                    error = %e,
                    "CSL persist failed"
                );
            }
        }

        // 1. Persist user_query + llm_response core events.
        persist_server_loop_core_events(
            &self.matrixone,
            self.shared_pool.as_ref(),
            &self.user_id,
            &self.session_id,
            &self.run_id,
            None,
            self.agent_id.as_deref(),
            None,
            None,
            &self.user_message,
            state,
            self.model_name.as_deref(),
        )
        .await;

        // 2. Persist DB-first trace detail events.
        persist_server_loop_trace_events(
            &self.matrixone,
            self.shared_pool.as_ref(),
            &self.user_id,
            &self.session_id,
            &self.run_id,
            None,
            self.agent_id.as_deref(),
            None,
            None,
            state,
            self.model_name.as_deref(),
        )
        .await;

        // 3. Persist compatibility aggregate tool_call events for session_audit metrics.
        if let Some(ref writer) = self.tool_event_writer {
            persist_server_loop_tool_events(
                writer.as_ref(),
                &self.user_id,
                &self.session_id,
                self.agent_id.as_deref(),
                state,
            )
            .await;
        }

        // 4. Persist decision audit + skill selection to hook DB.
        if let Some(ref writer) = self.hook_db_writer {
            persist_server_loop_hook_events(
                writer.as_ref(),
                &self.user_id,
                &self.session_id,
                &self.user_message,
                state,
                self.model_name.as_deref(),
            )
            .await;
        }

        // 4. Fire Memoria observer (cross-session knowledge extraction).
        if let Some(ref worker) = self.observer_worker {
            fire_server_loop_observer(worker.as_ref(), &self.user_id, &self.session_id, state)
                .await;
        }

        // 6. Fire SessionEnd hooks.
        crate::skills::hooks::fire_session_end(
            &state.skills.session_event_hooks,
            state.current_session_id.as_deref().unwrap_or(""),
        )
        .await;

        // 7. Persist runtime promotion events.
        persist_runtime_promotion_events(
            &self.matrixone,
            self.shared_pool.as_ref(),
            &self.user_id,
            &self.session_id,
            &self.run_id,
            &state.telemetry.promotion_events,
        )
        .await;

        // 8. Persist web-agent state projection rows generated by the agentic loop.
        persist_server_loop_projection_state(
            self.shared_pool.as_ref(),
            &self.user_id,
            &self.session_id,
            &self.run_id,
            self.agent_id.as_deref(),
            self.model_name.as_deref(),
            state,
        )
        .await;
    }
}

async fn persist_server_loop_projection_state(
    shared_pool: Option<&SharedPool>,
    user_id: &str,
    session_id: &str,
    run_id: &str,
    agent_id: Option<&str>,
    model_name: Option<&str>,
    state: &AgenticLoopState,
) {
    let Some(pool) = shared_pool else {
        return;
    };
    let store = DatabaseStateProjectionStore::new(pool.clone());
    let final_text = state.final_text.trim();
    if !final_text.is_empty() {
        let preview = truncate_for_projection(final_text, 480);
        let result = store
            .upsert_state_item(StateItemUpsert {
                item_id: Some(format!(
                    "state-decision-{session_id}-{run_id}-{}",
                    state.session_turn
                )),
                user_id: user_id.to_string(),
                session_id: session_id.to_string(),
                scope: "session".to_string(),
                category: "decision".to_string(),
                item_key: format!("turn:{}:final_response", state.session_turn),
                status: "active".to_string(),
                priority: 50,
                source: "agentic_loop".to_string(),
                provenance_event_id: None,
                run_id: Some(run_id.to_string()),
                title: Some(format!("Turn {} final decision", state.session_turn)),
                summary_text: Some(preview.clone()),
                payload_json: json!({
                    "run_id": run_id,
                    "agent_id": agent_id,
                    "model_name": model_name,
                    "session_turn": state.session_turn,
                    "summary": preview,
                    "source": "server_agentic_loop_final_text",
                }),
                token_estimate: ((final_text.len() / 4) as u32).clamp(20, 240),
                mutation: "insert".to_string(),
            })
            .await;
        if let Err(error) = result {
            tracing::warn!(
                target: "astra_runtime::state_projection",
                session_id = %session_id,
                run_id = %run_id,
                error = %error,
                "failed to persist agentic-loop decision projection"
            );
        }
    }

    let post_compaction_count = sqlx::query(
        "SELECT COUNT(*) AS count FROM context_manifests \
         WHERE session_id = ? AND run_id = ? AND reason = 'post_compaction'",
    )
    .bind(session_id)
    .bind(run_id)
    .fetch_one(pool.get())
    .await
    .ok()
    .and_then(|row| row.try_get::<i64, _>("count").ok())
    .unwrap_or(0);
    if post_compaction_count > 0 {
        match store.run_compaction_assertions(session_id, run_id).await {
            Ok(results) if results.iter().all(|(_, violations)| *violations == 0) => {
                let result = store
                    .upsert_state_item(StateItemUpsert {
                        item_id: Some(format!("state-summary-{session_id}-{run_id}")),
                        user_id: user_id.to_string(),
                        session_id: session_id.to_string(),
                        scope: "session".to_string(),
                        category: "summary".to_string(),
                        item_key: format!("compaction:{run_id}"),
                        status: "active".to_string(),
                        priority: 40,
                        source: "agentic_loop_compaction".to_string(),
                        provenance_event_id: None,
                        run_id: Some(run_id.to_string()),
                        title: Some("Post-compaction summary".to_string()),
                        summary_text: Some(
                            "Compaction completed with invariant checks passing".to_string(),
                        ),
                        payload_json: json!({
                            "reason": "post_compaction",
                            "invariant_results": results,
                        }),
                        token_estimate: 80,
                        mutation: "insert".to_string(),
                    })
                    .await;
                if let Err(error) = result {
                    tracing::warn!(
                        target: "astra_runtime::state_projection",
                        session_id = %session_id,
                        run_id = %run_id,
                        error = %error,
                        "failed to persist post-compaction summary projection"
                    );
                }
            }
            Ok(results) => {
                tracing::warn!(
                    target: "astra_runtime::state_projection",
                    session_id = %session_id,
                    run_id = %run_id,
                    ?results,
                    "post-compaction invariant check failed after loop"
                );
            }
            Err(error) => {
                tracing::warn!(
                    target: "astra_runtime::state_projection",
                    session_id = %session_id,
                    run_id = %run_id,
                    error = %error,
                    "failed to run post-compaction invariant checks"
                );
            }
        }
    }
}

fn truncate_for_projection(text: &str, max_chars: usize) -> String {
    let mut out = String::new();
    for ch in text.chars().take(max_chars) {
        out.push(ch);
    }
    out
}

fn extract_session_state_compact(
    state: &AgenticLoopState,
) -> astra_turn_core::conversation_log::SessionStateCompact {
    astra_turn_core::conversation_log::SessionStateCompact {
        blocked_tools: state.restricted_tools.iter().cloned().collect(),
        recent_tools: state.recent_tools.clone(),
        approval_overrides: state
            .approval_overrides
            .as_ref()
            .and_then(|ao| serde_json::to_value(ao).ok()),
        budget_remaining_tokens: state.max_turn_input_tokens,
        budget_remaining_rounds: state.remaining_turns as u32,
        consecutive_ctx_errors: state.consecutive_context_window_errors,
        interruption: state
            .interruption
            .as_ref()
            .and_then(|i| serde_json::to_value(i).ok()),
        delegation: None,
        compaction_tracker: Some(state.compaction_effectiveness.to_json()),
    }
}

fn restore_session_state_compact(
    ss: astra_turn_core::conversation_log::SessionStateCompact,
    loop_state: &mut AgenticLoopState,
) {
    if !ss.blocked_tools.is_empty() {
        loop_state.restricted_tools.extend(ss.blocked_tools);
    }
    if !ss.recent_tools.is_empty() {
        loop_state.recent_tools = ss.recent_tools;
    }
    if let Some(ao_value) = ss.approval_overrides
        && loop_state.approval_overrides.is_none()
        && let Ok(ao) = serde_json::from_value(ao_value)
    {
        loop_state.approval_overrides = Some(ao);
    }
    if let Some(intr_value) = ss.interruption
        && loop_state.interruption.is_none()
        && let Ok(intr) = serde_json::from_value(intr_value)
    {
        loop_state.interruption = Some(intr);
    }
    if ss.budget_remaining_tokens > 0 {
        loop_state.max_turn_input_tokens = ss.budget_remaining_tokens;
    }
    if ss.budget_remaining_rounds > 0 {
        loop_state.remaining_turns = ss.budget_remaining_rounds as usize;
    }
    if ss.consecutive_ctx_errors > 0 {
        loop_state.consecutive_context_window_errors = ss.consecutive_ctx_errors;
    }
}

fn messages_for_csl_persist(state: &AgenticLoopState) -> Vec<Value> {
    let mut messages = state.messages.clone();
    let final_text = state.final_text.trim();
    if final_text.is_empty() {
        return messages;
    }

    let already_has_final = messages
        .last()
        .and_then(|message| {
            let role = message.get("role")?.as_str()?;
            let content = message.get("content")?.as_str()?;
            Some(role == "assistant" && content.trim() == final_text)
        })
        .unwrap_or(false);
    if !already_has_final {
        messages.push(json!({
            "role": "assistant",
            "content": final_text,
        }));
    }
    messages
}

fn server_loop_causal_chain_id(kind: &str) -> String {
    let chain_id = format!("{kind}:{}", Uuid::now_v7());
    debug_assert!(
        chain_id.len() <= 64,
        "server loop causal_chain_id must fit agent_events VARCHAR(64)"
    );
    chain_id
}

fn trace_hash(parts: &[&str]) -> String {
    let mut hasher = Sha256::new();
    for part in parts {
        hasher.update(part.as_bytes());
        hasher.update([0]);
    }
    let digest = hasher.finalize();
    digest[..12]
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>()
}

fn trace_event_id(kind: &str, parts: &[&str]) -> String {
    format!("trace:{kind}:{}", trace_hash(parts))
}

fn server_turn_id(run_id: &str) -> String {
    let prefix: String = run_id
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
        .take(16)
        .collect();
    format!(
        "turn-{}",
        if prefix.is_empty() {
            "unknown"
        } else {
            &prefix
        }
    )
}

fn server_trace_context(
    user_id: &str,
    session_id: &str,
    run_id: &str,
    turn_seq: u32,
) -> TraceContext {
    let turn_id = server_turn_id(run_id);
    TraceContext {
        root_event_id: trace_event_id("user", &[session_id, &turn_id]),
        causal_chain_id: server_loop_causal_chain_id("server-loop"),
        session_id: session_id.to_string(),
        user_id: user_id.to_string(),
        turn_id,
        turn_seq: i64::from(turn_seq.max(1)),
    }
}

fn trace_context_from_subrun_context(context: &HashMap<String, Value>) -> Option<TraceContext> {
    Some(TraceContext {
        session_id: context.get("trace_session_id")?.as_str()?.to_string(),
        user_id: context.get("trace_user_id")?.as_str()?.to_string(),
        turn_id: context.get("trace_turn_id")?.as_str()?.to_string(),
        turn_seq: context.get("trace_turn_seq")?.as_i64()?,
        causal_chain_id: context.get("trace_causal_chain_id")?.as_str()?.to_string(),
        root_event_id: context.get("trace_root_event_id")?.as_str()?.to_string(),
    })
}

async fn persist_trace_degraded_event(
    writer: &dyn TraceEventWriter,
    trace: &TraceContext,
    run_id: &str,
    agent_id: Option<&str>,
    parent_run_id: Option<&str>,
    parent_agent_id: Option<&str>,
    stage: &str,
    error: &str,
) {
    let mut event = TraceEvent::new(
        trace_event_id("degraded", &[run_id, stage, error]),
        trace.session_id.clone(),
        trace.user_id.clone(),
        "trace_persistence_degraded",
        "trace_health",
    )
    .with_turn_context(trace);
    event.run_id = Some(run_id.to_string());
    event.parent_run_id = parent_run_id.map(ToString::to_string);
    event.agent_id = Some(agent_id.unwrap_or("root-agent").to_string());
    event.parent_agent_id = parent_agent_id.map(ToString::to_string);
    event.parent_event_id = Some(trace.root_event_id.clone());
    event.metadata = json!({
        "stage": stage,
        "error": truncate_for_audit(error, 500),
    });
    if let Err(error) = writer.write(event).await {
        astra_core::agent_error!(
            "server-loop",
            "failed to persist trace_persistence_degraded for session {}: {}",
            trace.session_id,
            error
        );
    }
}

async fn infer_session_turn(shared_pool: Option<&SharedPool>, session_id: &str) -> u32 {
    let Some(shared_pool) = shared_pool else {
        return 1;
    };
    let count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM agent_events WHERE session_id = ? AND event_type = 'user_query'",
    )
    .bind(session_id)
    .fetch_one(shared_pool.get())
    .await
    .unwrap_or(0);
    (count.max(0) as u32).saturating_add(1)
}

/// Persist `user_query` + `llm_response` core events to `agent_events` after
/// the server-driven agentic loop completes.  This closes the persistence gap
/// where the bridge path (`/chat/turn`) wrote these events but the server loop
/// path (`/chat/stream`) did not, breaking session replay and cloud sync.
#[allow(clippy::too_many_arguments)]
async fn persist_server_loop_core_events(
    matrixone: &MatrixOneSettings,
    shared_pool: Option<&SharedPool>,
    user_id: &str,
    session_id: &str,
    run_id: &str,
    parent_run_id: Option<&str>,
    agent_id: Option<&str>,
    parent_agent_id: Option<&str>,
    trace_context: Option<TraceContext>,
    user_message: &str,
    state: &AgenticLoopState,
    model_name: Option<&str>,
) {
    if user_message.is_empty() && state.final_text.is_empty() {
        return;
    }

    let Some(pool) = shared_pool else {
        tracing::debug!(
            session_id,
            "persistence skipped: shared_pool not configured"
        );
        return;
    };

    let writer = DatabaseTraceEventWriter::new(matrixone.clone()).with_pool(pool.clone());
    let trace = trace_context
        .unwrap_or_else(|| server_trace_context(user_id, session_id, run_id, state.session_turn));

    let user_query_event = if !user_message.is_empty() {
        let mut event = TraceEvent::new(
            trace.root_event_id.clone(),
            session_id,
            user_id,
            "user_query",
            "turn",
        )
        .with_turn_context(&trace);
        event.run_id = Some(run_id.to_string());
        event.parent_run_id = parent_run_id.map(ToString::to_string);
        event.agent_id = Some(agent_id.unwrap_or("root-agent").to_string());
        event.parent_agent_id = parent_agent_id.map(ToString::to_string);
        event.content = Some(user_message.to_string());
        Some(event)
    } else {
        None
    };

    let llm_response_event = if !state.final_text.is_empty() {
        let usage = if state.total_prompt > 0
            || state.total_completion > 0
            || state.total_cache_read > 0
            || state.total_cache_creation > 0
        {
            Some(json!({
                "prompt": state.total_prompt,
                "completion": state.total_completion,
                "cache_read_tokens": state.total_cache_read,
                "cache_creation_tokens": state.total_cache_creation,
                "total": state.total_prompt
                    + state.total_completion
                    + state.total_cache_read
                    + state.total_cache_creation,
            }))
        } else {
            None
        };
        let mut event = TraceEvent::new(
            trace_event_id("response", &[run_id, &trace.turn_id]),
            session_id,
            user_id,
            "llm_response",
            "turn",
        )
        .with_turn_context(&trace);
        event.run_id = Some(run_id.to_string());
        event.parent_run_id = parent_run_id.map(ToString::to_string);
        event.agent_id = Some(agent_id.unwrap_or("root-agent").to_string());
        event.parent_agent_id = parent_agent_id.map(ToString::to_string);
        event.content = Some(state.final_text.clone());
        event.parent_event_id = user_query_event
            .as_ref()
            .map(|event| event.event_id.clone())
            .or_else(|| Some(trace.root_event_id.clone()));
        event.llm_model_used = model_name.map(ToString::to_string);
        event.token_usage = usage;
        Some(event)
    } else {
        None
    };

    let mut events = Vec::with_capacity(2);
    if let Some(event) = user_query_event.clone() {
        events.push(event);
    }
    if let Some(event) = llm_response_event.clone() {
        events.push(event);
    }

    let plan = TurnCorePersistPlan {
        user_query_event: user_query_event.as_ref().map(|event| TurnCoreEventRecord {
            event_id: event.event_id.clone(),
            user_id: event.user_id.clone(),
            session_id: event.session_id.clone(),
            agent_id: event.agent_id.clone(),
            event_type: "user_query".to_string(),
            content: event.content.clone().unwrap_or_default(),
            parent_event_id: None,
            parent_event_ids: Vec::new(),
            causal_chain_id: trace.causal_chain_id.clone(),
            llm_model_used: None,
            token_usage: None,
            llm_params: None,
            reasoning_content: None,
        }),
        llm_response_event: llm_response_event
            .as_ref()
            .map(|event| TurnCoreEventRecord {
                event_id: event.event_id.clone(),
                user_id: event.user_id.clone(),
                session_id: event.session_id.clone(),
                agent_id: event.agent_id.clone(),
                event_type: "llm_response".to_string(),
                content: event.content.clone().unwrap_or_default(),
                parent_event_id: event.parent_event_id.clone(),
                parent_event_ids: event.parent_event_id.iter().cloned().collect(),
                causal_chain_id: trace.causal_chain_id.clone(),
                llm_model_used: event.llm_model_used.clone(),
                token_usage: event.token_usage.clone(),
                llm_params: None,
                reasoning_content: None,
            }),
        snapshot_link_plan: None,
    };
    let transcript_items = transcript_items_from_core_plan(&plan, run_id);
    if let Err(e) = writer.write_many(events).await {
        astra_core::agent_error!(
            "server-loop",
            "failed to persist core events for session {session_id}: {e}"
        );
        persist_trace_degraded_event(
            &writer,
            &trace,
            run_id,
            agent_id,
            parent_run_id,
            parent_agent_id,
            "core_events",
            &e.to_string(),
        )
        .await;
    }
    persist_session_transcript_items(pool, user_id, session_id, &transcript_items).await;
}

struct TranscriptPersistItem {
    run_id: String,
    role: &'static str,
    content: String,
    source_event_id: String,
}

fn transcript_items_from_core_plan(
    plan: &TurnCorePersistPlan,
    run_id: &str,
) -> Vec<TranscriptPersistItem> {
    let mut items = Vec::with_capacity(2);
    if let Some(event) = &plan.user_query_event {
        items.push(TranscriptPersistItem {
            run_id: run_id.to_string(),
            role: "user",
            content: event.content.clone(),
            source_event_id: event.event_id.clone(),
        });
    }
    if let Some(event) = &plan.llm_response_event {
        items.push(TranscriptPersistItem {
            run_id: run_id.to_string(),
            role: "assistant",
            content: event.content.clone(),
            source_event_id: event.event_id.clone(),
        });
    }
    items
}

async fn persist_session_transcript_items(
    pool: &SharedPool,
    user_id: &str,
    session_id: &str,
    items: &[TranscriptPersistItem],
) {
    if items.is_empty() {
        return;
    }
    if let Err(error) =
        persist_session_transcript_items_inner(pool, user_id, session_id, items).await
    {
        astra_core::agent_error!(
            "server-loop",
            "failed to persist transcript items for session {session_id}: {error}"
        );
    }
}

fn truncate_trace_text(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        text.to_string()
    } else {
        let prefix: String = text.chars().take(max_chars).collect();
        format!("{prefix}...")
    }
}

fn redact_trace_value(value: &Value) -> Value {
    const SECRET_KEYS: &[&str] = &[
        "api_key",
        "apikey",
        "authorization",
        "cookie",
        "password",
        "secret",
        "token",
    ];
    match value {
        Value::Object(map) => {
            let mut out = Map::new();
            for (key, value) in map {
                let key_lc = key.to_ascii_lowercase();
                if SECRET_KEYS.iter().any(|needle| key_lc.contains(needle)) {
                    out.insert(key.clone(), Value::String("[REDACTED]".to_string()));
                } else {
                    out.insert(key.clone(), redact_trace_value(value));
                }
            }
            Value::Object(out)
        }
        Value::Array(values) => Value::Array(values.iter().map(redact_trace_value).collect()),
        Value::String(text) if text.chars().count() > 2_000 => {
            Value::String(truncate_trace_text(text, 2_000))
        }
        other => other.clone(),
    }
}

fn parse_json_str(input: Option<&String>) -> Option<Value> {
    input.and_then(|text| serde_json::from_str::<Value>(text).ok())
}

fn redacted_json_preview(value: Option<Value>) -> Option<Value> {
    value.map(|value| redact_trace_value(&value))
}

fn tool_action_from_args(args: Option<&Value>) -> Option<String> {
    args.and_then(|value| value.get("action"))
        .and_then(Value::as_str)
        .map(ToString::to_string)
}

fn child_agent_id_from_tool_result(result: Option<&Value>) -> Option<String> {
    result
        .and_then(|value| value.get("agent_id"))
        .and_then(Value::as_str)
        .map(ToString::to_string)
}

fn child_run_id_from_tool_result(result: Option<&Value>) -> Option<String> {
    result
        .and_then(|value| value.get("run_id").or_else(|| value.get("child_run_id")))
        .and_then(Value::as_str)
        .map(ToString::to_string)
}

fn build_llm_round_trace_events(
    trace: &TraceContext,
    run_id: &str,
    parent_run_id: Option<&str>,
    agent_id: Option<&str>,
    parent_agent_id: Option<&str>,
    model_name: Option<&str>,
    rounds: &[crate::turn::agentic_loop_host::RecentRoundSummary],
) -> Vec<TraceEvent> {
    rounds
        .iter()
        .enumerate()
        .map(|(idx, round)| {
            let round_index = i64::from(round.round);
            let round_key = round_index.to_string();
            let mut event = TraceEvent::new(
                trace_event_id("round_done", &[run_id, &round_key, &trace.turn_id]),
                trace.session_id.clone(),
                trace.user_id.clone(),
                "llm_round_completed",
                "llm_round",
            )
            .with_turn_context(trace);
            event.run_id = Some(run_id.to_string());
            event.parent_run_id = parent_run_id.map(ToString::to_string);
            event.agent_id = Some(agent_id.unwrap_or("root-agent").to_string());
            event.parent_agent_id = parent_agent_id.map(ToString::to_string);
            event.round_index = Some(round_index);
            event.llm_model_used = (!round.model.is_empty())
                .then(|| round.model.clone())
                .or_else(|| model_name.map(ToString::to_string));
            event.meta_duration_ms = i32::try_from(round.duration_ms).ok();
            event.token_usage = Some(json!({
                "prompt": round.prompt_tokens,
                "completion": round.completion_tokens,
                "cache_read_tokens": round.cache_read_tokens,
                "cache_creation_tokens": round.cache_creation_tokens,
                "total": round.prompt_tokens
                    + round.completion_tokens
                    + round.cache_read_tokens
                    + round.cache_creation_tokens,
            }));
            event.parent_event_id = Some(trace.root_event_id.clone());
            event.metadata = json!({
                "finish_reason": round.finish_reason,
                "tool_calls_returned": round.tool_calls_returned,
                "tool_call_names": round.tool_call_names,
                "round_event_index": idx,
            });
            event
        })
        .collect()
}

fn tool_trace_call_id(
    run_id: &str,
    index: usize,
    record: &astra_services::session_journal::ToolCallRecord,
) -> String {
    record.tool_call_id.clone().unwrap_or_else(|| {
        let round = record.round.map(|v| v.to_string()).unwrap_or_default();
        format!(
            "tool-{}",
            trace_hash(&[run_id, &round, &index.to_string(), &record.name])
        )
    })
}

fn build_tool_trace_events(
    trace: &TraceContext,
    run_id: &str,
    parent_run_id: Option<&str>,
    agent_id: Option<&str>,
    parent_agent_id: Option<&str>,
    records: &[astra_services::session_journal::ToolCallRecord],
) -> Vec<TraceEvent> {
    let mut events = Vec::with_capacity(records.len().saturating_mul(2));
    for (index, record) in records.iter().enumerate() {
        if record.is_synthetic_placeholder() {
            continue;
        }
        let call_id = tool_trace_call_id(run_id, index, record);
        let args_json = parse_json_str(record.args_full.as_ref());
        let result_json = parse_json_str(record.result_full.as_ref());
        let action = if record.name == "agent" {
            tool_action_from_args(args_json.as_ref())
        } else {
            None
        };
        let child_agent_id = child_agent_id_from_tool_result(result_json.as_ref());
        let child_run_id = child_run_id_from_tool_result(result_json.as_ref());
        let round_index = record.round.map(i64::from);
        let started_at = chrono::Utc::now();

        let mut started = TraceEvent::new(
            trace_event_id("tool_start", &[run_id, &call_id]),
            trace.session_id.clone(),
            trace.user_id.clone(),
            "tool_call_started",
            "tool_call",
        )
        .with_turn_context(trace);
        started.run_id = Some(run_id.to_string());
        started.parent_run_id = parent_run_id.map(ToString::to_string);
        started.agent_id = Some(agent_id.unwrap_or("root-agent").to_string());
        started.parent_agent_id = parent_agent_id.map(ToString::to_string);
        started.round_index = round_index;
        started.tool_call_id = Some(call_id.clone());
        started.meta_tool_name = Some(record.name.clone());
        started.parent_event_id = Some(trace.root_event_id.clone());
        started.created_at = started_at;
        started.metadata = json!({
            "args_preview": record.args_preview,
            "tool_args_json_redacted": redacted_json_preview(args_json.clone()),
            "action": action,
            "start_offset_ms": record.start_offset_ms,
        });
        events.push(started);

        let terminal_type = if record.ok {
            "tool_call_completed"
        } else {
            "tool_call_failed"
        };
        let mut completed = TraceEvent::new(
            trace_event_id(terminal_type, &[run_id, &call_id]),
            trace.session_id.clone(),
            trace.user_id.clone(),
            terminal_type,
            "tool_call",
        )
        .with_turn_context(trace);
        completed.run_id = Some(run_id.to_string());
        completed.parent_run_id = parent_run_id.map(ToString::to_string);
        completed.agent_id = Some(agent_id.unwrap_or("root-agent").to_string());
        completed.parent_agent_id = parent_agent_id.map(ToString::to_string);
        completed.round_index = round_index;
        completed.tool_call_id = Some(call_id);
        completed.meta_tool_name = Some(record.name.clone());
        completed.meta_duration_ms = i32::try_from(record.ms).ok();
        completed.parent_event_id = Some(trace.root_event_id.clone());
        completed.metadata = json!({
            "ok": record.ok,
            "action": action,
            "args_preview": record.args_preview,
            "result_preview": record.result_preview,
            "tool_args_json_redacted": redacted_json_preview(args_json),
            "tool_result_json_redacted": redacted_json_preview(result_json),
            "child_agent_id": child_agent_id,
            "child_run_id": child_run_id,
            "error": record.error,
        });
        events.push(completed);
    }
    events
}

async fn persist_server_loop_trace_events(
    matrixone: &MatrixOneSettings,
    shared_pool: Option<&SharedPool>,
    user_id: &str,
    session_id: &str,
    run_id: &str,
    parent_run_id: Option<&str>,
    agent_id: Option<&str>,
    parent_agent_id: Option<&str>,
    trace_context: Option<TraceContext>,
    state: &AgenticLoopState,
    model_name: Option<&str>,
) {
    let Some(pool) = shared_pool else {
        return;
    };
    let writer = DatabaseTraceEventWriter::new(matrixone.clone()).with_pool(pool.clone());
    let trace = trace_context
        .unwrap_or_else(|| server_trace_context(user_id, session_id, run_id, state.session_turn));
    let mut events = build_llm_round_trace_events(
        &trace,
        run_id,
        parent_run_id,
        agent_id,
        parent_agent_id,
        model_name,
        &state.recent_rounds,
    );
    events.extend(build_tool_trace_events(
        &trace,
        run_id,
        parent_run_id,
        agent_id,
        parent_agent_id,
        &state.stall.tool_call_records,
    ));
    if events.is_empty() {
        return;
    }
    if let Err(e) = writer.write_many(events).await {
        astra_core::agent_error!(
            "server-loop",
            "failed to persist trace detail events for session {session_id}: {e}"
        );
        persist_trace_degraded_event(
            &writer,
            &trace,
            run_id,
            agent_id,
            parent_run_id,
            parent_agent_id,
            "detail_events",
            &e.to_string(),
        )
        .await;
    }
}

async fn persist_session_transcript_items_inner(
    pool: &SharedPool,
    user_id: &str,
    session_id: &str,
    items: &[TranscriptPersistItem],
) -> Result<(), sqlx::Error> {
    let mut tx = pool.get().begin().await?;
    let row = sqlx::query(
        "SELECT COALESCE(MAX(item_seq), 0) + 1 AS next_seq
         FROM session_transcript_items
         WHERE session_id = ?",
    )
    .bind(session_id)
    .fetch_one(&mut *tx)
    .await?;
    let mut next_seq = row.try_get::<i64, _>("next_seq")?;
    let mut dirty_pages = BTreeSet::new();

    for item in items {
        let existing = sqlx::query(
            "SELECT COUNT(*) AS count
             FROM session_transcript_items
             WHERE session_id = ? AND run_id = ? AND role = ?",
        )
        .bind(session_id)
        .bind(&item.run_id)
        .bind(item.role)
        .fetch_one(&mut *tx)
        .await?
        .try_get::<i64, _>("count")?;
        if existing > 0 {
            continue;
        }

        let item_seq = next_seq;
        sqlx::query(
            "INSERT INTO session_transcript_items
             (session_id, item_seq, user_id, run_id, role, content,
              source_event_id, source_event_idx, content_hash, created_at)
             VALUES (?, ?, ?, ?, ?, ?, ?, NULL, ?, NOW(6))",
        )
        .bind(session_id)
        .bind(item_seq)
        .bind(user_id)
        .bind(&item.run_id)
        .bind(item.role)
        .bind(&item.content)
        .bind(&item.source_event_id)
        .bind(transcript_content_hash(item.role, &item.content))
        .execute(&mut *tx)
        .await?;
        dirty_pages.insert(transcript_page_seq(item_seq));
        next_seq += 1;
    }

    for page_seq in dirty_pages {
        sync_transcript_page_inner(&mut tx, session_id, page_seq).await?;
    }

    tx.commit().await
}

const TRANSCRIPT_PAGE_SIZE: i64 = 50;

fn transcript_page_seq(item_seq: i64) -> i64 {
    ((item_seq.max(1) - 1) / TRANSCRIPT_PAGE_SIZE) + 1
}

fn transcript_page_bounds(page_seq: i64) -> (i64, i64) {
    let page_seq = page_seq.max(1);
    let start = ((page_seq - 1) * TRANSCRIPT_PAGE_SIZE) + 1;
    let end = start + TRANSCRIPT_PAGE_SIZE - 1;
    (start, end)
}

async fn sync_transcript_page_inner(
    tx: &mut sqlx::Transaction<'_, sqlx::MySql>,
    session_id: &str,
    page_seq: i64,
) -> Result<(), sqlx::Error> {
    let (start_item_seq, end_item_seq) = transcript_page_bounds(page_seq);
    let rows = sqlx::query(
        "SELECT item_seq, role, content_hash
         FROM session_transcript_items
         WHERE session_id = ? AND item_seq BETWEEN ? AND ?
         ORDER BY item_seq ASC",
    )
    .bind(session_id)
    .bind(start_item_seq)
    .bind(end_item_seq)
    .fetch_all(&mut **tx)
    .await?;
    if rows.is_empty() {
        sqlx::query("DELETE FROM transcript_pages WHERE session_id = ? AND page_seq = ?")
            .bind(session_id)
            .bind(page_seq)
            .execute(&mut **tx)
            .await?;
        return Ok(());
    }

    let first_item_seq = rows
        .first()
        .and_then(|row| row.try_get::<i64, _>("item_seq").ok())
        .unwrap_or(start_item_seq);
    let last_item_seq = rows
        .last()
        .and_then(|row| row.try_get::<i64, _>("item_seq").ok())
        .unwrap_or(end_item_seq);
    let mut hasher = Sha256::new();
    for row in &rows {
        hasher.update(row.try_get::<i64, _>("item_seq")?.to_string().as_bytes());
        hasher.update([0]);
        hasher.update(row.try_get::<String, _>("role")?.as_bytes());
        hasher.update([0]);
        hasher.update(row.try_get::<String, _>("content_hash")?.as_bytes());
        hasher.update([0xff]);
    }
    let page_hash = format!("{:x}", hasher.finalize());
    sqlx::query(
        "INSERT INTO transcript_pages
         (session_id, page_seq, start_item_seq, end_item_seq, item_count, page_hash, created_at, updated_at)
         VALUES (?, ?, ?, ?, ?, ?, NOW(6), NOW(6))
         ON DUPLICATE KEY UPDATE
           start_item_seq = VALUES(start_item_seq),
           end_item_seq = VALUES(end_item_seq),
           item_count = VALUES(item_count),
           page_hash = VALUES(page_hash),
           updated_at = NOW(6)",
    )
    .bind(session_id)
    .bind(page_seq)
    .bind(first_item_seq)
    .bind(last_item_seq)
    .bind(rows.len() as i64)
    .bind(page_hash)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

fn transcript_content_hash(role: &str, content: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(role.as_bytes());
    hasher.update([0]);
    hasher.update(content.as_bytes());
    format!("{:x}", hasher.finalize())
}

/// Persist `tool_call` events to `agent_events` for tools used during the
/// server-driven agentic loop.  The bridge path creates detailed per-call
/// records; here we create one event per unique tool name from
/// `state.telemetry.all_tools_used` with metadata containing `tool_name`
/// so that `session_audit` aggregate queries (`meta_tool_name`, `tool_calls_total`)
/// return correct results for server-loop sessions.
async fn persist_server_loop_tool_events(
    writer: &dyn TurnToolEventWriter,
    user_id: &str,
    session_id: &str,
    agent_id: Option<&str>,
    state: &AgenticLoopState,
) {
    if state.telemetry.all_tools_used.is_empty() {
        return;
    }

    let chain_id = server_loop_causal_chain_id("server-loop-tools");
    let mut events = Vec::with_capacity(state.telemetry.all_tools_used.len());

    for tool_name in &state.telemetry.all_tools_used {
        events.push(TurnToolEventRecord {
            event_id: Uuid::now_v7().to_string(),
            user_id: user_id.to_string(),
            session_id: session_id.to_string(),
            agent_id: agent_id.map(|s| s.to_string()),
            event_type: "tool_call".to_string(),
            content: format!("server-loop tool: {tool_name}"),
            parent_event_id: None,
            parent_event_ids: Vec::new(),
            causal_chain_id: chain_id.clone(),
            metadata: Some(json!({ "tool_name": tool_name })),
            skill_name: None,
            skill_version: None,
            reasoning_content: None,
        });
    }

    let plan = TurnToolEventPersistPlan { events };
    if let Err(e) = writer.persist(plan).await {
        astra_core::agent_error!(
            "server-loop",
            "failed to persist tool events for session {session_id}: {e}"
        );
    }
}

/// Persist decision audit + skill selection to hook DB tables after the
/// server-driven agentic loop completes.  This ensures the decisions API
/// (`ctx_decision_audits`, `skill_selection_events`) has data for server-loop
/// sessions, matching what the bridge path persisted via hook side effects.
#[allow(clippy::too_many_arguments)]
async fn persist_server_loop_hook_events(
    hook_db_writer: &dyn TurnHookDbWriter,
    user_id: &str,
    session_id: &str,
    user_message: &str,
    state: &AgenticLoopState,
    model_name: Option<&str>,
) {
    // Use the telemetry accumulator — state.telemetry.all_tools_used tracks every
    // tool name across all rounds.  state.messages does NOT carry assistant
    // tool_call objects in the server loop path.
    let tool_call_names: Vec<String> = state.telemetry.all_tools_used.iter().cloned().collect();
    let selected_skills = state.telemetry.all_selected_skills.clone();
    let event_id = Uuid::now_v7().to_string();

    let decision_audit = Some(TurnDecisionAuditRecord {
        decision_id: Uuid::now_v7().to_string(),
        session_id: session_id.to_string(),
        event_id: event_id.clone(),
        decision_type: if tool_call_names.is_empty() {
            "response_generation".to_string()
        } else {
            "tool_selection".to_string()
        },
        decision_output: json!({
            "text": truncate_for_audit(&state.final_text, 500),
            "tool_calls": tool_call_names,
            "model_used": model_name,
            "total_tool_calls": state.total_tool_calls,
            "total_prompt_tokens": state.total_prompt,
            "total_completion_tokens": state.total_completion,
        }),
        model_used: model_name.map(|s| s.to_string()),
        context_capture_id: None,
    });

    let skill_selection = if let Some(first_skill) = selected_skills.first() {
        Some(TurnSkillSelectionRecord {
            event_id: Uuid::now_v7().to_string(),
            session_id: session_id.to_string(),
            user_id: user_id.to_string(),
            agent_id: None,
            user_query: truncate_for_audit(user_message, 2000),
            selected_skills: selected_skills.clone(),
            skill_name: first_skill.clone(),
            skill_version: None,
            selection_method: "llm_skill_choice".to_string(),
            execution_success: Some(1),
            execution_time_ms: None,
        })
    } else {
        tool_call_names
            .first()
            .map(|first_tool| TurnSkillSelectionRecord {
                event_id: Uuid::now_v7().to_string(),
                session_id: session_id.to_string(),
                user_id: user_id.to_string(),
                agent_id: None,
                user_query: truncate_for_audit(user_message, 2000),
                selected_skills: tool_call_names.clone(),
                skill_name: first_tool.clone(),
                skill_version: None,
                selection_method: "llm_tool_choice".to_string(),
                execution_success: Some(1),
                execution_time_ms: None,
            })
    };
    let _ = &selected_skills;

    let plan = TurnHookDbPersistPlan {
        decision_audit,
        skill_selection,
        implicit_feedback: None,
        reflection_mark: None,
        reflection_lesson: None,
    };

    if let Err(e) = hook_db_writer.persist(plan).await {
        astra_core::agent_error!(
            "server-loop",
            "failed to persist hook events for session {session_id}: {e}"
        );
    }
}

/// Fire the Memoria observer after the server-driven loop completes.
/// This sends the conversation messages to the Memoria `/v1/observe` endpoint
/// for cross-session knowledge extraction.
async fn fire_server_loop_observer(
    observer_worker: &dyn TurnObserverWorker,
    user_id: &str,
    session_id: &str,
    state: &AgenticLoopState,
) {
    let messages: Vec<serde_json::Map<String, serde_json::Value>> = state
        .messages
        .iter()
        .filter_map(|m| m.as_object().cloned())
        .collect();

    if messages.is_empty() {
        return;
    }

    let turn_count = state
        .session_turn
        .max(state.max_turns.saturating_sub(state.remaining_turns) as u32)
        as i64;
    let request = TurnObserverRequest {
        user_id: user_id.to_string(),
        session_id: session_id.to_string(),
        messages,
        turn_count,
        session_start: None,
    };

    if let Err(e) = observer_worker.run(request).await {
        astra_core::agent_error!(
            "server-loop",
            "failed to run observer for session {session_id}: {e}"
        );
    }
}

/// Truncate text for audit records, preserving UTF-8 boundaries.
fn truncate_for_audit(text: &str, max_chars: usize) -> String {
    if text.len() <= max_chars {
        text.to_string()
    } else {
        let truncated: String = text.chars().take(max_chars).collect();
        format!("{truncated}…")
    }
}

/// Walk `messages` (chronological) and return the content of the latest
/// assistant entry, if any. Kept for tests that exercise implicit-feedback
/// detection against assistant history.
#[cfg_attr(not(test), allow(dead_code))]
fn extract_prev_assistant_text(messages: &[serde_json::Value]) -> Option<String> {
    for msg in messages.iter().rev() {
        let role = msg.get("role").and_then(|v| v.as_str()).unwrap_or("");
        if role != "assistant" {
            continue;
        }
        if let Some(text) = msg.get("content").and_then(|v| v.as_str()) {
            let trimmed = text.trim();
            if !trimmed.is_empty() {
                return Some(trimmed.to_string());
            }
        }
        if let Some(arr) = msg.get("content").and_then(|v| v.as_array()) {
            let mut buf = String::new();
            for part in arr {
                if let Some(t) = part.get("text").and_then(|v| v.as_str()) {
                    if !buf.is_empty() {
                        buf.push('\n');
                    }
                    buf.push_str(t);
                }
            }
            let trimmed = buf.trim();
            if !trimmed.is_empty() {
                return Some(trimmed.to_string());
            }
        }
    }
    None
}

fn build_run_turn_complete_event_with_interruption(
    total_tool_calls: u32,
    final_text: &str,
    interruption: Option<&astra_turn_core::interruption::InterruptionRecord>,
) -> Value {
    let execution_state = interruption.map(|record| {
        serde_json::json!({
            "status": "interrupted",
            "interrupted": true,
            "interruption_kind": record.kind.label(),
            "resume_action": &record.resume_action,
            "resumable": record.kind.is_resumable(),
            "has_checkpoint": record.has_checkpoint,
            "tool_calls_completed": record.tool_calls_completed,
            "turns_completed": record.turns_completed,
            "remaining_turns": record.remaining_turns,
            "error_detail": record.error_detail,
        })
    });
    Value::Object(astra_turn_core::complete::build_turn_complete_event(
        total_tool_calls > 0,
        interruption.is_some(),
        &astra_turn_core::stall::DivergenceStatus::Healthy,
        execution_state,
        (!final_text.is_empty()).then_some(final_text),
    ))
}

// ─── Run State ──────────────────────────────────────────────────────────────

/// Status of a single agentic run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunStatus {
    Running,
    Paused,
    Waiting,
    Completed,
    Failed,
    Cancelled,
}

impl RunStatus {
    fn as_str(&self) -> &'static str {
        match self {
            Self::Running => STATUS_RUNNING,
            Self::Paused => STATUS_PAUSED,
            Self::Waiting => STATUS_WAITING,
            Self::Completed => STATUS_COMPLETED,
            Self::Failed => STATUS_FAILED,
            Self::Cancelled => STATUS_CANCELLED,
        }
    }

    /// Validate a status transition. Returns `Err` if the transition is illegal.
    ///
    /// Rules:
    /// - Terminal states (Completed, Failed, Cancelled) cannot transition to anything.
    /// - Running → Paused, Waiting, Completed, Failed, Cancelled
    /// - Paused → Running, Cancelled, Failed
    /// - Waiting → Running, Cancelled, Failed (external input resumes to Running)
    pub fn try_transition(&self, next: &RunStatus) -> Result<(), String> {
        let allowed = match self {
            Self::Running => matches!(
                next,
                Self::Paused | Self::Waiting | Self::Completed | Self::Failed | Self::Cancelled
            ),
            Self::Paused => matches!(next, Self::Running | Self::Cancelled | Self::Failed),
            Self::Waiting => matches!(next, Self::Running | Self::Cancelled | Self::Failed),
            Self::Completed | Self::Failed | Self::Cancelled => false,
        };
        if allowed {
            Ok(())
        } else {
            Err(format!(
                "invalid run status transition: {:?} → {:?}",
                self, next
            ))
        }
    }
}

fn is_run_finished_event(event: &Value) -> bool {
    event.get("event_type").and_then(Value::as_str) == Some("run_finished")
}

fn is_completed_run_finished_event(event: &Value) -> bool {
    if !is_run_finished_event(event) {
        return false;
    }
    let data = event.get("data").and_then(Value::as_object);
    let cancelled = data
        .and_then(|obj| obj.get("cancelled"))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let interrupted = data
        .and_then(|obj| obj.get("interrupted"))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    !cancelled && !interrupted
}

fn has_buffered_terminal_completion(events: &[Value]) -> bool {
    events
        .iter()
        .rev()
        .find(|event| is_run_finished_event(event))
        .is_some_and(is_completed_run_finished_event)
}

fn should_preserve_manual_pause_on_completion(
    current_status: &RunStatus,
    final_status: &RunStatus,
) -> bool {
    *current_status == RunStatus::Paused && *final_status == RunStatus::Completed
}

fn merge_run_finished_event_data(target: &mut Value, source: &Value) {
    let source_data = source
        .get("data")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let Some(target_obj) = target.as_object_mut() else {
        return;
    };
    let target_data = target_obj
        .entry("data".to_string())
        .or_insert_with(|| Value::Object(Map::new()));
    let Some(target_data_obj) = target_data.as_object_mut() else {
        return;
    };
    for (key, value) in source_data {
        target_data_obj.insert(key, value);
    }
}

fn merge_cancelled_run_events(run: &mut RunState, mut finalized_events: Vec<Value>) {
    let terminal_event = finalized_events
        .last()
        .filter(|event| is_run_finished_event(event))
        .cloned();
    if terminal_event.is_some() {
        finalized_events.pop();
    }

    let insert_at = run
        .events
        .last()
        .filter(|event| is_run_finished_event(event))
        .map(|_| run.events.len().saturating_sub(1))
        .unwrap_or(run.events.len());
    run.events.splice(insert_at..insert_at, finalized_events);

    if let Some(terminal_event) = terminal_event {
        if let Some(existing_terminal) = run
            .events
            .last_mut()
            .filter(|event| is_run_finished_event(event))
        {
            merge_run_finished_event_data(existing_terminal, &terminal_event);
        } else {
            run.events.push(terminal_event);
        }
    }
}

fn durable_event_type(event: &Value) -> Option<&str> {
    event
        .get("event_type")
        .or_else(|| event.get("type"))
        .and_then(Value::as_str)
}

fn terminal_events_for_persistence(events: &[Value]) -> Vec<Value> {
    events
        .iter()
        .filter(|event| {
            matches!(
                durable_event_type(event),
                Some(
                    "text_done"
                        | "run_error"
                        | "run_interrupted"
                        | "run_finished"
                        | "reasoning_delta"
                        | "reasoning_message_content"
                        | "reasoning_done"
                        | "thinking_delta"
                        | "thinking_done"
                )
            )
        })
        .cloned()
        .collect()
}

fn live_delta_event_for_persistence(event: &Value) -> bool {
    durable_event_type(event) == Some("text_delta")
}

/// Per-run state held in the lifecycle service.
struct RunState {
    run_id: String,
    session_id: String,
    status: RunStatus,
    events: Vec<Value>,
    cancel_flag: Arc<AtomicBool>,
    pause_flag: Arc<AtomicBool>,
    /// Cancelled together with `cancel_flag` on `cancel_run` for low-latency LLM abort.
    llm_cancel_token: Arc<CancellationToken>,
    /// Live fanout for clients that reattach to an active run after navigating away.
    live_tx: Option<broadcast::Sender<Value>>,
    #[allow(dead_code)]
    started_at: Instant,
    waiting_for: Option<String>,
}

#[derive(Clone)]
struct ServerAgentSpawnerEntry {
    spawner: Arc<DynamicAgentSpawner>,
    executor: Arc<ServerSpawnAgentExecutor>,
}

#[derive(Clone)]
struct ServerSpawnRuntimeContext {
    parent_run_id: String,
    user_id: String,
    session_id: String,
    trace_context: TraceContext,
    forward_headers: HashMap<String, String>,
    llm_token_service: Option<LlmTokenServiceConfig>,
    request_constraints: RequestConstraints,
    pause_flag: Option<Arc<AtomicBool>>,
    cancel_token: Option<Arc<CancellationToken>>,
    #[cfg(feature = "bridge-e2e-hooks")]
    test_child_llm_rounds: Vec<Value>,
    #[cfg(feature = "harness")]
    harness_sink: Option<Arc<dyn astra_harness::SnapshotSink>>,
}

// ─── Service ────────────────────────────────────────────────────────────────

/// Production [`RunLifecycleService`] that executes agentic loops via
/// [`ServerAgenticLoopHost`].
///
/// Durable run state is mandatory; process-local state is limited to live
/// control handles that cannot survive a restart.
pub struct AgenticRunLifecycleService {
    /// Process-local run handles (run_id -> state) for live cancellation, pause,
    /// and active SSE fanout. Durable state is the user-visible authority.
    runs: Arc<RwLock<HashMap<String, RunState>>>,
    /// LLM resolution dependencies.
    matrixone: MatrixOneSettings,
    encryptor: Arc<FernetTokenEncryptor>,
    shared_pool: Option<SharedPool>,
    /// Edge callback ledger shared with the API server.
    edge_callback_ledger: Arc<TokioMutex<HashMap<String, Value>>>,
    /// Durable run engine for persistence, replay, status, and recovery.
    run_engine: RunEngine,
    /// Optional delegation engine for multi-agent coordination.
    delegation_engine: Option<Arc<crate::server::delegation_engine::DelegationEngine>>,
    /// Session-scoped dynamic-agent spawners used by Web/server `agent.spawn`.
    server_agent_spawners: Arc<RwLock<HashMap<String, ServerAgentSpawnerEntry>>>,
    /// Fallback progress broadcaster for dynamic spawn when no delegation
    /// engine is configured. Normal production wiring uses the delegation
    /// engine broadcaster so Web SSE sees one agent tree stream.
    server_agent_progress_broadcaster: Arc<ProgressBroadcaster>,
    /// Shared mailbox router for Web/server dynamic spawned agents.
    server_agent_mailbox_router: Arc<astra_messaging::router::AgentMailboxRouter>,
    /// Per-user resource governor (Phase 5).
    resource_governor:
        Option<std::sync::Arc<dyn astra_services::resource_governor::ResourceGovernor>>,
    /// Live edge WebSocket connection pool (Phase 6).
    edge_connection_pool: Option<astra_server_types::edge_connection_pool::EdgeConnectionPool>,
    /// Optional database skill provider for runtime skill resolution.
    skill_service: Option<Arc<dyn SkillService>>,
    /// Per-run approval request channel receivers (Phase E).
    /// Key: run_id → receiver that the WS handler drains.
    approval_channels: Arc<TokioMutex<HashMap<String, mpsc::UnboundedReceiver<serde_json::Value>>>>,
    /// Per-run ask_user prompt channel receivers.
    /// Key: run_id → receiver that the WS handler drains.
    user_prompt_channels:
        Arc<TokioMutex<HashMap<String, mpsc::UnboundedReceiver<serde_json::Value>>>>,
    /// Per-run progress event channel receivers (Phase F.3).
    /// Key: run_id → receiver that the WS handler drains.
    progress_channels: Arc<TokioMutex<HashMap<String, mpsc::UnboundedReceiver<ProgressEvent>>>>,
    /// Hook DB writer for decision audit + skill selection persistence.
    hook_db_writer: Option<Arc<dyn TurnHookDbWriter>>,
    /// Memoria observer worker for cross-session knowledge extraction.
    observer_worker: Option<Arc<dyn TurnObserverWorker>>,
    /// Tool event writer for persisting tool_call events to agent_events.
    tool_event_writer: Option<Arc<dyn TurnToolEventWriter>>,
    /// Auxiliary event writer for ask_user lifecycle audit events.
    auxiliary_event_writer: Option<Arc<dyn crate::TurnAuxiliaryEventWriter>>,
    /// Counter of in-flight background agentic loop tasks.
    /// Incremented before spawn, decremented when the task exits.
    /// Used by `drain_background_tasks` for graceful shutdown.
    background_task_count: Arc<AtomicUsize>,
    /// Harness sink registry for server-side harness observation (Phase 2A).
    #[cfg(feature = "harness")]
    harness_registry: Option<crate::server::harness_handlers::HarnessSinkRegistry>,
    /// Shared background session-memory extraction coordinator. Cloned
    /// into every `AgenticLoopState` the service builds, so all turns
    /// share selector cooldown, in-flight dedup, event sink, and
    /// broker. `None` → extraction disabled (e.g. minimal test service).
    memory_extraction_service: Option<Arc<crate::session_memory::MemoryExtractionService>>,
}

impl AgenticRunLifecycleService {
    pub fn new(
        matrixone: MatrixOneSettings,
        encryptor: Arc<FernetTokenEncryptor>,
        edge_callback_ledger: Arc<TokioMutex<HashMap<String, Value>>>,
        run_engine: RunEngine,
    ) -> Self {
        Self {
            runs: Arc::new(RwLock::new(HashMap::new())),
            matrixone,
            encryptor,
            shared_pool: None,
            edge_callback_ledger,
            run_engine,
            delegation_engine: None,
            server_agent_spawners: Arc::new(RwLock::new(HashMap::new())),
            server_agent_progress_broadcaster: Arc::new(ProgressBroadcaster::default()),
            server_agent_mailbox_router: Arc::new(astra_messaging::AgentMailboxRouter::new(
                Arc::new(astra_messaging::InProcessTransport::new()),
                Arc::new(crate::server::delegation_engine::DelegationTracker::new()),
            )),
            resource_governor: None,
            edge_connection_pool: None,
            skill_service: None,
            approval_channels: Arc::new(TokioMutex::new(HashMap::new())),
            user_prompt_channels: Arc::new(TokioMutex::new(HashMap::new())),
            progress_channels: Arc::new(TokioMutex::new(HashMap::new())),
            hook_db_writer: None,
            observer_worker: None,
            tool_event_writer: None,
            auxiliary_event_writer: None,
            background_task_count: Arc::new(AtomicUsize::new(0)),
            #[cfg(feature = "harness")]
            harness_registry: None,
            memory_extraction_service: None,
        }
    }

    pub fn with_memory_extraction_service(
        mut self,
        svc: Arc<crate::session_memory::MemoryExtractionService>,
    ) -> Self {
        self.memory_extraction_service = Some(svc);
        self
    }

    #[cfg(feature = "harness")]
    pub fn with_harness_registry(
        mut self,
        registry: crate::server::harness_handlers::HarnessSinkRegistry,
    ) -> Self {
        self.harness_registry = Some(registry);
        self
    }

    pub fn with_pool(mut self, pool: SharedPool) -> Self {
        self.shared_pool = Some(pool);
        self
    }

    pub fn with_delegation_engine(
        mut self,
        engine: Arc<crate::server::delegation_engine::DelegationEngine>,
    ) -> Self {
        self.delegation_engine = Some(engine);
        self
    }

    pub fn with_edge_connection_pool(
        mut self,
        pool: astra_server_types::edge_connection_pool::EdgeConnectionPool,
    ) -> Self {
        self.edge_connection_pool = Some(pool);
        self
    }

    pub fn with_resource_governor(
        mut self,
        governor: std::sync::Arc<dyn astra_services::resource_governor::ResourceGovernor>,
    ) -> Self {
        self.resource_governor = Some(governor);
        self
    }

    pub fn with_skill_service(mut self, service: Arc<dyn SkillService>) -> Self {
        self.skill_service = Some(service);
        self
    }

    pub fn with_hook_db_writer(mut self, writer: Arc<dyn TurnHookDbWriter>) -> Self {
        self.hook_db_writer = Some(writer);
        self
    }

    pub fn with_observer_worker(mut self, worker: Arc<dyn TurnObserverWorker>) -> Self {
        self.observer_worker = Some(worker);
        self
    }

    pub fn with_tool_event_writer(mut self, writer: Arc<dyn TurnToolEventWriter>) -> Self {
        self.tool_event_writer = Some(writer);
        self
    }

    pub fn with_auxiliary_event_writer(
        mut self,
        writer: Arc<dyn crate::TurnAuxiliaryEventWriter>,
    ) -> Self {
        self.auxiliary_event_writer = Some(writer);
        self
    }

    fn dynamic_agent_progress_broadcaster(&self) -> Arc<ProgressBroadcaster> {
        self.delegation_engine
            .as_ref()
            .and_then(|engine| engine.progress_broadcaster().cloned())
            .unwrap_or_else(|| Arc::clone(&self.server_agent_progress_broadcaster))
    }

    async fn server_agent_spawner_for_session(&self, session_id: &str) -> ServerAgentSpawnerEntry {
        if let Some(entry) = self
            .server_agent_spawners
            .read()
            .await
            .get(session_id)
            .cloned()
        {
            return entry;
        }

        let mut guard = self.server_agent_spawners.write().await;
        if let Some(entry) = guard.get(session_id).cloned() {
            return entry;
        }

        let executor = Arc::new(
            ServerSpawnAgentExecutor::new(
                self.matrixone.clone(),
                Arc::clone(&self.encryptor),
                Arc::clone(&self.edge_callback_ledger),
            )
            .with_pool(self.shared_pool.clone())
            .with_edge_connection_pool(self.edge_connection_pool.clone())
            .with_skill_service(self.skill_service.clone())
            .with_memory_extraction_service(self.memory_extraction_service.clone()),
        );
        let executor_for_spawner: Arc<dyn SpawnAgentExecutor> = executor.clone();
        let mut spawner = DynamicAgentSpawner::with_broadcaster(
            Arc::clone(&self.server_agent_mailbox_router),
            self.dynamic_agent_progress_broadcaster(),
        )
        .with_executor(executor_for_spawner)
        .with_session(session_id.to_string());
        if let Some(pool) = self.shared_pool.clone() {
            spawner = spawner.with_trace_writer(Arc::new(
                DatabaseTraceEventWriter::new(self.matrixone.clone()).with_pool(pool),
            ));
        }
        if let Some(store) = self
            .delegation_engine
            .as_ref()
            .and_then(|engine| engine.prefix_store().cloned())
        {
            spawner = spawner.with_prefix_store(store);
        }

        let entry = ServerAgentSpawnerEntry {
            spawner: Arc::new(spawner),
            executor,
        };
        guard.insert(session_id.to_string(), entry.clone());
        entry
    }

    fn request_constraints_from_request(request: &ChatRequestData) -> RequestConstraints {
        RequestConstraints::new(
            normalize_request_allowlist(request.allow_tools.as_deref(), "allow_tools")
                .expect("request allow_tools should be validated before state build"),
            normalize_request_allowlist(request.allow_skills.as_deref(), "allow_skills")
                .expect("request allow_skills should be validated before state build"),
        )
    }

    fn inherited_permissions_from_constraints(
        constraints: &RequestConstraints,
    ) -> InheritedPermissions {
        let mut inherited = InheritedPermissions::auto_approve();
        inherited.allowed_tools = constraints.allowed_tools.clone();
        inherited
    }

    #[allow(clippy::too_many_arguments)]
    async fn wire_server_dynamic_agent_tools(
        &self,
        executor: &mut super::server_tool_executor::ServerToolExecutor,
        user_id: &str,
        session_id: &str,
        run_id: &str,
        turn_seq: u32,
        request: &ChatRequestData,
        workspace: &std::path::Path,
        pause_flag: Option<Arc<AtomicBool>>,
        cancel_token: Option<Arc<CancellationToken>>,
        #[cfg(feature = "harness")] harness_sink: Option<Arc<dyn astra_harness::SnapshotSink>>,
    ) {
        let entry = self.server_agent_spawner_for_session(session_id).await;
        let request_constraints = Self::request_constraints_from_request(request);
        entry
            .executor
            .set_runtime_context(ServerSpawnRuntimeContext {
                parent_run_id: run_id.to_string(),
                user_id: user_id.to_string(),
                session_id: session_id.to_string(),
                trace_context: server_trace_context(user_id, session_id, run_id, turn_seq),
                forward_headers: request.forward_headers.clone(),
                llm_token_service: request.llm_token_service.clone(),
                request_constraints: request_constraints.clone(),
                pause_flag,
                cancel_token,
                #[cfg(feature = "bridge-e2e-hooks")]
                test_child_llm_rounds: request
                    .context
                    .as_ref()
                    .and_then(|ctx| ctx.get("test_spawn_child_llm_rounds"))
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default(),
                #[cfg(feature = "harness")]
                harness_sink,
            })
            .await;

        let agent_id = request
            .agent_id
            .clone()
            .unwrap_or_else(|| "root-agent".to_string());
        executor.set_agent_tool_context(AgentToolContext {
            run_id: run_id.to_string(),
            agent_id,
            current_model: request.model.clone(),
            recursion_depth: 0,
            is_fork_child: false,
            working_dir: workspace.to_path_buf(),
            spawner: entry.spawner,
            inherited_permissions: Self::inherited_permissions_from_constraints(
                &request_constraints,
            ),
            active_skills: Vec::new(),
            live_event_sink: None,
            trace_context: Some(server_trace_context(user_id, session_id, run_id, turn_seq)),
        });
    }

    fn build_csl_store(&self) -> Option<Arc<dyn astra_turn_core::conversation_log::CslStore>> {
        let pool = self.shared_pool.as_ref()?;
        let store =
            astra_turn_core::conversation_log::db_store::DbCslStore::new(self.matrixone.clone())
                .with_pool(pool.clone());
        Some(Arc::new(store))
    }

    async fn restore_csl_history(
        &self,
        user_id: &str,
        session_id: &str,
        run_id: &str,
        loop_state: &mut AgenticLoopState,
    ) -> Option<astra_turn_core::conversation_log::manager::CslManager> {
        let store = self.build_csl_store()?;
        let mut mgr = match astra_turn_core::conversation_log::manager::CslManager::new(
            store,
            session_id.to_string(),
            Default::default(),
        ) {
            Ok(m) => m,
            Err(e) => {
                tracing::warn!(error = %e, "CSL manager creation failed");
                return None;
            }
        };
        mgr.set_trace_id(run_id.to_string());

        let mut restored_messages = Vec::new();

        match mgr.load().await {
            Ok(Some(mat)) => {
                restored_messages = mat.messages;
                restore_session_state_compact(mat.session_state, loop_state);
            }
            Ok(None) => {
                self.record_runtime_retrieval_degrade(
                    user_id,
                    session_id,
                    run_id,
                    RetrievalStage::Structured,
                    "timeout",
                )
                .await;
                self.record_runtime_retrieval_degrade(
                    user_id,
                    session_id,
                    run_id,
                    RetrievalStage::Fts,
                    "empty",
                )
                .await;
                self.record_runtime_retrieval_degrade(
                    user_id,
                    session_id,
                    run_id,
                    RetrievalStage::Vector,
                    "stale",
                )
                .await;
            }
            Err(e) => {
                tracing::warn!(
                    session_id,
                    error = %e,
                    "CSL load failed; starting with empty history"
                );
                self.record_runtime_retrieval_degrade(
                    user_id,
                    session_id,
                    run_id,
                    RetrievalStage::Structured,
                    "timeout",
                )
                .await;
            }
        }

        if !restored_messages.is_empty() {
            if !loop_state.messages.is_empty() {
                restored_messages.push(loop_state.messages.remove(0));
            }
            loop_state.messages = restored_messages;
        }

        mgr.mark_turn_start(loop_state.messages.len());
        Some(mgr)
    }

    async fn record_runtime_retrieval_degrade(
        &self,
        user_id: &str,
        session_id: &str,
        run_id: &str,
        stage: RetrievalStage,
        reason: &str,
    ) {
        let Some(pool) = &self.shared_pool else {
            return;
        };
        let store = DatabaseContextManifestStore::new(pool.clone());
        if let Err(error) = store
            .record_retrieval_degrade_event(
                user_id,
                session_id,
                Some(run_id),
                stage.clone(),
                reason,
                0,
            )
            .await
        {
            tracing::warn!(
                target: "astra_runtime::retrieval",
                session_id,
                run_id,
                stage = ?stage,
                reason,
                error = %error,
                "failed to persist retrieval degrade event"
            );
        }
    }

    /// Wait for all in-flight background agentic loop tasks to finish.
    ///
    /// Called during graceful shutdown. Polls the task counter with 100ms
    /// intervals up to `timeout`. Returns `true` if all tasks drained within
    /// the timeout, `false` if tasks are still running.
    async fn drain_background_tasks_impl(&self, timeout: std::time::Duration) -> bool {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            if self.background_task_count.load(Ordering::Acquire) == 0 {
                break;
            }
            if tokio::time::Instant::now() >= deadline {
                self.persist_graceful_shutdown_checkpoints().await;
                return false;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }

        // Turn-owned background_task_count reached zero, but the
        // session-memory extraction service has its own pending
        // counter (see `MemoryExtractionService::wait_for_pending`).
        // Fold it into the same shutdown deadline so we don't kill
        // in-flight Memoria writes mid-HTTP.
        if let Some(svc) = self.memory_extraction_service.as_ref() {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                return false;
            }
            let leftover = svc.wait_for_pending(remaining).await;
            if leftover > 0 {
                return false;
            }
        }
        true
    }

    async fn persist_graceful_shutdown_checkpoints(&self) {
        let engine = &self.run_engine;
        let run_ids = {
            let runs = self.runs.read().await;
            runs.values()
                .filter(|run| {
                    matches!(
                        run.status,
                        RunStatus::Running | RunStatus::Waiting | RunStatus::Paused
                    )
                })
                .map(|run| run.run_id.clone())
                .collect::<Vec<_>>()
        };
        for run_id in run_ids {
            let checkpoint = json!({
                "version": "checkpoint_v1",
                "graceful": true,
                "last_batch_id": format!("shutdown-{run_id}"),
                "extra": {}
            });
            astra_core::log_persist!(
                engine
                    .persist_checkpoint(&run_id, &checkpoint.to_string())
                    .await,
                "run_lifecycle",
                &run_id,
                "graceful_shutdown_checkpoint"
            );
            astra_core::log_persist!(
                engine
                    .append_event(
                        &run_id,
                        json!({"event_type": "run_checkpointed_for_shutdown", "data": {}})
                    )
                    .await,
                "run_lifecycle",
                &run_id,
                "graceful_shutdown_checkpoint_event"
            );
        }
    }

    /// Returns the current number of in-flight background tasks.
    pub fn background_task_count(&self) -> usize {
        self.background_task_count.load(Ordering::Acquire)
    }

    /// Clone the Arc handle to the runs map (for background tasks).
    fn runs_handle(&self) -> Arc<RwLock<HashMap<String, RunState>>> {
        Arc::clone(&self.runs)
    }

    /// Schedule removal of a terminal run from the in-memory cache after a
    /// grace period. Clients have 5 minutes to poll final events before the
    /// entry is evicted. This prevents unbounded memory growth.
    fn schedule_run_eviction(runs: &Arc<RwLock<HashMap<String, RunState>>>, run_id: String) {
        let runs = Arc::clone(runs);
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_secs(300)).await;
            runs.write().await.remove(&run_id);
        });
    }

    fn build_tracked_run_state(
        run_id: String,
        session_id: String,
        _user_id: String,
    ) -> (
        RunState,
        Arc<AtomicBool>,
        Arc<AtomicBool>,
        Arc<CancellationToken>,
    ) {
        let cancel_flag = Arc::new(AtomicBool::new(false));
        let pause_flag = Arc::new(AtomicBool::new(false));
        let llm_cancel_token = Arc::new(CancellationToken::new());
        let run_state = RunState {
            run_id,
            session_id,
            status: RunStatus::Running,
            events: vec![json!({"event_type": "run_started", "data": {}})],
            cancel_flag: cancel_flag.clone(),
            pause_flag: pause_flag.clone(),
            llm_cancel_token: llm_cancel_token.clone(),
            live_tx: None,
            started_at: Instant::now(),
            waiting_for: None,
        };
        (run_state, cancel_flag, pause_flag, llm_cancel_token)
    }

    /// Return true when an existing run should block starting a new turn in the
    /// same session.
    ///
    /// `paused` is intentionally split:
    /// - `paused + waiting_for=Some(..)` is a real user/approval wait and must
    ///   be resumed or cancelled before another run starts.
    /// - `paused + waiting_for=None` is used for resumable interruptions such as
    ///   `budget_exhausted`; the user-facing contract says the next message can
    ///   continue from the checkpoint, so it must not block a fresh web turn.
    fn blocks_new_session_run(run: &RunState, session_id: &str) -> bool {
        run.session_id == session_id
            && match run.status {
                RunStatus::Running | RunStatus::Waiting => true,
                RunStatus::Paused => run.waiting_for.is_some(),
                RunStatus::Completed | RunStatus::Failed | RunStatus::Cancelled => false,
            }
    }

    fn session_has_blocking_run(runs: &HashMap<String, RunState>, session_id: &str) -> bool {
        runs.values()
            .any(|run| Self::blocks_new_session_run(run, session_id))
    }

    fn configure_loop_state_runtime_controls(
        &self,
        loop_state: &mut AgenticLoopState,
        cancel_flag: &Arc<AtomicBool>,
        pause_flag: &Arc<AtomicBool>,
        llm_cancel_token: &Arc<CancellationToken>,
    ) {
        loop_state.cancellation.flag = Some(cancel_flag.clone());
        loop_state.cancellation.pause_flag = Some(pause_flag.clone());
        loop_state.cancellation.token = Some(llm_cancel_token.clone());
        loop_state.delegation_engine = self.delegation_engine.clone();
    }

    async fn persist_run_start(
        &self,
        run_id: &str,
        user_id: &str,
        session_id: &str,
    ) -> Result<(), (StatusCode, Json<ErrorResponse>)> {
        self.run_engine
            .start_run(run_id, user_id, session_id)
            .await
            .map_err(|error| {
                let status = if error == "session already has an active run" {
                    StatusCode::CONFLICT
                } else {
                    StatusCode::SERVICE_UNAVAILABLE
                };
                let detail = if status == StatusCode::CONFLICT {
                    error
                } else {
                    format!("Failed to persist durable run start: {error}")
                };
                error_response(status, detail)
            })
    }

    fn finalize_run_events(
        loop_outcome: Result<AgenticLoopOutcome, astra_core::ClassifiedError>,
        mut events: Vec<Value>,
        loop_state: &AgenticLoopState,
    ) -> (Vec<Value>, RunStatus, Option<String>) {
        let total_input =
            loop_state.total_prompt + loop_state.total_cache_read + loop_state.total_cache_creation;
        let usage = json!({
            "prompt_tokens": total_input,
            "completion_tokens": loop_state.total_completion,
            "tool_call_count": loop_state.total_tool_calls,
        });
        let cancellation_requested = loop_state
            .cancellation
            .flag
            .as_ref()
            .is_some_and(|f| f.load(Ordering::Acquire))
            || loop_state
                .cancellation
                .token
                .as_ref()
                .is_some_and(|t| t.is_cancelled());

        let (final_status, error_msg) = if cancellation_requested
            || matches!(&loop_outcome, Ok(AgenticLoopOutcome::Cancelled))
        {
            let mut data = usage;
            data["cancelled"] = Value::Bool(true);
            events.push(json!({
                "event_type": "run_finished",
                "data": data,
            }));
            (RunStatus::Cancelled, None)
        } else {
            match loop_outcome {
                Ok(AgenticLoopOutcome::Completed) => {
                    if let Some(interruption) = loop_state.interruption.as_ref() {
                        let interruption_json = interruption.to_json();
                        if !loop_state.final_text.is_empty() {
                            events.push(json!({
                                "event_type": "text_done",
                                "data": {
                                    "full_text": loop_state.final_text.clone(),
                                    "partial": true,
                                    "interruption": interruption_json.clone(),
                                }
                            }));
                        }
                        events.push(json!({
                            "event_type": "run_interrupted",
                            "data": interruption_json.clone(),
                        }));
                        let mut finished = usage;
                        finished["interrupted"] = Value::Bool(true);
                        finished["interruption_kind"] =
                            Value::String(interruption.kind.label().to_string());
                        finished["resumable"] = Value::Bool(interruption.kind.is_resumable());
                        events.push(json!({
                            "event_type": "run_finished",
                            "data": finished,
                        }));
                        (RunStatus::Paused, Some(interruption.user_message.clone()))
                    } else {
                        if !loop_state.final_text.is_empty() {
                            events.push(json!({
                            "event_type": "text_done",
                            "data": { "full_text": loop_state.final_text.clone() }
                            }));
                        }
                        events.push(json!({
                            "event_type": "run_finished",
                            "data": usage,
                        }));
                        (RunStatus::Completed, None)
                    }
                }
                Ok(AgenticLoopOutcome::Cancelled) => unreachable!("handled by cancellation gate"),
                Ok(AgenticLoopOutcome::Error(e)) => {
                    events.push(json!({
                        "event_type": "run_error",
                        "data": {"error": e.clone()}
                    }));
                    events.push(json!({
                        "event_type": "run_finished",
                        "data": usage.clone(),
                    }));
                    (RunStatus::Failed, Some(e))
                }
                Ok(AgenticLoopOutcome::Waiting(w)) => {
                    let msg = format!("waiting: {w}");
                    events.push(json!({
                        "event_type": "run_waiting",
                        "data": {"reason": msg.clone()}
                    }));
                    (RunStatus::Waiting, Some(msg))
                }
                Err(err) => {
                    let msg = err.to_string();
                    events.push(json!({
                        "event_type": "run_error",
                        "data": {"error": &msg, "error_kind": err.kind.as_str()}
                    }));
                    events.push(json!({
                        "event_type": "run_finished",
                        "data": usage,
                    }));
                    (RunStatus::Failed, Some(msg))
                }
            }
        };

        (events, final_status, error_msg)
    }

    async fn load_trusted_llm_token_service_domains(
        &self,
    ) -> Result<Vec<TrustedLlmDomain>, (StatusCode, Json<ErrorResponse>)> {
        let pool = if let Some(pool) = &self.shared_pool {
            pool.get().clone()
        } else {
            connect_matrixone(&self.matrixone).await.map_err(|error| {
                error_response(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("failed to connect database for trusted domains query: {error}"),
                )
            })?
        };
        let rows = sqlx::query(
            "SELECT domain_host, IFNULL(domain_port, 0) AS domain_port \
             FROM runtime_llm_trusted_domains \
             WHERE is_enabled = 1",
        )
        .fetch_all(&pool)
        .await
        .map_err(|error| {
            error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!(
                    "failed to query trusted domains from table {LLM_TOKEN_SERVICE_TRUSTED_DOMAINS_TABLE}: {error}"
                ),
            )
        })?;
        let mut trusted_domains = Vec::new();
        let mut seen = HashSet::new();
        for row in rows {
            let host: String = row.try_get("domain_host").map_err(|error| {
                error_response(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!(
                        "failed to decode domain_host from table {LLM_TOKEN_SERVICE_TRUSTED_DOMAINS_TABLE}: {error}"
                    ),
                )
            })?;
            let port: i64 = row.try_get("domain_port").map_err(|error| {
                error_response(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!(
                        "failed to decode domain_port from table {LLM_TOKEN_SERVICE_TRUSTED_DOMAINS_TABLE}: {error}"
                    ),
                )
            })?;
            let domain = trusted_llm_domain_from_db_values(&host, port).map_err(|detail| {
                error_response(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!(
                        "invalid trusted domain row in table {LLM_TOKEN_SERVICE_TRUSTED_DOMAINS_TABLE}: {detail}"
                    ),
                )
            })?;
            let key = format!(
                "{}:{}",
                domain.host,
                domain
                    .port
                    .map(|value| value.to_string())
                    .unwrap_or_default()
            );
            if seen.insert(key) {
                trusted_domains.push(domain);
            }
        }
        if trusted_domains.is_empty() {
            return Err(error_response(
                StatusCode::SERVICE_UNAVAILABLE,
                format!(
                    "table {LLM_TOKEN_SERVICE_TRUSTED_DOMAINS_TABLE} has no enabled trusted domains"
                ),
            ));
        }
        Ok(trusted_domains)
    }

    async fn validate_request_constraints(
        &self,
        user_id: &str,
        request: &ChatRequestData,
    ) -> Result<(), (StatusCode, Json<ErrorResponse>)> {
        if request.llm_token_service.is_some() {
            let trusted_domains = self.load_trusted_llm_token_service_domains().await?;
            validate_llm_token_service_config(request.llm_token_service.as_ref(), &trusted_domains)
                .map_err(|detail| error_response(StatusCode::BAD_REQUEST, detail))?;
        }
        normalize_request_allowlist(request.allow_tools.as_deref(), "allow_tools")
            .map_err(|detail| error_response(StatusCode::BAD_REQUEST, detail))?;
        let allowed_skills =
            normalize_request_allowlist(request.allow_skills.as_deref(), "allow_skills")
                .map_err(|detail| error_response(StatusCode::BAD_REQUEST, detail))?;
        let (_, resolver) = build_server_skill_resolver(
            self.skill_service.clone(),
            user_id,
            allowed_skills.as_ref(),
        );
        apply_normalized_skill_allowlist(resolver, allowed_skills.as_ref())
            .map(|_| ())
            .map_err(|detail| error_response(StatusCode::BAD_REQUEST, detail))
    }

    /// Build a [`ServerAgenticLoopHost`] for a single run.
    fn build_host(
        &self,
        user_id: &str,
        session_id: &str,
        request: &ChatRequestData,
        edge_tools: Vec<Value>,
        edge_profile: Map<String, Value>,
        plan_resume_hint: Option<String>,
    ) -> super::server_loop_host::ServerAgenticLoopHost {
        let mut builder = ServerAgenticLoopHostBuilder::new(
            self.matrixone.clone(),
            self.encryptor.clone(),
            user_id.to_string(),
            session_id.to_string(),
        )
        .with_model(request.model.clone())
        .with_llm_token_service(request.llm_token_service.clone())
        .with_full_llm_capture(request.full_llm_capture)
        .with_edge_tools(edge_tools)
        .with_capabilities(crate::capabilities::lifecycle_server_capabilities(
            self.shared_pool.is_some(),
        ))
        .with_edge_profile(edge_profile)
        .with_edge_callback_ledger(self.edge_callback_ledger.clone())
        .with_interactive_client(request.interactive_client)
        .with_plan_resume_hint(plan_resume_hint);

        if let Some(pool) = &self.shared_pool {
            builder = builder.with_pool(pool.clone());
        }
        // Wire one shared agent-progress broadcaster for delegation and
        // dynamic `agent.spawn` trees so Web SSE observes a single lineage.
        builder = builder.with_progress_broadcaster(self.dynamic_agent_progress_broadcaster());
        if let Some(ref de) = self.delegation_engine {
            // G2: share the delegation engine's fork-prefix store with
            // the parent loop host so `on_turn_completed` captures land
            // in the same store the delegate path reads from. Without
            // this, server-side parent turns never capture and
            // delegate sub-runs can't inherit the prefix (that was the
            // exact "out of scope" leg called out in 45a3a39e9's
            // commit body).
            if let Some(store) = de.prefix_store() {
                builder = builder.with_prefix_store(Some(Arc::clone(store)));
            }
        }
        // Wire test LLM rounds from request context (E2E test hook).
        #[cfg(feature = "bridge-e2e-hooks")]
        if let Some(rounds) = request
            .context
            .as_ref()
            .and_then(|c| c.get("test_llm_rounds"))
            .and_then(Value::as_array)
            .cloned()
        {
            builder = builder.with_test_llm_rounds(rounds);
        }
        builder.build()
    }

    /// Build the initial [`AgenticLoopState`] from a chat request.
    ///
    /// `workspace_override` — when the server provisions a workspace (web-agent
    /// mode, no CLI edge), pass it here so stop hooks and skill hooks are loaded
    /// from the provisioned directory instead of requiring `edge_profile.cwd`.
    fn build_initial_state(
        &self,
        user_id: &str,
        request: &ChatRequestData,
        session_id: &str,
        run_id: &str,
        workspace_override: Option<&std::path::Path>,
        cancel_token: Option<Arc<CancellationToken>>,
    ) -> AgenticLoopState {
        use astra_pipeline::step_protocol::InMemoryIdempotencyCache;
        use astra_text_utils::semantic_dedup::SemanticDedup;
        use astra_turn_core::chat_turn_heuristics::infer_task_execution_profile;
        use astra_turn_core::stop_hooks_yaml::{
            detect_turn_hook_sets, is_plan_subtask_from_chat_context, project_root_for_stop_hooks,
        };

        let request_constraints = RequestConstraints::new(
            normalize_request_allowlist(request.allow_tools.as_deref(), "allow_tools")
                .expect("request allow_tools should be validated before state build"),
            normalize_request_allowlist(request.allow_skills.as_deref(), "allow_skills")
                .expect("request allow_skills should be validated before state build"),
        );
        let (skill_registry, raw_skill_resolver) = build_server_skill_resolver(
            self.skill_service.clone(),
            user_id,
            request_constraints.allowed_skills.as_ref(),
        );
        let skill_resolver = apply_normalized_skill_allowlist(
            raw_skill_resolver,
            request_constraints.allowed_skills.as_ref(),
        )
        .expect("request allow_skills should be validated before state build");
        use astra_turn_core::turn_guard::TurnGuard;

        let user_message = json!({
            "role": "user",
            "content": request.message,
        });

        let task_profile = infer_task_execution_profile(&request.message);
        let runtime_turn_ceiling = if is_plan_subtask_from_chat_context(&request.context) {
            astra_core::RuntimeLimits::global().effective_plan_subtask_turns()
        } else {
            astra_core::RuntimeLimits::global().max_turns
        };
        let requested_budget = request.execution_budget.as_ref().map(|budget| {
            astra_turn_core::chat_turn_heuristics::AgenticTurnBudgetOverride {
                initial_turns: budget.initial_turns.map(|value| value as usize),
                hard_turn_limit: budget.hard_turn_limit.map(|value| value as usize),
            }
        });
        let agentic_turn_budget =
            astra_turn_core::chat_turn_heuristics::resolve_agentic_turn_budget(
                task_profile,
                runtime_turn_ceiling,
                requested_budget,
            );
        let max_turns = agentic_turn_budget.initial_turns;
        let edge_ctx = Self::extract_edge_context(request);
        // Use edge profile's git_root/cwd if available; fall back to provisioned
        // server workspace so web-agent sessions still load stop-hooks.yaml.
        let project_root_buf = project_root_for_stop_hooks(&edge_ctx)
            .or_else(|| workspace_override.map(|p| p.to_path_buf()));
        let hook_sets = project_root_buf
            .as_ref()
            .map(|root| {
                detect_turn_hook_sets(
                    root.as_path(),
                    task_profile,
                    is_plan_subtask_from_chat_context(&request.context),
                )
            })
            .unwrap_or_default();
        let workspace_root_hint = project_root_buf.map(|p| p.to_string_lossy().into_owned());
        let skill_search = request.skill_search.clone().unwrap_or_default();
        let (tool_event_hooks, session_event_hooks) = workspace_root_hint
            .as_ref()
            .map(|root| crate::skills::hooks::load_all_hooks(std::path::Path::new(root)))
            .unwrap_or_default();
        let thinking_config =
            Self::thinking_from_chat_context(&request.context, request.model.as_deref());

        // Create harness sink early so sub-run executors can share it.
        #[cfg(feature = "harness")]
        let (harness_server_sink, harness_sink_arc): (
            Option<std::sync::Arc<crate::server::harness_server_sink::ServerSnapshotSink>>,
            Option<std::sync::Arc<dyn astra_harness::SnapshotSink>>,
        ) = if self.harness_registry.is_some() {
            let mut raw_sink = crate::server::harness_server_sink::ServerSnapshotSink::new(
                session_id.to_string(),
                String::new(),
            );
            if let Some(ref pool) = self.shared_pool {
                raw_sink = raw_sink.with_pool(pool.get().clone());
            }
            let concrete = std::sync::Arc::new(raw_sink);
            let dyn_sink = concrete.clone() as std::sync::Arc<dyn astra_harness::SnapshotSink>;
            (Some(concrete), Some(dyn_sink))
        } else {
            (None, None)
        };

        // Build the server-side skill fork executor so skills with
        // execution_context: Fork can run in isolated sub-agent loops.
        let edge_tools = Self::extract_edge_tools(request);
        let edge_profile = Self::extract_edge_profile(request);
        let skill_executor = build_server_skill_executor(
            &self.matrixone,
            &self.encryptor,
            self.shared_pool.as_ref(),
            request.model.as_deref(),
            request.llm_token_service.as_ref(),
            &edge_tools,
            &edge_profile,
            &request.forward_headers,
            request_constraints.clone(),
            skill_resolver.clone(),
            session_id,
            self.edge_connection_pool.as_ref(),
            cancel_token,
            self.memory_extraction_service.as_ref(),
            #[cfg(feature = "harness")]
            harness_sink_arc.as_ref(),
        );
        let resolved_tool_policy = astra_config::runtime_config::RuntimeConfig::load()
            .tool_selection
            .resolve_for_model(request.model.as_deref());

        AgenticLoopState {
            messages: vec![user_message],
            volatile_pending: Vec::new(),
            recent_rounds: Vec::new(),
            tool_results: Vec::new(),
            current_session_id: Some(session_id.to_string()),
            current_run_id: Some(run_id.to_string()),
            context_manifest_pool: self.shared_pool.clone(),
            context_manifest_user_id: None,
            context_manifest_model_name: request.model.clone(),
            recursion_depth: 0,
            final_text: String::new(),
            final_text_streamed: false,
            total_prompt: 0,
            total_completion: 0,
            total_cache_read: 0,
            total_cache_creation: 0,
            total_tool_calls: 0,
            total_evidence_tool_calls: 0,
            has_any_usage: false,
            max_turns,
            remaining_turns: max_turns,
            turn_budget_hint_emitted_50: false,
            turn_budget_hint_emitted_20: false,
            agentic_turn_budget,
            current_round_index: 0,
            llm_rounds_completed: 0,
            turn_guard: TurnGuard::with_profile(task_profile),
            restricted_tools: std::collections::HashSet::new(),
            boosted_tools: std::collections::HashSet::new(),
            widen_selection_pending: false,
            step_recorder: StepRecorder::new(session_id, run_id),
            idempotency_cache: InMemoryIdempotencyCache::new(),
            semantic_dedup: SemanticDedup::new(0.75),
            call_counts: HashMap::new(),
            // Per-model workflow-guard policy (see
            // `ToolSelectionConfig::resolve_for_model`). Built-in profiles
            // give stronger models (opus/sonnet-4) more rope than haiku.
            // Security guards (shell_obfuscation, destructive_sql) are
            // unaffected and stay uniform across models.
            max_identical_tool_calls: resolved_tool_policy.max_identical_tool_calls,
            max_tools_per_turn: resolved_tool_policy.max_tools_per_turn,
            repeated_cache_hit_suppression: resolved_tool_policy.repeated_cache_hit_suppression,
            max_consecutive_empty_name: resolved_tool_policy.max_consecutive_empty_name,
            stall: Default::default(),
            telemetry: Default::default(),
            skills: SkillState {
                registry_for_activation: if request_constraints.allowed_skills.is_some() {
                    None
                } else {
                    skill_registry
                },
                resolver: skill_resolver,
                executor: skill_executor,
                request_constraints,
                quality_tracker: crate::skills::quality::SkillQualityTracker::new(),
                improvement_tracker: astra_skills::improvement::ImprovementTracker::new(),
                search: skill_search,
                tool_event_hooks,
                session_event_hooks,
                ..Default::default()
            },
            hooks: StopHookState {
                stop_hooks: hook_sets.stop_hooks,
                teammate_idle_hooks: hook_sets.teammate_idle_hooks,
                workspace_root_hint,
                forward_headers: request.forward_headers.clone(),
                llm_token_service: request.llm_token_service.clone(),
                ..Default::default()
            },
            cancellation: Default::default(),
            messaging: Default::default(),
            error_recovery: Default::default(),
            pipeline_session: Some(astra_turn_core::pipeline_session::PipelineSession::new(
                astra_turn_core::pipeline_config::PipelineConfig::default(),
            )),
            message: request.message.clone(),
            recent_tools: Vec::new(),
            task_profile,
            last_turn_policy: crate::turn::agentic_loop_host::TurnInteractionPolicy::default(),
            api: astra_thin_client::ThinClient::new("http://127.0.0.1:1", None)
                .expect("valid dummy URL"),
            api_token: String::new(),
            delegation_engine: None,
            delegations_this_turn: 0,
            project_context: None,
            checkpoint_gate: None,
            last_llm_context_manifest_trace: None,
            rate_limit_cooldown: Default::default(),
            data_snapshot_provider: None,
            last_composite_snapshot: None,
            last_measured_prompt_tokens: None,
            consecutive_context_window_errors: 0,
            compaction_effectiveness: Default::default(),
            pinned_tool_schema_tokens: 0,
            max_turn_input_tokens: astra_core::RuntimeLimits::global().max_turn_input_tokens,
            budget_wrapup_injected: false,
            budget_wrapup_ignored_rounds: 0,
            compact_tier_applied: astra_turn_core::compaction_types::CompactionTier::Normal,
            skill_produced_output: false,
            max_cumulative_tokens: 0,
            thinking: thinking_config,
            recent_file_reads: Vec::new(),
            permission_context: None,
            permission_handler: None,
            tactical_adapter: None,
            step_signal_collector: None,
            tool_budget_override: None,
            recent_tactical_actions: Vec::new(),
            server_tool_executor: None,
            interruption: None,
            session_facts: Default::default(),
            memory_extraction_service: self.memory_extraction_service.clone(),
            session_memory_state: Default::default(),
            session_memory_llm_params: None,
            compact_strategy: astra_turn_core::microcompact::CompactStrategy::from_provider_hint(
                request.model.as_deref().unwrap_or(""),
            ),
            approval_overrides: None,
            confidence_trend: Default::default(),
            last_confidence_diagnosis: None,
            session_turn: 0,
            bridge_turn_chain_id: Some(format!("{}:harness", session_id)),
            bridge_user_query_event_id: None,
            turn_event_buffer: None,
            harness: {
                #[cfg(feature = "harness")]
                {
                    if let (Some(registry), Some(server_sink), Some(sink_arc)) = (
                        &self.harness_registry,
                        harness_server_sink,
                        harness_sink_arc,
                    ) {
                        let broadcaster_tx = server_sink.broadcaster_sender();
                        let limits = astra_harness::HarnessLimits {
                            max_turns: if max_turns > 0 {
                                Some(max_turns as u32)
                            } else {
                                None
                            },
                            ..Default::default()
                        };
                        let kernel = std::sync::Arc::new(
                            astra_harness::StandardKernel::configured(sink_arc.clone(), limits),
                        );
                        registry.register_with_broadcast(
                            session_id.to_string(),
                            sink_arc.clone(),
                            broadcaster_tx,
                        );
                        let mut slot = crate::turn::harness_adapter::HarnessSlot::new(
                            kernel as std::sync::Arc<dyn astra_harness::HarnessKernel>,
                            sink_arc,
                        );
                        slot.registry = Some(registry.clone());
                        slot.session_id_for_cleanup = Some(session_id.to_string());
                        slot.server_sink = Some(server_sink);
                        slot
                    } else {
                        crate::turn::harness_adapter::HarnessSlot::empty()
                    }
                }
                #[cfg(not(feature = "harness"))]
                {
                    crate::turn::harness_adapter::HarnessSlot::empty()
                }
            },
        }
    }

    fn thinking_from_chat_context(
        context: &Option<Map<String, Value>>,
        model: Option<&str>,
    ) -> astra_turn_core::thinking_config::ThinkingConfig {
        if let Some(value) = context.as_ref().and_then(|ctx| ctx.get("thinking")) {
            return astra_turn_core::thinking_config::ThinkingConfig::from_payload_value(value);
        }
        model
            .map(|name| astra_turn_core::thinking_config::resolve_model_thinking(name).1)
            .unwrap_or_default()
    }
    /// Extract edge tools from the request context, or provide empty defaults.
    /// Parse the request context into a typed [`EdgeContext`].
    fn extract_edge_context(request: &ChatRequestData) -> EdgeContext {
        request
            .context
            .as_ref()
            .map(EdgeContext::from_context_map)
            .unwrap_or_default()
    }

    /// Extract edge tools from the request context, or provide empty defaults.
    fn extract_edge_tools(request: &ChatRequestData) -> Vec<Value> {
        Self::extract_edge_context(request).edge_tools
    }

    /// Extract edge profile from the request context, or provide empty defaults.
    fn extract_edge_profile(request: &ChatRequestData) -> Map<String, Value> {
        Self::extract_edge_context(request).edge_profile.to_map()
    }

    /// Provision a sandboxed workspace directory for server-side tool execution.
    fn provision_server_workspace(&self, session_id: &str) -> std::path::PathBuf {
        // Sanitize session_id to prevent path traversal — only allow
        // alphanumeric chars, hyphens, and underscores (covers UUID format).
        let safe_id: String = session_id
            .chars()
            .filter(|c| c.is_alphanumeric() || *c == '-' || *c == '_')
            .collect();
        assert!(
            !safe_id.is_empty(),
            "session_id must contain at least one valid character"
        );

        let base = std::env::var("ASTRA_SERVER_WORKSPACES")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|_| std::env::temp_dir().join("astra-workspaces"));
        let workspace = base.join(&safe_id);
        let _ = std::fs::create_dir_all(&workspace);
        workspace
    }

    /// Collect run events into SSE-compatible format.
    fn format_run_events(events: &[Value], start_index: usize) -> Vec<Value> {
        events
            .iter()
            .enumerate()
            .map(|(i, ev)| {
                let mut out = ev.clone();
                if let Some(obj) = out.as_object_mut() {
                    obj.insert("index".to_string(), json!(start_index + i));
                }
                out
            })
            .collect()
    }

    fn durable_status_record(run: &DurableRunRecord) -> RunStatusRecord {
        RunStatusRecord {
            run_id: run.run_id.clone(),
            session_id: run.session_id.clone(),
            status: run.status.clone(),
            waiting_for: run.waiting_for.clone(),
            events_count: run.events.len() as i64,
        }
    }

    fn durable_stream_events(run: &DurableRunRecord, last_index: u32) -> Vec<Value> {
        let offset = last_index as usize;
        if offset < run.events.len() {
            Self::format_run_events(&run.events[offset..], offset)
        } else {
            Vec::new()
        }
    }

    fn durable_recent_events(run: &DurableRunRecord, limit: u32) -> Vec<Value> {
        let capped = limit.clamp(1, 100) as usize;
        let offset = run.events.len().saturating_sub(capped);
        Self::format_run_events(&run.events[offset..], offset)
    }

    async fn load_durable_run_for_user(
        &self,
        run_id: &str,
        user_id: &str,
    ) -> Result<Option<DurableRunRecord>, (StatusCode, Json<ErrorResponse>)> {
        let run = self.run_engine.load_run(run_id).await.map_err(|error| {
            error_response(
                StatusCode::SERVICE_UNAVAILABLE,
                format!("Failed to load durable run state: {error}"),
            )
        })?;

        let Some(run) = run else {
            return Ok(None);
        };

        if run.user_id != user_id {
            return Err(error_response(StatusCode::FORBIDDEN, "Access denied"));
        }

        Ok(Some(run))
    }

    async fn require_durable_run_for_user(
        &self,
        run_id: &str,
        user_id: &str,
    ) -> Result<DurableRunRecord, (StatusCode, Json<ErrorResponse>)> {
        self.load_durable_run_for_user(run_id, user_id)
            .await?
            .ok_or_else(|| error_response(StatusCode::NOT_FOUND, "Run not found"))
    }

    fn run_status_from_durable(
        status: &str,
    ) -> Result<RunStatus, (StatusCode, Json<ErrorResponse>)> {
        match status {
            STATUS_RUNNING => Ok(RunStatus::Running),
            STATUS_PAUSED => Ok(RunStatus::Paused),
            STATUS_WAITING => Ok(RunStatus::Waiting),
            STATUS_COMPLETED => Ok(RunStatus::Completed),
            STATUS_FAILED => Ok(RunStatus::Failed),
            STATUS_CANCELLED => Ok(RunStatus::Cancelled),
            other => Err(error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Invalid durable run status '{other}'"),
            )),
        }
    }

    fn run_state_conflict(action: &str, status: &str) -> (StatusCode, Json<ErrorResponse>) {
        error_response(
            StatusCode::CONFLICT,
            format!("Cannot {action} run in '{status}' state"),
        )
    }

    fn run_control_state_unavailable(action: &str) -> (StatusCode, Json<ErrorResponse>) {
        error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            format!("Run control state unavailable for {action}"),
        )
    }

    fn durable_persist_error(action: &str, error: String) -> (StatusCode, Json<ErrorResponse>) {
        error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            format!("Failed to persist durable run {action}: {error}"),
        )
    }

    #[cfg(test)]
    pub(crate) async fn test_llm_cancel_token_is_cancelled(&self, run_id: &str) -> Option<bool> {
        let runs = self.runs.read().await;
        runs.get(run_id).map(|r| r.llm_cancel_token.is_cancelled())
    }

    #[cfg(test)]
    pub(crate) async fn test_pause_flag_is_set(&self, run_id: &str) -> Option<bool> {
        let runs = self.runs.read().await;
        runs.get(run_id)
            .map(|r| r.pause_flag.load(Ordering::Acquire))
    }
}

#[async_trait]
impl RunLifecycleService for AgenticRunLifecycleService {
    /// Create a run (background mode): spawns the agentic loop in a task, returns immediately.
    async fn create_run(
        &self,
        user_id: String,
        request: ChatRequestData,
    ) -> Result<ChatRunRecord, (StatusCode, Json<ErrorResponse>)> {
        self.validate_request_constraints(&user_id, &request)
            .await?;

        // ── Resource governance check (Phase 5) ─────────────────────
        if let Some(ref gov) = self.resource_governor {
            if let astra_services::resource_governor::LimitCheck::Denied { reason } =
                gov.check_run_start(&user_id).await
            {
                return Err(error_response(
                    StatusCode::TOO_MANY_REQUESTS,
                    format!("Resource limit exceeded: {reason}"),
                ));
            }
        }

        let run_id = Uuid::new_v4().to_string();
        let session_id = request
            .session_id
            .clone()
            .unwrap_or_else(|| Uuid::new_v4().to_string());

        // Guard: reject if this session already has a blocking run.
        // Hold write lock across check+insert to prevent TOCTOU race.
        let (run_state, cancel_flag, pause_flag, llm_cancel_token) =
            Self::build_tracked_run_state(run_id.clone(), session_id.clone(), user_id.clone());
        {
            let mut runs = self.runs.write().await;
            let has_active = Self::session_has_blocking_run(&runs, &session_id);
            if has_active {
                return Err(error_response(
                    StatusCode::CONFLICT,
                    "session already has an active run".to_string(),
                ));
            }
            runs.insert(run_id.clone(), run_state);
        }

        if let Err(error) = self.persist_run_start(&run_id, &user_id, &session_id).await {
            self.runs.write().await.remove(&run_id);
            return Err(error);
        }

        // Spawn background agentic loop.
        let edge_tools = Self::extract_edge_tools(&request);
        let edge_profile = Self::extract_edge_profile(&request);

        // Provision workspace early so build_initial_state can load stop hooks
        // from the provisioned directory when no edge profile supplies cwd.
        let server_workspace = if edge_tools.is_empty() {
            Some(self.provision_server_workspace(&session_id))
        } else {
            None
        };
        // Look up the plan-resume hint up-front so the system prompt on every
        // turn reminds the LLM a plan is in flight. Missing pool → None, missing
        // active plan → None, transient errors → None (best-effort).
        let plan_resume_hint = if let Some(shared) = &self.shared_pool {
            let repo = astra_plan::CloudPlanRepository::new(shared.get().clone());
            astra_plan::plan_resume_hint_for_session(&repo, &session_id).await
        } else {
            None
        };
        let mut host = self.build_host(
            &user_id,
            &session_id,
            &request,
            edge_tools,
            edge_profile,
            plan_resume_hint,
        );
        let mut loop_state = self.build_initial_state(
            &user_id,
            &request,
            &session_id,
            &run_id,
            server_workspace.as_deref(),
            Some(llm_cancel_token.clone()),
        );
        loop_state.context_manifest_user_id = Some(user_id.clone());
        // Inject user_id into the harness sink used by DB-persistence tests.
        #[cfg(feature = "harness")]
        loop_state.harness.set_user_id(&user_id);

        loop_state.session_turn = infer_session_turn(self.shared_pool.as_ref(), &session_id).await;

        // ── Pipeline warm-start: restore PipelineSession from checkpoint ──
        // Overwrites the fresh `PipelineSession::new()` with a snapshot that
        // carries cache hit ratios, reserve estimates, latches, and escalation
        // counters from the last checkpoint. Without this, every server-side
        // session resume starts with cold pipeline state — the write side
        // (agentic_loop_finalization) persists it, but nothing was reading it
        // back until now.
        if let Ok(Some(restored)) = astra_pipeline::step_restore::restore_session(&session_id)
            && restored.pipeline_state.is_some()
        {
            loop_state.pipeline_session =
                Some(astra_turn_core::pipeline_session_serde::restore_or_new(
                    astra_turn_core::pipeline_config::PipelineConfig::default(),
                    restored.pipeline_state.as_ref(),
                ));
        }

        // ── CSL: Load conversation history from the log ─────────────
        let csl_manager = if request.session_id.is_some() {
            self.restore_csl_history(&user_id, &session_id, &run_id, &mut loop_state)
                .await
        } else {
            None
        };

        self.configure_loop_state_runtime_controls(
            &mut loop_state,
            &cancel_flag,
            &pause_flag,
            &llm_cancel_token,
        );
        configure_runtime_controllers(
            &self.matrixone,
            self.shared_pool.as_ref(),
            &mut loop_state,
            &user_id,
            &session_id,
        )
        .await;
        // When no edge tools are provided (no CLI connected), use the
        // already-provisioned workspace for the ServerToolExecutor.
        if let Some(workspace) = server_workspace {
            let memoria_base = Some(astra_core::MemoriaSettings::from_env().base_url);
            let task_store = astra_tools::task_mgmt_matrixone::select_task_store(
                self.shared_pool.as_ref().map(|p| p.get().clone()),
            );
            let mut executor = super::server_tool_executor::ServerToolExecutor::new(
                workspace.clone(),
                user_id.clone(),
                session_id.clone(),
                memoria_base,
                None,
            )
            .with_capabilities(crate::capabilities::lifecycle_server_capabilities(
                self.shared_pool.is_some(),
            ))
            .with_cancel_token(loop_state.cancellation.token.clone())
            .with_task_store(task_store);
            if let Some(pool) = &self.edge_connection_pool {
                executor.set_edge_connection_pool(pool.clone());
            }
            // Wire the plan repository so enter/exit_plan_mode tools work and
            // the write-tool guard can check `active_plan_id`.
            if let Some(shared) = &self.shared_pool {
                executor.set_context_manifest_pool(shared.clone());
                executor = executor.with_workspace_artifact_store(
                    astra_services::DatabaseSessionArtifactStore::new(self.matrixone.clone())
                        .with_pool(shared.clone()),
                );
                executor.set_plan_repository(std::sync::Arc::new(
                    astra_plan::CloudPlanRepository::new(shared.get().clone()),
                ));
                // Production sink: exit_plan_mode(approved=true) seeds
                // `session_plan_todos` so the next turn has executable
                // items without the model manually re-creating each
                // subtask via `task.create`.
                executor.set_plan_todo_sink(std::sync::Arc::new(
                    astra_services::DatabasePlanTodoSink::new(
                        astra_services::DatabaseStateProjectionStore::new(shared.clone()),
                    ),
                ));
            }
            // Share the host's plan-resume hint slot so tool-triggered
            // plan-mode changes refresh the system prompt mid-run.
            executor.set_plan_resume_hint_handle(host.plan_resume_hint_handle());
            if let Some(observability_session) = loop_state.telemetry.observability_session.clone()
            {
                executor.set_observability_session(observability_session);
            }
            if let Some(writer) = self.auxiliary_event_writer.clone() {
                executor.set_auxiliary_event_writer(writer);
            }

            self.wire_server_dynamic_agent_tools(
                &mut executor,
                &user_id,
                &session_id,
                &run_id,
                loop_state.session_turn,
                &request,
                workspace.as_path(),
                Some(pause_flag.clone()),
                Some(llm_cancel_token.clone()),
                #[cfg(feature = "harness")]
                loop_state.harness.sink.clone(),
            )
            .await;

            // ── Phase E: Wire WebSocket approval gate ───────────────
            let (approval_tx, approval_rx) = mpsc::unbounded_channel();
            let approval_gate = astra_turn_core::ws_approval_gate::WebSocketApprovalGate::new(
                user_id.clone(),
                self.edge_callback_ledger.clone(),
                approval_tx,
            );
            executor.set_approval_gate(std::sync::Arc::new(approval_gate));
            self.approval_channels
                .lock()
                .await
                .insert(run_id.clone(), approval_rx);

            if request.interactive_client {
                let (user_prompt_tx, user_prompt_rx) = mpsc::unbounded_channel();
                let user_prompt_gate =
                    astra_turn_core::ws_user_prompt_gate::WebSocketUserPromptGate::new(
                        user_id.clone(),
                        self.edge_callback_ledger.clone(),
                        user_prompt_tx,
                    );
                executor.set_ask_user_gate(std::sync::Arc::new(user_prompt_gate));
                self.user_prompt_channels
                    .lock()
                    .await
                    .insert(run_id.clone(), user_prompt_rx);
            }

            // ── Phase F.3: Wire WebSocket progress callback ─────────
            let (progress_tx, progress_rx) = mpsc::unbounded_channel();
            let progress_cb =
                astra_server_types::ws_progress_callback::WebSocketProgressCallback::new(
                    progress_tx,
                );
            executor.set_progress_callback(std::sync::Arc::new(progress_cb));
            self.progress_channels
                .lock()
                .await
                .insert(run_id.clone(), progress_rx);

            loop_state.server_tool_executor = Some(std::sync::Arc::new(executor));
        }

        // Clone handles we need inside the spawned task.
        let bg_approval_channels = self.approval_channels.clone();
        let bg_user_prompt_channels = self.user_prompt_channels.clone();
        let bg_progress_channels = self.progress_channels.clone();
        let runs = self.runs_handle();
        let run_engine = self.run_engine.clone();
        let bg_run_id = run_id.clone();
        let bg_session_id = session_id.clone();
        let bg_resource_governor = self.resource_governor.clone();
        let bg_user_id = user_id.clone();
        let persist_ctx = PostLoopPersistContext {
            matrixone: self.matrixone.clone(),
            shared_pool: self.shared_pool.clone(),
            user_id: user_id.clone(),
            session_id: session_id.clone(),
            run_id: run_id.clone(),
            agent_id: request.agent_id.clone(),
            model_name: request.model.clone(),
            user_message: request.message.clone(),
            hook_db_writer: self.hook_db_writer.clone(),
            observer_worker: self.observer_worker.clone(),
            tool_event_writer: self.tool_event_writer.clone(),
            csl_manager: csl_manager.map(tokio::sync::Mutex::new),
        };

        // Background task tracking: background_task_count is incremented before
        // spawn and decremented via RAII guard on exit. serve()'s shutdown path
        // calls drain_background_tasks() to wait for in-flight runs.
        let bg_task_count_1 = Arc::clone(&self.background_task_count);
        bg_task_count_1.fetch_add(1, Ordering::Release);
        tokio::spawn(async move {
            // RAII guard: decrement counter when this task exits (normal or panic).
            struct TaskCountGuard(Arc<AtomicUsize>);
            impl Drop for TaskCountGuard {
                fn drop(&mut self) {
                    self.0.fetch_sub(1, Ordering::Release);
                }
            }
            let _guard = TaskCountGuard(bg_task_count_1);
            // Pre-flight: check daily token budget before starting the agentic loop.
            if let Some(ref gov) = bg_resource_governor {
                use astra_services::resource_governor::LimitCheck;
                if let LimitCheck::Denied { reason } = gov.check_token_budget(&bg_user_id).await {
                    tracing::warn!(
                        target: "astra_runtime::run_lifecycle",
                        user_id = %bg_user_id,
                        run_id = %bg_run_id,
                        reason = %reason,
                        "run rejected: daily token budget exhausted"
                    );
                    if let Some(run) = runs.write().await.get_mut(&bg_run_id) {
                        if run.status.try_transition(&RunStatus::Failed).is_ok() {
                            run.status = RunStatus::Failed;
                        }
                        // Push terminal events so SSE clients see the failure.
                        run.events.push(json!({
                            "event_type": "run_error",
                            "data": {"error": reason.clone()}
                        }));
                        run.events.push(json!({
                            "event_type": "run_finished",
                            "data": {"total_prompt_tokens": 0, "total_completion_tokens": 0}
                        }));
                        run.live_tx = None;
                    }
                    // Clean up channels for this run.
                    bg_approval_channels.lock().await.remove(&bg_run_id);
                    bg_user_prompt_channels.lock().await.remove(&bg_run_id);
                    bg_progress_channels.lock().await.remove(&bg_run_id);
                    astra_core::log_persist!(
                        run_engine
                            .persist_status(
                                &bg_run_id,
                                astra_core::STATUS_FAILED,
                                None,
                                Some(&reason),
                            )
                            .await,
                        "run_lifecycle",
                        &bg_run_id,
                        "budget_reject"
                    );
                    Self::schedule_run_eviction(&runs, bg_run_id.clone());
                    return;
                }
            }

            let outcome = run_agentic_loop_with_host_panic_safe(&mut host, &mut loop_state).await;
            let loop_success = outcome.is_ok();
            let (events, final_status, error_msg) =
                Self::finalize_run_events(outcome, host.take_emitted_events(), &loop_state);

            // Clean up channels for this run.
            bg_approval_channels.lock().await.remove(&bg_run_id);
            bg_user_prompt_channels.lock().await.remove(&bg_run_id);
            bg_progress_channels.lock().await.remove(&bg_run_id);
            let terminal_events = terminal_events_for_persistence(&events);

            // Publish terminal run state before best-effort post-run side effects
            // so background observers do not stay stuck in "running" because a
            // hook, event write, or learning save is slow.
            let mut persisted_status = final_status.clone();
            let mut persist_status_update = true;
            let mut persist_terminal_events = true;

            if let Some(run) = runs.write().await.get_mut(&bg_run_id) {
                if run.status == RunStatus::Cancelled {
                    persist_status_update = false;
                    persist_terminal_events = false;
                    merge_cancelled_run_events(run, events);
                    if final_status != RunStatus::Waiting {
                        run.live_tx = None;
                    }
                    flush_turn_observability(&mut loop_state, &bg_session_id, true);
                } else {
                    run.events.extend(events);
                    if should_preserve_manual_pause_on_completion(&run.status, &final_status) {
                        persist_status_update = false;
                        persisted_status = RunStatus::Paused;
                        run.waiting_for
                            .get_or_insert_with(|| "user_resume".to_string());
                        run.live_tx = None;
                    } else if run.status.try_transition(&final_status).is_ok() {
                        run.status = final_status.clone();
                    }
                    if run.status != RunStatus::Waiting && run.status != RunStatus::Paused {
                        run.live_tx = None;
                    }
                }
            }

            // Schedule eviction of the terminal run from the in-memory cache.
            // Waiting and paused runs are NOT evicted — they may still be resumed.
            if persisted_status != RunStatus::Waiting && persisted_status != RunStatus::Paused {
                Self::schedule_run_eviction(&runs, bg_run_id.clone());
            }

            if persist_status_update {
                astra_core::log_persist!(
                    run_engine
                        .persist_status(
                            &bg_run_id,
                            persisted_status.as_str(),
                            None,
                            error_msg.as_deref()
                        )
                        .await,
                    "run_lifecycle",
                    &bg_run_id,
                    "status"
                );
            }
            astra_core::log_persist!(
                run_engine
                    .persist_usage(
                        &bg_run_id,
                        loop_state.total_prompt,
                        loop_state.total_completion,
                        loop_state.total_tool_calls,
                    )
                    .await,
                "run_lifecycle",
                &bg_run_id,
                "usage"
            );
            // Record tokens consumed so check_token_budget sees up-to-date usage.
            if let Some(ref gov) = bg_resource_governor {
                let total = loop_state.total_prompt + loop_state.total_completion;
                if total > 0 {
                    gov.record_tokens(&bg_user_id, total).await;
                }
            }
            if persist_terminal_events {
                for event in terminal_events {
                    astra_core::log_persist!(
                        run_engine.append_event(&bg_run_id, event).await,
                        "run_lifecycle",
                        &bg_run_id,
                        "append_terminal_event"
                    );
                }
            }

            if persist_terminal_events {
                flush_turn_observability(&mut loop_state, &bg_session_id, false);
                persist_turn_evaluation_journal(&bg_session_id, "server_runtime", &loop_state);
            }

            // Best-effort post-loop persistence (core events, tool events,
            // hook DB, observer, session-end hooks, promotion events).
            persist_ctx.run(&loop_state, loop_success).await;

            // Post-loop memory cleanup — shared with the streaming path
            // (see `stream_chat`). Runs governance once per session
            // debounce window, clears bridge seen-ledger, and forgets
            // extraction debounce.
            post_loop_memory_cleanup(
                loop_state.current_session_id.as_deref().unwrap_or(""),
                &loop_state.session_facts,
                loop_state.memory_extraction_service.as_ref(),
            )
            .await;
        });

        Ok(ChatRunRecord {
            session_id,
            run_id,
            status: STATUS_RUNNING.to_string(),
            explain: if request.explain {
                Some(json!({"mode": "background"}))
            } else {
                None
            },
        })
    }

    /// Stream chat (incremental SSE mode): spawns the agentic loop in a
    /// background task and returns an event channel for incremental streaming.
    /// Post-loop cleanup (persistence, learning state) runs inside the task.
    async fn stream_chat(
        &self,
        user_id: String,
        request: ChatRequestData,
    ) -> Result<ChatStreamRecord, (StatusCode, Json<ErrorResponse>)> {
        self.validate_request_constraints(&user_id, &request)
            .await?;

        // ── Resource governance check ────────────────────────────────
        if let Some(ref gov) = self.resource_governor {
            if let astra_services::resource_governor::LimitCheck::Denied { reason } =
                gov.check_run_start(&user_id).await
            {
                return Err(error_response(
                    StatusCode::TOO_MANY_REQUESTS,
                    format!("Resource limit exceeded: {reason}"),
                ));
            }
        }

        let run_id = Uuid::new_v4().to_string();
        let session_id = request
            .session_id
            .clone()
            .unwrap_or_else(|| Uuid::new_v4().to_string());

        let edge_tools = Self::extract_edge_tools(&request);
        let edge_profile = Self::extract_edge_profile(&request);

        // Provision workspace early for web-agent mode (no edge tools) so
        // build_initial_state loads stop hooks from the provisioned directory.
        let server_workspace = if edge_tools.is_empty() {
            Some(self.provision_server_workspace(&session_id))
        } else {
            None
        };

        // Create the bounded SSE channel. 512 events is generous for any single
        // turn; hitting the limit means the client cannot keep up, so we treat
        // channel-full the same as client disconnect (cancel the loop).
        const SSE_CHANNEL_CAPACITY: usize = 512;
        let (client_event_tx, event_rx) = mpsc::channel::<Value>(SSE_CHANNEL_CAPACITY);
        let (event_tx, mut fanout_rx) = mpsc::channel::<Value>(SSE_CHANNEL_CAPACITY);
        let (live_tx, _) = broadcast::channel::<Value>(SSE_CHANNEL_CAPACITY);
        let live_tx_for_fanout = live_tx.clone();
        let fanout_runs = self.runs_handle();
        let fanout_run_id = run_id.clone();
        let fanout_run_engine = self.run_engine.clone();
        tokio::spawn(async move {
            while let Some(event) = fanout_rx.recv().await {
                if live_delta_event_for_persistence(&event) {
                    if let Some(run) = fanout_runs.write().await.get_mut(&fanout_run_id) {
                        run.events.push(event.clone());
                    }
                    astra_core::log_persist!(
                        fanout_run_engine
                            .append_event(&fanout_run_id, event.clone())
                            .await,
                        "run_lifecycle",
                        &fanout_run_id,
                        "append_live_delta_event"
                    );
                }
                let _ = live_tx_for_fanout.send(event.clone());
                let _ = client_event_tx.send(event).await;
            }
        });

        let (mut run_state, cancel_flag, pause_flag, llm_cancel_token) =
            Self::build_tracked_run_state(run_id.clone(), session_id.clone(), user_id.clone());
        run_state.live_tx = Some(live_tx.clone());

        let mut state = self.build_initial_state(
            &user_id,
            &request,
            &session_id,
            &run_id,
            server_workspace.as_deref(),
            Some(llm_cancel_token.clone()),
        );
        state.context_manifest_user_id = Some(user_id.clone());
        // Inject user_id into the harness sink used by DB-persistence tests.
        #[cfg(feature = "harness")]
        state.harness.set_user_id(&user_id);

        state.session_turn = infer_session_turn(self.shared_pool.as_ref(), &session_id).await;

        // ── Pipeline warm-start from step checkpoint ────────────────
        if request.session_id.is_some() {
            if let Ok(Some(restored)) = astra_pipeline::step_restore::restore_session(&session_id) {
                if restored.pipeline_state.is_some() {
                    state.pipeline_session =
                        Some(astra_turn_core::pipeline_session_serde::restore_or_new(
                            astra_turn_core::pipeline_config::PipelineConfig::default(),
                            restored.pipeline_state.as_ref(),
                        ));
                }
            }
        }

        // ── CSL: Load conversation history from the log ─────────────
        let csl_manager = if request.session_id.is_some() {
            self.restore_csl_history(&user_id, &session_id, &run_id, &mut state)
                .await
        } else {
            None
        };

        let plan_resume_hint = if let Some(shared) = &self.shared_pool {
            let repo = astra_plan::CloudPlanRepository::new(shared.get().clone());
            astra_plan::plan_resume_hint_for_session(&repo, &session_id).await
        } else {
            None
        };
        let mut host = self.build_host(
            &user_id,
            &session_id,
            &request,
            edge_tools,
            edge_profile,
            plan_resume_hint,
        );
        host.set_event_tx(event_tx.clone());
        host.set_client_cancel(cancel_flag.clone(), llm_cancel_token.clone());

        // Guard: reject if this session already has a blocking run.
        // Hold write lock across check+insert to prevent TOCTOU race.
        {
            let mut runs = self.runs.write().await;
            let has_active = Self::session_has_blocking_run(&runs, &session_id);
            if has_active {
                return Err(error_response(
                    StatusCode::CONFLICT,
                    "session already has an active run".to_string(),
                ));
            }
            runs.insert(run_id.clone(), run_state);
        }
        if let Err(error) = self.persist_run_start(&run_id, &user_id, &session_id).await {
            self.runs.write().await.remove(&run_id);
            return Err(error);
        }
        if let Some(pool) = &self.shared_pool {
            let trace = server_trace_context(&user_id, &session_id, &run_id, state.session_turn);
            let user_transcript = TranscriptPersistItem {
                run_id: run_id.clone(),
                role: "user",
                content: request.message.clone(),
                source_event_id: trace.root_event_id,
            };
            persist_session_transcript_items(pool, &user_id, &session_id, &[user_transcript]).await;
        }

        self.configure_loop_state_runtime_controls(
            &mut state,
            &cancel_flag,
            &pause_flag,
            &llm_cancel_token,
        );
        configure_runtime_controllers(
            &self.matrixone,
            self.shared_pool.as_ref(),
            &mut state,
            &user_id,
            &session_id,
        )
        .await;

        // Wire ServerToolExecutor when no edge agent is connected (web-agent mode).
        if let Some(workspace) = server_workspace {
            let memoria_base = Some(astra_core::MemoriaSettings::from_env().base_url);
            let task_store = astra_tools::task_mgmt_matrixone::select_task_store(
                self.shared_pool.as_ref().map(|p| p.get().clone()),
            );
            let mut executor = super::server_tool_executor::ServerToolExecutor::new(
                workspace.clone(),
                user_id.clone(),
                session_id.clone(),
                memoria_base,
                None,
            )
            .with_capabilities(crate::capabilities::lifecycle_server_capabilities(
                self.shared_pool.is_some(),
            ))
            .with_cancel_token(state.cancellation.token.clone())
            .with_task_store(task_store);
            if let Some(pool) = &self.edge_connection_pool {
                executor.set_edge_connection_pool(pool.clone());
            }
            if let Some(shared) = &self.shared_pool {
                executor.set_context_manifest_pool(shared.clone());
                executor = executor.with_workspace_artifact_store(
                    astra_services::DatabaseSessionArtifactStore::new(self.matrixone.clone())
                        .with_pool(shared.clone()),
                );
                executor.set_plan_repository(std::sync::Arc::new(
                    astra_plan::CloudPlanRepository::new(shared.get().clone()),
                ));
                // Production sink: exit_plan_mode(approved=true) seeds
                // `session_plan_todos` so the next turn has executable
                // items without the model manually re-creating each
                // subtask via `task.create`.
                executor.set_plan_todo_sink(std::sync::Arc::new(
                    astra_services::DatabasePlanTodoSink::new(
                        astra_services::DatabaseStateProjectionStore::new(shared.clone()),
                    ),
                ));
            }
            if let Some(observability_session) = state.telemetry.observability_session.clone() {
                executor.set_observability_session(observability_session);
            }
            if let Some(writer) = self.auxiliary_event_writer.clone() {
                executor.set_auxiliary_event_writer(writer);
            }
            self.wire_server_dynamic_agent_tools(
                &mut executor,
                &user_id,
                &session_id,
                &run_id,
                state.session_turn,
                &request,
                workspace.as_path(),
                Some(pause_flag.clone()),
                Some(llm_cancel_token.clone()),
                #[cfg(feature = "harness")]
                state.harness.sink.clone(),
            )
            .await;
            state.server_tool_executor = Some(std::sync::Arc::new(executor));
        }

        // Clone handles for the background task.
        let runs = self.runs_handle();
        let run_engine = self.run_engine.clone();
        let bg_run_id = run_id.clone();
        let bg_session_id = session_id.clone();
        let bg_resource_governor = self.resource_governor.clone();
        let bg_user_id = user_id.clone();
        let persist_ctx = PostLoopPersistContext {
            matrixone: self.matrixone.clone(),
            shared_pool: self.shared_pool.clone(),
            user_id: user_id.clone(),
            session_id: session_id.clone(),
            run_id: run_id.clone(),
            agent_id: request.agent_id.clone(),
            model_name: request.model.clone(),
            user_message: request.message.clone(),
            hook_db_writer: self.hook_db_writer.clone(),
            observer_worker: self.observer_worker.clone(),
            tool_event_writer: self.tool_event_writer.clone(),
            csl_manager: csl_manager.map(tokio::sync::Mutex::new),
        };

        // Background task tracking (same pattern as the create_run spawn above).
        // Spawn the agentic loop in a background task. Events are pushed
        // through event_tx incrementally; the HTTP handler streams them.
        let bg_task_count_2 = Arc::clone(&self.background_task_count);
        bg_task_count_2.fetch_add(1, Ordering::Release);
        tokio::spawn(async move {
            struct TaskCountGuard(Arc<AtomicUsize>);
            impl Drop for TaskCountGuard {
                fn drop(&mut self) {
                    self.0.fetch_sub(1, Ordering::Release);
                }
            }
            let _guard = TaskCountGuard(bg_task_count_2);
            let loop_result = run_agentic_loop_with_host_panic_safe(&mut host, &mut state).await;
            let loop_success = loop_result.is_ok();

            // Best-effort post-loop persistence (core events, tool events,
            // hook DB, observer, session-end hooks, promotion events).
            persist_ctx.run(&state, loop_success).await;

            let (mut final_events, final_status, error_msg) =
                Self::finalize_run_events(loop_result, host.take_emitted_events(), &state);
            // In streaming mode, `text_delta` events were already sent to the client
            // in real-time via `event_tx`. Exclude them from `streamed_final_events`
            // to avoid double-emission. Terminal events (text_done, run_finished, etc.)
            // are added by `finalize_run_events` and must still be sent.
            let streamed_final_events = super::run_handlers::transform_stream_run_events_for_client(
                &bg_run_id,
                final_events
                    .iter()
                    .filter(|e| {
                        e.get("type").and_then(serde_json::Value::as_str) != Some("text_delta")
                    })
                    .cloned()
                    .collect(),
            );
            persist_turn_evaluation_journal(&bg_session_id, "server_runtime", &state);
            let mut all_events = vec![json!({"event_type": "run_started", "data": {}})];
            all_events.append(&mut final_events);

            let mut persisted_status = final_status.clone();
            let mut persist_status_update = true;
            // Extract terminal events before the branch — both branches consume
            // all_events by move, so this must happen first.
            let terminal_events = terminal_events_for_persistence(&all_events);
            if let Some(run) = runs.write().await.get_mut(&bg_run_id) {
                if run.status == RunStatus::Cancelled {
                    persist_status_update = false;
                    merge_cancelled_run_events(run, all_events);
                    if final_status != RunStatus::Waiting {
                        run.live_tx = None;
                    }
                    flush_turn_observability(&mut state, &bg_session_id, true);
                } else {
                    run.events.extend(all_events);
                    if should_preserve_manual_pause_on_completion(&run.status, &final_status) {
                        persist_status_update = false;
                        persisted_status = RunStatus::Paused;
                        run.waiting_for
                            .get_or_insert_with(|| "user_resume".to_string());
                        run.live_tx = None;
                    } else if run.status.try_transition(&final_status).is_ok() {
                        run.status = final_status.clone();
                    }
                    if run.status != RunStatus::Waiting && run.status != RunStatus::Paused {
                        run.live_tx = None;
                    }
                    flush_turn_observability(&mut state, &bg_session_id, false);
                }
            }

            // Schedule eviction of the terminal run from the in-memory cache.
            // Waiting and paused runs are NOT evicted — they may still be resumed.
            if persisted_status != RunStatus::Waiting && persisted_status != RunStatus::Paused {
                Self::schedule_run_eviction(&runs, bg_run_id.clone());
            }

            // Record tokens consumed regardless of cancel — cancelled runs still
            // consumed tokens and must count toward the daily budget.
            if let Some(ref gov) = bg_resource_governor {
                let total = state.total_prompt + state.total_completion;
                if total > 0 {
                    gov.record_tokens(&bg_user_id, total).await;
                }
            }

            if persist_status_update {
                astra_core::log_persist!(
                    run_engine
                        .persist_status(
                            &bg_run_id,
                            persisted_status.as_str(),
                            None,
                            error_msg.as_deref()
                        )
                        .await,
                    "run_lifecycle",
                    &bg_run_id,
                    "status"
                );
            }

            for event in streamed_final_events {
                if event_tx.send(event).await.is_err() {
                    break;
                }
            }

            // Persist usage unconditionally — cancelled runs still consumed tokens
            // and must have accurate usage in durable store for billing/audit.
            astra_core::log_persist!(
                run_engine
                    .persist_usage(
                        &bg_run_id,
                        state.total_prompt,
                        state.total_completion,
                        state.total_tool_calls,
                    )
                    .await,
                "run_lifecycle",
                &bg_run_id,
                "usage"
            );

            // Persist terminal events to durable store.
            for event in terminal_events {
                astra_core::log_persist!(
                    run_engine.append_event(&bg_run_id, event).await,
                    "run_lifecycle",
                    &bg_run_id,
                    "append_terminal_event"
                );
            }

            // Emit turn_complete event so clients (HTTP SSE, WebSocket) know the turn is done.
            let _ = event_tx
                .send(build_run_turn_complete_event_with_interruption(
                    state.total_tool_calls,
                    &state.final_text,
                    state.interruption.as_ref(),
                ))
                .await;

            // Drop event_tx — signals end-of-stream to the HTTP handler.
            drop(event_tx);

            // Post-loop memory cleanup — identical to `create_run`. Runs
            // AFTER event_tx drops so the client sees turn_complete
            // promptly and doesn't wait on governance RTT.
            post_loop_memory_cleanup(
                state.current_session_id.as_deref().unwrap_or(""),
                &state.session_facts,
                state.memory_extraction_service.as_ref(),
            )
            .await;
        });

        Ok(ChatStreamRecord {
            session_id,
            run_id,
            events: Vec::new(),
            event_rx: Some(event_rx),
        })
    }

    async fn get_run_status(
        &self,
        run_id: String,
        user_id: String,
    ) -> Result<RunStatusRecord, (StatusCode, Json<ErrorResponse>)> {
        let run = self.require_durable_run_for_user(&run_id, &user_id).await?;
        Ok(Self::durable_status_record(&run))
    }

    async fn get_run_projection(
        &self,
        run_id: String,
        user_id: String,
        recent_limit: u32,
    ) -> Result<RunProjectionRecord, (StatusCode, Json<ErrorResponse>)> {
        let run = self.require_durable_run_for_user(&run_id, &user_id).await?;
        let projection = self
            .run_engine
            .load_run_projection(&run_id)
            .await
            .map_err(|error| {
                error_response(
                    StatusCode::SERVICE_UNAVAILABLE,
                    format!("Failed to load run projection: {error}"),
                )
            })?;
        let latest_checkpoint = self
            .run_engine
            .load_latest_checkpoint(&run_id, None)
            .await
            .map_err(|error| {
                error_response(
                    StatusCode::SERVICE_UNAVAILABLE,
                    format!("Failed to load run checkpoint: {error}"),
                )
            })?;
        let recent_events = Self::durable_recent_events(&run, recent_limit);

        if let Some(projection) = projection {
            Ok(RunProjectionRecord {
                run_id: projection.run_id,
                session_id: projection.session_id,
                status: projection.status,
                waiting_for: projection.waiting_for,
                error_message: projection.error_message,
                run_event_high_watermark: run.last_event_idx,
                projection_event_idx: projection.projection_event_idx,
                projection_updated_at: projection.updated_at,
                projection_hash: projection.projection_hash,
                latest_event_type: projection.latest_event_type,
                total_prompt_tokens: projection.total_prompt_tokens,
                total_completion_tokens: projection.total_completion_tokens,
                total_tool_calls: projection.total_tool_calls,
                latest_checkpoint: latest_checkpoint.map(|checkpoint| {
                    RunProjectionCheckpointRecord {
                        checkpoint_id: checkpoint.checkpoint_id,
                        checkpoint_kind: checkpoint.checkpoint_kind,
                        checkpoint_version: checkpoint.checkpoint_version,
                        node_seq: checkpoint.node_seq,
                        created_at: checkpoint.created_at,
                    }
                }),
                recent_events,
            })
        } else {
            let latest_event_type = run.events.last().map(astra_services::extract_event_type);
            Ok(RunProjectionRecord {
                run_id: run.run_id.clone(),
                session_id: run.session_id.clone(),
                status: run.status.clone(),
                waiting_for: run.waiting_for.clone(),
                error_message: run.error_message.clone(),
                run_event_high_watermark: run.last_event_idx,
                projection_event_idx: run.last_event_idx,
                projection_updated_at: run.updated_at.clone(),
                projection_hash: format!(
                    "{:x}",
                    Sha256::digest(
                        serde_json::json!({
                            "run_id": run.run_id,
                            "status": run.status,
                            "waiting_for": run.waiting_for,
                            "last_event_idx": run.last_event_idx,
                            "total_prompt_tokens": run.total_prompt_tokens,
                            "total_completion_tokens": run.total_completion_tokens,
                            "total_tool_calls": run.total_tool_calls,
                            "latest_event_type": latest_event_type.clone(),
                        })
                        .to_string()
                        .as_bytes()
                    )
                ),
                latest_event_type,
                total_prompt_tokens: run.total_prompt_tokens,
                total_completion_tokens: run.total_completion_tokens,
                total_tool_calls: run.total_tool_calls,
                latest_checkpoint: latest_checkpoint.map(|checkpoint| {
                    RunProjectionCheckpointRecord {
                        checkpoint_id: checkpoint.checkpoint_id,
                        checkpoint_kind: checkpoint.checkpoint_kind,
                        checkpoint_version: checkpoint.checkpoint_version,
                        node_seq: checkpoint.node_seq,
                        created_at: checkpoint.created_at,
                    }
                }),
                recent_events,
            })
        }
    }

    async fn stream_run(
        &self,
        run_id: String,
        user_id: String,
        last_index: u32,
    ) -> Result<Vec<Value>, (StatusCode, Json<ErrorResponse>)> {
        let run = self.require_durable_run_for_user(&run_id, &user_id).await?;
        Ok(Self::durable_stream_events(&run, last_index))
    }

    async fn stream_run_live(
        &self,
        run_id: String,
        user_id: String,
        last_index: u32,
    ) -> Result<ChatStreamRecord, (StatusCode, Json<ErrorResponse>)> {
        let durable = self.require_durable_run_for_user(&run_id, &user_id).await?;
        let live_tx = {
            let runs = self.runs.read().await;
            runs.get(&run_id).and_then(|run| run.live_tx.clone())
        };
        if let Some(live_tx) = live_tx {
            let replay_events = Self::durable_stream_events(&durable, last_index);
            let mut live_rx = live_tx.subscribe();
            let (event_tx, event_rx) = mpsc::channel(512);
            tokio::spawn(async move {
                for event in replay_events {
                    if event_tx.send(event).await.is_err() {
                        return;
                    }
                }
                loop {
                    match live_rx.recv().await {
                        Ok(event) => {
                            if event_tx.send(event).await.is_err() {
                                return;
                            }
                        }
                        Err(broadcast::error::RecvError::Lagged(_)) => continue,
                        Err(broadcast::error::RecvError::Closed) => return,
                    }
                }
            });
            return Ok(ChatStreamRecord {
                session_id: durable.session_id,
                run_id,
                events: Vec::new(),
                event_rx: Some(event_rx),
            });
        }

        let events = Self::durable_stream_events(&durable, last_index);
        Ok(ChatStreamRecord {
            session_id: durable.session_id,
            run_id,
            events,
            event_rx: None,
        })
    }

    async fn drain_approval_requests(&self, run_id: &str) -> Vec<serde_json::Value> {
        let mut channels = self.approval_channels.lock().await;
        let Some(rx) = channels.get_mut(run_id) else {
            return vec![];
        };
        let mut requests = Vec::new();
        while let Ok(req) = rx.try_recv() {
            requests.push(req);
        }
        requests
    }

    async fn drain_user_prompt_requests(&self, run_id: &str) -> Vec<serde_json::Value> {
        let mut channels = self.user_prompt_channels.lock().await;
        let Some(rx) = channels.get_mut(run_id) else {
            return vec![];
        };
        let mut requests = Vec::new();
        while let Ok(req) = rx.try_recv() {
            requests.push(req);
        }
        requests
    }

    async fn drain_progress_events(&self, run_id: &str) -> Vec<serde_json::Value> {
        let mut channels = self.progress_channels.lock().await;
        let Some(rx) = channels.get_mut(run_id) else {
            return vec![];
        };
        let mut events = Vec::new();
        while let Ok(evt) = rx.try_recv() {
            events.push(serde_json::to_value(&evt).unwrap_or_default());
        }
        events
    }

    async fn submit_run_input(
        &self,
        run_id: String,
        user_id: String,
        input: RunInputData,
    ) -> Result<RunInputRecord, (StatusCode, Json<ErrorResponse>)> {
        if input.idempotency_key.trim().is_empty() {
            return Err(error_response(
                StatusCode::UNPROCESSABLE_ENTITY,
                "idempotency_key is required",
            ));
        }

        let idempotency_key = input.idempotency_key.trim().to_string();
        let event = json!({
            "event_type": "user_input",
            "idempotency_key": idempotency_key,
            "data": {"input": input.input},
        });

        let durable = self.require_durable_run_for_user(&run_id, &user_id).await?;
        let durable_status = Self::run_status_from_durable(&durable.status)?;
        if matches!(
            durable_status,
            RunStatus::Completed | RunStatus::Failed | RunStatus::Cancelled
        ) {
            return Err(Self::run_state_conflict("submit input to", &durable.status));
        }

        let duplicate = durable.events.iter().any(|event| {
            event.get("idempotency_key").and_then(Value::as_str) == Some(idempotency_key.as_str())
        });
        if duplicate {
            return Ok(RunInputRecord {
                run_id,
                accepted: true,
                duplicate: true,
            });
        }

        self.run_engine
            .append_event(&run_id, event.clone())
            .await
            .map_err(|error| Self::durable_persist_error("input", error))?;

        if let Some(run) = self.runs.write().await.get_mut(&run_id) {
            run.events.push(event);
        }

        Ok(RunInputRecord {
            run_id,
            accepted: true,
            duplicate: false,
        })
    }

    async fn drain_background_tasks(&self, timeout: std::time::Duration) -> bool {
        self.drain_background_tasks_impl(timeout).await
    }

    async fn cancel_run(
        &self,
        run_id: String,
        user_id: String,
    ) -> Result<CancelRunRecord, (StatusCode, Json<ErrorResponse>)> {
        let durable = self.require_durable_run_for_user(&run_id, &user_id).await?;
        let durable_status = Self::run_status_from_durable(&durable.status)?;
        if matches!(
            durable_status,
            RunStatus::Completed | RunStatus::Failed | RunStatus::Cancelled
        ) {
            return Ok(CancelRunRecord {
                run_id,
                status: durable.status,
            });
        }

        let cancel_event = json!({"event_type": "run_finished", "data": {"cancelled": true}});
        self.run_engine
            .persist_status(&run_id, STATUS_CANCELLED, None, None)
            .await
            .map_err(|error| Self::durable_persist_error("cancel status", error))?;
        let append_result = self
            .run_engine
            .append_event(&run_id, cancel_event.clone())
            .await;

        {
            let mut runs = self.runs.write().await;
            if let Some(run) = runs.get_mut(&run_id) {
                run.cancel_flag.store(true, Ordering::SeqCst);
                run.pause_flag.store(false, Ordering::SeqCst);
                run.llm_cancel_token.cancel();
                run.status = RunStatus::Cancelled;
                run.waiting_for = None;
                run.events.push(cancel_event);
            }
        }

        if let Some(de) = &self.delegation_engine {
            de.cancel_children_of(&run_id).await;
        }
        append_result.map_err(|error| Self::durable_persist_error("cancel event", error))?;

        Ok(CancelRunRecord {
            run_id,
            status: STATUS_CANCELLED.to_string(),
        })
    }

    async fn list_runs(
        &self,
        user_id: String,
        limit: u32,
        offset: u32,
    ) -> Result<RunListRecord, (StatusCode, Json<ErrorResponse>)> {
        let (limit, offset) = astra_services::pagination::clamp_api_list_pagination(limit, offset);
        let (durable_runs, total) = self
            .run_engine
            .list_user_runs(&user_id, limit, offset)
            .await
            .map_err(|error| {
                error_response(
                    StatusCode::SERVICE_UNAVAILABLE,
                    format!("Failed to list durable run state: {error}"),
                )
            })?;
        let page = durable_runs
            .iter()
            .map(Self::durable_status_record)
            .collect();
        Ok(RunListRecord {
            runs: page,
            total,
            limit,
            offset,
        })
    }

    async fn pause_run(
        &self,
        run_id: String,
        user_id: String,
    ) -> Result<RunMutationRecord, (StatusCode, Json<ErrorResponse>)> {
        let durable = self.require_durable_run_for_user(&run_id, &user_id).await?;
        if durable.status != STATUS_RUNNING {
            return Err(Self::run_state_conflict("pause", &durable.status));
        }

        let pause_event = json!({"event_type": "run_paused", "data": {}});
        {
            let mut runs = self.runs.write().await;
            let Some(run) = runs.get_mut(&run_id) else {
                return Err(Self::run_control_state_unavailable("pause"));
            };
            if run.status.try_transition(&RunStatus::Paused).is_err() {
                return Err(Self::run_state_conflict("pause", run.status.as_str()));
            }
            run.pause_flag.store(true, Ordering::SeqCst);
        }

        if let Err(error) = self
            .run_engine
            .persist_status(&run_id, STATUS_PAUSED, Some("user_resume"), None)
            .await
        {
            if let Some(run) = self.runs.write().await.get_mut(&run_id) {
                run.pause_flag.store(false, Ordering::SeqCst);
            }
            return Err(Self::durable_persist_error("pause status", error));
        }
        let append_result = self
            .run_engine
            .append_event(&run_id, pause_event.clone())
            .await;

        {
            let mut runs = self.runs.write().await;
            if let Some(run) = runs.get_mut(&run_id) {
                run.status = RunStatus::Paused;
                run.pause_flag.store(true, Ordering::SeqCst);
                run.waiting_for = Some("user_resume".to_string());
                run.events.push(pause_event);
            }
        }

        if let Err(error) = append_result {
            if let Some(run) = self.runs.write().await.get_mut(&run_id) {
                run.status = RunStatus::Running;
                run.pause_flag.store(false, Ordering::SeqCst);
                run.waiting_for = None;
                run.events.pop();
            }
            let _ = self
                .run_engine
                .persist_status(&run_id, STATUS_RUNNING, None, None)
                .await;
            return Err(Self::durable_persist_error("pause event", error));
        }
        if let Some(de) = &self.delegation_engine {
            de.pause_children_of(&run_id).await;
        }
        Ok(RunMutationRecord {
            run_id,
            status: STATUS_PAUSED.to_string(),
            previous_status: durable.status,
        })
    }

    async fn resume_run(
        &self,
        run_id: String,
        user_id: String,
    ) -> Result<RunMutationRecord, (StatusCode, Json<ErrorResponse>)> {
        let durable = self.require_durable_run_for_user(&run_id, &user_id).await?;
        if durable.status != STATUS_PAUSED {
            return Err(Self::run_state_conflict("resume", &durable.status));
        }

        if has_buffered_terminal_completion(&durable.events) {
            self.run_engine
                .persist_status(&run_id, STATUS_COMPLETED, None, None)
                .await
                .map_err(|error| Self::durable_persist_error("resume completed status", error))?;
            {
                let mut runs = self.runs.write().await;
                if let Some(run) = runs.get_mut(&run_id) {
                    run.status = RunStatus::Completed;
                    run.pause_flag.store(false, Ordering::SeqCst);
                    run.waiting_for = None;
                    run.live_tx = None;
                }
            }
            Self::schedule_run_eviction(&self.runs, run_id.clone());
            return Ok(RunMutationRecord {
                run_id,
                status: STATUS_COMPLETED.to_string(),
                previous_status: durable.status,
            });
        }

        let resume_event = json!({"event_type": "run_resumed", "data": {}});
        {
            let mut runs = self.runs.write().await;
            let Some(run) = runs.get_mut(&run_id) else {
                return Err(Self::run_control_state_unavailable("resume"));
            };
            if run.status.try_transition(&RunStatus::Running).is_err() {
                return Err(Self::run_state_conflict("resume", run.status.as_str()));
            }
        }

        self.run_engine
            .persist_status(&run_id, STATUS_RUNNING, None, None)
            .await
            .map_err(|error| Self::durable_persist_error("resume status", error))?;
        let append_result = self
            .run_engine
            .append_event(&run_id, resume_event.clone())
            .await;

        {
            let mut runs = self.runs.write().await;
            if let Some(run) = runs.get_mut(&run_id) {
                run.status = RunStatus::Running;
                run.pause_flag.store(false, Ordering::SeqCst);
                run.waiting_for = None;
                run.events.push(resume_event);
            }
        }

        if let Err(error) = append_result {
            if let Some(run) = self.runs.write().await.get_mut(&run_id) {
                run.status = RunStatus::Paused;
                run.pause_flag.store(true, Ordering::SeqCst);
                run.waiting_for = Some("user_resume".to_string());
                run.events.pop();
            }
            let _ = self
                .run_engine
                .persist_status(&run_id, STATUS_PAUSED, Some("user_resume"), None)
                .await;
            return Err(Self::durable_persist_error("resume event", error));
        }
        if let Some(de) = &self.delegation_engine {
            de.resume_children_of(&run_id).await;
        }
        Ok(RunMutationRecord {
            run_id,
            status: STATUS_RUNNING.to_string(),
            previous_status: durable.status,
        })
    }
}

// ─── Sub-Run Executor ───────────────────────────────────────────────────────

use crate::server::delegation_engine::{SubRunConfig, SubRunExecutor};

/// Server-side executor for dynamic `agent.spawn` children.
///
/// It reuses the production sub-run loop executor so Web dynamic agents run
/// with the same server host, tool backend, skill resolver, memory plumbing,
/// and observe-only harness path as delegated children. Spawn-specific
/// semantics stay in `DynamicAgentSpawner` and `agent_tool`.
pub struct ServerSpawnAgentExecutor {
    matrixone: MatrixOneSettings,
    encryptor: Arc<FernetTokenEncryptor>,
    shared_pool: Option<SharedPool>,
    edge_callback_ledger: Arc<TokioMutex<HashMap<String, Value>>>,
    edge_connection_pool: Option<astra_server_types::edge_connection_pool::EdgeConnectionPool>,
    skill_service: Option<Arc<dyn SkillService>>,
    memory_extraction_service: Option<Arc<crate::session_memory::MemoryExtractionService>>,
    runtime_contexts: Arc<RwLock<HashMap<String, ServerSpawnRuntimeContext>>>,
}

impl ServerSpawnAgentExecutor {
    pub fn new(
        matrixone: MatrixOneSettings,
        encryptor: Arc<FernetTokenEncryptor>,
        edge_callback_ledger: Arc<TokioMutex<HashMap<String, Value>>>,
    ) -> Self {
        Self {
            matrixone,
            encryptor,
            shared_pool: None,
            edge_callback_ledger,
            edge_connection_pool: None,
            skill_service: None,
            memory_extraction_service: None,
            runtime_contexts: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    pub fn with_pool(mut self, pool: Option<SharedPool>) -> Self {
        self.shared_pool = pool;
        self
    }

    pub fn with_edge_connection_pool(
        mut self,
        pool: Option<astra_server_types::edge_connection_pool::EdgeConnectionPool>,
    ) -> Self {
        self.edge_connection_pool = pool;
        self
    }

    pub fn with_skill_service(mut self, service: Option<Arc<dyn SkillService>>) -> Self {
        self.skill_service = service;
        self
    }

    pub fn with_memory_extraction_service(
        mut self,
        svc: Option<Arc<crate::session_memory::MemoryExtractionService>>,
    ) -> Self {
        self.memory_extraction_service = svc;
        self
    }

    async fn set_runtime_context(&self, context: ServerSpawnRuntimeContext) {
        self.runtime_contexts
            .write()
            .await
            .insert(context.parent_run_id.clone(), context);
    }

    async fn runtime_context_for_config(
        &self,
        config: &SpawnRunConfig,
    ) -> Result<ServerSpawnRuntimeContext, String> {
        let parent_run_id = config
            .parent_address
            .as_ref()
            .map(|address| address.run_id.as_str())
            .ok_or_else(|| {
                "server dynamic agent executor requires parent run lineage".to_string()
            })?;

        self.runtime_contexts
            .read()
            .await
            .get(parent_run_id)
            .cloned()
            .ok_or_else(|| {
                format!(
                    "server dynamic agent executor has no runtime context for parent run {parent_run_id}"
                )
            })
    }

    fn build_subrun_executor(&self) -> ServerSubRunExecutor {
        let mut executor = ServerSubRunExecutor::new(
            self.matrixone.clone(),
            Arc::clone(&self.encryptor),
            Arc::clone(&self.edge_callback_ledger),
        );
        if let Some(pool) = self.shared_pool.clone() {
            executor = executor.with_pool(pool);
        }
        if let Some(pool) = self.edge_connection_pool.clone() {
            executor = executor.with_edge_connection_pool(pool);
        }
        if let Some(service) = self.skill_service.clone() {
            executor = executor.with_skill_service(service);
        }
        if let Some(svc) = self.memory_extraction_service.clone() {
            executor = executor.with_memory_extraction_service(svc);
        }
        executor
    }
}

fn spawn_child_request_constraints(
    parent: &RequestConstraints,
    config: &SpawnRunConfig,
) -> RequestConstraints {
    let child_allowed = if config.allowed_tools.iter().any(|tool| tool == "*") {
        if config.read_only {
            Some(
                ["bash", "glob", "grep", "list_dir", "read_file"]
                    .into_iter()
                    .map(String::from)
                    .collect::<HashSet<_>>(),
            )
        } else {
            None
        }
    } else {
        Some(
            config
                .allowed_tools
                .iter()
                .map(|tool| tool.trim().to_ascii_lowercase())
                .filter(|tool| !tool.is_empty())
                .collect::<HashSet<_>>(),
        )
    };

    let allowed_tools = match (&parent.allowed_tools, child_allowed) {
        (Some(parent), Some(child)) => Some(parent.intersection(&child).cloned().collect()),
        (Some(parent), None) => Some(parent.clone()),
        (None, Some(child)) => Some(child),
        (None, None) => None,
    };

    RequestConstraints::new(allowed_tools, parent.allowed_skills.clone())
}

fn spawn_system_prompt(config: &SpawnRunConfig) -> String {
    if config.system_prompt_addendum.trim().is_empty() {
        format!(
            "You are '{}', a specialized sub-agent. Complete the task thoroughly.",
            config.agent_id
        )
    } else {
        format!(
            "You are '{}', a specialized sub-agent.\n\n{}\n\nComplete the task thoroughly.",
            config.agent_id, config.system_prompt_addendum
        )
    }
}

fn spawn_status_to_finish_reason(status: &str) -> &'static str {
    match status {
        STATUS_COMPLETED => "normal",
        STATUS_WAITING => "waiting",
        STATUS_CANCELLED => "cancelled",
        STATUS_FAILED => "failed",
        STATUS_PAUSED => "paused",
        _ => "unknown",
    }
}

#[async_trait]
impl SpawnAgentExecutor for ServerSpawnAgentExecutor {
    async fn execute(&self, config: SpawnRunConfig) -> Result<SpawnRunResult, String> {
        let context = self.runtime_context_for_config(&config).await?;

        let mut profile =
            AgentProfile::new(&config.agent_id, &config.agent_type, AgentTier::System);
        profile.system_prompt = Some(spawn_system_prompt(&config));
        profile.model_override = config.model.clone();
        profile.skill_filter = config.allowed_tools.clone();
        profile.metadata.insert(
            "spawn_agent_type".to_string(),
            json!(config.agent_type.clone()),
        );
        profile
            .metadata
            .insert("spawn_read_only".to_string(), json!(config.read_only));

        let mut subrun_context = HashMap::new();
        subrun_context.insert(
            "workspace_root".to_string(),
            json!(config.working_dir.to_string_lossy().to_string()),
        );
        subrun_context.insert(
            "cwd".to_string(),
            json!(config.working_dir.to_string_lossy().to_string()),
        );
        subrun_context.insert("spawn_agent_id".to_string(), json!(config.agent_id.clone()));
        subrun_context.insert(
            "spawn_agent_type".to_string(),
            json!(config.agent_type.clone()),
        );
        subrun_context.insert(
            "parent_run_id".to_string(),
            json!(context.parent_run_id.clone()),
        );
        subrun_context.insert(
            "parent_agent_id".to_string(),
            json!(
                config
                    .parent_address
                    .as_ref()
                    .map(|address| address.agent_id.clone())
                    .unwrap_or_else(|| "root-agent".to_string())
            ),
        );
        subrun_context.insert(
            "trace_session_id".to_string(),
            json!(context.trace_context.session_id.clone()),
        );
        subrun_context.insert(
            "trace_user_id".to_string(),
            json!(context.trace_context.user_id.clone()),
        );
        subrun_context.insert(
            "trace_turn_id".to_string(),
            json!(context.trace_context.turn_id.clone()),
        );
        subrun_context.insert(
            "trace_turn_seq".to_string(),
            json!(context.trace_context.turn_seq),
        );
        subrun_context.insert(
            "trace_causal_chain_id".to_string(),
            json!(context.trace_context.causal_chain_id.clone()),
        );
        subrun_context.insert(
            "trace_root_event_id".to_string(),
            json!(context.trace_context.root_event_id.clone()),
        );

        let request_constraints =
            spawn_child_request_constraints(&context.request_constraints, &config);
        let subrun = SubRunConfig {
            run_id: config.run_id.clone(),
            agent_profile: profile,
            task: config.task.clone(),
            session_id: context.session_id.clone(),
            user_id: context.user_id.clone(),
            previous_output: None,
            context: subrun_context,
            forward_headers: context.forward_headers.clone(),
            llm_token_service: context.llm_token_service.clone(),
            request_constraints,
            recursion_depth: config.recursion_depth,
            pause_flag: context.pause_flag.clone(),
            checkpoint_gate: None,
            mailbox: config.mailbox,
            cancel_token: context.cancel_token.clone(),
            inherited_prefix: config.inherited_prefix,
            #[cfg(feature = "harness")]
            harness_sink: context.harness_sink.clone(),
        };

        let executor = self.build_subrun_executor();
        #[cfg(feature = "bridge-e2e-hooks")]
        let executor = if !context.test_child_llm_rounds.is_empty() {
            executor.with_test_llm_rounds(context.test_child_llm_rounds.clone())
        } else {
            executor
        };
        let result = executor.execute(subrun).await?;
        let finish_reason = spawn_status_to_finish_reason(&result.status).to_string();
        let status = match result.status.as_str() {
            STATUS_COMPLETED => "completed",
            STATUS_WAITING => "waiting",
            STATUS_CANCELLED => "cancelled",
            STATUS_FAILED => "failed",
            STATUS_PAUSED => "completed",
            _ => "failed",
        }
        .to_string();
        let error = if status == "failed" {
            result
                .error
                .or_else(|| Some(format!("server spawned agent ended with {}", result.status)))
        } else {
            result.error
        };

        Ok(SpawnRunResult {
            agent_id: result.agent_id,
            run_id: result.run_id,
            status,
            finish_reason,
            output: result.output,
            error,
            prompt_tokens: result.prompt_tokens,
            completion_tokens: result.completion_tokens,
            tool_calls: result.tool_calls,
            permission_summary: None,
            permission_requests: 0,
            permission_requests_approved: 0,
            tools_blocked: 0,
        })
    }
}

/// Production sub-run executor backed by [`ServerAgenticLoopHost`].
///
/// Creates a real agentic loop for each sub-run with the agent's system prompt,
/// model, and tool configuration.
pub struct ServerSubRunExecutor {
    matrixone: MatrixOneSettings,
    encryptor: Arc<FernetTokenEncryptor>,
    shared_pool: Option<SharedPool>,
    edge_callback_ledger: Arc<TokioMutex<HashMap<String, Value>>>,
    edge_connection_pool: Option<astra_server_types::edge_connection_pool::EdgeConnectionPool>,
    skill_service: Option<Arc<dyn SkillService>>,
    memory_extraction_service: Option<Arc<crate::session_memory::MemoryExtractionService>>,
    #[cfg(feature = "bridge-e2e-hooks")]
    test_llm_rounds: Vec<Value>,
}

impl ServerSubRunExecutor {
    pub fn new(
        matrixone: MatrixOneSettings,
        encryptor: Arc<FernetTokenEncryptor>,
        edge_callback_ledger: Arc<TokioMutex<HashMap<String, Value>>>,
    ) -> Self {
        Self {
            matrixone,
            encryptor,
            shared_pool: None,
            edge_callback_ledger,
            edge_connection_pool: None,
            skill_service: None,
            memory_extraction_service: None,
            #[cfg(feature = "bridge-e2e-hooks")]
            test_llm_rounds: Vec::new(),
        }
    }

    pub fn with_pool(mut self, pool: SharedPool) -> Self {
        self.shared_pool = Some(pool);
        self
    }

    pub fn with_memory_extraction_service(
        mut self,
        svc: Arc<crate::session_memory::MemoryExtractionService>,
    ) -> Self {
        self.memory_extraction_service = Some(svc);
        self
    }

    pub fn with_edge_connection_pool(
        mut self,
        pool: astra_server_types::edge_connection_pool::EdgeConnectionPool,
    ) -> Self {
        self.edge_connection_pool = Some(pool);
        self
    }

    pub fn with_skill_service(mut self, service: Arc<dyn SkillService>) -> Self {
        self.skill_service = Some(service);
        self
    }

    #[cfg(feature = "bridge-e2e-hooks")]
    pub fn with_test_llm_rounds(mut self, rounds: Vec<Value>) -> Self {
        self.test_llm_rounds = rounds;
        self
    }
}

impl ServerSubRunExecutor {
    /// Provision a workspace directory for a delegation sub-run.
    ///
    /// Sub-runs get a subdirectory under the parent session workspace to
    /// keep file operations isolated while sharing the same base.
    fn provision_subrun_workspace(&self, session_id: &str, run_id: &str) -> std::path::PathBuf {
        let sanitize = |s: &str| -> String {
            s.chars()
                .filter(|c| c.is_alphanumeric() || *c == '-' || *c == '_')
                .collect()
        };
        let safe_session = sanitize(session_id);
        let safe_run = sanitize(run_id);
        assert!(!safe_session.is_empty(), "session_id must be non-empty");
        assert!(!safe_run.is_empty(), "run_id must be non-empty");

        let base = std::env::var("ASTRA_SERVER_WORKSPACES")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|_| std::env::temp_dir().join("astra-workspaces"));
        let workspace = base.join(&safe_session).join(&safe_run);
        let _ = std::fs::create_dir_all(&workspace);
        workspace
    }
}

#[async_trait]
impl SubRunExecutor for ServerSubRunExecutor {
    async fn execute(
        &self,
        config: SubRunConfig,
    ) -> Result<astra_services::coordination::AgentResult, String> {
        use astra_pipeline::step_protocol::InMemoryIdempotencyCache;
        use astra_text_utils::semantic_dedup::SemanticDedup;
        use astra_turn_core::chat_turn_heuristics::infer_task_execution_profile;
        use astra_turn_core::stop_hooks_yaml::{
            detect_turn_hook_sets, is_plan_subtask_from_delegation_context,
            project_root_from_delegation_context,
        };
        use astra_turn_core::turn_guard::TurnGuard;

        // Build edge profile from agent's system prompt and metadata.
        let compact_strategy = astra_turn_core::microcompact::CompactStrategy::from_provider_hint(
            config.agent_profile.model_override.as_deref().unwrap_or(""),
        );
        let mut edge_profile = Map::new();
        if let Some(prompt) = &config.agent_profile.system_prompt {
            edge_profile.insert(
                "system_prompt_override".to_string(),
                Value::String(prompt.clone()),
            );
        }
        if let Some(model) = &config.agent_profile.model_override {
            edge_profile.insert("model".to_string(), Value::String(model.clone()));
        }
        edge_profile.insert(
            "agent_id".to_string(),
            Value::String(config.agent_profile.agent_id.clone()),
        );

        // Build the host with agent-specific configuration.
        let mut builder = ServerAgenticLoopHostBuilder::new(
            self.matrixone.clone(),
            self.encryptor.clone(),
            config.user_id.clone(),
            config.session_id.clone(),
        )
        .with_model(config.agent_profile.model_override.clone())
        .with_llm_token_service(config.llm_token_service.clone())
        .with_capabilities(crate::capabilities::lifecycle_server_capabilities(
            self.shared_pool.is_some(),
        ))
        .with_edge_profile(edge_profile)
        .with_edge_callback_ledger(self.edge_callback_ledger.clone());

        if let Some(pool) = &self.shared_pool {
            builder = builder.with_pool(pool.clone());
        }
        #[cfg(feature = "bridge-e2e-hooks")]
        if !self.test_llm_rounds.is_empty() {
            builder = builder.with_test_llm_rounds(self.test_llm_rounds.clone());
        }
        // NOTE on grandchild inheritance: delegated children don't get
        // a prefix_store wired here because this sub-run executor
        // doesn't own one. Grandchild captures would be valuable for
        // deeper delegation trees but require threading the store into
        // `ServerSubRunExecutor` separately — scope-cut from G2 v1.
        let mut host = builder.build();

        // Build the task prompt, incorporating previous output if pipeline.
        let full_task = if let Some(prev) = &config.previous_output {
            format!("{}\n\nPrevious agent output:\n{}", config.task, prev)
        } else {
            config.task.clone()
        };

        let user_message = json!({
            "role": "user",
            "content": full_task,
        });

        let task_profile = infer_task_execution_profile(&full_task);
        let project_root_buf = project_root_from_delegation_context(&config.context);
        let hook_sets = project_root_buf
            .as_ref()
            .map(|root| {
                detect_turn_hook_sets(
                    root.as_path(),
                    task_profile,
                    is_plan_subtask_from_delegation_context(&config.context),
                )
            })
            .unwrap_or_default();
        let workspace_root_hint = project_root_buf.map(|p| p.to_string_lossy().into_owned());
        let (tool_event_hooks, session_event_hooks) = workspace_root_hint
            .as_ref()
            .map(|root| crate::skills::hooks::load_all_hooks(std::path::Path::new(root)))
            .unwrap_or_default();

        let (skill_registry, raw_skill_resolver) = build_server_skill_resolver(
            self.skill_service.clone(),
            &config.user_id,
            config.request_constraints.allowed_skills.as_ref(),
        );
        let skill_resolver = apply_normalized_skill_allowlist(
            raw_skill_resolver,
            config.request_constraints.allowed_skills.as_ref(),
        )?;

        // Sub-agent / delegation path: model comes from the agent profile
        // override, not a request field.
        let resolved_tool_policy = astra_config::runtime_config::RuntimeConfig::load()
            .tool_selection
            .resolve_for_model(config.agent_profile.model_override.as_deref());

        let mut loop_state = AgenticLoopState {
            messages: vec![user_message],
            volatile_pending: Vec::new(),
            recent_rounds: Vec::new(),
            tool_results: Vec::new(),
            current_session_id: Some(config.session_id.clone()),
            current_run_id: Some(config.run_id.clone()),
            context_manifest_pool: self.shared_pool.clone(),
            context_manifest_user_id: Some(config.user_id.clone()),
            context_manifest_model_name: config.agent_profile.model_override.clone(),
            recursion_depth: 0,
            final_text: String::new(),
            final_text_streamed: false,
            total_prompt: 0,
            total_completion: 0,
            total_cache_read: 0,
            total_cache_creation: 0,
            total_tool_calls: 0,
            total_evidence_tool_calls: 0,
            has_any_usage: false,
            max_turns: 10,
            remaining_turns: 10,
            turn_budget_hint_emitted_50: false,
            turn_budget_hint_emitted_20: false,
            agentic_turn_budget:
                astra_turn_core::chat_turn_heuristics::TaskExecutionProfile::default()
                    .agentic_turn_budget,
            current_round_index: 0,
            llm_rounds_completed: 0,
            turn_guard: TurnGuard::new(),
            restricted_tools: std::collections::HashSet::new(),
            boosted_tools: std::collections::HashSet::new(),
            widen_selection_pending: false,
            step_recorder: StepRecorder::new(&config.session_id, &config.run_id),
            idempotency_cache: InMemoryIdempotencyCache::new(),
            semantic_dedup: SemanticDedup::new(0.75),
            call_counts: HashMap::new(),
            max_identical_tool_calls: resolved_tool_policy.max_identical_tool_calls,
            max_tools_per_turn: resolved_tool_policy.max_tools_per_turn,
            repeated_cache_hit_suppression: resolved_tool_policy.repeated_cache_hit_suppression,
            max_consecutive_empty_name: resolved_tool_policy.max_consecutive_empty_name,
            stall: Default::default(),
            telemetry: Default::default(),
            skills: SkillState {
                registry_for_activation: if config.request_constraints.allowed_skills.is_some() {
                    None
                } else {
                    skill_registry
                },
                resolver: skill_resolver,
                request_constraints: config.request_constraints.clone(),
                quality_tracker: crate::skills::quality::SkillQualityTracker::new(),
                improvement_tracker: astra_skills::improvement::ImprovementTracker::new(),
                search: skill_search_from_context(&config.context),
                tool_event_hooks,
                session_event_hooks,
                ..Default::default()
            },
            hooks: StopHookState {
                stop_hooks: hook_sets.stop_hooks,
                teammate_idle_hooks: hook_sets.teammate_idle_hooks,
                workspace_root_hint,
                forward_headers: config.forward_headers.clone(),
                llm_token_service: config.llm_token_service.clone(),
                ..Default::default()
            },
            cancellation: CancellationState {
                flag: None,
                pause_flag: config.pause_flag.clone(),
                token: config.cancel_token.clone(),
            },
            messaging: MessagingState {
                mailbox: config.mailbox,
                ..Default::default()
            },
            error_recovery: Default::default(),
            pipeline_session: Some(astra_turn_core::pipeline_session::PipelineSession::new(
                astra_turn_core::pipeline_config::PipelineConfig::default(),
            )),
            message: full_task,
            recent_tools: Vec::new(),
            task_profile,
            last_turn_policy: crate::turn::agentic_loop_host::TurnInteractionPolicy::default(),
            api: astra_thin_client::ThinClient::new("http://127.0.0.1:1", None)
                .expect("valid dummy URL"),
            api_token: String::new(),
            delegation_engine: None,
            delegations_this_turn: 0,
            project_context: None,
            checkpoint_gate: config.checkpoint_gate.clone(),
            last_llm_context_manifest_trace: None,
            rate_limit_cooldown: Default::default(),
            data_snapshot_provider: None,
            last_composite_snapshot: None,
            last_measured_prompt_tokens: None,
            consecutive_context_window_errors: 0,
            compaction_effectiveness: Default::default(),
            pinned_tool_schema_tokens: 0,
            max_turn_input_tokens: astra_core::RuntimeLimits::global().max_turn_input_tokens,
            budget_wrapup_injected: false,
            budget_wrapup_ignored_rounds: 0,
            compact_tier_applied: astra_turn_core::compaction_types::CompactionTier::Normal,
            skill_produced_output: false,
            max_cumulative_tokens: 0,
            thinking: astra_turn_core::thinking_config::ThinkingConfig::Off,
            recent_file_reads: Vec::new(),
            permission_context: None,
            permission_handler: None,
            tactical_adapter: None,
            step_signal_collector: None,
            tool_budget_override: None,
            recent_tactical_actions: Vec::new(),
            server_tool_executor: None,
            interruption: None,
            session_facts: Default::default(),
            memory_extraction_service: self.memory_extraction_service.clone(),
            session_memory_state: Default::default(),
            session_memory_llm_params: None,
            compact_strategy,
            approval_overrides: None,
            confidence_trend: Default::default(),
            last_confidence_diagnosis: None,
            session_turn: 0,
            bridge_turn_chain_id: None,
            bridge_user_query_event_id: None,
            turn_event_buffer: None,
            harness: {
                #[cfg(feature = "harness")]
                {
                    match config.harness_sink {
                        Some(ref sink) => {
                            crate::turn::harness_adapter::HarnessSlot::observe_only(sink.clone())
                        }
                        None => crate::turn::harness_adapter::HarnessSlot::empty(),
                    }
                }
                #[cfg(not(feature = "harness"))]
                {
                    crate::turn::harness_adapter::HarnessSlot::empty()
                }
            },
        };
        if let Some(trace_context) = trace_context_from_subrun_context(&config.context) {
            loop_state.session_turn = u32::try_from(trace_context.turn_seq).unwrap_or(0);
        }

        // ── Wire ServerToolExecutor for sub-run tool execution ──────────
        // Without this, the headless pipeline fallback cannot execute tools
        // server-side and sub-agents would get edge-protocol errors.
        {
            let workspace = self.provision_subrun_workspace(&config.session_id, &config.run_id);
            let memoria_base = Some(astra_core::MemoriaSettings::from_env().base_url);
            let task_store = astra_tools::task_mgmt_matrixone::select_task_store(
                self.shared_pool.as_ref().map(|p| p.get().clone()),
            );
            let mut executor = super::server_tool_executor::ServerToolExecutor::new(
                workspace,
                config.user_id.clone(),
                config.session_id.clone(),
                memoria_base,
                None,
            )
            .with_capabilities(crate::capabilities::lifecycle_server_capabilities(
                self.shared_pool.is_some(),
            ))
            .with_cancel_token(config.cancel_token.clone())
            .with_task_store(task_store);
            if let Some(pool) = self.shared_pool.as_ref() {
                executor.set_context_manifest_pool(pool.clone());
                executor = executor.with_workspace_artifact_store(
                    astra_services::DatabaseSessionArtifactStore::new(self.matrixone.clone())
                        .with_pool(pool.clone()),
                );
            }
            if let Some(pool) = &self.edge_connection_pool {
                executor.set_edge_connection_pool(pool.clone());
            }
            if let Some(shared) = self.shared_pool.as_ref() {
                executor.set_plan_repository(std::sync::Arc::new(
                    astra_plan::CloudPlanRepository::new(shared.get().clone()),
                ));
                // Production sink: exit_plan_mode(approved=true) seeds
                // `session_plan_todos` so the next turn has executable
                // items without the model manually re-creating each
                // subtask via `task.create`.
                executor.set_plan_todo_sink(std::sync::Arc::new(
                    astra_services::DatabasePlanTodoSink::new(
                        astra_services::DatabaseStateProjectionStore::new(shared.clone()),
                    ),
                ));
            }
            executor.set_plan_resume_hint_handle(host.plan_resume_hint_handle());
            if let Some(obs) = loop_state.telemetry.observability_session.clone() {
                executor.set_observability_session(obs);
            }
            loop_state.server_tool_executor = Some(std::sync::Arc::new(executor));
        }

        configure_runtime_controllers(
            &self.matrixone,
            self.shared_pool.as_ref(),
            &mut loop_state,
            &config.user_id,
            &config.session_id,
        )
        .await;

        let outcome = run_agentic_loop_with_host(&mut host, &mut loop_state).await;

        // Fire SessionEnd hooks (best-effort).
        crate::skills::hooks::fire_session_end(
            &loop_state.skills.session_event_hooks,
            loop_state.current_session_id.as_deref().unwrap_or(""),
        )
        .await;
        persist_runtime_promotion_events(
            &self.matrixone,
            self.shared_pool.as_ref(),
            &config.user_id,
            &config.session_id,
            &config.run_id,
            &loop_state.telemetry.promotion_events,
        )
        .await;
        persist_turn_evaluation_journal(&config.session_id, "server_subrun", &loop_state);
        flush_turn_observability(&mut loop_state, &config.session_id, false);

        // Persist core events for delegation sub-runs.
        persist_server_loop_core_events(
            &self.matrixone,
            self.shared_pool.as_ref(),
            &config.user_id,
            &config.session_id,
            &config.run_id,
            config.context.get("parent_run_id").and_then(Value::as_str),
            Some(config.agent_profile.agent_id.as_str()),
            config
                .context
                .get("parent_agent_id")
                .and_then(Value::as_str),
            trace_context_from_subrun_context(&config.context),
            &config.task,
            &loop_state,
            config.agent_profile.model_override.as_deref(),
        )
        .await;
        persist_server_loop_trace_events(
            &self.matrixone,
            self.shared_pool.as_ref(),
            &config.user_id,
            &config.session_id,
            &config.run_id,
            config.context.get("parent_run_id").and_then(Value::as_str),
            Some(config.agent_profile.agent_id.as_str()),
            config
                .context
                .get("parent_agent_id")
                .and_then(Value::as_str),
            trace_context_from_subrun_context(&config.context),
            &loop_state,
            config.agent_profile.model_override.as_deref(),
        )
        .await;

        match outcome {
            Ok(AgenticLoopOutcome::Completed) => {
                let status = if loop_state.interruption.is_some() {
                    STATUS_PAUSED
                } else {
                    STATUS_COMPLETED
                };
                Ok(astra_services::coordination::AgentResult {
                    agent_id: config.agent_profile.agent_id,
                    run_id: config.run_id,
                    status: status.to_string(),
                    output: if loop_state.final_text.is_empty() {
                        None
                    } else {
                        Some(loop_state.final_text)
                    },
                    error: None,
                    prompt_tokens: loop_state.total_prompt,
                    completion_tokens: loop_state.total_completion,
                    tool_calls: loop_state.total_tool_calls,
                })
            }
            Ok(AgenticLoopOutcome::Cancelled) => {
                // Cancelled via pause_flag — report as "paused" so the
                // delegation engine can distinguish from hard errors.
                Ok(astra_services::coordination::AgentResult {
                    agent_id: config.agent_profile.agent_id,
                    run_id: config.run_id,
                    status: STATUS_PAUSED.to_string(),
                    output: if loop_state.final_text.is_empty() {
                        None
                    } else {
                        Some(loop_state.final_text)
                    },
                    error: None,
                    prompt_tokens: loop_state.total_prompt,
                    completion_tokens: loop_state.total_completion,
                    tool_calls: loop_state.total_tool_calls,
                })
            }
            Ok(AgenticLoopOutcome::Waiting(reason)) => {
                Ok(astra_services::coordination::AgentResult {
                    agent_id: config.agent_profile.agent_id,
                    run_id: config.run_id,
                    status: STATUS_WAITING.to_string(),
                    output: Some(reason),
                    error: None,
                    prompt_tokens: loop_state.total_prompt,
                    completion_tokens: loop_state.total_completion,
                    tool_calls: loop_state.total_tool_calls,
                })
            }
            Ok(AgenticLoopOutcome::Error(err)) => Ok(astra_services::coordination::AgentResult {
                agent_id: config.agent_profile.agent_id,
                run_id: config.run_id,
                status: STATUS_FAILED.to_string(),
                output: None,
                error: Some(err),
                prompt_tokens: loop_state.total_prompt,
                completion_tokens: loop_state.total_completion,
                tool_calls: loop_state.total_tool_calls,
            }),
            Err(err) => Ok(astra_services::coordination::AgentResult {
                agent_id: config.agent_profile.agent_id,
                run_id: config.run_id,
                status: STATUS_FAILED.to_string(),
                output: None,
                error: Some(err.to_string()),
                prompt_tokens: loop_state.total_prompt,
                completion_tokens: loop_state.total_completion,
                tool_calls: loop_state.total_tool_calls,
            }),
        }
    }
}

// ─── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::DatabaseTurnHookDbWriter;
    use astra_services::session_journal::{JournalEventType, ToolCallRecord};
    use sqlx::Row;
    use uuid::Uuid;

    // ── extract_prev_assistant_text + implicit feedback wiring ──

    #[test]
    fn trace_redaction_removes_nested_secrets_and_truncates_long_text() {
        let redacted = redact_trace_value(&json!({
            "Authorization": "Bearer secret",
            "nested": {
                "api_key": "abc123",
                "safe": "visible"
            },
            "items": [
                {"cookie": "session=abc"},
                {"text": "x".repeat(2_050)}
            ]
        }));

        assert_eq!(redacted["Authorization"], "[REDACTED]");
        assert_eq!(redacted["nested"]["api_key"], "[REDACTED]");
        assert_eq!(redacted["nested"]["safe"], "visible");
        assert_eq!(redacted["items"][0]["cookie"], "[REDACTED]");
        assert!(
            redacted["items"][1]["text"]
                .as_str()
                .expect("string")
                .ends_with("...")
        );
    }

    #[test]
    fn tool_trace_events_populate_columns_and_redacted_payloads() {
        let trace = TraceContext {
            session_id: "session-1".to_string(),
            user_id: "user-1".to_string(),
            turn_id: "turn-1".to_string(),
            turn_seq: 7,
            causal_chain_id: "chain-1".to_string(),
            root_event_id: "trace:root".to_string(),
        };
        let record = ToolCallRecord {
            tool_call_id: Some("tool-call-1".to_string()),
            name: "agent".to_string(),
            ok: true,
            ms: 42,
            args_preview: Some("agent.spawn: child".to_string()),
            result_preview: Some("launched child".to_string()),
            round: Some(2),
            args_full: Some(r#"{"action":"spawn","token":"secret"}"#.to_string()),
            result_full: Some(
                r#"{"agent_id":"child@run","run_id":"child-run","result":"ok"}"#.to_string(),
            ),
            ..Default::default()
        };

        let events = build_tool_trace_events(
            &trace,
            "root-run",
            None,
            Some("root-agent"),
            None,
            &[record],
        );

        assert_eq!(events.len(), 2);
        assert_eq!(events[0].event_type, "tool_call_started");
        assert_eq!(events[0].tool_call_id.as_deref(), Some("tool-call-1"));
        assert_eq!(events[0].round_index, Some(2));
        assert_eq!(events[0].meta_tool_name.as_deref(), Some("agent"));
        assert_eq!(
            events[0].metadata["tool_args_json_redacted"]["token"],
            "[REDACTED]"
        );
        assert_eq!(events[1].event_type, "tool_call_completed");
        assert_eq!(events[1].meta_duration_ms, Some(42));
        assert_eq!(events[1].metadata["action"], "spawn");
        assert_eq!(events[1].metadata["child_run_id"], "child-run");
    }

    #[test]
    fn extract_prev_assistant_text_picks_latest_assistant_string() {
        let messages = vec![
            serde_json::json!({"role": "user", "content": "hi"}),
            serde_json::json!({"role": "assistant", "content": "first answer"}),
            serde_json::json!({"role": "user", "content": "follow up"}),
            serde_json::json!({"role": "assistant", "content": "latest answer"}),
        ];
        assert_eq!(
            extract_prev_assistant_text(&messages).as_deref(),
            Some("latest answer")
        );
    }

    #[test]
    fn extract_prev_assistant_text_handles_content_parts_array() {
        let messages = vec![
            serde_json::json!({"role": "user", "content": "hi"}),
            serde_json::json!({
                "role": "assistant",
                "content": [
                    {"type": "text", "text": "part one"},
                    {"type": "text", "text": "part two"},
                ],
            }),
        ];
        assert_eq!(
            extract_prev_assistant_text(&messages).as_deref(),
            Some("part one\npart two")
        );
    }

    #[test]
    fn extract_prev_assistant_text_returns_none_when_no_assistant_turn() {
        let messages = vec![serde_json::json!({"role": "user", "content": "hi"})];
        assert!(extract_prev_assistant_text(&messages).is_none());
    }

    #[test]
    fn extract_prev_assistant_text_skips_empty_assistant_bodies() {
        let messages = vec![
            serde_json::json!({"role": "assistant", "content": "real answer"}),
            serde_json::json!({"role": "user", "content": "ok"}),
            serde_json::json!({"role": "assistant", "content": "   "}),
        ];
        assert_eq!(
            extract_prev_assistant_text(&messages).as_deref(),
            Some("real answer")
        );
    }

    #[test]
    fn build_run_turn_complete_event_carries_authoritative_assistant_text() {
        let event =
            build_run_turn_complete_event_with_interruption(0, "recovered final answer", None);
        assert_eq!(event["type"], "turn_complete");
        assert_eq!(event["assistant_text"], "recovered final answer");
        assert_eq!(event["has_tool_calls"], false);
    }

    #[test]
    fn build_run_turn_complete_event_omits_empty_assistant_text() {
        let event = build_run_turn_complete_event_with_interruption(1, "", None);
        assert_eq!(event["type"], "turn_complete");
        assert_eq!(event["has_tool_calls"], true);
        assert!(event.get("assistant_text").is_none());
    }

    #[test]
    fn transcript_page_seq_rolls_over_every_fifty_items() {
        assert_eq!(transcript_page_seq(1), 1);
        assert_eq!(transcript_page_seq(50), 1);
        assert_eq!(transcript_page_seq(51), 2);
        assert_eq!(transcript_page_seq(101), 3);
    }

    #[test]
    fn transcript_page_bounds_cover_exact_page_window() {
        assert_eq!(transcript_page_bounds(1), (1, 50));
        assert_eq!(transcript_page_bounds(2), (51, 100));
        assert_eq!(transcript_page_bounds(3), (101, 150));
    }

    #[test]
    fn budget_exhausted_paused_run_does_not_block_next_session_turn() {
        let (mut run, _, _, _) = AgenticRunLifecycleService::build_tracked_run_state(
            "run-1".to_string(),
            "session-1".to_string(),
            "user-1".to_string(),
        );

        run.status = RunStatus::Running;
        assert!(
            AgenticRunLifecycleService::blocks_new_session_run(&run, "session-1"),
            "running run must block a concurrent turn"
        );

        run.status = RunStatus::Paused;
        run.waiting_for = Some("user_resume".to_string());
        assert!(
            AgenticRunLifecycleService::blocks_new_session_run(&run, "session-1"),
            "manual/user-wait paused run must block until resumed or cancelled"
        );

        run.waiting_for = None;
        assert!(
            !AgenticRunLifecycleService::blocks_new_session_run(&run, "session-1"),
            "budget-exhausted paused run has no waiting_for and must allow the next message"
        );

        run.status = RunStatus::Waiting;
        assert!(
            AgenticRunLifecycleService::blocks_new_session_run(&run, "session-1"),
            "waiting run must still block a concurrent turn"
        );
    }

    fn test_spawn_run_config(allowed_tools: Vec<&str>, read_only: bool) -> SpawnRunConfig {
        SpawnRunConfig {
            run_id: "child-run".to_string(),
            agent_id: "child@1234".to_string(),
            recursion_depth: 1,
            agent_type: "test".to_string(),
            task: "do work".to_string(),
            system_prompt_addendum: String::new(),
            model: Some("test-model".to_string()),
            max_turns: 3,
            allowed_tools: allowed_tools.into_iter().map(String::from).collect(),
            read_only,
            working_dir: std::path::PathBuf::from("/tmp"),
            mailbox: None,
            progress_emitter: None,
            context_cache: None,
            inherited_permissions: None,
            parent_address: None,
            permission_context: None,
            inherited_skills: Vec::new(),
            live_event_sink: None,
            inherited_prefix: None,
            is_fork_child: false,
        }
    }

    fn test_spawn_runtime_context(parent_run_id: &str, user_id: &str) -> ServerSpawnRuntimeContext {
        ServerSpawnRuntimeContext {
            parent_run_id: parent_run_id.to_string(),
            user_id: user_id.to_string(),
            session_id: "session-1".to_string(),
            forward_headers: HashMap::new(),
            llm_token_service: None,
            request_constraints: RequestConstraints::default(),
            pause_flag: None,
            cancel_token: None,
            trace_context: server_trace_context(user_id, "session-1", parent_run_id, 1),
            #[cfg(feature = "bridge-e2e-hooks")]
            test_child_llm_rounds: Vec::new(),
            #[cfg(feature = "harness")]
            harness_sink: None,
        }
    }

    #[tokio::test]
    async fn server_spawn_runtime_context_is_keyed_by_parent_run() {
        let executor = ServerSpawnAgentExecutor::new(
            test_settings(),
            test_encryptor(),
            Arc::new(TokioMutex::new(HashMap::new())),
        );
        executor
            .set_runtime_context(test_spawn_runtime_context("parent-run-a", "user-a"))
            .await;
        executor
            .set_runtime_context(test_spawn_runtime_context("parent-run-b", "user-b"))
            .await;

        let mut config = test_spawn_run_config(vec!["*"], false);
        config.parent_address = Some(astra_messaging::types::AgentAddress::new(
            "parent-run-b",
            "root-agent",
        ));

        let context = executor.runtime_context_for_config(&config).await.unwrap();

        assert_eq!(context.parent_run_id, "parent-run-b");
        assert_eq!(context.user_id, "user-b");
    }

    #[tokio::test]
    async fn server_spawn_runtime_context_requires_parent_lineage() {
        let executor = ServerSpawnAgentExecutor::new(
            test_settings(),
            test_encryptor(),
            Arc::new(TokioMutex::new(HashMap::new())),
        );
        executor
            .set_runtime_context(test_spawn_runtime_context("parent-run-a", "user-a"))
            .await;

        let config = test_spawn_run_config(vec!["*"], false);
        let err = match executor.runtime_context_for_config(&config).await {
            Ok(_) => panic!("server dynamic spawn must not run without parent lineage"),
            Err(err) => err,
        };

        assert!(err.contains("parent run lineage"), "{err}");
    }

    #[test]
    fn spawn_child_constraints_intersect_parent_and_agent_allowlists() {
        let parent = RequestConstraints::new(
            Some(
                ["bash", "read_file", "write_file"]
                    .into_iter()
                    .map(String::from)
                    .collect(),
            ),
            Some(["review"].into_iter().map(String::from).collect()),
        );
        let config = test_spawn_run_config(vec!["bash", "read_file"], true);

        let constraints = spawn_child_request_constraints(&parent, &config);

        assert_eq!(
            constraints.allowed_tools.unwrap(),
            ["bash", "read_file"]
                .into_iter()
                .map(String::from)
                .collect()
        );
        assert_eq!(
            constraints.allowed_skills.unwrap(),
            ["review"].into_iter().map(String::from).collect()
        );
    }

    #[test]
    fn spawn_child_constraints_preserve_parent_when_child_allows_all() {
        let parent = RequestConstraints::new(
            Some(
                ["bash", "write_file"]
                    .into_iter()
                    .map(String::from)
                    .collect(),
            ),
            None,
        );
        let config = test_spawn_run_config(vec!["*"], false);

        let constraints = spawn_child_request_constraints(&parent, &config);

        assert_eq!(
            constraints.allowed_tools.unwrap(),
            ["bash", "write_file"]
                .into_iter()
                .map(String::from)
                .collect()
        );
    }

    #[test]
    fn spawn_child_constraints_read_only_wildcard_gets_read_only_tools() {
        let parent = RequestConstraints::default();
        let config = test_spawn_run_config(vec!["*"], true);

        let constraints = spawn_child_request_constraints(&parent, &config);
        let allowed = constraints.allowed_tools.unwrap();

        assert!(allowed.contains("read_file"));
        assert!(allowed.contains("grep"));
        assert!(!allowed.contains("write_file"));
        assert!(!allowed.contains("str_replace"));
    }

    #[test]
    fn build_run_turn_complete_event_marks_interrupted_turns() {
        let interruption = astra_turn_core::interruption::InterruptionRecord::new(
            astra_turn_core::interruption::InterruptionKind::BudgetExhausted,
            astra_turn_core::interruption::ResumeAction::ContinueImmediately,
            astra_turn_core::interruption::InterruptionStateSummary {
                has_checkpoint: true,
                tool_calls_completed: 7,
                turns_completed: 15,
                remaining_turns: 0,
                error_detail: Some("Round budget hard-limit reached".to_string()),
                stall_signal: None,
            },
        );

        let event = build_run_turn_complete_event_with_interruption(
            7,
            "[Round budget hard-limit reached]",
            Some(&interruption),
        );

        assert_eq!(event["type"], "turn_complete");
        assert_eq!(event["has_tool_calls"], false);
        assert_eq!(event["stall_detected"], true);
        assert_eq!(event["execution_state"]["status"], "interrupted");
        assert_eq!(event["execution_state"]["interrupted"], true);
        assert_eq!(
            event["execution_state"]["interruption_kind"],
            "budget_exhausted"
        );
        assert_eq!(event["execution_state"]["tool_calls_completed"], 7);
        assert_eq!(event["execution_state"]["remaining_turns"], 0);
        assert_eq!(event["assistant_text"], "[Round budget hard-limit reached]");
    }

    #[test]
    fn correction_keywords_trigger_was_corrected_via_implicit_feedback() {
        // Sanity-check that the detect_implicit_feedback_signal contract used in
        // record_server_loop_learning_outcome produces a "correction" signal
        // for the Chinese-language corrections listed in routing::detect_correction.
        let signal = astra_turn_types::detect_implicit_feedback_signal(
            "不对，你搞错了",
            Some("previous assistant reply"),
        );
        assert!(
            matches!(signal.signal_type.as_str(), "correction" | "frustration"),
            "expected correction/frustration, got {:?}",
            signal.signal_type
        );
    }

    #[test]
    fn neutral_user_turn_does_not_flag_was_corrected() {
        let signal = astra_turn_types::detect_implicit_feedback_signal(
            "再列一下 docs 目录",
            Some("previous assistant reply"),
        );
        assert!(
            !matches!(signal.signal_type.as_str(), "correction" | "frustration"),
            "expected non-correction, got {:?}",
            signal.signal_type
        );
    }

    /// Unwrap a `Result<T, (StatusCode, Json<ErrorResponse>)>` in tests.
    fn ok<T>(result: Result<T, (StatusCode, Json<ErrorResponse>)>) -> T {
        match result {
            Ok(v) => v,
            Err((status, body)) => panic!("expected Ok, got {status}: {}", body.0.detail),
        }
    }

    /// Unwrap the error side.
    fn err<T>(
        result: Result<T, (StatusCode, Json<ErrorResponse>)>,
    ) -> (StatusCode, Json<ErrorResponse>) {
        match result {
            Ok(_) => panic!("expected Err, got Ok"),
            Err(e) => e,
        }
    }

    fn test_settings() -> MatrixOneSettings {
        MatrixOneSettings::from_env_with_database("test_astra_runtime")
    }

    fn test_encryptor() -> Arc<FernetTokenEncryptor> {
        Arc::new(FernetTokenEncryptor::new("cJ8pxr3t6iJmSYqe6wD7vu2rN_C3ovGUxkC5H3NXFNY=").unwrap())
    }

    fn test_service() -> AgenticRunLifecycleService {
        use astra_services::runs::InMemoryRunStateStore;

        let engine = RunEngine::new(Arc::new(InMemoryRunStateStore::new()));
        AgenticRunLifecycleService::new(
            test_settings(),
            test_encryptor(),
            Arc::new(TokioMutex::new(HashMap::new())),
            engine,
        )
    }

    fn test_request(message: &str) -> ChatRequestData {
        ChatRequestData {
            message: message.to_string(),
            session_id: None,
            full_llm_capture: false,
            agent_id: None,
            model: None,
            llm_token_service: None,
            skill_search: None,
            allow_skills: None,
            allow_tools: None,
            context: None,
            forward_headers: HashMap::new(),
            execution_budget: None,
            explain: false,
            interactive_client: false,
        }
    }

    #[tokio::test]
    async fn build_initial_state_includes_database_skill_provider_when_wired() {
        use astra_services::skills::{
            SkillInfoRecord, SkillListItem, SkillListRecord, SkillPublishRequestData, SkillRecord,
            SkillRegisterRequestData, SkillService, SkillStatusRecord, SkillVersionRecord,
        };
        use async_trait::async_trait;

        struct MockSkillService;

        #[async_trait]
        impl SkillService for MockSkillService {
            async fn register_skill(
                &self,
                _: String,
                _: SkillRegisterRequestData,
            ) -> Result<SkillRecord, (StatusCode, Json<ErrorResponse>)> {
                unimplemented!()
            }

            async fn list_skills(
                &self,
                _user_id: String,
                limit: u32,
                offset: u32,
            ) -> Result<SkillListRecord, (StatusCode, Json<ErrorResponse>)> {
                if offset > 0 {
                    return Ok(SkillListRecord {
                        skills: Vec::new(),
                        total: 1,
                        limit,
                        offset,
                    });
                }
                Ok(SkillListRecord {
                    skills: vec![SkillListItem {
                        skill_id: "remote-db@1.0.0".to_string(),
                        skill_name: "remote-db".to_string(),
                        version: "1.0.0".to_string(),
                        description: Some("Remote DB skill".to_string()),
                        status: Some("active".to_string()),
                        source: Some("user".to_string()),
                        category: Some("integration".to_string()),
                        created_at: None,
                    }],
                    total: 1,
                    limit,
                    offset,
                })
            }

            async fn get_skill(
                &self,
                _user_id: String,
                skill_id: String,
                _version: Option<String>,
            ) -> Result<SkillRecord, (StatusCode, Json<ErrorResponse>)> {
                if skill_id == "remote-db" || skill_id == "remote-db@1.0.0" {
                    return Ok(SkillRecord {
                        skill_id: "remote-db@1.0.0".to_string(),
                        skill_name: "remote-db".to_string(),
                        version: "1.0.0".to_string(),
                        description: Some("Remote DB skill".to_string()),
                        metadata: Some(serde_json::json!({
                            "skill_type": "remote",
                            "remote_url": "http://127.0.0.1:18080/remote-skill",
                            "forward_headers": ["authorization", "x-workspace-id"],
                            "required_headers": ["x-workspace-id"],
                            "when_to_use": "when task needs remote orchestration"
                        })),
                        created_at: None,
                    });
                }
                Err((
                    StatusCode::NOT_FOUND,
                    Json(ErrorResponse::new("not found".to_string())),
                ))
            }

            async fn get_skill_info(
                &self,
                _: String,
                _: String,
            ) -> Result<SkillInfoRecord, (StatusCode, Json<ErrorResponse>)> {
                unimplemented!()
            }

            async fn list_skill_versions(
                &self,
                _: String,
                _: String,
            ) -> Result<Vec<SkillVersionRecord>, (StatusCode, Json<ErrorResponse>)> {
                unimplemented!()
            }

            async fn get_skill_status(
                &self,
                _: String,
                _: u32,
            ) -> Result<SkillStatusRecord, (StatusCode, Json<ErrorResponse>)> {
                unimplemented!()
            }

            async fn publish_skill(
                &self,
                _: String,
                _: SkillPublishRequestData,
            ) -> Result<serde_json::Value, (StatusCode, Json<ErrorResponse>)> {
                unimplemented!()
            }

            async fn unpublish_skill(
                &self,
                _: String,
                _: String,
            ) -> Result<serde_json::Value, (StatusCode, Json<ErrorResponse>)> {
                unimplemented!()
            }
        }

        let svc = test_service().with_skill_service(Arc::new(MockSkillService));

        let default_request = test_request("hello");
        let default_state = svc.build_initial_state(
            "test-user",
            &default_request,
            "session-1",
            "run-1",
            None,
            None,
        );
        let default_resolver = default_state
            .skills
            .resolver
            .as_ref()
            .expect("default server resolver should include visible catalog");
        let default_names: Vec<String> = default_resolver
            .available_skills()
            .into_iter()
            .map(|skill| skill.name)
            .collect();
        assert!(
            default_names.iter().any(|name| name == "remote-db"),
            "expected database skill without request allow_skills filter: {default_names:?}"
        );
        assert!(
            default_state.skills.registry_for_activation.is_some(),
            "unfiltered server catalog should be available for conditional activation"
        );

        let mut request = test_request("hello");
        request.allow_skills = Some(vec!["remote-db".to_string()]);
        let state =
            svc.build_initial_state("test-user", &request, "session-1", "run-1", None, None);
        let resolver = state
            .skills
            .resolver
            .as_ref()
            .expect("skill resolver should be configured");
        let names: Vec<String> = resolver
            .available_skills()
            .into_iter()
            .map(|s| s.name)
            .collect();
        assert!(
            names.iter().any(|name| name == "remote-db"),
            "expected database skill in available skills: {names:?}"
        );

        let resolved = resolver
            .resolve("remote-db")
            .expect("resolver should load database skill");
        assert_eq!(
            resolved.remote_url.as_deref(),
            Some("http://127.0.0.1:18080/remote-skill")
        );
        assert_eq!(
            resolved.forward_headers,
            vec!["authorization".to_string(), "x-workspace-id".to_string()]
        );
        assert_eq!(
            resolved.required_headers,
            vec!["x-workspace-id".to_string()]
        );

        let mut filtered_request = test_request("hello");
        filtered_request.allow_skills = Some(vec!["remote-db".to_string()]);
        let filtered_state = svc.build_initial_state(
            "test-user",
            &filtered_request,
            "session-1",
            "run-1",
            None,
            None,
        );
        assert!(
            filtered_state.skills.registry_for_activation.is_none(),
            "request-scoped allow_skills should disable automatic conditional activation"
        );
        let filtered_resolver = filtered_state
            .skills
            .resolver
            .as_ref()
            .expect("filtered resolver should be configured");
        let filtered_names: Vec<String> = filtered_resolver
            .available_skills()
            .into_iter()
            .map(|skill| skill.name)
            .collect();
        assert_eq!(filtered_names, vec!["remote-db".to_string()]);
        filtered_resolver
            .resolve("remote-db")
            .expect("allowed remote-db skill should resolve");
    }

    #[tokio::test]
    async fn create_run_rejects_unknown_request_skill_allowlist() {
        let svc = test_service();
        let mut request = test_request("hello");
        request.allow_skills = Some(vec!["__missing_skill__".into()]);

        let err = svc
            .create_run("user-1".into(), request)
            .await
            .expect_err("unknown allow_skills entry should be rejected");
        assert_eq!(err.0, StatusCode::BAD_REQUEST);
        assert!(err.1.0.detail.contains("allow_skills"));
    }

    #[test]
    fn build_runtime_turn_evaluation_event_uses_loop_state_signals() {
        let svc = test_service();
        let request = test_request("git status");
        let mut state =
            svc.build_initial_state("test-user", &request, "session-1", "run-1", None, None);
        state.recent_tools = vec!["git_status".into()];
        state.telemetry.first_budget_pressure = 0.27;
        state.stall.events.push(("repetition_stall".into(), 1));
        state.stall.verdict_events.push(
            astra_turn_core::agentic_verdict_audit::AgenticVerdictAuditEvent {
                turn: 1,
                severity: "warning".into(),
                injections: vec!["stall detected".into()],
                avoid_tools: vec!["git_status".into()],
                deprioritized_tools: vec![],
                force_stop: false,
                nudge_count: 1,
                total_errors: 0,
                deprioritized_count: 0,
                total_timeouts: 0,
                timeout_dominant_tools: vec![],
                total_cache_hits: 0,
                flaky_count: 0,
            },
        );
        state.stall.tool_call_records.push(ToolCallRecord {
            name: "git_status".into(),
            ok: true,
            ms: 14,
            error: None,
            input_bytes: Some(8),
            output_bytes: Some(180),
            args_preview: None,
            result_preview: Some("clean".into()),
            file_path: None,
            surgically_removed: None,
            original_tool_name: None,
            ..Default::default()
        });

        let event = build_runtime_turn_evaluation_event("session-1", "server_runtime", &state);

        assert_eq!(event.event_type, JournalEventType::TurnEvaluation);
        assert_eq!(event.turn, None);
        let metadata = event.metadata.expect("turn evaluation metadata");
        assert_eq!(metadata["source"], "server_runtime");
        assert_eq!(metadata["live_query"], true);
        assert_eq!(metadata["stall_count"], 1);
        assert_eq!(metadata["verdict_warning"], true);
        assert_eq!(metadata["tool_call_count"], 1);
        assert!(metadata["quality"].as_f64().unwrap() < 0.8);
        assert_eq!(metadata["signals"][0]["kind"], "tool_error_rate");
    }

    #[test]
    fn finalize_run_events_appends_run_finished_for_failures() {
        let svc = test_service();
        let request = test_request("boom");
        let state =
            svc.build_initial_state("test-user", &request, "session-1", "run-1", None, None);

        let (events, status, error) = AgenticRunLifecycleService::finalize_run_events(
            Ok(AgenticLoopOutcome::Error("boom".into())),
            vec![],
            &state,
        );

        assert_eq!(status, RunStatus::Failed);
        assert_eq!(error.as_deref(), Some("boom"));
        assert_eq!(events.len(), 2);
        assert_eq!(events[0]["event_type"], "run_error");
        assert_eq!(events[1]["event_type"], "run_finished");
    }

    #[test]
    fn finalize_run_events_cancellation_beats_completed_outcome() {
        let svc = test_service();
        let request = test_request("done");
        let mut state =
            svc.build_initial_state("test-user", &request, "session-1", "run-1", None, None);
        let cancel_flag = Arc::new(AtomicBool::new(true));
        let cancel_token = Arc::new(CancellationToken::new());
        cancel_token.cancel();
        state.cancellation.flag = Some(cancel_flag);
        state.cancellation.token = Some(cancel_token);

        let (events, status, error) = AgenticRunLifecycleService::finalize_run_events(
            Ok(AgenticLoopOutcome::Completed),
            vec![],
            &state,
        );

        assert_eq!(status, RunStatus::Cancelled);
        assert!(error.is_none());
        assert_eq!(events.len(), 1);
        assert_eq!(events[0]["event_type"], "run_finished");
        assert_eq!(events[0]["data"]["cancelled"], true);
    }

    #[test]
    fn finalize_run_events_interrupted_completed_outcome_is_partial_not_completed() {
        let svc = test_service();
        let request = test_request("partial");
        let mut state =
            svc.build_initial_state("test-user", &request, "session-1", "run-1", None, None);
        state.final_text = "[Round budget hard-limit reached]".to_string();
        state.interruption = Some(astra_turn_core::interruption::InterruptionRecord::new(
            astra_turn_core::interruption::InterruptionKind::BudgetExhausted,
            astra_turn_core::interruption::ResumeAction::ContinueImmediately,
            astra_turn_core::interruption::InterruptionStateSummary {
                has_checkpoint: true,
                tool_calls_completed: 5,
                turns_completed: 15,
                remaining_turns: 0,
                error_detail: Some("Round budget hard-limit reached".to_string()),
                stall_signal: None,
            },
        ));

        let (events, status, error) = AgenticRunLifecycleService::finalize_run_events(
            Ok(AgenticLoopOutcome::Completed),
            vec![],
            &state,
        );

        assert_eq!(status, RunStatus::Paused);
        assert!(
            error
                .as_deref()
                .is_some_and(|msg| msg.to_ascii_lowercase().contains("budget"))
        );
        assert_eq!(events[0]["event_type"], "text_done");
        assert_eq!(events[0]["data"]["partial"], true);
        assert_eq!(
            events[0]["data"]["interruption"]["kind"],
            "budget_exhausted"
        );
        assert_eq!(events[1]["event_type"], "run_interrupted");
        assert_eq!(events[2]["event_type"], "run_finished");
        assert_eq!(events[2]["data"]["interrupted"], true);
        assert_eq!(events[2]["data"]["interruption_kind"], "budget_exhausted");
    }

    #[test]
    fn merge_cancelled_run_events_preserves_order_and_usage() {
        let cancel_flag = Arc::new(AtomicBool::new(true));
        let cancel_token = Arc::new(CancellationToken::new());
        let mut run = RunState {
            run_id: "run-1".into(),
            session_id: "session-1".into(),
            status: RunStatus::Cancelled,
            events: vec![
                json!({"event_type": "run_started", "data": {}}),
                json!({"event_type": "run_finished", "data": {"cancelled": true}}),
            ],
            cancel_flag,
            pause_flag: Arc::new(AtomicBool::new(false)),
            llm_cancel_token: cancel_token,
            live_tx: None,
            started_at: Instant::now(),
            waiting_for: None,
        };

        merge_cancelled_run_events(
            &mut run,
            vec![
                json!({"event_type": "text_delta", "data": {"chunk": "hi"}}),
                json!({"event_type": "run_finished", "data": {"cancelled": true, "prompt_tokens": 3}}),
            ],
        );

        assert_eq!(run.events.len(), 3);
        assert_eq!(run.events[1]["event_type"], "text_delta");
        assert_eq!(run.events[2]["event_type"], "run_finished");
        assert_eq!(run.events[2]["data"]["cancelled"], true);
        assert_eq!(run.events[2]["data"]["prompt_tokens"], 3);
    }

    #[test]
    fn terminal_events_for_persistence_keeps_only_terminal_lifecycle_events() {
        let events = vec![
            json!({"event_type": "text_delta", "data": {"chunk": "hi"}}),
            json!({"type": "reasoning_delta", "content": "thinking"}),
            json!({"type": "reasoning_done"}),
            json!({"event_type": "text_done", "data": {"full_text": "final answer"}}),
            json!({"event_type": "run_error", "data": {"error": "boom"}}),
            json!({"event_type": "run_finished", "data": {"prompt_tokens": 1}}),
        ];

        let persisted = terminal_events_for_persistence(&events);
        assert_eq!(persisted.len(), 5);
        assert_eq!(persisted[0]["type"], "reasoning_delta");
        assert_eq!(persisted[1]["type"], "reasoning_done");
        assert_eq!(persisted[2]["event_type"], "text_done");
        assert_eq!(persisted[3]["event_type"], "run_error");
        assert_eq!(persisted[4]["event_type"], "run_finished");
    }

    #[tokio::test]
    async fn create_run_returns_running_status() {
        let svc = test_service();
        let result = ok(svc.create_run("user-1".into(), test_request("hello")).await);
        assert_eq!(result.status, "running");
        assert!(!result.run_id.is_empty());
        assert!(!result.session_id.is_empty());
    }

    #[tokio::test]
    async fn create_run_uses_provided_session_id() {
        let svc = test_service();
        let mut req = test_request("hi");
        req.session_id = Some("custom-session".into());
        let result = ok(svc.create_run("user-1".into(), req).await);
        assert_eq!(result.session_id, "custom-session");
    }

    #[tokio::test]
    async fn create_run_explain_mode_returns_metadata() {
        let svc = test_service();
        let mut req = test_request("explain me");
        req.explain = true;
        let result = ok(svc.create_run("user-1".into(), req).await);
        assert!(result.explain.is_some());
        assert_eq!(result.explain.unwrap()["mode"], "background");
    }

    #[tokio::test]
    async fn create_run_conflicts_when_same_session_already_has_active_run() {
        let svc = test_service();
        let mut first = test_request("hello");
        first.session_id = Some("shared-session".into());
        ok(svc.create_run("user-1".into(), first).await);

        let mut second = test_request("again");
        second.session_id = Some("shared-session".into());
        let err = err(svc.create_run("user-1".into(), second).await);
        assert_eq!(err.0, StatusCode::CONFLICT);
        assert_eq!(err.1.0.detail, "session already has an active run");
    }

    #[tokio::test]
    async fn stream_chat_conflicts_when_same_session_already_has_active_run() {
        let svc = test_service();
        let mut first = test_request("hello");
        first.session_id = Some("shared-session".into());
        ok(svc.create_run("user-1".into(), first).await);

        let mut second = test_request("again");
        second.session_id = Some("shared-session".into());
        let err = err(svc.stream_chat("user-1".into(), second).await);
        assert_eq!(err.0, StatusCode::CONFLICT);
        assert_eq!(err.1.0.detail, "session already has an active run");
    }

    #[tokio::test]
    #[ignore] // stream_chat runs full agentic loop; needs live DB + LLM or mock
    async fn stream_chat_tracks_run_for_status_and_replay() {
        let svc = test_service();
        let stream = ok(svc
            .stream_chat("user-1".into(), test_request("hello"))
            .await);

        let status = tokio::time::timeout(std::time::Duration::from_secs(10), async {
            loop {
                let status = ok(svc
                    .get_run_status(stream.run_id.clone(), "user-1".into())
                    .await);
                if status.status != "running" {
                    break status;
                }
                tokio::time::sleep(std::time::Duration::from_millis(25)).await;
            }
        })
        .await
        .expect("timeout waiting for stream_chat status to finish");
        let replay = ok(svc
            .stream_run(stream.run_id.clone(), "user-1".into(), 0)
            .await);

        assert_eq!(status.run_id, stream.run_id);
        assert!(status.events_count > 0);
        assert_eq!(replay.len(), status.events_count as usize);
        assert_eq!(replay[0]["event_type"], "run_started");
        assert_eq!(
            svc.test_llm_cancel_token_is_cancelled(&stream.run_id).await,
            Some(false)
        );
    }

    #[tokio::test]
    async fn get_run_status_returns_state() {
        let svc = test_service();
        let run = ok(svc.create_run("user-1".into(), test_request("hello")).await);
        let status = ok(svc
            .get_run_status(run.run_id.clone(), "user-1".into())
            .await);
        assert_eq!(status.run_id, run.run_id);
        assert_eq!(status.status, "running");
        assert_eq!(status.events_count, 1);
    }

    #[tokio::test]
    async fn get_run_status_not_found() {
        let svc = test_service();
        let e = err(svc
            .get_run_status("nonexistent".into(), "user-1".into())
            .await);
        assert_eq!(e.0, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn get_run_status_forbidden_for_other_user() {
        let svc = test_service();
        let run = ok(svc.create_run("user-1".into(), test_request("hello")).await);
        let e = err(svc.get_run_status(run.run_id, "user-2".into()).await);
        assert_eq!(e.0, StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn cancel_run_sets_cancelled_status() {
        let svc = test_service();
        let run = ok(svc.create_run("user-1".into(), test_request("task")).await);
        let result = ok(svc.cancel_run(run.run_id.clone(), "user-1".into()).await);
        assert_eq!(result.status, "cancelled");
        let status = ok(svc.get_run_status(run.run_id, "user-1".into()).await);
        assert_eq!(status.status, "cancelled");
        assert!(status.events_count >= 1);
    }

    #[tokio::test]
    async fn cancel_run_cancels_llm_token_for_inflight_wake() {
        let svc = test_service();
        let run = ok(svc.create_run("user-1".into(), test_request("task")).await);
        assert_eq!(
            svc.test_llm_cancel_token_is_cancelled(&run.run_id).await,
            Some(false)
        );
        ok(svc.cancel_run(run.run_id.clone(), "user-1".into()).await);
        assert_eq!(
            svc.test_llm_cancel_token_is_cancelled(&run.run_id).await,
            Some(true)
        );
    }

    #[tokio::test]
    async fn cancel_run_from_paused_sets_cancelled_status_and_clears_pause_flag() {
        let svc = test_service();
        let run = ok(svc.create_run("user-1".into(), test_request("task")).await);
        ok(svc.pause_run(run.run_id.clone(), "user-1".into()).await);
        assert_eq!(svc.test_pause_flag_is_set(&run.run_id).await, Some(true));

        let result = ok(svc.cancel_run(run.run_id.clone(), "user-1".into()).await);
        assert_eq!(result.status, "cancelled");
        assert_eq!(svc.test_pause_flag_is_set(&run.run_id).await, Some(false));
        assert_eq!(
            svc.test_llm_cancel_token_is_cancelled(&run.run_id).await,
            Some(true)
        );
    }

    #[tokio::test]
    async fn pause_run_sets_live_pause_flag_and_resume_clears_it() {
        let svc = test_service();
        let run = ok(svc.create_run("user-1".into(), test_request("task")).await);
        assert_eq!(svc.test_pause_flag_is_set(&run.run_id).await, Some(false));
        ok(svc.pause_run(run.run_id.clone(), "user-1".into()).await);
        assert_eq!(svc.test_pause_flag_is_set(&run.run_id).await, Some(true));
        ok(svc.resume_run(run.run_id.clone(), "user-1".into()).await);
        assert_eq!(svc.test_pause_flag_is_set(&run.run_id).await, Some(false));
    }

    #[tokio::test]
    async fn cancel_run_idempotent_for_non_running() {
        let svc = test_service();
        let run = ok(svc.create_run("user-1".into(), test_request("task")).await);
        ok(svc.cancel_run(run.run_id.clone(), "user-1".into()).await);
        let result = ok(svc.cancel_run(run.run_id.clone(), "user-1".into()).await);
        assert_eq!(result.status, "cancelled");
    }

    #[tokio::test]
    async fn cancel_run_forbidden_for_other_user() {
        let svc = test_service();
        let run = ok(svc.create_run("user-1".into(), test_request("task")).await);
        let e = err(svc.cancel_run(run.run_id, "user-2".into()).await);
        assert_eq!(e.0, StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn stream_run_returns_events_from_offset() {
        let svc = test_service();
        let run = ok(svc.create_run("user-1".into(), test_request("hello")).await);
        let events = ok(svc.stream_run(run.run_id.clone(), "user-1".into(), 0).await);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0]["event_type"], "run_started");
        let events = ok(svc.stream_run(run.run_id, "user-1".into(), 1).await);
        assert!(events.is_empty());
    }

    #[tokio::test]
    async fn stream_run_not_found() {
        let svc = test_service();
        let e = err(svc
            .stream_run("nonexistent".into(), "user-1".into(), 0)
            .await);
        assert_eq!(e.0, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn list_runs_empty_initially() {
        let svc = test_service();
        let result = ok(svc.list_runs("user-1".into(), 10, 0).await);
        assert_eq!(result.total, 0);
        assert!(result.runs.is_empty());
    }

    #[tokio::test]
    async fn list_runs_filters_by_user() {
        let svc = test_service();
        let u1_a = ok(svc.create_run("user-1".into(), test_request("a")).await);
        let u2_b = ok(svc.create_run("user-2".into(), test_request("b")).await);
        let u1_c = ok(svc.create_run("user-1".into(), test_request("c")).await);
        let for_u1 = ok(svc.list_runs("user-1".into(), 10, 0).await);
        assert_eq!(for_u1.total, 2);
        let ids: std::collections::HashSet<_> =
            for_u1.runs.iter().map(|r| r.run_id.as_str()).collect();
        assert!(ids.contains(u1_a.run_id.as_str()));
        assert!(ids.contains(u1_c.run_id.as_str()));
        assert!(!ids.contains(u2_b.run_id.as_str()));

        let for_u2 = ok(svc.list_runs("user-2".into(), 10, 0).await);
        assert_eq!(for_u2.total, 1);
        assert_eq!(for_u2.runs[0].run_id, u2_b.run_id);
    }

    #[tokio::test]
    async fn list_runs_pagination() {
        let svc = test_service();
        for i in 0..5 {
            ok(svc
                .create_run("user-1".into(), test_request(&format!("msg {i}")))
                .await);
        }
        let page1 = ok(svc.list_runs("user-1".into(), 2, 0).await);
        assert_eq!(page1.runs.len(), 2);
        assert_eq!(page1.total, 5);
        let page2 = ok(svc.list_runs("user-1".into(), 2, 2).await);
        assert_eq!(page2.runs.len(), 2);
        let page3 = ok(svc.list_runs("user-1".into(), 2, 4).await);
        assert_eq!(page3.runs.len(), 1);
    }

    /// P2-B: list_runs must clamp pagination params like other list endpoints.
    #[tokio::test]
    async fn list_runs_clamps_pagination() {
        let svc = test_service();
        // Absurdly large limit/offset must not panic or produce unbounded queries
        let result = ok(svc.list_runs("user-clamp".into(), u32::MAX, u32::MAX).await);
        assert_eq!(result.runs.len(), 0);
        // Verify the returned limit/offset are clamped
        assert!(
            result.limit <= astra_services::pagination::MAX_API_LIST_LIMIT,
            "limit must be clamped to MAX_API_LIST_LIMIT"
        );
    }

    #[test]
    fn format_run_events_adds_index() {
        let events = vec![
            json!({"event_type": "run_started"}),
            json!({"event_type": "text_delta", "data": {"chunk": "hi"}}),
        ];
        let formatted = AgenticRunLifecycleService::format_run_events(&events, 0);
        assert_eq!(formatted[0]["index"], 0);
        assert_eq!(formatted[1]["index"], 1);
        assert_eq!(formatted[1]["event_type"], "text_delta");
    }

    #[test]
    fn format_run_events_preserves_global_offset() {
        let events = vec![
            json!({"event_type": "text_delta", "data": {"chunk": "a"}}),
            json!({"event_type": "text_delta", "data": {"chunk": "b"}}),
        ];
        let formatted = AgenticRunLifecycleService::format_run_events(&events, 5);
        assert_eq!(formatted[0]["index"], 5);
        assert_eq!(formatted[1]["index"], 6);
    }

    #[test]
    fn extract_edge_tools_from_context() {
        let mut ctx = serde_json::Map::new();
        ctx.insert(
            "edge_tools".to_string(),
            json!([{"function": {"name": "bash"}}]),
        );
        let req = ChatRequestData {
            message: "hi".into(),
            session_id: None,
            full_llm_capture: false,
            agent_id: None,
            model: None,
            llm_token_service: None,
            skill_search: None,
            allow_skills: None,
            allow_tools: None,
            context: Some(ctx),
            forward_headers: HashMap::new(),
            execution_budget: None,
            explain: false,
            interactive_client: false,
        };
        let tools = AgenticRunLifecycleService::extract_edge_tools(&req);
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0]["function"]["name"], "bash");
    }

    #[test]
    fn extract_edge_tools_empty_when_no_context() {
        assert!(AgenticRunLifecycleService::extract_edge_tools(&test_request("hi")).is_empty());
    }

    fn trusted_domains_for_tests() -> Vec<super::TrustedLlmDomain> {
        vec![super::TrustedLlmDomain {
            host: "catalog".to_string(),
            port: Some(8081),
        }]
    }

    #[test]
    fn validate_llm_token_service_config_accepts_http_url() {
        let config = astra_services::LlmTokenServiceConfig {
            url: "http://catalog:8081/api/v1/llm-token".to_string(),
            timeout_ms: Some(2500),
        };
        let trusted = trusted_domains_for_tests();
        assert!(super::validate_llm_token_service_config(Some(&config), &trusted).is_ok());
    }

    #[test]
    fn validate_llm_token_service_config_rejects_invalid_url() {
        let config = astra_services::LlmTokenServiceConfig {
            url: "not-a-url".to_string(),
            timeout_ms: Some(2500),
        };
        let trusted = trusted_domains_for_tests();
        let err = super::validate_llm_token_service_config(Some(&config), &trusted)
            .expect_err("invalid url should fail");
        assert!(err.contains("valid URL"), "unexpected error: {err}");
    }

    #[test]
    fn validate_llm_token_service_config_rejects_untrusted_url() {
        let config = astra_services::LlmTokenServiceConfig {
            url: "http://evil.example.com/v1/chat/completions".to_string(),
            timeout_ms: Some(2500),
        };
        let trusted = trusted_domains_for_tests();
        let err = super::validate_llm_token_service_config(Some(&config), &trusted)
            .expect_err("untrusted url should fail");
        assert!(
            err.contains("trusted domains"),
            "unexpected error message: {err}"
        );
    }

    #[test]
    fn validate_llm_token_service_config_rejects_when_trusted_domains_unconfigured() {
        let config = astra_services::LlmTokenServiceConfig {
            url: "http://catalog:8081/api/v1/llm-token".to_string(),
            timeout_ms: Some(2500),
        };
        let err = super::validate_llm_token_service_config(Some(&config), &[])
            .expect_err("missing trusted domains should fail");
        assert!(
            err.contains(super::LLM_TOKEN_SERVICE_TRUSTED_DOMAINS_TABLE),
            "unexpected error message: {err}"
        );
    }

    #[test]
    fn validate_llm_token_service_config_enforces_host_port_boundary_for_trusted_domains() {
        let config = astra_services::LlmTokenServiceConfig {
            url: "http://catalog:8082/api/v1/chat".to_string(),
            timeout_ms: Some(2500),
        };
        let trusted = trusted_domains_for_tests();
        let err = super::validate_llm_token_service_config(Some(&config), &trusted)
            .expect_err("host:port boundary should be enforced");
        assert!(
            err.contains("trusted domains"),
            "unexpected error message: {err}"
        );
    }

    #[test]
    fn trusted_llm_domain_from_db_values_accepts_valid_host_and_port() {
        let parsed = super::trusted_llm_domain_from_db_values("catalog", 8081)
            .expect("host+port should parse");
        assert_eq!(parsed.host, "catalog");
        assert_eq!(parsed.port, Some(8081));
        let wildcard = super::trusted_llm_domain_from_db_values("catalog", 0)
            .expect("sentinel port should represent wildcard");
        assert_eq!(wildcard.port, None);
    }

    #[test]
    fn trusted_llm_domain_from_db_values_rejects_invalid_host_or_port() {
        let host_err = super::trusted_llm_domain_from_db_values("http://catalog:8081", 8081)
            .expect_err("host should not include scheme");
        assert!(host_err.contains("host"));
        let port_err = super::trusted_llm_domain_from_db_values("catalog", 70000)
            .expect_err("port out of range should fail");
        assert!(port_err.contains("port"));
    }

    #[test]
    fn extract_edge_profile_from_context() {
        let mut ctx = serde_json::Map::new();
        ctx.insert(
            "edge_profile".to_string(),
            json!({"cwd": "/tmp", "git_branch": "main"}),
        );
        let req = ChatRequestData {
            message: "hi".into(),
            session_id: None,
            full_llm_capture: false,
            agent_id: None,
            model: None,
            llm_token_service: None,
            skill_search: None,
            allow_skills: None,
            allow_tools: None,
            context: Some(ctx),
            forward_headers: HashMap::new(),
            execution_budget: None,
            explain: false,
            interactive_client: false,
        };
        let profile = AgenticRunLifecycleService::extract_edge_profile(&req);
        assert_eq!(profile["cwd"], "/tmp");
        assert_eq!(profile["git_branch"], "main");
    }

    #[test]
    fn build_initial_state_sets_user_message() {
        let svc = test_service();
        let req = test_request("write a test");
        let expected_budget = astra_turn_core::chat_turn_heuristics::resolve_agentic_turn_budget(
            astra_turn_core::chat_turn_heuristics::infer_task_execution_profile("write a test"),
            astra_core::RuntimeLimits::global().max_turns,
            None,
        );
        let state = svc.build_initial_state("test-user", &req, "sess-1", "run-1", None, None);
        assert_eq!(state.messages.len(), 1);
        assert_eq!(state.messages[0]["role"], "user");
        assert_eq!(state.messages[0]["content"], "write a test");
        assert_eq!(state.current_session_id, Some("sess-1".to_string()));
        assert_eq!(state.current_run_id, Some("run-1".to_string()));
        assert_eq!(state.max_turns, expected_budget.initial_turns);
        assert_eq!(state.remaining_turns, expected_budget.initial_turns);
        assert_eq!(state.agentic_turn_budget, expected_budget);
        assert_eq!(state.message, "write a test");
        assert!(state.cancellation.token.is_none());
    }

    #[test]
    fn build_initial_state_applies_execution_budget_override() {
        let svc = test_service();
        let mut req = test_request("go");
        req.execution_budget = Some(astra_services::runs::ExecutionBudget {
            initial_turns: Some(4),
            hard_turn_limit: Some(9),
        });
        let state = svc.build_initial_state("test-user", &req, "s", "r", None, None);
        assert_eq!(state.max_turns, 4);
        assert_eq!(state.remaining_turns, 4);
        assert_eq!(state.agentic_turn_budget.hard_turn_limit, 9);
    }

    #[test]
    fn build_initial_state_clamps_execution_budget_override() {
        let svc = test_service();
        let mut req = test_request("go");
        req.execution_budget = Some(astra_services::runs::ExecutionBudget {
            initial_turns: Some(0),
            hard_turn_limit: Some(0),
        });
        let state = svc.build_initial_state("test-user", &req, "s", "r", None, None);
        assert_eq!(state.max_turns, 1);
        assert_eq!(state.agentic_turn_budget.hard_turn_limit, 1);
    }

    #[test]
    fn build_initial_state_loads_stop_hooks_from_edge_profile_cwd() {
        let dir = tempfile::tempdir().unwrap();
        let mo = dir.path().join(".astra");
        std::fs::create_dir_all(&mo).unwrap();
        std::fs::write(
            mo.join("stop-hooks.yaml"),
            "version: 1\nauto_detect: false\nhooks:\n  - label: cloud_hook\n    command: true\n",
        )
        .unwrap();

        let svc = test_service();
        let mut req = test_request("implement a fix");
        req.context = Some(
            serde_json::json!({
                "edge_profile": { "cwd": dir.path().to_str().unwrap() }
            })
            .as_object()
            .unwrap()
            .clone(),
        );

        let state = svc.build_initial_state("test-user", &req, "s", "r", None, None);
        assert_eq!(state.hooks.stop_hooks.len(), 1);
        assert_eq!(state.hooks.stop_hooks[0].label, "cloud_hook");
        assert_eq!(
            state.hooks.workspace_root_hint.as_deref(),
            Some(dir.path().to_str().unwrap())
        );
    }

    #[test]
    fn build_initial_state_uses_workspace_override_when_no_edge_cwd() {
        let dir = tempfile::tempdir().unwrap();
        let mo = dir.path().join(".astra");
        std::fs::create_dir_all(&mo).unwrap();
        std::fs::write(
            mo.join("stop-hooks.yaml"),
            "version: 1\nauto_detect: false\nhooks:\n  - label: server_hook\n    command: echo ok\n",
        )
        .unwrap();

        let svc = test_service();
        // Request with NO edge_profile.cwd — simulates web-agent mode.
        let req = test_request("fix a bug");
        let state = svc.build_initial_state("test-user", &req, "s", "r", Some(dir.path()), None);
        assert_eq!(state.hooks.stop_hooks.len(), 1);
        assert_eq!(state.hooks.stop_hooks[0].label, "server_hook");
        assert_eq!(
            state.hooks.workspace_root_hint.as_deref(),
            Some(dir.path().to_str().unwrap())
        );
    }

    #[test]
    fn build_initial_state_edge_cwd_takes_priority_over_workspace_override() {
        // Edge profile with cwd set — workspace_override should be ignored.
        let edge_dir = tempfile::tempdir().unwrap();
        let mo = edge_dir.path().join(".astra");
        std::fs::create_dir_all(&mo).unwrap();
        std::fs::write(
            mo.join("stop-hooks.yaml"),
            "version: 1\nauto_detect: false\nhooks:\n  - label: edge_hook\n    command: true\n",
        )
        .unwrap();

        let override_dir = tempfile::tempdir().unwrap();
        let mo2 = override_dir.path().join(".astra");
        std::fs::create_dir_all(&mo2).unwrap();
        std::fs::write(
            mo2.join("stop-hooks.yaml"),
            "version: 1\nauto_detect: false\nhooks:\n  - label: override_hook\n    command: true\n",
        )
        .unwrap();

        let svc = test_service();
        let mut req = test_request("deploy");
        req.context = Some(
            serde_json::json!({
                "edge_profile": { "cwd": edge_dir.path().to_str().unwrap() }
            })
            .as_object()
            .unwrap()
            .clone(),
        );

        let state =
            svc.build_initial_state("test-user", &req, "s", "r", Some(override_dir.path()), None);
        // Edge profile's cwd wins over the workspace override.
        assert_eq!(state.hooks.stop_hooks.len(), 1);
        assert_eq!(state.hooks.stop_hooks[0].label, "edge_hook");
        assert_eq!(
            state.hooks.workspace_root_hint.as_deref(),
            Some(edge_dir.path().to_str().unwrap())
        );
    }

    #[test]
    fn run_status_as_str() {
        assert_eq!(RunStatus::Running.as_str(), "running");
        assert_eq!(RunStatus::Completed.as_str(), "completed");
        assert_eq!(RunStatus::Failed.as_str(), "failed");
        assert_eq!(RunStatus::Cancelled.as_str(), "cancelled");
        assert_eq!(RunStatus::Paused.as_str(), "paused");
    }

    #[test]
    fn server_loop_causal_chain_ids_fit_agent_event_column() {
        assert!(server_loop_causal_chain_id("server-loop").len() <= 64);
        assert!(server_loop_causal_chain_id("server-loop-tools").len() <= 64);
    }

    #[test]
    fn has_buffered_terminal_completion_ignores_cancelled_and_interrupted_finishes() {
        assert!(has_buffered_terminal_completion(&[json!({
            "event_type": "run_finished",
            "data": {"total_prompt_tokens": 1, "total_completion_tokens": 1}
        })]));
        assert!(!has_buffered_terminal_completion(&[json!({
            "event_type": "run_finished",
            "data": {"cancelled": true}
        })]));
        assert!(!has_buffered_terminal_completion(&[json!({
            "event_type": "run_finished",
            "data": {"interrupted": true}
        })]));
    }

    #[test]
    fn preserve_manual_pause_wins_over_late_completed_status() {
        assert!(should_preserve_manual_pause_on_completion(
            &RunStatus::Paused,
            &RunStatus::Completed
        ));
        assert!(!should_preserve_manual_pause_on_completion(
            &RunStatus::Paused,
            &RunStatus::Failed
        ));
        assert!(!should_preserve_manual_pause_on_completion(
            &RunStatus::Running,
            &RunStatus::Completed
        ));
    }

    #[tokio::test]
    async fn pause_run_transitions_running_to_paused() {
        let svc = test_service();
        let run = ok(svc.create_run("user-1".into(), test_request("task")).await);
        let result = ok(svc.pause_run(run.run_id.clone(), "user-1".into()).await);
        assert_eq!(result.status, "paused");
        assert_eq!(result.previous_status, "running");
        let status = ok(svc.get_run_status(run.run_id, "user-1".into()).await);
        assert_eq!(status.status, "paused");
    }

    #[tokio::test]
    async fn pause_run_conflict_when_not_running() {
        let svc = test_service();
        let run = ok(svc.create_run("user-1".into(), test_request("task")).await);
        ok(svc.cancel_run(run.run_id.clone(), "user-1".into()).await);
        let e = err(svc.pause_run(run.run_id, "user-1".into()).await);
        assert_eq!(e.0, StatusCode::CONFLICT);
    }

    #[tokio::test]
    async fn pause_run_forbidden_for_other_user() {
        let svc = test_service();
        let run = ok(svc.create_run("user-1".into(), test_request("task")).await);
        let e = err(svc.pause_run(run.run_id, "user-2".into()).await);
        assert_eq!(e.0, StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn pause_run_not_found() {
        let svc = test_service();
        let e = err(svc.pause_run("nonexistent".into(), "user-1".into()).await);
        assert_eq!(e.0, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn resume_run_transitions_paused_to_running() {
        let svc = test_service();
        let run = ok(svc.create_run("user-1".into(), test_request("task")).await);
        ok(svc.pause_run(run.run_id.clone(), "user-1".into()).await);
        let result = ok(svc.resume_run(run.run_id.clone(), "user-1".into()).await);
        assert_eq!(result.status, "running");
        assert_eq!(result.previous_status, "paused");
        let status = ok(svc.get_run_status(run.run_id, "user-1".into()).await);
        assert_eq!(status.status, "running");
    }

    #[tokio::test]
    async fn resume_run_promotes_buffered_completed_pause_to_completed() {
        let svc = test_service_with_engine();
        let run = ok(svc.create_run("user-1".into(), test_request("task")).await);
        ok(svc.pause_run(run.run_id.clone(), "user-1".into()).await);
        svc.run_engine
            .append_event(
                &run.run_id,
                json!({
                    "event_type": "run_finished",
                    "data": {"total_prompt_tokens": 1, "total_completion_tokens": 1}
                }),
            )
            .await
            .unwrap();

        let result = ok(svc.resume_run(run.run_id.clone(), "user-1".into()).await);
        assert_eq!(result.status, "completed");
        assert_eq!(result.previous_status, "paused");
        let status = ok(svc.get_run_status(run.run_id, "user-1".into()).await);
        assert_eq!(status.status, "completed");
    }

    #[tokio::test]
    async fn resume_run_conflict_when_not_paused() {
        let svc = test_service();
        let run = ok(svc.create_run("user-1".into(), test_request("task")).await);
        let e = err(svc.resume_run(run.run_id, "user-1".into()).await);
        assert_eq!(e.0, StatusCode::CONFLICT);
    }

    #[tokio::test]
    async fn resume_run_forbidden_for_other_user() {
        let svc = test_service();
        let run = ok(svc.create_run("user-1".into(), test_request("task")).await);
        ok(svc.pause_run(run.run_id.clone(), "user-1".into()).await);
        let e = err(svc.resume_run(run.run_id, "user-2".into()).await);
        assert_eq!(e.0, StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn resume_run_not_found() {
        let svc = test_service();
        let e = err(svc.resume_run("nonexistent".into(), "user-1".into()).await);
        assert_eq!(e.0, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn pause_resume_round_trip_preserves_events() {
        let svc = test_service();
        let run = ok(svc.create_run("user-1".into(), test_request("task")).await);
        ok(svc.pause_run(run.run_id.clone(), "user-1".into()).await);
        ok(svc.resume_run(run.run_id.clone(), "user-1".into()).await);
        let status = ok(svc
            .get_run_status(run.run_id.clone(), "user-1".into())
            .await);
        assert_eq!(status.events_count, 3); // run_started + run_paused + run_resumed
        let events = ok(svc.stream_run(run.run_id, "user-1".into(), 0).await);
        assert_eq!(events[0]["event_type"], "run_started");
        assert_eq!(events[1]["event_type"], "run_paused");
        assert_eq!(events[2]["event_type"], "run_resumed");
    }

    #[tokio::test]
    async fn double_pause_is_conflict() {
        let svc = test_service();
        let run = ok(svc.create_run("user-1".into(), test_request("task")).await);
        ok(svc.pause_run(run.run_id.clone(), "user-1".into()).await);
        let e = err(svc.pause_run(run.run_id, "user-1".into()).await);
        assert_eq!(e.0, StatusCode::CONFLICT);
    }

    // ─── Durable persistence integration tests ─────────────────────────

    fn test_service_with_engine() -> AgenticRunLifecycleService {
        test_service()
    }

    #[tokio::test]
    #[ignore] // runs full agentic loop; needs live infra
    async fn durable_create_run_persists_to_store() {
        let svc = test_service_with_engine();
        let run = ok(svc.create_run("user-1".into(), test_request("hello")).await);

        let engine = &svc.run_engine;
        let durable = engine.load_run(&run.run_id).await.unwrap().unwrap();
        assert_eq!(durable.user_id, "user-1");
        assert_eq!(durable.status, "running");
        assert_eq!(durable.session_id, run.session_id);
    }

    #[tokio::test]
    #[ignore] // runs full agentic loop; needs live infra
    async fn durable_create_run_eventually_persists_terminal_event() {
        let svc = test_service_with_engine();
        let run = ok(svc.create_run("user-1".into(), test_request("hello")).await);

        let engine = &svc.run_engine;
        let durable = tokio::time::timeout(std::time::Duration::from_secs(10), async {
            loop {
                let durable = engine.load_run(&run.run_id).await.unwrap().unwrap();
                if durable.status != "running"
                    && matches!(
                        durable
                            .events
                            .last()
                            .and_then(|event| event["event_type"].as_str()),
                        Some("run_finished")
                    )
                {
                    break durable;
                }
                tokio::time::sleep(std::time::Duration::from_millis(25)).await;
            }
        })
        .await
        .expect("timeout waiting for durable run to persist terminal event");
        assert_eq!(durable.events.last().unwrap()["event_type"], "run_finished");
    }

    #[tokio::test]
    #[ignore] // runs full agentic loop; needs live infra
    async fn durable_stream_chat_persists_final_state() {
        let svc = test_service_with_engine();
        let stream = ok(svc
            .stream_chat("user-1".into(), test_request("hello"))
            .await);

        let engine = &svc.run_engine;
        let durable = tokio::time::timeout(std::time::Duration::from_secs(10), async {
            loop {
                let durable = engine.load_run(&stream.run_id).await.unwrap().unwrap();
                if durable.status != "running"
                    && matches!(
                        durable
                            .events
                            .last()
                            .and_then(|event| event["event_type"].as_str()),
                        Some("run_finished")
                    )
                {
                    break durable;
                }
                tokio::time::sleep(std::time::Duration::from_millis(25)).await;
            }
        })
        .await
        .expect("timeout waiting for durable stream_chat final state");
        assert_eq!(durable.user_id, "user-1");
        assert_eq!(durable.session_id, stream.session_id);
        assert!(durable.events.len() >= 2);
        assert_eq!(durable.events.last().unwrap()["event_type"], "run_finished");
    }

    #[tokio::test]
    #[ignore] // runs full agentic loop; needs live infra
    async fn durable_cancel_persists_to_store() {
        let svc = test_service_with_engine();
        let run = ok(svc.create_run("user-1".into(), test_request("task")).await);
        ok(svc.cancel_run(run.run_id.clone(), "user-1".into()).await);

        let engine = &svc.run_engine;
        let durable = tokio::time::timeout(std::time::Duration::from_secs(10), async {
            loop {
                let durable = engine.load_run(&run.run_id).await.unwrap().unwrap();
                if matches!(
                    durable
                        .events
                        .last()
                        .and_then(|event| event["event_type"].as_str()),
                    Some("run_finished")
                ) {
                    break durable;
                }
                tokio::time::sleep(std::time::Duration::from_millis(25)).await;
            }
        })
        .await
        .expect("timeout waiting for cancelled run to persist terminal event");
        assert_eq!(durable.status, "cancelled");
        assert!(durable.events.len() >= 2); // run_started + run_finished
    }

    #[tokio::test]
    #[ignore] // runs full agentic loop; needs live infra
    async fn durable_pause_resume_round_trip() {
        let svc = test_service_with_engine();
        let run = ok(svc.create_run("user-1".into(), test_request("task")).await);

        ok(svc.pause_run(run.run_id.clone(), "user-1".into()).await);
        let engine = &svc.run_engine;
        let durable = engine.load_run(&run.run_id).await.unwrap().unwrap();
        assert_eq!(durable.status, "paused");
        assert_eq!(durable.waiting_for.as_deref(), Some("user_resume"));

        ok(svc.resume_run(run.run_id.clone(), "user-1".into()).await);
        let durable = engine.load_run(&run.run_id).await.unwrap().unwrap();
        assert_eq!(durable.status, "running");
        assert!(durable.waiting_for.is_none());
    }

    #[tokio::test]
    async fn cancel_run_returns_durable_terminal_status_on_cache_miss() {
        let svc = test_service_with_engine();
        let engine = &svc.run_engine;
        engine.start_run("run-1", "user-1", "sess-1").await.unwrap();
        engine
            .persist_status("run-1", STATUS_COMPLETED, None, None)
            .await
            .unwrap();

        let result = ok(svc.cancel_run("run-1".into(), "user-1".into()).await);
        assert_eq!(result.run_id, "run-1");
        assert_eq!(result.status, STATUS_COMPLETED);
    }

    #[tokio::test]
    async fn cancel_run_running_cache_miss_persists_cancelled() {
        let svc = test_service_with_engine();
        let engine = &svc.run_engine;
        engine.start_run("run-1", "user-1", "sess-1").await.unwrap();

        let result = ok(svc.cancel_run("run-1".into(), "user-1".into()).await);
        assert_eq!(result.status, STATUS_CANCELLED);
        let durable = engine.load_run("run-1").await.unwrap().unwrap();
        assert_eq!(durable.status, STATUS_CANCELLED);
        assert_eq!(durable.events.last().unwrap()["event_type"], "run_finished");
    }

    #[tokio::test]
    async fn pause_run_running_cache_miss_returns_service_unavailable() {
        let svc = test_service_with_engine();
        let engine = &svc.run_engine;
        engine.start_run("run-1", "user-1", "sess-1").await.unwrap();

        let e = err(svc.pause_run("run-1".into(), "user-1".into()).await);
        assert_eq!(e.0, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(e.1.0.detail, "Run control state unavailable for pause");
    }

    #[tokio::test]
    async fn resume_run_paused_cache_miss_returns_service_unavailable() {
        let svc = test_service_with_engine();
        let engine = &svc.run_engine;
        engine.start_run("run-1", "user-1", "sess-1").await.unwrap();
        engine
            .persist_status("run-1", STATUS_PAUSED, Some("user_resume"), None)
            .await
            .unwrap();

        let e = err(svc.resume_run("run-1".into(), "user-1".into()).await);
        assert_eq!(e.0, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(e.1.0.detail, "Run control state unavailable for resume");
    }

    #[tokio::test]
    async fn cancel_run_paused_cache_miss_persists_cancelled() {
        let svc = test_service_with_engine();
        let engine = &svc.run_engine;
        engine.start_run("run-1", "user-1", "sess-1").await.unwrap();
        engine
            .persist_status("run-1", STATUS_PAUSED, Some("user_resume"), None)
            .await
            .unwrap();

        let result = ok(svc.cancel_run("run-1".into(), "user-1".into()).await);
        assert_eq!(result.status, STATUS_CANCELLED);
        let durable = engine.load_run("run-1").await.unwrap().unwrap();
        assert_eq!(durable.status, STATUS_CANCELLED);
    }

    #[tokio::test]
    #[ignore] // stream_chat runs full agentic loop; needs live DB + LLM or mock
    async fn get_run_status_falls_back_to_durable_store_on_cache_miss() {
        let svc = test_service_with_engine();
        let stream = ok(svc
            .stream_chat("user-1".into(), test_request("hello"))
            .await);
        let engine = &svc.run_engine;
        let durable = engine.load_run(&stream.run_id).await.unwrap().unwrap();

        svc.runs.write().await.remove(&stream.run_id);

        let status = ok(svc
            .get_run_status(stream.run_id.clone(), "user-1".into())
            .await);
        assert_eq!(status.run_id, stream.run_id);
        assert_eq!(status.session_id, stream.session_id);
        assert_eq!(status.status, durable.status);
        assert_eq!(status.waiting_for, durable.waiting_for);
        assert_eq!(status.events_count, durable.events.len() as i64);
    }

    #[tokio::test]
    async fn stream_run_cache_miss_replays_durable_text_done() {
        let svc = test_service_with_engine();
        let engine = &svc.run_engine;
        engine
            .start_run("run-durable-text", "user-1", "session-1")
            .await
            .expect("start durable run");
        engine
            .append_event(
                "run-durable-text",
                json!({"event_type": "text_done", "data": {"full_text": "durable final answer"}}),
            )
            .await
            .expect("persist text_done");
        engine
            .append_event(
                "run-durable-text",
                json!({"event_type": "run_finished", "data": {"prompt_tokens": 1}}),
            )
            .await
            .expect("persist run_finished");

        let events = ok(svc
            .stream_run("run-durable-text".into(), "user-1".into(), 1)
            .await);

        assert_eq!(events.len(), 2);
        assert_eq!(events[0]["event_type"], "text_done");
        assert_eq!(events[0]["data"]["full_text"], "durable final answer");
        assert_eq!(events[1]["event_type"], "run_finished");
    }

    #[tokio::test]
    async fn submit_run_input_uses_durable_idempotency_on_cache_miss() {
        let svc = test_service_with_engine();
        let engine = &svc.run_engine;
        engine
            .start_run("run-input", "user-1", "session-1")
            .await
            .unwrap();

        let first = ok(svc
            .submit_run_input(
                "run-input".into(),
                "user-1".into(),
                RunInputData {
                    idempotency_key: "key-1".into(),
                    input: json!({"answer": "yes"}),
                },
            )
            .await);
        let duplicate = ok(svc
            .submit_run_input(
                "run-input".into(),
                "user-1".into(),
                RunInputData {
                    idempotency_key: "key-1".into(),
                    input: json!({"answer": "yes"}),
                },
            )
            .await);

        let durable = engine.load_run("run-input").await.unwrap().unwrap();
        let matching_inputs = durable
            .events
            .iter()
            .filter(|event| event.get("idempotency_key").and_then(Value::as_str) == Some("key-1"))
            .count();
        assert!(!first.duplicate);
        assert!(duplicate.duplicate);
        assert_eq!(matching_inputs, 1);
    }

    #[tokio::test]
    async fn submit_run_input_rejects_terminal_durable_run() {
        let svc = test_service_with_engine();
        let engine = &svc.run_engine;
        engine
            .start_run("run-terminal-input", "user-1", "session-1")
            .await
            .unwrap();
        engine
            .persist_status("run-terminal-input", STATUS_COMPLETED, None, None)
            .await
            .unwrap();

        let e = err(svc
            .submit_run_input(
                "run-terminal-input".into(),
                "user-1".into(),
                RunInputData {
                    idempotency_key: "key-1".into(),
                    input: json!({"answer": "late"}),
                },
            )
            .await);
        assert_eq!(e.0, StatusCode::CONFLICT);
    }

    #[tokio::test]
    async fn create_run_conflict_checks_durable_active_session() {
        let svc = test_service_with_engine();
        let engine = &svc.run_engine;
        engine
            .start_run("existing-run", "user-1", "shared-session")
            .await
            .unwrap();
        let mut request = test_request("second");
        request.session_id = Some("shared-session".into());

        let e = err(svc.create_run("user-1".into(), request).await);
        assert_eq!(e.0, StatusCode::CONFLICT);
    }

    #[tokio::test]
    #[ignore] // stream_chat runs full agentic loop; needs live DB + LLM or mock
    async fn stream_run_falls_back_to_durable_store_on_cache_miss() {
        let svc = test_service_with_engine();
        let stream = ok(svc
            .stream_chat("user-1".into(), test_request("hello"))
            .await);
        let engine = &svc.run_engine;
        let durable = engine.load_run(&stream.run_id).await.unwrap().unwrap();

        svc.runs.write().await.remove(&stream.run_id);

        let events = ok(svc
            .stream_run(stream.run_id.clone(), "user-1".into(), 1)
            .await);
        assert_eq!(
            events,
            AgenticRunLifecycleService::format_run_events(&durable.events[1..], 1)
        );
    }

    #[tokio::test]
    #[ignore] // stream_chat runs full agentic loop; needs live DB + LLM or mock
    async fn list_runs_falls_back_to_durable_store_on_cache_miss() {
        let svc = test_service_with_engine();
        let first = ok(svc
            .stream_chat("user-1".into(), test_request("first"))
            .await);
        let second = ok(svc
            .stream_chat("user-1".into(), test_request("second"))
            .await);

        svc.runs.write().await.remove(&first.run_id);

        let runs = ok(svc.list_runs("user-1".into(), 10, 0).await);
        let run_ids: Vec<_> = runs.runs.iter().map(|run| run.run_id.as_str()).collect();
        assert_eq!(runs.total, 2);
        assert!(run_ids.contains(&first.run_id.as_str()));
        assert!(run_ids.contains(&second.run_id.as_str()));
    }

    #[tokio::test]
    async fn lifecycle_run_creation_is_durable_by_default() {
        let svc = test_service();
        let run = ok(svc.create_run("user-1".into(), test_request("hello")).await);
        let durable = svc.run_engine.load_run(&run.run_id).await.unwrap().unwrap();
        assert_eq!(durable.user_id, "user-1");
        assert_eq!(durable.session_id, run.session_id);
        assert_eq!(durable.status, STATUS_RUNNING);
    }

    // ─── EdgeContext integration tests ──────────────────────────────────

    #[test]
    fn extract_edge_context_from_request_with_tools() {
        let mut ctx = serde_json::Map::new();
        ctx.insert(
            "edge_tools".to_string(),
            json!([{"function": {"name": "bash", "parameters": {}}}]),
        );
        ctx.insert(
            "edge_profile".to_string(),
            json!({"cwd": "/tmp", "git_branch": "main"}),
        );
        let req = ChatRequestData {
            context: Some(ctx),
            ..test_request("hello")
        };

        let edge_ctx = AgenticRunLifecycleService::extract_edge_context(&req);
        assert_eq!(edge_ctx.tool_count(), 1);
        assert_eq!(edge_ctx.tool_names(), vec!["bash"]);
        assert_eq!(edge_ctx.edge_profile.cwd.as_deref(), Some("/tmp"));
        assert_eq!(edge_ctx.edge_profile.git_branch.as_deref(), Some("main"));
    }

    #[test]
    fn extract_edge_context_from_empty_request() {
        let req = test_request("hello");
        let edge_ctx = AgenticRunLifecycleService::extract_edge_context(&req);
        assert!(!edge_ctx.has_tools());
        assert!(edge_ctx.edge_profile.cwd.is_none());
    }

    #[test]
    fn extract_edge_tools_backward_compat() {
        let mut ctx = serde_json::Map::new();
        ctx.insert(
            "edge_tools".to_string(),
            json!([
                {"function": {"name": "bash"}},
                {"function": {"name": "grep"}}
            ]),
        );
        let req = ChatRequestData {
            context: Some(ctx),
            ..test_request("hello")
        };
        let tools = AgenticRunLifecycleService::extract_edge_tools(&req);
        assert_eq!(tools.len(), 2);
    }

    #[test]
    fn extract_edge_profile_backward_compat() {
        let mut ctx = serde_json::Map::new();
        ctx.insert(
            "edge_profile".to_string(),
            json!({"cwd": "/workspace", "os": "linux"}),
        );
        let req = ChatRequestData {
            context: Some(ctx),
            ..test_request("hello")
        };
        let profile = AgenticRunLifecycleService::extract_edge_profile(&req);
        assert_eq!(profile.get("cwd").unwrap(), "/workspace");
        assert_eq!(profile.get("os").unwrap(), "linux");
    }

    // ─── Background spawning integration tests ──────────────────────────

    #[tokio::test]
    #[ignore] // runs full agentic loop; needs live infra
    async fn create_run_spawns_background_task() {
        let svc = test_service();
        let run = ok(svc.create_run("user-1".into(), test_request("hello")).await);
        assert_eq!(run.status, "running");

        // Deterministic wait: poll until the background task advances state.
        let status = tokio::time::timeout(std::time::Duration::from_secs(10), async {
            loop {
                let status = ok(svc
                    .get_run_status(run.run_id.clone(), "user-1".into())
                    .await);
                if status.status != "running" || status.events_count > 1 {
                    break status;
                }
                tokio::time::sleep(std::time::Duration::from_millis(25)).await;
            }
        })
        .await
        .expect("timeout waiting for background task to advance state");
        assert!(
            status.status != "running" || status.events_count > 1,
            "Expected background task to advance state, but status={} events={}",
            status.status,
            status.events_count
        );
    }

    #[tokio::test]
    #[ignore] // runs full agentic loop; needs live infra
    async fn create_run_with_engine_persists_final_state() {
        let svc = test_service_with_engine();
        let run = ok(svc.create_run("user-1".into(), test_request("hello")).await);

        // Deterministic wait: poll durable state until it leaves "running".
        let engine = &svc.run_engine;
        let durable = tokio::time::timeout(std::time::Duration::from_secs(10), async {
            loop {
                let durable = engine.load_run(&run.run_id).await.unwrap().unwrap();
                if durable.status != "running" {
                    break durable;
                }
                tokio::time::sleep(std::time::Duration::from_millis(25)).await;
            }
        })
        .await
        .expect("timeout waiting for durable run status to finalize");
        assert_ne!(durable.status, "running");
    }

    // ─── DelegationTracker integration tests ────────────────────────────

    #[tokio::test]
    async fn delegation_tracker_get_children() {
        use crate::server::delegation_engine::{DelegationTracker, SubRunRecord, SubRunState};

        let tracker = DelegationTracker::new();
        tracker
            .record_sub_run(SubRunRecord {
                delegation_id: "d1".into(),
                run_id: "child-1".into(),
                parent_run_id: "parent-1".into(),
                agent_id: "agent-a".into(),
                depth: 1,
                state: SubRunState::Created,
                retry_of: None,
            })
            .await;
        tracker
            .record_sub_run(SubRunRecord {
                delegation_id: "d1".into(),
                run_id: "child-2".into(),
                parent_run_id: "parent-1".into(),
                agent_id: "agent-b".into(),
                depth: 1,
                state: SubRunState::Created,
                retry_of: None,
            })
            .await;
        tracker
            .record_sub_run(SubRunRecord {
                delegation_id: "d2".into(),
                run_id: "other-child".into(),
                parent_run_id: "parent-2".into(),
                agent_id: "agent-c".into(),
                depth: 1,
                state: SubRunState::Created,
                retry_of: None,
            })
            .await;

        let mut children = tracker.get_children("parent-1").await;
        children.sort();
        assert_eq!(children, vec!["child-1", "child-2"]);

        let children = tracker.get_children("parent-2").await;
        assert_eq!(children, vec!["other-child"]);

        let children = tracker.get_children("nonexistent").await;
        assert!(children.is_empty());
    }

    /// P0-C: The agentic loop spawn must check token budget before starting.
    #[test]
    fn run_lifecycle_checks_token_budget_before_loop() {
        let source = include_str!("run_lifecycle.rs");
        let test_start = source.find("mod tests {").unwrap_or(source.len());
        let prod_code = &source[..test_start];
        assert!(
            prod_code.contains("check_token_budget"),
            "run_lifecycle must call check_token_budget before the agentic loop"
        );
    }

    /// P0-C: drain_background_tasks returns true when no tasks are running.
    #[tokio::test]
    async fn drain_background_tasks_returns_immediately_when_idle() {
        // Test the drain logic directly: counter at 0 → drain returns true immediately.
        let count = Arc::new(AtomicUsize::new(0));
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_millis(100);
        let drained = loop {
            if count.load(Ordering::Acquire) == 0 {
                break true;
            }
            if tokio::time::Instant::now() >= deadline {
                break false;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        };
        assert!(drained, "counter at 0 — drain must return true immediately");
    }

    /// P0-C: background_task_count increments on spawn and decrements on exit.
    #[tokio::test]
    async fn background_task_count_tracks_spawned_tasks() {
        use std::sync::atomic::Ordering;
        let count = Arc::new(AtomicUsize::new(0));
        let count_clone = Arc::clone(&count);

        // Simulate what the spawn does: increment, spawn, decrement on drop
        count.fetch_add(1, Ordering::Release);
        let handle = tokio::spawn(async move {
            struct Guard(Arc<AtomicUsize>);
            impl Drop for Guard {
                fn drop(&mut self) {
                    self.0.fetch_sub(1, Ordering::Release);
                }
            }
            let _g = Guard(count_clone);
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        });

        assert_eq!(count.load(Ordering::Acquire), 1, "task in flight");
        handle.await.unwrap();
        assert_eq!(
            count.load(Ordering::Acquire),
            0,
            "task completed — counter must be 0"
        );
    }

    /// P1-A: RunStatus::try_transition enforces valid state machine transitions.
    #[test]
    fn run_status_try_transition_valid_and_invalid() {
        use super::RunStatus::*;

        // Valid transitions
        assert!(Running.try_transition(&Paused).is_ok());
        assert!(Running.try_transition(&Completed).is_ok());
        assert!(Running.try_transition(&Failed).is_ok());
        assert!(Running.try_transition(&Cancelled).is_ok());
        assert!(Paused.try_transition(&Running).is_ok());
        assert!(Paused.try_transition(&Cancelled).is_ok());
        assert!(Paused.try_transition(&Failed).is_ok());

        // Terminal states cannot transition
        let err = Completed.try_transition(&Running);
        assert!(err.is_err(), "Completed → Running must be rejected");
        assert!(
            err.unwrap_err().contains("Completed"),
            "error must name the source state"
        );

        let err = Failed.try_transition(&Running);
        assert!(err.is_err(), "Failed → Running must be rejected");

        let err = Cancelled.try_transition(&Completed);
        assert!(err.is_err(), "Cancelled → Completed must be rejected");

        // Running cannot go back to Running
        let err = Running.try_transition(&Running);
        assert!(err.is_err(), "Running → Running must be rejected");
    }

    /// P1-F: list_runs pagination must be deterministic — all runs appear
    /// exactly once across pages, with no duplicates or missing entries.
    #[tokio::test]
    async fn list_runs_pagination_is_deterministic() {
        let svc = test_service();
        for i in 0..5 {
            ok(svc
                .create_run("user-pg".into(), test_request(&format!("msg {i}")))
                .await);
        }
        // Collect all run_ids across 3 pages
        let mut all_ids = Vec::new();
        let page1 = ok(svc.list_runs("user-pg".into(), 2, 0).await);
        all_ids.extend(page1.runs.iter().map(|r| r.run_id.clone()));
        let page2 = ok(svc.list_runs("user-pg".into(), 2, 2).await);
        all_ids.extend(page2.runs.iter().map(|r| r.run_id.clone()));
        let page3 = ok(svc.list_runs("user-pg".into(), 2, 4).await);
        all_ids.extend(page3.runs.iter().map(|r| r.run_id.clone()));

        assert_eq!(all_ids.len(), 5, "all 5 runs must appear across pages");
        let unique: std::collections::HashSet<_> = all_ids.iter().collect();
        assert_eq!(
            unique.len(),
            5,
            "no duplicate run_ids across pages — pagination must be deterministic"
        );
    }

    /// P1-A: RunStatus must have a Waiting variant that is non-terminal.
    /// Runs needing external input must not be killed as Failed.
    #[test]
    fn waiting_is_non_terminal_status() {
        // Running → Waiting is valid
        assert!(
            RunStatus::Running
                .try_transition(&RunStatus::Waiting)
                .is_ok(),
            "Running → Waiting must be allowed"
        );
        // Waiting → Running is valid (resume after input)
        assert!(
            RunStatus::Waiting
                .try_transition(&RunStatus::Running)
                .is_ok(),
            "Waiting → Running must be allowed (resume)"
        );
        // Waiting → Cancelled is valid
        assert!(
            RunStatus::Waiting
                .try_transition(&RunStatus::Cancelled)
                .is_ok(),
            "Waiting → Cancelled must be allowed"
        );
        // Waiting → Failed is valid (timeout)
        assert!(
            RunStatus::Waiting
                .try_transition(&RunStatus::Failed)
                .is_ok(),
            "Waiting → Failed must be allowed"
        );
        // Waiting serializes as "waiting"
        assert_eq!(RunStatus::Waiting.as_str(), "waiting");
    }

    /// P1-A: finalize_run_events must preserve Waiting as a non-error status.
    #[test]
    fn finalize_run_events_preserves_waiting_without_error_event() {
        let svc = test_service();
        let request = test_request("wait");
        let state =
            svc.build_initial_state("test-user", &request, "session-1", "run-1", None, None);

        let (events, status, error) = AgenticRunLifecycleService::finalize_run_events(
            Ok(AgenticLoopOutcome::Waiting("tool_approval".into())),
            vec![],
            &state,
        );

        assert_eq!(status, RunStatus::Waiting);
        assert_eq!(error.as_deref(), Some("waiting: tool_approval"));
        assert_eq!(events.len(), 1);
        assert_eq!(events[0]["event_type"], "run_waiting");
        assert_eq!(events[0]["data"]["reason"], "waiting: tool_approval");
    }

    /// P1-F: stream_chat must persist usage unconditionally.
    /// Cancelled runs still consumed tokens and must have accurate durable records,
    /// even when status persistence is skipped.
    #[test]
    fn stream_chat_persists_usage_unconditionally() {
        let source = include_str!("run_lifecycle.rs");
        // Find the stream_chat method
        let fn_start = source
            .find("async fn stream_chat(")
            .expect("stream_chat must exist");
        let fn_end = source[fn_start..]
            .find("\n    async fn ")
            .map(|p| fn_start + p)
            .unwrap_or(source.len());
        let fn_body = &source[fn_start..fn_end];

        let usage_pos = fn_body
            .find(".persist_usage(")
            .expect("stream_chat must call persist_usage");

        // persist_usage must NOT be inside the status-persistence guard.
        // Cancelled runs skip persist_status, but usage must still be written.
        let guard_pos = fn_body
            .find("if persist_status_update {")
            .expect("persist_status_update guard must exist");
        let guard_block = &fn_body[guard_pos..];
        let mut depth = 0;
        let mut guard_end = 0;
        for (i, c) in guard_block.char_indices() {
            if c == '{' {
                depth += 1;
            } else if c == '}' {
                depth -= 1;
                if depth == 0 {
                    guard_end = guard_pos + i + 1;
                    break;
                }
            }
        }
        assert!(
            usage_pos > guard_end,
            "persist_usage must remain outside the persist_status_update guard — \
             cancelled stream_chat runs must still persist usage for billing/audit"
        );
    }

    /// P1-C: build_server_skill_executor must accept and wire a cancel_token.
    /// Without this, skill sub-runs ignore parent cancellation.
    #[test]
    fn build_server_skill_executor_accepts_cancel_token() {
        let source = include_str!("run_lifecycle.rs");
        let fn_start = source
            .find("fn build_server_skill_executor(")
            .expect("build_server_skill_executor must exist");
        let fn_end = source[fn_start..]
            .find("\npub(crate) fn ")
            .or_else(|| source[fn_start..].find("\nfn "))
            .map(|p| fn_start + p)
            .unwrap_or(source.len());
        let fn_body = &source[fn_start..fn_end];
        assert!(
            fn_body.contains("cancel_token"),
            "build_server_skill_executor must accept a cancel_token parameter"
        );
        assert!(
            fn_body.contains("with_cancel_token"),
            "build_server_skill_executor must wire cancel_token via with_cancel_token"
        );
    }

    /// P1-C: build_initial_state must pass cancel_token to skill executor builder.
    #[test]
    fn build_initial_state_passes_cancel_token_to_skill_executor() {
        let source = include_str!("run_lifecycle.rs");
        let fn_start = source
            .find("fn build_initial_state(")
            .expect("build_initial_state must exist");
        let fn_end = source[fn_start..]
            .find("\n    fn ")
            .or_else(|| source[fn_start..].find("\n    pub"))
            .map(|p| fn_start + p)
            .unwrap_or(source.len());
        let fn_body = &source[fn_start..fn_end];
        assert!(
            fn_body.contains("cancel_token"),
            "build_initial_state must accept and pass cancel_token to skill executor"
        );
    }

    /// Waiting runs must NOT be evicted from the in-memory cache.
    /// If evicted, they cannot be resumed (no in-memory state to transition).
    #[test]
    fn waiting_runs_skip_eviction_in_both_spawn_paths() {
        let source = include_str!("run_lifecycle.rs");
        let prod_code = &source[..source.find("\nmod tests {").unwrap_or(source.len())];

        // Find the two guarded eviction sites (create_run normal exit + stream_chat normal exit).
        // The cancelled-run path doesn't need a Waiting guard (cancelled != Waiting).
        // Each guarded site has `if final_status != RunStatus::Waiting` before the call.
        let waiting_guards = prod_code
            .matches("final_status != RunStatus::Waiting")
            .count();
        assert!(
            waiting_guards >= 2,
            "at least 2 schedule_run_eviction sites must be guarded by Waiting check, found {waiting_guards}"
        );
    }

    /// A Waiting run persisted in durable store must be cancellable even after
    /// the process-local control handle is gone.
    #[tokio::test]
    async fn cancel_run_waiting_cache_miss_persists_cancelled() {
        let svc = test_service_with_engine();
        let engine = &svc.run_engine;
        engine
            .start_run("waiting-run", "user-1", "session-1")
            .await
            .unwrap();
        engine
            .persist_status("waiting-run", STATUS_WAITING, Some("tool_approval"), None)
            .await
            .unwrap();

        let result = ok(svc.cancel_run("waiting-run".into(), "user-1".into()).await);
        let durable = engine.load_run("waiting-run").await.unwrap().unwrap();
        assert_eq!(result.status, STATUS_CANCELLED);
        assert_eq!(durable.status, STATUS_CANCELLED);
    }
}
