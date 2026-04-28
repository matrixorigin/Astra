//! Plan execution wiring: spawns a background executor task, then runs the
//! in-process plan monitor in the current task until the run pauses, completes, or
//! the user cancels. Progress is shown live; between-monitor idle time (if the REPL
//! is reached again) may still use [`crate::plan_monitor::flush_plan_updates_between_prompts`]
//! when applicable.

use crate::durable_bridge;
use crate::plan_executor;
use crate::plan_interaction;
use crate::repl_runtime::create_background_plan_selector;
use crate::repl_state::ReplState;
use crate::theme;
use crossterm::style::Stylize;

/// Build a [`TaskLearningBridge`] from ReplState's shared pipeline components.
///
/// Returns `None` if any of the required learning modules (entity_graph,
/// pattern_library, calibrator) are not yet initialized.
pub(crate) fn build_learning_bridge(
    state: &ReplState,
) -> Option<std::sync::Arc<dyn astra_services::TaskLearningBridge>> {
    let eg = state.entity_graph.as_ref()?;
    let pl = state.pattern_library.as_ref()?;
    let cal = state.calibrator.as_ref()?;
    let mut bridge = astra_pipeline::task_learning::PipelineTaskLearningBridge::from_shared(
        eg.clone(),
        pl.clone(),
        cal.clone(),
    );
    if let Some(mc) = &state.matrix_runtime {
        let pool = mc.shared_pool().get().clone();
        let user_id = state.ingestion_user_id.as_deref().unwrap_or("anonymous");
        bridge = bridge.with_cloud_pool(pool, user_id);
    }
    Some(std::sync::Arc::new(bridge))
}

fn build_fallback_delegation_engine()
-> std::sync::Arc<astra_runtime::server::delegation_engine::DelegationEngine> {
    let mut registry = astra_services::AgentProfileRegistry::new();
    super::delegate_subrun::register_default_agents(&mut registry);
    let registry = std::sync::Arc::new(tokio::sync::RwLock::new(registry));
    let run_store = std::sync::Arc::new(astra_services::runs::InMemoryRunStateStore::default());
    let engine = astra_runtime::server::delegation_engine::DelegationEngine::with_executor(
        registry,
        std::sync::Arc::new(astra_runtime::server::run_engine::RunEngine::new(run_store)),
        std::sync::Arc::new(astra_runtime::server::delegation_engine::DelegationTracker::new()),
        std::sync::Arc::new(astra_runtime::server::delegation_engine::StubSubRunExecutor),
    );
    std::sync::Arc::new(engine)
}

/// Extract a [`BackgroundPlanContext`] from the current REPL state.
///
/// Clones the active plan for the background executor, moves durable task state
/// and corrections out of `state`, and leaves an in-memory copy behind so
/// `/plan status` can keep reporting progress after plan mode exits.
fn take_plan_context(
    state: &mut ReplState,
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
        evolution_service: state.evolution_service.clone(),
        ingestion_user_id: state.ingestion_user_id.clone(),
        matrix_runtime: state.matrix_runtime.clone(),
        entity_graph: state.entity_graph.clone(),
        pattern_library: state.pattern_library.clone(),
        calibrator: state.calibrator.clone(),
        plan_execution_config: state.plan_execution_config.clone(),
        turn: state.turn,
        turn_retry_counts: std::collections::HashMap::new(),
        current_subtask_strategy_hint: None,
    })
}

/// Create a `Box<dyn ToolSelector>` for the background plan executor.
///
/// Shares `entity_graph` / `pattern_library` / `calibrator` with
/// [`plan_executor::BackgroundPlanContext`] when all three are present.
fn create_background_selector(
    ctx: &plan_executor::BackgroundPlanContext,
) -> Box<dyn astra_runtime::tool_selector::ToolSelector> {
    create_background_plan_selector(ctx)
}

