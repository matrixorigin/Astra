use crate::cli::arg_render::{
    apply_system_prompt, render_agent_args, render_bug_args, render_debug_args, render_diff_args,
    render_grep_args, render_memory_args, render_messaging_args, render_permissions_args,
    render_review_args, render_team_args,
};
use crate::cli::auth_flow::{
    clear_profile_auth, do_login, do_register, is_auth_error, parse_auth_tokens,
    save_refreshed_profile_tokens,
};
use crate::cli::cli_config::cli_args::{
    AuditCmd, Cli, Command, JournalCmd, ModelCmd, SessionCaptureCmd, SessionCmd, SkillCmd,
};
use crate::cli::cli_config::cli_utils;
use crate::cli::cli_config::cli_utils::{
    clear_profile_last_session_if_matches_or_warn, get_profile_and_token, load_credentials,
    map_thin_err, persist_profile_last_session, print_json_or_raw, profile_name, prompt_or,
    prompt_password_masked, validate_cli_session_id,
};
use crate::cli::config_manager::{
    execute_config_command, latest_artifact_id, resolve_download_output_path,
    resolve_remote_session_id, write_downloaded_capture,
};
use crate::cli::exit_code::ExitCode;
use crate::cli::interactive_chat::run_interactive_chat;
use crate::cli::mcp_config::execute_mcp_command;
use crate::cli::one_shot_session_routing::resolve_one_shot_session_routing;
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
use crate::cli::slash::slash_memory::handle_memory_domain_command;
use crate::cli::slash::slash_messaging::handle_messaging_command;
use crate::cli::slash::{slash_agent, slash_team, slash_telemetry};
use crate::cli::stream::streaming_types::{
    StreamResult, format_background_agent_results, stream_result_from_resumable_turn_failure,
};
use crate::cli::{
    agent_loader, delegate_subrun, diff_presenter, journal_diff, journal_digest, journal_tree,
    theme,
};
use astra_thin_client::paths;
use clap::CommandFactory;
use crossterm::{style::Stylize, terminal};
use std::io::Read;

/// A wall deadline has already stopped useful new work.  The rest of its
/// reserve belongs to proving the server-side run reached a terminal state,
/// not to returning a local partial while an owned provider call is still
/// live.  Keep a small serialization margin for the caller/supervisor.
const WALL_DEADLINE_SERVER_TERMINAL_SETTLE_MARGIN: std::time::Duration =
    std::time::Duration::from_secs(5);

/// Time deliberately held back from a bounded one-shot turn for cancellation,
/// durable settlement, and final serialization. The model must plan against
/// the execution slice, not the supervisor-facing wall limit.
const WALL_DEADLINE_TERMINAL_RESERVE: std::time::Duration = std::time::Duration::from_secs(70);

/// Additional client-side allowance for payload construction, transport, and
/// request admission. This is deducted in addition to the terminal reserve so
/// the Server never receives a rounded-up execution slice.
const WALL_BUDGET_REQUEST_SAFETY_MARGIN: std::time::Duration = std::time::Duration::from_secs(2);

fn durable_run_is_terminal(status: Option<&str>) -> bool {
    matches!(status, Some("completed" | "failed" | "cancelled"))
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
        "/team" | "/memory" | "/plan" | "/review" | "/grep"
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

/// Durable settlement authority for a headless turn.
///
/// A canonical journal commit is irreversible business-turn progress. Derived
/// CSL/profile projections may still fail afterwards; callers must repair
/// those projections instead of retrying the already-committed turn.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum HeadlessCanonicalCommitStatus {
    NotRequested,
    Committed,
    NotCommitted,
    Unknown,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct HeadlessSessionSettlement {
    pub(crate) canonical_session_id: Option<String>,
    pub(crate) commit_status: HeadlessCanonicalCommitStatus,
    pub(crate) projection_repair_required: bool,
    pub(crate) persistence_error: Option<String>,
}

pub(crate) fn persist_headless_session_state(
    profile: Option<&str>,
    model: Option<&str>,
    line: &str,
    sr: &mut StreamResult,
    turn_start: std::time::Instant,
    execution_lease: Option<&astra_services::session_journal::SessionExecutionLease>,
) -> HeadlessSessionSettlement {
    let requested_session_id = sr.session_id.clone();
    let (commit_status, persisted_turn) = match session_side_effects::append_one_shot_journal_events(
        sr.session_id.as_deref(),
        model,
        line,
        sr,
        turn_start,
        execution_lease,
    ) {
        Ok(Some(turn)) => {
            if let Some(error) = turn.persistence_error.as_ref() {
                record_stream_persistence_error(sr, error.clone());
            }
            (HeadlessCanonicalCommitStatus::Committed, Some(turn))
        }
        Ok(None) => (HeadlessCanonicalCommitStatus::NotRequested, None),
        Err(session_side_effects::OneShotJournalCommitError::NotCommitted(error)) => {
            record_stream_persistence_error(sr, error);
            (HeadlessCanonicalCommitStatus::NotCommitted, None)
        }
        Err(session_side_effects::OneShotJournalCommitError::CommitUnknown(error)) => {
            record_stream_persistence_error(sr, error);
            (HeadlessCanonicalCommitStatus::Unknown, None)
        }
    };

    // Local journal and CSL persistence are independent of any degradation
    // already reported by the remote session. The local journal turn supplies
    // the sequence number and remains a lower-fidelity recovery fallback when
    // the richer CSL snapshot cannot be written.
    let canonical_committed = commit_status == HeadlessCanonicalCommitStatus::Committed;
    let mut canonical_recovery_persisted = false;
    if let (Some(session_id), Some(persisted)) = (sr.session_id.clone(), persisted_turn) {
        if sr.final_messages.is_empty() {
            record_stream_persistence_error(
                sr,
                "failed to persist one-shot canonical continuation: canonical messages are empty",
            );
        } else {
            let csl_state = astra_turn_core::conversation_log::SessionStateCompact {
                source_cursor: Some(persisted.cursor),
                recent_tools: sr.tools_used.clone(),
                activated_deferred_tool_names: sr.activated_deferred_tool_names.clone(),
                ..Default::default()
            };
            match crate::cli::session::session_recovery::csl::write_full_csl_snapshot_atomic(
                &session_id,
                persisted.turn,
                &sr.final_messages,
                &csl_state,
            ) {
                Ok(()) => canonical_recovery_persisted = true,
                Err(error) => record_stream_persistence_error(
                    sr,
                    format!("failed to persist one-shot canonical continuation: {error}"),
                ),
            }
        }
    }

    if canonical_committed
        && canonical_recovery_persisted
        && sr.session_persistence_error.is_none()
        && let Some(sid) = sr.session_id.as_deref()
        && let Err(error) = persist_profile_last_session(profile, sid)
    {
        record_stream_persistence_error(
            sr,
            format!("failed to persist last session pointer: {error}"),
        );
    }

    HeadlessSessionSettlement {
        canonical_session_id: canonical_committed
            .then_some(requested_session_id)
            .flatten(),
        commit_status,
        projection_repair_required: canonical_committed && sr.session_persistence_error.is_some(),
        persistence_error: sr.session_persistence_error.clone(),
    }
}

fn finalize_one_shot_stream_result(
    profile: Option<&str>,
    model: Option<&str>,
    line: &str,
    sr: &mut StreamResult,
    turn_start: std::time::Instant,
    execution_lease: Option<&astra_services::session_journal::SessionExecutionLease>,
) -> ExitCode {
    retain_interrupted_partial_canonical_messages(sr, line);
    let _settlement =
        persist_headless_session_state(profile, model, line, sr, turn_start, execution_lease);
    compute_exit_code(sr)
}

pub(crate) fn finalize_one_shot_stream_result_with_request_lease(
    profile: Option<&str>,
    model: Option<&str>,
    line: &str,
    sr: &mut StreamResult,
    turn_start: std::time::Instant,
    request_lease: &crate::cli::session::session_execution_lease::RequestSessionExecutionLease,
) -> ExitCode {
    let session_id = sr.session_id.clone();
    match request_lease.with_matching_lease(session_id.as_deref(), |lease| {
        finalize_one_shot_stream_result(profile, model, line, sr, turn_start, Some(lease))
    }) {
        Ok(exit_code) => exit_code,
        Err(failure) => {
            // Identity/lease admission precedes persistence. Once persistence
            // itself has reported a durability error, retain that exact cause
            // instead of replacing it with a secondary wrapper failure.
            if sr.session_persistence_error.is_none() {
                record_stream_persistence_error(sr, failure.message);
            }
            compute_exit_code(sr)
        }
    }
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

#[derive(Debug, PartialEq, Eq)]
struct ResolvedOneShotModel {
    model: Option<String>,
    offering_id: Option<String>,
}

async fn resolve_one_shot_model(
    api: &astra_thin_client::ThinClient,
    token: &str,
    explicit_model: Option<&str>,
    restored_model: Option<&str>,
    fallback_model: Option<&str>,
) -> Result<ResolvedOneShotModel, String> {
    let model = if let Some(model) =
        effective_one_shot_model(explicit_model, restored_model, fallback_model)
    {
        Some(model.to_string())
    } else {
        match session_runtime::resolve_server_default_model(api, token).await {
            session_runtime::ServerDefaultModel::Selected(selection) => Some(selection.name),
            session_runtime::ServerDefaultModel::NoModels
            | session_runtime::ServerDefaultModel::Unavailable => None,
        }
    };
    let Some(model) = model else {
        return Ok(ResolvedOneShotModel {
            model: None,
            offering_id: None,
        });
    };
    let selection = session_runtime::resolve_server_model_selection(api, token, &model)
        .await
        .map_err(|error| format!("failed to resolve selected model '{model}': {error}"))?;
    Ok(ResolvedOneShotModel {
        // Preserve the caller's thinking suffix and spelling in the turn
        // payload; the shared resolver owns the canonical Offering identity.
        model: Some(model),
        offering_id: Some(selection.offering_id),
    })
}

#[cfg(test)]
mod exact_model_resolution_tests {
    use super::resolve_one_shot_model;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[tokio::test]
    async fn async_resolution_rejects_model_missing_from_authoritative_catalog() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/models"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "items": [{
                    "offering_id": "offer-1",
                    "access_id": "self-hosted",
                    "access_kind": "self_hosted",
                    "access_label": "Self-hosted",
                    "execution_placement": "server",
                    "name": "listed-model",
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
            })))
            .mount(&server)
            .await;
        let api = astra_thin_client::ThinClient::new(&server.uri(), None).expect("client");
        let error = resolve_one_shot_model(&api, "token", Some("overflow-model"), None, None)
            .await
            .expect_err("missing Offering must fail closed");
        assert!(error.contains("authoritative catalog"), "{error}");
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

