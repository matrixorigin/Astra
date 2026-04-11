// Auto-Tuning Loop CLI commands
//
// /tuning status          - Show auto-tuning system status
// /tuning rules           - List all evolution rules
// /tuning feedback        - Show recent feedback signals
// /tuning history         - Show rule execution history
// /tuning enable          - Enable auto-tuning
// /tuning disable         - Disable auto-tuning
// /tuning cycle           - Run one evaluation cycle manually
// /tuning record <signal> - Record a manual feedback signal
// /tuning help            - Show help

use super::*;
use astra_runtime::auto_tuning::{
    AlertSeverity, AutoTuningEngine, EvolutionAction, EvolutionRule, EvolutionTrigger,
    FeedbackSignal, RollbackCondition, RuleExecution, Sentiment, SignalType,
};
use astra_runtime::runtime_config::RuntimeConfig;
use std::io::Write;
use std::sync::Arc;
use std::time::Duration;

/// Context for tuning commands
pub struct TuningCommandContext<'a, W: Write> {
    pub engine: &'a Arc<AutoTuningEngine>,
    pub runtime_config: &'a mut RuntimeConfig,
    pub writer: &'a mut W,
}

/// Handle /tuning command
pub fn handle_tuning_command<W: Write>(
    args: &str,
    ctx: TuningCommandContext<'_, W>,
) -> std::io::Result<()> {
    let parts: Vec<&str> = args.split_whitespace().collect();
    let subcommand = parts.first().copied().unwrap_or("status");

    match subcommand {
        "status" => cmd_status(ctx),
        "rules" => cmd_rules(ctx),
        "feedback" => cmd_feedback(parts.get(1).copied(), ctx),
        "history" => cmd_history(parts.get(1).copied(), ctx),
        "enable" => cmd_enable(ctx),
        "disable" => cmd_disable(ctx),
        "cycle" => cmd_cycle(ctx),
        "record" => cmd_record(&parts[1..], ctx),
        "help" | "-h" | "--help" => cmd_help(ctx),
        other => {
            writeln!(ctx.writer, "{} Unknown subcommand: {}", "✗".red(), other)?;
            cmd_help(ctx)
        }
    }
}

// ─── Status ─────────────────────────────────────────────────────────────────

fn cmd_status<W: Write>(ctx: TuningCommandContext<'_, W>) -> std::io::Result<()> {
    let w = ctx.writer;

    // Header
    writeln!(w, "\n{}", "Auto-Tuning System Status".cyan().bold())?;
    writeln!(w, "{}", "─".repeat(50).dim())?;

    // Enabled status
    let enabled = ctx.engine.is_enabled();
    let enabled_str = if enabled {
        "Enabled".green().bold().to_string()
    } else {
        "Disabled".red().bold().to_string()
    };
    writeln!(w, "Status:    {}", enabled_str)?;

    // Rules count
    let rules = ctx.engine.get_rules();
    writeln!(
        w,
        "Rules:     {} configured",
        rules.len().to_string().cyan()
    )?;

    // Active rules (enabled)
    let active_rules = rules.iter().filter(|r| r.enabled).count();
    writeln!(w, "Active:    {} enabled", active_rules.to_string().green())?;

    // Feedback signals (would need FeedbackAggregator access)
    // For now, show executions as proxy
    let executions = ctx.engine.get_executions();
    writeln!(
        w,
        "Executions: {} rule executions",
        executions.len().to_string().yellow()
    )?;

    // Recent rollbacks
    let rollbacks = executions.iter().filter(|e| e.rolled_back).count();
    if rollbacks > 0 {
        writeln!(
            w,
            "Rollbacks: {} (auto-reverted)",
            rollbacks.to_string().red()
        )?;
    }

    // Current config values (relevant to auto-tuning)
    writeln!(w, "\n{}", "Current Config Values".cyan())?;
    writeln!(w, "{}", "─".repeat(50).dim())?;

    let config = &ctx.runtime_config;
    writeln!(
        w,
        "  tool_selection.confidence_threshold: {}",
        format!("{:.2}", config.tool_selection.confidence_threshold).yellow()
    )?;
    writeln!(
        w,
        "  tool_selection.max_tools: {}",
        config.tool_selection.max_tools.to_string().yellow()
    )?;
    writeln!(
        w,
        "  token_budget.max_prompt_tokens: {}",
        config.token_budget.max_prompt_tokens.to_string().yellow()
    )?;

    writeln!(w)?;
    Ok(())
}

