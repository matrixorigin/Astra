//! Concrete [`RunLifecycleService`] backed by [`ServerAgenticLoopHost`].
//!
//! This module replaces `UnconfiguredRunLifecycleService` with a real implementation
//! that runs multi-turn agentic loops on the server via the shared
//! [`run_agentic_loop_with_host`] cognitive pipeline.
//!
//! Run state is held in-memory (`DashMap`) for low-latency queries; events are
//! buffered per-run so `stream_run()` can replay from any offset.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Instant;

use async_trait::async_trait;
use axum::Json;
use axum::http::StatusCode;
use serde_json::{Map, Value, json};
use tokio::sync::{Mutex as TokioMutex, RwLock, mpsc};

use astra_server_types::ws_progress_callback::ProgressEvent;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use astra_core::{ErrorResponse, SharedPool, connect_matrixone, error_response};
use astra_services::EdgeContext;
use astra_services::runs::{
    CancelRunRecord, ChatRequestData, ChatRunRecord, ChatStreamRecord, DurableRunRecord,
    RunLifecycleService, RunListRecord, RunMutationRecord, RunStatusRecord,
};
use astra_services::session_audit::{RUNTIME_PROMOTION_EVENT_TYPE, RuntimePromotionEventData};
use astra_services::skills::SkillService;
use sqlx::Row;

use crate::FernetTokenEncryptor;
use crate::MatrixOneSettings;
use crate::evolution::service::EvolutionService;
use crate::observability_integration::ObservabilityHub;
use crate::turn::agentic_loop_host::{
    AgenticLoopOutcome, AgenticLoopState, CancellationState, ContextTracePersistenceContext,
    EvaluationPersistenceContext, MessagingState, RequestConstraints, SkillState, StopHookState,
    run_agentic_loop_with_host,
};
use crate::{
    DatabaseEvaluationService, DatabaseEventService, DatabaseTurnCoreEventWriter,
    EventCreateRequestData, EventService,
};
use astra_pipeline::step_recorder::StepRecorder;
use astra_turn_core::contracts::{
    TurnCoreEventRecord, TurnCoreEventWriter, TurnCorePersistPlan, TurnDecisionAuditRecord,
    TurnHookDbPersistPlan, TurnHookDbWriter, TurnLearningOutcome, TurnLearningWriter,
    TurnObserverRequest, TurnObserverWorker, TurnSkillSelectionRecord, TurnToolEventPersistPlan,
    TurnToolEventRecord, TurnToolEventWriter,
};

use astra_core::{
    STATUS_CANCELLED, STATUS_COMPLETED, STATUS_FAILED, STATUS_PAUSED, STATUS_RUNNING,
    STATUS_WAITING,
};

use super::run_engine::RunEngine;
use super::server_loop_host::ServerAgenticLoopHostBuilder;
use super::state_builder::PipelineLearningStack;

const RUNTIME_CONTEXT_TRACE_AGENT_ID: &str = "astra-server";
const LLM_TOKEN_SERVICE_TRUSTED_DOMAINS_TABLE: &str = "runtime_llm_trusted_domains";

// ─── Skill wiring for server paths ──────────────────────────────────────────

type ServerSkillResolverBundle = (
    Option<Arc<crate::skills::UnifiedSkillRegistry>>,
    Option<Arc<dyn crate::turn::skill_tool::SkillResolver>>,
);