fn write_headless_stdout(bytes: &[u8]) -> Result<(), String> {
    crate::cli::stream::output_sink::write_stdout(bytes)
        .map(|_| ())
        .map_err(|error| format!("failed to write command output: {error}"))
}

fn write_headless_stdout_line(text: &str) -> Result<(), String> {
    crate::cli::stream::output_sink::write_stdout_line(text)
        .map(|_| ())
        .map_err(|error| format!("failed to write command output: {error}"))
}

fn execute_repl_bridge_command<'a>(
    slash_cmd: &'a str,
    arg: &'a str,
    profile: Option<&'a str>,
    global_model: Option<&'a str>,
    api: &'a astra_thin_client::ThinClient,
    cli_context: &'a crate::cli::cli_config::cli_context::CliContext,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<ExitCode, String>> + 'a>> {
    Box::pin(execute_repl_bridge_command_impl(
        slash_cmd,
        arg,
        profile,
        global_model,
        api,
        cli_context,
    ))
}

async fn execute_repl_bridge_command_impl(
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
        handle_messaging_command(arg, &state).await;
        return Ok(ExitCode::Success);
    }
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
        "/messaging" => handle_messaging_command(arg, &state).await,
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
            permission_mode_display_label(PermissionMode::Bypass),
            "Bypass"
        );
        assert_eq!(
            permission_mode_display_label(PermissionMode::AcceptEdits),
            "Edits"
        );
        assert_eq!(
            permission_mode_display_label(PermissionMode::Plan),
            "Read-only"
        );
        assert_eq!(permission_mode_display_label(PermissionMode::Deny), "Deny");
    }

    #[test]
    fn removed_permission_aliases_do_not_change_mode() {
        for alias in ["all", "default", "ask", "accept-edits", "plan"] {
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
    use super::{
        execute_cli_command, repl_bridge_access_token, repl_bridge_command_requires_access_token,
    };
    use crate::cli::cli_config::cli_args::Cli;
    use crate::cli::cli_config::cli_context::CliContext;
    use clap::Parser;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    struct EnvVarGuard {
        key: &'static str,
        previous: Option<String>,
    }

    impl EnvVarGuard {
        fn set(key: &'static str, value: &str) -> Self {
            let previous = std::env::var(key).ok();
            // SAFETY: callers use `#[serial]` to isolate process env mutation.
            unsafe { std::env::set_var(key, value) };
            Self { key, previous }
        }

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
        for command in ["/team", "/memory", "/plan", "/review", "/grep"] {
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

        let err = repl_bridge_access_token("/memory", &api, missing_profile)
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

    #[serial_test::serial]
    #[tokio::test]
    async fn headless_chat_accepts_gateway_token_without_a_local_profile() {
        let _env = EnvVarGuard::set("ASTRA_ACCESS_TOKEN", "gateway-token");
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/models"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "items": [],
                "next_cursor": null,
                "limit": 50,
                "total": 0,
                "catalog_revision": "sha256:test"
            })))
            .mount(&server)
            .await;

        let parsed = Cli::try_parse_from([
            "astra",
            "chat",
            "--no-resume",
            "--model",
            "missing-model",
            "--message",
            "hello",
        ])
        .expect("headless chat arguments");
        let api = astra_thin_client::ThinClient::new(&server.uri(), None).expect("client");
        let error = execute_cli_command(
            parsed.command,
            Some("__missing_gateway_profile__".to_string()),
            None,
            false,
            None,
            &api,
            true,
            &CliContext::default(),
        )
        .await
        .expect_err("the deliberately missing model should stop the request");

        assert!(
            error.contains("failed to resolve selected model 'missing-model'"),
            "gateway auth must advance past local-profile admission: {error}"
        );
        assert!(!error.contains("no profile"), "{error}");
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn execute_cli_command<'a>(
    command: Option<Command>,
    profile: Option<String>,
    global_model: Option<String>,
    auto_approve: bool,
    system_prompt: Option<String>,
    api: &'a astra_thin_client::ThinClient,
    no_instructions: bool,
    cli_context: &'a crate::cli::cli_config::cli_context::CliContext,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<ExitCode, String>> + 'a>> {
    // Erase the large command-dispatch future at the API boundary. Keeping this
    // wrapper as `async fn` embeds both `Command` and the boxed implementation
    // in every caller's state machine, which can overflow the normal Tokio test
    // thread stack even for small branches such as `health`.
    Box::pin(execute_cli_command_impl(
        command,
        profile,
        global_model,
        auto_approve,
        system_prompt,
        api,
        no_instructions,
        cli_context,
    ))
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
            let token = fresh_access_token_or_error(api, profile.as_deref()).await?;
            let mut session_routing = resolve_one_shot_session_routing(
                api,
                profile.as_deref(),
                cli_context.session_id.clone(),
                true,
            )
            .await?;
            let admitted_session_id = session_routing.server_session_id.clone();
            let request_session_execution_lease =
                crate::cli::session::session_execution_lease::RequestSessionExecutionLease::new(
                    admitted_session_id.as_deref(),
                )
                .map_err(|failure| failure.message)?;
            if let Some(session_id) = admitted_session_id.as_deref() {
                session_routing = resolve_one_shot_session_routing(
                    api,
                    profile.as_deref(),
                    Some(session_id.to_string()),
                    true,
                )
                .await?;
                if session_routing.server_session_id.as_deref() != Some(session_id) {
                    return Err("session routing changed after execution admission".to_string());
                }
            }
            let session_id = session_routing.server_session_id.clone();
            let resolved_model = resolve_one_shot_model(
                api,
                &token,
                None,
                session_routing.restored_model(),
                global_model.as_deref(),
            )
            .await?;
            let effective_model = resolved_model.model;
            let effective_offering_id = resolved_model.offering_id;
            let effective_permission_mode = effective_one_shot_permission_mode(
                None,
                auto_approve,
                session_routing.restored_permission_mode(),
                false,
            )?;
            let (mut continuation_messages, activated_deferred_tool_names) =
                session_routing.continuation_turn_inputs()?;
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
                offering_id: effective_offering_id.as_deref(),
                provider: None,
                explain: ExplainMode::Off,
                render_md: terminal::size().is_ok(),
                verbose_mode: true,
                render_policy: crate::cli::stream::stream_render::RenderPolicy::Stream,
                cli_context: Some(cli_context),
                unified_skill_registry: astra_runtime::skills::default_unified_registry(),
                agent_spawner: None,
                root_agent_id: None,
                bg_task_commands: None,
                bg_task_list_cache: None,
                bash_detach_slot: None,
                stream_event_tx: None,
                stream_json_emitter: None,
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
                activated_deferred_tool_names,
                turn_index: Some(session_routing.next_server_turn_index()),
                request_session_execution_lease: Some(request_session_execution_lease.clone()),
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
                Err(e) if crate::cli::turn::turn_facade::is_settled_stdout_closure(&e) => {
                    return Ok(ExitCode::Success);
                }
                Err(e) if is_auth_error(&e.error) => {
                    if session_runtime::attempt_token_refresh(api, profile.as_deref()).await {
                        if let Some(new_token) =
                            session_runtime::current_access_token(profile.as_deref())
                        {
                            eprintln!(
                                "  {} Token refreshed, retrying…",
                                crate::cli::theme::icon_ok()
                            );
                            match crate::cli::turn::execute_basic_cli_turn(
                                &chat_ctx,
                                &new_token,
                                session_id.as_deref(),
                                profile.as_deref(),
                                &mut pm,
                                &mut skill_qt,
                                turn_options.clone(),
                            )
                            .await
                            {
                                Ok(sr) => sr,
                                Err(failure)
                                    if crate::cli::turn::turn_facade::is_settled_stdout_closure(
                                        &failure,
                                    ) =>
                                {
                                    return Ok(ExitCode::Success);
                                }
                                Err(failure) => return Err(failure.error),
                            }
                        } else {
                            return Err(e.error);
                        }
                    } else {
                        return Err(e.error);
                    }
                }
                Err(e) => return Err(e.error),
            };
            let exit_code = finalize_one_shot_stream_result_with_request_lease(
                profile.as_deref(),
                effective_model.as_deref(),
                &message,
                &mut sr,
                turn_start,
                request_session_execution_lease.as_ref(),
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
            let tokens = parse_auth_tokens(&body)?;
            save_refreshed_profile_tokens(profile.as_deref(), &tokens)?;
            stdout_println!("  {} {}", theme::icon_ok(), "Token refreshed".green());
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

        Some(Command::Work(command)) => {
            let token = fresh_access_token_or_error(api, profile.as_deref()).await?;
            crate::cli::work_command::execute_work_command(command, &token, api).await
        }

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
                                stdout_println!("Context snapshot written to {}", p.display());
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
            // Anchor before any token/session/model/spawner await so the
            // process-level deadline covers the complete one-shot lifecycle.
            let one_shot_terminal_deadline = args.max_wall_time_seconds.map(|seconds| {
                tokio::time::Instant::now() + std::time::Duration::from_secs(seconds)
            });
            let one_shot_execution_time_budget = one_shot_terminal_deadline.map(|deadline| {
                crate::cli::chat_stream::ExecutionTimeBudgetClock::new(
                    deadline,
                    WALL_DEADLINE_TERMINAL_RESERVE,
                    WALL_BUDGET_REQUEST_SAFETY_MARGIN,
                )
            });
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
                    &interactive_context,
                )
                .await?;
                return Ok(ExitCode::Success);
            };

            let token = if let Some(deadline) = one_shot_terminal_deadline {
                tokio::time::timeout_at(
                    deadline,
                    fresh_access_token_or_error(api, profile.as_deref()),
                )
                .await
                .map_err(|_| "request wall deadline expired during authentication".to_string())??
            } else {
                fresh_access_token_or_error(api, profile.as_deref()).await?
            };
            let explicit_session_id = args.session_id.clone();
            let session_routing_future = resolve_one_shot_session_routing(
                api,
                profile.as_deref(),
                match explicit_session_id {
                    Some(session_id) => Some(session_id),
                    None => cli_context.session_id.clone(),
                },
                !args.no_resume,
            );
            let mut session_routing = if let Some(deadline) = one_shot_terminal_deadline {
                tokio::time::timeout_at(deadline, session_routing_future)
                    .await
                    .map_err(|_| {
                        "request wall deadline expired during session routing".to_string()
                    })??
            } else {
                session_routing_future.await?
            };
            let admitted_session_id = session_routing.server_session_id.clone();
            let request_session_execution_lease =
                crate::cli::session::session_execution_lease::RequestSessionExecutionLease::new(
                    admitted_session_id.as_deref(),
                )
                .map_err(|failure| failure.message)?;
            if let Some(session_id) = admitted_session_id.as_deref() {
                let refresh_future = resolve_one_shot_session_routing(
                    api,
                    profile.as_deref(),
                    Some(session_id.to_string()),
                    !args.no_resume,
                );
                session_routing = if let Some(deadline) = one_shot_terminal_deadline {
                    tokio::time::timeout_at(deadline, refresh_future)
                        .await
                        .map_err(|_| {
                            "request wall deadline expired while refreshing admitted session"
                                .to_string()
                        })??
                } else {
                    refresh_future.await?
                };
                if session_routing.server_session_id.as_deref() != Some(session_id) {
                    return Err("session routing changed after execution admission".to_string());
                }
            }
            let session_id = session_routing.server_session_id.clone();
            let model_future = resolve_one_shot_model(
                api,
                &token,
                args.model.as_deref(),
                session_routing.restored_model(),
                global_model.as_deref(),
            );
            let resolved_model = if let Some(deadline) = one_shot_terminal_deadline {
                tokio::time::timeout_at(deadline, model_future)
                    .await
                    .map_err(|_| {
                        "request wall deadline expired during model resolution".to_string()
                    })??
            } else {
                model_future.await?
            };
            let effective_model = resolved_model.model;
            let effective_offering_id = resolved_model.offering_id;
            let effective_permission_mode = effective_one_shot_permission_mode(
                args.permission_mode.as_deref(),
                args.auto_approve || auto_approve,
                session_routing.restored_permission_mode(),
                false,
            )?;
            let (mut continuation_messages, activated_deferred_tool_names) =
                session_routing.continuation_turn_inputs()?;
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
            let spawner_future = super::agent_runtime::build_one_shot_spawner(
                api,
                token.clone(),
                astra_runtime::skills::default_unified_registry().clone(),
                session_id.clone(),
                effective_model.clone(),
            );
            let one_shot_spawner = if let Some(deadline) = one_shot_terminal_deadline {
                tokio::time::timeout_at(deadline, spawner_future)
                    .await
                    .map_err(|_| "request wall deadline expired during agent setup".to_string())?
            } else {
                spawner_future.await
            };

            // Keep a clone of the Arc so we can drain background
            // spawned children before process exit — otherwise
            // background tasks (the default background-agent mode) get
            // aborted when main returns, which silently drops any
            // ForkCacheEvent / child telemetry they would have
            // emitted on their first response.
            let spawner_handle_for_drain = one_shot_spawner.clone();
            let (stream_event_tx, stream_event_writer) = if let Some(path) =
                args.stream_events.as_deref()
            {
                let (tx, rx) = crate::cli::chat_stream::stream_event_channel();
                let handle = crate::cli::stream::stream_events_writer::spawn_file_writer(rx, path)
                    .map_err(|error| {
                        format!(
                            "failed to open stream-event file {}: {error}",
                            path.display()
                        )
                    })?;
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
            let chat_ctx = crate::cli::chat_stream::BasicCliChatContext {
                api,
                auth_profile: profile.as_deref(),
                message: &message,
                model: effective_model.as_deref(),
                offering_id: effective_offering_id.as_deref(),
                provider: None,
                explain: explain_mode,
                render_md,
                verbose_mode: !quiet,
                render_policy,
                cli_context: Some(cli_context),
                unified_skill_registry: astra_runtime::skills::default_unified_registry(),
                agent_spawner: Some(one_shot_spawner),
                root_agent_id: Some(&root_agent_id),
                bg_task_commands: None,
                bg_task_list_cache: None,
                bash_detach_slot: None,
                stream_event_tx,
                stream_json_emitter: None,
                #[cfg(feature = "harness")]
                harness_sink: Some(harness_sink.clone()),
                #[cfg(feature = "harness")]
                harness_trace: Some(harness_trace),
                #[cfg(feature = "harness")]
                benchmark_profile: args.benchmark_profile,
            };
            let wall_deadline_cancel_token = args
                .max_wall_time_seconds
                .map(|_| std::sync::Arc::new(tokio_util::sync::CancellationToken::new()));
            let wall_deadline_incremental_state = args.max_wall_time_seconds.map(|_| {
                std::sync::Arc::new(
                    astra_turn_core::turn_event_sink::IncrementalTurnState::default(),
                )
            });
            let turn_options = crate::cli::turn::turn_facade::BasicCliTurnOptions {
                pre_loaded_messages: continuation_messages.take(),
                activated_deferred_tool_names,
                append_system_prompt: args.append_system_prompt.clone(),
                execution_time_budget: one_shot_execution_time_budget,
                disable_session_not_found_retry: args.no_resume || args.session_id.is_some(),
                turn_index: Some(session_routing.next_server_turn_index()),
                cancel_token: wall_deadline_cancel_token.clone(),
                incremental_state: wall_deadline_incremental_state.clone(),
                request_session_execution_lease: Some(request_session_execution_lease.clone()),
                ..Default::default()
            };
            let turn_start = std::time::Instant::now();
            let terminal_deadline = one_shot_terminal_deadline;
            let execution_deadline =
                terminal_deadline.map(|deadline| deadline - WALL_DEADLINE_TERMINAL_RESERVE);
            let mut wall_deadline_reached = false;
            let mut wall_deadline_root_settled = false;
            let mut wall_deadline_server_terminal_settled = false;
            let turn_result = {
                let turn_future = crate::cli::turn::execute_basic_cli_turn(
                    &chat_ctx,
                    &token,
                    session_id.as_deref(),
                    profile.as_deref(),
                    &mut pm,
                    &mut skill_qt,
                    turn_options,
                );
                tokio::pin!(turn_future);
                if let Some(deadline) = execution_deadline {
                    tokio::select! {
                        result = &mut turn_future => result,
                        _ = tokio::time::sleep_until(deadline) => {
                            wall_deadline_reached = true;
                            if let Some(cancel_token) = wall_deadline_cancel_token.as_ref() {
                                cancel_token.cancel();
                            }
                            let snapshot = wall_deadline_incremental_state
                                .as_ref()
                                .map(|state| state.snapshot())
                                .unwrap_or_default();
                            async {
                                let run_id = snapshot.run_id.clone().ok_or_else(|| crate::TurnFailure {
                                    error: "request wall deadline reached before an authoritative run id was observed".to_string(),
                                    partial: Default::default(),
                                })?;
                                let terminal = terminal_deadline.expect("deadline pair is constructed together");
                                let terminal_settle_deadline = terminal
                                    .checked_sub(WALL_DEADLINE_SERVER_TERMINAL_SETTLE_MARGIN)
                                    .unwrap_or(terminal);
                                let cancel_deadline = terminal_settle_deadline;
                                let mut cancellation_accepted = false;
                                loop {
                                    let remaining = cancel_deadline
                                        .saturating_duration_since(tokio::time::Instant::now());
                                    if remaining.is_zero() {
                                        return Err(crate::TurnFailure {
                                            error: format!("request wall deadline cancellation did not settle run {run_id}"),
                                            partial: Default::default(),
                                        });
                                    }
                                    if !cancellation_accepted {
                                        match tokio::time::timeout(
                                            remaining,
                                            api.cancel_run(Some(&token), &run_id),
                                        ).await {
                                            Ok(Ok(response)) if response
                                                .get("execution_settled")
                                                .and_then(serde_json::Value::as_bool)
                                            == Some(true) => {
                                                wall_deadline_server_terminal_settled = true;
                                                break;
                                            }
                                            Ok(Ok(response)) if response
                                                .get("status")
                                                .and_then(serde_json::Value::as_str)
                                                == Some("cancellation_requested") => {
                                                    cancellation_accepted = true;
                                                }
                                            Ok(Ok(response)) if durable_run_is_terminal(
                                                response.get("status").and_then(serde_json::Value::as_str),
                                            ) => {
                                                wall_deadline_server_terminal_settled = true;
                                                break;
                                            }
                                            Ok(Ok(_)) => {}
                                            Ok(Err(error)) => return Err(crate::TurnFailure {
                                                error: format!("request wall deadline could not cancel run {run_id}: {error}"),
                                                partial: Default::default(),
                                            }),
                                            Err(_) => return Err(crate::TurnFailure {
                                                error: format!("request wall deadline cancellation timed out for run {run_id}"),
                                                partial: Default::default(),
                                            }),
                                        }
                                    } else {
                                        match tokio::time::timeout(
                                            remaining,
                                            api.get_run(Some(&token), &run_id),
                                        ).await {
                                            Ok(Ok(run)) if durable_run_is_terminal(
                                                run.get("status").and_then(serde_json::Value::as_str),
                                            ) => {
                                                wall_deadline_server_terminal_settled = true;
                                                break;
                                            }
                                            Ok(Ok(_)) => {}
                                            Ok(Err(error)) => return Err(crate::TurnFailure {
                                                error: format!("request wall deadline could not verify run {run_id} cancellation: {error}"),
                                                partial: Default::default(),
                                            }),
                                            Err(_) => return Err(crate::TurnFailure {
                                                error: format!("request wall deadline cancellation status check timed out for run {run_id}"),
                                                partial: Default::default(),
                                            }),
                                        }
                                    }
                                    tokio::time::sleep(
                                        remaining.min(std::time::Duration::from_millis(100)),
                                    ).await;
                                }
                                let drain_budget = terminal
                                    .saturating_duration_since(tokio::time::Instant::now())
                                    .min(std::time::Duration::from_secs(20));
                                match tokio::time::timeout(
                                    drain_budget,
                                    &mut turn_future,
                                )
                                .await
                                {
                                    Ok(result) => {
                                        wall_deadline_root_settled = true;
                                        match result {
                                            Ok(result) => Ok(result),
                                            Err(_) => Ok(stream_result_from_incremental_snapshot(
                                                wall_deadline_incremental_state
                                                    .as_ref()
                                                    .map(|state| state.snapshot())
                                                    .unwrap_or(snapshot),
                                            )),
                                        }
                                    }
                                    Err(_) => Err(crate::TurnFailure {
                                        error: format!("request wall deadline root drain timed out for run {run_id}"),
                                        partial: Default::default(),
                                    }),
                                }
                            }.await
                        }
                    }
                } else {
                    turn_future.as_mut().await
                }
            };

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
            let mut terminal_settlement_error = None;
            let background_agent_results = if wall_deadline_reached
                && spawner_handle_for_drain.background_task_count() > 0
            {
                terminal_settlement_error = Some(
                    "request wall deadline reached with background agents still active".to_string(),
                );
                Vec::new()
            } else if wall_deadline_reached {
                // No task owns a background execution resource, so there is
                // nothing to drain and no unbounded cancellation tail.
                Vec::new()
            } else {
                spawner_handle_for_drain
                    .shutdown_and_wait(std::time::Duration::from_secs(30))
                    .await
            };

            // Child progress shares the root stream. Close and flush it only
            // after every terminal child event has had a chance to arrive.
            drop(chat_ctx);
            if let Some(mut handle) = stream_event_writer {
                if let Some(deadline) = terminal_deadline {
                    let writer_budget = deadline
                        .saturating_duration_since(tokio::time::Instant::now())
                        .min(std::time::Duration::from_secs(5));
                    match tokio::time::timeout(writer_budget, &mut handle).await {
                        Err(_) => {
                            handle.abort();
                            terminal_settlement_error = Some(
                                "request wall deadline stream-event writer did not settle"
                                    .to_string(),
                            );
                        }
                        Ok(Err(error)) => {
                            terminal_settlement_error =
                                Some(format!("stream-event writer task failed: {error}"));
                        }
                        Ok(Ok(Err(error))) => {
                            terminal_settlement_error =
                                Some(format!("stream-event file write failed: {error}"));
                        }
                        Ok(Ok(Ok(()))) => {}
                    }
                } else {
                    match handle.await {
                        Err(error) => {
                            terminal_settlement_error =
                                Some(format!("stream-event writer task failed: {error}"));
                        }
                        Ok(Err(error)) => {
                            terminal_settlement_error =
                                Some(format!("stream-event file write failed: {error}"));
                        }
                        Ok(Ok(())) => {}
                    }
                }
            }

            let mut sr = match turn_result {
                Ok(sr) => sr,
                Err(e) if crate::cli::turn::turn_facade::is_settled_stdout_closure(&e) => {
                    return Ok(ExitCode::Success);
                }
                Err(e) => {
                    if let Some(mut sr) = stream_result_from_resumable_turn_failure(&e) {
                        sr.background_agent_results = background_agent_results.clone();
                        let background_agent_section = sr.integrate_background_agent_results();
                        let exit_code = finalize_one_shot_stream_result_with_request_lease(
                            profile.as_deref(),
                            effective_model.as_deref(),
                            &message,
                            &mut sr,
                            turn_start,
                            request_session_execution_lease.as_ref(),
                        );

                        if args.json {
                            let mut json_output = final_json_output(&sr, exit_code);
                            if let Some(obj) = json_output.as_object_mut() {
                                obj.insert("ttft_ms".to_string(), serde_json::json!(sr.ttft_ms));
                                obj.insert(
                                    "context_ms".to_string(),
                                    serde_json::json!(sr.context_ms),
                                );
                                obj.insert(
                                    "background_agent_results".to_string(),
                                    serde_json::json!(
                                        sr.background_agent_results
                                            .iter()
                                            .map(|(id, text)| serde_json::json!({
                                                "agent_id": id,
                                                "result": text
                                            }))
                                            .collect::<Vec<_>>()
                                    ),
                                );
                            }
                            write_headless_stdout_line(
                                &serde_json::to_string_pretty(&json_output).unwrap_or_default(),
                            )?;
                            return Ok(exit_code);
                        }
                        if quiet {
                            write_headless_stdout_line(&sr.full_text)?;
                        } else if let Some(section) = background_agent_section {
                            write_headless_stdout_line(&format!("\n\n{section}"))?;
                        }
                        print_one_shot_completion_warning(&sr, exit_code, args.json);
                        return Ok(exit_code);
                    }
                    let mut error = e.error;
                    if let Some(section) =
                        format_background_agent_results(&background_agent_results)
                    {
                        error.push_str("\n\n");
                        error.push_str(&section);
                    }
                    return Err(error);
                }
            };
            if wall_deadline_reached {
                if !wall_deadline_root_settled {
                    return Err(
                        "request wall deadline ended without root execution settlement".to_string(),
                    );
                }
                if let Some(error) = terminal_settlement_error {
                    return Err(error);
                }
                apply_wall_deadline_interruption(&mut sr, wall_deadline_server_terminal_settled);
                retain_wall_deadline_partial_canonical_messages(&mut sr, &message);
            }
            sr.background_agent_results = background_agent_results;
            let background_agent_section = sr.integrate_background_agent_results();

            let exit_code = finalize_one_shot_stream_result_with_request_lease(
                profile.as_deref(),
                effective_model.as_deref(),
                &message,
                &mut sr,
                turn_start,
                request_session_execution_lease.as_ref(),
            );

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
                write_headless_stdout_line(
                    &serde_json::to_string_pretty(&json_output).unwrap_or_default(),
                )?;
                return Ok(exit_code);
            } else if quiet {
                // Quiet mode: just print the text without formatting
                write_headless_stdout_line(&sr.full_text)?;
            } else if let Some(section) = background_agent_section {
                // The primary assistant response was already streamed. Surface
                // only the newly reconciled child section here.
                write_headless_stdout_line(&format!("\n\n{section}"))?;
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

        Some(Command::Session(SessionCmd::Cancel(args))) => {
            let session_id = validated_cli_session_arg(&args.session_id)?;
            let (_, _, _, token) = get_profile_and_token(profile.as_deref())?;
            let body = api
                .post_session_cancel_text(&token, session_id)
                .await
                .map_err(map_thin_err)?;
            clear_profile_last_session_if_matches_or_warn(
                profile.as_deref(),
                session_id,
                "command_router:session_cancel",
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
                stdout_println!("  {} {}", theme::icon_ok(), "Deleted".green());
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
            stdout_println!(
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
            let body = session_runtime::load_server_model_catalog_json(api, &token)
                .await
                .map_err(|error| error.to_string())?;
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
            let mut completion = Vec::new();
            clap_complete::generate(args.shell, &mut cmd, "astra", &mut completion);
            write_headless_stdout(&completion)?;
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
fn apply_wall_deadline_interruption(sr: &mut StreamResult, server_terminal_verified: bool) {
    sr.final_state = "interrupted".to_string();
    sr.interruption_kind = Some("execution_incomplete".to_string());
    sr.interruption = Some(serde_json::json!({
        "kind": "execution_incomplete",
        "reason": "request_wall_deadline_reached",
        "resumable": true,
    }));
    sr.server_terminal_unverified = !server_terminal_verified;
    if sr.full_text.trim().is_empty() {
        sr.full_text = "Execution stopped at the request wall-time boundary before all work could be verified.".to_string();
    }
}

fn retain_interrupted_partial_canonical_messages(sr: &mut StreamResult, user_message: &str) {
    if !sr.final_messages.is_empty()
        || sr.full_text.trim().is_empty()
        || sr.final_state != "interrupted"
    {
        return;
    }
    // Any interrupted transport can return only the live incremental
    // snapshot. Preserve the exact user input and already-observed assistant
    // bytes as a resumable canonical partial; never turn this into a completed
    // turn or discard progress merely because the richer transcript did not
    // finish materializing.  Tool provenance remains separately fail-closed.
    sr.final_messages = vec![
        serde_json::json!({"role": "user", "content": user_message}),
        serde_json::json!({"role": "assistant", "content": sr.full_text}),
    ];
}

fn retain_wall_deadline_partial_canonical_messages(sr: &mut StreamResult, user_message: &str) {
    retain_interrupted_partial_canonical_messages(sr, user_message);
}

fn stream_result_from_incremental_snapshot(
    snapshot: astra_turn_core::turn_event_sink::TurnIncrementalSnapshot,
) -> StreamResult {
    StreamResult {
        session_id: snapshot.session_id,
        run_id: snapshot.run_id,
        full_text: snapshot.partial_text,
        prompt_tokens: snapshot.prompt_tokens,
        completion_tokens: snapshot.completion_tokens,
        cache_read_tokens: snapshot.cache_read_tokens,
        cache_creation_tokens: snapshot.cache_creation_tokens,
        // The retained record window is intentionally capped. Use the
        // monotonic logical counter for the aggregate and keep the window
        // separately as partial audit evidence.
        tool_calls_count: snapshot.tool_calls_count,
        tools_used: snapshot.tools_used,
        tool_call_records: snapshot.tool_call_records,
        llm_rounds: snapshot.llm_rounds,
        token_usage_coverage: snapshot.token_usage_coverage,
        tool_record_coverage_partial: true,
        ..StreamResult::default()
    }
}

fn compute_exit_code(sr: &StreamResult) -> ExitCode {
    let is_error = |record: &astra_services::session_journal::ToolCallRecord| {
        if record.effective_disposition()
            != astra_services::session_journal::ToolCallDisposition::Executed
        {
            return false;
        }
        match record.exit_semantics.as_deref().and_then(|tag| {
            serde_json::from_value::<astra_tools::exit_semantics::ExitSemantics>(
                serde_json::Value::String(tag.to_string()),
            )
            .ok()
        }) {
            Some(
                astra_tools::exit_semantics::ExitSemantics::Success
                | astra_tools::exit_semantics::ExitSemantics::EmptyResult
                | astra_tools::exit_semantics::ExitSemantics::DomainNegative
                | astra_tools::exit_semantics::ExitSemantics::PipelineTruncated,
            ) => false,
            None => !record.ok,
            Some(
                astra_tools::exit_semantics::ExitSemantics::TimedOut
                | astra_tools::exit_semantics::ExitSemantics::Cancelled
                | astra_tools::exit_semantics::ExitSemantics::Signaled
                | astra_tools::exit_semantics::ExitSemantics::ExecutionError,
            ) => true,
        }
    };

    if sr.session_persistence_error.is_some() {
        return ExitCode::PersistenceError;
    }
    // A typed interruption is the terminal lifecycle authority once the
    // request owner has confirmed execution settlement.  Its partial ledger
    // commonly ends in the tool cancellation that made settlement possible;
    // that cancellation must not demote the resumable outcome to an ordinary
    // tool failure (exit 1), which benchmark/process callers cannot
    // distinguish from an unstructured crash.  Durability failure remains a
    // harder error and is intentionally checked first.
    if sr.final_state == "interrupted" {
        return ExitCode::Partial;
    }
    if !sr.server_terminal_authoritative
        && sr.tool_call_records.iter().any(&is_error)
        && sr
            .tool_call_records
            .last()
            .is_none_or(|record| is_error(record))
    {
        return ExitCode::ToolFailure;
    }
    ExitCode::Success
}

/// Classify the user-visible terminal contract without turning advisory
/// evidence into another execution/retry authority. A natural-language stop
/// can still be useful output, but an active exact-argument failure or
/// rejection means Astra cannot honestly label the result verified.
fn completion_disposition(sr: &StreamResult, exit_code: ExitCode) -> &'static str {
    if exit_code != ExitCode::Success {
        return if sr.final_state == "interrupted" {
            "interrupted"
        } else {
            "failed"
        };
    }
    if sr.server_terminal_unverified {
        return "responded_unverified";
    }
    if sr.server_terminal_authoritative {
        return "completed";
    }
    let unresolved = !astra_turn_core::evaluation::active_execution_failure_operation_keys(
        &sr.tool_call_records,
    )
    .is_empty()
        || !astra_turn_core::evaluation::active_rejected_operation_keys(&sr.tool_call_records)
            .is_empty();
    if unresolved {
        "responded_unverified"
    } else {
        "completed"
    }
}

fn error_kind_for_exit_code(exit_code: ExitCode) -> Option<&'static str> {
    match exit_code {
        ExitCode::Success => None,
        ExitCode::ToolFailure => Some("tool_failure"),
        ExitCode::Cancelled => Some("cancelled"),
        ExitCode::ApiError => Some("api_error"),
        ExitCode::PersistenceError => Some("persistence_error"),
        ExitCode::Partial => Some("partial"),
        ExitCode::Unfinished => Some("unfinished"),
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

fn final_stream_json_result(sr: &StreamResult, exit_code: ExitCode) -> serde_json::Value {
    let mut result = final_json_output(sr, exit_code);
    if let Some(object) = result.as_object_mut() {
        object.remove("run_id");
    }
    result
}

fn final_json_output_with_context(
    sr: &StreamResult,
    exit_code: ExitCode,
    trace_id: Option<String>,
    request_id: Option<String>,
) -> serde_json::Value {
    let total_prompt_tokens = astra_turn_types::NormalizedPromptCacheUsage::new(
        sr.prompt_tokens,
        sr.cache_read_tokens,
        sr.cache_creation_tokens,
    )
    .total_input_tokens();
    let tool_result_class_counts = serde_json::to_value(sr.tool_ledger_aggregate.result_classes)
        .expect("fixed tool result-class aggregate must serialize");
    let disposition = completion_disposition(sr, exit_code);
    serde_json::json!({
        "trace_id": trace_id,
        "request_id": request_id,
        "run_id": sr.run_id,
        "session_id": sr.session_id,
        "text": sr.full_text,
        "final_state": sr.final_state,
        "interruption_kind": sr.interruption_kind,
        "server_terminal_unverified": sr.server_terminal_unverified,
        "server_terminal_authoritative": sr.server_terminal_authoritative,
        "tool_record_coverage": if sr.tool_record_coverage_partial {
            "partial"
        } else {
            "complete"
        },
        "tool_result_class_counts": tool_result_class_counts,
        "prompt_tokens": total_prompt_tokens,
        "fresh_prompt_tokens": sr.prompt_tokens,
        "cache": {
            "hit": sr.cache_read_tokens > 0,
            "read_tokens": sr.cache_read_tokens,
            "creation_tokens": sr.cache_creation_tokens,
        },
        "completion_tokens": sr.completion_tokens,
        "token_usage_coverage": {
            "scope": "logical_provider_calls",
            "attempts": sr.token_usage_coverage.attempts,
            "provider_reported": sr.token_usage_coverage.provider_reported,
            "unavailable": sr.token_usage_coverage.unavailable,
            "status": sr.token_usage_coverage.status(),
        },
        "llm_rounds": sr.llm_rounds,
        "tool_calls_count": sr.tool_calls_count,
        "tools_used": sr.tools_used,
        "persistence_error": sr.session_persistence_error,
        "exit_code": i32::from(exit_code),
        "success": exit_code == ExitCode::Success && disposition == "completed",
        "completion_disposition": disposition,
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

    let token = fresh_access_token_or_error(api, profile).await?;
    let mut session_routing =
        resolve_one_shot_session_routing(api, profile, cli_context.session_id.clone(), true)
            .await?;
    let admitted_session_id = session_routing.server_session_id.clone();
    let request_session_execution_lease =
        crate::cli::session::session_execution_lease::RequestSessionExecutionLease::new(
            admitted_session_id.as_deref(),
        )
        .map_err(|failure| failure.message)?;
    if let Some(session_id) = admitted_session_id.as_deref() {
        session_routing =
            resolve_one_shot_session_routing(api, profile, Some(session_id.to_string()), true)
                .await?;
        if session_routing.server_session_id.as_deref() != Some(session_id) {
            return Err("session routing changed after execution admission".to_string());
        }
    }
    let session_id = session_routing.server_session_id.clone();
    let resolved_model =
        resolve_one_shot_model(api, &token, None, session_routing.restored_model(), model).await?;
    let effective_model = resolved_model.model;
    let effective_offering_id = resolved_model.offering_id;
    let effective_permission_mode = effective_one_shot_permission_mode(
        None,
        false,
        session_routing.restored_permission_mode(),
        true,
    )?;
    let (mut continuation_messages, activated_deferred_tool_names) =
        session_routing.continuation_turn_inputs()?;
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

    let session_turn = session_routing.next_server_turn_index();
    let stream_json_emitter = if output_format == "stream-json" {
        Some(crate::cli::stream::stream_json::StreamJsonEmitter::stdout(
            session_turn,
        )?)
    } else {
        None
    };

    let chat_ctx = crate::cli::chat_stream::BasicCliChatContext {
        api,
        auth_profile: profile,
        message: &message,
        model: effective_model.as_deref(),
        offering_id: effective_offering_id.as_deref(),
        provider: None,
        explain: ExplainMode::Off,
        render_md: false,
        verbose_mode: false,
        render_policy: crate::cli::stream::stream_render::RenderPolicy::Silent,
        cli_context: Some(cli_context),
        unified_skill_registry: astra_runtime::skills::default_unified_registry(),
        agent_spawner: None,
        root_agent_id: None,
        bg_task_commands: None,
        bg_task_list_cache: None,
        bash_detach_slot: None,
        stream_event_tx: None,
        stream_json_emitter: stream_json_emitter.clone(),
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
        activated_deferred_tool_names,
        turn_index: Some(session_turn),
        request_session_execution_lease: Some(request_session_execution_lease.clone()),
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
        Err(e) if crate::cli::turn::turn_facade::is_settled_stdout_closure(&e) => {
            return Ok(ExitCode::Success);
        }
        Err(e) => {
            let error = e.error;
            let partial = e.partial;
            if let Some(emitter) = stream_json_emitter.as_ref() {
                let total_prompt_tokens = astra_turn_types::NormalizedPromptCacheUsage::new(
                    partial.prompt_tokens,
                    partial.cache_read_tokens,
                    partial.cache_creation_tokens,
                )
                .total_input_tokens();
                let result_session_id = partial.session_id.clone().or_else(|| session_id.clone());
                let (trace_id, request_id) = gateway_env_context();
                emitter.emit_result(
                    result_session_id.as_deref(),
                    serde_json::json!({
                        "trace_id": trace_id,
                        "request_id": request_id,
                        "session_id": partial.session_id,
                        "text": partial.partial_text,
                        "final_state": "failed",
                        "interruption_kind": serde_json::Value::Null,
                        "prompt_tokens": total_prompt_tokens,
                        "fresh_prompt_tokens": partial.prompt_tokens,
                        "cache": {
                            "hit": partial.cache_read_tokens > 0,
                            "read_tokens": partial.cache_read_tokens,
                            "creation_tokens": partial.cache_creation_tokens,
                        },
                        "completion_tokens": partial.completion_tokens,
                        "tool_calls_count": partial.tool_calls_count,
                        "tools_used": partial.tools_used,
                        "persistence_error": serde_json::Value::Null,
                        "exit_code": serde_json::Value::Null,
                        "success": false,
                        "error_kind": serde_json::Value::Null,
                        "error": &error,
                    }),
                )?;
            }
            return Err(error);
        }
    };

    let exit_code = finalize_one_shot_stream_result_with_request_lease(
        profile,
        effective_model.as_deref(),
        &message,
        &mut sr,
        turn_start,
        request_session_execution_lease.as_ref(),
    );

    match output_format {
        "json" => {
            let json_output = final_json_output(&sr, exit_code);
            write_headless_stdout_line(
                &serde_json::to_string_pretty(&json_output).unwrap_or_default(),
            )?;
        }
        "stream-json" => {
            let emitter = stream_json_emitter.as_ref().ok_or_else(|| {
                "stream-json output selected without a protocol emitter".to_string()
            })?;
            emitter.emit_result(
                sr.session_id.as_deref(),
                final_stream_json_result(&sr, exit_code),
            )?;
        }
        _ => {
            // text mode: just the response
            write_headless_stdout(sr.full_text.as_bytes())?;
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
    stdout_println!("\n{}", "Astra Doctor".bold());
    stdout_println!("{}\n", "═".repeat(50).dim());
    let mut issues: Vec<String> = Vec::new();

    // 1. Version
    let version = env!("CARGO_PKG_VERSION");
    stdout_println!("{}", "Version".bold().magenta());
    stdout_println!("  {} {}", "Binary:".dim(), version);
    stdout_println!(
        "  {} {}",
        "Executable:".dim(),
        std::env::current_exe()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| "unknown".into())
    );
    stdout_println!();

    // 2. API server connectivity
    stdout_println!("{}", "API Server".bold().magenta());
    stdout_println!("  {} {}", "URL:".dim(), api.api_origin());
    match api.get_health_text().await {
        Ok(body) => stdout_println!(
            "  {} {} {}",
            "Status:".dim(),
            theme::icon_ok(),
            format!("Healthy ({})", body.trim()).green()
        ),
        Err(e) => {
            stdout_println!(
                "  {} {} {}",
                "Status:".dim(),
                "✗".red(),
                "Unreachable".red()
            );
            issues.push(format!("API server unreachable: {e}"));
        }
    }
    stdout_println!();

    // 3. Authentication
    stdout_println!("{}", "Authentication".bold().magenta());
    let creds = load_credentials();
    let name = profile_name(profile, &creds);
    stdout_println!("  {} {}", "Profile:".dim(), name);
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
                        stdout_println!(
                            "  {} {} {}",
                            "Status:".dim(),
                            theme::icon_ok(),
                            format!("Logged in as {user}").green()
                        );
                    } else {
                        stdout_println!(
                            "  {} {} {}",
                            "Status:".dim(),
                            theme::icon_ok(),
                            "Authenticated".green()
                        );
                    }
                }
                Err(_) => {
                    stdout_println!(
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
            stdout_println!(
                "  {} {} {}",
                "Status:".dim(),
                "✗".red(),
                "Not logged in".red()
            );
            issues.push(format!("Not authenticated: {e}"));
        }
    }
    stdout_println!();

    // 4. Project config
    stdout_println!("{}", "Project Configuration".bold().magenta());
    let cwd = std::env::current_dir().unwrap_or_default();
    let astra_dir = cwd.join(".astra");
    if astra_dir.is_dir() {
        stdout_println!(
            "  {} {} {}",
            ".astra/:".dim(),
            theme::icon_ok(),
            "Found".green()
        );
    } else {
        stdout_println!("  {} {}", ".astra/:".dim(), "Not found (optional)".dim());
    }
    stdout_println!("  {} {}", "Working dir:".dim(), cwd.display());
    stdout_println!();

    // 5. MCP configuration
    stdout_println!("{}", "MCP Configuration".bold().magenta());
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
                            stdout_println!(
                                "  {} {} {} in {}",
                                scope,
                                theme::icon_ok(),
                                format!("{count} server(s)").green(),
                                path.display().to_string().dim()
                            );
                        }
                        Err(e) => {
                            stdout_println!(
                                "  {} {} {}",
                                scope,
                                "✗".red(),
                                format!("Invalid JSON in {}", path.display()).red()
                            );
                            issues.push(format!("MCP {scope} config parse error: {e}"));
                        }
                    },
                    Err(e) => {
                        stdout_println!(
                            "  {} {} {}",
                            scope,
                            "✗".red(),
                            format!("Cannot read {}", path.display()).red()
                        );
                        issues.push(format!("MCP {scope} config read error: {e}"));
                    }
                }
            } else {
                stdout_println!("  {} {}", scope, "No config file".dim());
            }
        }
    }
    stdout_println!();

    // 6. Environment
    stdout_println!("{}", "Environment".bold().magenta());
    stdout_println!("  {} {}", "OS:".dim(), std::env::consts::OS);
    stdout_println!("  {} {}", "Arch:".dim(), std::env::consts::ARCH);
    if let Ok(shell) = std::env::var("SHELL") {
        stdout_println!("  {} {shell}", "Shell:".dim());
    }
    if let Ok(term) = std::env::var("TERM") {
        stdout_println!("  {} {term}", "Terminal:".dim());
    }
    stdout_println!();

    // Summary
    if issues.is_empty() {
        stdout_println!("{} {}", theme::icon_ok().bold(), "No issues found".green());
    } else {
        stdout_println!(
            "{} {}:",
            "Found".yellow(),
            format!("{} issue(s)", issues.len()).yellow().bold()
        );
        for issue in &issues {
            stdout_println!("  {} {}", theme::icon_warn(), issue);
        }
    }
}

