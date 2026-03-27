use std::{
    borrow::Cow,
    collections::{HashMap, HashSet},
    fs,
    io::{self, Write},
    path::PathBuf,
    process::{Command as SysCommand, Stdio},
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicBool, Ordering},
    },
    time::Instant,
};

use clap::{Args, Parser, Subcommand};
use crossterm::{
    cursor,
    event::{self, Event, KeyCode, KeyEvent, KeyModifiers},
    execute,
    style::Stylize,
    terminal,
};
use mo_agent_runtime::{prompts, tool_registry, tool_selector};
use mo_agent_services::event_ingestion;
use mo_agent_services::session_journal;

mod edge_tools;
mod manifest_loader;
use reqwest::header::{AUTHORIZATION, CONTENT_TYPE, HeaderMap, HeaderValue};
use rustyline::{
    Cmd as RlCmd, CompletionType, ConditionalEventHandler, Config, Context, Editor,
    Event as RlEvent, EventContext as RlEventContext, EventHandler as RlEventHandler, Helper,
    KeyCode as RlKeyCode, KeyEvent as RlKeyEvent, Modifiers as RlModifiers, Movement as RlMovement,
    completion::{Completer, Pair},
    error::ReadlineError,
    highlight::Highlighter,
    hint::Hinter,
    history::FileHistory,
    validate::{ValidationContext, ValidationResult, Validator},
};
use serde::{Deserialize, Serialize};

#[path = "mo_agent/auth_flow.rs"]
mod auth_flow;
#[path = "mo_agent/chat_stream.rs"]
mod chat_stream;
#[path = "mo_agent/cli_utils.rs"]
mod cli_utils;
#[path = "mo_agent/command_router.rs"]
mod command_router;
#[path = "mo_agent/permission_manager.rs"]
mod permission_manager;
#[path = "mo_agent/repl_runtime.rs"]
mod repl_runtime;
#[path = "mo_agent/repl_turn.rs"]
mod repl_turn;
#[path = "mo_agent/repl_ui.rs"]
mod repl_ui;
#[path = "mo_agent/slash_account.rs"]
mod slash_account;
#[path = "mo_agent/slash_info.rs"]
mod slash_info;
#[path = "mo_agent/slash_memory.rs"]
mod slash_memory;
#[path = "mo_agent/slash_session.rs"]
mod slash_session;
#[path = "mo_agent/slash_skill.rs"]
mod slash_skill;
#[path = "mo_agent/slash_state.rs"]
mod slash_state;
#[path = "mo_agent/stream_render.rs"]
mod stream_render;

use auth_flow::{clear_profile_last_session, do_login, do_register};
use chat_stream::{
    ChatTurnParams, is_session_not_found_error, looks_like_live_query_with_context, stream_chat_sse,
};
use cli_utils::{
    Profile, auth_headers, capitalize, compact_or_raw, get_profile_and_token, interactive_select,
    load_credentials, print_json_or_raw, print_markdown, profile_name, prompt_or,
    prompt_password_masked, read_api_error, resumable_last_session_id, save_credentials,
    tool_call_detail, truncate_str, urlencoding,
};
use command_router::execute_cli_command;
use permission_manager::PermissionManager;
use stream_render::{Spinner, consume_turn_sse};
#[cfg(test)]
use stream_render::{StreamRenderState, TurnResult, dispatch_turn_event_block};

use repl_runtime::{
    build_repl_editor, create_tool_selector, create_tool_selector_with_quality,
    current_access_token, ensure_repl_authenticated, initialize_repl_state, print_repl_banner,
};
use repl_turn::{ReplTurnContext, handle_chat_input};
use repl_ui::{
    ReplHelper, SlashStartCompleteHandler, clear_slash_overlay, history_path,
    is_slash_picker_active, print_slash_commands, resolve_slash_command, suggest_commands,
};
use slash_account::handle_account_command;
use slash_info::handle_info_command;
use slash_memory::handle_memory_domain_command;
use slash_session::handle_session_command;
#[cfg(test)]
use slash_session::resolve_journal_target_session;
use slash_skill::handle_skill_command;
use slash_state::{StateCommandContext, handle_state_command};

// ══════════════════════════════════════════════════════════════════════ CLI ══

#[derive(Parser, Debug)]
#[command(name = "mo-agent")]
#[command(about = "AI agent CLI — run `mo-agent` for interactive chat")]
struct Cli {
    #[arg(long, default_value = "http://127.0.0.1:8000")]
    api_url: String,
    #[arg(long)]
    profile: Option<String>,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand, Debug)]
#[command(allow_external_subcommands = true)]
enum Command {
    /// Start the interactive REPL (default when no args given)
    Interactive,
    Register(RegisterArgs),
    Login(LoginArgs),
    Whoami,
    Refresh,
    Logout,
    Health,
    Chat(ChatArgs),
    Replay(ReplayArgs),
    #[command(subcommand)]
    Session(SessionCmd),
    #[command(subcommand)]
    Model(ModelCmd),
    #[command(subcommand)]
    Skill(SkillCmd),
    /// Direct message: mo-agent "your question here"
    #[command(external_subcommand)]
    Message(Vec<String>),
}

#[derive(Args, Debug)]
struct RegisterArgs {
    #[arg(long)]
    username: Option<String>,
    #[arg(long)]
    email: Option<String>,
    #[arg(long)]
    password: Option<String>,
}

#[derive(Args, Debug)]
struct LoginArgs {
    #[arg(long)]
    username: Option<String>,
    #[arg(long)]
    password: Option<String>,
}

#[derive(Args, Debug)]
struct ChatArgs {
    #[arg(short = 'm', long = "message")]
    message: Option<String>,
    #[arg(long)]
    session_id: Option<String>,
    #[arg(long)]
    model: Option<String>,
    #[arg(long, default_value_t = false)]
    explain: bool,
    #[arg(short = 'y', long = "auto-approve", default_value_t = false)]
    auto_approve: bool,
}

