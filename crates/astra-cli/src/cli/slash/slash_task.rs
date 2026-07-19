use crate::cli::chat_stream::{ChatTurnParams, DEFAULT_TURN_INDEX, stream_chat_sse};
use crate::cli::permission_manager::PermissionManager;
use crate::cli::session::session_state::{ExplainMode, SessionState};
use crate::cli::surface::task_checkpoint_surface::{
    encode_task_failure_message, task_list_item_claimability_icon,
    task_list_item_claimability_label, task_list_item_outcome, task_list_item_status_icon,
    task_list_item_status_label,
};
use crate::cli::surface::task_result_surface::{
    load_task_result_read_surface, render_task_result_header_value, task_result_header_fields,
};
use crate::cli::task::task_result_artifact::load_task_result_artifact;
use crate::cli::theme;
use crossterm::style::Stylize;

async fn mark_background_task_failed(
    svc: &dyn astra_services::TaskService,
    user_id: &str,
    task_id: &str,
    error_kind: &str,
    error: String,
) -> String {
    let stored_error = encode_task_failure_message(error_kind, &error);
    match svc.fail_task(user_id, task_id, &stored_error).await {
        Ok(()) => error,
        Err(fail_err) => {
            format!("{error}; additionally failed to persist failed status: {fail_err}")
        }
    }
}

async fn persist_background_task_result(
    svc: &dyn astra_services::TaskService,
    user_id: &str,
    task_id: &str,
    session_id: Option<String>,
    sr: &crate::StreamResult,
) -> Result<astra_services::TaskOutcome, String> {
    let exit_code = crate::cli::task::task_result_projection::stream_result_exit_code(sr);
    if let Err(error) = svc
        .save_checkpoint(
            user_id,
            task_id,
            &astra_services::task_orchestrator::TaskCheckpoint {
                active_subtask_id: None,
                turn: 0,
                session_id,
                state: crate::cli::task::task_result_projection::task_checkpoint_state_from_result(
                    sr, None, exit_code,
                ),
            },
        )
        .await
    {
        return Err(mark_background_task_failed(
            svc,
            user_id,
            task_id,
            "persistence_error",
            format!("failed to save background task result: {error}"),
        )
        .await);
    }

    match exit_code {
        crate::cli::exit_code::ExitCode::Success => {
            if let Err(error) = svc.complete_task(user_id, task_id).await {
                return Err(mark_background_task_failed(
                    svc,
                    user_id,
                    task_id,
                    "persistence_error",
                    format!("failed to mark background task finalized: {error}"),
                )
                .await);
            }
            Ok(astra_services::TaskOutcome::Success)
        }
        crate::cli::exit_code::ExitCode::Partial => {
            let outcome =
                crate::cli::task::task_result_projection::stream_result_completion_outcome(sr);
            if let Err(error) = svc
                .complete_task_with_outcome(user_id, task_id, outcome)
                .await
            {
                return Err(mark_background_task_failed(
                    svc,
                    user_id,
                    task_id,
                    "persistence_error",
                    format!("failed to mark background task finalized: {error}"),
                )
                .await);
            }
            Ok(outcome)
        }
        _ => Err(mark_background_task_failed(
            svc,
            user_id,
            task_id,
            crate::cli::command_router::error_kind_for_exit_code(exit_code)
                .unwrap_or("tool_failure"),
            crate::cli::task::task_result_projection::stream_result_failure_reason(exit_code, sr),
        )
        .await),
    }
}

const TASK_QUERY_MATCH_LIMIT: usize = 8;
const TASK_PENDING_DISPLAY_LIMIT: usize = 50;

fn format_task_query_ambiguity(query: &str, matches: &[astra_services::TaskListItem]) -> String {
    let mut lines = Vec::with_capacity(matches.len() + 2);
    lines.push(format!(
        "task query '{query}' is ambiguous; refine the id or title"
    ));
    for task in matches {
        let short = &task.task_id[..8.min(task.task_id.len())];
        let status = task_list_item_status_label(task);
        lines.push(format!("  {short}  {}  [{status}]", task.title));
    }
    lines.join("\n")
}

