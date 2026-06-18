//! Deployment profile type definitions.
//!
//! A deployment profile describes *what* execution backends are available
//! for a runtime session and *how* they are configured.  Profiles are
//! typically loaded from YAML at startup.
//!
//! ## Product-configuration shortcuts
//!
//! For common deployment shapes, use the pre-built profiles:
//!
//! ```ignore
//! use astra_runtime::deployment::DeploymentProfile;
//!
//! // Full server with all tools
//! let profile = DeploymentProfile::server_default();
//!
//! // Server without web_search / web_fetch
//! let profile = DeploymentProfile::server_without(&["web_search", "web_fetch"]);
//!
//! // Server with only explicit tools
//! let profile = DeploymentProfile::server_with_only(&["bash", "read_file"]);
//!
//! // Build the CapabilityRegistry
//! let registry = profile.build_registry(server_runtime).await?;
//! service.set_capability_registry(registry);
//! ```

use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::capability_registry::CapabilityRegistry;
use crate::provider::server_builtin::ServerBuiltinProvider;
use crate::provider::traits::{ProviderError, ServerToolRuntime};
use crate::provider::types::{ProviderKind, ToolCapability};
use crate::storage::{MountType, StorageAccess, WorkspaceSource};
use astra_runtime_env::IsolationIntent;

// ---------------------------------------------------------------------------
// Sandbox execution hints
// ---------------------------------------------------------------------------

/// Which sandbox manager orchestrates the lifecycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SandboxManagerKind {
    /// Firecracker microVM manager.
    Firecracker,
    /// Docker container runtime.
    Docker,
    /// gVisor sandbox runtime.
    GVisor,
}

/// Underlying isolation mechanism.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IsolationBackend {
    /// Process-level isolation (fork + exec).
    Process,
    /// Linux namespaces / seccomp.
    Namespace,
    /// Full VM-level isolation.
    MicroVM,
    /// Container-level isolation.
    Container,
}

/// How the sandbox process is launched.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LaunchDriver {
    /// Direct process invocation.
    Direct,
    /// Via a daemon / socket.
    Daemon,
}

// ---------------------------------------------------------------------------
// SandboxConfig — per-provider sandbox configuration
// ---------------------------------------------------------------------------

/// Configuration for a sandboxed execution environment.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SandboxConfig {
    /// Container / VM image to use.
    pub image: String,
    /// Where the workspace comes from.
    pub workspace_source: WorkspaceSource,
    /// Storage mounts to attach.
    #[serde(default)]
    pub storage_mounts: Vec<StorageAccess>,
    /// Isolation level this sandbox provides.
    pub isolation: IsolationIntent,
    /// Sandbox manager kind.
    #[serde(default)]
    pub manager: Option<SandboxManagerKind>,
    /// Isolation backend mechanism.
    #[serde(default)]
    pub isolation_backend: Option<IsolationBackend>,
    /// Launch driver.
    #[serde(default)]
    pub launch_driver: Option<LaunchDriver>,
    /// CPU limit (e.g. "2.0" for 2 vCPUs).
    pub cpu_limit: Option<String>,
    /// Memory limit (e.g. "512Mi").
    pub memory_limit: Option<String>,
    /// Execution timeout in seconds.
    pub timeout_secs: u64,
}

// ---------------------------------------------------------------------------
// ProviderConfig — per-provider registration
// ---------------------------------------------------------------------------

/// Configuration for a single capability provider.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderConfig {
    /// Provider kind.
    pub kind: ProviderKind,
    /// Capabilities this provider handles.
    #[serde(default)]
    pub capabilities: Vec<ToolCapability>,
    /// Routing priority (lower = preferred).
    pub priority: u8,
    /// Optional sandbox configuration (only for SandboxRuntime kind).
    pub sandbox: Option<SandboxConfig>,
}

// ---------------------------------------------------------------------------
// StorageConfig — default storage configuration
// ---------------------------------------------------------------------------

/// Global / default storage configuration for a deployment profile.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StorageConfig {
    /// How the volume is mounted.
    pub mount_type: MountType,
    /// Path inside the execution environment.
    pub mount_path: String,
    /// Source path on the host / external system.
    pub source_path: String,
    /// Whether the mount is read-only.
    #[serde(default)]
    pub read_only: bool,
}

// ---------------------------------------------------------------------------
// DeploymentProfile — the top-level configuration
// ---------------------------------------------------------------------------

/// A named deployment profile that describes a complete execution topology.
///
/// Loaded from YAML at startup.  Each profile specifies which providers
/// are available and how they are configured.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeploymentProfile {
    /// Human-readable profile name (e.g. "local-cli", "sandboxed-cloud").
    pub profile_name: String,
    /// All capability providers in this profile.
    #[serde(default)]
    pub providers: Vec<ProviderConfig>,
    /// Default storage configuration for workspace access.
    pub default_storage: Option<StorageConfig>,
}

// ---------------------------------------------------------------------------
// Pre-built profiles (product-configuration shortcut layer)
// ---------------------------------------------------------------------------

/// All tool names that a server deployment can provide.
///
/// Ordered by category for readability.  Each tool name maps to the
/// underlying handler registered in `server_tool_executor/tool_handlers.rs`.
pub const SERVER_ALL_TOOLS: &[&str] = &[
    // Shell
    "bash",
    // FileSystem
    "read_file",
    "write_file",
    "str_replace",
    "list_dir",
    "grep",
    "glob",
    // VersionControl
    "git",
    // ExternalApi
    "web_search",
    "web_fetch",
    "github",
    "tool_search",
    // StateManagement
    "memory",
    "session",
    "task",
    // AgentDelegation
    "agent",
    "agent_fanout",
    // Symbols / introspection
    "get_agent_info",
    "symbols",
    // Internal
    "notify",
];

