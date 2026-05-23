//! Plan execution wiring: spawns a background executor task, then runs the
//! in-process plan monitor in the current task until the run pauses, completes, or
//! the user cancels. Progress is shown live; between-monitor idle time (if the CLI
//! prompt is reached again) may still use [`crate::plan_monitor::flush_plan_updates_between_prompts`]
//! when applicable.

use crate::durable_bridge;
use crate::plan_executor;

use crate::session_state::SessionState;
use crate::theme;
use crossterm::style::Stylize;

fn build_fallback_delegation_engine()
-> std::sync::Arc<astra_runtime::server::delegation::engine::DelegationEngine> {
    let mut registry = astra_services::AgentProfileRegistry::new();
    super::delegate_subrun::register_default_agents(&mut registry);
    let registry = std::sync::Arc::new(tokio::sync::RwLock::new(registry));
    let run_store = std::sync::Arc::new(astra_services::runs::InMemoryRunStateStore::default());
    let engine = astra_runtime::server::delegation::engine::DelegationEngine::with_executor(
        registry,
        std::sync::Arc::new(astra_runtime::server::run::engine::RunEngine::new(
            run_store,
        )),
        std::sync::Arc::new(astra_runtime::server::delegation::engine::DelegationTracker::new()),
        std::sync::Arc::new(astra_runtime::server::delegation::engine::StubSubRunExecutor),
    );
    std::sync::Arc::new(engine)
}

/// Extract a [`BackgroundPlanContext`] from the current CLI session state.
///
/// Clones the active plan for the background executor, moves durable task state
/// and corrections out of `state`, and leaves an in-memory copy behind so
/// execution state can still be surfaced after plan mode exits.
fn take_plan_context(
    state: &mut SessionState,
    api: &astra_thin_client::ThinClient,
    current_token: Option<&str>,
    profile: Option<&str>,
) -> Result<plan_executor::BackgroundPlanContext, String> {
    let plan = state.executing_plan.clone().ok_or("No plan to execute")?;
    let token = current_token
        .ok_or("Not logged in — cannot start background plan")?
        .to_string();

    Ok(plan_executor::BackgroundPlanContext {
        api: api.clone(),
        token,
        profile: profile.map(|p| p.to_string()),
        model: state.model.clone(),
        plan,
        plan_goal: state.executing_plan_goal.clone(),
        plan_id: state.executing_plan_id.clone(),
        plan_corrections: std::mem::take(&mut state.plan_execution_corrections),
        history: state.history.clone(),
        session_id: state.session_id.clone(),
        recent_tools: state.recent_tools.clone(),
        tool_health_entries: state.tool_health_entries.clone(),
        unified_skill_registry: state.unified_skill_registry.clone(),
        skill_search: state.skill_search.clone(),
        delegation_engine: state.delegation_engine.clone(),
        messaging_metrics: state.messaging_metrics.clone(),
        agent_spawner: state.agent_spawner.clone(),
        root_mailbox: None,
        root_agent_id: format!("plan-{}", uuid::Uuid::new_v4()),
        durable_task_state: state.durable_task_state.take(),
        workspace_root: std::env::current_dir().unwrap_or_default(),
        observability_hub: state.observability_hub.clone(),
        observability_session: state.observability_session.clone(),
        file_journal: state.file_journal.clone(),
        file_state: state.file_state.clone(),
        database_snapshot_journal: state.database_snapshot_journal.clone(),
        git_stash_journal: state.git_stash_journal.clone(),
        git_commit_journal: state.git_commit_journal.clone(),
        git_worktree_journal: state.git_worktree_journal.clone(),
        session_state_journal: state.session_state_journal.clone(),
        task_manager: state.task_manager.clone(),
        bg_task_commands: Some(state.bg_task_commands.clone()),
        bash_detach_slot: Some(state.bash_detach_slot.clone()),
        #[cfg(feature = "harness")]
        harness_sink: Some(state.harness_sink.clone()),
        #[cfg(feature = "harness")]
        harness_trace: Some(state.harness_trace.clone()),
        turn: state.turn,
        turn_retry_counts: std::collections::HashMap::new(),
        current_subtask_strategy_hint: None,
    })
}

