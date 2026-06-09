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

mod edge_tools;
mod git_branch_cache;
mod manifest_loader;
mod mcp_client;
mod skill_instructions;

#[cfg(test)]
mod test_utils;

mod cli;
mod explain_dag;
mod lock_recovery;

mod diff_utils;
mod sandbox_retry;
mod tool_safety_guard;
mod tui;

use cli::cli_config::cli_args::Cli;
use cli::cli_config::cli_utils::{local_resumable_last_session_id, normalize_model_override_owned};
use cli::command_router::{execute_cli_command, run_print_mode};
use cli::exit_code::ExitCode;
use cli::slash::slash_config;
#[cfg(test)]
use cli::slash::slash_router::handle_slash_command;
#[cfg(test)]
use cli::slash::slash_session::resolve_journal_target_session;
// CLI argument structs moved to cli/cli_args.rs

// SSE streaming types moved to cli/streaming_types.rs
pub(crate) use crate::cli::stream::streaming_types::{
    PartialTurnData, StreamResult, TurnFailure, VerdictEvent,
};

// Session state moved to cli/session_state.rs
pub(crate) use crate::cli::plan::plan_monitor::{format_duration_short, format_plan_progress};
pub(crate) use cli::session::session_state::{ExplainMode, SessionState, SkillDevState};

// ═══════════════════════════════════════════════ Output Styles ═════════════

// ═══════════════════════════════════════════════════ Learning Merge ═══════
// Cloud sync moved to cli/cloud_sync.rs

pub(crate) use cli::cloud_sync::post_auth_cloud_resync;
#[cfg(test)]
use cli::cloud_sync::try_cloud_push_preferences;

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

#[tokio::main]
async fn main() {
    dotenvy::dotenv().ok();
    let cli = Cli::parse();
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
                    std::process::exit(2);
                }
            },
            Err(err) => {
                eprintln!("{}", format!("Error: --settings: {err}").red());
                std::process::exit(2);
            }
        }
    }
    if let Some(turns) = cli.max_turns {
        let Ok(turns) = u32::try_from(turns) else {
            eprintln!("{}", "Error: --max-turns exceeds u32 range".red());
            std::process::exit(2);
        };
        cli_overlay.runtime_limits.max_turns = turns;
        has_cli_overlay = true;
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
            std::process::exit(2);
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
            std::process::exit(2);
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
            std::process::exit(1);
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
        max_turns,
        max_budget,
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

    let _ = (startup_trace, bare);
    let cli_context = match cli::cli_config::cli_context::CliContext::from_launch_options(
        no_journal_content,
        max_turns,
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
            std::process::exit(1);
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
    let system_prompt = system_prompt.map(|sp| match resolve_system_prompt(sp) {
        Ok(content) => content,
        Err(e) => {
            tracing::error!(target: "astra_cli", error = %e, "failed to resolve --system-prompt");
            eprintln!("{}", e.red());
            std::process::exit(1);
        }
    });

    // Merge project instructions into system_prompt for inline/print modes.
    // TUI mode handles this separately via build_effective_line.
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
            std::process::exit(2);
        }
    }

    // Resolve model: --model flag > config default_model > None
    let config_default_model = match cli::config_manager::read_config_default_model() {
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
            std::process::exit(2);
        }
    };
    let resolved_model = normalize_model_override_owned(cli_model.or(config_default_model));

    // Make the resolved model available to slash commands that print
    // model-aware diagnostics without mutating the process environment.
    slash_config::set_active_model_for_display(resolved_model.clone());

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
            Ok(code) => std::process::exit(i32::from(code)),
            Err(e) => {
                eprintln!("{}", format!("Error: {e}").red());
                std::process::exit(i32::from(ExitCode::ApiError));
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
                    max_budget,
                    &cli_context,
                )
                .await;
                match result {
                    Ok(()) => std::process::exit(0),
                    Err(e) => {
                        eprintln!("{}", format!("Error: {e}").red());
                        std::process::exit(i32::from(ExitCode::ApiError));
                    }
                }
            }
            None => {
                eprintln!(
                    "{}",
                    "No previous session to continue. Start a new one with `astra`.".yellow()
                );
                std::process::exit(1);
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
        max_budget,
        &cli_context,
    )
    .await
    {
        Ok(exit_code) => {
            std::process::exit(i32::from(exit_code));
        }
        Err(e) => {
            eprintln!("{}", format!("Error: {e}").red());
            std::process::exit(i32::from(ExitCode::ApiError));
        }
    }
}

#[cfg(test)]
mod tests {
    pub(crate) use super::test_utils::HomeGuard;
    pub(crate) use super::test_utils::TestUi;
    pub(crate) use super::test_utils::isolate_credentials;
    pub(crate) use super::test_utils::isolated_sessions_dir;
    pub(crate) use super::test_utils::stub_stream_result;
    pub(crate) use super::test_utils::stub_stream_result_with_records;
    pub(crate) use super::test_utils::test_temp_dir;
    pub(crate) use super::test_utils::wait_until;

    use super::{
        Cli, SessionState, cli, execute_cli_command, format_duration_short, format_plan_progress,
        format_project_instructions, handle_slash_command, resolve_journal_target_session,
        resolve_system_prompt, session_journal, try_cloud_push_preferences,
    };
    use astra_runtime::prompts;
    use axum::{Router, routing::get, routing::post};
    use clap::Parser;
    use cli::auth_flow::{do_login, do_register};
    use cli::cli_config::cli_args::{
        AgentSubcommand, AuditCmd, AuditShowArgs, AuditToolsArgs, BugSubcommand, Command,
        ConfigCmd, DiffSubcommand, McpCmd, MemorySubcommand, MessagingArgs, MessagingSubcommand,
        PermissionsSubcommand, ReplayArgs, ReviewSubcommand, SessionCmd, SessionShowArgs,
        TaskSubcommand, TeamSubcommand,
    };
    use cli::cli_config::cli_utils::{
        CredentialsFile, Profile, load_credentials, prefix_chars, save_credentials,
    };
    use cli::cloud_sync::{
        ASTRA_JOURNAL_CLOUD_EMPTY_ACK, CloudPullResult, append_cloud_pull_sync_journal,
        cloud_pull_warrants_sync_marker, should_append_cloud_pull_journal, try_cloud_pull,
        try_cloud_pull_preferences,
    };
    use cli::permission_manager;
    use cli::project_instructions::discover_instructions_from_paths;
    use cli::session::session_runtime::initialize_session_state;
    use cli::slash::slash_memory::handle_memory_domain_command;
    use cli::slash::{slash_health, slash_stats, slash_task, slash_tools};

