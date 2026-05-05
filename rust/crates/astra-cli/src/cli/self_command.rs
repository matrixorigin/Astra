use chrono::Utc;
use serde::Serialize;

use crate::cli_args::{
    SelfCmd, SelfJournalArgs, SelfMutateCmd, SelfMutateConfigArgs, SelfMutateGoalArgs,
    SelfReflectArgs,
};
use crate::cli_utils::resumable_last_session_id;
use astra_config::runtime_config::RuntimeConfig;
use astra_config::user_profile::Scenario;
use astra_runtime::liquid::reflection::{
    GoalSummary as ReflectionGoalSummary, HealthSummary as ReflectionHealthSummary,
    ReflectionEventSummary, VerificationSummary as ReflectionVerificationSummary,
    summarize_recent_adaptation_impacts, summarize_recent_adaptation_verification_impacts,
    summarize_recent_performance_deltas,
};
use astra_runtime::self_model::ConstraintSet;
use astra_runtime::tool_registry::ToolRegistry;
use astra_services::self_surface::{
    EventPreview as SurfaceEventPreview, EvolutionRecord, GoalSurface,
    HealthSurface as SurfaceHealthSurface, LocalSelfSurfaceService, PersistentSelfSnapshot,
    SelfSurfaceDimension, SelfSurfaceResponse, SelfSurfaceService,
    ToolFailureView as SurfaceToolFailureView, ToolHealthView as SurfaceToolHealthView,
    VerificationEventView, VerificationSurface,
};
use astra_services::session_journal::{self, JournalEvent, JournalEventType};
use astra_services::session_restore::{
    HybridRestoreService, RestoredSession, SessionRestoreService,
};
use astra_services::session_workspace::{self, WorkspaceMetadata};

#[path = "self_surface.rs"]
mod self_surface;

#[derive(Debug, Clone)]
struct SessionArtifacts {
    session_id: String,
    workspace: Option<WorkspaceMetadata>,
    restored: Option<RestoredSession>,
    journal_events: Vec<JournalEvent>,
}

#[derive(Debug, Serialize)]
struct IdentityView {
    name: &'static str,
    version: &'static str,
    runtime: &'static str,
}

#[derive(Debug, Serialize)]
struct ReflectResponse {
    session_id: String,
    focus: String,
    question: Option<String>,
    reflection_context: astra_runtime::liquid::reflection::ReflectionContext,
    prompt_preview: String,
    recent_turns: Vec<EventPreview>,
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
        SelfCmd::Reflect(SelfReflectArgs {
            session_id,
            focus,
            question,
            last_n,
        }) => {
            render_reflect_surface_for_session(
                &resolve_session_id(session_id.as_deref(), profile)?,
                *last_n,
                Some(focus.as_str()),
                question.as_deref(),
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
            to_json(&persist_goal_mutation(
                &session_id,
                text,
                "astra self mutate",
            )?)
        }
    }
}

pub(crate) async fn render_surface_for_session(
    session_id: &str,
    surface: &str,
    journal_limit: usize,
) -> Result<String, String> {
    self_surface::render_surface_for_session(session_id, surface, journal_limit).await
}

pub(crate) async fn render_reflect_surface_for_session(
    session_id: &str,
    journal_limit: usize,
    focus: Option<&str>,
    question: Option<&str>,
) -> Result<String, String> {
    let artifacts = load_artifacts(session_id.to_string()).await?;
    to_json(
        &build_reflect_response(
            &artifacts,
            journal_limit.max(1),
            normalize_reflect_focus(focus),
            question
                .map(str::trim)
                .filter(|question| !question.is_empty()),
        )
        .await,
    )
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
    Ok(SessionArtifacts {
        session_id,
        workspace,
        restored,
        journal_events,
    })
}

async fn build_reflect_response(
    artifacts: &SessionArtifacts,
    journal_limit: usize,
    focus: &str,
    question: Option<&str>,
) -> ReflectResponse {
    let (
        goal,
        verification,
        health,
        recent_performance_deltas,
        recent_adaptation_impacts,
        recent_adaptation_verification_impacts,
        recent_evaluation_events,
        recent_adaptations,
        recent_adaptation_outcomes,
    ) = load_reflection_self_evidence(&artifacts.session_id, journal_limit.max(1)).await;
    let context = build_persistent_reflection_context(
        artifacts,
        journal_limit.max(1),
        focus,
        question,
        goal,
        verification,
        health,
        recent_performance_deltas,
        recent_adaptation_impacts,
        recent_adaptation_verification_impacts,
        recent_evaluation_events,
        recent_adaptations,
        recent_adaptation_outcomes,
    );
    let prompt_preview = context.render_prompt_section();
    ReflectResponse {
        session_id: artifacts.session_id.clone(),
        focus: focus.to_string(),
        question: question.map(str::to_string),
        reflection_context: context,
        prompt_preview,
        recent_turns: focused_recent_event_previews(
            &artifacts.journal_events,
            journal_limit,
            focus,
        ),
    }
}

