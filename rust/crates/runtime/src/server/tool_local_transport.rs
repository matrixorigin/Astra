use std::sync::Arc;

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

use super::tool_execution_binding::{
    ExecutorBinding, ToolExecutionRequest, ToolTransportKind, WorkspaceBinding,
};
use super::tool_transport_metadata::cancelled_runtime_tool_result_for_binding;

#[async_trait]
pub trait ServerLocalToolTransport: Send + Sync {
    async fn execute_server_local_tool(
        &self,
        request: &ToolExecutionRequest,
    ) -> astra_tools::ToolResult;
}

pub(crate) async fn execute_local_transport<L>(
    request: &ToolExecutionRequest,
    binding: &astra_runtime_env::RunBinding,
    result_workspace: &WorkspaceBinding,
    result_executor: &ExecutorBinding,
    result_transport: ToolTransportKind,
    local_transport: &L,
    cancel_token: Option<&Arc<CancellationToken>>,
) -> astra_tools::ToolResult
where
    L: ServerLocalToolTransport + ?Sized,
{
    if cancel_token.is_some_and(|token| token.is_cancelled()) {
        return cancelled_runtime_tool_result_for_binding(
            result_workspace,
            result_executor,
            &request.tool_name,
            binding,
            result_transport,
            false,
        );
    }
    let execution = local_transport.execute_server_local_tool(request);
    if let Some(token) = cancel_token {
        tokio::select! {
            _ = token.cancelled() => cancelled_runtime_tool_result_for_binding(
                result_workspace,
                result_executor,
                &request.tool_name,
                binding,
                result_transport,
                true,
            ),
            result = execution => result,
        }
    } else {
        execution.await
    }
}
