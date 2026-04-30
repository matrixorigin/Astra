//! SSE multi-turn agentic loop (`stream_chat_sse`).
//!
//! Entry [`stream_chat_sse`] builds a [`CliAgenticLoopHost`] + [`AgenticLoopState`],
//! runs the runtime's [`run_agentic_loop_with_host`], then finalizes to [`StreamResult`].
//! One iteration is driven by the runtime; the host handles payload prep + HTTP + SSE.

mod agentic_loop_turn;
mod agentic_sse_loop;
mod cli_loop_host;

pub(crate) use agentic_loop_turn::turn_policy_from_payload_edge_tools;

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use astra_core::RuntimeLimits;
use astra_runtime::{
    pipeline::step_protocol::InMemoryIdempotencyCache,
    pipeline::step_recorder::StepRecorder,
    plan_decompose::CHAT_PLAN_ONLY_SYSTEM,
    semantic_dedup::SemanticDedup,
    tool_registry::ToolRegistry,
    turn::agentic_loop_finalization::run_agentic_loop_with_host,
    turn::agentic_loop_host::{
        AgenticLoopState, CancellationState, ErrorRecoveryState, MessagingState, SkillState,
        StallTrackingState, StopHookState, TelemetryState,
    },
    turn::agentic_turn_telemetry::step_recorder_chat_ephemeral_run_id,
    turn::chat_history_openai::openai_messages_from_repl_history,
    turn::chat_turn_heuristics::infer_task_execution_profile,
    turn::edge_prompt_context::detect_project_languages,
    turn::skill_tool::SkillResolver,
    turn::stop_hooks_yaml::detect_turn_hook_sets,
    turn::tool_health::ToolHealthTracker,
    turn::tool_schema_prune::openai_tool_names_from_schemas,
    turn::turn_guard::TurnGuard,
};

use crate::{StreamResult, cli_utils::terminal_width_usize, edge_tools};

use super::ChatTurnParams;
use agentic_sse_loop::{
    StreamLoopSidecarEprint, StreamResultBuild, build_stream_result, eprint_stream_loop_sidecars,
    resolved_tool_metrics,
};
use cli_loop_host::CliAgenticLoopHost;
use serde_json::json;

/// Map `ToolSelectionConfig` (from `astra-config`) to `BreakerConfig` (from
/// `astra-turn-core`).
///
/// Lives in the CLI because `astra-config` and `astra-turn-core` are sibling
/// crates with no dependency edge between them; adding `impl From<…> for
/// BreakerConfig` in either crate would introduce an unwanted dependency.
/// The CLI is the natural composition layer that already depends on both.
/// If a second caller appears, promote this to a dedicated adapter crate
/// rather than coupling the two base crates.
fn circuit_breaker_config_from_tool_selection(
    config: &astra_config::runtime_config::ToolSelectionConfig,
) -> astra_turn_core::loop_circuit_breaker::BreakerConfig {
    // Resolve each threshold and warn when a user-supplied value was clamped
    // to its floor so operators can diagnose unexpected behaviour.
    macro_rules! resolve_and_warn {
        ($raw:expr, $effective:expr, $name:literal) => {{
            let raw = $raw;
            let effective = $effective;
            if raw > 0 && effective != raw {
                tracing::warn!(
                    config_field = $name,
                    user_value = raw,
                    applied_value = effective,
                    "circuit breaker config value below floor — clamped to minimum"
                );
            }
            effective as usize
        }};
    }

    astra_turn_core::loop_circuit_breaker::BreakerConfig {
        stall_threshold: resolve_and_warn!(
            config.circuit_breaker_stall_threshold,
            config.effective_circuit_breaker_stall_threshold(),
            "circuit_breaker_stall_threshold"
        ),
        repetition_threshold: resolve_and_warn!(
            config.circuit_breaker_repetition_threshold,
            config.effective_circuit_breaker_repetition_threshold(),
            "circuit_breaker_repetition_threshold"
        ),
        read_only_stall_threshold: resolve_and_warn!(
            config.circuit_breaker_read_only_stall_threshold,
            config.effective_circuit_breaker_read_only_stall_threshold(),
            "circuit_breaker_read_only_stall_threshold"
        ),
        max_introspect_emissions: resolve_and_warn!(
            config.circuit_breaker_max_introspect_emissions,
            config.effective_circuit_breaker_max_introspect_emissions(),
            "circuit_breaker_max_introspect_emissions"
        ),
        half_open_patience: resolve_and_warn!(
            config.circuit_breaker_half_open_patience,
            config.effective_circuit_breaker_half_open_patience(),
            "circuit_breaker_half_open_patience"
        ),
        absolute_max_rounds: resolve_and_warn!(
            config.circuit_breaker_absolute_max_rounds,
            config.effective_circuit_breaker_absolute_max_rounds(),
            "circuit_breaker_absolute_max_rounds"
        ),
    }
}

