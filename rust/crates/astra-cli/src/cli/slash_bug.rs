use super::*;
use crate::{cli_dim, cli_err, cli_ok, cli_warn};
use std::io::Write;
use std::process::{Command as SysCommand, Stdio};

/// Handle the `/bug` slash command — generate a diagnostic report for bug reports.
///
/// Usage:
///   /bug           — print diagnostic report to terminal
///   /bug copy      — copy report to clipboard
///   /bug save      — save report to file in cwd
pub(super) fn handle_bug_command(arg: &str, state: &ReplState) {
    let report = build_bug_report(state);

    match arg.trim() {
        "copy" => {
            if copy_to_clipboard(&report) {
                cli_ok!("Bug report copied to clipboard.");
            } else {
                cli_warn!("Could not copy to clipboard — printing instead:");
                eprintln!("{report}");
            }
        }
        "save" => {
            let filename = format!(
                "astra-bug-{}.md",
                chrono::Local::now().format("%Y%m%d-%H%M%S")
            );
            match std::fs::write(&filename, &report) {
                Ok(()) => {
                    cli_ok!("Saved to {}", filename);
                }
                Err(e) => {
                    cli_err!("Could not save: {}", e);
                    eprintln!("{report}");
                }
            }
        }
        "" => {
            eprintln!("{report}");
            cli_dim!("Use /bug copy to clipboard, /bug save to file.");
        }
        other => {
            cli_warn!("Unknown sub-command '{}'. Usage: /bug [copy|save]", other);
        }
    }
}

fn copy_to_clipboard(text: &str) -> bool {
    let wayland = std::env::var("WAYLAND_DISPLAY").is_ok();
    let mut candidates: Vec<(&str, &[&str])> = Vec::new();
    if wayland {
        candidates.push(("wl-copy", &[]));
    }
    candidates.extend_from_slice(&[
        ("xclip", &["-selection", "clipboard"] as &[&str]),
        ("xsel", &["--clipboard", "--input"]),
        ("pbcopy", &[]),
    ]);
    for (cmd, args) in &candidates {
        if let Ok(mut child) = SysCommand::new(cmd)
            .args(*args)
            .stdin(Stdio::piped())
            .spawn()
        {
            if let Some(mut stdin) = child.stdin.take() {
                let _ = stdin.write_all(text.as_bytes());
            }
            if child.wait().map(|s| s.success()).unwrap_or(false) {
                return true;
            }
        }
    }
    false
}

