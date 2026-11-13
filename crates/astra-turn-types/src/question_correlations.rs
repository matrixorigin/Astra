//! Bounded advisory question identities carried by canonical execution control.

use serde::{Deserialize, Serialize};

use crate::AgentCommunicationParty;

pub const MAX_QUESTION_CORRELATIONS: usize = 16;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QuestionCorrelation {
    pub request_id: String,
    pub expected_responder: AgentCommunicationParty,
}

/// Facts, not authority to launch an executor. The caller must validate the
/// producer against the paired checkpoint budget and execution custody.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QuestionCorrelationsSnapshotV1 {
    pub run_id: String,
    pub producer_owner_generation: u64,
    pub pending: Vec<QuestionCorrelation>,
}

impl QuestionCorrelationsSnapshotV1 {
    pub fn validate_owner(&self, run_id: &str, generation: u64) -> Result<(), &'static str> {
        if run_id.is_empty()
            || self.run_id != run_id
            || self.producer_owner_generation != generation
        {
            return Err("question correlations belong to another execution owner");
        }
        if self.pending.len() > MAX_QUESTION_CORRELATIONS {
            return Err("too many question correlations");
        }
        for (index, reply) in self.pending.iter().enumerate() {
            if reply.request_id.is_empty()
                || reply.expected_responder.run_id.is_empty()
                || reply.expected_responder.agent_id.is_empty()
            {
                return Err("question has no exact request or responder identity");
            }
            if self.pending[..index]
                .iter()
                .any(|prior| prior.request_id == reply.request_id)
            {
                return Err("question message identity was already reserved");
            }
        }
        Ok(())
    }
}
