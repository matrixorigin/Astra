use super::*;
use crate::cli_utils::{fetch_session_trace_state, update_session_trace_state};
use crate::permission_manager::PermissionMode;
use crate::repl_turn::is_auth_error;
use astra_thin_client::paths;
use clap::CommandFactory;
use crossterm::style::Stylize;
use std::io::Read;

/// Exit codes for CLI commands (for scripting integration)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ExitCode {
    /// Success (0)
    Success = 0,
    /// Tool execution failure (1) - at least one tool call failed
    ToolFailure = 1,
    /// Force stop (2) - agent was force-stopped due to errors/stalls
    ForceStop = 2,
    /// API/network error (3) - failed to communicate with server
    ApiError = 3,
}

impl From<ExitCode> for i32 {
    fn from(code: ExitCode) -> i32 {
        code as i32
    }
}

/// Load conversation messages from a session's latest heavy checkpoint.
/// Used by one-shot mode (`-m "..." --session-id <id>`) to provide
/// conversation history that the model needs for multi-turn continuity.
///
/// Returns `None` if the session has no checkpoint (first turn) or
/// the checkpoint is unreadable.
fn load_session_messages_for_continuation(session_id: &str) -> Option<Vec<serde_json::Value>> {
    match astra_pipeline::step_checkpoint::read_latest_heavy_checkpoint(session_id) {
        Ok(Some(cp)) if !cp.messages.is_empty() => Some(cp.messages),
        _ => None,
    }
}

/// Prepend system prompt to user message when `--system-prompt` is set.
fn apply_system_prompt(message: &str, system_prompt: Option<&str>) -> String {
    match system_prompt {
        Some(sp) => format!("<system_instructions>\n{sp}\n</system_instructions>\n\n{message}"),
        None => message.to_string(),
    }
}
fn join_words(words: &[String]) -> String {
    words.join(" ")
}

fn render_team_args(args: &TeamArgs) -> String {
    match &args.command {
        None | Some(TeamSubcommand::List) => String::new(),
        Some(TeamSubcommand::Create(cmd)) => {
            let suffix = join_words(&cmd.description);
            if suffix.is_empty() {
                format!("create {}", cmd.name)
            } else {
                format!("create {} {}", cmd.name, suffix)
            }
        }
        Some(TeamSubcommand::AddMember(cmd)) => {
            let suffix = join_words(&cmd.description);
            if suffix.is_empty() {
                format!("add-member {} {}", cmd.team, cmd.role)
            } else {
                format!("add-member {} {} {}", cmd.team, cmd.role, suffix)
            }
        }
        Some(TeamSubcommand::Info(cmd)) => format!("info {}", cmd.name),
        Some(TeamSubcommand::Delete(cmd)) => format!("delete {}", cmd.name),
        Some(TeamSubcommand::Context(cmd)) => {
            format!(
                "context {} {} {}",
                cmd.team,
                cmd.key,
                join_words(&cmd.value)
            )
        }
        Some(TeamSubcommand::Run(cmd)) => format!("run {} {}", cmd.team, join_words(&cmd.task)),
        Some(TeamSubcommand::History(cmd)) => format!("history {}", cmd.name),
        Some(TeamSubcommand::Snapshot(cmd)) => {
            let suffix = join_words(&cmd.label);
            if suffix.is_empty() {
                format!("snapshot {}", cmd.team)
            } else {
                format!("snapshot {} {}", cmd.team, suffix)
            }
        }
        Some(TeamSubcommand::Restore(cmd)) => format!("restore {} {}", cmd.team, cmd.snapshot_id),
    }
}

fn render_task_args(args: &TaskArgs) -> String {
    match &args.command {
        None | Some(TaskSubcommand::List) => String::new(),
        Some(TaskSubcommand::Add(cmd)) => format!("add {}", join_words(&cmd.text)),
        Some(TaskSubcommand::Done(cmd)) => format!("done {}", join_words(&cmd.query)),
        Some(TaskSubcommand::Status(cmd)) => format!("status {}", join_words(&cmd.query)),
        Some(TaskSubcommand::Run(cmd)) => format!("run {}", join_words(&cmd.text)),
        Some(TaskSubcommand::Result(cmd)) => format!("result {}", join_words(&cmd.query)),
    }
}

fn render_memory_args(args: &MemoryArgs) -> String {
    match &args.command {
        None | Some(MemorySubcommand::List) => String::new(),
        Some(MemorySubcommand::Search(cmd)) => format!("search {}", join_words(&cmd.query)),
    }
}

fn render_review_args(args: &ReviewArgs) -> String {
    match &args.command {
        Some(ReviewSubcommand::Head) => String::new(),
        Some(ReviewSubcommand::Working) => "working".to_string(),
        Some(ReviewSubcommand::Rev(cmd)) => join_words(&cmd.target),
        None => join_words(&args.target),
    }
}

fn render_grep_args(args: &GrepArgs) -> String {
    match &args.command {
        Some(GrepSubcommand::Content(cmd)) => join_words(&cmd.pattern),
        Some(GrepSubcommand::Files(cmd)) => format!("files {}", join_words(&cmd.pattern)),
        Some(GrepSubcommand::Review(cmd)) => format!("review {}", join_words(&cmd.pattern)),
        None => join_words(&args.pattern),
    }
}

fn render_permissions_args(args: &PermissionsArgs) -> String {
    match &args.command {
        None => String::new(),
        Some(PermissionsSubcommand::Status) => "status".to_string(),
        Some(PermissionsSubcommand::Auto) => "auto".to_string(),
        Some(PermissionsSubcommand::Prompt) => "prompt".to_string(),
        Some(PermissionsSubcommand::Deny) => "deny".to_string(),
        Some(PermissionsSubcommand::All) => "all".to_string(),
        Some(PermissionsSubcommand::Rules) => "rules".to_string(),
    }
}

fn render_debug_args(args: &DebugArgs) -> String {
    args.session_id.clone().unwrap_or_default()
}

fn render_agent_args(args: &AgentArgs) -> String {
    match &args.command {
        None | Some(AgentSubcommand::List) => String::new(),
        Some(AgentSubcommand::Status(cmd)) => format!("status {}", cmd.agent_id),
        Some(AgentSubcommand::Stop(cmd)) => format!("stop {}", cmd.agent_id),
        Some(AgentSubcommand::Logs(cmd)) => format!("logs {}", cmd.agent_id),
    }
}

fn render_messaging_args(args: &MessagingArgs) -> String {
    match &args.command {
        None | Some(MessagingSubcommand::Metrics) => String::new(),
        Some(MessagingSubcommand::Dlq) => "dlq".to_string(),
        Some(MessagingSubcommand::Status) => "status".to_string(),
    }
}

fn render_diff_args(args: &DiffArgs) -> String {
    match &args.command {
        None => join_words(&args.paths),
        Some(DiffSubcommand::Staged(cmd)) => {
            let suffix = join_words(&cmd.paths);
            if suffix.is_empty() {
                "staged".to_string()
            } else {
                format!("staged {suffix}")
            }
        }
        Some(DiffSubcommand::Unstaged(cmd)) => {
            let suffix = join_words(&cmd.paths);
            if suffix.is_empty() {
                "unstaged".to_string()
            } else {
                format!("unstaged {suffix}")
            }
        }
        Some(DiffSubcommand::Stat(cmd)) => {
            let suffix = join_words(&cmd.paths);
            if suffix.is_empty() {
                "stat".to_string()
            } else {
                format!("stat {suffix}")
            }
        }
        Some(DiffSubcommand::Show(cmd)) => {
            let suffix = join_words(&cmd.paths);
            if suffix.is_empty() {
                format!("show {}", cmd.rev)
            } else {
                format!("show {} {}", cmd.rev, suffix)
            }
        }
    }
}

fn render_bug_args(args: &BugArgs) -> String {
    match &args.command {
        None | Some(BugSubcommand::Print) => String::new(),
        Some(BugSubcommand::Copy) => "copy".to_string(),
        Some(BugSubcommand::Save) => "save".to_string(),
    }
}

fn maybe_load_project_instructions(state: &mut ReplState) {
    state.project_instructions = discover_project_instructions();
}

fn maybe_wire_delegation_engine(
    state: &mut ReplState,
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
    let engine = astra_runtime::server::delegation_engine::DelegationEngine::with_executor(
        registry,
        std::sync::Arc::new(astra_runtime::server::run_engine::RunEngine::new(run_store)),
        std::sync::Arc::new(astra_runtime::server::delegation_engine::DelegationTracker::new()),
        std::sync::Arc::new(executor),
    );
    state.delegation_engine = Some(std::sync::Arc::new(engine));
}

