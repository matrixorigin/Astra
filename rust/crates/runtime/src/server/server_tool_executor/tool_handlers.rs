use std::sync::atomic::Ordering;

use astra_turn_core::tool::schema::{retain_tool_schemas_by_names, tool_schema_name};
use async_trait::async_trait;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use astra_tools::ToolExecutor;
use astra_tools::tool_engine::{
    DynamicToolHandler, NotifyToolHandler, ToolEngine, ToolHandler, WebSearchToolHandler,
};

use super::ServerToolExecutor;
use crate::server::tool_agent_info::{AgentInfoIdentity, render_agent_info};
use crate::server::tool_agent_runtime::{execute_agent_fanout_tool, execute_agent_tool};
use crate::server::tool_database_snapshots::{execute_mo_query, rollback_database_snapshots};
use crate::server::tool_execution_result::tool_result_from_output;
use crate::server::tool_file_runtime::{
    execute_publish_artifact, execute_server_delete_file, execute_server_multi_edit,
    execute_server_run_script, execute_server_str_replace, execute_server_write_file,
};
use crate::server::tool_introspect::handle_introspect;
use crate::server::tool_local_execution::memory_args_with_context;
use crate::server::tool_plan_gate::{execute_enter_plan_mode, execute_exit_plan_mode};
use crate::server::tool_session_state_rollback::{
    self, RollbackSessionStateContext, SessionStateRestoreContext,
};

/// Register a tool handler and log an error on failure (duplicate name).
///
/// Eliminates ~200 lines of repetitive `if let Err(error)` + `tracing::error!`
/// boilerplate in [`server_tool_engine`].
macro_rules! register_handler_or_log {
    ($engine:expr, $name:expr, $handler:expr) => {
        if let Err(error) = $engine.register_handler($name, $handler) {
            tracing::error!(
                target: "astra_runtime::tool_engine",
                tool = $name,
                error = %error,
                "failed to register built-in server tool handler"
            );
        }
    };
}

pub(super) fn server_tool_engine() -> ToolEngine<ServerToolExecutor> {
    let mut engine = ToolEngine::new();

    register_handler_or_log!(engine, "notify", NotifyToolHandler);
    register_handler_or_log!(engine, "web_search", WebSearchToolHandler);

    for name in [
        "web_fetch",
        "read_file",
        "list_dir",
        "grep",
        "glob",
        "symbols",
    ] {
        register_handler_or_log!(engine, name, DefaultExecutorToolHandler { name });
    }

    register_handler_or_log!(engine, "write_file", WriteFileToolHandler);
    register_handler_or_log!(engine, "str_replace", StrReplaceToolHandler);
    register_handler_or_log!(engine, "bash", BashToolHandler);
    register_handler_or_log!(engine, "git", DefaultExecutorToolHandler { name: "git" });
    register_handler_or_log!(
        engine,
        "github",
        DefaultExecutorToolHandler { name: "github" }
    );
    register_handler_or_log!(engine, "get_agent_info", GetAgentInfoToolHandler);
    register_handler_or_log!(engine, "tool_search", ToolSearchToolHandler);
    register_handler_or_log!(engine, "memory", MemoryToolHandler);
    register_handler_or_log!(engine, "session", SessionToolHandler);
    register_handler_or_log!(engine, "task", TaskToolHandler);
    register_handler_or_log!(engine, "agent", AgentToolHandler);
    register_handler_or_log!(engine, "agent_fanout", AgentFanoutToolHandler);
    register_handler_or_log!(engine, "ask_user", AskUserToolHandler);
    register_handler_or_log!(engine, "enter_plan_mode", EnterPlanModeToolHandler);
    register_handler_or_log!(engine, "exit_plan_mode", ExitPlanModeToolHandler);
    register_handler_or_log!(engine, "introspect", IntrospectToolHandler);
    register_handler_or_log!(engine, "prioritize_tool", PrioritizeToolHandler);
    register_handler_or_log!(engine, "deprioritize_tool", DeprioritizeToolHandler);
    register_handler_or_log!(engine, "compress_context", CompressContextToolHandler);
    register_handler_or_log!(
        engine,
        "rollback_session_state",
        RollbackSessionStateToolHandler
    );
    register_handler_or_log!(engine, "mo", MoToolHandler);
    register_handler_or_log!(engine, "mo_query", MoQueryToolHandler);
    register_handler_or_log!(
        engine,
        "rollback_database_snapshots",
        RollbackDatabaseSnapshotsToolHandler
    );
    register_handler_or_log!(engine, "publish_artifact", PublishArtifactToolHandler);
    register_handler_or_log!(engine, "run_script", RunScriptToolHandler);

    if let Err(error) = engine.register_prefix_handler("mcp__", McpToolHandler) {
        tracing::error!(
            target: "astra_runtime::tool_engine",
            error = %error,
            "failed to register dynamic server tool handler"
        );
    }

    engine
}

