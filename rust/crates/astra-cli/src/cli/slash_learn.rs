#![allow(unused_imports)]
use super::*;

/// Handle `/learn` command — show learning insights, drift detection, exploration.
#[allow(deprecated)]
pub(super) fn handle_learn_command(arg: &str, state: &ReplState) {
    use astra_runtime::pipeline::pattern::ExplorationReason;

    let lib = match &state.pattern_library {
        Some(pl) => pl.lock().unwrap_or_else(|e| e.into_inner()),
        None => {
            eprintln!(
                "  {} {}",
                "○".dim(),
                "Pattern library not initialized".dim()
            );
            return;
        }
    };

    let sub = arg.trim();

    match sub {
        "" | "stats" => {
            let summary = lib.learning_summary();
            eprintln!(
                "\n{}",
                "─── Learning Stats ─────────────────────────────".bold()
            );
            eprintln!(
                "  Patterns:     {} total, {} active, {} drifting",
                summary.total_patterns.to_string().cyan(),
                summary.active_patterns.to_string().green(),
                if summary.drifting_patterns > 0 {
                    summary.drifting_patterns.to_string().red().to_string()
                } else {
                    "0".green().to_string()
                },
            );
            eprintln!(
                "  Success rate: {}",
                format!("{:.0}%", summary.avg_success_rate * 100.0).cyan()
            );
            eprintln!(
                "  Exploration:  {} opportunities",
                if summary.exploration_opportunities > 0 {
                    summary
                        .exploration_opportunities
                        .to_string()
                        .yellow()
                        .to_string()
                } else {
                    "0".green().to_string()
                }
            );

            if !summary.top_patterns.is_empty() {
                eprintln!();
                eprintln!("  {} Top Patterns:", "●".cyan());
                for (sig, score) in &summary.top_patterns {
                    let bar_len = (score * 20.0) as usize;
                    let bar = "█".repeat(bar_len);
                    let rest = "░".repeat(20 - bar_len);
                    eprintln!(
                        "    {}{} {:.2}  {}",
                        bar.green(),
                        rest.dim(),
                        score,
                        sig.as_str().dim()
                    );
                }
            }
            eprintln!(
                "{}",
                "────────────────────────────────────────────────".dim()
            );
            eprintln!();
        }
        "drift" => {
            let reports = lib.detect_drift();
            eprintln!(
                "\n{}",
                "─── Drift Detection ────────────────────────────".bold()
            );
            if reports.is_empty() {
                eprintln!("  {} No drifting patterns detected", theme::icon_ok());
            } else {
                eprintln!(
                    "  {} {} pattern(s) drifting:",
                    theme::icon_warn(),
                    reports.len()
                );
                eprintln!();
                for r in &reports {
                    let severity = if r.is_critical {
                        "CRITICAL".red().to_string()
                    } else {
                        "WARNING".yellow().to_string()
                    };
                    eprintln!("  {} {}", severity, r.signature.as_str().cyan());
                    eprintln!(
                        "    Historical: {:.0}% → Recent: {:.0}%  (drift: {:.2})",
                        r.historical_success_rate * 100.0,
                        r.recent_success_rate * 100.0,
                        r.drift_score
                    );
                    let domain_str = r
                        .domain
                        .map(|d| format!("{d:?}"))
                        .unwrap_or_else(|| "—".to_string());
                    eprintln!(
                        "    Task: {:?}  Domain: {}  Obs: {}",
                        r.task_type, domain_str, r.total_observations
                    );
                    eprintln!();
                }
            }
            eprintln!(
                "{}",
                "────────────────────────────────────────────────".dim()
            );
            eprintln!();
        }
        "explore" => {
            let opps = lib.exploration_opportunities();
            eprintln!(
                "\n{}",
                "─── Exploration Opportunities ──────────────────".bold()
            );
            if opps.is_empty() {
                eprintln!(
                    "  {} All domains have sufficient confidence",
                    theme::icon_ok()
                );
            } else {
                for opp in &opps {
                    let reason_str = match opp.reason {
                        ExplorationReason::ColdStart => "Cold start".yellow().to_string(),
                        ExplorationReason::Drift => "Drift".red().to_string(),
                        ExplorationReason::LowSuccess => "Low success".yellow().to_string(),
                    };
                    let domain_str = opp
                        .domain
                        .map(|d| format!("{d:?}"))
                        .unwrap_or_else(|| "—".to_string());
                    eprintln!(
                        "  {} {:?} / {}  (confidence: {:.0}%, {} patterns)",
                        reason_str,
                        opp.task_type,
                        domain_str.cyan(),
                        opp.confidence * 100.0,
                        opp.pattern_count,
                    );
                    if !opp.known_tools.is_empty() {
                        eprintln!("    Known tools: {}", opp.known_tools.join(", ").dim());
                    }
                }
            }
            eprintln!(
                "{}",
                "────────────────────────────────────────────────".dim()
            );
            eprintln!();
        }
        _ => {
            eprintln!();
            eprintln!("  {}", "Usage:".bold());
            eprintln!("    /learn          Show learning summary (same as /learn stats)");
            eprintln!("    /learn stats    Pattern library statistics");
            eprintln!("    /learn drift    Detect drifting patterns");
            eprintln!("    /learn explore  Show exploration opportunities");
            eprintln!();
        }
    }
}
