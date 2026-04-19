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
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use async_trait::async_trait;
use axum::Json;
use axum::http::StatusCode;
use serde_json::{Map, Value, json};
use tokio::sync::{Mutex as TokioMutex, RwLock, mpsc};

use super::ws_progress_callback::ProgressEvent;
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
use crate::pipeline::step_recorder::StepRecorder;
use crate::runtime_promotion_signals::{RuntimePromotionGateSignal, RuntimePromotionSignals};
use crate::turn::agentic_loop_host::{
    AgenticLoopOutcome, AgenticLoopState, CancellationState, ContextTracePersistenceContext,
    EvaluationPersistenceContext, MessagingState, RequestConstraints, SkillState, StopHookState,
    run_agentic_loop_with_host,
};
use crate::{
    DatabaseEvaluationService, DatabaseEventService, EvaluationService, EventCreateRequestData,
    EventService,
};

use astra_core::{
    STATUS_CANCELLED, STATUS_COMPLETED, STATUS_FAILED, STATUS_PAUSED, STATUS_RUNNING,
    STATUS_WAITING,
};

use super::run_engine::RunEngine;
use super::server_loop_host::ServerAgenticLoopHostBuilder;

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
    edge_connection_pool: Option<&super::edge_connection_pool::EdgeConnectionPool>,
) -> Option<Arc<dyn crate::skills::traits::SkillExecutor>> {
    use super::server_skill_subrun::ServerSkillSubRunExecutor;
    use crate::skills::executor::isolated::{IsolatedSkillExecutor, SkillExecutionRouter};

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
    .with_skill_resolver(skill_resolver);
    if let Some(pool) = edge_connection_pool {
        subrun_executor = subrun_executor.with_edge_connection_pool(pool.clone());
    }

    let isolated = Arc::new(IsolatedSkillExecutor::new(Arc::new(subrun_executor)));
    let router = SkillExecutionRouter::new(Some(isolated));
    Some(Arc::new(router))
}

