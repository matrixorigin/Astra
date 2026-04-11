//! `/experiment` slash command — A/B testing experiment management.
//!
//! Subcommands:
//! - `/experiment` or `/experiment list`: List all experiments
//! - `/experiment status`: Show current session's active experiment
//! - `/experiment show <id>`: Show experiment details
//! - `/experiment create <id>`: Create a new experiment (interactive)
//! - `/experiment start <id>`: Start an experiment
//! - `/experiment stop <id>`: Stop an experiment
//! - `/experiment analyze <id>`: Analyze experiment results
//! - `/experiment help`: Show help
#![allow(deprecated)]

use super::*;
use astra_runtime::ab_testing::{
    Experiment, ExperimentAnalyzer, ExperimentStatus, ExperimentStore, MetricDefinition,
    MetricType, Recommendation, Variant, VariantComparison,
};
use std::sync::{Arc, RwLock};

/// Experiment command context — passed from main.
pub struct ExperimentCommandContext<'a> {
    pub experiment_store: &'a Arc<RwLock<ExperimentStore>>,
    pub active_experiment_id: Option<&'a str>,
    pub active_variant_id: Option<&'a str>,
    pub session_id: Option<&'a str>,
}

/// Handle `/experiment [subcommand]` command.
pub fn handle_experiment_command(arg: &str, ctx: &ExperimentCommandContext<'_>) {
    let parts: Vec<&str> = arg.split_whitespace().collect();
    let subcmd = parts.first().copied().unwrap_or("list");

    match subcmd {
        "" | "list" => show_list(ctx),
        "status" => show_status(ctx),
        "show" => {
            if let Some(id) = parts.get(1) {
                show_experiment(ctx, id);
            } else {
                eprintln!("  {}", "Usage: /experiment show <experiment_id>".yellow());
            }
        }
        "create" => {
            if let Some(id) = parts.get(1) {
                create_experiment(ctx, id);
            } else {
                eprintln!("  {}", "Usage: /experiment create <experiment_id>".yellow());
            }
        }
        "start" => {
            if let Some(id) = parts.get(1) {
                start_experiment(ctx, id);
            } else {
                eprintln!("  {}", "Usage: /experiment start <experiment_id>".yellow());
            }
        }
        "stop" => {
            if let Some(id) = parts.get(1) {
                stop_experiment(ctx, id);
            } else {
                eprintln!("  {}", "Usage: /experiment stop <experiment_id>".yellow());
            }
        }
        "analyze" => {
            if let Some(id) = parts.get(1) {
                analyze_experiment(ctx, id);
            } else {
                eprintln!(
                    "  {}",
                    "Usage: /experiment analyze <experiment_id>".yellow()
                );
            }
        }
        "help" | "?" => show_help(),
        _ => {
            eprintln!(
                "  {}",
                format!("Unknown subcommand: {subcmd}. Try /experiment help").yellow()
            );
        }
    }
}

fn show_list(ctx: &ExperimentCommandContext<'_>) {
    let store = ctx
        .experiment_store
        .read()
        .unwrap_or_else(|e| e.into_inner());
    let experiments = store.list();

    if experiments.is_empty() {
        eprintln!("\n  {}", "🧪 No experiments defined".cyan().bold());
        eprintln!(
            "  {}",
            "Use /experiment create <id> to create a new experiment.".dim()
        );
        eprintln!();
        return;
    }

    eprintln!("\n  {}", "🧪 Experiments".cyan().bold());
    eprintln!("  {}", "─".repeat(70).dim());

    // Group by status
    let mut running: Vec<&Experiment> = Vec::new();
    let mut paused: Vec<&Experiment> = Vec::new();
    let mut draft: Vec<&Experiment> = Vec::new();
    let mut completed: Vec<&Experiment> = Vec::new();

    for exp in &experiments {
        match exp.status {
            ExperimentStatus::Running => running.push(exp),
            ExperimentStatus::Paused => paused.push(exp),
            ExperimentStatus::Draft => draft.push(exp),
            ExperimentStatus::Completed | ExperimentStatus::Cancelled => completed.push(exp),
        }
    }

    // Show running first
    for exp in &running {
        let samples = store.sample_counts(&exp.id);
        let total: usize = samples.values().sum();
        eprintln!(
            "  {} {} {} {} ({} samples)",
            "▶".green(),
            exp.id.clone().green().bold(),
            format_variants_inline(&exp.variants),
            exp.name.clone().dim(),
            total
        );
    }

    for exp in &paused {
        let samples = store.sample_counts(&exp.id);
        let total: usize = samples.values().sum();
        eprintln!(
            "  {} {} {} {} ({} samples)",
            "⏸".yellow(),
            exp.id.clone().yellow().bold(),
            format_variants_inline(&exp.variants),
            exp.name.clone().dim(),
            total
        );
    }

    for exp in &draft {
        eprintln!(
            "  {} {} {} {}",
            "○".dim(),
            exp.id.clone().dim(),
            format_variants_inline(&exp.variants),
            exp.name.clone().dim(),
        );
    }

    for exp in completed.iter().take(3) {
        eprintln!(
            "  {} {} {}",
            "✓".dim(),
            exp.id.clone().dim(),
            exp.name.clone().dim(),
        );
    }

    if completed.len() > 3 {
        eprintln!("  {} (+{} more completed)", "…".dim(), completed.len() - 3);
    }

    eprintln!();
}

