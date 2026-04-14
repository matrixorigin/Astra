//! In-memory pool of live edge agent WebSocket connections.
//!
//! Each entry maps `{user_id}:{edge_agent_id}` to a channel sender that can
//! push [`EdgeServerMessage`] frames to the connected edge agent. The pool is
//! stored in [`AppState`] and queried by the tool routing layer to decide
//! whether to route tool calls to a remote edge or fall back to the server.

use std::sync::Arc;
use std::time::Duration;

use dashmap::DashMap;
use tokio::sync::{mpsc, oneshot};
use uuid::Uuid;

use super::edge_ws_protocol::{EdgeServerMessage, EDGE_TOOL_TIMEOUT_SECS};

/// Sender half that pushes frames into an edge agent's WebSocket write loop.
pub type EdgeWsSender = mpsc::UnboundedSender<EdgeServerMessage>;

/// Metadata about a connected edge agent.
#[derive(Debug, Clone)]
pub struct EdgeConnection {
    pub user_id: String,
    pub edge_agent_id: String,
    pub hostname: Option<String>,
    pub workspace_dir: Option<String>,
    pub sender: EdgeWsSender,
    pub connected_at: std::time::Instant,
    /// Pending tool call responses: request_id → oneshot sender.
    pending_results: Arc<DashMap<String, oneshot::Sender<EdgeToolResult>>>,
}

/// Result from an edge tool execution.
#[derive(Debug, Clone)]
pub struct EdgeToolResult {
    pub output: String,
    pub is_error: bool,
    pub duration_ms: Option<u64>,
}

/// Pool key: `{user_id}:{edge_agent_id}`.
fn pool_key(user_id: &str, edge_agent_id: &str) -> String {
    format!("{user_id}:{edge_agent_id}")
}

/// Thread-safe pool of live edge WebSocket connections.
#[derive(Debug, Clone, Default)]
pub struct EdgeConnectionPool {
    connections: Arc<DashMap<String, EdgeConnection>>,
}

impl EdgeConnectionPool {
    pub fn new() -> Self {
        Self {
            connections: Arc::new(DashMap::new()),
        }
    }

    /// Register a new edge connection. Replaces any existing connection for the same key.
    pub fn register(
        &self,
        user_id: &str,
        edge_agent_id: &str,
        hostname: Option<String>,
        workspace_dir: Option<String>,
        sender: EdgeWsSender,
    ) {
        let key = pool_key(user_id, edge_agent_id);
        self.connections.insert(
            key,
            EdgeConnection {
                user_id: user_id.to_string(),
                edge_agent_id: edge_agent_id.to_string(),
                hostname,
                workspace_dir,
                sender,
                connected_at: std::time::Instant::now(),
                pending_results: Arc::new(DashMap::new()),
            },
        );
    }

    /// Remove an edge connection.
    pub fn unregister(&self, user_id: &str, edge_agent_id: &str) {
        let key = pool_key(user_id, edge_agent_id);
        self.connections.remove(&key);
    }

    /// Check if a user has any connected edge agent.
    pub fn has_connected_edge(&self, user_id: &str) -> bool {
        self.connections
            .iter()
            .any(|entry| entry.value().user_id == user_id && !entry.value().sender.is_closed())
    }

    /// Get all connected edge agents for a user.
    pub fn get_user_edges(&self, user_id: &str) -> Vec<EdgeConnectionInfo> {
        self.connections
            .iter()
            .filter(|entry| entry.value().user_id == user_id && !entry.value().sender.is_closed())
            .map(|entry| {
                let conn = entry.value();
                EdgeConnectionInfo {
                    edge_agent_id: conn.edge_agent_id.clone(),
                    hostname: conn.hostname.clone(),
                    workspace_dir: conn.workspace_dir.clone(),
                    connected_at: conn.connected_at,
                }
            })
            .collect()
    }

    /// Send a tool execution request to an edge agent and await the result.
    ///
    /// Returns `None` if the edge is not connected or the request times out.
    pub async fn execute_tool(
        &self,
        user_id: &str,
        edge_agent_id: &str,
        tool: &str,
        args: &serde_json::Value,
    ) -> Option<EdgeToolResult> {
        let key = pool_key(user_id, edge_agent_id);
        let (pending_results, sender) = {
            let entry = self.connections.get(&key)?;
            let conn = entry.value();
            if conn.sender.is_closed() {
                drop(entry);
                self.connections.remove(&key);
                return None;
            }
            (conn.pending_results.clone(), conn.sender.clone())
        };

        let request_id = Uuid::new_v4().to_string();
        let (tx, rx) = oneshot::channel();
        pending_results.insert(request_id.clone(), tx);

        let msg = EdgeServerMessage::ToolRequest {
            request_id: request_id.clone(),
            tool: tool.to_string(),
            args: args.clone(),
            timeout_secs: EDGE_TOOL_TIMEOUT_SECS,
        };

        if sender.send(msg).is_err() {
            pending_results.remove(&request_id);
            return None;
        }

        let timeout_dur = Duration::from_secs(EDGE_TOOL_TIMEOUT_SECS);
        match tokio::time::timeout(timeout_dur, rx).await {
            Ok(Ok(result)) => Some(result),
            _ => {
                pending_results.remove(&request_id);
                None
            }
        }
    }

