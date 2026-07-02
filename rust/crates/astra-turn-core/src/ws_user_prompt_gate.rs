//! WebSocket-based ask_user gate.
//!
//! Bridges the [`astra_tools::AskUserGate`] trait with the WebSocket protocol.
//! Outbound prompt requests are sent via an `mpsc` channel that the WS handler
//! drains during its polling loop. Inbound responses arrive through the shared
//! §5.5 edge callback ledger.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use astra_services::session_journal::{
    AskUserJournalResponse, JournalEvent, JournalWriter, find_latest_ask_user_response,
};
use astra_tools::{AskUserAnswers, AskUserDecision, AskUserGate, AskUserPrompt};
use async_trait::async_trait;
use serde_json::Value;
use tokio::sync::Mutex as TokioMutex;
use tokio::sync::mpsc;

use crate::edge_ledger::user_prompt_callback_key;

/// Default timeout for ask_user prompts (matches frontend 60s countdown).
const USER_PROMPT_TIMEOUT: Duration = Duration::from_secs(60);
const USER_PROMPT_POLL_INTERVAL: Duration = Duration::from_millis(50);
const USER_PROMPT_JOURNAL_POLL_INTERVAL: Duration = Duration::from_millis(250);

/// An outbound ask_user request to be forwarded over WebSocket.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserPromptOutboundRequest {
    pub request_id: String,
    pub session_id: String,
    pub run_id: String,
    pub prompt: AskUserPrompt,
}

/// [`AskUserGate`] implementation backed by WebSocket messaging.
pub struct WebSocketUserPromptGate {
    user_id: String,
    session_id: String,
    run_id: String,
    turn: Option<u32>,
    edge_callback_ledger: Arc<TokioMutex<HashMap<String, Value>>>,
    request_tx: mpsc::Sender<Value>,
    timeout: Duration,
}

impl WebSocketUserPromptGate {
    pub fn new(
        user_id: String,
        session_id: String,
        run_id: String,
        turn: Option<u32>,
        edge_callback_ledger: Arc<TokioMutex<HashMap<String, Value>>>,
        request_tx: mpsc::Sender<Value>,
    ) -> Self {
        Self {
            user_id,
            session_id,
            run_id,
            turn,
            edge_callback_ledger,
            request_tx,
            timeout: USER_PROMPT_TIMEOUT,
        }
    }
}

fn decision_from_user_prompt_value(value: Value) -> AskUserDecision {
    if value
        .get("cancelled")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        return AskUserDecision::Cancelled;
    }
    let Some(answers) = value.get("answers").cloned() else {
        return AskUserDecision::Error("Invalid user prompt response: missing answers".into());
    };
    match serde_json::from_value::<AskUserAnswers>(answers) {
        Ok(answers) => AskUserDecision::Submitted(answers),
        Err(error) => AskUserDecision::Error(format!("Invalid user prompt response: {error}")),
    }
}

fn decision_from_journal_response(response: AskUserJournalResponse) -> AskUserDecision {
    match response.status.as_str() {
        "submitted" => {
            let Some(answers) = response.answers else {
                return AskUserDecision::Error(
                    "Invalid user prompt journal response: missing answers".into(),
                );
            };
            match serde_json::from_value::<AskUserAnswers>(answers) {
                Ok(answers) => AskUserDecision::Submitted(answers),
                Err(error) => {
                    AskUserDecision::Error(format!("Invalid user prompt journal response: {error}"))
                }
            }
        }
        "cancelled" => AskUserDecision::Cancelled,
        other => AskUserDecision::Error(format!(
            "Invalid user prompt journal response status: {other}"
        )),
    }
}

fn append_ask_user_prompted_journal_event(
    session_id: &str,
    run_id: &str,
    turn: Option<u32>,
    request_id: &str,
    prompt: &AskUserPrompt,
) {
    let prompt_json = serde_json::to_value(prompt).unwrap_or(Value::Null);
    match JournalWriter::new(session_id).and_then(|writer| {
        writer.append(&JournalEvent::ask_user_prompted(
            Some(session_id),
            turn,
            request_id,
            Some(run_id),
            prompt_json,
        ))
    }) {
        Ok(()) => {}
        Err(error) => {
            tracing::warn!(
                target: "astra_turn_core::ws_user_prompt_gate",
                session_id = %session_id,
                run_id = %run_id,
                request_id = %request_id,
                error = %error,
                "failed to persist ask_user prompted journal event"
            );
        }
    }
}

