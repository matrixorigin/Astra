//! MCP JSON configuration management (read, write, add, remove, list, get).
//!
//! Extracted from `command_router.rs` as part of the god-module refactor (P0-2).

use super::theme;
use crate::cli::cli_config::cli_args::McpCmd;
use crossterm::style::Stylize;
use std::io::Write;

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
    let mut config = read_mcp_config(&project_path)?;

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

    write_mcp_config(&project_path, &config)?;
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
    let mut config = read_mcp_config(&path)?;

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

    write_mcp_config(&path, &config)?;
    stdout_println!(
        "  {} Added '{}' to {}",
        theme::icon_ok(),
        name.magenta(),
        path.display().to_string().dim()
    );
    Ok(())
}

fn mcp_add_json(name: &str, json: &str, scope: &str) -> Result<(), String> {
    let entry: serde_json::Value =
        serde_json::from_str(json).map_err(|e| json_parse_error("MCP server JSON", &e))?;
    if !entry.is_object() {
        return Err("JSON config must be an object".to_string());
    }

    let path = mcp_json_path_for_scope(scope)?;
    let mut config = read_mcp_config(&path)?;

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

    write_mcp_config(&path, &config)?;
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
    if !path.is_file() {
        return Err(format!("No config file at {}", path.display()));
    }
    let mut config = read_mcp_config(&path)?;

    let removed = config
        .get_mut("mcpServers")
        .and_then(|v| v.as_object_mut())
        .map(|m| m.remove(name).is_some())
        .unwrap_or(false);

    if !removed {
        return Err(format!("Server '{name}' not found in {}", path.display()));
    }

    write_mcp_config(&path, &config)?;
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

fn find_server_entry<'a>(
    config: &'a serde_json::Value,
    name: &str,
) -> Option<&'a serde_json::Map<String, serde_json::Value>> {
    config
        .get("mcpServers")
        .and_then(|v| v.as_object())
        .and_then(|m| m.get(name))
        .and_then(|v| v.as_object())
}

fn json_entry_to_mcp_config(
    name: &str,
    entry: &serde_json::Map<String, serde_json::Value>,
) -> Result<astra_mcp::McpServerConfig, String> {
    let server_type = entry
        .get("type")
        .and_then(|v| v.as_str())
        .unwrap_or("stdio");

    let transport = match server_type {
        "sse" => {
            let url = entry
                .get("url")
                .and_then(|v| v.as_str())
                .ok_or_else(|| format!("SSE server '{name}' missing `url`"))?;
            astra_mcp::Transport::Sse {
                url: url.to_string(),
                auth_token: entry
                    .get("auth_token")
                    .and_then(|v| v.as_str())
                    .map(String::from),
                headers: string_map_from_json_object(entry.get("headers")),
            }
        }
        "http" | "streamable_http" | "streamable-http" => {
            let url = entry
                .get("url")
                .and_then(|v| v.as_str())
                .ok_or_else(|| format!("HTTP MCP server '{name}' missing `url`"))?;
            astra_mcp::Transport::StreamableHttp {
                url: url.to_string(),
                auth_token: entry
                    .get("auth_token")
                    .and_then(|v| v.as_str())
                    .map(String::from),
                headers: string_map_from_json_object(entry.get("headers")),
            }
        }
        "ws" | "websocket" => {
            let url = entry
                .get("url")
                .and_then(|v| v.as_str())
                .ok_or_else(|| format!("WS server '{name}' missing `url`"))?;
            astra_mcp::Transport::Ws {
                url: url.to_string(),
                auth_token: entry
                    .get("auth_token")
                    .and_then(|v| v.as_str())
                    .map(String::from),
                headers: string_map_from_json_object(entry.get("headers")),
            }
        }
        "stdio" | "" => {
            let command = entry
                .get("command")
                .and_then(|v| v.as_str())
                .ok_or_else(|| format!("Stdio server '{name}' missing `command`"))?;
            let mut command_vec = vec![command.to_string()];
            if let Some(args) = entry.get("args").and_then(|v| v.as_array()) {
                command_vec.extend(args.iter().filter_map(|v| v.as_str()).map(String::from));
            }
            astra_mcp::Transport::Stdio {
                command: command_vec,
                args: Vec::new(),
                env: string_map_from_json_object(entry.get("env")),
            }
        }
        _ => return Err(format!("Unsupported MCP server type for '{name}'")),
    };

    Ok(astra_mcp::McpServerConfig {
        name: name.to_string(),
        transport,
        description: String::new(),
        enabled: true,
        retry: Default::default(),
    })
}

fn string_map_from_json_object(
    value: Option<&serde_json::Value>,
) -> std::collections::HashMap<String, String> {
    value
        .and_then(|v| v.as_object())
        .map(|object| {
            object
                .iter()
                .map(|(key, value)| (key.clone(), value.as_str().unwrap_or("").to_string()))
                .collect()
        })
        .unwrap_or_default()
}

