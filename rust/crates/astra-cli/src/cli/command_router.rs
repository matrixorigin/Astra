use crate::cli::arg_render::{
    apply_system_prompt, join_words, render_agent_args, render_bug_args, render_debug_args,
    render_diff_args, render_grep_args, render_memory_args, render_messaging_args,
    render_permissions_args, render_review_args, render_task_args, render_team_args,
};
use crate::cli::auth_flow::{clear_profile_auth, do_login, do_register, is_auth_error};
use crate::cli::cli_config::cli_args::{
    AuditCmd, Cli, Command, JournalCmd, ModelCmd, SessionCaptureCmd, SessionCmd, SkillCmd,
    TaskRunArgs, TaskSubcommand, TaskWorkerArgs,
};
use crate::cli::cli_config::cli_utils;
use crate::cli::cli_config::cli_utils::{
    clear_profile_last_session_if_matches_or_warn, cli_user_id, get_profile_and_token,
    load_credentials, map_thin_err, mutate_credentials, persist_profile_last_session, prefix_chars,
    print_json_or_raw, profile_name, prompt_or, prompt_password_masked, validate_cli_session_id,
};
use crate::cli::config_manager::{
    execute_config_command, latest_artifact_id, resolve_download_output_path,
    resolve_remote_session_id, write_downloaded_capture,
};
use crate::cli::exit_code::ExitCode;
use crate::cli::interactive_chat::run_interactive_chat;
use crate::cli::mcp_config::execute_mcp_command;
use crate::cli::one_shot_session_routing::{
    OneShotSessionRouting, resolve_one_shot_session_routing,
};
use crate::cli::permission_command::handle_permission_command;
use crate::cli::permission_manager::{PermissionManager, PermissionMode};
use crate::cli::project_instructions::discover_project_instructions;
use crate::cli::session::session_runtime;
use crate::cli::session::session_runtime::{
    create_pipeline_modules, create_pipeline_modules_quiet, initialize_session_state,
    try_silent_auth,
};
use crate::cli::session::session_side_effects;
use crate::cli::session::session_state::{ExplainMode, SessionState};
use crate::cli::skill_catalog::{
    SkillCatalogFilter, list_skill_record_from_registry, load_skill_record_from_registry,
    normalize_source_filter,
};
use crate::cli::slash::slash_bug::handle_bug_command;
use crate::cli::slash::slash_debug::handle_debug_command;
use crate::cli::slash::slash_info::handle_info_command;
#[cfg(feature = "harness")]
use crate::cli::slash::slash_inspect;
use crate::cli::slash::slash_memory::handle_memory_domain_command;
use crate::cli::slash::slash_messaging::handle_messaging_command;
use crate::cli::slash::{slash_agent, slash_task, slash_team, slash_telemetry};
use crate::cli::stream::streaming_types::StreamResult;
use crate::cli::surface::task_checkpoint_surface::encode_task_failure_message;
use crate::cli::task::task_command_utils::task_run_title;
use crate::cli::task::task_result_artifact::write_task_output;
use crate::cli::task::task_result_projection::stream_result_exit_code;
use crate::cli::task::task_worker_support::{
    ClaimedTaskLeaseGuard, WorkerClaim, claim_task_for_worker, default_task_agent_id,
    get_claimed_task_or_release, revert_interrupted_task_to_pending_if_still_owned,
};
use crate::cli::{
    agent_loader, delegate_subrun, diff_presenter, journal_diff, journal_digest, journal_tree,
    theme,
};
use astra_thin_client::paths;
use clap::CommandFactory;
use crossterm::{style::Stylize, terminal};
use std::io::Read;

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

async fn fresh_access_token_or_error(
    api: &astra_thin_client::ThinClient,
    profile: Option<&str>,
) -> Result<String, String> {
    session_runtime::fresh_access_token(api, profile)
        .await
        .ok_or_else(|| {
            "Unable to obtain a valid access token; run `astra login` and retry.".to_string()
        })
}

fn repl_bridge_command_requires_access_token(slash_cmd: &str) -> bool {
    matches!(
        slash_cmd,
        "/team" | "/task" | "/memory" | "/plan" | "/review" | "/grep"
    )
}

async fn repl_bridge_access_token(
    slash_cmd: &str,
    api: &astra_thin_client::ThinClient,
    profile: Option<&str>,
) -> Result<Option<String>, String> {
    if repl_bridge_command_requires_access_token(slash_cmd) {
        return fresh_access_token_or_error(api, profile).await.map(Some);
    }
    Ok(session_runtime::fresh_access_token(api, profile).await)
}

impl From<ExitCode> for i32 {
    fn from(code: ExitCode) -> i32 {
        code as i32
    }
}

fn maybe_load_project_instructions(state: &mut SessionState) {
    state.project_instructions = discover_project_instructions();
}

fn validated_cli_session_arg(session_id: &str) -> Result<&str, String> {
    validate_cli_session_id(session_id)?;
    Ok(session_id)
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
        state.perm_manager.inherited_permissions_for_child(true),
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

fn emit_task_event(enabled: bool, value: serde_json::Value) {
    if enabled {
        if let Ok(line) = serde_json::to_string(&value) {
            eprintln!("{line}");
        }
    }
}

pub(crate) fn exit_code_for_error_kind(error_kind: &str) -> Option<ExitCode> {
    match error_kind {
        "tool_failure" => Some(ExitCode::ToolFailure),
        "force_stop" => Some(ExitCode::ForceStop),
        "api_error" => Some(ExitCode::ApiError),
        "persistence_error" => Some(ExitCode::PersistenceError),
        "partial" => Some(ExitCode::Partial),
        "unfinished" => Some(ExitCode::Unfinished),
        _ => None,
    }
}

pub(crate) fn error_kind_for_exit_code(exit_code: ExitCode) -> Option<&'static str> {
    match exit_code {
        ExitCode::Success => None,
        ExitCode::ToolFailure => Some("tool_failure"),
        ExitCode::ForceStop => Some("force_stop"),
        ExitCode::ApiError => Some("api_error"),
        ExitCode::PersistenceError => Some("persistence_error"),
        ExitCode::Partial => Some("partial"),
        ExitCode::Unfinished => Some("unfinished"),
    }
}

fn task_status_for_exit_code(exit_code: ExitCode) -> &'static str {
    match exit_code {
        ExitCode::Success => "completed",
        ExitCode::Partial => "partial",
        ExitCode::Unfinished => "unfinished",
        ExitCode::PersistenceError => "persistence_error",
        ExitCode::ToolFailure | ExitCode::ForceStop | ExitCode::ApiError => "failed",
    }
}

fn task_notification_payload(
    task_id: &str,
    sr: &StreamResult,
    output_path: Option<&str>,
    exit_code: ExitCode,
) -> serde_json::Value {
    let mut payload = serde_json::Map::new();
    payload.insert(
        "type".to_string(),
        serde_json::json!("background_task_notification"),
    );
    payload.insert("task_id".to_string(), serde_json::json!(task_id));
    payload.insert(
        "status".to_string(),
        serde_json::json!(task_status_for_exit_code(exit_code)),
    );
    payload.insert(
        "success".to_string(),
        serde_json::json!(exit_code == ExitCode::Success),
    );
    payload.insert(
        "exit_code".to_string(),
        serde_json::json!(i32::from(exit_code)),
    );
    if let Some(output_path) = output_path {
        payload.insert("output_file".to_string(), serde_json::json!(output_path));
    }
    payload.insert(
        "summary".to_string(),
        serde_json::json!(sr.full_text.chars().take(200).collect::<String>()),
    );
    payload.insert("final_state".to_string(), serde_json::json!(sr.final_state));
    payload.insert(
        "interruption_kind".to_string(),
        serde_json::json!(sr.interruption_kind),
    );
    if let Some(error_kind) = error_kind_for_exit_code(exit_code) {
        payload.insert("error_kind".to_string(), serde_json::json!(error_kind));
    }
    if let Some(error) = sr.session_persistence_error.as_deref() {
        payload.insert("persistence_error".to_string(), serde_json::json!(error));
    }
    serde_json::Value::Object(payload)
}

fn failed_task_notification_payload(
    task_id: &str,
    summary: &str,
    error_kind: &str,
    output_path: Option<&str>,
    persistence_error: Option<&str>,
) -> serde_json::Value {
    let mut payload = serde_json::Map::new();
    payload.insert(
        "type".to_string(),
        serde_json::json!("background_task_notification"),
    );
    payload.insert("task_id".to_string(), serde_json::json!(task_id));
    let status = if error_kind == "persistence_error" {
        "persistence_error"
    } else {
        "failed"
    };
    payload.insert("status".to_string(), serde_json::json!(status));
    payload.insert("success".to_string(), serde_json::json!(false));
    payload.insert("summary".to_string(), serde_json::json!(summary));
    payload.insert("error_kind".to_string(), serde_json::json!(error_kind));
    if let Some(path) = output_path {
        payload.insert("output_file".to_string(), serde_json::json!(path));
    }
    if let Some(error) = persistence_error {
        payload.insert("persistence_error".to_string(), serde_json::json!(error));
    }
    serde_json::Value::Object(payload)
}

