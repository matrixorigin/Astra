use super::spawn_mock;
use crate::cli::chat_stream::{
    BasicCliChatContext, ChatTurnParams, DEFAULT_TURN_INDEX, stream_chat_sse,
};
use crate::cli::permission_manager::PermissionManager;
use crate::cli::session::session_state::ExplainMode;
use astra_services::session_journal::{self, JournalEventType, ProcessJournalDirGuard};
use axum::{Json, Router, routing::post};

const TEST_SSE_HEADERS: [(&str, &str); 2] = [
    ("content-type", "text/event-stream"),
    // Mirrors the public Server/ThinClient response contract. These tests use
    // an in-process Axum peer and must not silently emulate a pre-contract
    // Server now that production clients fail closed on a missing header.
    (
        astra_server_types::AGENT_INTERACTION_API_MAJOR_HEADER,
        astra_server_types::AGENT_INTERACTION_API_MAJOR,
    ),
];

fn basic_chat_context<'a>(
    api: &'a astra_thin_client::ThinClient,
    registry: &'a std::sync::Arc<astra_runtime::skills::UnifiedSkillRegistry>,
    message: &'a str,
) -> BasicCliChatContext<'a> {
    BasicCliChatContext {
        api: &api,
        auth_owner: None,
        message,
        offering_id: None,
        model: Some("test-model"),
        provider: None,
        explain: ExplainMode::Off,
        runtime_config: std::sync::Arc::new(astra_config::RuntimeConfig::default()),
        render_md: false,
        verbose_mode: false,
        render_policy: crate::cli::stream::stream_render::RenderPolicy::Silent,
        cli_context: None,
        unified_skill_registry: registry,
        stream_event_tx: None,
        stream_json_emitter: None,
        mcp_manager: None,
        agent_spawner: None,
        root_agent_id: None,
        bg_task_commands: None,
        bg_task_list_cache: None,
        bash_detach_slot: None,
        #[cfg(feature = "harness")]
        harness_sink: None,

        #[cfg(feature = "harness")]
        benchmark_profile: None,
    }
}

// ── chat_stream (SSE agentic loop) ────────────────────────────────────

#[tokio::test]
#[serial_test::serial]
async fn stream_chat_sse_partial_terminal_preserves_remote_tool_accounting() {
    use astra_turn_core::interruption::{
        InterruptionKind, InterruptionRecord, InterruptionStateSummary, ResumeAction,
    };
    let interruption = InterruptionRecord::new(
        InterruptionKind::ExecutionIncomplete,
        ResumeAction::ContinueImmediately,
        InterruptionStateSummary::default(),
    );
    #[derive(Clone, Copy)]
    enum Terminal {
        Failed,
        Interrupted,
        Mismatched,
        Completed,
        CallbackFailed,
        CallbackFailedWithExpired,
        AdmissionRejected,
        AdmissionAndCallbackFailed,
        CancelledDuringCallback,
        ConflictingIdentities,
    }
    use Terminal::*;
    for (terminal_kind, reported_count, root_callbacks, child_callbacks) in [
        (Interrupted, Some(4), 0, 0),
        (Failed, Some(4), 0, 0),
        (Interrupted, None, 0, 0),
        (Failed, None, 0, 0),
        (Failed, Some(4), 1, 0),
        (Failed, None, 1, 0),
        (Interrupted, Some(4), 1, 0),
        (Interrupted, None, 1, 0),
        // A lower-bound count must not repair the original receipt mismatch.
        (Mismatched, Some(1), 2, 0),
        // Child callbacks use the same transport, but belong to a different run.
        (Completed, Some(2), 1, 3),
        (Failed, None, 1, 3),
        (Interrupted, None, 1, 3),
        (CallbackFailed, None, 0, 2),
        (CallbackFailedWithExpired, None, 0, 2),
        (AdmissionRejected, Some(1), 1, 1),
        (AdmissionAndCallbackFailed, None, 0, 2),
        (CancelledDuringCallback, None, 1, 1),
        (ConflictingIdentities, None, 1, 3),
    ] {
        let callback_failed = matches!(
            terminal_kind,
            CallbackFailed
                | CallbackFailedWithExpired
                | AdmissionAndCallbackFailed
                | ConflictingIdentities
        );
        let admission_rejected = matches!(
            terminal_kind,
            AdmissionRejected | AdmissionAndCallbackFailed
        );
        let cancel_during_callback = matches!(terminal_kind, CancelledDuringCallback);
        let conflicting_identities = matches!(terminal_kind, ConflictingIdentities);
        let cancellation = std::sync::Arc::new(tokio_util::sync::CancellationToken::new());
        let callback_cancel = cancellation.clone();
        let expired_child = matches!(terminal_kind, CallbackFailedWithExpired);
        let classified = matches!(terminal_kind, Failed);
        let mismatched_summary = matches!(terminal_kind, Mismatched);
        let completed = matches!(terminal_kind, Completed | AdmissionRejected);
        let request_count = root_callbacks + child_callbacks;
        let callback_count =
            request_count - u32::from(admission_rejected || cancel_during_callback);
        let temp = tempfile::tempdir().unwrap();
        let _journal_guard = ProcessJournalDirGuard::new(temp.path());
        let path = temp.path().join("evidence.txt");
        std::fs::write(
            &path,
            "first evidence\nsecond evidence\nthird evidence\nfourth evidence\n",
        )
        .unwrap();
        let child_marker = temp.path().join("cancelled-child-must-not-write");
        let callback_done = std::sync::Arc::new(tokio::sync::Notify::new());
        let terminal = if classified {
            serde_json::json!({"type":"run_finished","run_id":"partial-run","owner_generation":1,"status":"failed", "error_kind":"network", "error_code":"network", "error":"connection lost"})
        } else if mismatched_summary {
            serde_json::json!({"type":"run_finished","run_id":"partial-run","owner_generation":1,"status":"paused"})
        } else {
            serde_json::json!({"type":"run_finished","run_id":"partial-run","owner_generation":1,"status":"failed", "error_code":"execution_incomplete","error_kind":null,"error":interruption.user_message,"interruption":interruption.to_json()})
        };
        let mut usage = serde_json::json!({"type":"usage","usage_scope":"run_total","input_tokens":12,"output_tokens":3});
        if let Some(count) = reported_count {
            usage["tool_call_count"] = serde_json::json!(count);
        }
        let prefix = format!(
            "data: {}\n\n",
            serde_json::json!({"type":"session_info","session_id":"partial-session","run_id":"partial-run"})
        );
        let request = serde_json::json!({
            "type":"tool_request", "session_id":"partial-session", "run_id":"partial-run",
            "turn_chain_id":"partial-chain", "request_id":"read-1", "schema_admitted_by_server":true,
            "execution_timeout_ms":300_000, "command_timeout_cap_ms":30_000,
            "execution_deadline_unix_ms":4_102_444_800_000_u64,
            "tool":"read_file", "args":{"path":path.to_str().unwrap()}
        });
        let mut prefix = prefix;
        for index in 0..request_count {
            let mut request = request.clone();
            request["request_id"] = serde_json::json!(format!("read-{index}"));
            if index >= root_callbacks {
                request["run_id"] = serde_json::json!("child-run");
                request["session_id"] = serde_json::json!(if callback_failed
                    || admission_rejected
                    || cancel_during_callback
                {
                    "partial-session"
                } else {
                    "child-session"
                });
            }
            request["args"]["start_line"] = serde_json::json!(index + 1);
            request["args"]["end_line"] = serde_json::json!(index + 1);
            if expired_child && index == 1 {
                request["execution_deadline_unix_ms"] = serde_json::json!(1);
            }
            if admission_rejected && index + 1 == request_count {
                request["tool"] = serde_json::json!("unavailable_tool");
                request["args"] = serde_json::json!({});
            }
            if cancel_during_callback {
                if index == 0 {
                    request["tool"] = serde_json::json!("bash");
                    request["args"] = serde_json::json!({"command":"printf first"});
                } else {
                    request["tool"] = serde_json::json!("write_file");
                    request["args"] =
                        serde_json::json!({"path":child_marker, "content":"must not execute"});
                }
            }
            if conflicting_identities {
                request["request_id"] =
                    serde_json::json!(["root-x", "child-y", "child-y", "root-x"][index as usize]);
                request["run_id"] = serde_json::json!(
                    ["partial-run", "child-a", "child-b", "child-c"][index as usize]
                );
            }
            prefix.push_str(&format!("data: {request}\n\n"));
        }
        let mut suffix = [
            serde_json::json!({"type":"text_done","full_text":"Partial result.","partial":true}),
            usage,
            terminal,
        ]
        .into_iter()
        .map(|event| format!("data: {event}\n\n"))
        .collect::<String>();
        if mismatched_summary {
            let wire = sse_text_response_with_execution_summary(
                "Partial result.",
                "partial-session",
                2,
                2,
                &["read_file"],
                1,
                ("partial-run", 1),
            );
            let mut summary = wire
                .lines()
                .filter_map(|line| line.strip_prefix("data: "))
                .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
                .find(|event| event["type"] == "turn_complete")
                .unwrap();
            summary["tool_calls_count"] = serde_json::json!(1);
            summary["observation_tool_calls_count"] = serde_json::json!(1);
            summary["execution_state"] = serde_json::json!({"status":"interrupted","interruption_kind":"execution_incomplete"});
            summary["interruption"] = interruption.to_json();
            suffix.push_str(&format!("data: {summary}\n\n"));
        }
        if completed {
            let count = reported_count.unwrap();
            suffix = sse_text_response_with_execution_summary(
                "Partial result.",
                "partial-session",
                count,
                count,
                if count == 0 { &[] } else { &["read_file"] },
                1,
                ("partial-run", 1),
            );
        }
        let callback_bodies = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let bodies_for_route = callback_bodies.clone();
        let callbacks_received = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
        let notify = callback_done.clone();
        let received_for_route = callbacks_received.clone();
        let app =
            Router::new()
                .route(
                    "/chat/stream",
                    post(move || {
                        let prefix = prefix.clone();
                        let suffix = suffix.clone();
                        let notify = notify.clone();
                        async move {
                            use futures_util::StreamExt;
                            let chunks = futures_util::stream::once(async move {
                                Ok::<_, std::io::Error>(prefix)
                            })
                            .chain(futures_util::stream::once(async move {
                                if callback_count > 0 && !callback_failed {
                                    tokio::time::timeout(
                                        std::time::Duration::from_secs(5),
                                        notify.notified(),
                                    )
                                    .await
                                    .unwrap();
                                }
                                if callback_failed {
                                    std::future::pending::<()>().await;
                                }
                                Ok::<_, std::io::Error>(suffix)
                            }));
                            (TEST_SSE_HEADERS, axum::body::Body::from_stream(chunks))
                        }
                    }),
                )
                .route(
                    "/tools/result",
                    post(move |Json(body): Json<serde_json::Value>| {
                        let callback_cancel = callback_cancel.clone();
                        let bodies = bodies_for_route.clone();
                        let notify = callback_done.clone();
                        let callbacks_received = received_for_route.clone();
                        async move {
                            bodies.lock().unwrap().push(body);
                            // Cancel while the first callback is in flight, before its HTTP acknowledgement.
                            if cancel_during_callback {
                                callback_cancel.cancel();
                            }
                            if callbacks_received.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
                                + 1
                                == callback_count
                            {
                                notify.notify_one();
                            }
                            (
                                if callback_failed {
                                    axum::http::StatusCode::SERVICE_UNAVAILABLE
                                } else {
                                    axum::http::StatusCode::OK
                                },
                                Json(serde_json::json!({"ok":!callback_failed})),
                            )
                        }
                    }),
                );
        let app = app.route(
            "/chat/runs/{run_id}",
            axum::routing::delete(|| async {
                Json(serde_json::json!({"run_id":"partial-run", "status":"cancelled"}))
            }),
        );
        let base = spawn_mock(app).await;
        let api = astra_thin_client::ThinClient::new(&base, None).unwrap();
        let registry = astra_runtime::skills::empty_unified_registry();
        let context = basic_chat_context(&api, &registry, "review results");
        let mut pm = PermissionManager::new(true);
        let mut params = ChatTurnParams::basic_cli(&context, "fake-token", None, &mut pm);
        if cancel_during_callback {
            params.cancel_token = Some(cancellation.clone());
        }
        let result = stream_chat_sse(params).await;
        if conflicting_identities {
            let failure = result.unwrap_err();
            assert_eq!(failure.partial.tool_calls_count, 0);
            assert!(failure.partial.tool_call_records.is_empty());
            assert!(callback_bodies.lock().unwrap().is_empty());
            let events = session_journal::read_journal("partial-session").unwrap();
            assert!(
                events
                    .iter()
                    .all(|event| event.tool_calls.as_ref().is_none_or(Vec::is_empty))
            );
            continue;
        }
        if cancel_during_callback {
            let failure = result.unwrap_err();
            assert_eq!(failure.partial.tool_calls_count, 1);
            assert_eq!(failure.partial.tool_call_records.len(), 1);
            assert_eq!(failure.partial.tool_call_records[0].name, "bash");
            assert!(!child_marker.exists());
            assert_eq!(callback_bodies.lock().unwrap().len(), 1);
            let events = session_journal::read_journal("partial-session").unwrap();
            let observations = events
                .iter()
                .filter(|event| event.event_type == JournalEventType::SyncMarker)
                .collect::<Vec<_>>();
            assert_eq!(observations.len(), 1);
            assert_eq!(
                observations[0].producer_scope.as_ref().unwrap().run_id,
                "child-run"
            );
            let records = observations[0].tool_calls.as_ref().unwrap();
            assert_eq!(records.len(), 1);
            assert_eq!(records[0].name, "write_file");
            assert!(!records[0].was_executed());
            assert!(observations[0].tool_count.is_none());
            assert!(observations[0].tool_outcomes.is_none());
            continue;
        }
        if admission_rejected {
            let bodies = callback_bodies.lock().unwrap();
            assert_eq!(bodies.len(), 1, "surface refusal sends no callback");
            assert_eq!(bodies[0]["request_id"], "read-0");
            if !callback_failed {
                assert_eq!(bodies[0]["run_id"], "partial-run");
                let events = session_journal::read_journal("partial-session").unwrap();
                let observations = events
                    .iter()
                    .filter(|event| event.event_type == JournalEventType::SyncMarker)
                    .collect::<Vec<_>>();
                assert_eq!(observations.len(), 1);
                let observation = observations[0];
                let records = observation.tool_calls.as_ref().unwrap();
                assert_eq!(records.len(), 1);
                assert_eq!(records[0].tool_call_id.as_deref(), Some("read-1"));
                assert!(!records[0].was_executed());
                assert!(records[0].execution_completion.is_none());
                assert_eq!(
                    observation.producer_scope.as_ref().unwrap().run_id,
                    "child-run"
                );
                assert!(observation.tool_count.is_none());
                assert!(observation.tool_outcomes.is_none());
            }
        }
        if callback_failed {
            let failure = result.unwrap_err();
            assert_eq!(failure.partial.tool_calls_count, 0);
            assert!(failure.partial.tool_call_records.is_empty());
            let events = session_journal::read_journal("partial-session").unwrap();
            let observation = events
                .iter()
                .find(|event| event.event_type == JournalEventType::SyncMarker)
                .unwrap();
            assert_eq!(
                observation.producer_scope.as_ref().unwrap().run_id,
                "child-run"
            );
            assert!(observation.tool_count.is_none());
            assert!(observation.tool_outcomes.is_none());
            let records = observation.tool_calls.as_ref().unwrap();
            assert_eq!(records.len(), 2);
            assert!(records[0].ok);
            let unexecuted_child = expired_child || admission_rejected;
            assert_eq!(records[1].ok, !unexecuted_child);
            assert_eq!(records[1].was_executed(), !unexecuted_child);
            assert!(
                records[1]
                    .result_full
                    .as_ref()
                    .unwrap()
                    .contains(if expired_child {
                        "deadline expired"
                    } else if admission_rejected {
                        "unavailable_tool"
                    } else {
                        "second evidence"
                    })
            );
            // The first executed result was posted; the sibling observation is retained once.
            assert_eq!(
                callbacks_received.load(std::sync::atomic::Ordering::SeqCst),
                1
            );
            let mut root_error = session_journal::JournalEvent::turn_error(
                Some("partial-session"),
                1,
                None,
                "request",
                "callback failed",
                1,
            )
            .with_producer_scope(Some("partial-run"));
            crate::cli::stream::streaming_types::apply_partial_turn_data_to_error_event(
                &mut root_error,
                &failure.partial,
            );
            session_journal::JournalWriter::new("partial-session")
                .unwrap()
                .append(&root_error)
                .unwrap();
            let diff = crate::cli::journal_diff::compute_diff("partial-session", "partial-session")
                .unwrap();
            let executed = if unexecuted_child { 1 } else { 2 };
            assert_eq!(diff.a_tool_calls_executed, executed);
            assert_eq!(diff.a_tool_sequence, vec!["read_file"; executed as usize]);
            assert!(!diff.a_tool_sequence_complete);
            let digest = crate::cli::journal_digest::build_digest(
                "partial-session",
                crate::cli::journal_digest::DigestFocus::All,
            )
            .unwrap();
            assert_eq!(digest.aggregates.total_tool_calls, 0);
            continue;
        }
        if classified {
            let failure = result.unwrap_err();
            assert_eq!(failure.partial.partial_text, "Partial result.");
            assert_eq!(
                failure.partial.tool_calls_count,
                reported_count.unwrap_or(0).max(root_callbacks)
            );
            assert_eq!(
                failure.partial.tool_call_records.len(),
                root_callbacks as usize
            );
            if root_callbacks > 0 {
                assert!(failure.partial.tool_call_records[0].ok);
            }
            assert!(failure.partial.tool_outcomes.is_none());
            assert!(failure.partial.failed_run_terminal.is_some());
            let sid = "failed-root-projection";
            let mut event = session_journal::JournalEvent::turn_error(
                Some(sid),
                1,
                None,
                "request",
                "connection lost",
                1,
            );
            crate::cli::stream::streaming_types::apply_partial_turn_data_to_error_event(
                &mut event,
                &failure.partial,
            );
            session_journal::JournalWriter::new(sid)
                .unwrap()
                .append(&event)
                .unwrap();
            let digest = crate::cli::journal_digest::build_digest(
                sid,
                crate::cli::journal_digest::DigestFocus::All,
            )
            .unwrap();
            assert_eq!(
                digest.aggregates.total_tool_calls,
                u64::from(root_callbacks)
            );
            assert_eq!(
                digest.aggregates.total_fresh_tool_calls,
                u64::from(root_callbacks)
            );
            continue;
        }
        let result = result.unwrap();
        assert_eq!(result.full_text, "Partial result.");
        assert_eq!(
            result.tool_calls_count,
            reported_count.unwrap_or(0).max(root_callbacks)
        );
        if completed {
            assert!(result.interruption.is_none());
            assert_eq!(
                result.canonical_tool_outcomes().unwrap().requested,
                reported_count.unwrap()
            );
            assert_eq!(result.tool_call_records.len(), root_callbacks as usize);
            continue;
        }
        assert!(result.canonical_tool_outcomes().is_none());
        if mismatched_summary {
            assert_eq!(result.llm_rounds, Some(1));
        }
        assert_eq!(result.tool_call_records.len(), root_callbacks as usize);
        if root_callbacks > 0 {
            assert!(result.tool_call_records[0].ok);
        }
        assert!(result.tool_record_coverage_partial);
        assert!(result.interruption.is_some());
        let mut event = session_journal::JournalEvent::turn(
            None,
            1,
            None,
            "request",
            &result.full_text,
            result.tool_calls_count,
            0,
            0,
            0,
        )
        .with_tool_calls(result.tool_call_records.clone());
        result.apply_canonical_tool_outcomes(&mut event);
        assert!(event.tool_outcomes.is_none());
        assert!(event.tool_count.is_none());
    }
}

