#![allow(unused_imports)]
use super::*;

pub(super) async fn handle_task_command(
    arg: &str,
    state: &mut ReplState,
    api: &astra_thin_client::ThinClient,
    profile: Option<&str>,
    token: Option<&str>,
) {
    use astra_services::{TaskCreateRequest, TaskService, TaskStatus};

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
        "list" | "" => match svc.list_tasks(user_id, None).await {
            Ok(tasks) if tasks.is_empty() => {
                eprintln!(
                    "  {}",
                    "No tasks. Use /task add <title> to create one.".dim()
                );
            }
            Ok(tasks) => {
                eprintln!(
                    "\n{}",
                    "─── Tasks ───────────────────────────────────────".bold()
                );
                for t in &tasks {
                    let icon = match t.status {
                        TaskStatus::Completed
                            if t.items_total > 0 && t.items_done < t.items_total =>
                        {
                            "△"
                        }
                        TaskStatus::Completed => "✓",
                        TaskStatus::Failed => "✗",
                        TaskStatus::InProgress => "▶",
                        TaskStatus::Paused => "⏸",
                        _ => "○",
                    };
                    let short_id = &t.task_id[..8.min(t.task_id.len())];
                    let status_label = match t.status {
                        TaskStatus::Completed
                            if t.items_total > 0 && t.items_done < t.items_total =>
                        {
                            "partial".to_string()
                        }
                        _ => t.status.as_str().to_string(),
                    };
                    let progress = if t.items_total > 0 {
                        format!(" ({}/{})", t.items_done, t.items_total)
                    } else {
                        String::new()
                    };
                    eprintln!(
                        "  {} {} {} [{}]{}",
                        short_id.dim(),
                        icon,
                        t.title,
                        status_label.cyan(),
                        progress,
                    );
                }
                eprintln!();
            }
            Err(e) => eprintln!("{}", format!("  {} {e}", theme::icon_err()).red()),
        },
        "add" if !sub_arg.is_empty() => {
            match svc
                .create_task(
                    user_id,
                    session_id,
                    TaskCreateRequest {
                        title: sub_arg.to_string(),
                        description: None,
                        plan: None,
                        parent_task_id: None,
                        project_type: None,
                        goal_pattern: None,
                    },
                )
                .await
            {
                Ok(tid) => {
                    let short = &tid[..8.min(tid.len())];
                    eprintln!(
                        "  {} Task created: {} ({})",
                        theme::icon_ok(),
                        sub_arg,
                        short.dim()
                    );
                }
                Err(e) => eprintln!("{}", format!("  {} {e}", theme::icon_err()).red()),
            }
        }
        "done" if !sub_arg.is_empty() => {
            // Find task by prefix match on task_id or title
            match find_task_by_query(&*svc, user_id, sub_arg).await {
                Ok(Some(tid)) => match svc.complete_task(&tid).await {
                    Ok(()) => eprintln!("  {} Task completed: {}", theme::icon_ok(), sub_arg),
                    Err(e) => eprintln!("{}", format!("  {} {e}", theme::icon_err()).red()),
                },
                Ok(None) => {
                    eprintln!("{}", format!("  Task not found: '{sub_arg}'").yellow());
                    eprintln!("{}", "  Use /task list to see available tasks.".dim());
                }
                Err(e) => eprintln!("{}", format!("  {} {e}", theme::icon_err()).red()),
            }
        }
        "status" if !sub_arg.is_empty() => {
            match find_task_by_query(&*svc, user_id, sub_arg).await {
                Ok(Some(tid)) => match svc.get_task(&tid).await {
                    Ok(Some(t)) => {
                        eprintln!(
                            "\n{}",
                            "─── Task Detail ─────────────────────────────────".bold()
                        );
                        eprintln!("  {:<12} {}", "id:".dim(), t.task_id.cyan());
                        eprintln!("  {:<12} {}", "title:".dim(), t.title);
                        let detail_status_label = match t.status {
                            TaskStatus::Completed
                                if t.items_total > 0 && t.items_done < t.items_total =>
                            {
                                "partial"
                            }
                            _ => t.status.as_str(),
                        };
                        eprintln!("  {:<12} {}", "status:".dim(), detail_status_label.cyan());
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
                        if let Some(ref err) = t.error_message {
                            eprintln!("  {:<12} {}", "error:".dim(), err.as_str().red());
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
            let bg_unified_skill_registry = state.unified_skill_registry.clone();
            let bg_skill_search = state.skill_search.clone();
            let bg_messaging_metrics = state.messaging_metrics.clone();
            let bg_agent_spawner = state.agent_spawner.clone();
            let bg_delegation_engine = state.delegation_engine.clone();
            let bg_observability_hub = state.observability_hub.clone();
            let bg_observability_session = state.observability_session.clone();
            let bg_evolution_service = state.evolution_service.clone();
            #[cfg(feature = "harness")]
            let bg_harness_sink = state.harness_sink.clone();
            #[cfg(feature = "harness")]
            let bg_harness_trace = state.harness_trace.clone();
            let svc_clone = svc.clone();
            let workspace_root = std::env::current_dir().unwrap_or_default();
            let bg_root_agent_id = format!("task-{task_id}");

            eprintln!(
                "  {} Background task started: {} ({})",
                "▶".cyan(),
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
                    .update_status(&bg_task_id, TaskStatus::InProgress)
                    .await;

                // Create fresh auto-approve permission manager for background
                let mut perm_manager = PermissionManager::with_project(true, &workspace_root);
                let mut skill_qt = astra_skills::quality::SkillQualityTracker::new();

                // Create a fresh tool selector for the background task
                let (selector, _modules) = create_tool_selector_quiet(&api_clone, None);

                let result = stream_chat_sse(ChatTurnParams {
                    api: &api_clone,
                    token: &token_str,
                    auth_profile: bg_profile.as_deref(),
                    message: &prompt,
                    session_id: bg_session_id.as_deref(),
                    model: bg_model.as_deref(),
                    provider: None,
                    explain: ExplainMode::Off,
                    render_md: false,
                    history: &bg_history,
                    perm_manager: &mut perm_manager,
                    verbose_mode: false,
                    render_policy: crate::stream_render::RenderPolicy::Silent,
                    selector: &*selector,
                    recent_tools: &[],
                    tool_health_entries: &[],
                    session_lessons: &[],
                    latest_skill_diagnosis: None,
                    unified_skill_registry: &bg_unified_skill_registry,
                    plan_only_chat: false,
                    is_plan_subtask: false,
                    plan_subtask_id: None,
                    delegation_engine: bg_delegation_engine.clone(),
                    cancel_token: None,
                    plan_assemble_line_release: None,
                    stream_event_tx: None,
                    approval_request_tx: None,
                    mcp_manager: None,
                    skill_search: &bg_skill_search,
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
                    runtime_continuity: None,
                    turn_index: 0,
                    evolution_service: bg_evolution_service.clone(),
                pipeline_state: None,
            pre_loaded_messages: None,
                    append_system_prompt: None,
                    #[cfg(feature = "harness")]
                    harness_sink: Some(bg_harness_sink.clone()),
                    #[cfg(feature = "harness")]
                    harness_trace: Some(bg_harness_trace.clone()),
                })
                .await;

                let short = &bg_task_id[..8.min(bg_task_id.len())];
                match result {
                    Ok(sr) => {
                        // Store result in checkpoint state map
                        let mut state_map = serde_json::Map::new();
                        state_map.insert(
                            "full_text".to_string(),
                            serde_json::Value::String(sr.full_text.clone()),
                        );
                        state_map.insert(
                            "prompt_tokens".to_string(),
                            serde_json::json!(sr.prompt_tokens),
                        );
                        state_map.insert(
                            "completion_tokens".to_string(),
                            serde_json::json!(sr.completion_tokens),
                        );
                        state_map.insert(
                            "tool_calls_count".to_string(),
                            serde_json::json!(sr.tool_calls_count),
                        );
                        let _ = svc_clone
                            .save_checkpoint(
                                &bg_task_id,
                                &astra_services::task_orchestrator::TaskCheckpoint {
                                    active_subtask_id: None,
                                    turn: 0,
                                    session_id: bg_session_id.clone(),
                                    state: state_map,
                                },
                            )
                            .await;
                        let _ = svc_clone.complete_task(&bg_task_id).await;
                        eprintln!(
                            "\n  {} Background task {} completed. Use /task result {} to view.",
                            theme::icon_ok(),
                            short.cyan(),
                            short.cyan()
                        );
                    }
                    Err(e) => {
                        let _ = svc_clone.fail_task(&bg_task_id, &e.error).await;
                        eprintln!(
                            "\n  {} Background task {} failed: {}",
                            theme::icon_err(),
                            short.cyan(),
                            e.error.red()
                        );
                    }
                }
            });
        }
        "result" if !sub_arg.is_empty() => {
            // Show the full result of a background task
            match find_task_by_query(&*svc, user_id, sub_arg).await {
                Ok(Some(tid)) => match svc.get_task(&tid).await {
                    Ok(Some(t)) => {
                        let short = &t.task_id[..8.min(t.task_id.len())];
                        eprintln!(
                            "\n{}",
                            format!("─── Task Result ({short}) ─────────────────────────").bold()
                        );
                        eprintln!("  {:<12} {}", "title:".dim(), t.title);
                        eprintln!("  {:<12} {}", "status:".dim(), t.status.as_str().cyan());
                        if let Some(ref err) = t.error_message {
                            eprintln!("  {:<12} {}", "error:".dim(), err.as_str().red());
                        }
                        // Print checkpoint data (the full_text from the agent)
                        let mut found_result = false;
                        if let Some(ref cp) = t.checkpoint {
                            if let Some(full_text) =
                                cp.state.get("full_text").and_then(|v| v.as_str())
                            {
                                found_result = true;
                                eprintln!();
                                eprintln!("{full_text}");
                                if let Some(tokens) =
                                    cp.state.get("prompt_tokens").and_then(|v| v.as_u64())
                                {
                                    let comp = cp
                                        .state
                                        .get("completion_tokens")
                                        .and_then(|v| v.as_u64())
                                        .unwrap_or(0);
                                    let tools = cp
                                        .state
                                        .get("tool_calls_count")
                                        .and_then(|v| v.as_u64())
                                        .unwrap_or(0);
                                    eprintln!(
                                        "\n  {}",
                                        format!("tokens: {tokens}→/{comp}← | tools: {tools}").dim()
                                    );
                                }
                            }
                        }
                        if !found_result {
                            match t.status {
                                TaskStatus::InProgress | TaskStatus::Pending => {
                                    eprintln!("  {}", "Task is still running…".yellow());
                                }
                                _ => {
                                    eprintln!("  {}", "No result data available.".dim());
                                }
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
            eprintln!(
                "  Usage: /task [list | add <title> | done <id> | status <id> | run <prompt> | result <id>]"
            );
        }
    }
}

/// Find a task by prefix match on task_id or substring match on title.
pub(super) async fn find_task_by_query(
    svc: &dyn astra_services::TaskService,
    user_id: &str,
    query: &str,
) -> Result<Option<String>, String> {
    let tasks = svc.list_tasks(user_id, None).await?;
    // Exact or prefix match on task_id
    if let Some(t) = tasks
        .iter()
        .find(|t| t.task_id == query || t.task_id.starts_with(query))
    {
        return Ok(Some(t.task_id.clone()));
    }
    // Substring match on title (case-insensitive)
    let q_lower = query.to_lowercase();
    if let Some(t) = tasks
        .iter()
        .find(|t| t.title.to_lowercase().contains(&q_lower))
    {
        return Ok(Some(t.task_id.clone()));
    }
    Ok(None)
}
