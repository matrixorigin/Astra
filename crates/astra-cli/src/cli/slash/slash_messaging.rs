//! `/messaging` slash command — inspect inter-agent messaging state.
//!
//! Shows the runtime's observed messaging metrics, not an inferred delivery
//! or retry status for messages without a production receipt owner.

use crate::cli::session::session_state::SessionState;
use crossterm::style::Stylize;

/// Handle `/messaging [subcommand]` command.
pub(crate) fn handle_messaging_command(arg: &str, state: &SessionState) {
    let parts: Vec<&str> = arg.split_whitespace().collect();
    let subcmd = parts.first().copied().unwrap_or("");

    match subcmd {
        "" | "metrics" => show_metrics(state),
        "help" | "?" => show_help(),
        _ => {
            eprintln!(
                "  {}",
                format!("Unknown subcommand: {subcmd}. Try /messaging help").yellow()
            );
        }
    }
}

fn show_metrics(state: &SessionState) {
    // Check if we have messaging metrics in the shared runtime.
    if let Some(ref metrics) = state.messaging_metrics {
        let snap = metrics.snapshot();
        eprintln!("\n  {}", "📊 Messaging Metrics".magenta().bold());
        eprintln!("  {}", "─".repeat(40).dim());
        eprintln!(
            "  {} {} sent, {} received, {} dropped",
            "Messages:".bold(),
            snap.messages_sent.to_string().green(),
            snap.messages_received.to_string().green(),
            if snap.messages_dropped > 0 {
                snap.messages_dropped.to_string().red()
            } else {
                snap.messages_dropped.to_string().dim()
            }
        );
        eprintln!(
            "  {} {} send, {} poll, {} broadcast lag",
            "Errors:".bold(),
            if snap.send_errors > 0 {
                snap.send_errors.to_string().red()
            } else {
                snap.send_errors.to_string().dim()
            },
            if snap.poll_errors > 0 {
                snap.poll_errors.to_string().red()
            } else {
                snap.poll_errors.to_string().dim()
            },
            if snap.broadcast_lag_events > 0 {
                snap.broadcast_lag_events.to_string().yellow()
            } else {
                snap.broadcast_lag_events.to_string().dim()
            }
        );
        // Latency
        if snap.delivery_latency.count > 0 {
            eprintln!(
                "  {} avg={}µs min={}µs max={}µs (n={})",
                "Delivery latency:".bold(),
                snap.delivery_latency.avg_us.to_string().magenta(),
                snap.delivery_latency.min_us.to_string().dim(),
                snap.delivery_latency.max_us.to_string().dim(),
                snap.delivery_latency.count.to_string().dim()
            );
        }
        eprintln!();
    } else {
        eprintln!(
            "  {}",
            "No messaging metrics available (no active delegation).".dim()
        );
    }
}

fn show_help() {
    eprintln!(
        "\n  {}",
        "/messaging — Inter-agent messaging inspector"
            .magenta()
            .bold()
    );
    eprintln!("  {}", "─".repeat(50).dim());
    eprintln!("  {}  Show metrics snapshot", "/messaging".bold());
    eprintln!("  {}  This help", "/messaging help".bold());
    eprintln!();
}

#[cfg(test)]
mod tests {
    use super::show_help;

    #[test]
    fn help_does_not_panic() {
        show_help();
    }
}
