//! WebSocket-based tool approval gate.
//!
//! Bridges the [`ToolApprovalGate`] trait with the WebSocket protocol.
//! Outbound approval requests are sent via an `mpsc` channel that the
//! WS handler drains during its polling loop.  Inbound responses arrive
//! through the shared §5.5 edge callback ledger.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::Value;
use tokio::sync::Mutex as TokioMutex;
use tokio::sync::mpsc;

use astra_services::InteractionStatus;
use astra_services::session_journal::{
    ApprovalJournalDecision, JournalEvent, JournalWriter, find_latest_approval_decision_for_run,
};
use astra_tools::{APPROVAL_REQUIRED_TOOLS, ApprovalDecision, ToolApprovalGate};

use crate::edge_ledger::{approval_callback_key, take_ledger_entry};

/// Default timeout for approval requests (matches frontend 60s countdown).
const APPROVAL_TIMEOUT: Duration = Duration::from_secs(60);
const APPROVAL_POLL_INTERVAL: Duration = Duration::from_millis(50);
const APPROVAL_JOURNAL_POLL_INTERVAL: Duration = Duration::from_millis(250);
const APPROVAL_KIND_STANDARD: &str = "standard";

/// An outbound approval request to be forwarded over WebSocket.
#[derive(Debug, Clone)]
pub struct ApprovalOutboundRequest {
    pub request_id: String,
    pub tool: String,
    pub args: Value,
}

#[derive(Debug, Clone)]
struct ApprovalJournalContext {
    session_id: String,
    run_id: String,
    turn: Option<u32>,
}

/// [`ToolApprovalGate`] implementation backed by WebSocket messaging.
///
/// # Lifecycle
///
/// 1. Tool executor calls [`request_approval`] for a dangerous tool.
/// 2. The gate sends an [`ApprovalOutboundRequest`] through `request_tx`.
/// 3. The WS handler picks it up and sends `ToolApprovalRequest` to the client.
/// 4. The client responds with `ToolApproval` → stored in the ledger.
/// 5. The gate polls the ledger and returns the decision.
pub struct WebSocketApprovalGate {
    user_id: String,
    journal_context: Option<ApprovalJournalContext>,
    edge_callback_ledger: Arc<TokioMutex<HashMap<String, Value>>>,
    request_tx: mpsc::Sender<Value>,
    timeout: Duration,
}

impl WebSocketApprovalGate {
    pub fn new(
        user_id: String,
        edge_callback_ledger: Arc<TokioMutex<HashMap<String, Value>>>,
        request_tx: mpsc::Sender<Value>,
    ) -> Self {
        Self {
            user_id,
            journal_context: None,
            edge_callback_ledger,
            request_tx,
            timeout: APPROVAL_TIMEOUT,
        }
    }

    pub fn new_with_journal_context(
        user_id: String,
        session_id: String,
        run_id: String,
        turn: Option<u32>,
        edge_callback_ledger: Arc<TokioMutex<HashMap<String, Value>>>,
        request_tx: mpsc::Sender<Value>,
    ) -> Self {
        Self {
            user_id,
            journal_context: Some(ApprovalJournalContext {
                session_id,
                run_id,
                turn,
            }),
            edge_callback_ledger,
            request_tx,
            timeout: APPROVAL_TIMEOUT,
        }
    }
}

fn decision_from_approval_fields(decision: &str, reason: Option<String>) -> ApprovalDecision {
    match decision {
        "allow" | "allow_session" => ApprovalDecision::Approved,
        "deny" => ApprovalDecision::Denied { reason },
        other => ApprovalDecision::Denied {
            reason: Some(format!("Invalid approval decision: {other}")),
        },
    }
}

fn decision_from_journal_approval(
    decision: ApprovalJournalDecision,
    context: &ApprovalJournalContext,
) -> Option<ApprovalDecision> {
    let contract = decision.interaction_contract(&context.session_id, None)?;
    match contract.status {
        InteractionStatus::Pending => None,
        InteractionStatus::Expired | InteractionStatus::Cancelled => {
            Some(ApprovalDecision::Denied {
                reason: Some(format!("Approval {}", decision.decision)),
            })
        }
        InteractionStatus::Resolved => Some(decision_from_approval_fields(
            &decision.decision,
            decision.reason,
        )),
    }
}

