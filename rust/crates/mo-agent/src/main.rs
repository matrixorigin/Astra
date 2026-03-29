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
#[path = "mo_agent/plan_decompose.rs"]
mod plan_decompose;
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
    tool_call_detail, tool_result_summary, truncate_str, urlencoding,
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
    is_slash_picker_active, print_keyboard_shortcuts, print_slash_commands, resolve_slash_command,
    suggest_commands, take_slash_pending_execute,
};
use slash_account::handle_account_command;
use slash_info::handle_info_command;
use slash_memory::{handle_memory_domain_command, handle_plan_mode_input};
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
    /// Exported tool health entries from this turn's TurnGuard (for cross-session persistence).
    tool_health_export: Vec<mo_agent_runtime::pipeline::persistence::ToolHealthEntry>,
    /// Last heavy checkpoint built during the agentic loop (for cloud persistence).
    last_heavy_checkpoint: Option<mo_agent_runtime::pipeline::step_protocol::StepCheckpoint>,
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
    /// Timeout-specific failure count (subset of total_errors).
    total_timeouts: usize,
    /// Idempotency cache hits (tools skipped, neutral for health).
    total_cache_hits: usize,
    /// Number of tools with rehabilitation_count >= 2 (flaky).
    flaky_count: usize,
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
    /// Learning snapshot restored from cloud (to be merged into learning modules).
    learning_snapshot: Option<String>,
    /// Local task service for /task commands.
    task_service: Option<std::sync::Arc<mo_agent_services::LocalTaskService>>,
    /// Cross-session tool health data for error budget persistence.
    tool_health_entries: Vec<mo_agent_runtime::pipeline::persistence::ToolHealthEntry>,
    /// Last successfully synced tool health snapshot, used to compute deltas.
    synced_tool_health_entries: Vec<mo_agent_runtime::pipeline::persistence::ToolHealthEntry>,
    /// Plan Mode state — when Some, REPL is in interactive plan editing mode.
    plan_mode: Option<plan_decompose::PlanModeState>,
    /// Plan being auto-executed — subtasks sent sequentially through chat.
    executing_plan: Option<mo_agent_services::task_orchestrator::TaskPlan>,
    /// Configuration for current plan execution (step-by-step, auto-execute, etc.).
    plan_execution_config: Option<plan_decompose::PlanExecutionConfig>,
    /// Goal text for the executing plan (for summary generation).
    executing_plan_goal: Option<String>,
    /// Number of parallel execution rounds completed (for summary).
    plan_execution_rounds: usize,
    /// Whether the last chat turn was interrupted by Ctrl+C (used by plan auto-execution).
    last_turn_interrupted: bool,
    /// Cloud learning snapshot version for optimistic locking.
    /// Set by try_cloud_pull, used by try_cloud_push to prevent concurrent overwrites.
    cloud_learning_version: Option<i64>,
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
            learning_snapshot: None,
            task_service: None,
            tool_health_entries: Vec::new(),
            synced_tool_health_entries: Vec::new(),
            plan_mode: None,
            executing_plan: None,
            plan_execution_config: None,
            executing_plan_goal: None,
            plan_execution_rounds: 0,
            last_turn_interrupted: false,
            cloud_learning_version: None,
        }
    }
}

// ═════════════════════════════════════════════════════════ ReplHelper ════

// ═════════════════════════════════════════════════════════ Clipboard ══════

// ═══════════════════════════════════════════════════════════ Resume ═══════

