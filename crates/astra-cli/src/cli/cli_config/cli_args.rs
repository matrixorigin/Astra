//! CLI argument structs and subcommand enums for clap.
//!
//! All main entry point arguments, subcommands, and their nested argument
//! types are defined here. These are used by `main.rs` for parsing and
//! by `command_router.rs` for dispatch.
//!
//! ## Observability (flags and environment)
//!
//! Applied in [`crate::cli::diagnostic_log::init_cli_observability`] immediately after [`Cli`] is parsed.
//!
//! **Priority:** `--log-file` → `ASTRA_LOG_FILE` → (`--diagnostic-log` or `ASTRA_DIAGNOSTIC_LOG=1`) for stderr.
//!
//! - **`--log-file <PATH>`** (hidden): append JSON lines to a file; overrides `ASTRA_LOG_FILE` when both are set.
//! - **`--diagnostic-log`** (hidden): structured [`tracing`] on stderr (`astra-logging`); same effect as `ASTRA_DIAGNOSTIC_LOG=1`.
//! - **`ASTRA_LOG_FILE`**: same as `--log-file` when the flag is absent.
//! - **`ASTRA_DIAGNOSTIC_LOG=1`**: stderr diagnostics when no file target is selected.
//!
//! See repository `README.md` for `RUST_LOG` / `ASTRA_LOG_FORMAT`.

use astra_core::DEFAULT_API_PORT;
use clap::{Args, Parser, Subcommand};
use std::path::PathBuf;

fn parse_permission_mode_arg(value: &str) -> Result<String, String> {
    value
        .parse::<crate::cli::permission_manager::PermissionMode>()
        .map(|mode| mode.to_string())
}

fn parse_explain_mode_arg(
    value: &str,
) -> Result<crate::cli::session::session_state::ExplainMode, String> {
    match value.trim().to_ascii_lowercase().as_str() {
        "on" | "true" => Ok(crate::cli::session::session_state::ExplainMode::On),
        "off" | "false" => Ok(crate::cli::session::session_state::ExplainMode::Off),
        "verbose" => Ok(crate::cli::session::session_state::ExplainMode::Verbose),
        other => Err(format!(
            "invalid explain mode `{other}` (expected on, off, or verbose)"
        )),
    }
}

#[cfg(feature = "harness")]
fn parse_benchmark_profile_arg(value: &str) -> Result<astra_harness::HarnessProfile, String> {
    value.parse()
}

#[derive(Parser, Debug)]
#[command(name = "astra", version)]
#[command(about = "AI agent CLI — run `astra` for interactive chat")]
pub(crate) struct Cli {
    /// API server base URL [env: ASTRA_API_URL] [config: api_url] [default: http://127.0.0.1:17001]
    #[arg(long)]
    pub api_url: Option<String>,
    /// Config profile name
    #[arg(long)]
    pub profile: Option<String>,
    /// Model to use (overrides config default_model)
    #[arg(long = "model")]
    pub model: Option<String>,
    /// Print mode: send prompt, print response, exit. No tools, no interaction.
    /// Usage: astra -p "your question" or echo "question" | astra -p
    #[arg(short = 'p', long = "print")]
    pub print: bool,
    /// Output format for --print mode
    #[arg(
        long = "output-format",
        default_value = "text",
        value_parser = ["text", "json", "stream-json"]
    )]
    pub output_format: String,
    /// Continue the most recent conversation
    #[arg(short = 'c', long = "continue")]
    pub continue_last: bool,
    /// Resume a specific session by ID (or prefix)
    #[arg(short = 'r', long = "resume")]
    pub resume: Option<String>,
    /// Skip approval prompts; hard safety denies still apply
    #[arg(short = 'y', long = "yes")]
    pub yes: bool,
    /// System prompt to prepend (useful with --print for scripting)
    #[arg(long = "system-prompt")]
    pub system_prompt: Option<String>,
    /// Comma or space-separated list of tool names to allow (e.g. "Bash Edit Read")
    #[arg(long = "allowed-tools", num_args = 1..)]
    pub allowed_tools: Vec<String>,
    /// Comma or space-separated list of tool names to deny (e.g. "Bash Edit")
    #[arg(long = "disallowed-tools", num_args = 1..)]
    pub disallowed_tools: Vec<String>,
    /// Additional directories to allow tool access to
    #[arg(long = "add-dir", num_args = 1..)]
    pub add_dir: Vec<String>,
    /// Enable verbose output (overrides config setting)
    #[arg(long = "verbose")]
    pub verbose: bool,
    /// Load MCP server config from JSON file(s) or inline JSON strings
    #[arg(long = "mcp-config", num_args = 1..)]
    pub mcp_config: Vec<String>,
    /// Overlay runtime settings from a JSON string or file path.
    ///
    /// Partial overlay: the JSON needs to mention only the fields you
    /// want to change; everything else keeps the value from config file
    /// + env + defaults. Inline vs. file is decided by a leading `{`.
    ///
    /// Examples:
    ///   astra -p 'fix tests' --model sonnet-4-6 \
    ///     --settings '{"memory":{"retrieval_top_k":8}}'
    ///   astra --settings overrides.json
    #[arg(long = "settings", value_name = "JSON-OR-PATH")]
    pub settings: Option<String>,
    /// Use a specific session ID (must be a valid UUID)
    #[arg(long = "session-id")]
    pub session_id: Option<String>,
    /// Set a display name for this session
    #[arg(short = 'n', long = "name")]
    pub session_name: Option<String>,
    /// Minimal mode: skip hooks, auto-memory, background prefetches.
    /// Only explicitly provided context (--system-prompt, --add-dir, --mcp-config) is used.
    #[arg(long = "bare")]
    pub bare: bool,
    /// Disable auto-loading of .astra/instructions.md project instructions
    #[arg(long = "no-instructions")]
    pub no_instructions: bool,
    /// Redact journal user/assistant content on disk (maps to ASTRA_JOURNAL_CONTENT_REDACT=1)
    #[arg(long = "no-journal-content")]
    pub no_journal_content: bool,
    /// Print startup timing for each initialization phase
    #[arg(long = "startup-trace")]
    pub startup_trace: bool,
    /// Emit structured tracing to stderr (same as ASTRA_DIAGNOSTIC_LOG=1); hidden from --help
    #[arg(long = "diagnostic-log", hide = true)]
    pub diagnostic_log: bool,
    /// Append JSON tracing lines to this file (overrides ASTRA_LOG_FILE env); hidden from --help
    #[arg(long = "log-file", value_name = "PATH", hide = true)]
    pub log_file: Option<String>,
    /// Trace profile for this session: "production" (lean) or "dev" (verbose).
    /// Production keeps coarse operational traces; Dev enables full Trace-level capture.
    #[arg(long = "trace-profile", value_parser = ["production", "dev", "custom"])]
    pub trace_profile: Option<String>,
    /// Minimum trace level for this session: error, warn, info, debug, trace.
    /// Overrides the profile default.
    #[arg(long = "trace-level", value_parser = ["error", "warn", "info", "debug", "trace"])]
    pub trace_level: Option<String>,
    /// Comma-separated trace categories to enable (e.g. "tool_calls,llm_exchanges,phase_transition").
    /// Use "all" for every category.
    #[arg(long = "trace-cat", value_name = "CATS")]
    pub trace_cat: Option<String>,
    #[command(subcommand)]
    pub(crate) command: Option<Command>,
}

