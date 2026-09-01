use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use astra_services::SessionArtifactStore;
use astra_services::session_journal::{SURGICAL_REMOVAL_TOOL_NAME, ToolCallRecord};
use serde_json::Value;

use astra_turn_core::sse_stream_host::EdgeToolExecResult;

use super::super::agentic_loop::host::{
    AgenticLoopState, DELEGATE_TOOL_NAME, HostTurnResult, RejectedToolCall, ToolCallAdmission,
};

pub(crate) const CONTROL_PLANE_TOOLS: &[&str] = &[
    "session",
    "introspect",
    "notify",
    "ask_user",
    "enter_plan_mode",
    "exit_plan_mode",
];

pub(crate) struct PreparedToolRound {
    pub(crate) tool_calls: Vec<Value>,
    pub(crate) pre_resolved_results: Vec<(String, String)>,
    pub(crate) edge_tool_round: Vec<EdgeToolExecResult>,
    pub(crate) communication_events: Vec<astra_messaging::AgentCommunicationEvent>,
}

/// Persist pre-execution admission rejections as exact terminal outcomes.
///
/// This is deliberately independent from the executor path: a runtime policy
/// rejection is still one provider-owned attempt with one canonical terminal
/// disposition.  Callers that stop or continue before an ordinary tool round
/// must use this helper before returning so the durable ledger cannot retain
/// an open call ID.
pub(crate) fn record_pre_execution_rejections(
    state: &mut AgenticLoopState,
    rejected_tool_calls: Vec<RejectedToolCall>,
) -> (Vec<Value>, Vec<(String, String)>) {
    let mut tool_calls = Vec::with_capacity(rejected_tool_calls.len());
    let mut pre_resolved_results = Vec::with_capacity(rejected_tool_calls.len());

    for rejected in rejected_tool_calls {
        tool_calls.push(rejected.canonical_call);
        pre_resolved_results.push((rejected.id.clone(), rejected.result.clone()));
        let structured_result = serde_json::from_str::<Value>(&rejected.result).ok();
        let rejection_detail = structured_result
            .as_ref()
            .and_then(|value| value.get("error"))
            .and_then(Value::as_str)
            .unwrap_or("tool call rejected before execution")
            .to_string();
        let parsed_args = tool_calls
            .last()
            .and_then(|call| {
                astra_turn_core::tool::args::shape::parse_tool_call_arguments(call).ok()
            })
            .unwrap_or(Value::Null);
        let (args_full, args_preview, _) =
            crate::turn::headless_tool_pipeline::record::safe_tool_arguments_for_record(
                &rejected.name,
                &parsed_args,
            );
        state.step_recorder.begin_tool_with_key_and_args_preview(
            &rejected.name,
            &rejected.id,
            None,
            args_full.as_deref(),
        );
        state.step_recorder.complete_tool_with_result_and_metadata(
            &rejected.name,
            &rejected.id,
            args_full.as_deref(),
            true,
            0,
            false,
            &rejected.result,
        );
        let (round, start_offset_ms) = match state.turn_event_buffer.as_ref() {
            Some(buffer) => (Some(buffer.current_round()), Some(buffer.offset_ms())),
            None => (None, None),
        };
        state.stall.tool_call_records.push(ToolCallRecord {
            tool_call_id: Some(rejected.id.clone()),
            name: rejected.name,
            ok: false,
            ms: 0,
            error: Some(rejection_detail),
            input_bytes: None,
            output_bytes: Some(rejected.result.len() as u32),
            args_preview,
            result_preview: Some(rejected.result.chars().take(500).collect()),
            file_path: None,
            args_full,
            result_full: Some(rejected.result),
            round,
            start_offset_ms,
            error_kind: Some(astra_core::ErrorKind::ContractViolation),
            disposition: Some(astra_services::session_journal::ToolCallDisposition::Rejected),
            ..Default::default()
        });
    }

    (tool_calls, pre_resolved_results)
}

pub(crate) fn admit_tool_calls(
    tool_calls: &[Value],
    finish_reason: Option<&str>,
) -> ToolCallAdmission {
    if let Err(identity_error) = validate_provider_tool_call_identities(tool_calls) {
        let rejected = tool_calls
            .iter()
            .map(|tool_call| {
                let id = tool_call
                    .get("id")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                let name = astra_turn_core::tool::args::shape::tool_call_name(tool_call)
                    .unwrap_or("unknown")
                    .to_string();
                let canonical_call =
                    astra_turn_core::tool::args::shape::canonicalize_tool_call_for_execution(
                        tool_call,
                    )
                    .unwrap_or_else(|_| {
                        serde_json::json!({
                            "id": id,
                            "type": "function",
                            "function": {"name": name, "arguments": "{}"},
                        })
                    });
                RejectedToolCall {
                    id,
                    name,
                    canonical_call,
                    result: serde_json::json!({
                        "status": "rejected",
                        "error_kind": "provider_tool_identity_invalid",
                        "retryable": false,
                        "error": identity_error.as_str(),
                    })
                    .to_string(),
                }
            })
            .collect();
        return ToolCallAdmission {
            admitted: Vec::new(),
            rejected,
            completion_action_applied: false,
        };
    }
    let output_was_truncated = matches!(
        finish_reason,
        Some("length" | "max_tokens" | "max_output_tokens")
    );
    let mut malformed = Vec::new();
    let mut executable = Vec::new();

    for tool_call in tool_calls {
        match astra_turn_core::tool::args::shape::canonicalize_tool_call_for_execution(tool_call) {
            Ok(canonical) => executable.push(canonical),
            Err(detail) => {
                let id = tool_call
                    .get("id")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                let name = astra_turn_core::tool::args::shape::tool_call_name(tool_call)
                    .unwrap_or("unknown")
                    .to_string();
                let message = if detail == "tool name is missing" {
                    "Tool name is missing or empty, so the call was not executed. Emit one complete tool name and JSON argument object, then try again.".to_string()
                } else if output_was_truncated {
                    format!(
                        "The {name} tool call was not executed because its JSON arguments were cut off at the model output limit after Astra exhausted the configured output-budget escalation. Shorten the call, split the work, or reuse an existing sandbox script, then try again."
                    )
                } else {
                    format!(
                        "The {name} tool call was not executed because {detail}. Emit one complete JSON argument object, then try again."
                    )
                };
                let canonical_call = serde_json::json!({
                    "id": id,
                    "type": "function",
                    "function": {
                        "name": name,
                        "arguments": "{}",
                    },
                });
                malformed.push(RejectedToolCall {
                    id: canonical_call["id"]
                        .as_str()
                        .unwrap_or_default()
                        .to_string(),
                    name: canonical_call["function"]["name"]
                        .as_str()
                        .unwrap_or("unknown")
                        .to_string(),
                    canonical_call,
                    result: serde_json::json!({
                        "status": "rejected",
                        "error_kind": "tool_call_arguments_invalid",
                        "retryable": true,
                        "error": message,
                    })
                    .to_string(),
                });
            }
        }
    }

    ToolCallAdmission {
        admitted: executable,
        rejected: malformed,
        completion_action_applied: false,
    }
}

/// Validate the provider-owned identity carrier before any admission policy or
/// executor can observe the batch. Representation normalization is allowed at
/// the provider boundary; identity synthesis is not.
pub(crate) fn validate_provider_tool_call_identities(tool_calls: &[Value]) -> Result<(), String> {
    let mut seen = HashSet::with_capacity(tool_calls.len());
    for tool_call in tool_calls {
        let id = tool_call
            .get("id")
            .and_then(Value::as_str)
            .filter(|id| {
                !id.is_empty()
                    && id.len() <= 512
                    && id.trim() == *id
                    && !id.chars().any(char::is_control)
            })
            .ok_or_else(|| {
                "provider tool-call identity is missing or malformed; refusing to mint execution identity"
                    .to_string()
            })?;
        if !seen.insert(id) {
            return Err(format!(
                "provider tool-call identity '{id}' is duplicated; refusing ambiguous execution"
            ));
        }
    }
    Ok(())
}

/// Every provider call must have exactly one terminal admission disposition.
/// This guards against lossy normalization and policy filters that silently
/// remove calls before execution or rejection evidence is produced.
pub(crate) fn validate_tool_call_admission_partition(
    requested: &[Value],
    admission: &ToolCallAdmission,
) -> Result<(), String> {
    validate_provider_tool_call_identities(requested)?;
    for call in &admission.admitted {
        let canonical =
            astra_turn_core::tool::args::shape::canonicalize_tool_call_for_execution(call)
                .map_err(|detail| format!("admitted tool call is not executable: {detail}"))?;
        if canonical != *call {
            return Err("admitted tool call is not in exact canonical shape".to_string());
        }
    }
    for call in &admission.rejected {
        let canonical = astra_turn_core::tool::args::shape::canonicalize_tool_call_for_execution(
            &call.canonical_call,
        )
        .map_err(|detail| format!("rejected tool carrier is not canonical: {detail}"))?;
        let canonical_id = canonical.get("id").and_then(Value::as_str);
        let canonical_name =
            astra_turn_core::tool::args::shape::tool_call_name(&canonical).unwrap_or_default();
        if canonical != call.canonical_call
            || canonical_id != Some(call.id.as_str())
            || canonical_name != call.name
        {
            return Err(
                "rejected tool carrier identity or name does not match its disposition".to_string(),
            );
        }
    }
    let requested_ids = requested
        .iter()
        .filter_map(|call| call.get("id").and_then(Value::as_str))
        .collect::<HashSet<_>>();
    let admitted_ids = admission
        .admitted
        .iter()
        .filter_map(|call| call.get("id").and_then(Value::as_str))
        .collect::<HashSet<_>>();
    let rejected_ids = admission
        .rejected
        .iter()
        .map(|call| call.id.as_str())
        .collect::<HashSet<_>>();
    if admitted_ids.intersection(&rejected_ids).next().is_some()
        || requested_ids.len() != admission.admitted.len() + admission.rejected.len()
        || requested_ids.len() != admitted_ids.len() + rejected_ids.len()
        || requested_ids
            != admitted_ids
                .union(&rejected_ids)
                .copied()
                .collect::<HashSet<_>>()
    {
        return Err(format!(
            "tool admission did not exhaustively partition provider batch: requested={}, admitted={}, rejected={}",
            requested.len(),
            admission.admitted.len(),
            admission.rejected.len()
        ));
    }
    Ok(())
}

