use super::session_continuation::*;
use crate::cli::arg_render::{
    apply_system_prompt, join_words, render_agent_args, render_bug_args, render_debug_args,
    render_diff_args, render_grep_args, render_memory_args, render_messaging_args,
    render_permissions_args, render_review_args, render_task_args, render_team_args,
};
use crate::cli::auth_flow::*;
use crate::cli::chat_turn::is_auth_error;
use crate::cli::cli_args::*;
use crate::cli::cli_utils::*;
use crate::cli::config_manager::{
    execute_config_command, latest_artifact_id, resolve_download_output_path,
    resolve_remote_session_id, write_downloaded_capture,
};
use crate::cli::interactive_chat::run_interactive_chat;
use crate::cli::mcp_config::execute_mcp_command;
use crate::cli::permission_manager::{PermissionManager, PermissionMode};
use crate::cli::project_instructions::*;
use crate::cli::session_runtime;
use crate::cli::session_runtime::*;
use crate::cli::session_state::*;
use crate::cli::skill_catalog::{
    SkillCatalogFilter, list_skill_record_from_registry, load_skill_record_from_registry,
    normalize_source_filter,
};
use crate::cli::slash_bug::*;
use crate::cli::slash_debug::*;
use crate::cli::slash_info::*;
use crate::cli::slash_memory::*;
use crate::cli::slash_messaging::*;
use crate::cli::streaming_types::*;
use crate::cli::{
    agent_loader, cli_utils, delegate_subrun, diff_presenter, journal_diff, journal_digest,
    journal_tree, slash_agent, slash_inspect, slash_task, slash_team, slash_telemetry, theme,
};
use astra_thin_client::paths;
use clap::CommandFactory;
use crossterm::{style::Stylize, terminal};
use std::{
    fs,
    io::{Read, Write},
};

/// Exit codes for CLI commands (for scripting integration)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ExitCode {
    /// Success (0)
    Success = 0,
    /// Tool execution failure (1) - at least one tool call failed
    ToolFailure = 1,
    /// Force stop (2) - agent was force-stopped due to errors/stalls
    ForceStop = 2,
    /// API/network error (3) - failed to communicate with server
    ApiError = 3,
}

async fn start_http_server(host: &str, port: u16) -> Result<(), String> {
    let addr: std::net::SocketAddr = format!("{host}:{port}")
        .parse()
        .map_err(|e| format!("Invalid listen address: {e}"))?;
    eprintln!(
        "  {} {} on {}",
        "▸".bold().magenta(),
        "Starting API server".bold(),
        addr.to_string().magenta()
    );
    astra_runtime::serve(addr)
        .await
        .map_err(|e| format!("API server failed to start: {e}"))
}

impl From<ExitCode> for i32 {
    fn from(code: ExitCode) -> i32 {
        code as i32
    }
}

fn maybe_load_project_instructions(state: &mut SessionState) {
    state.project_instructions = discover_project_instructions();
}

fn maybe_wire_delegation_engine(
    state: &mut SessionState,
    api: &astra_thin_client::ThinClient,
    token: &str,
) {
    let project_root = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    let executor = delegate_subrun::CliDelegateSubRunExecutor::new(
        api.clone(),
        token.to_string(),
        state.model.clone(),
        project_root.clone(),
        state.perm_manager.mode(),
        None,
    );
    let mut registry = astra_services::AgentProfileRegistry::new();
    delegate_subrun::register_default_agents(&mut registry);
    let _ = agent_loader::load_and_merge(&project_root, &mut registry);
    let registry = std::sync::Arc::new(tokio::sync::RwLock::new(registry));
    let run_store = std::sync::Arc::new(astra_services::runs::InMemoryRunStateStore::default());
    let engine = astra_runtime::server::delegation::engine::DelegationEngine::with_executor(
        registry,
        std::sync::Arc::new(astra_runtime::server::run::engine::RunEngine::new(
            run_store,
        )),
        std::sync::Arc::new(astra_runtime::server::delegation::engine::DelegationTracker::new()),
        std::sync::Arc::new(executor),
    );
    state.delegation_engine = Some(std::sync::Arc::new(engine));
}

fn task_run_title(prompt: &str) -> String {
    let summary = if prompt.chars().count() > 60 {
        format!("{}...", prompt.chars().take(60).collect::<String>())
    } else {
        prompt.to_string()
    };
    format!("run: {summary}")
}

fn task_output_path(task_id: &str) -> Result<std::path::PathBuf, String> {
    if !task_id
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Err(format!("unsafe task id for output path: {task_id}"));
    }
    let dir = dirs::home_dir()
        .unwrap_or_else(|| std::path::PathBuf::from("."))
        .join(".astra")
        .join("tasks")
        .join("outputs");
    std::fs::create_dir_all(&dir).map_err(|e| format!("create task output dir: {e}"))?;
    Ok(dir.join(format!("{task_id}.output")))
}

fn write_task_output(task_id: &str, text: &str) -> Result<std::path::PathBuf, String> {
    let path = task_output_path(task_id)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&path)
            .map_err(|e| format!("open task output: {e}"))?;
        file.write_all(text.as_bytes())
            .map_err(|e| format!("write task output: {e}"))?;
    }
    #[cfg(not(unix))]
    {
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(&path)
            .map_err(|e| format!("open task output: {e}"))?;
        file.write_all(text.as_bytes())
            .map_err(|e| format!("write task output: {e}"))?;
    }
    Ok(path)
}

fn emit_task_event(enabled: bool, value: serde_json::Value) {
    if enabled {
        if let Ok(line) = serde_json::to_string(&value) {
            eprintln!("{line}");
        }
    }
}

struct HeadlessTaskInput {
    task_id: String,
    prompt: String,
    svc: std::sync::Arc<dyn astra_services::TaskService>,
    session_id: Option<String>,
}

#[derive(Debug, Clone, Copy)]
struct HeadlessTaskOptions {
    json: bool,
    quiet: bool,
    stream_events: bool,
    print_started: bool,
}

async fn execute_headless_task_body(
    input: HeadlessTaskInput,
    options: HeadlessTaskOptions,
    profile: Option<&str>,
    global_model: Option<&str>,
    api: &astra_thin_client::ThinClient,
    cli_context: &crate::cli::cli_context::CliContext,
) -> Result<ExitCode, String> {
    let HeadlessTaskInput {
        task_id,
        prompt,
        svc,
        session_id,
    } = input;
    use astra_services::{TaskCheckpoint, TaskStatus};
    let (_creds, profile_name, _, token) = get_profile_and_token(profile)?;

    emit_task_event(
        options.stream_events,
        serde_json::json!({
            "type": "task_started",
            "task_id": task_id,
            "task_type": "local_agent",
            "description": prompt,
        }),
    );

    if options.print_started && !options.quiet && !options.json {
        eprintln!(
            "  {} Task started: {} ({})",
            "▶".cyan(),
            prompt.chars().take(50).collect::<String>(),
            prefix_chars(&task_id, 8).dim()
        );
    }

    svc.update_status(&task_id, TaskStatus::InProgress).await?;

    let pipeline_modules = session_runtime::create_pipeline_modules_quiet(api, profile, astra_config::runtime_config::SessionTraceConfig::default());
    let skill_search = astra_core::SkillSearchSettings::default();
    let project_root = std::env::current_dir().unwrap_or_default();
    let mut pm = PermissionManager::with_project(true, &project_root);
    let mut skill_qt = astra_skills::quality::SkillQualityTracker::new();
    let root_agent_id = format!("task-{task_id}");
    let spawner = super::agent_runtime::build_one_shot_spawner(
        api,
        token.clone(),
        pipeline_modules.unified_skill_registry.clone(),
        pm.mode(),
        skill_search.clone(),
        session_id.clone(),
        global_model.map(str::to_owned),
    )
    .await;
    let spawner_handle_for_drain = spawner.clone();

    let (stream_event_tx, stream_event_writer) = if options.stream_events {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let handle = crate::cli::stream_events_writer::spawn_stderr_writer(rx);
        (Some(tx), Some(handle))
    } else {
        (None, None)
    };

    let render_policy = if options.quiet || options.json {
        crate::cli::stream_render::RenderPolicy::Silent
    } else {
        crate::cli::stream_render::RenderPolicy::Stream
    };
    // Headless single-shot path: use the MO-backed task store when available
    // so session_todos is authoritative here the same way it is in the REPL.
    let task_store =
        crate::cli::session_runtime::resolve_task_store(profile, Some(&api.api_origin()))
            .await
            .0;
    let task_manager = std::sync::Arc::new(crate::edge_tools::TaskManager::new(
        session_id
            .clone()
            .unwrap_or_else(|| "no-session".to_string()),
        task_store,
    ));

    let chat_ctx = crate::cli::chat_stream::BasicCliChatContext {
        api,
        auth_profile: profile,
        message: &prompt,
        model: global_model,
        provider: None,
        explain: ExplainMode::Off,
        render_md: terminal::size().is_ok() && !options.quiet && !options.json,
        verbose_mode: !options.quiet && !options.json,
        render_policy,
        cli_context: Some(cli_context),
        unified_skill_registry: &pipeline_modules.unified_skill_registry,
        skill_search: &skill_search,
        agent_spawner: Some(spawner),
        root_agent_id: Some(&root_agent_id),
        task_manager: Some(task_manager),
        task_notify_tx: None,
        bg_task_commands: None,
        bash_detach_slot: None,
        stream_event_tx,
        #[cfg(feature = "harness")]
        harness_sink: Some(astra_harness::InMemorySnapshotSink::arc()),
        #[cfg(feature = "harness")]
        harness_trace: Some(std::sync::Arc::new(std::sync::RwLock::new(
            astra_harness::SessionTrace::new(None),
        ))),
    };

    let turn_options = crate::cli::turn_facade::BasicCliTurnOptions::default();
    let mut sr = match crate::cli::turn_facade::execute_basic_cli_turn(
        &chat_ctx,
        &token,
        session_id.as_deref(),
        None,
        &mut pm,
        &mut skill_qt,
        turn_options,
    )
    .await
    {
        Ok(sr) => sr,
        Err(e) => {
            let _ = svc.fail_task(&task_id, &e.error).await;
            emit_task_event(
                options.stream_events,
                serde_json::json!({
                    "type": "task_notification",
                    "task_id": task_id,
                    "status": "failed",
                    "summary": e.error,
                }),
            );
            return Err(e.error);
        }
    };

    sr.background_agent_results = spawner_handle_for_drain
        .shutdown_and_wait(std::time::Duration::from_secs(30))
        .await;

    drop(chat_ctx);
    if let Some(handle) = stream_event_writer {
        let _ = handle.await;
    }

    let output_path = match write_task_output(&task_id, &sr.full_text) {
        Ok(path) => path,
        Err(e) => {
            let _ = svc.fail_task(&task_id, &e).await;
            return Err(e);
        }
    };
    let output_path_string = output_path.to_string_lossy().to_string();
    let mut state_map = serde_json::Map::new();
    state_map.insert(
        "full_text".to_string(),
        serde_json::Value::String(sr.full_text.clone()),
    );
    state_map.insert(
        "output_file".to_string(),
        serde_json::Value::String(output_path_string.clone()),
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
    state_map.insert(
        "background_agent_results".to_string(),
        serde_json::json!(
            sr.background_agent_results
                .iter()
                .map(|(id, text)| serde_json::json!({"agent_id": id, "result": text}))
                .collect::<Vec<_>>()
        ),
    );
    if let Err(e) = svc
        .save_checkpoint(
            &task_id,
            &TaskCheckpoint {
                active_subtask_id: None,
                turn: 0,
                session_id: sr.session_id.clone().or(session_id.clone()),
                state: state_map,
            },
        )
        .await
    {
        let _ = svc.fail_task(&task_id, &e).await;
        return Err(e);
    }

    let exit_code = compute_exit_code(&sr);
    if exit_code == ExitCode::Success {
        svc.complete_task(&task_id).await?;
    } else {
        svc.fail_task(
            &task_id,
            error_kind_for_exit_code(exit_code).unwrap_or("task failed"),
        )
        .await?;
    }

    if let Some(ref sid) = sr.session_id {
        persist_profile_last_session(Some(&profile_name), sid)?;
    }

    emit_task_event(
        options.stream_events,
        serde_json::json!({
            "type": "task_notification",
            "task_id": task_id,
            "status": if exit_code == ExitCode::Success { "completed" } else { "failed" },
            "output_file": output_path_string,
            "summary": sr.full_text.chars().take(200).collect::<String>(),
        }),
    );

    if options.json {
        let mut json_output = final_json_output(&sr, exit_code);
        if let Some(obj) = json_output.as_object_mut() {
            obj.insert("task_id".to_string(), serde_json::json!(task_id));
            obj.insert(
                "task_status".to_string(),
                serde_json::json!(if exit_code == ExitCode::Success {
                    "completed"
                } else {
                    "failed"
                }),
            );
            obj.insert(
                "output_file".to_string(),
                serde_json::json!(output_path_string),
            );
            obj.insert(
                "background_agent_results".to_string(),
                serde_json::json!(
                    sr.background_agent_results
                        .iter()
                        .map(|(id, text)| serde_json::json!({"agent_id": id, "result": text}))
                        .collect::<Vec<_>>()
                ),
            );
        }
        println!(
            "{}",
            serde_json::to_string_pretty(&json_output).unwrap_or_default()
        );
    } else if options.quiet {
        println!("{}", sr.full_text);
    } else {
        eprintln!(
            "\n  {} Task {} finished; output saved to {}",
            theme::icon_ok(),
            prefix_chars(&task_id, 8).cyan(),
            output_path.display().to_string().dim()
        );
    }

    Ok(exit_code)
}

async fn execute_headless_task_run(
    args: TaskRunArgs,
    profile: Option<&str>,
    global_model: Option<&str>,
    api: &astra_thin_client::ThinClient,
    cli_context: &crate::cli::cli_context::CliContext,
) -> Result<ExitCode, String> {
    use astra_services::TaskCreateRequest;

    let prompt = join_words(&args.text);
    if prompt.trim().is_empty() {
        return Err("task prompt cannot be empty".to_string());
    }

    let session_id = match cli_context.session_id.clone() {
        Some(session_id) => Some(session_id),
        None => validated_resumable_last_session_id(api, profile).await,
    };
    let user_id = cli_user_id();
    let task_session_id = session_id.as_deref().unwrap_or("no-session");
    let svc = session_runtime::resolve_task_service(profile).await;
    let task_id = svc
        .create_task(
            &user_id,
            task_session_id,
            TaskCreateRequest {
                title: task_run_title(&prompt),
                description: Some(prompt.clone()),
                plan: None,
                parent_task_id: None,
                project_type: None,
                goal_pattern: None,
            },
        )
        .await?;

    execute_headless_task_body(
        HeadlessTaskInput {
            task_id,
            prompt,
            svc,
            session_id,
        },
        HeadlessTaskOptions {
            json: args.json,
            quiet: args.quiet,
            stream_events: args.stream_events,
            print_started: true,
        },
        profile,
        global_model,
        api,
        cli_context,
    )
    .await
}

async fn execute_task_queue(
    args: TaskQueueArgs,
    cli_context: &crate::cli::cli_context::CliContext,
) -> Result<ExitCode, String> {
    use astra_services::TaskCreateRequest;

    let prompt = join_words(&args.text);
    if prompt.trim().is_empty() {
        return Err("task prompt cannot be empty".to_string());
    }
    // execute_task_queue is a CLI subcommand without profile in
    // scope — token comes from env via current_access_token(None).
    // Per-user CLI sessions use `astra task worker` which threads
    // profile through.
    let (svc, _) = session_runtime::resolve_cloud_task_runtime(None).await?;
    let session_id = cli_context
        .session_id
        .clone()
        .unwrap_or_else(|| "cloud-queue".into());
    let user_id = cli_user_id();
    let task_id = svc
        .create_task(
            &user_id,
            &session_id,
            TaskCreateRequest {
                title: task_run_title(&prompt),
                description: Some(prompt.clone()),
                plan: None,
                parent_task_id: None,
                project_type: Some("cloud-agent".to_string()),
                goal_pattern: None,
            },
        )
        .await?;

    if args.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "task_id": task_id,
                "status": "pending",
                "backend": "cloud-api",
            }))
            .unwrap_or_default()
        );
    } else {
        eprintln!(
            "  {} Cloud task queued: {} ({})",
            theme::icon_ok(),
            prompt.chars().take(50).collect::<String>(),
            prefix_chars(&task_id, 8).dim()
        );
        eprintln!(
            "  {}",
            "Run `astra task worker --once` from a cloud agent/worker to claim it.".dim()
        );
    }
    Ok(ExitCode::Success)
}

