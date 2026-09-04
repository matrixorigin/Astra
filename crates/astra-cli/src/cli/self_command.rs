use chrono::Utc;
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};

use crate::cli::cli_config::cli_args::{
    SelfCmd, SelfJournalArgs, SelfMutateCmd, SelfMutateConfigArgs, SelfReflectArgs,
};
use crate::cli::cli_config::cli_utils::local_resumable_last_session_id;
use crate::cli::session::session_continuation::extract_text_content;
use crate::edge_tools;
use astra_config::runtime_config::RuntimeConfig;
use astra_core::observation::SourcePolicy;
use astra_core::{
    ObservationBudgetResult, ObservationConfidence, ObservationDataCoverage, ObservationEvidence,
    ObservationGraphEdge, ObservationGraphEdgeKind, ObservationGraphLayer, ObservationGraphNode,
    ObservationGraphNodeKind, ObservationGraphSlice, ObservationProviderCoverage,
    ObservationRecord, ObservationView, Urn,
};
use astra_runtime::self_model::ConstraintSet;
use astra_services::reflect::{AgentDeliveryRollup, ReflectReport, ReflectRequest};
use astra_services::self_surface::LoadedSelfSurfaceArtifacts;
use astra_services::session_journal::{self, JournalEvent, JournalEventType};
use astra_services::session_workspace::{self, WorkspaceMetadata};
use astra_turn_core::orchestration::agent_result_wire::AgentToolResultStatusKind;
use astra_turn_core::tool::schema::tool_schema_name;

use super::surface::self_surface;

type SessionArtifacts = LoadedSelfSurfaceArtifacts;

#[derive(Debug, Serialize)]
pub(crate) struct IdentityView {
    name: &'static str,
    version: &'static str,
    runtime: &'static str,
}

#[derive(Debug, Clone, Serialize)]
struct EventPreview {
    event_type: String,
    ts: String,
    turn: Option<u32>,
    error: Option<String>,
    tools_used: Option<Vec<String>>,
    metadata: Option<serde_json::Value>,
    user_input_preview: Option<String>,
    assistant_output_preview: Option<String>,
}

#[derive(Debug, Serialize)]
pub(crate) struct CheckResult {
    pub(crate) name: String,
    pub(crate) ok: bool,
    pub(crate) detail: String,
}

#[derive(Debug, Serialize)]
pub(crate) struct MutatePreviewResponse {
    pub(crate) session_id: String,
    pub(crate) path: String,
    pub(crate) old_value: serde_json::Value,
    pub(crate) new_value: serde_json::Value,
    valid: bool,
    effective_config_changed: bool,
    would_clear_override: bool,
    checks: Vec<CheckResult>,
}

#[derive(Debug, Serialize)]
pub(crate) struct MutateApplyResponse {
    #[serde(flatten)]
    pub(crate) preview: MutatePreviewResponse,
    pub(crate) config_revision: u64,
    #[serde(skip)]
    pub(crate) committed_config: RuntimeConfig,
    pub(crate) audit_recorded: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) audit_warning: Option<String>,
}

pub(crate) enum GovernedConfigMutationError {
    Rejected(serde_json::Value),
    Persistence(String),
    OutcomeUnknown(Box<GovernedConfigMutationUnknown>),
}

pub(crate) struct GovernedConfigMutationUnknown {
    pub(crate) preview: MutatePreviewResponse,
    pub(crate) drift: Option<f64>,
    pub(crate) proposed_revision: u64,
    pub(crate) observed_revision: Option<u64>,
    pub(crate) observed_config: Option<RuntimeConfig>,
    pub(crate) retry_revision: Option<u64>,
    pub(crate) reason: String,
}

pub(crate) async fn execute_self_command(
    cmd: &SelfCmd,
    profile: Option<&str>,
) -> Result<String, String> {
    match cmd {
        SelfCmd::Snapshot(args) => {
            render_surface_for_session_with_profile(
                &resolve_target_session_id(args.session_id.as_deref(), profile).await?,
                "snapshot",
                20,
                profile,
            )
            .await
        }
        SelfCmd::Reflect(SelfReflectArgs {
            session_id,
            topic,
            facet,
            question,
            last_n,
        }) => {
            let request = ReflectRequest::from_observation_params(
                Some(topic.as_str()),
                facet.as_deref(),
                None,
                None,
                i32::try_from(*last_n).unwrap_or(i32::MAX),
                question.as_deref().unwrap_or(""),
            );
            let journal_limit = usize::try_from(request.last_n).unwrap_or(20);
            render_reflect_surface_for_session_with_profile(
                &resolve_target_session_id(session_id.as_deref(), profile).await?,
                journal_limit,
                request,
                profile,
            )
            .await
        }
        SelfCmd::Profile(args) => {
            render_surface_for_session_with_profile(
                &resolve_target_session_id(args.session_id.as_deref(), profile).await?,
                "profile",
                20,
                profile,
            )
            .await
        }
        SelfCmd::Goal(args) => {
            render_surface_for_session_with_profile(
                &resolve_target_session_id(args.session_id.as_deref(), profile).await?,
                "goal",
                20,
                profile,
            )
            .await
        }
        SelfCmd::Trace(args) => {
            render_surface_for_session_with_profile(
                &resolve_target_session_id(args.session_id.as_deref(), profile).await?,
                "trace",
                20,
                profile,
            )
            .await
        }
        SelfCmd::Budget(args) => {
            render_surface_for_session_with_profile(
                &resolve_target_session_id(args.session_id.as_deref(), profile).await?,
                "budget",
                20,
                profile,
            )
            .await
        }
        SelfCmd::Signals(args) => {
            render_surface_for_session_with_profile(
                &resolve_target_session_id(args.session_id.as_deref(), profile).await?,
                "signals",
                20,
                profile,
            )
            .await
        }
        SelfCmd::Health(args) => {
            render_surface_for_session_with_profile(
                &resolve_target_session_id(args.session_id.as_deref(), profile).await?,
                "health",
                20,
                profile,
            )
            .await
        }
        SelfCmd::Journal(SelfJournalArgs { session_id, limit }) => {
            render_surface_for_session_with_profile(
                &resolve_target_session_id(session_id.as_deref(), profile).await?,
                "journal",
                *limit,
                profile,
            )
            .await
        }
        SelfCmd::Verify(args) => {
            render_surface_for_session_with_profile(
                &resolve_target_session_id(args.session_id.as_deref(), profile).await?,
                "verify",
                20,
                profile,
            )
            .await
        }
        SelfCmd::Mutate(SelfMutateCmd::Preview(args)) => {
            let session_id = resolve_target_session_id(args.session_id.as_deref(), profile).await?;
            to_json(&preview_config_mutation(&session_id, args)?)
        }
        SelfCmd::Mutate(SelfMutateCmd::Apply(args)) => {
            let session_id = resolve_target_session_id(args.session_id.as_deref(), profile).await?;
            to_json(&persist_config_mutation(&session_id, args)?)
        }
    }
}

pub(crate) async fn render_surface_for_session(
    session_id: &str,
    surface: &str,
    journal_limit: usize,
) -> Result<String, String> {
    render_surface_for_session_with_profile(session_id, surface, journal_limit, None).await
}

pub(crate) async fn render_surface_for_session_with_profile(
    session_id: &str,
    surface: &str,
    journal_limit: usize,
    profile: Option<&str>,
) -> Result<String, String> {
    self_surface::render_surface_for_session_with_profile(
        session_id,
        surface,
        journal_limit,
        profile,
    )
    .await
}

pub(crate) async fn render_reflect_surface_for_session(
    session_id: &str,
    journal_limit: usize,
    topic: Option<&str>,
    facet: Option<&str>,
    question: Option<&str>,
) -> Result<String, String> {
    let request = ReflectRequest::from_observation_params(
        topic,
        facet,
        None,
        None,
        i32::try_from(journal_limit).unwrap_or(i32::MAX),
        question.unwrap_or(""),
    );
    let bounded_limit = usize::try_from(request.last_n).unwrap_or(20);
    render_reflect_surface_for_session_with_profile(session_id, bounded_limit, request, None).await
}

pub(crate) async fn try_render_reflect_surface_for_session_with_profile(
    session_id: &str,
    request: ReflectRequest,
    profile: Option<&str>,
) -> Result<Option<String>, String> {
    let bounded_limit = usize::try_from(request.last_n).unwrap_or(20);
    match render_reflect_surface_for_session_with_profile(
        session_id,
        bounded_limit,
        request,
        profile,
    )
    .await
    {
        Ok(body) => Ok(Some(body)),
        Err(error) if reflect_surface_missing_state(&error) => Ok(None),
        Err(error) => Err(error),
    }
}

fn reflect_surface_missing_state(error: &str) -> bool {
    error.starts_with("no persistent local or cloud state found for session ")
        || error.starts_with("no persistent local state found for session ")
}

pub(crate) async fn render_reflect_surface_for_session_with_profile(
    session_id: &str,
    journal_limit: usize,
    request: ReflectRequest,
    profile: Option<&str>,
) -> Result<String, String> {
    let artifacts = self_surface::load_artifacts(session_id, profile).await?;
    let bounded_limit = usize::try_from(request.last_n).unwrap_or(journal_limit.max(1));
    to_json(&build_reflect_response(&artifacts, bounded_limit, request).await)
}

pub(crate) fn agent_info_surface_alias(dimension: &str) -> Option<&'static str> {
    match dimension {
        "snapshot" => Some("snapshot"),
        "reflect" => Some("reflect"),
        "profile" => Some("profile"),
        "goal" => Some("goal"),
        "trace" => Some("trace"),
        "budget" => Some("budget"),
        "signals" => Some("signals"),
        "health" => Some("health"),
        "journal" => Some("journal"),
        "verify" => Some("verify"),
        "identity" => Some("identity"),
        "capability" => Some("profile"),
        "state" | "all" => Some("snapshot"),
        "goals" => Some("goal"),
        "context_snapshot" | "context_trend" => Some("trace"),
        _ => None,
    }
}

pub(crate) fn to_json<T: Serialize>(value: &T) -> Result<String, String> {
    serde_json::to_string(value).map_err(|e| e.to_string())
}

pub(crate) fn identity_view() -> IdentityView {
    IdentityView {
        name: "astra",
        version: env!("CARGO_PKG_VERSION"),
        runtime: "Rust edge CLI",
    }
}

fn resolve_session_id(query: Option<&str>, profile: Option<&str>) -> Result<String, String> {
    match query.map(str::trim).filter(|s| !s.is_empty()) {
        Some(q) => session_journal::resolve_session_id(q)
            .or_else(|_| {
                session_journal::validate_session_id(q).map_err(|_| {
                    std::io::Error::new(
                        std::io::ErrorKind::NotFound,
                        format!("no session journal or workspace matches '{q}'"),
                    )
                })?;
                let ws_path = session_workspace::workspace_file_path(q)?;
                if ws_path.exists()
                    || (crate::cli::session::session_runtime::resolve_cloud_base().is_some()
                        && crate::cli::session::session_runtime::current_access_token(profile)
                            .is_some())
                {
                    Ok(q.to_string())
                } else {
                    Err(std::io::Error::new(
                        std::io::ErrorKind::NotFound,
                        format!("no session journal or workspace matches '{q}'"),
                    ))
                }
            })
            .map_err(|e| e.to_string()),
        None => Err("no resumable session found; pass a session id explicitly".to_string()),
    }
}

async fn resolve_target_session_id(
    query: Option<&str>,
    profile: Option<&str>,
) -> Result<String, String> {
    if query.map(str::trim).is_some_and(|s| !s.is_empty()) {
        return resolve_session_id(query, profile);
    }
    resolve_default_session_id(profile).await
}

