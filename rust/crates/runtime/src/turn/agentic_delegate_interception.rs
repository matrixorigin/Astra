use astra_services::session_journal::ToolCallRecord;
use serde_json::Value;

use crate::str_preview::truncate_str;
use crate::turn::agentic_headless_round::HeadlessStderrStyle;

use super::agentic_loop_host::{AgenticLoopHost, AgenticLoopState, HostTurnResult};

pub(crate) const DELEGATE_TOOL_NAME: &str = "delegate";

pub(crate) struct DelegationInterceptionResult {
    pub(crate) effective_tool_calls: Vec<Value>,
    pub(crate) intercepted_any: bool,
}

pub(crate) fn tool_call_name(tool_call: &Value) -> Option<&str> {
    super::tool_call_shape::tool_call_name(tool_call)
}

pub(crate) fn tool_call_arguments_value(tool_call: &Value) -> Value {
    super::tool_call_shape::tool_call_arguments_value(tool_call)
}

pub(crate) fn is_delegation_call(tool_call: &Value) -> bool {
    tool_call_name(tool_call) == Some(DELEGATE_TOOL_NAME)
}

pub(crate) async fn intercept_delegations<H: AgenticLoopHost>(
    host: &mut H,
    state: &mut AgenticLoopState,
    turn_result: &HostTurnResult,
    quiet: bool,
) -> DelegationInterceptionResult {
    let (delegation_results, remaining_tool_calls) = if let Some(engine) = &state.delegation_engine
    {
        let adaptive_delegation_context =
            state
                .telemetry
                .observability_session
                .as_ref()
                .map(|session| {
                    let session = session.read().unwrap_or_else(|e| e.into_inner());
                    delegation_adaptive_context(
                        &session,
                        state.telemetry.observability_hub.as_deref(),
                    )
                });
        if turn_result.accum.tool_calls.iter().any(is_delegation_call) {
            if !quiet {
                host.emit_headless_line(
                    HeadlessStderrStyle::Yellow,
                    "🤝 Delegating to sub-agents — parent agent is paused until results are aggregated."
                        .to_string(),
                );
            }
            let _ = crate::skills::hooks::evaluate_session_hooks(
                &state.skills.session_event_hooks,
                crate::skills::hooks::SessionEvent::SubagentStart,
                state.current_session_id.as_deref().unwrap_or(""),
                None,
            )
            .await;
        }
        partition_and_execute_delegations(
            &turn_result.accum.tool_calls,
            engine,
            state.current_run_id.as_deref().unwrap_or("unknown"),
            state.current_session_id.as_deref().unwrap_or("unknown"),
            state.recursion_depth,
            "orchestrator",
            state.hooks.workspace_root_hint.as_deref(),
            &state.skills.search,
            adaptive_delegation_context.as_ref(),
        )
        .await
    } else {
        (Vec::new(), turn_result.accum.tool_calls.clone())
    };

    if let Some(hub) = &state.telemetry.observability_hub {
        for result in &delegation_results {
            if let Some(ref outcome) = result.outcome {
                let scenario_key = outcome.scenario.as_deref().unwrap_or("unknown");
                hub.record_delegation_outcome(scenario_key, &outcome.pattern, outcome.succeeded);
            }
        }
    }

    if !delegation_results.is_empty() {
        if !quiet {
            for result in &delegation_results {
                for (style, line) in &result.preview_lines {
                    host.emit_headless_line(*style, line.clone());
                }
            }
        }
        let delegate_tool_calls: Vec<&Value> = turn_result
            .accum
            .tool_calls
            .iter()
            .filter(|tc| is_delegation_call(tc))
            .collect();
        if !delegate_tool_calls.is_empty() {
            let tc_entries: Vec<Value> = delegate_tool_calls
                .iter()
                .map(|tc| {
                    let id = tc
                        .get("id")
                        .and_then(Value::as_str)
                        .filter(|s| !s.is_empty())
                        .map(|s| s.to_string())
                        .unwrap_or_else(|| uuid::Uuid::now_v7().to_string());
                    let name = tool_call_name(tc).unwrap_or(DELEGATE_TOOL_NAME);
                    let args = tool_call_arguments_value(tc);
                    serde_json::json!({
                        "id": id,
                        "type": "function",
                        "function": {
                            "name": name,
                            "arguments": serde_json::to_string(&args)
                                .unwrap_or_else(|_| "{}".to_string()),
                        }
                    })
                })
                .collect();
            let mut assistant_msg = serde_json::json!({
                "role": "assistant",
                "content": Value::Null,
                "tool_calls": tc_entries,
            });
            let rc = &turn_result.accum.reasoning_content;
            if !rc.is_empty() {
                assistant_msg["reasoning_content"] = Value::String(rc.clone());
            } else if super::edge_ledger::history_has_reasoning(&state.messages) {
                assistant_msg["reasoning_content"] = Value::String(String::new());
            }
            state.messages.push(assistant_msg);
        }
        for result in &delegation_results {
            let summary_for_model =
                crate::turn::tool_result_sanitize::tool_result_content_for_model(
                    DELEGATE_TOOL_NAME,
                    &result.summary,
                );
            let tool_msg = serde_json::json!({
                "role": "tool",
                "tool_call_id": result.call_id,
                "content": summary_for_model,
            });
            state.messages.push(tool_msg.clone());
            state.tool_results.push(tool_msg);
            state.stall.tool_call_records.push(ToolCallRecord {
                name: DELEGATE_TOOL_NAME.to_string(),
                ok: !result.summary.starts_with("Delegation failed:")
                    && !result.summary.starts_with("Invalid delegation request:"),
                ms: 0,
                error: None,
                input_bytes: None,
                output_bytes: Some(result.summary.len() as u32),
                args_preview: Some(result.call_id.clone()),
                result_preview: Some(result.summary.chars().take(500).collect::<String>()),
            });
        }
        if !quiet {
            host.emit_headless_line(
                HeadlessStderrStyle::Dim,
                "🧠 Parent agent is incorporating delegated results into the final response…"
                    .to_string(),
            );
        }
    }

    if !delegation_results.is_empty()
        && state.hooks.teammate_idle_hook_runs == 0
        && let Some(prompt) = crate::turn::stop_hooks::build_teammate_idle_hook_prompt(
            &state.hooks.teammate_idle_hooks,
        )
    {
        state.hooks.teammate_idle_hook_runs = 1;
        if !quiet {
            host.emit_headless_line(
                HeadlessStderrStyle::Yellow,
                "⚠ Teammate-round verification…".to_string(),
            );
        }
        state.messages.push(prompt);
    }

    DelegationInterceptionResult {
        effective_tool_calls: if delegation_results.is_empty() {
            turn_result.accum.tool_calls.clone()
        } else {
            remaining_tool_calls
        },
        intercepted_any: !delegation_results.is_empty(),
    }
}