#[tokio::test]
async fn server_admission_cancelled_during_bearer_resolution_never_dispatches_a_new_attempt() {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    #[derive(Debug)]
    struct AdmissionBearer {
        calls: AtomicUsize,
        blocked_attempt: usize,
        entered: tokio::sync::Notify,
        release: tokio::sync::Notify,
    }

    impl astra_thin_client::client::BearerProvider for AdmissionBearer {
        fn token(
            &self,
        ) -> futures_util::future::BoxFuture<'_, Result<String, astra_thin_client::ThinClientError>>
        {
            Box::pin(async move {
                // The production entrypoint first authenticates GET /models;
                // block the subsequent admission, not that preflight read.
                if self.calls.fetch_add(1, Ordering::SeqCst) == self.blocked_attempt + 1 {
                    self.entered.notify_one();
                    self.release.notified().await;
                }
                Ok("synthetic-admission-token".into())
            })
        }
    }

    // Also cover a retry: cancelling authentication must not dispatch another
    // attempt or erase evidence that the first attempt was already sent.
    for blocked_attempt in [0, 1] {
        let dispatches = Arc::new(AtomicUsize::new(0));
        let observed = dispatches.clone();
        let app = Router::new().route(
            "/chat/stream",
            post(move || {
                let observed = observed.clone();
                async move {
                    let attempt = observed.fetch_add(1, Ordering::SeqCst);
                    let status = if blocked_attempt == 1 && attempt == 0 {
                        axum::http::StatusCode::TOO_MANY_REQUESTS
                    } else {
                        axum::http::StatusCode::SERVICE_UNAVAILABLE
                    };
                    (
                        status,
                        [("retry-after", "0")],
                        "synthetic admission response",
                    )
                }
            }),
        );
        let base = spawn_mock(app).await;
        let bearer = Arc::new(AdmissionBearer {
            calls: AtomicUsize::new(0),
            blocked_attempt,
            entered: tokio::sync::Notify::new(),
            release: tokio::sync::Notify::new(),
        });
        let api = astra_thin_client::ThinClient::new(&base, None)
            .unwrap()
            .with_bearer_provider(bearer.clone());
        let registry = astra_runtime::skills::empty_unified_registry();
        let context = basic_chat_context(&api, &registry, "synthetic request");
        let mut pm = PermissionManager::new(true);
        let cancel = Arc::new(tokio_util::sync::CancellationToken::new());
        let mut params = ChatTurnParams::basic_cli(&context, "synthetic-token", None, &mut pm);
        params.cancel_token = Some(cancel.clone());
        let mut stream = Box::pin(stream_chat_sse(params));
        tokio::select! {
            result = &mut stream => panic!("admission finished before bearer release: {result:?}"),
            entered = tokio::time::timeout(std::time::Duration::from_secs(10), bearer.entered.notified()) => {
                entered.expect("admission must reach its bearer preparation");
            }
        }
        assert_eq!(dispatches.load(Ordering::SeqCst), blocked_attempt);
        assert_eq!(bearer.calls.load(Ordering::SeqCst), blocked_attempt + 2);
        cancel.cancel();
        bearer.release.notify_one();
        // Keep polling the same production future, just as interactive
        // cancellation does while looking for the physical response identity.
        let failure = tokio::time::timeout(std::time::Duration::from_secs(5), stream)
            .await
            .expect("cancelled admission must finish")
            .unwrap_err();
        assert_eq!(dispatches.load(Ordering::SeqCst), blocked_attempt);
        assert_eq!(
            failure.partial.server_admission_not_dispatched,
            blocked_attempt == 0
        );
        assert!(!failure.partial.admission_rejected);
    }
}

#[tokio::test]
async fn server_admission_dispatch_evidence_comes_from_the_actual_request_boundary() {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    for (cancel_before_dispatch, cancel_before_retry) in
        [(true, false), (false, false), (false, true)]
    {
        let dispatches = Arc::new(AtomicUsize::new(0));
        let observed = dispatches.clone();
        let cancel = Arc::new(tokio_util::sync::CancellationToken::new());
        let cancel_at_server = cancel.clone();
        let app = Router::new().route(
            "/chat/stream",
            post(move || {
                let observed = observed.clone();
                let cancel = cancel_at_server.clone();
                async move {
                    observed.fetch_add(1, Ordering::SeqCst);
                    let status = if cancel_before_retry {
                        cancel.cancel();
                        axum::http::StatusCode::TOO_MANY_REQUESTS
                    } else {
                        axum::http::StatusCode::SERVICE_UNAVAILABLE
                    };
                    // No identity or typed pre-admission rejection: admission is
                    // ambiguous even though this response has already finished.
                    (status, [("retry-after", "0")], "synthetic upstream failure")
                }
            }),
        );
        let base = spawn_mock(app).await;
        let api = astra_thin_client::ThinClient::new(&base, None).unwrap();
        let registry = astra_runtime::skills::empty_unified_registry();
        let context = basic_chat_context(&api, &registry, "synthetic request");
        let mut pm = PermissionManager::new(true);
        if cancel_before_dispatch {
            cancel.cancel();
        }
        let mut params = ChatTurnParams::basic_cli(&context, "synthetic-token", None, &mut pm);
        params.cancel_token = Some(cancel);
        let failure = stream_chat_sse(params).await.unwrap_err();
        assert_eq!(
            dispatches.load(Ordering::SeqCst),
            usize::from(!cancel_before_dispatch)
        );
        assert_eq!(
            failure.partial.server_admission_not_dispatched,
            cancel_before_dispatch
        );
        assert!(!failure.partial.admission_rejected);
    }
}

/// Build a canonical SSE response for the mock chat-turn endpoint. Exposed
/// to sibling test modules (e.g. `resume_tests`) so they don't have to
/// duplicate the payload literal.
pub(super) fn sse_text_response(text: &str, session_id: &str) -> String {
    sse_text_response_with_execution_summary(
        text,
        session_id,
        0,
        0,
        &[],
        1,
        (&format!("run-{session_id}"), 1),
    )
}

fn with_root_communication(body: String, session_id: &str) -> String {
    let run_id = format!("run-{session_id}");
    let mut stream = format!(
        "data: {}\n\n",
        serde_json::json!({
            "type": "session_info", "session_id": session_id, "run_id": run_id,
        })
    );
    for (observed_run, direction, kind) in [
        (run_id.as_str(), "received", "text"),
        (run_id.as_str(), "received", "text"), // transport replay
        ("child-run", "received", "text"),
        (run_id.as_str(), "sent", "text"),
        (run_id.as_str(), "received", "progress"),
    ] {
        stream.push_str(&format!(
            "data: {}\n\n",
            serde_json::json!({
                "type": "agent_communication", "schema_version": "astra.agent_communication.v1",
                "observed_by": {"run_id": observed_run, "agent_id": "root"},
                "direction": direction, "message_id": "coordination-1",
                "from": {"run_id": "child-run", "agent_id": "worker"},
                "to": {"kind": "parent"}, "payload_kind": kind,
                "summary": "The requested checkpoint is ready", "timestamp_ms": 42,
            })
        ));
    }
    stream + &body
}

