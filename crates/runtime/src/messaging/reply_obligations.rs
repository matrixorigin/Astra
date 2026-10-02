//! Run-owned obligations created by inter-agent questions.
//!
//! The mailbox transports envelopes; it does not decide whether an agent may
//! finish. This small state is shared by the sending tool and the receiving
//! loop so a question cannot be followed by an unanswerable terminal result.

use std::collections::BTreeMap;
use std::sync::Mutex;

use astra_messaging::{AgentAddress, AgentMessage, MessagePayload};

pub use astra_turn_types::{MAX_PENDING_REPLIES, PendingReply, ReplyObligationsSnapshotV1};

#[derive(Default)]
struct ReplyState {
    owner_run_id: Option<String>,
    pending: BTreeMap<String, PendingReply>,
}

/// One execution's bounded, exact request/response completion obligations.
/// Ordinary runs pay only for an empty in-memory check; no database access is
/// introduced by this state.
#[derive(Default)]
pub struct ReplyObligations {
    state: Mutex<ReplyState>,
}

impl ReplyObligations {
    pub fn reserve(
        &self,
        owner_run_id: &str,
        request_id: &str,
        expected_responder: AgentAddress,
    ) -> Result<(), &'static str> {
        if owner_run_id.is_empty() || request_id.is_empty() {
            return Err("question has no run or message identity");
        }
        if expected_responder.run_id.is_empty() || expected_responder.agent_id.is_empty() {
            return Err("question has no exact responder identity");
        }
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        match state.owner_run_id.as_deref() {
            Some(owner) if owner != owner_run_id => {
                return Err("question belongs to another run");
            }
            None => state.owner_run_id = Some(owner_run_id.to_string()),
            _ => {}
        }
        if state.pending.len() >= MAX_PENDING_REPLIES {
            return Err("too many unanswered agent questions");
        }
        if state.pending.contains_key(request_id) {
            return Err("question message identity was already reserved");
        }
        state.pending.insert(
            request_id.to_string(),
            PendingReply {
                request_id: request_id.to_string(),
                expected_responder: astra_turn_types::AgentCommunicationParty {
                    run_id: expected_responder.run_id,
                    agent_id: expected_responder.agent_id,
                },
            },
        );
        Ok(())
    }

    /// Roll back only a definitively rejected send, never a transport-unknown
    /// send that may already have reached the recipient.
    pub fn reject(&self, owner_run_id: &str, request_id: &str) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if state.owner_run_id.as_deref() == Some(owner_run_id) {
            state.pending.remove(request_id);
        }
    }

    /// A response must come from the canonical recipient of this exact
    /// question. Unrelated text, wrong IDs and wrong senders cannot settle it.
    #[cfg(test)]
    pub fn observe_response(&self, owner_run_id: &str, message: &AgentMessage) -> bool {
        let MessagePayload::Response { request_id, .. } = &message.payload else {
            return false;
        };
        self.observe_typed_response(owner_run_id, request_id, &message.from)
    }

    /// Model observation uses retained typed context, not a transport envelope
    /// or claim token that disappears on restore.
    pub fn observe_typed_response(
        &self,
        owner_run_id: &str,
        request_id: &str,
        responder: &AgentAddress,
    ) -> bool {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if state.owner_run_id.as_deref() != Some(owner_run_id)
            || !state
                .pending
                .get(request_id)
                .is_some_and(|pending| matches_responder(pending, responder))
        {
            return false;
        }
        state.pending.remove(request_id);
        true
    }

    pub fn snapshot(
        &self,
        run_id: &str,
        producer_owner_generation: u64,
    ) -> Result<ReplyObligationsSnapshotV1, &'static str> {
        let state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if state
            .owner_run_id
            .as_deref()
            .is_some_and(|owner| owner != run_id)
        {
            return Err("reply obligations belong to another execution owner");
        }
        let snapshot = ReplyObligationsSnapshotV1 {
            run_id: run_id.into(),
            producer_owner_generation,
            pending: state.pending.values().cloned().collect(),
        };
        snapshot.validate_owner(run_id, producer_owner_generation)?;
        Ok(snapshot)
    }

    /// Restore into the same shared owner before wiring tools. Never overwrite
    /// a live sender's state, even with a superficially valid checkpoint.
    #[cfg(test)]
    pub fn restore(
        &self,
        snapshot: &ReplyObligationsSnapshotV1,
        run_id: &str,
        producer_owner_generation: u64,
    ) -> Result<(), &'static str> {
        snapshot.validate_owner(run_id, producer_owner_generation)?;
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if state.owner_run_id.is_some() || !state.pending.is_empty() {
            return Err("reply obligations already have a live owner");
        }
        state.owner_run_id = Some(run_id.into());
        state.pending = snapshot
            .pending
            .iter()
            .cloned()
            .map(|reply| (reply.request_id.clone(), reply))
            .collect();
        Ok(())
    }

    pub fn pending(&self, owner_run_id: &str) -> Vec<PendingReply> {
        let state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if state.owner_run_id.as_deref() != Some(owner_run_id) {
            return Vec::new();
        }
        state.pending.values().cloned().collect()
    }

    pub fn has_pending(&self, owner_run_id: &str) -> bool {
        let state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.owner_run_id.as_deref() == Some(owner_run_id) && !state.pending.is_empty()
    }

    /// A response that claims to settle an outstanding question must match
    /// its sender and ID before it is presented as an answer to the model.
    /// Runs with no question retain the ordinary response-message behavior.
    pub fn should_expose_response(&self, owner_run_id: &str, message: &AgentMessage) -> bool {
        let MessagePayload::Response { request_id, .. } = &message.payload else {
            return true;
        };
        let state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if state.owner_run_id.as_deref() != Some(owner_run_id) || state.pending.is_empty() {
            return true;
        }
        state
            .pending
            .get(request_id)
            .is_some_and(|pending| matches_responder(pending, &message.from))
    }
}