fn default_task_agent_id() -> String {
    std::env::var("ASTRA_EDGE_AGENT_ID")
        .or_else(|_| std::env::var("HOSTNAME").map(|host| format!("astra-{host}")))
        .unwrap_or_else(|_| format!("astra-worker-{}", std::process::id()))
}

/// Outcome of a single worker poll. `Interrupted` lets the outer
/// `--loop` driver tell a user-initiated Ctrl+C apart from a normal
/// "task done" cycle, so the loop exits promptly instead of requiring
/// a second Ctrl+C during the poll-interval sleep.
enum WorkerOutcome {
    Completed(ExitCode),
    Interrupted,
}

async fn execute_task_worker_once(
    args: &TaskWorkerArgs,
    profile: Option<&str>,
    global_model: Option<&str>,
    api: &astra_thin_client::ThinClient,
    cli_context: &crate::cli::cli_context::CliContext,
) -> Result<WorkerOutcome, String> {
    use astra_services::{LeaseClaimResult, TaskStatus};

    let (svc, lease_svc) = session_runtime::resolve_cloud_task_runtime(profile).await?;
    let user_id = cli_user_id();
    let agent_id = args.agent_id.clone().unwrap_or_else(default_task_agent_id);
    let edge_id = std::env::var("ASTRA_EDGE_ID").unwrap_or_else(|_| agent_id.clone());
    let pending_tasks = svc.list_tasks(&user_id, Some(TaskStatus::Pending)).await?;
    if pending_tasks.is_empty() {
        if args.json {
            println!(
                "{}",
                serde_json::json!({"claimed": false, "reason": "no_pending_tasks"})
            );
        } else if !args.quiet {
            eprintln!("  {}", "No pending cloud tasks.".dim());
        }
        return Ok(WorkerOutcome::Completed(ExitCode::Success));
    }

    let mut claimed_task_id = None;
    for candidate in pending_tasks {
        // A transient lease-claim failure (pool hiccup, replica lag,
        // brief MO unavailability) on ONE candidate must not abort
        // the whole poll — in loop mode that would kill the worker
        // on the first flaky query. Log and try the next candidate.
        let claim = match lease_svc
            .try_claim_lease(
                &user_id,
                &candidate.task_id,
                &agent_id,
                &edge_id,
                args.ttl_seconds,
            )
            .await
        {
            Ok(c) => c,
            Err(e) => {
                if !args.quiet && !args.json {
                    eprintln!(
                        "  {} claim failed for {}: {} — skipping",
                        "⚠".yellow(),
                        prefix_chars(&candidate.task_id, 8).dim(),
                        e
                    );
                }
                tracing::warn!(task_id = %candidate.task_id, error = %e, "try_claim_lease transient failure");
                continue;
            }
        };
        match claim {
            LeaseClaimResult::Granted {
                lease_version,
                expires_at,
            } => {
                if args.json {
                    eprintln!(
                        "{}",
                        serde_json::json!({
                            "claimed": true,
                            "task_id": candidate.task_id,
                            "agent_id": agent_id,
                            "lease_version": lease_version,
                            "expires_at": expires_at,
                        })
                    );
                } else if !args.quiet {
                    eprintln!(
                        "  {} Claimed cloud task {} as {}",
                        "▶".cyan(),
                        prefix_chars(&candidate.task_id, 8).dim(),
                        agent_id.as_str().cyan()
                    );
                }
                claimed_task_id = Some(candidate.task_id);
                break;
            }
            LeaseClaimResult::Contested {
                holder_agent_id,
                expires_at,
            } => {
                if !args.quiet && !args.json {
                    eprintln!(
                        "  {} Skipping leased task {} held by {} until {}",
                        "⚠".yellow(),
                        prefix_chars(&candidate.task_id, 8).dim(),
                        holder_agent_id,
                        expires_at
                    );
                }
            }
        }
    }
    let Some(claimed_task_id) = claimed_task_id else {
        if args.json {
            println!(
                "{}",
                serde_json::json!({"claimed": false, "reason": "all_pending_tasks_leased"})
            );
        } else if !args.quiet {
            eprintln!(
                "  {}",
                "All pending cloud tasks are currently leased.".dim()
            );
        }
        return Ok(WorkerOutcome::Completed(ExitCode::Success));
    };

    // `get_task` runs AFTER a successful claim — any failure here would
    // leak the lease until TTL expiry. Bail with a release so the task
    // is immediately re-claimable.
    let task = match svc.get_task(&claimed_task_id).await {
        Ok(Some(t)) => t,
        Ok(None) => {
            if let Err(e) = lease_svc
                .release_lease(&user_id, &claimed_task_id, &agent_id)
                .await
            {
                tracing::warn!(
                    task_id = %claimed_task_id,
                    error = %e,
                    "release_lease failed after get_task returned None"
                );
            }
            return Err(format!("claimed task disappeared: {claimed_task_id}"));
        }
        Err(e) => {
            if let Err(re) = lease_svc
                .release_lease(&user_id, &claimed_task_id, &agent_id)
                .await
            {
                tracing::warn!(
                    task_id = %claimed_task_id,
                    error = %re,
                    "release_lease failed after get_task error"
                );
            }
            return Err(format!("get_task failed after claim: {e}"));
        }
    };
    let prompt = task
        .description
        .clone()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| task.title.clone());

    // Renew the lease periodically while the task runs. Renewal interval is
    // ttl/2 so we always renew well before expiry. The renewal task is
    // wrapped in `AbortGuard` so ANY path out of this function (success,
    // error, Ctrl+C cancellation, panic unwind) aborts the background
    // task — dropping a JoinHandle alone does not cancel it, which used
    // to leave a zombie renewer refreshing a dead task's lease.
    //
    // Two-layer cancellation: `cancel` is the AtomicBool we check
    // before each renew SQL call — once set, the task returns without
    // starting another renew. `cancel_notify` wakes an in-progress
    // sleep so we don't wait the full ttl/2 interval after the caller
    // signals cancel. Notification loss (notify_waiters fires when no
    // one is listening) is harmless here because every loop iteration
    // also re-reads `cancel` before sleeping, so a lost notify just
    // means we cancel on the next wake rather than instantly.
    //
    // The `AbortHandle` wrapped in `AbortGuard` is the backstop for
    // unexpected exit paths (panic / outer cancel that skips our
    // cooperative cancel-and-await below).
    let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let cancel_notify = std::sync::Arc::new(tokio::sync::Notify::new());
    let renewal_user_id = user_id.clone();
    let mut renewal_handle: Option<tokio::task::JoinHandle<()>> = Some({
        let lease_svc = lease_svc.clone();
        let task_id = task.task_id.clone();
        let agent_id = agent_id.clone();
        let edge_id = edge_id.clone();
        let ttl = args.ttl_seconds;
        let interval_secs = (ttl / 2).max(1) as u64;
        let cancel = cancel.clone();
        let cancel_notify = cancel_notify.clone();
        tokio::spawn(async move {
            loop {
                if cancel.load(std::sync::atomic::Ordering::Acquire) {
                    return;
                }
                tokio::select! {
                    _ = tokio::time::sleep(std::time::Duration::from_secs(interval_secs)) => {}
                    _ = cancel_notify.notified() => {}
                }
                if cancel.load(std::sync::atomic::Ordering::Acquire) {
                    return;
                }
                if let Err(e) = lease_svc
                    .renew_lease(&renewal_user_id, &task_id, &agent_id, &edge_id, ttl)
                    .await
                {
                    tracing::debug!(
                        task_id = %task_id,
                        error = %e,
                        "lease renewal failed (non-fatal)"
                    );
                }
            }
        })
    });
    let _renewal_guard = AbortGuard::from_abort_handle(
        renewal_handle
            .as_ref()
            .expect("just spawned")
            .abort_handle(),
    );

    // Honour Ctrl+C during long-running task execution. Without this the
    // worker has to wait for the task body to finish, which can be
    // minutes; users expect interrupt to be prompt. On Ctrl+C we fall
    // through to release_lease so the task is freed for another worker.
    // `interrupted` lets the outer --loop driver exit cleanly instead
    // of requiring a second Ctrl+C during the poll-interval sleep.
    let (body_result, interrupted): (Result<ExitCode, String>, bool) = tokio::select! {
        res = execute_headless_task_body(
            HeadlessTaskInput {
                task_id: task.task_id.clone(),
                prompt,
                svc: svc.clone(),
                session_id: task.session_id.clone(),
            },
            HeadlessTaskOptions {
                json: args.json,
                quiet: args.quiet,
                stream_events: args.stream_events,
                print_started: false,
            },
            profile,
            global_model,
            api,
            cli_context,
        ) => (res, false),
        _ = tokio::signal::ctrl_c() => {
            if !args.quiet && !args.json {
                eprintln!("  {}", "Task interrupted — releasing lease.".dim());
            }
            (Ok(ExitCode::Success), true)
        }
    };

    // Cooperative cancellation: flip the atomic FIRST, then wake any
    // sleeping renewer. This ordering matters — if we notified before
    // setting the flag, a task just entering `select!` could miss the
    // notification and sleep the full interval. Awaiting the handle
    // afterwards guarantees the task has actually returned (including
    // any in-flight renew finishing) before we issue release_lease, so
    // there is no race window where a stale renew re-resurrects the
    // lease after release. The guard stays as a backstop for panic /
    // outer-cancel paths; `.abort()` on an already-finished task is
    // a safe no-op.
    cancel.store(true, std::sync::atomic::Ordering::Release);
    cancel_notify.notify_waiters();
    if let Some(h) = renewal_handle.take() {
        let _ = h.await;
    }

    // On Ctrl+C the body was dropped while the task row was already
    // `InProgress` (execute_headless_task_body sets that early). Without
    // a revert, `list_tasks(Pending)` wouldn't re-surface the task and
    // no other worker could claim it — the task would sit stranded
    // until the lease TTL expired. Revert to `Pending` BEFORE releasing
    // the lease so a concurrent worker polling post-release sees the
    // right status.
    //
    // Guards:
    // - **Lease ownership** — if another worker stole the lease while
    //   we ran (TTL expired, they claimed), that worker is now the
    //   authoritative state owner. Reverting their `InProgress` back
    //   to `Pending` would cause double-claim. Check the current
    //   lease holder first; skip revert if it isn't us.
    // - **Terminal-state** — `update_status` returns
    //   `Err("invalid task status transition: …")` if the row is
    //   already in a terminal state (another worker completed it).
    //   That's not a real failure for our path; log at debug and
    //   move on.
    // - **Timeout** — Ctrl+C is a prompt-exit signal; we MUST NOT
    //   block indefinitely here if MO is unavailable. 5 s is a
    //   generous budget for a single UPDATE; timeout degrades to
    //   "stranded until TTL" which matches pre-fix behaviour.
    if interrupted {
        use std::time::Duration;
        let revert_timeout = Duration::from_secs(5);
        let still_ours = match tokio::time::timeout(
            revert_timeout,
            lease_svc.get_lease(&user_id, &task.task_id),
        )
        .await
        {
            Ok(Ok(Some(view))) => view.holder_agent_id == agent_id,
            Ok(Ok(None)) => false, // lease already expired / released
            Ok(Err(e)) => {
                tracing::warn!(
                    task_id = %task.task_id,
                    error = %e,
                    "get_lease before revert failed — skipping revert"
                );
                false
            }
            Err(_) => {
                tracing::warn!(
                    task_id = %task.task_id,
                    "get_lease timed out before revert — skipping revert"
                );
                false
            }
        };
        if still_ours {
            let revert = tokio::time::timeout(
                revert_timeout,
                svc.update_status(&task.task_id, astra_services::TaskStatus::Pending),
            )
            .await;
            match revert {
                Ok(Ok(())) => {
                    tracing::debug!(task_id = %task.task_id, "interrupted task reverted to Pending");
                }
                Ok(Err(e)) if e.starts_with("invalid task status transition") => {
                    // Another worker finished the task while we were
                    // cleaning up — nothing to revert. Debug, not warn.
                    tracing::debug!(
                        task_id = %task.task_id,
                        error = %e,
                        "task already in terminal state; skipping revert"
                    );
                }
                Ok(Err(e)) => {
                    tracing::warn!(
                        task_id = %task.task_id,
                        error = %e,
                        "failed to revert interrupted task to Pending (task may appear stranded until lease TTL)"
                    );
                }
                Err(_) => {
                    tracing::warn!(
                        task_id = %task.task_id,
                        "update_status revert timed out (task may appear stranded until lease TTL)"
                    );
                }
            }
        }
    }

    let release_result = lease_svc
        .release_lease(&user_id, &task.task_id, &agent_id)
        .await;
    match release_result {
        Ok(true) => {
            tracing::debug!(task_id = %task.task_id, "lease released");
        }
        Ok(false) => {
            // No row deleted — either the lease expired during the
            // task or another worker stole it. Log at info so operators
            // can see the condition; not an error for this worker.
            tracing::info!(
                task_id = %task.task_id,
                "release_lease returned false (lease already expired or stolen)"
            );
        }
        Err(e) => {
            return Err(format!(
                "task execution finished but lease release failed: {e}"
            ));
        }
    }
    body_result.map(|code| {
        if interrupted {
            WorkerOutcome::Interrupted
        } else {
            WorkerOutcome::Completed(code)
        }
    })
}

/// Aborts a spawned tokio task when dropped. Used to guarantee
/// cleanup regardless of how the parent future exits (return, error,
/// cancellation, panic). Dropping a raw `JoinHandle` does NOT abort
/// the task — this wrapper is the fix.
///
/// Two constructors: `new` takes ownership of the JoinHandle (simple
/// fire-and-forget case); `from_abort_handle` keeps only the abort
/// side-channel so the caller retains the JoinHandle for cooperative
/// cancel-and-await — the guard then only runs on the unhappy path.
enum AbortGuard {
    Handle(tokio::task::JoinHandle<()>),
    AbortOnly(tokio::task::AbortHandle),
}

impl AbortGuard {
    fn new(handle: tokio::task::JoinHandle<()>) -> Self {
        Self::Handle(handle)
    }

    fn from_abort_handle(handle: tokio::task::AbortHandle) -> Self {
        Self::AbortOnly(handle)
    }
}

impl Drop for AbortGuard {
    fn drop(&mut self) {
        match self {
            Self::Handle(h) => h.abort(),
            Self::AbortOnly(h) => h.abort(),
        }
    }
}

#[cfg(test)]
mod abort_guard_tests {
    use super::AbortGuard;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;

    /// Proves the guard aborts the spawned task on drop. Without the
    /// guard, a dropped `JoinHandle` leaves the task running — this is
    /// the B2 bug regression test: Ctrl+C on the worker must stop the
    /// lease-renewer, not leak it.
    #[tokio::test]
    async fn drop_aborts_spawned_task() {
        let flag = Arc::new(AtomicBool::new(false));
        let flag_inner = flag.clone();

        let guard = AbortGuard::new(tokio::spawn(async move {
            // If the guard doesn't abort us, we sleep 500ms then set the
            // flag. The test waits 200ms before asserting — so the flag
            // being false is only possible if we were aborted first.
            tokio::time::sleep(Duration::from_millis(500)).await;
            flag_inner.store(true, Ordering::SeqCst);
        }));

        // Yield once so the spawned task is actually polled.
        tokio::time::sleep(Duration::from_millis(10)).await;
        drop(guard);

        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(
            !flag.load(Ordering::SeqCst),
            "spawned task kept running after guard drop — abort did not fire"
        );
    }