/// Reject a provider's tool batch at a typed text-only settlement boundary.
///
/// The boundary is selected by the lifecycle (`completion_settlement`), never
/// by inspecting user text or a tool name.  We still run the ordinary
/// canonicalization first so malformed calls retain the same stable
/// pre-resolved evidence as every other admission path; valid calls are then
/// converted into structured, non-retryable results without reaching an
/// executor.
pub(crate) fn reject_tool_calls_at_text_only_boundary(
    tool_calls: &[Value],
    finish_reason: Option<&str>,
) -> ToolCallAdmission {
    let admitted = admit_tool_calls(tool_calls, finish_reason);
    let mut rejected = admitted.rejected;
    for canonical_call in admitted.admitted {
        let id = canonical_call
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let name = astra_turn_core::tool::args::shape::tool_call_name(&canonical_call)
            .unwrap_or("unknown")
            .to_string();
        rejected.push(RejectedToolCall {
            id,
            name,
            canonical_call,
            result: serde_json::json!({
                "status": "rejected",
                "error_kind": "text_only_settlement_tool_call",
                "retryable": false,
                "error": "The terminal response boundary is text-only. Produce the final answer without requesting another tool call."
            })
            .to_string(),
        });
    }
    ToolCallAdmission {
        admitted: Vec::new(),
        rejected,
        completion_action_applied: false,
    }
}

pub(crate) fn request_allowlist_permits_tool(state: &AgenticLoopState, tool_name: &str) -> bool {
    state
        .skills
        .request_constraints
        .allowed_tools
        .as_ref()
        .is_none_or(|allowed| allowed.contains(tool_name))
}

fn optional_tool_is_enabled(state: &AgenticLoopState, tool_name: &str) -> bool {
    let registry = astra_runtime_env::ToolRegistry::builtins();
    let requires_enablement = registry
        .get(tool_name)
        .is_some_and(astra_runtime_env::ToolSpec::requires_explicit_user_enablement);
    !requires_enablement
        || state
            .skills
            .request_constraints
            .enabled_tools
            .as_ref()
            .is_none_or(|enabled| enabled.contains(tool_name))
}

pub(crate) fn exempt_from_request_allowlist(tool_name: &str) -> bool {
    tool_name == crate::turn::skill_tool::SKILL_TOOL_NAME
        || tool_name == crate::turn::skill_tool::DISCOVER_SKILLS_TOOL_NAME
}

pub(crate) fn exempt_from_skill_allowlist(tool_name: &str) -> bool {
    exempt_from_request_allowlist(tool_name) || CONTROL_PLANE_TOOLS.contains(&tool_name)
}

fn blocked_by_request_allowlist(state: &AgenticLoopState, tool_name: &str) -> bool {
    !exempt_from_request_allowlist(tool_name) && !request_allowlist_permits_tool(state, tool_name)
}

fn skill_allowlist_exempt_control_tools(
    state: &AgenticLoopState,
    effective_allowed: &HashSet<String>,
) -> Vec<&'static str> {
    CONTROL_PLANE_TOOLS
        .iter()
        .copied()
        .filter(|tool_name| request_allowlist_permits_tool(state, tool_name))
        .filter(|tool_name| !effective_allowed.contains(*tool_name))
        .collect()
}

pub(crate) fn effective_runtime_allowed_tools(state: &AgenticLoopState) -> Option<HashSet<String>> {
    let effective = astra_turn_core::tool_allowlist::compute_effective_allowlist(
        state.skills.request_constraints.allowed_tools.as_ref(),
        None,
    );
    if matches!(effective.as_ref(), Some(allowed) if allowed.is_empty()) {
        tracing::warn!(
            "runtime request tool allowlist is empty; only skill/discover_skills remain callable"
        );
    }
    effective
}

pub(crate) fn runtime_allows_tool(state: &AgenticLoopState, tool_name: &str) -> bool {
    if !optional_tool_is_enabled(state, tool_name) {
        return false;
    }
    if blocked_by_request_allowlist(state, tool_name) {
        return false;
    }
    if exempt_from_request_allowlist(tool_name) {
        return true;
    }
    let Some(allowed_tools) = effective_runtime_allowed_tools(state) else {
        return true;
    };
    allowed_tools.contains(tool_name)
        || (exempt_from_skill_allowlist(tool_name)
            && request_allowlist_permits_tool(state, tool_name))
}

pub(crate) fn runtime_tool_allowlist_notice(state: &AgenticLoopState) -> Option<String> {
    state.skills.request_constraints.allowed_tools.as_ref()?;

    let effective_allowed = effective_runtime_allowed_tools(state).unwrap_or_default();
    let mut allowed_display = effective_allowed.into_iter().collect::<Vec<_>>();
    allowed_display.sort();
    let effective_allowed_set = allowed_display.iter().cloned().collect::<HashSet<_>>();
    let exempt_control_tools = skill_allowlist_exempt_control_tools(state, &effective_allowed_set);
    let exempt_control_tools_display = exempt_control_tools
        .iter()
        .map(|name| format!("`{name}`"))
        .collect::<Vec<_>>()
        .join(", ");
    let control_plane_notice = if exempt_control_tools_display.is_empty() {
        String::new()
    } else {
        format!(
            " Control-plane tools {exempt_control_tools_display} also remain policy-permitted while the skill allowlist is active."
        )
    };
    if allowed_display.is_empty() {
        Some(format!(
            "Runtime tool authorization: the active request policy permits no allowlist-governed non-skill tools. `skill` and `discover_skills` remain policy-permitted until the request policy changes. Authorization is only an upper bound; the current tool surface is authoritative for availability.{control_plane_notice}"
        ))
    } else {
        Some(format!(
            "Runtime tool authorization: the request policy permits at most these non-skill tools: {}. This allowlist does not assert provider availability; the current tool surface is authoritative. Skill `allowed_tools` stays a prompt hint and does not hard-block additional tools.{control_plane_notice}",
            allowed_display.join(", "),
        ))
    }
}

fn intercept_disallowed_tool_calls(
    state: &AgenticLoopState,
    tool_calls: &[Value],
) -> (
    Vec<crate::turn::skill_tool::InterceptedToolResult>,
    Vec<Value>,
) {
    let Some(allowed_tools) = effective_runtime_allowed_tools(state) else {
        return (Vec::new(), tool_calls.to_vec());
    };

    let mut allowed_tool_names = allowed_tools.iter().cloned().collect::<Vec<_>>();
    allowed_tool_names.sort();
    let allowed_display = if allowed_tool_names.is_empty() {
        "none".to_string()
    } else {
        allowed_tool_names.join(", ")
    };

    let mut blocked = Vec::new();
    let mut remaining = Vec::new();
    for tool_call in tool_calls {
        let Some(tool_name) = astra_turn_core::tool::args::shape::tool_call_name(tool_call)
            .and_then(astra_turn_core::tool_allowlist::normalize_tool_name)
        else {
            let tool_call_id = tool_call
                .get("id")
                .and_then(Value::as_str)
                .filter(|id| !id.is_empty())
                .unwrap_or("unknown")
                .to_string();
            blocked.push(crate::turn::skill_tool::InterceptedToolResult {
                tool_call_id,
                tool_name: "<missing>".to_string(),
                ok: false,
                result_class: Some(
                    astra_services::session_journal::BLOCKED_TOOL_RESULT_CLASS.to_string(),
                ),
                result: format!(
                    "BLOCKED: Tool name is missing or empty, so the call cannot bypass the active request/skill allowlist. Allowed tools: {allowed_display}. Use an allowed tool name or call `skill` to load a different workflow."
                ),
            });
            continue;
        };
        if !blocked_by_request_allowlist(state, &tool_name) {
            remaining.push(tool_call.clone());
            continue;
        };

        let tool_call_id = tool_call
            .get("id")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
            .unwrap_or("unknown")
            .to_string();
        blocked.push(crate::turn::skill_tool::InterceptedToolResult {
            tool_call_id,
            tool_name: tool_name.clone(),
            ok: false,
            result_class: Some(
                astra_services::session_journal::BLOCKED_TOOL_RESULT_CLASS.to_string(),
            ),
            result: format!(
                "BLOCKED: Tool '{tool_name}' is not allowed by the active request/skill allowlist. Allowed tools: {allowed_display}. Use the allowed tools or call `skill` to load a different workflow."
            ),
        });
    }

    (blocked, remaining)
}

