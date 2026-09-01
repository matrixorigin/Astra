use std::sync::{Arc, Mutex};

use astra_core::sync_poison::recover_mutex_lock;
use astra_runtime::turn::run_control::{
    ActionAdmissionOutcome, ActionAdmissionRequest, QueuedUserIntent, RunControlStatus,
    RunStatusProvider, UserIntentAdmissionAuthority, UserIntentPoll, UserIntentProvider,
};
use astra_turn_types::{UserIntentDelivery, UserIntentStatus};
use serde_json::Value;

const MAX_USER_INTENT_CHARS: usize = 20_000;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct UserIntentReceipt {
    pub(crate) run_id: Option<String>,
    pub(crate) intent_id: String,
    pub(crate) delivery: UserIntentDelivery,
    pub(crate) status: UserIntentStatus,
    pub(crate) event_index: i64,
}

/// A reducer-completion latch for one server-owned guidance disposition.
///
/// The observer owns only this process-local latch and a weak turn reference,
/// so waiting for the foreground reducer cannot keep a finished turn alive.
pub(crate) struct RemoteDispositionProjectionAck {
    run_control: std::sync::Weak<LocalRunControl>,
    notify: Arc<tokio::sync::Notify>,
    intent_id: String,
}

impl RemoteDispositionProjectionAck {
    pub(crate) async fn wait(self) -> bool {
        loop {
            let notified = self.notify.notified();
            tokio::pin!(notified);
            // Register before observing the predicate. `notify_waiters` does
            // not retain a permit, so checking first has an unlock-to-poll
            // lost-wake window.
            notified.as_mut().enable();
            let Some(run_control) = self.run_control.upgrade() else {
                return false;
            };
            if !run_control.remote_disposition_is_pending(&self.intent_id) {
                return true;
            }
            drop(run_control);
            notified.await;
        }
    }
}

#[derive(Default)]
struct LocalRunControlState {
    next_event_index: usize,
    intents: Vec<QueuedUserIntent>,
    admitted_action_ids: std::collections::HashSet<String>,
    /// Runtime facts that reached a model boundary but are not safe to forget
    /// until the enclosing turn settles successfully.
    applied_runtime_notifications: Vec<String>,
    /// Server-owned dispositions observed on the independent durable control
    /// tail. This bridges a closed primary SSE fanout into final local turn
    /// settlement without treating the guidance as a second model input.
    remotely_applied_user_intents:
        Vec<crate::cli::stream::streaming_types::AppliedStreamUserIntent>,
    /// Reducer-completed identities live for the whole turn-control lifetime.
    /// The applied payload queue is drained into settlement, so it cannot also
    /// serve as the ordering fence for a later acceptance acknowledgement.
    remotely_resolved_user_intent_ids: std::collections::HashSet<String>,
    /// Locally-owned submissions whose stable identity has been created but
    /// whose durable server acknowledgement is still in flight. Turn
    /// settlement must not race this boundary: an accepted submission is
    /// atomically promoted into `pending_remote_dispositions`, while a
    /// rejected/unconfirmed one is explicitly released.
    pending_remote_submissions: std::collections::HashSet<String>,
    /// Durable cursor at which each accepted remote intent was committed.
    /// Keeping the cursor with the intent is essential for recovery: if the
    /// shared observer exits abnormally, a later intent must restart from the
    /// oldest unresolved boundary rather than skip an earlier disposition.
    pending_remote_dispositions: std::collections::HashMap<String, i64>,
    remote_disposition_observer_running: bool,
    status: Option<RunControlStatus>,
    cancellation_origin: Option<astra_turn_core::orchestration_types::CancellationOrigin>,
}

/// In-process run-control provider for the CLI/TUI agentic loop.
///
/// Server-backed runs use the durable run engine for this contract. CLI local
/// runs use this turn-scoped provider so the same runtime polling paths can
/// observe user cancellation and active-run guidance without requiring a server-side
/// workspace executor.
#[derive(Default)]
pub(crate) struct LocalRunControl {
    // This lock is only held for short in-memory queue mutations and never
    // across an `.await`, so a std::sync::Mutex keeps the local TUI hot path
    // simple without introducing async lock wakeups.
    state: Mutex<LocalRunControlState>,
    remote_disposition_notify: Arc<tokio::sync::Notify>,
}

