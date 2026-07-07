use std::sync::{Arc, Mutex};

use astra_core::sync_poison::recover_mutex_lock;
use astra_runtime::turn::run_control::{
    QueuedRunInputEvent, RunControlStatus, RunInputProvider, RunQueuedInputPoll, RunStatusProvider,
};
use serde_json::Value;

const MAX_DEFERRED_INPUT_CHARS: usize = 20_000;

#[derive(Default)]
struct LocalRunControlState {
    next_event_index: usize,
    inputs: Vec<QueuedRunInputEvent>,
    status: Option<RunControlStatus>,
}

/// In-process run-control provider for the CLI/TUI agentic loop.
///
/// Server-backed runs use the durable run engine for this contract. CLI local
/// runs use this turn-scoped provider so the same runtime polling paths can
/// observe user cancellation and deferred input without requiring a server-side
/// workspace executor.
#[derive(Default)]
pub(crate) struct LocalDeferredInputRunControl {
    // This lock is only held for short in-memory queue mutations and never
    // across an `.await`, so a std::sync::Mutex keeps the local TUI hot path
    // simple without introducing async lock wakeups.
    state: Mutex<LocalRunControlState>,
}

impl LocalDeferredInputRunControl {
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

    pub(crate) fn enqueue_text(&self, text: &str) -> Result<(), String> {
        if text.trim().is_empty() {
            return Err("Deferred input cannot be empty.".to_string());
        }
        if text.chars().count() > MAX_DEFERRED_INPUT_CHARS {
            return Err(format!(
                "Deferred input is too large. Limit it to {MAX_DEFERRED_INPUT_CHARS} characters."
            ));
        }
        self.enqueue_input(serde_json::json!({ "content": text }));
        Ok(())
    }

    pub(crate) fn enqueue_input(&self, input: Value) {
        let mut guard = recover_mutex_lock(&self.state);
        guard.next_event_index += 1;
        let event_index = guard.next_event_index;
        guard
            .inputs
            .push(QueuedRunInputEvent { event_index, input });
    }
}

#[async_trait::async_trait]
impl RunStatusProvider for LocalDeferredInputRunControl {
    async fn control_status(
        &self,
        _user_id: &str,
        _run_id: &str,
    ) -> Result<Option<RunControlStatus>, String> {
        Ok(recover_mutex_lock(&self.state).status)
    }
}

#[async_trait::async_trait]
impl RunInputProvider for LocalDeferredInputRunControl {
    async fn poll_user_inputs(
        &self,
        _user_id: &str,
        _run_id: &str,
        after_event_index: usize,
    ) -> RunQueuedInputPoll {
        let guard = recover_mutex_lock(&self.state);
        let inputs = guard
            .inputs
            .iter()
            .filter(|event| event.event_index > after_event_index)
            .cloned()
            .collect::<Vec<_>>();
        RunQueuedInputPoll {
            next_cursor: guard.next_event_index.max(after_event_index),
            inputs,
            error: None,
        }
    }

    async fn mark_user_inputs_released(
        &self,
        _user_id: &str,
        _run_id: &str,
        event_indices: &[usize],
    ) -> Result<(), String> {
        if event_indices.is_empty() {
            return Ok(());
        }
        let released = event_indices
            .iter()
            .copied()
            .collect::<std::collections::HashSet<_>>();
        let mut guard = recover_mutex_lock(&self.state);
        guard
            .inputs
            .retain(|event| !released.contains(&event.event_index));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn local_run_control_polls_inputs_after_cursor() {
        let provider = LocalDeferredInputRunControl::default();
        provider.enqueue_text("first").expect("enqueue first");
        provider.enqueue_text("second").expect("enqueue second");

        let first = provider
            .poll_user_inputs("local-user", "run-local", 0)
            .await;
        assert_eq!(first.next_cursor, 2);
        assert_eq!(first.inputs.len(), 2);
        assert_eq!(first.inputs[0].input["content"], "first");
        assert_eq!(first.inputs[1].input["content"], "second");

        let second = provider
            .poll_user_inputs("local-user", "run-local", 1)
            .await;
        assert_eq!(second.next_cursor, 2);
        assert_eq!(second.inputs.len(), 1);
        assert_eq!(second.inputs[0].input["content"], "second");
    }

    #[tokio::test]
    async fn local_run_control_reports_cancel_status_through_shared_contract() {
        let provider = LocalDeferredInputRunControl::default();
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
        let provider = LocalDeferredInputRunControl::default();
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
        let provider = LocalDeferredInputRunControl::default();
        let error = provider
            .enqueue_text("   ")
            .expect_err("blank deferred input should be rejected");
        assert!(error.contains("cannot be empty"));
    }

    #[test]
    fn local_run_control_rejects_oversized_input() {
        let provider = LocalDeferredInputRunControl::default();
        let text = "x".repeat(MAX_DEFERRED_INPUT_CHARS + 1);
        let error = provider
            .enqueue_text(&text)
            .expect_err("oversized deferred input should be rejected");
        assert!(error.contains("too large"));
    }

    #[tokio::test]
    async fn local_run_control_evicts_released_inputs() {
        let provider = LocalDeferredInputRunControl::default();
        provider.enqueue_text("first").expect("enqueue first");
        provider.enqueue_text("second").expect("enqueue second");

        provider
            .mark_user_inputs_released("local-user", "run-local", &[1])
            .await
            .expect("release should succeed");

        let remaining = provider
            .poll_user_inputs("local-user", "run-local", 0)
            .await;
        assert_eq!(remaining.next_cursor, 2);
        assert_eq!(remaining.inputs.len(), 1);
        assert_eq!(remaining.inputs[0].event_index, 2);
    }
}