async fn finalize_root_mailbox(
    slot: Option<&mut Option<astra_messaging::router::AgentMailbox>>,
    mailbox: &mut Option<astra_messaging::router::AgentMailbox>,
) {
    if let Some(slot) = slot {
        *slot = mailbox.take();
        return;
    }

    if let Some(mailbox) = mailbox.take() {
        let addr = mailbox.address.clone();
        let router = mailbox.router();
        if let Err(e) = router.unregister(&addr).await {
            eprintln!(
                "astra: failed to unregister mailbox for run_id={} agent_id={}: {e}",
                addr.run_id, addr.agent_id
            );
        }
    }
}

fn extend_restricted_with_blocked_tools(
    restricted: &mut HashSet<String>,
    observability_hub: Option<&Arc<astra_runtime::observability_integration::ObservabilityHub>>,
) {
    if let Some(hub) = observability_hub
        && let Some(pattern_library) = hub.pattern_library()
        && let Ok(lib) = pattern_library.lock()
    {
        for name in lib.blocked_tool_names() {
            if !astra_turn_core::tool_registry_meta::is_pinned_tool(&name) {
                restricted.insert(name);
            }
        }
    }
}

pub(crate) async fn stream_chat_sse(
    mut p: ChatTurnParams<'_>,
) -> Result<StreamResult, crate::TurnFailure> {
    let start = Instant::now();
    let root_agent_id = p.root_agent_id.unwrap_or("main");
    let term_width = terminal_width_usize();
    // Capture the model id up front for later `resolve_for_model` calls —
    // `p.model` (Option<&str>) gets consumed into `host.model` below.
    let model_id_for_policy = p.model;
    let tool_selection_config = astra_config::runtime_config::RuntimeConfig::load().tool_selection;
    let resolved_tool_policy = tool_selection_config.resolve_for_model(model_id_for_policy);
    let circuit_breaker_config = circuit_breaker_config_from_tool_selection(&tool_selection_config);

    // Paint an immediate spinner so the user sees feedback during init (executor, schemas,
    // skill discovery, etc.) before the per-turn prep spinner takes over.
    let show_early_hint = !p.render_policy.suppress_text()
        && std::io::IsTerminal::is_terminal(&std::io::stderr())
        && p.plan_assemble_line_release.is_none();
    let early_spinner: Option<crate::effects::Spinner> = if show_early_hint {
        Some(crate::effects::Spinner::start_immediate(
            "Preparing…".to_string(),
        ))
    } else {
        None
    };

    let project_root = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let file_context = detect_project_languages(&project_root);
    let mut executor = {
        let ex =
            edge_tools::ToolExecutor::new(&project_root).with_cloud(p.api.api_origin(), p.token);
        let ex = if let Some(session_id) = p.session_id {
            ex.with_active_session_id(session_id.to_string())
        } else {
            ex
        };
        // Wire session-scoped file journal for cross-turn undo support
        let ex = if let Some(ref journal) = p.file_journal {
            ex.with_shared_file_journal(journal.clone())
        } else {
            ex
        };
        // Wire session-scoped file-state cache for cross-subtask read-before-write
        let ex = if let Some(ref state) = p.file_state {
            ex.with_shared_file_state(state.clone())
        } else {
            ex
        };
        let ex = if let Some(ref journal) = p.database_snapshot_journal {
            ex.with_shared_database_snapshot_journal(journal.clone())
        } else {
            ex
        };
        let ex = if let Some(ref journal) = p.git_stash_journal {
            ex.with_shared_git_stash_journal(journal.clone())
        } else {
            ex
        };
        let ex = if let Some(ref journal) = p.git_commit_journal {
            ex.with_shared_git_commit_journal(journal.clone())
        } else {
            ex
        };
        let ex = if let Some(ref journal) = p.git_worktree_journal {
            ex.with_shared_git_worktree_journal(journal.clone())
        } else {
            ex
        };
        let ex = if let Some(ref journal) = p.session_state_journal {
            ex.with_shared_session_state_journal(journal.clone())
        } else {
            ex
        };
        let ex = if let Some(ref task_manager) = p.task_manager {
            ex.with_shared_task_manager(task_manager.clone())
        } else {
            ex
        };
        // Set turn index so journal entries are tagged for undo
        ex.journal_turn_index
            .store(p.turn_index, std::sync::atomic::Ordering::Release);
        let ex = if let Some(ref mgr) = p.mcp_manager {
            ex.with_mcp_manager(mgr.clone())
        } else {
            ex
        };
        // Wire spawn_agent tool context when spawner is available
        if let Some(ref spawner) = p.agent_spawner {
            let spawn_ctx = edge_tools::agent_spawning::SpawnAgentContext {
                run_id: p.session_id.unwrap_or("ephemeral").to_string(),
                agent_id: root_agent_id.to_string(),
                recursion_depth: 0,
                working_dir: project_root.clone(),
                spawner: spawner.clone(),
                inherited_permissions: p.perm_manager.inherited_permissions_for_child(false),
                active_skills: Vec::new(), // root agent — no inherited skills
            };
            ex.with_spawn_context(spawn_ctx)
        } else {
            ex
        }
    };
    // Wire observability session for context_analysis tool
    if let Some(ref obs) = p.observability_session {
        executor.observability_session = Some(obs.clone());
    }
    let root_send_message_context = p.agent_spawner.as_ref().map(|spawner| {
        edge_tools::agent_messaging::SendMessageRuntimeContext {
            agent_id: root_agent_id.to_string(),
            router: spawner.mailbox_router(),
            metrics: p.messaging_metrics.clone(),
            delegation_id: None,
        }
    });

    // --add-dir: expand sandbox to include additional directories
    if let Ok(dirs) = std::env::var("ASTRA_ADD_DIRS") {
        for dir in dirs.split(':').filter(|s| !s.is_empty()) {
            executor.expand_sandbox_path(PathBuf::from(dir));
        }
    }
    let mut messages = load_turn_messages(p.pre_loaded_messages.take(), p.history, p.message);

    // ─── Context pre-fetch (disabled) ─────────────────────────────────────
    let all_schemas = if p.plan_only_chat {
        messages.insert(
            0,
            json!({
                "role": "system",
                "content": CHAT_PLAN_ONLY_SYSTEM,
            }),
        );
        Vec::new()
    } else {
        // Refresh any MCP servers that received tool-list-changed notifications
        if let Some(ref mgr) = p.mcp_manager {
            let mut m = mgr.write().await;
            m.refresh_changed_tools().await;
            m.consume_prompt_changes();
            m.consume_resource_changes();
        }
        let mut schemas = edge_tools::all_tool_schemas();
        // Inject MCP tool schemas from connected servers
        if let Some(ref mgr) = p.mcp_manager {
            let m = mgr.read().await;
            schemas.extend(m.all_tool_schemas());
        }
        schemas
    };
    let registry = ToolRegistry::new(all_schemas.clone());
    let pinned_schema_tokens = registry.total_pinned_token_cost() as u64;
    let valid_tool_names = openai_tool_names_from_schemas(&all_schemas);

    // --allowed-tools: if set, restrict to only the specified tools
    let mut initial_restricted: HashSet<String> =
        if let Ok(allowed_csv) = std::env::var("ASTRA_ALLOWED_TOOLS") {
            let allowed: HashSet<&str> = allowed_csv
                .split(',')
                .map(|s| s.trim())
                .filter(|s| !s.is_empty())
                .collect();
            if !allowed.is_empty() {
                valid_tool_names
                    .iter()
                    .filter(|name| !allowed.contains(name.as_str()))
                    .cloned()
                    .collect()
            } else {
                HashSet::new()
            }
        } else {
            HashSet::new()
        };

    // --disallowed-tools: directly add to restricted set
    if let Ok(denied_csv) = std::env::var("ASTRA_DISALLOWED_TOOLS") {
        for name in denied_csv
            .split(',')
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
        {
            initial_restricted.insert(name.to_string());
        }
    }

    // Seed persisted hard-blocks from cross-session learning so blocked tools
    // never appear in the visible schema set for a new CLI turn.
    extend_restricted_with_blocked_tools(&mut initial_restricted, p.observability_hub.as_ref());

    let current_session_id = p.session_id.map(|s| s.to_string());
    let existing_root_mailbox = if let Some(slot) = p.root_mailbox_slot.as_deref_mut() {
        slot.take()
    } else {
        None
    };
    let root_mailbox = if let Some(mailbox) = existing_root_mailbox {
        Some(mailbox)
    } else if let Some(ref root_ctx) = root_send_message_context {
        let run_id = current_session_id
            .clone()
            .unwrap_or_else(|| "ephemeral".to_string());
        root_ctx
            .router
            .register(
                astra_messaging::types::AgentAddress::new(run_id, &root_ctx.agent_id),
                None,
            )
            .await
            .ok()
    } else {
        None
    };
    let mut task_profile = infer_task_execution_profile(p.message);
    // Plan-only turns have no tools; factual retry would inject a useless "call tools" nudge and
    // spurious "↻ … corrective retry" when the decomposition prompt mentions repo/context words.
    if p.plan_only_chat {
        task_profile.allow_factual_retry = false;
    }

    let turn_guard = if p.tool_health_entries.is_empty() {
        TurnGuard::with_profile(task_profile)
    } else {
        let health = ToolHealthTracker::from_entries(p.tool_health_entries);
        TurnGuard::with_health_and_profile(health, task_profile)
    };

    let max_turns = if p.is_plan_subtask {
        RuntimeLimits::global().effective_plan_subtask_turns()
    } else {
        RuntimeLimits::global().max_turns
    };
    let step_recorder = if let Some(session_id) = current_session_id.as_deref() {
        StepRecorder::with_persistence(
            session_id,
            step_recorder_chat_ephemeral_run_id(start.elapsed().as_millis()).as_str(),
        )
    } else {
        StepRecorder::new(
            "ephemeral",
            step_recorder_chat_ephemeral_run_id(start.elapsed().as_millis()).as_str(),
        )
    };
    let mut local_discovered_skills = HashSet::new();
    let discovered_skills = match p.discovered_skills.as_deref_mut() {
        Some(shared) => std::mem::take(shared),
        None => std::mem::take(&mut local_discovered_skills),
    };

    // Capture full permission mode before perm_manager is moved into the host.
    let parent_perm_mode = p.perm_manager.mode();
    let parent_cancel_token = p.cancel_token.clone();

    // Snapshot approval overrides for checkpoint persistence.
    let initial_approval_overrides = p.perm_manager.export_session_overrides();

    // ─── Build host + state ──────────────────────────────────────────────
    let mut host = CliAgenticLoopHost {
        api: p.api,
        token: p.token.to_string(),
        auth_profile: p.auth_profile,
        model: p.model,
        explain: p.explain,
        render_md: p.render_md,
        term_width,
        render_policy: p.render_policy,
        message: p.message,
        history: p.history,
        recent_tools: p.recent_tools,
        project_root: project_root.clone(),
        executor: std::sync::Arc::new(executor),
        selector: p.selector,
        registry,
        all_schemas,
        file_context,
        perm_manager: p.perm_manager,
        valid_tool_names,
        pending_clear_lines: 0,
        is_plan_subtask: p.is_plan_subtask,
        plan_subtask_id: p.plan_subtask_id,
        plan_assemble_line_release: p.plan_assemble_line_release.clone(),
        stream_event_tx: p.stream_event_tx,
        approval_request_tx: p.approval_request_tx,
        root_send_message_context,
        repl_turn_index: p.turn_index,
        tool_cache: crate::stream_render::EdgeToolCache::new(
            resolved_tool_policy.max_identical_tool_calls,
        ),
    };

    let bare_mode = std::env::var("ASTRA_BARE")
        .map(|v| v == "1")
        .unwrap_or(false);
    let hook_sets = if bare_mode {
        // Bare mode: skip all hooks
        astra_turn_core::stop_hooks_yaml::TurnHookSets::default()
    } else {
        detect_turn_hook_sets(&project_root, task_profile, p.is_plan_subtask)
    };

    // Build skill resolver — shared with sub-run executor for nested skill invocations.
    let skill_resolver: Option<Arc<dyn astra_runtime::turn::skill_tool::SkillResolver>> = {
        let reg_arc = Arc::clone(&p.unified_skill_registry);
        if reg_arc.is_empty() {
            let _ = reg_arc.discover_all().await;
        }
        let resolver = Arc::new(astra_runtime::skills::UnifiedSkillResolver::new(reg_arc));
        let skills = resolver.available_skills();
        if skills.is_empty() {
            None
        } else {
            Some(resolver as Arc<dyn astra_runtime::turn::skill_tool::SkillResolver>)
        }
    };

    // Build skill executor — fork sub-runs inherit the resolver for nesting.
    let skill_executor: Option<Arc<dyn astra_skills::SkillExecutor>> = {
        let mut subrun_exec = crate::skill_subrun::CliSkillSubRunExecutor::new(
            p.api.clone(),
            p.token.to_string(),
            p.model.map(|m| m.to_string()),
            project_root.clone(),
            parent_perm_mode,
            parent_cancel_token,
        )
        .with_skill_resolver(skill_resolver.clone())
        .with_skill_search(p.skill_search.clone());
        if let Some(session_id) = p.session_id {
            subrun_exec = subrun_exec.with_active_session_id(session_id.to_string());
        }
        let subrun_exec = Arc::new(subrun_exec);
        let isolated = Arc::new(astra_skills::executor::IsolatedSkillExecutor::new(
            subrun_exec,
        ));
        let router = Arc::new(astra_skills::executor::SkillExecutionRouter::new(Some(
            isolated,
        )));
        Some(router as Arc<dyn astra_skills::SkillExecutor>)
    };

    // Pre-compute project-level cross-session context (knowledge backflow P2).
    // Cached per-process via OnceLock since git_root is constant for a session.
    use std::sync::OnceLock;
    static PROJECT_CONTEXT_CACHE: OnceLock<Option<String>> = OnceLock::new();

    let project_context: Option<String> = PROJECT_CONTEXT_CACHE
        .get_or_init(|| {
            let p2_enabled = std::env::var("MO_SESSION_PROJECT_CONTEXT")
                .map(|v| v != "0" && v.to_lowercase() != "false")
                .unwrap_or(true);
            if !p2_enabled {
                return None;
            }
            let git_root = std::process::Command::new("git")
                .args(["rev-parse", "--show-toplevel"])
                .current_dir(&project_root)
                .output()
                .ok()
                .filter(|o| o.status.success())
                .and_then(|o| {
                    String::from_utf8(o.stdout)
                        .ok()
                        .map(|s| s.trim().to_string())
                });
            git_root.and_then(|root| {
                let sid = current_session_id.as_deref();
                let summaries =
                    astra_services::session_workspace::list_sessions_by_git_root(&root, sid, 5);
                if summaries.is_empty() {
                    None
                } else {
                    let ctx = astra_services::session_workspace::format_project_context(&summaries);
                    if ctx.is_empty() { None } else { Some(ctx) }
                }
            })
        })
        .clone();

    let mut state = AgenticLoopState {
        messages,
        tool_results: Vec::new(),
        current_session_id,
        current_run_id: None,
        recursion_depth: 0,
        final_text: String::new(),
        final_text_streamed: false,
        total_prompt: 0,
        total_completion: 0,
        total_cache_read: 0,
        total_cache_creation: 0,
        total_tool_calls: 0,
        total_evidence_tool_calls: 0,
        has_any_usage: false,
        max_turns,
        remaining_turns: max_turns,
        agentic_turn_budget: task_profile.agentic_turn_budget,
        current_round_index: 0,
        llm_rounds_completed: 0,
        turn_guard,
        restricted_tools: initial_restricted,
        boosted_tools: HashSet::new(),
        widen_selection_pending: false,
        step_recorder,
        idempotency_cache: InMemoryIdempotencyCache::new(),
        semantic_dedup: SemanticDedup::new(
            astra_runtime::semantic_dedup::DEFAULT_SIMILARITY_THRESHOLD,
        ),
        call_counts: HashMap::new(),
        max_identical_tool_calls: resolved_tool_policy.max_identical_tool_calls,
        max_tools_per_turn: resolved_tool_policy.max_tools_per_turn,
        repeated_cache_hit_suppression: resolved_tool_policy.repeated_cache_hit_suppression,
        max_consecutive_empty_name: resolved_tool_policy.max_consecutive_empty_name,
        stall: StallTrackingState {
            turn_sigs: Vec::new(),
            turn_tool_names: Vec::new(),
            events: Vec::new(),
            intent_tool_turns: Vec::new(),
            verdict_events: Vec::new(),
            last_heavy_checkpoint: None,
            tool_call_records: Vec::new(),
            forced_factual_retry: false,
            forced_execution_retry: false,
            forced_execution_escalation: false,
            forced_parallel_batching: false,
            forced_round_budget_phase1: false,
            forced_round_budget_phase2: false,
            introspection_count: 0,
            forced_redundant_reads_corrective: false,
            forced_cache_waste_corrective: false,
            forced_exploration_family_corrective: false,
            forced_exploration_family_phase2: false,
            exploration_family_corrective_family: None,
            nudge_count: 0,
            circuit_breaker: astra_turn_core::loop_circuit_breaker::LoopCircuitBreaker::new(
                circuit_breaker_config,
            ),
            guardrail_tuner: astra_runtime::guardrail_tuning::GuardrailTuner::default(),
            guardrail_tuner_records_cursor: 0,
        },
        telemetry: TelemetryState {
            explain_turns: Vec::new(),
            first_ttft_ms: None,
            all_tools_used: HashSet::new(),
            first_selection_report: None,
            first_budget_pressure: 0.0,
            first_context_assembly_ms: None,
            first_memoria_ms: None,
            first_selector_ms: None,
            first_selector_strategy: None,
            first_selector_confidence: None,
            selector_tokens_in: 0,
            selector_tokens_out: 0,
            all_selected_skills: Vec::new(),
            initial_skill_selector_shortlist: None,
            observability_session: p.observability_session.clone(),
            observability_hub: p.observability_hub.clone(),
            turn_trace_collector: None,
            completed_turns_for_tuning: 0,
            evaluation_persistence: None,
            context_trace_persistence: None,
            promotion_events: Vec::new(),
            pending_context_assembly_trace: None,
        },
        skills: SkillState {
            registry_for_activation: Some(Arc::clone(&p.unified_skill_registry)),
            resolver: skill_resolver,
            executor: skill_executor,
            quality_tracker: p.skill_quality_tracker.clone(),
            improvement_tracker: astra_skills::improvement::ImprovementTracker::new(),
            pinned: std::collections::HashSet::new(),
            discovered: discovered_skills,
            search: p.skill_search.clone(),
            tool_event_hooks: if bare_mode {
                Default::default()
            } else {
                astra_skills::hooks::load_tool_event_hooks(&project_root)
            },
            session_event_hooks: if bare_mode {
                Default::default()
            } else {
                astra_skills::hooks::load_session_event_hooks(&project_root)
            },
            listing_message: None,
            invoked: std::collections::HashMap::new(),
            ..Default::default()
        },
        hooks: StopHookState {
            stop_hooks: hook_sets.stop_hooks,
            stop_hook_runs: 0,
            teammate_idle_hooks: hook_sets.teammate_idle_hooks,
            teammate_idle_hook_runs: 0,
            workspace_root_hint: Some(project_root.to_string_lossy().into_owned()),
            forward_headers: std::collections::HashMap::new(),
            llm_token_service: None,
        },
        messaging: MessagingState {
            mailbox: root_mailbox,
            ack_tracker: None,
            metrics: p.messaging_metrics.clone(),
            progress_emitter: None,
            ..Default::default()
        },
        cancellation: CancellationState {
            flag: None,
            pause_flag: None,
            token: p.cancel_token.clone(),
        },
        error_recovery: ErrorRecoveryState {
            consecutive_same_error: 0,
            last_error_category: None,
        },
        message: p.message.to_string(),
        recent_tools: p.recent_tools.to_vec(),
        task_profile,
        last_turn_policy: astra_runtime::turn::agentic_loop_host::TurnInteractionPolicy::default(),
        api: p.api.clone(),
        api_token: p.token.to_string(),
        delegation_engine: p.delegation_engine,
        delegations_this_turn: 0,
        project_context,
        checkpoint_gate: None,
        evolution_service: p.evolution_service.clone(),
        rate_limit_cooldown: Default::default(),
        data_snapshot_provider: None,
        last_composite_snapshot: None,
        last_measured_prompt_tokens: None,
        consecutive_context_window_errors: 0,
        compaction_effectiveness: Default::default(),
        pinned_tool_schema_tokens: pinned_schema_tokens,
        max_turn_input_tokens: RuntimeLimits::global().max_turn_input_tokens,
        budget_wrapup_injected: false,
        skill_produced_output: false,
        max_cumulative_tokens: 0,
        thinking: astra_turn_core::thinking_config::ThinkingConfig::Off,
        recent_file_reads: Vec::new(),
        permission_context: None,
        permission_handler: None,
        tactical_adapter: None,
        step_signal_collector: None,
        tool_budget_override: None,
        pending_reflection_signals: Vec::new(),
        recent_tactical_actions: Vec::new(),
        server_tool_executor: None,
        interruption: None,
        session_facts: Default::default(),
        continuity: p.runtime_continuity.cloned().unwrap_or_default(),
        compact_strategy: astra_turn_core::microcompact::CompactStrategy::from_provider_and_model(
            p.provider, p.model,
        ),
        approval_overrides: initial_approval_overrides,
        confidence_trend: Default::default(),
        last_confidence_diagnosis: None,
        // turn_index is 0-based (pre-increment); turn events are written
        // after state.turn += 1, so add 1 here to keep llm_round.turn
        // consistent with the turn event's turn number.
        session_turn: p.turn_index + 1,
        bridge_turn_chain_id: Some(uuid::Uuid::now_v7().to_string()),
        bridge_user_query_event_id: Some(uuid::Uuid::now_v7().to_string()),
        turn_event_buffer: None,
    };

    // ─── Run the runtime loop ────────────────────────────────────────────
    // Stop the early spinner — the per-turn prep spinner inside execute_turn takes over.
    if let Some(s) = early_spinner {
        s.stop_clear();
    }
    if let Err(e) = run_agentic_loop_with_host(&mut host, &mut state).await {
        finalize_root_mailbox(p.root_mailbox_slot, &mut state.messaging.mailbox).await;
        if let Some(shared) = p.discovered_skills {
            *shared = state.skills.discovered;
        }
        let (tool_calls_count, tools_used) = resolved_tool_metrics(
            state.total_tool_calls,
            state.telemetry.all_tools_used.iter().cloned(),
            &state.stall.tool_call_records,
        );
        return Err(crate::TurnFailure {
            error: e.to_string(),
            partial: crate::PartialTurnData {
                tool_call_records: std::mem::take(&mut state.stall.tool_call_records),
                tools_used,
                stall_events: std::mem::take(&mut state.stall.events),
                verdict_events: std::mem::take(&mut state.stall.verdict_events),
                prompt_tokens: state.total_prompt,
                completion_tokens: state.total_completion,
                tool_calls_count,
                tool_health_export: state.turn_guard.health.export_merged(p.tool_health_entries),
                session_id: state.current_session_id.clone(),
                last_heavy_checkpoint: state.stall.last_heavy_checkpoint.take(),
                partial_text: std::mem::take(&mut state.final_text),
            },
        });
    }

    // ─── Finalize ────────────────────────────────────────────────────────
    // Merge skill quality data back to session-scoped tracker
    *p.skill_quality_tracker = state.skills.quality_tracker.clone();
    if let Some(shared) = p.discovered_skills {
        *shared = state.skills.discovered.clone();
    }
    finalize_root_mailbox(p.root_mailbox_slot, &mut state.messaging.mailbox).await;

    eprint_stream_loop_sidecars(StreamLoopSidecarEprint {
        explain: p.explain,
        quiet: p.render_policy.is_silent(),
        verbose_mode: p.verbose_mode,
        start,
        model: p.model,
        explain_turns: &state.telemetry.explain_turns,
        verdict_events: &state.stall.verdict_events,
        has_any_usage: state.has_any_usage,
        total_prompt: state.total_prompt,
        total_cache_read: state.total_cache_read,
        total_cache_creation: state.total_cache_creation,
        total_completion: state.total_completion,
        current_session_id: state.current_session_id.as_deref(),
    });

    let final_messages = std::mem::take(&mut state.messages);

    let result = build_stream_result(StreamResultBuild {
        tool_health_entries: p.tool_health_entries,
        session_id: state.current_session_id,
        run_id: state.current_run_id,
        full_text: state.final_text,
        prompt_tokens: state.total_prompt,
        completion_tokens: state.total_completion,
        cache_read_tokens: state.total_cache_read,
        cache_creation_tokens: state.total_cache_creation,
        tool_calls_count: state.total_tool_calls,
        first_selection_report: state.telemetry.first_selection_report,
        selected_skills: state.telemetry.all_selected_skills,
        tools_used: state.telemetry.all_tools_used,
        tool_call_records: state.stall.tool_call_records,
        budget_pressure: state.telemetry.first_budget_pressure,
        stall_events: state.stall.events,
        verdict_events: state.stall.verdict_events,
        step_recorder: &state.step_recorder,
        turn_guard: &state.turn_guard,
        last_heavy_checkpoint: state.stall.last_heavy_checkpoint,
        runtime_continuity: state.continuity,
        ttft_ms: state.telemetry.first_ttft_ms,
        context_ms: state.telemetry.first_context_assembly_ms,
        selector_strategy: state.telemetry.first_selector_strategy,
        selector_ms: state.telemetry.first_selector_ms,
        selector_confidence: state.telemetry.first_selector_confidence,
        selector_tokens_in: state.telemetry.selector_tokens_in,
        selector_tokens_out: state.telemetry.selector_tokens_out,
        memoria_ms: state.telemetry.first_memoria_ms,
        routing_domain_hint: None,
        entity_learn_skipped_no_domain: false,
        pending_context_assembly_trace: state.telemetry.pending_context_assembly_trace,
        turn_observability_events: state
            .turn_event_buffer
            .as_mut()
            .map(|b| b.drain())
            .unwrap_or_default(),
        llm_rounds: state.turn_event_buffer.as_ref().map(|b| b.current_round()),
        interruption: state.interruption.as_ref().map(|i| i.to_json()),
        final_messages,
    });
    Ok(result)
}