fn task_terminal_summary_line(
    task_id: &str,
    output_path: Option<&str>,
    exit_code: ExitCode,
) -> String {
    let (icon, outcome) = match exit_code {
        ExitCode::Success => (theme::icon_ok(), "finished"),
        ExitCode::Partial => (theme::icon_warn(), "finished partially"),
        ExitCode::Unfinished => (theme::icon_warn(), "unfinished"),
        ExitCode::PersistenceError => (theme::icon_warn(), "finished with persistence degradation"),
        ExitCode::ForceStop => (theme::icon_warn(), "stopped"),
        ExitCode::ToolFailure | ExitCode::ApiError => (theme::icon_err(), "failed"),
    };
    match output_path {
        Some(output_path) => format!(
            "\n  {} Task {} {}; output saved to {}",
            icon,
            prefix_chars(task_id, 8).cyan(),
            outcome,
            output_path.dim(),
        ),
        None => format!(
            "\n  {} Task {} {}; output file unavailable",
            icon,
            prefix_chars(task_id, 8).cyan(),
            outcome,
        ),
    }
}

fn record_stream_persistence_error(sr: &mut StreamResult, detail: impl Into<String>) {
    let detail = detail.into();
    match sr.session_persistence_error.as_deref() {
        Some(existing) if existing == detail => {}
        Some(existing) => {
            sr.session_persistence_error = Some(format!("{existing}; {detail}"));
        }
        None => sr.session_persistence_error = Some(detail),
    }
}

fn persist_one_shot_session_state(
    profile: Option<&str>,
    model: Option<&str>,
    line: &str,
    sr: &mut StreamResult,
    turn_start: std::time::Instant,
) {
    if let Err(error) = session_side_effects::append_one_shot_journal_events(
        sr.session_id.as_deref(),
        model,
        line,
        sr,
        turn_start,
    ) {
        record_stream_persistence_error(sr, error);
    }

    if sr.session_persistence_error.is_none()
        && let Some(sid) = sr.session_id.as_deref()
        && let Err(error) = persist_profile_last_session(profile, sid)
    {
        record_stream_persistence_error(
            sr,
            format!("failed to persist last session pointer: {error}"),
        );
    }
}

fn finalize_one_shot_stream_result(
    profile: Option<&str>,
    model: Option<&str>,
    line: &str,
    sr: &mut StreamResult,
    turn_start: std::time::Instant,
) -> ExitCode {
    persist_one_shot_session_state(profile, model, line, sr, turn_start);
    compute_exit_code(sr)
}

fn effective_one_shot_model<'a>(
    explicit_model: Option<&'a str>,
    restored_model: Option<&'a str>,
    fallback_model: Option<&'a str>,
) -> Option<&'a str> {
    explicit_model
        .filter(|model| !model.trim().is_empty())
        .or_else(|| restored_model.filter(|model| !model.trim().is_empty()))
        .or_else(|| fallback_model.filter(|model| !model.trim().is_empty()))
}

async fn resolve_one_shot_model(
    api: &astra_thin_client::ThinClient,
    token: &str,
    explicit_model: Option<&str>,
    restored_model: Option<&str>,
    fallback_model: Option<&str>,
) -> Option<String> {
    if let Some(model) = effective_one_shot_model(explicit_model, restored_model, fallback_model) {
        return Some(model.to_string());
    }
    match session_runtime::resolve_server_default_model(api, token).await {
        session_runtime::ServerDefaultModel::Selected(model) => Some(model),
        session_runtime::ServerDefaultModel::NoModels
        | session_runtime::ServerDefaultModel::Unavailable => None,
    }
}

fn effective_one_shot_permission_mode(
    explicit_mode: Option<&str>,
    explicit_auto: bool,
    restored_mode: Option<&str>,
    fallback_auto: bool,
) -> Result<PermissionMode, String> {
    if let Some(mode) = explicit_mode.filter(|mode| !mode.trim().is_empty()) {
        return mode
            .parse::<PermissionMode>()
            .map_err(|error| format!("invalid permission mode '{mode}': {error}"));
    }
    if explicit_auto {
        return Ok(PermissionMode::Auto);
    }
    if let Some(mode) = restored_mode.filter(|mode| !mode.trim().is_empty()) {
        return mode.parse::<PermissionMode>().map_err(|error| {
            format!("invalid restored session permission mode '{mode}': {error}")
        });
    }
    Ok(if fallback_auto {
        PermissionMode::Auto
    } else {
        PermissionMode::Prompt
    })
}

fn one_shot_completion_warning(sr: &StreamResult, exit_code: ExitCode) -> Option<String> {
    if let Some(error) = sr.session_persistence_error.as_deref() {
        Some(format!("Session persistence degraded: {error}"))
    } else if exit_code == ExitCode::Partial {
        Some(match sr.interruption_kind.as_deref() {
            Some(kind) => format!(
                "Turn finished partially ({kind}). Inspect partial output before continuing."
            ),
            None => {
                "Turn finished partially. Inspect partial output before continuing.".to_string()
            }
        })
    } else {
        None
    }
}

fn print_one_shot_completion_warning(sr: &StreamResult, exit_code: ExitCode, json_output: bool) {
    if let Some(message) = one_shot_completion_warning(sr, exit_code)
        && !json_output
    {
        eprintln!("  {}", message.yellow());
    }
}

struct HeadlessTaskInput {
    task_id: std::sync::Arc<String>,
    task_session_id: std::sync::Arc<String>,
    prompt: String,
    svc: std::sync::Arc<dyn astra_services::TaskService>,
    session_routing: OneShotSessionRouting,
}

#[derive(Debug, Clone, Copy)]
struct HeadlessTaskOptions {
    json: bool,
    quiet: bool,
    stream_events: bool,
    print_started: bool,
}

const NON_CANONICAL_TASK_SCOPE: &str = "no-session";

async fn build_one_shot_task_manager(
    profile: Option<&str>,
    api_origin: &str,
    session_id: Option<&str>,
) -> std::sync::Arc<crate::edge_tools::TaskManager> {
    if let Some(session_id) = session_id {
        let task_store =
            crate::cli::session::session_runtime::resolve_task_store(profile, Some(api_origin))
                .await
                .0;
        std::sync::Arc::new(crate::edge_tools::TaskManager::new(
            session_id.to_string(),
            task_store,
        ))
    } else {
        std::sync::Arc::new(crate::edge_tools::TaskManager::new(
            NON_CANONICAL_TASK_SCOPE.to_string(),
            std::sync::Arc::new(astra_tools::task_mgmt::InMemoryTaskStore::new().with_validation()),
        ))
    }
}

