use std::future::Future;
use std::sync::{Arc, Mutex as StdMutex, RwLock};

use serde_json::{Value, json};

use crate::server::runtime_tool_executor::SessionConfigInner;
use crate::server::tool_session_state_rollback::{
    self, SessionStateRollbackAction, SessionStateRollbackJournal,
};

type RuntimeConfigUpdate = astra_config::GovernedConfigMutation;

pub(crate) fn effective_runtime_config(
    workspace: Option<&astra_services::session_workspace::WorkspaceMetadata>,
) -> Result<astra_config::runtime_config::RuntimeConfig, String> {
    match workspace.and_then(|workspace| workspace.tuned_config_json.as_deref()) {
        Some(json) => serde_json::from_str(json).map_err(|error| error.to_string()),
        None => Ok(astra_config::runtime_config::RuntimeConfig::load()),
    }
}

pub(crate) fn append_config_change_event(
    user_id: &str,
    session_id: &str,
    turn: u32,
    key: &str,
    new_value: &Value,
    old_value: Option<Value>,
    source: &str,
) -> Result<(), String> {
    let writer = astra_services::session_journal::JournalWriter::for_user(user_id, session_id)
        .map_err(|e| e.to_string())?;
    let mut event = astra_services::session_journal::JournalEvent::config_change(
        Some(session_id),
        key,
        &new_value.to_string(),
    );
    event.turn = Some(turn);
    let mut metadata =
        serde_json::Map::from_iter([("source".to_string(), Value::String(source.to_string()))]);
    if let Some(old_value) = old_value {
        metadata.insert("old_value".to_string(), old_value);
    }
    event.metadata = Some(Value::Object(metadata));
    writer.append(&event).map_err(|e| e.to_string())
}

#[derive(Debug)]
struct DurableConfigMutation {
    update: RuntimeConfigUpdate,
    previous_config: Box<astra_config::RuntimeConfig>,
    committed_config: Box<astra_config::RuntimeConfig>,
    committed_tuned_config_json: Option<String>,
    turn: u32,
}

#[derive(Debug)]
enum ConfigMutationRejection {
    Invalid(Value),
    RevisionConflict {
        current_revision: u64,
        current_value: Value,
        current_config: Box<astra_config::RuntimeConfig>,
        current_workspace: Box<astra_services::session_workspace::WorkspaceMetadata>,
    },
}

type ConfigMutationOutcome = astra_services::session_workspace::WorkspaceConfigMutationOutcome<
    DurableConfigMutation,
    ConfigMutationRejection,
>;

fn bounded_warning(error: impl std::fmt::Display) -> String {
    error.to_string().chars().take(240).collect()
}

fn config_value(config: &astra_config::RuntimeConfig, path: &str) -> Result<Value, String> {
    let value = serde_json::to_value(config).map_err(|error| error.to_string())?;
    astra_config::read_existing_json_path(&value, path).map_err(|error| error.to_string())
}

fn commit_config_mutation(
    session_id: &str,
    path: &str,
    expected_revision: Option<u64>,
    mutate: impl FnOnce(
        &mut astra_config::RuntimeConfig,
    ) -> Result<RuntimeConfigUpdate, ConfigMutationRejection>,
) -> Result<ConfigMutationOutcome, String> {
    let baseline_json = serde_json::to_value(astra_config::runtime_config::RuntimeConfig::load())
        .map_err(|e| e.to_string())?;
    astra_services::session_workspace::update_existing_workspace_config(session_id, |workspace| {
        let previous_config = effective_runtime_config(Some(workspace))
            .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
        if expected_revision.is_some_and(|expected| expected != workspace.config_mutation_revision)
        {
            return Ok(std::ops::ControlFlow::Break(
                ConfigMutationRejection::RevisionConflict {
                    current_revision: workspace.config_mutation_revision,
                    current_value: config_value(&previous_config, path).map_err(|error| {
                        std::io::Error::new(std::io::ErrorKind::InvalidData, error)
                    })?,
                    current_config: Box::new(previous_config),
                    current_workspace: Box::new(workspace.clone()),
                },
            ));
        }
        let mut committed_config = previous_config.clone();
        let update = match mutate(&mut committed_config) {
            Ok(update) => update,
            Err(rejection) => {
                return Ok(std::ops::ControlFlow::Break(rejection));
            }
        };
        let committed_json =
            serde_json::to_value(&committed_config).map_err(std::io::Error::other)?;
        workspace.tuned_config_json = if committed_json == baseline_json {
            None
        } else {
            Some(serde_json::to_string(&committed_config).map_err(std::io::Error::other)?)
        };
        workspace.updated_at = chrono::Utc::now().to_rfc3339();
        Ok(std::ops::ControlFlow::Continue(DurableConfigMutation {
            update,
            previous_config: Box::new(previous_config),
            committed_config: Box::new(committed_config),
            committed_tuned_config_json: workspace.tuned_config_json.clone(),
            turn: workspace.turn_count,
        }))
    })
    .map_err(|error| error.to_string())
}

fn record_outcome_unknown_config_rollback(
    journal: &StdMutex<SessionStateRollbackJournal>,
    journal_turn_index: u32,
    path: &str,
    mutation: &DurableConfigMutation,
    proposed_revision: u64,
    observed: Option<&astra_services::session_workspace::WorkspaceMetadata>,
) -> bool {
    let Some(owner_revision) =
        astra_services::session_workspace::exact_workspace_config_owner_revision(
            proposed_revision,
            &mutation.committed_tuned_config_json,
            observed,
        )
    else {
        return false;
    };
    tool_session_state_rollback::record(
        journal,
        journal_turn_index,
        format!("adjust_config:{path}"),
        SessionStateRollbackAction::ConfigOverride {
            path: path.to_string(),
            old_value: mutation.update.old_value.clone(),
            expected_revision: Arc::new(std::sync::atomic::AtomicU64::new(owner_revision)),
        },
    );
    true
}

