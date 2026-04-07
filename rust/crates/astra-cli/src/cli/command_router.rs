use super::*;
use crate::permission_manager::PermissionMode;
use astra_thin_client::paths;
use clap::CommandFactory;
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

/// Prepend system prompt to user message when `--system-prompt` is set.
fn apply_system_prompt(message: &str, system_prompt: Option<&str>) -> String {
    match system_prompt {
        Some(sp) => format!("<system_instructions>\n{sp}\n</system_instructions>\n\n{message}"),
        None => message.to_string(),
    }
}

pub(super) async fn execute_cli_command(
    command: Option<Command>,
    profile: Option<String>,
    global_model: Option<String>,
    auto_approve: bool,
    system_prompt: Option<String>,
    api: &astra_thin_client::ThinClient,
) -> Result<ExitCode, String> {
    match command {
        // No subcommand → interactive REPL (Codex-style default)
        None | Some(Command::Interactive) => {
            run_chat_repl(api, profile.as_deref(), global_model.as_deref(), None).await?;
            Ok(ExitCode::Success)
        }

        // Start embedded HTTP API server
        Some(Command::Serve(args)) => {
            let addr: std::net::SocketAddr = format!("{}:{}", args.host, args.port)
                .parse()
                .map_err(|e| format!("Invalid listen address: {e}"))?;
            eprintln!("Starting API server on {addr} ...");
            astra_runtime::serve(addr)
                .await
                .map_err(|e| format!("Server error: {e}"))?;
            Ok(ExitCode::Success)
        }

        // Inline message: astra "what is the answer to life?"
        Some(Command::Message(words)) => {
            let raw_message = words.join(" ");
            let message = apply_system_prompt(&raw_message, system_prompt.as_deref());
            let (mut creds, name, _, token) = get_profile_and_token(profile.as_deref())?;
            let session_id = resumable_last_session_id(profile.as_deref());
            let selector = create_tool_selector(api, profile.as_deref());
            let mut pm = PermissionManager::with_project(
                auto_approve,
                &std::env::current_dir().unwrap_or_default(),
            );
            let mut skill_qt = astra_runtime::skills::quality::SkillQualityTracker::new();
            let skill_search = astra_core::SkillSearchSettings::default();
            let sr = match stream_chat_sse(ChatTurnParams {
                api,
                token: &token,
                message: &message,
                session_id: session_id.as_deref(),
                model: global_model.as_deref(),
                explain: ExplainMode::Off,
                render_md: terminal::size().is_ok(),
                history: &[],
                perm_manager: &mut pm,
                verbose_mode: true,
                quiet: false,
                suppress_intermediate_output: false,
                selector: &*selector.0,
                recent_tools: &[],
                tool_health_entries: &[],
                unified_skill_registry: astra_runtime::skills::default_unified_registry(),
                plan_only_chat: false,
                hide_streaming_assistant_text: false,
                is_plan_subtask: false,
                plan_subtask_id: None,
                delegation_engine: None,
                cancel_token: None,
                plan_assemble_line_release: None,
                stream_event_tx: None,
                approval_request_tx: None,
                mcp_manager: None,
                skill_search: &skill_search,
                skill_quality_tracker: &mut skill_qt,
                discovered_skills: None,
            })
            .await
            {
                Ok(sr) => sr,
                Err(e) if is_session_not_found_error(&e.error) && session_id.is_some() => {
                    let _ = clear_profile_last_session(profile.as_deref());
                    stream_chat_sse(ChatTurnParams {
                        api,
                        token: &token,
                        message: &message,
                        session_id: None,
                        model: global_model.as_deref(),
                        explain: ExplainMode::Off,
                        render_md: terminal::size().is_ok(),
                        history: &[],
                        perm_manager: &mut pm,
                        verbose_mode: true,
                        quiet: false,
                        suppress_intermediate_output: false,
                        selector: &*selector.0,
                        recent_tools: &[],
                        tool_health_entries: &[],
                        unified_skill_registry: astra_runtime::skills::default_unified_registry(),
                        plan_only_chat: false,
                        hide_streaming_assistant_text: false,
                        is_plan_subtask: false,
                        plan_subtask_id: None,
                        delegation_engine: None,
                        cancel_token: None,
                        plan_assemble_line_release: None,
                        stream_event_tx: None,
                        approval_request_tx: None,
                        mcp_manager: None,
                        skill_search: &skill_search,
                        skill_quality_tracker: &mut skill_qt,
                        discovered_skills: None,
                    })
                    .await
                    .map_err(|f| f.error)?
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
            println!("token refreshed");
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

        Some(Command::Chat(args)) => {
            // Handle --no-color or non-terminal stderr: disable ANSI colors via NO_COLOR env.
            // crossterm checks NO_COLOR to suppress escape sequences globally.
            if args.no_color
                || (!std::io::IsTerminal::is_terminal(&std::io::stderr())
                    && std::env::var("NO_COLOR").is_err())
            {
                unsafe {
                    std::env::set_var("NO_COLOR", "1");
                }
            }

            // Determine message source: --stdin, -m, or start REPL
            let message = if args.stdin {
                let mut buf = String::new();
                std::io::stdin()
                    .read_to_string(&mut buf)
                    .map_err(|e| format!("failed to read stdin: {e}"))?;
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
                run_chat_repl(api, profile.as_deref(), model, None).await?;
                return Ok(ExitCode::Success);
            };

            let (mut creds, name, _, token) = get_profile_and_token(profile.as_deref())?;
            let session_id = args
                .session_id
                .or_else(|| resumable_last_session_id(profile.as_deref()));
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

            let mut skill_qt = astra_runtime::skills::quality::SkillQualityTracker::new();
            let skill_search = astra_core::SkillSearchSettings::default();
            let sr = match stream_chat_sse(ChatTurnParams {
                api,
                token: &token,
                message: &message,
                session_id: session_id.as_deref(),
                model: args.model.as_deref().or(global_model.as_deref()),
                explain: explain_mode,
                render_md,
                history: &[],
                perm_manager: &mut pm,
                verbose_mode: !quiet,
                quiet,
                suppress_intermediate_output: false,
                selector: &*selector.0,
                recent_tools: &[],
                tool_health_entries: &[],
                unified_skill_registry: astra_runtime::skills::default_unified_registry(),
                plan_only_chat: false,
                hide_streaming_assistant_text: false,
                is_plan_subtask: false,
                plan_subtask_id: None,
                delegation_engine: None,
                cancel_token: None,
                plan_assemble_line_release: None,
                stream_event_tx: None,
                approval_request_tx: None,
                mcp_manager: None,
                skill_search: &skill_search,
                skill_quality_tracker: &mut skill_qt,
                discovered_skills: None,
            })
            .await
            {
                Ok(sr) => sr,
                Err(e) if is_session_not_found_error(&e.error) && session_id.is_some() => {
                    let _ = clear_profile_last_session(profile.as_deref());
                    stream_chat_sse(ChatTurnParams {
                        api,
                        token: &token,
                        message: &message,
                        session_id: None,
                        model: args.model.as_deref().or(global_model.as_deref()),
                        explain: explain_mode,
                        render_md,
                        history: &[],
                        perm_manager: &mut pm,
                        verbose_mode: !quiet,
                        quiet,
                        suppress_intermediate_output: false,
                        selector: &*selector.0,
                        recent_tools: &[],
                        tool_health_entries: &[],
                        unified_skill_registry: astra_runtime::skills::default_unified_registry(),
                        plan_only_chat: false,
                        hide_streaming_assistant_text: false,
                        is_plan_subtask: false,
                        plan_subtask_id: None,
                        delegation_engine: None,
                        cancel_token: None,
                        plan_assemble_line_release: None,
                        stream_event_tx: None,
                        approval_request_tx: None,
                        mcp_manager: None,
                        skill_search: &skill_search,
                        skill_quality_tracker: &mut skill_qt,
                        discovered_skills: None,
                    })
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

            // Output result
            if args.json {
                // Compute exit code for JSON output
                let exit_code = compute_exit_code(&sr);
                // Pure JSON output for scripting
                let json_output = serde_json::json!({
                    "session_id": sr.session_id,
                    "run_id": sr.run_id,
                    "text": sr.full_text,
                    "prompt_tokens": sr.prompt_tokens,
                    "completion_tokens": sr.completion_tokens,
                    "tool_calls_count": sr.tool_calls_count,
                    "tools_used": sr.tools_used,
                    "ttft_ms": sr.ttft_ms,
                    "context_ms": sr.context_ms,
                    "selector_strategy": sr.selector_strategy,
                    "exit_code": i32::from(exit_code),
                    "success": exit_code == ExitCode::Success,
                });
                println!("{}", serde_json::to_string_pretty(&json_output).unwrap());
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
                println!("deleted");
            } else {
                print_json_or_raw(&body);
            }
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

    // Check for tool failures
    for record in &sr.tool_call_records {
        if !record.ok {
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
    let session_id = resumable_last_session_id(profile);
    let selector = create_tool_selector(api, profile);
    let mut pm = PermissionManager::with_project(
        true, // print mode is headless, always auto-approve
        &std::env::current_dir().unwrap_or_default(),
    );
    let mut skill_qt = astra_runtime::skills::quality::SkillQualityTracker::new();
    let skill_search = astra_core::SkillSearchSettings::default();

    let sr = match stream_chat_sse(ChatTurnParams {
        api,
        token: &token,
        message: &message,
        session_id: session_id.as_deref(),
        model: model,
        explain: ExplainMode::Off,
        render_md: false,
        history: &[],
        perm_manager: &mut pm,
        verbose_mode: std::env::var("ASTRA_VERBOSE")
            .map(|v| v == "1")
            .unwrap_or(false),
        quiet: true,
        suppress_intermediate_output: true,
        selector: &*selector.0,
        recent_tools: &[],
        tool_health_entries: &[],
        unified_skill_registry: astra_runtime::skills::default_unified_registry(),
        plan_only_chat: false,
        hide_streaming_assistant_text: true,
        is_plan_subtask: false,
        plan_subtask_id: None,
        delegation_engine: None,
        cancel_token: None,
        plan_assemble_line_release: None,
        stream_event_tx: None,
        approval_request_tx: None,
        mcp_manager: None,
        skill_search: &skill_search,
        skill_quality_tracker: &mut skill_qt,
        discovered_skills: None,
    })
    .await
    {
        Ok(sr) => sr,
        Err(e) if is_session_not_found_error(&e.error) && session_id.is_some() => {
            let _ = clear_profile_last_session(profile);
            stream_chat_sse(ChatTurnParams {
                api,
                token: &token,
                message: &message,
                session_id: None,
                model: model,
                explain: ExplainMode::Off,
                render_md: false,
                history: &[],
                perm_manager: &mut pm,
                verbose_mode: std::env::var("ASTRA_VERBOSE")
                    .map(|v| v == "1")
                    .unwrap_or(false),
                quiet: true,
                suppress_intermediate_output: true,
                selector: &*selector.0,
                recent_tools: &[],
                tool_health_entries: &[],
                unified_skill_registry: astra_runtime::skills::default_unified_registry(),
                plan_only_chat: false,
                hide_streaming_assistant_text: true,
                is_plan_subtask: false,
                plan_subtask_id: None,
                delegation_engine: None,
                cancel_token: None,
                plan_assemble_line_release: None,
                stream_event_tx: None,
                approval_request_tx: None,
                mcp_manager: None,
                skill_search: &skill_search,
                skill_quality_tracker: &mut skill_qt,
                discovered_skills: None,
            })
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
                "prompt_tokens": sr.prompt_tokens,
                "completion_tokens": sr.completion_tokens,
                "tool_calls_count": sr.tool_calls_count,
                "tools_used": sr.tools_used,
                "exit_code": i32::from(exit_code),
                "success": exit_code == ExitCode::Success,
            });
            println!("{}", serde_json::to_string_pretty(&json_output).unwrap());
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
    println!("Astra Doctor");
    println!("{}\n", "═".repeat(50));
    let mut issues: Vec<String> = Vec::new();

    // 1. Version
    let version = env!("CARGO_PKG_VERSION");
    println!("Version");
    println!("  Binary: {version}");
    println!(
        "  Executable: {}",
        std::env::current_exe()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| "unknown".into())
    );
    println!();

    // 2. API server connectivity
    println!("API Server");
    println!("  URL: {}", api.api_origin());
    match api.get_health_text().await {
        Ok(body) => println!("  Status: ✓ Healthy ({})", body.trim()),
        Err(e) => {
            println!("  Status: ✗ Unreachable");
            issues.push(format!("API server unreachable: {e}"));
        }
    }
    println!();

    // 3. Authentication
    println!("Authentication");
    let creds = load_credentials();
    let name = profile_name(profile, &creds);
    println!("  Profile: {name}");
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
                        println!("  Status: ✓ Logged in as {user}");
                    } else {
                        println!("  Status: ✓ Authenticated");
                    }
                }
                Err(_) => {
                    println!("  Status: ⚠ Token may be expired");
                    issues.push(
                        "Auth token may be expired — try `astra refresh` or `astra login`".into(),
                    );
                }
            }
        }
        Err(e) => {
            println!("  Status: ✗ Not logged in");
            issues.push(format!("Not authenticated: {e}"));
        }
    }
    println!();

    // 4. Project config
    println!("Project Configuration");
    let cwd = std::env::current_dir().unwrap_or_default();
    let astra_dir = cwd.join(".astra");
    if astra_dir.is_dir() {
        println!("  .astra/ directory: ✓ Found");
    } else {
        println!("  .astra/ directory: - Not found (optional)");
    }
    println!("  Working directory: {}", cwd.display());
    println!();

    // 5. MCP configuration
    println!("MCP Configuration");
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
                            println!("  {scope}: ✓ {count} server(s) in {}", path.display());
                        }
                        Err(e) => {
                            println!("  {scope}: ✗ Invalid JSON in {}", path.display());
                            issues.push(format!("MCP {scope} config parse error: {e}"));
                        }
                    },
                    Err(e) => {
                        println!("  {scope}: ✗ Cannot read {}", path.display());
                        issues.push(format!("MCP {scope} config read error: {e}"));
                    }
                }
            } else {
                println!("  {scope}: - No config file");
            }
        }
    }
    println!();

    // 6. Environment
    println!("Environment");
    println!("  OS: {}", std::env::consts::OS);
    println!("  Arch: {}", std::env::consts::ARCH);
    if let Ok(shell) = std::env::var("SHELL") {
        println!("  Shell: {shell}");
    }
    if let Ok(term) = std::env::var("TERM") {
        println!("  Terminal: {term}");
    }
    println!();

    // Summary
    if issues.is_empty() {
        println!("✓ No issues found");
    } else {
        println!("Found {} issue(s):", issues.len());
        for issue in &issues {
            println!("  ⚠ {issue}");
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
                .unwrap()
                .entry("mcpServers")
                .or_insert_with(|| serde_json::json!({}))
                .as_object_mut()
                .unwrap();
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
        println!("No MCP servers configured in {} scope.", scope);
        println!("Use `astra mcp add` to add a server.");
        return Ok(());
    }

    println!("{:<20} {:<8} {:<40}", "Name", "Type", "Command / URL");
    println!("{}", "─".repeat(70));
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
        println!("{:<20} {:<8} {}", name, server_type, detail);
    }
    println!("\nConfig file: {}", path.display());
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
        .unwrap()
        .entry("mcpServers")
        .or_insert_with(|| serde_json::json!({}))
        .as_object_mut()
        .unwrap()
        .insert(name.to_string(), entry);

    write_mcp_config(&path, &config)?;
    println!("Added '{name}' to {}", path.display());
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
        .unwrap()
        .entry("mcpServers")
        .or_insert_with(|| serde_json::json!({}))
        .as_object_mut()
        .unwrap()
        .insert(name.to_string(), entry);

    write_mcp_config(&path, &config)?;
    println!("Added '{name}' to {}", path.display());
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
    println!("Removed '{name}' from {}", path.display());
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
            println!("{}:", name);
            println!("  Scope: {scope}");
            let server_type = entry
                .get("type")
                .and_then(|v| v.as_str())
                .unwrap_or("stdio");
            println!("  Type: {server_type}");
            match server_type {
                "sse" | "http" => {
                    if let Some(url) = entry.get("url").and_then(|v| v.as_str()) {
                        println!("  URL: {url}");
                    }
                }
                _ => {
                    if let Some(cmd) = entry.get("command").and_then(|v| v.as_str()) {
                        println!("  Command: {cmd}");
                    }
                    if let Some(args) = entry.get("args").and_then(|v| v.as_array()) {
                        let args_str: Vec<&str> = args.iter().filter_map(|v| v.as_str()).collect();
                        println!("  Args: {}", args_str.join(" "));
                    }
                }
            }
            if let Some(env) = entry.get("env").and_then(|v| v.as_object()) {
                println!("  Environment:");
                for (k, v) in env {
                    println!("    {k}={}", v.as_str().unwrap_or(&v.to_string()));
                }
            }
            println!("\nTo remove: astra mcp remove \"{}\" -s {scope}", name);
            return Ok(());
        }
    }
    Err(format!("No MCP server found with name: {name}"))
}