pub(crate) fn parse_delegation_request(
    tool_call: &Value,
    parent_run_id: &str,
    session_id: &str,
    recursion_depth: u8,
    skill_search: &astra_core::SkillSearchSettings,
    adaptive_context: Option<&DelegationAdaptiveContext>,
) -> Result<astra_services::coordination::DelegationRequest, String> {
    let args_str = tool_call
        .get("function")
        .and_then(|f| f.get("arguments"))
        .and_then(Value::as_str)
        .ok_or("delegate call missing arguments")?;

    let args: Value =
        serde_json::from_str(args_str).map_err(|e| format!("invalid delegation JSON: {e}"))?;

    let task = args
        .get("task")
        .and_then(Value::as_str)
        .unwrap_or("delegated task")
        .to_string();

    let explicit_pattern = args.get("pattern").and_then(Value::as_str);
    let (pattern, adaptive_policy) = if explicit_pattern.is_some() {
        (parse_coordination_pattern(&args)?, None)
    } else {
        let (pattern, policy) =
            select_default_coordination_pattern(&args, &task, adaptive_context)?;
        (pattern, Some(policy))
    };

    let mut context = std::collections::HashMap::new();
    context.insert(
        "session_id".to_string(),
        Value::String(session_id.to_string()),
    );
    context.insert(
        "skill_search".to_string(),
        serde_json::to_value(skill_search)
            .map_err(|e| format!("failed to encode skill_search config: {e}"))?,
    );
    if let Some(ctx) = args.get("context").and_then(Value::as_object) {
        for (k, v) in ctx {
            context.insert(k.clone(), v.clone());
        }
    }
    if let Some(policy) = adaptive_policy {
        context.insert("adaptive_coordination".to_string(), policy);
    }
    crate::turn::agentic_recursion_guard::checked_child_recursion_depth(recursion_depth)?;

    Ok(astra_services::coordination::DelegationRequest {
        delegation_id: uuid::Uuid::new_v4().to_string(),
        parent_run_id: parent_run_id.to_string(),
        task,
        pattern,
        user_id: "system".to_string(),
        depth: u32::from(recursion_depth),
        context,
    })
}