pub(super) fn sse_text_response_with_execution_summary(
    text: &str,
    session_id: &str,
    tool_calls_count: u32,
    observation_tool_calls_count: u32,
    tools_used: &[&str],
    llm_rounds: u32,
    server_turn: (&str, u32),
) -> String {
    let (run_id, session_turn) = server_turn;
    let tool_ledger_receipt = astra_turn_core::tool_ledger_receipt::ToolLedgerReceipt::new(
        run_id,
        1,
        tool_calls_count,
        tool_calls_count,
        0,
        astra_turn_core::tool_ledger_receipt::ToolLedgerResultClassCounts {
            succeeded: tool_calls_count,
            ..Default::default()
        },
        u64::from(tool_calls_count),
        astra_turn_core::tool_ledger_receipt::EMPTY_TOOL_LEDGER_ROOT,
        true,
    );
    format!(
        "data: {{\"type\":\"session_info\",\"session_id\":\"{session_id}\",\"run_id\":\"{run_id}\"}}\n\n\
             data: {{\"type\":\"text_delta\",\"content\":\"{text}\"}}\n\n\
             data: {{\"type\":\"text_done\",\"full_text\":\"{text}\"}}\n\n\
             data: {{\"type\":\"usage\",\"input_tokens\":10,\"output_tokens\":5}}\n\n\
             data: {{\"type\":\"run_finished\",\"run_id\":\"{run_id}\",\"status\":\"completed\",\"owner_generation\":1}}\n\n\
             data: {}\n\n\
             data: [DONE]\n\n",
        serde_json::json!({
            "type": "turn_complete",
            "has_tool_calls": tool_calls_count > 0,
            "continuation_owner": "server",
            "tool_calls_count": tool_calls_count,
            "observation_tool_calls_count": observation_tool_calls_count,
            "tools_used": tools_used,
            "llm_rounds": llm_rounds,
            "tool_ledger_receipt": tool_ledger_receipt,
            "runtime_feedback": {
                "schema_version": astra_turn_core::context_feedback::RuntimeFeedbackFrame::SCHEMA_VERSION,
                "identity": {
                    "session_id": session_id,
                    "run_id": run_id,
                    "agent_id": "root",
                    "model_id": "mock-model",
                    "topology": "cli_server"
                },
                "progress": {
                    "session_turn": session_turn,
                    "agentic_round_index": llm_rounds.saturating_sub(1),
                    "llm_rounds_completed": llm_rounds,
                    "slice_round_limit": 60,
                    "slice_rounds_remaining": 60u32.saturating_sub(llm_rounds)
                },
                "context": { "compaction_tier": "normal" },
                "request_usage": {
                    "prompt": 10,
                    "cache_read": 0,
                    "cache_creation": 0,
                    "completion": 5
                },
                "run_usage": {
                    "prompt": 10,
                    "cache_read": 0,
                    "cache_creation": 0,
                    "completion": 5
                },
                "was_truncated": false,
                "policy_feedback": { "state": "not_evaluated" }
            }
        })
    )
}

#[tokio::test]
async fn stream_chat_sse_sends_active_work_as_authoritative_server_context() {
    let captured_request = std::sync::Arc::new(std::sync::Mutex::new(None));
    let captured_request_for_route = captured_request.clone();
    let app = Router::new().route(
        "/chat/stream",
        post(move |Json(payload): Json<serde_json::Value>| {
            let captured_request = captured_request_for_route.clone();
            async move {
                *captured_request.lock().unwrap() = Some(payload);
                (
                    TEST_SSE_HEADERS,
                    sse_text_response(
                        "All three agents completed. Here is the consolidated report.",
                        "sess-active-fanout",
                    ),
                )
            }
        }),
    );
    let base = spawn_mock(app).await;
    let api = astra_thin_client::ThinClient::new(&base, None).unwrap();
    let unified_skill_registry = astra_runtime::skills::empty_unified_registry().clone();
    let mcp_manager = std::sync::Arc::new(tokio::sync::RwLock::new(
        crate::mcp_client::McpClientManager::new(),
    ));
    let mut cli_context = crate::cli::cli_config::cli_context::CliContext::default();
    cli_context.select_model(Some("test-model"));
    let mut context = BasicCliChatContext {
        mcp_manager: Some(mcp_manager.clone()),
        api: &api,
        auth_owner: None,
        message: "What is still running?",
        offering_id: None,
        model: Some("test-model"),
        provider: None,
        explain: ExplainMode::Off,
        runtime_config: std::sync::Arc::new(astra_config::RuntimeConfig::default()),
        render_md: false,
        verbose_mode: false,
        render_policy: crate::cli::stream::stream_render::RenderPolicy::Silent,
        cli_context: Some(&cli_context),
        unified_skill_registry: &unified_skill_registry,
        agent_spawner: None,
        root_agent_id: None,
        bg_task_commands: None,
        bg_task_list_cache: None,
        bash_detach_slot: None,
        stream_event_tx: None,
        stream_json_emitter: None,
        #[cfg(feature = "harness")]
        harness_sink: None,

        #[cfg(feature = "harness")]
        benchmark_profile: None,
    };
    let observations = vec![
        astra_core::work_unit::WorkUnitObservation::new(
            "review-group",
            "agent_fanout",
            astra_core::work_unit::WorkUnitStatus::Running,
            7,
            astra_core::work_unit::WorkUnitObservationMode::Current,
        )
        .unwrap()
        .with_wake_policy(astra_core::work_unit::WorkUnitWakePolicy::OnTerminal),
    ];
    let mut permission_manager = PermissionManager::new(true);

    let mut params = ChatTurnParams::basic_cli(
        &context,
        "fake-token",
        Some("sess-active-fanout"),
        &mut permission_manager,
    );
    params.input_work_unit_observations = &observations;
    assert!(std::sync::Arc::ptr_eq(
        params
            .mcp_manager
            .as_ref()
            .expect("basic CLI must retain MCP bindings"),
        &mcp_manager,
    ));

    let result = stream_chat_sse(params).await.unwrap();

    assert!(
        result
            .full_text
            .contains("All three agents completed. Here is the consolidated report.")
    );
    assert_eq!(
        result.full_text, "All three agents completed. Here is the consolidated report.",
        "the Edge client must not rewrite Server-owned completion text"
    );

    let request = captured_request.lock().unwrap().clone().unwrap();
    assert_eq!(
        request["requested_model_policy"],
        serde_json::json!({
            "mode": "fixed", "selector": { "kind": "offering_id",
                "offering_id": request["model_selection"]["offering_id"] }
        })
    );
    let injections = request["context"]["edge_profile"]
        [astra_turn_core::chat_turn_edge_profile::EDGE_PROFILE_KEY_RUNTIME_VOLATILE_INJECTIONS]
        .as_array()
        .expect("typed runtime injection lane");
    let active_work = injections
        .iter()
        .find(|injection| injection["kind"] == "active_work_snapshot")
        .expect("active work snapshot reaches the model boundary");
    assert_eq!(active_work["delivery_class"], "required_context");
    assert_eq!(active_work["payload"]["authority"], "runtime_producer");
    assert_eq!(
        active_work["payload"]["work_unit_observations"][0]["id"],
        "review-group"
    );
    assert_eq!(
        active_work["payload"]["work_unit_observations"][0]["status"],
        "running"
    );

    // Ordinary follow-up input retains the active Work context without
    // requiring the user to repeat it.
    context.message = "Continue with the same work";
    let follow_up = ChatTurnParams::basic_cli(
        &context,
        "fake-token",
        Some("sess-active-fanout"),
        &mut permission_manager,
    );
    stream_chat_sse(follow_up).await.unwrap();
    let follow_up = captured_request.lock().unwrap().clone().unwrap();
    assert_eq!(request["session_id"], "sess-active-fanout");
    assert_eq!(follow_up["session_id"], request["session_id"]);
}

/// Exercise the real SSE adapter and ordered TUI bridge together. Injecting
/// RunInterrupted directly into a widget would miss the notice-only regression.
#[tokio::test(flavor = "current_thread")]
#[serial_test::serial]
async fn stream_chat_sse_projects_rate_limit_notice_without_answer_deltas() {
    use crate::tui::{TuiAppEvent, create_channels, create_controlled_per_turn_bridge};
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    let temp = tempfile::tempdir().unwrap();
    let _guard = ProcessJournalDirGuard::new(temp.path());
    for (text, tools, interrupted) in [
        ("", 2, true),
        ("", 0, true),
        ("Partial findings from the tools.", 2, true),
        ("The requested work is complete.", 2, false),
    ] {
        let notice = format!(
            "The model provider is temporarily rate limited. {tools} tool call(s) completed. Please wait ~30s before retrying."
        );
        let session_id = format!("sess-rate-limit-{tools}-{}-{interrupted}", text.len());
        let run_id = format!("run-{session_id}");
        let tools_used: &[&str] = if tools == 0 { &[] } else { &["introspect"] };
        let source = sse_text_response_with_execution_summary(
            text,
            &session_id,
            tools,
            tools,
            tools_used,
            2,
            (&run_id, 1),
        );
        let mut body = String::new();
        for block in source.split("\n\n") {
            let Some(data) = block.strip_prefix("data: ") else {
                continue;
            };
            if data == "[DONE]" {
                body.push_str("data: [DONE]\n\n");
                continue;
            }
            let mut event: serde_json::Value = serde_json::from_str(data).unwrap();
            let event_type = event["type"].as_str().unwrap().to_string();
            if text.is_empty() && matches!(event_type.as_str(), "text_delta" | "text_done") {
                continue;
            }
            if interrupted && event_type == "run_finished" {
                body.push_str(&format!(
                    "data: {}\n\n",
                    serde_json::json!({
                        "type": "run_interrupted", "run_id": run_id,
                        "kind": "rate_limited", "resumable": true,
                        "message": notice,
                    })
                ));
                event["status"] = serde_json::json!("paused");
            }
            if interrupted && event_type == "turn_complete" {
                event["execution_state"] = serde_json::json!({
                    "status": "interrupted", "interruption_kind": "rate_limited",
                });
                event["interruption"] = serde_json::json!({
                    "kind": "rate_limited", "resumable": true,
                    "resume_action": {"wait_and_retry": {"delay_seconds": 30}},
                    "user_message": notice, "has_checkpoint": false,
                    "tool_calls_completed": tools, "turns_completed": 2,
                    "remaining_turns": 22,
                    "error_detail": "synthetic private provider diagnostic",
                });
            }
            body.push_str(&format!("data: {event}\n\n"));
            if event_type == "session_info" {
                for index in 0..tools {
                    for tool_event in [
                        serde_json::json!({
                            "type": "tool_call_start", "call_id": format!("call-{index}"),
                            "tool": "introspect", "arguments": {"scope": "current_turn"},
                            "transport": "server_local", "executor": {"kind": "server_local"},
                        }),
                        serde_json::json!({
                            "type": "tool_call_end", "call_id": format!("call-{index}"),
                            "tool": "introspect", "status": "ok", "duration_ms": 10,
                            "result": format!("completed result {index}"),
                            "transport": "server_local", "executor": {"kind": "server_local"},
                        }),
                    ] {
                        body.push_str(&format!("data: {tool_event}\n\n"));
                    }
                }
            }
        }

        let requests = Arc::new(AtomicUsize::new(0));
        let observed = requests.clone();
        let app = Router::new().route(
            "/chat/stream",
            post(move || {
                observed.fetch_add(1, Ordering::SeqCst);
                let body = body.clone();
                async move { (TEST_SSE_HEADERS, body) }
            }),
        );
        let base = spawn_mock(app).await;
        let api = astra_thin_client::ThinClient::new(&base, None).unwrap();
        let registry = astra_runtime::skills::empty_unified_registry();
        let (tui_tx, mut tui_rx) = create_channels();
        let (stream_tx, control) = create_controlled_per_turn_bridge(tui_tx);
        let mut context = basic_chat_context(&api, &registry, "Inspect the current turn");
        context.stream_event_tx = Some(stream_tx);
        let mut pm = PermissionManager::new(true);
        let result = stream_chat_sse(ChatTurnParams::basic_cli(
            &context,
            "synthetic-token",
            Some(&session_id),
            &mut pm,
        ))
        .await
        .unwrap();
        control.close_and_drain();

        let mut notices = 0;
        let mut answer = String::new();
        let mut completed_tools = Vec::new();
        loop {
            let event = tokio::time::timeout(std::time::Duration::from_secs(2), tui_rx.recv())
                .await
                .expect("interrupted stream must drain")
                .expect("TUI channel must stay open until the drain marker");
            match event {
                TuiAppEvent::RunInterrupted { user_message } => {
                    assert_eq!(user_message, notice);
                    notices += 1;
                }
                TuiAppEvent::Token { text, .. } => answer.push_str(&text),
                TuiAppEvent::ToolCompleted {
                    tool_use_id,
                    status,
                    output,
                    ..
                } => completed_tools.push((tool_use_id, status, output)),
                TuiAppEvent::TurnProjectionDrained => break,
                _ => {}
            }
        }
        assert_eq!(requests.load(Ordering::SeqCst), 1, "no automatic replay");
        assert_eq!(result.tool_calls_count, tools);
        assert_eq!(
            result.final_state,
            if interrupted {
                "interrupted"
            } else {
                "completed"
            }
        );
        assert_eq!(notices, usize::from(interrupted));
        assert_eq!(answer, text, "no synthetic answer or repeated partial text");
        assert_eq!(completed_tools.len(), tools as usize);
        for index in 0..tools {
            assert_eq!(
                completed_tools[index as usize],
                (
                    format!("call-{index}"),
                    "completed".into(),
                    Some(format!("completed result {index}"))
                )
            );
        }
        if interrupted {
            let interruption = result.interruption.as_ref().unwrap();
            assert_eq!(interruption["kind"], "rate_limited");
            assert_eq!(interruption["resumable"], true);
            assert_eq!(
                interruption["resume_action"]["wait_and_retry"]["delay_seconds"],
                30
            );
            assert_eq!(
                result.full_text,
                if text.is_empty() {
                    notice.as_str()
                } else {
                    text
                }
            );
        }
    }
}

fn mock_mcp_server_binary() -> std::path::PathBuf {
    crate::mcp_client::ensure_mock_mcp_server_binary()
}