fn show_status(ctx: &ExperimentCommandContext<'_>) {
    eprintln!(
        "\n  {}",
        "🧪 Current Session Experiment Status".cyan().bold()
    );
    eprintln!("  {}", "─".repeat(50).dim());

    if let Some(session_id) = ctx.session_id {
        eprintln!("  Session: {}", session_id.dim());
    }

    match (ctx.active_experiment_id, ctx.active_variant_id) {
        (Some(exp_id), Some(variant_id)) => {
            eprintln!("  Experiment: {}", exp_id.green().bold());
            eprintln!("  Variant: {}", variant_id.yellow());

            // Show variant config diff
            let store = ctx
                .experiment_store
                .read()
                .unwrap_or_else(|e| e.into_inner());
            if let Some(exp) = store.get(exp_id) {
                if let Some(variant) = exp.variants.iter().find(|v| v.id == variant_id) {
                    if !variant.config_diff.is_empty() {
                        eprintln!("  Config overrides:");
                        for (key, value) in &variant.config_diff {
                            eprintln!("    {} = {}", key.clone().cyan(), value);
                        }
                    } else if variant.is_control {
                        eprintln!("  {} (no config changes)", "Control variant".dim());
                    }
                }
            }
        }
        _ => {
            eprintln!("  {}", "Not enrolled in any experiment.".dim());
            eprintln!(
                "  {}",
                "Add experiment IDs to your user profile to participate.".dim()
            );
        }
    }

    eprintln!();
}

fn show_experiment(ctx: &ExperimentCommandContext<'_>, id: &str) {
    let store = ctx
        .experiment_store
        .read()
        .unwrap_or_else(|e| e.into_inner());
    let Some(exp) = store.get(id) else {
        eprintln!("  {}", format!("Experiment not found: {id}").red());
        return;
    };

    eprintln!("\n  {}", format!("🧪 Experiment: {}", exp.id).cyan().bold());
    eprintln!("  {}", "─".repeat(60).dim());

    eprintln!("  Name: {}", exp.name);
    if !exp.description.is_empty() {
        eprintln!("  Description: {}", exp.description.dim());
    }
    eprintln!("  Status: {}", format_status(exp.status));

    if let Some(started) = exp.started_at {
        let elapsed = started.elapsed().unwrap_or_default();
        eprintln!("  Running for: {}", format_duration(elapsed));
    }

    eprintln!("\n  {}", "Variants:".bold());
    for v in &exp.variants {
        let control_tag = if v.is_control { " (control)" } else { "" };
        eprintln!(
            "    {} {}{} — {}% traffic",
            if v.is_control { "◉" } else { "○" },
            v.id.clone().cyan(),
            control_tag.dim(),
            (v.traffic_percentage * 100.0) as u32
        );
        if !v.config_diff.is_empty() {
            for (key, value) in &v.config_diff {
                eprintln!("      {} = {}", key.clone().dim(), value);
            }
        }
    }

    eprintln!("\n  {}", "Metrics:".bold());
    for m in &exp.metrics {
        eprintln!(
            "    {} ({}{})",
            m.name.clone().cyan(),
            format_metric_type(&m.metric_type),
            if m.lower_is_better {
                ", lower is better"
            } else {
                ""
            }
        );
    }

    // Show sample counts
    let samples = store.sample_counts(id);
    if !samples.is_empty() {
        eprintln!("\n  {}", "Sample Counts:".bold());
        for (variant_id, count) in &samples {
            eprintln!("    {}: {}", variant_id, count);
        }
    }

    eprintln!();
}