#[derive(Subcommand, Debug)]
#[command(allow_external_subcommands = true)]
pub(crate) enum Command {
    /// Start the interactive TUI (default when no args given)
    Interactive,
    /// Start an Astra service process
    Serve(ServeArgs),
    /// Register a new account
    Register(RegisterArgs),
    /// Log in with stored credentials
    Login(LoginArgs),
    /// Show the current authenticated user
    Whoami,
    /// Refresh the current auth token
    Refresh,
    /// Log out and clear local credentials
    Logout,
    /// Check API health
    Health,
    /// Run a one-shot chat request
    Chat(ChatArgs),
    /// Replay a recorded session
    Replay(ReplayArgs),
    /// Inspect and manage sessions
    #[command(alias = "sessions")]
    #[command(subcommand)]
    Session(SessionCmd),
    /// Introspect persistent agent state
    #[command(name = "self")]
    #[command(subcommand)]
    SelfInspect(SelfCmd),
    /// Inspect available models
    #[command(alias = "models")]
    #[command(subcommand)]
    Model(ModelCmd),
    /// Inspect and manage skills
    #[command(alias = "skills")]
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
    /// Administrative operations
    Admin(crate::admin_cli::AdminArgs),
    /// Generate shell completion script
    Completion(CompletionArgs),
    /// Diagnose installation, config, and connectivity
    Doctor,
    /// Team orchestration and shared context management
    #[command(alias = "teams")]
    Team(TeamArgs),
    /// Start, inspect, and continue durable Work
    #[command(subcommand)]
    Work(WorkSubcommand),
    /// Memory search and inspection
    #[command(alias = "memories")]
    Memory(MemoryArgs),
    /// Review git changes with the agent
    Review(ReviewArgs),
    /// Search workspace content or changed files
    Grep(GrepArgs),
    /// Show git diffs
    Diff(DiffArgs),
    /// Permission mode and rule inspection
    #[command(visible_alias = "allow")]
    Permissions(PermissionsArgs),
    /// Inspect session checkpoints and turn payloads
    Debug(DebugArgs),
    /// Generate a bug report from current local state
    Bug(BugArgs),
    /// Inspect spawned agents
    #[command(alias = "agents")]
    Agent(AgentArgs),
    /// Inspect inter-agent messaging state
    Messaging(MessagingArgs),
    /// Dump the `/context` snapshot for a persisted session
    #[command(subcommand)]
    Context(ContextCmd),
    /// Direct message: astra "your question here"
    #[command(external_subcommand)]
    Message(Vec<String>),
}

impl Command {
    pub(crate) fn profile_identity_admission(
        &self,
    ) -> crate::cli::cli_config::cli_utils::CliProfileIdentityAdmission {
        use crate::cli::cli_config::cli_utils::CliProfileIdentityAdmission;

        match self {
            Self::Login(_) | Self::Register(_) => {
                CliProfileIdentityAdmission::AuthenticationBootstrap
            }
            Self::Admin(args) if args.is_authentication_bootstrap() => {
                CliProfileIdentityAdmission::AuthenticationBootstrap
            }
            _ => CliProfileIdentityAdmission::RequireBoundAccount,
        }
    }
}

impl Cli {
    /// Reject the ambiguous `astra <unknown> --help` shape before it can be
    /// interpreted as a direct model message and appended to a session.
    /// Direct-message shorthand remains supported; command-looking help must
    /// fail fast because its natural meaning is CLI discovery, not chat.
    pub(crate) fn validate_external_message_shorthand(&self) -> Result<(), String> {
        let Some(Command::Message(words)) = self.command.as_ref() else {
            return Ok(());
        };
        if words.len() == 2 && matches!(words[1].as_str(), "--help" | "-h") {
            return Err(format!(
                "unknown command `{}`; run `astra --help` to list commands, or use `astra chat -m ...` for an explicit message",
                words[0]
            ));
        }
        Ok(())
    }
}

/// Subcommands for the standalone `astra context` group.  Mirrors
/// the TUI's `/context` slash command but works without a running
/// TUI session — useful for forensic replay from a persisted session.
#[derive(Subcommand, Debug)]
pub(crate) enum ContextCmd {
    /// Dump a session's context as JSON, or a human-readable
    /// summary to stdout.
    ///
    /// Examples:
    ///     astra context dump                # most-recent session, JSON to ~/.astra/context-dumps/
    ///     astra context dump -s 01e363ed    # 8-char prefix (any unique prefix works)
    ///     astra context dump --summary      # plain-text summary to stdout, no file write
    ///     astra context dump -s abc -o snap.json
    #[command(verbatim_doc_comment)]
    Dump(ContextDumpArgs),
}

#[derive(clap::Args, Debug)]
pub(crate) struct ContextDumpArgs {
    /// Session id — any unique prefix (e.g. first 8 chars) works.
    /// When omitted, falls back to the most recently modified
    /// session in `~/.astra/sessions/`.
    #[arg(short = 's', long)]
    pub session: Option<String>,
    /// Write a JSON dump to this path instead of the default
    /// `~/.astra/context-dumps/<sid>-t<turn>-<ts>.json`.  Ignored
    /// when `--summary` is set.
    #[arg(short = 'o', long)]
    pub output: Option<String>,
    /// Print a human-readable summary to stdout instead of writing
    /// a JSON file.  Useful for `| grep`, CI logs, bug reports.
    #[arg(long)]
    pub summary: bool,
}