async fn resolve_default_session_id(profile: Option<&str>) -> Result<String, String> {
    if let Some(api) = crate::cli::session::session_restore_client::cloud_resume_client()?
        && crate::cli::session::session_runtime::current_access_token(profile).is_some()
    {
        if let Some(session_id) =
            crate::cli::cli_config::cli_utils::validated_resumable_last_session_id(&api, profile)
                .await
        {
            return Ok(session_id);
        }

        let sessions = crate::cli::session::session_restore_client::list_cloud_resumable_sessions(
            profile, &api,
        )
        .await?;
        if let Some(session) = sessions.into_iter().find(|session| session.turn_count > 0) {
            crate::cli::cli_config::cli_utils::persist_profile_last_session_or_warn(
                profile,
                &session.session_id,
                "self_command:resolve_default_session_id",
            );
            return Ok(session.session_id);
        }
    }

    local_resumable_last_session_id(profile)
        .ok_or_else(|| "no resumable session found; pass a session id explicitly".to_string())
}

async fn build_reflect_response(
    artifacts: &SessionArtifacts,
    journal_limit: usize,
    request: ReflectRequest,
) -> ReflectReport {
    let analysis_view = request.analysis_view.clone();
    let persistence_warning = artifacts
        .workspace
        .as_ref()
        .and_then(|workspace| workspace.last_persistence_error.as_deref())
        .map(str::trim)
        .filter(|error| !error.is_empty())
        .map(|error| format!("session persistence degraded: {error}"));
    let recent_events = if artifacts.journal_events.is_empty() {
        restored_recent_turn_previews(artifacts, journal_limit)
    } else {
        analysis_view_recent_event_previews(
            &artifacts.journal_events,
            journal_limit,
            &analysis_view,
        )
    };
    let mut warnings = local_reflect_warnings(&request);
    if let Some(warning) = persistence_warning {
        warnings.push(warning);
    }
    let total_events = if !artifacts.journal_events.is_empty() {
        artifacts.journal_events.len() as i64
    } else {
        recent_events.len() as i64
    };
    let adverse_count = recent_events
        .iter()
        .filter(|event| event_preview_has_adverse_signal(event))
        .count() as i64;
    let data_coverage =
        local_reflect_data_coverage(&request, total_events, warnings.clone(), &recent_events);
    let mut summary = if total_events == 0 {
        "No local session observations are available yet.".to_string()
    } else if adverse_count > 0 {
        format!(
            "Local session artifacts contain {} observed event{}; {} recent adverse or degraded signal{} appear in {} relevant event{} reviewed.",
            total_events,
            if total_events == 1 { "" } else { "s" },
            adverse_count,
            if adverse_count == 1 { "" } else { "s" },
            recent_events.len(),
            if recent_events.len() == 1 { "" } else { "s" },
        )
    } else if data_coverage.overall == "partial" {
        format!(
            "Local session artifacts are available with partial provider coverage: {} event{} observed.",
            total_events,
            if total_events == 1 { "" } else { "s" }
        )
    } else {
        format!(
            "Local session artifacts contain {} observed event{}; {} recent relevant event{} reviewed contain no explicit adverse signal.",
            total_events,
            if total_events == 1 { "" } else { "s" },
            recent_events.len(),
            if recent_events.len() == 1 { "" } else { "s" },
        )
    };
    if let Some(agent_delivery) = session_agent_delivery_summary(&artifacts.journal_events) {
        summary.push(' ');
        summary.push_str(&agent_delivery);
    }
    let (observations, evidence, graph_slice) =
        local_reflect_observation_graph(&artifacts.session_id, &request, &summary, &recent_events);
    let view = ObservationView {
        topic: request.topic.as_str().to_string(),
        facet: request.facet.as_str().to_string(),
        depth: request.depth.as_str().to_string(),
        horizon: request.horizon.as_str().to_string(),
        data_coverage: data_coverage.clone(),
    };

    ReflectReport {
        schema_version: 1,
        tool: "reflect".to_string(),
        session_id: artifacts.session_id.clone(),
        analysis_view,
        topic: request.topic.as_str().to_string(),
        facet: request.facet.as_str().to_string(),
        depth: request.depth.as_str().to_string(),
        horizon: request.horizon.as_str().to_string(),
        source_policy: request.source_policy.as_str().to_string(),
        include_context: request.include_context,
        data_coverage,
        view: Some(view),
        summary,
        observations,
        evidence,
        action_hints: Vec::new(),
        failure_clusters: Vec::new(),
        graph_slice,
        budget_result: ObservationBudgetResult::default(),
    }
}

fn session_agent_delivery_summary(events: &[JournalEvent]) -> Option<String> {
    // Journal append/replay is at-least-once evidence. Aggregate by durable
    // child run identity so duplicate lifecycle records cannot manufacture a
    // higher spawn count or multiple deliverables.
    let mut spawned_runs = BTreeSet::new();
    let mut terminal_by_run = BTreeMap::new();
    let event_run_identity = |event: &JournalEvent| {
        event
            .metadata
            .as_ref()
            .and_then(|metadata| metadata.get("run_id").or_else(|| metadata.get("agent_id")))
            .and_then(serde_json::Value::as_str)
            .filter(|identity| !identity.is_empty())
            .map(str::to_string)
    };

    for event in events {
        match &event.event_type {
            JournalEventType::AgentSpawned => {
                if let Some(identity) = event_run_identity(event) {
                    spawned_runs.insert(identity);
                }
            }
            JournalEventType::AgentTerminated => {
                if let Some(identity) = event_run_identity(event) {
                    let status = event
                        .metadata
                        .as_ref()
                        .and_then(|metadata| metadata.get("status"))
                        .and_then(serde_json::Value::as_str)
                        .map(AgentToolResultStatusKind::parse_wire);
                    terminal_by_run.insert(identity, status);
                }
            }
            _ => {}
        }
    }

    let mut rollup = AgentDeliveryRollup::default();
    rollup.spawned = spawned_runs.len();
    for status in terminal_by_run
        .into_iter()
        .filter_map(|(identity, status)| spawned_runs.contains(&identity).then_some(status))
    {
        match status {
            Some(AgentToolResultStatusKind::Completed) => rollup.completed += 1,
            Some(AgentToolResultStatusKind::Interrupted) => rollup.interrupted += 1,
            Some(AgentToolResultStatusKind::Failed) => rollup.failed += 1,
            Some(AgentToolResultStatusKind::Cancelled) => rollup.cancelled += 1,
            Some(_) | None => rollup.other_terminal += 1,
        }
    }
    rollup.render_session_summary()
}

fn local_reflect_warnings(request: &ReflectRequest) -> Vec<String> {
    let mut warnings = Vec::new();
    if matches!(request.horizon.as_str(), "cross_session") {
        warnings.push(
            "cross_session horizon is not available from a single local session surface"
                .to_string(),
        );
    }
    if matches!(request.source_policy, SourcePolicy::CloudOnly) {
        warnings
            .push("cloud_only source policy is not available from local CLI artifacts".to_string());
    }
    if matches!(request.source_policy, SourcePolicy::LiveOnly) {
        warnings.push(
            "live_only source policy is bounded to persisted local turn artifacts in CLI mode"
                .to_string(),
        );
    }
    if request.include_context {
        warnings.push(
            "include_context requested, but local reflect only exposes persisted context summaries"
                .to_string(),
        );
    }
    warnings
}

fn local_reflect_data_coverage(
    request: &ReflectRequest,
    total_events: i64,
    warnings: Vec<String>,
    recent_events: &[EventPreview],
) -> ObservationDataCoverage {
    let mut providers = BTreeMap::new();
    let has_local_journal = recent_events
        .iter()
        .any(|event| event_preview_evidence_source(event) == "local_journal");
    let has_cloud_resume = recent_events
        .iter()
        .any(|event| event_preview_evidence_source(event) == "cloud_resume");
    if has_local_journal || recent_events.is_empty() {
        providers.insert(
            "local_journal".to_string(),
            ObservationProviderCoverage {
                status: "fresh".to_string(),
                freshness_ms: None,
                reason: None,
            },
        );
    }
    if has_cloud_resume {
        providers.insert(
            "cloud_resume".to_string(),
            ObservationProviderCoverage {
                status: "fresh".to_string(),
                freshness_ms: None,
                reason: None,
            },
        );
    }
    if matches!(request.source_policy, SourcePolicy::CloudOnly) {
        providers.insert(
            "cloud_events".to_string(),
            ObservationProviderCoverage {
                status: "unavailable".to_string(),
                freshness_ms: None,
                reason: Some("local CLI reflect cannot read cloud-only data directly".to_string()),
            },
        );
    }
    if request.include_context {
        providers.insert(
            "visible_context".to_string(),
            ObservationProviderCoverage {
                status: "partial".to_string(),
                freshness_ms: None,
                reason: Some("local reflect exposes persisted context summaries only".to_string()),
            },
        );
    }

    ObservationDataCoverage {
        overall: if warnings.is_empty() {
            "fresh".to_string()
        } else {
            "partial".to_string()
        },
        source: "local_session_artifacts".to_string(),
        events: total_events,
        decisions: 0,
        providers,
        warnings,
    }
}

fn local_reflect_observation_graph(
    session_id: &str,
    request: &ReflectRequest,
    summary: &str,
    recent_events: &[EventPreview],
) -> (
    Vec<ObservationRecord>,
    Vec<ObservationEvidence>,
    ObservationGraphSlice,
) {
    let observation_ref = Urn::new("observation", "local", "reflect")
        .seg(session_id)
        .seg(request.topic.as_str())
        .seg(request.facet.as_str())
        .build();
    let mut evidence = Vec::new();
    let mut nodes = vec![ObservationGraphNode {
        ref_id: observation_ref.clone(),
        layer: ObservationGraphLayer::Observation,
        kind: ObservationGraphNodeKind::Observation,
        label: "local_reflect_summary".to_string(),
        summary: Some(summary.to_string()),
        metadata: Some(serde_json::json!({
            "topic": request.topic.as_str(),
            "facet": request.facet.as_str(),
            "depth": request.depth.as_str(),
            "horizon": request.horizon.as_str(),
            "source_policy": request.source_policy.as_str(),
        })),
    }];
    let mut edges = Vec::new();
    let mut evidence_refs = Vec::new();

    for (idx, event) in recent_events.iter().enumerate() {
        let event_source = event_preview_evidence_source(event);
        let event_namespace = event_preview_ref_namespace(event);
        let event_ref = Urn::new("event", event_namespace, session_id)
            .idx(idx + 1)
            .build();
        let event_summary = event_preview_summary(event);
        evidence_refs.push(event_ref.clone());
        evidence.push(ObservationEvidence {
            ref_id: event_ref.clone(),
            evidence_class: "observed_evidence".to_string(),
            source: event_source.to_string(),
            summary: event_summary.clone(),
            confidence: ObservationConfidence::evidence(0.80),
        });
        nodes.push(ObservationGraphNode {
            ref_id: event_ref.clone(),
            layer: ObservationGraphLayer::Runtime,
            kind: if event_preview_has_adverse_signal(event) {
                ObservationGraphNodeKind::Outcome
            } else {
                ObservationGraphNodeKind::Event
            },
            label: event.event_type.clone(),
            summary: Some(event_summary),
            metadata: Some(serde_json::json!({
                "turn": event.turn,
                "ts": event.ts,
                "tools_used": event.tools_used,
                "source": event_source,
                "event_metadata": event.metadata,
            })),
        });
        edges.push(ObservationGraphEdge {
            from: observation_ref.clone(),
            to: event_ref,
            kind: ObservationGraphEdgeKind::DerivedFrom,
        });
    }

    let observation = ObservationRecord {
        ref_id: observation_ref,
        topic: request.topic.as_str().to_string(),
        facet: request.facet.as_str().to_string(),
        kind: "local_session_summary".to_string(),
        severity: if recent_events.iter().any(event_preview_has_adverse_signal) {
            "warning".to_string()
        } else {
            "info".to_string()
        },
        summary: summary.to_string(),
        confidence: ObservationConfidence::classification_evidence(0.75, 0.80),
        evidence_refs,
    };

    (
        vec![observation],
        evidence,
        ObservationGraphSlice {
            nodes,
            edges,
            budget_result: ObservationBudgetResult::default(),
        },
    )
}

