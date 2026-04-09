//! `/config` slash command — view and manage runtime configuration.
//!
//! Commands:
//! - `/config` — Show current configuration
//! - `/config show` — Show current configuration (same as `/config`)
//! - `/config paths` — Show configuration file paths
//! - `/config export [path]` — Export configuration to file
//! - `/config diff` — Show difference from defaults

use astra_runtime::runtime_config::RuntimeConfig;
use crossterm::style::Stylize;
use std::path::PathBuf;

/// Handle /config command.
pub fn handle_config_command(arg: &str) {
    let parts: Vec<&str> = arg.split_whitespace().collect();
    let subcommand = parts.first().copied().unwrap_or("show");

    match subcommand {
        "show" | "" => show_config(),
        "paths" => show_paths(),
        "export" => {
            let path = parts.get(1).map(|s| PathBuf::from(*s));
            export_config(path);
        }
        "diff" => show_diff(),
        "help" | "-h" | "--help" => print_help(),
        _ => {
            eprintln!("{}", format!("Unknown subcommand: {}", subcommand).red());
            print_help();
        }
    }
}

fn show_config() {
    let config = RuntimeConfig::load();

    println!("\n{}", "Runtime Configuration".bold().cyan());
    println!("{}", "═".repeat(50).dim());

    // Compression settings
    println!("\n{}", "📦 Compression".bold());
    println!(
        "  max_history_tokens: {}",
        config.compression.max_history_tokens.to_string().yellow()
    );
    println!(
        "  compression_threshold: {}",
        format!("{:.0}%", config.compression.compression_threshold * 100.0).yellow()
    );
    println!(
        "  preserve_tool_calls: {}",
        config.compression.preserve_tool_calls.to_string().yellow()
    );
    println!(
        "  preserve_recent_turns: {}",
        config.compression.preserve_recent_turns.to_string().yellow()
    );
    println!(
        "  strategy: {}",
        format!("{:?}", config.compression.strategy).yellow()
    );

    // Memory settings
    println!("\n{}", "🧠 Memory".bold());
    println!(
        "  retrieval_top_k: {}",
        config.memory.retrieval_top_k.to_string().yellow()
    );
    println!(
        "  min_relevance_score: {}",
        format!("{:.2}", config.memory.min_relevance_score).yellow()
    );
    println!(
        "  session_weight: {}",
        format!("{:.2}", config.memory.session_weight).yellow()
    );
    println!(
        "  long_term_weight: {}",
        format!("{:.2}", config.memory.long_term_weight).yellow()
    );

    // Tool selection settings
    println!("\n{}", "🔧 Tool Selection".bold());
    println!(
        "  confidence_threshold: {}",
        format!("{:.2}", config.tool_selection.confidence_threshold).yellow()
    );
    println!(
        "  max_tools: {}",
        config.tool_selection.max_tools.to_string().yellow()
    );
    println!(
        "  prefer_recent_tools: {}",
        config.tool_selection.prefer_recent_tools.to_string().yellow()
    );

    // Learning settings
    println!("\n{}", "📈 Learning".bold());
    println!(
        "  enabled: {}",
        config.learning.enabled.to_string().yellow()
    );
    println!(
        "  entity_decay_half_life_days: {}",
        config.learning.entity_decay_half_life_days.to_string().yellow()
    );
    println!(
        "  exploration_rate: {}",
        format!("{:.2}", config.learning.exploration_rate).yellow()
    );

    // Token budget settings
    println!("\n{}", "🎯 Token Budget".bold());
    println!(
        "  max_turn_input_tokens: {}",
        config.token_budget.max_turn_input_tokens.to_string().yellow()
    );
    println!(
        "  system_prompt_reserve: {}",
        config.token_budget.system_prompt_reserve.to_string().yellow()
    );
    println!(
        "  tools_reserve: {}",
        config.token_budget.tools_reserve.to_string().yellow()
    );

    // Telemetry settings
    println!("\n{}", "📊 Telemetry".bold());
    println!(
        "  capture_context_traces: {}",
        config.telemetry.capture_context_traces.to_string().yellow()
    );
    println!(
        "  capture_explanations: {}",
        config.telemetry.capture_explanations.to_string().yellow()
    );
    println!(
        "  persist_to_journal: {}",
        config.telemetry.persist_to_journal.to_string().yellow()
    );

    println!("\n{}", "Use `/config paths` to see configuration file locations.".dim());
}