#[derive(Args, Debug)]
#[command(
    after_help = "Examples:\n  astra register --username alice --email alice@example.com --password secret"
)]
pub(crate) struct RegisterArgs {
    /// Username for the new account
    #[arg(long)]
    pub username: Option<String>,
    /// Email for the new account
    #[arg(long)]
    pub email: Option<String>,
    /// Password for the new account
    #[arg(long)]
    pub password: Option<String>,
}

#[derive(Args, Debug)]
#[command(after_help = "Examples:\n  astra login --username alice --password secret")]
pub(crate) struct LoginArgs {
    /// Username to log in with
    #[arg(long)]
    pub username: Option<String>,
    /// Password to log in with
    #[arg(long)]
    pub password: Option<String>,
}

#[derive(Args, Debug)]
#[command(
    after_help = "Examples:\n  astra serve\n  astra serve http --host 127.0.0.1 --port 17001\n  astra serve stdio\n\nModes:\n  http   Starts the Axum HTTP API server. This is also the default when no mode is provided.\n  stdio  Starts a long-lived app-server over stdin/stdout JSON-RPC. A parent process sends requests on stdin and reads events/responses from stdout, allowing one child process to keep session and turn state across requests. In stdio mode stdout is reserved for protocol messages; diagnostics must go to stderr or a log file."
)]
pub(crate) struct ServeArgs {
    /// Serve mode. Defaults to `http` for backwards compatibility.
    #[command(subcommand)]
    pub mode: Option<ServeMode>,
    /// Address to listen on for the default HTTP mode
    #[arg(long, default_value = "127.0.0.1")]
    pub host: String,
    /// Port to listen on for the default HTTP mode
    #[arg(short, long, default_value_t = DEFAULT_API_PORT)]
    pub port: u16,
}

#[derive(Subcommand, Debug)]
pub(crate) enum ServeMode {
    /// Start the HTTP API server
    Http(ServeHttpArgs),
    /// Start the stdio JSON-RPC app-server
    #[command(
        after_help = "This mode speaks newline-delimited JSON-RPC on stdin/stdout. The parent process writes requests to stdin, reads responses and notifications from stdout, and may keep the child alive for multiple turns. Do not print human-readable output to stdout in this mode; use stderr or ASTRA_LOG_FILE for diagnostics."
    )]
    Stdio,
}

#[derive(Args, Debug)]
pub(crate) struct ServeHttpArgs {
    /// Address to listen on
    #[arg(long, default_value = "127.0.0.1")]
    pub host: String,
    /// Port to listen on
    #[arg(short, long, default_value_t = DEFAULT_API_PORT)]
    pub port: u16,
}

#[derive(Args, Debug)]
#[command(
    after_help = "Examples:\n  astra chat -m \"总结当前仓库结构\"\n  echo \"修复测试失败\" | astra chat --stdin --json"
)]
pub(crate) struct ChatArgs {
    /// Chat message text
    // A task, shell snippet, diff, or Markdown list commonly begins with a
    // hyphen.  Treat the next token as the explicit `--message` value rather
    // than attempting to parse it as another CLI option.
    #[arg(short = 'm', long = "message", allow_hyphen_values = true)]
    pub message: Option<String>,
    /// Existing session id to continue
    #[arg(long)]
    pub session_id: Option<String>,
    /// Do not auto-resume the most recent session when running one-shot chat.
    ///
    /// This is useful for benchmark/scripted runs where every invocation must
    /// start with an isolated server-created session.
    #[arg(long)]
    pub no_resume: bool,
    /// Model override
    #[arg(long)]
    pub model: Option<String>,
    /// Benchmark execution profile. Currently supports `swebench`.
    #[cfg(feature = "harness")]
    #[arg(long = "benchmark-profile", value_parser = parse_benchmark_profile_arg)]
    pub benchmark_profile: Option<astra_harness::HarnessProfile>,
    /// Enable explain mode (`--explain` => on, `--explain verbose` => verbose)
    #[arg(
        long,
        num_args = 0..=1,
        default_missing_value = "on",
        value_name = "MODE",
        value_parser = parse_explain_mode_arg
    )]
    pub explain: Option<crate::cli::session::session_state::ExplainMode>,
    /// Skip approval prompts; hard safety denies still apply
    #[arg(short = 'y', long = "auto-approve", default_value_t = false)]
    pub auto_approve: bool,
    /// Permission mode: auto, bypass, plan, accept_edits, prompt (interactive, default), or deny.
    #[arg(long = "permission-mode", value_parser = parse_permission_mode_arg)]
    pub permission_mode: Option<String>,
    /// Suppress spinner and progress output (result still printed)
    #[arg(long, default_value_t = false)]
    pub quiet: bool,
    /// Output result as JSON (implies --quiet)
    #[arg(long, default_value_t = false)]
    pub json: bool,
    /// Read message from stdin instead of -m
    #[arg(long, default_value_t = false)]
    pub stdin: bool,
    /// Disable ANSI colors in output
    #[arg(long, default_value_t = false)]
    pub no_color: bool,
    /// Append extra context to the system prompt (used by gateway)
    #[arg(long = "append-system-prompt", hide = true)]
    pub append_system_prompt: Option<String>,
    /// Write structured JSONL events to this dedicated machine-event file.
    /// stderr remains a diagnostic channel and is never part of the JSONL
    /// protocol.
    #[arg(long = "stream-events", hide = true, value_name = "PATH")]
    pub stream_events: Option<PathBuf>,
    /// Bound a one-shot turn's execution time so cancellation and terminal
    /// serialization finish before an external process supervisor intervenes.
    #[arg(
        long = "max-wall-time-seconds",
        hide = true,
        value_parser = clap::value_parser!(u64).range(71..)
    )]
    pub max_wall_time_seconds: Option<u64>,
}

#[derive(Args, Debug)]
#[command(
    after_help = "Examples:\n  astra team list\n  astra team create dev Frontend delivery team\n  astra team add-member dev planner Break work into steps\n  astra team run dev 在/tmp下实现一个登录页面"
)]
pub(crate) struct TeamArgs {
    #[command(subcommand)]
    pub command: Option<TeamSubcommand>,
}