#[tokio::test(flavor = "current_thread")]
#[serial_test::serial]
async fn stream_chat_sse_late_binds_fresh_request_then_persists_canonical_turn() {
    let temp = tempfile::tempdir().unwrap();
    let _guard = ProcessJournalDirGuard::new(temp.path());
    let app = Router::new().route(
        "/chat/stream",
        post(|| async {
            (
                TEST_SSE_HEADERS,
                with_root_communication(
                    format!("data: {}\n\n{}", serde_json::json!({"type":"context_meta", "compactions":[{
                        "id":"server-compact-1", "kind":"wire_assembly", "tier":"compact_history",
                        "messages_before":8, "messages_after":4, "tokens_before":5000,
                        "tokens_after":3000, "tokens_saved":2000
                    }]}), sse_text_response("Hello!", "sess-step-adopt")),
                    "sess-step-adopt",
                ),
            )
        }),
    );
    let base = spawn_mock(app).await;
    let api = astra_thin_client::ThinClient::new(&base, None).unwrap();
    let mut pm = PermissionManager::new(true);

    let request_lease =
        crate::cli::session::session_execution_lease::RequestSessionExecutionLease::new(None)
            .unwrap();
    let turn_start = std::time::Instant::now();
    let (event_tx, mut event_rx) = crate::cli::chat_stream::stream_event_channel();

    let mut result = stream_chat_sse(ChatTurnParams {
        api: &api,
        token: "fake-token",
        auth_owner: None,
        message: "hi",
        user_intent: "hi",
        input_runtime_required_texts: &[],
        input_runtime_volatile_texts: &[],
        input_work_unit_observations: &[],
        semantic_query_override: None,
        session_id: None,
        offering_id: None,
        model: Some("test-model"),
        provider: None,
        explain: ExplainMode::Off,
        runtime_config: std::sync::Arc::new(astra_config::RuntimeConfig::default()),
        render_md: false,
        history: &[],
        perm_manager: &mut pm,
        verbose_mode: false,
        render_policy: crate::cli::stream::stream_render::RenderPolicy::Silent,
        cli_context: None,
        recent_tools: &[],
        deferred_tool_activations: None,
        resume_restricted_tools: &[],
        tool_health_entries: &[],
        workspace_observation_quarantine: None,
        session_lessons: &[],
        memory_selection_reports: &[],

        latest_turn_quality_feedback: None,
        unified_skill_registry: astra_runtime::skills::empty_unified_registry(),
        is_plan_subtask: false,
        plan_subtask_id: None,
        cancel_token: None,
        execution_time_budget: None,
        run_control: None,
        incremental_state: None,
        executor_observer: None,
        request_session_execution_lease: Some(request_lease.clone()),
        plan_assemble_line_release: None,
        stream_event_tx: Some(event_tx),
        explain_analyze_terminal_degraded: None,
        stream_json_emitter: None,
        agent_live_event_sink: None,
        approval_request_tx: None,
        ask_user_request_tx: None,
        plan_review_request_tx: None,
        mcp_manager: None,

        agent_spawner: None,
        root_agent_id: None,
        observability_hub: None,
        observability_session: None,
        file_journal: None,
        file_state: None,
        database_snapshot_journal: None,

        git_worktree_journal: None,
        session_state_journal: None,
        bg_task_commands: None,
        bg_task_list_cache: None,
        bash_detach_slot: None,
        turn_index: DEFAULT_TURN_INDEX,
        pipeline_state: None,
        compaction_state: None,
        consecutive_context_window_errors: 0,
        idempotency_cache: None,
        pre_loaded_messages: None,
        append_system_prompt: None,
        #[cfg(feature = "harness")]
        harness_sink: None,

        #[cfg(feature = "harness")]
        benchmark_profile: None,
    })
    .await
    .unwrap();

    assert_eq!(result.session_id.as_deref(), Some("sess-step-adopt"));
    let trace = &result
        .pending_context_assembly_trace
        .as_ref()
        .expect("shared measured trace")
        .1;
    assert_eq!(trace["token_budget"]["compression_triggered"], true);
    let mut observed_compactions = Vec::new();
    while let Ok(event) = event_rx.try_recv() {
        if let crate::cli::chat_stream::StreamEvent::Compaction(event) = event {
            observed_compactions.push(event);
        }
    }
    assert_eq!(observed_compactions.len(), 1);
    assert_eq!(observed_compactions[0].tokens_freed, 2000);
    // A later physical exchange/recovery may repeat an already observed C1.
    // Conversion must deduplicate evidence, not ordinary equal-text messages.
    let replay = result
        .run_transcript_messages
        .iter()
        .find(|message| message.get("evidence").is_some())
        .unwrap()
        .clone();
    result.run_transcript_messages.push(replay);
    assert!(
        astra_services::session_journal::SessionExecutionLease::try_acquire("sess-step-adopt")
            .is_err(),
        "the first accepted Server identity must bind before stream completion"
    );
    let exit_code = crate::cli::command_router::finalize_one_shot_stream_result_with_request_lease(
        None,
        Some("test-model"),
        "hi",
        &mut result,
        turn_start,
        request_lease.as_ref(),
    );
    assert_eq!(exit_code, crate::cli::exit_code::ExitCode::Success);
    assert_eq!(result.session_persistence_error, None);
    let restored =
        crate::cli::session::session_continuation::load_session_messages_for_continuation(
            "sess-step-adopt",
        )
        .expect("late-bound canonical continuation");
    assert_eq!(restored.last().unwrap()["content"], "Hello!");
    assert!(restored.iter().all(|message| message["role"] != "event"));
    let evidence = session_journal::read_journal("sess-step-adopt")
        .unwrap()
        .into_iter()
        .filter_map(|event| event.transcript_item)
        .filter(|item| item.message.get("evidence").is_some())
        .collect::<Vec<_>>();
    assert_eq!(evidence.len(), 1);
    assert_eq!(evidence[0].run_id, "run-sess-step-adopt");
    assert_eq!(evidence[0].agent_id, "root");
    let observed = &evidence[0].message["evidence"]["event"];
    assert_eq!(
        observed["observed_by"],
        serde_json::json!({"run_id":"run-sess-step-adopt","agent_id":"root"})
    );
    assert_eq!(
        observed["from"],
        serde_json::json!({"run_id":"child-run","agent_id":"worker"})
    );
    assert_eq!(observed["to"], serde_json::json!({"kind":"parent"}));
    assert_eq!(observed["direction"], "received");
    assert_eq!(observed["payload_kind"], "text");
    assert_eq!(
        evidence[0].message["evidence"]["event"]["message_id"],
        "coordination-1"
    );

    let cli_user_id = crate::cli::cli_config::cli_utils::cli_user_id();
    let store =
        astra_pipeline::step_checkpoint::FileBackedEventStore::new(&cli_user_id, "sess-step-adopt");
    let events = store.all_events();
    assert!(
        !events.is_empty(),
        "step events should persist under adopted session"
    );
    assert!(
        events
            .iter()
            .any(|event| event.step_id.starts_with("sess-step-adopt-run-")
                && event.step_id.ends_with("-turn-1-step-0")),
        "expected a run-scoped first-turn step identity, found: {:?}",
        events.iter().map(|e| &e.step_id).collect::<Vec<_>>()
    );
    let ephemeral_store =
        astra_pipeline::step_checkpoint::FileBackedEventStore::new(&cli_user_id, "ephemeral");
    assert!(
        ephemeral_store.all_events().is_empty(),
        "new-session first turn must not persist step events under ephemeral/"
    );
}

#[tokio::test]
async fn stream_chat_sse_simple_text_response() {
    let app = Router::new().route(
        "/chat/stream",
        post(|| async { (TEST_SSE_HEADERS, sse_text_response("Hello!", "sess-001")) }),
    );
    let base = spawn_mock(app).await;
    let api = astra_thin_client::ThinClient::new(&base, None).unwrap();
    let mut pm = PermissionManager::new(true);

    let result = stream_chat_sse(ChatTurnParams {
        api: &api,
        token: "fake-token",
        auth_owner: None,
        message: "hi",
        user_intent: "hi",
        input_runtime_required_texts: &[],
        input_runtime_volatile_texts: &[],
        input_work_unit_observations: &[],
        semantic_query_override: None,
        session_id: None,
        offering_id: None,
        model: Some("test-model"),
        provider: None,
        explain: ExplainMode::Off,
        runtime_config: std::sync::Arc::new(astra_config::RuntimeConfig::default()),
        render_md: false,
        history: &[],
        perm_manager: &mut pm,
        verbose_mode: false,
        render_policy: crate::cli::stream::stream_render::RenderPolicy::Silent,
        cli_context: None,
        recent_tools: &[],
        deferred_tool_activations: None,
        resume_restricted_tools: &[],
        tool_health_entries: &[],
        workspace_observation_quarantine: None,
        session_lessons: &[],
        memory_selection_reports: &[],

        latest_turn_quality_feedback: None,
        unified_skill_registry: astra_runtime::skills::empty_unified_registry(),
        is_plan_subtask: false,
        plan_subtask_id: None,
        cancel_token: None,
        execution_time_budget: None,
        run_control: None,
        incremental_state: None,
        executor_observer: None,
        request_session_execution_lease: None,
        plan_assemble_line_release: None,
        stream_event_tx: None,
        explain_analyze_terminal_degraded: None,
        stream_json_emitter: None,
        agent_live_event_sink: None,
        approval_request_tx: None,
        ask_user_request_tx: None,
        plan_review_request_tx: None,
        mcp_manager: None,

        agent_spawner: None,
        root_agent_id: None,
        observability_hub: None,
        observability_session: None,
        file_journal: None,
        file_state: None,
        database_snapshot_journal: None,

        git_worktree_journal: None,
        session_state_journal: None,
        bg_task_commands: None,
        bg_task_list_cache: None,
        bash_detach_slot: None,
        turn_index: DEFAULT_TURN_INDEX,
        pipeline_state: None,
        compaction_state: None,
        consecutive_context_window_errors: 0,
        idempotency_cache: None,
        pre_loaded_messages: None,
        append_system_prompt: None,
        #[cfg(feature = "harness")]
        harness_sink: None,

        #[cfg(feature = "harness")]
        benchmark_profile: None,
    })
    .await
    .unwrap();
    assert_eq!(result.full_text, "Hello!");
    assert_eq!(result.session_id.as_deref(), Some("sess-001"));
    assert_eq!(result.prompt_tokens, 10);
    assert_eq!(result.completion_tokens, 5);
}

#[tokio::test]
async fn stream_chat_sse_preserves_existing_session_id_for_server_scoped_trace() {
    use futures_util::StreamExt;

    #[derive(Clone)]
    struct MockState {
        turn_payloads: std::sync::Arc<tokio::sync::Mutex<Vec<serde_json::Value>>>,
        finish: std::sync::Arc<tokio::sync::Notify>,
    }

    let state = MockState {
        turn_payloads: std::sync::Arc::new(tokio::sync::Mutex::new(Vec::new())),
        finish: std::sync::Arc::new(tokio::sync::Notify::new()),
    };
    let app = Router::new().route(
        "/chat/stream",
        post({
            let state = state.clone();
            move |axum::Json(body): axum::Json<serde_json::Value>| {
                let state = state.clone();
                async move {
                    state.turn_payloads.lock().await.push(body);
                    let response = sse_text_response("Hello!", "sess-traced");
                    let terminal = response.find("data: {\"type\":\"text_done\"").unwrap();
                    let first_frame = response.split_once("\n\n").unwrap().0;
                    let early = format!("{first_frame}\n\n{}", &response[..terminal]);
                    let final_frames = response[terminal..].to_owned();
                    let body = futures_util::stream::iter([Ok::<_, std::io::Error>(early)]).chain(
                        futures_util::stream::once(async move {
                            state.finish.notified().await;
                            Ok(final_frames)
                        }),
                    );
                    (TEST_SSE_HEADERS, axum::body::Body::from_stream(body))
                }
            }
        }),
    );
    let base = spawn_mock(app).await;
    let api = astra_thin_client::ThinClient::new(&base, None).unwrap();
    let mut pm = PermissionManager::new(true);

    let (event_tx, mut event_rx) = crate::cli::chat_stream::stream_event_channel();
    let turn = stream_chat_sse(ChatTurnParams {
        api: &api,
        token: "fake-token",
        auth_owner: None,
        message: "hi",
        user_intent: "hi",
        input_runtime_required_texts: &[],
        input_runtime_volatile_texts: &[],
        input_work_unit_observations: &[],
        semantic_query_override: None,
        session_id: Some("sess-traced"),
        offering_id: None,
        model: Some("test-model"),
        provider: None,
        explain: ExplainMode::Off,
        runtime_config: std::sync::Arc::new(astra_config::RuntimeConfig::default()),
        render_md: false,
        history: &[],
        perm_manager: &mut pm,
        verbose_mode: false,
        render_policy: crate::cli::stream::stream_render::RenderPolicy::Silent,
        cli_context: None,
        recent_tools: &[],
        deferred_tool_activations: None,
        resume_restricted_tools: &[],
        tool_health_entries: &[],
        workspace_observation_quarantine: None,
        session_lessons: &[],
        memory_selection_reports: &[],

        latest_turn_quality_feedback: None,
        unified_skill_registry: astra_runtime::skills::empty_unified_registry(),
        is_plan_subtask: false,
        plan_subtask_id: None,
        cancel_token: None,
        execution_time_budget: None,
        run_control: None,
        incremental_state: None,
        executor_observer: None,
        request_session_execution_lease: None,
        plan_assemble_line_release: None,
        stream_event_tx: Some(event_tx),
        explain_analyze_terminal_degraded: None,
        stream_json_emitter: None,
        agent_live_event_sink: None,
        approval_request_tx: None,
        ask_user_request_tx: None,
        plan_review_request_tx: None,
        mcp_manager: None,

        agent_spawner: None,
        root_agent_id: None,
        observability_hub: None,
        observability_session: None,
        file_journal: None,
        file_state: None,
        database_snapshot_journal: None,

        git_worktree_journal: None,
        session_state_journal: None,
        bg_task_commands: None,
        bg_task_list_cache: None,
        bash_detach_slot: None,
        turn_index: DEFAULT_TURN_INDEX,
        pipeline_state: None,
        compaction_state: None,
        consecutive_context_window_errors: 0,
        idempotency_cache: None,
        pre_loaded_messages: None,
        append_system_prompt: None,
        #[cfg(feature = "harness")]
        harness_sink: None,

        #[cfg(feature = "harness")]
        benchmark_profile: None,
    });
    let observe_binding = async {
        let observed = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            let mut bindings = Vec::new();
            while let Some(event) = event_rx.recv().await {
                match event {
                    crate::cli::chat_stream::StreamEvent::SessionBound(id) => {
                        bindings.push(("session", id))
                    }
                    crate::cli::chat_stream::StreamEvent::RunBound(id) => {
                        bindings.push(("run", id))
                    }
                    crate::cli::chat_stream::StreamEvent::Token { .. } => return bindings,
                    _ => {}
                }
            }
            panic!("stream closed before the first text");
        })
        .await;
        state.finish.notify_one();
        observed.expect("bindings and first text must be visible before the terminal frames")
    };
    let (result, bindings) = tokio::join!(turn, observe_binding);
    let result = result.unwrap();
    assert_eq!(result.session_id.as_deref(), Some("sess-traced"));
    assert_eq!(
        bindings,
        vec![
            ("session", "sess-traced".into()),
            ("run", "run-sess-traced".into()),
        ],
        "duplicate wire frames must publish the resumed binding once before completion"
    );

    let payloads = state.turn_payloads.lock().await;
    assert_eq!(payloads.len(), 1);
    assert_eq!(payloads[0]["session_id"], serde_json::json!("sess-traced"));
}