async fn load_reflection_self_evidence(
    session_id: &str,
    journal_limit: usize,
) -> (
    Option<ReflectionGoalSummary>,
    Option<ReflectionVerificationSummary>,
    Option<ReflectionHealthSummary>,
    Vec<ReflectionEventSummary>,
    Vec<ReflectionEventSummary>,
    Vec<ReflectionEventSummary>,
    Vec<ReflectionEventSummary>,
    Vec<ReflectionEventSummary>,
    Vec<ReflectionEventSummary>,
) {
    let service = LocalSelfSurfaceService::new();
    let snapshot = service.snapshot(session_id, journal_limit).await.ok();
    let goal_surface = match service
        .surface(session_id, SelfSurfaceDimension::Goal, journal_limit)
        .await
    {
        Ok(SelfSurfaceResponse::Goal(goal)) => Some(goal),
        _ => None,
    };
    let verification_surface = match service
        .surface(session_id, SelfSurfaceDimension::Verify, journal_limit)
        .await
    {
        Ok(SelfSurfaceResponse::Verify(verification)) => Some(verification),
        _ => None,
    };
    let health_surface = match service
        .surface(session_id, SelfSurfaceDimension::Health, journal_limit)
        .await
    {
        Ok(SelfSurfaceResponse::Health(health)) => Some(health),
        _ => None,
    };
    let recent_performance_deltas = snapshot
        .as_ref()
        .map(|snapshot| summarize_recent_performance_deltas(&snapshot.recent_steps, 4))
        .unwrap_or_default();
    let recent_adaptation_impacts = snapshot
        .as_ref()
        .map(|snapshot| {
            summarize_recent_adaptation_impacts(
                &snapshot.recent_steps,
                &snapshot.evolution.records,
                3,
            )
        })
        .unwrap_or_default();
    let recent_adaptation_verification_impacts = snapshot
        .as_ref()
        .zip(verification_surface.as_ref())
        .map(|(snapshot, verification_surface)| {
            summarize_recent_adaptation_verification_impacts(
                &verification_surface.objective.recent_verifications,
                &snapshot.evolution.records,
                3,
            )
        })
        .unwrap_or_default();
    let recent_evaluation_events =
        reflection_recent_evaluation_events(goal_surface.as_ref(), verification_surface.as_ref());
    let recent_adaptations = reflection_recent_adaptations(snapshot.as_ref());
    let recent_adaptation_outcomes = reflection_recent_adaptation_outcomes(snapshot.as_ref());
    (
        goal_surface.and_then(reflection_goal_summary),
        verification_surface.map(reflection_verification_summary),
        health_surface.and_then(reflection_health_summary),
        recent_performance_deltas,
        recent_adaptation_impacts,
        recent_adaptation_verification_impacts,
        recent_evaluation_events,
        recent_adaptations,
        recent_adaptation_outcomes,
    )
}

fn reflection_goal_summary(goal: GoalSurface) -> Option<ReflectionGoalSummary> {
    Some(ReflectionGoalSummary {
        effective_goal: goal.goal?,
        goal_source: goal.goal_source,
        tracking_status: goal.tracking_status,
        progress_summary: goal.progress.map(|progress| progress.summary),
    })
}

fn reflection_verification_summary(
    verification: VerificationSurface,
) -> ReflectionVerificationSummary {
    ReflectionVerificationSummary {
        ok: verification.ok,
        acceptance_ok: verification.acceptance_ok,
        objective_ok: verification.objective_ok,
        summary: verification.summary,
        pending_blockers: verification.objective.pending_blockers,
        latest_verification: verification
            .objective
            .latest_verification
            .map(|event| event.summary),
    }
}

fn reflection_health_summary(health: SurfaceHealthSurface) -> Option<ReflectionHealthSummary> {
    let risk_flags = health.risk_flags.into_iter().take(4).collect::<Vec<_>>();
    let blocked_tools = health.blocked_tools.into_iter().take(4).collect::<Vec<_>>();
    let hotspots = health
        .tool_hotspots
        .into_iter()
        .take(3)
        .map(reflection_tool_hotspot_summary)
        .collect::<Vec<_>>();
    let recent_failures = health
        .recent_failures
        .into_iter()
        .take(3)
        .map(reflection_tool_failure_summary)
        .collect::<Vec<_>>();
    if risk_flags.is_empty()
        && blocked_tools.is_empty()
        && hotspots.is_empty()
        && recent_failures.is_empty()
    {
        return None;
    }
    Some(ReflectionHealthSummary {
        risk_flags,
        blocked_tools,
        hotspots,
        recent_failures,
    })
}

fn reflection_tool_hotspot_summary(tool: SurfaceToolHealthView) -> String {
    let mut parts = vec![format!(
        "{} success={:.0}%",
        tool.name,
        tool.success_rate * 100.0
    )];
    if tool.deprioritized {
        parts.push("deprioritized".into());
    }
    if tool.consecutive_failures > 0 {
        parts.push(format!(
            "consecutive_failures={}",
            tool.consecutive_failures
        ));
    }
    if tool.rehabilitation_count > 0 {
        parts.push(format!("rehab={}", tool.rehabilitation_count));
    }
    parts.join(", ")
}

fn reflection_tool_failure_summary(failure: SurfaceToolFailureView) -> String {
    let mut detail = match failure.turn {
        Some(turn) => format!("turn {turn} {}", failure.tool),
        None => failure.tool,
    };
    if let Some(error) = failure.error {
        detail.push_str(" — ");
        detail.push_str(&truncate(&error, 80));
    }
    detail
}

fn reflection_recent_evaluation_events(
    goal: Option<&GoalSurface>,
    verification: Option<&VerificationSurface>,
) -> Vec<ReflectionEventSummary> {
    const REFLECTION_EVENT_LIMIT: usize = 4;

    let mut events = Vec::new();
    if let Some(goal) = goal {
        events.extend(
            goal.recent_goal_events
                .iter()
                .filter_map(reflection_goal_event_summary),
        );
    }
    if let Some(verification) = verification {
        events.extend(
            verification
                .objective
                .recent_verifications
                .iter()
                .map(reflection_verification_event_summary),
        );
    }
    events.sort_by(|a, b| {
        b.turn
            .unwrap_or_default()
            .cmp(&a.turn.unwrap_or_default())
            .then_with(|| a.kind.cmp(&b.kind))
    });
    events.truncate(REFLECTION_EVENT_LIMIT);
    events
}

fn reflection_recent_adaptations(
    snapshot: Option<&PersistentSelfSnapshot>,
) -> Vec<ReflectionEventSummary> {
    const REFLECTION_EVENT_LIMIT: usize = 4;

    let Some(snapshot) = snapshot else {
        return Vec::new();
    };
    snapshot
        .evolution
        .records
        .iter()
        .filter_map(reflection_adaptation_summary)
        .take(REFLECTION_EVENT_LIMIT)
        .collect()
}

fn reflection_adaptation_summary(record: &EvolutionRecord) -> Option<ReflectionEventSummary> {
    if !is_reflection_adaptation_record(record) {
        return None;
    }
    Some(ReflectionEventSummary {
        kind: reflection_kind_label(&record.kind),
        turn: record.turn,
        detail: format!("{} — {}", record.status, truncate(&record.summary, 120)),
    })
}

