//! SSE multi-turn agentic loop (`stream_chat_sse`).
//!
//! Entry [`stream_chat_sse`] builds a [`CliAgenticLoopHost`] + [`AgenticLoopState`],
//! runs the runtime's [`run_agentic_loop_with_host`], then finalizes to [`StreamResult`].
//! One iteration is driven by the runtime; the host handles payload prep + HTTP + SSE.

mod agentic_loop_turn;
mod agentic_sse_loop;
mod cli_loop_host;

use std::collections::HashSet;
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
    turn::agentic_loop_host::{AgenticLoopState, run_agentic_loop_with_host},
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
};
use cli_loop_host::CliAgenticLoopHost;
use serde_json::json;

pub(crate) async fn stream_chat_sse(
    mut p: ChatTurnParams<'_>,
) -> Result<StreamResult, crate::TurnFailure> {
    let start = Instant::now();
    let term_width = terminal_width_usize();

    // Paint an immediate spinner so the user sees feedback during init (executor, schemas,
    // skill discovery, etc.) before the per-turn prep spinner takes over.
    let show_early_hint = !p.quiet
        && !p.suppress_intermediate_output
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
        if let Some(ref mgr) = p.mcp_manager {
            ex.with_mcp_manager(mgr.clone())
        } else {
            ex
        }
    };

    // --add-dir: expand sandbox to include additional directories
    if let Ok(dirs) = std::env::var("ASTRA_ADD_DIRS") {
        for dir in dirs.split(':').filter(|s| !s.is_empty()) {
            executor.expand_sandbox_path(PathBuf::from(dir));
        }
    }
    let mut messages = openai_messages_from_repl_history(p.history, p.message);
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

    let current_session_id = p.session_id.map(|s| s.to_string());
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

    let max_turns = RuntimeLimits::global().max_turns;
    let step_recorder = StepRecorder::with_persistence(
        current_session_id.as_deref().unwrap_or("ephemeral"),
        step_recorder_chat_ephemeral_run_id(start.elapsed().as_millis()).as_str(),
    );
    let mut local_discovered_skills = HashSet::new();
    let discovered_skills = match p.discovered_skills.as_deref_mut() {
        Some(shared) => std::mem::take(shared),
        None => std::mem::take(&mut local_discovered_skills),
    };

    // Capture full permission mode before perm_manager is moved into the host.
    let parent_perm_mode = p.perm_manager.mode();
    let parent_cancel_token = p.cancel_token.clone();

    // ─── Build host + state ──────────────────────────────────────────────
    let mut host = CliAgenticLoopHost {
        api: p.api,
        token: p.token,
        model: p.model,
        explain: p.explain,
        render_md: p.render_md,
        term_width,
        quiet: p.quiet,
        suppress_intermediate_output: p.suppress_intermediate_output,
        hide_streaming_assistant_text: p.hide_streaming_assistant_text,
        message: p.message,
        history: p.history,
        recent_tools: p.recent_tools,
        project_root: project_root.clone(),
        executor,
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
    };

    let bare_mode = std::env::var("ASTRA_BARE")
        .map(|v| v == "1")
        .unwrap_or(false);
    let hook_sets = if bare_mode {
        // Bare mode: skip all hooks
        astra_runtime::turn::stop_hooks_yaml::TurnHookSets::default()
    } else {
        detect_turn_hook_sets(&project_root, task_profile, p.is_plan_subtask)
    };

    // Build skill resolver — shared with sub-run executor for nested skill invocations.
    let skill_resolver: Option<Arc<dyn astra_runtime::turn::skill_tool::SkillResolver>> = {
        let reg_arc = Arc::clone(&p.unified_skill_registry);
        if reg_arc.is_empty() {
            let _ = reg_arc.discover_all().await;
        }
        let inner_resolver = Arc::new(astra_runtime::skills::UnifiedSkillResolver::new(reg_arc));
        let adapter =
            astra_runtime::skills::registry::LegacySkillResolverAdapter::new(inner_resolver);
        let skills = adapter.available_skills();
        if skills.is_empty() {
            None
        } else {
            Some(Arc::new(adapter) as Arc<dyn astra_runtime::turn::skill_tool::SkillResolver>)
        }
    };

    // Build skill executor — fork sub-runs inherit the resolver for nesting.
    let skill_executor: Option<Arc<dyn astra_runtime::skills::SkillExecutor>> = {
        let subrun_exec = Arc::new(
            crate::skill_subrun::CliSkillSubRunExecutor::new(
                p.api.clone(),
                p.token.to_string(),
                p.model.map(|m| m.to_string()),
                project_root.clone(),
                parent_perm_mode,
                parent_cancel_token,
            )
            .with_skill_resolver(skill_resolver.clone())
            .with_skill_search(p.skill_search.clone()),
        );
        let isolated = Arc::new(astra_runtime::skills::executor::IsolatedSkillExecutor::new(
            subrun_exec,
        ));
        let router = Arc::new(astra_runtime::skills::executor::SkillExecutionRouter::new(
            Some(isolated),
        ));
        Some(router as Arc<dyn astra_runtime::skills::SkillExecutor>)
    };

    let mut state = AgenticLoopState {
        messages,
        tool_results: Vec::new(),
        current_session_id,
        current_run_id: None,
        final_text: String::new(),
        total_prompt: 0,
        total_completion: 0,
        total_cache_read: 0,
        total_cache_creation: 0,
        total_tool_calls: 0,
        has_any_usage: false,
        max_turns,
        remaining_turns: max_turns,
        turn_guard,
        restricted_tools: initial_restricted,
        step_recorder,
        idempotency_cache: InMemoryIdempotencyCache::new(),
        semantic_dedup: SemanticDedup::new(
            astra_runtime::semantic_dedup::DEFAULT_SIMILARITY_THRESHOLD,
        ),
        turn_sigs: Vec::new(),
        turn_tool_names: Vec::new(),
        stall_events: Vec::new(),
        intent_tool_turns: Vec::new(),
        verdict_events: Vec::new(),
        last_heavy_checkpoint: None,
        tool_call_records: Vec::new(),
        forced_factual_retry: false,
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
        message: p.message.to_string(),
        recent_tools: p.recent_tools.to_vec(),
        task_profile,
        api: p.api.clone(),
        api_token: p.token.to_string(),
        cancel_flag: None,
        cancel_token: p.cancel_token.clone(),
        delegation_engine: p.delegation_engine,
        skill_registry_for_activation: Some(Arc::clone(&p.unified_skill_registry)),
        skill_resolver,
        skill_executor,
        skill_model_override: None,
        skill_effort: None,
        skill_agent_type: None,
        skill_allowed_tools: None,
        skill_sandbox_policy: None,
        skill_quality_tracker: p.skill_quality_tracker.clone(),
        skill_improvement_tracker: astra_runtime::skills::improvement::ImprovementTracker::new(),
        pinned_skills: std::collections::HashSet::new(),
        discovered_skills,
        skill_search: p.skill_search.clone(),
        tool_event_hooks: if bare_mode {
            Default::default()
        } else {
            astra_runtime::skills::hooks::load_tool_event_hooks(&project_root)
        },
        session_event_hooks: if bare_mode {
            Default::default()
        } else {
            astra_runtime::skills::hooks::load_session_event_hooks(&project_root)
        },
        stop_hooks: hook_sets.stop_hooks,
        stop_hook_runs: 0,
        teammate_idle_hooks: hook_sets.teammate_idle_hooks,
        teammate_idle_hook_runs: 0,
        workspace_root_hint: Some(project_root.to_string_lossy().into_owned()),
        consecutive_same_error: 0,
        last_error_category: None,
        checkpoint_gate: None,
        data_snapshot_provider: None,
        last_composite_snapshot: None,
        last_measured_prompt_tokens: None,
        consecutive_context_window_errors: 0,
        max_turn_input_tokens: RuntimeLimits::global().max_turn_input_tokens,
        budget_wrapup_injected: false,
        thinking_budget_tokens: None,
        skill_listing_message: None,
        invoked_skills: std::collections::HashMap::new(),
        recent_file_reads: Vec::new(),
        mailbox: None,
        ack_tracker: None,
        dead_letter_queue: None,
    };

    // ─── Run the runtime loop ────────────────────────────────────────────
    // Stop the early spinner — the per-turn prep spinner inside execute_turn takes over.
    if let Some(s) = early_spinner {
        s.stop_clear();
    }
    if let Err(e) = run_agentic_loop_with_host(&mut host, &mut state).await {
        if let Some(shared) = p.discovered_skills {
            *shared = state.discovered_skills;
        }
        return Err(crate::TurnFailure {
            error: e,
            partial: crate::PartialTurnData {
                tool_call_records: std::mem::take(&mut state.tool_call_records),
                tools_used: state.all_tools_used.iter().cloned().collect(),
                stall_events: std::mem::take(&mut state.stall_events),
                verdict_events: std::mem::take(&mut state.verdict_events),
                prompt_tokens: state.total_prompt,
                completion_tokens: state.total_completion,
                tool_calls_count: state.total_tool_calls,
                tool_health_export: state.turn_guard.health.export_merged(p.tool_health_entries),
                session_id: state.current_session_id.clone(),
                last_heavy_checkpoint: state.last_heavy_checkpoint.take(),
                partial_text: std::mem::take(&mut state.final_text),
            },
        });
    }

    // ─── Finalize ────────────────────────────────────────────────────────
    // Merge skill quality data back to session-scoped tracker
    *p.skill_quality_tracker = state.skill_quality_tracker.clone();
    if let Some(shared) = p.discovered_skills {
        *shared = state.discovered_skills.clone();
    }

    eprint_stream_loop_sidecars(StreamLoopSidecarEprint {
        explain: p.explain,
        quiet: p.quiet || p.suppress_intermediate_output,
        verbose_mode: p.verbose_mode,
        start,
        model: p.model,
        explain_turns: &state.explain_turns,
        verdict_events: &state.verdict_events,
        has_any_usage: state.has_any_usage,
        total_prompt: state.total_prompt,
        total_completion: state.total_completion,
        current_session_id: state.current_session_id.as_deref(),
    });

    Ok(build_stream_result(StreamResultBuild {
        tool_health_entries: p.tool_health_entries,
        session_id: state.current_session_id,
        run_id: state.current_run_id,
        full_text: state.final_text,
        prompt_tokens: state.total_prompt,
        completion_tokens: state.total_completion,
        cache_read_tokens: state.total_cache_read,
        cache_creation_tokens: state.total_cache_creation,
        tool_calls_count: state.total_tool_calls,
        first_selection_report: state.first_selection_report,
        selected_skills: state.all_selected_skills,
        tools_used: state.all_tools_used,
        tool_call_records: state.tool_call_records,
        budget_pressure: state.first_budget_pressure,
        stall_events: state.stall_events,
        verdict_events: state.verdict_events,
        step_recorder: &state.step_recorder,
        turn_guard: &state.turn_guard,
        last_heavy_checkpoint: state.last_heavy_checkpoint,
        ttft_ms: state.first_ttft_ms,
        context_ms: state.first_context_assembly_ms,
        selector_strategy: state.first_selector_strategy,
        selector_ms: state.first_selector_ms,
        selector_confidence: state.first_selector_confidence,
        selector_tokens_in: state.selector_tokens_in,
        selector_tokens_out: state.selector_tokens_out,
        memoria_ms: state.first_memoria_ms,
        routing_domain_hint: None,
        entity_learn_skipped_no_domain: false,
    }))
}

#[cfg(test)]
mod tests {
    use super::detect_turn_hook_sets;
    use astra_runtime::turn::chat_turn_heuristics::infer_task_execution_profile;
    use std::path::Path;
    use tempfile::tempdir;

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
}
