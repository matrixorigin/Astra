//! Canonical journal hydration for a resumed TUI session.

use std::{
    collections::{HashMap, HashSet},
    time::Duration,
};

use futures_util::StreamExt;

use super::ChatWidget;
use crate::tui::turn_event::{ToolStatus, TurnEvent};

const EXPLAIN_REPLAY_BUDGET: Duration = Duration::from_secs(10);
const EXPLAIN_REPLAY_TOKEN_BUDGET: Duration = Duration::from_secs(5);

#[derive(Debug, Default)]
struct RestoredExplainAnalyze {
    events: Vec<astra_turn_types::ExplainAnalyzeEventV1>,
    delivery_degraded: bool,
    publication: Option<astra_turn_types::ArtifactPublicationV1>,
}

/// Read the root's canonical append-only transcript lane away from the UI
/// worker, then rebuild the compact-chat scrollback. Presentation-only system
/// rows and old TUI JSONL projections intentionally do not participate: the
/// durable transcript browser and resumed chat now share one source.
pub(crate) async fn load(
    session_id: impl Into<String>,
    api: &astra_thin_client::ThinClient,
    profile: Option<&str>,
    restore_explain_analyze: bool,
) -> ChatWidget {
    let session_id = session_id.into();
    let id_for_read = session_id.clone();
    let journal_dir_override = astra_services::session_journal::current_journal_dir_override();
    let events = tokio::task::spawn_blocking(move || {
        let _scope = journal_dir_override
            .as_deref()
            .map(astra_services::session_journal::JournalDirGuard::new);
        astra_services::session_journal::read_journal_append_order(&id_for_read)
    })
    .await
    .ok()
    .and_then(Result::ok)
    .unwrap_or_default();

    let run_ids = root_transcript_run_ids(&events);
    let (explain_by_run, unavailable_runs) = if restore_explain_analyze {
        restore_run_explain_analyze(api, profile, run_ids).await
    } else {
        (HashMap::new(), 0)
    };

    let mut widget = ChatWidget::new(session_id);
    widget.replay(canonical_root_turn_events(events, explain_by_run));
    if unavailable_runs > 0 {
        widget.commit_ephemeral_warning(format!(
            "Explain Analyze · {unavailable_runs} saved graph(s) are partial or unavailable because durable event replay was interrupted. The transcript remains available."
        ));
    }
    widget
}

fn root_transcript_run_ids(
    events: &[astra_services::session_journal::JournalEvent],
) -> Vec<String> {
    let mut seen = HashSet::new();
    events
        .iter()
        .filter_map(|event| event.transcript_item.as_ref())
        .filter(|item| item.agent_id == "root")
        .filter_map(|item| {
            let run_id = item.run_id.trim();
            (!run_id.is_empty() && seen.insert(run_id.to_string())).then(|| run_id.to_string())
        })
        .collect()
}

async fn restore_run_explain_analyze(
    api: &astra_thin_client::ThinClient,
    profile: Option<&str>,
    run_ids: Vec<String>,
) -> (HashMap<String, RestoredExplainAnalyze>, usize) {
    if run_ids.is_empty() {
        return (HashMap::new(), 0);
    }

    let deadline = tokio::time::Instant::now() + EXPLAIN_REPLAY_BUDGET;
    let token_deadline = (tokio::time::Instant::now() + EXPLAIN_REPLAY_TOKEN_BUDGET).min(deadline);
    let token = tokio::time::timeout_at(
        token_deadline,
        crate::cli::session::session_runtime::fresh_access_token(api, profile),
    )
    .await
    .ok()
    .flatten();
    let Some(token) = token else {
        return (HashMap::new(), run_ids.len());
    };

    restore_run_explain_analyze_with_token(api, token, run_ids, deadline).await
}

async fn restore_run_explain_analyze_with_token(
    api: &astra_thin_client::ThinClient,
    token: String,
    run_ids: Vec<String>,
    deadline: tokio::time::Instant,
) -> (HashMap<String, RestoredExplainAnalyze>, usize) {
    let restored = futures_util::stream::iter(run_ids.into_iter().map(|run_id| {
        let api = api.clone();
        let token = token.clone();
        async move {
            let stream = api.stream_run_replay(&run_id, 0, Some(&token));
            let restored = collect_explain_analyze_replay(stream, deadline).await;
            (run_id, restored)
        }
    }))
    .buffer_unordered(4)
    .collect::<Vec<_>>()
    .await;

    let unavailable = restored
        .iter()
        .filter(|(_, restored)| restored.events.is_empty() || restored.delivery_degraded)
        .count();
    let by_run = restored.into_iter().collect();
    (by_run, unavailable)
}

