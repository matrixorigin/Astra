use super::*;
use crate::manifest_loader::project_mcp_json_path;
use crate::mcp_client::{ConnectionState, McpClientManager};

pub(super) async fn handle_mcp_command(arg: &str, state: &ReplState) -> Result<(), String> {
    let sub = arg.trim();

    match sub {
        "" | "status" => show_status(state).await,
        "servers" => show_servers(state).await,
        "prompts" => show_prompts(state).await,
        s if s.starts_with("add ") => handle_mcp_add(&s[4..]).await,
        "add" => {
            eprintln!(
                "{}",
                "  Usage: /mcp add <name> <command> [args...]\n  Example: /mcp add github npx @modelcontextprotocol/server-github".dim()
            );
        }
        s if s.starts_with("remove ") => handle_mcp_remove(&s[7..]).await,
        "remove" => {
            eprintln!("{}", "  Usage: /mcp remove <name>".dim());
        }
        _ => {
            eprintln!(
                "{}",
                format!("  Unknown /mcp subcommand: '{sub}'. Try /mcp, /mcp add, /mcp remove, /mcp servers, /mcp prompts")
                    .yellow()
            );
        }
    }

    Ok(())
}

async fn show_status(state: &ReplState) {
    let manager = state.mcp_manager.read().await;
    let count = manager.connection_count();

    if count == 0 {
        eprintln!("{}", "  No MCP servers connected.".dim());
        eprintln!(
            "{}",
            "  Configure servers in .astra/mcp.json or skill manifest.yaml".dim()
        );
        return;
    }

    eprintln!("{}", format!("  MCP Servers: {count} connected").bold());
    eprintln!();

    print_server_table(&manager);
}

async fn show_servers(state: &ReplState) {
    let manager = state.mcp_manager.read().await;
    let servers = manager.connected_servers();

    if servers.is_empty() {
        eprintln!("{}", "  No MCP servers connected.".dim());
        return;
    }

    for name in &servers {
        if let Some(conn) = manager.get(name) {
            let tools = conn.tools();
            let state = manager
                .server_state(name)
                .unwrap_or(ConnectionState::Connected);
            let uptime = conn
                .uptime()
                .map(|d| format_duration(d))
                .unwrap_or_else(|| "n/a".to_string());

            eprintln!("{}", format!("  ┌─ {name}").bold());
            eprintln!("  │ State:   {}", format_state(state));
            eprintln!("  │ Uptime:  {uptime}");
            eprintln!("  │ Tools:   {}", tools.len());

            if !tools.is_empty() {
                for tool in tools.iter().take(10) {
                    let desc = tool.description.as_deref().unwrap_or("");
                    let short_desc = if desc.len() > 60 {
                        format!("{}…", &desc[..desc.floor_char_boundary(60)])
                    } else {
                        desc.to_string()
                    };
                    eprintln!("  │   {} {}", tool.name, short_desc.dim());
                }
                if tools.len() > 10 {
                    eprintln!("  │   … and {} more", tools.len() - 10);
                }
            }
            eprintln!("  └─");
        }
    }
}

async fn show_prompts(state: &ReplState) {
    let manager = state.mcp_manager.read().await;

    if manager.connection_count() == 0 {
        eprintln!("{}", "  No MCP servers connected.".dim());
        return;
    }

    let prompts = manager.all_prompts().await;

    if prompts.is_empty() {
        eprintln!("{}", "  No prompts available from connected MCP servers.".dim());
        return;
    }

    eprintln!(
        "{}",
        format!("  MCP Prompts: {} available", prompts.len()).bold()
    );
    eprintln!();

    for (server, prompt) in &prompts {
        let desc = prompt
            .description
            .as_deref()
            .unwrap_or("")
            .chars()
            .take(60)
            .collect::<String>();
        let args = prompt
            .arguments
            .as_ref()
            .map(|a| {
                a.iter()
                    .map(|arg| {
                        if arg.required.unwrap_or(false) {
                            format!("<{}>", arg.name)
                        } else {
                            format!("[{}]", arg.name)
                        }
                    })
                    .collect::<Vec<_>>()
                    .join(" ")
            })
            .unwrap_or_default();

        eprintln!(
            "  {} {} {}",
            format!("{server}:{}", prompt.name).bold(),
            args.dim(),
            desc.dim(),
        );
    }
}

