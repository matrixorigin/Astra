//! Bounded advisory question identities. This owner never decides whether a run waits or finishes.

use astra_messaging::{AgentAddress, AgentMessage, MessagePayload};
pub use astra_turn_types::{
    MAX_QUESTION_CORRELATIONS, QuestionCorrelation, QuestionCorrelationsSnapshotV1,
};
use std::collections::VecDeque;
use std::sync::Mutex;

#[derive(Default)]
struct CorrelationState {
    owner_run_id: Option<String>,
    pending: VecDeque<QuestionCorrelation>,
}

#[derive(Default)]
pub struct QuestionCorrelations {
    state: Mutex<CorrelationState>,
}

impl QuestionCorrelations {
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
        let mut state = astra_core::sync_poison::recover_mutex_lock(&self.state);
        match state.owner_run_id.as_deref() {
            Some(owner) if owner != owner_run_id => return Err("question belongs to another run"),
            None => state.owner_run_id = Some(owner_run_id.to_string()),
            _ => {}
        }
        if state
            .pending
            .iter()
            .any(|entry| entry.request_id == request_id)
        {
            return Err("question message identity was already reserved");
        }
        // Losing advisory correlation makes a later response unverified. It
        // must not prevent an otherwise authorized message from being sent.
        if state.pending.len() == MAX_QUESTION_CORRELATIONS {
            state.pending.pop_front();
        }
        state.pending.push_back(QuestionCorrelation {
            request_id: request_id.into(),
            expected_responder: astra_turn_types::AgentCommunicationParty {
                run_id: expected_responder.run_id,
                agent_id: expected_responder.agent_id,
            },
        });
        Ok(())
    }

    /// Definite non-delivery removes metadata; uncertain delivery retains the original identity.
    pub fn reject(&self, owner_run_id: &str, request_id: &str) {
        let mut state = astra_core::sync_poison::recover_mutex_lock(&self.state);
        if state.owner_run_id.as_deref() == Some(owner_run_id) {
            state.pending.retain(|entry| entry.request_id != request_id);
        }
    }

    pub fn classify_response(&self, owner_run_id: &str, message: &AgentMessage) -> &'static str {
        if astra_messaging::types::is_non_adoption_response(&message.payload) {
            return "not_adopted";
        }
        let MessagePayload::Response { request_id, .. } = &message.payload else {
            return "unverified";
        };
        let state = astra_core::sync_poison::recover_mutex_lock(&self.state);
        match state.pending.iter().find(|entry| {
            state.owner_run_id.as_deref() == Some(owner_run_id) && entry.request_id == *request_id
        }) {
            Some(entry) if matches_responder(entry, &message.from) => "matched",
            Some(_) => "mismatch",
            None => "unverified",
        }
    }

    #[cfg(test)]
    pub fn observe_response(&self, owner_run_id: &str, message: &AgentMessage) -> bool {
        if astra_messaging::types::is_non_adoption_response(&message.payload) {
            return false;
        }
        let MessagePayload::Response { request_id, .. } = &message.payload else {
            return false;
        };
        self.observe_typed_response(owner_run_id, request_id, &message.from)
    }

    /// Observation updates correlation only after the existing provider-attempt lease commits.
    pub fn observe_typed_response(
        &self,
        owner_run_id: &str,
        request_id: &str,
        responder: &AgentAddress,
    ) -> bool {
        let mut state = astra_core::sync_poison::recover_mutex_lock(&self.state);
        if state.owner_run_id.as_deref() != Some(owner_run_id) {
            return false;
        }
        let Some(index) = state.pending.iter().position(|entry| {
            entry.request_id == request_id && matches_responder(entry, responder)
        }) else {
            return false;
        };
        state.pending.remove(index);
        true
    }

    pub fn snapshot(
        &self,
        run_id: &str,
        producer_owner_generation: u64,
    ) -> Result<QuestionCorrelationsSnapshotV1, &'static str> {
        let state = astra_core::sync_poison::recover_mutex_lock(&self.state);
        if state
            .owner_run_id
            .as_deref()
            .is_some_and(|owner| owner != run_id)
        {
            return Err("question correlations belong to another execution owner");
        }
        let snapshot = QuestionCorrelationsSnapshotV1 {
            run_id: run_id.into(),
            producer_owner_generation,
            pending: state.pending.iter().cloned().collect(),
        };
        snapshot.validate_owner(run_id, producer_owner_generation)?;
        Ok(snapshot)
    }

    pub fn restore(
        &self,
        snapshot: &QuestionCorrelationsSnapshotV1,
        run_id: &str,
        producer_owner_generation: u64,
    ) -> Result<(), &'static str> {
        snapshot.validate_owner(run_id, producer_owner_generation)?;
        let mut state = astra_core::sync_poison::recover_mutex_lock(&self.state);
        if state.owner_run_id.is_some() || !state.pending.is_empty() {
            return Err("question correlations already have a live owner");
        }
        state.owner_run_id = Some(run_id.into());
        state.pending = snapshot.pending.iter().cloned().collect();
        Ok(())
    }

    pub fn pending(&self, owner_run_id: &str) -> Vec<QuestionCorrelation> {
        let state = astra_core::sync_poison::recover_mutex_lock(&self.state);
        if state.owner_run_id.as_deref() != Some(owner_run_id) {
            return Vec::new();
        }
        state.pending.iter().cloned().collect()
    }
}