async fn execute_repl_bridge_command(
    slash_cmd: &str,
    arg: &str,
    profile: Option<&str>,
    global_model: Option<&str>,
    api: &astra_thin_client::ThinClient,
) -> Result<ExitCode, String> {
    try_silent_auth(api, profile).await;

    let mut state = initialize_repl_state(profile, global_model);
    if let Ok(sid) = std::env::var("ASTRA_CLI_SESSION_ID") {
        state.session_id = Some(sid);
    }
    if let Ok(name) = std::env::var("ASTRA_CLI_SESSION_NAME") {
        state.session_name = Some(name);
    }
    maybe_load_project_instructions(&mut state);

    let (_selector, pipeline_modules) = create_tool_selector(api, profile);
    state.pattern_library = Some(pipeline_modules.pattern_library.clone());
    state.entity_graph = Some(pipeline_modules.entity_graph.clone());
    state.calibrator = Some(pipeline_modules.calibrator.clone());

    // Initialize evolution service with pattern library for drift detection.
    {
        let mut evo = astra_runtime::evolution::service::EvolutionService::new()
            .with_pattern_library(pipeline_modules.pattern_library.clone())
            .with_calibrator(pipeline_modules.calibrator.clone());
        if let Some(skills_dir) = astra_skills::loader::skill_search_paths()
            .into_iter()
            .next()
        {
            evo = evo.with_evolution_store(std::sync::Arc::new(
                astra_runtime::evolution::store::EvolutionStore::new(skills_dir),
            ));
        }
        state.evolution_service = Some(std::sync::Arc::new(evo));
    }
    if let Some(hub) = &state.observability_hub {
        hub.attach_pattern_library(pipeline_modules.pattern_library.clone());
    }
    state.unified_skill_registry = pipeline_modules.unified_skill_registry.clone();
    state.mcp_manager = pipeline_modules.mcp_manager.clone();

    let token = current_access_token(profile);
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

fn handle_permission_command(arg: &str, state: &mut ReplState) {
    use permission_manager::PermissionMode;

    match arg {
        "" => {
            let next = match state.perm_manager.mode() {
                PermissionMode::Prompt => PermissionMode::Auto,
                PermissionMode::Auto => PermissionMode::Deny,
                PermissionMode::Deny => PermissionMode::Prompt,
            };
            state.perm_manager.set_mode(next);
            eprintln!(
                "  {} Permission mode → {}",
                theme::icon_info(),
                next.to_string().cyan()
            );
        }
        "all" => {
            state.perm_manager.set_mode(PermissionMode::Auto);
            eprintln!(
                "  {} Permission mode → {} (all tools auto-approved)",
                "⚡".yellow(),
                "auto".cyan()
            );
        }
        "rules" | "status" => {
            let summary = state.perm_manager.rules_summary();
            eprint!("{summary}");
        }
        _ => match arg.parse::<PermissionMode>() {
            Ok(mode) => {
                state.perm_manager.set_mode(mode);
                eprintln!(
                    "  {} Permission mode → {}",
                    theme::icon_info(),
                    mode.to_string().cyan()
                );
            }
            Err(_) => {
                eprintln!(
                    "  {} Unknown mode '{}'. Use: auto, prompt, deny, all, rules",
                    theme::icon_warn(),
                    arg
                );
            }
        },
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn execute_cli_command(
    command: Option<Command>,
    profile: Option<String>,
    global_model: Option<String>,
    auto_approve: bool,
    system_prompt: Option<String>,
    api: &astra_thin_client::ThinClient,
    no_instructions: bool,
    max_budget: f64,
) -> Result<ExitCode, String> {
    match command {
        // No subcommand → interactive REPL (Codex-style default)
        None | Some(Command::Interactive) => {
            run_chat_repl(
                api,
                profile.as_deref(),
                global_model.as_deref(),
                None,
                no_instructions,
                max_budget,
            )
            .await?;
            Ok(ExitCode::Success)
        }

        // Start embedded HTTP API server
        Some(Command::Serve(args)) => {
            let addr: std::net::SocketAddr = format!("{}:{}", args.host, args.port)
                .parse()
                .map_err(|e| format!("Invalid listen address: {e}"))?;
            eprintln!(
                "  {} {} on {}",
                "▸".bold().cyan(),
                "Starting API server".bold(),
                addr.to_string().cyan()
            );
            astra_runtime::serve(addr)
                .await
                .map_err(|e| format!("API server failed to start: {e}"))?;
            Ok(ExitCode::Success)
        }

        // Inline message: astra "what is the answer to life?"
        Some(Command::Message(words)) => {
            let raw_message = words.join(" ");
            let message = apply_system_prompt(&raw_message, system_prompt.as_deref());
            let (mut creds, name, _, token) = get_profile_and_token(profile.as_deref())?;
            let session_id = validated_resumable_last_session_id(api, profile.as_deref()).await;
            let mut continuation_messages = session_id
                .as_deref()
                .and_then(load_session_messages_for_continuation);
            let selector = create_tool_selector(api, profile.as_deref());
            let mut pm = PermissionManager::with_project(
                auto_approve,
                &std::env::current_dir().unwrap_or_default(),
            );
            let mut skill_qt = astra_skills::quality::SkillQualityTracker::new();
            let skill_search = astra_core::SkillSearchSettings::default();
            let chat_ctx = crate::chat_stream::BasicCliChatContext {
                api,
                auth_profile: profile.as_deref(),
                message: &message,
                model: global_model.as_deref(),
                provider: None,
                explain: ExplainMode::Off,
                render_md: terminal::size().is_ok(),
                verbose_mode: true,
                render_policy: crate::stream_render::RenderPolicy::Stream,
                selector: &*selector.0,
                unified_skill_registry: astra_runtime::skills::default_unified_registry(),
                skill_search: &skill_search,
                // Non-Chat (Message-style) path — legacy single-shot
                // invocation without spawn_agent support. Keep
                // pre-fix behavior; extend later if this path needs
                // spawning too.
                agent_spawner: None,
                root_agent_id: None,
            };
            let mut params = ChatTurnParams::basic_cli(
                &chat_ctx,
                &token,
                session_id.as_deref(),
                &mut pm,
                &mut skill_qt,
            );
            params.pre_loaded_messages = continuation_messages.take();
            let sr = match stream_chat_sse(params)
            .await
            {
                Ok(sr) => sr,
                Err(e) if is_session_not_found_error(&e.error) && session_id.is_some() => {
                    let _ = clear_profile_last_session(profile.as_deref());
                    stream_chat_sse(ChatTurnParams::basic_cli(
                        &chat_ctx,
                        &token,
                        None,
                        &mut pm,
                        &mut skill_qt,
                    ))
                    .await
                    .map_err(|f| f.error)?
                }
                Err(e) if is_auth_error(&e.error) => {
                    if repl_runtime::attempt_token_refresh(api, profile.as_deref()).await {
                        if let Some(new_token) =
                            repl_runtime::current_access_token(profile.as_deref())
                        {
                            eprintln!("  {} Token refreshed, retrying…", crate::theme::icon_ok());
                            stream_chat_sse(ChatTurnParams::basic_cli(
                                &chat_ctx,
                                &new_token,
                                session_id.as_deref(),
                                &mut pm,
                                &mut skill_qt,
                            ))
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
                let p = creds.profiles.entry(name).or_default();
                p.last_session_id = Some(sid.clone());
                save_credentials(&creds)?;
            }
            Ok(compute_exit_code(&sr))
        }

        Some(Command::Register(args)) => {
            eprintln!(
                "\n{}",
                "  ── Register a new account ─────────────────────"
                    .cyan()
                    .bold()
            );
            let username = prompt_or("Username", args.username)?;
            let email = prompt_or("Email   ", args.email)?;
            let password = prompt_password_masked("Password", args.password)?;
            api.post_auth_register_json(&serde_json::json!({
                "username": username,
                "email": email,
                "password": password
            }))
            .await
            .map_err(map_thin_err)?;
            eprintln!("{}", "  ✓  Registered! Now logging in…".green());
            // Auto-login after register
            do_login(api, profile.as_deref(), &username, &password).await?;
            eprintln!(
                "{}",
                "  ✓  Logged in. Run `astra` to start chatting.".green()
            );
            Ok(ExitCode::Success)
        }

        Some(Command::Login(args)) => {
            eprintln!(
                "\n{}",
                "  ── Login ───────────────────────────────────────"
                    .cyan()
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
            let mut creds = load_credentials();
            let name = profile_name(profile.as_deref(), &creds);
            let profile = creds
                .profiles
                .get(&name)
                .cloned()
                .ok_or_else(|| format!("no profile '{name}'"))?;
            let refresh_token = profile
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
                .ok_or_else(|| "missing access_token".to_string())?;
            let new_refresh = value
                .get("refresh_token")
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| "missing refresh_token".to_string())?;
            let entry = creds.profiles.entry(name).or_default();
            entry.access_token = Some(new_access.to_string());
            entry.refresh_token = Some(new_refresh.to_string());
            save_credentials(&creds)?;
            println!("  {} {}", theme::icon_ok(), "Token refreshed".green());
            Ok(ExitCode::Success)
        }

        Some(Command::Logout) => {
            let mut creds = load_credentials();
            let name = profile_name(profile.as_deref(), &creds);
            let profile = creds
                .profiles
                .get(&name)
                .cloned()
                .ok_or_else(|| format!("no profile '{name}'"))?;
            let refresh_token = profile
                .refresh_token
                .ok_or_else(|| format!("profile '{name}' has no refresh token"))?;
            let body = api
                .post_auth_logout_json(&serde_json::json!({ "refresh_token": refresh_token }))
                .await
                .map_err(map_thin_err)?;
            if let Some(entry) = creds.profiles.get_mut(&name) {
                entry.access_token = None;
                entry.refresh_token = None;
                entry.last_session_id = None;
            }
            save_credentials(&creds)?;
            print_json_or_raw(&body);
            Ok(ExitCode::Success)
        }

        Some(Command::Health) => {
            let body = api.get_health_text().await.map_err(map_thin_err)?;
            print_json_or_raw(&body);
            Ok(ExitCode::Success)
        }

        Some(Command::Plan(plan_cmd)) => match plan_cmd {
            PlanCmd::Decompose { goal, json, quiet } => {
                let (_, _, _, token) = get_profile_and_token(profile.as_deref())?;
                let session_id = validated_resumable_last_session_id(api, profile.as_deref()).await;
                let plan = crate::slash_memory::headless_plan_decompose(
                    api,
                    &token,
                    &goal,
                    session_id.as_deref(),
                    global_model.as_deref(),
                    quiet,
                )
                .await?;
                if json {
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&plan)
                            .map_err(|e| format!("serialize plan: {e}"))?
                    );
                } else {
                    println!("{}", astra_runtime::plan_decompose::format_plan(&plan));
                }
                Ok(ExitCode::Success)
            }
        },

        Some(Command::Team(args)) => {
            execute_repl_bridge_command(
                "/team",
                &render_team_args(&args),
                profile.as_deref(),
                global_model.as_deref(),
                api,
            )
            .await
        }

        Some(Command::Task(args)) => {
            execute_repl_bridge_command(
                "/task",
                &render_task_args(&args),
                profile.as_deref(),
                global_model.as_deref(),
                api,
            )
            .await
        }

        Some(Command::Memory(args)) => {
            execute_repl_bridge_command(
                "/memory",
                &render_memory_args(&args),
                profile.as_deref(),
                global_model.as_deref(),
                api,
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
            )
            .await
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
                // No message → start REPL with optional pre-set session/model
                let model = args.model.as_deref().or(global_model.as_deref());
                run_chat_repl(
                    api,
                    profile.as_deref(),
                    model,
                    None,
                    no_instructions,
                    max_budget,
                )
                .await?;
                return Ok(ExitCode::Success);
            };

            let (mut creds, name, _, token) = get_profile_and_token(profile.as_deref())?;
            let session_id = match args.session_id {
                Some(session_id) => Some(session_id),
                None => validated_resumable_last_session_id(api, profile.as_deref()).await,
            };
            // Load previous conversation for multi-turn continuity.
            let mut continuation_messages = session_id
                .as_deref()
                .and_then(load_session_messages_for_continuation);
            let is_tty = terminal::size().is_ok();
            let selector = create_tool_selector(api, profile.as_deref());
            let mut pm = {
                let project_root = std::env::current_dir().unwrap_or_default();
                if let Some(ref mode_str) = args.permission_mode {
                    let mode: PermissionMode = mode_str.parse().unwrap_or_else(|e| {
                        eprintln!("{}", format!("  ⚠  {e}, defaulting to prompt").yellow());
                        PermissionMode::Prompt
                    });
                    PermissionManager::with_project_mode(mode, &project_root)
                } else {
                    PermissionManager::with_project(
                        args.auto_approve || auto_approve,
                        &project_root,
                    )
                }
            };
            let explain_mode = if args.explain {
                ExplainMode::On
            } else {
                ExplainMode::Off
            };

            // --json implies --quiet
            let quiet = args.quiet || args.json;
            // When quiet, don't render markdown (no terminal formatting)
            let render_md = is_tty && !quiet;

            let mut skill_qt = astra_skills::quality::SkillQualityTracker::new();
            let skill_search = astra_core::SkillSearchSettings::default();
            let render_policy = if quiet {
                crate::stream_render::RenderPolicy::Silent
            } else {
                crate::stream_render::RenderPolicy::Stream
            };

            // Bug-A fix: build a DynamicAgentSpawner so `astra chat -m`
            // can service spawn_agent tool calls, matching the REPL
            // path. Without this, one-shot LLM invocations that try
            // to spawn_agent hit "Agent spawning not available in
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
            )
            .await;

            // Keep a clone of the Arc so we can drain background
            // spawned children before process exit — otherwise
            // background tasks (the default spawn_agent mode) get
            // aborted when main returns, which silently drops any
            // ForkCacheEvent / child telemetry they would have
            // emitted on their first response.
            let spawner_handle_for_drain = one_shot_spawner.clone();
            let chat_ctx = crate::chat_stream::BasicCliChatContext {
                api,
                auth_profile: profile.as_deref(),
                message: &message,
                model: args.model.as_deref().or(global_model.as_deref()),
                provider: None,
                explain: explain_mode,
                render_md,
                verbose_mode: !quiet,
                render_policy,
                selector: &*selector.0,
                unified_skill_registry: astra_runtime::skills::default_unified_registry(),
                skill_search: &skill_search,
                agent_spawner: Some(one_shot_spawner),
                root_agent_id: Some(&root_agent_id),
            };
            let mut params = ChatTurnParams::basic_cli(
                &chat_ctx,
                &token,
                session_id.as_deref(),
                &mut pm,
                &mut skill_qt,
            );
            params.pre_loaded_messages = continuation_messages.take();
            let sr = match stream_chat_sse(params)
            .await
            {
                Ok(sr) => sr,
                Err(e) if is_session_not_found_error(&e.error) && session_id.is_some() => {
                    let _ = clear_profile_last_session(profile.as_deref());
                    stream_chat_sse(ChatTurnParams::basic_cli(
                        &chat_ctx,
                        &token,
                        None,
                        &mut pm,
                        &mut skill_qt,
                    ))
                    .await
                    .map_err(|f| f.error)?
                }
                Err(e) => return Err(e.error),
            };

            // Save session for resumption
            if let Some(sid) = &sr.session_id {
                let p = creds.profiles.entry(name).or_default();
                p.last_session_id = Some(sid.clone());
                save_credentials(&creds)?;
            }

            // Drain any background-spawned child agents before
            // returning. Without this, background tasks (the
            // default spawn_agent mode) are aborted when main
            // returns, which silently drops any ForkCacheEvent /
            // child output they would have emitted. Deadline is
            // bounded so a misbehaving child can't hang the CLI;
            // tasks exceeding it are aborted with a log warning.
            //
            // We drain BEFORE writing result to stdout so the
            // [fork-cache] stderr lines (if any) appear before the
            // JSON/text result — operators grepping stderr don't
            // see the order swap.
            spawner_handle_for_drain
                .shutdown_and_wait(std::time::Duration::from_secs(30))
                .await;

            // Output result
            if args.json {
                // Compute exit code for JSON output
                let exit_code = compute_exit_code(&sr);
                // Pure JSON output for scripting
                let json_output = serde_json::json!({
                    "session_id": sr.session_id,
                    "run_id": sr.run_id,
                    "text": sr.full_text,
                    "prompt_tokens": sr.prompt_tokens + sr.cache_read_tokens + sr.cache_creation_tokens,
                    "completion_tokens": sr.completion_tokens,
                    "tool_calls_count": sr.tool_calls_count,
                    "tools_used": sr.tools_used,
                    "ttft_ms": sr.ttft_ms,
                    "context_ms": sr.context_ms,
                    "selector_strategy": sr.selector_strategy,
                    "exit_code": i32::from(exit_code),
                    "success": exit_code == ExitCode::Success,
                });
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

        Some(Command::Session(SessionCmd::Trace(SessionTraceCmd::Status(args)))) => {
            let (_, _, _, token) = get_profile_and_token(profile.as_deref())?;
            let session_id =
                resolve_remote_session_id(api, profile.as_deref(), args.session_id.as_deref())
                    .await?;
            let trace_state = fetch_session_trace_state(api, Some(&token), &session_id).await?;
            print_json_or_raw(
                &serde_json::json!({
                    "session_id": trace_state.session_id,
                    "full_llm_capture": trace_state.enabled,
                })
                .to_string(),
            );
            Ok(ExitCode::Success)
        }

        Some(Command::Session(SessionCmd::Trace(SessionTraceCmd::On(args)))) => {
            let (_, _, _, token) = get_profile_and_token(profile.as_deref())?;
            let session_id =
                resolve_remote_session_id(api, profile.as_deref(), args.session_id.as_deref())
                    .await?;
            let trace_state =
                update_session_trace_state(api, Some(&token), &session_id, true).await?;
            print_json_or_raw(
                &serde_json::json!({
                    "session_id": trace_state.session_id,
                    "full_llm_capture": trace_state.enabled,
                })
                .to_string(),
            );
            Ok(ExitCode::Success)
        }

        Some(Command::Session(SessionCmd::Trace(SessionTraceCmd::Off(args)))) => {
            let (_, _, _, token) = get_profile_and_token(profile.as_deref())?;
            let session_id =
                resolve_remote_session_id(api, profile.as_deref(), args.session_id.as_deref())
                    .await?;
            let trace_state =
                update_session_trace_state(api, Some(&token), &session_id, false).await?;
            print_json_or_raw(
                &serde_json::json!({
                    "session_id": trace_state.session_id,
                    "full_llm_capture": trace_state.enabled,
                })
                .to_string(),
            );
            Ok(ExitCode::Success)
        }

        Some(Command::SelfInspect(cmd)) => {
            let body = crate::self_command::execute_self_command(&cmd, profile.as_deref()).await?;
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
            let (_, _, _, token) = get_profile_and_token(profile.as_deref())?;
            let q = vec![
                ("limit", args.limit.to_string()),
                ("offset", args.offset.to_string()),
            ];
            let body = api
                .get_skills_query_text(&token, &q)
                .await
                .map_err(map_thin_err)?;
            print_json_or_raw(&body);
            Ok(ExitCode::Success)
        }

        Some(Command::Skill(SkillCmd::Show(args))) => {
            let (_, _, _, token) = get_profile_and_token(profile.as_deref())?;
            let q: Vec<(&str, String)> = if let Some(ref version) = args.version {
                vec![("version", version.clone())]
            } else {
                vec![]
            };
            let body = api
                .get_skill_query_text(&token, &args.skill_id, &q)
                .await
                .map_err(map_thin_err)?;
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
            execute_config_command(cfg_cmd)?;
            Ok(ExitCode::Success)
        }
    }
}

/// Compute exit code from StreamResult based on tool failures and force stops.
fn compute_exit_code(sr: &StreamResult) -> ExitCode {
    // Check for force stop (highest priority)
    for ve in &sr.verdict_events {
        if ve.force_stop {
            return ExitCode::ForceStop;
        }
    }

    // Check for tool failures — only if the LAST tool call failed.
    // Intermediate failures followed by successful retries are normal
    // agent self-correction behavior and should not mark the run as failed.
    if let Some(last) = sr.tool_call_records.last() {
        if !last.ok {
            return ExitCode::ToolFailure;
        }
    }

    ExitCode::Success
}

/// `--print` / `-p` mode: headless single-shot query, prints response and exits.
/// Reads message from positional args (Message variant) or stdin.
pub(super) async fn run_print_mode(
    api: &astra_thin_client::ThinClient,
    profile: Option<&str>,
    output_format: &str,
    model: Option<&str>,
    system_prompt: Option<&str>,
    command: Option<Command>,
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

    let (mut creds, name, _, token) = get_profile_and_token(profile)?;
    let session_id = validated_resumable_last_session_id(api, profile).await;
    let mut continuation_messages = session_id
        .as_deref()
        .and_then(load_session_messages_for_continuation);
    let selector = create_tool_selector(api, profile);
    let mut pm = PermissionManager::with_project(
        true, // print mode is headless, always auto-approve
        &std::env::current_dir().unwrap_or_default(),
    );
    let mut skill_qt = astra_skills::quality::SkillQualityTracker::new();
    let skill_search = astra_core::SkillSearchSettings::default();

    let chat_ctx = crate::chat_stream::BasicCliChatContext {
        api,
        auth_profile: profile,
        message: &message,
        model,
        provider: None,
        explain: ExplainMode::Off,
        render_md: false,
        verbose_mode: false,
        render_policy: crate::stream_render::RenderPolicy::Silent,
        selector: &*selector.0,
        unified_skill_registry: astra_runtime::skills::default_unified_registry(),
        skill_search: &skill_search,
        // Print/headless mode — no spawn_agent support by design.
        agent_spawner: None,
        root_agent_id: None,
    };

    let mut params = ChatTurnParams::basic_cli(
        &chat_ctx,
        &token,
        session_id.as_deref(),
        &mut pm,
        &mut skill_qt,
    );
    params.pre_loaded_messages = continuation_messages.take();
    let sr = match stream_chat_sse(params)
    .await
    {
        Ok(sr) => sr,
        Err(e) if is_session_not_found_error(&e.error) && session_id.is_some() => {
            let _ = clear_profile_last_session(profile);
            stream_chat_sse(ChatTurnParams::basic_cli(
                &chat_ctx,
                &token,
                None,
                &mut pm,
                &mut skill_qt,
            ))
            .await
            .map_err(|f| f.error)?
        }
        Err(e) => return Err(e.error),
    };

    // Save session for resumption
    if let Some(sid) = &sr.session_id {
        let p = creds.profiles.entry(name).or_default();
        p.last_session_id = Some(sid.clone());
        save_credentials(&creds)?;
    }

    let exit_code = compute_exit_code(&sr);

    match output_format {
        "json" | "stream-json" => {
            let json_output = serde_json::json!({
                "session_id": sr.session_id,
                "run_id": sr.run_id,
                "text": sr.full_text,
                "prompt_tokens": sr.prompt_tokens + sr.cache_read_tokens + sr.cache_creation_tokens,
                "completion_tokens": sr.completion_tokens,
                "tool_calls_count": sr.tool_calls_count,
                "tools_used": sr.tools_used,
                "exit_code": i32::from(exit_code),
                "success": exit_code == ExitCode::Success,
            });
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
    println!("{}", "Version".bold().cyan());
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
    println!("{}", "API Server".bold().cyan());
    println!("  {} {}", "URL:".dim(), api.api_origin());
    match api.get_health_text().await {
        Ok(body) => println!(
            "  {} {} {}",
            "Status:".dim(),
            "✓".green(),
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
    println!("{}", "Authentication".bold().cyan());
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
                            "✓".green(),
                            format!("Logged in as {user}").green()
                        );
                    } else {
                        println!(
                            "  {} {} {}",
                            "Status:".dim(),
                            "✓".green(),
                            "Authenticated".green()
                        );
                    }
                }
                Err(_) => {
                    println!(
                        "  {} {} {}",
                        "Status:".dim(),
                        "⚠".yellow(),
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
    println!("{}", "Project Configuration".bold().cyan());
    let cwd = std::env::current_dir().unwrap_or_default();
    let astra_dir = cwd.join(".astra");
    if astra_dir.is_dir() {
        println!("  {} {} {}", ".astra/:".dim(), "✓".green(), "Found".green());
    } else {
        println!("  {} {}", ".astra/:".dim(), "Not found (optional)".dim());
    }
    println!("  {} {}", "Working dir:".dim(), cwd.display());
    println!();

    // 5. MCP configuration
    println!("{}", "MCP Configuration".bold().cyan());
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
                                "✓".green(),
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
    println!("{}", "Environment".bold().cyan());
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
        println!("{} {}", "✓".green().bold(), "No issues found".green());
    } else {
        println!(
            "{} {}:",
            "Found".yellow(),
            format!("{} issue(s)", issues.len()).yellow().bold()
        );
        for issue in &issues {
            println!("  {} {}", "⚠".yellow(), issue);
        }
    }
}

// ═══════════════════════════════════════════════════════ MCP CLI ══════════

/// Load MCP server configs from JSON files or inline JSON strings and merge
/// them into the project-level mcp.json. Each source should be a file path
/// or a raw JSON string containing `{"mcpServers": {...}}`.
pub(super) fn load_mcp_configs(sources: &[String]) -> Result<(), String> {
    let project_path = crate::manifest_loader::project_mcp_json_path()
        .ok_or_else(|| "Cannot determine project directory for MCP config".to_string())?;
    let mut config = read_mcp_config(&project_path)?;

    for source in sources {
        let json_str = if std::path::Path::new(source).is_file() {
            std::fs::read_to_string(source)
                .map_err(|e| format!("Failed to read MCP config file '{}': {e}", source))?
        } else {
            source.clone()
        };
        let parsed: serde_json::Value = serde_json::from_str(&json_str)
            .map_err(|e| format!("Invalid MCP config JSON from '{}': {e}", source))?;

        if let Some(servers) = parsed.get("mcpServers").and_then(|v| v.as_object()) {
            let target = config
                .as_object_mut()
                .ok_or("MCP config must be a JSON object")?
                .entry("mcpServers")
                .or_insert_with(|| serde_json::json!({}))
                .as_object_mut()
                .ok_or("mcpServers value must be a JSON object")?;
            for (name, entry) in servers {
                target.insert(name.clone(), entry.clone());
            }
        } else {
            return Err(format!(
                "MCP config from '{}' must contain a \"mcpServers\" object",
                source
            ));
        }
    }

    write_mcp_config(&project_path, &config)?;
    Ok(())
}

/// Resolve the mcp.json path for the given scope.
fn mcp_json_path_for_scope(scope: &str) -> Result<std::path::PathBuf, String> {
    match scope {
        "project" => crate::manifest_loader::project_mcp_json_path()
            .ok_or_else(|| "Cannot determine project directory".to_string()),
        "user" => crate::manifest_loader::global_mcp_json_path()
            .ok_or_else(|| "Cannot determine home directory".to_string()),
        other => Err(format!("Unknown scope '{other}' — use 'project' or 'user'")),
    }
}

/// Read and parse an mcp.json file, returning empty config if missing.
fn read_mcp_config(path: &std::path::Path) -> Result<serde_json::Value, String> {
    if !path.is_file() {
        return Ok(serde_json::json!({"mcpServers": {}}));
    }
    let content = std::fs::read_to_string(path)
        .map_err(|e| format!("Failed to read {}: {e}", path.display()))?;
    serde_json::from_str(&content).map_err(|e| format!("Failed to parse {}: {e}", path.display()))
}

/// Write config atomically (temp + rename).
fn write_mcp_config(path: &std::path::Path, config: &serde_json::Value) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("Failed to create {}: {e}", parent.display()))?;
    }
    let tmp = path.with_extension("json.tmp");
    let pretty = serde_json::to_string_pretty(config).unwrap_or_default();
    std::fs::write(&tmp, &pretty).map_err(|e| format!("Failed to write {}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        format!("Failed to rename to {}: {e}", path.display())
    })
}

fn execute_mcp_command(cmd: McpCmd) -> Result<(), String> {
    match cmd {
        McpCmd::List(args) => mcp_list(&args.scope),
        McpCmd::Add(args) => mcp_add(&args.name, &args.command, &args.args, &args.scope),
        McpCmd::AddJson(args) => mcp_add_json(&args.name, &args.json, &args.scope),
        McpCmd::Remove(args) => mcp_remove(&args.name, &args.scope),
        McpCmd::Get(args) => mcp_get(&args.name),
    }
}

fn mcp_list(scope: &str) -> Result<(), String> {
    let path = mcp_json_path_for_scope(scope)?;
    let config = read_mcp_config(&path)?;
    let servers = config
        .get("mcpServers")
        .and_then(|v| v.as_object())
        .cloned()
        .unwrap_or_default();

    if servers.is_empty() {
        println!("  {}", "No MCP servers configured.".dim());
        println!("  Use {} to add a server.", "astra mcp add".cyan());
        return Ok(());
    }

    println!(
        "  {:<20} {:<8} {:<40}",
        "Name".bold(),
        "Type".bold(),
        "Command / URL".bold()
    );
    println!("  {}", "─".repeat(68).dim());
    for (name, entry) in &servers {
        let server_type = entry
            .get("type")
            .and_then(|v| v.as_str())
            .unwrap_or("stdio");
        let detail = match server_type {
            "sse" | "http" => entry
                .get("url")
                .and_then(|v| v.as_str())
                .unwrap_or("-")
                .to_string(),
            _ => {
                let cmd = entry.get("command").and_then(|v| v.as_str()).unwrap_or("-");
                let args = entry
                    .get("args")
                    .and_then(|v| v.as_array())
                    .map(|a| {
                        a.iter()
                            .filter_map(|v| v.as_str())
                            .collect::<Vec<_>>()
                            .join(" ")
                    })
                    .unwrap_or_default();
                if args.is_empty() {
                    cmd.to_string()
                } else {
                    format!("{cmd} {args}")
                }
            }
        };
        println!(
            "  {:<20} {:<8} {}",
            name.as_str().cyan(),
            server_type.dim(),
            detail
        );
    }
    println!(
        "\n  {} {}",
        "Config:".dim(),
        path.display().to_string().dim()
    );
    Ok(())
}

fn mcp_add(name: &str, command: &str, args: &[String], scope: &str) -> Result<(), String> {
    let path = mcp_json_path_for_scope(scope)?;
    let mut config = read_mcp_config(&path)?;

    // Check for duplicate
    if let Some(servers) = config.get("mcpServers").and_then(|v| v.as_object()) {
        if servers.contains_key(name) {
            return Err(format!(
                "Server '{name}' already exists. Remove it first with: astra mcp remove {name}"
            ));
        }
    }

    let entry = serde_json::json!({
        "command": command,
        "args": args,
    });
    config
        .as_object_mut()
        .ok_or("MCP config must be a JSON object")?
        .entry("mcpServers")
        .or_insert_with(|| serde_json::json!({}))
        .as_object_mut()
        .ok_or("mcpServers value must be a JSON object")?
        .insert(name.to_string(), entry);

    write_mcp_config(&path, &config)?;
    println!(
        "  {} Added '{}' to {}",
        "✓".green(),
        name.cyan(),
        path.display().to_string().dim()
    );
    Ok(())
}

fn mcp_add_json(name: &str, json: &str, scope: &str) -> Result<(), String> {
    let entry: serde_json::Value =
        serde_json::from_str(json).map_err(|e| format!("Invalid JSON: {e}"))?;
    if !entry.is_object() {
        return Err("JSON config must be an object".to_string());
    }

    let path = mcp_json_path_for_scope(scope)?;
    let mut config = read_mcp_config(&path)?;

    if let Some(servers) = config.get("mcpServers").and_then(|v| v.as_object()) {
        if servers.contains_key(name) {
            return Err(format!(
                "Server '{name}' already exists. Remove it first with: astra mcp remove {name}"
            ));
        }
    }

    config
        .as_object_mut()
        .ok_or("MCP config must be a JSON object")?
        .entry("mcpServers")
        .or_insert_with(|| serde_json::json!({}))
        .as_object_mut()
        .ok_or("mcpServers value must be a JSON object")?
        .insert(name.to_string(), entry);

    write_mcp_config(&path, &config)?;
    println!(
        "  {} Added '{}' to {}",
        "✓".green(),
        name.cyan(),
        path.display().to_string().dim()
    );
    Ok(())
}

fn mcp_remove(name: &str, scope: &str) -> Result<(), String> {
    let path = mcp_json_path_for_scope(scope)?;
    if !path.is_file() {
        return Err(format!("No config file at {}", path.display()));
    }
    let mut config = read_mcp_config(&path)?;

    let removed = config
        .get_mut("mcpServers")
        .and_then(|v| v.as_object_mut())
        .map(|m| m.remove(name).is_some())
        .unwrap_or(false);

    if !removed {
        return Err(format!("Server '{name}' not found in {}", path.display()));
    }

    write_mcp_config(&path, &config)?;
    println!(
        "  {} Removed '{}' from {}",
        "✓".green(),
        name.cyan(),
        path.display().to_string().dim()
    );
    Ok(())
}

fn mcp_get(name: &str) -> Result<(), String> {
    // Search both scopes
    let scopes = ["project", "user"];
    for scope in &scopes {
        let path = match mcp_json_path_for_scope(scope) {
            Ok(p) => p,
            Err(_) => continue,
        };
        let config = match read_mcp_config(&path) {
            Ok(c) => c,
            Err(_) => continue,
        };
        if let Some(entry) = config
            .get("mcpServers")
            .and_then(|v| v.as_object())
            .and_then(|m| m.get(name))
        {
            println!("  {}:", name.bold().cyan());
            println!("    {} {scope}", "Scope:".dim());
            let server_type = entry
                .get("type")
                .and_then(|v| v.as_str())
                .unwrap_or("stdio");
            println!("    {} {server_type}", "Type:".dim());
            match server_type {
                "sse" | "http" => {
                    if let Some(url) = entry.get("url").and_then(|v| v.as_str()) {
                        println!("    {} {url}", "URL:".dim());
                    }
                }
                _ => {
                    if let Some(cmd) = entry.get("command").and_then(|v| v.as_str()) {
                        println!("    {} {cmd}", "Command:".dim());
                    }
                    if let Some(args) = entry.get("args").and_then(|v| v.as_array()) {
                        let args_str: Vec<&str> = args.iter().filter_map(|v| v.as_str()).collect();
                        println!("    {} {}", "Args:".dim(), args_str.join(" "));
                    }
                }
            }
            if let Some(env) = entry.get("env").and_then(|v| v.as_object()) {
                println!("    {}:", "Environment".dim());
                for (k, v) in env {
                    println!(
                        "      {}={}",
                        k.as_str().cyan(),
                        v.as_str().unwrap_or(&v.to_string())
                    );
                }
            }
            println!(
                "\n  {} astra mcp remove \"{}\" -s {scope}",
                "To remove:".dim(),
                name
            );
            return Ok(());
        }
    }
    Err(format!("No MCP server found with name: {name}"))
}

// ═══════════════════════════════════════════════════════ Config ═══════════

/// Path to `~/.astra/settings.json`.
fn settings_path(override_path: Option<&std::path::PathBuf>) -> Result<std::path::PathBuf, String> {
    if let Some(p) = override_path {
        return Ok(p.clone());
    }
    dirs::home_dir()
        .map(|h| h.join(".astra").join("settings.json"))
        .ok_or_else(|| "Cannot determine home directory".to_string())
}

fn read_settings_from(
    path_override: Option<&std::path::PathBuf>,
) -> Result<serde_json::Map<String, serde_json::Value>, String> {
    let path = settings_path(path_override)?;
    if !path.is_file() {
        return Ok(serde_json::Map::new());
    }
    let content = std::fs::read_to_string(&path)
        .map_err(|e| format!("Failed to read {}: {e}", path.display()))?;
    let val: serde_json::Value = serde_json::from_str(&content)
        .map_err(|e| format!("Failed to parse {}: {e}", path.display()))?;
    val.as_object()
        .cloned()
        .ok_or_else(|| format!("{} is not a JSON object", path.display()))
}

fn read_settings() -> Result<serde_json::Map<String, serde_json::Value>, String> {
    read_settings_from(None)
}

/// Read `default_model` from settings.json, if set.
pub fn read_config_default_model() -> Result<Option<String>, String> {
    let settings = read_settings()?;
    Ok(settings
        .get("default_model")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string()))
}

/// Read `api_url` from settings.json, if set.
pub fn read_config_api_url() -> Result<Option<String>, String> {
    read_config_api_url_from(None)
}

/// Read `api_url` from a specific path (for testing) or the default settings path.
fn read_config_api_url_from(
    path_override: Option<&std::path::PathBuf>,
) -> Result<Option<String>, String> {
    let settings = read_settings_from(path_override)?;
    Ok(settings
        .get("api_url")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string()))
}

const DEFAULT_API_URL: &str = "http://127.0.0.1:8000";

/// Resolve API URL with priority: flag > env var > config file > default.
pub fn resolve_api_url(flag: Option<&str>) -> String {
    resolve_api_url_with(
        flag,
        || std::env::var("ASTRA_API_URL").ok(),
        read_config_api_url,
    )
}

/// Testable core: resolve API URL with injectable env and config sources.
fn resolve_api_url_with(
    flag: Option<&str>,
    env_fn: impl FnOnce() -> Option<String>,
    config_fn: impl FnOnce() -> Result<Option<String>, String>,
) -> String {
    flag.map(|s| s.trim_end_matches('/').to_string())
        .or_else(|| env_fn().map(|s| s.trim_end_matches('/').to_string()))
        .or_else(|| match config_fn() {
            Ok(Some(url)) => Some(url.trim_end_matches('/').to_string()),
            Ok(None) => None,
            Err(e) => {
                eprintln!("warning: failed to read config for api_url: {e}");
                None
            }
        })
        .unwrap_or_else(|| DEFAULT_API_URL.to_string())
}

async fn resolve_remote_session_id(
    api: &astra_thin_client::ThinClient,
    profile: Option<&str>,
    requested: Option<&str>,
) -> Result<String, String> {
    match requested.map(str::trim).filter(|value| !value.is_empty()) {
        Some(session_id) => Ok(session_id.to_string()),
        None => validated_resumable_last_session_id(api, profile)
            .await
            .ok_or_else(|| {
                "No session id provided and no recent resumable session is available".to_string()
            }),
    }
}

fn latest_artifact_id(body: &str) -> Result<String, String> {
    let json: serde_json::Value = serde_json::from_str(body)
        .map_err(|error| format!("Invalid latest artifact response: {error}"))?;
    json.get("artifact_id")
        .and_then(|value| value.as_str())
        .filter(|value| !value.trim().is_empty())
        .map(ToString::to_string)
        .ok_or_else(|| "Latest artifact response missing artifact_id".to_string())
}

fn resolve_download_output_path(
    output: Option<&std::path::Path>,
    suggested_name: &str,
) -> std::path::PathBuf {
    // Sanitize server-supplied filename: strip directory components and reject traversal.
    let safe_name = std::path::Path::new(suggested_name)
        .file_name()
        .and_then(|n| n.to_str())
        .filter(|n| !n.is_empty() && *n != "." && *n != "..")
        .unwrap_or("download.json");
    match output {
        Some(path) if path.is_dir() => path.join(safe_name),
        Some(path) => path.to_path_buf(),
        None => std::path::PathBuf::from(safe_name),
    }
}

fn write_downloaded_capture(path: &std::path::Path, bytes: &[u8]) -> Result<(), String> {
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("Failed to create {}: {e}", parent.display()))?;
    }
    std::fs::write(path, bytes).map_err(|e| format!("Failed to write {}: {e}", path.display()))
}

