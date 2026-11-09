//! One user-requested model change on the existing foreground Run channel.
use std::sync::{Arc, Mutex};
use std::time::Duration;

use astra_core::sync_poison::recover_mutex_lock;
use astra_thin_client::{ThinClient, ThinClientError};
use astra_turn_types::{RunModelOutcome, RunModelOutcomeKind, RunModelRequest};
use tokio::sync::{mpsc, watch};

use crate::cli::session::session_runtime::{
    AdmittedAuth, ServerModelSelection, fetch_model_catalog,
    resolve_server_model_selection_from_catalog,
};

#[derive(Clone)]
pub(super) enum ModelChoice {
    Picked(ServerModelSelection),
    Named(String),
}

impl ModelChoice {
    fn label(&self) -> &str {
        match self {
            Self::Picked(model) => &model.name,
            Self::Named(name) => name,
        }
    }
}

#[derive(Clone)]
struct Pending {
    request_id: String,
    choice: ModelChoice,
    attempt: Option<super::event_loop::CatalogAttempt>,
    phase: PendingPhase,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum PendingPhase {
    Sending,
    Accepted,
    Unknown,
}

enum Submission {
    Accepted {
        request_id: String,
    },
    Observed {
        request_id: String,
        run_id: String,
        session_id: String,
        outcome: RunModelOutcome,
    },
    Rejected {
        request_id: String,
        reason: String,
        keep_for_next_turn: bool,
    },
    Superseded {
        request_id: String,
    },
    Unknown {
        request_id: String,
        selection: ServerModelSelection,
    },
}

pub(super) enum ModelFeedback {
    Accepted,
    Applied(String),
    Rejected(String),
    KeepForNext(String),
    Superseded,
    Unknown,
}

#[derive(Clone)]
struct SubmissionContext {
    api: ThinClient,
    owner: crate::cli::cli_config::cli_utils::CliOwnerAuthSnapshot,
    run_id: watch::Receiver<Option<String>>,
    auth: watch::Receiver<Option<AdmittedAuth>>,
    session_id: Arc<Mutex<Option<String>>>,
    session_bound: Arc<tokio::sync::Notify>,
}

impl SubmissionContext {
    fn is_current(&self, pending: &Pending) -> bool {
        self.owner.is_current()
            && pending
                .attempt
                .as_ref()
                .is_none_or(|attempt| attempt.is_current())
            && matches!(self.auth.has_changed(), Ok(false))
            && matches!(self.run_id.has_changed(), Ok(false))
    }

    async fn prepare(
        &mut self,
        pending: &Pending,
    ) -> Result<(String, String, String, AdmittedAuth, ServerModelSelection), String> {
        let (run_id, session_id, auth) = loop {
            if !self.owner.is_current()
                || pending
                    .attempt
                    .as_ref()
                    .is_some_and(|attempt| !attempt.is_current())
            {
                return Err("Account or task authentication changed. Open /model again.".into());
            }
            let run = self.run_id.borrow_and_update().clone();
            let auth = self.auth.borrow_and_update().clone();
            let session = recover_mutex_lock(&self.session_id).clone();
            if let (Some(run), Some(session), Some(auth)) = (run, session, auth) {
                break (run, session, auth);
            }
            tokio::select! {
                changed = self.run_id.changed() => if changed.is_err() { return Err("The task ended before the model change was sent.".into()); },
                changed = self.auth.changed() => if changed.is_err() { return Err("Task authentication ended before the model change was sent.".into()); },
                _ = self.session_bound.notified() => {}
            }
        };
        let token = auth
            .access_token(&self.api, None)
            .await
            .ok_or("Authentication unavailable for model change")?;
        let selection = match &pending.choice {
            ModelChoice::Picked(selection) => selection.clone(),
            ModelChoice::Named(name) => {
                let catalog = fetch_model_catalog(&self.api, Some(&token))
                    .await
                    .map_err(|error| error.to_string())?;
                let mut selection = resolve_server_model_selection_from_catalog(name, &catalog)
                    .map_err(|error| error.to_string())?;
                selection.name = name.clone();
                selection
            }
        };
        if !self.is_current(pending) {
            return Err("Account or task authentication changed. Open /model again.".into());
        }
        Ok((run_id, session_id, token, auth, selection))
    }

