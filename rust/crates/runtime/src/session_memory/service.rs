//! [`MemoryExtractionService`] — the single entry point for background
//! session-memory extraction.
//!
//! Produces one unified artifact per turn: an L1 markdown document
//! persisted to Memoria under the [`SESSION_MEMORY_PREFIX`] convention,
//! keyed on `session_id`. Writes go through
//! [`persist_l1`](crate::turn::cloud::session_memory_protocol::persist_l1)
//! — same path as the pre-existing bridge write, now the only path.
//!
//! Read-side consumers (compaction injection,
//! `run_lifecycle::session_end_governance`, `session_cleanup`) all
//! read from Memoria by prefix, so there's exactly one storage and one
//! schema.
//!
//! Ownership model:
//!
//! * [`SelectorParamsResolver`] + [`Arc<dyn MemoriaClient>`] are the
//!   only production dependencies. Both are injected at construction.
//!   Tests swap in [`ConstSelectorResolver`] and a minimal capturing
//!   mock client.
//! * [`SelectorHealth`] and the in-flight session set live as per-
//!   service fields — no process globals. Multi-tenant servers or
//!   parallel test runs stay isolated.
//! * Offline CLI (no Memoria connectivity) builds the service with
//!   `memoria_client: None` and the whole service becomes a no-op —
//!   see [`MemoryExtractionService::new`].

use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;

use astra_services::event_ingestion::{IngestionEvent, IngestionSender};
use astra_services::session_journal::{
    JournalEvent, SessionMemoryExtractionBreadcrumbs, SessionMemoryExtractionErrorReason,
    SessionMemoryExtractionOutcome, SessionMemoryExtractionSkipReason,
    SessionMemoryExtractionSource,
};
use astra_turn_core::cloud_session_memory_extract::SessionMemoryState;

use crate::memory_relevance::LlmConnParams;
use crate::turn::cloud::memoria_compact::MemoriaClient;

use super::activity::{BackgroundActivity, BackgroundActivityBroker};
use super::gate::{GateDecision, evaluate};
use super::health::{MemoriaAdmit, MemoriaHealth, SelectorHealth};
use super::request::{ExtractionRequest, SpawnDecision};
use super::runner::{ExtractionArtifacts, run_extraction};

/// Hard upper bound on one LLM call. Memory extraction is background
/// work; a hung call must never linger past this.
pub const LLM_TIMEOUT: Duration = Duration::from_secs(30);

/// Output token budget for extraction responses. `max_total_tokens` on
/// [`SessionMemoryExtractConfig`] (~12K) already bounds the document;
/// this keeps per-call cost predictable on pricier selectors.
pub const EXTRACTION_MAX_OUTPUT_TOKENS: usize = 4096;

// ───────────────────────────────────────────────────────────────────────
// Selector-params resolution (async trait so tests can swap in a const)
// ───────────────────────────────────────────────────────────────────────

/// Resolve the cheap selector-tagged LLM params used by the extractor.
/// Called once per extraction attempt.
#[async_trait]
pub trait SelectorParamsResolver: Send + Sync + std::fmt::Debug {
    async fn resolve(&self) -> Option<LlmConnParams>;
}

/// Always returns the same params. Unit tests.
#[derive(Debug)]
pub struct ConstSelectorResolver(pub Option<LlmConnParams>);

#[async_trait]
impl SelectorParamsResolver for ConstSelectorResolver {
    async fn resolve(&self) -> Option<LlmConnParams> {
        self.0.clone()
    }
}

// ───────────────────────────────────────────────────────────────────────
// The service
// ───────────────────────────────────────────────────────────────────────

/// Per-process background extraction coordinator. Build once at
/// server/CLI boot, hold an [`Arc`] on
/// [`crate::turn::agentic_loop_host::AgenticLoopState`].
pub struct MemoryExtractionService {
    selector_resolver: Arc<dyn SelectorParamsResolver>,
    memoria_client: Arc<dyn MemoriaClient>,
    ingestion: IngestionSender,
    user_id: Arc<str>,
    health: Arc<SelectorHealth>,
    memoria_health: Arc<MemoriaHealth>,
    in_flight: Arc<tokio::sync::Mutex<std::collections::HashSet<String>>>,
    broker: Arc<BackgroundActivityBroker>,
    /// Counter of background workers that have been spawned but not yet
    /// finished. Callers (CLI `session_cleanup`, server drain) use this
    /// via [`Self::wait_for_pending`] to hold shutdown open until the
    /// spawned Memoria writes actually land. Without this, CLI process
    /// exit routinely kills the extraction task mid-HTTP and the L1
    /// never persists — observable as "gate said Run but no
    /// session_memory_extraction event / no Memoria row".
    pending: Arc<std::sync::atomic::AtomicUsize>,
    /// Notifier the worker wakes when `pending` reaches zero. Pair with
    /// `pending` as a lightweight wait-for-empty primitive.
    pending_done: Arc<tokio::sync::Notify>,
    /// Per-session debounce state. Owned by the service so it survives
    /// the per-turn `AgenticLoopState` rebuild — the previous design
    /// stored this on `AgenticLoopState` and lost `initialized` /
    /// `tokens_at_last_extraction` every turn, making the growth-delta
    /// branch of the gate unreachable. Entries are removed on
    /// [`Self::forget_session`] (session end).
    session_states: Arc<std::sync::Mutex<std::collections::HashMap<String, SessionMemoryState>>>,
}

impl std::fmt::Debug for MemoryExtractionService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MemoryExtractionService")
            .field("user_id", &self.user_id)
            .field("subscribers", &self.broker.subscriber_count())
            .finish()
    }
}

impl MemoryExtractionService {
    /// Build a service. `memoria_client` is required — callers that
    /// can't produce one (offline CLI, no Memoria configured) should
    /// simply skip constructing the service and leave
    /// `AgenticLoopState::memory_extraction_service = None`.
    pub fn new(
        selector_resolver: Arc<dyn SelectorParamsResolver>,
        memoria_client: Arc<dyn MemoriaClient>,
        ingestion: IngestionSender,
        user_id: impl Into<Arc<str>>,
        broker: Arc<BackgroundActivityBroker>,
    ) -> Self {
        Self {
            selector_resolver,
            memoria_client,
            ingestion,
            user_id: user_id.into(),
            health: Arc::new(SelectorHealth::new()),
            memoria_health: Arc::new(MemoriaHealth::new()),
            in_flight: Arc::new(tokio::sync::Mutex::new(std::collections::HashSet::new())),
            broker,
            pending: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            pending_done: Arc::new(tokio::sync::Notify::new()),
            session_states: Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
        }
    }

    /// Release per-session debounce state. Callers invoke this at
    /// session end so the service doesn't leak state across an unbounded
    /// number of historical sessions. Idempotent.
    pub fn forget_session(&self, session_id: &str) {
        if let Ok(mut map) = self.session_states.lock() {
            map.remove(session_id);
        }
    }

    /// Test-only probe: inspect the per-session debounce state.
    #[cfg(test)]
    pub(crate) fn peek_state(&self, session_id: &str) -> Option<SessionMemoryState> {
        self.session_states
            .lock()
            .ok()
            .and_then(|m| m.get(session_id).cloned())
    }

    /// Block until every spawned worker has finished or `timeout` elapses.
    /// Returns the number of workers that were still in flight when the
    /// wait gave up (0 = clean drain, >0 = worker was killed on timeout).
    pub async fn wait_for_pending(&self, timeout: Duration) -> usize {
        use std::sync::atomic::Ordering;
        let deadline = Instant::now() + timeout;
        loop {
            let n = self.pending.load(Ordering::Acquire);
            if n == 0 {
                return 0;
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return n;
            }
            // Notified() must be set up BEFORE re-reading pending to
            // avoid the lost-wakeup race: if the worker fires notify
            // between our check and our sleep, notified() still fires.
            let notified = self.pending_done.notified();
            tokio::pin!(notified);
            if self.pending.load(Ordering::Acquire) == 0 {
                return 0;
            }
            match tokio::time::timeout(remaining, &mut notified).await {
                Ok(()) => continue, // re-check pending on next iter
                Err(_) => return self.pending.load(Ordering::Acquire),
            }
        }
    }