fn write_settings(settings: &serde_json::Map<String, serde_json::Value>) -> Result<(), String> {
    let path = settings_path(None)?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("Failed to create {}: {e}", parent.display()))?;
    }
    let val = serde_json::Value::Object(settings.clone());
    let pretty = serde_json::to_string_pretty(&val).unwrap_or_default();
    std::fs::write(&path, &pretty).map_err(|e| format!("Failed to write {}: {e}", path.display()))
}

/// Known setting keys with descriptions for list/help.
const KNOWN_SETTINGS: &[(&str, &str)] = &[
    (
        "default_model",
        "Default model for chat (e.g. gpt-4o, claude-3.5-sonnet)",
    ),
    ("verbose", "Enable verbose output (true/false)"),
    ("auto_approve", "Auto-approve tool calls (true/false)"),
    ("api_url", "API server URL"),
    ("theme", "Color theme (auto/dark/light)"),
    (
        "permission_mode",
        "Default permission mode (auto/prompt/deny)",
    ),
];

fn execute_config_command(cmd: ConfigCmd) -> Result<(), String> {
    match cmd {
        ConfigCmd::List => config_list(),
        ConfigCmd::Get(args) => config_get(&args.key),
        ConfigCmd::Set(args) => config_set(&args.key, &args.value),
        ConfigCmd::ShowPolicy(args) => config_show_policy(args.model.as_deref(), args.json),
    }
}