impl LocalRunControl {
    pub(crate) fn shared() -> Arc<Self> {
        Arc::new(Self::default())
    }

    fn request_cancel(&self, origin: astra_turn_core::orchestration_types::CancellationOrigin) {
        let mut guard = recover_mutex_lock(&self.state);
        if guard.status != Some(RunControlStatus::Cancelled) {
            guard.status = Some(RunControlStatus::Cancelled);
            guard.cancellation_origin = Some(origin);
        }
    }

    pub(crate) fn request_cancel_for_user(&self) {
        self.request_cancel(astra_turn_core::orchestration_types::CancellationOrigin::User);
    }

    pub(crate) fn request_cancel_for_runtime(&self) {
        self.request_cancel(astra_turn_core::orchestration_types::CancellationOrigin::Runtime);
    }

    pub(crate) fn request_pause(&self) {
        let mut guard = recover_mutex_lock(&self.state);
        if guard.status != Some(RunControlStatus::Cancelled) {
            guard.status = Some(RunControlStatus::Paused);
        }
    }

    pub(crate) fn resume(&self) {
        let mut guard = recover_mutex_lock(&self.state);
        if guard.status == Some(RunControlStatus::Paused) {
            guard.status = None;
        }
    }

    /// Build the typed active-run guidance payload shared by local UI context
    /// capture and the durable server intent endpoint. This does not enqueue or
    /// acknowledge anything; only the server response can produce a receipt.
    pub(crate) fn guidance_input(
        text: &str,
        background_work_snapshot: Option<&str>,
        work_unit_observations: &[astra_core::work_unit::WorkUnitObservation],
    ) -> Result<Value, String> {
        if text.trim().is_empty() {
            return Err("Guidance cannot be empty.".to_string());
        }
        if text.chars().count() > MAX_USER_INTENT_CHARS {
            return Err(format!(
                "Guidance is too large. Limit it to {MAX_USER_INTENT_CHARS} characters."
            ));
        }
        let mut input = serde_json::json!({ "content": text });
        let background_work_snapshot = background_work_snapshot
            .map(str::trim)
            .filter(|snapshot| !snapshot.is_empty());
        if background_work_snapshot.is_some() || !work_unit_observations.is_empty() {
            input
                .as_object_mut()
                .expect("guidance input is an object")
                .insert(
                    "astra_runtime_context".to_string(),
                    serde_json::json!({
                        "schema": "active_work_snapshot.v1",
                        "authority": "run_control_provider",
                        "background_work_snapshot": background_work_snapshot.unwrap_or(""),
                        "work_unit_observations": work_unit_observations,
                    }),
                );
        }
        Ok(input)
    }

    pub(crate) fn accept_runtime_notification(&self, content: &str) -> Result<(), String> {
        if content.trim().is_empty() {
            return Err("Runtime notification cannot be empty.".to_string());
        }
        if content.chars().count() > MAX_USER_INTENT_CHARS {
            return Err(format!(
                "Runtime notification is too large. Limit it to {MAX_USER_INTENT_CHARS} characters."
            ));
        }
        self.accept_intent(
            UserIntentDelivery::GuideCurrentRun,
            astra_runtime::turn::run_control::runtime_notification_input(content),
        );
        Ok(())
    }

    /// Recover runtime facts that never reached a model boundary before the
    /// active turn settled. Applied items have already been evicted by the
    /// provider acknowledgement path, so this returns only genuinely pending
    /// notifications and prevents a completion from disappearing in the
    /// active→idle handoff.
    pub(crate) fn take_pending_runtime_notifications(&self) -> Vec<String> {
        let mut guard = recover_mutex_lock(&self.state);
        let mut pending = std::mem::take(&mut guard.applied_runtime_notifications);
        guard.intents.retain(|event| {
            if let Some(content) =
                astra_runtime::turn::run_control::runtime_notification_content(&event.input)
            {
                pending.push(content);
                false
            } else {
                true
            }
        });
        pending
    }

    /// Commit runtime facts only after the enclosing turn has produced and
    /// settled a successful answer. Guidance acknowledgements still mean
    /// "applied at a model boundary"; this extra local checkpoint prevents a
    /// later model/persistence failure from losing the background fact.
    pub(crate) fn commit_applied_runtime_notifications(&self) {
        recover_mutex_lock(&self.state)
            .applied_runtime_notifications
            .clear();
    }