    /// Models the `tokio::select!` cancellation path used on Ctrl+C:
    /// the owning future is dropped mid-await by the select arm picking
    /// the signal branch. The guard's Drop must still abort the child
    /// — this is the exact scenario B2 reports (Ctrl+C → zombie
    /// renewer).
    #[tokio::test]
    async fn select_cancellation_aborts_guarded_task() {
        let flag = Arc::new(AtomicBool::new(false));
        let flag_inner = flag.clone();

        // A future that owns the guard and waits. We "cancel" it by
        // racing it against a short timer in tokio::select! — the
        // select drops the losing arm, which invokes Drop on the guard
        // (B2's exact scenario: signal arm wins, task arm drops).
        let owning_future = async {
            let _guard = AbortGuard::new(tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(500)).await;
                flag_inner.store(true, Ordering::SeqCst);
            }));
            tokio::time::sleep(Duration::from_secs(10)).await; // never completes in test window
        };

        tokio::select! {
            _ = owning_future => panic!("owning future should still be blocked"),
            _ = tokio::time::sleep(Duration::from_millis(50)) => {}
        }

        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(
            !flag.load(Ordering::SeqCst),
            "select-cancellation dropped guard but background task kept running"
        );
    }

    /// Models the renewer loop's cooperative cancel + await pattern
    /// and asserts no renew call can land AFTER the caller's await on
    /// the handle completes. This is the stronger guarantee that
    /// `AbortGuard::abort` alone cannot provide (abort does not kill
    /// in-flight awaits, only delivers cancellation at the next await
    /// point). The loop shape here mirrors
    /// `execute_task_worker_once`'s spawned renewer.
    #[tokio::test]
    async fn cooperative_cancel_and_await_has_no_late_renew() {
        use std::sync::atomic::AtomicU32;
        let cancel = Arc::new(AtomicBool::new(false));
        let notify = Arc::new(tokio::sync::Notify::new());
        let renew_count = Arc::new(AtomicU32::new(0));

        let handle = {
            let cancel = cancel.clone();
            let notify = notify.clone();
            let renew_count = renew_count.clone();
            tokio::spawn(async move {
                loop {
                    if cancel.load(Ordering::Acquire) {
                        return;
                    }
                    tokio::select! {
                        _ = tokio::time::sleep(Duration::from_millis(50)) => {}
                        _ = notify.notified() => {}
                    }
                    if cancel.load(Ordering::Acquire) {
                        return;
                    }
                    // Simulate a renew SQL call that takes 40ms.
                    tokio::time::sleep(Duration::from_millis(40)).await;
                    renew_count.fetch_add(1, Ordering::SeqCst);
                }
            })
        };

        // Let at least one renew run.
        tokio::time::sleep(Duration::from_millis(110)).await;
        let before = renew_count.load(Ordering::SeqCst);
        assert!(
            before >= 1,
            "test scaffolding: renew should have fired at least once"
        );

        // Cooperative cancel.
        cancel.store(true, Ordering::Release);
        notify.notify_waiters();
        let _ = handle.await;

        // Any renew happening strictly after the awaited handle would
        // be a regression — the task is fully returned here. Wait a
        // generous window to catch a spurious late renew.
        let after_await = renew_count.load(Ordering::SeqCst);
        tokio::time::sleep(Duration::from_millis(150)).await;
        let after_wait = renew_count.load(Ordering::SeqCst);
        assert_eq!(
            after_await, after_wait,
            "renew fired after the awaited handle completed (late-renew race)"
        );
    }

    /// Simulates the panic-unwind exit path: if the parent scope
    /// panics, the guard's Drop still aborts the background task.
    #[tokio::test]
    async fn panic_still_drops_guard() {
        let flag = Arc::new(AtomicBool::new(false));
        let flag_inner = flag.clone();
        let result = std::panic::AssertUnwindSafe(async {
            let _guard = AbortGuard::new(tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(500)).await;
                flag_inner.store(true, Ordering::SeqCst);
            }));
            tokio::time::sleep(Duration::from_millis(10)).await;
            panic!("simulated cancellation path");
        });
        let _ = futures_util::FutureExt::catch_unwind(result).await;

        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(
            !flag.load(Ordering::SeqCst),
            "panic unwound without aborting the renewal task"
        );
    }
}

async fn execute_task_worker(
    args: TaskWorkerArgs,
    profile: Option<&str>,
    global_model: Option<&str>,
    api: &astra_thin_client::ThinClient,
    cli_context: &crate::cli::cli_context::CliContext,
) -> Result<ExitCode, String> {
    if !args.once && !args.loop_mode {
        return Err("choose --once or --loop for task worker".to_string());
    }
    if args.once {
        return match execute_task_worker_once(&args, profile, global_model, api, cli_context)
            .await?
        {
            WorkerOutcome::Completed(code) => Ok(code),
            WorkerOutcome::Interrupted => Ok(ExitCode::Success),
        };
    }
    loop {
        match execute_task_worker_once(&args, profile, global_model, api, cli_context).await? {
            WorkerOutcome::Completed(code) if code != ExitCode::Success => return Ok(code),
            WorkerOutcome::Completed(_) => {}
            // User Ctrl+C'd mid-task — exit the loop now so they don't
            // have to hit Ctrl+C again during the poll-interval sleep.
            WorkerOutcome::Interrupted => {
                if !args.quiet && !args.json {
                    eprintln!("  {}", "Worker interrupted.".dim());
                }
                return Ok(ExitCode::Success);
            }
        }
        let sleep = tokio::time::sleep(std::time::Duration::from_secs(args.poll_seconds.max(1)));
        tokio::select! {
            _ = sleep => {}
            _ = tokio::signal::ctrl_c() => {
                if !args.quiet && !args.json {
                    eprintln!("  {}", "Worker interrupted.".dim());
                }
                return Ok(ExitCode::Success);
            }
        }
    }
}

async fn execute_task_result(args: TaskResultArgs) -> Result<ExitCode, String> {
    use astra_services::TaskStatus;

    let query = join_words(&args.query);
    if query.trim().is_empty() {
        return Err("provide a task id or title fragment".to_string());
    }

    // No profile in this CLI subcommand context; HttpTaskService
    // falls back to env-only token resolution which is fine for
    // one-shot `astra task result <query>` invocations.
    let svc = session_runtime::resolve_task_service(None).await;
    let task_id = super::slash_task::find_task_by_query(&*svc, "local", &query)
        .await?
        .ok_or_else(|| format!("no task matching '{query}'"))?;
    let task = svc
        .get_task(&task_id)
        .await?
        .ok_or_else(|| format!("task disappeared: {task_id}"))?;

    let short = &task.task_id[..8.min(task.task_id.len())];
    eprintln!(
        "\n{}",
        format!("─── Task Result ({short}) ─────────────────────────").bold()
    );
    eprintln!("  {:<12} {}", "title:".dim(), task.title);
    eprintln!("  {:<12} {}", "status:".dim(), task.status.as_str().cyan());
    if let Some(ref err) = task.error_message {
        eprintln!("  {:<12} {}", "error:".dim(), err.as_str().red());
    }

    // 1. Try checkpoint (set by `task run` and worker)
    if let Some(ref cp) = task.checkpoint {
        if let Some(full_text) = cp.state.get("full_text").and_then(|v| v.as_str()) {
            eprintln!();
            if args.json {
                let tokens = cp
                    .state
                    .get("prompt_tokens")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0);
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
                let output_file = cp
                    .state
                    .get("output_file")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "task_id": task.task_id,
                        "title": task.title,
                        "status": task.status.as_str(),
                        "full_text": full_text,
                        "prompt_tokens": tokens,
                        "completion_tokens": comp,
                        "tool_calls_count": tools,
                        "output_file": output_file,
                    }))
                    .unwrap_or_default()
                );
            } else {
                println!("{full_text}");
                if let Some(tokens) = cp.state.get("prompt_tokens").and_then(|v| v.as_u64()) {
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
                if let Some(output_file) = cp.state.get("output_file").and_then(|v| v.as_str()) {
                    eprintln!("  {}", format!("output: {output_file}").dim());
                }
            }
            eprintln!();
            return Ok(ExitCode::Success);
        }
    }

    // 2. Fallback: read output file at the canonical path for this task_id
    if let Ok(output_path) = task_output_path(&task.task_id) {
        if let Ok(text) = std::fs::read_to_string(&output_path) {
            if !text.trim().is_empty() {
                eprintln!();
                if args.json {
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&serde_json::json!({
                            "task_id": task.task_id,
                            "title": task.title,
                            "status": task.status.as_str(),
                            "full_text": text,
                            "output_file": output_path.display().to_string(),
                        }))
                        .unwrap_or_default()
                    );
                } else {
                    println!("{text}");
                    eprintln!("  {}", format!("output: {}", output_path.display()).dim());
                }
                eprintln!();
                return Ok(ExitCode::Success);
            }
        }
    }

    // 3. Still running / no data yet
    match task.status {
        TaskStatus::InProgress | TaskStatus::Pending => {
            if args.json {
                println!(
                    "{}",
                    serde_json::json!({"task_id": task.task_id, "status": "running"})
                );
            } else {
                eprintln!("  {}", "Task is still running…".yellow());
            }
        }
        _ => {
            if args.json {
                println!(
                    "{}",
                    serde_json::json!({"task_id": task.task_id, "status": task.status.as_str(), "result": null})
                );
            } else {
                eprintln!("  {}", "No result available.".dim());
            }
        }
    }
    eprintln!();
    Ok(ExitCode::Success)
}

async fn execute_repl_bridge_command(
    slash_cmd: &str,
    arg: &str,
    profile: Option<&str>,
    global_model: Option<&str>,
    api: &astra_thin_client::ThinClient,
    cli_context: &crate::cli::cli_context::CliContext,
) -> Result<ExitCode, String> {
    try_silent_auth(api, profile).await;

    let mut state = initialize_session_state(profile, global_model, cli_context);
    if slash_cmd == "/messaging" {
        handle_messaging_command(arg, &state);
        return Ok(ExitCode::Success);
    }
    let task_service = session_runtime::resolve_task_service(profile).await;
    session_runtime::install_task_service(&mut state, task_service);
    let (task_store, task_notify_tx) =
        session_runtime::resolve_task_store(profile, Some(&api.api_origin())).await;
    session_runtime::install_task_store(&mut state, task_store);
    state.task_notify_tx = task_notify_tx;
    maybe_load_project_instructions(&mut state);

    let pipeline_modules = create_pipeline_modules(api, profile, astra_config::runtime_config::SessionTraceConfig::default());
    state.unified_skill_registry = pipeline_modules.unified_skill_registry.clone();
    state.mcp_manager = pipeline_modules.mcp_manager.clone();

    let token = session_runtime::fresh_access_token(api, profile).await;
    if let Some(ref tok) = token {
        maybe_wire_delegation_engine(&mut state, api, tok);
    }

    match slash_cmd {
        "/team" => slash_team::handle_team_command(arg, api, profile, &mut state).await,
        "/telemetry" => slash_telemetry::handle_telemetry_command(arg, &state),
        "/task" => {
            slash_task::handle_task_command(arg, &mut state, api, profile, token.as_deref()).await
        }
        "/memory" => {
            handle_memory_domain_command("/memory", arg, api, &mut state, token.as_deref()).await?
        }
        "/plan" => {
            crate::cli::slash_plan::handle_plan_command(
                arg,
                api,
                profile,
                &mut state,
                token.as_deref(),
            )
            .await?
        }
        "/review" | "/grep" => {
            handle_info_command(slash_cmd, arg, api, &mut state, profile, token.as_deref()).await?
        }
        "/diff" => {
            let root = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
            diff_presenter::run_diff_command(&root, arg, cli_utils::terminal_width_usize());
        }
        "/allow" => {
            handle_permission_command(arg, &mut state);
        }
        "/debug" => handle_debug_command(arg, &state),
        "/bug" => handle_bug_command(arg, &state),
        "/agent" => {
            let ctx = slash_agent::AgentCommandContext {
                spawner: state.agent_spawner.clone(),
                session_id: state.session_id.clone(),
            };
            slash_agent::handle_agent_command(arg, &ctx).await;
        }
        "/messaging" => handle_messaging_command(arg, &state),
        _ => return Err(format!("unsupported bridged command: {slash_cmd}")),
    }

    Ok(ExitCode::Success)
}

fn handle_permission_command(arg: &str, state: &mut SessionState) {
    match arg {
        "" => {
            let next = match state.perm_manager.mode() {
                PermissionMode::Prompt => PermissionMode::Plan,
                PermissionMode::Plan => PermissionMode::AcceptEdits,
                PermissionMode::AcceptEdits => PermissionMode::Auto,
                PermissionMode::Auto => PermissionMode::Deny,
                PermissionMode::Deny => PermissionMode::Prompt,
            };
            state.perm_manager.set_mode(next);
            eprintln!(
                "  {} Permission mode → {}",
                theme::icon_info(),
                permission_mode_display_label(next).magenta()
            );
        }
        "all" => {
            state.perm_manager.set_mode(PermissionMode::Auto);
            eprintln!(
                "  {} Permission mode → {} (all tools auto-approved)",
                "⚡".yellow(),
                "auto".magenta()
            );
        }
        "plan" => {
            state.perm_manager.set_mode(PermissionMode::Plan);
            eprintln!(
                "  {} Permission mode → {} (read-only investigation mode)",
                theme::icon_info(),
                "plan".magenta()
            );
        }
        "accept_edits" | "accept-edits" => {
            state.perm_manager.set_mode(PermissionMode::AcceptEdits);
            eprintln!(
                "  {} Permission mode → {} (workspace-local edits auto-approved)",
                theme::icon_info(),
                permission_mode_display_label(PermissionMode::AcceptEdits).magenta()
            );
        }
        "rules" | "status" => {
            let summary = state.perm_manager.rules_summary();
            eprint!("{summary}");
        }
        "trust" => match state.perm_manager.trust_workspace() {
            Ok(message) => eprintln!("  {} {message}", theme::icon_info()),
            Err(err) => eprintln!("  {} Failed to trust workspace: {err}", theme::icon_warn()),
        },
        "untrust" => match state.perm_manager.untrust_workspace() {
            Ok(message) => eprintln!("  {} {message}", theme::icon_info()),
            Err(err) => eprintln!(
                "  {} Failed to mark workspace untrusted: {err}",
                theme::icon_warn()
            ),
        },
        "trace" => {
            for line in astra_turn_core::permission::audit::format_snapshot_lines(50) {
                eprintln!("{line}");
            }
        }
        arg if arg.starts_with("trace --export ") => {
            let path = arg.trim_start_matches("trace --export ").trim();
            if path.is_empty() {
                eprintln!("  {} Missing export path", theme::icon_warn());
                return;
            }
            let lines = astra_turn_core::permission::audit::snapshot_redacted_jsonl_lines();
            let body = if lines.is_empty() {
                String::new()
            } else {
                format!("{}\n", lines.join("\n"))
            };
            match std::fs::write(path, body) {
                Ok(()) => eprintln!(
                    "  {} Permission trace exported to {path}",
                    theme::icon_info()
                ),
                Err(err) => eprintln!(
                    "  {} Failed to export permission trace to {path}: {err}",
                    theme::icon_warn()
                ),
            }
        }
        _ => match arg.parse::<PermissionMode>() {
            Ok(mode) => {
                state.perm_manager.set_mode(mode);
                eprintln!(
                    "  {} Permission mode → {}",
                    theme::icon_info(),
                    permission_mode_display_label(mode).magenta()
                );
            }
            Err(_) => {
                eprintln!(
                    "  {} Unknown mode '{}'. Use: auto, plan, accept-edits, prompt, deny, all, rules, trust, untrust, trace",
                    theme::icon_warn(),
                    arg
                );
            }
        },
    }
}

pub(crate) fn permission_mode_display_label(mode: PermissionMode) -> &'static str {
    match mode {
        PermissionMode::Prompt => "prompt",
        PermissionMode::Auto => "auto",
        PermissionMode::AcceptEdits => "accept-edits",
        PermissionMode::Plan => "plan",
        PermissionMode::Deny => "deny",
    }
}

#[cfg(test)]
mod permission_mode_display_tests {
    use super::{PermissionMode, permission_mode_display_label};