/// Spawns a background plan executor, then **blocks the caller** in
/// [`crate::plan_monitor::run_blocking_plan_monitor`] until the run pauses, finishes,
/// or the user hits Ctrl+C (per monitor behavior).
///
/// The heavy work still runs in the executor’s `tokio` task. This function only
/// returns after the blocking monitor loop exits, so the normal CLI prompt is not
/// interleaved with that plan run. The in-memory `executing_plan` copy and
/// `plan_handle` keep [`crate::plan_monitor::flush_plan_updates_between_prompts`] and
/// related execution state available when the user is at the prompt again.
pub(crate) async fn start_and_monitor_plan(
    state: &mut SessionState,
    current_token: Option<&str>,
    api: &astra_thin_client::ThinClient,
    profile: Option<&str>,
) -> Result<(), String> {
    if shutdown_plan_executor(state) {
        eprintln!(
            "  {}  Previous plan executor cancelled before starting new run.",
            theme::icon_warn()
        );
    }

    ensure_durable_task_state(state, Some(api), current_token).await;

    let ctx = take_plan_context(state, api, current_token, profile)?;
    let handle = plan_executor::spawn_plan_executor(ctx);
    state.plan_handle = Some(handle);

    eprintln!(
        "  {} {}",
        "▸".bold().magenta(),
        "Plan executing — Ctrl+C to pause.".bold()
    );

    // Run the blocking monitor so the user sees live progress.
    // The monitor handles Ctrl+C (pause/cancel) and approval prompts inline.
    super::plan_monitor::run_blocking_plan_monitor(state).await;

    Ok(())
}

/// Cleanly shut down a running plan executor handle.
///
/// Sends `Cancel`, drains remaining updates, and returns `true` if a handle was
/// actually present (i.e. an executor was running). Call this before spawning a
/// new executor or when abandoning an in-flight run.
pub(crate) fn shutdown_plan_executor(state: &mut SessionState) -> bool {
    if let Some(mut h) = state.plan_handle.take() {
        let _ = h.send_command(crate::plan_executor::PlanCommand::Cancel);
        while h.try_recv().is_some() {}
        true
    } else {
        false
    }
}