fn preview_config_mutation(
    session_id: &str,
    args: &SelfMutateConfigArgs,
) -> Result<MutatePreviewResponse, String> {
    preview_config_mutation_value(session_id, &args.path, parse_value_arg(&args.value))
}

fn preview_config_mutation_value(
    session_id: &str,
    path: &str,
    new_value: serde_json::Value,
) -> Result<MutatePreviewResponse, String> {
    let workspace = session_workspace::read_workspace(session_id)
        .map_err(|e| format!("workspace metadata missing for session {session_id}: {e}"))?;
    let base_config = effective_runtime_config(Some(&workspace))?;
    prepare_config_mutation(session_id, path, new_value, &base_config).map(|(preview, _)| preview)
}

fn prepare_config_mutation(
    session_id: &str,
    path: &str,
    new_value: serde_json::Value,
    base_config: &RuntimeConfig,
) -> Result<(MutatePreviewResponse, RuntimeConfig), String> {
    let base_json = serde_json::to_value(base_config).map_err(|e| e.to_string())?;
    let mut value = base_json.clone();
    let old_value = astra_config::replace_existing_json_path(&mut value, path, new_value.clone())
        .map_err(|error| error.to_string())?;
    let candidate_config: RuntimeConfig = serde_json::from_value(value.clone())
        .map_err(|e| format!("mutation produced invalid RuntimeConfig at '{}': {e}", path))?;
    let candidate_json = serde_json::to_string(&candidate_config).map_err(|e| e.to_string())?;
    let candidate_checks = verify_runtime_config(Some(&candidate_json));
    let baseline_json = serde_json::to_value(RuntimeConfig::load()).map_err(|e| e.to_string())?;
    Ok((
        MutatePreviewResponse {
            session_id: session_id.to_string(),
            path: path.to_string(),
            old_value,
            new_value,
            valid: candidate_checks.iter().all(|check| check.ok),
            effective_config_changed: value != base_json,
            would_clear_override: value == baseline_json,
            checks: candidate_checks,
        },
        candidate_config,
    ))
}

pub(crate) fn prepare_governed_config_mutation(
    session_id: &str,
    path: &str,
    new_value: serde_json::Value,
    base_config: &RuntimeConfig,
    force: bool,
    drift_ceiling: f64,
) -> Result<(MutatePreviewResponse, RuntimeConfig, Option<f64>), serde_json::Value> {
    let mut candidate_config = base_config.clone();
    let mutation = astra_config::apply_governed_config_mutation(
        &mut candidate_config,
        path,
        &new_value,
        force,
        drift_ceiling,
    )
    .map_err(|error| error.to_json())?;
    let candidate_json = serde_json::to_string(&candidate_config).map_err(
        |error| serde_json::json!({"error": "invalid_config_mutation", "detail": error.to_string()}),
    )?;
    let candidate_checks = verify_runtime_config(Some(&candidate_json));
    let candidate_value = serde_json::to_value(&candidate_config).map_err(
        |error| serde_json::json!({"error": "invalid_config_mutation", "detail": error.to_string()}),
    )?;
    let base_value = serde_json::to_value(base_config).map_err(
        |error| serde_json::json!({"error": "invalid_config_mutation", "detail": error.to_string()}),
    )?;
    let baseline = serde_json::to_value(RuntimeConfig::load()).map_err(
        |error| serde_json::json!({"error": "invalid_config_mutation", "detail": error.to_string()}),
    )?;
    let preview = MutatePreviewResponse {
        session_id: session_id.to_string(),
        path: mutation.path.as_str().to_string(),
        old_value: mutation.old_value,
        new_value: mutation.new_value,
        valid: candidate_checks.iter().all(|check| check.ok),
        effective_config_changed: candidate_value != base_value,
        would_clear_override: candidate_value == baseline,
        checks: candidate_checks,
    };
    if !preview.valid {
        return Err(serde_json::json!({
            "error": "invalid_runtime_config",
            "checks": preview.checks,
        }));
    }
    Ok((preview, candidate_config, mutation.drift))
}

fn persist_config_mutation(
    session_id: &str,
    args: &SelfMutateConfigArgs,
) -> Result<MutateApplyResponse, String> {
    persist_config_mutation_value(session_id, &args.path, parse_value_arg(&args.value))
}

fn persist_config_mutation_value(
    session_id: &str,
    path: &str,
    new_value: serde_json::Value,
) -> Result<MutateApplyResponse, String> {
    let outcome = session_workspace::update_existing_workspace_config(
        session_id,
        |workspace| {
            let base_config = effective_runtime_config(Some(workspace))
                .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
            let (preview, candidate_config) =
                prepare_config_mutation(session_id, path, new_value.clone(), &base_config)
                    .map_err(|error| {
                        std::io::Error::new(std::io::ErrorKind::InvalidInput, error)
                    })?;
            let candidate_json =
                serde_json::to_string(&candidate_config).map_err(std::io::Error::other)?;
            workspace.tuned_config_json = if preview.would_clear_override {
                None
            } else {
                Some(candidate_json)
            };
            workspace.updated_at = Utc::now().to_rfc3339();
            Ok(std::ops::ControlFlow::<(), _>::Continue((
                preview,
                workspace.turn_count,
            )))
        },
        |_, revision, value| {
            append_config_change_event(
                session_id,
                value.1,
                path,
                &value.0.new_value,
                Some(value.0.old_value.clone()),
                revision,
            )
        },
    )
    .map_err(|error| error.to_string())?;
    let (preview, config_revision, committed_config, postcommit) = match outcome {
        session_workspace::WorkspaceConfigMutationOutcome::Applied {
            value,
            revision,
            workspace,
            postcommit,
        } => (
            value.0,
            revision,
            effective_runtime_config(Some(&workspace))?,
            postcommit,
        ),
        session_workspace::WorkspaceConfigMutationOutcome::Rejected(()) => {
            unreachable!("unconditional CLI config mutation cannot reject")
        }
        session_workspace::WorkspaceConfigMutationOutcome::OutcomeUnknown { reason, .. } => {
            return Err(format!("workspace config commit outcome unknown: {reason}"));
        }
    };
    let audit_warning = postcommit
        .warning
        .map(|warning| warning.chars().take(240).collect());
    Ok(MutateApplyResponse {
        preview,
        config_revision,
        committed_config,
        audit_recorded: postcommit.recorded,
        audit_warning,
    })
}

pub(crate) fn persist_config_override(
    session_id: &str,
    path: &str,
    new_value: serde_json::Value,
) -> Result<MutateApplyResponse, String> {
    persist_config_mutation_value(session_id, path, new_value)
}

pub(crate) fn persist_governed_config_override(
    session_id: &str,
    path: &str,
    new_value: serde_json::Value,
    force: bool,
    drift_ceiling: f64,
) -> Result<(MutateApplyResponse, Option<f64>), GovernedConfigMutationError> {
    let outcome = session_workspace::update_existing_workspace_config(
        session_id,
        |workspace| {
            let base_config = effective_runtime_config(Some(workspace))
                .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
            let prepared = prepare_governed_config_mutation(
                session_id,
                path,
                new_value.clone(),
                &base_config,
                force,
                drift_ceiling,
            );
            let (preview, candidate_config, drift) = match prepared {
                Ok(prepared) => prepared,
                Err(rejection) => return Ok(std::ops::ControlFlow::Break(rejection)),
            };
            workspace.tuned_config_json = if preview.would_clear_override {
                None
            } else {
                Some(serde_json::to_string(&candidate_config).map_err(std::io::Error::other)?)
            };
            workspace.updated_at = Utc::now().to_rfc3339();
            Ok(std::ops::ControlFlow::Continue((
                preview,
                candidate_config,
                drift,
                workspace.turn_count,
                workspace.tuned_config_json.clone(),
            )))
        },
        |_, revision, value| {
            append_config_change_event(
                session_id,
                value.3,
                path,
                &value.0.new_value,
                Some(value.0.old_value.clone()),
                revision,
            )
        },
    )
    .map_err(|error| GovernedConfigMutationError::Persistence(error.to_string()))?;
    let (preview, committed_config, drift, config_revision, postcommit) = match outcome {
        session_workspace::WorkspaceConfigMutationOutcome::Applied {
            value,
            revision,
            postcommit,
            ..
        } => (value.0, value.1, value.2, revision, postcommit),
        session_workspace::WorkspaceConfigMutationOutcome::Rejected(rejection) => {
            return Err(GovernedConfigMutationError::Rejected(rejection));
        }
        session_workspace::WorkspaceConfigMutationOutcome::OutcomeUnknown {
            value,
            revision,
            observed,
            reason,
        } => {
            let observed_revision = observed
                .as_ref()
                .map(|workspace| workspace.config_mutation_revision);
            let observed_config = observed
                .as_deref()
                .and_then(|workspace| effective_runtime_config(Some(workspace)).ok());
            let retry_revision = session_workspace::exact_workspace_config_owner_revision(
                revision,
                &value.4,
                observed.as_deref(),
            );
            return Err(GovernedConfigMutationError::OutcomeUnknown(Box::new(
                GovernedConfigMutationUnknown {
                    preview: value.0,
                    drift: value.2,
                    proposed_revision: revision,
                    observed_revision,
                    observed_config,
                    retry_revision,
                    reason,
                },
            )));
        }
    };
    let audit_warning = postcommit
        .warning
        .map(|warning| warning.chars().take(240).collect());
    Ok((
        MutateApplyResponse {
            preview,
            config_revision,
            committed_config,
            audit_recorded: postcommit.recorded,
            audit_warning,
        },
        drift,
    ))
}

pub(crate) fn restore_config_override(
    session_id: &str,
    path: &str,
    new_value: serde_json::Value,
    expected_revision: u64,
) -> Result<session_workspace::WorkspaceConfigRestoreOutcome, String> {
    let outcome = session_workspace::restore_workspace_config_override(
        session_id,
        path,
        new_value.clone(),
        expected_revision,
        |workspace, revision, previous_value| {
            append_config_change_event(
                session_id,
                workspace.turn_count,
                path,
                &new_value,
                Some(previous_value.clone()),
                revision,
            )
        },
    )?;
    Ok(outcome)
}

pub(crate) fn persist_manual_compression(
    session_id: &str,
    turn: u32,
    reason: &str,
) -> Result<(), String> {
    let writer = session_journal::JournalWriter::new(session_id).map_err(|e| e.to_string())?;
    let mut evt = JournalEvent::compact(Some(session_id), turn, 1, 0);
    evt.metadata = Some(serde_json::json!({
        "source": "compress_context",
        "reason": reason,
    }));
    writer.append(&evt).map_err(|e| e.to_string())
}

fn append_config_change_event(
    session_id: &str,
    turn: u32,
    key: &str,
    new_value: &serde_json::Value,
    old_value: Option<serde_json::Value>,
    config_revision: u64,
) -> Result<(), String> {
    let writer = session_journal::JournalWriter::new(session_id).map_err(|e| e.to_string())?;
    let mut metadata = serde_json::Map::new();
    metadata.insert(
        "source".to_string(),
        serde_json::Value::String("astra self mutate".to_string()),
    );
    metadata.insert(
        "config_revision".to_string(),
        serde_json::Value::from(config_revision),
    );
    if let Some(old_value) = old_value {
        metadata.insert("old_value".to_string(), old_value);
    }
    let mut evt = JournalEvent::config_change(Some(session_id), key, &new_value.to_string());
    evt.turn = Some(turn);
    evt.metadata = Some(serde_json::Value::Object(metadata));
    writer.append(&evt).map_err(|e| e.to_string())
}

fn recent_event_previews(
    events: &[JournalEvent],
    limit: usize,
    kinds: &[JournalEventType],
) -> Vec<EventPreview> {
    events
        .iter()
        .rev()
        .filter(|event| kinds.iter().any(|kind| kind == &event.event_type))
        .take(limit)
        .map(event_preview)
        .collect()
}