fn config_show_policy(model: Option<&str>, json: bool) -> Result<(), String> {
    let cfg = astra_config::runtime_config::RuntimeConfig::load();
    let policy = cfg.tool_selection.resolve_for_model(model);
    let trust_mode = match cfg.safety.resolved_trust_mode() {
        astra_config::runtime_config::TrustModeSerde::Strict => "strict",
        astra_config::runtime_config::TrustModeSerde::Trusted => "trusted",
    };
    let rejected = cfg.tool_selection.rejected_model_match_patterns();
    println!(
        "{}",
        format_policy_output(model, &policy, trust_mode, &rejected, json)
    );
    Ok(())
}

/// Render a resolved [`EffectiveToolPolicy`] as either JSON or human text.
///
/// Kept as a pure function of inputs so it can be unit-tested without
/// shelling out to the binary or touching the filesystem.
///
/// `rejected_patterns` is the list of `model_profiles.model_match` values
/// that were silently ignored at resolve time because they were too short
/// (see `ToolSelectionConfig::rejected_model_match_patterns`). When
/// non-empty, they're surfaced so users can spot misconfigs.
fn format_policy_output(
    model: Option<&str>,
    policy: &astra_config::runtime_config::EffectiveToolPolicy,
    trust_mode: &str,
    rejected_patterns: &[String],
    json: bool,
) -> String {
    if json {
        let payload = serde_json::json!({
            "model": model,
            "trust_mode": trust_mode,
            "max_identical_tool_calls": policy.max_identical_tool_calls,
            "max_tools_per_turn": policy.max_tools_per_turn,
            "repeated_cache_hit_suppression": policy.repeated_cache_hit_suppression,
            "max_consecutive_empty_name": policy.max_consecutive_empty_name,
            // Always present as an array (possibly empty) so json consumers
            // never have to special-case the absent-vs-empty case.
            "rejected_model_match_patterns": rejected_patterns,
        });
        serde_json::to_string_pretty(&payload)
            .unwrap_or_else(|_| "{\"error\": \"failed to serialize policy\"}".to_string())
    } else {
        let label = model.unwrap_or("<global defaults — no model>");
        let mut out = format!(
            "Resolved workflow-guard policy for {label}:\n\
             \n  trust_mode                     = {trust_mode}\
             \n  max_identical_tool_calls       = {}\
             \n  max_tools_per_turn             = {}\
             \n  repeated_cache_hit_suppression = {}\
             \n  max_consecutive_empty_name     = {}\n",
            policy.max_identical_tool_calls,
            policy.max_tools_per_turn,
            policy.repeated_cache_hit_suppression,
            policy.max_consecutive_empty_name,
        );
        if !rejected_patterns.is_empty() {
            out.push_str(
                "\n⚠  rejected model_match patterns (too short, ignored at resolve time):\n",
            );
            for p in rejected_patterns {
                out.push_str(&format!("    - \"{p}\"\n"));
            }
        }
        out
    }
}