#[cfg(test)]
mod exit_code_tests {
    use super::{
        ExitCode, StreamResult, WALL_BUDGET_REQUEST_SAFETY_MARGIN,
        WALL_DEADLINE_SERVER_TERMINAL_SETTLE_MARGIN, apply_wall_deadline_interruption,
        compute_exit_code, durable_run_is_terminal, stream_result_from_incremental_snapshot,
    };
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
    fn wall_deadline_is_a_typed_partial_even_with_no_terminal_server_frame() {
        let mut sr = empty_stream_result();
        apply_wall_deadline_interruption(&mut sr, false);

        assert_eq!(sr.final_state, "interrupted");
        assert_eq!(
            sr.interruption_kind.as_deref(),
            Some("execution_incomplete")
        );
        assert!(sr.server_terminal_unverified);
        assert!(!sr.full_text.is_empty());
        assert_eq!(compute_exit_code(&sr), ExitCode::Partial);
        assert_eq!(
            sr.interruption
                .as_ref()
                .and_then(|value| value.get("reason"))
                .and_then(serde_json::Value::as_str),
            Some("request_wall_deadline_reached")
        );
    }

    #[test]
    fn cancellation_requested_is_not_a_terminal_durable_run_state() {
        assert!(!durable_run_is_terminal(Some("cancellation_requested")));
        assert!(!durable_run_is_terminal(Some("running")));
        assert!(!durable_run_is_terminal(None));
        assert!(durable_run_is_terminal(Some("completed")));
        assert!(durable_run_is_terminal(Some("failed")));
        assert!(durable_run_is_terminal(Some("cancelled")));
    }