fn matches_responder(pending: &QuestionCorrelation, responder: &AgentAddress) -> bool {
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
    fn non_adoption_feedback_is_visible_without_settling_an_unrelated_question() {
        let replies = QuestionCorrelations::default();
        let sender = AgentAddress::new("native", "collaborator");
        replies.reserve("parent", "q1", sender.clone()).unwrap();
        let mut receipt = response(sender, "q1");
        receipt.payload = MessagePayload::Response {
            request_id: "q1".into(),
            accepted: false,
            data: Some(serde_json::json!({"disposition":"not_adopted", "message_id":"q1"})),
        };
        assert_eq!(replies.classify_response("parent", &receipt), "not_adopted");
        assert!(!replies.observe_response("parent", &receipt));
        assert!(!replies.pending("parent").is_empty());
    }

    #[test]
    fn only_the_exact_responder_and_request_can_settle_a_question() {
        let replies = QuestionCorrelations::default();
        let parent = AgentAddress::new("parent", "orchestrator");
        replies.reserve("questioner", "q1", parent.clone()).unwrap();

        assert!(!replies.observe_response(
            "questioner",
            &response(AgentAddress::new("stranger", "orchestrator"), "q1"),
        ));
        assert!(!replies.observe_response("questioner", &response(parent.clone(), "other")));
        assert!(!replies.observe_response("other-run", &response(parent.clone(), "q1")));
        assert_eq!(
            replies.classify_response("questioner", &response(parent.clone(), "other")),
            "unverified"
        );
        assert_eq!(
            replies.classify_response(
                "questioner",
                &response(AgentAddress::new("stranger", "orchestrator"), "q1")
            ),
            "mismatch"
        );
        assert_eq!(
            replies.classify_response("questioner", &response(parent.clone(), "q1")),
            "matched"
        );
        let text = AgentMessage::new(
            parent.clone(),
            MessageTarget::Parent,
            MessagePayload::Text {
                content: "informal reply".into(),
                summary: None,
            },
        );
        assert!(!replies.observe_response("questioner", &text));
        assert!(!replies.pending("questioner").is_empty());

        assert!(replies.observe_response("questioner", &response(parent.clone(), "q1")));
        assert_eq!(
            replies.classify_response("questioner", &response(parent.clone(), "q1")),
            "unverified"
        );
        assert!(!replies.observe_response("questioner", &response(parent, "q1")));
        assert!(replies.pending("questioner").is_empty());
    }

    #[test]
    fn serialized_snapshot_restores_exact_bounded_owner_without_overwriting_live_state() {
        let replies = QuestionCorrelations::default();
        let parent = AgentAddress::new("parent-mailbox", "orchestrator");
        replies.reserve("questioner", "q1", parent.clone()).unwrap();
        let wire = serde_json::to_value(replies.snapshot("questioner", 7).unwrap()).unwrap();
        let snapshot: QuestionCorrelationsSnapshotV1 =
            serde_json::from_value(wire.clone()).unwrap();
        let restored = QuestionCorrelations::default();
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
        assert!(!restored.pending("questioner").is_empty());
        assert!(restored.observe_response("questioner", &response(parent, "q1")));
        assert!(restored.snapshot("another-run", 7).is_err());

        let mut missing = wire.clone();
        missing.as_object_mut().unwrap().remove("pending");
        assert!(serde_json::from_value::<QuestionCorrelationsSnapshotV1>(missing).is_err());
        for pending in [
            vec![wire["pending"][0].clone(); 2],
            vec![wire["pending"][0].clone(); MAX_QUESTION_CORRELATIONS + 1],
            vec![
                serde_json::json!({"request_id":"q1", "expected_responder":{"run_id":"", "agent_id":"parent"}}),
            ],
        ] {
            let mut invalid = wire.clone();
            invalid["pending"] = serde_json::json!(pending);
            let invalid: QuestionCorrelationsSnapshotV1 = serde_json::from_value(invalid).unwrap();
            assert!(
                QuestionCorrelations::default()
                    .restore(&invalid, "questioner", 7)
                    .is_err()
            );
        }
    }

    #[test]
    fn bounded_correlations_evict_oldest_without_rejecting_new_questions() {
        let replies = QuestionCorrelations::default();
        let parent = AgentAddress::new("parent", "orchestrator");
        for index in 0..=MAX_QUESTION_CORRELATIONS {
            replies
                .reserve("questioner", &format!("q{index}"), parent.clone())
                .unwrap();
        }
        assert_eq!(
            replies.pending("questioner").len(),
            MAX_QUESTION_CORRELATIONS
        );
        assert_eq!(
            replies.classify_response("questioner", &response(parent.clone(), "q0")),
            "unverified"
        );
        assert_eq!(
            replies.classify_response("questioner", &response(parent.clone(), "q1")),
            "matched"
        );
        replies.reject("other-run", "q1");
        assert_eq!(
            replies.pending("questioner").len(),
            MAX_QUESTION_CORRELATIONS
        );
        assert_eq!(
            replies.reserve("other-run", "q", parent),
            Err("question belongs to another run")
        );
        replies.reject("questioner", "q1");
        assert_eq!(
            replies.pending("questioner").len(),
            MAX_QUESTION_CORRELATIONS - 1
        );
    }
}