pub(crate) fn has_turn_verdict_warning(
    verdict_events: &[crate::turn::agentic_verdict_audit::AgenticVerdictAuditEvent],
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
    let eval = crate::pipeline::evaluation::evaluate_tool_call_records(
        &state.message,
        &state.recent_tools,
        &state.stall.tool_call_records,
        state.stall.events.len(),
        verdict_warning,
        state.telemetry.first_budget_pressure,
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

pub(crate) async fn load_runtime_promotion_signals_with_service(
    service: &impl EvaluationService,
    user_id: &str,
) -> Result<RuntimePromotionSignals, (StatusCode, Json<ErrorResponse>)> {
    let quality = service.get_quality_trend(user_id, 30, None).await?;
    let gate_history = service.get_gate_history(user_id, 1).await?;
    let calibration = service.get_calibration(user_id, None, 30).await?;
    let latest_gate = gate_history.gates.first();
    let calibration_error = if calibration.noise_filtered_sample_count > 0 {
        calibration.noise_filtered_calibration_error_interval
    } else {
        calibration.calibration_error_interval
    };

    Ok(RuntimePromotionSignals {
        noise_filtered_quality: Some(quality.noise_filtered_overall_avg_interval),
        latest_gate: latest_gate.map(|gate| RuntimePromotionGateSignal {
            passed: gate.passed,
            score_delta: Some(gate.score_delta_interval),
        }),
        calibration_error: Some(calibration_error),
        recent_turn: None,
    })
}

fn seed_restricted_tools_from_blocked_patterns(
    loop_state: &mut AgenticLoopState,
    pattern_library: &crate::pipeline::pattern::PatternLibrary,
) {
    for name in pattern_library.blocked_tool_names() {
        loop_state.restricted_tools.insert(name);
    }
}

async fn initialize_runtime_controllers(
    loop_state: &mut AgenticLoopState,
    user_id: &str,
    session_id: &str,
    promotion_signals: Option<RuntimePromotionSignals>,
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
    evolution_service.set_runtime_promotion_signals(promotion_signals.clone());

    loop_state.telemetry.observability_hub = Some(hub);
    loop_state.telemetry.observability_session = Some(session);
    loop_state.telemetry.runtime_promotion_signals = promotion_signals;
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
    // Promotion signals require a live database connection. When no shared pool
    // is available (e.g. edge-only mode) skip the preload rather than creating a
    // throwaway connection that may hang on unreachable hosts.
    let evaluation_persistence = shared_pool.map(|pool| EvaluationPersistenceContext {
        user_id: user_id.to_string(),
        evaluation_service: build_runtime_evaluation_service(matrixone, Some(pool)),
    });
    let context_trace_persistence = shared_pool.map(|pool| ContextTracePersistenceContext {
        user_id: user_id.to_string(),
        event_service: build_runtime_event_service(matrixone, Some(pool)),
        agent_id: RUNTIME_CONTEXT_TRACE_AGENT_ID.to_string(),
    });
    let promotion_signals = if let Some(context) = evaluation_persistence.as_ref() {
        match load_runtime_promotion_signals_with_service(&context.evaluation_service, user_id)
            .await
        {
            Ok(context) => Some(context),
            Err((status, response)) => {
                tracing::warn!(
                    target: "astra_runtime::run_lifecycle",
                    user_id = %user_id,
                    status = %status,
                    detail = %response.0.detail,
                    "promotion-signals preload failed"
                );
                None
            }
        }
    } else {
        tracing::debug!("skipping promotion-signals preload: no shared database pool");
        None
    };
    initialize_runtime_controllers(
        loop_state,
        user_id,
        session_id,
        promotion_signals,
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

    let service = build_runtime_event_service(matrixone, shared_pool);
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

// ─── Run State ──────────────────────────────────────────────────────────────

/// Status of a single agentic run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunStatus {
    Running,
    Paused,
    Completed,
    Failed,
    Cancelled,
}

impl RunStatus {
    fn as_str(&self) -> &'static str {
        match self {
            Self::Running => STATUS_RUNNING,
            Self::Paused => STATUS_PAUSED,
            Self::Completed => STATUS_COMPLETED,
            Self::Failed => STATUS_FAILED,
            Self::Cancelled => STATUS_CANCELLED,
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
                Some("run_error" | "run_finished")
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
    edge_connection_pool: Option<super::edge_connection_pool::EdgeConnectionPool>,
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
        pool: super::edge_connection_pool::EdgeConnectionPool,
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

    /// Clone the Arc handle to the runs map (for background tasks).
    fn runs_handle(&self) -> Arc<RwLock<HashMap<String, RunState>>> {
        Arc::clone(&self.runs)
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

    async fn persist_terminal_events_if_configured(&self, run_id: &str, events: &[Value]) {
        let Some(engine) = &self.run_engine else {
            return;
        };
        for event in terminal_events_for_persistence(events) {
            astra_core::log_persist!(
                engine.append_event(run_id, event).await,
                "run_lifecycle",
                run_id,
                "append_terminal_event"
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
            .is_some_and(|f| f.load(Ordering::Relaxed))
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
                        "event_type": "run_error",
                        "data": {"error": msg.clone()}
                    }));
                    events.push(json!({
                        "event_type": "run_finished",
                        "data": usage.clone(),
                    }));
                    (RunStatus::Failed, Some(msg))
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
    ) -> super::server_loop_host::ServerAgenticLoopHost {
        let mut builder = ServerAgenticLoopHostBuilder::new(
            self.matrixone.clone(),
            self.encryptor.clone(),
            user_id.to_string(),
            session_id.to_string(),
        )
        .with_model(request.model.clone())
        .with_llm_token_service(request.llm_token_service.clone())
        .with_edge_tools(edge_tools)
        .with_edge_profile(edge_profile)
        .with_edge_callback_ledger(self.edge_callback_ledger.clone())
        .with_interactive_client(request.interactive_client);

        if let Some(pool) = &self.shared_pool {
            builder = builder.with_pool(pool.clone());
        }
        // Wire progress broadcaster from delegation engine for SSE agent tree events
        if let Some(ref de) = self.delegation_engine {
            if let Some(broadcaster) = de.progress_broadcaster() {
                builder = builder.with_progress_broadcaster(Arc::clone(broadcaster));
            }
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
    ) -> AgenticLoopState {
        use crate::pipeline::step_protocol::InMemoryIdempotencyCache;
        use crate::semantic_dedup::SemanticDedup;
        use crate::turn::chat_turn_heuristics::infer_task_execution_profile;
        use crate::turn::stop_hooks_yaml::{
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
        use crate::turn::turn_guard::TurnGuard;

        let user_message = json!({
            "role": "user",
            "content": request.message,
        });

        let max_turns = request.max_candidates.max(1) as usize;
        let task_profile = infer_task_execution_profile(&request.message);
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
        );

        AgenticLoopState {
            messages: vec![user_message],
            tool_results: Vec::new(),
            current_session_id: Some(session_id.to_string()),
            current_run_id: Some(run_id.to_string()),
            recursion_depth: 0,
            final_text: String::new(),
            total_prompt: 0,
            total_completion: 0,
            total_cache_read: 0,
            total_cache_creation: 0,
            total_tool_calls: 0,
            total_evidence_tool_calls: 0,
            has_any_usage: false,
            max_turns,
            remaining_turns: max_turns,
            turn_guard: TurnGuard::with_profile(task_profile),
            restricted_tools: std::collections::HashSet::new(),
            step_recorder: StepRecorder::new(session_id, run_id),
            idempotency_cache: InMemoryIdempotencyCache::new(),
            semantic_dedup: SemanticDedup::new(0.75),
            call_counts: HashMap::new(),
            max_identical_tool_calls: crate::runtime_config::RuntimeConfig::load()
                .tool_selection
                .effective_max_identical_calls(),
            max_tools_per_turn: crate::runtime_config::RuntimeConfig::load()
                .tool_selection
                .effective_max_tools_per_turn(),
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
                improvement_tracker: crate::skills::improvement::ImprovementTracker::new(),
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
            project_context: None,
            checkpoint_gate: None,
            evolution_service: None,
            rate_limit_cooldown: Default::default(),
            data_snapshot_provider: None,
            last_composite_snapshot: None,
            last_measured_prompt_tokens: None,
            consecutive_context_window_errors: 0,
            compaction_effectiveness: Default::default(),
            max_turn_input_tokens: astra_core::RuntimeLimits::global().max_turn_input_tokens,
            budget_wrapup_injected: false,
            skill_produced_output: false,
            max_cumulative_tokens: 0,
            thinking_budget_tokens: None,
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
            approval_overrides: None,
            confidence_trend: Default::default(),
            last_confidence_diagnosis: None,
            turn_event_buffer: None,
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
            .map(|r| r.pause_flag.load(Ordering::Relaxed))
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

        let (run_state, cancel_flag, pause_flag, llm_cancel_token) =
            Self::build_tracked_run_state(run_id.clone(), session_id.clone(), user_id.clone());
        self.runs.write().await.insert(run_id.clone(), run_state);

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
        let mut host = self.build_host(&user_id, &session_id, &request, edge_tools, edge_profile);
        let mut loop_state =
            self.build_initial_state(&request, &session_id, &run_id, server_workspace.as_deref());
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
            if let Some(observability_session) = loop_state.telemetry.observability_session.clone()
            {
                executor.set_observability_session(observability_session);
            }

            // ── Phase E: Wire WebSocket approval gate ───────────────
            let (approval_tx, approval_rx) = mpsc::unbounded_channel();
            let approval_gate = super::ws_approval_gate::WebSocketApprovalGate::new(
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
                let user_prompt_gate = super::ws_user_prompt_gate::WebSocketUserPromptGate::new(
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
                super::ws_progress_callback::WebSocketProgressCallback::new(progress_tx);
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
        let bg_user_id = user_id.clone();
        let bg_session_id = session_id.clone();
        let bg_matrixone = self.matrixone.clone();
        let bg_shared_pool = self.shared_pool.clone();

        tokio::spawn(async move {
            let outcome = run_agentic_loop_with_host(&mut host, &mut loop_state).await;
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
            let status_str = final_status.as_str();
            let mut persist_terminal_state = true;

            if let Some(run) = runs.write().await.get_mut(&bg_run_id) {
                if run.status == RunStatus::Cancelled {
                    persist_terminal_state = false;
                    merge_cancelled_run_events(run, events);
                    flush_turn_observability(&mut loop_state, &bg_session_id, true);
                } else {
                    run.events.extend(events);
                    run.status = final_status;
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

            // Fire SessionEnd hooks (best-effort, non-blocking).
            crate::skills::hooks::fire_session_end(
                &loop_state.skills.session_event_hooks,
                loop_state.current_session_id.as_deref().unwrap_or(""),
            )
            .await;

            // Session-end governance: extract learnings, store to Memoria, purge working memory.
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

            persist_runtime_promotion_events(
                &bg_matrixone,
                bg_shared_pool.as_ref(),
                &bg_user_id,
                &bg_session_id,
                &bg_run_id,
                &loop_state.telemetry.promotion_events,
            )
            .await;

            // Persist learning state (patterns, calibration, entities) so the
            // next session starts with accumulated cross-session knowledge.
            let active_canary = match loop_state.evolution_service.as_ref() {
                Some(evolution_service) => evolution_service.export_active_canary().await,
                None => None,
            };
            learning_stack.save_with_active_canary(active_canary);
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

    /// Stream chat (synchronous mode): runs the full agentic loop, returns all events.
    async fn stream_chat(
        &self,
        user_id: String,
        request: ChatRequestData,
    ) -> Result<ChatStreamRecord, (StatusCode, Json<ErrorResponse>)> {
        self.validate_request_constraints(&request).await?;

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

        // If no edge tools are provided, return a minimal "no tools" response.
        // The client (CLI thin-client) is expected to provide edge_tools in context.
        let mut host = self.build_host(&user_id, &session_id, &request, edge_tools, edge_profile);
        let mut state =
            self.build_initial_state(&request, &session_id, &run_id, server_workspace.as_deref());
        let (run_state, cancel_flag, pause_flag, llm_cancel_token) =
            Self::build_tracked_run_state(run_id.clone(), session_id.clone(), user_id.clone());
        self.runs.write().await.insert(run_id.clone(), run_state);
        self.persist_run_start_if_configured(&run_id, &user_id, &session_id)
            .await;
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

        let loop_result = run_agentic_loop_with_host(&mut host, &mut state).await;

        // Fire SessionEnd hooks (best-effort).
        crate::skills::hooks::fire_session_end(
            &state.skills.session_event_hooks,
            state.current_session_id.as_deref().unwrap_or(""),
        )
        .await;
        persist_runtime_promotion_events(
            &self.matrixone,
            self.shared_pool.as_ref(),
            &user_id,
            &session_id,
            &run_id,
            &state.telemetry.promotion_events,
        )
        .await;

        // Persist cross-session learning state.
        let active_canary = match state.evolution_service.as_ref() {
            Some(evolution_service) => evolution_service.export_active_canary().await,
            None => None,
        };
        learning_stack.save_with_active_canary(active_canary);

        let (mut final_events, final_status, error_msg) =
            Self::finalize_run_events(loop_result, host.take_emitted_events(), &state);
        flush_turn_observability(&mut state, &session_id, false);
        persist_turn_evaluation_journal(&session_id, "server_runtime", &state);
        let mut all_events = vec![json!({"event_type": "run_started", "data": {}})];
        all_events.append(&mut final_events);

        if let Some(run) = self.runs.write().await.get_mut(&run_id) {
            run.events = all_events.clone();
            run.status = final_status.clone();
        }

        if let Some(engine) = &self.run_engine {
            astra_core::log_persist!(
                engine
                    .persist_status(&run_id, final_status.as_str(), None, error_msg.as_deref())
                    .await,
                "run_lifecycle",
                &run_id,
                "status"
            );
            astra_core::log_persist!(
                engine
                    .persist_usage(
                        &run_id,
                        state.total_prompt,
                        state.total_completion,
                        state.total_tool_calls,
                    )
                    .await,
                "run_lifecycle",
                &run_id,
                "usage"
            );
        }
        self.persist_terminal_events_if_configured(&run_id, &all_events[1..])
            .await;

        Ok(ChatStreamRecord {
            session_id,
            run_id,
            events: all_events,
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
                if matches!(run.status, RunStatus::Running | RunStatus::Paused) {
                    run.cancel_flag.store(true, Ordering::SeqCst);
                    run.pause_flag.store(false, Ordering::SeqCst);
                    run.llm_cancel_token.cancel();
                    run.status = RunStatus::Cancelled;
                    run.waiting_for = None;
                    run.events.push(json!({
                        "event_type": "run_finished",
                        "data": {"cancelled": true}
                    }));
                    // Persist cancellation
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
                return Ok(CancelRunRecord {
                    run_id,
                    status: run.status.as_str().to_string(),
                });
            }
        }

        if let Some(run) = self.load_durable_run_for_user(&run_id, &user_id).await? {
            return if matches!(run.status.as_str(), STATUS_RUNNING | STATUS_PAUSED) {
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
        let all: Vec<RunStatusRecord> = runs
            .values()
            .filter(|run| run.user_id == user_id)
            .map(Self::status_record)
            .collect();
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
                if run.status != RunStatus::Running {
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
                if run.status != RunStatus::Paused {
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
    edge_connection_pool: Option<super::edge_connection_pool::EdgeConnectionPool>,
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
        pool: super::edge_connection_pool::EdgeConnectionPool,
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
        use crate::pipeline::step_protocol::InMemoryIdempotencyCache;
        use crate::semantic_dedup::SemanticDedup;
        use crate::turn::chat_turn_heuristics::infer_task_execution_profile;
        use crate::turn::stop_hooks_yaml::{
            detect_turn_hook_sets, is_plan_subtask_from_delegation_context,
            project_root_from_delegation_context,
        };
        use crate::turn::turn_guard::TurnGuard;

        // Build edge profile from agent's system prompt and metadata.
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

        let mut loop_state = AgenticLoopState {
            messages: vec![user_message],
            tool_results: Vec::new(),
            current_session_id: Some(config.session_id.clone()),
            current_run_id: Some(config.run_id.clone()),
            recursion_depth: 0,
            final_text: String::new(),
            total_prompt: 0,
            total_completion: 0,
            total_cache_read: 0,
            total_cache_creation: 0,
            total_tool_calls: 0,
            total_evidence_tool_calls: 0,
            has_any_usage: false,
            max_turns: 10,
            remaining_turns: 10,
            turn_guard: TurnGuard::new(),
            restricted_tools: std::collections::HashSet::new(),
            step_recorder: StepRecorder::new(&config.session_id, &config.run_id),
            idempotency_cache: InMemoryIdempotencyCache::new(),
            semantic_dedup: SemanticDedup::new(0.75),
            call_counts: HashMap::new(),
            max_identical_tool_calls: crate::runtime_config::RuntimeConfig::load()
                .tool_selection
                .effective_max_identical_calls(),
            max_tools_per_turn: crate::runtime_config::RuntimeConfig::load()
                .tool_selection
                .effective_max_tools_per_turn(),
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
                improvement_tracker: crate::skills::improvement::ImprovementTracker::new(),
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
            project_context: None,
            checkpoint_gate: config.checkpoint_gate.clone(),
            evolution_service: None,
            rate_limit_cooldown: Default::default(),
            data_snapshot_provider: None,
            last_composite_snapshot: None,
            last_measured_prompt_tokens: None,
            consecutive_context_window_errors: 0,
            compaction_effectiveness: Default::default(),
            max_turn_input_tokens: astra_core::RuntimeLimits::global().max_turn_input_tokens,
            budget_wrapup_injected: false,
            skill_produced_output: false,
            max_cumulative_tokens: 0,
            thinking_budget_tokens: None,
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
            approval_overrides: None,
            confidence_trend: Default::default(),
            last_confidence_diagnosis: None,
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
            if let Some(pool) = &self.edge_connection_pool {
                executor.set_edge_connection_pool(pool.clone());
            }
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

        // Persist cross-session learning state.
        let active_canary = match loop_state.evolution_service.as_ref() {
            Some(evolution_service) => evolution_service.export_active_canary().await,
            None => None,
        };
        learning_stack.save_with_active_canary(active_canary);

        match outcome {
            Ok(AgenticLoopOutcome::Completed) => Ok(astra_services::coordination::AgentResult {
                agent_id: config.agent_profile.agent_id,
                run_id: config.run_id,
                status: STATUS_COMPLETED.to_string(),
                output: if loop_state.final_text.is_empty() {
                    None
                } else {
                    Some(loop_state.final_text)
                },
                error: None,
                prompt_tokens: loop_state.total_prompt,
                completion_tokens: loop_state.total_completion,
                tool_calls: loop_state.total_tool_calls,
            }),
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
    use astra_services::session_journal::{JournalEventType, ToolCallRecord};

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

    fn test_request(message: &str) -> ChatRequestData {
        ChatRequestData {
            message: message.to_string(),
            session_id: None,
            agent_id: None,
            model: None,
            llm_token_service: None,
            skill_search: None,
            allow_skills: None,
            allow_tools: None,
            context: None,
            forward_headers: HashMap::new(),
            max_candidates: 5,
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
        let state = svc.build_initial_state(&test_request("hello"), "session-1", "run-1", None);
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
        let filtered_state = svc.build_initial_state(&filtered_request, "session-1", "run-1", None);
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
        let mut state = svc.build_initial_state(&request, "session-1", "run-1", None);
        state.recent_tools = vec!["git_status".into()];
        state.telemetry.first_budget_pressure = 0.27;
        state.stall.events.push(("repetition_stall".into(), 1));
        state.stall.verdict_events.push(
            crate::turn::agentic_verdict_audit::AgenticVerdictAuditEvent {
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
        let mut state = svc.build_initial_state(&request, "session-1", "run-1", None);
        let mut pattern_library = crate::pipeline::pattern::PatternLibrary::new();

        for _ in 0..3 {
            pattern_library.record_outcome(
                &["bash".to_string()],
                crate::pipeline::routing::TaskType::Code,
                None,
                true,
                0.8,
                None,
            );
        }
        pattern_library
            .apply_evolution_action("bash", crate::evolution::types::PatternAction::Block);

        seed_restricted_tools_from_blocked_patterns(&mut state, &pattern_library);

        assert!(state.restricted_tools.contains("bash"));
    }

    #[test]
    fn finalize_run_events_appends_run_finished_for_failures() {
        let svc = test_service();
        let request = test_request("boom");
        let state = svc.build_initial_state(&request, "session-1", "run-1", None);

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
        let mut state = svc.build_initial_state(&request, "session-1", "run-1", None);
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
            json!({"event_type": "run_error", "data": {"error": "boom"}}),
            json!({"event_type": "run_finished", "data": {"prompt_tokens": 1}}),
        ];

        let persisted = terminal_events_for_persistence(&events);
        assert_eq!(persisted.len(), 2);
        assert_eq!(persisted[0]["event_type"], "run_error");
        assert_eq!(persisted[1]["event_type"], "run_finished");
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

        let status = ok(svc
            .get_run_status(stream.run_id.clone(), "user-1".into())
            .await);
        let replay = ok(svc
            .stream_run(stream.run_id.clone(), "user-1".into(), 0)
            .await);

        assert_ne!(status.status, "running");
        assert_eq!(status.run_id, stream.run_id);
        assert_eq!(status.events_count as usize, stream.events.len());
        assert_eq!(replay.len(), stream.events.len());
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
            agent_id: None,
            model: None,
            llm_token_service: None,
            skill_search: None,
            allow_skills: None,
            allow_tools: None,
            context: Some(ctx),
            forward_headers: HashMap::new(),
            max_candidates: 5,
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
            agent_id: None,
            model: None,
            llm_token_service: None,
            skill_search: None,
            allow_skills: None,
            allow_tools: None,
            context: Some(ctx),
            forward_headers: HashMap::new(),
            max_candidates: 5,
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
        let state = svc.build_initial_state(&req, "sess-1", "run-1", None);
        assert_eq!(state.messages.len(), 1);
        assert_eq!(state.messages[0]["role"], "user");
        assert_eq!(state.messages[0]["content"], "write a test");
        assert_eq!(state.current_session_id, Some("sess-1".to_string()));
        assert_eq!(state.current_run_id, Some("run-1".to_string()));
        assert_eq!(state.max_turns, 5);
        assert_eq!(state.remaining_turns, 5);
        assert_eq!(state.message, "write a test");
        assert!(state.cancellation.token.is_none());
    }

    #[test]
    fn build_initial_state_clamps_max_turns() {
        let svc = test_service();
        let mut req = test_request("go");
        req.max_candidates = 0;
        let state = svc.build_initial_state(&req, "s", "r", None);
        assert_eq!(state.max_turns, 1);
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

        let state = svc.build_initial_state(&req, "s", "r", None);
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
        let state = svc.build_initial_state(&req, "s", "r", Some(dir.path()));
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

        let state = svc.build_initial_state(&req, "s", "r", Some(override_dir.path()));
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
        let durable = engine.load_run(&stream.run_id).await.unwrap().unwrap();
        assert_eq!(durable.user_id, "user-1");
        assert_eq!(durable.session_id, stream.session_id);
        assert_ne!(durable.status, "running");
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
}
