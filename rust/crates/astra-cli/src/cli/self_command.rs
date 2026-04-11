use std::collections::BTreeSet;
use std::time::{Duration, SystemTime};

use chrono::{DateTime, Utc};
use serde::Serialize;

use crate::cli_args::{
    SelfCmd, SelfJournalArgs, SelfMutateCmd, SelfMutateConfigArgs, SelfMutateGoalArgs,
};
use crate::cli_utils::resumable_last_session_id;
use astra_runtime::auto_tuning::{FeedbackSignal, SignalType};
use astra_runtime::runtime_config::RuntimeConfig;
use astra_runtime::self_model::{ConstraintSet, SelfModel};
use astra_runtime::tool_registry::{TOOL_CATALOG, ToolRegistry};
use astra_runtime::turn::context_assembly_trace::TokenBudgetTrace;
use astra_runtime::turn::tool_health::ToolHealthTracker;
use astra_runtime::user_profile::Scenario;
use astra_services::session_journal::{self, JournalEvent, JournalEventType};
use astra_services::session_restore::{
    HybridRestoreService, RestoredSession, SessionRestoreService,
};
use astra_services::session_workspace::{self, ContextTraceSignal, WorkspaceMetadata};

#[derive(Debug, Clone)]
struct SessionArtifacts {
    session_id: String,
    workspace: Option<WorkspaceMetadata>,
    restored: Option<RestoredSession>,
    journal_events: Vec<JournalEvent>,
    latest_full_context_trace: Option<serde_json::Value>,
}

#[derive(Debug, Serialize)]
struct IdentityView {
    name: &'static str,
    version: &'static str,
    runtime: &'static str,
}

#[derive(Debug, Serialize)]
struct SessionView {
    session_id: String,
    cwd: Option<String>,
    git_branch: Option<String>,
    git_head: Option<String>,
    model: Option<String>,
    status: Option<String>,
    created_at: Option<String>,
    updated_at: Option<String>,
    resolved_sources: Vec<&'static str>,
}

#[derive(Debug, Serialize)]
struct AdaptiveView {
    last_scenario_change_turn: Option<u32>,
    last_token_budget_direction: i8,
    last_token_budget_change_turn: Option<u32>,
    active_experiment_id: Option<String>,
    active_variant: Option<String>,
    tuned_config_present: bool,
}

#[derive(Debug, Serialize)]
struct JournalSummary {
    total_events: usize,
    last_event_type: Option<String>,
    recent_event_types: Vec<String>,
    failure_event_count: usize,
    recent_failures: Vec<ToolFailureView>,
}

#[derive(Debug, Serialize)]
struct SnapshotResponse {
    identity: IdentityView,
    session: SessionView,
    self_model: SelfModel,
    adaptive: AdaptiveView,
    trace: TraceResponse,
    journal: JournalSummary,
}

#[derive(Debug, Serialize)]
struct ProfileResponse {
    session_id: String,
    identity: IdentityView,
    self_model: SelfModel,
}

#[derive(Debug, Serialize)]
struct GoalResponse {
    session_id: String,
    session_goal: Option<String>,
    plan_goal: Option<String>,
    plan_execution_rounds: usize,
    plan_corrections: Vec<String>,
    recent_goal_events: Vec<EventPreview>,
}

#[derive(Debug, Serialize)]
struct BudgetResponse {
    session_id: String,
    token_budget: Option<astra_runtime::self_model::TokenBudgetSnapshot>,
    tool_budget_tokens: u32,
    compression_threshold: f64,
    max_turn_input_tokens: u32,
    compression_threshold_min: f64,
    compression_threshold_max: f64,
}

#[derive(Debug, Serialize)]
struct SignalsResponse {
    session_id: String,
    recent_signals: Vec<astra_runtime::self_model::SignalSummary>,
    recent_events: Vec<EventPreview>,
}

#[derive(Debug, Serialize)]
struct TraceResponse {
    session_id: String,
    compact_trace: Option<ContextTraceSignal>,
    compact_preview: Option<String>,
    latest_selection_trace: Option<serde_json::Value>,
    latest_full_context_trace: Option<serde_json::Value>,
}

#[derive(Debug, Serialize)]
struct HealthResponse {
    session_id: String,
    blocked_tools: Vec<String>,
    tool_health: Vec<ToolHealthView>,
    recent_failures: Vec<ToolFailureView>,
}

#[derive(Debug, Serialize)]
struct ToolHealthView {
    name: String,
    total_calls: usize,
    total_failures: usize,
    success_rate: f64,
    deprioritized: bool,
    consecutive_failures: usize,
    rehabilitation_count: usize,
}

#[derive(Debug, Serialize)]
struct ToolFailureView {
    ts: String,
    tool: String,
    error: Option<String>,
    turn: Option<u32>,
}

#[derive(Debug, Serialize)]
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
struct VerifyResponse {
    session_id: String,
    ok: bool,
    checks: Vec<CheckResult>,
}

#[derive(Debug, Serialize)]
struct CheckResult {
    name: String,
    ok: bool,
    detail: String,
}

#[derive(Debug, Serialize)]
struct MutatePreviewResponse {
    session_id: String,
    path: String,
    old_value: serde_json::Value,
    new_value: serde_json::Value,
    valid: bool,
    effective_config_changed: bool,
    would_clear_override: bool,
    checks: Vec<CheckResult>,
}

#[derive(Debug, Serialize)]
struct GoalMutationResponse {
    session_id: String,
    old_goal: Option<String>,
    new_goal: String,
    persisted: bool,
}