    async fn submit(mut self, pending: Pending, recheck: bool) -> Submission {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
        let (run_id, session_id, mut token, admitted_auth, selection) =
            match tokio::time::timeout_at(deadline, self.prepare(&pending)).await {
                Ok(Ok(prepared)) => prepared,
                Ok(Err(reason)) => {
                    return Submission::Rejected {
                        request_id: pending.request_id,
                        reason,
                        keep_for_next_turn: false,
                    };
                }
                Err(_) => {
                    return Submission::Rejected {
                        request_id: pending.request_id,
                        reason: "Model selection timed out before it was sent.".into(),
                        keep_for_next_turn: false,
                    };
                }
            };
        let (_, thinking) =
            astra_turn_core::thinking_config::resolve_model_thinking_request(&selection.name);
        let request = RunModelRequest {
            expected_session_id: session_id.clone(),
            request_id: pending.request_id.clone(),
            model: astra_turn_types::ModelSelection {
                offering_id: selection.offering_id.clone(),
            },
            reasoning: (&thinking).into(),
        };
        let operation = async {
            let mut first = self
                .api
                .request_run_model(Some(&token), &run_id, &request)
                .await;
            if !self.is_current(&pending) {
                return Err(None);
            }
            if matches!(&first, Err(ThinClientError::Api { status, .. }) if status.as_u16() == 401)
                && let Some(refreshed) = admitted_auth.access_token(&self.api, Some(&token)).await
            {
                if !self.is_current(&pending) {
                    return Err(None);
                }
                token = refreshed;
                first = self
                    .api
                    .request_run_model(Some(&token), &run_id, &request)
                    .await;
            }
            let accepted = match first {
                Ok(accepted) => accepted,
                Err(error) if delivery_unconfirmed(&error) => {
                    match self
                        .api
                        .request_run_model(Some(&token), &run_id, &request)
                        .await
                    {
                        Ok(accepted) => accepted,
                        Err(_) => {
                            let snapshot = self
                                .api
                                .get_run_model(Some(&token), &run_id, &session_id)
                                .await
                                .map_err(|_| None)?;
                            snapshot
                                .requested
                                .filter(|selected| {
                                    selected.request_id == request.request_id
                                        && selected.model == request.model
                                        && selected.reasoning == request.reasoning
                                })
                                .ok_or(None)?
                        }
                    }
                }
                Err(error) => {
                    let inactive = run_inactive(&error);
                    let reason = if inactive {
                        "This task has ended. Your model selection will be used for the next task."
                            .into()
                    } else {
                        error.to_string()
                    };
                    return Err(Some((reason, inactive)));
                }
            };
            if !self.is_current(&pending)
                || accepted.request_id != request.request_id
                || accepted.model != request.model
                || accepted.reasoning != request.reasoning
                || accepted.revision < 0
            {
                return Err(None);
            }
            if recheck {
                let snapshot = self
                    .api
                    .get_run_model(Some(&token), &run_id, &session_id)
                    .await
                    .map_err(|_| None)?;
                if !self.is_current(&pending) {
                    return Err(None);
                }
                if let Some(outcome) = snapshot
                    .outcome
                    .filter(|outcome| outcome.selection == accepted)
                {
                    return Ok(Some(Ok(outcome)));
                }
                if snapshot.requested.as_ref().is_some_and(|newer| {
                    newer.revision > accepted.revision && newer.request_id != accepted.request_id
                }) {
                    return Ok(Some(Err(())));
                }
            }
            Ok(None)
        };
        match tokio::time::timeout_at(deadline, operation).await {
            Ok(Ok(Some(Ok(outcome)))) => Submission::Observed {
                request_id: pending.request_id,
                run_id,
                session_id,
                outcome,
            },
            Ok(Ok(Some(Err(())))) => Submission::Superseded {
                request_id: pending.request_id,
            },
            Ok(Ok(None)) => Submission::Accepted {
                request_id: pending.request_id,
            },
            Ok(Err(Some((reason, keep_for_next_turn)))) => Submission::Rejected {
                request_id: pending.request_id,
                reason,
                keep_for_next_turn,
            },
            Ok(Err(None)) | Err(_) => Submission::Unknown {
                request_id: pending.request_id,
                selection,
            },
        }
    }
}

fn run_inactive(error: &ThinClientError) -> bool {
    matches!(error, ThinClientError::Api { status, body }
        if status.as_u16() == 409
            && serde_json::from_str::<serde_json::Value>(body).ok()
                .and_then(|value| value.get("error_code").and_then(serde_json::Value::as_str).map(str::to_owned))
                .as_deref() == Some("run_model_target_inactive"))
}

fn delivery_unconfirmed(error: &ThinClientError) -> bool {
    match error {
        ThinClientError::Api { status, .. } => status.is_server_error() || status.as_u16() == 408,
        ThinClientError::Http(_)
        | ThinClientError::Json(_)
        | ThinClientError::ResponseTooLarge { .. } => true,
        _ => false,
    }
}

pub(super) struct ActiveModelControl {
    context: SubmissionContext,
    pending: Option<Pending>,
    result_tx: mpsc::Sender<Submission>,
    result_rx: mpsc::Receiver<Submission>,
    task: Option<tokio::task::JoinHandle<()>>,
}

impl ActiveModelControl {
    pub(super) fn new(
        api: ThinClient,
        owner: crate::cli::cli_config::cli_utils::CliOwnerAuthSnapshot,
        run_id: watch::Receiver<Option<String>>,
        auth: watch::Receiver<Option<AdmittedAuth>>,
        session_id: Arc<Mutex<Option<String>>>,
        session_bound: Arc<tokio::sync::Notify>,
    ) -> Self {
        let (result_tx, result_rx) = mpsc::channel(1);
        Self {
            context: SubmissionContext {
                api: api.without_bearer_provider(),
                owner,
                run_id,
                auth,
                session_id,
                session_bound,
            },
            pending: None,
            result_tx,
            result_rx,
            task: None,
        }
    }

