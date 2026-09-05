// Clippy 1.94 — allow backlog in the large CLI binary; refine incrementally.
#![allow(
    dead_code,
    deprecated,
    clippy::collapsible_if,
    clippy::derivable_impls,
    clippy::field_reassign_with_default,
    clippy::items_after_test_module,
    clippy::let_unit_value,
    clippy::manual_strip,
    clippy::needless_borrow,
    clippy::redundant_closure,
    clippy::single_match,
    clippy::unnecessary_mut_passed
)]

use astra_services::session_journal;
use clap::Parser;
use crossterm::style::Stylize;

use crate::cli;

use cli::cli_config::cli_args::Cli;
use cli::cli_config::cli_utils::{
    CliProfileIdentityAdmission, configure_cli_profile_identity, local_resumable_last_session_id,
    normalize_model_override_owned,
};
use cli::command_router::{execute_cli_command, run_print_mode};
use cli::exit_code::ExitCode;
use cli::slash::slash_config;
#[cfg(test)]
use cli::slash::slash_router::handle_slash_command;
// CLI argument structs moved to cli/cli_args.rs

// SSE streaming types moved to cli/streaming_types.rs
// Session state moved to cli/session_state.rs
#[cfg(test)]
#[cfg(test)]
pub(crate) use cli::session::session_state::SessionState;

// ═══════════════════════════════════════════════ Output Styles ═════════════

// ═══════════════════════════════════════════════════ Learning Merge ═══════
// Cloud sync moved to cli/cloud_sync.rs

// ═══════════════════════════════════════════════════════ Task Commands ════

// ══════════════════════════════════════════════════════ Slash Commands ════

// ---------------------------------------------------------------------------
// Session finalization — shared logic for all exit paths
// ---------------------------------------------------------------------------

// Session cleanup moved to session_cleanup.rs
use cli::project_instructions::{
    discover_project_instructions, format_project_instructions, resolve_system_prompt,
};

// ════════════════════════════════════════════════════════════════ main ════

fn apply_trace_cli_overlay(
    overlay: &mut astra_config::runtime_config::RuntimeConfig,
    trace_profile: Option<&str>,
    trace_level: Option<&str>,
    trace_cat: Option<&str>,
) -> Result<bool, String> {
    if trace_profile.is_none() && trace_level.is_none() && trace_cat.is_none() {
        return Ok(false);
    }
    overlay.trace =
        overlay
            .trace
            .clone()
            .with_cli_overrides(trace_profile, trace_level, trace_cat)?;
    Ok(true)
}

#[cfg(test)]
mod trace_overlay_tests {
    use super::apply_trace_cli_overlay;

    #[test]
    fn apply_trace_cli_overlay_preserves_existing_overlay_fields() {
        let mut overlay = astra_config::runtime_config::RuntimeConfig::default();
        overlay.runtime_limits.max_turns = 17;

        let changed = apply_trace_cli_overlay(&mut overlay, Some("dev"), None, None)
            .expect("trace overlay should parse");

        assert!(changed);
        assert_eq!(overlay.runtime_limits.max_turns, 17);
        assert_eq!(
            overlay.trace.enabled_categories,
            astra_config::runtime_config::TraceCategory::individual_categories().to_vec()
        );
    }

    #[test]
    fn apply_trace_cli_overlay_is_noop_without_trace_flags() {
        let mut overlay = astra_config::runtime_config::RuntimeConfig::default();
        overlay.runtime_limits.max_turns = 23;

        let changed =
            apply_trace_cli_overlay(&mut overlay, None, None, None).expect("no-op should succeed");

        assert!(!changed);
        assert_eq!(overlay.runtime_limits.max_turns, 23);
        assert_eq!(
            overlay.trace,
            astra_config::runtime_config::SessionTraceConfig::default()
        );
    }
}

pub fn run() -> i32 {
    match astra_core::process_runtime::build_process_runtime() {
        Ok(runtime) => runtime.block_on(run_async()),
        Err(error) => {
            eprintln!("Error: unable to initialize Astra runtime: {error}");
            3
        }
    }
}