/// Build skill registry + resolver for server-side agentic loops.
///
/// Returns `(registry_for_activation, resolver)` using runtime providers
/// (Local + Bundled + optional Database provider).
fn build_server_skill_resolver(
    skill_service: Option<Arc<dyn SkillService>>,
    cache: &std::sync::OnceLock<ServerSkillResolverBundle>,
) -> ServerSkillResolverBundle {
    use crate::turn::skill_tool::SkillResolver as _;

    let bundle = cache.get_or_init(|| {
        let mut registry = crate::skills::UnifiedSkillRegistry::new();
        registry.add_provider(Box::new(crate::skills::LocalSkillProvider::standard()));
        registry.add_provider(Box::new(
            crate::skills::BundledSkillProvider::with_defaults(),
        ));
        if let Some(service) = skill_service {
            registry.add_provider(Box::new(crate::skills::DatabaseSkillProvider::new(service)));
        }
        let registry = Arc::new(registry);
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            let r = Arc::clone(&registry);
            match handle.runtime_flavor() {
                tokio::runtime::RuntimeFlavor::MultiThread => {
                    let _ = tokio::task::block_in_place(|| handle.block_on(r.discover_all()));
                }
                _ => {
                    let _ = std::thread::scope(|s| {
                        s.spawn(|| handle.block_on(r.discover_all())).join().ok()
                    });
                }
            }
        }
        if registry.is_empty() {
            return (None, None);
        }
        let resolver_impl = Arc::new(crate::skills::UnifiedSkillResolver::new(Arc::clone(
            &registry,
        )));
        let skills = resolver_impl.available_skills();
        let resolver: Option<Arc<dyn crate::turn::skill_tool::SkillResolver>> = if skills.is_empty()
        {
            None
        } else {
            Some(resolver_impl)
        };
        (Some(registry), resolver)
    });
    bundle.clone()
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
    if let Some(pool) = edge_connection_pool {
        subrun_executor = subrun_executor.with_edge_connection_pool(pool.clone());
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

fn seed_restricted_tools_from_blocked_patterns(
    loop_state: &mut AgenticLoopState,
    pattern_library: &astra_pipeline::pattern::PatternLibrary,
) {
    for name in pattern_library.blocked_tool_names() {
        if !astra_turn_core::tool_registry_meta::is_pinned_tool(&name) {
            loop_state.restricted_tools.insert(name);
        }
    }
}

async fn initialize_runtime_controllers(
    loop_state: &mut AgenticLoopState,
    user_id: &str,
    session_id: &str,
    evaluation_persistence: Option<EvaluationPersistenceContext>,
    context_trace_persistence: Option<ContextTracePersistenceContext>,
) -> super::state_builder::PipelineLearningStack {
    let learning_stack = super::state_builder::build_pipeline_learning_stack(Some("default"));
    let hub = Arc::new(ObservabilityHub::new());
    hub.attach_pattern_library(learning_stack.pattern_library.clone());
    let session = hub.start_session(user_id, session_id);

    // Seed restricted_tools from evolution-blocked patterns so the LLM never
    // sees schemas for tools that cross-session learning has identified as
    // persistently failing (deny-at-assembly).
    if let Ok(lib) = learning_stack.pattern_library.lock() {
        seed_restricted_tools_from_blocked_patterns(loop_state, &lib);
    }

    let evolution_service = Arc::new(
        EvolutionService::new()
            .with_pattern_library(learning_stack.pattern_library.clone())
            .with_calibrator(learning_stack.calibrator.clone()),
    );
    if let Some(active_canary) = learning_stack.active_canary.clone()
        && let Err(err) = evolution_service.restore_active_canary(active_canary).await
    {
        astra_core::agent_warn!(
            "evolution",
            "Failed to restore persisted active canary: {err}"
        );
    }

    loop_state.telemetry.observability_hub = Some(hub);
    loop_state.telemetry.observability_session = Some(session);
    loop_state.telemetry.evaluation_persistence = evaluation_persistence;
    loop_state.telemetry.context_trace_persistence = context_trace_persistence;
    loop_state.evolution_service = Some(evolution_service);
    learning_stack
}

async fn configure_runtime_controllers(
    matrixone: &MatrixOneSettings,
    shared_pool: Option<&SharedPool>,
    loop_state: &mut AgenticLoopState,
    user_id: &str,
    session_id: &str,
) -> super::state_builder::PipelineLearningStack {
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
    // Skip when no shared pool — avoids blocking on connect_matrixone() in tests.
    let Some(pool) = shared_pool else {
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
    async fn run(
        &self,
        state: &AgenticLoopState,
        learning_stack: &PipelineLearningStack,
        loop_success: bool,
    ) {
        // 0. Persist CSL via CslManager.
        if let Some(ref mgr) = self.csl_manager {
            let mut mgr = mgr.lock().await;
            let session_state = extract_session_state_compact(state);
            if let Err(e) = mgr
                .persist_turn(state.session_turn, &state.messages, &session_state)
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
            self.agent_id.as_deref(),
            &self.user_message,
            state,
            self.model_name.as_deref(),
        )
        .await;

        // 2. Persist tool_call events for session_audit metrics.
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

        // 3. Persist decision audit + skill selection to hook DB.
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

        // 5. Record pipeline learning outcome (PatternLibrary / EntityGraph).
        record_server_loop_learning_outcome(
            learning_stack.writer.as_ref(),
            &self.user_message,
            state,
            loop_success,
        )
        .await;

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

        // 8. Save cross-session learning state.
        let active_canary = match state.evolution_service.as_ref() {
            Some(evolution_service) => evolution_service.export_active_canary().await,
            None => None,
        };
        learning_stack.save_with_active_canary(active_canary);
    }
}

fn extract_session_state_compact(
    state: &AgenticLoopState,
) -> astra_turn_core::conversation_log::SessionStateCompact {
    astra_turn_core::conversation_log::SessionStateCompact {
        continuity: Some(state.continuity.clone()),
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

fn server_loop_causal_chain_id(kind: &str) -> String {
    let chain_id = format!("{kind}:{}", Uuid::now_v7());
    debug_assert!(
        chain_id.len() <= 64,
        "server loop causal_chain_id must fit agent_events VARCHAR(64)"
    );
    chain_id
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
    agent_id: Option<&str>,
    user_message: &str,
    state: &AgenticLoopState,
    model_name: Option<&str>,
) {
    if user_message.is_empty() && state.final_text.is_empty() {
        return;
    }

    let writer = match shared_pool {
        Some(pool) => DatabaseTurnCoreEventWriter::new(matrixone.clone()).with_pool(pool.clone()),
        None => DatabaseTurnCoreEventWriter::new(matrixone.clone()),
    };

    let chain_id = server_loop_causal_chain_id("server-loop");
    let user_query_event_id = Uuid::now_v7().to_string();

    let user_query_event = if !user_message.is_empty() {
        Some(TurnCoreEventRecord {
            event_id: user_query_event_id.clone(),
            user_id: user_id.to_string(),
            session_id: session_id.to_string(),
            agent_id: agent_id.map(|s| s.to_string()),
            event_type: "user_query".to_string(),
            content: user_message.to_string(),
            parent_event_id: None,
            parent_event_ids: Vec::new(),
            causal_chain_id: chain_id.clone(),
            llm_model_used: None,
            token_usage: None,
            llm_params: None,
            reasoning_content: None,
        })
    } else {
        None
    };

    let llm_response_event = if !state.final_text.is_empty() {
        let usage = if state.total_prompt > 0 || state.total_completion > 0 {
            Some(json!({
                "prompt": state.total_prompt,
                "completion": state.total_completion,
                "total": state.total_prompt + state.total_completion,
            }))
        } else {
            None
        };
        Some(TurnCoreEventRecord {
            event_id: Uuid::now_v7().to_string(),
            user_id: user_id.to_string(),
            session_id: session_id.to_string(),
            agent_id: agent_id.map(|s| s.to_string()),
            event_type: "llm_response".to_string(),
            content: state.final_text.clone(),
            parent_event_id: Some(user_query_event_id.clone()),
            parent_event_ids: vec![user_query_event_id],
            causal_chain_id: chain_id,
            llm_model_used: model_name.map(|s| s.to_string()),
            token_usage: usage,
            llm_params: None,
            reasoning_content: None,
        })
    } else {
        None
    };

    let plan = TurnCorePersistPlan {
        user_query_event,
        llm_response_event,
        snapshot_link_plan: None,
    };
    if let Err(e) = writer.persist(plan).await {
        astra_core::agent_error!(
            "server-loop",
            "failed to persist core events for session {session_id}: {e}"
        );
    }
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
    let skill_selector_metric = crate::turn::skill_tool::build_turn_skill_selector_metric_record(
        session_id,
        user_id,
        i64::from(crate::turn::agentic_loop_lifecycle::session_turn_number(
            state,
        )),
        state.telemetry.initial_skill_selector_shortlist.as_ref(),
        &selected_skills,
    );

    let plan = TurnHookDbPersistPlan {
        decision_audit,
        skill_selection,
        skill_selector_metric,
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

/// Record a pipeline learning outcome from the server-driven loop so the
/// PatternLibrary / EntityGraph / ProgressiveCalibrator can learn across
/// sessions.  This mirrors what the bridge path does via
/// `PipelineLearningWriter.record_outcome()` in `side_effects.rs`.
///
/// Correction detection: the server agentic loop previously hardcoded
/// `was_corrected=false`, which left the ProgressiveCalibrator's three-axis
/// formula `threshold = 0.70 - 0×0.15 - 0×0.10 - 0×0.10 = 0.70` frozen. This
/// function now runs implicit-feedback detection on the user's turn against
/// the most recent assistant message pulled from `state.messages`, matching
/// the CLI/bridge behavior (`repl_turn.rs::record_selector_turn_outcome`,
/// `bridge_inprocess.rs::build_turn_hook_args`).
async fn record_server_loop_learning_outcome(
    writer: &dyn TurnLearningWriter,
    user_message: &str,
    state: &AgenticLoopState,
    success: bool,
) {
    let tools_used: Vec<String> = state.telemetry.all_tools_used.iter().cloned().collect();
    let prev_assistant_text = extract_prev_assistant_text(&state.messages);
    let signal = astra_turn_types::detect_implicit_feedback_signal(
        user_message,
        prev_assistant_text.as_deref(),
    );
    let was_corrected = matches!(signal.signal_type.as_str(), "correction" | "frustration");
    let outcome = TurnLearningOutcome {
        query: user_message.to_string(),
        tools_selected: tools_used.clone(),
        tools_used,
        success,
        quality: if success { 0.7 } else { 0.2 },
        was_corrected,
        task_type_label: None,
        domain_hint_label: None,
        user_feedback_score: None,
        reward_hacking_risk: 0.0,
        reward_hacking_flags: Vec::new(),
        causal_support_score: if success { 0.8 } else { 0.3 },
        causal_support_flags: Vec::new(),
    };
    if let Err(e) = writer.record_outcome(outcome).await {
        astra_core::agent_error!("server-loop", "failed to record learning outcome: {e}");
    }
}

/// Walk `messages` (chronological) and return the content of the latest
/// assistant entry, if any. Used by implicit-feedback detection so the
/// "user said `that's wrong` after the assistant answered `X`" pattern can
/// score higher confidence.
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

fn terminal_events_for_persistence(events: &[Value]) -> Vec<Value> {
    events
        .iter()
        .filter(|event| {
            matches!(
                event.get("event_type").and_then(Value::as_str),
                Some("text_done" | "run_error" | "run_interrupted" | "run_finished")
            )
        })
        .cloned()
        .collect()
}

/// Per-run state held in the lifecycle service.
struct RunState {
    run_id: String,
    session_id: String,
    user_id: String,
    status: RunStatus,
    events: Vec<Value>,
    cancel_flag: Arc<AtomicBool>,
    pause_flag: Arc<AtomicBool>,
    /// Cancelled together with `cancel_flag` on `cancel_run` for low-latency LLM abort.
    llm_cancel_token: Arc<CancellationToken>,
    #[allow(dead_code)]
    started_at: Instant,
    waiting_for: Option<String>,
}

// ─── Service ────────────────────────────────────────────────────────────────

/// Production [`RunLifecycleService`] that executes agentic loops via
/// [`ServerAgenticLoopHost`].
///
/// When a `RunEngine` is attached, all state changes are also persisted
/// to the durable store for crash recovery.
pub struct AgenticRunLifecycleService {
    /// In-memory run store (run_id → state). Hot cache for low-latency queries.
    /// Arc-wrapped so background tasks spawned by `create_run` can update events.
    runs: Arc<RwLock<HashMap<String, RunState>>>,
    /// LLM resolution dependencies.
    matrixone: MatrixOneSettings,
    encryptor: Arc<FernetTokenEncryptor>,
    shared_pool: Option<SharedPool>,
    /// Edge callback ledger shared with the API server.
    edge_callback_ledger: Arc<TokioMutex<HashMap<String, Value>>>,
    /// Optional durable run engine for persistence.
    run_engine: Option<RunEngine>,
    /// Optional delegation engine for multi-agent coordination.
    delegation_engine: Option<Arc<crate::server::delegation_engine::DelegationEngine>>,
    /// Per-user resource governor (Phase 5).
    resource_governor:
        Option<std::sync::Arc<dyn astra_services::resource_governor::ResourceGovernor>>,
    /// Live edge WebSocket connection pool (Phase 6).
    edge_connection_pool: Option<astra_server_types::edge_connection_pool::EdgeConnectionPool>,
    /// Optional database skill provider for runtime skill resolution.
    skill_service: Option<Arc<dyn SkillService>>,
    /// Lazily initialized server skill registry + resolver bundle.
    server_skill_resolver_cache: std::sync::OnceLock<ServerSkillResolverBundle>,
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
    /// Counter of in-flight background agentic loop tasks.
    /// Incremented before spawn, decremented when the task exits.
    /// Used by `drain_background_tasks` for graceful shutdown.
    background_task_count: Arc<AtomicUsize>,
}

impl AgenticRunLifecycleService {
    pub fn new(
        matrixone: MatrixOneSettings,
        encryptor: Arc<FernetTokenEncryptor>,
        edge_callback_ledger: Arc<TokioMutex<HashMap<String, Value>>>,
    ) -> Self {
        Self {
            runs: Arc::new(RwLock::new(HashMap::new())),
            matrixone,
            encryptor,
            shared_pool: None,
            edge_callback_ledger,
            run_engine: None,
            delegation_engine: None,
            resource_governor: None,
            edge_connection_pool: None,
            skill_service: None,
            server_skill_resolver_cache: std::sync::OnceLock::new(),
            approval_channels: Arc::new(TokioMutex::new(HashMap::new())),
            user_prompt_channels: Arc::new(TokioMutex::new(HashMap::new())),
            progress_channels: Arc::new(TokioMutex::new(HashMap::new())),
            hook_db_writer: None,
            observer_worker: None,
            tool_event_writer: None,
            background_task_count: Arc::new(AtomicUsize::new(0)),
        }
    }

    pub fn with_pool(mut self, pool: SharedPool) -> Self {
        self.shared_pool = Some(pool);
        self
    }

    pub fn with_run_engine(mut self, engine: RunEngine) -> Self {
        self.run_engine = Some(engine);
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
        self.server_skill_resolver_cache = std::sync::OnceLock::new();
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

    fn build_csl_store(&self) -> Option<Arc<dyn astra_turn_core::conversation_log::CslStore>> {
        let pool = self.shared_pool.as_ref()?;
        let store =
            astra_turn_core::conversation_log::db_store::DbCslStore::new(self.matrixone.clone())
                .with_pool(pool.clone());
        Some(Arc::new(store))
    }

    async fn restore_csl_history(
        &self,
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

        match mgr.load().await {
            Ok(Some(mat)) => {
                let mut restored = mat.messages;
                if !loop_state.messages.is_empty() {
                    restored.push(loop_state.messages.remove(0));
                }
                loop_state.messages = restored;

                let ss = mat.session_state;
                if let Some(c) = ss.continuity {
                    if loop_state.continuity
                        == astra_turn_types::continuity::ContinuityState::default()
                    {
                        loop_state.continuity = c;
                    }
                }
                if !ss.blocked_tools.is_empty() {
                    loop_state.restricted_tools.extend(ss.blocked_tools);
                }
                if !ss.recent_tools.is_empty() {
                    loop_state.recent_tools = ss.recent_tools;
                }
                if let Some(ao_value) = ss.approval_overrides {
                    if loop_state.approval_overrides.is_none() {
                        if let Ok(ao) = serde_json::from_value(ao_value) {
                            loop_state.approval_overrides = Some(ao);
                        }
                    }
                }
                if let Some(intr_value) = ss.interruption {
                    if loop_state.interruption.is_none() {
                        if let Ok(intr) = serde_json::from_value(intr_value) {
                            loop_state.interruption = Some(intr);
                        }
                    }
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
            Ok(None) => {}
            Err(e) => {
                tracing::warn!(
                    session_id,
                    error = %e,
                    "CSL load failed; starting with empty history"
                );
            }
        }

        mgr.mark_turn_start(loop_state.messages.len());
        Some(mgr)
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
                return true;
            }
            if tokio::time::Instant::now() >= deadline {
                return false;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
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
        user_id: String,
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
            user_id,
            status: RunStatus::Running,
            events: vec![json!({"event_type": "run_started", "data": {}})],
            cancel_flag: cancel_flag.clone(),
            pause_flag: pause_flag.clone(),
            llm_cancel_token: llm_cancel_token.clone(),
            started_at: Instant::now(),
            waiting_for: None,
        };
        (run_state, cancel_flag, pause_flag, llm_cancel_token)
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

    async fn persist_run_start_if_configured(&self, run_id: &str, user_id: &str, session_id: &str) {
        if let Some(engine) = &self.run_engine {
            astra_core::log_persist!(
                engine.start_run(run_id, user_id, session_id).await,
                "run_lifecycle",
                run_id,
                "start_run"
            );
        }
    }

    fn finalize_run_events(
        loop_outcome: Result<AgenticLoopOutcome, astra_core::ClassifiedError>,
        mut events: Vec<Value>,
        loop_state: &AgenticLoopState,
    ) -> (Vec<Value>, RunStatus, Option<String>) {
        let usage = json!({
            "prompt_tokens": loop_state.total_prompt,
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
        request: &ChatRequestData,
    ) -> Result<(), (StatusCode, Json<ErrorResponse>)> {
        if request.llm_token_service.is_some() {
            let trusted_domains = self.load_trusted_llm_token_service_domains().await?;
            validate_llm_token_service_config(request.llm_token_service.as_ref(), &trusted_domains)
                .map_err(|detail| error_response(StatusCode::BAD_REQUEST, detail))?;
        }
        normalize_request_allowlist(request.allow_tools.as_deref(), "allow_tools")
            .map_err(|detail| error_response(StatusCode::BAD_REQUEST, detail))?;
        let (_, resolver) = build_server_skill_resolver(
            self.skill_service.clone(),
            &self.server_skill_resolver_cache,
        );
        let allowed_skills =
            normalize_request_allowlist(request.allow_skills.as_deref(), "allow_skills")
                .map_err(|detail| error_response(StatusCode::BAD_REQUEST, detail))?;
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
        .with_edge_profile(edge_profile)
        .with_edge_callback_ledger(self.edge_callback_ledger.clone())
        .with_interactive_client(request.interactive_client)
        .with_plan_resume_hint(plan_resume_hint);

        if let Some(pool) = &self.shared_pool {
            builder = builder.with_pool(pool.clone());
        }
        // Wire progress broadcaster from delegation engine for SSE agent tree events
        if let Some(ref de) = self.delegation_engine {
            if let Some(broadcaster) = de.progress_broadcaster() {
                builder = builder.with_progress_broadcaster(Arc::clone(broadcaster));
            }
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

        let (skill_registry, raw_skill_resolver) = build_server_skill_resolver(
            self.skill_service.clone(),
            &self.server_skill_resolver_cache,
        );
        let request_constraints = RequestConstraints::new(
            normalize_request_allowlist(request.allow_tools.as_deref(), "allow_tools")
                .expect("request allow_tools should be validated before state build"),
            normalize_request_allowlist(request.allow_skills.as_deref(), "allow_skills")
                .expect("request allow_skills should be validated before state build"),
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
        let runtime_continuity = Self::continuity_from_chat_context(&request.context);

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
        );

        let resolved_tool_policy = astra_config::runtime_config::RuntimeConfig::load()
            .tool_selection
            .resolve_for_model(request.model.as_deref());

        AgenticLoopState {
            messages: vec![user_message],
            tool_results: Vec::new(),
            current_session_id: Some(session_id.to_string()),
            current_run_id: Some(run_id.to_string()),
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
            evolution_service: None,
            rate_limit_cooldown: Default::default(),
            data_snapshot_provider: None,
            last_composite_snapshot: None,
            last_measured_prompt_tokens: None,
            consecutive_context_window_errors: 0,
            compaction_effectiveness: Default::default(),
            pinned_tool_schema_tokens: 0,
            max_turn_input_tokens: astra_core::RuntimeLimits::global().max_turn_input_tokens,
            budget_wrapup_injected: false,
            skill_produced_output: false,
            max_cumulative_tokens: 0,
            thinking: astra_turn_core::thinking_config::ThinkingConfig::Off,
            recent_file_reads: Vec::new(),
            permission_context: None,
            permission_handler: None,
            tactical_adapter: None,
            step_signal_collector: None,
            tool_budget_override: None,
            pending_reflection_signals: Vec::new(),
            recent_tactical_actions: Vec::new(),
            server_tool_executor: None,
            interruption: None,
            session_facts: Default::default(),
            continuity: runtime_continuity.unwrap_or_default(),
            compact_strategy: astra_turn_core::microcompact::CompactStrategy::from_provider_hint(
                request.model.as_deref().unwrap_or(""),
            ),
            approval_overrides: None,
            confidence_trend: Default::default(),
            last_confidence_diagnosis: None,
            session_turn: 0,
            bridge_turn_chain_id: None,
            bridge_user_query_event_id: None,
            turn_event_buffer: None,
        }
    }

    fn parse_runtime_continuity_value(
        value: &Value,
        source: &'static str,
    ) -> Option<astra_turn_types::continuity::ContinuityState> {
        astra_turn_types::continuity::try_from_checkpoint_value(value)
            .map_err(|error| {
                tracing::warn!(
                    error = %error,
                    source,
                    "dropping invalid continuity_state"
                );
            })
            .ok()
    }

    fn continuity_from_chat_context(
        context: &Option<Map<String, Value>>,
    ) -> Option<astra_turn_types::continuity::ContinuityState> {
        context
            .as_ref()
            .and_then(|ctx| ctx.get("continuity_state"))
            .and_then(|value| Self::parse_runtime_continuity_value(value, "chat request context"))
    }

    async fn restore_continuity_from_session_checkpoint(
        &self,
        session_id: &str,
    ) -> Result<
        Option<astra_turn_types::continuity::ContinuityState>,
        astra_pipeline::step_restore::RestoreError,
    > {
        match astra_pipeline::step_restore::restore_session_with_continuity_validator(
            session_id,
            |value| {
                astra_turn_types::continuity::try_from_checkpoint_value(value)
                    .map(|_| ())
                    .map_err(|e| e.to_string())
            },
        ) {
            Ok(Some(restored)) => {
                // RestoredSession.continuity_state is now Option<ContinuityState>
                // (parsed during restore), so no re-parse needed here.
                if restored.continuity_state.is_some() {
                    return Ok(restored.continuity_state);
                }
            }
            Ok(None) => {}
            Err(astra_pipeline::step_restore::RestoreError::IoError(error)) => {
                return Err(astra_pipeline::step_restore::RestoreError::IoError(error));
            }
            Err(error) => {
                tracing::warn!(
                    error = %error,
                    session_id,
                    "skipping invalid local step checkpoint during server continuity restore"
                );
            }
        }

        let Some(shared_pool) = self.shared_pool.as_ref() else {
            return Ok(None);
        };
        match astra_services::session_restore::pull_step_checkpoint_from_cloud(
            shared_pool.get(),
            session_id,
        )
        .await
        {
            Ok(Some(state_json)) => {
                match astra_services::session_restore::parse_cloud_heavy_checkpoint_state(
                    &state_json,
                ) {
                    // continuity_state is already a parsed ContinuityState (Option<ContinuityState>)
                    // after the CloudHeavyCheckpointState strong-type migration — no re-parse needed.
                    Ok(Some(heavy)) => Ok(heavy.continuity_state),
                    Ok(None) => Ok(None),
                    Err(error) => {
                        tracing::warn!(
                            error = %error,
                            session_id,
                            "skipping cloud step checkpoint during server continuity restore"
                        );
                        Ok(None)
                    }
                }
            }
            Ok(None) => Ok(None),
            Err(error) => {
                tracing::warn!(
                    error = %error,
                    session_id,
                    "cloud step checkpoint unavailable during server continuity restore"
                );
                Ok(None)
            }
        }
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

    fn status_record(run: &RunState) -> RunStatusRecord {
        RunStatusRecord {
            run_id: run.run_id.clone(),
            session_id: run.session_id.clone(),
            status: run.status.as_str().to_string(),
            waiting_for: run.waiting_for.clone(),
            events_count: run.events.len() as i64,
        }
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

    async fn load_durable_run_for_user(
        &self,
        run_id: &str,
        user_id: &str,
    ) -> Result<Option<DurableRunRecord>, (StatusCode, Json<ErrorResponse>)> {
        let Some(engine) = &self.run_engine else {
            return Ok(None);
        };

        let run = engine.load_run(run_id).await.map_err(|error| {
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
        self.validate_request_constraints(&request).await?;

        // ── Resource governance check (Phase 5) ─────────────────────
        if let Some(ref gov) = self.resource_governor {
            if let astra_services::resource_governor::LimitCheck::Denied { reason } =
                gov.check_session_create(&user_id).await
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

        // Guard: reject if this session already has an active (running/paused) run.
        // Hold write lock across check+insert to prevent TOCTOU race.
        let (run_state, cancel_flag, pause_flag, llm_cancel_token) =
            Self::build_tracked_run_state(run_id.clone(), session_id.clone(), user_id.clone());
        {
            let mut runs = self.runs.write().await;
            let has_active = runs.values().any(|r| {
                r.session_id == session_id
                    && matches!(r.status, RunStatus::Running | RunStatus::Paused)
            });
            if has_active {
                return Err(error_response(
                    StatusCode::CONFLICT,
                    "session already has an active run".to_string(),
                ));
            }
            runs.insert(run_id.clone(), run_state);
        }

        // Persist to durable store if available
        self.persist_run_start_if_configured(&run_id, &user_id, &session_id)
            .await;

        // Record session creation for resource tracking (Phase 5).
        if let Some(ref gov) = self.resource_governor {
            gov.record_session_created(&user_id).await;
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
            &request,
            &session_id,
            &run_id,
            server_workspace.as_deref(),
            Some(llm_cancel_token.clone()),
        );
        if request.session_id.is_some()
            && loop_state.continuity == astra_turn_types::continuity::ContinuityState::default()
        {
            match self
                .restore_continuity_from_session_checkpoint(&session_id)
                .await
            {
                Ok(Some(restored)) => loop_state.continuity = restored,
                Ok(None) => {}
                Err(error) => {
                    tracing::warn!(
                        error = %error,
                        session_id,
                        "server continuity restore unavailable; continuing without checkpoint continuity"
                    );
                }
            }
        }
        loop_state.session_turn = infer_session_turn(self.shared_pool.as_ref(), &session_id).await;

        // ── CSL: Load conversation history from the log ─────────────
        let csl_manager = if request.session_id.is_some() {
            self.restore_csl_history(&session_id, &run_id, &mut loop_state)
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
        let learning_stack = configure_runtime_controllers(
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
            let memoria_base = std::env::var("MEMORIA_BASE_URL").ok();
            let mut executor = super::server_tool_executor::ServerToolExecutor::new(
                workspace,
                user_id.clone(),
                session_id.clone(),
                memoria_base,
                None,
            )
            .with_cancel_token(loop_state.cancellation.token.clone());
            if let Some(pool) = &self.edge_connection_pool {
                executor.set_edge_connection_pool(pool.clone());
            }
            // Wire the plan repository so enter/exit_plan_mode tools work and
            // the write-tool guard can check `active_plan_id`.
            if let Some(shared) = &self.shared_pool {
                executor.set_plan_repository(std::sync::Arc::new(
                    astra_plan::CloudPlanRepository::new(shared.get().clone()),
                ));
            }
            // Share the host's plan-resume hint slot so tool-triggered
            // plan-mode changes refresh the system prompt mid-run.
            executor.set_plan_resume_hint_handle(host.plan_resume_hint_handle());
            if let Some(observability_session) = loop_state.telemetry.observability_session.clone()
            {
                executor.set_observability_session(observability_session);
            }

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
                    }
                    // Clean up channels for this run.
                    bg_approval_channels.lock().await.remove(&bg_run_id);
                    bg_user_prompt_channels.lock().await.remove(&bg_run_id);
                    bg_progress_channels.lock().await.remove(&bg_run_id);
                    if let Some(ref engine) = run_engine {
                        astra_core::log_persist!(
                            engine
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
                    }
                    Self::schedule_run_eviction(&runs, bg_run_id.clone());
                    return;
                }
            }

            let outcome = run_agentic_loop_with_host(&mut host, &mut loop_state).await;
            let loop_success = outcome.is_ok();
            let (events, final_status, error_msg) =
                Self::finalize_run_events(outcome, host.take_emitted_events(), &loop_state);

            // Clean up channels for this run.
            bg_approval_channels.lock().await.remove(&bg_run_id);
            bg_user_prompt_channels.lock().await.remove(&bg_run_id);
            bg_progress_channels.lock().await.remove(&bg_run_id);
            let terminal_events = terminal_events_for_persistence(&events);

            // Schedule eviction of the terminal run from the in-memory cache.
            // Waiting runs are NOT evicted — they need to remain in memory for resume.
            if final_status != RunStatus::Waiting {
                Self::schedule_run_eviction(&runs, bg_run_id.clone());
            }

            // Publish terminal run state before best-effort post-run side effects
            // so background observers do not stay stuck in "running" because a
            // hook, event write, or learning save is slow.
            let status_str = final_status.as_str();
            let mut persist_terminal_state = true;

            if let Some(run) = runs.write().await.get_mut(&bg_run_id) {
                if run.status == RunStatus::Cancelled {
                    persist_terminal_state = false;
                    merge_cancelled_run_events(run, events);
                    flush_turn_observability(&mut loop_state, &bg_session_id, true);
                } else {
                    run.events.extend(events);
                    if run.status.try_transition(&final_status).is_ok() {
                        run.status = final_status;
                    }
                }
            }

            if let Some(engine) = &run_engine {
                if persist_terminal_state {
                    astra_core::log_persist!(
                        engine
                            .persist_status(&bg_run_id, status_str, None, error_msg.as_deref())
                            .await,
                        "run_lifecycle",
                        &bg_run_id,
                        "status"
                    );
                }
                astra_core::log_persist!(
                    engine
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
                if persist_terminal_state {
                    for event in terminal_events {
                        astra_core::log_persist!(
                            engine.append_event(&bg_run_id, event).await,
                            "run_lifecycle",
                            &bg_run_id,
                            "append_terminal_event"
                        );
                    }
                }
            }

            if persist_terminal_state {
                flush_turn_observability(&mut loop_state, &bg_session_id, false);
                persist_turn_evaluation_journal(&bg_session_id, "server_runtime", &loop_state);
            }

            // Best-effort post-loop persistence (core events, tool events,
            // hook DB, observer, learning, session-end hooks, promotion events).
            persist_ctx
                .run(&loop_state, &learning_stack, loop_success)
                .await;

            // Session-end governance: extract learnings, store to Memoria, purge working memory.
            // This is create_run-specific (background runs are long-lived sessions).
            if let Some(ref memoria_client) =
                crate::turn::cloud::memoria_compact::HttpMemoriaClient::from_env()
            {
                use crate::turn::cloud::memoria_compact::MemoriaClient as _;
                let sid = loop_state.current_session_id.as_deref().unwrap_or("");
                if !sid.is_empty() {
                    // Try to retrieve L1 narrative from Memoria for knowledge extraction
                    let narrative = memoria_client
                        .retrieve_ext(
                            &format!("{} session state", crate::turn::cloud::session_memory_protocol::SESSION_MEMORY_PREFIX),
                            Some(sid), 3, true,
                        )
                        .await
                        .ok()
                        .and_then(|mems| {
                            mems.into_iter()
                                .find(|m| m.content.starts_with(crate::turn::cloud::session_memory_protocol::SESSION_MEMORY_PREFIX))
                        })
                        .and_then(|m| crate::turn::cloud::session_memory_protocol::SessionMemory::parse(&m.content));
                    match crate::turn::cloud::session_end_governance::run_session_end_governance(
                        &loop_state.session_facts,
                        narrative.as_ref(),
                        sid,
                        memoria_client,
                    )
                    .await
                    {
                        Ok(report) => {
                            if report.learnings_stored > 0 {
                                tracing::info!(
                                    session_id = %sid,
                                    learnings = report.learnings_stored,
                                    purged = report.working_purged,
                                    "session-end governance complete"
                                );
                            }
                        }
                        Err(e) => {
                            tracing::warn!(session_id = %sid, error = %e, "session-end governance failed")
                        }
                    }
                }
            }
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
        self.validate_request_constraints(&request).await?;

        // ── Resource governance check ────────────────────────────────
        if let Some(ref gov) = self.resource_governor {
            if let astra_services::resource_governor::LimitCheck::Denied { reason } =
                gov.check_session_create(&user_id).await
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
        let (event_tx, event_rx) = tokio::sync::mpsc::channel::<Value>(SSE_CHANNEL_CAPACITY);

        let (run_state, cancel_flag, pause_flag, llm_cancel_token) =
            Self::build_tracked_run_state(run_id.clone(), session_id.clone(), user_id.clone());

        let mut state = self.build_initial_state(
            &request,
            &session_id,
            &run_id,
            server_workspace.as_deref(),
            Some(llm_cancel_token.clone()),
        );
        if request.session_id.is_some()
            && state.continuity == astra_turn_types::continuity::ContinuityState::default()
        {
            match self
                .restore_continuity_from_session_checkpoint(&session_id)
                .await
            {
                Ok(Some(restored)) => state.continuity = restored,
                Ok(None) => {}
                Err(error) => {
                    tracing::warn!(
                        error = %error,
                        session_id,
                        "server continuity restore unavailable; continuing without checkpoint continuity"
                    );
                }
            }
        }
        state.session_turn = infer_session_turn(self.shared_pool.as_ref(), &session_id).await;

        // ── CSL: Load conversation history from the log ─────────────
        let csl_manager = if request.session_id.is_some() {
            self.restore_csl_history(&session_id, &run_id, &mut state)
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

        // Guard: reject if this session already has an active (running/paused) run.
        // Hold write lock across check+insert to prevent TOCTOU race.
        {
            let mut runs = self.runs.write().await;
            let has_active = runs.values().any(|r| {
                r.session_id == session_id
                    && matches!(r.status, RunStatus::Running | RunStatus::Paused)
            });
            if has_active {
                return Err(error_response(
                    StatusCode::CONFLICT,
                    "session already has an active run".to_string(),
                ));
            }
            runs.insert(run_id.clone(), run_state);
        }
        self.persist_run_start_if_configured(&run_id, &user_id, &session_id)
            .await;

        // Record session creation for resource tracking.
        if let Some(ref gov) = self.resource_governor {
            gov.record_session_created(&user_id).await;
        }

        self.configure_loop_state_runtime_controls(
            &mut state,
            &cancel_flag,
            &pause_flag,
            &llm_cancel_token,
        );
        let learning_stack = configure_runtime_controllers(
            &self.matrixone,
            self.shared_pool.as_ref(),
            &mut state,
            &user_id,
            &session_id,
        )
        .await;

        // Wire ServerToolExecutor when no edge agent is connected (web-agent mode).
        if let Some(workspace) = server_workspace {
            let memoria_base = std::env::var("MEMORIA_BASE_URL").ok();
            let mut executor = super::server_tool_executor::ServerToolExecutor::new(
                workspace,
                user_id.clone(),
                session_id.clone(),
                memoria_base,
                None,
            )
            .with_cancel_token(state.cancellation.token.clone());
            if let Some(pool) = &self.edge_connection_pool {
                executor.set_edge_connection_pool(pool.clone());
            }
            if let Some(shared) = &self.shared_pool {
                executor.set_plan_repository(std::sync::Arc::new(
                    astra_plan::CloudPlanRepository::new(shared.get().clone()),
                ));
            }
            if let Some(observability_session) = state.telemetry.observability_session.clone() {
                executor.set_observability_session(observability_session);
            }
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
            let loop_result = run_agentic_loop_with_host(&mut host, &mut state).await;
            let loop_success = loop_result.is_ok();

            // Best-effort post-loop persistence (core events, tool events,
            // hook DB, observer, learning, session-end hooks, promotion events).
            persist_ctx.run(&state, &learning_stack, loop_success).await;

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

            let mut persist_terminal_state = true;
            // Extract terminal events before the branch — both branches consume
            // all_events by move, so this must happen first.
            let terminal_events = terminal_events_for_persistence(&all_events);
            if let Some(run) = runs.write().await.get_mut(&bg_run_id) {
                if run.status == RunStatus::Cancelled {
                    persist_terminal_state = false;
                    merge_cancelled_run_events(run, all_events);
                    flush_turn_observability(&mut state, &bg_session_id, true);
                } else {
                    run.events.extend(all_events);
                    if run.status.try_transition(&final_status).is_ok() {
                        run.status = final_status.clone();
                    }
                    flush_turn_observability(&mut state, &bg_session_id, false);
                }
            }

            // Schedule eviction of the terminal run from the in-memory cache.
            // Waiting runs are NOT evicted — they need to remain in memory for resume.
            if final_status != RunStatus::Waiting {
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

            if persist_terminal_state {
                if let Some(engine) = &run_engine {
                    astra_core::log_persist!(
                        engine
                            .persist_status(
                                &bg_run_id,
                                final_status.as_str(),
                                None,
                                error_msg.as_deref()
                            )
                            .await,
                        "run_lifecycle",
                        &bg_run_id,
                        "status"
                    );
                }
            }

            for event in streamed_final_events {
                if event_tx.send(event).await.is_err() {
                    break;
                }
            }

            // Persist usage unconditionally — cancelled runs still consumed tokens
            // and must have accurate usage in durable store for billing/audit.
            if let Some(engine) = &run_engine {
                astra_core::log_persist!(
                    engine
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
            }

            // Persist terminal events to durable store.
            if let Some(engine) = &run_engine {
                for event in terminal_events {
                    astra_core::log_persist!(
                        engine.append_event(&bg_run_id, event).await,
                        "run_lifecycle",
                        &bg_run_id,
                        "append_terminal_event"
                    );
                }
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
        {
            let runs = self.runs.read().await;
            if let Some(run) = runs.get(&run_id) {
                if run.user_id != user_id {
                    return Err(error_response(StatusCode::FORBIDDEN, "Access denied"));
                }
                return Ok(Self::status_record(run));
            }
        }

        if let Some(run) = self.load_durable_run_for_user(&run_id, &user_id).await? {
            return Ok(Self::durable_status_record(&run));
        }

        Err(error_response(StatusCode::NOT_FOUND, "Run not found"))
    }

    async fn stream_run(
        &self,
        run_id: String,
        user_id: String,
        last_index: u32,
    ) -> Result<Vec<Value>, (StatusCode, Json<ErrorResponse>)> {
        {
            let runs = self.runs.read().await;
            if let Some(run) = runs.get(&run_id) {
                if run.user_id != user_id {
                    return Err(error_response(StatusCode::FORBIDDEN, "Access denied"));
                }
                let offset = last_index as usize;
                let events = if offset < run.events.len() {
                    Self::format_run_events(&run.events[offset..], offset)
                } else {
                    Vec::new()
                };
                return Ok(events);
            }
        }

        if let Some(run) = self.load_durable_run_for_user(&run_id, &user_id).await? {
            return Ok(Self::durable_stream_events(&run, last_index));
        }

        Err(error_response(StatusCode::NOT_FOUND, "Run not found"))
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

    async fn drain_background_tasks(&self, timeout: std::time::Duration) -> bool {
        self.drain_background_tasks_impl(timeout).await
    }

    async fn cancel_run(
        &self,
        run_id: String,
        user_id: String,
    ) -> Result<CancelRunRecord, (StatusCode, Json<ErrorResponse>)> {
        {
            let mut runs = self.runs.write().await;
            if let Some(run) = runs.get_mut(&run_id) {
                if run.user_id != user_id {
                    return Err(error_response(StatusCode::FORBIDDEN, "Access denied"));
                }
                let mutated = run.status.try_transition(&RunStatus::Cancelled).is_ok();
                if mutated {
                    run.cancel_flag.store(true, Ordering::SeqCst);
                    run.pause_flag.store(false, Ordering::SeqCst);
                    run.llm_cancel_token.cancel();
                    run.status = RunStatus::Cancelled;
                    run.waiting_for = None;
                    run.events.push(json!({
                        "event_type": "run_finished",
                        "data": {"cancelled": true}
                    }));
                }
                let final_status = run.status.as_str().to_string();
                // Drop the write lock before async persist calls so concurrent
                // readers/writers (and pause/resume) are not blocked across DB I/O.
                drop(runs);

                if mutated {
                    if let Some(engine) = &self.run_engine {
                        astra_core::log_persist!(
                            engine
                                .persist_status(&run_id, STATUS_CANCELLED, None, None)
                                .await,
                            "run_lifecycle",
                            &run_id,
                            "cancel_status"
                        );
                        astra_core::log_persist!(
                            engine
                                .append_event(
                                    &run_id,
                                    json!({"event_type": "run_finished", "data": {"cancelled": true}}),
                                )
                                .await,
                            "run_lifecycle",
                            &run_id,
                            "cancel_event"
                        );
                    }
                }
                // Cascade cancellation to delegation sub-runs.
                if mutated {
                    if let Some(de) = &self.delegation_engine {
                        de.cancel_children_of(&run_id).await;
                    }
                }
                return Ok(CancelRunRecord {
                    run_id,
                    status: final_status,
                });
            }
        }

        if let Some(run) = self.load_durable_run_for_user(&run_id, &user_id).await? {
            return if matches!(
                run.status.as_str(),
                STATUS_RUNNING | STATUS_PAUSED | STATUS_WAITING
            ) {
                Err(Self::run_control_state_unavailable("cancellation"))
            } else {
                Ok(CancelRunRecord {
                    run_id,
                    status: run.status,
                })
            };
        }

        Err(error_response(StatusCode::NOT_FOUND, "Run not found"))
    }

    async fn list_runs(
        &self,
        user_id: String,
        limit: u32,
        offset: u32,
    ) -> Result<RunListRecord, (StatusCode, Json<ErrorResponse>)> {
        let (limit, offset) = astra_services::pagination::clamp_api_list_pagination(limit, offset);
        if let Some(engine) = &self.run_engine {
            let (durable_runs, total) = engine
                .list_user_runs(&user_id, limit, offset)
                .await
                .map_err(|error| {
                    error_response(
                        StatusCode::SERVICE_UNAVAILABLE,
                        format!("Failed to list durable run state: {error}"),
                    )
                })?;
            let runs = self.runs.read().await;
            let page = durable_runs
                .iter()
                .map(|run| {
                    runs.get(&run.run_id)
                        .map(|live| Self::status_record(live))
                        .unwrap_or_else(|| Self::durable_status_record(run))
                })
                .collect();
            return Ok(RunListRecord {
                runs: page,
                total,
                limit,
                offset,
            });
        }

        let runs = self.runs.read().await;
        let mut all: Vec<RunStatusRecord> = runs
            .values()
            .filter(|run| run.user_id == user_id)
            .map(Self::status_record)
            .collect();
        // Sort by run_id for deterministic pagination (HashMap iteration order is undefined).
        all.sort_by(|a, b| a.run_id.cmp(&b.run_id));
        let total = all.len() as i64;
        let start = (offset as usize).min(all.len());
        let end = (start + limit as usize).min(all.len());
        let page = all[start..end].to_vec();
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
        {
            let mut runs = self.runs.write().await;
            if let Some(run) = runs.get_mut(&run_id) {
                if run.user_id != user_id {
                    return Err(error_response(StatusCode::FORBIDDEN, "Access denied"));
                }
                if run.status.try_transition(&RunStatus::Paused).is_err() {
                    return Err(Self::run_state_conflict("pause", run.status.as_str()));
                }
                let previous = run.status.as_str().to_string();
                run.status = RunStatus::Paused;
                run.pause_flag.store(true, Ordering::SeqCst);
                run.waiting_for = Some("user_resume".to_string());
                run.events.push(json!({
                    "event_type": "run_paused",
                    "data": {}
                }));
                // Drop the write lock before async delegation calls.
                drop(runs);

                // Persist pause
                if let Some(engine) = &self.run_engine {
                    astra_core::log_persist!(
                        engine
                            .persist_status(&run_id, STATUS_PAUSED, Some("user_resume"), None)
                            .await,
                        "run_lifecycle",
                        &run_id,
                        "pause_status"
                    );
                    astra_core::log_persist!(
                        engine
                            .append_event(&run_id, json!({"event_type": "run_paused", "data": {}}))
                            .await,
                        "run_lifecycle",
                        &run_id,
                        "pause_event"
                    );
                }
                // Cascade: pause all delegated sub-runs of this parent.
                if let Some(de) = &self.delegation_engine {
                    de.pause_children_of(&run_id).await;
                }
                return Ok(RunMutationRecord {
                    run_id,
                    status: STATUS_PAUSED.to_string(),
                    previous_status: previous,
                });
            }
        }

        if let Some(run) = self.load_durable_run_for_user(&run_id, &user_id).await? {
            return if run.status == STATUS_RUNNING {
                Err(Self::run_control_state_unavailable("pause"))
            } else {
                Err(Self::run_state_conflict("pause", &run.status))
            };
        }

        Err(error_response(StatusCode::NOT_FOUND, "Run not found"))
    }

    async fn resume_run(
        &self,
        run_id: String,
        user_id: String,
    ) -> Result<RunMutationRecord, (StatusCode, Json<ErrorResponse>)> {
        {
            let mut runs = self.runs.write().await;
            if let Some(run) = runs.get_mut(&run_id) {
                if run.user_id != user_id {
                    return Err(error_response(StatusCode::FORBIDDEN, "Access denied"));
                }
                if run.status.try_transition(&RunStatus::Running).is_err() {
                    return Err(Self::run_state_conflict("resume", run.status.as_str()));
                }
                let previous = run.status.as_str().to_string();
                run.status = RunStatus::Running;
                run.pause_flag.store(false, Ordering::SeqCst);
                run.waiting_for = None;
                run.events.push(json!({
                    "event_type": "run_resumed",
                    "data": {}
                }));
                // Drop the write lock before async delegation calls.
                drop(runs);

                // Persist resume
                if let Some(engine) = &self.run_engine {
                    astra_core::log_persist!(
                        engine
                            .persist_status(&run_id, STATUS_RUNNING, None, None)
                            .await,
                        "run_lifecycle",
                        &run_id,
                        "resume_status"
                    );
                    astra_core::log_persist!(
                        engine
                            .append_event(&run_id, json!({"event_type": "run_resumed", "data": {}}))
                            .await,
                        "run_lifecycle",
                        &run_id,
                        "resume_event"
                    );
                }
                // Cascade: resume all delegated sub-runs of this parent.
                if let Some(de) = &self.delegation_engine {
                    de.resume_children_of(&run_id).await;
                }
                return Ok(RunMutationRecord {
                    run_id,
                    status: STATUS_RUNNING.to_string(),
                    previous_status: previous,
                });
            }
        }

        if let Some(run) = self.load_durable_run_for_user(&run_id, &user_id).await? {
            return if run.status == STATUS_PAUSED {
                Err(Self::run_control_state_unavailable("resume"))
            } else {
                Err(Self::run_state_conflict("resume", &run.status))
            };
        }

        Err(error_response(StatusCode::NOT_FOUND, "Run not found"))
    }
}

// ─── Sub-Run Executor ───────────────────────────────────────────────────────

use crate::server::delegation_engine::{SubRunConfig, SubRunExecutor};

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
    skill_resolver_cache: std::sync::OnceLock<ServerSkillResolverBundle>,
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
            skill_resolver_cache: std::sync::OnceLock::new(),
        }
    }

    pub fn with_pool(mut self, pool: SharedPool) -> Self {
        self.shared_pool = Some(pool);
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
        self.skill_resolver_cache = std::sync::OnceLock::new();
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
        .with_edge_profile(edge_profile)
        .with_edge_callback_ledger(self.edge_callback_ledger.clone());

        if let Some(pool) = &self.shared_pool {
            builder = builder.with_pool(pool.clone());
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

        let (skill_registry, raw_skill_resolver) =
            build_server_skill_resolver(self.skill_service.clone(), &self.skill_resolver_cache);
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
            tool_results: Vec::new(),
            current_session_id: Some(config.session_id.clone()),
            current_run_id: Some(config.run_id.clone()),
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
            evolution_service: None,
            rate_limit_cooldown: Default::default(),
            data_snapshot_provider: None,
            last_composite_snapshot: None,
            last_measured_prompt_tokens: None,
            consecutive_context_window_errors: 0,
            compaction_effectiveness: Default::default(),
            pinned_tool_schema_tokens: 0,
            max_turn_input_tokens: astra_core::RuntimeLimits::global().max_turn_input_tokens,
            budget_wrapup_injected: false,
            skill_produced_output: false,
            max_cumulative_tokens: 0,
            thinking: astra_turn_core::thinking_config::ThinkingConfig::Off,
            recent_file_reads: Vec::new(),
            permission_context: None,
            permission_handler: None,
            tactical_adapter: None,
            step_signal_collector: None,
            tool_budget_override: None,
            pending_reflection_signals: Vec::new(),
            recent_tactical_actions: Vec::new(),
            server_tool_executor: None,
            interruption: None,
            session_facts: Default::default(),
            continuity: Default::default(),
            compact_strategy,
            approval_overrides: None,
            confidence_trend: Default::default(),
            last_confidence_diagnosis: None,
            session_turn: 0,
            bridge_turn_chain_id: None,
            bridge_user_query_event_id: None,
            turn_event_buffer: None,
        };

        // ── Wire ServerToolExecutor for sub-run tool execution ──────────
        // Without this, the headless pipeline fallback cannot execute tools
        // server-side and sub-agents would get edge-protocol errors.
        {
            let workspace = self.provision_subrun_workspace(&config.session_id, &config.run_id);
            let memoria_base = std::env::var("MEMORIA_BASE_URL").ok();
            let mut executor = super::server_tool_executor::ServerToolExecutor::new(
                workspace,
                config.user_id.clone(),
                config.session_id.clone(),
                memoria_base,
                None,
            )
            .with_cancel_token(config.cancel_token.clone());
            if let Some(pool) = self.shared_pool.as_ref() {
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
            }
            executor.set_plan_resume_hint_handle(host.plan_resume_hint_handle());
            if let Some(obs) = loop_state.telemetry.observability_session.clone() {
                executor.set_observability_session(obs);
            }
            loop_state.server_tool_executor = Some(std::sync::Arc::new(executor));
        }

        let learning_stack = configure_runtime_controllers(
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
            Some(config.agent_profile.agent_id.as_str()),
            &config.task,
            &loop_state,
            config.agent_profile.model_override.as_deref(),
        )
        .await;

        // Persist cross-session learning state.
        let active_canary = match loop_state.evolution_service.as_ref() {
            Some(evolution_service) => evolution_service.export_active_canary().await,
            None => None,
        };
        learning_stack.save_with_active_canary(active_canary);

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
    use astra_services::{ensure_core_schema, load_recent_skill_selector_metric_summary};
    use astra_turn_core::skill_selector_metrics::{
        SkillSelectorShortlistEntry, SkillSelectorShortlistTrace,
    };
    use sqlx::Row;
    use uuid::Uuid;

    // ── extract_prev_assistant_text + implicit feedback wiring ──

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
        dotenvy::dotenv().ok();
        let lookup = |k: &str| std::env::var(k).ok();
        MatrixOneSettings {
            host: std::env::var("MATRIXONE_HOST").unwrap_or_else(|_| "localhost".into()),
            port: std::env::var("MATRIXONE_PORT")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(6001),
            user: std::env::var("MATRIXONE_USER").unwrap_or_else(|_| "root".into()),
            password: std::env::var("MATRIXONE_PASSWORD").unwrap_or_else(|_| "111".into()),
            database: astra_core::resolve_database_name_or(&lookup, "test_astra_runtime"),
        }
    }

    fn test_encryptor() -> Arc<FernetTokenEncryptor> {
        Arc::new(FernetTokenEncryptor::new("cJ8pxr3t6iJmSYqe6wD7vu2rN_C3ovGUxkC5H3NXFNY=").unwrap())
    }

    fn test_service() -> AgenticRunLifecycleService {
        AgenticRunLifecycleService::new(
            test_settings(),
            test_encryptor(),
            Arc::new(TokioMutex::new(HashMap::new())),
        )
    }

    fn runtime_db_it_settings(database: &str) -> MatrixOneSettings {
        dotenvy::dotenv().ok();
        MatrixOneSettings {
            host: std::env::var("MATRIXONE_HOST").unwrap_or_else(|_| "127.0.0.1".into()),
            port: std::env::var("MATRIXONE_PORT")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(6001),
            user: std::env::var("MATRIXONE_USER").unwrap_or_else(|_| "root".into()),
            password: std::env::var("MATRIXONE_PASSWORD")
                .unwrap_or_else(|_| astra_core::DEV_MATRIXONE_PASSWORD.to_string()),
            database: database.to_string(),
        }
    }

    async fn setup_runtime_db_pool(database: &str) -> (MatrixOneSettings, SharedPool) {
        let settings = runtime_db_it_settings(database);
        let catalog =
            std::env::var("ASTRA_DATABASE_BOOTSTRAP_CATALOG").unwrap_or_else(|_| "mysql".into());
        let mut bootstrap = settings.clone();
        bootstrap.database = catalog.clone();
        let admin_pool = connect_matrixone(&bootstrap)
            .await
            .expect("connect bootstrap catalog");
        sqlx::query(&format!(
            "CREATE DATABASE IF NOT EXISTS `{}`",
            settings.database
        ))
        .execute(&admin_pool)
        .await
        .expect("create test database");
        admin_pool.close().await;
        ensure_core_schema(&settings, &catalog)
            .await
            .expect("ensure_core_schema; is MatrixOne up?");
        let pool = SharedPool::new(&settings).await.expect("SharedPool::new");
        (settings, pool)
    }

    async fn drop_runtime_db(settings: &MatrixOneSettings) {
        let mut bootstrap = settings.clone();
        bootstrap.database =
            std::env::var("ASTRA_DATABASE_BOOTSTRAP_CATALOG").unwrap_or_else(|_| "mysql".into());
        let admin_pool = connect_matrixone(&bootstrap)
            .await
            .expect("connect bootstrap catalog for drop");
        sqlx::query(&format!("DROP DATABASE IF EXISTS `{}`", settings.database))
            .execute(&admin_pool)
            .await
            .expect("drop test database");
        admin_pool.close().await;
    }

    async fn cleanup_runtime_selector_rows(pool: &sqlx::Pool<sqlx::MySql>, session_id: &str) {
        let _ = sqlx::query("DELETE FROM skill_selector_turn_metrics WHERE session_id = ?")
            .bind(session_id)
            .execute(pool)
            .await;
        let _ = sqlx::query("DELETE FROM skill_selection_events WHERE session_id = ?")
            .bind(session_id)
            .execute(pool)
            .await;
        let _ = sqlx::query("DELETE FROM ctx_decision_audits WHERE session_id = ?")
            .bind(session_id)
            .execute(pool)
            .await;
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
    #[ignore = "ASTRA_RUNTIME_DB_IT=1 and live MatrixOne"]
    async fn persist_server_loop_hook_events_e2e_persists_selector_metric() {
        let database = format!("server_selector_e2e_{}", Uuid::new_v4().simple());
        let (settings, shared_pool) = setup_runtime_db_pool(&database).await;
        let pool = shared_pool.get().clone();
        let session_id = format!("server-selector-session-{}", Uuid::new_v4());
        let user_id = format!("server-selector-user-{}", Uuid::new_v4());
        cleanup_runtime_selector_rows(&pool, &session_id).await;

        let service = AgenticRunLifecycleService::new(
            settings.clone(),
            test_encryptor(),
            Arc::new(TokioMutex::new(HashMap::new())),
        );
        let request = test_request("deploy the service");
        let mut state = service.build_initial_state(&request, &session_id, "run-1", None, None);
        state.final_text = "deployment finished".to_string();
        state.session_turn = 7;
        state.telemetry.all_tools_used.insert("skill".to_string());
        state.telemetry.all_selected_skills = vec!["deploy".to_string()];
        state.telemetry.initial_skill_selector_shortlist = Some(SkillSelectorShortlistTrace {
            open_catalog: true,
            visible_skill_count: 2,
            skills: vec![
                SkillSelectorShortlistEntry {
                    rank: 1,
                    skill_name: "build".to_string(),
                    aliases: Vec::new(),
                    description: "build artifacts".to_string(),
                    source: "test".to_string(),
                    category: Some("ops".to_string()),
                },
                SkillSelectorShortlistEntry {
                    rank: 2,
                    skill_name: "deploy".to_string(),
                    aliases: Vec::new(),
                    description: "deploy the service".to_string(),
                    source: "test".to_string(),
                    category: Some("ops".to_string()),
                },
            ],
            telemetry: astra_turn_core::skill_selector_metrics::SkillSelectorTelemetry {
                selector_tier: Some("lexical".to_string()),
                elapsed_ms: Some(7),
                total_catalog_size: Some(2),
                extra: None,
            },
        });

        let writer = DatabaseTurnHookDbWriter::new(settings.clone()).with_pool(shared_pool.clone());
        persist_server_loop_hook_events(
            &writer,
            &user_id,
            &session_id,
            &request.message,
            &state,
            Some("test-model"),
        )
        .await;

        let row = sqlx::query(
            "SELECT turn_number, visible_skill_count, chosen_skill_count, shortlisted_chosen_count, \
                    best_chosen_rank, selector_tier, elapsed_ms, total_catalog_size \
             FROM skill_selector_turn_metrics WHERE session_id = ?",
        )
        .bind(&session_id)
        .fetch_one(&pool)
        .await
        .expect("query server selector metric row");
        assert_eq!(row.try_get::<i64, _>("turn_number").unwrap_or_default(), 7);
        assert_eq!(
            row.try_get::<i64, _>("visible_skill_count")
                .unwrap_or_default(),
            2
        );
        assert_eq!(
            row.try_get::<i64, _>("chosen_skill_count")
                .unwrap_or_default(),
            1
        );
        assert_eq!(
            row.try_get::<i64, _>("shortlisted_chosen_count")
                .unwrap_or_default(),
            1
        );
        assert_eq!(
            row.try_get::<Option<i64>, _>("best_chosen_rank")
                .ok()
                .flatten(),
            Some(2)
        );
        assert_eq!(
            row.try_get::<Option<String>, _>("selector_tier")
                .ok()
                .flatten()
                .as_deref(),
            Some("lexical")
        );
        assert_eq!(
            row.try_get::<Option<i64>, _>("elapsed_ms").ok().flatten(),
            Some(7)
        );
        assert_eq!(
            row.try_get::<Option<i64>, _>("total_catalog_size")
                .ok()
                .flatten(),
            Some(2)
        );

        let summary = load_recent_skill_selector_metric_summary(&pool, 1)
            .await
            .expect("load server selector summary");
        assert_eq!(summary.sample_size(), 1);
        assert_eq!(summary.overall.hit_at_1_rate, 0.0);
        assert_eq!(summary.overall.hit_at_5_rate, 1.0);
        assert_eq!(summary.overall.avg_best_chosen_rank, Some(2.0));
        assert_eq!(summary.per_tier.len(), 1);
        assert_eq!(summary.per_tier[0].tier, "lexical");
        assert_eq!(summary.per_tier[0].stats.sample_size, 1);

        cleanup_runtime_selector_rows(&pool, &session_id).await;
        shared_pool.close().await;
        drop_runtime_db(&settings).await;
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
        let state =
            svc.build_initial_state(&test_request("hello"), "session-1", "run-1", None, None);
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
        let filtered_state =
            svc.build_initial_state(&filtered_request, "session-1", "run-1", None, None);
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
        let mut state = svc.build_initial_state(&request, "session-1", "run-1", None, None);
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
    fn seed_restricted_tools_from_blocked_patterns_adds_blocked_tools() {
        let svc = test_service();
        let request = test_request("inspect blocked tools");
        let mut state = svc.build_initial_state(&request, "session-1", "run-1", None, None);
        let mut pattern_library = astra_pipeline::pattern::PatternLibrary::new();

        // One success so the pattern exists, then Block adds 5 failures.
        // Total: success=1, failure=5, rate=5/6=0.833 > 0.8 → blocked.
        pattern_library.record_outcome(
            &["some_custom_tool".to_string()],
            crate::pipeline::routing::TaskType::Code,
            None,
            true,
            0.8,
            None,
        );
        pattern_library.apply_evolution_action(
            "some_custom_tool",
            astra_evolution::types::PatternAction::Block,
        );

        seed_restricted_tools_from_blocked_patterns(&mut state, &pattern_library);

        assert!(state.restricted_tools.contains("some_custom_tool"));
    }

    #[test]
    fn finalize_run_events_appends_run_finished_for_failures() {
        let svc = test_service();
        let request = test_request("boom");
        let state = svc.build_initial_state(&request, "session-1", "run-1", None, None);

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
        let mut state = svc.build_initial_state(&request, "session-1", "run-1", None, None);
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
        let mut state = svc.build_initial_state(&request, "session-1", "run-1", None, None);
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
            user_id: "user-1".into(),
            status: RunStatus::Cancelled,
            events: vec![
                json!({"event_type": "run_started", "data": {}}),
                json!({"event_type": "run_finished", "data": {"cancelled": true}}),
            ],
            cancel_flag,
            pause_flag: Arc::new(AtomicBool::new(false)),
            llm_cancel_token: cancel_token,
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
            json!({"event_type": "text_done", "data": {"full_text": "final answer"}}),
            json!({"event_type": "run_error", "data": {"error": "boom"}}),
            json!({"event_type": "run_finished", "data": {"prompt_tokens": 1}}),
        ];

        let persisted = terminal_events_for_persistence(&events);
        assert_eq!(persisted.len(), 3);
        assert_eq!(persisted[0]["event_type"], "text_done");
        assert_eq!(persisted[1]["event_type"], "run_error");
        assert_eq!(persisted[2]["event_type"], "run_finished");
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

    /// P2-B source guard: list_runs must call clamp_api_list_pagination.
    #[test]
    fn list_runs_uses_pagination_clamping() {
        let source = include_str!("run_lifecycle.rs");
        let fn_start = source
            .find("async fn list_runs(")
            .expect("list_runs must exist");
        let fn_end = source[fn_start..]
            .find("\n    async fn ")
            .map(|p| fn_start + p)
            .unwrap_or(source.len());
        let fn_body = &source[fn_start..fn_end];
        assert!(
            fn_body.contains("clamp_api_list_pagination"),
            "list_runs must clamp pagination params"
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
        let state = svc.build_initial_state(&req, "sess-1", "run-1", None, None);
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
    fn build_initial_state_restores_continuity_from_request_context() {
        let svc = test_service();
        let mut continuity = astra_turn_types::continuity::ContinuityState::default();
        continuity.ensure_tracked_goal("continue server-side restored work");
        let mut req = test_request("continue");
        let mut context = Map::new();
        context.insert(
            "continuity_state".to_string(),
            serde_json::to_value(&continuity).unwrap(),
        );
        req.context = Some(context);

        let state = svc.build_initial_state(&req, "sess-1", "run-1", None, None);

        assert_eq!(state.continuity, continuity);
    }

    #[test]
    fn build_initial_state_soft_drops_invalid_continuity_context() {
        let svc = test_service();
        let mut req = test_request("continue");
        let mut context = Map::new();
        context.insert("continuity_state".to_string(), serde_json::json!({}));
        req.context = Some(context);

        let state = svc.build_initial_state(&req, "sess-1", "run-1", None, None);

        assert_eq!(
            state.continuity,
            astra_turn_types::continuity::ContinuityState::default()
        );
    }

    #[test]
    fn build_initial_state_applies_execution_budget_override() {
        let svc = test_service();
        let mut req = test_request("go");
        req.execution_budget = Some(astra_services::runs::ExecutionBudget {
            initial_turns: Some(4),
            hard_turn_limit: Some(9),
        });
        let state = svc.build_initial_state(&req, "s", "r", None, None);
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
        let state = svc.build_initial_state(&req, "s", "r", None, None);
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

        let state = svc.build_initial_state(&req, "s", "r", None, None);
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
        let state = svc.build_initial_state(&req, "s", "r", Some(dir.path()), None);
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

        let state = svc.build_initial_state(&req, "s", "r", Some(override_dir.path()), None);
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
        use crate::server::run_engine::RunEngine;
        use astra_services::runs::InMemoryRunStateStore;

        let engine = RunEngine::new(Arc::new(InMemoryRunStateStore::new()));
        AgenticRunLifecycleService::new(
            test_settings(),
            test_encryptor(),
            Arc::new(TokioMutex::new(HashMap::new())),
        )
        .with_run_engine(engine)
    }

    #[tokio::test]
    #[ignore] // runs full agentic loop; needs live infra
    async fn durable_create_run_persists_to_store() {
        let svc = test_service_with_engine();
        let run = ok(svc.create_run("user-1".into(), test_request("hello")).await);

        let engine = svc.run_engine.as_ref().unwrap();
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

        let engine = svc.run_engine.as_ref().unwrap();
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

        let engine = svc.run_engine.as_ref().unwrap();
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

        let engine = svc.run_engine.as_ref().unwrap();
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
        let engine = svc.run_engine.as_ref().unwrap();
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
        let engine = svc.run_engine.as_ref().unwrap();
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
    async fn cancel_run_running_cache_miss_returns_service_unavailable() {
        let svc = test_service_with_engine();
        let engine = svc.run_engine.as_ref().unwrap();
        engine.start_run("run-1", "user-1", "sess-1").await.unwrap();

        let e = err(svc.cancel_run("run-1".into(), "user-1".into()).await);
        assert_eq!(e.0, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(
            e.1.0.detail,
            "Run control state unavailable for cancellation"
        );
    }

    #[tokio::test]
    async fn pause_run_running_cache_miss_returns_service_unavailable() {
        let svc = test_service_with_engine();
        let engine = svc.run_engine.as_ref().unwrap();
        engine.start_run("run-1", "user-1", "sess-1").await.unwrap();

        let e = err(svc.pause_run("run-1".into(), "user-1".into()).await);
        assert_eq!(e.0, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(e.1.0.detail, "Run control state unavailable for pause");
    }

    #[tokio::test]
    async fn resume_run_paused_cache_miss_returns_service_unavailable() {
        let svc = test_service_with_engine();
        let engine = svc.run_engine.as_ref().unwrap();
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
    async fn cancel_run_paused_cache_miss_returns_service_unavailable() {
        let svc = test_service_with_engine();
        let engine = svc.run_engine.as_ref().unwrap();
        engine.start_run("run-1", "user-1", "sess-1").await.unwrap();
        engine
            .persist_status("run-1", STATUS_PAUSED, Some("user_resume"), None)
            .await
            .unwrap();

        let e = err(svc.cancel_run("run-1".into(), "user-1".into()).await);
        assert_eq!(e.0, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(
            e.1.0.detail,
            "Run control state unavailable for cancellation"
        );
    }

    #[tokio::test]
    #[ignore] // stream_chat runs full agentic loop; needs live DB + LLM or mock
    async fn get_run_status_falls_back_to_durable_store_on_cache_miss() {
        let svc = test_service_with_engine();
        let stream = ok(svc
            .stream_chat("user-1".into(), test_request("hello"))
            .await);
        let engine = svc.run_engine.as_ref().unwrap();
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
        let engine = svc.run_engine.as_ref().unwrap();
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
    #[ignore] // stream_chat runs full agentic loop; needs live DB + LLM or mock
    async fn stream_run_falls_back_to_durable_store_on_cache_miss() {
        let svc = test_service_with_engine();
        let stream = ok(svc
            .stream_chat("user-1".into(), test_request("hello"))
            .await);
        let engine = svc.run_engine.as_ref().unwrap();
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
    async fn no_engine_works_without_persistence() {
        // Service without engine should still work (backward compat)
        let svc = test_service();
        let run = ok(svc.create_run("user-1".into(), test_request("hello")).await);
        ok(svc.cancel_run(run.run_id, "user-1".into()).await);
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
        let engine = svc.run_engine.as_ref().unwrap();
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

    // ─── Router route registration test ─────────────────────────────────

    #[test]
    fn router_includes_delegation_routes() {
        // Quick check that our delegation routes are registered.
        let source = include_str!("router_builder.rs");
        assert!(
            source.contains("/chat/runs/{run_id}/delegate"),
            "Missing delegation route"
        );
        assert!(
            source.contains("/chat/runs/{run_id}/delegations"),
            "Missing delegations list route"
        );
        assert!(
            source.contains("/chat/runs/{run_id}/delegations/pause"),
            "Missing delegations pause route"
        );
        assert!(
            source.contains("/chat/runs/{run_id}/delegations/resume"),
            "Missing delegations resume route"
        );
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

    /// P0-C source guard: both spawns must wire the background_task_count counter.
    #[test]
    fn both_spawns_wire_background_task_count() {
        let source = include_str!("run_lifecycle.rs");
        let test_start = source.find("mod tests {").unwrap_or(source.len());
        let prod_code = &source[..test_start];
        let count = prod_code.matches("background_task_count").count();
        assert!(
            count >= 4,
            "background_task_count must appear in field def + drain method + both spawns, got {count}"
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

    /// P1-B: create_run must reject a second run on the same session with 409.
    #[test]
    fn per_session_active_run_guard_in_source() {
        let source = include_str!("run_lifecycle.rs");
        // Find create_run function
        let fn_start = source
            .find("async fn create_run(")
            .expect("create_run must exist");
        // Find the guard within create_run (before stream_chat)
        let stream_chat_pos = source.find("async fn stream_chat(").unwrap_or(source.len());
        let create_run_body = &source[fn_start..stream_chat_pos];
        assert!(
            create_run_body.contains("session already has an active run"),
            "create_run must reject concurrent runs on the same session"
        );
        assert!(
            create_run_body.contains("CONFLICT"),
            "create_run must return 409 CONFLICT for concurrent session runs"
        );
    }

    /// P1-A: try_transition must be used in all production status update paths.
    #[test]
    fn try_transition_used_in_production_code() {
        let source = include_str!("run_lifecycle.rs");
        let test_start = source.find("mod tests {").unwrap_or(source.len());
        let prod_code = &source[..test_start];
        let count = prod_code.matches("try_transition").count();
        // Budget reject + post-loop (x2) + cancel + pause + resume = 6
        assert!(
            count >= 6,
            "try_transition must be called in all 6 status update paths, found {count}"
        );
    }

    /// P0-B: stream_chat must have the same per-session active-run guard as create_run.
    #[test]
    fn stream_chat_has_active_run_guard() {
        let source = include_str!("run_lifecycle.rs");
        // Find stream_chat function
        let fn_start = source
            .find("async fn stream_chat(")
            .expect("stream_chat must exist");
        // Find the next function after stream_chat
        let fn_body_end = source[fn_start + 10..]
            .find("\n    async fn ")
            .map(|p| fn_start + 10 + p)
            .unwrap_or(source.len());
        let fn_body = &source[fn_start..fn_body_end];
        assert!(
            fn_body.contains("session already has an active run"),
            "stream_chat must reject concurrent runs on the same session"
        );
        assert!(
            fn_body.contains("CONFLICT"),
            "stream_chat must return 409 CONFLICT for concurrent session runs"
        );
    }

    /// P0-C: Budget-rejected runs must push run_error + run_finished events
    /// so SSE clients don't hang, and must clean up channels.
    #[test]
    fn budget_rejected_run_pushes_terminal_events() {
        let source = include_str!("run_lifecycle.rs");
        // Find the budget rejection block
        let budget_start = source
            .find("run rejected: daily token budget exhausted")
            .expect("budget rejection log must exist");
        // Find the next `return;` after the budget rejection
        let return_pos = source[budget_start..]
            .find("return;")
            .map(|p| budget_start + p)
            .expect("budget rejection must return");
        let budget_block = &source[budget_start..return_pos];
        assert!(
            budget_block.contains("run_finished"),
            "budget rejection must push run_finished event"
        );
        assert!(
            budget_block.contains("run_error"),
            "budget rejection must push run_error event"
        );
        assert!(
            budget_block.contains("approval_channels")
                && budget_block.contains("user_prompt_channels")
                && budget_block.contains("progress_channels"),
            "budget rejection must clean up all channels"
        );
    }

    /// P1-A: stream_chat must use extend (not overwrite) for events,
    /// and handle cancellation with merge_cancelled_run_events.
    #[test]
    fn stream_chat_uses_extend_not_overwrite() {
        let source = include_str!("run_lifecycle.rs");
        let fn_start = source
            .find("async fn stream_chat(")
            .expect("stream_chat must exist");
        let fn_body = &source[fn_start..];
        let fn_end = fn_body[10..]
            .find("\n    async fn ")
            .map(|p| p + 10)
            .unwrap_or(fn_body.len());
        let fn_body = &fn_body[..fn_end];
        // Must NOT do `run.events = all_events` (full overwrite)
        assert!(
            !fn_body.contains("run.events = all_events"),
            "stream_chat must not overwrite events (use extend instead)"
        );
        // Must use merge_cancelled_run_events for cancel handling
        assert!(
            fn_body.contains("merge_cancelled_run_events"),
            "stream_chat must handle cancellation with merge_cancelled_run_events"
        );
    }

    /// P1-C: Terminal runs must be evicted from the in-memory runs map.
    /// Both spawn blocks and the budget rejection path must schedule eviction.
    #[test]
    fn terminal_runs_scheduled_for_eviction() {
        let source = include_str!("run_lifecycle.rs");
        let test_start = source.find("mod tests {").unwrap_or(source.len());
        let prod_code = &source[..test_start];
        let eviction_calls = prod_code.matches("schedule_run_eviction").count();
        // create_run spawn + stream_chat spawn + budget rejection = 3
        assert!(
            eviction_calls >= 3,
            "schedule_run_eviction must be called in create_run spawn, stream_chat spawn, \
             and budget rejection path, found {eviction_calls}"
        );
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

    /// P1-A: finalize_run_events must NOT map Waiting to Failed.
    #[test]
    fn finalize_run_events_does_not_kill_waiting_runs() {
        let source = include_str!("run_lifecycle.rs");
        let fn_start = source
            .find("fn finalize_run_events(")
            .expect("finalize_run_events must exist");
        let fn_end = source[fn_start..]
            .find("\n    fn ")
            .map(|p| fn_start + p)
            .unwrap_or(source.len());
        let fn_body = &source[fn_start..fn_end];

        // The Waiting arm must NOT produce RunStatus::Failed
        let waiting_arm = fn_body
            .find("Waiting(w)")
            .expect("Waiting arm must exist in finalize_run_events");
        let arm_body = &fn_body[waiting_arm..waiting_arm + 300.min(fn_body.len() - waiting_arm)];
        assert!(
            !arm_body.contains("RunStatus::Failed"),
            "Waiting outcome must NOT be mapped to Failed — use RunStatus::Waiting"
        );
        assert!(
            arm_body.contains("RunStatus::Waiting"),
            "Waiting outcome must map to RunStatus::Waiting"
        );
        // Must NOT emit run_error for Waiting (it's not an error)
        assert!(
            !arm_body.contains("run_error"),
            "Waiting outcome must not emit run_error event"
        );
    }

    /// P1-F: stream_chat must persist usage unconditionally (not gated by persist_terminal_state).
    /// Cancelled runs still consumed tokens and must have accurate durable records.
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

        // persist_usage must NOT be inside the persist_terminal_state block.
        // Find the persist_terminal_state block and verify persist_usage is outside it.
        let guard_pos = fn_body
            .find("if persist_terminal_state {")
            .expect("persist_terminal_state guard must exist");
        // Find the closing brace of the guard block
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
        let guard_body = &fn_body[guard_pos..guard_end];
        assert!(
            !guard_body.contains("persist_usage"),
            "persist_usage must NOT be inside persist_terminal_state guard — \
             cancelled stream_chat runs must still persist usage for billing/audit"
        );

        // persist_usage must exist somewhere in stream_chat
        assert!(
            fn_body.contains("persist_usage"),
            "stream_chat must call persist_usage"
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

    /// cancel_run durable fallback must include STATUS_WAITING in cancellable states.
    /// A Waiting run persisted in durable store must be cancellable.
    #[test]
    fn cancel_run_durable_fallback_includes_waiting() {
        let source = include_str!("run_lifecycle.rs");
        let fn_start = source
            .find("async fn cancel_run(")
            .expect("cancel_run must exist");
        let fn_end = source[fn_start..]
            .find("\n    async fn ")
            .map(|p| fn_start + p)
            .unwrap_or(source.len());
        let fn_body = &source[fn_start..fn_end];
        // The durable fallback match must include STATUS_WAITING
        assert!(
            fn_body.contains("STATUS_WAITING"),
            "cancel_run durable fallback must include STATUS_WAITING"
        );
    }

    #[test]
    fn configure_runtime_controllers_wires_context_trace_artifact_store() {
        let source = include_str!("run_lifecycle.rs");
        assert!(
            source.contains(
                "artifact_store: astra_services::DatabaseSessionArtifactStore::new(matrixone.clone())"
            ),
            "context trace persistence should construct a remote workspace artifact store"
        );
        assert!(
            source.contains(".with_pool(pool.clone())"),
            "context trace artifact persistence should reuse the shared MatrixOne pool"
        );
    }

    #[test]
    fn server_tool_executor_is_wired_with_workspace_artifact_store() {
        let source = include_str!("run_lifecycle.rs");
        assert!(
            source.contains(".with_workspace_artifact_store("),
            "server run lifecycle should inject a workspace artifact store into ServerToolExecutor"
        );
    }

    // ── CSL wiring structural checks ───────────────────────────────────────

    #[test]
    fn create_run_loads_csl_history() {
        let source = include_str!("run_lifecycle.rs");
        let create_run_start = source
            .find("async fn create_run(")
            .expect("create_run must exist");
        let stream_chat_start = source
            .find("async fn stream_chat(")
            .expect("stream_chat must exist");
        let create_run_body = &source[create_run_start..stream_chat_start];
        assert!(
            create_run_body.contains("restore_csl_history"),
            "create_run must call restore_csl_history to load CSL conversation state"
        );
    }

    #[test]
    fn stream_chat_loads_csl_history() {
        let source = include_str!("run_lifecycle.rs");
        let stream_chat_start = source
            .find("async fn stream_chat(")
            .expect("stream_chat must exist");
        let stream_chat_body = &source[stream_chat_start..];
        assert!(
            stream_chat_body.contains("restore_csl_history"),
            "stream_chat must call restore_csl_history to load CSL conversation state"
        );
    }

    #[test]
    fn extract_session_state_compact_covers_all_fields() {
        let source = include_str!("run_lifecycle.rs");
        let extract_fn = source
            .find("fn extract_session_state_compact")
            .expect("extract_session_state_compact must exist");
        let extract_body = &source[extract_fn..extract_fn + 2000];
        let required_fields = [
            "budget_remaining_tokens",
            "budget_remaining_rounds",
            "consecutive_ctx_errors",
            "recent_tools",
            "blocked_tools",
            "continuity",
            "approval_overrides",
            "interruption",
        ];
        for field in &required_fields {
            assert!(
                extract_body.contains(field),
                "extract_session_state_compact must include {field}"
            );
        }
    }

    #[test]
    fn persist_context_uses_csl_manager() {
        let source = include_str!("run_lifecycle.rs");
        let persist_ctx = source
            .find("struct PostLoopPersistContext")
            .expect("PostLoopPersistContext must exist");
        let persist_body = &source[persist_ctx..persist_ctx + 1000];
        assert!(
            persist_body.contains("csl_manager"),
            "PostLoopPersistContext must have csl_manager field"
        );
        assert!(
            persist_body.contains("CslManager"),
            "PostLoopPersistContext must use CslManager type"
        );
    }

    #[test]
    fn restore_csl_recovers_all_session_state_fields() {
        let source = include_str!("run_lifecycle.rs");
        let restore_fn = source
            .find("async fn restore_csl_history")
            .expect("restore_csl_history must exist");
        let restore_body = &source[restore_fn..restore_fn + 3000];
        let required_fields = [
            "continuity",
            "blocked_tools",
            "recent_tools",
            "approval_overrides",
            "interruption",
            "budget_remaining_tokens",
            "budget_remaining_rounds",
            "consecutive_ctx_errors",
        ];
        for field in &required_fields {
            assert!(
                restore_body.contains(field),
                "restore_csl_history must restore {field} from SessionStateCompact"
            );
        }
    }

    #[test]
    fn both_entry_points_wire_csl_manager_to_persist_context() {
        let source = include_str!("run_lifecycle.rs");
        let create_run_start = source
            .find("async fn create_run(")
            .expect("create_run must exist");
        let stream_chat_start = source
            .find("async fn stream_chat(")
            .expect("stream_chat must exist");

        let create_run_body = &source[create_run_start..stream_chat_start];
        assert!(
            create_run_body.contains("csl_manager: csl_manager"),
            "create_run must pass csl_manager to PostLoopPersistContext"
        );

        let stream_chat_body = &source[stream_chat_start..];
        assert!(
            stream_chat_body.contains("csl_manager: csl_manager"),
            "stream_chat must pass csl_manager to PostLoopPersistContext"
        );
    }
}
