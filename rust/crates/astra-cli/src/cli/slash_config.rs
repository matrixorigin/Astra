//! `/config` slash command — view and manage runtime configuration.
//!
//! Primary entry point (matches the reference implementation):
//! - `/config` — Open the interactive panel (search, pick, edit).
//!
//! Sub-commands (unchanged, used by scripts / for introspection):
//! - `/config show` — Print current configuration
//! - `/config paths` — Show configuration file paths
//! - `/config export [path]` — Export configuration to file
//! - `/config diff` — Show difference from defaults
//! - `/config edit` — Alias for `/config` (kept for muscle memory)

use astra_config::runtime_config::RuntimeConfig;
use crossterm::style::Stylize;
use std::path::PathBuf;
use std::sync::{OnceLock, RwLock};

use super::theme;

static ACTIVE_MODEL_FOR_DISPLAY: OnceLock<RwLock<Option<String>>> = OnceLock::new();

pub(crate) fn set_active_model_for_display(model: Option<String>) {
    let lock = ACTIVE_MODEL_FOR_DISPLAY.get_or_init(|| RwLock::new(None));
    if let Ok(mut guard) = lock.write() {
        *guard = model;
    }
}

/// Resolve the active model for "effective budget" display. Returns
/// `None` when no model is pinned (legacy sub-runs, tests, etc.).
fn active_model_for_display() -> Option<String> {
    ACTIVE_MODEL_FOR_DISPLAY
        .get()
        .and_then(|lock| lock.read().ok().and_then(|guard| guard.clone()))
}

// ── `/session analyze deep <id>` TUI→line-mode hand-off ─────────
//
// The TUI-side `/session analyze` hands off to the line-mode deep
// analyzer via `SlashResult::Fallback`. Because the fallback only
// sees the bare command string we stash any user-supplied session
// id in this slot so the line-mode handler can pick it up instead
// of silently dropping it (the pre-fix behaviour).
static DEEP_ANALYZE_ARG: OnceLock<RwLock<Option<String>>> = OnceLock::new();

/// Store (or clear) the session id the user passed to
/// `/session analyze deep <id>`. `None` means "use the current
/// session".
pub(crate) fn set_deep_analyze_arg(arg: Option<String>) {
    let lock = DEEP_ANALYZE_ARG.get_or_init(|| RwLock::new(None));
    if let Ok(mut guard) = lock.write() {
        *guard = arg;
    }
}

/// Consume the stashed `/session analyze deep` argument. Returns
/// `None` when the caller didn't supply a session id (the
/// line-mode analyzer should then default to the current session).
#[allow(dead_code)]
pub(crate) fn take_deep_analyze_arg() -> Option<String> {
    DEEP_ANALYZE_ARG
        .get()
        .and_then(|lock| lock.write().ok().and_then(|mut g| g.take()))
}

