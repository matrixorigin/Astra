#![allow(unused_imports)]
use super::*;
use crate::{cli_dim, cli_err, cli_info, cli_ok, cli_section, cli_warn};

/// Handle `/sync` command — show unified sync state across all domains.
/// Subcommands: `/sync push`, `/sync pull`, `/sync log`.
pub(super) async fn handle_sync_command(arg: &str, state: &ReplState) {
    let sub = arg.trim();

    // /sync push — force-push all dirty domains
    if sub == "push" {
        return handle_sync_push(state).await;
    }

    // /sync pull — force-pull all pullable domains from cloud
    if sub == "pull" {
        return handle_sync_pull(state).await;
    }

    let show_log = sub == "log";

    cli_section!("Sync Engine Status");
    eprintln!();

    let Some(mc) = state.matrix_runtime.as_ref() else {
        cli_dim!("Sync orchestrator not initialized (no cloud connection)");
        eprintln!();
        return;
    };
    let orch = mc.sync_orchestrator_lock().await;

    // Cloud availability
    let cloud_status = if orch.is_cloud_available() {
        "● Connected".green().to_string()
    } else {
        "○ Offline".dim().to_string()
    };
    eprintln!("  Cloud: {cloud_status}");
    eprintln!();

    // Per-domain status
    eprintln!(
        "  {:<14} {:<12} {:>8} {:>8} {:>8} {:>8}",
        "Domain".bold(),
        "State".bold(),
        "Pushes".bold(),
        "Pulls".bold(),
        "Conflicts".bold(),
        "Errors".bold(),
    );
    let mut domains = orch.status_summary();
    domains.sort_by_key(|(d, _)| format!("{d}"));
    for (domain, sync_state) in &domains {
        let state_str = match sync_state {
            astra_services::SyncState::Clean => "✓ clean".green().to_string(),
            astra_services::SyncState::Dirty => "● dirty".yellow().to_string(),
            astra_services::SyncState::Syncing => "↻ syncing".cyan().to_string(),
            astra_services::SyncState::Pulling => "↓ pulling".cyan().to_string(),
            astra_services::SyncState::Conflict { .. } => "⚠ conflict".red().to_string(),
            astra_services::SyncState::Error { retry_count, .. } => {
                format!("✗ error({})", retry_count).red().to_string()
            }
        };
        let stats = orch.domain_stats(*domain).unwrap_or_default();
        eprintln!(
            "  {:<14} {:<12} {:>8} {:>8} {:>8} {:>8}",
            format!("{domain}").cyan(),
            state_str,
            stats.pushes,
            stats.pulls,
            stats.conflicts,
            stats.errors,
        );
    }

    // Sync event log
    if show_log {
        let events = orch.event_log();
        if events.is_empty() {
            eprintln!();
            cli_dim!("No sync events yet.");
        } else {
            cli_section!("Sync Event Log");
            eprintln!();
            eprintln!(
                "  {:<10} {:<12} {:<8} {:>8} {:>10}",
                "Domain".bold(),
                "Operation".bold(),
                "Result".bold(),
                "Duration".bold(),
                "Bytes".bold(),
            );
            for event in events.iter().rev().take(20) {
                let op_str = format!("{:?}", event.operation).to_lowercase();
                let result = if event.success {
                    "✓ ok".green().to_string()
                } else {
                    event.error.as_deref().unwrap_or("fail").red().to_string()
                };
                eprintln!(
                    "  {:<10} {:<12} {:<8} {:>6}ms {:>10}",
                    format!("{}", event.domain),
                    op_str,
                    result,
                    event.duration_ms,
                    if event.bytes_transferred > 0 {
                        format_bytes(event.bytes_transferred)
                    } else {
                        "-".to_string()
                    },
                );
            }
        }
    } else {
        eprintln!();
        cli_dim!("Use /sync log | push | pull");
    }
    eprintln!();
}

