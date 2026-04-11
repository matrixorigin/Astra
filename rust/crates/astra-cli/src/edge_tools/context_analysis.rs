//! Context analysis tool — LLM-callable deep context window analysis.
//!
//! Provides programmatic access to:
//! - Per-turn context composition with detailed token proportions
//! - Multi-turn session analysis showing context evolution
//! - Component-level breakdown (system prompt, history, memory, tools, user message)
//! - Compression impact analysis and budget pressure trends

use serde_json::{Value, json};

use super::ToolExecutor;

impl ToolExecutor {
    /// Analyze context window composition and token distribution.
    ///
    /// Modes:
    /// - `turn` (default): Detailed breakdown for a specific turn
    /// - `session`: Multi-turn analysis across the entire session
    /// - `compare`: Compare two turns side-by-side
    pub(super) fn context_analysis(&self, args: &Value) -> String {
        let session_lock = match &self.observability_session {
            Some(s) => s,
            None => {
                return json!({
                    "error": "No observability session available. Start a conversation first."
                })
                .to_string();
            }
        };

        let session = match session_lock.read() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };

        let traces = &session.context_traces;
        if traces.is_empty() {
            return json!({
                "error": "No context assembly traces yet. Complete at least one turn."
            })
            .to_string();
        }

        let mode = args.get("mode").and_then(|v| v.as_str()).unwrap_or("turn");

        match mode {
            "turn" => {
                let turn = args.get("turn").and_then(|v| v.as_i64()).unwrap_or(-1);
                let idx = resolve_turn_idx(turn, traces.len());
                match idx {
                    Some(i) => analyze_turn(
                        &traces[i],
                        i,
                        traces.len(),
                        &session.fuzzy_match_events,
                    )
                    .to_string(),
                    None => json!({
                        "error": format!("Invalid turn {}. Available: 1–{} or -1 for latest.", turn, traces.len())
                    })
                    .to_string(),
                }
            }
            "session" => {
                analyze_session(traces, &session.turn_timings, &session.fuzzy_match_events)
                    .to_string()
            }
            "compare" => {
                let t1 = args.get("turn_a").and_then(|v| v.as_i64()).unwrap_or(1);
                let t2 = args.get("turn_b").and_then(|v| v.as_i64()).unwrap_or(-1);
                let idx1 = resolve_turn_idx(t1, traces.len());
                let idx2 = resolve_turn_idx(t2, traces.len());
                match (idx1, idx2) {
                    (Some(a), Some(b)) => compare_turns(&traces[a], a, &traces[b], b).to_string(),
                    _ => json!({
                        "error": format!(
                            "Invalid turns: {t1} or {t2}. Available: 1–{}",
                            traces.len()
                        )
                    })
                    .to_string(),
                }
            }
            _ => json!({
                "error": format!("Unknown mode '{mode}'. Use: turn, session, compare")
            })
            .to_string(),
        }
    }
}

fn resolve_turn_idx(turn: i64, len: usize) -> Option<usize> {
    if len == 0 {
        return None;
    }
    if turn > 0 {
        let idx = (turn - 1) as usize;
        if idx < len { Some(idx) } else { None }
    } else if turn < 0 {
        let from_end = (-turn) as usize;
        if from_end <= len {
            Some(len - from_end)
        } else {
            None
        }
    } else {
        None // 0 is invalid
    }
}

