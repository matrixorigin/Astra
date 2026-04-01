use super::*;
use crate::permission_manager::PermissionMode;
use mo_thin_client::paths;
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

pub(super) async fn execute_cli_command(
    command: Option<Command>,
    profile: Option<String>,
    api: &mo_thin_client::ThinClient,
) -> Result<ExitCode, String> {
    match command {
        // No subcommand → interactive REPL (Codex-style default)
        None | Some(Command::Interactive) => {
            run_chat_repl(api, profile.as_deref(), None).await?;
            Ok(ExitCode::Success)
        }

        // Inline message: mo-agent "what is the answer to life?"
        Some(Command::Message(words)) => {
            let message = words.join(" ");
            let (mut creds, name, _, token) = get_profile_and_token(profile.as_deref())?;
            let session_id = resumable_last_session_id(profile.as_deref());
            let selector = create_tool_selector(api, profile.as_deref());
            let mut pm = PermissionManager::with_project(
                false,
                &std::env::current_dir().unwrap_or_default(),
            );
            let sr = match stream_chat_sse(ChatTurnParams {
                api,
                token: &token,
                message: &message,
                session_id: session_id.as_deref(),
                model: None,
                explain: ExplainMode::Off,
                render_md: terminal::size().is_ok(),
                history: &[],
                perm_manager: &mut pm,
                verbose_mode: true,
                quiet: false,
                selector: &*selector.0,
                recent_tools: &[],
                tool_health_entries: &[],
                skill_registry: crate::skill_instructions::empty_registry(),
            })
            .await
            {
                Ok(sr) => sr,
                Err(e) if is_session_not_found_error(&e) && session_id.is_some() => {
                    let _ = clear_profile_last_session(profile.as_deref());
                    stream_chat_sse(ChatTurnParams {
                        api,
                        token: &token,
                        message: &message,
                        session_id: None,
                        model: None,
                        explain: ExplainMode::Off,
                        render_md: terminal::size().is_ok(),
                        history: &[],
                        perm_manager: &mut pm,
                        verbose_mode: true,
                        quiet: false,
                        selector: &*selector.0,
                        recent_tools: &[],
                        tool_health_entries: &[],
                        skill_registry: crate::skill_instructions::empty_registry(),
                    })
                    .await?
                }
                Err(e) => return Err(e),
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
                "  ✓  Logged in. Run `mo-agent` to start chatting.".green()
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
                "  ✓  Logged in. Run `mo-agent` to start chatting.".green()
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
            // Handle --no-color: set NO_COLOR environment variable for crossterm
            if args.no_color {
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
                run_chat_repl(api, profile.as_deref(), args.model.as_deref()).await?;
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
                    PermissionManager::with_project(args.auto_approve, &project_root)
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

            let sr = match stream_chat_sse(ChatTurnParams {
                api,
                token: &token,
                message: &message,
                session_id: session_id.as_deref(),
                model: args.model.as_deref(),
                explain: explain_mode,
                render_md,
                history: &[],
                perm_manager: &mut pm,
                verbose_mode: !quiet,
                quiet,
                selector: &*selector.0,
                recent_tools: &[],
                tool_health_entries: &[],
                skill_registry: crate::skill_instructions::empty_registry(),
            })
            .await
            {
                Ok(sr) => sr,
                Err(e) if is_session_not_found_error(&e) && session_id.is_some() => {
                    let _ = clear_profile_last_session(profile.as_deref());
                    stream_chat_sse(ChatTurnParams {
                        api,
                        token: &token,
                        message: &message,
                        session_id: None,
                        model: args.model.as_deref(),
                        explain: explain_mode,
                        render_md,
                        history: &[],
                        perm_manager: &mut pm,
                        verbose_mode: !quiet,
                        quiet,
                        selector: &*selector.0,
                        recent_tools: &[],
                        tool_health_entries: &[],
                        skill_registry: crate::skill_instructions::empty_registry(),
                    })
                    .await?
                }
                Err(e) => return Err(e),
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
                api.get_bearer_path_query_text(
                    &token,
                    &paths::session_audit_tools(sid),
                    &[],
                )
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