pub(crate) async fn execute_self_command(
    cmd: &SelfCmd,
    profile: Option<&str>,
) -> Result<String, String> {
    match cmd {
        SelfCmd::Snapshot(args) => {
            render_surface_for_session(
                &resolve_session_id(args.session_id.as_deref(), profile)?,
                "snapshot",
                20,
            )
            .await
        }
        SelfCmd::Profile(args) => {
            render_surface_for_session(
                &resolve_session_id(args.session_id.as_deref(), profile)?,
                "profile",
                20,
            )
            .await
        }
        SelfCmd::Goal(args) => {
            render_surface_for_session(
                &resolve_session_id(args.session_id.as_deref(), profile)?,
                "goal",
                20,
            )
            .await
        }
        SelfCmd::Trace(args) => {
            render_surface_for_session(
                &resolve_session_id(args.session_id.as_deref(), profile)?,
                "trace",
                20,
            )
            .await
        }
        SelfCmd::Budget(args) => {
            render_surface_for_session(
                &resolve_session_id(args.session_id.as_deref(), profile)?,
                "budget",
                20,
            )
            .await
        }
        SelfCmd::Signals(args) => {
            render_surface_for_session(
                &resolve_session_id(args.session_id.as_deref(), profile)?,
                "signals",
                20,
            )
            .await
        }
        SelfCmd::Health(args) => {
            render_surface_for_session(
                &resolve_session_id(args.session_id.as_deref(), profile)?,
                "health",
                20,
            )
            .await
        }
        SelfCmd::Journal(SelfJournalArgs { session_id, limit }) => {
            render_surface_for_session(
                &resolve_session_id(session_id.as_deref(), profile)?,
                "journal",
                *limit,
            )
            .await
        }
        SelfCmd::Verify(args) => {
            render_surface_for_session(
                &resolve_session_id(args.session_id.as_deref(), profile)?,
                "verify",
                20,
            )
            .await
        }
        SelfCmd::Mutate(SelfMutateCmd::Preview(args)) => {
            let session_id = resolve_session_id(args.session_id.as_deref(), profile)?;
            to_json(&preview_config_mutation(&session_id, args)?)
        }
        SelfCmd::Mutate(SelfMutateCmd::Apply(args)) => {
            let session_id = resolve_session_id(args.session_id.as_deref(), profile)?;
            let preview = preview_config_mutation(&session_id, args)?;
            persist_config_mutation(&session_id, args, &preview)?;
            to_json(&preview)
        }
        SelfCmd::Mutate(SelfMutateCmd::Goal(SelfMutateGoalArgs { session_id, text })) => {
            let session_id = resolve_session_id(session_id.as_deref(), profile)?;
            to_json(&persist_goal_mutation(&session_id, text)?)
        }
    }
}

pub(crate) async fn render_surface_for_session(
    session_id: &str,
    surface: &str,
    journal_limit: usize,
) -> Result<String, String> {
    let artifacts = load_artifacts(session_id.to_string()).await?;
    render_surface_from_artifacts(surface, &artifacts, journal_limit)
}

pub(crate) fn agent_info_surface_alias(dimension: &str) -> Option<&'static str> {
    match dimension {
        "snapshot" => Some("snapshot"),
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

fn render_surface_from_artifacts(
    surface: &str,
    artifacts: &SessionArtifacts,
    journal_limit: usize,
) -> Result<String, String> {
    match surface {
        "snapshot" => to_json(&build_snapshot_response(artifacts)?),
        "profile" => to_json(&ProfileResponse {
            session_id: artifacts.session_id.clone(),
            identity: identity_view(),
            self_model: build_self_model(artifacts)?,
        }),
        "goal" => to_json(&build_goal_response(artifacts)),
        "trace" => to_json(&build_trace_response(artifacts)),
        "budget" => {
            let model = build_self_model(artifacts)?;
            let config = effective_runtime_config(artifacts.workspace.as_ref())?;
            to_json(&BudgetResponse {
                session_id: artifacts.session_id.clone(),
                token_budget: model.state.token_budget,
                tool_budget_tokens: config.tool_selection.tool_budget_tokens,
                compression_threshold: config.compression.compression_threshold,
                max_turn_input_tokens: config.token_budget.max_turn_input_tokens,
                compression_threshold_min: config.context_window.compression_threshold_min,
                compression_threshold_max: config.context_window.compression_threshold_max,
            })
        }
        "signals" => {
            let model = build_self_model(artifacts)?;
            to_json(&SignalsResponse {
                session_id: artifacts.session_id.clone(),
                recent_signals: model.recent_signals,
                recent_events: recent_event_previews(
                    &artifacts.journal_events,
                    12,
                    &[
                        JournalEventType::DriftDetected,
                        JournalEventType::StallDetected,
                        JournalEventType::AdaptiveScenarioApplied,
                        JournalEventType::AdaptivePerTurnApplied,
                        JournalEventType::AdaptiveExperimentEnrolled,
                        JournalEventType::AdaptiveTuningRuleTriggered,
                        JournalEventType::TurnError,
                        JournalEventType::VerificationCompleted,
                    ],
                ),
            })
        }
        "health" => to_json(&build_health_response(artifacts)),
        "journal" => {
            let events = artifacts
                .journal_events
                .iter()
                .rev()
                .take(journal_limit)
                .map(event_preview)
                .collect::<Vec<_>>();
            to_json(&serde_json::json!({
                "session_id": artifacts.session_id,
                "total_events": artifacts.journal_events.len(),
                "returned": events.len(),
                "events": events,
            }))
        }
        "verify" => to_json(&verify_artifacts(artifacts)),
        "identity" => to_json(&identity_view()),
        other => Err(format!("unsupported self surface '{other}'")),
    }
}

fn to_json<T: Serialize>(value: &T) -> Result<String, String> {
    serde_json::to_string(value).map_err(|e| e.to_string())
}

fn identity_view() -> IdentityView {
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
                let ws_path = session_workspace::workspace_dir_for(q).join("workspace.yaml");
                if ws_path.exists() {
                    Ok(q.to_string())
                } else {
                    Err(std::io::Error::new(
                        std::io::ErrorKind::NotFound,
                        format!("no session journal or workspace matches '{q}'"),
                    ))
                }
            })
            .map_err(|e| e.to_string()),
        None => resumable_last_session_id(profile)
            .ok_or_else(|| "no resumable session found; pass a session id explicitly".to_string()),
    }
}