#[derive(Debug, Clone, Copy, Default)]
struct GetAgentInfoToolHandler;

#[async_trait]
impl ToolHandler<ServerToolExecutor> for GetAgentInfoToolHandler {
    async fn execute(
        &self,
        context: &ServerToolExecutor,
        args: &Value,
        _cancel_token: Option<&CancellationToken>,
    ) -> astra_tools::ToolResult {
        let schemas = context.capability_filtered_server_tool_schemas();
        let tool_names: Vec<&str> = schemas.iter().filter_map(tool_schema_name).collect();
        tool_result_from_output(render_agent_info(
            args,
            AgentInfoIdentity {
                name: "astra",
                version: env!("CARGO_PKG_VERSION"),
                runtime: "cloud-server",
                user_id: &context.user_id,
                session_id: &context.session_id,
                workspace: context.workspace_root.display().to_string(),
            },
            &tool_names,
        ))
    }
}

#[derive(Debug, Clone, Copy, Default)]
struct ToolSearchToolHandler;

#[async_trait]
impl ToolHandler<ServerToolExecutor> for ToolSearchToolHandler {
    async fn execute(
        &self,
        context: &ServerToolExecutor,
        args: &Value,
        _cancel_token: Option<&CancellationToken>,
    ) -> astra_tools::ToolResult {
        let mut pool = context.capability_filtered_server_tool_schemas();
        pool.extend(context.plugin_schemas_snapshot("plugin_schemas_tool_search"));
        let Some(searchable_names) = context.current_searchable_tool_names() else {
            return tool_result_from_output(astra_tools::tool_search::tool_search(&pool, args));
        };
        retain_tool_schemas_by_names(&mut pool, &searchable_names);
        tool_result_from_output(astra_tools::tool_search::tool_search(&pool, args))
    }
}

#[derive(Debug, Clone, Copy, Default)]
struct MemoryToolHandler;

#[async_trait]
impl ToolHandler<ServerToolExecutor> for MemoryToolHandler {
    async fn execute(
        &self,
        context: &ServerToolExecutor,
        args: &Value,
        cancel_token: Option<&CancellationToken>,
    ) -> astra_tools::ToolResult {
        // P2-C: Cooperative cancellation check at heavy handler entry
        if cancel_token.is_some_and(|t| t.is_cancelled()) {
            return astra_tools::ToolResult::error(
                "Memory tool not executed: run was cancelled".to_string(),
            );
        }
        let Some(op) = args.get("action").and_then(Value::as_str) else {
            return astra_tools::ToolResult::error(
                "Error: missing required parameter `action`. Use one of: remember, recall, expand, forget, update, focus, reflect, profile, feedback".to_string(),
            );
        };
        let isolated_args = memory_args_with_context(
            args,
            &context.session_id,
            &context.user_id,
            context.journal_turn_index.load(Ordering::Relaxed),
        );
        let output = context.memoria_client.call(op, &isolated_args).await;
        if output.starts_with("Error") {
            astra_tools::ToolResult::error(output)
        } else {
            astra_tools::ToolResult::text(output)
        }
    }
}

#[derive(Debug, Clone, Copy, Default)]
struct SessionToolHandler;

#[async_trait]
impl ToolHandler<ServerToolExecutor> for SessionToolHandler {
    async fn execute(
        &self,
        context: &ServerToolExecutor,
        args: &Value,
        cancel_token: Option<&CancellationToken>,
    ) -> astra_tools::ToolResult {
        // P2-C: Cooperative cancellation check at heavy handler entry
        if cancel_token.is_some_and(|t| t.is_cancelled()) {
            return astra_tools::ToolResult::error(
                "Session tool not executed: run was cancelled".to_string(),
            );
        }
        crate::server::tool_session_runtime::execute_with_executor(context, args).await
    }
}