    #[test]
    fn accept_edits_displays_as_kebab_case() {
        assert_eq!(
            permission_mode_display_label(PermissionMode::AcceptEdits),
            "accept-edits"
        );
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn execute_cli_command(
    command: Option<Command>,
    profile: Option<String>,
    global_model: Option<String>,
    auto_approve: bool,
    system_prompt: Option<String>,
    api: &astra_thin_client::ThinClient,
    no_instructions: bool,
    max_budget: f64,
    cli_context: &crate::cli::cli_context::CliContext,
) -> Result<ExitCode, String> {
    match command {
        // No subcommand → interactive TUI (Codex-style default)
        None | Some(Command::Interactive) => {
            run_interactive_chat(
                api,
                profile.as_deref(),
                global_model.as_deref(),
                None,
                no_instructions,
                max_budget,
                cli_context,
            )
            .await?;
            Ok(ExitCode::Success)
        }

        Some(Command::Serve(args)) => {
            match args.mode {
                None => {
                    start_http_server(&args.host, args.port).await?;
                }
                Some(crate::cli::cli_args::ServeMode::Http(http_args)) => {
                    start_http_server(&http_args.host, http_args.port).await?;
                }
                Some(crate::cli::cli_args::ServeMode::Stdio) => {
                    crate::cli::app_server::run_stdio_app_server(
                        "stdio://",
                        api,
                        profile.as_deref(),
                        global_model.as_deref(),
                        system_prompt.as_deref(),
                        auto_approve,
                    )
                    .await?;
                }
            }
            Ok(ExitCode::Success)
        }

        // Inline message: astra "what is the answer to life?"
        Some(Command::Message(words)) => {
            let raw_message = words.join(" ");
            let message = apply_system_prompt(&raw_message, system_prompt.as_deref());
            let (_, _, _, token) = get_profile_and_token(profile.as_deref())?;
            let session_id = match cli_context.session_id.clone() {
                Some(session_id) => Some(session_id),
                None => validated_resumable_last_session_id(api, profile.as_deref()).await,
            };
            let mut continuation_messages = session_id
                .as_deref()
                .and_then(load_session_messages_for_continuation);
            let _pipeline = create_pipeline_modules(api, profile.as_deref(), astra_config::runtime_config::SessionTraceConfig::default());
            let mode = if auto_approve {
                PermissionMode::Auto
            } else {
                PermissionMode::Prompt
            };
            let mut pm = PermissionManager::with_load_policy(
                mode,
                &std::env::current_dir().unwrap_or_default(),
                &crate::cli::permission_manager::PermissionLoadPolicy::HeadlessSafe,
            );
            let mut skill_qt = astra_skills::quality::SkillQualityTracker::new();
            let skill_search = astra_core::SkillSearchSettings::default();
            let chat_ctx = crate::cli::chat_stream::BasicCliChatContext {
                api,
                auth_profile: profile.as_deref(),
                message: &message,
                model: global_model.as_deref(),
                provider: None,
                explain: ExplainMode::Off,
                render_md: terminal::size().is_ok(),
                verbose_mode: true,
                render_policy: crate::cli::stream_render::RenderPolicy::Stream,
                cli_context: Some(cli_context),
                unified_skill_registry: astra_runtime::skills::default_unified_registry(),
                skill_search: &skill_search,
                // Non-Chat (Message-style) path — legacy single-shot
                // invocation without dynamic sub-agent support. Keep
                // pre-fix behavior; extend later if this path needs
                // spawning too.
                agent_spawner: None,
                root_agent_id: None,
                task_manager: None,
                task_notify_tx: None,
                bg_task_commands: None,
                bash_detach_slot: None,
                stream_event_tx: None,
                #[cfg(feature = "harness")]
                harness_sink: Some(astra_harness::InMemorySnapshotSink::arc()),
                #[cfg(feature = "harness")]
                harness_trace: Some(std::sync::Arc::new(std::sync::RwLock::new(
                    astra_harness::SessionTrace::new(None),
                ))),
            };
            let turn_options = crate::cli::turn_facade::BasicCliTurnOptions {
                pre_loaded_messages: continuation_messages.take(),
                ..Default::default()
            };
            let sr = match crate::cli::turn_facade::execute_basic_cli_turn(
                &chat_ctx,
                &token,
                session_id.as_deref(),
                profile.as_deref(),
                &mut pm,
                &mut skill_qt,
                turn_options.clone(),
            )
            .await
            {
                Ok(sr) => sr,
                Err(e) if is_auth_error(&e.error) => {
                    if session_runtime::attempt_token_refresh(api, profile.as_deref()).await {
                        if let Some(new_token) =
                            session_runtime::current_access_token(profile.as_deref())
                        {
                            eprintln!(
                                "  {} Token refreshed, retrying…",
                                crate::cli::theme::icon_ok()
                            );
                            crate::cli::turn_facade::execute_basic_cli_turn(
                                &chat_ctx,
                                &new_token,
                                session_id.as_deref(),
                                profile.as_deref(),
                                &mut pm,
                                &mut skill_qt,
                                turn_options.clone(),
                            )
                            .await
                            .map_err(|f| f.error)?
                        } else {
                            return Err(e.error);
                        }
                    } else {
                        return Err(e.error);
                    }
                }
                Err(e) => return Err(e.error),
            };
            if let Some(ref sid) = sr.session_id {
                persist_profile_last_session(profile.as_deref(), sid)?;
            }
            Ok(compute_exit_code(&sr))
        }

        Some(Command::Register(args)) => {
            eprintln!(
                "\n{}",
                "  ── Register a new account ─────────────────────"
                    .magenta()
                    .bold()
            );
            let username = prompt_or("Username", args.username)?;
            let email = prompt_or("Email   ", args.email)?;
            let password = prompt_password_masked("Password", args.password)?;
            do_register(api, profile.as_deref(), &username, &email, &password).await?;
            eprintln!(
                "{}",
                "  ✓  Registered and logged in. Run `astra` to start chatting.".green()
            );
            Ok(ExitCode::Success)
        }

        Some(Command::Login(args)) => {
            eprintln!(
                "\n{}",
                "  ── Login ───────────────────────────────────────"
                    .magenta()
                    .bold()
            );
            let username = prompt_or("Username", args.username)?;
            let password = prompt_password_masked("Password", args.password)?;
            do_login(api, profile.as_deref(), &username, &password).await?;
            eprintln!(
                "{}",
                "  ✓  Logged in. Run `astra` to start chatting.".green()
            );
            Ok(ExitCode::Success)
        }

        Some(Command::Whoami) => {
            let (_, _, _, token) = get_profile_and_token(profile.as_deref())?;
            let body = api.get_auth_me_text(&token).await.map_err(map_thin_err)?;
            print_json_or_raw(&body);
            Ok(ExitCode::Success)
        }

        Some(Command::Refresh) => {
            let creds = load_credentials();
            let name = profile_name(profile.as_deref(), &creds);
            let saved_profile = creds
                .profiles
                .get(&name)
                .cloned()
                .ok_or_else(|| format!("no profile '{name}'"))?;
            let refresh_token = saved_profile
                .refresh_token
                .ok_or_else(|| format!("profile '{name}' has no refresh token"))?;
            let body = api
                .post_auth_refresh_json(&serde_json::json!({ "refresh_token": refresh_token }))
                .await
                .map_err(map_thin_err)?;
            let value: serde_json::Value =
                serde_json::from_str(&body).map_err(|e| e.to_string())?;
            let new_access = value
                .get("access_token")
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| "missing access_token".to_string())?
                .to_string();
            let new_refresh = value
                .get("refresh_token")
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| "missing refresh_token".to_string())?
                .to_string();
            mutate_credentials(|creds| {
                let name = profile_name(profile.as_deref(), creds);
                let entry = creds.profiles.entry(name).or_default();
                entry.access_token = Some(new_access.clone());
                entry.refresh_token = Some(new_refresh.clone());
            })?;
            println!("  {} {}", theme::icon_ok(), "Token refreshed".green());
            Ok(ExitCode::Success)
        }

        Some(Command::Logout) => {
            let creds = load_credentials();
            let name = profile_name(profile.as_deref(), &creds);
            let saved_profile = creds
                .profiles
                .get(&name)
                .cloned()
                .ok_or_else(|| format!("no profile '{name}'"))?;
            let refresh_token = saved_profile
                .refresh_token
                .ok_or_else(|| format!("profile '{name}' has no refresh token"))?;
            let body = api
                .post_auth_logout_json(&serde_json::json!({ "refresh_token": refresh_token }))
                .await
                .map_err(map_thin_err)?;
            mutate_credentials(|creds| {
                let name = profile_name(profile.as_deref(), creds);
                if let Some(entry) = creds.profiles.get_mut(&name) {
                    entry.access_token = None;
                    entry.refresh_token = None;
                    entry.last_session_id = None;
                }
            })?;
            print_json_or_raw(&body);
            Ok(ExitCode::Success)
        }

        Some(Command::Health) => {
            let body = api.get_health_text().await.map_err(map_thin_err)?;
            print_json_or_raw(&body);
            Ok(ExitCode::Success)
        }

        Some(Command::Team(args)) => {
            execute_repl_bridge_command(
                "/team",
                &render_team_args(&args),
                profile.as_deref(),
                global_model.as_deref(),
                api,
                cli_context,
            )
            .await
        }

        Some(Command::Task(mut args)) => match args.command.take() {
            Some(TaskSubcommand::Run(run_args)) => {
                execute_headless_task_run(
                    run_args,
                    profile.as_deref(),
                    global_model.as_deref(),
                    api,
                    cli_context,
                )
                .await
            }
            Some(TaskSubcommand::Queue(queue_args)) => {
                execute_task_queue(queue_args, cli_context).await
            }
            Some(TaskSubcommand::Worker(worker_args)) => {
                execute_task_worker(
                    worker_args,
                    profile.as_deref(),
                    global_model.as_deref(),
                    api,
                    cli_context,
                )
                .await
            }
            Some(TaskSubcommand::Result(result_args)) => execute_task_result(result_args).await,
            _ => {
                execute_repl_bridge_command(
                    "/task",
                    &render_task_args(&args),
                    profile.as_deref(),
                    global_model.as_deref(),
                    api,
                    cli_context,
                )
                .await
            }
        },

        Some(Command::Memory(args)) => {
            execute_repl_bridge_command(
                "/memory",
                &render_memory_args(&args),
                profile.as_deref(),
                global_model.as_deref(),
                api,
                cli_context,
            )
            .await
        }

        Some(Command::Review(args)) => {
            execute_repl_bridge_command(
                "/review",
                &render_review_args(&args),
                profile.as_deref(),
                global_model.as_deref(),
                api,
                cli_context,
            )
            .await
        }

        Some(Command::Grep(args)) => {
            execute_repl_bridge_command(
                "/grep",
                &render_grep_args(&args),
                profile.as_deref(),
                global_model.as_deref(),
                api,
                cli_context,
            )
            .await
        }

        Some(Command::Diff(args)) => {
            execute_repl_bridge_command(
                "/diff",
                &render_diff_args(&args),
                profile.as_deref(),
                global_model.as_deref(),
                api,
                cli_context,
            )
            .await
        }

        Some(Command::Permissions(args)) => {
            execute_repl_bridge_command(
                "/allow",
                &render_permissions_args(&args),
                profile.as_deref(),
                global_model.as_deref(),
                api,
                cli_context,
            )
            .await
        }

        Some(Command::Debug(args)) => {
            execute_repl_bridge_command(
                "/debug",
                &render_debug_args(&args),
                profile.as_deref(),
                global_model.as_deref(),
                api,
                cli_context,
            )
            .await
        }

        Some(Command::Bug(args)) => {
            execute_repl_bridge_command(
                "/bug",
                &render_bug_args(&args),
                profile.as_deref(),
                global_model.as_deref(),
                api,
                cli_context,
            )
            .await
        }

        Some(Command::Agent(args)) => {
            execute_repl_bridge_command(
                "/agent",
                &render_agent_args(&args),
                profile.as_deref(),
                global_model.as_deref(),
                api,
                cli_context,
            )
            .await
        }

        Some(Command::Messaging(args)) => {
            execute_repl_bridge_command(
                "/messaging",
                &render_messaging_args(&args),
                profile.as_deref(),
                global_model.as_deref(),
                api,
                cli_context,
            )
            .await
        }

        Some(Command::Context(ctx_cmd)) => {
            // Forensic `/context dump` — reads a persisted journal
            // and writes a snapshot JSON file (or prints a
            // human-readable summary with `--summary`). No TUI,
            // no REPL — just enough to let users share a full
            // context state from a session that's already been
            // closed.
            match ctx_cmd {
                crate::cli::cli_args::ContextCmd::Dump(args) => {
                    // Resolve session: explicit arg → prefix match;
                    // omitted → most recently touched session on disk.
                    let sid =
                        match crate::cli::context_dump::resolve_session_id(args.session.as_deref())
                        {
                            Ok(s) => s,
                            Err(e) => {
                                eprintln!("context dump: {e}");
                                return Ok(ExitCode::ApiError);
                            }
                        };
                    if args.summary {
                        match crate::cli::context_dump::print_summary(&sid) {
                            Ok(()) => Ok(ExitCode::Success),
                            Err(e) => {
                                eprintln!("context dump failed: {e}");
                                Ok(ExitCode::ApiError)
                            }
                        }
                    } else {
                        match crate::cli::context_dump::write_dump_from_journal(
                            &sid,
                            args.output.as_deref(),
                        ) {
                            Ok(p) => {
                                println!("Context snapshot written to {}", p.display());
                                Ok(ExitCode::Success)
                            }
                            Err(e) => {
                                eprintln!("context dump failed: {e}");
                                Ok(ExitCode::ApiError)
                            }
                        }
                    }
                }
            }
        }

        Some(Command::Chat(args)) => {
            // Handle --no-color or non-terminal stderr: disable ANSI colors via NO_COLOR env.
            // crossterm checks NO_COLOR to suppress escape sequences globally.
            if args.no_color
                || (!std::io::IsTerminal::is_terminal(&std::io::stderr())
                    && std::env::var("NO_COLOR").is_err())
            {
                astra_core::session_env_overlay::set("NO_COLOR", "1");
                // `crossterm` reads the real process environment for ANSI suppression, not the
                // overlay. SAFETY: CLI `/chat` dispatch runs before concurrent tool work; setting
                // `NO_COLOR` here matches the prior single-threaded initialization pattern.
                unsafe {
                    std::env::set_var("NO_COLOR", "1");
                }
            }

            // Determine message source: --stdin, -m, or start REPL
            let message = if args.stdin {
                let mut buf = String::new();
                std::io::stdin()
                    .read_to_string(&mut buf)
                    .map_err(|e| format!("Could not read input from stdin: {e}"))?;
                let msg = buf.trim().to_string();
                if msg.is_empty() {
                    return Err(
                        "message cannot be empty (stdin was empty or whitespace-only)".to_string(),
                    );
                }
                msg
            } else if let Some(m) = args.message {
                if m.trim().is_empty() {
                    return Err("message cannot be empty".to_string());
                }
                m
            } else {
                // No message → start interactive TUI with optional pre-set session/model
                let model = args.model.as_deref().or(global_model.as_deref());
                run_interactive_chat(
                    api,
                    profile.as_deref(),
                    model,
                    None,
                    no_instructions,
                    max_budget,
                    cli_context,
                )
                .await?;
                return Ok(ExitCode::Success);
            };

            let (_, _, _, token) = get_profile_and_token(profile.as_deref())?;
            let session_id = match args.session_id {
                Some(session_id) => Some(session_id),
                None => match cli_context.session_id.clone() {
                    Some(session_id) => Some(session_id),
                    None => validated_resumable_last_session_id(api, profile.as_deref()).await,
                },
            };
            // Load previous conversation for multi-turn continuity.
            let mut continuation_messages = session_id
                .as_deref()
                .and_then(load_session_messages_for_continuation);
            let is_tty = terminal::size().is_ok();
            let _pipeline = create_pipeline_modules(api, profile.as_deref(), astra_config::runtime_config::SessionTraceConfig::default());
            let mut pm = {
                let project_root = std::env::current_dir().unwrap_or_default();
                if let Some(ref mode_str) = args.permission_mode {
                    let mode: PermissionMode = mode_str.parse().unwrap_or_else(|e| {
                        eprintln!("{}", format!("  ⚠  {e}, defaulting to prompt").yellow());
                        PermissionMode::Prompt
                    });
                    PermissionManager::with_load_policy(
                        mode,
                        &project_root,
                        &crate::cli::permission_manager::PermissionLoadPolicy::HeadlessSafe,
                    )
                } else {
                    let mode = if args.auto_approve || auto_approve {
                        PermissionMode::Auto
                    } else {
                        PermissionMode::Prompt
                    };
                    PermissionManager::with_load_policy(
                        mode,
                        &project_root,
                        &crate::cli::permission_manager::PermissionLoadPolicy::HeadlessSafe,
                    )
                }
            };
            let explain_mode = args.explain.unwrap_or(ExplainMode::Off);

            // --json implies --quiet
            let quiet = args.quiet || args.json;
            // When quiet, don't render markdown (no terminal formatting)
            let render_md = is_tty && !quiet;

            let mut skill_qt = astra_skills::quality::SkillQualityTracker::new();
            let skill_search = astra_core::SkillSearchSettings::default();
            let render_policy = if quiet {
                crate::cli::stream_render::RenderPolicy::Silent
            } else {
                crate::cli::stream_render::RenderPolicy::Stream
            };

            // Bug-A fix: build a DynamicAgentSpawner so `astra chat -m`
            // can service `agent(action='spawn', ...)`, matching the REPL
            // path. Without this, one-shot LLM invocations that try
            // to spawn hit "Agent spawning not available in
            // this context." — discovered during real-world MiniMax
            // verification. Mirrors the REPL's
            // `initialize_multi_agent_runtime` wiring via the
            // extracted `build_one_shot_spawner` helper so the
            // fork-prefix pipeline is identically configured.
            let root_agent_id = format!("root-{}", uuid::Uuid::new_v4());
            let one_shot_spawner = super::agent_runtime::build_one_shot_spawner(
                api,
                token.clone(),
                astra_runtime::skills::default_unified_registry().clone(),
                pm.mode(),
                skill_search.clone(),
                session_id.clone(),
                args.model
                    .as_deref()
                    .or(global_model.as_deref())
                    .map(str::to_owned),
            )
            .await;

            // Keep a clone of the Arc so we can drain background
            // spawned children before process exit — otherwise
            // background tasks (the default background-agent mode) get
            // aborted when main returns, which silently drops any
            // ForkCacheEvent / child telemetry they would have
            // emitted on their first response.
            let spawner_handle_for_drain = one_shot_spawner.clone();
            let (stream_event_tx, _stream_event_writer) = if args.stream_events {
                let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
                let handle = crate::cli::stream_events_writer::spawn_stderr_writer(rx);
                (Some(tx), Some(handle))
            } else {
                (None, None)
            };
            #[cfg(feature = "harness")]
            let harness_sink = astra_harness::InMemorySnapshotSink::arc();
            #[cfg(feature = "harness")]
            let harness_trace = std::sync::Arc::new(std::sync::RwLock::new(
                astra_harness::SessionTrace::new(None),
            ));
            // Wire the MO-backed task store for `astra chat -m` single-shot
            // runs so `task_create` / `task_list` in this path write through
            // to `session_todos`. Without this the tool runs against a
            // throwaway in-memory manager and the Tier 1 board is invisible
            // across edge/cloud boundaries.
            let (chat_task_store, _chat_task_notify_tx) =
                super::session_runtime::resolve_task_store(
                    profile.as_deref(),
                    Some(&api.api_origin()),
                )
                .await;
            let chat_task_manager = std::sync::Arc::new(crate::edge_tools::TaskManager::new(
                session_id
                    .clone()
                    .unwrap_or_else(|| "no-session".to_string()),
                chat_task_store,
            ));
            let chat_ctx = crate::cli::chat_stream::BasicCliChatContext {
                api,
                auth_profile: profile.as_deref(),
                message: &message,
                model: args.model.as_deref().or(global_model.as_deref()),
                provider: None,
                explain: explain_mode,
                render_md,
                verbose_mode: !quiet,
                render_policy,
                cli_context: Some(cli_context),
                unified_skill_registry: astra_runtime::skills::default_unified_registry(),
                skill_search: &skill_search,
                agent_spawner: Some(one_shot_spawner),
                root_agent_id: Some(&root_agent_id),
                task_manager: Some(chat_task_manager),
                task_notify_tx: None,
                bg_task_commands: None,
                bash_detach_slot: None,
                stream_event_tx,
                #[cfg(feature = "harness")]
                harness_sink: Some(harness_sink.clone()),
                #[cfg(feature = "harness")]
                harness_trace: Some(harness_trace),
            };
            let turn_options = crate::cli::turn_facade::BasicCliTurnOptions {
                pre_loaded_messages: continuation_messages.take(),
                append_system_prompt: args.append_system_prompt.clone(),
                ..Default::default()
            };
            let turn_start = std::time::Instant::now();
            let mut sr = match crate::cli::turn_facade::execute_basic_cli_turn(
                &chat_ctx,
                &token,
                session_id.as_deref(),
                profile.as_deref(),
                &mut pm,
                &mut skill_qt,
                turn_options,
            )
            .await
            {
                Ok(sr) => sr,
                Err(e) => return Err(e.error),
            };

            // Save session for resumption
            if let Some(sid) = &sr.session_id {
                persist_profile_last_session(profile.as_deref(), sid)?;
            }
            super::chat_turn::append_one_shot_journal_events(
                sr.session_id.as_deref(),
                args.model.as_deref().or(global_model.as_deref()),
                &message,
                &sr,
                turn_start,
            );

            // Drain any background-spawned child agents before
            // returning. Without this, background tasks (the
            // default background-agent mode) are aborted when main
            // returns, which silently drops any ForkCacheEvent /
            // child output they would have emitted. Deadline is
            // bounded so a misbehaving child can't hang the CLI;
            // tasks exceeding it are aborted with a log warning.
            //
            // We drain BEFORE writing result to stdout so the
            // [fork-cache] stderr lines (if any) appear before the
            // JSON/text result — operators grepping stderr don't
            // see the order swap.
            sr.background_agent_results = spawner_handle_for_drain
                .shutdown_and_wait(std::time::Duration::from_secs(30))
                .await;

            // Drain stream event writer: drop sender, then await writer task
            // so all JSONL events are flushed to stderr before stdout output.
            drop(chat_ctx);
            if let Some(handle) = _stream_event_writer {
                let _ = handle.await;
            }

            #[cfg(feature = "harness")]
            append_headless_inspect_snapshot(&mut sr, &message, &harness_sink);

            // Output result
            if args.json {
                // Compute exit code for JSON output
                let exit_code = compute_exit_code(&sr);
                // Pure JSON output for scripting
                let mut json_output = final_json_output(&sr, exit_code);
                if let Some(obj) = json_output.as_object_mut() {
                    obj.insert("ttft_ms".to_string(), serde_json::json!(sr.ttft_ms));
                    obj.insert("context_ms".to_string(), serde_json::json!(sr.context_ms));
                    obj.insert(
                        "background_agent_results".to_string(),
                        serde_json::json!(
                            sr.background_agent_results
                                .iter()
                                .map(
                                    |(id, text)| serde_json::json!({"agent_id": id, "result": text})
                                )
                                .collect::<Vec<_>>()
                        ),
                    );
                }
                println!(
                    "{}",
                    serde_json::to_string_pretty(&json_output).unwrap_or_default()
                );
                return Ok(exit_code);
            } else if quiet {
                // Quiet mode: just print the text without formatting
                println!("{}", sr.full_text);
            }
            // Normal mode output is already handled by stream_chat_sse

            Ok(compute_exit_code(&sr))
        }

        Some(Command::Replay(args)) => {
            let (_, _, _, token) = get_profile_and_token(profile.as_deref())?;
            let replay_body = api
                .post_session_replay_json(
                    &token,
                    &args.session_id,
                    &serde_json::json!({
                        "sandbox_name": args.sandbox_name,
                        "mock_mode": args.mock_mode
                    }),
                )
                .await
                .map_err(map_thin_err)?;
            print_json_or_raw(&replay_body);
            if args.compare {
                let compare_body = api
                    .get_session_replay_compare_text(&token, &args.session_id)
                    .await
                    .map_err(map_thin_err)?;
                print_json_or_raw(&compare_body);
            }
            Ok(ExitCode::Success)
        }

        Some(Command::Session(SessionCmd::List(args))) => {
            let (_, _, _, token) = get_profile_and_token(profile.as_deref())?;
            let mut q: Vec<(&str, String)> = vec![
                ("limit", args.limit.to_string()),
                ("offset", args.offset.to_string()),
            ];
            if let Some(ref agent_id) = args.agent_id {
                q.push(("agent_id", agent_id.clone()));
            }
            if let Some(ref status) = args.status {
                q.push(("session_status", status.clone()));
            }
            let body = api
                .get_sessions_query_text(&token, &q)
                .await
                .map_err(map_thin_err)?;
            print_json_or_raw(&body);
            Ok(ExitCode::Success)
        }

        Some(Command::Session(SessionCmd::Show(args))) => {
            let (_, _, _, token) = get_profile_and_token(profile.as_deref())?;
            let body = api
                .get_session_text(&token, &args.session_id)
                .await
                .map_err(map_thin_err)?;
            print_json_or_raw(&body);
            Ok(ExitCode::Success)
        }

        Some(Command::Session(SessionCmd::Close(args))) => {
            let (creds, name, _, token) = get_profile_and_token(profile.as_deref())?;
            let body = api
                .post_session_close_text(&token, &args.session_id)
                .await
                .map_err(map_thin_err)?;
            if creds
                .profiles
                .get(&name)
                .and_then(|profile| profile.last_session_id.as_deref())
                == Some(args.session_id.as_str())
            {
                let _ = clear_profile_last_session(profile.as_deref());
            }
            print_json_or_raw(&body);
            Ok(ExitCode::Success)
        }

        Some(Command::Session(SessionCmd::Delete(args))) => {
            let (creds, name, _, token) = get_profile_and_token(profile.as_deref())?;
            let body = api
                .delete_session_text(&token, &args.session_id)
                .await
                .map_err(map_thin_err)?;
            if creds
                .profiles
                .get(&name)
                .and_then(|profile| profile.last_session_id.as_deref())
                == Some(args.session_id.as_str())
            {
                let _ = clear_profile_last_session(profile.as_deref());
            }
            if body.is_empty() {
                println!("  {} {}", theme::icon_ok(), "Deleted".green());
            } else {
                print_json_or_raw(&body);
            }
            Ok(ExitCode::Success)
        }

        Some(Command::Session(SessionCmd::Capture(SessionCaptureCmd::Latest(args)))) => {
            let (_, _, _, token) = get_profile_and_token(profile.as_deref())?;
            let session_id =
                resolve_remote_session_id(api, profile.as_deref(), args.session_id.as_deref())
                    .await?;
            let body = api
                .get_session_artifact_latest_text(&token, &session_id, &args.artifact_kind)
                .await
                .map_err(map_thin_err)?;
            print_json_or_raw(&body);
            Ok(ExitCode::Success)
        }

        Some(Command::Session(SessionCmd::Capture(SessionCaptureCmd::Download(args)))) => {
            let (_, _, _, token) = get_profile_and_token(profile.as_deref())?;
            let session_id =
                resolve_remote_session_id(api, profile.as_deref(), args.session_id.as_deref())
                    .await?;
            let latest_body = api
                .get_session_artifact_latest_text(&token, &session_id, &args.artifact_kind)
                .await
                .map_err(map_thin_err)?;
            let artifact_id = latest_artifact_id(&latest_body)?;
            let (bytes, suggested_name) = api
                .download_session_artifact(&token, &session_id, &artifact_id)
                .await
                .map_err(map_thin_err)?;
            let fallback_name = format!("{}_{}.json", args.artifact_kind, artifact_id);
            let output_path = resolve_download_output_path(
                args.output.as_deref(),
                suggested_name.as_deref().unwrap_or(&fallback_name),
            );
            write_downloaded_capture(&output_path, &bytes)?;
            println!(
                "{} Saved latest {} for session {} to {}",
                theme::icon_ok(),
                args.artifact_kind,
                session_id,
                output_path.display()
            );
            Ok(ExitCode::Success)
        }

        Some(Command::SelfInspect(cmd)) => {
            let body =
                crate::cli::self_command::execute_self_command(&cmd, profile.as_deref()).await?;
            print_json_or_raw(&body);
            Ok(ExitCode::Success)
        }

        Some(Command::Model(ModelCmd::List)) => {
            let (_, _, _, token) = get_profile_and_token(profile.as_deref())?;
            let body = api.get_models_text(&token).await.map_err(map_thin_err)?;
            print_json_or_raw(&body);
            Ok(ExitCode::Success)
        }

        Some(Command::Model(ModelCmd::Show(args))) => {
            let (_, _, _, token) = get_profile_and_token(profile.as_deref())?;
            let body = api
                .get_model_text(&token, &args.model_name)
                .await
                .map_err(map_thin_err)?;
            print_json_or_raw(&body);
            Ok(ExitCode::Success)
        }

        Some(Command::Skill(SkillCmd::List(args))) => {
            let pipeline_modules = create_pipeline_modules_quiet(api, profile.as_deref(), astra_config::runtime_config::SessionTraceConfig::default());
            let filter = SkillCatalogFilter {
                query: (!args.query.is_empty()).then(|| args.query.join(" ").to_lowercase()),
                source: args
                    .source
                    .as_deref()
                    .map(normalize_source_filter)
                    .transpose()?,
                category: args
                    .category
                    .as_ref()
                    .map(|category| category.to_lowercase()),
            };
            let body = serde_json::to_string(&list_skill_record_from_registry(
                &pipeline_modules.unified_skill_registry,
                &filter,
                args.limit,
                args.offset,
            ))
            .map_err(|source| format!("failed to serialize skill list: {source}"))?;
            print_json_or_raw(&body);
            Ok(ExitCode::Success)
        }

        Some(Command::Skill(SkillCmd::Show(args))) => {
            let pipeline_modules = create_pipeline_modules_quiet(api, profile.as_deref(), astra_config::runtime_config::SessionTraceConfig::default());
            let body = serde_json::to_string(
                &load_skill_record_from_registry(
                    &pipeline_modules.unified_skill_registry,
                    &args.skill_id,
                    args.version.as_deref(),
                )
                .await?,
            )
            .map_err(|source| format!("failed to serialize skill record: {source}"))?;
            print_json_or_raw(&body);
            Ok(ExitCode::Success)
        }

        Some(Command::Skill(SkillCmd::Status(args))) => {
            let (_, _, _, token) = get_profile_and_token(profile.as_deref())?;
            let q = vec![("per_group", args.per_group.to_string())];
            let body = api
                .get_skills_status_query_text(&token, &q)
                .await
                .map_err(map_thin_err)?;
            print_json_or_raw(&body);
            Ok(ExitCode::Success)
        }

        Some(Command::Skill(SkillCmd::Register(args))) => {
            let (_, _, _, token) = get_profile_and_token(profile.as_deref())?;
            let skill_code = match (args.code, args.code_file) {
                (Some(code), None) => code,
                (None, Some(path)) => fs::read_to_string(path).map_err(|e| e.to_string())?,
                (Some(_), Some(_)) => {
                    return Err("provide either --code or --code-file, not both".to_string());
                }
                (None, None) => {
                    return Err("missing skill code: set --code or --code-file".to_string());
                }
            };
            let metadata = if let Some(raw) = args.metadata_json {
                Some(serde_json::from_str::<serde_json::Value>(&raw).map_err(|e| e.to_string())?)
            } else {
                None
            };
            let skill_id = args
                .skill_id
                .unwrap_or_else(|| format!("{}@{}", args.name, args.version));
            let body = api
                .post_skills_register_json(
                    &token,
                    &serde_json::json!({
                        "skill_id": skill_id,
                        "skill_name": args.name,
                        "skill_version": args.version,
                        "skill_code": skill_code,
                        "description": args.description,
                        "metadata": metadata
                    }),
                )
                .await
                .map_err(map_thin_err)?;
            print_json_or_raw(&body);
            Ok(ExitCode::Success)
        }

        // ── Audit commands ──────────────────────────────────────────────────
        Some(Command::Audit(AuditCmd::List(args))) => {
            let (_, _, _, token) = get_profile_and_token(profile.as_deref())?;
            let mut q: Vec<(&str, String)> = vec![
                ("page", args.page.to_string()),
                ("per_page", args.limit.to_string()),
                ("sort", args.sort.clone()),
            ];
            if let Some(ref s) = args.status {
                q.push(("status", s.clone()));
            }
            if let Some(ref m) = args.model {
                q.push(("model", m.clone()));
            }
            if let Some(ref s) = args.since {
                q.push(("since", s.clone()));
            }
            if let Some(ref u) = args.until {
                q.push(("until", u.clone()));
            }
            if let Some(mt) = args.min_turns {
                q.push(("min_turns", mt.to_string()));
            }
            let body = api
                .get_bearer_path_query_text(&token, paths::AUDIT_SESSIONS, &q)
                .await
                .map_err(map_thin_err)?;
            print_json_or_raw(&body);
            Ok(ExitCode::Success)
        }

        Some(Command::Audit(AuditCmd::Show(args))) => {
            let (_, _, _, token) = get_profile_and_token(profile.as_deref())?;
            let body = api
                .get_bearer_path_query_text(
                    &token,
                    &paths::session_audit_summary(&args.session_id),
                    &[],
                )
                .await
                .map_err(map_thin_err)?;
            print_json_or_raw(&body);
            Ok(ExitCode::Success)
        }

        Some(Command::Audit(AuditCmd::Turns(args))) => {
            let (_, _, _, token) = get_profile_and_token(profile.as_deref())?;
            let body = if let Some(turn) = args.turn {
                api.get_bearer_path_query_text(
                    &token,
                    &paths::session_audit_turn_detail(&args.session_id, turn),
                    &[],
                )
                .await
            } else {
                let q = vec![
                    ("page", args.page.to_string()),
                    ("per_page", args.per_page.to_string()),
                ];
                api.get_bearer_path_query_text(
                    &token,
                    &paths::session_audit_turns(&args.session_id),
                    &q,
                )
                .await
            }
            .map_err(map_thin_err)?;
            print_json_or_raw(&body);
            Ok(ExitCode::Success)
        }

        Some(Command::Audit(AuditCmd::Tools(args))) => {
            let (_, _, _, token) = get_profile_and_token(profile.as_deref())?;
            let body = if let Some(ref sid) = args.session_id {
                api.get_bearer_path_query_text(&token, &paths::session_audit_tools(sid), &[])
                    .await
            } else {
                let mut q: Vec<(&str, String)> = Vec::new();
                if let Some(ref s) = args.since {
                    q.push(("since", s.clone()));
                }
                if let Some(ref u) = args.until {
                    q.push(("until", u.clone()));
                }
                api.get_bearer_path_query_text(&token, paths::AUDIT_TOOLS, &q)
                    .await
            }
            .map_err(map_thin_err)?;
            print_json_or_raw(&body);
            Ok(ExitCode::Success)
        }

        Some(Command::Journal(JournalCmd::Digest(args))) => {
            journal_digest::run_digest(&args)?;
            Ok(ExitCode::Success)
        }

        Some(Command::Journal(JournalCmd::Tree(args))) => {
            journal_tree::run_tree(&args)?;
            Ok(ExitCode::Success)
        }

        Some(Command::Journal(JournalCmd::Diff(args))) => {
            journal_diff::run_diff(&args)?;
            Ok(ExitCode::Success)
        }

        // ── MCP server management (offline, no server needed) ──────────
        Some(Command::Mcp(mcp_cmd)) => {
            execute_mcp_command(mcp_cmd)?;
            Ok(ExitCode::Success)
        }

        // ── Shell completion script generation ──────────────────────────
        Some(Command::Completion(args)) => {
            let mut cmd = Cli::command();
            clap_complete::generate(args.shell, &mut cmd, "astra", &mut std::io::stdout());
            Ok(ExitCode::Success)
        }

        // ── Doctor: diagnose installation and config ────────────────────
        Some(Command::Doctor) => {
            run_doctor(api, profile.as_deref()).await;
            Ok(ExitCode::Success)
        }

        // ── Config management ───────────────────────────────────────────
        Some(Command::Config(cfg_cmd)) => {
            execute_config_command(cfg_cmd).await?;
            Ok(ExitCode::Success)
        }
    }
}