pub(crate) async fn handle_task_command(
    arg: &str,
    state: &mut SessionState,
    api: &astra_thin_client::ThinClient,
    profile: Option<&str>,
    token: Option<&str>,
) {
    use astra_services::{TaskCreateRequest, TaskStatus};

    let svc = match &state.task_service {
        Some(s) => s.clone(),
        None => {
            eprintln!(
                "{}",
                "  ⚠ Task service not available (local-only mode).".yellow()
            );
            eprintln!("{}", "  Use /login to enable cloud task tracking.".dim());
            return;
        }
    };

    let user_id = state.ingestion_user_id.as_deref().unwrap_or("local");
    let session_id = state.session_id.as_deref().unwrap_or("no-session");

    let subcmd = arg.split_whitespace().next().unwrap_or("list");
    let sub_arg = arg.strip_prefix(subcmd).unwrap_or("").trim();

    match subcmd {
        "list" | "" => match svc.list_recent_tasks(user_id, None).await {
            Ok(tasks) if tasks.is_empty() => {
                eprintln!(
                    "  {}",
                    "No recent tasks. Use /task run <prompt> to start one.".dim()
                );
            }
            Ok(tasks) => {
                eprintln!(
                    "\n{}",
                    "─── Recent Tasks ─────────────────────────────────".bold()
                );
                for t in &tasks {
                    let icon = task_list_item_status_icon(t);
                    let short_id = &t.task_id[..8.min(t.task_id.len())];
                    let status_label = task_list_item_status_label(t).to_string();
                    let progress = if t.items_total > 0 {
                        format!(" ({}/{})", t.items_done, t.items_total)
                    } else {
                        String::new()
                    };
                    let outcome_suffix = task_list_item_outcome(t)
                        .filter(|outcome| *outcome != astra_services::TaskOutcome::Success)
                        .map(|outcome| format!(" [{}]", outcome.as_str().magenta()))
                        .unwrap_or_default();
                    eprintln!(
                        "  {} {} {} [{}]{}{}",
                        short_id.dim(),
                        icon,
                        t.title,
                        status_label.magenta(),
                        outcome_suffix,
                        progress,
                    );
                }
                eprintln!(
                    "  {}",
                    "Use /task pending for the oldest-first claimable queue.".dim()
                );
                eprintln!();
            }
            Err(e) => eprintln!("{}", format!("  {} {e}", theme::icon_err()).red()),
        },
        "pending" => match svc
            .list_claimable_tasks_for_worker(user_id, TASK_PENDING_DISPLAY_LIMIT)
            .await
        {
            Ok(tasks) if tasks.is_empty() => {
                eprintln!("  {}", "No claimable tasks in the queue.".dim());
            }
            Ok(tasks) => {
                eprintln!(
                    "\n{}",
                    "─── Claimable Queue (oldest first) ─────────────".bold()
                );
                for t in &tasks {
                    let short_id = &t.task_id[..8.min(t.task_id.len())];
                    let progress = if t.items_total > 0 {
                        format!(" ({}/{})", t.items_done, t.items_total)
                    } else {
                        String::new()
                    };
                    let status = task_list_item_claimability_label(t)
                        .unwrap_or_else(|| task_list_item_status_label(t));
                    let icon = task_list_item_claimability_icon(t).unwrap_or("◻");
                    eprintln!(
                        "  {} {} {} [{}]{}",
                        short_id.dim(),
                        icon,
                        t.title,
                        status.magenta(),
                        progress
                    );
                }
                eprintln!();
            }
            Err(e) => eprintln!("{}", format!("  {} {e}", theme::icon_err()).red()),
        },
        "status" if !sub_arg.is_empty() => {
            match find_task_by_query(&*svc, user_id, sub_arg).await {
                Ok(Some(tid)) => match svc.get_task(user_id, &tid).await {
                    Ok(Some(t)) => {
                        let read = load_task_result_read_surface(&t);
                        eprintln!(
                            "\n{}",
                            "─── Task Detail ─────────────────────────────────".bold()
                        );
                        eprintln!("  {:<12} {}", "id:".dim(), t.task_id.as_str().magenta());
                        eprintln!("  {:<12} {}", "title:".dim(), t.title);
                        for field in task_result_header_fields(&read) {
                            eprintln!(
                                "  {:<12} {}",
                                field.label.dim(),
                                render_task_result_header_value(&field)
                            );
                        }
                        eprintln!("  {:<12} {}%", "progress:".dim(), t.progress_pct);
                        if let Some(ref desc) = t.description {
                            eprintln!("  {:<12} {}", "desc:".dim(), desc);
                        }
                        if let Some(ref plan) = t.plan {
                            eprintln!(
                                "  {:<12} {}/{}",
                                "items:".dim(),
                                t.items_done,
                                t.items_total
                            );
                            for st in &plan.subtasks {
                                let icon = match st.status {
                                    TaskStatus::Completed => "✓",
                                    TaskStatus::InProgress => "▶",
                                    _ => "○",
                                };
                                eprintln!("    {} {}", icon, st.title);
                            }
                        }
                        eprintln!();
                    }
                    Ok(None) => {
                        eprintln!("{}", format!("  Task not found: '{sub_arg}'").yellow());
                        eprintln!("{}", "  Use /task list to see available tasks.".dim());
                    }
                    Err(e) => eprintln!("{}", format!("  {} {e}", theme::icon_err()).red()),
                },
                Ok(None) => {
                    eprintln!("{}", format!("  Task not found: '{sub_arg}'").yellow());
                    eprintln!("{}", "  Use /task list to see available tasks.".dim());
                }
                Err(e) => eprintln!("{}", format!("  {} {e}", theme::icon_err()).red()),
            }
        }
        "run" if !sub_arg.is_empty() => {
            let token_str = match token {
                Some(t) => t.to_string(),
                None => {
                    eprintln!(
                        "{}",
                        "  ⚠ No API token available. Use /login first.".yellow()
                    );
                    return;
                }
            };

            // Create task record
            let task_id = match svc
                .create_task(
                    user_id,
                    session_id,
                    TaskCreateRequest {
                        title: format!(
                            "run: {}",
                            if sub_arg.len() > 60 {
                                format!("{}…", sub_arg.chars().take(60).collect::<String>())
                            } else {
                                sub_arg.to_string()
                            }
                        ),
                        description: Some(sub_arg.to_string()),
                        plan: None,
                        parent_task_id: None,
                        project_type: None,
                        goal_pattern: None,
                    },
                )
                .await
            {
                Ok(tid) => tid,
                Err(e) => {
                    eprintln!("{}", format!("  {} {e}", theme::icon_err()).red());
                    return;
                }
            };
            let short_id = task_id[..8.min(task_id.len())].to_string();

            // Clone owned values for the background task
            let api_clone = api.clone();
            let prompt = sub_arg.to_string();
            let bg_profile = profile.map(ToString::to_string);
            let bg_session_id = state.session_id.clone();
            let bg_model = state.model.clone();
            let bg_history = state.history.clone();
            let bg_cli_context = state.cli_context.clone();
            let bg_unified_skill_registry = state.unified_skill_registry.clone();
            let bg_messaging_metrics = state.messaging_metrics.clone();
            let bg_agent_spawner = state.agent_spawner.clone();
            let bg_delegation_engine = state.delegation_engine.clone();
            let bg_observability_hub = state.observability_hub.clone();
            let bg_observability_session = state.observability_session.clone();
            #[cfg(feature = "harness")]
            let bg_harness_sink = state.harness_sink.clone();
            #[cfg(feature = "harness")]
            let bg_harness_trace = state.harness_trace.clone();
            let svc_clone = svc.clone();
            let bg_user_id = user_id.to_string();
            let workspace_root = std::env::current_dir().unwrap_or_default();
            let bg_root_agent_id = format!("task-{task_id}");

            eprintln!(
                "  {} Background task started: {} ({})",
                "▶".magenta(),
                if sub_arg.len() > 50 {
                    format!("{}…", sub_arg.chars().take(50).collect::<String>())
                } else {
                    sub_arg.to_string()
                },
                short_id.dim()
            );
            eprintln!(
                "  {}",
                "Use /task status or /task result to check progress.".dim()
            );

            // Spawn background task
            let bg_task_id = task_id.clone();
            tokio::spawn(async move {
                // Mark in-progress
                let _ = svc_clone
                    .update_status(&bg_user_id, &bg_task_id, TaskStatus::InProgress)
                    .await;

                // Create fresh auto-approve permission manager for background
                let mut perm_manager = PermissionManager::with_load_policy(
                    crate::cli::permission_manager::PermissionMode::Auto,
                    &workspace_root,
                    &crate::cli::permission_manager::PermissionLoadPolicy::HeadlessSafe,
                );
                let mut skill_qt = astra_skills::quality::SkillQualityTracker::new();

                let _modules = crate::cli::session::session_runtime::create_pipeline_modules_quiet(
                    &api_clone, None,
                );

                let result = stream_chat_sse(ChatTurnParams {
                    api: &api_clone,
                    token: &token_str,
                    auth_profile: bg_profile.as_deref(),
                    message: &prompt,
                    user_intent: &prompt,
                    input_runtime_required_texts: &[],
                    input_runtime_volatile_texts: &[],
                    input_work_unit_observations: &[],
                    semantic_query_override: None,
                    session_id: bg_session_id.as_deref(),
                    model_id: None,
                    model: bg_model.as_deref(),
                    provider: None,
                    explain: ExplainMode::Off,
                    render_md: false,
                    history: &bg_history,
                    perm_manager: &mut perm_manager,
                    verbose_mode: false,
                    render_policy: crate::cli::stream::stream_render::RenderPolicy::Silent,
                    cli_context: Some(&bg_cli_context),
                    recent_tools: &[],
                    activated_deferred_tool_names: None,
                    tool_health_entries: &[],
                    resume_restricted_tools: &[],
                    session_lessons: &[],
                    latest_skill_diagnosis: None,
                    latest_turn_quality_feedback: None,
                    unified_skill_registry: &bg_unified_skill_registry,
                    is_plan_subtask: false,
                    plan_subtask_id: None,
                    delegation_engine: bg_delegation_engine.clone(),
                    cancel_token: None,
                    run_control: None,
                    incremental_state: None,
                    plan_assemble_line_release: None,
                    stream_event_tx: None,
                    agent_live_event_sink: None,
                    approval_request_tx: None,
                    ask_user_request_tx: None,
                    plan_review_request_tx: None,
                    mcp_manager: None,
                    skill_quality_tracker: &mut skill_qt,
                    discovered_skills: None,
                    messaging_metrics: bg_messaging_metrics.clone(),
                    agent_spawner: bg_agent_spawner.clone(),
                    root_agent_id: Some(bg_root_agent_id.as_str()),
                    root_mailbox_slot: None,
                    observability_hub: bg_observability_hub.clone(),
                    observability_session: bg_observability_session.clone(),
                    file_journal: None,
                    file_state: None,
                    database_snapshot_journal: None,
                    git_stash_journal: None,
                    git_commit_journal: None,
                    git_worktree_journal: None,
                    session_state_journal: None,
                    task_manager: None,
                    task_notify_tx: None,
                    bg_task_commands: None,
                    bg_task_list_cache: None,
                    bash_detach_slot: None,
                    turn_index: DEFAULT_TURN_INDEX,
                    pipeline_state: None,
                    compaction_state: None,
                    consecutive_context_window_errors: 0,
                    idempotency_cache: None,
                    pre_loaded_messages: None,
                    append_system_prompt: None,
                    session_memory_extractor: None,
                    #[cfg(feature = "harness")]
                    harness_sink: Some(bg_harness_sink.clone()),
                    #[cfg(feature = "harness")]
                    harness_trace: Some(bg_harness_trace.clone()),
                    #[cfg(feature = "harness")]
                    benchmark_profile: None,
                })
                .await;

                let short = &bg_task_id[..8.min(bg_task_id.len())];
                match result {
                    Ok(sr) => {
                        match persist_background_task_result(
                            &*svc_clone,
                            &bg_user_id,
                            &bg_task_id,
                            bg_session_id.clone(),
                            &sr,
                        )
                        .await
                        {
                            Ok(outcome) => {
                                let icon = if outcome == astra_services::TaskOutcome::Partial {
                                    theme::icon_warn()
                                } else {
                                    theme::icon_ok()
                                };
                                let terminal = if outcome == astra_services::TaskOutcome::Partial {
                                    "completed partially"
                                } else {
                                    "completed"
                                };
                                eprintln!(
                                    "\n  {} Background task {} {}. Use /task result {} to view.",
                                    icon,
                                    short.magenta(),
                                    terminal,
                                    short.magenta()
                                );
                            }
                            Err(error) => {
                                eprintln!(
                                    "\n  {} Background task {} failed: {}",
                                    theme::icon_err(),
                                    short.magenta(),
                                    error.red()
                                );
                            }
                        }
                    }
                    Err(e) => {
                        let error = mark_background_task_failed(
                            &*svc_clone,
                            &bg_user_id,
                            &bg_task_id,
                            "turn_error",
                            e.error,
                        )
                        .await;
                        eprintln!(
                            "\n  {} Background task {} failed: {}",
                            theme::icon_err(),
                            short.magenta(),
                            error.red()
                        );
                    }
                }
            });
        }
        "result" if !sub_arg.is_empty() => {
            // Show the full result of a background task.
            match find_task_by_query(&*svc, user_id, sub_arg).await {
                Ok(Some(tid)) => match svc.get_task(user_id, &tid).await {
                    Ok(Some(t)) => {
                        let read = load_task_result_read_surface(&t);
                        let short = &t.task_id[..8.min(t.task_id.len())];
                        eprintln!(
                            "\n{}",
                            format!("─── Task Result ({short}) ─────────────────────────").bold()
                        );
                        eprintln!("  {:<12} {}", "title:".dim(), t.title);
                        for field in task_result_header_fields(&read) {
                            eprintln!(
                                "  {:<12} {}",
                                field.label.dim(),
                                render_task_result_header_value(&field)
                            );
                        }
                        match load_task_result_artifact(&t) {
                            Ok(Some(artifact)) => {
                                eprintln!();
                                eprintln!("{}", artifact.full_text);
                                if let Some(tokens) = artifact.prompt_tokens {
                                    let comp = artifact.completion_tokens;
                                    let tools = artifact.tool_calls_count;
                                    eprintln!(
                                        "\n  {}",
                                        format!("tokens: {tokens}→/{comp}← | tools: {tools}").dim()
                                    );
                                }
                                if let Some(output_file) = artifact.output_file {
                                    eprintln!("  {}", format!("output: {output_file}").dim());
                                }
                            }
                            Ok(None) => {
                                if read.header.is_unfinished {
                                    eprintln!("  {}", read.missing_text.yellow());
                                } else {
                                    eprintln!("  {}", read.missing_text.dim());
                                }
                            }
                            Err(e) => {
                                eprintln!("{}", format!("  {} {e}", theme::icon_err()).red());
                            }
                        }
                        eprintln!();
                    }
                    Ok(None) => {
                        eprintln!("{}", format!("  Task not found: '{sub_arg}'").yellow());
                    }
                    Err(e) => eprintln!("{}", format!("  {} {e}", theme::icon_err()).red()),
                },
                Ok(None) => {
                    eprintln!("{}", format!("  Task not found: '{sub_arg}'").yellow());
                    eprintln!("{}", "  Use /task list to see available tasks.".dim());
                }
                Err(e) => eprintln!("{}", format!("  {} {e}", theme::icon_err()).red()),
            }
        }
        _ => {
            eprintln!("  Usage: /task [list | pending | status <id> | run <prompt> | result <id>]");
        }
    }
}

