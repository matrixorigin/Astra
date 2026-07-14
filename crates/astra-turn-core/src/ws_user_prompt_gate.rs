//! WebSocket-based ask_user gate.
//!
//! Bridges the [`astra_tools::AskUserGate`] trait with the WebSocket protocol.
//! Outbound prompt requests are sent via an `mpsc` channel that the WS handler
//! drains during its polling loop. Inbound responses arrive through the shared
//! §5.5 edge callback ledger.

use std::collections::HashMap;
use std::sync::{Arc, OnceLock, RwLock};
use std::time::Duration;

use crate::pipeline_metrics::MetricsRegistry;
use astra_services::InteractionStatus;
use astra_services::session_journal::{
    AskUserJournalResponse, AskUserResponseAppendOutcome, JournalEvent, JournalWriter,
    append_ask_user_response_for_run_if_absent, find_latest_ask_user_response_for_run,
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
const METRIC_ASK_USER_WAIT_TOTAL: &str = "astra_interaction_ask_user_wait_total";
const METRIC_ASK_USER_JOURNAL_LOOKUP_TOTAL: &str =
    "astra_interaction_ask_user_journal_lookup_total";
const METRIC_ASK_USER_JOURNAL_WRITE_TOTAL: &str = "astra_interaction_ask_user_journal_write_total";
const METRIC_ASK_USER_LEDGER_CLEANUP_TOTAL: &str =
    "astra_interaction_ask_user_ledger_cleanup_total";

/// An outbound ask_user request to be forwarded over WebSocket.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserPromptOutboundRequest {
    pub request_id: String,
    pub session_id: String,
    pub run_id: String,
    pub prompt: AskUserPrompt,
}

/// Durable identity for one ask-user interaction.  Delivery transports are
/// optional projections; this journal identity remains authoritative across
/// reconnects and Server Only execution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserPromptJournalContext {
    pub session_id: String,
    pub run_id: String,
    pub turn: Option<u32>,
}

impl UserPromptJournalContext {
    pub fn new(session_id: String, run_id: String, turn: Option<u32>) -> Self {
        Self {
            session_id,
            run_id,
            turn,
        }
    }
}

/// The durable outcome observed when an ask-user deadline closes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UserPromptDeadlineClose {
    TimedOut,
    Resolved(AskUserDecision),
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

fn metrics_slot() -> &'static RwLock<Option<Arc<MetricsRegistry>>> {
    static SLOT: OnceLock<RwLock<Option<Arc<MetricsRegistry>>>> = OnceLock::new();
    SLOT.get_or_init(Default::default)
}

/// Attach the shared runtime metrics registry used by `/metrics`.
pub fn set_ws_user_prompt_metrics_registry(registry: Arc<MetricsRegistry>) {
    register_ws_user_prompt_metrics(&registry);
    *metrics_slot()
        .write()
        .expect("ws user prompt metrics registry lock poisoned") = Some(registry);
}

fn ws_user_prompt_metrics_registry() -> Option<Arc<MetricsRegistry>> {
    metrics_slot()
        .read()
        .expect("ws user prompt metrics registry lock poisoned")
        .clone()
}

pub fn register_ws_user_prompt_metrics(registry: &MetricsRegistry) {
    registry.register_counter(
        METRIC_ASK_USER_WAIT_TOTAL,
        "ask_user waits by response source and low-cardinality outcome.",
    );
    registry.register_counter(
        METRIC_ASK_USER_JOURNAL_LOOKUP_TOTAL,
        "ask_user durable response journal lookups by low-cardinality outcome.",
    );
    registry.register_counter(
        METRIC_ASK_USER_JOURNAL_WRITE_TOTAL,
        "ask_user durable journal writes by event type and low-cardinality outcome.",
    );
    registry.register_counter(
        METRIC_ASK_USER_LEDGER_CLEANUP_TOTAL,
        "ask_user ledger timeout cleanup attempts by low-cardinality outcome.",
    );
}

fn record_wait_metric(source: &'static str, outcome: &'static str) {
    let Some(registry) = ws_user_prompt_metrics_registry() else {
        return;
    };
    register_ws_user_prompt_metrics(&registry);
    registry.increment_counter(
        METRIC_ASK_USER_WAIT_TOTAL,
        &[("source", source), ("outcome", outcome)],
        1,
    );
}