/// Spawns a background plan executor, then **blocks the caller** in
/// [`crate::plan_monitor::run_blocking_plan_monitor`] until the run pauses, finishes,
/// or the user hits Ctrl+C (per monitor behavior).
///
/// The heavy work still runs in the executor’s `tokio` task. This function only
/// returns after the blocking monitor loop exits, so the normal REPL prompt is not
/// interleaved with that plan run. The in-memory `executing_plan` copy and
/// `plan_handle` keep [`crate::plan_monitor::flush_plan_updates_between_prompts`] and
/// `/plan status` useful when the user is at the prompt again.
pub(crate) async fn start_and_monitor_plan(
    state: &mut ReplState,
    current_token: Option<&str>,
    api: &astra_thin_client::ThinClient,
    profile: Option<&str>,
) -> Result<(), String> {
    if plan_interaction::shutdown_plan_executor(state) {
        eprintln!(
            "  {}  Previous plan executor cancelled before starting new run.",
            theme::icon_warn()
        );
    }

    ensure_durable_task_state(state, Some(api), current_token).await;

    let ctx = take_plan_context(state, api, current_token, profile)?;
    let selector = create_background_selector(&ctx);
    let handle = plan_executor::spawn_plan_executor(ctx, selector);
    state.plan_handle = Some(handle);

    eprintln!(
        "  {} {}",
        "▸".bold().cyan(),
        "Plan executing — Ctrl+C to pause.".bold()
    );

    // Run the blocking monitor so the user sees live progress.
    // The monitor handles Ctrl+C (pause/cancel) and approval prompts inline.
    super::plan_monitor::run_blocking_plan_monitor(state).await;

    Ok(())
}

/// Initialize `durable_task_state` on `ReplState` if it's `None` and a plan
/// is ready for execution. This generates a [`TaskContract`] with structured
/// verification criteria so the background executor can gate subtask completion.
async fn ensure_durable_task_state(
    state: &mut ReplState,
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

    let server_proxy_judge: Option<std::sync::Arc<dyn astra_services::LlmJudge>> =
        if let (Some(a), Some(t)) = (api, token) {
            Some(std::sync::Arc::new(
                durable_bridge::ServerProxyLlmJudge::new(
                    a.clone(),
                    t.to_string(),
                    state.model.clone(),
                ),
            ))
        } else {
            None
        };

    let ingestion_sender = state
        .matrix_runtime
        .as_ref()
        .and_then(|mc| mc.clone_ingestion_sender());
    let cloud_judge = state
        .matrix_runtime
        .as_ref()
        .and_then(|mc| mc.create_cloud_llm_judge())
        .map(|j| std::sync::Arc::new(j) as std::sync::Arc<dyn astra_services::LlmJudge>);
    let learning = build_learning_bridge(state);

    let lifecycle = if let Some(pool) = state
        .matrix_runtime
        .as_ref()
        .map(|mc| mc.shared_pool().get().clone())
    {
        durable_bridge::create_cloud_lifecycle_full(
            pool,
            &work_dir,
            ingestion_sender,
            Some(session_id),
            Some(user_id),
            cloud_judge,
            learning,
            server_proxy_judge,
        )
    } else {
        let session_dir = state
            .session_id
            .as_ref()
            .map(|sid| astra_services::session_workspace::workspace_dir_for(sid))
            .unwrap_or_else(|| work_dir.join(".mo-session"));
        durable_bridge::create_local_lifecycle_full(
            &session_dir,
            &work_dir,
            ingestion_sender,
            Some(session_id),
            Some(user_id),
            cloud_judge,
            learning,
            server_proxy_judge,
        )
    };

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
        let mut state = ReplState::default();
        state.executing_plan = Some(TaskPlan::default());
        state.session_id = Some("sess-plan".to_string());
        state.turn = 7;

        let hub =
            std::sync::Arc::new(astra_runtime::observability_integration::ObservabilityHub::new());
        let session = hub.start_session("user-1", "sess-plan");
        let evolution =
            std::sync::Arc::new(astra_runtime::evolution::service::EvolutionService::new());
        state.observability_hub = Some(hub.clone());
        state.observability_session = Some(session.clone());
        state.evolution_service = Some(evolution.clone());

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
        assert!(std::sync::Arc::ptr_eq(
            ctx.evolution_service.as_ref().unwrap(),
            &evolution
        ));
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
}