    pub fn broker(&self) -> Arc<BackgroundActivityBroker> {
        Arc::clone(&self.broker)
    }

    /// Synchronous entry point. Evaluates the gate against the service's
    /// own per-session debounce state, emits a skip event inline when
    /// rejected, advances the debounce state and spawns the async worker
    /// when admitted.
    ///
    /// **Must run inside a Tokio runtime.**
    pub fn maybe_spawn(self: &Arc<Self>, req: ExtractionRequest) -> SpawnDecision {
        // Snapshot state under lock so the gate evaluates a consistent
        // view; `mark_extracted` below happens under the same lock
        // acquire so two near-simultaneous calls don't both see
        // `!initialized` and both admit.
        let decision = {
            let map = match self.session_states.lock() {
                Ok(m) => m,
                Err(p) => p.into_inner(),
            };
            let state_ref = map.get(&req.session_id);
            let default_state;
            let state = match state_ref {
                Some(s) => s,
                None => {
                    default_state = SessionMemoryState::default();
                    &default_state
                }
            };
            evaluate(
                state,
                &req.session_id,
                req.current_tokens,
                req.current_tool_calls,
                req.had_error,
                &req.config,
            )
        };
        // Breadcrumbs for sync-path skip events. `selector_model` and
        // `attempt` only make sense in the async worker after LLM
        // resolve / persist attempt.
        let skip_breadcrumbs = SessionMemoryExtractionBreadcrumbs {
            messages_count: Some(req.messages.len() as u32),
            selector_model: None,
            attempt: None,
        };
        if let GateDecision::Skip(reason) = decision {
            self.emit_skip_event(
                if req.session_id.is_empty() {
                    None
                } else {
                    Some(&req.session_id)
                },
                req.turn_number,
                reason,
                &skip_breadcrumbs,
            );
            return SpawnDecision::Skipped;
        }

        // Memoria circuit breaker: fail fast when the endpoint has
        // tripped. Without this, every turn where the gate passes
        // would still pile on HTTP attempts (two per turn in the worst
        // case: retrieve + store with retry) against the unreachable
        // Memoria host. The breaker keeps the work local and makes
        // recovery automatic once the cooldown elapses.
        //
        // Placement: AFTER the gate (so pure-decision skips like
        // `no_growth` continue to work as debounce signals even when
        // Memoria is down) but BEFORE the in-flight claim (so we don't
        // occupy the slot with a doomed attempt).
        match self.memoria_health.admit() {
            MemoriaAdmit::Closed | MemoriaAdmit::HalfOpenProbe => {
                // Proceed. The spawn worker records success/failure
                // after the persist attempt.
            }
            MemoriaAdmit::Open => {
                self.emit_skip_event(
                    Some(&req.session_id),
                    req.turn_number,
                    SessionMemoryExtractionSkipReason::MemoriaUnhealthy,
                    &skip_breadcrumbs,
                );
                return SpawnDecision::Skipped;
            }
        }

        // Try to claim the in-flight slot synchronously — if this
        // session already has an extraction running, skip.
        let in_flight = Arc::clone(&self.in_flight);
        if let Ok(mut set) = in_flight.try_lock() {
            if !set.insert(req.session_id.clone()) {
                self.emit_skip_event(
                    Some(&req.session_id),
                    req.turn_number,
                    SessionMemoryExtractionSkipReason::InFlight,
                    &skip_breadcrumbs,
                );
                return SpawnDecision::Skipped;
            }
        } else {
            // Someone else is holding the lock mid-check — treat as
            // in-flight rather than retrying or waiting.
            self.emit_skip_event(
                Some(&req.session_id),
                req.turn_number,
                SessionMemoryExtractionSkipReason::InFlight,
                &skip_breadcrumbs,
            );
            return SpawnDecision::Skipped;
        }

        // Admitted. Advance debounce so the next turn doesn't re-trigger
        // on the same growth window even if this attempt falls back to
        // rule-based content. Per-session state survives turn boundaries.
        if let Ok(mut map) = self.session_states.lock() {
            let entry = map.entry(req.session_id.clone()).or_default();
            entry.mark_extracted(req.current_tokens, req.current_tool_calls);
        }

        // Track in-flight workers so shutdown can drain them. The
        // counter is `fetch_add`'d synchronously BEFORE the spawn so a
        // caller that calls `wait_for_pending` immediately after
        // `maybe_spawn` still sees the task. The counter is decremented
        // in the spawned task, and `pending_done` is notified so waiters
        // can re-check without busy-polling.
        use std::sync::atomic::Ordering;
        self.pending.fetch_add(1, Ordering::AcqRel);
        let svc = Arc::clone(self);
        let pending = Arc::clone(&self.pending);
        let pending_done = Arc::clone(&self.pending_done);
        tokio::spawn(async move {
            svc.run_one(req).await;
            if pending.fetch_sub(1, Ordering::AcqRel) == 1 {
                // Went from 1 → 0: wake every waiter.
                pending_done.notify_waiters();
            }
        });
        SpawnDecision::Spawned
    }

    // ── internals ─────────────────────────────────────────────────────

