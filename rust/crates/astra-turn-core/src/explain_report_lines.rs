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

    // ──────────────────────────────────────────────────────────
    // explain_turn_summary_line
    // ──────────────────────────────────────────────────────────

    #[test]
    fn turn_summary_line_format() {
        let s = explain_turn_summary_line(1, 250, "1024", "512", "tools: 3/10");
        assert!(s.contains("Turn 1"));
        assert!(s.contains("250ms"));
        assert!(s.contains("1024→512"));
        assert!(s.contains("tools: 3/10"));
    }

    #[test]
    fn turn_summary_line_zero_values() {
        let s = explain_turn_summary_line(0, 0, "0", "0", "");
        assert!(s.contains("Turn 0"));
        assert!(s.contains("0ms"));
    }

    // ──────────────────────────────────────────────────────────
    // explain_tool_info_line
    // ──────────────────────────────────────────────────────────

    #[test]
    fn tool_info_line_no_suffixes_no_skills() {
        let s = explain_tool_info_line("5", "20", None, None, "");
        assert_eq!(s, "tools: 5/20");
    }

    #[test]
    fn tool_info_line_selection_suffix_only() {
        let s = explain_tool_info_line("3", "10", Some(" → adaptive".into()), None, "");
        assert!(s.contains("→ adaptive"));
        assert!(!s.contains("skills="));
    }

    #[test]
    fn tool_info_line_fallback_suffix_only() {
        let s = explain_tool_info_line("3", "10", None, Some(" ⚠fallback:x".into()), "");
        assert!(s.contains("⚠fallback:x"));
    }

    // ──────────────────────────────────────────────────────────
    // explain_routing_*
    // ──────────────────────────────────────────────────────────

    #[test]
    fn routing_skipped_line() {
        let s = explain_routing_skipped_line("no classifier loaded");
        assert!(s.contains("skipped (no classifier loaded)"));
    }

    #[test]
    fn routing_active_line() {
        let s = explain_routing_active_line("code_edit", "0.95", "fast", 12.5, "4096");
        assert!(s.contains("code_edit"));
        assert!(s.contains("conf=0.95"));
        assert!(s.contains("tier=fast"));
        assert!(s.contains("12ms") || s.contains("13ms")); // {:.0} rounds to nearest
        assert!(s.contains("~4096tok"));
    }

    // ──────────────────────────────────────────────────────────
    // explain_l0_profile_line
    // ──────────────────────────────────────────────────────────

    #[test]
    fn l0_profile_line() {
        let s = explain_l0_profile_line("cached", 2048, 5.3);
        assert!(s.contains("L0 profile"));
        assert!(s.contains("cached"));
        assert!(s.contains("2048 tokens"));
        assert!(s.contains("5ms"));
    }

    // ──────────────────────────────────────────────────────────
    // explain_l1_retrieval_line
    // ──────────────────────────────────────────────────────────

    #[test]
    fn l1_retrieval_line() {
        let s = explain_l1_retrieval_line(25.0, "3", 10, "5", 20, 7, 5, 512);
        assert!(s.contains("L1 retrieval"));
        assert!(s.contains("25ms"));
        assert!(s.contains("kw=3(10)"));
        assert!(s.contains("vec=5(20)"));
        assert!(s.contains("512 tokens"));
    }

    // ──────────────────────────────────────────────────────────
    // explain_memory_total_line
    // ──────────────────────────────────────────────────────────

    #[test]
    fn memory_total_line() {
        let s = explain_memory_total_line(42.7);
        assert!(s.contains("memory total"));
        assert!(s.contains("43ms")); // rounded
    }

    // ──────────────────────────────────────────────────────────
    // explain_step_llm_line / explain_step_generic_line
    // ──────────────────────────────────────────────────────────

    #[test]
    fn step_llm_line() {
        let s = explain_step_llm_line(300, "in=1024 out=256");
        assert!(s.contains("LLM"));
        assert!(s.contains("300ms"));
        assert!(s.contains("in=1024 out=256"));
    }

    #[test]
    fn step_generic_line() {
        let s = explain_step_generic_line("planning", 50);
        assert!(s.contains("planning"));
        assert!(s.contains("50ms"));
    }

    // ──────────────────────────────────────────────────────────
    // explain_llm_tokens_suffix
    // ──────────────────────────────────────────────────────────

    #[test]
    fn llm_tokens_suffix_no_tool_calls() {
        let s = explain_llm_tokens_suffix("1024", "256", 0);
        assert_eq!(s, "in=1024 out=256");
    }

    #[test]
    fn llm_tokens_suffix_with_tool_calls() {
        let s = explain_llm_tokens_suffix("1024", "256", 3);
        assert!(s.contains("tool_calls=3"));
    }

    // ──────────────────────────────────────────────────────────
    // explain_auxiliary_llm_*
    // ──────────────────────────────────────────────────────────

    #[test]
    fn auxiliary_llm_header_line() {
        let s = explain_auxiliary_llm_header_line(2, "512");
        assert!(s.contains("auxiliary LLM"));
        assert!(s.contains("2 calls"));
        assert!(s.contains("512 tokens"));
    }

    #[test]
    fn auxiliary_llm_call_line() {
        let s = explain_auxiliary_llm_call_line("routing", 15, "100", "50");
        assert!(s.contains("routing"));
        assert!(s.contains("15ms"));
        assert!(s.contains("100→50"));
    }

    // ──────────────────────────────────────────────────────────
    // explain_content_preview_line / explain_phase_timing_line
    // ──────────────────────────────────────────────────────────

    #[test]
    fn content_preview_line() {
        let s = explain_content_preview_line("Hello world...");
        assert!(s.contains("content  Hello world..."));
    }

    #[test]
    fn phase_timing_line() {
        let s = explain_phase_timing_line("memory", 42);
        assert!(s.contains("phase  memory  42ms"));
    }

    // ──────────────────────────────────────────────────────────
    // explain_memory_candidate_line
    // ──────────────────────────────────────────────────────────

    #[test]
    fn memory_candidate_line() {
        let s = explain_memory_candidate_line("mem_42", 0.875);
        assert!(s.contains("candidate  mem_42  score=0.875"));
    }

    // ──────────────────────────────────────────────────────────
    // explain_totals_line
    // ──────────────────────────────────────────────────────────

    #[test]
    fn totals_line() {
        let s = explain_totals_line(500, "2048", "1024");
        assert!(s.contains("Total: 500ms"));
        assert!(s.contains("2048→1024"));
    }

    // ──────────────────────────────────────────────────────────
    // verdict_severity_icon
    // ──────────────────────────────────────────────────────────

    #[test]
    fn verdict_icon_warning() {
        assert_eq!(verdict_severity_icon("warning"), "⚠");
    }

    #[test]
    fn verdict_icon_unknown() {
        assert_eq!(verdict_severity_icon("info"), "ℹ");
        assert_eq!(verdict_severity_icon("other"), "ℹ");
    }

    // ──────────────────────────────────────────────────────────
    // verdict_event_summary_line
    // ──────────────────────────────────────────────────────────

    #[test]
    fn verdict_event_summary_no_force_stop() {
        let s = verdict_event_summary_line(3, "⚠", "warning", 2, 1, 0, false);
        assert!(s.contains("T3"));
        assert!(s.contains("⚠"));
        assert!(s.contains("nudges=2"));
        assert!(s.contains("errors=1"));
        assert!(!s.contains("FORCE_STOP"));
    }

    #[test]
    fn verdict_event_summary_with_force_stop() {
        let s = verdict_event_summary_line(1, "🛑", "critical", 0, 5, 2, true);
        assert!(s.contains("FORCE_STOP"));
        assert!(s.contains("deprioritized=2"));
    }

    // ──────────────────────────────────────────────────────────
    // verdict_avoid_tools_line / verdict_injection_*
    // ──────────────────────────────────────────────────────────

    #[test]
    fn verdict_avoid_tools_line_format() {
        let s = verdict_avoid_tools_line("bash, exec");
        assert!(s.contains("avoid: [bash, exec]"));
    }

    #[test]
    fn verdict_injection_preview_line_format() {
        let s = verdict_injection_preview_line(0, "You should not use bash");
        assert!(s.contains("injection[0]:"));
        assert!(s.contains("You should not use bash"));
    }

    #[test]
    fn verdict_injection_count_line_format() {
        assert!(verdict_injection_count_line(3).contains("3 injection(s)"));
    }
}