#[derive(Debug, Clone)]
pub(crate) struct DelegationAdaptiveContext {
    pub(crate) scenario: Option<crate::user_profile::Scenario>,
    pub(crate) preferred_pattern: Option<String>,
}

pub(crate) fn delegation_adaptive_context(
    session: &crate::observability_integration::ObservabilitySession,
    hub: Option<&crate::observability_integration::ObservabilityHub>,
) -> DelegationAdaptiveContext {
    let scenario = session.current_scenario();
    let preferred_pattern = scenario.as_ref().and_then(|s| {
        let scenario_key = serde_json::to_value(s)
            .ok()
            .and_then(|v| v.as_str().map(String::from))?;
        hub?.preferred_delegation_pattern(&scenario_key, 3)
    });
    DelegationAdaptiveContext {
        scenario,
        preferred_pattern,
    }
}

pub(crate) fn select_default_coordination_pattern(
    args: &Value,
    task: &str,
    adaptive_context: Option<&DelegationAdaptiveContext>,
) -> Result<(astra_services::coordination::CoordinationPattern, Value), String> {
    let agents = parse_delegate_agents(args);
    let scenario = adaptive_context.and_then(|ctx| ctx.scenario);
    let preferred = adaptive_context.and_then(|ctx| ctx.preferred_pattern.as_deref());
    let task_requests_review = task_needs_review(task);

    if let Some(pref) = preferred {
        if let Some(pattern) = pattern_from_name(pref, &agents, args) {
            return Ok((
                pattern,
                serde_json::json!({
                    "selected_pattern": pref,
                    "selection_source": "outcome_history",
                    "reason": "historically preferred pattern for this scenario",
                    "scenario": scenario,
                }),
            ));
        }
    }

    let should_adapt = matches!(
        scenario,
        Some(
            crate::user_profile::Scenario::CodeReview
                | crate::user_profile::Scenario::Exploration
                | crate::user_profile::Scenario::Debugging
                | crate::user_profile::Scenario::Testing
        )
    ) || task_requests_review;

    if !should_adapt {
        let pattern = astra_services::coordination::CoordinationPattern::Sequential {
            agent_ids: agents.clone(),
            stop_on_success: false,
            timeout_sec: 0,
        };
        return Ok((
            pattern,
            serde_json::json!({
                "selected_pattern": "sequential",
                "selection_source": "legacy_default",
                "reason": "no explicit pattern and no adaptive delegation signal",
                "scenario": scenario,
            }),
        ));
    }

    let hints = astra_services::coordination::CoordinationHints {
        agent_ids: agents,
        task: task.to_string(),
        needs_review: matches!(scenario, Some(crate::user_profile::Scenario::CodeReview)),
        has_dependencies: !matches!(
            scenario,
            Some(
                crate::user_profile::Scenario::Exploration
                    | crate::user_profile::Scenario::CodeReview
                    | crate::user_profile::Scenario::Testing
            )
        ),
        timeout_sec: args.get("timeout").and_then(Value::as_u64).unwrap_or(0),
    };
    let pattern = astra_services::coordination::suggest_pattern(&hints);
    let selected_pattern = coordination_pattern_name(&pattern);
    let reason = if matches!(scenario, Some(crate::user_profile::Scenario::CodeReview)) {
        "code_review_scenario_prefers_review_loop"
    } else if matches!(scenario, Some(crate::user_profile::Scenario::Exploration)) {
        "exploration_scenario_prefers_parallel_scouting"
    } else if matches!(scenario, Some(crate::user_profile::Scenario::Debugging)) {
        "debugging_scenario_prefers_sequential_with_stop"
    } else if matches!(scenario, Some(crate::user_profile::Scenario::Testing)) {
        "testing_scenario_prefers_parallel_execution"
    } else if task_requests_review {
        "task_keywords_request_review"
    } else {
        "adaptive_default"
    };

    Ok((
        pattern,
        serde_json::json!({
            "selected_pattern": selected_pattern,
            "selection_source": "adaptive_default",
            "reason": reason,
            "scenario": scenario,
        }),
    ))
}