#[derive(Subcommand, Debug)]
pub(crate) enum TeamSubcommand {
    /// List defined teams
    List,
    /// Create a team
    Create(TeamCreateArgs),
    #[command(name = "add-member")]
    /// Add a role/member to a team
    AddMember(TeamAddMemberArgs),
    /// Show team details
    Info(TeamNameArgs),
    /// Delete a team
    Delete(TeamNameArgs),
    /// Set shared team context
    Context(TeamContextArgs),
    /// Execute a task with a team
    Run(TeamRunArgs),
    /// Show execution history for a team
    History(TeamNameArgs),
    /// Save a team snapshot
    Snapshot(TeamSnapshotArgs),
    /// Restore a team snapshot
    Restore(TeamRestoreArgs),
}

#[derive(Args, Debug)]
pub(crate) struct TeamCreateArgs {
    /// Team name
    pub name: String,
    /// Optional description
    #[arg(trailing_var_arg = true)]
    pub description: Vec<String>,
}

#[derive(Args, Debug)]
pub(crate) struct TeamAddMemberArgs {
    /// Team name
    pub team: String,
    /// Member role
    pub role: String,
    /// Optional description for the member
    #[arg(trailing_var_arg = true)]
    pub description: Vec<String>,
}

#[derive(Args, Debug)]
pub(crate) struct TeamNameArgs {
    /// Team name
    pub name: String,
}

#[derive(Args, Debug)]
pub(crate) struct TeamContextArgs {
    /// Team name
    pub team: String,
    /// Context key
    pub key: String,
    /// Context value
    #[arg(required = true, num_args = 1.., trailing_var_arg = true)]
    pub value: Vec<String>,
}

#[derive(Args, Debug)]
pub(crate) struct TeamRunArgs {
    /// Team name
    pub team: String,
    /// Task description
    #[arg(required = true, num_args = 1.., trailing_var_arg = true)]
    pub task: Vec<String>,
}

#[derive(Args, Debug)]
pub(crate) struct TeamSnapshotArgs {
    /// Team name
    pub team: String,
    /// Optional snapshot label
    #[arg(trailing_var_arg = true)]
    pub label: Vec<String>,
}

#[derive(Args, Debug)]
pub(crate) struct TeamRestoreArgs {
    /// Team name
    pub team: String,
    /// Snapshot identifier
    pub snapshot_id: String,
}

#[derive(Subcommand, Debug)]
pub(crate) enum WorkSubcommand {
    /// Create a Work and start its server-owned developer loop
    Start(WorkStartArgs),
    /// Show the current Goal, delivery state, and canonical Task Graph
    Show(WorkShowArgs),
    /// Continue a Work branch with guidance or a new constraint
    Continue(WorkContinueArgs),
}

#[derive(Args, Debug)]
pub(crate) struct WorkStartArgs {
    /// Explicit user-authored Done-when criterion; may be repeated
    #[arg(long = "done-when")]
    pub done_when: Vec<String>,
    /// Desired outcome
    #[arg(required = true, num_args = 1.., trailing_var_arg = true)]
    pub goal: Vec<String>,
}

#[derive(Args, Debug)]
pub(crate) struct WorkShowArgs {
    /// Public Work identity
    pub work_id: String,
    /// Inspect a non-delivery branch
    #[arg(long)]
    pub branch: Option<String>,
    /// Print the exact JSON observation and Task Graph
    #[arg(long, default_value_t = false)]
    pub json: bool,
}

#[derive(Args, Debug)]
pub(crate) struct WorkContinueArgs {
    /// Public Work identity
    pub work_id: String,
    /// Continue a non-delivery branch
    #[arg(long)]
    pub branch: Option<String>,
    /// Guidance, correction, or additional constraint
    #[arg(required = true, num_args = 1.., trailing_var_arg = true)]
    pub message: Vec<String>,
}

#[derive(Args, Debug)]
#[command(
    after_help = "Examples:\n  astra memory list\n  astra memory list --type profile\n  astra memory search 用户偏好\n  astra memory show <memory_id>\n  astra memory forget <memory_id>"
)]
pub(crate) struct MemoryArgs {
    #[command(subcommand)]
    pub command: Option<MemorySubcommand>,
}

#[derive(Subcommand, Debug)]
pub(crate) enum MemorySubcommand {
    /// List recent memories, newest first.
    List(MemoryListArgs),
    /// Semantic search across all memories.
    Search(MemorySearchArgs),
    /// Show full content of a single memory by id.
    Show(MemoryShowArgs),
    /// Soft-delete a memory by id (can be restored by an admin).
    Forget(MemoryForgetArgs),
}

#[derive(Args, Debug)]
pub(crate) struct MemoryListArgs {
    /// Filter to a single memory_type (profile / semantic / procedural /
    /// episodic / working). When omitted, all types are listed.
    #[arg(long = "type")]
    pub memory_type: Option<String>,
    /// Maximum number of entries to print.
    #[arg(long, default_value_t = 20)]
    pub limit: u32,
}

#[derive(Args, Debug)]
pub(crate) struct MemorySearchArgs {
    /// Search query
    #[arg(required = true, num_args = 1.., trailing_var_arg = true)]
    pub query: Vec<String>,
}

#[derive(Args, Debug)]
pub(crate) struct MemoryShowArgs {
    /// Memory id to inspect (UUID from `astra memory list`).
    pub memory_id: String,
}

#[derive(Args, Debug)]
pub(crate) struct MemoryForgetArgs {
    /// Memory id to delete.
    pub memory_id: String,
    /// Optional reason for the audit trail.
    #[arg(long)]
    pub reason: Option<String>,
}

#[derive(Args, Debug)]
#[command(
    after_help = "Examples:\n  astra review\n  astra review working\n  astra review rev HEAD~2",
    override_usage = "astra review [head|working|rev <TARGET>|<TARGET>]"
)]
pub(crate) struct ReviewArgs {
    #[command(subcommand)]
    pub command: Option<ReviewSubcommand>,
    /// Optional review target such as a revision when not using a named subcommand
    #[arg(trailing_var_arg = true)]
    pub target: Vec<String>,
}

