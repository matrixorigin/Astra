use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// Reserved structured tool-result field for asynchronously evolving work.
///
/// Producers own this record. Consumers must not infer work state from tool
/// names, prose, elapsed time, or transport events.
pub const WORK_UNIT_OBSERVATION_FIELD: &str = "work_unit_observation";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkUnitStatus {
    Pending,
    Running,
    WaitingForInput,
    Stopping,
    Completed,
    CompletedWithIssues,
    Failed,
    Interrupted,
    Cancelled,
    Unavailable,
}

impl WorkUnitStatus {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Running => "running",
            Self::WaitingForInput => "waiting_for_input",
            Self::Stopping => "stopping",
            Self::Completed => "completed",
            Self::CompletedWithIssues => "completed_with_issues",
            Self::Failed => "failed",
            Self::Interrupted => "interrupted",
            Self::Cancelled => "cancelled",
            Self::Unavailable => "unavailable",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "pending" => Some(Self::Pending),
            "running" => Some(Self::Running),
            "waiting_for_input" => Some(Self::WaitingForInput),
            "stopping" => Some(Self::Stopping),
            "completed" => Some(Self::Completed),
            "completed_with_issues" => Some(Self::CompletedWithIssues),
            "failed" => Some(Self::Failed),
            "interrupted" => Some(Self::Interrupted),
            "cancelled" | "killed" => Some(Self::Cancelled),
            "unavailable" => Some(Self::Unavailable),
            _ => None,
        }
    }

    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Completed
                | Self::CompletedWithIssues
                | Self::Failed
                | Self::Interrupted
                | Self::Cancelled
                | Self::Unavailable
        )
    }

    pub fn requires_attention_or_terminal_wake(self) -> bool {
        self.is_terminal() || self == Self::WaitingForInput
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkUnitObservationMode {
    /// Receipt for creation, detach, cancellation, or another state mutation.
    Transition,
    /// One current, non-blocking state read.
    Current,
    /// One bounded wait owned by the runtime.
    Wait,
    /// Explicit historical pagination; never interpreted as live polling.
    Historical,
    /// Explicit evidence/diagnostic read; never interpreted as live polling.
    Diagnostic,
}

impl WorkUnitObservationMode {
    pub fn tracks_live_progress(self) -> bool {
        matches!(self, Self::Transition | Self::Current | Self::Wait)
    }
}

/// Who owns the next user-visible update after a non-terminal observation.
///
/// This is deliberately separate from lifecycle status. A producer can report
/// `running` truthfully without promising that a model turn or UI notification
/// will be scheduled later. Consumers must not invent that promise.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkUnitWakePolicy {
    /// No automatic update is promised. A later observation is caller-owned.
    #[default]
    None,
    /// The runtime will surface one update when the unit becomes terminal.
    OnTerminal,
    /// The runtime will surface attention-required and terminal boundaries.
    OnAttentionOrTerminal,
}

impl WorkUnitWakePolicy {
    pub fn owns_next_update(self) -> bool {
        self != Self::None
    }
}

/// Canonical observation of one asynchronously evolving unit of work.
///
/// `version` is an opaque producer-owned token. It must change whenever the
/// material state represented by this observation changes. Consumers compare
/// it for equality; they do not parse it or manufacture their own revision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkUnitObservation {
    pub id: String,
    pub kind: String,
    pub status: WorkUnitStatus,
    pub version: String,
    pub mode: WorkUnitObservationMode,
    /// Delivery contract for the next meaningful boundary. Defaults to
    /// `none` so older or third-party producers cannot accidentally claim an
    /// automatic wake that they do not implement.
    #[serde(default)]
    pub wake_policy: WorkUnitWakePolicy,
}

impl WorkUnitObservation {
    pub fn new(
        id: impl Into<String>,
        kind: impl Into<String>,
        status: WorkUnitStatus,
        version: impl Into<String>,
        mode: WorkUnitObservationMode,
    ) -> Option<Self> {
        let observation = Self {
            id: id.into(),
            kind: kind.into(),
            status,
            version: version.into(),
            mode,
            wake_policy: WorkUnitWakePolicy::None,
        };
        observation.is_valid().then_some(observation)
    }