fn record_journal_lookup_metric(outcome: &'static str) {
    let Some(registry) = ws_user_prompt_metrics_registry() else {
        return;
    };
    register_ws_user_prompt_metrics(&registry);
    registry.increment_counter(
        METRIC_ASK_USER_JOURNAL_LOOKUP_TOTAL,
        &[("outcome", outcome)],
        1,
    );
}

fn record_journal_write_metric(event: &'static str, outcome: &'static str) {
    let Some(registry) = ws_user_prompt_metrics_registry() else {
        return;
    };
    register_ws_user_prompt_metrics(&registry);
    registry.increment_counter(
        METRIC_ASK_USER_JOURNAL_WRITE_TOTAL,
        &[("event", event), ("outcome", outcome)],
        1,
    );
}

fn record_ledger_cleanup_metric(outcome: &'static str) {
    let Some(registry) = ws_user_prompt_metrics_registry() else {
        return;
    };
    register_ws_user_prompt_metrics(&registry);
    registry.increment_counter(
        METRIC_ASK_USER_LEDGER_CLEANUP_TOTAL,
        &[("outcome", outcome)],
        1,
    );
}

fn decision_outcome_label(decision: &AskUserDecision) -> &'static str {
    match decision {
        AskUserDecision::Submitted(_) => "submitted",
        AskUserDecision::Cancelled => "cancelled",
        AskUserDecision::Timeout => "timeout",
        AskUserDecision::Error(_) => "error",
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

fn decision_from_journal_response(
    response: AskUserJournalResponse,
    session_id: &str,
    user_id: &str,
) -> Option<AskUserDecision> {
    let Some(contract) = response.interaction_contract(session_id, Some(user_id)) else {
        return Some(AskUserDecision::Error(
            "Invalid user prompt journal response identity".into(),
        ));
    };
    match contract.status {
        InteractionStatus::Pending => None,
        InteractionStatus::Expired => Some(AskUserDecision::Timeout),
        InteractionStatus::Cancelled => Some(AskUserDecision::Cancelled),
        InteractionStatus::Resolved => {
            let Some(answers) = response.answers else {
                return Some(AskUserDecision::Error(
                    "Invalid user prompt journal response: missing answers".into(),
                ));
            };
            match serde_json::from_value::<AskUserAnswers>(answers) {
                Ok(answers) => Some(AskUserDecision::Submitted(answers)),
                Err(error) => Some(AskUserDecision::Error(format!(
                    "Invalid user prompt journal response: {error}"
                ))),
            }
        }
    }
}

/// A same-pod ledger value is only a delivery acceleration.  Turn it into the
/// same immutable journal fact before returning it to the tool loop so a
/// process crash after the wake-up cannot erase the user's answer.
fn persist_ledger_user_prompt_decision(
    context: &UserPromptJournalContext,
    request_id: &str,
    user_id: &str,
    decision: AskUserDecision,
) -> AskUserDecision {
    let (status, answers) = match &decision {
        AskUserDecision::Submitted(answers) => match serde_json::to_value(answers) {
            Ok(value) => ("submitted", Some(value)),
            Err(error) => {
                return AskUserDecision::Error(format!(
                    "failed to serialize user prompt answers: {error}"
                ));
            }
        },
        AskUserDecision::Cancelled => ("cancelled", None),
        AskUserDecision::Timeout => ("timeout", None),
        AskUserDecision::Error(_) => return decision,
    };
    match append_ask_user_response_for_run_if_absent(
        &context.session_id,
        context.turn,
        request_id,
        &context.run_id,
        status,
        answers,
    ) {
        Ok(AskUserResponseAppendOutcome::Appended | AskUserResponseAppendOutcome::Idempotent) => {
            decision
        }
        Ok(AskUserResponseAppendOutcome::Conflict(existing)) => {
            decision_from_journal_response(existing, &context.session_id, user_id).unwrap_or(
                AskUserDecision::Error(
                    "conflicting user prompt response is not terminal".to_string(),
                ),
            )
        }
        Err(error) => AskUserDecision::Error(format!(
            "failed to record user prompt response durably: {error}"
        )),
    }
}

pub fn persist_user_prompt_required(
    context: &UserPromptJournalContext,
    request_id: &str,
    prompt: &AskUserPrompt,
) -> std::io::Result<()> {
    let prompt_json = serde_json::to_value(prompt).unwrap_or(Value::Null);
    JournalWriter::new(&context.session_id).and_then(|writer| {
        writer.append(&JournalEvent::ask_user_prompted(
            Some(&context.session_id),
            context.turn,
            request_id,
            Some(&context.run_id),
            prompt_json,
        ))
    })
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
        record_ledger_cleanup_metric("evicted");
        true
    } else {
        tracing::debug!(
            target: "astra_turn_core::ws_user_prompt_gate",
            request_id = %request_id,
            "no user prompt response observed before timeout; pending key cleared"
        );
        record_ledger_cleanup_metric("empty");
        false
    }
}

