use std::sync::atomic::Ordering;

use astra_turn_core::tool::schema::tool_schema_name;
use async_trait::async_trait;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use astra_tools::ToolExecutor;
use astra_tools::executor::SERVER_DIRECT_DEFAULT_EXECUTOR_TOOL_NAMES;
use astra_tools::tool_engine::{
    DynamicToolHandler, NotifyToolHandler, ToolEngine, ToolHandler, ToolInvocationMetadata,
};

use super::RuntimeToolExecutor;
use crate::server::tool_agent_info::{AgentInfoIdentity, render_agent_info};
use crate::server::tool_agent_runtime::{execute_agent_fanout_tool, execute_agent_tool};
use crate::server::tool_database_snapshots::{execute_mo_query, rollback_database_snapshots};
use crate::server::tool_execution_result::tool_result_from_output;
use crate::server::tool_file_runtime::{
    execute_publish_artifact, execute_rollback_file_edits, execute_server_delete_file,
    execute_server_multi_edit, execute_server_run_script, execute_server_str_replace,
    execute_server_write_file,
};
use crate::server::tool_introspect::{current_introspect_snapshot, render_introspect_snapshot};
use crate::server::tool_local_execution::memory_args_with_context;
use crate::server::tool_plan_gate::{execute_enter_plan_mode, execute_exit_plan_mode};
use crate::server::tool_session_state_rollback::{
    self, RollbackSessionStateContext, SessionStateRestoreContext,
};

/// Register a tool handler and log an error on failure (duplicate name).
///
/// Eliminates ~200 lines of repetitive `if let Err(error)` + `tracing::error!`
/// boilerplate in [`runtime_tool_engine`].
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

