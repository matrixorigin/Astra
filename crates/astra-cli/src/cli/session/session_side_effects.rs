use crate::cli::session::session_state::SessionState;
use crate::cli::stream::streaming_types::StreamResult;
use astra_services::session_journal;
use std::collections::BTreeSet;
use std::time::Instant;

/// Cloud journal ingestion is server-owned; CLI keeps the local journal path.
fn enqueue_ingestion(_state: &SessionState, event: &session_journal::JournalEvent) {
    enqueue_ingestion_batch(_state, std::slice::from_ref(event));
}

/// Notify the asynchronous journal→outbox projector about every source session
/// represented by an appended journal batch. The local journal is canonical;
/// the projector owns durable outbox batching and crash recovery by source
/// watermark, so this turn-local call never waits for its lock or fsync.
fn enqueue_ingestion_batch(_state: &SessionState, events: &[session_journal::JournalEvent]) {
    enqueue_ingestion_events(events);
}

/// Schedule journal-to-outbox projection for records written by a deferred
/// local sidecar. The journal is already durable at this point; this is only a
/// latency hint for the independently recoverable projector.
pub(crate) fn enqueue_ingestion_events(events: &[session_journal::JournalEvent]) {
    let mut source_sessions = BTreeSet::new();
    for event in events {
        if event
            .session_id
            .as_deref()
            .is_none_or(|session_id| session_id.trim().is_empty())
        {
            tracing::warn!(
                target: "astra_cli::cloud_sync",
                event_type = ?event.event_type,
                "skipping journal-to-outbox projection hint for event without a session_id"
            );
            continue;
        }
        source_sessions.insert(event.session_id.as_deref().unwrap_or_default().to_string());
    }
    let scheduled = !source_sessions.is_empty()
        && source_sessions.iter().all(|session_id| {
            crate::cli::cloud_sync::schedule_sync_outbox_journal_ingestion(session_id).accepted()
        });
    if !scheduled {
        // Non-interactive one-shot/tests can append journals without a Tokio
        // runtime. They still need a correct durable outbox boundary; this
        // fallback is outside the live TUI completion path.
        enqueue_ingestion_batch_without_runtime(events);
    }
}

fn enqueue_ingestion_batch_without_runtime(events: &[session_journal::JournalEvent]) {
    if events.is_empty() {
        return;
    }
    let store = astra_services::SyncOutboxStore::local();
    let mut deliverable = Vec::with_capacity(events.len());
    for event in events {
        if event
            .session_id
            .as_deref()
            .is_none_or(|session_id| session_id.trim().is_empty())
        {
            if let Err(error) = store.record_skipped_journal_event(
                event,
                astra_services::SyncOutboxSkipKind::MissingSessionId,
                "journal event has no session_id and cannot be delivered to /events",
            ) {
                tracing::warn!(
                    target: "astra_cli::cloud_sync",
                    ?error,
                    event_type = ?event.event_type,
                    "failed to record skipped sync outbox event"
                );
            }
            continue;
        }
        deliverable.push(event.clone());
    }
    if deliverable.is_empty() {
        return;
    }
    match store.enqueue_journal_events(&deliverable) {
        Ok(_) => {}
        Err(error) => tracing::warn!(
            target: "astra_cli::cloud_sync",
            ?error,
            event_count = deliverable.len(),
            "failed to enqueue journal events into durable sync outbox"
        ),
    }
}

pub(crate) fn enqueue_ingestion_pub(state: &SessionState, event: &session_journal::JournalEvent) {
    enqueue_ingestion(state, event);
}

pub(crate) fn enqueue_ingestion_batch_pub(
    state: &SessionState,
    events: &[session_journal::JournalEvent],
) {
    enqueue_ingestion_batch(state, events);
}

pub(crate) fn enqueue_ingestion_for_immediate_drain_pub(
    _state: &SessionState,
    event: &session_journal::JournalEvent,
) {
    enqueue_ingestion_batch_without_runtime(std::slice::from_ref(event));
}

#[derive(Clone)]
struct JournalPromptTurn {
    model_id: String,
    snapshot: astra_turn_core::cache_diagnostics::PromptStateSnapshot,
    usage: astra_runtime::turn::token_usage::TokenUsage,
}

