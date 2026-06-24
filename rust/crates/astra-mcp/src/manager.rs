use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use rmcp::model::{
    CallToolResult, CompleteResult, GetPromptResult, Prompt, Reference, Resource, Root, Tool,
};
use serde_json::Value;
use tokio::sync::RwLock;

use crate::connection::{self, McpConnection};
use crate::error::McpError;
use crate::tools::{extract_result_text, mcp_tool_to_schema, sanitize_tool_name};
use crate::types::{ConnectionState, McpServerConfig};

#[derive(Debug, Clone, PartialEq, Eq)]
struct McpToolRoute {
    server_name: String,
    original_tool_name: String,
}

/// MCP client manager for multiple server connections.
pub struct McpClientManager {
    connections: HashMap<String, Arc<McpConnection>>,
    states: HashMap<String, ConnectionState>,
    tool_routes_by_public_name: HashMap<String, McpToolRoute>,
    /// Shared roots list — returned to servers via `roots/list`.
    roots: Arc<RwLock<Vec<Root>>>,
}

impl Default for McpClientManager {
    fn default() -> Self {
        Self {
            connections: HashMap::new(),
            states: HashMap::new(),
            tool_routes_by_public_name: HashMap::new(),
            roots: Arc::new(RwLock::new(Vec::new())),
        }
    }
}

impl McpClientManager {
    pub fn new() -> Self {
        Self::default()
    }

    /// Get a reference to the shared roots list.
    pub fn roots(&self) -> &Arc<RwLock<Vec<Root>>> {
        &self.roots
    }

    /// Connect to an MCP server with retry. Returns the number of tools discovered.
    pub async fn connect(&mut self, config: McpServerConfig) -> Result<usize, McpError> {
        if !config.enabled {
            return Ok(0);
        }

        let name = config.name.clone();
        self.states
            .insert(name.clone(), ConnectionState::Connecting);

        match connection::connect_to_server(config, self.roots.clone()).await {
            Ok(conn) => {
                let tool_count = conn.tools().len();
                self.states.insert(name.clone(), ConnectionState::Connected);
                self.connections.insert(name, Arc::new(conn));
                self.rebuild_tool_route_index();
                Ok(tool_count)
            }
            Err(e) => {
                self.states.insert(name, ConnectionState::Failed);
                Err(e)
            }
        }
    }

    /// Reconnect a server using its stored config.
    pub async fn reconnect(&mut self, name: &str) -> Result<usize, McpError> {
        let config = match self.connections.get(name) {
            Some(conn) => conn.config.clone(),
            None => return Err(McpError::ServerNotConnected(name.to_string())),
        };

        self.connections.remove(name);
        self.states
            .insert(name.to_string(), ConnectionState::Reconnecting);

        match connection::connect_to_server(config, self.roots.clone()).await {
            Ok(conn) => {
                let tool_count = conn.tools().len();
                self.states
                    .insert(name.to_string(), ConnectionState::Connected);
                self.connections.insert(name.to_string(), Arc::new(conn));
                self.rebuild_tool_route_index();
                Ok(tool_count)
            }
            Err(e) => {
                self.states
                    .insert(name.to_string(), ConnectionState::Failed);
                self.rebuild_tool_route_index();
                Err(e)
            }
        }
    }

    /// Disconnect from a server by name.
    pub fn disconnect(&mut self, name: &str) -> bool {
        let removed = self.connections.remove(name).is_some();
        if removed {
            self.states
                .insert(name.to_string(), ConnectionState::Disconnected);
            self.rebuild_tool_route_index();
        }
        removed
    }

    /// Get a connection by name.
    pub fn get(&self, name: &str) -> Option<Arc<McpConnection>> {
        self.connections.get(name).cloned()
    }

    /// List all connected server names.
    pub fn connected_servers(&self) -> Vec<&str> {
        self.connections.keys().map(|s| s.as_str()).collect()
    }

    /// Get the connection state for a server.
    pub fn server_state(&self, name: &str) -> Option<ConnectionState> {
        self.states.get(name).copied()
    }

    /// Get all tools from all connected servers.
    pub fn all_tools(&self) -> Vec<(&str, &Tool)> {
        let mut tools: Vec<(&str, &Tool)> = self
            .connections
            .iter()
            .flat_map(|(name, conn)| conn.tools().iter().map(move |t| (name.as_str(), t)))
            .collect();
        tools.sort_by(|(server_a, tool_a), (server_b, tool_b)| {
            server_a
                .cmp(server_b)
                .then_with(|| tool_a.name.as_ref().cmp(tool_b.name.as_ref()))
        });
        tools
    }