    async fn run_one(self: Arc<Self>, req: ExtractionRequest) {
        let session_id = req.session_id.clone();
        let turn = req.turn_number;
        let messages_count = req.messages.len() as u32;
        let started = Instant::now();
        if std::env::var("ASTRA_SESSION_MEMORY_TRACE").is_ok() {
            eprintln!(
                "[run_one] start sid={} turn={} msgs={} tokens={}",
                session_id.get(..8).unwrap_or(&session_id),
                turn,
                messages_count,
                req.current_tokens,
            );
        }

        let selector_params = self.selector_resolver.resolve().await;
        let selector_healthy = match selector_params.as_ref() {
            Some(p) => self.health.is_healthy(&p.model_name),
            None => false,
        };

        if selector_params.is_some() && !selector_healthy {
            let bc = SessionMemoryExtractionBreadcrumbs {
                messages_count: Some(messages_count),
                selector_model: selector_params.as_ref().map(|p| p.model_name.clone()),
                attempt: None,
            };
            self.emit_skip_event(
                Some(&session_id),
                turn,
                SessionMemoryExtractionSkipReason::SelectorCooldown,
                &bc,
            );
            self.release_in_flight(&session_id).await;
            return;
        }

        if selector_healthy && selector_params.is_some() {
            self.broker.emit(BackgroundActivity::Started {
                session_id: session_id.clone(),
                turn,
            });
        }

        let effective_selector = if selector_healthy {
            selector_params
        } else {
            None
        };
        let params_for_health = effective_selector.as_ref().map(|p| p.model_name.clone());

        // Fetch current L1 so the extraction prompt can build on it.
        // Any retrieve failure (Memoria offline, auth) → treat as no
        // prior memory; the next write will just reset state.
        let current_memory = self.load_current_memory(&session_id).await;

        let artifacts = run_extraction(
            &self.memoria_client,
            &session_id,
            &req.messages,
            turn as usize,
            req.current_tokens,
            &current_memory,
            effective_selector.as_ref(),
            LLM_TIMEOUT,
            EXTRACTION_MAX_OUTPUT_TOKENS,
        )
        .await;
        let duration_ms = started.elapsed().as_millis() as u64;
        if std::env::var("ASTRA_SESSION_MEMORY_TRACE").is_ok() {
            let tag = match &artifacts {
                ExtractionArtifacts::Persisted {
                    source,
                    bytes_written,
                    store_attempt,
                } => {
                    format!(
                        "Persisted{{source={source:?}, bytes={bytes_written}, attempt={store_attempt}}}"
                    )
                }
                ExtractionArtifacts::LlmFailedPersistedFallback {
                    error_reason,
                    bytes_written,
                    store_attempt,
                } => {
                    format!(
                        "LlmFailedPersistedFallback{{err={error_reason:?}, bytes={bytes_written}, attempt={store_attempt}}}"
                    )
                }
                ExtractionArtifacts::PersistFailed { error_reason } => {
                    format!("PersistFailed{{err={error_reason:?}}}")
                }
            };
            eprintln!(
                "[run_one] done sid={} dur={}ms outcome={}",
                session_id.get(..8).unwrap_or(&session_id),
                duration_ms,
                tag,
            );
        }

        // Model name surfaced in events is what the worker actually
        // attempted — not what the resolver gave us, since LLM-path
        // source==Llm means the selector was both resolved and healthy.
        let selector_model_used = params_for_health.clone();

        match artifacts {
            ExtractionArtifacts::Persisted {
                source,
                bytes_written,
                store_attempt,
            } => {
                // Memoria accepted a write → breaker closes (or stays
                // closed) and the consecutive-failure counter resets.
                self.memoria_health.record_success();
                self.broker.emit(BackgroundActivity::Finished {
                    session_id: session_id.clone(),
                    turn,
                    source,
                    duration_ms,
                });
                let bc = SessionMemoryExtractionBreadcrumbs {
                    messages_count: Some(messages_count),
                    selector_model: match source {
                        SessionMemoryExtractionSource::Llm => selector_model_used.clone(),
                        SessionMemoryExtractionSource::RuleFallback => None,
                    },
                    attempt: Some(store_attempt),
                };
                self.emit_success_event(
                    Some(&session_id),
                    turn,
                    source,
                    bytes_written,
                    duration_ms,
                    &bc,
                );
            }
            ExtractionArtifacts::LlmFailedPersistedFallback {
                error_reason,
                bytes_written,
                store_attempt,
            } => {
                if let Some(name) = params_for_health.as_deref() {
                    self.health.mark_failed(name);
                }
                // Memoria persist still succeeded on this branch, so
                // the circuit breaker resets. Only the LLM selector
                // model is marked unhealthy.
                self.memoria_health.record_success();
                // LLM failed but rule-based content did land. Record
                // both the error (for observability) and the write (so
                // the broker reflects that memory is up-to-date, just
                // via fallback).
                let bc = SessionMemoryExtractionBreadcrumbs {
                    messages_count: Some(messages_count),
                    selector_model: selector_model_used.clone(),
                    attempt: Some(store_attempt),
                };
                self.emit_error_event(Some(&session_id), turn, error_reason, duration_ms, &bc);
                self.broker.emit(BackgroundActivity::Finished {
                    session_id: session_id.clone(),
                    turn,
                    source: SessionMemoryExtractionSource::RuleFallback,
                    duration_ms,
                });
                let _ = bytes_written;
            }
            ExtractionArtifacts::PersistFailed { error_reason } => {
                // Memoria persist failed → breaker counts it. Enough
                // consecutive failures trip the breaker and skip
                // future `maybe_spawn` until the cooldown elapses.
                self.memoria_health.record_failure();
                let bc = SessionMemoryExtractionBreadcrumbs {
                    messages_count: Some(messages_count),
                    selector_model: selector_model_used.clone(),
                    // `attempt` is unavailable on PersistFailed since
                    // run_extraction doesn't surface partial-attempt
                    // counts when nothing landed; use None so the
                    // field is omitted rather than misleadingly 0.
                    attempt: None,
                };
                self.emit_error_event(Some(&session_id), turn, error_reason, duration_ms, &bc);
                self.broker.emit(BackgroundActivity::Errored {
                    session_id: session_id.clone(),
                    turn,
                    reason: error_reason,
                    duration_ms,
                });
            }
        }

        self.release_in_flight(&session_id).await;
    }

    async fn load_current_memory(&self, session_id: &str) -> String {
        use crate::turn::cloud::session_memory_protocol::{SESSION_MEMORY_PREFIX, pick_latest_l1};
        let Ok(memories) = self
            .memoria_client
            .retrieve_ext(
                &format!("{SESSION_MEMORY_PREFIX} session state"),
                Some(session_id),
                3,
                true,
            )
            .await
        else {
            return String::new();
        };
        pick_latest_l1(&memories)
            .map(|m| m.content.clone())
            .unwrap_or_default()
    }

    async fn release_in_flight(&self, session_id: &str) {
        let mut set = self.in_flight.lock().await;
        set.remove(session_id);
    }

    // ── event emission helpers ────────────────────────────────────────

    fn enqueue(&self, event: JournalEvent) {
        let ingestion_event = IngestionEvent::from_journal_event(&event, &self.user_id);
        self.ingestion.enqueue(ingestion_event);
    }

    fn emit_skip_event(
        &self,
        session_id: Option<&str>,
        turn: u32,
        reason: SessionMemoryExtractionSkipReason,
        breadcrumbs: &SessionMemoryExtractionBreadcrumbs,
    ) {
        self.enqueue(JournalEvent::session_memory_extraction(
            session_id,
            turn,
            0,
            SessionMemoryExtractionOutcome::Skipped { reason },
            breadcrumbs,
        ));
    }

    fn emit_success_event(
        &self,
        session_id: Option<&str>,
        turn: u32,
        source: SessionMemoryExtractionSource,
        bytes_written: u64,
        duration_ms: u64,
        breadcrumbs: &SessionMemoryExtractionBreadcrumbs,
    ) {
        self.enqueue(JournalEvent::session_memory_extraction(
            session_id,
            turn,
            duration_ms,
            SessionMemoryExtractionOutcome::Extracted {
                source,
                bytes_written,
            },
            breadcrumbs,
        ));
    }

    fn emit_error_event(
        &self,
        session_id: Option<&str>,
        turn: u32,
        reason: SessionMemoryExtractionErrorReason,
        duration_ms: u64,
        breadcrumbs: &SessionMemoryExtractionBreadcrumbs,
    ) {
        self.enqueue(JournalEvent::session_memory_extraction(
            session_id,
            turn,
            duration_ms,
            SessionMemoryExtractionOutcome::Errored { reason },
            breadcrumbs,
        ));
    }
}