// ─── Rules ──────────────────────────────────────────────────────────────────

fn cmd_rules<W: Write>(ctx: TuningCommandContext<'_, W>) -> std::io::Result<()> {
    let w = ctx.writer;
    let rules = ctx.engine.get_rules();

    writeln!(w, "\n{}", "Evolution Rules".cyan().bold())?;
    writeln!(w, "{}", "─".repeat(70).dim())?;

    if rules.is_empty() {
        writeln!(w, "  No rules configured.")?;
        writeln!(w, "  Use default_rules() to add preset rules.")?;
        writeln!(w)?;
        return Ok(());
    }

    for rule in &rules {
        print_rule(w, rule)?;
        writeln!(w)?;
    }

    Ok(())
}

fn print_rule<W: Write>(w: &mut W, rule: &EvolutionRule) -> std::io::Result<()> {
    // Rule ID and name
    let enabled_icon = if rule.enabled { "●" } else { "○" };
    let enabled_color = if rule.enabled {
        enabled_icon.green().to_string()
    } else {
        enabled_icon.dim().to_string()
    };

    writeln!(
        w,
        "{} {} {}",
        enabled_color,
        rule.id.clone().cyan().bold(),
        if rule.name.is_empty() {
            "".to_string()
        } else {
            format!("({})", rule.name).dim().to_string()
        }
    )?;

    // Trigger
    writeln!(
        w,
        "  {}: {}",
        "Trigger".yellow(),
        format_trigger(&rule.trigger)
    )?;

    // Action
    writeln!(
        w,
        "  {}: {}",
        "Action".yellow(),
        format_action(&rule.action)
    )?;

    // Cooldown
    if rule.cooldown > Duration::ZERO {
        writeln!(
            w,
            "  {}: {}",
            "Cooldown".dim(),
            format_duration(rule.cooldown).dim()
        )?;
    }

    // Rollback condition
    if let Some(ref rb) = rule.rollback_condition {
        writeln!(w, "  {}: {}", "Rollback".dim(), format_rollback(rb).dim())?;
    }

    Ok(())
}

fn format_trigger(trigger: &EvolutionTrigger) -> String {
    match trigger {
        EvolutionTrigger::LowSuccessRate {
            threshold,
            window_secs,
            min_samples,
        } => {
            format!(
                "Success rate < {:.0}% ({}s window, min {} samples)",
                threshold * 100.0,
                window_secs,
                min_samples
            )
        }
        EvolutionTrigger::HighTokenUsage {
            threshold_tokens,
            window_secs,
            min_samples,
        } => {
            format!(
                "Avg tokens > {} ({}s window, min {} samples)",
                threshold_tokens, window_secs, min_samples
            )
        }
        EvolutionTrigger::HighRetryRate {
            threshold,
            window_secs,
            min_samples,
        } => {
            format!(
                "Retry rate > {:.0}% ({}s window, min {} samples)",
                threshold * 100.0,
                window_secs,
                min_samples
            )
        }
        EvolutionTrigger::NegativeFeedbackStreak { count } => {
            format!("{} consecutive negative signals", count)
        }
        EvolutionTrigger::PatternDrift { confidence_drop } => {
            format!(
                "Pattern confidence drops by {:.0}%",
                confidence_drop * 100.0
            )
        }
        EvolutionTrigger::SignalAccumulation {
            signal_type,
            count,
            window_secs,
        } => {
            format!("{}x {} signals in {}s", count, signal_type, window_secs)
        }
    }
}