/// Resolve a user task query using the shared service-side matching semantics.
///
/// Returns `Ok(None)` when nothing matches, `Ok(Some(task_id))` when the best
/// match tier is unique, and `Err(...)` when the query is ambiguous.
pub(crate) async fn find_task_by_query(
    svc: &dyn astra_services::TaskService,
    user_id: &str,
    query: &str,
) -> Result<Option<String>, String> {
    let query = query.trim();
    let matches = svc
        .search_tasks(user_id, query, TASK_QUERY_MATCH_LIMIT)
        .await?;
    match matches.as_slice() {
        [] => Ok(None),
        [task] => Ok(Some(task.task_id.clone())),
        _ => Err(format_task_query_ambiguity(query, &matches)),
    }
}

#[cfg(test)]
mod tests {
    use super::{find_task_by_query, persist_background_task_result};
    use crate::cli::surface::task_checkpoint_surface::{
        task_checkpoint_surface, task_status_icon, task_status_label,
    };
    use crate::lock_recovery::LockRecovery;
    use crate::tests::stub_stream_result;
    use async_trait::async_trait;
    use std::sync::{Arc, Mutex};

    #[derive(Default)]
    struct MockTaskState {
        saved_checkpoint: bool,
        completed: bool,
        completed_outcome: Option<astra_services::TaskOutcome>,
        failed_error: Option<String>,
        checkpoint: Option<astra_services::TaskCheckpoint>,
    }

