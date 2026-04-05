use super::*;
use crate::mcp_client::{ConnectionState, McpClientManager};

pub(super) async fn handle_mcp_command(
    arg: &str,
    state: &ReplState,
) -> Result<(), String> {
    let sub = arg.trim();

    match sub {
        "" | "status" => show_status(state).await,
        "servers" => show_servers(state).await,
        _ => {
            eprintln!(
                "{}",
                format!("  Unknown /mcp subcommand: '{sub}'. Try /mcp, /mcp status, /mcp servers").yellow()
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
        eprintln!("{}", "  Configure servers in manifest.yaml or .astra/mcp.yaml".dim());
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
            let state = manager.server_state(name).unwrap_or(ConnectionState::Connected);
            let uptime = conn.uptime()
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
            let state = manager.server_state(name).unwrap_or(ConnectionState::Connected);
            let tool_count = conn.tools().len();
            let uptime = conn.uptime()
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
        assert_eq!(format_duration(std::time::Duration::from_secs(125)), "2m 5s");
    }

    #[test]
    fn format_duration_hours() {
        assert_eq!(format_duration(std::time::Duration::from_secs(7500)), "2h 5m");
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