async fn execute_headless_task_body(
    input: HeadlessTaskInput,
    options: HeadlessTaskOptions,
    profile: Option<&str>,
    global_model: Option<&str>,
    api: &astra_thin_client::ThinClient,
    cli_context: &crate::cli::cli_config::cli_context::CliContext,
) -> Result<ExitCode, String> {
    let HeadlessTaskInput {
        task_id,
        task_session_id,
        prompt,
        svc,
        session_routing,
    } = input;
    use astra_services::TaskStatus;
    let (_creds, profile_name, _, _token) = get_profile_and_token(profile)?;
    let token = fresh_access_token_or_error(api, profile).await?;
    let session_id = session_routing.server_session_id.clone();
    let effective_model = resolve_one_shot_model(
        api,
        &token,
        None,
        session_routing.restored_model(),
        global_model,
    )
    .await;
    let effective_permission_mode = effective_one_shot_permission_mode(
        None,
        false,
        session_routing.restored_permission_mode(),
        true,
    )?;
    let mut continuation_messages = session_routing.continuation_messages();

    emit_task_event(
        options.stream_events,
        serde_json::json!({
            "type": "background_task_started",
            "task_id": task_id.as_str(),
            "task_kind": "local_agent",
            "description": prompt,
        }),
    );

    if options.print_started && !options.quiet && !options.json {
        eprintln!(
            "  {} Task started: {} ({})",
            "▶".cyan(),
            prompt.chars().take(50).collect::<String>(),
            prefix_chars(task_id.as_str(), 8).dim()
        );
    }

    svc.update_status(task_id.as_str(), TaskStatus::InProgress)
        .await?;

    let pipeline_modules = session_runtime::create_pipeline_modules_quiet(api, profile);
    let project_root = std::env::current_dir().unwrap_or_default();
    let mut pm = PermissionManager::with_load_policy(
        effective_permission_mode,
        &project_root,
        &crate::cli::permission_manager::PermissionLoadPolicy::HeadlessSafe,
    );
    let mut skill_qt = astra_skills::quality::SkillQualityTracker::new();
    let root_agent_id = format!("task-{}", task_id.as_str());
    let spawner = super::agent_runtime::build_one_shot_spawner(
        api,
        token.clone(),
        pipeline_modules.unified_skill_registry.clone(),
        session_id.clone(),
        effective_model.clone(),
    )
    .await;
    let spawner_handle_for_drain = spawner.clone();

    let (stream_event_tx, stream_event_writer) = if options.stream_events {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let handle = crate::cli::stream::stream_events_writer::spawn_stderr_writer(rx);
        (Some(tx), Some(handle))
    } else {
        (None, None)
    };

    let render_policy = if options.quiet || options.json {
        crate::cli::stream::stream_render::RenderPolicy::Silent
    } else {
        crate::cli::stream::stream_render::RenderPolicy::Stream
    };
    // Headless single-shot path: use the MO-backed task store when available
    // so session_todos is authoritative here the same way it is in the REPL.
    let task_manager = build_one_shot_task_manager(
        profile,
        &api.api_origin(),
        session_routing.task_scope_session_id(),
    )
    .await;

    let chat_ctx = crate::cli::chat_stream::BasicCliChatContext {
        api,
        auth_profile: profile,
        message: &prompt,
        model: effective_model.as_deref(),
        provider: None,
        explain: ExplainMode::Off,
        render_md: terminal::size().is_ok() && !options.quiet && !options.json,
        verbose_mode: !options.quiet && !options.json,
        render_policy,
        cli_context: Some(cli_context),
        unified_skill_registry: &pipeline_modules.unified_skill_registry,
        agent_spawner: Some(spawner),
        root_agent_id: Some(&root_agent_id),
        task_manager: Some(task_manager),
        task_notify_tx: None,
        bg_task_commands: None,
        bg_task_list_cache: None,
        bash_detach_slot: None,
        stream_event_tx,
        #[cfg(feature = "harness")]
        harness_sink: Some(astra_harness::InMemorySnapshotSink::arc()),
        #[cfg(feature = "harness")]
        harness_trace: Some(std::sync::Arc::new(std::sync::RwLock::new(
            astra_harness::SessionTrace::new(None),
        ))),
        #[cfg(feature = "harness")]
        benchmark_profile: None,
    };

    let turn_start = std::time::Instant::now();
    let turn_options = crate::cli::turn::turn_facade::BasicCliTurnOptions {
        pre_loaded_messages: continuation_messages.take(),
        ..Default::default()
    };
    let mut sr = match crate::cli::turn::execute_basic_cli_turn(
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
            let _ = svc.fail_task(task_id.as_str(), &e.error).await;
            emit_task_event(
                options.stream_events,
                failed_task_notification_payload(
                    task_id.as_str(),
                    &e.error,
                    "turn_error",
                    None,
                    None,
                ),
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

    persist_one_shot_session_state(
        Some(&profile_name),
        effective_model.as_deref(),
        &prompt,
        &mut sr,
        turn_start,
    );

    let output_path_result = write_task_output(task_id.as_str(), &sr.full_text);
    let output_path_string = match output_path_result.as_ref() {
        Ok(path) => Some(path.to_string_lossy().to_string()),
        Err(error) => {
            record_stream_persistence_error(&mut sr, error.clone());
            None
        }
    };
    let exit_code = match crate::cli::task::task_result_command::finalize_headless_task_result(
        svc.as_ref(),
        task_id.as_str(),
        &sr,
        Some(task_session_id.as_str()),
        output_path_string.as_deref(),
    )
    .await
    {
        Ok(code) => code,
        Err(e) => {
            let _ = svc
                .fail_task(
                    task_id.as_str(),
                    &encode_task_failure_message("persistence_error", &e),
                )
                .await;
            emit_task_event(
                options.stream_events,
                failed_task_notification_payload(
                    task_id.as_str(),
                    &e,
                    "task_record_error",
                    output_path_string.as_deref(),
                    sr.session_persistence_error.as_deref(),
                ),
            );
            return Err(e);
        }
    };

    emit_task_event(
        options.stream_events,
        task_notification_payload(
            task_id.as_str(),
            &sr,
            output_path_string.as_deref(),
            exit_code,
        ),
    );

    if options.json {
        let mut json_output = final_json_output(&sr, exit_code);
        if let Some(obj) = json_output.as_object_mut() {
            obj.insert("task_id".to_string(), serde_json::json!(task_id.as_str()));
            obj.insert(
                "task_status".to_string(),
                serde_json::json!(task_status_for_exit_code(exit_code)),
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
            "{}",
            task_terminal_summary_line(task_id.as_str(), output_path_string.as_deref(), exit_code)
        );
    }

    print_one_shot_completion_warning(&sr, exit_code, options.json);

    Ok(exit_code)
}

async fn execute_headless_task_run(
    args: TaskRunArgs,
    profile: Option<&str>,
    global_model: Option<&str>,
    api: &astra_thin_client::ThinClient,
    cli_context: &crate::cli::cli_config::cli_context::CliContext,
) -> Result<ExitCode, String> {
    use astra_services::TaskCreateRequest;

    let prompt = join_words(&args.text);
    if prompt.trim().is_empty() {
        return Err("task prompt cannot be empty".to_string());
    }

    let session_routing =
        resolve_one_shot_session_routing(api, profile, cli_context.session_id.clone(), true)
            .await?;
    let user_id = cli_user_id();
    let task_session_id = session_routing
        .task_scope_session_id()
        .unwrap_or(NON_CANONICAL_TASK_SCOPE)
        .to_string();
    let svc = session_runtime::resolve_task_service(profile).await;
    let task_id = svc
        .create_task(
            &user_id,
            &task_session_id,
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
            task_id: std::sync::Arc::new(task_id),
            task_session_id: std::sync::Arc::new(task_session_id),
            prompt,
            svc,
            session_routing,
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
    cli_context: &crate::cli::cli_config::cli_context::CliContext,
) -> Result<WorkerOutcome, String> {
    let (svc, lease_svc) = session_runtime::resolve_cloud_task_runtime(profile).await?;
    let user_id = cli_user_id();
    let agent_id = args.agent_id.clone().unwrap_or_else(default_task_agent_id);
    let edge_id = std::env::var("ASTRA_EDGE_ID").unwrap_or_else(|_| agent_id.clone());
    let claimed_task_id =
        match claim_task_for_worker(&*lease_svc, &user_id, &agent_id, &edge_id, args.ttl_seconds)
            .await?
        {
            WorkerClaim::Granted(grant) => {
                if args.json {
                    eprintln!(
                        "{}",
                        serde_json::json!({
                            "claimed": true,
                            "task_id": grant.task_id,
                            "agent_id": agent_id,
                            "lease_version": grant.lease_version,
                            "expires_at": grant.expires_at,
                        })
                    );
                } else if !args.quiet {
                    eprintln!(
                        "  {} Claimed cloud task {} as {}",
                        "▶".cyan(),
                        prefix_chars(&grant.task_id, 8).dim(),
                        agent_id.as_str().cyan()
                    );
                }
                grant.task_id
            }
            WorkerClaim::Idle(reason) => {
                if args.json {
                    println!(
                        "{}",
                        serde_json::json!({"claimed": false, "reason": reason.json_reason()})
                    );
                } else if !args.quiet {
                    eprintln!("  {}", reason.human_message().dim());
                }
                return Ok(WorkerOutcome::Completed(ExitCode::Success));
            }
        };

    let task =
        get_claimed_task_or_release(&*svc, &*lease_svc, &user_id, &claimed_task_id, &agent_id)
            .await?;
    let astra_services::TaskRecord {
        task_id,
        session_id,
        title,
        description,
        ..
    } = task;
    let prompt = description
        .filter(|s| !s.trim().is_empty())
        .unwrap_or(title);
    let task_session_id = std::sync::Arc::new(
        session_id
            .clone()
            .unwrap_or_else(|| NON_CANONICAL_TASK_SCOPE.to_string()),
    );
    let user_id = std::sync::Arc::new(user_id);
    let task_id = std::sync::Arc::new(task_id);
    let agent_id = std::sync::Arc::new(agent_id);
    let edge_id = std::sync::Arc::new(edge_id);

    let lease_guard = ClaimedTaskLeaseGuard::new(
        lease_svc.clone(),
        user_id.clone(),
        task_id.clone(),
        agent_id.clone(),
    );
    let mut renewal_task =
        astra_services::LeaseRenewalTask::spawn(astra_services::LeaseRenewalConfig {
            lease_svc: lease_svc.clone(),
            user_id: user_id.clone(),
            task_id: task_id.clone(),
            agent_id: agent_id.clone(),
            edge_id: edge_id.clone(),
            ttl_sec: args.ttl_seconds,
            metrics: None,
        });

    // Honour Ctrl+C during long-running task execution. Without this the
    // worker has to wait for the task body to finish, which can be
    // minutes; users expect interrupt to be prompt. On Ctrl+C we fall
    // through to release_lease so the task is freed for another worker.
    // `interrupted` lets the outer --loop driver exit cleanly instead
    // of requiring a second Ctrl+C during the poll-interval sleep.
    let (body_result, interrupted): (Result<ExitCode, String>, bool) = tokio::select! {
        res = async {
            let session_routing = resolve_one_shot_session_routing(
                api,
                profile,
                session_id,
                true,
            ).await?;
            execute_headless_task_body(
                HeadlessTaskInput {
                    task_id: task_id.clone(),
                    task_session_id: task_session_id.clone(),
                    prompt,
                    svc: svc.clone(),
                    session_routing,
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
            ).await
        } => (res, false),
        _ = tokio::signal::ctrl_c() => {
            if !args.quiet && !args.json {
                eprintln!("  {}", "Task interrupted — releasing lease.".dim());
            }
            (Ok(ExitCode::Success), true)
        }
    };

    renewal_task.cancel_and_wait().await;

    if interrupted {
        use std::time::Duration;
        revert_interrupted_task_to_pending_if_still_owned(
            &*svc,
            &*lease_svc,
            user_id.as_str(),
            task_id.as_str(),
            agent_id.as_str(),
            Duration::from_secs(5),
        )
        .await;
    }

    lease_guard.release_and_disarm().await?;
    body_result.map(|code| {
        if interrupted {
            WorkerOutcome::Interrupted
        } else {
            WorkerOutcome::Completed(code)
        }
    })
}

async fn execute_task_worker(
    args: TaskWorkerArgs,
    profile: Option<&str>,
    global_model: Option<&str>,
    api: &astra_thin_client::ThinClient,
    cli_context: &crate::cli::cli_config::cli_context::CliContext,
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

async fn execute_repl_bridge_command(
    slash_cmd: &str,
    arg: &str,
    profile: Option<&str>,
    global_model: Option<&str>,
    api: &astra_thin_client::ThinClient,
    cli_context: &crate::cli::cli_config::cli_context::CliContext,
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

    let pipeline_modules = create_pipeline_modules(api, profile);
    state.unified_skill_registry = pipeline_modules.unified_skill_registry.clone();
    state.mcp_manager = pipeline_modules.mcp_manager.clone();

    let token = repl_bridge_access_token(slash_cmd, api, profile).await?;
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
            crate::cli::slash::slash_plan::handle_plan_command(
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

#[cfg(test)]
mod permission_mode_display_tests {
    use super::PermissionMode;
    use crate::cli::permission_command::{
        handle_permission_command, permission_mode_display_label,
    };
    use crate::cli::session::session_state::SessionState;

    #[test]
    fn labels_match_tui_status_chips() {
        assert_eq!(permission_mode_display_label(PermissionMode::Prompt), "Ask");
        assert_eq!(permission_mode_display_label(PermissionMode::Auto), "Auto");
        assert_eq!(
            permission_mode_display_label(PermissionMode::AcceptEdits),
            "Edits"
        );
        assert_eq!(permission_mode_display_label(PermissionMode::Plan), "Plan");
        assert_eq!(permission_mode_display_label(PermissionMode::Deny), "Deny");
    }

    #[test]
    fn removed_permission_aliases_do_not_change_mode() {
        for alias in ["all", "default", "ask", "accept-edits"] {
            let mut state = SessionState::default();
            state.perm_manager.set_mode(PermissionMode::Deny);

            handle_permission_command(alias, &mut state);

            assert_eq!(
                state.perm_manager.mode(),
                PermissionMode::Deny,
                "removed alias must be rejected: {alias}"
            );
        }
    }
}

#[cfg(test)]
mod token_refresh_error_tests {
    use super::{repl_bridge_access_token, repl_bridge_command_requires_access_token};

    struct EnvVarGuard {
        key: &'static str,
        previous: Option<String>,
    }

    impl EnvVarGuard {
        fn remove(key: &'static str) -> Self {
            let previous = std::env::var(key).ok();
            // SAFETY: callers use `#[serial]` to isolate process env mutation.
            unsafe { std::env::remove_var(key) };
            Self { key, previous }
        }
    }

    impl Drop for EnvVarGuard {
        fn drop(&mut self) {
            match &self.previous {
                Some(value) => {
                    // SAFETY: callers use `#[serial]` to isolate process env mutation.
                    unsafe { std::env::set_var(self.key, value) };
                }
                None => {
                    // SAFETY: callers use `#[serial]` to isolate process env mutation.
                    unsafe { std::env::remove_var(self.key) };
                }
            }
        }
    }

    #[test]
    fn repl_bridge_auth_policy_matches_command_capabilities() {
        for command in ["/team", "/task", "/memory", "/plan", "/review", "/grep"] {
            assert!(
                repl_bridge_command_requires_access_token(command),
                "{command} needs cloud auth or delegation wiring and must fail fast"
            );
        }
        for command in ["/diff", "/allow", "/debug", "/bug", "/agent", "/telemetry"] {
            assert!(
                !repl_bridge_command_requires_access_token(command),
                "{command} must remain available without cloud auth"
            );
        }
    }

    #[serial_test::serial]
    #[tokio::test]
    async fn repl_bridge_auth_policy_fails_fast_only_for_cloud_commands() {
        let _env = EnvVarGuard::remove("ASTRA_ACCESS_TOKEN");

        let api = astra_thin_client::ThinClient::new("http://127.0.0.1:9", None).unwrap();
        let missing_profile = Some("__missing_repl_bridge_auth_test_profile__");

        let err = repl_bridge_access_token("/task", &api, missing_profile)
            .await
            .unwrap_err();
        assert!(
            err.contains("Unable to obtain a valid access token"),
            "cloud-backed slash commands should fail before running half-wired: {err}"
        );

        let local_token = repl_bridge_access_token("/diff", &api, missing_profile)
            .await
            .expect("local slash command auth lookup should be best-effort");
        assert_eq!(local_token, None);
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
    cli_context: &crate::cli::cli_config::cli_context::CliContext,
) -> Result<ExitCode, String> {
    Box::pin(execute_cli_command_impl(
        command,
        profile,
        global_model,
        auto_approve,
        system_prompt,
        api,
        no_instructions,
        max_budget,
        cli_context,
    ))
    .await
}

#[allow(clippy::too_many_arguments)]
async fn execute_cli_command_impl(
    command: Option<Command>,
    profile: Option<String>,
    global_model: Option<String>,
    auto_approve: bool,
    system_prompt: Option<String>,
    api: &astra_thin_client::ThinClient,
    no_instructions: bool,
    max_budget: f64,
    cli_context: &crate::cli::cli_config::cli_context::CliContext,
) -> Result<ExitCode, String> {
    match command {
        // No subcommand → interactive TUI (Codex-style default)
        None | Some(Command::Interactive) => {
            let mut interactive_context = cli_context.clone();
            let resume_session_id = interactive_context.session_id.take();
            run_interactive_chat(
                api,
                profile.as_deref(),
                global_model.as_deref(),
                resume_session_id.as_deref(),
                no_instructions,
                max_budget,
                &interactive_context,
            )
            .await?;
            Ok(ExitCode::Success)
        }

        Some(Command::Serve(args)) => {
            match args.mode {
                None => {
                    start_http_server(&args.host, args.port).await?;
                }
                Some(crate::cli::cli_config::cli_args::ServeMode::Http(http_args)) => {
                    start_http_server(&http_args.host, http_args.port).await?;
                }
                Some(crate::cli::cli_config::cli_args::ServeMode::Stdio) => {
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

        Some(Command::Admin(args)) => {
            let inherited_api_url = api.api_origin();
            crate::admin_cli::run(args, Some(&inherited_api_url), profile.as_deref()).await?;
            Ok(ExitCode::Success)
        }

        // Inline message: astra "what is the answer to life?"
        Some(Command::Message(words)) => {
            let raw_message = words.join(" ");
            let message = apply_system_prompt(&raw_message, system_prompt.as_deref());
            let (_, _, _, _token) = get_profile_and_token(profile.as_deref())?;
            let token = fresh_access_token_or_error(api, profile.as_deref()).await?;
            let session_routing = resolve_one_shot_session_routing(
                api,
                profile.as_deref(),
                cli_context.session_id.clone(),
                true,
            )
            .await?;
            let session_id = session_routing.server_session_id.clone();
            let effective_model = resolve_one_shot_model(
                api,
                &token,
                None,
                session_routing.restored_model(),
                global_model.as_deref(),
            )
            .await;
            let effective_permission_mode = effective_one_shot_permission_mode(
                None,
                auto_approve,
                session_routing.restored_permission_mode(),
                false,
            )?;
            let mut continuation_messages = session_routing.continuation_messages();
            let _pipeline = create_pipeline_modules(api, profile.as_deref());
            let mut pm = PermissionManager::with_load_policy(
                effective_permission_mode,
                &std::env::current_dir().unwrap_or_default(),
                &crate::cli::permission_manager::PermissionLoadPolicy::HeadlessSafe,
            );
            let mut skill_qt = astra_skills::quality::SkillQualityTracker::new();
            let chat_ctx = crate::cli::chat_stream::BasicCliChatContext {
                api,
                auth_profile: profile.as_deref(),
                message: &message,
                model: effective_model.as_deref(),
                provider: None,
                explain: ExplainMode::Off,
                render_md: terminal::size().is_ok(),
                verbose_mode: true,
                render_policy: crate::cli::stream::stream_render::RenderPolicy::Stream,
                cli_context: Some(cli_context),
                unified_skill_registry: astra_runtime::skills::default_unified_registry(),
                agent_spawner: None,
                root_agent_id: None,
                task_manager: None,
                task_notify_tx: None,
                bg_task_commands: None,
                bg_task_list_cache: None,
                bash_detach_slot: None,
                stream_event_tx: None,
                #[cfg(feature = "harness")]
                harness_sink: Some(astra_harness::InMemorySnapshotSink::arc()),
                #[cfg(feature = "harness")]
                harness_trace: Some(std::sync::Arc::new(std::sync::RwLock::new(
                    astra_harness::SessionTrace::new(None),
                ))),
                #[cfg(feature = "harness")]
                benchmark_profile: None,
            };
            let turn_options = crate::cli::turn::turn_facade::BasicCliTurnOptions {
                pre_loaded_messages: continuation_messages.take(),
                ..Default::default()
            };
            let turn_start = std::time::Instant::now();
            let mut sr = match crate::cli::turn::execute_basic_cli_turn(
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
                            crate::cli::turn::execute_basic_cli_turn(
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
            let exit_code = finalize_one_shot_stream_result(
                profile.as_deref(),
                effective_model.as_deref(),
                &message,
                &mut sr,
                turn_start,
            );
            print_one_shot_completion_warning(&sr, exit_code, false);
            Ok(exit_code)
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
            clear_profile_auth(profile.as_deref())?;
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
                crate::cli::task::task_queue_command::execute_task_queue(queue_args, cli_context)
                    .await
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
            Some(TaskSubcommand::Result(result_args)) => {
                crate::cli::task::task_result_command::execute_task_result(result_args).await
            }
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
                crate::cli::cli_config::cli_args::ContextCmd::Dump(args) => {
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
                let mut interactive_context = cli_context
                    .clone()
                    .with_permission_mode(args.permission_mode.clone());
                let resume_session_id = args
                    .session_id
                    .clone()
                    .or_else(|| interactive_context.session_id.take());
                run_interactive_chat(
                    api,
                    profile.as_deref(),
                    model,
                    resume_session_id.as_deref(),
                    no_instructions,
                    max_budget,
                    &interactive_context,
                )
                .await?;
                return Ok(ExitCode::Success);
            };

            let (_, _, _, _token) = get_profile_and_token(profile.as_deref())?;
            let token = fresh_access_token_or_error(api, profile.as_deref()).await?;
            let explicit_session_id = args.session_id.clone();
            let session_routing = resolve_one_shot_session_routing(
                api,
                profile.as_deref(),
                match explicit_session_id {
                    Some(session_id) => Some(session_id),
                    None => cli_context.session_id.clone(),
                },
                !args.no_resume,
            )
            .await?;
            let session_id = session_routing.server_session_id.clone();
            let effective_model = resolve_one_shot_model(
                api,
                &token,
                args.model.as_deref(),
                session_routing.restored_model(),
                global_model.as_deref(),
            )
            .await;
            let effective_permission_mode = effective_one_shot_permission_mode(
                args.permission_mode.as_deref(),
                args.auto_approve || auto_approve,
                session_routing.restored_permission_mode(),
                false,
            )?;
            let mut continuation_messages = session_routing.continuation_messages();
            let is_tty = terminal::size().is_ok();
            let _pipeline = create_pipeline_modules(api, profile.as_deref());
            let mut pm = {
                let project_root = std::env::current_dir().unwrap_or_default();
                PermissionManager::with_load_policy(
                    effective_permission_mode,
                    &project_root,
                    &crate::cli::permission_manager::PermissionLoadPolicy::HeadlessSafe,
                )
            };
            let explain_mode = args.explain.unwrap_or(ExplainMode::Off);

            // --json implies --quiet
            let quiet = args.quiet || args.json;
            // When quiet, don't render markdown (no terminal formatting)
            let render_md = is_tty && !quiet;

            let mut skill_qt = astra_skills::quality::SkillQualityTracker::new();
            let render_policy = if quiet {
                crate::cli::stream::stream_render::RenderPolicy::Silent
            } else {
                crate::cli::stream::stream_render::RenderPolicy::Stream
            };

            // One-shot chat uses the same local agent spawner wiring as the
            // REPL so agent(action='spawn', ...) has the same behavior.
            let root_agent_id = format!("root-{}", uuid::Uuid::new_v4());
            let one_shot_spawner = super::agent_runtime::build_one_shot_spawner(
                api,
                token.clone(),
                astra_runtime::skills::default_unified_registry().clone(),
                session_id.clone(),
                effective_model.clone(),
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
                let handle = crate::cli::stream::stream_events_writer::spawn_stderr_writer(rx);
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
            // runs so unified `task(action=...)` calls in this path write
            // through to `session_todos`. Without this the tool runs against
            // a throwaway in-memory manager and the Tier 1 board is invisible
            // across edge/cloud boundaries.
            let chat_task_manager = build_one_shot_task_manager(
                profile.as_deref(),
                &api.api_origin(),
                session_id.as_deref(),
            )
            .await;
            let chat_ctx = crate::cli::chat_stream::BasicCliChatContext {
                api,
                auth_profile: profile.as_deref(),
                message: &message,
                model: effective_model.as_deref(),
                provider: None,
                explain: explain_mode,
                render_md,
                verbose_mode: !quiet,
                render_policy,
                cli_context: Some(cli_context),
                unified_skill_registry: astra_runtime::skills::default_unified_registry(),
                agent_spawner: Some(one_shot_spawner),
                root_agent_id: Some(&root_agent_id),
                task_manager: Some(chat_task_manager),
                task_notify_tx: None,
                bg_task_commands: None,
                bg_task_list_cache: None,
                bash_detach_slot: None,
                stream_event_tx,
                #[cfg(feature = "harness")]
                harness_sink: Some(harness_sink.clone()),
                #[cfg(feature = "harness")]
                harness_trace: Some(harness_trace),
                #[cfg(feature = "harness")]
                benchmark_profile: args.benchmark_profile,
            };
            let turn_options = crate::cli::turn::turn_facade::BasicCliTurnOptions {
                pre_loaded_messages: continuation_messages.take(),
                append_system_prompt: args.append_system_prompt.clone(),
                disable_session_not_found_retry: args.no_resume || args.session_id.is_some(),
                ..Default::default()
            };
            let turn_start = std::time::Instant::now();
            let mut sr = match crate::cli::turn::execute_basic_cli_turn(
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

            let exit_code = finalize_one_shot_stream_result(
                profile.as_deref(),
                effective_model.as_deref(),
                &message,
                &mut sr,
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

            print_one_shot_completion_warning(&sr, exit_code, args.json);

            Ok(exit_code)
        }

        Some(Command::Replay(args)) => {
            let session_id = validated_cli_session_arg(&args.session_id)?;
            let (_, _, _, token) = get_profile_and_token(profile.as_deref())?;
            let replay_body = api
                .post_session_replay_json(
                    &token,
                    session_id,
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
                    .get_session_replay_compare_text(&token, session_id)
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
            let session_id = validated_cli_session_arg(&args.session_id)?;
            let (_, _, _, token) = get_profile_and_token(profile.as_deref())?;
            let body = api
                .get_session_text(&token, session_id)
                .await
                .map_err(map_thin_err)?;
            print_json_or_raw(&body);
            Ok(ExitCode::Success)
        }

        Some(Command::Session(SessionCmd::Close(args))) => {
            let session_id = validated_cli_session_arg(&args.session_id)?;
            let (_, _, _, token) = get_profile_and_token(profile.as_deref())?;
            let body = api
                .post_session_close_text(&token, session_id)
                .await
                .map_err(map_thin_err)?;
            clear_profile_last_session_if_matches_or_warn(
                profile.as_deref(),
                session_id,
                "command_router:session_close",
            );
            print_json_or_raw(&body);
            Ok(ExitCode::Success)
        }

        Some(Command::Session(SessionCmd::Delete(args))) => {
            let session_id = validated_cli_session_arg(&args.session_id)?;
            let (_, _, _, token) = get_profile_and_token(profile.as_deref())?;
            let body = api
                .delete_session_text(&token, session_id)
                .await
                .map_err(map_thin_err)?;
            clear_profile_last_session_if_matches_or_warn(
                profile.as_deref(),
                session_id,
                "command_router:session_delete",
            );
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
            let pipeline_modules = create_pipeline_modules_quiet(api, profile.as_deref());
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
            let pipeline_modules = create_pipeline_modules_quiet(api, profile.as_deref());
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
            let session_id = validated_cli_session_arg(&args.session_id)?;
            let (_, _, _, token) = get_profile_and_token(profile.as_deref())?;
            let body = api
                .get_bearer_path_query_text(&token, &paths::session_audit_summary(session_id), &[])
                .await
                .map_err(map_thin_err)?;
            print_json_or_raw(&body);
            Ok(ExitCode::Success)
        }

        Some(Command::Audit(AuditCmd::Turns(args))) => {
            let session_id = validated_cli_session_arg(&args.session_id)?;
            let (_, _, _, token) = get_profile_and_token(profile.as_deref())?;
            let body = if let Some(turn) = args.turn {
                api.get_bearer_path_query_text(
                    &token,
                    &paths::session_audit_turn_detail(session_id, turn),
                    &[],
                )
                .await
            } else {
                let q = vec![
                    ("page", args.page.to_string()),
                    ("per_page", args.per_page.to_string()),
                ];
                api.get_bearer_path_query_text(&token, &paths::session_audit_turns(session_id), &q)
                    .await
            }
            .map_err(map_thin_err)?;
            print_json_or_raw(&body);
            Ok(ExitCode::Success)
        }

        Some(Command::Audit(AuditCmd::Tools(args))) => {
            let session_id = args
                .session_id
                .as_deref()
                .map(validated_cli_session_arg)
                .transpose()?;
            let (_, _, _, token) = get_profile_and_token(profile.as_deref())?;
            let body = if let Some(sid) = session_id {
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
            execute_mcp_command(mcp_cmd).await?;
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
    stream_result_exit_code(sr)
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
    let mut tool_result_class_counts = serde_json::Map::new();
    for class in sr
        .tool_call_records
        .iter()
        .filter_map(|record| record.result_class.as_deref())
    {
        let next = tool_result_class_counts
            .get(class)
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0)
            + 1;
        tool_result_class_counts.insert(class.to_string(), serde_json::json!(next));
    }
    serde_json::json!({
        "trace_id": trace_id,
        "request_id": request_id,
        "run_id": sr.run_id,
        "session_id": sr.session_id,
        "text": sr.full_text,
        "final_state": sr.final_state,
        "interruption_kind": sr.interruption_kind,
        "tool_result_class_counts": tool_result_class_counts,
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
        "persistence_error": sr.session_persistence_error,
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
    cli_context: &crate::cli::cli_config::cli_context::CliContext,
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

    let (_, _, _, _token) = get_profile_and_token(profile)?;
    let token = fresh_access_token_or_error(api, profile).await?;
    let session_routing =
        resolve_one_shot_session_routing(api, profile, cli_context.session_id.clone(), true)
            .await?;
    let session_id = session_routing.server_session_id.clone();
    let effective_model =
        resolve_one_shot_model(api, &token, None, session_routing.restored_model(), model).await;
    let effective_permission_mode = effective_one_shot_permission_mode(
        None,
        false,
        session_routing.restored_permission_mode(),
        true,
    )?;
    let mut continuation_messages = session_routing.continuation_messages();
    let _pipeline = create_pipeline_modules(api, profile);
    // Print mode is non-interactive. Restored session mode wins when present;
    // otherwise Auto is the headless fallback.
    // Issue #326 P5b: print mode is headless — strip project
    // allow rules so a hostile project file can't quietly enable
    // capabilities the user didn't ask for. Project deny rules
    // still apply (a project can tighten, never loosen, the
    // headless policy).
    let mut pm = PermissionManager::with_load_policy(
        effective_permission_mode,
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

    // Print mode wires an MO-backed TaskManager when available so that the
    // `task` tool's writes land in `session_todos` the same way the REPL
    // path handles them. Without this, single-shot runs silently drop to
    // in-memory scratchpad and the Tier 1 board is invisible across turns
    // that reuse the same `session_id`.
    let print_task_manager = build_one_shot_task_manager(
        profile,
        &api.api_origin(),
        session_routing.task_scope_session_id(),
    )
    .await;

    let chat_ctx = crate::cli::chat_stream::BasicCliChatContext {
        api,
        auth_profile: profile,
        message: &message,
        model: effective_model.as_deref(),
        provider: None,
        explain: ExplainMode::Off,
        render_md: false,
        verbose_mode: false,
        render_policy: crate::cli::stream::stream_render::RenderPolicy::Silent,
        cli_context: Some(cli_context),
        unified_skill_registry: astra_runtime::skills::default_unified_registry(),
        agent_spawner: None,
        root_agent_id: None,
        task_manager: Some(print_task_manager),
        task_notify_tx: None,
        bg_task_commands: None,
        bg_task_list_cache: None,
        bash_detach_slot: None,
        stream_event_tx: None,
        #[cfg(feature = "harness")]
        harness_sink: Some(astra_harness::InMemorySnapshotSink::arc()),
        #[cfg(feature = "harness")]
        harness_trace: Some(std::sync::Arc::new(std::sync::RwLock::new(
            astra_harness::SessionTrace::new(None),
        ))),
        #[cfg(feature = "harness")]
        benchmark_profile: None,
    };

    let turn_options = crate::cli::turn::turn_facade::BasicCliTurnOptions {
        pre_loaded_messages: continuation_messages.take(),
        ..Default::default()
    };
    let turn_start = std::time::Instant::now();
    let mut sr = match crate::cli::turn::execute_basic_cli_turn(
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

    let exit_code = finalize_one_shot_stream_result(
        profile,
        effective_model.as_deref(),
        &message,
        &mut sr,
        turn_start,
    );

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

    print_one_shot_completion_warning(
        &sr,
        exit_code,
        matches!(output_format, "json" | "stream-json"),
    );

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
    use super::{ExitCode, StreamResult, compute_exit_code};
    #[cfg(feature = "harness")]
    use super::{append_headless_inspect_snapshot, message_requests_headless_inspect};
    use crate::cli::stream::streaming_types::VerdictEvent;

    fn empty_stream_result() -> StreamResult {
        StreamResult::default()
    }

    #[test]
    fn exit_code_success_on_empty_result() {
        let sr = empty_stream_result();
        assert_eq!(compute_exit_code(&sr), ExitCode::Success);
    }

    #[test]
    fn exit_code_persistence_error_on_successful_turn_with_durability_failure() {
        let mut sr = empty_stream_result();
        sr.session_persistence_error = Some("failed to append one-shot journal events".into());
        assert_eq!(compute_exit_code(&sr), ExitCode::PersistenceError);
    }

    #[test]
    fn exit_code_partial_on_interrupted_turn_without_harder_failure() {
        let mut sr = empty_stream_result();
        sr.final_state = "interrupted".into();
        sr.interruption_kind = Some("budget_exhausted".into());
        assert_eq!(compute_exit_code(&sr), ExitCode::Partial);
    }

    #[test]
    fn exit_code_persistence_error_overrides_partial_turn() {
        let mut sr = empty_stream_result();
        sr.final_state = "interrupted".into();
        sr.interruption_kind = Some("budget_exhausted".into());
        sr.session_persistence_error = Some("journal append failed".into());
        assert_eq!(compute_exit_code(&sr), ExitCode::PersistenceError);
    }

    #[test]
    fn exit_code_tool_failure_takes_precedence_over_persistence_error() {
        let mut sr = empty_stream_result();
        sr.session_persistence_error = Some("failed to append one-shot journal events".into());
        sr.tool_call_records
            .push(astra_services::session_journal::ToolCallRecord {
                name: "Bash".to_string(),
                ok: false,
                ms: 100,
                error: Some("exit code 1".to_string()),
                ..Default::default()
            });
        assert_eq!(compute_exit_code(&sr), ExitCode::ToolFailure);
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
    fn exit_code_success_on_empty_result_semantics() {
        let mut sr = empty_stream_result();
        sr.tool_call_records.push(tool_call_record(
            "Bash",
            false,
            Some("grep returned 1"),
            Some("empty_result"),
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
            health_avoidance_tools: vec![],
            force_stop: true,
            nudge_count: 0,
            interaction_mode: "prompt".to_string(),
            suppressed_loop_nudges: false,
            total_errors: 3,
            health_avoidance_count: 0,
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
            health_avoidance_tools: vec![],
            force_stop: false,
            nudge_count: 1,
            interaction_mode: "prompt".to_string(),
            suppressed_loop_nudges: false,
            total_errors: 1,
            health_avoidance_count: 0,
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
    use super::{ExitCode, StreamResult, final_json_output_with_context};

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
            tools_used: vec!["bash".to_string(), "read_file".to_string()],
            ..Default::default()
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
        assert_eq!(output["final_state"], "completed");
        assert!(output["interruption_kind"].is_null());
        assert_eq!(output["tool_result_class_counts"], serde_json::json!({}));
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
            "final_state",
            "interruption_kind",
            "tool_result_class_counts",
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

    #[test]
    fn final_json_output_includes_persistence_error() {
        let mut sr = stream_result_for_json();
        sr.session_persistence_error = Some("failed to append one-shot journal events".into());
        let output = final_json_output_with_context(&sr, ExitCode::PersistenceError, None, None);

        assert_eq!(output["exit_code"], 4);
        assert_eq!(output["success"], false);
        assert_eq!(output["error_kind"], "persistence_error");
        assert_eq!(
            output["persistence_error"],
            "failed to append one-shot journal events"
        );
    }
}

#[cfg(test)]
mod one_shot_effective_settings_tests {
    use super::{effective_one_shot_model, effective_one_shot_permission_mode};
    use crate::cli::permission_manager::PermissionMode;

    #[test]
    fn effective_one_shot_model_prefers_explicit_then_restored_then_fallback() {
        assert_eq!(
            effective_one_shot_model(Some("chat-explicit"), Some("restored"), Some("fallback")),
            Some("chat-explicit")
        );
        assert_eq!(
            effective_one_shot_model(None, Some("restored"), Some("fallback")),
            Some("restored")
        );
        assert_eq!(
            effective_one_shot_model(None, None, Some("fallback")),
            Some("fallback")
        );
    }

    #[test]
    fn effective_one_shot_permission_mode_prefers_explicit_then_auto_then_restored() {
        assert_eq!(
            effective_one_shot_permission_mode(Some("plan"), true, Some("accept_edits"), false)
                .unwrap(),
            PermissionMode::Plan
        );
        assert_eq!(
            effective_one_shot_permission_mode(None, true, Some("plan"), false).unwrap(),
            PermissionMode::Auto
        );
        assert_eq!(
            effective_one_shot_permission_mode(None, false, Some("accept_edits"), false).unwrap(),
            PermissionMode::AcceptEdits
        );
        assert_eq!(
            effective_one_shot_permission_mode(None, false, None, true).unwrap(),
            PermissionMode::Auto
        );
    }
}

#[cfg(test)]
mod one_shot_persistence_tests {
    use super::{
        ExitCode, StreamResult, finalize_one_shot_stream_result, persist_one_shot_session_state,
    };

    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    #[cfg(unix)]
    #[test]
    #[serial_test::serial]
    fn persist_one_shot_session_state_marks_stream_result_and_skips_pointer_update_on_append_failure()
     {
        let _home = crate::tests::HomeGuard::temp();
        let sessions = dirs::home_dir().unwrap().join(".astra").join("sessions");
        std::fs::create_dir_all(&sessions).unwrap();
        let _guard = astra_services::session_journal::JournalDirGuard::new(&sessions);

        let sid = format!("one-shot-persist-{}", uuid::Uuid::new_v4());
        let writer = astra_services::session_journal::JournalWriter::new(&sid).unwrap();
        writer
            .append(
                &astra_services::session_journal::JournalEvent::llm_request_full(
                    Some(&sid),
                    1,
                    0,
                    serde_json::json!({
                        "request": {
                            "messages": [{"role": "user", "content": "hi"}],
                            "tools": []
                        },
                        "model": "test-model",
                        "provider": "openai"
                    }),
                ),
            )
            .unwrap();
        writer
            .append(
                &astra_services::session_journal::JournalEvent::llm_response_full(
                    Some(&sid),
                    1,
                    0,
                    serde_json::json!({
                        "response": {
                            "response": {
                                "usage": {
                                    "input_tokens": 1,
                                    "output_tokens": 1
                                }
                            }
                        },
                        "provider": "openai"
                    }),
                ),
            )
            .unwrap();

        let journal_path = astra_services::session_journal::journal_file_path(&sid);
        std::fs::set_permissions(&journal_path, std::fs::Permissions::from_mode(0o444)).unwrap();

        let mut sr = StreamResult {
            session_id: Some(sid.clone()),
            run_id: None,
            session_persistence_error: None,
            full_text: "answer".into(),
            prompt_tokens: 0,
            completion_tokens: 0,
            cache_read_tokens: 0,
            cache_creation_tokens: 0,
            tool_calls_count: 0,
            visible_tools: Vec::new(),
            selected_skills: Vec::new(),
            tools_used: Vec::new(),
            tool_call_records: Vec::new(),
            budget_used: 0,
            budget_pressure: 0.0,
            stall_events: Vec::new(),
            verdict_events: Vec::new(),
            step_recorder_summary: None,
            tool_health_export: Vec::new(),
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
            final_state: "completed".into(),
            interruption_kind: None,
            final_messages: Vec::new(),
            background_agent_results: Vec::new(),
        };

        persist_one_shot_session_state(
            Some("default"),
            Some("test-model"),
            "continue",
            &mut sr,
            std::time::Instant::now(),
        );

        assert!(
            sr.session_persistence_error
                .as_deref()
                .unwrap_or_default()
                .contains("failed to append one-shot journal events")
        );
        let creds = crate::cli::cli_config::cli_utils::load_credentials();
        assert_eq!(
            creds
                .profiles
                .get("default")
                .and_then(|profile| profile.last_session_id.clone()),
            None
        );

        std::fs::set_permissions(&journal_path, std::fs::Permissions::from_mode(0o644)).unwrap();
    }

    #[cfg(unix)]
    #[test]
    #[serial_test::serial]
    fn finalize_one_shot_stream_result_returns_persistence_error_on_append_failure() {
        let _home = crate::tests::HomeGuard::temp();
        let sessions = dirs::home_dir().unwrap().join(".astra").join("sessions");
        std::fs::create_dir_all(&sessions).unwrap();
        let _guard = astra_services::session_journal::JournalDirGuard::new(&sessions);

        let sid = format!("one-shot-exit-{}", uuid::Uuid::new_v4());
        let writer = astra_services::session_journal::JournalWriter::new(&sid).unwrap();
        writer
            .append(
                &astra_services::session_journal::JournalEvent::llm_request_full(
                    Some(&sid),
                    1,
                    0,
                    serde_json::json!({
                        "request": {
                            "messages": [{"role": "user", "content": "hi"}],
                            "tools": []
                        },
                        "model": "test-model",
                        "provider": "openai"
                    }),
                ),
            )
            .unwrap();
        writer
            .append(
                &astra_services::session_journal::JournalEvent::llm_response_full(
                    Some(&sid),
                    1,
                    0,
                    serde_json::json!({
                        "response": {
                            "response": {
                                "usage": {
                                    "input_tokens": 1,
                                    "output_tokens": 1
                                }
                            }
                        },
                        "provider": "openai"
                    }),
                ),
            )
            .unwrap();
        let journal_path = astra_services::session_journal::journal_file_path(&sid);
        std::fs::set_permissions(&journal_path, std::fs::Permissions::from_mode(0o444)).unwrap();

        let mut sr = StreamResult {
            session_id: Some(sid.clone()),
            run_id: None,
            session_persistence_error: None,
            full_text: "answer".into(),
            prompt_tokens: 0,
            completion_tokens: 0,
            cache_read_tokens: 0,
            cache_creation_tokens: 0,
            tool_calls_count: 0,
            visible_tools: Vec::new(),
            selected_skills: Vec::new(),
            tools_used: Vec::new(),
            tool_call_records: Vec::new(),
            budget_used: 0,
            budget_pressure: 0.0,
            stall_events: Vec::new(),
            verdict_events: Vec::new(),
            step_recorder_summary: None,
            tool_health_export: Vec::new(),
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
            final_state: "completed".into(),
            interruption_kind: None,
            final_messages: Vec::new(),
            background_agent_results: Vec::new(),
        };

        let exit_code = finalize_one_shot_stream_result(
            Some("default"),
            Some("test-model"),
            "continue",
            &mut sr,
            std::time::Instant::now(),
        );

        assert_eq!(exit_code, ExitCode::PersistenceError);
        assert!(
            sr.session_persistence_error
                .as_deref()
                .unwrap_or_default()
                .contains("failed to append one-shot journal events")
        );

        std::fs::set_permissions(&journal_path, std::fs::Permissions::from_mode(0o644)).unwrap();
    }
}

#[cfg(test)]
mod task_run_projection_tests {
    use super::{
        ExitCode, NON_CANONICAL_TASK_SCOPE, StreamResult, build_one_shot_task_manager,
        failed_task_notification_payload, one_shot_completion_warning, task_notification_payload,
        task_terminal_summary_line,
    };
    use crate::cli::task::task_result_projection::{
        stream_result_failure_reason, task_checkpoint_state_from_result,
    };

    fn stream_result_for_task_checkpoint() -> StreamResult {
        StreamResult {
            session_id: Some("session-1".to_string()),
            run_id: Some("run-1".to_string()),
            session_persistence_error: Some("failed to append one-shot journal events".into()),
            full_text: "hello".to_string(),
            prompt_tokens: 10,
            completion_tokens: 3,
            cache_read_tokens: 2,
            cache_creation_tokens: 1,
            tool_calls_count: 2,
            tools_used: vec!["bash".to_string()],
            background_agent_results: vec![("agent-1".into(), "done".into())],
            ..Default::default()
        }
    }

    #[test]
    fn task_checkpoint_state_includes_persistence_error() {
        let sr = stream_result_for_task_checkpoint();
        let state = task_checkpoint_state_from_result(
            &sr,
            Some("/tmp/out.txt"),
            ExitCode::PersistenceError,
        );

        assert_eq!(
            state.get("full_text").and_then(|v| v.as_str()),
            Some("hello")
        );
        assert_eq!(
            state.get("output_file").and_then(|v| v.as_str()),
            Some("/tmp/out.txt")
        );
        assert_eq!(
            state.get("persistence_error").and_then(|v| v.as_str()),
            Some("failed to append one-shot journal events")
        );
        assert_eq!(
            state["background_agent_results"],
            serde_json::json!([{"agent_id":"agent-1","result":"done"}])
        );
        assert_eq!(state["exit_code"], 4);
        assert_eq!(state["error_kind"], "persistence_error");
        assert_eq!(state["final_state"], "completed");
        assert!(state["interruption_kind"].is_null());
    }

    #[test]
    fn task_status_label_marks_partial_completed_tasks() {
        assert_eq!(
            crate::cli::surface::task_checkpoint_surface::task_status_label(
                astra_services::TaskStatus::Completed,
                Some(astra_services::TaskOutcome::Partial),
            ),
            "partial"
        );
        assert_eq!(
            crate::cli::surface::task_checkpoint_surface::task_status_label(
                astra_services::TaskStatus::Completed,
                Some(astra_services::TaskOutcome::Success),
            ),
            "completed"
        );
        assert_eq!(
            crate::cli::surface::task_checkpoint_surface::task_status_label(
                astra_services::TaskStatus::Failed,
                None,
            ),
            "failed"
        );
    }

    #[test]
    fn stream_result_failure_reason_prefers_persistence_detail() {
        let sr = stream_result_for_task_checkpoint();
        assert_eq!(
            stream_result_failure_reason(ExitCode::PersistenceError, &sr),
            "failed to append one-shot journal events"
        );
        assert_eq!(
            stream_result_failure_reason(ExitCode::ToolFailure, &sr),
            "tool_failure"
        );
    }

    #[test]
    fn task_notification_payload_includes_exit_semantics_for_persistence_error() {
        let sr = stream_result_for_task_checkpoint();
        let payload = task_notification_payload(
            "task-12345678",
            &sr,
            Some("/tmp/out.txt"),
            ExitCode::PersistenceError,
        );

        assert_eq!(payload["type"], "background_task_notification");
        assert_eq!(payload["task_id"], "task-12345678");
        assert_eq!(payload["status"], "persistence_error");
        assert_eq!(payload["success"], false);
        assert_eq!(payload["exit_code"], 4);
        assert_eq!(payload["error_kind"], "persistence_error");
        assert_eq!(payload["output_file"], "/tmp/out.txt");
        assert_eq!(payload["final_state"], "completed");
        assert!(payload["interruption_kind"].is_null());
        assert_eq!(
            payload["persistence_error"],
            "failed to append one-shot journal events"
        );
        assert_eq!(payload["summary"], "hello");
    }

    #[test]
    fn task_notification_payload_marks_partial_outcome() {
        let mut sr = stream_result_for_task_checkpoint();
        sr.session_persistence_error = None;
        sr.final_state = "interrupted".into();
        sr.interruption_kind = Some("budget_exhausted".into());

        let payload = task_notification_payload(
            "task-12345678",
            &sr,
            Some("/tmp/out.txt"),
            ExitCode::Partial,
        );

        assert_eq!(payload["status"], "partial");
        assert_eq!(payload["success"], false);
        assert_eq!(payload["exit_code"], 5);
        assert_eq!(payload["error_kind"], "partial");
        assert_eq!(payload["final_state"], "interrupted");
        assert_eq!(payload["interruption_kind"], "budget_exhausted");
    }

    #[test]
    fn failed_task_notification_payload_carries_failure_detail() {
        let payload = failed_task_notification_payload(
            "task-12345678",
            "write task output: permission denied",
            "persistence_error",
            None,
            Some("write task output: permission denied"),
        );

        assert_eq!(payload["type"], "background_task_notification");
        assert_eq!(payload["task_id"], "task-12345678");
        assert_eq!(payload["status"], "persistence_error");
        assert_eq!(payload["success"], false);
        assert_eq!(payload["error_kind"], "persistence_error");
        assert_eq!(payload["summary"], "write task output: permission denied");
        assert_eq!(
            payload["persistence_error"],
            "write task output: permission denied"
        );
        assert!(payload.get("output_file").is_none());
    }

    #[test]
    fn task_terminal_summary_line_distinguishes_success_and_persistence_failure() {
        let success =
            task_terminal_summary_line("task-12345678", Some("/tmp/out.txt"), ExitCode::Success);
        assert!(success.contains("finished; output saved to"));
        assert!(!success.contains("persistence degradation"));

        let partial =
            task_terminal_summary_line("task-12345678", Some("/tmp/out.txt"), ExitCode::Partial);
        assert!(partial.contains("finished partially; output saved to"));

        let degraded = task_terminal_summary_line(
            "task-12345678",
            Some("/tmp/out.txt"),
            ExitCode::PersistenceError,
        );
        assert!(degraded.contains("finished with persistence degradation; output saved to"));

        let tool_failure = task_terminal_summary_line(
            "task-12345678",
            Some("/tmp/out.txt"),
            ExitCode::ToolFailure,
        );
        assert!(tool_failure.contains("failed; output saved to"));

        let unfinished =
            task_terminal_summary_line("task-12345678", Some("/tmp/out.txt"), ExitCode::Unfinished);
        assert!(unfinished.contains("unfinished; output saved to"));
    }

    #[test]
    fn task_terminal_summary_line_handles_missing_output_file() {
        let degraded =
            task_terminal_summary_line("task-12345678", None, ExitCode::PersistenceError);
        assert!(degraded.contains("output file unavailable"));
    }

    #[test]
    fn one_shot_completion_warning_prefers_persistence_error_over_partial() {
        let mut sr = stream_result_for_task_checkpoint();
        sr.final_state = "interrupted".into();
        sr.interruption_kind = Some("budget_exhausted".into());

        assert_eq!(
            one_shot_completion_warning(&sr, ExitCode::PersistenceError).as_deref(),
            Some("Session persistence degraded: failed to append one-shot journal events")
        );
    }

    #[test]
    fn one_shot_completion_warning_surfaces_partial_reason() {
        let mut sr = stream_result_for_task_checkpoint();
        sr.session_persistence_error = None;
        sr.final_state = "interrupted".into();
        sr.interruption_kind = Some("budget_exhausted".into());

        assert_eq!(
            one_shot_completion_warning(&sr, ExitCode::Partial).as_deref(),
            Some(
                "Turn finished partially (budget_exhausted). Inspect partial output before continuing."
            )
        );
    }

    #[tokio::test]
    async fn build_one_shot_task_manager_without_canonical_session_uses_ephemeral_store() {
        let api = astra_thin_client::ThinClient::new("http://127.0.0.1:9", None).unwrap();

        let first = build_one_shot_task_manager(None, &api.api_origin(), None).await;
        let second = build_one_shot_task_manager(None, &api.api_origin(), None).await;

        assert_eq!(first.session_id(), NON_CANONICAL_TASK_SCOPE);
        assert_eq!(second.session_id(), NON_CANONICAL_TASK_SCOPE);

        let created = first
            .create(&serde_json::json!({ "title": "ephemeral" }))
            .await;
        assert!(created.contains("ephemeral"));
        assert_eq!(first.snapshot().await.unwrap().len(), 1);
        assert!(
            second.snapshot().await.unwrap().is_empty(),
            "non-canonical one-shot managers must not share a durable session store"
        );
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
        // profile is 4 / 128 / 4 / 3 (see
        // `ToolPolicyConfig::builtin_model_profiles`).
        let cfg = astra_config::runtime_config::RuntimeConfig::load();
        let policy = cfg.tool_policy.resolve_for_model(Some("opus"));
        let human = format_policy_output(Some("opus"), &policy, "strict", &[], false);
        assert!(human.contains("= 4"), "expected 4s for opus: {human}");
        assert!(human.contains("= 128"), "expected 128 for opus: {human}");

        let json = format_policy_output(Some("opus"), &policy, "strict", &[], true);
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["max_identical_tool_calls"], 4);
        assert_eq!(parsed["max_tools_per_turn"], 128);
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
    use crate::cli::config_manager::{
        DEFAULT_API_URL, KNOWN_SETTINGS, latest_artifact_id, read_config_api_url_from,
        resolve_api_url_with, resolve_download_output_path, write_downloaded_capture,
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