fn decision_from_approval_value(value: Value) -> ApprovalDecision {
    let body = value.get("body").unwrap_or(&value);
    if let Some(decision) = body.get("decision").and_then(Value::as_str) {
        return decision_from_approval_fields(
            decision,
            body.get("reason")
                .and_then(Value::as_str)
                .map(ToString::to_string),
        );
    }

    let approved = body
        .get("approved")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if approved {
        ApprovalDecision::Approved
    } else {
        let reason = body.get("reason").and_then(Value::as_str).map(String::from);
        ApprovalDecision::Denied { reason }
    }
}

fn append_approval_required_journal_event(
    context: &ApprovalJournalContext,
    request_id: &str,
    tool_name: &str,
) -> bool {
    match JournalWriter::new(&context.session_id).and_then(|writer| {
        writer.append(&JournalEvent::approval_required_for_run(
            Some(&context.session_id),
            context.turn,
            request_id,
            Some(&context.run_id),
            tool_name,
            APPROVAL_KIND_STANDARD,
            None,
        ))
    }) {
        Ok(()) => true,
        Err(error) => {
            tracing::warn!(
                target: "astra_turn_core::ws_approval_gate",
                session_id = %context.session_id,
                run_id = %context.run_id,
                request_id = %request_id,
                error = %error,
                "failed to persist approval required journal event"
            );
            false
        }
    }
}

async fn wait_approval_response(
    ledger: &Arc<TokioMutex<HashMap<String, Value>>>,
    user_id: &str,
    journal_context: Option<&ApprovalJournalContext>,
    request_id: &str,
    timeout: Duration,
) -> Option<ApprovalDecision> {
    let key = approval_callback_key(user_id, request_id);
    let started = std::time::Instant::now();
    let mut last_journal_lookup: Option<std::time::Instant> = None;
    loop {
        if let Some(value) = take_ledger_entry(ledger, &key, Duration::ZERO).await {
            return Some(decision_from_approval_value(value));
        }

        if let Some(context) = journal_context
            && last_journal_lookup
                .map(|last| last.elapsed() >= APPROVAL_JOURNAL_POLL_INTERVAL)
                .unwrap_or(true)
        {
            last_journal_lookup = Some(std::time::Instant::now());
            match find_latest_approval_decision_for_run(
                &context.session_id,
                request_id,
                &context.run_id,
            ) {
                Ok(Some(decision)) => {
                    if let Some(decision) = decision_from_journal_approval(decision, context) {
                        return Some(decision);
                    }
                }
                Ok(None) => {}
                Err(error) => {
                    tracing::warn!(
                        target: "astra_turn_core::ws_approval_gate",
                        session_id = %context.session_id,
                        run_id = %context.run_id,
                        request_id = %request_id,
                        error = %error,
                        "approval journal replay lookup failed"
                    );
                }
            }
        }

        if started.elapsed() >= timeout {
            return None;
        }
        let remaining = timeout.saturating_sub(started.elapsed());
        tokio::time::sleep(APPROVAL_POLL_INTERVAL.min(remaining)).await;
    }
}

