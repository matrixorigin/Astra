//! CLI argument structs and subcommand enums for clap.
//!
//! All main entry point arguments, subcommands, and their nested argument
//! types are defined here. These are used by `main.rs` for parsing and
//! by `command_router.rs` for dispatch.
//!
//! ## Observability (flags and environment)
//!
//! Applied in [`crate::diagnostic_log::init_cli_observability`] immediately after [`Cli`] is parsed.
//!
//! **Priority:** `--log-file` → `ASTRA_LOG_FILE` → (`--diagnostic-log` or `ASTRA_DIAGNOSTIC_LOG=1`) for stderr.
//!
//! - **`--log-file <PATH>`** (hidden): append JSON lines to a file; overrides `ASTRA_LOG_FILE` when both are set.
//! - **`--diagnostic-log`** (hidden): structured [`tracing`] on stderr (`astra-logging`); same effect as `ASTRA_DIAGNOSTIC_LOG=1`.
//! - **`ASTRA_LOG_FILE`**: same as `--log-file` when the flag is absent.
//! - **`ASTRA_DIAGNOSTIC_LOG=1`**: stderr diagnostics when no file target is selected.
//!
//! See repository `README.md` for `RUST_LOG` / `ASTRA_LOG_FORMAT`.

use clap::{Args, Parser, Subcommand};

#[derive(Parser, Debug)]
#[command(name = "astra")]
#[command(about = "AI agent CLI — run `astra` for interactive chat")]
pub(crate) struct Cli {
    /// API server base URL [env: ASTRA_API_URL] [config: api_url] [default: http://127.0.0.1:8000]
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
    /// Auto-approve tool calls without prompting
    #[arg(short = 'y', long = "yes")]
    pub yes: bool,
    /// System prompt to prepend (useful with --print for scripting)
    #[arg(long = "system-prompt")]
    pub system_prompt: Option<String>,
    /// Maximum agentic turns (useful with --print to limit cost)
    #[arg(long = "max-turns")]
    pub max_turns: Option<usize>,
    /// Maximum session cost in USD before auto-exit (0 = unlimited)
    #[arg(long = "max-budget", default_value_t = 0.0)]
    pub max_budget: f64,
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
    /// Enable experimental full-screen TUI (default: classic line-mode REPL)
    #[arg(long = "tui")]
    pub tui: bool,
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
    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Subcommand, Debug)]
#[command(allow_external_subcommands = true)]
pub(crate) enum Command {
    /// Start the interactive REPL (default when no args given)
    Interactive,
    /// Start the HTTP API server
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
    /// Run a non-REPL chat request
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
    /// Generate shell completion script
    Completion(CompletionArgs),
    /// Diagnose installation, config, and connectivity
    Doctor,
    /// Structured plan without the REPL (scripting / CI)
    #[command(subcommand)]
    Plan(PlanCmd),
    /// Team orchestration and shared context management
    #[command(alias = "teams")]
    Team(TeamArgs),
    /// Local/cloud task management
    Task(TaskArgs),
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
    /// Direct message: astra "your question here"
    #[command(external_subcommand)]
    Message(Vec<String>),
}

/// Headless plan commands (no interactive `plan>` prompt).
#[derive(Subcommand, Debug)]
pub(crate) enum PlanCmd {
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
pub(crate) struct ServeArgs {
    /// Address to listen on
    #[arg(long, default_value = "127.0.0.1")]
    pub host: String,
    /// Port to listen on
    #[arg(short, long, default_value_t = 8000)]
    pub port: u16,
}

#[derive(Args, Debug)]
#[command(
    after_help = "Examples:\n  astra chat -m \"总结当前仓库结构\"\n  echo \"修复测试失败\" | astra chat --stdin --json"
)]
pub(crate) struct ChatArgs {
    /// Chat message text
    #[arg(short = 'm', long = "message")]
    pub message: Option<String>,
    /// Existing session id to continue
    #[arg(long)]
    pub session_id: Option<String>,
    /// Model override
    #[arg(long)]
    pub model: Option<String>,
    /// Enable explain mode
    #[arg(long, default_value_t = false)]
    pub explain: bool,
    /// Auto-approve tool calls
    #[arg(short = 'y', long = "auto-approve", default_value_t = false)]
    pub auto_approve: bool,
    /// Permission mode: auto (approve all), prompt (interactive, default), deny (reject all writes)
    #[arg(long = "permission-mode", value_parser = ["auto", "prompt", "deny"])]
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
    /// Emit structured JSONL events to stderr for gateway integration.
    /// Each line is a JSON object with a "type" field (token, thinking,
    /// tool_started, tool_completed, status).
    #[arg(long = "stream-events", hide = true, default_value_t = false)]
    pub stream_events: bool,
}

#[derive(Args, Debug, Clone)]
pub(crate) struct ForwardArgs {
    /// Remaining arguments forwarded to the underlying command handler
    #[arg(
        value_name = "ARGS",
        trailing_var_arg = true,
        allow_hyphen_values = true
    )]
    pub args: Vec<String>,
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

#[derive(Args, Debug)]
#[command(
    after_help = "Examples:\n  astra task list\n  astra task add 修复登录重定向\n  astra task run 在当前目录补一个最小登录页\n  astra task result abc12345"
)]
pub(crate) struct TaskArgs {
    #[command(subcommand)]
    pub command: Option<TaskSubcommand>,
}

#[derive(Subcommand, Debug)]
pub(crate) enum TaskSubcommand {
    /// List tasks
    List,
    /// Create a task
    Add(TaskTextArgs),
    /// Mark a task as done
    Done(TaskQueryArgs),
    /// Show task status and details
    Status(TaskQueryArgs),
    /// Run a background task with the agent
    Run(TaskTextArgs),
    /// Show the result of a task run
    Result(TaskQueryArgs),
}

