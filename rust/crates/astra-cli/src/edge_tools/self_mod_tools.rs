use astra_runtime::self_model::ConstraintSet;
use astra_runtime::turn::goal_tracker::GoalTracker;
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
        let mut old_value: Option<Value>;
        let mut new_value: Option<Value>;
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
                if let Some(d) = normalized_drift(old, new) {
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
                if let Some(d) = normalized_drift(old as f64, new as f64) {
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
            "tool_selection.max_tools" => {
                let Some(new) = parse_u32(value) else {
                    return json!({"error": "value must be an integer"}).to_string();
                };
                if !(5..=80).contains(&new) {
                    return json!({"error": "tool_selection.max_tools must be within [5, 80]"})
                        .to_string();
                }
                let old = session.config.tool_selection.max_tools;
                if let Some(d) = normalized_drift(old as f64, new as f64) {
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
                session.config.tool_selection.max_tools = new;
                old_value = Some(json!(old));
                new_value = Some(json!(new));
            }
            "tool_selection.tool_budget_tokens" => {
                let Some(new) = parse_u32(value) else {
                    return json!({"error": "value must be an integer"}).to_string();
                };
                if new > 40_000 {
                    return json!({"error": "tool_selection.tool_budget_tokens must be within [0, 40000]"}).to_string();
                }
                let old = session.config.tool_selection.tool_budget_tokens;
                if let Some(d) = normalized_drift(old as f64, new as f64) {
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
                session.config.tool_selection.tool_budget_tokens = new;
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
                if let Some(d) = normalized_drift(old as f64, new as f64) {
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
                if let Some(d) = normalized_drift(old as f64, new as f64) {
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
                if let Some(d) = normalized_drift(old, new) {
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
                        "tool_selection.max_tools",
                        "tool_selection.tool_budget_tokens",
                        "token_budget.max_turn_input_tokens",
                        "token_budget.tools_reserve",
                        "verification.strictness"
                    ]
                })
                .to_string();
            }
        }

        counter.1 += 1;
        json!({
            "status": "ok",
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

    pub(super) fn prioritize_tool(&self, args: &Value) -> String {
        let Some(tool) = extract_tool_name(args) else {
            return json!({"error": "Missing required parameter: tool"}).to_string();
        };
        if !self.tool_names().iter().any(|t| t == &tool) {
            return json!({"error": format!("Unknown tool: {tool}")}).to_string();
        }

        let mut pinned = match self.self_mod_pinned_tools.lock() {
            Ok(v) => v,
            Err(_) => return json!({"error": "Failed to access pinned tools"}).to_string(),
        };
        let mut deprioritized = match self.self_mod_deprioritized_tools.lock() {
            Ok(v) => v,
            Err(_) => return json!({"error": "Failed to access deprioritized tools"}).to_string(),
        };

        if !pinned.contains(&tool) {
            pinned.push(tool.clone());
        }
        pinned.sort();
        deprioritized.retain(|t| t != &tool);

        json!({
            "status": "ok",
            "prioritized_tool": tool,
            "pinned_tools": pinned.clone(),
            "deprioritized_tools": deprioritized.clone()
        })
        .to_string()
    }

    pub(super) fn deprioritize_tool(&self, args: &Value) -> String {
        let Some(tool) = extract_tool_name(args) else {
            return json!({"error": "Missing required parameter: tool"}).to_string();
        };
        if !self.tool_names().iter().any(|t| t == &tool) {
            return json!({"error": format!("Unknown tool: {tool}")}).to_string();
        }

        let mut pinned = match self.self_mod_pinned_tools.lock() {
            Ok(v) => v,
            Err(_) => return json!({"error": "Failed to access pinned tools"}).to_string(),
        };
        let mut deprioritized = match self.self_mod_deprioritized_tools.lock() {
            Ok(v) => v,
            Err(_) => return json!({"error": "Failed to access deprioritized tools"}).to_string(),
        };

        if !deprioritized.contains(&tool) {
            deprioritized.push(tool.clone());
        }
        deprioritized.sort();
        pinned.retain(|t| t != &tool);

        json!({
            "status": "ok",
            "deprioritized_tool": tool,
            "pinned_tools": pinned.clone(),
            "deprioritized_tools": deprioritized.clone()
        })
        .to_string()
    }

    pub(super) fn set_goal(&self, args: &Value) -> String {
        let goal = match args.get("goal").and_then(Value::as_str) {
            Some(g) if !g.trim().is_empty() => g.trim(),
            _ => return json!({"error": "Missing required parameter: goal"}).to_string(),
        };
        let Some(obs) = self.observability_session.as_ref() else {
            return json!({"error": "No observability session available"}).to_string();
        };
        let mut session = match obs.write() {
            Ok(g) => g,
            Err(_) => {
                return json!({"error": "Failed to acquire observability session"}).to_string();
            }
        };

        session.goal_tracker = Some(GoalTracker::new(goal));
        session.original_query = Some(goal.to_string());

        json!({
            "status": "ok",
            "goal": goal,
            "turn": session.turn_number
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

        let turn = if session.turn_number == 0 {
            1
        } else {
            session.turn_number
        };
        session.record_compression(turn);

        json!({
            "status": "ok",
            "turn": turn,
            "reason": reason,
            "compression_count": session.compressed_turns.len()
        })
        .to_string()
    }
}

fn normalized_drift(old: f64, new: f64) -> Option<f64> {
    let denom = old.abs();
    if denom < f64::EPSILON {
        return None;
    }
    Some((new - old).abs() / denom)
}

fn extract_tool_name(args: &Value) -> Option<String> {
    args.get("tool")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(ToString::to_string)
}