fn load_turn_messages(
    pre_loaded_messages: Option<Vec<serde_json::Value>>,
    history: &[(String, String)],
    current_message: &str,
) -> Vec<serde_json::Value> {
    if let Some(mut msgs) = pre_loaded_messages {
        msgs.push(json!({"role": "user", "content": current_message}));
        return msgs;
    }
    openai_messages_from_repl_history(history, current_message)
}

#[cfg(test)]
mod tests {
    use super::circuit_breaker_config_from_tool_selection;
    use super::detect_turn_hook_sets;
    use super::extend_restricted_with_blocked_tools;
    use astra_pipeline::pattern::PatternLibrary;
    use astra_runtime::evolution::types::PatternAction;
    use astra_runtime::observability_integration::ObservabilityHub;
    use astra_turn_core::chat_turn_heuristics::infer_task_execution_profile;
    use astra_turn_core::routing_engine::TaskType;
    use std::collections::HashSet;
    use std::path::Path;
    use std::sync::{Arc, Mutex};
    use tempfile::tempdir;

    #[test]
    fn circuit_breaker_config_uses_runtime_config_defaults() {
        let cfg = circuit_breaker_config_from_tool_selection(
            &astra_config::runtime_config::ToolSelectionConfig::default(),
        );

        assert_eq!(cfg.stall_threshold, 3);
        assert_eq!(cfg.repetition_threshold, 3);
        assert_eq!(cfg.read_only_stall_threshold, 12);
        // `0` in user config means "use default (3)", NOT the BreakerConfig sentinel "unbounded".
        assert_eq!(cfg.max_introspect_emissions, 3);
        assert_eq!(cfg.half_open_patience, 2);
        assert_eq!(cfg.absolute_max_rounds, 200);
    }