    #[test]
    fn wall_deadline_keeps_a_small_terminal_serialization_margin() {
        assert_eq!(
            WALL_DEADLINE_SERVER_TERMINAL_SETTLE_MARGIN,
            std::time::Duration::from_secs(5)
        );
        assert!(
            WALL_DEADLINE_SERVER_TERMINAL_SETTLE_MARGIN < std::time::Duration::from_secs(70),
            "terminal settlement must leave time for final CLI serialization"
        );
        assert_eq!(
            WALL_BUDGET_REQUEST_SAFETY_MARGIN,
            std::time::Duration::from_secs(2),
            "request admission needs a small margin beyond terminal settlement"
        );
    }

    #[test]
    fn wall_deadline_preserves_a_verified_server_terminal_state() {
        let mut sr = empty_stream_result();

        apply_wall_deadline_interruption(&mut sr, true);

        assert_eq!(sr.final_state, "interrupted");
        assert!(!sr.server_terminal_unverified);
    }

    #[test]
    fn wall_deadline_remains_partial_when_settlement_cancelled_the_last_tool() {
        let mut sr = empty_stream_result();
        sr.tool_call_records
            .push(astra_services::session_journal::ToolCallRecord {
                ok: false,
                exit_semantics: Some("cancelled".to_string()),
                disposition: Some(astra_services::session_journal::ToolCallDisposition::Executed),
                ..Default::default()
            });

        apply_wall_deadline_interruption(&mut sr, false);

        assert_eq!(compute_exit_code(&sr), ExitCode::Partial);
    }

