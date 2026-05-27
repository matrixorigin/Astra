use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use rmcp::model::{
    CallToolResult, CompleteResult, GetPromptResult, Prompt, Reference, Resource, Tool,
};
use serde_json::Value;

use crate::connection::{self, McpConnection};
use crate::error::McpError;
use crate::tools::{extract_result_text, mcp_tool_to_schema, sanitize_tool_name};
use crate::types::{ConnectionState, McpServerConfig};

/// MCP client manager for multiple server connections.
pub struct McpClientManager {
    connections: HashMap<String, Arc<McpConnection>>,
    states: HashMap<String, ConnectionState>,
}

impl Default for McpClientManager {
    fn default() -> Self {
        Self {
            connections: HashMap::new(),
            states: HashMap::new(),
        }
    }
}

impl McpClientManager {
    pub fn new() -> Self {
        Self::default()
    }

    /// Connect to an MCP server. Returns the number of tools discovered.
    pub async fn connect(&mut self, config: McpServerConfig) -> Result<usize, McpError> {
        if !config.enabled {
            return Ok(0);
        }

        let name = config.name.clone();
        self.states
            .insert(name.clone(), ConnectionState::Connecting);

        match connection::connect_to_server(config).await {
            Ok(conn) => {
                let tool_count = conn.tools().len();
                self.states.insert(name.clone(), ConnectionState::Connected);
                self.connections.insert(name, Arc::new(conn));
                Ok(tool_count)
            }
            Err(e) => {
                self.states.insert(name, ConnectionState::Failed);
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
        self.connections
            .iter()
            .flat_map(|(name, conn)| conn.tools().iter().map(move |t| (name.as_str(), t)))
            .collect()
    }

    /// Get all MCP tool schemas in OpenAI function-calling format.
    /// Names follow the `mcp_{server}_{tool}` convention. Deduplicates on name collision.
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
                    "MCP tool name collision: '{name}' from server '{server}' \
                     conflicts with server '{prev_server}' — skipping duplicate"
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

    /// Find which server owns a sanitized MCP tool name (e.g. "mcp_moi_query_sql").
    /// Returns (server_name, original_tool_name) if found.
    pub fn find_tool_by_mcp_name(&self, mcp_name: &str) -> Option<(&str, &str)> {
        for (server_name, conn) in &self.connections {
            for tool in conn.tools() {
                let sanitized = sanitize_tool_name(&format!("mcp__{}__{}", server_name, tool.name));
                if sanitized == mcp_name {
                    return Some((server_name, tool.name.as_ref()));
                }
            }
        }
        None
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

    /// Call a tool by its MCP-prefixed public name (e.g. "mcp_moi_query_sql").
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

    /// Reconnect a server using its stored config.
    pub async fn reconnect(&mut self, name: &str) -> Result<usize, McpError> {
        let config = match self.connections.get(name) {
            Some(conn) => conn.config.clone(),
            None => return Err(McpError::ServerNotConnected(name.to_string())),
        };

        self.connections.remove(name);
        self.states
            .insert(name.to_string(), ConnectionState::Reconnecting);

        match connection::connect_to_server(config).await {
            Ok(conn) => {
                let tool_count = conn.tools().len();
                self.states
                    .insert(name.to_string(), ConnectionState::Connected);
                self.connections.insert(name.to_string(), Arc::new(conn));
                Ok(tool_count)
            }
            Err(e) => {
                self.states
                    .insert(name.to_string(), ConnectionState::Failed);
                Err(e)
            }
        }
    }

    /// Number of active connections.
    pub fn connection_count(&self) -> usize {
        self.connections.len()
    }

    /// Check all connections for tool list change notifications and refresh.
    pub async fn refresh_changed_tools(&mut self) -> Vec<String> {
        let mut refreshed = Vec::new();
        for (name, conn) in &mut self.connections {
            if conn.has_pending_tool_change() {
                if let Some(inner) = Arc::get_mut(conn) {
                    match inner.refresh_tools_if_changed().await {
                        Ok(true) => refreshed.push(name.clone()),
                        Ok(false) => {}
                        Err(e) => tracing::warn!("Failed to refresh tools for {name}: {e}"),
                    }
                }
            }
        }
        if !refreshed.is_empty() {
            tracing::info!("Refreshed tool lists for: {}", refreshed.join(", "));
        }
        refreshed
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
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