    /// Get all MCP tool schemas in OpenAI function-calling format.
    /// Names follow the `mcp__{server}__{tool}` convention. Deduplicates on name collision.
    pub fn all_tool_schemas(&self) -> Vec<Value> {
        let mut seen: HashMap<String, &str> = HashMap::new();
        let mut schemas = Vec::new();
        let mut collision_count = 0usize;

        for (server, tool) in self.all_tools() {
            let schema = mcp_tool_to_schema(server, tool);
            let name = schema["function"]["name"]
                .as_str()
                .unwrap_or("")
                .to_string();
            if let Some(prev_server) = seen.get(&name) {
                tracing::warn!(
                    public_name = %name,
                    skipped_server = %server,
                    kept_server = %prev_server,
                    "MCP tool name collision; keeping first server and skipping duplicate"
                );
                collision_count += 1;
                continue;
            }
            seen.insert(name, server);
            schemas.push(schema);
        }

        if collision_count > 0 {
            tracing::warn!("{collision_count} MCP tool(s) skipped due to name collisions");
        }
        schemas
    }

    /// Find which server owns a sanitized MCP tool name (e.g. "mcp__moi__query_sql").
    /// Returns (server_name, original_tool_name) if found.
    pub fn find_tool_by_mcp_name(&self, mcp_name: &str) -> Option<(&str, &str)> {
        self.tool_routes_by_public_name.get(mcp_name).map(|route| {
            (
                route.server_name.as_str(),
                route.original_tool_name.as_str(),
            )
        })
    }

    /// Find which server has a specific original tool name.
    pub fn find_tool(&self, tool_name: &str) -> Option<(&str, &Tool)> {
        for (server_name, conn) in &self.connections {
            for tool in conn.tools() {
                if tool.name == tool_name {
                    return Some((server_name, tool));
                }
            }
        }
        None
    }

    /// Call a tool by its original name, routing to the correct server.
    pub async fn call_tool(
        &self,
        tool_name: &str,
        arguments: Value,
    ) -> Result<CallToolResult, McpError> {
        let (server_name, _) = self
            .find_tool(tool_name)
            .ok_or_else(|| McpError::ToolNotFound(tool_name.to_string()))?;

        let conn = self
            .connections
            .get(server_name)
            .ok_or_else(|| McpError::ServerNotConnected(server_name.to_string()))?;

        conn.call_tool(tool_name, arguments)
            .await
            .map_err(McpError::Service)
    }

    /// Call a tool by its MCP-prefixed public name (e.g. "mcp__moi__query_sql").
    /// Resolves the server + original name, executes, and returns text result.
    pub async fn call_tool_by_mcp_name(
        &self,
        mcp_name: &str,
        arguments: Value,
    ) -> Result<String, McpError> {
        let (server_name, original_name) = self
            .find_tool_by_mcp_name(mcp_name)
            .ok_or_else(|| McpError::ToolNotFound(mcp_name.to_string()))?;

        let conn = self
            .connections
            .get(server_name)
            .ok_or_else(|| McpError::ServerNotConnected(server_name.to_string()))?;

        let result = conn
            .call_tool(original_name, arguments)
            .await
            .map_err(McpError::Service)?;

        Ok(extract_result_text(&result))
    }

    /// Number of active connections.
    pub fn connection_count(&self) -> usize {
        self.connections.len()
    }

    /// Check all connections for tool list change notifications and refresh.
    pub async fn refresh_changed_tools(&mut self) -> Vec<String> {
        let mut refreshed = Vec::new();
        for (name, conn) in &mut self.connections {
            if conn.has_pending_tool_change()
                && let Some(inner) = Arc::get_mut(conn)
            {
                match inner.refresh_tools_if_changed().await {
                    Ok(true) => refreshed.push(name.clone()),
                    Ok(false) => {}
                    Err(e) => tracing::warn!("Failed to refresh tools for {name}: {e}"),
                }
            }
        }
        if !refreshed.is_empty() {
            self.rebuild_tool_route_index();
            tracing::info!("Refreshed tool lists for: {}", refreshed.join(", "));
        }
        refreshed
    }

    /// Consume prompt-list change notifications and return changed servers.
    pub fn consume_prompt_changes(&self) -> Vec<String> {
        let mut changed = Vec::new();
        for (name, conn) in &self.connections {
            if conn.consume_prompt_change() {
                changed.push(name.clone());
            }
        }
        changed
    }

    /// Consume resource-list change notifications and return changed servers.
    pub fn consume_resource_changes(&self) -> Vec<String> {
        let mut changed = Vec::new();
        for (name, conn) in &self.connections {
            if conn.consume_resource_change() {
                changed.push(name.clone());
            }
        }
        changed
    }

    /// Aggregate all prompts from all connected servers.
    /// Returns (server_name, prompt) pairs.
    pub async fn all_prompts(&self) -> Vec<(String, Prompt)> {
        let mut result = Vec::new();
        for (name, conn) in &self.connections {
            match conn.list_prompts().await {
                Ok(prompts) => {
                    for p in prompts {
                        result.push((name.clone(), p));
                    }
                }
                Err(e) => {
                    tracing::warn!("Failed to list prompts from {name}: {e}");
                }
            }
        }
        result
    }

    /// Aggregate all resources from all connected servers.
    /// Returns (server_name, resource) pairs.
    pub async fn all_resources(&self) -> Vec<(String, Resource)> {
        let mut result = Vec::new();
        for (name, conn) in &self.connections {
            match conn.list_resources().await {
                Ok(resources) => {
                    for r in resources {
                        result.push((name.clone(), r));
                    }
                }
                Err(e) => {
                    tracing::warn!("Failed to list resources from {name}: {e}");
                }
            }
        }
        result
    }