pub async fn wait_for_durable_user_prompt_response(
    ledger: &Arc<TokioMutex<HashMap<String, Value>>>,
    user_id: &str,
    context: &UserPromptJournalContext,
    request_id: &str,
    timeout: Duration,
) -> Option<AskUserDecision> {
    let key = user_prompt_callback_key(user_id, &context.session_id, &context.run_id, request_id);
    let started = std::time::Instant::now();
    let mut last_journal_lookup: Option<std::time::Instant> = None;
    loop {
        if let Some(value) = {
            let mut guard = ledger.lock().await;
            guard.remove(&key)
        } {
            let decision = decision_from_user_prompt_value(value);
            let decision =
                persist_ledger_user_prompt_decision(context, request_id, user_id, decision);
            record_wait_metric("ledger", decision_outcome_label(&decision));
            return Some(decision);
        }

        if last_journal_lookup
            .map(|last| last.elapsed() >= USER_PROMPT_JOURNAL_POLL_INTERVAL)
            .unwrap_or(true)
        {
            last_journal_lookup = Some(std::time::Instant::now());
            match find_latest_ask_user_response_for_run(
                &context.session_id,
                request_id,
                &context.run_id,
            ) {
                Ok(Some(response)) => {
                    match decision_from_journal_response(response, &context.session_id, user_id) {
                        Some(decision) => {
                            record_journal_lookup_metric("hit");
                            record_wait_metric("journal", decision_outcome_label(&decision));
                            return Some(decision);
                        }
                        None => {
                            record_journal_lookup_metric("pending");
                        }
                    }
                }
                Ok(None) => {
                    record_journal_lookup_metric("miss");
                }
                Err(error) => {
                    record_journal_lookup_metric("error");
                    tracing::warn!(
                        target: "astra_turn_core::ws_user_prompt_gate",
                        session_id = %context.session_id,
                        request_id = %request_id,
                        error = %error,
                        "ask_user journal replay lookup failed"
                    );
                }
            }
        }

        if started.elapsed() >= timeout {
            record_wait_metric("timeout", "timeout");
            return None;
        }
        let remaining = timeout.saturating_sub(started.elapsed());
        tokio::time::sleep(USER_PROMPT_POLL_INTERVAL.min(remaining)).await;
    }
}

