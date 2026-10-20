//! MCP JSON configuration management (read, write, add, remove, list, get).
//!
//! Extracted from `command_router.rs` as part of the god-module refactor (P0-2).

use super::theme;
use crate::cli::cli_config::cli_args::McpCmd;
use crossterm::style::Stylize;
use std::io::Write;

/// Stable CLI capability guidance; syntax and validation stay with `astra mcp --help`.
pub(crate) const MCP_SETUP_GUIDANCE: &str = "Astra supports project/user mcp.json configuration. For MCP setup, use `astra mcp --help` and `astra mcp list --effective` through the explicitly selected local executor; preserve its permissions. `add-json` accepts one server entry extracted from mcpServers; choose project or user scope. Do not print or repeat credentials; use protected local files for secret-bearing configuration rather than inline shell arguments. Configuration does not install software or prove connectivity and takes effect at the next chat startup. `test`/`ping` launch configured processes or contact networks: run only when authorized, in the intended scope; a scoped check does not prove the effective winner. If local execution is unavailable, give these instructions without accessing the Server filesystem.";

/// Transport errors can contain credential-bearing URLs or provider payloads.
/// Keep command output actionable without copying those values to the terminal.
fn connection_failure(name: &str, error: &astra_mcp::McpError) -> String {
    let cause = match error {
        astra_mcp::McpError::InvalidConfig(_) => "invalid transport configuration",
        astra_mcp::McpError::Spawn(_) => "could not start the configured process",
        astra_mcp::McpError::Initialize(_) => "MCP initialization failed",
        _ => "MCP service failed",
    };
    format!(
        "MCP check failed for '{name}': {cause}. Check the selected configuration, executable, authentication and connectivity; connection details omitted."
    )
}

/// Serialize cooperating writers across atomic replacement of the config file.
/// The sibling lock is stable and must never be removed while writers may exist.
fn lock_mcp_config(path: &std::path::Path) -> Result<std::fs::File, String> {
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| std::path::Path::new("."));
    std::fs::create_dir_all(parent)
        .map_err(|e| format!("Cannot create MCP config directory: {e}"))?;
    let mut name = path.as_os_str().to_os_string();
    name.push(".lock");
    let lock_path = std::path::PathBuf::from(name);
    match std::fs::symlink_metadata(&lock_path) {
        Ok(meta) if !meta.file_type().is_file() => {
            return Err("MCP config lock must be a regular file, not a symlink".into());
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(format!("Cannot inspect MCP config lock: {e}")),
        Ok(_) => {}
    }
    let mut options = std::fs::OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    let file = options
        .open(&lock_path)
        .map_err(|e| format!("Cannot open MCP config lock: {e}"))?;
    if !file
        .metadata()
        .map_err(|e| format!("Cannot inspect MCP config lock: {e}"))?
        .is_file()
    {
        return Err("MCP config lock must be a regular file".into());
    }
    file.lock()
        .map_err(|e| format!("Cannot acquire MCP config lock: {e}"))?;
    Ok(file)
}

fn validate_mcp_config_shape(
    config: serde_json::Value,
    source: &str,
) -> Result<serde_json::Value, String> {
    if !config.is_object() {
        return Err(format!("{source} must be a JSON object"));
    }
    if let Some(mcp_servers) = config.get("mcpServers")
        && !mcp_servers.is_object()
    {
        return Err(format!(
            "{source} field \"mcpServers\" must be a JSON object"
        ));
    }
    Ok(config)
}

pub(crate) fn load_mcp_configs(sources: &[String]) -> Result<(), String> {
    let project_path = crate::manifest_loader::project_mcp_json_path()
        .ok_or_else(|| "Cannot determine project directory for MCP config".to_string())?;
    load_mcp_configs_at_path(sources, &project_path)
}

fn load_mcp_configs_at_path(
    sources: &[String],
    project_path: &std::path::Path,
) -> Result<(), String> {
    let _guard = lock_mcp_config(project_path)?;
    let mut config = read_mcp_config(project_path)?;

    for (index, source) in sources.iter().enumerate() {
        let json_str = if std::path::Path::new(source).is_file() {
            std::fs::read_to_string(source)
                .map_err(|e| format!("Failed to read MCP config file '{}': {e}", source))?
        } else {
            source.clone()
        };
        // Inline sources may contain credentials, as may serde's rejected values.
        let label = format!("MCP config source {}", index + 1);
        let parsed = parse_mcp_config_source(&json_str, &label)?;

        if let Some(servers) = parsed.get("mcpServers").and_then(|v| v.as_object()) {
            let target = config
                .as_object_mut()
                .ok_or("MCP config must be a JSON object")?
                .entry("mcpServers")
                .or_insert_with(|| serde_json::json!({}))
                .as_object_mut()
                .ok_or("mcpServers value must be a JSON object")?;
            for (name, entry) in servers {
                target.insert(name.clone(), entry.clone());
            }
        } else {
            return Err(format!("{label} must contain a \"mcpServers\" object"));
        }
    }

    write_mcp_config(project_path, &config)?;
    Ok(())
}

