//! After one SSE turn with tool work: assistant + `tool_calls` message, then edge-only tool results.

use std::collections::{HashMap, HashSet};
use std::time::Instant;

use crossterm::style::Stylize;
use mo_agent_core::agent_warn;
use mo_agent_runtime::{
    pipeline::step_protocol::{CachedToolResult, IdempotencyKey, InMemoryIdempotencyCache},
    pipeline::step_recorder::StepRecorder,
    semantic_dedup::SemanticDedup,
    turn::edge_prompt_context::make_args_preview,
    turn::headless_tool_assembly::{
        openai_assistant_with_tool_calls_message, openai_tool_roundtrip_values,
        take_edge_output_for_tool_call, CACHEABLE_TOOLS,
    },
    turn::tool_result_semantics::{is_resource_limit_output, is_tool_error, tool_dedup_signature},
};

use crate::cli_utils::{tool_call_detail, tool_result_summary};
use crate::stream_render::TurnResult;

use super::super::hydrate_reflect::hydrate_reflect_placeholder_if_needed;

pub(crate) struct HeadlessToolRoundRequest<'a> {
    pub turn_index: usize,
    pub quiet: bool,
    pub api: &'a mo_thin_client::ThinClient,
    pub token: &'a str,
    pub current_session_id: Option<&'a String>,
    pub turn_result: &'a TurnResult,
    pub messages: &'a mut Vec<serde_json::Value>,
    pub tool_results: &'a mut Vec<serde_json::Value>,
    pub valid_tool_names: &'a HashSet<String>,
    pub restricted_tools: &'a mut HashSet<String>,
    pub turn_guard: &'a mut mo_agent_runtime::turn::turn_guard::TurnGuard,
    pub step_recorder: &'a mut StepRecorder,
    pub idempotency_cache: &'a mut InMemoryIdempotencyCache,
    pub semantic_dedup: &'a mut SemanticDedup,
    pub tool_call_records: &'a mut Vec<mo_agent_services::session_journal::ToolCallRecord>,
}

enum RoundToolItem {
    ServerTc(usize),
    Synthetic(usize),
}

/// Clears `tool_results`, appends the assistant tool-call message, then fills `tool_results` and
/// matching `tool` OpenAI messages for the next `/chat` request.
pub(crate) async fn run_headless_tool_round(ctx: HeadlessToolRoundRequest<'_>) {
    let HeadlessToolRoundRequest {
        turn_index,
        quiet,
        api,
        token,
        current_session_id,
        turn_result,
        messages,
        tool_results,
        valid_tool_names,
        restricted_tools,
        turn_guard,
        step_recorder,
        idempotency_cache,
        semantic_dedup,
        tool_call_records,
    } = ctx;

    tool_results.clear();

    let assistant_tc_msg = openai_assistant_with_tool_calls_message(
        &turn_result.tool_calls,
        &turn_result.edge_tool_round,
        &turn_result.reasoning_content,
    );
    messages.push(assistant_tc_msg);

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
            current_session_id,
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
            use mo_agent_runtime::turn::error_recovery::{build_recovery_message, classify_error};
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
            let recovery_msg = build_recovery_message(&name, &result_str, category, &deprioritized);
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

        if let Some(sid) = current_session_id
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
                semantic_dedup.check_and_record(&name, &args, &result_str, turn_index)
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
}