fn event_preview(event: &JournalEvent) -> EventPreview {
    EventPreview {
        event_type: event_type_name(&event.event_type),
        ts: event.ts.clone(),
        turn: event.turn,
        error: event.error.clone(),
        tools_used: event.tools_used.clone(),
        metadata: event.metadata.clone(),
        user_input_preview: event.user_input.as_deref().map(|s| truncate(s, 160)),
        assistant_output_preview: event.assistant_output.as_deref().map(|s| truncate(s, 160)),
    }
}

fn analysis_view_recent_event_previews(
    events: &[JournalEvent],
    journal_limit: usize,
    analysis_view: &str,
) -> Vec<EventPreview> {
    let event_types: &[JournalEventType] = match analysis_view {
        "execution_errors" => &[
            JournalEventType::TurnError,
            JournalEventType::Error,
            JournalEventType::ToolCallError,
            JournalEventType::TurnGuardVerdict,
            JournalEventType::StallDetected,
            JournalEventType::TurnEvaluation,
            JournalEventType::PipelineAlert,
            JournalEventType::AgentSpawned,
            JournalEventType::AgentTerminated,
            JournalEventType::InterruptionRecorded,
            JournalEventType::VerificationCompleted,
            JournalEventType::Turn,
        ],
        "runtime_performance" => &[
            JournalEventType::Turn,
            JournalEventType::TurnError,
            JournalEventType::StallDetected,
            JournalEventType::TurnEvaluation,
            JournalEventType::PipelineAlert,
            JournalEventType::SessionMemoryExtraction,
            JournalEventType::AgentSpawned,
            JournalEventType::AgentTerminated,
            JournalEventType::InterruptionRecorded,
            JournalEventType::AdaptivePerTurnApplied,
        ],
        "execution_tools" => &[
            JournalEventType::Turn,
            JournalEventType::ToolCallError,
            JournalEventType::TurnGuardVerdict,
            JournalEventType::TurnEvaluation,
            JournalEventType::PipelineAlert,
            JournalEventType::AgentSpawned,
            JournalEventType::AgentTerminated,
            JournalEventType::InterruptionRecorded,
            JournalEventType::AdaptiveScenarioApplied,
            JournalEventType::AdaptivePerTurnApplied,
        ],
        "execution_trace" => &[
            JournalEventType::Turn,
            JournalEventType::TurnError,
            JournalEventType::Error,
            JournalEventType::ToolCallError,
            JournalEventType::TurnGuardVerdict,
            JournalEventType::StallDetected,
            JournalEventType::DriftDetected,
            JournalEventType::TurnEvaluation,
            JournalEventType::PipelineAlert,
            JournalEventType::SessionMemoryExtraction,
            JournalEventType::AgentSpawned,
            JournalEventType::AgentTerminated,
            JournalEventType::InterruptionRecorded,
            JournalEventType::AdaptiveScenarioApplied,
            JournalEventType::AdaptivePerTurnApplied,
            JournalEventType::VerificationCompleted,
        ],
        _ => &[
            JournalEventType::Turn,
            JournalEventType::TurnError,
            JournalEventType::Error,
            JournalEventType::StallDetected,
            JournalEventType::DriftDetected,
            JournalEventType::TurnEvaluation,
            JournalEventType::PipelineAlert,
            JournalEventType::SessionMemoryExtraction,
            JournalEventType::AgentSpawned,
            JournalEventType::AgentTerminated,
            JournalEventType::InterruptionRecorded,
            JournalEventType::AdaptiveScenarioApplied,
            JournalEventType::AdaptivePerTurnApplied,
        ],
    };
    let limit = journal_limit.clamp(1, 12);
    if analysis_view == "execution_errors" {
        return events
            .iter()
            .rev()
            .filter(|event| event_types.iter().any(|kind| kind == &event.event_type))
            .map(event_preview)
            .filter(event_preview_has_adverse_signal)
            .take(limit)
            .collect();
    }
    recent_event_previews(events, limit, event_types)
}

fn restored_recent_turn_previews(
    artifacts: &SessionArtifacts,
    journal_limit: usize,
) -> Vec<EventPreview> {
    let Some(restored) = artifacts.restored.as_ref() else {
        return Vec::new();
    };

    let ts = artifacts
        .workspace
        .as_ref()
        .map(|workspace| workspace.updated_at.clone())
        .unwrap_or_default();
    let mut previews = Vec::new();
    let mut pending_user: Option<String> = None;
    let mut turn = 0u32;

    for message in restored.resume_messages() {
        let role = message.get("role").and_then(serde_json::Value::as_str);
        match role {
            Some("user") if astra_turn_types::is_human_user_message(message) => {
                pending_user = extract_text_content(message);
            }
            Some("assistant") => {
                turn += 1;
                previews.push(EventPreview {
                    event_type: "turn".to_string(),
                    ts: ts.clone(),
                    turn: Some(turn),
                    error: None,
                    tools_used: None,
                    metadata: Some(serde_json::json!({ "source": "cloud_resume" })),
                    user_input_preview: pending_user.take().map(|text| truncate(&text, 160)),
                    assistant_output_preview: extract_text_content(message)
                        .map(|text| truncate(&text, 160)),
                });
            }
            Some("tool") => {
                if let Some(last) = previews.last_mut() {
                    let tool_name = message
                        .get("name")
                        .and_then(serde_json::Value::as_str)
                        .map(str::to_string)
                        .or_else(|| {
                            message
                                .get("tool_name")
                                .and_then(serde_json::Value::as_str)
                                .map(str::to_string)
                        });
                    if let Some(tool_name) = tool_name {
                        last.tools_used.get_or_insert_with(Vec::new).push(tool_name);
                    }
                }
            }
            _ => {}
        }
    }

    if pending_user.is_some() {
        turn += 1;
        previews.push(EventPreview {
            event_type: "turn".to_string(),
            ts,
            turn: Some(turn),
            error: None,
            tools_used: None,
            metadata: Some(serde_json::json!({ "source": "cloud_resume" })),
            user_input_preview: pending_user.map(|text| truncate(&text, 160)),
            assistant_output_preview: None,
        });
    }

    previews
        .into_iter()
        .rev()
        .take(journal_limit.clamp(1, 12))
        .collect()
}

fn effective_runtime_config(
    workspace: Option<&WorkspaceMetadata>,
) -> Result<RuntimeConfig, String> {
    match workspace.and_then(|ws| ws.tuned_config_json.as_deref()) {
        Some(json) => serde_json::from_str(json).map_err(|e| e.to_string()),
        None => Ok(RuntimeConfig::load()),
    }
}

pub(crate) fn verify_runtime_config(tuned_config_json: Option<&str>) -> Vec<CheckResult> {
    let mut checks = Vec::new();
    let config = match tuned_config_json {
        Some(json) => match serde_json::from_str::<RuntimeConfig>(json) {
            Ok(config) => {
                checks.push(CheckResult {
                    name: "runtime_config_parse".to_string(),
                    ok: true,
                    detail: "tuned RuntimeConfig parsed".to_string(),
                });
                config
            }
            Err(err) => {
                checks.push(CheckResult {
                    name: "runtime_config_parse".to_string(),
                    ok: false,
                    detail: err.to_string(),
                });
                return checks;
            }
        },
        None => {
            checks.push(CheckResult {
                name: "runtime_config_parse".to_string(),
                ok: true,
                detail: "using baseline RuntimeConfig::load()".to_string(),
            });
            RuntimeConfig::load()
        }
    };

    let invariant_validation = astra_config::governed_config_invariant_validation(&config);
    checks.push(CheckResult {
        name: "verification_bounds".to_string(),
        ok: invariant_validation.verification_bounds,
        detail: format!(
            "min={} strictness={} max={}",
            config.verification.min_strictness,
            config.verification.strictness,
            config.verification.max_strictness
        ),
    });

    checks.push(CheckResult {
        name: "compression_bounds".to_string(),
        ok: invariant_validation.compression_bounds,
        detail: format!(
            "compression={} window=[{}, {}]",
            config.compression.compression_threshold,
            config.context_window.compression_threshold_min,
            config.context_window.compression_threshold_max
        ),
    });

    let available_tools = cli_provider_visible_tool_names().len();
    let min_required = ConstraintSet::default().min_available_tool_count;
    checks.push(CheckResult {
        name: "available_tool_floor".to_string(),
        ok: available_tools >= min_required,
        detail: format!(
            "available_tools={} min_required={}",
            available_tools, min_required
        ),
    });

    checks
}

pub(crate) fn cli_provider_visible_tool_names() -> Vec<String> {
    let mut names = edge_tools::local_tool_schemas()
        .iter()
        .filter_map(|schema| tool_schema_name(schema).map(str::to_string))
        .collect::<Vec<_>>();
    names.sort();
    names.dedup();
    names
}

fn parse_value_arg(raw: &str) -> serde_json::Value {
    serde_json::from_str(raw).unwrap_or_else(|_| serde_json::Value::String(raw.to_string()))
}

fn event_type_name(event_type: &JournalEventType) -> String {
    match event_type {
        JournalEventType::SessionStart => "session_start",
        JournalEventType::Turn => "turn",
        JournalEventType::TurnError => "turn_error",
        JournalEventType::Compact => "compact",
        JournalEventType::ConfigChange => "config_change",
        JournalEventType::Error => "error",
        JournalEventType::SessionEnd => "session_end",
        JournalEventType::StallDetected => "stall_detected",
        JournalEventType::Checkpoint => "checkpoint",
        JournalEventType::TurnGuardVerdict => "turn_guard_verdict",
        JournalEventType::TurnEvaluation => "turn_evaluation",
        JournalEventType::PlanProgress => "plan_progress",
        JournalEventType::SessionFork => "session_fork",
        JournalEventType::SyncMarker => "sync_marker",
        JournalEventType::DelegationStarted => "delegation_started",
        JournalEventType::DelegationSubRunStarted => "delegation_sub_run_started",
        JournalEventType::DelegationSubRunCompleted => "delegation_sub_run_completed",
        JournalEventType::DelegationRetry => "delegation_retry",
        JournalEventType::DelegationCompleted => "delegation_completed",
        JournalEventType::AgentSpawned => "agent_spawned",
        JournalEventType::AgentTerminated => "agent_terminated",
        JournalEventType::TranscriptItem => "transcript_item",
        JournalEventType::VerificationCompleted => "verification_completed",
        JournalEventType::PlanEdit => "plan_edit",
        JournalEventType::PlanLifecycle => "plan_lifecycle",
        JournalEventType::TaskLifecycle => "task_lifecycle",
        JournalEventType::GoalSteered => "goal_steered",
        JournalEventType::ApprovalRequired => "approval_required",
        JournalEventType::ApprovalDecision => "approval_decision",
        JournalEventType::ApprovalTimeout => "approval_timeout",
        JournalEventType::AskUserPrompted => "ask_user_prompted",
        JournalEventType::AskUserResponse => "ask_user_response",
        JournalEventType::PermissionAudit => "permission_audit",
        JournalEventType::ExecutionBoundaryOpened => "execution_boundary_opened",
        JournalEventType::ExecutionBoundaryCommitted => "execution_boundary_committed",
        JournalEventType::ExecutionBoundaryAborted => "execution_boundary_aborted",
        JournalEventType::ContextAssemblyRecorded => "context_assembly_recorded",
        JournalEventType::DriftDetected => "drift_detected",
        JournalEventType::AdaptiveScenarioApplied => "adaptive_scenario_applied",
        JournalEventType::AdaptivePerTurnApplied => "adaptive_per_turn_applied",
        JournalEventType::InterruptionRecorded => "interruption_recorded",
        JournalEventType::CompactionRetry => "compaction_retry",
        JournalEventType::LlmRound => "llm_round",
        JournalEventType::LlmRequestFull => "llm_request_full",
        JournalEventType::LlmResponseFull => "llm_response_full",
        JournalEventType::SessionMemoryExtraction => "session_memory_extraction",
        JournalEventType::SubsystemDiagnostic => "subsystem_diagnostic",
        JournalEventType::SubsystemSettled => "subsystem_settled",
        JournalEventType::PipelineFeedback => "pipeline_feedback",
        JournalEventType::PipelineAlert => "pipeline_alert",
        JournalEventType::PipelineCompactionAudit => "pipeline_compaction_audit",
        JournalEventType::Bootstrap => "bootstrap",
        JournalEventType::TraceSpan => "trace_span",
        JournalEventType::ToolCallError => "tool_call_error",
    }
    .to_string()
}

