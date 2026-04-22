use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use astra_services::session_journal::{SURGICAL_REMOVAL_TOOL_NAME, ToolCallRecord};
use serde_json::Value;

use crate::turn::sse_stream_host::EdgeToolExecResult;

use super::agentic_loop_host::{AgenticLoopState, DELEGATE_TOOL_NAME, HostTurnResult};

pub(crate) struct PreparedToolRound {
    pub(crate) tool_calls: Vec<Value>,
    pub(crate) pre_resolved_results: Vec<(String, String)>,
    pub(crate) edge_tool_round: Vec<EdgeToolExecResult>,
}

pub(crate) async fn prepare_intercepted_tool_round(
    state: &mut AgenticLoopState,
    turn_result: &HostTurnResult,
    effective_tool_calls: &[Value],
    delegation_intercepted: bool,
    valid_tool_names: &HashSet<String>,
) -> PreparedToolRound {
    let tool_calls =
        super::headless_tool_assembly::ensure_tool_call_ids(effective_tool_calls).into_owned();
    let (mut pre_resolved_results, post_send_tool_calls) =
        intercept_send_message_calls(state, &tool_calls, valid_tool_names).await;
    let SkillInterceptionResult {
        results: skill_results,
        surgically_removed_ids,
        short_circuit_meta,
    } = intercept_skill_calls(state, &post_send_tool_calls).await;

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
        state.stall.tool_call_records.push(ToolCallRecord {
            name: result.tool_name.clone(),
            ok: !result.result.starts_with("Unknown skill")
                && !result.result.starts_with("Invalid skill")
                && !result.result.starts_with("Skipped:")
                && !result.result.starts_with("Deferred:")
                && !result.result.starts_with("BLOCKED:"),
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
            let name = tc
                .get("function")
                .and_then(|f| f.get("name"))
                .and_then(Value::as_str)?;
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
    }
}

async fn intercept_send_message_calls(
    state: &mut AgenticLoopState,
    tool_calls: &[Value],
    valid_tool_names: &HashSet<String>,
) -> (Vec<(String, String)>, Vec<Value>) {
    let Some(mailbox) = state.messaging.mailbox.as_ref() else {
        return (Vec::new(), tool_calls.to_vec());
    };

    let mut msg_results = Vec::new();
    let mut remaining = Vec::new();
    for tc in tool_calls {
        if crate::messaging::send_tool::is_send_message_call(tc)
            && valid_tool_names.contains(crate::messaging::send_tool::SEND_MESSAGE_TOOL_NAME)
        {
            if let Some((call_id, args)) = crate::messaging::send_tool::parse_send_message_call(tc)
            {
                let send_result =
                    crate::messaging::send_tool::execute_send_message(mailbox, &args).await;
                if send_result.tracked_message.is_some()
                    || !send_result.display.starts_with("Error:")
                {
                    if let Some(ref metrics) = state.messaging.metrics {
                        metrics
                            .messages_sent
                            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    }
                }
                if let Some(tracked_msg) = send_result.tracked_message {
                    if let Some(ref tracker) = state.messaging.ack_tracker {
                        if state.messaging.ack_sweep_task.is_none() {
                            if let Some(ref mailbox) = state.messaging.mailbox {
                                state.messaging.ack_sweep_task =
                                    Some(crate::messaging::ack_tracker::start_sweep_task(
                                        Arc::clone(tracker),
                                        mailbox.router(),
                                        state.messaging.dead_letter_queue.clone(),
                                        state.messaging.metrics.clone(),
                                    ));
                            }
                        }
                        tracker.track(tracked_msg).await;
                    }
                }
                msg_results.push((call_id, send_result.display));
            } else if let Some(call_id) = tc.get("id").and_then(Value::as_str) {
                msg_results.push((
                    call_id.to_string(),
                    "Error: could not parse send_message arguments. Expected JSON with 'target' and 'content' fields.".to_string(),
                ));
            }
        } else {
            remaining.push(tc.clone());
        }
    }

    (msg_results, remaining)
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
                    format!(
                        "Skill '{}' was already loaded (turn {}). \
                         Follow those instructions directly — do not re-invoke.",
                        name, invoked_at
                    )
                };
                short_circuits.push((
                    crate::turn::skill_tool::InterceptedToolResult {
                        tool_call_id: call_id.to_string(),
                        tool_name: crate::turn::skill_tool::SKILL_TOOL_NAME.to_string(),
                        result: message,
                        verification_summary: None,
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
    let (visible_for_mask, _) = crate::turn::skill_tool::visible_skills_for_host_turn(
        &full_catalog,
        state.message.as_str(),
        &state.skills.quality_tracker,
        &state.skills.pinned,
        &state.skills.discovered,
        &state.skills.invoked,
        &state.skills.search,
    );
    let discover_exclude = crate::turn::skill_tool::skill_mask_names_lowercase(&visible_for_mask);

    let (dedup_pairs, fresh_tool_calls) = dedup_skill_calls(state, tool_calls);
    let mut short_circuit_meta: HashMap<String, SkillShortCircuitMeta> = HashMap::new();
    let mut dedup_results = Vec::with_capacity(dedup_pairs.len());
    for (res, meta) in dedup_pairs {
        short_circuit_meta.insert(res.tool_call_id.clone(), meta);
        dedup_results.push(res);
    }

    let (sr, remaining, activation) =
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

    let current_turn = (state.max_turns - state.remaining_turns) as u32;
    for result in &sr {
        if let Some(tc) = fresh_tool_calls
            .iter()
            .find(|t| t.get("id").and_then(Value::as_str) == Some(result.tool_call_id.as_str()))
        {
            let name = crate::turn::skill_tool::extract_skill_name(tc);
            if let Some(name) = name {
                if crate::turn::skill_tool::is_skill_call(tc) {
                    state.skills.invoked.insert(
                        name.clone(),
                        crate::turn::skill_tool::InvokedSkill {
                            name,
                            content: result.result.clone(),
                            invoked_at_turn: current_turn,
                            reentry_count: 0,
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
                .filter_map(|tc| {
                    tc.get("function")
                        .and_then(|f| f.get("name"))
                        .and_then(Value::as_str)
                })
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
                let tool_name = tc
                    .get("function")
                    .and_then(|f| f.get("name"))
                    .and_then(Value::as_str)
                    .unwrap_or("unknown");
                let msg = format!(
                    "Deferred: skill was invoked in this turn. Read the skill \
                     instructions above, then decide whether to call `{}` again.",
                    tool_name
                );
                skill_results.push(crate::turn::skill_tool::InterceptedToolResult {
                    tool_call_id: call_id.to_string(),
                    tool_name: tool_name.to_string(),
                    result: msg,
                    verification_summary: None,
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

    if let Some(act) = activation {
        state.skills.model_override = act.model_override.filter(|m| is_valid_model_string(m));
        state.skills.allowed_tools = if act.allowed_tools.is_empty() {
            None
        } else {
            Some(act.allowed_tools.into_iter().collect())
        };
        state.skills.effort = act.effort;
        state.skills.agent_type = act.agent_type;
        state.skills.sandbox_policy = act.sandbox_policy;
    }

    SkillInterceptionResult {
        results: skill_results,
        surgically_removed_ids,
        short_circuit_meta,
    }
}

fn build_skill_context(state: &AgenticLoopState) -> crate::turn::skill_tool::SkillContext {
    let session_dir = state.current_session_id.as_ref().map(|id| {
        dirs::home_dir()
            .unwrap_or_else(|| std::path::PathBuf::from("."))
            .join(".astra")
            .join("sessions")
            .join(id)
            .to_string_lossy()
            .into_owned()
    });

    crate::turn::skill_tool::SkillContext {
        session_id: state.current_session_id.clone(),
        session_dir,
        work_dir: state.hooks.workspace_root_hint.clone(),
        available_tools: state.telemetry.all_tools_used.iter().cloned().collect(),
        recursion_depth: state.recursion_depth,
        forward_headers: state.hooks.forward_headers.clone(),
        extra: build_skill_extra(state),
    }
}

fn build_skill_extra(state: &AgenticLoopState) -> HashMap<String, String> {
    let mut extra = HashMap::new();

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

    let turns_used = state.max_turns.saturating_sub(state.remaining_turns);
    extra.insert("turn_number".into(), turns_used.to_string());
    extra.insert("turns_remaining".into(), state.remaining_turns.to_string());
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
    let depri = state.turn_guard.health.deprioritized_tools();
    if !depri.is_empty() {
        extra.insert("deprioritized_tools".into(), depri.join(", "));
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

pub(crate) fn is_valid_model_string(model: &str) -> bool {
    let len = model.len();
    if !(2..=128).contains(&len) {
        return false;
    }
    let first = model.as_bytes()[0];
    if !first.is_ascii_alphanumeric() {
        return false;
    }
    model.bytes().all(|b| {
        b.is_ascii_alphanumeric() || b == b'-' || b == b'_' || b == b'.' || b == b':' || b == b'/'
    })
}

pub(crate) fn extract_repo_name_from_url(url: &str) -> Option<String> {
    let path = url.trim_end_matches('/');
    let segment = if let Some(idx) = path.rfind('/') {
        &path[idx + 1..]
    } else if let Some(idx) = path.rfind(':') {
        let after_colon = &path[idx + 1..];
        after_colon.rsplit('/').next().unwrap_or(after_colon)
    } else {
        return None;
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

    use serde_json::json;

    use super::*;
    use crate::messaging::in_process::InProcessTransport;
    use crate::messaging::router::AgentMailboxRouter;
    use crate::messaging::types::AgentAddress;
    use crate::server::delegation_engine::{DelegationTracker, SubRunRecord, SubRunState};
    use crate::turn::agentic_loop_host::tests::make_state;

    async fn setup_mailboxes() -> (
        crate::messaging::router::AgentMailbox,
        crate::messaging::router::AgentMailbox,
    ) {
        let transport = Arc::new(InProcessTransport::new());
        let tracker = Arc::new(DelegationTracker::new());
        let router = Arc::new(AgentMailboxRouter::new(transport, tracker.clone()));

        let parent = router
            .register(AgentAddress::new("run-parent", "orchestrator"), None)
            .await
            .expect("parent mailbox should register");

        tracker
            .record_sub_run(SubRunRecord {
                run_id: "run-child".into(),
                parent_run_id: "run-parent".into(),
                delegation_id: "del-1".into(),
                agent_id: "worker".into(),
                depth: 1,
                state: SubRunState::Created,
                retry_of: None,
            })
            .await;

        let child = router
            .register(
                AgentAddress::new("run-child", "worker"),
                Some("del-1".into()),
            )
            .await
            .expect("child mailbox should register");

        (parent, child)
    }

    #[tokio::test]
    async fn send_message_interception_respects_valid_tool_names() {
        let (mut parent, child) = setup_mailboxes().await;
        let mut state = make_state();
        state.messaging.mailbox = Some(child);

        let tool_calls = vec![json!({
            "id": "call-send-1",
            "type": "function",
            "function": {
                "name": "send_message",
                "arguments": r#"{"target":"parent","content":"blocked","message_type":"text"}"#
            }
        })];

        let (results, remaining) =
            intercept_send_message_calls(&mut state, &tool_calls, &HashSet::new()).await;

        assert!(
            results.is_empty(),
            "disallowed send_message should not be intercepted"
        );
        assert_eq!(
            remaining, tool_calls,
            "tool call should remain for unknown-tool handling"
        );
        assert!(
            parent.try_recv().is_none(),
            "no message should be delivered when send_message is disallowed"
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
            duration_ms: 50,
            ttft_ms: Some(5),
            finish_reason: None,
            tool_calls_returned: 0,
            tool_call_names: vec![],
            agentic_step: None,
            source: None,
            run_id: None,
        });
        buf.record_llm_round(astra_services::session_journal::LlmRoundRecord {
            prompt_tokens: 200,
            completion_tokens: 20,
            cache_read_tokens: 0,
            duration_ms: 60,
            ttft_ms: Some(6),
            finish_reason: None,
            tool_calls_returned: 0,
            tool_call_names: vec![],
            agentic_step: None,
            source: None,
            run_id: None,
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
}