fn append_config_mutation_audit(
    user_id: &str,
    session_id: &str,
    path: &str,
    mutation: &DurableConfigMutation,
    source: &str,
) -> Option<String> {
    match append_config_change_event(
        user_id,
        session_id,
        mutation.turn,
        path,
        &mutation.update.new_value,
        Some(mutation.update.old_value.clone()),
        source,
    ) {
        Ok(()) => None,
        Err(error) => {
            let warning = bounded_warning(&error);
            tracing::warn!(
                session_id,
                path,
                source,
                error,
                "config override committed but its audit event could not be appended"
            );
            Some(warning)
        }
    }
}

pub(crate) type ConfigRestoreOutcome =
    astra_services::session_workspace::WorkspaceConfigRestoreOutcome;

pub(crate) fn restore_config_override(
    user_id: &str,
    session_id: &str,
    path: &str,
    new_value: Value,
    expected_revision: u64,
    source: &str,
) -> Result<ConfigRestoreOutcome, String> {
    let outcome = astra_services::session_workspace::restore_workspace_config_override(
        session_id,
        path,
        new_value.clone(),
        expected_revision,
    )?;
    if let ConfigRestoreOutcome::Applied {
        previous_value,
        workspace,
        ..
    } = &outcome
    {
        let _ = append_config_change_event(
            user_id,
            session_id,
            workspace.turn_count,
            path,
            &new_value,
            Some(previous_value.clone()),
            source,
        );
    }
    Ok(outcome)
}

fn session_tool_output(value: Value) -> String {
    value.to_string()
}

struct ConfigMutationReservation {
    config: Arc<StdMutex<SessionConfigInner>>,
    turn: u32,
    state: ConfigMutationReservationState,
}

#[derive(Clone, Copy)]
enum ConfigMutationReservationState {
    Pending,
    DurablyCommitted,
    Compensated,
    Superseded,
    OutcomeUnknown,
}

#[derive(Clone, Copy)]
enum ConfigMutationSettlement {
    Committed,
    Compensated,
    Superseded,
    OutcomeUnknown,
}

fn with_config_governor<T>(
    config: &StdMutex<SessionConfigInner>,
    operation: &'static str,
    f: impl FnOnce(&mut SessionConfigInner) -> T,
) -> T {
    match config.lock() {
        Ok(mut inner) => f(&mut inner),
        Err(poisoned) => {
            tracing::warn!(
                operation,
                "session config governor mutex poisoned; recovering"
            );
            f(&mut poisoned.into_inner())
        }
    }
}

fn reserve_config_mutation(
    config: &Arc<StdMutex<SessionConfigInner>>,
    turn: u32,
    force: bool,
    max_mutations: u32,
) -> Result<ConfigMutationReservation, Value> {
    with_config_governor(config, "reserve_config_mutation", |inner| {
        if inner.mutation_counter.0 != turn {
            inner.mutation_counter = (turn, 0);
        }
        if !force && inner.mutation_counter.1 >= max_mutations {
            return Err(json!({
                "error": "mutation_limit_exceeded",
                "turn": turn,
                "max_mutations_per_turn": max_mutations,
                "hint": "Set force=true to override governor once.",
            }));
        }
        inner.mutation_counter.1 = inner.mutation_counter.1.saturating_add(1);
        Ok(ConfigMutationReservation {
            config: config.clone(),
            turn,
            state: ConfigMutationReservationState::Pending,
        })
    })
}

impl ConfigMutationReservation {
    fn mark_durably_committed(&mut self) {
        debug_assert!(matches!(
            self.state,
            ConfigMutationReservationState::Pending
        ));
        self.state = ConfigMutationReservationState::DurablyCommitted;
    }

    fn finish(mut self, settlement: ConfigMutationSettlement) -> u32 {
        self.state = match settlement {
            ConfigMutationSettlement::Committed => {
                debug_assert!(matches!(
                    self.state,
                    ConfigMutationReservationState::DurablyCommitted
                ));
                ConfigMutationReservationState::DurablyCommitted
            }
            ConfigMutationSettlement::Compensated => ConfigMutationReservationState::Compensated,
            ConfigMutationSettlement::Superseded => ConfigMutationReservationState::Superseded,
            ConfigMutationSettlement::OutcomeUnknown => {
                ConfigMutationReservationState::OutcomeUnknown
            }
        };
        with_config_governor(&self.config, "finalize_config_mutation", |inner| {
            if inner.mutation_counter.0 != self.turn {
                return 0;
            }
            if matches!(
                self.state,
                ConfigMutationReservationState::Compensated
                    | ConfigMutationReservationState::Superseded
            ) {
                inner.mutation_counter.1 = inner.mutation_counter.1.saturating_sub(1);
            }
            inner.mutation_counter.1
        })
    }
}

impl Drop for ConfigMutationReservation {
    fn drop(&mut self) {
        if !matches!(self.state, ConfigMutationReservationState::Pending) {
            return;
        }
        with_config_governor(&self.config, "drop_config_mutation_reservation", |inner| {
            if inner.mutation_counter.0 != self.turn {
                return;
            }
            inner.mutation_counter.1 = inner.mutation_counter.1.saturating_sub(1);
        });
    }
}

