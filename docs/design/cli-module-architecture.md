# CLI Module Architecture

The `astra-cli` crate provides the interactive REPL and command-line interface for Mo Dev Agent. This document describes the architecture of the `cli/` module (~52,500 lines across 54 files).

## Module Overview

```
cli/
├── Core Infrastructure
│   ├── command_registry.rs    — Single source of truth for 62 slash commands
│   ├── command_router.rs      — CLI argument parsing and one-shot command dispatch
│   ├── repl_runtime.rs        — Tool selector, skill registry, MCP manager setup
│   ├── repl_turn.rs           — Single conversation turn execution
│   └── repl_ui.rs             — Interactive UI (help palette, fuzzy matching)
│
├── Streaming & Rendering
│   ├── stream_render.rs       — SSE stream consumption with terminal effects
│   ├── streaming_md.rs        — Markdown rendering during streaming
│   ├── terminal_region.rs     — Terminal region management
│   ├── diff_presenter.rs      — File diff visualization
│   ├── cli_formatting.rs      — Output formatting utilities
│   ├── cli_output.rs          — Structured output macros (cli_ok!, cli_err!, etc.)
│   └── theme.rs               — Color theme definitions
│
├── Slash Command Handlers (22 files)
│   ├── slash_account.rs       — /account, /auth, /login, /logout
│   ├── slash_agent.rs         — /agent subcommands
│   ├── slash_config.rs        — /config management
│   ├── slash_debug.rs         — /debug introspection tools
│   ├── slash_health.rs        — /doctor, /health checks
│   ├── slash_info.rs          — /info, /models, /repo
│   ├── slash_mcp.rs           — /mcp server management
│   ├── slash_memory.rs        — /memory operations
│   ├── slash_session.rs       — /session, /checkpoint, /save, /history
│   ├── slash_skill.rs         — /skill management (4,181 lines)
│   ├── slash_state.rs         — /state inspection
│   ├── slash_team.rs          — /team multi-agent orchestration
│   └── ...                    — Other slash handlers
│
├── Session & Planning
│   ├── plan_interaction.rs    — LLM plan parsing and subtask management
│   ├── plan_executor.rs       — Plan execution engine
│   ├── journal_digest.rs      — Session journal summarization
│   └── durable_bridge.rs      — Cloud session persistence
│
├── Subsystem Integration
│   ├── edge_lifecycle.rs      — Edge tool heartbeat and lifecycle
│   ├── delegate_subrun.rs     — Agent delegation for sub-tasks
│   ├── skill_subrun.rs        — Skill execution in sub-runs
│   ├── spawn_subrun.rs        — Sub-run process spawning
│   ├── permission_manager.rs  — Tool permission prompts (2,218 lines)
│   └── auth_flow.rs           — OAuth/auth flows
│
├── Effects & Animations
│   └── effects/               — Terminal effects (spinners, progress)
│
└── Utilities
    ├── cli_utils.rs           — Common utilities
    ├── sse_utils.rs           — SSE parsing helpers
    ├── mock_llm.rs            — Mock LLM for testing
    └── readline_actor.rs      — Async readline integration
```

## Data Flow

### One-Shot Command (Non-Interactive)

```
main.rs
  └─► command_router.rs::route_cli_command()
        ├─► Parse clap args
        ├─► Apply system prompt
        ├─► repl_runtime::create_tool_selector()
        └─► stream_render::consume_chat_turn_sse()
              └─► Terminal output
```

### Interactive REPL Session

```
main.rs
  └─► repl_loop()
        ├─► repl_ui::show_help_palette() [on /]
        ├─► command_registry::resolve_command()
        └─► Dispatch to slash handler
              │
              ├─ Local handlers (slash_*.rs)
              │    └─► Direct terminal output
              │
              └─ LLM chat turns
                   └─► repl_turn::run_chat_turn()
                         ├─► Build prompt
                         ├─► API call with SSE
                         └─► stream_render::consume_chat_turn_sse()
                               ├─► Tool execution (edge_tools)
                               ├─► Permission prompts
                               └─► Terminal rendering
```

## Key Components

### 1. Command Registry (`command_registry.rs`)

Single source of truth for all 62 slash commands:

```rust
pub struct CommandMeta {
    pub name: &'static str,           // "/help"
    pub description: &'static str,    // "Show help palette"
    pub group: CommandGroup,          // CommandGroup::Core
    pub is_alias: bool,               // false
    pub subcommands: &'static [(&'static str, &'static str)],
    pub arg_hint: Option<&'static str>, // "<query>"
}

pub enum CommandGroup {
    Core,           // ⚡ Essential commands
    Workspace,      // 📂 File/directory operations
    Observability,  // 🔭 Debugging/monitoring
    SessionPlan,    // 📋 Session and plan management
    MemoryTasks,    // 🧠 Memory and task tracking
    Skills,         // 📦 Skill management
    Mcp,            // 🔌 MCP server management
    TeamAccount,    // 👥 Team and account
    System,         // 🔧 System configuration
}
```

Query functions:
- `resolve_command(input)` → Resolve aliases and partial matches
- `suggest_commands(prefix, limit)` → Fuzzy completion candidates
- `completion_candidates(partial)` → Tab completion
- `subcommand_completions(cmd, partial)` → Subcommand completion
- `commands_by_group()` → Grouped for help palette

### 2. REPL Runtime (`repl_runtime.rs`)

Sets up the execution environment:

