use std::future::Future;
use std::pin::Pin;

use astra_core::SharedPool;
use astra_tools::ToolExecutor;
use astra_tools::session_tool_contract::{SessionAction, session_action_from_args};
use serde_json::Value;

use crate::server::runtime_tool_executor::RuntimeToolExecutor;
use crate::server::tool_execution_result::tool_result_from_output;
use crate::server::tool_session_history;

pub(crate) type SessionToolFuture<'a> =
    Pin<Box<dyn Future<Output = astra_tools::ToolResult> + Send + 'a>>;
pub(crate) type SessionConfigFuture<'a> = Pin<Box<dyn Future<Output = String> + Send + 'a>>;

pub(crate) struct SessionToolRuntimeContext<'a> {
    pub(crate) user_id: &'a str,
    pub(crate) session_id: &'a str,
    pub(crate) context_manifest_pool: Option<&'a SharedPool>,
}

pub(crate) async fn execute_session_tool<'a, Config, Sleep>(
    context: SessionToolRuntimeContext<'a>,
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

    let session_history_context = || {
        tool_session_history::context(
            context.user_id,
            context.session_id,
            context.context_manifest_pool,
        )
    };

    match action {
        SessionAction::Config => tool_result_from_output(config(args).await),
        SessionAction::Sleep => sleep(args).await,
        SessionAction::HistoryPage => {
            tool_session_history::history_page(session_history_context(), args).await
        }
        SessionAction::HistorySearch => {
            tool_session_history::history_search(session_history_context(), args).await
        }
        SessionAction::HistoryAround => {
            tool_session_history::history_around(session_history_context(), args).await
        }
    }
}

/// Server-side entry point for the `session` tool. Constructs the runtime
/// context and executor-owned closures, then delegates to [`execute_session_tool`].
pub(super) async fn execute_with_executor(
    executor: &RuntimeToolExecutor,
    args: &Value,
) -> astra_tools::ToolResult {
    execute_session_tool(
        SessionToolRuntimeContext {
            user_id: &executor.user_id,
            session_id: &executor.session_id,
            context_manifest_pool: executor.context_manifest_pool.as_ref(),
        },
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

    fn test_context<'a>() -> SessionToolRuntimeContext<'a> {
        SessionToolRuntimeContext {
            user_id: "user-1",
            session_id: "session-1",
            context_manifest_pool: None,
        }
    }

    fn unreachable_text_action(_: &Value) -> SessionConfigFuture<'_> {
        panic!("session text action should not be called")
    }

    fn unreachable_async_action<'a>(_: &'a Value) -> SessionToolFuture<'a> {
        Box::pin(async { panic!("session async action should not be called") })
    }

    #[tokio::test]
    async fn session_tool_rejects_missing_action() {
        let result = execute_session_tool(
            test_context(),
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
            test_context(),
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
            test_context(),
            &json!({"action": "sleep", "seconds": 0}),
            unreachable_text_action,
            |args| {
                assert_eq!(args.get("seconds").and_then(Value::as_i64), Some(0));
                Box::pin(async { astra_tools::ToolResult::text("sleep ok".to_string()) })
            },
        )
        .await;

        assert!(!result.is_error, "{result:?}");
        assert_eq!(result.output, "sleep ok");
    }
}
