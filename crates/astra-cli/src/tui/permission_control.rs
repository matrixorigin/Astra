//! One bounded permission-control submission lane per foreground run.
//! The UI owns intent; the acknowledged mode remains owned by PermissionManager.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use astra_core::sync_poison::recover_mutex_lock;
use astra_thin_client::{RunPermissionModeRequest, ThinClient};
use astra_turn_types::PermissionMode;
use tokio::sync::watch;

use crate::cli::permission_manager::PermissionModeMirror;

#[derive(Clone, Debug)]
struct ModeIntent {
    request_id: String,
    mode: PermissionMode,
}

#[derive(Clone)]
struct SubmissionResult {
    request_id: String,
    outcome: Result<(String, astra_turn_types::RunPermissionModeSelection), String>,
}

pub(super) struct ActivePermissionControl {
    sender: watch::Sender<Option<ModeIntent>>,
    result: watch::Receiver<Option<SubmissionResult>>,
    latest: Option<ModeIntent>,
    reported_error: Option<String>,
    session_id: Arc<Mutex<Option<String>>>,
    session_bound: Arc<tokio::sync::Notify>,
    task: tokio::task::JoinHandle<()>,
}

impl ActivePermissionControl {
    pub(super) fn new(
        api: ThinClient,
        owner: crate::cli::cli_config::cli_utils::CliOwnerAuthSnapshot,
        mut remote_run_id: watch::Receiver<Option<String>>,
        mut admitted_auth: watch::Receiver<
            Option<crate::cli::session::session_runtime::AdmittedAuth>,
        >,
        signal: crate::cli::permission_manager::PermissionControlSignal,
    ) -> Self {
        let api = api.without_bearer_provider();
        let session_id = Arc::new(Mutex::new(None::<String>));
        let bound_session = session_id.clone();
        let session_bound = Arc::new(tokio::sync::Notify::new());
        let bound_wake = session_bound.clone();
        let (sender, mut receiver) = watch::channel(None::<ModeIntent>);
        let (result_tx, result) = watch::channel(None);
        let task = tokio::spawn(async move {
            while receiver.changed().await.is_ok() {
                let Some(mut intent) = receiver.borrow_and_update().clone() else {
                    continue;
                };
                // Startup may not yet have delivered the authenticated Run id.
                // Only an outstanding user request polls; idle sessions do no work.
                let (run_id, session_id, auth) = loop {
                    if !owner.is_current() {
                        result_tx.send_replace(Some(SubmissionResult {
                            request_id: intent.request_id,
                            outcome: Err(
                                "Sign-in changed before the permission request was sent.".into()
                            ),
                        }));
                        return;
                    }
                    if let (Some(run_id), Some(session_id), Some(auth)) = (
                        remote_run_id.borrow().clone(),
                        recover_mutex_lock(&bound_session).clone(),
                        admitted_auth.borrow().clone(),
                    ) {
                        break (run_id, session_id, auth);
                    }
                    tokio::select! {
                        changed = receiver.changed() => {
                            if changed.is_err() { return; }
                            if let Some(latest) = receiver.borrow_and_update().clone() { intent = latest; }
                        }
                        changed = remote_run_id.changed() => if changed.is_err() { return; },
                        changed = admitted_auth.changed() => if changed.is_err() { return; },
                        _ = bound_wake.notified() => {}
                    }
                };
                let outcome = tokio::time::timeout(Duration::from_secs(15), async {
                    let token = auth.access_token(&api, None)
                        .await.ok_or_else(|| "Authentication unavailable for permission change".to_string())?;
                    if !owner.is_current() {
                        return Err("Sign-in changed before the permission request was sent".to_string());
                    }
                    let request = RunPermissionModeRequest {
                        expected_session_id: session_id.to_owned(),
                        request_id: intent.request_id.clone(), mode: intent.mode,
                    };
                    let accepted = match api.request_run_permission_mode(Some(&token), &run_id, &request).await {
                        Err(error) if error.is_transport() => api.request_run_permission_mode(Some(&token), &run_id, &request).await,
                        outcome => outcome,
                    }.map_err(|error| error.to_string())?;
                    if accepted.request_id != intent.request_id || accepted.mode != intent.mode || accepted.revision < 0 {
                        return Err("Permission acknowledgement does not match the requested change".to_string());
                    }
                    signal.accepted(&run_id, &accepted);
                    Ok::<_, String>((run_id.clone(), accepted))
                }).await.unwrap_or_else(|_| Err("Permission change could not be confirmed; the displayed active mode is unchanged".into()));
                result_tx.send_replace(Some(SubmissionResult {
                    request_id: intent.request_id,
                    outcome,
                }));
            }
        });
        Self {
            sender,
            result,
            latest: None,
            reported_error: None,
            session_id,
            session_bound,
            task,
        }
    }

    pub(super) fn bind_session(&self, session_id: &str) {
        let mut bound = recover_mutex_lock(&self.session_id);
        if bound.is_none() {
            *bound = Some(session_id.to_owned());
            self.session_bound.notify_one();
        }
    }

    pub(super) fn session_binding(&self) -> Arc<Mutex<Option<String>>> {
        self.session_id.clone()
    }

    pub(super) fn session_bound_notification(&self) -> Arc<tokio::sync::Notify> {
        self.session_bound.clone()
    }

    pub(super) fn request(&mut self, mode: PermissionMode) -> String {
        let intent = ModeIntent {
            request_id: uuid::Uuid::now_v7().to_string(),
            mode,
        };
        self.reported_error = None;
        self.latest = Some(intent.clone());
        let request_id = intent.request_id.clone();
        self.sender.send_replace(Some(intent));
        request_id
    }

