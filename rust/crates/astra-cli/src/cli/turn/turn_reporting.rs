//! Final turn reporting, status lines, and summary rendering.

use std::time::Instant;

use crate::cli::session::session_state::SessionState;
use crate::cli::stream::streaming_types::StreamResult;
use astra_services::session_journal;
use crossterm::style::Stylize;

pub(crate) fn compact_token_count(tokens: u64) -> String {
    if tokens > 1000 {
        format!("{:.1}k", tokens as f64 / 1000.0)
    } else {
        format!("{tokens}")
    }
}

pub(crate) fn cache_hit_percentage(
    prompt_tokens: u64,
    cache_read_tokens: u64,
    cache_creation_tokens: u64,
) -> f64 {
    let total_input = prompt_tokens + cache_read_tokens + cache_creation_tokens;
    cache_read_tokens as f64 / total_input.max(1) as f64 * 100.0
}

/// Build a compact tool-call summary for cross-turn context continuity.
///
/// Appended to the assistant text in history so the next turn's prompt
/// contains file paths and tool outcomes from the previous turn — without
/// storing the full tool_call / tool_result messages.
pub(crate) fn build_turn_tool_summary(records: &[session_journal::ToolCallRecord]) -> String {
    if records.is_empty() {
        return String::new();
    }

    let mut files = Vec::new();
    let mut failed = Vec::new();
    for record in records {
        if let Some(file_path) = record.file_path.as_deref()
            && !files.contains(&file_path)
        {
            files.push(file_path);
        }
        if !record.ok && !failed.contains(&record.name.as_str()) {
            failed.push(record.name.as_str());
        }
    }

    let mut parts = Vec::new();
    if !files.is_empty() {
        if files.len() <= 15 {
            parts.push(format!("files: {}", files.join(", ")));
        } else {
            parts.push(format!(
                "files: {} (+{} more)",
                files[..15].join(", "),
                files.len() - 15
            ));
        }
    }
    if !failed.is_empty() {
        parts.push(format!("failed: {}", failed.join(", ")));
    }
    parts.push(format!("tool_calls: {}", records.len()));

    format!("\n\n[Turn context: {}]", parts.join(" | "))
}

/// Build the text stored in history: assistant response + optional tool summary.
pub(crate) fn build_history_text(
    full_text: &str,
    records: &[session_journal::ToolCallRecord],
) -> String {
    let summary = build_turn_tool_summary(records);
    if summary.is_empty() {
        return full_text.to_string();
    }
    format!("{full_text}{summary}")
}

pub(crate) fn print_turn_status_line(
    state: &SessionState,
    result: &StreamResult,
    turn_start: Instant,
) {
    if state.tui_render_policy.is_some() {
        return;
    }
    let elapsed = turn_start.elapsed();
    let elapsed_str = if elapsed.as_secs() >= 60 {
        format!("{}m{:.0}s", elapsed.as_secs() / 60, elapsed.as_secs() % 60)
    } else {
        format!("{:.1}s", elapsed.as_secs_f64())
    };

    let total_input =
        result.prompt_tokens + result.cache_read_tokens + result.cache_creation_tokens;
    let total_tokens = total_input + result.completion_tokens;
    let tokens_str = compact_token_count(total_tokens);
    let prompt_short = compact_token_count(total_input);
    let completion_short = compact_token_count(result.completion_tokens);

    let turn_cost = crate::cli::slash::slash_stats::cost_for_tokens(
        result.prompt_tokens,
        result.completion_tokens,
        result.cache_read_tokens,
        result.cache_creation_tokens,
        &state.cached_pricing,
    );

    let mut parts = Vec::new();
    if let Some(model) = state.model.as_ref() {
        parts.push(format!("model:{model}"));
    }
    parts.push(format!(
        "tokens:{tokens_str} (↑{prompt_short} ↓{completion_short})"
    ));
    if turn_cost > 0.0 {
        parts.push(crate::cli::slash::slash_stats::format_cost(turn_cost));
    }
    parts.push(elapsed_str);
    if let Some(ttft) = result.ttft_ms
        && ttft > 0
    {
        parts.push(format!("ttft:{ttft}ms"));
    }
    if result.tool_calls_count > 0 {
        parts.push(format!(
            "{} tool{}",
            result.tool_calls_count,
            if result.tool_calls_count == 1 {
                ""
            } else {
                "s"
            }
        ));
    }
    if result.cache_read_tokens > 0 {
        let cache_pct = cache_hit_percentage(
            result.prompt_tokens,
            result.cache_read_tokens,
            result.cache_creation_tokens,
        );
        parts.push(format!("cache:{cache_pct:.0}%"));
    }

    eprintln!("{}", format!("  ─ {} ─", parts.join(" │ ")).dim());

    let session_cost = state.total_session_cost + turn_cost;
    if session_cost > 0.0 && state.turn > 0 {
        eprintln!(
            "{}",
            format!(
                "  session: {}",
                crate::cli::slash::slash_stats::format_cost(session_cost)
            )
            .dim()
        );
    }
    if let Some(error) = state
        .session_persistence_error
        .as_deref()
        .map(str::trim)
        .filter(|error| !error.is_empty())
    {
        eprintln!(
            "{}",
            format!("  ⚠ Session persistence degraded: {error}").yellow()
        );
    }

    if let Some(notice) = interruption_status_notice(result) {
        eprintln!("{}", format!("  ⚠ {notice}").yellow());
    }
    print_context_window_warning(result.budget_pressure);

    let width = crossterm::terminal::size()
        .map(|(columns, _)| columns as usize)
        .unwrap_or(80);
    eprintln!("{}", "─".repeat(width.min(72)).dim());
}

