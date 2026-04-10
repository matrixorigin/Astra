//! Background plan execution wiring.

use crate::durable_bridge;
use crate::plan_executor;
use crate::plan_interaction;
use crate::plan_monitor::run_blocking_plan_monitor;
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
    let mut bridge =
        astra_runtime::pipeline::task_learning::PipelineTaskLearningBridge::from_shared(
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

/// Extract a [`BackgroundPlanContext`] from the current REPL state.
///
/// Moves the active plan, durable task state, and corrections out of `state`
/// (using `take()`), and clones the remaining fields needed by the background
/// executor. On success `state.executing_plan` will be `None`.
fn take_plan_context(
    state: &mut ReplState,
    api: &astra_thin_client::ThinClient,
    current_token: Option<&str>,
    profile: Option<&str>,
) -> Result<plan_executor::BackgroundPlanContext, String> {
    let plan = state.executing_plan.take().ok_or("No plan to execute")?;
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
        ingestion_user_id: state.ingestion_user_id.clone(),
        matrix_runtime: state.matrix_runtime.clone(),
        entity_graph: state.entity_graph.clone(),
        pattern_library: state.pattern_library.clone(),
        calibrator: state.calibrator.clone(),
        plan_execution_config: state.plan_execution_config.clone(),
        turn: state.turn,
        turn_retry_counts: std::collections::HashMap::new(),
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

/// Spawn a plan executor, then block until it finishes, pauses, or errors.
///
/// The executor runs as a `tokio` task; this function enters a monitoring
/// loop that displays progress in real-time, handles Ctrl-C (→ pause), and
/// resolves approval prompts inline. The REPL prompt is not shown until
/// this function returns.
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

    eprintln!("  {} {}", "▸".bold().cyan(), "Plan executing…".bold());

    run_blocking_plan_monitor(state).await;

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
    if state.durable_task_state.is_some() {
        return;
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
            let registry = std::sync::Arc::new(tokio::sync::RwLock::new(
                astra_services::AgentProfileRegistry::new(),
            ));
            let run_store =
                std::sync::Arc::new(astra_services::runs::InMemoryRunStateStore::default());
            let engine = astra_runtime::server::delegation_engine::DelegationEngine::with_executor(
                registry,
                std::sync::Arc::new(astra_runtime::server::run_engine::RunEngine::new(run_store)),
                std::sync::Arc::new(
                    astra_runtime::server::delegation_engine::DelegationTracker::new(),
                ),
                std::sync::Arc::new(astra_runtime::server::delegation_engine::StubSubRunExecutor),
            );
            state.delegation_engine = Some(std::sync::Arc::new(engine));
        }
    }
}