fn format_action(action: &EvolutionAction) -> String {
    match action {
        EvolutionAction::AdjustConfig {
            path,
            delta,
            min,
            max,
        } => {
            let sign = if *delta >= 0.0 { "+" } else { "" };
            let bounds = match (min, max) {
                (Some(mi), Some(ma)) => format!(" [{:.2}..{:.2}]", mi, ma),
                (Some(mi), None) => format!(" [≥{:.2}]", mi),
                (None, Some(ma)) => format!(" [≤{:.2}]", ma),
                (None, None) => String::new(),
            };
            format!("{} {}{:.2}{}", path, sign, delta, bounds)
        }
        EvolutionAction::SetConfig { path, value } => {
            format!("{} = {}", path, value)
        }
        EvolutionAction::SwitchStrategy {
            strategy_key,
            new_value,
        } => {
            format!("Switch {}: → {}", strategy_key, new_value)
        }
        EvolutionAction::EnableExperiment { experiment_id } => {
            format!("Enable experiment: {}", experiment_id)
        }
        EvolutionAction::DisableExperiment { experiment_id } => {
            format!("Disable experiment: {}", experiment_id)
        }
        EvolutionAction::ResetConfig { path } => {
            format!("Reset {} to default", path)
        }
        EvolutionAction::Alert { message, severity } => {
            let sev = match severity {
                AlertSeverity::Info => "INFO",
                AlertSeverity::Warning => "WARN",
                AlertSeverity::Error => "ERROR",
            };
            format!("[{}] {}", sev, message)
        }
    }
}

fn format_rollback(condition: &RollbackCondition) -> String {
    match condition {
        RollbackCondition::SuccessRateDrops {
            threshold,
            window_secs,
        } => {
            format!(
                "if success rate < {:.0}% in {}s",
                threshold * 100.0,
                window_secs
            )
        }
        RollbackCondition::NegativeFeedbackIncreases { count, window_secs } => {
            format!("if {} negative signals in {}s", count, window_secs)
        }
        RollbackCondition::TimeLimit { secs } => {
            format!("after {}s", secs)
        }
    }
}

fn format_duration(d: Duration) -> String {
    let secs = d.as_secs();
    if secs >= 3600 {
        format!("{}h", secs / 3600)
    } else if secs >= 60 {
        format!("{}m", secs / 60)
    } else {
        format!("{}s", secs)
    }
}

// ─── Feedback ───────────────────────────────────────────────────────────────

fn cmd_feedback<W: Write>(
    limit: Option<&str>,
    ctx: TuningCommandContext<'_, W>,
) -> std::io::Result<()> {
    let w = ctx.writer;
    let limit: usize = limit.and_then(|s| s.parse().ok()).unwrap_or(10);

    writeln!(w, "\n{}", "Recent Feedback Signals".cyan().bold())?;
    writeln!(w, "{}", "─".repeat(60).dim())?;

    // Note: AutoTuningEngine doesn't expose raw signals directly
    // We would need to add a method or access the aggregator
    writeln!(
        w,
        "  {} Feedback signals are aggregated internally.",
        "ℹ".blue()
    )?;
    writeln!(w, "  Use `/tuning record <signal>` to add manual signals.")?;
    writeln!(w)?;
    writeln!(w, "  {}", "Available signal types:".dim())?;
    writeln!(w, "    - success      Task completed successfully")?;
    writeln!(w, "    - failure      Task failed")?;
    writeln!(w, "    - retry        User retried")?;
    writeln!(w, "    - correction   User corrected output")?;
    writeln!(w, "    - thumbs_up    Positive feedback")?;
    writeln!(w, "    - thumbs_down  Negative feedback")?;
    writeln!(w, "    - star_1..5    Star rating (1-5)")?;
    writeln!(w, "    - positive     Positive text feedback")?;
    writeln!(w, "    - negative     Negative text feedback")?;
    writeln!(w, "    - accept       User accepted output")?;
    writeln!(w, "    - interrupt    User interrupted agent")?;
    writeln!(w, "    - drift        Agent lost focus")?;
    writeln!(w)?;
    writeln!(w, "  Limit: {} (specify as `/tuning feedback <n>`)", limit)?;
    writeln!(w)?;

    Ok(())
}

// ─── History ────────────────────────────────────────────────────────────────

fn cmd_history<W: Write>(
    limit: Option<&str>,
    ctx: TuningCommandContext<'_, W>,
) -> std::io::Result<()> {
    let w = ctx.writer;
    let limit: usize = limit.and_then(|s| s.parse().ok()).unwrap_or(20);

    let executions = ctx.engine.get_executions();

    writeln!(w, "\n{}", "Rule Execution History".cyan().bold())?;
    writeln!(w, "{}", "─".repeat(70).dim())?;

    if executions.is_empty() {
        writeln!(w, "  No rule executions yet.")?;
        writeln!(
            w,
            "  Rules execute when their triggers are met and cooldown has elapsed."
        )?;
        writeln!(w)?;
        return Ok(());
    }

    // Show most recent first
    let recent: Vec<_> = executions.iter().rev().take(limit).collect();

    for exec in recent {
        print_execution(w, exec)?;
    }

    if executions.len() > limit {
        writeln!(
            w,
            "  {} more executions (use `/tuning history <n>` to see more)",
            (executions.len() - limit).to_string().dim()
        )?;
    }

    writeln!(w)?;
    Ok(())
}