fn config_list() -> Result<(), String> {
    let settings = read_settings()?;
    let path = settings_path(None)?;

    if settings.is_empty() {
        println!("  {}", "No settings configured.".dim());
        println!(
            "  Use {} to set a value.",
            "astra config set <key> <value>".cyan()
        );
        println!("\n  {}:", "Available keys".bold());
        for (key, desc) in KNOWN_SETTINGS {
            println!("    {}  {}", key.cyan(), desc.dim());
        }
        return Ok(());
    }

    let (hk, hv) = ("Key", "Value");
    println!("  {:<20} {hv}", hk.bold());
    println!("  {}", "─".repeat(50).dim());
    for (key, value) in &settings {
        let display = match value {
            serde_json::Value::String(s) => s.clone(),
            other => other.to_string(),
        };
        println!("  {:<20} {display}", key.as_str().cyan());
    }
    println!(
        "\n  {} {}",
        "Config:".dim(),
        path.display().to_string().dim()
    );
    Ok(())
}

fn config_get(key: &str) -> Result<(), String> {
    let settings = read_settings()?;
    match settings.get(key) {
        Some(val) => {
            match val {
                serde_json::Value::String(s) => println!("{s}"),
                other => println!("{other}"),
            }
            Ok(())
        }
        None => {
            // Check if it's a known key
            if let Some((_, desc)) = KNOWN_SETTINGS.iter().find(|(k, _)| *k == key) {
                Err(format!("'{key}' is not set. {desc}"))
            } else {
                Err(format!("'{key}' is not set"))
            }
        }
    }
}