pub(crate) fn interruption_status_notice(result: &StreamResult) -> Option<String> {
    let interruption = result.interruption.as_ref()?;
    if let Some(user_message) = interruption
        .get("user_message")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|message| !message.is_empty())
    {
        return Some(user_message.to_string());
    }

    let kind = result
        .interruption_kind
        .as_deref()
        .or_else(|| interruption.get("kind").and_then(serde_json::Value::as_str))?;
    let resumable = interruption
        .get("resumable")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    let suffix = if resumable {
        " You can continue in the next message."
    } else {
        ""
    };
    Some(format!("[{kind}] Turn interrupted.{suffix}"))
}

/// Print a context window warning when budget pressure exceeds thresholds.
pub(crate) fn print_context_window_warning(budget_pressure: f64) {
    const WARNING_THRESHOLD: f64 = 0.70;
    const CRITICAL_THRESHOLD: f64 = 0.85;

    if budget_pressure >= CRITICAL_THRESHOLD {
        let remaining = ((1.0 - budget_pressure) * 100.0).max(0.0);
        eprintln!(
            "{}",
            format!(
                "  🔴 Context window {:.0}% full ({:.0}% remaining) — consider /compact or starting a new session",
                budget_pressure * 100.0,
                remaining
            )
            .red()
        );
    } else if budget_pressure >= WARNING_THRESHOLD {
        let remaining = ((1.0 - budget_pressure) * 100.0).max(0.0);
        eprintln!(
            "{}",
            format!(
                "  🟡 Context window {:.0}% used ({:.0}% remaining) — use /stats context for details",
                budget_pressure * 100.0,
                remaining
            )
            .yellow()
        );
    }
}

#[cfg(test)]
mod tests {
    use super::{
        build_history_text, build_turn_tool_summary, cache_hit_percentage, compact_token_count,
        interruption_status_notice,
    };
    use astra_services::session_journal;

    fn make_record(
        name: &str,
        ok: bool,
        file_path: Option<&str>,
    ) -> session_journal::ToolCallRecord {
        session_journal::ToolCallRecord {
            name: name.into(),
            ok,
            file_path: file_path.map(|path| path.into()),
            ..Default::default()
        }
    }

    #[test]
    fn interruption_status_notice_prefers_user_message() {
        let mut result = crate::tests::stub_stream_result("");
        result.interruption = Some(serde_json::json!({
            "kind": "budget_exhausted",
            "resumable": true,
            "user_message": "[budget_exhausted] 2 tool call(s) completed. Continue next turn."
        }));
        assert_eq!(
            interruption_status_notice(&result).as_deref(),
            Some("[budget_exhausted] 2 tool call(s) completed. Continue next turn.")
        );
    }

