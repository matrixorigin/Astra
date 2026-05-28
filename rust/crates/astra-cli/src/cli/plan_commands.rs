use astra_services::task_orchestrator::{TaskPlan, TaskStatus};
use serde_json::Value;

use crate::cli::session_state::SessionState;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ParsedPlanCommand {
    Go,
    Show,
    Rewind { anchor: String },
    AddCorrection { note: String },
    ClearCorrections,
}

pub(crate) fn parse_plan_command(text: &str) -> Option<ParsedPlanCommand> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return None;
    }

    if trimmed.eq_ignore_ascii_case("go") {
        return Some(ParsedPlanCommand::Go);
    }
    if trimmed.eq_ignore_ascii_case("show") {
        return Some(ParsedPlanCommand::Show);
    }

    for prefix in ["rewind", "restart", "redo from"] {
        if let Some(rest) = strip_ascii_ci_prefix(trimmed, prefix) {
            return Some(ParsedPlanCommand::Rewind {
                anchor: rest.to_string(),
            });
        }
    }

    for prefix in ["correct", "note", "adjust"] {
        if let Some(rest) = strip_ascii_ci_prefix(trimmed, prefix) {
            if rest.eq_ignore_ascii_case("clear") {
                return Some(ParsedPlanCommand::ClearCorrections);
            }
            return Some(ParsedPlanCommand::AddCorrection {
                note: rest.to_string(),
            });
        }
    }

    None
}

pub(crate) fn is_plan_command_available(state: &SessionState, command: &ParsedPlanCommand) -> bool {
    match command {
        ParsedPlanCommand::Go | ParsedPlanCommand::Show | ParsedPlanCommand::Rewind { .. } => {
            has_authoring_plan(state) || has_executing_plan(state)
        }
        ParsedPlanCommand::AddCorrection { .. } | ParsedPlanCommand::ClearCorrections => {
            has_executing_plan(state)
        }
    }
}

pub(crate) fn render_plan_snapshot(state: &SessionState) -> Result<String, String> {
    if let Some(sync_error) = state.plan_mode_sync_error.as_deref()
        && state
            .cloud_plan_mirror
            .as_ref()
            .is_some_and(|plan_mode| !plan_mode.goal.trim().is_empty())
    {
        return Err(format!(
            "Plan mirror is stale: {sync_error}. Send another planning turn after the server recovers, or use `/plan` to exit and re-enter before using `show`."
        ));
    }

    if let Some(plan_mode) = state
        .cloud_plan_mirror
        .as_ref()
        .filter(|plan_mode| !plan_mode.goal.trim().is_empty())
    {
        return Ok(render_plan(
            "Plan authoring",
            Some(plan_mode.goal.as_str()),
            &plan_mode.plan,
            &[],
        ));
    }

    if let Some(plan) = state.executing_plan.as_ref() {
        return Ok(render_plan(
            "Paused plan",
            state.executing_plan_goal.as_deref(),
            plan,
            &state.plan_execution_corrections,
        ));
    }

    Err("No active plan to show.".to_string())
}

pub(crate) fn apply_plan_correction(
    state: &mut SessionState,
    command: &ParsedPlanCommand,
) -> Result<String, String> {
    if !has_executing_plan(state) {
        return Err(
            "No paused plan to annotate. Run a plan first, then use `correct ...` after it pauses."
                .to_string(),
        );
    }

    match command {
        ParsedPlanCommand::ClearCorrections => {
            state.plan_execution_corrections.clear();
            Ok("Cleared queued plan corrections.".to_string())
        }
        ParsedPlanCommand::AddCorrection { note } => {
            let note = note.trim();
            if note.is_empty() {
                return Err("Usage: correct <note> (or `correct clear`).".to_string());
            }
            state.plan_execution_corrections.push(note.to_string());
            Ok(format!(
                "Queued plan correction #{}: {}",
                state.plan_execution_corrections.len(),
                note
            ))
        }
        _ => Err("That command does not add plan corrections.".to_string()),
    }
}