    struct MockTaskService {
        save_checkpoint_error: Option<String>,
        complete_task_error: Option<String>,
        fail_task_error: Option<String>,
        state: Arc<Mutex<MockTaskState>>,
    }

    #[async_trait]
    impl astra_services::TaskService for MockTaskService {
        async fn create_task(
            &self,
            _: &str,
            _: &str,
            _: astra_services::TaskCreateRequest,
        ) -> Result<String, String> {
            unimplemented!()
        }

        async fn get_task(
            &self,
            _: &str,
            _: &str,
        ) -> Result<Option<astra_services::TaskRecord>, String> {
            unimplemented!()
        }

        async fn list_recent_tasks(
            &self,
            _: &str,
            _: Option<astra_services::TaskStatus>,
        ) -> Result<Vec<astra_services::TaskListItem>, String> {
            unimplemented!()
        }

        async fn list_recent_tasks_for_session(
            &self,
            _: &str,
            _: &str,
            _: Option<astra_services::TaskStatus>,
        ) -> Result<Vec<astra_services::TaskListItem>, String> {
            unimplemented!()
        }

        async fn search_tasks(
            &self,
            _: &str,
            _: &str,
            _: usize,
        ) -> Result<Vec<astra_services::TaskListItem>, String> {
            unimplemented!()
        }