```rust
pub struct PipelineModules {
    pub entity_graph: Arc<Mutex<EntityGraph>>,           // Entity relationships
    pub pattern_library: Arc<Mutex<PatternLibrary>>,     // Learned patterns
    pub calibrator: Arc<Mutex<ProgressiveCalibrator>>,   // Tool selection tuning
    pub unified_skill_registry: Arc<UnifiedSkillRegistry>, // All skills
    pub mcp_manager: Arc<RwLock<McpClientManager>>,      // MCP servers
    pub _skill_watcher: Option<SkillWatcherHandle>,      // Hot-reload watcher
}

fn create_tool_selector(api, profile) -> (Box<dyn ToolSelector>, PipelineModules) {
    // 1. Load all tool schemas
    // 2. Create PluginRegistry and load skill manifests
    // 3. Register plugins with ToolRegistry
    // 4. Create TfIdfSelector with quality tracking
    // 5. Wire up pipeline learning modules
}
```

### 3. Stream Rendering (`stream_render.rs`)

Handles SSE (Server-Sent Events) consumption with terminal effects:

```rust
struct CliSseStreamHost<'a> {
    api: &'a ThinClient,
    token: &'a str,
    executor: &'a mut ToolExecutor,
    perm_manager: Option<&'a mut PermissionManager>,
    // ...
}

impl SseStreamHost for CliSseStreamHost<'_> {
    async fn on_tool_request(&mut self, req: ToolRequest) -> EdgeToolExecResult;
    async fn on_approval_required(&mut self, req: ApprovalRequest) -> EdgeApprovalResult;
    fn on_stream_text(&mut self, text: &str);
    fn on_reasoning_delta(&mut self, delta: &str);
    // ...
}
```

Effects during streaming:
- `TtftWaitLineSpinner` — "Waiting for first token" spinner
- `ThinkingPreviewPane` — Shows reasoning tokens in a viewport
- `ToolRunningLineSpinner` — Per-tool execution progress
- `Spinner` — Generic animated spinner

### 4. Permission Manager (`permission_manager.rs`)

Handles tool approval prompts:

```rust
pub enum PermissionMode {
    AlwaysAsk,      // Prompt for every tool call
    TrustPaths,     // Auto-approve for trusted paths
    TrustSession,   // Auto-approve all for this session
}

pub struct PermissionManager {
    mode: PermissionMode,
    trusted_paths: HashSet<PathBuf>,
    session_approvals: HashMap<String, Approval>,
}

impl PermissionManager {
    pub async fn prompt_tool_approval(&mut self, tool: &str, args: &Value) -> Approval;
    pub fn auto_approve(&self, tool: &str, args: &Value) -> Option<Approval>;
}
```

### 5. Plan Interaction (`plan_interaction.rs`)

Manages LLM-generated plans:

```rust
pub fn try_replace_plan_from_llm_json(
    plan: &Plan,
    llm_json: &Value,
) -> Option<Plan> {
    // Parse LLM's JSON plan output
    // Preserve completed subtasks even if LLM drops them
    // Maintain subtask order
}

pub fn render_plan_sidebar(plan: &Plan, width: usize) -> Vec<String>;
pub fn plan_progress_summary(plan: &Plan) -> String;
```

## Slash Handler Pattern

Each `slash_*.rs` file follows a consistent pattern:

```rust
// slash_example.rs

use super::*;

pub(super) async fn handle_example_command(
    state: &mut ReplState,
    args: &str,
    api: &ThinClient,
    token: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    // 1. Parse arguments (often with subcommand match)
    let parts: Vec<&str> = args.split_whitespace().collect();
    match parts.first().map(|s| *s) {
        Some("list") | None => handle_list(state).await?,
        Some("add") => handle_add(state, &parts[1..]).await?,
        Some(other) => cli_err!("Unknown subcommand: {other}"),
    }
    Ok(())
}

// Private helpers for each subcommand
async fn handle_list(state: &ReplState) -> Result<()> { ... }
async fn handle_add(state: &mut ReplState, args: &[&str]) -> Result<()> { ... }
```

## Extension Points

### Adding a New Slash Command

1. **Register in `command_registry.rs`**:
   ```rust
   CommandMeta::new("/newcmd", "Description", CommandGroup::System)
       .with_subcommands(&[("sub1", "Sub description")])
       .with_arg_hint("<arg>"),
   ```

2. **Create handler in `slash_newcmd.rs`**:
   ```rust
   pub(super) async fn handle_newcmd_command(
       state: &mut ReplState,
       args: &str,
       // ... other context as needed
   ) -> Result<()>;
   ```

3. **Wire up in `main.rs`** dispatch:
   ```rust
   "/newcmd" => slash_newcmd::handle_newcmd_command(state, args, ...).await,
   ```

### Adding Terminal Effects

1. Define effect type in `effects/mod.rs`
2. Implement `Drop` for cleanup
3. Use in stream handlers

## Testing

Test files live alongside source files:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_command_resolution() {
        assert_eq!(resolve_command("/h"), Some("/help"));
    }

    #[tokio::test]
    async fn test_plan_parsing() {
        let plan = try_replace_plan_from_llm_json(&old, &json);
        assert!(plan.is_some());
    }
}
```

Run CLI tests:
```bash
cd rust && cargo test -p astra-cli
```

## Performance Considerations

- **Startup**: Tool selector creation is expensive (~200ms). Deferred completions refresh to first REPL iteration.
- **Streaming**: SSE events processed incrementally; terminal updates batched.
- **Skills**: Hot-reload via `SkillWatcherHandle` avoids restart for skill changes.
- **Fuzzy matching**: Incremental scoring with early termination.

## Related Documentation

- [Slash Commands Reference](../reference/slash-commands.md) — Complete command reference
- [Skills and Tools](./skills-and-tools.md) — Skill system design
- [Multi-Agent Delegation](./multi-agent-delegation-guide.md) — Team orchestration