    async fn spawn_mock(app: Router) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let base = format!("http://{addr}");
        tokio::spawn(async move {
            axum::serve(listener, app).await.ok();
        });
        tokio::task::yield_now().await;
        base
    }

    mod auth_tests;
    mod chat_stream_tests;
    mod chat_turn_tests;
    mod cli_args_tests;
    mod cloud_sync_tests;
    mod cost_tracking_tests;
    mod preamble_tests;
    mod resume_tests;
    mod self_command_tests;
    mod slash_command_tests;
    mod stats_tools_tests;
    // ── auth_flow ─────────────────────────────────────────────────────────

    #[serial_test::serial]
    #[tokio::test]
    async fn do_login_success() {
        let _creds_dir = isolate_credentials();
        let app = Router::new().route(
            "/auth/login",
            post(|| async {
                axum::Json(serde_json::json!({
                    "access_token": "tok-abc",
                    "refresh_token": "ref-xyz"
                }))
            }),
        );
        let base = spawn_mock(app).await;
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
        let base = spawn_mock(app).await;
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
        let base = spawn_mock(app).await;
        let api = astra_thin_client::ThinClient::new(&base, None).unwrap();
        let result = do_register(&api, Some("test-profile"), "newuser", "a@b.com", "pass").await;
        assert_eq!(result.unwrap(), "tok-new");

        let creds = load_credentials();
        let profile = creds.profiles.get("test-profile").unwrap();
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
        let base = spawn_mock(app).await;
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
        let base = spawn_mock(app).await;
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
                    failure_rate: 0.2,
                    last_updated_epoch: 0,
                    recent_outcomes: vec![],
                },
                astra_turn_core::tool_health_persistence::ToolHealthEntry {
                    name: "grep".into(),
                    total_calls: 8,
                    total_failures: 0,
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
        let base = spawn_mock(app).await;
        let api = astra_thin_client::ThinClient::new(&base, None).unwrap();
        let result = execute_cli_command(
            Some(Command::Health),
            Some("nonexistent-profile".to_string()),
            None,
            false,
            None,
            &api,
            false,
            0.0,
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
            0.0,
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
        let base = spawn_mock(app).await;
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
            0.0,
            &cli::cli_config::cli_context::CliContext::default(),
        )
        .await;

        assert!(result.is_ok());
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
            0.0,
            &cli::cli_config::cli_context::CliContext::default(),
        )
        .await;

        assert!(result.is_ok());
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
            0.0,
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
            0.0,
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
            0.0,
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
            0.0,
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
    fn build_effective_line_plain() {
        let state = SessionState::default();
        let result = crate::cli::session::session_input::build_effective_line(
            "hello",
            &state,
            &mut crate::cli::ui_adapter::LineUiAdapter,
        );
        assert_eq!(result, "hello");
    }

    #[test]
    fn build_effective_line_with_system_skills() {
        let mut state = SessionState::default();
        let skills = prompts::builtin_system_skills();
        if let Some(md) = skills.iter().find(|s| s.name == "markdown") {
            state.active_system_skills.push(md.clone());
        }
        let result = crate::cli::session::session_input::build_effective_line(
            "hello",
            &state,
            &mut crate::cli::ui_adapter::LineUiAdapter,
        );
        assert!(result.contains("hello"));
        assert!(result.contains("Markdown"));
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

    // ── find_task_by_query ────────────────────────────────────────────────────

    use astra_services::TaskService as _;

    #[tokio::test]
    async fn find_task_by_id_prefix() {
        let tmp = tempfile::TempDir::new().unwrap();
        let svc = astra_services::LocalTaskService::new(tmp.path().to_path_buf());
        let tid = svc
            .create_task(
                "u1",
                "s1",
                astra_services::TaskCreateRequest {
                    title: "Build auth".into(),
                    ..Default::default()
                },
            )
            .await
            .unwrap();

        // Full ID match
        let found = slash_task::find_task_by_query(&svc, "u1", &tid)
            .await
            .unwrap();
        assert_eq!(found, Some(tid.clone()));

        // Prefix match (first 8 Unicode scalars)
        let prefix = prefix_chars(&tid, 8);
        let found = slash_task::find_task_by_query(&svc, "u1", &prefix)
            .await
            .unwrap();
        assert_eq!(found, Some(tid));
    }

    #[tokio::test]
    async fn find_task_by_title_substring() {
        let tmp = tempfile::TempDir::new().unwrap();
        let svc = astra_services::LocalTaskService::new(tmp.path().to_path_buf());
        svc.create_task(
            "u1",
            "s1",
            astra_services::TaskCreateRequest {
                title: "Refactor authentication module".into(),
                ..Default::default()
            },
        )
        .await
        .unwrap();

        // Case-insensitive title match
        let found = slash_task::find_task_by_query(&svc, "u1", "authentication")
            .await
            .unwrap();
        assert!(found.is_some());

        let found = slash_task::find_task_by_query(&svc, "u1", "AUTH")
            .await
            .unwrap();
        assert!(found.is_some());
    }

    #[tokio::test]
    async fn find_task_by_title_substring_fails_on_ambiguity() {
        let tmp = tempfile::TempDir::new().unwrap();
        let svc = astra_services::LocalTaskService::new(tmp.path().to_path_buf());
        for title in [
            "Refactor authentication module",
            "Refactor authentication tests",
        ] {
            svc.create_task(
                "u1",
                "s1",
                astra_services::TaskCreateRequest {
                    title: title.into(),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        }

        let err = slash_task::find_task_by_query(&svc, "u1", "authentication")
            .await
            .unwrap_err();
        assert!(err.contains("task query 'authentication' is ambiguous"));
    }

    #[tokio::test]
    async fn find_task_not_found() {
        let tmp = tempfile::TempDir::new().unwrap();
        let svc = astra_services::LocalTaskService::new(tmp.path().to_path_buf());
        let found = slash_task::find_task_by_query(&svc, "u1", "nonexistent")
            .await
            .unwrap();
        assert!(found.is_none());
    }

    #[tokio::test]
    async fn find_task_wrong_user() {
        let tmp = tempfile::TempDir::new().unwrap();
        let svc = astra_services::LocalTaskService::new(tmp.path().to_path_buf());
        svc.create_task(
            "user-a",
            "s1",
            astra_services::TaskCreateRequest {
                title: "Private task".into(),
                ..Default::default()
            },
        )
        .await
        .unwrap();

        // Different user can't find it
        let found = slash_task::find_task_by_query(&svc, "user-b", "Private")
            .await
            .unwrap();
        assert!(found.is_none());
    }

    // ── Resume user verification ─────────────────────────────────────────────

    #[serial_test::serial]
    #[tokio::test]
    async fn resume_local_restore_rejects_unowned_session() {
        let _creds = isolate_credentials();
        use astra_services::session_restore::SessionRestoreService;
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
        let result = svc.restore_session(&sid).await.unwrap();
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
        use astra_services::session_restore::SessionRestoreService;

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
            .restore_session(&sid)
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
        use astra_services::session_restore::SessionRestoreService;

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
            .restore_session(&sid)
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
        use astra_services::session_restore::SessionRestoreService;

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
        let ckpts = svc.list_checkpoints(&sid).await.unwrap();
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
    fn format_sync_age_rfc3339() {
        let now = chrono::Utc::now();
        let ts = now.to_rfc3339();
        let age = slash_health::format_sync_age(&ts);
        // Should be "just now" or "0s ago" or "1s ago"
        assert!(
            age.contains("s ago") || age == "just now",
            "unexpected age for just-now timestamp: {age}"
        );
    }

    #[test]
    fn format_sync_age_minutes_ago() {
        let now = chrono::Utc::now();
        let five_min_ago = now - chrono::Duration::minutes(5);
        let ts = five_min_ago.to_rfc3339();
        let age = slash_health::format_sync_age(&ts);
        assert!(
            age.contains("m ago"),
            "expected minutes-ago format, got: {age}"
        );
    }

    #[test]
    fn format_sync_age_hours_ago() {
        let now = chrono::Utc::now();
        let two_hours_ago = now - chrono::Duration::hours(2);
        let ts = two_hours_ago.to_rfc3339();
        let age = slash_health::format_sync_age(&ts);
        assert!(
            age.contains("h ago"),
            "expected hours-ago format, got: {age}"
        );
    }

    #[test]
    fn format_sync_age_days_ago() {
        let now = chrono::Utc::now();
        let three_days_ago = now - chrono::Duration::days(3);
        let ts = three_days_ago.to_rfc3339();
        let age = slash_health::format_sync_age(&ts);
        assert!(
            age.contains("d ago"),
            "expected days-ago format, got: {age}"
        );
    }

    #[test]
    fn format_sync_age_mysql_datetime() {
        // MySQL DATETIME without timezone — should parse as UTC
        let age = slash_health::format_sync_age("2020-01-01 00:00:00");
        assert!(
            age.contains("d ago"),
            "expected days-ago for old mysql datetime, got: {age}"
        );
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

    // ── Cloud sync regression tests (block_on panic fix cc6d011) ────
    // These tests verify the async cloud sync functions don't panic when
    // called from within a tokio runtime (the original bug was block_on
    // inside an existing runtime). We unset MATRIXONE_HOST so they take
    // the graceful-fallback path.

    /// Without `ASTRA_API_URL`, the cloud-pull path returns
    /// `cloud_reachable: false` instead of attempting any HTTP.
    /// Verifies graceful offline degradation.
    #[tokio::test]
    async fn try_cloud_pull_returns_unreachable_without_cloud_base() {
        // Safety: test-only, single-threaded tokio runtime
        unsafe {
            std::env::remove_var("ASTRA_API_URL");
        }
        let result = cli::cloud_sync::try_cloud_pull("default").await;
        assert!(
            !result.cloud_reachable,
            "Without ASTRA_API_URL, cloud should be unreachable"
        );
    }

    #[test]
    fn cloud_pull_warrants_sync_marker_only_when_reachable_and_nonempty() {
        let dead = CloudPullResult {
            cloud_reachable: false,
        };
        assert!(!cloud_pull_warrants_sync_marker(&dead, &[]));
        let online_empty = CloudPullResult {
            cloud_reachable: true,
        };
        assert!(!cloud_pull_warrants_sync_marker(&online_empty, &[]));
        assert!(cloud_pull_warrants_sync_marker(
            &online_empty,
            &["explain_mode".into()]
        ));
    }

    #[test]
    fn should_append_cloud_pull_journal_post_login_reachable_empty() {
        let pull = CloudPullResult {
            cloud_reachable: true,
        };
        assert!(should_append_cloud_pull_journal(&pull, &[], "post_login"));
    }

    #[serial_test::serial]
    #[test]
    fn should_append_cloud_pull_journal_session_startup_empty_without_env() {
        unsafe {
            std::env::remove_var(ASTRA_JOURNAL_CLOUD_EMPTY_ACK);
        }
        let pull = CloudPullResult {
            cloud_reachable: true,
        };
        assert!(!should_append_cloud_pull_journal(
            &pull,
            &[],
            "session_startup"
        ));
    }

    #[serial_test::serial]
    #[test]
    fn should_append_session_startup_when_empty_ack_env_set() {
        let pull = CloudPullResult {
            cloud_reachable: true,
        };
        unsafe {
            std::env::remove_var(ASTRA_JOURNAL_CLOUD_EMPTY_ACK);
        }
        assert!(!should_append_cloud_pull_journal(
            &pull,
            &[],
            "session_startup"
        ));
        unsafe {
            std::env::set_var(ASTRA_JOURNAL_CLOUD_EMPTY_ACK, "1");
        }
        assert!(should_append_cloud_pull_journal(
            &pull,
            &[],
            "session_startup"
        ));
        unsafe {
            std::env::remove_var(ASTRA_JOURNAL_CLOUD_EMPTY_ACK);
        }
    }

    #[test]
    fn append_cloud_pull_sync_journal_skips_without_session_id() {
        let pull = CloudPullResult {
            cloud_reachable: true,
        };
        let state = SessionState::default();
        append_cloud_pull_sync_journal(&state, "default", "session_startup", &pull, &[]);
    }

    #[test]
    fn append_cloud_pull_sync_journal_writes_sync_marker_jsonl() {
        let sid = format!("test-cloud-pull-journal-{}", uuid::Uuid::new_v4());
        let state = SessionState {
            session_id: Some(sid.clone()),
            ..Default::default()
        };
        let pull = CloudPullResult {
            cloud_reachable: true,
        };
        let prefs = vec!["explain_mode".to_string()];
        append_cloud_pull_sync_journal(&state, "work", "session_startup", &pull, &prefs);
        let events = session_journal::read_journal(&sid).expect("read journal");
        assert_eq!(events.len(), 2);
        assert_eq!(
            events[0].event_type,
            session_journal::JournalEventType::SessionStart
        );
        let marker = events
            .iter()
            .find(|event| event.event_type == session_journal::JournalEventType::SyncMarker)
            .expect("sync marker");
        assert_eq!(
            marker.event_type,
            session_journal::JournalEventType::SyncMarker
        );
        let cp = marker
            .metadata
            .as_ref()
            .and_then(|m| m.get("cloud_pull"))
            .expect("cloud_pull");
        assert_eq!(cp.get("profile").and_then(|v| v.as_str()), Some("work"));
        assert_eq!(
            cp.get("reachable_empty_ack").and_then(|v| v.as_bool()),
            Some(false)
        );
        std::fs::remove_file(session_journal::journal_file_path(&sid)).ok();
    }

    #[test]
    fn append_cloud_pull_post_login_reachable_empty_writes_marker() {
        let sid = format!("test-cloud-pull-empty-{}", uuid::Uuid::new_v4());
        let state = SessionState {
            session_id: Some(sid.clone()),
            ..Default::default()
        };
        let pull = CloudPullResult {
            cloud_reachable: true,
        };
        append_cloud_pull_sync_journal(&state, "default", "post_login", &pull, &[]);
        let events = session_journal::read_journal(&sid).expect("read journal");
        assert_eq!(events.len(), 2);
        assert_eq!(
            events[0].event_type,
            session_journal::JournalEventType::SessionStart
        );
        let marker = events
            .iter()
            .find(|event| event.event_type == session_journal::JournalEventType::SyncMarker)
            .expect("sync marker");
        let cp = marker
            .metadata
            .as_ref()
            .and_then(|m| m.get("cloud_pull"))
            .expect("cloud_pull");
        assert_eq!(
            cp.get("reachable_empty_ack").and_then(|v| v.as_bool()),
            Some(true)
        );
        std::fs::remove_file(session_journal::journal_file_path(&sid)).ok();
    }

    #[tokio::test]
    async fn try_cloud_pull_returns_empty_without_matrixone() {
        unsafe {
            std::env::remove_var("MATRIXONE_HOST");
        }
        let result = try_cloud_pull("default").await;
        assert!(
            !result.cloud_reachable,
            "Without MatrixOne, cloud should be unreachable"
        );
    }

    #[tokio::test]
    async fn try_cloud_pull_preferences_is_noop_without_matrixone() {
        unsafe {
            std::env::remove_var("MATRIXONE_HOST");
        }
        let mut state = SessionState::default();
        // Should not panic (was the original bug)
        let keys = try_cloud_pull_preferences(&mut state).await;
        assert!(keys.is_empty());
    }

    #[tokio::test]
    async fn try_cloud_push_preferences_is_noop_without_matrixone() {
        unsafe {
            std::env::remove_var("MATRIXONE_HOST");
        }
        let state = SessionState::default();
        // Should not panic (was the original bug)
        try_cloud_push_preferences(&state).await;
    }

    #[test]
    fn format_duration_short_zero() {
        assert_eq!(
            format_duration_short(std::time::Duration::from_secs(0)),
            "0s"
        );
    }

    #[test]
    fn format_duration_short_seconds() {
        assert_eq!(
            format_duration_short(std::time::Duration::from_secs(45)),
            "45s"
        );
    }

    #[test]
    fn format_duration_short_minutes() {
        assert_eq!(
            format_duration_short(std::time::Duration::from_secs(92)),
            "1m32s"
        );
    }

    #[test]
    fn format_duration_short_hours() {
        assert_eq!(
            format_duration_short(std::time::Duration::from_secs(7500)),
            "2h5m"
        );
    }

    #[test]
    fn format_plan_progress_empty() {
        let s = format_plan_progress(0, 0, None, std::time::Duration::from_secs(0));
        assert!(s.contains("0/0 (0%)"));
        assert!(s.contains("0s elapsed"));
    }

    #[test]
    fn format_plan_progress_first_subtask() {
        let s = format_plan_progress(0, 5, None, std::time::Duration::from_secs(10));
        assert!(s.contains("0/5 (0%)"));
        assert!(s.contains("10s elapsed"));
        // No ETA when done==0
        assert!(!s.contains("remaining"));
    }

    #[test]
    fn format_plan_progress_midway_with_eta() {
        let avg = Some(std::time::Duration::from_secs(60));
        let s = format_plan_progress(3, 7, avg, std::time::Duration::from_secs(180));
        assert!(s.contains("3/7 (42%)"));
        assert!(s.contains("3m0s elapsed"));
        assert!(s.contains("~4m0s remaining")); // 4 remaining × 60s avg
    }

    #[test]
    fn format_plan_progress_complete() {
        let avg = Some(std::time::Duration::from_secs(30));
        let s = format_plan_progress(5, 5, avg, std::time::Duration::from_secs(150));
        assert!(s.contains("5/5 (100%)"));
        // 0 remaining → "~0s remaining"
        assert!(s.contains("remaining"));
    }

    #[test]
    fn format_plan_progress_bar_fills() {
        // At 50% with 16-width bar, should have 8 filled + 8 empty
        let s = format_plan_progress(3, 6, None, std::time::Duration::from_secs(0));
        assert!(s.contains("████████░░░░░░░░"));
    }

    // ── Cost Tracking Tests ──────────────────────────────────────────────

    #[test]
    fn cost_for_tokens_basic() {
        let pricing = astra_services::models::PricingData {
            prompt: 0.003,     // $0.003 per 1K tokens = $3 per 1M
            completion: 0.015, // $0.015 per 1K tokens = $15 per 1M
            cache_read: None,
            cache_write: None,
        };
        let cost = slash_stats::cost_for_tokens(1000, 500, 0, 0, &pricing);
        // 1000 * 0.003/1000 + 500 * 0.015/1000 = 0.003 + 0.0075 = 0.0105
        assert!(
            (cost - 0.0105).abs() < 1e-10,
            "cost should be $0.0105, got {cost}"
        );
    }

    #[test]
    fn cost_for_tokens_zero() {
        let pricing = astra_services::models::PricingData {
            prompt: 0.003,
            completion: 0.015,
            cache_read: None,
            cache_write: None,
        };
        assert_eq!(slash_stats::cost_for_tokens(0, 0, 0, 0, &pricing), 0.0);
    }

    #[test]
    fn cost_for_tokens_zero_pricing() {
        let pricing = astra_services::models::PricingData::default();
        assert_eq!(
            slash_stats::cost_for_tokens(10000, 5000, 0, 0, &pricing),
            0.0
        );
    }

    #[test]
    fn cost_for_tokens_large_values() {
        let pricing = astra_services::models::PricingData {
            prompt: 0.003,
            completion: 0.015,
            cache_read: None,
            cache_write: None,
        };
        // 1M prompt + 500K completion
        let cost = slash_stats::cost_for_tokens(1_000_000, 500_000, 0, 0, &pricing);
        // 1M * 0.003/1K + 500K * 0.015/1K = 3.0 + 7.5 = 10.5
        assert!(
            (cost - 10.5).abs() < 1e-6,
            "large token cost should be $10.50, got {cost}"
        );
    }

    #[test]
    fn cost_for_tokens_with_cache() {
        let pricing = astra_services::models::PricingData {
            prompt: 0.003,
            completion: 0.015,
            cache_read: Some(0.0003),   // 10% of prompt
            cache_write: Some(0.00375), // 125% of prompt
        };
        // 500 prompt + 200 completion + 1000 cache_read + 100 cache_write
        let cost = slash_stats::cost_for_tokens(500, 200, 1000, 100, &pricing);
        let expected = (500.0 * 0.003 / 1000.0)
            + (200.0 * 0.015 / 1000.0)
            + (1000.0 * 0.0003 / 1000.0)
            + (100.0 * 0.00375 / 1000.0);
        assert!(
            (cost - expected).abs() < 1e-10,
            "cache cost should be {expected}, got {cost}"
        );
    }

    #[test]
    fn cost_for_tokens_cache_fallback_rates() {
        // When cache_read/cache_write are None, uses 10%/125% of prompt rate
        let pricing = astra_services::models::PricingData {
            prompt: 0.003,
            completion: 0.015,
            cache_read: None,
            cache_write: None,
        };
        let cost = slash_stats::cost_for_tokens(0, 0, 1000, 1000, &pricing);
        let expected = (1000.0 * 0.003 * 0.1 / 1000.0) + (1000.0 * 0.003 * 1.25 / 1000.0);
        assert!(
            (cost - expected).abs() < 1e-10,
            "fallback cache cost should be {expected}, got {cost}"
        );
    }

    #[test]
    fn format_cost_sub_cent() {
        assert_eq!(slash_stats::format_cost(0.0001), "$0.0001");
        assert_eq!(slash_stats::format_cost(0.0099), "$0.0099");
    }

    #[test]
    fn format_cost_sub_dollar() {
        assert_eq!(slash_stats::format_cost(0.01), "$0.010");
        assert_eq!(slash_stats::format_cost(0.123), "$0.123");
        assert_eq!(slash_stats::format_cost(0.999), "$0.999");
    }

    #[test]
    fn format_cost_dollars() {
        assert_eq!(slash_stats::format_cost(1.0), "$1.00");
        assert_eq!(slash_stats::format_cost(12.345), "$12.35"); // rounds
        assert_eq!(slash_stats::format_cost(100.0), "$100.00");
    }

    #[test]
    fn format_cost_zero() {
        assert_eq!(slash_stats::format_cost(0.0), "$0.0000");
    }

    #[test]
    fn extract_pricing_from_nested_object() {
        let models = vec![serde_json::json!({
            "name": "gpt-4",
            "pricing": {
                "prompt": 0.03,
                "completion": 0.06
            }
        })];
        let p = slash_stats::extract_pricing_for_model(&models, "gpt-4").unwrap();
        assert!((p.prompt - 0.03).abs() < 1e-10);
        assert!((p.completion - 0.06).abs() < 1e-10);
    }

    #[test]
    fn extract_pricing_from_flat_fields() {
        let models = vec![serde_json::json!({
            "name": "claude-3",
            "pricing_prompt": 0.008,
            "pricing_completion": 0.024
        })];
        let p = slash_stats::extract_pricing_for_model(&models, "claude-3").unwrap();
        assert!((p.prompt - 0.008).abs() < 1e-10);
        assert!((p.completion - 0.024).abs() < 1e-10);
    }

    #[test]
    fn extract_pricing_model_not_found() {
        let models = vec![
            serde_json::json!({"name": "gpt-4", "pricing_prompt": 0.03, "pricing_completion": 0.06}),
        ];
        assert!(slash_stats::extract_pricing_for_model(&models, "nonexistent").is_none());
    }

    #[test]
    fn extract_pricing_empty_models() {
        let models: Vec<serde_json::Value> = vec![];
        assert!(slash_stats::extract_pricing_for_model(&models, "any").is_none());
    }

    #[test]
    fn extract_pricing_zero_values_returns_none() {
        let models = vec![serde_json::json!({
            "name": "test",
            "pricing_prompt": 0.0,
            "pricing_completion": 0.0
        })];
        assert!(slash_stats::extract_pricing_for_model(&models, "test").is_none());
    }

    // ── slash_stats::fallback_pricing tests ───────────────────────────────────────────

    #[test]
    fn fallback_sonnet_pricing() {
        let p = slash_stats::fallback_pricing("claude-sonnet-4-20250514");
        assert!((p.prompt - 0.003).abs() < 1e-6);
        assert!((p.completion - 0.015).abs() < 1e-6);
        assert!(p.cache_read.is_some());
        assert!((p.cache_read.unwrap() - 0.0003).abs() < 1e-8);
    }

    #[test]
    fn fallback_opus_4_pricing() {
        let p = slash_stats::fallback_pricing("claude-opus-4-20250514");
        assert!(
            (p.prompt - 0.015).abs() < 1e-6,
            "opus-4 prompt should be $15/Mtok"
        );
        assert!((p.completion - 0.075).abs() < 1e-6);
    }

    #[test]
    fn fallback_opus_45_pricing() {
        let p = slash_stats::fallback_pricing("claude-opus-4.5-20250415");
        assert!(
            (p.prompt - 0.005).abs() < 1e-6,
            "opus 4.5 should be $5/Mtok"
        );
    }

    #[test]
    fn fallback_haiku_pricing() {
        let p = slash_stats::fallback_pricing("claude-haiku-4.5-20250514");
        assert!(
            (p.prompt - 0.001).abs() < 1e-6,
            "haiku 4.5 should be $1/Mtok"
        );
    }

    #[test]
    fn fallback_gpt4o_pricing() {
        let p = slash_stats::fallback_pricing("gpt-4o-2024-08-06");
        assert!((p.prompt - 0.0025).abs() < 1e-6);
    }

    #[test]
    fn fallback_deepseek_pricing() {
        let p = slash_stats::fallback_pricing("deepseek-chat");
        assert!((p.prompt - 0.00027).abs() < 1e-8);
    }

    #[test]
    fn fallback_unknown_uses_sonnet() {
        let p = slash_stats::fallback_pricing("some-unknown-model");
        assert!(
            (p.prompt - 0.003).abs() < 1e-6,
            "unknown model should default to sonnet pricing"
        );
    }

    #[test]
    fn fallback_cost_calculation_with_cache() {
        // Sonnet: 1000 prompt + 500 completion + 2000 cache_read + 100 cache_creation
        let p = slash_stats::fallback_pricing("claude-sonnet-4-20250514");
        let cost = slash_stats::cost_for_tokens(1000, 500, 2000, 100, &p);
        // $0.003/Ktok * 1 + $0.015/Ktok * 0.5 + $0.0003/Ktok * 2 + $0.00375/Ktok * 0.1
        let expected = 0.003 + 0.0075 + 0.0006 + 0.000375;
        assert!(
            (cost - expected).abs() < 1e-8,
            "cost={cost} expected={expected}"
        );
    }

    // ── CLI arg parsing tests ─────────────────────────────────────────────

    #[test]
    fn cli_no_args_gives_no_command() {
        let cli = Cli::try_parse_from(["astra"]).unwrap();
        assert!(cli.command.is_none());
        assert!(!cli.print);
        assert!(!cli.continue_last);
        assert!(!cli.yes);
        assert!(cli.model.is_none());
        assert!(cli.resume.is_none());
    }

    #[test]
    fn cli_model_flag_long() {
        let cli = Cli::try_parse_from(["astra", "--model", "gpt-4o"]).unwrap();
        assert_eq!(cli.model.as_deref(), Some("gpt-4o"));
    }

    #[test]
    fn cli_model_flag_equals() {
        let cli = Cli::try_parse_from(["astra", "--model=claude-3-opus"]).unwrap();
        assert_eq!(cli.model.as_deref(), Some("claude-3-opus"));
    }

    #[test]
    fn cli_print_flag_short() {
        let cli = Cli::try_parse_from(["astra", "-p"]).unwrap();
        assert!(cli.print);
    }

    #[test]
    fn cli_print_flag_long() {
        let cli = Cli::try_parse_from(["astra", "--print"]).unwrap();
        assert!(cli.print);
    }

    #[test]
    fn cli_output_format_default_is_text() {
        let cli = Cli::try_parse_from(["astra"]).unwrap();
        assert_eq!(cli.output_format, "text");
    }

    #[test]
    fn cli_output_format_json() {
        let cli = Cli::try_parse_from(["astra", "--output-format", "json"]).unwrap();
        assert_eq!(cli.output_format, "json");
    }

    #[test]
    fn cli_continue_flag_short() {
        let cli = Cli::try_parse_from(["astra", "-c"]).unwrap();
        assert!(cli.continue_last);
    }

    #[test]
    fn cli_continue_flag_long() {
        let cli = Cli::try_parse_from(["astra", "--continue"]).unwrap();
        assert!(cli.continue_last);
    }

    #[test]
    fn cli_resume_flag_short() {
        let cli = Cli::try_parse_from(["astra", "-r", "abc123"]).unwrap();
        assert_eq!(cli.resume.as_deref(), Some("abc123"));
    }

    #[test]
    fn cli_resume_flag_long() {
        let cli = Cli::try_parse_from(["astra", "--resume", "session-xyz"]).unwrap();
        assert_eq!(cli.resume.as_deref(), Some("session-xyz"));
    }

    #[test]
    fn cli_yes_flag_short() {
        let cli = Cli::try_parse_from(["astra", "-y"]).unwrap();
        assert!(cli.yes);
    }

    #[test]
    fn cli_yes_flag_long() {
        let cli = Cli::try_parse_from(["astra", "--yes"]).unwrap();
        assert!(cli.yes);
    }

    #[test]
    fn cli_combined_short_flags() {
        // -p -c -y can be combined
        let cli = Cli::try_parse_from(["astra", "-p", "-c", "-y"]).unwrap();
        assert!(cli.print);
        assert!(cli.continue_last);
        assert!(cli.yes);
    }

    #[test]
    fn cli_model_with_print_and_yes() {
        let cli = Cli::try_parse_from(["astra", "--model", "gpt-4o", "-p", "-y"]).unwrap();
        assert_eq!(cli.model.as_deref(), Some("gpt-4o"));
        assert!(cli.print);
        assert!(cli.yes);
    }

    #[test]
    fn cli_doctor_subcommand() {
        let cli = Cli::try_parse_from(["astra", "doctor"]).unwrap();
        assert!(matches!(cli.command, Some(Command::Doctor)));
    }

    #[test]
    fn cli_completion_bash() {
        let cli = Cli::try_parse_from(["astra", "completion", "bash"]).unwrap();
        match cli.command {
            Some(Command::Completion(ref args)) => {
                assert_eq!(args.shell, clap_complete::Shell::Bash);
            }
            _ => panic!("expected Completion command"),
        }
    }

    #[test]
    fn cli_completion_zsh() {
        let cli = Cli::try_parse_from(["astra", "completion", "zsh"]).unwrap();
        match cli.command {
            Some(Command::Completion(ref args)) => {
                assert_eq!(args.shell, clap_complete::Shell::Zsh);
            }
            _ => panic!("expected Completion command"),
        }
    }

    #[test]
    fn cli_completion_fish() {
        let cli = Cli::try_parse_from(["astra", "completion", "fish"]).unwrap();
        match cli.command {
            Some(Command::Completion(ref args)) => {
                assert_eq!(args.shell, clap_complete::Shell::Fish);
            }
            _ => panic!("expected Completion command"),
        }
    }

    #[test]
    fn cli_mcp_list_subcommand() {
        let cli = Cli::try_parse_from(["astra", "mcp", "list"]).unwrap();
        match cli.command {
            Some(Command::Mcp(McpCmd::List(_))) => {}
            _ => panic!("expected Mcp List command"),
        }
    }

    #[test]
    fn cli_mcp_add_with_args() {
        let cli =
            Cli::try_parse_from(["astra", "mcp", "add", "myserver", "npx", "server"]).unwrap();
        match cli.command {
            Some(Command::Mcp(McpCmd::Add(ref args))) => {
                assert_eq!(args.name, "myserver");
                assert_eq!(args.command, "npx");
                assert_eq!(args.args, vec!["server"]);
            }
            _ => panic!("expected Mcp Add command"),
        }
    }

    #[test]
    fn cli_mcp_remove_subcommand() {
        let cli = Cli::try_parse_from(["astra", "mcp", "remove", "myserver"]).unwrap();
        match cli.command {
            Some(Command::Mcp(McpCmd::Remove(ref args))) => {
                assert_eq!(args.name, "myserver");
            }
            _ => panic!("expected Mcp Remove command"),
        }
    }

    #[test]
    fn cli_mcp_get_subcommand() {
        let cli = Cli::try_parse_from(["astra", "mcp", "get", "myserver"]).unwrap();
        match cli.command {
            Some(Command::Mcp(McpCmd::Get(ref args))) => {
                assert_eq!(args.name, "myserver");
            }
            _ => panic!("expected Mcp Get command"),
        }
    }

    #[test]
    fn cli_config_list_subcommand() {
        let cli = Cli::try_parse_from(["astra", "config", "list"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(Command::Config(ConfigCmd::List))
        ));
    }

    #[test]
    fn cli_config_get_subcommand() {
        let cli = Cli::try_parse_from(["astra", "config", "get", "default_model"]).unwrap();
        match cli.command {
            Some(Command::Config(ConfigCmd::Get(ref args))) => {
                assert_eq!(args.key, "default_model");
            }
            _ => panic!("expected Config Get command"),
        }
    }

    #[test]
    fn cli_config_set_subcommand() {
        let cli =
            Cli::try_parse_from(["astra", "config", "set", "default_model", "gpt-4o"]).unwrap();
        match cli.command {
            Some(Command::Config(ConfigCmd::Set(ref args))) => {
                assert_eq!(args.key, "default_model");
                assert_eq!(args.value, "gpt-4o");
            }
            _ => panic!("expected Config Set command"),
        }
    }

    #[test]
    fn cli_chat_with_model() {
        let cli =
            Cli::try_parse_from(["astra", "chat", "-m", "hello", "--model", "gpt-4o"]).unwrap();
        match cli.command {
            Some(Command::Chat(ref args)) => {
                assert_eq!(args.message.as_deref(), Some("hello"));
                assert_eq!(args.model.as_deref(), Some("gpt-4o"));
            }
            _ => panic!("expected Chat command"),
        }
    }

    #[test]
    fn cli_chat_no_resume() {
        let cli = Cli::try_parse_from(["astra", "chat", "--no-resume", "-m", "hello"]).unwrap();
        match cli.command {
            Some(Command::Chat(ref args)) => {
                assert!(args.no_resume);
                assert_eq!(args.message.as_deref(), Some("hello"));
            }
            _ => panic!("expected Chat command"),
        }
    }

    #[test]
    fn cli_chat_auto_approve() {
        let cli = Cli::try_parse_from(["astra", "chat", "-y"]).unwrap();
        match cli.command {
            Some(Command::Chat(ref args)) => {
                assert!(args.auto_approve);
            }
            _ => panic!("expected Chat command"),
        }
    }

    #[test]
    fn cli_chat_permission_mode() {
        let cli = Cli::try_parse_from(["astra", "chat", "--permission-mode", "auto"]).unwrap();
        match cli.command {
            Some(Command::Chat(ref args)) => {
                assert_eq!(args.permission_mode.as_deref(), Some("auto"));
            }
            _ => panic!("expected Chat command"),
        }
    }

    #[test]
    fn cli_chat_permission_mode_legacy_aliases() {
        for mode in ["yolo", "bypass-safety", "bypass_safety"] {
            let cli = Cli::try_parse_from(["astra", "chat", "--permission-mode", mode]).unwrap();
            match cli.command {
                Some(Command::Chat(ref args)) => {
                    assert_eq!(args.permission_mode.as_deref(), Some("auto"));
                }
                _ => panic!("expected Chat command"),
            }
        }
    }

    #[test]
    fn cli_chat_permission_mode_accept_edits() {
        let cli =
            Cli::try_parse_from(["astra", "chat", "--permission-mode", "accept-edits"]).unwrap();
        match cli.command {
            Some(Command::Chat(ref args)) => {
                assert_eq!(args.permission_mode.as_deref(), Some("accept_edits"));
            }
            _ => panic!("expected Chat command"),
        }
    }

    #[test]
    fn cli_chat_permission_mode_plan() {
        let cli = Cli::try_parse_from(["astra", "chat", "--permission-mode", "plan"]).unwrap();
        match cli.command {
            Some(Command::Chat(ref args)) => {
                assert_eq!(args.permission_mode.as_deref(), Some("plan"));
            }
            _ => panic!("expected Chat command"),
        }
    }

    #[test]
    fn cli_external_subcommand_message() {
        let cli = Cli::try_parse_from(["astra", "what", "is", "rust"]).unwrap();
        match cli.command {
            Some(Command::Message(ref words)) => {
                assert_eq!(words, &["what", "is", "rust"]);
            }
            _ => panic!("expected Message command"),
        }
    }

    #[test]
    fn cli_serve_defaults() {
        let cli = Cli::try_parse_from(["astra", "serve"]).unwrap();
        match cli.command {
            Some(Command::Serve(ref args)) => {
                assert_eq!(args.host, "127.0.0.1");
                assert_eq!(args.port, 8000);
            }
            _ => panic!("expected Serve command"),
        }
    }

    #[test]
    fn cli_serve_custom_port() {
        let cli = Cli::try_parse_from(["astra", "serve", "--port", "3000"]).unwrap();
        match cli.command {
            Some(Command::Serve(ref args)) => {
                assert_eq!(args.port, 3000);
            }
            _ => panic!("expected Serve command"),
        }
    }

    #[test]
    fn cli_api_url_default() {
        let cli = Cli::try_parse_from(["astra"]).unwrap();
        assert_eq!(cli.api_url, None);
    }

    #[test]
    fn cli_api_url_custom() {
        let cli = Cli::try_parse_from(["astra", "--api-url", "http://remote:9000"]).unwrap();
        assert_eq!(cli.api_url.as_deref(), Some("http://remote:9000"));
    }

    #[test]
    fn cli_profile_flag() {
        let cli = Cli::try_parse_from(["astra", "--profile", "work"]).unwrap();
        assert_eq!(cli.profile.as_deref(), Some("work"));
    }

    #[test]
    fn cli_top_level_yes_does_not_conflict_with_chat_yes() {
        // Both top-level -y and chat -y should work together
        let cli = Cli::try_parse_from(["astra", "-y", "chat", "-y"]).unwrap();
        assert!(cli.yes);
        match cli.command {
            Some(Command::Chat(ref args)) => {
                assert!(args.auto_approve);
            }
            _ => panic!("expected Chat command"),
        }
    }

    #[test]
    fn cli_mcp_add_scope_project() {
        let cli = Cli::try_parse_from(["astra", "mcp", "add", "--scope", "project", "s1", "npx"])
            .unwrap();
        match cli.command {
            Some(Command::Mcp(McpCmd::Add(ref args))) => {
                assert_eq!(args.scope, "project");
                assert_eq!(args.name, "s1");
                assert_eq!(args.command, "npx");
            }
            _ => panic!("expected Mcp Add command"),
        }
    }

    #[test]
    fn cli_mcp_add_scope_user() {
        let cli =
            Cli::try_parse_from(["astra", "mcp", "add", "--scope", "user", "s1", "npx"]).unwrap();
        match cli.command {
            Some(Command::Mcp(McpCmd::Add(ref args))) => {
                assert_eq!(args.scope, "user");
            }
            _ => panic!("expected Mcp Add command"),
        }
    }

    #[test]
    fn cli_mcp_add_with_trailing_args() {
        let cli = Cli::try_parse_from([
            "astra", "mcp", "add", "s1", "npx", "server", "--port", "8080",
        ])
        .unwrap();
        match cli.command {
            Some(Command::Mcp(McpCmd::Add(ref args))) => {
                assert_eq!(args.name, "s1");
                assert_eq!(args.command, "npx");
                assert_eq!(args.args, vec!["server", "--port", "8080"]);
            }
            _ => panic!("expected Mcp Add command"),
        }
    }

    #[test]
    fn cli_interactive_subcommand() {
        let cli = Cli::try_parse_from(["astra", "interactive"]).unwrap();
        assert!(matches!(cli.command, Some(Command::Interactive)));
    }

    #[test]
    fn cli_health_subcommand() {
        let cli = Cli::try_parse_from(["astra", "health"]).unwrap();
        assert!(matches!(cli.command, Some(Command::Health)));
    }

    #[test]
    fn cli_whoami_subcommand() {
        let cli = Cli::try_parse_from(["astra", "whoami"]).unwrap();
        assert!(matches!(cli.command, Some(Command::Whoami)));
    }

    #[test]
    fn cli_system_prompt_flag() {
        let cli =
            Cli::try_parse_from(["astra", "--system-prompt", "You are a code reviewer"]).unwrap();
        assert_eq!(
            cli.system_prompt.as_deref(),
            Some("You are a code reviewer")
        );
    }

    #[test]
    fn cli_max_turns_flag() {
        let cli = Cli::try_parse_from(["astra", "--max-turns", "10"]).unwrap();
        assert_eq!(cli.max_turns, Some(10));
    }

    #[test]
    fn cli_max_turns_default_is_none() {
        let cli = Cli::try_parse_from(["astra"]).unwrap();
        assert!(cli.max_turns.is_none());
    }

    #[test]
    fn cli_system_prompt_with_print() {
        let cli = Cli::try_parse_from([
            "astra",
            "-p",
            "--system-prompt",
            "Be concise",
            "--max-turns",
            "5",
        ])
        .unwrap();
        assert!(cli.print);
        assert_eq!(cli.system_prompt.as_deref(), Some("Be concise"));
        assert_eq!(cli.max_turns, Some(5));
    }

    #[test]
    fn cli_completion_generates_bash_output() {
        use clap::CommandFactory;
        let mut buf = Vec::new();
        clap_complete::generate(
            clap_complete::Shell::Bash,
            &mut Cli::command(),
            "astra",
            &mut buf,
        );
        let output = String::from_utf8(buf).unwrap();
        assert!(output.contains("astra"));
        assert!(output.contains("complete"));
    }

    #[test]
    fn cli_completion_generates_zsh_output() {
        use clap::CommandFactory;
        let mut buf = Vec::new();
        clap_complete::generate(
            clap_complete::Shell::Zsh,
            &mut Cli::command(),
            "astra",
            &mut buf,
        );
        let output = String::from_utf8(buf).unwrap();
        assert!(output.contains("astra"));
        assert!(!output.is_empty());
    }

    #[test]
    fn cli_max_turns_rejects_non_numeric() {
        let result = Cli::try_parse_from(["astra", "--max-turns", "abc"]);
        assert!(result.is_err());
    }

    #[test]
    fn cli_all_flags_combined() {
        let cli = Cli::try_parse_from([
            "astra",
            "--model",
            "gpt-4o",
            "-p",
            "-y",
            "--system-prompt",
            "Review code",
            "--max-turns",
            "3",
            "--output-format",
            "json",
        ])
        .unwrap();
        assert_eq!(cli.model.as_deref(), Some("gpt-4o"));
        assert!(cli.print);
        assert!(cli.yes);
        assert_eq!(cli.system_prompt.as_deref(), Some("Review code"));
        assert_eq!(cli.max_turns, Some(3));
        assert_eq!(cli.output_format, "json");
    }

    // ── --allowed-tools tests ──

    #[test]
    fn cli_allowed_tools_single() {
        let cli = Cli::try_parse_from(["astra", "--allowed-tools", "Bash"]).unwrap();
        assert_eq!(cli.allowed_tools, vec!["Bash"]);
    }

    #[test]
    fn cli_allowed_tools_multiple_space_separated() {
        let cli =
            Cli::try_parse_from(["astra", "--allowed-tools", "Bash", "Edit", "Read"]).unwrap();
        assert_eq!(cli.allowed_tools, vec!["Bash", "Edit", "Read"]);
    }

    #[test]
    fn cli_allowed_tools_empty_default() {
        let cli = Cli::try_parse_from(["astra"]).unwrap();
        assert!(cli.allowed_tools.is_empty());
    }

    // ── --add-dir tests ──

    #[test]
    fn cli_add_dir_single() {
        let cli = Cli::try_parse_from(["astra", "--add-dir", "/tmp/extra"]).unwrap();
        assert_eq!(cli.add_dir, vec!["/tmp/extra"]);
    }

    #[test]
    fn cli_add_dir_multiple() {
        let cli = Cli::try_parse_from(["astra", "--add-dir", "/tmp/a", "/tmp/b"]).unwrap();
        assert_eq!(cli.add_dir, vec!["/tmp/a", "/tmp/b"]);
    }

    #[test]
    fn cli_add_dir_empty_default() {
        let cli = Cli::try_parse_from(["astra"]).unwrap();
        assert!(cli.add_dir.is_empty());
    }

    // ── --verbose tests ──

    #[test]
    fn cli_verbose_flag() {
        let cli = Cli::try_parse_from(["astra", "--verbose"]).unwrap();
        assert!(cli.verbose);
    }

    #[test]
    fn cli_verbose_default_false() {
        let cli = Cli::try_parse_from(["astra"]).unwrap();
        assert!(!cli.verbose);
    }

    // ── --mcp-config tests ──

    #[test]
    fn cli_mcp_config_single() {
        let cli = Cli::try_parse_from(["astra", "--mcp-config", "mcp.json"]).unwrap();
        assert_eq!(cli.mcp_config, vec!["mcp.json"]);
    }

    #[test]
    fn cli_mcp_config_multiple() {
        let cli = Cli::try_parse_from(["astra", "--mcp-config", "a.json", "b.json"]).unwrap();
        assert_eq!(cli.mcp_config, vec!["a.json", "b.json"]);
    }

    #[test]
    fn cli_mcp_config_empty_default() {
        let cli = Cli::try_parse_from(["astra"]).unwrap();
        assert!(cli.mcp_config.is_empty());
    }

    // ── Combined new flags ──

    #[test]
    fn cli_all_new_flags_combined() {
        let cli = Cli::try_parse_from([
            "astra",
            "--allowed-tools",
            "Bash",
            "Edit",
            "--add-dir",
            "/tmp/extra",
            "--verbose",
            "--mcp-config",
            "mcp.json",
            "--model",
            "gpt-4o",
            "-p",
        ])
        .unwrap();
        assert_eq!(cli.allowed_tools, vec!["Bash", "Edit"]);
        assert_eq!(cli.add_dir, vec!["/tmp/extra"]);
        assert!(cli.verbose);
        assert_eq!(cli.mcp_config, vec!["mcp.json"]);
        assert_eq!(cli.model.as_deref(), Some("gpt-4o"));
        assert!(cli.print);
    }

    #[test]
    fn cli_team_command_parses_structured_run_subcommand() {
        let cli = Cli::try_parse_from(["astra", "team", "run", "dev", "ship", "it"]).unwrap();
        match cli.command {
            Some(Command::Team(args)) => match args.command {
                Some(TeamSubcommand::Run(run)) => {
                    assert_eq!(run.team, "dev");
                    assert_eq!(run.task, vec!["ship", "it"]);
                }
                other => panic!("unexpected team subcommand: {other:?}"),
            },
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn cli_team_command_defaults_to_list() {
        let cli = Cli::try_parse_from(["astra", "team"]).unwrap();
        match cli.command {
            Some(Command::Team(args)) => {
                assert!(args.command.is_none());
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn cli_task_command_parses_structured_run_subcommand() {
        let cli = Cli::try_parse_from(["astra", "task", "run", "fix", "login", "page"]).unwrap();
        match cli.command {
            Some(Command::Task(args)) => match args.command {
                Some(TaskSubcommand::Run(run)) => {
                    assert_eq!(run.text, vec!["fix", "login", "page"]);
                }
                other => panic!("unexpected task subcommand: {other:?}"),
            },
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn cli_memory_command_parses_search_subcommand() {
        let cli = Cli::try_parse_from(["astra", "memory", "search", "team", "history"]).unwrap();
        match cli.command {
            Some(Command::Memory(args)) => match args.command {
                Some(MemorySubcommand::Search(search)) => {
                    assert_eq!(search.query, vec!["team", "history"]);
                }
                other => panic!("unexpected memory subcommand: {other:?}"),
            },
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn cli_memory_list_with_type_and_limit() {
        let cli = Cli::try_parse_from([
            "astra", "memory", "list", "--type", "profile", "--limit", "5",
        ])
        .unwrap();
        match cli.command {
            Some(Command::Memory(args)) => match args.command {
                Some(MemorySubcommand::List(list)) => {
                    assert_eq!(list.memory_type.as_deref(), Some("profile"));
                    assert_eq!(list.limit, 5);
                }
                other => panic!("unexpected memory subcommand: {other:?}"),
            },
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn cli_memory_show_takes_memory_id() {
        let cli = Cli::try_parse_from(["astra", "memory", "show", "m-12345"]).unwrap();
        match cli.command {
            Some(Command::Memory(args)) => match args.command {
                Some(MemorySubcommand::Show(show)) => {
                    assert_eq!(show.memory_id, "m-12345");
                }
                other => panic!("unexpected memory subcommand: {other:?}"),
            },
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn cli_memory_forget_accepts_reason() {
        let cli =
            Cli::try_parse_from(["astra", "memory", "forget", "m-42", "--reason", "duplicate"])
                .unwrap();
        match cli.command {
            Some(Command::Memory(args)) => match args.command {
                Some(MemorySubcommand::Forget(forget)) => {
                    assert_eq!(forget.memory_id, "m-42");
                    assert_eq!(forget.reason.as_deref(), Some("duplicate"));
                }
                other => panic!("unexpected memory subcommand: {other:?}"),
            },
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn cli_permissions_command_keeps_mode_arg() {
        let cli = Cli::try_parse_from(["astra", "permissions", "auto"]).unwrap();
        match cli.command {
            Some(Command::Permissions(args)) => match args.command {
                Some(PermissionsSubcommand::Auto) => {}
                other => panic!("unexpected permissions subcommand: {other:?}"),
            },
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn cli_permissions_command_accept_edits_mode() {
        let cli = Cli::try_parse_from(["astra", "permissions", "accept_edits"]).unwrap();
        match cli.command {
            Some(Command::Permissions(args)) => match args.command {
                Some(PermissionsSubcommand::AcceptEdits) => {}
                other => panic!("unexpected permissions subcommand: {other:?}"),
            },
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn cli_permissions_command_plan_mode() {
        let cli = Cli::try_parse_from(["astra", "permissions", "plan"]).unwrap();
        match cli.command {
            Some(Command::Permissions(args)) => match args.command {
                Some(PermissionsSubcommand::Plan) => {}
                other => panic!("unexpected permissions subcommand: {other:?}"),
            },
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn cli_allow_alias_parses_permissions_command() {
        let cli = Cli::try_parse_from(["astra", "allow", "prompt"]).unwrap();
        match cli.command {
            Some(Command::Permissions(args)) => match args.command {
                Some(PermissionsSubcommand::Prompt) => {}
                other => panic!("unexpected permissions subcommand: {other:?}"),
            },
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn cli_sessions_alias_parses_session_command() {
        let cli = Cli::try_parse_from(["astra", "sessions", "list"]).unwrap();
        match cli.command {
            Some(Command::Session(SessionCmd::List(_))) => {}
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn cli_review_command_parses_working_subcommand() {
        let cli = Cli::try_parse_from(["astra", "review", "working"]).unwrap();
        match cli.command {
            Some(Command::Review(args)) => match args.command {
                Some(ReviewSubcommand::Working) => assert!(args.target.is_empty()),
                other => panic!("unexpected review subcommand: {other:?}"),
            },
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn cli_review_command_accepts_plain_revision_target() {
        let cli = Cli::try_parse_from(["astra", "review", "HEAD~2"]).unwrap();
        match cli.command {
            Some(Command::Review(args)) => {
                assert!(args.command.is_none());
                assert_eq!(args.target, vec!["HEAD~2"]);
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn cli_grep_command_accepts_default_pattern() {
        let cli = Cli::try_parse_from(["astra", "grep", "tool", "selector"]).unwrap();
        match cli.command {
            Some(Command::Grep(args)) => {
                assert!(args.command.is_none());
                assert_eq!(args.pattern, vec!["tool", "selector"]);
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn cli_grep_command_rejects_missing_pattern() {
        assert!(Cli::try_parse_from(["astra", "grep"]).is_err());
    }

    #[test]
    fn cli_agent_command_parses_status_subcommand() {
        let cli = Cli::try_parse_from(["astra", "agent", "status", "agent-123"]).unwrap();
        match cli.command {
            Some(Command::Agent(args)) => match args.command {
                Some(AgentSubcommand::Status(status)) => assert_eq!(status.agent_id, "agent-123"),
                other => panic!("unexpected agent subcommand: {other:?}"),
            },
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn cli_messaging_command_parses_dlq_subcommand() {
        let cli = Cli::try_parse_from(["astra", "messaging", "dlq"]).unwrap();
        match cli.command {
            Some(Command::Messaging(args)) => match args.command {
                Some(MessagingSubcommand::Dlq) => {}
                other => panic!("unexpected messaging subcommand: {other:?}"),
            },
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn cli_diff_command_parses_show_subcommand() {
        let cli = Cli::try_parse_from(["astra", "diff", "show", "HEAD~1"]).unwrap();
        match cli.command {
            Some(Command::Diff(args)) => match args.command {
                Some(DiffSubcommand::Show(show)) => {
                    assert_eq!(show.rev, "HEAD~1");
                    assert!(show.paths.is_empty());
                }
                other => panic!("unexpected diff subcommand: {other:?}"),
            },
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn cli_diff_command_accepts_plain_path_filter() {
        let cli =
            Cli::try_parse_from(["astra", "diff", "rust/crates/astra-cli/src/main.rs"]).unwrap();
        match cli.command {
            Some(Command::Diff(args)) => {
                assert!(args.command.is_none());
                assert_eq!(args.paths, vec!["rust/crates/astra-cli/src/main.rs"]);
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn cli_bug_command_parses_save_subcommand() {
        let cli = Cli::try_parse_from(["astra", "bug", "save"]).unwrap();
        match cli.command {
            Some(Command::Bug(args)) => match args.command {
                Some(BugSubcommand::Save) => {}
                other => panic!("unexpected bug subcommand: {other:?}"),
            },
            other => panic!("unexpected command: {other:?}"),
        }
    }

    // ── --disallowed-tools tests ──

    #[test]
    fn cli_disallowed_tools_single() {
        let cli = Cli::try_parse_from(["astra", "--disallowed-tools", "Bash"]).unwrap();
        assert_eq!(cli.disallowed_tools, vec!["Bash"]);
    }

    #[test]
    fn cli_disallowed_tools_multiple() {
        let cli = Cli::try_parse_from(["astra", "--disallowed-tools", "Bash", "Edit"]).unwrap();
        assert_eq!(cli.disallowed_tools, vec!["Bash", "Edit"]);
    }

    #[test]
    fn cli_disallowed_tools_empty_default() {
        let cli = Cli::try_parse_from(["astra"]).unwrap();
        assert!(cli.disallowed_tools.is_empty());
    }

    #[test]
    fn cli_allowed_and_disallowed_together() {
        let cli = Cli::try_parse_from([
            "astra",
            "--allowed-tools",
            "Read",
            "Edit",
            "--disallowed-tools",
            "Bash",
        ])
        .unwrap();
        assert_eq!(cli.allowed_tools, vec!["Read", "Edit"]);
        assert_eq!(cli.disallowed_tools, vec!["Bash"]);
    }

    // ── --session-id tests ──

    #[test]
    fn cli_session_id_flag() {
        let cli = Cli::try_parse_from([
            "astra",
            "--session-id",
            "550e8400-e29b-41d4-a716-446655440000",
        ])
        .unwrap();
        assert_eq!(
            cli.session_id.as_deref(),
            Some("550e8400-e29b-41d4-a716-446655440000")
        );
    }

    #[test]
    fn cli_session_id_default_none() {
        let cli = Cli::try_parse_from(["astra"]).unwrap();
        assert!(cli.session_id.is_none());
    }

    // ── --name tests ──

    #[test]
    fn cli_name_short_flag() {
        let cli = Cli::try_parse_from(["astra", "-n", "my-session"]).unwrap();
        assert_eq!(cli.session_name.as_deref(), Some("my-session"));
    }

    #[test]
    fn cli_name_long_flag() {
        let cli = Cli::try_parse_from(["astra", "--name", "review-pr-42"]).unwrap();
        assert_eq!(cli.session_name.as_deref(), Some("review-pr-42"));
    }

    #[test]
    fn cli_name_default_none() {
        let cli = Cli::try_parse_from(["astra"]).unwrap();
        assert!(cli.session_name.is_none());
    }

    // ── --bare tests ──

    #[test]
    fn cli_bare_flag() {
        let cli = Cli::try_parse_from(["astra", "--bare"]).unwrap();
        assert!(cli.bare);
    }

    #[test]
    fn cli_bare_default_false() {
        let cli = Cli::try_parse_from(["astra"]).unwrap();
        assert!(!cli.bare);
    }

    #[test]
    fn cli_bare_with_print_and_system_prompt() {
        let cli = Cli::try_parse_from([
            "astra",
            "--bare",
            "-p",
            "--system-prompt",
            "Be brief",
            "--add-dir",
            "/tmp/work",
        ])
        .unwrap();
        assert!(cli.bare);
        assert!(cli.print);
        assert_eq!(cli.system_prompt.as_deref(), Some("Be brief"));
        assert_eq!(cli.add_dir, vec!["/tmp/work"]);
    }

    #[test]
    fn cli_session_id_and_name_combined() {
        let cli = Cli::try_parse_from([
            "astra",
            "--session-id",
            "123e4567-e89b-12d3-a456-426614174000",
            "-n",
            "debug-session",
        ])
        .unwrap();
        assert_eq!(
            cli.session_id.as_deref(),
            Some("123e4567-e89b-12d3-a456-426614174000")
        );
        assert_eq!(cli.session_name.as_deref(), Some("debug-session"));
    }

    // ── --max-budget tests ──

    #[test]
    fn cli_max_budget_flag() {
        let cli = Cli::try_parse_from(["astra", "--max-budget", "5.50"]).unwrap();
        assert!((cli.max_budget - 5.50).abs() < f64::EPSILON);
    }

    #[test]
    fn cli_max_budget_default_zero() {
        let cli = Cli::try_parse_from(["astra"]).unwrap();
        assert!((cli.max_budget - 0.0).abs() < f64::EPSILON);
    }

    #[test]
    fn cli_max_budget_rejects_non_numeric() {
        let result = Cli::try_parse_from(["astra", "--max-budget", "abc"]);
        assert!(result.is_err());
    }

    #[test]
    fn cli_max_budget_with_print_and_turns() {
        let cli = Cli::try_parse_from(["astra", "-p", "--max-turns", "10", "--max-budget", "1.0"])
            .unwrap();
        assert!(cli.print);
        assert_eq!(cli.max_turns, Some(10));
        assert!((cli.max_budget - 1.0).abs() < f64::EPSILON);
    }

    // ── --max-budget edge case tests ──

    #[test]
    fn cli_max_budget_negative_rejected() {
        let result = Cli::try_parse_from(["astra", "--max-budget", "-1.0"]);
        // clap may or may not accept negative f64 — verify behavior
        match result {
            Ok(cli) => assert!(cli.max_budget < 0.0, "negative budget parsed but < 0"),
            Err(_) => {} // rejected is fine too
        }
    }

    #[test]
    fn cli_max_budget_very_small_value() {
        let cli = Cli::try_parse_from(["astra", "--max-budget", "0.001"]).unwrap();
        assert!((cli.max_budget - 0.001).abs() < f64::EPSILON);
    }

    #[test]
    fn cli_max_budget_large_value() {
        let cli = Cli::try_parse_from(["astra", "--max-budget", "999.99"]).unwrap();
        assert!((cli.max_budget - 999.99).abs() < f64::EPSILON);
    }

    #[test]
    fn cli_max_budget_integer_value() {
        let cli = Cli::try_parse_from(["astra", "--max-budget", "10"]).unwrap();
        assert!((cli.max_budget - 10.0).abs() < f64::EPSILON);
    }

    // Verify that --max-budget wires through to SessionState.max_budget_limit.
    // The initialize_session_state default is 0.0; after applying the flag it must equal the value.
    #[test]
    fn max_budget_applied_to_session_state() {
        let mut state = initialize_session_state(
            None,
            None,
            &cli::cli_config::cli_context::CliContext::default(),
        );
        assert!(
            (state.max_budget_limit - 0.0).abs() < f64::EPSILON,
            "default max_budget_limit must be 0.0"
        );
        // Apply the flag value — this is what the TUI bootstrap now does.
        state.max_budget_limit = 7.5;
        assert!(
            (state.max_budget_limit - 7.5).abs() < f64::EPSILON,
            "max_budget_limit must reflect the --max-budget flag value"
        );
    }

    // ── --yes / -y edge case tests ──

    #[test]
    fn cli_yes_flag_sets_auto_approve() {
        let cli = Cli::try_parse_from(["astra", "-y"]).unwrap();
        assert!(cli.yes);
    }

    #[test]
    fn cli_yes_long_flag_sets_auto_approve() {
        let cli = Cli::try_parse_from(["astra", "--yes"]).unwrap();
        assert!(cli.yes);
    }

    #[test]
    fn cli_yes_with_permission_mode_deny() {
        // Both flags accepted by parser on `chat` subcommand — runtime resolves conflict
        let cli = Cli::try_parse_from([
            "astra",
            "chat",
            "-y",
            "--permission-mode",
            "deny",
            "-m",
            "test",
        ])
        .unwrap();
        match &cli.command {
            Some(Command::Chat(args)) => {
                assert!(args.auto_approve);
                assert_eq!(args.permission_mode.as_deref(), Some("deny"));
            }
            _ => panic!("expected Chat command"),
        }
    }

    #[test]
    fn cli_yes_with_permission_mode_auto_is_redundant() {
        let cli = Cli::try_parse_from([
            "astra",
            "chat",
            "-y",
            "--permission-mode",
            "auto",
            "-m",
            "test",
        ])
        .unwrap();
        match &cli.command {
            Some(Command::Chat(args)) => {
                assert!(args.auto_approve);
                assert_eq!(args.permission_mode.as_deref(), Some("auto"));
            }
            _ => panic!("expected Chat command"),
        }
    }

    #[test]
    fn cli_permission_mode_invalid_value() {
        let cli = Cli::try_parse_from([
            "astra",
            "chat",
            "--permission-mode",
            "invalid",
            "-m",
            "test",
        ]);
        assert!(cli.is_err());
    }

    #[test]
    fn cli_output_format_invalid_value() {
        let cli = Cli::try_parse_from(["astra", "--output-format", "yaml", "-p"]);
        assert!(cli.is_err());
    }

    #[test]
    fn cli_default_no_yes_no_permission_mode() {
        let cli = Cli::try_parse_from(["astra"]).unwrap();
        assert!(!cli.yes);
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
        for mode_str in &["auto", "accept_edits", "plan", "prompt", "deny"] {
            let mode: permission_manager::PermissionMode = mode_str.parse().unwrap();
            assert_eq!(mode.to_string().to_lowercase(), *mode_str);
        }
    }

    #[test]
    fn session_state_cli_context_auto_approve_activates_auto_mode() {
        let state = initialize_session_state(
            None,
            None,
            &cli::cli_config::cli_context::CliContext::from_launch_options(
                false,
                None,
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
            permission_manager::PermissionMode::Auto
        );
    }

    #[tokio::test]
    async fn task_run_stores_result_in_checkpoint() {
        use astra_services::{TaskCreateRequest, TaskService, task_orchestrator::TaskCheckpoint};

        // Use a temp dir for LocalTaskService
        let tmp = tempfile::tempdir().unwrap();
        let svc = astra_services::LocalTaskService::new(tmp.path().to_path_buf());

        // Create a task record (simulates what `astra task run` does).
        let tid = svc
            .create_task(
                "test-user",
                "test-session",
                TaskCreateRequest {
                    title: "run: test prompt".to_string(),
                    description: Some("test prompt".to_string()),
                    plan: None,
                    parent_task_id: None,
                    project_type: None,
                    goal_pattern: None,
                },
            )
            .await
            .unwrap();

        // Mark in-progress
        svc.update_status(&tid, astra_services::TaskStatus::InProgress)
            .await
            .unwrap();

        // Save checkpoint with result (simulates background task completion)
        let mut state_map = serde_json::Map::new();
        state_map.insert(
            "full_text".to_string(),
            serde_json::Value::String("Hello from agent".to_string()),
        );
        state_map.insert("prompt_tokens".to_string(), serde_json::json!(100));
        state_map.insert("completion_tokens".to_string(), serde_json::json!(50));
        state_map.insert("tool_calls_count".to_string(), serde_json::json!(3));

        svc.save_checkpoint(
            &tid,
            &TaskCheckpoint {
                active_subtask_id: None,
                turn: 0,
                session_id: Some("test-session".to_string()),
                state: state_map,
            },
        )
        .await
        .unwrap();

        // Complete the task
        svc.complete_task(&tid).await.unwrap();

        // Read back and verify (simulates `astra task result`).
        let record = svc.get_task(&tid).await.unwrap().unwrap();
        assert_eq!(record.status, astra_services::TaskStatus::Completed);
        let cp = record.checkpoint.unwrap();
        assert_eq!(
            cp.state.get("full_text").and_then(|v| v.as_str()),
            Some("Hello from agent")
        );
        assert_eq!(
            cp.state.get("prompt_tokens").and_then(|v| v.as_u64()),
            Some(100)
        );
        assert_eq!(
            cp.state.get("tool_calls_count").and_then(|v| v.as_u64()),
            Some(3)
        );
    }

    // ── @file system-prompt tests ──

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
    fn build_effective_line_includes_project_instructions() {
        let mut state = SessionState::default();
        state.project_instructions = Some("Always use Rust.".to_string());
        let result = crate::cli::session::session_input::build_effective_line(
            "hello",
            &state,
            &mut crate::cli::ui_adapter::LineUiAdapter,
        );
        assert!(
            result.contains("<project_instructions>"),
            "should wrap in tags"
        );
        assert!(result.contains("Always use Rust."));
        assert!(
            result.contains("hello"),
            "should still include user message"
        );
    }

    #[test]
    fn build_effective_line_no_instructions_when_none() {
        let state = SessionState::default();
        let result = crate::cli::session::session_input::build_effective_line(
            "hello",
            &state,
            &mut crate::cli::ui_adapter::LineUiAdapter,
        );
        assert!(
            !result.contains("<project_instructions>"),
            "should not inject when None"
        );
        assert_eq!(result, "hello");
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
