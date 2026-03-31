//! Multi-turn `/chat/stream` loop (`stream_chat_sse`), kept here so `sse_loop/` can split further without one monolithic file.

use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
    time::Instant,
};

use crossterm::style::Stylize;
use crossterm::terminal;
use mo_agent_core::{RuntimeLimits, agent_warn};
use mo_agent_runtime::{
    pipeline::step_protocol::{CachedToolResult, IdempotencyKey, InMemoryIdempotencyCache},
    tool_registry::{
        self,
        apply_selector_hints_to_edge_profile,
    },
    tool_selector,
    turn::boost_domain_hints::domain_hints_from_boost_terms,
    turn::chat_history_openai::{
        append_openai_user_content_messages, openai_messages_from_repl_history,
        openai_user_content_message,
    },
    turn::chat_turn_edge_profile::{detect_active_system_skills_in_message, read_git_branch_abbrev},
    turn::chat_turn_payload::{
        chat_turn_base_payload, merge_active_skills_into_edge_profile,
        merge_skill_instructions_into_edge_profile, set_payload_edge_tools,
        set_payload_tool_results_if_non_empty, ChatTurnBasePayloadInput,
    },
    turn::chat_turn_heuristics::{
        extract_repos_from_memory, openai_factual_tool_retry_user_message,
        should_force_factual_tool_retry,
    },
    turn::edge_prompt_context::{detect_project_languages, make_args_preview},
    turn::tool_schema_prune::{filter_tool_schemas_by_excluded_names, pin_invoked_tool_schemas},
    turn::headless_tool_assembly::{
        openai_assistant_with_tool_calls_message, openai_tool_roundtrip_values,
        take_edge_output_for_tool_call, tool_calls_for_stall_guard, CACHEABLE_TOOLS,
    },
    turn::tool_result_semantics::{is_resource_limit_output, is_tool_error, tool_dedup_signature},
};

use crate::{
    cli_utils::{compact_or_raw, tool_call_detail, tool_result_summary},
    edge_tools,
    stream_render::consume_turn_sse,
    ExplainMode, StreamResult, VerdictEvent,
};

use super::explain_sidecar::{
    eprint_restricted_tools_explain, eprint_selector_guidance_explain,
};
use super::skill_instructions_round::{load_skill_instructions_text, merge_skill_names_track};
use super::super::{
    edge_executor::edge_executor_instance_id,
    explain_reports::{print_explain_report, print_verdict_report},
    hydrate_reflect::hydrate_reflect_placeholder_if_needed,
    ChatTurnParams,
};

