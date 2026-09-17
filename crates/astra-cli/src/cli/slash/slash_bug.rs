use crate::cli::session::session_state::SessionState;
use crate::{cli_dim, cli_err, cli_ok, cli_warn};

/// Handle the `/bug` slash command — generate a diagnostic report for bug reports.
///
/// Usage:
///   /bug           — print diagnostic report to terminal
///   /bug copy      — copy report to clipboard
///   /bug save      — save report to file in cwd
pub(crate) fn handle_bug_command(arg: &str, state: &SessionState) {
    let report = build_bug_report(state);

    match arg.trim() {
        "copy" => match crate::cli::slash::slash_info::copy_to_clipboard(&report) {
            Ok(()) => cli_ok!("Bug report copied to clipboard."),
            Err(error) => {
                cli_warn!("Could not copy to clipboard: {}", error);
                cli_warn!("Printing bug report instead:");
                eprintln!("{report}");
            }
        },
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

fn build_bug_report(state: &SessionState) -> String {
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
            crate::cli::slash::slash_stats::format_cost(state.total_session_cost)
        ));
    }
    lines.push(String::new());

    // ── Configuration ──
    lines.push("## Configuration".to_string());
    lines.push(String::new());

    // Redacted env vars — show presence but not values for sensitive ones.
    // LLM model / API key are stored server-side in infra_llm_models — inspect via
    // `astra admin model list` / `astra admin config list`.
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