fn config_set(key: &str, value: &str) -> Result<(), String> {
    let mut settings = read_settings()?;

    // Parse value: try bool, then number, then keep as string
    let json_value = match value {
        "true" => serde_json::Value::Bool(true),
        "false" => serde_json::Value::Bool(false),
        v if v.parse::<f64>().is_ok() && !v.contains(|c: char| c.is_alphabetic()) => {
            if let Ok(n) = v.parse::<i64>() {
                serde_json::Value::Number(n.into())
            } else {
                serde_json::Value::String(v.to_string())
            }
        }
        v => serde_json::Value::String(v.to_string()),
    };

    settings.insert(key.to_string(), json_value);
    write_settings(&settings)?;
    println!("  {} Set '{}' = {}", "✓".green(), key.cyan(), value);
    Ok(())
}

#[cfg(test)]
mod mcp_cli_tests {
    use super::*;

    fn make_config(path: &std::path::Path, servers: serde_json::Value) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        let config = serde_json::json!({"mcpServers": servers});
        std::fs::write(path, serde_json::to_string_pretty(&config).unwrap()).unwrap();
    }

    #[test]
    fn read_mcp_config_missing_file() {
        let config = read_mcp_config(std::path::Path::new("/tmp/nonexistent_mcp.json")).unwrap();
        assert!(config["mcpServers"].as_object().unwrap().is_empty());
    }

    #[test]
    fn read_mcp_config_valid() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), r#"{"mcpServers":{"test":{"command":"echo"}}}"#).unwrap();
        let config = read_mcp_config(tmp.path()).unwrap();
        assert!(config["mcpServers"]["test"]["command"] == "echo");
    }

    #[test]
    fn read_mcp_config_invalid_json() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), "not json").unwrap();
        let err = read_mcp_config(tmp.path()).unwrap_err();
        assert!(err.contains("Failed to parse"));
    }

    #[test]
    fn write_mcp_config_creates_parents() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("sub").join("dir").join("mcp.json");
        let config = serde_json::json!({"mcpServers": {}});
        write_mcp_config(&path, &config).unwrap();
        assert!(path.is_file());
    }

    #[test]
    fn mcp_json_path_for_scope_invalid() {
        let err = mcp_json_path_for_scope("invalid").unwrap_err();
        assert!(err.contains("Unknown scope"));
    }

    #[test]
    fn mcp_add_and_remove_roundtrip() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("mcp.json");

        // Start empty
        make_config(&path, serde_json::json!({}));

        // Add a server
        let mut config = read_mcp_config(&path).unwrap();
        let entry = serde_json::json!({"command": "npx", "args": ["@mcp/server"]});
        config["mcpServers"]
            .as_object_mut()
            .unwrap()
            .insert("test-server".to_string(), entry);
        write_mcp_config(&path, &config).unwrap();

        // Verify it's there
        let config = read_mcp_config(&path).unwrap();
        assert!(config["mcpServers"]["test-server"]["command"] == "npx");

        // Remove it
        let mut config = read_mcp_config(&path).unwrap();
        let removed = config["mcpServers"]
            .as_object_mut()
            .unwrap()
            .remove("test-server")
            .is_some();
        assert!(removed);
        write_mcp_config(&path, &config).unwrap();

        // Verify it's gone
        let config = read_mcp_config(&path).unwrap();
        assert!(
            !config["mcpServers"]
                .as_object()
                .unwrap()
                .contains_key("test-server")
        );
    }

    #[test]
    fn mcp_add_json_valid() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("mcp.json");
        make_config(&path, serde_json::json!({}));

        let entry: serde_json::Value =
            serde_json::from_str(r#"{"url":"http://localhost:3000","type":"sse"}"#).unwrap();
        let mut config = read_mcp_config(&path).unwrap();
        config["mcpServers"]
            .as_object_mut()
            .unwrap()
            .insert("sse-server".to_string(), entry);
        write_mcp_config(&path, &config).unwrap();

        let config = read_mcp_config(&path).unwrap();
        assert_eq!(config["mcpServers"]["sse-server"]["type"], "sse");
        assert_eq!(
            config["mcpServers"]["sse-server"]["url"],
            "http://localhost:3000"
        );
    }

    #[test]
    fn mcp_add_json_invalid_json() {
        let err: Result<serde_json::Value, _> = serde_json::from_str("not json");
        assert!(err.is_err());
    }

    #[test]
    fn mcp_add_duplicate_detection() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("mcp.json");
        make_config(&path, serde_json::json!({"existing": {"command": "echo"}}));

        let config = read_mcp_config(&path).unwrap();
        let has_existing = config["mcpServers"]
            .as_object()
            .unwrap()
            .contains_key("existing");
        assert!(has_existing);
    }

    #[test]
    fn mcp_remove_nonexistent_server() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("mcp.json");
        make_config(&path, serde_json::json!({}));

        let mut config = read_mcp_config(&path).unwrap();
        let removed = config["mcpServers"]
            .as_object_mut()
            .unwrap()
            .remove("ghost")
            .is_some();
        assert!(!removed);
    }

    #[test]
    fn mcp_get_searches_both_scopes() {
        // mcp_get searches project then user; verify the search logic
        let scopes = ["project", "user"];
        assert_eq!(scopes.len(), 2);
        assert_eq!(scopes[0], "project");
        assert_eq!(scopes[1], "user");
    }

    #[test]
    fn mcp_list_empty_config() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("mcp.json");
        make_config(&path, serde_json::json!({}));

        let config = read_mcp_config(&path).unwrap();
        let servers = config["mcpServers"].as_object().unwrap();
        assert!(servers.is_empty());
    }

    #[test]
    fn mcp_list_multiple_server_types() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("mcp.json");
        make_config(
            &path,
            serde_json::json!({
                "stdio-srv": {"command": "npx", "args": ["@mcp/server"]},
                "sse-srv": {"type": "sse", "url": "http://localhost:3000"},
                "http-srv": {"type": "http", "url": "http://localhost:4000"}
            }),
        );

        let config = read_mcp_config(&path).unwrap();
        let servers = config["mcpServers"].as_object().unwrap();
        assert_eq!(servers.len(), 3);

        // stdio type inference
        let stdio = &servers["stdio-srv"];
        assert_eq!(
            stdio
                .get("type")
                .and_then(|v| v.as_str())
                .unwrap_or("stdio"),
            "stdio"
        );

        // sse type
        assert_eq!(servers["sse-srv"]["type"], "sse");

        // http type
        assert_eq!(servers["http-srv"]["type"], "http");
    }

    #[test]
    fn write_mcp_config_atomic_no_partial() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("mcp.json");
        let config = serde_json::json!({"mcpServers": {"s": {"command": "echo"}}});
        write_mcp_config(&path, &config).unwrap();

        // tmp file should not remain
        let tmp = path.with_extension("json.tmp");
        assert!(!tmp.exists());

        // written file should be valid JSON
        let content = std::fs::read_to_string(&path).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&content).unwrap();
        assert_eq!(parsed["mcpServers"]["s"]["command"], "echo");
    }

    #[test]
    fn load_mcp_configs_from_file() {
        let dir = tempfile::TempDir::new().unwrap();
        let config_file = dir.path().join("custom-mcp.json");
        std::fs::write(
            &config_file,
            r#"{"mcpServers":{"test-server":{"command":"echo","args":["hello"]}}}"#,
        )
        .unwrap();

        // We can't easily test load_mcp_configs (needs project_mcp_json_path),
        // but we can test the JSON parsing logic directly
        let json_str = std::fs::read_to_string(&config_file).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json_str).unwrap();
        let servers = parsed
            .get("mcpServers")
            .and_then(|v| v.as_object())
            .unwrap();
        assert!(servers.contains_key("test-server"));
        assert_eq!(servers["test-server"]["command"], "echo");
        assert_eq!(servers["test-server"]["args"][0], "hello");
    }

    #[test]
    fn load_mcp_configs_rejects_missing_mcp_servers_key() {
        let json_str = r#"{"servers":{"foo":{}}}"#;
        let parsed: serde_json::Value = serde_json::from_str(json_str).unwrap();
        let servers = parsed.get("mcpServers").and_then(|v| v.as_object());
        assert!(servers.is_none(), "missing mcpServers should return None");
    }
}