#[tokio::test]
async fn stream_chat_sse_preserves_server_rounds_without_a_local_spawner() {
    let admissions = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
    let admissions_for_route = admissions.clone();
    let app = Router::new().route(
        "/chat/stream",
        post(move || {
            let admissions = admissions_for_route.clone();
            async move {
                admissions.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                (
                    TEST_SSE_HEADERS,
                    sse_text_response_with_execution_summary(
                        "Server completed the turn",
                        "sess-server-loop",
                        3,
                        2,
                        &["agent", "tool_search"],
                        4,
                        ("run-sess-server-loop", 1),
                    ),
                )
            }
        }),
    );
    let base = spawn_mock(app).await;
    let api = astra_thin_client::ThinClient::new(&base, None).unwrap();
    let unified_skill_registry = astra_runtime::skills::empty_unified_registry().clone();
    let mut pm = PermissionManager::new(true);

    let result = tokio::time::timeout(
        std::time::Duration::from_secs(20),
        stream_chat_sse(ChatTurnParams {
            api: &api,
            token: "fake-token",
            auth_owner: None,
            message: "delegate this work",
            user_intent: "delegate this work",
            input_runtime_required_texts: &[],
            input_runtime_volatile_texts: &[],
            input_work_unit_observations: &[],
            semantic_query_override: None,
            session_id: None,
            offering_id: None,
            model: Some("mock-model"),
            provider: None,
            explain: ExplainMode::Off,
            runtime_config: std::sync::Arc::new(astra_config::RuntimeConfig {
                runtime_limits: astra_config::runtime_config::RuntimeLimitsConfig {
                    max_turns: 1,
                    ..Default::default()
                },
                ..Default::default()
            }),
            render_md: false,
            history: &[],
            perm_manager: &mut pm,
            verbose_mode: false,
            render_policy: crate::cli::stream::stream_render::RenderPolicy::Silent,
            cli_context: None,
            recent_tools: &[],
            deferred_tool_activations: None,
            resume_restricted_tools: &[],
            tool_health_entries: &[],
            workspace_observation_quarantine: None,
            session_lessons: &[],
            memory_selection_reports: &[],

            latest_turn_quality_feedback: None,
            unified_skill_registry: &unified_skill_registry,
            is_plan_subtask: false,
            plan_subtask_id: None,
            cancel_token: None,
            execution_time_budget: None,
            run_control: None,
            incremental_state: None,
            executor_observer: None,
            request_session_execution_lease: None,
            plan_assemble_line_release: None,
            stream_event_tx: None,
            explain_analyze_terminal_degraded: None,
            stream_json_emitter: None,
            agent_live_event_sink: None,
            approval_request_tx: None,
            ask_user_request_tx: None,
            plan_review_request_tx: None,
            mcp_manager: None,

            agent_spawner: None,
            root_agent_id: None,
            observability_hub: None,
            observability_session: None,
            file_journal: None,
            file_state: None,
            database_snapshot_journal: None,

            git_worktree_journal: None,
            session_state_journal: None,
            bg_task_commands: None,
            bg_task_list_cache: None,
            bash_detach_slot: None,
            turn_index: DEFAULT_TURN_INDEX,
            pipeline_state: None,
            compaction_state: None,
            consecutive_context_window_errors: 0,
            idempotency_cache: None,
            pre_loaded_messages: None,
            append_system_prompt: None,
            #[cfg(feature = "harness")]
            harness_sink: None,

            #[cfg(feature = "harness")]
            benchmark_profile: None,
        }),
    )
    .await
    .expect("agent spawn turn should not hang")
    .expect("agent spawn turn should complete");

    assert_eq!(result.full_text, "Server completed the turn");
    assert_eq!(result.tool_calls_count, 3);
    assert_eq!(result.tools_used, ["agent", "tool_search"]);
    assert_eq!(result.llm_rounds, Some(4));
    assert_eq!(
        admissions.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "one Server-owned turn must use one admission"
    );
}

#[tokio::test]
async fn stream_chat_sse_api_error_propagated() {
    let app = Router::new().route(
        "/chat/stream",
        post(|| async {
            (
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                axum::Json(serde_json::json!({"detail": "model overloaded"})),
            )
        }),
    );
    let base = spawn_mock(app).await;
    let api = astra_thin_client::ThinClient::new(&base, None).unwrap();
    let mut pm = PermissionManager::new(true);

    let result = stream_chat_sse(ChatTurnParams {
        api: &api,
        token: "fake-token",
        auth_owner: None,
        message: "hi",
        user_intent: "hi",
        input_runtime_required_texts: &[],
        input_runtime_volatile_texts: &[],
        input_work_unit_observations: &[],
        semantic_query_override: None,
        session_id: None,
        offering_id: None,
        model: Some("test-model"),
        provider: None,
        explain: ExplainMode::Off,
        runtime_config: std::sync::Arc::new(astra_config::RuntimeConfig::default()),
        render_md: false,
        history: &[],
        perm_manager: &mut pm,
        verbose_mode: false,
        render_policy: crate::cli::stream::stream_render::RenderPolicy::Silent,
        cli_context: None,
        recent_tools: &[],
        deferred_tool_activations: None,
        resume_restricted_tools: &[],
        tool_health_entries: &[],
        workspace_observation_quarantine: None,
        session_lessons: &[],
        memory_selection_reports: &[],

        latest_turn_quality_feedback: None,
        unified_skill_registry: astra_runtime::skills::empty_unified_registry(),
        is_plan_subtask: false,
        plan_subtask_id: None,
        cancel_token: None,
        execution_time_budget: None,
        run_control: None,
        incremental_state: None,
        executor_observer: None,
        request_session_execution_lease: None,
        plan_assemble_line_release: None,
        stream_event_tx: None,
        explain_analyze_terminal_degraded: None,
        stream_json_emitter: None,
        agent_live_event_sink: None,
        approval_request_tx: None,
        ask_user_request_tx: None,
        plan_review_request_tx: None,
        mcp_manager: None,

        agent_spawner: None,
        root_agent_id: None,
        observability_hub: None,
        observability_session: None,
        file_journal: None,
        file_state: None,
        database_snapshot_journal: None,

        git_worktree_journal: None,
        session_state_journal: None,
        bg_task_commands: None,
        bg_task_list_cache: None,
        bash_detach_slot: None,
        turn_index: DEFAULT_TURN_INDEX,
        pipeline_state: None,
        compaction_state: None,
        consecutive_context_window_errors: 0,
        idempotency_cache: None,
        pre_loaded_messages: None,
        append_system_prompt: None,
        #[cfg(feature = "harness")]
        harness_sink: None,

        #[cfg(feature = "harness")]
        benchmark_profile: None,
    })
    .await;
    assert!(result.is_err());
    let failure = result.unwrap_err();
    assert!(failure.error.contains("500"), "got: {}", failure.error);
}

#[tokio::test]
async fn stream_chat_sse_rejects_client_tool_continuation() {
    // Neither text syntax nor a malformed typed continuation authorizes admission.
    for native_tool_call in [false, true] {
        let partial_text = "Observed partial response\n<invoke name=\"introspect\"/>";
        let call_count = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
        let cc = call_count.clone();
        let app = Router::new().route(
            "/chat/stream",
            post(move || {
                let cc = cc.clone();
                async move {
                    cc.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    let native_event = if native_tool_call {
                        "data: {\"type\":\"tool_call\",\"id\":\"tc-1\",\"name\":\"bash\",\"arguments\":{\"command\":\"echo hi\"}}\n\n"
                    } else {
                        ""
                    };
                    let text_event = serde_json::json!({"type":"text_delta", "content":partial_text});
                    let body = format!(
                        "data: {{\"type\":\"session_info\",\"session_id\":\"sess-tc\",\"run_id\":\"run-sess-tc\"}}\n\n\
                         data: {text_event}\n\n\
                         data: {{\"type\":\"usage\",\"input_tokens\":10,\"output_tokens\":5}}\n\n\
                         {native_event}\
                         data: {{\"type\":\"turn_complete\",\"has_tool_calls\":{native_tool_call}}}\n\n\
                         data: [DONE]\n\n"
                    );
                    (
                        TEST_SSE_HEADERS,
                        with_root_communication(body, "sess-tc"),
                    )
                }
            }),
        );
        let base = spawn_mock(app).await;
        let api = astra_thin_client::ThinClient::new(&base, None).unwrap();
        let mut pm = PermissionManager::new(true); // auto-approve

        let result = stream_chat_sse(ChatTurnParams {
            api: &api,
            token: "fake-token",
            auth_owner: None,
            message: "run echo hi",
            user_intent: "run echo hi",
            input_runtime_required_texts: &[],
            input_runtime_volatile_texts: &[],
            input_work_unit_observations: &[],
            semantic_query_override: None,
            session_id: None,
            offering_id: None,
            model: Some("test-model"),
            provider: None,
            explain: ExplainMode::Off,
            runtime_config: std::sync::Arc::new(astra_config::RuntimeConfig::default()),
            render_md: false,
            history: &[],
            perm_manager: &mut pm,
            verbose_mode: false,
            render_policy: crate::cli::stream::stream_render::RenderPolicy::Silent,
            cli_context: None,
            recent_tools: &[],
            deferred_tool_activations: None,
            resume_restricted_tools: &[],
            tool_health_entries: &[],
            workspace_observation_quarantine: None,
            session_lessons: &[],
            memory_selection_reports: &[],

            latest_turn_quality_feedback: None,
            unified_skill_registry: astra_runtime::skills::empty_unified_registry(),
            is_plan_subtask: false,
            plan_subtask_id: None,
            cancel_token: None,
            execution_time_budget: None,
            run_control: None,
            incremental_state: None,
            executor_observer: None,
            request_session_execution_lease: None,
            plan_assemble_line_release: None,
            stream_event_tx: None,
            explain_analyze_terminal_degraded: None,
            stream_json_emitter: None,
            agent_live_event_sink: None,
            approval_request_tx: None,
            ask_user_request_tx: None,
            plan_review_request_tx: None,
            mcp_manager: None,

            agent_spawner: None,
            root_agent_id: None,
            observability_hub: None,
            observability_session: None,
            file_journal: None,
            file_state: None,
            database_snapshot_journal: None,

            git_worktree_journal: None,
            session_state_journal: None,
            bg_task_commands: None,
            bg_task_list_cache: None,
            bash_detach_slot: None,
            turn_index: DEFAULT_TURN_INDEX,
            pipeline_state: None,
            compaction_state: None,
            consecutive_context_window_errors: 0,
            idempotency_cache: None,
            pre_loaded_messages: None,
            append_system_prompt: None,
            #[cfg(feature = "harness")]
            harness_sink: None,

            #[cfg(feature = "harness")]
            benchmark_profile: None,
        })
        .await
        .expect_err("Server-owned streams cannot delegate continuation to the CLI");
        assert!(result.error.contains("terminal execution evidence"));
        assert_eq!(
            result.partial.partial_text,
            if native_tool_call {
                "Observed partial response"
            } else {
                partial_text
            }
        );
        assert_eq!(result.partial.prompt_tokens, 10);
        assert_eq!(result.partial.completion_tokens, 5);
        assert_eq!(result.partial.tool_calls_count, 0);
        let evidence = result
            .partial
            .run_transcript_messages
            .iter()
            .filter_map(|message| message.get("evidence"))
            .collect::<Vec<_>>();
        assert_eq!(evidence.len(), 1);
        assert_eq!(evidence[0]["event"]["observed_by"]["run_id"], "run-sess-tc");
        assert_eq!(call_count.load(std::sync::atomic::Ordering::SeqCst), 1);
    }
}