fn reflection_recent_adaptation_outcomes(
    snapshot: Option<&PersistentSelfSnapshot>,
) -> Vec<ReflectionEventSummary> {
    const REFLECTION_EVENT_LIMIT: usize = 4;

    let Some(snapshot) = snapshot else {
        return Vec::new();
    };
    let mut outcomes = Vec::new();
    let mut previous_adaptation_index = None;
    for (index, record) in snapshot.evolution.records.iter().enumerate() {
        if !is_reflection_adaptation_record(record) {
            continue;
        }
        let start = previous_adaptation_index.map_or(0, |previous| previous + 1);
        if let Some(outcome) = snapshot.evolution.records[start..index]
            .iter()
            .find(|candidate| is_reflection_adaptation_outcome_record(candidate))
        {
            outcomes.push(reflection_adaptation_outcome_summary(record, outcome));
        }
        previous_adaptation_index = Some(index);
        if outcomes.len() >= REFLECTION_EVENT_LIMIT {
            break;
        }
    }
    outcomes
}

fn reflection_adaptation_outcome_summary(
    adaptation: &EvolutionRecord,
    outcome: &EvolutionRecord,
) -> ReflectionEventSummary {
    let detail = match adaptation.turn {
        Some(turn) => format!(
            "after {} turn {} — {}",
            reflection_kind_label(&adaptation.kind),
            turn,
            truncate(&outcome.summary, 120)
        ),
        None => format!(
            "after {} — {}",
            reflection_kind_label(&adaptation.kind),
            truncate(&outcome.summary, 120)
        ),
    };
    ReflectionEventSummary {
        kind: reflection_kind_label(&outcome.kind),
        turn: outcome.turn.or(adaptation.turn),
        detail,
    }
}

fn is_reflection_adaptation_record(record: &EvolutionRecord) -> bool {
    matches!(record.status.as_str(), "applied" | "enrolled" | "promoted")
        && !is_reflection_adaptation_outcome_record(record)
}

fn is_reflection_adaptation_outcome_record(record: &EvolutionRecord) -> bool {
    matches!(
        record.kind.as_str(),
        "verification" | "failure" | "stall" | "drift"
    )
}

fn reflection_goal_event_summary(event: &SurfaceEventPreview) -> Option<ReflectionEventSummary> {
    if event.event_type != "goal_steered" {
        return None;
    }
    let metadata = event.metadata.as_ref();
    let source = metadata
        .and_then(|meta| meta.get("source"))
        .and_then(serde_json::Value::as_str)
        .unwrap_or("goal_steered");
    let previous_goal = metadata
        .and_then(|meta| meta.get("previous_goal"))
        .and_then(serde_json::Value::as_str)
        .filter(|goal| !goal.is_empty());
    let new_goal = metadata
        .and_then(|meta| meta.get("new_goal"))
        .and_then(serde_json::Value::as_str)
        .unwrap_or("goal updated");
    let mut detail = match previous_goal {
        Some(previous_goal) => format!("{source}: {previous_goal} -> {new_goal}"),
        None => format!("{source}: {new_goal}"),
    };
    if let Some(extra) = metadata
        .and_then(|meta| meta.get("detail"))
        .filter(|value| !value.is_null())
    {
        detail.push_str(&format!(" ({})", compact_json_value(extra)));
    }
    Some(ReflectionEventSummary {
        kind: "GoalSteered".into(),
        turn: event.turn,
        detail,
    })
}

fn reflection_verification_event_summary(event: &VerificationEventView) -> ReflectionEventSummary {
    let outcome = match event.passed {
        Some(true) => "passed",
        Some(false) => "failed",
        None => "recorded",
    };
    let mut detail = outcome.to_string();
    if let Some(scope) = event.scope.as_deref() {
        detail.push(' ');
        detail.push_str(scope);
    }
    if let Some(target) = event.target.as_deref() {
        detail.push(' ');
        detail.push_str(target);
    }
    detail.push_str(" — ");
    detail.push_str(&truncate(&event.summary, 120));
    ReflectionEventSummary {
        kind: "Verification".into(),
        turn: event.turn,
        detail,
    }
}