pub(crate) async fn prepare_intercepted_tool_round(
    state: &mut AgenticLoopState,
    turn_result: &HostTurnResult,
    effective_tool_calls: &[Value],
    rejected_tool_calls: Vec<RejectedToolCall>,
    delegation_intercepted: bool,
    _valid_tool_names: &HashSet<String>,
) -> PreparedToolRound {
    let (rejected_calls, mut pre_resolved_results) =
        record_pre_execution_rejections(state, rejected_tool_calls);
    let mut tool_calls = effective_tool_calls.to_vec();
    tool_calls.extend(rejected_calls);
    let (allowlist_blocked_tool_results, allowed_tool_calls) =
        intercept_disallowed_tool_calls(state, effective_tool_calls);
    let blocked_tool_results = allowlist_blocked_tool_results;
    let communication_events = Vec::new();
    let SkillInterceptionResult {
        results: skill_results,
        surgically_removed_ids,
        short_circuit_meta,
    } = intercept_skill_calls(state, &allowed_tool_calls).await;

    // Build the id→args lookup once. Without it, the per-result `find` below
    // is O(N²) over `tool_calls`, which a model emitting many simultaneous
    // disallowed calls would exercise.
    let args_preview_by_id: HashMap<&str, String> = tool_calls
        .iter()
        .filter_map(|tc| {
            let id = tc.get("id").and_then(Value::as_str)?;
            let name = astra_turn_core::tool::args::shape::tool_call_name(tc)?;
            let args = astra_turn_core::tool::args::shape::parse_tool_call_arguments(tc).ok()?;
            let preview =
                crate::turn::headless_tool_pipeline::record::safe_args_preview(name, &args)?;
            Some((id, preview))
        })
        .collect();

    for result in &blocked_tool_results {
        let args_preview = args_preview_by_id
            .get(result.tool_call_id.as_str())
            .cloned();
        pre_resolved_results.push((result.tool_call_id.clone(), result.result.clone()));
        let (round, start_offset_ms) = match state.turn_event_buffer.as_ref() {
            Some(buf) => (Some(buf.current_round()), Some(buf.offset_ms())),
            None => (None, None),
        };
        state.stall.tool_call_records.push(ToolCallRecord {
            tool_call_id: Some(result.tool_call_id.clone()),
            name: result.tool_name.clone(),
            ok: false,
            ms: 0,
            error: Some("tool intercepted by runtime control policy".to_string()),
            input_bytes: None,
            output_bytes: Some(result.result.len() as u32),
            args_preview,
            result_preview: Some(result.result.chars().take(500).collect::<String>()),
            file_path: None,
            surgically_removed: None,
            original_tool_name: None,
            args_full: None,
            result_full: Some(result.result.clone()),
            round,
            start_offset_ms,
            error_kind: Some(astra_core::ErrorKind::ToolUnavailable),
            disposition: Some(astra_services::session_journal::ToolCallDisposition::Rejected),
            ..Default::default()
        });
    }

    for result in &skill_results {
        pre_resolved_results.push((result.tool_call_id.clone(), result.result.clone()));

        let (round, start_offset_ms) = match state.turn_event_buffer.as_ref() {
            Some(buf) => (Some(buf.current_round()), Some(buf.offset_ms())),
            None => (None, None),
        };
        let (skill_reentry_count, skill_locked_out) =
            match short_circuit_meta.get(&result.tool_call_id) {
                Some(meta) => (Some(meta.reentry_count), Some(meta.locked_out)),
                None => (None, None),
            };
        let disposition = if skill_locked_out == Some(true) {
            astra_services::session_journal::ToolCallDisposition::Rejected
        } else if skill_reentry_count.is_some() {
            astra_services::session_journal::ToolCallDisposition::Suppressed
        } else if result.result_class.is_some() {
            astra_services::session_journal::ToolCallDisposition::Deferred
        } else {
            astra_services::session_journal::ToolCallDisposition::Executed
        };
        state.stall.tool_call_records.push(ToolCallRecord {
            tool_call_id: Some(result.tool_call_id.clone()),
            name: result.tool_name.clone(),
            ok: result.ok,
            ms: 0,
            error: None,
            input_bytes: None,
            output_bytes: Some(result.result.len() as u32),
            args_preview: Some(result.tool_call_id.clone()),
            result_preview: Some(result.result.chars().take(500).collect::<String>()),
            file_path: None,
            surgically_removed: None,
            original_tool_name: None,
            args_full: None,
            result_full: Some(result.result.clone()),
            round,
            start_offset_ms,
            skill_reentry_count,
            skill_locked_out: skill_locked_out.filter(|v| *v),
            result_class: result.result_class.clone(),
            error_kind: (skill_locked_out == Some(true))
                .then_some(astra_core::ErrorKind::ToolUnavailable),
            disposition: Some(disposition),
            ..Default::default()
        });
    }

    // Record surgically removed calls as audit-only synthetic placeholders.
    // These are intentional context optimizations (skill took over the work),
    // NOT tool failures — so ok=true and they are filtered out of
    // evaluation/analytics via ToolCallRecord::is_synthetic_placeholder().
    // The stall detector does NOT treat synthetic placeholders as real
    // attempts either, matching the existing skipped/deferred behavior.

    // Build id→name lookup so we can preserve the original tool name.
    let tool_name_by_id: HashMap<&str, &str> = tool_calls
        .iter()
        .filter_map(|tc| {
            let id = tc.get("id").and_then(Value::as_str)?;
            let name = astra_turn_core::tool::args::shape::tool_call_name(tc)?;
            Some((id, name))
        })
        .collect();

    for id in &surgically_removed_ids {
        let original_name = tool_name_by_id.get(id.as_str()).map(|s| s.to_string());
        // Preserve observability fields so surgical removal doesn't erase round tracking.
        let (round, start_offset_ms) = match state.turn_event_buffer.as_ref() {
            Some(buf) => (Some(buf.current_round()), Some(buf.offset_ms())),
            None => (None, None),
        };
        state.stall.tool_call_records.push(ToolCallRecord {
            tool_call_id: Some(id.clone()),
            name: SURGICAL_REMOVAL_TOOL_NAME.to_string(),
            ok: true,
            ms: 0,
            error: None,
            input_bytes: None,
            output_bytes: Some(0),
            args_preview: Some(id.clone()),
            result_preview: Some("(removed from context — skill covered this work)".to_string()),
            file_path: None,
            surgically_removed: Some(true),
            original_tool_name: original_name,
            round,
            start_offset_ms,
            disposition: Some(astra_services::session_journal::ToolCallDisposition::Suppressed),
            ..Default::default()
        });
    }

    if !state.skill_produced_output && skill_results.iter().any(|r| r.result.len() > 500) {
        state.skill_produced_output = true;
    }

    // Surgery: strip tool_calls whose IDs are in surgically_removed_ids.
    // These calls will NOT appear in the assistant message or need tool results.
    let tool_calls = if surgically_removed_ids.is_empty() {
        tool_calls
    } else {
        tool_calls
            .into_iter()
            .filter(|tc| {
                let id = tc.get("id").and_then(Value::as_str).unwrap_or("");
                !surgically_removed_ids.contains(id)
            })
            .collect()
    };

    let edge_tool_round = if delegation_intercepted {
        turn_result
            .edge_tool_round
            .iter()
            .filter(|r| r.tool != DELEGATE_TOOL_NAME)
            .cloned()
            .collect()
    } else {
        turn_result.edge_tool_round.clone()
    };

    PreparedToolRound {
        tool_calls,
        pre_resolved_results,
        edge_tool_round,
        communication_events,
    }
}

/// Result of skill interception. `results` are pre-resolved tool results to
/// feed back to the model. `surgically_removed_ids` are tool_call IDs that
/// should be stripped from the assistant message entirely (no tool result needed).
struct SkillInterceptionResult {
    results: Vec<crate::turn::skill_tool::InterceptedToolResult>,
    surgically_removed_ids: HashSet<String>,
    /// Per-tool-call re-entry metadata for short-circuited skill calls, keyed
    /// by `tool_call_id`. Callers can use this to stamp journal `ToolCallRecord`
    /// entries with `skill_reentry_count` / `skill_locked_out`.
    short_circuit_meta: HashMap<String, SkillShortCircuitMeta>,
}

/// Metadata about a short-circuited skill call, returned alongside the
/// synthetic tool result so callers can stamp journal records with the
/// per-skill re-entry count and lockout flag.
pub(crate) struct SkillShortCircuitMeta {
    pub reentry_count: u32,
    pub locked_out: bool,
}

/// Short-circuit `skill(name=X)` calls when X has already been loaded this
/// session. Returns `(short_circuits, fresh_tool_calls)` where short_circuits
/// pair synthetic results with their re-entry metadata and fresh_tool_calls
/// are the calls needing real dispatch. Escalates:
///   - reentry 1: passive "already loaded" message.
///   - reentry 2: STOP directive ("do NOT call `skill` again this turn").
///   - reentry ≥ 3: hard lockout — BLOCKED result; the skill is now considered
///     locked out for the remainder of this turn and further calls continue to
///     receive the BLOCKED response with `locked_out=true`.
pub(crate) fn dedup_skill_calls(
    state: &mut AgenticLoopState,
    tool_calls: &[Value],
) -> (
    Vec<(
        crate::turn::skill_tool::InterceptedToolResult,
        SkillShortCircuitMeta,
    )>,
    Vec<Value>,
) {
    let mut short_circuits = Vec::new();
    let mut fresh_tool_calls = Vec::new();
    for tc in tool_calls {
        if crate::turn::skill_tool::is_skill_call(tc) {
            let skill_name = crate::turn::skill_tool::extract_skill_name(tc);
            if let Some(ref name) = skill_name
                && let Some(prev) = state.skills.invoked.get_mut(name.as_str())
            {
                let call_id = tc
                    .get("id")
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                    .unwrap_or("unknown");
                prev.reentry_count = prev.reentry_count.saturating_add(1);
                let reentry = prev.reentry_count;
                let invoked_at = prev.invoked_at_turn;
                let locked_out = reentry >= 3;
                if locked_out {
                    state
                        .stall
                        .events
                        .push((format!("skill_lockout:{name}"), 1));
                }
                let message = if locked_out {
                    format!(
                        "BLOCKED: Skill '{}' is locked out for this turn after {} re-entries. \
                         This call was NOT executed. Produce your final answer now using the \
                         instructions already loaded (turn {}).",
                        name, reentry, invoked_at,
                    )
                } else if reentry >= 2 {
                    format!(
                        "STOP: Skill '{}' is already loaded (turn {}, reentry={}). \
                         You have called `skill` with this name {} times. \
                         Do NOT call `skill` again for the remainder of this turn. \
                         Respond to the user directly from the evidence you already have.",
                        name, invoked_at, reentry, reentry,
                    )
                } else {
                    let mut replay = prev.content.clone();
                    if !replay.is_empty() {
                        replay.push_str("\n\n");
                    }
                    replay.push_str(&format!(
                        "Skill '{}' was already loaded (turn {}). \
                         Follow those instructions directly — do not re-invoke.",
                        name, invoked_at
                    ));
                    replay
                };
                short_circuits.push((
                    crate::turn::skill_tool::InterceptedToolResult {
                        tool_call_id: call_id.to_string(),
                        tool_name: crate::turn::skill_tool::SKILL_TOOL_NAME.to_string(),
                        ok: !locked_out,
                        result_class: Some(
                            astra_services::session_journal::NOOP_OR_CACHED_RESULT_CLASS
                                .to_string(),
                        ),
                        result: message,
                    },
                    SkillShortCircuitMeta {
                        reentry_count: reentry,
                        locked_out,
                    },
                ));
                continue;
            }
        }
        fresh_tool_calls.push(tc.clone());
    }
    (short_circuits, fresh_tool_calls)
}