pub(crate) fn build_bridge_pipeline_journal_events(
    session_id: Option<&str>,
    turn: u32,
    model_id: &str,
    current_turn_events: &[session_journal::JournalEvent],
) -> Result<Vec<session_journal::JournalEvent>, String> {
    let Some(session_id) = session_id.filter(|sid| !sid.is_empty()) else {
        return Ok(Vec::new());
    };
    let mut events = match session_journal::read_journal(session_id) {
        Ok(events) => events,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(error) => {
            return Err(format!(
                "failed to read session journal for {session_id}: {error}"
            ));
        }
    };
    events.extend_from_slice(current_turn_events);
    if events.iter().any(|event| {
        event.turn == Some(turn)
            && matches!(
                event.event_type,
                session_journal::JournalEventType::PipelineFeedback
                    | session_journal::JournalEventType::PipelineAlert
            )
    }) {
        return Ok(Vec::new());
    }

    let turns = journal_prompt_turns(&events);
    let Some(current) = turns.last() else {
        return Ok(Vec::new());
    };

    let mut feedback = astra_turn_core::context_feedback::ContextFeedback::from_usage(
        current.usage.input_tokens,
        current.usage.cached_input_tokens,
        current.usage.cache_creation_tokens,
        current.usage.output_tokens,
        false,
    );

    if let Some(previous) = turns.iter().rev().nth(1) {
        let mut detector = astra_turn_core::cache_diagnostics::CacheBreakDetector::new();
        let _ = detector.record_turn_for_source(
            "bridge_inprocess",
            previous.snapshot.clone(),
            Some(previous.usage.cached_input_tokens),
        );
        if let Some(event) = detector.record_turn_for_source(
            "bridge_inprocess",
            current.snapshot.clone(),
            Some(current.usage.cached_input_tokens),
        ) {
            feedback.attribute_cache_break(event.reason);
        }
    }

    let prior_feedback_ratios: Vec<f64> = events
        .iter()
        .filter(|event| {
            event.turn.unwrap_or(0) < turn
                && event.event_type == session_journal::JournalEventType::PipelineFeedback
        })
        .filter_map(|event| {
            let metadata = event.metadata.as_ref()?;
            (metadata.get("model_id").and_then(serde_json::Value::as_str) == Some(model_id))
                .then(|| {
                    metadata
                        .get("cache_hit_ratio")
                        .and_then(serde_json::Value::as_f64)
                })
                .flatten()
        })
        .collect();
    let prior_ratios = if prior_feedback_ratios.is_empty() {
        turns
            .iter()
            .take(turns.len().saturating_sub(1))
            .filter(|turn| turn.model_id == model_id)
            .filter_map(|turn| {
                let total_input = turn
                    .usage
                    .input_tokens
                    .saturating_add(turn.usage.cached_input_tokens)
                    .saturating_add(turn.usage.cache_creation_tokens);
                (total_input > 0)
                    .then_some(turn.usage.cached_input_tokens as f64 / total_input as f64)
            })
            .collect::<Vec<_>>()
    } else {
        prior_feedback_ratios
    };
    let avg_cache_hit_ratio = if prior_ratios.is_empty() {
        0.0
    } else {
        prior_ratios.iter().sum::<f64>() / prior_ratios.len() as f64
    };
    let mut stats = astra_turn_core::pipeline_stats::PipelineStats {
        turns_executed: prior_ratios.len() as u32,
        avg_cache_hit_ratio,
        ..Default::default()
    };
    for ratio in &prior_ratios {
        stats.record_cache_read_share_observation(model_id, "bridge_inprocess", *ratio);
    }
    stats.record_cache_read_share_observation(
        model_id,
        "bridge_inprocess",
        feedback.cache_hit_ratio,
    );

    let feedback_evt = astra_turn_core::pipeline_journal::PipelineJournalEvent::from_feedback(
        turn, model_id, &feedback,
    );
    let mut journal_events = Vec::new();
    if let Ok(payload) = serde_json::to_value(&feedback_evt) {
        journal_events.push(session_journal::JournalEvent::pipeline_feedback(
            Some(session_id),
            turn,
            payload,
        ));
    }

    for alert in astra_turn_core::trace_alert::evaluate_alerts(
        turn,
        model_id,
        "bridge_inprocess",
        &feedback,
        &stats,
        &astra_turn_core::recovery_state::RecoveryState::default(),
    ) {
        let alert_evt = astra_turn_core::pipeline_journal::PipelineJournalEvent::from_alert(&alert);
        if let Ok(payload) = serde_json::to_value(&alert_evt) {
            journal_events.push(session_journal::JournalEvent::pipeline_alert(
                Some(session_id),
                turn,
                payload,
            ));
        }
    }
    Ok(journal_events)
}

