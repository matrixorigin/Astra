use astra_turn_types::{UserIntentDelivery, UserIntentStatus};
use async_trait::async_trait;
use serde_json::Value;

/// Run status for cross-pod control polling.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RunControlStatus {
    Cancelled,
    Paused,
}

#[derive(Clone, Debug, PartialEq)]
pub struct QueuedUserIntent {
    /// Stable identity minted by the accepting client and preserved through
    /// durable delivery, application, replay, and transcript projection.
    pub intent_id: String,
    pub delivery: UserIntentDelivery,
    pub status: UserIntentStatus,
    pub event_index: usize,
    pub input: Value,
}

/// Normalize the user-facing content carried by a user-intent payload. This is
/// shared by prompt injection and every applied-event surface so CLI and
/// server modes report the same bytes for the same accepted input.
pub fn user_intent_content(input: &Value) -> Option<String> {
    fn trimmed_text(value: Option<&Value>) -> Option<String> {
        value
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|text| !text.is_empty())
            .map(ToString::to_string)
    }

    fn active_skills_text(input: &Value) -> Option<String> {
        let skills = input
            .get("active_skills")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .map(str::trim)
            .filter(|skill| !skill.is_empty())
            .collect::<Vec<_>>();
        (!skills.is_empty()).then(|| format!("Requested active skills: {}.", skills.join(", ")))
    }

    if let Some(text) = trimmed_text(Some(input)) {
        return Some(text);
    }

    let content = trimmed_text(input.get("content"));
    let text = trimmed_text(input.get("text"));
    let active_skills = active_skills_text(input);
    match (content.or(text), active_skills) {
        (Some(content), Some(active_skills)) => Some(format!("{active_skills}\n{content}")),
        (Some(content), None) => Some(content),
        (None, Some(active_skills)) => Some(active_skills),
        (None, None) => None,
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct UserIntentPoll {
    pub next_cursor: usize,
    pub inputs: Vec<QueuedUserIntent>,
    /// Malformed durable events skipped during this scan. A corrupt record is
    /// observable evidence, but never a reason to block later valid intents.
    pub issues: Vec<UserIntentPollIssue>,
    pub error: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UserIntentPollIssueKind {
    MissingData,
    MissingIntentId,
    MissingDelivery,
    InvalidDelivery,
    MissingInput,
    NoActionableContent,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UserIntentPollIssue {
    pub event_index: usize,
    pub intent_id: Option<String>,
    pub kind: UserIntentPollIssueKind,
}

/// Durable outcome of acknowledging locally staged user intent application.
/// A terminal run is not equivalent to an applied intent: callers must not
/// publish an `applied` event when the terminal transition won the race.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UserIntentApplyAck {
    Applied,
    RunTerminal,
}

/// Polls the database for the authoritative run status, enabling cross-pod
/// cancel/pause control without sticky sessions.
#[async_trait]
pub trait RunStatusProvider: Send + Sync {
    /// Returns `Some(Cancelled)`, `Some(Paused)`, or `None` if the run is
    /// still active (or doesn't exist). Transient lookup failures must be
    /// surfaced so callers do not confuse control-plane unavailability with a
    /// durable cancel/pause signal.
    async fn control_status(
        &self,
        user_id: &str,
        run_id: &str,
    ) -> Result<Option<RunControlStatus>, String>;
}

/// Polls durable user intents accepted while a run is executing.
#[async_trait]
pub trait UserIntentProvider: Send + Sync {
    /// Poll `user_intent` events appended to a durable run after the
    /// provided exclusive cursor.
    async fn poll_user_intents(
        &self,
        user_id: &str,
        run_id: &str,
        after_event_index: usize,
    ) -> UserIntentPoll;

    /// Mark accepted intents as applied to the next model boundary.
    async fn mark_user_intents_applied(
        &self,
        user_id: &str,
        run_id: &str,
        event_indices: &[usize],
    ) -> Result<UserIntentApplyAck, String>;
}

/// Full run-control surface required by the agentic loop.
///
/// This is intentionally a composition of the status and input traits instead
/// of a trait with optional no-op methods. Implementors must explicitly provide
/// both halves, so a missing user-intent implementation fails at compile time.
pub trait RunControlProvider: RunStatusProvider + UserIntentProvider {}

impl<T> RunControlProvider for T where T: RunStatusProvider + UserIntentProvider + Send + Sync {}