    pub(super) fn request(
        &mut self,
        choice: ModelChoice,
        attempt: Option<super::event_loop::CatalogAttempt>,
    ) -> Result<(), &'static str> {
        if let Some(pending) = self.pending.as_ref() {
            if pending.phase == PendingPhase::Unknown {
                let mut retry = pending.clone();
                retry.phase = PendingPhase::Sending;
                self.start_submission(retry, true);
                return Err(
                    "Rechecking the previous model change. Select another model after it is confirmed.",
                );
            }
            if pending.phase == PendingPhase::Sending {
                return Err("A model change is still being sent. Try again after it is received.");
            }
        }
        if !self.context.owner.is_current()
            || attempt
                .as_ref()
                .is_some_and(|attempt| !attempt.is_current())
        {
            return Err("Account or task authentication changed. Open /model again.");
        }
        self.start_submission(
            Pending {
                request_id: uuid::Uuid::now_v7().to_string(),
                choice,
                attempt,
                phase: PendingPhase::Sending,
            },
            false,
        );
        Ok(())
    }

    fn start_submission(&mut self, pending: Pending, recheck: bool) {
        self.pending = Some(pending.clone());
        let context = self.context.clone();
        let tx = self.result_tx.clone();
        self.task = Some(tokio::spawn(async move {
            let _ = tx.send(context.submit(pending, recheck).await).await;
        }));
    }