fn parse_mcp_config_source(json: &str, source: &str) -> Result<serde_json::Value, String> {
    let parsed = serde_json::from_str(json).map_err(|e| json_parse_error(source, &e))?;
    let parsed = validate_mcp_config_shape(parsed, source)?;
    if parsed.get("mcpServers").is_none() {
        return Err(format!("{source} must contain a \"mcpServers\" object"));
    }
    Ok(parsed)
}

/// Resolve the mcp.json path for the given scope.
fn mcp_json_path_for_scope(scope: &str) -> Result<std::path::PathBuf, String> {
    match scope {
        "project" => crate::manifest_loader::project_mcp_json_path()
            .ok_or_else(|| "Cannot determine project directory".to_string()),
        "user" => crate::manifest_loader::global_mcp_json_path()
            .ok_or_else(|| "Cannot determine home directory".to_string()),
        other => Err(format!("Unknown scope '{other}' — use 'project' or 'user'")),
    }
}

fn json_parse_error(source: &str, error: &serde_json::Error) -> String {
    format!(
        "Failed to parse {source} (line {}, column {})",
        error.line(),
        error.column()
    )
}

/// Read and parse an mcp.json file, returning empty config only if missing.
fn read_mcp_config(path: &std::path::Path) -> Result<serde_json::Value, String> {
    let Some(content) = crate::manifest_loader::read_optional_mcp_config_file(path)? else {
        return Ok(serde_json::json!({"mcpServers": {}}));
    };
    let parsed = serde_json::from_str(&content)
        .map_err(|e| json_parse_error(&path.display().to_string(), &e))?;
    validate_mcp_config_shape(parsed, &path.display().to_string())
}

/// Write config atomically (temp + rename).
fn write_mcp_config(path: &std::path::Path, config: &serde_json::Value) -> Result<(), String> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| std::path::Path::new("."));
    std::fs::create_dir_all(parent)
        .map_err(|e| format!("Failed to create {}: {e}", parent.display()))?;
    let pretty = serde_json::to_string_pretty(config)
        .map_err(|e| format!("Failed to serialize {}: {e}", path.display()))?;
    let mut tmp = tempfile::NamedTempFile::new_in(parent)
        .map_err(|e| format!("Failed to create temp file in {}: {e}", parent.display()))?;
    tmp.write_all(pretty.as_bytes())
        .map_err(|e| format!("Failed to write temp file for {}: {e}", path.display()))?;
    tmp.flush()
        .map_err(|e| format!("Failed to flush temp file for {}: {e}", path.display()))?;
    tmp.persist(path).map_err(|e| {
        format!(
            "Failed to replace {} atomically: {}",
            path.display(),
            e.error
        )
    })?;
    Ok(())
}

pub(crate) async fn execute_mcp_command(cmd: McpCmd) -> Result<(), String> {
    match cmd {
        McpCmd::List(args) => mcp_list(&args.scope, args.effective),
        McpCmd::Add(args) => mcp_add(&args.name, &args.command, &args.args, &args.scope),
        McpCmd::AddJson(args) => mcp_add_json(&args.name, &args.json, &args.scope),
        McpCmd::Remove(args) => mcp_remove(&args.name, &args.scope),
        McpCmd::Get(args) => mcp_get(&args.name),
        McpCmd::Test(args) => mcp_test(&args.name, &args.scope).await,
        McpCmd::Ping(args) => mcp_ping(&args.name, &args.scope).await,
    }
}

fn mcp_list(scope: &str, effective: bool) -> Result<(), String> {
    let discovery = if effective {
        crate::manifest_loader::discover_mcp_server_configs()
    } else {
        let path = mcp_json_path_for_scope(scope)?;
        crate::manifest_loader::discover_mcp_server_configs_from_paths(
            (scope == "project").then_some(path.as_path()),
            &[],
            (scope == "user").then_some(path.as_path()),
        )
    };
    stdout_print!("{}", render_mcp_list(&discovery, scope, effective));
    Ok(())
}

fn render_mcp_list(
    discovery: &crate::manifest_loader::McpConfigDiscovery,
    scope: &str,
    effective: bool,
) -> String {
    use std::fmt::Write as _;
    let mut output = String::new();
    if effective {
        writeln!(
            output,
            "MCP configuration (effective; first enabled definition wins: project > skills > user)"
        )
        .unwrap();
    } else {
        writeln!(output, "MCP configuration (scope: {scope})").unwrap();
        writeln!(output, "Use `astra mcp list --effective` for chat's configuration, or `astra mcp list -s user` for user scope.").unwrap();
    }
    for diagnostic in &discovery.diagnostics {
        writeln!(output, "Warning: {diagnostic}").unwrap();
    }
    if discovery.servers.is_empty() {
        if discovery.diagnostics.is_empty() {
            writeln!(output, "No enabled MCP servers configured in this view.").unwrap();
        } else {
            writeln!(
                output,
                "No enabled MCP servers loaded; resolve the configuration warnings above."
            )
            .unwrap();
        }
        writeln!(
            output,
            "Use `astra mcp add` or `astra mcp add-json` to configure a server."
        )
        .unwrap();
    } else {
        writeln!(output, "{:<20} {:<16} Source", "Name", "Type").unwrap();
        for server in &discovery.servers {
            writeln!(
                output,
                "{:<20} {:<16} {} ({})",
                server.config.name,
                transport_name(&server.config.transport),
                server.scope,
                server.path.display()
            )
            .unwrap();
        }
    }
    writeln!(output, "Connection settings are hidden. Configuration discovery does not test connectivity; `/mcp` in chat shows connected servers.").unwrap();
    output
}