    #[test]
    fn circuit_breaker_config_uses_runtime_config_overrides_with_floors() {
        let tool_selection = astra_config::runtime_config::ToolSelectionConfig {
            circuit_breaker_stall_threshold: 1,
            circuit_breaker_repetition_threshold: 7,
            circuit_breaker_read_only_stall_threshold: 2,
            // user=0 → effective_*() returns default (3); floor is 1
            circuit_breaker_max_introspect_emissions: 0,
            circuit_breaker_half_open_patience: 5,
            circuit_breaker_absolute_max_rounds: 10,
            ..Default::default()
        };

        let cfg = circuit_breaker_config_from_tool_selection(&tool_selection);

        assert_eq!(cfg.stall_threshold, 2);
        assert_eq!(cfg.repetition_threshold, 7);
        assert_eq!(cfg.read_only_stall_threshold, 4);
        // user=0 means "use default" → 3, never unbounded (BreakerConfig.0 sentinel)
        assert_eq!(cfg.max_introspect_emissions, 3);
        assert_eq!(cfg.half_open_patience, 5);
        assert_eq!(cfg.absolute_max_rounds, 20);
    }

    #[test]
    fn circuit_breaker_config_introspect_floor_is_one() {
        // user supplies explicit value=1 (at the floor) — should pass through unchanged
        let tool_selection = astra_config::runtime_config::ToolSelectionConfig {
            circuit_breaker_max_introspect_emissions: 1,
            ..Default::default()
        };
        let cfg = circuit_breaker_config_from_tool_selection(&tool_selection);
        assert_eq!(cfg.max_introspect_emissions, 1);
    }