async fn evict_late_user_prompt_response(
    ledger: &Arc<TokioMutex<HashMap<String, Value>>>,
    key: &str,
    request_id: &str,
) -> bool {
    let mut guard = ledger.lock().await;
    if guard.remove(key).is_some() {
        tracing::info!(
            target: "astra_turn_core::ws_user_prompt_gate",
            request_id = %request_id,
            "user prompt response arrived after timeout; evicted from ledger"
        );
        true
    } else {
        tracing::debug!(
            target: "astra_turn_core::ws_user_prompt_gate",
            request_id = %request_id,
            "no user prompt response observed before timeout; pending key cleared"
        );
        false
    }
}

async fn wait_user_prompt_response(
    ledger: &Arc<TokioMutex<HashMap<String, Value>>>,
    user_id: &str,
    session_id: &str,
    request_id: &str,
    timeout: Duration,
) -> Option<AskUserDecision> {
    let key = user_prompt_callback_key(user_id, request_id);
    let started = std::time::Instant::now();
    let mut last_journal_lookup: Option<std::time::Instant> = None;
    loop {
        if let Some(value) = {
            let mut guard = ledger.lock().await;
            guard.remove(&key)
        } {
            return Some(decision_from_user_prompt_value(value));
        }

        if last_journal_lookup
            .map(|last| last.elapsed() >= USER_PROMPT_JOURNAL_POLL_INTERVAL)
            .unwrap_or(true)
        {
            last_journal_lookup = Some(std::time::Instant::now());
            match find_latest_ask_user_response(session_id, request_id) {
                Ok(Some(response)) => {
                    return Some(decision_from_journal_response(response));
                }
                Ok(None) => {}
                Err(error) => {
                    tracing::warn!(
                        target: "astra_turn_core::ws_user_prompt_gate",
                        session_id = %session_id,
                        request_id = %request_id,
                        error = %error,
                        "ask_user journal replay lookup failed"
                    );
                }
            }
        }

        if started.elapsed() >= timeout {
            return None;
        }
        let remaining = timeout.saturating_sub(started.elapsed());
        tokio::time::sleep(USER_PROMPT_POLL_INTERVAL.min(remaining)).await;
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
            "session_id": self.session_id,
            "run_id": self.run_id,
            "prompt": prompt,
        });

        if self.request_tx.send(request).await.is_err() {
            return AskUserDecision::Error("WebSocket connection closed".into());
        }
        append_ask_user_prompted_journal_event(
            &self.session_id,
            &self.run_id,
            self.turn,
            request_id,
            prompt,
        );

        let key = user_prompt_callback_key(&self.user_id, request_id);
        let timeout = prompt
            .timeout_ms
            .map(Duration::from_millis)
            .unwrap_or(self.timeout);
        match wait_user_prompt_response(
            &self.edge_callback_ledger,
            &self.user_id,
            &self.session_id,
            request_id,
            timeout,
        )
        .await
        {
            Some(decision) => decision,
            None => {
                evict_late_user_prompt_response(&self.edge_callback_ledger, &key, request_id).await;
                AskUserDecision::Timeout
            }
        }
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
    ) -> WebSocketUserPromptGate {
        WebSocketUserPromptGate {
            user_id: "u1".into(),
            session_id: "sess-user-prompt".into(),
            run_id: "run-user-prompt".into(),
            turn: Some(3),
            edge_callback_ledger: ledger,
            request_tx: tx,
            timeout,
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn answered_via_ledger() {
        let tmp = tempfile::tempdir().unwrap();
        let _guard = astra_services::session_journal::JournalDirGuard::new(tmp.path());
        let ledger = Arc::new(TokioMutex::new(HashMap::new()));
        let (tx, mut rx) = mpsc::channel::<Value>(1);

        let gate = test_gate(ledger.clone(), tx, Duration::from_secs(5));

        let ledger_bg = ledger.clone();
        tokio::spawn(async move {
            let req = rx.recv().await.unwrap();
            assert_eq!(req["session_id"].as_str(), Some("sess-user-prompt"));
            assert_eq!(req["run_id"].as_str(), Some("run-user-prompt"));
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

    #[tokio::test(flavor = "current_thread")]
    async fn timeout_when_no_response() {
        let tmp = tempfile::tempdir().unwrap();
        let _guard = astra_services::session_journal::JournalDirGuard::new(tmp.path());
        let ledger = Arc::new(TokioMutex::new(HashMap::new()));
        let (tx, _rx) = mpsc::channel::<Value>(1);

        let gate = test_gate(ledger, tx, Duration::from_millis(100));

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

    #[tokio::test(flavor = "current_thread")]
    async fn timeout_cleanup_removes_late_response_from_ledger() {
        let ledger = Arc::new(TokioMutex::new(HashMap::new()));
        let key = user_prompt_callback_key("u1", "req-late");
        ledger
            .lock()
            .await
            .insert(key.clone(), json!({"cancelled": true}));

        assert!(
            evict_late_user_prompt_response(&ledger, &key, "req-late").await,
            "cleanup should report that it removed a late response"
        );
        assert!(
            !ledger.lock().await.contains_key(&key),
            "late user prompt response must not linger in the ledger"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn answered_via_journal_when_ledger_is_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let _guard = astra_services::session_journal::JournalDirGuard::new(tmp.path());
        let writer = JournalWriter::new("sess-user-prompt").unwrap();
        writer
            .append(&JournalEvent::ask_user_response(
                Some("sess-user-prompt"),
                Some(3),
                "req-journal",
                Some("run-user-prompt"),
                "submitted",
                Some(json!({
                    "answers": [{
                        "question": "Continue?",
                        "answers": ["yes"],
                        "multi_select": false
                    }]
                })),
            ))
            .unwrap();

        let ledger = Arc::new(TokioMutex::new(HashMap::new()));
        let (tx, _rx) = mpsc::channel::<Value>(1);
        let gate = test_gate(ledger.clone(), tx, Duration::from_secs(5));

        let decision = gate
            .request_questionnaire(
                "req-journal",
                &AskUserPrompt {
                    context: None,
                    questions: vec![astra_tools::AskUserQuestion {
                        header: "Confirm".into(),
                        question: "Continue?".into(),
                        options: vec![],
                        multi_select: false,
                        allow_freeform: true,
                    }],
                    timeout_ms: Some(250),
                },
            )
            .await;

        assert!(matches!(decision, AskUserDecision::Submitted(_)));
        assert!(ledger.lock().await.is_empty());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn prompt_timeout_overrides_gate_default() {
        let tmp = tempfile::tempdir().unwrap();
        let _guard = astra_services::session_journal::JournalDirGuard::new(tmp.path());
        let ledger = Arc::new(TokioMutex::new(HashMap::new()));
        let (tx, _rx) = mpsc::channel::<Value>(1);

        let gate = test_gate(ledger, tx, Duration::from_secs(5));

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

    #[tokio::test(flavor = "current_thread")]
    async fn channel_closed_returns_error() {
        let ledger = Arc::new(TokioMutex::new(HashMap::new()));
        let (tx, rx) = mpsc::channel::<Value>(1);
        drop(rx);

        let gate = test_gate(ledger, tx, Duration::from_secs(5));

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

    #[tokio::test(flavor = "current_thread")]
    async fn cancelled_via_ledger() {
        let tmp = tempfile::tempdir().unwrap();
        let _guard = astra_services::session_journal::JournalDirGuard::new(tmp.path());
        let ledger = Arc::new(TokioMutex::new(HashMap::new()));
        let (tx, mut rx) = mpsc::channel::<Value>(1);

        let gate = test_gate(ledger.clone(), tx, Duration::from_secs(5));

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
