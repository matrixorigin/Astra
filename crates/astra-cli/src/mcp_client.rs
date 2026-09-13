//! CLI-facing MCP adapter.
//!
//! The protocol client lives in `astra-mcp`. This module only re-exports the
//! shared client types and keeps CLI-specific skill-registry wiring at the
//! session boundary.

pub use astra_mcp::{
    CallLogEntry, ConnectionState, MAX_RESULT_CONTENT_LENGTH, McpClientManager, McpError,
    McpServerConfig, RetryConfig, Transport, extract_result_text_with_limit, sanitize_tool_name,
};

/// Connect an MCP server and register any `skill://` resources it exposes.
pub async fn connect_and_discover_skills(
    manager: &std::sync::Arc<tokio::sync::RwLock<McpClientManager>>,
    config: McpServerConfig,
    skill_registry: &astra_runtime::skills::UnifiedSkillRegistry,
) -> Result<usize, McpError> {
    let server_name = config.name.clone();
    let roots = manager.read().await.roots().clone();
    let prepared = match McpClientManager::prepare_connection(config, roots).await {
        Ok(prepared) => prepared,
        Err(error) => {
            manager
                .write()
                .await
                .record_connection_failure(server_name.clone());
            return Err(error);
        }
    };
    let Some(prepared) = prepared else {
        return Ok(0);
    };
    let conn = prepared.connection();
    manager.write().await.install_prepared_connection(prepared);

    let skill_resources = conn.discover_skill_resources().await;
    let mut registered = 0usize;
    for (_name, content) in &skill_resources {
        match skill_registry
            .register_mcp_skill(&server_name, content)
            .await
        {
            Ok(_) => registered += 1,
            Err(error) => {
                tracing::warn!(
                    server = %server_name,
                    error = %error,
                    "MCP: failed to register skill"
                );
            }
        }
    }

    if registered > 0 {
        tracing::info!(
            server = %server_name,
            skills = registered,
            "MCP: registered skills from server"
        );
    }

    Ok(registered)
}

/// Disconnect an MCP server and remove its `skill://` resources.
pub async fn disconnect_and_remove_skills(
    manager: &mut McpClientManager,
    name: &str,
    skill_registry: &astra_runtime::skills::UnifiedSkillRegistry,
) -> bool {
    let removed = manager.disconnect(name);
    if removed && let Err(error) = skill_registry.remove_mcp_server_skills(name).await {
        tracing::warn!(
            server = name,
            error = %error,
            "MCP: failed to remove skills for server"
        );
    }
    removed
}

#[cfg(test)]
pub(crate) fn ensure_mock_mcp_server_binary() -> std::path::PathBuf {
    // Cargo places this unit-test executable in <target>/<profile>/deps.
    // Deriving the fixture beside it honors custom target directories and
    // target triples instead of assuming the checkout's target/debug.
    let executable = std::env::current_exe().expect("locate MCP test executable");
    let profile_dir = executable
        .parent()
        .and_then(std::path::Path::parent)
        .expect("MCP test executable must live in the Cargo deps directory");
    let binary = profile_dir.join(format!("mock_mcp_server{}", std::env::consts::EXE_SUFFIX));
    assert!(
        binary.is_file(),
        "mock MCP fixture missing at {}. Build it before running tests: cargo build -p astra-cli --bin mock_mcp_server (with the same target directory and profile). make test-offline prepares it automatically.",
        binary.display()
    );
    // Never start Cargo from a timed test: compilation is setup, and nested
    // builds can contend with other tests for the artifact lock.
    binary
}