#[derive(Subcommand, Debug)]
pub(crate) enum ReviewSubcommand {
    /// Review the current HEAD commit
    Head,
    /// Review working tree changes
    Working,
    /// Review a specific git revision
    Rev(ReviewTargetArgs),
}

#[derive(Args, Debug)]
pub(crate) struct ReviewTargetArgs {
    /// Revision, commit hash, or ref
    #[arg(required = true, num_args = 1.., trailing_var_arg = true)]
    pub target: Vec<String>,
}

#[derive(Args, Debug)]
#[command(
    after_help = "Examples:\n  astra grep selector\n  astra grep files 'rust/**/*.rs'\n  astra grep review permission",
    override_usage = "astra grep <PATTERN> | astra grep content <PATTERN> | astra grep files <GLOB> | astra grep review <PATTERN>"
)]
pub(crate) struct GrepArgs {
    #[command(subcommand)]
    pub command: Option<GrepSubcommand>,
    /// Workspace content search pattern when not using a named subcommand
    #[arg(required = true, num_args = 1.., trailing_var_arg = true)]
    pub pattern: Vec<String>,
}

#[derive(Subcommand, Debug)]
pub(crate) enum GrepSubcommand {
    /// Search workspace content
    Content(GrepPatternArgs),
    /// Match files by glob
    Files(GrepPatternArgs),
    /// Search only changed files relevant to review
    Review(GrepPatternArgs),
}

#[derive(Args, Debug)]
pub(crate) struct GrepPatternArgs {
    /// Search pattern or glob
    #[arg(required = true, num_args = 1.., trailing_var_arg = true)]
    pub pattern: Vec<String>,
}

#[derive(Args, Debug)]
#[command(
    after_help = "Examples:\n  astra permissions rules\n  astra permissions auto\n  astra permissions bypass\n  astra permissions plan\n  astra permissions accept_edits\n  astra permissions prompt\n  astra permissions trust\n  astra permissions trace"
)]
pub(crate) struct PermissionsArgs {
    #[command(subcommand)]
    pub command: Option<PermissionsSubcommand>,
}

#[derive(Subcommand, Debug)]
pub(crate) enum PermissionsSubcommand {
    /// Auto-approve allowed tool calls
    Auto,
    /// Skip approval prompts; catastrophic and policy hard-denies still apply
    Bypass,
    /// Auto-approve workspace-local edits while still prompting for shell and external writes
    #[command(name = "accept_edits")]
    AcceptEdits,
    /// Read-only investigation mode: allow reads, deny mutations
    Plan,
    /// Prompt before running allowed tool calls
    Prompt,
    /// Deny writes and high-risk tools
    Deny,
    /// Show permission rules summary
    Rules,
    /// Trust this workspace and enable saved workspace allow rules
    Trust,
    /// Mark this workspace untrusted and ignore saved workspace allow rules
    Untrust,
    /// Show recent permission audit events
    Trace(PermissionsTraceArgs),
}

#[derive(Args, Debug)]
pub(crate) struct PermissionsTraceArgs {
    /// Write redacted JSONL audit events to this file
    #[arg(long = "export", value_name = "PATH")]
    pub export: Option<PathBuf>,
}

#[derive(Args, Debug)]
#[command(
    after_help = "Examples:\n  astra debug\n  astra debug d5c81bcf-d811-4d66-a559-e4da775d6e92"
)]
pub(crate) struct DebugArgs {
    /// Session id to inspect; defaults to the active session
    pub session_id: Option<String>,
}

#[derive(Args, Debug)]
#[command(
    after_help = "Examples:\n  astra agent list\n  astra agent status team-dev-planner\n  astra agent logs team-dev-planner"
)]
pub(crate) struct AgentArgs {
    #[command(subcommand)]
    pub command: Option<AgentSubcommand>,
}

#[derive(Subcommand, Debug)]
pub(crate) enum AgentSubcommand {
    /// List active agents
    List,
    /// Show agent status
    Status(AgentIdArgs),
    /// Stop an agent
    Stop(AgentIdArgs),
    /// Show recent agent logs
    Logs(AgentIdArgs),
}

#[derive(Args, Debug)]
pub(crate) struct AgentIdArgs {
    /// Agent identifier
    pub agent_id: String,
}

#[derive(Args, Debug)]
#[command(
    after_help = "Examples:\n  astra messaging\n  astra messaging dlq\n  astra messaging status"
)]
pub(crate) struct MessagingArgs {
    #[command(subcommand)]
    pub command: Option<MessagingSubcommand>,
}

#[derive(Subcommand, Debug)]
pub(crate) enum MessagingSubcommand {
    /// Show metrics snapshot
    Metrics,
    /// Show dead letter queue summary
    Dlq,
    /// Show mailbox status
    Status,
}

#[derive(Args, Debug)]
#[command(
    after_help = "Examples:\n  astra diff\n  astra diff staged\n  astra diff stat crates/astra-cli/src/main.rs\n  astra diff show HEAD~1",
    override_usage = "astra diff [<PATH>...] | astra diff staged [<PATH>...] | astra diff unstaged [<PATH>...] | astra diff stat [<PATH>...] | astra diff show <REV> [<PATH>...]"
)]
pub(crate) struct DiffArgs {
    #[command(subcommand)]
    pub command: Option<DiffSubcommand>,
    /// Optional path filters when not using a named subcommand
    #[arg(trailing_var_arg = true)]
    pub paths: Vec<String>,
}

#[derive(Subcommand, Debug)]
pub(crate) enum DiffSubcommand {
    /// Show staged changes
    Staged(DiffPathsArgs),
    /// Show unstaged changes
    Unstaged(DiffPathsArgs),
    /// Show diff stat summary
    Stat(DiffPathsArgs),
    /// Show a specific revision
    Show(DiffShowArgs),
}

#[derive(Args, Debug)]
pub(crate) struct DiffPathsArgs {
    /// Optional path filters
    #[arg(trailing_var_arg = true)]
    pub paths: Vec<String>,
}

#[derive(Args, Debug)]
pub(crate) struct DiffShowArgs {
    /// Revision, commit hash, or ref
    pub rev: String,
    /// Optional path filters
    #[arg(trailing_var_arg = true)]
    pub paths: Vec<String>,
}