fn transport_name(transport: &astra_mcp::Transport) -> &'static str {
    match transport {
        astra_mcp::Transport::Stdio { .. } => "stdio",
        astra_mcp::Transport::Sse { .. } => "sse",
        astra_mcp::Transport::StreamableHttp { .. } => "streamable_http",
        astra_mcp::Transport::Ws { .. } => "ws",
    }
}

fn mcp_add(name: &str, command: &str, args: &[String], scope: &str) -> Result<(), String> {
    let path = mcp_json_path_for_scope(scope)?;
    mcp_add_at_path(name, command, args, &path)
}

fn mcp_add_at_path(
    name: &str,
    command: &str,
    args: &[String],
    path: &std::path::Path,
) -> Result<(), String> {
    let config_lock = lock_mcp_config(path)?;
    let mut config = read_mcp_config(path)?;

    // Check for duplicate
    if let Some(servers) = config.get("mcpServers").and_then(|v| v.as_object()) {
        if servers.contains_key(name) {
            return Err(format!(
                "Server '{name}' already exists. Remove it first with: astra mcp remove {name}"
            ));
        }
    }

    let entry = serde_json::json!({
        "command": command,
        "args": args,
    });
    config
        .as_object_mut()
        .ok_or("MCP config must be a JSON object")?
        .entry("mcpServers")
        .or_insert_with(|| serde_json::json!({}))
        .as_object_mut()
        .ok_or("mcpServers value must be a JSON object")?
        .insert(name.to_string(), entry);

    write_mcp_config(path, &config)?;
    drop(config_lock);
    stdout_println!(
        "  {} Added '{}' to {}",
        theme::icon_ok(),
        name.magenta(),
        path.display().to_string().dim()
    );
    Ok(())
}

fn mcp_add_json(name: &str, json: &str, scope: &str) -> Result<(), String> {
    let path = mcp_json_path_for_scope(scope)?;
    mcp_add_json_at_path(name, json, &path)
}

fn mcp_add_json_at_path(name: &str, json: &str, path: &std::path::Path) -> Result<(), String> {
    let entry: serde_json::Value =
        serde_json::from_str(json).map_err(|e| json_parse_error("MCP server JSON", &e))?;
    if !entry.is_object() {
        return Err("JSON config must be an object".to_string());
    }

    let config_lock = lock_mcp_config(path)?;
    let mut config = read_mcp_config(path)?;

    if let Some(servers) = config.get("mcpServers").and_then(|v| v.as_object()) {
        if servers.contains_key(name) {
            return Err(format!(
                "Server '{name}' already exists. Remove it first with: astra mcp remove {name}"
            ));
        }
    }

    config
        .as_object_mut()
        .ok_or("MCP config must be a JSON object")?
        .entry("mcpServers")
        .or_insert_with(|| serde_json::json!({}))
        .as_object_mut()
        .ok_or("mcpServers value must be a JSON object")?
        .insert(name.to_string(), entry);

    write_mcp_config(path, &config)?;
    drop(config_lock);
    stdout_println!(
        "  {} Added '{}' to {}",
        theme::icon_ok(),
        name.magenta(),
        path.display().to_string().dim()
    );
    Ok(())
}

fn mcp_remove(name: &str, scope: &str) -> Result<(), String> {
    let path = mcp_json_path_for_scope(scope)?;
    mcp_remove_at_path(name, &path)
}

fn mcp_remove_at_path(name: &str, path: &std::path::Path) -> Result<(), String> {
    // Reject absent input before the writer protocol creates directories/locks.
    // This is only a preflight: the mutation still reads its snapshot under lock.
    match path.metadata() {
        Ok(metadata) if metadata.is_file() => {}
        Ok(_) => {
            return Err(format!(
                "MCP config {} must be a regular file",
                path.display()
            ));
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err(format!("No config file at {}", path.display()));
        }
        Err(error) => {
            return Err(format!(
                "Cannot inspect MCP config {}: {error}",
                path.display()
            ));
        }
    }
    let config_lock = lock_mcp_config(path)?;
    if !path.is_file() {
        return Err(format!("No config file at {}", path.display()));
    }
    let mut config = read_mcp_config(path)?;

    let removed = config
        .get_mut("mcpServers")
        .and_then(|v| v.as_object_mut())
        .map(|m| m.remove(name).is_some())
        .unwrap_or(false);

    if !removed {
        return Err(format!("Server '{name}' not found in {}", path.display()));
    }

    write_mcp_config(path, &config)?;
    drop(config_lock);
    stdout_println!(
        "  {} Removed '{}' from {}",
        theme::icon_ok(),
        name.magenta(),
        path.display().to_string().dim()
    );
    Ok(())
}