async fn load_artifacts(session_id: String) -> Result<SessionArtifacts, String> {
    let workspace = session_workspace::read_workspace(&session_id).ok();
    let journal_events = session_journal::read_journal(&session_id).unwrap_or_default();
    let restore_service = HybridRestoreService::local_only();
    let restored = restore_service.restore_session(&session_id).await?;
    if workspace.is_none() && restored.is_none() && journal_events.is_empty() {
        return Err(format!(
            "no persistent local state found for session {session_id}"
        ));
    }
    let latest_full_context_trace = journal_events
        .iter()
        .rev()
        .find_map(|event| event.context_assembly_trace.clone());
    Ok(SessionArtifacts {
        session_id,
        workspace,
        restored,
        journal_events,
        latest_full_context_trace,
    })
}

fn build_snapshot_response(artifacts: &SessionArtifacts) -> Result<SnapshotResponse, String> {
    let workspace = artifacts.workspace.as_ref();
    let mut resolved_sources = Vec::new();
    if workspace.is_some() {
        resolved_sources.push("workspace");
    }
    if artifacts.restored.is_some() {
        resolved_sources.push("restore");
    }
    if !artifacts.journal_events.is_empty() {
        resolved_sources.push("journal");
    }

    Ok(SnapshotResponse {
        identity: identity_view(),
        session: SessionView {
            session_id: artifacts.session_id.clone(),
            cwd: workspace.map(|ws| ws.cwd.clone()),
            git_branch: workspace.and_then(|ws| ws.git_branch.clone()),
            git_head: workspace.and_then(|ws| ws.git_head.clone()),
            model: workspace
                .map(|ws| ws.model.clone())
                .or_else(|| artifacts.restored.as_ref().and_then(|r| r.model.clone())),
            status: workspace.map(|ws| ws.status.clone()),
            created_at: workspace.map(|ws| ws.created_at.clone()),
            updated_at: workspace.map(|ws| ws.updated_at.clone()),
            resolved_sources,
        },
        self_model: build_self_model(artifacts)?,
        adaptive: AdaptiveView {
            last_scenario_change_turn: workspace.and_then(|ws| ws.last_scenario_change_turn),
            last_token_budget_direction: workspace
                .map(|ws| ws.last_token_budget_direction)
                .unwrap_or_default(),
            last_token_budget_change_turn: workspace
                .and_then(|ws| ws.last_token_budget_change_turn),
            active_experiment_id: workspace.and_then(|ws| ws.active_experiment_id.clone()),
            active_variant: workspace.and_then(|ws| ws.active_variant.clone()),
            tuned_config_present: workspace
                .and_then(|ws| ws.tuned_config_json.as_ref())
                .is_some(),
        },
        trace: build_trace_response(artifacts),
        journal: JournalSummary {
            total_events: artifacts.journal_events.len(),
            last_event_type: artifacts
                .journal_events
                .last()
                .map(|event| event_type_name(&event.event_type)),
            recent_event_types: artifacts
                .journal_events
                .iter()
                .rev()
                .take(8)
                .map(|event| event_type_name(&event.event_type))
                .collect(),
            failure_event_count: artifacts
                .journal_events
                .iter()
                .filter(|event| {
                    matches!(
                        event.event_type,
                        JournalEventType::TurnError | JournalEventType::Error
                    )
                })
                .count(),
            recent_failures: recent_tool_failures(&artifacts.journal_events, 8),
        },
    })
}

fn build_self_model(artifacts: &SessionArtifacts) -> Result<SelfModel, String> {
    let workspace = artifacts.workspace.as_ref();
    let restored = artifacts.restored.as_ref();
    let mut blocked_tools = restored
        .map(|r| r.blocked_tools.clone())
        .unwrap_or_default();
    if let Some(ws) = workspace {
        for tool in &ws.deprioritized_tools {
            if !blocked_tools.contains(tool) {
                blocked_tools.push(tool.clone());
            }
        }
        blocked_tools.sort();
    }
    let tracker = build_tool_health_tracker(&artifacts.journal_events, &blocked_tools);
    let skills = merged_skills(workspace);
    let goal_text = workspace
        .and_then(|ws| ws.session_goal.as_deref())
        .or_else(|| workspace.and_then(|ws| ws.plan_goal.as_deref()));
    let scenario = latest_scenario(&artifacts.journal_events);
    let active_experiment = workspace.and_then(|ws| ws.active_experiment_id.as_deref());
    let session_elapsed_secs = workspace
        .and_then(|ws| parse_rfc3339(&ws.created_at))
        .and_then(|created| SystemTime::now().duration_since(created).ok())
        .unwrap_or_default()
        .as_secs();
    let correction_count = workspace.map(|ws| ws.plan_corrections.len()).unwrap_or(0);
    let compression_count = artifacts
        .journal_events
        .iter()
        .filter(|event| event.event_type == JournalEventType::Compact)
        .count();
    let recent_signals = build_feedback_signals(&artifacts.journal_events);
    let effective_config = effective_runtime_config(workspace)?;
    let tool_names = ToolRegistry::all_tool_names();
    let mut pinned_tools = TOOL_CATALOG
        .iter()
        .filter(|tool| tool.pinned)
        .map(|tool| tool.name.to_string())
        .collect::<Vec<_>>();
    if let Some(ws) = workspace {
        for tool in &ws.pinned_tools {
            if !pinned_tools.contains(tool) {
                pinned_tools.push(tool.clone());
            }
        }
        pinned_tools.sort();
    }
    let budget_trace = workspace
        .and_then(|ws| ws.last_context_trace.as_ref())
        .and_then(token_budget_from_trace);

    Ok(SelfModel::snapshot(
        &tool_names,
        &pinned_tools,
        &blocked_tools,
        &skills,
        Some(&tracker),
        restored.map(|r| r.turn_count).unwrap_or_default(),
        budget_trace.as_ref(),
        scenario.as_ref(),
        active_experiment,
        session_elapsed_secs,
        correction_count,
        compression_count,
        goal_text,
        None,
        None,
        &recent_signals,
        &effective_config,
    ))
}

