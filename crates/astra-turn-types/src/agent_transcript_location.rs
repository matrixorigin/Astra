use serde::{Deserialize, Serialize};

/// The authoritative store for one agent run's canonical conversation.
///
/// This belongs to the run identity rather than to a control lease: a client
/// may read a transcript before it is entitled to pause, resume, or cancel
/// that run. Every live agent event carries this fact so a workbench can open
/// the same real conversation even if a tool-launch receipt is delayed or
/// lost.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentTranscriptLocation {
    LocalJournal,
    DurableServer,
}

impl AgentTranscriptLocation {
    pub const fn wire_value(self) -> &'static str {
        match self {
            Self::LocalJournal => "local_journal",
            Self::DurableServer => "durable_server",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::AgentTranscriptLocation;

    #[test]
    fn wire_values_round_trip_with_the_delivery_contract() {
        for (location, wire) in [
            (AgentTranscriptLocation::LocalJournal, "local_journal"),
            (AgentTranscriptLocation::DurableServer, "durable_server"),
        ] {
            assert_eq!(location.wire_value(), wire);
            assert_eq!(
                serde_json::from_str::<AgentTranscriptLocation>(&format!("\"{wire}\"")).unwrap(),
                location
            );
        }
    }
}