/// Compute exit code from StreamResult using semantic exit classification.
///
/// Tool-call records carry an optional `exit_semantics` field (snake_case
/// serialization of [`astra_tools::exit_semantics::ExitSemantics`]) that
/// distinguishes real execution errors from domain-negative outcomes
/// (grep no-match, diff differences, test failures). This function reads
/// that field to avoid treating those as tool failures — a grep that
/// finds nothing or a diff that reports differences is a successful tool
/// execution, not an error the agent needs to recover from.
fn compute_exit_code(sr: &StreamResult) -> ExitCode {
    // ── Force stop (highest priority) ──────────────────────────────────
    for ve in &sr.verdict_events {
        if ve.force_stop {
            return ExitCode::ForceStop;
        }
    }

    // ── Semantic classification of each tool call ──────────────────────
    let is_error = |r: &astra_services::session_journal::ToolCallRecord| -> bool {
        match r
            .exit_semantics
            .as_deref()
            .and_then(parse_exit_semantics_tag)
        {
            // ExecutionError is a genuine tool failure (command crashed,
            // permission denied, signal kill, unknown command, etc.)
            Some(astra_tools::exit_semantics::ExitSemantics::ExecutionError) => true,
            // Success, InformationalFailure (grep no-match), and
            // DomainNegative (diff differences, test failures) are all
            // intentional domain outcomes — the tool worked correctly.
            Some(
                astra_tools::exit_semantics::ExitSemantics::Success
                | astra_tools::exit_semantics::ExitSemantics::InformationalFailure
                | astra_tools::exit_semantics::ExitSemantics::DomainNegative,
            ) => false,
            // Unknown or missing semantics fall back to the legacy ok flag.
            // That keeps malformed records from silently downgrading a real
            // tool failure into success.
            None => !r.ok,
        }
    };

    // Check for unrecovered tool failures. Agents self-correct by
    // retrying with the same or different tools (write_file fails →
    // bash echo succeeds). Only fail if the agent never recovered —
    // i.e. the last error was not followed by a successful call.
    let has_any_failure = sr.tool_call_records.iter().any(&is_error);
    if has_any_failure {
        let last_ok = sr
            .tool_call_records
            .iter()
            .rev()
            .find(|r| !is_error(r))
            .is_some();
        let last_ok_explicit = sr
            .tool_call_records
            .last()
            .map(|r| !is_error(r))
            .unwrap_or(true);
        if !last_ok || !last_ok_explicit {
            return ExitCode::ToolFailure;
        }
    }

    ExitCode::Success
}