#[derive(Args, Debug)]
#[command(after_help = "Examples:\n  astra bug\n  astra bug copy\n  astra bug save")]
pub(crate) struct BugArgs {
    #[command(subcommand)]
    pub command: Option<BugSubcommand>,
}

#[derive(Subcommand, Debug)]
pub(crate) enum BugSubcommand {
    /// Print the report to the terminal
    Print,
    /// Copy the report to the clipboard
    Copy,
    /// Save the report to a file
    Save,
}

#[derive(Subcommand, Debug)]
#[command(
    after_help = "Examples:\n  astra session list\n  astra session show 550e8400-e29b-41d4-a716-446655440000\n  astra session cancel 550e8400-e29b-41d4-a716-446655440000\n  astra session capture latest\n  astra session capture download --output llm_capture.json"
)]
pub(crate) enum SessionCmd {
    /// List sessions
    List(SessionListArgs),
    /// Show session details
    Show(SessionShowArgs),
    /// Close an active session
    Close(SessionShowArgs),
    /// Cancel active runs in a session and mark it cancelled
    Cancel(SessionShowArgs),
    /// Delete a session record
    Delete(SessionShowArgs),
    /// Inspect or download session-scoped LLM captures
    #[command(subcommand)]
    Capture(SessionCaptureCmd),
}

#[derive(Subcommand, Debug)]
pub(crate) enum SessionCaptureCmd {
    /// Show the latest capture for a session
    Latest(SessionCaptureLatestArgs),
    /// Download the latest capture for a session to a JSON file
    Download(SessionCaptureDownloadArgs),
}

#[derive(Args, Debug)]
pub(crate) struct SessionCaptureLatestArgs {
    /// Session id or unique prefix (defaults to the most recent resumable session)
    pub session_id: Option<String>,
    /// Artifact kind to read (defaults to llm_capture)
    #[arg(long, default_value = "llm_capture")]
    pub artifact_kind: String,
}

#[derive(Args, Debug)]
pub(crate) struct SessionCaptureDownloadArgs {
    /// Session id or unique prefix (defaults to the most recent resumable session)
    pub session_id: Option<String>,
    /// Artifact kind to download (defaults to llm_capture)
    #[arg(long, default_value = "llm_capture")]
    pub artifact_kind: String,
    /// Output file path (defaults to the server-suggested filename in the current directory)
    #[arg(long)]
    pub output: Option<std::path::PathBuf>,
}

#[derive(Subcommand, Debug)]
#[command(
    after_help = "Examples:\n  astra self snapshot\n  astra self reflect\n  astra self profile 550e8400-e29b-41d4-a716-446655440000\n  astra self mutate preview --path verification.strictness --value 0.8\n  astra self mutate apply --session-id 550e8400-e29b-41d4-a716-446655440000 --path token_budget.max_turn_input_tokens --value 90000"
)]
pub(crate) enum SelfCmd {
    /// Full persistent self snapshot for a session
    Snapshot(SelfSessionArgs),
    /// Liquid reflection summary reconstructed from persistent local state
    Reflect(SelfReflectArgs),
    /// Core self-model profile (capabilities, state, goals, constraints)
    Profile(SelfSessionArgs),
    /// Goal and plan state for a session
    Goal(SelfSessionArgs),
    /// Latest context-assembly trace and selector context
    Trace(SelfSessionArgs),
    /// Budget and pressure view for the effective runtime config
    Budget(SelfSessionArgs),
    /// Recent adaptive and feedback-like signals
    Signals(SelfSessionArgs),
    /// Session-scoped tool health and blocked tool state
    Health(SelfSessionArgs),
    /// Recent journal events
    Journal(SelfJournalArgs),
    /// Validate persisted self state and tuned config invariants
    Verify(SelfSessionArgs),
    /// Preview or apply persistent self mutations
    #[command(subcommand)]
    Mutate(SelfMutateCmd),
}

#[derive(Args, Debug)]
pub(crate) struct SelfSessionArgs {
    /// Session id or unique prefix (defaults to the most recent resumable session)
    pub session_id: Option<String>,
}

#[derive(Args, Debug)]
pub(crate) struct SelfReflectArgs {
    /// Session id or unique prefix (defaults to the most recent resumable session)
    pub session_id: Option<String>,
    /// Observation topic to prioritize in the local reflection surface
    #[arg(long, default_value = "overview", value_parser = ["overview", "runtime", "execution", "knowledge", "adaptation"])]
    pub topic: String,
    /// Optional observation facet under the selected topic
    #[arg(long)]
    pub facet: Option<String>,
    /// Optional concrete question to keep in the reflection prompt preview
    #[arg(long)]
    pub question: Option<String>,
    /// Maximum number of recent events to use for the reflection window
    #[arg(long, default_value_t = 20)]
    pub last_n: usize,
}

#[derive(Args, Debug)]
pub(crate) struct SelfJournalArgs {
    /// Session id or unique prefix (defaults to the most recent resumable session)
    pub session_id: Option<String>,
    /// Maximum number of recent events to return
    #[arg(long, default_value_t = 20)]
    pub limit: usize,
}

#[derive(Subcommand, Debug)]
pub(crate) enum SelfMutateCmd {
    /// Preview a RuntimeConfig mutation without persisting it
    Preview(SelfMutateConfigArgs),
    /// Apply a RuntimeConfig mutation to persisted session state
    Apply(SelfMutateConfigArgs),
}

#[derive(Args, Debug)]
pub(crate) struct SelfMutateConfigArgs {
    /// Session id or unique prefix (defaults to the most recent resumable session)
    #[arg(long)]
    pub session_id: Option<String>,
    /// Dotted RuntimeConfig path (for example: verification.strictness)
    #[arg(long)]
    pub path: String,
    /// New value as JSON (falls back to a raw string when not valid JSON)
    #[arg(long)]
    pub value: String,
}

#[derive(Args, Debug)]
pub(crate) struct SessionListArgs {
    #[arg(long)]
    pub agent_id: Option<String>,
    #[arg(long)]
    pub status: Option<String>,
    #[arg(long, default_value_t = 20)]
    pub limit: u32,
    #[arg(long, default_value_t = 0)]
    pub offset: u32,
}

#[derive(Args, Debug)]
pub(crate) struct SessionShowArgs {
    pub session_id: String,
}