    pub fn with_wake_policy(mut self, wake_policy: WorkUnitWakePolicy) -> Self {
        self.wake_policy = wake_policy;
        self
    }

    pub fn is_valid(&self) -> bool {
        !self.id.trim().is_empty()
            && !self.kind.trim().is_empty()
            && !self.version.trim().is_empty()
    }

    pub fn identity(&self) -> String {
        format!("{}:{}", self.kind.trim(), self.id.trim())
    }

    pub fn to_value(&self) -> Value {
        serde_json::to_value(self).expect("WorkUnitObservation is JSON serializable")
    }

    pub fn insert_into(&self, fields: &mut Map<String, Value>) {
        fields.insert(WORK_UNIT_OBSERVATION_FIELD.to_string(), self.to_value());
    }

    pub fn from_fields(fields: &Map<String, Value>) -> Option<Self> {
        let observation: Self =
            serde_json::from_value(fields.get(WORK_UNIT_OBSERVATION_FIELD)?.clone()).ok()?;
        observation.is_valid().then_some(observation)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkUnitObservationOutcome {
    Ignored,
    First,
    Advanced,
    Unchanged { consecutive: u32 },
    Terminal,
}

#[derive(Debug, Clone)]
struct WorkUnitCursor {
    id: String,
    kind: String,
    status: WorkUnitStatus,
    wake_policy: WorkUnitWakePolicy,
    version: String,
    unchanged: u32,
}

/// Latest producer-owned state for one non-terminal work unit.
///
/// This is the settlement-facing view of [`WorkUnitObservationTracker`]. It
/// intentionally omits the producer revision and observation mode: callers
/// deciding whether a turn may claim completion only need identity, lifecycle
/// state, and who owns the next meaningful update.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActiveWorkUnit {
    pub id: String,
    pub kind: String,
    pub status: WorkUnitStatus,
    pub wake_policy: WorkUnitWakePolicy,
}

/// Turn-scoped progress tracker shared by every asynchronous work producer.
///
/// It deliberately knows nothing about tool names or argument shapes.
#[derive(Debug, Clone, Default)]
pub struct WorkUnitObservationTracker {
    cursors: BTreeMap<String, WorkUnitCursor>,
    /// Terminal truth is retained until the turn settles. A snapshot captured
    /// before settlement can be delivered after the producer's terminal
    /// observation; keeping a tombstone prevents that delayed projection from
    /// resurrecting completed work.
    terminal: BTreeMap<String, WorkUnitObservation>,
}

impl WorkUnitObservationTracker {
    pub fn observe(&mut self, observation: &WorkUnitObservation) -> WorkUnitObservationOutcome {
        if !observation.is_valid() || !observation.mode.tracks_live_progress() {
            return WorkUnitObservationOutcome::Ignored;
        }
        let identity = observation.identity();
        if self.terminal.contains_key(&identity) && !observation.status.is_terminal() {
            return WorkUnitObservationOutcome::Ignored;
        }
        if observation.status.is_terminal() {
            self.cursors.remove(&identity);
            self.terminal.insert(identity, observation.clone());
            return WorkUnitObservationOutcome::Terminal;
        }
        match self.cursors.get_mut(&identity) {
            None => {
                self.cursors.insert(
                    identity,
                    WorkUnitCursor {
                        id: observation.id.trim().to_string(),
                        kind: observation.kind.trim().to_string(),
                        status: observation.status,
                        wake_policy: observation.wake_policy,
                        version: observation.version.clone(),
                        unchanged: 0,
                    },
                );
                WorkUnitObservationOutcome::First
            }
            Some(cursor) if cursor.version == observation.version => {
                // Wake ownership is delivery metadata, not lifecycle state.
                // Honor the latest producer contract even if an adapter adds
                // it without changing the underlying work revision.
                cursor.status = observation.status;
                cursor.wake_policy = observation.wake_policy;
                cursor.unchanged = cursor.unchanged.saturating_add(1);
                WorkUnitObservationOutcome::Unchanged {
                    consecutive: cursor.unchanged,
                }
            }
            Some(cursor) => {
                cursor.version.clone_from(&observation.version);
                cursor.status = observation.status;
                cursor.wake_policy = observation.wake_policy;
                cursor.unchanged = 0;
                WorkUnitObservationOutcome::Advanced
            }
        }
    }