        async fn list_claimable_tasks_for_worker(
            &self,
            _: &str,
            _: usize,
        ) -> Result<Vec<astra_services::TaskListItem>, String> {
            unimplemented!()
        }

        async fn update_status(
            &self,
            _: &str,
            _: &str,
            _: astra_services::TaskStatus,
        ) -> Result<(), String> {
            unimplemented!()
        }

        async fn update_progress(
            &self,
            _: &str,
            _: &str,
            _: u32,
            _: u32,
            _: u32,
        ) -> Result<(), String> {
            unimplemented!()
        }

        async fn save_checkpoint(
            &self,
            _: &str,
            _: &str,
            checkpoint: &astra_services::TaskCheckpoint,
        ) -> Result<(), String> {
            if let Some(error) = &self.save_checkpoint_error {
                return Err(error.clone());
            }
            let mut state = self.state.lock_recover();
            state.saved_checkpoint = true;
            state.checkpoint = Some(checkpoint.clone());
            Ok(())
        }

        async fn update_plan(
            &self,
            _: &str,
            _: &str,
            _: &astra_services::TaskPlan,
        ) -> Result<(), String> {
            unimplemented!()
        }

        async fn fail_task(&self, _: &str, _: &str, error: &str) -> Result<(), String> {
            if let Some(fail_error) = &self.fail_task_error {
                return Err(fail_error.clone());
            }
            self.state.lock_recover().failed_error = Some(error.to_string());
            Ok(())
        }

        async fn complete_task(&self, _: &str, _: &str) -> Result<(), String> {
            if let Some(error) = &self.complete_task_error {
                return Err(error.clone());
            }
            self.state.lock_recover().completed = true;
            Ok(())
        }

        async fn complete_plan_run(
            &self,
            _: &str,
            _: &str,
            _: u32,
            _: u32,
            _: u32,
            outcome: astra_services::TaskOutcome,
        ) -> Result<(), String> {
            self.state.lock_recover().completed_outcome = Some(outcome);
            Ok(())
        }

        async fn complete_task_with_outcome(
            &self,
            _: &str,
            _: &str,
            outcome: astra_services::TaskOutcome,
        ) -> Result<(), String> {
            self.state.lock_recover().completed_outcome = Some(outcome);
            Ok(())
        }