async fn collect_explain_analyze_replay<S>(
    mut stream: S,
    deadline: tokio::time::Instant,
) -> RestoredExplainAnalyze
where
    S: futures_util::Stream<
            Item = Result<astra_thin_client::StreamEvent, astra_thin_client::ThinClientError>,
        > + Unpin,
{
    let mut restored = RestoredExplainAnalyze::default();
    loop {
        let next = match tokio::time::timeout_at(deadline, stream.next()).await {
            Ok(next) => next,
            Err(_) => {
                tracing::debug!("timed out restoring Explain Analyze from durable run stream");
                restored.delivery_degraded = true;
                break;
            }
        };
        match next {
            Some(Ok(astra_thin_client::StreamEvent::ArtifactPublication(outcome))) => {
                restored.publication = Some(outcome);
            }
            Some(Ok(astra_thin_client::StreamEvent::ExplainAnalyze(fact))) => {
                restored.events.push(fact);
            }
            Some(Ok(astra_thin_client::StreamEvent::Other { event_type, raw }))
                if event_type == "stream_gap"
                    && raw.get("explain_analyze_recovered")
                        == Some(&serde_json::Value::Bool(false)) =>
            {
                restored.delivery_degraded = true;
            }
            Some(Ok(astra_thin_client::StreamEvent::Error { raw, .. }))
                if raw
                    .get("index")
                    .and_then(serde_json::Value::as_u64)
                    .is_none() =>
            {
                restored.delivery_degraded = true;
            }
            Some(Ok(_)) => {}
            Some(Err(error)) => {
                tracing::debug!(%error, "could not restore Explain Analyze from durable run stream");
                restored.delivery_degraded = true;
                break;
            }
            None => break,
        }
    }
    restored
}