    pub(crate) fn record_remotely_applied_user_intent(
        &self,
        intent: crate::cli::stream::streaming_types::AppliedStreamUserIntent,
    ) {
        let intent_id = intent.intent_id.clone();
        let mut guard = recover_mutex_lock(&self.state);
        if !guard
            .remotely_applied_user_intents
            .iter()
            .any(|existing| existing.intent_id == intent.intent_id)
        {
            guard.remotely_applied_user_intents.push(intent);
        }
        guard
            .remotely_resolved_user_intent_ids
            .insert(intent_id.clone());
        guard.pending_remote_submissions.remove(&intent_id);
        guard.pending_remote_dispositions.remove(&intent_id);
        drop(guard);
        self.remote_disposition_notify.notify_waiters();
    }

    pub(crate) fn expect_remote_user_intent_disposition(&self, intent_id: &str, event_index: i64) {
        let mut guard = recover_mutex_lock(&self.state);
        guard.pending_remote_submissions.remove(intent_id);
        if guard.remotely_resolved_user_intent_ids.contains(intent_id) {
            drop(guard);
            self.remote_disposition_notify.notify_waiters();
            return;
        }
        guard
            .pending_remote_dispositions
            .entry(intent_id.to_string())
            .and_modify(|cursor| *cursor = (*cursor).min(event_index))
            .or_insert(event_index);
    }

    pub(crate) fn expect_remote_user_intent_submission(&self, intent_id: &str) {
        recover_mutex_lock(&self.state)
            .pending_remote_submissions
            .insert(intent_id.to_string());
    }

    pub(crate) fn release_remote_user_intent_submission(&self, intent_id: &str) {
        recover_mutex_lock(&self.state)
            .pending_remote_submissions
            .remove(intent_id);
        self.remote_disposition_notify.notify_waiters();
    }

    pub(crate) fn pending_remote_submission_ids(&self) -> Vec<String> {
        recover_mutex_lock(&self.state)
            .pending_remote_submissions
            .iter()
            .cloned()
            .collect()
    }

    pub(crate) fn abandon_remote_user_intent_disposition(&self, intent_id: &str) {
        recover_mutex_lock(&self.state)
            .pending_remote_dispositions
            .remove(intent_id);
        self.remote_disposition_notify.notify_waiters();
    }

    pub(crate) fn record_remotely_returned_user_intent(&self, intent_id: &str) {
        let mut guard = recover_mutex_lock(&self.state);
        guard
            .remotely_resolved_user_intent_ids
            .insert(intent_id.to_string());
        guard.pending_remote_submissions.remove(intent_id);
        guard.pending_remote_dispositions.remove(intent_id);
        drop(guard);
        self.remote_disposition_notify.notify_waiters();
    }

    pub(crate) fn pending_remote_disposition_ids(&self) -> Vec<String> {
        recover_mutex_lock(&self.state)
            .pending_remote_dispositions
            .keys()
            .cloned()
            .collect()
    }

    fn remote_disposition_is_pending(&self, intent_id: &str) -> bool {
        recover_mutex_lock(&self.state)
            .pending_remote_dispositions
            .contains_key(intent_id)
    }

    pub(crate) fn remote_disposition_projection_ack(
        self: &Arc<Self>,
        intent_id: &str,
    ) -> RemoteDispositionProjectionAck {
        RemoteDispositionProjectionAck {
            run_control: Arc::downgrade(self),
            notify: self.remote_disposition_notify.clone(),
            intent_id: intent_id.to_string(),
        }
    }

    /// Claim the single durable control-tail observer for this run. Multiple
    /// accepted intents share it; they must not create one long-lived SSE
    /// connection per user message.
    pub(crate) fn claim_remote_disposition_observer(&self) -> Option<i64> {
        let mut guard = recover_mutex_lock(&self.state);
        if guard.remote_disposition_observer_running {
            return None;
        }
        let oldest_cursor = guard.pending_remote_dispositions.values().copied().min()?;
        guard.remote_disposition_observer_running = true;
        Some(oldest_cursor)
    }

    pub(crate) fn release_remote_disposition_observer(&self) {
        recover_mutex_lock(&self.state).remote_disposition_observer_running = false;
    }

