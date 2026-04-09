// Clippy 1.94 — allow backlog in the large CLI binary; refine incrementally.
#![allow(
    dead_code,
    clippy::collapsible_if,
    clippy::derivable_impls,
    clippy::field_reassign_with_default,
    clippy::items_after_test_module,
    clippy::let_unit_value,
    clippy::manual_strip,
    clippy::needless_borrow,
    clippy::redundant_closure,
    clippy::single_match,
    clippy::unnecessary_mut_passed
)]

use std::{
    borrow::Cow,
    collections::{HashMap, HashSet},
    fs,
    io::{self, Write},
    path::{Path, PathBuf},
    process::{Command as SysCommand, Stdio},
    sync::{Mutex, OnceLock},
};

use astra_core::SharedPool;
use astra_runtime::{plan_decompose, prompts, tool_registry, tool_selector};
use astra_services::session_journal;
use clap::{Args, Parser, Subcommand};
use crossterm::{
    cursor,
    event::{self, Event, KeyCode, KeyEvent, KeyModifiers},
    execute,
    style::Stylize,
    terminal,
};

mod edge_tools;
mod manifest_loader;
mod mcp_client;
mod skill_instructions;
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

#[path = "cli/agent_loader.rs"]
mod agent_loader;
#[path = "cli/auth_flow.rs"]
mod auth_flow;
#[path = "cli/chat_stream/mod.rs"]
mod chat_stream;
#[path = "cli/cli_formatting.rs"]
mod cli_formatting;
#[path = "cli/cli_utils.rs"]
mod cli_utils;
#[path = "cli/command_router.rs"]
mod command_router;
#[path = "cli/delegate_subrun.rs"]
mod delegate_subrun;
#[path = "cli/diff_presenter.rs"]
mod diff_presenter;
#[path = "cli/durable_bridge.rs"]
mod durable_bridge;
#[path = "cli/edge_lifecycle.rs"]
mod edge_lifecycle;
#[path = "cli/effects/mod.rs"]
mod effects;
#[path = "cli/journal_digest.rs"]
mod journal_digest;
#[path = "cli/permission_manager.rs"]
mod permission_manager;
#[path = "cli/plan_executor.rs"]
mod plan_executor;
#[path = "cli/plan_interaction.rs"]
mod plan_interaction;
#[path = "cli/readline_actor.rs"]
mod readline_actor;
#[path = "cli/repl_runtime.rs"]
mod repl_runtime;
#[path = "cli/repl_turn.rs"]
mod repl_turn;
#[path = "cli/repl_ui.rs"]
mod repl_ui;
#[path = "cli/skill_subrun.rs"]
mod skill_subrun;
#[path = "cli/slash_account.rs"]
mod slash_account;
#[path = "cli/slash_agent.rs"]
mod slash_agent;
#[path = "cli/slash_bug.rs"]
mod slash_bug;
#[path = "cli/slash_debug.rs"]
mod slash_debug;
#[path = "cli/slash_info.rs"]
mod slash_info;
#[path = "cli/slash_mcp.rs"]
mod slash_mcp;
#[path = "cli/slash_memory.rs"]
mod slash_memory;
#[path = "cli/slash_messaging.rs"]
mod slash_messaging;
#[path = "cli/slash_session.rs"]
mod slash_session;
#[path = "cli/slash_skill.rs"]
mod slash_skill;
#[path = "cli/slash_state.rs"]
mod slash_state;
#[path = "cli/slash_team.rs"]
mod slash_team;
#[path = "cli/spawn_subrun.rs"]
mod spawn_subrun;
#[path = "cli/sse_utils.rs"]
mod sse_utils;
#[path = "cli/stream_render.rs"]
mod stream_render;
#[path = "cli/streaming_md.rs"]
mod streaming_md;
#[path = "cli/terminal_region.rs"]
mod terminal_region;
#[path = "cli/theme.rs"]
mod theme;

use astra_runtime::turn::chat_turn_heuristics::{
    is_session_not_found_error, looks_like_live_query_with_context,
};
use auth_flow::{clear_profile_last_session, do_login, do_register};
use chat_stream::{ChatTurnParams, stream_chat_sse};
use cli_utils::{
    Profile, compact_or_raw, get_profile_and_token, interactive_select, load_credentials,
    map_thin_err, prefix_chars, print_json_or_raw, profile_name, prompt_or, prompt_password_masked,
    resumable_last_session_id, save_credentials, truncate_str, urlencoding,
};
use command_router::{ExitCode, execute_cli_command, run_print_mode};
use edge_lifecycle::register_and_start_heartbeat;
use permission_manager::PermissionManager;
#[cfg(test)]
use stream_render::{StreamRenderState, TurnResult, dispatch_turn_event_block};

use plan_interaction::{handle_plan_mode_input, plan_execution_ui_active};
use repl_runtime::{
    build_repl_editor, check_server_has_models, create_background_plan_selector,
    create_tool_selector, create_tool_selector_quiet, create_tool_selector_with_quality,
    current_access_token, initialize_repl_state, print_repl_banner, try_silent_auth,
};
use repl_turn::{ReplTurnContext, create_manual_repl_checkpoint, handle_chat_input};
use repl_ui::{
    ReplHelper, SlashStartCompleteHandler, clear_slash_overlay, history_path,
    is_slash_picker_active, print_keyboard_shortcuts, print_slash_commands, resolve_slash_command,
    suggest_commands,
};
use slash_account::handle_account_command;
use slash_bug::handle_bug_command;
use slash_debug::handle_debug_command;
use slash_info::handle_info_command;
use slash_memory::handle_memory_domain_command;
use slash_messaging::handle_messaging_command;
use slash_session::handle_session_command;
#[cfg(test)]
use slash_session::resolve_journal_target_session;
use slash_skill::handle_skill_command;
use slash_state::{StateCommandContext, handle_state_command};

// ── Panic-safe & signal-safe session guard ────────────────────────────────────
// On panic or SIGTERM, writes a `session_end` event to the local journal
// so the session file is properly closed even on unexpected crashes.

/// Session context stored globally so the panic/signal hooks can write `session_end`.
struct PanicSessionGuard {
    session_id: String,
    turn: u32,
}

static PANIC_SESSION_GUARD: std::sync::Mutex<Option<PanicSessionGuard>> =
    std::sync::Mutex::new(None);

/// Global reference to the MatrixCloudRuntime so the SIGTERM handler can flush
/// ingestion before exit. Set once when the REPL creates the runtime.
static SIGTERM_RUNTIME: OnceLock<std::sync::Arc<astra_runtime::MatrixCloudRuntime>> =
    OnceLock::new();

/// Best-effort write of `session_end` to journal from the global guard.
/// Safe to call from panic hooks and signal handlers (no async, no cloud).
fn emergency_session_end() {
    if let Ok(guard) = PANIC_SESSION_GUARD.lock() {
        if let Some(ref ctx) = *guard {
            let end_event =
                session_journal::JournalEvent::session_end(Some(ctx.session_id.as_str()), ctx.turn);
            if let Ok(writer) = session_journal::JournalWriter::new(&ctx.session_id) {
                let _ = writer.append(&end_event);
            }
        }
    }
}

/// Install a panic hook that writes `session_end` to the local journal.
/// Called once at startup before the REPL loop.
fn install_session_panic_hook() {
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        emergency_session_end();
        default_hook(info);
    }));
}

/// Install a SIGTERM handler that writes `session_end` and flushes ingestion before exit.
/// Must be called inside a tokio runtime.
fn install_sigterm_handler() {
    tokio::spawn(async {
        #[cfg(unix)]
        {
            use tokio::signal::unix::{SignalKind, signal};
            if let Ok(mut sigterm) = signal(SignalKind::terminate()) {
                sigterm.recv().await;
                emergency_session_end();
                // Flush cloud ingestion before exit (best-effort, 15s timeout)
                if let Some(mc) = SIGTERM_RUNTIME.get() {
                    mc.shutdown_ingestion_and_wait().await;
                }
                std::process::exit(0);
            }
        }
    });
}

/// Update the global panic guard with current session state.
fn update_panic_guard(session_id: &str, turn: u32) {
    if let Ok(mut guard) = PANIC_SESSION_GUARD.lock() {
        *guard = Some(PanicSessionGuard {
            session_id: session_id.to_string(),
            turn,
        });
    }
}

/// Clear the panic guard (e.g., on graceful exit after session_end is already written).
fn clear_panic_guard() {
    if let Ok(mut guard) = PANIC_SESSION_GUARD.lock() {
        *guard = None;
    }
}

// ══════════════════════════════════════════════════════════════════════ CLI ══

#[derive(Parser, Debug)]
#[command(name = "astra")]
#[command(about = "AI agent CLI — run `astra` for interactive chat")]
struct Cli {
    #[arg(long, default_value = "http://127.0.0.1:8000")]
    api_url: String,
    #[arg(long)]
    profile: Option<String>,
    /// Model to use (overrides config default_model)
    #[arg(long = "model")]
    model: Option<String>,
    /// Print mode: send prompt, print response, exit. No tools, no interaction.
    /// Usage: astra -p "your question" or echo "question" | astra -p
    #[arg(short = 'p', long = "print")]
    print: bool,
    /// Output format for --print mode
    #[arg(long = "output-format", default_value = "text")]
    output_format: String,
    /// Continue the most recent conversation
    #[arg(short = 'c', long = "continue")]
    continue_last: bool,
    /// Resume a specific session by ID (or prefix)
    #[arg(short = 'r', long = "resume")]
    resume: Option<String>,
    /// Auto-approve tool calls without prompting
    #[arg(short = 'y', long = "yes")]
    yes: bool,
    /// System prompt to prepend (useful with --print for scripting)
    #[arg(long = "system-prompt")]
    system_prompt: Option<String>,
    /// Maximum agentic turns (useful with --print to limit cost)
    #[arg(long = "max-turns")]
    max_turns: Option<usize>,
    /// Maximum session cost in USD before auto-exit (0 = unlimited)
    #[arg(long = "max-budget", default_value_t = 0.0)]
    max_budget: f64,
    /// Comma or space-separated list of tool names to allow (e.g. "Bash Edit Read")
    #[arg(long = "allowed-tools", num_args = 1..)]
    allowed_tools: Vec<String>,
    /// Comma or space-separated list of tool names to deny (e.g. "Bash Edit")
    #[arg(long = "disallowed-tools", num_args = 1..)]
    disallowed_tools: Vec<String>,
    /// Additional directories to allow tool access to
    #[arg(long = "add-dir", num_args = 1..)]
    add_dir: Vec<String>,
    /// Enable verbose output (overrides config setting)
    #[arg(long = "verbose")]
    verbose: bool,
    /// Load MCP server config from JSON file(s) or inline JSON strings
    #[arg(long = "mcp-config", num_args = 1..)]
    mcp_config: Vec<String>,
    /// Use a specific session ID (must be a valid UUID)
    #[arg(long = "session-id")]
    session_id: Option<String>,
    /// Set a display name for this session
    #[arg(short = 'n', long = "name")]
    session_name: Option<String>,
    /// Minimal mode: skip hooks, auto-memory, background prefetches.
    /// Only explicitly provided context (--system-prompt, --add-dir, --mcp-config) is used.
    #[arg(long = "bare")]
    bare: bool,
    /// Disable auto-loading of .astra/instructions.md project instructions
    #[arg(long = "no-instructions")]
    no_instructions: bool,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand, Debug)]
#[command(allow_external_subcommands = true)]
enum Command {
    /// Start the interactive REPL (default when no args given)
    Interactive,
    /// Start the HTTP API server
    Serve(ServeArgs),
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
    /// Session audit: astra audit list/show/turns/tools
    #[command(subcommand)]
    Audit(AuditCmd),
    /// Local session journal (offline): astra journal digest
    #[command(subcommand)]
    Journal(JournalCmd),
    /// Manage MCP servers: astra mcp add/remove/list/get
    #[command(subcommand)]
    Mcp(McpCmd),
    /// Manage settings: astra config list/get/set
    #[command(subcommand)]
    Config(ConfigCmd),
    /// Generate shell completion script
    Completion(CompletionArgs),
    /// Diagnose installation, config, and connectivity
    Doctor,
    /// Structured plan without the REPL (scripting / CI)
    #[command(subcommand)]
    Plan(PlanCmd),
    /// Direct message: astra "your question here"
    #[command(external_subcommand)]
    Message(Vec<String>),
}

/// Headless plan commands (no interactive `plan>` prompt).
#[derive(Subcommand, Debug)]
enum PlanCmd {
    /// Decompose a goal into a structured plan (same backend as `/plan enter`).
    Decompose {
        /// Goal text for decomposition
        #[arg(short = 'g', long)]
        goal: String,
        /// Print parsed plan as JSON on stdout
        #[arg(long, default_value_t = false)]
        json: bool,
        /// Suppress progress messages on stderr
        #[arg(short, long, default_value_t = false)]
        quiet: bool,
    },
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
struct ServeArgs {
    /// Address to listen on
    #[arg(long, default_value = "127.0.0.1")]
    host: String,
    /// Port to listen on
    #[arg(short, long, default_value_t = 8000)]
    port: u16,
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
    /// Permission mode: auto (approve all), prompt (interactive, default), deny (reject all writes)
    #[arg(long = "permission-mode")]
    permission_mode: Option<String>,
    /// Suppress spinner and progress output (result still printed)
    #[arg(long, default_value_t = false)]
    quiet: bool,
    /// Output result as JSON (implies --quiet)
    #[arg(long, default_value_t = false)]
    json: bool,
    /// Read message from stdin instead of -m
    #[arg(long, default_value_t = false)]
    stdin: bool,
    /// Disable ANSI colors in output
    #[arg(long, default_value_t = false)]
    no_color: bool,
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

#[derive(Subcommand, Debug)]
enum AuditCmd {
    /// List sessions with filters (status, model, since/until)
    List(AuditListArgs),
    /// Show audit summary for a session
    Show(AuditShowArgs),
    /// List turns in a session (paginated)
    Turns(AuditTurnsArgs),
    /// Show tool analytics for a session (or cross-session)
    Tools(AuditToolsArgs),
}

#[derive(Args, Debug)]
struct AuditListArgs {
    #[arg(long)]
    status: Option<String>,
    #[arg(long)]
    model: Option<String>,
    #[arg(long)]
    since: Option<String>,
    #[arg(long)]
    until: Option<String>,
    #[arg(long)]
    min_turns: Option<u32>,
    #[arg(long, default_value = "created")]
    sort: String,
    #[arg(long, default_value_t = 20)]
    limit: u32,
    #[arg(long, default_value_t = 1)]
    page: u32,
}

#[derive(Args, Debug)]
struct AuditShowArgs {
    session_id: String,
}

#[derive(Args, Debug)]
struct AuditTurnsArgs {
    session_id: String,
    /// Show detail for a specific turn number
    #[arg(long)]
    turn: Option<u32>,
    #[arg(long, default_value_t = 1)]
    page: u32,
    #[arg(long, default_value_t = 20)]
    per_page: u32,
}

#[derive(Args, Debug)]
struct AuditToolsArgs {
    /// Session ID; omit for cross-session tool analytics
    session_id: Option<String>,
    #[arg(long)]
    since: Option<String>,
    #[arg(long)]
    until: Option<String>,
}

#[derive(Subcommand, Debug)]
enum JournalCmd {
    /// Print a deterministic digest of a local session journal (JSON or text)
    Digest(JournalDigestArgs),
}

#[derive(Args, Debug)]
pub(crate) struct JournalDigestArgs {
    /// Session id, unique prefix, `last`, or omit for most recent local journal
    #[arg(value_name = "SESSION")]
    session_id: Option<String>,
    /// Same meaning as positional SESSION (positional wins if both are set)
    #[arg(long = "session", value_name = "SESSION")]
    session: Option<String>,
    /// Output format: json or text
    #[arg(long, default_value = "json")]
    format: String,
    /// all (default) or summary (smaller turn rows)
    #[arg(long)]
    focus: Option<String>,
}

#[derive(Subcommand, Debug)]
enum McpCmd {
    /// List configured MCP servers
    List(McpListArgs),
    /// Add a stdio MCP server
    Add(McpAddArgs),
    /// Add an MCP server from a JSON config string
    #[command(name = "add-json")]
    AddJson(McpAddJsonArgs),
    /// Remove an MCP server
    Remove(McpRemoveArgs),
    /// Show details of a configured MCP server
    Get(McpGetArgs),
}

#[derive(Args, Debug)]
struct McpListArgs {
    /// Config scope: project or user
    #[arg(short = 's', long, default_value = "project")]
    scope: String,
}

#[derive(Args, Debug)]
struct McpAddArgs {
    /// Server name
    name: String,
    /// Command to run
    command: String,
    /// Command arguments
    #[arg(trailing_var_arg = true)]
    args: Vec<String>,
    /// Config scope: project or user
    #[arg(short = 's', long, default_value = "project")]
    scope: String,
}

#[derive(Args, Debug)]
struct McpAddJsonArgs {
    /// Server name
    name: String,
    /// JSON configuration string
    json: String,
    /// Config scope: project or user
    #[arg(short = 's', long, default_value = "project")]
    scope: String,
}

#[derive(Args, Debug)]
struct McpRemoveArgs {
    /// Server name to remove
    name: String,
    /// Config scope: project or user
    #[arg(short = 's', long, default_value = "project")]
    scope: String,
}

#[derive(Args, Debug)]
struct McpGetArgs {
    /// Server name to inspect
    name: String,
}

#[derive(Args, Debug)]
struct CompletionArgs {
    /// Shell to generate completions for
    #[arg(value_enum)]
    shell: clap_complete::Shell,
}

#[derive(Subcommand, Debug)]
enum ConfigCmd {
    /// List all settings and their values
    List,
    /// Get a specific setting value
    Get(ConfigGetArgs),
    /// Set a setting value
    Set(ConfigSetArgs),
}

#[derive(Args, Debug)]
struct ConfigGetArgs {
    /// Setting key (e.g. default_model, verbose, api_url)
    key: String,
}

#[derive(Args, Debug)]
struct ConfigSetArgs {
    /// Setting key
    key: String,
    /// Setting value
    value: String,
}

// ═══════════════════════════════════════════════════════ Credentials ══════

// ══════════════════════════════════════════════════════════════════════════════

// ══════════════════════════════════════════════════════ SSE Streaming ════

pub(crate) type VerdictEvent = astra_runtime::turn::agentic_verdict_audit::AgenticVerdictAuditEvent;

/// Partial data rescued from `AgenticLoopState` when a turn fails.
/// Enables enriched error logging, failure learning, and post-mortem analysis.
#[derive(Debug, Default)]
pub(crate) struct PartialTurnData {
    pub tool_call_records: Vec<astra_services::session_journal::ToolCallRecord>,
    pub tools_used: Vec<String>,
    pub stall_events: Vec<(String, u32)>,
    pub verdict_events: Vec<VerdictEvent>,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub tool_calls_count: u32,
    #[allow(dead_code)]
    pub tool_health_export: Vec<astra_runtime::pipeline::persistence::ToolHealthEntry>,
    pub session_id: Option<String>,
    pub last_heavy_checkpoint: Option<astra_runtime::pipeline::step_protocol::StepCheckpoint>,
    /// Partial text the model generated before the turn was interrupted.
    /// Preserved in conversation history so the next turn has context.
    pub partial_text: String,
}

/// A turn failure that carries partial data for post-mortem analysis.
#[derive(Debug)]
pub(crate) struct TurnFailure {
    pub error: String,
    pub partial: PartialTurnData,
}

impl std::fmt::Display for TurnFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.error)
    }
}

#[derive(Debug)]
pub(crate) struct StreamResult {
    session_id: Option<String>,
    run_id: Option<String>,
    full_text: String,
    prompt_tokens: u64,
    completion_tokens: u64,
    cache_read_tokens: u64,
    cache_creation_tokens: u64,
    tool_calls_count: u32,
    /// Tool names selected for LLM (first turn selection report).
    tools_selected: Vec<String>,
    /// Skill names selected by the LLM during tool selection.
    selected_skills: Vec<String>,
    /// Tool names actually invoked by LLM across all turns.
    tools_used: Vec<String>,
    /// Per-tool-call audit records: name, ok, ms, error.
    tool_call_records: Vec<astra_services::session_journal::ToolCallRecord>,
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
    step_recorder_summary: Option<astra_runtime::pipeline::step_recorder::RecorderSummary>,
    /// Exported tool health entries from this turn's TurnGuard (for cross-session persistence).
    tool_health_export: Vec<astra_runtime::pipeline::persistence::ToolHealthEntry>,
    /// Last heavy checkpoint built during the agentic loop (for cloud persistence).
    last_heavy_checkpoint: Option<astra_runtime::pipeline::step_protocol::StepCheckpoint>,
    /// Time to first token in milliseconds.
    ttft_ms: Option<u64>,
    /// Context assembly time in milliseconds.
    context_ms: Option<u64>,
    /// Tool selection strategy used.
    selector_strategy: Option<String>,
    /// Tool selection time in milliseconds (subset of context_ms).
    selector_ms: Option<u64>,
    /// LLM tokens consumed by tool selector (0 if TF-IDF only).
    selector_tokens_in: u64,
    selector_tokens_out: u64,
    /// Memoria search time in milliseconds (subset of context_ms).
    memoria_ms: Option<u64>,
    /// First tool-selection confidence (0.0–1.0) from the agentic loop prep pass.
    selector_confidence: Option<f64>,
    /// Routing domain label for this user line (filled in REPL when writing the journal row).
    routing_domain_hint: Option<String>,
    /// Entity graph skipped learning: success with tools but no routing domain.
    entity_learn_skipped_no_domain: bool,
}

impl StreamResult {
    /// Filled by the REPL after the agentic loop returns (routing + entity-learn eligibility).
    pub(crate) fn set_repl_learning_journal_fields(
        &mut self,
        routing_domain_hint: Option<String>,
        entity_learn_skipped_no_domain: bool,
    ) {
        self.routing_domain_hint = routing_domain_hint;
        self.entity_learn_skipped_no_domain = entity_learn_skipped_no_domain;
    }
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

/// Active `/skill dev` session — name and directory are always set together.
#[derive(Clone, Debug)]
struct SkillDevState {
    name: String,
    dir: std::path::PathBuf,
}

// NOTE: ReplState is per-session and NOT shared across sessions. In future
// server/multi-session mode, ensure each session gets its own ReplState
// instance to prevent cross-session data leakage (permissions, history, tokens).
struct ReplState {
    session_id: Option<String>,
    run_id: Option<String>,
    /// Display name for this session (set via --name flag).
    session_name: Option<String>,
    model: Option<String>,
    turn: u32,
    last_response: Option<String>,
    /// Sticky task/thread summary used to anchor ultra-short follow-ups like
    /// "继续" even after history compaction prunes earlier turns.
    continuation_anchor: Option<String>,
    /// Session-level goal derived from the first substantive user message.
    /// Survives compaction and is injected alongside the continuation anchor.
    session_goal: Option<String>,
    explain: ExplainMode,
    verbose_mode: bool,
    history: Vec<(String, String)>, // (user_msg, assistant_msg)
    total_prompt_tokens: u64,
    total_completion_tokens: u64,
    total_cache_read_tokens: u64,
    total_cache_creation_tokens: u64,
    /// Per-turn cost accumulator (sum of all turns in this session).
    total_session_cost: f64,
    /// Maximum session cost in USD before auto-exit (0.0 = unlimited).
    max_budget_limit: f64,
    /// Cached pricing data for the active model (used by /cost).
    cached_pricing: astra_services::models::PricingData,
    skill_dev: Option<SkillDevState>,
    active_system_skills: Vec<prompts::SystemSkill>,
    context_budget: prompts::ContextBudget,
    journal: Option<session_journal::JournalWriter>,
    /// Tools used in the last turn — fed into selection for recency boost.
    recent_tools: Vec<String>,
    /// Session-persistent permission manager — "always"/"skip" survives across turns.
    perm_manager: PermissionManager,
    /// User ID for event ingestion attribution.
    ingestion_user_id: Option<String>,
    /// Matrix pool + journal ingestion + sync orchestrator (None if MatrixOne unavailable).
    matrix_runtime: Option<std::sync::Arc<astra_runtime::MatrixCloudRuntime>>,
    /// Learning snapshot restored from cloud (to be merged into learning modules).
    learning_snapshot: Option<String>,
    /// Local task service for /task commands.
    task_service: Option<std::sync::Arc<astra_services::LocalTaskService>>,
    /// Cross-session tool health data for error budget persistence.
    tool_health_entries: Vec<astra_runtime::pipeline::persistence::ToolHealthEntry>,
    /// Last successfully synced tool health snapshot, used to compute deltas.
    synced_tool_health_entries: Vec<astra_runtime::pipeline::persistence::ToolHealthEntry>,
    /// Plan-only chat (`/plan on`): normal REPL turns omit edge tools; model plans without executing.
    chat_plan_only: bool,
    /// Plan Mode state — when Some, REPL is in interactive plan editing mode.
    plan_mode: Option<plan_decompose::PlanModeState>,
    /// Plan being auto-executed — subtasks sent sequentially through chat.
    executing_plan: Option<astra_services::task_orchestrator::TaskPlan>,
    /// Configuration for current plan execution (step-by-step, auto-execute, etc.).
    plan_execution_config: Option<plan_decompose::PlanExecutionConfig>,
    /// Goal text for the executing plan (for summary generation).
    executing_plan_goal: Option<String>,
    /// Number of parallel execution rounds completed (for summary).
    plan_execution_rounds: usize,
    /// ID of the currently-executing plan subtask (set during plan execution,
    /// read by apply_turn_success to tag journal events).
    current_plan_subtask_id: Option<String>,
    /// Whether the last chat turn was interrupted by Ctrl+C (used by plan auto-execution).
    last_turn_interrupted: bool,
    /// Cloud learning snapshot version for optimistic locking.
    /// Set by try_cloud_pull, used by try_cloud_push to prevent concurrent overwrites.
    cloud_learning_version: Option<i64>,
    /// Last turn's journal event — for /turn command display.
    last_turn_event: Option<session_journal::JournalEvent>,
    /// Shared pattern library reference for /learn command.
    pattern_library:
        Option<std::sync::Arc<std::sync::Mutex<astra_runtime::pipeline::pattern::PatternLibrary>>>,
    /// Shared entity graph (learning feedback loop + post-login cloud pull).
    entity_graph:
        Option<std::sync::Arc<std::sync::Mutex<astra_runtime::pipeline::entity::EntityGraph>>>,
    /// Shared calibrator (learning feedback loop + post-login cloud pull).
    calibrator: Option<
        std::sync::Arc<
            std::sync::Mutex<astra_runtime::pipeline::calibration::ProgressiveCalibrator>,
        >,
    >,
    /// Unified skill registry (single source of truth for all skill resolution).
    unified_skill_registry: std::sync::Arc<astra_runtime::skills::UnifiedSkillRegistry>,
    /// Session-scoped skill quality tracker for learning loop.
    skill_quality_tracker: astra_runtime::skills::quality::SkillQualityTracker,
    /// Session-scoped skill surfacing config for dynamic tuning.
    skill_search: astra_core::SkillSearchSettings,
    /// Skill auto-improvement tracker — detects user corrections and proposes SKILL.md rewrites.
    skill_improvement_tracker: astra_runtime::skills::improvement::ImprovementTracker,
    /// Skills pinned by the user — always included in budget (never truncated).
    pinned_skills: std::collections::HashSet<String>,
    /// Skills surfaced by `discover_skills` during this REPL session.
    discovered_skills: std::collections::HashSet<String>,
    mcp_manager: std::sync::Arc<tokio::sync::RwLock<mcp_client::McpClientManager>>,
    /// Skill classification cache for LLM-based skill detection.
    #[allow(dead_code)]
    skill_classification_cache: skill_instructions::SkillClassificationCache,
    /// Active durable-task contract for plan execution verification.
    durable_task_state: Option<durable_bridge::DurableTaskState>,
    /// Last delivery report — kept after plan completion so `/report` works post-plan.
    last_delivery_report: Option<astra_services::durable_task::TaskDeliveryReport>,
    /// Stacked operator notes while plan execution is paused (`correct` / `note` at ⏸>).
    plan_execution_corrections: Vec<String>,
    /// Delegation engine for multi-agent coordination.
    /// Constructed at REPL startup with a real `CliDelegateSubRunExecutor` when
    /// the user is authenticated. Falls back to stub creation during plan execution
    /// if not already initialized.
    delegation_engine:
        Option<std::sync::Arc<astra_runtime::server::delegation_engine::DelegationEngine>>,
    /// Team coordination registry for multi-agent team patterns.
    team_registry: slash_team::TeamRegistry,
    /// Shared team persistence service (in-memory or MatrixOne-backed).
    /// Used for execution history and snapshot persistence.
    team_store: std::sync::Arc<dyn astra_services::team_persistence::TeamPersistenceService>,
    /// Handle for communicating with the plan executor.
    /// When Some, a plan executor is alive (either actively running or paused
    /// waiting for Resume/Cancel).
    plan_handle: Option<plan_executor::PlanExecutorHandle>,