fn build_goal_response(artifacts: &SessionArtifacts) -> GoalResponse {
    let workspace = artifacts.workspace.as_ref();
    GoalResponse {
        session_id: artifacts.session_id.clone(),
        session_goal: workspace.and_then(|ws| ws.session_goal.clone()),
        plan_goal: workspace.and_then(|ws| ws.plan_goal.clone()),
        plan_execution_rounds: workspace
            .map(|ws| ws.plan_execution_rounds)
            .unwrap_or_default(),
        plan_corrections: workspace
            .map(|ws| ws.plan_corrections.clone())
            .unwrap_or_default(),
        recent_goal_events: recent_event_previews(
            &artifacts.journal_events,
            10,
            &[
                JournalEventType::PlanProgress,
                JournalEventType::PlanEdit,
                JournalEventType::PlanLifecycle,
                JournalEventType::VerificationCompleted,
            ],
        ),
    }
}

fn build_trace_response(artifacts: &SessionArtifacts) -> TraceResponse {
    let compact_trace = artifacts
        .workspace
        .as_ref()
        .and_then(|ws| ws.last_context_trace.clone());
    let latest_selection_trace = artifacts
        .journal_events
        .iter()
        .rev()
        .find_map(|event| serde_json::to_value(event.selection_trace.as_ref()?).ok());

    TraceResponse {
        session_id: artifacts.session_id.clone(),
        compact_preview: compact_trace.as_ref().map(ContextTraceSignal::preview),
        compact_trace,
        latest_selection_trace,
        latest_full_context_trace: artifacts.latest_full_context_trace.clone(),
    }
}

fn build_health_response(artifacts: &SessionArtifacts) -> HealthResponse {
    let blocked_tools = artifacts
        .restored
        .as_ref()
        .map(|restored| restored.blocked_tools.clone())
        .unwrap_or_default();
    let tracker = build_tool_health_tracker(&artifacts.journal_events, &blocked_tools);
    let mut tool_health = tracker
        .all()
        .iter()
        .map(|(name, health)| ToolHealthView {
            name: name.clone(),
            total_calls: health.total_calls,
            total_failures: health.total_failures,
            success_rate: health.success_rate(),
            deprioritized: health.deprioritized,
            consecutive_failures: health.consecutive_failures,
            rehabilitation_count: health.rehabilitation_count,
        })
        .collect::<Vec<_>>();
    tool_health.sort_by(|a, b| {
        b.total_calls
            .cmp(&a.total_calls)
            .then_with(|| a.name.cmp(&b.name))
    });

    HealthResponse {
        session_id: artifacts.session_id.clone(),
        blocked_tools,
        tool_health,
        recent_failures: recent_tool_failures(&artifacts.journal_events, 12),
    }
}

fn verify_artifacts(artifacts: &SessionArtifacts) -> VerifyResponse {
    let workspace = artifacts.workspace.as_ref();
    let mut checks = Vec::new();
    checks.push(CheckResult {
        name: "workspace_present".to_string(),
        ok: workspace.is_some(),
        detail: if workspace.is_some() {
            "workspace.yaml loaded".to_string()
        } else {
            "workspace.yaml missing".to_string()
        },
    });
    checks.push(CheckResult {
        name: "journal_present".to_string(),
        ok: !artifacts.journal_events.is_empty(),
        detail: format!("{} journal events", artifacts.journal_events.len()),
    });
    checks.push(CheckResult {
        name: "restore_present".to_string(),
        ok: artifacts.restored.is_some(),
        detail: if artifacts.restored.is_some() {
            "restored session available".to_string()
        } else {
            "restore snapshot unavailable".to_string()
        },
    });

    if let Some(ws) = workspace {
        checks.push(CheckResult {
            name: "workspace_session_match".to_string(),
            ok: ws.session_id == artifacts.session_id,
            detail: format!("workspace session_id={}", ws.session_id),
        });
        checks.extend(verify_runtime_config(ws.tuned_config_json.as_deref()));
        if let Some(trace) = ws.last_context_trace.as_ref() {
            checks.push(verify_trace_budget(trace));
        }
    } else {
        checks.push(CheckResult {
            name: "runtime_config_parse".to_string(),
            ok: true,
            detail: "no workspace override present".to_string(),
        });
    }

    let ok = checks.iter().all(|check| check.ok);
    VerifyResponse {
        session_id: artifacts.session_id.clone(),
        ok,
        checks,
    }
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
    let base_json = serde_json::to_value(&base_config).map_err(|e| e.to_string())?;
    let mut value = base_json.clone();
    let old_value = replace_json_path(&mut value, path, new_value.clone())?;
    let candidate_config: RuntimeConfig = serde_json::from_value(value.clone())
        .map_err(|e| format!("mutation produced invalid RuntimeConfig at '{}': {e}", path))?;
    let candidate_json = serde_json::to_string(&candidate_config).map_err(|e| e.to_string())?;
    let candidate_checks = verify_runtime_config(Some(&candidate_json));
    let baseline_json = serde_json::to_value(RuntimeConfig::load()).map_err(|e| e.to_string())?;
    Ok(MutatePreviewResponse {
        session_id: session_id.to_string(),
        path: path.to_string(),
        old_value,
        new_value,
        valid: candidate_checks.iter().all(|check| check.ok),
        effective_config_changed: value != base_json,
        would_clear_override: value == baseline_json,
        checks: candidate_checks,
    })
}

