//! `/profile` slash command — User profile management.
//!
//! Subcommands:
//! - `/profile` or `/profile show`: Show current profile
//! - `/profile edit <key> <value>`: Edit a preference
//! - `/profile scenario`: Show detected scenario
//! - `/profile stats`: Show usage statistics
//! - `/profile tools`: Show tool preferences
//! - `/profile experiments`: Show enrolled experiments
//! - `/profile reset`: Reset to defaults
//! - `/profile help`: Show help

use super::*;
use astra_runtime::user_profile::{
    CodeCommentStyle, EmojiUsage, Formality, ResponseLength, Scenario, UserProfile,
    UserProfileManager, Verbosity,
};
use std::sync::Arc;

/// Profile command context — passed from main.
pub struct ProfileCommandContext<'a> {
    pub profile_manager: &'a Arc<UserProfileManager>,
    pub user_id: &'a str,
}

/// Handle `/profile [subcommand]` command.
pub fn handle_profile_command(arg: &str, ctx: &ProfileCommandContext<'_>) {
    let parts: Vec<&str> = arg.split_whitespace().collect();
    let subcmd = parts.first().copied().unwrap_or("show");

    match subcmd {
        "" | "show" => show_profile(ctx),
        "edit" | "set" => {
            if parts.len() < 3 {
                eprintln!("  {}", "Usage: /profile edit <key> <value>".yellow());
                eprintln!(
                    "  {}",
                    "Keys: verbosity, language, formality, response_length, comments, emoji".dim()
                );
            } else {
                edit_preference(ctx, parts[1], &parts[2..].join(" "));
            }
        }
        "scenario" => show_scenario(ctx),
        "stats" => show_stats(ctx),
        "tools" => show_tools(ctx),
        "experiments" | "exp" => show_experiments(ctx),
        "reset" => reset_profile(ctx),
        "help" | "-h" | "--help" => show_help(),
        _ => {
            eprintln!(
                "  {}",
                format!("Unknown subcommand: {subcmd}. Try /profile help").yellow()
            );
        }
    }
}

fn show_profile(ctx: &ProfileCommandContext<'_>) {
    let profile = ctx.profile_manager.get_profile(ctx.user_id);
    let prefs = &profile.preferences;

    eprintln!("\n  {}", "👤 User Profile".cyan().bold());
    eprintln!("  {}", "─".repeat(50).dim());

    eprintln!("  User ID: {}", ctx.user_id.dim());
    eprintln!("  Created: {}", format_time(profile.created_at).dim());
    eprintln!("  Updated: {}", format_time(profile.updated_at).dim());

    eprintln!("\n  {}", "Preferences:".bold());
    eprintln!(
        "    Verbosity: {}",
        format_verbosity(prefs.verbosity).cyan()
    );
    eprintln!(
        "    Language: {}",
        prefs.language_style.language.clone().cyan()
    );
    eprintln!(
        "    Formality: {}",
        format_formality(prefs.language_style.formality).cyan()
    );
    eprintln!(
        "    Response Length: {}",
        format_response_length(prefs.response_length).cyan()
    );
    eprintln!(
        "    Code Comments: {}",
        format_code_comments(prefs.language_style.code_comments).cyan()
    );
    eprintln!(
        "    Emoji Usage: {}",
        format_emoji_usage(prefs.language_style.emoji_usage).cyan()
    );
    eprintln!(
        "    Technical Jargon: {}",
        if prefs.language_style.technical_jargon {
            "yes".green()
        } else {
            "no".yellow()
        }
    );

    if !prefs.config_overrides.is_empty() {
        eprintln!("\n  {}", "Config Overrides:".bold());
        for (key, value) in &prefs.config_overrides {
            eprintln!("    {} = {}", key.clone().dim(), value);
        }
    }

    if let Some(ref suffix) = prefs.custom_prompt_suffix {
        eprintln!("\n  {}", "Custom Prompt Suffix:".bold());
        eprintln!("    {}", suffix.clone().dim());
    }

    if let Some(scenario) = profile.current_scenario {
        eprintln!(
            "\n  Current Scenario: {}",
            format_scenario(scenario).green()
        );
    }

    if !profile.active_experiments.is_empty() {
        eprintln!(
            "\n  {} ({})",
            "Active Experiments:".bold(),
            profile.active_experiments.len()
        );
        for exp_id in &profile.active_experiments {
            eprintln!("    • {}", exp_id.clone().cyan());
        }
    }

    eprintln!();
}