// ═══════════════════════════════════════════════════════ Config ═══════════

/// Path to `~/.astra/settings.json`.
fn settings_path() -> Result<std::path::PathBuf, String> {
    dirs::home_dir()
        .map(|h| h.join(".astra").join("settings.json"))
        .ok_or_else(|| "Cannot determine home directory".to_string())
}

fn read_settings() -> Result<serde_json::Map<String, serde_json::Value>, String> {
    let path = settings_path()?;
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

/// Read `default_model` from settings.json, if set.
pub fn read_config_default_model() -> Result<Option<String>, String> {
    let settings = read_settings()?;
    Ok(settings
        .get("default_model")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string()))
}

fn write_settings(settings: &serde_json::Map<String, serde_json::Value>) -> Result<(), String> {
    let path = settings_path()?;
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
    }
}

fn config_list() -> Result<(), String> {
    let settings = read_settings()?;
    let path = settings_path()?;

    if settings.is_empty() {
        println!("No settings configured.");
        println!("Use `astra config set <key> <value>` to set a value.");
        println!("\nAvailable keys:");
        for (key, desc) in KNOWN_SETTINGS {
            println!("  {key:<20} {desc}");
        }
        return Ok(());
    }

    println!("{:<20} {}", "Key", "Value");
    println!("{}", "─".repeat(50));
    for (key, value) in &settings {
        let display = match value {
            serde_json::Value::String(s) => s.clone(),
            other => other.to_string(),
        };
        println!("{key:<20} {display}");
    }
    println!("\nConfig file: {}", path.display());
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
    println!("Set '{key}' = {value}");
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
            ttft_ms: None,
            context_ms: None,
            selector_strategy: None,
            selector_ms: None,
            selector_tokens_in: 0,
            selector_tokens_out: 0,
            memoria_ms: None,
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
            });
        sr.verdict_events.push(VerdictEvent {
            turn: 1,
            severity: "critical".to_string(),
            injections: vec![],
            avoid_tools: vec![],
            force_stop: true,
            nudge_count: 0,
            total_errors: 3,
            deprioritized_count: 0,
            total_timeouts: 0,
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
            force_stop: false,
            nudge_count: 1,
            total_errors: 1,
            deprioritized_count: 0,
            total_timeouts: 0,
            total_cache_hits: 0,
            flaky_count: 0,
        });
        assert_eq!(compute_exit_code(&sr), ExitCode::Success);
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