#[derive(Debug, Clone, Copy, Default)]
struct TaskToolHandler;

#[async_trait]
impl ToolHandler<ServerToolExecutor> for TaskToolHandler {
    async fn execute(
        &self,
        context: &ServerToolExecutor,
        args: &Value,
        cancel_token: Option<&CancellationToken>,
    ) -> astra_tools::ToolResult {
        // P2-C: Cooperative cancellation check at heavy handler entry
        if cancel_token.is_some_and(|t| t.is_cancelled()) {
            return astra_tools::ToolResult::error(
                "Task tool not executed: run was cancelled".to_string(),
            );
        }
        crate::server::tool_task_runtime::execute_with_executor(context, args).await
    }
}

#[derive(Debug, Clone, Copy, Default)]
struct AgentToolHandler;

#[async_trait]
impl ToolHandler<ServerToolExecutor> for AgentToolHandler {
    async fn execute(
        &self,
        context: &ServerToolExecutor,
        args: &Value,
        cancel_token: Option<&CancellationToken>,
    ) -> astra_tools::ToolResult {
        // P2-C: Cooperative cancellation check at heavy handler entry
        if cancel_token.is_some_and(|t| t.is_cancelled()) {
            return astra_tools::ToolResult::error(
                "Agent tool not executed: run was cancelled".to_string(),
            );
        }
        execute_agent_tool(
            &context.default_executor,
            context.agent_tool_context.as_ref(),
            args,
        )
        .await
    }
}

#[derive(Debug, Clone, Copy, Default)]
struct AgentFanoutToolHandler;

#[async_trait]
impl ToolHandler<ServerToolExecutor> for AgentFanoutToolHandler {
    async fn execute(
        &self,
        context: &ServerToolExecutor,
        args: &Value,
        cancel_token: Option<&CancellationToken>,
    ) -> astra_tools::ToolResult {
        // P2-C: Cooperative cancellation check at heavy handler entry
        if cancel_token.is_some_and(|t| t.is_cancelled()) {
            return astra_tools::ToolResult::error(
                "Agent fanout not executed: run was cancelled".to_string(),
            );
        }
        execute_agent_fanout_tool(context.agent_tool_context.as_ref(), args).await
    }
}

#[derive(Debug, Clone, Copy, Default)]
struct AskUserToolHandler;

#[async_trait]
impl ToolHandler<ServerToolExecutor> for AskUserToolHandler {
    async fn execute(
        &self,
        context: &ServerToolExecutor,
        args: &Value,
        cancel_token: Option<&CancellationToken>,
    ) -> astra_tools::ToolResult {
        // P2-C: 重型 handler 入口处合作式取消检查
        if cancel_token.is_some_and(|t| t.is_cancelled()) {
            return astra_tools::ToolResult::error(
                "Ask user not executed: run was cancelled".to_string(),
            );
        }
        context.server_ask_user(args).await
    }
}

#[derive(Debug, Clone, Copy, Default)]
struct EnterPlanModeToolHandler;

#[async_trait]
impl ToolHandler<ServerToolExecutor> for EnterPlanModeToolHandler {
    async fn execute(
        &self,
        context: &ServerToolExecutor,
        args: &Value,
        cancel_token: Option<&CancellationToken>,
    ) -> astra_tools::ToolResult {
        // P2-C: Cooperative cancellation check at heavy handler entry
        if cancel_token.is_some_and(|t| t.is_cancelled()) {
            return astra_tools::ToolResult::error(
                "Enter plan mode not executed: run was cancelled".to_string(),
            );
        }
        astra_tools::ToolResult::text(
            execute_enter_plan_mode(
                context.plan_repo.as_ref(),
                &context.session_id,
                &context.user_id,
                context.plan_mode_cache.as_ref(),
                context.plan_resume_hint_handle.as_ref(),
                args,
            )
            .await,
        )
    }
}

#[derive(Debug, Clone, Copy, Default)]
struct ExitPlanModeToolHandler;