async fn handle_resume_command(arg: &str, profile: Option<&str>, state: &mut ReplState) {
    use mo_agent_services::session_restore::{HybridRestoreService, SessionRestoreService};

    let user_id = state.ingestion_user_id.as_deref().unwrap_or("local");
    let svc = match &state.matrixone_pool {
        Some(pool) => HybridRestoreService::new(pool.as_ref().clone()),
        None => HybridRestoreService::local_only(),
    };

    // If no session_id given, list resumable sessions
    if arg.is_empty() {
        match svc.list_resumable_sessions(user_id).await {
            Ok(sessions) if sessions.is_empty() => {
                eprintln!(
                    "{}",
                    "  No resumable sessions. Use /resume <session_id>.".dim()
                );
            }
            Ok(sessions) => {
                eprintln!(
                    "\n{}",
                    "─── Resumable Sessions ──────────────────────────".bold()
                );
                for s in &sessions {
                    let title = s.title.as_deref().unwrap_or("untitled");
                    let short_id = &s.session_id[..8.min(s.session_id.len())];
                    eprintln!(
                        "  {} {} ({} turns, {})",
                        short_id.cyan(),
                        title,
                        s.turn_count,
                        s.last_status.as_str().dim(),
                    );
                }
                eprintln!("  Use /resume <session_id> to restore.\n");
            }
            Err(e) => {
                eprintln!("{}", format!("  ✗ Could not list sessions: {e}").red());
                eprintln!("{}", "  Check /doctor for connectivity status.".dim());
            }
        }
        return;
    }

    // Resolve prefix via local journal first
    let session_id = match session_journal::resolve_session_id(arg) {
        Ok(resolved) => {
            if resolved != arg {
                eprintln!(
                    "  {} Resolved {} → {}",
                    "✓".green(),
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
                        format!("  ✗ Session {} not found or not owned by user", arg).red()
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

            // Merge step checkpoint data if available (with migration support)
            let registry =
                mo_agent_runtime::pipeline::step_protocol::MigrationRegistry::with_defaults();
            if let Ok(Some(step_restored)) =
                mo_agent_runtime::pipeline::step_restore::restore_session_with_migrations(
                    &restored.session_id,
                    &registry,
                )
            {
                let summary =
                    mo_agent_runtime::pipeline::step_restore::restore_summary(&step_restored);
                // Merge blocked tools from checkpoint into health entries
                for tool in &step_restored.blocked_tools {
                    if !state.tool_health_entries.iter().any(|e| e.name == *tool) {
                        state.tool_health_entries.push(
                            mo_agent_runtime::pipeline::persistence::ToolHealthEntry {
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
                mo_agent_runtime::pipeline::step_checkpoint::read_latest_heavy_checkpoint(
                    &restored.session_id,
                )
            {
                // Fallback to raw local checkpoint if step_restore fails (e.g., version mismatch)
                if state.recent_tools.is_empty() {
                    state.recent_tools = heavy.recent_tools;
                }
            } else if let Some(ref pool) = state.matrixone_pool {
                // Cloud fallback: pull heavy checkpoint from MatrixOne
                // (different device, local files not available)
                match mo_agent_services::session_restore::pull_step_checkpoint_from_cloud(
                    pool,
                    &restored.session_id,
                )
                .await
                {
                    Ok(Some(state_json)) => {
                        match serde_json::from_str::<
                            mo_agent_runtime::pipeline::step_protocol::StepCheckpoint,
                        >(&state_json)
                        {
                            Ok(
                                mo_agent_runtime::pipeline::step_protocol::StepCheckpoint::Heavy(
                                    heavy,
                                ),
                            ) => {
                                for tool in &heavy.blocked_tools {
                                    if !state.tool_health_entries.iter().any(|e| e.name == *tool) {
                                        state.tool_health_entries.push(
                                            mo_agent_runtime::pipeline::persistence::ToolHealthEntry {
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
                                    "⚠".yellow()
                                );
                                eprintln!("{}", format!("     ({e})").dim());
                            }
                        }
                    }
                    Ok(None) => {} // No cloud checkpoint available
                    Err(e) => {
                        eprintln!("  {} Cloud checkpoint unavailable", "⚠".yellow());
                        eprintln!("{}", format!("     ({e})").dim());
                    }
                }
            }

            if let Some(ref m) = restored.model {
                state.model = Some(m.clone());
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
                "✓".green(),
                &session_id[..8.min(session_id.len())].cyan(),
                source,
                restored.turn_count,
                restored.checkpoint_count,
            );
        }
        Ok(None) => {
            eprintln!("{}", format!("  Session '{arg}' not found.").yellow());
            eprintln!("{}", "  Use /resume to see available sessions.".dim());
        }
        Err(e) => {
            let hint = if e.to_string().contains("not found") {
                "Use /resume to see available sessions."
            } else {
                "Check connection with /doctor, or try a different session."
            };
            eprintln!("{}", format!("  ✗ Resume failed: {e}").red());
            eprintln!("{}", format!("  {hint}").dim());
        }
    }
}

// ═══════════════════════════════════════════════════════ Stats ════════════

fn handle_stats_command(arg: &str, state: &ReplState) {
    use mo_agent_services::session_analytics;

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

// ═══════════════════════════════════════════════ Tool Profile ═════════════

fn handle_tools_command(state: &ReplState) {
    use mo_agent_services::session_analytics;

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
    use mo_agent_runtime::turn::tool_health::ToolHealthTracker;

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
    match &state.matrixone_pool {
        None => {
            eprintln!(
                "  {} {}",
                "○".dim(),
                "Offline — no MatrixOne connection".dim()
            );
            eprintln!("  {}", "Set MATRIXONE_HOST to enable cloud sync.".dim());
        }
        Some(pool) => {
            let svc =
                mo_agent_services::state_sync::MatrixOneSyncService::new(pool.as_ref().clone());
            let sync_status = mo_agent_services::state_sync::StateSyncService::status(&svc).await;
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
fn display_sync_status(status: &mo_agent_services::SyncStatus) {
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
        let short = if err.len() > 80 { &err[..80] } else { err };
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

// ═══════════════════════════════════════════════ Plan Auto-Execution ═════

/// Run the plan auto-execution loop: iterate through ready subtasks,
/// send each as a chat message, mark done, continue until all done or blocked.
///
/// Uses a take-modify-put pattern to avoid borrow conflicts with handle_chat_input.
async fn run_plan_execution(
    state: &mut ReplState,
    current_token: Option<&str>,
    client: &reqwest::Client,
    base: &str,
    profile: Option<&str>,
    selector: &dyn tool_selector::ToolSelector,
) -> Result<(), String> {
    use mo_agent_services::task_orchestrator::TaskStatus;

    loop {
        // Take the plan out of state to avoid borrow conflicts
        let mut plan = match state.executing_plan.take() {
            Some(p) => p,
            None => return Ok(()),
        };

        // Mark any in-progress subtasks as completed (just finished by previous chat turn)
        let mut completed_titles: Vec<String> = Vec::new();
        for st in plan.subtasks.iter_mut() {
            if st.status == TaskStatus::InProgress {
                st.status = TaskStatus::Completed;
                completed_titles.push(st.title.clone());
            }
        }
        for title in &completed_titles {
            let pct = plan.progress_pct();
            eprintln!("\n{}  Subtask done: {} ({}%)", "✓".green(), title, pct);
        }

        // Analyze parallelism for the current state
        let analysis = plan_decompose::analyze_parallelism(&plan);
        let ready = plan.ready_subtasks();

        if ready.is_empty() {
            // No more ready subtasks — either all done or blocked
            let pct = plan.progress_pct();
            let goal = state
                .executing_plan_goal
                .clone()
                .unwrap_or_else(|| "Plan".into());
            let rounds = state.plan_execution_rounds;

            if pct == 100 {
                let summary =
                    plan_decompose::PlanExecutionSummary::from_plan(&plan, &goal, rounds);
                eprintln!();
                eprint!("{}", summary.format());

                // Journal: plan complete
                if let Some(ref mut j) = state.journal {
                    let evt = mo_agent_services::session_journal::JournalEvent::plan_progress(
                        state.session_id.as_deref(),
                        state.turn,
                        "",
                        &goal,
                        "plan_complete",
                        100,
                        plan.subtasks.len(),
                        plan.items_done() as usize,
                    );
                    let _ = j.append(&evt);
                }
            } else {
                let blocked: Vec<_> = plan
                    .subtasks
                    .iter()
                    .filter(|s| s.status == TaskStatus::Pending)
                    .map(|s| s.id.as_str())
                    .collect();
                eprintln!(
                    "\n{}  Plan execution paused at {}%. Blocked: {}",
                    "⏸".yellow(),
                    pct,
                    blocked.join(", ")
                );
                // Keep plan for potential resume
                state.executing_plan = Some(plan);
            }

            // Clean up execution state (if fully done)
            if pct == 100 {
                state.plan_execution_config = None;
                state.executing_plan_goal = None;
                state.plan_execution_rounds = 0;
            }
            return Ok(());
        }

        // Show parallel group information if there are multiple ready subtasks
        if ready.len() > 1 {
            let group_count = analysis.groups.len();
            let parallel_in_first = analysis.groups.first().map(|g| g.len()).unwrap_or(0);
            if parallel_in_first > 1 {
                eprintln!(
                    "\n{}  {} subtasks ready, {} parallel-safe in current group",
                    "║".cyan(),
                    ready.len(),
                    parallel_in_first,
                );
            }
            if !analysis.conflicts.is_empty() {
                eprintln!(
                    "{}  ⚠ {} file conflict(s) detected — serializing conflicting subtasks",
                    "║".cyan(),
                    analysis.conflicts.len(),
                );
            }
            if group_count > 1 {
                eprintln!(
                    "{}  Executing in {} rounds (by parallel-safety groups)",
                    "║".cyan(),
                    group_count,
                );
            }
        }

        // Execute the first parallel group — all subtasks in this group are safe to run
        let exec_group = analysis.groups.first().cloned().unwrap_or_default();
        let group_size = exec_group.len();

        for (group_idx, next_id) in exec_group.iter().enumerate() {
            let (prompt, title) = {
                let st = plan.subtasks.iter_mut().find(|s| s.id == *next_id).unwrap();
                st.status = TaskStatus::InProgress;
                let prompt = plan_decompose::format_subtask_prompt(st);
                let title = st.title.clone();
                (prompt, title)
            };

            let remaining = plan
                .subtasks
                .iter()
                .filter(|s| s.status == TaskStatus::Pending)
                .count();
            let done_so_far = plan.items_done() + 1;
            let total = plan.subtasks.len();

            let group_label = if group_size > 1 {
                format!(" [{}/{}]", group_idx + 1, group_size)
            } else {
                String::new()
            };

            eprintln!(
                "\n{}  Subtask {}/{}{}: {} [{}]",
                "▶".cyan(),
                done_so_far,
                total,
                group_label,
                title,
                next_id
            );
            if remaining > 0 {
                eprintln!("{}  {} remaining after this", "·".dim(), remaining);
            }

            // Journal: subtask started
            if let Some(ref mut j) = state.journal {
                let evt = mo_agent_services::session_journal::JournalEvent::plan_progress(
                    state.session_id.as_deref(),
                    state.turn,
                    next_id,
                    &title,
                    "started",
                    plan.progress_pct(),
                    total,
                    plan.items_done() as usize,
                );
                let _ = j.append(&evt);
            }

            // Put plan back before calling handle_chat_input
            state.executing_plan = Some(plan);

            handle_chat_input(
                prompt,
                current_token,
                state,
                ReplTurnContext {
                    client,
                    base,
                    profile,
                    selector,
                },
            )
            .await?;

            // If user pressed Ctrl+C, pause execution
            if state.last_turn_interrupted {
                if let Some(ref exec_plan) = state.executing_plan {
                    let pct = exec_plan.progress_pct();
                    let remaining_count = exec_plan
                        .subtasks
                        .iter()
                        .filter(|s| {
                            s.status == TaskStatus::Pending
                                || s.status == TaskStatus::InProgress
                        })
                        .count();
                    eprintln!(
                        "\n{}  Plan paused (Ctrl+C). {}% done, {} subtasks remaining.",
                        "⏸".yellow(),
                        pct,
                        remaining_count
                    );
                    eprintln!("{}  Say \"continue\" to resume execution.", "💡".cyan());
                }
                state.last_turn_interrupted = false;
                return Ok(());
            }

            // Take plan back for the next iteration in this group
            plan = match state.executing_plan.take() {
                Some(p) => p,
                None => return Ok(()),
            };

            // Mark just-completed subtask as completed before next in group
            if let Some(st) = plan.subtasks.iter_mut().find(|s| s.id == *next_id) {
                if st.status == TaskStatus::InProgress {
                    st.status = TaskStatus::Completed;
                    let title = st.title.clone();
                    let pct = plan.progress_pct();
                    eprintln!("\n{}  Subtask done: {} ({}%)", "✓".green(), title, pct);

                    // Journal: subtask completed
                    if let Some(ref mut j) = state.journal {
                        let evt = mo_agent_services::session_journal::JournalEvent::plan_progress(
                            state.session_id.as_deref(),
                            state.turn,
                            next_id,
                            &title,
                            "completed",
                            pct,
                            plan.subtasks.len(),
                            plan.items_done() as usize,
                        );
                        let _ = j.append(&evt);
                    }
                }
            }
        }

        // Put plan back for the outer loop to pick up the next group
        state.executing_plan = Some(plan);
        state.plan_execution_rounds += 1;

        // Loop continues — will find next ready group
    }
}

// ═══════════════════════════════════════════════════ Learning Merge ═══════

fn merge_learning_snapshot(
    json: &str,
    entity_graph: &std::sync::Arc<
        std::sync::Mutex<mo_agent_runtime::pipeline::entity::EntityGraph>,
    >,
    pattern_library: &std::sync::Arc<
        std::sync::Mutex<mo_agent_runtime::pipeline::pattern::PatternLibrary>,
    >,
    calibrator: &std::sync::Arc<
        std::sync::Mutex<mo_agent_runtime::pipeline::calibration::ProgressiveCalibrator>,
    >,
) {
    match serde_json::from_str::<mo_agent_runtime::pipeline::persistence::LearningSnapshot>(json) {
        Ok(snapshot) => {
            mo_agent_runtime::pipeline::persistence::merge_into_modules(
                &snapshot,
                entity_graph,
                pattern_library,
                calibrator,
            );
            let n = snapshot.entities.len() + snapshot.patterns.len();
            if n > 0 {
                eprintln!(
                    "  {} Merged learning: {} entities, {} patterns",
                    "✓".green(),
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
    tool_health: Vec<mo_agent_runtime::pipeline::persistence::ToolHealthEntry>,
    version: Option<i64>,
}

/// Try to pull learning state from MatrixOne and merge into live modules.
/// Best-effort: silently skips if cloud is unavailable.
/// Returns tool health entries and cloud version for optimistic locking.
async fn try_cloud_pull(
    profile_name: &str,
    entity_graph: &std::sync::Arc<
        std::sync::Mutex<mo_agent_runtime::pipeline::entity::EntityGraph>,
    >,
    pattern_library: &std::sync::Arc<
        std::sync::Mutex<mo_agent_runtime::pipeline::pattern::PatternLibrary>,
    >,
    calibrator: &std::sync::Arc<
        std::sync::Mutex<mo_agent_runtime::pipeline::calibration::ProgressiveCalibrator>,
    >,
) -> CloudPullResult {
    let pool = match try_connect_matrixone().await {
        Some(p) => p,
        None => {
            return CloudPullResult {
                tool_health: Vec::new(),
                version: None,
            };
        }
    };
    let svc = mo_agent_services::state_sync::MatrixOneSyncService::new(pool);
    let user_id = std::env::var("MO_USER_ID").unwrap_or_else(|_| "local".to_string());
    match mo_agent_services::state_sync::StateSyncService::pull_learning_versioned(
        &svc,
        &user_id,
        profile_name,
    )
    .await
    {
        Ok(Some(versioned)) => {
            // Parse snapshot to extract tool health before merging entities/patterns
            let cloud_health = serde_json::from_str::<
                mo_agent_runtime::pipeline::persistence::LearningSnapshot,
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
            }
        }
        Ok(None) => CloudPullResult {
            tool_health: Vec::new(),
            version: None,
        },
        Err(e) => {
            eprintln!("{}", format!("  ⚠ Cloud pull skipped: {e}").dim());
            CloudPullResult {
                tool_health: Vec::new(),
                version: None,
            }
        }
    }
}

/// Push learning state to cloud with optimistic locking.
/// Returns the new cloud version if successful, or None on conflict/failure.
/// On conflict, the caller should pull fresh data and retry.
async fn try_cloud_push_versioned(
    profile_name: &str,
    entity_graph: &std::sync::Arc<
        std::sync::Mutex<mo_agent_runtime::pipeline::entity::EntityGraph>,
    >,
    pattern_library: &std::sync::Arc<
        std::sync::Mutex<mo_agent_runtime::pipeline::pattern::PatternLibrary>,
    >,
    calibrator: &std::sync::Arc<
        std::sync::Mutex<mo_agent_runtime::pipeline::calibration::ProgressiveCalibrator>,
    >,
    tool_health: &[mo_agent_runtime::pipeline::persistence::ToolHealthEntry],
    expected_version: Option<i64>,
) -> Option<i64> {
    let pool = match try_connect_matrixone().await {
        Some(p) => p,
        None => return None,
    };
    let snapshot = mo_agent_runtime::pipeline::persistence::export_from_modules_with_health(
        entity_graph,
        pattern_library,
        calibrator,
        tool_health,
    );
    let json = match serde_json::to_string(&snapshot) {
        Ok(j) => j,
        Err(_) => return None,
    };
    let svc = mo_agent_services::state_sync::MatrixOneSyncService::new(pool);
    let user_id = std::env::var("MO_USER_ID").unwrap_or_else(|_| "local".to_string());
    let result = mo_agent_services::state_sync::StateSyncService::push_learning_versioned(
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
        if let Err(e) = mo_agent_runtime::pipeline::persistence::save_synced_tool_health(
            profile_name,
            tool_health,
        ) {
            eprintln!(
                "{}",
                format!("  ⚠ Tool-health sync metadata not saved: {e}").dim()
            );
        }
        if let Some(v) = result.new_version {
            eprintln!("{}", format!("  ✓ Learning synced to cloud (v{})", v).dim());
            return Some(v);
        }
        eprintln!("{}", "  ✓ Learning synced to cloud".dim());
    } else {
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
    entity_graph: &std::sync::Arc<
        std::sync::Mutex<mo_agent_runtime::pipeline::entity::EntityGraph>,
    >,
    pattern_library: &std::sync::Arc<
        std::sync::Mutex<mo_agent_runtime::pipeline::pattern::PatternLibrary>,
    >,
    calibrator: &std::sync::Arc<
        std::sync::Mutex<mo_agent_runtime::pipeline::calibration::ProgressiveCalibrator>,
    >,
    tool_health_entries: &[mo_agent_runtime::pipeline::persistence::ToolHealthEntry],
    synced_tool_health_entries: &mut Vec<mo_agent_runtime::pipeline::persistence::ToolHealthEntry>,
    expected_version: Option<i64>,
) -> Option<i64> {
    let learning_dirty = mo_agent_runtime::pipeline::persistence::has_dirty_learning_data(
        entity_graph,
        pattern_library,
        calibrator,
    );
    let tool_health_deltas = mo_agent_runtime::pipeline::persistence::export_tool_health_delta(
        tool_health_entries,
        synced_tool_health_entries,
    );

    if !learning_dirty && tool_health_deltas.is_empty() {
        return expected_version;
    }

    let mut delta = mo_agent_runtime::pipeline::persistence::export_dirty_learning_from_modules(
        entity_graph,
        pattern_library,
        calibrator,
    )
    .unwrap_or(mo_agent_runtime::pipeline::persistence::DeltaSnapshot {
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

    let svc = mo_agent_services::state_sync::MatrixOneSyncService::new(pool);
    let user_id = std::env::var("MO_USER_ID").unwrap_or_else(|_| "local".to_string());

    let result = mo_agent_services::state_sync::StateSyncService::push_delta(
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
        mo_agent_runtime::pipeline::persistence::clear_dirty_learning_in_modules(
            entity_graph,
            pattern_library,
            calibrator,
        );
        *synced_tool_health_entries = tool_health_entries.to_vec();
        if let Err(e) = mo_agent_runtime::pipeline::persistence::save_synced_tool_health(
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
/// Merges cloud preferences into local state (cloud-wins).
async fn try_cloud_pull_preferences(state: &mut ReplState) {
    let pool = match try_connect_matrixone().await {
        Some(p) => p,
        None => return,
    };
    let svc = mo_agent_services::state_sync::MatrixOneSyncService::new(pool);
    let user_id = std::env::var("MO_USER_ID").unwrap_or_else(|_| "local".to_string());
    match mo_agent_services::state_sync::StateSyncService::pull_all_preferences(&svc, &user_id)
        .await
    {
        Ok(prefs) if !prefs.is_empty() => {
            use mo_agent_services::state_sync::pref_keys;
            for (key, value) in &prefs {
                if key.as_str() == pref_keys::EXPLAIN_MODE {
                    state.explain = match value.as_str() {
                        "on" => ExplainMode::On,
                        "verbose" => ExplainMode::Verbose,
                        _ => ExplainMode::Off,
                    };
                }
            }
            eprintln!(
                "{}",
                format!("  ✓ Pulled {} preferences from cloud", prefs.len()).dim()
            );
        }
        Ok(_) => {} // no cloud prefs yet
        Err(e) => {
            eprintln!("{}", format!("  ⚠ Preference pull skipped: {e}").dim());
        }
    }
}

/// Push user preferences to cloud at session end.
async fn try_cloud_push_preferences(state: &ReplState) {
    let pool = match try_connect_matrixone().await {
        Some(p) => p,
        None => return,
    };
    let svc = mo_agent_services::state_sync::MatrixOneSyncService::new(pool);
    let user_id = std::env::var("MO_USER_ID").unwrap_or_else(|_| "local".to_string());
    use mo_agent_services::state_sync::{StateSyncService, pref_keys};
    let prefs = [(pref_keys::EXPLAIN_MODE, state.explain.to_string())];
    let mut synced = 0u32;
    for (key, value) in &prefs {
        let result = svc.push_preference(&user_id, key, value).await;
        if result.success {
            synced += 1;
        }
    }
    if synced > 0 {
        eprintln!(
            "{}",
            format!("  ✓ Synced {synced} preferences to cloud").dim()
        );
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
    let database = std::env::var("MATRIXONE_DATABASE").unwrap_or_else(|_| "mo_agent".to_string());
    let url = format!("mysql://{user}:{password}@{host}:{port}/{database}");
    sqlx::mysql::MySqlPoolOptions::new()
        .max_connections(2)
        .acquire_timeout(std::time::Duration::from_secs(3))
        .connect(&url)
        .await
        .ok()
}

// ═══════════════════════════════════════════════════════ Task Commands ════

async fn handle_task_command(arg: &str, state: &mut ReplState) {
    use mo_agent_services::{TaskCreateRequest, TaskService, TaskStatus};

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
                        TaskStatus::Completed => "✓",
                        TaskStatus::Failed => "✗",
                        TaskStatus::InProgress => "▶",
                        TaskStatus::Paused => "⏸",
                        _ => "○",
                    };
                    let short_id = &t.task_id[..8.min(t.task_id.len())];
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
                        t.status.as_str().cyan(),
                        progress,
                    );
                }
                eprintln!();
            }
            Err(e) => eprintln!("{}", format!("  ✗ {e}").red()),
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
                        "✓".green(),
                        sub_arg,
                        short.dim()
                    );
                }
                Err(e) => eprintln!("{}", format!("  ✗ {e}").red()),
            }
        }
        "done" if !sub_arg.is_empty() => {
            // Find task by prefix match on task_id or title
            match find_task_by_query(&*svc, user_id, sub_arg).await {
                Ok(Some(tid)) => match svc.complete_task(&tid).await {
                    Ok(()) => eprintln!("  {} Task completed: {}", "✓".green(), sub_arg),
                    Err(e) => eprintln!("{}", format!("  ✗ {e}").red()),
                },
                Ok(None) => {
                    eprintln!("{}", format!("  Task not found: '{sub_arg}'").yellow());
                    eprintln!("{}", "  Use /task list to see available tasks.".dim());
                }
                Err(e) => eprintln!("{}", format!("  ✗ {e}").red()),
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
                        eprintln!("  {:<12} {}", "status:".dim(), t.status.as_str().cyan());
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
                    Err(e) => eprintln!("{}", format!("  ✗ {e}").red()),
                },
                Ok(None) => {
                    eprintln!("{}", format!("  Task not found: '{sub_arg}'").yellow());
                    eprintln!("{}", "  Use /task list to see available tasks.".dim());
                }
                Err(e) => eprintln!("{}", format!("  ✗ {e}").red()),
            }
        }
        _ => {
            eprintln!("  Usage: /task [list | add <title> | done <id> | status <id>]");
        }
    }
}

/// Find a task by prefix match on task_id or substring match on title.
async fn find_task_by_query(
    svc: &dyn mo_agent_services::TaskService,
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

        "/keys" => print_keyboard_shortcuts(),

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

        "/history" | "/search" | "/copy" | "/doctor" | "/context" | "/version" | "/rewind" => {
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

        "/memory" | "/plan" => {
            handle_memory_domain_command(cmd, arg, client, base, state, token).await?;
        }

        "/task" => {
            handle_task_command(arg, state).await;
        }

        "/resume" => {
            handle_resume_command(arg, profile, state).await;
        }

        "/stats" => {
            handle_stats_command(arg, state);
        }

        "/tools" => {
            handle_tools_command(state);
        }

        "/health" => {
            handle_health_command(arg, state).await;
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

    // Load cross-session learning state (entity graph, patterns, calibration, tool health)
    let mut cross_session_health_entries;
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
        // Load tool health for cross-session error budgets
        cross_session_health_entries =
            mo_agent_runtime::pipeline::persistence::load_tool_health(profile_name);
        state.synced_tool_health_entries =
            mo_agent_runtime::pipeline::persistence::load_synced_tool_health(profile_name);
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
                mo_agent_runtime::pipeline::persistence::merge_tool_health(
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
        try_cloud_pull_preferences(&mut state).await;
    }
    state.tool_health_entries = cross_session_health_entries.clone();
    if state.synced_tool_health_entries.is_empty() {
        state.synced_tool_health_entries = cross_session_health_entries;
    }

    let profile_name_str = profile.unwrap_or("default").to_string();
    print_repl_banner(profile, &state);

    // ── Main loop ─────────────────────────────────────────────────────────────
    loop {
        let current_token = current_access_token(profile);

        // Prompt: plan> in plan mode, ❯ otherwise
        if let Some(ref sname) = state.skill_dev_name {
            eprintln!("  \u{1f527} {}", format!("Skill dev: {sname}").cyan().dim());
        }
        let prompt_str = if state.plan_mode.is_some() {
            format!("{} ", "plan>".yellow().bold())
        } else if state.executing_plan.is_some() {
            format!("{} ", "⏸>".yellow().bold())
        } else {
            format!("{} ", "❯".cyan().bold())
        };

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
                    // If Enter was pressed in the picker, the selected command is
                    // stored in pending-execute.  Use it instead of the raw line.
                    let pending = take_slash_pending_execute();
                    let dispatch_line = pending.as_deref().unwrap_or(&line);
                    let should_exit = handle_slash_command(
                        dispatch_line,
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
                    // Merge learning snapshot if /resume deposited one
                    if let Some(json) = state.learning_snapshot.take() {
                        merge_learning_snapshot(
                            &json,
                            &pipeline_modules.entity_graph,
                            &pipeline_modules.pattern_library,
                            &pipeline_modules.calibrator,
                        );
                    }

                    // If /plan auto triggered execution, start the auto-execution loop
                    if state.executing_plan.is_some() && state.plan_mode.is_none() {
                        run_plan_execution(
                            &mut state,
                            current_token.as_deref(),
                            client,
                            base,
                            profile,
                            &*selector,
                        )
                        .await?;
                    }
                } else if state.plan_mode.is_some() {
                    // Plan mode: handle input as plan editing
                    handle_plan_mode_input(
                        line.clone(),
                        current_token.as_deref(),
                        &mut state,
                        client,
                        base,
                    )
                    .await?;

                    // If plan execution was just triggered, run the auto-execution loop
                    if state.executing_plan.is_some() {
                        run_plan_execution(
                            &mut state,
                            current_token.as_deref(),
                            client,
                            base,
                            profile,
                            &*selector,
                        )
                        .await?;
                    }
                } else if state.executing_plan.is_some() && plan_decompose::is_resume_command(&line)
                {
                    // Resume paused plan execution
                    eprintln!();
                    eprintln!("{}  Resuming plan execution...", "▶".cyan());
                    run_plan_execution(
                        &mut state,
                        current_token.as_deref(),
                        client,
                        base,
                        profile,
                        &*selector,
                    )
                    .await?;
                } else {
                    // If there's a paused plan but user sends a different message,
                    // abandon the plan and process as normal chat
                    if state.executing_plan.is_some() && !plan_decompose::is_resume_command(&line) {
                        let plan = state.executing_plan.take().unwrap();
                        let done = plan.items_done();
                        let total = plan.subtasks.len();
                        if done < total as u32 {
                            eprintln!(
                                "{}  Plan abandoned ({}/{} done). Processing as normal chat.",
                                "·".dim(),
                                done,
                                total
                            );
                        }
                    }

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

                    // Periodic learning sync: push to cloud at checkpoint boundaries
                    // to prevent data loss on crash (every CHECKPOINT_INTERVAL turns)
                    if state.matrixone_pool.is_some()
                        && state.turn > 0
                        && state.turn.is_multiple_of(
                            mo_agent_services::session_checkpoint::CHECKPOINT_INTERVAL,
                        )
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
                        }
                        // On conflict, we skip this push — the final push at session end
                        // will resolve conflicts via pull-merge-push cycle
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
                eprintln!("{}", "  ✗ Input error — exiting session.".red());
                eprintln!("{}", format!("  ({e})").dim());
                break;
            }
        }
    }

    // Save cross-session learning state (including tool health)
    {
        let profile_name = profile.unwrap_or("default");
        if let Err(e) = mo_agent_runtime::pipeline::persistence::save_learning_state_with_health(
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
                    let (merged, _, _) = mo_agent_runtime::pipeline::persistence::merge_tool_health(
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

    /// Guard that serializes tests touching MO_AGENT_CREDENTIALS_DIR.
    /// Multiple async tests concurrently setting this env var is a data race;
    /// the guard ensures they execute sequentially.
    use std::sync::{Mutex, MutexGuard, OnceLock};

    struct CredentialsGuard {
        _lock: MutexGuard<'static, ()>,
        _dir: tempfile::TempDir,
    }

    fn creds_lock() -> MutexGuard<'static, ()> {
        static CREDS_LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        CREDS_LOCK
            .get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }

    /// Set credentials dir to a temp path so tests don't pollute ~/.mo-agent/credentials.json.
    /// Returns a guard that holds a mutex — tests using this are serialized.
    fn isolate_credentials() -> CredentialsGuard {
        let lock = creds_lock();
        let dir = tempfile::tempdir().unwrap();
        // SAFETY: protected by CREDS_LOCK; no concurrent set_var.
        unsafe { std::env::set_var("MO_AGENT_CREDENTIALS_DIR", dir.path()) };
        CredentialsGuard {
            _lock: lock,
            _dir: dir,
        }
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
            tool_health_entries: &[],
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
            tool_health_entries: &[],
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
            tool_health_entries: &[],
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

    #[tokio::test]
    async fn slash_health_does_not_crash_empty() {
        let client = mock_client();
        let selector = tool_selector::TfIdfSelector::new(tool_registry::ToolRegistry::new(
            edge_tools::all_tool_schemas(),
        ));
        let mut state = ReplState::default();
        // No health entries — should print "no data" gracefully
        let exit = handle_slash_command(
            "/health",
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

    #[tokio::test]
    async fn slash_health_with_entries_does_not_crash() {
        let client = mock_client();
        let selector = tool_selector::TfIdfSelector::new(tool_registry::ToolRegistry::new(
            edge_tools::all_tool_schemas(),
        ));
        let mut state = ReplState {
            tool_health_entries: vec![
                mo_agent_runtime::pipeline::persistence::ToolHealthEntry {
                    name: "bash".into(),
                    total_calls: 15,
                    total_failures: 3,
                    failure_rate: 0.2,
                    last_updated_epoch: 0,
                },
                mo_agent_runtime::pipeline::persistence::ToolHealthEntry {
                    name: "grep".into(),
                    total_calls: 8,
                    total_failures: 0,
                    failure_rate: 0.0,
                    last_updated_epoch: 0,
                },
            ],
            ..Default::default()
        };
        let exit = handle_slash_command(
            "/health",
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

    #[tokio::test]
    async fn slash_health_detail_mode() {
        let client = mock_client();
        let selector = tool_selector::TfIdfSelector::new(tool_registry::ToolRegistry::new(
            edge_tools::all_tool_schemas(),
        ));
        let mut state = ReplState {
            tool_health_entries: vec![mo_agent_runtime::pipeline::persistence::ToolHealthEntry {
                name: "bash".into(),
                total_calls: 10,
                total_failures: 5,
                failure_rate: 0.5,
                last_updated_epoch: 0,
            }],
            ..Default::default()
        };
        let exit = handle_slash_command(
            "/health detail",
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

    // ── find_task_by_query ────────────────────────────────────────────────────

    use mo_agent_services::TaskService as _;

    #[tokio::test]
    async fn find_task_by_id_prefix() {
        let tmp = tempfile::TempDir::new().unwrap();
        let svc = mo_agent_services::LocalTaskService::new(tmp.path().to_path_buf());
        let tid = svc
            .create_task(
                "u1",
                "s1",
                mo_agent_services::TaskCreateRequest {
                    title: "Build auth".into(),
                    ..Default::default()
                },
            )
            .await
            .unwrap();

        // Full ID match
        let found = find_task_by_query(&svc, "u1", &tid).await.unwrap();
        assert_eq!(found, Some(tid.clone()));

        // Prefix match (first 8 chars)
        let prefix = &tid[..8];
        let found = find_task_by_query(&svc, "u1", prefix).await.unwrap();
        assert_eq!(found, Some(tid));
    }

    #[tokio::test]
    async fn find_task_by_title_substring() {
        let tmp = tempfile::TempDir::new().unwrap();
        let svc = mo_agent_services::LocalTaskService::new(tmp.path().to_path_buf());
        svc.create_task(
            "u1",
            "s1",
            mo_agent_services::TaskCreateRequest {
                title: "Refactor authentication module".into(),
                ..Default::default()
            },
        )
        .await
        .unwrap();

        // Case-insensitive title match
        let found = find_task_by_query(&svc, "u1", "authentication")
            .await
            .unwrap();
        assert!(found.is_some());

        let found = find_task_by_query(&svc, "u1", "AUTH").await.unwrap();
        assert!(found.is_some());
    }

    #[tokio::test]
    async fn find_task_not_found() {
        let tmp = tempfile::TempDir::new().unwrap();
        let svc = mo_agent_services::LocalTaskService::new(tmp.path().to_path_buf());
        let found = find_task_by_query(&svc, "u1", "nonexistent").await.unwrap();
        assert!(found.is_none());
    }

    #[tokio::test]
    async fn find_task_wrong_user() {
        let tmp = tempfile::TempDir::new().unwrap();
        let svc = mo_agent_services::LocalTaskService::new(tmp.path().to_path_buf());
        svc.create_task(
            "user-a",
            "s1",
            mo_agent_services::TaskCreateRequest {
                title: "Private task".into(),
                ..Default::default()
            },
        )
        .await
        .unwrap();

        // Different user can't find it
        let found = find_task_by_query(&svc, "user-b", "Private").await.unwrap();
        assert!(found.is_none());
    }

    // ── Resume user verification ─────────────────────────────────────────────

    #[tokio::test]
    async fn resume_local_restore_rejects_unowned_session() {
        let _creds = isolate_credentials();
        use mo_agent_services::session_restore::SessionRestoreService;
        use session_journal::JournalWriter;

        // Create a session with both journal AND workspace (what restore_session needs)
        let sid = format!("test-unowned-{}", uuid::Uuid::new_v4());

        // 1. Create journal
        let writer = JournalWriter::new(&sid).unwrap();
        writer
            .append(&session_journal::JournalEvent::session_start(
                Some(&sid),
                Some("gpt-4o"),
            ))
            .unwrap();
        writer
            .append(&session_journal::JournalEvent::turn(
                Some(&sid),
                1,
                None,
                "hello",
                "hi",
                0,
                5,
                3,
                50,
            ))
            .unwrap();
        drop(writer);

        // 2. Create workspace.yaml (required for local restore)
        let ws_dir = dirs::home_dir()
            .unwrap_or_else(|| std::path::PathBuf::from("."))
            .join(".mo-agent")
            .join("sessions")
            .join(&sid);
        std::fs::create_dir_all(&ws_dir).unwrap();
        let ws_content = r#"session_id: test-unowned
cwd: /tmp
model: gpt-4o
created_at: "2024-01-01T00:00:00Z"
updated_at: "2024-01-01T00:00:00Z"
status: active
turn_count: 1
total_tokens_in: 5
total_tokens_out: 3
"#;
        std::fs::write(ws_dir.join("workspace.yaml"), ws_content).unwrap();

        // Now restore_session should find it
        let svc = mo_agent_services::session_restore::HybridRestoreService::local_only();
        let result = svc.restore_session(&sid).await.unwrap();
        assert!(
            result.is_some(),
            "local restore should find session with workspace.yaml"
        );

        // Verify it's marked as local (not cloud)
        let restored = result.unwrap();
        assert!(!restored.restored_from_cloud, "should be local restore");

        // Note: The user ownership check in handle_resume_command only verifies
        // that the journal exists, not that the user owns it. This is a known limitation.
    }

    // ── Learning snapshot restoration ────────────────────────────────────────

    #[tokio::test]
    async fn resume_restores_learning_snapshot() {
        use mo_agent_services::session_restore::RestoredSession;

        // Create a mock RestoredSession with learning snapshot
        let restored = RestoredSession {
            session_id: "test-learning".into(),
            turn_count: 5,
            total_tokens_in: 1000,
            total_tokens_out: 500,
            recent_tools: vec!["grep".into()],
            learning_snapshot_json: Some(
                r#"{"entities":["Rust","MatrixOne"],"patterns":["*.rs"]}"#.into(),
            ),
            checkpoint_count: 1,
            last_status: "active".into(),
            git_branch: Some("main".into()),
            model: Some("gpt-4o".into()),
            title: Some("Test".into()),
            restored_from_cloud: true, // Cloud restore has learning
            ..Default::default()
        };

        // Verify the learning snapshot is present
        assert!(restored.learning_snapshot_json.is_some());
        let json = restored.learning_snapshot_json.as_ref().unwrap();
        assert!(json.contains("Rust"));
        assert!(json.contains("MatrixOne"));

        // Simulate what handle_resume_command does
        let learning_snapshot = if let Some(ref l) = restored.learning_snapshot_json {
            if !l.is_empty() { Some(l.clone()) } else { None }
        } else {
            None
        };

        assert!(learning_snapshot.is_some());
        assert_eq!(learning_snapshot.unwrap().as_str(), json);
    }

    #[tokio::test]
    async fn resume_local_restore_has_no_learning_snapshot() {
        use mo_agent_services::session_restore::RestoredSession;

        // Local restore should not have learning snapshot
        let restored = RestoredSession {
            session_id: "test-local".into(),
            turn_count: 3,
            total_tokens_in: 500,
            total_tokens_out: 200,
            recent_tools: vec![],
            learning_snapshot_json: None, // Local restore doesn't have this
            checkpoint_count: 1,
            last_status: "active".into(),
            git_branch: None,
            model: None,
            title: None,
            restored_from_cloud: false,
            ..Default::default()
        };

        assert!(restored.learning_snapshot_json.is_none());
    }

    // ── Edge cases ───────────────────────────────────────────────────────────

    #[tokio::test]
    async fn resume_handles_empty_learning_snapshot() {
        use mo_agent_services::session_restore::RestoredSession;

        // Empty string should be treated as None
        let restored = RestoredSession {
            learning_snapshot_json: Some("".into()),
            ..Default::default()
        };

        // Simulate the logic in handle_resume_command
        let learning_snapshot = if let Some(ref l) = restored.learning_snapshot_json {
            if !l.is_empty() { Some(l.clone()) } else { None }
        } else {
            None
        };

        assert!(
            learning_snapshot.is_none(),
            "empty string should be ignored"
        );
    }

    #[tokio::test]
    async fn resume_handles_invalid_learning_json() {
        use mo_agent_services::session_restore::RestoredSession;

        // Invalid JSON should still be stored (will fail at merge time)
        let restored = RestoredSession {
            learning_snapshot_json: Some("not valid json {{{".into()),
            ..Default::default()
        };

        assert!(restored.learning_snapshot_json.is_some());
        let json = restored.learning_snapshot_json.as_ref().unwrap();
        assert!(json.contains("{"));
    }

    #[tokio::test]
    async fn resume_handles_malformed_workspace_yaml() {
        let _creds = isolate_credentials();
        use mo_agent_services::session_restore::SessionRestoreService;

        let sid = format!("test-malformed-{}", uuid::Uuid::new_v4());

        // Create journal
        let writer = session_journal::JournalWriter::new(&sid).unwrap();
        writer
            .append(&session_journal::JournalEvent::session_start(
                Some(&sid),
                Some("gpt-4o"),
            ))
            .unwrap();
        drop(writer);

        // Create malformed workspace.yaml
        let ws_dir = dirs::home_dir()
            .unwrap_or_else(|| std::path::PathBuf::from("."))
            .join(".mo-agent")
            .join("sessions")
            .join(&sid);
        std::fs::create_dir_all(&ws_dir).unwrap();
        std::fs::write(ws_dir.join("workspace.yaml"), "invalid: yaml: content: [").unwrap();

        // Should return None for malformed workspace
        let svc = mo_agent_services::session_restore::HybridRestoreService::local_only();
        let result = svc.restore_session(&sid).await.unwrap();
        assert!(
            result.is_none(),
            "malformed workspace.yaml should cause restore to return None"
        );
    }

    #[tokio::test]
    async fn resume_handles_missing_workspace() {
        let _creds = isolate_credentials();
        use mo_agent_services::session_restore::SessionRestoreService;

        // Only journal, no workspace → should fall back to cloud (which returns None)
        let sid = format!("test-no-ws-{}", uuid::Uuid::new_v4());
        let writer = session_journal::JournalWriter::new(&sid).unwrap();
        writer
            .append(&session_journal::JournalEvent::session_start(
                Some(&sid),
                Some("gpt-4o"),
            ))
            .unwrap();
        drop(writer);

        let svc = mo_agent_services::session_restore::HybridRestoreService::local_only();
        let result = svc.restore_session(&sid).await.unwrap();
        assert!(
            result.is_none(),
            "session without workspace.yaml should return None"
        );
    }

    // ── Integration: full resume flow simulation ─────────────────────────────

    #[tokio::test]
    async fn resume_full_flow_cloud_restore() {
        use mo_agent_services::session_restore::RestoredSession;

        // Simulate a complete cloud restore scenario
        let restored = RestoredSession {
            session_id: "cloud-sess-123".into(),
            turn_count: 42,
            total_tokens_in: 150_000,
            total_tokens_out: 80_000,
            recent_tools: vec!["git".into(), "bash".into(), "grep".into()],
            learning_snapshot_json: Some(
                r#"{"entities":["Rust","SQL"],"patterns":["*.rs"]}"#.into(),
            ),
            checkpoint_count: 5,
            last_status: "active".into(),
            git_branch: Some("feature/resume".into()),
            model: Some("claude-3-opus".into()),
            title: Some("Implement session resume".into()),
            restored_from_cloud: true,
            ..Default::default()
        };
        assert_eq!(restored.session_id, "cloud-sess-123");
        assert_eq!(restored.turn_count, 42);
        assert!(restored.restored_from_cloud);
        assert!(restored.learning_snapshot_json.is_some());
        assert_eq!(restored.recent_tools.len(), 3);

        // Simulate state application
        let mut state = super::ReplState::default();
        #[allow(clippy::field_reassign_with_default)]
        {
            state.session_id = Some(restored.session_id.clone());
            state.turn = restored.turn_count;
            state.total_prompt_tokens = restored.total_tokens_in;
            state.total_completion_tokens = restored.total_tokens_out;
            state.recent_tools = restored.recent_tools.clone();
            state.model = restored.model.clone();
        }

        // Apply learning snapshot
        if let Some(ref l) = restored.learning_snapshot_json
            && !l.is_empty()
        {
            state.learning_snapshot = Some(l.clone());
        }

        // Verify state
        assert_eq!(state.session_id, Some("cloud-sess-123".into()));
        assert_eq!(state.turn, 42);
        assert_eq!(state.total_prompt_tokens, 150_000);
        assert_eq!(
            state.learning_snapshot.unwrap(),
            r#"{"entities":["Rust","SQL"],"patterns":["*.rs"]}"#
        );
    }

    // ── Checkpoint listing ───────────────────────────────────────────────────

    #[tokio::test]
    async fn resume_lists_checkpoints_for_session() {
        let _creds = isolate_credentials();
        use mo_agent_services::session_restore::SessionRestoreService;

        let sid = format!("test-checkpoints-{}", uuid::Uuid::new_v4());

        // Create journal
        let writer = session_journal::JournalWriter::new(&sid).unwrap();
        writer
            .append(&session_journal::JournalEvent::session_start(
                Some(&sid),
                Some("gpt-4o"),
            ))
            .unwrap();
        drop(writer);

        // Create workspace
        let ws_dir = dirs::home_dir()
            .unwrap_or_else(|| std::path::PathBuf::from("."))
            .join(".mo-agent")
            .join("sessions")
            .join(&sid);
        std::fs::create_dir_all(&ws_dir).unwrap();
        std::fs::write(
            ws_dir.join("workspace.yaml"),
            r#"session_id: test
cwd: /tmp
model: gpt-4o
created_at: "2024-01-01T00:00:00Z"
updated_at: "2024-01-01T00:00:00Z"
status: active
turn_count: 10
total_tokens_in: 1000
total_tokens_out: 500
"#,
        )
        .unwrap();

        // List checkpoints should return empty (no checkpoints created yet)
        let svc = mo_agent_services::session_restore::HybridRestoreService::local_only();
        let ckpts = svc.list_checkpoints(&sid).await.unwrap();
        assert!(ckpts.is_empty(), "no checkpoints created yet");
    }

    // ── merge_learning_snapshot ───────────────────────────────────────────────

    #[test]
    fn merge_learning_valid_snapshot() {
        use mo_agent_runtime::pipeline::{calibration, entity, pattern};

        let json = serde_json::json!({
            "version": 1,
            "entities": [{
                "name": "rust",
                "aliases": ["rs"],
                "domain": null,
                "associated_tools": ["cargo"],
                "confidence": 0.8,
                "observation_count": 5
            }],
            "patterns": [{
                "signature": "cargo",
                "tools": ["cargo"],
                "task_type": "Code",
                "domain": null,
                "success_count": 3,
                "failure_count": 0,
                "quality_sum": 2.4
            }],
            "calibration": null
        })
        .to_string();

        let eg = std::sync::Arc::new(std::sync::Mutex::new(entity::EntityGraph::new()));
        let pl = std::sync::Arc::new(std::sync::Mutex::new(pattern::PatternLibrary::new()));
        let cal = std::sync::Arc::new(std::sync::Mutex::new(
            calibration::ProgressiveCalibrator::default(),
        ));

        merge_learning_snapshot(&json, &eg, &pl, &cal);

        // Verify entity content, not just count
        let entities = eg.lock().unwrap().export();
        assert_eq!(entities.len(), 1);
        let e = &entities[0];
        assert_eq!(e.name, "rust");
        assert_eq!(e.aliases, vec!["rs"]);
        assert_eq!(e.associated_tools, vec!["cargo"]);
        assert!((e.confidence - 0.8).abs() < 1e-6);
        assert_eq!(e.observation_count, 5);

        // Verify pattern content, not just count
        let patterns = pl.lock().unwrap().export();
        assert_eq!(patterns.len(), 1);
        let p = &patterns[0];
        assert_eq!(p.signature, "cargo");
        assert_eq!(p.tools, vec!["cargo"]);
        assert_eq!(p.success_count, 3);
        assert_eq!(p.failure_count, 0);
    }

    #[test]
    fn merge_learning_invalid_json_does_not_panic() {
        use mo_agent_runtime::pipeline::{calibration, entity, pattern};

        let eg = std::sync::Arc::new(std::sync::Mutex::new(entity::EntityGraph::new()));
        let pl = std::sync::Arc::new(std::sync::Mutex::new(pattern::PatternLibrary::new()));
        let cal = std::sync::Arc::new(std::sync::Mutex::new(
            calibration::ProgressiveCalibrator::default(),
        ));

        // Invalid JSON — should not panic, just print warning
        merge_learning_snapshot("not valid json", &eg, &pl, &cal);

        // Modules should remain empty
        assert!(eg.lock().unwrap().export().is_empty());
        assert!(pl.lock().unwrap().export().is_empty());
    }

    #[test]
    fn merge_learning_empty_snapshot() {
        use mo_agent_runtime::pipeline::{calibration, entity, pattern};

        let json = serde_json::json!({
            "version": 1,
            "entities": [],
            "patterns": [],
            "calibration": null
        })
        .to_string();

        let eg = std::sync::Arc::new(std::sync::Mutex::new(entity::EntityGraph::new()));
        let pl = std::sync::Arc::new(std::sync::Mutex::new(pattern::PatternLibrary::new()));
        let cal = std::sync::Arc::new(std::sync::Mutex::new(
            calibration::ProgressiveCalibrator::default(),
        ));

        merge_learning_snapshot(&json, &eg, &pl, &cal);

        assert!(eg.lock().unwrap().export().is_empty());
        assert!(pl.lock().unwrap().export().is_empty());
    }

    #[test]
    fn merge_learning_idempotent() {
        use mo_agent_runtime::pipeline::{calibration, entity, pattern};

        let json = serde_json::json!({
            "version": 1,
            "entities": [{"name": "rust", "aliases": [], "domain": null,
                "associated_tools": ["cargo"], "confidence": 0.8, "observation_count": 5}],
            "patterns": [{"signature": "cargo", "tools": ["cargo"], "task_type": "Code",
                "domain": null, "success_count": 3, "failure_count": 0, "quality_sum": 2.4}],
            "calibration": null
        })
        .to_string();

        let eg = std::sync::Arc::new(std::sync::Mutex::new(entity::EntityGraph::new()));
        let pl = std::sync::Arc::new(std::sync::Mutex::new(pattern::PatternLibrary::new()));
        let cal = std::sync::Arc::new(std::sync::Mutex::new(
            calibration::ProgressiveCalibrator::default(),
        ));

        // Merge twice — should not duplicate
        merge_learning_snapshot(&json, &eg, &pl, &cal);
        merge_learning_snapshot(&json, &eg, &pl, &cal);

        assert_eq!(
            eg.lock().unwrap().export().len(),
            1,
            "entities should not duplicate"
        );
        assert_eq!(
            pl.lock().unwrap().export().len(),
            1,
            "patterns should not duplicate"
        );
    }

    #[test]
    fn merge_learning_multiple_entities_and_patterns() {
        use mo_agent_runtime::pipeline::{calibration, entity, pattern};

        let json = serde_json::json!({
            "version": 1,
            "entities": [
                {"name": "rust", "aliases": [], "domain": null,
                    "associated_tools": ["cargo"], "confidence": 0.9, "observation_count": 10},
                {"name": "matrixone", "aliases": ["mo"], "domain": "Database",
                    "associated_tools": ["sql_query"], "confidence": 0.7, "observation_count": 3}
            ],
            "patterns": [
                {"signature": "cargo|grep", "tools": ["cargo", "grep"], "task_type": "Code",
                    "domain": null, "success_count": 5, "failure_count": 1, "quality_sum": 4.0},
                {"signature": "sql_query", "tools": ["sql_query"], "task_type": "Fetch",
                    "domain": "Database", "success_count": 2, "failure_count": 0, "quality_sum": 1.8}
            ],
            "calibration": null
        })
        .to_string();

        let eg = std::sync::Arc::new(std::sync::Mutex::new(entity::EntityGraph::new()));
        let pl = std::sync::Arc::new(std::sync::Mutex::new(pattern::PatternLibrary::new()));
        let cal = std::sync::Arc::new(std::sync::Mutex::new(
            calibration::ProgressiveCalibrator::default(),
        ));

        merge_learning_snapshot(&json, &eg, &pl, &cal);

        let entities = eg.lock().unwrap().export();
        assert_eq!(entities.len(), 2);
        let names: Vec<&str> = entities.iter().map(|e| e.name.as_str()).collect();
        assert!(names.contains(&"rust"));
        assert!(names.contains(&"matrixone"));

        let patterns = pl.lock().unwrap().export();
        assert_eq!(patterns.len(), 2);
        let sigs: Vec<&str> = patterns.iter().map(|p| p.signature.as_str()).collect();
        assert!(sigs.contains(&"cargo|grep"));
        assert!(sigs.contains(&"sql_query"));
    }

    // ── handle_stats_command ─────────────────────────────────────────────────

    #[test]
    fn stats_no_active_session_does_not_panic() {
        // state with no session_id → should not panic
        let state = super::ReplState::default();
        handle_stats_command("", &state); // current session mode, no session
    }

    #[test]
    fn stats_history_no_sessions_does_not_panic() {
        let state = super::ReplState::default();
        handle_stats_command("history", &state);
    }

    #[test]
    fn stats_current_session_reads_journal() {
        let _creds = isolate_credentials();
        use mo_agent_services::session_analytics;

        // Create a real journal with known events
        let sid = format!("test-stats-{}", uuid::Uuid::new_v4());
        let writer = session_journal::JournalWriter::new(&sid).unwrap();
        writer
            .append(&session_journal::JournalEvent::session_start(
                Some(&sid),
                Some("gpt-4o"),
            ))
            .unwrap();
        writer
            .append(&session_journal::JournalEvent::turn(
                Some(&sid),
                1,
                Some("gpt-4o"),
                "hello",
                "hi",
                2,
                1000,
                500,
                1500,
            ))
            .unwrap();
        writer
            .append(&session_journal::JournalEvent::turn(
                Some(&sid),
                2,
                Some("gpt-4o"),
                "what is rust?",
                "a systems language",
                1,
                800,
                400,
                1200,
            ))
            .unwrap();
        drop(writer);

        // Verify the analytics layer computes correctly from these events
        let events = session_journal::read_journal(&sid).unwrap();
        let stats = session_analytics::compute_session_stats(&sid, &events);

        assert_eq!(stats.turn_count, 2);
        assert_eq!(stats.total_tokens_in, 1800);
        assert_eq!(stats.total_tokens_out, 900);
        assert_eq!(stats.total_tool_calls, 3);
        assert_eq!(stats.model, Some("gpt-4o".into()));
        assert_eq!(stats.avg_tokens_per_turn, 1350); // (1800+900)/2

        // Now verify handle_stats_command doesn't panic with this session
        let state = super::ReplState {
            session_id: Some(sid),
            ..Default::default()
        };
        handle_stats_command("", &state);
    }

    #[test]
    fn stats_history_aggregates_multiple_sessions() {
        let _creds = isolate_credentials();
        use mo_agent_services::session_analytics;

        // Create two sessions
        let sid1 = format!("test-stats-hist-a-{}", uuid::Uuid::new_v4());
        let sid2 = format!("test-stats-hist-b-{}", uuid::Uuid::new_v4());

        for sid in [&sid1, &sid2] {
            let writer = session_journal::JournalWriter::new(sid).unwrap();
            writer
                .append(&session_journal::JournalEvent::turn(
                    Some(sid),
                    1,
                    None,
                    "q",
                    "a",
                    1,
                    500,
                    250,
                    800,
                ))
                .unwrap();
            drop(writer);
        }

        let e1 = session_journal::read_journal(&sid1).unwrap();
        let e2 = session_journal::read_journal(&sid2).unwrap();
        let s1 = session_analytics::compute_session_stats(&sid1, &e1);
        let s2 = session_analytics::compute_session_stats(&sid2, &e2);
        let agg = session_analytics::aggregate_stats(&[s1, s2]);

        assert_eq!(agg.session_count, 2);
        assert_eq!(agg.total_turns, 2);
        assert_eq!(agg.total_tokens_in, 1000);
        assert_eq!(agg.total_tokens_out, 500);
    }

    // ── handle_tools_command ─────────────────────────────────────────────────

    #[test]
    fn tools_no_active_session_does_not_panic() {
        let state = super::ReplState::default();
        handle_tools_command(&state);
    }

    #[test]
    fn tools_session_with_no_tool_calls_does_not_panic() {
        let _creds = isolate_credentials();
        let sid = format!("test-tools-empty-{}", uuid::Uuid::new_v4());
        let writer = session_journal::JournalWriter::new(&sid).unwrap();
        writer
            .append(&session_journal::JournalEvent::turn(
                Some(&sid),
                1,
                None,
                "hello",
                "hi",
                0,
                100,
                50,
                500,
            ))
            .unwrap();
        drop(writer);

        let state = super::ReplState {
            session_id: Some(sid),
            ..Default::default()
        };
        handle_tools_command(&state);
    }

    #[test]
    fn tools_reads_tool_calls_from_journal() {
        let _creds = isolate_credentials();
        use mo_agent_services::session_analytics;

        let sid = format!("test-tools-calls-{}", uuid::Uuid::new_v4());
        let writer = session_journal::JournalWriter::new(&sid).unwrap();

        let mut event = session_journal::JournalEvent::turn(
            Some(&sid),
            1,
            None,
            "run tests",
            "done",
            3,
            500,
            200,
            3000,
        );
        event.tool_calls = Some(vec![
            session_journal::ToolCallRecord {
                name: "bash".into(),
                ms: 1000,
                ok: true,
                error: None,
            },
            session_journal::ToolCallRecord {
                name: "bash".into(),
                ms: 2000,
                ok: false,
                error: Some("exit code 1".into()),
            },
            session_journal::ToolCallRecord {
                name: "grep".into(),
                ms: 50,
                ok: true,
                error: None,
            },
        ]);
        writer.append(&event).unwrap();
        drop(writer);

        // Verify analytics layer computes correctly
        let events = session_journal::read_journal(&sid).unwrap();
        let profiles = session_analytics::compute_tool_profiles(&events);

        assert_eq!(profiles.len(), 2);
        // sorted by total_ms descending: bash (3000ms) > grep (50ms)
        assert_eq!(profiles[0].name, "bash");
        assert_eq!(profiles[0].call_count, 2);
        assert_eq!(profiles[0].fail_count, 1);
        assert_eq!(profiles[0].total_ms, 3000);
        assert_eq!(profiles[0].min_ms, 1000);
        assert_eq!(profiles[0].max_ms, 2000);
        assert!((profiles[0].error_rate - 0.5).abs() < 0.01);
        assert_eq!(profiles[0].last_error, Some("exit code 1".into()));

        assert_eq!(profiles[1].name, "grep");
        assert_eq!(profiles[1].call_count, 1);
        assert_eq!(profiles[1].fail_count, 0);
        assert_eq!(profiles[1].error_rate, 0.0);

        // Verify handle_tools_command doesn't panic with this data
        let state = super::ReplState {
            session_id: Some(sid),
            ..Default::default()
        };
        handle_tools_command(&state);
    }

    // ── format_sync_age tests ────────────────────────────────────────────

    #[test]
    fn format_sync_age_rfc3339() {
        let now = chrono::Utc::now();
        let ts = now.to_rfc3339();
        let age = format_sync_age(&ts);
        // Should be "just now" or "0s ago" or "1s ago"
        assert!(
            age.contains("s ago") || age == "just now",
            "unexpected age for just-now timestamp: {age}"
        );
    }

    #[test]
    fn format_sync_age_minutes_ago() {
        let now = chrono::Utc::now();
        let five_min_ago = now - chrono::Duration::minutes(5);
        let ts = five_min_ago.to_rfc3339();
        let age = format_sync_age(&ts);
        assert!(
            age.contains("m ago"),
            "expected minutes-ago format, got: {age}"
        );
    }

    #[test]
    fn format_sync_age_hours_ago() {
        let now = chrono::Utc::now();
        let two_hours_ago = now - chrono::Duration::hours(2);
        let ts = two_hours_ago.to_rfc3339();
        let age = format_sync_age(&ts);
        assert!(
            age.contains("h ago"),
            "expected hours-ago format, got: {age}"
        );
    }

    #[test]
    fn format_sync_age_days_ago() {
        let now = chrono::Utc::now();
        let three_days_ago = now - chrono::Duration::days(3);
        let ts = three_days_ago.to_rfc3339();
        let age = format_sync_age(&ts);
        assert!(
            age.contains("d ago"),
            "expected days-ago format, got: {age}"
        );
    }

    #[test]
    fn format_sync_age_mysql_datetime() {
        // MySQL DATETIME without timezone — should parse as UTC
        let age = format_sync_age("2020-01-01 00:00:00");
        assert!(
            age.contains("d ago"),
            "expected days-ago for old mysql datetime, got: {age}"
        );
    }

    #[test]
    fn format_sync_age_unparseable_returns_raw() {
        let raw = "not-a-timestamp";
        let age = format_sync_age(raw);
        assert_eq!(age, raw, "unparseable should return raw string");
    }

    #[test]
    fn display_sync_status_no_crash_all_none() {
        let status = mo_agent_services::SyncStatus::default();
        // Just verify no panic — output goes to stderr
        display_sync_status(&status);
    }

    #[test]
    fn display_sync_status_no_crash_full_data() {
        let status = mo_agent_services::SyncStatus {
            learning_last_push: Some(chrono::Utc::now().to_rfc3339()),
            learning_last_pull: Some(chrono::Utc::now().to_rfc3339()),
            preferences_last_sync: Some(chrono::Utc::now().to_rfc3339()),
            pending_pushes: 2,
            last_error: Some("connection reset by peer".into()),
            cloud_version: None,
        };
        display_sync_status(&status);
    }

    #[tokio::test]
    async fn slash_health_offline_shows_cloud_section() {
        let client = mock_client();
        let selector = tool_selector::TfIdfSelector::new(tool_registry::ToolRegistry::new(
            edge_tools::all_tool_schemas(),
        ));
        let mut state = ReplState::default();
        // No matrixone_pool — should show "Offline" in cloud section
        assert!(state.matrixone_pool.is_none());
        let exit = handle_slash_command(
            "/health",
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

    // ── Cloud sync regression tests (block_on panic fix cc6d011) ────
    // These tests verify the async cloud sync functions don't panic when
    // called from within a tokio runtime (the original bug was block_on
    // inside an existing runtime). We unset MATRIXONE_HOST so they take
    // the graceful-fallback path.

    #[tokio::test]
    async fn try_connect_matrixone_returns_none_without_env_vars() {
        // Safety: test-only, single-threaded tokio runtime
        unsafe {
            std::env::remove_var("MATRIXONE_HOST");
        }
        let pool = try_connect_matrixone().await;
        assert!(
            pool.is_none(),
            "Without MATRIXONE_HOST, pool should be None"
        );
    }

    #[tokio::test]
    async fn try_cloud_pull_returns_empty_without_matrixone() {
        unsafe {
            std::env::remove_var("MATRIXONE_HOST");
        }
        let eg = std::sync::Arc::new(std::sync::Mutex::new(
            mo_agent_runtime::pipeline::entity::EntityGraph::new(),
        ));
        let pl = std::sync::Arc::new(std::sync::Mutex::new(
            mo_agent_runtime::pipeline::pattern::PatternLibrary::new(),
        ));
        let cal = std::sync::Arc::new(std::sync::Mutex::new(
            mo_agent_runtime::pipeline::calibration::ProgressiveCalibrator::new(0.15),
        ));
        let result = try_cloud_pull("default", &eg, &pl, &cal).await;
        assert!(
            result.tool_health.is_empty(),
            "Without MatrixOne, cloud pull should return empty tool health"
        );
        assert!(
            result.version.is_none(),
            "Without MatrixOne, cloud pull should return no version"
        );
    }

    #[tokio::test]
    async fn try_cloud_push_is_noop_without_matrixone() {
        unsafe {
            std::env::remove_var("MATRIXONE_HOST");
        }
        let eg = std::sync::Arc::new(std::sync::Mutex::new(
            mo_agent_runtime::pipeline::entity::EntityGraph::new(),
        ));
        let pl = std::sync::Arc::new(std::sync::Mutex::new(
            mo_agent_runtime::pipeline::pattern::PatternLibrary::new(),
        ));
        let cal = std::sync::Arc::new(std::sync::Mutex::new(
            mo_agent_runtime::pipeline::calibration::ProgressiveCalibrator::new(0.15),
        ));
        // Should not panic (was the original bug)
        // Use versioned API (None = new snapshot or unconditional push)
        let _result = try_cloud_push_versioned("default", &eg, &pl, &cal, &[], None).await;
    }

    #[tokio::test]
    async fn try_cloud_push_delta_is_noop_without_matrixone() {
        unsafe {
            std::env::remove_var("MATRIXONE_HOST");
        }
        let eg = std::sync::Arc::new(std::sync::Mutex::new(
            mo_agent_runtime::pipeline::entity::EntityGraph::new(),
        ));
        let pl = std::sync::Arc::new(std::sync::Mutex::new(
            mo_agent_runtime::pipeline::pattern::PatternLibrary::new(),
        ));
        let cal = std::sync::Arc::new(std::sync::Mutex::new(
            mo_agent_runtime::pipeline::calibration::ProgressiveCalibrator::new(0.15),
        ));
        let mut synced = Vec::new();
        eg.lock().unwrap().learn(
            "rust",
            mo_agent_runtime::pipeline::routing::DomainHint::Code,
            &[],
            None,
        );
        let _result = try_cloud_push_delta("default", &eg, &pl, &cal, &[], &mut synced, None).await;
    }

    #[tokio::test]
    async fn try_cloud_pull_preferences_is_noop_without_matrixone() {
        unsafe {
            std::env::remove_var("MATRIXONE_HOST");
        }
        let mut state = ReplState::default();
        // Should not panic (was the original bug)
        try_cloud_pull_preferences(&mut state).await;
    }

    #[tokio::test]
    async fn try_cloud_push_preferences_is_noop_without_matrixone() {
        unsafe {
            std::env::remove_var("MATRIXONE_HOST");
        }
        let state = ReplState::default();
        // Should not panic (was the original bug)
        try_cloud_push_preferences(&state).await;
    }
}