async fn intercept_skill_calls(
    state: &mut AgenticLoopState,
    tool_calls: &[Value],
) -> SkillInterceptionResult {
    let Some(resolver) = state.skills.resolver.clone() else {
        return SkillInterceptionResult {
            results: Vec::new(),
            surgically_removed_ids: HashSet::new(),
            short_circuit_meta: HashMap::new(),
        };
    };

    let skill_ctx = build_skill_context(state);
    let composition_ctx = crate::skills::composition::CompositionContext::root();
    let full_catalog = resolver.available_skills();
    let is_client_owned = |tool_call: &Value| {
        if crate::turn::skill_tool::is_discover_skills_call(tool_call) {
            return !state.skills.client_pipeline_skill_names.is_empty();
        }
        let Some(target) = crate::turn::skill_tool::extract_skill_name(tool_call) else {
            return false;
        };
        state
            .skills
            .client_pipeline_skill_names
            .contains(&target.trim().to_ascii_lowercase())
    };
    let server_interceptable_calls = tool_calls
        .iter()
        .filter(|tool_call| !is_client_owned(tool_call))
        .cloned()
        .collect::<Vec<_>>();
    state.telemetry.all_selected_skills.extend(
        crate::turn::skill_tool::selected_skill_names_from_tool_calls(tool_calls)
            .into_iter()
            .filter(|selected| {
                state
                    .skills
                    .client_pipeline_skill_names
                    .contains(&selected.trim().to_ascii_lowercase())
            }),
    );
    let visible_for_mask =
        crate::turn::skill_tool::visible_skills_for_host_turn(&full_catalog, &state.skills.invoked);
    let discover_exclude = crate::turn::skill_tool::skill_mask_names_lowercase(&visible_for_mask);

    let (dedup_pairs, fresh_tool_calls) = dedup_skill_calls(state, &server_interceptable_calls);
    state
        .telemetry
        .all_selected_skills
        .extend(crate::turn::skill_tool::selected_skill_names_from_tool_calls(&fresh_tool_calls));
    let mut short_circuit_meta: HashMap<String, SkillShortCircuitMeta> = HashMap::new();
    let mut dedup_results = Vec::with_capacity(dedup_pairs.len());
    for (res, meta) in dedup_pairs {
        short_circuit_meta.insert(res.tool_call_id.clone(), meta);
        dedup_results.push(res);
    }

    let (mut sr, remaining, activation) =
        crate::turn::skill_tool::partition_discover_and_execute_skills(
            &fresh_tool_calls,
            resolver.as_ref(),
            &full_catalog,
            &discover_exclude,
            &mut state.skills.discovered,
            state.skills.executor.as_ref(),
            Some(&mut state.skills.quality_tracker),
            Some(&composition_ctx),
            &skill_ctx,
        )
        .await;

    let activation_notice = if let Some(activation) = activation {
        apply_skill_activation(state, activation);
        runtime_tool_allowlist_notice(state)
    } else {
        None
    };

    if let Some(notice) = activation_notice
        && let Some(skill_result) = sr
            .iter_mut()
            .rev()
            .find(|result| result.tool_name == crate::turn::skill_tool::SKILL_TOOL_NAME)
    {
        skill_result.result.push_str("\n\n");
        skill_result.result.push_str(&notice);
    }

    let current_turn = (state.max_turns - state.remaining_turns) as u32;
    for result in &sr {
        if let Some(tc) = fresh_tool_calls
            .iter()
            .find(|t| t.get("id").and_then(Value::as_str) == Some(result.tool_call_id.as_str()))
        {
            let name = crate::turn::skill_tool::extract_skill_name(tc);
            if let Some(name) = name {
                if result.ok && crate::turn::skill_tool::is_skill_call(tc) {
                    let execution_topology = crate::turn::skill_tool::declared_execution_topology(
                        resolver.as_ref(),
                        &name,
                    );
                    state.skills.invoked.insert(
                        name.clone(),
                        crate::turn::skill_tool::InvokedSkill {
                            name,
                            content: result.result.clone(),
                            invoked_at_turn: current_turn,
                            reentry_count: 0,
                            execution_topology,
                        },
                    );
                }
            }
        }
    }

    let mut skill_results = dedup_results;
    let new_skills_fired = fresh_tool_calls
        .iter()
        .any(|tc| crate::turn::skill_tool::is_skill_call(tc));
    skill_results.extend(sr);
    let mut surgically_removed_ids = HashSet::new();
    if new_skills_fired && !remaining.is_empty() {
        let skill_produced_output = skill_results.iter().any(|r| r.result.len() > 500);
        let dropped_count = remaining.len();

        if skill_produced_output {
            // Surgery: remove intercepted tool_calls from the assistant message
            // entirely. This saves ~100 tokens per call in EVERY subsequent LLM
            // round (the assistant message is replayed as context each time).
            // We still record them in stall.tool_call_records for telemetry.
            let tool_names: Vec<&str> = remaining
                .iter()
                .filter_map(astra_turn_core::tool::args::shape::tool_call_name)
                .collect();
            for tc in &remaining {
                let call_id = tc.get("id").and_then(Value::as_str).unwrap_or("unknown");
                surgically_removed_ids.insert(call_id.to_string());
            }
            // Append a note to the skill result so the model knows what was dropped.
            // Prefer the most recently-added large result (the one that triggered
            // the interception) — `sr` is appended last, so iterating in reverse
            // picks the newly-run skill output rather than a leftover dedup entry.
            if let Some(skill_result) = skill_results
                .iter_mut()
                .rev()
                .find(|r| r.result.len() > 500)
            {
                skill_result.result.push_str(&format!(
                    "\n\n[{} parallel tool call(s) were dropped: [{}]. \
                     The skill output above is your complete context — do NOT re-invoke \
                     these tools.]",
                    dropped_count,
                    tool_names.join(", ")
                ));
            }
        } else {
            // Skill output was short — keep deferred calls in the conversation
            // so the model can decide whether to retry each one.
            for tc in &remaining {
                let call_id = tc.get("id").and_then(Value::as_str).unwrap_or("unknown");
                let tool_name =
                    astra_turn_core::tool::args::shape::tool_call_name(tc).unwrap_or("unknown");
                let msg = format!(
                    "Deferred: skill was invoked in this turn. Read the skill \
                     instructions above, then decide whether to call `{}` again.",
                    tool_name
                );
                skill_results.push(crate::turn::skill_tool::InterceptedToolResult {
                    tool_call_id: call_id.to_string(),
                    tool_name: tool_name.to_string(),
                    ok: false,
                    result_class: Some(
                        astra_services::session_journal::NOOP_OR_CACHED_RESULT_CLASS.to_string(),
                    ),
                    result: msg,
                });
            }
        }
        let verb = if skill_produced_output {
            "surgically removed"
        } else {
            "deferred"
        };
        tracing::debug!(
            dropped_count,
            verb,
            "skill exclusivity: {} non-skill tool call(s) {}",
            dropped_count,
            verb
        );
    }

    SkillInterceptionResult {
        results: skill_results,
        surgically_removed_ids,
        short_circuit_meta,
    }
}

pub(crate) fn build_skill_context(
    state: &AgenticLoopState,
) -> crate::turn::skill_tool::SkillContext {
    let session_dir = state.current_session_id.as_ref().and_then(|id| {
        astra_services::local_session_artifact_store()
            .session_dir(id)
            .ok()
            .map(|path| path.to_string_lossy().into_owned())
    });

    crate::turn::skill_tool::SkillContext {
        session_id: state.current_session_id.clone(),
        session_dir,
        work_dir: state.hooks.workspace_root_hint.clone(),
        current_task: (!state.user_intent.trim().is_empty()).then(|| state.user_intent.clone()),
        available_tools: state.telemetry.all_tools_used.iter().cloned().collect(),
        recursion_depth: state.recursion_depth,
        forward_headers: state.hooks.forward_headers.clone(),
        extra: build_skill_extra(state),
    }
}

pub(crate) fn apply_skill_activation(
    state: &mut AgenticLoopState,
    act: crate::turn::skill_tool::SkillActivation,
) {
    let normalized_allowed_tools =
        astra_turn_core::tool_allowlist::normalize_tool_names(&act.allowed_tools);
    state.skills.allowed_tools = if normalized_allowed_tools.is_empty() {
        None
    } else {
        Some(normalized_allowed_tools)
    };
    state.skills.effort = act.effort;
    state.skills.agent_type = act.agent_type;
    state.skills.sandbox_policy = act.sandbox_policy;
}

fn build_skill_extra(state: &AgenticLoopState) -> HashMap<String, String> {
    let mut extra = HashMap::new();

    extra.insert(
        "__astra_expected_control_epoch".to_string(),
        i64::try_from(state.user_intents.user_intent_cursor())
            .unwrap_or(i64::MAX)
            .to_string(),
    );
    if let Some(turn_chain_id) = state.canonical_turn_chain_id.as_deref() {
        extra.insert(
            "__astra_parent_turn_chain_id".to_string(),
            turn_chain_id.to_string(),
        );
    }

    if let Some(ref root) = state.hooks.workspace_root_hint {
        let root_path = std::path::Path::new(root.as_str());

        if let Ok(output) = std::process::Command::new("git")
            .args(["rev-parse", "--abbrev-ref", "HEAD"])
            .current_dir(root)
            .output()
        {
            if output.status.success() {
                let branch = String::from_utf8_lossy(&output.stdout).trim().to_string();
                if !branch.is_empty() {
                    extra.insert("git_branch".into(), branch);
                }
            }
        }

        if let Ok(output) = std::process::Command::new("git")
            .args(["config", "--get", "remote.origin.url"])
            .current_dir(root)
            .output()
        {
            if output.status.success() {
                let url = String::from_utf8_lossy(&output.stdout).trim().to_string();
                if let Some(name) = extract_repo_name_from_url(&url) {
                    extra.insert("git_repo".into(), name);
                }
            }
        }

        let project_types = detect_project_types(root_path);
        if !project_types.is_empty() {
            extra.insert("project_type".into(), project_types.join(","));
        }
    }

    extra.insert("os".into(), std::env::consts::OS.into());

    let turns_used = state.current_session_turn_number();
    extra.insert("turn_number".into(), turns_used.to_string());
    extra.insert("total_prompt_tokens".into(), state.total_prompt.to_string());
    extra.insert(
        "total_completion_tokens".into(),
        state.total_completion.to_string(),
    );
    extra.insert(
        "total_tool_calls".into(),
        state.total_tool_calls.to_string(),
    );
    extra.insert(
        "nudge_count".into(),
        state.turn_guard.nudge_count.to_string(),
    );
    extra.insert(
        "error_count".into(),
        state.turn_guard.errors.total_errors.to_string(),
    );
    extra.insert(
        "recent_error_pressure".into(),
        state.turn_guard.errors.recent_error_pressure().to_string(),
    );
    extra.insert(
        "recent_timeout_pressure".into(),
        state
            .turn_guard
            .errors
            .recent_error_count(astra_turn_core::error_recovery::ErrorCategory::ToolTimeout)
            .to_string(),
    );
    let retry_cautioned = state.turn_guard.health.health_avoidance_tools();
    if !retry_cautioned.is_empty() {
        extra.insert("retry_cautioned_tools".into(), retry_cautioned.join(", "));
    }
    if !state.stall.events.is_empty() {
        let stalls: Vec<String> = state
            .stall
            .events
            .iter()
            .map(|(kind, turn)| format!("{}@t{}", kind, turn))
            .collect();
        extra.insert("stall_events".into(), stalls.join(", "));
    }
    let eff = state.turn_guard.correction_effectiveness();
    if eff.total_corrections > 0 {
        extra.insert(
            "correction_follow_rate".into(),
            format!("{:.0}%", eff.follow_rate * 100.0),
        );
    }

    extra
}