    #[test]
    fn wall_deadline_snapshot_preserves_run_usage_text_and_partial_coverage() {
        let snapshot = astra_turn_core::turn_event_sink::TurnIncrementalSnapshot {
            prompt_tokens: 123,
            completion_tokens: 45,
            cache_read_tokens: 67,
            cache_creation_tokens: 8,
            llm_rounds: Some(4),
            tool_calls_count: 201,
            token_usage_coverage: astra_turn_core::chat_turn_sse_dispatch::TokenUsageCoverage {
                attempts: 4,
                provider_reported: 3,
                unavailable: 1,
            },
            partial_text: "partial answer".to_string(),
            tool_call_records: vec![Default::default()],
            tools_used: vec!["bash".to_string()],
            session_id: Some("session-1".to_string()),
            run_id: Some("run-1".to_string()),
        };

        let sr = stream_result_from_incremental_snapshot(snapshot);
        assert_eq!(sr.session_id.as_deref(), Some("session-1"));
        assert_eq!(sr.run_id.as_deref(), Some("run-1"));
        assert_eq!(sr.prompt_tokens, 123);
        assert_eq!(sr.completion_tokens, 45);
        assert_eq!(sr.full_text, "partial answer");
        assert_eq!(sr.tool_calls_count, 201);
        assert_eq!(sr.llm_rounds, Some(4));
        assert_eq!(sr.token_usage_coverage.attempts, 4);
        assert_eq!(sr.token_usage_coverage.provider_reported, 3);
        assert!(sr.tool_record_coverage_partial);
    }

