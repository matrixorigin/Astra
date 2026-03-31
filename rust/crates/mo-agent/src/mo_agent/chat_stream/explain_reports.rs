use crossterm::style::Stylize;

use crate::VerdictEvent;

pub(super) fn print_explain_report(turns: &[serde_json::Value], verbose: bool) {
    eprintln!("\n{}", "── EXPLAIN ─────────────────────────────".dim());
    let mut total_ms = 0i64;
    let mut total_prompt = 0i64;
    let mut total_completion = 0i64;
    let mut total_prompt_known = true;
    let mut total_completion_known = true;
    for (idx, turn) in turns.iter().enumerate() {
        let ms = turn.get("total_ms").and_then(|v| v.as_i64()).unwrap_or(0);
        let prompt = turn.get("prompt_tokens").and_then(|v| v.as_i64());
        let completion = turn.get("completion_tokens").and_then(|v| v.as_i64());
        total_ms += ms;
        if let Some(value) = prompt {
            total_prompt += value;
        } else {
            total_prompt_known = false;
        }
        if let Some(value) = completion {
            total_completion += value;
        } else {
            total_completion_known = false;
        }

        let selected = turn
            .get("tools_selected")
            .and_then(|v| v.as_u64())
            .map(|v| v.to_string())
            .unwrap_or_else(|| "?".to_string());
        let selected_skills = turn
            .get("selected_skills")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            })
            .unwrap_or_default();
        let available = turn
            .get("tools_available")
            .and_then(|v| v.as_u64())
            .map(|v| v.to_string())
            .unwrap_or_else(|| "?".to_string());
        let prompt_s = prompt
            .map(|v| v.to_string())
            .unwrap_or_else(|| "?".to_string());
        let completion_s = completion
            .map(|v| v.to_string())
            .unwrap_or_else(|| "?".to_string());
        let mut tool_info = format!("tools: {selected}/{available}");
        if let Some(selection) = turn.get("tool_selection").filter(|value| !value.is_null()) {
            tool_info.push_str(&format!(" → {selection}"));
        }
        if let Some(fallback) = turn
            .get("tool_selection_fallback")
            .filter(|value| !value.is_null())
        {
            tool_info.push_str(&format!(" ⚠fallback:{fallback}"));
        }
        if !selected_skills.is_empty() {
            tool_info.push_str(&format!("  skills=[{selected_skills}]"));
        }
        eprintln!(
            "{}",
            format!(
                "Turn {}  {}ms  tokens: {}→{}  {}",
                idx + 1,
                ms,
                prompt_s,
                completion_s,
                tool_info
            )
            .dim()
        );

        if let Some(routing) = turn.get("routing").and_then(|v| v.as_object()) {
            if routing.get("skipped").and_then(|v| v.as_bool()) == Some(true) {
                let reason = routing
                    .get("reason")
                    .and_then(|v| v.as_str())
                    .unwrap_or("?");
                eprintln!("{}", format!("  ├─ routing  skipped ({reason})").dim());
            } else {
                let intent = routing
                    .get("intent")
                    .and_then(|v| v.as_str())
                    .unwrap_or("?");
                let confidence = routing.get("confidence").and_then(|v| v.as_f64());
                let tier = routing
                    .get("tier")
                    .map(|v| v.to_string())
                    .unwrap_or_else(|| "?".to_string());
                let latency_ms = routing
                    .get("latency_ms")
                    .and_then(|v| v.as_f64())
                    .unwrap_or(0.0);
                let est = routing
                    .get("estimated_tokens")
                    .and_then(|v| v.as_i64())
                    .map(|v| v.to_string())
                    .unwrap_or_else(|| "?".to_string());
                let confidence_s = if intent == "default" {
                    "-".to_string()
                } else {
                    confidence
                        .map(|v| format!("{v:.2}"))
                        .unwrap_or_else(|| "?".to_string())
                };
                eprintln!(
                    "{}",
                    format!(
                        "  ├─ routing  {}  conf={}  tier={}  {:.0}ms  ~{}tok",
                        intent, confidence_s, tier, latency_ms, est
                    )
                    .dim()
                );
            }
        }

        if let Some(memory) = turn.get("memory").and_then(|v| v.as_object()) {
            if let Some(l0) = memory.get("l0").and_then(|v| v.as_object()) {
                let loaded = if l0.get("loaded").and_then(|v| v.as_bool()) == Some(true) {
                    "✓"
                } else {
                    "✗"
                };
                let l0_tokens = l0.get("tokens").and_then(|v| v.as_i64()).unwrap_or(0);
                let l0_ms = l0.get("ms").and_then(|v| v.as_f64()).unwrap_or(0.0);
                eprintln!(
                    "{}",
                    format!(
                        "  ├─ L0 profile  {}  {} tokens  {:.0}ms",
                        loaded, l0_tokens, l0_ms
                    )
                    .dim()
                );
            }
            if let Some(ret) = memory.get("retrieval").and_then(|v| v.as_object()) {
                let kw_hit = if ret.get("keyword_hit").and_then(|v| v.as_bool()) == Some(true) {
                    "✓"
                } else {
                    "✗"
                };
                let vec_hit = if ret.get("vector_hit").and_then(|v| v.as_bool()) == Some(true) {
                    "✓"
                } else {
                    "✗"
                };
                let p1 = ret
                    .get("phase1_candidates")
                    .and_then(|v| v.as_i64())
                    .unwrap_or(0);
                let p2 = ret
                    .get("phase2_candidates")
                    .and_then(|v| v.as_i64())
                    .unwrap_or(0);
                let merged = ret
                    .get("merged_candidates")
                    .and_then(|v| v.as_i64())
                    .unwrap_or(0);
                let final_count = ret.get("final_count").and_then(|v| v.as_i64()).unwrap_or(0);
                let ret_ms = ret.get("total_ms").and_then(|v| v.as_f64()).unwrap_or(0.0);
                let l1_tokens = memory
                    .get("l1")
                    .and_then(|v| v.as_object())
                    .and_then(|l1| l1.get("tokens"))
                    .and_then(|v| v.as_i64())
                    .unwrap_or(0);
                eprintln!(
                    "{}",
                    format!(
                        "  ├─ L1 retrieval  {:.0}ms  kw={}({}) vec={}({}) → {} → {}  {} tokens",
                        ret_ms, kw_hit, p1, vec_hit, p2, merged, final_count, l1_tokens
                    )
                    .dim()
                );
            } else if let Some(mem_ms) = memory.get("total_ms").and_then(|v| v.as_f64()) {
                eprintln!("{}", format!("  └─ memory total  {:.0}ms", mem_ms).dim());
            }
        }

        if let Some(steps) = turn.get("steps").and_then(|v| v.as_array()) {
            for step in steps {
                let label = step.get("step").and_then(|v| v.as_str()).unwrap_or("?");
                let dur = step
                    .get("duration_ms")
                    .and_then(|v| v.as_i64())
                    .unwrap_or(0);
                if label == "llm" {
                    let sin = step
                        .get("in")
                        .and_then(|v| v.as_i64())
                        .map(|v| v.to_string())
                        .unwrap_or_else(|| "?".to_string());
                    let sout = step
                        .get("out")
                        .and_then(|v| v.as_i64())
                        .map(|v| v.to_string())
                        .unwrap_or_else(|| "?".to_string());
                    let tc = step.get("tool_calls").and_then(|v| v.as_u64()).unwrap_or(0);
                    let suffix = if tc > 0 {
                        format!("in={} out={} tool_calls={}", sin, sout, tc)
                    } else {
                        format!("in={} out={}", sin, sout)
                    };
                    eprintln!("{}", format!("  └─ LLM  {}ms  {}", dur, suffix).dim());
                } else {
                    eprintln!("{}", format!("  └─ {}  {}ms", label, dur).dim());
                }
            }
        }

        if let Some(aux) = turn.get("auxiliary_llm_calls").and_then(|v| v.as_array()) {
            let mut aux_tokens_known = true;
            let aux_tokens = aux
                .iter()
                .map(|item| {
                    let tin = item.get("tokens_in").and_then(|v| v.as_i64());
                    let tout = item.get("tokens_out").and_then(|v| v.as_i64());
                    if tin.is_none() || tout.is_none() {
                        aux_tokens_known = false;
                    }
                    tin.unwrap_or(0) + tout.unwrap_or(0)
                })
                .sum::<i64>();
            eprintln!(
                "{}",
                format!(
                    "  ├─ auxiliary LLM  {} calls  {} tokens",
                    aux.len(),
                    if aux_tokens_known {
                        aux_tokens.to_string()
                    } else {
                        "?".to_string()
                    }
                )
                .dim()
            );
            for call in aux {
                let purpose = call.get("purpose").and_then(|v| v.as_str()).unwrap_or("?");
                let ms = call.get("ms").and_then(|v| v.as_i64()).unwrap_or(0);
                let tin = call
                    .get("tokens_in")
                    .and_then(|v| v.as_i64())
                    .map(|v| v.to_string())
                    .unwrap_or_else(|| "?".to_string());
                let tout = call
                    .get("tokens_out")
                    .and_then(|v| v.as_i64())
                    .map(|v| v.to_string())
                    .unwrap_or_else(|| "?".to_string());
                eprintln!(
                    "{}",
                    format!("  │    {}  {}ms  {}→{}", purpose, ms, tin, tout).dim()
                );
            }
        }
        if verbose {
            if let Some(preview) = turn.get("content_preview").and_then(|v| v.as_str()) {
                eprintln!("{}", format!("  ├─ content  {}", preview).dim());
            }
            if let Some(phase_timing) = turn.get("phase_timing").and_then(|v| v.as_array()) {
                for entry in phase_timing {
                    let step = entry.get("step").and_then(|v| v.as_str()).unwrap_or("?");
                    let ms = entry.get("ms").and_then(|v| v.as_i64()).unwrap_or(0);
                    eprintln!("{}", format!("  ├─ phase  {}  {}ms", step, ms).dim());
                }
            }
            if let Some(candidates) = turn
                .get("memory")
                .and_then(|v| v.get("retrieval"))
                .and_then(|v| v.get("candidates"))
                .and_then(|v| v.as_array())
            {
                for cand in candidates {
                    let score = cand.get("score").and_then(|v| v.as_f64()).unwrap_or(0.0);
                    let id = cand.get("id").and_then(|v| v.as_str()).unwrap_or("?");
                    eprintln!(
                        "{}",
                        format!("  ├─ candidate  {}  score={:.3}", id, score).dim()
                    );
                }
            }
        }
    }
    let total_prompt_s = if total_prompt_known {
        total_prompt.to_string()
    } else {
        "?".to_string()
    };
    let total_completion_s = if total_completion_known {
        total_completion.to_string()
    } else {
        "?".to_string()
    };
    eprintln!(
        "{}",
        format!(
            "Total: {}ms  tokens: {}→{}",
            total_ms, total_prompt_s, total_completion_s
        )
        .dim()
    );
    eprintln!("{}", "─────────────────────────────────────────────".dim());
}

