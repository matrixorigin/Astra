//! Run-scoped identity shared by durable approval consumers.
//!
//! Delivery transports do not own interaction authority or lifecycle.

/// The session and run scope of one durable interaction.
#[derive(Debug, Clone)]
pub struct ApprovalJournalContext {
    pub session_id: String,
    pub run_id: String,
    pub turn: Option<u32>,
}

impl ApprovalJournalContext {
    pub fn new(session_id: String, run_id: String, turn: Option<u32>) -> Self {
        Self {
            session_id,
            run_id,
            turn,
        }
    }
}
