use std::sync::{Arc, Mutex, RwLock};

use serde_json::{Value, json};

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

pub(crate) fn replace_json_path(
    root: &mut Value,
    path: &str,
    new_value: Value,
) -> Result<Value, String> {
    let segments: Vec<&str> = path
        .split('.')
        .filter(|segment| !segment.is_empty())
        .collect();
    if segments.is_empty() {
        return Err("mutation path cannot be empty".to_string());
    }

    let mut current = root;
    for segment in &segments[..segments.len() - 1] {
        current = current
            .get_mut(*segment)
            .ok_or_else(|| format!("unknown config path segment '{segment}'"))?;
    }

    let Some(last) = segments.last() else {
        return Err("mutation path cannot be empty".to_string());
    };
    let object = current
        .as_object_mut()
        .ok_or_else(|| format!("config path '{path}' does not point to an object parent"))?;
    let slot = object
        .get_mut(*last)
        .ok_or_else(|| format!("unknown config leaf '{last}'"))?;
    let old_value = slot.clone();
    *slot = new_value;
    Ok(old_value)
}

pub(crate) fn append_config_change_event(
    session_id: &str,
    turn: u32,
    key: &str,
    new_value: &Value,
    old_value: Option<Value>,
    source: &str,
) -> Result<(), String> {
    let writer = astra_services::session_journal::JournalWriter::new(session_id)
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

pub(crate) fn persist_config_override(
    session_id: &str,
    path: &str,
    new_value: Value,
    source: &str,
) -> Result<(), String> {
    let mut workspace =
        astra_services::session_workspace::read_workspace(session_id).map_err(|e| e.to_string())?;
    let base_config = effective_runtime_config(Some(&workspace))?;
    let mut value = serde_json::to_value(&base_config).map_err(|e| e.to_string())?;
    let old_value = replace_json_path(&mut value, path, new_value.clone())?;
    let candidate_config: astra_config::runtime_config::RuntimeConfig =
        serde_json::from_value(value.clone()).map_err(|e| e.to_string())?;
    let baseline_json = serde_json::to_value(astra_config::runtime_config::RuntimeConfig::load())
        .map_err(|e| e.to_string())?;
    workspace.tuned_config_json = if value == baseline_json {
        None
    } else {
        Some(serde_json::to_string(&candidate_config).map_err(|e| e.to_string())?)
    };
    workspace.updated_at = chrono::Utc::now().to_rfc3339();
    astra_services::session_workspace::write_workspace(&workspace).map_err(|e| e.to_string())?;
    append_config_change_event(
        session_id,
        workspace.turn_count,
        path,
        &new_value,
        Some(old_value),
        source,
    )
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub(crate) struct AdjustConfigRollback {
    pub(crate) path: String,
    pub(crate) old_value: Value,
    pub(crate) snapshot: crate::observability::ObservabilitySessionRollbackSnapshot,
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub(crate) struct AdjustConfigOutcome {
    pub(crate) output: String,
    pub(crate) rollback: Option<AdjustConfigRollback>,
}

fn adjust_config_output(value: Value) -> AdjustConfigOutcome {
    AdjustConfigOutcome {
        output: value.to_string(),
        rollback: None,
    }
}

pub(crate) fn execute_adjust_config<PublishWorkspace>(
    session_id: &str,
    observability_session: Option<&Arc<RwLock<crate::observability::ObservabilitySession>>>,
    config: &Mutex<SessionConfigInner>,
    args: &Value,
    publish_workspace: PublishWorkspace,
    session_state_journal: &Mutex<SessionStateRollbackJournal>,
    journal_turn_index: u32,
) -> AdjustConfigOutcome
where
    PublishWorkspace: FnOnce() -> Result<(), String>,
{
    use crate::server::runtime_tool_executor::SessionConfigInner;

    let path = match args.get("path").and_then(Value::as_str) {
        Some(path) if !path.trim().is_empty() => path.trim(),
        _ => return adjust_config_output(json!({"error": "Missing required parameter: path"})),
    };
    let value = match args.get("value") {
        Some(value) => value,
        None => return adjust_config_output(json!({"error": "Missing required parameter: value"})),
    };
    let force = args.get("force").and_then(Value::as_bool).unwrap_or(false);

    let Some(observability_session) = observability_session else {
        return adjust_config_output(json!({"error": "No observability session available"}));
    };

    let constraints = crate::self_model::ConstraintSet::default();

    // Phase 1: acquire locks, apply update, snapshot results, then release.
    let (turn, update, session_snapshot, mut inner_mutation_increment) = {
        // LOCK ORDER: observability_session → config.inner.
        let mut session = match observability_session.write() {
            Ok(guard) => guard,
            Err(_) => {
                return adjust_config_output(
                    json!({"error": "Failed to acquire observability session"}),
                );
            }
        };

        let turn = session.turn_number;

        let mut inner = match config.lock() {
            Ok(inner) => inner,
            Err(_) => {
                return adjust_config_output(json!({"error": "Failed to access session config"}));
            }
        };
        if inner.mutation_counter.0 != turn {
            inner.mutation_counter = (turn, 0);
        }
        if !force && inner.mutation_counter.1 >= constraints.max_mutations_per_turn {
            return adjust_config_output(json!({
                "error": "mutation_limit_exceeded",
                "turn": turn,
                "max_mutations_per_turn": constraints.max_mutations_per_turn,
                "hint": "Set force=true to override governor once.",
            }));
        }

        let ceiling = constraints.config_drift_ceiling;
        let session_snapshot = session.rollback_snapshot();
        let update =
            match apply_runtime_config_update(&mut session.config, path, value, force, ceiling) {
                Ok(update) => update,
                Err(error) => return adjust_config_output(error),
            };

        (turn, update, session_snapshot, 1u32)
    };

    // Phase 2: I/O outside locks.
    if let Err(error) = persist_config_override(
        session_id,
        path,
        update.new_value.clone(),
        "runtime_tool_executor:adjust_config",
    ) {
        // Rollback observability state.
        if let Ok(mut session) = observability_session.write() {
            session.restore_rollback_snapshot(&session_snapshot);
        }
        return adjust_config_output(json!({
            "error": "failed_to_persist_config_override",
            "path": path,
            "detail": error,
        }));
    }
    if let Err(error) = publish_workspace() {
        if let Ok(mut session) = observability_session.write() {
            session.restore_rollback_snapshot(&session_snapshot);
        }
        return adjust_config_output(json!({
            "error": "failed_to_publish_workspace_artifact",
            "path": path,
            "detail": error,
        }));
    }

    // Phase 3: I/O succeeded — finalize in-memory mutation counter.
    if let Ok(mut inner) = config.lock() {
        inner.mutation_counter.1 += 1;
        inner_mutation_increment = inner.mutation_counter.1;
    }

    let rollback = AdjustConfigRollback {
        path: path.to_string(),
        old_value: update.old_value.clone(),
        snapshot: session_snapshot.clone(),
    };
    tool_session_state_rollback::record(
        session_state_journal,
        journal_turn_index,
        format!("adjust_config:{path}"),
        SessionStateRollbackAction::ConfigOverride {
            path: path.to_string(),
            old_value: update.old_value,
            snapshot: session_snapshot,
        },
    );
    AdjustConfigOutcome {
        output: json!({
            "status": "ok",
            "path": path,
            "old": rollback.old_value,
            "new": update.new_value,
            "turn": turn,
            "mutations_this_turn": inner_mutation_increment,
            "max_mutations_per_turn": constraints.max_mutations_per_turn,
            "drift": update.drift,
            "drift_ceiling": constraints.config_drift_ceiling,
        })
        .to_string(),
        rollback: Some(rollback),
    }
}

pub(crate) fn persist_manual_compression(
    session_id: &str,
    turn: u32,
    reason: &str,
    source: &str,
) -> Result<(), String> {
    let writer = astra_services::session_journal::JournalWriter::new(session_id)
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

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub(crate) struct CompressContextRollback {
    pub(crate) turn: u32,
    pub(crate) snapshot: crate::observability::ObservabilitySessionRollbackSnapshot,
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub(crate) struct CompressContextOutcome {
    pub(crate) output: String,
    pub(crate) rollback: Option<CompressContextRollback>,
}

fn compress_context_output(value: Value) -> CompressContextOutcome {
    CompressContextOutcome {
        output: value.to_string(),
        rollback: None,
    }
}

pub(crate) fn execute_compress_context(
    session_id: &str,
    observability_session: Option<&Arc<RwLock<crate::observability::ObservabilitySession>>>,
    args: &Value,
    session_state_journal: &Mutex<SessionStateRollbackJournal>,
    journal_turn_index: u32,
) -> CompressContextOutcome {
    let reason = args
        .get("reason")
        .and_then(Value::as_str)
        .unwrap_or("manual_request");
    let Some(observability_session) = observability_session else {
        return compress_context_output(json!({"error": "No observability session available"}));
    };
    let mut session = match observability_session.write() {
        Ok(guard) => guard,
        Err(_) => {
            return compress_context_output(
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
        session_id,
        turn,
        reason,
        "runtime_tool_executor:compress_context",
    ) {
        return compress_context_output(json!({
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
            snapshot: session_snapshot.clone(),
        },
    );

    CompressContextOutcome {
        output: json!({
            "status": "ok",
            "turn": turn,
            "reason": reason,
            "previous_compression_count": previous_compression_count,
            "already_compressed_this_turn": already_compressed_this_turn,
            "compression_count": compression_count,
        })
        .to_string(),
        rollback: Some(CompressContextRollback {
            turn,
            snapshot: session_snapshot,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn execute_compress_context_records_compression_and_returns_rollback() {
        let sessions = tempfile::TempDir::new().expect("sessions tempdir");
        let _journal_guard = astra_services::session_journal::JournalDirGuard::new(sessions.path());
        let session_id = "sess-compress-context";
        let session = Arc::new(RwLock::new(
            crate::observability::ObservabilitySession::new_simple(session_id),
        ));
        session.write().expect("session write").turn_number = 3;

        let journal = Mutex::new(SessionStateRollbackJournal::default());
        let outcome = execute_compress_context(
            session_id,
            Some(&session),
            &json!({"reason": "manual_test"}),
            &journal,
            0,
        );
        let output: Value = serde_json::from_str(&outcome.output).expect("json output");

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
        let rollback = outcome.rollback.expect("successful compression rollback");
        assert_eq!(rollback.turn, 3);
        assert!(rollback.snapshot.compressed_turns.is_empty());

        let events =
            astra_services::session_journal::read_journal(session_id).expect("compression journal");
        assert!(events.iter().any(|event| {
            event.event_type == astra_services::session_journal::JournalEventType::Compact
                && event.turn == Some(3)
        }));
    }

    #[test]
    fn normalized_drift_is_symmetric_and_handles_zero_baseline() {
        assert_eq!(normalized_drift(0.0, 0.0), Some(0.0));
        assert_eq!(normalized_drift(0.0, 10.0), Some(1.0));
        assert_eq!(normalized_drift(10.0, 0.0), Some(1.0));
        assert_eq!(normalized_drift(f64::NAN, 1.0), None);
    }

    #[test]
    fn apply_runtime_config_update_mutates_supported_leaf_and_reports_delta() {
        let mut config = astra_config::runtime_config::RuntimeConfig::load();
        config.memory.retrieval_top_k = 5;

        let update = apply_runtime_config_update(
            &mut config,
            "memory.retrieval_top_k",
            &json!(6),
            false,
            1.0,
        )
        .expect("valid update");

        assert_eq!(config.memory.retrieval_top_k, 6);
        assert_eq!(update.old_value, json!(5));
        assert_eq!(update.new_value, json!(6));
        assert!(update.drift.is_some());
    }

    #[test]
    fn apply_runtime_config_update_rejects_type_range_and_unknown_path() {
        let mut config = astra_config::runtime_config::RuntimeConfig::load();

        let type_error = apply_runtime_config_update(
            &mut config,
            "memory.retrieval_top_k",
            &json!(1.2),
            false,
            1.0,
        )
        .expect_err("float top_k must fail");
        assert_eq!(type_error["error"], "value must be an integer");

        let range_error = apply_runtime_config_update(
            &mut config,
            "memory.retrieval_top_k",
            &json!(0),
            false,
            1.0,
        )
        .expect_err("out of range top_k must fail");
        assert_eq!(
            range_error["error"],
            "memory.retrieval_top_k must be within [1, 20]"
        );

        let unsupported =
            apply_runtime_config_update(&mut config, "unknown.path", &json!(1), false, 1.0)
                .expect_err("unknown path must fail");
        assert_eq!(unsupported["error"], "Unsupported config path");
        assert!(unsupported["supported_paths"].as_array().is_some());
    }

    #[test]
    fn apply_runtime_config_update_enforces_drift_ceiling_without_mutating() {
        let mut config = astra_config::runtime_config::RuntimeConfig::load();
        config.memory.retrieval_top_k = 5;

        let err = apply_runtime_config_update(
            &mut config,
            "memory.retrieval_top_k",
            &json!(20),
            false,
            0.1,
        )
        .expect_err("large drift must fail");

        assert_eq!(err["error"], "config_drift_ceiling_exceeded");
        assert_eq!(config.memory.retrieval_top_k, 5);
    }

    #[test]
    fn apply_runtime_config_update_force_overrides_drift_ceiling() {
        let mut config = astra_config::runtime_config::RuntimeConfig::load();
        config.memory.retrieval_top_k = 5;

        let update = apply_runtime_config_update(
            &mut config,
            "memory.retrieval_top_k",
            &json!(20),
            true,
            0.1,
        )
        .expect("force should allow drift");

        assert_eq!(config.memory.retrieval_top_k, 20);
        assert_eq!(update.old_value, json!(5));
        assert_eq!(update.new_value, json!(20));
    }

    #[test]
    fn execute_adjust_config_updates_session_counter_and_returns_rollback() {
        let sessions = tempfile::TempDir::new().expect("sessions tempdir");
        let _journal_guard = astra_services::session_journal::JournalDirGuard::new(sessions.path());
        let session_id = "sess-adjust-config";
        astra_services::session_workspace::write_workspace(
            &astra_services::session_workspace::WorkspaceMetadata::new(session_id, "test-model"),
        )
        .expect("workspace");
        let session = Arc::new(RwLock::new(
            crate::observability::ObservabilitySession::new_simple(session_id),
        ));
        let old_top_k = {
            let mut guard = session.write().expect("session write");
            guard.turn_number = 5;
            guard.config.memory.retrieval_top_k
        };
        let new_top_k = if old_top_k >= 20 {
            old_top_k - 1
        } else {
            old_top_k + 1
        };
        let config = Mutex::new(SessionConfigInner::default());

        let journal = Mutex::new(SessionStateRollbackJournal::default());
        let outcome = execute_adjust_config(
            session_id,
            Some(&session),
            &config,
            &json!({
                "path": "memory.retrieval_top_k",
                "value": new_top_k,
                "force": true,
            }),
            || Ok(()),
            &journal,
            0,
        );
        let output: Value = serde_json::from_str(&outcome.output).expect("json output");

        assert_eq!(output["status"], "ok");
        assert_eq!(output["path"], "memory.retrieval_top_k");
        assert_eq!(output["old"], json!(old_top_k));
        assert_eq!(output["new"], json!(new_top_k));
        assert_eq!(config.lock().expect("config").mutation_counter, (5, 1));
        assert_eq!(
            session
                .read()
                .expect("session read")
                .config
                .memory
                .retrieval_top_k,
            new_top_k
        );
        let rollback = outcome.rollback.expect("config rollback");
        assert_eq!(rollback.path, "memory.retrieval_top_k");
        assert_eq!(rollback.old_value, json!(old_top_k));
        assert_eq!(rollback.snapshot.config.memory.retrieval_top_k, old_top_k);
        let workspace =
            astra_services::session_workspace::read_workspace(session_id).expect("workspace read");
        assert!(workspace.tuned_config_json.is_some());
    }

    #[test]
    fn replace_json_path_mutates_leaf_and_returns_previous_value() {
        let mut value = json!({
            "memory": {
                "retrieval_top_k": 5
            }
        });

        let old = replace_json_path(&mut value, "memory.retrieval_top_k", json!(8)).unwrap();

        assert_eq!(old, json!(5));
        assert_eq!(value["memory"]["retrieval_top_k"], json!(8));
    }

    #[test]
    fn replace_json_path_reports_empty_unknown_and_non_object_paths() {
        let mut value = json!({"memory": {"retrieval_top_k": 5}});

        assert!(
            replace_json_path(&mut value, "", json!(8))
                .expect_err("empty path must fail")
                .contains("empty")
        );
        assert!(
            replace_json_path(&mut value, "memory.missing", json!(8))
                .expect_err("missing leaf must fail")
                .contains("unknown config leaf")
        );
        assert!(
            replace_json_path(&mut value, "memory.retrieval_top_k.value", json!(8))
                .expect_err("non-object parent must fail")
                .contains("does not point to an object parent")
        );
    }
}
