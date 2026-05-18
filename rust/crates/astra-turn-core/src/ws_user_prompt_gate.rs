//! WebSocket-based ask_user gate.
//!
//! Bridges the [`astra_tools::AskUserGate`] trait with the WebSocket protocol.
//! Outbound prompt requests are sent via an `mpsc` channel that the WS handler
//! drains during its polling loop. Inbound responses arrive through the shared
//! §5.5 edge callback ledger.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use astra_tools::{AskUserAnswers, AskUserDecision, AskUserGate, AskUserPrompt};
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
    pub prompt: AskUserPrompt,
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
    async fn request_questionnaire(
        &self,
        request_id: &str,
        prompt: &AskUserPrompt,
    ) -> AskUserDecision {
        let request = serde_json::json!({
            "request_id": request_id,
            "prompt": prompt,
        });

        if self.request_tx.send(request).is_err() {
            return AskUserDecision::Error("WebSocket connection closed".into());
        }

        let key = user_prompt_callback_key(&self.user_id, request_id);
        let timeout = prompt
            .timeout_ms
            .map(Duration::from_millis)
            .unwrap_or(self.timeout);
        match take_ledger_entry(&self.edge_callback_ledger, &key, timeout).await {
            Some(value) => {
                if value
                    .get("cancelled")
                    .and_then(Value::as_bool)
                    .unwrap_or(false)
                {
                    return AskUserDecision::Cancelled;
                }
                let Some(answers) = value.get("answers").cloned() else {
                    return AskUserDecision::Error(
                        "Invalid user prompt response: missing answers".into(),
                    );
                };
                match serde_json::from_value::<AskUserAnswers>(answers) {
                    Ok(answers) => AskUserDecision::Submitted(answers),
                    Err(error) => {
                        AskUserDecision::Error(format!("Invalid user prompt response: {error}"))
                    }
                }
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
            assert_eq!(
                req["prompt"]["questions"][0]["question"].as_str().unwrap(),
                "Continue?"
            );
            assert_eq!(req["prompt"]["questions"].as_array().unwrap().len(), 1);

            let key = user_prompt_callback_key("u1", req["request_id"].as_str().unwrap());
            let mut g = ledger_bg.lock().await;
            g.insert(
                key,
                json!({
                    "answers": {
                        "answers": [{
                            "question": "Continue?",
                            "answers": ["yes"],
                            "multi_select": false
                        }]
                    }
                }),
            );
        });

        let decision = gate
            .request_questionnaire(
                "req-1",
                &AskUserPrompt {
                    context: Some("Pick one".into()),
                    questions: vec![astra_tools::AskUserQuestion {
                        header: "Confirm".into(),
                        question: "Continue?".into(),
                        options: vec![
                            astra_tools::AskUserChoice {
                                label: "yes".into(),
                                description: None,
                                preview: None,
                            },
                            astra_tools::AskUserChoice {
                                label: "no".into(),
                                description: None,
                                preview: None,
                            },
                        ],
                        multi_select: false,
                        allow_freeform: false,
                    }],
                    timeout_ms: None,
                },
            )
            .await;
        assert_eq!(
            decision,
            AskUserDecision::Submitted(AskUserAnswers {
                answers: vec![astra_tools::AskUserQuestionAnswer {
                    question: "Continue?".into(),
                    answers: vec!["yes".into()],
                    multi_select: false,
                    annotation: None,
                }],
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
            .request_questionnaire(
                "req-2",
                &AskUserPrompt {
                    context: None,
                    questions: vec![astra_tools::AskUserQuestion {
                        header: "Confirm".into(),
                        question: "Continue?".into(),
                        options: vec![],
                        multi_select: false,
                        allow_freeform: true,
                    }],
                    timeout_ms: None,
                },
            )
            .await;
        assert_eq!(decision, AskUserDecision::Timeout);
    }

    #[tokio::test]
    async fn prompt_timeout_overrides_gate_default() {
        let ledger = Arc::new(TokioMutex::new(HashMap::new()));
        let (tx, _rx) = mpsc::unbounded_channel();

        let gate = WebSocketUserPromptGate {
            user_id: "u1".into(),
            edge_callback_ledger: ledger,
            request_tx: tx,
            timeout: Duration::from_secs(5),
        };

        let start = std::time::Instant::now();
        let decision = gate
            .request_questionnaire(
                "req-timeout",
                &AskUserPrompt {
                    context: None,
                    questions: vec![astra_tools::AskUserQuestion {
                        header: "Confirm".into(),
                        question: "Continue?".into(),
                        options: vec![],
                        multi_select: false,
                        allow_freeform: true,
                    }],
                    timeout_ms: Some(10),
                },
            )
            .await;
        assert_eq!(decision, AskUserDecision::Timeout);
        assert!(start.elapsed() < Duration::from_secs(1));
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
            .request_questionnaire(
                "req-3",
                &AskUserPrompt {
                    context: None,
                    questions: vec![astra_tools::AskUserQuestion {
                        header: "Confirm".into(),
                        question: "Continue?".into(),
                        options: vec![],
                        multi_select: false,
                        allow_freeform: true,
                    }],
                    timeout_ms: None,
                },
            )
            .await;
        assert_eq!(
            decision,
            AskUserDecision::Error("WebSocket connection closed".into())
        );
    }

    #[tokio::test]
    async fn cancelled_via_ledger() {
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
            let key = user_prompt_callback_key("u1", req["request_id"].as_str().unwrap());
            let mut g = ledger_bg.lock().await;
            g.insert(key, json!({"cancelled": true}));
        });

        let decision = gate
            .request_questionnaire(
                "req-4",
                &AskUserPrompt {
                    context: None,
                    questions: vec![astra_tools::AskUserQuestion {
                        header: "Confirm".into(),
                        question: "Continue?".into(),
                        options: vec![],
                        multi_select: false,
                        allow_freeform: true,
                    }],
                    timeout_ms: None,
                },
            )
            .await;
        assert_eq!(decision, AskUserDecision::Cancelled);
    }
}