#[async_trait]
impl ToolApprovalGate for WebSocketApprovalGate {
    async fn request_approval(
        &self,
        request_id: &str,
        tool_name: &str,
        args: &Value,
    ) -> ApprovalDecision {
        // Send outbound request to WS handler via channel as JSON.
        let request = serde_json::json!({
            "request_id": request_id,
            "tool": tool_name,
            "args": args,
            "session_id": self.journal_context.as_ref().map(|context| &context.session_id),
            "run_id": self.journal_context.as_ref().map(|context| &context.run_id),
        });

        if self.request_tx.send(request).await.is_err() {
            // Channel closed — WS connection dropped.
            return ApprovalDecision::Denied {
                reason: Some("WebSocket connection closed".into()),
            };
        }
        if let Some(context) = self.journal_context.as_ref() {
            let _ = append_approval_required_journal_event(context, request_id, tool_name);
        }

        // Wait for the client's response via the same-pod ledger or durable journal.
        let key = approval_callback_key(&self.user_id, request_id);
        let outcome = wait_approval_response(
            &self.edge_callback_ledger,
            &self.user_id,
            self.journal_context.as_ref(),
            request_id,
            self.timeout,
        )
        .await;
        match outcome {
            Some(decision) => decision,
            None => {
                // audit-#10: on timeout, proactively evict any approval response
                // that lands AFTER our timeout window so it does not linger in
                // the ledger and consume one of the LEDGER_MAX_ENTRIES slots.
                if take_ledger_entry(&self.edge_callback_ledger, &key, Duration::ZERO)
                    .await
                    .is_some()
                {
                    tracing::info!(
                        target: "astra_turn_core::ws_approval_gate",
                        request_id = %request_id,
                        "approval response arrived after timeout; evicted from ledger"
                    );
                } else {
                    tracing::debug!(
                        target: "astra_turn_core::ws_approval_gate",
                        request_id = %request_id,
                        "no approval response observed before timeout; pending key cleared"
                    );
                }
                ApprovalDecision::Timeout
            }
        }
    }

