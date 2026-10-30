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

/// Round-local observation of the existing durable permission lane. This is
/// only a read cache: application still requires the exact durable owner ACK.
/// The control watcher and committed same-pod requests share its wake source.
#[derive(Default)]
pub(crate) struct PermissionModeObservation {
    wake: Option<tokio::sync::watch::Receiver<i64>>,
    snapshot: Option<astra_turn_types::RunPermissionModeSnapshot>,
}

impl PermissionModeObservation {
    pub(crate) fn bind_wake(&mut self, wake: tokio::sync::watch::Receiver<i64>) {
        self.wake = Some(wake);
        self.snapshot = None;
    }

    /// The opening ACK proves empty permission history, not an applied mode.
    /// Initial restrictions still come from the admitted request; later changes
    /// require their existing durable application acknowledgement.
    pub(crate) fn bind_opening_wake(
        &mut self,
        mut wake: tokio::sync::watch::Receiver<i64>,
        tail: i64,
    ) {
        self.snapshot = Some(astra_turn_types::RunPermissionModeSnapshot::default());
        let observed = *wake.borrow_and_update();
        if observed > tail {
            wake.mark_changed();
        }
        self.wake = Some(wake);
    }

    async fn read(
        &mut self,
        control: &dyn crate::turn::run_control::RunControlProvider,
        user_id: &str,
        session_id: &str,
        run_id: &str,
    ) -> Result<Option<astra_turn_types::RunPermissionModeSnapshot>, String> {
        let refresh = self.snapshot.is_none()
            || self
                .wake
                .as_ref()
                .map(|wake| wake.has_changed().unwrap_or(true))
                .unwrap_or(true);
        if refresh {
            // Consume before I/O so a change committed during the read remains
            // pending for the next boundary. A failed read retains retry duty.
            if let Some(wake) = self.wake.as_mut() {
                let _ = wake.borrow_and_update();
            }
            match control
                .permission_mode_snapshot(user_id, session_id, run_id)
                .await
            {
                Ok(snapshot) => self.snapshot = snapshot,
                Err(error) => {
                    if let Some(wake) = self.wake.as_mut() {
                        wake.mark_changed();
                    }
                    return Err(error);
                }
            }
        }
        Ok(self.snapshot.clone())
    }
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
        state.owner_user_id.clone(),
        state.current_run_id.clone(),
    ) else {
        return Ok(());
    };
    let session_id = state
        .current_session_id
        .clone()
        .ok_or_else(|| boundary_error("run has no immutable session identity"))?;
    if state
        .durable_control_watcher
        .as_ref()
        .is_some_and(|watcher| watcher.has_changed().is_err())
    {
        state.permission_mode_observation.wake = None;
    }
    let Some(snapshot) = state
        .permission_mode_observation
        .read(control.as_ref(), &user_id, &session_id, &run_id)
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
        reads: std::sync::atomic::AtomicUsize,
        control_reads: std::sync::atomic::AtomicUsize,
        reject_read: std::sync::atomic::AtomicBool,
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
            self.control_reads
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            Ok(None)
        }
        async fn permission_mode_snapshot(
            &self,
            _: &str,
            _: &str,
            _: &str,
        ) -> Result<Option<RunPermissionModeSnapshot>, String> {
            self.reads
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            if self.reject_read.load(std::sync::atomic::Ordering::Relaxed) {
                return Err("permission read unavailable".into());
            }
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
            _invocation_identity: Option<
                crate::turn::run_control::UserIntentInvocationApplication<'_>,
            >,
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
        state.owner_user_id = Some("owner".into());
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
    async fn opening_ack_skips_only_empty_permission_history_and_keeps_late_requests() {
        use std::sync::atomic::Ordering;
        let control = Control::default();
        let (send, receive) = tokio::sync::watch::channel(2);
        let mut observation = PermissionModeObservation::default();
        observation.bind_opening_wake(receive, 2);
        assert_eq!(
            observation
                .read(&control, "owner", "session", "run")
                .await
                .unwrap(),
            Some(RunPermissionModeSnapshot::default())
        );
        assert_eq!(control.reads.load(Ordering::Relaxed), 0);
        control.request(PermissionMode::Prompt, 3);
        send.send_replace(3);
        let current = observation
            .read(&control, "owner", "session", "run")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(current.requested.unwrap().mode, PermissionMode::Prompt);
        assert!(
            current.applied.is_none(),
            "observing a request cannot apply its permission mode"
        );
        assert_eq!(control.reads.load(Ordering::Relaxed), 1);
        let mut attached_late = PermissionModeObservation::default();
        attached_late.bind_opening_wake(send.subscribe(), 2);
        assert!(
            attached_late
                .read(&control, "owner", "session", "run")
                .await
                .unwrap()
                .unwrap()
                .requested
                .is_some()
        );
        assert_eq!(control.reads.load(Ordering::Relaxed), 2);
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

    #[tokio::test]
    async fn quiet_rounds_reuse_permission_snapshot_but_apply_committed_changes() {
        use std::sync::atomic::Ordering;
        let (mut host, mut state) = fixture(PermissionMode::Bypass);
        let (wake, receive) = tokio::sync::watch::channel(1);
        state.permission_mode_observation.bind_wake(receive);
        for round in 0..6 {
            state.current_round_index = round;
            apply_round_permission_mode(&mut host, &mut state)
                .await
                .unwrap();
            assert_eq!(host.mode, PermissionMode::Bypass);
        }
        assert_eq!(host.control.reads.load(Ordering::Relaxed), 1);
        assert_eq!(host.control.acknowledgements.lock().unwrap().len(), 1);
        host.control.request(PermissionMode::Prompt, 2);
        wake.send_replace(2);
        state.current_round_index = 6;
        apply_round_permission_mode(&mut host, &mut state)
            .await
            .unwrap();
        assert_eq!(host.mode, PermissionMode::Prompt);
        assert_eq!(host.control.reads.load(Ordering::Relaxed), 2);
        assert_eq!(
            *host.control.acknowledgements.lock().unwrap(),
            [(7, 1, 0), (7, 2, 6)]
        );

        // A failed notification channel restores authoritative polling; it
        // must never turn a cached permissive choice into permanent authority.
        drop(wake);
        host.control.request(PermissionMode::Deny, 3);
        state.current_round_index = 7;
        apply_round_permission_mode(&mut host, &mut state)
            .await
            .unwrap();
        assert_eq!(host.mode, PermissionMode::Deny);
        assert_eq!(host.control.reads.load(Ordering::Relaxed), 3);
    }

    #[tokio::test]
    async fn failed_permission_refresh_retries_without_another_notification() {
        use std::sync::atomic::Ordering;
        let (mut host, mut state) = fixture(PermissionMode::Bypass);
        let (_wake, receive) = tokio::sync::watch::channel(1);
        state.permission_mode_observation.bind_wake(receive);
        host.control.reject_read.store(true, Ordering::Relaxed);
        assert!(
            apply_round_permission_mode(&mut host, &mut state)
                .await
                .is_err()
        );
        assert!(state.applied_permission_mode.is_none());
        host.control.reject_read.store(false, Ordering::Relaxed);
        apply_round_permission_mode(&mut host, &mut state)
            .await
            .unwrap();
        assert_eq!(host.control.reads.load(Ordering::Relaxed), 2);
        assert_eq!(
            state.applied_permission_mode.unwrap().mode,
            PermissionMode::Bypass
        );
    }

    #[tokio::test]
    async fn watched_round_preparation_uses_shared_control_flags() {
        use std::sync::atomic::Ordering;
        let (mut host, mut state) = fixture(PermissionMode::Prompt);
        let (watcher, observed) = tokio::sync::watch::channel(());
        state.durable_control_watcher = Some(observed);
        let prepared = super::super::lifecycle::prepare_turn_iteration(&mut host, &mut state, 0)
            .await
            .unwrap();
        assert!(!matches!(
            prepared,
            super::super::lifecycle::PreparedTurnIteration::Finished(
                super::super::host::AgenticLoopOutcome::Cancelled
            )
        ));
        assert_eq!(host.control.control_reads.load(Ordering::Relaxed), 0);
        assert_eq!(host.control.reads.load(Ordering::Relaxed), 1);
        drop(watcher);
        super::super::lifecycle::prepare_turn_iteration(&mut host, &mut state, 1)
            .await
            .unwrap();
        assert_eq!(
            host.control.control_reads.load(Ordering::Relaxed),
            1,
            "an exited watcher must restore the ordinary control reader"
        );
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
