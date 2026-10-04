use std::collections::HashSet;

use astra_turn_core::headless_tool_assembly::HeadlessPreResolvedToolResult;
use serde_json::Value;

use super::headless_round::HeadlessStderrStyle;
use astra_text_utils::str_preview::truncate_str;

use super::super::agentic_loop::host::{
    AgenticLoopHost, AgenticLoopState, HostTurnResult, RequestConstraints,
};

pub(crate) const DELEGATE_TOOL_NAME: &str = "delegate";
pub(crate) const FORWARD_HEADERS_CONTEXT_KEY: &str = "__astra_forward_headers";
pub(crate) const REQUEST_ALLOWED_TOOLS_CONTEXT_KEY: &str = "__astra_request_allowed_tools";
pub(crate) const REQUEST_ENABLED_TOOLS_CONTEXT_KEY: &str = "__astra_request_enabled_tools";
pub(crate) const REQUEST_ALLOWED_SKILLS_CONTEXT_KEY: &str = "__astra_request_allowed_skills";
pub(crate) const REQUEST_ALLOWED_SKILL_SOURCES_CONTEXT_KEY: &str =
    "__astra_request_allowed_skill_sources";

pub(crate) struct DelegationInterceptionResult {
    pub(crate) effective_tool_calls: Vec<Value>,
    pub(crate) pre_resolved_results: Vec<HeadlessPreResolvedToolResult>,
    pub(crate) intercepted_any: bool,
}

pub(crate) fn tool_call_name(tool_call: &Value) -> Option<&str> {
    tool_call
        .get("function")
        .and_then(|function| function.get("name"))
        .and_then(Value::as_str)
        .and_then(astra_core::canonical_names::normalize_name)
}

pub(crate) fn tool_call_arguments_value(tool_call: &Value) -> Value {
    astra_turn_core::tool_call_shape::tool_call_arguments_value(tool_call)
}

pub(crate) fn is_delegation_call(tool_call: &Value) -> bool {
    tool_call_name(tool_call) == Some(DELEGATE_TOOL_NAME)
}

async fn execute_delegation(
    engine: &crate::server::delegation::engine::DelegationEngine,
    request: astra_services::coordination::DelegationRequest,
    source_agent_id: &str,
    forward_headers: &std::collections::HashMap<String, String>,
    admitted_model_execution: Option<&astra_services::AdmittedModelExecution>,
    parent_model_reasoning: Option<astra_turn_core::orchestration_spawn_tool::ParentModelReasoning>,
    live_event_sink: Option<astra_turn_core::agent_live_event::SharedAgentLiveEventSink>,
) -> Result<astra_services::coordination::DelegationResult, String> {
    engine
        .execute_with_forward_headers_and_live_events(
            request,
            source_agent_id,
            None,
            forward_headers.clone(),
            admitted_model_execution.cloned(),
            parent_model_reasoning,
            live_event_sink,
        )
        .await
}