    /// Get a prompt from a specific server.
    pub async fn get_prompt(
        &self,
        server_name: &str,
        prompt_name: &str,
        arguments: Option<serde_json::Map<String, serde_json::Value>>,
    ) -> Result<GetPromptResult, McpError> {
        let conn = self
            .connections
            .get(server_name)
            .ok_or_else(|| McpError::ServerNotConnected(server_name.to_string()))?;
        conn.get_prompt(prompt_name, arguments)
            .await
            .map_err(McpError::Service)
    }

    /// Request argument completions from a specific server.
    pub async fn complete(
        &self,
        server: &str,
        reference: Reference,
        argument_name: &str,
        argument_value: &str,
    ) -> Result<CompleteResult, McpError> {
        let conn = self
            .connections
            .get(server)
            .ok_or_else(|| McpError::ServerNotConnected(server.to_string()))?;
        conn.complete(reference, argument_name, argument_value)
            .await
            .map_err(McpError::Service)
    }

    /// Ping a specific server to check connectivity, returning latency.
    pub async fn ping(&self, server: &str) -> Result<std::time::Duration, McpError> {
        let conn = self
            .connections
            .get(server)
            .ok_or_else(|| McpError::ServerNotConnected(server.to_string()))?;
        let start = Instant::now();
        conn.ping().await.map_err(McpError::Service)?;
        Ok(start.elapsed())
    }

    /// Ping all connected servers, returning (name, latency_or_error) pairs.
    pub async fn ping_all(&self) -> Vec<(String, Result<std::time::Duration, McpError>)> {
        let mut results = Vec::new();
        for (name, conn) in &self.connections {
            let start = Instant::now();
            let result = conn.ping().await;
            results.push((
                name.clone(),
                result.map(|_| start.elapsed()).map_err(McpError::Service),
            ));
        }
        results
    }

    /// Get the connection state for all servers.
    pub fn server_states(&self) -> Vec<(&str, ConnectionState)> {
        self.states
            .iter()
            .map(|(name, state)| (name.as_str(), *state))
            .collect()
    }

    fn rebuild_tool_route_index(&mut self) {
        self.tool_routes_by_public_name = build_tool_route_index(self.all_tools());
    }
}

fn build_tool_route_index<'a, I>(tools: I) -> HashMap<String, McpToolRoute>
where
    I: IntoIterator<Item = (&'a str, &'a Tool)>,
{
    let mut routes: HashMap<String, McpToolRoute> = HashMap::new();
    for (server_name, tool) in tools {
        let public_name = sanitize_tool_name(&format!("mcp__{}__{}", server_name, tool.name));
        let route = McpToolRoute {
            server_name: server_name.to_string(),
            original_tool_name: tool.name.to_string(),
        };
        if let Some(existing) = routes.get(&public_name) {
            tracing::warn!(
                public_name = %public_name,
                skipped_server = %route.server_name,
                kept_server = %existing.server_name,
                "MCP tool route collision; keeping first server and skipping duplicate"
            );
            continue;
        }
        routes.insert(public_name, route);
    }
    routes
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn empty_schema() -> Arc<serde_json::Map<String, serde_json::Value>> {
        Arc::new(serde_json::Map::new())
    }

    #[test]
    fn manager_empty() {
        let manager = McpClientManager::new();
        assert!(manager.connected_servers().is_empty());
        assert!(manager.all_tools().is_empty());
        assert!(manager.find_tool("any_tool").is_none());
    }

    #[test]
    fn manager_disconnect_nonexistent() {
        let mut manager = McpClientManager::new();
        assert!(!manager.disconnect("nonexistent"));
    }

    #[test]
    fn tool_route_index_maps_public_names_to_original_server_tools() {
        let tools = vec![
            (
                "mock-server",
                Tool::new("echo message", "Echo", empty_schema()),
            ),
            ("sql", Tool::new("query.sql", "Query", empty_schema())),
        ];

        let index = build_tool_route_index(tools.iter().map(|(server, tool)| (*server, tool)));

        assert_eq!(
            index.get("mcp__mock-server__echo_message"),
            Some(&McpToolRoute {
                server_name: "mock-server".to_string(),
                original_tool_name: "echo message".to_string(),
            })
        );
        assert_eq!(
            index.get("mcp__sql__query_sql"),
            Some(&McpToolRoute {
                server_name: "sql".to_string(),
                original_tool_name: "query.sql".to_string(),
            })
        );
    }

    #[test]
    fn tool_route_index_keeps_first_collision_winner() {
        let tools = vec![
            ("api", Tool::new("query.sql", "Query", empty_schema())),
            ("api", Tool::new("query sql", "Query", empty_schema())),
        ];

        let index = build_tool_route_index(tools.iter().map(|(server, tool)| (*server, tool)));

        assert_eq!(
            index.get("mcp__api__query_sql"),
            Some(&McpToolRoute {
                server_name: "api".to_string(),
                original_tool_name: "query.sql".to_string(),
            })
        );
    }
}