pub(crate) async fn prepare_plan_execution(
    state: &mut SessionState,
    api: &astra_thin_client::ThinClient,
    token: &str,
) -> Result<(), String> {
    let from_authoring = has_authoring_plan(state);
    let (plan, goal) = if let Some(plan_mode) = state
        .cloud_plan_mirror
        .as_ref()
        .filter(|plan_mode| !plan_mode.goal.trim().is_empty())
    {
        if let Some(sync_error) = state.plan_mode_sync_error.as_deref() {
            return Err(format!(
                "Plan mirror is stale: {sync_error}. Send another planning turn after the server recovers, or use `/plan` to exit and re-enter before running `go`."
            ));
        }
        (plan_mode.plan.clone(), Some(plan_mode.goal.clone()))
    } else if let Some(plan) = state.executing_plan.clone() {
        (plan, state.executing_plan_goal.clone())
    } else {
        return Err("No plan is ready to run. Create or restore a plan first.".to_string());
    };

    if plan.subtasks.is_empty() {
        return Err(
            "No plan is ready to run yet — wait for subtasks before using `go`.".to_string(),
        );
    }

    let plan_id = if from_authoring {
        crate::cli::plan_lifecycle::exit_remote_plan_mode(api, token, state, true).await?
    } else {
        state.executing_plan_id.clone()
    };

    reset_plan_runtime_metadata(state);
    state.last_delivery_report = None;
    state.executing_plan = Some(plan);
    state.executing_plan_goal = goal;
    state.executing_plan_id = plan_id;
    state.plan_execution_rounds = state.plan_execution_rounds.saturating_add(1);
    Ok(())
}

pub(crate) async fn rewind_plan(
    state: &mut SessionState,
    api: &astra_thin_client::ThinClient,
    token: Option<&str>,
    anchor: &str,
) -> Result<String, String> {
    let target = if has_authoring_plan(state) {
        PlanTarget::Authoring
    } else if has_executing_plan(state) {
        PlanTarget::Executing
    } else {
        return Err("No active plan to rewind.".to_string());
    };

    let anchor = anchor.trim();
    if anchor.is_empty() {
        return Err("Usage: rewind <step-number|subtask-id-prefix>.".to_string());
    }

    let remote_plan_id = match target {
        PlanTarget::Authoring => {
            if let (Some(token), Some(session_id)) = (
                token,
                state
                    .session_id
                    .as_deref()
                    .filter(|sid| !sid.trim().is_empty()),
            ) {
                crate::cli::plan_lifecycle::active_remote_planning_plan_id(api, token, session_id)
                    .await?
            } else {
                None
            }
        }
        PlanTarget::Executing => state.executing_plan_id.clone(),
    };

    let (reset_count, version) =
        if let (Some(token), Some(plan_id)) = (token, remote_plan_id.as_deref()) {
            let response = api
                .post_plan_rewind_json(token, plan_id, &serde_json::json!({ "anchor": anchor }))
                .await
                .map_err(crate::map_thin_err)?;
            let plan_value = response
                .get("plan")
                .cloned()
                .ok_or_else(|| "rewind response missing plan".to_string())?;
            let plan: TaskPlan = serde_json::from_value(plan_value)
                .map_err(|error| format!("invalid rewind plan payload: {error}"))?;
            let reset_count = response
                .get("reset_count")
                .and_then(Value::as_u64)
                .unwrap_or_default() as usize;
            let version = response.get("version").and_then(Value::as_u64);
            apply_rewound_plan(state, target, plan, version);
            (reset_count, version)
        } else {
            let reset_count = rewind_local_plan(state, target, anchor)?;
            (reset_count, None)
        };

    if matches!(target, PlanTarget::Executing) {
        reset_plan_runtime_metadata(state);
        state.durable_task_state = None;
        state.last_delivery_report = None;
    }

    let mut message = format!("Rewound plan from `{anchor}`; reset {reset_count} subtask(s).");
    if matches!(target, PlanTarget::Authoring) {
        if let Some(version) = version {
            message.push_str(&format!(" Plan version is now {version}."));
        } else {
            message.push_str(" Plan authoring view updated.");
        }
    } else {
        message.push_str(" Use `show` to inspect, then `go` to rerun.");
    }
    Ok(message)
}

