use std::future::Future;
use std::sync::{Arc, Mutex as StdMutex, RwLock};

use serde_json::{Value, json};
use tokio::sync::Mutex;

use crate::server::runtime_tool_executor::SessionConfigInner;
use crate::server::tool_session_state_rollback::{
    self, SessionStateRollbackAction, SessionStateRollbackJournal,
};

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct RuntimeConfigUpdate {
    pub(crate) old_value: Value,
    pub(crate) new_value: Value,
    pub(crate) drift: Option<f64>,
}

pub(crate) fn normalized_drift(old: f64, new: f64) -> Option<f64> {
    if !old.is_finite() || !new.is_finite() {
        return None;
    }
    let denom = old.abs().max(new.abs());
    if denom < f64::EPSILON {
        return Some(0.0);
    }

    Some((new - old).abs() / denom)
}

fn parse_u32(value: &Value) -> Option<u32> {
    value.as_u64().and_then(|number| {
        u32::try_from(number)
            .inspect_err(|e| {
                tracing::warn!("config_drift: u64->u32 overflow converting {number}: {e}");
            })
            .ok()
    })
}

fn parse_f64(value: &Value) -> Option<f64> {
    value.as_f64()
}

fn drift_ceiling_error(
    path: &str,
    old: impl serde::Serialize,
    new: impl serde::Serialize,
    drift: f64,
    ceiling: f64,
) -> Value {
    json!({
        "error": "config_drift_ceiling_exceeded",
        "path": path,
        "old": old,
        "new": new,
        "drift": drift,
        "ceiling": ceiling,
    })
}

fn check_drift(
    path: &str,
    old: impl serde::Serialize,
    new: impl serde::Serialize,
    drift: Option<f64>,
    force: bool,
    ceiling: f64,
) -> Result<(), Value> {
    if let Some(drift_value) = drift
        && !force
        && drift_value > ceiling
    {
        return Err(drift_ceiling_error(path, old, new, drift_value, ceiling));
    }
    Ok(())
}

pub(crate) fn apply_runtime_config_update(
    config: &mut astra_config::runtime_config::RuntimeConfig,
    path: &str,
    value: &Value,
    force: bool,
    ceiling: f64,
) -> Result<RuntimeConfigUpdate, Value> {
    match path {
        "compression.compression_threshold" => {
            let Some(new) = parse_f64(value) else {
                return Err(json!({"error": "value must be a number"}));
            };
            if !(0.5..=0.98).contains(&new) {
                return Err(
                    json!({"error": "compression.compression_threshold must be within [0.5, 0.98]"}),
                );
            }
            let old = config.compression.compression_threshold;
            let drift = normalized_drift(old, new);
            check_drift(path, old, new, drift, force, ceiling)?;
            config.compression.compression_threshold = new;
            Ok(RuntimeConfigUpdate {
                old_value: json!(old),
                new_value: json!(new),
                drift,
            })
        }
        "memory.retrieval_top_k" => {
            let Some(new) = parse_u32(value) else {
                return Err(json!({"error": "value must be an integer"}));
            };
            if !(1..=20).contains(&new) {
                return Err(json!({"error": "memory.retrieval_top_k must be within [1, 20]"}));
            }
            let old = config.memory.retrieval_top_k;
            let drift = normalized_drift(old as f64, new as f64);
            check_drift(path, old, new, drift, force, ceiling)?;
            config.memory.retrieval_top_k = new;
            Ok(RuntimeConfigUpdate {
                old_value: json!(old),
                new_value: json!(new),
                drift,
            })
        }
        "token_budget.max_turn_input_tokens" => {
            let Some(new) = parse_u32(value) else {
                return Err(json!({"error": "value must be an integer"}));
            };
            if !(8_000..=200_000).contains(&new) {
                return Err(
                    json!({"error": "token_budget.max_turn_input_tokens must be within [8000, 200000]"}),
                );
            }
            let old = config.token_budget.max_turn_input_tokens;
            let drift = normalized_drift(old as f64, new as f64);
            check_drift(path, old, new, drift, force, ceiling)?;
            config.token_budget.max_turn_input_tokens = new;
            Ok(RuntimeConfigUpdate {
                old_value: json!(old),
                new_value: json!(new),
                drift,
            })
        }
        "token_budget.tools_reserve" => {
            let Some(new) = parse_u32(value) else {
                return Err(json!({"error": "value must be an integer"}));
            };
            if !(1_000..=40_000).contains(&new) {
                return Err(
                    json!({"error": "token_budget.tools_reserve must be within [1000, 40000]"}),
                );
            }
            let old = config.token_budget.tools_reserve;
            let drift = normalized_drift(old as f64, new as f64);
            check_drift(path, old, new, drift, force, ceiling)?;
            config.token_budget.tools_reserve = new;
            Ok(RuntimeConfigUpdate {
                old_value: json!(old),
                new_value: json!(new),
                drift,
            })
        }
        "verification.strictness" => {
            let Some(new) = parse_f64(value) else {
                return Err(json!({"error": "value must be a number"}));
            };
            if !(0.2..=0.95).contains(&new) {
                return Err(json!({"error": "verification.strictness must be within [0.2, 0.95]"}));
            }
            let old = config.verification.strictness;
            let drift = normalized_drift(old, new);
            check_drift(path, old, new, drift, force, ceiling)?;
            config.verification.strictness = new;
            Ok(RuntimeConfigUpdate {
                old_value: json!(old),
                new_value: json!(new),
                drift,
            })
        }
        _ => Err(json!({
            "error": "Unsupported config path",
            "path": path,
            "supported_paths": [
                "compression.compression_threshold",
                "memory.retrieval_top_k",
                "token_budget.max_turn_input_tokens",
                "token_budget.tools_reserve",
                "verification.strictness",
            ],
        })),
    }
}

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