/// Force-push all dirty sync domains to cloud.
async fn handle_sync_push(state: &ReplState) {
    cli_section!("Sync Push");
    eprintln!();

    let Some(mc) = state.matrix_runtime.as_ref() else {
        cli_dim!("No cloud connection — nothing to push.");
        eprintln!();
        return;
    };

    let mut orch = mc.sync_orchestrator_lock().await;

    // Check dirty count before push
    let dirty_count = orch
        .status_summary()
        .iter()
        .filter(|(_, s)| s.is_dirty())
        .count();
    if dirty_count == 0 {
        cli_ok!("All domains clean — nothing to push.");
        eprintln!();
        return;
    }

    cli_info!(
        "Pushing {} dirty domain{}...",
        dirty_count,
        if dirty_count == 1 { "" } else { "s" }
    );
    eprintln!();

    let results = orch.push_dirty().await;
    drop(orch); // release lock before printing

    let mut ok_count = 0usize;
    let mut fail_count = 0usize;
    for r in &results {
        if r.success {
            ok_count += 1;
            let version_str = r
                .version
                .map(|v| format!("v{v}"))
                .unwrap_or_else(|| "-".into());
            eprintln!(
                "  {} {:<14} {} ({}ms)",
                theme::icon_ok(),
                format!("{}", r.domain).cyan(),
                version_str.dim(),
                r.duration_ms,
            );
        } else {
            fail_count += 1;
            let err = r.error.as_deref().unwrap_or("unknown error");
            eprintln!(
                "  {} {:<14} {}",
                theme::icon_err(),
                format!("{}", r.domain).cyan(),
                err.red(),
            );
        }
    }

    eprintln!();
    if fail_count == 0 {
        cli_ok!(
            "{} domain{} pushed successfully.",
            ok_count,
            if ok_count == 1 { "" } else { "s" }
        );
    } else {
        eprintln!(
            "  {} pushed, {} failed.",
            format!("{ok_count} ✓").green(),
            format!("{fail_count} ✗").red(),
        );
    }
    eprintln!();
}

/// Force-pull all pullable domains from cloud (skips write-only domains like Events).
async fn handle_sync_pull(state: &ReplState) {
    cli_section!("Sync Pull");
    eprintln!();

    let Some(mc) = state.matrix_runtime.as_ref() else {
        cli_dim!("No cloud connection — nothing to pull.");
        eprintln!();
        return;
    };

    let mut orch = mc.sync_orchestrator_lock().await;
    cli_info!("Pulling from cloud...");
    eprintln!();

    let results = orch.pull_all().await;
    drop(orch);

    if results.is_empty() {
        cli_dim!("No pullable domains configured.");
        eprintln!();
        return;
    }

    let mut ok_count = 0usize;
    let mut fail_count = 0usize;
    for r in &results {
        if r.success {
            ok_count += 1;
            let version_str = r
                .version
                .map(|v| format!("v{v}"))
                .unwrap_or_else(|| "-".into());
            let merge_str = r
                .merge
                .as_ref()
                .map(|m| {
                    let total = m.items_added + m.items_updated;
                    if total > 0 {
                        format!(" (+{} added, ~{} updated)", m.items_added, m.items_updated)
                    } else {
                        String::new()
                    }
                })
                .unwrap_or_default();
            eprintln!(
                "  {} {:<14} {}{} ({}ms)",
                theme::icon_ok(),
                format!("{}", r.domain).cyan(),
                version_str.dim(),
                merge_str.dim(),
                r.duration_ms,
            );
        } else {
            fail_count += 1;
            let err = r.error.as_deref().unwrap_or("unknown error");
            eprintln!(
                "  {} {:<14} {}",
                theme::icon_err(),
                format!("{}", r.domain).cyan(),
                err.red(),
            );
        }
    }

    eprintln!();
    if fail_count == 0 {
        cli_ok!(
            "{} domain{} pulled successfully.",
            ok_count,
            if ok_count == 1 { "" } else { "s" }
        );
    } else {
        eprintln!(
            "  {} pulled, {} failed.",
            format!("{ok_count} ✓").green(),
            format!("{fail_count} ✗").red(),
        );
    }
    eprintln!();
}

pub(super) fn format_bytes(bytes: u64) -> String {
    if bytes < 1024 {
        format!("{}B", bytes)
    } else if bytes < 1024 * 1024 {
        format!("{:.1}KB", bytes as f64 / 1024.0)
    } else {
        format!("{:.1}MB", bytes as f64 / (1024.0 * 1024.0))
    }
}