async fn run_async() -> i32 {
    let explicit_env_config = match astra_core::config::explicit_env_config_requested() {
        Ok(value) => value,
        Err(error) => {
            eprintln!("Error: {error}");
            return 2;
        }
    };
    if !explicit_env_config {
        dotenvy::dotenv().ok();
    }
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(error) => {
            let exit_code = error.exit_code();
            if error.use_stderr() {
                if let Err(print_error) = error.print() {
                    eprintln!("Error: failed to render command-line diagnostics: {print_error}");
                }
            } else {
                let rendered = error.render().to_string();
                let _ = crate::cli::stream::output_sink::write_stdout_records(&rendered);
            }
            return exit_code;
        }
    };
    if let Err(error) = cli.validate_external_message_shorthand() {
        eprintln!("Error: {error}");
        return 2;
    }
    cli::diagnostic_log::init_cli_observability(&cli);
    let mut cli_overlay = astra_config::runtime_config::RuntimeConfig::default();
    let mut has_cli_overlay = false;
    // Accumulate CLI-driven config overrides into a local overlay. The
    // overlay is NOT installed yet — it will be installed via
    // set_cli_overlay immediately before the first RuntimeConfig::load()
    // call. Keeping accumulation and installation close together prevents
    // a footgun window: any code placed between here and the load() call
    // would see stale config if the overlay were installed early.
    //
    // parse_settings_source handles inline JSON vs. filesystem path;
    // apply_settings_json parses the JSON into a RuntimeConfig (all fields
    // default) that `load()` will then merge with non-default-wins
    // semantics. Malformed input aborts with a clear message instead of
    // silently dropping the flag.
    if let Some(raw) = cli.settings.as_deref() {
        match astra_config::config_overlay::parse_settings_source(raw) {
            Ok(json) => match astra_config::config_overlay::apply_settings_json(
                astra_config::runtime_config::RuntimeConfig::default(),
                &json,
            ) {
                Ok(overlay) => {
                    cli_overlay = overlay;
                    has_cli_overlay = true;
                }
                Err(err) => {
                    eprintln!(
                        "{}",
                        format!("Error: --settings JSON is invalid: {err}").red()
                    );
                    return 2;
                }
            },
            Err(err) => {
                eprintln!("{}", format!("Error: --settings: {err}").red());
                return 2;
            }
        }
    }
    match apply_trace_cli_overlay(
        &mut cli_overlay,
        cli.trace_profile.as_deref(),
        cli.trace_level.as_deref(),
        cli.trace_cat.as_deref(),
    ) {
        Ok(changed) => {
            has_cli_overlay |= changed;
        }
        Err(err) => {
            eprintln!("{}", format!("Error: {err}").red());
            return 2;
        }
    }
    // Apply safety.trust_mode from runtime config to the global guard.
    // Defaults to Strict — users must explicitly opt in via
    // `~/.astra/config/runtime.toml` [safety] trust_mode = "trusted".
    //
    // Install CLI overlay immediately before the first RuntimeConfig::load()
    // to minimise the window where future code could call load()/cached()
    // without the overlay.
    if has_cli_overlay {
        astra_config::runtime_config::set_cli_overlay(Some(cli_overlay));
    }
    astra_runtime::apply_safety_config_from_runtime_config(
        &astra_config::runtime_config::RuntimeConfig::load(),
    );
    // Resolve API URL: --api-url flag > ASTRA_API_URL env var > config file > default
    let base = match cli::config_manager::resolve_api_url(cli.api_url.as_deref()) {
        Ok(base) => base,
        Err(err) => {
            tracing::error!(target: "astra_cli", error = %err, "failed to resolve API URL");
            eprintln!(
                "{}",
                format!("Error: failed to resolve API URL: {err}").red()
            );
            return 2;
        }
    };
    let api = match astra_thin_client::ThinClient::new(&base, None) {
        Ok(api) => api,
        Err(err) => {
            tracing::error!(
                target: "astra_cli",
                api_base = %base,
                error = %err,
                "invalid API URL or thin client init failed"
            );
            eprintln!(
                "{}",
                format!("Error: invalid API URL '{base}': {err}").red()
            );
            return 1;
        }
    };

    let Cli {
        api_url: _,
        profile,
        model: cli_model,
        print: print_mode,
        output_format,
        continue_last,
        resume,
        yes: auto_approve,
        system_prompt,
        allowed_tools,
        disallowed_tools,
        add_dir,
        verbose,
        mcp_config,
        settings: _settings_already_applied,
        session_id: cli_session_id,
        session_name,
        bare,
        no_instructions,
        no_journal_content,
        startup_trace,
        diagnostic_log: _,
        log_file: _,
        trace_profile: _trace_profile_already_applied,
        trace_level: _trace_level_already_applied,
        trace_cat: _trace_cat_already_applied,
        command,
    } = cli;

    let identity_admission = command
        .as_ref()
        .map(cli::cli_config::cli_args::Command::profile_identity_admission)
        .unwrap_or(CliProfileIdentityAdmission::RequireBoundAccount);
    if let Err(error) = configure_cli_profile_identity(profile.as_deref(), identity_admission) {
        tracing::error!(
            target: "astra_cli",
            %error,
            "failed to bind the local profile/account identity"
        );
        eprintln!("{}", format!("Error: {error}").red());
        return 1;
    }

    let _ = (startup_trace, bare);
    let mut cli_context = match cli::cli_config::cli_context::CliContext::from_launch_options(
        no_journal_content,
        &allowed_tools,
        &disallowed_tools,
        &add_dir,
        auto_approve,
        cli_session_id.clone(),
        session_name.clone(),
    ) {
        Ok(cli_context) => cli_context,
        Err(err) => {
            tracing::error!(target: "astra_cli", error = %err, "invalid CLI startup context");
            eprintln!("{}", err.red());
            return 1;
        }
    };
    // Preserve the historical process-wide signal for call sites that still
    // construct SessionState directly instead of going through CliContext.
    if auto_approve {
        unsafe {
            std::env::set_var("ASTRA_CLI_AUTO_APPROVE", "1");
        }
    }
    if cli_context.no_journal_content {
        session_journal::set_journal_content_redact_override(Some(true));
    }

    // --system-prompt: support @file syntax to read from file
    let system_prompt = match system_prompt {
        Some(system_prompt) => match resolve_system_prompt(system_prompt) {
            Ok(content) => Some(content),
            Err(e) => {
                tracing::error!(target: "astra_cli", error = %e, "failed to resolve --system-prompt");
                eprintln!("{}", e.red());
                return 1;
            }
        },
        None => None,
    };

    // Merge project instructions into system_prompt for inline/print modes.
    // TUI mode handles this separately via the typed input preparation path.
    let system_prompt = if no_instructions {
        system_prompt
    } else {
        match (system_prompt, discover_project_instructions()) {
            (Some(sp), Some(pi)) => Some(format!("{sp}\n\n{}", format_project_instructions(&pi))),
            (Some(sp), None) => Some(sp),
            (None, Some(pi)) => Some(format_project_instructions(&pi)),
            (None, None) => None,
        }
    };

    let _ = verbose;

    // --mcp-config: load MCP server configs from files/JSON strings
    if !mcp_config.is_empty() {
        if let Err(e) = cli::mcp_config::load_mcp_configs(&mcp_config) {
            tracing::error!(target: "astra_cli", error = %e, "failed to load MCP config");
            eprintln!("{}", format!("Error: failed to load MCP config: {e}").red());
            return 2;
        }
    }

    // Resolve model: --model flag > config default_model > None
    let config_default_model = if cli_model.is_none() {
        match cli::config_manager::read_config_default_model() {
            Ok(model) => model,
            Err(err) => {
                tracing::error!(
                    target: "astra_cli",
                    error = %err,
                    "failed to read default_model from settings"
                );
                eprintln!(
                    "{}",
                    format!("Error: failed to read default_model from settings: {err}").red()
                );
                return 2;
            }
        }
    } else {
        None
    };
    let resolved_model = normalize_model_override_owned(cli_model.or(config_default_model));

    let runner_surface = print_mode
        || continue_last
        || resume.is_some()
        || matches!(
            command.as_ref(),
            None | Some(cli::cli_config::cli_args::Command::Interactive)
                | Some(cli::cli_config::cli_args::Command::Chat(_))
                | Some(cli::cli_config::cli_args::Command::Message(_))
                | Some(cli::cli_config::cli_args::Command::Review(_))
                | Some(cli::cli_config::cli_args::Command::Team(_))
                | Some(cli::cli_config::cli_args::Command::Work(_))
        );
    let local_models_configured = astra_credentials::LocalModelConfigStore::new()
        .load()
        .map(|config| !config.models.is_empty())
        .unwrap_or(false);
    let interactive_runner_surface = !print_mode
        && (continue_last
            || resume.is_some()
            || matches!(
                command.as_ref(),
                None | Some(cli::cli_config::cli_args::Command::Interactive)
            ));
    let local_runner = if runner_surface && (local_models_configured || interactive_runner_surface)
    {
        let workspace = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
        match cli::local_runner_lifecycle::start(&api.api_origin(), profile.as_deref(), &workspace)
        {
            Ok(mut runner) => match runner
                .wait_until_alive(std::time::Duration::from_millis(500))
                .await
            {
                Ok(()) => Some(runner),
                Err(error) if local_models_configured => {
                    eprintln!("{}", format!("Error: {error}").red());
                    return i32::from(ExitCode::ApiError);
                }
                Err(error) => {
                    tracing::debug!(%error, "User Runner exited before model setup was requested");
                    None
                }
            },
            Err(error) if local_models_configured => {
                eprintln!("{}", format!("Error: {error}").red());
                return i32::from(ExitCode::ApiError);
            }
            Err(error) => {
                tracing::debug!(%error, "User Runner is not installed; Server models remain available");
                None
            }
        }
    } else {
        None
    };
    cli_context.local_runner_id = local_runner
        .as_ref()
        .map(|runner| runner.edge_id().to_owned());

    // Make the resolved model available to slash commands that print
    // model-aware diagnostics without mutating the process environment.
    slash_config::set_active_model_for_display(resolved_model.clone());
    slash_config::set_active_offering_id_for_request(None);

    // --print mode: headless single-shot, always auto-approve (can't prompt)
    if print_mode {
        match run_print_mode(
            &api,
            profile.as_deref(),
            &output_format,
            resolved_model.as_deref(),
            system_prompt.as_deref(),
            command,
            &cli_context,
        )
        .await
        {
            Ok(code) => return i32::from(code),
            Err(e) => {
                eprintln!("{}", format!("Error: {e}").red());
                return i32::from(ExitCode::ApiError);
            }
        }
    }

    // -c / --continue: resume most recent session
    // -r / --resume <ID>: resume specific session
    if continue_last || resume.is_some() {
        let session_id = resume.as_deref();

        // For -c, resolve the last session ID from credentials
        let resolved_sid = if continue_last && session_id.is_none() {
            if cli::session::session_runtime::resolve_cloud_base().is_some()
                && cli::session::session_runtime::current_access_token(profile.as_deref()).is_some()
            {
                cli::cli_config::cli_utils::validated_resumable_last_session_id(
                    &api,
                    profile.as_deref(),
                )
                .await
                .or_else(|| local_resumable_last_session_id(profile.as_deref()))
            } else {
                local_resumable_last_session_id(profile.as_deref())
            }
        } else {
            session_id.map(|s| s.to_string())
        };

        match resolved_sid {
            Some(sid) => {
                let result = cli::interactive_chat::run_interactive_chat(
                    &api,
                    profile.as_deref(),
                    resolved_model.as_deref(),
                    Some(&sid),
                    no_instructions,
                    &cli_context,
                )
                .await;
                match result {
                    Ok(()) => return 0,
                    Err(e) => {
                        eprintln!("{}", format!("Error: {e}").red());
                        return i32::from(ExitCode::ApiError);
                    }
                }
            }
            None => {
                eprintln!(
                    "{}",
                    "No previous session to continue. Start a new one with `astra`.".yellow()
                );
                return 1;
            }
        }
    }

    match execute_cli_command(
        command,
        profile,
        resolved_model,
        auto_approve,
        system_prompt,
        &api,
        no_instructions,
        &cli_context,
    )
    .await
    {
        Ok(exit_code) => i32::from(exit_code),
        Err(e) => {
            eprintln!("{}", format!("Error: {e}").red());
            i32::from(ExitCode::ApiError)
        }
    }
}

#[cfg(test)]
mod tests {
    pub(crate) use crate::test_utils::isolate_credentials;