        async fn record_feedback(
            &self,
            _: &str,
            _: &str,
            _: u8,
            _: astra_services::TaskOutcome,
            _: Option<i32>,
        ) -> Result<(), String> {
            unimplemented!()
        }

        async fn increment_replan_count(&self, _: &str, _: &str) -> Result<(), String> {
            unimplemented!()
        }

        async fn extract_template(
            &self,
            _: &str,
            _: &str,
            _: &str,
        ) -> Result<Option<String>, String> {
            unimplemented!()
        }

        async fn recommend_templates(
            &self,
            _: &str,
            _: &str,
            _: Option<&str>,
            _: usize,
        ) -> Result<Vec<astra_services::task_orchestrator::TemplateRecommendation>, String>
        {
            unimplemented!()
        }

        async fn record_template_usage(&self, _: &str, _: &str) -> Result<(), String> {
            unimplemented!()
        }

        async fn get_learning_stats(
            &self,
            _: &str,
            _: &str,
        ) -> Result<astra_services::task_orchestrator::LearningStats, String> {
            unimplemented!()
        }
    }

    struct SearchOnlyTaskService {
        results: Vec<astra_services::TaskListItem>,
        users: Arc<Mutex<Vec<String>>>,
    }

    #[async_trait]
    impl astra_services::TaskService for SearchOnlyTaskService {
        async fn create_task(
            &self,
            _: &str,
            _: &str,
            _: astra_services::TaskCreateRequest,
        ) -> Result<String, String> {
            unimplemented!()
        }

        async fn get_task(
            &self,
            _: &str,
            _: &str,
        ) -> Result<Option<astra_services::TaskRecord>, String> {
            unimplemented!()
        }

        async fn list_recent_tasks(
            &self,
            _: &str,
            _: Option<astra_services::TaskStatus>,
        ) -> Result<Vec<astra_services::TaskListItem>, String> {
            panic!("find_task_by_query should not call list_recent_tasks")
        }

        async fn list_recent_tasks_for_session(
            &self,
            _: &str,
            _: &str,
            _: Option<astra_services::TaskStatus>,
        ) -> Result<Vec<astra_services::TaskListItem>, String> {
            unimplemented!()
        }

        async fn search_tasks(
            &self,
            user_id: &str,
            _: &str,
            _: usize,
        ) -> Result<Vec<astra_services::TaskListItem>, String> {
            self.users.lock_recover().push(user_id.to_string());
            Ok(self.results.clone())
        }

        async fn list_claimable_tasks_for_worker(
            &self,
            _: &str,
            _: usize,
        ) -> Result<Vec<astra_services::TaskListItem>, String> {
            unimplemented!()
        }

        async fn update_status(
            &self,
            _: &str,
            _: &str,
            _: astra_services::TaskStatus,
        ) -> Result<(), String> {
            unimplemented!()
        }

        async fn update_progress(
            &self,
            _: &str,
            _: &str,
            _: u32,
            _: u32,
            _: u32,
        ) -> Result<(), String> {
            unimplemented!()
        }

        async fn save_checkpoint(
            &self,
            _: &str,
            _: &str,
            _: &astra_services::TaskCheckpoint,
        ) -> Result<(), String> {
            unimplemented!()
        }

        async fn update_plan(
            &self,
            _: &str,
            _: &str,
            _: &astra_services::TaskPlan,
        ) -> Result<(), String> {
            unimplemented!()
        }

        async fn fail_task(&self, _: &str, _: &str, _: &str) -> Result<(), String> {
            unimplemented!()
        }

        async fn complete_task(&self, _: &str, _: &str) -> Result<(), String> {
            unimplemented!()
        }

        async fn complete_plan_run(
            &self,
            _: &str,
            _: &str,
            _: u32,
            _: u32,
            _: u32,
            _: astra_services::TaskOutcome,
        ) -> Result<(), String> {
            unimplemented!()
        }

        async fn complete_task_with_outcome(
            &self,
            _: &str,
            _: &str,
            _: astra_services::TaskOutcome,
        ) -> Result<(), String> {
            unimplemented!()
        }

        async fn record_feedback(
            &self,
            _: &str,
            _: &str,
            _: u8,
            _: astra_services::TaskOutcome,
            _: Option<i32>,
        ) -> Result<(), String> {
            unimplemented!()
        }

        async fn increment_replan_count(&self, _: &str, _: &str) -> Result<(), String> {
            unimplemented!()
        }

        async fn extract_template(
            &self,
            _: &str,
            _: &str,
            _: &str,
        ) -> Result<Option<String>, String> {
            unimplemented!()
        }

        async fn recommend_templates(
            &self,
            _: &str,
            _: &str,
            _: Option<&str>,
            _: usize,
        ) -> Result<Vec<astra_services::task_orchestrator::TemplateRecommendation>, String>
        {
            unimplemented!()
        }