#[async_trait]
impl ToolHandler<ServerToolExecutor> for ExitPlanModeToolHandler {
    async fn execute(
        &self,
        context: &ServerToolExecutor,
        args: &Value,
        cancel_token: Option<&CancellationToken>,
    ) -> astra_tools::ToolResult {
        // P2-C: Cooperative cancellation check at heavy handler entry
        if cancel_token.is_some_and(|t| t.is_cancelled()) {
            return astra_tools::ToolResult::error(
                "Exit plan mode not executed: run was cancelled".to_string(),
            );
        }
        astra_tools::ToolResult::text(
            execute_exit_plan_mode(
                context.plan_repo.as_ref(),
                &context.user_id,
                &context.session_id,
                context.plan_mode_cache.as_ref(),
                context.plan_resume_hint_handle.as_ref(),
                args,
            )
            .await,
        )
    }
}

#[derive(Debug, Clone, Copy, Default)]
struct IntrospectToolHandler;

#[async_trait]
impl ToolHandler<ServerToolExecutor> for IntrospectToolHandler {
    async fn execute(
        &self,
        context: &ServerToolExecutor,
        args: &Value,
        _cancel_token: Option<&CancellationToken>,
    ) -> astra_tools::ToolResult {
        tool_result_from_output(handle_introspect(
            args,
            &context.session_id,
            &context.introspect_snapshot,
        ))
    }
}

#[derive(Debug, Clone, Copy)]
struct DefaultExecutorToolHandler {
    name: &'static str,
}

#[async_trait]
impl ToolHandler<ServerToolExecutor> for DefaultExecutorToolHandler {
    async fn execute(
        &self,
        context: &ServerToolExecutor,
        args: &Value,
        cancel_token: Option<&CancellationToken>,
    ) -> astra_tools::ToolResult {
        // P2-C: Cooperative cancellation check at heavy handler entry
        if cancel_token.is_some_and(|t| t.is_cancelled()) {
            return astra_tools::ToolResult::error(format!(
                "Tool '{}' not executed: run was cancelled",
                self.name
            ));
        }
        context.default_executor.execute(self.name, args).await
    }
}

#[derive(Debug, Clone, Copy, Default)]
struct WriteFileToolHandler;

#[async_trait]
impl ToolHandler<ServerToolExecutor> for WriteFileToolHandler {
    async fn execute(
        &self,
        context: &ServerToolExecutor,
        args: &Value,
        cancel_token: Option<&CancellationToken>,
    ) -> astra_tools::ToolResult {
        // P2-C: 重型 handler 入口处合作式取消检查
        if cancel_token.is_some_and(|t| t.is_cancelled()) {
            return astra_tools::ToolResult::error(
                "Write file not executed: run was cancelled".to_string(),
            );
        }
        let turn_index = context.journal_turn_index.load(Ordering::Relaxed);
        if args
            .get("delete")
            .and_then(|value| value.as_bool())
            .unwrap_or(false)
        {
            tool_result_from_output(execute_server_delete_file(
                &context.workspace_root,
                args,
                turn_index,
                context.file_journal.as_ref(),
            ))
        } else {
            tool_result_from_output(execute_server_write_file(
                &context.workspace_root,
                args,
                turn_index,
                context.file_journal.as_ref(),
            ))
        }
    }
}

#[derive(Debug, Clone, Copy, Default)]
struct StrReplaceToolHandler;

#[async_trait]
impl ToolHandler<ServerToolExecutor> for StrReplaceToolHandler {
    async fn execute(
        &self,
        context: &ServerToolExecutor,
        args: &Value,
        cancel_token: Option<&CancellationToken>,
    ) -> astra_tools::ToolResult {
        // P2-C: 重型 handler 入口处合作式取消检查
        if cancel_token.is_some_and(|t| t.is_cancelled()) {
            return astra_tools::ToolResult::error(
                "Str replace not executed: run was cancelled".to_string(),
            );
        }
        let args = match astra_tools::fs_ops::normalize_str_replace_args(args) {
            Ok(args) => args,
            Err(error) => return tool_result_from_output(error),
        };
        let turn_index = context.journal_turn_index.load(Ordering::Relaxed);
        if args
            .get("edits")
            .and_then(|value| value.as_array())
            .is_some()
        {
            tool_result_from_output(execute_server_multi_edit(
                &context.workspace_root,
                &args,
                turn_index,
                context.file_journal.as_ref(),
            ))
        } else {
            tool_result_from_output(execute_server_str_replace(
                &context.workspace_root,
                &args,
                turn_index,
                context.file_journal.as_ref(),
            ))
        }
    }
}