fn parse_exit_semantics_tag(tag: &str) -> Option<astra_tools::exit_semantics::ExitSemantics> {
    serde_json::from_value::<astra_tools::exit_semantics::ExitSemantics>(serde_json::Value::String(
        tag.to_string(),
    ))
    .ok()
}

fn error_kind_for_exit_code(exit_code: ExitCode) -> Option<&'static str> {
    match exit_code {
        ExitCode::Success => None,
        ExitCode::ToolFailure => Some("tool_failure"),
        ExitCode::ForceStop => Some("force_stop"),
        ExitCode::ApiError => Some("api_error"),
    }
}

fn gateway_env_context() -> (Option<String>, Option<String>) {
    (
        std::env::var("ASTRA_GATEWAY_TRACE_ID")
            .ok()
            .filter(|value| !value.is_empty()),
        std::env::var("ASTRA_GATEWAY_REQUEST_ID")
            .ok()
            .filter(|value| !value.is_empty()),
    )
}

fn final_json_output(sr: &StreamResult, exit_code: ExitCode) -> serde_json::Value {
    let (trace_id, request_id) = gateway_env_context();
    final_json_output_with_context(sr, exit_code, trace_id, request_id)
}

#[cfg(feature = "harness")]
fn message_requests_headless_inspect(message: &str) -> bool {
    fn normalize(raw: &str) -> String {
        raw.trim_matches(|c: char| {
            matches!(
                c,
                '"' | '\''
                    | '`'
                    | '('
                    | ')'
                    | '['
                    | ']'
                    | '{'
                    | '}'
                    | ','
                    | '.'
                    | ':'
                    | ';'
                    | '!'
                    | '?'
            )
        })
        .to_ascii_lowercase()
    }

    for line in message.lines() {
        if line.split_whitespace().next().map(normalize).as_deref() == Some("/inspect") {
            return true;
        }
    }

    let tokens: Vec<String> = message.split_whitespace().map(normalize).collect();
    tokens.iter().enumerate().any(|(idx, token)| {
        if token != "/inspect" {
            return false;
        }
        let prev = idx
            .checked_sub(1)
            .and_then(|i| tokens.get(i).map(String::as_str));
        let prev2 = idx
            .checked_sub(2)
            .and_then(|i| tokens.get(i).map(String::as_str));
        matches!(
            prev,
            Some("use" | "run" | "execute" | "invoke" | "call" | "show" | "append" | "appends")
        ) || (prev == Some("the")
            && matches!(
                prev2,
                Some(
                    "use"
                        | "run"
                        | "execute"
                        | "invoke"
                        | "call"
                        | "show"
                        | "append"
                        | "appends"
                        | "appended"
                )
            ))
    })
}

#[cfg(feature = "harness")]
fn append_headless_inspect_snapshot(
    sr: &mut StreamResult,
    message: &str,
    sink: &std::sync::Arc<astra_harness::InMemorySnapshotSink>,
) {
    if !message_requests_headless_inspect(message) {
        return;
    }

    use astra_harness::SnapshotSink;
    let snapshot_text = match sink.latest() {
        Some(snapshot) => slash_inspect::format_snapshot_summary(&snapshot),
        None => "No harness snapshot available yet.".to_string(),
    };

    if !sr.full_text.is_empty() && !sr.full_text.ends_with('\n') {
        sr.full_text.push('\n');
    }
    if !sr.full_text.is_empty() {
        sr.full_text.push('\n');
    }
    sr.full_text.push_str(&snapshot_text);
}

fn final_json_output_with_context(
    sr: &StreamResult,
    exit_code: ExitCode,
    trace_id: Option<String>,
    request_id: Option<String>,
) -> serde_json::Value {
    let total_prompt_tokens = sr.prompt_tokens + sr.cache_read_tokens + sr.cache_creation_tokens;
    serde_json::json!({
        "trace_id": trace_id,
        "request_id": request_id,
        "run_id": sr.run_id,
        "session_id": sr.session_id,
        "text": sr.full_text,
        "prompt_tokens": total_prompt_tokens,
        "fresh_prompt_tokens": sr.prompt_tokens,
        "cache": {
            "hit": sr.cache_read_tokens > 0,
            "read_tokens": sr.cache_read_tokens,
            "creation_tokens": sr.cache_creation_tokens,
        },
        "completion_tokens": sr.completion_tokens,
        "tool_calls_count": sr.tool_calls_count,
        "tools_used": sr.tools_used,
        "exit_code": i32::from(exit_code),
        "success": exit_code == ExitCode::Success,
        "error_kind": error_kind_for_exit_code(exit_code),
    })
}

/// `--print` / `-p` mode: headless single-shot query, prints response and exits.
/// Reads message from positional args (Message variant) or stdin.
pub(crate) async fn run_print_mode(
    api: &astra_thin_client::ThinClient,
    profile: Option<&str>,
    output_format: &str,
    model: Option<&str>,
    system_prompt: Option<&str>,
    command: Option<Command>,
    cli_context: &crate::cli::cli_context::CliContext,
) -> Result<ExitCode, String> {
    // Extract message from command or stdin
    let raw_message = match command {
        Some(Command::Message(words)) if !words.is_empty() => words.join(" "),
        _ => {
            // Try reading from stdin
            let mut buf = String::new();
            std::io::stdin()
                .read_to_string(&mut buf)
                .map_err(|e| format!("Failed to read stdin: {e}"))?;
            let msg = buf.trim().to_string();
            if msg.is_empty() {
                return Err(
                    "Print mode requires a message. Usage: astra -p \"question\" or echo \"question\" | astra -p"
                        .to_string(),
                );
            }
            msg
        }
    };
    let message = apply_system_prompt(&raw_message, system_prompt);

    let (_, _, _, token) = get_profile_and_token(profile)?;
    let session_id = match cli_context.session_id.clone() {
        Some(session_id) => Some(session_id),
        None => validated_resumable_last_session_id(api, profile).await,
    };
    let mut continuation_messages = session_id
        .as_deref()
        .and_then(load_session_messages_for_continuation);
    let _pipeline = create_pipeline_modules(api, profile, astra_config::runtime_config::SessionTraceConfig::default());
    // Issue #326 P0 / R1 Major 2: print mode (headless `astra -p`) is
    // non-interactive — there is no TUI to ask for approvals. We force
    // `auto_approve = true` (= PermissionMode::Auto) here. The
    // bypass-immune deny rules (sensitive paths, git-destructive,
    // execute hard-deny, sandbox circuit breaker) still fire in Auto
    // mode; this only avoids popping a non-existent prompt. If a tool genuinely requires
    // NeedApproval (e.g. compensation prompts after a denial), the
    // gate fans out to silent-fail-closed in stream_render.rs (line
    // ~1983), surfacing the deny reason to the LLM instead of hanging.
    // Issue #326 P5b: print mode is headless — strip project
    // allow rules so a hostile project file can't quietly enable
    // capabilities the user didn't ask for. Project deny rules
    // still apply (a project can tighten, never loosen, the
    // headless policy).
    let mut pm = PermissionManager::with_load_policy(
        crate::cli::permission_manager::PermissionMode::Auto,
        &std::env::current_dir().unwrap_or_default(),
        &crate::cli::permission_manager::PermissionLoadPolicy::HeadlessSafe,
    );
    // Surface load_errors as exit-1: a corrupt project permissions.json
    // in CI must not silently fall back to "no rules" (issue #326 P0
    // task #12 / scenario #34).
    if !pm.load_errors().is_empty() {
        for err in pm.load_errors() {
            eprintln!("astra: {err}");
        }
        return Ok(ExitCode::ToolFailure);
    }
    let mut skill_qt = astra_skills::quality::SkillQualityTracker::new();
    let skill_search = astra_core::SkillSearchSettings::default();

    // Print mode wires an MO-backed TaskManager when available so that the
    // `task` tool's writes land in `session_todos` the same way the REPL
    // path handles them. Without this, single-shot runs silently drop to
    // in-memory scratchpad and the Tier 1 board is invisible across turns
    // that reuse the same `session_id`.
    let task_store =
        crate::cli::session_runtime::resolve_task_store(profile, Some(&api.api_origin()))
            .await
            .0;
    let print_task_manager = std::sync::Arc::new(crate::edge_tools::TaskManager::new(
        session_id
            .clone()
            .unwrap_or_else(|| "no-session".to_string()),
        task_store,
    ));

    let chat_ctx = crate::cli::chat_stream::BasicCliChatContext {
        api,
        auth_profile: profile,
        message: &message,
        model,
        provider: None,
        explain: ExplainMode::Off,
        render_md: false,
        verbose_mode: false,
        render_policy: crate::cli::stream_render::RenderPolicy::Silent,
        cli_context: Some(cli_context),
        unified_skill_registry: astra_runtime::skills::default_unified_registry(),
        skill_search: &skill_search,
        agent_spawner: None,
        root_agent_id: None,
        task_manager: Some(print_task_manager),
        task_notify_tx: None,
        bg_task_commands: None,
        bash_detach_slot: None,
        stream_event_tx: None,
        #[cfg(feature = "harness")]
        harness_sink: Some(astra_harness::InMemorySnapshotSink::arc()),
        #[cfg(feature = "harness")]
        harness_trace: Some(std::sync::Arc::new(std::sync::RwLock::new(
            astra_harness::SessionTrace::new(None),
        ))),
    };

    let turn_options = crate::cli::turn_facade::BasicCliTurnOptions {
        pre_loaded_messages: continuation_messages.take(),
        ..Default::default()
    };
    let sr = match crate::cli::turn_facade::execute_basic_cli_turn(
        &chat_ctx,
        &token,
        session_id.as_deref(),
        profile,
        &mut pm,
        &mut skill_qt,
        turn_options,
    )
    .await
    {
        Ok(sr) => sr,
        Err(e) => return Err(e.error),
    };

    // Save session for resumption
    if let Some(sid) = &sr.session_id {
        persist_profile_last_session(profile, sid)?;
    }

    let exit_code = compute_exit_code(&sr);

    match output_format {
        "json" | "stream-json" => {
            let json_output = final_json_output(&sr, exit_code);
            println!(
                "{}",
                serde_json::to_string_pretty(&json_output).unwrap_or_default()
            );
        }
        _ => {
            // text mode: just the response
            print!("{}", sr.full_text);
        }
    }

    Ok(exit_code)
}