pub(crate) fn append_one_shot_journal_events(
    session_id: Option<&str>,
    model_id: Option<&str>,
    line: &str,
    result: &StreamResult,
    turn_start: Instant,
) -> Result<Option<u32>, String> {
    let Some(session_id) = session_id.filter(|sid| !sid.is_empty()) else {
        return Ok(None);
    };
    let existing_events = match session_journal::read_journal(session_id) {
        Ok(events) => events,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(error) => {
            return Err(format!(
                "failed to read session journal for {session_id}: {error}"
            ));
        }
    };

    // A completed user turn is the primary durable fact. LLM request/response
    // snapshots are optional diagnostics, so their absence must not suppress
    // turn persistence. A user turn can also span several model rounds, making
    // the number of model requests the wrong sequence source.
    let turn = existing_events
        .iter()
        .filter(|event| {
            matches!(
                event.event_type,
                session_journal::JournalEventType::Turn
                    | session_journal::JournalEventType::TurnError
            )
        })
        .filter_map(|event| event.turn)
        .max()
        .unwrap_or(0)
        .saturating_add(1);
    if existing_events.iter().any(|event| {
        event.turn == Some(turn) && event.event_type == session_journal::JournalEventType::Turn
    }) {
        return Ok(Some(turn));
    }
    let writer = session_journal::JournalWriter::new(session_id)
        .map_err(|error| format!("failed to create session journal for {session_id}: {error}"))?;

    let mut append_events = build_bridge_pipeline_journal_events(
        Some(session_id),
        turn,
        model_id.unwrap_or("unknown"),
        &result.turn_observability_events,
    )?;
    // The stream keeps context assembly as a deferred sidecar so it is only
    // made durable alongside a settled turn. The interactive commit path does
    // this already; one-shot chat must preserve the same evidence or a later
    // `self trace` / resume inspection incorrectly reports that no context was
    // assembled.
    if let Some((_, trace_json)) = &result.pending_context_assembly_trace {
        append_events.push(session_journal::JournalEvent::context_assembly_recorded(
            Some(session_id),
            turn,
            trace_json.clone(),
        ));
    }
    append_events.push(
        session_journal::JournalEvent::turn(
            Some(session_id),
            turn,
            model_id,
            line,
            &result.full_text,
            result.tool_calls_count,
            result.prompt_tokens,
            result.completion_tokens,
            turn_start.elapsed().as_millis() as u64,
        )
        .with_tool_surface(
            result.visible_tools.clone(),
            result.selected_skills.clone(),
            result.tools_used.clone(),
            result.budget_used,
        )
        .with_tool_calls(result.tool_call_records.clone())
        .with_run_id(result.run_id.as_deref())
        .with_budget_pressure(result.budget_pressure)
        .with_cache_tokens(result.cache_read_tokens, result.cache_creation_tokens),
    );
    if let Err(error) = writer.append_bulk(&append_events) {
        return Err(format!(
            "failed to append one-shot journal events for {session_id}: {error}"
        ));
    }
    Ok(Some(turn))
}

fn journal_prompt_turns(events: &[session_journal::JournalEvent]) -> Vec<JournalPromptTurn> {
    let mut pending_request: Option<(
        String,
        Vec<serde_json::Value>,
        Vec<serde_json::Value>,
        String,
    )> = None;
    let mut turns = Vec::new();

    for event in events {
        match event.event_type {
            session_journal::JournalEventType::LlmRequestFull => {
                let Some(metadata) = event.metadata.as_ref() else {
                    continue;
                };
                let Some(request) = metadata
                    .get("request")
                    .and_then(serde_json::Value::as_object)
                else {
                    continue;
                };
                let messages = request
                    .get("messages")
                    .and_then(serde_json::Value::as_array)
                    .cloned()
                    .unwrap_or_default();
                let tools = request
                    .get("tools")
                    .and_then(serde_json::Value::as_array)
                    .cloned()
                    .unwrap_or_default();
                let model = metadata
                    .get("model")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("unknown")
                    .to_string();
                let provider = metadata
                    .get("provider")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("unknown")
                    .to_string();
                pending_request = Some((model, messages, tools, provider));
            }
            session_journal::JournalEventType::LlmResponseFull => {
                let Some((model, messages, tools, provider)) = pending_request.take() else {
                    continue;
                };
                let Some(usage) = journal_usage_from_response_event(event) else {
                    continue;
                };
                let total_input = usage
                    .input_tokens
                    .saturating_add(usage.cached_input_tokens)
                    .saturating_add(usage.cache_creation_tokens);
                let Some(mut snapshot) = journal_prompt_snapshot_from_messages(
                    &messages,
                    &tools,
                    &model,
                    &provider,
                    total_input,
                ) else {
                    continue;
                };
                snapshot.provider = provider;
                turns.push(JournalPromptTurn {
                    model_id: model,
                    snapshot,
                    usage,
                });
            }
            _ => {}
        }
    }
    turns
}