    /// `agent_tasks` row created when plan execution starts (`go`); used to sync
    /// `/task list` with the background executor (progress + terminal status).
    plan_run_task_id: Option<String>,
    /// Latest `(progress_pct, items_done, items_total)` from [`PlanUpdate::PlanProgress`].
    plan_run_task_last_progress: Option<(u32, u32, u32)>,
    /// Set when the executor exits with [`PlanUpdate::PlanError`].
    plan_run_task_last_error: Option<String>,

    /// Set by `handle_plan_command(Resume)` so the main loop re-enters
    /// the blocking plan monitor after `handle_plan_mode_input` returns.
    plan_resume_pending: bool,

    /// When Some, a plan-executor tool is waiting for user approval.
    /// In blocking mode this is handled inline; kept for edge-case fallback.
    pending_approval: Option<tokio::sync::oneshot::Sender<bool>>,
    /// True while plan display is in the middle of printing streaming LLM tokens.
    /// Used to insert a newline before the next non-token event.
    plan_in_token_stream: bool,
    /// Streaming markdown renderer for plan execution token output.
    plan_md_renderer: Option<streaming_md::StreamingMarkdown>,
    /// Thinking preview pane for plan execution (reasoning visibility).
    plan_thinking_pane: Option<effects::ThinkingPreviewPane>,
    /// Project-level instructions loaded from `.astra/instructions.md`.
    /// Injected into every turn's effective message as `<project_instructions>`.
    project_instructions: Option<String>,
    /// Shared messaging metrics (populated when delegation is active).
    messaging_metrics: Option<std::sync::Arc<astra_runtime::messaging::MessagingMetrics>>,
    /// Shared dead letter queue (populated when delegation is active).
    dead_letter_queue:
        Option<std::sync::Arc<astra_runtime::messaging::dead_letter::DeadLetterQueue>>,
    /// Dynamic agent spawner for runtime agent creation.
    agent_spawner: Option<std::sync::Arc<astra_runtime::orchestration::DynamicAgentSpawner>>,
    /// Persistent top-level mailbox so spawned agents can reply across turns.
    root_mailbox: Option<astra_runtime::messaging::router::AgentMailbox>,
    /// Replies received while the REPL is idle at the prompt. Flushed only at safe redraw points.
    pending_idle_agent_messages: Vec<std::sync::Arc<astra_runtime::messaging::AgentMessage>>,
}

impl Default for ReplState {
    fn default() -> Self {
        Self {
            session_id: None,
            run_id: None,
            session_name: None,
            model: None,
            turn: 0,
            last_response: None,
            continuation_anchor: None,
            session_goal: None,
            explain: ExplainMode::Off,
            verbose_mode: true,
            history: Vec::new(),
            total_prompt_tokens: 0,
            total_completion_tokens: 0,
            total_cache_read_tokens: 0,
            total_cache_creation_tokens: 0,
            total_session_cost: 0.0,
            max_budget_limit: std::env::var("MO_MAX_BUDGET")
                .ok()
                .and_then(|v| v.parse::<f64>().ok())
                .unwrap_or(0.0),
            cached_pricing: Default::default(),
            skill_dev: None,
            active_system_skills: Vec::new(),
            context_budget: prompts::ContextBudget::default(),
            journal: None,
            recent_tools: Vec::new(),
            perm_manager: PermissionManager::with_project(
                std::env::var("ASTRA_AUTO_APPROVE")
                    .map(|v| v == "1")
                    .unwrap_or(false),
                &std::env::current_dir().unwrap_or_default(),
            ),
            ingestion_user_id: None,
            matrix_runtime: None,
            learning_snapshot: None,
            task_service: None,
            tool_health_entries: Vec::new(),
            synced_tool_health_entries: Vec::new(),
            chat_plan_only: false,
            plan_mode: None,
            executing_plan: None,
            plan_execution_config: None,
            executing_plan_goal: None,
            plan_execution_rounds: 0,
            current_plan_subtask_id: None,
            last_turn_interrupted: false,
            cloud_learning_version: None,
            last_turn_event: None,
            pattern_library: None,
            entity_graph: None,
            calibrator: None,
            unified_skill_registry: astra_runtime::skills::default_unified_registry().clone(),
            skill_quality_tracker: astra_runtime::skills::quality::SkillQualityTracker::new(),
            skill_search: astra_core::SkillSearchSettings::default(),
            skill_improvement_tracker: astra_runtime::skills::improvement::ImprovementTracker::new(
            ),
            pinned_skills: std::collections::HashSet::new(),
            discovered_skills: std::collections::HashSet::new(),
            mcp_manager: std::sync::Arc::new(tokio::sync::RwLock::new(
                mcp_client::McpClientManager::new(),
            )),
            skill_classification_cache: skill_instructions::SkillClassificationCache::default(),
            durable_task_state: None,
            last_delivery_report: None,
            plan_execution_corrections: Vec::new(),
            delegation_engine: None,
            team_registry: slash_team::TeamRegistry::new(),
            team_store: std::sync::Arc::new(
                astra_services::team_persistence::InMemoryTeamStore::new(),
            ),
            plan_handle: None,
            plan_run_task_id: None,
            plan_run_task_last_progress: None,
            plan_run_task_last_error: None,
            plan_resume_pending: false,
            pending_approval: None,
            plan_in_token_stream: false,
            plan_md_renderer: None,
            plan_thinking_pane: None,
            project_instructions: None,
            // Create shared messaging infrastructure eagerly so /messaging always has data
            messaging_metrics: Some(std::sync::Arc::new(
                astra_runtime::messaging::MessagingMetrics::new(),
            )),
            dead_letter_queue: Some(std::sync::Arc::new(
                astra_runtime::messaging::dead_letter::DeadLetterQueue::new(),
            )),
            agent_spawner: None, // Created lazily when spawn_agent is first used
            root_mailbox: None,
            pending_idle_agent_messages: Vec::new(),
        }
    }
}

// ═════════════════════════════════════════════════════════ ReplHelper ════

// ═════════════════════════════════════════════════════════ Clipboard ══════

// ═══════════════════════════════════════════════════════════ Resume ═══════

async fn handle_resume_command(arg: &str, profile: Option<&str>, state: &mut ReplState) {
    use astra_services::session_restore::{HybridRestoreService, SessionRestoreService};

    let user_id = state.ingestion_user_id.as_deref().unwrap_or("local");
    let svc = match &state.matrix_runtime {
        Some(mc) => HybridRestoreService::new(mc.shared_pool().get().clone()),
        None => HybridRestoreService::local_only(),
    };

    // If no session_id given, list and let user pick
    let effective_arg;
    if arg.is_empty() {
        // Merge cloud + local sessions, deduplicate, sort by recency
        let cloud_sessions = svc
            .list_resumable_sessions(user_id)
            .await
            .unwrap_or_default();
        let local_ids = session_journal::list_sessions_by_time(20).unwrap_or_default();

        // Build merged map: session_id → RestoredSession (cloud wins on metadata)
        let mut merged: std::collections::HashMap<
            String,
            astra_services::session_restore::RestoredSession,
        > = std::collections::HashMap::new();

        // Insert local sessions first (lower priority)
        for sid in &local_ids {
            merged.entry(sid.clone()).or_insert_with(|| {
                astra_services::session_restore::RestoredSession {
                    session_id: sid.clone(),
                    turn_count: session_journal::count_turns(sid),
                    last_status: "local".to_string(),
                    ..Default::default()
                }
            });
        }

        // Cloud sessions override local (richer metadata: title, turn_count, status)
        for s in cloud_sessions {
            merged.insert(s.session_id.clone(), s);
        }

        // Sort by local file order (newest first), cloud-only sessions appended at front
        let mut result: Vec<_> = Vec::new();
        let mut seen = std::collections::HashSet::new();

        // Local order first (already sorted by mtime)
        for sid in &local_ids {
            if let Some(s) = merged.remove(sid) {
                seen.insert(sid.clone());
                result.push(s);
            }
        }
        // Remaining cloud-only sessions at the front (they're newer if not local)
        let mut cloud_only: Vec<_> = merged.into_values().collect();
        cloud_only.sort_by(|a, b| b.turn_count.cmp(&a.turn_count));
        result.splice(0..0, cloud_only);

        // Filter out empty sessions (0 turns = nothing to resume)
        result.retain(|s| s.turn_count > 0);

        if result.is_empty() {
            eprintln!("{}", "  No resumable sessions found.".dim());
            return;
        }

        let sessions = &result[..result.len().min(10)];

        // Enrich with local metadata (workspace + journal peek) — fast, no DB
        struct SessionDisplay {
            idx: usize,
            session_id: String,
            title: Option<String>,
            first_prompt: Option<String>,
            turn_count: u32,
            model: Option<String>,
            cwd_short: Option<String>,
            git_branch: Option<String>,
            source: String,
            has_plan: bool,
            age: String,
        }

        let mut items: Vec<SessionDisplay> = Vec::new();
        for (i, s) in sessions.iter().enumerate() {
            let peek = session_journal::peek_session_meta(&s.session_id);
            let ws = astra_services::session_workspace::read_workspace(&s.session_id).ok();

            // Title: cloud title > workspace summary > first prompt preview
            let title = s
                .title
                .clone()
                .or_else(|| ws.as_ref().and_then(|w| w.summary.clone()))
                .or_else(|| peek.as_ref().and_then(|p| p.first_prompt.clone()));

            let first_prompt = peek.as_ref().and_then(|p| p.first_prompt.clone());

            // Model: cloud > workspace > journal peek
            let model = s
                .model
                .clone()
                .or_else(|| ws.as_ref().map(|w| w.model.clone()))
                .or_else(|| peek.as_ref().and_then(|p| p.model.clone()));

            // cwd: shorten to last 2 path components
            let cwd_short = ws.as_ref().map(|w| {
                let parts: Vec<&str> = w.cwd.split('/').filter(|s| !s.is_empty()).collect();
                if parts.len() <= 2 {
                    w.cwd.clone()
                } else {
                    format!("…/{}", parts[parts.len() - 2..].join("/"))
                }
            });

            let git_branch = s
                .git_branch
                .clone()
                .or_else(|| ws.as_ref().and_then(|w| w.git_branch.clone()));

            let source = if s.restored_from_cloud {
                "☁".to_string()
            } else if s.last_status == "local" {
                "⊙".to_string()
            } else {
                s.last_status.clone()
            };

            let has_plan = ws.as_ref().is_some_and(|w| w.executing_plan_json.is_some());

            // Age: from workspace or journal timestamp
            let age = ws
                .as_ref()
                .map(|w| &w.updated_at)
                .or_else(|| peek.as_ref().and_then(|p| p.created_at.as_ref()))
                .and_then(|ts| chrono::DateTime::parse_from_rfc3339(ts).ok())
                .map(|dt| {
                    let dur = chrono::Utc::now().signed_duration_since(dt);
                    if dur.num_minutes() < 60 {
                        format!("{}m ago", dur.num_minutes())
                    } else if dur.num_hours() < 24 {
                        format!("{}h ago", dur.num_hours())
                    } else {
                        format!("{}d ago", dur.num_days())
                    }
                })
                .unwrap_or_default();

            items.push(SessionDisplay {
                idx: i + 1,
                session_id: s.session_id.clone(),
                title,
                first_prompt,
                turn_count: s.turn_count,
                model,
                cwd_short,
                git_branch,
                source,
                has_plan,
                age,
            });
        }

        eprintln!(
            "\n{}",
            "─── Resumable Sessions ──────────────────────────".bold()
        );
        for s in &items {
            // Line 1: [N]  title or first prompt  (age)
            let display_text = s
                .title
                .as_deref()
                .or(s.first_prompt.as_deref())
                .unwrap_or("(no prompt)");
            let display_truncated: String = display_text.chars().take(60).collect();
            let plan_badge = if s.has_plan { " 📋" } else { "" };
            eprintln!(
                "  {}  {}{}  {}",
                format!("[{}]", s.idx).cyan().bold(),
                display_truncated,
                plan_badge,
                s.age.as_str().dim(),
            );
            // Line 2: context details
            let short_id = &s.session_id[..8.min(s.session_id.len())];
            let model_str = s.model.as_deref().unwrap_or("?");
            let branch_str = s
                .git_branch
                .as_deref()
                .map(|b| format!(" {b}"))
                .unwrap_or_default();
            let cwd_str = s.cwd_short.as_deref().unwrap_or("");
            eprintln!(
                "      {} {} {} turns · {}{} {}",
                s.source.as_str().dim(),
                short_id.dim(),
                s.turn_count,
                model_str.dim(),
                branch_str.dim(),
                cwd_str.dim(),
            );
        }
        eprintln!();
        eprint!("  {} ", "Select (number or Enter to cancel):".bold());
        std::io::Write::flush(&mut std::io::stderr()).ok();
        let mut input = String::new();
        if std::io::stdin().read_line(&mut input).is_ok() {
            if let Ok(n) = input.trim().parse::<usize>() {
                if n >= 1 && n <= sessions.len() {
                    effective_arg = sessions[n - 1].session_id.clone();
                } else {
                    eprintln!("{}", "  Cancelled.".dim());
                    return;
                }
            } else {
                eprintln!("{}", "  Cancelled.".dim());
                return;
            }
        } else {
            return;
        }
    } else {
        effective_arg = arg.to_string();
    }
    let arg = effective_arg.as_str();

    // Resolve prefix via local journal first
    let session_id = match session_journal::resolve_session_id(arg) {
        Ok(resolved) => {
            if resolved != arg {
                eprintln!(
                    "  {} Resolved {} → {}",
                    theme::icon_ok(),
                    arg.cyan(),
                    resolved.as_str().cyan()
                );
            }
            resolved
        }
        Err(_) => arg.to_string(),
    };

    // Restore session
    match svc.restore_session(&session_id).await {
        Ok(Some(restored)) => {
            // Issue 1: Verify session belongs to current user
            // For cloud restore, the session should already have user_id check done in DB query
            // For local restore, we verify the session exists in user's journal
            if !restored.restored_from_cloud {
                // Local restore: verify user owns this session by checking journal exists
                if session_journal::read_journal(&session_id).is_err() {
                    eprintln!(
                        "{}",
                        format!(
                            "  {} Session {} not found or not owned by user",
                            theme::icon_err(),
                            arg
                        )
                        .red()
                    );
                    return;
                }
            }

            // Apply restored state
            state.session_id = Some(restored.session_id.clone());
            state.turn = restored.turn_count;
            state.total_prompt_tokens = restored.total_tokens_in;
            state.total_completion_tokens = restored.total_tokens_out;
            state.recent_tools = restored.recent_tools;

            // Merge step checkpoint data when the on-disk checkpoint matches current protocol.
            if let Ok(Some(step_restored)) =
                astra_runtime::pipeline::step_restore::restore_session(&restored.session_id)
            {
                let summary =
                    astra_runtime::pipeline::step_restore::restore_summary(&step_restored);
                // Merge blocked tools from checkpoint into health entries
                for tool in &step_restored.blocked_tools {
                    if !state.tool_health_entries.iter().any(|e| e.name == *tool) {
                        state.tool_health_entries.push(
                            astra_runtime::pipeline::persistence::ToolHealthEntry {
                                name: tool.clone(),
                                total_calls: 3,
                                total_failures: 3,
                                failure_rate: 1.0,
                                last_updated_epoch: 0, // synthetic — will be overridden by real data
                            },
                        );
                    }
                }
                if state.recent_tools.is_empty() {
                    state.recent_tools = step_restored.recent_tools;
                }
                eprintln!("  {} {}", "↻".cyan(), summary.dim());
            } else if let Ok(Some(heavy)) =
                astra_runtime::pipeline::step_checkpoint::read_latest_heavy_checkpoint(
                    &restored.session_id,
                )
            {
                // Fallback to raw local checkpoint if step_restore fails (e.g., version mismatch)
                if state.recent_tools.is_empty() {
                    state.recent_tools = heavy.recent_tools;
                }
            } else if let Some(ref mc) = state.matrix_runtime {
                // Cloud fallback: pull heavy checkpoint from MatrixOne
                // (different device, local files not available)
                let pool = mc.shared_pool().get();
                match astra_services::session_restore::pull_step_checkpoint_from_cloud(
                    pool,
                    &restored.session_id,
                )
                .await
                {
                    Ok(Some(state_json)) => {
                        match serde_json::from_str::<
                            astra_runtime::pipeline::step_protocol::StepCheckpoint,
                        >(&state_json)
                        {
                            Ok(astra_runtime::pipeline::step_protocol::StepCheckpoint::Heavy(
                                heavy,
                            )) => {
                                for tool in &heavy.blocked_tools {
                                    if !state.tool_health_entries.iter().any(|e| e.name == *tool) {
                                        state.tool_health_entries.push(
                                            astra_runtime::pipeline::persistence::ToolHealthEntry {
                                                name: tool.clone(),
                                                total_calls: 3,
                                                total_failures: 3,
                                                failure_rate: 1.0,
                                                last_updated_epoch: 0,
                                            },
                                        );
                                    }
                                }
                                if state.recent_tools.is_empty() {
                                    state.recent_tools = heavy.recent_tools;
                                }
                                // Restore conversation history from cloud checkpoint
                                if state.history.is_empty() && !heavy.messages.is_empty() {
                                    // Extract user/assistant pairs from messages for history
                                    let mut pairs = Vec::new();
                                    let mut last_user = String::new();
                                    for msg in &heavy.messages {
                                        let role =
                                            msg.get("role").and_then(|r| r.as_str()).unwrap_or("");
                                        let content = msg
                                            .get("content")
                                            .and_then(|c| c.as_str())
                                            .unwrap_or("");
                                        match role {
                                            "user" => last_user = content.to_string(),
                                            "assistant" if !last_user.is_empty() => {
                                                pairs
                                                    .push((last_user.clone(), content.to_string()));
                                                last_user.clear();
                                            }
                                            _ => {}
                                        }
                                    }
                                    if !pairs.is_empty() {
                                        state.history = pairs;
                                    }
                                }
                                eprintln!("  {} Restored step checkpoint from cloud", "☁".cyan());
                            }
                            Ok(_) => {} // Light checkpoint — less useful, skip
                            Err(e) => {
                                eprintln!(
                                    "  {} Cloud checkpoint corrupted, skipping",
                                    theme::icon_warn()
                                );
                                eprintln!("{}", format!("     ({e})").dim());
                            }
                        }
                    }
                    Ok(None) => {} // No cloud checkpoint available
                    Err(e) => {
                        eprintln!("  {} Cloud checkpoint unavailable", theme::icon_warn());
                        eprintln!("{}", format!("     ({e})").dim());
                    }
                }
            }

            if let Some(ref m) = restored.model {
                state.model = Some(m.clone());
                state.cached_pricing = fallback_pricing(m);
            }

            // Store learning snapshot for merge after handler returns
            // (pipeline modules are only accessible in run_chat_repl)
            if let Some(ref learning_json) = restored.learning_snapshot_json
                && !learning_json.is_empty()
            {
                state.learning_snapshot = Some(learning_json.clone());
            }

            // Issue 3: Restore conversation history from local journal
            // restore_history_from_journal already handles session segmentation (only reads after latest session_start)
            state.history = repl_runtime::restore_history_from_journal(&session_id);

            // Restore last turn event for /turn command
            if let Ok(events) = session_journal::read_journal(&session_id) {
                state.last_turn_event = events
                    .iter()
                    .rev()
                    .find(|e| e.event_type == session_journal::JournalEventType::Turn)
                    .cloned();
            }

            // Restore plan execution state from workspace snapshot
            if let Some(ref json) = restored.executing_plan_json {
                state.executing_plan = serde_json::from_str(json).ok();
            }
            if let Some(ref goal) = restored.plan_goal {
                state.executing_plan_goal = Some(goal.clone());
            }
            if let Some(ref json) = restored.plan_config_json {
                state.plan_execution_config = serde_json::from_str(json).ok();
            }
            state.plan_execution_rounds = restored.plan_execution_rounds;

            // Restore operator corrections stacked during plan pause
            state.plan_execution_corrections = restored.plan_corrections;

            // Restore durable task contract if present
            if let Some(ref json) = restored.contract_json
                && let Ok(contract) = serde_json::from_str::<astra_services::TaskContract>(json)
            {
                let work_dir = std::env::current_dir().unwrap_or_default();
                let ingestion_sender = state
                    .matrix_runtime
                    .as_ref()
                    .and_then(|mc| mc.clone_ingestion_sender());
                let cloud_judge = state
                    .matrix_runtime
                    .as_ref()
                    .and_then(|mc| mc.create_cloud_llm_judge())
                    .map(|j| {
                        std::sync::Arc::new(j) as std::sync::Arc<dyn astra_services::LlmJudge>
                    });
                let learning = build_learning_bridge(state);

                let lifecycle = if let Some(pool) = state
                    .matrix_runtime
                    .as_ref()
                    .map(|mc| mc.shared_pool().get().clone())
                {
                    durable_bridge::create_cloud_lifecycle_full(
                        pool,
                        &work_dir,
                        ingestion_sender,
                        Some(&session_id),
                        state.ingestion_user_id.as_deref(),
                        cloud_judge,
                        learning,
                        None, // no server proxy during session restore
                    )
                } else {
                    let session_dir =
                        astra_services::session_workspace::workspace_dir_for(&session_id);
                    durable_bridge::create_local_lifecycle_full(
                        &session_dir,
                        &work_dir,
                        ingestion_sender,
                        Some(&session_id),
                        state.ingestion_user_id.as_deref(),
                        cloud_judge,
                        learning,
                        None, // no server proxy during session restore
                    )
                };
                state.durable_task_state = Some(durable_bridge::DurableTaskState {
                    contract,
                    lifecycle,
                    last_report: None,
                });
            }

            // Re-initialize journal for the resumed session
            repl_turn::initialize_journal_pub(state, &session_id);
            repl_turn::persist_last_session_id(profile, &session_id);

            let source = if restored.restored_from_cloud {
                "cloud"
            } else {
                "local"
            };
            eprintln!(
                "  {} Resumed session {} ({}, {} turns, {} checkpoints)",
                theme::icon_ok(),
                &session_id[..8.min(session_id.len())].cyan(),
                source,
                restored.turn_count,
                restored.checkpoint_count,
            );

            // Show paused plan banner
            if let Some(ref plan) = state.executing_plan {
                let done = plan.items_done();
                let total = plan.subtasks.len();
                let pct = plan.progress_pct();
                eprintln!(
                    "  {} Paused plan restored: {}/{} subtasks done ({}%)",
                    "📋".cyan(),
                    done,
                    total,
                    pct,
                );
                if let Some(ref goal) = state.executing_plan_goal {
                    eprintln!("    {} {}", "Goal:".dim(), goal.as_str().dim());
                }
                eprintln!(
                    "    {}",
                    "Say continue / resume / next / go to pick up; correct … / rewind N to adjust; slash lines keep the plan; any other line abandons it."
                        .dim()
                );
            }
        }
        Ok(None) => {
            // Service didn't find workspace/cloud data, but journal may exist.
            // Don't reuse the old session_id — server doesn't know it.
            // Restore history as context for a new session.
            match session_journal::read_journal(&session_id) {
                Ok(events) if !events.is_empty() => {
                    let turn_count = events
                        .iter()
                        .filter(|e| e.event_type == session_journal::JournalEventType::Turn)
                        .count() as u32;
                    // Restore last turn event for /turn command
                    state.last_turn_event = events
                        .iter()
                        .rev()
                        .find(|e| e.event_type == session_journal::JournalEventType::Turn)
                        .cloned();
                    state.session_id = None; // new session on next message
                    state.turn = turn_count;
                    state.history = repl_runtime::restore_history_from_journal(&session_id);
                    eprintln!(
                        "  {} Restored {} turns from journal {}. Next message starts a new session.",
                        theme::icon_ok(),
                        turn_count,
                        &session_id[..8.min(session_id.len())].cyan(),
                    );
                }
                _ => {
                    eprintln!("{}", format!("  Session '{arg}' not found.").yellow());
                    eprintln!("{}", "  Use /resume to see available sessions.".dim());
                }
            }
        }
        Err(e) => {
            let hint = if e.to_string().contains("not found") {
                "Use /resume to see available sessions."
            } else {
                "Check connection with /diagnostics, or try a different session."
            };
            eprintln!(
                "  {} {}",
                theme::icon_err(),
                format!("Resume failed: {e}").red()
            );
            eprintln!("{}", format!("  {hint}").dim());
        }
    }
}

// ═══════════════════════════════════════════════════════ Stats ════════════

fn handle_stats_command(arg: &str, state: &ReplState) {
    use astra_services::session_analytics;

    match arg {
        "history" => {
            // Show stats across recent sessions
            let sessions = match session_journal::list_sessions() {
                Ok(s) => s,
                Err(e) => {
                    eprintln!(
                        "{}",
                        format!("  ⚠ Could not read session history: {e}").yellow()
                    );
                    return;
                }
            };
            if sessions.is_empty() {
                eprintln!("{}", "  No sessions found.".dim());
                return;
            }
            let recent: Vec<_> = sessions.into_iter().take(10).collect();
            let mut all_stats = Vec::new();
            for sid in &recent {
                if let Ok(events) = session_journal::read_journal(sid) {
                    all_stats.push(session_analytics::compute_session_stats(sid, &events));
                }
            }
            if all_stats.is_empty() {
                eprintln!("{}", "  No session data.".dim());
                return;
            }
            eprintln!(
                "\n{}",
                "─── Recent Sessions ─────────────────────────────".bold()
            );
            for s in &all_stats {
                let short = &s.session_id[..8.min(s.session_id.len())];
                let model = s.model.as_deref().unwrap_or("?");
                eprintln!(
                    "  {} {:>3} turns  {:>6}+{:<6} tok  {:>3} tools  {} err  {}",
                    short.cyan(),
                    s.turn_count,
                    s.total_tokens_in,
                    s.total_tokens_out,
                    s.total_tool_calls,
                    s.error_count,
                    model.dim(),
                );
            }
            let agg = session_analytics::aggregate_stats(&all_stats);
            eprintln!(
                "\n  {} {} sessions, {} turns, {}+{} tokens, {:.1}% tool errors",
                "Summary:".bold(),
                agg.session_count,
                agg.total_turns,
                agg.total_tokens_in,
                agg.total_tokens_out,
                agg.overall_tool_error_rate * 100.0,
            );
            eprintln!();
        }
        _ => {
            // Show current session stats
            let sid = match &state.session_id {
                Some(s) => s.clone(),
                None => {
                    eprintln!("{}", "  No active session. Use /stats history.".dim());
                    return;
                }
            };
            let events = session_journal::read_journal(&sid).unwrap_or_default();
            let stats = session_analytics::compute_session_stats(&sid, &events);

            eprintln!(
                "\n{}",
                "─── Session Stats ───────────────────────────────".bold()
            );
            eprintln!(
                "  {:<14} {}",
                "session:".dim(),
                sid[..8.min(sid.len())].cyan()
            );
            if let Some(ref m) = stats.model {
                eprintln!("  {:<14} {}", "model:".dim(), m.as_str().cyan());
            }
            eprintln!("  {:<14} {}", "turns:".dim(), stats.turn_count);
            eprintln!(
                "  {:<14} {} in + {} out",
                "tokens:".dim(),
                stats.total_tokens_in,
                stats.total_tokens_out
            );
            eprintln!(
                "  {:<14} {:.1}s ({:.0}ms/turn)",
                "duration:".dim(),
                stats.total_duration_ms as f64 / 1000.0,
                stats.avg_duration_ms as f64
            );
            eprintln!(
                "  {:<14} {} ({} failed, {:.1}% error rate)",
                "tool calls:".dim(),
                stats.total_tool_calls,
                stats.failed_tool_calls,
                stats.tool_error_rate * 100.0
            );
            if !stats.unique_tools.is_empty() {
                eprintln!(
                    "  {:<14} {}",
                    "tools used:".dim(),
                    stats.unique_tools.join(", ")
                );
            }
            if stats.error_count > 0 || stats.stall_count > 0 {
                eprintln!(
                    "  {:<14} {} errors, {} stalls",
                    "issues:".dim(),
                    stats.error_count,
                    stats.stall_count
                );
            }
            if stats.checkpoint_count > 0 {
                eprintln!("  {:<14} {}", "checkpoints:".dim(), stats.checkpoint_count);
            }
            eprintln!();
        }
    }
}

// ═══════════════════════════════════════════════ Cost Tracking ═════════════

/// Per-turn cost record for granular cost breakdown.
#[derive(Clone, Debug)]
struct TurnCostEntry {
    turn: u32,
    prompt_tokens: u64,
    completion_tokens: u64,
    model: String,
}

/// Handle the `/cost` slash command — display per-session API cost estimates.
///
/// Subcommands:
///   /cost           — current session summary
///   /cost detail    — per-turn breakdown
///   /cost history   — across recent sessions
fn handle_cost_command(arg: &str, state: &ReplState) {
    use astra_services::session_analytics;

    match arg {
        "detail" | "breakdown" => {
            // Per-turn breakdown from journal
            let sid = match &state.session_id {
                Some(s) => s.clone(),
                None => {
                    eprintln!("{}", "  No active session.".dim());
                    return;
                }
            };
            let events = session_journal::read_journal(&sid).unwrap_or_default();
            let pricing = &state.cached_pricing;

            eprintln!(
                "\n{}",
                "─── Per-Turn Cost Breakdown ─────────────────────".bold()
            );
            if let Some(ref m) = state.model {
                eprintln!("  {:<14} {}", "model:".dim(), m.as_str().cyan());
            }
            eprintln!(
                "  {:<14} ${:.4}/1k prompt, ${:.4}/1k completion",
                "rates:".dim(),
                pricing.prompt,
                pricing.completion
            );
            eprintln!();

            let mut total_in = 0u64;
            let mut total_out = 0u64;
            let mut total_cost = 0.0f64;
            let mut turn_num = 0u32;

            for ev in &events {
                if ev.event_type == session_journal::JournalEventType::Turn {
                    turn_num += 1;
                    let p_tok = ev.tokens_in.unwrap_or(0);
                    let c_tok = ev.tokens_out.unwrap_or(0);
                    let cr = ev.cache_read_tokens.unwrap_or(0);
                    let cw = ev.cache_creation_tokens.unwrap_or(0);
                    let cost = cost_for_tokens(p_tok, c_tok, cr, cw, pricing);
                    total_in += p_tok;
                    total_out += c_tok;
                    total_cost += cost;

                    let cache_info = if cr > 0 {
                        let pct = cr as f64 / (p_tok + cr).max(1) as f64 * 100.0;
                        format!("  cache:{pct:.0}%")
                    } else {
                        String::new()
                    };
                    eprintln!(
                        "  {} {:>6}+{:<6} tok  {}{}",
                        format!("Turn {:>3}", turn_num).dim(),
                        p_tok,
                        c_tok,
                        format_cost(cost),
                        cache_info.dim()
                    );
                }
            }

            eprintln!(
                "\n  {}",
                "─────────────────────────────────────────────────".dim()
            );
            eprintln!(
                "  {:<14} {}+{} tok  {}",
                "total:".bold(),
                total_in,
                total_out,
                format_cost(total_cost).bold(),
            );
            eprintln!();
        }

        "history" => {
            // Across recent sessions
            let sessions = match session_journal::list_sessions() {
                Ok(s) => s,
                Err(e) => {
                    eprintln!(
                        "{}",
                        format!("  ⚠ Could not read session history: {e}").yellow()
                    );
                    return;
                }
            };
            if sessions.is_empty() {
                eprintln!("{}", "  No sessions found.".dim());
                return;
            }

            let pricing = &state.cached_pricing;

            eprintln!(
                "\n{}",
                "─── Session Cost History ────────────────────────".bold()
            );
            eprintln!(
                "  {:<14} ${:.4}/1k prompt, ${:.4}/1k completion",
                "rates:".dim(),
                pricing.prompt,
                pricing.completion
            );
            eprintln!();

            let recent: Vec<_> = sessions.into_iter().take(10).collect();
            let mut grand_total = 0.0f64;

            for sid in &recent {
                if let Ok(events) = session_journal::read_journal(sid) {
                    let stats = session_analytics::compute_session_stats(sid, &events);
                    let cost = cost_for_tokens(
                        stats.total_tokens_in,
                        stats.total_tokens_out,
                        stats.total_cache_read,
                        stats.total_cache_creation,
                        pricing,
                    );
                    grand_total += cost;

                    let short = &sid[..8.min(sid.len())];
                    let model = stats.model.as_deref().unwrap_or("?");
                    eprintln!(
                        "  {} {:>3} turns  {:>6}+{:<6} tok  {}  {}",
                        short.cyan(),
                        stats.turn_count,
                        stats.total_tokens_in,
                        stats.total_tokens_out,
                        format_cost(cost),
                        model.dim(),
                    );
                }
            }

            eprintln!(
                "\n  {} across {} sessions",
                format_cost(grand_total).bold(),
                recent.len(),
            );
            eprintln!();
        }

        _ => {
            // Current session summary
            let pricing = &state.cached_pricing;
            let cache_read_rate = pricing.cache_read.unwrap_or(pricing.prompt * 0.1);
            let cache_write_rate = pricing.cache_write.unwrap_or(pricing.prompt * 1.25);
            let cost = cost_for_tokens(
                state.total_prompt_tokens,
                state.total_completion_tokens,
                state.total_cache_read_tokens,
                state.total_cache_creation_tokens,
                pricing,
            );

            eprintln!(
                "\n{}",
                "─── Session Cost ────────────────────────────────".bold()
            );
            if let Some(ref sid) = state.session_id {
                eprintln!(
                    "  {:<14} {}",
                    "session:".dim(),
                    sid[..8.min(sid.len())].cyan()
                );
            }
            if let Some(ref m) = state.model {
                eprintln!("  {:<14} {}", "model:".dim(), m.as_str().cyan());
            }
            eprintln!(
                "  {:<14} ${:.4}/1k prompt, ${:.4}/1k completion",
                "rates:".dim(),
                pricing.prompt,
                pricing.completion
            );
            eprintln!();
            eprintln!(
                "  {:<14} {} ({})",
                "prompt:".dim(),
                state.total_prompt_tokens,
                format_cost(state.total_prompt_tokens as f64 * pricing.prompt / 1000.0),
            );
            eprintln!(
                "  {:<14} {} ({})",
                "completion:".dim(),
                state.total_completion_tokens,
                format_cost(state.total_completion_tokens as f64 * pricing.completion / 1000.0),
            );
            if state.total_cache_read_tokens > 0 {
                eprintln!(
                    "  {:<14} {} ({})",
                    "cache read:".dim(),
                    state.total_cache_read_tokens,
                    format_cost(state.total_cache_read_tokens as f64 * cache_read_rate / 1000.0),
                );
            }
            if state.total_cache_creation_tokens > 0 {
                eprintln!(
                    "  {:<14} {} ({})",
                    "cache write:".dim(),
                    state.total_cache_creation_tokens,
                    format_cost(
                        state.total_cache_creation_tokens as f64 * cache_write_rate / 1000.0
                    ),
                );
            }
            eprintln!("  {:<14} {}", "total:".bold(), format_cost(cost).bold());
            if state.turn > 0 {
                eprintln!(
                    "  {:<14} {} per turn",
                    "avg:".dim(),
                    format_cost(cost / state.turn as f64)
                );
            }
            if state.total_cache_read_tokens > 0 {
                let total_input = state.total_prompt_tokens + state.total_cache_read_tokens;
                let cache_pct =
                    state.total_cache_read_tokens as f64 / total_input.max(1) as f64 * 100.0;
                let saved = state.total_cache_read_tokens as f64
                    * (pricing.prompt - cache_read_rate)
                    / 1000.0;
                eprintln!(
                    "  {:<14} {:.0}% cache hit, {} saved",
                    "savings:".dim(),
                    cache_pct,
                    format_cost(saved),
                );
            }
            eprintln!(
                "\n  {}",
                "Use /cost detail for per-turn breakdown, /cost history for past sessions.".dim()
            );
            eprintln!();
        }
    }
}

/// Calculate cost in dollars for given token counts.
pub(crate) fn cost_for_tokens(
    prompt_tokens: u64,
    completion_tokens: u64,
    cache_read_tokens: u64,
    cache_creation_tokens: u64,
    pricing: &astra_services::models::PricingData,
) -> f64 {
    let cache_read_rate = pricing.cache_read.unwrap_or(pricing.prompt * 0.1);
    let cache_write_rate = pricing.cache_write.unwrap_or(pricing.prompt * 1.25);
    (prompt_tokens as f64 * pricing.prompt / 1000.0)
        + (completion_tokens as f64 * pricing.completion / 1000.0)
        + (cache_read_tokens as f64 * cache_read_rate / 1000.0)
        + (cache_creation_tokens as f64 * cache_write_rate / 1000.0)
}

/// Format a dollar cost for display.
pub(crate) fn format_cost(cost: f64) -> String {
    if cost < 0.01 {
        format!("${:.4}", cost)
    } else if cost < 1.0 {
        format!("${:.3}", cost)
    } else {
        format!("${:.2}", cost)
    }
}

/// Extract pricing data for a model from the API models list.
pub(crate) fn extract_pricing_for_model(
    models: &[serde_json::Value],
    model_name: &str,
) -> Option<astra_services::models::PricingData> {
    for m in models {
        let name = m
            .get("name")
            .or_else(|| m.get("model_name"))
            .and_then(|v| v.as_str())?;
        if name != model_name {
            continue;
        }
        if let Some(pricing) = m.get("pricing") {
            return serde_json::from_value(pricing.clone()).ok();
        }
        // Fallback: top-level pricing_prompt / pricing_completion fields
        let prompt = m
            .get("pricing_prompt")
            .and_then(|v| v.as_f64())
            .unwrap_or(0.0);
        let completion = m
            .get("pricing_completion")
            .and_then(|v| v.as_f64())
            .unwrap_or(0.0);
        if prompt > 0.0 || completion > 0.0 {
            return Some(astra_services::models::PricingData {
                prompt,
                completion,
                cache_read: None,
                cache_write: None,
            });
        }
        return None;
    }
    None
}

/// Built-in pricing table for known models ($/Ktok).
/// Used when the API model list doesn't include pricing data.
/// Pricing from https://platform.claude.com/docs/en/about-claude/pricing
/// and https://openai.com/api/pricing/
pub(crate) fn fallback_pricing(model_name: &str) -> astra_services::models::PricingData {
    use astra_services::models::PricingData;
    let name = model_name.to_lowercase();

    // Claude Opus 4/4.1: $15/$75 per Mtok
    if name.contains("opus-4") && !name.contains("4.5") && !name.contains("4.6") {
        return PricingData {
            prompt: 0.015,
            completion: 0.075,
            cache_read: Some(0.0015),
            cache_write: Some(0.01875),
        };
    }
    // Claude Opus 4.5/4.6: $5/$25 per Mtok
    if name.contains("opus") {
        return PricingData {
            prompt: 0.005,
            completion: 0.025,
            cache_read: Some(0.0005),
            cache_write: Some(0.00625),
        };
    }
    // Claude Sonnet (3.5/3.7/4/4.5/4.6): $3/$15 per Mtok
    if name.contains("sonnet") {
        return PricingData {
            prompt: 0.003,
            completion: 0.015,
            cache_read: Some(0.0003),
            cache_write: Some(0.00375),
        };
    }
    // Claude Haiku 4.5: $1/$5 per Mtok
    if name.contains("haiku") && (name.contains("4.5") || name.contains("4-5")) {
        return PricingData {
            prompt: 0.001,
            completion: 0.005,
            cache_read: Some(0.0001),
            cache_write: Some(0.00125),
        };
    }
    // Claude Haiku 3.5: $0.80/$4 per Mtok
    if name.contains("haiku") {
        return PricingData {
            prompt: 0.0008,
            completion: 0.004,
            cache_read: Some(0.00008),
            cache_write: Some(0.001),
        };
    }
    // GPT-4o / GPT-4.1: $2.5/$10 per Mtok
    if name.contains("gpt-4o") || name.contains("gpt-4.1") {
        return PricingData {
            prompt: 0.0025,
            completion: 0.01,
            cache_read: Some(0.000625),
            cache_write: None,
        };
    }
    // GPT-4o-mini / GPT-4.1-mini: $0.15/$0.60 per Mtok
    if name.contains("4o-mini")
        || name.contains("4.1-mini")
        || name.contains("5-mini")
        || name.contains("5.4-mini")
    {
        return PricingData {
            prompt: 0.00015,
            completion: 0.0006,
            cache_read: Some(0.0000375),
            cache_write: None,
        };
    }
    // DeepSeek V3/R1: $0.27/$1.10 per Mtok (cache read $0.07)
    if name.contains("deepseek") {
        return PricingData {
            prompt: 0.00027,
            completion: 0.0011,
            cache_read: Some(0.00007),
            cache_write: None,
        };
    }
    // Default: Sonnet pricing as safe fallback
    PricingData {
        prompt: 0.003,
        completion: 0.015,
        cache_read: Some(0.0003),
        cache_write: Some(0.00375),
    }
}

// ═══════════════════════════════════════════════ Output Styles ═════════════

fn handle_style_command(arg: &str) {
    match arg {
        "" | "list" => {
            let current = theme::current_theme_name();
            eprintln!(
                "\n{}",
                "─── Output Styles ───────────────────────────────".bold()
            );
            eprintln!("  {}\n", "Built-in:".dim());
            for t in theme::builtin_themes() {
                let marker = if t.name == current { " ◉" } else { "  " };
                let name = &t.name;
                eprintln!("  {marker} {}", name.as_str().cyan());
            }
            let user_themes = theme::load_user_themes();
            if !user_themes.is_empty() {
                eprintln!("\n  {}\n", "User (~/.astra/styles/):".dim());
                for t in &user_themes {
                    let marker = if t.name == current { " ◉" } else { "  " };
                    let name = &t.name;
                    eprintln!("  {marker} {}", name.as_str().cyan());
                }
            }
            eprintln!(
                "\n  {}",
                "Use /style <name> to switch. Active theme marked with ◉.".dim()
            );
            eprintln!();
        }
        name => match theme::activate_theme_by_name(name) {
            Ok(()) => {
                eprintln!(
                    "  {} {}",
                    theme::icon_ok(),
                    format!("Style set to: {name}").green()
                );
            }
            Err(e) => {
                eprintln!("  {} {e}", theme::icon_err());
                let available: Vec<_> = theme::builtin_themes()
                    .iter()
                    .map(|t| t.name.clone())
                    .chain(theme::load_user_themes().iter().map(|t| t.name.clone()))
                    .collect();
                eprintln!(
                    "  {} Available: {}",
                    theme::icon_info(),
                    available.join(", ")
                );
            }
        },
    }
}

// ═══════════════════════════════════════════════ Tool Profile ═════════════

fn handle_tools_command(state: &ReplState) {
    use astra_services::session_analytics;

    let sid = match &state.session_id {
        Some(s) => s.clone(),
        None => {
            eprintln!("{}", "  No active session.".dim());
            return;
        }
    };
    let events = session_journal::read_journal(&sid).unwrap_or_default();
    let profiles = session_analytics::compute_tool_profiles(&events);

    if profiles.is_empty() {
        eprintln!("{}", "  No tool calls recorded yet.".dim());
        return;
    }

    eprintln!(
        "\n{}",
        "─── Tool Performance ────────────────────────────".bold()
    );
    eprintln!(
        "  {:<20} {:>5} {:>5} {:>7} {:>7} {:>7} {:>6}",
        "tool".bold(),
        "calls".bold(),
        "fail".bold(),
        "avg ms".bold(),
        "min ms".bold(),
        "max ms".bold(),
        "err%".bold(),
    );
    for p in &profiles {
        let err_pct = format!("{:.0}%", p.error_rate * 100.0);
        let err_display = if p.fail_count > 0 {
            err_pct.red().to_string()
        } else {
            err_pct
        };
        eprintln!(
            "  {:<20} {:>5} {:>5} {:>7} {:>7} {:>7} {:>6}",
            p.name.as_str().cyan(),
            p.call_count,
            p.fail_count,
            p.avg_ms,
            p.min_ms,
            p.max_ms,
            err_display,
        );
    }
    let total_ms: u64 = profiles.iter().map(|p| p.total_ms).sum();
    let total_calls: u32 = profiles.iter().map(|p| p.call_count).sum();
    eprintln!(
        "\n  {} {} calls, {:.1}s total tool time",
        "Summary:".bold(),
        total_calls,
        total_ms as f64 / 1000.0,
    );
    eprintln!();
}

async fn handle_health_command(arg: &str, state: &ReplState) {
    use astra_runtime::turn::tool_health::ToolHealthTracker;

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
            sorted.sort_by(|a, b| b.1.total_failures.cmp(&a.1.total_failures));
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
            let svc = astra_services::state_sync::MatrixOneSyncService::new(
                mc.shared_pool().get().clone(),
            );
            let sync_status = astra_services::state_sync::StateSyncService::status(&svc).await;
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
fn display_sync_status(status: &astra_services::SyncStatus) {
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
fn format_sync_age(ts: &str) -> String {
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
                        .with_timezone(&chrono::FixedOffset::east_opt(0).unwrap())
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

/// Handle `/sync` command — show unified sync state across all domains.
/// Subcommands: `/sync push`, `/sync pull`, `/sync log`.
async fn handle_sync_command(arg: &str, state: &ReplState) {
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

    eprintln!(
        "\n{}",
        "─── Sync Engine Status ─────────────────────────".bold()
    );

    let Some(mc) = state.matrix_runtime.as_ref() else {
        eprintln!(
            "  {} {}",
            "○".dim(),
            "Sync orchestrator not initialized (no cloud connection)".dim()
        );
        eprintln!(
            "{}",
            "────────────────────────────────────────────────".dim()
        );
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
            eprintln!("\n  {}", "No sync events yet.".dim());
        } else {
            eprintln!(
                "\n{}",
                "─── Sync Event Log ─────────────────────────────".bold()
            );
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
        eprintln!("\n  {}", "Use /sync log | push | pull".dim());
    }

    eprintln!(
        "{}",
        "────────────────────────────────────────────────".dim()
    );
    eprintln!();
}

/// Force-push all dirty sync domains to cloud.
async fn handle_sync_push(state: &ReplState) {
    eprintln!(
        "\n{}",
        "─── Sync Push ──────────────────────────────────".bold()
    );

    let Some(mc) = state.matrix_runtime.as_ref() else {
        eprintln!(
            "  {} {}",
            "○".dim(),
            "No cloud connection — nothing to push.".dim()
        );
        eprintln!(
            "{}",
            "────────────────────────────────────────────────".dim()
        );
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
        eprintln!(
            "  {} All domains clean — nothing to push.",
            theme::icon_ok()
        );
        eprintln!(
            "{}",
            "────────────────────────────────────────────────".dim()
        );
        eprintln!();
        return;
    }

    eprintln!(
        "  Pushing {} dirty domain{}...\n",
        dirty_count,
        if dirty_count == 1 { "" } else { "s" }
    );

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
        eprintln!(
            "  {} {} domain{} pushed successfully.",
            "✓".green().bold(),
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

    eprintln!(
        "{}",
        "────────────────────────────────────────────────".dim()
    );
    eprintln!();
}

/// Force-pull all pullable domains from cloud (skips write-only domains like Events).
async fn handle_sync_pull(state: &ReplState) {
    eprintln!(
        "\n{}",
        "─── Sync Pull ──────────────────────────────────".bold()
    );

    let Some(mc) = state.matrix_runtime.as_ref() else {
        eprintln!(
            "  {} {}",
            "○".dim(),
            "No cloud connection — nothing to pull.".dim()
        );
        eprintln!(
            "{}",
            "────────────────────────────────────────────────".dim()
        );
        eprintln!();
        return;
    };

    let mut orch = mc.sync_orchestrator_lock().await;
    eprintln!("  Pulling from cloud...\n");

    let results = orch.pull_all().await;
    drop(orch);

    if results.is_empty() {
        eprintln!("  {} No pullable domains configured.", "○".dim());
        eprintln!(
            "{}",
            "────────────────────────────────────────────────".dim()
        );
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
        eprintln!(
            "  {} {} domain{} pulled successfully.",
            "✓".green().bold(),
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

    eprintln!(
        "{}",
        "────────────────────────────────────────────────".dim()
    );
    eprintln!();
}

/// Handle `/learn` command — show learning insights, drift detection, exploration.
fn handle_learn_command(arg: &str, state: &ReplState) {
    use astra_runtime::pipeline::pattern::ExplorationReason;

    let lib = match &state.pattern_library {
        Some(pl) => pl.lock().unwrap(),
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

fn format_bytes(bytes: u64) -> String {
    if bytes < 1024 {
        format!("{}B", bytes)
    } else if bytes < 1024 * 1024 {
        format!("{:.1}KB", bytes as f64 / 1024.0)
    } else {
        format!("{:.1}MB", bytes as f64 / (1024.0 * 1024.0))
    }
}

async fn build_turn_skill_resolver(
    unified_skill_registry: std::sync::Arc<astra_runtime::skills::UnifiedSkillRegistry>,
) -> Option<std::sync::Arc<dyn astra_runtime::turn::skill_tool::SkillResolver>> {
    if unified_skill_registry.is_empty() {
        let _ = unified_skill_registry.discover_all().await;
    }

    let inner_resolver = std::sync::Arc::new(astra_runtime::skills::UnifiedSkillResolver::new(
        unified_skill_registry,
    ));
    let adapter = astra_runtime::skills::registry::LegacySkillResolverAdapter::new(inner_resolver);
    let skills = astra_runtime::turn::skill_tool::SkillResolver::available_skills(&adapter);
    if skills.is_empty() {
        None
    } else {
        Some(std::sync::Arc::new(adapter)
            as std::sync::Arc<
                dyn astra_runtime::turn::skill_tool::SkillResolver,
            >)
    }
}

async fn initialize_multi_agent_runtime(
    state: &mut ReplState,
    api: &astra_thin_client::ThinClient,
    token: String,
) {
    let project_root = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let skill_resolver = build_turn_skill_resolver(state.unified_skill_registry.clone()).await;

    let mut registry = astra_services::AgentProfileRegistry::new();
    delegate_subrun::register_default_agents(&mut registry);
    let custom_count = agent_loader::load_and_merge(&project_root, &mut registry);
    if custom_count > 0 {
        eprintln!("  loaded {custom_count} custom agent(s) from .astra/agents/");
    }
    let registry = std::sync::Arc::new(tokio::sync::RwLock::new(registry));

    let run_store = std::sync::Arc::new(astra_services::runs::InMemoryRunStateStore::default());
    let tracker =
        std::sync::Arc::new(astra_runtime::server::delegation_engine::DelegationTracker::new());
    let transport = std::sync::Arc::new(astra_runtime::messaging::InProcessTransport::new());
    let mailbox_router = std::sync::Arc::new(astra_runtime::messaging::AgentMailboxRouter::new(
        transport,
        tracker.clone(),
    ));

    let delegate_executor = delegate_subrun::CliDelegateSubRunExecutor::new(
        api.clone(),
        token.clone(),
        state.model.clone(),
        project_root.clone(),
        state.perm_manager.mode(),
        None,
    )
    .with_skill_resolver(skill_resolver.clone())
    .with_skill_search(state.skill_search.clone());

    let engine = astra_runtime::server::delegation_engine::DelegationEngine::with_executor(
        registry,
        std::sync::Arc::new(astra_runtime::server::run_engine::RunEngine::new(run_store)),
        tracker,
        std::sync::Arc::new(delegate_executor),
    )
    .with_mailbox_router(mailbox_router.clone());
    state.delegation_engine = Some(std::sync::Arc::new(engine));

    let spawn_executor = spawn_subrun::CliSpawnAgentExecutor::new(
        api.clone(),
        token,
        project_root,
        state.perm_manager.mode(),
        None,
    )
    .with_skill_resolver(skill_resolver)
    .with_skill_search(state.skill_search.clone());

    state.agent_spawner = Some(std::sync::Arc::new(
        astra_runtime::orchestration::DynamicAgentSpawner::new(mailbox_router)
            .with_executor(std::sync::Arc::new(spawn_executor)),
    ));
}

// ═══════════════════════════════════════════════ Plan Auto-Execution ═════

/// Build a [`TaskLearningBridge`] from ReplState's shared pipeline components.
///
/// Returns `None` if any of the required learning modules (entity_graph,
/// pattern_library, calibrator) are not yet initialized.
fn build_learning_bridge(
    state: &ReplState,
) -> Option<std::sync::Arc<dyn astra_services::TaskLearningBridge>> {
    let eg = state.entity_graph.as_ref()?;
    let pl = state.pattern_library.as_ref()?;
    let cal = state.calibrator.as_ref()?;
    let mut bridge =
        astra_runtime::pipeline::task_learning::PipelineTaskLearningBridge::from_shared(
            eg.clone(),
            pl.clone(),
            cal.clone(),
        );
    // Wire cloud pool for template persistence
    if let Some(mc) = &state.matrix_runtime {
        let pool = mc.shared_pool().get().clone();
        let user_id = state.ingestion_user_id.as_deref().unwrap_or("anonymous");
        bridge = bridge.with_cloud_pool(pool, user_id);
    }
    Some(std::sync::Arc::new(bridge))
}

/// Shown after Ctrl+C pauses plan auto-execution (interrupt is not sent to the model).
#[allow(dead_code)] // Used in interactive pause path; kept for background executor integration
fn eprint_plan_execution_paused_hints() {
    eprintln!("{}", "  What you can do:".dim());
    eprintln!(
        "    {}",
        "continue · resume · next · go · 继续 — resume execution from this point".dim()
    );
    eprintln!(
        "    {}",
        "Lines starting with / — run a slash command; the paused plan stays in memory".dim()
    );
    eprintln!(
        "    {}",
        "Any other message — abandons the plan and sends it as a normal chat turn".dim()
    );
    eprintln!(
        "    {}",
        "Step-by-step mode: at \"Execute this subtask?\", use skip to defer one subtask".dim()
    );
    eprintln!(
        "    {}",
        "correct … / note … / adjust … — stack guidance for upcoming subtasks (correct clear to drop)"
            .dim()
    );
    eprintln!(
        "    {}",
        "rewind N · restart N · redo from N — reset step N and all later steps (1-based list order)"
            .dim()
    );
    eprintln!(
        "    {}",
        "rewind <id-prefix> — same, anchored by subtask id (prefix must match exactly one id)"
            .dim()
    );
}

/// Format a progress bar line for plan execution.
///
/// Example: `[████████░░░░] 3/7 (42%) · ~2m14s remaining`
pub(crate) fn format_plan_progress(
    done: usize,
    total: usize,
    avg_duration: Option<std::time::Duration>,
    elapsed: std::time::Duration,
) -> String {
    let bar_width = 16;
    let pct = if total > 0 {
        (done as f64 / total as f64 * 100.0) as u32
    } else {
        0
    };
    let filled = if total > 0 {
        (done * bar_width) / total
    } else {
        0
    };
    let empty = bar_width - filled;
    let bar = format!("{}{}", "█".repeat(filled), "░".repeat(empty),);

    let elapsed_str = format_duration_short(elapsed);

    let eta_str = if done > 0 {
        if let Some(avg) = avg_duration {
            let remaining = total.saturating_sub(done);
            let eta = avg * remaining as u32;
            format!(" · ~{} remaining", format_duration_short(eta))
        } else {
            String::new()
        }
    } else {
        String::new()
    };

    format!("[{bar}] {done}/{total} ({pct}%) · {elapsed_str} elapsed{eta_str}")
}

/// Format a Duration as a short human-readable string (e.g., "1m32s", "45s", "2h5m").
pub(crate) fn format_duration_short(d: std::time::Duration) -> String {
    let secs = d.as_secs();
    if secs >= 3600 {
        let h = secs / 3600;
        let m = (secs % 3600) / 60;
        format!("{h}h{m}m")
    } else if secs >= 60 {
        let m = secs / 60;
        let s = secs % 60;
        format!("{m}m{s}s")
    } else {
        format!("{secs}s")
    }
}

/// Format a duration in milliseconds as a compact human-readable string.
fn format_duration_ms(ms: u64) -> String {
    if ms >= 60_000 {
        let m = ms / 60_000;
        let s = (ms % 60_000) / 1000;
        format!("{m}m{s}s")
    } else if ms >= 1000 {
        let s = ms as f64 / 1000.0;
        format!("{s:.1}s")
    } else {
        format!("{ms}ms")
    }
}

// ═══════════════════════════════════ Background Plan Execution Wiring ═════

/// Extract a [`BackgroundPlanContext`] from the current REPL state.
///
/// Moves the active plan, durable task state, and corrections out of `state`
/// (using `take()`), and clones the remaining fields needed by the background
/// executor.  On success `state.executing_plan` will be `None`.
fn take_plan_context(
    state: &mut ReplState,
    api: &astra_thin_client::ThinClient,
    current_token: Option<&str>,
    profile: Option<&str>,
) -> Result<plan_executor::BackgroundPlanContext, String> {
    let plan = state.executing_plan.take().ok_or("No plan to execute")?;
    let token = current_token
        .ok_or("Not logged in — cannot start background plan")?
        .to_string();

    Ok(plan_executor::BackgroundPlanContext {
        api: api.clone(),
        token,
        profile: profile.map(|p| p.to_string()),
        model: state.model.clone(),
        plan,
        plan_goal: state.executing_plan_goal.clone(),
        plan_corrections: std::mem::take(&mut state.plan_execution_corrections),
        history: state.history.clone(),
        session_id: state.session_id.clone(),
        recent_tools: state.recent_tools.clone(),
        tool_health_entries: state.tool_health_entries.clone(),
        unified_skill_registry: state.unified_skill_registry.clone(),
        skill_search: state.skill_search.clone(),
        delegation_engine: state.delegation_engine.clone(),
        messaging_metrics: state.messaging_metrics.clone(),
        agent_spawner: state.agent_spawner.clone(),
        root_mailbox: None,
        root_agent_id: format!("plan-{}", uuid::Uuid::new_v4()),
        durable_task_state: state.durable_task_state.take(),
        workspace_root: std::env::current_dir().unwrap_or_default(),
        // Cloud + learning integration
        ingestion_user_id: state.ingestion_user_id.clone(),
        matrix_runtime: state.matrix_runtime.clone(),
        entity_graph: state.entity_graph.clone(),
        pattern_library: state.pattern_library.clone(),
        calibrator: state.calibrator.clone(),
        // Execution config
        plan_execution_config: state.plan_execution_config.clone(),
        turn: state.turn,
        turn_retry_counts: std::collections::HashMap::new(),
    })
}

/// Create a `Box<dyn ToolSelector>` for the background plan executor.
///
/// Shares `entity_graph` / `pattern_library` / `calibrator` with [`plan_executor::BackgroundPlanContext`]
/// when all three are present (same `Arc`s as the REPL). TF-IDF index is still per-selector.
fn create_background_selector(
    ctx: &plan_executor::BackgroundPlanContext,
) -> Box<dyn tool_selector::ToolSelector> {
    create_background_plan_selector(ctx)
}

/// Spawn a plan executor, then block until it finishes, pauses, or errors.
///
/// The executor runs as a `tokio` task; this function enters a monitoring
/// loop that displays progress in real-time, handles Ctrl-C (→ pause), and
/// resolves approval prompts inline. The REPL prompt is not shown until
/// this function returns.
async fn start_and_monitor_plan(
    state: &mut ReplState,
    current_token: Option<&str>,
    api: &astra_thin_client::ThinClient,
    profile: Option<&str>,
) -> Result<(), String> {
    // ── Cancel any existing executor to prevent orphan tasks ─────
    if plan_interaction::shutdown_plan_executor(state) {
        eprintln!(
            "  {}  Previous plan executor cancelled before starting new run.",
            theme::icon_warn()
        );
    }

    // ── Generate durable contract if not already present ─────────
    ensure_durable_task_state(state, Some(api), current_token).await;

    // ── Extract context & spawn ──────────────────────────────────────
    let ctx = take_plan_context(state, api, current_token, profile)?;
    let selector = create_background_selector(&ctx);
    let handle = plan_executor::spawn_plan_executor(ctx, selector);
    state.plan_handle = Some(handle);

    eprintln!("  {} {}", "▸".bold().cyan(), "Plan executing…".bold());

    // ── Block until done / paused / error ────────────────────────────
    run_blocking_plan_monitor(state).await;

    Ok(())
}

/// Initialize `durable_task_state` on `ReplState` if it's `None` and a plan
/// is ready for execution.  This generates a [`TaskContract`] with structured
/// verification criteria so the background executor can gate subtask completion.
async fn ensure_durable_task_state(
    state: &mut ReplState,
    api: Option<&astra_thin_client::ThinClient>,
    token: Option<&str>,
) {
    if state.durable_task_state.is_some() {
        return;
    }
    let plan = match state.executing_plan.as_ref() {
        Some(p) => p,
        None => return,
    };

    let goal = state
        .executing_plan_goal
        .as_deref()
        .unwrap_or("Plan execution");
    let user_id = state.ingestion_user_id.as_deref().unwrap_or("local");
    let session_id = state.session_id.as_deref().unwrap_or("unknown");
    let work_dir = std::env::current_dir().unwrap_or_default();

    // Build server proxy judge from ThinClient (zero-config fallback)
    let server_proxy_judge: Option<std::sync::Arc<dyn astra_services::LlmJudge>> =
        if let (Some(a), Some(t)) = (api, token) {
            Some(std::sync::Arc::new(
                durable_bridge::ServerProxyLlmJudge::new(
                    a.clone(),
                    t.to_string(),
                    state.model.clone(),
                ),
            ))
        } else {
            None
        };

    // Prefer cloud-backed lifecycle when MatrixOne pool is available;
    // fall back to local filesystem persistence otherwise.
    let ingestion_sender = state
        .matrix_runtime
        .as_ref()
        .and_then(|mc| mc.clone_ingestion_sender());
    let cloud_judge = state
        .matrix_runtime
        .as_ref()
        .and_then(|mc| mc.create_cloud_llm_judge())
        .map(|j| std::sync::Arc::new(j) as std::sync::Arc<dyn astra_services::LlmJudge>);
    let learning = build_learning_bridge(state);

    let lifecycle = if let Some(pool) = state
        .matrix_runtime
        .as_ref()
        .map(|mc| mc.shared_pool().get().clone())
    {
        durable_bridge::create_cloud_lifecycle_full(
            pool,
            &work_dir,
            ingestion_sender,
            Some(session_id),
            Some(user_id),
            cloud_judge,
            learning,
            server_proxy_judge,
        )
    } else {
        let session_dir = state
            .session_id
            .as_ref()
            .map(|sid| astra_services::session_workspace::workspace_dir_for(sid))
            .unwrap_or_else(|| work_dir.join(".mo-session"));
        durable_bridge::create_local_lifecycle_full(
            &session_dir,
            &work_dir,
            ingestion_sender,
            Some(session_id),
            Some(user_id),
            cloud_judge,
            learning,
            server_proxy_judge,
        )
    };

    if let Some(contract) =
        durable_bridge::generate_contract(&lifecycle, plan, goal, user_id, session_id, &work_dir)
            .await
    {
        state.durable_task_state = Some(durable_bridge::DurableTaskState {
            contract,
            lifecycle,
            last_report: None,
        });

        // Construct a delegation engine for plan execution with verification gates.
        if state.delegation_engine.is_none() {
            let registry = std::sync::Arc::new(tokio::sync::RwLock::new(
                astra_services::AgentProfileRegistry::new(),
            ));
            let run_store =
                std::sync::Arc::new(astra_services::runs::InMemoryRunStateStore::default());
            let engine = astra_runtime::server::delegation_engine::DelegationEngine::with_executor(
                registry,
                std::sync::Arc::new(astra_runtime::server::run_engine::RunEngine::new(run_store)),
                std::sync::Arc::new(
                    astra_runtime::server::delegation_engine::DelegationTracker::new(),
                ),
                std::sync::Arc::new(astra_runtime::server::delegation_engine::StubSubRunExecutor),
            );
            state.delegation_engine = Some(std::sync::Arc::new(engine));
        }
    }
}

/// Outcome of draining plan updates — used by the blocking monitor to decide
/// whether to keep looping, break on pause, or stop on finish/error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PlanMonitorOutcome {
    /// Normal drain — more updates may follow.
    Continue,
    /// `PlanPaused` received — executor is waiting for Resume/Cancel.
    Paused,
    /// `PlanCompleted` or `PlanError` received — executor has exited.
    Finished,
}

/// Wrapper enum for the different spinner types used in plan mode.
/// Provides a uniform interface so the plan monitor can swap spinner styles.
enum PlanSpinner {
    /// Plan-specific spinner: `[subtask] Ns Label ⣾`
    Activity(effects::PlanActivitySpinner),
    /// Chat-style TTFT spinner: `Ns Waiting for stream ⣾`
    Ttft(effects::TtftWaitLineSpinner),
    /// Chat-style tool spinner: `Ns Running… description ⣾`
    Tool(effects::ToolRunningLineSpinner),
}

impl PlanSpinner {
    fn stop_clear(self) {
        match self {
            Self::Activity(s) => s.stop_clear(),
            Self::Ttft(s) => s.stop_clear(),
            Self::Tool(s) => s.stop_clear(),
        }
    }
}

/// Drain plan updates from the executor channel and display them via
/// `eprintln!`. Returns the monitor outcome so the caller can decide
/// whether to keep polling.
fn display_plan_updates_live(
    state: &mut ReplState,
    plan_spinner: &mut Option<PlanSpinner>,
    current_subtask_tag: &mut String,
) -> PlanMonitorOutcome {
    use plan_executor::PlanUpdate;
    let mut outcome = PlanMonitorOutcome::Continue;

    /// Finish any in-flight markdown stream: clear spinner, finalize renderer, newline.
    fn finalize_plan_stream(
        in_stream: &mut bool,
        spinner: &mut Option<PlanSpinner>,
        md: &mut Option<streaming_md::StreamingMarkdown>,
        thinking_pane: &mut Option<effects::ThinkingPreviewPane>,
    ) {
        // Finalize thinking pane before any other output
        if let Some(mut pane) = thinking_pane.take() {
            let summary = pane.summary_line();
            pane.clear();
            eprintln!("{summary}");
        }
        if *in_stream {
            *in_stream = false;
            if let Some(s) = spinner.take() {
                s.stop_clear();
            }
            if let Some(renderer) = md {
                renderer.finish();
            }
            *md = None;
            eprintln!();
        }
    }

    /// Clear active plan spinner (if any), finalize token/md stream, then print a line.
    fn print_plan_monitor_line(
        spinner: &mut Option<PlanSpinner>,
        in_stream: &mut bool,
        md: &mut Option<streaming_md::StreamingMarkdown>,
        thinking_pane: &mut Option<effects::ThinkingPreviewPane>,
        msg: String,
    ) {
        finalize_plan_stream(in_stream, spinner, md, thinking_pane);
        if let Some(s) = spinner.take() {
            s.stop_clear();
        }
        eprintln!("{msg}");
    }

    let handle = match state.plan_handle.as_mut() {
        Some(h) => h,
        None => return outcome,
    };

    while let Some(update) = handle.try_recv() {
        // Determine the message to display and what spinner to start after printing.
        enum PostSpinner {
            None,
            Ttft,
            Tool(String),
            Activity(String),
        }
        let (msg, post_spinner): (String, PostSpinner) = match update {
            PlanUpdate::SubtaskStarted {
                id,
                title,
                index,
                total,
                ..
            } => {
                *current_subtask_tag = id;
                (
                    format!(
                        "\n  {} {} {}",
                        format!("▸ [{index}/{total}]").bold().cyan(),
                        title.bold(),
                        ""
                    ),
                    PostSpinner::Ttft,
                )
            }
            PlanUpdate::SubtaskCompleted {
                id,
                verification_passed,
                elapsed,
                ..
            } => {
                let dur = elapsed
                    .map(|d| format!(" ({})", format_duration_short(d)))
                    .unwrap_or_default();
                if verification_passed {
                    (
                        format!(
                            "  {} {} {}{}",
                            theme::icon_ok(),
                            "done".bold(),
                            id.dim(),
                            dur.dim()
                        ),
                        PostSpinner::Activity("Next subtask".to_string()),
                    )
                } else {
                    (
                        format!(
                            "  {} {} — {}{}",
                            theme::icon_warn(),
                            id,
                            "verification failed".yellow(),
                            dur.dim()
                        ),
                        PostSpinner::Activity("Next subtask".to_string()),
                    )
                }
            }
            PlanUpdate::SubtaskTurnResult {
                subtask_id,
                prompt_tokens,
                completion_tokens,
                ..
            } => {
                state.total_prompt_tokens += prompt_tokens;
                state.total_completion_tokens += completion_tokens;
                state.turn += 1;
                state.current_plan_subtask_id = Some(subtask_id);
                continue; // No visible output for token accounting
            }
            PlanUpdate::PlanProgress {
                done,
                total,
                elapsed,
                eta,
            } => {
                if state.plan_run_task_id.is_some() {
                    let pct = if total > 0 {
                        (done * 100 / total) as u32
                    } else {
                        0
                    };
                    state.plan_run_task_last_progress = Some((pct, done as u32, total as u32));
                }
                // Feed ETA into the active spinner (only PlanActivitySpinner supports it)
                if let Some(PlanSpinner::Activity(spinner)) = plan_spinner.as_ref() {
                    spinner.set_eta_secs(eta.map(|d| d.as_secs()).unwrap_or(0));
                }
                // Don't print progress line or stop spinner — just update state silently.
                // The spinner keeps running so the user always sees activity.
                let _ = (elapsed, eta); // suppress unused warnings
                continue;
            }
            PlanUpdate::PlanCompleted { pct, elapsed } => {
                // Stop spinner
                if let Some(s) = plan_spinner.take() {
                    s.stop_clear();
                }
                // Take handle out of state to drain without double-borrow
                if let Some(mut h) = state.plan_handle.take() {
                    while let Some(trailing) = h.try_recv() {
                        apply_trailing_update(trailing, state);
                    }
                }
                let msg = format!(
                    "\n🏁  Plan complete — {pct}% verified in {}",
                    format_duration_short(elapsed),
                );
                state.executing_plan = None;
                state.current_plan_subtask_id = None;
                // Deny any pending approval (plan is done)
                if let Some(tx) = state.pending_approval.take() {
                    let _ = tx.send(false);
                }
                print_plan_monitor_line(
                    plan_spinner,
                    &mut state.plan_in_token_stream,
                    &mut state.plan_md_renderer,
                    &mut state.plan_thinking_pane,
                    msg,
                );
                // Auto-display delivery report if available
                if let Some(ref report) = state.last_delivery_report {
                    eprintln!();
                    durable_bridge::display_delivery_report(report);
                }
                if state.plan_mode.is_some() {
                    eprintln!(
                        "{}",
                        "  Still in plan mode — type exit when you want normal chat.".dim()
                    );
                }
                return PlanMonitorOutcome::Finished;
            }
            PlanUpdate::PlanError { error } => {
                state.plan_run_task_last_error = Some(error.clone());
                // Stop spinner
                if let Some(s) = plan_spinner.take() {
                    s.stop_clear();
                }
                if let Some(mut h) = state.plan_handle.take() {
                    while let Some(trailing) = h.try_recv() {
                        apply_trailing_update(trailing, state);
                    }
                }
                let msg = format!("\n❌  Plan error: {error}");
                state.executing_plan = None;
                state.current_plan_subtask_id = None;
                // Deny any pending approval (plan failed)
                if let Some(tx) = state.pending_approval.take() {
                    let _ = tx.send(false);
                }
                print_plan_monitor_line(
                    plan_spinner,
                    &mut state.plan_in_token_stream,
                    &mut state.plan_md_renderer,
                    &mut state.plan_thinking_pane,
                    msg,
                );
                if state.plan_mode.is_some() {
                    eprintln!(
                        "{}",
                        "  Still in plan mode — type exit to leave or resume after fixing.".dim()
                    );
                }
                return PlanMonitorOutcome::Finished;
            }
            PlanUpdate::PlanPaused {
                pct,
                remaining,
                elapsed,
            } => {
                outcome = PlanMonitorOutcome::Paused;
                (
                    format!(
                        "\n⏸  Plan paused — {pct}% done, {remaining} remaining ({})",
                        format_duration_short(elapsed),
                    ),
                    PostSpinner::None,
                )
            }
            PlanUpdate::GlobalVerificationFailed => (
                "  ⚠ Global verification failed".to_string(),
                PostSpinner::None,
            ),
            PlanUpdate::JournalEvent(event) => {
                // Write journal event to the REPL-owned journal writer
                if let Some(ref journal) = state.journal {
                    let _ = journal.append(&event);
                }
                continue; // No visible output
            }
            PlanUpdate::HistoryEntry {
                user_msg,
                assistant_msg,
            } => {
                // Append to REPL conversation history
                state.history.push((user_msg, assistant_msg));
                continue;
            }
            PlanUpdate::DeliveryReport(report) => {
                state.last_delivery_report = Some(report);
                continue;
            }
            PlanUpdate::VerificationReport(report) => {
                finalize_plan_stream(
                    &mut state.plan_in_token_stream,
                    plan_spinner,
                    &mut state.plan_md_renderer,
                    &mut state.plan_thinking_pane,
                );
                if let Some(s) = plan_spinner.take() {
                    s.stop_clear();
                }
                durable_bridge::display_verification_report(&report);
                // Start a spinner so the user sees activity between verification and next event
                *plan_spinner = Some(PlanSpinner::Activity(effects::PlanActivitySpinner::start(
                    current_subtask_tag,
                    "Continuing",
                )));
                continue;
            }
            PlanUpdate::SubtaskRetry {
                id,
                retries_exhausted,
                attempt,
                max_retries,
                failure_hint,
                ..
            } => {
                let attempt_str = if max_retries > 0 {
                    format!(" ({attempt}/{max_retries})")
                } else {
                    String::new()
                };
                let hint_str = failure_hint.map(|h| format!(": {h}")).unwrap_or_default();
                if retries_exhausted {
                    (
                        format!("  ⚠ {id} — verification failed{attempt_str}{hint_str}"),
                        PostSpinner::None,
                    )
                } else {
                    (
                        format!("  ↻ {id} — verification failed{attempt_str}{hint_str}, retrying…"),
                        PostSpinner::Ttft,
                    )
                }
            }
            PlanUpdate::StreamingEvent { event, .. } => {
                use chat_stream::StreamEvent;
                match event {
                    StreamEvent::ToolStarted { name, description } => {
                        let styled = stream_render::style_tool_description(&name, &description);
                        (
                            format!("  {} {} …", "⬢".cyan(), styled),
                            PostSpinner::Tool(description),
                        )
                    }
                    StreamEvent::ToolCompleted {
                        name,
                        description,
                        status,
                        duration_ms,
                        output_summary,
                    } => {
                        let dur = cli_formatting::format_duration_suffix(duration_ms);
                        let icon = if status == "error" {
                            theme::icon_err()
                        } else {
                            theme::icon_ok()
                        };
                        let styled = stream_render::style_tool_description(&name, &description);
                        let summary = output_summary
                            .map(|s| format!("\n    {}", s.dim()))
                            .unwrap_or_default();
                        (
                            format!("  {icon} {styled}{}{summary}", dur.dim()),
                            PostSpinner::None,
                        )
                    }
                    StreamEvent::WaitingForModel => {
                        finalize_plan_stream(
                            &mut state.plan_in_token_stream,
                            plan_spinner,
                            &mut state.plan_md_renderer,
                            &mut state.plan_thinking_pane,
                        );
                        if let Some(s) = plan_spinner.take() {
                            s.stop_clear();
                        }
                        *plan_spinner =
                            Some(PlanSpinner::Ttft(effects::TtftWaitLineSpinner::start()));
                        continue;
                    }
                    StreamEvent::ModelResponding => {
                        finalize_plan_stream(
                            &mut state.plan_in_token_stream,
                            plan_spinner,
                            &mut state.plan_md_renderer,
                            &mut state.plan_thinking_pane,
                        );
                        if let Some(s) = plan_spinner.take() {
                            s.stop_clear();
                        }
                        *plan_spinner =
                            Some(PlanSpinner::Activity(effects::PlanActivitySpinner::start(
                                current_subtask_tag,
                                "Model responding",
                            )));
                        continue;
                    }
                    StreamEvent::Thinking(true) => {
                        // Reuse existing pane if model sends multiple thinking blocks
                        if state.plan_thinking_pane.is_none() {
                            finalize_plan_stream(
                                &mut state.plan_in_token_stream,
                                plan_spinner,
                                &mut state.plan_md_renderer,
                                &mut state.plan_thinking_pane,
                            );
                            if let Some(s) = plan_spinner.take() {
                                s.stop_clear();
                            }
                            use std::io::IsTerminal;
                            let rows = effects::thinking_viewport_rows();
                            let tw = crossterm::terminal::size()
                                .map(|(w, _)| w as usize)
                                .unwrap_or(80);
                            if rows > 0 && std::io::stdout().is_terminal() {
                                state.plan_thinking_pane =
                                    Some(effects::ThinkingPreviewPane::new(rows, tw));
                            } else {
                                *plan_spinner = Some(PlanSpinner::Activity(
                                    effects::PlanActivitySpinner::start(
                                        current_subtask_tag,
                                        "Thinking",
                                    ),
                                ));
                            }
                        }
                        continue;
                    }
                    StreamEvent::ThinkingChunk(text) => {
                        if let Some(ref mut pane) = state.plan_thinking_pane {
                            pane.push_chunk(&text);
                        }
                        continue;
                    }
                    StreamEvent::Thinking(false) => {
                        // Don't destroy the pane — the model may send more thinking blocks.
                        // Summary is printed when we transition to tokens/tools.
                        continue;
                    }
                    StreamEvent::Token(text) => {
                        // Finalize thinking pane before token stream starts
                        if let Some(mut pane) = state.plan_thinking_pane.take() {
                            let summary = pane.summary_line();
                            pane.clear();
                            eprintln!("{summary}");
                        }
                        if !state.plan_in_token_stream {
                            if let Some(s) = plan_spinner.take() {
                                s.stop_clear();
                            }
                            state.plan_in_token_stream = true;
                            let tw = crossterm::terminal::size()
                                .map(|(w, _)| w as usize)
                                .unwrap_or(80);
                            state.plan_md_renderer = Some(streaming_md::StreamingMarkdown::new(tw));
                        }
                        if let Some(ref mut md) = state.plan_md_renderer {
                            md.push(&text);
                        }
                        continue;
                    }
                    StreamEvent::StatusLine(line) => {
                        finalize_plan_stream(
                            &mut state.plan_in_token_stream,
                            plan_spinner,
                            &mut state.plan_md_renderer,
                            &mut state.plan_thinking_pane,
                        );
                        if let Some(s) = plan_spinner.take() {
                            s.stop_clear();
                        }
                        eprintln!("    {line}");
                        continue;
                    }
                }
            }
            PlanUpdate::ApprovalNeeded {
                tool,
                header,
                detail,
                reason,
                response_tx,
            } => {
                // Stop spinner while waiting for user input
                if let Some(s) = plan_spinner.take() {
                    s.stop_clear();
                }
                let msg = format!(
                    "\n{}  {} — {}\n   {}\n   Reason: {}",
                    theme::icon_warn(),
                    tool,
                    header,
                    detail.as_deref().unwrap_or(""),
                    reason,
                );
                print_plan_monitor_line(
                    plan_spinner,
                    &mut state.plan_in_token_stream,
                    &mut state.plan_md_renderer,
                    &mut state.plan_thinking_pane,
                    msg,
                );
                // Store the response channel for the REPL to resolve
                state.pending_approval = Some(response_tx);
                continue;
            }
            _ => continue, // ParallelGroupInfo, StepByStepPrompt — future use
        };

        // Print the message (stops spinner internally)
        print_plan_monitor_line(
            plan_spinner,
            &mut state.plan_in_token_stream,
            &mut state.plan_md_renderer,
            &mut state.plan_thinking_pane,
            msg,
        );

        // Optionally start a new spinner after the printed line
        match post_spinner {
            PostSpinner::Ttft => {
                *plan_spinner = Some(PlanSpinner::Ttft(effects::TtftWaitLineSpinner::start()));
            }
            PostSpinner::Tool(desc) => {
                *plan_spinner = Some(PlanSpinner::Tool(effects::ToolRunningLineSpinner::start(
                    desc,
                )));
            }
            PostSpinner::Activity(label) => {
                *plan_spinner = Some(PlanSpinner::Activity(effects::PlanActivitySpinner::start(
                    current_subtask_tag,
                    &label,
                )));
            }
            PostSpinner::None => {}
        }
    }
    // Tick thinking pane header (elapsed time) after draining all events
    if let Some(ref mut pane) = state.plan_thinking_pane {
        pane.tick();
    }
    outcome
}

/// Flush queued background plan updates when the REPL is *not* actively reading a line.
///
/// We intentionally avoid printing plan updates while `rustyline` is in `readline()`.
/// Prompt redraws during active input can interrupt IME composition and cause
/// probabilistic dropped characters for CJK input. The plan update channel is
/// unbounded, so deferring display between prompts is safe and lossless.
/// Truncate skill description for readline completion (≤39 chars + `…` when longer).
///
/// Do not slice by byte index: descriptions may contain multi-byte Unicode (em dash, CJK, …).
fn truncate_skill_desc_for_completion(description: &str) -> String {
    const MAX_CHARS: usize = 39;
    let mut iter = description.chars();
    let preview: String = iter.by_ref().take(MAX_CHARS).collect();
    if iter.next().is_some() {
        format!("{preview}…")
    } else {
        description.to_string()
    }
}

/// Refresh dynamic Tab-completion data (skill names, MCP server names) from
/// the current REPL state so the readline completer offers them.
async fn refresh_dynamic_completions(state: &ReplState) {
    // Skill names from UnifiedSkillRegistry
    let skill_entries: Vec<(String, String)> = {
        let manifests = state.unified_skill_registry.all_manifests();
        manifests
            .into_iter()
            .map(|m| {
                let desc = truncate_skill_desc_for_completion(m.description.as_str());
                (m.name, desc)
            })
            .collect()
    };
    repl_ui::update_skill_completions(skill_entries);

    // MCP server names
    let mcp_entries: Vec<(String, String)> = {
        let mgr = state.mcp_manager.read().await;
        mgr.server_states()
            .into_iter()
            .map(|(name, st)| (name.to_string(), format!("{:?}", st)))
            .collect()
    };
    repl_ui::update_mcp_completions(mcp_entries);
}

/// Push latest plan progress to [`ReplState::task_service`] for `/task list`.
async fn sync_plan_run_task_progress(state: &mut ReplState) {
    let Some(ref tid) = state.plan_run_task_id else {
        return;
    };
    let Some((pct, done, total)) = state.plan_run_task_last_progress else {
        return;
    };
    let Some(ref svc) = state.task_service else {
        return;
    };
    use astra_services::TaskService;
    let _ = svc.update_progress(tid, pct, done, total).await;
}

/// Terminal sync: `/task list` stays `pending` unless we mark the row completed here.
async fn finalize_plan_run_task_after_executor(state: &mut ReplState) {
    let Some(tid) = state.plan_run_task_id.clone() else {
        return;
    };
    let Some(ref svc) = state.task_service else {
        return;
    };
    use astra_services::{TaskService, task_orchestrator::TaskOutcome};
    if let Some(ref err) = state.plan_run_task_last_error {
        let err = err.clone();
        let _ = svc.fail_task(&tid, &err).await;
    } else if let Some(ref report) = state.last_delivery_report {
        let (outcome, pct, done, total) =
            durable_bridge::plan_run_finish_from_delivery_report(report);
        let _ = svc.complete_plan_run(&tid, pct, done, total, outcome).await;
    } else if let Some((pct, done, total)) = state.plan_run_task_last_progress {
        let _ = svc
            .complete_plan_run(&tid, pct, done, total, TaskOutcome::Success)
            .await;
    } else {
        let _ = svc.complete_task(&tid).await;
    }
    state.plan_run_task_id = None;
    state.plan_run_task_last_progress = None;
    state.plan_run_task_last_error = None;
}

/// Returns `true` when the executor sent a terminal event (`PlanCompleted` / `PlanError`).
fn flush_plan_updates_between_prompts(state: &mut ReplState) -> bool {
    if state.plan_handle.is_none() {
        return false;
    }

    let mut plan_spinner: Option<PlanSpinner> = None;
    let mut current_subtask_tag = state.current_plan_subtask_id.clone().unwrap_or_default();
    let outcome = display_plan_updates_live(state, &mut plan_spinner, &mut current_subtask_tag);
    if let Some(spinner) = plan_spinner.take() {
        spinner.stop_clear();
    }
    outcome == PlanMonitorOutcome::Finished
}

fn drain_root_mailbox_into_idle_queue(state: &mut ReplState) {
    let Some(mailbox) = state.root_mailbox.as_mut() else {
        return;
    };
    while let Some(message) = mailbox.try_recv() {
        state.pending_idle_agent_messages.push(message);
    }
}

fn format_idle_agent_message_payload(payload: &astra_runtime::messaging::MessagePayload) -> String {
    use astra_runtime::messaging::{AgentSignal, MessagePayload, RequestType};

    match payload {
        MessagePayload::Text { content, summary } => {
            summary.clone().unwrap_or_else(|| content.clone())
        }
        MessagePayload::Progress {
            turn_index,
            tool_calls,
            status,
            detail,
        } => {
            let detail = detail
                .as_ref()
                .map(|text| format!(" — {text}"))
                .unwrap_or_default();
            format!("progress turn {turn_index}, {tool_calls} tool calls: {status}{detail}")
        }
        MessagePayload::Request { request_type, data } => {
            let request = match request_type {
                RequestType::Shutdown => "shutdown".to_string(),
                RequestType::ToolPermission => "tool_permission".to_string(),
                RequestType::ContextShare => "context_share".to_string(),
                RequestType::Custom(name) => format!("custom:{name}"),
            };
            if data.is_null() {
                format!("request {request}")
            } else {
                format!("request {request}: {data}")
            }
        }
        MessagePayload::Response {
            request_id,
            accepted,
            data,
        } => {
            let data = data
                .as_ref()
                .map(|value| format!(": {value}"))
                .unwrap_or_default();
            format!(
                "response to {request_id}: {}{data}",
                if *accepted { "accepted" } else { "rejected" }
            )
        }
        MessagePayload::Signal(signal) => match signal {
            AgentSignal::Heartbeat => "heartbeat".to_string(),
            AgentSignal::Idle => "idle".to_string(),
            AgentSignal::Stalled { reason } => format!("stalled: {reason}"),
            AgentSignal::Completed { output } => format!("completed: {output}"),
            AgentSignal::Failed { error } => format!("failed: {error}"),
        },
        MessagePayload::Ack { message_id } => format!("acknowledged {message_id}"),
        MessagePayload::Nack { message_id, reason } => {
            let reason = reason
                .as_ref()
                .map(|text| format!(": {text}"))
                .unwrap_or_default();
            format!("rejected {message_id}{reason}")
        }
    }
}

fn flush_idle_agent_messages_between_prompts(state: &mut ReplState) {
    drain_root_mailbox_into_idle_queue(state);
    if state.pending_idle_agent_messages.is_empty() {
        return;
    }

    let pending = std::mem::take(&mut state.pending_idle_agent_messages);
    for message in pending {
        let payload = format_idle_agent_message_payload(&message.payload);
        eprintln!(
            "\n  {} {} {}",
            "mail".cyan(),
            format!("{} -> main", message.from.agent_id).bold(),
            payload
        );
    }
}

/// Clear REPL state when the plan update channel closed without `PlanCompleted` / `PlanError`.
fn cleanup_orphan_plan_executor(state: &mut ReplState, plan_spinner: &mut Option<PlanSpinner>) {
    if let Some(s) = plan_spinner.take() {
        s.stop_clear();
    }
    if let Some(mut pane) = state.plan_thinking_pane.take() {
        pane.clear();
    }
    if let Some(mut h) = state.plan_handle.take() {
        while h.try_recv().is_some() {}
    }
    state.executing_plan = None;
    state.current_plan_subtask_id = None;
    if let Some(tx) = state.pending_approval.take() {
        let _ = tx.send(false);
    }
    eprintln!(
        "\n{}  Plan executor stopped without a final status (channel closed). State cleared.",
        theme::icon_warn()
    );
}

/// Block the REPL until the plan executor finishes, pauses, or errors.
///
/// Replaces the old "fire and forget" background model: the user cannot type
/// at the prompt while a plan is running. First Ctrl-C sends Pause; a second
/// Ctrl-C within two seconds sends Cancel. Approval prompts are read from stdin inline.
async fn run_blocking_plan_monitor(state: &mut ReplState) {
    let mut plan_spinner: Option<PlanSpinner> = None;
    let mut current_subtask_tag = state.current_plan_subtask_id.clone().unwrap_or_default();
    let mut last_ctrl_c: Option<std::time::Instant> = None;
    const CTRL_C_CANCEL_WINDOW: std::time::Duration = std::time::Duration::from_secs(2);

    loop {
        // Drain all currently available updates (non-blocking).
        let outcome = display_plan_updates_live(state, &mut plan_spinner, &mut current_subtask_tag);

        sync_plan_run_task_progress(state).await;

        match outcome {
            PlanMonitorOutcome::Finished => {
                finalize_plan_run_task_after_executor(state).await;
                break;
            }
            PlanMonitorOutcome::Paused => {
                break;
            }
            PlanMonitorOutcome::Continue => {}
        }

        // Executor exited without sending PlanCompleted / PlanError (e.g. task panic).
        if state.plan_handle.as_ref().is_some_and(|h| h.is_finished()) {
            cleanup_orphan_plan_executor(state, &mut plan_spinner);
            sync_plan_run_task_progress(state).await;
            if state.plan_run_task_id.is_some() {
                state.plan_run_task_last_error.get_or_insert(
                    "Plan executor stopped without PlanCompleted/PlanError (channel closed)."
                        .into(),
                );
                finalize_plan_run_task_after_executor(state).await;
            }
            break;
        }

        // Handle pending approval inline (readline is not active).
        if state.pending_approval.is_some() {
            let approved = tokio::task::spawn_blocking(|| {
                use std::io::IsTerminal;
                if std::io::stdin().is_terminal() {
                    inquire::Confirm::new("Approve?")
                        .with_default(false)
                        .prompt()
                        .unwrap_or(false)
                } else {
                    use std::io::Write;
                    let _ = std::io::stderr().flush();
                    eprint!("   Approve? [y/N]: ");
                    let _ = std::io::stderr().flush();
                    let mut line = String::new();
                    if std::io::stdin().read_line(&mut line).is_err() {
                        return false;
                    }
                    let t = line.trim().to_lowercase();
                    t == "y" || t == "yes"
                }
            })
            .await
            .unwrap_or(false);
            if let Some(tx) = state.pending_approval.take() {
                let _ = tx.send(approved);
            }
            continue;
        }

        // Wait for the next event or Ctrl-C.
        tokio::select! {
            biased;
            _ = tokio::signal::ctrl_c() => {
                if let Some(ref handle) = state.plan_handle {
                    let now = std::time::Instant::now();
                    let second_in_window = last_ctrl_c
                        .is_some_and(|t| now.duration_since(t) < CTRL_C_CANCEL_WINDOW);
                    last_ctrl_c = Some(now);
                    if second_in_window {
                        let _ = handle.send_command(plan_executor::PlanCommand::Cancel);
                        if let Some(s) = plan_spinner.take() {
                            s.stop_clear();
                        }
                        if let Some(mut pane) = state.plan_thinking_pane.take() {
                            pane.clear();
                        }
                        eprintln!(
                            "\n{}  Second interrupt — cancelling plan.",
                            "⏹".yellow()
                        );
                        break; // Exit monitor immediately after cancel
                    } else {
                        let _ = handle.send_command(plan_executor::PlanCommand::Pause);
                        if let Some(s) = plan_spinner.take() {
                            s.stop_clear();
                        }
                        if let Some(mut pane) = state.plan_thinking_pane.take() {
                            pane.clear();
                        }
                        eprintln!(
                            "\n{}  Pausing plan… (current subtask will finish first). Press Ctrl-C again within {}s to cancel.",
                            "⏸".yellow(),
                            CTRL_C_CANCEL_WINDOW.as_secs(),
                        );
                    }
                }
            }
            _ = tokio::time::sleep(std::time::Duration::from_millis(30)) => {}
        }
    }

    if let Some(s) = plan_spinner.take() {
        s.stop_clear();
    }
    if let Some(mut pane) = state.plan_thinking_pane.take() {
        pane.clear();
    }

    // Show pause hints when returning to the REPL prompt after a pause.
    if state.plan_handle.is_some() {
        eprint_plan_execution_paused_hints();
    }
}

/// Apply a single trailing update from the plan executor channel.
/// Called when draining remaining messages after PlanCompleted/PlanError.
fn apply_trailing_update(update: plan_executor::PlanUpdate, state: &mut ReplState) {
    use plan_executor::PlanUpdate;
    match update {
        PlanUpdate::HistoryEntry {
            user_msg,
            assistant_msg,
        } => {
            state.history.push((user_msg, assistant_msg));
        }
        PlanUpdate::JournalEvent(event) => {
            if let Some(ref journal) = state.journal {
                let _ = journal.append(&event);
            }
        }
        PlanUpdate::DeliveryReport(report) => {
            state.last_delivery_report = Some(report);
        }
        PlanUpdate::SubtaskTurnResult {
            subtask_id,
            prompt_tokens,
            completion_tokens,
            ..
        } => {
            state.total_prompt_tokens += prompt_tokens;
            state.total_completion_tokens += completion_tokens;
            state.turn += 1;
            state.current_plan_subtask_id = Some(subtask_id);
        }
        _ => {}
    }
}

// ═══════════════════════════════════════════════════ Learning Merge ═══════

fn merge_learning_snapshot(
    json: &str,
    entity_graph: &std::sync::Arc<std::sync::Mutex<astra_runtime::pipeline::entity::EntityGraph>>,
    pattern_library: &std::sync::Arc<
        std::sync::Mutex<astra_runtime::pipeline::pattern::PatternLibrary>,
    >,
    calibrator: &std::sync::Arc<
        std::sync::Mutex<astra_runtime::pipeline::calibration::ProgressiveCalibrator>,
    >,
) {
    if json.trim().is_empty() {
        return;
    }
    match serde_json::from_str::<astra_runtime::pipeline::persistence::LearningSnapshot>(json) {
        Ok(snapshot) => {
            astra_runtime::pipeline::persistence::merge_into_modules(
                &snapshot,
                entity_graph,
                pattern_library,
                calibrator,
            );
            let n = snapshot.entities.len() + snapshot.patterns.len();
            if n > 0 {
                eprintln!(
                    "  {} Merged learning: {} entities, {} patterns",
                    theme::icon_ok(),
                    snapshot.entities.len(),
                    snapshot.patterns.len(),
                );
            }
        }
        Err(e) => {
            eprintln!(
                "{}",
                format!("  ⚠ Learning snapshot format changed (starting fresh): {e}").yellow()
            );
        }
    }
}

// ═══════════════════════════════════════════ Cloud Learning Sync ═══════

/// Result from cloud pull including tool health and version for optimistic locking.
struct CloudPullResult {
    tool_health: Vec<astra_runtime::pipeline::persistence::ToolHealthEntry>,
    version: Option<i64>,
    /// True when MatrixOne was reachable and versioned pull was attempted (may return no row).
    cloud_reachable: bool,
}

/// Try to pull learning state from MatrixOne and merge into live modules.
/// Best-effort: silently skips if cloud is unavailable.
/// Returns tool health entries and cloud version for optimistic locking.
async fn try_cloud_pull(
    profile_name: &str,
    entity_graph: &std::sync::Arc<std::sync::Mutex<astra_runtime::pipeline::entity::EntityGraph>>,
    pattern_library: &std::sync::Arc<
        std::sync::Mutex<astra_runtime::pipeline::pattern::PatternLibrary>,
    >,
    calibrator: &std::sync::Arc<
        std::sync::Mutex<astra_runtime::pipeline::calibration::ProgressiveCalibrator>,
    >,
) -> CloudPullResult {
    let pool = match try_connect_matrixone().await {
        Some(p) => p,
        None => {
            return CloudPullResult {
                tool_health: Vec::new(),
                version: None,
                cloud_reachable: false,
            };
        }
    };
    let svc = astra_services::state_sync::MatrixOneSyncService::new(pool);
    let user_id = std::env::var("MO_USER_ID").unwrap_or_else(|_| "local".to_string());
    match astra_services::state_sync::StateSyncService::pull_learning_versioned(
        &svc,
        &user_id,
        profile_name,
    )
    .await
    {
        Ok(Some(versioned)) => {
            // Parse snapshot to extract tool health before merging entities/patterns
            let cloud_health = serde_json::from_str::<
                astra_runtime::pipeline::persistence::LearningSnapshot,
            >(&versioned.json)
            .map(|s| s.tool_health)
            .unwrap_or_default();
            merge_learning_snapshot(&versioned.json, entity_graph, pattern_library, calibrator);
            eprintln!(
                "{}",
                format!("  ✓ Cloud learning merged (v{})", versioned.version).dim()
            );
            CloudPullResult {
                tool_health: cloud_health,
                version: Some(versioned.version),
                cloud_reachable: true,
            }
        }
        Ok(None) => CloudPullResult {
            tool_health: Vec::new(),
            version: None,
            cloud_reachable: true,
        },
        Err(e) => {
            eprintln!("{}", format!("  ⚠ Cloud pull skipped: {e}").dim());
            CloudPullResult {
                tool_health: Vec::new(),
                version: None,
                cloud_reachable: true,
            }
        }
    }
}

/// Push learning state to cloud with optimistic locking.
/// Returns the new cloud version if successful, or None on conflict/failure.
/// On conflict, the caller should pull fresh data and retry.
async fn try_cloud_push_versioned(
    profile_name: &str,
    entity_graph: &std::sync::Arc<std::sync::Mutex<astra_runtime::pipeline::entity::EntityGraph>>,
    pattern_library: &std::sync::Arc<
        std::sync::Mutex<astra_runtime::pipeline::pattern::PatternLibrary>,
    >,
    calibrator: &std::sync::Arc<
        std::sync::Mutex<astra_runtime::pipeline::calibration::ProgressiveCalibrator>,
    >,
    tool_health: &[astra_runtime::pipeline::persistence::ToolHealthEntry],
    expected_version: Option<i64>,
) -> Option<i64> {
    let pool = match try_connect_matrixone().await {
        Some(p) => p,
        None => return None,
    };
    let snapshot = astra_runtime::pipeline::persistence::export_from_modules_with_health(
        entity_graph,
        pattern_library,
        calibrator,
        tool_health,
    );
    let json = match serde_json::to_string(&snapshot) {
        Ok(j) => j,
        Err(_) => return None,
    };
    let svc = astra_services::state_sync::MatrixOneSyncService::new(pool);
    let user_id = std::env::var("MO_USER_ID").unwrap_or_else(|_| "local".to_string());
    let result = astra_services::state_sync::StateSyncService::push_learning_versioned(
        &svc,
        &user_id,
        profile_name,
        &json,
        snapshot.entities.len() as u32,
        snapshot.patterns.len() as u32,
        snapshot.calibration.is_some(),
        expected_version,
    )
    .await;

    if result.is_conflict {
        eprintln!(
            "{}",
            "  ⚠ Cloud sync conflict (another session updated)".yellow()
        );
        return None;
    }

    if result.success {
        if let Err(e) =
            astra_runtime::pipeline::persistence::save_synced_tool_health(profile_name, tool_health)
        {
            eprintln!(
                "{}",
                format!("  ⚠ Tool-health sync metadata not saved: {e}").dim()
            );
        }
        if let Some(v) = result.new_version {
            return Some(v);
        }
    } else if !result.message.is_empty() {
        eprintln!(
            "{}",
            format!("  ⚠ Cloud push skipped: {}", result.message).dim()
        );
    }
    result.new_version
}

/// Push only changed learning data to cloud using delta sync.
///
/// Delta sync reduces bandwidth by ~90%: full snapshot ~40KB, delta ~2-5KB.
/// Falls back to full push if delta export fails or is empty.
///
/// Returns the new cloud version if successful, None otherwise.
async fn try_cloud_push_delta(
    profile_name: &str,
    entity_graph: &std::sync::Arc<std::sync::Mutex<astra_runtime::pipeline::entity::EntityGraph>>,
    pattern_library: &std::sync::Arc<
        std::sync::Mutex<astra_runtime::pipeline::pattern::PatternLibrary>,
    >,
    calibrator: &std::sync::Arc<
        std::sync::Mutex<astra_runtime::pipeline::calibration::ProgressiveCalibrator>,
    >,
    tool_health_entries: &[astra_runtime::pipeline::persistence::ToolHealthEntry],
    synced_tool_health_entries: &mut Vec<astra_runtime::pipeline::persistence::ToolHealthEntry>,
    expected_version: Option<i64>,
) -> Option<i64> {
    let learning_dirty = astra_runtime::pipeline::persistence::has_dirty_learning_data(
        entity_graph,
        pattern_library,
        calibrator,
    );
    let tool_health_deltas = astra_runtime::pipeline::persistence::export_tool_health_delta(
        tool_health_entries,
        synced_tool_health_entries,
    );

    if !learning_dirty && tool_health_deltas.is_empty() {
        return expected_version;
    }

    let mut delta = astra_runtime::pipeline::persistence::export_dirty_learning_from_modules(
        entity_graph,
        pattern_library,
        calibrator,
    )
    .unwrap_or(astra_runtime::pipeline::persistence::DeltaSnapshot {
        baseline_epoch: 0,
        entity_deltas: Vec::new(),
        pattern_deltas: Vec::new(),
        calibration: None,
        tool_health_deltas: Vec::new(),
        delta_count: 0,
    });

    delta.delta_count += tool_health_deltas.len() as u32;
    delta.tool_health_deltas = tool_health_deltas;

    let delta_json = match serde_json::to_string(&delta) {
        Ok(j) => j,
        Err(_) => return None,
    };

    let pool = match try_connect_matrixone().await {
        Some(p) => p,
        None => return None,
    };

    let svc = astra_services::state_sync::MatrixOneSyncService::new(pool);
    let user_id = std::env::var("MO_USER_ID").unwrap_or_else(|_| "local".to_string());

    let result = astra_services::state_sync::StateSyncService::push_delta(
        &svc,
        &user_id,
        profile_name,
        &delta_json,
        expected_version,
    )
    .await;

    if result.is_conflict {
        eprintln!(
            "{}",
            "  ⚠ Delta sync conflict (another session updated)".yellow()
        );
        return None;
    }

    if result.success {
        astra_runtime::pipeline::persistence::clear_dirty_learning_in_modules(
            entity_graph,
            pattern_library,
            calibrator,
        );
        *synced_tool_health_entries = tool_health_entries.to_vec();
        if let Err(e) = astra_runtime::pipeline::persistence::save_synced_tool_health(
            profile_name,
            synced_tool_health_entries,
        ) {
            eprintln!(
                "{}",
                format!("  ⚠ Tool-health sync metadata not saved: {e}").dim()
            );
        }

        if let Some(v) = result.new_version {
            eprintln!(
                "{}",
                format!(
                    "  ✓ Delta synced to cloud (v{}, {} items, {}B)",
                    v,
                    delta.delta_count,
                    delta_json.len()
                )
                .dim()
            );
            return Some(v);
        }
        eprintln!(
            "{}",
            format!("  ✓ Delta synced ({} items)", delta.delta_count).dim()
        );
    } else {
        eprintln!(
            "{}",
            format!("  ⚠ Delta push skipped: {}", result.message).dim()
        );
    }
    result.new_version
}

/// Pull user preferences from cloud at session start.
/// Merges cloud preferences into local state (cloud-wins). Returns keys merged (for journal audit).
async fn try_cloud_pull_preferences(state: &mut ReplState) -> Vec<String> {
    let pool = match try_connect_matrixone().await {
        Some(p) => p,
        None => return Vec::new(),
    };
    let svc = astra_services::state_sync::MatrixOneSyncService::new(pool);
    let user_id = std::env::var("MO_USER_ID").unwrap_or_else(|_| "local".to_string());
    match astra_services::state_sync::StateSyncService::pull_all_preferences(&svc, &user_id).await {
        Ok(prefs) if !prefs.is_empty() => {
            use astra_services::state_sync::pref_keys;
            let keys: Vec<String> = prefs.iter().map(|(k, _)| k.clone()).collect();
            for (key, value) in &prefs {
                match key.as_str() {
                    pref_keys::EXPLAIN_MODE => {
                        state.explain = match value.as_str() {
                            "on" => ExplainMode::On,
                            "verbose" => ExplainMode::Verbose,
                            _ => ExplainMode::Off,
                        };
                    }
                    pref_keys::BLOCKED_TOOLS => {
                        // Merge cloud-persisted blocked tools into tool_health_entries
                        if let Ok(tools) = serde_json::from_str::<Vec<String>>(value) {
                            let existing: std::collections::HashSet<String> = state
                                .tool_health_entries
                                .iter()
                                .map(|e| e.name.clone())
                                .collect();
                            for tool_name in tools {
                                if !existing.contains(&tool_name) {
                                    state.tool_health_entries.push(
                                        astra_runtime::pipeline::persistence::ToolHealthEntry {
                                            name: tool_name,
                                            total_calls: 3,
                                            total_failures: 3,
                                            failure_rate: 1.0,
                                            last_updated_epoch: 0,
                                        },
                                    );
                                }
                            }
                        }
                    }
                    _ => {}
                }
            }
            eprintln!(
                "{}",
                format!("  ✓ Pulled {} preferences from cloud", prefs.len()).dim()
            );
            keys
        }
        Ok(_) => Vec::new(), // no cloud prefs yet
        Err(e) => {
            eprintln!("{}", format!("  ⚠ Preference pull skipped: {e}").dim());
            Vec::new()
        }
    }
}

fn cloud_pull_warrants_sync_marker(pull: &CloudPullResult, pref_keys: &[String]) -> bool {
    pull.cloud_reachable
        && (pull.version.is_some() || !pull.tool_health.is_empty() || !pref_keys.is_empty())
}

/// When set to `1`, `repl_startup` also journals a sync marker if MatrixOne was reachable but
/// returned no learning rows, tool health, or preferences (audit / connectivity proof).
const ASTRA_JOURNAL_CLOUD_EMPTY_ACK: &str = "ASTRA_JOURNAL_CLOUD_EMPTY_ACK";

fn cloud_pull_empty_ack_desired_for_source(source: &str) -> bool {
    if source == "post_login" {
        return true;
    }
    std::env::var(ASTRA_JOURNAL_CLOUD_EMPTY_ACK).ok().as_deref() == Some("1")
}

fn should_append_cloud_pull_journal(
    pull: &CloudPullResult,
    pref_keys: &[String],
    source: &str,
) -> bool {
    if !pull.cloud_reachable {
        return false;
    }
    if cloud_pull_warrants_sync_marker(pull, pref_keys) {
        return true;
    }
    cloud_pull_empty_ack_desired_for_source(source)
}

fn append_cloud_pull_sync_journal(
    state: &ReplState,
    profile: &str,
    source: &str,
    pull: &CloudPullResult,
    pref_keys: &[String],
) {
    if !should_append_cloud_pull_journal(pull, pref_keys, source) {
        return;
    }
    let Some(sid) = state.session_id.as_deref() else {
        return;
    };
    let reachable_empty_ack =
        pull.cloud_reachable && !cloud_pull_warrants_sync_marker(pull, pref_keys);
    let evt = session_journal::JournalEvent::cloud_pull_sync_marker(
        Some(sid),
        profile,
        source,
        pull.version,
        pull.version.is_some(),
        pull.tool_health.len(),
        pref_keys,
        reachable_empty_ack,
    );
    let Ok(writer) = session_journal::JournalWriter::new(sid) else {
        return;
    };
    if writer.append(&evt).is_ok() {
        repl_turn::enqueue_ingestion_pub(state, &evt);
    }
}

pub(crate) async fn post_auth_cloud_resync(profile: Option<&str>, state: &mut ReplState) {
    let profile_name = profile.unwrap_or("default");
    let (Some(eg), Some(pl), Some(cal)) = (
        state.entity_graph.as_ref(),
        state.pattern_library.as_ref(),
        state.calibrator.as_ref(),
    ) else {
        return;
    };
    let pull = try_cloud_pull(profile_name, eg, pl, cal).await;
    state.cloud_learning_version = pull.version.or(state.cloud_learning_version);
    if !pull.tool_health.is_empty() {
        let (merged, _, _) = astra_runtime::pipeline::persistence::merge_tool_health(
            &state.tool_health_entries,
            &pull.tool_health,
        );
        state.tool_health_entries = merged;
    }
    let pref_keys = try_cloud_pull_preferences(state).await;
    append_cloud_pull_sync_journal(state, profile_name, "post_login", &pull, &pref_keys);
}

/// Push user preferences to cloud at session end.
async fn try_cloud_push_preferences(state: &ReplState) {
    let pool = match try_connect_matrixone().await {
        Some(p) => p,
        None => return,
    };
    let svc = astra_services::state_sync::MatrixOneSyncService::new(pool);
    let user_id = std::env::var("MO_USER_ID").unwrap_or_else(|_| "local".to_string());
    use astra_services::state_sync::{StateSyncService, pref_keys};

    // Collect blocked/deprioritized tools from health entries
    let blocked: Vec<String> = state
        .tool_health_entries
        .iter()
        .filter(|e| e.failure_rate >= 1.0)
        .map(|e| e.name.clone())
        .collect();
    let blocked_json = serde_json::to_string(&blocked).unwrap_or_else(|_| "[]".to_string());

    let prefs = [
        (pref_keys::EXPLAIN_MODE, state.explain.to_string()),
        (pref_keys::BLOCKED_TOOLS, blocked_json),
    ];
    let mut synced = 0u32;
    for (key, value) in &prefs {
        let result = svc.push_preference(&user_id, key, value).await;
        if result.success {
            synced += 1;
        }
    }
    if synced > 0 {
        // Silently succeed — only warn on failure
    }
}

/// Best-effort MatrixOne pool creation for sync operations.
async fn try_connect_matrixone() -> Option<sqlx::Pool<sqlx::MySql>> {
    let host = std::env::var("MATRIXONE_HOST").ok()?;
    let port: u16 = std::env::var("MATRIXONE_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(6001);
    let user = std::env::var("MATRIXONE_USER").unwrap_or_else(|_| "root".to_string());
    let password = std::env::var("MATRIXONE_PASSWORD").unwrap_or_default();
    let database = std::env::var("MATRIXONE_DATABASE").unwrap_or_else(|_| "astra".to_string());
    let url = format!("mysql://{user}:{password}@{host}:{port}/{database}");
    sqlx::mysql::MySqlPoolOptions::new()
        .max_connections(2)
        .acquire_timeout(std::time::Duration::from_secs(3))
        .connect(&url)
        .await
        .ok()
}

// ═══════════════════════════════════════════════════════ Task Commands ════

async fn handle_task_command(
    arg: &str,
    state: &mut ReplState,
    api: &astra_thin_client::ThinClient,
    token: Option<&str>,
) {
    use astra_services::{TaskCreateRequest, TaskService, TaskStatus};

    let svc = match &state.task_service {
        Some(s) => s.clone(),
        None => {
            eprintln!(
                "{}",
                "  ⚠ Task service not available (local-only mode).".yellow()
            );
            eprintln!("{}", "  Use /login to enable cloud task tracking.".dim());
            return;
        }
    };

    let user_id = state.ingestion_user_id.as_deref().unwrap_or("local");
    let session_id = state.session_id.as_deref().unwrap_or("no-session");

    let subcmd = arg.split_whitespace().next().unwrap_or("list");
    let sub_arg = arg.strip_prefix(subcmd).unwrap_or("").trim();

    match subcmd {
        "list" | "" => match svc.list_tasks(user_id, None).await {
            Ok(tasks) if tasks.is_empty() => {
                eprintln!(
                    "  {}",
                    "No tasks. Use /task add <title> to create one.".dim()
                );
            }
            Ok(tasks) => {
                eprintln!(
                    "\n{}",
                    "─── Tasks ───────────────────────────────────────".bold()
                );
                for t in &tasks {
                    let icon = match t.status {
                        TaskStatus::Completed
                            if t.items_total > 0 && t.items_done < t.items_total =>
                        {
                            "△"
                        }
                        TaskStatus::Completed => "✓",
                        TaskStatus::Failed => "✗",
                        TaskStatus::InProgress => "▶",
                        TaskStatus::Paused => "⏸",
                        _ => "○",
                    };
                    let short_id = &t.task_id[..8.min(t.task_id.len())];
                    let status_label = match t.status {
                        TaskStatus::Completed
                            if t.items_total > 0 && t.items_done < t.items_total =>
                        {
                            "partial".to_string()
                        }
                        _ => t.status.as_str().to_string(),
                    };
                    let progress = if t.items_total > 0 {
                        format!(" ({}/{})", t.items_done, t.items_total)
                    } else {
                        String::new()
                    };
                    eprintln!(
                        "  {} {} {} [{}]{}",
                        short_id.dim(),
                        icon,
                        t.title,
                        status_label.cyan(),
                        progress,
                    );
                }
                eprintln!();
            }
            Err(e) => eprintln!("{}", format!("  {} {e}", theme::icon_err()).red()),
        },
        "add" if !sub_arg.is_empty() => {
            match svc
                .create_task(
                    user_id,
                    session_id,
                    TaskCreateRequest {
                        title: sub_arg.to_string(),
                        description: None,
                        plan: None,
                        parent_task_id: None,
                        project_type: None,
                        goal_pattern: None,
                    },
                )
                .await
            {
                Ok(tid) => {
                    let short = &tid[..8.min(tid.len())];
                    eprintln!(
                        "  {} Task created: {} ({})",
                        theme::icon_ok(),
                        sub_arg,
                        short.dim()
                    );
                }
                Err(e) => eprintln!("{}", format!("  {} {e}", theme::icon_err()).red()),
            }
        }
        "done" if !sub_arg.is_empty() => {
            // Find task by prefix match on task_id or title
            match find_task_by_query(&*svc, user_id, sub_arg).await {
                Ok(Some(tid)) => match svc.complete_task(&tid).await {
                    Ok(()) => eprintln!("  {} Task completed: {}", theme::icon_ok(), sub_arg),
                    Err(e) => eprintln!("{}", format!("  {} {e}", theme::icon_err()).red()),
                },
                Ok(None) => {
                    eprintln!("{}", format!("  Task not found: '{sub_arg}'").yellow());
                    eprintln!("{}", "  Use /task list to see available tasks.".dim());
                }
                Err(e) => eprintln!("{}", format!("  {} {e}", theme::icon_err()).red()),
            }
        }
        "status" if !sub_arg.is_empty() => {
            match find_task_by_query(&*svc, user_id, sub_arg).await {
                Ok(Some(tid)) => match svc.get_task(&tid).await {
                    Ok(Some(t)) => {
                        eprintln!(
                            "\n{}",
                            "─── Task Detail ─────────────────────────────────".bold()
                        );
                        eprintln!("  {:<12} {}", "id:".dim(), t.task_id.cyan());
                        eprintln!("  {:<12} {}", "title:".dim(), t.title);
                        let detail_status_label = match t.status {
                            TaskStatus::Completed
                                if t.items_total > 0 && t.items_done < t.items_total =>
                            {
                                "partial"
                            }
                            _ => t.status.as_str(),
                        };
                        eprintln!("  {:<12} {}", "status:".dim(), detail_status_label.cyan());
                        eprintln!("  {:<12} {}%", "progress:".dim(), t.progress_pct);
                        if let Some(ref desc) = t.description {
                            eprintln!("  {:<12} {}", "desc:".dim(), desc);
                        }
                        if let Some(ref plan) = t.plan {
                            eprintln!(
                                "  {:<12} {}/{}",
                                "items:".dim(),
                                t.items_done,
                                t.items_total
                            );
                            for st in &plan.subtasks {
                                let icon = match st.status {
                                    TaskStatus::Completed => "✓",
                                    TaskStatus::InProgress => "▶",
                                    _ => "○",
                                };
                                eprintln!("    {} {}", icon, st.title);
                            }
                        }
                        if let Some(ref err) = t.error_message {
                            eprintln!("  {:<12} {}", "error:".dim(), err.as_str().red());
                        }
                        eprintln!();
                    }
                    Ok(None) => {
                        eprintln!("{}", format!("  Task not found: '{sub_arg}'").yellow());
                        eprintln!("{}", "  Use /task list to see available tasks.".dim());
                    }
                    Err(e) => eprintln!("{}", format!("  {} {e}", theme::icon_err()).red()),
                },
                Ok(None) => {
                    eprintln!("{}", format!("  Task not found: '{sub_arg}'").yellow());
                    eprintln!("{}", "  Use /task list to see available tasks.".dim());
                }
                Err(e) => eprintln!("{}", format!("  {} {e}", theme::icon_err()).red()),
            }
        }
        "run" if !sub_arg.is_empty() => {
            let token_str = match token {
                Some(t) => t.to_string(),
                None => {
                    eprintln!(
                        "{}",
                        "  ⚠ No API token available. Use /login first.".yellow()
                    );
                    return;
                }
            };

            // Create task record
            let task_id = match svc
                .create_task(
                    user_id,
                    session_id,
                    TaskCreateRequest {
                        title: format!(
                            "run: {}",
                            if sub_arg.len() > 60 {
                                format!("{}…", &sub_arg[..60])
                            } else {
                                sub_arg.to_string()
                            }
                        ),
                        description: Some(sub_arg.to_string()),
                        plan: None,
                        parent_task_id: None,
                        project_type: None,
                        goal_pattern: None,
                    },
                )
                .await
            {
                Ok(tid) => tid,
                Err(e) => {
                    eprintln!("{}", format!("  {} {e}", theme::icon_err()).red());
                    return;
                }
            };
            let short_id = task_id[..8.min(task_id.len())].to_string();

            // Clone owned values for the background task
            let api_clone = api.clone();
            let prompt = sub_arg.to_string();
            let bg_session_id = state.session_id.clone();
            let bg_model = state.model.clone();
            let bg_history = state.history.clone();
            let bg_unified_skill_registry = state.unified_skill_registry.clone();
            let bg_skill_search = state.skill_search.clone();
            let bg_messaging_metrics = state.messaging_metrics.clone();
            let bg_agent_spawner = state.agent_spawner.clone();
            let bg_delegation_engine = state.delegation_engine.clone();
            let svc_clone = svc.clone();
            let workspace_root = std::env::current_dir().unwrap_or_default();
            let bg_root_agent_id = format!("task-{task_id}");

            eprintln!(
                "  {} Background task started: {} ({})",
                "▶".cyan(),
                if sub_arg.len() > 50 {
                    format!("{}…", &sub_arg[..50])
                } else {
                    sub_arg.to_string()
                },
                short_id.dim()
            );
            eprintln!(
                "  {}",
                "Use /task status or /task result to check progress.".dim()
            );

            // Spawn background task
            let bg_task_id = task_id.clone();
            tokio::spawn(async move {
                // Mark in-progress
                let _ = svc_clone
                    .update_status(&bg_task_id, TaskStatus::InProgress)
                    .await;

                // Create fresh auto-approve permission manager for background
                let mut perm_manager = PermissionManager::with_project(true, &workspace_root);
                let mut skill_qt = astra_runtime::skills::quality::SkillQualityTracker::new();

                // Create a fresh tool selector for the background task
                let (selector, _modules) = create_tool_selector_quiet(&api_clone, None);

                let result = stream_chat_sse(ChatTurnParams {
                    api: &api_clone,
                    token: &token_str,
                    message: &prompt,
                    session_id: bg_session_id.as_deref(),
                    model: bg_model.as_deref(),
                    explain: ExplainMode::Off,
                    render_md: false,
                    history: &bg_history,
                    perm_manager: &mut perm_manager,
                    verbose_mode: false,
                    quiet: true,
                    suppress_intermediate_output: true,
                    selector: &*selector,
                    recent_tools: &[],
                    tool_health_entries: &[],
                    unified_skill_registry: &bg_unified_skill_registry,
                    plan_only_chat: false,
                    hide_streaming_assistant_text: true,
                    is_plan_subtask: false,
                    plan_subtask_id: None,
                    delegation_engine: bg_delegation_engine.clone(),
                    cancel_token: None,
                    plan_assemble_line_release: None,
                    stream_event_tx: None,
                    approval_request_tx: None,
                    mcp_manager: None,
                    skill_search: &bg_skill_search,
                    skill_quality_tracker: &mut skill_qt,
                    discovered_skills: None,
                    messaging_metrics: bg_messaging_metrics.clone(),
                    agent_spawner: bg_agent_spawner.clone(),
                    root_agent_id: Some(bg_root_agent_id.as_str()),
                    root_mailbox_slot: None,
                })
                .await;

                let short = &bg_task_id[..8.min(bg_task_id.len())];
                match result {
                    Ok(sr) => {
                        // Store result in checkpoint state map
                        let mut state_map = serde_json::Map::new();
                        state_map.insert(
                            "full_text".to_string(),
                            serde_json::Value::String(sr.full_text.clone()),
                        );
                        state_map.insert(
                            "prompt_tokens".to_string(),
                            serde_json::json!(sr.prompt_tokens),
                        );
                        state_map.insert(
                            "completion_tokens".to_string(),
                            serde_json::json!(sr.completion_tokens),
                        );
                        state_map.insert(
                            "tool_calls_count".to_string(),
                            serde_json::json!(sr.tool_calls_count),
                        );
                        let _ = svc_clone
                            .save_checkpoint(
                                &bg_task_id,
                                &astra_services::task_orchestrator::TaskCheckpoint {
                                    active_subtask_id: None,
                                    turn: 0,
                                    session_id: bg_session_id.clone(),
                                    state: state_map,
                                },
                            )
                            .await;
                        let _ = svc_clone.complete_task(&bg_task_id).await;
                        eprintln!(
                            "\n  {} Background task {} completed. Use /task result {} to view.",
                            theme::icon_ok(),
                            short.cyan(),
                            short.cyan()
                        );
                    }
                    Err(e) => {
                        let _ = svc_clone.fail_task(&bg_task_id, &e.error).await;
                        eprintln!(
                            "\n  {} Background task {} failed: {}",
                            theme::icon_err(),
                            short.cyan(),
                            e.error.red()
                        );
                    }
                }
            });
        }
        "result" if !sub_arg.is_empty() => {
            // Show the full result of a background task
            match find_task_by_query(&*svc, user_id, sub_arg).await {
                Ok(Some(tid)) => match svc.get_task(&tid).await {
                    Ok(Some(t)) => {
                        let short = &t.task_id[..8.min(t.task_id.len())];
                        eprintln!(
                            "\n{}",
                            format!("─── Task Result ({short}) ─────────────────────────").bold()
                        );
                        eprintln!("  {:<12} {}", "title:".dim(), t.title);
                        eprintln!("  {:<12} {}", "status:".dim(), t.status.as_str().cyan());
                        if let Some(ref err) = t.error_message {
                            eprintln!("  {:<12} {}", "error:".dim(), err.as_str().red());
                        }
                        // Print checkpoint data (the full_text from the agent)
                        let mut found_result = false;
                        if let Some(ref cp) = t.checkpoint {
                            if let Some(full_text) =
                                cp.state.get("full_text").and_then(|v| v.as_str())
                            {
                                found_result = true;
                                eprintln!();
                                eprintln!("{full_text}");
                                if let Some(tokens) =
                                    cp.state.get("prompt_tokens").and_then(|v| v.as_u64())
                                {
                                    let comp = cp
                                        .state
                                        .get("completion_tokens")
                                        .and_then(|v| v.as_u64())
                                        .unwrap_or(0);
                                    let tools = cp
                                        .state
                                        .get("tool_calls_count")
                                        .and_then(|v| v.as_u64())
                                        .unwrap_or(0);
                                    eprintln!(
                                        "\n  {}",
                                        format!("tokens: {tokens}→/{comp}← | tools: {tools}").dim()
                                    );
                                }
                            }
                        }
                        if !found_result {
                            match t.status {
                                TaskStatus::InProgress | TaskStatus::Pending => {
                                    eprintln!("  {}", "Task is still running…".yellow());
                                }
                                _ => {
                                    eprintln!("  {}", "No result data available.".dim());
                                }
                            }
                        }
                        eprintln!();
                    }
                    Ok(None) => {
                        eprintln!("{}", format!("  Task not found: '{sub_arg}'").yellow());
                    }
                    Err(e) => eprintln!("{}", format!("  {} {e}", theme::icon_err()).red()),
                },
                Ok(None) => {
                    eprintln!("{}", format!("  Task not found: '{sub_arg}'").yellow());
                    eprintln!("{}", "  Use /task list to see available tasks.".dim());
                }
                Err(e) => eprintln!("{}", format!("  {} {e}", theme::icon_err()).red()),
            }
        }
        _ => {
            eprintln!(
                "  Usage: /task [list | add <title> | done <id> | status <id> | run <prompt> | result <id>]"
            );
        }
    }
}

/// Find a task by prefix match on task_id or substring match on title.
async fn find_task_by_query(
    svc: &dyn astra_services::TaskService,
    user_id: &str,
    query: &str,
) -> Result<Option<String>, String> {
    let tasks = svc.list_tasks(user_id, None).await?;
    // Exact or prefix match on task_id
    if let Some(t) = tasks
        .iter()
        .find(|t| t.task_id == query || t.task_id.starts_with(query))
    {
        return Ok(Some(t.task_id.clone()));
    }
    // Substring match on title (case-insensitive)
    let q_lower = query.to_lowercase();
    if let Some(t) = tasks
        .iter()
        .find(|t| t.title.to_lowercase().contains(&q_lower))
    {
        return Ok(Some(t.task_id.clone()));
    }
    Ok(None)
}

// ══════════════════════════════════════════════════════ Slash Commands ════

/// Returns `true` when the REPL should exit.
async fn handle_slash_command(
    line: &str,
    api: &astra_thin_client::ThinClient,
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

        "/keys" => print_keyboard_shortcuts(),

        "/model" if arg.is_empty() => {
            let Some(tok) = token else {
                eprintln!("{}", "  Not logged in. Use /login.".yellow());
                return Ok(false);
            };
            let body = api.get_models_text(tok).await.map_err(map_thin_err)?;
            {
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
                    // Cache pricing: prefer API-provided, fall back to built-in table
                    state.cached_pricing = extract_pricing_for_model(&models, &chosen)
                        .unwrap_or_else(|| fallback_pricing(&chosen));
                    eprintln!(
                        "  {} {}",
                        theme::icon_ok(),
                        format!("Model set to: {chosen}").green()
                    );
                } else {
                    eprintln!("{}", "  Cancelled.".dim());
                }
            }
        }

        "/model" => {
            state.model = Some(arg.to_string());
            state.cached_pricing = fallback_pricing(arg);
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

        "/checkpoint" => match create_manual_repl_checkpoint(state, arg) {
            Ok(msg) => {
                eprintln!("  {} {}", theme::icon_ok(), msg.green());
            }
            Err(e) => {
                eprintln!("  {}", e.yellow());
            }
        },

        "/debug" => handle_debug_command(arg, state),

        "/style" => {
            handle_style_command(arg);
        }

        "/history" | "/grep" | "/review" | "/copy" | "/diagnostics" | "/context" | "/version"
        | "/rewind" | "/turn" | "/report" => {
            handle_info_command(cmd, arg, api, state, token).await?;
        }

        "/skill" => {
            handle_skill_command(arg, api, state, token).await?;
        }

        "/mcp" => {
            slash_mcp::handle_mcp_command(arg, state).await?;
        }

        "/team" => {
            slash_team::handle_team_command(arg, state).await;
        }

        "/messaging" => {
            handle_messaging_command(arg, state);
        }

        "/agent" => {
            let ctx = slash_agent::AgentCommandContext {
                spawner: state.agent_spawner.clone(),
            };
            slash_agent::handle_agent_command(arg, &ctx);
        }

        "/register" | "/login" | "/logout" | "/memory-setup" => {
            handle_account_command(cmd, arg, api, profile, state).await?;
        }

        "/allow" | "/yolo" => {
            use permission_manager::PermissionMode;
            if cmd == "/yolo" {
                state.perm_manager.set_mode(PermissionMode::Auto);
                eprintln!(
                    "  {} {} All tools auto-approved for this session.",
                    "⚡".yellow(),
                    "YOLO mode!".bold().yellow()
                );
                eprintln!(
                    "  {}",
                    "  Use /allow prompt to restore confirmation prompts.".dim()
                );
            } else {
                match arg {
                    "" => {
                        // Cycle: Prompt → Auto → Deny → Prompt
                        let next = match state.perm_manager.mode() {
                            PermissionMode::Prompt => PermissionMode::Auto,
                            PermissionMode::Auto => PermissionMode::Deny,
                            PermissionMode::Deny => PermissionMode::Prompt,
                        };
                        state.perm_manager.set_mode(next);
                        eprintln!(
                            "  {} Permission mode → {}",
                            theme::icon_info(),
                            next.to_string().cyan()
                        );
                    }
                    "all" => {
                        state.perm_manager.set_mode(PermissionMode::Auto);
                        eprintln!(
                            "  {} Permission mode → {} (all tools auto-approved)",
                            "⚡".yellow(),
                            "auto".cyan()
                        );
                    }
                    "rules" | "status" => {
                        let summary = state.perm_manager.rules_summary();
                        eprint!("{summary}");
                    }
                    _ => match arg.parse::<PermissionMode>() {
                        Ok(mode) => {
                            state.perm_manager.set_mode(mode);
                            eprintln!(
                                "  {} Permission mode → {}",
                                theme::icon_info(),
                                mode.to_string().cyan()
                            );
                        }
                        Err(_) => {
                            eprintln!(
                                "  {} Unknown mode '{}'. Use: auto, prompt, deny, all, rules",
                                theme::icon_warn(),
                                arg
                            );
                        }
                    },
                }
            }
        }

        "/instructions" => match arg {
            "" | "show" => {
                if let Some(ref pi) = state.project_instructions {
                    let lines = pi.lines().count();
                    eprintln!(
                        "  {} Project instructions ({lines} lines):\n",
                        theme::icon_info()
                    );
                    for line in pi.lines() {
                        eprintln!("  {line}");
                    }
                    eprintln!();
                } else {
                    eprintln!("  {} No project instructions loaded.", theme::icon_info());
                    eprintln!(
                        "  {}",
                        "  Create .astra/instructions.md in your project root to add instructions."
                            .dim()
                    );
                }
            }
            "reload" => {
                let no_inst = std::env::var("ASTRA_NO_INSTRUCTIONS")
                    .map(|v| v == "1")
                    .unwrap_or(false);
                if no_inst {
                    eprintln!(
                        "  {} Instructions disabled (--no-instructions).",
                        theme::icon_warn()
                    );
                } else if let Some(instructions) = discover_project_instructions() {
                    let lines = instructions.lines().count();
                    state.project_instructions = Some(instructions);
                    eprintln!(
                        "  {} Reloaded project instructions ({lines} lines).",
                        theme::icon_ok()
                    );
                } else {
                    state.project_instructions = None;
                    eprintln!("  {} No .astra/instructions.md found.", theme::icon_info());
                }
            }
            "off" => {
                state.project_instructions = None;
                eprintln!(
                    "  {} Project instructions disabled for this session.",
                    theme::icon_ok()
                );
            }
            _ => {
                eprintln!(
                    "  {} Usage: /instructions [show|reload|off]",
                    theme::icon_warn()
                );
            }
        },

        "/clear" | "/explain" | "/verbose" | "/compact" | "/reflect" | "/undo" => {
            handle_state_command(
                cmd,
                arg,
                StateCommandContext {
                    api,
                    profile,
                    token,
                    selector,
                },
                state,
            )
            .await?;
        }

        "/memory" | "/plan" => {
            handle_memory_domain_command(cmd, arg, api, state, token).await?;
        }

        "/task" => {
            handle_task_command(arg, state, api, token).await;
        }

        "/resume" => {
            handle_resume_command(arg, profile, state).await;
        }

        "/stats" => {
            handle_stats_command(arg, state);
        }

        "/cost" => {
            handle_cost_command(arg, state);
        }

        "/bug" => {
            handle_bug_command(arg, state);
        }

        "/tools" => {
            handle_tools_command(state);
        }

        "/health" => {
            handle_health_command(arg, state).await;
        }

        "/sync" => {
            handle_sync_command(arg, state).await;
        }

        "/diff" => {
            let root = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
            diff_presenter::run_diff_command(&root, arg, cli_utils::terminal_width_usize());
        }

        "/learn" => {
            handle_learn_command(arg, state);
        }

        "/exit" | "/quit" => {
            eprintln!("{}", "  Goodbye.".dim());
            // Journal + ingestion: session end (same as Ctrl+D path)
            if let Some(ref j) = state.journal {
                let end_event = session_journal::JournalEvent::session_end(
                    state.session_id.as_deref(),
                    state.turn,
                );
                let _ = j.append(&end_event);
                repl_turn::enqueue_ingestion_pub(state, &end_event);
            }
            if let Some(mc) = state.matrix_runtime.as_ref() {
                mc.shutdown_ingestion_and_wait().await;
            }
            clear_panic_guard();
            if state.turn > 0
                && let Some(ref sid) = state.session_id
            {
                let short = prefix_chars(sid, 8);
                eprintln!(
                    "{}",
                    format!("  Session {short}… saved. To resume: /resume {sid}").dim()
                );
            }
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
    api: &astra_thin_client::ThinClient,
    profile: Option<&str>,
    initial_model: Option<&str>,
    resume_session_id: Option<&str>,
) -> Result<(), String> {
    // Try silent auth (validate/refresh token) but don't block entry.
    // If not authenticated, user can still explore — operations that need
    // auth will prompt "Not logged in. Use /login."
    try_silent_auth(api, profile).await;

    let (editor, hist_path) = build_repl_editor()?;
    let mut readline = readline_actor::ReadlineActor::spawn(editor)?;
    let mut state = initialize_repl_state(profile, initial_model);

    // Install panic hook to write session_end on unexpected crashes.
    install_session_panic_hook();
    // Install SIGTERM handler so `kill <pid>` writes session_end before exit.
    install_sigterm_handler();

    // Apply resume session if requested (-c or -r)
    if let Some(sid) = resume_session_id {
        state.session_id = Some(sid.to_string());
        eprintln!(
            "{}",
            format!("  Resuming session {}", truncate_str(sid, 12)).cyan()
        );
    }

    // --session-id: override with explicit session UUID
    if let Ok(sid) = std::env::var("ASTRA_SESSION_ID") {
        state.session_id = Some(sid.clone());
        eprintln!(
            "{}",
            format!("  Using session {}", truncate_str(&sid, 12)).cyan()
        );
    }

    // --name: set session display name
    if let Ok(name) = std::env::var("ASTRA_SESSION_NAME") {
        state.session_name = Some(name);
    }

    // --yes: warn about auto-approve mode
    if state.perm_manager.mode() == permission_manager::PermissionMode::Auto {
        eprintln!(
            "{}",
            "  ⚠ Auto-approve mode: all tool calls will execute without confirmation.".yellow()
        );
    }

    // Load project instructions from .astra/instructions.md (unless --no-instructions)
    let no_instructions = std::env::var("ASTRA_NO_INSTRUCTIONS")
        .map(|v| v == "1")
        .unwrap_or(false);
    if !no_instructions {
        if let Some(instructions) = discover_project_instructions() {
            let lines = instructions.lines().count();
            eprintln!(
                "  {} {}",
                theme::icon_ok(),
                format!("Loaded project instructions ({lines} lines)").dim()
            );
            state.project_instructions = Some(instructions);
        }
    }

    // Session lifecycle maintenance: compress old journals and delete expired sessions.
    // Non-blocking, best-effort — errors are silently ignored.
    {
        let ttl_days: u64 = std::env::var("ASTRA_SESSION_TTL_DAYS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(30);
        let compress_days: u64 = std::env::var("ASTRA_JOURNAL_COMPRESS_DAYS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(7);
        let maint = session_journal::run_session_maintenance(ttl_days, compress_days);
        if maint.sessions_deleted > 0 || maint.journals_compressed > 0 {
            let mut parts = Vec::new();
            if maint.sessions_deleted > 0 {
                parts.push(format!(
                    "{} expired sessions removed",
                    maint.sessions_deleted
                ));
            }
            if maint.journals_compressed > 0 {
                parts.push(format!("{} journals compressed", maint.journals_compressed));
            }
            eprintln!("  {} {}", theme::icon_ok(), parts.join(", ").dim());
        }
    }

    // Load persisted skill quality data from previous sessions
    let skill_quality_path = dirs::config_dir()
        .unwrap_or_else(|| std::path::PathBuf::from("."))
        .join("astra")
        .join("skill_quality.json");
    state.skill_quality_tracker =
        astra_runtime::skills::quality::SkillQualityTracker::load(&skill_quality_path);

    // Load pinned skills from previous sessions
    let pinned_skills_path = dirs::config_dir()
        .unwrap_or_else(|| std::path::PathBuf::from("."))
        .join("astra")
        .join("pinned_skills.json");
    if let Ok(data) = std::fs::read_to_string(&pinned_skills_path) {
        match serde_json::from_str::<std::collections::HashSet<String>>(&data) {
            Ok(set) => state.pinned_skills = set,
            Err(e) => eprintln!("⚠ Failed to parse pinned_skills.json: {e}"),
        }
    }

    // Session-scoped quality tracker: tools that work well get boosted over time
    let quality_tracker = std::sync::Arc::new(std::sync::Mutex::new(
        tool_registry::ToolQualityTracker::new(),
    ));
    // Session-scoped confidence calibrator: thresholds adapt to correction rates
    let confidence_calibrator =
        std::sync::Arc::new(astra_runtime::turn::routing_metrics::ConfidenceCalibrator::default());
    let (selector, pipeline_modules) = create_tool_selector_with_quality(
        api,
        profile,
        Some(quality_tracker),
        Some(confidence_calibrator),
    );

    // Load cross-session learning state (entity graph, patterns, calibration, tool health)
    let profile_name = profile.unwrap_or("default");
    let (cross_session_health_entries, cloud_pull_result, pref_keys_after_pull) = {
        let loaded = astra_runtime::pipeline::persistence::load_learning_state(
            profile_name,
            &pipeline_modules.entity_graph,
            &pipeline_modules.pattern_library,
            &pipeline_modules.calibrator,
        );
        if loaded {
            eprintln!(
                "  {} {}",
                theme::icon_ok(),
                "Loaded learning state from prior sessions".dim()
            );
        }
        // Load tool health for cross-session error budgets
        let mut cross_session_health_entries =
            astra_runtime::pipeline::persistence::load_tool_health(profile_name);
        state.synced_tool_health_entries =
            astra_runtime::pipeline::persistence::load_synced_tool_health(profile_name);
        if !cross_session_health_entries.is_empty() {
            eprintln!(
                "{}",
                format!(
                    "  ✓ Restored tool health ({} tools tracked)",
                    cross_session_health_entries.len()
                )
                .dim()
            );
        }
        // Try to merge cloud learning (best-effort, returns cloud tool health and version)
        let cloud_pull_result = try_cloud_pull(
            profile_name,
            &pipeline_modules.entity_graph,
            &pipeline_modules.pattern_library,
            &pipeline_modules.calibrator,
        )
        .await;
        // Store cloud version for optimistic locking on push
        state.cloud_learning_version = cloud_pull_result.version;
        // Merge cloud tool health: timestamp-based conflict resolution
        if !cloud_pull_result.tool_health.is_empty() {
            let (merged, cloud_wins, cloud_only) =
                astra_runtime::pipeline::persistence::merge_tool_health(
                    &cross_session_health_entries,
                    &cloud_pull_result.tool_health,
                );
            cross_session_health_entries = merged;
            if cloud_wins > 0 || cloud_only > 0 {
                let mut parts = Vec::new();
                if cloud_wins > 0 {
                    parts.push(format!("{cloud_wins} updated from cloud"));
                }
                if cloud_only > 0 {
                    parts.push(format!("{cloud_only} new from cloud"));
                }
                eprintln!(
                    "{}",
                    format!("  ✓ Merged tool health: {}", parts.join(", ")).dim()
                );
            }
        }
        // Try to pull user preferences from cloud
        let pref_keys = try_cloud_pull_preferences(&mut state).await;
        (cross_session_health_entries, cloud_pull_result, pref_keys)
    };
    state.tool_health_entries = cross_session_health_entries.clone();
    if state.synced_tool_health_entries.is_empty() {
        state.synced_tool_health_entries = cross_session_health_entries;
    }

    // ── Matrix pool + ingestion + sync orchestrator (single bundle) ─────────
    {
        let settings = astra_runtime::matrix_settings_from_env();
        state.matrix_runtime = match SharedPool::new(&settings).await {
            Ok(pool) => {
                let user_id = std::env::var("MO_USER_ID").unwrap_or_else(|_| "local".to_string());
                let th =
                    std::sync::Arc::new(std::sync::Mutex::new(state.tool_health_entries.clone()));
                let lease = std::sync::Arc::new(astra_services::TaskLeaseHoldCache::default());
                Some(std::sync::Arc::new(
                    astra_runtime::MatrixCloudRuntime::attach(
                        pool,
                        profile.unwrap_or("default"),
                        &user_id,
                        pipeline_modules.entity_graph.clone(),
                        pipeline_modules.pattern_library.clone(),
                        pipeline_modules.calibrator.clone(),
                        th,
                        state.cloud_learning_version,
                        lease,
                    ),
                ))
            }
            Err(_) => None,
        };
        // Register runtime in global so SIGTERM handler can flush ingestion.
        if let Some(ref mc) = state.matrix_runtime {
            let _ = SIGTERM_RUNTIME.set(mc.clone());
        }
    }

    // Store pipeline learning modules for /learn command and learning feedback loop
    state.pattern_library = Some(pipeline_modules.pattern_library.clone());
    state.entity_graph = Some(pipeline_modules.entity_graph.clone());
    state.calibrator = Some(pipeline_modules.calibrator.clone());
    // Store skill registry and MCP manager from pipeline initialization
    state.unified_skill_registry = pipeline_modules.unified_skill_registry.clone();
    state.mcp_manager = pipeline_modules.mcp_manager.clone();

    append_cloud_pull_sync_journal(
        &state,
        profile_name,
        "repl_startup",
        &cloud_pull_result,
        &pref_keys_after_pull,
    );

    let profile_name_str = profile_name.to_string();

    // Pre-flight: check if server has any LLM models configured
    if let Some(token) = current_access_token(profile) {
        let has_models = check_server_has_models(api, &token).await;
        if !has_models {
            state.model = Some("⚠ none".to_string());
        }
    }

    print_repl_banner(profile, &state);

    // Warn if HTTP proxy is set — local service calls bypass it, but users
    // may see confusing 502s when testing with curl/wget.
    if let Ok(proxy) = std::env::var("http_proxy").or_else(|_| std::env::var("HTTP_PROXY"))
        && !proxy.is_empty()
    {
        eprintln!(
            "  {}  {} {}",
            theme::icon_warn(),
            "HTTP proxy detected:".yellow(),
            proxy.dim()
        );
        eprintln!(
            "     {}",
            "Agent bypasses proxy for local calls. For curl: use --noproxy '*'".dim()
        );
    }

    let mut edge_heartbeat_task: Option<tokio::task::JoinHandle<()>> = None;
    if let Some(ref tok) = current_access_token(profile) {
        edge_heartbeat_task = register_and_start_heartbeat(api, tok).await;
    }

    if state.model.as_deref() == Some("⚠ none") {
        eprintln!(
            "  {}  {}",
            theme::icon_warn(),
            "No LLM model configured on server. Run: astra-admin model add".yellow()
        );
        eprintln!();
        state.model = None; // reset so chat uses "auto" for actual requests
    }

    // Seed dynamic Tab-completion with available skills / MCP servers.
    refresh_dynamic_completions(&state).await;

    // ── Wire multi-agent runtime (delegation + dynamic spawning) ──────────────
    if let Some(token) = current_access_token(profile) {
        initialize_multi_agent_runtime(&mut state, api, token).await;
    }

    // ── Main loop ─────────────────────────────────────────────────────────────
    loop {
        flush_idle_agent_messages_between_prompts(&mut state);
        let plan_terminal = flush_plan_updates_between_prompts(&mut state);
        sync_plan_run_task_progress(&mut state).await;
        if plan_terminal {
            finalize_plan_run_task_after_executor(&mut state).await;
        }
        // Refresh Tab-completion data (skills/MCP may change mid-session).
        refresh_dynamic_completions(&state).await;
        let current_token = current_access_token(profile);

        // Keep readline prompt TEXT as ASCII-only. Unicode characters (⏸, 🔄, ❯)
        // have ambiguous display widths that break cursor tracking for CJK input.
        // ANSI color codes are safe — rustyline's calculate_position() treats them
        // as width=0 so cursor math is unaffected.
        if let Some(ref dev) = state.skill_dev {
            eprintln!(
                "  \u{1f527} {}",
                format!("Skill dev: {}", dev.name).cyan().dim()
            );
        }
        // Single source for "plan run in progress": live handle and/or persisted background flag.
        let plan_run_active = plan_execution_ui_active(&state);
        let prompt_str = if let Some(ref ps) = state.plan_mode {
            if ps.goal.is_empty() {
                if plan_run_active {
                    "\x1b[1;33mplan*>\x1b[0m ".to_string()
                } else {
                    theme::PROMPT_PLAN.to_string()
                }
            } else {
                let short_goal: String = ps.goal.chars().take(20).collect();
                let suffix = if ps.goal.len() > 20 { "…" } else { "" };
                let star = if plan_run_active { "*" } else { "" };
                format!("\x1b[1;33mplan{star}[{short_goal}{suffix}]>\x1b[0m ")
            }
        } else if state.executing_plan.is_some() {
            theme::PROMPT_PAUSE.to_string()
        } else if plan_run_active {
            theme::PROMPT_BG.to_string()
        } else if state.chat_plan_only {
            theme::PROMPT_PLAN_ONLY.to_string()
        } else {
            theme::PROMPT_DEFAULT.to_string()
        };

        // ── Send readline request to actor thread ────────────────────
        readline.request_readline(prompt_str);

        // Wait for user input. Do NOT flush plan updates during active readline — writing
        // to stderr (\r\x1b[2K) while rustyline owns the terminal disrupts cursor tracking
        // for wide (CJK) characters, causing the last character to visually disappear.
        // Plan updates are buffered and flushed between prompts instead.
        enum PromptWaitOutcome {
            Readline(Result<String, ReadlineError>, Option<String>),
            IdleAgentMessage(Option<std::sync::Arc<astra_runtime::messaging::AgentMessage>>),
        }
        let (readline_result, pending_execute): (Result<String, ReadlineError>, Option<String>) = loop {
            let outcome = if let Some(mailbox) = state.root_mailbox.as_ref() {
                tokio::select! {
                    result = readline.recv() => match result {
                        Some(readline_actor::ReadlineResponse::Line { result, pending_execute }) => {
                            PromptWaitOutcome::Readline(result, pending_execute)
                        }
                        None => PromptWaitOutcome::Readline(Err(ReadlineError::Eof), None),
                    },
                    message = mailbox.recv() => PromptWaitOutcome::IdleAgentMessage(message),
                }
            } else {
                match readline.recv().await {
                    Some(readline_actor::ReadlineResponse::Line {
                        result,
                        pending_execute,
                    }) => PromptWaitOutcome::Readline(result, pending_execute),
                    None => PromptWaitOutcome::Readline(Err(ReadlineError::Eof), None),
                }
            };

            match outcome {
                PromptWaitOutcome::Readline(result, pending_execute) => {
                    break (result, pending_execute);
                }
                PromptWaitOutcome::IdleAgentMessage(Some(message)) => {
                    state.pending_idle_agent_messages.push(message);
                }
                PromptWaitOutcome::IdleAgentMessage(None) => {
                    state.root_mailbox = None;
                }
            }
        };
        flush_idle_agent_messages_between_prompts(&mut state);

        // ── Process readline result ──────────────────────────────────
        match readline_result {
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
                readline.add_history(line.clone());

                // ── Handle pending approval from background plan executor ──
                if let Some(tx) = state.pending_approval.take() {
                    let trimmed = line.trim().to_lowercase();
                    let approved = trimmed == "y" || trimmed == "yes" || trimmed == "a";
                    let autorun = trimmed == "!" || trimmed == "all" || trimmed == "yolo";
                    let denied = trimmed == "n" || trimmed == "no";
                    if approved || autorun || denied {
                        let _ = tx.send(approved || autorun);
                        if autorun {
                            state
                                .perm_manager
                                .set_mode(permission_manager::PermissionMode::Auto);
                            eprintln!(
                                "  {} {} All tools auto-approved for this session.",
                                "⚡".yellow(),
                                "Auto-run enabled!".bold().yellow()
                            );
                            eprintln!(
                                "  {}",
                                "  Use /allow prompt to restore confirmation prompts.".dim()
                            );
                        } else if approved {
                            eprintln!("  {} Approved", theme::icon_ok());
                        } else {
                            eprintln!("  {} Denied", theme::icon_err());
                        }
                        continue;
                    } else {
                        // Unrecognized — treat as deny and fall through
                        let _ = tx.send(false);
                        eprintln!(
                            "  {} Unrecognized response, treating as denied",
                            theme::icon_err()
                        );
                        // Fall through to normal input handling
                    }
                }

                if line.starts_with('/') {
                    // If Enter was pressed in the picker, the selected command is
                    // stored in pending-execute (captured by readline actor thread).
                    let dispatch_line_owned = pending_execute.unwrap_or_else(|| line.clone());
                    let dispatch_line = dispatch_line_owned.as_str();
                    let should_exit = handle_slash_command(
                        dispatch_line,
                        api,
                        profile,
                        &mut state,
                        current_token.as_deref(),
                        &*selector,
                    )
                    .await?;
                    if should_exit {
                        break;
                    }
                    // Merge learning snapshot if /resume deposited one
                    if let Some(json) = state.learning_snapshot.take() {
                        merge_learning_snapshot(
                            &json,
                            &pipeline_modules.entity_graph,
                            &pipeline_modules.pattern_library,
                            &pipeline_modules.calibrator,
                        );
                    }

                    // If /plan auto triggered execution, start the background executor
                    if state.executing_plan.is_some() && state.plan_mode.is_none() {
                        start_and_monitor_plan(&mut state, current_token.as_deref(), api, profile)
                            .await?;
                    }
                } else if state.plan_mode.is_some() {
                    // Plan mode: handle input as plan editing
                    if let Err(e) = handle_plan_mode_input(
                        line.clone(),
                        current_token.as_deref(),
                        &mut state,
                        api,
                    )
                    .await
                    {
                        state.plan_resume_pending = false;
                        return Err(e);
                    }

                    // If plan execution was just triggered, start the executor (blocking).
                    if state.executing_plan.is_some() {
                        start_and_monitor_plan(&mut state, current_token.as_deref(), api, profile)
                            .await?;
                    } else if state.plan_resume_pending {
                        // Resume was sent to a paused executor — re-enter blocking monitor.
                        state.plan_resume_pending = false;
                        run_blocking_plan_monitor(&mut state).await;
                    }
                } else if (state.executing_plan.is_some() || state.plan_handle.is_some())
                    && plan_decompose::is_resume_command(&line)
                {
                    // Resume paused plan execution
                    eprintln!();
                    eprintln!("{}  Resuming plan execution...", "▶".cyan());
                    if let Some(ref handle) = state.plan_handle {
                        let _ = handle.send_command(plan_executor::PlanCommand::Resume {
                            corrections: if state.plan_execution_corrections.is_empty() {
                                None
                            } else {
                                Some(std::mem::take(&mut state.plan_execution_corrections))
                            },
                        });
                        // Re-enter blocking monitor until done/paused/error
                        run_blocking_plan_monitor(&mut state).await;
                    } else {
                        start_and_monitor_plan(&mut state, current_token.as_deref(), api, profile)
                            .await?;
                    }
                } else {
                    let has_paused_plan =
                        state.executing_plan.is_some() || state.plan_handle.is_some();
                    if has_paused_plan {
                        if let Some(action) = plan_decompose::parse_plan_paused_user_line(&line) {
                            match action {
                                plan_decompose::PlanPausedUserAction::ClearCorrections => {
                                    state.plan_execution_corrections.clear();
                                    eprintln!("{}", "  Cleared stacked operator guidance.".dim());
                                }
                                plan_decompose::PlanPausedUserAction::Correction(s) => {
                                    state.plan_execution_corrections.push(s);
                                    eprintln!(
                                        "{}  Recorded guidance ({}). It will prefix each upcoming subtask. Type continue when ready.",
                                        "💡".cyan(),
                                        state.plan_execution_corrections.len(),
                                    );
                                }
                                plan_decompose::PlanPausedUserAction::Rewind(anchor) => {
                                    if let Some(plan) = state.executing_plan.as_mut() {
                                        match plan_decompose::resolve_rewind_start_index(
                                            plan, &anchor,
                                        ) {
                                            Ok(idx) => {
                                                let reset =
                                                    plan_decompose::rewind_plan_from_subtask(
                                                        plan, idx,
                                                    );
                                                eprintln!(
                                                    "{}  Rewound from step {} — {} subtask(s) set back to pending. Type continue to resume.",
                                                    "↩".cyan(),
                                                    idx + 1,
                                                    reset,
                                                );
                                            }
                                            Err(e) => {
                                                eprintln!(
                                                    "{}",
                                                    format!("  {} {e}", theme::icon_err()).red()
                                                );
                                            }
                                        }
                                    } else {
                                        eprintln!(
                                            "  {} Rewind not available while plan is held by the executor. Type continue first.",
                                            theme::icon_warn()
                                        );
                                    }
                                }
                            }
                            continue;
                        }
                        // Paused plan: any other non-resume line abandons and becomes normal chat
                        let had_executor = state.plan_handle.is_some();
                        plan_interaction::shutdown_plan_executor(&mut state);
                        let plan = state.executing_plan.take();
                        state.plan_execution_corrections.clear();
                        match plan.as_ref() {
                            Some(p) => {
                                let (done, total) = (p.items_done(), p.subtasks.len());
                                if done < total as u32 {
                                    eprintln!(
                                        "{}  Plan abandoned ({}/{} done). Processing as normal chat.",
                                        "·".dim(),
                                        done,
                                        total
                                    );
                                }
                            }
                            None if had_executor => {
                                eprintln!(
                                    "{}  Plan abandoned (executor was cancelled; in-memory progress was not available). Processing as normal chat.",
                                    "·".dim(),
                                );
                            }
                            None => {}
                        }
                    }

                    // Auto plan detection: suggest plan mode for complex tasks
                    let mut should_proceed_normal = true;
                    let line_for_plan = line.clone(); // Clone early to avoid borrow issues
                    if let Some(reason) = plan_decompose::should_suggest_plan_mode(&line) {
                        eprintln!();
                        eprintln!("{}  {}", "📋".yellow(), reason);
                        eprintln!(
                            "{}  This task might benefit from planning. Enter plan mode? (y/n)",
                            "💡".cyan()
                        );

                        // Read user response
                        let mut response = String::new();
                        if std::io::stdin().read_line(&mut response).is_ok() {
                            let resp = response.trim().to_lowercase();
                            if resp == "y" || resp == "yes" || resp == "是" {
                                // Enter plan mode with the goal
                                let project_root = std::env::current_dir()
                                    .unwrap_or_else(|_| std::path::PathBuf::from("."));
                                let context = plan_decompose::analyze_project(&project_root);
                                let goal_display = line_for_plan.clone();
                                let plan_state = plan_decompose::PlanModeState::new(
                                    line_for_plan.clone(),
                                    context,
                                );

                                eprintln!();
                                eprintln!(
                                    "{}  Entering plan mode for: {}",
                                    "📋".green(),
                                    goal_display.cyan()
                                );
                                eprintln!("{}  Generating plan...", "⋯".dim());

                                // Trigger plan generation (set goal, plan will be generated in plan mode)
                                state.plan_mode = Some(plan_state);
                                should_proceed_normal = false;

                                // Call handle_plan_mode_input to generate the plan
                                handle_plan_mode_input(
                                    line_for_plan,
                                    current_token.as_deref(),
                                    &mut state,
                                    api,
                                )
                                .await?;
                            } else {
                                eprintln!("{}  Proceeding with normal chat...", "→".dim());
                            }
                        }
                    }

                    if should_proceed_normal {
                        handle_chat_input(
                            line,
                            current_token.as_deref(),
                            &mut state,
                            ReplTurnContext {
                                api,
                                profile,
                                selector: &*selector,
                            },
                        )
                        .await?;
                    }

                    // Keep panic guard in sync with current session state.
                    if let Some(ref sid) = state.session_id {
                        update_panic_guard(sid, state.turn);
                    }

                    // Periodic learning sync: push to cloud at checkpoint boundaries
                    // to prevent data loss on crash (every CHECKPOINT_INTERVAL turns)
                    if state.matrix_runtime.is_some()
                        && state.turn > 0
                        && state
                            .turn
                            .is_multiple_of(astra_services::session_checkpoint::CHECKPOINT_INTERVAL)
                    {
                        // Use delta push at checkpoints to reduce sync bandwidth.
                        // Final session-end sync still uses full push for convergence.
                        if let Some(new_version) = try_cloud_push_delta(
                            &profile_name_str,
                            &pipeline_modules.entity_graph,
                            &pipeline_modules.pattern_library,
                            &pipeline_modules.calibrator,
                            &state.tool_health_entries,
                            &mut state.synced_tool_health_entries,
                            state.cloud_learning_version,
                        )
                        .await
                        {
                            state.cloud_learning_version = Some(new_version);
                            // Update orchestrator envelope to reflect the push
                            if let Some(ref mc) = state.matrix_runtime {
                                let orch = mc.sync_orchestrator_lock().await;
                                if let Some(mut env) =
                                    orch.envelope(astra_services::SyncDomain::Learning)
                                {
                                    env.mark_synced(new_version as u64);
                                    orch.update_envelope(astra_services::SyncDomain::Learning, env);
                                }
                            }
                        }
                        // On conflict, we skip this push — the final push at session end
                        // will resolve conflicts via pull-merge-push cycle
                    }

                    // --max-budget enforcement: check accumulated cost against budget limit
                    if state.max_budget_limit > 0.0 {
                        let current_cost = cost_for_tokens(
                            state.total_prompt_tokens,
                            state.total_completion_tokens,
                            state.total_cache_read_tokens,
                            state.total_cache_creation_tokens,
                            &state.cached_pricing,
                        );
                        state.total_session_cost = current_cost;
                        if current_cost >= state.max_budget_limit {
                            eprintln!(
                                "\n  {} Session budget reached: {} / {} limit. Exiting.",
                                theme::icon_warn(),
                                format_cost(current_cost).bold(),
                                format_cost(state.max_budget_limit),
                            );
                            break;
                        }
                    }
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
                // Graceful ingestion shutdown: await worker flush to ensure short sessions sync
                if let Some(mc) = state.matrix_runtime.as_ref() {
                    mc.shutdown_ingestion_and_wait().await;
                }
                clear_panic_guard();
                // Show resume hint if session had any turns
                if state.turn > 0
                    && let Some(ref sid) = state.session_id
                {
                    let short = prefix_chars(sid, 8);
                    eprintln!(
                        "{}",
                        format!("  Session {short}… saved. To resume: /resume {sid}").dim()
                    );
                }
                if state.session_id.is_some() {
                    let _ = clear_profile_last_session(profile);
                }
                break;
            }
            Err(e) => {
                clear_slash_overlay();
                eprintln!(
                    "  {} {}",
                    theme::icon_err(),
                    "Input error — exiting session.".red()
                );
                eprintln!("{}", format!("  ({e})").dim());
                // Journal + ingestion: session end on error exit
                if let Some(ref j) = state.journal {
                    let end_event = session_journal::JournalEvent::session_end(
                        state.session_id.as_deref(),
                        state.turn,
                    );
                    let _ = j.append(&end_event);
                    repl_turn::enqueue_ingestion_pub(&state, &end_event);
                }
                if let Some(mc) = state.matrix_runtime.as_ref() {
                    mc.shutdown_ingestion_and_wait().await;
                }
                clear_panic_guard();
                break;
            }
        }
    }

    // Save cross-session learning state (including tool health)
    {
        // Save skill quality metrics
        if let Err(e) = state.skill_quality_tracker.save(&skill_quality_path) {
            eprintln!(
                "{}",
                format!("  ⚠ Skill quality data not saved: {e}").yellow()
            );
        }

        // Save pinned skills (atomic: write to temp file, then rename)
        if !state.pinned_skills.is_empty() {
            if let Ok(json) = serde_json::to_string_pretty(&state.pinned_skills) {
                if let Some(parent) = pinned_skills_path.parent() {
                    let _ = std::fs::create_dir_all(parent);
                }
                let tmp = pinned_skills_path.with_extension("tmp");
                match std::fs::write(&tmp, &json) {
                    Ok(()) => {
                        if let Err(e) = std::fs::rename(&tmp, &pinned_skills_path) {
                            eprintln!("⚠ Failed to save pinned_skills.json: {e}");
                            let _ = std::fs::remove_file(&tmp);
                        }
                    }
                    Err(e) => eprintln!("⚠ Failed to write pinned_skills.json: {e}"),
                }
            }
        } else {
            let _ = std::fs::remove_file(&pinned_skills_path);
        }

        // Upload quality metrics to marketplace (opt-in via ASTRA_QUALITY_UPLOAD=true)
        slash_skill::maybe_upload_quality_on_exit(
            api,
            &state.skill_quality_tracker,
            current_access_token(profile).as_deref(),
        )
        .await;

        let profile_name = profile.unwrap_or("default");
        if let Err(e) = astra_runtime::pipeline::persistence::save_learning_state_with_health(
            profile_name,
            &pipeline_modules.entity_graph,
            &pipeline_modules.pattern_library,
            &pipeline_modules.calibrator,
            &state.tool_health_entries,
        ) {
            eprintln!(
                "{}",
                format!("  ⚠ Learning state not saved (will retry next session): {e}").yellow()
            );
        }
        // Push learning to cloud with versioned API (conflict resolution loop)
        // On conflict: pull fresh data, merge, retry push (max 3 attempts)
        const MAX_SYNC_RETRIES: u32 = 3;
        let mut expected_version = state.cloud_learning_version;
        for attempt in 0..MAX_SYNC_RETRIES {
            if try_cloud_push_versioned(
                profile_name,
                &pipeline_modules.entity_graph,
                &pipeline_modules.pattern_library,
                &pipeline_modules.calibrator,
                &state.tool_health_entries,
                expected_version,
            )
            .await
            .is_some()
            {
                // Success — done
                break;
            }
            // Conflict or failure — pull fresh, merge, retry
            if attempt + 1 < MAX_SYNC_RETRIES {
                eprintln!("{}", "  ↻ Pulling fresh cloud state for merge...".dim());
                let pull_result = try_cloud_pull(
                    profile_name,
                    &pipeline_modules.entity_graph,
                    &pipeline_modules.pattern_library,
                    &pipeline_modules.calibrator,
                )
                .await;
                expected_version = pull_result.version;
                // Merge tool health from cloud pull
                if !pull_result.tool_health.is_empty() {
                    let (merged, _, _) = astra_runtime::pipeline::persistence::merge_tool_health(
                        &state.tool_health_entries,
                        &pull_result.tool_health,
                    );
                    // Update tool health in memory (though session is ending)
                    state.tool_health_entries = merged;
                }
            }
        }
        // Push preferences to cloud (best-effort)
        try_cloud_push_preferences(&state).await;
    }

    if let Some(h) = edge_heartbeat_task.take() {
        h.abort();
    }

    readline.shutdown(hist_path);
    Ok(())
}

/// Resolve `--system-prompt` value: if it starts with `@`, read the file;
/// otherwise return the string as-is.
fn resolve_system_prompt(sp: String) -> Result<String, String> {
    if let Some(path) = sp.strip_prefix('@') {
        if path.is_empty() {
            return Err("Error: @file syntax requires a file path (e.g. @prompt.txt)".to_string());
        }
        match std::fs::read_to_string(path) {
            Ok(content) => Ok(content),
            Err(e) => Err(format!(
                "Error: cannot read system prompt file '{}': {}",
                path, e
            )),
        }
    } else {
        Ok(sp)
    }
}

/// Discover project-level instructions from `.astra/instructions.md` files.
///
/// Search order (first match per level wins):
/// 1. `.astra/instructions.md` in the current working directory (project-level)
/// 2. `~/.astra/instructions.md` in the user home (global/user-level)
///
/// Both levels are combined if present: project-level first, then global,
/// separated by a newline.
fn discover_project_instructions() -> Option<String> {
    let project_root = std::env::current_dir().ok();
    let home = dirs::home_dir();
    discover_instructions_from_paths(project_root.as_deref(), home.as_deref())
}

/// Core logic: discover instructions from explicit paths (testable without cwd mutation).
fn discover_instructions_from_paths(
    project_root: Option<&std::path::Path>,
    home: Option<&std::path::Path>,
) -> Option<String> {
    let mut parts = Vec::new();

    // Project-level: .astra/instructions.md
    if let Some(root) = project_root {
        let project_path = root.join(".astra").join("instructions.md");
        if let Ok(content) = std::fs::read_to_string(&project_path) {
            let trimmed = content.trim();
            if !trimmed.is_empty() {
                parts.push((project_path.display().to_string(), trimmed.to_string()));
            }
        }
    }

    // User-level: ~/.astra/instructions.md
    if let Some(h) = home {
        let user_path = h.join(".astra").join("instructions.md");
        if let Ok(content) = std::fs::read_to_string(&user_path) {
            let trimmed = content.trim();
            if !trimmed.is_empty() {
                parts.push((user_path.display().to_string(), trimmed.to_string()));
            }
        }
    }

    if parts.is_empty() {
        return None;
    }

    let combined = parts
        .iter()
        .map(|(path, content)| format!("<!-- source: {} -->\n{}", path, content))
        .collect::<Vec<_>>()
        .join("\n\n");

    Some(combined)
}

/// Format project instructions for injection into the effective message.
fn format_project_instructions(instructions: &str) -> String {
    format!(
        "<project_instructions>\nThe following are project-level instructions that apply to all interactions in this workspace.\n\n{instructions}\n</project_instructions>"
    )
}

// ════════════════════════════════════════════════════════════════ main ════

#[tokio::main]
async fn main() {
    dotenvy::dotenv().ok();
    let cli = Cli::parse();
    let base = cli.api_url.trim_end_matches('/').to_string();
    let api = astra_thin_client::ThinClient::new(&base, None).expect("valid API URL");

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
        model: cli_model,
        print: print_mode,
        output_format,
        continue_last,
        resume,
        yes: auto_approve,
        system_prompt,
        max_turns,
        max_budget,
        allowed_tools,
        disallowed_tools,
        add_dir,
        verbose,
        mcp_config,
        session_id: cli_session_id,
        session_name,
        bare,
        no_instructions,
        command,
    } = cli;

    // --bare: set env var for minimal mode
    if bare {
        unsafe {
            std::env::set_var("ASTRA_BARE", "1");
        }
    }

    // --no-instructions: disable .astra/instructions.md auto-loading
    if no_instructions {
        unsafe {
            std::env::set_var("ASTRA_NO_INSTRUCTIONS", "1");
        }
    }

    // --max-turns: override via env var before RuntimeLimits singleton is initialized
    if let Some(turns) = max_turns {
        unsafe {
            std::env::set_var("MO_MAX_TURNS", turns.to_string());
        }
    }

    // --max-budget: store the limit; enforcement happens in the REPL loop
    if max_budget > 0.0 {
        unsafe {
            std::env::set_var("MO_MAX_BUDGET", max_budget.to_string());
        }
    }

    // --system-prompt: support @file syntax to read from file
    let system_prompt = system_prompt.map(|sp| match resolve_system_prompt(sp) {
        Ok(content) => content,
        Err(e) => {
            eprintln!("{}", e.red());
            std::process::exit(1);
        }
    });

    // Merge project instructions into system_prompt for inline/print modes.
    // REPL mode handles this separately via build_effective_line.
    let system_prompt = if no_instructions {
        system_prompt
    } else {
        match (system_prompt, discover_project_instructions()) {
            (Some(sp), Some(pi)) => Some(format!("{sp}\n\n{}", format_project_instructions(&pi))),
            (Some(sp), None) => Some(sp),
            (None, Some(pi)) => Some(format_project_instructions(&pi)),
            (None, None) => None,
        }
    };

    // --allowed-tools: normalize comma/space-separated list and export as env var
    if !allowed_tools.is_empty() {
        let normalized: Vec<String> = allowed_tools
            .iter()
            .flat_map(|s| s.split([',', ' ']))
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
        if !normalized.is_empty() {
            unsafe {
                std::env::set_var("ASTRA_ALLOWED_TOOLS", normalized.join(","));
            }
        }
    }

    // --disallowed-tools: normalize and export as env var (deny-list, opposite of --allowed-tools)
    if !disallowed_tools.is_empty() {
        let normalized: Vec<String> = disallowed_tools
            .iter()
            .flat_map(|s| s.split([',', ' ']))
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
        if !normalized.is_empty() {
            unsafe {
                std::env::set_var("ASTRA_DISALLOWED_TOOLS", normalized.join(","));
            }
        }
    }

    // --add-dir: export additional directories as env var
    if !add_dir.is_empty() {
        let dirs: Vec<String> = add_dir
            .iter()
            .map(|d| {
                std::path::Path::new(d)
                    .canonicalize()
                    .map(|p| p.to_string_lossy().into_owned())
                    .unwrap_or_else(|_| d.clone())
            })
            .collect();
        unsafe {
            std::env::set_var("ASTRA_ADD_DIRS", dirs.join(":"));
        }
    }

    // --yes (-y): set auto-approve mode for the interactive REPL
    if auto_approve {
        unsafe {
            std::env::set_var("ASTRA_AUTO_APPROVE", "1");
        }
    }
    if verbose {
        unsafe {
            std::env::set_var("ASTRA_VERBOSE", "1");
        }
    }

    // --mcp-config: load MCP server configs from files/JSON strings
    if !mcp_config.is_empty() {
        if let Err(e) = command_router::load_mcp_configs(&mcp_config) {
            eprintln!(
                "{}",
                format!("Warning: failed to load MCP config: {e}").yellow()
            );
        }
    }

    // --session-id: validate UUID format and export for REPL to pick up
    if let Some(ref sid) = cli_session_id {
        if uuid::Uuid::parse_str(sid).is_err() {
            eprintln!(
                "{}",
                format!("Error: --session-id must be a valid UUID, got '{sid}'").red()
            );
            std::process::exit(1);
        }
        unsafe {
            std::env::set_var("ASTRA_SESSION_ID", sid);
        }
    }

    // --name: export session display name
    if let Some(ref name) = session_name {
        unsafe {
            std::env::set_var("ASTRA_SESSION_NAME", name);
        }
    }

    // Resolve model: --model flag > config default_model > None
    let resolved_model =
        cli_model.or_else(|| command_router::read_config_default_model().ok().flatten());

    // --print mode: headless single-shot, always auto-approve (can't prompt)
    if print_mode {
        match run_print_mode(
            &api,
            profile.as_deref(),
            &output_format,
            resolved_model.as_deref(),
            system_prompt.as_deref(),
            command,
        )
        .await
        {
            Ok(code) => std::process::exit(i32::from(code)),
            Err(e) => {
                eprintln!("{}", format!("Error: {e}").red());
                std::process::exit(i32::from(ExitCode::ApiError));
            }
        }
    }

    // -c / --continue: resume most recent session
    // -r / --resume <ID>: resume specific session
    if continue_last || resume.is_some() {
        let session_id = resume.as_deref();

        // For -c, resolve the last session ID from credentials
        let resolved_sid = if continue_last && session_id.is_none() {
            resumable_last_session_id(profile.as_deref())
        } else {
            session_id.map(|s| s.to_string())
        };

        match resolved_sid {
            Some(sid) => {
                let result = run_chat_repl(
                    &api,
                    profile.as_deref(),
                    resolved_model.as_deref(),
                    Some(&sid),
                )
                .await;
                match result {
                    Ok(()) => std::process::exit(0),
                    Err(e) => {
                        eprintln!("{}", format!("Error: {e}").red());
                        std::process::exit(i32::from(ExitCode::ApiError));
                    }
                }
            }
            None => {
                eprintln!(
                    "{}",
                    "No previous session to continue. Start a new one with `astra`.".yellow()
                );
                std::process::exit(1);
            }
        }
    }

    match execute_cli_command(
        command,
        profile,
        resolved_model,
        auto_approve,
        system_prompt,
        &api,
    )
    .await
    {
        Ok(exit_code) => {
            std::process::exit(i32::from(exit_code));
        }
        Err(e) => {
            eprintln!("{}", format!("Error: {e}").red());
            std::process::exit(i32::from(ExitCode::ApiError));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{Router, routing::get, routing::post};

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

    /// Guard that serializes tests touching ASTRA_CREDENTIALS_DIR.
    /// Multiple async tests concurrently setting this env var is a data race;
    /// the guard ensures they execute sequentially.
    use std::sync::{Mutex, MutexGuard, OnceLock};

    struct CredentialsGuard {
        _lock: MutexGuard<'static, ()>,
        _dir: tempfile::TempDir,
    }

    impl Drop for CredentialsGuard {
        fn drop(&mut self) {
            unsafe {
                std::env::remove_var("ASTRA_CREDENTIALS_DIR");
            }
        }
    }

    fn creds_lock() -> MutexGuard<'static, ()> {
        static CREDS_LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        CREDS_LOCK
            .get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }

    /// Set credentials dir to a temp path so tests don't pollute ~/.astra/credentials.json.
    /// Returns a guard that holds a mutex — tests using this are serialized.
    fn isolate_credentials() -> CredentialsGuard {
        let lock = creds_lock();
        let dir = tempfile::tempdir().unwrap();
        // SAFETY: protected by CREDS_LOCK; no concurrent set_var.
        unsafe { std::env::set_var("ASTRA_CREDENTIALS_DIR", dir.path()) };
        CredentialsGuard {
            _lock: lock,
            _dir: dir,
        }
    }

    mod preamble_tests;
    mod auth_tests;
    mod chat_stream_tests;
    mod slash_command_tests;
    mod repl_tests;
    mod resume_tests;
    mod stats_tools_tests;
    mod cloud_sync_tests;
    mod cost_tracking_tests;
    mod cli_args_tests;
}