pub(crate) fn abandon_plan_execution(state: &mut SessionState) -> bool {
    let had_plan = has_executing_plan(state)
        || state.plan_run_task_id.is_some()
        || state.plan_run_task_last_error.is_some()
        || state.plan_run_task_last_progress.is_some();
    if !had_plan {
        return false;
    }

    let _ = crate::cli::plan_runtime::shutdown_plan_executor(state);
    reset_plan_runtime_metadata(state);
    state.executing_plan = None;
    state.executing_plan_goal = None;
    state.executing_plan_id = None;
    state.plan_execution_config = None;
    state.plan_execution_rounds = 0;
    state.plan_execution_corrections.clear();
    state.durable_task_state = None;
    state.last_delivery_report = None;
    true
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PlanTarget {
    Authoring,
    Executing,
}

fn has_authoring_plan(state: &SessionState) -> bool {
    state.cloud_plan_mirror.as_ref().is_some_and(|plan_mode| {
        !plan_mode.goal.trim().is_empty() && state.plan_mode_sync_error.is_none()
    })
}

fn has_executing_plan(state: &SessionState) -> bool {
    state.executing_plan.is_some() || state.plan_handle.is_some()
}

fn strip_ascii_ci_prefix<'a>(text: &'a str, prefix: &str) -> Option<&'a str> {
    let head = text.get(..prefix.len())?;
    if !head.eq_ignore_ascii_case(prefix) {
        return None;
    }
    let rest = &text[prefix.len()..];
    if rest.is_empty() {
        return Some(rest);
    }
    rest.chars()
        .next()
        .filter(|ch| ch.is_whitespace())
        .map(|_| rest.trim())
}

fn render_plan(label: &str, goal: Option<&str>, plan: &TaskPlan, corrections: &[String]) -> String {
    let mut lines = Vec::new();
    let total = plan.subtasks.len();
    let done = plan.items_done();
    lines.push(format!(
        "{label}: {done}/{total} subtasks complete ({}%).",
        plan.progress_pct()
    ));
    if let Some(goal) = goal.map(str::trim).filter(|goal| !goal.is_empty()) {
        lines.push(format!("Goal: {goal}"));
    }
    if plan.subtasks.is_empty() {
        lines.push("No subtasks yet.".to_string());
    } else {
        for (idx, subtask) in plan.subtasks.iter().enumerate() {
            lines.push(format!(
                "{}. [{}] {} ({})",
                idx + 1,
                status_label(subtask.status),
                subtask.title,
                subtask.id
            ));
        }
    }
    if !corrections.is_empty() {
        lines.push("Queued corrections:".to_string());
        for correction in corrections {
            lines.push(format!("- {}", correction.trim()));
        }
    }
    lines.join("\n")
}

fn status_label(status: TaskStatus) -> &'static str {
    match status {
        TaskStatus::Pending => "pending",
        TaskStatus::InProgress => "in_progress",
        TaskStatus::Paused => "paused",
        TaskStatus::Completed => "completed",
        TaskStatus::Failed => "failed",
        TaskStatus::Cancelled => "cancelled",
    }
}

fn resolve_rewind_start_index(plan: &TaskPlan, anchor: &str) -> Result<usize, String> {
    if let Ok(index) = anchor.parse::<usize>() {
        if index == 0 || index > plan.subtasks.len() {
            return Err(format!("subtask index must be 1..={}", plan.subtasks.len()));
        }
        return Ok(index - 1);
    }

    let matches: Vec<usize> = plan
        .subtasks
        .iter()
        .enumerate()
        .filter(|(_, subtask)| subtask.id == anchor || subtask.id.starts_with(anchor))
        .map(|(idx, _)| idx)
        .collect();
    match matches.len() {
        0 => Err(format!("no subtask id matches {anchor:?}")),
        1 => Ok(matches[0]),
        _ => Err(format!(
            "ambiguous id {anchor:?} ({} matches); use a longer prefix or `rewind N`",
            matches.len()
        )),
    }
}