/// Atomically make an unanswered prompt terminal at its deadline.  A late
/// callback cannot overwrite the timeout; if a callback won the race, return
/// its existing durable outcome instead.
pub fn close_user_prompt_at_deadline(
    context: &UserPromptJournalContext,
    request_id: &str,
    user_id: &str,
) -> std::io::Result<UserPromptDeadlineClose> {
    if let Some(response) =
        find_latest_ask_user_response_for_run(&context.session_id, request_id, &context.run_id)?
    {
        if let Some(decision) =
            decision_from_journal_response(response, &context.session_id, user_id)
        {
            return Ok(UserPromptDeadlineClose::Resolved(decision));
        }
    }

    match append_ask_user_response_for_run_if_absent(
        &context.session_id,
        context.turn,
        request_id,
        &context.run_id,
        "timeout",
        None,
    )? {
        AskUserResponseAppendOutcome::Appended | AskUserResponseAppendOutcome::Idempotent => {
            Ok(UserPromptDeadlineClose::TimedOut)
        }
        AskUserResponseAppendOutcome::Conflict(response) => Ok(UserPromptDeadlineClose::Resolved(
            decision_from_journal_response(response, &context.session_id, user_id).unwrap_or(
                AskUserDecision::Error("invalid ask_user response at deadline".into()),
            ),
        )),
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

        let context =
            UserPromptJournalContext::new(self.session_id.clone(), self.run_id.clone(), self.turn);
        match persist_user_prompt_required(&context, request_id, prompt) {
            Ok(()) => record_journal_write_metric("prompted", "ok"),
            Err(error) => {
                record_journal_write_metric("prompted", "error");
                tracing::warn!(
                    target: "astra_turn_core::ws_user_prompt_gate",
                    session_id = %self.session_id,
                    run_id = %self.run_id,
                    request_id = %request_id,
                    error = %error,
                    "failed to persist ask_user prompted journal event"
                );
                return AskUserDecision::Error(
                    "ask_user prompt could not be recorded durably".into(),
                );
            }
        }
        if self.request_tx.send(request).await.is_err() {
            record_wait_metric("channel", "send_error");
            return AskUserDecision::Error("WebSocket connection closed".into());
        }

        let key =
            user_prompt_callback_key(&self.user_id, &self.session_id, &self.run_id, request_id);
        let timeout = prompt
            .timeout_ms
            .map(Duration::from_millis)
            .unwrap_or(self.timeout);
        match wait_for_durable_user_prompt_response(
            &self.edge_callback_ledger,
            &self.user_id,
            &context,
            request_id,
            timeout,
        )
        .await
        {
            Some(decision) => decision,
            None => {
                evict_late_user_prompt_response(&self.edge_callback_ledger, &key, request_id).await;
                match close_user_prompt_at_deadline(&context, request_id, &self.user_id) {
                    Ok(UserPromptDeadlineClose::TimedOut) => AskUserDecision::Timeout,
                    Ok(UserPromptDeadlineClose::Resolved(decision)) => decision,
                    Err(error) => AskUserDecision::Error(format!(
                        "ask_user deadline could not be closed durably: {error}"
                    )),
                }
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

            let key = user_prompt_callback_key(
                "u1",
                req["session_id"].as_str().unwrap(),
                req["run_id"].as_str().unwrap(),
                req["request_id"].as_str().unwrap(),
            );
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
        let timeout =
            find_latest_ask_user_response_for_run("sess-user-prompt", "req-2", "run-user-prompt")
                .unwrap()
                .expect("timeout must be an immutable durable interaction outcome");
        assert_eq!(timeout.status, "timeout");
        let late = append_ask_user_response_for_run_if_absent(
            "sess-user-prompt",
            Some(3),
            "req-2",
            "run-user-prompt",
            "submitted",
            Some(json!({"answers": []})),
        )
        .unwrap();
        assert!(matches!(
            late,
            AskUserResponseAppendOutcome::Conflict(AskUserJournalResponse { status, .. })
                if status == "timeout"
        ));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn timeout_cleanup_removes_late_response_from_ledger() {
        let ledger = Arc::new(TokioMutex::new(HashMap::new()));
        let key = user_prompt_callback_key("u1", "sess-user-prompt", "run-user-prompt", "req-late");
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
        let registry = Arc::new(crate::pipeline_metrics::MetricsRegistry::new());
        set_ws_user_prompt_metrics_registry(registry.clone());
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
        let metrics = registry.render_prometheus();
        assert!(
            metrics.contains(
                "astra_interaction_ask_user_wait_total{outcome=\"submitted\",source=\"journal\"}"
            ),
            "{metrics}"
        );
        assert!(
            metrics.contains("astra_interaction_ask_user_journal_lookup_total{outcome=\"hit\"}"),
            "{metrics}"
        );
        assert!(
            metrics.contains(
                "astra_interaction_ask_user_journal_write_total{event=\"prompted\",outcome=\"ok\"}"
            ),
            "{metrics}"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn journal_response_from_other_run_does_not_satisfy_wait() {
        let tmp = tempfile::tempdir().unwrap();
        let _guard = astra_services::session_journal::JournalDirGuard::new(tmp.path());
        let writer = JournalWriter::new("sess-user-prompt").unwrap();
        writer
            .append(&JournalEvent::ask_user_response(
                Some("sess-user-prompt"),
                Some(3),
                "req-cross-run",
                Some("other-run"),
                "submitted",
                Some(json!({
                    "answers": [{
                        "question": "Continue?",
                        "answers": ["wrong"],
                        "multi_select": false
                    }]
                })),
            ))
            .unwrap();

        let ledger = Arc::new(TokioMutex::new(HashMap::new()));
        let (tx, _rx) = mpsc::channel::<Value>(1);
        let gate = test_gate(ledger, tx, Duration::from_millis(40));

        let decision = gate
            .request_questionnaire(
                "req-cross-run",
                &AskUserPrompt {
                    context: None,
                    questions: vec![astra_tools::AskUserQuestion {
                        header: "Confirm".into(),
                        question: "Continue?".into(),
                        options: vec![],
                        multi_select: false,
                        allow_freeform: true,
                    }],
                    timeout_ms: Some(40),
                },
            )
            .await;

        assert_eq!(decision, AskUserDecision::Timeout);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn ledger_response_from_other_run_does_not_satisfy_wait() {
        let tmp = tempfile::tempdir().unwrap();
        let _guard = astra_services::session_journal::JournalDirGuard::new(tmp.path());
        let ledger = Arc::new(TokioMutex::new(HashMap::new()));
        let wrong_key = user_prompt_callback_key(
            "u1",
            "sess-user-prompt",
            "other-run",
            "req-cross-run-ledger",
        );
        ledger.lock().await.insert(
            wrong_key.clone(),
            json!({
                "answers": {
                    "answers": [{
                        "question": "Continue?",
                        "answers": ["wrong"],
                        "multi_select": false
                    }]
                }
            }),
        );
        let (tx, _rx) = mpsc::channel::<Value>(1);
        let gate = test_gate(ledger.clone(), tx, Duration::from_millis(40));

        let decision = gate
            .request_questionnaire(
                "req-cross-run-ledger",
                &AskUserPrompt {
                    context: None,
                    questions: vec![astra_tools::AskUserQuestion {
                        header: "Confirm".into(),
                        question: "Continue?".into(),
                        options: vec![],
                        multi_select: false,
                        allow_freeform: true,
                    }],
                    timeout_ms: Some(40),
                },
            )
            .await;

        assert_eq!(decision, AskUserDecision::Timeout);
        assert!(
            ledger.lock().await.contains_key(&wrong_key),
            "wrong-run ledger entry must not be consumed by this waiter"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn pending_journal_response_does_not_satisfy_wait() {
        let tmp = tempfile::tempdir().unwrap();
        let _guard = astra_services::session_journal::JournalDirGuard::new(tmp.path());
        let writer = JournalWriter::new("sess-user-prompt").unwrap();
        writer
            .append(&JournalEvent::ask_user_response(
                Some("sess-user-prompt"),
                Some(3),
                "req-pending-journal",
                Some("run-user-prompt"),
                "waiting",
                None,
            ))
            .unwrap();

        let ledger = Arc::new(TokioMutex::new(HashMap::new()));
        let (tx, _rx) = mpsc::channel::<Value>(1);
        let gate = test_gate(ledger, tx, Duration::from_millis(40));

        let decision = gate
            .request_questionnaire(
                "req-pending-journal",
                &AskUserPrompt {
                    context: None,
                    questions: vec![astra_tools::AskUserQuestion {
                        header: "Confirm".into(),
                        question: "Continue?".into(),
                        options: vec![],
                        multi_select: false,
                        allow_freeform: true,
                    }],
                    timeout_ms: Some(40),
                },
            )
            .await;

        assert_eq!(decision, AskUserDecision::Timeout);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn expired_journal_response_maps_to_timeout() {
        let tmp = tempfile::tempdir().unwrap();
        let _guard = astra_services::session_journal::JournalDirGuard::new(tmp.path());
        let writer = JournalWriter::new("sess-user-prompt").unwrap();
        writer
            .append(&JournalEvent::ask_user_response(
                Some("sess-user-prompt"),
                Some(3),
                "req-expired-journal",
                Some("run-user-prompt"),
                "expired",
                None,
            ))
            .unwrap();

        let ledger = Arc::new(TokioMutex::new(HashMap::new()));
        let (tx, _rx) = mpsc::channel::<Value>(1);
        let gate = test_gate(ledger, tx, Duration::from_secs(5));

        let decision = gate
            .request_questionnaire(
                "req-expired-journal",
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

        assert_eq!(decision, AskUserDecision::Timeout);
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
            let key = user_prompt_callback_key(
                "u1",
                req["session_id"].as_str().unwrap(),
                req["run_id"].as_str().unwrap(),
                req["request_id"].as_str().unwrap(),
            );
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