#[derive(Subcommand, Debug)]
#[command(after_help = "Examples:\n  astra model list\n  astra model show gpt-4o")]
pub(crate) enum ModelCmd {
    /// List available models
    List,
    /// Show model details
    Show(ModelShowArgs),
}

#[derive(Args, Debug)]
pub(crate) struct ModelShowArgs {
    pub model_name: String,
}

#[derive(Subcommand, Debug)]
#[command(
    after_help = "Examples:\n  astra skill list\n  astra skill show memory-search\n  astra skill status"
)]
pub(crate) enum SkillCmd {
    /// List visible skills
    List(SkillListArgs),
    /// Show skill details
    Show(SkillShowArgs),
    /// Show skill group status
    Status(SkillStatusArgs),
}

#[derive(Args, Debug)]
pub(crate) struct SkillListArgs {
    #[arg()]
    pub query: Vec<String>,
    #[arg(long, default_value_t = 50)]
    pub limit: u32,
    #[arg(long, default_value_t = 0)]
    pub offset: u32,
    #[arg(long)]
    pub source: Option<String>,
    #[arg(long)]
    pub category: Option<String>,
}

#[derive(Args, Debug)]
pub(crate) struct SkillShowArgs {
    pub skill_id: String,
    #[arg(long)]
    pub version: Option<String>,
}

#[derive(Args, Debug)]
pub(crate) struct SkillStatusArgs {
    #[arg(long, default_value_t = 50)]
    pub per_group: u32,
}

#[derive(Args, Debug)]
#[command(
    after_help = "Examples:\n  astra replay 550e8400-e29b-41d4-a716-446655440000\n  astra replay 550e8400-e29b-41d4-a716-446655440000 --compare"
)]
pub(crate) struct ReplayArgs {
    /// Session id to replay
    pub session_id: String,
    /// Optional sandbox profile
    #[arg(long)]
    pub sandbox_name: Option<String>,
    /// Use mock mode during replay
    #[arg(long, default_value_t = true)]
    pub mock_mode: bool,
    /// Compare replay output against the recorded run
    #[arg(long)]
    pub compare: bool,
}

#[derive(Subcommand, Debug)]
pub(crate) enum AuditCmd {
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
pub(crate) struct AuditListArgs {
    #[arg(long)]
    pub status: Option<String>,
    #[arg(long)]
    pub model: Option<String>,
    #[arg(long)]
    pub since: Option<String>,
    #[arg(long)]
    pub until: Option<String>,
    #[arg(long)]
    pub min_turns: Option<u32>,
    #[arg(long, default_value = "created")]
    pub sort: String,
    #[arg(long, default_value_t = 20)]
    pub limit: u32,
    #[arg(long, default_value_t = 1)]
    pub page: u32,
}

#[derive(Args, Debug)]
pub(crate) struct AuditShowArgs {
    pub session_id: String,
}

#[derive(Args, Debug)]
pub(crate) struct AuditTurnsArgs {
    pub session_id: String,
    /// Show detail for a specific turn number
    #[arg(long)]
    pub turn: Option<u32>,
    #[arg(long, default_value_t = 1)]
    pub page: u32,
    #[arg(long, default_value_t = 20)]
    pub per_page: u32,
}

#[derive(Args, Debug)]
pub(crate) struct AuditToolsArgs {
    /// Session ID; omit for cross-session tool analytics
    pub session_id: Option<String>,
    #[arg(long)]
    pub since: Option<String>,
    #[arg(long)]
    pub until: Option<String>,
}

#[derive(Subcommand, Debug)]
pub(crate) enum JournalCmd {
    /// Print a deterministic digest of a local session journal (JSON or text)
    Digest(JournalDigestArgs),
    /// Render the delegation / sub-run tree for a session (ASCII or JSON)
    Tree(JournalTreeArgs),
    /// Compare two session journals on tool sequence, token totals, event counts
    Diff(JournalDiffArgs),
}

#[derive(Args, Debug)]
pub(crate) struct JournalTreeArgs {
    /// Session id, unique prefix, `last`, or omit for most recent local journal
    #[arg(value_name = "SESSION")]
    pub session_id: Option<String>,
    /// Same meaning as positional SESSION (positional wins if both are set)
    #[arg(long = "session", value_name = "SESSION")]
    pub session: Option<String>,
    /// Output format: text (default) or json
    #[arg(long, default_value = "text")]
    pub format: String,
}

#[derive(Args, Debug)]
pub(crate) struct JournalDiffArgs {
    /// First session id (or unique prefix / `last`).
    #[arg(value_name = "A")]
    pub a: String,
    /// Second session id (or unique prefix / `last`).
    #[arg(value_name = "B")]
    pub b: String,
    /// Output format: text (default) or json
    #[arg(long, default_value = "text")]
    pub format: String,
}

#[derive(Args, Debug)]
pub(crate) struct JournalDigestArgs {
    /// Session id, unique prefix, `last`, or omit for most recent local journal
    #[arg(value_name = "SESSION")]
    pub session_id: Option<String>,
    /// Same meaning as positional SESSION (positional wins if both are set)
    #[arg(long = "session", value_name = "SESSION")]
    pub session: Option<String>,
    /// Output format: json or text
    #[arg(long, default_value = "json")]
    pub format: String,
    /// all (default) or summary (smaller turn rows)
    #[arg(long)]
    pub focus: Option<String>,
}

#[derive(Subcommand, Debug)]
pub(crate) enum McpCmd {
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
    /// Test connection to an MCP server and list its tools
    Test(McpTestArgs),
    /// Ping an MCP server to check connectivity
    Ping(McpPingArgs),
}

#[derive(Args, Debug)]
pub(crate) struct McpListArgs {
    /// Config scope: project or user
    #[arg(short = 's', long, default_value = "project")]
    pub scope: String,
}

#[derive(Args, Debug)]
pub(crate) struct McpAddArgs {
    /// Server name
    pub name: String,
    /// Command to run
    pub command: String,
    /// Command arguments
    #[arg(trailing_var_arg = true)]
    pub args: Vec<String>,
    /// Config scope: project or user
    #[arg(short = 's', long, default_value = "project")]
    pub scope: String,
}

#[derive(Args, Debug)]
pub(crate) struct McpAddJsonArgs {
    /// Server name
    pub name: String,
    /// JSON configuration string
    pub json: String,
    /// Config scope: project or user
    #[arg(short = 's', long, default_value = "project")]
    pub scope: String,
}

#[derive(Args, Debug)]
pub(crate) struct McpRemoveArgs {
    /// Server name to remove
    pub name: String,
    /// Config scope: project or user
    #[arg(short = 's', long, default_value = "project")]
    pub scope: String,
}

#[derive(Args, Debug)]
pub(crate) struct McpGetArgs {
    /// Server name to inspect
    pub name: String,
}

#[derive(Args, Debug)]
pub(crate) struct McpTestArgs {
    /// Server name to test
    pub name: String,
    /// Config scope: project or user
    #[arg(short = 's', long, default_value = "project")]
    pub scope: String,
}

#[derive(Args, Debug)]
pub(crate) struct McpPingArgs {
    /// Server name to ping
    pub name: String,
    /// Config scope: project or user
    #[arg(short = 's', long, default_value = "project")]
    pub scope: String,
}

#[derive(Args, Debug)]
pub(crate) struct CompletionArgs {
    /// Shell to generate completions for
    #[arg(value_enum)]
    pub shell: clap_complete::Shell,
}

#[derive(Subcommand, Debug)]
pub(crate) enum ConfigCmd {
    /// List all settings and their values
    List,
    /// Get a specific setting value
    Get(ConfigGetArgs),
    /// Set a setting value
    Set(ConfigSetArgs),
    /// Show the resolved workflow-guard policy for a model
    ShowPolicy(ConfigShowPolicyArgs),
    /// Inspect the content-addressed history of saved configs.
    #[command(subcommand)]
    Version(ConfigVersionCmd),
}

/// `astra config version ...` — browse, diff, and inspect saved config
/// versions. Every `/config` save and every session startup writes an
/// entry to the version store (`~/.astra/config/versions/`); these
/// commands are the read-side of that store.
#[derive(Subcommand, Debug)]
pub(crate) enum ConfigVersionCmd {
    /// List versions newest-first.
    List(ConfigVersionListArgs),
    /// Print the TOML body of a specific version.
    Show(ConfigVersionShowArgs),
    /// Show field-level diff between two versions.
    Diff(ConfigVersionDiffArgs),
    /// Print the id of the config the current process would run under.
    Current,
    /// Deprecated: config version pull is server-owned.
    Pull(ConfigVersionPullArgs),
}

#[derive(Args, Debug)]
pub(crate) struct ConfigVersionPullArgs {
    /// Maximum number of versions to pull (default: 500).
    #[arg(long, default_value_t = 500)]
    pub limit: i64,
}

#[derive(Args, Debug)]
pub(crate) struct ConfigVersionListArgs {
    /// Maximum number of rows to render (default: all).
    #[arg(long)]
    pub limit: Option<usize>,
}

#[derive(Args, Debug)]
pub(crate) struct ConfigVersionShowArgs {
    /// Version id (full or a unique prefix, e.g. `cfg_a7b2`).
    pub id: String,
}

#[derive(Args, Debug)]
pub(crate) struct ConfigVersionDiffArgs {
    /// First version id (or unique prefix).
    pub a: String,
    /// Second version id (or unique prefix).
    pub b: String,
}

#[derive(Args, Debug)]
pub(crate) struct ConfigGetArgs {
    /// Setting key (e.g. default_model, verbose, api_url)
    pub key: String,
}

#[derive(Args, Debug)]
pub(crate) struct ConfigSetArgs {
    /// Setting key
    pub key: String,
    /// Setting value
    pub value: String,
}

#[derive(Args, Debug)]
pub(crate) struct ConfigShowPolicyArgs {
    /// Model id to resolve the policy for (substring-matched against
    /// built-in and user profiles). Omit to show global defaults.
    #[arg(long)]
    pub model: Option<String>,
    /// Emit the resolved policy as JSON instead of human-readable text.
    #[arg(long)]
    pub json: bool,
}

#[cfg(test)]
mod tests {
    use super::{Cli, Command, SessionCmd, WorkSubcommand};
    use clap::Parser;