#[derive(Args, Debug)]
pub(crate) struct TaskTextArgs {
    /// Task text or prompt
    #[arg(required = true, num_args = 1.., trailing_var_arg = true)]
    pub text: Vec<String>,
}

#[derive(Args, Debug)]
pub(crate) struct TaskQueryArgs {
    /// Task id or title query
    #[arg(required = true, num_args = 1.., trailing_var_arg = true)]
    pub query: Vec<String>,
}

#[derive(Args, Debug)]
#[command(
    after_help = "Examples:\n  astra memory list\n  astra memory search team bootstrap\n  astra memory search 用户偏好"
)]
pub(crate) struct MemoryArgs {
    #[command(subcommand)]
    pub command: Option<MemorySubcommand>,
}

#[derive(Subcommand, Debug)]
pub(crate) enum MemorySubcommand {
    /// List recent memories
    List,
    /// Search memories
    Search(MemorySearchArgs),
}

#[derive(Args, Debug)]
pub(crate) struct MemorySearchArgs {
    /// Search query
    #[arg(required = true, num_args = 1.., trailing_var_arg = true)]
    pub query: Vec<String>,
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
    after_help = "Examples:\n  astra permissions status\n  astra permissions auto\n  astra permissions prompt\n  astra permissions rules"
)]
pub(crate) struct PermissionsArgs {
    #[command(subcommand)]
    pub command: Option<PermissionsSubcommand>,
}

#[derive(Subcommand, Debug)]
pub(crate) enum PermissionsSubcommand {
    /// Show current permission state
    Status,
    /// Auto-approve allowed tool calls
    Auto,
    /// Prompt before running allowed tool calls
    Prompt,
    /// Deny writes and high-risk tools
    Deny,
    /// Auto-approve all tool calls
    All,
    /// Show permission rules summary
    Rules,
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
    after_help = "Examples:\n  astra diff\n  astra diff staged\n  astra diff stat rust/crates/astra-cli/src/main.rs\n  astra diff show HEAD~1",
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
    after_help = "Examples:\n  astra session list\n  astra session show 550e8400-e29b-41d4-a716-446655440000\n  astra session trace on\n  astra session trace status 550e8400-e29b-41d4-a716-446655440000\n  astra session capture latest\n  astra session capture download --output llm_capture.json"
)]
pub(crate) enum SessionCmd {
    /// List sessions
    List(SessionListArgs),
    /// Show session details
    Show(SessionShowArgs),
    /// Close an active session
    Close(SessionShowArgs),
    /// Delete a session record
    Delete(SessionShowArgs),
    /// Inspect or download session-scoped LLM captures
    #[command(subcommand)]
    Capture(SessionCaptureCmd),
    /// Toggle or inspect per-session full LLM request/response capture
    #[command(subcommand)]
    Trace(SessionTraceCmd),
}

#[derive(Subcommand, Debug)]
pub(crate) enum SessionCaptureCmd {
    /// Show the latest capture for a session
    Latest(SessionCaptureLatestArgs),
    /// Download the latest capture for a session to a JSON file
    Download(SessionCaptureDownloadArgs),
}

#[derive(Subcommand, Debug)]
pub(crate) enum SessionTraceCmd {
    /// Enable full LLM request/response capture for a session
    On(SessionTraceArgs),
    /// Disable full LLM request/response capture for a session
    Off(SessionTraceArgs),
    /// Show whether full LLM request/response capture is enabled for a session
    Status(SessionTraceArgs),
}

#[derive(Args, Debug)]
pub(crate) struct SessionTraceArgs {
    /// Session id or unique prefix (defaults to the most recent resumable session)
    pub session_id: Option<String>,
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
    after_help = "Examples:\n  astra self snapshot\n  astra self reflect\n  astra self profile 550e8400-e29b-41d4-a716-446655440000\n  astra self mutate preview --path verification.strictness --value 0.8\n  astra self mutate apply --session-id 550e8400-e29b-41d4-a716-446655440000 --path tool_selection.tool_budget_tokens --value 900"
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
    /// Reflection focus to prioritize in the local surface
    #[arg(long, default_value = "auto", value_parser = ["auto", "skill_failure", "unexpected_result", "data_quality", "tool_selection", "history", "performance"])]
    pub focus: String,
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
    /// Set the persisted session goal text
    Goal(SelfMutateGoalArgs),
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
pub(crate) struct SelfMutateGoalArgs {
    /// Session id or unique prefix (defaults to the most recent resumable session)
    #[arg(long)]
    pub session_id: Option<String>,
    /// New session goal text
    #[arg(long)]
    pub text: String,
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
    /// List registered skills
    List(SkillListArgs),
    /// Show skill details
    Show(SkillShowArgs),
    /// Register a skill
    Register(SkillRegisterArgs),
    /// Show skill group status
    Status(SkillStatusArgs),
}

#[derive(Args, Debug)]
pub(crate) struct SkillListArgs {
    #[arg(long, default_value_t = 50)]
    pub limit: u32,
    #[arg(long, default_value_t = 0)]
    pub offset: u32,
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
pub(crate) struct SkillRegisterArgs {
    #[arg(long)]
    pub name: String,
    #[arg(long)]
    pub version: String,
    #[arg(long)]
    pub code: Option<String>,
    #[arg(long)]
    pub code_file: Option<String>,
    #[arg(long)]
    pub skill_id: Option<String>,
    #[arg(long)]
    pub description: Option<String>,
    #[arg(long)]
    pub metadata_json: Option<String>,
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