#[derive(Subcommand, Debug)]
enum SessionCmd {
    List(SessionListArgs),
    Show(SessionShowArgs),
    Close(SessionShowArgs),
    Delete(SessionShowArgs),
}

#[derive(Args, Debug)]
struct SessionListArgs {
    #[arg(long)]
    agent_id: Option<String>,
    #[arg(long)]
    status: Option<String>,
    #[arg(long, default_value_t = 20)]
    limit: u32,
    #[arg(long, default_value_t = 0)]
    offset: u32,
}

#[derive(Args, Debug)]
struct SessionShowArgs {
    session_id: String,
}

#[derive(Subcommand, Debug)]
enum ModelCmd {
    List,
    Show(ModelShowArgs),
}

#[derive(Args, Debug)]
struct ModelShowArgs {
    model_name: String,
}

#[derive(Subcommand, Debug)]
enum SkillCmd {
    List(SkillListArgs),
    Show(SkillShowArgs),
    Register(SkillRegisterArgs),
    Status(SkillStatusArgs),
}

#[derive(Args, Debug)]
struct SkillListArgs {
    #[arg(long, default_value_t = 50)]
    limit: u32,
    #[arg(long, default_value_t = 0)]
    offset: u32,
}

#[derive(Args, Debug)]
struct SkillShowArgs {
    skill_id: String,
    #[arg(long)]
    version: Option<String>,
}

#[derive(Args, Debug)]
struct SkillStatusArgs {
    #[arg(long, default_value_t = 50)]
    per_group: u32,
}

#[derive(Args, Debug)]
struct SkillRegisterArgs {
    #[arg(long)]
    name: String,
    #[arg(long)]
    version: String,
    #[arg(long)]
    code: Option<String>,
    #[arg(long)]
    code_file: Option<String>,
    #[arg(long)]
    skill_id: Option<String>,
    #[arg(long)]
    description: Option<String>,
    #[arg(long)]
    metadata_json: Option<String>,
}

#[derive(Args, Debug)]
struct ReplayArgs {
    session_id: String,
    #[arg(long)]
    sandbox_name: Option<String>,
    #[arg(long, default_value_t = true)]
    mock_mode: bool,
    #[arg(long)]
    compare: bool,
}

// ═══════════════════════════════════════════════════════ Credentials ══════

// ══════════════════════════════════════════════════════════════════════════════

// ══════════════════════════════════════════════════════ SSE Streaming ════

#[derive(Debug)]
struct StreamResult {
    session_id: Option<String>,
    run_id: Option<String>,
    full_text: String,
    prompt_tokens: u64,
    completion_tokens: u64,
    tool_calls_count: u32,
    /// Tool names selected for LLM (first turn selection report).
    tools_selected: Vec<String>,
    /// Tool names actually invoked by LLM across all turns.
    tools_used: Vec<String>,
    /// Per-tool-call audit records: name, ok, ms, error.
    tool_call_records: Vec<mo_agent_services::session_journal::ToolCallRecord>,
    /// Token budget used by selected dynamic tools.
    budget_used: u32,
    /// Token budget pressure (0.0-0.9) from compaction tier.
    budget_pressure: f64,
    /// Stall events that occurred during the agentic loop (stall_type, turn_number).
    stall_events: Vec<(String, u32)>,
    /// TurnGuard verdict events (severity, turn, injections, avoid_tools, force_stop,
    /// nudge_count, total_errors, deprioritized_count). Only non-Healthy verdicts.
    verdict_events: Vec<VerdictEvent>,
    /// Step Protocol recorder summary for debugging and audit.
    step_recorder_summary: Option<mo_agent_runtime::pipeline::step_recorder::RecorderSummary>,
}

/// Structured audit record for a TurnGuard verdict.
#[derive(Debug, Clone)]
struct VerdictEvent {
    turn: u32,
    severity: String,
    injections: Vec<String>,
    avoid_tools: Vec<String>,
    force_stop: bool,
    nudge_count: usize,
    total_errors: usize,
    deprioritized_count: usize,
}

// ══════════════════════════════════════════════════════════ REPL State ════

#[derive(Clone, Copy, PartialEq, Debug)]
enum ExplainMode {
    Off,
    On,
    Verbose,
}

impl std::fmt::Display for ExplainMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ExplainMode::Off => write!(f, "off"),
            ExplainMode::On => write!(f, "on"),
            ExplainMode::Verbose => write!(f, "verbose"),
        }
    }
}

struct ReplState {
    session_id: Option<String>,
    run_id: Option<String>,
    model: Option<String>,
    turn: u32,
    last_response: Option<String>,
    explain: ExplainMode,
    verbose_mode: bool,
    history: Vec<(String, String)>, // (user_msg, assistant_msg)
    total_prompt_tokens: u64,
    total_completion_tokens: u64,
    skill_dev_name: Option<String>,
    skill_dev_dir: Option<String>,
    skill_dev_context: Option<String>,
    active_system_skills: Vec<prompts::SystemSkill>,
    context_budget: prompts::ContextBudget,
    journal: Option<session_journal::JournalWriter>,
    /// Tools used in the last turn — fed into selection for recency boost.
    recent_tools: Vec<String>,
    /// Session-persistent permission manager — "always"/"skip" survives across turns.
    perm_manager: PermissionManager,
    /// Async event ingestion sender for cloud push (None if MatrixOne unavailable).
    ingestion_sender: Option<event_ingestion::IngestionSender>,
    /// User ID for event ingestion attribution.
    ingestion_user_id: Option<String>,
    /// Shared MatrixOne pool for checkpoint push and cloud sync (None if unavailable).
    matrixone_pool: Option<std::sync::Arc<sqlx::Pool<sqlx::MySql>>>,
}