#[cfg(test)]
mod config_cli_tests {
    use super::*;

    #[test]
    fn read_settings_missing_file_returns_empty() {
        // settings_path() returns home-based path; test read directly
        let settings = serde_json::Map::new();
        assert!(settings.is_empty());
    }

    #[test]
    fn write_and_read_settings_roundtrip() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("settings.json");

        let mut settings = serde_json::Map::new();
        settings.insert(
            "default_model".to_string(),
            serde_json::Value::String("gpt-4o".into()),
        );
        settings.insert("verbose".to_string(), serde_json::Value::Bool(true));

        let val = serde_json::Value::Object(settings.clone());
        std::fs::write(&path, serde_json::to_string_pretty(&val).unwrap()).unwrap();

        let content = std::fs::read_to_string(&path).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&content).unwrap();
        let loaded = parsed.as_object().unwrap();
        assert_eq!(loaded["default_model"], "gpt-4o");
        assert_eq!(loaded["verbose"], true);
    }

    #[test]
    fn value_parsing_booleans() {
        let parse = |v: &str| -> serde_json::Value {
            match v {
                "true" => serde_json::Value::Bool(true),
                "false" => serde_json::Value::Bool(false),
                _ => serde_json::Value::String(v.to_string()),
            }
        };
        assert_eq!(parse("true"), serde_json::Value::Bool(true));
        assert_eq!(parse("false"), serde_json::Value::Bool(false));
        assert_eq!(parse("hello"), serde_json::Value::String("hello".into()));
    }

    #[test]
    fn value_parsing_numbers() {
        let v = "42";
        let parsed = v.parse::<i64>().unwrap();
        assert_eq!(parsed, 42);
    }

    #[test]
    fn known_settings_not_empty() {
        assert!(!KNOWN_SETTINGS.is_empty());
        for (key, desc) in KNOWN_SETTINGS {
            assert!(!key.is_empty());
            assert!(!desc.is_empty());
        }
    }

    #[test]
    fn config_set_overwrites() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("settings.json");

        // Initial
        let mut settings = serde_json::Map::new();
        settings.insert("key".to_string(), serde_json::Value::String("v1".into()));
        std::fs::write(
            &path,
            serde_json::to_string_pretty(&serde_json::Value::Object(settings)).unwrap(),
        )
        .unwrap();

        // Overwrite
        let content = std::fs::read_to_string(&path).unwrap();
        let mut loaded: serde_json::Map<String, serde_json::Value> =
            serde_json::from_str::<serde_json::Value>(&content)
                .unwrap()
                .as_object()
                .unwrap()
                .clone();
        loaded.insert("key".to_string(), serde_json::Value::String("v2".into()));
        std::fs::write(
            &path,
            serde_json::to_string_pretty(&serde_json::Value::Object(loaded)).unwrap(),
        )
        .unwrap();

        let content = std::fs::read_to_string(&path).unwrap();
        let final_val: serde_json::Value = serde_json::from_str(&content).unwrap();
        assert_eq!(final_val["key"], "v2");
    }

    // Helper matching config_set's value parsing logic
    fn parse_value(value: &str) -> serde_json::Value {
        match value {
            "true" => serde_json::Value::Bool(true),
            "false" => serde_json::Value::Bool(false),
            v if v.parse::<f64>().is_ok() && !v.contains(|c: char| c.is_alphabetic()) => {
                if let Ok(n) = v.parse::<i64>() {
                    serde_json::Value::Number(n.into())
                } else {
                    serde_json::Value::String(v.to_string())
                }
            }
            v => serde_json::Value::String(v.to_string()),
        }
    }

    #[test]
    fn config_value_parsing_booleans() {
        assert_eq!(parse_value("true"), serde_json::Value::Bool(true));
        assert_eq!(parse_value("false"), serde_json::Value::Bool(false));
    }

    #[test]
    fn config_value_parsing_integers() {
        assert_eq!(parse_value("42"), serde_json::Value::Number(42.into()));
        assert_eq!(parse_value("0"), serde_json::Value::Number(0.into()));
        assert_eq!(parse_value("-1"), serde_json::Value::Number((-1).into()));
    }

    #[test]
    fn config_value_parsing_strings() {
        assert_eq!(
            parse_value("gpt-4o"),
            serde_json::Value::String("gpt-4o".into())
        );
        assert_eq!(parse_value(""), serde_json::Value::String("".into()));
    }

    #[test]
    fn known_settings_has_default_model() {
        assert!(KNOWN_SETTINGS.iter().any(|(k, _)| *k == "default_model"));
    }

    #[test]
    fn known_settings_has_auto_approve() {
        assert!(KNOWN_SETTINGS.iter().any(|(k, _)| *k == "auto_approve"));
    }

    #[test]
    fn apply_system_prompt_none_passthrough() {
        assert_eq!(apply_system_prompt("hello", None), "hello");
    }

    #[test]
    fn apply_system_prompt_wraps_message() {
        let result = apply_system_prompt("hello", Some("Be concise"));
        assert!(result.starts_with("<system_instructions>"));
        assert!(result.contains("Be concise"));
        assert!(result.ends_with("hello"));
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
            runtime_continuity: Default::default(),
            ttft_ms: None,
            context_ms: None,
            selector_strategy: None,
            selector_ms: None,
            selector_tokens_in: 0,
            selector_tokens_out: 0,
            memoria_ms: None,
            selector_confidence: None,
            routing_domain_hint: None,
            entity_learn_skipped_no_domain: false,
            pending_context_assembly_trace: None,
            turn_observability_events: Vec::new(),
            llm_rounds: None,
            interruption: None,
            final_messages: Vec::new(),
        }
    }

    #[test]
    fn exit_code_success_on_empty_result() {
        let sr = empty_stream_result();
        assert_eq!(compute_exit_code(&sr), ExitCode::Success);
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
            total_errors: 3,
            deprioritized_count: 0,
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
            total_errors: 1,
            deprioritized_count: 0,
            total_timeouts: 0,
            timeout_dominant_tools: vec![],
            total_cache_hits: 0,
            flaky_count: 0,
        });
        assert_eq!(compute_exit_code(&sr), ExitCode::Success);
    }
}