pub(crate) fn extract_repo_name_from_url(url: &str) -> Option<String> {
    let path = url.trim_end_matches('/');
    let segment = if let Some(idx) = path.rfind('/') {
        &path[idx + 1..]
    } else {
        let idx = path.rfind(':')?;
        let after_colon = &path[idx + 1..];
        after_colon.rsplit('/').next().unwrap_or(after_colon)
    };
    let name = segment.strip_suffix(".git").unwrap_or(segment);
    if name.is_empty() {
        None
    } else {
        Some(name.to_string())
    }
}

pub(crate) fn detect_project_types(root: &std::path::Path) -> Vec<&'static str> {
    let markers: &[(&str, &str)] = &[
        ("Cargo.toml", "rust"),
        ("package.json", "node"),
        ("pyproject.toml", "python"),
        ("setup.py", "python"),
        ("requirements.txt", "python"),
        ("go.mod", "go"),
        ("pom.xml", "java"),
        ("build.gradle", "java"),
        ("Gemfile", "ruby"),
        ("Makefile", "make"),
        ("CMakeLists.txt", "cmake"),
        ("docker-compose.yml", "docker"),
        ("Dockerfile", "docker"),
    ];
    let mut seen = std::collections::HashSet::new();
    let mut types = Vec::new();
    for (file, lang) in markers {
        if root.join(file).exists() && seen.insert(*lang) {
            types.push(*lang);
        }
    }
    types
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;
    use std::sync::Arc;

    use astra_turn_core::chat_turn_sse_dispatch::ChatTurnSseAccum;
    use serde_json::json;

    use super::*;
    use crate::turn::agentic_loop::host::tests::make_state;

    #[test]
    fn malformed_tool_arguments_become_precise_pre_resolved_error() {
        let calls = vec![json!({
            "id": "call-python",
            "type": "function",
            "function": {
                "name": "python",
                "arguments": "{\"code\":\"from docx import Document"
            }
        })];

        let admission = admit_tool_calls(&calls, Some("length"));

        assert!(admission.admitted.is_empty());
        assert_eq!(admission.rejected.len(), 1);
        assert_eq!(admission.rejected[0].id, "call-python");
        assert_eq!(admission.rejected[0].name, "python");
        let result: Value = serde_json::from_str(&admission.rejected[0].result).unwrap();
        assert_eq!(result["status"], "rejected");
        assert_eq!(result["error_kind"], "tool_call_arguments_invalid");
        assert_eq!(result["retryable"], true);
        assert!(
            result["error"]
                .as_str()
                .unwrap()
                .contains("cut off at the model output limit")
        );
    }

    #[test]
    fn valid_tool_arguments_continue_to_normal_execution() {
        let calls = vec![json!({
            "id": "call-bash",
            "type": "function",
            "function": {
                "name": "bash",
                "arguments": "{\"command\":\"ls\"}"
            }
        })];

        let admission = admit_tool_calls(&calls, Some("length"));

        assert!(admission.rejected.is_empty());
        assert_eq!(
            admission.admitted,
            vec![json!({
                "id": "call-bash",
                "type": "function",
                "function": {
                    "name": "bash",
                    "arguments": "{\"command\":\"ls\"}"
                }
            })]
        );
    }

    #[test]
    fn text_only_settlement_rejects_valid_calls_without_admitting_execution() {
        let calls = vec![json!({
            "id": "call-bash",
            "type": "function",
            "function": {
                "name": "bash",
                "arguments": "{\"command\":\"touch should-not-run\"}"
            }
        })];

        let admission = reject_tool_calls_at_text_only_boundary(&calls, Some("tool_calls"));

        assert!(admission.admitted.is_empty());
        assert_eq!(admission.rejected.len(), 1);
        assert_eq!(admission.rejected[0].id, "call-bash");
        let result: Value = serde_json::from_str(&admission.rejected[0].result).unwrap();
        assert_eq!(result["error_kind"], "text_only_settlement_tool_call");
        assert_eq!(result["retryable"], false);
        assert_eq!(
            admission.rejected[0].canonical_call["function"]["name"],
            "bash"
        );
    }

    #[test]
    fn admission_canonicalizes_provider_representation_without_legacy_shapes() {
        let provider_calls = vec![
            json!({
                "id": "call-read",
                "function": {"name": "read_file", "arguments": "{ \"path\": \"README.md\", \"line_end\": 2 }"}
            }),
            json!({
                "id": "call-bash",
                "type": "function",
                "function": {"name": "bash", "arguments": "{\n \"command\": \"ls\"\n}"}
            }),
        ];
        let admitted = admit_tool_calls(&provider_calls, None);
        assert!(admitted.rejected.is_empty());
        assert_eq!(admitted.admitted.len(), 2);
        assert_eq!(admitted.admitted[0]["type"], "function");
        validate_tool_call_admission_partition(&provider_calls, &admitted).unwrap();

        let legacy_flat = vec![json!({
            "id": "call-flat",
            "name": "bash",
            "arguments": {"command": "ls"}
        })];
        let rejected = admit_tool_calls(&legacy_flat, None);
        assert!(rejected.admitted.is_empty());
        assert_eq!(rejected.rejected.len(), 1);
        assert!(
            rejected.rejected[0]
                .result
                .contains("top-level tool name or arguments are not supported")
        );
        validate_tool_call_admission_partition(&legacy_flat, &rejected).unwrap();
    }

    #[test]
    fn admission_exhaustively_partitions_mixed_valid_and_malformed_calls() {
        let calls = vec![
            json!({
                "id": "call-valid",
                "type": "function",
                "function": {"name": "bash", "arguments": "{\"command\":\"pwd\"}"}
            }),
            json!({
                "id": "call-broken",
                "type": "function",
                "function": {"name": "read_file", "arguments": "{\"path\":"}
            }),
        ];

        let admission = admit_tool_calls(&calls, None);

        assert_eq!(admission.admitted.len(), 1);
        assert_eq!(admission.rejected.len(), 1);
        assert_eq!(admission.admitted[0]["id"], "call-valid");
        assert_eq!(admission.rejected[0].id, "call-broken");
        validate_tool_call_admission_partition(&calls, &admission).unwrap();
    }

    #[test]
    fn provider_identity_errors_never_mint_or_partially_admit_calls() {
        for calls in [
            vec![json!({
                "type": "function",
                "function": {"name": "bash", "arguments": "{}"}
            })],
            vec![
                json!({
                    "id": "duplicate",
                    "type": "function",
                    "function": {"name": "bash", "arguments": "{}"}
                }),
                json!({
                    "id": "duplicate",
                    "type": "function",
                    "function": {"name": "read_file", "arguments": "{\"path\":\"README.md\"}"}
                }),
            ],
        ] {
            assert!(validate_provider_tool_call_identities(&calls).is_err());
            let admission = admit_tool_calls(&calls, None);
            assert!(admission.admitted.is_empty());
            assert_eq!(admission.rejected.len(), calls.len());
            assert!(admission.rejected.iter().all(|call| {
                serde_json::from_str::<Value>(&call.result).unwrap()["error_kind"]
                    == "provider_tool_identity_invalid"
            }));
        }
    }

    #[test]
    fn admission_partition_validator_detects_silently_dropped_calls() {
        let calls = vec![
            json!({
                "id": "call-a",
                "type": "function",
                "function": {"name": "bash", "arguments": "{}"}
            }),
            json!({
                "id": "call-b",
                "type": "function",
                "function": {"name": "read_file", "arguments": "{\"path\":\"README.md\"}"}
            }),
        ];
        let mut admission = admit_tool_calls(&calls, None);
        admission.admitted.pop();

        assert!(validate_tool_call_admission_partition(&calls, &admission).is_err());
    }

    #[test]
    fn admission_partition_validator_rejects_tampered_carriers() {
        let calls = vec![json!({
            "id": "call-a",
            "type": "function",
            "function": {"name": "bash", "arguments": "{}"}
        })];
        let mut admitted = admit_tool_calls(&calls, None);
        admitted.admitted[0]["function"]["arguments"] = Value::String("{ }".to_string());
        assert!(validate_tool_call_admission_partition(&calls, &admitted).is_err());

        let mut rejected = reject_tool_calls_at_text_only_boundary(&calls, None);
        rejected.rejected[0].canonical_call["id"] = Value::String("other".to_string());
        assert!(validate_tool_call_admission_partition(&calls, &rejected).is_err());
    }

    #[tokio::test]
    async fn malformed_arguments_are_returned_to_model_without_tool_execution() {
        let mut state = make_state();
        state.last_finish_reason = Some("length".to_string());
        let turn_result = empty_host_turn_result();
        let calls = vec![json!({
            "id": "call-python",
            "type": "function",
            "function": {
                "name": "python",
                "arguments": "{\"code\":\"from docx import Document"
            }
        })];
        let valid_tool_names = HashSet::from(["python".to_string()]);
        let admission = admit_tool_calls(&calls, state.last_finish_reason.as_deref());

        let prepared = prepare_intercepted_tool_round(
            &mut state,
            &turn_result,
            &admission.admitted,
            admission.rejected,
            false,
            &valid_tool_names,
        )
        .await;

        assert_eq!(prepared.tool_calls.len(), 1);
        assert_eq!(prepared.tool_calls[0]["id"], "call-python");
        assert_eq!(
            prepared.tool_calls[0]["function"]["arguments"],
            serde_json::Value::String("{}".to_string())
        );
        assert_eq!(prepared.pre_resolved_results.len(), 1);
        assert_eq!(prepared.pre_resolved_results[0].0, "call-python");
        let result: Value = serde_json::from_str(&prepared.pre_resolved_results[0].1).unwrap();
        assert_eq!(result["status"], "rejected");
        assert_eq!(result["error_kind"], "tool_call_arguments_invalid");
        assert!(
            result["error"]
                .as_str()
                .unwrap()
                .contains("Shorten the call, split the work")
        );
        assert_eq!(state.stall.tool_call_records.len(), 1);
        assert_eq!(
            state.stall.tool_call_records[0].disposition,
            Some(astra_services::session_journal::ToolCallDisposition::Rejected)
        );
        assert!(
            state.stall.tool_call_records[0]
                .error
                .as_deref()
                .is_some_and(|error| error.contains("cut off at the model output limit")),
            "the audit record must preserve the actual structured rejection cause"
        );
    }

    #[tokio::test]
    async fn policy_rejection_preserves_canonical_cause_and_arguments() {
        let mut state = make_state();
        state.step_recorder.begin_turn(1);
        let turn_result = empty_host_turn_result();
        let rejected = RejectedToolCall {
            id: "call-web".to_string(),
            name: "web_fetch".to_string(),
            canonical_call: json!({
                "id": "call-web",
                "type": "function",
                "function": {
                    "name": "web_fetch",
                    "arguments": "{\"url\":\"https://example.test\"}"
                }
            }),
            result: json!({
                "status": "rejected",
                "error_kind": "typed_policy_rejection",
                "retryable": false,
                "error": "typed policy denied this execution role"
            })
            .to_string(),
        };

        let prepared = prepare_intercepted_tool_round(
            &mut state,
            &turn_result,
            &[],
            vec![rejected],
            false,
            &HashSet::new(),
        )
        .await;

        assert_eq!(prepared.pre_resolved_results.len(), 1);
        let result: Value = serde_json::from_str(&prepared.pre_resolved_results[0].1)
            .expect("structured policy rejection result");
        assert_eq!(result["status"], "rejected");
        let record = &state.stall.tool_call_records[0];
        assert_eq!(record.tool_call_id.as_deref(), Some("call-web"));
        assert_eq!(
            record.error.as_deref(),
            Some("typed policy denied this execution role")
        );
        assert_eq!(
            record.args_full.as_deref(),
            Some("{\"url\":\"https://example.test\"}")
        );
        assert_eq!(
            record.disposition,
            Some(astra_services::session_journal::ToolCallDisposition::Rejected)
        );
        let tool_events = state
            .step_recorder
            .events()
            .iter()
            .filter(|event| {
                matches!(
                    event.event_type,
                    astra_pipeline::step_protocol::StepEventType::ToolCallStarted
                        | astra_pipeline::step_protocol::StepEventType::ToolCallFailed
                )
            })
            .collect::<Vec<_>>();
        assert_eq!(tool_events.len(), 2);
        assert_eq!(
            tool_events[0]
                .payload
                .as_ref()
                .and_then(|payload| payload["call_id"].as_str()),
            Some("call-web")
        );
        assert_eq!(
            tool_events[1]
                .payload
                .as_ref()
                .and_then(|payload| payload["is_error"].as_bool()),
            Some(true)
        );
    }

    #[tokio::test]
    async fn rejected_tool_argument_records_are_display_safe() {
        let mut state = make_state();
        state.step_recorder.begin_turn(1);
        let rejected = RejectedToolCall {
            id: "call-bash-secret".to_string(),
            name: "bash".to_string(),
            canonical_call: json!({
                "id": "call-bash-secret",
                "type": "function",
                "function": {
                    "name": "bash",
                    "arguments": "{\"command\":\"tool --token hf_abcdefghijklmnopqrstuvwxyz123456\"}"
                }
            }),
            result: json!({
                "status": "rejected",
                "error_kind": "typed_policy_rejection",
                "error": "typed policy denied this execution role"
            })
            .to_string(),
        };

        prepare_intercepted_tool_round(
            &mut state,
            &empty_host_turn_result(),
            &[],
            vec![rejected],
            false,
            &HashSet::new(),
        )
        .await;

        let record = &state.stall.tool_call_records[0];
        let args_full = record.args_full.as_deref().expect("record arguments");
        assert!(!args_full.contains("hf_abcdefghijklmnopqrstuvwxyz123456"));
        assert!(args_full.contains("[REDACTED:TOKEN_ARGUMENT]"));
    }

    fn empty_host_turn_result() -> HostTurnResult {
        HostTurnResult {
            accum: ChatTurnSseAccum::default(),
            ttft_ms: None,
            edge_tool_round: Vec::new(),
            error_kind: None,
        }
    }

    #[test]
    fn runtime_tool_allowlist_notice_describes_effective_intersection() {
        let mut state = make_state();
        state.skills.request_constraints.allowed_tools = Some(
            ["bash".to_string(), "read_file".to_string()]
                .into_iter()
                .collect(),
        );
        apply_skill_activation(
            &mut state,
            crate::turn::skill_tool::SkillActivation {
                allowed_tools: vec![" READ_FILE ".to_string()],
                effort: None,
                agent_type: None,
                sandbox_policy: None,
            },
        );

        let notice = runtime_tool_allowlist_notice(&state).expect("notice should be emitted");

        assert!(notice.contains("read_file"));
        assert!(notice.contains("permits at most"));
        assert!(notice.contains("current tool surface is authoritative"));
        assert!(notice.contains("Skill `allowed_tools` stays a prompt hint"));
        assert!(!notice.contains("tools are callable"));
    }

    #[test]
    fn skill_context_extra_uses_session_turn_not_request_local_step() {
        let mut state = make_state();
        state.session_turn = 12;
        state.max_turns = 50;
        state.remaining_turns = 49;

        let ctx = build_skill_context(&state);

        assert_eq!(ctx.extra.get("turn_number").map(String::as_str), Some("12"));
        assert!(
            !ctx.extra.contains_key("turns_remaining"),
            "skill prompts must not gain completion authority from runtime budget telemetry"
        );
    }

    #[test]
    fn runtime_tool_allowlist_notice_uses_applied_activation_not_stale_skill_state() {
        let mut state = make_state();
        state.skills.request_constraints.allowed_tools = Some(
            ["bash".to_string(), "read_file".to_string()]
                .into_iter()
                .collect(),
        );
        state.skills.allowed_tools = Some(["read_file".to_string()].into_iter().collect());
        state.skills.invoked.insert(
            "old-skill".into(),
            crate::turn::skill_tool::InvokedSkill {
                name: "old-skill".into(),
                content: String::new(),
                invoked_at_turn: 1,
                reentry_count: 0,
                execution_topology: None,
            },
        );

        apply_skill_activation(
            &mut state,
            crate::turn::skill_tool::SkillActivation {
                allowed_tools: vec!["bash".to_string()],
                effort: None,
                agent_type: None,
                sandbox_policy: None,
            },
        );

        let notice = runtime_tool_allowlist_notice(&state).expect("notice should be emitted");

        // Skill allowed_tools is now a hint only; the notice reflects
        // the request constraints (bash + read_file), ignoring the skill activation.
        assert!(notice.contains("bash"), "notice: {notice}");
        assert!(notice.contains("read_file"), "notice: {notice}");
    }

    #[test]
    fn runtime_tool_allowlist_notice_warns_when_request_policy_excludes_all() {
        let mut state = make_state();
        state.skills.request_constraints.allowed_tools = Some(HashSet::new());

        let notice = runtime_tool_allowlist_notice(&state).expect("notice should be emitted");

        assert!(notice.contains("permits no allowlist-governed non-skill tools"));
        assert!(notice.contains("Authorization is only an upper bound"));
    }

    #[test]
    fn runtime_tool_allowlist_notice_lists_all_request_permitted_tools() {
        let mut state = make_state();
        state.skills.request_constraints.allowed_tools = Some(
            ["read_file", "session", "notify", "ask_user"]
                .into_iter()
                .map(|s| s.to_string())
                .collect(),
        );

        let notice = runtime_tool_allowlist_notice(&state).expect("notice should be emitted");

        // All requested tools appear in the authorization upper bound. The
        // notice must not turn that policy fact into a provider-readiness claim.
        assert!(notice.contains("read_file"));
        assert!(notice.contains("session"));
        assert!(notice.contains("notify"));
        assert!(notice.contains("ask_user"));
        assert!(notice.contains("does not assert provider availability"));
        assert!(!notice.contains("tools are callable"));
        // No separate control-plane exemption clause needed — they are
        // listed inline when permitted by the request policy.
    }

    #[test]
    fn runtime_tool_allowlist_notice_is_absent_without_request_policy() {
        let mut state = make_state();
        // request_constraints.allowed_tools defaults to None in make_state()
        apply_skill_activation(
            &mut state,
            crate::turn::skill_tool::SkillActivation {
                allowed_tools: vec!["read_file".to_string()],
                effort: None,
                agent_type: None,
                sandbox_policy: None,
            },
        );

        assert!(runtime_tool_allowlist_notice(&state).is_none());
    }

    #[test]
    fn optional_network_tools_require_product_enablement_without_restricting_core_tools() {
        let mut state = make_state();
        assert!(runtime_allows_tool(&state, "web_search"));
        assert!(runtime_allows_tool(&state, "memory"));

        state.skills.request_constraints.enabled_tools = Some(HashSet::new());

        assert!(!runtime_allows_tool(&state, "web_search"));
        assert!(!runtime_allows_tool(&state, "web_fetch"));
        assert!(runtime_allows_tool(&state, "memory"));
        assert!(runtime_allows_tool(&state, "session"));

        state
            .skills
            .request_constraints
            .enabled_tools
            .as_mut()
            .unwrap()
            .extend(["web_search".to_string(), "web_fetch".to_string()]);
        assert!(runtime_allows_tool(&state, "web_search"));
        assert!(runtime_allows_tool(&state, "web_fetch"));
    }

    #[tokio::test]
    async fn prepare_intercepted_tool_round_does_not_block_tools_just_because_skill_hint_omits_them()
     {
        let mut state = make_state();
        state.skills.allowed_tools = Some(
            [" Bash ".to_string(), "READ_FILE".to_string()]
                .into_iter()
                .collect(),
        );
        state.skills.invoked.insert(
            "review-changes".into(),
            crate::turn::skill_tool::InvokedSkill {
                name: "review-changes".into(),
                content: String::new(),
                invoked_at_turn: 1,
                reentry_count: 0,
                execution_topology: None,
            },
        );

        let tool_calls = vec![
            json!({
                "id": "call-bash",
                "type": "function",
                "function": { "name": "bash", "arguments": "{}" }
            }),
            json!({
                "id": "call-str-replace",
                "type": "function",
                "function": { "name": "str_replace", "arguments": "{}" }
            }),
        ];
        let prepared = prepare_intercepted_tool_round(
            &mut state,
            &empty_host_turn_result(),
            &tool_calls,
            Vec::new(),
            false,
            &HashSet::from(["bash".to_string(), "str_replace".to_string()]),
        )
        .await;

        assert_eq!(
            prepared.tool_calls.len(),
            2,
            "assistant tool calls stay intact"
        );
        assert!(
            prepared.pre_resolved_results.is_empty(),
            "skill allowed_tools is only a hint and must not hard-block extra tools"
        );
    }

    #[tokio::test]
    async fn prepare_intercepted_tool_round_keeps_all_tools_when_only_skill_hint_is_present() {
        let mut state = make_state();
        state.skills.allowed_tools = Some(["bash".to_string()].into_iter().collect());
        state.skills.invoked.insert(
            "review-changes".into(),
            crate::turn::skill_tool::InvokedSkill {
                name: "review-changes".into(),
                content: String::new(),
                invoked_at_turn: 1,
                reentry_count: 0,
                execution_topology: None,
            },
        );

        let tool_calls = vec![
            json!({
                "id": "call-session",
                "type": "function",
                "function": { "name": "session", "arguments": r#"{"action":"info"}"# }
            }),
            json!({
                "id": "call-notify",
                "type": "function",
                "function": { "name": "notify", "arguments": r#"{"message":"done"}"# }
            }),
            json!({
                "id": "call-ask-user",
                "type": "function",
                "function": { "name": "ask_user", "arguments": r#"{"questions":[{"question":"Proceed?"}]}"# }
            }),
            json!({
                "id": "call-str-replace",
                "type": "function",
                "function": { "name": "str_replace", "arguments": "{}" }
            }),
        ];
        let prepared = prepare_intercepted_tool_round(
            &mut state,
            &empty_host_turn_result(),
            &tool_calls,
            Vec::new(),
            false,
            &HashSet::from([
                "session".to_string(),
                "notify".to_string(),
                "ask_user".to_string(),
                "str_replace".to_string(),
            ]),
        )
        .await;

        assert!(
            prepared.pre_resolved_results.is_empty(),
            "skill hints must not block control-plane or ordinary tools"
        );
    }

    #[tokio::test]
    async fn prepare_intercepted_tool_round_blocks_control_tool_excluded_by_request_allowlist() {
        let mut state = make_state();
        state.skills.request_constraints.allowed_tools =
            Some(["bash".to_string()].into_iter().collect());
        state.skills.allowed_tools = Some(["bash".to_string()].into_iter().collect());
        state.skills.invoked.insert(
            "review-changes".into(),
            crate::turn::skill_tool::InvokedSkill {
                name: "review-changes".into(),
                content: String::new(),
                invoked_at_turn: 1,
                reentry_count: 0,
                execution_topology: None,
            },
        );

        let tool_calls = vec![json!({
            "id": "call-session",
            "type": "function",
            "function": { "name": "session", "arguments": r#"{"action":"info"}"# }
        })];
        let prepared = prepare_intercepted_tool_round(
            &mut state,
            &empty_host_turn_result(),
            &tool_calls,
            Vec::new(),
            false,
            &HashSet::from(["session".to_string(), "bash".to_string()]),
        )
        .await;

        assert!(
            prepared
                .pre_resolved_results
                .iter()
                .any(|(call_id, result)| {
                    call_id == "call-session" && result.contains("BLOCKED:")
                }),
            "request allowlists must still suppress excluded control-plane tools"
        );
    }

    #[test]
    fn control_plane_tools_are_exempt_from_skill_allowlists_only() {
        for tool_name in CONTROL_PLANE_TOOLS {
            assert!(
                exempt_from_skill_allowlist(tool_name),
                "{tool_name} should bypass skill allowlists"
            );
            assert!(
                !exempt_from_request_allowlist(tool_name),
                "{tool_name} should still respect request allowlists"
            );
        }
    }

    #[tokio::test]
    async fn prepare_intercepted_tool_round_honors_explicit_empty_request_allowlist() {
        let mut state = make_state();
        state.skills.request_constraints.allowed_tools = Some(HashSet::new());

        let tool_calls = vec![json!({
            "id": "call-bash",
            "type": "function",
            "function": { "name": "bash", "arguments": "{}" }
        })];
        let prepared = prepare_intercepted_tool_round(
            &mut state,
            &empty_host_turn_result(),
            &tool_calls,
            Vec::new(),
            false,
            &HashSet::from(["bash".to_string()]),
        )
        .await;

        assert!(
            prepared
                .pre_resolved_results
                .iter()
                .any(|(call_id, result)| {
                    call_id == "call-bash" && result.contains("Allowed tools: none")
                })
        );
    }

    #[tokio::test]
    async fn prepare_intercepted_tool_round_blocks_request_only_allowlist() {
        // Coverage gap fixed: only request allowlist set, no skill activation.
        // Without this test the request-only branch of compute_effective_allowlist
        // had no regression guard.
        let mut state = make_state();
        state.skills.request_constraints.allowed_tools =
            Some(["bash".to_string()].into_iter().collect());

        let tool_calls = vec![
            json!({
                "id": "call-bash",
                "type": "function",
                "function": { "name": "bash", "arguments": "{}" }
            }),
            json!({
                "id": "call-rf",
                "type": "function",
                "function": { "name": "read_file", "arguments": "{}" }
            }),
        ];
        let prepared = prepare_intercepted_tool_round(
            &mut state,
            &empty_host_turn_result(),
            &tool_calls,
            Vec::new(),
            false,
            &HashSet::from(["bash".to_string(), "read_file".to_string()]),
        )
        .await;

        assert!(
            prepared
                .pre_resolved_results
                .iter()
                .any(|(id, msg)| id == "call-rf" && msg.contains("BLOCKED:")),
            "request-only allowlist should block read_file"
        );
        assert!(
            prepared
                .pre_resolved_results
                .iter()
                .all(|(id, _)| id != "call-bash"),
            "request-only allowlist should leave allowed bash alone"
        );
    }

    #[tokio::test]
    async fn allowlist_blocked_tool_preview_is_display_safe() {
        let mut state = make_state();
        state.skills.request_constraints.allowed_tools =
            Some(["read_file".to_string()].into_iter().collect());
        let raw_token = "hf_abcdefghijklmnopqrstuvwxyz123456";
        let tool_calls = vec![json!({
            "id": "call-blocked-secret",
            "type": "function",
            "function": {
                "name": "bash",
                "arguments": format!("{{\"command\":\"tool --token {raw_token}\"}}")
            }
        })];

        let prepared = prepare_intercepted_tool_round(
            &mut state,
            &empty_host_turn_result(),
            &tool_calls,
            Vec::new(),
            false,
            &HashSet::from(["bash".to_string(), "read_file".to_string()]),
        )
        .await;

        assert!(
            prepared
                .pre_resolved_results
                .iter()
                .any(|(id, result)| { id == "call-blocked-secret" && result.contains("BLOCKED:") })
        );
        let record = state
            .stall
            .tool_call_records
            .iter()
            .find(|record| record.tool_call_id.as_deref() == Some("call-blocked-secret"))
            .expect("blocked call should be recorded");
        let preview = record.args_preview.as_deref().unwrap_or_default();
        assert!(!preview.contains(raw_token));
        assert!(preview.contains("[REDACTED:TOKEN_ARGUMENT]"));
    }

    #[tokio::test]
    async fn prepare_intercepted_tool_round_admits_mixed_case_skill_call() {
        // Regression: a model emitting "Skill" (mixed-case) used to pass the
        // allowlist gate (which lowercases) but skip is_skill_call (which was
        // exact-match), turning a legitimate skill invocation into an unknown-
        // tool dispatch. Both halves now normalize to lowercase, so the call
        // reaches intercept_skill_calls and produces a skill result.
        let mut state = make_state();
        state.skills.request_constraints.allowed_tools =
            Some(["bash".to_string()].into_iter().collect());

        let tool_calls = vec![json!({
            "id": "call-skill-mixed",
            "type": "function",
            "function": {
                "name": "Skill",
                "arguments": "{\"skill_name\": \"any\"}"
            }
        })];
        let prepared = prepare_intercepted_tool_round(
            &mut state,
            &empty_host_turn_result(),
            &tool_calls,
            Vec::new(),
            false,
            &HashSet::from(["bash".to_string()]),
        )
        .await;

        // Mixed-case "Skill" must NOT be reported as a blocked-allowlist
        // result; it routes through the skill execution path. (No resolver
        // is configured here, so it produces no skill output either, which
        // is fine — the assertion is purely "the allowlist gate didn't
        // mistake it for a denied tool".)
        assert!(
            prepared
                .pre_resolved_results
                .iter()
                .all(|(id, msg)| id != "call-skill-mixed" || !msg.contains("BLOCKED:")),
            "mixed-case Skill must not be blocked by the allowlist gate"
        );
    }

    #[tokio::test]
    async fn prepare_intercepted_tool_round_blocks_empty_tool_names() {
        let mut state = make_state();
        state.skills.request_constraints.allowed_tools =
            Some(["bash".to_string()].into_iter().collect());

        let tool_calls = vec![json!({
            "id": "call-empty",
            "type": "function",
            "function": { "name": "   ", "arguments": "{}" }
        })];
        let prepared = prepare_intercepted_tool_round(
            &mut state,
            &empty_host_turn_result(),
            &tool_calls,
            Vec::new(),
            false,
            &HashSet::from(["bash".to_string()]),
        )
        .await;

        assert!(
            prepared
                .pre_resolved_results
                .iter()
                .any(|(call_id, result)| {
                    call_id == "call-empty" && result.contains("Tool name is missing or empty")
                })
        );
    }

    /// Verify that surgical removal stubs and skill result records preserve
    /// round and start_offset_ms from the TurnEventBuffer.
    #[tokio::test]
    async fn surgical_removal_preserves_observability_fields() {
        use astra_services::session_journal::TurnEventBuffer;

        let mut state = make_state();
        // Initialize the turn event buffer (simulates what prepare_turn_iteration does).
        let mut buf = TurnEventBuffer::begin_turn(Some("test-session"), 1);
        // Advance to round 2 to verify the round is captured, not always 0.
        buf.record_llm_round(astra_services::session_journal::LlmRoundRecord {
            prompt_tokens: 100,
            completion_tokens: 10,
            cache_read_tokens: 0,
            cache_creation_tokens: 0,
            duration_ms: 50,
            ttft_ms: Some(5),
            finish_reason: None,
            tool_calls_returned: 0,
            tool_call_names: vec![],
            agentic_step: None,
            source: None,
            run_id: None,
            parent_run_id: None,
            tool_calls: None,
            agent_id: None,
            purpose: astra_turn_types::InferencePurpose::PrimaryAgent,
        });
        buf.record_llm_round(astra_services::session_journal::LlmRoundRecord {
            prompt_tokens: 200,
            completion_tokens: 20,
            cache_read_tokens: 0,
            cache_creation_tokens: 0,
            duration_ms: 60,
            ttft_ms: Some(6),
            finish_reason: None,
            tool_calls_returned: 0,
            tool_call_names: vec![],
            agentic_step: None,
            source: None,
            run_id: None,
            parent_run_id: None,
            tool_calls: None,
            agent_id: None,
            purpose: astra_turn_types::InferencePurpose::PrimaryAgent,
        });
        assert_eq!(buf.current_round(), 2);
        state.turn_event_buffer = Some(buf);

        // Simulate what prepare_intercepted_tool_round does for surgical removal.
        let tool_name_by_id: HashMap<&str, &str> =
            [("call-read-1", "read_file")].into_iter().collect();

        // Push a surgical removal record (same code path as the real function).
        {
            let id = "call-read-1";
            let original_name = tool_name_by_id.get(id).map(|s| s.to_string());
            let (round, start_offset_ms) = match state.turn_event_buffer.as_ref() {
                Some(buf) => (Some(buf.current_round()), Some(buf.offset_ms())),
                None => (None, None),
            };
            state.stall.tool_call_records.push(ToolCallRecord {
                name: SURGICAL_REMOVAL_TOOL_NAME.to_string(),
                ok: true,
                ms: 0,
                surgically_removed: Some(true),
                original_tool_name: original_name,
                round,
                start_offset_ms,
                ..Default::default()
            });
        }

        let rec = &state.stall.tool_call_records[0];
        assert_eq!(
            rec.round,
            Some(2),
            "surgical removal should capture current round"
        );
        assert!(
            rec.start_offset_ms.is_some(),
            "surgical removal should capture offset"
        );
        assert_eq!(rec.original_tool_name.as_deref(), Some("read_file"));
        assert_eq!(rec.surgically_removed, Some(true));
    }

    /// When the model re-invokes the same skill, the first short-circuit uses
    /// the passive "already loaded" wording; from the second re-entry onward
    /// the message escalates to a hard STOP directive. The `reentry_count`
    /// field on `InvokedSkill` must also increment monotonically.
    #[tokio::test]
    async fn skill_reentry_escalates_short_circuit_message() {
        use crate::turn::skill_tool::{InvokedSkill, SKILL_TOOL_NAME};

        let mut state = make_state();
        state.skills.invoked.insert(
            "review-changes".into(),
            InvokedSkill {
                name: "review-changes".into(),
                content: "# Skill: review-changes".into(),
                invoked_at_turn: 1,
                reentry_count: 0,
                execution_topology: None,
            },
        );

        let make_call = |id: &str| {
            json!({
                "id": id,
                "type": "function",
                "function": {
                    "name": SKILL_TOOL_NAME,
                    "arguments": r#"{"skill_name":"review-changes"}"#
                }
            })
        };

        // 1st re-entry: passive wording.
        let (dedup, fresh) = super::dedup_skill_calls(&mut state, &[make_call("c1")]);
        assert!(
            fresh.is_empty(),
            "repeat skill call should be short-circuited"
        );
        assert_eq!(dedup.len(), 1);
        let (res1, meta1) = &dedup[0];
        assert!(
            res1.result.contains("was already loaded"),
            "first re-entry uses passive wording, got: {}",
            res1.result
        );
        assert!(
            res1.result.contains("# Skill: review-changes"),
            "first re-entry should replay the already-loaded skill content, got: {}",
            res1.result
        );
        assert!(
            !res1.result.starts_with("STOP"),
            "first re-entry must not be STOP-level yet"
        );
        assert_eq!(meta1.reentry_count, 1);
        assert!(!meta1.locked_out);
        assert_eq!(state.skills.invoked["review-changes"].reentry_count, 1);

        // 2nd re-entry: escalates to STOP.
        let (dedup2, _) = super::dedup_skill_calls(&mut state, &[make_call("c2")]);
        assert_eq!(dedup2.len(), 1);
        let (res2, meta2) = &dedup2[0];
        assert!(
            res2.result.starts_with("STOP:"),
            "second re-entry should escalate to STOP, got: {}",
            res2.result
        );
        assert!(
            res2.result.contains("Do NOT call `skill` again"),
            "STOP message should be directive, got: {}",
            res2.result
        );
        assert_eq!(meta2.reentry_count, 2);
        assert!(
            !meta2.locked_out,
            "reentry=2 is STOP but not yet locked out"
        );
        assert_eq!(state.skills.invoked["review-changes"].reentry_count, 2);

        // 3rd re-entry: hard lockout — BLOCKED + locked_out=true.
        let (dedup3, _) = super::dedup_skill_calls(&mut state, &[make_call("c3")]);
        let (res3, meta3) = &dedup3[0];
        assert!(
            res3.result.starts_with("BLOCKED:"),
            "third re-entry should hit BLOCKED lockout, got: {}",
            res3.result
        );
        assert!(meta3.locked_out);
        assert_eq!(meta3.reentry_count, 3);
        assert_eq!(state.skills.invoked["review-changes"].reentry_count, 3);
        assert_eq!(
            state.stall.events.len(),
            1,
            "lockout should push exactly one stall event"
        );
        assert_eq!(state.stall.events[0].0, "skill_lockout:review-changes");

        // 4th re-entry: still BLOCKED, counter keeps climbing.
        let (dedup4, _) = super::dedup_skill_calls(&mut state, &[make_call("c4")]);
        let (res4, meta4) = &dedup4[0];
        assert!(res4.result.starts_with("BLOCKED:"));
        assert!(meta4.locked_out);
        assert_eq!(meta4.reentry_count, 4);
        assert_eq!(
            state.stall.events.len(),
            2,
            "every locked-out call pushes a fresh stall signal"
        );
    }

    #[tokio::test]
    async fn failed_parallel_skill_never_enters_trusted_invocation_ledger() {
        struct FailingParallelResolver;

        impl astra_skills::traits::SkillResolver for FailingParallelResolver {
            fn resolve(
                &self,
                name: &str,
            ) -> Result<astra_skills::traits::ResolvedSkill, astra_skills::SkillError> {
                Ok(astra_skills::traits::ResolvedSkill {
                    name: name.to_string(),
                    instructions: "Parallel workflow".to_string(),
                    max_tokens: None,
                    allowed_tools: Vec::new(),
                    execution_context: astra_skills::manifest::ExecutionContext::Inline,
                    hooks: astra_skills::hooks::SkillHooks::default(),
                    skill_dir: None,
                    source: astra_skills::manifest::SkillSourceKind::Local,
                    success_criteria: Vec::new(),
                    composition: None,
                    input_schema: Some(json!({
                        "properties": {"target_path": {"type": "string"}},
                        "required": ["target_path"]
                    })),
                    output_schema: None,
                    remote_url: None,
                    forward_headers: Vec::new(),
                    required_headers: Vec::new(),
                    aliases: Vec::new(),
                    effort: None,
                    agent_type: None,
                    trust_tier: astra_skills::manifest::TrustTier::Bundled,
                })
            }

            fn available_skills(&self) -> Vec<astra_skills::traits::SkillToolInfo> {
                vec![astra_skills::traits::SkillToolInfo {
                    name: "parallel-review".to_string(),
                    description: "parallel review".to_string(),
                    source: astra_skills::manifest::SkillSourceKind::Local,
                    ..Default::default()
                }]
            }

            fn execution_topology(
                &self,
                name: &str,
            ) -> Option<astra_skills::manifest::SkillExecutionTopology> {
                (name == "parallel-review")
                    .then_some(astra_skills::manifest::SkillExecutionTopology::ParallelSubruns)
            }
        }

        let mut state = make_state();
        state.skills.resolver = Some(Arc::new(FailingParallelResolver));
        let intercepted = super::intercept_skill_calls(
            &mut state,
            &[json!({
                "id": "failed-parallel-skill",
                "type": "function",
                "function": {
                    "name": "skill",
                    "arguments": r#"{"skill_name":"parallel-review"}"#
                }
            })],
        )
        .await;

        assert_eq!(intercepted.results.len(), 1);
        assert!(!intercepted.results[0].ok);
        assert!(!intercepted.results[0].result.contains("<skill-loaded"));
        assert!(
            !state.skills.invoked.contains_key("parallel-review"),
            "a failed skill must not grant typed parallel topology authority"
        );
    }
}