    pub(super) fn take_submission(&mut self) -> Option<ModelFeedback> {
        let submission = loop {
            let submission = self.result_rx.try_recv().ok()?;
            let request_id = match &submission {
                Submission::Accepted { request_id }
                | Submission::Observed { request_id, .. }
                | Submission::Rejected { request_id, .. }
                | Submission::Superseded { request_id }
                | Submission::Unknown { request_id, .. } => request_id,
            };
            if self
                .pending
                .as_ref()
                .is_some_and(|pending| &pending.request_id == request_id)
            {
                break submission;
            }
        };
        if !self.context.owner.is_current()
            || self
                .pending
                .as_ref()
                .and_then(|pending| pending.attempt.as_ref())
                .is_some_and(|attempt| !attempt.is_current())
        {
            self.pending = None;
            return Some(ModelFeedback::Rejected(
                "Task authentication changed. Open /model again.".into(),
            ));
        }
        match submission {
            Submission::Accepted { .. } => {
                if let Some(pending) = &mut self.pending {
                    pending.phase = PendingPhase::Accepted;
                }
                Some(ModelFeedback::Accepted)
            }
            Submission::Observed {
                run_id,
                session_id,
                outcome,
                ..
            } => self.observe(&run_id, &session_id, &outcome),
            Submission::Rejected {
                reason,
                keep_for_next_turn,
                ..
            } => {
                self.pending = None;
                Some(if keep_for_next_turn {
                    ModelFeedback::KeepForNext(reason)
                } else {
                    ModelFeedback::Rejected(reason)
                })
            }
            Submission::Superseded { .. } => {
                self.pending = None;
                Some(ModelFeedback::Superseded)
            }
            Submission::Unknown { selection, .. } => {
                if let Some(pending) = &mut self.pending {
                    pending.phase = PendingPhase::Unknown;
                    pending.choice = ModelChoice::Picked(selection);
                }
                Some(ModelFeedback::Unknown)
            }
        }
    }

    pub(super) fn observe(
        &mut self,
        run_id: &str,
        session_id: &str,
        outcome: &RunModelOutcome,
    ) -> Option<ModelFeedback> {
        let pending = self.pending.as_ref()?;
        if self.context.run_id.borrow().as_deref() != Some(run_id)
            || recover_mutex_lock(&self.context.session_id).as_deref() != Some(session_id)
            || !self.context.owner.is_current()
            || pending
                .attempt
                .as_ref()
                .is_some_and(|attempt| !attempt.is_current())
            || pending.request_id != outcome.selection.request_id
        {
            return None;
        }
        let label = pending.choice.label().to_string();
        let valid = match &pending.choice {
            ModelChoice::Picked(selection) => {
                selection.offering_id == outcome.selection.model.offering_id
            }
            ModelChoice::Named(_) => true,
        } && match &outcome.result {
            RunModelOutcomeKind::Applied { model, reasoning } => {
                model == &outcome.selection.model && reasoning == &outcome.selection.reasoning
            }
            _ => true,
        };
        if !valid {
            if let Some(pending) = &mut self.pending {
                pending.phase = PendingPhase::Unknown;
            }
            if let Some(task) = self.task.take() {
                task.abort();
            }
            return Some(ModelFeedback::Unknown);
        }
        self.pending = None;
        if let Some(task) = self.task.take() {
            task.abort();
        }
        Some(match &outcome.result {
            RunModelOutcomeKind::Applied { .. } => ModelFeedback::Applied(label),
            RunModelOutcomeKind::Rejected { error_code, .. } => {
                if error_code.as_deref() == Some("model_history_incomplete") {
                    return Some(ModelFeedback::KeepForNext(
                        "The current task could not safely continue with that model. Your selection will be used for the next task.".into(),
                    ));
                }
                ModelFeedback::Rejected(match error_code.as_deref() {
                    Some("model_access_denied" | "model_authentication_failed") => {
                        "The selected model is unavailable to this account.".into()
                    }
                    Some("model_reasoning_unsupported") => {
                        "The selected model does not support that reasoning setting.".into()
                    }
                    _ => {
                        "The model change could not be applied. The previous model is still active."
                            .into()
                    }
                })
            }
            RunModelOutcomeKind::Superseded => ModelFeedback::Superseded,
        })
    }