fn edit_preference(ctx: &ProfileCommandContext<'_>, key: &str, value: &str) {
    let mut profile = ctx.profile_manager.get_profile(ctx.user_id);
    let prefs = &mut profile.preferences;
    let value_lower = value.to_lowercase();

    match key {
        "verbosity" | "v" => {
            match value_lower.as_str() {
                "quiet" | "q" => prefs.verbosity = Verbosity::Quiet,
                "normal" | "n" => prefs.verbosity = Verbosity::Normal,
                "verbose" | "v" => prefs.verbosity = Verbosity::Verbose,
                "debug" | "d" => prefs.verbosity = Verbosity::Debug,
                _ => {
                    eprintln!(
                        "  {}",
                        "Invalid verbosity. Options: quiet, normal, verbose, debug".yellow()
                    );
                    return;
                }
            }
            eprintln!(
                "  {} Verbosity set to: {}",
                "✓".green(),
                format_verbosity(prefs.verbosity).cyan()
            );
        }
        "language" | "lang" => {
            prefs.language_style.language = value.to_string();
            eprintln!("  {} Language set to: {}", "✓".green(), value.cyan());
        }
        "formality" | "f" => {
            match value_lower.as_str() {
                "casual" | "c" => prefs.language_style.formality = Formality::Casual,
                "neutral" | "n" => prefs.language_style.formality = Formality::Neutral,
                "formal" | "f" => prefs.language_style.formality = Formality::Formal,
                _ => {
                    eprintln!(
                        "  {}",
                        "Invalid formality. Options: casual, neutral, formal".yellow()
                    );
                    return;
                }
            }
            eprintln!(
                "  {} Formality set to: {}",
                "✓".green(),
                format_formality(prefs.language_style.formality).cyan()
            );
        }
        "response_length" | "length" | "len" => {
            match value_lower.as_str() {
                "short" | "s" => prefs.response_length = ResponseLength::Short,
                "medium" | "m" => prefs.response_length = ResponseLength::Medium,
                "long" | "l" => prefs.response_length = ResponseLength::Long,
                _ => {
                    eprintln!(
                        "  {}",
                        "Invalid response length. Options: short, medium, long".yellow()
                    );
                    return;
                }
            }
            eprintln!(
                "  {} Response length set to: {}",
                "✓".green(),
                format_response_length(prefs.response_length).cyan()
            );
        }
        "comments" | "code_comments" => {
            match value_lower.as_str() {
                "none" | "n" => prefs.language_style.code_comments = CodeCommentStyle::None,
                "minimal" | "min" => prefs.language_style.code_comments = CodeCommentStyle::Minimal,
                "moderate" | "mod" => {
                    prefs.language_style.code_comments = CodeCommentStyle::Moderate
                }
                "extensive" | "ext" => {
                    prefs.language_style.code_comments = CodeCommentStyle::Extensive
                }
                _ => {
                    eprintln!(
                        "  {}",
                        "Invalid comment style. Options: none, minimal, moderate, extensive"
                            .yellow()
                    );
                    return;
                }
            }
            eprintln!(
                "  {} Code comments set to: {}",
                "✓".green(),
                format_code_comments(prefs.language_style.code_comments).cyan()
            );
        }
        "emoji" => {
            match value_lower.as_str() {
                "none" | "n" => prefs.language_style.emoji_usage = EmojiUsage::None,
                "minimal" | "min" => prefs.language_style.emoji_usage = EmojiUsage::Minimal,
                "moderate" | "mod" => prefs.language_style.emoji_usage = EmojiUsage::Moderate,
                "frequent" | "freq" => prefs.language_style.emoji_usage = EmojiUsage::Frequent,
                _ => {
                    eprintln!(
                        "  {}",
                        "Invalid emoji usage. Options: none, minimal, moderate, frequent".yellow()
                    );
                    return;
                }
            }
            eprintln!(
                "  {} Emoji usage set to: {}",
                "✓".green(),
                format_emoji_usage(prefs.language_style.emoji_usage).cyan()
            );
        }
        "jargon" | "technical" => {
            match value_lower.as_str() {
                "yes" | "y" | "true" | "on" => prefs.language_style.technical_jargon = true,
                "no" | "n" | "false" | "off" => prefs.language_style.technical_jargon = false,
                _ => {
                    eprintln!("  {}", "Invalid value. Options: yes, no".yellow());
                    return;
                }
            }
            eprintln!(
                "  {} Technical jargon set to: {}",
                "✓".green(),
                if prefs.language_style.technical_jargon {
                    "yes".cyan()
                } else {
                    "no".cyan()
                }
            );
        }
        "prompt" | "suffix" => {
            if value.is_empty() || value_lower == "none" || value_lower == "clear" {
                prefs.custom_prompt_suffix = None;
                eprintln!("  {} Custom prompt suffix cleared", "✓".green());
            } else {
                prefs.custom_prompt_suffix = Some(value.to_string());
                eprintln!(
                    "  {} Custom prompt suffix set to: {}",
                    "✓".green(),
                    value.dim()
                );
            }
        }
        _ => {
            eprintln!(
                "  {}",
                format!(
                    "Unknown preference: {key}. Available: verbosity, language, formality, response_length, comments, emoji, jargon, prompt"
                )
                .yellow()
            );
            return;
        }
    }

    profile.touch();
    ctx.profile_manager.update_profile(profile);
    eprintln!("  {}", "✓ Profile updated.".green());
}

