use std::sync::{Arc, Mutex};

use astra_core::sync_poison::recover_mutex_lock;
use astra_runtime::turn::run_control::{
    QueuedRunInputEvent, RunControlStatus, RunInputProvider, RunQueuedInputPoll, RunStatusProvider,
};
use serde_json::Value;

#[derive(Default)]
struct LocalRunControlState {
    next_event_index: usize,
    inputs: Vec<QueuedRunInputEvent>,
}

/// In-process deferred-input queue for the CLI/TUI agentic loop.
///
/// The local `/chat/turn` SSE loop is not backed by a durable run record,
/// so `/chat/runs/{run_id}/input` cannot address it. This provider lets the
/// runtime's existing deferred-input release logic operate against a
/// turn-scoped in-memory queue instead.
#[derive(Default)]
pub(crate) struct LocalDeferredInputRunControl {
    state: Mutex<LocalRunControlState>,
}

impl LocalDeferredInputRunControl {
    pub(crate) fn shared() -> Arc<Self> {
        Arc::new(Self::default())
    }

    pub(crate) fn enqueue_text(&self, text: &str) -> Result<(), String> {
        if text.trim().is_empty() {
            return Err("Deferred input cannot be empty.".to_string());
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
    async fn control_status(&self, _run_id: &str) -> Option<RunControlStatus> {
        None
    }
}

#[async_trait::async_trait]
impl RunInputProvider for LocalDeferredInputRunControl {
    async fn poll_user_inputs(
        &self,
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

    async fn mark_user_inputs_released(&self, _run_id: &str, _event_indices: &[usize]) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn local_run_control_polls_inputs_after_cursor() {
        let provider = LocalDeferredInputRunControl::default();
        provider.enqueue_text("first").expect("enqueue first");
        provider.enqueue_text("second").expect("enqueue second");

        let first = provider.poll_user_inputs("run-local", 0).await;
        assert_eq!(first.next_cursor, 2);
        assert_eq!(first.inputs.len(), 2);
        assert_eq!(first.inputs[0].input["content"], "first");
        assert_eq!(first.inputs[1].input["content"], "second");

        let second = provider.poll_user_inputs("run-local", 1).await;
        assert_eq!(second.next_cursor, 2);
        assert_eq!(second.inputs.len(), 1);
        assert_eq!(second.inputs[0].input["content"], "second");
    }

    #[test]
    fn local_run_control_rejects_blank_input() {
        let provider = LocalDeferredInputRunControl::default();
        let error = provider
            .enqueue_text("   ")
            .expect_err("blank deferred input should be rejected");
        assert!(error.contains("cannot be empty"));
    }
}