pub(crate) async fn intercept_delegations<H: AgenticLoopHost>(
    host: &mut H,
    state: &mut AgenticLoopState,
    tool_calls: &[Value],
    quiet: bool,
    valid_tool_names: &HashSet<String>,
) -> DelegationInterceptionResult {
    if tool_calls.iter().any(is_delegation_call) && !valid_tool_names.contains(DELEGATE_TOOL_NAME) {
        return DelegationInterceptionResult {
            effective_tool_calls: tool_calls.to_vec(),
            pre_resolved_results: Vec::new(),
            intercepted_any: false,
        };
    }

    // Per-turn delegation limit: prevent runaway delegation loops where the
    // parent agent keeps delegating without synthesizing results.
    const MAX_DELEGATIONS_PER_TURN: u32 = 3;
    if tool_calls.iter().any(is_delegation_call)
        && state.delegations_this_turn >= MAX_DELEGATIONS_PER_TURN
    {
        if !quiet {
            host.emit_headless_line(
                HeadlessStderrStyle::Yellow,
                format!(
                    "⚠️  Delegation limit reached ({MAX_DELEGATIONS_PER_TURN} per turn). Synthesize existing results instead of delegating again."
                ),
            );
        }
        // Keep every provider call in its original round. The headless round
        // owns the one assistant carrier and one terminal tool result for each
        // call ID; removing calls here made the later paired-round join put
        // them back into the executable queue and also duplicated history.
        // Provider identities were validated before this interceptor, so a
        // refusal may use the original ID without minting a synthetic one.
        let pre_resolved_results = tool_calls
            .iter()
            .filter(|tool_call| is_delegation_call(tool_call))
            .filter_map(|tool_call| {
                tool_call
                    .get("id")
                    .and_then(Value::as_str)
                    .filter(|id| !id.is_empty())
                    .map(|id| {
                        HeadlessPreResolvedToolResult::new(
                            id.to_string(),
                            DELEGATE_TOOL_NAME,
                            format!(
                                "ERROR: Delegation limit reached ({} delegations already executed this turn). \
                                 You must synthesize the results from previous delegations and respond to the user directly. \
                                 Do NOT delegate again.",
                                state.delegations_this_turn
                            ),
                            astra_turn_core::tool_result_semantics::ToolResultStatus::Failed,
                        )
                    })
            })
            .collect();
        return DelegationInterceptionResult {
            effective_tool_calls: tool_calls.to_vec(),
            pre_resolved_results,
            intercepted_any: true,
        };
    }

    let (delegation_results, _remaining_tool_calls) = if let Some(engine) = &state.delegation_engine
    {
        let adaptive_delegation_context =
            state
                .telemetry
                .observability_session
                .as_ref()
                .map(|session| {
                    let session = astra_core::sync_poison::recover_rwlock_read(session);
                    delegation_adaptive_context(&session)
                });
        if tool_calls.iter().any(is_delegation_call) {
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
        let parent_model_reasoning = host.parent_model_reasoning_snapshot(state);
        partition_and_execute_delegations(
            tool_calls,
            engine,
            state.current_run_id.as_deref().unwrap_or("unknown"),
            state.current_session_id.as_deref().unwrap_or("unknown"),
            state.recursion_depth,
            &state.self_agent_id,
            state.hooks.workspace_root_hint.as_deref(),
            &state.hooks.forward_headers,
            state.hooks.admitted_model_execution.as_ref(),
            parent_model_reasoning.as_ref(),
            &state.skills.request_constraints,
            adaptive_delegation_context.as_ref(),
            &state.delegation_chain,
            host.agent_live_event_sink(),
            state
                .turn_intent
                .as_ref()
                .map(|intent| intent.workspace_mutation)
                .unwrap_or_default(),
        )
        .await
    } else {
        (Vec::new(), tool_calls.to_vec())
    };

    if !delegation_results.is_empty() {
        state.delegations_this_turn += delegation_results.len() as u32;
        if !quiet {
            for result in &delegation_results {
                for (style, line) in &result.preview_lines {
                    host.emit_headless_line(*style, line.clone());
                }
            }
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
        && let Some(prompt) = astra_turn_core::stop_hooks::build_teammate_idle_hook_prompt(
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
        if let Some(content) = prompt.get("content").and_then(Value::as_str) {
            state.push_volatile(
                crate::turn::agentic_loop::host::VolatileKind::StopHookEvidence,
                content.to_string(),
            );
        }
    }

    let pre_resolved_results = delegation_results
        .iter()
        .map(|result| {
            HeadlessPreResolvedToolResult::new(
                result.call_id.clone(),
                DELEGATE_TOOL_NAME,
                result.summary.clone(),
                if result
                    .outcome
                    .as_ref()
                    .is_some_and(|outcome| outcome.succeeded)
                {
                    astra_turn_core::tool_result_semantics::ToolResultStatus::Completed
                } else {
                    astra_turn_core::tool_result_semantics::ToolResultStatus::Failed
                },
            )
        })
        .collect();
    DelegationInterceptionResult {
        // Intercepted calls remain in the canonical round and close through
        // the same pre-resolved terminal lane as every other runtime-owned
        // control result. This preserves one assistant carrier and one tool
        // terminal per provider call without dispatching delegation twice.
        effective_tool_calls: tool_calls.to_vec(),
        pre_resolved_results,
        intercepted_any: !delegation_results.is_empty(),
    }
}

pub(crate) fn parse_delegation_request(
    tool_call: &Value,
    parent_run_id: &str,
    session_id: &str,
    recursion_depth: u8,
    adaptive_context: Option<&DelegationAdaptiveContext>,
) -> Result<astra_services::coordination::DelegationRequest, String> {
    let args_str = tool_call
        .get("function")
        .and_then(|f| f.get("arguments"))
        .and_then(Value::as_str)
        .ok_or("delegate call missing arguments")?;

    let args: Value =
        serde_json::from_str(args_str).map_err(|e| format!("invalid delegation JSON: {e}"))?;

    for field in ["max_turns", "needs_review", "max_rounds"] {
        if args.get(field).is_some() {
            return Err(format!("delegate field '{field}' is not supported"));
        }
    }

    let task = args
        .get("task")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|task| !task.is_empty())
        .ok_or("delegate call requires a non-empty string field 'task'")?
        .to_string();

    let (pattern, adaptive_policy) = if args.get("pattern").is_some() {
        (parse_coordination_pattern(&args)?, None)
    } else {
        let (pattern, policy) = select_default_coordination_pattern(&args, adaptive_context)?;
        (pattern, Some(policy))
    };

    let mut context = std::collections::HashMap::new();
    if let Some(ctx) = args.get("context") {
        let ctx = ctx
            .as_object()
            .ok_or("delegate field 'context' must be an object")?;
        for (k, v) in ctx {
            context.insert(k.clone(), v.clone());
        }
    }
    // Session lineage is runtime identity, never model-provided task context.
    // Remove this reserved key entirely rather than allowing it to travel to a
    // child prompt as ambiguous metadata.
    context.remove("session_id");
    // Delegated model requirements are runtime-owned admission state. A model
    // may request delegation, but it must not manufacture the typed handoff
    // that the authenticated parent turn received from its caller.
    context.remove(astra_turn_types::DELEGATED_MODEL_REQUIREMENTS_CONTEXT_KEY);
    if let Some(policy) = adaptive_policy {
        context.insert("adaptive_coordination".to_string(), policy);
    }
    merge_forward_headers_into_delegation_context(&mut context);
    astra_turn_core::agentic_recursion_guard::checked_child_recursion_depth(recursion_depth)?;

    Ok(astra_services::coordination::DelegationRequest {
        delegation_id: uuid::Uuid::new_v4().to_string(),
        session_id: session_id.to_string(),
        parent_run_id: parent_run_id.to_string(),
        task,
        pattern,
        user_id: "system".to_string(),
        depth: u32::from(recursion_depth),
        delegation_chain: Vec::new(),
        context,
        execution_metadata: None,
    })
}

pub(crate) fn merge_forward_headers_into_delegation_context(
    context: &mut std::collections::HashMap<String, serde_json::Value>,
) {
    // Forwarded headers now travel only through trusted sideband state.
    // Always clear the reserved context key so user/model supplied values
    // cannot leak into delegated prompts or sub-run configuration.
    context.remove(FORWARD_HEADERS_CONTEXT_KEY);
}

#[derive(Debug, Clone)]
pub(crate) struct DelegationAdaptiveContext {
    pub(crate) scenario: Option<astra_config::user_profile::Scenario>,
}

pub(crate) fn delegation_adaptive_context(
    session: &crate::observability::ObservabilitySession,
) -> DelegationAdaptiveContext {
    DelegationAdaptiveContext {
        scenario: session.current_scenario(),
    }
}

pub(crate) fn select_default_coordination_pattern(
    args: &Value,
    adaptive_context: Option<&DelegationAdaptiveContext>,
) -> Result<(astra_services::coordination::CoordinationPattern, Value), String> {
    let agents = parse_delegate_agents(args)?;
    let scenario = adaptive_context.and_then(|ctx| ctx.scenario);
    let explicit_has_dependencies = optional_bool_arg(args, "has_dependencies")?.unwrap_or(false);
    let timeout = optional_u64_arg(args, "timeout")?.unwrap_or(0);

    let hints = astra_services::coordination::CoordinationHints {
        agent_ids: agents,
        has_dependencies: explicit_has_dependencies,
        timeout_sec: timeout,
    };
    let pattern = astra_services::coordination::suggest_pattern(&hints);
    let selected_pattern = coordination_pattern_name(&pattern);
    let reason = if explicit_has_dependencies && selected_pattern == "sequential" {
        "typed_request_has_dependencies"
    } else {
        "available_agents_default"
    };

    Ok((
        pattern,
        serde_json::json!({
            "selected_pattern": selected_pattern,
            "selection_source": "typed_hints",
            "reason": reason,
            "scenario": scenario,
        }),
    ))
}

pub(crate) fn parse_delegate_agents(args: &Value) -> Result<Vec<String>, String> {
    let agents = args
        .get("agents")
        .and_then(Value::as_array)
        .ok_or("delegate call requires a non-empty string array field 'agents'")?;
    if agents.is_empty() {
        return Err("delegate call requires at least one agent".to_string());
    }
    let mut seen = HashSet::with_capacity(agents.len());
    let mut parsed = Vec::with_capacity(agents.len());
    for (index, value) in agents.iter().enumerate() {
        let agent = value
            .as_str()
            .map(str::trim)
            .filter(|agent| !agent.is_empty())
            .ok_or_else(|| format!("delegate agents[{index}] must be a non-empty string"))?;
        if !seen.insert(agent.to_string()) {
            return Err(format!(
                "delegate agents contains duplicate agent_id '{agent}'"
            ));
        }
        parsed.push(agent.to_string());
    }
    Ok(parsed)
}

fn optional_bool_arg(args: &Value, field: &str) -> Result<Option<bool>, String> {
    match args.get(field) {
        None => Ok(None),
        Some(Value::Bool(value)) => Ok(Some(*value)),
        Some(_) => Err(format!("delegate field '{field}' must be a boolean")),
    }
}

fn optional_u64_arg(args: &Value, field: &str) -> Result<Option<u64>, String> {
    match args.get(field) {
        None => Ok(None),
        Some(value) => value
            .as_u64()
            .map(Some)
            .ok_or_else(|| format!("delegate field '{field}' must be a non-negative integer")),
    }
}

pub(crate) fn coordination_pattern_name(
    pattern: &astra_services::coordination::CoordinationPattern,
) -> &'static str {
    match pattern {
        astra_services::coordination::CoordinationPattern::FanOut { .. } => "fan_out",
        astra_services::coordination::CoordinationPattern::Sequential { .. } => "sequential",
        astra_services::coordination::CoordinationPattern::Fork { .. } => "fork",
    }
}

pub(crate) fn parse_coordination_pattern(
    args: &Value,
) -> Result<astra_services::coordination::CoordinationPattern, String> {
    let pattern_type = match args.get("pattern") {
        None => "sequential",
        Some(Value::String(pattern)) if !pattern.trim().is_empty() => pattern.as_str(),
        Some(_) => return Err("delegate field 'pattern' must be a non-empty string".to_string()),
    };

    let agents = parse_delegate_agents(args)?;
    let timeout = optional_u64_arg(args, "timeout")?.unwrap_or(0);
    match pattern_type {
        "fan_out" if agents.len() >= 2 => {
            Ok(astra_services::coordination::CoordinationPattern::FanOut {
                agent_ids: agents,
                aggregation: astra_services::coordination::AggregationStrategy::AllResults,
                timeout_sec: timeout,
            })
        }
        "fan_out" => Err("delegate pattern 'fan_out' requires at least two agents".to_string()),
        "fork" if agents.len() == 1 => {
            let tasks = args
                .get("tasks")
                .and_then(Value::as_array)
                .ok_or("delegate pattern 'fork' requires a 'tasks' array")?;
            if tasks.len() < 2 {
                return Err("delegate pattern 'fork' requires at least two tasks".to_string());
            }
            let tasks = tasks
                .iter()
                .enumerate()
                .map(|(index, task)| {
                    task.as_str()
                        .map(str::trim)
                        .filter(|task| !task.is_empty())
                        .map(ToString::to_string)
                        .ok_or_else(|| {
                            format!("delegate fork tasks[{index}] must be a non-empty string")
                        })
                })
                .collect::<Result<Vec<_>, _>>()?;
            Ok(astra_services::coordination::CoordinationPattern::Fork {
                tasks,
                agent_id: agents[0].clone(),
                aggregation: astra_services::coordination::AggregationStrategy::AllResults,
                timeout_sec: timeout,
            })
        }
        "fork" => Err("delegate pattern 'fork' requires exactly one agent".to_string()),
        "auto" => {
            let hints = astra_services::coordination::CoordinationHints {
                agent_ids: agents,
                has_dependencies: optional_bool_arg(args, "has_dependencies")?.unwrap_or(false),
                timeout_sec: timeout,
            };
            Ok(astra_services::coordination::suggest_pattern(&hints))
        }
        "sequential" => Ok(
            astra_services::coordination::CoordinationPattern::Sequential {
                agent_ids: agents,
                stop_on_success: false,
                timeout_sec: timeout,
            },
        ),
        unknown => Err(format!(
            "unknown delegate pattern '{unknown}'; expected sequential, fan_out, fork, or auto"
        )),
    }
}

/// Splice an allowlist into the cross-process delegation context as a sorted
/// JSON string array.
///
/// Generic over the element type so callers can pass `HashSet<String>` for
/// `allowed_tools` / `allowed_skills` and `HashSet<SkillSourceKind>` for
/// `allowed_skill_sources` directly — the previous shape forced a manual
/// `to_string()` round-trip at the caller, which lost type information at
/// the boundary and left no way to add a new typed allowlist axis without
/// finding the round-trip code.
///
/// The output is still a JSON `Array<String>` because [`DelegationRequest`]
/// crosses a process boundary with no shared schema; the receiver re-parses
/// each entry through `T::from_str` (or whatever the receiver-side typed
/// allowlist parser uses).
fn merge_request_allowlist_into_delegation_request<T>(
    request: &mut astra_services::coordination::DelegationRequest,
    key: &str,
    allowlist: Option<&HashSet<T>>,
) where
    T: std::fmt::Display,
{
    let Some(allowlist) = allowlist else {
        request.context.remove(key);
        return;
    };
    let mut values: Vec<String> = allowlist.iter().map(|item| item.to_string()).collect();
    values.sort();
    request.context.insert(
        key.to_string(),
        Value::Array(values.into_iter().map(Value::String).collect()),
    );
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

pub(crate) fn merge_workspace_mutation_into_delegation_request(
    request: &mut astra_services::coordination::DelegationRequest,
    workspace_mutation: astra_config::user_profile::WorkspaceMutationIntent,
) {
    // Runtime authority always wins, including Unknown: a model cannot smuggle
    // a stronger or weaker effect policy through its free-form context.
    request
        .context
        .remove(crate::orchestration::WORKSPACE_MUTATION_CONTEXT_KEY);
    if workspace_mutation != astra_config::user_profile::WorkspaceMutationIntent::Unknown {
        request.context.insert(
            crate::orchestration::WORKSPACE_MUTATION_CONTEXT_KEY.to_string(),
            serde_json::to_value(workspace_mutation)
                .expect("workspace mutation intent has a closed JSON representation"),
        );
    }
}

pub(crate) async fn partition_and_execute_delegations(
    tool_calls: &[Value],
    engine: &crate::server::delegation::engine::DelegationEngine,
    parent_run_id: &str,
    session_id: &str,
    recursion_depth: u8,
    self_agent_id: &str,
    workspace_hint: Option<&str>,
    forward_headers: &std::collections::HashMap<String, String>,
    admitted_model_execution: Option<&astra_services::AdmittedModelExecution>,
    parent_model_reasoning: Option<
        &astra_turn_core::orchestration_spawn_tool::ParentModelReasoning,
    >,
    request_constraints: &RequestConstraints,
    adaptive_context: Option<&DelegationAdaptiveContext>,
    parent_delegation_chain: &[String],
    live_event_sink: Option<astra_turn_core::agent_live_event::SharedAgentLiveEventSink>,
    workspace_mutation: astra_config::user_profile::WorkspaceMutationIntent,
) -> (Vec<DelegationExecutionResult>, Vec<Value>) {
    let source_agent_id = self_agent_id;
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
                adaptive_context,
            ) {
                Ok(mut request) => {
                    // This key is runtime-owned. Never let model-authored
                    // delegation context widen or contradict the root effect
                    // boundary.
                    merge_workspace_mutation_into_delegation_request(
                        &mut request,
                        workspace_mutation,
                    );
                    // Inherit parent's delegation chain and append source agent_id
                    // to enable circular delegation detection across hops.
                    request.delegation_chain = parent_delegation_chain.to_vec();
                    request.delegation_chain.push(source_agent_id.to_string());
                    merge_workspace_hint_into_delegation_request(&mut request, workspace_hint);
                    merge_request_allowlist_into_delegation_request(
                        &mut request,
                        REQUEST_ALLOWED_TOOLS_CONTEXT_KEY,
                        request_constraints.allowed_tools.as_ref(),
                    );
                    merge_request_allowlist_into_delegation_request(
                        &mut request,
                        REQUEST_ENABLED_TOOLS_CONTEXT_KEY,
                        request_constraints.enabled_tools.as_ref(),
                    );
                    merge_request_allowlist_into_delegation_request(
                        &mut request,
                        REQUEST_ALLOWED_SKILLS_CONTEXT_KEY,
                        request_constraints.allowed_skills.as_ref(),
                    );
                    merge_request_allowlist_into_delegation_request(
                        &mut request,
                        REQUEST_ALLOWED_SKILL_SOURCES_CONTEXT_KEY,
                        request_constraints.allowed_skill_sources.as_ref(),
                    );
                    if !matches!(
                        request_constraints.delegated_model_requirements,
                        astra_turn_types::DelegationIntentRequirements::Unassessed
                    ) {
                        match serde_json::to_value(
                            &request_constraints.delegated_model_requirements,
                        ) {
                            Ok(value) => {
                                request.context.insert(
                                    astra_turn_types::DELEGATED_MODEL_REQUIREMENTS_CONTEXT_KEY
                                        .to_string(),
                                    value,
                                );
                            }
                            Err(error) => {
                                delegation_results.push(DelegationExecutionResult {
                                    call_id,
                                    summary: format!(
                                        "Delegation blocked: model handoff serialization failed: {error}"
                                    ),
                                    preview_lines: vec![(
                                        HeadlessStderrStyle::Yellow,
                                        "🤝 Delegation blocked — model handoff serialization failed"
                                            .to_string(),
                                    )],
                                    outcome: None,
                                });
                                continue;
                            }
                        }
                    }
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
                    match execute_delegation(
                        engine,
                        request,
                        source_agent_id,
                        forward_headers,
                        admitted_model_execution,
                        parent_model_reasoning.cloned(),
                        live_event_sink.clone(),
                    )
                    .await
                    {
                        Ok(result) => {
                            delegation_results.push(DelegationExecutionResult {
                                call_id,
                                summary: format_delegation_result(&result),
                                preview_lines: format_delegation_terminal_preview(&result),
                                outcome: Some(DelegationOutcomeMetadata {
                                    scenario: scenario_name,
                                    pattern: pattern_name,
                                    succeeded: result.is_success(),
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
    let unfinished = result
        .agent_results
        .iter()
        .filter(|ar| ar.is_unfinished())
        .count();
    let failed = result
        .agent_results
        .len()
        .saturating_sub(succeeded + unfinished);
    let mut parts = Vec::new();
    parts.push(format!(
        "Delegation {} — status: {} ({} ok / {} unfinished / {} failed)",
        result.delegation_id, result.status, succeeded, unfinished, failed
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
        let status_icon = if ar.is_success() {
            "✅"
        } else if ar.is_unfinished() {
            "⏳"
        } else {
            "❌"
        };
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
    let unfinished = result
        .agent_results
        .iter()
        .filter(|ar| ar.is_unfinished())
        .count();
    let failed = result
        .agent_results
        .len()
        .saturating_sub(succeeded + unfinished);
    let status_style = if result.is_success() {
        HeadlessStderrStyle::Green
    } else if result.is_unfinished() || succeeded > 0 {
        HeadlessStderrStyle::Yellow
    } else {
        HeadlessStderrStyle::Red
    };
    let mut lines = vec![(
        status_style,
        format!(
            "🤝 Delegation {} {} [{} ok / {} unfinished / {} failed]",
            result.delegation_id, result.status, succeeded, unfinished, failed
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

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use crate::turn::agentic_loop::host::tests::{
        MockHost, make_state, make_test_delegation_engine,
    };
    use astra_services::AgentProfileRegistry;
    use astra_services::coordination::{AgentProfile, AgentTier};
    use serde_json::json;

    use super::*;

    #[test]
    fn is_delegation_call_detects_delegate_tool() {
        let delegate = json!({
            "id": "call_123",
            "type": "function",
            "function": {
                "name": "delegate",
                "arguments": "{}"
            }
        });
        let non_delegate = json!({
            "id": "call_456",
            "type": "function",
            "function": {
                "name": "bash",
                "arguments": "{}"
            }
        });
        assert!(is_delegation_call(&delegate));
        assert!(!is_delegation_call(&non_delegate));
    }

    #[test]
    fn is_delegation_call_rejects_missing_function() {
        let malformed = json!({"id": "call_000"});
        assert!(!is_delegation_call(&malformed));
    }

    #[test]
    fn runtime_workspace_mutation_authority_overwrites_model_context() {
        let mut request = astra_services::coordination::DelegationRequest {
            delegation_id: "delegation-1".into(),
            session_id: "session-1".into(),
            parent_run_id: "run-1".into(),
            task: "inspect the repository".into(),
            pattern: astra_services::coordination::CoordinationPattern::Sequential {
                agent_ids: vec!["reviewer".into()],
                stop_on_success: true,
                timeout_sec: 0,
            },
            user_id: "user-1".into(),
            depth: 0,
            delegation_chain: Vec::new(),
            context: std::collections::HashMap::from([(
                crate::orchestration::WORKSPACE_MUTATION_CONTEXT_KEY.to_string(),
                json!("must_mutate"),
            )]),
            execution_metadata: None,
        };

        merge_workspace_mutation_into_delegation_request(
            &mut request,
            astra_config::user_profile::WorkspaceMutationIntent::ReadOnly,
        );
        assert_eq!(
            crate::orchestration::workspace_mutation_from_context(&request.context),
            astra_config::user_profile::WorkspaceMutationIntent::ReadOnly
        );

        merge_workspace_mutation_into_delegation_request(
            &mut request,
            astra_config::user_profile::WorkspaceMutationIntent::Unknown,
        );
        assert!(
            !request
                .context
                .contains_key(crate::orchestration::WORKSPACE_MUTATION_CONTEXT_KEY),
            "unknown authority must remove an untrusted model-provided value"
        );
    }

    #[test]
    fn is_delegation_call_rejects_flat_provider_shape() {
        let delegate = json!({
            "id": "call_flat",
            "name": "delegate",
            "arguments": {"task": "review"}
        });
        assert!(!is_delegation_call(&delegate));
    }

    #[test]
    fn tool_call_arguments_value_parses_canonical_argument_string() {
        let tool_call = json!({
            "id": "call_123",
            "type": "function",
            "function": {
                "name": "delegate",
                "arguments": "{\"task\":\"review\",\"agents\":[\"reviewer\"]}"
            }
        });
        assert_eq!(
            tool_call_arguments_value(&tool_call),
            json!({"task":"review","agents":["reviewer"]})
        );
    }

    #[test]
    fn parse_coordination_pattern_defaults_to_sequential() {
        let args = json!({"agents": ["coder", "reviewer"]});
        let pattern = parse_coordination_pattern(&args).unwrap();
        match pattern {
            astra_services::coordination::CoordinationPattern::Sequential {
                agent_ids,
                stop_on_success,
                ..
            } => {
                assert_eq!(agent_ids, vec!["coder", "reviewer"]);
                assert!(!stop_on_success);
            }
            _ => panic!("expected Sequential"),
        }
    }

    #[test]
    fn parse_coordination_pattern_fan_out() {
        let args = json!({"pattern": "fan_out", "agents": ["coder", "writer"]});
        let pattern = parse_coordination_pattern(&args).unwrap();
        match pattern {
            astra_services::coordination::CoordinationPattern::FanOut {
                agent_ids,
                timeout_sec,
                ..
            } => {
                assert_eq!(agent_ids, vec!["coder", "writer"]);
                assert_eq!(timeout_sec, 0);
            }
            _ => panic!("expected FanOut"),
        }
    }

    #[test]
    fn retired_coordination_patterns_are_rejected() {
        for pattern in ["pipeline", "adversarial"] {
            let args = json!({"pattern": pattern, "agents": ["coder", "reviewer"]});
            assert_eq!(
                parse_coordination_pattern(&args).unwrap_err(),
                format!(
                    "unknown delegate pattern '{pattern}'; expected sequential, fan_out, fork, or auto"
                )
            );
        }
    }

    #[test]
    fn parse_coordination_pattern_rejects_invalid_or_incompatible_topology() {
        for (args, expected) in [
            (
                json!({"pattern": "adversarial", "agents": ["coder"]}),
                "unknown delegate pattern",
            ),
            (
                json!({"pattern": "fan_out", "agents": ["coder"]}),
                "requires at least two agents",
            ),
            (
                json!({"pattern": "pipline", "agents": ["coder", "reviewer"]}),
                "unknown delegate pattern",
            ),
            (
                json!({"pattern": "fork", "agents": ["coder"], "tasks": ["only one"]}),
                "requires at least two tasks",
            ),
        ] {
            let error = parse_coordination_pattern(&args).unwrap_err();
            assert!(error.contains(expected), "unexpected error: {error}");
        }
    }

    #[test]
    fn parse_coordination_pattern_accepts_explicit_fork_tasks() {
        let pattern = parse_coordination_pattern(&json!({
            "pattern": "fork",
            "agents": ["coder"],
            "tasks": ["inspect storage", "inspect TUI"],
            "timeout": 30
        }))
        .unwrap();
        assert!(matches!(
            pattern,
            astra_services::coordination::CoordinationPattern::Fork {
                tasks,
                agent_id,
                timeout_sec: 30,
                ..
            } if tasks == ["inspect storage", "inspect TUI"] && agent_id == "coder"
        ));
    }

    #[test]
    fn delegate_rejects_obsolete_fork_hard_limit() {
        for pattern in [Some("fork"), Some("auto"), None] {
            let mut args = json!({
                "task": "Inspect and report evidence",
                "agents": ["coder"],
                "tasks": ["inspect storage", "inspect TUI"],
                "max_turns": 2
            });
            if let Some(pattern) = pattern {
                args["pattern"] = json!(pattern);
            }
            let call = json!({"function": {"arguments": args.to_string()}});
            let error = parse_delegation_request(&call, "run-1", "session-1", 0, None)
                .expect_err("ignored hard limit must not appear accepted");
            assert!(error.contains("max_turns"), "{error}");
        }
    }

    #[test]
    fn parse_delegate_agents_rejects_missing_malformed_and_duplicate_identity() {
        for args in [
            json!({}),
            json!({"agents": []}),
            json!({"agents": ["coder", 7]}),
            json!({"agents": ["coder", " coder "]}),
        ] {
            assert!(parse_delegate_agents(&args).is_err(), "{args}");
        }
    }

    #[test]
    fn parse_delegation_request_extracts_fields() {
        let tool_call = json!({
            "id": "call_abc",
            "type": "function",
            "function": {
                "name": "delegate",
                "arguments": "{\"task\": \"write tests\", \"agents\": [\"coder\"], \"pattern\": \"sequential\", \"context\": {\"repo\": \"my-repo\"}}"
            }
        });
        let req = parse_delegation_request(&tool_call, "run-123", "session-456", 2, None).unwrap();
        assert_eq!(req.task, "write tests");
        assert_eq!(req.parent_run_id, "run-123");
        assert_eq!(req.depth, 2);
        assert_eq!(req.session_id, "session-456");
        assert!(!req.context.contains_key("session_id"));
        assert!(req.context.contains_key("repo"));
    }

    #[test]
    fn parse_delegation_request_keeps_session_identity_out_of_tool_control() {
        let tool_call = json!({
            "id": "call_abc",
            "type": "function",
            "function": {
                "name": "delegate",
                "arguments": "{\"task\": \"write tests\", \"agents\": [\"coder\"], \"context\": {\"session_id\": \"other-session\"}}"
            }
        });

        let request =
            parse_delegation_request(&tool_call, "run-123", "trusted-session", 0, None).unwrap();
        assert_eq!(request.session_id, "trusted-session");
        assert!(
            !request.context.contains_key("session_id"),
            "runtime identity is not child task context"
        );
    }

    #[test]
    fn parse_delegation_request_removes_untrusted_model_requirement_handoff() {
        let tool_call = json!({
            "id": "call_abc",
            "type": "function",
            "function": {
                "name": "delegate",
                "arguments": "{\"task\": \"write tests\", \"agents\": [\"coder\"], \"context\": {\"__astra_delegated_model_requirements\": {\"state\": \"assessed\", \"source\": {\"kind\": \"user\"}}}}"
            }
        });

        let request =
            parse_delegation_request(&tool_call, "run-123", "trusted-session", 0, None).unwrap();
        assert!(
            !request
                .context
                .contains_key(astra_turn_types::DELEGATED_MODEL_REQUIREMENTS_CONTEXT_KEY),
            "model-authored context must not become a trusted admission handoff"
        );
    }

    #[test]
    fn parse_delegation_request_strips_reserved_forward_headers_even_with_trusted_state() {
        let tool_call = json!({
            "id": "call_abc",
            "type": "function",
            "function": {
                "name": "delegate",
                "arguments": "{\"task\": \"write tests\", \"agents\": [\"coder\"], \"context\": {\"__astra_forward_headers\": {\"x-workspace-id\": \"evil\"}}}"
            }
        });
        let req = parse_delegation_request(&tool_call, "run-123", "session-456", 2, None).unwrap();
        assert!(
            !req.context.contains_key(FORWARD_HEADERS_CONTEXT_KEY),
            "forwarded headers should stay in trusted sideband state"
        );
    }

    #[test]
    fn parse_delegation_request_removes_untrusted_forward_headers_when_trusted_map_empty() {
        let tool_call = json!({
            "id": "call_abc",
            "type": "function",
            "function": {
                "name": "delegate",
                "arguments": "{\"task\": \"write tests\", \"agents\": [\"coder\"], \"context\": {\"__astra_forward_headers\": {\"x-workspace-id\": \"evil\"}}}"
            }
        });
        let req = parse_delegation_request(&tool_call, "run-123", "session-456", 2, None).unwrap();
        assert!(
            !req.context.contains_key(FORWARD_HEADERS_CONTEXT_KEY),
            "trusted state should clear any user-supplied forward headers"
        );
    }

    #[test]
    fn parse_delegation_request_handles_missing_args() {
        let tool_call = json!({
            "id": "call_bad",
            "type": "function",
            "function": {
                "name": "delegate"
            }
        });
        let result = parse_delegation_request(&tool_call, "run-1", "sess-1", 0, None);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("missing arguments"));
    }

    #[test]
    fn parse_delegation_request_rejects_missing_or_malformed_typed_fields() {
        for (arguments, expected) in [
            (r#"{"agents":["coder"]}"#, "non-empty string field 'task'"),
            (r#"{"task":"work"}"#, "string array field 'agents'"),
            (
                r#"{"task":"work","agents":["coder"],"pattern":7}"#,
                "field 'pattern' must be a non-empty string",
            ),
            (
                r#"{"task":"work","agents":["coder"],"context":"repo"}"#,
                "field 'context' must be an object",
            ),
            (
                r#"{"task":"work","agents":["coder"],"needs_review":true}"#,
                "field 'needs_review' is not supported",
            ),
            (
                r#"{"task":"work","agents":["coder"],"max_rounds":2}"#,
                "field 'max_rounds' is not supported",
            ),
        ] {
            let tool_call = json!({
                "type": "function",
                "function": {"name": "delegate", "arguments": arguments}
            });
            let error =
                parse_delegation_request(&tool_call, "run-1", "sess-1", 0, None).unwrap_err();
            assert!(error.contains(expected), "unexpected error: {error}");
        }
    }

    #[test]
    fn parse_delegation_request_rejects_max_recursion_depth() {
        let tool_call = json!({
            "type": "function",
            "function": {
                "name": "delegate",
                "arguments": "{\"task\": \"review this patch\", \"agents\": [\"reviewer\"]}"
            }
        });

        let result = parse_delegation_request(
            &tool_call,
            "run-1",
            "sess-1",
            astra_turn_core::agentic_recursion_guard::ABSOLUTE_MAX_AGENT_RECURSION_DEPTH,
            None,
        );
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .contains("recursion depth 8 reached absolute safety ceiling 8")
        );
    }

    #[test]
    fn parse_delegation_request_without_pattern_uses_exploration_fan_out() {
        let tool_call = json!({
            "type": "function",
            "function": {
                "name": "delegate",
                "arguments": "{\"task\": \"search the codebase for relevant modules\", \"agents\": [\"coder\", \"reviewer\"]}"
            }
        });
        let adaptive_context = DelegationAdaptiveContext {
            scenario: Some(astra_config::user_profile::Scenario::Exploration),
        };

        let req = parse_delegation_request(
            &tool_call,
            "run-123",
            "session-456",
            0,
            Some(&adaptive_context),
        )
        .unwrap();

        assert!(matches!(
            req.pattern,
            astra_services::coordination::CoordinationPattern::FanOut { .. }
        ));
        assert_eq!(
            req.context["adaptive_coordination"]["selected_pattern"],
            json!("fan_out")
        );
    }

    #[test]
    fn parse_delegation_request_uses_typed_default_without_task_prose_matching() {
        let tool_call = json!({
            "type": "function",
            "function": {
                "name": "delegate",
                "arguments": "{\"task\": \"review and verify this patch\", \"agents\": [\"coder\", \"reviewer\"]}"
            }
        });

        let req = parse_delegation_request(&tool_call, "run-123", "session-456", 0, None).unwrap();

        assert!(matches!(
            req.pattern,
            astra_services::coordination::CoordinationPattern::FanOut { .. }
        ));
        assert_eq!(
            req.context["adaptive_coordination"]["selection_source"],
            json!("typed_hints")
        );
    }

    #[test]
    fn select_default_preserves_all_agents_and_timeout() {
        let args = json!({
            "agents": ["first", "second", "third"],
            "timeout": 45,
        });

        let (pattern, policy) = select_default_coordination_pattern(&args, None).unwrap();
        match pattern {
            astra_services::coordination::CoordinationPattern::FanOut {
                agent_ids,
                timeout_sec,
                ..
            } => {
                assert_eq!(agent_ids, ["first", "second", "third"]);
                assert_eq!(timeout_sec, 45);
            }
            other => panic!("expected fan-out for independent agents, got {other:?}"),
        }
        assert_eq!(policy["selection_source"], "typed_hints");
    }

    #[test]
    fn select_default_dependency_hint_preserves_order_and_timeout() {
        let args = json!({
            "agents": ["plan", "verify", "publish"],
            "has_dependencies": true,
            "timeout": 30,
        });

        let (pattern, _) = select_default_coordination_pattern(&args, None).unwrap();
        match pattern {
            astra_services::coordination::CoordinationPattern::Sequential {
                agent_ids,
                timeout_sec,
                stop_on_success,
            } => {
                assert_eq!(agent_ids, ["plan", "verify", "publish"]);
                assert_eq!(timeout_sec, 30);
                assert!(!stop_on_success);
            }
            other => panic!("expected sequential dependencies, got {other:?}"),
        }
    }

    #[test]
    fn format_delegation_result_includes_status_and_agents() {
        let result = astra_services::coordination::DelegationResult {
            delegation_id: "del-1".to_string(),
            status: "completed".to_string(),
            agent_results: vec![astra_services::coordination::AgentResult {
                agent_id: "coder".to_string(),
                run_id: "run-1".to_string(),
                status: "completed".to_string(),
                output: Some("implemented feature X".to_string()),
                error: None,
                prompt_tokens: 100,
                completion_tokens: 50,
                tool_calls: 3,
            }],
            aggregated_output: Some("All tasks done.".to_string()),
            total_prompt_tokens: 100,
            total_completion_tokens: 50,
            total_tool_calls: 3,
        };
        let formatted = format_delegation_result(&result);
        assert!(formatted.contains("del-1"));
        assert!(formatted.contains("completed"));
        assert!(formatted.contains("✅"));
        assert!(formatted.contains("coder"));
        assert!(formatted.contains("implemented feature X"));
        assert!(formatted.contains("All tasks done"));
        assert!(
            formatted
                .find("Final aggregated result")
                .unwrap_or(usize::MAX)
                < formatted.find("Sub-agent results").unwrap_or(usize::MAX)
        );
        assert!(formatted.contains("Tokens:"));
    }

    #[test]
    fn format_delegation_result_truncates_long_output() {
        let long_output = "x".repeat(1000);
        let result = astra_services::coordination::DelegationResult {
            delegation_id: "del-2".to_string(),
            status: "completed".to_string(),
            agent_results: vec![astra_services::coordination::AgentResult {
                agent_id: "writer".to_string(),
                run_id: "run-2".to_string(),
                status: "completed".to_string(),
                output: Some(long_output),
                error: None,
                prompt_tokens: 200,
                completion_tokens: 100,
                tool_calls: 1,
            }],
            aggregated_output: None,
            total_prompt_tokens: 200,
            total_completion_tokens: 100,
            total_tool_calls: 1,
        };
        let formatted = format_delegation_result(&result);
        assert!(formatted.contains('…'));
        assert!(formatted.len() < 1500);
    }

    #[test]
    fn format_delegation_result_shows_errors() {
        let result = astra_services::coordination::DelegationResult {
            delegation_id: "del-3".to_string(),
            status: "partial_failure".to_string(),
            agent_results: vec![astra_services::coordination::AgentResult {
                agent_id: "coder".to_string(),
                run_id: "run-3".to_string(),
                status: "failed".to_string(),
                output: None,
                error: Some("timeout".to_string()),
                prompt_tokens: 50,
                completion_tokens: 0,
                tool_calls: 0,
            }],
            aggregated_output: None,
            total_prompt_tokens: 50,
            total_completion_tokens: 0,
            total_tool_calls: 0,
        };
        let formatted = format_delegation_result(&result);
        assert!(formatted.contains("❌"));
        assert!(formatted.contains("timeout"));
        assert!(formatted.contains("partial_failure"));
    }

    #[test]
    fn format_delegation_result_surfaces_unfinished_agents_distinctly() {
        let result = astra_services::coordination::DelegationResult {
            delegation_id: "del-unfinished".to_string(),
            status: "unfinished".to_string(),
            agent_results: vec![
                astra_services::coordination::AgentResult {
                    agent_id: "coder".to_string(),
                    run_id: "run-1".to_string(),
                    status: "completed".to_string(),
                    output: Some("done".to_string()),
                    error: None,
                    prompt_tokens: 0,
                    completion_tokens: 0,
                    tool_calls: 0,
                },
                astra_services::coordination::AgentResult {
                    agent_id: "reviewer".to_string(),
                    run_id: "run-2".to_string(),
                    status: "waiting".to_string(),
                    output: Some("waiting for approval".to_string()),
                    error: None,
                    prompt_tokens: 0,
                    completion_tokens: 0,
                    tool_calls: 0,
                },
            ],
            aggregated_output: None,
            total_prompt_tokens: 0,
            total_completion_tokens: 0,
            total_tool_calls: 0,
        };

        let formatted = format_delegation_result(&result);
        assert!(formatted.contains("1 ok / 1 unfinished / 0 failed"));
        assert!(formatted.contains("⏳"));

        let preview = format_delegation_terminal_preview(&result);
        assert_eq!(preview[0].0, HeadlessStderrStyle::Yellow);
        assert!(preview[0].1.contains("unfinished"));
    }

    #[test]
    fn format_delegation_result_falls_back_to_single_success_output() {
        let result = astra_services::coordination::DelegationResult {
            delegation_id: "del-fallback".to_string(),
            status: "completed".to_string(),
            agent_results: vec![astra_services::coordination::AgentResult {
                agent_id: "coder".to_string(),
                run_id: "run-1".to_string(),
                status: "completed".to_string(),
                output: Some("single-agent final answer".to_string()),
                error: None,
                prompt_tokens: 0,
                completion_tokens: 0,
                tool_calls: 0,
            }],
            aggregated_output: None,
            total_prompt_tokens: 0,
            total_completion_tokens: 0,
            total_tool_calls: 0,
        };
        let formatted = format_delegation_result(&result);
        assert!(formatted.contains("Final aggregated result"));
        assert!(formatted.contains("single-agent final answer"));
    }

    #[test]
    fn format_delegation_terminal_preview_surfaces_summary_first() {
        let result = astra_services::coordination::DelegationResult {
            delegation_id: "del-preview".to_string(),
            status: "completed".to_string(),
            agent_results: vec![astra_services::coordination::AgentResult {
                agent_id: "coder".to_string(),
                run_id: "run-1".to_string(),
                status: "completed".to_string(),
                output: Some("implemented feature X".to_string()),
                error: None,
                prompt_tokens: 0,
                completion_tokens: 0,
                tool_calls: 0,
            }],
            aggregated_output: Some("Final merged answer".to_string()),
            total_prompt_tokens: 0,
            total_completion_tokens: 0,
            total_tool_calls: 0,
        };
        let lines = format_delegation_terminal_preview(&result);
        assert_eq!(lines[0].0, HeadlessStderrStyle::Green);
        assert!(lines[0].1.contains("del-preview"));
        assert!(lines[1].1.contains("Final merged answer"));
    }

    fn make_partition_engine(
        root_agent_id: &str,
        agent_ids: &[&str],
    ) -> crate::server::delegation::engine::DelegationEngine {
        use crate::server::delegation::engine::{
            DelegationEngine, DelegationTracker, StubSubRunExecutor,
        };
        use crate::server::run::engine::RunEngine;

        let mut registry = AgentProfileRegistry::new();
        let _ = registry.register(AgentProfile::new(
            root_agent_id,
            &root_agent_id.to_uppercase(),
            AgentTier::Orchestrator,
        ));
        for agent_id in agent_ids {
            let _ = registry.register(AgentProfile::new(
                agent_id,
                &agent_id.to_uppercase(),
                AgentTier::System,
            ));
        }
        let run_store = Arc::new(astra_services::runs::InMemoryRunStateStore::default());
        DelegationEngine::with_executor(
            Arc::new(tokio::sync::RwLock::new(registry)),
            Arc::new(RunEngine::new(run_store)),
            Arc::new(DelegationTracker::new()),
            Arc::new(StubSubRunExecutor),
        )
    }

    #[tokio::test]
    async fn partition_separates_delegate_from_regular_calls() {
        let engine = make_partition_engine("main", &["coder"]);
        let tool_calls = vec![
            json!({
                "id": "call_delegate",
                "type": "function",
                "function": {
                    "name": "delegate",
                    "arguments": "{\"task\": \"write tests\", \"agents\": [\"coder\"]}"
                }
            }),
            json!({
                "id": "call_bash",
                "type": "function",
                "function": {
                    "name": "bash",
                    "arguments": "{\"command\": \"ls\"}"
                }
            }),
        ];

        let (delegation_results, remaining) = partition_and_execute_delegations(
            &tool_calls,
            &engine,
            "test-run",
            "test-session",
            0,
            "main",
            None,
            &std::collections::HashMap::new(),
            None,
            None,
            &RequestConstraints::default(),
            None,
            &[],
            None,
            Default::default(),
        )
        .await;

        assert_eq!(delegation_results.len(), 1);
        assert_eq!(remaining.len(), 1);
        assert_eq!(delegation_results[0].call_id, "call_delegate");
        assert!(delegation_results[0].summary.contains("Delegation"));
        assert_eq!(remaining[0]["id"], "call_bash");
    }

    #[tokio::test]
    async fn partition_handles_all_delegate_calls() {
        let engine = make_partition_engine("main", &["coder", "reviewer"]);
        let tool_calls = vec![
            json!({
                "id": "d1",
                "function": {"name": "delegate", "arguments": "{\"task\": \"code\", \"agents\": [\"coder\"]}"}
            }),
            json!({
                "id": "d2",
                "function": {"name": "delegate", "arguments": "{\"task\": \"review\", \"agents\": [\"reviewer\"]}"}
            }),
        ];

        let (delegation_results, remaining) = partition_and_execute_delegations(
            &tool_calls,
            &engine,
            "run-1",
            "sess-1",
            0,
            "main",
            None,
            &std::collections::HashMap::new(),
            None,
            None,
            &RequestConstraints::default(),
            None,
            &[],
            None,
            Default::default(),
        )
        .await;

        assert_eq!(delegation_results.len(), 2);
        assert!(remaining.is_empty());
    }

    #[tokio::test]
    async fn partition_handles_invalid_delegation_args_gracefully() {
        let engine = make_partition_engine("main", &[]);
        let tool_calls = vec![json!({
            "id": "bad_call",
            "function": {"name": "delegate", "arguments": "not valid json!!!"}
        })];

        let (delegation_results, remaining) = partition_and_execute_delegations(
            &tool_calls,
            &engine,
            "run-1",
            "sess-1",
            0,
            "main",
            None,
            &std::collections::HashMap::new(),
            None,
            None,
            &RequestConstraints::default(),
            None,
            &[],
            None,
            Default::default(),
        )
        .await;

        assert_eq!(delegation_results.len(), 1);
        assert!(
            delegation_results[0]
                .summary
                .contains("Invalid delegation request")
        );
        assert!(remaining.is_empty());
    }

    #[tokio::test]
    async fn partition_preserves_target_error_with_exact_source_identity() {
        let engine = make_partition_engine("main", &[]);
        let tool_calls = vec![json!({
            "id": "missing_target",
            "function": {"name": "delegate", "arguments": "{\"task\": \"code\", \"agents\": [\"coder\"]}"}
        })];

        let (delegation_results, remaining) = partition_and_execute_delegations(
            &tool_calls,
            &engine,
            "run-1",
            "sess-1",
            0,
            "main",
            None,
            &std::collections::HashMap::new(),
            None,
            None,
            &RequestConstraints::default(),
            None,
            &[],
            None,
            Default::default(),
        )
        .await;

        assert_eq!(delegation_results.len(), 1);
        assert!(remaining.is_empty());
        assert!(
            delegation_results[0]
                .summary
                .contains("target agent 'coder' not registered"),
            "{:?}",
            delegation_results[0]
        );
    }

    #[tokio::test]
    async fn partition_rejects_retired_root_agent_alias() {
        let engine = make_partition_engine("main", &["coder"]);
        let tool_calls = vec![json!({
            "id": "retired_source",
            "function": {"name": "delegate", "arguments": "{\"task\": \"code\", \"agents\": [\"coder\"]}"}
        })];

        let (delegation_results, remaining) = partition_and_execute_delegations(
            &tool_calls,
            &engine,
            "run-1",
            "sess-1",
            0,
            "orchestrator",
            None,
            &std::collections::HashMap::new(),
            None,
            None,
            &RequestConstraints::default(),
            None,
            &[],
            None,
            Default::default(),
        )
        .await;

        assert_eq!(delegation_results.len(), 1);
        assert!(remaining.is_empty());
        assert!(
            delegation_results[0].summary.contains("orchestrator")
                && delegation_results[0].summary.contains("not registered"),
            "{:?}",
            delegation_results[0]
        );
    }

    #[tokio::test]
    async fn intercept_delegations_respects_valid_tool_names() {
        let mut host = MockHost::new(Vec::new()).with_valid_tools(&[]);
        let mut state = make_state();
        state.delegation_engine = Some(make_test_delegation_engine("unknown", "unknown").await);

        let turn_result = HostTurnResult {
            accum: astra_turn_core::chat_turn_sse_dispatch::ChatTurnSseAccum {
                has_tool_calls: true,
                tool_calls: vec![json!({
                    "id": "call_delegate",
                    "type": "function",
                    "function": {
                        "name": "delegate",
                        "arguments": "{\"task\":\"write tests\",\"agents\":[\"coder\"]}"
                    }
                })],
                ..astra_turn_core::chat_turn_sse_dispatch::ChatTurnSseAccum::default()
            },
            ttft_ms: Some(10),
            edge_tool_round: Vec::new(),
            error_kind: None,
        };

        let valid_tool_names = host.valid_tool_names().clone();
        let result = intercept_delegations(
            &mut host,
            &mut state,
            &turn_result.accum.tool_calls,
            true,
            &valid_tool_names,
        )
        .await;

        assert!(
            !result.intercepted_any,
            "disallowed delegate should not be intercepted"
        );
        assert_eq!(
            result.effective_tool_calls, turn_result.accum.tool_calls,
            "delegate call should remain for unknown-tool handling"
        );
        assert!(
            state.tool_results.is_empty(),
            "no synthetic delegation result should be injected when delegate is disallowed"
        );
    }

    #[tokio::test]
    async fn intercept_delegations_uses_exact_root_agent_identity() {
        let mut host = MockHost::new(Vec::new()).with_valid_tools(&["delegate"]);
        let mut state = make_state();
        state.delegation_engine = Some(Arc::new(make_partition_engine("main", &["coder"])));

        let turn_result = HostTurnResult {
            accum: astra_turn_core::chat_turn_sse_dispatch::ChatTurnSseAccum {
                has_tool_calls: true,
                tool_calls: vec![json!({
                    "id": "call_delegate",
                    "type": "function",
                    "function": {
                        "name": "delegate",
                        "arguments": "{\"task\":\"write tests\",\"agents\":[\"coder\"]}"
                    }
                })],
                ..astra_turn_core::chat_turn_sse_dispatch::ChatTurnSseAccum::default()
            },
            ttft_ms: Some(10),
            edge_tool_round: Vec::new(),
            error_kind: None,
        };

        let valid_tool_names = host.valid_tool_names().clone();
        let result = intercept_delegations(
            &mut host,
            &mut state,
            &turn_result.accum.tool_calls,
            true,
            &valid_tool_names,
        )
        .await;

        assert!(result.intercepted_any);
        assert_eq!(result.effective_tool_calls, turn_result.accum.tool_calls);
        assert_eq!(result.pre_resolved_results.len(), 1);
        assert_eq!(result.pre_resolved_results[0].call_id, "call_delegate");
        assert!(
            result.pre_resolved_results[0]
                .content
                .contains("Delegation"),
            "{:?}",
            result.pre_resolved_results
        );
        assert!(state.tool_results.is_empty());
    }

    #[tokio::test]
    async fn intercept_delegations_refuses_after_per_turn_limit() {
        let mut host = MockHost::new(Vec::new()).with_valid_tools(&["delegate"]);
        let mut state = make_state();
        state.delegation_engine = Some(make_test_delegation_engine("unknown", "unknown").await);
        // Simulate 3 delegations already executed this turn.
        state.delegations_this_turn = 3;

        let turn_result = HostTurnResult {
            accum: crate::turn::chat_turn_sse_dispatch::ChatTurnSseAccum {
                has_tool_calls: true,
                tool_calls: vec![json!({
                    "id": "call_4th",
                    "type": "function",
                    "function": {
                        "name": "delegate",
                        "arguments": "{\"task\":\"one more\",\"agents\":[\"coder\"]}"
                    }
                })],
                ..crate::turn::chat_turn_sse_dispatch::ChatTurnSseAccum::default()
            },
            ttft_ms: Some(10),
            edge_tool_round: Vec::new(),
            error_kind: None,
        };

        let valid_tool_names = host.valid_tool_names().clone();
        let result = intercept_delegations(
            &mut host,
            &mut state,
            &turn_result.accum.tool_calls,
            true,
            &valid_tool_names,
        )
        .await;

        assert!(result.intercepted_any);
        assert_eq!(result.effective_tool_calls, turn_result.accum.tool_calls);
        assert_eq!(result.pre_resolved_results.len(), 1);
        assert_eq!(result.pre_resolved_results[0].call_id, "call_4th");
        let content = &result.pre_resolved_results[0].content;
        assert!(
            content.contains("Delegation limit reached"),
            "expected refusal message, got: {content}"
        );
        assert!(state.messages.is_empty(), "headless owns round history");
        // Counter should NOT have incremented (delegation was refused).
        assert_eq!(state.delegations_this_turn, 3);
    }
}