#[tokio::test(flavor = "current_thread")]
#[serial_test::serial]
async fn stream_chat_sse_journals_transaction_boundaries_end_to_end() {
    let temp = tempfile::tempdir().unwrap();
    let _guard = ProcessJournalDirGuard::new(temp.path());
    #[derive(Clone)]
    struct StreamingMockState {
        call_count: std::sync::Arc<std::sync::atomic::AtomicU32>,
        tool_results: std::sync::Arc<tokio::sync::Mutex<Vec<serde_json::Value>>>,
    }

    let state = StreamingMockState {
        call_count: std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0)),
        tool_results: std::sync::Arc::new(tokio::sync::Mutex::new(Vec::new())),
    };
    let app = Router::new()
        .route(
            "/chat/stream",
            post({
                let state = state.clone();
                move || {
                    let state = state.clone();
                    async move {
                        state
                            .call_count
                            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        let body = format!(
                                "data: {{\"type\":\"session_info\",\"session_id\":\"sess-tx-e2e\",\"run_id\":\"run-sess-tx-e2e\"}}\n\n\
                                 data: {}\n\n\
                                 {}",
                                serde_json::json!({
                                    "type": "tool_request",
                                    "session_id": "sess-tx-e2e",
                                    "run_id": "run-sess-tx-e2e",
                                    "turn_chain_id": "chain-tx-e2e",
                                    "request_id": "tr-tx-1",
                                    "schema_admitted_by_server": true,
                                    "execution_timeout_ms": 300_000, "command_timeout_cap_ms": 30_000,
                                    "execution_deadline_unix_ms": 4_102_444_800_000_u64,
                                    "tool": "bash",
                                    "args": {
                                        "command": "echo hi",
                                        "transaction_id": "tx-e2e",
                                        "rollback_on_failure": true
                                    }
                                }),
                                sse_text_response_with_execution_summary(
                                    "Done!", "sess-tx-e2e", 1, 0, &["bash"], 1, ("run-sess-tx-e2e", 1),
                                ),
                            );
                        (TEST_SSE_HEADERS, body)
                    }
                }
            }),
        )
        .route(
            "/tools/result",
            post({
                let state = state.clone();
                move |axum::Json(body): axum::Json<serde_json::Value>| {
                    let state = state.clone();
                    async move {
                        state.tool_results.lock().await.push(body);
                        axum::Json(serde_json::json!({ "ok": true }))
                    }
                }
            }),
        );
    let base = spawn_mock(app).await;
    let api = astra_thin_client::ThinClient::new(&base, None).unwrap();
    let mut pm = PermissionManager::new(true);

    let result = stream_chat_sse(ChatTurnParams {
        api: &api,
        token: "fake-token",
        auth_owner: None,
        message: "write inside a transaction",
        user_intent: "write inside a transaction",
        input_runtime_required_texts: &[],
        input_runtime_volatile_texts: &[],
        input_work_unit_observations: &[],
        semantic_query_override: None,
        session_id: None,
        offering_id: None,
        model: Some("test-model"),
        provider: None,
        explain: ExplainMode::Off,
        runtime_config: std::sync::Arc::new(astra_config::RuntimeConfig::default()),
        render_md: false,
        history: &[],
        perm_manager: &mut pm,
        verbose_mode: false,
        render_policy: crate::cli::stream::stream_render::RenderPolicy::Silent,
        cli_context: None,
        recent_tools: &[],
        deferred_tool_activations: None,
        resume_restricted_tools: &[],
        tool_health_entries: &[],
        workspace_observation_quarantine: None,
        session_lessons: &[],
        memory_selection_reports: &[],

        latest_turn_quality_feedback: None,
        unified_skill_registry: astra_runtime::skills::empty_unified_registry(),
        is_plan_subtask: false,
        plan_subtask_id: None,
        cancel_token: None,
        execution_time_budget: None,
        run_control: None,
        incremental_state: None,
        executor_observer: None,
        request_session_execution_lease: None,
        plan_assemble_line_release: None,
        stream_event_tx: None,
        explain_analyze_terminal_degraded: None,
        stream_json_emitter: None,
        agent_live_event_sink: None,
        approval_request_tx: None,
        ask_user_request_tx: None,
        plan_review_request_tx: None,
        mcp_manager: None,

        agent_spawner: None,
        root_agent_id: None,
        observability_hub: None,
        observability_session: None,
        file_journal: None,
        file_state: None,
        database_snapshot_journal: None,

        git_worktree_journal: None,
        session_state_journal: None,
        bg_task_commands: None,
        bg_task_list_cache: None,
        bash_detach_slot: None,
        turn_index: DEFAULT_TURN_INDEX,
        pipeline_state: None,
        compaction_state: None,
        consecutive_context_window_errors: 0,
        idempotency_cache: None,
        pre_loaded_messages: None,
        append_system_prompt: None,
        #[cfg(feature = "harness")]
        harness_sink: None,

        #[cfg(feature = "harness")]
        benchmark_profile: None,
    })
    .await
    .unwrap();

    assert!(
        result.full_text.starts_with("Done!"),
        "unexpected full_text: {:?}",
        result.full_text
    );
    assert!(result.tool_calls_count > 0);
    assert_eq!(
        state.call_count.load(std::sync::atomic::Ordering::SeqCst),
        1
    );

    let tool_results = state.tool_results.lock().await;
    assert_eq!(tool_results.len(), 1);
    assert_eq!(tool_results[0]["request_id"].as_str(), Some("tr-tx-1"));
    drop(tool_results);

    let boundary_events: Vec<_> = session_journal::read_journal("sess-tx-e2e")
        .unwrap()
        .into_iter()
        .filter(|event| {
            matches!(
                event.event_type,
                JournalEventType::ExecutionBoundaryOpened
                    | JournalEventType::ExecutionBoundaryCommitted
            )
        })
        .collect();
    assert_eq!(boundary_events.len(), 2);
    assert_eq!(
        boundary_events[0].event_type,
        JournalEventType::ExecutionBoundaryOpened
    );
    assert_eq!(
        boundary_events[1].event_type,
        JournalEventType::ExecutionBoundaryCommitted
    );

    let opened = boundary_events[0]
        .metadata
        .as_ref()
        .and_then(|meta| meta.get("execution_boundary"))
        .expect("opened boundary metadata");
    assert_eq!(opened["kind"].as_str(), Some("tool_batch"));
    assert_eq!(opened["transaction_id"].as_str(), Some("tx-e2e"));

    let committed = boundary_events[1]
        .metadata
        .as_ref()
        .and_then(|meta| meta.get("execution_boundary"))
        .expect("committed boundary metadata");
    assert_eq!(committed["kind"].as_str(), Some("tool_batch"));
    assert_eq!(committed["transaction_id"].as_str(), Some("tx-e2e"));
}

#[tokio::test(flavor = "current_thread")]
async fn stream_chat_sse_submits_one_server_owned_turn_without_client_cursor() {
    #[derive(Clone)]
    struct StreamingMockState {
        call_count: std::sync::Arc<std::sync::atomic::AtomicU32>,
        turn_payloads: std::sync::Arc<tokio::sync::Mutex<Vec<serde_json::Value>>>,
    }

    let state = StreamingMockState {
        call_count: std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0)),
        turn_payloads: std::sync::Arc::new(tokio::sync::Mutex::new(Vec::new())),
    };
    let app = Router::new().route(
        "/chat/stream",
        post({
            let state = state.clone();
            move |axum::Json(body): axum::Json<serde_json::Value>| {
                let state = state.clone();
                async move {
                    state.turn_payloads.lock().await.push(body);
                    state
                        .call_count
                        .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    (
                        TEST_SSE_HEADERS,
                        sse_text_response("Done!", "sess-turn-identity"),
                    )
                }
            }
        }),
    );
    let base = spawn_mock(app).await;
    let api = astra_thin_client::ThinClient::new(&base, None).unwrap();
    let mut pm = PermissionManager::new(true);

    let mut config = astra_config::RuntimeConfig::default();
    config.tool_surface.pinned_tools = vec!["glob".into(), "-read_file".into()];
    let result = stream_chat_sse(ChatTurnParams {
        api: &api,
        token: "fake-token",
        auth_owner: None,
        message: "review local changes",
        user_intent: "review local changes",
        input_runtime_required_texts: &[],
        input_runtime_volatile_texts: &[],
        input_work_unit_observations: &[],
        semantic_query_override: None,
        session_id: None,
        offering_id: None,
        model: Some("test-model"),
        provider: None,
        explain: ExplainMode::Off,
        runtime_config: std::sync::Arc::new(config),
        render_md: false,
        history: &[],
        perm_manager: &mut pm,
        verbose_mode: false,
        render_policy: crate::cli::stream::stream_render::RenderPolicy::Silent,
        cli_context: None,
        recent_tools: &[],
        deferred_tool_activations: None,
        resume_restricted_tools: &[],
        tool_health_entries: &[],
        workspace_observation_quarantine: None,
        session_lessons: &[],
        memory_selection_reports: &[],

        latest_turn_quality_feedback: None,
        unified_skill_registry: astra_runtime::skills::empty_unified_registry(),
        is_plan_subtask: false,
        plan_subtask_id: None,
        cancel_token: None,
        execution_time_budget: None,
        run_control: None,
        incremental_state: None,
        executor_observer: None,
        request_session_execution_lease: None,
        plan_assemble_line_release: None,
        stream_event_tx: None,
        explain_analyze_terminal_degraded: None,
        stream_json_emitter: None,
        agent_live_event_sink: None,
        approval_request_tx: None,
        ask_user_request_tx: None,
        plan_review_request_tx: None,
        mcp_manager: None,

        agent_spawner: None,
        root_agent_id: None,
        observability_hub: None,
        observability_session: None,
        file_journal: None,
        file_state: None,
        database_snapshot_journal: None,

        git_worktree_journal: None,
        session_state_journal: None,
        bg_task_commands: None,
        bg_task_list_cache: None,
        bash_detach_slot: None,
        turn_index: DEFAULT_TURN_INDEX,
        pipeline_state: None,
        compaction_state: None,
        consecutive_context_window_errors: 0,
        idempotency_cache: None,
        pre_loaded_messages: None,
        append_system_prompt: None,
        #[cfg(feature = "harness")]
        harness_sink: None,

        #[cfg(feature = "harness")]
        benchmark_profile: None,
    })
    .await
    .unwrap();

    assert!(
        result.full_text.starts_with("Done!"),
        "unexpected full_text: {:?}",
        result.full_text
    );

    let payloads = state.turn_payloads.lock().await;
    assert_eq!(payloads.len(), 1, "one user action is one Server admission");
    let payload = &payloads[0];
    assert_eq!(payload["message"], "review local changes");
    let tools: Vec<_> = payload["context"]["edge_tools"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|schema| schema["function"]["name"].as_str())
        .collect();
    assert!(
        tools.contains(&"glob"),
        "selected session pins must reach the wire: {tools:?}"
    );
    assert!(
        !tools.contains(&"read_file"),
        "session removal must override process defaults: {tools:?}"
    );
    for client_owned in [
        "messages",
        "tool_results",
        "session_turn",
        "turn_chain_id",
        "user_query_event_id",
    ] {
        assert!(
            payload.get(client_owned).is_none(),
            "{client_owned} must be restored by the Server"
        );
    }
}

#[tokio::test(flavor = "current_thread")]
async fn stream_chat_sse_does_not_retry_server_conflicts_with_client_cursor_state() {
    #[derive(Clone)]
    struct StreamingMockState {
        call_count: std::sync::Arc<std::sync::atomic::AtomicU32>,
        turn_payloads: std::sync::Arc<tokio::sync::Mutex<Vec<serde_json::Value>>>,
    }

    let state = StreamingMockState {
        call_count: std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0)),
        turn_payloads: std::sync::Arc::new(tokio::sync::Mutex::new(Vec::new())),
    };
    let app = Router::new().route(
        "/chat/stream",
        post({
            let state = state.clone();
            move |axum::Json(body): axum::Json<serde_json::Value>| {
                let state = state.clone();
                async move {
                    state.turn_payloads.lock().await.push(body);
                    let n = state
                        .call_count
                        .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    let response = if n == 0 {
                        format!(
                            "data: {}\n\ndata: [DONE]\n\n",
                            serde_json::json!({
                                "type": "error",
                                "message": "explicit bridge session_turn 3 does not match canonical turn 2",
                                "error_code": "session_turn_mismatch",
                                "metadata": {
                                    "session_id": "sess-stale",
                                    "actual_session_turn": 3,
                                    "expected_session_turn": 2,
                                    "turn_chain_id": "root-chain",
                                    "user_query_event_id": "root-query"
                                }
                            })
                        )
                    } else {
                        sse_text_response("Recovered!", "sess-stale")
                    };
                    (TEST_SSE_HEADERS, response)
                }
            }
        }),
    );
    let base = spawn_mock(app).await;
    let api = astra_thin_client::ThinClient::new(&base, None).unwrap();
    let mut pm = PermissionManager::new(true);

    let failure = stream_chat_sse(ChatTurnParams {
        api: &api,
        token: "fake-token",
        auth_owner: None,
        message: "continue after interrupted turn",
        user_intent: "continue after interrupted turn",
        input_runtime_required_texts: &[],
        input_runtime_volatile_texts: &[],
        input_work_unit_observations: &[],
        semantic_query_override: None,
        session_id: Some("sess-stale"),
        offering_id: None,
        model: Some("test-model"),
        provider: None,
        explain: ExplainMode::Off,
        runtime_config: std::sync::Arc::new(astra_config::RuntimeConfig::default()),
        render_md: false,
        history: &[],
        perm_manager: &mut pm,
        verbose_mode: false,
        render_policy: crate::cli::stream::stream_render::RenderPolicy::Silent,
        cli_context: None,
        recent_tools: &[],
        deferred_tool_activations: None,
        resume_restricted_tools: &[],
        tool_health_entries: &[],
        workspace_observation_quarantine: None,
        session_lessons: &[],
        memory_selection_reports: &[],

        latest_turn_quality_feedback: None,
        unified_skill_registry: astra_runtime::skills::empty_unified_registry(),
        is_plan_subtask: false,
        plan_subtask_id: None,
        cancel_token: None,
        execution_time_budget: None,
        run_control: None,
        incremental_state: None,
        executor_observer: None,
        request_session_execution_lease: None,
        plan_assemble_line_release: None,
        stream_event_tx: None,
        explain_analyze_terminal_degraded: None,
        stream_json_emitter: None,
        agent_live_event_sink: None,
        approval_request_tx: None,
        ask_user_request_tx: None,
        plan_review_request_tx: None,
        mcp_manager: None,

        agent_spawner: None,
        root_agent_id: None,
        observability_hub: None,
        observability_session: None,
        file_journal: None,
        file_state: None,
        database_snapshot_journal: None,

        git_worktree_journal: None,
        session_state_journal: None,
        bg_task_commands: None,
        bg_task_list_cache: None,
        bash_detach_slot: None,
        turn_index: 3,
        pipeline_state: None,
        compaction_state: None,
        consecutive_context_window_errors: 0,
        idempotency_cache: None,
        pre_loaded_messages: None,
        append_system_prompt: None,
        #[cfg(feature = "harness")]
        harness_sink: None,

        #[cfg(feature = "harness")]
        benchmark_profile: None,
    })
    .await
    .expect_err("canonical conflicts must be returned to the caller");

    assert!(
        failure.error.contains("canonical turn 2"),
        "typed Server error should be preserved: {}",
        failure.error
    );

    let payloads = state.turn_payloads.lock().await;
    assert_eq!(payloads.len(), 1, "the CLI must not manufacture a retry");
    assert!(payloads[0].get("session_turn").is_none());
    assert!(payloads[0].get("turn_chain_id").is_none());
    assert!(payloads[0].get("user_query_event_id").is_none());
}