fn create_experiment(ctx: &ExperimentCommandContext<'_>, id: &str) {
    // Create a default compression threshold experiment as example
    let experiment = Experiment::new(id)
        .with_name(format!("Experiment: {}", id))
        .with_description("A/B test created via CLI")
        .with_variant(Variant::control().with_traffic(0.5))
        .with_variant(
            Variant::new("treatment")
                .with_name("Treatment")
                .with_traffic(0.5)
                .with_config_diff("compression.compression_threshold", serde_json::json!(0.6)),
        )
        .with_metric(MetricDefinition::token_usage())
        .with_metric(MetricDefinition::latency())
        .with_metric(MetricDefinition::success_rate())
        .with_min_samples(50)
        .build();

    ctx.experiment_store
        .write()
        .unwrap_or_else(|e| e.into_inner())
        .register(experiment);

    eprintln!("  {}", format!("✅ Created experiment: {id}").green());
    eprintln!(
        "  {}",
        "Default: control (50%) vs treatment (50%, threshold=0.6)".dim()
    );
    eprintln!(
        "  {}",
        format!("Use /experiment start {id} to begin.").dim()
    );
}

fn start_experiment(ctx: &ExperimentCommandContext<'_>, id: &str) {
    let store = ctx
        .experiment_store
        .write()
        .unwrap_or_else(|e| e.into_inner());
    let Some(mut exp) = store.get(id) else {
        eprintln!("  {}", format!("Experiment not found: {id}").red());
        return;
    };

    if exp.status == ExperimentStatus::Running {
        eprintln!(
            "  {}",
            format!("Experiment {id} is already running.").yellow()
        );
        return;
    }

    exp.start();
    store.register(exp);

    eprintln!("  {}", format!("▶ Started experiment: {id}").green());
    eprintln!(
        "  {}",
        "Users will be assigned to variants based on their ID hash.".dim()
    );
}

fn stop_experiment(ctx: &ExperimentCommandContext<'_>, id: &str) {
    let store = ctx
        .experiment_store
        .write()
        .unwrap_or_else(|e| e.into_inner());
    let Some(mut exp) = store.get(id) else {
        eprintln!("  {}", format!("Experiment not found: {id}").red());
        return;
    };

    if exp.status != ExperimentStatus::Running {
        eprintln!(
            "  {}",
            format!("Experiment {id} is not running (status: {:?}).", exp.status).yellow()
        );
        return;
    }

    exp.stop();
    store.register(exp);

    eprintln!("  {}", format!("⏹ Stopped experiment: {id}").green());
    eprintln!(
        "  {}",
        format!("Use /experiment analyze {id} to see results.").dim()
    );
}

fn analyze_experiment(ctx: &ExperimentCommandContext<'_>, id: &str) {
    let store = ctx
        .experiment_store
        .read()
        .unwrap_or_else(|e| e.into_inner());
    let Some(exp) = store.get(id) else {
        eprintln!("  {}", format!("Experiment not found: {id}").red());
        return;
    };

    let outcomes = store.get_outcomes(id);
    if outcomes.is_empty() {
        eprintln!(
            "  {}",
            format!("No outcomes recorded for experiment: {id}").yellow()
        );
        eprintln!(
            "  {}",
            "Run some sessions with this experiment active to collect data.".dim()
        );
        return;
    }

    let analysis = ExperimentAnalyzer::analyze(&exp, &outcomes);

    eprintln!("\n  {}", format!("📊 Analysis: {}", id).cyan().bold());
    eprintln!("  {}", "─".repeat(70).dim());

    // Variant stats
    eprintln!("  {}", "Variant Statistics:".bold());
    for (variant_id, stats) in &analysis.variant_stats {
        let is_control = exp
            .variants
            .iter()
            .any(|v| v.id == *variant_id && v.is_control);
        let label = if is_control {
            format!("{} (control)", variant_id)
        } else {
            variant_id.clone()
        };
        eprintln!("    {} (n={})", label.cyan(), stats.sample_count);

        for (metric_name, metric_stats) in &stats.metric_stats {
            eprintln!(
                "      {}: mean={:.2} ± {:.2} (p95={:.2})",
                metric_name, metric_stats.mean, metric_stats.std_dev, metric_stats.p95
            );
        }
    }

    // Comparisons
    if !analysis.comparisons.is_empty() {
        eprintln!("\n  {}", "Comparisons (vs Control):".bold());
        for comp in &analysis.comparisons {
            let change_str = format_change(comp);
            let significance = if comp.is_significant {
                if comp.is_improvement {
                    "✅ significant improvement".green().to_string()
                } else {
                    "⚠️ significant regression".red().to_string()
                }
            } else {
                "○ not significant".dim().to_string()
            };

            eprintln!(
                "    {} [{}]: {} {}",
                comp.treatment_id.clone().cyan(),
                comp.metric,
                change_str,
                significance
            );
            eprintln!(
                "      p={:.4}, 95% CI: [{:.2}, {:.2}]",
                comp.p_value, comp.confidence_interval.0, comp.confidence_interval.1
            );
        }
    }

    // Recommendation
    eprintln!("\n  {}", "Recommendation:".bold());
    match &analysis.recommendation {
        Recommendation::InsufficientData => {
            eprintln!(
                "    {}",
                "⏳ Insufficient data - need more samples.".yellow()
            );
            eprintln!(
                "    Min samples per variant: {}",
                exp.min_samples_per_variant
            );
        }
        Recommendation::KeepControl => {
            eprintln!("    {}", "📌 Keep control - treatment is worse.".cyan());
        }
        Recommendation::RolloutTreatment { variant_id } => {
            eprintln!(
                "    {}",
                format!("🚀 Roll out variant: {}", variant_id).green()
            );
        }
        Recommendation::NoSignificantDifference => {
            eprintln!(
                "    {}",
                "🤷 No significant difference between variants.".dim()
            );
        }
        Recommendation::NeedsManualReview => {
            eprintln!(
                "    {}",
                "👀 Multiple treatments show improvement - needs manual review.".yellow()
            );
        }
    }

    eprintln!();
}