    pub(crate) fn release_remote_disposition_observer_if_idle(&self) -> bool {
        let mut guard = recover_mutex_lock(&self.state);
        if !guard.pending_remote_dispositions.is_empty() {
            return false;
        }
        guard.remote_disposition_observer_running = false;
        true
    }

    pub(crate) async fn wait_for_remote_user_intent_dispositions(
        &self,
        deadline: std::time::Duration,
    ) -> bool {
        let settled = self
            .wait_for_remote_user_intent_dispositions_with_hook(deadline, || async {})
            .await;
        settled
    }

    async fn wait_for_remote_user_intent_dispositions_with_hook<F, Fut>(
        &self,
        deadline: std::time::Duration,
        mut after_pending_check: F,
    ) -> bool
    where
        F: FnMut() -> Fut,
        Fut: std::future::Future<Output = ()>,
    {
        tokio::time::timeout(deadline, async {
            loop {
                let notified = self.remote_disposition_notify.notified();
                tokio::pin!(notified);
                notified.as_mut().enable();
                let closure_is_clear = {
                    let guard = recover_mutex_lock(&self.state);
                    guard.pending_remote_submissions.is_empty()
                        && guard.pending_remote_dispositions.is_empty()
                };
                if closure_is_clear {
                    break;
                }
                after_pending_check().await;
                notified.await;
            }
        })
        .await
        .is_ok()
    }

    pub(crate) fn take_remotely_applied_user_intents(
        &self,
    ) -> Vec<crate::cli::stream::streaming_types::AppliedStreamUserIntent> {
        std::mem::take(&mut recover_mutex_lock(&self.state).remotely_applied_user_intents)
    }

    fn accept_intent(&self, delivery: UserIntentDelivery, input: Value) -> UserIntentReceipt {
        let mut guard = recover_mutex_lock(&self.state);
        guard.next_event_index += 1;
        let event_index = guard.next_event_index;
        let intent_id = format!("intent_{}", uuid::Uuid::now_v7().simple());
        guard.intents.push(QueuedUserIntent {
            intent_id: intent_id.clone(),
            delivery,
            status: UserIntentStatus::AcceptedLocal,
            event_index,
            input,
        });
        UserIntentReceipt {
            run_id: None,
            intent_id,
            delivery,
            status: UserIntentStatus::AcceptedLocal,
            event_index: event_index as i64,
        }
    }
}

#[async_trait::async_trait]
impl RunStatusProvider for LocalRunControl {
    async fn control_status(
        &self,
        _user_id: &str,
        _run_id: &str,
    ) -> Result<Option<RunControlStatus>, String> {
        Ok(recover_mutex_lock(&self.state).status)
    }

    async fn cancellation_origin(
        &self,
        _user_id: &str,
        _run_id: &str,
    ) -> Result<astra_turn_core::orchestration_types::CancellationOrigin, String> {
        Ok(recover_mutex_lock(&self.state)
            .cancellation_origin
            .unwrap_or(astra_turn_core::orchestration_types::CancellationOrigin::Runtime))
    }
}

#[async_trait::async_trait]
impl UserIntentProvider for LocalRunControl {
    fn has_pending_inputs(&self) -> bool {
        !recover_mutex_lock(&self.state).intents.is_empty()
    }

    async fn fence_user_intent_submissions(
        &self,
        _user_id: &str,
        _expected_session_id: &str,
        _run_id: &str,
        authority: UserIntentAdmissionAuthority,
    ) -> Result<(), String> {
        match authority {
            UserIntentAdmissionAuthority::ProcessLocal => Ok(()),
            UserIntentAdmissionAuthority::DurableOwnerGeneration(_) => Err(
                "local user-intent admission cannot validate durable owner authority".to_string(),
            ),
        }
    }

    async fn reopen_user_intent_submissions(
        &self,
        _user_id: &str,
        _expected_session_id: &str,
        _run_id: &str,
        authority: UserIntentAdmissionAuthority,
    ) -> Result<(), String> {
        match authority {
            UserIntentAdmissionAuthority::ProcessLocal => Ok(()),
            UserIntentAdmissionAuthority::DurableOwnerGeneration(_) => Err(
                "local user-intent admission cannot validate durable owner authority".to_string(),
            ),
        }
    }