// ── Phase 3C: Chat stream MCP integration tests ──────────────────────────────

#[tokio::test]
async fn stream_chat_sse_mcp_requires_server_owned_callback() {
    let mock_server_bin = mock_mcp_server_binary();

    // Connect McpClientManager to mock server via stdio
    let mut manager = crate::mcp_client::McpClientManager::new();
    let config = crate::mcp_client::McpServerConfig {
        name: "mock".to_string(),
        transport: crate::mcp_client::Transport::Stdio {
            command: vec![mock_server_bin.to_string_lossy().to_string()],
            args: vec![],
            env: std::collections::HashMap::new(),
        },
        description: String::new(),
        enabled: true,
        retry: crate::mcp_client::RetryConfig::default(),
    };
    manager
        .connect(config)
        .await
        .expect("connect to mock MCP server");

    // Verify tools were discovered (echo, add, get_time)
    let tools = manager.all_tools();
    assert!(!tools.is_empty(), "mock MCP server should expose tools");
    let tool_names: Vec<String> = tools.iter().map(|t| t.1.name.to_string()).collect();
    assert!(
        tool_names.iter().any(|n| n.contains("echo")),
        "expected echo tool, got: {:?}",
        tool_names
    );

    let mcp_tool_name = manager
        .all_tool_schemas()
        .into_iter()
        .filter_map(|schema| schema["function"]["name"].as_str().map(str::to_owned))
        .find(|name| manager.find_tool_by_mcp_name(name) == Some(("mock", "echo")))
        .expect("discovery must expose the public echo identity");
    let mcp_arc = std::sync::Arc::new(tokio::sync::RwLock::new(manager));

    // Exercise the public entrypoint with both a genuine callback request
    // and an unauthorized client-continuation response, using one fixture.
    for mode in ["unadmitted", "completed", "missing_terminal"] {
        let callback_admitted = mode != "unadmitted";
        let call_count = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
        let callbacks = std::sync::Arc::new(tokio::sync::Mutex::new(Vec::new()));
        let captured_callbacks = callbacks.clone();
        let cc = call_count.clone();
        let tool_name_clone = mcp_tool_name.clone();
        let app = axum::Router::new().route(
        "/chat/stream",
        axum::routing::post(move || {
            let cc = cc.clone();
            let tn = tool_name_clone.clone();
            async move {
                cc.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let body = if callback_admitted {
                    let request = serde_json::json!({
                        "type": "tool_request", "session_id": "sess-mcp",
                        "run_id": "run-sess-mcp", "turn_chain_id": "chain-mcp",
                        "request_id": "mcp-1", "schema_admitted_by_server": true,
                        "execution_timeout_ms": 300000, "command_timeout_cap_ms": 30_000,
                        "execution_deadline_unix_ms": 4102444800000u64,
                        "tool": tn, "args": {"message": "hello from test"}
                    });
                    format!(
                        "data: {{\"type\":\"session_info\",\"session_id\":\"sess-mcp\",\"run_id\":\"run-sess-mcp\"}}\n\ndata: {request}\n\n{}",
                        sse_text_response_with_execution_summary(
                            "MCP done!", "sess-mcp", 1, 1, &[&tn], 1, ("run-sess-mcp", 1),
                        ),
                    )
                } else { format!(
                        "data: {{\"type\":\"session_info\",\"session_id\":\"sess-mcp\",\"run_id\":\"run-sess-mcp\"}}\n\n\
                         data: {{\"type\":\"tool_call\",\"id\":\"mcp-1\",\"name\":\"{}\",\"arguments\":{{\"message\":\"hello from test\"}}}}\n\n\
                         data: {{\"type\":\"turn_complete\",\"has_tool_calls\":true}}\n\n\
                         data: [DONE]\n\n",
                        tn
                    ) };
                let body = if mode == "missing_terminal" {
                    let end = body.find("data: {\"type\":\"run_finished\"").unwrap();
                    format!("{}data: [DONE]\n\n", &body[..end])
                } else { body };
                (TEST_SSE_HEADERS, body)
            }
        }),
    ).route("/tools/result", post(move |Json(body): Json<serde_json::Value>| {
        let callbacks = captured_callbacks.clone();
        async move {
            callbacks.lock().await.push(body);
            Json(serde_json::json!({"ok": true}))
        }
    }));
        let base = spawn_mock(app).await;
        let api = astra_thin_client::ThinClient::new(&base, None).unwrap();

        let mut pm = PermissionManager::new(true);

        let unified_skill_registry = astra_runtime::skills::empty_unified_registry().clone();
        let context = BasicCliChatContext {
            api: &api,
            auth_owner: None,
            message: "call echo",
            offering_id: None,
            model: Some("test-model"),
            provider: None,
            explain: ExplainMode::Off,
            runtime_config: std::sync::Arc::new(astra_config::RuntimeConfig::default()),
            render_md: false,
            verbose_mode: false,
            render_policy: crate::cli::stream::stream_render::RenderPolicy::Silent,
            cli_context: None,
            unified_skill_registry: &unified_skill_registry,
            stream_event_tx: None,
            stream_json_emitter: None,
            mcp_manager: Some(mcp_arc.clone()),
            agent_spawner: None,
            root_agent_id: None,
            bg_task_commands: None,
            bg_task_list_cache: None,
            bash_detach_slot: None,
            #[cfg(feature = "harness")]
            harness_sink: None,

            #[cfg(feature = "harness")]
            benchmark_profile: None,
        };
        let stream = stream_chat_sse(ChatTurnParams::basic_cli(
            &context,
            "fake-token",
            None,
            &mut pm,
        ));
        assert_eq!(
            std::mem::size_of_val(&stream),
            std::mem::size_of::<usize>(),
            "the shared stream entrypoint must bound every caller's embedded future"
        );
        let result = stream.await;
        assert_eq!(call_count.load(std::sync::atomic::Ordering::SeqCst), 1);
        let callbacks = callbacks.lock().await;
        if callback_admitted {
            if mode == "completed" {
                let result = result.expect("Server callback must execute the real MCP tool");
                assert_eq!(result.full_text, "MCP done!");
            } else {
                let failure =
                    result.expect_err("completed callback cannot authorize an unfinished run");
                assert_eq!(failure.partial.partial_text, "MCP done!");
                assert_eq!(failure.partial.tool_call_records.len(), 1);
                assert_eq!(failure.partial.tool_calls_count, 1);
                assert!(failure.partial.tool_call_records[0].ok);
                assert!(
                    failure.partial.tool_outcomes.is_none(),
                    "a callback is not a complete Server ledger"
                );
                assert!(
                    !failure
                        .partial
                        .run_transcript_messages
                        .iter()
                        .any(|message| message["role"] == "assistant"
                            && message["content"] == "MCP done!")
                );
            }
            assert_eq!(callbacks.len(), 1);
            assert_eq!(callbacks[0]["request_id"], "mcp-1");
            assert_eq!(callbacks[0]["status"], "completed");
            assert!(
                callbacks[0]["output"]
                    .as_str()
                    .unwrap()
                    .contains("hello from test")
            );
        } else {
            let error =
                result.expect_err("MCP availability does not authorize client continuation");
            assert!(error.error.contains("terminal execution evidence"));
            assert!(
                callbacks.is_empty(),
                "unadmitted MCP calls must not execute"
            );
        }
    }
}

#[tokio::test(flavor = "current_thread")]
#[serial_test::serial]
async fn stream_chat_sse_preserves_runtime_notifications_across_admission_and_failure() {
    use crate::cli::turn::local_run_control::LocalRunControl;
    use axum::response::IntoResponse;
    use std::sync::{Arc, Mutex};

    let temp = tempfile::tempdir().unwrap();
    let _journal_guard = ProcessJournalDirGuard::new(temp.path());
    for outcome in [
        "completed",
        "failed",
        "runtime_cancelled",
        "http_failure",
        "retry",
        "cancelled_before_admission",
    ] {
        let control = LocalRunControl::shared();
        control
            .accept_runtime_notification("first child finished")
            .unwrap();
        control
            .accept_runtime_notification("second child finished")
            .unwrap();
        let bodies = Arc::new(Mutex::new(Vec::new()));
        let app = Router::new().route("/chat/stream", post({
            let bodies = bodies.clone();
            let control = control.clone();
            move |Json(body): Json<serde_json::Value>| {
                let bodies = bodies.clone();
                let control = control.clone();
                async move {
                    let call = {
                        let mut bodies = bodies.lock().unwrap();
                        bodies.push(body);
                        bodies.len()
                    };
                    if outcome == "retry" && call == 1 {
                        return (TEST_SSE_HEADERS,
                            "data: {\"type\":\"error\",\"message\":\"session not found\",\"error_code\":\"session_not_found\",\"metadata\":{\"admission_state\":\"rejected\"}}\n\ndata: [DONE]\n\n".to_string()).into_response();
                    }
                if outcome == "runtime_cancelled" {
                        control.request_cancel_for_runtime();
                    }
                    // This arrives after input admission, so the next turn owns it.
                    control.accept_runtime_notification("third child finished").unwrap();
                    if outcome == "http_failure" {
                        return (axum::http::StatusCode::SERVICE_UNAVAILABLE, "service unavailable").into_response();
                    }
                    let response = if matches!(outcome, "completed" | "retry") {
                        sse_text_response("Done!", "sess-notifications")
                    } else {
                        "data: {\"type\":\"session_info\",\"session_id\":\"sess-notifications\",\"run_id\":\"run-sess-notifications\"}\n\n\
                         data: {\"type\":\"text_delta\",\"content\":\"Partial answer\"}\n\n\
                         data: [DONE]\n\n".into()
                    };
                    let guidance = serde_json::json!({
                        "type": "user_intent_applied", "run_id": "run-sess-notifications",
                        "intent_id": "guidance-1", "delivery": "guide_current_run",
                        "status": "applied", "event_index": 7, "content": "wait for the review"
                    });
                    let mut child_guidance = guidance.clone();
                    child_guidance["run_id"] = serde_json::json!("child-run");
                    child_guidance["intent_id"] = serde_json::json!("guidance-child");
                    child_guidance["content"] = serde_json::json!("child-only guidance");
                    let response = response.replacen(
                        "data: {\"type\":\"text_delta\"",
                        &format!("data: {guidance}\n\ndata: {guidance}\n\ndata: {child_guidance}\n\ndata: {{\"type\":\"text_delta\""),
                        1,
                    );
                    let response = if outcome == "failed" {
                        response.replace("data: [DONE]", "data: {\"type\":\"error\",\"message\":\"stream stalled\",\"error_kind\":\"stream_idle\"}\n\ndata: [DONE]")
                    } else if outcome == "runtime_cancelled" {
                        response.replace("data: [DONE]", "data: {\"type\":\"error\",\"message\":\"runtime cancelled\",\"error_kind\":\"cancelled\"}\n\ndata: [DONE]")
                    } else { response };
                    (TEST_SSE_HEADERS, response).into_response()
                }
            }
        }));
        let base = spawn_mock(app).await;
        let api = astra_thin_client::ThinClient::new(&base, None).unwrap();
        let mut pm = PermissionManager::new(true);

        let registry = astra_runtime::skills::empty_unified_registry();
        let context = basic_chat_context(&api, &registry, "review results");
        #[cfg(feature = "harness")]
        let harness_sink = astra_harness::InMemorySnapshotSink::arc();
        let hub = Arc::new(astra_runtime::observability::ObservabilityHub::new());
        let observer = hub.start_session("test-owner", "sess-notifications");
        let mut params = ChatTurnParams::basic_cli(&context, "fake-token", None, &mut pm);
        params.run_control = Some(control.clone());
        #[cfg(feature = "harness")]
        {
            params.harness_sink = Some(harness_sink.clone());
            params.benchmark_profile = Some(astra_harness::HarnessProfile::Swebench);
        }
        if matches!(outcome, "failed" | "http_failure") {
            params.observability_session = Some(observer.clone());
            params.session_state_journal = Some(Arc::new(Mutex::new(
                crate::edge_tools::SessionStateRollbackJournal::default(),
            )));
        }
        if outcome == "cancelled_before_admission" {
            let token = Arc::new(tokio_util::sync::CancellationToken::new());
            token.cancel();
            params.cancel_token = Some(token);
            control.request_cancel_for_user();
        }
        let mut result = stream_chat_sse(params).await;
        if outcome == "retry" {
            let rejected = result.unwrap_err();
            assert!(rejected.partial.admission_rejected);
            assert!(rejected.partial.interruption.is_none());
            control
                .accept_runtime_notification("between requests finished")
                .unwrap();
            let mut retry = ChatTurnParams::basic_cli(&context, "fake-token", None, &mut pm);
            retry.run_control = Some(control.clone());
            result = stream_chat_sse(retry).await;
        }
        let bodies = bodies.lock().unwrap();
        if outcome == "cancelled_before_admission" {
            assert!(result.is_err());
            assert!(bodies.is_empty(), "cancelled input must never be admitted");
        } else {
            assert_eq!(bodies.len(), if outcome == "retry" { 2 } else { 1 });
            for (index, body) in bodies.iter().enumerate() {
                let request = body.to_string();
                assert!(request.contains("first child finished"));
                assert!(request.contains("second child finished"));
                assert!(!request.contains("third child finished"));
                assert_eq!(
                    request.contains("between requests finished"),
                    outcome == "retry" && index == 1
                );
                assert_eq!(request.matches("first child finished").count(), 1);
                assert_eq!(request.matches("second child finished").count(), 1);
            }
            if matches!(outcome, "completed" | "retry") {
                let result = result.unwrap();
                assert_eq!(result.full_text, "Done!");
                assert_eq!(result.applied_user_intents.len(), 1);
                assert_eq!(result.applied_user_intents[0].intent_id, "guidance-1");
                assert_eq!(
                    result.applied_user_intents[0].content,
                    "wait for the review"
                );

                assert_eq!(
                    result
                        .run_transcript_messages
                        .iter()
                        .filter(|message| message["role"] == "assistant"
                            && message["content"] == "Done!")
                        .count(),
                    1
                );
                // This is the enclosing successful settlement's existing commit.
                control.commit_applied_runtime_notifications();
            } else {
                let failure = result.unwrap_err();
                assert_eq!(
                    failure.partial.partial_text,
                    if outcome == "http_failure" {
                        ""
                    } else {
                        "Partial answer"
                    }
                );
                if outcome != "http_failure" {
                    assert_eq!(failure.partial.applied_user_intents.len(), 1);
                    assert_eq!(
                        failure.partial.applied_user_intents[0].intent_id,
                        "guidance-1"
                    );
                }
                if outcome == "http_failure" {
                    assert_eq!(
                        failure
                            .partial
                            .interruption
                            .as_ref()
                            .expect("HTTP transport recovery")["kind"],
                        "stream_transport"
                    );
                }
                if outcome == "failed" {
                    let interruption = failure
                        .partial
                        .interruption
                        .as_ref()
                        .expect("typed error recovery");
                    assert_eq!(interruption["kind"], "stream_idle");
                    let Some(astra_pipeline::step_protocol::StepCheckpoint::Heavy(checkpoint)) =
                        failure.partial.last_heavy_checkpoint.as_ref()
                    else {
                        panic!("admitted failure retains root heavy checkpoint");
                    };
                    assert_eq!(
                        checkpoint.budget_remaining_rounds, 0,
                        "CLI continuity must not invent a Server execution allowance"
                    );
                    let observed = observer.read().unwrap();
                    assert_eq!(
                        observed.context_traces.len(),
                        1,
                        "failure settles measured trace"
                    );
                    assert_eq!(observed.context_traces[0].session_id, "sess-notifications");
                }
                if outcome == "runtime_cancelled" {
                    assert!(
                        failure
                            .partial
                            .interruption
                            .as_ref()
                            .is_none_or(|value| value["kind"] != "user_cancelled")
                    );
                }
                assert!(
                    !failure
                        .partial
                        .run_transcript_messages
                        .iter()
                        .any(|message| message["role"] == "assistant"
                            && message["content"] == "Partial answer")
                );
            }
        }
        #[cfg(feature = "harness")]
        if matches!(outcome, "completed" | "failed") {
            use astra_harness::SnapshotSink;
            let snapshot = harness_sink.latest().expect("terminal runtime snapshot");
            let (final_state, interruption, tokens) = if outcome == "completed" {
                ("completed", None, 15)
            } else {
                ("interrupted", Some("stream_idle"), 0)
            };
            assert_eq!(snapshot.session_id, "sess-notifications");
            assert_eq!(snapshot.final_state.as_deref(), Some(final_state));
            // Partial failure text remains observable; typed state determines settlement.
            assert!(snapshot.has_final_text);
            assert_eq!(snapshot.interruption_kind.as_deref(), interruption);
            assert_eq!(snapshot.tokens_used_session, tokens);
            let state = crate::cli::session::session_state::SessionState {
                harness_sink: harness_sink.clone(),
                ..Default::default()
            };
            let displayed = crate::tui::inspection::render_snapshot_summary(&state)
                .expect("inspect reads the stream's snapshot sink");
            assert_eq!(
                displayed
                    .lines()
                    .find(|line| line.contains("Tokens (session):"))
                    .and_then(|line| line.split_whitespace().last())
                    .and_then(|value| value.parse::<u64>().ok()),
                Some(tokens)
            );
        }
        let pending = control.take_pending_runtime_notifications();
        let expected = match outcome {
            "completed" | "retry" => vec!["third child finished"],
            "failed" | "runtime_cancelled" | "http_failure" => vec![
                "first child finished",
                "second child finished",
                "third child finished",
            ],
            _ => vec!["first child finished", "second child finished"],
        };
        assert_eq!(pending, expected, "input ownership after {outcome}");
    }
}

