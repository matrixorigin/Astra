use astra_runtime::self_model::ConstraintSet;
use serde_json::{Value, json};

use super::ToolExecutor;

impl ToolExecutor {
    #[allow(unused_assignments)]
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
        let mut session = match obs.write() {
            Ok(g) => g,
            Err(_) => {
                return json!({"error": "Failed to acquire observability session"}).to_string();
            }
        };

        let turn = session.turn_number;
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
        let session_snapshot = session.rollback_snapshot();
        let old_value: Option<Value>;
        let new_value: Option<Value>;
        let mut drift: Option<f64> = None;

        let parse_u32 = |v: &Value| v.as_u64().and_then(|n| u32::try_from(n).ok());
        let parse_f64 = |v: &Value| v.as_f64();

        match path {
            "compression.compression_threshold" => {
                let Some(new) = parse_f64(value) else {
                    return json!({"error": "value must be a number"}).to_string();
                };
                if !(0.5..=0.98).contains(&new) {
                    return json!({"error": "compression.compression_threshold must be within [0.5, 0.98]"}).to_string();
                }
                let old = session.config.compression.compression_threshold;
                if let Some(d) = bounded_drift(old, new, 0.5, 0.98) {
                    if !force && d > ceiling {
                        return json!({
                            "error": "config_drift_ceiling_exceeded",
                            "path": path,
                            "old": old,
                            "new": new,
                            "drift": d,
                            "ceiling": ceiling
                        })
                        .to_string();
                    }
                    drift = Some(d);
                }
                session.config.compression.compression_threshold = new;
                old_value = Some(json!(old));
                new_value = Some(json!(new));
            }
            "memory.retrieval_top_k" => {
                let Some(new) = parse_u32(value) else {
                    return json!({"error": "value must be an integer"}).to_string();
                };
                if !(1..=20).contains(&new) {
                    return json!({"error": "memory.retrieval_top_k must be within [1, 20]"})
                        .to_string();
                }
                let old = session.config.memory.retrieval_top_k;
                if let Some(d) = bounded_drift(old as f64, new as f64, 1.0, 20.0) {
                    if !force && d > ceiling {
                        return json!({
                            "error": "config_drift_ceiling_exceeded",
                            "path": path,
                            "old": old,
                            "new": new,
                            "drift": d,
                            "ceiling": ceiling
                        })
                        .to_string();
                    }
                    drift = Some(d);
                }
                session.config.memory.retrieval_top_k = new;
                old_value = Some(json!(old));
                new_value = Some(json!(new));
            }
            "tool_policy.max_tools" => {
                let Some(new) = parse_u32(value) else {
                    return json!({"error": "value must be an integer"}).to_string();
                };
                if !(5..=80).contains(&new) {
                    return json!({"error": "tool_policy.max_tools must be within [5, 80]"})
                        .to_string();
                }
                let old = session.config.tool_policy.max_tools;
                if let Some(d) = bounded_drift(old as f64, new as f64, 5.0, 80.0) {
                    if !force && d > ceiling {
                        return json!({
                            "error": "config_drift_ceiling_exceeded",
                            "path": path,
                            "old": old,
                            "new": new,
                            "drift": d,
                            "ceiling": ceiling
                        })
                        .to_string();
                    }
                    drift = Some(d);
                }
                session.config.tool_policy.max_tools = new;
                old_value = Some(json!(old));
                new_value = Some(json!(new));
            }
            "token_budget.max_turn_input_tokens" => {
                let Some(new) = parse_u32(value) else {
                    return json!({"error": "value must be an integer"}).to_string();
                };
                if !(8_000..=200_000).contains(&new) {
                    return json!({"error": "token_budget.max_turn_input_tokens must be within [8000, 200000]"}).to_string();
                }
                let old = session.config.token_budget.max_turn_input_tokens;
                if let Some(d) = bounded_drift(old as f64, new as f64, 8_000.0, 200_000.0) {
                    if !force && d > ceiling {
                        return json!({
                            "error": "config_drift_ceiling_exceeded",
                            "path": path,
                            "old": old,
                            "new": new,
                            "drift": d,
                            "ceiling": ceiling
                        })
                        .to_string();
                    }
                    drift = Some(d);
                }
                session.config.token_budget.max_turn_input_tokens = new;
                old_value = Some(json!(old));
                new_value = Some(json!(new));
            }
            "token_budget.tools_reserve" => {
                let Some(new) = parse_u32(value) else {
                    return json!({"error": "value must be an integer"}).to_string();
                };
                if !(1_000..=40_000).contains(&new) {
                    return json!({"error": "token_budget.tools_reserve must be within [1000, 40000]"}).to_string();
                }
                let old = session.config.token_budget.tools_reserve;
                if let Some(d) = bounded_drift(old as f64, new as f64, 1_000.0, 40_000.0) {
                    if !force && d > ceiling {
                        return json!({
                            "error": "config_drift_ceiling_exceeded",
                            "path": path,
                            "old": old,
                            "new": new,
                            "drift": d,
                            "ceiling": ceiling
                        })
                        .to_string();
                    }
                    drift = Some(d);
                }
                session.config.token_budget.tools_reserve = new;
                old_value = Some(json!(old));
                new_value = Some(json!(new));
            }
            "verification.strictness" => {
                let Some(new) = parse_f64(value) else {
                    return json!({"error": "value must be a number"}).to_string();
                };
                if !(0.2..=0.95).contains(&new) {
                    return json!({"error": "verification.strictness must be within [0.2, 0.95]"})
                        .to_string();
                }
                let old = session.config.verification.strictness;
                if let Some(d) = bounded_drift(old, new, 0.2, 0.95) {
                    if !force && d > ceiling {
                        return json!({
                            "error": "config_drift_ceiling_exceeded",
                            "path": path,
                            "old": old,
                            "new": new,
                            "drift": d,
                            "ceiling": ceiling
                        })
                        .to_string();
                    }
                    drift = Some(d);
                }
                session.config.verification.strictness = new;
                old_value = Some(json!(old));
                new_value = Some(json!(new));
            }
            _ => {
                return json!({
                    "error": "Unsupported config path",
                    "path": path,
                    "supported_paths": [
                        "compression.compression_threshold",
                        "memory.retrieval_top_k",
                        "tool_policy.max_tools",
                        "token_budget.max_turn_input_tokens",
                        "token_budget.tools_reserve",
                        "verification.strictness"
                    ]
                })
                .to_string();
            }
        }

        if let Some(session_id) = self.active_session_id()
            && let Some(ref persisted_value) = new_value
            && let Err(error) = crate::cli::self_command::persist_config_override(
                &session_id,
                path,
                persisted_value.clone(),
            )
        {
            session.restore_rollback_snapshot(&session_snapshot);
            return json!({
                "error": "failed_to_persist_config_override",
                "path": path,
                "detail": error,
            })
            .to_string();
        }

        counter.1 += 1;
        let path_owned = path.to_string();
        if let Some(old_value) = old_value.clone() {
            self.record_adjust_config_rollback(path_owned, old_value, session_snapshot);
        }
        json!({
            "status": "completed",
            "path": path,
            "old": old_value.unwrap_or(Value::Null),
            "new": new_value.unwrap_or(Value::Null),
            "turn": turn,
            "mutations_this_turn": counter.1,
            "max_mutations_per_turn": constraints.max_mutations_per_turn,
            "drift": drift,
            "drift_ceiling": ceiling
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

fn bounded_drift(old: f64, new: f64, min: f64, max: f64) -> Option<f64> {
    let span = max - min;
    if span <= f64::EPSILON {
        return None;
    }
    Some((new - old).abs() / span)
}