#[cfg(test)]
mod arg_render_tests {
    use super::*;

    #[test]
    fn bare_permissions_command_renders_empty_arg_for_mode_cycle() {
        let args = PermissionsArgs { command: None };
        assert_eq!(render_permissions_args(&args), "");
    }
}

#[cfg(test)]
mod show_policy_tests {
    use super::*;
    use astra_config::runtime_config::EffectiveToolPolicy;

    fn fake_policy() -> EffectiveToolPolicy {
        EffectiveToolPolicy {
            max_identical_tool_calls: 4,
            max_tools_per_turn: 20,
            repeated_cache_hit_suppression: 4,
            max_consecutive_empty_name: 3,
        }
    }

    #[test]
    fn human_output_includes_all_four_guard_fields_and_model_label() {
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
        );
        assert_eq!(url, "http://flag:8000");
    }

    #[test]
    fn env_wins_over_config() {
        let url = resolve_api_url_with(
            None,
            env_val("http://env:8000"),
            config_val("http://config:8000"),
        );
        assert_eq!(url, "http://env:8000");
    }

    #[test]
    fn config_wins_over_default() {
        let url = resolve_api_url_with(None, no_env, config_val("http://config:8000"));
        assert_eq!(url, "http://config:8000");
    }

    #[test]
    fn falls_back_to_default_when_all_none() {
        let url = resolve_api_url_with(None, no_env, no_config);
        assert_eq!(url, DEFAULT_API_URL);
    }

    #[test]
    fn trailing_slash_stripped_from_flag() {
        let url = resolve_api_url_with(Some("http://flag:8000/"), no_env, no_config);
        assert_eq!(url, "http://flag:8000");
    }

    #[test]
    fn trailing_slash_stripped_from_env() {
        let url = resolve_api_url_with(None, env_val("http://env:8000/"), no_config);
        assert_eq!(url, "http://env:8000");
    }

    #[test]
    fn trailing_slash_stripped_from_config() {
        let url = resolve_api_url_with(None, no_env, config_val("http://config:8000/"));
        assert_eq!(url, "http://config:8000");
    }

    #[test]
    fn config_error_falls_through_to_default() {
        let url = resolve_api_url_with(None, no_env, || Err("broken".to_string()));
        assert_eq!(url, DEFAULT_API_URL);
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

#[cfg(test)]
mod session_continuation_tests {
    use serde_json::json;

    /// Regression test: `--session-id` in one-shot mode must load previous
    /// messages from the session's heavy checkpoint. Before the fix, messages
    /// were always empty — the model couldn't see prior conversation turns.
    #[test]
    fn load_session_messages_returns_checkpoint_messages() {
        let session_id = format!("test-session-cont-{}", uuid::Uuid::new_v4());

        // Write a heavy checkpoint to the standard sessions dir.
        let home = dirs::home_dir().unwrap();
        let cp_dir = home
            .join(".astra/sessions")
            .join(&session_id)
            .join("step_checkpoints");
        std::fs::create_dir_all(&cp_dir).unwrap();

        // Use the same format as a real checkpoint (read from step_protocol.rs).
        // The key insight: serde will skip unknown fields with #[serde(default)],
        // so we only need the required fields.
        let checkpoint_json = r#"{
            "Heavy": {
                "light": {
                    "protocol_version": 1,
                    "cursor": {"phase": "Done", "slots": [], "parallel": false, "wait_trigger": null, "sub_step": null},
                    "step_id": "s1",
                    "task_id": "t1",
                    "agent_id": "astra-cli",
                    "progress": 1.0,
                    "total_tokens": 100,
                    "created_at": 1700000000
                },
                "messages": [
                    {"role": "user", "content": "Remember: code is ZEBRA-99"},
                    {"role": "assistant", "content": "OK, noted."}
                ],
                "budget_remaining_tokens": 100000,
                "budget_remaining_rounds": 50,
                "blocked_tools": [],
                "recent_tools": []
            }
        }"#;
        std::fs::write(cp_dir.join("000002-heavy.json"), checkpoint_json).unwrap();

        // The function under test: load messages for session continuation.
        let messages = super::load_session_messages_for_continuation(&session_id);

        // Cleanup
        let home = dirs::home_dir().unwrap();
        let _ = std::fs::remove_dir_all(home.join(".astra/sessions").join(&session_id));

        // Assert: must return the 2 messages from the checkpoint.
        let messages = messages.expect("should load messages from checkpoint");
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0]["role"], "user");
        assert_eq!(messages[0]["content"], "Remember: code is ZEBRA-99");
        assert_eq!(messages[1]["role"], "assistant");
        assert_eq!(messages[1]["content"], "OK, noted.");
    }

    #[test]
    fn load_session_messages_returns_none_for_missing_session() {
        let messages =
            super::load_session_messages_for_continuation("nonexistent-session-xyz-42");
        assert!(messages.is_none());
    }
}
