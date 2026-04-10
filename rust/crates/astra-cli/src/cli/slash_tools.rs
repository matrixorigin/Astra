#![allow(unused_imports)]
use super::*;

// ═══════════════════════════════════════════════ Tool Profile ═════════════

pub(super) fn handle_tools_command(state: &ReplState) {
    use astra_services::session_analytics;

    let sid = match &state.session_id {
        Some(s) => s.clone(),
        None => {
            eprintln!("{}", "  No active session.".dim());
            return;
        }
    };
    let events = session_journal::read_journal(&sid).unwrap_or_default();
    let profiles = session_analytics::compute_tool_profiles(&events);

    if profiles.is_empty() {
        eprintln!("{}", "  No tool calls recorded yet.".dim());
        return;
    }

    eprintln!(
        "\n{}",
        "─── Tool Performance ────────────────────────────".bold()
    );
    eprintln!(
        "  {:<20} {:>5} {:>5} {:>7} {:>7} {:>7} {:>6}",
        "tool".bold(),
        "calls".bold(),
        "fail".bold(),
        "avg ms".bold(),
        "min ms".bold(),
        "max ms".bold(),
        "err%".bold(),
    );
    for p in &profiles {
        let err_pct = format!("{:.0}%", p.error_rate * 100.0);
        let err_display = if p.fail_count > 0 {
            err_pct.red().to_string()
        } else {
            err_pct
        };
        eprintln!(
            "  {:<20} {:>5} {:>5} {:>7} {:>7} {:>7} {:>6}",
            p.name.as_str().cyan(),
            p.call_count,
            p.fail_count,
            p.avg_ms,
            p.min_ms,
            p.max_ms,
            err_display,
        );
    }
    let total_ms: u64 = profiles.iter().map(|p| p.total_ms).sum();
    let total_calls: u32 = profiles.iter().map(|p| p.call_count).sum();
    eprintln!(
        "\n  {} {} calls, {:.1}s total tool time",
        "Summary:".bold(),
        total_calls,
        total_ms as f64 / 1000.0,
    );
    eprintln!();
}