async fn mcp_test(name: &str, scope: &str) -> Result<(), String> {
    let path = mcp_json_path_for_scope(scope)?;
    let config = read_mcp_config(&path)?;
    let entry = find_server_entry(&config, name)
        .ok_or_else(|| format!("Server '{name}' not found in {}", path.display()))?;
    let mcp_config = json_entry_to_mcp_config(name, entry)?;

    eprintln!("  {} Connecting to {}...", theme::icon_ok(), name.magenta());

    let mut manager = astra_mcp::McpClientManager::new();
    let tool_count = manager
        .connect(mcp_config)
        .await
        .map_err(|e| format!("Failed to connect to '{name}': {e}"))?;

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
    let config = read_mcp_config(&path)?;
    let entry = find_server_entry(&config, name)
        .ok_or_else(|| format!("Server '{name}' not found in {}", path.display()))?;
    let mcp_config = json_entry_to_mcp_config(name, entry)?;

    let mut manager = astra_mcp::McpClientManager::new();
    manager
        .connect(mcp_config)
        .await
        .map_err(|e| format!("Failed to connect to '{name}': {e}"))?;

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
            Err(format!("Ping failed for '{name}': {error}"))
        }
    }
}

#[cfg(test)]
mod mcp_cli_tests {
    use super::{
        mcp_add_at_path, mcp_json_path_for_scope, read_mcp_config, render_mcp_get, render_mcp_list,
        write_mcp_config,
    };

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
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("mcp.json");

        // Start empty
        make_config(&path, serde_json::json!({}));

        // Add a server
        let mut config = read_mcp_config(&path).unwrap();
        let entry = serde_json::json!({"command": "npx", "args": ["@mcp/server"]});
        config["mcpServers"]
            .as_object_mut()
            .unwrap()
            .insert("test-server".to_string(), entry);
        write_mcp_config(&path, &config).unwrap();

        // Verify it's there
        let config = read_mcp_config(&path).unwrap();
        assert!(config["mcpServers"]["test-server"]["command"] == "npx");

        // Remove it
        let mut config = read_mcp_config(&path).unwrap();
        let removed = config["mcpServers"]
            .as_object_mut()
            .unwrap()
            .remove("test-server")
            .is_some();
        assert!(removed);
        write_mcp_config(&path, &config).unwrap();

        // Verify it's gone
        let config = read_mcp_config(&path).unwrap();
        assert!(
            !config["mcpServers"]
                .as_object()
                .unwrap()
                .contains_key("test-server")
        );
    }

    #[test]
    fn mcp_add_json_valid() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("mcp.json");
        make_config(&path, serde_json::json!({}));

        let entry: serde_json::Value =
            serde_json::from_str(r#"{"url":"http://localhost:3000","type":"sse"}"#).unwrap();
        let mut config = read_mcp_config(&path).unwrap();
        config["mcpServers"]
            .as_object_mut()
            .unwrap()
            .insert("sse-server".to_string(), entry);
        write_mcp_config(&path, &config).unwrap();

        let config = read_mcp_config(&path).unwrap();
        assert_eq!(config["mcpServers"]["sse-server"]["type"], "sse");
        assert_eq!(
            config["mcpServers"]["sse-server"]["url"],
            "http://localhost:3000"
        );
    }

    #[test]
    fn mcp_add_json_invalid_json() {
        let err: Result<serde_json::Value, _> = serde_json::from_str("not json");
        assert!(err.is_err());
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
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("mcp.json");
        make_config(&path, serde_json::json!({}));

        let mut config = read_mcp_config(&path).unwrap();
        let removed = config["mcpServers"]
            .as_object_mut()
            .unwrap()
            .remove("ghost")
            .is_some();
        assert!(!removed);
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
    fn mcp_list_empty_config() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("mcp.json");
        make_config(&path, serde_json::json!({}));

        let config = read_mcp_config(&path).unwrap();
        let servers = config["mcpServers"].as_object().unwrap();
        assert!(servers.is_empty());
    }

    #[test]
    fn mcp_list_multiple_server_types() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("mcp.json");
        make_config(
            &path,
            serde_json::json!({
                "stdio-srv": {"command": "npx", "args": ["@mcp/server"]},
                "sse-srv": {"type": "sse", "url": "http://localhost:3000"},
                "http-srv": {"type": "streamable_http", "url": "http://localhost:4000"}
            }),
        );

        let config = read_mcp_config(&path).unwrap();
        let servers = config["mcpServers"].as_object().unwrap();
        assert_eq!(servers.len(), 3);

        // stdio type inference
        let stdio = &servers["stdio-srv"];
        assert_eq!(
            stdio
                .get("type")
                .and_then(|v| v.as_str())
                .unwrap_or("stdio"),
            "stdio"
        );

        // sse type
        assert_eq!(servers["sse-srv"]["type"], "sse");

        // http type
        assert_eq!(servers["http-srv"]["type"], "streamable_http");
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
        let dir = tempfile::TempDir::new().unwrap();
        let config_file = dir.path().join("custom-mcp.json");
        std::fs::write(
            &config_file,
            r#"{"mcpServers":{"test-server":{"command":"echo","args":["hello"]}}}"#,
        )
        .unwrap();

        let json_str = std::fs::read_to_string(&config_file).unwrap();
        let parsed = super::parse_mcp_config_source(&json_str, "test file").unwrap();
        let servers = parsed
            .get("mcpServers")
            .and_then(|v| v.as_object())
            .unwrap();
        assert!(servers.contains_key("test-server"));
        assert_eq!(servers["test-server"]["command"], "echo");
        assert_eq!(servers["test-server"]["args"][0], "hello");
    }

    #[test]
    fn load_mcp_configs_rejects_missing_mcp_servers_key() {
        let json_str = r#"{"servers":{"foo":{}}}"#;
        let error = super::parse_mcp_config_source(json_str, "test source").unwrap_err();
        assert!(error.contains("must contain a \"mcpServers\" object"));
    }
}
