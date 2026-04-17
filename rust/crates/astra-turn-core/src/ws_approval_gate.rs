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

use astra_tools::{APPROVAL_REQUIRED_TOOLS, ApprovalDecision, ToolApprovalGate};

use crate::edge_ledger::{approval_callback_key, take_ledger_entry};

/// Default timeout for approval requests (matches frontend 60s countdown).
const APPROVAL_TIMEOUT: Duration = Duration::from_secs(60);

/// An outbound approval request to be forwarded over WebSocket.
#[derive(Debug, Clone)]
pub struct ApprovalOutboundRequest {
    pub request_id: String,
    pub tool: String,
    pub args: Value,
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
    edge_callback_ledger: Arc<TokioMutex<HashMap<String, Value>>>,
    request_tx: mpsc::UnboundedSender<Value>,
    timeout: Duration,
}

impl WebSocketApprovalGate {
    pub fn new(
        user_id: String,
        edge_callback_ledger: Arc<TokioMutex<HashMap<String, Value>>>,
        request_tx: mpsc::UnboundedSender<Value>,
    ) -> Self {
        Self {
            user_id,
            edge_callback_ledger,
            request_tx,
            timeout: APPROVAL_TIMEOUT,
        }
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
        });

        if self.request_tx.send(request).is_err() {
            // Channel closed — WS connection dropped.
            return ApprovalDecision::Denied {
                reason: Some("WebSocket connection closed".into()),
            };
        }

        // Wait for the client's response via the shared ledger.
        let key = approval_callback_key(&self.user_id, request_id);
        match take_ledger_entry(&self.edge_callback_ledger, &key, self.timeout).await {
            Some(value) => {
                let approved = value
                    .get("approved")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                if approved {
                    ApprovalDecision::Approved
                } else {
                    let reason = value
                        .get("reason")
                        .and_then(|v| v.as_str())
                        .map(String::from);
                    ApprovalDecision::Denied { reason }
                }
            }
            None => ApprovalDecision::Timeout,
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

    #[tokio::test]
    async fn approved_via_ledger() {
        let ledger = Arc::new(TokioMutex::new(HashMap::new()));
        let (tx, mut rx) = mpsc::unbounded_channel();

        let gate = WebSocketApprovalGate {
            user_id: "u1".into(),
            edge_callback_ledger: ledger.clone(),
            request_tx: tx,
            timeout: Duration::from_secs(5),
        };

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
        let (tx, mut rx) = mpsc::unbounded_channel();

        let gate = WebSocketApprovalGate {
            user_id: "u1".into(),
            edge_callback_ledger: ledger.clone(),
            request_tx: tx,
            timeout: Duration::from_secs(5),
        };

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
        let (tx, _rx) = mpsc::unbounded_channel();

        let gate = WebSocketApprovalGate {
            user_id: "u1".into(),
            edge_callback_ledger: ledger,
            request_tx: tx,
            timeout: Duration::from_millis(100), // Very short for test.
        };

        let decision = gate
            .request_approval("req-3", "bash", &json!({"command": "rm -rf /"}))
            .await;
        assert!(matches!(decision, ApprovalDecision::Timeout));
    }

    #[tokio::test]
    async fn channel_closed_returns_denied() {
        let ledger = Arc::new(TokioMutex::new(HashMap::new()));
        let (tx, rx) = mpsc::unbounded_channel();
        drop(rx); // Close the channel.

        let gate = WebSocketApprovalGate {
            user_id: "u1".into(),
            edge_callback_ledger: ledger,
            request_tx: tx,
            timeout: Duration::from_secs(5),
        };

        let decision = gate.request_approval("req-4", "bash", &json!({})).await;
        match decision {
            ApprovalDecision::Denied { reason } => {
                assert!(reason.unwrap().contains("connection closed"));
            }
            _ => panic!("expected Denied"),
        }
    }

    #[test]
    fn requires_approval_for_dangerous_tools() {
        let ledger = Arc::new(TokioMutex::new(HashMap::new()));
        let (tx, _rx) = mpsc::unbounded_channel();
        let gate = WebSocketApprovalGate::new("u1".into(), ledger, tx);

        assert!(gate.requires_approval("bash"));
        assert!(gate.requires_approval("write_file"));
        assert!(gate.requires_approval("delete_file"));
        assert!(!gate.requires_approval("read_file"));
        assert!(!gate.requires_approval("list_dir"));
        assert!(!gate.requires_approval("grep"));
    }
}