/// Convert only typed, root-owned transcript payloads into the compact chat
/// cells. The full-fidelity browser consumes the same payload directly; this
/// conversion is intentionally a lossy visual projection, never a second
/// durable record or a prompt-history reconstruction.
fn canonical_root_turn_events(
    events: Vec<astra_services::session_journal::JournalEvent>,
    mut explain_by_run: HashMap<String, RestoredExplainAnalyze>,
) -> Vec<TurnEvent> {
    let mut last_item_seq_by_run = HashMap::new();
    for item in events
        .iter()
        .filter_map(|event| event.transcript_item.as_ref())
        .filter(|item| item.agent_id == "root")
    {
        last_item_seq_by_run
            .entry(item.run_id.clone())
            .and_modify(|last: &mut u64| *last = (*last).max(item.item_seq))
            .or_insert(item.item_seq);
    }

    let mut seen_source_ids = HashSet::new();
    let mut out = Vec::new();
    for event in events {
        let Some(item) = event.transcript_item else {
            continue;
        };
        if item.agent_id != "root" {
            continue;
        }
        let source_id = if item.source_event_id.trim().is_empty() {
            format!("{}:{}", item.run_id, item.item_seq)
        } else {
            item.source_event_id
        };
        if !seen_source_ids.insert(source_id) {
            continue;
        }
        let run_id = item.run_id.clone();
        let is_last_run_item = last_item_seq_by_run.get(&run_id) == Some(&item.item_seq);
        let message = item.message;
        let role = message.get("role").and_then(serde_json::Value::as_str);
        match role {
            Some("user") => {
                if let Some(content) = message.get("content").and_then(serde_json::Value::as_str)
                    && !content.is_empty()
                {
                    out.push(TurnEvent::User {
                        ts: Some(event.ts),
                        text: content.to_string(),
                    });
                }
            }
            Some("assistant") => {
                if let Some(reasoning) = message
                    .get("reasoning_content")
                    .or_else(|| message.get("reasoning"))
                    .and_then(serde_json::Value::as_str)
                    .filter(|text| !text.is_empty())
                {
                    out.push(TurnEvent::Thinking {
                        ts: Some(event.ts.clone()),
                        text: reasoning.to_string(),
                        duration_ms: message
                            .get("reasoning_duration_ms")
                            .and_then(serde_json::Value::as_u64),
                    });
                }
                if let Some(content) = message
                    .get("content")
                    .and_then(serde_json::Value::as_str)
                    .filter(|text| !text.is_empty())
                {
                    out.push(TurnEvent::Assistant {
                        ts: Some(event.ts),
                        markdown: content.to_string(),
                    });
                }
            }
            Some("tool") => {
                let content = message
                    .get("content")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                out.push(TurnEvent::Tool {
                    ts: Some(event.ts),
                    name: message
                        .get("name")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or("tool")
                        .to_string(),
                    description: String::new(),
                    status: match message.get("status").and_then(serde_json::Value::as_str) {
                        Some("uncertain") => ToolStatus::Uncertain,
                        Some("failed" | "error") => ToolStatus::Failed,
                        _ => ToolStatus::Success,
                    },
                    duration_ms: message
                        .get("duration_ms")
                        .and_then(serde_json::Value::as_u64)
                        .unwrap_or_default(),
                    output_summary: (!content.is_empty()).then_some(content.clone()),
                    output: (!content.is_empty()).then_some(content),
                });
            }
            _ => {}
        }
        if is_last_run_item && let Some(restored) = explain_by_run.remove(&run_id) {
            if !restored.events.is_empty() {
                out.push(TurnEvent::ExplainAnalyze {
                    events: restored.events,
                    delivery_degraded: restored.delivery_degraded,
                });
            }
            if let Some(outcome) = restored
                .publication
                .filter(|outcome| outcome.run_id == run_id)
            {
                out.push(TurnEvent::System {
                    ts: None,
                    level: match outcome.result {
                        astra_turn_types::ArtifactPublicationResult::Published { .. } => {
                            crate::tui::turn_event::SystemLevel::Info
                        }
                        astra_turn_types::ArtifactPublicationResult::Unavailable { .. } => {
                            crate::tui::turn_event::SystemLevel::Warning
                        }
                    },
                    text: outcome.user_notice(),
                    link: None,
                });
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use astra_services::session_journal::{self, JournalDirGuard, JournalEvent};

    fn replay_fact() -> astra_turn_types::ExplainAnalyzeEventV1 {
        use astra_turn_types::{
            EXPLAIN_ANALYZE_SCHEMA_VERSION, ExplainAnalyzeNodeKindV1, ExplainAnalyzeTransitionV1,
        };

        astra_turn_types::ExplainAnalyzeEventV1 {
            schema_version: EXPLAIN_ANALYZE_SCHEMA_VERSION,
            event_id: "clock:fact".into(),
            run_id: "run".into(),
            turn_id: "turn".into(),
            node_id: "turn".into(),
            parent_node_id: None,
            dependency_node_ids: Vec::new(),
            producer_id: "runtime".into(),
            clock_domain_id: "clock".into(),
            kind: ExplainAnalyzeNodeKindV1::Turn,
            round_index: None,
            attempt_index: None,
            label: "User turn".into(),
            transition: ExplainAnalyzeTransitionV1::Started,
            elapsed_ms: 0,
            start_elapsed_ms: None,
            duration_ms: None,
            outcome: None,
            usage: None,
            context: None,
            coverage_gaps: Vec::new(),
        }
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn canonical_root_journal_hydrates_chat_without_child_or_retry_duplicates() {
        session_journal::set_journal_content_redact_override(Some(false));
        let tmp = tempfile::tempdir().unwrap();
        let _guard = JournalDirGuard::new(tmp.path());
        let session_id = "sess_resume_canonical";
        let event = |run_id: &str, agent_id: &str, seq, message| {
            JournalEvent::transcript_item(session_id, run_id, agent_id, seq, &message)
                .expect("valid transcript message")
        };
        let journal = session_journal::JournalWriter::new(session_id).unwrap();
        journal
            .append_bulk(&[
                event(
                    "root-run",
                    "root",
                    1,
                    serde_json::json!({"role": "user", "content": "what's up"}),
                ),
                event(
                    "root-run",
                    "root",
                    2,
                    serde_json::json!({
                        "role": "assistant",
                        "reasoning_content": "inspect the state",
                        "content": "all good",
                    }),
                ),
                event(
                    "child-run",
                    "reviewer",
                    1,
                    serde_json::json!({"role": "assistant", "content": "child-only"}),
                ),
                event(
                    "root-run",
                    "root",
                    2,
                    serde_json::json!({"role": "assistant", "content": "retry must not win"}),
                ),
            ])
            .unwrap();

        let api = astra_thin_client::ThinClient::new("http://127.0.0.1:1", None).unwrap();
        let widget = load(session_id, &api, None, false).await;

        assert_eq!(widget.session_id(), session_id);
        assert_eq!(widget.history().len(), 3);
        let text = widget
            .history()
            .iter()
            .flat_map(|cell| cell.display_lines(100))
            .flat_map(|line| line.spans)
            .map(|span| span.content.into_owned())
            .collect::<String>();
        assert!(text.contains("what's up"), "{text}");
        assert!(text.contains("all good"), "{text}");
        assert!(!text.contains("child-only"), "{text}");
        assert!(!text.contains("retry must not win"), "{text}");
        session_journal::set_journal_content_redact_override(None);
    }

    #[test]
    fn resume_places_durable_graph_facts_after_their_run_transcript() {
        use astra_turn_types::{
            EXPLAIN_ANALYZE_SCHEMA_VERSION, ExplainAnalyzeEventV1, ExplainAnalyzeNodeKindV1,
            ExplainAnalyzeOutcomeV1, ExplainAnalyzeTransitionV1,
        };

        let journal_event = |run_id: &str, seq, message| {
            JournalEvent::transcript_item("session", run_id, "root", seq, &message)
                .expect("valid transcript message")
        };
        let events = vec![
            journal_event(
                "run-a",
                1,
                serde_json::json!({"role": "user", "content": "first"}),
            ),
            journal_event(
                "run-a",
                2,
                serde_json::json!({"role": "assistant", "content": "one"}),
            ),
            journal_event(
                "run-b",
                1,
                serde_json::json!({"role": "user", "content": "second"}),
            ),
            journal_event(
                "run-b",
                2,
                serde_json::json!({"role": "assistant", "content": "two"}),
            ),
        ];
        let finished_turn = |run_id: &str, turn_id: &str, label: &str| ExplainAnalyzeEventV1 {
            schema_version: EXPLAIN_ANALYZE_SCHEMA_VERSION,
            event_id: format!("{run_id}:finish"),
            run_id: run_id.to_string(),
            turn_id: turn_id.to_string(),
            node_id: turn_id.to_string(),
            parent_node_id: None,
            dependency_node_ids: Vec::new(),
            producer_id: "runtime".to_string(),
            clock_domain_id: format!("clock-{run_id}"),
            kind: ExplainAnalyzeNodeKindV1::Turn,
            round_index: None,
            attempt_index: None,
            label: label.to_string(),
            transition: ExplainAnalyzeTransitionV1::Finished,
            elapsed_ms: 35,
            start_elapsed_ms: Some(0),
            duration_ms: Some(35),
            outcome: Some(ExplainAnalyzeOutcomeV1::Completed),
            usage: None,
            context: None,
            coverage_gaps: Vec::new(),
        };
        let events = canonical_root_turn_events(
            events,
            HashMap::from([
                (
                    "run-a".to_string(),
                    RestoredExplainAnalyze {
                        events: vec![finished_turn("run-a", "turn-a", "First turn")],
                        delivery_degraded: false,
                        publication: None,
                    },
                ),
                (
                    "run-b".to_string(),
                    RestoredExplainAnalyze {
                        events: vec![finished_turn("run-b", "turn-b", "Second turn")],
                        delivery_degraded: true,
                        publication: None,
                    },
                ),
            ]),
        );

        assert!(matches!(events[2], TurnEvent::ExplainAnalyze { .. }));
        assert!(matches!(events[5], TurnEvent::ExplainAnalyze { .. }));

        let mut widget = ChatWidget::new("session");
        widget.replay(events);
        assert!(
            widget.history()[2]
                .as_any_ref()
                .is::<crate::tui::history_cell::explain_analyze::ExplainAnalyzeCell>()
        );
        assert!(
            widget.history()[5]
                .as_any_ref()
                .is::<crate::tui::history_cell::explain_analyze::ExplainAnalyzeCell>()
        );
        let first_graph = widget.history()[2]
            .display_lines(100)
            .into_iter()
            .flat_map(|line| line.spans)
            .map(|span| span.content.into_owned())
            .collect::<String>();
        assert!(first_graph.contains("First turn"), "{first_graph}");
        assert!(!first_graph.contains("Second turn"), "{first_graph}");
        let second_graph = widget.history()[5]
            .display_lines(120)
            .into_iter()
            .flat_map(|line| line.spans)
            .map(|span| span.content.into_owned())
            .collect::<String>();
        assert!(
            second_graph.contains("incomplete · stream gap"),
            "{second_graph}"
        );
    }

    #[tokio::test]
    async fn replay_reads_publication_after_the_task_terminal() {
        let outcome = astra_turn_types::ArtifactPublicationV1 {
            schema_version: 1,
            run_id: "run-1".into(),
            turn_id: "turn-1".into(),
            execution_owner_generation: 1,
            artifact_type: "explain_analyze_snapshot".into(),
            recorded: true,
            result: astra_turn_types::ArtifactPublicationResult::Unavailable {
                reason_code: "storage_failed".into(),
                message: "Report storage failed.".into(),
            },
        };
        let stream = futures_util::stream::iter(vec![
            astra_thin_client::classify_stream_event(
                serde_json::json!({"type":"run_finished", "run_id":"run-1", "status":"completed"}),
            ),
            astra_thin_client::classify_stream_event(outcome.to_wire()),
        ]);
        let restored = collect_explain_analyze_replay(
            stream,
            tokio::time::Instant::now() + Duration::from_secs(1),
        )
        .await;
        assert_eq!(restored.publication, Some(outcome));
        assert!(!restored.delivery_degraded);
    }

    #[tokio::test]
    async fn replay_keeps_partial_facts_and_marks_only_explain_delivery_failures() {
        use astra_thin_client::StreamEvent;
        use futures_util::StreamExt;

        let event = replay_fact();
        let stream = futures_util::stream::iter(vec![
            Ok(StreamEvent::ExplainAnalyze(event.clone())),
            Ok(StreamEvent::Other {
                event_type: "stream_gap".into(),
                raw: serde_json::json!({
                    "type": "stream_gap",
                    "explain_analyze_recovered": false,
                }),
            }),
        ]);
        let restored = collect_explain_analyze_replay(
            stream,
            tokio::time::Instant::now() + Duration::from_secs(1),
        )
        .await;
        assert_eq!(restored.events, vec![event]);
        assert!(restored.delivery_degraded);

        let historical_only = futures_util::stream::iter(vec![
            Ok(StreamEvent::Other {
                event_type: "stream_gap".into(),
                raw: serde_json::json!({"type": "stream_gap"}),
            }),
            Ok(StreamEvent::Error {
                message: "historical run error".into(),
                code: None,
                retryable: false,
                raw: serde_json::json!({"type": "error", "index": 8}),
            }),
        ]);
        let restored = collect_explain_analyze_replay(
            historical_only,
            tokio::time::Instant::now() + Duration::from_secs(1),
        )
        .await;
        assert!(!restored.delivery_degraded);

        let connection_failure = futures_util::stream::iter(vec![Ok(StreamEvent::Error {
            message: "connection interrupted".into(),
            code: None,
            retryable: true,
            raw: serde_json::json!({"type": "error", "message": "connection interrupted"}),
        })]);
        let restored = collect_explain_analyze_replay(
            connection_failure,
            tokio::time::Instant::now() + Duration::from_secs(1),
        )
        .await;
        assert!(restored.delivery_degraded);

        let delayed =
            futures_util::stream::iter(vec![Ok::<_, astra_thin_client::ThinClientError>(
                StreamEvent::ExplainAnalyze(replay_fact()),
            )])
            .chain(futures_util::stream::once(async {
                tokio::time::sleep(Duration::from_secs(5)).await;
                Ok(StreamEvent::Ping)
            }))
            .boxed();
        let restored = collect_explain_analyze_replay(
            delayed,
            tokio::time::Instant::now() + Duration::from_millis(20),
        )
        .await;
        assert_eq!(restored.events.len(), 1, "facts before the timeout survive");
        assert!(restored.delivery_degraded);
    }

    #[tokio::test]
    async fn expired_shared_deadline_marks_all_saved_runs_without_per_run_waits() {
        let api =
            astra_thin_client::ThinClient::new("http://127.0.0.1:1", None).expect("test client");
        let deadline = tokio::time::Instant::now() - Duration::from_millis(1);
        let restored = tokio::time::timeout(
            Duration::from_millis(250),
            restore_run_explain_analyze_with_token(
                &api,
                "test-token".to_string(),
                (0..32).map(|index| format!("run-{index}")).collect(),
                deadline,
            ),
        )
        .await
        .expect("an expired shared deadline must return promptly");
        assert_eq!(restored.0.len(), 32);
        assert_eq!(restored.1, 32);
        assert!(restored.0.values().all(|run| run.delivery_degraded));
    }
}
