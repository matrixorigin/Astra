use super::*;
use crate::manifest_loader::project_mcp_json_path;
use crate::mcp_client::{ConnectionState, McpClientManager};

pub(super) async fn handle_mcp_command(arg: &str, state: &ReplState) -> Result<(), String> {
    let sub = arg.trim();

    match sub {
        "" | "status" => show_status(state).await,
        "servers" => show_servers(state).await,
        "prompts" => show_prompts(state).await,
        "resources" => show_resources(state).await,
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
        s if s.starts_with("resource ") => handle_mcp_resource_read(&s[9..], state).await,
        "resource" => {
            eprintln!("{}", "  Usage: /mcp resource <server>:<uri>".dim());
        }
        s if s.starts_with("subscribe ") => handle_mcp_subscribe(&s[10..], state).await,
        "subscribe" => {
            eprintln!("{}", "  Usage: /mcp subscribe <server>:<uri>".dim());
        }
        s if s.starts_with("unsubscribe ") => handle_mcp_unsubscribe(&s[12..], state).await,
        "unsubscribe" => {
            eprintln!("{}", "  Usage: /mcp unsubscribe <server>:<uri>".dim());
        }
        s if s.starts_with("log-level ") => handle_mcp_log_level(&s[10..], state).await,
        "log-level" => {
            eprintln!("{}", "  Usage: /mcp log-level <server> <level>".dim());
            eprintln!("{}", "  Levels: debug, info, notice, warning, error, critical, alert, emergency".dim());
        }
        s if s.starts_with("prompt ") => {
            eprintln!("{}", "  Hint: use /mcp prompt <server>:<name> [arg1 arg2 ...]".dim());
        }
        "prompt" => {
            eprintln!("{}", "  Usage: /mcp prompt <server>:<name> [arg1 arg2 ...]".dim());
        }
        _ => {
            eprintln!(
                "{}",
                format!("  Unknown /mcp subcommand: '{sub}'. Try /mcp, /mcp add, /mcp remove, /mcp servers, /mcp prompts, /mcp resources, /mcp prompt, /mcp resource")
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

async fn show_resources(state: &ReplState) {
    let manager = state.mcp_manager.read().await;

    if manager.connection_count() == 0 {
        eprintln!("{}", "  No MCP servers connected.".dim());
        return;
    }

    let resources = manager.all_resources().await;

    if resources.is_empty() {
        eprintln!(
            "{}",
            "  No resources available from connected MCP servers.".dim()
        );
        return;
    }

    eprintln!(
        "{}",
        format!("  MCP Resources: {} available", resources.len()).bold()
    );
    eprintln!();

    for (server, resource) in &resources {
        let desc = resource
            .description
            .as_deref()
            .unwrap_or("")
            .chars()
            .take(60)
            .collect::<String>();
        let mime = resource
            .raw
            .mime_type
            .as_deref()
            .map(|m| format!("[{m}]"))
            .unwrap_or_default();

        eprintln!(
            "  {} {} {}",
            format!("{server}:{}", resource.raw.uri).bold(),
            mime.dim(),
            desc.dim(),
        );
    }
}

/// `/mcp resource <server>:<uri>` — read an MCP resource by URI.
async fn handle_mcp_resource_read(arg: &str, state: &ReplState) {
    let rest = arg.trim();
    if rest.is_empty() {
        eprintln!("{}", "  Usage: /mcp resource <server>:<uri>".dim());
        eprintln!(
            "{}",
            "  Example: /mcp resource github:file:///README.md".dim()
        );
        return;
    }

    // Parse server:uri (split on first colon only)
    let (server_name, uri) = match rest.split_once(':') {
        Some((s, u)) if !s.is_empty() && !u.is_empty() => (s, u),
        _ => {
            eprintln!(
                "{}",
                format!("  ⚠ Invalid format: '{rest}'. Use <server>:<uri>").yellow()
            );
            return;
        }
    };

    let manager = state.mcp_manager.read().await;
    let conn = match manager.get(server_name) {
        Some(c) => c,
        None => {
            eprintln!(
                "{}",
                format!("  ⚠ Server '{server_name}' not found.").yellow()
            );
            return;
        }
    };

    match conn.read_resource(uri).await {
        Ok(content) => {
            if content.is_empty() {
                eprintln!("{}", "  (empty resource)".dim());
            } else {
                eprintln!("{content}");
            }
        }
        Err(e) => {
            eprintln!(
                "{}",
                format!("  ⚠ Failed to read resource '{uri}' from {server_name}: {e}").yellow()
            );
        }
    }
}

/// `/mcp subscribe <server>:<uri>` — subscribe to resource updates.
async fn handle_mcp_subscribe(arg: &str, state: &ReplState) {
    let rest = arg.trim();
    if rest.is_empty() {
        eprintln!("{}", "  Usage: /mcp subscribe <server>:<uri>".dim());
        return;
    }

    let (server_name, uri) = match rest.split_once(':') {
        Some((s, u)) if !s.is_empty() && !u.is_empty() => (s, u),
        _ => {
            eprintln!(
                "{}",
                format!("  ⚠ Invalid format: '{rest}'. Use <server>:<uri>").yellow()
            );
            return;
        }
    };

    let manager = state.mcp_manager.read().await;
    let conn = match manager.get(server_name) {
        Some(c) => c,
        None => {
            eprintln!(
                "{}",
                format!("  ⚠ Server '{server_name}' not found.").yellow()
            );
            return;
        }
    };

    match conn.subscribe_resource(uri).await {
        Ok(()) => {
            eprintln!(
                "{}",
                format!("  ✓ Subscribed to '{uri}' on {server_name}").green()
            );
        }
        Err(e) => {
            eprintln!(
                "{}",
                format!("  ⚠ Failed to subscribe to '{uri}' on {server_name}: {e}").yellow()
            );
        }
    }
}

/// `/mcp unsubscribe <server>:<uri>` — unsubscribe from resource updates.
async fn handle_mcp_unsubscribe(arg: &str, state: &ReplState) {
    let rest = arg.trim();
    if rest.is_empty() {
        eprintln!("{}", "  Usage: /mcp unsubscribe <server>:<uri>".dim());
        return;
    }

    let (server_name, uri) = match rest.split_once(':') {
        Some((s, u)) if !s.is_empty() && !u.is_empty() => (s, u),
        _ => {
            eprintln!(
                "{}",
                format!("  ⚠ Invalid format: '{rest}'. Use <server>:<uri>").yellow()
            );
            return;
        }
    };

    let manager = state.mcp_manager.read().await;
    let conn = match manager.get(server_name) {
        Some(c) => c,
        None => {
            eprintln!(
                "{}",
                format!("  ⚠ Server '{server_name}' not found.").yellow()
            );
            return;
        }
    };

    match conn.unsubscribe_resource(uri).await {
        Ok(()) => {
            eprintln!(
                "{}",
                format!("  ✓ Unsubscribed from '{uri}' on {server_name}").green()
            );
        }
        Err(e) => {
            eprintln!(
                "{}",
                format!("  ⚠ Failed to unsubscribe from '{uri}' on {server_name}: {e}").yellow()
            );
        }
    }
}

/// `/mcp log-level <server> <level>` — set logging level for an MCP server.
async fn handle_mcp_log_level(arg: &str, state: &ReplState) {
    let parts: Vec<&str> = arg.trim().split_whitespace().collect();
    if parts.len() != 2 {
        eprintln!("{}", "  Usage: /mcp log-level <server> <level>".dim());
        eprintln!(
            "{}",
            "  Levels: debug, info, notice, warning, error, critical, alert, emergency".dim()
        );
        return;
    }

    let server_name = parts[0];
    let level = match parts[1].to_lowercase().as_str() {
        "debug" => rmcp::model::LoggingLevel::Debug,
        "info" => rmcp::model::LoggingLevel::Info,
        "notice" => rmcp::model::LoggingLevel::Notice,
        "warning" | "warn" => rmcp::model::LoggingLevel::Warning,
        "error" => rmcp::model::LoggingLevel::Error,
        "critical" | "crit" => rmcp::model::LoggingLevel::Critical,
        "alert" => rmcp::model::LoggingLevel::Alert,
        "emergency" | "emerg" => rmcp::model::LoggingLevel::Emergency,
        other => {
            eprintln!(
                "{}",
                format!("  ⚠ Unknown level: '{other}'. Use: debug, info, notice, warning, error, critical, alert, emergency").yellow()
            );
            return;
        }
    };

    let manager = state.mcp_manager.read().await;
    let conn = match manager.get(server_name) {
        Some(c) => c,
        None => {
            eprintln!(
                "{}",
                format!("  ⚠ Server '{server_name}' not found.").yellow()
            );
            return;
        }
    };

    match conn.set_log_level(level).await {
        Ok(()) => {
            eprintln!(
                "{}",
                format!("  ✓ Set log level to '{}' on {server_name}", parts[1]).green()
            );
        }
        Err(e) => {
            eprintln!(
                "{}",
                format!("  ⚠ Failed to set log level on {server_name}: {e}").yellow()
            );
        }
    }
}

/// `/mcp prompt <server>:<name> [arg1 arg2 ...]` — invoke an MCP prompt.
///
/// Fetches the prompt result from the server, extracts text content,
/// and injects it into conversation history so the LLM sees it on the
/// next turn.
pub(super) async fn handle_mcp_prompt_invoke(
    arg: &str,
    state: &mut ReplState,
) -> Result<(), String> {
    // arg here is everything after "/mcp" — so it starts with "prompt ..."
    // But the main.rs match on "/mcp prompt" already split: cmd="/mcp", first word consumed
    // by resolve, so arg = "prompt <server>:<name> [args...]"
    // We need to strip "prompt " prefix
    let rest = if let Some(stripped) = arg.trim().strip_prefix("prompt") {
        stripped.trim()
    } else {
        arg.trim()
    };

    if rest.is_empty() {
        eprintln!(
            "{}",
            "  Usage: /mcp prompt <server>:<name> [arg1 arg2 ...]".dim()
        );
        eprintln!(
            "{}",
            "  Example: /mcp prompt github:create-pr fix authentication bug".dim()
        );
        return Ok(());
    }

    let parts: Vec<&str> = rest.splitn(2, ' ').collect();
    let qualified_name = parts[0];
    let raw_args = parts.get(1).copied().unwrap_or("");

    // Parse server:name
    let (server_name, prompt_name) = match qualified_name.split_once(':') {
        Some((s, n)) if !s.is_empty() && !n.is_empty() => (s, n),
        _ => {
            eprintln!(
                "{}",
                format!("  ⚠ Invalid format: '{qualified_name}'. Use <server>:<prompt_name>").yellow()
            );
            return Ok(());
        }
    };

    // Build arguments map: match positional args to prompt argument names
    let manager = state.mcp_manager.read().await;
    let prompts = manager.all_prompts().await;
    let prompt_def = prompts.iter().find(|(s, p)| s == server_name && p.name == prompt_name);

    let arguments = if !raw_args.is_empty() {
        let arg_values: Vec<&str> = raw_args.split_whitespace().collect();
        let mut map = serde_json::Map::new();

        if let Some((_, def)) = prompt_def {
            // Map positional args to named arguments
            if let Some(ref arg_defs) = def.arguments {
                for (i, val) in arg_values.iter().enumerate() {
                    if let Some(arg_def) = arg_defs.get(i) {
                        map.insert(arg_def.name.clone(), serde_json::Value::String(val.to_string()));
                    }
                }
                // If more values than named args, join remaining as last arg
                if arg_values.len() > arg_defs.len() && !arg_defs.is_empty() {
                    let last_key = &arg_defs[arg_defs.len() - 1].name;
                    let joined = arg_values[arg_defs.len() - 1..].join(" ");
                    map.insert(last_key.clone(), serde_json::Value::String(joined));
                }
            } else {
                // No arg definitions — use "input" as key
                map.insert("input".to_string(), serde_json::Value::String(raw_args.to_string()));
            }
        } else {
            // Prompt definition not found in cache — use "input" as key
            map.insert("input".to_string(), serde_json::Value::String(raw_args.to_string()));
        }
        Some(map)
    } else {
        None
    };

    eprintln!(
        "  {} Invoking prompt {}:{}…",
        "⟳".yellow(),
        server_name,
        prompt_name
    );

    let result = match manager.get_prompt(server_name, prompt_name, arguments).await {
        Ok(r) => r,
        Err(e) => {
            eprintln!("{}", format!("  ⚠ Failed to get prompt: {e}").yellow());
            return Ok(());
        }
    };
    drop(manager);

    if result.messages.is_empty() {
        eprintln!("{}", "  Prompt returned no messages.".dim());
        return Ok(());
    }

    // Extract text content from prompt messages
    let mut user_parts = Vec::new();
    let mut assistant_parts = Vec::new();

    for msg in &result.messages {
        let text = extract_prompt_message_text(&msg.content);
        if text.is_empty() {
            continue;
        }
        match msg.role {
            rmcp::model::PromptMessageRole::User => user_parts.push(text),
            rmcp::model::PromptMessageRole::Assistant => assistant_parts.push(text),
        }
    }

    let user_text = if user_parts.is_empty() {
        format!("[MCP prompt {server_name}:{prompt_name}]")
    } else {
        user_parts.join("\n\n")
    };
    let assistant_text = assistant_parts.join("\n\n");

    // Display what was injected
    if !assistant_text.is_empty() {
        eprintln!(
            "  {} Injected prompt result ({} message{})",
            "✓".green(),
            result.messages.len(),
            if result.messages.len() == 1 { "" } else { "s" },
        );
        // Show a preview
        let preview: String = assistant_text.chars().take(200).collect();
        if assistant_text.len() > 200 {
            eprintln!("  {}", format!("{preview}…").dim());
        } else {
            eprintln!("  {}", preview.dim());
        }
    } else {
        eprintln!(
            "  {} Injected prompt context ({} message{})",
            "✓".green(),
            result.messages.len(),
            if result.messages.len() == 1 { "" } else { "s" },
        );
    }

    // Inject into conversation history
    state.history.push((user_text, assistant_text));

    Ok(())
}

/// Extract text content from a PromptMessageContent.
fn extract_prompt_message_text(content: &rmcp::model::PromptMessageContent) -> String {
    match content {
        rmcp::model::PromptMessageContent::Text { text } => text.clone(),
        rmcp::model::PromptMessageContent::Resource { resource } => {
            // Try to extract text from embedded resource
            match &resource.resource {
                rmcp::model::ResourceContents::TextResourceContents { text, .. } => text.clone(),
                _ => String::new(),
            }
        }
        _ => String::new(),
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

    #[test]
    fn extract_prompt_text_content() {
        let content = rmcp::model::PromptMessageContent::Text {
            text: "Hello world".to_string(),
        };
        assert_eq!(extract_prompt_message_text(&content), "Hello world");
    }

    #[test]
    fn extract_prompt_image_content_empty() {
        use rmcp::model::{Annotated, RawImageContent};
        let content = rmcp::model::PromptMessageContent::Image {
            image: Annotated {
                raw: RawImageContent {
                    data: "abc".to_string(),
                    mime_type: "image/png".to_string(),
                    meta: None,
                },
                annotations: None,
            },
        };
        assert_eq!(extract_prompt_message_text(&content), "");
    }

    #[test]
    fn extract_prompt_resource_text() {
        use rmcp::model::{Annotated, RawEmbeddedResource, ResourceContents};
        let content = rmcp::model::PromptMessageContent::Resource {
            resource: Annotated {
                raw: RawEmbeddedResource {
                    meta: None,
                    resource: ResourceContents::TextResourceContents {
                        uri: "file:///test.txt".to_string(),
                        mime_type: Some("text/plain".to_string()),
                        text: "resource content".to_string(),
                        meta: None,
                    },
                },
                annotations: None,
            },
        };
        assert_eq!(extract_prompt_message_text(&content), "resource content");
    }
}