        async fn record_template_usage(&self, _: &str, _: &str) -> Result<(), String> {
            unimplemented!()
        }

        async fn get_learning_stats(
            &self,
            _: &str,
            _: &str,
        ) -> Result<astra_services::task_orchestrator::LearningStats, String> {
            unimplemented!()
        }
    }

    #[test]
    fn task_status_label_prefers_partial_outcome_over_completed_status() {
        assert_eq!(
            task_status_label(
                astra_services::TaskStatus::Completed,
                Some(astra_services::TaskOutcome::Partial),
            ),
            "partial"
        );
        assert_eq!(
            task_status_label(
                astra_services::TaskStatus::Completed,
                Some(astra_services::TaskOutcome::Success),
            ),
            "completed"
        );
    }

    #[test]
    fn task_status_icon_marks_partial_completed_tasks() {
        assert_eq!(
            task_status_icon(
                astra_services::TaskStatus::Completed,
                Some(astra_services::TaskOutcome::Partial),
                3,
                3,
            ),
            "△"
        );
        assert_eq!(
            task_status_icon(
                astra_services::TaskStatus::Completed,
                Some(astra_services::TaskOutcome::Success),
                3,
                3,
            ),
            "✓"
        );
    }

    #[test]
    fn task_checkpoint_surface_reads_machine_readable_task_metadata() {
        let checkpoint = astra_services::TaskCheckpoint {
            active_subtask_id: None,
            turn: 0,
            session_id: Some("sess-1".into()),
            state: serde_json::Map::from_iter([
                ("error_kind".to_string(), serde_json::json!("partial")),
                ("final_state".to_string(), serde_json::json!("interrupted")),
                (
                    "interruption_kind".to_string(),
                    serde_json::json!("budget_exhausted"),
                ),
                (
                    "persistence_error".to_string(),
                    serde_json::json!("write task output: permission denied"),
                ),
            ]),
        };
        let checkpoint = task_checkpoint_surface(&checkpoint);

        assert_eq!(checkpoint.error_kind, Some("partial"));
        assert_eq!(checkpoint.final_state, Some("interrupted"));
        assert_eq!(checkpoint.interruption_kind, Some("budget_exhausted"));
        assert_eq!(
            checkpoint.persistence_error,
            Some("write task output: permission denied")
        );
    }

    #[tokio::test]
    async fn persist_background_task_result_fails_closed_when_checkpoint_save_fails() {
        let state = Arc::new(Mutex::new(MockTaskState::default()));
        let svc = MockTaskService {
            save_checkpoint_error: Some("disk full".into()),
            complete_task_error: None,
            fail_task_error: None,
            state: state.clone(),
        };
        let mut sr = stub_stream_result("answer");
        sr.prompt_tokens = 10;
        sr.completion_tokens = 20;

        let err =
            persist_background_task_result(&svc, "test-user", "task-1", Some("sess-1".into()), &sr)
                .await
                .unwrap_err();

        let snapshot = state.lock_recover();
        assert!(snapshot.failed_error.is_some());
        assert!(!snapshot.completed);
        assert_eq!(
            crate::cli::surface::task_checkpoint_surface::parse_task_failure_message(
                snapshot.failed_error.as_deref().unwrap()
            ),
            (
                Some("persistence_error"),
                "failed to save background task result: disk full"
            )
        );
        assert!(err.contains("failed to save background task result"));
    }

    #[tokio::test]
    async fn persist_background_task_result_marks_complete_after_checkpoint_save() {
        let state = Arc::new(Mutex::new(MockTaskState::default()));
        let svc = MockTaskService {
            save_checkpoint_error: None,
            complete_task_error: None,
            fail_task_error: None,
            state: state.clone(),
        };
        let mut sr = stub_stream_result("answer");
        sr.prompt_tokens = 10;
        sr.completion_tokens = 20;
        sr.tool_calls_count = 1;

        let outcome =
            persist_background_task_result(&svc, "test-user", "task-1", Some("sess-1".into()), &sr)
                .await
                .unwrap();

        let snapshot = state.lock_recover();
        assert_eq!(outcome, astra_services::TaskOutcome::Success);
        assert!(snapshot.saved_checkpoint);
        assert!(snapshot.completed);
        assert!(snapshot.failed_error.is_none());
        let checkpoint = snapshot.checkpoint.as_ref().expect("checkpoint saved");
        assert_eq!(checkpoint.state["final_state"], "completed");
        assert!(checkpoint.state.get("output_file").is_none());
    }