impl DeploymentProfile {
    /// Standard server deployment with all built-in tools.
    pub fn server_default() -> Self {
        Self {
            profile_name: "server-default".into(),
            providers: vec![ProviderConfig {
                kind: ProviderKind::ServerBuiltin,
                capabilities: SERVER_ALL_TOOLS
                    .iter()
                    .map(|t| ToolCapability::Named(t.to_string()))
                    .collect(),
                priority: 10,
                sandbox: None,
            }],
            default_storage: None,
        }
    }

    /// Server deployment with the given tools **disabled**.
    ///
    /// ```ignore
    /// let profile = DeploymentProfile::server_without(&["web_search", "web_fetch"]);
    /// ```
    pub fn server_without(disabled: &[&str]) -> Self {
        let mut profile = Self::server_default();
        if let Some(prov) = profile.providers.first_mut() {
            prov.capabilities.retain(|c| match c {
                ToolCapability::Named(name) => !disabled.contains(&name.as_str()),
                _ => true,
            });
        }
        profile
    }

    /// Server deployment with **only** the given tools enabled.
    ///
    /// ```ignore
    /// let profile = DeploymentProfile::server_with_only(&["bash", "read_file", "write_file"]);
    /// ```
    pub fn server_with_only(enabled: &[&str]) -> Self {
        let mut profile = Self::server_default();
        if let Some(prov) = profile.providers.first_mut() {
            prov.capabilities.retain(|c| match c {
                ToolCapability::Named(name) => enabled.contains(&name.as_str()),
                _ => false,
            });
        }
        profile
    }

    /// Build a `CapabilityRegistry` from this deployment profile.
    ///
    /// Requires a `ServerToolRuntime` for the builtin provider.
    /// All other provider kinds are skipped with a warning.
    pub async fn build_registry(
        &self,
        builtin_runtime: Arc<dyn ServerToolRuntime>,
    ) -> Result<CapabilityRegistry, ProviderError> {
        let registry = CapabilityRegistry::new();

        for provider_cfg in &self.providers {
            match provider_cfg.kind {
                ProviderKind::ServerBuiltin => {
                    let tool_names: Option<Vec<String>> = if provider_cfg.capabilities.is_empty() {
                        None // empty → full category set
                    } else {
                        let names: Vec<String> = provider_cfg
                            .capabilities
                            .iter()
                            .filter_map(|c| match c {
                                ToolCapability::Named(n) => Some(n.clone()),
                                _ => None,
                            })
                            .collect();
                        if names.is_empty() { None } else { Some(names) }
                    };

                    let provider = Arc::new(ServerBuiltinProvider::new(
                        provider_cfg.priority,
                        Arc::clone(&builtin_runtime),
                        tool_names,
                    ));
                    registry
                        .register(
                            format!("server-builtin-p{}", provider_cfg.priority),
                            provider,
                        )
                        .await?;
                }
                // Other provider kinds (EdgeConnection, SandboxRuntime, McpServer)
                // require their own runtime dependencies and are wired in L2+.
                _ => {
                    tracing::warn!(
                        "build_registry: skipping provider kind {:?} — requires L2+ wiring",
                        provider_cfg.kind
                    );
                }
            }
        }

        Ok(registry)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn server_default_has_all_tools() {
        let profile = DeploymentProfile::server_default();
        assert_eq!(profile.providers.len(), 1);
        let caps = &profile.providers[0].capabilities;
        assert_eq!(caps.len(), SERVER_ALL_TOOLS.len());
        let names: Vec<&str> = caps
            .iter()
            .filter_map(|c| match c {
                ToolCapability::Named(n) => Some(n.as_str()),
                _ => None,
            })
            .collect();
        assert!(names.contains(&"bash"));
        assert!(names.contains(&"web_search"));
        assert!(names.contains(&"memory"));
        assert!(names.contains(&"agent"));
    }

    #[test]
    fn server_without_removes_tools() {
        let profile = DeploymentProfile::server_without(&["web_search", "web_fetch"]);
        let caps = &profile.providers[0].capabilities;
        let names: Vec<&str> = caps
            .iter()
            .filter_map(|c| match c {
                ToolCapability::Named(n) => Some(n.as_str()),
                _ => None,
            })
            .collect();
        assert!(!names.contains(&"web_search"));
        assert!(!names.contains(&"web_fetch"));
        assert!(names.contains(&"bash"));
        assert!(names.contains(&"memory"));
        assert_eq!(names.len(), SERVER_ALL_TOOLS.len() - 2);
    }

    #[test]
    fn server_with_only_keeps_specified() {
        let profile = DeploymentProfile::server_with_only(&["bash", "read_file", "memory"]);
        let caps = &profile.providers[0].capabilities;
        let names: Vec<&str> = caps
            .iter()
            .filter_map(|c| match c {
                ToolCapability::Named(n) => Some(n.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(names.len(), 3);
        assert!(names.contains(&"bash"));
        assert!(names.contains(&"read_file"));
        assert!(names.contains(&"memory"));
        assert!(!names.contains(&"web_search"));
        assert!(!names.contains(&"agent"));
    }

    #[test]
    fn server_default_profile_name() {
        let profile = DeploymentProfile::server_default();
        assert_eq!(profile.profile_name, "server-default");
    }

    // Integration-style tests for build_registry are in
    // tests/deployment_integration_tests.rs (requires tokio runtime).
}