fn truncate(input: &str, max_chars: usize) -> String {
    let head: String = input.chars().take(max_chars).collect();
    if input.chars().count() > max_chars {
        format!("{head}...")
    } else {
        head
    }
}

fn compact_json_value(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::Null => "null".to_string(),
        serde_json::Value::String(inner) => truncate(inner, 80),
        _ => truncate(&value.to_string(), 80),
    }
}

fn event_preview_summary(event: &EventPreview) -> String {
    let metadata_detail = event
        .metadata
        .as_ref()
        .and_then(|metadata| event_metadata_detail(&event.event_type, metadata));
    event
        .error
        .as_deref()
        .or(metadata_detail.as_deref())
        .or(event.user_input_preview.as_deref())
        .or(event.assistant_output_preview.as_deref())
        .map(|detail| format!("{}: {}", event.event_type, truncate(detail, 180)))
        .unwrap_or_else(|| event.event_type.clone())
}

fn event_metadata_detail(event_type: &str, metadata: &serde_json::Value) -> Option<String> {
    match event_type {
        "turn_evaluation" => metadata
            .get("signals")
            .and_then(serde_json::Value::as_array)
            .and_then(|signals| {
                signals
                    .iter()
                    .find_map(|signal| signal.get("message").and_then(serde_json::Value::as_str))
            })
            .map(str::to_string)
            .or_else(|| {
                (metadata.get("success").and_then(serde_json::Value::as_bool) == Some(false))
                    .then(|| "turn evaluation reported unsuccessful execution".to_string())
            }),
        "pipeline_alert" => metadata
            .get("alert_message")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string),
        "session_memory_extraction" => metadata
            .get("llm_detail")
            .and_then(serde_json::Value::as_str)
            .or_else(|| metadata.get("reason").and_then(serde_json::Value::as_str))
            .map(str::to_string),
        "agent_spawned" => {
            let agent_id = metadata
                .get("agent_id")
                .and_then(serde_json::Value::as_str)?;
            let agent_type = metadata
                .get("agent_type")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("unknown");
            let description = metadata
                .get("description")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("");
            Some(format!(
                "agent={agent_id}; type={agent_type}; task={description}"
            ))
        }
        "agent_terminated" => metadata_fields_summary(
            metadata,
            &[
                "agent_id",
                "status",
                "finish_reason",
                "turns_completed",
                "tool_calls",
                "prompt_tokens",
                "completion_tokens",
                "duration_ms",
            ],
        ),
        "interruption_recorded" => metadata_fields_summary(
            metadata.get("interruption")?,
            &[
                "kind",
                "resume_mode",
                "turns_completed",
                "tool_calls_completed",
                "remaining_turns",
                "stall_signal",
            ],
        ),
        _ => None,
    }
}

fn metadata_fields_summary(metadata: &serde_json::Value, keys: &[&str]) -> Option<String> {
    let fields = keys
        .iter()
        .filter_map(|key| {
            metadata
                .get(*key)
                .filter(|value| !value.is_null())
                .map(|value| format!("{key}={}", compact_json_value(value)))
        })
        .collect::<Vec<_>>();
    (!fields.is_empty()).then(|| fields.join("; "))
}

fn event_preview_has_adverse_signal(event: &EventPreview) -> bool {
    if event.error.is_some()
        || matches!(
            event.event_type.as_str(),
            "turn_error" | "error" | "tool_call_error" | "stall_detected"
        )
    {
        return true;
    }
    let Some(metadata) = event.metadata.as_ref() else {
        return false;
    };
    match event.event_type.as_str() {
        "turn_evaluation" => {
            metadata.get("success").and_then(serde_json::Value::as_bool) == Some(false)
                || metadata
                    .get("verdict_warning")
                    .and_then(serde_json::Value::as_bool)
                    == Some(true)
        }
        "pipeline_alert" => metadata
            .get("alert_severity")
            .and_then(serde_json::Value::as_str)
            .is_some_and(|severity| {
                matches!(
                    severity.to_ascii_lowercase().as_str(),
                    "warning" | "error" | "critical"
                )
            }),
        "turn_guard_verdict" => {
            metadata
                .get("severity")
                .and_then(serde_json::Value::as_str)
                .is_some_and(|severity| matches!(severity, "warning" | "error" | "critical"))
                || metadata
                    .get("advisory_threshold_reached")
                    .and_then(serde_json::Value::as_bool)
                    == Some(true)
                || metadata
                    .get("avoid_tools_count")
                    .and_then(serde_json::Value::as_u64)
                    .is_some_and(|count| count > 0)
                || metadata
                    .get("nudge_count")
                    .and_then(serde_json::Value::as_u64)
                    .is_some_and(|count| count > 0)
        }
        "session_memory_extraction" => {
            metadata
                .get("outcome")
                .and_then(serde_json::Value::as_str)
                .is_some_and(|outcome| matches!(outcome, "errored" | "failed"))
                || metadata.get("source").and_then(serde_json::Value::as_str)
                    == Some("rule_fallback")
        }
        "agent_terminated" => metadata
            .get("status")
            .and_then(serde_json::Value::as_str)
            .is_some_and(|status| matches!(status, "failed" | "cancelled" | "interrupted")),
        "interruption_recorded" => true,
        _ => false,
    }
}

fn event_preview_evidence_source(event: &EventPreview) -> &'static str {
    if event
        .metadata
        .as_ref()
        .and_then(|metadata| metadata.get("source"))
        .and_then(serde_json::Value::as_str)
        == Some("cloud_resume")
    {
        "cloud_resume"
    } else {
        "local_journal"
    }
}

fn event_preview_ref_namespace(event: &EventPreview) -> &'static str {
    match event_preview_evidence_source(event) {
        "cloud_resume" => "cloud",
        _ => "local",
    }
}

#[cfg(test)]
mod tests {
    use super::{
        EventPreview, analysis_view_recent_event_previews, build_reflect_response,
        cli_provider_visible_tool_names, event_preview_has_adverse_signal, event_preview_summary,
        execute_self_command, persist_config_override, resolve_session_id,
        restored_recent_turn_previews, session_agent_delivery_summary, verify_runtime_config,
    };
    use crate::cli::cli_config::cli_args::{
        SelfCmd, SelfMutateCmd, SelfMutateConfigArgs, SelfReflectArgs, SelfSessionArgs,
    };
    use crate::cli::cli_config::cli_utils::{
        CredentialsFile, Profile, load_credentials, save_credentials,
    };
    use astra_services::reflect::ReflectRequest;

    #[test]
    fn reflect_agent_termination_preserves_execution_metrics_and_reason() {
        let event = EventPreview {
            event_type: "agent_terminated".to_string(),
            ts: "2026-07-18T00:00:00Z".to_string(),
            turn: Some(2),
            error: None,
            tools_used: None,
            metadata: Some(serde_json::json!({
                "agent_id": "reviewer@abc",
                "status": "interrupted",
                "finish_reason": "empty_completion",
                "turns_completed": 6,
                "tool_calls": 5,
                "prompt_tokens": 43086,
                "completion_tokens": 120,
                "duration_ms": 127900
            })),
            user_input_preview: None,
            assistant_output_preview: None,
        };

        let summary = event_preview_summary(&event);

        for evidence in [
            "status=interrupted",
            "finish_reason=empty_completion",
            "turns_completed=6",
            "tool_calls=5",
            "prompt_tokens=43086",
        ] {
            assert!(summary.contains(evidence), "{summary}");
        }
    }

    #[test]
    fn restored_preview_does_not_show_runtime_authority_as_user_input() {
        let mut runtime = serde_json::json!({"role": "user", "content": "runtime control"});
        astra_turn_types::mark_append_only_required_context(
            &mut runtime,
            "final_answer_settlement",
            astra_turn_types::RuntimeAuthorityLifetime::NextAssistantDecision,
        );
        let messages = vec![
            serde_json::json!({"role": "user", "content": "real request"}),
            runtime,
            serde_json::json!({"role": "assistant", "content": "answer"}),
        ];
        let artifacts = astra_services::self_surface::LoadedSelfSurfaceArtifacts {
            session_id: "sid".to_string(),
            workspace: None,
            restored: Some(astra_services::session_restore::RestoredSession {
                session_id: "sid".to_string(),
                resume_bundle: Some(typed_resume_bundle("sid", 1, messages)),
                ..Default::default()
            }),
            journal_events: Vec::new(),
            latest_full_context_trace: None,
        };

        let previews = restored_recent_turn_previews(&artifacts, 4);
        assert_eq!(previews.len(), 1);
        assert_eq!(
            previews[0].user_input_preview.as_deref(),
            Some("real request")
        );
        assert_eq!(
            previews[0].assistant_output_preview.as_deref(),
            Some("answer")
        );
    }

    #[test]
    fn reflect_interruption_preserves_resume_and_stall_causality() {
        let event = EventPreview {
            event_type: "interruption_recorded".to_string(),
            ts: "2026-07-18T00:00:00Z".to_string(),
            turn: Some(1),
            error: None,
            tools_used: None,
            metadata: Some(serde_json::json!({
                "interruption": {
                    "kind": "empty_completion",
                    "resume_mode": "settle",
                    "turns_completed": 11,
                    "tool_calls_completed": 17,
                    "remaining_turns": 289,
                    "stall_signal": "single_tool_streak=9"
                }
            })),
            user_input_preview: Some("legacy flattened summary".to_string()),
            assistant_output_preview: None,
        };

        let summary = event_preview_summary(&event);

        assert!(summary.contains("kind=empty_completion"), "{summary}");
        assert!(summary.contains("remaining_turns=289"), "{summary}");
        assert!(
            summary.contains("stall_signal=single_tool_streak=9"),
            "{summary}"
        );
        assert!(event_preview_has_adverse_signal(&event));
    }

    #[test]
    fn reflect_summary_reports_agent_deliverable_ratio_from_typed_lifecycle() {
        let mut events = Vec::new();
        for index in 0..3 {
            events.push(
                astra_services::session_journal::JournalEvent::agent_spawned(
                    Some("session-1"),
                    &format!("reviewer-{index}"),
                    &format!("run-{index}"),
                    "root-run",
                    "code-review",
                    "Review one angle",
                    None,
                    false,
                    None,
                ),
            );
        }
        for index in 0..3 {
            events.push(
                astra_services::session_journal::JournalEvent::agent_terminated(
                    Some("session-1"),
                    &format!("reviewer-{index}"),
                    &format!("run-{index}"),
                    "code-review",
                    "interrupted",
                    Some("empty_completion"),
                    Some(6 + index),
                    5 + index,
                    43_000,
                    0,
                    120_000,
                    None,
                ),
            );
        }
        // Replayed lifecycle evidence must not change the delivery ratio.
        events.push(
            astra_services::session_journal::JournalEvent::agent_spawned(
                Some("session-1"),
                "reviewer-0",
                "run-0",
                "root-run",
                "code-review",
                "Review one angle",
                None,
                false,
                None,
            ),
        );
        events.push(
            astra_services::session_journal::JournalEvent::agent_terminated(
                Some("session-1"),
                "reviewer-0",
                "run-0",
                "code-review",
                "interrupted",
                Some("empty_completion"),
                Some(6),
                5,
                43_000,
                0,
                120_000,
                None,
            ),
        );

        let summary = session_agent_delivery_summary(&events).unwrap();

        assert!(summary.contains("0/3"), "{summary}");
        assert!(summary.contains("3 interrupted"), "{summary}");
        assert!(summary.contains("0 other terminal"), "{summary}");
        assert!(summary.contains("0 without terminal evidence"), "{summary}");
    }
    use astra_services::self_surface::LoadedSelfSurfaceArtifacts;
    use astra_services::session_journal::{
        self, JournalDirGuard, JournalEvent, JournalEventType, ToolCallRecord,
    };
    use astra_services::session_workspace::{self, ContextTraceSignal, WorkspaceMetadata};
    use chrono::Utc;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    struct EnvGuard {
        key: &'static str,
        old: Option<String>,
    }