    #[test]
    fn session_cancel_is_a_distinct_lifecycle_command() {
        let cli = Cli::try_parse_from([
            "astra",
            "session",
            "cancel",
            "550e8400-e29b-41d4-a716-446655440000",
        ])
        .expect("session cancel command");
        let Some(Command::Session(SessionCmd::Cancel(args))) = cli.command else {
            panic!("expected SessionCmd::Cancel")
        };
        assert_eq!(args.session_id, "550e8400-e29b-41d4-a716-446655440000");
    }

    #[test]
    fn work_commands_are_typed_and_do_not_reinterpret_goal_or_guidance() {
        let start = Cli::try_parse_from([
            "astra",
            "work",
            "start",
            "--done-when",
            "targeted tests pass",
            "implement",
            "the",
            "feature",
        ])
        .expect("Start Work");
        let Some(Command::Work(WorkSubcommand::Start(start))) = start.command else {
            panic!("expected typed Start Work")
        };
        assert_eq!(start.done_when, ["targeted tests pass"]);
        assert_eq!(start.goal, ["implement", "the", "feature"]);

        let continuation = Cli::try_parse_from([
            "astra", "work", "continue", "work-1", "--branch", "branch-2", "cancel", "the",
            "obsolete", "path",
        ])
        .expect("Continue Work");
        let Some(Command::Work(WorkSubcommand::Continue(continuation))) = continuation.command
        else {
            panic!("expected typed Continue Work")
        };
        assert_eq!(continuation.branch.as_deref(), Some("branch-2"));
        assert_eq!(continuation.message, ["cancel", "the", "obsolete", "path"]);
    }

    #[test]
    fn unknown_command_help_cannot_become_a_model_message() {
        for help in ["--help", "-h"] {
            let cli = Cli::try_parse_from(["astra", "history", help]).unwrap();
            let error = cli
                .validate_external_message_shorthand()
                .expect_err("command-looking help must fail before session dispatch");
            assert!(error.contains("unknown command `history`"));
        }

        let message = Cli::try_parse_from(["astra", "explain", "history", "please"]).unwrap();
        assert!(message.validate_external_message_shorthand().is_ok());
    }
}