    pub fn repeatedly_unchanged_ids(&self, threshold: u32) -> Vec<String> {
        self.cursors
            .values()
            .filter(|cursor| cursor.unchanged >= threshold)
            .map(|cursor| cursor.id.clone())
            .collect()
    }

    pub fn repeatedly_unchanged_without_wake(&self, threshold: u32) -> Vec<String> {
        self.cursors
            .values()
            .filter(|cursor| {
                cursor.unchanged >= threshold && !cursor.wake_policy.owns_next_update()
            })
            .map(|cursor| cursor.id.clone())
            .collect()
    }

    /// Return the current non-terminal truth used by final-answer settlement.
    ///
    /// Terminal observations remove their cursor in [`Self::observe`], so
    /// this snapshot cannot resurrect completed work or infer state from
    /// prose/tool names. The map key keeps ordering deterministic for prompts,
    /// journals, and tests.
    pub fn active_work_units(&self) -> Vec<ActiveWorkUnit> {
        self.cursors
            .values()
            .map(|cursor| ActiveWorkUnit {
                id: cursor.id.clone(),
                kind: cursor.kind.clone(),
                status: cursor.status,
                wake_policy: cursor.wake_policy,
            })
            .collect()
    }

    /// Accepted terminal producer observation for an identity, if any.
    ///
    /// This is intentionally identity-based and does not compare opaque
    /// versions. Arrival at the producer seam establishes order; terminal is
    /// absorbing for a work-unit identity within one turn.
    pub fn terminal_observation(&self, id: &str, kind: &str) -> Option<&WorkUnitObservation> {
        let identity = format!("{}:{}", kind.trim(), id.trim());
        self.terminal.get(&identity)
    }

    pub fn clear(&mut self) {
        self.cursors.clear();
        self.terminal.clear();
    }