    pub(super) fn has_pending(&self) -> bool {
        self.pending.is_some()
    }
}

impl Drop for ActiveModelControl {
    fn drop(&mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use astra_turn_types::{ReasoningRequirement, RunModelSelection};
    use wiremock::{
        Mock, MockServer, Request, ResponseTemplate,
        matchers::{method, path},
    };

    fn selection(name: &str, offering_id: &str) -> ServerModelSelection {
        ServerModelSelection {
            name: name.into(),
            offering_id: offering_id.into(),
            context_window: None,
            pricing: None,
        }
    }

    fn control(
        server: &MockServer,
    ) -> (
        ActiveModelControl,
        watch::Sender<Option<String>>,
        watch::Sender<Option<AdmittedAuth>>,
    ) {
        let (run_tx, run_rx) = watch::channel(Some("run".to_string()));
        let (auth_tx, auth_rx) = watch::channel(Some(AdmittedAuth::new("test-token", None)));
        (
            ActiveModelControl::new(
                ThinClient::new(&server.uri(), None).unwrap(),
                crate::cli::cli_config::cli_utils::cli_owner_auth_snapshot(),
                run_rx,
                auth_rx,
                Arc::new(Mutex::new(Some("session".into()))),
                Arc::new(tokio::sync::Notify::new()),
            ),
            run_tx,
            auth_tx,
        )
    }

    async fn receive_submission(control: &mut ActiveModelControl) -> Option<ModelFeedback> {
        let submission = tokio::time::timeout(Duration::from_secs(5), control.result_rx.recv())
            .await
            .unwrap()
            .unwrap();
        control.result_tx.send(submission).await.unwrap();
        control.take_submission()
    }

    #[tokio::test]
    async fn model_request_stays_pending_until_matching_durable_outcome() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/runs/run/model"))
            .respond_with(|request: &Request| {
                let body: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
                ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "request_id":body["request_id"], "revision":5,
                    "model":body["model"], "reasoning":body["reasoning"]
                }))
            })
            .expect(1)
            .mount(&server)
            .await;
        let (mut control, _run, _auth) = control(&server);
        control
            .request(ModelChoice::Picked(selection("model", "offering")), None)
            .unwrap();
        assert!(matches!(
            receive_submission(&mut control).await,
            Some(ModelFeedback::Accepted)
        ));
        assert!(control.has_pending());
        let id = control.pending.as_ref().unwrap().request_id.clone();
        let outcome = RunModelOutcome {
            selection: RunModelSelection {
                request_id: id,
                revision: 5,
                model: astra_turn_types::ModelSelection {
                    offering_id: "offering".into(),
                },
                reasoning: ReasoningRequirement::ModelDefault,
            },
            owner_generation: 1,
            round_index: 2,
            result: RunModelOutcomeKind::Applied {
                model: astra_turn_types::ModelSelection {
                    offering_id: "offering".into(),
                },
                reasoning: ReasoningRequirement::ModelDefault,
            },
        };
        assert!(control.observe("wrong-run", "session", &outcome).is_none());
        assert!(control.observe("run", "wrong-session", &outcome).is_none());
        assert!(
            matches!(control.observe("run", "session", &outcome), Some(ModelFeedback::Applied(name)) if name == "model")
        );
        assert!(!control.has_pending());
    }

    #[tokio::test]
    async fn accepted_choice_can_be_replaced_and_old_outcome_is_ignored() {
        use std::sync::atomic::{AtomicI64, Ordering};
        let server = MockServer::start().await;
        let revision = Arc::new(AtomicI64::new(4));
        Mock::given(method("POST"))
            .and(path("/chat/runs/run/model"))
            .respond_with(move |request: &Request| {
                let body: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
                ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "request_id":body["request_id"], "revision":revision.fetch_add(1, Ordering::SeqCst),
                    "model":body["model"], "reasoning":body["reasoning"]
                }))
            })
            .expect(2)
            .mount(&server)
            .await;
        let (mut control, _run, _auth) = control(&server);
        control
            .request(ModelChoice::Picked(selection("a", "a")), None)
            .unwrap();
        assert!(matches!(
            receive_submission(&mut control).await,
            Some(ModelFeedback::Accepted)
        ));
        let old_id = control.pending.as_ref().unwrap().request_id.clone();
        control
            .request(ModelChoice::Picked(selection("b", "b")), None)
            .unwrap();
        assert!(matches!(
            receive_submission(&mut control).await,
            Some(ModelFeedback::Accepted)
        ));
        let current_id = control.pending.as_ref().unwrap().request_id.clone();
        let outcome = |id: String, offering: &str, revision| RunModelOutcome {
            selection: RunModelSelection {
                request_id: id,
                revision,
                model: astra_turn_types::ModelSelection {
                    offering_id: offering.into(),
                },
                reasoning: ReasoningRequirement::ModelDefault,
            },
            owner_generation: 1,
            round_index: 2,
            result: RunModelOutcomeKind::Applied {
                model: astra_turn_types::ModelSelection {
                    offering_id: offering.into(),
                },
                reasoning: ReasoningRequirement::ModelDefault,
            },
        };
        assert!(
            control
                .observe("run", "session", &outcome(old_id, "a", 4))
                .is_none()
        );
        assert!(
            matches!(control.observe("run", "session", &outcome(current_id, "b", 5)),
            Some(ModelFeedback::Applied(name)) if name == "b")
        );
    }

    #[tokio::test]
    async fn uncertain_delivery_retries_the_same_selection_before_any_new_one() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let server = MockServer::start().await;
        let calls = Arc::new(AtomicUsize::new(0));
        let observed = calls.clone();
        Mock::given(method("POST"))
            .and(path("/chat/runs/run/model"))
            .respond_with(move |request: &Request| {
                let body: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
                if observed.fetch_add(1, Ordering::SeqCst) < 2 {
                    return ResponseTemplate::new(503);
                }
                ResponseTemplate::new(200).set_body_json(
                    serde_json::json!({"request_id":body["request_id"], "revision":5,
                    "model":body["model"], "reasoning":body["reasoning"]}),
                )
            })
            .expect(3)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/chat/runs/run/model"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(
                    serde_json::json!({"requested":null,"outcome":null,"applied":null}),
                ),
            )
            .expect(2)
            .mount(&server)
            .await;
        let (mut control, _run, _auth) = control(&server);
        control
            .request(
                ModelChoice::Picked(selection("model-a", "offering-a")),
                None,
            )
            .unwrap();
        let id = control.pending.as_ref().unwrap().request_id.clone();
        assert!(matches!(
            receive_submission(&mut control).await,
            Some(ModelFeedback::Unknown)
        ));
        assert!(
            control
                .request(
                    ModelChoice::Picked(selection("model-b", "offering-b")),
                    None
                )
                .is_err()
        );
        assert!(matches!(
            receive_submission(&mut control).await,
            Some(ModelFeedback::Accepted)
        ));
        let requests = server.received_requests().await.unwrap();
        let posts: Vec<_> = requests
            .iter()
            .filter(|request| request.method.as_str() == "POST")
            .map(|request| serde_json::from_slice::<serde_json::Value>(&request.body).unwrap())
            .collect();
        assert_eq!(posts.len(), 3);
        assert!(
            posts.iter().all(
                |body| body["request_id"] == id && body["model"]["offering_id"] == "offering-a"
            )
        );
    }
}
