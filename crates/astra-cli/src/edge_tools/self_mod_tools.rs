use astra_runtime::self_model::ConstraintSet;
use serde_json::{Value, json};

use super::ToolExecutor;

impl ToolExecutor {
    pub(super) fn adjust_config(&self, args: &Value) -> String {
        let path = match args.get("path").and_then(Value::as_str) {
            Some(p) if !p.trim().is_empty() => p.trim(),
            _ => return json!({"error": "Missing required parameter: path"}).to_string(),
        };
        let value = match args.get("value") {
            Some(v) => v,
            None => return json!({"error": "Missing required parameter: value"}).to_string(),
        };
        let force = args.get("force").and_then(Value::as_bool).unwrap_or(false);

        let Some(obs) = self.observability_session.as_ref() else {
            return json!({"error": "No observability session available"}).to_string();
        };
        let (turn, session_snapshot, observed_config) = {
            let session = match obs.read() {
                Ok(session) => session,
                Err(poisoned) => poisoned.into_inner(),
            };
            (
                session.turn_number,
                session.rollback_snapshot(),
                session.config.clone(),
            )
        };
        let constraints = ConstraintSet::default();
        let mut counter = match self.self_mod_mutation_counter.lock() {
            Ok(c) => c,
            Err(_) => return json!({"error": "Failed to access mutation counter"}).to_string(),
        };
        if counter.0 != turn {
            *counter = (turn, 0);
        }
        if !force && counter.1 >= constraints.max_mutations_per_turn {
            return json!({
                "error": "mutation_limit_exceeded",
                "turn": turn,
                "max_mutations_per_turn": constraints.max_mutations_per_turn,
                "hint": "Set force=true to override governor once."
            })
            .to_string();
        }

        let ceiling = constraints.config_drift_ceiling;
        let active_session_id = self.active_session_id();
        let (old_value, new_value, drift, durable_revision, audit_recorded, audit_warning) =
            if let Some(session_id) = active_session_id.as_deref() {
                let (receipt, drift) =
                    match crate::cli::self_command::persist_governed_config_override(
                        session_id,
                        path,
                        value.clone(),
                        force,
                        ceiling,
                    ) {
                        Ok(receipt) => receipt,
                        Err(crate::cli::self_command::GovernedConfigMutationError::Rejected(
                            rejection,
                        )) => return rejection.to_string(),
                        Err(
                            crate::cli::self_command::GovernedConfigMutationError::Persistence(
                                error,
                            ),
                        ) => {
                            return json!({
                                "error": "failed_to_persist_config_override",
                                "path": path,
                                "detail": error,
                            })
                            .to_string();
                        }
                        Err(
                            crate::cli::self_command::GovernedConfigMutationError::OutcomeUnknown(
                                unknown,
                            ),
                        ) => {
                            let projection_warning = match unknown.observed_config {
                                Some(config) => match obs.write() {
                                    Ok(mut session) => {
                                        session.config = config;
                                        None
                                    }
                                    Err(_) => Some(
                                        "observability config projection lock poisoned".to_string(),
                                    ),
                                },
                                None => Some("workspace config readback unavailable".to_string()),
                            };
                            counter.1 += 1;
                            let rollback_recorded = unknown.retry_revision.is_some();
                            if let Some(owner_revision) = unknown.retry_revision {
                                self.record_adjust_config_rollback(
                                    path.to_string(),
                                    unknown.preview.old_value.clone(),
                                    session_snapshot,
                                    Some(owner_revision),
                                );
                            }
                            return json!({
                                "error": "config_commit_outcome_unknown",
                                "path": path,
                                "old": unknown.preview.old_value,
                                "new": unknown.preview.new_value,
                                "drift": unknown.drift,
                                "proposed_revision": unknown.proposed_revision,
                                "observed_revision": unknown.observed_revision,
                                "retry_revision": unknown.retry_revision,
                                "side_effects_maybe": true,
                                "mutations_this_turn": counter.1,
                                "rollback_recorded": rollback_recorded,
                                "audit_recorded": false,
                                "projection_recorded": projection_warning.is_none(),
                                "projection_warning": projection_warning,
                                "detail": unknown.reason.chars().take(240).collect::<String>(),
                            })
                            .to_string();
                        }
                    };
                let projection_warning = match obs.write() {
                    Ok(mut session) => {
                        session.config = receipt.committed_config;
                        None
                    }
                    Err(_) => Some("observability config projection lock poisoned"),
                };
                counter.1 += 1;
                self.record_adjust_config_rollback(
                    path.to_string(),
                    receipt.preview.old_value.clone(),
                    session_snapshot,
                    Some(receipt.config_revision),
                );
                return json!({
                    "status": "completed",
                    "path": path,
                    "old": receipt.preview.old_value,
                    "new": receipt.preview.new_value,
                    "turn": turn,
                    "mutations_this_turn": counter.1,
                    "max_mutations_per_turn": constraints.max_mutations_per_turn,
                    "drift": drift,
                    "drift_ceiling": ceiling,
                    "config_revision": receipt.config_revision,
                    "audit_recorded": receipt.audit_recorded,
                    "audit_warning": receipt.audit_warning,
                    "projection_recorded": projection_warning.is_none(),
                    "projection_warning": projection_warning,
                })
                .to_string();
            } else {
                let (preview, candidate_config, drift) =
                    match crate::cli::self_command::prepare_governed_config_mutation(
                        "ephemeral",
                        path,
                        value.clone(),
                        &observed_config,
                        force,
                        ceiling,
                    ) {
                        Ok(prepared) => prepared,
                        Err(rejection) => return rejection.to_string(),
                    };
                let mut session = match obs.write() {
                    Ok(session) => session,
                    Err(_) => {
                        return json!({"error": "Failed to acquire observability session"})
                            .to_string();
                    }
                };
                session.config = candidate_config;
                (
                    preview.old_value,
                    preview.new_value,
                    drift,
                    None,
                    true,
                    None::<String>,
                )
            };

        counter.1 += 1;
        self.record_adjust_config_rollback(
            path.to_string(),
            old_value.clone(),
            session_snapshot,
            durable_revision,
        );
        json!({
            "status": "completed",
            "path": path,
            "old": old_value,
            "new": new_value,
            "turn": turn,
            "mutations_this_turn": counter.1,
            "max_mutations_per_turn": constraints.max_mutations_per_turn,
            "drift": drift,
            "drift_ceiling": ceiling,
            "audit_recorded": audit_recorded,
            "audit_warning": audit_warning,
            "projection_recorded": true,
        })
        .to_string()
    }

    pub(super) fn compress_context(&self, args: &Value) -> String {
        let reason = args
            .get("reason")
            .and_then(Value::as_str)
            .unwrap_or("manual_request");
        let Some(obs) = self.observability_session.as_ref() else {
            return json!({"error": "No observability session available"}).to_string();
        };
        let mut session = match obs.write() {
            Ok(g) => g,
            Err(_) => {
                return json!({"error": "Failed to acquire observability session"}).to_string();
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

        if let Some(session_id) = self.active_session_id()
            && let Err(error) =
                crate::cli::self_command::persist_manual_compression(&session_id, turn, reason)
        {
            return json!({
                "error": "failed_to_persist_manual_compression",
                "detail": error,
                "turn": turn,
                "reason": reason,
            })
            .to_string();
        }

        session.record_compression(turn);
        self.record_compression_rollback(turn, session_snapshot);

        json!({
            "status": "completed",
            "turn": turn,
            "reason": reason,
            "previous_compression_count": previous_compression_count,
            "already_compressed_this_turn": already_compressed_this_turn,
            "compression_count": session.compressed_turns.len()
        })
        .to_string()
    }
}