    #[test]
    fn interruption_status_notice_falls_back_to_kind_and_resumable_hint() {
        let mut result = crate::tests::stub_stream_result("");
        result.interruption_kind = Some("context_budget".into());
        result.interruption = Some(serde_json::json!({
            "kind": "context_budget",
            "resumable": true
        }));
        assert_eq!(
            interruption_status_notice(&result).as_deref(),
            Some("[context_budget] Turn interrupted. You can continue in the next message.")
        );
    }

    #[test]
    fn tool_summary_empty_when_no_tools() {
        let summary = build_turn_tool_summary(&[]);
        assert!(summary.is_empty());
    }

    #[test]
    fn tool_summary_lists_files_touched() {
        let records = vec![
            make_record("read_file", true, Some("src/main.rs")),
            make_record("str_replace", true, Some("src/lib.rs")),
            make_record("read_file", true, Some("src/main.rs")),
        ];

        let summary = build_turn_tool_summary(&records);
        assert!(summary.contains("files: src/main.rs, src/lib.rs"));
        assert!(summary.contains("tool_calls: 3"));
        assert!(!summary.contains("failed:"));
    }

    #[test]
    fn tool_summary_includes_failures_and_caps_files() {
        let mut records = Vec::new();
        for idx in 0..18 {
            records.push(make_record(
                if idx % 2 == 0 { "read_file" } else { "edit" },
                idx % 5 != 0,
                Some(&format!("src/file_{idx}.rs")),
            ));
        }

        let summary = build_turn_tool_summary(&records);
        assert!(summary.contains("failed: read_file, edit"));
        assert!(summary.contains("(+3 more)"));
    }

    #[test]
    fn tool_summary_stays_compact_under_heavy_load() {
        let mut records = Vec::new();
        for idx in 0..50 {
            let file = format!("src/module_{}/file_{}.rs", idx / 5, idx % 5);
            records.push(make_record(
                if idx % 3 == 0 {
                    "read_file"
                } else {
                    "str_replace"
                },
                idx % 7 != 0,
                Some(&file),
            ));
        }

        let summary = build_turn_tool_summary(&records);
        assert!(
            summary.len() < 2048,
            "summary should be compact, got {} bytes: {summary}",
            summary.len()
        );
        assert!(summary.contains("src/module_0/file_0.rs"));
        assert!(
            summary.contains("more)"),
            "should truncate beyond 15 files: {summary}"
        );
    }

    #[test]
    fn history_text_appends_tool_summary() {
        let full_text = "Updated three files.";
        let records = vec![
            make_record("read_file", true, Some("src/main.rs")),
            make_record("edit", false, Some("src/lib.rs")),
        ];
        let history_text = build_history_text(full_text, &records);
        assert!(history_text.starts_with(full_text));
        assert!(history_text.contains("[Turn context:"));
        assert!(history_text.contains("failed: edit"));
    }

    #[test]
    fn history_text_noop_without_tool_summary() {
        let full_text = "No tools used.";
        assert_eq!(build_history_text(full_text, &[]), full_text);
    }

    #[test]
    fn cache_hit_percentage_formula() {
        let cache_pct = cache_hit_percentage(200, 800, 0);
        assert!((cache_pct - 80.0).abs() < 0.01);
    }

    #[test]
    fn cache_hit_percentage_zero_when_no_cache() {
        let cache_pct = cache_hit_percentage(1000, 0, 0);
        assert!((cache_pct - 0.0).abs() < 0.01);
    }

    #[test]
    fn cache_hit_percentage_with_heavy_cache_creation() {
        let cache_pct = cache_hit_percentage(12, 29_816, 38_788);
        assert!(
            (cache_pct - 43.5).abs() < 1.0,
            "expected ~43.5%, got {cache_pct:.1}%"
        );
    }

    #[test]
    fn cache_hit_percentage_100_only_when_all_input_was_cache_read() {
        let cache_pct = cache_hit_percentage(0, 5000, 0);
        assert!((cache_pct - 100.0).abs() < 0.01);
    }

    #[test]
    fn compact_token_count_below_1k() {
        assert_eq!(compact_token_count(999), "999");
    }

    #[test]
    fn compact_token_count_above_1k() {
        assert_eq!(compact_token_count(12_500), "12.5k");
    }
}
