//! Provider type definitions — pure data enumerations for capability routing.
//!
//! These types describe *what* a provider can do and *what kind* of provider
//! it is. They form the vocabulary used by the capability registry at L1.

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// ToolCapability — what a provider can execute
// ---------------------------------------------------------------------------

/// Describes a capability a provider can fulfill.
///
/// Capabilities are either an exact tool-name match or a broader category.
/// The registry resolves provider selection via these capabilities.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ToolCapability {
    /// Exact tool-name match (e.g. `"bash"`, `"read_file"`).
    Named(String),
    /// Category-level match — the provider can handle any tool in this
    /// category.
    Category(ToolCategory),
}

// ---------------------------------------------------------------------------
// ToolCategory — broad buckets for tool routing
// ---------------------------------------------------------------------------

/// Broad category of tool execution.
///
/// Each tool in the system maps to exactly one category.  When routing,
/// the registry falls back to category matching if no exact-name match
/// is registered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ToolCategory {
    /// Shell / process execution (`bash`, `powershell`).
    Shell,
    /// File read / write / list / search (`read_file`, `write_file`,
    /// `glob`, `grep`).
    FileSystem,
    /// Git operations (`git` tool).
    VersionControl,
    /// External API calls (`web_fetch`, `web_search`, `github`).
    ExternalApi,
    /// Session-level state management (`memory`, `session`, `task`).
    StateManagement,
    /// Local typed background process/task projection (`task_output`,
    /// `task_stop`, `task_list`), distinct from the durable task board.
    BackgroundTaskProcess,
    /// Agent fan-out / delegation (`agent`, `agent_fanout`).
    AgentDelegation,
    /// MCP protocol tools.
    McpProtocol,
    /// Code symbols / LSP (`symbols`, `lsp`, `find_definition`,
    /// `find_references`).
    Symbols,
}

impl ToolCategory {
    /// Return the canonical category for a built-in tool name.
    ///
    /// Unknown tool names intentionally do not fall back to category providers:
    /// a provider advertising `Shell` should not become a catch-all executor
    /// for arbitrary named tools.
    pub fn for_tool_name(name: &str) -> Option<Self> {
        match name {
            "bash" | "powershell" | "run_script" | "background_shell" => Some(Self::Shell),
            "read_file"
            | "write_file"
            | "str_replace"
            | "delete_file"
            | "multi_edit"
            | "rollback_file_edits"
            | "list_dir"
            | "grep"
            | "glob"
            | "publish_artifact" => Some(Self::FileSystem),
            "git" | "git_clone" => Some(Self::VersionControl),
            "web_search" | "web_fetch" | "github" | "tool_search" => Some(Self::ExternalApi),
            "ask_user"
            | "notify"
            | "enter_plan_mode"
            | "exit_plan_mode"
            | "get_agent_info"
            | "introspect"
            | "compress_context"
            | "memory"
            | "session"
            | "task"
            | "mo_query"
            | "rollback_database_snapshots"
            | "rollback_session_state" => Some(Self::StateManagement),
            "task_output" | "task_stop" | "task_list" => Some(Self::BackgroundTaskProcess),
            "agent" | "agent_fanout" => Some(Self::AgentDelegation),
            "symbols" | "lsp" | "find_definition" | "find_references" => Some(Self::Symbols),
            _ if name.starts_with("mcp__") => Some(Self::McpProtocol),
            _ => None,
        }
    }
}

// ---------------------------------------------------------------------------
// ProviderKind — where execution happens
// ---------------------------------------------------------------------------

/// The *kind* of provider — where and how it executes tools.
///
/// This drives the deployment profile selection at runtime.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ProviderKind {
    /// Tools built into the server process (always available, no isolation).
    ServerBuiltin,
    /// Edge-executed tools in the CLI process (local file access).
    EdgeConnection,
    /// Tools running in a sandboxed runtime (Firecracker, Docker, etc.).
    SandboxRuntime,
    /// Tools served by an external MCP server.
    McpServer,
}

#[cfg(test)]
mod tests {
    use super::ToolCategory;

    #[test]
    fn tool_name_category_mapping_covers_control_plane_and_mcp() {
        assert_eq!(
            ToolCategory::for_tool_name("bash"),
            Some(ToolCategory::Shell)
        );
        assert_eq!(
            ToolCategory::for_tool_name("ask_user"),
            Some(ToolCategory::StateManagement)
        );
        assert_eq!(
            ToolCategory::for_tool_name("task"),
            Some(ToolCategory::StateManagement)
        );
        for local_background_tool in ["task_output", "task_stop", "task_list"] {
            assert_eq!(
                ToolCategory::for_tool_name(local_background_tool),
                Some(ToolCategory::BackgroundTaskProcess),
                "{local_background_tool} must not be confused with the durable task board"
            );
        }
        assert_eq!(
            ToolCategory::for_tool_name("mcp__node_repl__js"),
            Some(ToolCategory::McpProtocol)
        );
        assert_eq!(ToolCategory::for_tool_name("unknown_tool"), None);
    }
}