#[derive(Debug, Clone, Copy, Default)]
struct BashToolHandler;

#[async_trait]
impl ToolHandler<ServerToolExecutor> for BashToolHandler {
    async fn execute(
        &self,
        context: &ServerToolExecutor,
        args: &Value,
        cancel_token: Option<&CancellationToken>,
    ) -> astra_tools::ToolResult {
        // P2-C: Cooperative cancellation check at heavy handler entry
        if cancel_token.is_some_and(|t| t.is_cancelled()) {
            return astra_tools::ToolResult::error(
                "Bash tool not executed: run was cancelled".to_string(),
            );
        }
        context.server_bash(args).await
    }
}

#[derive(Debug, Clone, Copy, Default)]
struct PrioritizeToolHandler;

#[async_trait]
impl ToolHandler<ServerToolExecutor> for PrioritizeToolHandler {
    async fn execute(
        &self,
        context: &ServerToolExecutor,
        args: &Value,
        _cancel_token: Option<&CancellationToken>,
    ) -> astra_tools::ToolResult {
        tool_result_from_output(context.prioritize_tool(args))
    }
}

#[derive(Debug, Clone, Copy, Default)]
struct DeprioritizeToolHandler;

#[async_trait]
impl ToolHandler<ServerToolExecutor> for DeprioritizeToolHandler {
    async fn execute(
        &self,
        context: &ServerToolExecutor,
        args: &Value,
        _cancel_token: Option<&CancellationToken>,
    ) -> astra_tools::ToolResult {
        tool_result_from_output(context.deprioritize_tool(args))
    }
}

#[derive(Debug, Clone, Copy, Default)]
struct CompressContextToolHandler;

#[async_trait]
impl ToolHandler<ServerToolExecutor> for CompressContextToolHandler {
    async fn execute(
        &self,
        context: &ServerToolExecutor,
        args: &Value,
        _cancel_token: Option<&CancellationToken>,
    ) -> astra_tools::ToolResult {
        tool_result_from_output(context.compress_context(args))
    }
}

#[derive(Debug, Clone, Copy, Default)]
struct RollbackSessionStateToolHandler;

#[async_trait]
impl ToolHandler<ServerToolExecutor> for RollbackSessionStateToolHandler {
    async fn execute(
        &self,
        context: &ServerToolExecutor,
        args: &Value,
        cancel_token: Option<&CancellationToken>,
    ) -> astra_tools::ToolResult {
        // P2-C: Cooperative cancellation check at heavy handler entry
        if cancel_token.is_some_and(|t| t.is_cancelled()) {
            return astra_tools::ToolResult::error(
                "Rollback session state not executed: run was cancelled".to_string(),
            );
        }
        tool_result_from_output(
            tool_session_state_rollback::execute_rollback_session_state(
                RollbackSessionStateContext {
                    journal: context.session_state_journal.as_ref(),
                    current_turn_index: context.journal_turn_index.load(Ordering::Relaxed),
                    restore_context: SessionStateRestoreContext {
                        session_id: &context.session_id,
                        observability_session: context.observability_session.as_ref(),
                        config: &context.session_config.inner,
                        task_manager: &context.task_manager(),
                    },
                },
                args,
                || context.publish_current_workspace("server_tool_executor:rollback_session_state"),
            )
            .await,
        )
    }
}

#[derive(Debug, Clone, Copy, Default)]
struct MoToolHandler;

#[async_trait]
impl ToolHandler<ServerToolExecutor> for MoToolHandler {
    async fn execute(
        &self,
        context: &ServerToolExecutor,
        args: &Value,
        cancel_token: Option<&CancellationToken>,
    ) -> astra_tools::ToolResult {
        // P2-C: Cooperative cancellation check at heavy handler entry
        if cancel_token.is_some_and(|t| t.is_cancelled()) {
            return astra_tools::ToolResult::error(
                "MatrixOne tool not executed: run was cancelled".to_string(),
            );
        }
        let action = args
            .get("action")
            .and_then(|value| value.as_str())
            .unwrap_or("query");
        match action {
            "query" => execute_mo_query(
                context.database_snapshot_journal.as_ref(),
                args,
                context.journal_turn_index.load(Ordering::Relaxed),
            ),
            "snapshot" | "branch" => {
                context
                    .default_executor
                    .execute(&format!("mo_{action}"), args)
                    .await
            }
            other => tool_result_from_output(format!(
                "Error: Unknown mo action: '{other}'. Use: query, snapshot, branch"
            )),
        }
    }
}