/// Detailed per-turn context breakdown with proportional analysis.
fn analyze_turn(
    trace: &astra_runtime::turn::context_assembly_trace::ContextAssemblyTrace,
    idx: usize,
    total_turns: usize,
    fuzzy_events: &[astra_runtime::observability_integration::FuzzyMatchEvent],
) -> Value {
    let tb = &trace.token_budget;
    let sp = &trace.system_prompt;
    let hist = &trace.history;
    let mem = &trace.memory;
    let tools = &trace.tools;

    let total = tb.total_used.max(1) as f64;
    let turn_number = (idx + 1) as u32;
    let turn_fuzzy_events: Vec<_> = fuzzy_events
        .iter()
        .filter(|event| event.turn == turn_number)
        .collect();
    let fuzzy_matches = turn_fuzzy_events
        .iter()
        .filter(|event| {
            event.outcome == astra_runtime::observability_integration::FuzzyMatchOutcome::Matched
        })
        .count();
    let fuzzy_ambiguous = turn_fuzzy_events
        .iter()
        .filter(|event| {
            event.outcome == astra_runtime::observability_integration::FuzzyMatchOutcome::Ambiguous
        })
        .count();
    let fuzzy_not_found = turn_fuzzy_events
        .iter()
        .filter(|event| {
            event.outcome == astra_runtime::observability_integration::FuzzyMatchOutcome::NotFound
        })
        .count();

    // System prompt sub-components
    let mut system_components = vec![
        json!({
            "component": "base_persona",
            "tokens": sp.base_persona_tokens,
            "pct_of_system": pct(sp.base_persona_tokens, sp.total_tokens),
            "pct_of_total": pct(sp.base_persona_tokens, tb.total_used),
        }),
        json!({
            "component": "environment",
            "tokens": sp.environment_tokens,
            "pct_of_system": pct(sp.environment_tokens, sp.total_tokens),
            "pct_of_total": pct(sp.environment_tokens, tb.total_used),
        }),
        json!({
            "component": "user_preferences",
            "tokens": sp.user_preferences_tokens,
            "pct_of_system": pct(sp.user_preferences_tokens, sp.total_tokens),
            "pct_of_total": pct(sp.user_preferences_tokens, tb.total_used),
        }),
    ];

    // Skills breakdown
    if !sp.skills_injected.is_empty() {
        let skills_tokens: u32 = sp.skills_injected.iter().map(|s| s.tokens).sum();
        let skills_detail: Vec<Value> = sp
            .skills_injected
            .iter()
            .map(|s| {
                json!({
                    "name": s.skill_name,
                    "tokens": s.tokens,
                    "pct_of_system": pct(s.tokens, sp.total_tokens),
                    "reason": s.selection_reason,
                })
            })
            .collect();
        system_components.push(json!({
            "component": "skills",
            "tokens": skills_tokens,
            "pct_of_system": pct(skills_tokens, sp.total_tokens),
            "pct_of_total": pct(skills_tokens, tb.total_used),
            "detail": skills_detail,
        }));
    }

    // Repository memories breakdown
    if !sp.repository_memories.is_empty() {
        let mem_tokens: u32 = sp.repository_memories.iter().map(|m| m.tokens).sum();
        let mem_detail: Vec<Value> = sp
            .repository_memories
            .iter()
            .map(|m| {
                json!({
                    "memory_type": m.memory_type,
                    "tokens": m.tokens,
                    "relevance": format!("{:.2}", m.relevance_score),
                    "preview": m.content_preview.chars().take(60).collect::<String>(),
                })
            })
            .collect();
        system_components.push(json!({
            "component": "repository_memories",
            "tokens": mem_tokens,
            "pct_of_system": pct(mem_tokens, sp.total_tokens),
            "pct_of_total": pct(mem_tokens, tb.total_used),
            "detail": mem_detail,
        }));
    }

    // History breakdown
    let history_detail = json!({
        "turns_available": hist.total_turns_available,
        "turns_retained": hist.turns_retained.len(),
        "turns_compressed": hist.turns_compressed.len(),
        "turns_dropped": hist.turns_dropped.len(),
        "tokens_before_compression": hist.tokens_before,
        "tokens_after_compression": hist.tokens_after,
        "compression_ratio": format!("{:.2}", hist.compression_ratio),
        "compression_methods": hist.turns_compressed.iter()
            .map(|tc| format!("{:?}", tc.compression_method))
            .collect::<Vec<_>>(),
    });

    // Memory retrieval breakdown
    let memory_detail = json!({
        "query": mem.query.chars().take(80).collect::<String>(),
        "candidates_considered": mem.candidates_considered,
        "selected": mem.memories_selected.len(),
        "rejected": mem.memories_rejected.len(),
        "retrieval_latency_ms": mem.retrieval_latency_ms,
        "selected_detail": mem.memories_selected.iter().map(|m| json!({
            "source": format!("{:?}", m.source),
            "tokens": m.tokens,
            "relevance": format!("{:.3}", m.relevance_score),
            "preview": m.content_preview.chars().take(50).collect::<String>(),
        })).collect::<Vec<_>>(),
    });

    // Tool selection breakdown
    let tools_detail = json!({
        "available": tools.tools_available,
        "selected": tools.tools_selected.len(),
        "rejected": tools.tools_rejected.len(),
        "strategy": tools.selection_strategy,
        "confidence": format!("{:.2}", tools.selection_confidence),
        "latency_ms": tools.selection_latency_ms,
        "selected_tools": tools.tools_selected.iter().map(|t| json!({
            "name": t.tool_name,
            "tokens": t.tokens,
            "score": format!("{:.3}", t.score),
        })).collect::<Vec<_>>(),
    });

    json!({
        "turn": idx + 1,
        "of_total_turns": total_turns,
        "token_budget": {
            "max_tokens": tb.max_tokens,
            "total_used": tb.total_used,
            "utilization_pct": format!("{:.1}", tb.total_used as f64 / tb.max_tokens.max(1) as f64 * 100.0),
            "budget_pressure": format!("{:.1}%", tb.budget_pressure * 100.0),
            "compression_triggered": tb.compression_triggered,
        },
        "composition": {
            "system_prompt": {
                "tokens": tb.system_prompt_tokens,
                "pct": format!("{:.1}%", tb.system_prompt_tokens as f64 / total * 100.0),
                "sub_components": system_components,
            },
            "history": {
                "tokens": tb.history_tokens,
                "pct": format!("{:.1}%", tb.history_tokens as f64 / total * 100.0),
                "detail": history_detail,
            },
            "memory": {
                "tokens": tb.memory_tokens,
                "pct": format!("{:.1}%", tb.memory_tokens as f64 / total * 100.0),
                "detail": memory_detail,
            },
            "tool_schemas": {
                "tokens": tb.tool_schema_tokens,
                "pct": format!("{:.1}%", tb.tool_schema_tokens as f64 / total * 100.0),
                "detail": tools_detail,
            },
            "user_message": {
                "tokens": tb.user_message_tokens,
                "pct": format!("{:.1}%", tb.user_message_tokens as f64 / total * 100.0),
            },
        },
        "decisions": trace.explanations.iter().map(|e| json!({
            "type": format!("{:?}", e.decision_type),
            "confidence": format!("{:.2}", e.confidence),
            "reasoning": e.reasoning.chars().take(120).collect::<String>(),
        })).collect::<Vec<_>>(),
        "fuzzy_matching": {
            "events": turn_fuzzy_events.len(),
            "matched": fuzzy_matches,
            "ambiguous": fuzzy_ambiguous,
            "not_found": fuzzy_not_found,
            "detail": turn_fuzzy_events.iter().map(|event| json!({
                "path": event.path,
                "strategy": event.strategy,
                "outcome": format!("{:?}", event.outcome),
            })).collect::<Vec<_>>(),
        },
    })
}