fn show_help() {
    eprintln!(
        "\n  {}",
        "🧪 /experiment — A/B Testing Commands".cyan().bold()
    );
    eprintln!("  {}", "─".repeat(50).dim());
    eprintln!("  /experiment            List all experiments");
    eprintln!("  /experiment status     Show current session's experiment");
    eprintln!("  /experiment show <id>  Show experiment details");
    eprintln!("  /experiment create <id> Create a new experiment");
    eprintln!("  /experiment start <id> Start collecting data");
    eprintln!("  /experiment stop <id>  Stop an experiment");
    eprintln!("  /experiment analyze <id> Statistical analysis");
    eprintln!("  /experiment help       Show this help");
    eprintln!();
    eprintln!("  {}", "Experiment Flow:".bold());
    eprintln!("  1. /experiment create my-test");
    eprintln!("  2. /experiment start my-test");
    eprintln!("  3. Run sessions (outcomes are auto-recorded)");
    eprintln!("  4. /experiment analyze my-test");
    eprintln!("  5. /experiment stop my-test");
    eprintln!();
}

// ─── Helpers ────────────────────────────────────────────────────────────────

fn format_status(status: ExperimentStatus) -> String {
    match status {
        ExperimentStatus::Draft => "Draft".dim().to_string(),
        ExperimentStatus::Running => "Running".green().to_string(),
        ExperimentStatus::Paused => "Paused".yellow().to_string(),
        ExperimentStatus::Completed => "Completed".cyan().to_string(),
        ExperimentStatus::Cancelled => "Cancelled".dim().to_string(),
    }
}

fn format_variants_inline(variants: &[Variant]) -> String {
    let names: Vec<&str> = variants.iter().map(|v| v.id.as_str()).collect();
    format!("[{}]", names.join("/")).dim().to_string()
}

fn format_metric_type(mt: &MetricType) -> &'static str {
    match mt {
        MetricType::Counter => "counter",
        MetricType::Timer => "timer",
        MetricType::Rate => "rate",
        MetricType::Score => "score",
        MetricType::Histogram => "histogram",
    }
}

fn format_duration(d: std::time::Duration) -> String {
    let secs = d.as_secs();
    if secs < 60 {
        format!("{}s", secs)
    } else if secs < 3600 {
        format!("{}m", secs / 60)
    } else if secs < 86400 {
        format!("{}h", secs / 3600)
    } else {
        format!("{}d", secs / 86400)
    }
}

fn format_change(comp: &VariantComparison) -> String {
    let sign = if comp.relative_change >= 0.0 { "+" } else { "" };
    format!(
        "{}{}% ({}{:.2})",
        sign,
        (comp.relative_change * 100.0) as i32,
        sign,
        comp.absolute_change
    )
}
