//! `/messaging` slash command — inspect inter-agent messaging state.
//!
//! Subcommands:
//! - `/messaging` (no args): Show current metrics snapshot
//! - `/messaging dlq`: Show dead letter queue summary
//! - `/messaging status`: Show mailbox status if available

use super::*;

/// Handle `/messaging [subcommand]` command.
pub(super) fn handle_messaging_command(arg: &str, state: &ReplState) {
    let parts: Vec<&str> = arg.split_whitespace().collect();
    let subcmd = parts.first().copied().unwrap_or("");

    match subcmd {
        "" | "metrics" => show_metrics(state),
        "dlq" | "deadletter" => show_dlq(state),
        "status" => show_status(state),
        "help" | "?" => show_help(),
        _ => {
            eprintln!(
                "  {}",
                format!("Unknown subcommand: {subcmd}. Try /messaging help").yellow()
            );
        }
    }
}

fn show_metrics(state: &ReplState) {
    // Check if we have messaging metrics in the shared runtime.
    if let Some(ref metrics) = state.messaging_metrics {
        let snap = metrics.snapshot();
        eprintln!("\n  {}", "📊 Messaging Metrics".cyan().bold());
        eprintln!("  {}", "─".repeat(40).dim());
        eprintln!(
            "  {} {} sent, {} received, {} dropped",
            "Messages:".white().bold(),
            snap.messages_sent.to_string().green(),
            snap.messages_received.to_string().green(),
            if snap.messages_dropped > 0 {
                snap.messages_dropped.to_string().red()
            } else {
                snap.messages_dropped.to_string().dim()
            }
        );
        eprintln!(
            "  {} {} sent, {} received",
            "Acks:".white().bold(),
            snap.acks_sent.to_string().green(),
            snap.acks_received.to_string().green()
        );
        eprintln!(
            "  {} {} sent, {} received",
            "Nacks:".white().bold(),
            if snap.nacks_sent > 0 {
                snap.nacks_sent.to_string().yellow()
            } else {
                snap.nacks_sent.to_string().dim()
            },
            if snap.nacks_received > 0 {
                snap.nacks_received.to_string().yellow()
            } else {
                snap.nacks_received.to_string().dim()
            }
        );
        eprintln!(
            "  {} {} retries, {} dead-lettered",
            "Failures:".white().bold(),
            if snap.retries > 0 {
                snap.retries.to_string().yellow()
            } else {
                snap.retries.to_string().dim()
            },
            if snap.dead_letters > 0 {
                snap.dead_letters.to_string().red()
            } else {
                snap.dead_letters.to_string().dim()
            }
        );
        eprintln!(
            "  {} {} send, {} poll, {} broadcast lag",
            "Errors:".white().bold(),
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
                "Delivery latency:".white().bold(),
                snap.delivery_latency.avg_us.to_string().cyan(),
                snap.delivery_latency.min_us.to_string().dim(),
                snap.delivery_latency.max_us.to_string().dim(),
                snap.delivery_latency.count.to_string().dim()
            );
        }
        if snap.ack_latency.count > 0 {
            eprintln!(
                "  {} avg={}µs min={}µs max={}µs (n={})",
                "Ack latency:".white().bold(),
                snap.ack_latency.avg_us.to_string().cyan(),
                snap.ack_latency.min_us.to_string().dim(),
                snap.ack_latency.max_us.to_string().dim(),
                snap.ack_latency.count.to_string().dim()
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

fn show_dlq(state: &ReplState) {
    if let Some(ref dlq) = state.dead_letter_queue {
        let rt = tokio::runtime::Handle::try_current();
        if let Ok(rt) = rt {
            let summary = rt.block_on(dlq.reason_summary());
            eprintln!("\n  {}", "📭 Dead Letter Queue".red().bold());
            eprintln!("  {}", "─".repeat(40).dim());
            eprintln!(
                "  {} {} messages",
                "Total:".white().bold(),
                summary.total.to_string().red()
            );
            if summary.ack_timeouts > 0 {
                eprintln!(
                    "    {} ack timeouts",
                    summary.ack_timeouts.to_string().yellow()
                );
            }
            if summary.rejections > 0 {
                eprintln!(
                    "    {} rejected (nack)",
                    summary.rejections.to_string().yellow()
                );
            }
            if summary.transport_failures > 0 {
                eprintln!(
                    "    {} transport failures",
                    summary.transport_failures.to_string().yellow()
                );
            }
            if summary.expired > 0 {
                eprintln!("    {} expired (TTL)", summary.expired.to_string().dim());
            }
            eprintln!();

            // List recent entries
            let recent = rt.block_on(dlq.list_page(0, 5));
            if !recent.is_empty() {
                eprintln!("  {}", "Recent entries:".white().bold());
                for dl in recent {
                    let reason_str = match &dl.reason {
                        astra_messaging::DeadLetterReason::AckTimeout { attempts } => {
                            format!("ack timeout ({attempts} attempts)")
                        }
                        astra_messaging::DeadLetterReason::Rejected { reason } => {
                            format!("rejected: {}", reason.as_deref().unwrap_or("no reason"))
                        }
                        astra_messaging::DeadLetterReason::TransportFailure { error } => {
                            format!("transport: {error}")
                        }
                        astra_messaging::DeadLetterReason::Expired => "expired".into(),
                    };
                    let short_id = if dl.message.id.len() >= 8 {
                        &dl.message.id[..8]
                    } else {
                        &dl.message.id
                    };
                    eprintln!(
                        "    {} {} → {}",
                        short_id.dim(),
                        dl.message.from.agent_id.clone().cyan(),
                        reason_str.yellow()
                    );
                }
            }
            eprintln!();
        } else {
            eprintln!("  {}", "Cannot access DLQ outside tokio runtime.".dim());
        }
    } else {
        eprintln!(
            "  {}",
            "No dead letter queue available (no active delegation).".dim()
        );
    }
}

fn show_status(state: &ReplState) {
    eprintln!("\n  {}", "📬 Mailbox Status".blue().bold());
    eprintln!("  {}", "─".repeat(40).dim());

    let has_metrics = state.messaging_metrics.is_some();
    let has_dlq = state.dead_letter_queue.is_some();

    eprintln!(
        "  {} {}",
        "Metrics:".white().bold(),
        if has_metrics {
            "active".green()
        } else {
            "not available".dim()
        }
    );
    eprintln!(
        "  {} {}",
        "Dead Letter Queue:".white().bold(),
        if has_dlq {
            "active".green()
        } else {
            "not available".dim()
        }
    );

    eprintln!(
        "\n  {}",
        "Note: Messaging state is per-delegation, not per-session.".dim()
    );
    eprintln!();
}

fn show_help() {
    eprintln!(
        "\n  {}",
        "/messaging — Inter-agent messaging inspector".cyan().bold()
    );
    eprintln!("  {}", "─".repeat(50).dim());
    eprintln!("  {}  Show metrics snapshot", "/messaging".white().bold());
    eprintln!(
        "  {}  Show dead letter queue",
        "/messaging dlq".white().bold()
    );
    eprintln!(
        "  {}  Show mailbox status",
        "/messaging status".white().bold()
    );
    eprintln!("  {}  This help", "/messaging help".white().bold());
    eprintln!();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn help_does_not_panic() {
        show_help();
    }
}