    async fn begin_action(
        &self,
        _user_id: &str,
        _run_id: &str,
        request: ActionAdmissionRequest,
    ) -> Result<ActionAdmissionOutcome, String> {
        if request.action_id.trim().is_empty() {
            return Err("local action admission requires a non-empty action id".to_string());
        }
        if request.expected_owner_generation.is_some() {
            return Err(
                "local action admission cannot validate a durable owner generation".to_string(),
            );
        }
        let mut guard = recover_mutex_lock(&self.state);
        if let Some(status) = guard.status {
            return Ok(ActionAdmissionOutcome::Inactive {
                status: match status {
                    RunControlStatus::Cancelled => "cancelled",
                    RunControlStatus::Paused => "paused",
                }
                .to_string(),
            });
        }
        if let Some(intent) = guard.intents.iter().find(|intent| {
            i64::try_from(intent.event_index).unwrap_or(i64::MAX) > request.expected_control_epoch
        }) {
            return Ok(ActionAdmissionOutcome::Superseded {
                user_intent_event_index: i64::try_from(intent.event_index).unwrap_or(i64::MAX),
            });
        }
        if guard.admitted_action_ids.contains(&request.action_id) {
            return Ok(ActionAdmissionOutcome::AlreadyStarted {
                event_index: i64::try_from(guard.next_event_index).unwrap_or(i64::MAX),
            });
        }
        guard.next_event_index = guard.next_event_index.saturating_add(1);
        let event_index = i64::try_from(guard.next_event_index).unwrap_or(i64::MAX);
        guard.admitted_action_ids.insert(request.action_id);
        Ok(ActionAdmissionOutcome::Started { event_index })
    }

    async fn poll_user_intents(
        &self,
        _user_id: &str,
        _run_id: &str,
        after_event_index: usize,
    ) -> UserIntentPoll {
        let guard = recover_mutex_lock(&self.state);
        let inputs = guard
            .intents
            .iter()
            .filter(|event| event.event_index > after_event_index)
            .cloned()
            .collect::<Vec<_>>();
        UserIntentPoll {
            next_cursor: guard.next_event_index.max(after_event_index),
            snapshot_has_more: false,
            snapshot_page_fact_count: inputs.len(),
            inputs,
            issues: Vec::new(),
            error: None,
        }
    }