fn show_paths() {
    println!("\n{}", "Configuration Paths".bold().cyan());
    println!("{}", "═".repeat(50).dim());

    // User config
    let user_config = dirs::home_dir()
        .map(|h| h.join(".astra/config/runtime.toml"))
        .unwrap_or_else(|| PathBuf::from("~/.astra/config/runtime.toml"));
    let user_exists = user_config.exists();

    println!("\n{}", "User-level (highest priority):".bold());
    println!(
        "  {} {}",
        user_config.display(),
        if user_exists {
            "✓".green().to_string()
        } else {
            "(not found)".dim().to_string()
        }
    );

    // Project config
    let project_config = std::env::current_dir()
        .map(|d| d.join(".astra/config/runtime.toml"))
        .unwrap_or_else(|_| PathBuf::from(".astra/config/runtime.toml"));
    let project_exists = project_config.exists();

    println!("\n{}", "Project-level:".bold());
    println!(
        "  {} {}",
        project_config.display(),
        if project_exists {
            "✓".green().to_string()
        } else {
            "(not found)".dim().to_string()
        }
    );

    // Environment variables
    println!("\n{}", "Environment Variables:".bold());
    let env_vars = [
        ("MO_MAX_HISTORY_TOKENS", "compression.max_history_tokens"),
        ("MO_COMPRESSION_THRESHOLD", "compression.compression_threshold"),
        ("MO_RETRIEVAL_TOP_K", "memory.retrieval_top_k"),
        ("MO_MAX_TURN_INPUT_TOKENS", "token_budget.max_turn_input_tokens"),
        ("MO_CAPTURE_TRACES", "telemetry.capture_context_traces"),
    ];

    for (var, config_path) in env_vars {
        let value = std::env::var(var).ok();
        println!(
            "  {} → {} {}",
            var.cyan(),
            config_path.dim(),
            if let Some(v) = value {
                format!("= {}", v).green().to_string()
            } else {
                "(not set)".dim().to_string()
            }
        );
    }

    println!(
        "\n{}",
        "Priority: env vars > project > user > defaults".dim()
    );
}

fn export_config(path: Option<PathBuf>) {
    let config = RuntimeConfig::load();
    let toml = match config.to_toml() {
        Ok(s) => s,
        Err(e) => {
            eprintln!("{}", format!("Failed to serialize config: {}", e).red());
            return;
        }
    };

    if let Some(p) = path {
        // Write to file
        if let Some(parent) = p.parent() {
            if !parent.exists() {
                if let Err(e) = std::fs::create_dir_all(parent) {
                    eprintln!("{}", format!("Failed to create directory: {}", e).red());
                    return;
                }
            }
        }
        match std::fs::write(&p, &toml) {
            Ok(_) => {
                println!("{} Configuration exported to {}", "✓".green(), p.display());
            }
            Err(e) => {
                eprintln!("{}", format!("Failed to write file: {}", e).red());
            }
        }
    } else {
        // Print to stdout
        println!("\n{}", "# Runtime Configuration (TOML)".dim());
        println!("{}", toml);
    }
}

fn show_diff() {
    let current = RuntimeConfig::load();
    let default = RuntimeConfig::default();

    println!("\n{}", "Configuration Differences from Defaults".bold().cyan());
    println!("{}", "═".repeat(50).dim());

    let mut has_diff = false;

    // Compression
    if current.compression.max_history_tokens != default.compression.max_history_tokens {
        has_diff = true;
        println!(
            "  compression.max_history_tokens: {} → {}",
            default.compression.max_history_tokens.to_string().dim(),
            current.compression.max_history_tokens.to_string().yellow()
        );
    }
    if (current.compression.compression_threshold - default.compression.compression_threshold).abs() > 0.001
    {
        has_diff = true;
        println!(
            "  compression.compression_threshold: {} → {}",
            format!("{:.2}", default.compression.compression_threshold).dim(),
            format!("{:.2}", current.compression.compression_threshold).yellow()
        );
    }

    // Memory
    if current.memory.retrieval_top_k != default.memory.retrieval_top_k {
        has_diff = true;
        println!(
            "  memory.retrieval_top_k: {} → {}",
            default.memory.retrieval_top_k.to_string().dim(),
            current.memory.retrieval_top_k.to_string().yellow()
        );
    }

    // Token budget
    if current.token_budget.max_turn_input_tokens != default.token_budget.max_turn_input_tokens {
        has_diff = true;
        println!(
            "  token_budget.max_turn_input_tokens: {} → {}",
            default.token_budget.max_turn_input_tokens.to_string().dim(),
            current.token_budget.max_turn_input_tokens.to_string().yellow()
        );
    }

    // Telemetry
    if current.telemetry.capture_context_traces != default.telemetry.capture_context_traces {
        has_diff = true;
        println!(
            "  telemetry.capture_context_traces: {} → {}",
            default.telemetry.capture_context_traces.to_string().dim(),
            current.telemetry.capture_context_traces.to_string().yellow()
        );
    }

    if !has_diff {
        println!("\n{}", "  All settings are at their default values.".dim());
    }
}

fn print_help() {
    println!(
        r#"
{title}

{usage}
  /config               Show current configuration
  /config show          Show current configuration  
  /config paths         Show configuration file paths
  /config export [path] Export configuration to file (or stdout)
  /config diff          Show differences from defaults

{examples}
  /config                       View all settings
  /config export ./my-config.toml   Save to file
  /config diff                  See what's changed from defaults

{env}
  MO_MAX_HISTORY_TOKENS=50000   Override max history tokens
  MO_CAPTURE_TRACES=1           Enable context assembly traces
"#,
        title = "Runtime Configuration Management".bold().cyan(),
        usage = "Usage:".bold(),
        examples = "Examples:".bold(),
        env = "Environment Variables:".bold(),
    );
}