    impl EnvGuard {
        fn set(key: &'static str, value: &str) -> Self {
            let old = std::env::var(key).ok();
            unsafe {
                std::env::set_var(key, value);
            }
            Self { key, old }
        }
    }

    #[test]
    #[serial_test::serial]
    fn config_override_preserves_background_task_projection() {
        let temp = tempfile::tempdir().unwrap();
        let _guard = JournalDirGuard::new(temp.path());
        let session_id = "self-config-preserves-background";
        let mut workspace = WorkspaceMetadata::new(session_id, "gpt-5");
        workspace.background_shell_tasks = vec![session_workspace::BackgroundShellTaskProjection {
            id: "shell-1".into(),
            status: "running".into(),
            title: "make check".into(),
            started_at_ms: 1,
            ended_at_ms: None,
            stdout_path: "/tmp/shell-1.stdout".into(),
            stderr_path: "/tmp/shell-1.stderr".into(),
            exit_code: None,
            terminal_reason: None,
        }];
        session_workspace::write_workspace(&workspace).unwrap();
        let current = astra_config::runtime_config::RuntimeConfig::load()
            .memory
            .retrieval_top_k;
        let replacement = if current == 5 { 6 } else { 5 };

        persist_config_override(
            session_id,
            "memory.retrieval_top_k",
            serde_json::json!(replacement),
        )
        .unwrap();

        let persisted = session_workspace::read_workspace(session_id).unwrap();
        assert_eq!(persisted.background_shell_tasks.len(), 1);
        assert!(persisted.tuned_config_json.is_some());
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn mutate_apply_reports_audit_failure_without_rolling_back_config() {
        let temp = tempfile::tempdir().unwrap();
        let _guard = JournalDirGuard::new(temp.path());
        let session_id = "self-config-audit-failure";
        session_workspace::write_workspace(&WorkspaceMetadata::new(session_id, "gpt-5")).unwrap();
        std::fs::create_dir_all(session_journal::journal_file_path(session_id)).unwrap();
        let current = astra_config::RuntimeConfig::load().memory.retrieval_top_k;
        let replacement = if current == 5 { 6 } else { 5 };

        let output = execute_self_command(
            &SelfCmd::Mutate(SelfMutateCmd::Apply(SelfMutateConfigArgs {
                session_id: Some(session_id.into()),
                path: "memory.retrieval_top_k".into(),
                value: replacement.to_string(),
            })),
            None,
        )
        .await
        .unwrap();
        let output: serde_json::Value = serde_json::from_str(&output).unwrap();

        assert_eq!(output["audit_recorded"], false);
        assert!(
            output["audit_warning"]
                .as_str()
                .is_some_and(|value| !value.is_empty())
        );
        let persisted = session_workspace::read_workspace(session_id).unwrap();
        assert_eq!(persisted.config_mutation_revision, 1);
        assert!(persisted.tuned_config_json.is_some());
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            match &self.old {
                Some(value) => unsafe {
                    std::env::set_var(self.key, value);
                },
                None => unsafe {
                    std::env::remove_var(self.key);
                },
            }
        }
    }