/// `/mcp add <name> <command> [args...]` — add a stdio MCP server to project config.
async fn handle_mcp_add(arg: &str) {
    let parts: Vec<&str> = arg.split_whitespace().collect();
    if parts.len() < 2 {
        eprintln!("{}", "  Usage: /mcp add <name> <command> [args...]".dim());
        eprintln!(
            "{}",
            "  Example: /mcp add github npx @modelcontextprotocol/server-github".dim()
        );
        return;
    }
    let name = parts[0];
    let command = parts[1];
    let args: Vec<&str> = parts[2..].to_vec();

    let path = match project_mcp_json_path() {
        Some(p) => p,
        None => {
            eprintln!("{}", "  ⚠ Cannot determine project directory.".yellow());
            return;
        }
    };

    // Load existing or create new config
    let mut config: serde_json::Value = if path.is_file() {
        match std::fs::read_to_string(&path) {
            Ok(content) => serde_json::from_str(&content)
                .unwrap_or_else(|_| serde_json::json!({"mcpServers": {}})),
            Err(_) => serde_json::json!({"mcpServers": {}}),
        }
    } else {
        serde_json::json!({"mcpServers": {}})
    };

    // Check for duplicate
    if let Some(servers) = config.get("mcpServers").and_then(|v| v.as_object()) {
        if servers.contains_key(name) {
            eprintln!("{}", format!("  ⚠ Server '{name}' already exists in .astra/mcp.json. Remove first with /mcp remove {name}").yellow());
            return;
        }
    }

    // Add new server entry
    let entry = serde_json::json!({
        "command": command,
        "args": args,
    });
    config
        .as_object_mut()
        .unwrap()
        .entry("mcpServers")
        .or_insert_with(|| serde_json::json!({}))
        .as_object_mut()
        .unwrap()
        .insert(name.to_string(), entry);

    // Write atomically (temp + rename)
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let tmp = path.with_extension("json.tmp");
    let pretty = serde_json::to_string_pretty(&config).unwrap_or_default();
    match std::fs::write(&tmp, &pretty) {
        Ok(()) => {
            if let Err(e) = std::fs::rename(&tmp, &path) {
                eprintln!(
                    "{}",
                    format!("  ⚠ Failed to write {}: {e}", path.display()).yellow()
                );
                let _ = std::fs::remove_file(&tmp);
                return;
            }
        }
        Err(e) => {
            eprintln!(
                "{}",
                format!("  ⚠ Failed to write {}: {e}", path.display()).yellow()
            );
            return;
        }
    }

    eprintln!(
        "{}",
        format!("  ✓ Added '{name}' to {}", path.display()).green()
    );
    eprintln!(
        "{}",
        "  Restart the session or reconnect to activate.".dim()
    );
}