fn build_bug_report(state: &ReplState) -> String {
    let mut lines = Vec::new();

    lines.push("# Astra Bug Report".to_string());
    lines.push(String::new());

    // ── Environment ──
    lines.push("## Environment".to_string());
    lines.push(String::new());
    lines.push(format!("- **Version**: {}", env!("CARGO_PKG_VERSION")));
    lines.push(format!(
        "- **OS**: {} {}",
        std::env::consts::OS,
        std::env::consts::ARCH
    ));

    if let Ok(shell) = std::env::var("SHELL") {
        lines.push(format!("- **Shell**: {}", shell));
    }
    if let Ok(term) = std::env::var("TERM") {
        lines.push(format!("- **Terminal**: {}", term));
    }
    lines.push(String::new());

    // ── Session ──
    lines.push("## Session".to_string());
    lines.push(String::new());
    lines.push(format!(
        "- **Model**: {}",
        state.model.as_deref().unwrap_or("(none)")
    ));
    lines.push(format!(
        "- **Session ID**: {}",
        state.session_id.as_deref().unwrap_or("(none)")
    ));
    lines.push(format!("- **Turns**: {}", state.turn));
    lines.push(format!(
        "- **Tokens**: prompt={}, completion={}, cache_read={}, cache_write={}",
        state.total_prompt_tokens,
        state.total_completion_tokens,
        state.total_cache_read_tokens,
        state.total_cache_creation_tokens,
    ));
    if state.total_session_cost > 0.0 {
        lines.push(format!(
            "- **Cost**: {}",
            crate::slash_stats::format_cost(state.total_session_cost)
        ));
    }
    lines.push(String::new());

    // ── Configuration ──
    lines.push("## Configuration".to_string());
    lines.push(String::new());

    // Redacted env vars — show presence but not values for sensitive ones.
    let config_vars = [
        ("MO_API_BASE", false),
        ("MO_API_KEY", true),
        ("MO_MODEL", false),
        ("MO_MAX_TURNS", false),
        ("MO_PLAN_SUBTASK_MAX_TURNS", false),
        ("MO_MAX_TOOL_RETRIES", false),
        ("MO_RETRY_BASE_MS", false),
        ("MO_BUDGET_LIMIT", false),
        ("MO_THINKING_BUDGET", false),
    ];
    for (var, sensitive) in &config_vars {
        match std::env::var(var) {
            Ok(val) => {
                if *sensitive {
                    lines.push(format!("- `{var}`: [REDACTED, {} chars]", val.len()));
                } else {
                    lines.push(format!("- `{var}`: {val}"));
                }
            }
            Err(_) => {}
        }
    }
    lines.push(String::new());

    // ── MCP servers ──
    lines.push("## MCP Servers".to_string());
    lines.push(String::new());
    // We can't easily get MCP servers synchronously from the RwLock, so list what we can.
    lines.push("(Use `/mcp status` for detailed MCP server info)".to_string());
    lines.push(String::new());

    // ── Active skills ──
    lines.push("## Active Skills".to_string());
    lines.push(String::new());
    if state.active_system_skills.is_empty() {
        lines.push("- (none)".to_string());
    } else {
        for skill in &state.active_system_skills {
            lines.push(format!("- {}", skill.name));
        }
    }
    lines.push(String::new());

    // ── Last response (truncated) ──
    lines.push("## Last Response (truncated)".to_string());
    lines.push(String::new());
    match &state.last_response {
        Some(resp) if !resp.is_empty() => {
            let truncated: String = resp.chars().take(500).collect();
            lines.push(format!("```\n{truncated}\n```"));
            if resp.len() > 500 {
                lines.push(format!("... ({} chars total, truncated)", resp.len()));
            }
        }
        _ => {
            lines.push("(no response yet)".to_string());
        }
    }
    lines.push(String::new());

    // ── Steps to reproduce ──
    lines.push("## Steps to Reproduce".to_string());
    lines.push(String::new());
    lines.push("1. ".to_string());
    lines.push("2. ".to_string());
    lines.push("3. ".to_string());
    lines.push(String::new());

    // ── Expected vs actual ──
    lines.push("## Expected Behavior".to_string());
    lines.push(String::new());
    lines.push(String::new());
    lines.push("## Actual Behavior".to_string());
    lines.push(String::new());
    lines.push(String::new());

    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Drift guard: slash_bug tests must not re-introduce raw unsafe env
    /// mutation. Use `temp_env::with_var` so env state is restored via RAII
    /// even when a test panics.
    #[test]
    fn slash_bug_tests_use_temp_env_not_unsafe_set_var() {
        let unsafe_open = format!("{}{}", "unsafe", " { ");
        let std_env = format!("{}{}", "std::", "env::");
        let sentinel_set = format!("{unsafe_open}{std_env}set_{}", "var");
        let sentinel_remove = format!("{unsafe_open}{std_env}remove_{}", "var");
        let source = include_str!("slash_bug.rs");
        assert!(
            !source.contains(&sentinel_set) && !source.contains(&sentinel_remove),
            "slash_bug tests must use temp_env::with_var instead of raw unsafe env mutation"
        );
    }

    fn test_state() -> ReplState {
        let mut s = ReplState::default();
        s.model = Some("gpt-4o".to_string());
        s.session_id = Some("test-session-001".to_string());
        s.turn = 5;
        s.total_prompt_tokens = 1200;
        s.total_completion_tokens = 800;
        s
    }

    #[test]
    fn bug_report_contains_version() {
        let report = build_bug_report(&test_state());
        assert!(report.contains(env!("CARGO_PKG_VERSION")));
    }

    #[test]
    fn bug_report_contains_os() {
        let report = build_bug_report(&test_state());
        assert!(report.contains(std::env::consts::OS));
        assert!(report.contains(std::env::consts::ARCH));
    }

    #[test]
    fn bug_report_contains_model() {
        let report = build_bug_report(&test_state());
        assert!(report.contains("gpt-4o"));
    }

    #[test]
    fn bug_report_contains_session_id() {
        let report = build_bug_report(&test_state());
        assert!(report.contains("test-session-001"));
    }

    #[test]
    fn bug_report_contains_turn_count() {
        let report = build_bug_report(&test_state());
        assert!(report.contains("**Turns**: 5"));
    }

    #[test]
    fn bug_report_contains_token_counts() {
        let report = build_bug_report(&test_state());
        assert!(report.contains("prompt=1200"));
        assert!(report.contains("completion=800"));
    }

    #[test]
    fn bug_report_redacts_api_key() {
        temp_env::with_var("MO_API_KEY", Some("sk-secret-key-12345"), || {
            let report = build_bug_report(&test_state());
            assert!(
                !report.contains("sk-secret-key-12345"),
                "API key must be redacted"
            );
            assert!(report.contains("[REDACTED"));
        });
    }

    #[test]
    fn bug_report_shows_non_sensitive_env() {
        temp_env::with_var("MO_MODEL", Some("claude-sonnet-4"), || {
            let report = build_bug_report(&test_state());
            assert!(report.contains("claude-sonnet-4"));
        });
    }

    #[test]
    fn bug_report_truncates_long_response() {
        let mut state = test_state();
        state.last_response = Some("x".repeat(1000));
        let report = build_bug_report(&state);
        assert!(report.contains("truncated"));
        // Should contain exactly 500 x's in the code block, not 1000
        let code_block_start = report.find("```\n").unwrap() + 4;
        let code_block_end = report[code_block_start..].find("\n```").unwrap();
        assert_eq!(code_block_end, 500);
    }

    #[test]
    fn bug_report_none_session() {
        let state = ReplState::default();
        let report = build_bug_report(&state);
        assert!(report.contains("(none)"));
    }

    #[test]
    fn bug_report_has_template_sections() {
        let report = build_bug_report(&test_state());
        assert!(report.contains("## Steps to Reproduce"));
        assert!(report.contains("## Expected Behavior"));
        assert!(report.contains("## Actual Behavior"));
    }
}
