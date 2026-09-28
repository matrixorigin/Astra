//! Run-owned obligations created by inter-agent questions.
//!
//! The mailbox transports envelopes; it does not decide whether an agent may
//! finish. This small state is shared by the sending tool and the receiving
//! loop so a question cannot be followed by an unanswerable terminal result.

use std::collections::BTreeMap;
use std::sync::Mutex;

use astra_messaging::{AgentAddress, AgentMessage, MessagePayload};

const MAX_PENDING_REPLIES: usize = 16;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PendingReply {
    pub request_id: String,
    pub expected_responder: AgentAddress,
}

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
                expected_responder,
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
    pub fn observe_response(&self, owner_run_id: &str, message: &AgentMessage) -> bool {
        let MessagePayload::Response { request_id, .. } = &message.payload else {
            return false;
        };
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if state.owner_run_id.as_deref() != Some(owner_run_id)
            || !state
                .pending
                .get(request_id)
                .is_some_and(|pending| pending.expected_responder == message.from)
        {
            return false;
        }
        state.pending.remove(request_id);
        true
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
            .is_some_and(|pending| pending.expected_responder == message.from)
    }
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
