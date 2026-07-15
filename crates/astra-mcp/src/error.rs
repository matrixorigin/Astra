use rmcp::ServiceError;

/// MCP client errors.
#[derive(Debug, thiserror::Error)]
pub enum McpError {
    #[error("Invalid configuration: {0}")]
    InvalidConfig(String),

    #[error("Failed to spawn MCP server: {0}")]
    Spawn(String),

    #[error("Failed to initialize MCP connection: {0}")]
    Initialize(String),

    #[error("MCP service error: {0}")]
    Service(#[from] ServiceError),

    #[error("Tool not found: {0}")]
    ToolNotFound(String),

    #[error("Server not connected: {0}")]
    ServerNotConnected(String),

    #[error("Connection lost to server {0}: {1}")]
    ConnectionLost(String, String),

    #[error("Reconnection failed for server {0} after {1} attempts")]
    ReconnectionFailed(String, u32),
}

impl McpError {
    /// Whether a tool request may have crossed the MCP transport boundary
    /// without a terminal provider acknowledgement.
    ///
    /// Callers must preserve this as outcome-unknown evidence; retrying such
    /// an invocation can duplicate external side effects.
    pub fn side_effects_maybe(&self) -> bool {
        matches!(
            self,
            Self::Service(
                ServiceError::TransportSend(_)
                    | ServiceError::TransportClosed
                    | ServiceError::UnexpectedResponse
                    | ServiceError::Cancelled { .. }
                    | ServiceError::Timeout { .. }
            ) | Self::ConnectionLost(_, _)
        )
    }

    pub fn is_timeout(&self) -> bool {
        matches!(self, Self::Service(ServiceError::Timeout { .. }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timeout_is_outcome_unknown_but_pre_dispatch_lookup_failure_is_terminal() {
        let timeout = McpError::Service(ServiceError::Timeout {
            timeout: std::time::Duration::from_secs(30),
        });
        let lookup_failure = McpError::ToolNotFound("missing".to_string());

        assert!(timeout.side_effects_maybe());
        assert!(timeout.is_timeout());
        assert!(!lookup_failure.side_effects_maybe());
        assert!(!lookup_failure.is_timeout());
    }
}