pub(crate) enum ConfigRestoreOutcome {
    Applied {
        config: Box<astra_config::RuntimeConfig>,
        revision: u64,
    },
    Rejected {
        current_revision: u64,
    },
    OutcomeUnknown {
        revision: u64,
        reason: String,
        observed_config: Option<Box<astra_config::RuntimeConfig>>,
        retry_revision: Option<u64>,
    },
}

pub(crate) fn restore_config_override(
    user_id: &str,
    session_id: &str,
    path: &str,
    new_value: Value,
    expected_revision: u64,
    source: &str,
) -> Result<ConfigRestoreOutcome, String> {
    let outcome = commit_config_mutation(session_id, path, Some(expected_revision), |config| {
        let mut value = serde_json::to_value(&*config).map_err(|error| {
            ConfigMutationRejection::Invalid(json!({"error": error.to_string()}))
        })?;
        let old_value =
            astra_config::replace_existing_json_path(&mut value, path, new_value.clone()).map_err(
                |error| ConfigMutationRejection::Invalid(json!({"error": error.to_string()})),
            )?;
        *config = serde_json::from_value(value).map_err(|error| {
            ConfigMutationRejection::Invalid(json!({"error": error.to_string()}))
        })?;
        Ok(RuntimeConfigUpdate {
            old_value,
            new_value,
            drift: None,
        })
    })?;
    match outcome {
        ConfigMutationOutcome::Applied {
            value, revision, ..
        } => {
            append_config_mutation_audit(user_id, session_id, path, &value, source);
            Ok(ConfigRestoreOutcome::Applied {
                config: value.committed_config,
                revision,
            })
        }
        ConfigMutationOutcome::Rejected(ConfigMutationRejection::Invalid(error)) => {
            Err(error.to_string())
        }
        ConfigMutationOutcome::Rejected(ConfigMutationRejection::RevisionConflict {
            current_revision,
            ..
        }) => Ok(ConfigRestoreOutcome::Rejected { current_revision }),
        ConfigMutationOutcome::OutcomeUnknown {
            value,
            revision,
            observed,
            reason,
        } => {
            let retry_revision = observed
                .as_ref()
                .filter(|workspace| {
                    workspace.config_mutation_revision == revision
                        && workspace.tuned_config_json == value.committed_tuned_config_json
                })
                .map(|_| revision);
            let observed_config = observed
                .as_deref()
                .and_then(|workspace| effective_runtime_config(Some(workspace)).ok())
                .map(Box::new);
            Ok(ConfigRestoreOutcome::OutcomeUnknown {
                revision,
                reason,
                observed_config,
                retry_revision,
            })
        }
    }
}

fn session_tool_output(value: Value) -> String {
    value.to_string()
}