// ═══════════════════════════════════════════════════════ Doctor ═══════════

async fn run_doctor(api: &astra_thin_client::ThinClient, profile: Option<&str>) {
    println!("\n{}", "Astra Doctor".bold());
    println!("{}\n", "═".repeat(50).dim());
    let mut issues: Vec<String> = Vec::new();

    // 1. Version
    let version = env!("CARGO_PKG_VERSION");
    println!("{}", "Version".bold().magenta());
    println!("  {} {}", "Binary:".dim(), version);
    println!(
        "  {} {}",
        "Executable:".dim(),
        std::env::current_exe()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| "unknown".into())
    );
    println!();

    // 2. API server connectivity
    println!("{}", "API Server".bold().magenta());
    println!("  {} {}", "URL:".dim(), api.api_origin());
    match api.get_health_text().await {
        Ok(body) => println!(
            "  {} {} {}",
            "Status:".dim(),
            theme::icon_ok(),
            format!("Healthy ({})", body.trim()).green()
        ),
        Err(e) => {
            println!(
                "  {} {} {}",
                "Status:".dim(),
                "✗".red(),
                "Unreachable".red()
            );
            issues.push(format!("API server unreachable: {e}"));
        }
    }
    println!();

    // 3. Authentication
    println!("{}", "Authentication".bold().magenta());
    let creds = load_credentials();
    let name = profile_name(profile, &creds);
    println!("  {} {}", "Profile:".dim(), name);
    match get_profile_and_token(profile) {
        Ok((_, _, _, token)) => {
            match api.get_auth_me_text(&token).await {
                Ok(body) => {
                    // Try to extract username from JSON response
                    if let Ok(val) = serde_json::from_str::<serde_json::Value>(&body) {
                        let user = val
                            .get("username")
                            .or_else(|| val.get("name"))
                            .and_then(|v| v.as_str())
                            .unwrap_or("authenticated");
                        println!(
                            "  {} {} {}",
                            "Status:".dim(),
                            theme::icon_ok(),
                            format!("Logged in as {user}").green()
                        );
                    } else {
                        println!(
                            "  {} {} {}",
                            "Status:".dim(),
                            theme::icon_ok(),
                            "Authenticated".green()
                        );
                    }
                }
                Err(_) => {
                    println!(
                        "  {} {} {}",
                        "Status:".dim(),
                        theme::icon_warn(),
                        "Token may be expired".yellow()
                    );
                    issues.push(
                        "Auth token may be expired — try `astra refresh` or `astra login`".into(),
                    );
                }
            }
        }
        Err(e) => {
            println!(
                "  {} {} {}",
                "Status:".dim(),
                "✗".red(),
                "Not logged in".red()
            );
            issues.push(format!("Not authenticated: {e}"));
        }
    }
    println!();

    // 4. Project config
    println!("{}", "Project Configuration".bold().magenta());
    let cwd = std::env::current_dir().unwrap_or_default();
    let astra_dir = cwd.join(".astra");
    if astra_dir.is_dir() {
        println!(
            "  {} {} {}",
            ".astra/:".dim(),
            theme::icon_ok(),
            "Found".green()
        );
    } else {
        println!("  {} {}", ".astra/:".dim(), "Not found (optional)".dim());
    }
    println!("  {} {}", "Working dir:".dim(), cwd.display());
    println!();

    // 5. MCP configuration
    println!("{}", "MCP Configuration".bold().magenta());
    for (scope, path_fn) in &[
        (
            "project",
            crate::manifest_loader::project_mcp_json_path as fn() -> Option<std::path::PathBuf>,
        ),
        (
            "user",
            crate::manifest_loader::global_mcp_json_path as fn() -> Option<std::path::PathBuf>,
        ),
    ] {
        if let Some(path) = path_fn() {
            if path.is_file() {
                match std::fs::read_to_string(&path) {
                    Ok(content) => match serde_json::from_str::<serde_json::Value>(&content) {
                        Ok(config) => {
                            let count = config
                                .get("mcpServers")
                                .and_then(|v| v.as_object())
                                .map(|m| m.len())
                                .unwrap_or(0);
                            println!(
                                "  {} {} {} in {}",
                                scope,
                                theme::icon_ok(),
                                format!("{count} server(s)").green(),
                                path.display().to_string().dim()
                            );
                        }
                        Err(e) => {
                            println!(
                                "  {} {} {}",
                                scope,
                                "✗".red(),
                                format!("Invalid JSON in {}", path.display()).red()
                            );
                            issues.push(format!("MCP {scope} config parse error: {e}"));
                        }
                    },
                    Err(e) => {
                        println!(
                            "  {} {} {}",
                            scope,
                            "✗".red(),
                            format!("Cannot read {}", path.display()).red()
                        );
                        issues.push(format!("MCP {scope} config read error: {e}"));
                    }
                }
            } else {
                println!("  {} {}", scope, "No config file".dim());
            }
        }
    }
    println!();

    // 6. Environment
    println!("{}", "Environment".bold().magenta());
    println!("  {} {}", "OS:".dim(), std::env::consts::OS);
    println!("  {} {}", "Arch:".dim(), std::env::consts::ARCH);
    if let Ok(shell) = std::env::var("SHELL") {
        println!("  {} {shell}", "Shell:".dim());
    }
    if let Ok(term) = std::env::var("TERM") {
        println!("  {} {term}", "Terminal:".dim());
    }
    println!();

    // Summary
    if issues.is_empty() {
        println!("{} {}", theme::icon_ok().bold(), "No issues found".green());
    } else {
        println!(
            "{} {}:",
            "Found".yellow(),
            format!("{} issue(s)", issues.len()).yellow().bold()
        );
        for issue in &issues {
            println!("  {} {}", theme::icon_warn(), issue);
        }
    }
}

#[cfg(test)]
mod exit_code_tests {
    use super::*;

    fn empty_stream_result() -> StreamResult {
        StreamResult {
            session_id: None,
            run_id: None,
            full_text: String::new(),
            prompt_tokens: 0,
            completion_tokens: 0,
            cache_read_tokens: 0,
            cache_creation_tokens: 0,
            tool_calls_count: 0,
            tools_selected: vec![],
            selected_skills: vec![],
            tools_used: vec![],
            tool_call_records: vec![],
            budget_used: 0,
            budget_pressure: 0.0,
            stall_events: vec![],
            verdict_events: vec![],
            step_recorder_summary: None,
            tool_health_export: vec![],
            last_heavy_checkpoint: None,
            ttft_ms: None,
            context_ms: None,
            memoria_ms: None,
            routing_domain_hint: None,
            entity_learn_skipped_no_domain: false,
            pending_context_assembly_trace: None,
            turn_observability_events: Vec::new(),
            llm_rounds: None,
            interruption: None,
            final_messages: Vec::new(),
            background_agent_results: Vec::new(),
        }
    }

    #[test]
    fn exit_code_success_on_empty_result() {
        let sr = empty_stream_result();
        assert_eq!(compute_exit_code(&sr), ExitCode::Success);
    }

    #[cfg(feature = "harness")]
    #[test]
    fn headless_inspect_request_appends_latest_snapshot() {
        use astra_harness::{DecisionRecord, HookPoint, RuntimeSnapshot, SnapshotSink};

        let sink = astra_harness::InMemorySnapshotSink::arc();
        let mut snapshot = RuntimeSnapshot::empty();
        snapshot.session_id = "s1".to_string();
        snapshot.turn_number = 1;
        snapshot.turns_used = 2;
        snapshot.tool_calls_this_session = 1;
        snapshot.unique_tools_used = vec!["bash".to_string()];
        snapshot.last_tool_called = Some("bash".to_string());
        sink.update(&DecisionRecord {
            session_id: "s1".to_string(),
            turn: 1,
            point: HookPoint::PostToolBatch,
            wall_time_unix_millis: 0,
            monotonic_millis_since_session: 0,
            snapshot,
        });

        let mut sr = empty_stream_result();
        sr.full_text = "done".to_string();
        append_headless_inspect_snapshot(
            &mut sr,
            "Run a command, then use /inspect to show the snapshot.",
            &sink,
        );

        assert!(sr.full_text.contains("done"));
        assert!(sr.full_text.contains("Harness Snapshot"));
        assert!(sr.full_text.contains("Tool calls:"));
        assert!(sr.full_text.contains("Unique tools:        bash"));
    }

    #[cfg(feature = "harness")]
    #[test]
    fn headless_inspect_request_requires_slash_token() {
        assert!(message_requests_headless_inspect("Then use /inspect."));
        assert!(message_requests_headless_inspect("/inspect"));
        assert!(message_requests_headless_inspect(
            "The headless CLI should append the /inspect snapshot."
        ));
        assert!(message_requests_headless_inspect(
            "The harness runner automatically appends the /inspect snapshot."
        ));
        assert!(!message_requests_headless_inspect(
            "Mention inspection, but do not run the command"
        ));
        assert!(!message_requests_headless_inspect("What is `/inspect`?"));
        assert!(!message_requests_headless_inspect(
            "https://example.com/inspect"
        ));
    }

    #[test]
    fn exit_code_tool_failure_on_failed_tool() {
        let mut sr = empty_stream_result();
        sr.tool_call_records
            .push(astra_services::session_journal::ToolCallRecord {
                name: "Bash".to_string(),
                ok: false,
                ms: 100,
                error: Some("exit code 1".to_string()),
                input_bytes: None,
                output_bytes: None,
                args_preview: None,
                result_preview: None,
                file_path: None,
                surgically_removed: None,
                original_tool_name: None,
                ..Default::default()
            });
        assert_eq!(compute_exit_code(&sr), ExitCode::ToolFailure);
    }

    fn tool_call_record(
        name: &str,
        ok: bool,
        error: Option<&str>,
        exit_semantics: Option<&str>,
    ) -> astra_services::session_journal::ToolCallRecord {
        astra_services::session_journal::ToolCallRecord {
            name: name.to_string(),
            ok,
            ms: 100,
            error: error.map(str::to_string),
            exit_semantics: exit_semantics.map(str::to_string),
            ..Default::default()
        }
    }

    #[test]
    fn exit_code_success_on_informational_failure_semantics() {
        let mut sr = empty_stream_result();
        sr.tool_call_records.push(tool_call_record(
            "Bash",
            false,
            Some("grep returned 1"),
            Some("informational_failure"),
        ));
        assert_eq!(compute_exit_code(&sr), ExitCode::Success);
    }

    #[test]
    fn exit_code_success_on_domain_negative_semantics() {
        let mut sr = empty_stream_result();
        sr.tool_call_records.push(tool_call_record(
            "Bash",
            false,
            Some("cargo test returned 1"),
            Some("domain_negative"),
        ));
        assert_eq!(compute_exit_code(&sr), ExitCode::Success);
    }

    #[test]
    fn exit_code_tool_failure_on_execution_error_semantics() {
        let mut sr = empty_stream_result();
        sr.tool_call_records.push(tool_call_record(
            "Bash",
            false,
            Some("command not found"),
            Some("execution_error"),
        ));
        assert_eq!(compute_exit_code(&sr), ExitCode::ToolFailure);
    }

    #[test]
    fn exit_code_unknown_semantics_falls_back_to_legacy_failure() {
        let mut sr = empty_stream_result();
        sr.tool_call_records.push(tool_call_record(
            "Bash",
            false,
            Some("unknown failure"),
            Some("mystery_status"),
        ));
        assert_eq!(compute_exit_code(&sr), ExitCode::ToolFailure);
    }

    #[test]
    fn exit_code_success_when_execution_error_is_followed_by_domain_negative() {
        let mut sr = empty_stream_result();
        sr.tool_call_records.push(tool_call_record(
            "Bash",
            false,
            Some("permission denied"),
            Some("execution_error"),
        ));
        sr.tool_call_records.push(tool_call_record(
            "Bash",
            false,
            Some("git diff reported changes"),
            Some("domain_negative"),
        ));
        assert_eq!(compute_exit_code(&sr), ExitCode::Success);
    }

    #[test]
    fn exit_code_force_stop_overrides_tool_failure() {
        let mut sr = empty_stream_result();
        sr.tool_call_records
            .push(astra_services::session_journal::ToolCallRecord {
                name: "Bash".to_string(),
                ok: false,
                ms: 100,
                error: None,
                input_bytes: None,
                output_bytes: None,
                args_preview: None,
                result_preview: None,
                file_path: None,
                surgically_removed: None,
                original_tool_name: None,
                ..Default::default()
            });
        sr.verdict_events.push(VerdictEvent {
            turn: 1,
            severity: "critical".to_string(),
            injections: vec![],
            avoid_tools: vec![],
            deprioritized_tools: vec![],
            force_stop: true,
            nudge_count: 0,
            interaction_mode: "prompt".to_string(),
            suppressed_loop_nudges: false,
            total_errors: 3,
            deprioritized_count: 0,
            recent_error_pressure: 0,
            recent_timeout_pressure: 0,
            total_timeouts: 0,
            timeout_dominant_tools: vec![],
            total_cache_hits: 0,
            flaky_count: 0,
        });
        assert_eq!(compute_exit_code(&sr), ExitCode::ForceStop);
    }

    #[test]
    fn exit_code_success_when_all_tools_ok() {
        let mut sr = empty_stream_result();
        sr.tool_call_records
            .push(astra_services::session_journal::ToolCallRecord {
                name: "Read".to_string(),
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
            });
        sr.tool_call_records
            .push(astra_services::session_journal::ToolCallRecord {
                name: "Edit".to_string(),
                ok: true,
                ms: 80,
                error: None,
                input_bytes: None,
                output_bytes: None,
                args_preview: None,
                result_preview: None,
                file_path: None,
                surgically_removed: None,
                original_tool_name: None,
                ..Default::default()
            });
        assert_eq!(compute_exit_code(&sr), ExitCode::Success);
    }

    #[test]
    fn exit_code_success_same_tool_retry() {
        let mut sr = empty_stream_result();
        // bash fails first
        sr.tool_call_records
            .push(astra_services::session_journal::ToolCallRecord {
                name: "Bash".to_string(),
                ok: false,
                ms: 50,
                error: Some("exit 1".to_string()),
                ..Default::default()
            });
        // agent retries bash successfully
        sr.tool_call_records
            .push(astra_services::session_journal::ToolCallRecord {
                name: "Bash".to_string(),
                ok: true,
                ms: 80,
                ..Default::default()
            });
        assert_eq!(compute_exit_code(&sr), ExitCode::Success);
    }

    #[test]
    fn exit_code_success_cross_tool_recovery() {
        let mut sr = empty_stream_result();
        // write_file fails (sandbox denied)
        sr.tool_call_records
            .push(astra_services::session_journal::ToolCallRecord {
                name: "write_file".to_string(),
                ok: false,
                ms: 30,
                error: Some("SANDBOX_DENIED".to_string()),
                ..Default::default()
            });
        // agent self-corrects by using bash instead
        sr.tool_call_records
            .push(astra_services::session_journal::ToolCallRecord {
                name: "Bash".to_string(),
                ok: true,
                ms: 100,
                ..Default::default()
            });
        assert_eq!(compute_exit_code(&sr), ExitCode::Success);
    }

    #[test]
    fn exit_code_failure_when_last_call_fails() {
        let mut sr = empty_stream_result();
        sr.tool_call_records
            .push(astra_services::session_journal::ToolCallRecord {
                name: "Bash".to_string(),
                ok: true,
                ms: 50,
                ..Default::default()
            });
        sr.tool_call_records
            .push(astra_services::session_journal::ToolCallRecord {
                name: "Bash".to_string(),
                ok: false,
                ms: 100,
                error: Some("exit 1".to_string()),
                ..Default::default()
            });
        assert_eq!(compute_exit_code(&sr), ExitCode::ToolFailure);
    }