/// Initialize `durable_task_state` on `SessionState` if it's `None` and a plan
/// is ready for execution. This generates a [`TaskContract`] with structured
/// verification criteria so the background executor can gate subtask completion.
async fn ensure_durable_task_state(
    state: &mut SessionState,
    api: Option<&astra_thin_client::ThinClient>,
    token: Option<&str>,
) {
    if let Some(ref durable) = state.durable_task_state {
        // Reuse existing contract if subtask IDs still match the plan.
        // If the user edited the plan (added/removed subtasks), regenerate.
        if let Some(ref plan) = state.executing_plan {
            let contract_ids: std::collections::HashSet<&str> = durable
                .contract
                .subtasks
                .iter()
                .map(|s| s.id.as_str())
                .collect();
            let plan_ids: std::collections::HashSet<&str> =
                plan.subtasks.iter().map(|s| s.id.as_str()).collect();
            if contract_ids == plan_ids {
                return;
            }
            // Mismatch — drop stale contract so it gets regenerated below
            state.durable_task_state = None;
        } else {
            return;
        }
    }
    let plan = match state.executing_plan.as_ref() {
        Some(p) => p,
        None => return,
    };

    let goal = state
        .executing_plan_goal
        .as_deref()
        .unwrap_or("Plan execution");
    let user_id = state.ingestion_user_id.as_deref().unwrap_or("local");
    let session_id = state.session_id.as_deref().unwrap_or("unknown");
    let work_dir = std::env::current_dir().unwrap_or_default();

    // Judge uses the server's reasoning model (via admin_config.reasoning_model_name →
    // cheapest active fallback). Do NOT pass state.model: the chat model may be expensive,
    // while the judge should use the cheap reasoning model.
    let server_proxy_judge: Option<std::sync::Arc<dyn astra_services::LlmJudge>> =
        if let (Some(a), Some(t)) = (api, token) {
            Some(std::sync::Arc::new(
                durable_bridge::ServerProxyLlmJudge::new(a.clone(), t.to_string(), None),
            ))
        } else {
            None
        };

    // Judge runs server-side via server_proxy_judge (the server resolves the reasoning model
    // from admin_config + infra_llm_models). No local cloud judge.
    let cloud_judge: Option<std::sync::Arc<dyn astra_services::LlmJudge>> = None;

    let session_dir = state
        .session_id
        .as_ref()
        .map(|sid| astra_services::session_workspace::workspace_dir_for(sid))
        .unwrap_or_else(|| work_dir.join(".mo-session"));
    let lifecycle = durable_bridge::create_local_lifecycle_full(
        &session_dir,
        &work_dir,
        None,
        Some(session_id),
        Some(user_id),
        cloud_judge,
        server_proxy_judge,
    );

    if let Some(contract) =
        durable_bridge::generate_contract(&lifecycle, plan, goal, user_id, session_id, &work_dir)
            .await
    {
        state.durable_task_state = Some(durable_bridge::DurableTaskState {
            contract,
            lifecycle,
            last_report: None,
        });

        if state.delegation_engine.is_none() {
            state.delegation_engine = Some(build_fallback_delegation_engine());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use astra_services::coordination::{
        AggregationStrategy, CoordinationPattern, DelegationRequest,
    };
    use astra_services::task_orchestrator::TaskPlan;

    #[test]
    fn take_plan_context_preserves_nested_turn_runtime_context() {
        let api = astra_thin_client::ThinClient::new("http://127.0.0.1:1", None).unwrap();
        let mut state = SessionState::default();
        state.executing_plan = Some(TaskPlan::default());
        state.set_session_id("sess-plan".to_string());
        state.turn = 7;

        let hub = std::sync::Arc::new(astra_runtime::observability::ObservabilityHub::new());
        let session = hub.start_session("user-1", "sess-plan");
        state.observability_hub = Some(hub.clone());
        state.observability_session = Some(session.clone());

        let file_journal = state.file_journal.clone();
        let database_snapshot_journal = state.database_snapshot_journal.clone();
        let git_stash_journal = state.git_stash_journal.clone();
        let git_commit_journal = state.git_commit_journal.clone();
        let git_worktree_journal = state.git_worktree_journal.clone();
        let session_state_journal = state.session_state_journal.clone();
        let task_manager = state.task_manager.clone();

        let ctx = take_plan_context(&mut state, &api, Some("token"), None).unwrap();

        assert_eq!(ctx.turn, 7);
        assert!(std::sync::Arc::ptr_eq(
            ctx.observability_hub.as_ref().unwrap(),
            &hub
        ));
        assert!(std::sync::Arc::ptr_eq(
            ctx.observability_session.as_ref().unwrap(),
            &session
        ));
        assert!(std::sync::Arc::ptr_eq(&ctx.file_journal, &file_journal));
        assert!(std::sync::Arc::ptr_eq(
            &ctx.database_snapshot_journal,
            &database_snapshot_journal
        ));
        assert!(std::sync::Arc::ptr_eq(
            &ctx.git_stash_journal,
            &git_stash_journal
        ));
        assert!(std::sync::Arc::ptr_eq(
            &ctx.git_commit_journal,
            &git_commit_journal
        ));
        assert!(std::sync::Arc::ptr_eq(
            &ctx.git_worktree_journal,
            &git_worktree_journal
        ));
        assert!(std::sync::Arc::ptr_eq(
            &ctx.session_state_journal,
            &session_state_journal
        ));
        assert!(std::sync::Arc::ptr_eq(&ctx.task_manager, &task_manager));
    }

    #[tokio::test]
    async fn fallback_delegation_engine_registers_default_root_agent() {
        let engine = build_fallback_delegation_engine();
        let request = DelegationRequest {
            delegation_id: "del-1".to_string(),
            parent_run_id: "run-1".to_string(),
            task: "delegate".to_string(),
            pattern: CoordinationPattern::FanOut {
                agent_ids: vec!["coder".to_string()],
                aggregation: AggregationStrategy::AllResults,
                timeout_sec: 10,
            },
            user_id: "user-1".to_string(),
            depth: 0,
            context: std::collections::HashMap::new(),
        };

        assert!(engine.validate(&request, "main").await.is_ok());
    }

    #[test]
    fn shutdown_plan_executor_returns_false_without_handle() {
        let mut state = SessionState::default();
        assert!(!shutdown_plan_executor(&mut state));
    }

    #[test]
    fn shutdown_plan_executor_cancels_and_clears_handle() {
        let mut state = SessionState::default();
        let (handle, _update_tx, _cmd_rx) = plan_executor::create_plan_channels();
        state.plan_handle = Some(handle);

        let had_handle = shutdown_plan_executor(&mut state);

        assert!(had_handle);
        assert!(state.plan_handle.is_none());
    }
}