fn show_scenario(ctx: &ProfileCommandContext<'_>) {
    let profile = ctx.profile_manager.get_profile(ctx.user_id);

    eprintln!("\n  {}", "🎯 Scenario Detection".cyan().bold());
    eprintln!("  {}", "─".repeat(50).dim());

    match profile.current_scenario {
        Some(scenario) => {
            eprintln!(
                "  Current Scenario: {}",
                format_scenario(scenario).green().bold()
            );
            eprintln!();

            let strategy = scenario.strategy_hints();
            eprintln!("  {}", "Strategy Hints:".bold());
            eprintln!("    Max tools/turn: {}", strategy.max_tools_per_turn);
            eprintln!(
                "    Prefer read-only: {}",
                if strategy.prefer_read_only {
                    "yes"
                } else {
                    "no"
                }
            );
            eprintln!(
                "    Detail level: {}",
                format_verbosity(strategy.detail_level)
            );

            eprintln!("\n  {}", "Recommended Tools:".bold());
            for tool in scenario.recommended_tools() {
                eprintln!("    • {}", tool.cyan());
            }
        }
        None => {
            eprintln!("  {}", "No scenario detected yet.".dim());
            eprintln!(
                "  {}",
                "Scenarios are detected based on your queries and tool usage.".dim()
            );
        }
    }

    eprintln!();
}