    use super::{
        Cli, SessionState, cli, execute_cli_command, format_project_instructions,
        handle_slash_command, resolve_system_prompt, session_journal,
    };
    use astra_runtime::prompts;
    use axum::{Router, routing::get, routing::post};
    use clap::Parser;
    use cli::auth_flow::{do_login, do_register};
    use cli::cli_config::cli_args::{
        AuditCmd, AuditShowArgs, AuditToolsArgs, Command, MessagingArgs, ReplayArgs, SessionCmd,
        SessionShowArgs,
    };
    use cli::cli_config::cli_utils::{
        CredentialsFile, Profile, load_credentials, save_credentials,
    };
    use cli::permission_manager;
    use cli::project_instructions::discover_instructions_from_paths;
    use cli::session::session_runtime::initialize_session_state;
    use cli::slash::slash_memory::handle_memory_domain_command;
    use cli::slash::{slash_health, slash_stats, slash_tools};

    async fn mock_models_response() -> axum::Json<serde_json::Value> {
        axum::Json(crate::test_utils::mock_model_catalog_json(&[
            "test-model",
            "mock-model",
        ]))
    }

    async fn mock_model_access_response() -> axum::Json<serde_json::Value> {
        axum::Json(crate::test_utils::mock_model_access_json(&[
            "test-model",
            "mock-model",
        ]))
    }