pub(crate) async fn stream_chat_sse(p: ChatTurnParams<'_>) -> Result<StreamResult, String> {
    // Destructure for readability within the function body
    let ChatTurnParams {
        api,
        token,
        message,
        session_id,
        model,
        explain,
        render_md,
        history,
        perm_manager,
        verbose_mode,
        quiet,
        selector,
        recent_tools,
        tool_health_entries,
        skill_registry,
    } = p;
    let start = Instant::now();
    let term_width = terminal::size().map(|(w, _)| w as usize).unwrap_or(80);
    let project_root = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let file_context = detect_project_languages(&project_root);
    let executor = edge_tools::ToolExecutor::new(&project_root).with_cloud(api.api_origin(), token);
    let all_schemas = edge_tools::all_tool_schemas();
    let registry = tool_registry::ToolRegistry::new(all_schemas.clone());
    let valid_tool_names: HashSet<String> = all_schemas
        .iter()
        .filter_map(|s| {
            s.get("function")
                .and_then(|f| f.get("name"))
                .and_then(|n| n.as_str())
                .map(String::from)
        })
        .collect();

    let mut current_session_id: Option<String> = session_id.map(|s| s.to_string());
    // Build messages: history + current user message
    let mut messages: Vec<serde_json::Value> = openai_messages_from_repl_history(history, message);

    let mut tool_results: Vec<serde_json::Value> = Vec::new();
    let mut final_text = String::new();
    let mut total_prompt = 0u64;
    let mut total_completion = 0u64;
    let mut total_tool_calls = 0u32;
    let mut has_any_usage = false;
    let mut explain_turns: Vec<serde_json::Value> = Vec::new();
    // Track first-turn selection report and all unique tools actually used
    let mut first_selection_report: Option<tool_registry::SelectionReport> = None;
    let mut first_budget_pressure: f64 = 0.0;
    let mut all_tools_used: std::collections::HashSet<String> = std::collections::HashSet::new();

    let mut turn_sigs: Vec<std::collections::BTreeSet<String>> = Vec::new();
    let mut turn_tool_names: Vec<std::collections::HashSet<String>> = Vec::new();
    let mut forced_factual_retry = false;
    const TOOL_NAME_STALL_WINDOW: usize = 3;
    let mut current_run_id: Option<String> = None;
    let mut stall_events: Vec<(String, u32)> = Vec::new();
    let mut verdict_events: Vec<VerdictEvent> = Vec::new();
    let mut last_heavy_checkpoint: Option<
        mo_agent_runtime::pipeline::step_protocol::StepCheckpoint,
    > = None;
    let mut tool_call_records: Vec<mo_agent_services::session_journal::ToolCallRecord> = Vec::new();
    // Capture first turn's TTFT for observability
    let mut first_ttft_ms: Option<u64> = None;
    // Cross-turn dedup: IdempotencyCache with content-hash keys (Step Protocol)
    let mut idempotency_cache = InMemoryIdempotencyCache::new();
    // Semantic near-duplicate tracker (Tier 2: param-aware, Tier 3: output similarity)
    let mut semantic_dedup = mo_agent_runtime::semantic_dedup::SemanticDedup::new(
        mo_agent_runtime::semantic_dedup::DEFAULT_SIMILARITY_THRESHOLD,
    );
    // Unified non-happy-path guard: stall + divergence + tool health + error recovery + escalation
    let mut turn_guard = if tool_health_entries.is_empty() {
        mo_agent_runtime::turn::turn_guard::TurnGuard::new()
    } else {
        let health = mo_agent_runtime::turn::tool_health::ToolHealthTracker::from_entries(
            tool_health_entries,
        );
        mo_agent_runtime::turn::turn_guard::TurnGuard::with_health(health)
    };
    // Stall enforcement: tools restricted from schema after nudge-ignore
    let mut restricted_tools: HashSet<String> = HashSet::new();
    // Dynamic turn budget: each stall/divergence costs turns to prevent runaway sessions
    let max_turns = RuntimeLimits::global().max_turns;
    let mut remaining_turns: usize = max_turns;
    // Intent drift tracker: per-turn tool names + args for drift detection
    let mut intent_tool_turns: Vec<(Vec<String>, String)> = Vec::new();
    // Step Protocol recorder: maps implicit chat_stream phases to explicit Step events
    let mut step_recorder =
        mo_agent_runtime::pipeline::step_recorder::StepRecorder::with_persistence(
            current_session_id.as_deref().unwrap_or("ephemeral"),
            &format!("chat-{}", start.elapsed().as_millis()),
        );

    // Track first turn's context assembly time for observability
    let mut first_context_assembly_ms: Option<u64> = None;
    let mut first_memoria_ms: Option<u64> = None;
    let mut first_selector_ms: Option<u64> = None;
    let mut first_selector_strategy: Option<String> = None;
    let mut selector_tokens_in: u64 = 0;
    let mut selector_tokens_out: u64 = 0;
    let mut all_selected_skills: Vec<String> = Vec::new();

    for _turn in 0..max_turns {
        if remaining_turns == 0 {
            return Err("Turn budget exhausted due to repeated stalls. Aborting.".to_string());
        }
        remaining_turns = remaining_turns.saturating_sub(1);
        step_recorder.begin_turn(_turn as u32);

        // Track context assembly time
        let assembly_start = Instant::now();

        // Build request payload (invariant top-level keys; tools / tool_results added below).
        let git_branch = read_git_branch_abbrev();
        let mut payload = chat_turn_base_payload(ChatTurnBasePayloadInput {
            messages: &messages,
            session_id: current_session_id.as_deref(),
            model,
            explain_verbose: matches!(explain, ExplainMode::Verbose),
            explain_on: matches!(explain, ExplainMode::On),
            edge_executor_id: edge_executor_instance_id(),
            capabilities: mo_thin_client::builtin_capability_preset(),
            project_root: &project_root,
            git_branch,
        });
        let active_skills = detect_active_system_skills_in_message(message);
        merge_active_skills_into_edge_profile(&mut payload, &active_skills);
        // NOTE: Skill instructions are now injected after tool selection (see below)
        // when LLM-based selection chooses a skill.
        
        // Tool selection via pluggable ToolSelector strategy.
        // First turn: selector decides which tools. Follow-up turns: also pin
        // tools the LLM already invoked so they remain available.

        // ── Budget pressure: pre-estimate token usage to reduce tool count ──
        // When context is filling up, select fewer dynamic tools to save tokens.
        // Uses precise estimation with actual schema token costs when available.
        let budget_pressure = {
            let schema_tokens = selector.registry().total_pinned_token_cost();
            let estimated = mo_agent_runtime::prompts::estimate_tokens_precise(
                &messages,
                schema_tokens as usize,
                0, // use default system prompt estimate
            );
            let budget = mo_agent_runtime::prompts::budget_for_model(model);
            let tier = budget.compaction_tier(estimated);
            tier.budget_pressure()
        };

        // Phase 7.5: Memory-augmented boost terms.
        // Step 1: Extract domain keywords from session history (sync, always works).
        let mut boost_terms =
            mo_agent_runtime::turn::retrieval::extract_boost_terms_from_pairs(history, message);
        // Step 2: Augment with memory service (async, best-effort, 2s timeout).
        // On cold-start (no relevant history), memory may still have stored
        // domain hints (e.g., "matrixorigin is a GitHub org") that improve
        // tool selection. This closes the cold-start gap in entity-rich queries.
        //
        // Memory results are re-ranked by TF-IDF cosine similarity to filter
        // irrelevant memories before boost term extraction (Phase A.2).
        {
            let mem_start = Instant::now();
            let memory_contents = executor.memory_boost_search(message, 5).await;
            let mem_elapsed = mem_start.elapsed().as_millis() as u64;
            if first_memoria_ms.is_none() {
                first_memoria_ms = Some(mem_elapsed);
            }
            if !memory_contents.is_empty() {
                // Bridge memory→preferred_repos: extract owner/repo references
                // from memory content so tool executor can resolve bare repo names.
                for content in &memory_contents {
                    for repo in extract_repos_from_memory(content) {
                        executor.add_preferred_repo(&repo);
                    }
                }

                // Re-rank by TF-IDF similarity; filter below threshold.
                let ranked = mo_agent_runtime::turn::retrieval::rank_memory_results(
                    message,
                    &memory_contents,
                );
                mo_agent_runtime::turn::retrieval::append_boost_terms_from_ranked_memory(
                    &mut boost_terms,
                    message,
                    &ranked,
                );
            }
        }

        // ── Extract memory domain hints from boost terms ──
        let memory_domain_hints = domain_hints_from_boost_terms(&boost_terms);

        // Proactively seed restricted_tools with deprioritized tools from health tracker.
        // This ensures cross-session deprioritized tools are excluded BEFORE scoring.
        for tool in turn_guard.health.deprioritized_tools() {
            restricted_tools.insert(tool.to_string());
        }
        let restricted_vec: Vec<String> = restricted_tools.iter().cloned().collect();

        // Record PERCEIVE phase: user query + memory context + domain hints
        step_recorder.record_perceive(
            message,
            &[], // memory IDs not yet tracked individually
            &memory_domain_hints
                .iter()
                .map(|h| format!("{:?}", h))
                .collect::<Vec<_>>(),
            &boost_terms,
        );

        let learned_context = selector.learned_context(message, recent_tools);
        let learned_context_hint = learned_context.prompt_fragment();
        let learned_task_type = learned_context
            .task_archetype
            .map(|task_type| format!("{task_type:?}").to_lowercase());

        // Variables to capture selection results including skills
        let mut selected_skills: Vec<String> = Vec::new();
        
        let (turn_schemas, selection_report, selection_confidence) = if tool_results.is_empty() {
            let sel_start = Instant::now();
            let turn_count = history.len() as u32 + 1;
            let sel_ctx = tool_selector::SelectionContext {
                query: message,
                turn_count,
                recent_tools,
                budget_tokens: registry.default_budget(),
                boost_terms: boost_terms.clone(),
                budget_pressure,
                memory_domain_hints: memory_domain_hints.clone(),
                restricted_tools: restricted_vec.clone(),
                file_context: file_context.clone(),
            };
            let sel_result = selector
                .select_with_learned_context(&sel_ctx, &learned_context)
                .await;
            if first_selector_ms.is_none() {
                first_selector_ms = Some(sel_start.elapsed().as_millis() as u64);
                first_selector_strategy = Some(format!(
                    "{} (conf={:.2})",
                    sel_result.strategy, sel_result.confidence
                ));
            }
            selector_tokens_in += sel_result.selector_tokens_in;
            selector_tokens_out += sel_result.selector_tokens_out;
            
            // Capture selected skills from LLM selection
            selected_skills = sel_result.selected_skills.clone();
            
            let conf = sel_result.confidence;
            let (schemas, report) = tool_selector::resolve_schemas_with_pressure(
                &registry,
                &sel_result.tool_names,
                budget_pressure,
            );
            (schemas, report, conf)
        } else {
            // Follow-up turn: use 2x budget, then pin tools already invoked.
            let turn_count = history.len() as u32 + 1;
            let sel_ctx = tool_selector::SelectionContext {
                query: message,
                turn_count,
                recent_tools,
                budget_tokens: registry.default_budget() * 2,
                boost_terms,
                budget_pressure,
                memory_domain_hints,
                restricted_tools: restricted_vec,
                file_context: file_context.clone(),
            };
            let sel_result = selector
                .select_with_learned_context(&sel_ctx, &learned_context)
                .await;
            
            // Capture selected skills (may be new skills in follow-up)
            if !sel_result.selected_skills.is_empty() {
                selected_skills = sel_result.selected_skills.clone();
            }
            
            let conf = sel_result.confidence;
            let (mut selected, mut report) = tool_selector::resolve_schemas_with_pressure(
                &registry,
                &sel_result.tool_names,
                budget_pressure,
            );
            // Pin schemas for tools the LLM already invoked in prior turns
            pin_invoked_tool_schemas(&mut selected, &mut report, &tool_results, &all_schemas);
            (selected, report, conf)
        };
        
        let skill_instructions =
            load_skill_instructions_text(skill_registry, &selected_skills, quiet);
        merge_skill_names_track(&mut all_selected_skills, &selected_skills);
        
        merge_skill_instructions_into_edge_profile(&mut payload, skill_instructions.as_deref());
        
        if first_selection_report.is_none() {
            first_selection_report = Some(selection_report);
            first_budget_pressure = budget_pressure;
        }
        // Propagate budget pressure to tool executor for output scaling.
        // Updated each iteration so tools always use the latest pressure.
        executor.set_budget_pressure(budget_pressure);

        // ── Tool guidance hint: when the selector is confident, tell the server
        // which dynamic tools scored highest (see `apply_selector_hints_to_edge_profile`).
        apply_selector_hints_to_edge_profile(
            &mut payload["edge_profile"],
            first_selection_report.as_ref(),
            selection_confidence,
            &learned_context_hint,
            learned_task_type.as_deref(),
        );
        // Dynamic schema restriction: remove tools that were stall-restricted
        let final_schemas =
            filter_tool_schemas_by_excluded_names(turn_schemas, &restricted_tools);
        set_payload_edge_tools(&mut payload, final_schemas);
        let explain_stderr = explain != ExplainMode::Off;
        eprint_restricted_tools_explain(explain_stderr, &restricted_tools);
        eprint_selector_guidance_explain(explain_stderr, &payload, selection_confidence);
        set_payload_tool_results_if_non_empty(&mut payload, &tool_results);

        // Step recorder: mark plan phase (tool selection done, LLM call about to start)
        {
            let selected_tool_names: Vec<String> = first_selection_report
                .as_ref()
                .map(|r| r.tools_selected.clone())
                .unwrap_or_default();
            let bp = first_budget_pressure;
            let bt = first_selection_report
                .as_ref()
                .map(|r| r.budget_used as u64)
                .unwrap_or(0);
            step_recorder.record_plan(&selected_tool_names, selection_confidence, bp, bt);
        }

        // Capture context assembly time (first turn only)
        if first_context_assembly_ms.is_none() {
            first_context_assembly_ms = Some(assembly_start.elapsed().as_millis() as u64);
        }

        let resp = api
            .post_chat_turn_retry_429(token, &payload, 3, quiet)
            .await
            .map_err(|e| e.to_string())?;

        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.map_err(|e| e.to_string())?;
            return Err(format!("API Error ({}): {}", status, compact_or_raw(&body)));
        }

        let edge_ctx = crate::stream_render::EdgeSseContext {
            api,
            token,
            executor_id: edge_executor_instance_id(),
            executor: &executor,
            quiet,
            perm_manager: Some(std::ptr::NonNull::from(&mut *perm_manager)),
            _pm: std::marker::PhantomData,
        };
        let turn_result = consume_turn_sse(
            resp,
            render_md,
            term_width,
            quiet,
            Some(edge_ctx),
        )
        .await;

        // Capture TTFT from first turn for observability
        if first_ttft_ms.is_none() {
            first_ttft_ms = turn_result.ttft_ms;
        }

        if let Some(sid) = &turn_result.session_id {
            current_session_id = Some(sid.clone());
        }
        if turn_result.run_id.is_some() {
            current_run_id = turn_result.run_id.clone();
        }
        if !turn_result.full_text.is_empty() {
            final_text = turn_result.full_text.clone();

            // Unified response guard: hard blocks (prompt leak, repetition) + soft quality signals
            let guard = mo_agent_runtime::turn::response_guard::apply_response_guards(
                &final_text,
                &turn_result.tool_calls,
                &[], // tool name validation handled at execution time
                message,
            );
            if let Some(replacement) = guard.replacement {
                agent_warn!("response_guard", "Guard triggered, replacing LLM output");
                final_text = replacement;
                break;
            }
            if guard.quality.has_fabrication_markers {
                agent_warn!(
                    "response_guard",
                    "Fabrication markers detected: placeholder paths in response"
                );
            }
            if guard.quality.is_echo {
                agent_warn!(
                    "response_guard",
                    "Echo detected: LLM repeated user query instead of answering"
                );
            }
        }
        total_prompt += turn_result.prompt_tokens;
        total_completion += turn_result.completion_tokens;
        total_tool_calls += if !turn_result.tool_calls.is_empty() {
            turn_result.tool_calls.len()
        } else {
            turn_result.edge_tool_round.len()
        } as u32;

        // Record LLM token usage in step recorder
        step_recorder.record_tokens(turn_result.prompt_tokens, turn_result.completion_tokens);
        // Track all unique tool names that the LLM actually invoked
        for tc in &turn_result.tool_calls {
            if let Some(name) = tc.get("name").and_then(|v| v.as_str()) {
                all_tools_used.insert(name.to_string());
            }
        }
        for e in &turn_result.edge_tool_round {
            all_tools_used.insert(e.tool.clone());
        }
        has_any_usage = has_any_usage || turn_result.has_usage;
        explain_turns.extend(turn_result.explain_turns);

        if let Some(ref err) = turn_result.error_message {
            return Err(err.clone());
        }

        let round_has_edge_work =
            !turn_result.tool_calls.is_empty() || !turn_result.edge_tool_round.is_empty();
        if !round_has_edge_work {
            if should_force_factual_tool_retry(
                message,
                recent_tools,
                total_tool_calls,
                forced_factual_retry,
            ) {
                forced_factual_retry = true;
                if !quiet {
                    eprintln!(
                        "{}",
                        "  ↻ No tool call on a live-data query; forcing one corrective retry…"
                            .yellow()
                    );
                }
                messages.push(openai_factual_tool_retry_user_message(message));
                final_text.clear();
                continue;
            }
            break;
        }

        let tool_calls_for_guard =
            tool_calls_for_stall_guard(&turn_result.tool_calls, &turn_result.edge_tool_round);

        // Stall & divergence detection via unified TurnGuard
        {
            use std::collections::BTreeSet;

            let sig_set: BTreeSet<String> = tool_calls_for_guard
                .iter()
                .map(|tc| {
                    let name = tc.get("name").and_then(|v| v.as_str()).unwrap_or("");
                    let args = tc.get("arguments").cloned().unwrap_or_default();
                    format!(
                        "{}:{}",
                        name,
                        serde_json::to_string(&args).unwrap_or_default()
                    )
                })
                .collect();
            let name_set: HashSet<String> = tool_calls_for_guard
                .iter()
                .map(|tc| {
                    tc.get("name")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string()
                })
                .collect();
            turn_sigs.push(sig_set);
            turn_tool_names.push(name_set.clone());

            // Feed tool call signatures into TurnGuard
            turn_guard.record_tool_calls(&tool_calls_for_guard);

            // Name-based stall detection (complementary to TurnGuard's signature stall)
            let name_stall = turn_tool_names.len() >= TOOL_NAME_STALL_WINDOW
                && turn_tool_names[turn_tool_names.len() - TOOL_NAME_STALL_WINDOW..]
                    .windows(2)
                    .all(|w| w[0] == w[1]);

            if name_stall {
                stall_events.push(("name_stall".to_string(), _turn as u32));
            }
        }

        // Assemble tool results from SSE `tool_request` only — legacy inline execution removed.
        tool_results = Vec::new();

        let assistant_tc_msg = openai_assistant_with_tool_calls_message(
            &turn_result.tool_calls,
            &turn_result.edge_tool_round,
            &turn_result.reasoning_content,
        );
        messages.push(assistant_tc_msg);

        enum RoundToolItem {
            ServerTc(usize),
            Synthetic(usize),
        }
        let indices: Vec<RoundToolItem> = if !turn_result.tool_calls.is_empty() {
            (0..turn_result.tool_calls.len())
                .map(RoundToolItem::ServerTc)
                .collect()
        } else {
            (0..turn_result.edge_tool_round.len())
                .map(RoundToolItem::Synthetic)
                .collect()
        };

        let tool_count = indices.len().max(1);
        let mut seen_calls: HashSet<String> = HashSet::new();
        step_recorder.begin_act(tool_count);
        let step_start_time = std::time::Instant::now();
        let step_timeout_ms = step_recorder.scheduling().timeout_ms;
        let mut consumed_edge = vec![false; turn_result.edge_tool_round.len()];
        let by_sig: &HashMap<String, String> = &turn_result.edge_callback_outputs;

        for item in &indices {
            let step_elapsed_ms = step_start_time.elapsed().as_millis() as u64;
            if step_elapsed_ms > step_timeout_ms {
                let aborted_count = indices.len() - tool_results.len();
                let aborted_tools: Vec<String> = indices[tool_results.len()..]
                    .iter()
                    .map(|it| match it {
                        RoundToolItem::ServerTc(i) => turn_result.tool_calls[*i]
                            .get("name")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string(),
                        RoundToolItem::Synthetic(i) => turn_result.edge_tool_round[*i].tool.clone(),
                    })
                    .collect();
                agent_warn!(
                    "step",
                    "Step timeout exceeded: {}ms > {}ms, aborting {} tools: {:?}",
                    step_elapsed_ms,
                    step_timeout_ms,
                    aborted_count,
                    aborted_tools
                );
                turn_guard.record_step_abort(&aborted_tools);
                break;
            }

            let (id, name, args, from_synthetic) = match item {
                RoundToolItem::ServerTc(i) => {
                    let tc_event = &turn_result.tool_calls[*i];
                    let id = tc_event
                        .get("id")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    let name = tc_event
                        .get("name")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    let args_raw = tc_event
                        .get("arguments")
                        .cloned()
                        .unwrap_or(serde_json::Value::Object(Default::default()));
                    let args = match args_raw {
                        serde_json::Value::String(s) => serde_json::from_str::<serde_json::Value>(&s)
                            .unwrap_or_else(|_| serde_json::Value::Object(Default::default())),
                        other => other,
                    };
                    (id, name, args, false)
                }
                RoundToolItem::Synthetic(i) => {
                    let e = &turn_result.edge_tool_round[*i];
                    (
                        format!("edge-{i}"),
                        e.tool.clone(),
                        e.args.clone(),
                        true,
                    )
                }
            };

            let call_sig = tool_dedup_signature(&name, &args);
            if !seen_calls.insert(call_sig.clone()) {
                let dup = "(duplicate call — result same as previous identical call this turn)";
                let (tool_msg, tr) = openai_tool_roundtrip_values(&id, &name, dup);
                messages.push(tool_msg);
                tool_results.push(tr);
                tool_call_records.push(mo_agent_services::session_journal::ToolCallRecord {
                    name: name.clone(),
                    ok: true,
                    ms: 0,
                    error: Some("duplicate_within_turn".to_string()),
                    input_bytes: None,
                    output_bytes: None,
                    args_preview: make_args_preview(&name, &args),
                });
                continue;
            }

            let idem_key = IdempotencyKey::semantic(&name, &args);
            if CACHEABLE_TOOLS.contains(&name.as_str())
                && let Some(cached) = idempotency_cache.check(&idem_key)
            {
                let cached_note = format!(
                    "(cached from earlier turn — identical call)\n{}",
                    cached.output
                );
                if !quiet {
                    eprintln!("{}", format!("  ↻ {name} (cached)").dim());
                }
                let (tool_msg, tr) = openai_tool_roundtrip_values(&id, &name, &cached_note);
                messages.push(tool_msg);
                tool_results.push(tr);
                let cache_key = idem_key.cache_key();
                step_recorder.begin_tool_with_key(&name, &id, Some(&cache_key));
                step_recorder.record_cache_hit(&name, cached.clone());
                turn_guard.record_cache_hit(&name);
                tool_call_records.push(mo_agent_services::session_journal::ToolCallRecord {
                    name: name.clone(),
                    ok: true,
                    ms: 0,
                    error: Some("cached_cross_turn".to_string()),
                    input_bytes: None,
                    output_bytes: Some(cached.output.len() as u32),
                    args_preview: make_args_preview(&name, &args),
                });
                continue;
            }

            let mut result_str = if from_synthetic {
                match item {
                    RoundToolItem::Synthetic(i) => turn_result.edge_tool_round[*i].output.clone(),
                    _ => unreachable!(),
                }
            } else {
                take_edge_output_for_tool_call(
                    &name,
                    &args,
                    &turn_result.edge_tool_round,
                    &mut consumed_edge,
                    by_sig,
                )
            };

            if !valid_tool_names.contains(&name) {
                let err_msg = format!(
                    "Unknown tool '{}'. Available: {}",
                    name,
                    valid_tool_names
                        .iter()
                        .cloned()
                        .collect::<Vec<_>>()
                        .join(", ")
                );
                if !quiet {
                    eprintln!("{}", format!("  ✗ {name}").red());
                }
                if !quiet {
                    eprintln!("  {}", format!("└ {err_msg}").dim());
                }
                let (tool_msg, err_tr) = openai_tool_roundtrip_values(&id, &name, &err_msg);
                messages.push(tool_msg);
                tool_results.push(err_tr);
                tool_call_records.push(mo_agent_services::session_journal::ToolCallRecord {
                    name: name.clone(),
                    ok: false,
                    ms: 0,
                    error: Some(format!("unknown_tool: {name}")),
                    input_bytes: None,
                    output_bytes: None,
                    args_preview: None,
                });
                continue;
            }

            result_str = hydrate_reflect_placeholder_if_needed(
                api,
                token,
                current_session_id.as_ref(),
                &name,
                &args,
                result_str,
            )
            .await;

            let tool_start = Instant::now();
            let tool_idem_key = if CACHEABLE_TOOLS.contains(&name.as_str()) {
                Some(idem_key.cache_key())
            } else {
                None
            };
            step_recorder.begin_tool_with_key(&name, &id, tool_idem_key.as_deref());

            let mut is_err = is_tool_error(&result_str);
            let tool_already_restricted = restricted_tools.contains(&name);
            let mut resource_limit_recorded = false;

            if is_err && !tool_already_restricted {
                use mo_agent_runtime::turn::error_recovery::{
                    build_recovery_message, classify_error,
                };
                let category = classify_error(&result_str);

                if matches!(
                    category,
                    mo_agent_runtime::turn::error_recovery::ErrorCategory::ResourceLimit
                ) {
                    turn_guard.health.record_resource_limit_failure(&name);
                    turn_guard.errors.record_error(category);
                    restricted_tools.insert(name.clone());
                    resource_limit_recorded = true;
                    if !quiet {
                        eprintln!(
                            "{}",
                            format!("  ⚠ {name} blocked: system resource limit reached").yellow()
                        );
                    }
                }

                if matches!(
                    category,
                    mo_agent_runtime::turn::error_recovery::ErrorCategory::Transient
                ) {
                    turn_guard.errors.record_retry(false);
                }

                let deprioritized = turn_guard.health.deprioritized_tools();
                let recovery_msg =
                    build_recovery_message(&name, &result_str, category, &deprioritized);
                result_str.push_str(&format!("\n{recovery_msg}"));
            }

            if !is_err && !tool_already_restricted && is_resource_limit_output(&result_str) {
                turn_guard.health.record_resource_limit_failure(&name);
                turn_guard.errors.record_error(
                    mo_agent_runtime::turn::error_recovery::ErrorCategory::ResourceLimit,
                );
                restricted_tools.insert(name.clone());
                is_err = true;
                resource_limit_recorded = true;
                if !quiet {
                    eprintln!(
                        "{}",
                        format!("  ⚠ {name}: resource limit detected in output — tool blocked").dim()
                    );
                }
            }

            let result_quality = if resource_limit_recorded {
                mo_agent_runtime::turn::result_quality::ResultQuality::Error
            } else {
                turn_guard.record_tool_result(&name, &result_str)
            };

            if let Some(feedback) = turn_guard.result_feedback(&name, result_quality) {
                result_str.push_str(&format!("\n{feedback}"));
            }

            let args_size = serde_json::to_string(&args)
                .map(|s| s.len() as u32)
                .unwrap_or(0);
            let result_size = result_str.len() as u32;
            let args_preview = make_args_preview(&name, &args);
            let tool_elapsed = tool_start.elapsed();
            tool_call_records.push(mo_agent_services::session_journal::ToolCallRecord {
                name: name.clone(),
                ok: !is_err,
                ms: tool_elapsed.as_millis() as u64,
                error: if is_err {
                    result_str
                        .lines()
                        .next()
                        .map(|l| l.chars().take(200).collect())
                } else {
                    None
                },
                input_bytes: Some(args_size),
                output_bytes: Some(result_size),
                args_preview,
            });
            step_recorder.complete_tool_with_result(
                &name,
                is_err,
                tool_elapsed.as_millis() as u64,
                false,
                &result_str,
            );

            if let Some(ref sid) = current_session_id
                && let Some(light) = step_recorder.build_light_checkpoint()
            {
                let cp = mo_agent_runtime::pipeline::step_protocol::StepCheckpoint::Light(light);
                let _ = mo_agent_runtime::pipeline::step_checkpoint::write_step_checkpoint(
                    sid,
                    step_recorder.summary().checkpoints,
                    &cp,
                );
            }

            if !is_err && CACHEABLE_TOOLS.contains(&name.as_str()) {
                let cached_result = CachedToolResult {
                    tool_name: name.clone(),
                    output: result_str.clone(),
                    is_error: false,
                    cached_at: mo_agent_runtime::pipeline::step_protocol::epoch_ms(),
                };
                step_recorder.attach_cached_result(cached_result.clone());
                idempotency_cache.record(&idem_key, cached_result);
                if let Some((prev_turn, reason)) =
                    semantic_dedup.check_and_record(&name, &args, &result_str, _turn)
                {
                    let hint = format!(
                        "\n⚠ Note: this result is similar to a previous {} call (turn {}, {}). \
                         Avoid re-fetching the same information.",
                        name,
                        prev_turn + 1,
                        reason
                    );
                    result_str.push_str(&hint);
                }
            }

            if !quiet {
                let duration_str = if tool_elapsed.as_secs_f64() >= 1.0 {
                    format!("{:.1}s", tool_elapsed.as_secs_f64())
                } else {
                    format!("{}ms", tool_elapsed.as_millis())
                };
                let detail = tool_call_detail(&name, &args);
                let summary = if !is_err {
                    tool_result_summary(&name, &result_str)
                } else {
                    None
                };
                if is_err {
                    eprintln!("{}", format!("  ✗ {name} ({duration_str})").red());
                    if let Some(first_line) = result_str.lines().next() {
                        let preview = if first_line.len() > 100 {
                            format!("{}…", &first_line[..100])
                        } else {
                            first_line.to_string()
                        };
                        eprintln!("  {}", format!("└ Error: {preview}").dim());
                    }
                } else {
                    eprintln!("{}", format!("  ✓ {name} ({duration_str})").green());
                    match (&detail, &summary) {
                        (Some(d), Some(s)) => {
                            eprintln!("  {}", format!("└ {d}  →  {s}").dim());
                        }
                        (Some(d), None) => {
                            eprintln!("  {}", format!("└ {d}").dim());
                        }
                        (None, Some(s)) => {
                            eprintln!("  {}", format!("└ {s}").dim());
                        }
                        (None, None) => {}
                    }
                }
            }

            let (tool_msg, tr) = openai_tool_roundtrip_values(&id, &name, &result_str);
            messages.push(tool_msg);
            tool_results.push(tr);
        }
        // ── Intent drift detection ──
        // Track per-turn tool names + args, detect when agent drifts from user's query
        {
            let turn_names: Vec<String> = tool_calls_for_guard
                .iter()
                .filter_map(|tc| tc.get("name").and_then(|v| v.as_str()).map(String::from))
                .collect();
            let turn_args_text: String = tool_calls_for_guard
                .iter()
                .filter_map(|tc| {
                    tc.get("arguments")
                        .map(|v| serde_json::to_string(v).unwrap_or_default())
                })
                .collect::<Vec<_>>()
                .join(" ");
            intent_tool_turns.push((turn_names, turn_args_text));

            if let mo_agent_runtime::turn::stall::IntentDrift::Drifting { correction, .. } =
                mo_agent_runtime::turn::stall::detect_intent_drift(message, &intent_tool_turns)
            {
                messages.push(openai_user_content_message(&correction));
                stall_events.push(("intent_drift".to_string(), _turn as u32));
            }
        }

        // ── TurnGuard: unified non-happy-path evaluation ──
        // Evaluate AFTER all tool results recorded, BEFORE next LLM call.
        {
            use mo_agent_runtime::turn::turn_guard::VerdictSeverity;

            let verdict = turn_guard.evaluate();

            // ── Audit: collect non-Healthy verdict events ──
            if verdict.severity > VerdictSeverity::Healthy {
                let severity_str = match verdict.severity {
                    VerdictSeverity::Critical => "critical",
                    VerdictSeverity::Warning => "warning",
                    VerdictSeverity::Info => "info",
                    VerdictSeverity::Healthy => unreachable!(),
                };
                let health_summary = turn_guard.health.summary();
                verdict_events.push(VerdictEvent {
                    turn: _turn as u32,
                    severity: severity_str.to_string(),
                    injections: verdict.injections.clone(),
                    avoid_tools: verdict.avoid_tools.clone(),
                    force_stop: verdict.force_stop,
                    nudge_count: turn_guard.nudge_count,
                    total_errors: turn_guard.errors.total_errors,
                    deprioritized_count: health_summary.deprioritized_count,
                    total_timeouts: health_summary.total_timeouts,
                    total_cache_hits: health_summary.total_cache_hits,
                    flaky_count: health_summary.flaky_count,
                });
            }

            // Inject all verdict messages (stall nudge, divergence correction,
            // tool health warnings, escalation messages, nudge-ignore warnings)
            append_openai_user_content_messages(&mut messages, &verdict.injections);

            // Restrict tools that TurnGuard says to avoid
            for tool in &verdict.avoid_tools {
                restricted_tools.insert(tool.clone());
            }

            // Apply turn budget penalties based on severity.
            match verdict.severity {
                VerdictSeverity::Critical => {
                    remaining_turns = remaining_turns.saturating_sub(5);
                }
                VerdictSeverity::Warning => {
                    remaining_turns = remaining_turns.saturating_sub(2);
                }
                _ => {}
            }

            // Step recorder: record verdict outcome
            let severity_label = match verdict.severity {
                VerdictSeverity::Critical => "critical",
                VerdictSeverity::Warning => "warning",
                VerdictSeverity::Info => "info",
                VerdictSeverity::Healthy => "healthy",
            };
            step_recorder.record_verdict(
                severity_label,
                verdict.stall_detected,
                verdict.is_diverging,
                verdict.force_stop,
                verdict.injections.len(),
            );

            // Heavy checkpoint after verdict (captures full conversation state)
            if let Some(ref sid) = current_session_id
                && let Some(heavy) = step_recorder.build_heavy_checkpoint(
                    &messages,
                    0, // budget tokens filled by caller if available
                    max_turns.saturating_sub(_turn) as u32,
                    &turn_guard
                        .health
                        .deprioritized_tools()
                        .iter()
                        .map(|s| s.to_string())
                        .collect::<Vec<_>>(),
                    recent_tools,
                )
            {
                let cp = mo_agent_runtime::pipeline::step_protocol::StepCheckpoint::Heavy(
                    Box::new(heavy),
                );
                let _ = mo_agent_runtime::pipeline::step_checkpoint::write_step_checkpoint(
                    sid,
                    step_recorder.summary().checkpoints,
                    &cp,
                );
                last_heavy_checkpoint = Some(cp);
            }

            // Force stop on critical verdict
            if verdict.force_stop {
                step_recorder.end_turn(true);
                return Err(
                    "Agent escalated to critical — too many errors and stalls. Aborting."
                        .to_string(),
                );
            }

            // If verdict injected stall messages, skip to next LLM call (don't re-process results)
            if !verdict.injections.is_empty() && verdict.severity >= VerdictSeverity::Warning {
                step_recorder.end_turn(false);
                tool_results = Vec::new();
                continue;
            }
        }
        step_recorder.end_turn(false);
    }

    if explain != ExplainMode::Off && !explain_turns.is_empty() && !quiet {
        print_explain_report(&explain_turns, explain == ExplainMode::Verbose);
    }
    if explain != ExplainMode::Off && !verdict_events.is_empty() && !quiet {
        print_verdict_report(&verdict_events, explain == ExplainMode::Verbose);
    }

    let elapsed = start.elapsed().as_secs_f64();
    let format_footer_tokens = |tokens: u64| -> String {
        if tokens < 1000 {
            format!("{}tok", tokens)
        } else {
            format!("{:.1}k", tokens as f64 / 1000.0)
        }
    };
    let model_tag = model.unwrap_or("auto");
    let session_tag = current_session_id
        .as_deref()
        .map(|s| if s.len() > 8 { &s[..8] } else { s })
        .unwrap_or("?");
    if verbose_mode && !quiet {
        eprintln!(
            "{}",
            format!(
                "  ⏱ {:.1}s  ↓ {}  ↑ {}  model: {}  session: {}",
                elapsed,
                if has_any_usage {
                    format_footer_tokens(total_completion)
                } else {
                    "?".to_string()
                },
                if has_any_usage {
                    format_footer_tokens(total_prompt)
                } else {
                    "?".to_string()
                },
                model_tag,
                session_tag,
            )
            .dim()
        );
    }

    let report = first_selection_report.unwrap_or_else(|| tool_registry::SelectionReport {
        tools_selected: Vec::new(),
        selected_count: 0,
        budget_used: 0,
        budget_total: 0,
    });

    // Deduplicate stall events by type (keep only one of each type per user turn).
    // The internal _turn numbers were used for in-loop deduplication; for journal
    // output, we normalize all turn numbers to 0 (repl_turn.rs will use state.turn).
    let deduped_stall_events: Vec<(String, u32)> = {
        let mut seen = std::collections::HashSet::new();
        stall_events
            .into_iter()
            .filter(|(stall_type, _)| seen.insert(stall_type.clone()))
            .map(|(stall_type, _)| (stall_type, 0)) // turn will be filled by repl_turn
            .collect()
    };

    // Deduplicate verdict events by severity (keep only the first of each severity).
    // Same rationale: internal turn numbers are loop-internal, not user turns.
    let deduped_verdict_events: Vec<VerdictEvent> = {
        let mut seen = std::collections::HashSet::new();
        verdict_events
            .into_iter()
            .filter(|ve| seen.insert(ve.severity.clone()))
            .map(|mut ve| {
                ve.turn = 0; // turn will be filled by repl_turn
                ve
            })
            .collect()
    };

    Ok(StreamResult {
        session_id: current_session_id,
        run_id: current_run_id,
        full_text: final_text,
        prompt_tokens: total_prompt,
        completion_tokens: total_completion,
        tool_calls_count: total_tool_calls,
        tools_selected: report.tools_selected,
        selected_skills: all_selected_skills,
        tools_used: all_tools_used.into_iter().collect(),
        tool_call_records,
        budget_used: report.budget_used,
        budget_pressure: first_budget_pressure,
        stall_events: deduped_stall_events,
        verdict_events: deduped_verdict_events,
        step_recorder_summary: Some(step_recorder.summary()),
        // Export tool health with merged historical entries to preserve unused tools
        tool_health_export: turn_guard.health.export_merged(tool_health_entries),
        last_heavy_checkpoint,
        ttft_ms: first_ttft_ms,
        context_ms: first_context_assembly_ms,
        selector_strategy: first_selector_strategy,
        selector_ms: first_selector_ms,
        selector_tokens_in,
        selector_tokens_out,
        memoria_ms: first_memoria_ms,
    })
}
