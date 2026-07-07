use crate::cli::session::session_state::SessionState;
use crate::cli::stream::streaming_types::StreamResult;
use astra_services::session_journal;
use std::time::Instant;

/// Cloud journal ingestion is server-owned; CLI keeps the local journal path.
fn enqueue_ingestion(_state: &SessionState, event: &session_journal::JournalEvent) {
    let store = astra_services::SyncOutboxStore::local();
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
        tracing::warn!(
            target: "astra_cli::cloud_sync",
            event_type = ?event.event_type,
            "skipping sync outbox enqueue for journal event without a session_id"
        );
        return;
    }
    match store.enqueue_journal_event(event) {
        Ok(_) => crate::cli::cloud_sync::schedule_sync_outbox_drain(),
        Err(error) => {
            tracing::warn!(
                target: "astra_cli::cloud_sync",
                ?error,
                event_type = ?event.event_type,
                session_id = event.session_id.as_deref().unwrap_or(""),
                "failed to enqueue journal event into durable sync outbox"
            );
        }
    }
}

pub(crate) fn enqueue_ingestion_pub(state: &SessionState, event: &session_journal::JournalEvent) {
    enqueue_ingestion(state, event);
}

pub(crate) async fn close_pending_memory_feedback_at_turn_end(
    session_id: Option<&str>,
    cloud_base: Option<String>,
    cloud_token: Option<String>,
    context_prefix: &str,
) -> astra_tools::memoria::FeedbackDrainReport {
    let Some(session_id) = session_id.filter(|sid| !sid.trim().is_empty()) else {
        return astra_tools::memoria::FeedbackDrainReport::default();
    };
    crate::edge_tools::memoria::close_pending_recall_feedback_with_proxy(
        session_id,
        "useful",
        context_prefix,
        cloud_base,
        cloud_token,
    )
    .await
}

#[derive(Clone)]
struct JournalPromptTurn {
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
            event
                .metadata
                .as_ref()
                .and_then(|meta| meta.get("cache_hit_ratio"))
                .and_then(serde_json::Value::as_f64)
        })
        .collect();
    let prior_ratios = if prior_feedback_ratios.is_empty() {
        turns
            .iter()
            .take(turns.len().saturating_sub(1))
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
    let stats = astra_turn_core::pipeline_stats::PipelineStats {
        turns_executed: prior_ratios.len() as u32,
        avg_cache_hit_ratio,
        ..Default::default()
    };

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
) -> Result<(), String> {
    let Some(session_id) = session_id.filter(|sid| !sid.is_empty()) else {
        return Ok(());
    };
    let writer = session_journal::JournalWriter::new(session_id)
        .map_err(|error| format!("failed to create session journal for {session_id}: {error}"))?;
    let existing_events = match session_journal::read_journal(session_id) {
        Ok(events) => events,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(error) => {
            return Err(format!(
                "failed to read session journal for {session_id}: {error}"
            ));
        }
    };

    let mut prompt_events = existing_events.clone();
    prompt_events.extend_from_slice(&result.turn_observability_events);
    let turn = journal_prompt_turns(&prompt_events).len() as u32;
    if turn == 0 {
        return Ok(());
    }
    if existing_events.iter().any(|event| {
        event.turn == Some(turn) && event.event_type == session_journal::JournalEventType::Turn
    }) {
        return Ok(());
    }

    let mut append_events = build_bridge_pipeline_journal_events(
        Some(session_id),
        turn,
        model_id.unwrap_or("unknown"),
        &result.turn_observability_events,
    )?;
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
        .with_run_id(result.run_id.as_deref())
        .with_budget_pressure(result.budget_pressure)
        .with_cache_tokens(result.cache_read_tokens, result.cache_creation_tokens),
    );
    if let Err(error) = writer.append_bulk(&append_events) {
        return Err(format!(
            "failed to append one-shot journal events for {session_id}: {error}"
        ));
    }
    Ok(())
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
                turns.push(JournalPromptTurn { snapshot, usage });
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
        cache_eligible_tokens as usize,
    )
}

#[cfg(test)]
mod tests {
    use super::{
        append_one_shot_journal_events, build_bridge_pipeline_journal_events,
        close_pending_memory_feedback_at_turn_end,
    };
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;
    use std::time::Instant;

    use astra_services::session_journal;

    #[tokio::test]
    async fn close_pending_memory_feedback_at_turn_end_drains_recall_queue() {
        let session_id = "chat-turn-close-feedback";
        astra_tools::memoria::MemoriaClient::reset_recall_ledger(session_id);
        astra_tools::memoria::MemoriaClient::record_recall(session_id, 4, vec!["m1".into()]);

        let report = close_pending_memory_feedback_at_turn_end(
            Some(session_id),
            Some("http://127.0.0.1:9".to_string()),
            Some("token".to_string()),
            "unit-test",
        )
        .await;

        assert_eq!(report.attempted, 1);
        assert_eq!(report.failed, 1);
        assert_eq!(report.succeeded, 0);
        assert_eq!(
            astra_tools::memoria::MemoriaClient::pending_recall_count(session_id),
            0
        );
    }

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
