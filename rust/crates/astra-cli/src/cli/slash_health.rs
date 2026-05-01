#![allow(unused_imports)]
use super::*;

pub(super) async fn handle_health_command(arg: &str, state: &ReplState) {
    use astra_turn_core::tool_health::ToolHealthTracker;

    let detail = arg.trim() == "detail";

    // Build a live tracker from persisted entries for rich analysis
    let tracker = ToolHealthTracker::from_entries(&state.tool_health_entries);
    let summary = tracker.summary();

    // Header
    eprintln!(
        "\n{}",
        "─── Tool Health Dashboard ──────────────────────".bold()
    );

    if summary.total_tools == 0 {
        eprintln!(
            "  {}",
            "No tool health data yet (run some turns first).".dim()
        );
    } else {
        // Overall status
        let status = if summary.deprioritized_count > 0 || summary.flaky_count > 0 {
            "⚠ Degraded".yellow().to_string()
        } else if summary.total_errors > 0 {
            "● Minor issues".to_string()
        } else {
            "✓ Healthy".green().to_string()
        };
        eprintln!("  Status: {status}");
        eprintln!(
            "  Tools: {}  Errors: {}  Timeouts: {}  Cache hits: {}",
            summary.total_tools.to_string().cyan(),
            if summary.total_errors > 0 {
                summary.total_errors.to_string().red().to_string()
            } else {
                "0".to_string()
            },
            if summary.total_timeouts > 0 {
                summary.total_timeouts.to_string().yellow().to_string()
            } else {
                "0".to_string()
            },
            summary.total_cache_hits,
        );
        if summary.deprioritized_count > 0 {
            eprintln!(
                "  {} deprioritized, {} flaky",
                summary.deprioritized_count.to_string().red(),
                summary.flaky_count,
            );
        }
        eprintln!();

        if detail {
            // Per-tool breakdown
            eprintln!(
                "  {:<20} {:>5} {:>5} {:>4} {:>5} {:>5}  {}",
                "tool".bold(),
                "calls".bold(),
                "fail".bold(),
                "TO".bold(),
                "cache".bold(),
                "rehab".bold(),
                "status".bold(),
            );
            let all = tracker.all();
            let mut sorted: Vec<_> = all.iter().collect();
            sorted.sort_by_key(|x| std::cmp::Reverse(x.1.total_failures));
            for (name, health) in &sorted {
                let status_str = if health.deprioritized {
                    "⛔ deprioritized".red().to_string()
                } else if health.rehabilitation_count >= 2 {
                    "⚠ flaky".yellow().to_string()
                } else if health.total_failures > 0 {
                    "● recovering".to_string()
                } else {
                    "✓ healthy".green().to_string()
                };
                eprintln!(
                    "  {:<20} {:>5} {:>5} {:>4} {:>5} {:>5}  {}",
                    name.as_str().cyan(),
                    health.total_calls,
                    health.total_failures,
                    health.timeout_count,
                    health.cache_hit_count,
                    health.rehabilitation_count,
                    status_str,
                );
            }
            eprintln!();

            // Timeout-dominant tools
            let timeout_tools = tracker.timeout_dominant_tools();
            if !timeout_tools.is_empty() {
                eprintln!(
                    "  {} Timeout-dominant (≥70% infra): {}",
                    "⏱".bold(),
                    timeout_tools.join(", ").yellow()
                );
            }
            // Cache-wasteful tools
            let cache_tools = tracker.cache_wasteful_tools(3);
            if !cache_tools.is_empty() {
                let names: Vec<String> = cache_tools
                    .iter()
                    .map(|(n, c)| format!("{n}({c}×)"))
                    .collect();
                eprintln!("  {} Duplicate calls: {}", "♻".bold(), names.join(", "));
            }
        } else {
            // Compact view: only show problematic tools
            let deprioritized = tracker.deprioritized_tools();
            if !deprioritized.is_empty() {
                eprintln!(
                    "  {} {}",
                    "Deprioritized:".red(),
                    deprioritized.join(", ").red()
                );
            }
            let all = tracker.all();
            let recovering: Vec<&str> = all
                .iter()
                .filter(|(_, h)| h.total_failures > 0 && !h.deprioritized)
                .map(|(n, _)| n.as_str())
                .collect();
            if !recovering.is_empty() {
                eprintln!("  {} {}", "With errors:".yellow(), recovering.join(", "));
            }
            if !detail {
                eprintln!("  {}", "Use /health detail for per-tool breakdown.".dim());
            }
        }
    }

    // ── Cloud Sync Status ──
    eprintln!(
        "\n{}",
        "─── Cloud Sync ─────────────────────────────────".bold()
    );
    match &state.matrix_runtime {
        None => {
            eprintln!(
                "  {} {}",
                "○".dim(),
                "Offline — no MatrixOne connection".dim()
            );
            eprintln!("  {}", "Set MATRIXONE_HOST to enable cloud sync.".dim());
        }
        Some(mc) => {
            let sync_status =
                astra_services::state_sync::StateSyncService::status(mc.sync_service().as_ref())
                    .await;
            display_sync_status(&sync_status);
        }
    }

    eprintln!(
        "{}",
        "────────────────────────────────────────────────".dim()
    );
    eprintln!();
}