fn persist_config_mutation(
    session_id: &str,
    args: &SelfMutateConfigArgs,
    preview: &MutatePreviewResponse,
) -> Result<(), String> {
    persist_config_mutation_value(
        session_id,
        &args.path,
        parse_value_arg(&args.value),
        preview,
    )
}

fn persist_config_mutation_value(
    session_id: &str,
    path: &str,
    new_value: serde_json::Value,
    preview: &MutatePreviewResponse,
) -> Result<(), String> {
    let mut ws = session_workspace::read_workspace(session_id).map_err(|e| e.to_string())?;
    let base_config = effective_runtime_config(Some(&ws))?;
    let mut value = serde_json::to_value(&base_config).map_err(|e| e.to_string())?;
    replace_json_path(&mut value, path, new_value)?;
    let candidate_config: RuntimeConfig =
        serde_json::from_value(value.clone()).map_err(|e| e.to_string())?;
    let baseline_json = serde_json::to_value(RuntimeConfig::load()).map_err(|e| e.to_string())?;
    ws.tuned_config_json = if value == baseline_json {
        None
    } else {
        Some(serde_json::to_string(&candidate_config).map_err(|e| e.to_string())?)
    };
    ws.updated_at = Utc::now().to_rfc3339();
    session_workspace::write_workspace(&ws).map_err(|e| e.to_string())?;
    append_config_change_event(
        session_id,
        ws.turn_count,
        path,
        &preview.new_value,
        Some(preview.old_value.clone()),
    )?;
    Ok(())
}

pub(crate) fn persist_config_override(
    session_id: &str,
    path: &str,
    new_value: serde_json::Value,
) -> Result<serde_json::Value, String> {
    let preview = preview_config_mutation_value(session_id, path, new_value.clone())?;
    persist_config_mutation_value(session_id, path, new_value, &preview)?;
    serde_json::to_value(&preview).map_err(|e| e.to_string())
}

fn persist_goal_mutation(session_id: &str, text: &str) -> Result<GoalMutationResponse, String> {
    let mut ws = session_workspace::read_workspace(session_id).map_err(|e| e.to_string())?;
    let old_goal = ws.session_goal.clone();
    ws.session_goal = Some(text.to_string());
    ws.updated_at = Utc::now().to_rfc3339();
    session_workspace::write_workspace(&ws).map_err(|e| e.to_string())?;
    append_config_change_event(
        session_id,
        ws.turn_count,
        "session_goal",
        &serde_json::Value::String(text.to_string()),
        old_goal.clone().map(serde_json::Value::String),
    )?;
    Ok(GoalMutationResponse {
        session_id: session_id.to_string(),
        old_goal,
        new_goal: text.to_string(),
        persisted: true,
    })
}

pub(crate) fn persist_goal_override(
    session_id: &str,
    text: &str,
) -> Result<serde_json::Value, String> {
    serde_json::to_value(persist_goal_mutation(session_id, text)?).map_err(|e| e.to_string())
}

pub(crate) fn persist_tool_preferences(
    session_id: &str,
    pinned_tools: &[String],
    deprioritized_tools: &[String],
) -> Result<(), String> {
    let mut ws = session_workspace::read_workspace(session_id).map_err(|e| e.to_string())?;
    let mut pinned = pinned_tools.to_vec();
    pinned.sort();
    pinned.dedup();
    let mut deprioritized = deprioritized_tools.to_vec();
    deprioritized.sort();
    deprioritized.dedup();

    let old_pinned = ws.pinned_tools.clone();
    let old_deprioritized = ws.deprioritized_tools.clone();
    ws.pinned_tools = pinned.clone();
    ws.deprioritized_tools = deprioritized.clone();
    ws.updated_at = Utc::now().to_rfc3339();
    session_workspace::write_workspace(&ws).map_err(|e| e.to_string())?;

    if old_pinned != pinned {
        append_config_change_event(
            session_id,
            ws.turn_count,
            "pinned_tools",
            &serde_json::json!(pinned),
            Some(serde_json::json!(old_pinned)),
        )?;
    }
    if old_deprioritized != deprioritized {
        append_config_change_event(
            session_id,
            ws.turn_count,
            "deprioritized_tools",
            &serde_json::json!(deprioritized),
            Some(serde_json::json!(old_deprioritized)),
        )?;
    }
    Ok(())
}