fn print_execution<W: Write>(w: &mut W, exec: &RuleExecution) -> std::io::Result<()> {
    // Timestamp
    let age = exec
        .timestamp
        .elapsed()
        .map(|d| format_duration(d))
        .unwrap_or_else(|_| "?".to_string());

    // Rollback indicator
    let status = if exec.rolled_back {
        "↩ ROLLED BACK".red().to_string()
    } else {
        "✓".green().to_string()
    };

    writeln!(
        w,
        "  {} {} ago - {} {}",
        status,
        age.dim(),
        exec.rule_id.clone().cyan(),
        format_action(&exec.action).dim()
    )?;

    // Value change
    if let (Some(prev), Some(new)) = (&exec.previous_value, &exec.new_value) {
        let prev_str: String = prev.to_string();
        let new_str: String = new.to_string();
        writeln!(w, "       {} → {}", prev_str.yellow(), new_str.green())?;
    }

    Ok(())
}

// ─── Enable/Disable ─────────────────────────────────────────────────────────

fn cmd_enable<W: Write>(ctx: TuningCommandContext<'_, W>) -> std::io::Result<()> {
    ctx.engine.set_enabled(true);
    writeln!(
        ctx.writer,
        "{} Auto-tuning {}",
        "✓".green(),
        "enabled".green().bold()
    )?;
    Ok(())
}

fn cmd_disable<W: Write>(ctx: TuningCommandContext<'_, W>) -> std::io::Result<()> {
    ctx.engine.set_enabled(false);
    writeln!(
        ctx.writer,
        "{} Auto-tuning {}",
        "✓".yellow(),
        "disabled".yellow().bold()
    )?;
    Ok(())
}

// ─── Cycle ──────────────────────────────────────────────────────────────────

fn cmd_cycle<W: Write>(ctx: TuningCommandContext<'_, W>) -> std::io::Result<()> {
    let w = ctx.writer;

    if !ctx.engine.is_enabled() {
        writeln!(
            w,
            "{} Auto-tuning is disabled. Enable with `/tuning enable`.",
            "⚠".yellow()
        )?;
        return Ok(());
    }

    writeln!(w, "Running evaluation cycle...")?;

    let executions = ctx.engine.run_cycle(ctx.runtime_config);

    if executions.is_empty() {
        writeln!(w, "  No rules triggered.")?;
    } else {
        writeln!(
            w,
            "  {} rule(s) executed:",
            executions.len().to_string().green()
        )?;
        for exec in &executions {
            writeln!(
                w,
                "    - {}: {}",
                exec.rule_id.clone().cyan(),
                format_action(&exec.action)
            )?;
        }
    }

    // Check rollbacks
    let rollbacks = ctx.engine.check_rollbacks(ctx.runtime_config);
    if !rollbacks.is_empty() {
        writeln!(
            w,
            "  {} rule(s) rolled back:",
            rollbacks.len().to_string().red()
        )?;
        for rule_id in &rollbacks {
            let id: String = rule_id.clone();
            writeln!(w, "    - {}", id.red())?;
        }
    }

    writeln!(w)?;
    Ok(())
}

// ─── Record ─────────────────────────────────────────────────────────────────