/// Handle /config command.
///
/// Primary dispatch matches the reference CLI: bare `/config` opens the
/// interactive panel. The explicit `show` sub-command is retained for
/// scripts / introspection that want the static print.
pub fn handle_config_command(arg: &str) {
    let parts: Vec<&str> = arg.split_whitespace().collect();
    // Empty arg → open interactive panel. `edit` is an alias.
    let subcommand = parts.first().copied().unwrap_or("");

    match subcommand {
        "" | "edit" => run_config_edit(),
        "show" => show_config(),
        "paths" => show_paths(),
        "sources" => show_sources(),
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

    println!("\n{}", "Runtime Configuration".bold().magenta());
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
        config
            .compression
            .preserve_recent_turns
            .to_string()
            .yellow()
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
        config
            .tool_selection
            .prefer_recent_tools
            .to_string()
            .yellow()
    );

    // Token budget settings
    println!("\n{}", "🎯 Token Budget".bold());
    println!(
        "  max_turn_input_tokens: {}",
        config
            .token_budget
            .max_turn_input_tokens
            .to_string()
            .yellow()
    );
    // Effective-for-model line. The configured value above is one number;
    // the actual budget a turn sees is model-dependent because 1M-window
    // models (Sonnet 4.6 / Opus 4.6) read 80% of their window directly,
    // bypassing the configured default. Surface both so operators stop
    // assuming their Sonnet 4.6 is stuck at 200k.
    let effective_model = active_model_for_display();
    let effective_budget = astra_config::config_overlay::effective_budget_for_model(
        &config,
        effective_model.as_deref(),
    );
    match effective_model.as_deref() {
        Some(model) if effective_budget != config.token_budget.max_turn_input_tokens as u64 => {
            println!(
                "  effective_for_{}: {} {}",
                model,
                effective_budget.to_string().magenta(),
                "(from model context window)".dim()
            );
        }
        Some(model) => {
            println!(
                "  effective_for_{}: {} {}",
                model,
                effective_budget.to_string().magenta(),
                "(same as configured)".dim()
            );
        }
        None => {
            println!(
                "  {}",
                "(no active model known; effective budget = configured)".dim()
            );
        }
    }
    println!(
        "  system_prompt_reserve: {}",
        config
            .token_budget
            .system_prompt_reserve
            .to_string()
            .yellow()
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

    println!(
        "\n{}",
        "Use `/config paths` to see configuration file locations.".dim()
    );
    println!(
        "{}",
        "Use `/config sources` to see where each value came from.".dim()
    );
}

/// Show where each non-default configuration value originated.
fn show_sources() {
    let defaults = RuntimeConfig::default();

    // Check which config files exist
    let user_config_path = dirs::home_dir().map(|h| h.join(".astra/config/runtime.toml"));
    let user_exists = user_config_path.as_ref().is_some_and(|p| p.exists());

    let project_config_path = std::env::current_dir()
        .map(|d| d.join(".astra/config/runtime.toml"))
        .ok();
    let project_exists = project_config_path.as_ref().is_some_and(|p| p.exists());

    // Final merged config
    let final_config = RuntimeConfig::load();

    println!(
        "\n{}",
        "Configuration Sources (showing non-default values)"
            .bold()
            .magenta()
    );
    println!("{}", "═".repeat(55).dim());
    println!(
        "  {} = default, {} = user, {} = project, {} = env",
        "D".dim(),
        "U".blue(),
        "P".magenta(),
        "E".green()
    );

    let mut shown_any = false;

    // Helper to determine likely source of a value
    let source_for = |env_var: &str| -> String {
        if std::env::var(env_var).is_ok() {
            format!("{} env", "E".green())
        } else if project_exists {
            format!("{} project", "P".magenta())
        } else if user_exists {
            format!("{} user", "U".blue())
        } else {
            format!("{} (unknown)", "?".dim())
        }
    };

    // Compression
    if final_config.compression.max_history_tokens != defaults.compression.max_history_tokens {
        shown_any = true;
        println!(
            "  • {} = {} [{}]",
            "compression.max_history_tokens".magenta(),
            final_config
                .compression
                .max_history_tokens
                .to_string()
                .yellow(),
            source_for("ASTRA_MAX_HISTORY_TOKENS")
        );
    }

    if (final_config.compression.compression_threshold - defaults.compression.compression_threshold)
        .abs()
        > 0.001
    {
        shown_any = true;
        println!(
            "  • {} = {} [{}]",
            "compression.compression_threshold".magenta(),
            format!("{:.2}", final_config.compression.compression_threshold).yellow(),
            source_for("ASTRA_COMPRESSION_THRESHOLD")
        );
    }

    // Memory
    if final_config.memory.retrieval_top_k != defaults.memory.retrieval_top_k {
        shown_any = true;
        println!(
            "  • {} = {} [{}]",
            "memory.retrieval_top_k".magenta(),
            final_config.memory.retrieval_top_k.to_string().yellow(),
            source_for("ASTRA_RETRIEVAL_TOP_K")
        );
    }

    // Token budget
    if final_config.token_budget.max_turn_input_tokens
        != defaults.token_budget.max_turn_input_tokens
    {
        shown_any = true;
        println!(
            "  • {} = {} [{}]",
            "token_budget.max_turn_input_tokens".magenta(),
            final_config
                .token_budget
                .max_turn_input_tokens
                .to_string()
                .yellow(),
            source_for("ASTRA_MAX_TURN_INPUT_TOKENS")
        );
    }

    // Telemetry
    if final_config.telemetry.capture_context_traces != defaults.telemetry.capture_context_traces {
        shown_any = true;
        println!(
            "  • {} = {} [{}]",
            "telemetry.capture_context_traces".magenta(),
            final_config
                .telemetry
                .capture_context_traces
                .to_string()
                .yellow(),
            source_for("ASTRA_CAPTURE_TRACES")
        );
    }

    if !shown_any {
        println!("\n{}", "  All settings are at their default values.".dim());
    }

    println!("\n{}", "Priority: env > project > user > defaults".dim());
}

fn show_paths() {
    println!("\n{}", "Configuration Paths".bold().magenta());
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
            theme::icon_ok().to_string()
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
            theme::icon_ok().to_string()
        } else {
            "(not found)".dim().to_string()
        }
    );

    // Environment variables
    println!("\n{}", "Environment Variables:".bold());
    let env_vars = [
        ("ASTRA_MAX_HISTORY_TOKENS", "compression.max_history_tokens"),
        (
            "ASTRA_COMPRESSION_THRESHOLD",
            "compression.compression_threshold",
        ),
        ("ASTRA_RETRIEVAL_TOP_K", "memory.retrieval_top_k"),
        (
            "ASTRA_MAX_TURN_INPUT_TOKENS",
            "token_budget.max_turn_input_tokens",
        ),
        ("ASTRA_CAPTURE_TRACES", "telemetry.capture_context_traces"),
    ];

    for (var, config_path) in env_vars {
        let value = std::env::var(var).ok();
        println!(
            "  {} → {} {}",
            var.magenta(),
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
                println!("{} Configuration exported to {}", theme::icon_ok(), p.display());
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

    println!(
        "\n{}",
        "Configuration Differences from Defaults".bold().magenta()
    );
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
    if (current.compression.compression_threshold - default.compression.compression_threshold).abs()
        > 0.001
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
            current
                .token_budget
                .max_turn_input_tokens
                .to_string()
                .yellow()
        );
    }

    // Telemetry
    if current.telemetry.capture_context_traces != default.telemetry.capture_context_traces {
        has_diff = true;
        println!(
            "  telemetry.capture_context_traces: {} → {}",
            default.telemetry.capture_context_traces.to_string().dim(),
            current
                .telemetry
                .capture_context_traces
                .to_string()
                .yellow()
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
  /config               Open the interactive panel (search, pick, edit)
  /config edit          Alias for /config
  /config show          Print current configuration (non-interactive)
  /config paths         Show configuration file paths
  /config sources       Show where each non-default value came from
  /config export [path] Export configuration to file (or stdout)
  /config diff          Show differences from defaults

{hierarchy}
  Priority (higher overrides lower):
  1. Environment variables (MO_*)
  2. Project-level: .astra/config/runtime.toml
  3. User-level: ~/.astra/config/runtime.toml  
  4. Built-in defaults

{examples}
  /config                       View all settings
  /config sources               See value origins (user/project/env)
  /config export ./my-config.toml   Save to file
  /config diff                  See what's changed from defaults

{env}
  ASTRA_MAX_HISTORY_TOKENS=50000   Override max history tokens
  ASTRA_CAPTURE_TRACES=1           Enable context assembly traces
"#,
        title = "Runtime Configuration Management".bold().magenta(),
        usage = "Usage:".bold(),
        hierarchy = "Configuration Hierarchy:".bold(),
        examples = "Examples:".bold(),
        env = "Environment Variables:".bold(),
    );
}

