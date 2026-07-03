//! Shared semantic contract for run-blocking interaction results.
//!
//! This module intentionally does not introduce a unified storage table.
//! Approval and ask_user facts currently live in the session journal, while
//! edge tool facts live in `edge_pending_dispatch`. The contract below defines
//! the common identity and terminal-state semantics those stores must preserve
//! so no-sticky deployments can reason about them uniformly.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InteractionKind {
    Approval,
    UserPrompt,
    EdgeTool,
}

impl InteractionKind {
    pub fn durable_store(self) -> InteractionDurableStore {
        match self {
            Self::Approval | Self::UserPrompt => InteractionDurableStore::SessionJournal,
            Self::EdgeTool => InteractionDurableStore::EdgePendingDispatch,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Approval => "approval",
            Self::UserPrompt => "user_prompt",
            Self::EdgeTool => "edge_tool",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InteractionDurableStore {
    SessionJournal,
    EdgePendingDispatch,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InteractionStatus {
    /// A request exists but no terminal decision/result has been accepted.
    Pending,
    /// A terminal decision/result exists. Rejections and tool failures still
    /// resolve the wait because they are durable results for the run.
    Resolved,
    /// The request expired before a usable result arrived.
    Expired,
    /// The run or requester cancelled the interaction before resolution.
    Cancelled,
}

impl InteractionStatus {
    pub fn is_terminal(self) -> bool {
        !matches!(self, Self::Pending)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InteractionIdentity {
    /// Some current stores encode the owner by path or table boundary rather
    /// than as a payload field. New durable coordination stores must set this.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_id: Option<String>,
    pub session_id: String,
    pub run_id: String,
    pub request_id: String,
}

impl InteractionIdentity {
    pub fn new(
        user_id: Option<impl Into<String>>,
        session_id: impl Into<String>,
        run_id: impl Into<String>,
        request_id: impl Into<String>,
    ) -> Self {
        Self {
            user_id: user_id.map(Into::into),
            session_id: session_id.into(),
            run_id: run_id.into(),
            request_id: request_id.into(),
        }
    }

    pub fn is_run_scoped(&self) -> bool {
        !self.session_id.trim().is_empty()
            && !self.run_id.trim().is_empty()
            && !self.request_id.trim().is_empty()
    }

    pub fn idempotency_key(&self, kind: InteractionKind) -> String {
        match self.user_id.as_deref() {
            Some(user_id) if !user_id.trim().is_empty() => format!(
                "{}:{user_id}:{}:{}:{}",
                kind.as_str(),
                self.session_id,
                self.run_id,
                self.request_id
            ),
            _ => format!(
                "{}:{}:{}:{}",
                kind.as_str(),
                self.session_id,
                self.run_id,
                self.request_id
            ),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InteractionContract {
    pub kind: InteractionKind,
    pub durable_store: InteractionDurableStore,
    pub identity: InteractionIdentity,
    pub status: InteractionStatus,
    /// Inline result payload, content hash, table row id, or journal event ref.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result_ref: Option<String>,
}

impl InteractionContract {
    pub fn new(
        kind: InteractionKind,
        identity: InteractionIdentity,
        status: InteractionStatus,
        result_ref: Option<String>,
    ) -> Self {
        Self {
            kind,
            durable_store: kind.durable_store(),
            identity,
            status,
            result_ref,
        }
    }

    pub fn is_wait_satisfied(&self) -> bool {
        self.status.is_terminal()
    }
}

pub fn approval_decision_status(decision: &str) -> InteractionStatus {
    match normalize_status(decision).as_str() {
        "timeout" | "timed_out" | "expired" => InteractionStatus::Expired,
        "cancelled" | "canceled" => InteractionStatus::Cancelled,
        _ => InteractionStatus::Resolved,
    }
}

pub fn ask_user_response_status(status: &str) -> InteractionStatus {
    match normalize_status(status).as_str() {
        "pending" | "prompted" | "waiting" => InteractionStatus::Pending,
        "timeout" | "timed_out" | "expired" => InteractionStatus::Expired,
        "cancelled" | "canceled" => InteractionStatus::Cancelled,
        _ => InteractionStatus::Resolved,
    }
}

pub fn edge_dispatch_status(status: &str, result_json: Option<&str>) -> InteractionStatus {
    match normalize_status(status).as_str() {
        "pending" | "dispatched" => InteractionStatus::Pending,
        "completed" => InteractionStatus::Resolved,
        "cancelled" | "canceled" => InteractionStatus::Cancelled,
        "expired" => InteractionStatus::Expired,
        "failed" => edge_failed_status(result_json),
        _ => InteractionStatus::Resolved,
    }
}

fn edge_failed_status(result_json: Option<&str>) -> InteractionStatus {
    let Some(result_json) = result_json else {
        return InteractionStatus::Resolved;
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(result_json) else {
        return InteractionStatus::Resolved;
    };
    let output = value
        .get("output")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default();
    match normalize_status(output).as_str() {
        text if text.contains("cancelled") || text.contains("canceled") => {
            InteractionStatus::Cancelled
        }
        text if text.contains("expired") || text.contains("timeout") => InteractionStatus::Expired,
        _ => InteractionStatus::Resolved,
    }
}

fn normalize_status(value: &str) -> String {
    value.trim().to_ascii_lowercase().replace('-', "_")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durable_store_is_part_of_kind_contract() {
        assert_eq!(
            InteractionKind::Approval.durable_store(),
            InteractionDurableStore::SessionJournal
        );
        assert_eq!(
            InteractionKind::UserPrompt.durable_store(),
            InteractionDurableStore::SessionJournal
        );
        assert_eq!(
            InteractionKind::EdgeTool.durable_store(),
            InteractionDurableStore::EdgePendingDispatch
        );
    }

    #[test]
    fn identity_requires_session_run_and_request_scope() {
        let scoped = InteractionIdentity::new(Some("u1"), "s1", "r1", "req1");
        assert!(scoped.is_run_scoped());
        assert_eq!(
            scoped.idempotency_key(InteractionKind::Approval),
            "approval:u1:s1:r1:req1"
        );

        let legacy_journal_scope = InteractionIdentity::new(None::<String>, "s1", "r1", "req1");
        assert!(legacy_journal_scope.is_run_scoped());
        assert_eq!(
            legacy_journal_scope.idempotency_key(InteractionKind::UserPrompt),
            "user_prompt:s1:r1:req1"
        );

        let missing_run = InteractionIdentity::new(Some("u1"), "s1", "", "req1");
        assert!(!missing_run.is_run_scoped());
    }

    #[test]
    fn approval_and_user_prompt_statuses_map_to_common_contract() {
        assert_eq!(
            approval_decision_status("approved"),
            InteractionStatus::Resolved
        );
        assert_eq!(
            approval_decision_status("rejected"),
            InteractionStatus::Resolved
        );
        assert_eq!(
            approval_decision_status("timed-out"),
            InteractionStatus::Expired
        );
        assert_eq!(
            ask_user_response_status("submitted"),
            InteractionStatus::Resolved
        );
        assert_eq!(
            ask_user_response_status("cancelled"),
            InteractionStatus::Cancelled
        );
        assert_eq!(
            ask_user_response_status("waiting"),
            InteractionStatus::Pending
        );
    }

    #[test]
    fn edge_dispatch_storage_status_maps_to_semantic_status() {
        assert_eq!(
            edge_dispatch_status("pending", None),
            InteractionStatus::Pending
        );
        assert_eq!(
            edge_dispatch_status("dispatched", None),
            InteractionStatus::Pending
        );
        assert_eq!(
            edge_dispatch_status("completed", Some(r#"{"status":"ok"}"#)),
            InteractionStatus::Resolved
        );
        assert_eq!(
            edge_dispatch_status(
                "failed",
                Some(r#"{"status":"error","output":"edge dispatch expired"}"#)
            ),
            InteractionStatus::Expired
        );
        assert_eq!(
            edge_dispatch_status(
                "failed",
                Some(r#"{"status":"error","output":"edge dispatch cancelled"}"#)
            ),
            InteractionStatus::Cancelled
        );
        assert_eq!(
            edge_dispatch_status(
                "failed",
                Some(r#"{"status":"error","output":"edge executor failed"}"#)
            ),
            InteractionStatus::Resolved
        );
    }

    #[test]
    fn terminal_status_satisfies_wait_without_assuming_success() {
        let identity = InteractionIdentity::new(Some("u1"), "s1", "r1", "req1");
        let failed_edge_result = InteractionContract::new(
            InteractionKind::EdgeTool,
            identity,
            InteractionStatus::Resolved,
            Some("edge_pending_dispatch.result_json".to_string()),
        );

        assert!(failed_edge_result.is_wait_satisfied());
        assert_eq!(
            failed_edge_result.durable_store,
            InteractionDurableStore::EdgePendingDispatch
        );
    }
}