pub(crate) fn pattern_from_name(
    name: &str,
    agents: &[String],
    args: &Value,
) -> Option<astra_services::coordination::CoordinationPattern> {
    let timeout = args.get("timeout").and_then(Value::as_u64).unwrap_or(0);
    match name {
        "fan_out" => Some(astra_services::coordination::CoordinationPattern::FanOut {
            agent_ids: agents.to_vec(),
            aggregation: astra_services::coordination::AggregationStrategy::AllResults,
            timeout_sec: timeout,
        }),
        "sequential" => Some(
            astra_services::coordination::CoordinationPattern::Sequential {
                agent_ids: agents.to_vec(),
                stop_on_success: false,
                timeout_sec: timeout,
            },
        ),
        "pipeline" => Some(
            astra_services::coordination::CoordinationPattern::Pipeline {
                stages: agents
                    .iter()
                    .cloned()
                    .map(|agent_id| astra_services::coordination::PipelineStage {
                        agent_id,
                        output_transform: None,
                    })
                    .collect(),
                timeout_sec: timeout,
            },
        ),
        "adversarial" if agents.len() >= 2 => Some(
            astra_services::coordination::CoordinationPattern::AdversarialReview {
                producer_id: agents[0].clone(),
                reviewer_id: agents[1].clone(),
                max_rounds: 3,
                acceptance_threshold: 0.7,
                timeout_sec: timeout,
            },
        ),
        "fork" => {
            agents.first().map(
                |agent| astra_services::coordination::CoordinationPattern::Fork {
                    agent_id: agent.clone(),
                    tasks: vec!["delegated task".to_string()],
                    max_turns: 10,
                    aggregation: astra_services::coordination::AggregationStrategy::AllResults,
                    timeout_sec: timeout,
                },
            )
        }
        _ => None,
    }
}

pub(crate) fn parse_delegate_agents(args: &Value) -> Vec<String> {
    args.get("agents")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(Value::as_str)
                .map(String::from)
                .collect()
        })
        .unwrap_or_else(|| vec!["coder".to_string()])
}

pub(crate) fn task_needs_review(task: &str) -> bool {
    let review_keywords = ["review", "审查", "check", "verify", "验证", "critique"];
    let task_lower = task.to_lowercase();
    review_keywords
        .iter()
        .any(|keyword| task_lower.contains(keyword))
}

pub(crate) fn coordination_pattern_name(
    pattern: &astra_services::coordination::CoordinationPattern,
) -> &'static str {
    match pattern {
        astra_services::coordination::CoordinationPattern::FanOut { .. } => "fan_out",
        astra_services::coordination::CoordinationPattern::Pipeline { .. } => "pipeline",
        astra_services::coordination::CoordinationPattern::AdversarialReview { .. } => {
            "adversarial"
        }
        astra_services::coordination::CoordinationPattern::Sequential { .. } => "sequential",
        astra_services::coordination::CoordinationPattern::Fork { .. } => "fork",
    }
}

pub(crate) fn parse_coordination_pattern(
    args: &Value,
) -> Result<astra_services::coordination::CoordinationPattern, String> {
    let pattern_type = args
        .get("pattern")
        .and_then(Value::as_str)
        .unwrap_or("sequential");

    let agents = parse_delegate_agents(args);
    let task = args
        .get("task")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();

    match pattern_type {
        "fan_out" => Ok(astra_services::coordination::CoordinationPattern::FanOut {
            agent_ids: agents,
            aggregation: astra_services::coordination::AggregationStrategy::AllResults,
            timeout_sec: 300,
        }),
        "pipeline" => {
            let stages = agents
                .into_iter()
                .map(|id| astra_services::coordination::PipelineStage {
                    agent_id: id,
                    output_transform: None,
                })
                .collect();
            Ok(
                astra_services::coordination::CoordinationPattern::Pipeline {
                    stages,
                    timeout_sec: 0,
                },
            )
        }
        "adversarial" => {
            let producer = agents
                .first()
                .cloned()
                .unwrap_or_else(|| "coder".to_string());
            let reviewer = agents
                .get(1)
                .cloned()
                .unwrap_or_else(|| "reviewer".to_string());
            let max_rounds = args.get("max_rounds").and_then(Value::as_u64).unwrap_or(2) as u32;
            Ok(
                astra_services::coordination::CoordinationPattern::AdversarialReview {
                    producer_id: producer,
                    reviewer_id: reviewer,
                    max_rounds,
                    acceptance_threshold: 0.8,
                    timeout_sec: 0,
                },
            )
        }
        "auto" => {
            let hints = astra_services::coordination::CoordinationHints {
                agent_ids: agents,
                task,
                needs_review: false,
                has_dependencies: args
                    .get("has_dependencies")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
                timeout_sec: args.get("timeout").and_then(Value::as_u64).unwrap_or(0),
            };
            Ok(astra_services::coordination::suggest_pattern(&hints))
        }
        _ => Ok(
            astra_services::coordination::CoordinationPattern::Sequential {
                agent_ids: agents,
                stop_on_success: false,
                timeout_sec: 0,
            },
        ),
    }
}