/// Print TurnGuard verdict details in explain mode.
pub(super) fn print_verdict_report(verdict_events: &[VerdictEvent], verbose: bool) {
    if verdict_events.is_empty() {
        return;
    }
    eprintln!("\n{}", "── TURN GUARD ──────────────────────────".dim());
    for ve in verdict_events {
        let icon = match ve.severity.as_str() {
            "critical" => "🛑",
            "warning" => "⚠",
            _ => "ℹ",
        };
        eprintln!(
            "{}",
            format!(
                "T{} {} {}  nudges={}  errors={}  deprioritized={}{}",
                ve.turn,
                icon,
                ve.severity,
                ve.nudge_count,
                ve.total_errors,
                ve.deprioritized_count,
                if ve.force_stop { "  FORCE_STOP" } else { "" },
            )
            .dim()
        );
        if !ve.avoid_tools.is_empty() {
            eprintln!(
                "{}",
                format!("  ├─ avoid: [{}]", ve.avoid_tools.join(", ")).dim()
            );
        }
        if verbose {
            for (i, inj) in ve.injections.iter().enumerate() {
                let preview: String = inj.chars().take(120).collect();
                eprintln!("{}", format!("  ├─ injection[{}]: {}…", i, preview).dim());
            }
        } else if !ve.injections.is_empty() {
            eprintln!(
                "{}",
                format!("  └─ {} injection(s)", ve.injections.len()).dim()
            );
        }
    }
    eprintln!("{}", "─────────────────────────────────────────────".dim());
}
