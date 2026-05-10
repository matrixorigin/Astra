use chrono::Utc;
use serde::Serialize;

use crate::cli_args::{
    SelfCmd, SelfJournalArgs, SelfMutateCmd, SelfMutateConfigArgs, SelfMutateGoalArgs,
    SelfReflectArgs,
};
use crate::cli_utils::resumable_last_session_id;
use astra_config::runtime_config::RuntimeConfig;
use astra_runtime::self_model::ConstraintSet;
use astra_runtime::tool_registry::ToolRegistry;
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
    /// Placeholder: the old liquid-reflection subsystem was removed. The CLI now
    /// returns a minimal reflection surface so callers can still inspect the
    /// recent journal turns under the chosen focus.
    reflection_context: serde_json::Value,
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
    // The old liquid-reflection subsystem has been removed. Callers now get a
    // trimmed surface: the session id, the requested focus/question, and a
    // journal preview filtered by the focus kind. Downstream UIs should read
    // `recent_turns` directly; `reflection_context` is intentionally minimal.
    let turns_completed = artifacts
        .journal_events
        .iter()
        .filter_map(|event| event.turn)
        .max()
        .unwrap_or_default();
    let reflection_context = serde_json::json!({
        "session_id": artifacts.session_id,
        "turns_completed": turns_completed,
        "focus": focus,
        "question": question,
        "note": "liquid-reflection subsystem removed; see recent_turns for journal signals",
    });
    let prompt_preview = match question {
        Some(q) => format!("Focus: {focus}\nQuestion: {q}"),
        None => format!("Focus: {focus}"),
    };
    ReflectResponse {
        session_id: artifacts.session_id.clone(),
        focus: focus.to_string(),
        question: question.map(str::to_string),
        reflection_context,
        prompt_preview,
        recent_turns: focused_recent_event_previews(
            &artifacts.journal_events,
            journal_limit,
            focus,
        ),
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
        JournalEventType::AgentSpawned => "agent_spawned",
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
        JournalEventType::SessionMemoryExtraction => "session_memory_extraction",
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
    use crate::cli_args::SelfSessionArgs;
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