fn rewind_plan_from_subtask(plan: &mut TaskPlan, start_idx: usize) -> usize {
    let mut reset_count = 0usize;
    for subtask in plan.subtasks.iter_mut().skip(start_idx) {
        if matches!(
            subtask.status,
            TaskStatus::Completed
                | TaskStatus::InProgress
                | TaskStatus::Paused
                | TaskStatus::Failed
        ) {
            subtask.status = TaskStatus::Pending;
            reset_count += 1;
        }
    }
    reset_count
}

fn rewind_local_plan(
    state: &mut SessionState,
    target: PlanTarget,
    anchor: &str,
) -> Result<usize, String> {
    match target {
        PlanTarget::Authoring => {
            let plan_mode = state
                .cloud_plan_mirror
                .as_mut()
                .ok_or_else(|| "No active plan to rewind.".to_string())?;
            let start_idx = resolve_rewind_start_index(&plan_mode.plan, anchor)?;
            let reset_count = rewind_plan_from_subtask(&mut plan_mode.plan, start_idx);
            plan_mode.modified = true;
            Ok(reset_count)
        }
        PlanTarget::Executing => {
            let plan = state
                .executing_plan
                .as_mut()
                .ok_or_else(|| "No active plan to rewind.".to_string())?;
            let start_idx = resolve_rewind_start_index(plan, anchor)?;
            Ok(rewind_plan_from_subtask(plan, start_idx))
        }
    }
}

fn apply_rewound_plan(
    state: &mut SessionState,
    target: PlanTarget,
    plan: TaskPlan,
    version: Option<u64>,
) {
    match target {
        PlanTarget::Authoring => {
            if let Some(plan_mode) = state.cloud_plan_mirror.as_mut() {
                plan_mode.plan = plan;
                plan_mode.modified = true;
                if let Some(version) = version {
                    plan_mode.version = version;
                }
            }
        }
        PlanTarget::Executing => {
            state.executing_plan = Some(plan);
        }
    }
}

fn reset_plan_runtime_metadata(state: &mut SessionState) {
    let _ = crate::cli::plan_runtime::shutdown_plan_executor(state);
    state.current_plan_subtask_id = None;
    state.plan_run_task_id = None;
    state.plan_run_task_last_progress = None;
    state.plan_run_task_last_error = None;
    state.pending_approval = None;
    state.plan_in_token_stream = false;
    state.plan_md_renderer = None;
    state.plan_thinking_pane = None;
}

