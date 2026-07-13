//! Typed state carried by the Agent monitor projection.
//!
//! Agent lifecycle and projection confidence are deliberately separate:
//! receiving a recent live event can tell us what was last observed, while
//! only reconciliation with an owning runtime or the durable server can make
//! that state authoritative. A dropped stream therefore degrades confidence;
//! it does not invent a terminal outcome.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AgentRunStatus {
    Starting,
    Running,
    Waiting,
    Paused,
    Pausing,
    Resuming,
    Cancelling,
    Completed,
    Delegated,
    Interrupted,
    Failed,
    Cancelled,
}

impl AgentRunStatus {
    pub(crate) fn is_active(self) -> bool {
        matches!(
            self,
            AgentRunStatus::Starting
                | AgentRunStatus::Running
                | AgentRunStatus::Waiting
                | AgentRunStatus::Paused
                | AgentRunStatus::Pausing
                | AgentRunStatus::Resuming
                | AgentRunStatus::Cancelling
        )
    }

    pub(crate) fn is_terminal(self) -> bool {
        !self.is_active()
    }

    pub(crate) fn is_failure(self) -> bool {
        matches!(self, AgentRunStatus::Failed)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AgentProjectionConfidence {
    /// A structured live event was observed, but has not been reconciled with
    /// the owning runtime or durable store.
    Observed,
    /// The owning local runtime or durable server confirmed this projection.
    Confirmed,
    /// A previously confirmed projection is older than its freshness window.
    Stale,
    /// The last non-terminal observation can no longer be asserted, typically
    /// because its event stream ended before a terminal event arrived.
    Unconfirmed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AgentProjectionSource {
    LiveStream,
    /// Terminal local lifecycle events reconstructed from the canonical
    /// session journal after the in-memory spawner is gone.
    LocalJournal,
    LocalRuntime,
    DurableServer,
    WorkspaceSnapshot,
    LocalIntent,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum AgentControlTarget {
    LocalAgent {
        agent_id: String,
    },
    /// A local child owned by the delegation engine rather than the dynamic
    /// agent spawner. It has a canonical local transcript and supports a
    /// cooperative cancel request, but no fake pause/resume control.
    LocalDelegatedRun {
        run_id: String,
    },
    DurableRun {
        run_id: String,
    },
}

/// Authoritative read path for a run transcript.
///
/// This intentionally does not carry pause/resume/cancel authority. A launch
/// receipt can establish where the canonical history lives before an owning
/// runtime has published which controls are currently available.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AgentTranscriptTarget {
    LocalJournal,
    DurableServer,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct AgentActivityCounts {
    pub tool_calls: usize,
    pub child_agents: usize,
    pub messages_sent: usize,
    pub messages_received: usize,
    /// The durable server snapshot was bounded, so `child_agents` is only the
    /// number observed in that snapshot rather than a complete direct-child
    /// count. Local runtime counts are always complete.
    pub child_agents_partial: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct AgentRunState {
    pub status: AgentRunStatus,
    pub confidence: AgentProjectionConfidence,
    pub source: AgentProjectionSource,
}

impl AgentRunState {
    pub(crate) fn observed(status: AgentRunStatus) -> Self {
        Self {
            status,
            confidence: AgentProjectionConfidence::Observed,
            source: AgentProjectionSource::LiveStream,
        }
    }

    pub(crate) fn unconfirmed(status: AgentRunStatus) -> Self {
        Self {
            status,
            confidence: AgentProjectionConfidence::Unconfirmed,
            source: AgentProjectionSource::LiveStream,
        }
    }

    pub(crate) fn local_intent(status: AgentRunStatus) -> Self {
        Self {
            status,
            confidence: AgentProjectionConfidence::Observed,
            source: AgentProjectionSource::LocalIntent,
        }
    }

    pub(crate) fn confirmed_local(status: AgentRunStatus) -> Self {
        Self {
            status,
            confidence: AgentProjectionConfidence::Confirmed,
            source: AgentProjectionSource::LocalRuntime,
        }
    }

    pub(crate) fn confirmed_local_journal(status: AgentRunStatus) -> Self {
        Self {
            status,
            confidence: AgentProjectionConfidence::Confirmed,
            source: AgentProjectionSource::LocalJournal,
        }
    }

    pub(crate) fn confirmed_server(status: AgentRunStatus) -> Self {
        Self {
            status,
            confidence: AgentProjectionConfidence::Confirmed,
            source: AgentProjectionSource::DurableServer,
        }
    }

    pub(crate) fn stale_workspace(status: AgentRunStatus) -> Self {
        Self {
            status,
            confidence: AgentProjectionConfidence::Stale,
            source: AgentProjectionSource::WorkspaceSnapshot,
        }
    }

    pub(crate) fn is_actionable_active(self) -> bool {
        self.status.is_active()
            && matches!(
                self.confidence,
                AgentProjectionConfidence::Observed | AgentProjectionConfidence::Confirmed
            )
    }

    pub(crate) fn mark_unconfirmed_if_active(&mut self) -> bool {
        if self.status.is_active()
            && !matches!(self.confidence, AgentProjectionConfidence::Unconfirmed)
        {
            self.confidence = AgentProjectionConfidence::Unconfirmed;
            return true;
        }
        false
    }

    pub(crate) fn mark_stale_if_active(&mut self) -> bool {
        if self.status.is_active() && !matches!(self.confidence, AgentProjectionConfidence::Stale) {
            self.confidence = AgentProjectionConfidence::Stale;
            return true;
        }
        false
    }

    pub(crate) fn source_rank(self) -> u8 {
        match self.source {
            AgentProjectionSource::WorkspaceSnapshot => 1,
            AgentProjectionSource::LiveStream | AgentProjectionSource::LocalIntent => 2,
            AgentProjectionSource::LocalJournal => 3,
            AgentProjectionSource::LocalRuntime => 4,
            AgentProjectionSource::DurableServer => 5,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stream_gap_degrades_active_state_without_inventing_terminal_status() {
        let mut state = AgentRunState::observed(AgentRunStatus::Running);

        assert!(state.mark_unconfirmed_if_active());
        assert_eq!(state.status, AgentRunStatus::Running);
        assert_eq!(state.confidence, AgentProjectionConfidence::Unconfirmed);
        assert!(!state.is_actionable_active());
    }

    #[test]
    fn stream_gap_does_not_degrade_explicit_terminal_observation() {
        let mut state = AgentRunState::observed(AgentRunStatus::Completed);

        assert!(!state.mark_unconfirmed_if_active());
        assert_eq!(state.status, AgentRunStatus::Completed);
        assert_eq!(state.confidence, AgentProjectionConfidence::Observed);
    }
}
