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
    use std::fs::OpenOptions;
    use std::io::Write;

    use fs2::FileExt;

    static BINARY: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();

    BINARY
        .get_or_init(|| {
            let manifest_dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
            let profile = if cfg!(debug_assertions) {
                "debug"
            } else {
                "release"
            };
            let binary_name = format!("mock_mcp_server{}", std::env::consts::EXE_SUFFIX);
            let binary = manifest_dir
                .join("../..")
                .join("target")
                .join(profile)
                .join(&binary_name);
            let lock_path = binary.with_file_name(format!("{binary_name}.build.lock"));

            if !binary.exists() {
                std::fs::create_dir_all(
                    binary
                        .parent()
                        .expect("mock_mcp_server binary always has a parent directory"),
                )
                .unwrap_or_else(|error| {
                    panic!(
                        "failed to create mock_mcp_server directory {:?}: {error}",
                        binary.parent()
                    )
                });

                let mut lock_file = OpenOptions::new()
                    .create(true)
                    .truncate(false)
                    .read(true)
                    .write(true)
                    .open(&lock_path)
                    .unwrap_or_else(|error| {
                        panic!(
                            "failed to open mock_mcp_server build lock {:?}: {error}",
                            lock_path
                        )
                    });
                lock_file.lock_exclusive().unwrap_or_else(|error| {
                    panic!(
                        "failed to acquire mock_mcp_server build lock {:?}: {error}",
                        lock_path
                    )
                });

                if !binary.exists() {
                    let _ = lock_file.set_len(0);
                    let _ = writeln!(
                        lock_file,
                        "building mock_mcp_server for pid {}",
                        std::process::id()
                    );

                    let status = std::process::Command::new("cargo")
                        .args(["build", "-p", "astra-cli", "--bin", "mock_mcp_server"])
                        .current_dir(manifest_dir.join("../.."))
                        .status()
                        .unwrap_or_else(|error| {
                            panic!("failed to build mock_mcp_server bin: {error}")
                        });

                    assert!(
                        status.success(),
                        "cargo build -p astra-cli --bin mock_mcp_server failed with status {status}"
                    );
                }
            }

            assert!(
                binary.exists(),
                "mock_mcp_server binary missing at {:?} after prebuild or fallback build",
                binary
            );

            binary
        })
        .clone()
}