#[derive(Debug, Clone, Copy, Default)]
struct MoQueryToolHandler;

#[async_trait]
impl ToolHandler<ServerToolExecutor> for MoQueryToolHandler {
    async fn execute(
        &self,
        context: &ServerToolExecutor,
        args: &Value,
        cancel_token: Option<&CancellationToken>,
    ) -> astra_tools::ToolResult {
        // P2-C: Cooperative cancellation check at heavy handler entry
        if cancel_token.is_some_and(|t| t.is_cancelled()) {
            return astra_tools::ToolResult::error(
                "Mo query not executed: run was cancelled".to_string(),
            );
        }
        execute_mo_query(
            context.database_snapshot_journal.as_ref(),
            args,
            context.journal_turn_index.load(Ordering::Relaxed),
        )
    }
}

#[derive(Debug, Clone, Copy, Default)]
struct RollbackDatabaseSnapshotsToolHandler;

#[async_trait]
impl ToolHandler<ServerToolExecutor> for RollbackDatabaseSnapshotsToolHandler {
    async fn execute(
        &self,
        context: &ServerToolExecutor,
        args: &Value,
        _cancel_token: Option<&CancellationToken>,
    ) -> astra_tools::ToolResult {
        tool_result_from_output(rollback_database_snapshots(
            context.database_snapshot_journal.as_ref(),
            args,
            context.journal_turn_index.load(Ordering::Relaxed),
        ))
    }
}

#[derive(Debug, Clone, Copy, Default)]
struct PublishArtifactToolHandler;

#[async_trait]
impl ToolHandler<ServerToolExecutor> for PublishArtifactToolHandler {
    async fn execute(
        &self,
        context: &ServerToolExecutor,
        args: &Value,
        cancel_token: Option<&CancellationToken>,
    ) -> astra_tools::ToolResult {
        // P2-C: 重型 handler 入口处合作式取消检查
        if cancel_token.is_some_and(|t| t.is_cancelled()) {
            return astra_tools::ToolResult::error(
                "Publish artifact not executed: run was cancelled".to_string(),
            );
        }
        execute_publish_artifact(
            args,
            context.workspace_artifact_store.as_ref(),
            &context.workspace_root,
            &context.session_id,
            &context.user_id,
            context.journal_turn_index.load(Ordering::Relaxed),
        )
        .await
    }
}

#[derive(Debug, Clone, Copy, Default)]
struct RunScriptToolHandler;

#[async_trait]
impl ToolHandler<ServerToolExecutor> for RunScriptToolHandler {
    async fn execute(
        &self,
        context: &ServerToolExecutor,
        args: &Value,
        cancel_token: Option<&CancellationToken>,
    ) -> astra_tools::ToolResult {
        // P2-C: Cooperative cancellation check at heavy handler entry
        if cancel_token.is_some_and(|t| t.is_cancelled()) {
            return astra_tools::ToolResult::error(
                "Run script not executed: run was cancelled".to_string(),
            );
        }
        execute_server_run_script(args, context, &context.workspace_root).await
    }
}

#[derive(Debug, Clone, Copy, Default)]
struct McpToolHandler;

#[async_trait]
impl DynamicToolHandler<ServerToolExecutor> for McpToolHandler {
    async fn execute(
        &self,
        name: &str,
        context: &ServerToolExecutor,
        args: &Value,
        cancel_token: Option<&CancellationToken>,
    ) -> astra_tools::ToolResult {
        // P2-C: 重型 handler 入口处合作式取消检查
        if cancel_token.is_some_and(|t| t.is_cancelled()) {
            return astra_tools::ToolResult::error(format!(
                "MCP tool '{}' not executed: run was cancelled",
                name
            ));
        }
        context.execute_mcp_tool(name, args).await
    }
}