#[cfg(test)]
mod tests {
    use super::*;
    use astra_runtime::plan::PlanModeState;
    use astra_services::task_orchestrator::SubtaskPlan;
    use wiremock::matchers::{header, method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn sample_plan() -> TaskPlan {
        TaskPlan {
            subtasks: vec![
                SubtaskPlan {
                    id: "s1".into(),
                    title: "Inspect auth flow".into(),
                    status: TaskStatus::Completed,
                    ..Default::default()
                },
                SubtaskPlan {
                    id: "s2".into(),
                    title: "Patch middleware".into(),
                    status: TaskStatus::Failed,
                    ..Default::default()
                },
                SubtaskPlan {
                    id: "s3".into(),
                    title: "Write regression tests".into(),
                    status: TaskStatus::Pending,
                    ..Default::default()
                },
            ],
            notes: None,
        }
    }

    fn sample_plan_mode(goal: &str) -> PlanModeState {
        let mut state = PlanModeState::new(goal.to_string());
        state.plan = sample_plan();
        state.modified = true;
        state
    }

    #[test]
    fn parse_plan_command_recognizes_aliases() {
        assert_eq!(parse_plan_command("go"), Some(ParsedPlanCommand::Go));
        assert_eq!(parse_plan_command("show"), Some(ParsedPlanCommand::Show));
        assert_eq!(
            parse_plan_command("restart 2"),
            Some(ParsedPlanCommand::Rewind { anchor: "2".into() })
        );
        assert_eq!(
            parse_plan_command("redo from s2"),
            Some(ParsedPlanCommand::Rewind {
                anchor: "s2".into()
            })
        );
        assert_eq!(
            parse_plan_command("correct clear"),
            Some(ParsedPlanCommand::ClearCorrections)
        );
        assert_eq!(
            parse_plan_command("note add more verification"),
            Some(ParsedPlanCommand::AddCorrection {
                note: "add more verification".into()
            })
        );
    }

    #[test]
    fn correction_commands_only_intercept_paused_execution() {
        let mut state = crate::SessionState::default();
        state.cloud_plan_mirror = Some(sample_plan_mode("Ship auth"));
        assert!(is_plan_command_available(&state, &ParsedPlanCommand::Go));
        assert!(!is_plan_command_available(
            &state,
            &ParsedPlanCommand::AddCorrection {
                note: "keep logs".into()
            }
        ));

        state.executing_plan = Some(sample_plan());
        assert!(is_plan_command_available(
            &state,
            &ParsedPlanCommand::AddCorrection {
                note: "keep logs".into()
            }
        ));
    }

    #[test]
    fn stale_authoring_mirror_blocks_plan_commands_that_read_or_run_it() {
        let mut state = crate::SessionState::default();
        state.cloud_plan_mirror = Some(sample_plan_mode("Ship auth"));
        state.plan_mode_sync_error = Some("server returned 409".into());

        assert!(
            !is_plan_command_available(&state, &ParsedPlanCommand::Show),
            "show must not render stale authoring state when sync failed"
        );
        assert!(
            !is_plan_command_available(&state, &ParsedPlanCommand::Go),
            "go must not start from stale authoring state when sync failed"
        );
        assert!(
            render_plan_snapshot(&state)
                .unwrap_err()
                .contains("Send another planning turn"),
            "snapshot should explain why the authoring plan cannot be shown and how to recover"
        );
    }

    #[test]
    fn transient_sync_failure_blocks_commands_until_recovery() {
        let mut state = crate::SessionState::default();
        state.cloud_plan_mirror = Some(sample_plan_mode("Ship auth"));

        assert!(is_plan_command_available(&state, &ParsedPlanCommand::Show));
        assert!(render_plan_snapshot(&state).is_ok());

        state.plan_mode_sync_error = Some("network timeout".into());
        assert!(
            !is_plan_command_available(&state, &ParsedPlanCommand::Show),
            "transient sync failures must block stale authoring reads"
        );
        let stale_message = render_plan_snapshot(&state).unwrap_err();
        assert!(stale_message.contains("network timeout"));
        assert!(
            stale_message.contains("/plan"),
            "stale mirror errors must include a concrete escape hatch: {stale_message}"
        );

        state.plan_mode_sync_error = None;
        assert!(
            is_plan_command_available(&state, &ParsedPlanCommand::Show),
            "clearing the sync error after recovery should restore authoring commands"
        );
        assert!(render_plan_snapshot(&state).is_ok());
    }

    #[test]
    fn apply_plan_correction_adds_and_clears_notes() {
        let mut state = crate::SessionState::default();
        state.executing_plan = Some(sample_plan());

        let added = apply_plan_correction(
            &mut state,
            &ParsedPlanCommand::AddCorrection {
                note: "add regression coverage".into(),
            },
        )
        .unwrap();
        assert!(added.contains("Queued plan correction #1"));
        assert_eq!(
            state.plan_execution_corrections,
            vec!["add regression coverage"]
        );

        let cleared =
            apply_plan_correction(&mut state, &ParsedPlanCommand::ClearCorrections).unwrap();
        assert_eq!(cleared, "Cleared queued plan corrections.");
        assert!(state.plan_execution_corrections.is_empty());
    }

    #[test]
    fn render_plan_snapshot_includes_goal_statuses_and_corrections() {
        let mut state = crate::SessionState::default();
        state.executing_plan = Some(sample_plan());
        state.executing_plan_goal = Some("Ship auth".into());
        state.plan_execution_corrections = vec!["add regression coverage".into()];

        let rendered = render_plan_snapshot(&state).unwrap();
        assert!(rendered.contains("Paused plan: 1/3 subtasks complete"));
        assert!(rendered.contains("Goal: Ship auth"));
        assert!(rendered.contains("2. [failed] Patch middleware (s2)"));
        assert!(rendered.contains("Queued corrections:"));
    }

    #[tokio::test]
    async fn prepare_plan_execution_exits_authoring_mode_and_stages_execution() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/plans"))
            .and(header("authorization", "Bearer token"))
            .and(query_param("session_id", "sess-1"))
            .and(query_param("phase", "planning"))
            .and(query_param("limit", "1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "plans": [{ "plan_id": "plan-7", "goal": "Ship auth" }]
            })))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/plans/plan-7/exit-plan-mode"))
            .and(header("authorization", "Bearer token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "plan_id": "plan-7",
                "phase": "refining",
                "goal": "Ship auth"
            })))
            .mount(&server)
            .await;

        let api = astra_thin_client::ThinClient::new(&server.uri(), None).unwrap();
        let mut state = crate::SessionState::default();
        state.session_id = Some("sess-1".into());
        state.cloud_plan_mirror = Some(sample_plan_mode("Ship auth"));

        prepare_plan_execution(&mut state, &api, "token")
            .await
            .unwrap();

        assert!(state.cloud_plan_mirror.is_none());
        assert_eq!(state.executing_plan_goal.as_deref(), Some("Ship auth"));
        assert_eq!(state.executing_plan_id.as_deref(), Some("plan-7"));
        assert_eq!(state.plan_execution_rounds, 1);
        assert_eq!(
            state
                .executing_plan
                .as_ref()
                .map(|plan| plan.subtasks.len()),
            Some(3)
        );
    }

    #[tokio::test]
    async fn rewind_plan_resets_paused_execution_and_clears_runtime_metadata() {
        let mut state = crate::SessionState::default();
        state.executing_plan = Some(sample_plan());
        state.executing_plan_goal = Some("Ship auth".into());
        state.current_plan_subtask_id = Some("s2".into());
        state.plan_run_task_id = Some("task-1".into());
        state.plan_run_task_last_progress = Some((33, 1, 3));
        state.plan_run_task_last_error = Some("boom".into());
        state.plan_execution_corrections = vec!["retry with tests".into()];

        let api = astra_thin_client::ThinClient::new("http://localhost:1", None).unwrap();
        let message = rewind_plan(&mut state, &api, None, "2").await.unwrap();

        assert!(message.contains("reset 1 subtask(s)"));
        let plan = state.executing_plan.as_ref().unwrap();
        assert_eq!(plan.subtasks[0].status, TaskStatus::Completed);
        assert_eq!(plan.subtasks[1].status, TaskStatus::Pending);
        assert_eq!(plan.subtasks[2].status, TaskStatus::Pending);
        assert!(state.current_plan_subtask_id.is_none());
        assert!(state.plan_run_task_id.is_none());
        assert!(state.plan_run_task_last_progress.is_none());
        assert!(state.plan_run_task_last_error.is_none());
        assert_eq!(state.plan_execution_corrections, vec!["retry with tests"]);
    }

    #[test]
    fn abandon_plan_execution_clears_active_plan_state() {
        let mut state = crate::SessionState::default();
        state.executing_plan = Some(sample_plan());
        state.executing_plan_goal = Some("Ship auth".into());
        state.executing_plan_id = Some("plan-7".into());
        state.plan_execution_rounds = 2;
        state.plan_execution_corrections = vec!["retry with tests".into()];
        state.plan_run_task_id = Some("task-1".into());

        assert!(abandon_plan_execution(&mut state));
        assert!(state.executing_plan.is_none());
        assert!(state.executing_plan_goal.is_none());
        assert!(state.executing_plan_id.is_none());
        assert_eq!(state.plan_execution_rounds, 0);
        assert!(state.plan_execution_corrections.is_empty());
        assert!(state.plan_run_task_id.is_none());
    }
}