    async fn mark_user_intents_applied(
        &self,
        _user_id: &str,
        _expected_session_id: &str,
        _run_id: &str,
        event_indices: &[usize],
        authority: astra_runtime::turn::run_control::UserIntentAdmissionAuthority,
    ) -> Result<astra_runtime::turn::run_control::UserIntentApplyAck, String> {
        if authority != astra_runtime::turn::run_control::UserIntentAdmissionAuthority::ProcessLocal
        {
            return Err(
                "process-local user-intent apply rejects durable owner authority".to_string(),
            );
        }
        if event_indices.is_empty() {
            return Ok(astra_runtime::turn::run_control::UserIntentApplyAck::Applied);
        }
        let released = event_indices
            .iter()
            .copied()
            .collect::<std::collections::HashSet<_>>();
        let mut guard = recover_mutex_lock(&self.state);
        let mut applied_runtime_notifications = Vec::new();
        guard.intents.retain(|event| {
            if !released.contains(&event.event_index) {
                return true;
            }
            if let Some(content) =
                astra_runtime::turn::run_control::runtime_notification_content(&event.input)
            {
                applied_runtime_notifications.push(content);
            }
            false
        });
        guard
            .applied_runtime_notifications
            .extend(applied_runtime_notifications);
        Ok(astra_runtime::turn::run_control::UserIntentApplyAck::Applied)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn local_run_control_polls_runtime_notifications_after_cursor() {
        let provider = LocalRunControl::default();
        provider
            .accept_runtime_notification("first")
            .expect("enqueue first");
        provider
            .accept_runtime_notification("second")
            .expect("enqueue second");

        let first = provider
            .poll_user_intents("local-user", "run-local", 0)
            .await;
        assert_eq!(first.next_cursor, 2);
        assert_eq!(first.inputs.len(), 2);
        assert_ne!(first.inputs[0].intent_id, first.inputs[1].intent_id);
        assert_eq!(
            astra_runtime::turn::run_control::runtime_notification_content(&first.inputs[0].input),
            Some("first".to_string())
        );

        let second = provider
            .poll_user_intents("local-user", "run-local", 1)
            .await;
        assert_eq!(second.next_cursor, 2);
        assert_eq!(second.inputs.len(), 1);
        assert_eq!(
            astra_runtime::turn::run_control::runtime_notification_content(&second.inputs[0].input),
            Some("second".to_string())
        );
    }

    #[tokio::test]
    async fn local_run_control_reports_cancel_status_through_shared_contract() {
        let provider = LocalRunControl::default();
        assert_eq!(
            provider
                .control_status("local-user", "run-local")
                .await
                .expect("status poll should succeed"),
            None
        );
        assert_eq!(
            provider
                .cancellation_origin("local-user", "run-local")
                .await
                .expect("origin poll should succeed"),
            astra_turn_core::orchestration_types::CancellationOrigin::Runtime,
            "the presence of a local provider is not proof of a user request"
        );

        provider.request_cancel_for_user();

        assert_eq!(
            provider
                .control_status("local-user", "run-local")
                .await
                .expect("status poll should succeed"),
            Some(RunControlStatus::Cancelled)
        );
        assert_eq!(
            provider
                .cancellation_origin("local-user", "run-local")
                .await
                .expect("origin poll should succeed"),
            astra_turn_core::orchestration_types::CancellationOrigin::User
        );
    }

    #[tokio::test]
    async fn local_runtime_cleanup_never_fabricates_user_cancellation_origin() {
        let provider = LocalRunControl::default();
        provider.request_cancel_for_runtime();

        assert_eq!(
            provider
                .control_status("local-user", "run-local")
                .await
                .unwrap(),
            Some(RunControlStatus::Cancelled)
        );
        assert_eq!(
            provider
                .cancellation_origin("local-user", "run-local")
                .await
                .unwrap(),
            astra_turn_core::orchestration_types::CancellationOrigin::Runtime
        );

        provider.request_cancel_for_user();
        assert_eq!(
            provider
                .cancellation_origin("local-user", "run-local")
                .await
                .unwrap(),
            astra_turn_core::orchestration_types::CancellationOrigin::Runtime,
            "the first cancellation linearization point owns the typed origin"
        );
    }

    #[tokio::test]
    async fn local_action_admission_linearizes_guidance_and_never_replays_started_action() {
        let provider = LocalRunControl::default();
        let started = provider
            .begin_action(
                "local-user",
                "run-local",
                ActionAdmissionRequest {
                    action_id: "round:0:serial:first".to_string(),
                    expected_session_id: "local-session".to_string(),
                    expected_control_epoch: 0,
                    expected_owner_generation: None,
                },
            )
            .await
            .expect("first action admission");
        assert!(matches!(started, ActionAdmissionOutcome::Started { .. }));

        let retry = provider
            .begin_action(
                "local-user",
                "run-local",
                ActionAdmissionRequest {
                    action_id: "round:0:serial:first".to_string(),
                    expected_session_id: "local-session".to_string(),
                    expected_control_epoch: 0,
                    expected_owner_generation: None,
                },
            )
            .await
            .expect("idempotent lookup");
        assert!(matches!(
            retry,
            ActionAdmissionOutcome::AlreadyStarted { .. }
        ));
        assert!(!retry.is_fresh_grant());

        provider
            .accept_runtime_notification("replace stale work")
            .expect("guidance accepted");
        let superseded = provider
            .begin_action(
                "local-user",
                "run-local",
                ActionAdmissionRequest {
                    action_id: "round:0:serial:second".to_string(),
                    expected_session_id: "local-session".to_string(),
                    expected_control_epoch: 0,
                    expected_owner_generation: None,
                },
            )
            .await
            .expect("second action admission");
        assert!(matches!(
            superseded,
            ActionAdmissionOutcome::Superseded { .. }
        ));
    }

    #[tokio::test]
    async fn local_run_control_pause_can_resume_but_not_override_cancel() {
        let provider = LocalRunControl::default();
        provider.request_pause();
        assert_eq!(
            provider
                .control_status("local-user", "run-local")
                .await
                .expect("status poll should succeed"),
            Some(RunControlStatus::Paused)
        );

        provider.resume();
        assert_eq!(
            provider
                .control_status("local-user", "run-local")
                .await
                .expect("status poll should succeed"),
            None
        );

        provider.request_cancel_for_user();
        provider.request_pause();
        provider.resume();
        assert_eq!(
            provider
                .control_status("local-user", "run-local")
                .await
                .expect("status poll should succeed"),
            Some(RunControlStatus::Cancelled),
            "cancelled is terminal for the turn-scoped local provider"
        );
    }

    #[test]
    fn local_run_control_rejects_blank_runtime_notification() {
        let provider = LocalRunControl::default();
        let error = provider
            .accept_runtime_notification("   ")
            .expect_err("blank runtime notification should be rejected");
        assert!(error.contains("cannot be empty"));
    }

    #[test]
    fn local_run_control_rejects_oversized_runtime_notification() {
        let provider = LocalRunControl::default();
        let text = "x".repeat(MAX_USER_INTENT_CHARS + 1);
        let error = provider
            .accept_runtime_notification(&text)
            .expect_err("oversized runtime notification should be rejected");
        assert!(error.contains("too large"));
    }

    #[tokio::test]
    async fn local_run_control_evicts_released_runtime_notifications() {
        let provider = LocalRunControl::default();
        provider
            .accept_runtime_notification("first")
            .expect("enqueue first");
        provider
            .accept_runtime_notification("second")
            .expect("enqueue second");

        provider
            .mark_user_intents_applied(
                "local-user",
                "local-session",
                "run-local",
                &[1],
                astra_runtime::turn::run_control::UserIntentAdmissionAuthority::ProcessLocal,
            )
            .await
            .expect("release should succeed");

        let remaining = provider
            .poll_user_intents("local-user", "run-local", 0)
            .await;
        assert_eq!(remaining.next_cursor, 2);
        assert_eq!(remaining.inputs.len(), 1);
        assert_eq!(remaining.inputs[0].event_index, 2);
    }

    #[tokio::test]
    async fn local_run_control_rejects_durable_apply_authority() {
        let provider = LocalRunControl::default();
        provider
            .accept_runtime_notification("local guidance")
            .unwrap();

        let error = provider
            .mark_user_intents_applied(
                "local-user",
                "local-session",
                "run-local",
                &[1],
                astra_runtime::turn::run_control::UserIntentAdmissionAuthority::DurableOwnerGeneration(7),
            )
            .await
            .expect_err("process-local provider must reject durable authority");

        assert!(error.contains("process-local"));
        assert_eq!(
            provider
                .poll_user_intents("local-user", "run-local", 0)
                .await
                .inputs
                .len(),
            1
        );
    }

    #[tokio::test]
    async fn remote_applied_guidance_survives_closed_primary_fanout_until_settlement() {
        let provider = LocalRunControl::default();
        let intent = crate::cli::stream::streaming_types::AppliedStreamUserIntent {
            intent_id: "intent-durable".into(),
            delivery: UserIntentDelivery::GuideCurrentRun,
            status: UserIntentStatus::Applied,
            event_index: 19,
            content: "wait".into(),
        };

        provider.expect_remote_user_intent_disposition(
            &intent.intent_id,
            i64::try_from(intent.event_index).expect("test cursor fits i64"),
        );
        assert!(
            !provider
                .wait_for_remote_user_intent_dispositions(std::time::Duration::ZERO)
                .await
        );
        provider.record_remotely_applied_user_intent(intent.clone());
        provider.record_remotely_applied_user_intent(intent.clone());

        assert!(
            provider
                .wait_for_remote_user_intent_dispositions(std::time::Duration::from_millis(10))
                .await
        );
        assert_eq!(provider.take_remotely_applied_user_intents(), vec![intent]);
        assert!(provider.take_remotely_applied_user_intents().is_empty());
    }

    #[tokio::test]
    async fn guidance_submission_is_a_settlement_barrier_until_promoted_or_released() {
        let provider = LocalRunControl::default();
        provider.expect_remote_user_intent_submission("intent-submit");
        assert!(
            !provider
                .wait_for_remote_user_intent_dispositions(std::time::Duration::ZERO)
                .await,
            "a half-open submission must keep the turn owner alive"
        );

        provider.expect_remote_user_intent_disposition("intent-submit", 12);
        assert!(
            !provider
                .wait_for_remote_user_intent_dispositions(std::time::Duration::ZERO)
                .await,
            "durable acceptance must atomically replace, not release, the barrier"
        );
        provider.record_remotely_returned_user_intent("intent-submit");
        assert!(
            provider
                .wait_for_remote_user_intent_dispositions(std::time::Duration::from_millis(10))
                .await
        );

        provider.expect_remote_user_intent_submission("intent-rejected");
        provider.release_remote_user_intent_submission("intent-rejected");
        assert!(
            provider
                .wait_for_remote_user_intent_dispositions(std::time::Duration::from_millis(10))
                .await
        );
    }

    #[tokio::test]
    async fn disposition_wait_registers_before_the_unlock_to_first_poll_race() {
        let provider = Arc::new(LocalRunControl::default());
        provider.expect_remote_user_intent_disposition("intent-race", 22);
        let entered_race_window = Arc::new(tokio::sync::Notify::new());
        let release_race_window = Arc::new(tokio::sync::Notify::new());
        let waiter_provider = provider.clone();
        let waiter_entered = entered_race_window.clone();
        let waiter_release = release_race_window.clone();
        let waiter = tokio::spawn(async move {
            waiter_provider
                .wait_for_remote_user_intent_dispositions_with_hook(
                    std::time::Duration::from_secs(1),
                    move || {
                        let entered = waiter_entered.clone();
                        let release = waiter_release.clone();
                        async move {
                            entered.notify_one();
                            release.notified().await;
                        }
                    },
                )
                .await
        });

        entered_race_window.notified().await;
        provider.record_remotely_returned_user_intent("intent-race");
        release_race_window.notify_one();

        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(100), waiter)
                .await
                .expect("pre-registered notification must close the race without the 5s timeout")
                .expect("waiter task")
        );
    }

    #[tokio::test]
    async fn reducer_resolution_before_acceptance_does_not_reopen_the_barrier() {
        let provider = LocalRunControl::default();
        provider.expect_remote_user_intent_submission("intent-applied-first");
        provider.record_remotely_applied_user_intent(
            crate::cli::stream::streaming_types::AppliedStreamUserIntent {
                intent_id: "intent-applied-first".into(),
                delivery: UserIntentDelivery::GuideCurrentRun,
                status: UserIntentStatus::Applied,
                event_index: 31,
                content: "new priority".into(),
            },
        );
        assert_eq!(provider.take_remotely_applied_user_intents().len(), 1);
        provider.expect_remote_user_intent_disposition("intent-applied-first", 30);
        assert!(provider.pending_remote_disposition_ids().is_empty());
        assert!(
            provider
                .wait_for_remote_user_intent_dispositions(std::time::Duration::ZERO)
                .await
        );

        provider.expect_remote_user_intent_submission("intent-returned-first");
        provider.record_remotely_returned_user_intent("intent-returned-first");
        provider.expect_remote_user_intent_disposition("intent-returned-first", 32);
        assert!(provider.pending_remote_disposition_ids().is_empty());
        assert!(
            provider
                .wait_for_remote_user_intent_dispositions(std::time::Duration::ZERO)
                .await
        );
    }

    #[test]
    fn remote_guidance_uses_one_observer_until_every_pending_intent_settles() {
        let provider = LocalRunControl::default();
        provider.expect_remote_user_intent_disposition("intent-a", 10);
        assert_eq!(provider.claim_remote_disposition_observer(), Some(10));
        provider.expect_remote_user_intent_disposition("intent-b", 20);
        assert_eq!(provider.claim_remote_disposition_observer(), None);

        provider.record_remotely_returned_user_intent("intent-a");
        assert!(
            !provider.release_remote_disposition_observer_if_idle(),
            "the shared observer must remain owned while a later intent is pending"
        );
        provider.record_remotely_returned_user_intent("intent-b");
        assert!(provider.release_remote_disposition_observer_if_idle());
        assert_eq!(provider.claim_remote_disposition_observer(), None);
    }

    #[test]
    fn replacement_observer_resumes_from_oldest_unsettled_intent() {
        let provider = LocalRunControl::default();
        provider.expect_remote_user_intent_disposition("intent-old", 10);
        assert_eq!(provider.claim_remote_disposition_observer(), Some(10));

        // Authentication loss, transport exhaustion, or task cancellation
        // releases the observer but must not release its durable cursor.
        provider.release_remote_disposition_observer();
        provider.expect_remote_user_intent_disposition("intent-new", 30);

        assert_eq!(provider.claim_remote_disposition_observer(), Some(10));
        provider.release_remote_disposition_observer();
    }
}