    #[test]
    fn read_only_requests_skip_stop_hooks() {
        let s = detect_turn_hook_sets(
            Path::new("."),
            infer_task_execution_profile("review 最新的commit"),
            false,
        );
        assert!(s.stop_hooks.is_empty());
        let s = detect_turn_hook_sets(
            Path::new("."),
            infer_task_execution_profile("explain this diff"),
            false,
        );
        assert!(s.stop_hooks.is_empty());
    }

    #[test]
    fn mutating_requests_keep_stop_hooks() {
        let s = detect_turn_hook_sets(
            Path::new("."),
            infer_task_execution_profile("implement a fix for the failing test"),
            false,
        );
        // Smart hook returns a single "verify-changes" entry (if project detected)
        // or empty (if no project markers in cwd)
        let _ = s.stop_hooks;
    }

    #[test]
    fn plan_subtask_ignores_when_stop_uses_task_completed() {
        let dir = tempdir().unwrap();
        let mo = dir.path().join(".astra");
        std::fs::create_dir_all(&mo).unwrap();
        std::fs::write(
            mo.join("stop-hooks.yaml"),
            r#"version: 1
auto_detect: false
hooks:
  - label: global
    command: echo stop
    when: stop
  - label: sub
    command: echo task
    when: task_completed
"#,
        )
        .unwrap();
        let prof = infer_task_execution_profile("implement the subtask");
        let s = detect_turn_hook_sets(dir.path(), prof, true);
        assert_eq!(s.stop_hooks.len(), 1);
        assert_eq!(s.stop_hooks[0].label, "sub");
    }