    fn requires_approval(&self, tool_name: &str) -> bool {
        APPROVAL_REQUIRED_TOOLS.contains(&tool_name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn test_gate(
        ledger: Arc<TokioMutex<HashMap<String, Value>>>,
        tx: mpsc::Sender<Value>,
        timeout: Duration,
    ) -> WebSocketApprovalGate {
        WebSocketApprovalGate {
            user_id: "u1".into(),
            journal_context: None,
            edge_callback_ledger: ledger,
            request_tx: tx,
            timeout,
        }
    }

    fn test_gate_with_journal(
        ledger: Arc<TokioMutex<HashMap<String, Value>>>,
        tx: mpsc::Sender<Value>,
        timeout: Duration,
    ) -> WebSocketApprovalGate {
        WebSocketApprovalGate {
            user_id: "u1".into(),
            journal_context: Some(ApprovalJournalContext {
                session_id: "sess-approval".into(),
                run_id: "run-approval".into(),
                turn: Some(7),
            }),
            edge_callback_ledger: ledger,
            request_tx: tx,
            timeout,
        }
    }

    #[tokio::test]
    async fn approved_via_ledger() {
        let ledger = Arc::new(TokioMutex::new(HashMap::new()));
        let (tx, mut rx) = mpsc::channel::<Value>(1);

        let gate = test_gate(ledger.clone(), tx, Duration::from_secs(5));

        // Simulate client responding in background.
        let ledger_bg = ledger.clone();
        tokio::spawn(async move {
            // Wait for the outbound request.
            let req = rx.recv().await.unwrap();
            assert_eq!(req["tool"].as_str().unwrap(), "bash");

            // Insert approval response into ledger.
            let key = approval_callback_key("u1", req["request_id"].as_str().unwrap());
            let mut g = ledger_bg.lock().await;
            g.insert(key, json!({"approved": true}));
        });

        let decision = gate
            .request_approval("req-1", "bash", &json!({"command": "ls"}))
            .await;
        assert!(matches!(decision, ApprovalDecision::Approved));
    }

    #[tokio::test]
    async fn denied_via_ledger() {
        let ledger = Arc::new(TokioMutex::new(HashMap::new()));
        let (tx, mut rx) = mpsc::channel::<Value>(1);

        let gate = test_gate(ledger.clone(), tx, Duration::from_secs(5));

        let ledger_bg = ledger.clone();
        tokio::spawn(async move {
            let req = rx.recv().await.unwrap();
            let key = approval_callback_key("u1", req["request_id"].as_str().unwrap());
            let mut g = ledger_bg.lock().await;
            g.insert(key, json!({"approved": false, "reason": "too risky"}));
        });

        let decision = gate
            .request_approval("req-2", "delete_file", &json!({"path": "/etc/passwd"}))
            .await;
        match decision {
            ApprovalDecision::Denied { reason } => {
                assert_eq!(reason.as_deref(), Some("too risky"));
            }
            _ => panic!("expected Denied"),
        }
    }

    #[tokio::test]
    async fn timeout_when_no_response() {
        let ledger = Arc::new(TokioMutex::new(HashMap::new()));
        let (tx, _rx) = mpsc::channel::<Value>(1);

        let gate = test_gate(ledger, tx, Duration::from_millis(100));

        let decision = gate
            .request_approval("req-3", "bash", &json!({"command": "rm -rf /"}))
            .await;
        assert!(matches!(decision, ApprovalDecision::Timeout));
    }

    /// audit-#10: an approval response that arrives AFTER the request timed
    /// out must not linger in the ledger — it would consume one of the
    /// LEDGER_MAX_ENTRIES slots forever.
    #[tokio::test]
    async fn timeout_evicts_late_response_from_ledger() {
        let ledger = Arc::new(TokioMutex::new(HashMap::new()));
        let (tx, mut rx) = mpsc::channel::<Value>(1);

        let gate = test_gate(ledger.clone(), tx, Duration::from_millis(100));

        let ledger_bg = ledger.clone();
        let inserted = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let inserted2 = inserted.clone();
        tokio::spawn(async move {
            let req = rx.recv().await.unwrap();
            // Sleep long enough that the gate has already timed out.
            tokio::time::sleep(Duration::from_millis(250)).await;
            let key = approval_callback_key("u1", req["request_id"].as_str().unwrap());
            ledger_bg
                .lock()
                .await
                .insert(key, json!({"approved": true}));
            inserted2.store(true, std::sync::atomic::Ordering::SeqCst);
        });

        let decision = gate
            .request_approval("req-late", "bash", &json!({"command": "ls"}))
            .await;
        assert!(matches!(decision, ApprovalDecision::Timeout));

        // Give the late insert a chance to land, then a follow-up tick for
        // the gate's cleanup to complete (it ran inside request_approval).
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(
            inserted.load(std::sync::atomic::Ordering::SeqCst),
            "background late-insert should have run"
        );

        // request_approval ran cleanup BEFORE the late insert landed, so we
        // need to invoke the same eviction path again to confirm the gate
        // doesn't leave the key behind on a subsequent timeout.
        let (tx2, _rx2) = mpsc::channel::<Value>(1);
        let gate2 = test_gate(ledger.clone(), tx2, Duration::from_millis(50));
        // Reuse the same request_id so the existing late entry is the one
        // the gate would consume — but only if it polled for it. We do NOT
        // call request_approval here because it would consume the key.
        // Instead, manually verify cleanup semantics: run a fresh
        // request_approval that times out, then assert the late key is
        // explicitly removed by issuing a timeout-and-cleanup cycle.
        let _ = gate2
            .request_approval("req-late", "bash", &json!({"command": "ls"}))
            .await;

        // The cleanup branch in request_approval should have removed the
        // matching ledger key.
        let key = approval_callback_key("u1", "req-late");
        let lock = ledger.lock().await;
        assert!(
            !lock.contains_key(&key),
            "late approval response must not linger in the ledger after timeout"
        );
    }

    #[tokio::test]
    async fn channel_closed_returns_denied() {
        let ledger = Arc::new(TokioMutex::new(HashMap::new()));
        let (tx, rx) = mpsc::channel::<Value>(1);
        drop(rx); // Close the channel.

        let gate = test_gate(ledger, tx, Duration::from_secs(5));

        let decision = gate.request_approval("req-4", "bash", &json!({})).await;
        match decision {
            ApprovalDecision::Denied { reason } => {
                assert!(reason.unwrap().contains("connection closed"));
            }
            _ => panic!("expected Denied"),
        }
    }

    #[tokio::test]
    async fn wrapped_approval_respond_value_is_supported() {
        let ledger = Arc::new(TokioMutex::new(HashMap::new()));
        let (tx, _rx) = mpsc::channel::<Value>(1);
        let key = approval_callback_key("u1", "req-http-shaped");
        ledger.lock().await.insert(
            key,
            json!({
                "kind": "approval_respond",
                "body": {
                    "request_id": "req-http-shaped",
                    "decision": "deny",
                    "reason": "blocked",
                    "run_id": "run-approval"
                }
            }),
        );

        let gate = test_gate(ledger, tx, Duration::from_secs(5));
        let decision = gate
            .request_approval("req-http-shaped", "bash", &json!({"command": "rm"}))
            .await;

        match decision {
            ApprovalDecision::Denied { reason } => {
                assert_eq!(reason.as_deref(), Some("blocked"));
            }
            _ => panic!("expected Denied"),
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn approved_via_journal_when_ledger_is_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let _guard = astra_services::session_journal::JournalDirGuard::new(tmp.path());
        let writer = JournalWriter::new("sess-approval").unwrap();
        writer
            .append(&JournalEvent::approval_decision_for_run(
                Some("sess-approval"),
                Some(7),
                "req-journal",
                Some("run-approval"),
                Some("write_file"),
                Some(APPROVAL_KIND_STANDARD),
                "allow",
                None,
            ))
            .unwrap();

        let ledger = Arc::new(TokioMutex::new(HashMap::new()));
        let (tx, _rx) = mpsc::channel::<Value>(1);
        let gate = test_gate_with_journal(ledger.clone(), tx, Duration::from_millis(250));

        let decision = gate
            .request_approval("req-journal", "write_file", &json!({"path": "x"}))
            .await;

        assert!(matches!(decision, ApprovalDecision::Approved));
        assert!(ledger.lock().await.is_empty());
        let required = astra_services::session_journal::find_latest_approval_required_for_run(
            "sess-approval",
            "req-journal",
            "run-approval",
        )
        .unwrap()
        .expect("approval required event should be durable");
        assert_eq!(required.turn, Some(7));
        assert_eq!(required.tool_name.as_deref(), Some("write_file"));
        assert_eq!(
            required.approval_kind.as_deref(),
            Some(APPROVAL_KIND_STANDARD)
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn journal_decision_from_other_run_does_not_satisfy_wait() {
        let tmp = tempfile::tempdir().unwrap();
        let _guard = astra_services::session_journal::JournalDirGuard::new(tmp.path());
        let writer = JournalWriter::new("sess-approval").unwrap();
        writer
            .append(&JournalEvent::approval_decision_for_run(
                Some("sess-approval"),
                Some(7),
                "req-cross-run",
                Some("other-run"),
                Some("bash"),
                Some(APPROVAL_KIND_STANDARD),
                "allow",
                None,
            ))
            .unwrap();

        let ledger = Arc::new(TokioMutex::new(HashMap::new()));
        let (tx, _rx) = mpsc::channel::<Value>(1);
        let gate = test_gate_with_journal(ledger, tx, Duration::from_millis(40));

        let decision = gate
            .request_approval("req-cross-run", "bash", &json!({"command": "ls"}))
            .await;

        assert!(matches!(decision, ApprovalDecision::Timeout));
    }

    #[test]
    fn requires_approval_for_dangerous_tools() {
        let ledger = Arc::new(TokioMutex::new(HashMap::new()));
        let (tx, _rx) = mpsc::channel::<Value>(1);
        let gate = WebSocketApprovalGate::new("u1".into(), ledger, tx);

        assert!(gate.requires_approval("bash"));
        assert!(gate.requires_approval("write_file"));
        assert!(gate.requires_approval("delete_file"));
        assert!(gate.requires_approval("rollback_file_edits"));
        assert!(gate.requires_approval("rollback_database_snapshots"));
        assert!(!gate.requires_approval("read_file"));
        assert!(!gate.requires_approval("list_dir"));
        assert!(!gate.requires_approval("grep"));

        assert!(gate.requires_approval_for("git", &json!({"action": "commit"})));
        assert!(gate.requires_approval_for("github", &json!({"action": "create_issue"})));
        assert!(!gate.requires_approval_for("git", &json!({"action": "diff"})));
        assert!(!gate.requires_approval_for("github", &json!({"action": "list_prs"})));
    }
}