fn reflection_kind_label(kind: &str) -> String {
    let mut chars = kind.chars();
    match chars.next() {
        Some(first) => format!("{}{}", first.to_ascii_uppercase(), chars.as_str()),
        None => "Event".to_string(),
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

fn persist_goal_mutation(
    session_id: &str,
    text: &str,
    source: &str,
) -> Result<GoalMutationResponse, String> {
    let mut ws = session_workspace::read_workspace(session_id).map_err(|e| e.to_string())?;
    let old_goal = ws.session_goal.clone();
    ws.session_goal = Some(text.to_string());
    ws.goal_progress = None;
    ws.updated_at = Utc::now().to_rfc3339();
    session_workspace::write_workspace(&ws).map_err(|e| e.to_string())?;
    append_config_change_event(
        session_id,
        ws.turn_count,
        "session_goal",
        &serde_json::Value::String(text.to_string()),
        old_goal.clone().map(serde_json::Value::String),
    )?;
    if old_goal.as_deref() != Some(text) {
        append_goal_steering_event(
            session_id,
            ws.turn_count,
            source,
            old_goal.as_deref(),
            text,
            None,
        )?;
    }
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
    serde_json::to_value(persist_goal_mutation(
        session_id,
        text,
        "edge_tool:set_goal",
    )?)
    .map_err(|e| e.to_string())
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
    let mut evt = JournalEvent::config_change(Some(session_id), key, &new_value.to_string());
    evt.turn = Some(turn);
    evt.metadata = Some(serde_json::Value::Object(metadata));
    writer.append(&evt).map_err(|e| e.to_string())
}

pub(crate) fn append_goal_steering_event(
    session_id: &str,
    turn: u32,
    source: &str,
    previous_goal: Option<&str>,
    new_goal: &str,
    detail: Option<serde_json::Value>,
) -> Result<(), String> {
    let writer = session_journal::JournalWriter::new(session_id).map_err(|e| e.to_string())?;
    writer
        .append(&JournalEvent::goal_steered(
            Some(session_id),
            turn,
            source,
            previous_goal,
            new_goal,
            detail,
        ))
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

#[allow(clippy::too_many_arguments)]
fn build_persistent_reflection_context(
    artifacts: &SessionArtifacts,
    signal_limit: usize,
    focus: &str,
    question: Option<&str>,
    goal: Option<ReflectionGoalSummary>,
    verification: Option<ReflectionVerificationSummary>,
    health: Option<ReflectionHealthSummary>,
    recent_performance_deltas: Vec<ReflectionEventSummary>,
    recent_adaptation_impacts: Vec<ReflectionEventSummary>,
    recent_adaptation_verification_impacts: Vec<ReflectionEventSummary>,
    recent_evaluation_events: Vec<ReflectionEventSummary>,
    recent_adaptations: Vec<ReflectionEventSummary>,
    recent_adaptation_outcomes: Vec<ReflectionEventSummary>,
) -> astra_runtime::liquid::reflection::ReflectionContext {
    const REFLECTION_TOOL_RECORD_LIMIT: usize = 24;
    const REFLECTION_TOOL_STAT_LIMIT: usize = 8;
    const REFLECTION_TACTICAL_ACTION_LIMIT: usize = 8;

    let mut context =
        astra_runtime::liquid::reflection::ReflectionContext::new(artifacts.session_id.clone());
    let journal_turn_count = artifacts
        .journal_events
        .iter()
        .filter_map(|event| event.turn)
        .max()
        .unwrap_or_default();
    context.turns_completed = artifacts
        .workspace
        .as_ref()
        .map(|ws| ws.turn_count)
        .or_else(|| {
            artifacts
                .restored
                .as_ref()
                .map(|restored| restored.turn_count)
        })
        .unwrap_or(journal_turn_count)
        .max(journal_turn_count);
    context.scenario =
        latest_scenario(&artifacts.journal_events).map(|scenario| format!("{scenario:?}"));
    context.token_utilisation = reflection_token_utilisation(artifacts);
    context.tool_stats = focus_tool_stats(
        astra_runtime::liquid::reflection::ToolStat::summarize_records(
            &recent_tool_records(&artifacts.journal_events, REFLECTION_TOOL_RECORD_LIMIT),
            REFLECTION_TOOL_STAT_LIMIT,
        ),
        focus,
    );
    context.recent_tactical_actions = focus_tactical_actions(
        recent_tactical_actions(&artifacts.journal_events, REFLECTION_TACTICAL_ACTION_LIMIT),
        focus,
    );
    context.signals = focus_reflection_signals(
        recent_reflection_signals(&artifacts.journal_events, signal_limit, question),
        focus,
    );
    context.active_experiment = active_reflection_experiment(artifacts, context.turns_completed);
    context.goal = goal;
    context.verification = verification;
    context.health = health;
    context.recent_performance_deltas = recent_performance_deltas;
    context.recent_adaptation_impacts = recent_adaptation_impacts;
    context.recent_adaptation_verification_impacts = recent_adaptation_verification_impacts;
    context.recent_evaluation_events = recent_evaluation_events;
    context.recent_adaptations = recent_adaptations;
    context.recent_adaptation_outcomes = recent_adaptation_outcomes;
    context
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

fn normalize_reflect_focus(focus: Option<&str>) -> &'static str {
    match focus.unwrap_or("auto").trim().to_ascii_lowercase().as_str() {
        "skill_failure" => "skill_failure",
        "unexpected_result" => "unexpected_result",
        "data_quality" => "data_quality",
        "tool_selection" => "tool_selection",
        "history" => "history",
        "performance" => "performance",
        _ => "auto",
    }
}

fn focused_recent_event_previews(
    events: &[JournalEvent],
    journal_limit: usize,
    focus: &str,
) -> Vec<EventPreview> {
    let event_types: &[JournalEventType] = match focus {
        "skill_failure" => &[
            JournalEventType::TurnError,
            JournalEventType::Error,
            JournalEventType::StallDetected,
            JournalEventType::VerificationCompleted,
            JournalEventType::Turn,
        ],
        "performance" => &[
            JournalEventType::Turn,
            JournalEventType::TurnError,
            JournalEventType::StallDetected,
            JournalEventType::AdaptivePerTurnApplied,
        ],
        "tool_selection" => &[
            JournalEventType::Turn,
            JournalEventType::AdaptiveScenarioApplied,
            JournalEventType::AdaptivePerTurnApplied,
            JournalEventType::AdaptiveExperimentEnrolled,
        ],
        "history" => &[
            JournalEventType::Turn,
            JournalEventType::TurnError,
            JournalEventType::Error,
            JournalEventType::StallDetected,
            JournalEventType::DriftDetected,
            JournalEventType::AdaptiveScenarioApplied,
            JournalEventType::AdaptivePerTurnApplied,
            JournalEventType::AdaptiveExperimentEnrolled,
            JournalEventType::VerificationCompleted,
        ],
        _ => &[
            JournalEventType::Turn,
            JournalEventType::TurnError,
            JournalEventType::Error,
            JournalEventType::StallDetected,
            JournalEventType::DriftDetected,
            JournalEventType::AdaptiveScenarioApplied,
            JournalEventType::AdaptivePerTurnApplied,
            JournalEventType::AdaptiveExperimentEnrolled,
        ],
    };
    recent_event_previews(events, journal_limit.clamp(1, 12), event_types)
}

fn reflection_token_utilisation(artifacts: &SessionArtifacts) -> f64 {
    if let Some(pressure) = artifacts
        .workspace
        .as_ref()
        .and_then(|ws| ws.last_context_trace.as_ref())
        .and_then(|trace| trace.budget.as_ref())
        .map(|budget| budget.budget_pressure)
    {
        return pressure;
    }

    artifacts
        .journal_events
        .iter()
        .rev()
        .find_map(|event| event.budget_pressure)
        .unwrap_or_default()
}

fn recent_tool_records(
    events: &[JournalEvent],
    max_records: usize,
) -> Vec<astra_services::session_journal::ToolCallRecord> {
    let mut records = Vec::new();
    for event in events.iter().rev() {
        let Some(tool_calls) = event.tool_calls.as_ref() else {
            continue;
        };
        for call in tool_calls.iter().rev() {
            records.push(call.clone());
            if records.len() >= max_records {
                return records;
            }
        }
    }
    records
}

fn recent_tactical_actions(events: &[JournalEvent], limit: usize) -> Vec<String> {
    let mut actions = Vec::new();
    for event in events.iter().rev() {
        if event.event_type != JournalEventType::AdaptivePerTurnApplied {
            continue;
        }
        let Some(metadata) = event.metadata.as_ref() else {
            continue;
        };
        if let Some(triggers) = metadata
            .get("triggers")
            .and_then(serde_json::Value::as_array)
        {
            for trigger in triggers {
                if let Some(trigger) = trigger.as_str() {
                    actions.push(trigger.to_string());
                    if actions.len() >= limit {
                        return actions;
                    }
                }
            }
        }
        if let Some(changes) = metadata
            .get("changes")
            .and_then(serde_json::Value::as_array)
        {
            for change in changes {
                let Some(key) = change.get("key").and_then(serde_json::Value::as_str) else {
                    continue;
                };
                let from = change
                    .get("from")
                    .map(compact_json_value)
                    .unwrap_or_else(|| "unknown".to_string());
                let to = change
                    .get("to")
                    .map(compact_json_value)
                    .unwrap_or_else(|| "unknown".to_string());
                actions.push(format!("{key}: {from} -> {to}"));
                if actions.len() >= limit {
                    return actions;
                }
            }
        }
    }
    actions
}

fn recent_reflection_signals(
    events: &[JournalEvent],
    limit: usize,
    question: Option<&str>,
) -> Vec<astra_runtime::liquid::reflection::SignalSummary> {
    let mut signals = Vec::new();
    if let Some(question) = question.filter(|question| !question.is_empty()) {
        signals.push(astra_runtime::liquid::reflection::SignalSummary {
            kind: "Question".to_string(),
            detail: truncate(question, 160),
            skill_context: None,
            turn_id: "user".to_string(),
        });
    }
    for event in events.iter().rev() {
        let turn_id = event
            .turn
            .map(|turn| format!("turn-{turn}"))
            .unwrap_or_else(|| "session".to_string());
        let skill_context = event
            .selected_skills
            .as_ref()
            .and_then(|skills| skills.first().cloned());
        match event.event_type {
            JournalEventType::Turn | JournalEventType::TurnError | JournalEventType::Error => {
                if let Some(tool_calls) = event.tool_calls.as_ref() {
                    for call in tool_calls.iter().filter(|call| !call.ok) {
                        let detail = call
                            .error
                            .as_deref()
                            .map(|error| format!("{} failed: {}", call.name, truncate(error, 160)))
                            .unwrap_or_else(|| format!("{} failed", call.name));
                        signals.push(astra_runtime::liquid::reflection::SignalSummary {
                            kind: "ToolFailure".to_string(),
                            detail,
                            skill_context: skill_context.clone(),
                            turn_id: turn_id.clone(),
                        });
                        if signals.len() >= limit {
                            return signals;
                        }
                    }
                }
                if let Some(error) = event.error.as_deref() {
                    signals.push(astra_runtime::liquid::reflection::SignalSummary {
                        kind: "TurnError".to_string(),
                        detail: truncate(error, 160),
                        skill_context,
                        turn_id,
                    });
                    if signals.len() >= limit {
                        return signals;
                    }
                }
            }
            JournalEventType::StallDetected => {
                let detail = event
                    .tools_used
                    .as_ref()
                    .filter(|tools| !tools.is_empty())
                    .map(|tools| format!("tools repeated: {}", tools.join(", ")))
                    .or_else(|| event.stall_type.clone())
                    .unwrap_or_else(|| "stall detected".to_string());
                signals.push(astra_runtime::liquid::reflection::SignalSummary {
                    kind: "StallDetected".to_string(),
                    detail: truncate(&detail, 160),
                    skill_context,
                    turn_id,
                });
                if signals.len() >= limit {
                    return signals;
                }
            }
            JournalEventType::DriftDetected => {
                let detail = event
                    .metadata
                    .as_ref()
                    .and_then(|metadata| metadata.get("detail"))
                    .map(compact_json_value)
                    .unwrap_or_else(|| "focus drift detected".to_string());
                signals.push(astra_runtime::liquid::reflection::SignalSummary {
                    kind: "DriftDetected".to_string(),
                    detail: truncate(&detail, 160),
                    skill_context,
                    turn_id,
                });
                if signals.len() >= limit {
                    return signals;
                }
            }
            _ => {}
        }
    }
    signals
}

fn focus_tool_stats(
    mut tool_stats: Vec<astra_runtime::liquid::reflection::ToolStat>,
    focus: &str,
) -> Vec<astra_runtime::liquid::reflection::ToolStat> {
    match focus {
        "performance" => tool_stats.sort_by(|a, b| {
            b.avg_latency_ms
                .cmp(&a.avg_latency_ms)
                .then_with(|| b.failures.cmp(&a.failures))
                .then_with(|| b.calls.cmp(&a.calls))
                .then_with(|| a.tool_name.cmp(&b.tool_name))
        }),
        "tool_selection" => tool_stats.sort_by(|a, b| {
            b.calls
                .cmp(&a.calls)
                .then_with(|| b.failures.cmp(&a.failures))
                .then_with(|| b.avg_latency_ms.cmp(&a.avg_latency_ms))
                .then_with(|| a.tool_name.cmp(&b.tool_name))
        }),
        _ => {}
    }
    tool_stats
}

fn focus_tactical_actions(actions: Vec<String>, focus: &str) -> Vec<String> {
    let filtered = match focus {
        "performance" => actions
            .into_iter()
            .filter(|action| {
                let lower = action.to_ascii_lowercase();
                lower.contains("token")
                    || lower.contains("latency")
                    || lower.contains("compression")
                    || lower.contains("budget")
            })
            .collect::<Vec<_>>(),
        "tool_selection" => actions
            .into_iter()
            .filter(|action| {
                let lower = action.to_ascii_lowercase();
                lower.contains("tool") || lower.contains("selection")
            })
            .collect::<Vec<_>>(),
        _ => actions,
    };
    if filtered.is_empty() {
        recent_default_actions(focus)
    } else {
        filtered
    }
}

fn recent_default_actions(focus: &str) -> Vec<String> {
    match focus {
        "performance" => vec!["prioritize latency, timeout, and token pressure".to_string()],
        "tool_selection" => {
            vec!["inspect selected vs used tools before changing routing".to_string()]
        }
        "skill_failure" => {
            vec!["trace the most recent failed tool/verification path first".to_string()]
        }
        _ => Vec::new(),
    }
}

fn focus_reflection_signals(
    signals: Vec<astra_runtime::liquid::reflection::SignalSummary>,
    focus: &str,
) -> Vec<astra_runtime::liquid::reflection::SignalSummary> {
    let original = signals.clone();
    let filtered = match focus {
        "performance" => signals
            .into_iter()
            .filter(|signal| {
                let kind = signal.kind.to_ascii_lowercase();
                let detail = signal.detail.to_ascii_lowercase();
                kind.contains("question")
                    || kind.contains("stall")
                    || detail.contains("timeout")
                    || detail.contains("latency")
                    || detail.contains("token")
            })
            .collect::<Vec<_>>(),
        "skill_failure" => signals
            .into_iter()
            .filter(|signal| {
                let kind = signal.kind.to_ascii_lowercase();
                kind.contains("question")
                    || kind.contains("toolfailure")
                    || kind.contains("turnerror")
                    || kind.contains("stall")
            })
            .collect::<Vec<_>>(),
        _ => return signals,
    };
    if filtered.is_empty() {
        original
    } else {
        filtered
    }
}

fn active_reflection_experiment(
    artifacts: &SessionArtifacts,
    turns_completed: u32,
) -> Option<astra_runtime::liquid::reflection::ExperimentSummary> {
    let workspace_experiment = artifacts
        .workspace
        .as_ref()
        .and_then(|ws| ws.active_experiment_id.clone());
    let workspace_variant = artifacts
        .workspace
        .as_ref()
        .and_then(|ws| ws.active_variant.clone());
    let journal_enrollment = latest_experiment_enrollment(&artifacts.journal_events);
    let experiment_id = workspace_experiment.or_else(|| journal_enrollment.0.clone())?;
    let variant = workspace_variant.or_else(|| journal_enrollment.1.clone())?;
    Some(astra_runtime::liquid::reflection::ExperimentSummary {
        experiment_id,
        variant,
        samples: turns_completed,
    })
}

fn latest_experiment_enrollment(events: &[JournalEvent]) -> (Option<String>, Option<String>) {
    events
        .iter()
        .rev()
        .find(|event| event.event_type == JournalEventType::AdaptiveExperimentEnrolled)
        .and_then(|event| event.metadata.as_ref())
        .map(|metadata| {
            (
                metadata
                    .get("experiment_id")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_string),
                metadata
                    .get("variant")
                    .or_else(|| metadata.get("variant_id"))
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_string),
            )
        })
        .unwrap_or((None, None))
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
        JournalEventType::AdaptiveBaselinePromoted => "adaptive_baseline_promoted",
        JournalEventType::AgentTerminated => "agent_terminated",
        JournalEventType::VerificationCompleted => "verification_completed",
        JournalEventType::CompositeSnapshot => "composite_snapshot",
        JournalEventType::PlanEdit => "plan_edit",
        JournalEventType::PlanLifecycle => "plan_lifecycle",
        JournalEventType::GoalSteered => "goal_steered",
        JournalEventType::ApprovalRequired => "approval_required",
        JournalEventType::ApprovalDecision => "approval_decision",
        JournalEventType::ApprovalTimeout => "approval_timeout",
        JournalEventType::ExecutionBoundaryOpened => "execution_boundary_opened",
        JournalEventType::ExecutionBoundaryCommitted => "execution_boundary_committed",
        JournalEventType::ExecutionBoundaryAborted => "execution_boundary_aborted",
        JournalEventType::ContextAssemblyRecorded => "context_assembly_recorded",
        JournalEventType::DriftDetected => "drift_detected",
        JournalEventType::AdaptiveScenarioApplied => "adaptive_scenario_applied",
        JournalEventType::AdaptivePerTurnApplied => "adaptive_per_turn_applied",
        JournalEventType::AdaptiveExperimentEnrolled => "adaptive_experiment_enrolled",
        JournalEventType::AdaptiveTuningRuleTriggered => "adaptive_tuning_rule_triggered",
        JournalEventType::InterruptionRecorded => "interruption_recorded",
        JournalEventType::ConfidenceDiagnosisRecorded => "confidence_diagnosis_recorded",
        JournalEventType::CompactionRetry => "compaction_retry",
        JournalEventType::LlmRound => "llm_round",
        JournalEventType::LlmRequestFull => "llm_request_full",
        JournalEventType::LlmResponseFull => "llm_response_full",
        JournalEventType::MemoryExtraction => "memory_extraction",
        JournalEventType::PipelineFeedback => "pipeline_feedback",
        JournalEventType::PipelineAlert => "pipeline_alert",
        JournalEventType::PipelineCompactionAudit => "pipeline_compaction_audit",
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli_args::{SelfReflectArgs, SelfSessionArgs};
    use astra_services::session_journal::{JournalDirGuard, ToolCallRecord};
    use astra_services::session_workspace::ContextTraceSignal;

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
                    file_path: None,
                    surgically_removed: None,
                    original_tool_name: None,
                    ..Default::default()
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
        assert_eq!(value["run"]["goal"], "finish the engine");
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
    async fn reflect_reconstructs_local_liquid_context() {
        let temp = tempfile::tempdir().unwrap();
        let _guard = JournalDirGuard::new(temp.path());
        let session_id = "self-reflect-session";
        let mut ws = WorkspaceMetadata::with_context(session_id, "gpt-5.4", "/repo", Some("main"));
        ws.turn_count = 9;
        ws.session_goal = Some("ship self surface".to_string());
        ws.plan_goal = Some("stabilize reflection loop".to_string());
        ws.goal_progress = Some(astra_services::session_workspace::GoalProgressSnapshot {
            goal: "ship self surface".to_string(),
            completion_score: 0.6,
            momentum: 0.3,
            milestone_count: 2,
            summary: "2/3 milestones complete".to_string(),
            weighted_progress: 0.6,
            negative_signals: 0.0,
            milestones: Vec::new(),
        });
        ws.contract_json = Some(
            serde_json::to_string(&astra_services::TaskContract {
                contract_id: "contract-reflect".to_string(),
                task_id: "task-reflect".to_string(),
                goal: "stabilize reflection loop".to_string(),
                scope: astra_services::TaskScope::default(),
                subtasks: vec![astra_services::DurableSubtask {
                    id: "subtask-1".to_string(),
                    title: "close reflection gap".to_string(),
                    stage: astra_services::SubtaskStage::Pending,
                    criteria: vec![astra_services::VerificationCriterion {
                        id: "criterion-1".to_string(),
                        description: "reflection evidence prompt wired".to_string(),
                        verifier: astra_services::VerifierKind::BuildPass {
                            cmd: "cargo test".to_string(),
                        },
                        required: true,
                        timeout_sec: 120,
                        global_only: false,
                    }],
                    ..Default::default()
                }],
                global_verification: Vec::new(),
                version: 1,
                status: astra_services::ContractStatus::Active,
                created_at: Utc::now().to_rfc3339(),
                updated_at: Utc::now().to_rfc3339(),
                domain_hint: None,
                task_type: None,
                last_global_results: Vec::new(),
            })
            .unwrap(),
        );
        ws.active_experiment_id = Some("exp-liquid".to_string());
        ws.active_variant = Some("treatment-a".to_string());
        ws.deprioritized_tools = vec!["bash".to_string()];
        ws.last_context_trace = Some(ContextTraceSignal {
            turn_id: "turn-9".to_string(),
            captured_at: Some(Utc::now().to_rfc3339()),
            tool_selection: None,
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
            explanations: vec!["token pressure rising".to_string()],
        });
        session_workspace::write_workspace(&ws).unwrap();

        let writer = session_journal::JournalWriter::new(session_id).unwrap();
        for (turn, bash_ok, bash_ms, rg_ms) in [
            (6, true, 60_u64, 25_u64),
            (7, true, 70, 30),
            (8, false, 220, 300),
        ] {
            let mut event = JournalEvent::turn(
                Some(session_id),
                turn,
                Some("gpt-5.4"),
                "inspect tool health",
                "record tool outcome",
                2,
                12,
                24,
                bash_ms.max(rg_ms),
            );
            event.tools_selected = Some(vec!["bash".to_string(), "rg".to_string()]);
            event.tools_used = Some(vec!["bash".to_string(), "rg".to_string()]);
            event.tool_calls = Some(vec![
                ToolCallRecord {
                    name: "bash".to_string(),
                    ok: bash_ok,
                    ms: bash_ms,
                    error: (!bash_ok).then(|| "bash regression".to_string()),
                    input_bytes: None,
                    output_bytes: None,
                    args_preview: None,
                    result_preview: None,
                    file_path: None,
                    surgically_removed: None,
                    original_tool_name: None,
                    ..Default::default()
                },
                ToolCallRecord {
                    name: "rg".to_string(),
                    ok: true,
                    ms: rg_ms,
                    error: None,
                    input_bytes: None,
                    output_bytes: None,
                    args_preview: None,
                    result_preview: None,
                    file_path: None,
                    surgically_removed: None,
                    original_tool_name: None,
                    ..Default::default()
                },
            ]);
            if !bash_ok {
                event.error = Some("bash regression".to_string());
            }
            writer.append(&event).unwrap();
        }
        writer
            .append(&JournalEvent {
                event_type: JournalEventType::AdaptiveScenarioApplied,
                ts: Utc::now().to_rfc3339(),
                session_id: Some(session_id.to_string()),
                turn: Some(8),
                agentic_step: None,
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
                turns_compacted: None,
                facts_stored: None,
                tools_selected: None,
                selected_skills: None,
                tools_used: None,
                tool_calls: None,
                budget_used: None,
                budget_pressure: None,
                stall_type: None,
                metadata: Some(serde_json::json!({"scenario": "debugging"})),
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
        writer
            .append(&JournalEvent {
                event_type: JournalEventType::AdaptivePerTurnApplied,
                ts: Utc::now().to_rfc3339(),
                session_id: Some(session_id.to_string()),
                turn: Some(9),
                agentic_step: None,
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
                turns_compacted: None,
                facts_stored: None,
                tools_selected: None,
                selected_skills: None,
                tools_used: None,
                tool_calls: None,
                budget_used: None,
                budget_pressure: None,
                stall_type: None,
                metadata: Some(serde_json::json!({
                    "triggers": ["high token pressure"],
                    "changes": [{
                        "key": "verification.strictness",
                        "from": 0.6,
                        "to": 0.7
                    }]
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
        writer
            .append(&JournalEvent {
                event_type: JournalEventType::TurnError,
                ts: Utc::now().to_rfc3339(),
                session_id: Some(session_id.to_string()),
                turn: Some(9),
                agentic_step: None,
                model: Some("gpt-5.4".to_string()),
                user_input: Some("fix the bug".to_string()),
                assistant_output: None,
                tool_count: Some(2),
                tokens_in: Some(20),
                tokens_out: Some(40),
                duration_ms: Some(120),
                error: Some("timed out waiting for test".to_string()),
                config_key: None,
                config_value: None,
                turns_compacted: None,
                facts_stored: None,
                tools_selected: Some(vec!["bash".to_string(), "rg".to_string()]),
                selected_skills: Some(vec!["goal-driven-evolution".to_string()]),
                tools_used: Some(vec!["bash".to_string(), "rg".to_string()]),
                tool_calls: Some(vec![
                    ToolCallRecord {
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
                    },
                    ToolCallRecord {
                        name: "rg".to_string(),
                        ok: true,
                        ms: 12,
                        error: None,
                        input_bytes: None,
                        output_bytes: None,
                        args_preview: None,
                        result_preview: None,
                        file_path: None,
                        surgically_removed: None,
                        original_tool_name: None,
                        ..Default::default()
                    },
                ]),
                budget_used: Some(9100),
                budget_pressure: Some(0.91),
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
                context_assembly_trace: Some(serde_json::json!({"tokens": 9100})),
                selector_confidence: Some(0.8),
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
        writer
            .append(&JournalEvent::goal_steered(
                Some(session_id),
                8,
                "plan_execution_start",
                Some("ship self surface"),
                "stabilize reflection loop",
                None,
            ))
            .unwrap();
        writer
            .append(&JournalEvent::verification_completed(
                Some(session_id),
                8,
                "subtask-1",
                "global",
                true,
                &serde_json::json!([{"check":"unit-tests","passed":true}]),
            ))
            .unwrap();
        writer
            .append(&JournalEvent::verification_completed(
                Some(session_id),
                9,
                "subtask-1",
                "global",
                false,
                &serde_json::json!([{"check":"integration-tests","passed":false}]),
            ))
            .unwrap();

        let body = execute_self_command(
            &SelfCmd::Reflect(SelfReflectArgs {
                session_id: Some(session_id.to_string()),
                focus: "performance".to_string(),
                question: Some("why was bash slow?".to_string()),
                last_n: 20,
            }),
            None,
        )
        .await
        .unwrap();

        let value: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(value["focus"], "performance");
        assert_eq!(value["question"], "why was bash slow?");
        assert_eq!(value["reflection_context"]["scenario"], "Debugging");
        assert_eq!(
            value["reflection_context"]["active_experiment"]["experiment_id"],
            "exp-liquid"
        );
        assert_eq!(
            value["reflection_context"]["tool_stats"][0]["tool_name"],
            "bash"
        );
        assert_eq!(
            value["reflection_context"]["signals"][0]["kind"],
            "Question"
        );
        assert_eq!(
            value["reflection_context"]["goal"]["effective_goal"],
            "stabilize reflection loop"
        );
        assert_eq!(
            value["reflection_context"]["goal"]["goal_source"],
            "plan_goal"
        );
        assert_eq!(
            value["reflection_context"]["verification"]["objective_ok"],
            false
        );
        assert_eq!(
            value["reflection_context"]["health"]["blocked_tools"][0],
            "bash"
        );
        assert_eq!(
            value["reflection_context"]["recent_performance_deltas"][0]["kind"],
            "Regressed"
        );
        assert_eq!(
            value["reflection_context"]["recent_adaptation_impacts"][0]["kind"],
            "Regressed"
        );
        assert_eq!(
            value["reflection_context"]["recent_adaptation_verification_impacts"][0]["kind"],
            "Regressed"
        );
        assert_eq!(
            value["reflection_context"]["recent_evaluation_events"][0]["kind"],
            "Verification"
        );
        assert_eq!(
            value["reflection_context"]["recent_evaluation_events"][1]["kind"],
            "GoalSteered"
        );
        assert_eq!(
            value["reflection_context"]["recent_adaptations"][0]["kind"],
            "Adaptation"
        );
        assert_eq!(
            value["reflection_context"]["recent_adaptation_outcomes"][0]["kind"],
            "Verification"
        );
        assert_eq!(
            value["reflection_context"]["recent_tactical_actions"][0],
            "high token pressure"
        );
        assert!(
            value["prompt_preview"]
                .as_str()
                .unwrap()
                .contains("Question")
        );
        assert!(
            value["prompt_preview"]
                .as_str()
                .unwrap()
                .contains("Effective goal: stabilize reflection loop")
        );
        assert!(
            value["prompt_preview"]
                .as_str()
                .unwrap()
                .contains("Verification summary:")
        );
        assert!(
            value["prompt_preview"]
                .as_str()
                .unwrap()
                .contains("Tool health:")
        );
        assert!(
            value["prompt_preview"]
                .as_str()
                .unwrap()
                .contains("Recent performance deltas:")
        );
        assert!(
            value["prompt_preview"]
                .as_str()
                .unwrap()
                .contains("Recent adaptation impacts:")
        );
        assert!(
            value["prompt_preview"]
                .as_str()
                .unwrap()
                .contains("Recent adaptation verification impacts:")
        );
        assert!(
            value["prompt_preview"]
                .as_str()
                .unwrap()
                .contains("Blocked tools: bash")
        );
        assert!(
            value["prompt_preview"]
                .as_str()
                .unwrap()
                .contains("Recent evaluation events:")
        );
        assert!(
            value["prompt_preview"]
                .as_str()
                .unwrap()
                .contains("[GoalSteered]")
        );
        assert!(
            value["prompt_preview"]
                .as_str()
                .unwrap()
                .contains("Recent adaptations:")
        );
        assert!(
            value["prompt_preview"]
                .as_str()
                .unwrap()
                .contains("[Adaptation]")
        );
        assert!(
            value["prompt_preview"]
                .as_str()
                .unwrap()
                .contains("Recent adaptation outcomes:")
        );
        assert!(
            value["prompt_preview"]
                .as_str()
                .unwrap()
                .contains("[Verification] after Adaptation turn 9")
        );
    }

    #[tokio::test]
    async fn health_surface_exposes_risk_flags_and_acceptance() {
        let temp = tempfile::tempdir().unwrap();
        let _guard = JournalDirGuard::new(temp.path());
        let session_id = "self-health-session";
        let mut ws = WorkspaceMetadata::with_context(session_id, "gpt-5.4", "/repo", Some("main"));
        ws.deprioritized_tools = vec!["bash".to_string()];
        ws.last_context_trace = Some(ContextTraceSignal {
            turn_id: "turn-3".to_string(),
            captured_at: Some(Utc::now().to_rfc3339()),
            tool_selection: None,
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
                tools_selected: Some(vec!["bash".to_string()]),
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
                budget_used: Some(9100),
                budget_pressure: Some(0.91),
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
                context_assembly_trace: None,
                selector_confidence: Some(0.6),
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
}