/// Render cloud sync status section.
pub(super) fn display_sync_status(status: &astra_services::SyncStatus) {
    // Connection confirmed — show details
    let overall = if status.last_error.is_some() {
        "⚠ Error".yellow().to_string()
    } else if status.pending_pushes > 0 {
        "● Pending".yellow().to_string()
    } else if status.learning_last_push.is_some() || status.learning_last_pull.is_some() {
        "✓ Connected".green().to_string()
    } else {
        "○ No sync history".to_string()
    };
    eprintln!("  Status: {overall}");

    // Last push
    match &status.learning_last_push {
        Some(ts) => {
            let age = format_sync_age(ts);
            eprintln!("  Last push:  {} ({})", ts.as_str().cyan(), age);
        }
        None => eprintln!("  Last push:  {}", "never".dim()),
    }

    // Last pull
    match &status.learning_last_pull {
        Some(ts) => {
            let age = format_sync_age(ts);
            eprintln!("  Last pull:  {} ({})", ts.as_str().cyan(), age);
        }
        None => eprintln!("  Last pull:  {}", "never".dim()),
    }

    // Preferences
    if let Some(ts) = &status.preferences_last_sync {
        eprintln!("  Prefs sync: {}", ts.as_str().cyan());
    }

    // Pending pushes
    if status.pending_pushes > 0 {
        eprintln!(
            "  Pending:    {}",
            format!("{} operations queued", status.pending_pushes).yellow()
        );
    }

    // Last error
    if let Some(err) = &status.last_error {
        let short = truncate_str(err, 80);
        eprintln!("  Last error: {}", short.red());
    }
}

/// Format an ISO 8601 timestamp as relative age (e.g., "3m ago", "2h ago").
pub(super) fn format_sync_age(ts: &str) -> String {
    // Try to parse ISO 8601 timestamps in common formats
    let now = chrono::Utc::now();
    let parsed = chrono::DateTime::parse_from_rfc3339(ts)
        .or_else(|_| chrono::DateTime::parse_from_str(ts, "%Y-%m-%d %H:%M:%S%.f%z"))
        .or_else(|_| {
            // MySQL DATETIME format (no timezone) — assume UTC
            chrono::NaiveDateTime::parse_from_str(ts, "%Y-%m-%d %H:%M:%S")
                .or_else(|_| chrono::NaiveDateTime::parse_from_str(ts, "%Y-%m-%d %H:%M:%S%.f"))
                .map(|naive| {
                    naive
                        .and_utc()
                        .with_timezone(&chrono::FixedOffset::east_opt(0).expect("UTC offset"))
                })
        });
    match parsed {
        Ok(dt) => {
            let dur = now.signed_duration_since(dt);
            if dur.num_seconds() < 0 {
                "just now".to_string()
            } else if dur.num_seconds() < 60 {
                format!("{}s ago", dur.num_seconds())
            } else if dur.num_minutes() < 60 {
                format!("{}m ago", dur.num_minutes())
            } else if dur.num_hours() < 24 {
                format!("{}h ago", dur.num_hours())
            } else {
                format!("{}d ago", dur.num_days())
            }
        }
        Err(_) => ts.to_string(), // Fallback: show raw timestamp
    }
}