fn matches_responder(pending: &PendingReply, responder: &AgentAddress) -> bool {
    pending.expected_responder.run_id == responder.run_id
        && pending.expected_responder.agent_id == responder.agent_id
}

#[cfg(test)]
mod tests {
    use super::*;
    use astra_messaging::MessageTarget;

    fn response(from: AgentAddress, request_id: &str) -> AgentMessage {
        AgentMessage::new(
            from,
            MessageTarget::Direct {
                address: AgentAddress::new("questioner", "worker"),
            },
            MessagePayload::Response {
                request_id: request_id.into(),
                accepted: true,
                data: None,
            },
        )
    }

    #[test]
    fn only_the_exact_responder_and_request_can_settle_a_question() {
        let replies = ReplyObligations::default();
        let parent = AgentAddress::new("parent", "orchestrator");
        replies.reserve("questioner", "q1", parent.clone()).unwrap();

        assert!(!replies.observe_response(
            "questioner",
            &response(AgentAddress::new("stranger", "orchestrator"), "q1"),
        ));
        assert!(!replies.observe_response("questioner", &response(parent.clone(), "other")));
        assert!(!replies.observe_response("other-run", &response(parent.clone(), "q1")));
        assert!(!replies.should_expose_response("questioner", &response(parent.clone(), "other")));
        assert!(!replies.should_expose_response(
            "questioner",
            &response(AgentAddress::new("stranger", "orchestrator"), "q1"),
        ));
        let text = AgentMessage::new(
            parent.clone(),
            MessageTarget::Parent,
            MessagePayload::Text {
                content: "informal reply".into(),
                summary: None,
            },
        );
        assert!(!replies.observe_response("questioner", &text));
        assert!(replies.has_pending("questioner"));

        assert!(replies.observe_response("questioner", &response(parent.clone(), "q1")));
        assert!(replies.should_expose_response("questioner", &response(parent.clone(), "q1")));
        assert!(!replies.observe_response("questioner", &response(parent, "q1")));
        assert!(!replies.has_pending("questioner"));
    }

    #[test]
    fn serialized_snapshot_restores_exact_bounded_owner_without_overwriting_live_state() {
        let replies = ReplyObligations::default();
        let parent = AgentAddress::new("parent-mailbox", "orchestrator");
        replies.reserve("questioner", "q1", parent.clone()).unwrap();
        let wire = serde_json::to_value(replies.snapshot("questioner", 7).unwrap()).unwrap();
        let snapshot: ReplyObligationsSnapshotV1 = serde_json::from_value(wire.clone()).unwrap();
        let restored = ReplyObligations::default();
        assert!(restored.restore(&snapshot, "another-run", 7).is_err());
        assert!(restored.restore(&snapshot, "questioner", 8).is_err());
        restored.restore(&snapshot, "questioner", 7).unwrap();
        assert_eq!(
            restored.pending("questioner"),
            replies.pending("questioner")
        );
        assert!(restored.restore(&snapshot, "questioner", 7).is_err());
        assert!(!restored.observe_response("questioner", &response(parent.clone(), "wrong-id")));
        assert!(!restored.observe_response(
            "questioner",
            &response(AgentAddress::new("other-parent", "orchestrator"), "q1")
        ));
        assert!(!restored.observe_response("another-run", &response(parent.clone(), "q1")));
        assert!(restored.has_pending("questioner"));
        assert!(restored.observe_response("questioner", &response(parent, "q1")));
        assert!(restored.snapshot("another-run", 7).is_err());

        let mut missing = wire.clone();
        missing.as_object_mut().unwrap().remove("pending");
        assert!(serde_json::from_value::<ReplyObligationsSnapshotV1>(missing).is_err());
        for pending in [
            vec![wire["pending"][0].clone(); 2],
            vec![wire["pending"][0].clone(); MAX_PENDING_REPLIES + 1],
            vec![
                serde_json::json!({"request_id":"q1", "expected_responder":{"run_id":"", "agent_id":"parent"}}),
            ],
        ] {
            let mut invalid = wire.clone();
            invalid["pending"] = serde_json::json!(pending);
            let invalid: ReplyObligationsSnapshotV1 = serde_json::from_value(invalid).unwrap();
            assert!(
                ReplyObligations::default()
                    .restore(&invalid, "questioner", 7)
                    .is_err()
            );
        }
    }

    #[test]
    fn obligations_are_run_scoped_and_bounded() {
        let replies = ReplyObligations::default();
        let parent = AgentAddress::new("parent", "orchestrator");
        for index in 0..MAX_PENDING_REPLIES {
            replies
                .reserve("questioner", &format!("q{index}"), parent.clone())
                .unwrap();
        }
        assert_eq!(replies.pending("questioner").len(), MAX_PENDING_REPLIES);
        assert_eq!(
            replies.reserve("questioner", "overflow", parent.clone()),
            Err("too many unanswered agent questions")
        );
        replies.reject("other-run", "q0");
        assert!(replies.has_pending("questioner"));
        assert_eq!(
            replies.reserve("other-run", "q", parent.clone()),
            Err("question belongs to another run")
        );
        replies.reject("questioner", "q0");
        assert_eq!(replies.pending("questioner").len(), MAX_PENDING_REPLIES - 1);
        replies
            .reserve("questioner", "replacement", parent)
            .unwrap();
    }
}