    /// Send a tool execution request to the first available edge for a user.
    pub async fn execute_tool_any_edge(
        &self,
        user_id: &str,
        tool: &str,
        args: &serde_json::Value,
    ) -> Option<EdgeToolResult> {
        // Find the first connected edge for this user
        let edge_agent_id = self
            .connections
            .iter()
            .find(|entry| entry.value().user_id == user_id && !entry.value().sender.is_closed())
            .map(|entry| entry.value().edge_agent_id.clone())?;

        self.execute_tool(user_id, &edge_agent_id, tool, args).await
    }

    /// Deliver a tool result from an edge agent (called from the edge WS read loop).
    pub fn deliver_tool_result(
        &self,
        user_id: &str,
        edge_agent_id: &str,
        request_id: &str,
        result: EdgeToolResult,
    ) -> bool {
        let key = pool_key(user_id, edge_agent_id);
        if let Some(entry) = self.connections.get(&key) {
            if let Some((_, tx)) = entry.value().pending_results.remove(request_id) {
                return tx.send(result).is_ok();
            }
        }
        false
    }

    /// Remove stale connections (sender closed).
    pub fn cleanup_stale(&self) {
        self.connections
            .retain(|_, conn| !conn.sender.is_closed());
    }

    /// Number of active connections.
    pub fn connection_count(&self) -> usize {
        self.connections.len()
    }
}

/// Public info about a connected edge (no sender exposed).
#[derive(Debug, Clone)]
pub struct EdgeConnectionInfo {
    pub edge_agent_id: String,
    pub hostname: Option<String>,
    pub workspace_dir: Option<String>,
    pub connected_at: std::time::Instant,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn register_and_check_connected() {
        let pool = EdgeConnectionPool::new();
        assert!(!pool.has_connected_edge("user-1"));

        let (tx, _rx) = mpsc::unbounded_channel();
        pool.register("user-1", "edge-a", Some("laptop".into()), None, tx);

        assert!(pool.has_connected_edge("user-1"));
        assert!(!pool.has_connected_edge("user-2"));
        assert_eq!(pool.connection_count(), 1);
    }

    #[test]
    fn unregister_removes_connection() {
        let pool = EdgeConnectionPool::new();
        let (tx, _rx) = mpsc::unbounded_channel();
        pool.register("user-1", "edge-a", None, None, tx);
        assert!(pool.has_connected_edge("user-1"));

        pool.unregister("user-1", "edge-a");
        assert!(!pool.has_connected_edge("user-1"));
        assert_eq!(pool.connection_count(), 0);
    }

    #[test]
    fn get_user_edges_returns_info() {
        let pool = EdgeConnectionPool::new();
        let (tx1, _rx1) = mpsc::unbounded_channel();
        let (tx2, _rx2) = mpsc::unbounded_channel();
        pool.register("user-1", "edge-a", Some("laptop".into()), None, tx1);
        pool.register("user-1", "edge-b", Some("desktop".into()), None, tx2);

        let edges = pool.get_user_edges("user-1");
        assert_eq!(edges.len(), 2);
        let names: Vec<&str> = edges.iter().map(|e| e.edge_agent_id.as_str()).collect();
        assert!(names.contains(&"edge-a"));
        assert!(names.contains(&"edge-b"));
    }

    #[test]
    fn closed_sender_detected_as_disconnected() {
        let pool = EdgeConnectionPool::new();
        let (tx, rx) = mpsc::unbounded_channel();
        pool.register("user-1", "edge-a", None, None, tx);
        drop(rx); // close the receiver, which closes the sender
        assert!(!pool.has_connected_edge("user-1"));
    }

    #[test]
    fn cleanup_stale_removes_closed() {
        let pool = EdgeConnectionPool::new();
        let (tx, rx) = mpsc::unbounded_channel();
        pool.register("user-1", "edge-stale", None, None, tx);
        drop(rx);
        assert_eq!(pool.connection_count(), 1);
        pool.cleanup_stale();
        assert_eq!(pool.connection_count(), 0);
    }

    #[tokio::test]
    async fn deliver_tool_result_completes_pending() {
        let pool = EdgeConnectionPool::new();
        let (tx, mut rx) = mpsc::unbounded_channel();
        pool.register("user-1", "edge-a", None, None, tx);

        // Spawn a task that will call execute_tool
        let pool_clone = pool.clone();
        let handle = tokio::spawn(async move {
            pool_clone
                .execute_tool("user-1", "edge-a", "bash", &json!({"command": "ls"}))
                .await
        });

        // Wait a bit for the request to be sent
        tokio::time::sleep(Duration::from_millis(50)).await;

        // Read the tool request from the receiver
        let msg = rx.recv().await.unwrap();
        let request_id = match msg {
            EdgeServerMessage::ToolRequest { request_id, .. } => request_id,
            _ => panic!("expected ToolRequest"),
        };

        // Deliver the result
        let delivered = pool.deliver_tool_result(
            "user-1",
            "edge-a",
            &request_id,
            EdgeToolResult {
                output: "file1.txt\nfile2.txt".into(),
                is_error: false,
                duration_ms: Some(10),
            },
        );
        assert!(delivered);

        // The execute_tool should now complete
        let result = handle.await.unwrap();
        assert!(result.is_some());
        let result = result.unwrap();
        assert_eq!(result.output, "file1.txt\nfile2.txt");
        assert!(!result.is_error);
    }

    #[tokio::test]
    async fn execute_tool_returns_none_for_missing_edge() {
        let pool = EdgeConnectionPool::new();
        let result = pool
            .execute_tool("user-1", "nonexistent", "bash", &json!({}))
            .await;
        assert!(result.is_none());
    }
}