impl Default for ReplState {
    fn default() -> Self {
        Self {
            session_id: None,
            run_id: None,
            model: None,
            turn: 0,
            last_response: None,
            explain: ExplainMode::Off,
            verbose_mode: true,
            history: Vec::new(),
            total_prompt_tokens: 0,
            total_completion_tokens: 0,
            skill_dev_name: None,
            skill_dev_dir: None,
            skill_dev_context: None,
            active_system_skills: Vec::new(),
            context_budget: prompts::ContextBudget::default(),
            journal: None,
            recent_tools: Vec::new(),
            perm_manager: PermissionManager::new(false),
            ingestion_sender: None,
            ingestion_user_id: None,
            matrixone_pool: None,
        }
    }
}

// ═════════════════════════════════════════════════════════ ReplHelper ════

// ═════════════════════════════════════════════════════════ Clipboard ══════

// ══════════════════════════════════════════════════════ Slash Commands ════

/// Returns `true` when the REPL should exit.
async fn handle_slash_command(
    line: &str,
    client: &reqwest::Client,
    base: &str,
    profile: Option<&str>,
    state: &mut ReplState,
    token: Option<&str>,
    selector: &dyn tool_selector::ToolSelector,
) -> Result<bool, String> {
    clear_slash_overlay();

    let mut parts = line.splitn(2, ' ');
    let raw_cmd = parts.next().unwrap_or("");
    let arg = parts.next().unwrap_or("").trim();
    let cmd = match resolve_slash_command(raw_cmd) {
        Ok(command) => command,
        Err(candidates) if candidates.is_empty() => {
            let suggestions = suggest_commands(raw_cmd, 3);
            if suggestions.is_empty() {
                eprintln!(
                    "{}",
                    format!("  Unknown command: {}  (type /help for list)", raw_cmd).yellow()
                );
            } else {
                let hint = suggestions.join(", ");
                eprintln!(
                    "{}",
                    format!("  Unknown command: {}  (did you mean: {}?)", raw_cmd, hint).yellow()
                );
            }
            return Ok(false);
        }
        Err(candidates) if candidates.len() == 1 => candidates[0],
        Err(candidates) => {
            let preview: Vec<&str> = candidates.iter().take(5).copied().collect();
            eprintln!(
                "{}",
                format!("  Ambiguous: {}  — type more to narrow", preview.join(", ")).yellow()
            );
            return Ok(false);
        }
    };

    if cmd == "/" && arg.is_empty() && is_slash_picker_active() {
        return Ok(false);
    }

    match cmd {
        "/" | "/?" | "/commands" | "/help" => print_slash_commands(Some(arg)),

        "/model" if arg.is_empty() => {
            let Some(tok) = token else {
                eprintln!("{}", "  Not logged in. Use /login.".yellow());
                return Ok(false);
            };
            let resp = client
                .get(format!("{base}/models"))
                .headers(auth_headers(tok)?)
                .send()
                .await
                .map_err(|e| e.to_string())?;
            let status = resp.status();
            let body = resp.text().await.map_err(|e| e.to_string())?;
            if !status.is_success() {
                eprintln!(
                    "{}",
                    format!(
                        "  \u{2717} API Error ({}): {}",
                        status,
                        compact_or_raw(&body)
                    )
                    .red()
                );
            } else {
                let value: serde_json::Value = serde_json::from_str(&body).unwrap_or_default();
                let models = value
                    .as_array()
                    .cloned()
                    .or_else(|| value.get("models").and_then(|v| v.as_array()).cloned())
                    .unwrap_or_default();

                let items: Vec<(String, String)> = models
                    .iter()
                    .filter_map(|m| {
                        let name = m
                            .get("name")
                            .or_else(|| m.get("model_name"))
                            .and_then(|v| v.as_str())?;
                        let active = m.get("active").and_then(|v| v.as_bool()).unwrap_or(true);
                        if !active {
                            return None;
                        }
                        let desc = m
                            .get("description")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string();
                        Some((name.to_string(), desc))
                    })
                    .collect();

                if let Some(chosen) = interactive_select(
                    "Select model (type to search):",
                    &items,
                    state.model.as_deref(),
                ) {
                    state.model = Some(chosen.clone());
                    eprintln!("{}", format!("  ✓ Model set to: {chosen}").green());
                } else {
                    eprintln!("{}", "  Cancelled.".dim());
                }
            }
        }

        "/model" => {
            state.model = Some(arg.to_string());
            state.context_budget = prompts::budget_for_model(Some(arg));
            eprintln!("{}", format!("  \u{2713}  Model set to: {}", arg).green());
            if let Some(ref j) = state.journal {
                let _ = j.append(&session_journal::JournalEvent::config_change(
                    state.session_id.as_deref(),
                    "model",
                    arg,
                ));
            }
        }

        "/session" => handle_session_command(arg, state),

        "/history" | "/copy" | "/doctor" | "/context" | "/version" | "/rewind" => {
            handle_info_command(cmd, arg, client, base, state, token).await?;
        }

        "/skill" | "/skill list" | "/skill new" | "/skill test" | "/skill dev"
        | "/skill doctor" | "/skill validate" | "/skill config" | "/skill system" => {
            handle_skill_command(arg, client, base, state, token).await?;
        }

        "/register" | "/login" | "/logout" | "/memory-setup" => {
            handle_account_command(cmd, arg, client, base, profile).await?;
        }

        "/clear" | "/explain" | "/verbose" | "/compact" | "/reflect" => {
            handle_state_command(
                cmd,
                arg,
                StateCommandContext {
                    client,
                    base,
                    profile,
                    token,
                    selector,
                },
                state,
            )
            .await?;
        }

        "/memory" | "/plan" | "/task" => {
            handle_memory_domain_command(cmd, arg, client, base, state, token).await?;
        }

        "/exit" | "/quit" => {
            eprintln!("{}", "  Goodbye.".dim());
            return Ok(true);
        }

        _ => {
            let suggestions = suggest_commands(cmd, 3);
            if suggestions.is_empty() {
                eprintln!(
                    "{}",
                    format!("  Unknown command: {}  (type /help for list)", cmd).yellow()
                );
            } else {
                let hint = suggestions.join(", ");
                eprintln!(
                    "{}",
                    format!("  Unknown command: {}  (did you mean: {}?)", cmd, hint).yellow()
                );
            }
        }
    }

    Ok(false)
}