pub(crate) async fn execute_adjust_config<PublishWorkspace, PublishFuture>(
    user_id: String,
    session_id: String,
    observability_session: Option<Arc<RwLock<crate::observability::ObservabilitySession>>>,
    config: Arc<StdMutex<SessionConfigInner>>,
    args: Value,
    publish_workspace: PublishWorkspace,
    session_state_journal: Arc<StdMutex<SessionStateRollbackJournal>>,
    journal_turn_index: u32,
) -> String
where
    PublishWorkspace:
        Fn(astra_services::session_workspace::WorkspaceMetadata) -> PublishFuture + Send + 'static,
    PublishFuture: Future<Output = Result<(), String>> + Send + 'static,
{
    let path = match args.get("path").and_then(Value::as_str) {
        Some(path) if !path.trim().is_empty() => path.trim().to_string(),
        _ => return session_tool_output(json!({"error": "Missing required parameter: path"})),
    };
    let value = match args.get("value") {
        Some(value) => value.clone(),
        None => return session_tool_output(json!({"error": "Missing required parameter: value"})),
    };
    let force = args.get("force").and_then(Value::as_bool).unwrap_or(false);

    let Some(observability_session) = observability_session else {
        return session_tool_output(json!({"error": "No observability session available"}));
    };

    let constraints = crate::self_model::ConstraintSet::default();
    let turn = {
        let session = match observability_session.read() {
            Ok(guard) => guard,
            Err(_) => {
                return session_tool_output(
                    json!({"error": "Failed to acquire observability session"}),
                );
            }
        };

        session.turn_number
    };
    let mut reservation =
        match reserve_config_mutation(&config, turn, force, constraints.max_mutations_per_turn) {
            Ok(reservation) => reservation,
            Err(error) => return session_tool_output(error),
        };

    let mutation = match commit_config_mutation(&session_id, &path, None, |config| {
        astra_config::apply_governed_config_mutation(
            config,
            &path,
            &value,
            force,
            constraints.config_drift_ceiling,
        )
        .map_err(|error| ConfigMutationRejection::Invalid(error.to_json()))
    }) {
        Ok(ConfigMutationOutcome::Applied {
            value,
            revision,
            workspace,
        }) => (value, revision, workspace),
        Ok(ConfigMutationOutcome::Rejected(ConfigMutationRejection::Invalid(error))) => {
            drop(reservation);
            return session_tool_output(error);
        }
        Ok(ConfigMutationOutcome::Rejected(ConfigMutationRejection::RevisionConflict {
            ..
        })) => {
            unreachable!("an initial authority-based config mutation has no expected revision")
        }
        Ok(ConfigMutationOutcome::OutcomeUnknown {
            value: mutation,
            revision,
            observed,
            reason,
        }) => {
            let observed_revision = observed
                .as_ref()
                .map(|workspace| workspace.config_mutation_revision);
            let rollback_recorded = record_outcome_unknown_config_rollback(
                &session_state_journal,
                journal_turn_index,
                &path,
                &mutation,
                revision,
                observed.as_deref(),
            );
            if let Some(workspace) = observed.as_ref()
                && let Ok(config) = effective_runtime_config(Some(workspace))
                && let Ok(mut session) = observability_session.write()
            {
                session.config = config;
            }
            reservation.finish(ConfigMutationSettlement::OutcomeUnknown);
            return session_tool_output(json!({
                "error": "config_commit_outcome_unknown",
                "path": path,
                "proposed_revision": revision,
                "observed_revision": observed_revision,
                "detail": bounded_warning(reason),
                "side_effects_maybe": true,
                "owner_confirmed": rollback_recorded,
                "rollback_recorded": rollback_recorded,
                "audit_recorded": false,
            }));
        }
        Err(error) => {
            drop(reservation);
            return session_tool_output(json!({
                "error": "failed_to_persist_config_override",
                "path": path,
                "detail": error,
            }));
        }
    };
    let (mutation, revision, committed_workspace) = mutation;
    reservation.mark_durably_committed();
    if let Ok(mut session) = observability_session.write() {
        session.config = (*mutation.committed_config).clone();
    }
    let audit_warning = append_config_mutation_audit(
        &user_id,
        &session_id,
        &path,
        &mutation,
        "runtime_tool_executor:adjust_config",
    );

    let settlement = tokio::spawn(async move {
        if let Err(error) = publish_workspace(*committed_workspace).await {
            let compensation =
                commit_config_mutation(&session_id, &path, Some(revision), |config| {
                    *config = (*mutation.previous_config).clone();
                    Ok(RuntimeConfigUpdate {
                        path: mutation.update.path,
                        old_value: mutation.update.new_value.clone(),
                        new_value: mutation.update.old_value.clone(),
                        drift: None,
                    })
                });
            return match compensation {
                Ok(ConfigMutationOutcome::Applied {
                    value: compensation,
                    revision: compensation_revision,
                    workspace,
                }) => {
                    if let Ok(mut session) = observability_session.write() {
                        session.config = (*compensation.committed_config).clone();
                    }
                    let compensation_audit_warning = append_config_mutation_audit(
                        &user_id,
                        &session_id,
                        &path,
                        &compensation,
                        "runtime_tool_executor:adjust_config:publish_rollback",
                    );
                    reservation.finish(ConfigMutationSettlement::Compensated);
                    let remote_restore = publish_workspace(*workspace).await;
                    let remote_outcome_unknown = remote_restore.is_err();
                    session_tool_output(json!({
                        "error": "failed_to_publish_workspace_artifact",
                        "path": path,
                        "detail": error,
                        "persisted_config_restored": true,
                        "restored_revision": compensation_revision,
                        "remote_projection_restored": !remote_outcome_unknown,
                        "remote_outcome_unknown": remote_outcome_unknown,
                        "remote_restore_detail": remote_restore.err().map(bounded_warning),
                        "retryable": remote_outcome_unknown,
                        "audit_recorded": audit_warning.is_none(),
                        "audit_warning": audit_warning.as_deref(),
                        "compensation_audit_recorded": compensation_audit_warning.is_none(),
                        "compensation_audit_warning": compensation_audit_warning.as_deref(),
                    }))
                }
                Ok(ConfigMutationOutcome::Rejected(
                    ConfigMutationRejection::RevisionConflict {
                        current_revision,
                        current_value,
                        current_config,
                        current_workspace,
                    },
                )) => {
                    if let Ok(mut session) = observability_session.write() {
                        session.config = *current_config;
                    }
                    reservation.finish(ConfigMutationSettlement::Superseded);
                    let remote_current = publish_workspace(*current_workspace).await;
                    let remote_outcome_unknown = remote_current.is_err();
                    session_tool_output(json!({
                        "error": "failed_to_publish_workspace_artifact",
                        "path": path,
                        "detail": error,
                        "persisted_config_restored": false,
                        "concurrent_value_preserved": true,
                        "current": current_value,
                        "current_revision": current_revision,
                        "remote_projection_current": !remote_outcome_unknown,
                        "remote_outcome_unknown": remote_outcome_unknown,
                        "remote_current_detail": remote_current.err().map(bounded_warning),
                        "retryable": remote_outcome_unknown,
                        "audit_recorded": audit_warning.is_none(),
                        "audit_warning": audit_warning.as_deref(),
                    }))
                }
                Ok(ConfigMutationOutcome::Rejected(ConfigMutationRejection::Invalid(_))) => {
                    unreachable!("compensation does not perform value validation")
                }
                Ok(ConfigMutationOutcome::OutcomeUnknown {
                    revision,
                    observed,
                    reason,
                    ..
                }) => {
                    let observed_revision = observed
                        .as_ref()
                        .map(|workspace| workspace.config_mutation_revision);
                    if let Some(workspace) = observed.as_ref()
                        && let Ok(config) = effective_runtime_config(Some(workspace))
                        && let Ok(mut session) = observability_session.write()
                    {
                        session.config = config;
                    }
                    reservation.finish(ConfigMutationSettlement::OutcomeUnknown);
                    session_tool_output(json!({
                        "error": "failed_to_publish_workspace_artifact",
                        "path": path,
                        "detail": error,
                        "persisted_config_restored": Value::Null,
                        "compensation_outcome_unknown": true,
                        "compensation_revision": revision,
                        "observed_revision": observed_revision,
                        "compensation_detail": bounded_warning(reason),
                        "side_effects_maybe": true,
                        "recovery_recorded": false,
                        "audit_recorded": audit_warning.is_none(),
                        "audit_warning": audit_warning.as_deref(),
                    }))
                }
                Err(compensation_error) => {
                    // The initial commit is still authoritative. Do not create an
                    // unconditional recovery action that could overwrite a newer
                    // writer when replayed later.
                    if let Ok(mut session) = observability_session.write() {
                        session.config = (*mutation.committed_config).clone();
                    }
                    reservation.finish(ConfigMutationSettlement::Committed);
                    session_tool_output(json!({
                        "error": "failed_to_publish_workspace_artifact",
                        "path": path,
                        "detail": error,
                        "persisted_config_restored": false,
                        "compensation_error": bounded_warning(compensation_error),
                        "recovery_recorded": false,
                        "audit_recorded": audit_warning.is_none(),
                        "audit_warning": audit_warning.as_deref(),
                    }))
                }
            };
        }

        let mutations_this_turn = reservation.finish(ConfigMutationSettlement::Committed);

        let old_value = mutation.update.old_value.clone();
        tool_session_state_rollback::record(
            &session_state_journal,
            journal_turn_index,
            format!("adjust_config:{path}"),
            SessionStateRollbackAction::ConfigOverride {
                path: path.to_string(),
                old_value: mutation.update.old_value,
                expected_revision: Arc::new(std::sync::atomic::AtomicU64::new(revision)),
            },
        );
        json!({
            "status": "ok",
            "path": path,
            "old": old_value,
            "new": mutation.update.new_value,
            "turn": turn,
            "mutations_this_turn": mutations_this_turn,
            "max_mutations_per_turn": constraints.max_mutations_per_turn,
            "drift": mutation.update.drift,
            "drift_ceiling": constraints.config_drift_ceiling,
            "config_revision": revision,
            "audit_recorded": audit_warning.is_none(),
            "audit_warning": audit_warning.as_deref(),
        })
        .to_string()
    });
    match settlement.await {
        Ok(output) => output,
        Err(error) => session_tool_output(json!({
            "error": "config_settlement_task_failed",
            "detail": bounded_warning(error),
            "side_effects_maybe": true,
        })),
    }
}

