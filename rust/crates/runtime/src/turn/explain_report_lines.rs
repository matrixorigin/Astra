//! Plain text lines for `--explain` stderr reports (CLI applies crossterm styles).

pub const EXPLAIN_REPORT_HEADER: &str = "── EXPLAIN ─────────────────────────────";
pub const REPORT_SEPARATOR_LINE: &str = "─────────────────────────────────────────────";
pub const VERDICT_REPORT_HEADER: &str = "── TURN GUARD ──────────────────────────";

#[must_use]
pub fn explain_turn_summary_line(
    turn_index_1based: usize,
    ms: i64,
    prompt_s: &str,
    completion_s: &str,
    tool_info: &str,
) -> String {
    format!(
        "Turn {}  {}ms  tokens: {}→{}  {}",
        turn_index_1based, ms, prompt_s, completion_s, tool_info
    )
}

#[must_use]
pub fn explain_tool_info_line(
    selected: &str,
    available: &str,
    selection_suffix: Option<String>,
    fallback_suffix: Option<String>,
    selected_skills_csv: &str,
) -> String {
    let mut tool_info = format!("tools: {selected}/{available}");
    if let Some(s) = selection_suffix {
        tool_info.push_str(&s);
    }
    if let Some(s) = fallback_suffix {
        tool_info.push_str(&s);
    }
    if !selected_skills_csv.is_empty() {
        tool_info.push_str(&format!("  skills=[{selected_skills_csv}]"));
    }
    tool_info
}

#[must_use]
pub fn explain_routing_skipped_line(reason: &str) -> String {
    format!("  ├─ routing  skipped ({reason})")
}

#[must_use]
pub fn explain_routing_active_line(
    intent: &str,
    confidence_s: &str,
    tier: &str,
    latency_ms: f64,
    est_tok: &str,
) -> String {
    format!(
        "  ├─ routing  {}  conf={}  tier={}  {:.0}ms  ~{}tok",
        intent, confidence_s, tier, latency_ms, est_tok
    )
}

#[must_use]
pub fn explain_l0_profile_line(loaded: &str, l0_tokens: i64, l0_ms: f64) -> String {
    format!(
        "  ├─ L0 profile  {}  {} tokens  {:.0}ms",
        loaded, l0_tokens, l0_ms
    )
}

#[must_use]
#[allow(clippy::too_many_arguments)] // Mirrors legacy explain JSON field bundle; keep one formatter.
pub fn explain_l1_retrieval_line(
    ret_ms: f64,
    kw_hit: &str,
    p1: i64,
    vec_hit: &str,
    p2: i64,
    merged: i64,
    final_count: i64,
    l1_tokens: i64,
) -> String {
    format!(
        "  ├─ L1 retrieval  {:.0}ms  kw={}({}) vec={}({}) → {} → {}  {} tokens",
        ret_ms, kw_hit, p1, vec_hit, p2, merged, final_count, l1_tokens
    )
}

#[must_use]
pub fn explain_memory_total_line(mem_ms: f64) -> String {
    format!("  └─ memory total  {:.0}ms", mem_ms)
}

#[must_use]
pub fn explain_step_llm_line(dur: i64, suffix: &str) -> String {
    format!("  └─ LLM  {}ms  {}", dur, suffix)
}

#[must_use]
pub fn explain_step_generic_line(label: &str, dur: i64) -> String {
    format!("  └─ {}  {}ms", label, dur)
}

#[must_use]
pub fn explain_llm_tokens_suffix(sin: &str, sout: &str, tool_calls: u64) -> String {
    if tool_calls > 0 {
        format!("in={} out={} tool_calls={}", sin, sout, tool_calls)
    } else {
        format!("in={} out={}", sin, sout)
    }
}

#[must_use]
pub fn explain_auxiliary_llm_header_line(n_calls: usize, tokens_display: &str) -> String {
    format!(
        "  ├─ auxiliary LLM  {} calls  {} tokens",
        n_calls, tokens_display
    )
}

#[must_use]
pub fn explain_auxiliary_llm_call_line(purpose: &str, ms: i64, tin: &str, tout: &str) -> String {
    format!("  │    {}  {}ms  {}→{}", purpose, ms, tin, tout)
}

#[must_use]
pub fn explain_content_preview_line(preview: &str) -> String {
    format!("  ├─ content  {}", preview)
}

#[must_use]
pub fn explain_phase_timing_line(step: &str, ms: i64) -> String {
    format!("  ├─ phase  {}  {}ms", step, ms)
}

#[must_use]
pub fn explain_memory_candidate_line(id: &str, score: f64) -> String {
    format!("  ├─ candidate  {}  score={:.3}", id, score)
}

#[must_use]
pub fn explain_totals_line(
    total_ms: i64,
    total_prompt_s: &str,
    total_completion_s: &str,
) -> String {
    format!(
        "Total: {}ms  tokens: {}→{}",
        total_ms, total_prompt_s, total_completion_s
    )
}

#[must_use]
pub fn verdict_severity_icon(severity: &str) -> &'static str {
    match severity {
        "critical" => "🛑",
        "warning" => "⚠",
        _ => "ℹ",
    }
}

#[must_use]
pub fn verdict_event_summary_line(
    turn: u32,
    icon: &str,
    severity: &str,
    nudge_count: usize,
    total_errors: usize,
    deprioritized_count: usize,
    force_stop: bool,
) -> String {
    format!(
        "T{} {} {}  nudges={}  errors={}  deprioritized={}{}",
        turn,
        icon,
        severity,
        nudge_count,
        total_errors,
        deprioritized_count,
        if force_stop { "  FORCE_STOP" } else { "" },
    )
}

#[must_use]
pub fn verdict_avoid_tools_line(tools_csv: &str) -> String {
    format!("  ├─ avoid: [{}]", tools_csv)
}

#[must_use]
pub fn verdict_injection_preview_line(index: usize, preview: &str) -> String {
    format!("  ├─ injection[{}]: {}…", index, preview)
}

#[must_use]
pub fn verdict_injection_count_line(n: usize) -> String {
    format!("  └─ {} injection(s)", n)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_info_line_joins_optional_suffixes() {
        let s = explain_tool_info_line(
            "2",
            "10",
            Some(" → true".into()),
            Some(" ⚠fallback:x".into()),
            "a, b",
        );
        assert!(s.contains("tools: 2/10"));
        assert!(s.contains("skills=[a, b]"));
    }

    #[test]
    fn verdict_icon_critical() {
        assert_eq!(verdict_severity_icon("critical"), "🛑");
    }
}
