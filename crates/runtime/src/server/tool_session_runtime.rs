use std::future::Future;
use std::pin::Pin;

use astra_tools::ToolExecutor;
use astra_tools::session_tool_contract::{SessionAction, session_action_from_args};
use serde_json::Value;

use crate::server::runtime_tool_executor::RuntimeToolExecutor;
use crate::server::tool_execution_result::tool_result_from_output;

pub(crate) type SessionToolFuture<'a> =
    Pin<Box<dyn Future<Output = astra_tools::ToolResult> + Send + 'a>>;
pub(crate) type SessionConfigFuture<'a> = Pin<Box<dyn Future<Output = String> + Send + 'a>>;

pub(crate) async fn execute_session_tool<'a, Config, Sleep>(
    args: &'a Value,
    config: Config,
    sleep: Sleep,
) -> astra_tools::ToolResult
where
    Config: FnOnce(&'a Value) -> SessionConfigFuture<'a>,
    Sleep: FnOnce(&'a Value) -> SessionToolFuture<'a>,
{
    let action = match session_action_from_args(args) {
        Ok(action) => action,
        Err(error) => return astra_tools::ToolResult::error(format!("Error: {error}")),
    };

    match action {
        SessionAction::Config => tool_result_from_output(config(args).await),
        SessionAction::Sleep => sleep(args).await,
    }
}

/// Server-side entry point for the `session` tool with executor-owned actions.
pub(super) async fn execute_with_executor(
    executor: &RuntimeToolExecutor,
    args: &Value,
) -> astra_tools::ToolResult {
    execute_session_tool(
        args,
        |args| Box::pin(executor.adjust_config(args)),
        |args| Box::pin(executor.default_executor.execute("sleep", args)),
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn unreachable_text_action(_: &Value) -> SessionConfigFuture<'_> {
        panic!("session text action should not be called")
    }

    fn unreachable_async_action<'a>(_: &'a Value) -> SessionToolFuture<'a> {
        Box::pin(async { panic!("session async action should not be called") })
    }

    #[tokio::test]
    async fn session_tool_rejects_missing_action() {
        let result = execute_session_tool(
            &json!({}),
            unreachable_text_action,
            unreachable_async_action,
        )
        .await;

        assert!(result.is_error, "{result:?}");
        assert!(result.output.contains("missing required parameter"));
        assert!(!result.output.contains("prioritize"));
        assert!(!result.output.contains("ask_user"));
    }

    #[tokio::test]
    async fn session_tool_rejects_non_string_action() {
        let result = execute_session_tool(
            &json!({"action": 7}),
            unreachable_text_action,
            unreachable_async_action,
        )
        .await;

        assert!(result.is_error, "{result:?}");
        assert!(result.output.contains("must be a string"));
    }

    #[tokio::test]
    async fn session_tool_routes_sleep_to_runtime_action() {
        let result = execute_session_tool(
            &json!({"action": "sleep", "duration_ms": 0}),
            unreachable_text_action,
            |args| {
                assert_eq!(args.get("duration_ms").and_then(Value::as_i64), Some(0));
                Box::pin(async { astra_tools::ToolResult::text("sleep ok".to_string()) })
            },
        )
        .await;

        assert!(!result.is_error, "{result:?}");
        assert_eq!(result.output, "sleep ok");
    }
}