// ─── /config edit ─────────────────────────────────────────────────────────

/// Interactive edit loop modelled after the reference implementation's
/// Config.tsx state machine:
///
///   (search) → pick item → dispatch by kind → write back → persist →
///   (search) …
///
/// Differences from the TSX original (by necessity, not by choice):
///   * No Pane/Tabs chrome — line-mode inquire::Select is our selector.
///   * No ThemePicker/ModelPicker submenu — we lean on the catalog's
///     `SettingKind::Enum` options instead of specialised components.
///   * Persistence writes to the user-level TOML at ~/.astra/config/runtime.toml,
///     matching what `/config paths` labels as the durable home for
///     user-initiated edits. Project-level writes are a follow-up.
///
/// Returning from this function ends the /config edit session. Ctrl+C /
/// Esc from inquire is surfaced as a graceful cancel.
fn run_config_edit() {
    use astra_config::config_overlay::{
        SettingItem, apply_edit, build_settings_catalog, filter_settings,
    };

    let config = RuntimeConfig::load();
    let catalog = build_settings_catalog(&config);

    // Outer search + select loop. Each pass asks for an optional query,
    // then lets the user pick from the filtered list. Enter on an item
    // opens the per-kind editor; a special "(done — save and exit)"
    // sentinel exits.
    let mut working = config;
    let mut dirty = false;

    loop {
        let query = match inquire::Text::new("Search setting (blank = all, Esc to finish):")
            .with_help_message("Type a keyword; matches id or label. Empty shows all.")
            .prompt_skippable()
        {
            Ok(Some(q)) => q,
            Ok(None) | Err(_) => break, // Esc / Ctrl+C
        };
        let filtered = filter_settings(&catalog_with_values(&catalog, &working), &query);
        if filtered.is_empty() {
            println!("{}", format!("No settings match `{query}`.").dim());
            continue;
        }

        let labels: Vec<String> = filtered
            .iter()
            .map(|i| format!("{}  [{}]  = {}", i.label, i.id, render_value(&i.value)))
            .collect();

        let picked = match inquire::Select::new("Pick a setting to edit (Esc to return):", labels)
            .with_page_size(15)
            .prompt_skippable()
        {
            Ok(Some(lbl)) => lbl,
            Ok(None) | Err(_) => continue,
        };
        let idx = match filtered.iter().position(|i| {
            format!("{}  [{}]  = {}", i.label, i.id, render_value(&i.value)) == picked
        }) {
            Some(n) => n,
            None => continue,
        };
        let item: &SettingItem = &filtered[idx];

        let new_value = match prompt_new_value(item) {
            Some(v) => v,
            None => continue, // user cancelled the per-kind editor
        };
        match apply_edit(working.clone(), &item.id, new_value.clone()) {
            Ok(next) => {
                working = next;
                dirty = true;
                println!(
                    "  {} {} = {}",
                    theme::icon_ok(),
                    item.id.clone().magenta(),
                    render_value(&new_value).yellow()
                );
            }
            Err(err) => {
                eprintln!("  {} {}", "✗".red(), err.to_string().red());
            }
        }
    }

    if !dirty {
        println!("{}", "No changes made.".dim());
        return;
    }

    // Confirm-then-save. Writing to user-level is the safer default;
    // project-level overrides would silently affect collaborators.
    let save = inquire::Confirm::new("Save changes to ~/.astra/config/runtime.toml?")
        .with_default(true)
        .prompt_skippable()
        .ok()
        .flatten()
        .unwrap_or(false);
    if !save {
        println!("{}", "Discarded — no files written.".dim());
        return;
    }
    match write_user_runtime_toml(&working) {
        Ok(path) => {
            println!(
                "  {} {}",
                theme::icon_ok(),
                format!("Saved to {}", path.display()).magenta()
            );
            // Content-addressed put so the saved config lands in the
            // version store and shows up in `astra config version list`.
            // Best-effort: no session / journal context in line mode;
            // the next session startup will write a `startup` journal
            // event with the new id.
            use astra_config::config_versions::ConfigVersionStore;
            if let Some(store) = astra_config::config_versions::LocalFileStore::at_default_root() {
                let meta = astra_config::config_versions::PutMetadata {
                    source_session: None,
                    parent: None,
                };
                if let Ok(id) = store.put(&working, meta) {
                    println!("  {}  config version: {}", "·".dim(), id.to_string().magenta());
                }
            }
        }
        Err(err) => {
            eprintln!("  {} Failed to save: {}", "✗".red(), err.to_string().red());
        }
    }
}