    #[test]
    fn plan_subtask_empty_without_task_completed_hooks() {
        let dir = tempdir().unwrap();
        let mo = dir.path().join(".astra");
        std::fs::create_dir_all(&mo).unwrap();
        std::fs::write(
            mo.join("stop-hooks.yaml"),
            "version: 1\nauto_detect: false\nhooks:\n  - label: only_stop\n    command: true\n    when: stop\n",
        )
        .unwrap();
        let prof = infer_task_execution_profile("implement the subtask");
        let s = detect_turn_hook_sets(dir.path(), prof, true);
        assert!(s.stop_hooks.is_empty());
    }

    #[test]
    fn teammate_idle_hooks_loaded_alongside_stop() {
        let dir = tempdir().unwrap();
        let mo = dir.path().join(".astra");
        std::fs::create_dir_all(&mo).unwrap();
        std::fs::write(
            mo.join("stop-hooks.yaml"),
            r#"version: 1
auto_detect: false
hooks:
  - label: fin
    command: cargo check
    when: stop
  - label: after_delegate
    command: ./scripts/sync-check.sh
    when: teammate_idle
"#,
        )
        .unwrap();
        let s = detect_turn_hook_sets(
            dir.path(),
            infer_task_execution_profile("fix the bug"),
            false,
        );
        assert_eq!(s.stop_hooks.len(), 1);
        assert_eq!(s.teammate_idle_hooks.len(), 1);
        assert_eq!(s.teammate_idle_hooks[0].label, "after_delegate");
    }

