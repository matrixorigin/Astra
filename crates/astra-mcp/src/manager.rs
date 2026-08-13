use std::collections::HashMap;

#[cfg(test)]
use std::collections::HashSet;
use std::sync::Arc;
use std::time::Instant;

use rmcp::model::{
    CallToolResult, CompleteResult, GetPromptResult, Prompt, Reference, Resource, Root, Tool,
};
use serde_json::Value;
use tokio::sync::RwLock;

use crate::connection::{self, McpConnection};
use crate::error::McpError;
use crate::tools::{
    McpToolCallResult, extract_tool_call_result, mcp_tool_to_schema, sanitize_tool_name,
};
use crate::types::{ConnectionState, McpServerConfig};

#[derive(Debug, Clone, PartialEq, Eq)]
struct McpToolRoute {
    server_name: String,
    original_tool_name: String,
}

/// Concrete MCP tool source that maps to a public tool name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpToolCollisionSource {
    pub server: String,
    pub original_tool_name: String,
}

/// Diagnostic record for multiple MCP tools mapping to one public tool name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpToolCollision {
    pub public_name: String,
    pub sources: Vec<McpToolCollisionSource>,
}

/// MCP client manager for multiple server connections.
pub struct McpClientManager {
    connections: HashMap<String, Arc<McpConnection>>,
    states: HashMap<String, ConnectionState>,
    tool_routes_by_public_name: HashMap<String, McpToolRoute>,
    /// Collisions detected during the last tool route index rebuild.
    tool_collisions: Vec<McpToolCollision>,
    /// Shared roots list — returned to servers via `roots/list`.
    roots: Arc<RwLock<Vec<Root>>>,
}

/// A fully established MCP connection that has not yet been published into a
/// shared manager. Connection handshakes may involve process startup and
/// network I/O, so callers can prepare one without holding the manager lock and
/// then install it with a short synchronous mutation.
pub struct PreparedMcpConnection {
    name: String,
    connection: Arc<McpConnection>,
}

impl PreparedMcpConnection {
    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn connection(&self) -> Arc<McpConnection> {
        self.connection.clone()
    }
}