fn mcp_get(name: &str) -> Result<(), String> {
    let discovery = crate::manifest_loader::discover_mcp_server_configs();
    stdout_print!("{}", render_mcp_get(name, &discovery)?);
    Ok(())
}

fn render_mcp_get(
    name: &str,
    discovery: &crate::manifest_loader::McpConfigDiscovery,
) -> Result<String, String> {
    use std::fmt::Write as _;
    let mut output = String::new();
    for diagnostic in &discovery.diagnostics {
        writeln!(output, "Warning: {diagnostic}").unwrap();
    }
    let Some(server) = discovery
        .servers
        .iter()
        .find(|server| server.config.name == name)
    else {
        return Err(format!(
            "{output}No enabled MCP server found with name: {name}"
        ));
    };
    writeln!(output, "{} (effective configuration):", server.config.name).unwrap();
    writeln!(output, "  Scope: {}", server.scope).unwrap();
    writeln!(output, "  Source: {}", server.path.display()).unwrap();
    writeln!(
        output,
        "  Type: {}",
        transport_name(&server.config.transport)
    )
    .unwrap();
    writeln!(output, "  Connection settings: [hidden]").unwrap();
    writeln!(
        output,
        "Configuration discovery does not test connectivity."
    )
    .unwrap();
    Ok(output)
}

/// Diagnose exactly the enabled configuration chat would load from this file.
fn diagnostic_mcp_config(
    name: &str,
    path: &std::path::Path,
) -> Result<astra_mcp::McpServerConfig, String> {
    let discovery =
        crate::manifest_loader::discover_mcp_server_configs_from_paths(Some(path), &[], None);
    if let Some(server) = discovery
        .servers
        .into_iter()
        .find(|server| server.config.name == name)
    {
        let mut config = server.config;
        // A user-requested diagnostic performs one top-level connection attempt.
        config.retry.max_retries = 0;
        return Ok(config);
    }
    let mut errors = discovery.diagnostics;
    errors.push(format!(
        "No enabled MCP server found with name '{name}' in {}",
        path.display()
    ));
    Err(errors.join("\n"))
}

async fn mcp_test(name: &str, scope: &str) -> Result<(), String> {
    let path = mcp_json_path_for_scope(scope)?;
    let mcp_config = diagnostic_mcp_config(name, &path)?;

    eprintln!("  {} Connecting to {}...", theme::icon_ok(), name.magenta());

    let mut manager = astra_mcp::McpClientManager::new();
    let tool_count = manager
        .connect(mcp_config)
        .await
        .map_err(|e| connection_failure(name, &e))?;

    let conn = manager
        .get(name)
        .ok_or_else(|| format!("Connected but cannot find '{name}'"))?;

    eprintln!(
        "  {} Connected successfully ({} tools, uptime {}s)",
        theme::icon_ok(),
        tool_count.to_string().magenta(),
        conn.uptime()
            .map(|d| d.as_secs().to_string())
            .unwrap_or_else(|| "n/a".to_string())
    );

    let tools = conn.tools();
    if !tools.is_empty() {
        eprintln!("\n  {}:", "Tools".bold());
        for tool in tools {
            let desc = tool.description.as_deref().unwrap_or("");
            let short_desc = if desc.len() > 80 {
                format!("{}...", &desc[..desc.floor_char_boundary(80)])
            } else {
                desc.to_string()
            };
            eprintln!("    {} {}", tool.name.magenta(), short_desc.dim());
        }
    }

    if let Ok(resources) = conn.list_resources().await
        && !resources.is_empty()
    {
        eprintln!("\n  {}:", "Resources".bold());
        for resource in &resources {
            eprintln!(
                "    {} -> {}",
                resource.raw.name.clone().magenta(),
                resource.raw.uri.clone().dim()
            );
        }
    }

    manager.disconnect(name);
    eprintln!("\n  {} Disconnected.", theme::icon_ok());
    Ok(())
}

async fn mcp_ping(name: &str, scope: &str) -> Result<(), String> {
    let path = mcp_json_path_for_scope(scope)?;
    let mcp_config = diagnostic_mcp_config(name, &path)?;

    let mut manager = astra_mcp::McpClientManager::new();
    manager
        .connect(mcp_config)
        .await
        .map_err(|e| connection_failure(name, &e))?;

    match manager.ping(name).await {
        Ok(latency) => {
            eprintln!(
                "  {} {} - {}",
                theme::icon_ok(),
                name.magenta(),
                format!("{latency:?}").dim()
            );
            manager.disconnect(name);
            Ok(())
        }
        Err(error) => {
            manager.disconnect(name);
            Err(connection_failure(name, &error))
        }
    }
}

#[cfg(test)]
mod mcp_cli_tests {
    use super::{
        mcp_add_at_path, mcp_json_path_for_scope, read_mcp_config, render_mcp_get, render_mcp_list,
        write_mcp_config,
    };

