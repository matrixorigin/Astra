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
    /// Agent fan-out / delegation (`agent`, `agent_fanout`).
    AgentDelegation,
    /// MCP protocol tools.
    McpProtocol,
    /// Code symbols / LSP (`symbols`, `lsp`, `find_definition`,
    /// `find_references`).
    Symbols,
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