pub(crate) fn persist_manual_compression(
    user_id: &str,
    session_id: &str,
    turn: u32,
    reason: &str,
    source: &str,
) -> Result<(), String> {
    let writer = astra_services::session_journal::JournalWriter::for_user(user_id, session_id)
        .map_err(|e| e.to_string())?;
    let mut event = astra_services::session_journal::JournalEvent::compact_with_summary(
        Some(session_id),
        turn,
        1,
        0,
        Some(reason),
    );
    event.metadata = Some(json!({
        "source": source,
        "reason": reason,
        "manual": true,
    }));
    writer.append(&event).map_err(|e| e.to_string())
}

pub(crate) fn execute_compress_context(
    user_id: &str,
    session_id: &str,
    observability_session: Option<&Arc<RwLock<crate::observability::ObservabilitySession>>>,
    args: &Value,
    session_state_journal: &StdMutex<SessionStateRollbackJournal>,
    journal_turn_index: u32,
) -> String {
    let reason = args
        .get("reason")
        .and_then(Value::as_str)
        .unwrap_or("manual_request");
    let Some(observability_session) = observability_session else {
        return session_tool_output(json!({"error": "No observability session available"}));
    };
    let mut session = match observability_session.write() {
        Ok(guard) => guard,
        Err(_) => {
            return session_tool_output(
                json!({"error": "Failed to acquire observability session"}),
            );
        }
    };
    let session_snapshot = session.rollback_snapshot();

    let turn = if session.turn_number == 0 {
        1
    } else {
        session.turn_number
    };
    let previous_compression_count = session.compressed_turns.len();
    let already_compressed_this_turn = session.compressed_turns.contains(&turn);

    if let Err(error) = persist_manual_compression(
        user_id,
        session_id,
        turn,
        reason,
        "runtime_tool_executor:compress_context",
    ) {
        return session_tool_output(json!({
            "error": "failed_to_persist_manual_compression",
            "detail": error,
            "turn": turn,
            "reason": reason,
        }));
    }

    session.record_compression(turn);
    let compression_count = session.compressed_turns.len();

    tool_session_state_rollback::record(
        session_state_journal,
        journal_turn_index,
        format!("compress_context:turn-{turn}"),
        SessionStateRollbackAction::Compression {
            turn,
            snapshot: Box::new(session_snapshot),
        },
    );

    json!({
        "status": "ok",
        "turn": turn,
        "reason": reason,
        "previous_compression_count": previous_compression_count,
        "already_compressed_this_turn": already_compressed_this_turn,
        "compression_count": compression_count,
    })
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEST_USER: &str = "test-user";
    const TOP_K_PATH: &str = "memory.retrieval_top_k";

    fn set_config_for_test(session_id: &str, value: Value) -> Result<(), String> {
        let expected_revision = astra_services::session_workspace::read_workspace(session_id)
            .map_err(|error| error.to_string())?
            .config_mutation_revision;
        match restore_config_override(
            TEST_USER,
            session_id,
            TOP_K_PATH,
            value,
            expected_revision,
            "test:config_writer",
        )? {
            ConfigRestoreOutcome::Applied { .. } => Ok(()),
            ConfigRestoreOutcome::Rejected {
                current_revision, ..
            } => Err(format!("test config revision conflict: {current_revision}")),
            ConfigRestoreOutcome::OutcomeUnknown { reason, .. } => Err(reason),
        }
    }

    struct ConfigFixture {
        _journal_guard: astra_services::session_journal::JournalDirGuard,
        _sessions: tempfile::TempDir,
        session_id: &'static str,
        session: Arc<RwLock<crate::observability::ObservabilitySession>>,
        config: Arc<StdMutex<SessionConfigInner>>,
        rollback_journal: Arc<StdMutex<SessionStateRollbackJournal>>,
        baseline: u32,
    }

    impl ConfigFixture {
        fn new(session_id: &'static str, turn: u32) -> Self {
            let sessions = tempfile::TempDir::new().expect("sessions tempdir");
            let journal_guard =
                astra_services::session_journal::JournalDirGuard::new(sessions.path());
            astra_services::session_workspace::write_workspace(
                &astra_services::session_workspace::WorkspaceMetadata::new(
                    session_id,
                    "test-model",
                ),
            )
            .expect("workspace");
            let session = Arc::new(RwLock::new(
                crate::observability::ObservabilitySession::new_simple(session_id),
            ));
            let baseline = {
                let mut session = session.write().expect("session write");
                session.turn_number = turn;
                session.config.memory.retrieval_top_k
            };
            Self {
                _journal_guard: journal_guard,
                _sessions: sessions,
                session_id,
                session,
                config: Arc::new(StdMutex::new(SessionConfigInner::default())),
                rollback_journal: Arc::new(StdMutex::new(SessionStateRollbackJournal::default())),
                baseline,
            }
        }

        fn alternate(&self) -> u32 {
            (1..=20)
                .find(|value| *value != self.baseline)
                .expect("alternate top-k")
        }

        fn persisted_top_k(&self) -> u32 {
            effective_runtime_config(Some(&self.workspace()))
                .expect("effective config")
                .memory
                .retrieval_top_k
        }

        fn workspace(&self) -> astra_services::session_workspace::WorkspaceMetadata {
            astra_services::session_workspace::read_workspace(self.session_id)
                .expect("workspace read")
        }

        fn observed_top_k(&self) -> u32 {
            self.session
                .read()
                .expect("session read")
                .config
                .memory
                .retrieval_top_k
        }

        async fn adjust<Publish, Published>(&self, value: u32, publish: Publish) -> Value
        where
            Publish: Fn(astra_services::session_workspace::WorkspaceMetadata) -> Published
                + Send
                + 'static,
            Published: Future<Output = Result<(), String>> + Send + 'static,
        {
            serde_json::from_str(
                &execute_adjust_config(
                    TEST_USER.to_string(),
                    self.session_id.to_string(),
                    Some(self.session.clone()),
                    self.config.clone(),
                    json!({"path": TOP_K_PATH, "value": value, "force": true}),
                    publish,
                    self.rollback_journal.clone(),
                    0,
                )
                .await,
            )
            .expect("config output")
        }
    }

    #[tokio::test]
    async fn execute_compress_context_records_and_replays_canonical_rollback() {
        let sessions = tempfile::TempDir::new().expect("sessions tempdir");
        let _journal_guard = astra_services::session_journal::JournalDirGuard::new(sessions.path());
        let session_id = "sess-compress-context";
        let session = Arc::new(RwLock::new(
            crate::observability::ObservabilitySession::new_simple(session_id),
        ));
        session.write().expect("session write").turn_number = 3;

        let journal = StdMutex::new(SessionStateRollbackJournal::default());
        let output = execute_compress_context(
            "test-user",
            session_id,
            Some(&session),
            &json!({"reason": "manual_test"}),
            &journal,
            0,
        );
        let output: Value = serde_json::from_str(&output).expect("json output");

        assert_eq!(output["status"], "ok");
        assert_eq!(output["turn"], 3);
        assert_eq!(output["reason"], "manual_test");
        assert_eq!(output["previous_compression_count"], 0);
        assert_eq!(output["compression_count"], 1);
        assert!(
            session
                .read()
                .expect("session read")
                .compressed_turns
                .contains(&3)
        );
        let rollback_entries = tool_session_state_rollback::entries(&journal);
        assert_eq!(rollback_entries.len(), 1);
        match &rollback_entries[0].action {
            SessionStateRollbackAction::Compression { turn, snapshot } => {
                assert_eq!(*turn, 3);
                assert!(snapshot.compressed_turns.is_empty());
            }
            other => panic!("expected compression rollback, got {other:?}"),
        }
        tool_session_state_rollback::restore_entry(
            &tool_session_state_rollback::SessionStateRestoreContext {
                user_id: "test-user".into(),
                session_id: session_id.into(),
                observability_session: Some(session.clone()),
            },
            &rollback_entries[0],
            None,
        )
        .await
        .expect("replay compression rollback");
        assert!(
            !session
                .read()
                .expect("session read after rollback")
                .compressed_turns
                .contains(&3)
        );

        let events =
            astra_services::session_journal::read_journal_for_user("test-user", session_id)
                .expect("compression journal");
        assert!(events.iter().any(|event| {
            event.event_type == astra_services::session_journal::JournalEventType::Compact
                && event.turn == Some(3)
        }));
    }

    #[tokio::test]
    async fn execute_adjust_config_updates_and_replays_canonical_rollback() {
        let fixture = ConfigFixture::new("sess-adjust-config", 5);
        let old_top_k = fixture.alternate();
        set_config_for_test(fixture.session_id, json!(old_top_k)).unwrap();
        let new_top_k = (1..=20).find(|value| *value != old_top_k).unwrap();

        let output = fixture.adjust(new_top_k, |_| async { Ok(()) }).await;
        assert_eq!(output["old"], old_top_k);
        assert_eq!(output["new"], new_top_k);
        let rollback_entries = tool_session_state_rollback::entries(&fixture.rollback_journal);
        assert_eq!(rollback_entries.len(), 1);
        match &rollback_entries[0].action {
            SessionStateRollbackAction::ConfigOverride {
                path,
                old_value,
                expected_revision,
            } => {
                assert_eq!(path, TOP_K_PATH);
                assert_eq!(old_value, &json!(old_top_k));
                assert_eq!(
                    expected_revision.load(std::sync::atomic::Ordering::Relaxed),
                    2
                );
            }
            other => panic!("expected config rollback, got {other:?}"),
        }
        tool_session_state_rollback::restore_entry(
            &tool_session_state_rollback::SessionStateRestoreContext {
                user_id: TEST_USER.into(),
                session_id: fixture.session_id.into(),
                observability_session: Some(fixture.session.clone()),
            },
            &rollback_entries[0],
            None,
        )
        .await
        .expect("replay config rollback");
        assert_eq!(fixture.persisted_top_k(), old_top_k);
        assert_eq!(fixture.observed_top_k(), old_top_k);
    }

    #[cfg(feature = "e2e-hooks")]
    #[tokio::test(flavor = "current_thread")]
    #[serial_test::serial]
    async fn post_rename_sync_unknown_records_rollback_for_exact_owner() {
        let fixture = ConfigFixture::new("sess-adjust-config-sync-unknown", 6);
        let changed = fixture.alternate();
        astra_services::session_workspace::inject_workspace_commit_parent_sync_failure_once();

        let output = fixture.adjust(changed, |_| async { Ok(()) }).await;

        assert_eq!(output["error"], "config_commit_outcome_unknown");
        assert_eq!(output["proposed_revision"], 1);
        assert_eq!(output["observed_revision"], 1);
        assert_eq!(output["owner_confirmed"], true);
        assert_eq!(output["rollback_recorded"], true);
        assert_eq!(output["side_effects_maybe"], true);
        assert_eq!(fixture.persisted_top_k(), changed);
        assert_eq!(fixture.observed_top_k(), changed);
        assert_eq!(fixture.config.lock().unwrap().mutation_counter, (6, 1));
        let entries = tool_session_state_rollback::entries(&fixture.rollback_journal);
        assert_eq!(entries.len(), 1);
        match &entries[0].action {
            SessionStateRollbackAction::ConfigOverride {
                path,
                old_value,
                expected_revision,
            } => {
                assert_eq!(path, TOP_K_PATH);
                assert_eq!(old_value, &json!(fixture.baseline));
                assert_eq!(
                    expected_revision.load(std::sync::atomic::Ordering::Relaxed),
                    1
                );
            }
            other => panic!("expected config rollback, got {other:?}"),
        }
    }

    #[test]
    fn outcome_unknown_without_exact_owner_records_no_rollback() {
        let previous = astra_config::RuntimeConfig::load();
        let mut committed = previous.clone();
        committed.memory.retrieval_top_k = (1..=20)
            .find(|value| *value != previous.memory.retrieval_top_k)
            .unwrap();
        let candidate_json = Some(serde_json::to_string(&committed).unwrap());
        let mutation = DurableConfigMutation {
            update: RuntimeConfigUpdate {
                path: astra_config::GovernedConfigPath::RetrievalTopK,
                old_value: json!(previous.memory.retrieval_top_k),
                new_value: json!(committed.memory.retrieval_top_k),
                drift: None,
            },
            previous_config: Box::new(previous),
            committed_config: Box::new(committed),
            committed_tuned_config_json: candidate_json.clone(),
            turn: 1,
        };
        let journal = StdMutex::new(SessionStateRollbackJournal::default());
        assert!(!record_outcome_unknown_config_rollback(
            &journal, 0, TOP_K_PATH, &mutation, 1, None,
        ));

        let mut conflicting = astra_services::session_workspace::WorkspaceMetadata::new(
            "sess-conflicting-config-owner",
            "test-model",
        );
        conflicting.config_mutation_revision = 1;
        conflicting.tuned_config_json = Some("{}".to_string());
        assert!(!record_outcome_unknown_config_rollback(
            &journal,
            0,
            TOP_K_PATH,
            &mutation,
            1,
            Some(&conflicting),
        ));
        conflicting.config_mutation_revision = 2;
        conflicting.tuned_config_json = candidate_json;
        assert!(!record_outcome_unknown_config_rollback(
            &journal,
            0,
            TOP_K_PATH,
            &mutation,
            1,
            Some(&conflicting),
        ));
        assert!(tool_session_state_rollback::entries(&journal).is_empty());
    }

    #[tokio::test]
    async fn execute_adjust_config_restores_persisted_state_when_publish_fails() {
        let fixture = ConfigFixture::new("sess-adjust-config-publish-failure", 7);
        let published = Arc::new(StdMutex::new(Vec::new()));

        let output = fixture
            .adjust(fixture.alternate(), {
                let published = published.clone();
                move |workspace| {
                    let published = published.clone();
                    async move {
                        let top_k = effective_runtime_config(Some(&workspace))?
                            .memory
                            .retrieval_top_k;
                        let mut published = published.lock().unwrap();
                        published.push(top_k);
                        if published.len() == 1 {
                            Err("publish committed before response failed".to_string())
                        } else {
                            Ok(())
                        }
                    }
                }
            })
            .await;

        assert_eq!(output["error"], "failed_to_publish_workspace_artifact");
        assert_eq!(output["persisted_config_restored"], true);
        assert_eq!(output["remote_projection_restored"], true);
        assert_eq!(fixture.persisted_top_k(), fixture.baseline);
        assert_eq!(
            published.lock().unwrap().as_slice(),
            &[fixture.alternate(), fixture.baseline]
        );
    }

    #[tokio::test]
    async fn compensated_reservation_is_released_before_remote_restore_panics() {
        let fixture = ConfigFixture::new("sess-adjust-config-restore-panic", 7);
        let publish_calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let output = fixture
            .adjust(fixture.alternate(), {
                let publish_calls = publish_calls.clone();
                move |_| {
                    let call = publish_calls.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    async move {
                        if call == 0 {
                            Err("initial publish failed".to_string())
                        } else {
                            panic!("restored projection publisher panic")
                        }
                    }
                }
            })
            .await;

        assert_eq!(output["error"], "config_settlement_task_failed");
        assert_eq!(fixture.persisted_top_k(), fixture.baseline);
        assert_eq!(fixture.config.lock().unwrap().mutation_counter, (7, 0));
    }

    #[tokio::test]
    async fn superseded_reservation_is_released_before_current_publish_panics() {
        let fixture = ConfigFixture::new("sess-adjust-config-current-panic", 7);
        let changed = fixture.alternate();
        let concurrent = (1..=20)
            .find(|value| *value != fixture.baseline && *value != changed)
            .unwrap();
        let publish_calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let output = fixture
            .adjust(changed, {
                let publish_calls = publish_calls.clone();
                move |_| {
                    let call = publish_calls.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    async move {
                        if call == 0 {
                            set_config_for_test(
                                "sess-adjust-config-current-panic",
                                json!(concurrent),
                            )?;
                            Err("initial publish failed".to_string())
                        } else {
                            panic!("current projection publisher panic")
                        }
                    }
                }
            })
            .await;

        assert_eq!(output["error"], "config_settlement_task_failed");
        assert_eq!(fixture.persisted_top_k(), concurrent);
        assert_eq!(fixture.config.lock().unwrap().mutation_counter, (7, 0));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn owned_config_settlement_does_not_hold_governor_lock() {
        let fixture = ConfigFixture::new("sess-adjust-config-cancelled-waiter", 8);
        let changed = fixture.alternate();
        let second_changed = (1..=20)
            .find(|value| *value != fixture.baseline && *value != changed)
            .unwrap();
        let publish_started = Arc::new(tokio::sync::Notify::new());
        let release_publish = Arc::new(tokio::sync::Notify::new());
        let publish_done = Arc::new(tokio::sync::Notify::new());
        let task = tokio::spawn(execute_adjust_config(
            TEST_USER.to_string(),
            fixture.session_id.to_string(),
            Some(fixture.session.clone()),
            fixture.config.clone(),
            json!({"path": TOP_K_PATH, "value": changed, "force": true}),
            {
                let publish_started = publish_started.clone();
                let release_publish = release_publish.clone();
                let publish_done = publish_done.clone();
                move |_| {
                    let publish_started = publish_started.clone();
                    let release_publish = release_publish.clone();
                    let publish_done = publish_done.clone();
                    async move {
                        publish_started.notify_one();
                        release_publish.notified().await;
                        publish_done.notify_one();
                        Ok(())
                    }
                }
            },
            fixture.rollback_journal.clone(),
            0,
        ));

        if tokio::time::timeout(
            std::time::Duration::from_secs(1),
            publish_started.notified(),
        )
        .await
        .is_err()
        {
            assert!(
                task.is_finished(),
                "config mutation stalled before publication"
            );
            let output = task.await.expect("config mutation task");
            panic!("config mutation ended before publication: {output}");
        }
        let second = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            fixture.adjust(second_changed, |_| async { Ok(()) }),
        )
        .await
        .expect("a pending remote publication must not retain the config governor mutex");
        assert_eq!(second["status"], "ok");
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        release_publish.notify_one();
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            publish_done.notified().await;
            while tool_session_state_rollback::entries(&fixture.rollback_journal).len() < 2 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("owned settlement must finish after its caller is dropped");

        assert_eq!(fixture.config.lock().unwrap().mutation_counter, (8, 2));
    }

    #[tokio::test]
    async fn publisher_panic_preserves_durable_value_and_consumed_governor_slot() {
        let fixture = ConfigFixture::new("sess-adjust-config-publisher-panic", 4);
        let changed = fixture.alternate();
        let constraints = crate::self_model::ConstraintSet::default();
        fixture.config.lock().unwrap().mutation_counter =
            (4, constraints.max_mutations_per_turn.saturating_sub(1));

        let output: Value = serde_json::from_str(
            &execute_adjust_config(
                TEST_USER.to_string(),
                fixture.session_id.to_string(),
                Some(fixture.session.clone()),
                fixture.config.clone(),
                json!({"path": TOP_K_PATH, "value": changed, "force": true}),
                |_| async { panic!("injected publisher panic") },
                fixture.rollback_journal.clone(),
                0,
            )
            .await,
        )
        .expect("config output");
        assert_eq!(output["error"], "config_settlement_task_failed");
        assert_eq!(fixture.persisted_top_k(), changed);
        assert_eq!(
            fixture.config.lock().unwrap().mutation_counter,
            (4, constraints.max_mutations_per_turn)
        );

        let rejected: Value = serde_json::from_str(
            &execute_adjust_config(
                TEST_USER.to_string(),
                fixture.session_id.to_string(),
                Some(fixture.session.clone()),
                fixture.config.clone(),
                json!({"path": TOP_K_PATH, "value": fixture.baseline}),
                |_| async { Ok(()) },
                fixture.rollback_journal.clone(),
                0,
            )
            .await,
        )
        .expect("config output");
        assert_eq!(rejected["error"], "mutation_limit_exceeded");
        assert_eq!(fixture.persisted_top_k(), changed);
    }

    #[tokio::test]
    async fn compensation_io_failure_keeps_authority_and_records_no_unsafe_replay() {
        let fixture = ConfigFixture::new("sess-adjust-config-compensation-io", 3);
        let changed = fixture.alternate();
        let lock_path = astra_services::session_workspace::workspace_dir_for(fixture.session_id)
            .join(".workspace.lock");

        let output = fixture
            .adjust(changed, move |_| {
                let lock_path = lock_path.clone();
                async move {
                    std::fs::remove_file(&lock_path).map_err(|error| error.to_string())?;
                    std::fs::create_dir(&lock_path).map_err(|error| error.to_string())?;
                    Err("publish unavailable".to_string())
                }
            })
            .await;

        assert_eq!(output["persisted_config_restored"], false);
        assert_eq!(output["recovery_recorded"], false);
        assert_eq!(fixture.persisted_top_k(), changed);
        assert_eq!(fixture.observed_top_k(), changed);
        assert_eq!(fixture.config.lock().unwrap().mutation_counter, (3, 1));
        assert!(tool_session_state_rollback::entries(&fixture.rollback_journal).is_empty());
    }

    #[tokio::test]
    async fn adjust_config_reports_audit_failure_without_rolling_back_commit() {
        let fixture = ConfigFixture::new("sess-config-audit-failure", 0);
        let changed = fixture.alternate();
        let journal_path = astra_services::session_journal::journal_file_path_for_user(
            TEST_USER,
            fixture.session_id,
        )
        .expect("journal path");
        std::fs::create_dir_all(&journal_path).expect("block journal file with a directory");

        let output = fixture.adjust(changed, |_| async { Ok(()) }).await;

        assert_eq!(output["status"], "ok");
        assert_eq!(output["audit_recorded"], false);
        assert!(
            output["audit_warning"]
                .as_str()
                .is_some_and(|warning| !warning.is_empty())
        );
        assert_eq!(fixture.persisted_top_k(), changed);
        assert_eq!(fixture.observed_top_k(), changed);
    }

    #[tokio::test]
    async fn publish_compensation_token_rejects_distinct_same_value_and_aba_writers() {
        let fixture = ConfigFixture::new("sess-adjust-config-aba-writer", 0);
        let failed_value = fixture.alternate();
        let concurrent_value = (1..=20)
            .find(|value| *value != fixture.baseline && *value != failed_value)
            .expect("second alternate value");
        let publish_calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));

        let output = fixture
            .adjust(failed_value, move |_| {
                let publish_call = publish_calls.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                async move {
                    if publish_call > 0 {
                        return Ok(());
                    }
                    // A -> A -> B -> A covers same-value and ABA writers.
                    for value in [failed_value, concurrent_value, failed_value] {
                        set_config_for_test(fixture.session_id, json!(value))?;
                    }
                    Err("publish unavailable".to_string())
                }
            })
            .await;

        assert_eq!(output["persisted_config_restored"], false);
        assert_eq!(output["concurrent_value_preserved"], true);
        assert_eq!(output["current"], failed_value);
        assert_eq!(output["current_revision"], 4);
        assert_eq!(output["remote_projection_current"], true);
        assert!(tool_session_state_rollback::entries(&fixture.rollback_journal).is_empty());
        assert_eq!(fixture.persisted_top_k(), failed_value);
        assert_eq!(fixture.observed_top_k(), failed_value);
        assert_eq!(fixture.workspace().config_mutation_revision, 4);
    }
}