fn show_stats(ctx: &ProfileCommandContext<'_>) {
    let profile = ctx.profile_manager.get_profile(ctx.user_id);
    let stats = &profile.stats;

    eprintln!("\n  {}", "📊 Usage Statistics".cyan().bold());
    eprintln!("  {}", "─".repeat(50).dim());

    eprintln!("  Total Sessions: {}", stats.total_sessions);
    eprintln!("  Total Queries: {}", stats.total_queries);
    eprintln!("  Total Tool Calls: {}", stats.total_tool_calls);

    if stats.avg_session_duration_secs > 0.0 {
        eprintln!(
            "  Avg Session Duration: {}",
            format_duration_secs(stats.avg_session_duration_secs)
        );
    }

    if !stats.tool_usage.is_empty() {
        eprintln!("\n  {}", "Top Tools:".bold());
        for (tool, count) in stats.top_tools(10) {
            let bar_len = (count as f64 / stats.total_tool_calls.max(1) as f64 * 20.0) as usize;
            let bar = "█".repeat(bar_len);
            eprintln!("    {:20} {:4} {}", tool, count, bar.cyan());
        }
    }

    if !stats.scenario_frequency.is_empty() {
        eprintln!("\n  {}", "Scenario Frequency:".bold());
        let mut scenarios: Vec<_> = stats.scenario_frequency.iter().collect();
        scenarios.sort_by(|a, b| b.1.cmp(a.1));
        for (scenario, count) in scenarios.iter().take(5) {
            eprintln!("    {} ({}x)", (*scenario).clone().cyan(), count);
        }
    }

    eprintln!();
}

fn show_tools(ctx: &ProfileCommandContext<'_>) {
    let profile = ctx.profile_manager.get_profile(ctx.user_id);
    let prefs = &profile.preferences;

    eprintln!("\n  {}", "🔧 Tool Preferences".cyan().bold());
    eprintln!("  {}", "─".repeat(50).dim());

    if prefs.preferred_tools.is_empty() && prefs.blocked_tools.is_empty() {
        eprintln!("  {}", "No tool preferences configured.".dim());
        eprintln!(
            "  {}",
            "Preferred tools get boosted in selection, blocked tools are never used.".dim()
        );
    } else {
        if !prefs.preferred_tools.is_empty() {
            eprintln!("  {}", "Preferred (boosted):".bold());
            for tool in &prefs.preferred_tools {
                eprintln!("    {} {}", "▲".green(), tool.clone().cyan());
            }
        }

        if !prefs.blocked_tools.is_empty() {
            eprintln!("\n  {}", "Blocked (never used):".bold());
            for tool in &prefs.blocked_tools {
                eprintln!("    {} {}", "✗".red(), tool.clone().dim());
            }
        }
    }

    // Show top used tools
    let stats = &profile.stats;
    if !stats.tool_usage.is_empty() {
        eprintln!("\n  {}", "Most Used:".bold());
        for (tool, count) in stats.top_tools(5) {
            eprintln!("    {} ({}x)", tool.cyan(), count);
        }
    }

    eprintln!();
}

fn show_experiments(ctx: &ProfileCommandContext<'_>) {
    let profile = ctx.profile_manager.get_profile(ctx.user_id);

    eprintln!("\n  {}", "🧪 Experiment Enrollment".cyan().bold());
    eprintln!("  {}", "─".repeat(50).dim());

    if profile.active_experiments.is_empty() {
        eprintln!("  {}", "Not enrolled in any experiments.".dim());
        eprintln!(
            "  {}",
            "Use /experiment list to see available experiments.".dim()
        );
    } else {
        eprintln!(
            "  Enrolled in {} experiment(s):",
            profile.active_experiments.len()
        );
        for exp_id in &profile.active_experiments {
            eprintln!("    • {}", exp_id.clone().cyan());
        }
    }

    eprintln!();
}

fn reset_profile(ctx: &ProfileCommandContext<'_>) {
    let new_profile = UserProfile::new(ctx.user_id);
    ctx.profile_manager.update_profile(new_profile);

    eprintln!("  {} Profile reset to defaults.", "✓".green());
}