pub(crate) fn persist_manual_compression(
    session_id: &str,
    turn: u32,
    reason: &str,
) -> Result<(), String> {
    let writer = session_journal::JournalWriter::new(session_id).map_err(|e| e.to_string())?;
    writer
        .append(&JournalEvent {
            event_type: JournalEventType::Compact,
            ts: Utc::now().to_rfc3339(),
            session_id: Some(session_id.to_string()),
            turn: Some(turn),
            model: None,
            user_input: None,
            assistant_output: None,
            tool_count: None,
            tokens_in: None,
            tokens_out: None,
            duration_ms: None,
            error: None,
            config_key: None,
            config_value: None,
            turns_compacted: Some(1),
            facts_stored: None,
            tools_selected: None,
            selected_skills: None,
            tools_used: None,
            tool_calls: None,
            budget_used: None,
            budget_pressure: None,
            stall_type: None,
            metadata: Some(serde_json::json!({
                "source": "compress_context",
                "reason": reason,
            })),
            plan_subtask_id: None,
            ttft_ms: None,
            context_ms: None,
            selector_strategy: None,
            selector_ms: None,
            selector_tokens_in: None,
            selector_tokens_out: None,
            cache_read_tokens: None,
            cache_creation_tokens: None,
            memoria_ms: None,
            session_lineage: None,
            coordination: None,
            edge_policy: None,
            selection_trace: None,
            context_assembly_trace: None,
            selector_confidence: None,
            routing_domain_hint: None,
            entity_learn_skipped_no_domain: false,
        })
        .map_err(|e| e.to_string())
}