#[test]
fn stream_chat_sse_publishes_headless_answer_once() {
    for policy in ["stream", "final_only"] {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "tests::chat_stream_tests::stream_chat_sse_stdout_probe",
                "--nocapture",
            ])
            .env("ASTRA_STREAM_STDOUT_PROBE", policy)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{policy}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            String::from_utf8(output.stdout)
                .unwrap()
                .matches("terminal publication marker")
                .count(),
            1,
            "the real {policy} stream consumer must publish terminal text exactly once"
        );
    }
}

#[tokio::test(flavor = "current_thread")]
async fn stream_chat_sse_stdout_probe() {
    let Ok(policy) = std::env::var("ASTRA_STREAM_STDOUT_PROBE") else {
        return;
    };
    let app = Router::new().route(
        "/chat/stream",
        post(|| async {
            (
                TEST_SSE_HEADERS,
                sse_text_response("terminal publication marker", "sess-stdout"),
            )
        }),
    );
    let base = spawn_mock(app).await;
    let api = astra_thin_client::ThinClient::new(&base, None).unwrap();
    let registry = astra_runtime::skills::empty_unified_registry();
    let mut context = basic_chat_context(&api, &registry, "review results");
    context.render_policy = match policy.as_str() {
        "stream" => crate::cli::stream::stream_render::RenderPolicy::Stream,
        "final_only" => crate::cli::stream::stream_render::RenderPolicy::FinalOnly,
        _ => panic!("unexpected probe render policy"),
    };
    let mut pm = PermissionManager::new(true);

    let result = stream_chat_sse(ChatTurnParams::basic_cli(
        &context,
        "fake-token",
        None,
        &mut pm,
    ))
    .await
    .unwrap();
    assert_eq!(result.full_text, "terminal publication marker");
}

#[tokio::test]
async fn basic_cli_catalog_rejection_preserves_typed_preflight_evidence() {
    use crate::cli::turn::{execute_basic_cli_turn, turn_facade::BasicCliTurnOptions};
    use wiremock::{Mock, MockServer, ResponseTemplate};
    for status in [401, 403, 429] {
        for selected in [None, Some("test-model")] {
            let server = MockServer::start().await;
            Mock::given(wiremock::matchers::method("GET"))
                .respond_with(
                    ResponseTemplate::new(status).set_body_json(serde_json::json!({
                        "detail": "synthetic catalog denial"
                    })),
                )
                .expect(1)
                .mount(&server)
                .await;
            let api = astra_thin_client::ThinClient::new(&server.uri(), None).unwrap();
            let registry = astra_runtime::skills::default_unified_registry();
            let mut context = basic_chat_context(&api, &registry, "synthetic preflight");
            context.model = selected;
            let mut pm = PermissionManager::new(true);
            let observer = crate::cli::chat_stream::CliExecutorObserver::default();
            let failure = execute_basic_cli_turn(
                &context,
                "synthetic-token",
                None,
                &mut pm,
                BasicCliTurnOptions {
                    executor_observer: Some(observer.clone()),
                    ..Default::default()
                },
            )
            .await
            .expect_err("catalog rejected");
            assert_eq!(
                failure
                    .partial
                    .error_metadata
                    .as_ref()
                    .and_then(|v| v["http_status"].as_u64()),
                Some(status as u64),
                "{selected:?}: {failure:?}"
            );
            assert_eq!(
                crate::cli::turn::turn_auth_retry::should_retry_after_auth_refresh(&failure),
                status == 401
            );
            assert!(
                observer.get().is_none(),
                "catalog denial is before the executor factory"
            );
            assert!(failure.partial.run_id.is_none());
            assert!(failure.partial.tool_call_records.is_empty());
            server.verify().await;
        }
    }
}

#[tokio::test]
#[serial_test::serial]
async fn unsupported_workspace_host_rejects_public_turn_before_admission_http() {
    use crate::cli::chat_stream::WORKSPACE_HOST_VALIDATION_FOR_TEST;
    use astra_tools::workspace_observation::WorkspaceLeaseFailure;
    let _home = crate::test_utils::HomeGuard::temp();
    let _credentials = crate::test_utils::isolate_credentials();
    let server = wiremock::MockServer::start().await;
    wiremock::Mock::given(wiremock::matchers::method("GET"))
        .and(wiremock::matchers::path("/models"))
        .respond_with(
            wiremock::ResponseTemplate::new(200)
                .set_body_json(crate::test_utils::mock_model_catalog_json(&["test-model"])),
        )
        .expect(1)
        .mount(&server)
        .await;
    let api = astra_thin_client::ThinClient::new(&server.uri(), None).unwrap();
    let registry = astra_runtime::skills::empty_unified_registry();
    let context = basic_chat_context(&api, &registry, "write a file in this workspace");
    let mut pm = PermissionManager::new(true);
    let failure = WORKSPACE_HOST_VALIDATION_FOR_TEST
        .scope(
            Err(WorkspaceLeaseFailure::CoordinationHostUnsupported),
            async {
                stream_chat_sse(ChatTurnParams::basic_cli(
                    &context,
                    "synthetic-token",
                    None,
                    &mut pm,
                ))
                .await
            },
        )
        .await
        .expect_err("unsupported local host must reject before Server admission");
    assert!(failure.error.contains("root-owned"), "{failure:?}");
    assert!(failure.error.contains("administrator"), "{failure:?}");
    let requests = server.received_requests().await.unwrap();
    assert_eq!(
        requests.len(),
        1,
        "only the read-only model catalog may be requested"
    );
    assert_eq!(requests[0].method.as_str(), "GET");
    assert_eq!(requests[0].url.path(), "/models");
    assert_eq!(
        failure.partial.error_code.as_deref(),
        Some("workspace_host_unsupported")
    );
    assert!(failure.partial.server_admission_not_dispatched);
    assert_eq!(
        failure.partial.failure_kind,
        Some(astra_core::ErrorKind::ToolUnavailable)
    );
    assert_eq!(
        failure.partial.error_metadata.as_ref().unwrap()["retryable"],
        false
    );
    assert!(!failure.partial.admission_rejected);
    assert_eq!(failure.partial.prompt_tokens, 0);
    assert_eq!(failure.partial.completion_tokens, 0);
    assert_eq!(failure.partial.tool_calls_count, 0);
    assert!(failure.partial.run_id.is_none());
    assert!(failure.partial.tool_call_records.is_empty());
}

#[tokio::test]
async fn basic_cli_catalog_auth_retry_observes_only_the_successful_factory() {
    use crate::cli::turn::{execute_basic_cli_turn, turn_facade::BasicCliTurnOptions};
    use axum::response::IntoResponse as _;
    use axum::routing::get;
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    let catalogs = Arc::new(AtomicUsize::new(0));
    let catalog_count = catalogs.clone();
    let chats = Arc::new(AtomicUsize::new(0));
    let chat_count = chats.clone();
    let app = Router::new()
        .route(
            "/models",
            get(move || {
                let count = catalog_count.clone();
                async move {
                    if count.fetch_add(1, Ordering::SeqCst) == 0 {
                        (
                            axum::http::StatusCode::UNAUTHORIZED,
                            "synthetic expired token",
                        )
                            .into_response()
                    } else {
                        axum::Json(crate::test_utils::mock_model_catalog_json(&["test-model"]))
                            .into_response()
                    }
                }
            }),
        )
        .route(
            "/chat/stream",
            post(move || {
                let count = chat_count.clone();
                async move {
                    count.fetch_add(1, Ordering::SeqCst);
                    (
                        TEST_SSE_HEADERS,
                        sse_text_response("retry result", "sess-observer-retry"),
                    )
                }
            }),
        );
    let base = super::spawn_mock_app(app).await;
    let api = astra_thin_client::ThinClient::new(&base, None).unwrap();
    let registry = astra_runtime::skills::empty_unified_registry();
    let context = basic_chat_context(&api, &registry, "synthetic retry");
    let mut pm = PermissionManager::new(true);
    let observer = crate::cli::chat_stream::CliExecutorObserver::default();
    let options = BasicCliTurnOptions {
        executor_observer: Some(observer.clone()),
        ..Default::default()
    };
    // Utility/child constructors have no observer unless the outer request explicitly supplies it.
    assert!(
        ChatTurnParams::basic_cli(&context, "synthetic", None, &mut pm)
            .executor_observer
            .is_none()
    );
    let failure = execute_basic_cli_turn(
        &context,
        "synthetic-expired",
        None,
        &mut pm,
        options.clone(),
    )
    .await
    .unwrap_err();
    assert!(crate::cli::turn::turn_auth_retry::should_retry_after_auth_refresh(&failure));
    assert!(observer.get().is_none());
    // The same cloned options used by auth refresh must still publish the real successful factory.
    let result = execute_basic_cli_turn(&context, "synthetic-refreshed", None, &mut pm, options)
        .await
        .unwrap();
    assert_eq!(result.full_text, "retry result");
    assert!(observer.get().is_some());
    assert_eq!(chats.load(Ordering::SeqCst), 1);
    assert_eq!(catalogs.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn basic_cli_post_catalog_auth_failure_never_authorizes_replay() {
    use crate::cli::turn::{execute_basic_cli_turn, turn_facade::BasicCliTurnOptions};
    let app = Router::new().route(
        "/chat/stream",
        post(|| async {
            (
                axum::http::StatusCode::UNAUTHORIZED,
                "synthetic post-catalog auth denial",
            )
        }),
    );
    let base = spawn_mock(app).await;
    let api = astra_thin_client::ThinClient::new(&base, None).unwrap();
    let registry = astra_runtime::skills::empty_unified_registry();
    let context = basic_chat_context(&api, &registry, "no replay");
    let observer = crate::cli::chat_stream::CliExecutorObserver::default();
    let mut pm = PermissionManager::new(true);
    let failure = execute_basic_cli_turn(
        &context,
        "synthetic",
        None,
        &mut pm,
        BasicCliTurnOptions {
            executor_observer: Some(observer.clone()),
            ..Default::default()
        },
    )
    .await
    .unwrap_err();
    assert!(
        observer.get().is_some(),
        "this failure occurs after executor creation"
    );
    assert!(!crate::cli::turn::turn_auth_retry::should_retry_after_auth_refresh(&failure));
}