fn journal_usage_from_response_event(
    event: &session_journal::JournalEvent,
) -> Option<astra_runtime::turn::token_usage::TokenUsage> {
    let usage = event
        .metadata
        .as_ref()
        .and_then(|meta| meta.get("response"))
        .and_then(|response| response.get("response"))
        .and_then(|response| response.get("usage"))
        .and_then(serde_json::Value::as_object)?;
    let canonical = astra_runtime::turn::token_usage::TokenUsage::from_partial_json_map(usage);
    if !canonical.is_empty() {
        return Some(canonical);
    }
    let provider = event
        .metadata
        .as_ref()
        .and_then(|meta| meta.get("provider"))
        .and_then(serde_json::Value::as_str)
        .unwrap_or("openai");
    astra_runtime::turn::token_usage::extract_usage(
        astra_runtime::turn::token_usage::UsageDialect::for_provider(provider),
        usage,
    )
}

fn journal_prompt_snapshot_from_messages(
    messages: &[serde_json::Value],
    tools: &[serde_json::Value],
    model: &str,
    provider: &str,
    cache_eligible_tokens: u64,
) -> Option<astra_turn_core::cache_diagnostics::PromptStateSnapshot> {
    astra_turn_core::cache_diagnostics::prompt_snapshot_from_messages(
        messages,
        tools,
        provider,
        model,
        usize::try_from(cache_eligible_tokens).unwrap_or(usize::MAX),
    )
}

#[cfg(test)]
mod tests {
    use super::{append_one_shot_journal_events, build_bridge_pipeline_journal_events};
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;
    use std::time::Instant;

    use astra_services::session_journal;

    #[test]
    fn build_bridge_pipeline_journal_events_surfaces_unreadable_journal() {
        let (_tmp, _guard) = crate::tests::isolated_sessions_dir();
        let sid = format!("bridge-pipeline-unreadable-{}", uuid::Uuid::new_v4());
        std::fs::create_dir_all(session_journal::journal_file_path(&sid)).unwrap();

        let error = build_bridge_pipeline_journal_events(Some(&sid), 1, "test-model", &[])
            .expect_err("directory journal path should surface an error");

        assert!(error.contains("failed to read session journal"), "{error}");
    }

    #[test]
    fn append_one_shot_journal_events_surfaces_unreadable_journal() {
        let (_tmp, _guard) = crate::tests::isolated_sessions_dir();
        let sid = format!("one-shot-unreadable-{}", uuid::Uuid::new_v4());
        std::fs::create_dir_all(session_journal::journal_file_path(&sid)).unwrap();

        let error = append_one_shot_journal_events(
            Some(&sid),
            Some("test-model"),
            "continue",
            &crate::tests::stub_stream_result("answer"),
            Instant::now(),
        )
        .expect_err("directory journal path should surface an error");

        assert!(error.contains("failed to read session journal"), "{error}");
    }

