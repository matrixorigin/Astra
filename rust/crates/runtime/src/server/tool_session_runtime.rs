use std::future::Future;
use std::path::Path;
use std::pin::Pin;
use std::sync::Mutex;
use std::sync::atomic::Ordering;

use astra_core::SharedPool;
use astra_tools::ToolExecutor;
use astra_turn_core::file_edit_journal::FileEditJournal;
use serde_json::Value;

use crate::server::server_tool_executor::ServerToolExecutor;
use crate::server::tool_execution_result::tool_result_from_output;
use crate::server::tool_file_runtime::execute_rollback_file_edits;
use crate::server::tool_session_history;

pub(crate) type SessionToolFuture<'a> =
    Pin<Box<dyn Future<Output = astra_tools::ToolResult> + Send + 'a>>;

pub(crate) struct SessionToolRuntimeContext<'a> {
    pub(crate) user_id: &'a str,
    pub(crate) session_id: &'a str,
    pub(crate) context_manifest_pool: Option<&'a SharedPool>,
    pub(crate) workspace_root: &'a Path,
    pub(crate) turn_index: u32,
    pub(crate) file_journal: &'a Mutex<FileEditJournal>,
}

pub(crate) async fn execute_session_tool<'a, Config, Sleep>(
    context: SessionToolRuntimeContext<'a>,
    args: &'a Value,
    config: Config,
    sleep: Sleep,
) -> astra_tools::ToolResult
where
    Config: FnOnce(&Value) -> String,
    Sleep: FnOnce(&'a Value) -> SessionToolFuture<'a>,
{
    let action = match args.get("action") {
        Some(Value::String(action)) => action.as_str(),
        Some(_) => {
            return astra_tools::ToolResult::error(
                "Error: field `action` for `session` must be a string".to_string(),
            );
        }
        None => return missing_action_result(),
    };

    let session_history_context = || {
        tool_session_history::context(
            context.user_id,
            context.session_id,
            context.context_manifest_pool,
        )
    };

    match action {
        "config" => tool_result_from_output(config(args)),
        "rollback_edits" => tool_result_from_output(execute_rollback_file_edits(
            context.workspace_root,
            args,
            context.turn_index,
            context.file_journal,
        )),
        "sleep" => sleep(args).await,
        "history_page" => tool_session_history::history_page(session_history_context(), args).await,
        "history_search" => {
            tool_session_history::history_search(session_history_context(), args).await
        }
        "history_around" => {
            tool_session_history::history_around(session_history_context(), args).await
        }
        "" => missing_action_result(),
        other => astra_tools::ToolResult::error(format!(
            "Error: unknown `session` action '{other}'. For plan mode use `enter_plan_mode` / `exit_plan_mode`."
        )),
    }
}

fn missing_action_result() -> astra_tools::ToolResult {
    astra_tools::ToolResult::error(
        "Error: missing required parameter `action` for `session`. Use: config, rollback_edits, sleep, history_page, history_search, history_around. Use dedicated tools: prioritize_tool, deprioritize_tool, compress_context, ask_user, enter_plan_mode, exit_plan_mode."
            .to_string(),
    )
}

/// Server-side entry point for the `session` tool. Constructs the runtime
/// context and executor-owned closures, then delegates to [`execute_session_tool`].
pub(super) async fn execute_with_executor(
    executor: &ServerToolExecutor,
    args: &Value,
) -> astra_tools::ToolResult {
    execute_session_tool(
        SessionToolRuntimeContext {
            user_id: &executor.user_id,
            session_id: &executor.session_id,
            context_manifest_pool: executor.context_manifest_pool.as_ref(),
            workspace_root: &executor.workspace_root,
            turn_index: executor.journal_turn_index.load(Ordering::Relaxed),
            file_journal: executor.file_journal.as_ref(),
        },
        args,
        |args| executor.adjust_config(args),
        |args| Box::pin(executor.default_executor.execute("sleep", args)),
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn test_context<'a>(
        workspace_root: &'a Path,
        file_journal: &'a Mutex<FileEditJournal>,
    ) -> SessionToolRuntimeContext<'a> {
        SessionToolRuntimeContext {
            user_id: "user-1",
            session_id: "session-1",
            context_manifest_pool: None,
            workspace_root,
            turn_index: 7,
            file_journal,
        }
    }

    fn unreachable_text_action(_: &Value) -> String {
        panic!("session text action should not be called")
    }

    fn unreachable_async_action<'a>(_: &'a Value) -> SessionToolFuture<'a> {
        Box::pin(async { panic!("session async action should not be called") })
    }

    #[tokio::test]
    async fn session_tool_rejects_missing_action() {
        let dir = tempfile::tempdir().expect("tempdir");
        let journal = Mutex::new(FileEditJournal::new(10));
        let result = execute_session_tool(
            test_context(dir.path(), &journal),
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
        let dir = tempfile::tempdir().expect("tempdir");
        let journal = Mutex::new(FileEditJournal::new(10));
        let result = execute_session_tool(
            test_context(dir.path(), &journal),
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
        let dir = tempfile::tempdir().expect("tempdir");
        let journal = Mutex::new(FileEditJournal::new(10));
        let result = execute_session_tool(
            test_context(dir.path(), &journal),
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