    #[test]
    fn server_terminal_authority_overrides_stale_local_failure() {
        let mut sr = empty_stream_result();
        sr.server_terminal_authoritative = true;
        sr.tool_call_records
            .push(astra_services::session_journal::ToolCallRecord {
                name: "bash".to_string(),
                ok: false,
                result_class: Some("execution_error".to_string()),
                ..Default::default()
            });

        assert_eq!(compute_exit_code(&sr), ExitCode::Success);
        assert_eq!(
            super::completion_disposition(&sr, ExitCode::Success),
            "completed"
        );

        sr.server_terminal_unverified = true;
        assert_eq!(
            super::completion_disposition(&sr, ExitCode::Success),
            "responded_unverified"
        );
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
    fn exit_code_persistence_error_overrides_interrupted_tool_failure() {
        let mut sr = empty_stream_result();
        sr.final_state = "interrupted".into();
        sr.interruption_kind = Some("execution_incomplete".into());
        sr.session_persistence_error = Some("failed to append one-shot journal events".into());
        sr.tool_call_records
            .push(astra_services::session_journal::ToolCallRecord {
                name: "Bash".to_string(),
                ok: false,
                ms: 100,
                error: Some("exit code 1".to_string()),
                ..Default::default()
            });
        assert_eq!(compute_exit_code(&sr), ExitCode::PersistenceError);
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
    fn strong_behavior_advisory_does_not_override_tool_failure() {
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
            advisory_threshold_reached: true,
            nudge_count: 0,
            interaction_mode: "prompt".to_string(),
            total_errors: 3,
            health_avoidance_count: 0,
            recent_error_pressure: 0,
            recent_timeout_pressure: 0,
            total_timeouts: 0,
            timeout_dominant_tools: vec![],
            total_cache_hits: 0,
            flaky_count: 0,
        });
        assert_eq!(compute_exit_code(&sr), ExitCode::ToolFailure);
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
    fn exit_code_success_with_behavior_advisory() {
        let mut sr = empty_stream_result();
        sr.verdict_events.push(VerdictEvent {
            turn: 1,
            severity: "warning".to_string(),
            injections: vec![],
            avoid_tools: vec![],
            health_avoidance_tools: vec![],
            advisory_threshold_reached: false,
            nudge_count: 1,
            interaction_mode: "prompt".to_string(),
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
    use super::{ExitCode, StreamResult, final_json_output_with_context, final_stream_json_result};

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
            tool_ledger_aggregate:
                astra_turn_core::tool_ledger_receipt::ToolLedgerCanonicalAggregate {
                    attempted: 2,
                    terminal: 2,
                    unresolved: 0,
                    result_classes:
                        astra_turn_core::tool_ledger_receipt::ToolLedgerResultClassCounts {
                            succeeded: 2,
                            ..Default::default()
                        },
                    consistent: true,
                },
            tools_used: vec!["bash".to_string(), "read_file".to_string()],
            llm_rounds: Some(3),
            token_usage_coverage: astra_turn_core::chat_turn_sse_dispatch::TokenUsageCoverage {
                attempts: 3,
                provider_reported: 2,
                unavailable: 1,
            },
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
        assert_eq!(
            output["tool_result_class_counts"],
            serde_json::json!({
                "succeeded": 2,
                "failed": 0,
                "rejected": 0,
                "reused": 0,
                "suppressed": 0,
            })
        );
        assert_eq!(output["prompt_tokens"], 13);
        assert_eq!(output["fresh_prompt_tokens"], 10);
        assert!(output.get("cached_input_tokens").is_none());
        assert!(output.get("cache_creation_tokens").is_none());
        assert_eq!(output["cache"]["hit"], true);
        assert_eq!(output["cache"]["read_tokens"], 2);
        assert_eq!(output["cache"]["creation_tokens"], 1);
        assert_eq!(output["completion_tokens"], 3);
        assert_eq!(output["token_usage_coverage"]["status"], "partial");
        assert_eq!(output["token_usage_coverage"]["attempts"], 3);
        assert_eq!(output["token_usage_coverage"]["provider_reported"], 2);
        assert_eq!(output["token_usage_coverage"]["unavailable"], 1);
        assert_eq!(output["llm_rounds"], 3);
        assert_eq!(output["tool_calls_count"], 2);
        assert_eq!(output["tool_record_coverage"], "complete");
        assert_eq!(
            output["tools_used"],
            serde_json::json!(["bash", "read_file"])
        );
        assert_eq!(output["exit_code"], 0);
        assert_eq!(output["success"], true);
        assert_eq!(output["completion_disposition"], "completed");
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
            "token_usage_coverage",
            "llm_rounds",
            "tool_calls_count",
            "tools_used",
            "exit_code",
            "success",
            "completion_disposition",
            "error_kind",
        ] {
            assert!(output.get(field).is_some(), "missing {field}");
        }
    }

    #[test]
    fn final_json_marks_partial_tool_coverage_independently_of_terminal_authority() {
        let mut sr = stream_result_for_json();
        sr.tool_record_coverage_partial = true;
        sr.server_terminal_authoritative = false;

        let output = final_json_output_with_context(&sr, ExitCode::Success, None, None);

        assert_eq!(output["tool_record_coverage"], "partial");
        assert_eq!(output["server_terminal_authoritative"], false);
    }

    #[test]
    fn final_json_preserves_authoritative_terminal_with_partial_local_coverage() {
        let mut sr = stream_result_for_json();
        // The remote server owns the complete execution ledger; this flag
        // only describes the thin client's local per-call projection.
        sr.tool_record_coverage_partial = true;
        sr.server_terminal_authoritative = true;

        let completed = final_json_output_with_context(&sr, ExitCode::Success, None, None);
        assert_eq!(completed["tool_record_coverage"], "partial");
        assert_eq!(completed["completion_disposition"], "completed");
        assert_eq!(completed["success"], true);

        sr.server_terminal_unverified = true;
        let unverified = final_json_output_with_context(&sr, ExitCode::Success, None, None);
        assert_eq!(unverified["completion_disposition"], "responded_unverified");
        assert_eq!(unverified["success"], false);
    }

    #[test]
    fn final_json_uses_canonical_result_classes_without_local_record_inference() {
        let mut sr = stream_result_for_json();
        sr.tool_call_records.clear();
        sr.tool_ledger_aggregate.result_classes =
            astra_turn_core::tool_ledger_receipt::ToolLedgerResultClassCounts {
                succeeded: 1,
                failed: 1,
                ..Default::default()
            };

        let output = final_json_output_with_context(&sr, ExitCode::Success, None, None);

        assert_eq!(output["tool_result_class_counts"]["succeeded"], 1);
        assert_eq!(output["tool_result_class_counts"]["failed"], 1);
        assert_eq!(output["tool_calls_count"], 2);
    }

    #[test]
    fn execution_incomplete_receipt_can_never_serialize_as_success() {
        let mut sr = stream_result_for_json();
        sr.final_state = "interrupted".into();
        sr.interruption_kind = Some("execution_incomplete".into());
        sr.interruption = Some(serde_json::json!({
            "kind": "execution_incomplete",
            "reason": "remote_tool_receipt_unresolved",
            "resumable": true,
        }));
        sr.server_terminal_authoritative = false;
        sr.server_terminal_unverified = true;

        let output = final_json_output_with_context(&sr, ExitCode::Partial, None, None);

        assert_eq!(output["final_state"], "interrupted");
        assert_eq!(output["completion_disposition"], "interrupted");
        assert_eq!(output["success"], false);
        assert_eq!(output["exit_code"], 5);
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

    #[test]
    fn final_json_output_marks_active_exact_failure_as_unverified() {
        let mut sr = stream_result_for_json();
        sr.tool_call_records
            .push(astra_services::session_journal::ToolCallRecord {
                name: "bash".to_string(),
                ok: false,
                args_full: Some(serde_json::json!({"command": "cargo test"}).to_string()),
                result_class: Some("execution_error".to_string()),
                error: Some("command failed".to_string()),
                ..Default::default()
            });
        let output = final_json_output_with_context(&sr, ExitCode::Success, None, None);

        assert_eq!(output["completion_disposition"], "responded_unverified");
        assert_eq!(output["success"], false);
        assert_eq!(output["exit_code"], 0);
    }

    #[test]
    fn final_json_output_marks_exact_failure_recovery_completed() {
        let mut sr = stream_result_for_json();
        let args = serde_json::json!({"command": "cargo test"}).to_string();
        sr.tool_call_records
            .push(astra_services::session_journal::ToolCallRecord {
                name: "bash".to_string(),
                ok: false,
                args_full: Some(args.clone()),
                result_class: Some("execution_error".to_string()),
                ..Default::default()
            });
        sr.tool_call_records
            .push(astra_services::session_journal::ToolCallRecord {
                name: "bash".to_string(),
                ok: true,
                args_full: Some(args),
                result_class: Some("success".to_string()),
                ..Default::default()
            });
        let output = final_json_output_with_context(&sr, ExitCode::Success, None, None);

        assert_eq!(output["completion_disposition"], "completed");
        assert_eq!(output["success"], true);
    }

    #[test]
    fn final_json_output_preserves_server_owned_unverified_fact() {
        let mut sr = stream_result_for_json();
        sr.server_terminal_unverified = true;
        let output = final_json_output_with_context(&sr, ExitCode::Success, None, None);

        assert_eq!(output["completion_disposition"], "responded_unverified");
        assert_eq!(output["server_terminal_unverified"], true);
        assert_eq!(output["success"], false);
        assert_eq!(output["exit_code"], 0);
    }

    #[test]
    fn stream_json_result_does_not_alias_local_execution_as_durable_run() {
        let sr = stream_result_for_json();
        let output = final_stream_json_result(&sr, ExitCode::Success);

        assert!(output.get("run_id").is_none());
        assert_eq!(output["session_id"], "session-1");
        assert_eq!(output["success"], true);
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
            effective_one_shot_permission_mode(Some("bypass"), false, Some("plan"), false).unwrap(),
            PermissionMode::Bypass
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
        ExitCode, HeadlessCanonicalCommitStatus, StreamResult, apply_wall_deadline_interruption,
        finalize_one_shot_stream_result, finalize_one_shot_stream_result_with_request_lease,
        persist_headless_session_state, retain_wall_deadline_partial_canonical_messages,
        stream_result_from_incremental_snapshot,
    };

    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    fn execution_lease(session_id: &str) -> astra_services::session_journal::SessionExecutionLease {
        astra_services::session_journal::SessionExecutionLease::try_acquire(session_id).unwrap()
    }

    #[test]
    #[serial_test::serial]
    fn one_shot_settlement_persists_canonical_tool_evidence_for_next_process() {
        let (_sessions, _sessions_guard) = crate::tests::isolated_sessions_dir();
        let sid = format!("one-shot-csl-tools-{}", uuid::Uuid::new_v4());
        let lease = execution_lease(&sid);
        let mut result = crate::tests::stub_stream_result("manifest inspected");
        result.session_id = Some(sid.clone());
        result.tools_used = vec!["read_file".to_string()];
        result.activated_deferred_tool_names = vec!["github".to_string()];
        result.tool_calls_count = 1;
        result.final_messages = vec![
            serde_json::json!({"role": "user", "content": "inspect Cargo.toml"}),
            serde_json::json!({
                "role": "assistant",
                "tool_calls": [{
                    "id": "call-1",
                    "type": "function",
                    "function": {
                        "name": "read_file",
                        "arguments": "{\"path\":\"Cargo.toml\"}"
                    }
                }]
            }),
            serde_json::json!({
                "role": "tool",
                "tool_call_id": "call-1",
                "content": "[package]\nname = \"astra\""
            }),
            serde_json::json!({"role": "assistant", "content": "manifest inspected"}),
        ];

        let settlement = persist_headless_session_state(
            None,
            Some("test-model"),
            "inspect Cargo.toml",
            &mut result,
            std::time::Instant::now(),
            Some(&lease),
        );

        assert_eq!(result.session_persistence_error, None);
        assert_eq!(
            settlement.commit_status,
            HeadlessCanonicalCommitStatus::Committed
        );
        assert!(!settlement.projection_repair_required);
        assert_eq!(
            settlement.canonical_session_id.as_deref(),
            Some(sid.as_str())
        );
        assert_eq!(settlement.persistence_error, None);
        let restored =
            crate::cli::session::session_continuation::load_session_messages_for_continuation(&sid)
                .expect("one-shot canonical continuation");
        let tool_call = restored
            .iter()
            .find(|message| message.get("tool_calls").is_some())
            .expect("assistant tool call");
        let tool_result = restored
            .iter()
            .find(|message| message["role"] == "tool")
            .expect("paired tool result");
        assert_eq!(tool_call["tool_calls"][0]["id"], "call-1");
        assert_eq!(tool_result["tool_call_id"], "call-1");
        assert_eq!(tool_result["content"], "[package]\nname = \"astra\"");
        assert_eq!(
            crate::cli::session::session_continuation::load_csl_continuation(&sid)
                .unwrap()
                .unwrap()
                .activated_deferred_tool_names,
            vec!["github"],
            "one-shot settlement must make deferred activation durable for the next process"
        );
    }

    #[test]
    #[serial_test::serial]
    fn fresh_wall_deadline_partial_commits_and_restores_partial_canonical_output() {
        let (_sessions, _sessions_guard) = crate::tests::isolated_sessions_dir();
        let sid = format!("fresh-wall-partial-{}", uuid::Uuid::new_v4());
        let request_lease =
            crate::cli::session::session_execution_lease::RequestSessionExecutionLease::new(None)
                .unwrap();
        request_lease.bind(&sid).unwrap();
        let mut result = stream_result_from_incremental_snapshot(
            astra_turn_core::turn_event_sink::TurnIncrementalSnapshot {
                session_id: Some(sid.clone()),
                run_id: Some("run-fresh-wall-partial".to_string()),
                partial_text: "partial answer before deadline".to_string(),
                ..Default::default()
            },
        );
        apply_wall_deadline_interruption(&mut result, false);
        retain_wall_deadline_partial_canonical_messages(&mut result, "perform bounded work");

        let exit_code = finalize_one_shot_stream_result_with_request_lease(
            None,
            Some("test-model"),
            "perform bounded work",
            &mut result,
            std::time::Instant::now(),
            request_lease.as_ref(),
        );

        assert_eq!(exit_code, ExitCode::Partial);
        assert_eq!(result.session_persistence_error, None);
        let restored =
            crate::cli::session::session_continuation::load_session_messages_for_continuation(&sid)
                .expect("fresh partial canonical continuation");
        assert_eq!(
            restored
                .last()
                .and_then(|message| message["content"].as_str()),
            Some("partial answer before deadline")
        );
        assert!(
            astra_services::session_journal::SessionExecutionLease::try_acquire(&sid).is_err(),
            "request authority remains held after canonical commit until request teardown"
        );
    }

    #[test]
    #[serial_test::serial]
    fn interrupted_transport_partial_commits_resumable_canonical_output() {
        let (_sessions, _sessions_guard) = crate::tests::isolated_sessions_dir();
        let sid = format!("transport-partial-{}", uuid::Uuid::new_v4());
        let lease = execution_lease(&sid);
        let mut result =
            crate::tests::stub_stream_result("partial reasoning before transport loss");
        result.session_id = Some(sid.clone());
        result.final_state = "interrupted".to_string();
        result.interruption_kind = Some("stream_transport".to_string());
        result.server_terminal_unverified = true;

        let exit_code = finalize_one_shot_stream_result(
            None,
            Some("test-model"),
            "continue the workspace task",
            &mut result,
            std::time::Instant::now(),
            Some(&lease),
        );

        assert_eq!(exit_code, ExitCode::Partial);
        assert_eq!(result.session_persistence_error, None);
        assert!(result.server_terminal_unverified);
        let restored =
            crate::cli::session::session_continuation::load_session_messages_for_continuation(&sid)
                .expect("transport continuation");
        assert_eq!(restored[0]["content"], "continue the workspace task");
        assert_eq!(
            restored
                .last()
                .and_then(|message| message["content"].as_str()),
            Some("partial reasoning before transport loss")
        );
    }

    #[test]
    #[serial_test::serial]
    fn remote_persistence_degradation_withholds_durable_session_pointer() {
        let _home = crate::tests::HomeGuard::temp();
        let sessions = dirs::home_dir().unwrap().join(".astra").join("sessions");
        std::fs::create_dir_all(&sessions).unwrap();
        let _guard = astra_services::session_journal::JournalDirGuard::new(&sessions);

        let sid = format!("one-shot-local-recovery-{}", uuid::Uuid::new_v4());
        let lease = execution_lease(&sid);
        let mut result = crate::tests::stub_stream_result("locally recoverable answer");
        result.session_id = Some(sid.clone());
        result.session_persistence_error = Some("remote journal returned 503".into());
        result.final_messages = vec![
            serde_json::json!({"role": "user", "content": "keep this turn"}),
            serde_json::json!({"role": "assistant", "content": "locally recoverable answer"}),
        ];

        let settlement = persist_headless_session_state(
            Some("default"),
            Some("test-model"),
            "keep this turn",
            &mut result,
            std::time::Instant::now(),
            Some(&lease),
        );

        assert_eq!(
            settlement.commit_status,
            HeadlessCanonicalCommitStatus::Committed
        );
        assert!(settlement.projection_repair_required);
        assert_eq!(
            settlement.canonical_session_id.as_deref(),
            Some(sid.as_str())
        );
        assert_eq!(
            settlement.persistence_error.as_deref(),
            Some("remote journal returned 503")
        );
        assert_eq!(
            result.session_persistence_error.as_deref(),
            Some("remote journal returned 503"),
            "local recovery must not hide the remote durability degradation"
        );
        let credentials = crate::cli::cli_config::cli_utils::load_credentials();
        assert_eq!(
            credentials
                .profiles
                .get("default")
                .and_then(|profile| profile.last_session_id.as_deref()),
            None,
            "a degraded settlement must not publish a session pointer as fully durable"
        );
        let restored =
            crate::cli::session::session_continuation::load_session_messages_for_continuation(&sid)
                .expect("local continuation");
        assert_eq!(
            restored.last().unwrap()["content"],
            "locally recoverable answer"
        );
    }

    #[test]
    #[serial_test::serial]
    fn missing_canonical_messages_withhold_durable_session_pointer() {
        let _home = crate::tests::HomeGuard::temp();
        let sessions = dirs::home_dir().unwrap().join(".astra").join("sessions");
        std::fs::create_dir_all(&sessions).unwrap();
        let _guard = astra_services::session_journal::JournalDirGuard::new(&sessions);

        let sid = format!("one-shot-no-canonical-messages-{}", uuid::Uuid::new_v4());
        let lease = execution_lease(&sid);
        let mut result = crate::tests::stub_stream_result("answer missing canonical history");
        result.session_id = Some(sid.clone());

        let settlement = persist_headless_session_state(
            Some("default"),
            Some("test-model"),
            "keep this turn",
            &mut result,
            std::time::Instant::now(),
            Some(&lease),
        );

        assert_eq!(
            settlement.commit_status,
            HeadlessCanonicalCommitStatus::Committed
        );
        assert!(settlement.projection_repair_required);
        assert_eq!(
            settlement.canonical_session_id.as_deref(),
            Some(sid.as_str())
        );
        assert!(
            result
                .session_persistence_error
                .as_deref()
                .unwrap_or_default()
                .contains("canonical messages are empty")
        );
        assert!(
            astra_services::session_journal::read_journal(&sid)
                .unwrap()
                .iter()
                .any(|event| event.event_type
                    == astra_services::session_journal::JournalEventType::Turn),
            "journal success must not be mistaken for complete canonical recovery"
        );
        assert_eq!(
            crate::cli::cli_config::cli_utils::load_credentials()
                .profiles
                .get("default")
                .and_then(|profile| profile.last_session_id.as_deref()),
            None
        );
    }

    #[cfg(unix)]
    #[test]
    #[serial_test::serial]
    fn persist_headless_session_state_marks_stream_result_and_skips_pointer_update_on_append_failure()
     {
        let _home = crate::tests::HomeGuard::temp();
        let sessions = dirs::home_dir().unwrap().join(".astra").join("sessions");
        std::fs::create_dir_all(&sessions).unwrap();
        let _guard = astra_services::session_journal::JournalDirGuard::new(&sessions);

        let sid = format!("one-shot-persist-{}", uuid::Uuid::new_v4());
        let lease = execution_lease(&sid);
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
            tool_ledger_aggregate: Default::default(),
            visible_tools: Vec::new(),
            selected_skills: Vec::new(),
            tools_used: Vec::new(),
            activated_deferred_tool_names: Vec::new(),
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
            token_usage_coverage: Default::default(),
            interruption: None,
            final_state: "completed".into(),
            interruption_kind: None,
            server_terminal_unverified: false,
            server_terminal_authoritative: false,
            tool_record_coverage_partial: false,
            final_messages: Vec::new(),
            run_transcript_messages: Vec::new(),
            applied_user_intents: Vec::new(),
            background_agent_results: Vec::new(),
        };

        let settlement = persist_headless_session_state(
            Some("default"),
            Some("test-model"),
            "continue",
            &mut sr,
            std::time::Instant::now(),
            Some(&lease),
        );

        assert_eq!(
            settlement.commit_status,
            HeadlessCanonicalCommitStatus::Unknown
        );
        assert!(!settlement.projection_repair_required);
        assert_eq!(settlement.canonical_session_id, None);
        assert!(
            sr.session_persistence_error
                .as_deref()
                .unwrap_or_default()
                .contains("canonical journal commit")
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
        let request_lease =
            crate::cli::session::session_execution_lease::RequestSessionExecutionLease::new(Some(
                &sid,
            ))
            .unwrap();
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
            tool_ledger_aggregate: Default::default(),
            visible_tools: Vec::new(),
            selected_skills: Vec::new(),
            tools_used: Vec::new(),
            activated_deferred_tool_names: Vec::new(),
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
            token_usage_coverage: Default::default(),
            interruption: None,
            final_state: "completed".into(),
            interruption_kind: None,
            server_terminal_unverified: false,
            server_terminal_authoritative: false,
            tool_record_coverage_partial: false,
            final_messages: Vec::new(),
            run_transcript_messages: Vec::new(),
            applied_user_intents: Vec::new(),
            background_agent_results: Vec::new(),
        };

        let exit_code = finalize_one_shot_stream_result_with_request_lease(
            Some("default"),
            Some("test-model"),
            "continue",
            &mut sr,
            std::time::Instant::now(),
            request_lease.as_ref(),
        );

        assert_eq!(exit_code, ExitCode::PersistenceError);
        assert!(
            sr.session_persistence_error
                .as_deref()
                .unwrap_or_default()
                .contains("canonical journal commit")
        );

        std::fs::set_permissions(&journal_path, std::fs::Permissions::from_mode(0o644)).unwrap();
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