/// Multi-turn session analysis with trends and aggregation.
fn analyze_session(
    traces: &[astra_runtime::turn::context_assembly_trace::ContextAssemblyTrace],
    timings: &[astra_runtime::observability_integration::TurnTiming],
    fuzzy_events: &[astra_runtime::observability_integration::FuzzyMatchEvent],
) -> Value {
    use std::collections::BTreeMap;

    use astra_runtime::turn::context_assembly_trace::TraceAggregation;

    let agg = TraceAggregation::from_traces(traces);
    let n = traces.len();

    // Per-turn token breakdown for trend analysis
    let per_turn: Vec<Value> = traces
        .iter()
        .enumerate()
        .map(|(i, t)| {
            let tb = &t.token_budget;
            let timing = timings.get(i);
            json!({
                "turn": i + 1,
                "total_tokens": tb.total_used,
                "system": tb.system_prompt_tokens,
                "history": tb.history_tokens,
                "memory": tb.memory_tokens,
                "tools": tb.tool_schema_tokens,
                "user_msg": tb.user_message_tokens,
                "pressure": format!("{:.0}%", tb.budget_pressure * 100.0),
                "compressed": tb.compression_triggered,
                "timing_ms": timing.map(|t| t.total_ms).unwrap_or(0),
            })
        })
        .collect();

    // Compression events
    let compression_events: Vec<Value> = traces
        .iter()
        .enumerate()
        .filter(|(_, t)| t.token_budget.compression_triggered)
        .map(|(i, t)| {
            json!({
                "turn": i + 1,
                "pressure_at_trigger": format!("{:.0}%", t.token_budget.budget_pressure * 100.0),
                "history_before": t.history.tokens_before,
                "history_after": t.history.tokens_after,
                "tokens_freed": t.history.tokens_before.saturating_sub(t.history.tokens_after),
                "turns_compressed": t.history.turns_compressed.len(),
                "turns_dropped": t.history.turns_dropped.len(),
            })
        })
        .collect();

    // Compute growth rates
    let history_growth = if n >= 2 {
        let first = traces[0].token_budget.history_tokens as i64;
        let last = traces[n - 1].token_budget.history_tokens as i64;
        Some(last - first)
    } else {
        None
    };

    let pressure_trend = if n >= 2 {
        let first = traces[0].token_budget.budget_pressure;
        let last = traces[n - 1].token_budget.budget_pressure;
        Some(last - first)
    } else {
        None
    };

    // Peak values
    let peak_total = traces
        .iter()
        .map(|t| t.token_budget.total_used)
        .max()
        .unwrap_or(0);
    let peak_pressure = traces
        .iter()
        .map(|t| t.token_budget.budget_pressure)
        .fold(0.0f64, f64::max);

    let fuzzy_matched = fuzzy_events
        .iter()
        .filter(|event| {
            event.outcome == astra_runtime::observability_integration::FuzzyMatchOutcome::Matched
        })
        .count();
    let fuzzy_ambiguous = fuzzy_events
        .iter()
        .filter(|event| {
            event.outcome == astra_runtime::observability_integration::FuzzyMatchOutcome::Ambiguous
        })
        .count();
    let fuzzy_not_found = fuzzy_events
        .iter()
        .filter(|event| {
            event.outcome == astra_runtime::observability_integration::FuzzyMatchOutcome::NotFound
        })
        .count();
    let mut fuzzy_by_strategy = BTreeMap::<String, usize>::new();
    for event in fuzzy_events.iter().filter(|event| {
        event.outcome == astra_runtime::observability_integration::FuzzyMatchOutcome::Matched
    }) {
        *fuzzy_by_strategy.entry(event.strategy.clone()).or_default() += 1;
    }

    json!({
        "session_summary": {
            "total_turns": n,
            "peak_total_tokens": peak_total,
            "peak_budget_pressure": format!("{:.0}%", peak_pressure * 100.0),
            "compression_events": compression_events.len(),
            "total_timing_ms": timings.iter().map(|t| t.total_ms).sum::<u64>(),
        },
        "fuzzy_matching": {
            "events": fuzzy_events.len(),
            "matched": fuzzy_matched,
            "ambiguous": fuzzy_ambiguous,
            "not_found": fuzzy_not_found,
            "by_strategy": fuzzy_by_strategy.into_iter().map(|(strategy, count)| json!({
                "strategy": strategy,
                "count": count,
            })).collect::<Vec<_>>(),
        },
        "averages": {
            "system_prompt": format!("{:.0}", agg.avg_system_prompt_tokens),
            "history": format!("{:.0}", agg.avg_history_tokens),
            "memory": format!("{:.0}", agg.avg_memory_tokens),
            "tool_schemas": format!("{:.0}", agg.avg_tool_schema_tokens),
            "memories_selected": format!("{:.1}", agg.avg_memories_selected),
            "memory_relevance": format!("{:.3}", agg.avg_memory_relevance),
            "tools_selected": format!("{:.1}", agg.avg_tools_selected),
            "selection_confidence": format!("{:.2}", agg.avg_selection_confidence),
            "compression_ratio": format!("{:.2}", agg.avg_compression_ratio),
        },
        "trends": {
            "history_growth_tokens": history_growth,
            "pressure_change": pressure_trend.map(|p| format!("{:+.0}%", p * 100.0)),
            "compression_trigger_rate": format!("{:.0}%", agg.compression_trigger_rate * 100.0),
        },
        "per_turn": per_turn,
        "compression_events": compression_events,
    })
}

