use super::*;
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
    client: &reqwest::Client,
    base: &str,
) -> Result<ExitCode, String> {
    match command {
        // No subcommand → interactive REPL (Codex-style default)
        None | Some(Command::Interactive) => {
            run_chat_repl(client, base, profile.as_deref(), None).await?;
            Ok(ExitCode::Success)
        }

        // Inline message: mo-agent "what is the answer to life?"
        Some(Command::Message(words)) => {
            let message = words.join(" ");
            let (mut creds, name, _, token) = get_profile_and_token(profile.as_deref())?;
            let session_id = resumable_last_session_id(profile.as_deref());
            let selector = create_tool_selector(client, base, profile.as_deref());
            let mut pm = PermissionManager::new(false);
            let sr = match stream_chat_sse(ChatTurnParams {
                client,
                base,
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
            })
            .await
            {
                Ok(sr) => sr,
                Err(e) if is_session_not_found_error(&e) && session_id.is_some() => {
                    let _ = clear_profile_last_session(profile.as_deref());
                    stream_chat_sse(ChatTurnParams {
                        client,
                        base,
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
            let resp = client
                .post(format!("{base}/auth/register"))
                .header(CONTENT_TYPE, "application/json")
                .json(&serde_json::json!({
                    "username": username,
                    "email": email,
                    "password": password
                }))
                .send()
                .await
                .map_err(|e| e.to_string())?;
            let status = resp.status();
            let body = resp.text().await.map_err(|e| e.to_string())?;
            if !status.is_success() {
                return Err(read_api_error(status, &body));
            }
            eprintln!("{}", "  ✓  Registered! Now logging in…".green());
            // Auto-login after register
            do_login(client, base, profile.as_deref(), &username, &password).await?;
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
            do_login(client, base, profile.as_deref(), &username, &password).await?;
            eprintln!(
                "{}",
                "  ✓  Logged in. Run `mo-agent` to start chatting.".green()
            );
            Ok(ExitCode::Success)
        }

        Some(Command::Whoami) => {
            let (_, _, _, token) = get_profile_and_token(profile.as_deref())?;
            let resp = client
                .get(format!("{base}/auth/me"))
                .headers(auth_headers(&token)?)
                .send()
                .await
                .map_err(|e| e.to_string())?;
            let status = resp.status();
            let body = resp.text().await.map_err(|e| e.to_string())?;
            if !status.is_success() {
                return Err(read_api_error(status, &body));
            }
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
            let resp = client
                .post(format!("{base}/auth/refresh"))
                .header(CONTENT_TYPE, "application/json")
                .json(&serde_json::json!({ "refresh_token": refresh_token }))
                .send()
                .await
                .map_err(|e| e.to_string())?;
            let status = resp.status();
            let body = resp.text().await.map_err(|e| e.to_string())?;
            if !status.is_success() {
                return Err(read_api_error(status, &body));
            }
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
            let resp = client
                .post(format!("{base}/auth/logout"))
                .header(CONTENT_TYPE, "application/json")
                .json(&serde_json::json!({ "refresh_token": refresh_token }))
                .send()
                .await
                .map_err(|e| e.to_string())?;
            let status = resp.status();
            let body = resp.text().await.map_err(|e| e.to_string())?;
            if !status.is_success() {
                return Err(read_api_error(status, &body));
            }
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
            let resp = client
                .get(format!("{base}/health"))
                .send()
                .await
                .map_err(|e| e.to_string())?;
            let status = resp.status();
            let body = resp.text().await.map_err(|e| e.to_string())?;
            if !status.is_success() {
                return Err(read_api_error(status, &body));
            }
            print_json_or_raw(&body);
            Ok(ExitCode::Success)
        }

        Some(Command::Chat(args)) => {
            // Handle --no-color: set NO_COLOR environment variable for crossterm
            if args.no_color {
                unsafe { std::env::set_var("NO_COLOR", "1"); }
            }

            // Determine message source: --stdin, -m, or start REPL
            let message = if args.stdin {
                let mut buf = String::new();
                std::io::stdin()
                    .read_to_string(&mut buf)
                    .map_err(|e| format!("failed to read stdin: {e}"))?;
                buf.trim().to_string()
            } else if let Some(m) = args.message {
                m
            } else {
                // No message → start REPL with optional pre-set session/model
                run_chat_repl(client, base, profile.as_deref(), args.model.as_deref())
                    .await?;
                return Ok(ExitCode::Success);
            };

            let (mut creds, name, _, token) = get_profile_and_token(profile.as_deref())?;
            let session_id = args
                .session_id
                .or_else(|| resumable_last_session_id(profile.as_deref()));
            let is_tty = terminal::size().is_ok();
            let selector = create_tool_selector(client, base, profile.as_deref());
            let mut pm = PermissionManager::new(args.auto_approve);
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
                client,
                base,
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
            })
            .await
            {
                Ok(sr) => sr,
                Err(e) if is_session_not_found_error(&e) && session_id.is_some() => {
                    let _ = clear_profile_last_session(profile.as_deref());
                    stream_chat_sse(ChatTurnParams {
                        client,
                        base,
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
            let replay_resp = client
                .post(format!("{base}/sessions/{}/replay", args.session_id))
                .headers(auth_headers(&token)?)
                .json(&serde_json::json!({
                    "sandbox_name": args.sandbox_name,
                    "mock_mode": args.mock_mode
                }))
                .send()
                .await
                .map_err(|e| e.to_string())?;
            let replay_status = replay_resp.status();
            let replay_body = replay_resp.text().await.map_err(|e| e.to_string())?;
            if !replay_status.is_success() {
                return Err(read_api_error(replay_status, &replay_body));
            }
            print_json_or_raw(&replay_body);
            if args.compare {
                let compare_resp = client
                    .get(format!(
                        "{base}/sessions/{}/replay/compare",
                        args.session_id
                    ))
                    .headers(auth_headers(&token)?)
                    .send()
                    .await
                    .map_err(|e| e.to_string())?;
                let compare_status = compare_resp.status();
                let compare_body = compare_resp.text().await.map_err(|e| e.to_string())?;
                if !compare_status.is_success() {
                    return Err(read_api_error(compare_status, &compare_body));
                }
                print_json_or_raw(&compare_body);
            }
            Ok(ExitCode::Success)
        }

        Some(Command::Session(SessionCmd::List(args))) => {
            let (_, _, _, token) = get_profile_and_token(profile.as_deref())?;
            let mut req = client
                .get(format!("{base}/sessions"))
                .headers(auth_headers(&token)?)
                .query(&[
                    ("limit", args.limit.to_string()),
                    ("offset", args.offset.to_string()),
                ]);
            if let Some(agent_id) = args.agent_id {
                req = req.query(&[("agent_id", agent_id)]);
            }
            if let Some(status) = args.status {
                req = req.query(&[("session_status", status)]);
            }
            let resp = req.send().await.map_err(|e| e.to_string())?;
            let status = resp.status();
            let body = resp.text().await.map_err(|e| e.to_string())?;
            if !status.is_success() {
                return Err(read_api_error(status, &body));
            }
            print_json_or_raw(&body);
            Ok(ExitCode::Success)
        }

        Some(Command::Session(SessionCmd::Show(args))) => {
            let (_, _, _, token) = get_profile_and_token(profile.as_deref())?;
            let resp = client
                .get(format!("{base}/sessions/{}", args.session_id))
                .headers(auth_headers(&token)?)
                .send()
                .await
                .map_err(|e| e.to_string())?;
            let status = resp.status();
            let body = resp.text().await.map_err(|e| e.to_string())?;
            if !status.is_success() {
                return Err(read_api_error(status, &body));
            }
            print_json_or_raw(&body);
            Ok(ExitCode::Success)
        }

        Some(Command::Session(SessionCmd::Close(args))) => {
            let (creds, name, _, token) = get_profile_and_token(profile.as_deref())?;
            let resp = client
                .post(format!("{base}/sessions/{}/close", args.session_id))
                .headers(auth_headers(&token)?)
                .send()
                .await
                .map_err(|e| e.to_string())?;
            let status = resp.status();
            let body = resp.text().await.map_err(|e| e.to_string())?;
            if !status.is_success() {
                return Err(read_api_error(status, &body));
            }
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
            let resp = client
                .delete(format!("{base}/sessions/{}", args.session_id))
                .headers(auth_headers(&token)?)
                .send()
                .await
                .map_err(|e| e.to_string())?;
            let status = resp.status();
            let body = resp.text().await.map_err(|e| e.to_string())?;
            if !status.is_success() {
                return Err(read_api_error(status, &body));
            }
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
            let resp = client
                .get(format!("{base}/models"))
                .headers(auth_headers(&token)?)
                .send()
                .await
                .map_err(|e| e.to_string())?;
            let status = resp.status();
            let body = resp.text().await.map_err(|e| e.to_string())?;
            if !status.is_success() {
                return Err(read_api_error(status, &body));
            }
            print_json_or_raw(&body);
            Ok(ExitCode::Success)
        }

        Some(Command::Model(ModelCmd::Show(args))) => {
            let (_, _, _, token) = get_profile_and_token(profile.as_deref())?;
            let resp = client
                .get(format!("{base}/models/{}", args.model_name))
                .headers(auth_headers(&token)?)
                .send()
                .await
                .map_err(|e| e.to_string())?;
            let status = resp.status();
            let body = resp.text().await.map_err(|e| e.to_string())?;
            if !status.is_success() {
                return Err(read_api_error(status, &body));
            }
            print_json_or_raw(&body);
            Ok(ExitCode::Success)
        }

        Some(Command::Skill(SkillCmd::List(args))) => {
            let (_, _, _, token) = get_profile_and_token(profile.as_deref())?;
            let resp = client
                .get(format!("{base}/skills"))
                .headers(auth_headers(&token)?)
                .query(&[
                    ("limit", args.limit.to_string()),
                    ("offset", args.offset.to_string()),
                ])
                .send()
                .await
                .map_err(|e| e.to_string())?;
            let status = resp.status();
            let body = resp.text().await.map_err(|e| e.to_string())?;
            if !status.is_success() {
                return Err(read_api_error(status, &body));
            }
            print_json_or_raw(&body);
            Ok(ExitCode::Success)
        }

        Some(Command::Skill(SkillCmd::Show(args))) => {
            let (_, _, _, token) = get_profile_and_token(profile.as_deref())?;
            let mut req = client
                .get(format!("{base}/skills/{}", args.skill_id))
                .headers(auth_headers(&token)?);
            if let Some(version) = args.version {
                req = req.query(&[("version", version)]);
            }
            let resp = req.send().await.map_err(|e| e.to_string())?;
            let status = resp.status();
            let body = resp.text().await.map_err(|e| e.to_string())?;
            if !status.is_success() {
                return Err(read_api_error(status, &body));
            }
            print_json_or_raw(&body);
            Ok(ExitCode::Success)
        }

        Some(Command::Skill(SkillCmd::Status(args))) => {
            let (_, _, _, token) = get_profile_and_token(profile.as_deref())?;
            let resp = client
                .get(format!("{base}/skills/status"))
                .headers(auth_headers(&token)?)
                .query(&[("per_group", args.per_group.to_string())])
                .send()
                .await
                .map_err(|e| e.to_string())?;
            let status = resp.status();
            let body = resp.text().await.map_err(|e| e.to_string())?;
            if !status.is_success() {
                return Err(read_api_error(status, &body));
            }
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
            let resp = client
                .post(format!("{base}/skills"))
                .headers(auth_headers(&token)?)
                .json(&serde_json::json!({
                    "skill_id": skill_id,
                    "skill_name": args.name,
                    "skill_version": args.version,
                    "skill_code": skill_code,
                    "description": args.description,
                    "metadata": metadata
                }))
                .send()
                .await
                .map_err(|e| e.to_string())?;
            let status = resp.status();
            let body = resp.text().await.map_err(|e| e.to_string())?;
            if !status.is_success() {
                return Err(read_api_error(status, &body));
            }
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