    fn typed_resume_bundle(
        session_id: &str,
        turn_count: u32,
        messages: Vec<serde_json::Value>,
    ) -> astra_turn_types::ResumeBundleV1 {
        let sequence = u64::from(turn_count);
        let cursor = astra_turn_types::SessionCursorV1 {
            schema_version: astra_turn_types::SESSION_CURSOR_SCHEMA_VERSION,
            owner_id: crate::cli::cli_config::cli_utils::cli_user_id(),
            session_id: session_id.to_string(),
            branch_id: astra_turn_types::DEFAULT_CONVERSATION_BRANCH_ID.to_string(),
            completed_turn: turn_count,
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
                projections: Default::default(),
            }],
        )
        .expect("valid test resume bundle")
    }

    async fn mock_cloud_resume(
        server: &MockServer,
        session_id: &str,
        restored: &astra_services::session_restore::RestoredSession,
    ) {
        let mut restored = restored.clone();
        if restored.resume_bundle.is_none() {
            restored.resume_bundle = Some(typed_resume_bundle(
                session_id,
                restored.turn_count,
                restored.conversation_messages.clone(),
            ));
        }
        restored.conversation_messages.clear();
        Mock::given(method("POST"))
            .and(path(format!("/sessions/{session_id}/resume")))
            .respond_with(ResponseTemplate::new(200).set_body_json(restored))
            .mount(server)
            .await;
    }

    async fn mock_cloud_resumable_list(
        server: &MockServer,
        sessions: &[astra_services::session_restore::RestoredSession],
    ) {
        Mock::given(method("GET"))
            .and(path("/sessions/resumable"))
            .respond_with(ResponseTemplate::new(200).set_body_json(
                astra_services::session_restore::ResumableSessionsResponse {
                    sessions: sessions.to_vec(),
                },
            ))
            .mount(server)
            .await;
    }

    #[test]
    fn verify_runtime_config_flags_invalid_bounds() {
        let checks = verify_runtime_config(Some(
            r#"{"verification":{"strictness":0.2,"min_strictness":0.6,"max_strictness":0.9}}"#,
        ));

        assert!(
            checks
                .iter()
                .any(|check| { check.name == "verification_bounds" && !check.ok })
        );
    }

    #[test]
    fn self_runtime_tool_names_use_provider_visible_cli_surface() {
        let names = cli_provider_visible_tool_names();
        assert!(names.contains(&"bash".to_string()));
        assert!(!names.contains(&"delete_file".to_string()));
        assert!(!names.contains(&"multi_edit".to_string()));

        let checks = verify_runtime_config(None);
        let available = checks
            .iter()
            .find(|check| check.name == "available_tool_floor")
            .expect("available_tool_floor check");
        assert!(available.ok);
        assert!(
            available
                .detail
                .contains(&format!("available_tools={}", names.len())),
            "{}",
            available.detail
        );
    }

    #[test]
    fn resolve_session_id_rejects_invalid_input_without_panicking() {
        let error = resolve_session_id(Some("../bad"), None).unwrap_err();
        assert!(error.contains("no session journal or workspace matches"));
    }

    #[tokio::test]
    async fn snapshot_aggregates_workspace_and_journal_state() {
        let temp = tempfile::tempdir().unwrap();
        let _guard = JournalDirGuard::new(temp.path());
        let session_id = "self-snapshot-session";
        let mut ws = WorkspaceMetadata::with_context(session_id, "gpt-5.4", "/repo", Some("main"));
        ws.discovered_skills = vec!["goal-driven-evolution".to_string()];
        ws.active_experiment_id = Some("exp-42".to_string());
        ws.last_context_trace = Some(ContextTraceSignal {
            turn_id: "turn-7".to_string(),
            captured_at: Some(Utc::now().to_rfc3339()),
            tool_surface: None,
            memory: None,
            history: None,
            budget: Some(
                astra_services::session_workspace::ContextTraceBudgetSignal {
                    max_tokens: 10000,
                    total_used: 7000,
                    budget_pressure: 0.7,
                    compression_triggered: false,
                },
            ),
            timing: None,
            explanations: vec!["budget stable".to_string()],
        });
        session_workspace::write_workspace(&ws).unwrap();

        let writer = session_journal::JournalWriter::new(session_id).unwrap();
        writer
            .append(&JournalEvent {
                event_type: JournalEventType::Turn,
                ts: Utc::now().to_rfc3339(),
                session_id: Some(session_id.to_string()),
                producer_scope: None,
                turn: Some(7),
                agentic_step: None,
                model: Some("gpt-5.4".to_string()),
                user_input: Some("continue".to_string()),
                assistant_output: Some("implemented".to_string()),
                tool_count: Some(1),
                tokens_in: Some(10),
                tokens_out: Some(20),
                duration_ms: Some(50),
                error: None,
                config_key: None,
                config_value: None,
                turns_compacted: None,
                facts_stored: None,
                visible_tools: Some(vec!["bash".to_string()]),
                selected_skills: None,
                tools_used: Some(vec!["bash".to_string()]),
                tool_calls: Some(vec![ToolCallRecord {
                    name: "bash".to_string(),
                    ok: true,
                    ms: 50,
                    error: None,
                    input_bytes: None,
                    output_bytes: None,
                    args_preview: None,
                    result_preview: None,
                    file_path: None,
                    surgically_removed: None,
                    original_tool_name: None,
                    ..Default::default()
                }]),
                tool_outcomes: None,
                budget_used: Some(7000),
                budget_pressure: Some(0.7),
                stall_type: None,
                metadata: None,
                plan_subtask_id: None,
                ttft_ms: None,
                context_ms: None,
                cache_read_tokens: None,
                cache_creation_tokens: None,
                memoria_ms: None,
                session_lineage: None,
                coordination: None,
                transcript_item: None,
                conversation_commit: None,
                edge_policy: None,
                context_assembly_trace: Some(serde_json::json!({"tokens": 7000})),
                routing_domain_hint: Some("code".to_string()),
                entity_learn_skipped_no_domain: false,
                round: None,
                tool_calls_returned: None,
                offset_ms: None,
                llm_rounds: None,
                total_llm_ms: None,
                total_tool_ms: None,
                parent_event_id: None,
                git_head: None,
                git_branch: None,
            })
            .unwrap();

        let body = execute_self_command(
            &SelfCmd::Snapshot(SelfSessionArgs {
                session_id: Some(session_id.to_string()),
            }),
            None,
        )
        .await
        .unwrap();

        let value: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(value["run"]["session_id"], session_id);
        assert!(value["run"]["goal"].is_null());
        assert_eq!(value["recent_steps"][0]["event_type"], "turn");
        assert!(
            value["environment"]["last_context_trace_preview"]
                .as_str()
                .unwrap()
                .contains("turn-7")
        );
        assert_eq!(value["acceptance"]["ok"], true);
    }

    #[tokio::test]
    async fn snapshot_surfaces_persistence_error_field() {
        let temp = tempfile::tempdir().unwrap();
        let _guard = JournalDirGuard::new(temp.path());
        let session_id = "self-snapshot-persistence";
        let mut ws = WorkspaceMetadata::with_context(session_id, "gpt-5.4", "/repo", Some("main"));
        ws.last_persistence_error = Some("failed to append turn event".to_string());
        session_workspace::write_workspace(&ws).unwrap();

        let writer = session_journal::JournalWriter::new(session_id).unwrap();
        writer
            .append(&JournalEvent {
                event_type: JournalEventType::Turn,
                ts: Utc::now().to_rfc3339(),
                session_id: Some(session_id.to_string()),
                producer_scope: None,
                turn: Some(1),
                agentic_step: None,
                model: Some("gpt-5.4".to_string()),
                user_input: Some("continue".to_string()),
                assistant_output: Some("implemented".to_string()),
                tool_count: Some(0),
                tokens_in: Some(10),
                tokens_out: Some(20),
                duration_ms: Some(50),
                error: None,
                config_key: None,
                config_value: None,
                turns_compacted: None,
                facts_stored: None,
                visible_tools: None,
                selected_skills: None,
                tools_used: None,
                tool_calls: None,
                tool_outcomes: None,
                budget_used: None,
                budget_pressure: None,
                stall_type: None,
                metadata: None,
                plan_subtask_id: None,
                ttft_ms: None,
                context_ms: None,
                cache_read_tokens: None,
                cache_creation_tokens: None,
                memoria_ms: None,
                session_lineage: None,
                coordination: None,
                transcript_item: None,
                conversation_commit: None,
                edge_policy: None,
                context_assembly_trace: None,
                routing_domain_hint: Some("code".to_string()),
                entity_learn_skipped_no_domain: false,
                round: None,
                tool_calls_returned: None,
                offset_ms: None,
                llm_rounds: None,
                total_llm_ms: None,
                total_tool_ms: None,
                parent_event_id: None,
                git_head: None,
                git_branch: None,
            })
            .unwrap();

        let body = execute_self_command(
            &SelfCmd::Snapshot(SelfSessionArgs {
                session_id: Some(session_id.to_string()),
            }),
            None,
        )
        .await
        .unwrap();

        let value: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(
            value["run"]["persistence_error"].as_str(),
            Some("failed to append turn event")
        );
    }

    #[tokio::test]
    async fn health_surface_exposes_risk_flags_and_acceptance() {
        let temp = tempfile::tempdir().unwrap();
        let _guard = JournalDirGuard::new(temp.path());
        let session_id = "self-health-session";
        let mut ws = WorkspaceMetadata::with_context(session_id, "gpt-5.4", "/repo", Some("main"));
        ws.last_context_trace = Some(ContextTraceSignal {
            turn_id: "turn-3".to_string(),
            captured_at: Some(Utc::now().to_rfc3339()),
            tool_surface: None,
            memory: None,
            history: None,
            budget: Some(
                astra_services::session_workspace::ContextTraceBudgetSignal {
                    max_tokens: 10000,
                    total_used: 9100,
                    budget_pressure: 0.91,
                    compression_triggered: true,
                },
            ),
            timing: None,
            explanations: vec!["pressure rising".to_string()],
        });
        session_workspace::write_workspace(&ws).unwrap();

        let writer = session_journal::JournalWriter::new(session_id).unwrap();
        writer
            .append(&JournalEvent {
                event_type: JournalEventType::TurnError,
                ts: Utc::now().to_rfc3339(),
                session_id: Some(session_id.to_string()),
                producer_scope: None,
                turn: Some(3),
                agentic_step: None,
                model: Some("gpt-5.4".to_string()),
                user_input: Some("debug".to_string()),
                assistant_output: None,
                tool_count: Some(1),
                tokens_in: Some(20),
                tokens_out: Some(0),
                duration_ms: Some(120),
                error: Some("bash failed".to_string()),
                config_key: None,
                config_value: None,
                turns_compacted: None,
                facts_stored: None,
                visible_tools: Some(vec!["bash".to_string()]),
                selected_skills: Some(vec!["goal-driven-evolution".to_string()]),
                tools_used: Some(vec!["bash".to_string()]),
                tool_calls: Some(vec![ToolCallRecord {
                    name: "bash".to_string(),
                    ok: false,
                    ms: 120,
                    error: Some("command timed out".to_string()),
                    input_bytes: None,
                    output_bytes: None,
                    args_preview: None,
                    result_preview: None,
                    file_path: None,
                    surgically_removed: None,
                    original_tool_name: None,
                    ..Default::default()
                }]),
                tool_outcomes: None,
                budget_used: Some(9100),
                budget_pressure: Some(0.91),
                stall_type: None,
                metadata: None,
                plan_subtask_id: None,
                ttft_ms: None,
                context_ms: None,
                cache_read_tokens: None,
                cache_creation_tokens: None,
                memoria_ms: None,
                session_lineage: None,
                coordination: None,
                transcript_item: None,
                conversation_commit: None,
                edge_policy: None,
                context_assembly_trace: None,
                routing_domain_hint: Some("code".to_string()),
                entity_learn_skipped_no_domain: false,
                round: None,
                tool_calls_returned: None,
                offset_ms: None,
                llm_rounds: None,
                total_llm_ms: None,
                total_tool_ms: None,
                parent_event_id: None,
                git_head: None,
                git_branch: None,
            })
            .unwrap();

        let body = execute_self_command(
            &SelfCmd::Health(SelfSessionArgs {
                session_id: Some(session_id.to_string()),
            }),
            None,
        )
        .await
        .unwrap();

        let value: serde_json::Value = serde_json::from_str(&body).unwrap();
        let risk_flags = value["risk_flags"].as_array().unwrap();
        assert!(risk_flags.iter().any(|flag| flag == "high_token_pressure"));
        assert!(risk_flags.iter().any(|flag| flag == "recent_tool_failures"));
        assert_eq!(value["recent_failures"][0]["tool"], "bash");
        assert_eq!(value["acceptance_ok"], true);
    }

    #[tokio::test]
    async fn verify_surface_reports_acceptance_gaps() {
        let temp = tempfile::tempdir().unwrap();
        let _guard = JournalDirGuard::new(temp.path());
        let session_id = "self-verify-session";
        let ws = WorkspaceMetadata::with_context(session_id, "gpt-5.4", "/repo", Some("main"));
        session_workspace::write_workspace(&ws).unwrap();

        let body = execute_self_command(
            &SelfCmd::Verify(SelfSessionArgs {
                session_id: Some(session_id.to_string()),
            }),
            None,
        )
        .await
        .unwrap();

        let value: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(value["ok"], false);
        let checks = value["checks"].as_array().unwrap();
        assert!(
            checks
                .iter()
                .any(|check| { check["name"] == "journal_present" && check["ok"] == false })
        );
        assert!(checks.iter().any(|check| {
            check["name"] == "steps_present_when_journal_present" && check["ok"] == true
        }));
    }

    #[serial_test::serial]
    #[tokio::test]
    async fn snapshot_uses_cloud_restore_when_local_state_missing() {
        let temp = tempfile::tempdir().unwrap();
        let _guard = JournalDirGuard::new(temp.path());
        let _creds_guard = crate::tests::isolate_credentials();
        let server = MockServer::start().await;
        let _api_url = EnvGuard::set("ASTRA_API_URL", &server.uri());
        let _token = EnvGuard::set("ASTRA_ACCESS_TOKEN", "test-token");
        let session_id = "11111111-1111-1111-1111-111111111111";

        let mut workspace =
            WorkspaceMetadata::with_context(session_id, "gpt-5.4", "/srv/cloud-repo", Some("main"));
        workspace.discovered_skills = vec!["session-recovery".to_string()];
        let restored = astra_services::session_restore::RestoredSession {
            session_id: session_id.to_string(),
            turn_count: 3,
            total_tokens_in: 120,
            total_tokens_out: 45,
            last_status: "active".to_string(),
            restored_from_cloud: true,
            workspace: Some(workspace),
            conversation_messages: vec![
                serde_json::json!({"role":"user","content":"continue"}),
                serde_json::json!({"role":"assistant","content":"restored from cloud"}),
            ],
            ..Default::default()
        };
        mock_cloud_resume(&server, session_id, &restored).await;

        let body = execute_self_command(
            &SelfCmd::Snapshot(SelfSessionArgs {
                session_id: Some(session_id.to_string()),
            }),
            None,
        )
        .await
        .unwrap();

        let value: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(value["run"]["session_id"], session_id);
        assert_eq!(value["run"]["turn_count"], 3);
        assert!(value["run"]["goal"].is_null());
        assert_eq!(value["environment"]["cwd"], "/srv/cloud-repo");
        assert_eq!(value["environment"]["model"], "gpt-5.4");
        assert_eq!(
            value["environment"]["discovered_skills"][0],
            "session-recovery"
        );
    }

    #[serial_test::serial]
    #[tokio::test]
    async fn snapshot_merges_local_and_restored_persistence_errors() {
        let temp = tempfile::tempdir().unwrap();
        let _guard = JournalDirGuard::new(temp.path());
        let _creds_guard = crate::tests::isolate_credentials();
        let server = MockServer::start().await;
        let _api_url = EnvGuard::set("ASTRA_API_URL", &server.uri());
        let _token = EnvGuard::set("ASTRA_ACCESS_TOKEN", "test-token");
        let session_id = "66666666-6666-6666-6666-666666666666";

        let mut local_workspace =
            WorkspaceMetadata::with_context(session_id, "gpt-5.4", "/repo", Some("main"));
        local_workspace.last_persistence_error = Some("failed to append turn event".to_string());
        session_workspace::write_workspace(&local_workspace).unwrap();

        let mut restored_workspace =
            WorkspaceMetadata::with_context(session_id, "gpt-5.4", "/srv/cloud-repo", Some("main"));
        restored_workspace.last_persistence_error =
            Some("failed to write workspace metadata".to_string());
        let restored = astra_services::session_restore::RestoredSession {
            session_id: session_id.to_string(),
            turn_count: 3,
            total_tokens_in: 120,
            total_tokens_out: 45,
            last_status: "active".to_string(),
            restored_from_cloud: true,
            workspace: Some(restored_workspace),
            conversation_messages: vec![
                serde_json::json!({"role":"user","content":"continue"}),
                serde_json::json!({"role":"assistant","content":"restored from cloud"}),
            ],
            ..Default::default()
        };
        mock_cloud_resume(&server, session_id, &restored).await;

        let body = execute_self_command(
            &SelfCmd::Snapshot(SelfSessionArgs {
                session_id: Some(session_id.to_string()),
            }),
            None,
        )
        .await
        .unwrap();

        let value: serde_json::Value = serde_json::from_str(&body).unwrap();
        let persistence_error = value["run"]["persistence_error"]
            .as_str()
            .expect("snapshot should surface merged persistence error");
        assert!(persistence_error.contains("failed to append turn event"));
        assert!(
            persistence_error.contains("restored snapshot: failed to write workspace metadata")
        );
    }

    #[serial_test::serial]
    #[tokio::test]
    async fn reflect_uses_cloud_conversation_when_local_journal_missing() {
        let temp = tempfile::tempdir().unwrap();
        let _guard = JournalDirGuard::new(temp.path());
        let _creds_guard = crate::tests::isolate_credentials();
        let server = MockServer::start().await;
        let _api_url = EnvGuard::set("ASTRA_API_URL", &server.uri());
        let _token = EnvGuard::set("ASTRA_ACCESS_TOKEN", "test-token");
        let session_id = "22222222-2222-2222-2222-222222222222";

        let restored = astra_services::session_restore::RestoredSession {
            session_id: session_id.to_string(),
            turn_count: 2,
            total_tokens_in: 88,
            total_tokens_out: 34,
            last_status: "active".to_string(),
            restored_from_cloud: true,
            conversation_messages: vec![
                serde_json::json!({"role":"user","content":"check history"}),
                serde_json::json!({"role":"assistant","content":"history restored"}),
            ],
            ..Default::default()
        };
        mock_cloud_resume(&server, session_id, &restored).await;

        let body = execute_self_command(
            &SelfCmd::Reflect(SelfReflectArgs {
                session_id: Some(session_id.to_string()),
                topic: "execution".to_string(),
                facet: Some("trace".to_string()),
                question: None,
                last_n: 4,
            }),
            None,
        )
        .await
        .unwrap();

        let value: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(value["session_id"], session_id);
        assert!(value.get("reflection_context").is_none());
        assert!(value.get("recent_turns").is_none());
        assert_eq!(value["data_coverage"]["events"], 1);
        assert_eq!(value["data_coverage"]["source"], "local_session_artifacts");
        let evidence = value["evidence"].as_array().unwrap();
        assert_eq!(evidence.len(), 1);
        assert_eq!(evidence[0]["source"], "cloud_resume");
        assert!(
            evidence[0]["summary"]
                .as_str()
                .is_some_and(|summary| summary.contains("check history")),
            "{value}"
        );
        assert_eq!(
            value["data_coverage"]["providers"]["cloud_resume"]["status"],
            "fresh"
        );
        assert!(
            evidence[0]["ref_id"]
                .as_str()
                .is_some_and(|ref_id| ref_id.starts_with("urn:astra:event:cloud:")),
            "{value}"
        );
        assert_eq!(
            value["graph_slice"]["nodes"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|node| node["layer"] == "runtime")
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn reflect_marks_local_journal_events_with_local_provenance() {
        let session_id = "reflect-local-provenance-session";
        let mut event = astra_services::session_journal::JournalEvent::turn(
            Some(session_id),
            1,
            Some("gpt-5.4"),
            "inspect local",
            "local response",
            1,
            100,
            25,
            300,
        );
        event.tools_used = Some(vec!["read_file".to_string()]);
        let artifacts = LoadedSelfSurfaceArtifacts {
            session_id: session_id.to_string(),
            workspace: None,
            restored: None,
            journal_events: vec![event],
            latest_full_context_trace: None,
        };
        let request = ReflectRequest::from_observation_params(
            Some("execution"),
            Some("trace"),
            None,
            None,
            4,
            "",
        );

        let response = build_reflect_response(&artifacts, 4, request).await;

        assert_eq!(
            response.data_coverage.providers["local_journal"].status,
            "fresh"
        );
        assert_eq!(response.evidence.len(), 1);
        assert_eq!(response.evidence[0].source, "local_journal");
        assert!(
            response.evidence[0]
                .ref_id
                .starts_with("urn:astra:event:local:"),
            "{:?}",
            response.evidence[0]
        );
        assert_eq!(response.observations.len(), 1);
    }

    #[tokio::test]
    async fn reflect_reports_failed_turn_evaluation_as_adverse_evidence() {
        let session_id = "reflect-failed-evaluation-session";
        let normal_turn = JournalEvent::turn(
            Some(session_id),
            1,
            Some("gpt-5.4"),
            "double check",
            "reviewed",
            1,
            100,
            25,
            300,
        );
        let failed_evaluation = JournalEvent::turn_evaluation(
            Some(session_id),
            Some(1),
            "cli_repl",
            false,
            false,
            0.21,
            0.37,
            0.05,
            0,
            false,
            23,
            vec![serde_json::json!({
                "kind": "llm_round_churn",
                "message": "Detected 13 LLM rounds with low evidence yield"
            })],
        );
        let artifacts = LoadedSelfSurfaceArtifacts {
            session_id: session_id.to_string(),
            workspace: None,
            restored: None,
            journal_events: vec![normal_turn, failed_evaluation],
            latest_full_context_trace: None,
        };
        let request = ReflectRequest::from_observation_params(
            Some("execution"),
            Some("trace"),
            None,
            None,
            4,
            "",
        );

        let response = build_reflect_response(&artifacts, 4, request).await;

        assert_eq!(response.observations[0].severity, "warning");
        assert!(response.summary.contains("adverse or degraded"));
        assert!(!response.summary.contains("no local errors detected"));
        assert!(response.evidence.iter().any(|evidence| {
            evidence
                .summary
                .contains("Detected 13 LLM rounds with low evidence yield")
        }));
    }

    #[tokio::test]
    async fn standalone_error_reflect_includes_canonical_tool_failure() {
        let session_id = "reflect-tool-error-session";
        let tool_error = JournalEvent::tool_call_error(
            Some(session_id),
            5,
            "memory",
            "tool 'memory' failed: missing non-empty required field `reason`",
            astra_services::session_journal::ToolCallRecord {
                name: "memory".to_string(),
                ok: false,
                error_kind: Some(astra_core::ErrorKind::ToolInvalidArgs),
                ..Default::default()
            },
        );
        let unrelated = JournalEvent::session_memory_extraction(
            Some(session_id),
            5,
            1,
            astra_services::session_journal::SessionMemoryExtractionOutcome::Skipped {
                reason:
                    astra_services::session_journal::SessionMemoryExtractionSkipReason::InFlight,
            },
            &astra_services::session_journal::SessionMemoryExtractionBreadcrumbs::default(),
        );
        let artifacts = LoadedSelfSurfaceArtifacts {
            session_id: session_id.to_string(),
            workspace: None,
            restored: None,
            journal_events: vec![unrelated, tool_error],
            latest_full_context_trace: None,
        };
        let request = ReflectRequest::from_observation_params(
            None,
            Some("errors"),
            None,
            None,
            8,
            "why did the operation fail?",
        );

        let response = build_reflect_response(&artifacts, 8, request).await;

        let projected_event_labels = response
            .graph_slice
            .nodes
            .iter()
            .filter(|node| node.layer == astra_core::ObservationGraphLayer::Runtime)
            .map(|node| node.label.as_str())
            .collect::<Vec<_>>();
        assert_eq!(projected_event_labels, vec!["tool_call_error"]);
        assert_eq!(response.evidence.len(), 1);
        assert_eq!(response.evidence[0].source, "local_journal");
    }

    #[test]
    fn error_view_filters_non_adverse_events_before_applying_its_limit() {
        let session_id = "reflect-error-window";
        let mut events = vec![JournalEvent::tool_call_error(
            Some(session_id),
            1,
            "memory",
            "invalid arguments",
            astra_services::session_journal::ToolCallRecord {
                name: "memory".to_string(),
                ok: false,
                error_kind: Some(astra_core::ErrorKind::ToolInvalidArgs),
                ..Default::default()
            },
        )];
        for turn in 2..20 {
            events.push(JournalEvent::turn_guard_verdict(
                Some(session_id),
                turn,
                "info",
                &[],
                &[],
                &[],
                false,
                0,
                0,
                0,
                &[],
                0,
                0,
            ));
        }

        let previews = analysis_view_recent_event_previews(&events, 1, "execution_errors");

        assert_eq!(previews.len(), 1);
        assert_eq!(previews[0].event_type, "tool_call_error");
    }

    #[tokio::test]
    async fn reflect_response_includes_persistence_warning_from_workspace() {
        let session_id = "reflect-persistence-session";
        let mut workspace =
            WorkspaceMetadata::with_context(session_id, "gpt-5.4", "/repo", Some("main"));
        workspace.last_persistence_error = Some("failed to append turn event".to_string());
        let artifacts = LoadedSelfSurfaceArtifacts {
            session_id: session_id.to_string(),
            workspace: Some(workspace),
            restored: None,
            journal_events: Vec::new(),
            latest_full_context_trace: None,
        };

        let request = ReflectRequest::from_observation_params(
            Some("execution"),
            Some("trace"),
            None,
            None,
            4,
            "",
        );
        let response = build_reflect_response(&artifacts, 4, request).await;

        assert!(
            response
                .data_coverage
                .warnings
                .iter()
                .any(|warning| warning
                    == "session persistence degraded: failed to append turn event"),
            "{:?}",
            response.data_coverage.warnings
        );
        assert_eq!(response.data_coverage.overall, "partial");
    }

    #[serial_test::serial]
    #[tokio::test]
    async fn snapshot_without_session_id_replaces_stale_pointer_with_cloud_resumable_session() {
        let temp = tempfile::tempdir().unwrap();
        let _guard = JournalDirGuard::new(temp.path());
        let _creds_guard = crate::tests::isolate_credentials();
        let server = MockServer::start().await;
        let _api_url = EnvGuard::set("ASTRA_API_URL", &server.uri());
        let stale_session_id = "33333333-3333-3333-3333-333333333333";
        let live_session_id = "44444444-4444-4444-4444-444444444444";

        let mut creds = CredentialsFile::default();
        creds.profiles.insert(
            "default".to_string(),
            Profile {
                access_token: Some("test-token".to_string()),
                last_session_id: Some(stale_session_id.to_string()),
                ..Default::default()
            },
        );
        save_credentials(&creds).unwrap();

        Mock::given(method("GET"))
            .and(path(format!("/sessions/{stale_session_id}")))
            .respond_with(ResponseTemplate::new(404).set_body_json(serde_json::json!({
                "detail": "missing"
            })))
            .mount(&server)
            .await;

        let workspace = WorkspaceMetadata::with_context(
            live_session_id,
            "gpt-5.4",
            "/srv/cloud-picked",
            Some("main"),
        );
        let listed = astra_services::session_restore::RestoredSession {
            session_id: live_session_id.to_string(),
            turn_count: 6,
            total_tokens_in: 210,
            total_tokens_out: 90,
            last_status: "active".to_string(),
            restored_from_cloud: true,
            workspace: Some(workspace.clone()),
            ..Default::default()
        };
        mock_cloud_resumable_list(&server, std::slice::from_ref(&listed)).await;

        let restored = astra_services::session_restore::RestoredSession {
            conversation_messages: vec![
                serde_json::json!({"role":"user","content":"status"}),
                serde_json::json!({"role":"assistant","content":"picked from cloud list"}),
            ],
            ..listed.clone()
        };
        mock_cloud_resume(&server, live_session_id, &restored).await;

        let body = execute_self_command(
            &SelfCmd::Snapshot(SelfSessionArgs { session_id: None }),
            None,
        )
        .await
        .unwrap();

        let value: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(value["run"]["session_id"], live_session_id);
        assert_eq!(value["run"]["turn_count"], 6);
        assert_eq!(value["environment"]["cwd"], "/srv/cloud-picked");
        assert_eq!(
            load_credentials()
                .profiles
                .get("default")
                .and_then(|profile| profile.last_session_id.as_deref()),
            Some(live_session_id)
        );
    }

    #[serial_test::serial]
    #[tokio::test]
    async fn snapshot_without_session_id_ignores_stale_local_pointer() {
        let temp = tempfile::tempdir().unwrap();
        let _guard = JournalDirGuard::new(temp.path());
        let _creds_guard = crate::tests::isolate_credentials();
        let _api_url = EnvGuard::set("ASTRA_API_URL", "");
        let _token = EnvGuard::set("ASTRA_ACCESS_TOKEN", "");
        let stale_session_id = "77777777-7777-7777-7777-777777777777";

        let mut creds = CredentialsFile::default();
        creds.profiles.insert(
            "default".to_string(),
            Profile {
                last_session_id: Some(stale_session_id.to_string()),
                ..Default::default()
            },
        );
        save_credentials(&creds).unwrap();

        let error = execute_self_command(
            &SelfCmd::Snapshot(SelfSessionArgs { session_id: None }),
            None,
        )
        .await
        .expect_err("stale local pointer should not resolve to a resumable session");

        assert!(error.contains("no resumable session found"), "{error}");
    }

    #[serial_test::serial]
    #[tokio::test]
    async fn snapshot_without_session_id_ignores_stale_remote_pointer_when_cloud_is_configured_but_unauthenticated()
     {
        let temp = tempfile::tempdir().unwrap();
        let _guard = JournalDirGuard::new(temp.path());
        let _creds_guard = crate::tests::isolate_credentials();
        let server = MockServer::start().await;
        let _api_url = EnvGuard::set("ASTRA_API_URL", &server.uri());
        let _token = EnvGuard::set("ASTRA_ACCESS_TOKEN", "");
        let stale_session_id = "88888888-8888-8888-8888-888888888888";

        let mut creds = CredentialsFile::default();
        creds.profiles.insert(
            "default".to_string(),
            Profile {
                last_session_id: Some(stale_session_id.to_string()),
                ..Default::default()
            },
        );
        save_credentials(&creds).unwrap();

        let error = execute_self_command(
            &SelfCmd::Snapshot(SelfSessionArgs { session_id: None }),
            None,
        )
        .await
        .expect_err("unauthenticated cloud pointer should not resolve to a resumable session");

        assert!(error.contains("no resumable session found"), "{error}");
        assert_eq!(
            load_credentials()
                .profiles
                .get("default")
                .and_then(|profile| profile.last_session_id.as_deref()),
            Some(stale_session_id),
            "missing auth should not clear the stored pointer"
        );
    }

    #[tokio::test]
    async fn snapshot_surfaces_session_journal_io_error() {
        let temp = tempfile::tempdir().unwrap();
        let _guard = JournalDirGuard::new(temp.path());
        let session_id = "55555555-5555-5555-5555-555555555555";
        let journal_path = astra_services::session_journal::journal_file_path(session_id);
        std::fs::create_dir_all(journal_path).unwrap();

        let error = execute_self_command(
            &SelfCmd::Snapshot(SelfSessionArgs {
                session_id: Some(session_id.to_string()),
            }),
            None,
        )
        .await
        .expect_err("journal io error should surface");

        assert!(error.contains("failed to read session journal"));
    }
}
