use std::sync::{Arc, Mutex};

use astra_core::sync_poison::recover_mutex_lock;
use astra_runtime::turn::run_control::{
    QueuedUserIntent, RunControlStatus, RunStatusProvider, UserIntentPoll, UserIntentProvider,
};
use astra_turn_types::{UserIntentDelivery, UserIntentStatus};
use serde_json::Value;

const MAX_USER_INTENT_CHARS: usize = 20_000;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct LocalUserIntentReceipt {
    pub(crate) intent_id: String,
    pub(crate) delivery: UserIntentDelivery,
    pub(crate) status: UserIntentStatus,
}

#[derive(Default)]
struct LocalRunControlState {
    next_event_index: usize,
    intents: Vec<QueuedUserIntent>,
    status: Option<RunControlStatus>,
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
}

impl LocalRunControl {
    pub(crate) fn shared() -> Arc<Self> {
        Arc::new(Self::default())
    }

    pub(crate) fn request_cancel(&self) {
        let mut guard = recover_mutex_lock(&self.state);
        guard.status = Some(RunControlStatus::Cancelled);
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

    pub(crate) fn accept_guidance(&self, text: &str) -> Result<LocalUserIntentReceipt, String> {
        if text.trim().is_empty() {
            return Err("Guidance cannot be empty.".to_string());
        }
        if text.chars().count() > MAX_USER_INTENT_CHARS {
            return Err(format!(
                "Guidance is too large. Limit it to {MAX_USER_INTENT_CHARS} characters."
            ));
        }
        Ok(self.accept_intent(
            UserIntentDelivery::GuideCurrentRun,
            serde_json::json!({ "content": text }),
        ))
    }

    fn accept_intent(&self, delivery: UserIntentDelivery, input: Value) -> LocalUserIntentReceipt {
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
        LocalUserIntentReceipt {
            intent_id,
            delivery,
            status: UserIntentStatus::AcceptedLocal,
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
}

#[async_trait::async_trait]
impl UserIntentProvider for LocalRunControl {
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
            inputs,
            issues: Vec::new(),
            error: None,
        }
    }

    async fn mark_user_intents_applied(
        &self,
        _user_id: &str,
        _run_id: &str,
        event_indices: &[usize],
    ) -> Result<astra_runtime::turn::run_control::UserIntentApplyAck, String> {
        if event_indices.is_empty() {
            return Ok(astra_runtime::turn::run_control::UserIntentApplyAck::Applied);
        }
        let released = event_indices
            .iter()
            .copied()
            .collect::<std::collections::HashSet<_>>();
        let mut guard = recover_mutex_lock(&self.state);
        guard
            .intents
            .retain(|event| !released.contains(&event.event_index));
        Ok(astra_runtime::turn::run_control::UserIntentApplyAck::Applied)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn local_run_control_polls_inputs_after_cursor() {
        let provider = LocalRunControl::default();
        let first_receipt = provider.accept_guidance("first").expect("enqueue first");
        let second_receipt = provider.accept_guidance("second").expect("enqueue second");

        let first = provider
            .poll_user_intents("local-user", "run-local", 0)
            .await;
        assert_eq!(first.next_cursor, 2);
        assert_eq!(first.inputs.len(), 2);
        assert_eq!(first.inputs[0].intent_id, first_receipt.intent_id);
        assert_eq!(first.inputs[1].intent_id, second_receipt.intent_id);
        assert_ne!(first.inputs[0].intent_id, first.inputs[1].intent_id);
        assert_eq!(first.inputs[0].input["content"], "first");
        assert_eq!(first.inputs[1].input["content"], "second");

        let second = provider
            .poll_user_intents("local-user", "run-local", 1)
            .await;
        assert_eq!(second.next_cursor, 2);
        assert_eq!(second.inputs.len(), 1);
        assert_eq!(second.inputs[0].input["content"], "second");
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

        provider.request_cancel();

        assert_eq!(
            provider
                .control_status("local-user", "run-local")
                .await
                .expect("status poll should succeed"),
            Some(RunControlStatus::Cancelled)
        );
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

        provider.request_cancel();
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
    fn local_run_control_rejects_blank_input() {
        let provider = LocalRunControl::default();
        let error = provider
            .accept_guidance("   ")
            .expect_err("blank user intent should be rejected");
        assert!(error.contains("cannot be empty"));
    }

    #[test]
    fn local_run_control_rejects_oversized_input() {
        let provider = LocalRunControl::default();
        let text = "x".repeat(MAX_USER_INTENT_CHARS + 1);
        let error = provider
            .accept_guidance(&text)
            .expect_err("oversized user intent should be rejected");
        assert!(error.contains("too large"));
    }

    #[tokio::test]
    async fn local_run_control_evicts_released_inputs() {
        let provider = LocalRunControl::default();
        provider.accept_guidance("first").expect("enqueue first");
        provider.accept_guidance("second").expect("enqueue second");

        provider
            .mark_user_intents_applied("local-user", "run-local", &[1])
            .await
            .expect("release should succeed");

        let remaining = provider
            .poll_user_intents("local-user", "run-local", 0)
            .await;
        assert_eq!(remaining.next_cursor, 2);
        assert_eq!(remaining.inputs.len(), 1);
        assert_eq!(remaining.inputs[0].event_index, 2);
    }
}