    async fn spawn_mock_app(app: Router) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let base = format!("http://{addr}");
        tokio::spawn(async move {
            axum::serve(listener, app).await.ok();
        });
        tokio::task::yield_now().await;
        base
    }

    async fn spawn_mock(app: Router) -> String {
        spawn_mock_app(
            app.route("/models", get(mock_models_response))
                .route("/model-access", get(mock_model_access_response)),
        )
        .await
    }

    // ── auth_flow ─────────────────────────────────────────────────────────

    #[serial_test::serial]
    #[tokio::test]
    async fn do_login_success() {
        let _creds_dir = isolate_credentials();
        let app = Router::new().route(
            "/auth/login",
            post(|| async {
                axum::Json(serde_json::json!({
                    "user_id": "user-id-1",
                    "access_token": "tok-abc",
                    "refresh_token": "ref-xyz"
                }))
            }),
        );
        let base = spawn_mock_app(app).await;
        let api = astra_thin_client::ThinClient::new(&base, None).unwrap();
        let result = do_login(&api, Some("__test__"), "user1", "pass1").await;
        assert_eq!(result.unwrap(), "tok-abc");
    }

    #[serial_test::serial]
    #[tokio::test]
    async fn do_login_failure_returns_error() {
        let _creds_dir = isolate_credentials();
        let app = Router::new().route(
            "/auth/login",
            post(|| async {
                (
                    axum::http::StatusCode::UNAUTHORIZED,
                    axum::Json(serde_json::json!({"detail": "bad credentials"})),
                )
            }),
        );
        let base = spawn_mock_app(app).await;
        let api = astra_thin_client::ThinClient::new(&base, None).unwrap();
        let result = do_login(&api, Some("test-profile"), "user1", "wrong").await;
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("401"));
    }

    #[serial_test::serial]
    #[tokio::test]
    async fn do_register_success() {
        let _creds_dir = isolate_credentials();
        let app = Router::new().route(
            "/auth/register",
            post(|| async {
                axum::Json(serde_json::json!({
                    "user_id": "user-123",
                    "username": "newuser",
                    "email": "a@b.com",
                    "display_name": null,
                    "access_token": "tok-new",
                    "refresh_token": "ref-new",
                    "token_type": "Bearer",
                    "expires_in": 3600
                }))
            }),
        );
        let base = spawn_mock_app(app).await;
        let api = astra_thin_client::ThinClient::new(&base, None).unwrap();
        let result = do_register(&api, Some("test-profile"), "newuser", "a@b.com", "pass").await;
        assert_eq!(result.unwrap(), "tok-new");

        let creds = load_credentials();
        let profile = creds.profiles.get("test-profile").unwrap();
        assert_eq!(profile.account_id.as_deref(), Some("user-123"));
        assert_eq!(profile.username.as_deref(), Some("newuser"));
        assert_eq!(profile.access_token.as_deref(), Some("tok-new"));
        assert_eq!(profile.refresh_token.as_deref(), Some("ref-new"));
    }

    #[serial_test::serial]
    #[tokio::test]
    async fn do_register_conflict_returns_error() {
        let _creds_dir = isolate_credentials();
        let app = Router::new().route(
            "/auth/register",
            post(|| async {
                (
                    axum::http::StatusCode::CONFLICT,
                    axum::Json(serde_json::json!({"detail": "username taken"})),
                )
            }),
        );
        let base = spawn_mock_app(app).await;
        let api = astra_thin_client::ThinClient::new(&base, None).unwrap();
        let result = do_register(&api, Some("test-profile"), "taken", "a@b.com", "pass").await;
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("409"));
    }

    // `stream_chat_sse` tests live in `tests/chat_stream_tests.rs` — the
    // canonical home since that module was extracted. An earlier incarnation
    // left the same three tests duplicated here, which meant every
    // `make test-offline` ran them twice under parallel contention and
    // contributed to the slow-case tail.

    // ── slash commands with mock server ───────────────────────────────────

    #[serial_test::serial]
    #[tokio::test]
    async fn slash_clear_creates_new_session() {
        let _creds_dir = isolate_credentials();
        let app = Router::new().route(
            "/sessions",
            post(|| async { axum::Json(serde_json::json!({"session_id": "new-sess-42"})) }),
        );
        let base = spawn_mock_app(app).await;
        let api = astra_thin_client::ThinClient::new(&base, None).unwrap();
        let mut state = SessionState {
            session_id: Some("old-sess".to_string()),
            turn: 5,
            history: vec![("q".to_string(), "a".to_string())],
            ..Default::default()
        };
        let exit = handle_slash_command("/clear", &api, None, &mut state, Some("fake-token"))
            .await
            .unwrap();
        assert!(!exit);
        assert_eq!(state.session_id.as_deref(), Some("new-sess-42"));
        assert_eq!(state.turn, 0);
        assert!(state.history.is_empty());
    }

    #[serial_test::serial]
    #[tokio::test]
    async fn slash_model_with_arg_sets_model() {
        let api = astra_thin_client::ThinClient::new("http://unused", None).unwrap();
        let mut state = SessionState::default();
        let exit = handle_slash_command("/model gpt-4o", &api, None, &mut state, None)
            .await
            .unwrap();
        assert!(!exit);
        assert_eq!(state.model.as_deref(), Some("gpt-4o"));
    }

    #[serial_test::serial]
    #[tokio::test]
    async fn slash_model_with_offering_id_preserves_selection_and_display_model() {
        let app = Router::new().route(
            "/models",
            get(|| async {
                axum::Json(serde_json::json!({
                    "items": [{
                        "offering_id": "offer-model",
                        "access_id": "self-hosted",
                        "access_kind": "self_hosted",
                        "access_label": "Self-hosted",
                        "execution_placement": "server",
                        "name": "Display Model",
                        "provider": "openai",
                        "description": null,
                        "is_active": true,
                        "context_window": 128000,
                        "max_completion_tokens": null,
                        "architecture": null,
                        "thinking_capability": null
                    }],
                    "next_cursor": null,
                    "limit": 50,
                    "total": 1,
                    "catalog_revision": "sha256:test"
                }))
            }),
        );
        let base = spawn_mock_app(app).await;
        let api = astra_thin_client::ThinClient::new(&base, None).unwrap();
        let mut state = SessionState::default();
        cli::slash::slash_config::set_active_offering_id_for_request(None);
        let exit = handle_slash_command(
            "/model offer-model",
            &api,
            None,
            &mut state,
            Some("fake-token"),
        )
        .await
        .unwrap();

        assert!(!exit);
        assert_eq!(state.model.as_deref(), Some("Display Model"));
        assert_eq!(
            cli::slash::slash_config::active_offering_id_for_request().as_deref(),
            Some("offer-model")
        );
        cli::slash::slash_config::set_active_offering_id_for_request(None);
    }

    #[serial_test::serial]
    #[tokio::test]
    async fn slash_model_with_token_does_not_update_state_when_model_list_fails() {
        let app = Router::new().route(
            "/models",
            get(|| async {
                (
                    axum::http::StatusCode::BAD_GATEWAY,
                    axum::Json(serde_json::json!({"detail": "provider catalog down"})),
                )
            }),
        );
        let base = spawn_mock_app(app).await;
        let api = astra_thin_client::ThinClient::new(&base, None).unwrap();
        let mut state = SessionState {
            model: Some("old-model".to_string()),
            ..Default::default()
        };
        cli::slash::slash_config::set_active_offering_id_for_request(Some("offer-old".to_string()));
        let exit = handle_slash_command(
            "/model offer-model",
            &api,
            None,
            &mut state,
            Some("fake-token"),
        )
        .await
        .unwrap();

        assert!(!exit);
        assert_eq!(state.model.as_deref(), Some("old-model"));
        assert_eq!(
            cli::slash::slash_config::active_offering_id_for_request().as_deref(),
            Some("offer-old")
        );
        cli::slash::slash_config::set_active_offering_id_for_request(None);
    }

    #[serial_test::serial]
    #[tokio::test]
    async fn slash_model_with_token_does_not_update_state_when_model_list_is_empty() {
        let app = Router::new().route(
            "/models",
            get(|| async {
                axum::Json(serde_json::json!({
                    "items": [],
                    "next_cursor": null,
                    "limit": 50,
                    "total": 0,
                    "catalog_revision": "sha256:empty"
                }))
            }),
        );
        let base = spawn_mock_app(app).await;
        let api = astra_thin_client::ThinClient::new(&base, None).unwrap();
        let mut state = SessionState {
            model: Some("old-model".to_string()),
            ..Default::default()
        };
        cli::slash::slash_config::set_active_offering_id_for_request(Some("offer-old".to_string()));
        let exit = handle_slash_command(
            "/model offer-model",
            &api,
            None,
            &mut state,
            Some("fake-token"),
        )
        .await
        .unwrap();

        assert!(!exit);
        assert_eq!(state.model.as_deref(), Some("old-model"));
        assert_eq!(
            cli::slash::slash_config::active_offering_id_for_request().as_deref(),
            Some("offer-old")
        );
        cli::slash::slash_config::set_active_offering_id_for_request(None);
    }

    #[tokio::test]
    async fn slash_exit_returns_true() {
        let api = astra_thin_client::ThinClient::new("http://unused", None).unwrap();
        let mut state = SessionState::default();
        let exit = handle_slash_command("/exit", &api, None, &mut state, None)
            .await
            .unwrap();
        assert!(exit);
    }

    // The `slash_exit_writes_session_end_to_journal` and
    // `slash_quit_writes_session_end_to_journal` tests previously
    // exercised `finalize_repl_exit`, which lived inside the
    // line-mode REPL exit path. Both the function and the path are
    // gone; session_end is now written by the TUI shutdown handler
    // and exercised by `tui::tests::*`.

    #[tokio::test]
    async fn slash_unknown_command_does_not_crash() {
        let api = astra_thin_client::ThinClient::new("http://unused", None).unwrap();
        let mut state = SessionState::default();
        let exit = handle_slash_command("/nonexistent_command_xyz", &api, None, &mut state, None)
            .await
            .unwrap();
        assert!(!exit);
    }

    #[tokio::test]
    async fn slash_health_does_not_crash_empty() {
        let api = astra_thin_client::ThinClient::new("http://unused", None).unwrap();
        let mut state = SessionState::default();
        // No health entries — should print "no data" gracefully
        let exit = handle_slash_command("/health", &api, None, &mut state, None)
            .await
            .unwrap();
        assert!(!exit);
    }

    #[tokio::test]
    async fn slash_health_with_entries_does_not_crash() {
        let api = astra_thin_client::ThinClient::new("http://unused", None).unwrap();
        let mut state = SessionState {
            tool_health_entries: vec![
                astra_turn_core::tool_health_persistence::ToolHealthEntry {
                    name: "bash".into(),
                    total_calls: 15,
                    total_failures: 3,
                    input_validation_failures: 0,
                    failure_rate: 0.2,
                    last_updated_epoch: 0,
                    recent_outcomes: vec![],
                },
                astra_turn_core::tool_health_persistence::ToolHealthEntry {
                    name: "grep".into(),
                    total_calls: 8,
                    total_failures: 0,
                    input_validation_failures: 0,
                    failure_rate: 0.0,
                    last_updated_epoch: 0,
                    recent_outcomes: vec![],
                },
            ],
            ..Default::default()
        };
        let exit = handle_slash_command("/health", &api, None, &mut state, None)
            .await
            .unwrap();
        assert!(!exit);
    }

    #[tokio::test]
    async fn slash_health_detail_mode() {
        let api = astra_thin_client::ThinClient::new("http://unused", None).unwrap();
        let mut state = SessionState {
            tool_health_entries: vec![astra_turn_core::tool_health_persistence::ToolHealthEntry {
                name: "bash".into(),
                total_calls: 10,
                total_failures: 5,
                input_validation_failures: 0,
                failure_rate: 0.5,
                last_updated_epoch: 0,
                recent_outcomes: vec![],
            }],
            ..Default::default()
        };
        let exit = handle_slash_command("/health detail", &api, None, &mut state, None)
            .await
            .unwrap();
        assert!(!exit);
    }

    // ── command_router ────────────────────────────────────────────────────

    #[serial_test::serial]
    #[tokio::test]
    async fn execute_cli_health_command() {
        let _creds_dir = isolate_credentials();
        let app = Router::new().route(
            "/health",
            get(|| async { axum::Json(serde_json::json!({"status": "ok"})) }),
        );
        let base = spawn_mock_app(app).await;
        let api = astra_thin_client::ThinClient::new(&base, None).unwrap();
        let result = execute_cli_command(
            Some(Command::Health),
            Some("nonexistent-profile".to_string()),
            None,
            false,
            None,
            &api,
            false,
            &cli::cli_config::cli_context::CliContext::default(),
        )
        .await;
        // Health command should succeed regardless of auth
        assert!(result.is_ok());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn execute_cli_messaging_bridge_command() {
        let api = astra_thin_client::ThinClient::new("http://unused", None).unwrap();
        let result = execute_cli_command(
            Some(Command::Messaging(MessagingArgs { command: None })),
            None,
            None,
            false,
            None,
            &api,
            false,
            &cli::cli_config::cli_context::CliContext::default(),
        )
        .await;
        assert!(result.is_ok());
    }

    #[serial_test::serial]
    #[tokio::test]
    async fn execute_cli_session_close_clears_matching_pointer_across_profiles() {
        let _creds_dir = isolate_credentials();
        let mut creds = CredentialsFile::default();
        creds.profiles.insert(
            "default".to_string(),
            Profile {
                account_id: Some("account-close".to_string()),
                access_token: Some("tok-default".to_string()),
                ..Default::default()
            },
        );
        creds.profiles.insert(
            "other".to_string(),
            Profile {
                last_session_id: Some("sess-close-1".to_string()),
                ..Default::default()
            },
        );
        save_credentials(&creds).unwrap();

        let app = Router::new().route(
            "/sessions/{id}/close",
            post(|| async { axum::Json(serde_json::json!({ "status": "closed" })) }),
        );
        let base = spawn_mock_app(app).await;
        let api = astra_thin_client::ThinClient::new(&base, None).unwrap();

        let result = execute_cli_command(
            Some(Command::Session(SessionCmd::Close(SessionShowArgs {
                session_id: "sess-close-1".to_string(),
            }))),
            None,
            None,
            false,
            None,
            &api,
            false,
            &cli::cli_config::cli_context::CliContext::default(),
        )
        .await;

        result.expect("bound profile should authorize session close");
        let creds = load_credentials();
        assert_eq!(
            creds.profiles["other"].last_session_id.as_deref(),
            None,
            "close should clear matching stale pointer even when another profile holds it"
        );
    }

    #[serial_test::serial]
    #[tokio::test]
    async fn execute_cli_session_delete_clears_matching_pointer_across_profiles() {
        let _creds_dir = isolate_credentials();
        let mut creds = CredentialsFile::default();
        creds.profiles.insert(
            "default".to_string(),
            Profile {
                account_id: Some("account-delete".to_string()),
                access_token: Some("tok-default".to_string()),
                ..Default::default()
            },
        );
        creds.profiles.insert(
            "other".to_string(),
            Profile {
                last_session_id: Some("sess-delete-1".to_string()),
                ..Default::default()
            },
        );
        save_credentials(&creds).unwrap();

        let app = Router::new().route("/sessions/{id}", axum::routing::delete(|| async { "" }));
        let base = spawn_mock(app).await;
        let api = astra_thin_client::ThinClient::new(&base, None).unwrap();

        let result = execute_cli_command(
            Some(Command::Session(SessionCmd::Delete(SessionShowArgs {
                session_id: "sess-delete-1".to_string(),
            }))),
            None,
            None,
            false,
            None,
            &api,
            false,
            &cli::cli_config::cli_context::CliContext::default(),
        )
        .await;

        result.expect("bound profile should authorize session delete");
        let creds = load_credentials();
        assert_eq!(
            creds.profiles["other"].last_session_id.as_deref(),
            None,
            "delete should clear matching stale pointer even when another profile holds it"
        );
    }

    #[tokio::test]
    async fn execute_cli_replay_rejects_invalid_session_id_before_auth() {
        let api = astra_thin_client::ThinClient::new("http://unused", None).unwrap();
        let error = execute_cli_command(
            Some(Command::Replay(ReplayArgs {
                session_id: "../escape".to_string(),
                sandbox_name: None,
                mock_mode: true,
                compare: false,
            })),
            None,
            None,
            false,
            None,
            &api,
            false,
            &cli::cli_config::cli_context::CliContext::default(),
        )
        .await
        .unwrap_err();

        assert!(error.contains("invalid session_id"), "got: {error}");
    }

    #[tokio::test]
    async fn execute_cli_session_close_rejects_invalid_session_id_before_auth() {
        let api = astra_thin_client::ThinClient::new("http://unused", None).unwrap();
        let error = execute_cli_command(
            Some(Command::Session(SessionCmd::Close(SessionShowArgs {
                session_id: "../escape".to_string(),
            }))),
            None,
            None,
            false,
            None,
            &api,
            false,
            &cli::cli_config::cli_context::CliContext::default(),
        )
        .await
        .unwrap_err();

        assert!(error.contains("invalid session_id"), "got: {error}");
    }

    #[tokio::test]
    async fn execute_cli_audit_show_rejects_invalid_session_id_before_auth() {
        let api = astra_thin_client::ThinClient::new("http://unused", None).unwrap();
        let error = execute_cli_command(
            Some(Command::Audit(AuditCmd::Show(AuditShowArgs {
                session_id: "../escape".to_string(),
            }))),
            None,
            None,
            false,
            None,
            &api,
            false,
            &cli::cli_config::cli_context::CliContext::default(),
        )
        .await
        .unwrap_err();

        assert!(error.contains("invalid session_id"), "got: {error}");
    }

    #[tokio::test]
    async fn execute_cli_audit_tools_rejects_invalid_optional_session_id_before_auth() {
        let api = astra_thin_client::ThinClient::new("http://unused", None).unwrap();
        let error = execute_cli_command(
            Some(Command::Audit(AuditCmd::Tools(AuditToolsArgs {
                session_id: Some("../escape".to_string()),
                since: None,
                until: None,
            }))),
            None,
            None,
            false,
            None,
            &api,
            false,
            &cli::cli_config::cli_context::CliContext::default(),
        )
        .await
        .unwrap_err();

        assert!(error.contains("invalid session_id"), "got: {error}");
    }

    // ── chat_turn pure functions ──────────────────────────────────────────

    // The `picker_submission_echo_*` tests verified line-mode-only
    // behaviour around the rustyline picker overlay; both
    // `should_clear_picker_submission_echo` and
    // `build_picker_submission_echo` are gone with the rest of the
    // line-mode REPL.

    #[test]
    fn prepare_input_keeps_plain_user_message() {
        let state = SessionState::default();
        let result = crate::cli::session::session_input::prepare_input(
            "hello",
            &state,
            &mut crate::cli::ui_adapter::LineUiAdapter,
        );
        assert_eq!(result.user_message, "hello");
        assert!(result.runtime_required_texts.is_empty());
    }

    #[test]
    fn prepare_input_routes_system_skills_out_of_user_message() {
        let mut state = SessionState::default();
        let skills = prompts::builtin_system_skills();
        if let Some(md) = skills.iter().find(|s| s.name == "markdown") {
            state.active_system_skills.push(md.clone());
        }
        let result = crate::cli::session::session_input::prepare_input(
            "hello",
            &state,
            &mut crate::cli::ui_adapter::LineUiAdapter,
        );
        assert_eq!(result.user_message, "hello");
        assert_eq!(result.active_system_skill_names, vec!["markdown"]);
        assert!(result.runtime_required_texts[0].contains("Markdown"));
    }

    #[test]
    fn history_as_messages_normal_turns() {
        let history = vec![
            ("q1".to_string(), "a1".to_string()),
            ("q2".to_string(), "a2".to_string()),
        ];
        let msgs = cli::session::session_projection::history_as_messages(&history);
        assert_eq!(msgs.len(), 4);
        assert_eq!(msgs[0]["role"], "user");
        assert_eq!(msgs[1]["role"], "assistant");
    }

    #[test]
    fn history_as_messages_compacted_turn() {
        let history = vec![("".to_string(), "summary".to_string())];
        let msgs = cli::session::session_projection::history_as_messages(&history);
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0]["role"], "assistant");
    }

    // ── slash_memory mock ─────────────────────────────────────────────────

    #[tokio::test]
    async fn slash_memory_search_with_mock() {
        let app = Router::new().route(
            "/memory/search",
            post(|| async {
                axum::Json(serde_json::json!({
                    "results": [
                        {"content": "user prefers Rust", "memory_type": "profile", "score": 0.9}
                    ]
                }))
            }),
        );
        let base = spawn_mock(app).await;
        let api = astra_thin_client::ThinClient::new(&base, None).unwrap();
        let mut state = SessionState {
            session_id: Some("sess-1".to_string()),
            ..Default::default()
        };
        // This should not panic or error
        let result = handle_memory_domain_command(
            "/memory",
            "search rust preferences",
            &api,
            &mut state,
            Some("fake-token"),
        )
        .await;
        assert!(result.is_ok());
    }

    // ── Resume user verification ─────────────────────────────────────────────

    #[serial_test::serial]
    #[tokio::test]
    async fn resume_local_restore_rejects_unowned_session() {
        let _creds = isolate_credentials();
        use session_journal::JournalWriter;

        // Create a session with both journal AND workspace (what restore_session needs)
        let sid = format!("test-unowned-{}", uuid::Uuid::new_v4());

        // 1. Create journal
        let writer = JournalWriter::new(&sid).unwrap();
        writer
            .append(&session_journal::JournalEvent::session_start(
                Some(&sid),
                Some("gpt-4o"),
            ))
            .unwrap();
        writer
            .append(&session_journal::JournalEvent::turn(
                Some(&sid),
                1,
                None,
                "hello",
                "hi",
                0,
                5,
                3,
                50,
            ))
            .unwrap();
        drop(writer);

        // 2. Create workspace.yaml (required for local restore)
        let ws_dir = dirs::home_dir()
            .unwrap_or_else(|| std::path::PathBuf::from("."))
            .join(".astra")
            .join("sessions")
            .join(&sid);
        std::fs::create_dir_all(&ws_dir).unwrap();
        let ws_content = r#"session_id: test-unowned
cwd: /tmp
model: gpt-4o
created_at: "2024-01-01T00:00:00Z"
updated_at: "2024-01-01T00:00:00Z"
status: active
turn_count: 1
total_tokens_in: 5
total_tokens_out: 3
"#;
        std::fs::write(ws_dir.join("workspace.yaml"), ws_content).unwrap();

        // Now restore_session should find it
        let svc = astra_services::session_restore::HybridRestoreService::local_only();
        let result = svc.restore_local_session(&sid).await.unwrap();
        assert!(
            result.is_some(),
            "local restore should find session with workspace.yaml"
        );

        // Verify it's marked as local (not cloud)
        let restored = result.unwrap();
        assert!(!restored.restored_from_cloud, "should be local restore");

        // Note: The user ownership check in handle_resume_command only verifies
        // that the journal exists, not that the user owns it. This is a known limitation.
    }

    // ── Edge cases ───────────────────────────────────────────────────────────

    #[serial_test::serial]
    #[tokio::test]
    async fn resume_handles_malformed_workspace_yaml() {
        let _creds = isolate_credentials();

        let sid = format!("test-malformed-{}", uuid::Uuid::new_v4());

        // Create journal
        let writer = session_journal::JournalWriter::new(&sid).unwrap();
        writer
            .append(&session_journal::JournalEvent::session_start(
                Some(&sid),
                Some("gpt-4o"),
            ))
            .unwrap();
        drop(writer);

        // Create malformed workspace.yaml
        let ws_dir = dirs::home_dir()
            .unwrap_or_else(|| std::path::PathBuf::from("."))
            .join(".astra")
            .join("sessions")
            .join(&sid);
        std::fs::create_dir_all(&ws_dir).unwrap();
        std::fs::write(ws_dir.join("workspace.yaml"), "invalid: yaml: content: [").unwrap();

        // Malformed workspace now falls back to journal-only local restore.
        let svc = astra_services::session_restore::HybridRestoreService::local_only();
        let result = svc
            .restore_local_session(&sid)
            .await
            .unwrap()
            .expect("malformed workspace should still restore from journal");
        assert_eq!(result.session_id, sid);
        assert_eq!(result.turn_count, 0);
        assert_eq!(result.model.as_deref(), Some("gpt-4o"));
        assert_eq!(result.last_status, "local");
        assert!(!result.restored_from_cloud);
    }

    #[serial_test::serial]
    #[tokio::test]
    async fn resume_handles_missing_workspace() {
        let _creds = isolate_credentials();

        // Only journal, no workspace → local journal-only restore should still work.
        let sid = format!("test-no-ws-{}", uuid::Uuid::new_v4());
        let writer = session_journal::JournalWriter::new(&sid).unwrap();
        writer
            .append(&session_journal::JournalEvent::session_start(
                Some(&sid),
                Some("gpt-4o"),
            ))
            .unwrap();
        drop(writer);

        let svc = astra_services::session_restore::HybridRestoreService::local_only();
        let result = svc
            .restore_local_session(&sid)
            .await
            .unwrap()
            .expect("journal-only session should restore");
        assert_eq!(result.session_id, sid);
        assert_eq!(result.turn_count, 0);
        assert_eq!(result.model.as_deref(), Some("gpt-4o"));
        assert_eq!(result.last_status, "local");
        assert!(!result.restored_from_cloud);
    }

    // ── Checkpoint listing ───────────────────────────────────────────────────

    #[serial_test::serial]
    #[tokio::test]
    async fn resume_lists_checkpoints_for_session() {
        let _creds = isolate_credentials();

        let sid = format!("test-checkpoints-{}", uuid::Uuid::new_v4());

        // Create journal
        let writer = session_journal::JournalWriter::new(&sid).unwrap();
        writer
            .append(&session_journal::JournalEvent::session_start(
                Some(&sid),
                Some("gpt-4o"),
            ))
            .unwrap();
        drop(writer);

        // Create workspace
        let ws_dir = dirs::home_dir()
            .unwrap_or_else(|| std::path::PathBuf::from("."))
            .join(".astra")
            .join("sessions")
            .join(&sid);
        std::fs::create_dir_all(&ws_dir).unwrap();
        std::fs::write(
            ws_dir.join("workspace.yaml"),
            r#"session_id: test
cwd: /tmp
model: gpt-4o
created_at: "2024-01-01T00:00:00Z"
updated_at: "2024-01-01T00:00:00Z"
status: active
turn_count: 10
total_tokens_in: 1000
total_tokens_out: 500
"#,
        )
        .unwrap();

        // List checkpoints should return empty (no checkpoints created yet)
        let svc = astra_services::session_restore::HybridRestoreService::local_only();
        let ckpts = svc.list_local_checkpoints(&sid).await.unwrap();
        assert!(ckpts.is_empty(), "no checkpoints created yet");
    }

    // merge_learning_snapshot tests removed: the entity/pattern/calibration
    // learning subsystem has been deleted. Tool-health sync is exercised in
    // the tests below.

    // ── handle_stats_command ─────────────────────────────────────────────────

    #[test]
    fn stats_no_active_session_does_not_panic() {
        // state with no session_id → should not panic
        let state = super::SessionState::default();
        tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(slash_stats::handle_stats_command("", &state)); // current session mode, no session
    }

    #[test]
    fn stats_history_no_sessions_does_not_panic() {
        let (_tmp, _guard) = crate::tests::isolated_sessions_dir();
        let state = super::SessionState::default();
        tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(slash_stats::handle_stats_command("history", &state));
    }

    #[test]
    fn stats_current_session_reads_journal() {
        let _creds = isolate_credentials();
        use astra_services::session_analytics;

        // Create a real journal with known events
        let sid = format!("test-stats-{}", uuid::Uuid::new_v4());
        let writer = session_journal::JournalWriter::new(&sid).unwrap();
        writer
            .append(&session_journal::JournalEvent::session_start(
                Some(&sid),
                Some("gpt-4o"),
            ))
            .unwrap();
        writer
            .append(&session_journal::JournalEvent::turn(
                Some(&sid),
                1,
                Some("gpt-4o"),
                "hello",
                "hi",
                2,
                1000,
                500,
                1500,
            ))
            .unwrap();
        writer
            .append(&session_journal::JournalEvent::turn(
                Some(&sid),
                2,
                Some("gpt-4o"),
                "what is rust?",
                "a systems language",
                1,
                800,
                400,
                1200,
            ))
            .unwrap();
        drop(writer);

        // Verify the analytics layer computes correctly from these events
        let events = session_journal::read_journal(&sid).unwrap();
        let stats = session_analytics::compute_session_stats(&sid, &events);

        assert_eq!(stats.turn_count, 2);
        assert_eq!(stats.total_tokens_in, 1800);
        assert_eq!(stats.total_tokens_out, 900);
        assert_eq!(stats.total_tool_calls, 3);
        assert_eq!(stats.model, Some("gpt-4o".into()));
        assert_eq!(stats.avg_tokens_per_turn, 1350); // (1800+900)/2

        // Now verify handle_stats_command doesn't panic with this session
        let state = super::SessionState {
            session_id: Some(sid),
            ..Default::default()
        };
        tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(slash_stats::handle_stats_command("", &state));
    }

    #[test]
    fn stats_history_aggregates_multiple_sessions() {
        let _creds = isolate_credentials();
        use astra_services::session_analytics;

        // Create two sessions
        let sid1 = format!("test-stats-hist-a-{}", uuid::Uuid::new_v4());
        let sid2 = format!("test-stats-hist-b-{}", uuid::Uuid::new_v4());

        for sid in [&sid1, &sid2] {
            let writer = session_journal::JournalWriter::new(sid).unwrap();
            writer
                .append(&session_journal::JournalEvent::turn(
                    Some(sid),
                    1,
                    None,
                    "q",
                    "a",
                    1,
                    500,
                    250,
                    800,
                ))
                .unwrap();
            drop(writer);
        }

        let e1 = session_journal::read_journal(&sid1).unwrap();
        let e2 = session_journal::read_journal(&sid2).unwrap();
        let s1 = session_analytics::compute_session_stats(&sid1, &e1);
        let s2 = session_analytics::compute_session_stats(&sid2, &e2);
        let agg = session_analytics::aggregate_stats(&[s1, s2]);

        assert_eq!(agg.session_count, 2);
        assert_eq!(agg.total_turns, 2);
        assert_eq!(agg.total_tokens_in, 1000);
        assert_eq!(agg.total_tokens_out, 500);
    }

    // ── handle_tools_command ─────────────────────────────────────────────────

    #[test]
    fn tools_no_active_session_does_not_panic() {
        let state = super::SessionState::default();
        slash_tools::handle_tools_command(&state);
    }

    #[test]
    fn tools_session_with_no_tool_calls_does_not_panic() {
        let _creds = isolate_credentials();
        let sid = format!("test-tools-empty-{}", uuid::Uuid::new_v4());
        let writer = session_journal::JournalWriter::new(&sid).unwrap();
        writer
            .append(&session_journal::JournalEvent::turn(
                Some(&sid),
                1,
                None,
                "hello",
                "hi",
                0,
                100,
                50,
                500,
            ))
            .unwrap();
        drop(writer);

        let state = super::SessionState {
            session_id: Some(sid),
            ..Default::default()
        };
        slash_tools::handle_tools_command(&state);
    }

    #[test]
    fn tools_reads_tool_calls_from_journal() {
        let _creds = isolate_credentials();
        use astra_services::session_analytics;

        let sid = format!("test-tools-calls-{}", uuid::Uuid::new_v4());
        let writer = session_journal::JournalWriter::new(&sid).unwrap();

        let mut event = session_journal::JournalEvent::turn(
            Some(&sid),
            1,
            None,
            "run tests",
            "done",
            3,
            500,
            200,
            3000,
        );
        event.tool_calls = Some(vec![
            session_journal::ToolCallRecord {
                name: "bash".into(),
                ms: 1000,
                ok: true,
                error: None,
                input_bytes: Some(50),
                output_bytes: Some(200),
                args_preview: Some("npm test".into()),
                result_preview: None,
                file_path: None,
                surgically_removed: None,
                original_tool_name: None,
                ..Default::default()
            },
            session_journal::ToolCallRecord {
                name: "bash".into(),
                ms: 2000,
                ok: false,
                error: Some("exit code 1".into()),
                input_bytes: Some(30),
                output_bytes: Some(100),
                args_preview: Some("cargo build".into()),
                result_preview: None,
                file_path: None,
                surgically_removed: None,
                original_tool_name: None,
                ..Default::default()
            },
            session_journal::ToolCallRecord {
                name: "grep".into(),
                ms: 50,
                ok: true,
                error: None,
                input_bytes: Some(20),
                output_bytes: Some(500),
                args_preview: Some("/error/ in src/".into()),
                result_preview: None,
                file_path: None,
                surgically_removed: None,
                original_tool_name: None,
                ..Default::default()
            },
        ]);
        writer.append(&event).unwrap();
        drop(writer);

        // Verify analytics layer computes correctly
        let events = session_journal::read_journal(&sid).unwrap();
        let profiles = session_analytics::compute_tool_profiles(&events);

        assert_eq!(profiles.len(), 2);
        // sorted by total_ms descending: bash (3000ms) > grep (50ms)
        assert_eq!(profiles[0].name, "bash");
        assert_eq!(profiles[0].call_count, 2);
        assert_eq!(profiles[0].fail_count, 1);
        assert_eq!(profiles[0].total_ms, 3000);
        assert_eq!(profiles[0].min_ms, 1000);
        assert_eq!(profiles[0].max_ms, 2000);
        assert!((profiles[0].error_rate - 0.5).abs() < 0.01);
        assert_eq!(profiles[0].last_error, Some("exit code 1".into()));

        assert_eq!(profiles[1].name, "grep");
        assert_eq!(profiles[1].call_count, 1);
        assert_eq!(profiles[1].fail_count, 0);
        assert_eq!(profiles[1].error_rate, 0.0);

        // Verify handle_tools_command doesn't panic with this data
        let state = super::SessionState {
            session_id: Some(sid),
            ..Default::default()
        };
        slash_tools::handle_tools_command(&state);
    }

    // ── slash_health::format_sync_age tests ────────────────────────────────────────────

    #[test]
    fn format_sync_age_various_durations() {
        let now = chrono::Utc::now();
        let cases = [
            (now.to_rfc3339(), "s ago", "just now / seconds"),
            (
                (now - chrono::Duration::minutes(5)).to_rfc3339(),
                "m ago",
                "minutes",
            ),
            (
                (now - chrono::Duration::hours(2)).to_rfc3339(),
                "h ago",
                "hours",
            ),
            (
                (now - chrono::Duration::days(3)).to_rfc3339(),
                "d ago",
                "days",
            ),
            ("2020-01-01 00:00:00".to_string(), "d ago", "mysql datetime"),
        ];
        for (ts, expected, label) in cases {
            let age = slash_health::format_sync_age(&ts);
            assert!(
                age.contains(expected) || age == "just now",
                "{label}: expected '{expected}', got: {age}"
            );
        }
    }

    #[test]
    fn format_sync_age_unparseable_returns_raw() {
        let raw = "not-a-timestamp";
        let age = slash_health::format_sync_age(raw);
        assert_eq!(age, raw, "unparseable should return raw string");
    }

    #[test]
    fn display_sync_status_no_crash_all_none() {
        let status = astra_services::SyncStatus::default();
        // Just verify no panic — output goes to stderr
        slash_health::display_sync_status(&status);
    }

    #[test]
    fn display_sync_status_no_crash_full_data() {
        let status = astra_services::SyncStatus {
            preferences_last_sync: Some(chrono::Utc::now().to_rfc3339()),
            pending_pushes: 2,
            last_error: Some("connection reset by peer".into()),
            ..Default::default()
        };
        slash_health::display_sync_status(&status);
    }

    #[tokio::test]
    async fn slash_health_offline_shows_cloud_section() {
        let api = astra_thin_client::ThinClient::new("http://unused", None).unwrap();
        let mut state = SessionState::default();
        let exit = handle_slash_command("/health", &api, None, &mut state, None)
            .await
            .unwrap();
        assert!(!exit);
    }

    // ── /allow command tests ──

    #[test]
    fn permission_mode_set_mode() {
        let mut pm = cli::permission_manager::PermissionManager::with_project(
            false,
            &std::path::PathBuf::from("/tmp"),
        );
        assert_eq!(pm.mode(), permission_manager::PermissionMode::Prompt);
        pm.set_mode(cli::permission_manager::PermissionMode::AcceptEdits);
        assert_eq!(pm.mode(), permission_manager::PermissionMode::AcceptEdits);
        pm.set_mode(cli::permission_manager::PermissionMode::Auto);
        assert_eq!(pm.mode(), permission_manager::PermissionMode::Auto);
        pm.set_mode(cli::permission_manager::PermissionMode::Deny);
        assert_eq!(pm.mode(), permission_manager::PermissionMode::Deny);
    }

    #[test]
    fn permission_mode_roundtrip_parse() {
        for mode_str in &["auto", "bypass", "accept_edits", "plan", "prompt", "deny"] {
            let mode: permission_manager::PermissionMode = mode_str.parse().unwrap();
            assert_eq!(mode.to_string().to_lowercase(), *mode_str);
        }
    }

    #[test]
    fn session_state_cli_context_auto_approve_activates_bypass_mode() {
        let state = initialize_session_state(
            None,
            None,
            &cli::cli_config::cli_context::CliContext::from_launch_options(
                false,
                &[],
                &[],
                &[],
                true,
                None,
                None,
            )
            .expect("cli context"),
        );
        assert_eq!(
            state.perm_manager.mode(),
            permission_manager::PermissionMode::Bypass
        );
    }

    #[test]
    #[should_panic(expected = "invalid permission mode in CliContext")]
    fn session_state_invalid_permission_mode_does_not_fallback_to_prompt() {
        let context = cli::cli_config::cli_context::CliContext::from_launch_options(
            false,
            &[],
            &[],
            &[],
            false,
            None,
            None,
        )
        .expect("cli context")
        .with_permission_mode(Some("bogus".into()));

        let _ = initialize_session_state(None, None, &context);
    }

    #[test]
    fn session_state_cli_context_permission_mode_overrides_auto_approve() {
        let context = cli::cli_config::cli_context::CliContext::from_launch_options(
            false,
            &[],
            &[],
            &[],
            true,
            None,
            None,
        )
        .expect("cli context")
        .with_permission_mode(Some("plan".into()));

        let state = initialize_session_state(None, None, &context);

        assert_eq!(
            state.perm_manager.mode(),
            permission_manager::PermissionMode::Plan
        );
    }

    #[test]
    fn resolve_system_prompt_literal_text() {
        let result = resolve_system_prompt("You are a helpful assistant.".to_string());
        assert_eq!(result.unwrap(), "You are a helpful assistant.");
    }

    #[test]
    fn resolve_system_prompt_at_file_reads_content() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("prompt.txt");
        std::fs::write(&path, "Custom system prompt from file").unwrap();
        let result = resolve_system_prompt(format!("@{}", path.display()));
        assert_eq!(result.unwrap(), "Custom system prompt from file");
    }

    #[test]
    fn resolve_system_prompt_at_file_not_found() {
        let result = resolve_system_prompt("@/nonexistent/path/prompt.txt".to_string());
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .contains("cannot read system prompt file")
        );
    }

    #[test]
    fn resolve_system_prompt_at_file_empty_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("empty.txt");
        std::fs::write(&path, "").unwrap();
        let result = resolve_system_prompt(format!("@{}", path.display()));
        assert_eq!(result.unwrap(), "");
    }

    #[test]
    fn resolve_system_prompt_at_bare_is_error() {
        let result = resolve_system_prompt("@".to_string());
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("requires a file path"));
    }

    #[test]
    fn resolve_system_prompt_at_file_with_unicode() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("unicode.txt");
        std::fs::write(&path, "你好世界 🌍 مرحبا").unwrap();
        let result = resolve_system_prompt(format!("@{}", path.display()));
        assert_eq!(result.unwrap(), "你好世界 🌍 مرحبا");
    }

    #[test]
    fn resolve_system_prompt_at_file_with_newlines() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("multi.txt");
        std::fs::write(&path, "line1\nline2\nline3\n").unwrap();
        let result = resolve_system_prompt(format!("@{}", path.display()));
        assert_eq!(result.unwrap(), "line1\nline2\nline3\n");
    }

    #[test]
    fn resolve_system_prompt_no_at_prefix_passes_through() {
        let result = resolve_system_prompt("/some/path/prompt.txt".to_string());
        assert_eq!(result.unwrap(), "/some/path/prompt.txt");
    }

    #[test]
    fn resolve_system_prompt_at_file_large_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("large.txt");
        let content = "x".repeat(1_000_000);
        std::fs::write(&path, &content).unwrap();
        let result = resolve_system_prompt(format!("@{}", path.display()));
        assert_eq!(result.unwrap().len(), 1_000_000);
    }

    #[test]
    fn resolve_system_prompt_at_file_permission_denied() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("noperm.txt");
        std::fs::write(&path, "secret").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o000)).unwrap();
        let result = resolve_system_prompt(format!("@{}", path.display()));
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644));
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .contains("cannot read system prompt file")
        );
    }

    // ── project instructions tests ──

    #[test]
    fn discover_project_instructions_from_tempdir() {
        let dir = tempfile::tempdir().unwrap();
        let astra_dir = dir.path().join(".astra");
        std::fs::create_dir_all(&astra_dir).unwrap();
        std::fs::write(
            astra_dir.join("instructions.md"),
            "Always use Rust.\nPrefer async.",
        )
        .unwrap();

        let result = discover_instructions_from_paths(Some(dir.path()), None);
        let instructions = result.expect("should discover instructions");
        assert!(instructions.contains("Always use Rust."));
        assert!(instructions.contains("Prefer async."));
    }

    #[test]
    fn discover_project_instructions_empty_file_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        let astra_dir = dir.path().join(".astra");
        std::fs::create_dir_all(&astra_dir).unwrap();
        std::fs::write(astra_dir.join("instructions.md"), "   \n  \n").unwrap();

        let result = discover_instructions_from_paths(Some(dir.path()), None);
        assert!(result.is_none(), "empty file should return None");
    }

    #[test]
    fn discover_project_instructions_no_file_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        let result = discover_instructions_from_paths(Some(dir.path()), Some(dir.path()));
        assert!(result.is_none(), "no file should return None");
    }

    #[test]
    fn discover_project_instructions_combines_project_and_user() {
        let project = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let p_astra = project.path().join(".astra");
        let h_astra = home.path().join(".astra");
        std::fs::create_dir_all(&p_astra).unwrap();
        std::fs::create_dir_all(&h_astra).unwrap();
        std::fs::write(p_astra.join("instructions.md"), "Project rules").unwrap();
        std::fs::write(h_astra.join("instructions.md"), "Global rules").unwrap();

        let result = discover_instructions_from_paths(Some(project.path()), Some(home.path()));
        let instructions = result.expect("should combine both");
        assert!(instructions.contains("Project rules"));
        assert!(instructions.contains("Global rules"));
        // Project should come first
        let project_pos = instructions.find("Project rules").unwrap();
        let global_pos = instructions.find("Global rules").unwrap();
        assert!(project_pos < global_pos, "project should precede global");
    }

    #[test]
    fn discover_project_instructions_user_only() {
        let project = tempfile::tempdir().unwrap(); // no .astra dir
        let home = tempfile::tempdir().unwrap();
        let h_astra = home.path().join(".astra");
        std::fs::create_dir_all(&h_astra).unwrap();
        std::fs::write(h_astra.join("instructions.md"), "User-level rules").unwrap();

        let result = discover_instructions_from_paths(Some(project.path()), Some(home.path()));
        let instructions = result.expect("should find user-level");
        assert!(instructions.contains("User-level rules"));
    }

    #[test]
    fn format_project_instructions_wraps_in_tags() {
        let content = "Use tabs for indentation.";
        let formatted = format_project_instructions(content);
        assert!(formatted.starts_with("<project_instructions>"));
        assert!(formatted.ends_with("</project_instructions>"));
        assert!(formatted.contains(content));
    }

    #[test]
    fn prepare_input_routes_project_instructions_out_of_user_message() {
        let mut state = SessionState::default();
        state.project_instructions = Some("Always use Rust.".to_string());
        let result = crate::cli::session::session_input::prepare_input(
            "hello",
            &state,
            &mut crate::cli::ui_adapter::LineUiAdapter,
        );
        assert!(
            result.runtime_required_texts[0].contains("<project_instructions>"),
            "should wrap in tags"
        );
        assert!(result.runtime_required_texts[0].contains("Always use Rust."));
        assert_eq!(result.user_message, "hello");
    }

    #[test]
    fn prepare_input_has_no_runtime_context_when_none() {
        let state = SessionState::default();
        let result = crate::cli::session::session_input::prepare_input(
            "hello",
            &state,
            &mut crate::cli::ui_adapter::LineUiAdapter,
        );
        assert!(
            result.runtime_required_texts.is_empty(),
            "should not inject when None"
        );
        assert_eq!(result.user_message, "hello");
    }

    #[test]
    fn cli_no_instructions_flag() {
        let cli = Cli::try_parse_from(["astra", "--no-instructions"]).unwrap();
        assert!(cli.no_instructions);
    }

    #[test]
    fn discover_instructions_includes_knowledge_md() {
        let dir = tempfile::tempdir().unwrap();
        let astra_dir = dir.path().join(".astra");
        std::fs::create_dir_all(&astra_dir).unwrap();
        std::fs::write(astra_dir.join("instructions.md"), "Use Rust conventions").unwrap();
        std::fs::write(
            astra_dir.join("knowledge.md"),
            "# Project Knowledge\n\n- Always run clippy",
        )
        .unwrap();
        let result = discover_instructions_from_paths(Some(dir.path()), None).unwrap();
        assert!(result.contains("Use Rust conventions"));
        assert!(result.contains("Always run clippy"));
    }

    #[test]
    fn discover_instructions_knowledge_md_only() {
        let dir = tempfile::tempdir().unwrap();
        let astra_dir = dir.path().join(".astra");
        std::fs::create_dir_all(&astra_dir).unwrap();
        std::fs::write(astra_dir.join("knowledge.md"), "- Some learning").unwrap();
        let result = discover_instructions_from_paths(Some(dir.path()), None).unwrap();
        assert!(result.contains("Some learning"));
    }

    #[test]
    fn discover_instructions_empty_knowledge_md_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let astra_dir = dir.path().join(".astra");
        std::fs::create_dir_all(&astra_dir).unwrap();
        std::fs::write(astra_dir.join("knowledge.md"), "   ").unwrap();
        assert!(discover_instructions_from_paths(Some(dir.path()), None).is_none());
    }

    // append_to_knowledge_md tests moved to session_cleanup.rs

    #[test]
    fn discover_instructions_knowledge_md_capped_at_8kb() {
        let dir = tempfile::tempdir().unwrap();
        let astra_dir = dir.path().join(".astra");
        std::fs::create_dir_all(&astra_dir).unwrap();
        // Write a 12KB knowledge.md
        let big_content = "- ".to_string() + &"x".repeat(998) + "\n";
        let repeated = big_content.repeat(12); // ~12KB
        std::fs::write(astra_dir.join("knowledge.md"), &repeated).unwrap();
        let result = discover_instructions_from_paths(Some(dir.path()), None).unwrap();
        // The injected content should be capped — less than the full 12KB
        assert!(
            result.len() < repeated.len(),
            "should be capped below original size"
        );
        // But should have substantial content (at least 7KB from 8KB cap minus header)
        assert!(
            result.len() > 7000,
            "should retain most of 8KB cap, got {}",
            result.len()
        );
    }

    #[test]
    fn discover_instructions_knowledge_md_capped_at_8kb_cjk() {
        // Regression: truncation at byte offset must not panic on multi-byte chars.
        let dir = tempfile::tempdir().unwrap();
        let astra_dir = dir.path().join(".astra");
        std::fs::create_dir_all(&astra_dir).unwrap();
        // CJK chars are 3 bytes each. Build >8KB of CJK content.
        let cjk_line = "- 知识回流测试行内容填充\n"; // ~38 bytes per line
        let mut content = String::new();
        while content.len() < 12_000 {
            content.push_str(cjk_line);
        }
        std::fs::write(astra_dir.join("knowledge.md"), &content).unwrap();
        // This must not panic (previously did on non-char-boundary byte index)
        let result = discover_instructions_from_paths(Some(dir.path()), None).unwrap();
        assert!(result.len() > 5000, "should retain substantial CJK content");
        assert!(
            result.len() < content.len(),
            "should be capped below original"
        );
    }

    // session_end_extract_learnings tests moved to session_cleanup.rs
}