fn show_help() {
    eprintln!(
        "\n  {}",
        "👤 /profile - User Profile Management".cyan().bold()
    );
    eprintln!("  {}", "─".repeat(55).dim());

    eprintln!("\n  {}", "Subcommands:".bold());
    eprintln!(
        "    {}",
        "/profile               Show current profile".cyan()
    );
    eprintln!("    {}", "/profile edit <k> <v>  Edit a preference".cyan());
    eprintln!(
        "    {}",
        "/profile scenario      Show detected scenario".cyan()
    );
    eprintln!(
        "    {}",
        "/profile stats         Show usage statistics".cyan()
    );
    eprintln!(
        "    {}",
        "/profile tools         Show tool preferences".cyan()
    );
    eprintln!(
        "    {}",
        "/profile experiments   Show experiment enrollment".cyan()
    );
    eprintln!("    {}", "/profile reset         Reset to defaults".cyan());

    eprintln!("\n  {}", "Editable Preferences:".bold());
    eprintln!("    {} quiet|normal|verbose|debug", "verbosity".green());
    eprintln!("    {}     en|zh|ja|ko|es|fr|de|...", "language".green());
    eprintln!("    {}    casual|neutral|formal", "formality".green());
    eprintln!("    {}       short|medium|long", "length".green());
    eprintln!(
        "    {}     none|minimal|moderate|extensive",
        "comments".green()
    );
    eprintln!(
        "    {}        none|minimal|moderate|frequent",
        "emoji".green()
    );
    eprintln!(
        "    {}       yes|no (use technical jargon)",
        "jargon".green()
    );
    eprintln!(
        "    {}       <text> (custom prompt addition)",
        "prompt".green()
    );

    eprintln!("\n  {}", "Examples:".bold());
    eprintln!("    /profile edit verbosity verbose");
    eprintln!("    /profile edit language zh");
    eprintln!("    /profile edit comments minimal");
    eprintln!("    /profile edit prompt Always explain your reasoning");

    eprintln!();
}

// ─── Formatting Helpers ─────────────────────────────────────────────────────

fn format_verbosity(v: Verbosity) -> &'static str {
    match v {
        Verbosity::Quiet => "quiet",
        Verbosity::Normal => "normal",
        Verbosity::Verbose => "verbose",
        Verbosity::Debug => "debug",
    }
}

fn format_formality(f: Formality) -> &'static str {
    match f {
        Formality::Casual => "casual",
        Formality::Neutral => "neutral",
        Formality::Formal => "formal",
    }
}

fn format_response_length(l: ResponseLength) -> &'static str {
    match l {
        ResponseLength::Short => "short",
        ResponseLength::Medium => "medium",
        ResponseLength::Long => "long",
    }
}

fn format_code_comments(c: CodeCommentStyle) -> &'static str {
    match c {
        CodeCommentStyle::None => "none",
        CodeCommentStyle::Minimal => "minimal",
        CodeCommentStyle::Moderate => "moderate",
        CodeCommentStyle::Extensive => "extensive",
    }
}

fn format_emoji_usage(e: EmojiUsage) -> &'static str {
    match e {
        EmojiUsage::None => "none",
        EmojiUsage::Minimal => "minimal",
        EmojiUsage::Moderate => "moderate",
        EmojiUsage::Frequent => "frequent",
    }
}

fn format_scenario(s: Scenario) -> &'static str {
    match s {
        Scenario::CodeReview => "Code Review",
        Scenario::Debugging => "Debugging",
        Scenario::Exploration => "Exploration",
        Scenario::Planning => "Planning",
        Scenario::Implementation => "Implementation",
        Scenario::Refactoring => "Refactoring",
        Scenario::Testing => "Testing",
        Scenario::Documentation => "Documentation",
        Scenario::DevOps => "DevOps",
        Scenario::Learning => "Learning",
    }
}

fn format_time(t: std::time::SystemTime) -> String {
    let elapsed = t.elapsed().unwrap_or_default();
    if elapsed.as_secs() < 60 {
        "just now".to_string()
    } else if elapsed.as_secs() < 3600 {
        format!("{} min ago", elapsed.as_secs() / 60)
    } else if elapsed.as_secs() < 86400 {
        format!("{} hours ago", elapsed.as_secs() / 3600)
    } else {
        format!("{} days ago", elapsed.as_secs() / 86400)
    }
}

fn format_duration_secs(secs: f64) -> String {
    if secs < 60.0 {
        format!("{:.0}s", secs)
    } else if secs < 3600.0 {
        format!("{:.1}m", secs / 60.0)
    } else {
        format!("{:.1}h", secs / 3600.0)
    }
}