/// `/mcp remove <name>` — remove an MCP server from project config.
async fn handle_mcp_remove(arg: &str) {
    let name = arg.trim();
    if name.is_empty() {
        eprintln!("{}", "  Usage: /mcp remove <name>".dim());
        return;
    }

    let path = match project_mcp_json_path() {
        Some(p) => p,
        None => {
            eprintln!("{}", "  ⚠ Cannot determine project directory.".yellow());
            return;
        }
    };

    if !path.is_file() {
        eprintln!(
            "{}",
            format!("  ⚠ No .astra/mcp.json found at {}", path.display()).yellow()
        );
        return;
    }

    let content = match std::fs::read_to_string(&path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!(
                "{}",
                format!("  ⚠ Failed to read {}: {e}", path.display()).yellow()
            );
            return;
        }
    };
    let mut config: serde_json::Value = match serde_json::from_str(&content) {
        Ok(c) => c,
        Err(e) => {
            eprintln!(
                "{}",
                format!("  ⚠ Failed to parse {}: {e}", path.display()).yellow()
            );
            return;
        }
    };

    let removed = config
        .get_mut("mcpServers")
        .and_then(|v| v.as_object_mut())
        .map(|m| m.remove(name).is_some())
        .unwrap_or(false);

    if !removed {
        eprintln!(
            "{}",
            format!("  ⚠ Server '{name}' not found in {}", path.display()).yellow()
        );
        return;
    }

    // Write back atomically
    let tmp = path.with_extension("json.tmp");
    let pretty = serde_json::to_string_pretty(&config).unwrap_or_default();
    match std::fs::write(&tmp, &pretty) {
        Ok(()) => {
            if let Err(e) = std::fs::rename(&tmp, &path) {
                eprintln!(
                    "{}",
                    format!("  ⚠ Failed to write {}: {e}", path.display()).yellow()
                );
                let _ = std::fs::remove_file(&tmp);
                return;
            }
        }
        Err(e) => {
            eprintln!(
                "{}",
                format!("  ⚠ Failed to write {}: {e}", path.display()).yellow()
            );
            return;
        }
    }

    eprintln!(
        "{}",
        format!("  ✓ Removed '{name}' from {}", path.display()).green()
    );
}

fn print_server_table(manager: &McpClientManager) {
    // Header
    eprintln!(
        "  {:<20} {:<12} {:<8} {:<10}",
        "Server", "State", "Tools", "Uptime"
    );
    eprintln!("  {}", "─".repeat(52));

    let mut servers = manager.connected_servers();
    servers.sort();

    for name in &servers {
        if let Some(conn) = manager.get(name) {
            let state = manager
                .server_state(name)
                .unwrap_or(ConnectionState::Connected);
            let tool_count = conn.tools().len();
            let uptime = conn
                .uptime()
                .map(|d| format_duration(d))
                .unwrap_or_else(|| "—".to_string());

            let display_name = if name.len() > 20 {
                format!("{}…", &name[..name.floor_char_boundary(19)])
            } else {
                name.to_string()
            };

            eprintln!(
                "  {:<20} {:<12} {:<8} {:<10}",
                display_name,
                format_state(state),
                tool_count,
                uptime,
            );
        }
    }
}

fn format_state(state: ConnectionState) -> String {
    match state {
        ConnectionState::Connected => "✓ connected".green().to_string(),
        ConnectionState::Connecting => "⟳ connecting".yellow().to_string(),
        ConnectionState::Reconnecting => "↻ reconnecting".yellow().to_string(),
        ConnectionState::Disconnected => "✗ disconnected".red().to_string(),
        ConnectionState::Failed => "✗ failed".red().to_string(),
    }
}

fn format_duration(d: std::time::Duration) -> String {
    let secs = d.as_secs();
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m {}s", secs / 60, secs % 60)
    } else {
        format!("{}h {}m", secs / 3600, (secs % 3600) / 60)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_duration_seconds() {
        assert_eq!(format_duration(std::time::Duration::from_secs(42)), "42s");
    }

    #[test]
    fn format_duration_minutes() {
        assert_eq!(
            format_duration(std::time::Duration::from_secs(125)),
            "2m 5s"
        );
    }

    #[test]
    fn format_duration_hours() {
        assert_eq!(
            format_duration(std::time::Duration::from_secs(7500)),
            "2h 5m"
        );
    }

    #[test]
    fn format_state_all_variants() {
        // Just verify no panics
        let _ = format_state(ConnectionState::Connected);
        let _ = format_state(ConnectionState::Connecting);
        let _ = format_state(ConnectionState::Reconnecting);
        let _ = format_state(ConnectionState::Disconnected);
        let _ = format_state(ConnectionState::Failed);
    }
}