    #[test]
    fn append_one_shot_journal_events_persists_turns_without_full_llm_observability() {
        let (_tmp, _guard) = crate::tests::isolated_sessions_dir();
        let sid = format!("one-shot-primary-turn-{}", uuid::Uuid::new_v4());
        let writer = session_journal::JournalWriter::new(&sid).unwrap();
        writer
            .append(&session_journal::JournalEvent::session_start(
                Some(&sid),
                Some("test-model"),
            ))
            .unwrap();

        let mut first = crate::tests::stub_stream_result_with_records(
            "first answer",
            vec![session_journal::ToolCallRecord {
                name: "read_file".to_string(),
                ok: true,
                ms: 4,
                ..Default::default()
            }],
        );
        first.prompt_tokens = 11;
        first.completion_tokens = 7;
        first.pending_context_assembly_trace = Some((
            41,
            serde_json::json!({
                "turn_id": "runtime-round-41",
                "token_budget": {"max_tokens": 128000, "total_used": 2048},
                "tools": {"visible_tools": [{"tool_name": "read_file"}]}
            }),
        ));
        append_one_shot_journal_events(
            Some(&sid),
            Some("test-model"),
            "first question",
            &first,
            Instant::now(),
        )
        .unwrap();

        let second = crate::tests::stub_stream_result("second answer");
        append_one_shot_journal_events(
            Some(&sid),
            Some("test-model"),
            "second question",
            &second,
            Instant::now(),
        )
        .unwrap();

        let events = session_journal::read_journal(&sid).unwrap();
        assert!(events.iter().all(|event| {
            !matches!(
                event.event_type,
                session_journal::JournalEventType::LlmRequestFull
                    | session_journal::JournalEventType::LlmResponseFull
            )
        }));
        let turns: Vec<_> = events
            .iter()
            .filter(|event| event.event_type == session_journal::JournalEventType::Turn)
            .collect();
        assert_eq!(turns.len(), 2);
        assert_eq!(turns[0].turn, Some(1));
        assert_eq!(turns[0].user_input.as_deref(), Some("first question"));
        assert_eq!(turns[0].assistant_output.as_deref(), Some("first answer"));
        assert_eq!(turns[0].tokens_in, Some(11));
        assert_eq!(turns[0].tokens_out, Some(7));
        assert_eq!(
            turns[0]
                .tool_calls
                .as_ref()
                .map(|calls| calls.iter().filter(|call| call.was_executed()).count()),
            Some(1)
        );
        assert_eq!(turns[1].turn, Some(2));
        assert_eq!(turns[1].user_input.as_deref(), Some("second question"));
        assert_eq!(turns[1].assistant_output.as_deref(), Some("second answer"));
        let traces: Vec<_> = events
            .iter()
            .filter(|event| {
                event.event_type == session_journal::JournalEventType::ContextAssemblyRecorded
            })
            .collect();
        assert_eq!(traces.len(), 1);
        assert_eq!(traces[0].turn, Some(1));
        assert_eq!(
            traces[0]
                .context_assembly_trace
                .as_ref()
                .and_then(|trace| trace.get("turn_id"))
                .and_then(serde_json::Value::as_str),
            Some("runtime-round-41")
        );

        // Tool events may carry an internal agentic-round number. They must
        // not advance the externally visible user-turn sequence.
        let mut tool_event = session_journal::JournalEvent::base_public(
            session_journal::JournalEventType::ToolCallError,
            Some(&sid),
        );
        tool_event.turn = Some(99);
        session_journal::JournalWriter::new(&sid)
            .unwrap()
            .append(&tool_event)
            .unwrap();
        append_one_shot_journal_events(
            Some(&sid),
            Some("test-model"),
            "third question",
            &crate::tests::stub_stream_result("third answer"),
            Instant::now(),
        )
        .unwrap();

        let events = session_journal::read_journal(&sid).unwrap();
        let turns: Vec<_> = events
            .iter()
            .filter(|event| event.event_type == session_journal::JournalEventType::Turn)
            .collect();
        assert_eq!(turns.len(), 3);
        assert_eq!(turns[2].turn, Some(3));
    }

    #[cfg(unix)]
    #[test]
    fn append_one_shot_journal_events_surfaces_append_failure() {
        let (_tmp, _guard) = crate::tests::isolated_sessions_dir();
        let sid = format!("one-shot-append-fail-{}", uuid::Uuid::new_v4());
        let writer = session_journal::JournalWriter::new(&sid).unwrap();
        writer
            .append(&session_journal::JournalEvent::llm_request_full(
                Some(&sid),
                1,
                0,
                serde_json::json!({
                    "request": {
                        "messages": [{"role": "user", "content": "hi"}],
                        "tools": []
                    },
                    "model": "test-model",
                    "provider": "openai"
                }),
            ))
            .unwrap();
        writer
            .append(&session_journal::JournalEvent::llm_response_full(
                Some(&sid),
                1,
                0,
                serde_json::json!({
                    "response": {
                        "response": {
                            "usage": {
                                "input_tokens": 1,
                                "output_tokens": 1
                            }
                        }
                    },
                    "provider": "openai"
                }),
            ))
            .unwrap();

        let journal_path = session_journal::journal_file_path(&sid);
        std::fs::set_permissions(&journal_path, std::fs::Permissions::from_mode(0o444)).unwrap();

        let error = append_one_shot_journal_events(
            Some(&sid),
            Some("test-model"),
            "continue",
            &crate::tests::stub_stream_result("answer"),
            Instant::now(),
        )
        .expect_err("read-only journal should surface append failure");

        assert!(
            error.contains("failed to append one-shot journal events"),
            "{error}"
        );

        std::fs::set_permissions(&journal_path, std::fs::Permissions::from_mode(0o644)).unwrap();
    }
}
