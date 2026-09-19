//! Exercise sink installation and early flush through the shared loop entrypoint.
use super::*;
use crate::turn::agentic_loop::host::{
    HostTurnResult,
    tests::{MockHost, make_state},
};
use astra_services::event_ingestion::IngestionSender;
use astra_services::session_journal::{JournalDirGuard, JournalEventType, TraceSpanBuilder};

const OWNER: &str = "trace-loop-owner";
const SESSION: &str = "trace-loop-session";
const SPAN: &str = "trace-before-execution-exit";

struct ExitHost {
    inner: MockHost,
    cancelled: bool,
    rejected: bool,
    entered: bool,
}

#[async_trait::async_trait]
impl AgenticLoopHost for ExitHost {
    fn emit_headless_line(
        &mut self,
        style: astra_turn_core::headless_tool_body_preview::HeadlessStderrStyle,
        line: String,
    ) {
        self.inner.emit_headless_line(style, line);
    }

    fn is_quiet(&self) -> bool {
        self.inner.is_quiet()
    }

    fn valid_tool_names(&self) -> &std::collections::HashSet<String> {
        self.inner.valid_tool_names()
    }

    fn is_pre_admission_rejection(&self) -> bool {
        self.entered && self.rejected
    }

    async fn execute_turn(
        &mut self,
        state: &mut AgenticLoopState,
    ) -> Result<HostTurnResult, astra_core::ClassifiedError> {
        self.entered = true;
        // The fixture neither constructs the buffer nor binds its sink. Both
        // must already have been installed by prepare_turn_iteration.
        state
            .turn_event_buffer
            .as_mut()
            .expect("lifecycle buffer")
            .record_trace_span_v2(
                TraceSpanBuilder::default()
                    .span_id(SPAN.into())
                    .name("execution_exit_fixture".into())
                    .start_us(1)
                    .end_us(2),
            );
        if self.cancelled {
            Err(astra_core::ClassifiedError::new(
                astra_core::ErrorKind::Cancelled,
                "fixture execution cancelled",
            ))
        } else {
            self.inner.execute_turn(state).await
        }
    }
}

async fn exercise_exit(cancelled: bool, rejected: bool, writer_unavailable: bool) {
    let dir = tempfile::tempdir().unwrap();
    let blocked = tempfile::NamedTempFile::new().unwrap();
    let _guard = JournalDirGuard::new(if writer_unavailable {
        blocked.path()
    } else {
        dir.path()
    });
    let (sender, mut receiver) = IngestionSender::for_tests(64);
    let mut state = make_state();
    state.current_session_id = Some(SESSION.into());
    state.context_manifest_user_id = Some(OWNER.into());
    state.current_run_id = Some("trace-loop-run".into());
    state.current_run_owner_generation = Some(7);
    state.telemetry.trace_ingestion = Some(sender);
    assert!(state.turn_event_buffer.is_none());
    let mut host = ExitHost {
        inner: MockHost::new(Vec::new()),
        cancelled,
        rejected,
        entered: false,
    };

    let error = run_agentic_loop_with_host(&mut host, &mut state)
        .await
        .unwrap_err();
    assert!(
        host.entered,
        "must reach execution after lifecycle preparation"
    );
    assert_eq!(
        error.kind,
        if cancelled {
            astra_core::ErrorKind::Cancelled
        } else {
            astra_core::ErrorKind::BudgetExhausted
        }
    );
    let mut captured = Vec::new();
    while let Ok(event) = receiver.try_recv() {
        captured.push(event);
    }
    if rejected {
        assert!(
            captured.is_empty(),
            "pre-admission traces must not enter ingestion"
        );
        assert!(state.turn_event_buffer.is_none());
        assert!(state.interruption.is_none());
        assert!(
            astra_services::session_journal::read_journal_for_user(OWNER, SESSION)
                .unwrap()
                .is_empty()
        );
        return;
    }
    let matching: Vec<_> = captured
        .iter()
        .filter(|event| {
            event
                .metadata
                .as_ref()
                .and_then(|m| m.get("span_id"))
                .and_then(serde_json::Value::as_str)
                == Some(SPAN)
        })
        .collect();
    if writer_unavailable {
        // The error flush retains the batch when writer initialization fails;
        // the interruption branch retries that same prepared batch. Require
        // actual storage identity AND payload equality, not merely a shared
        // span ID, before accepting the second enqueue as idempotent replay.
        assert_eq!(matching.len(), 2, "error and interruption flush attempts");
        assert!(state.interruption.is_some());
        assert_eq!(
            matching[0].event_id, matching[1].event_id,
            "a retained-batch retry must preserve the content-addressed storage ID"
        );
        assert_eq!(
            serde_json::to_value(matching[0]).unwrap(),
            serde_json::to_value(matching[1]).unwrap(),
            "equal storage IDs must carry identical serialized ingestion facts"
        );
    } else {
        assert_eq!(
            matching.len(),
            1,
            "successful local flush must consume the lifecycle-bound trace"
        );
    }
    let event = matching[0];
    assert_eq!(event.user_id, OWNER);
    assert_eq!(event.session_id, SESSION);
    assert_eq!(event.metadata.as_ref().unwrap()["partial"], true);
    if writer_unavailable {
        assert!(
            !state.turn_event_buffer.as_ref().unwrap().is_empty(),
            "writer initialization failure must retain the batch after enqueue"
        );
    } else {
        assert!(state.turn_event_buffer.as_ref().unwrap().is_empty());
        let journal =
            astra_services::session_journal::read_journal_for_user(OWNER, SESSION).unwrap();
        assert!(journal.iter().any(|event| {
            event.event_type == JournalEventType::TraceSpan
                && event
                    .metadata
                    .as_ref()
                    .and_then(|m| m.get("span_id"))
                    .and_then(serde_json::Value::as_str)
                    == Some(SPAN)
        }));
    }
}

#[tokio::test]
#[serial_test::serial(session_journal_dir)]
async fn trace_ingestion_loop_error_and_cancellation_flush_lifecycle_bound_sink() {
    for cancelled in [false, true] {
        exercise_exit(cancelled, false, false).await;
    }
}

#[tokio::test]
#[serial_test::serial(session_journal_dir)]
async fn trace_ingestion_loop_error_enqueues_before_writer_initialization() {
    exercise_exit(false, false, true).await;
}

#[tokio::test]
#[serial_test::serial(session_journal_dir)]
async fn trace_ingestion_loop_pre_admission_rejection_discards_bound_buffer() {
    exercise_exit(false, true, false).await;
}