// ═══════════════════════════════════════════════════════════════ REPL ════

async fn run_chat_repl(
    client: &reqwest::Client,
    base: &str,
    profile: Option<&str>,
    initial_model: Option<&str>,
) -> Result<(), String> {
    if let Err(e) = ensure_repl_authenticated(client, base, profile).await {
        if e.contains("cancelled") || e.contains("exited before authentication") {
            return Ok(());
        }
        return Err(e);
    }

    let (mut editor, hist_path) = build_repl_editor()?;
    let mut state = initialize_repl_state(profile, initial_model);
    // Session-scoped quality tracker: tools that work well get boosted over time
    let quality_tracker = std::sync::Arc::new(std::sync::Mutex::new(
        tool_registry::ToolQualityTracker::new(),
    ));
    // Session-scoped confidence calibrator: thresholds adapt to correction rates
    let confidence_calibrator = std::sync::Arc::new(
        mo_agent_runtime::turn::routing_metrics::ConfidenceCalibrator::default(),
    );
    let (selector, pipeline_modules) = create_tool_selector_with_quality(
        client,
        base,
        profile,
        Some(quality_tracker),
        Some(confidence_calibrator),
    );

    // Load cross-session learning state (entity graph, patterns, calibration)
    {
        let profile_name = profile.unwrap_or("default");
        let loaded = mo_agent_runtime::pipeline::persistence::load_learning_state(
            profile_name,
            &pipeline_modules.entity_graph,
            &pipeline_modules.pattern_library,
            &pipeline_modules.calibrator,
        );
        if loaded {
            eprintln!("{}", "  ✓ Loaded learning state from prior sessions".dim());
        }
    }

    print_repl_banner(profile, &state);

    // ── Main loop ─────────────────────────────────────────────────────────────
    loop {
        let current_token = current_access_token(profile);

        // Simple prompt line with ❯
        if let Some(ref sname) = state.skill_dev_name {
            eprintln!("  \u{1f527} {}", format!("Skill dev: {sname}").cyan().dim());
        }
        let prompt_str = format!("{} ", "❯".cyan().bold(),);

        let readline = tokio::task::block_in_place(|| editor.readline(&prompt_str));

        match readline {
            Ok(line) => {
                clear_slash_overlay();
                // Multi-line: strip continuation backslashes and join lines
                let line = line
                    .lines()
                    .map(|l| l.strip_suffix('\\').unwrap_or(l))
                    .collect::<Vec<_>>()
                    .join("\n")
                    .trim()
                    .to_string();
                if line.is_empty() {
                    continue;
                }
                let _ = editor.add_history_entry(line.as_str());

                if line.starts_with('/') {
                    let should_exit = handle_slash_command(
                        &line,
                        client,
                        base,
                        profile,
                        &mut state,
                        current_token.as_deref(),
                        &*selector,
                    )
                    .await?;
                    if should_exit {
                        break;
                    }
                } else {
                    handle_chat_input(
                        line,
                        current_token.as_deref(),
                        &mut state,
                        ReplTurnContext {
                            client,
                            base,
                            profile,
                            selector: &*selector,
                        },
                    )
                    .await?;
                }
            }
            Err(ReadlineError::Interrupted) => {
                clear_slash_overlay();
                eprintln!("^C");
            }
            Err(ReadlineError::Eof) => {
                clear_slash_overlay();
                eprintln!("{}", "\nGoodbye.".dim());
                // Journal: session end
                if let Some(ref j) = state.journal {
                    let end_event = session_journal::JournalEvent::session_end(
                        state.session_id.as_deref(),
                        state.turn,
                    );
                    let _ = j.append(&end_event);
                    repl_turn::enqueue_ingestion_pub(&state, &end_event);
                }
                // Graceful ingestion shutdown: drop sender so worker flushes remaining buffer
                if let Some(sender) = state.ingestion_sender.take() {
                    sender.shutdown();
                }
                if state.session_id.is_some() {
                    let _ = clear_profile_last_session(profile);
                }
                break;
            }
            Err(e) => {
                clear_slash_overlay();
                eprintln!("{}", format!("readline error: {}", e).red());
                break;
            }
        }
    }

    // Save cross-session learning state
    {
        let profile_name = profile.unwrap_or("default");
        if let Err(e) = mo_agent_runtime::pipeline::persistence::save_learning_state(
            profile_name,
            &pipeline_modules.entity_graph,
            &pipeline_modules.pattern_library,
            &pipeline_modules.calibrator,
        ) {
            eprintln!(
                "{}",
                format!("  ⚠ Failed to save learning state: {e}").yellow()
            );
        }
    }

    let _ = editor.save_history(&hist_path);
    Ok(())
}

// ════════════════════════════════════════════════════════════════ main ════