pub(super) fn runtime_tool_engine() -> ToolEngine<RuntimeToolExecutor> {
    let mut engine = ToolEngine::new();

    register_handler_or_log!(engine, "notify", NotifyToolHandler);
    register_handler_or_log!(
        engine,
        "web_search",
        DefaultExecutorToolHandler { name: "web_search" }
    );

    for name in SERVER_DIRECT_DEFAULT_EXECUTOR_TOOL_NAMES {
        register_handler_or_log!(engine, *name, DefaultExecutorToolHandler { name });
    }

    register_handler_or_log!(engine, "write_file", WriteFileToolHandler);
    register_handler_or_log!(engine, "str_replace", StrReplaceToolHandler);
    register_handler_or_log!(engine, "rollback_file_edits", RollbackFileEditsToolHandler);
    register_handler_or_log!(engine, "bash", BashToolHandler);
    register_handler_or_log!(engine, "get_agent_info", GetAgentInfoToolHandler);
    register_handler_or_log!(engine, "tool_search", ToolSearchToolHandler);
    register_handler_or_log!(engine, "memory", MemoryToolHandler);
    register_handler_or_log!(engine, "session", SessionToolHandler);
    register_handler_or_log!(engine, "task_board", TaskBoardToolHandler);
    register_handler_or_log!(engine, "agent", AgentToolHandler);
    register_handler_or_log!(engine, "agent_fanout", AgentFanoutToolHandler);
    register_handler_or_log!(engine, "ask_user", AskUserToolHandler);
    register_handler_or_log!(engine, "enter_plan_mode", EnterPlanModeToolHandler);
    register_handler_or_log!(engine, "exit_plan_mode", ExitPlanModeToolHandler);
    register_handler_or_log!(engine, "introspect", IntrospectToolHandler);
    register_handler_or_log!(engine, "reflect", ReflectToolHandler);
    register_handler_or_log!(engine, "compress_context", CompressContextToolHandler);
    register_handler_or_log!(
        engine,
        "rollback_session_state",
        RollbackSessionStateToolHandler
    );
    register_handler_or_log!(engine, "mo_query", MoQueryToolHandler);
    register_handler_or_log!(
        engine,
        "rollback_database_snapshots",
        RollbackDatabaseSnapshotsToolHandler
    );
    register_handler_or_log!(engine, "publish_artifact", PublishArtifactToolHandler);
    register_handler_or_log!(engine, "run_script", RunScriptToolHandler);

    if let Err(error) = engine.register_prefix_handler_with_validator(
        "mcp__",
        astra_core::tool_offer::is_mcp_namespaced_tool_name,
        McpToolHandler,
    ) {
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
impl ToolHandler<RuntimeToolExecutor> for GetAgentInfoToolHandler {
    async fn execute(
        &self,
        context: &RuntimeToolExecutor,
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
impl ToolHandler<RuntimeToolExecutor> for ToolSearchToolHandler {
    async fn execute(
        &self,
        context: &RuntimeToolExecutor,
        args: &Value,
        _cancel_token: Option<&CancellationToken>,
    ) -> astra_tools::ToolResult {
        let pool = context.current_tool_search_pool_schemas();
        tool_result_from_output(astra_tools::tool_search::tool_search(&pool, args))
    }
}

#[derive(Debug, Clone, Copy, Default)]
struct MemoryToolHandler;

#[async_trait]
impl ToolHandler<RuntimeToolExecutor> for MemoryToolHandler {
    async fn execute(
        &self,
        context: &RuntimeToolExecutor,
        args: &Value,
        cancel_token: Option<&CancellationToken>,
    ) -> astra_tools::ToolResult {
        // P2-C: Cooperative cancellation check at heavy handler entry
        if cancel_token.is_some_and(|t| t.is_cancelled()) {
            return astra_tools::ToolResult::error(
                "Memory tool not executed: run was cancelled".to_string(),
            );
        }
        let action = match astra_tools::memory_tool_contract::memory_action_from_args(args) {
            Ok(action) => action,
            Err(error) => {
                return astra_tools::ToolResult::error(format!("Error: {error}"));
            }
        };
        if action == astra_tools::memory_tool_contract::MemoryAction::SessionAudit {
            let inventory = if let Some(shared_pool) = context.context_manifest_pool.as_ref() {
                match astra_services::session_memory_inventory::load_database_session_memory_inventory(
                    shared_pool.get(),
                    &context.user_id,
                    &context.session_id,
                )
                .await
                {
                    Ok(inventory) => inventory,
                    Err(error) => {
                        return astra_tools::ToolResult::error(format!(
                            "Error: session memory extraction audit failed: {error}"
                        ));
                    }
                }
            } else {
                match astra_services::session_memory_inventory::load_local_session_memory_inventory(
                    &context.session_id,
                ) {
                    Ok(inventory) => inventory,
                    Err(error) => {
                        return astra_tools::ToolResult::error(format!(
                            "Error: session memory extraction audit failed: {error}"
                        ));
                    }
                }
            };
            return match serde_json::to_string(&inventory) {
                Ok(output) => astra_tools::ToolResult::text(output),
                Err(error) => astra_tools::ToolResult::error(format!(
                    "Error: serialize session memory extraction audit: {error}"
                )),
            };
        }
        let isolated_args = memory_args_with_context(
            args,
            &context.session_id,
            &context.user_id,
            context.journal_turn_index.load(Ordering::Relaxed),
        );
        let output = context
            .memoria_client
            .call(action.as_str(), &isolated_args)
            .await;
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
impl ToolHandler<RuntimeToolExecutor> for SessionToolHandler {
    async fn execute(
        &self,
        context: &RuntimeToolExecutor,
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
struct TaskBoardToolHandler;

#[async_trait]
impl ToolHandler<RuntimeToolExecutor> for TaskBoardToolHandler {
    async fn execute(
        &self,
        context: &RuntimeToolExecutor,
        args: &Value,
        cancel_token: Option<&CancellationToken>,
    ) -> astra_tools::ToolResult {
        self.execute_invocation(
            context,
            args,
            ToolInvocationMetadata::default(),
            cancel_token,
        )
        .await
    }

    async fn execute_invocation(
        &self,
        context: &RuntimeToolExecutor,
        args: &Value,
        invocation: ToolInvocationMetadata<'_>,
        cancel_token: Option<&CancellationToken>,
    ) -> astra_tools::ToolResult {
        // P2-C: Cooperative cancellation check at heavy handler entry
        if cancel_token.is_some_and(|t| t.is_cancelled()) {
            return astra_tools::ToolResult::error(
                "Task tool not executed: run was cancelled".to_string(),
            );
        }
        crate::server::tool_task_runtime::execute_with_executor(context, args, invocation.run_id)
            .await
    }
}

#[derive(Debug, Clone, Copy, Default)]
struct AgentToolHandler;

#[async_trait]
impl ToolHandler<RuntimeToolExecutor> for AgentToolHandler {
    async fn execute(
        &self,
        context: &RuntimeToolExecutor,
        args: &Value,
        cancel_token: Option<&CancellationToken>,
    ) -> astra_tools::ToolResult {
        self.execute_invocation(
            context,
            args,
            ToolInvocationMetadata::default(),
            cancel_token,
        )
        .await
    }

    async fn execute_invocation(
        &self,
        context: &RuntimeToolExecutor,
        args: &Value,
        invocation: ToolInvocationMetadata<'_>,
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
            invocation.tool_call_id,
        )
        .await
    }
}

#[derive(Debug, Clone, Copy, Default)]
struct AgentFanoutToolHandler;

#[async_trait]
impl ToolHandler<RuntimeToolExecutor> for AgentFanoutToolHandler {
    async fn execute(
        &self,
        context: &RuntimeToolExecutor,
        args: &Value,
        cancel_token: Option<&CancellationToken>,
    ) -> astra_tools::ToolResult {
        self.execute_invocation(
            context,
            args,
            ToolInvocationMetadata::default(),
            cancel_token,
        )
        .await
    }

    async fn execute_invocation(
        &self,
        context: &RuntimeToolExecutor,
        args: &Value,
        invocation: ToolInvocationMetadata<'_>,
        cancel_token: Option<&CancellationToken>,
    ) -> astra_tools::ToolResult {
        // P2-C: Cooperative cancellation check at heavy handler entry
        if cancel_token.is_some_and(|t| t.is_cancelled()) {
            return astra_tools::ToolResult::error(
                "Agent fanout not executed: run was cancelled".to_string(),
            );
        }
        execute_agent_fanout_tool(
            context.agent_tool_context.as_ref(),
            args,
            invocation.tool_call_id,
        )
        .await
    }
}

#[derive(Debug, Clone, Copy, Default)]
struct AskUserToolHandler;

#[async_trait]
impl ToolHandler<RuntimeToolExecutor> for AskUserToolHandler {
    async fn execute(
        &self,
        context: &RuntimeToolExecutor,
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
impl ToolHandler<RuntimeToolExecutor> for EnterPlanModeToolHandler {
    async fn execute(
        &self,
        context: &RuntimeToolExecutor,
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
                context.plan_authoring_active_handle.as_ref(),
                args,
            )
            .await,
        )
    }
}

#[derive(Debug, Clone, Copy, Default)]
struct ExitPlanModeToolHandler;

#[async_trait]
impl ToolHandler<RuntimeToolExecutor> for ExitPlanModeToolHandler {
    async fn execute(
        &self,
        context: &RuntimeToolExecutor,
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
                context.plan_authoring_active_handle.as_ref(),
                args,
            )
            .await,
        )
    }
}

#[derive(Debug, Clone, Copy, Default)]
struct IntrospectToolHandler;

#[async_trait]
impl ToolHandler<RuntimeToolExecutor> for IntrospectToolHandler {
    async fn execute(
        &self,
        context: &RuntimeToolExecutor,
        args: &Value,
        _cancel_token: Option<&CancellationToken>,
    ) -> astra_tools::ToolResult {
        let mut snapshot = current_introspect_snapshot(
            &context.session_id,
            &context.introspect_snapshot,
            context.journal_turn_index.load(Ordering::Acquire),
        );
        let run_id = args
            .get("_run_id")
            .or_else(|| args.get("run_id"))
            .and_then(Value::as_str)
            .filter(|run_id| !run_id.trim().is_empty());
        match context.invocation_ledger.as_ref() {
            Some(ledger) => match ledger
                .lifecycle_diagnostics(&context.user_id, &context.session_id, run_id)
                .await
            {
                Ok(Some(diagnostics)) => {
                    snapshot.invocation_lifecycle = Some(
                        astra_turn_core::introspect::InvocationLifecycleSnapshot {
                            run_id: diagnostics.run_id,
                            hot_total: diagnostics.hot_total,
                            prepared: diagnostics.prepared,
                            dispatched: diagnostics.dispatched,
                            succeeded: diagnostics.succeeded,
                            failed: diagnostics.failed,
                            rejected: diagnostics.rejected,
                            outcome_unknown: diagnostics.outcome_unknown,
                            rejected_without_dispatch: diagnostics.rejected_without_dispatch,
                            archive_chunks: diagnostics.archive_chunks,
                            durable_artifact_references: diagnostics.durable_artifact_references,
                            reconciliation_events: diagnostics.reconciliation_events,
                            compaction_deferred_events: diagnostics.compaction_deferred_events,
                            compaction_cursor_generation: diagnostics
                                .compaction_cursor_generation,
                            compaction_cursor_updated_at: diagnostics
                                .compaction_cursor_updated_at,
                        },
                    );
                }
                Ok(None) => snapshot.alerts.push(
                    "durable invocation lifecycle unavailable: in-memory ledger has no durable evidence plane"
                        .to_string(),
                ),
                Err(error) => {
                    tracing::warn!(
                        user_id = %context.user_id,
                        session_id = %context.session_id,
                        ?run_id,
                        %error,
                        "introspect durable invocation lifecycle query failed"
                    );
                    snapshot.alerts.push(format!(
                        "durable invocation lifecycle degraded: {error}"
                    ));
                }
            },
            None => snapshot.alerts.push(
                "durable invocation lifecycle unavailable: invocation ledger is not configured"
                    .to_string(),
            ),
        }
        tool_result_from_output(render_introspect_snapshot(args, &snapshot))
    }
}

#[derive(Debug, Clone, Copy, Default)]
struct ReflectToolHandler;

#[async_trait]
impl ToolHandler<RuntimeToolExecutor> for ReflectToolHandler {
    async fn execute(
        &self,
        context: &RuntimeToolExecutor,
        args: &Value,
        cancel_token: Option<&CancellationToken>,
    ) -> astra_tools::ToolResult {
        if cancel_token.is_some_and(|t| t.is_cancelled()) {
            return astra_tools::ToolResult::error(
                "Reflect tool not executed: run was cancelled".to_string(),
            );
        }

        let topic = string_arg(args, "topic");
        let facet = string_arg(args, "facet");
        let depth = string_arg(args, "depth");
        let horizon = string_arg(args, "horizon");
        let source_policy = string_arg(args, "source_policy");
        let include_context = args
            .get("include_context")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let last_n = args
            .get("last_n")
            .and_then(Value::as_i64)
            .unwrap_or(20)
            .clamp(1, 100) as i32;
        let question = args
            .get("question")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let request = astra_services::reflect::ReflectRequest::from_observation_params_with_source(
            topic,
            facet,
            depth,
            horizon,
            source_policy,
            include_context,
            last_n,
            question,
        );

        match context
            .reflect_service
            .build_evidence(&context.user_id, &context.session_id, request.clone())
            .await
        {
            Ok(mut report) => {
                inject_runtime_provider_coverage(&mut report, context.capacity_provider_coverage());
                match serde_json::to_string(&report) {
                    Ok(output) => astra_tools::ToolResult::text(output),
                    Err(error) => astra_tools::ToolResult::error(format!(
                        "Error: failed to encode reflect report: {error}"
                    )),
                }
            }
            Err((_status, axum::Json(body))) => {
                // Fall back to local snapshot-based reflect when the cloud
                // service is unavailable and the source policy allows local data.
                if request.source_policy.allows_edge_local_artifacts() {
                    if let Ok(guard) = context.introspect_snapshot.read() {
                        if let Some(ref snapshot) = *guard {
                            let mut snapshot = snapshot.clone();
                            astra_turn_core::introspect::mark_snapshot_age(
                                &mut snapshot,
                                context.journal_turn_index.load(Ordering::Acquire),
                            );
                            let local_summary =
                                crate::turn::inspection_service::local_reflect_from_snapshot(
                                    &snapshot,
                                    request.facet,
                                );
                            return astra_tools::ToolResult::text(local_summary);
                        }
                    }
                }

                astra_tools::ToolResult::error(
                    serde_json::json!({
                        "tool": "reflect",
                        "status": "reflect_unavailable",
                        "http_status": _status.as_u16(),
                        "error": body.detail,
                        "error_code": body.error_code,
                        "session_id": context.session_id,
                        "topic": request.topic,
                        "facet": request.facet,
                        "depth": request.depth,
                        "horizon": request.horizon,
                        "source_policy": request.source_policy,
                        "include_context": request.include_context,
                        "last_n": request.last_n,
                        "question": request.question,
                    })
                    .to_string(),
                )
            }
        }
    }
}

fn inject_runtime_provider_coverage(
    report: &mut astra_services::ReflectReport,
    coverage: Vec<astra_turn_core::introspect::CapacityProviderCoverageEntry>,
) {
    for provider in coverage {
        report.data_coverage.providers.insert(
            format!("runtime_provider:{}", provider.provider_type.as_str()),
            astra_core::ObservationProviderCoverage {
                status: provider.status.as_str().to_string(),
                freshness_ms: None,
                reason: provider.unavailable_reason.clone(),
            },
        );
    }
    if let Some(view) = report.view.as_mut() {
        view.data_coverage = report.data_coverage.clone();
    }
}

fn string_arg<'a>(args: &'a Value, key: &str) -> Option<&'a str> {
    args.get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

#[derive(Debug, Clone, Copy)]
struct DefaultExecutorToolHandler {
    name: &'static str,
}

#[async_trait]
impl ToolHandler<RuntimeToolExecutor> for DefaultExecutorToolHandler {
    async fn execute(
        &self,
        context: &RuntimeToolExecutor,
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
impl ToolHandler<RuntimeToolExecutor> for WriteFileToolHandler {
    async fn execute(
        &self,
        context: &RuntimeToolExecutor,
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
impl ToolHandler<RuntimeToolExecutor> for StrReplaceToolHandler {
    async fn execute(
        &self,
        context: &RuntimeToolExecutor,
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
struct RollbackFileEditsToolHandler;

#[async_trait]
impl ToolHandler<RuntimeToolExecutor> for RollbackFileEditsToolHandler {
    async fn execute(
        &self,
        context: &RuntimeToolExecutor,
        args: &Value,
        cancel_token: Option<&CancellationToken>,
    ) -> astra_tools::ToolResult {
        if cancel_token.is_some_and(|t| t.is_cancelled()) {
            return astra_tools::ToolResult::error(
                "File rollback not executed: run was cancelled".to_string(),
            );
        }
        tool_result_from_output(execute_rollback_file_edits(
            &context.workspace_root,
            args,
            context.journal_turn_index.load(Ordering::Relaxed),
            context.file_journal.as_ref(),
        ))
    }
}

#[derive(Debug, Clone, Copy, Default)]
struct BashToolHandler;

#[async_trait]
impl ToolHandler<RuntimeToolExecutor> for BashToolHandler {
    async fn execute(
        &self,
        context: &RuntimeToolExecutor,
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
struct CompressContextToolHandler;

#[async_trait]
impl ToolHandler<RuntimeToolExecutor> for CompressContextToolHandler {
    async fn execute(
        &self,
        context: &RuntimeToolExecutor,
        args: &Value,
        _cancel_token: Option<&CancellationToken>,
    ) -> astra_tools::ToolResult {
        tool_result_from_output(context.compress_context(args))
    }
}

#[derive(Debug, Clone, Copy, Default)]
struct RollbackSessionStateToolHandler;

#[async_trait]
impl ToolHandler<RuntimeToolExecutor> for RollbackSessionStateToolHandler {
    async fn execute(
        &self,
        context: &RuntimeToolExecutor,
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
                        task_manager: &context.task_manager(),
                    },
                },
                args,
                || {
                    context
                        .publish_current_workspace("runtime_tool_executor:rollback_session_state")
                },
            )
            .await,
        )
    }
}

#[derive(Debug, Clone, Copy, Default)]
struct MoQueryToolHandler;

#[async_trait]
impl ToolHandler<RuntimeToolExecutor> for MoQueryToolHandler {
    async fn execute(
        &self,
        context: &RuntimeToolExecutor,
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
impl ToolHandler<RuntimeToolExecutor> for RollbackDatabaseSnapshotsToolHandler {
    async fn execute(
        &self,
        context: &RuntimeToolExecutor,
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
impl ToolHandler<RuntimeToolExecutor> for PublishArtifactToolHandler {
    async fn execute(
        &self,
        context: &RuntimeToolExecutor,
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
            context.session_artifact_store.as_deref(),
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
impl ToolHandler<RuntimeToolExecutor> for RunScriptToolHandler {
    async fn execute(
        &self,
        context: &RuntimeToolExecutor,
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
impl DynamicToolHandler<RuntimeToolExecutor> for McpToolHandler {
    async fn execute(
        &self,
        name: &str,
        context: &RuntimeToolExecutor,
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
        context.execute_mcp_tool(name, args, None).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn server_direct_default_executor_handlers_follow_shared_contract() {
        let engine = runtime_tool_engine();
        let handlers = engine
            .handler_names()
            .collect::<std::collections::HashSet<_>>();

        for name in SERVER_DIRECT_DEFAULT_EXECUTOR_TOOL_NAMES {
            assert!(
                handlers.contains(name),
                "server runtime must register direct DefaultToolExecutor handler for {name}"
            );
        }
        for wrapped in [
            "write_file",
            "str_replace",
            "bash",
            "run_script",
            "task_board",
            "session",
            "memory",
            "rollback_file_edits",
        ] {
            assert!(
                !astra_tools::executor::is_server_direct_default_executor_tool(wrapped),
                "server-specific wrapper `{wrapped}` must not be classified as direct default executor"
            );
            assert!(
                handlers.contains(wrapped),
                "server-specific wrapper `{wrapped}` must still have a runtime handler"
            );
        }
    }
}
