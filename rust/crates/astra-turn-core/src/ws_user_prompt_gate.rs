//! WebSocket-based ask_user gate.
//!
//! Bridges the [`astra_tools::AskUserGate`] trait with the WebSocket protocol.
//! Outbound prompt requests are sent via an `mpsc` channel that the WS handler
//! drains during its polling loop. Inbound responses arrive through the shared
//! §5.5 edge callback ledger.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use astra_tools::{AskUserDecision, AskUserGate, AskUserResponse};
use async_trait::async_trait;
use serde_json::Value;
use tokio::sync::Mutex as TokioMutex;
use tokio::sync::mpsc;

use crate::edge_ledger::{take_ledger_entry, user_prompt_callback_key};

/// Default timeout for ask_user prompts (matches frontend 60s countdown).
const USER_PROMPT_TIMEOUT: Duration = Duration::from_secs(60);

/// An outbound ask_user request to be forwarded over WebSocket.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserPromptOutboundRequest {
    pub request_id: String,
    pub question: String,
    pub choices: Vec<String>,
    pub default: Option<String>,
    pub context: Option<String>,
}

/// [`AskUserGate`] implementation backed by WebSocket messaging.
pub struct WebSocketUserPromptGate {
    user_id: String,
    edge_callback_ledger: Arc<TokioMutex<HashMap<String, Value>>>,
    request_tx: mpsc::UnboundedSender<Value>,
    timeout: Duration,
}

impl WebSocketUserPromptGate {
    pub fn new(
        user_id: String,
        edge_callback_ledger: Arc<TokioMutex<HashMap<String, Value>>>,
        request_tx: mpsc::UnboundedSender<Value>,
    ) -> Self {
        Self {
            user_id,
            edge_callback_ledger,
            request_tx,
            timeout: USER_PROMPT_TIMEOUT,
        }
    }
}

#[async_trait]
impl AskUserGate for WebSocketUserPromptGate {
    async fn request_user_input(
        &self,
        request_id: &str,
        question: &str,
        choices: &[String],
        default: Option<&str>,
        context: Option<&str>,
    ) -> AskUserDecision {
        let request = serde_json::json!({
            "request_id": request_id,
            "question": question,
            "choices": choices,
            "default": default,
            "context": context,
        });

        if self.request_tx.send(request).is_err() {
            return AskUserDecision::Error("WebSocket connection closed".into());
        }

        let key = user_prompt_callback_key(&self.user_id, request_id);
        match take_ledger_entry(&self.edge_callback_ledger, &key, self.timeout).await {
            Some(value) => {
                let Some(answer) = value
                    .get("answer")
                    .and_then(Value::as_str)
                    .map(ToString::to_string)
                else {
                    return AskUserDecision::Error(
                        "Invalid user prompt response: missing answer".into(),
                    );
                };
                AskUserDecision::Answer(AskUserResponse {
                    answer,
                    was_custom: value
                        .get("was_custom")
                        .and_then(Value::as_bool)
                        .unwrap_or(false),
                })
            }
            None => AskUserDecision::Timeout,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[tokio::test]
    async fn answered_via_ledger() {
        let ledger = Arc::new(TokioMutex::new(HashMap::new()));
        let (tx, mut rx) = mpsc::unbounded_channel();

        let gate = WebSocketUserPromptGate {
            user_id: "u1".into(),
            edge_callback_ledger: ledger.clone(),
            request_tx: tx,
            timeout: Duration::from_secs(5),
        };

        let ledger_bg = ledger.clone();
        tokio::spawn(async move {
            let req = rx.recv().await.unwrap();
            assert_eq!(req["question"].as_str().unwrap(), "Continue?");
            assert_eq!(req["choices"].as_array().unwrap().len(), 2);

            let key = user_prompt_callback_key("u1", req["request_id"].as_str().unwrap());
            let mut g = ledger_bg.lock().await;
            g.insert(key, json!({"answer": "yes", "was_custom": false}));
        });

        let decision = gate
            .request_user_input(
                "req-1",
                "Continue?",
                &["yes".to_string(), "no".to_string()],
                Some("yes"),
                Some("Pick one"),
            )
            .await;
        assert_eq!(
            decision,
            AskUserDecision::Answer(AskUserResponse {
                answer: "yes".into(),
                was_custom: false,
            })
        );
    }

    #[tokio::test]
    async fn timeout_when_no_response() {
        let ledger = Arc::new(TokioMutex::new(HashMap::new()));
        let (tx, _rx) = mpsc::unbounded_channel();

        let gate = WebSocketUserPromptGate {
            user_id: "u1".into(),
            edge_callback_ledger: ledger,
            request_tx: tx,
            timeout: Duration::from_millis(100),
        };

        let decision = gate
            .request_user_input("req-2", "Continue?", &[], None, None)
            .await;
        assert_eq!(decision, AskUserDecision::Timeout);
    }

    #[tokio::test]
    async fn channel_closed_returns_error() {
        let ledger = Arc::new(TokioMutex::new(HashMap::new()));
        let (tx, rx) = mpsc::unbounded_channel();
        drop(rx);

        let gate = WebSocketUserPromptGate {
            user_id: "u1".into(),
            edge_callback_ledger: ledger,
            request_tx: tx,
            timeout: Duration::from_secs(5),
        };

        let decision = gate
            .request_user_input("req-3", "Continue?", &[], None, None)
            .await;
        assert_eq!(
            decision,
            AskUserDecision::Error("WebSocket connection closed".into())
        );
    }
}