fn cmd_record<W: Write>(args: &[&str], ctx: TuningCommandContext<'_, W>) -> std::io::Result<()> {
    let w = ctx.writer;

    if args.is_empty() {
        writeln!(w, "{} Usage: /tuning record <signal_type>", "⚠".yellow())?;
        writeln!(w)?;
        writeln!(w, "Signal types:")?;
        writeln!(w, "  success, failure, retry, correction")?;
        writeln!(w, "  thumbs_up, thumbs_down, accept")?;
        writeln!(w, "  star_1..star_5, feedback_positive, feedback_negative")?;
        return Ok(());
    }

    let signal_type = match args[0].to_lowercase().as_str() {
        "success" => SignalType::TaskSuccess,
        "failure" => SignalType::TaskFailure {
            reason: args.get(1).unwrap_or(&"manual").to_string(),
        },
        "retry" => SignalType::Retry {
            count: args.get(1).and_then(|s| s.parse().ok()).unwrap_or(1),
        },
        "correction" => SignalType::Correction,
        "thumbs_up" | "up" | "+1" => SignalType::ThumbsRating { positive: true },
        "thumbs_down" | "down" | "-1" => SignalType::ThumbsRating { positive: false },
        "accept" | "acceptance" => SignalType::Acceptance,
        "interrupt" | "interruption" => SignalType::Interruption,
        "drift" => SignalType::FocusDrift,
        s if s.starts_with("star_") => {
            let stars: u8 = s
                .strip_prefix("star_")
                .and_then(|n| n.parse().ok())
                .unwrap_or(0);
            if !(1..=5).contains(&stars) {
                writeln!(w, "{} Star rating must be 1-5", "✗".red())?;
                return Ok(());
            }
            SignalType::StarRating { stars }
        }
        "feedback_positive" | "positive" => SignalType::TextFeedback {
            sentiment: Sentiment::Positive,
        },
        "feedback_negative" | "negative" => SignalType::TextFeedback {
            sentiment: Sentiment::Negative,
        },
        "feedback_neutral" | "neutral" => SignalType::TextFeedback {
            sentiment: Sentiment::Neutral,
        },
        other => {
            writeln!(w, "{} Unknown signal type: {}", "✗".red(), other)?;
            return Ok(());
        }
    };

    let signal = FeedbackSignal::new(signal_type.clone());
    ctx.engine.record_feedback(signal);

    writeln!(
        w,
        "{} Recorded {} signal",
        "✓".green(),
        format!("{:?}", signal_type).cyan()
    )?;

    Ok(())
}

// ─── Help ───────────────────────────────────────────────────────────────────

fn cmd_help<W: Write>(ctx: TuningCommandContext<'_, W>) -> std::io::Result<()> {
    let w = ctx.writer;

    writeln!(w, "\n{}", "Auto-Tuning Commands".cyan().bold())?;
    writeln!(w, "{}", "─".repeat(50).dim())?;
    writeln!(w)?;
    writeln!(
        w,
        "  {}   Show system status and current config",
        "/tuning status".green()
    )?;
    writeln!(
        w,
        "  {}    List all evolution rules",
        "/tuning rules".green()
    )?;
    writeln!(
        w,
        "  {} Show feedback signal info",
        "/tuning feedback".green()
    )?;
    writeln!(
        w,
        "  {}  Show rule execution history",
        "/tuning history".green()
    )?;
    writeln!(w, "  {}   Enable auto-tuning", "/tuning enable".green())?;
    writeln!(w, "  {}  Disable auto-tuning", "/tuning disable".green())?;
    writeln!(
        w,
        "  {}    Run one evaluation cycle",
        "/tuning cycle".green()
    )?;
    writeln!(
        w,
        "  {} Record a feedback signal",
        "/tuning record <signal>".green()
    )?;
    writeln!(w)?;
    writeln!(w, "{}", "About Auto-Tuning".cyan())?;
    writeln!(w, "{}", "─".repeat(50).dim())?;
    writeln!(w)?;
    writeln!(w, "  The auto-tuning system monitors feedback signals and")?;
    writeln!(
        w,
        "  automatically adjusts runtime configuration to improve"
    )?;
    writeln!(w, "  performance. Evolution rules define:")?;
    writeln!(w)?;
    writeln!(
        w,
        "  • {} - When to act (e.g., low success rate)",
        "Triggers".yellow()
    )?;
    writeln!(
        w,
        "  • {} - What to change (e.g., adjust threshold)",
        "Actions".yellow()
    )?;
    writeln!(
        w,
        "  • {} - When to undo (e.g., if things get worse)",
        "Rollbacks".yellow()
    )?;
    writeln!(w)?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tuning_record_help_lists_only_supported_signals() {
        let engine = Arc::new(AutoTuningEngine::new());
        let mut runtime_config = RuntimeConfig::default();
        let mut output = Vec::new();

        handle_tuning_command(
            "record",
            TuningCommandContext {
                engine: &engine,
                runtime_config: &mut runtime_config,
                writer: &mut output,
            },
        )
        .unwrap();

        let rendered = String::from_utf8(output).unwrap();
        assert!(rendered.contains("accept"));
        assert!(!rendered.contains("reject"));
    }
}
