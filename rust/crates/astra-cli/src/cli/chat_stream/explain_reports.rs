use astra_turn_core::explain_report_lines::{
    EXPLAIN_REPORT_HEADER, REPORT_SEPARATOR_LINE, VERDICT_REPORT_HEADER,
    explain_auxiliary_llm_call_line, explain_auxiliary_llm_header_line,
    explain_content_preview_line, explain_l0_profile_line, explain_l1_retrieval_line,
    explain_llm_tokens_suffix, explain_memory_candidate_line, explain_memory_total_line,
    explain_phase_timing_line, explain_routing_active_line, explain_routing_skipped_line,
    explain_step_generic_line, explain_step_llm_line, explain_tool_info_line, explain_totals_line,
    explain_turn_summary_line, verdict_avoid_tools_line, verdict_event_summary_line,
    verdict_injection_count_line, verdict_injection_preview_line, verdict_severity_icon,
};
use crossterm::style::Stylize;

use crate::VerdictEvent;

/// Verbose explain `content_preview` can echo tool-shaped dumps (e.g. read_file line grids). Omit those.
fn scrub_explain_content_preview(raw: &str, max_chars: usize) -> Option<String> {
    let t = raw.trim();
    if t.is_empty() {
        return None;
    }
    let digit_prefix_lines = t
        .lines()
        .filter(|l| {
            let s = l.trim_start();
            s.chars().next().is_some_and(|c| c.is_ascii_digit())
        })
        .count();
    if digit_prefix_lines >= 4 {
        return None;
    }
    let lower = t.to_ascii_lowercase();
    if lower.contains("\"read_file\"") && t.contains('{') {
        return None;
    }
    let mut out: String = t.chars().take(max_chars).collect();
    if t.chars().count() > max_chars {
        out.push('…');
    }
    Some(out)
}

pub(super) fn print_explain_report(turns: &[serde_json::Value], verbose: bool) {
    eprintln!("\n{}", EXPLAIN_REPORT_HEADER.dim());
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
        let tool_info = explain_tool_info_line(
            selected.as_str(),
            available.as_str(),
            turn.get("tool_selection")
                .filter(|value| !value.is_null())
                .map(|v| format!(" → {v}")),
            turn.get("tool_selection_fallback")
                .filter(|value| !value.is_null())
                .map(|v| format!(" ⚠fallback:{v}")),
            selected_skills.as_str(),
        );
        eprintln!(
            "{}",
            explain_turn_summary_line(idx + 1, ms, &prompt_s, &completion_s, tool_info.as_str())
                .dim()
        );

        if let Some(routing) = turn.get("routing").and_then(|v| v.as_object()) {
            if routing.get("skipped").and_then(|v| v.as_bool()) == Some(true) {
                let reason = routing
                    .get("reason")
                    .and_then(|v| v.as_str())
                    .unwrap_or("?");
                eprintln!("{}", explain_routing_skipped_line(reason).dim());
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
                    explain_routing_active_line(
                        intent,
                        confidence_s.as_str(),
                        tier.as_str(),
                        latency_ms,
                        est.as_str(),
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
                    explain_l0_profile_line(loaded, l0_tokens, l0_ms).dim()
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
                    explain_l1_retrieval_line(
                        ret_ms,
                        kw_hit,
                        p1,
                        vec_hit,
                        p2,
                        merged,
                        final_count,
                        l1_tokens,
                    )
                    .dim()
                );
            } else if let Some(mem_ms) = memory.get("total_ms").and_then(|v| v.as_f64()) {
                eprintln!("{}", explain_memory_total_line(mem_ms).dim());
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
                    let suffix = explain_llm_tokens_suffix(sin.as_str(), sout.as_str(), tc);
                    eprintln!("{}", explain_step_llm_line(dur, suffix.as_str()).dim());
                } else {
                    eprintln!("{}", explain_step_generic_line(label, dur).dim());
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
            let tokens_display = if aux_tokens_known {
                aux_tokens.to_string()
            } else {
                "?".to_string()
            };
            eprintln!(
                "{}",
                explain_auxiliary_llm_header_line(aux.len(), tokens_display.as_str()).dim()
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
                    explain_auxiliary_llm_call_line(purpose, ms, tin.as_str(), tout.as_str()).dim()
                );
            }
        }
        if verbose {
            if let Some(preview) = turn
                .get("content_preview")
                .and_then(|v| v.as_str())
                .and_then(|p| scrub_explain_content_preview(p, 220))
            {
                eprintln!("{}", explain_content_preview_line(preview.as_str()).dim());
            }
            if let Some(phase_timing) = turn.get("phase_timing").and_then(|v| v.as_array()) {
                for entry in phase_timing {
                    let step = entry.get("step").and_then(|v| v.as_str()).unwrap_or("?");
                    let ms = entry.get("ms").and_then(|v| v.as_i64()).unwrap_or(0);
                    eprintln!("{}", explain_phase_timing_line(step, ms).dim());
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
                    eprintln!("{}", explain_memory_candidate_line(id, score).dim());
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
        explain_totals_line(
            total_ms,
            total_prompt_s.as_str(),
            total_completion_s.as_str()
        )
        .dim()
    );
    eprintln!("{}", REPORT_SEPARATOR_LINE.dim());
}

/// Print TurnGuard verdict details in explain mode.
pub(super) fn print_verdict_report(verdict_events: &[VerdictEvent], verbose: bool) {
    if verdict_events.is_empty() {
        return;
    }
    eprintln!("\n{}", VERDICT_REPORT_HEADER.dim());
    for ve in verdict_events {
        let icon = verdict_severity_icon(ve.severity.as_str());
        eprintln!(
            "{}",
            verdict_event_summary_line(
                ve.turn,
                icon,
                ve.severity.as_str(),
                ve.nudge_count,
                ve.total_errors,
                ve.deprioritized_count,
                ve.force_stop,
            )
            .dim()
        );
        if !ve.avoid_tools.is_empty() {
            eprintln!(
                "{}",
                verdict_avoid_tools_line(ve.avoid_tools.join(", ").as_str()).dim()
            );
        }
        if verbose {
            for (i, inj) in ve.injections.iter().enumerate() {
                let preview: String = inj.chars().take(120).collect();
                eprintln!(
                    "{}",
                    verdict_injection_preview_line(i, preview.as_str()).dim()
                );
            }
        } else if !ve.injections.is_empty() {
            eprintln!(
                "{}",
                verdict_injection_count_line(ve.injections.len()).dim()
            );
        }
    }
    eprintln!("{}", REPORT_SEPARATOR_LINE.dim());
}

#[cfg(test)]
mod explain_preview_tests {
    use super::scrub_explain_content_preview;

    #[test]
    fn scrub_drops_numbered_line_grids() {
        let raw = "    1|a\n    2|b\n    3|c\n    4|d\n";
        assert!(scrub_explain_content_preview(raw, 80).is_none());
    }

    #[test]
    fn scrub_drops_read_file_json_snippet() {
        let raw = r#"{"tool":"read_file","path":"x"}"#;
        assert!(scrub_explain_content_preview(raw, 80).is_none());
    }

    #[test]
    fn scrub_keeps_short_prose() {
        let raw = "Here is a concise summary of the change.";
        let s = scrub_explain_content_preview(raw, 80).expect("some");
        assert!(s.contains("concise"));
    }
}