impl Default for McpClientManager {
    fn default() -> Self {
        Self {
            connections: HashMap::new(),
            states: HashMap::new(),
            tool_routes_by_public_name: HashMap::new(),
            tool_collisions: Vec::new(),
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

    /// Establish a connection without mutating (or requiring a lock on) a
    /// shared manager.
    pub async fn prepare_connection(
        config: McpServerConfig,
        roots: Arc<RwLock<Vec<Root>>>,
    ) -> Result<Option<PreparedMcpConnection>, McpError> {
        if !config.enabled {
            return Ok(None);
        }
        let name = config.name.clone();
        let connection = connection::connect_to_server(config, roots).await?;
        Ok(Some(PreparedMcpConnection {
            name,
            connection: Arc::new(connection),
        }))
    }

    /// Atomically publish a prepared connection into this manager.
    pub fn install_prepared_connection(&mut self, prepared: PreparedMcpConnection) -> usize {
        let tool_count = prepared.connection.tools().len();
        self.states
            .insert(prepared.name.clone(), ConnectionState::Connected);
        self.connections.insert(prepared.name, prepared.connection);
        self.rebuild_tool_route_index();
        tool_count
    }

    /// Record a failed detached connection attempt for status surfaces.
    pub fn record_connection_failure(&mut self, name: impl Into<String>) {
        self.states.insert(name.into(), ConnectionState::Failed);
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
    /// Names follow the `mcp__{server}__{tool}` convention. Public-name
    /// collisions fail closed instead of choosing an arbitrary route.
    pub fn all_tool_schemas(&self) -> Vec<Value> {
        build_tool_schemas(self.all_tools())
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
    /// Resolves the server + original name, executes, and returns text plus
    /// structured result fields.
    pub async fn call_tool_by_mcp_name(
        &self,
        mcp_name: &str,
        arguments: Value,
    ) -> Result<McpToolCallResult, McpError> {
        self.call_tool_by_mcp_name_with_metadata(mcp_name, arguments, None)
            .await
    }

    pub async fn call_tool_by_mcp_name_with_metadata(
        &self,
        mcp_name: &str,
        arguments: Value,
        protocol_metadata: Option<serde_json::Map<String, Value>>,
    ) -> Result<McpToolCallResult, McpError> {
        let (server_name, original_name) = self
            .find_tool_by_mcp_name(mcp_name)
            .ok_or_else(|| McpError::ToolNotFound(mcp_name.to_string()))?;

        let conn = self
            .connections
            .get(server_name)
            .ok_or_else(|| McpError::ServerNotConnected(server_name.to_string()))?;

        let result = conn
            .call_tool_with_metadata(original_name, arguments, protocol_metadata)
            .await
            .map_err(McpError::Service)?;

        Ok(extract_tool_call_result(&result))
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
            // Execution/runtime-binding lookups use `find_tool_by_mcp_name`,
            // not `all_tool_schemas()`. Any refreshed tool list must therefore
            // rebuild the public-name route index atomically with the schema
            // snapshot update.
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

    /// Tool name collisions detected during the most recent tool route rebuild.
    /// Each collision means all tools sharing that public name are hidden.
    pub fn tool_collisions(&self) -> &[McpToolCollision] {
        &self.tool_collisions
    }

    fn rebuild_tool_route_index(&mut self) {
        let (routes, collisions) = build_tool_route_index(self.all_tools());
        for collision in &collisions {
            tracing::error!(
                public_name = %collision.public_name,
                sources = ?collision.sources,
                "MCP tool name collision — multiple MCP tools map to the same public name"
            );
        }
        self.tool_routes_by_public_name = routes;
        self.tool_collisions = collisions;
    }
}

fn build_tool_route_index<'a, I>(tools: I) -> (HashMap<String, McpToolRoute>, Vec<McpToolCollision>)
where
    I: IntoIterator<Item = (&'a str, &'a Tool)>,
{
    let tools = tools.into_iter().collect::<Vec<_>>();
    let collisions =
        colliding_public_tool_names(tools.iter().map(|(server, tool)| (*server, *tool)));
    let mut routes: HashMap<String, McpToolRoute> = HashMap::new();
    let mut collision_diagnostics: Vec<McpToolCollision> = Vec::new();
    for (server_name, tool) in tools {
        let public_name = public_tool_name(server_name, tool);
        if collisions.contains_key(&public_name) {
            continue;
        }
        let route = McpToolRoute {
            server_name: server_name.to_string(),
            original_tool_name: tool.name.to_string(),
        };
        routes.insert(public_name, route);
    }
    for (public_name, sources) in collisions {
        collision_diagnostics.push(McpToolCollision {
            public_name,
            sources,
        });
    }
    (routes, collision_diagnostics)
}

fn build_tool_schemas<'a, I>(tools: I) -> Vec<Value>
where
    I: IntoIterator<Item = (&'a str, &'a Tool)>,
{
    let tools = tools.into_iter().collect::<Vec<_>>();
    let collisions =
        colliding_public_tool_names(tools.iter().map(|(server, tool)| (*server, *tool)));
    let mut schemas = Vec::new();
    for (server, tool) in tools {
        let public_name = public_tool_name(server, tool);
        if collisions.contains_key(&public_name) {
            continue;
        }
        schemas.push(mcp_tool_to_schema(server, tool));
    }
    schemas
}

fn colliding_public_tool_names<'a, I>(tools: I) -> HashMap<String, Vec<McpToolCollisionSource>>
where
    I: IntoIterator<Item = (&'a str, &'a Tool)>,
{
    let mut sources_by_public_name: HashMap<String, Vec<McpToolCollisionSource>> = HashMap::new();
    for (server_name, tool) in tools {
        sources_by_public_name
            .entry(public_tool_name(server_name, tool))
            .or_default()
            .push(McpToolCollisionSource {
                server: server_name.to_string(),
                original_tool_name: tool.name.to_string(),
            });
    }
    sources_by_public_name
        .into_iter()
        .filter(|(_, sources)| sources.len() > 1)
        .collect()
}

fn public_tool_name(server_name: &str, tool: &Tool) -> String {
    sanitize_tool_name(&format!("mcp__{}__{}", server_name, tool.name))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{RetryConfig, Transport};
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
        assert!(manager.tool_collisions().is_empty());
    }

    #[tokio::test]
    async fn disabled_connection_can_be_prepared_without_mutating_shared_manager() {
        let manager = McpClientManager::new();
        let config = McpServerConfig {
            name: "disabled".to_string(),
            transport: Transport::Stdio {
                command: vec!["must-not-run".to_string()],
                args: Vec::new(),
                env: HashMap::new(),
            },
            description: String::new(),
            enabled: false,
            retry: RetryConfig::default(),
        };

        let prepared = McpClientManager::prepare_connection(config, manager.roots().clone())
            .await
            .expect("disabled configuration is a no-op");

        assert!(prepared.is_none());
        assert_eq!(manager.connection_count(), 0);
        assert!(manager.server_state("disabled").is_none());
    }

    #[test]
    fn manager_disconnect_nonexistent() {
        let mut manager = McpClientManager::new();
        assert!(!manager.disconnect("nonexistent"));
    }

    #[test]
    fn tool_route_index_maps_public_names_to_original_server_tools() {
        let tools = [
            (
                "mock-server",
                Tool::new("echo message", "Echo", empty_schema()),
            ),
            ("sql", Tool::new("query.sql", "Query", empty_schema())),
        ];

        let (index, collisions) =
            build_tool_route_index(tools.iter().map(|(server, tool)| (*server, tool)));

        assert!(collisions.is_empty());
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
    fn tool_route_index_drops_colliding_public_names_and_reports_collision() {
        let tools = [
            ("api", Tool::new("query.sql", "Query", empty_schema())),
            ("api", Tool::new("query sql", "Query", empty_schema())),
        ];

        let (index, collisions) =
            build_tool_route_index(tools.iter().map(|(server, tool)| (*server, tool)));

        assert!(!index.contains_key("mcp__api__query_sql"));
        assert_eq!(collisions.len(), 1);
        assert_eq!(collisions[0].public_name, "mcp__api__query_sql");
        assert_eq!(
            collisions[0].sources,
            vec![
                McpToolCollisionSource {
                    server: "api".to_string(),
                    original_tool_name: "query.sql".to_string(),
                },
                McpToolCollisionSource {
                    server: "api".to_string(),
                    original_tool_name: "query sql".to_string(),
                },
            ]
        );
    }

    #[test]
    fn tool_schema_builder_drops_colliding_public_names() {
        let tools = [
            ("api", Tool::new("query.sql", "Query", empty_schema())),
            ("api", Tool::new("query sql", "Query", empty_schema())),
            ("api", Tool::new("status", "Status", empty_schema())),
        ];

        let schemas = build_tool_schemas(tools.iter().map(|(server, tool)| (*server, tool)));
        let names: HashSet<String> = schemas
            .iter()
            .filter_map(|schema| {
                schema
                    .get("function")
                    .and_then(|function| function.get("name"))
                    .and_then(Value::as_str)
            })
            .map(str::to_string)
            .collect();

        assert!(!names.contains("mcp__api__query_sql"));
        assert!(names.contains("mcp__api__status"));
    }

    #[test]
    fn tool_route_index_rebuild_drops_removed_tools_and_adds_new_tools() {
        let first_tools = [("api", Tool::new("old.query", "Old", empty_schema()))];
        let second_tools = [("api", Tool::new("new.query", "New", empty_schema()))];

        let (first, _) =
            build_tool_route_index(first_tools.iter().map(|(server, tool)| (*server, tool)));
        assert!(first.contains_key("mcp__api__old_query"));
        assert!(!first.contains_key("mcp__api__new_query"));

        let (rebuilt, _) =
            build_tool_route_index(second_tools.iter().map(|(server, tool)| (*server, tool)));
        assert!(!rebuilt.contains_key("mcp__api__old_query"));
        assert_eq!(
            rebuilt.get("mcp__api__new_query"),
            Some(&McpToolRoute {
                server_name: "api".to_string(),
                original_tool_name: "new.query".to_string(),
            })
        );
    }

    #[test]
    fn same_server_sanitized_tool_collision_reports_original_tool_sources() {
        // The public name includes the server prefix, so different servers with
        // the same tool name produce different public names. Collision happens
        // when distinct tools on the same server sanitize to the same suffix.
        let tools = [
            ("server-a", Tool::new("query sql", "Query", empty_schema())),
            ("server-a", Tool::new("query.sql", "Query", empty_schema())),
        ];

        let (index, collisions) =
            build_tool_route_index(tools.iter().map(|(server, tool)| (*server, tool)));

        assert!(!index.contains_key("mcp__server-a__query_sql"));
        assert_eq!(collisions.len(), 1);
        assert_eq!(collisions[0].public_name, "mcp__server-a__query_sql");
        assert_eq!(
            collisions[0].sources,
            vec![
                McpToolCollisionSource {
                    server: "server-a".to_string(),
                    original_tool_name: "query sql".to_string(),
                },
                McpToolCollisionSource {
                    server: "server-a".to_string(),
                    original_tool_name: "query.sql".to_string(),
                },
            ]
        );
    }
}
