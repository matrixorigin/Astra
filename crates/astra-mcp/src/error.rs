use rmcp::ServiceError;

/// Failure of a cancellation-aware tool call. Only the transport preflight
/// can prove that cancellation happened before the request was dispatched.
#[derive(Debug, thiserror::Error)]
pub enum McpToolCallError {
    #[error("MCP tool cancelled before dispatch")]
    CancelledBeforeDispatch,
    #[error(transparent)]
    Service(#[from] ServiceError),
}

impl McpToolCallError {
    /// Preserve the existing non-cancellable connection API's error contract.
    pub fn into_service_error(self) -> ServiceError {
        match self {
            Self::CancelledBeforeDispatch => ServiceError::Cancelled {
                reason: Some("run cancelled before dispatch".into()),
            },
            Self::Service(error) => error,
        }
    }
}

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

    #[error("No resolved execution policy for MCP tool: {0}")]
    ToolPolicyUnavailable(String),

    #[error("Server not connected: {0}")]
    ServerNotConnected(String),

    #[error("Connection lost to server {0}: {1}")]
    ConnectionLost(String, String),

    #[error("Reconnection failed for server {0} after {1} attempts")]
    ReconnectionFailed(String, u32),
}

impl McpError {
    /// A correlated provider error is a terminal response, unlike a local
    /// transport/preparation failure while resuming an interaction.
    pub fn provider_acknowledged(&self) -> bool {
        matches!(self, Self::Service(ServiceError::McpError(_)))
    }

    /// Whether a tool request may have crossed the MCP transport boundary
    /// without a terminal provider acknowledgement.
    ///
    /// Callers must preserve this as outcome-unknown evidence; retrying such
    /// an invocation can duplicate external side effects.
    pub fn side_effects_maybe(&self) -> bool {
        if let Self::Service(ServiceError::McpError(error)) = self
            && error
                .data
                .as_ref()
                .and_then(|data| data.get("execution_outcome"))
                .and_then(serde_json::Value::as_str)
                == Some("unknown")
        {
            return true;
        }
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
    fn correlated_provider_error_preserves_explicit_unknown_without_losing_acknowledgement() {
        for outcome in [None, Some("failed"), Some("unknown")] {
            let error = serde_json::from_value::<rmcp::model::ErrorData>(serde_json::json!({
                "code":-32000, "message":"unknown is only prose here", "data":outcome.map(|value| serde_json::json!({"execution_outcome":value}))
            })).unwrap();
            let error = McpError::Service(ServiceError::McpError(error));
            assert!(error.provider_acknowledged());
            assert_eq!(error.side_effects_maybe(), outcome == Some("unknown"));
        }
    }

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