    #[test]
    fn diagnostic_errors_hide_transport_values() {
        let sentinel = "https://example.invalid/mcp?credential=private-fixture";
        for error in [
            astra_mcp::McpError::InvalidConfig(sentinel.into()),
            astra_mcp::McpError::Spawn(sentinel.into()),
            astra_mcp::McpError::Initialize(sentinel.into()),
            astra_mcp::McpError::ConnectionLost("fixture".into(), sentinel.into()),
        ] {
            let displayed = super::connection_failure("fixture", &error);
            assert!(
                displayed.contains("fixture")
                    && displayed.contains("Check the selected configuration")
            );
            assert!(!displayed.contains(sentinel) && !displayed.contains("private-fixture"));
        }
    }

    #[test]
    fn concurrent_config_changes_preserve_every_committed_entry() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mcp.json");
        for index in 0..8 {
            mcp_add_at_path(&format!("remove-{index}"), "fixture-command", &[], &path).unwrap();
        }
        let barrier = std::sync::Barrier::new(8);
        std::thread::scope(|scope| {
            for index in 0..8 {
                let path = &path;
                let barrier = &barrier;
                scope.spawn(move || {
                    barrier.wait();
                    mcp_add_at_path(&format!("fixture-{index}"), "fixture-command", &[], path)
                        .unwrap();
                    super::mcp_remove_at_path(&format!("remove-{index}"), path).unwrap();
                });
            }
        });
        let config = read_mcp_config(&path).unwrap();
        assert_eq!(config["mcpServers"].as_object().unwrap().len(), 8);
        for index in 0..8 {
            assert_eq!(
                config["mcpServers"][format!("fixture-{index}")]["command"],
                "fixture-command"
            );
        }
    }

    #[test]
    fn concurrent_same_name_changes_follow_serial_order() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mcp.json");
        mcp_add_at_path("fixture", "old-command", &[], &path).unwrap();
        let barrier = std::sync::Barrier::new(2);
        let outcomes = std::thread::scope(|scope| {
            let remove = || {
                barrier.wait();
                super::mcp_remove_at_path("fixture", &path)
            };
            let first = scope.spawn(remove);
            let second = scope.spawn(remove);
            [first.join().unwrap(), second.join().unwrap()]
        });
        assert_eq!(outcomes.iter().filter(|result| result.is_ok()).count(), 1);
        assert!(
            read_mcp_config(&path).unwrap()["mcpServers"]
                .get("fixture")
                .is_none()
        );

        mcp_add_at_path("fixture", "old-command", &[], &path).unwrap();
        let barrier = std::sync::Barrier::new(2);
        let added = std::thread::scope(|scope| {
            let remove = scope.spawn(|| {
                barrier.wait();
                super::mcp_remove_at_path("fixture", &path)
            });
            let add = scope.spawn(|| {
                barrier.wait();
                mcp_add_at_path("fixture", "new-command", &[], &path)
            });
            remove.join().unwrap().unwrap();
            add.join().unwrap()
        });
        let config = read_mcp_config(&path).unwrap();
        match added {
            Ok(()) => assert_eq!(config["mcpServers"]["fixture"]["command"], "new-command"),
            Err(error) => {
                assert!(error.contains("already exists"));
                assert!(config["mcpServers"].get("fixture").is_none());
            }
        }
    }

    #[test]
    fn config_locks_are_per_destination_and_release_after_errors() {
        let dir = tempfile::tempdir().unwrap();
        let first = dir.path().join("first.json");
        let second = dir.path().join("second.json");
        let held = super::lock_mcp_config(&first).unwrap();
        mcp_add_at_path("second", "fixture-command", &[], &second).unwrap();
        drop(held);
        std::fs::write(&first, "invalid json").unwrap();
        assert!(mcp_add_at_path("bad", "fixture-command", &[], &first).is_err());
        assert_eq!(std::fs::read_to_string(&first).unwrap(), "invalid json");
        make_config(&first, serde_json::json!({}));
        mcp_add_at_path("first", "fixture-command", &[], &first).unwrap();
        let before = std::fs::read(&first).unwrap();
        assert!(mcp_add_at_path("first", "duplicate", &[], &first).is_err());
        assert_eq!(std::fs::read(&first).unwrap(), before);
        mcp_add_at_path("next", "fixture-command", &[], &first).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn unsafe_lock_entries_do_not_mutate_config_or_symlink_targets() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mcp.json");
        let lock = dir.path().join("mcp.json.lock");
        make_config(&path, serde_json::json!({}));
        let before = std::fs::read(&path).unwrap();
        let target = dir.path().join("sentinel");
        std::fs::write(&target, "unchanged").unwrap();
        std::os::unix::fs::symlink(&target, &lock).unwrap();
        assert!(mcp_add_at_path("fixture", "fixture-command", &[], &path).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), before);
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "unchanged");
        std::fs::remove_file(&lock).unwrap();
        std::fs::create_dir(&lock).unwrap();
        assert!(mcp_add_at_path("fixture", "fixture-command", &[], &path).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), before);
    }

    fn make_config(path: &std::path::Path, servers: serde_json::Value) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        let config = serde_json::json!({"mcpServers": servers});
        std::fs::write(path, serde_json::to_string_pretty(&config).unwrap()).unwrap();
    }

    #[test]
    fn read_mcp_config_missing_file() {
        let config = read_mcp_config(std::path::Path::new("/tmp/nonexistent_mcp.json")).unwrap();
        assert!(config["mcpServers"].as_object().unwrap().is_empty());
    }

    #[test]
    fn read_mcp_config_valid() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), r#"{"mcpServers":{"test":{"command":"echo"}}}"#).unwrap();
        let config = read_mcp_config(tmp.path()).unwrap();
        assert!(config["mcpServers"]["test"]["command"] == "echo");
    }

    #[test]
    fn read_mcp_config_invalid_json() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), "not json").unwrap();
        let err = read_mcp_config(tmp.path()).unwrap_err();
        assert!(err.contains("Failed to parse"));
    }

    #[test]
    fn write_mcp_config_creates_parents() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("sub").join("dir").join("mcp.json");
        let config = serde_json::json!({"mcpServers": {}});
        write_mcp_config(&path, &config).unwrap();
        assert!(path.is_file());
    }

    #[test]
    fn mcp_json_path_for_scope_invalid() {
        let err = mcp_json_path_for_scope("invalid").unwrap_err();
        assert!(err.contains("Unknown scope"));
    }

    #[test]
    fn read_mcp_config_rejects_non_object_root() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), r#"["bad"]"#).unwrap();

        let err = read_mcp_config(tmp.path()).unwrap_err();
        assert!(err.contains("must be a JSON object"));
    }

    #[test]
    fn read_mcp_config_rejects_non_object_mcp_servers() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), r#"{"mcpServers":[]}"#).unwrap();

        let err = read_mcp_config(tmp.path()).unwrap_err();
        assert!(err.contains("\"mcpServers\" must be a JSON object"));
    }

    #[test]
    fn mcp_add_and_remove_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mcp.json");
        super::mcp_add_at_path("fixture", "fixture-command", &["argument".into()], &path).unwrap();
        assert_eq!(
            read_mcp_config(&path).unwrap()["mcpServers"]["fixture"]["command"],
            "fixture-command"
        );
        super::mcp_remove_at_path("fixture", &path).unwrap();
        assert!(
            read_mcp_config(&path).unwrap()["mcpServers"]
                .as_object()
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn mcp_add_json_valid() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mcp.json");
        super::mcp_add_json_at_path(
            "sse-server",
            r#"{"type":"sse","url":"https://example.invalid/mcp"}"#,
            &path,
        )
        .unwrap();
        assert_eq!(
            read_mcp_config(&path).unwrap()["mcpServers"]["sse-server"]["type"],
            "sse"
        );
    }

    #[test]
    fn mcp_add_json_invalid_json() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mcp.json");
        make_config(&path, serde_json::json!({}));
        let before = std::fs::read(&path).unwrap();
        assert!(super::mcp_add_json_at_path("fixture", "not json", &path).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), before);
        super::mcp_add_json_at_path("fixture", r#"{"command":"fixture"}"#, &path).unwrap();
    }

    #[test]
    fn mcp_add_duplicate_detection() {
        let project = tempfile::TempDir::new().unwrap();
        let path = project.path().join("mcp.json");
        make_config(&path, serde_json::json!({"existing": {"command": "echo"}}));

        let err =
            mcp_add_at_path("existing", "echo", &[], &path).expect_err("duplicate add should fail");

        assert!(err.contains("already exists"));
    }

    #[test]
    fn mcp_remove_nonexistent_server() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("scope/mcp.json");
        assert!(super::mcp_remove_at_path("missing", &path).is_err());
        assert!(!dir.path().join("scope").exists());
        make_config(&path, serde_json::json!({}));
        let before = std::fs::read(&path).unwrap();
        assert!(super::mcp_remove_at_path("missing", &path).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), before);
        mcp_add_at_path("fixture", "fixture-command", &[], &path).unwrap();
    }

    #[test]
    fn effective_and_scoped_views_are_safe_and_explicit() {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("project.json");
        let user = dir.path().join("user.json");
        let skills = dir.path().join("skills");
        std::fs::create_dir_all(skills.join("integration")).unwrap();
        let skill_path = skills.join("integration/manifest.yaml");
        std::fs::write(&skill_path, "mcp_servers:\n  - name: skill-server\n    transport:\n      type: stdio\n      command: [synthetic-private-skill-command]\n").unwrap();
        make_config(
            &user,
            serde_json::json!({
                "stdio-server": {"command": "synthetic-private-command", "args": ["--token", "synthetic-private-arg"], "env": {"API_TOKEN": "synthetic-private-env"}},
                "remote-server": {"type": "streamable_http", "url": "https://synthetic-private-user:synthetic-private-password@example.test/mcp?key=synthetic-private-query", "headers": {"Authorization": "synthetic-private-header"}, "auth_token": "synthetic-private-auth"},
                "sse-server": {"type": "sse", "url": "synthetic-private-sse-url"},
                "ws-server": {"type": "ws", "url": "synthetic-private-ws-url"},
                "disabled-server": {"command": "ignored", "disabled": true}
            }),
        );
        let scoped = crate::manifest_loader::discover_mcp_server_configs_from_paths(
            Some(&project),
            &[],
            None,
        );
        let scoped_text = render_mcp_list(&scoped, "project", false);
        assert!(scoped_text.contains("scope: project"));
        assert!(scoped_text.contains("--effective"));
        assert!(scoped_text.contains("-s user"));
        assert!(scoped_text.contains("No enabled"));
        let effective = crate::manifest_loader::discover_mcp_server_configs_from_paths(
            Some(&project),
            &[skills],
            Some(&user),
        );
        let output = render_mcp_list(&effective, "project", true);
        assert!(output.contains("effective"));
        assert!(output.contains("project > skills > user"));
        assert!(output.contains("skill-server"));
        assert!(output.contains("streamable_http"));
        assert!(output.contains(&user.display().to_string()));
        assert!(output.contains(&skill_path.display().to_string()));
        assert!(!output.contains("disabled-server"));
        assert!(!output.contains("synthetic-private"));
        for server in &effective.servers {
            let get = render_mcp_get(&server.config.name, &effective).unwrap();
            assert!(get.contains(&server.path.display().to_string()));
            assert!(get.contains(super::transport_name(&server.config.transport)));
            assert!(!get.contains("synthetic-private"));
        }
        // Redaction affects only the display; runtime still receives original settings.
        let runtime = effective.into_configs();
        let json = serde_json::to_string(&runtime).unwrap();
        for secret in [
            "command",
            "arg",
            "env",
            "header",
            "auth",
            "query",
            "skill-command",
        ] {
            assert!(json.contains(&format!("synthetic-private-{secret}")));
        }
    }

    #[cfg(unix)]
    #[test]
    fn scoped_config_reader_rejects_fifo_without_opening_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mcp.json");
        nix::unistd::mkfifo(
            &path,
            nix::sys::stat::Mode::S_IRUSR | nix::sys::stat::Mode::S_IWUSR,
        )
        .unwrap();
        assert!(
            read_mcp_config(&path)
                .unwrap_err()
                .contains("expected a regular file")
        );
        assert!(
            super::mcp_remove_at_path("missing", &path)
                .unwrap_err()
                .contains("regular file")
        );
        assert!(!dir.path().join("mcp.json.lock").exists());
        assert!(
            super::mcp_remove_at_path("missing", dir.path())
                .unwrap_err()
                .contains("regular file")
        );
    }

    #[test]
    fn malformed_config_warnings_are_visible_without_echoing_rejected_values() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mcp.json");
        std::fs::write(
            &path,
            r#"{"mcpServers":{"bad":{"type":"synthetic-private-type","env":{"TOKEN":4}}}}"#,
        )
        .unwrap();
        let discovery =
            crate::manifest_loader::discover_mcp_server_configs_from_paths(Some(&path), &[], None);
        let output = render_mcp_list(&discovery, "project", false);
        assert!(output.contains("Warning: Failed to parse"));
        assert!(output.contains("resolve the configuration warnings"));
        assert!(!output.contains("synthetic-private"));
        let error = render_mcp_get("missing", &discovery).unwrap_err();
        assert!(error.contains("Failed to parse"));
        assert!(!error.contains("synthetic-private"));
        // Invalid JSON imports also redact inline sources and parser fragments.
        let error = super::parse_mcp_config_source(
            r#"{"mcpServers": "synthetic-private-inline""#,
            "MCP config source 1",
        )
        .unwrap_err();
        assert!(error.contains("MCP config source 1"));
        assert!(!error.contains("synthetic-private"));
        assert!(
            read_mcp_config(dir.path())
                .unwrap_err()
                .contains("Failed to read")
        );
    }

    #[test]
    fn mcp_get_searches_both_scopes() {
        let project = tempfile::TempDir::new().unwrap();
        let home = tempfile::TempDir::new().unwrap();
        let project_path = project.path().join("mcp.json");
        let home_path = home.path().join("mcp.json");
        make_config(&project_path, serde_json::json!({}));
        make_config(
            &home_path,
            serde_json::json!({"user-server": {"command": "echo"}}),
        );

        let discovery = crate::manifest_loader::discover_mcp_server_configs_from_paths(
            Some(&project_path),
            &[],
            Some(&home_path),
        );
        let output = render_mcp_get("user-server", &discovery).unwrap();
        assert!(output.contains("Scope: user"));
        assert!(output.contains(&home_path.display().to_string()));
    }

    #[test]
    fn diagnostics_preserve_canonical_fields_and_use_one_attempt() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mcp.json");
        let secret = "synthetic-private-value";
        make_config(
            &path,
            serde_json::json!({
                "stdio": {"command":"fixture-command", "args":["--token", secret], "env":{"TOKEN":secret}},
                "sse": {"type":"sse", "url":secret, "headers":{"Authorization":secret}, "auth_token":secret},
                "streamable_http": {"type":"streamable_http", "url":secret, "headers":{"Authorization":secret}, "auth_token":secret},
                "ws": {"type":"ws", "url":secret, "headers":{"Authorization":secret}, "auth_token":secret}
            }),
        );
        for name in ["stdio", "sse", "streamable_http", "ws"] {
            let config = super::diagnostic_mcp_config(name, &path).unwrap();
            assert_eq!(config.name, name);
            assert!(config.enabled);
            assert_eq!(config.retry.max_retries, 0);
            let defaults = astra_mcp::RetryConfig::default();
            assert_eq!(config.retry.initial_delay_ms, defaults.initial_delay_ms);
            assert_eq!(config.retry.max_delay_ms, defaults.max_delay_ms);
            let transport = serde_json::to_value(config.transport).unwrap();
            let expected = if name == "stdio" {
                serde_json::json!({"type":"stdio", "command":["fixture-command", "--token", secret], "args":[], "env":{"TOKEN":secret}})
            } else {
                serde_json::json!({"type":name, "url":secret, "headers":{"Authorization":secret}, "auth_token":secret})
            };
            assert_eq!(transport, expected);
        }
        let discovery =
            crate::manifest_loader::discover_mcp_server_configs_from_paths(Some(&path), &[], None);
        assert!(
            discovery
                .servers
                .iter()
                .all(|server| server.config.retry.max_retries
                    == astra_mcp::RetryConfig::default().max_retries)
        );
    }

    #[test]
    fn diagnostics_reject_configuration_chat_cannot_enable() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mcp.json");
        let secret = "synthetic-private-value";
        assert!(super::diagnostic_mcp_config("fixture", &path).is_err());
        for entry in [
            serde_json::json!({"command":secret, "args":[7, secret]}),
            serde_json::json!({"command":secret, "env":{"TOKEN":7}}),
            serde_json::json!({"type":"streamable_http", "url":secret, "headers":{"Authorization":7}}),
            serde_json::json!({"command":secret, "disabled":true}),
            serde_json::json!({"type":"http", "url":secret}),
            serde_json::json!({"type":"streamable-http", "url":secret}),
            serde_json::json!({"type":"websocket", "url":secret}),
            serde_json::json!({"type":secret}),
            serde_json::json!({"type":7, "command":secret}),
            serde_json::json!({"type":"sse", "auth_token":secret}),
            serde_json::json!({}),
        ] {
            make_config(&path, serde_json::json!({"fixture":entry}));
            let error = super::diagnostic_mcp_config("fixture", &path).unwrap_err();
            assert!(error.contains("No enabled MCP server"), "{error}");
            assert!(!error.contains(secret), "{error}");
        }
        // A malformed sibling invalidates the same file for chat and diagnostics.
        make_config(
            &path,
            serde_json::json!({
                "fixture":{"command":"fixture-command"},
                "broken":{"command":secret,"args":[7]}
            }),
        );
        let error = super::diagnostic_mcp_config("fixture", &path).unwrap_err();
        assert!(error.contains("Failed to parse"), "{error}");
        assert!(!error.contains(secret), "{error}");
    }

    #[test]
    fn write_mcp_config_atomic_no_partial() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("mcp.json");
        let config = serde_json::json!({"mcpServers": {"s": {"command": "echo"}}});
        write_mcp_config(&path, &config).unwrap();

        // tmp file should not remain
        let tmp = path.with_extension("json.tmp");
        assert!(!tmp.exists());

        // written file should be valid JSON
        let content = std::fs::read_to_string(&path).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&content).unwrap();
        assert_eq!(parsed["mcpServers"]["s"]["command"], "echo");
    }

    #[test]
    fn load_mcp_configs_from_file() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("source.json");
        let path = dir.path().join("mcp.json");
        std::fs::write(
            &source,
            r#"{"mcpServers":{"fixture":{"command":"fixture-command"}}}"#,
        )
        .unwrap();
        super::load_mcp_configs_at_path(&[source.to_string_lossy().into_owned()], &path).unwrap();
        assert_eq!(
            read_mcp_config(&path).unwrap()["mcpServers"]["fixture"]["command"],
            "fixture-command"
        );
    }

    #[test]
    fn load_mcp_configs_rejects_missing_mcp_servers_key() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mcp.json");
        make_config(&path, serde_json::json!({"keep":{"command":"keep"}}));
        let before = std::fs::read(&path).unwrap();
        assert!(
            super::load_mcp_configs_at_path(&[r#"{"servers":{"fixture":{}}}"#.into()], &path)
                .is_err()
        );
        assert_eq!(std::fs::read(&path).unwrap(), before);
        super::load_mcp_configs_at_path(
            &[r#"{"mcpServers":{"fixture":{"command":"fixture-command"}}}"#.into()],
            &path,
        )
        .unwrap();
        assert_eq!(
            read_mcp_config(&path).unwrap()["mcpServers"]
                .as_object()
                .unwrap()
                .len(),
            2
        );
    }
}