    pub fn is_empty(&self) -> bool {
        self.cursors.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_status_protocol_accepts_legacy_killed_without_reemitting_it() {
        for status in [
            WorkUnitStatus::Pending,
            WorkUnitStatus::Running,
            WorkUnitStatus::WaitingForInput,
            WorkUnitStatus::Stopping,
            WorkUnitStatus::Completed,
            WorkUnitStatus::CompletedWithIssues,
            WorkUnitStatus::Failed,
            WorkUnitStatus::Interrupted,
            WorkUnitStatus::Cancelled,
            WorkUnitStatus::Unavailable,
        ] {
            assert_eq!(WorkUnitStatus::parse(status.as_str()), Some(status));
        }
        assert_eq!(
            WorkUnitStatus::parse("killed"),
            Some(WorkUnitStatus::Cancelled)
        );
        assert_eq!(WorkUnitStatus::Cancelled.as_str(), "cancelled");
        assert_eq!(WorkUnitStatus::parse("unknown"), None);
    }

    fn observation(
        version: &str,
        status: WorkUnitStatus,
        mode: WorkUnitObservationMode,
    ) -> WorkUnitObservation {
        WorkUnitObservation::new("work-1", "test", status, version, mode).unwrap()
    }

    #[test]
    fn tracker_uses_producer_version_instead_of_tool_identity() {
        let mut tracker = WorkUnitObservationTracker::default();
        assert_eq!(
            tracker.observe(&observation(
                "v1",
                WorkUnitStatus::Running,
                WorkUnitObservationMode::Transition,
            )),
            WorkUnitObservationOutcome::First
        );
        assert_eq!(
            tracker.observe(&observation(
                "v1",
                WorkUnitStatus::Running,
                WorkUnitObservationMode::Current,
            )),
            WorkUnitObservationOutcome::Unchanged { consecutive: 1 }
        );
        assert_eq!(
            tracker.observe(&observation(
                "v2",
                WorkUnitStatus::Running,
                WorkUnitObservationMode::Wait,
            )),
            WorkUnitObservationOutcome::Advanced
        );
        assert!(tracker.repeatedly_unchanged_ids(1).is_empty());
    }

    #[test]
    fn historical_and_diagnostic_reads_do_not_look_like_live_polling() {
        let mut tracker = WorkUnitObservationTracker::default();
        for mode in [
            WorkUnitObservationMode::Historical,
            WorkUnitObservationMode::Diagnostic,
        ] {
            assert_eq!(
                tracker.observe(&observation("v1", WorkUnitStatus::Running, mode)),
                WorkUnitObservationOutcome::Ignored
            );
        }
        assert!(tracker.is_empty());
    }

    #[test]
    fn terminal_status_is_the_single_source_of_truth() {
        let mut tracker = WorkUnitObservationTracker::default();
        tracker.observe(&observation(
            "v1",
            WorkUnitStatus::Running,
            WorkUnitObservationMode::Current,
        ));
        assert_eq!(
            tracker.observe(&observation(
                "v2",
                WorkUnitStatus::CompletedWithIssues,
                WorkUnitObservationMode::Transition,
            )),
            WorkUnitObservationOutcome::Terminal
        );
        assert!(tracker.is_empty());
        assert_eq!(
            tracker
                .terminal_observation("work-1", "test")
                .map(|observation| observation.status),
            Some(WorkUnitStatus::CompletedWithIssues)
        );
        assert_eq!(
            tracker.observe(&observation(
                "stale-v1",
                WorkUnitStatus::Running,
                WorkUnitObservationMode::Current,
            )),
            WorkUnitObservationOutcome::Ignored,
            "a delayed non-terminal snapshot must not resurrect terminal work"
        );
        assert!(tracker.is_empty());
    }

    #[test]
    fn automatic_wake_is_an_explicit_producer_promise() {
        let mut tracker = WorkUnitObservationTracker::default();
        let first = observation(
            "v1",
            WorkUnitStatus::Running,
            WorkUnitObservationMode::Current,
        );
        tracker.observe(&first);
        tracker.observe(&first);
        assert_eq!(tracker.repeatedly_unchanged_without_wake(1), ["work-1"]);

        let promised = first.with_wake_policy(WorkUnitWakePolicy::OnTerminal);
        tracker.observe(&promised);
        assert!(tracker.repeatedly_unchanged_without_wake(1).is_empty());
    }

    #[test]
    fn active_work_snapshot_tracks_latest_nonterminal_truth_and_drops_terminal_units() {
        let mut tracker = WorkUnitObservationTracker::default();
        let running = observation(
            "revision-1",
            WorkUnitStatus::Running,
            WorkUnitObservationMode::Current,
        )
        .with_wake_policy(WorkUnitWakePolicy::OnTerminal);
        assert_eq!(tracker.observe(&running), WorkUnitObservationOutcome::First);
        assert_eq!(
            tracker.active_work_units(),
            [ActiveWorkUnit {
                id: "work-1".to_string(),
                kind: "test".to_string(),
                status: WorkUnitStatus::Running,
                wake_policy: WorkUnitWakePolicy::OnTerminal,
            }]
        );

        let waiting = observation(
            "revision-2",
            WorkUnitStatus::WaitingForInput,
            WorkUnitObservationMode::Transition,
        )
        .with_wake_policy(WorkUnitWakePolicy::OnAttentionOrTerminal);
        assert_eq!(
            tracker.observe(&waiting),
            WorkUnitObservationOutcome::Advanced
        );
        assert_eq!(
            tracker.active_work_units()[0].status,
            WorkUnitStatus::WaitingForInput
        );

        let completed = observation(
            "revision-3",
            WorkUnitStatus::Completed,
            WorkUnitObservationMode::Transition,
        );
        assert_eq!(
            tracker.observe(&completed),
            WorkUnitObservationOutcome::Terminal
        );
        assert!(tracker.active_work_units().is_empty());
    }
}