    #[test]
    fn declarative_stop_hooks_apply_on_read_only_turn() {
        let dir = tempdir().unwrap();
        let mo = dir.path().join(".astra");
        std::fs::create_dir_all(&mo).unwrap();
        std::fs::write(
            mo.join("stop-hooks.yaml"),
            "version: 1\nauto_detect: false\nhooks:\n  - label: audit\n    command: ./scripts/audit.sh\n",
        )
        .unwrap();
        let s = detect_turn_hook_sets(
            dir.path(),
            infer_task_execution_profile("explain this file"),
            false,
        );
        assert_eq!(s.stop_hooks.len(), 1);
        assert_eq!(s.stop_hooks[0].label, "audit");
    }

    #[test]
    fn blocked_patterns_seed_initial_restrictions_from_observability_hub() {
        let pattern_library = Arc::new(Mutex::new(PatternLibrary::new()));
        {
            let mut lib = pattern_library.lock().unwrap();
            let tools = vec!["grep".to_string()];
            lib.record_outcome(&tools, TaskType::Code, None, true, 0.8, None);
            lib.apply_evolution_action("grep", PatternAction::Block);
        }

        let hub = Arc::new(ObservabilityHub::new());
        hub.attach_pattern_library(pattern_library);

        let mut restricted = HashSet::new();
        extend_restricted_with_blocked_tools(&mut restricted, Some(&hub));

        assert!(restricted.contains("grep"));
    }
}