pub(crate) async fn execute_adjust_config<PublishWorkspace, PublishFuture>(
    user_id: String,
    session_id: String,
    observability_session: Option<Arc<RwLock<crate::observability::ObservabilitySession>>>,
    config: Arc<Mutex<SessionConfigInner>>,
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

    let mut inner = config.lock_owned().await;

    let turn = {
        let session = match observability_session.read() {
            Ok(guard) => guard,
            Err(_) => {
                return session_tool_output(
                    json!({"error": "Failed to acquire observability session"}),
                );
            }
        };

        let turn = session.turn_number;
        if inner.mutation_counter.0 != turn {
            inner.mutation_counter = (turn, 0);
        }
        if !force && inner.mutation_counter.1 >= constraints.max_mutations_per_turn {
            return session_tool_output(json!({
                "error": "mutation_limit_exceeded",
                "turn": turn,
                "max_mutations_per_turn": constraints.max_mutations_per_turn,
                "hint": "Set force=true to override governor once.",
            }));
        }

        turn
    };

    let mutation = match commit_config_mutation(&session_id, &path, None, |config| {
        apply_runtime_config_update(
            config,
            &path,
            &value,
            force,
            constraints.config_drift_ceiling,
        )
        .map_err(ConfigMutationRejection::Invalid)
    }) {
        Ok(ConfigMutationOutcome::Applied {
            value,
            revision,
            workspace,
        }) => (value, revision, workspace),
        Ok(ConfigMutationOutcome::Rejected(ConfigMutationRejection::Invalid(error))) => {
            return session_tool_output(error);
        }
        Ok(ConfigMutationOutcome::Rejected(ConfigMutationRejection::RevisionConflict {
            ..
        })) => {
            unreachable!("an initial authority-based config mutation has no expected revision")
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
            return session_tool_output(json!({
                "error": "config_commit_outcome_unknown",
                "path": path,
                "proposed_revision": revision,
                "observed_revision": observed_revision,
                "detail": bounded_warning(reason),
                "side_effects_maybe": true,
                "audit_recorded": false,
            }));
        }
        Err(error) => {
            return session_tool_output(json!({
                "error": "failed_to_persist_config_override",
                "path": path,
                "detail": error,
            }));
        }
    };
    let (mutation, revision, committed_workspace) = mutation;
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
                    inner.mutation_counter.1 += 1;
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

        inner.mutation_counter.1 += 1;
        let inner_mutation_increment = inner.mutation_counter.1;

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
            "mutations_this_turn": inner_mutation_increment,
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
            ConfigRestoreOutcome::Rejected { current_revision } => {
                Err(format!("test config revision conflict: {current_revision}"))
            }
            ConfigRestoreOutcome::OutcomeUnknown { reason, .. } => Err(reason),
        }
    }

    struct ConfigFixture {
        _journal_guard: astra_services::session_journal::JournalDirGuard,
        _sessions: tempfile::TempDir,
        session_id: &'static str,
        session: Arc<RwLock<crate::observability::ObservabilitySession>>,
        config: Arc<Mutex<SessionConfigInner>>,
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
                config: Arc::new(Mutex::new(SessionConfigInner::default())),
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
    async fn committed_config_settlement_survives_outer_future_cancellation() {
        let fixture = ConfigFixture::new("sess-adjust-config-cancelled-waiter", 8);
        let changed = fixture.alternate();
        let publish_started = Arc::new(tokio::sync::Notify::new());
        let release_publish = Arc::new(tokio::sync::Notify::new());
        let task = tokio::spawn(execute_adjust_config(
            TEST_USER.to_string(),
            fixture.session_id.to_string(),
            Some(fixture.session.clone()),
            fixture.config.clone(),
            json!({"path": TOP_K_PATH, "value": changed, "force": true}),
            {
                let publish_started = publish_started.clone();
                let release_publish = release_publish.clone();
                move |_| {
                    let publish_started = publish_started.clone();
                    let release_publish = release_publish.clone();
                    async move {
                        publish_started.notify_one();
                        release_publish.notified().await;
                        Ok(())
                    }
                }
            },
            fixture.rollback_journal.clone(),
            0,
        ));

        publish_started.notified().await;
        task.abort();
        release_publish.notify_one();
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            while tool_session_state_rollback::entries(&fixture.rollback_journal).is_empty() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("owned settlement must finish after its caller is dropped");

        assert_eq!(fixture.config.lock().await.mutation_counter, (8, 1));
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
        assert_eq!(fixture.config.lock().await.mutation_counter, (3, 1));
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
