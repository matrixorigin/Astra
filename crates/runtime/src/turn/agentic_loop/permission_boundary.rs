//! Permission interaction changes are captured once before round preparation.
//! Guidance/action epochs remain independent of this control lane.

use super::host::{AgenticLoopHost, AgenticLoopState, VolatileKind};
use astra_turn_types::RunPermissionModeApplied;

fn boundary_error(message: impl std::fmt::Display) -> astra_core::ClassifiedError {
    astra_core::ClassifiedError::new(
        astra_core::ErrorKind::ContractViolation,
        format!("permission-mode boundary failed: {message}"),
    )
}

pub(crate) async fn apply_round_permission_mode<H: AgenticLoopHost>(
    host: &mut H,
    state: &mut AgenticLoopState,
) -> Result<(), astra_core::ClassifiedError> {
    // A root user's interaction choice must never replace inherited child policy.
    if state.recursion_depth != 0 {
        return Ok(());
    }
    let (Some(control), Some(user_id), Some(run_id)) = (
        state.run_control.clone(),
        state.context_manifest_user_id.clone(),
        state.current_run_id.clone(),
    ) else {
        return Ok(());
    };
    let session_id = state
        .current_session_id
        .clone()
        .ok_or_else(|| boundary_error("run has no immutable session identity"))?;
    let Some(snapshot) = control
        .permission_mode_snapshot(&user_id, &session_id, &run_id)
        .await
        .map_err(boundary_error)?
    else {
        return Ok(());
    };
    // Applied state hydrates a recovered executor even if there is no pending
    // request. Capture once: requests arriving during this round wait for the next.
    let selection = match (snapshot.requested, snapshot.applied.as_ref()) {
        (Some(requested), Some(applied)) if requested.revision < applied.selection.revision => {
            applied.selection.clone()
        }
        (Some(requested), _) => requested,
        (None, Some(applied)) => applied.selection.clone(),
        (None, None) => return Ok(()),
    };
    if state.applied_permission_mode.as_ref() != Some(&selection) {
        let generation = state
            .current_run_owner_generation
            .ok_or_else(|| boundary_error("durable selection has no execution-owner generation"))?;
        host.apply_permission_mode(selection.mode)
            .map_err(boundary_error)?;
        if let Some(context) = state.permission_context.as_ref() {
            // Preserve inherited/session allow/deny rules, tool allowlists,
            // fingerprint decisions, telemetry, and skill restrictions.
            context.write().await.inherited.mode = selection.mode;
        } else {
            state.permission_context = Some(
                crate::orchestration::PermissionSyncContext::shared_root(selection.mode),
            );
        }
        if !control
            .apply_permission_mode(
                &user_id,
                &session_id,
                &run_id,
                generation,
                &selection,
                state.current_round_index,
            )
            .await
            .map_err(boundary_error)?
        {
            return Err(boundary_error(
                "durable application rejected execution authority",
            ));
        }
        let applied = RunPermissionModeApplied {
            selection: selection.clone(),
            round_index: state.current_round_index,
            owner_generation: generation,
        };
        state.applied_permission_mode = Some(selection.clone());
        host.on_permission_mode_applied(state, &applied).await;
    }
    // A volatile system fact leaves the canonical cacheable prefix and tool
    // schemas stable. It is descriptive and grants no additional authority.
    state.push_volatile_payload(
        VolatileKind::PermissionMode,
        serde_json::json!({
            "schema": "permission_mode.v1",
            "mode": selection.mode,
            "revision": selection.revision,
            "scope": "current_model_round",
            "policy": "Explicit deny rules, tool allowlists, skill restrictions, and safety policy remain enforced."
        }),
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::turn::agentic::headless_round::HeadlessStderrStyle;
    use crate::turn::agentic_loop::host::{HostTurnResult, make_test_loop_state};
    use crate::turn::run_control::{
        RunControlStatus, RunStatusProvider, UserIntentAdmissionAuthority, UserIntentApplyAck,
        UserIntentPoll, UserIntentProvider,
    };
    use astra_turn_types::{PermissionMode, RunPermissionModeSelection, RunPermissionModeSnapshot};
    use async_trait::async_trait;
    use std::collections::HashSet;
    use std::sync::{Arc, Mutex};

    #[derive(Default)]
    struct Control {
        snapshot: Mutex<RunPermissionModeSnapshot>,
        acknowledgements: Mutex<Vec<(u64, i64, u32)>>,
        reject_ack: std::sync::atomic::AtomicBool,
    }
    impl Control {
        fn request(&self, mode: PermissionMode, revision: i64) {
            self.snapshot.lock().unwrap().requested = Some(RunPermissionModeSelection {
                mode,
                revision,
                request_id: format!("request-{revision}"),
            });
        }
    }
    #[async_trait]
    impl RunStatusProvider for Control {
        async fn control_status(
            &self,
            _: &str,
            _: &str,
        ) -> Result<Option<RunControlStatus>, String> {
            Ok(None)
        }
        async fn permission_mode_snapshot(
            &self,
            _: &str,
            _: &str,
            _: &str,
        ) -> Result<Option<RunPermissionModeSnapshot>, String> {
            Ok(Some(self.snapshot.lock().unwrap().clone()))
        }
        async fn apply_permission_mode(
            &self,
            _: &str,
            _: &str,
            _: &str,
            generation: u64,
            selection: &RunPermissionModeSelection,
            round: u32,
        ) -> Result<bool, String> {
            if self.reject_ack.load(std::sync::atomic::Ordering::SeqCst) {
                return Ok(false);
            }
            self.acknowledgements
                .lock()
                .unwrap()
                .push((generation, selection.revision, round));
            self.snapshot.lock().unwrap().applied = Some(RunPermissionModeApplied {
                selection: selection.clone(),
                round_index: round,
                owner_generation: generation,
            });
            Ok(true)
        }
    }
    #[async_trait]
    impl UserIntentProvider for Control {
        async fn poll_user_intents(&self, _: &str, _: &str, _: usize) -> UserIntentPoll {
            UserIntentPoll::default()
        }
        async fn mark_user_intents_applied(
            &self,
            _: &str,
            _: &str,
            _: &str,
            _: &[usize],
            _: UserIntentAdmissionAuthority,
        ) -> Result<UserIntentApplyAck, String> {
            Ok(UserIntentApplyAck::Applied)
        }
    }
    struct Host {
        control: Arc<Control>,
        mode: PermissionMode,
        next: Option<PermissionMode>,
        observed: Vec<PermissionMode>,
        tools: HashSet<String>,
    }
    #[async_trait]
    impl AgenticLoopHost for Host {
        fn apply_permission_mode(&mut self, mode: PermissionMode) -> Result<(), String> {
            self.mode = mode;
            Ok(())
        }
        async fn execute_turn(
            &mut self,
            state: &mut AgenticLoopState,
        ) -> Result<HostTurnResult, astra_core::ClassifiedError> {
            let selected = state
                .permission_context
                .as_ref()
                .unwrap()
                .read()
                .await
                .mode();
            assert_eq!(selected, self.mode);
            assert!(
                !self.control.acknowledgements.lock().unwrap().is_empty(),
                "ack precedes model"
            );
            self.observed.push(selected);
            if let Some(next) = self.next.take() {
                self.control.request(next, 2);
                assert_eq!(
                    state
                        .permission_context
                        .as_ref()
                        .unwrap()
                        .read()
                        .await
                        .mode(),
                    selected,
                    "in-flight round stays immutable"
                );
            }
            Ok(HostTurnResult {
                accum: Default::default(),
                ttft_ms: None,
                edge_tool_round: Vec::new(),
                error_kind: None,
            })
        }
        fn emit_headless_line(&mut self, _: HeadlessStderrStyle, _: String) {}
        fn is_quiet(&self) -> bool {
            true
        }
        fn valid_tool_names(&self) -> &HashSet<String> {
            &self.tools
        }
    }
    fn fixture(mode: PermissionMode) -> (Host, AgenticLoopState) {
        let control = Arc::new(Control::default());
        control.request(mode, 1);
        let mut state = make_test_loop_state();
        state.context_manifest_user_id = Some("owner".into());
        state.current_session_id = Some("session".into());
        state.current_run_id = Some("run".into());
        state.current_run_owner_generation = Some(7);
        state.run_control = Some(control.clone());
        (
            Host {
                control,
                mode,
                next: None,
                observed: Vec::new(),
                tools: HashSet::from(["read_file".into()]),
            },
            state,
        )
    }
    #[tokio::test]
    async fn permission_mode_pending_selection_yields_to_cancellation() {
        let (mut host, mut state) = fixture(PermissionMode::Bypass);
        state.cancellation.flag = Some(Arc::new(std::sync::atomic::AtomicBool::new(true)));
        host.control
            .reject_ack
            .store(true, std::sync::atomic::Ordering::SeqCst);
        let prepared = super::super::lifecycle::prepare_turn_iteration(&mut host, &mut state, 0)
            .await
            .expect("cancellation must not attempt a permission acknowledgement");
        assert!(matches!(
            prepared,
            super::super::lifecycle::PreparedTurnIteration::Finished(
                super::super::host::AgenticLoopOutcome::Cancelled
            )
        ));
        assert!(state.applied_permission_mode.is_none());
        assert!(host.control.acknowledgements.lock().unwrap().is_empty());
        assert!(host.observed.is_empty());
    }

    async fn two_rounds(first: PermissionMode, next: PermissionMode) {
        let (mut host, mut state) = fixture(first);
        host.next = Some(next);
        let prefix = state.messages.clone();
        let tools = host.tools.clone();
        apply_round_permission_mode(&mut host, &mut state)
            .await
            .unwrap();
        host.execute_turn(&mut state).await.unwrap();
        assert_eq!(host.mode, first);
        state.current_round_index = 1;
        apply_round_permission_mode(&mut host, &mut state)
            .await
            .unwrap();
        host.execute_turn(&mut state).await.unwrap();
        assert_eq!(host.observed, [first, next]);
        assert_eq!(
            *host.control.acknowledgements.lock().unwrap(),
            [(7, 1, 0), (7, 2, 1)]
        );
        assert_eq!(state.messages, prefix);
        assert_eq!(host.tools, tools);
        assert_eq!(
            state
                .volatile_pending
                .iter()
                .filter(|note| note.kind == VolatileKind::PermissionMode)
                .count(),
            1
        );
    }
    #[tokio::test]
    async fn permission_mode_prompt_to_bypass_at_next_mock_model_round() {
        two_rounds(PermissionMode::Prompt, PermissionMode::Bypass).await;
    }
    #[tokio::test]
    async fn permission_mode_bypass_to_deny_at_next_mock_model_round() {
        two_rounds(PermissionMode::Bypass, PermissionMode::Deny).await;
    }
    #[tokio::test]
    async fn permission_mode_preserves_denies_allowlist_and_child_restrictions() {
        use crate::orchestration::{PermissionRule, PermissionSyncContext};
        let (mut host, mut state) = fixture(PermissionMode::Bypass);
        let mut context = PermissionSyncContext::root(PermissionMode::Prompt);
        context.record_blocked_tool_with_reason("bash", Some("explicit deny"));
        context
            .inherited
            .deny_rules
            .push(PermissionRule::tool("bash"));
        context.inherited.allowed_tools = Some(HashSet::from(["read_file".into()]));
        state.permission_context = Some(context.into_shared());
        apply_round_permission_mode(&mut host, &mut state)
            .await
            .unwrap();
        let context = state.permission_context.as_ref().unwrap().read().await;
        assert_eq!(context.telemetry().tools_blocked, 1);
        assert!(context.is_denied("bash", Some("echo test")));
        assert_eq!(
            context.inherited.allowed_tools,
            Some(HashSet::from(["read_file".into()]))
        );
        assert_eq!(context.for_child(false).mode, PermissionMode::Auto);
    }
    #[tokio::test]
    async fn permission_mode_ack_failure_stops_round_and_recovery_restores_applied() {
        let (mut host, mut state) = fixture(PermissionMode::Bypass);
        host.control
            .reject_ack
            .store(true, std::sync::atomic::Ordering::SeqCst);
        assert!(
            apply_round_permission_mode(&mut host, &mut state)
                .await
                .is_err()
        );
        assert!(host.observed.is_empty());
        assert!(state.applied_permission_mode.is_none());
        host.control
            .reject_ack
            .store(false, std::sync::atomic::Ordering::SeqCst);
        apply_round_permission_mode(&mut host, &mut state)
            .await
            .unwrap();
        state.applied_permission_mode = None;
        state.permission_context = None;
        state.current_run_owner_generation = Some(8);
        host.control.snapshot.lock().unwrap().requested = None;
        apply_round_permission_mode(&mut host, &mut state)
            .await
            .unwrap();
        assert_eq!(
            state
                .permission_context
                .as_ref()
                .unwrap()
                .read()
                .await
                .mode(),
            PermissionMode::Bypass
        );
        assert_eq!(
            host.control
                .acknowledgements
                .lock()
                .unwrap()
                .last()
                .unwrap()
                .0,
            8
        );
    }
    #[tokio::test]
    async fn permission_mode_root_request_does_not_replace_child_policy() {
        let (mut host, mut state) = fixture(PermissionMode::Bypass);
        state.recursion_depth = 1;
        state.permission_context = Some(crate::orchestration::PermissionSyncContext::shared_root(
            PermissionMode::Deny,
        ));
        apply_round_permission_mode(&mut host, &mut state)
            .await
            .unwrap();
        assert_eq!(
            state
                .permission_context
                .as_ref()
                .unwrap()
                .read()
                .await
                .mode(),
            PermissionMode::Deny
        );
        assert!(host.control.acknowledgements.lock().unwrap().is_empty());
    }
}