#[tokio::main]
async fn main() -> Result<(), String> {
    dotenvy::dotenv().ok();
    let cli = Cli::parse();
    let client = reqwest::Client::builder()
        .no_proxy()
        .build()
        .expect("http client should build");
    let base = cli.api_url.trim_end_matches('/').to_string();

    // Set MEMORIA_API_KEY from credentials if not already set
    if std::env::var("MEMORIA_API_KEY").is_err() {
        let creds = load_credentials();
        let name = profile_name(cli.profile.as_deref(), &creds);
        if let Some(key) = creds
            .profiles
            .get(&name)
            .and_then(|p| p.memoria_api_key.as_deref())
        {
            unsafe {
                std::env::set_var("MEMORIA_API_KEY", key);
            }
        }
    }

    let Cli {
        api_url: _,
        profile,
        command,
    } = cli;

    // Set MEMORIA_API_KEY from credentials if not already set
    if std::env::var("MEMORIA_API_KEY").is_err() {
        let creds = load_credentials();
        let name = profile_name(profile.as_deref(), &creds);
        if let Some(key) = creds
            .profiles
            .get(&name)
            .and_then(|p| p.memoria_api_key.as_deref())
        {
            unsafe {
                std::env::set_var("MEMORIA_API_KEY", key);
            }
        }
    }

    execute_cli_command(command, profile, &client, &base).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dispatch_turn_event_collects_explain_events() {
        let mut result = TurnResult::new();
        let block = "data: {\"type\":\"explain\",\"total_ms\":7,\"tools_selected\":1,\"tools_available\":2,\"tool_selection\":null,\"tool_selection_fallback\":null,\"steps\":[]}\n\n";
        let mut render = StreamRenderState::new();
        dispatch_turn_event_block(block, &mut result, &mut render, false);
        assert_eq!(result.explain_turns.len(), 1);
        assert_eq!(
            result.explain_turns[0].get("type").and_then(|v| v.as_str()),
            Some("explain")
        );
    }

    #[test]
    fn dispatch_thinking_delta_captures_reasoning_content() {
        let mut result = TurnResult::new();
        let mut render = StreamRenderState::new();
        // thinking_delta (Kimi-k2.5 / Moonshot style)
        let block = "data: {\"type\":\"thinking_delta\",\"content\":\"Let me think...\"}\n\n";
        dispatch_turn_event_block(block, &mut result, &mut render, false);
        assert_eq!(result.reasoning_content, "Let me think...");
    }

    #[test]
    fn dispatch_reasoning_delta_captures_reasoning_content() {
        let mut result = TurnResult::new();
        let mut render = StreamRenderState::new();
        // reasoning_delta (DeepSeek-R1 style)
        let block = "data: {\"type\":\"reasoning_delta\",\"content\":\"Step 1: search PRs\"}\n\n";
        dispatch_turn_event_block(block, &mut result, &mut render, false);
        assert_eq!(result.reasoning_content, "Step 1: search PRs");
    }

    #[test]
    fn dispatch_thinking_delta_accumulates_across_events() {
        let mut result = TurnResult::new();
        let mut render = StreamRenderState::new();
        let block = concat!(
            "data: {\"type\":\"thinking_delta\",\"content\":\"part1\"}\n\n",
            "data: {\"type\":\"thinking_delta\",\"content\":\" part2\"}\n\n",
        );
        dispatch_turn_event_block(block, &mut result, &mut render, false);
        assert_eq!(result.reasoning_content, "part1 part2");
    }

    /// Verifies that an assistant tool-call message includes reasoning_content when the
    /// LLM produced thinking output.  Without this field, thinking models return HTTP 400:
    /// "thinking is enabled but reasoning_content is missing in assistant tool call message"
    #[test]
    fn assistant_tc_msg_includes_reasoning_content_when_present() {
        let reasoning = "I should call github_list_prs.".to_string();
        let tool_call = serde_json::json!({
            "id": "tc-1",
            "name": "github_list_prs",
            "arguments": {"owner": "matrixorigin", "repo": "matrixone"}
        });

        let mut assistant_tc_msg = serde_json::json!({
            "role": "assistant",
            "content": serde_json::Value::Null,
            "tool_calls": [{
                "id": tool_call["id"],
                "type": "function",
                "function": {
                    "name": tool_call["name"],
                    "arguments": serde_json::to_string(&tool_call["arguments"]).unwrap(),
                }
            }]
        });
        if !reasoning.is_empty() {
            assistant_tc_msg["reasoning_content"] = serde_json::Value::String(reasoning.clone());
        }

        assert_eq!(
            assistant_tc_msg["reasoning_content"].as_str(),
            Some(reasoning.as_str()),
            "reasoning_content must be present for thinking models"
        );
    }

    /// Verifies that when reasoning_content is empty (non-thinking model), it is NOT
    /// added to the assistant message (keeps payloads clean for standard models).
    #[test]
    fn assistant_tc_msg_omits_reasoning_content_when_empty() {
        let reasoning = String::new();
        let mut assistant_tc_msg = serde_json::json!({
            "role": "assistant",
            "content": serde_json::Value::Null,
            "tool_calls": []
        });
        if !reasoning.is_empty() {
            assistant_tc_msg["reasoning_content"] = serde_json::Value::String(reasoning);
        }
        assert!(
            assistant_tc_msg.get("reasoning_content").is_none(),
            "reasoning_content must NOT be present for non-thinking models"
        );
    }

    #[test]
    fn resolve_unique_prefix_command() {
        let resolved = resolve_slash_command("/mo").expect("/mo should resolve to /model");
        assert_eq!(resolved, "/model");
    }

    #[test]
    fn resolve_journal_target_session_uses_active_session_without_argument() {
        let state = ReplState {
            session_id: Some("sess-123".to_string()),
            ..Default::default()
        };
        let (resolved, from_prefix) =
            resolve_journal_target_session("", &state, "missing").expect("should resolve");
        assert_eq!(resolved, "sess-123");
        assert!(!from_prefix);
    }

    #[test]
    fn alias_completion_is_ranked_after_primary() {
        let candidates = repl_ui::completion_candidates("/");
        let commands: Vec<&str> = candidates.iter().map(|(cmd, _)| *cmd).collect();
        let help_idx = commands.iter().position(|cmd| *cmd == "/help").unwrap();
        let alias_idx = commands.iter().position(|cmd| *cmd == "/?").unwrap();
        assert!(
            help_idx < alias_idx,
            "primary commands should rank before aliases"
        );
    }

    #[tokio::test]
    async fn slash_explain_toggles_state() {
        let client = reqwest::Client::new();
        let mut state = ReplState::default();
        let selector = tool_selector::TfIdfSelector::new(tool_registry::ToolRegistry::new(
            edge_tools::all_tool_schemas(),
        ));
        assert_eq!(state.explain, ExplainMode::Off);

        let should_exit = handle_slash_command(
            "/explain",
            &client,
            "http://127.0.0.1:8000",
            None,
            &mut state,
            None,
            &selector,
        )
        .await
        .expect("slash command should succeed");
        assert!(!should_exit);
        assert_eq!(state.explain, ExplainMode::On);

        let should_exit = handle_slash_command(
            "/explain",
            &client,
            "http://127.0.0.1:8000",
            None,
            &mut state,
            None,
            &selector,
        )
        .await
        .expect("slash command should succeed");
        assert!(!should_exit);
        assert_eq!(state.explain, ExplainMode::Verbose);

        let should_exit = handle_slash_command(
            "/explain",
            &client,
            "http://127.0.0.1:8000",
            None,
            &mut state,
            None,
            &selector,
        )
        .await
        .expect("slash command should succeed");
        assert!(!should_exit);
        assert_eq!(state.explain, ExplainMode::Off);
    }

    #[test]
    fn quiet_dispatch_captures_text_without_output() {
        // In quiet mode, dispatch_turn_event_block should capture text but not print.
        // We can't easily test print suppression, but we verify text capture.
        let block = "data: {\"type\":\"text_delta\",\"content\":\"hello world\"}\n\n";
        let mut result = TurnResult::new();
        let mut render = StreamRenderState::new();
        dispatch_turn_event_block(block, &mut result, &mut render, true);
        assert_eq!(result.full_text, "hello world");
    }

    #[test]
    fn compacted_history_skips_empty_user_messages() {
        // When user message is empty (compacted context), only the assistant message
        // should appear in serialized history.
        let history: Vec<(String, String)> = vec![
            (
                String::new(),
                "[Prior context — 5 turns compacted]\n\nSummary here".to_string(),
            ),
            ("real question".to_string(), "real answer".to_string()),
        ];
        let messages: Vec<serde_json::Value> = history
            .iter()
            .flat_map(|(u, a)| {
                if u.is_empty() {
                    vec![serde_json::json!({"role": "assistant", "content": a})]
                } else {
                    vec![
                        serde_json::json!({"role": "user", "content": u}),
                        serde_json::json!({"role": "assistant", "content": a}),
                    ]
                }
            })
            .collect();
        assert_eq!(messages.len(), 3); // 1 assistant (compact) + 1 user + 1 assistant
        assert_eq!(messages[0]["role"], "assistant");
        assert!(
            messages[0]["content"]
                .as_str()
                .unwrap()
                .contains("compacted")
        );
        assert_eq!(messages[1]["role"], "user");
        assert_eq!(messages[2]["role"], "assistant");
    }

    #[test]
    fn system_skill_toggle_lifecycle() {
        let available = prompts::builtin_system_skills();
        let mut active: Vec<prompts::SystemSkill> = Vec::new();

        // Activate markdown
        let md = available.iter().find(|s| s.name == "markdown").unwrap();
        active.push(md.clone());
        assert_eq!(active.len(), 1);

        // Deactivate markdown
        active.retain(|s| s.name != "markdown");
        assert!(active.is_empty());

        // Activate both
        for s in &available {
            active.push(s.clone());
        }
        assert!(active.len() >= 2);

        // Build instructions
        let block = prompts::build_skill_instructions(&active);
        assert!(block.contains("Markdown"));
        assert!(block.contains("Concise"));
    }

    #[test]
    fn tool_name_validation_catches_unknown() {
        let valid: std::collections::HashSet<String> = ["bash", "read_file", "write_file"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert!(valid.contains("bash"));
        assert!(!valid.contains("run_tests")); // hallucinated tool
        assert!(!valid.contains(""));
    }

    // ═══════════════════════════════════════════════════════════════════════
    // Integration tests with mock HTTP servers
    // ═══════════════════════════════════════════════════════════════════════

    use axum::{Router, routing::get, routing::post};

    /// Spawn a mock HTTP server on a random port, return its base URL.
    async fn spawn_mock(app: Router) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let base = format!("http://{addr}");
        tokio::spawn(async move {
            axum::serve(listener, app).await.ok();
        });
        tokio::task::yield_now().await;
        base
    }

    /// HTTP/1.1-only client for mock server tests (axum test server is HTTP/1 only).
    fn mock_client() -> reqwest::Client {
        reqwest::Client::builder()
            .http1_only()
            .no_proxy()
            .build()
            .unwrap()
    }

    /// Set credentials dir to a temp path so tests don't pollute ~/.mo-agent/credentials.json.
    fn isolate_credentials() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        // SAFETY: tests run in a single process; env var is set before any credential I/O.
        unsafe { std::env::set_var("MO_AGENT_CREDENTIALS_DIR", dir.path()) };
        dir
    }

    // ── auth_flow ─────────────────────────────────────────────────────────

    #[tokio::test]
    async fn do_login_success() {
        let _creds_dir = isolate_credentials();
        let app = Router::new().route(
            "/auth/login",
            post(|| async {
                axum::Json(serde_json::json!({
                    "access_token": "tok-abc",
                    "refresh_token": "ref-xyz"
                }))
            }),
        );
        let base = spawn_mock(app).await;
        let client = mock_client();
        let result = do_login(&client, &base, Some("__test__"), "user1", "pass1").await;
        assert_eq!(result.unwrap(), "tok-abc");
    }

    #[tokio::test]
    async fn do_login_failure_returns_error() {
        let _creds_dir = isolate_credentials();
        let app = Router::new().route(
            "/auth/login",
            post(|| async {
                (
                    axum::http::StatusCode::UNAUTHORIZED,
                    axum::Json(serde_json::json!({"detail": "bad credentials"})),
                )
            }),
        );
        let base = spawn_mock(app).await;
        let client = mock_client();
        let result = do_login(&client, &base, Some("test-profile"), "user1", "wrong").await;
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("401"));
    }

    #[tokio::test]
    async fn do_register_success() {
        let _creds_dir = isolate_credentials();
        let app = Router::new().route(
            "/auth/register",
            post(|| async { axum::Json(serde_json::json!({"ok": true})) }),
        );
        let base = spawn_mock(app).await;
        let client = mock_client();
        let result = do_register(&client, &base, "newuser", "a@b.com", "pass").await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn do_register_conflict_returns_error() {
        let _creds_dir = isolate_credentials();
        let app = Router::new().route(
            "/auth/register",
            post(|| async {
                (
                    axum::http::StatusCode::CONFLICT,
                    axum::Json(serde_json::json!({"detail": "username taken"})),
                )
            }),
        );
        let base = spawn_mock(app).await;
        let client = mock_client();
        let result = do_register(&client, &base, "taken", "a@b.com", "pass").await;
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("409"));
    }

    // ── chat_stream (SSE agentic loop) ────────────────────────────────────

    fn sse_text_response(text: &str, session_id: &str) -> String {
        format!(
            "data: {{\"type\":\"session_info\",\"session_id\":\"{session_id}\"}}\n\n\
             data: {{\"type\":\"text_delta\",\"content\":\"{text}\"}}\n\n\
             data: {{\"type\":\"text_done\",\"full_text\":\"{text}\"}}\n\n\
             data: {{\"type\":\"usage\",\"prompt_tokens\":10,\"completion_tokens\":5}}\n\n\
             data: {{\"type\":\"turn_complete\",\"has_tool_calls\":false}}\n\n\
             data: [DONE]\n\n"
        )
    }

    #[tokio::test]
    async fn stream_chat_sse_simple_text_response() {
        let app = Router::new().route(
            "/chat/turn",
            post(|| async {
                (
                    [("content-type", "text/event-stream")],
                    sse_text_response("Hello!", "sess-001"),
                )
            }),
        );
        let base = spawn_mock(app).await;
        let client = mock_client();
        let registry = tool_registry::ToolRegistry::new(edge_tools::all_tool_schemas());
        let selector = tool_selector::TfIdfSelector::new(registry);
        let mut pm = PermissionManager::new(true);
        let result = stream_chat_sse(ChatTurnParams {
            client: &client,
            base: &base,
            token: "fake-token",
            message: "hi",
            session_id: None,
            model: None,
            explain: ExplainMode::Off,
            render_md: false,
            history: &[],
            perm_manager: &mut pm,
            verbose_mode: false,
            quiet: true,
            selector: &selector,
            recent_tools: &[],
        })
        .await
        .unwrap();
        assert_eq!(result.full_text, "Hello!");
        assert_eq!(result.session_id.as_deref(), Some("sess-001"));
        assert_eq!(result.prompt_tokens, 10);
        assert_eq!(result.completion_tokens, 5);
    }

    #[tokio::test]
    async fn stream_chat_sse_api_error_propagated() {
        let app = Router::new().route(
            "/chat/turn",
            post(|| async {
                (
                    axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                    axum::Json(serde_json::json!({"detail": "model overloaded"})),
                )
            }),
        );
        let base = spawn_mock(app).await;
        let client = mock_client();
        let registry = tool_registry::ToolRegistry::new(edge_tools::all_tool_schemas());
        let selector = tool_selector::TfIdfSelector::new(registry);
        let mut pm = PermissionManager::new(true);
        let result = stream_chat_sse(ChatTurnParams {
            client: &client,
            base: &base,
            token: "fake-token",
            message: "hi",
            session_id: None,
            model: None,
            explain: ExplainMode::Off,
            render_md: false,
            history: &[],
            perm_manager: &mut pm,
            verbose_mode: false,
            quiet: true,
            selector: &selector,
            recent_tools: &[],
        })
        .await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.contains("500"), "got: {err}");
    }

    #[tokio::test]
    async fn stream_chat_sse_with_tool_call_loop() {
        // Mock server: first call returns a tool call, second call returns text.
        let call_count = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let cc = call_count.clone();
        let app = Router::new().route(
            "/chat/turn",
            post(move || {
                let cc = cc.clone();
                async move {
                    let n = cc.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    let body = if n == 0 {
                        // First turn: return a tool call for bash
                        "data: {\"type\":\"session_info\",\"session_id\":\"sess-tc\"}\n\n\
                         data: {\"type\":\"tool_call\",\"id\":\"tc-1\",\"name\":\"bash\",\"arguments\":{\"command\":\"echo hi\"}}\n\n\
                         data: {\"type\":\"turn_complete\",\"has_tool_calls\":true}\n\n\
                         data: [DONE]\n\n"
                            .to_string()
                    } else {
                        // Second turn: return text
                        sse_text_response("Done!", "sess-tc")
                    };
                    (
                        [("content-type", "text/event-stream")],
                        body,
                    )
                }
            }),
        );
        let base = spawn_mock(app).await;
        let client = mock_client();
        let registry = tool_registry::ToolRegistry::new(edge_tools::all_tool_schemas());
        let selector = tool_selector::TfIdfSelector::new(registry);
        let mut pm = PermissionManager::new(true); // auto-approve
        let result = stream_chat_sse(ChatTurnParams {
            client: &client,
            base: &base,
            token: "fake-token",
            message: "run echo hi",
            session_id: None,
            model: None,
            explain: ExplainMode::Off,
            render_md: false,
            history: &[],
            perm_manager: &mut pm,
            verbose_mode: false,
            quiet: true,
            selector: &selector,
            recent_tools: &[],
        })
        .await
        .unwrap();
        assert_eq!(result.full_text, "Done!");
        assert!(result.tool_calls_count > 0);
        assert!(call_count.load(std::sync::atomic::Ordering::SeqCst) >= 2);
    }

    // ── slash commands with mock server ───────────────────────────────────

    #[tokio::test]
    async fn slash_clear_creates_new_session() {
        let _creds_dir = isolate_credentials();
        let app = Router::new().route(
            "/sessions",
            post(|| async { axum::Json(serde_json::json!({"session_id": "new-sess-42"})) }),
        );
        let base = spawn_mock(app).await;
        let client = mock_client();
        let selector = tool_selector::TfIdfSelector::new(tool_registry::ToolRegistry::new(
            edge_tools::all_tool_schemas(),
        ));
        let mut state = ReplState {
            session_id: Some("old-sess".to_string()),
            turn: 5,
            history: vec![("q".to_string(), "a".to_string())],
            ..Default::default()
        };
        let exit = handle_slash_command(
            "/clear",
            &client,
            &base,
            None,
            &mut state,
            Some("fake-token"),
            &selector,
        )
        .await
        .unwrap();
        assert!(!exit);
        assert_eq!(state.session_id.as_deref(), Some("new-sess-42"));
        assert_eq!(state.turn, 0);
        assert!(state.history.is_empty());
    }

    #[tokio::test]
    async fn slash_verbose_sets_flag() {
        let client = mock_client();
        let selector = tool_selector::TfIdfSelector::new(tool_registry::ToolRegistry::new(
            edge_tools::all_tool_schemas(),
        ));
        let mut state = ReplState {
            verbose_mode: false,
            ..Default::default()
        };
        let exit = handle_slash_command(
            "/verbose",
            &client,
            "http://unused",
            None,
            &mut state,
            None,
            &selector,
        )
        .await
        .unwrap();
        assert!(!exit);
        assert!(state.verbose_mode);
    }

    #[tokio::test]
    async fn slash_model_with_arg_sets_model() {
        let client = mock_client();
        let selector = tool_selector::TfIdfSelector::new(tool_registry::ToolRegistry::new(
            edge_tools::all_tool_schemas(),
        ));
        let mut state = ReplState::default();
        let exit = handle_slash_command(
            "/model gpt-4o",
            &client,
            "http://unused",
            None,
            &mut state,
            None,
            &selector,
        )
        .await
        .unwrap();
        assert!(!exit);
        assert_eq!(state.model.as_deref(), Some("gpt-4o"));
    }

    #[tokio::test]
    async fn slash_exit_returns_true() {
        let client = mock_client();
        let selector = tool_selector::TfIdfSelector::new(tool_registry::ToolRegistry::new(
            edge_tools::all_tool_schemas(),
        ));
        let mut state = ReplState::default();
        let exit = handle_slash_command(
            "/exit",
            &client,
            "http://unused",
            None,
            &mut state,
            None,
            &selector,
        )
        .await
        .unwrap();
        assert!(exit);
    }

    #[tokio::test]
    async fn slash_unknown_command_does_not_crash() {
        let client = mock_client();
        let selector = tool_selector::TfIdfSelector::new(tool_registry::ToolRegistry::new(
            edge_tools::all_tool_schemas(),
        ));
        let mut state = ReplState::default();
        let exit = handle_slash_command(
            "/nonexistent_command_xyz",
            &client,
            "http://unused",
            None,
            &mut state,
            None,
            &selector,
        )
        .await
        .unwrap();
        assert!(!exit);
    }

    // ── command_router ────────────────────────────────────────────────────

    #[tokio::test]
    async fn execute_cli_health_command() {
        let _creds_dir = isolate_credentials();
        let app = Router::new().route(
            "/health",
            get(|| async { axum::Json(serde_json::json!({"status": "ok"})) }),
        );
        let base = spawn_mock(app).await;
        let client = mock_client();
        let result = execute_cli_command(
            Some(Command::Health),
            Some("nonexistent-profile".to_string()),
            &client,
            &base,
        )
        .await;
        // Health command should succeed regardless of auth
        assert!(result.is_ok());
    }

    // ── repl_turn pure functions ──────────────────────────────────────────

    #[test]
    fn build_effective_line_plain() {
        let state = ReplState::default();
        let result = repl_turn::build_effective_line("hello", &state);
        assert_eq!(result, "hello");
    }

    #[test]
    fn build_effective_line_with_system_skills() {
        let mut state = ReplState::default();
        let skills = prompts::builtin_system_skills();
        if let Some(md) = skills.iter().find(|s| s.name == "markdown") {
            state.active_system_skills.push(md.clone());
        }
        let result = repl_turn::build_effective_line("hello", &state);
        assert!(result.contains("hello"));
        assert!(result.contains("Markdown"));
    }

    #[test]
    fn history_as_messages_normal_turns() {
        let history = vec![
            ("q1".to_string(), "a1".to_string()),
            ("q2".to_string(), "a2".to_string()),
        ];
        let msgs = repl_turn::history_as_messages(&history);
        assert_eq!(msgs.len(), 4);
        assert_eq!(msgs[0]["role"], "user");
        assert_eq!(msgs[1]["role"], "assistant");
    }

    #[test]
    fn history_as_messages_compacted_turn() {
        let history = vec![("".to_string(), "summary".to_string())];
        let msgs = repl_turn::history_as_messages(&history);
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0]["role"], "assistant");
    }

    // ── slash_memory mock ─────────────────────────────────────────────────

    #[tokio::test]
    async fn slash_memory_search_with_mock() {
        let app = Router::new().route(
            "/memory/search",
            post(|| async {
                axum::Json(serde_json::json!({
                    "results": [
                        {"content": "user prefers Rust", "memory_type": "profile", "score": 0.9}
                    ]
                }))
            }),
        );
        let base = spawn_mock(app).await;
        let client = mock_client();
        let mut state = ReplState {
            session_id: Some("sess-1".to_string()),
            ..Default::default()
        };
        // This should not panic or error
        let result = handle_memory_domain_command(
            "/memory",
            "search rust preferences",
            &client,
            &base,
            &mut state,
            Some("fake-token"),
        )
        .await;
        assert!(result.is_ok());
    }
}