/// The catalog built by `build_settings_catalog(&config)` is a snapshot of
/// values at the time it was built. As the user edits, we need the list
/// to reflect the in-progress config — so rebuild from the working copy
/// every iteration. Cheap: the catalog has ~15 entries.
fn catalog_with_values(
    _stale: &[astra_config::config_overlay::SettingItem],
    working: &RuntimeConfig,
) -> Vec<astra_config::config_overlay::SettingItem> {
    astra_config::config_overlay::build_settings_catalog(working)
}

fn render_value(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::Bool(b) => b.to_string(),
        serde_json::Value::Number(n) => n.to_string(),
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Null => "null".to_string(),
        other => other.to_string(),
    }
}

/// Per-kind editor. Returns `None` if the user cancels mid-edit.
fn prompt_new_value(item: &astra_config::config_overlay::SettingItem) -> Option<serde_json::Value> {
    use astra_config::config_overlay::SettingKind;
    match &item.kind {
        SettingKind::Bool => {
            let current = item.value_as_bool().unwrap_or(false);
            inquire::Confirm::new(&format!("{} →", item.label))
                .with_default(!current)
                .prompt_skippable()
                .ok()
                .flatten()
                .map(serde_json::Value::from)
        }
        SettingKind::Number { min, max, .. } => {
            let current = item
                .value_as_number()
                .map(|n| n.to_string())
                .unwrap_or_default();
            let range_hint = format!("range {}..={}", min, max);
            let text = inquire::Text::new(&format!("{} →", item.label))
                .with_initial_value(&current)
                .with_help_message(&range_hint)
                .prompt_skippable()
                .ok()
                .flatten()?;
            // Accept either integer or float — serde_json::Value carries
            // both; apply_edit's as_u32 helper will coerce integer-valued
            // floats correctly.
            match text.parse::<f64>() {
                Ok(n) if n.is_finite() => Some(serde_json::json!(n)),
                _ => {
                    eprintln!("  {}", format!("Invalid number: {}", text).red());
                    None
                }
            }
        }
        SettingKind::Enum { options } => {
            inquire::Select::new(&format!("{} →", item.label), options.clone())
                .prompt_skippable()
                .ok()
                .flatten()
                .map(serde_json::Value::from)
        }
    }
}

/// Write `config` as TOML to `~/.astra/config/runtime.toml`, creating the
/// directory if missing. Returns the path on success.
fn write_user_runtime_toml(config: &RuntimeConfig) -> std::io::Result<PathBuf> {
    let home = dirs::home_dir().ok_or_else(|| std::io::Error::other("home directory not found"))?;
    let dir = home.join(".astra/config");
    std::fs::create_dir_all(&dir)?;
    let path = dir.join("runtime.toml");
    let toml_str = toml::to_string_pretty(config)
        .map_err(|e| std::io::Error::other(format!("TOML serialise failed: {e}")))?;
    std::fs::write(&path, toml_str)?;
    Ok(path)
}