    #[tokio::test]
    async fn persist_background_task_result_marks_partial_outcome_for_interrupted_turn() {
        let state = Arc::new(Mutex::new(MockTaskState::default()));
        let svc = MockTaskService {
            save_checkpoint_error: None,
            complete_task_error: None,
            fail_task_error: None,
            state: state.clone(),
        };
        let mut sr = stub_stream_result("partial answer");
        sr.prompt_tokens = 10;
        sr.completion_tokens = 20;
        sr.tool_calls_count = 1;
        sr.final_state = "interrupted".into();
        sr.interruption_kind = Some("budget_exhausted".into());

        let outcome =
            persist_background_task_result(&svc, "test-user", "task-1", Some("sess-1".into()), &sr)
                .await
                .unwrap();

        let snapshot = state.lock_recover();
        assert_eq!(outcome, astra_services::TaskOutcome::Partial);
        assert!(snapshot.saved_checkpoint);
        assert!(!snapshot.completed);
        assert_eq!(
            snapshot.completed_outcome,
            Some(astra_services::TaskOutcome::Partial)
        );
        let checkpoint = snapshot.checkpoint.as_ref().expect("checkpoint saved");
        assert_eq!(checkpoint.state["final_state"], "interrupted");
        assert_eq!(checkpoint.state["interruption_kind"], "budget_exhausted");
    }

    #[tokio::test]
    async fn persist_background_task_result_fails_on_persistence_degradation() {
        let state = Arc::new(Mutex::new(MockTaskState::default()));
        let svc = MockTaskService {
            save_checkpoint_error: None,
            complete_task_error: None,
            fail_task_error: None,
            state: state.clone(),
        };
        let mut sr = stub_stream_result("answer");
        sr.prompt_tokens = 10;
        sr.completion_tokens = 20;
        sr.tool_calls_count = 1;
        sr.session_persistence_error = Some("failed to append turn event".into());

        let err =
            persist_background_task_result(&svc, "test-user", "task-1", Some("sess-1".into()), &sr)
                .await
                .unwrap_err();

        let snapshot = state.lock_recover();
        assert!(snapshot.saved_checkpoint);
        assert!(!snapshot.completed);
        assert!(snapshot.completed_outcome.is_none());
        assert_eq!(
            crate::cli::surface::task_checkpoint_surface::parse_task_failure_message(
                snapshot.failed_error.as_deref().unwrap()
            ),
            (Some("persistence_error"), "failed to append turn event")
        );
        let checkpoint = snapshot.checkpoint.as_ref().expect("checkpoint saved");
        assert_eq!(checkpoint.state["error_kind"], "persistence_error");
        assert_eq!(
            checkpoint.state["persistence_error"],
            "failed to append turn event"
        );
        assert_eq!(err, "failed to append turn event");
    }

    #[tokio::test]
    async fn find_task_by_query_uses_service_search_and_records_user_scope() {
        let users = Arc::new(Mutex::new(Vec::new()));
        let svc = SearchOnlyTaskService {
            results: vec![astra_services::TaskListItem {
                task_id: "task-123".into(),
                title: "Build auth".into(),
                session_id: Some("sess-1".into()),
                status: astra_services::TaskStatus::Completed,
                progress_pct: 100,
                items_done: 1,
                items_total: 1,
                created_at: "2025-01-01T00:00:00Z".into(),
                updated_at: "2025-01-01T00:00:00Z".into(),
                completed_at: Some("2025-01-01T00:00:00Z".into()),
                outcome: Some(astra_services::TaskOutcome::Success),
                error_message: None,
                project_type: None,
                claimability: None,
            }],
            users: users.clone(),
        };

        let found = find_task_by_query(&svc, "user-123", "Build").await.unwrap();
        assert_eq!(found.as_deref(), Some("task-123"));
        assert_eq!(users.lock_recover().as_slice(), ["user-123"]);
    }

    #[tokio::test]
    async fn find_task_by_query_fails_closed_on_ambiguous_matches() {
        let svc = SearchOnlyTaskService {
            results: vec![
                astra_services::TaskListItem {
                    task_id: "task-111".into(),
                    title: "Refactor auth module".into(),
                    session_id: Some("sess-1".into()),
                    status: astra_services::TaskStatus::Completed,
                    progress_pct: 100,
                    items_done: 1,
                    items_total: 1,
                    created_at: "2025-01-01T00:00:00Z".into(),
                    updated_at: "2025-01-02T00:00:00Z".into(),
                    completed_at: Some("2025-01-02T00:00:00Z".into()),
                    outcome: Some(astra_services::TaskOutcome::Success),
                    error_message: None,
                    project_type: None,
                    claimability: None,
                },
                astra_services::TaskListItem {
                    task_id: "task-222".into(),
                    title: "Refactor auth tests".into(),
                    session_id: Some("sess-2".into()),
                    status: astra_services::TaskStatus::InProgress,
                    progress_pct: 50,
                    items_done: 1,
                    items_total: 2,
                    created_at: "2025-01-01T00:00:00Z".into(),
                    updated_at: "2025-01-03T00:00:00Z".into(),
                    completed_at: None,
                    outcome: None,
                    error_message: None,
                    project_type: None,
                    claimability: None,
                },
            ],
            users: Arc::new(Mutex::new(Vec::new())),
        };

        let err = find_task_by_query(&svc, "user-123", "auth")
            .await
            .unwrap_err();
        assert!(err.contains("task query 'auth' is ambiguous"));
        assert!(err.contains("task-111"));
        assert!(err.contains("task-222"));
    }
}