fn append_config_change_event(
    session_id: &str,
    turn: u32,
    key: &str,
    new_value: &serde_json::Value,
    old_value: Option<serde_json::Value>,
) -> Result<(), String> {
    let writer = session_journal::JournalWriter::new(session_id).map_err(|e| e.to_string())?;
    let mut metadata = serde_json::Map::new();
    metadata.insert(
        "source".to_string(),
        serde_json::Value::String("astra self mutate".to_string()),
    );
    if let Some(old_value) = old_value {
        metadata.insert("old_value".to_string(), old_value);
    }
    writer
        .append(&JournalEvent {
            event_type: JournalEventType::ConfigChange,
            ts: Utc::now().to_rfc3339(),
            session_id: Some(session_id.to_string()),
            turn: Some(turn),
            model: None,
            user_input: None,
            assistant_output: None,
            tool_count: None,
            tokens_in: None,
            tokens_out: None,
            duration_ms: None,
            error: None,
            config_key: Some(key.to_string()),
            config_value: Some(new_value.to_string()),
            turns_compacted: None,
            facts_stored: None,
            tools_selected: None,
            selected_skills: None,
            tools_used: None,
            tool_calls: None,
            budget_used: None,
            budget_pressure: None,
            stall_type: None,
            metadata: Some(serde_json::Value::Object(metadata)),
            plan_subtask_id: None,
            ttft_ms: None,
            context_ms: None,
            selector_strategy: None,
            selector_ms: None,
            selector_tokens_in: None,
            selector_tokens_out: None,
            cache_read_tokens: None,
            cache_creation_tokens: None,
            memoria_ms: None,
            session_lineage: None,
            coordination: None,
            edge_policy: None,
            selection_trace: None,
            context_assembly_trace: None,
            selector_confidence: None,
            routing_domain_hint: None,
            entity_learn_skipped_no_domain: false,
        })
        .map_err(|e| e.to_string())
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

fn recent_tool_failures(events: &[JournalEvent], limit: usize) -> Vec<ToolFailureView> {
    let mut failures = Vec::new();
    for event in events.iter().rev() {
        if let Some(tool_calls) = event.tool_calls.as_ref() {
            for call in tool_calls.iter().rev().filter(|call| !call.ok) {
                failures.push(ToolFailureView {
                    ts: event.ts.clone(),
                    tool: call.name.clone(),
                    error: call.error.clone(),
                    turn: event.turn,
                });
                if failures.len() >= limit {
                    return failures;
                }
            }
        }
    }
    failures
}

fn merged_skills(workspace: Option<&WorkspaceMetadata>) -> Vec<String> {
    let mut skills = BTreeSet::new();
    if let Some(ws) = workspace {
        for skill in ws.pinned_skills.iter().chain(ws.discovered_skills.iter()) {
            skills.insert(skill.clone());
        }
    }
    skills.into_iter().collect()
}

fn build_tool_health_tracker(
    events: &[JournalEvent],
    blocked_tools: &[String],
) -> ToolHealthTracker {
    let mut tracker = ToolHealthTracker::new();
    for event in events {
        if let Some(tool_calls) = event.tool_calls.as_ref() {
            for call in tool_calls {
                if call.ok {
                    tracker.record_success(&call.name);
                } else if call
                    .error
                    .as_deref()
                    .map(|err| err.to_ascii_lowercase().contains("timeout"))
                    .unwrap_or(false)
                {
                    tracker.record_timeout(&call.name);
                } else {
                    tracker.record_failure(&call.name);
                }
            }
        }
    }
    for tool in blocked_tools {
        tracker.force_deprioritize(tool);
    }
    tracker
}

fn build_feedback_signals(events: &[JournalEvent]) -> Vec<FeedbackSignal> {
    let mut signals = Vec::new();
    for event in events {
        let timestamp = parse_rfc3339(&event.ts).unwrap_or_else(SystemTime::now);
        match event.event_type {
            JournalEventType::Turn => {
                if event
                    .budget_pressure
                    .is_some_and(|pressure| pressure >= 0.85)
                {
                    signals.push(FeedbackSignal {
                        signal_type: SignalType::HighTokenUsage {
                            tokens: event.budget_used.unwrap_or_default() as u64,
                            threshold: 0,
                        },
                        timestamp,
                        turn_id: event.turn.map(|turn| turn.to_string()),
                        context: Default::default(),
                    });
                } else {
                    signals.push(FeedbackSignal {
                        signal_type: SignalType::TaskSuccess,
                        timestamp,
                        turn_id: event.turn.map(|turn| turn.to_string()),
                        context: Default::default(),
                    });
                }
            }
            JournalEventType::TurnError | JournalEventType::Error => {
                signals.push(FeedbackSignal {
                    signal_type: SignalType::TaskFailure {
                        reason: event
                            .error
                            .clone()
                            .unwrap_or_else(|| event_type_name(&event.event_type)),
                    },
                    timestamp,
                    turn_id: event.turn.map(|turn| turn.to_string()),
                    context: Default::default(),
                });
            }
            JournalEventType::DriftDetected => {
                signals.push(FeedbackSignal {
                    signal_type: SignalType::FocusDrift,
                    timestamp,
                    turn_id: event.turn.map(|turn| turn.to_string()),
                    context: Default::default(),
                });
            }
            JournalEventType::StallDetected => {
                let unique_tools = event
                    .tools_used
                    .as_ref()
                    .map(|tools| tools.iter().collect::<BTreeSet<_>>().len() as u32)
                    .unwrap_or_default();
                signals.push(FeedbackSignal {
                    signal_type: SignalType::ToolChurn {
                        calls: event.tool_count.unwrap_or_default(),
                        unique_tools,
                    },
                    timestamp,
                    turn_id: event.turn.map(|turn| turn.to_string()),
                    context: Default::default(),
                });
            }
            _ => {}
        }
    }
    signals
}

fn latest_scenario(events: &[JournalEvent]) -> Option<Scenario> {
    events.iter().rev().find_map(|event| {
        if event.event_type != JournalEventType::AdaptiveScenarioApplied {
            return None;
        }
        let candidate = event
            .metadata
            .as_ref()
            .and_then(|m| m.get("scenario").or_else(|| m.get("scenario_name")))
            .and_then(serde_json::Value::as_str)?;
        parse_scenario(candidate)
    })
}

fn parse_scenario(input: &str) -> Option<Scenario> {
    match input.trim().to_ascii_lowercase().as_str() {
        "code_review" | "codereview" | "code-review" => Some(Scenario::CodeReview),
        "debugging" | "debug" => Some(Scenario::Debugging),
        "exploration" | "explore" => Some(Scenario::Exploration),
        "planning" | "plan" => Some(Scenario::Planning),
        "implementation" | "implement" => Some(Scenario::Implementation),
        "refactoring" | "refactor" => Some(Scenario::Refactoring),
        "testing" | "test" => Some(Scenario::Testing),
        "documentation" | "docs" => Some(Scenario::Documentation),
        "devops" => Some(Scenario::DevOps),
        "learning" | "learn" => Some(Scenario::Learning),
        _ => None,
    }
}

fn token_budget_from_trace(trace: &ContextTraceSignal) -> Option<TokenBudgetTrace> {
    let budget = trace.budget.as_ref()?;
    Some(TokenBudgetTrace {
        max_tokens: budget.max_tokens,
        system_prompt_tokens: 0,
        history_tokens: 0,
        memory_tokens: 0,
        tool_schema_tokens: 0,
        user_message_tokens: 0,
        total_used: budget.total_used,
        budget_pressure: budget.budget_pressure,
        compression_triggered: budget.compression_triggered,
    })
}

fn effective_runtime_config(
    workspace: Option<&WorkspaceMetadata>,
) -> Result<RuntimeConfig, String> {
    match workspace.and_then(|ws| ws.tuned_config_json.as_deref()) {
        Some(json) => serde_json::from_str(json).map_err(|e| e.to_string()),
        None => Ok(RuntimeConfig::load()),
    }
}

fn verify_runtime_config(tuned_config_json: Option<&str>) -> Vec<CheckResult> {
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

    let verification_ok = config.verification.min_strictness <= config.verification.strictness
        && config.verification.strictness <= config.verification.max_strictness
        && (0.0..=1.0).contains(&config.verification.min_strictness)
        && (0.0..=1.0).contains(&config.verification.strictness)
        && (0.0..=1.0).contains(&config.verification.max_strictness);
    checks.push(CheckResult {
        name: "verification_bounds".to_string(),
        ok: verification_ok,
        detail: format!(
            "min={} strictness={} max={}",
            config.verification.min_strictness,
            config.verification.strictness,
            config.verification.max_strictness
        ),
    });

    let compression_ok = (0.0..=1.0).contains(&config.compression.compression_threshold)
        && (0.0..=1.0).contains(&config.context_window.compression_threshold_min)
        && (0.0..=1.0).contains(&config.context_window.compression_threshold_max)
        && config.context_window.compression_threshold_min
            <= config.context_window.compression_threshold_max;
    checks.push(CheckResult {
        name: "compression_bounds".to_string(),
        ok: compression_ok,
        detail: format!(
            "compression={} window=[{}, {}]",
            config.compression.compression_threshold,
            config.context_window.compression_threshold_min,
            config.context_window.compression_threshold_max
        ),
    });

    let available_tools = ToolRegistry::all_tool_names().len();
    let min_required = ConstraintSet::default().min_tool_pool_size;
    checks.push(CheckResult {
        name: "tool_pool_floor".to_string(),
        ok: available_tools >= min_required,
        detail: format!(
            "available_tools={} min_required={}",
            available_tools, min_required
        ),
    });

    checks
}

fn verify_trace_budget(trace: &ContextTraceSignal) -> CheckResult {
    if let Some(budget) = trace.budget.as_ref() {
        let pressure_ok = if budget.max_tokens == 0 {
            true
        } else if budget.total_used > budget.max_tokens {
            budget.budget_pressure >= 1.0
        } else {
            budget.budget_pressure <= 1.05
        };
        CheckResult {
            name: "trace_budget_coherence".to_string(),
            ok: pressure_ok,
            detail: format!(
                "used={} max={} pressure={}",
                budget.total_used, budget.max_tokens, budget.budget_pressure
            ),
        }
    } else {
        CheckResult {
            name: "trace_budget_coherence".to_string(),
            ok: true,
            detail: "no compact budget trace recorded".to_string(),
        }
    }
}

fn replace_json_path(
    root: &mut serde_json::Value,
    path: &str,
    new_value: serde_json::Value,
) -> Result<serde_json::Value, String> {
    let segments: Vec<&str> = path
        .split('.')
        .filter(|segment| !segment.is_empty())
        .collect();
    if segments.is_empty() {
        return Err("mutation path cannot be empty".to_string());
    }

    let mut current = root;
    for segment in &segments[..segments.len() - 1] {
        current = current
            .get_mut(*segment)
            .ok_or_else(|| format!("unknown config path segment '{segment}'"))?;
    }

    let last = segments.last().expect("checked non-empty");
    let object = current
        .as_object_mut()
        .ok_or_else(|| format!("config path '{}' does not point to an object parent", path))?;
    let slot = object
        .get_mut(*last)
        .ok_or_else(|| format!("unknown config leaf '{}'", last))?;
    let old_value = slot.clone();
    *slot = new_value;
    Ok(old_value)
}

fn parse_value_arg(raw: &str) -> serde_json::Value {
    serde_json::from_str(raw).unwrap_or_else(|_| serde_json::Value::String(raw.to_string()))
}

fn parse_rfc3339(ts: &str) -> Option<SystemTime> {
    let dt = DateTime::parse_from_rfc3339(ts).ok()?;
    let utc = dt.with_timezone(&Utc);
    let secs = utc.timestamp();
    if secs >= 0 {
        Some(SystemTime::UNIX_EPOCH + Duration::from_secs(secs as u64))
    } else {
        SystemTime::UNIX_EPOCH.checked_sub(Duration::from_secs((-secs) as u64))
    }
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
        JournalEventType::PlanProgress => "plan_progress",
        JournalEventType::SessionFork => "session_fork",
        JournalEventType::SyncMarker => "sync_marker",
        JournalEventType::DelegationStarted => "delegation_started",
        JournalEventType::DelegationSubRunStarted => "delegation_sub_run_started",
        JournalEventType::DelegationSubRunCompleted => "delegation_sub_run_completed",
        JournalEventType::DelegationRetry => "delegation_retry",
        JournalEventType::DelegationCompleted => "delegation_completed",
        JournalEventType::AdaptiveBaselinePromoted => "adaptive_baseline_promoted",
        JournalEventType::AgentTerminated => "agent_terminated",
        JournalEventType::VerificationCompleted => "verification_completed",
        JournalEventType::CompositeSnapshot => "composite_snapshot",
        JournalEventType::PlanEdit => "plan_edit",
        JournalEventType::PlanLifecycle => "plan_lifecycle",
        JournalEventType::ContextAssemblyRecorded => "context_assembly_recorded",
        JournalEventType::DriftDetected => "drift_detected",
        JournalEventType::AdaptiveScenarioApplied => "adaptive_scenario_applied",
        JournalEventType::AdaptivePerTurnApplied => "adaptive_per_turn_applied",
        JournalEventType::AdaptiveExperimentEnrolled => "adaptive_experiment_enrolled",
        JournalEventType::AdaptiveTuningRuleTriggered => "adaptive_tuning_rule_triggered",
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli_args::SelfSessionArgs;
    use astra_services::session_journal::{JournalDirGuard, ToolCallRecord};

    #[test]
    fn replace_json_path_updates_existing_leaf() {
        let mut value = serde_json::json!({
            "verification": {
                "strictness": 0.5
            }
        });

        let old = replace_json_path(
            &mut value,
            "verification.strictness",
            serde_json::json!(0.8),
        )
        .unwrap();

        assert_eq!(old, serde_json::json!(0.5));
        assert_eq!(value["verification"]["strictness"], serde_json::json!(0.8));
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

    #[tokio::test]
    async fn snapshot_aggregates_workspace_and_journal_state() {
        let temp = tempfile::tempdir().unwrap();
        let _guard = JournalDirGuard::new(temp.path());
        let session_id = "self-snapshot-session";
        let mut ws = WorkspaceMetadata::with_context(session_id, "gpt-5.4", "/repo", Some("main"));
        ws.session_goal = Some("finish the engine".to_string());
        ws.discovered_skills = vec!["goal-driven-evolution".to_string()];
        ws.active_experiment_id = Some("exp-42".to_string());
        ws.last_context_trace = Some(ContextTraceSignal {
            turn_id: "turn-7".to_string(),
            captured_at: Some(Utc::now().to_rfc3339()),
            tool_selection: None,
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
                turn: Some(7),
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
                tools_selected: Some(vec!["bash".to_string()]),
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
                }]),
                budget_used: Some(7000),
                budget_pressure: Some(0.7),
                stall_type: None,
                metadata: None,
                plan_subtask_id: None,
                ttft_ms: None,
                context_ms: None,
                selector_strategy: Some("tfidf".to_string()),
                selector_ms: None,
                selector_tokens_in: None,
                selector_tokens_out: None,
                cache_read_tokens: None,
                cache_creation_tokens: None,
                memoria_ms: None,
                session_lineage: None,
                coordination: None,
                edge_policy: None,
                selection_trace: None,
                context_assembly_trace: Some(serde_json::json!({"tokens": 7000})),
                selector_confidence: Some(0.8),
                routing_domain_hint: Some("code".to_string()),
                entity_learn_skipped_no_domain: false,
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
        assert_eq!(value["session"]["session_id"], session_id);
        assert_eq!(
            value["self_model"]["goals"]["session_goal"],
            "finish the engine"
        );
        assert_eq!(value["trace"]["compact_trace"]["turn_id"], "turn-7");
        assert_eq!(value["journal"]["total_events"], 1);
    }
}