// ───────────────────────────────────────────────────────────────────────
// Tests
// ───────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::turn::cloud::memoria_compact::MemoriaMemory;
    use astra_services::event_ingestion::IngestionEvent;
    use astra_turn_core::cloud_session_memory_extract::SessionMemoryExtractConfig;
    use serde_json::json;
    use std::sync::Mutex;

    /// Minimal capturing mock — records every `store` for assertion.
    #[derive(Default)]
    struct CapturingMemoria {
        stored: Mutex<Vec<(String, String, Option<String>)>>, // (content, memory_type, session_id)
        purged: Mutex<Vec<String>>,
    }

    #[async_trait]
    impl MemoriaClient for CapturingMemoria {
        async fn retrieve_ext(
            &self,
            _query: &str,
            _session_id: Option<&str>,
            _top_k: usize,
            _filter_session: bool,
        ) -> Result<Vec<MemoriaMemory>, String> {
            Ok(Vec::new())
        }

        async fn store(
            &self,
            content: &str,
            memory_type: &str,
            session_id: Option<&str>,
            _trust_tier: Option<&str>,
        ) -> Result<String, String> {
            self.stored.lock().unwrap().push((
                content.to_string(),
                memory_type.to_string(),
                session_id.map(str::to_string),
            ));
            Ok(format!("mem-{}", self.stored.lock().unwrap().len()))
        }

        async fn purge_working(&self, session_id: &str) -> Result<u64, String> {
            self.purged.lock().unwrap().push(session_id.to_string());
            Ok(0)
        }
    }

    struct TestCtx {
        svc: Arc<MemoryExtractionService>,
        rx: tokio::sync::mpsc::Receiver<IngestionEvent>,
        broker: Arc<BackgroundActivityBroker>,
        memoria: Arc<CapturingMemoria>,
    }

    fn build_ctx(selector: Option<LlmConnParams>) -> TestCtx {
        let (ingestion, rx) = IngestionSender::for_tests(256);
        let broker = Arc::new(BackgroundActivityBroker::new());
        let memoria = Arc::new(CapturingMemoria::default());
        let svc = Arc::new(MemoryExtractionService::new(
            Arc::new(ConstSelectorResolver(selector)),
            Arc::clone(&memoria) as Arc<dyn MemoriaClient>,
            ingestion,
            "test-user",
            Arc::clone(&broker),
        ));
        TestCtx {
            svc,
            rx,
            broker,
            memoria,
        }
    }

    fn sample_req(session_id: &str, tokens: usize, had_error: bool) -> ExtractionRequest {
        ExtractionRequest {
            session_id: session_id.to_string(),
            messages: vec![json!({"role": "user", "content": "hello world"})],
            current_tokens: tokens,
            current_tool_calls: 0,
            had_error,
            turn_number: 1,
            config: SessionMemoryExtractConfig::default(),
        }
    }

    fn collect_extraction_events(
        rx: &mut tokio::sync::mpsc::Receiver<IngestionEvent>,
    ) -> Vec<IngestionEvent> {
        let mut out = Vec::new();
        while let Ok(evt) = rx.try_recv() {
            if evt.event_type == "session_memory_extraction" {
                out.push(evt);
            }
        }
        out
    }

    async fn wait_for_memoria_store(memoria: &Arc<CapturingMemoria>) {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if !memoria.stored.lock().unwrap().is_empty() {
                return;
            }
            if Instant::now() >= deadline {
                panic!("no Memoria store landed within 5s");
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    fn nanos() -> u128 {
        use std::time::{SystemTime, UNIX_EPOCH};
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    }

    #[tokio::test]
    async fn skip_below_gate_emits_skipped_event_with_reason() {
        let mut ctx = build_ctx(None);
        let req = sample_req("sess-below", 1_000, false);
        assert_eq!(ctx.svc.maybe_spawn(req), SpawnDecision::Skipped);

        let events = collect_extraction_events(&mut ctx.rx);
        assert_eq!(events.len(), 1);
        let m = events[0].metadata.as_ref().unwrap();
        assert_eq!(m["outcome"], "skipped");
        assert_eq!(m["reason"], "below_init_gate");
        assert!(ctx.memoria.stored.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn no_selector_persists_rule_based_to_memoria() {
        let mut ctx = build_ctx(None);
        let sid = format!("no-sel-{}", nanos());
        let req = sample_req(&sid, 50_000, false);
        assert_eq!(ctx.svc.maybe_spawn(req), SpawnDecision::Spawned);
        wait_for_memoria_store(&ctx.memoria).await;

        // Exactly one `store` call with memory_type=working, same session_id.
        let stored = ctx.memoria.stored.lock().unwrap().clone();
        assert_eq!(stored.len(), 1, "expected 1 store, got {stored:?}");
        let (content, memory_type, stored_sid) = &stored[0];
        assert_eq!(memory_type, "working");
        assert_eq!(stored_sid.as_deref(), Some(sid.as_str()));
        assert!(
            content.starts_with(crate::turn::cloud::session_memory_protocol::SESSION_MEMORY_PREFIX),
            "content should be prefixed; got: {content:.60}…"
        );

        // Journal event: extracted + rule_fallback.
        let events = collect_extraction_events(&mut ctx.rx);
        let extracted: Vec<&IngestionEvent> = events
            .iter()
            .filter(|e| {
                e.metadata.as_ref().and_then(|m| m.get("outcome")) == Some(&json!("extracted"))
            })
            .collect();
        assert_eq!(extracted.len(), 1, "events: {events:?}");
        assert_eq!(
            extracted[0].metadata.as_ref().unwrap()["source"],
            "rule_fallback"
        );
    }

    #[tokio::test]
    async fn persist_purges_previous_l1_before_store() {
        let ctx = build_ctx(None);
        let sid = format!("purge-{}", nanos());
        let req = sample_req(&sid, 50_000, false);
        assert_eq!(ctx.svc.maybe_spawn(req), SpawnDecision::Spawned);
        wait_for_memoria_store(&ctx.memoria).await;

        // persist_l1 calls `purge_working(sid)` before `store`.
        let purged = ctx.memoria.purged.lock().unwrap().clone();
        assert_eq!(purged, vec![sid]);
    }

    #[tokio::test]
    async fn in_flight_dedup_emits_skipped_in_flight() {
        let mut ctx = build_ctx(None);
        let sid = format!("in-flight-{}", nanos());
        {
            let mut set = ctx.svc.in_flight.lock().await;
            set.insert(sid.clone());
        }
        let req = sample_req(&sid, 50_000, false);
        assert_eq!(ctx.svc.maybe_spawn(req), SpawnDecision::Skipped);

        let events = collect_extraction_events(&mut ctx.rx);
        assert!(events.iter().any(|e| {
            let m = e.metadata.as_ref().unwrap();
            m["outcome"] == "skipped" && m["reason"] == "in_flight"
        }));
    }

    #[tokio::test]
    async fn broker_does_not_fire_started_for_rule_based_path() {
        let ctx = build_ctx(None);
        let mut sub = ctx.broker.subscribe();
        let sid = format!("no-start-{}", nanos());
        let req = sample_req(&sid, 50_000, false);
        assert_eq!(ctx.svc.maybe_spawn(req), SpawnDecision::Spawned);
        wait_for_memoria_store(&ctx.memoria).await;

        let mut saw_started = false;
        while let Ok(evt) = sub.try_recv() {
            if matches!(evt, BackgroundActivity::Started { .. }) {
                saw_started = true;
            }
        }
        assert!(
            !saw_started,
            "Started should only fire when an LLM call is actually attempted"
        );
    }

    // ── unhappy paths ─────────────────────────────────────────────────

    /// Mock that lets tests script purge + retrieve behaviour.
    struct ScriptedMemoria {
        purge_fail_permanently: bool,
        store_fail_permanently: bool,
        retrieve_fail: bool,
        stored: Mutex<Vec<String>>,
    }

    impl ScriptedMemoria {
        fn new() -> Self {
            Self {
                purge_fail_permanently: false,
                store_fail_permanently: false,
                retrieve_fail: false,
                stored: Mutex::new(Vec::new()),
            }
        }
    }

    #[async_trait]
    impl MemoriaClient for ScriptedMemoria {
        async fn retrieve_ext(
            &self,
            _q: &str,
            _sid: Option<&str>,
            _k: usize,
            _f: bool,
        ) -> Result<Vec<MemoriaMemory>, String> {
            if self.retrieve_fail {
                Err("retrieve flaked".to_string())
            } else {
                Ok(Vec::new())
            }
        }
        async fn store(
            &self,
            content: &str,
            _ty: &str,
            _sid: Option<&str>,
            _t: Option<&str>,
        ) -> Result<String, String> {
            if self.store_fail_permanently {
                Err("store down".to_string())
            } else {
                self.stored.lock().unwrap().push(content.to_string());
                Ok("ok".to_string())
            }
        }
        async fn purge_working(&self, _sid: &str) -> Result<u64, String> {
            if self.purge_fail_permanently {
                Err("purge down".to_string())
            } else {
                Ok(0)
            }
        }
    }

    fn build_ctx_with_memoria(
        selector: Option<LlmConnParams>,
        memoria: Arc<dyn MemoriaClient>,
    ) -> (
        Arc<MemoryExtractionService>,
        tokio::sync::mpsc::Receiver<IngestionEvent>,
        Arc<BackgroundActivityBroker>,
    ) {
        let (ingestion, rx) = IngestionSender::for_tests(256);
        let broker = Arc::new(BackgroundActivityBroker::new());
        let svc = Arc::new(MemoryExtractionService::new(
            Arc::new(ConstSelectorResolver(selector)),
            memoria,
            ingestion,
            "test-user",
            Arc::clone(&broker),
        ));
        (svc, rx, broker)
    }

    #[tokio::test]
    async fn persist_failure_emits_purge_failed_event_without_store() {
        let memoria = Arc::new(ScriptedMemoria {
            purge_fail_permanently: true,
            ..ScriptedMemoria::new()
        });
        let (svc, mut rx, _broker) =
            build_ctx_with_memoria(None, Arc::clone(&memoria) as Arc<dyn MemoriaClient>);
        let sid = format!("purge-fail-{}", nanos());
        let req = sample_req(&sid, 50_000, false);
        assert_eq!(svc.maybe_spawn(req), SpawnDecision::Spawned);

        // Wait for the error event to surface.
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut saw_purge_failed = false;
        while Instant::now() < deadline && !saw_purge_failed {
            while let Ok(evt) = rx.try_recv() {
                if evt.event_type != "session_memory_extraction" {
                    continue;
                }
                let m = evt.metadata.as_ref().unwrap();
                if m["outcome"] == "errored" && m["reason"] == "purge_failed" {
                    saw_purge_failed = true;
                }
            }
            if !saw_purge_failed {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        }
        assert!(saw_purge_failed, "expected errored{{purge_failed}} event");
        assert!(
            memoria.stored.lock().unwrap().is_empty(),
            "no store should have happened after exhausted purge retries"
        );
    }

    #[tokio::test]
    async fn store_failure_emits_write_failed_event() {
        let memoria = Arc::new(ScriptedMemoria {
            store_fail_permanently: true,
            ..ScriptedMemoria::new()
        });
        let (svc, mut rx, _broker) =
            build_ctx_with_memoria(None, Arc::clone(&memoria) as Arc<dyn MemoriaClient>);
        let sid = format!("store-fail-{}", nanos());
        let req = sample_req(&sid, 50_000, false);
        assert_eq!(svc.maybe_spawn(req), SpawnDecision::Spawned);

        let deadline = Instant::now() + Duration::from_secs(5);
        let mut saw_write_failed = false;
        while Instant::now() < deadline && !saw_write_failed {
            while let Ok(evt) = rx.try_recv() {
                if evt.event_type != "session_memory_extraction" {
                    continue;
                }
                let m = evt.metadata.as_ref().unwrap();
                if m["outcome"] == "errored" && m["reason"] == "write_failed" {
                    saw_write_failed = true;
                }
            }
            if !saw_write_failed {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        }
        assert!(saw_write_failed, "expected errored{{write_failed}} event");
    }

    #[tokio::test]
    async fn retrieve_failure_falls_back_to_empty_current_memory_and_still_writes() {
        // Tests that `load_current_memory` errors don't kill the
        // extraction — the prompt just sees empty current memory.
        let memoria = Arc::new(ScriptedMemoria {
            retrieve_fail: true,
            ..ScriptedMemoria::new()
        });
        let (svc, _rx, _broker) =
            build_ctx_with_memoria(None, Arc::clone(&memoria) as Arc<dyn MemoriaClient>);
        let sid = format!("retrieve-fail-{}", nanos());
        let req = sample_req(&sid, 50_000, false);
        assert_eq!(svc.maybe_spawn(req), SpawnDecision::Spawned);

        // Extraction should still produce a store via rule-based path.
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if !memoria.stored.lock().unwrap().is_empty() {
                break;
            }
            if Instant::now() >= deadline {
                panic!("extraction didn't survive a retrieve failure");
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    #[tokio::test]
    async fn wait_for_pending_returns_zero_when_worker_finishes() {
        let memoria = Arc::new(CapturingMemoria::default());
        let (svc, _rx, _broker) =
            build_ctx_with_memoria(None, Arc::clone(&memoria) as Arc<dyn MemoriaClient>);
        let sid = format!("drain-ok-{}", nanos());
        let req = sample_req(&sid, 50_000, false);
        assert_eq!(svc.maybe_spawn(req), SpawnDecision::Spawned);

        // Generous timeout — the worker writes to the in-memory mock
        // which returns synchronously; in practice it finishes in ms.
        let leftover = svc.wait_for_pending(Duration::from_secs(2)).await;
        assert_eq!(leftover, 0, "worker should have drained cleanly");
        assert_eq!(
            memoria.stored.lock().unwrap().len(),
            1,
            "worker must have completed its store before drain returned"
        );
    }

    /// If `wait_for_pending` returned before the worker finished the
    /// store, this would be zero — proves the drain actually waits for
    /// the Memoria write and not just the `maybe_spawn` synchronous part.
    #[tokio::test]
    async fn wait_for_pending_blocks_until_store_completes() {
        use std::sync::atomic::{AtomicBool, Ordering};
        // Slow mock: store blocks for 200ms before returning.
        struct SlowMemoria {
            stored: Mutex<bool>,
            completed: Arc<AtomicBool>,
        }
        #[async_trait]
        impl MemoriaClient for SlowMemoria {
            async fn retrieve_ext(
                &self,
                _: &str,
                _: Option<&str>,
                _: usize,
                _: bool,
            ) -> Result<Vec<crate::turn::cloud::memoria_compact::MemoriaMemory>, String>
            {
                Ok(Vec::new())
            }
            async fn store(
                &self,
                _: &str,
                _: &str,
                _: Option<&str>,
                _: Option<&str>,
            ) -> Result<String, String> {
                tokio::time::sleep(Duration::from_millis(200)).await;
                *self.stored.lock().unwrap() = true;
                self.completed.store(true, Ordering::Release);
                Ok("slow-ok".into())
            }
            async fn purge_working(&self, _: &str) -> Result<u64, String> {
                Ok(0)
            }
        }
        let completed = Arc::new(AtomicBool::new(false));
        let slow = Arc::new(SlowMemoria {
            stored: Mutex::new(false),
            completed: Arc::clone(&completed),
        });
        let (svc, _rx, _broker) =
            build_ctx_with_memoria(None, Arc::clone(&slow) as Arc<dyn MemoriaClient>);
        let req = sample_req("slow-1", 50_000, false);
        assert_eq!(svc.maybe_spawn(req), SpawnDecision::Spawned);

        // `maybe_spawn` returned immediately (that's the whole point of
        // spawn-dedupe). The store is still in flight.
        assert!(!completed.load(Ordering::Acquire));

        // wait_for_pending must block until the store returns.
        let started = Instant::now();
        let leftover = svc.wait_for_pending(Duration::from_secs(2)).await;
        let elapsed = started.elapsed();

        assert_eq!(leftover, 0);
        assert!(
            completed.load(Ordering::Acquire),
            "wait must not return before store completed"
        );
        assert!(
            elapsed >= Duration::from_millis(150),
            "wait should cover the 200ms store latency, got {elapsed:?}"
        );
    }

    #[tokio::test]
    async fn wait_for_pending_times_out_on_hung_worker() {
        // Worker that never resolves — simulates a hung Memoria HTTP call.
        struct HangingMemoria;
        #[async_trait]
        impl MemoriaClient for HangingMemoria {
            async fn retrieve_ext(
                &self,
                _: &str,
                _: Option<&str>,
                _: usize,
                _: bool,
            ) -> Result<Vec<crate::turn::cloud::memoria_compact::MemoriaMemory>, String>
            {
                Ok(Vec::new())
            }
            async fn store(
                &self,
                _: &str,
                _: &str,
                _: Option<&str>,
                _: Option<&str>,
            ) -> Result<String, String> {
                // Hang forever.
                std::future::pending::<()>().await;
                unreachable!()
            }
            async fn purge_working(&self, _: &str) -> Result<u64, String> {
                Ok(0)
            }
        }
        let (svc, _rx, _broker) =
            build_ctx_with_memoria(None, Arc::new(HangingMemoria) as Arc<dyn MemoriaClient>);
        let req = sample_req("hang-1", 50_000, false);
        assert_eq!(svc.maybe_spawn(req), SpawnDecision::Spawned);

        // Short timeout — we expect leftover == 1 because the worker
        // never completes.
        let leftover = svc.wait_for_pending(Duration::from_millis(200)).await;
        assert_eq!(leftover, 1, "hung worker must count as leftover");
    }

    #[tokio::test]
    async fn wait_for_pending_returns_zero_immediately_when_nothing_spawned() {
        let (svc, _rx, _broker) = build_ctx_with_memoria(
            None,
            Arc::new(CapturingMemoria::default()) as Arc<dyn MemoriaClient>,
        );
        let started = Instant::now();
        let leftover = svc.wait_for_pending(Duration::from_secs(10)).await;
        assert_eq!(leftover, 0);
        assert!(
            started.elapsed() < Duration::from_millis(50),
            "should not sleep when nothing is in flight"
        );
    }

    #[tokio::test]
    async fn selector_cooldown_fires_after_llm_failure() {
        // This test exercises the health-map side of the service:
        // after the runner reports an LLM error, `selector_healthy`
        // should flip the next attempt into SelectorCooldown.
        // Because we can't easily mock an LLM failure inline (runner
        // hits real http), directly mark the selector unhealthy on the
        // service's health map and verify maybe_spawn's next run emits
        // the cooldown skip event.
        let selector_params = LlmConnParams {
            base_url: "https://nope.invalid".to_string(),
            api_key: "k".to_string(),
            model_name: "cheap-selector".to_string(),
            provider: "test".to_string(),
        };
        let memoria = Arc::new(CapturingMemoria::default());
        let (svc, mut rx, _broker) = build_ctx_with_memoria(
            Some(selector_params.clone()),
            Arc::clone(&memoria) as Arc<dyn MemoriaClient>,
        );
        // Pre-populate the health map with a recent failure so the
        // next attempt finds the selector unhealthy without us having
        // to actually hit the network.
        svc.health.mark_failed(&selector_params.model_name);

        let sid = format!("cooldown-{}", nanos());
        let req = sample_req(&sid, 50_000, false);
        assert_eq!(svc.maybe_spawn(req), SpawnDecision::Spawned);

        let deadline = Instant::now() + Duration::from_secs(5);
        let mut saw_cooldown = false;
        while Instant::now() < deadline && !saw_cooldown {
            while let Ok(evt) = rx.try_recv() {
                if evt.event_type != "session_memory_extraction" {
                    continue;
                }
                let m = evt.metadata.as_ref().unwrap();
                if m["outcome"] == "skipped" && m["reason"] == "selector_cooldown" {
                    saw_cooldown = true;
                }
            }
            if !saw_cooldown {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        }
        assert!(
            saw_cooldown,
            "unhealthy selector must emit skipped{{selector_cooldown}} event"
        );
        assert!(
            memoria.stored.lock().unwrap().is_empty(),
            "cooldown must prevent any store"
        );
    }

    // ── concurrency / edge cases ─────────────────────────────────────

    /// Two parallel maybe_spawn calls on the SAME session must result
    /// in at most one background worker. The second caller must get
    /// `Skipped` with reason `in_flight`.
    #[tokio::test]
    async fn concurrent_same_session_maybe_spawn_serializes_to_one_worker() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        struct CountingMemoria {
            stored: AtomicUsize,
            purged: AtomicUsize,
        }
        #[async_trait]
        impl MemoriaClient for CountingMemoria {
            async fn retrieve_ext(
                &self,
                _: &str,
                _: Option<&str>,
                _: usize,
                _: bool,
            ) -> Result<Vec<crate::turn::cloud::memoria_compact::MemoriaMemory>, String>
            {
                Ok(Vec::new())
            }
            async fn store(
                &self,
                _: &str,
                _: &str,
                _: Option<&str>,
                _: Option<&str>,
            ) -> Result<String, String> {
                // Hold the slot long enough to make concurrency
                // deterministic — the second spawn attempt must
                // observe an in-flight guard.
                tokio::time::sleep(Duration::from_millis(200)).await;
                self.stored.fetch_add(1, Ordering::AcqRel);
                Ok("ok".into())
            }
            async fn purge_working(&self, _: &str) -> Result<u64, String> {
                self.purged.fetch_add(1, Ordering::AcqRel);
                Ok(0)
            }
        }
        let memoria = Arc::new(CountingMemoria {
            stored: AtomicUsize::new(0),
            purged: AtomicUsize::new(0),
        });
        let (svc, _rx, _broker) =
            build_ctx_with_memoria(None, Arc::clone(&memoria) as Arc<dyn MemoriaClient>);
        let sid = format!("race-{}", nanos());

        // First spawn.
        let req1 = sample_req(&sid, 50_000, false);
        let d1 = svc.maybe_spawn(req1);

        // Second spawn (same session, BEFORE first finishes).
        let req2 = sample_req(&sid, 60_000, false);
        let d2 = svc.maybe_spawn(req2);

        assert_eq!(d1, SpawnDecision::Spawned);
        assert_eq!(
            d2,
            SpawnDecision::Skipped,
            "second concurrent spawn on same session must be deduped"
        );

        svc.wait_for_pending(Duration::from_secs(2)).await;
        assert_eq!(
            memoria.stored.load(Ordering::Acquire),
            1,
            "exactly one store should have landed"
        );
    }

    /// Empty messages: service still persists the rule-based skeleton
    /// so the session has *something* at the L1 prefix — `find`
    /// queries downstream shouldn't crash on empty content.
    #[tokio::test]
    async fn empty_messages_still_produce_a_store() {
        let memoria = Arc::new(CapturingMemoria::default());
        let (svc, _rx, _broker) =
            build_ctx_with_memoria(None, Arc::clone(&memoria) as Arc<dyn MemoriaClient>);
        let sid = format!("empty-msg-{}", nanos());
        let req = ExtractionRequest {
            session_id: sid.clone(),
            messages: Vec::new(), // ← zero messages
            current_tokens: 50_000,
            current_tool_calls: 0,
            had_error: false,
            turn_number: 0,
            config: SessionMemoryExtractConfig::default(),
        };
        assert_eq!(svc.maybe_spawn(req), SpawnDecision::Spawned);
        svc.wait_for_pending(Duration::from_secs(2)).await;

        let stored = memoria.stored.lock().unwrap().clone();
        assert_eq!(
            stored.len(),
            1,
            "empty-messages session should still land a L1"
        );
        assert!(
            stored[0].0.starts_with("[session-memory:v1]"),
            "L1 must carry the prefix even when input was empty; got: {:?}",
            &stored[0].0[..60.min(stored[0].0.len())]
        );
    }

    /// Huge messages: 10K lines × 200 chars. Worker must not OOM or
    /// exceed reasonable latency. Verifies build_l1 handles bulk input.
    #[tokio::test]
    async fn huge_messages_do_not_oom_worker() {
        let memoria = Arc::new(CapturingMemoria::default());
        let (svc, _rx, _broker) =
            build_ctx_with_memoria(None, Arc::clone(&memoria) as Arc<dyn MemoriaClient>);
        let sid = format!("huge-{}", nanos());

        let mut messages = Vec::with_capacity(10_000);
        let fat_line = "x".repeat(200);
        for i in 0..10_000 {
            messages.push(json!({
                "role": if i % 2 == 0 { "user" } else { "assistant" },
                "content": fat_line.clone(),
            }));
        }
        let req = ExtractionRequest {
            session_id: sid,
            messages,
            current_tokens: 500_000,
            current_tool_calls: 0,
            had_error: false,
            turn_number: 20,
            config: SessionMemoryExtractConfig::default(),
        };

        let started = Instant::now();
        assert_eq!(svc.maybe_spawn(req), SpawnDecision::Spawned);
        let leftover = svc.wait_for_pending(Duration::from_secs(10)).await;
        assert_eq!(leftover, 0, "huge session must still complete within 10s");
        let elapsed = started.elapsed();
        assert!(
            elapsed < Duration::from_secs(10),
            "10K-message extraction should finish promptly; elapsed={elapsed:?}"
        );

        // Verify the stored L1 doesn't itself blow past a sane size
        // (build_l1 should truncate). We don't lock a specific cap
        // here, just that it's far less than input (~2MB).
        let stored = memoria.stored.lock().unwrap().clone();
        assert_eq!(stored.len(), 1);
        assert!(
            stored[0].0.len() < 200_000,
            "L1 must be truncated; got {} chars",
            stored[0].0.len()
        );
    }

    /// Special characters in session_id: spaces, unicode, quote,
    /// slash. Memoria store must see the session_id intact; gate must
    /// not crash on truncation boundaries.
    #[tokio::test]
    async fn special_chars_in_session_id_propagate_to_memoria() {
        let memoria = Arc::new(CapturingMemoria::default());
        let (svc, _rx, _broker) =
            build_ctx_with_memoria(None, Arc::clone(&memoria) as Arc<dyn MemoriaClient>);
        let weird_sids = ["has space", "中文会话", "quote'inside", "path/with/slash"];
        for (i, sid) in weird_sids.iter().enumerate() {
            let req = sample_req(sid, 50_000, false);
            let d = svc.maybe_spawn(req);
            assert_eq!(d, SpawnDecision::Spawned, "sid[{i}]='{sid}' must spawn");
        }
        svc.wait_for_pending(Duration::from_secs(3)).await;

        let stored = memoria.stored.lock().unwrap().clone();
        assert_eq!(stored.len(), weird_sids.len());
        for (i, sid) in weird_sids.iter().enumerate() {
            assert_eq!(
                stored[i].2.as_deref(),
                Some(*sid),
                "session_id must reach Memoria unchanged"
            );
        }
    }

    // ── cross-turn state persistence (regression for turn-scoped state bug) ──

    /// The critical fix: per-session debounce state must persist across
    /// simulated turn boundaries. Previously state lived on
    /// `AgenticLoopState` which got rebuilt every turn, so `initialized`
    /// was always `false` at gate time and the growth-delta branch of
    /// the gate was structurally unreachable. Now state lives in the
    /// service itself, keyed by session_id.
    #[tokio::test]
    async fn debounce_state_persists_across_simulated_turns() {
        let memoria = Arc::new(CapturingMemoria::default());
        let (svc, _rx, _broker) =
            build_ctx_with_memoria(None, Arc::clone(&memoria) as Arc<dyn MemoriaClient>);
        let sid = format!("cross-turn-{}", nanos());

        // Turn 1: past init gate → Run. Worker finishes and
        // `mark_extracted` updates the service-internal state.
        let req1 = ExtractionRequest {
            session_id: sid.clone(),
            messages: vec![json!({"role": "user", "content": "hi"})],
            current_tokens: 20_000,
            current_tool_calls: 2,
            had_error: false,
            turn_number: 1,
            config: SessionMemoryExtractConfig::default(),
        };
        assert_eq!(svc.maybe_spawn(req1), SpawnDecision::Spawned);
        svc.wait_for_pending(Duration::from_secs(2)).await;
        // Confirm state actually recorded.
        let state = svc
            .peek_state(&sid)
            .expect("state should be recorded after first extraction");
        assert!(state.initialized);
        assert_eq!(state.tokens_at_last_extraction, 20_000);
        assert_eq!(state.tool_calls_at_last_extraction, 2);

        // Turn 2: tiny growth. With the OLD turn-scoped state this
        // would have reset `initialized = false` and seen `20_500 >
        // min_tokens_to_init (10K)` → Run. With the new per-session
        // state this should correctly debounce to `no_growth` because
        // growth < 5K between updates.
        let req2 = ExtractionRequest {
            session_id: sid.clone(),
            messages: vec![json!({"role": "user", "content": "hi again"})],
            current_tokens: 20_500,
            current_tool_calls: 3,
            had_error: false,
            turn_number: 2,
            config: SessionMemoryExtractConfig::default(),
        };
        assert_eq!(
            svc.maybe_spawn(req2),
            SpawnDecision::Skipped,
            "tiny growth on the next turn should debounce, not re-spawn"
        );

        // Turn 3: big growth → Run again.
        let req3 = ExtractionRequest {
            session_id: sid.clone(),
            messages: vec![json!({"role": "user", "content": "big turn"})],
            current_tokens: 30_000,
            current_tool_calls: 8,
            had_error: false,
            turn_number: 3,
            config: SessionMemoryExtractConfig::default(),
        };
        assert_eq!(
            svc.maybe_spawn(req3),
            SpawnDecision::Spawned,
            "5K+ growth + 3+ tool call growth must re-admit extraction"
        );
        svc.wait_for_pending(Duration::from_secs(2)).await;

        // Verify the service advanced its state to the turn-3 counters.
        let state = svc.peek_state(&sid).unwrap();
        assert_eq!(state.tokens_at_last_extraction, 30_000);
        assert_eq!(state.tool_calls_at_last_extraction, 8);

        // Two extractions landed in total (turns 1 and 3).
        assert_eq!(memoria.stored.lock().unwrap().len(), 2);
    }

    /// Different sessions must not pollute each other's debounce state.
    #[tokio::test]
    async fn debounce_state_is_per_session() {
        let memoria = Arc::new(CapturingMemoria::default());
        let (svc, _rx, _broker) =
            build_ctx_with_memoria(None, Arc::clone(&memoria) as Arc<dyn MemoriaClient>);
        let sid_a = format!("iso-a-{}", nanos());
        let sid_b = format!("iso-b-{}", nanos());

        // Session A: one extraction.
        let req_a = sample_req(&sid_a, 20_000, false);
        assert_eq!(svc.maybe_spawn(req_a), SpawnDecision::Spawned);
        svc.wait_for_pending(Duration::from_secs(2)).await;

        // Session B on same service: fresh init gate applies.
        let req_b = sample_req(&sid_b, 15_000, false);
        assert_eq!(
            svc.maybe_spawn(req_b),
            SpawnDecision::Spawned,
            "Session B must not inherit Session A's debounce state"
        );
        svc.wait_for_pending(Duration::from_secs(2)).await;

        let a_state = svc.peek_state(&sid_a).unwrap();
        let b_state = svc.peek_state(&sid_b).unwrap();
        assert_eq!(a_state.tokens_at_last_extraction, 20_000);
        assert_eq!(b_state.tokens_at_last_extraction, 15_000);
    }

    /// `forget_session` must clear the entry so a second extraction for
    /// the same session_id after session-end starts fresh.
    #[tokio::test]
    async fn forget_session_clears_debounce_state() {
        let memoria = Arc::new(CapturingMemoria::default());
        let (svc, _rx, _broker) =
            build_ctx_with_memoria(None, Arc::clone(&memoria) as Arc<dyn MemoriaClient>);
        let sid = format!("forget-{}", nanos());

        let req = sample_req(&sid, 20_000, false);
        svc.maybe_spawn(req);
        svc.wait_for_pending(Duration::from_secs(2)).await;
        assert!(svc.peek_state(&sid).is_some());

        svc.forget_session(&sid);
        assert!(
            svc.peek_state(&sid).is_none(),
            "forget_session must remove the entry"
        );

        // A later reuse of the same sid should start fresh (below init
        // gate now applies again if tokens < 10K).
        let req2 = sample_req(&sid, 5_000, false);
        assert_eq!(
            svc.maybe_spawn(req2),
            SpawnDecision::Skipped,
            "after forget, the below-init-gate check should apply again"
        );
    }

    // ── Memoria circuit breaker integration ─────────────────────────

    /// Build a service whose Memoria always fails, with a tight breaker
    /// config so the test doesn't have to wait real seconds.
    fn build_breaker_ctx() -> (
        Arc<MemoryExtractionService>,
        tokio::sync::mpsc::Receiver<IngestionEvent>,
        Arc<BackgroundActivityBroker>,
    ) {
        struct FailingMemoria;
        #[async_trait]
        impl MemoriaClient for FailingMemoria {
            async fn retrieve_ext(
                &self,
                _: &str,
                _: Option<&str>,
                _: usize,
                _: bool,
            ) -> Result<Vec<crate::turn::cloud::memoria_compact::MemoriaMemory>, String>
            {
                Ok(Vec::new())
            }
            async fn store(
                &self,
                _: &str,
                _: &str,
                _: Option<&str>,
                _: Option<&str>,
            ) -> Result<String, String> {
                Err("memoria down".into())
            }
            async fn purge_working(&self, _: &str) -> Result<u64, String> {
                Ok(0)
            }
        }
        let (ingestion, rx) = IngestionSender::for_tests(256);
        let broker = Arc::new(BackgroundActivityBroker::new());
        // Override: low threshold (2 failures) + short cooldown so the
        // test can exercise the Open → HalfOpen transition.
        let mut svc = MemoryExtractionService::new(
            Arc::new(ConstSelectorResolver(None)),
            Arc::new(FailingMemoria) as Arc<dyn MemoriaClient>,
            ingestion,
            "breaker-test",
            Arc::clone(&broker),
        );
        svc.memoria_health = Arc::new(crate::session_memory::health::MemoriaHealth::with_config(
            2,
            Duration::from_millis(100),
        ));
        (Arc::new(svc), rx, broker)
    }

    #[tokio::test]
    async fn breaker_opens_after_threshold_failures_and_skips_spawn() {
        let (svc, mut rx, _broker) = build_breaker_ctx();

        // Two failing attempts trip the breaker.
        for i in 0..2 {
            let sid = format!("fail-{i}-{}", nanos());
            let req = sample_req(&sid, 20_000, false);
            assert_eq!(svc.maybe_spawn(req), SpawnDecision::Spawned);
            svc.wait_for_pending(Duration::from_secs(2)).await;
        }

        // Third attempt: breaker is Open → skipped synchronously, no
        // new HTTP attempt, no spawn.
        let sid = format!("tripped-{}", nanos());
        let req = sample_req(&sid, 20_000, false);
        assert_eq!(
            svc.maybe_spawn(req),
            SpawnDecision::Skipped,
            "breaker must fail fast on third attempt"
        );

        // An event must have been emitted with reason=memoria_unhealthy.
        let mut saw_unhealthy = false;
        while let Ok(evt) = rx.try_recv() {
            if evt.event_type != "session_memory_extraction" {
                continue;
            }
            let m = evt.metadata.as_ref().unwrap();
            if m["outcome"] == "skipped" && m["reason"] == "memoria_unhealthy" {
                saw_unhealthy = true;
            }
        }
        assert!(
            saw_unhealthy,
            "expected skipped{{memoria_unhealthy}} event once breaker tripped"
        );
    }

    #[tokio::test]
    async fn breaker_recovers_after_cooldown_when_probe_succeeds() {
        // Start with failing Memoria to trip the breaker, then swap to
        // a succeeding one via a shared flag.
        use std::sync::atomic::{AtomicBool, Ordering};
        struct FlakeyMemoria {
            alive: Arc<AtomicBool>,
            stored: Mutex<u32>,
        }
        #[async_trait]
        impl MemoriaClient for FlakeyMemoria {
            async fn retrieve_ext(
                &self,
                _: &str,
                _: Option<&str>,
                _: usize,
                _: bool,
            ) -> Result<Vec<crate::turn::cloud::memoria_compact::MemoriaMemory>, String>
            {
                Ok(Vec::new())
            }
            async fn store(
                &self,
                _: &str,
                _: &str,
                _: Option<&str>,
                _: Option<&str>,
            ) -> Result<String, String> {
                if self.alive.load(Ordering::Acquire) {
                    *self.stored.lock().unwrap() += 1;
                    Ok("ok".into())
                } else {
                    Err("memoria down".into())
                }
            }
            async fn purge_working(&self, _: &str) -> Result<u64, String> {
                Ok(0)
            }
        }
        let alive = Arc::new(AtomicBool::new(false));
        let memoria = Arc::new(FlakeyMemoria {
            alive: Arc::clone(&alive),
            stored: Mutex::new(0),
        });
        let (ingestion, mut rx) = IngestionSender::for_tests(256);
        let broker = Arc::new(BackgroundActivityBroker::new());
        let mut svc = MemoryExtractionService::new(
            Arc::new(ConstSelectorResolver(None)),
            Arc::clone(&memoria) as Arc<dyn MemoriaClient>,
            ingestion,
            "recovery-test",
            broker,
        );
        svc.memoria_health = Arc::new(crate::session_memory::health::MemoriaHealth::with_config(
            1,
            Duration::from_millis(80),
        ));
        let svc = Arc::new(svc);

        // Trip the breaker with one failure.
        svc.maybe_spawn(sample_req(&format!("fail-{}", nanos()), 20_000, false));
        svc.wait_for_pending(Duration::from_secs(2)).await;
        // Next attempt → breaker is Open.
        assert_eq!(
            svc.maybe_spawn(sample_req(&format!("blocked-{}", nanos()), 20_000, false)),
            SpawnDecision::Skipped
        );

        // Flip the Memoria to healthy + wait past the cooldown.
        alive.store(true, Ordering::Release);
        tokio::time::sleep(Duration::from_millis(100)).await;

        // Next attempt is the probe — should be admitted.
        let probe_sid = format!("probe-{}", nanos());
        assert_eq!(
            svc.maybe_spawn(sample_req(&probe_sid, 20_000, false)),
            SpawnDecision::Spawned
        );
        svc.wait_for_pending(Duration::from_secs(2)).await;

        // And the next one after probe success is back to Closed.
        let post_sid = format!("post-{}", nanos());
        assert_eq!(
            svc.maybe_spawn(sample_req(&post_sid, 20_000, false)),
            SpawnDecision::Spawned
        );
        svc.wait_for_pending(Duration::from_secs(2)).await;

        assert_eq!(
            *memoria.stored.lock().unwrap(),
            2,
            "probe + post-probe should both have written"
        );

        // Drain events so the channel isn't reported as leaking.
        while rx.try_recv().is_ok() {}
    }

    // ── Breadcrumb fields in emitted events ─────────────────────────

    #[tokio::test]
    async fn extracted_event_carries_messages_count_and_attempt() {
        let memoria = Arc::new(CapturingMemoria::default());
        let (svc, mut rx, _broker) =
            build_ctx_with_memoria(None, Arc::clone(&memoria) as Arc<dyn MemoriaClient>);
        let sid = format!("bc-ok-{}", nanos());
        let req = ExtractionRequest {
            session_id: sid.clone(),
            messages: vec![
                json!({"role": "user", "content": "one"}),
                json!({"role": "assistant", "content": "two"}),
                json!({"role": "user", "content": "three"}),
            ],
            current_tokens: 50_000,
            current_tool_calls: 3,
            had_error: false,
            turn_number: 5,
            config: SessionMemoryExtractConfig::default(),
        };
        assert_eq!(svc.maybe_spawn(req), SpawnDecision::Spawned);
        svc.wait_for_pending(Duration::from_secs(2)).await;

        let events = collect_extraction_events(&mut rx);
        let extracted = events
            .iter()
            .find(|e| {
                e.metadata
                    .as_ref()
                    .and_then(|m| m.get("outcome"))
                    .and_then(|v| v.as_str())
                    == Some("extracted")
            })
            .expect("expected an extracted event");
        let meta = extracted.metadata.as_ref().unwrap();
        // Breadcrumbs must have landed in the metadata JSON.
        assert_eq!(meta["messages_count"], 3);
        // Rule-based path → no selector_model (None is omitted).
        assert!(
            meta.get("selector_model").is_none(),
            "rule-based extraction must not emit selector_model; got: {meta:?}"
        );
        assert_eq!(
            meta["attempt"], 1,
            "first-try store success must report attempt=1"
        );
    }

    #[tokio::test]
    async fn skip_event_carries_messages_count_but_no_attempt() {
        let memoria = Arc::new(CapturingMemoria::default());
        let (svc, mut rx, _broker) =
            build_ctx_with_memoria(None, Arc::clone(&memoria) as Arc<dyn MemoriaClient>);
        let sid = format!("bc-skip-{}", nanos());
        // Below init gate → Skipped{BelowInitGate}.
        let req = ExtractionRequest {
            session_id: sid,
            messages: vec![json!({"role": "user", "content": "x"})],
            current_tokens: 1_000,
            current_tool_calls: 0,
            had_error: false,
            turn_number: 1,
            config: SessionMemoryExtractConfig::default(),
        };
        assert_eq!(svc.maybe_spawn(req), SpawnDecision::Skipped);

        let events = collect_extraction_events(&mut rx);
        let skip = &events[0];
        let meta = skip.metadata.as_ref().unwrap();
        assert_eq!(meta["outcome"], "skipped");
        assert_eq!(meta["messages_count"], 1);
        assert!(meta.get("attempt").is_none(), "no persist attempt happened");
        assert!(
            meta.get("selector_model").is_none(),
            "no selector resolved yet"
        );
    }
}