    #[test]
    fn exit_code_success_with_non_force_stop_verdict() {
        let mut sr = empty_stream_result();
        sr.verdict_events.push(VerdictEvent {
            turn: 1,
            severity: "warning".to_string(),
            injections: vec![],
            avoid_tools: vec![],
            deprioritized_tools: vec![],
            force_stop: false,
            nudge_count: 1,
            interaction_mode: "prompt".to_string(),
            suppressed_loop_nudges: false,
            total_errors: 1,
            deprioritized_count: 0,
            recent_error_pressure: 0,
            recent_timeout_pressure: 0,
            total_timeouts: 0,
            timeout_dominant_tools: vec![],
            total_cache_hits: 0,
            flaky_count: 0,
        });
        assert_eq!(compute_exit_code(&sr), ExitCode::Success);
    }
}

#[cfg(test)]
mod final_json_output_tests {
    use super::*;

    fn stream_result_for_json() -> StreamResult {
        StreamResult {
            session_id: Some("session-1".to_string()),
            run_id: Some("run-1".to_string()),
            full_text: "hello".to_string(),
            prompt_tokens: 10,
            completion_tokens: 3,
            cache_read_tokens: 2,
            cache_creation_tokens: 1,
            tool_calls_count: 2,
            tools_selected: vec![],
            selected_skills: vec![],
            tools_used: vec!["bash".to_string(), "read_file".to_string()],
            tool_call_records: vec![],
            budget_used: 0,
            budget_pressure: 0.0,
            stall_events: vec![],
            verdict_events: vec![],
            step_recorder_summary: None,
            tool_health_export: vec![],
            last_heavy_checkpoint: None,
            ttft_ms: None,
            context_ms: None,
            memoria_ms: None,
            routing_domain_hint: None,
            entity_learn_skipped_no_domain: false,
            pending_context_assembly_trace: None,
            turn_observability_events: Vec::new(),
            llm_rounds: None,
            interruption: None,
            final_messages: Vec::new(),
            background_agent_results: Vec::new(),
        }
    }

    #[test]
    fn final_json_output_contains_gateway_contract_fields() {
        let sr = stream_result_for_json();
        let output = final_json_output_with_context(
            &sr,
            ExitCode::Success,
            Some("trace-1".to_string()),
            Some("request-1".to_string()),
        );

        assert_eq!(output["trace_id"], "trace-1");
        assert_eq!(output["request_id"], "request-1");
        assert_eq!(output["run_id"], "run-1");
        assert_eq!(output["session_id"], "session-1");
        assert_eq!(output["text"], "hello");
        assert_eq!(output["prompt_tokens"], 13);
        assert_eq!(output["fresh_prompt_tokens"], 10);
        assert!(output.get("cached_input_tokens").is_none());
        assert!(output.get("cache_creation_tokens").is_none());
        assert_eq!(output["cache"]["hit"], true);
        assert_eq!(output["cache"]["read_tokens"], 2);
        assert_eq!(output["cache"]["creation_tokens"], 1);
        assert_eq!(output["completion_tokens"], 3);
        assert_eq!(output["tool_calls_count"], 2);
        assert_eq!(
            output["tools_used"],
            serde_json::json!(["bash", "read_file"])
        );
        assert_eq!(output["exit_code"], 0);
        assert_eq!(output["success"], true);
        assert!(output["error_kind"].is_null());

        for field in [
            "trace_id",
            "request_id",
            "run_id",
            "session_id",
            "text",
            "prompt_tokens",
            "fresh_prompt_tokens",
            "cache",
            "completion_tokens",
            "tool_calls_count",
            "tools_used",
            "exit_code",
            "success",
            "error_kind",
        ] {
            assert!(output.get(field).is_some(), "missing {field}");
        }
    }

    #[test]
    fn final_json_output_sets_error_kind_on_failure() {
        let sr = stream_result_for_json();
        let output = final_json_output_with_context(
            &sr,
            ExitCode::ToolFailure,
            Some("trace-1".to_string()),
            Some("request-1".to_string()),
        );

        assert_eq!(output["exit_code"], 1);
        assert_eq!(output["success"], false);
        assert_eq!(output["error_kind"], "tool_failure");
    }
}

#[cfg(test)]
mod show_policy_tests {
    use crate::cli::config_manager::format_policy_output;
    use astra_config::runtime_config::EffectiveToolPolicy;

    fn fake_policy() -> EffectiveToolPolicy {
        EffectiveToolPolicy {
            max_identical_tool_calls: 4,
            max_tools_per_turn: 20,
            repeated_cache_hit_suppression: 4,
            max_consecutive_empty_name: 3,
            parallel_batching_force_streak: 5,
        }
    }

    #[test]
    fn human_output_includes_all_guard_fields_and_model_label() {
        let out = format_policy_output(Some("opus"), &fake_policy(), "strict", &[], false);
        assert!(out.contains("opus"), "model label missing: {out}");
        assert!(
            out.contains("max_identical_tool_calls"),
            "field name missing: {out}"
        );
        assert!(out.contains("= 4"), "opus's value 4 missing: {out}");
        assert!(out.contains("max_tools_per_turn"), "field missing: {out}");
        assert!(out.contains("= 20"), "opus's value 20 missing: {out}");
        assert!(
            out.contains("repeated_cache_hit_suppression"),
            "field missing: {out}"
        );
        assert!(
            out.contains("max_consecutive_empty_name"),
            "field missing: {out}"
        );
        assert!(
            out.contains("parallel_batching_force_streak"),
            "field missing: {out}"
        );
        assert!(
            out.contains("trust_mode") && out.contains("strict"),
            "trust_mode row missing: {out}"
        );
    }

    #[test]
    fn human_output_shows_trusted_mode_when_configured() {
        let out = format_policy_output(Some("opus"), &fake_policy(), "trusted", &[], false);
        assert!(
            out.contains("trust_mode") && out.contains("trusted"),
            "expected trust_mode=trusted line: {out}"
        );
    }

    #[test]
    fn human_output_without_model_shows_global_defaults_label() {
        let out = format_policy_output(None, &fake_policy(), "strict", &[], false);
        assert!(
            out.contains("global defaults"),
            "no-model label missing: {out}"
        );
    }

    #[test]
    fn json_output_is_parseable_and_contains_expected_keys() {
        let out = format_policy_output(Some("haiku"), &fake_policy(), "strict", &[], true);
        let parsed: serde_json::Value = serde_json::from_str(&out).expect("json output must parse");
        assert_eq!(parsed["model"], "haiku");
        assert_eq!(parsed["trust_mode"], "strict");
        assert_eq!(parsed["max_identical_tool_calls"], 4);
        assert_eq!(parsed["max_tools_per_turn"], 20);
        assert_eq!(parsed["repeated_cache_hit_suppression"], 4);
        assert_eq!(parsed["max_consecutive_empty_name"], 3);
    }

    #[test]
    fn json_output_with_none_model_yields_json_null() {
        let out = format_policy_output(None, &fake_policy(), "strict", &[], true);
        let parsed: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert!(parsed["model"].is_null());
    }

    #[test]
    fn config_show_policy_end_to_end_opus_hits_builtin_profile() {
        // End-to-end: load config, resolve, format. Asserts the whole
        // wiring works — not just the string formatter. Opus's built-in
        // profile is 4 / 20 / 4 / 3 (see
        // `ToolSelectionConfig::builtin_model_profiles`).
        let cfg = astra_config::runtime_config::RuntimeConfig::load();
        let policy = cfg.tool_selection.resolve_for_model(Some("opus"));
        let human = format_policy_output(Some("opus"), &policy, "strict", &[], false);
        assert!(human.contains("= 4"), "expected 4s for opus: {human}");
        assert!(human.contains("= 20"), "expected 20 for opus: {human}");

        let json = format_policy_output(Some("opus"), &policy, "strict", &[], true);
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["max_identical_tool_calls"], 4);
        assert_eq!(parsed["max_tools_per_turn"], 20);
    }

    #[test]
    fn human_output_surfaces_rejected_short_patterns() {
        // When the config has model_profiles with patterns shorter than
        // MIN_MODEL_MATCH_LEN, they're silently ignored at resolve time
        // but `show-policy` must call it out so the user can spot the
        // misconfig. Pattern is surfaced verbatim (quoted).
        let out = format_policy_output(
            Some("opus"),
            &fake_policy(),
            "strict",
            &["4".to_string(), "op".to_string()],
            false,
        );
        assert!(
            out.contains("rejected"),
            "expected 'rejected' warning in output: {out}"
        );
        assert!(out.contains("\"4\""), "pattern not quoted: {out}");
        assert!(out.contains("\"op\""), "pattern not quoted: {out}");
    }

    #[test]
    fn human_output_has_no_warning_block_when_no_rejections() {
        // Don't add a warning section when everything is clean — the output
        // should be identical to the pre-feature version.
        let out = format_policy_output(Some("opus"), &fake_policy(), "strict", &[], false);
        assert!(
            !out.to_lowercase().contains("rejected"),
            "output should not contain 'rejected' when no short patterns: {out}"
        );
    }

    #[test]
    fn json_output_includes_rejected_patterns_array() {
        let out = format_policy_output(
            Some("opus"),
            &fake_policy(),
            "strict",
            &["4".to_string()],
            true,
        );
        let parsed: serde_json::Value = serde_json::from_str(&out).unwrap();
        let rejected = parsed["rejected_model_match_patterns"]
            .as_array()
            .expect("rejected_model_match_patterns must be an array");
        assert_eq!(rejected.len(), 1);
        assert_eq!(rejected[0], "4");
    }

    #[test]
    fn json_output_rejected_patterns_empty_array_when_clean() {
        // Always present as an array — never missing / null — so json
        // consumers don't have to special-case the absent-vs-empty case.
        let out = format_policy_output(Some("opus"), &fake_policy(), "strict", &[], true);
        let parsed: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(
            parsed["rejected_model_match_patterns"]
                .as_array()
                .expect("must be array")
                .len(),
            0
        );
    }
}

#[cfg(test)]
mod default_model_tests {
    #[test]
    fn read_config_default_model_from_file() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("settings.json");
        let settings = serde_json::json!({
            "default_model": "gpt-4o",
            "verbose": true
        });
        std::fs::write(&path, serde_json::to_string_pretty(&settings).unwrap()).unwrap();

        // read_config_default_model uses the real settings_path, so we test the
        // extraction logic directly
        let content = std::fs::read_to_string(&path).unwrap();
        let val: serde_json::Value = serde_json::from_str(&content).unwrap();
        let model = val
            .get("default_model")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        assert_eq!(model, Some("gpt-4o".to_string()));
    }

    #[test]
    fn read_config_default_model_missing_key() {
        let settings = serde_json::json!({ "verbose": true });
        let model = settings
            .get("default_model")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        assert_eq!(model, None);
    }

    #[test]
    fn read_config_default_model_non_string_value() {
        let settings = serde_json::json!({ "default_model": 42 });
        let model = settings
            .get("default_model")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        assert_eq!(model, None); // non-string returns None
    }
}

#[cfg(test)]
mod api_url_config_tests {
    use super::*;
    use crate::cli::config_manager::{
        DEFAULT_API_URL, KNOWN_SETTINGS, read_config_api_url_from, resolve_api_url_with,
    };

    fn no_env() -> Option<String> {
        None
    }
    fn no_config() -> Result<Option<String>, String> {
        Ok(None)
    }
    fn env_val(url: &str) -> impl FnOnce() -> Option<String> {
        let s = url.to_string();
        move || Some(s)
    }
    fn config_val(url: &str) -> impl FnOnce() -> Result<Option<String>, String> {
        let s = url.to_string();
        move || Ok(Some(s))
    }

    #[test]
    fn flag_wins_over_env_and_config() {
        let url = resolve_api_url_with(
            Some("http://flag:8000"),
            env_val("http://env:8000"),
            config_val("http://config:8000"),
        )
        .expect("flag should win");
        assert_eq!(url, "http://flag:8000");
    }

    #[test]
    fn env_wins_over_config() {
        let url = resolve_api_url_with(
            None,
            env_val("http://env:8000"),
            config_val("http://config:8000"),
        )
        .expect("env should win");
        assert_eq!(url, "http://env:8000");
    }

    #[test]
    fn config_wins_over_default() {
        let url = resolve_api_url_with(None, no_env, config_val("http://config:8000"))
            .expect("config should win");
        assert_eq!(url, "http://config:8000");
    }

    #[test]
    fn falls_back_to_default_when_all_none() {
        let url = resolve_api_url_with(None, no_env, no_config).expect("default should apply");
        assert_eq!(url, DEFAULT_API_URL);
    }

    #[test]
    fn trailing_slash_stripped_from_flag() {
        let url = resolve_api_url_with(Some("http://flag:8000/"), no_env, no_config)
            .expect("flag should trim slash");
        assert_eq!(url, "http://flag:8000");
    }

    #[test]
    fn trailing_slash_stripped_from_env() {
        let url = resolve_api_url_with(None, env_val("http://env:8000/"), no_config)
            .expect("env should trim slash");
        assert_eq!(url, "http://env:8000");
    }

    #[test]
    fn trailing_slash_stripped_from_config() {
        let url = resolve_api_url_with(None, no_env, config_val("http://config:8000/"))
            .expect("config should trim slash");
        assert_eq!(url, "http://config:8000");
    }

    #[test]
    fn config_error_is_propagated() {
        let err = resolve_api_url_with(None, no_env, || Err("broken".to_string()))
            .expect_err("config error should not fall through");
        assert_eq!(err, "broken");
    }

    #[test]
    fn api_url_is_known_setting() {
        assert!(
            KNOWN_SETTINGS.iter().any(|(k, _)| *k == "api_url"),
            "api_url must be in KNOWN_SETTINGS"
        );
    }

    /// Integration test: `read_config_api_url` actually reads `settings.json` from disk.
    #[test]
    fn read_config_api_url_reads_real_file() {
        let tmp = tempfile::tempdir().unwrap();
        let astra_dir = tmp.path().join(".astra");
        std::fs::create_dir_all(&astra_dir).unwrap();
        let settings = astra_dir.join("settings.json");
        std::fs::write(
            &settings,
            r#"{"api_url":"http://from-disk:9999","default_model":"gpt-4"}"#,
        )
        .unwrap();

        let result = read_config_api_url_from(Some(&settings));
        assert_eq!(
            result.unwrap().as_deref(),
            Some("http://from-disk:9999"),
            "read_config_api_url should read from disk"
        );
    }

    #[test]
    fn read_config_api_url_returns_none_when_key_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let astra_dir = tmp.path().join(".astra");
        std::fs::create_dir_all(&astra_dir).unwrap();
        std::fs::write(astra_dir.join("settings.json"), r#"{}"#).unwrap();

        let settings = astra_dir.join("settings.json");
        let result = read_config_api_url_from(Some(&settings));
        assert_eq!(result.unwrap(), None);
    }

    #[test]
    fn latest_artifact_id_reads_response_shape() {
        let artifact_id =
            latest_artifact_id(r#"{"artifact_id":"art-123","artifact_kind":"llm_capture"}"#)
                .unwrap();
        assert_eq!(artifact_id, "art-123");
    }

    #[test]
    fn resolve_download_output_path_appends_to_directory() {
        let dir = tempfile::tempdir().unwrap();
        let resolved = resolve_download_output_path(Some(dir.path()), "capture.json");
        assert_eq!(resolved, dir.path().join("capture.json"));
    }

    #[test]
    fn resolve_download_output_path_strips_path_traversal() {
        let resolved = resolve_download_output_path(None, "../../.bashrc");
        assert_eq!(
            resolved,
            std::path::PathBuf::from(".bashrc"),
            "path traversal components must be stripped from server-suggested filename"
        );
    }

    #[test]
    fn resolve_download_output_path_strips_absolute_path() {
        let dir = tempfile::tempdir().unwrap();
        let resolved = resolve_download_output_path(Some(dir.path()), "/etc/cron.d/backdoor");
        assert_eq!(
            resolved,
            dir.path().join("backdoor"),
            "absolute path components must be stripped"
        );
    }

    #[test]
    fn write_downloaded_capture_creates_parent_dirs() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("nested").join("capture.json");
        write_downloaded_capture(&target, br#"{"ok":true}"#).unwrap();
        assert_eq!(std::fs::read_to_string(&target).unwrap(), r#"{"ok":true}"#);
    }
}