    pub(super) fn is_current_request(&self, request_id: &str) -> bool {
        self.latest
            .as_ref()
            .is_some_and(|intent| intent.request_id == request_id)
    }

    pub(super) fn has_pending(&self) -> bool {
        self.latest.is_some()
    }

    /// A stale response must never erase a newer requested mode.
    pub(super) fn reconcile(
        &mut self,
        mirror: &PermissionModeMirror,
    ) -> Option<Result<PermissionMode, String>> {
        let latest = self.latest.as_ref()?;
        if mirror.applied_request_id().as_deref() == Some(latest.request_id.as_str()) {
            let mode = latest.mode;
            self.latest = None;
            return Some(Ok(mode));
        }
        let result = self.result.borrow().clone()?;
        if result.request_id == latest.request_id {
            match result.outcome {
                Err(error) => {
                    if self.reported_error.as_deref() != Some(latest.request_id.as_str()) {
                        self.reported_error = Some(latest.request_id.clone());
                        return Some(Err(error));
                    }
                }
                Ok((run_id, accepted)) => {
                    if let Some((applied_run, applied)) = mirror.applied_selection()
                        && applied_run == run_id
                        && applied.revision >= accepted.revision
                    {
                        self.latest = None;
                        return Some(Ok(applied.mode));
                    }
                }
            }
        }
        None
    }
}

impl Drop for ActivePermissionControl {
    fn drop(&mut self) {
        self.task.abort();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::permission_manager::PermissionManager;

    #[tokio::test]
    async fn newer_selection_survives_old_ack_and_matching_mode_without_matching_id() {
        let api = ThinClient::new("http://127.0.0.1:1", None).unwrap();
        let mut control = ActivePermissionControl::new(
            api,
            crate::cli::cli_config::cli_utils::cli_owner_auth_snapshot(),
            watch::channel(None).1,
            watch::channel(None).1,
            PermissionManager::new(false).permission_control_signal(),
        );
        let mut manager = PermissionManager::new(false);
        let mirror = manager.mode_mirror_handle();
        control.request(PermissionMode::Bypass);
        let older = control.latest.as_ref().unwrap().request_id.clone();
        control.request(PermissionMode::Prompt);
        let latest = control.latest.as_ref().unwrap().request_id.clone();
        manager.apply_acknowledged_mode(
            "run",
            &astra_turn_types::RunPermissionModeSelection {
                request_id: older,
                mode: PermissionMode::Bypass,
                revision: 1,
            },
        );
        assert!(control.reconcile(&mirror).is_none());
        manager.set_mode(PermissionMode::Prompt);
        assert!(control.reconcile(&mirror).is_none());
        manager.apply_acknowledged_mode(
            "run",
            &astra_turn_types::RunPermissionModeSelection {
                request_id: latest,
                mode: PermissionMode::Prompt,
                revision: 2,
            },
        );
        assert_eq!(control.reconcile(&mirror), Some(Ok(PermissionMode::Prompt)));
        assert!(control.reconcile(&mirror).is_none());
    }

    #[tokio::test]
    async fn dropping_foreground_control_stops_unbound_submission_worker() {
        let api = ThinClient::new("http://127.0.0.1:1", None).unwrap();
        let mut control = ActivePermissionControl::new(
            api,
            crate::cli::cli_config::cli_utils::cli_owner_auth_snapshot(),
            watch::channel(None).1,
            watch::channel(None).1,
            PermissionManager::new(false).permission_control_signal(),
        );
        let task = control.task.abort_handle();
        control.request(PermissionMode::Auto);
        drop(control);
        tokio::task::yield_now().await;
        assert!(task.is_finished());
    }
    #[test]
    fn late_http_acceptance_cannot_reopen_an_already_applied_older_selection() {
        let mut manager = PermissionManager::new(false);
        let signal = manager.permission_control_signal();
        let older = astra_turn_types::RunPermissionModeSelection {
            request_id: "old".into(),
            mode: PermissionMode::Bypass,
            revision: 1,
        };
        let newer = astra_turn_types::RunPermissionModeSelection {
            request_id: "new".into(),
            mode: PermissionMode::Prompt,
            revision: 2,
        };
        manager.apply_acknowledged_mode("run", &newer);
        signal.accepted("run", &older);
        assert!(signal.sender.borrow().is_none());
        signal.accepted("another-run", &older);
        assert_eq!(signal.sender.borrow().as_ref().unwrap().0, "another-run");
    }

    #[tokio::test]
    async fn newer_remote_application_supersedes_an_accepted_local_intent() {
        let api = ThinClient::new("http://127.0.0.1:1", None).unwrap();
        let mut manager = PermissionManager::new(false);
        let mut control = ActivePermissionControl::new(
            api,
            crate::cli::cli_config::cli_utils::cli_owner_auth_snapshot(),
            watch::channel(None).1,
            watch::channel(None).1,
            manager.permission_control_signal(),
        );
        control.request(PermissionMode::Bypass);
        let request_id = control.latest.as_ref().unwrap().request_id.clone();
        let (_tx, rx) = watch::channel(Some(SubmissionResult {
            request_id: request_id.clone(),
            outcome: Ok((
                "run".into(),
                astra_turn_types::RunPermissionModeSelection {
                    request_id,
                    mode: PermissionMode::Bypass,
                    revision: 1,
                },
            )),
        }));
        control.result = rx;
        manager.apply_acknowledged_mode(
            "run",
            &astra_turn_types::RunPermissionModeSelection {
                request_id: "other-device".into(),
                mode: PermissionMode::Plan,
                revision: 2,
            },
        );
        assert_eq!(
            control.reconcile(&manager.mode_mirror_handle()),
            Some(Ok(PermissionMode::Plan))
        );
    }
}