/// Compare two turns side-by-side.
fn compare_turns(
    a: &astra_runtime::turn::context_assembly_trace::ContextAssemblyTrace,
    a_idx: usize,
    b: &astra_runtime::turn::context_assembly_trace::ContextAssemblyTrace,
    b_idx: usize,
) -> Value {
    let ta = &a.token_budget;
    let tb = &b.token_budget;

    let diff = |a: u32, b: u32| -> Value {
        let d = b as i64 - a as i64;
        json!({
            "turn_a": a,
            "turn_b": b,
            "delta": d,
            "delta_pct": if a > 0 {
                format!("{:+.1}%", d as f64 / a as f64 * 100.0)
            } else {
                "N/A".to_string()
            },
        })
    };

    json!({
        "turn_a": a_idx + 1,
        "turn_b": b_idx + 1,
        "total_tokens": diff(ta.total_used, tb.total_used),
        "system_prompt": diff(ta.system_prompt_tokens, tb.system_prompt_tokens),
        "history": diff(ta.history_tokens, tb.history_tokens),
        "memory": diff(ta.memory_tokens, tb.memory_tokens),
        "tool_schemas": diff(ta.tool_schema_tokens, tb.tool_schema_tokens),
        "user_message": diff(ta.user_message_tokens, tb.user_message_tokens),
        "pressure": {
            "turn_a": format!("{:.0}%", ta.budget_pressure * 100.0),
            "turn_b": format!("{:.0}%", tb.budget_pressure * 100.0),
            "delta": format!("{:+.0}%", (tb.budget_pressure - ta.budget_pressure) * 100.0),
        },
        "compression": {
            "turn_a": ta.compression_triggered,
            "turn_b": tb.compression_triggered,
        },
    })
}

/// Calculate percentage with safety for zero denominator.
fn pct(numerator: u32, denominator: u32) -> String {
    if denominator == 0 {
        "0.0%".to_string()
    } else {
        format!("{:.1}%", numerator as f64 / denominator as f64 * 100.0)
    }
}