pub(crate) fn merge_workspace_hint_into_delegation_request(
    request: &mut astra_services::coordination::DelegationRequest,
    hint: Option<&str>,
) {
    let Some(root) = hint.map(str::trim).filter(|s| !s.is_empty()) else {
        return;
    };
    let c = &mut request.context;
    if c.contains_key("git_root") || c.contains_key("workspace_root") || c.contains_key("cwd") {
        return;
    }
    c.insert("cwd".to_string(), Value::String(root.to_string()));
}

pub(crate) async fn partition_and_execute_delegations(
    tool_calls: &[Value],
    engine: &crate::server::delegation_engine::DelegationEngine,
    parent_run_id: &str,
    session_id: &str,
    recursion_depth: u8,
    source_agent_id: &str,
    workspace_hint: Option<&str>,
    skill_search: &astra_core::SkillSearchSettings,
    adaptive_context: Option<&DelegationAdaptiveContext>,
) -> (Vec<DelegationExecutionResult>, Vec<Value>) {
    let mut delegation_results = Vec::new();
    let mut remaining = Vec::new();

    for tc in tool_calls {
        if is_delegation_call(tc) {
            let call_id = tc
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or("unknown")
                .to_string();

            match parse_delegation_request(
                tc,
                parent_run_id,
                session_id,
                recursion_depth,
                skill_search,
                adaptive_context,
            ) {
                Ok(mut request) => {
                    merge_workspace_hint_into_delegation_request(&mut request, workspace_hint);
                    let pattern_name = coordination_pattern_name(&request.pattern).to_string();
                    let scenario_name =
                        adaptive_context
                            .and_then(|ctx| ctx.scenario.as_ref())
                            .map(|s| {
                                serde_json::to_value(s)
                                    .ok()
                                    .and_then(|v| v.as_str().map(String::from))
                                    .unwrap_or_else(|| format!("{s:?}").to_lowercase())
                            });
                    match engine.execute(request, source_agent_id, None).await {
                        Ok(result) => {
                            let succeeded =
                                result.status == "completed" || result.status == "success";
                            delegation_results.push(DelegationExecutionResult {
                                call_id,
                                summary: format_delegation_result(&result),
                                preview_lines: format_delegation_terminal_preview(&result),
                                outcome: Some(DelegationOutcomeMetadata {
                                    scenario: scenario_name,
                                    pattern: pattern_name,
                                    succeeded,
                                }),
                            });
                        }
                        Err(e) => {
                            delegation_results.push(DelegationExecutionResult {
                                call_id,
                                summary: format!("Delegation failed: {e}"),
                                preview_lines: vec![(
                                    HeadlessStderrStyle::Yellow,
                                    format!("🤝 Delegation failed — {e}"),
                                )],
                                outcome: Some(DelegationOutcomeMetadata {
                                    scenario: scenario_name,
                                    pattern: pattern_name,
                                    succeeded: false,
                                }),
                            });
                        }
                    }
                }
                Err(e) => {
                    delegation_results.push(DelegationExecutionResult {
                        call_id,
                        summary: format!("Invalid delegation request: {e}"),
                        preview_lines: vec![(
                            HeadlessStderrStyle::Yellow,
                            format!("🤝 Invalid delegation request — {e}"),
                        )],
                        outcome: None,
                    });
                }
            }
        } else {
            remaining.push(tc.clone());
        }
    }

    (delegation_results, remaining)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DelegationExecutionResult {
    pub(crate) call_id: String,
    pub(crate) summary: String,
    pub(crate) preview_lines: Vec<(HeadlessStderrStyle, String)>,
    pub(crate) outcome: Option<DelegationOutcomeMetadata>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DelegationOutcomeMetadata {
    pub(crate) scenario: Option<String>,
    pub(crate) pattern: String,
    pub(crate) succeeded: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DelegationFinalOutputSource {
    Aggregated,
    SingleSuccessfulSubRun,
}

pub(crate) fn delegation_final_output_preview(
    result: &astra_services::coordination::DelegationResult,
    limit: usize,
) -> Option<(String, DelegationFinalOutputSource)> {
    if let Some(agg) = &result.aggregated_output {
        return Some((
            truncate_str(agg, limit),
            DelegationFinalOutputSource::Aggregated,
        ));
    }
    let successful_outputs: Vec<_> = result
        .agent_results
        .iter()
        .filter(|ar| ar.is_success())
        .filter_map(|ar| ar.output.as_deref())
        .collect();
    if successful_outputs.len() == 1 {
        Some((
            truncate_str(successful_outputs[0], limit.min(500)),
            DelegationFinalOutputSource::SingleSuccessfulSubRun,
        ))
    } else {
        None
    }
}

pub(crate) fn format_delegation_result(
    result: &astra_services::coordination::DelegationResult,
) -> String {
    let succeeded = result
        .agent_results
        .iter()
        .filter(|ar| ar.is_success())
        .count();
    let failed = result.agent_results.len().saturating_sub(succeeded);
    let mut parts = Vec::new();
    parts.push(format!(
        "Delegation {} — status: {} ({} ok / {} failed)",
        result.delegation_id, result.status, succeeded, failed
    ));

    let final_output = delegation_final_output_preview(result, 1_500);
    if let Some((final_output, _)) = &final_output {
        parts.push(format!("\n📋 Final aggregated result:\n{final_output}"));
    }

    parts.push("\nSub-agent results:".to_string());
    let single_successful_sub_run_fallback = matches!(
        final_output.as_ref().map(|(_, source)| *source),
        Some(DelegationFinalOutputSource::SingleSuccessfulSubRun)
    );
    for ar in &result.agent_results {
        let status_icon = if ar.is_success() { "✅" } else { "❌" };
        let output_preview =
            if single_successful_sub_run_fallback && ar.is_success() && ar.output.is_some() {
                "[same as final result above]".to_string()
            } else {
                ar.output
                    .as_deref()
                    .map(|o| truncate_str(o, 500))
                    .unwrap_or_else(|| "[no output]".to_string())
            };
        parts.push(format!(
            "\n{status_icon} Agent '{}' ({}): {output_preview}",
            ar.agent_id, ar.status
        ));
        if let Some(err) = &ar.error {
            parts.push(format!("   Error: {err}"));
        }
    }

    parts.push(format!(
        "\nTokens: {} prompt + {} completion, {} tool calls",
        result.total_prompt_tokens, result.total_completion_tokens, result.total_tool_calls
    ));

    parts.join("\n")
}

pub(crate) fn format_delegation_terminal_preview(
    result: &astra_services::coordination::DelegationResult,
) -> Vec<(HeadlessStderrStyle, String)> {
    let succeeded = result
        .agent_results
        .iter()
        .filter(|ar| ar.is_success())
        .count();
    let failed = result.agent_results.len().saturating_sub(succeeded);
    let status_style = if failed == 0 {
        HeadlessStderrStyle::Green
    } else {
        HeadlessStderrStyle::Yellow
    };
    let mut lines = vec![(
        status_style,
        format!(
            "🤝 Delegation {} completed [{} ok / {} failed]",
            result.delegation_id, succeeded, failed
        ),
    )];
    if let Some((final_output, _)) = delegation_final_output_preview(result, 200) {
        let preview = final_output.lines().next().unwrap_or(final_output.as_str());
        lines.push((
            HeadlessStderrStyle::Dim,
            format!("   {}", truncate_str(preview, 200)),
        ));
    }
    lines
}
