//! [`MemoryExtractionService`] — the single entry point for background
//! session-memory extraction.
//!
//! Produces one unified artifact per turn: an L1 markdown document
//! persisted to Memoria under the [`SESSION_MEMORY_PREFIX`] convention,
//! keyed on `session_id`. CLI edge-cloud mode can additionally require a
//! local `session-memory.md` refresh before the extraction is considered
//! successful for current-session UX.
//!
//! Read-side consumers still share one schema. The durable store remains
//! Memoria; the optional local artifact is only the CLI edge mode's
//! current-session read model.
//!
//! Ownership model:
//!
//! * [`MemoryInferenceResolver`] + [`Arc<dyn MemoriaPort>`] are the
//!   only production dependencies. Both are injected at construction.
//!   Tests swap in [`ConstMemoryInferenceResolver`] and a minimal capturing
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
use serde_json::Value;

use astra_services::event_ingestion::{IngestionEvent, IngestionSender};
use astra_services::session_journal::{
    JournalEvent, SessionMemoryExtractionBreadcrumbs, SessionMemoryExtractionErrorReason,
    SessionMemoryExtractionOutcome, SessionMemoryExtractionSkipReason,
    SessionMemoryExtractionSource,
};
use astra_turn_core::cloud_session_memory_extract::SessionMemoryState;
use astra_turn_types::is_runtime_owned_message;

use crate::memory_hooks::MemoryInferenceClient;
use crate::turn::cloud::memoria_compact::MemoriaPort;

use super::activity::{BackgroundActivity, BackgroundActivityBroker};
use super::gate::{GateDecision, evaluate};
use super::health::{MemoriaAdmit, MemoriaHealth, SelectorHealth};
use super::request::{ExtractionRequest, SpawnDecision};
use super::runner::{
    ExtractionArtifacts, persist_local_session_memory_artifact,
    persist_local_session_memory_metadata, run_extraction,
};

type LocalJournalEventSink = dyn Fn(&JournalEvent) + Send + Sync + 'static;

#[derive(Default)]
struct SessionWorkCoordinator {
    active_fingerprints: std::collections::HashMap<String, u64>,
    queued_latest: std::collections::HashMap<String, (ExtractionRequest, u64)>,
}

/// Hard upper bound on one LLM call. Memory extraction is background
/// work; a hung call must never linger past this.
pub const LLM_TIMEOUT: Duration = Duration::from_secs(30);

/// Output token budget for the sparse JSON update. The canonical narrative is
/// intentionally small; a larger budget would only invite history-log output.
pub const EXTRACTION_MAX_OUTPUT_TOKENS: usize = 2048;

// ───────────────────────────────────────────────────────────────────────
// Memory-inference resolution (async trait so tests can swap in a const)
// ───────────────────────────────────────────────────────────────────────

/// Resolve the cheap selector-tagged inference clients used by the extractor.
/// Called once per extraction attempt.
#[async_trait]
pub trait MemoryInferenceResolver: Send + Sync + std::fmt::Debug {
    async fn resolve_candidates(&self, user_id: &str) -> Vec<MemoryInferenceClient>;
}

/// Always returns the same client. Unit tests.
#[derive(Debug)]
pub struct ConstMemoryInferenceResolver(pub Option<MemoryInferenceClient>);

#[async_trait]
impl MemoryInferenceResolver for ConstMemoryInferenceResolver {
    async fn resolve_candidates(&self, _user_id: &str) -> Vec<MemoryInferenceClient> {
        self.0.iter().cloned().collect()
    }
}

// ───────────────────────────────────────────────────────────────────────
// The service
// ───────────────────────────────────────────────────────────────────────

/// Per-process background extraction coordinator. Build once at
/// server/CLI boot, hold an [`Arc`] on
/// [`crate::turn::agentic_loop::host::AgenticLoopState`].
pub struct MemoryExtractionService {
    inference_resolver: Arc<dyn MemoryInferenceResolver>,
    memoria_client: Arc<dyn MemoriaPort>,
    ingestion: IngestionSender,
    /// Authenticated owner for an executable service. The process-wide
    /// template is deliberately unbound and can only create owner-scoped
    /// services through [`Self::scoped_to_owner`].
    user_id: Option<Arc<str>>,
    health: Arc<SelectorHealth>,
    memoria_health: Arc<MemoriaHealth>,
    /// Per-session active fingerprint plus one latest-wins queued refresh.
    /// A std mutex is appropriate because admission and handoff are short,
    /// synchronous map updates with no `.await` inside.
    work: Arc<std::sync::Mutex<SessionWorkCoordinator>>,
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
    /// stored this on `AgenticLoopState` and lost the last semantic
    /// fingerprint every turn, defeating debounce. Entries are removed on
    /// [`Self::forget_session`] (session end).
    session_states: Arc<std::sync::Mutex<std::collections::HashMap<String, SessionMemoryState>>>,
    /// Request-owner scoped service handles. Each owner gets isolated
    /// debounce/in-flight maps while endpoint health, shutdown accounting,
    /// ingestion, and the activity broker remain shared.
    owner_scoped_services: Arc<
        std::sync::Mutex<
            std::collections::HashMap<String, std::sync::Weak<MemoryExtractionService>>,
        >,
    >,
    local_event_sink: Option<Arc<LocalJournalEventSink>>,
    require_local_current_snapshot: bool,
}

impl std::fmt::Debug for MemoryExtractionService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MemoryExtractionService")
            .field("user_id", &self.user_id.as_deref())
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
        inference_resolver: Arc<dyn MemoryInferenceResolver>,
        memoria_client: Arc<dyn MemoriaPort>,
        ingestion: IngestionSender,
        user_id: impl Into<Arc<str>>,
        broker: Arc<BackgroundActivityBroker>,
    ) -> Self {
        Self::new_with_owner(
            inference_resolver,
            memoria_client,
            ingestion,
            Some(user_id.into()),
            broker,
        )
    }

    /// Build an owner-neutral process template. It cannot execute extraction
    /// itself; callers must bind it to an authenticated owner first.
    pub(crate) fn new_owner_scoped_template(
        inference_resolver: Arc<dyn MemoryInferenceResolver>,
        memoria_client: Arc<dyn MemoriaPort>,
        ingestion: IngestionSender,
        broker: Arc<BackgroundActivityBroker>,
    ) -> Self {
        Self::new_with_owner(inference_resolver, memoria_client, ingestion, None, broker)
    }

    fn new_with_owner(
        inference_resolver: Arc<dyn MemoryInferenceResolver>,
        memoria_client: Arc<dyn MemoriaPort>,
        ingestion: IngestionSender,
        user_id: Option<Arc<str>>,
        broker: Arc<BackgroundActivityBroker>,
    ) -> Self {
        Self {
            inference_resolver,
            memoria_client,
            ingestion,
            user_id,
            health: Arc::new(SelectorHealth::new()),
            memoria_health: Arc::new(MemoriaHealth::new()),
            work: Arc::new(std::sync::Mutex::new(SessionWorkCoordinator::default())),
            broker,
            pending: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            pending_done: Arc::new(tokio::sync::Notify::new()),
            session_states: Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
            owner_scoped_services: Arc::new(
                std::sync::Mutex::new(std::collections::HashMap::new()),
            ),
            local_event_sink: None,
            require_local_current_snapshot: false,
        }
    }

    /// Mirror emitted journal events into a caller-owned local sink.
    ///
    /// CLI uses this to append session-memory extraction events to the
    /// local `~/.astra/sessions/*.jsonl` journal even though cloud
    /// ingestion remains server-owned.
    pub fn with_local_event_sink(mut self, sink: Arc<LocalJournalEventSink>) -> Self {
        self.local_event_sink = Some(sink);
        self
    }

    /// Require a successful local `session-memory.md` refresh before the
    /// extraction counts as success. CLI edge-cloud mode enables this so
    /// `/memory session` reflects the same current-session artifact the
    /// extractor just wrote; web/cloud runtimes leave it off because their
    /// current-session read model is server-side.
    pub fn with_local_current_snapshot(mut self) -> Self {
        self.require_local_current_snapshot = true;
        self
    }

    /// Bind a multi-tenant service to the authenticated request owner.
    ///
    /// Repeated requests for the same owner share process-local admission and
    /// debounce state; different owners cannot collide even if a client
    /// supplies the same session id. Unsupported transport rebinding fails
    /// closed, disabling extraction for that request rather than writing under
    /// a bootstrap/default tenant.
    pub fn scoped_to_owner(self: &Arc<Self>, user_id: &str) -> Result<Arc<Self>, String> {
        let scope = astra_memoria::MemoryScope::new(user_id, "owner-binding-validation")?;
        if self.user_id.as_deref() == Some(scope.user_id.as_str()) {
            return Ok(Arc::clone(self));
        }

        let mut scoped = self
            .owner_scoped_services
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        scoped.retain(|_, service| service.strong_count() > 0);
        if let Some(existing) = scoped
            .get(&scope.user_id)
            .and_then(std::sync::Weak::upgrade)
        {
            return Ok(existing);
        }

        let service = Arc::new(Self {
            inference_resolver: Arc::clone(&self.inference_resolver),
            memoria_client: self.memoria_client.bind_owner(&scope.user_id)?,
            ingestion: self.ingestion.clone(),
            user_id: Some(Arc::from(scope.user_id.as_str())),
            health: Arc::clone(&self.health),
            memoria_health: Arc::clone(&self.memoria_health),
            work: Arc::new(std::sync::Mutex::new(SessionWorkCoordinator::default())),
            broker: Arc::clone(&self.broker),
            pending: Arc::clone(&self.pending),
            pending_done: Arc::clone(&self.pending_done),
            session_states: Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
            owner_scoped_services: Arc::clone(&self.owner_scoped_services),
            local_event_sink: self.local_event_sink.clone(),
            require_local_current_snapshot: self.require_local_current_snapshot,
        });
        scoped.insert(scope.user_id, Arc::downgrade(&service));
        Ok(service)
    }

    #[must_use]
    pub fn owner_user_id(&self) -> Option<&str> {
        self.user_id.as_deref()
    }

    fn write_required_local_snapshot(&self, session_id: &str, content: &str) -> Result<(), String> {
        if !self.require_local_current_snapshot {
            return Ok(());
        }
        persist_local_session_memory_artifact(session_id, content)
    }

    fn persist_success_metadata(
        &self,
        session_id: &str,
        turn: u32,
        source: SessionMemoryExtractionSource,
        selector_model: Option<&str>,
    ) {
        if !self.require_local_current_snapshot {
            return;
        }
        let mut metadata =
            super::runner::load_local_session_memory_metadata(session_id).unwrap_or_default();
        metadata.session_id = session_id.to_string();
        metadata.current_snapshot_source = Some("background_extraction".to_string());
        metadata.last_extracted_turn = Some(turn);
        metadata.last_extraction_source = Some(
            match source {
                SessionMemoryExtractionSource::Llm => "llm",
                SessionMemoryExtractionSource::RuleFallback => "rule_fallback",
            }
            .to_string(),
        );
        metadata.last_remote_sync_status = Some("memoria_synced".to_string());
        metadata.last_remote_sync_at = Some(chrono::Utc::now().to_rfc3339());
        metadata.last_remote_sync_detail = None;
        metadata.last_selector_model = selector_model.map(str::to_string);
        if let Err(error) = persist_local_session_memory_metadata(session_id, &metadata) {
            tracing::warn!(
                session_id = %session_id,
                error = %error,
                "failed to persist local session-memory metadata after successful extraction"
            );
        }
    }

    /// Number of background workers that have been spawned but not
    /// finished yet.
    pub fn pending_drain(&self) -> usize {
        self.pending.load(std::sync::atomic::Ordering::Acquire)
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

    /// Consolidate the final canonical session snapshot through the same
    /// provider instance used for extraction and prompt recall. This keeps
    /// CLI+Server, Edge+Server, and Server Only on one ownership boundary;
    /// lifecycle code must not silently construct a second env-only client.
    pub async fn run_session_end_governance(
        &self,
        facts: &astra_turn_types::session_facts::SessionFacts,
        session_id: &str,
    ) -> Result<crate::turn::cloud::session_end_governance::SessionEndReport, String> {
        crate::turn::cloud::session_end_governance::run_session_end_governance(
            facts,
            session_id,
            self.memoria_client.as_ref(),
        )
        .await
    }

    pub fn broker(&self) -> Arc<BackgroundActivityBroker> {
        Arc::clone(&self.broker)
    }

    /// Best-effort final flush for short or lightly-active sessions.
    ///
    /// Normal turn-end extraction is still the primary path. This helper
    /// exists for session shutdown: if a session never crossed the normal
    /// init/growth gates but still accumulated meaningful state, enqueue one
    /// last extraction before callers block in [`Self::wait_for_pending`].
    ///
    /// Message text is not classified here. The canonical fingerprint decides
    /// whether the latest snapshot is already fresh; semantic interpretation
    /// belongs to the configured extraction provider.
    pub fn maybe_spawn_shutdown_flush(self: &Arc<Self>, req: ExtractionRequest) -> SpawnDecision {
        // Shutdown does not invent a second freshness policy. The same
        // semantic fingerprint, low-information gate, health cooldown, and
        // in-flight coordination used at turn end remain authoritative.
        self.maybe_spawn(req)
    }

    /// Synchronous entry point. Evaluates the gate against the service's
    /// own per-session debounce state, emits a skip event inline when
    /// rejected, advances the debounce state and spawns the async worker
    /// when admitted.
    ///
    /// **Must run inside a Tokio runtime.**
    pub fn maybe_spawn(self: &Arc<Self>, req: ExtractionRequest) -> SpawnDecision {
        let Some(user_id) = self.user_id.as_deref() else {
            tracing::error!(
                session_id = %req.session_id(),
                "refusing session-memory extraction from an owner-neutral service template"
            );
            return SpawnDecision::Skipped;
        };
        let content_fingerprint = extraction_input_fingerprint(&req);
        // Breadcrumbs for sync-path skip events. `selector_model` and
        // `attempt` only make sense in the async worker after LLM
        // resolve / persist attempt.
        let skip_breadcrumbs = SessionMemoryExtractionBreadcrumbs {
            messages_count: Some(req.messages.len() as u32),
            selector_model: None,
            attempt: None,
            llm_reason: None,
            llm_detail: None,
            persist_detail: None,
        };

        enum Admission {
            Spawn,
            Queue,
            Skip(SessionMemoryExtractionSkipReason),
        }

        // Keep gate evaluation, external admission checks, and debounce
        // advancement in one critical section. Otherwise two callers can
        // both evaluate a stale pre-extraction state and the second can
        // spawn after the first worker has already completed.
        let admission = {
            let map = match self.session_states.lock() {
                Ok(m) => m,
                Err(p) => p.into_inner(),
            };
            let state_ref = map.get(req.session_id());
            let default_state;
            let state = match state_ref {
                Some(s) => s,
                None => {
                    default_state = SessionMemoryState::default();
                    &default_state
                }
            };
            let dec = evaluate(state, req.session_id(), content_fingerprint);

            if let GateDecision::Skip(reason) = dec {
                Admission::Skip(reason)
            } else {
                match self.memoria_health.admit() {
                    MemoriaAdmit::CoolingDown => {
                        Admission::Skip(SessionMemoryExtractionSkipReason::MemoriaUnhealthy)
                    }
                    MemoriaAdmit::Ready => {
                        let mut work = self
                            .work
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner);
                        match work.active_fingerprints.get(req.session_id()).copied() {
                            None => {
                                work.active_fingerprints
                                    .insert(req.session_id().to_string(), content_fingerprint);
                                Admission::Spawn
                            }
                            Some(active_fingerprint)
                                if active_fingerprint == content_fingerprint
                                    || work.queued_latest.get(req.session_id()).is_some_and(
                                        |(_, queued_fingerprint)| {
                                            *queued_fingerprint == content_fingerprint
                                        },
                                    ) =>
                            {
                                Admission::Skip(SessionMemoryExtractionSkipReason::InFlight)
                            }
                            Some(_) => {
                                work.queued_latest.insert(
                                    req.session_id().to_string(),
                                    (req.clone(), content_fingerprint),
                                );
                                Admission::Queue
                            }
                        }
                    }
                }
            }
        };

        match admission {
            Admission::Spawn => {}
            Admission::Queue => return SpawnDecision::Queued,
            Admission::Skip(reason) => {
                let sid_opt = if req.session_id().is_empty() {
                    None
                } else {
                    Some(req.session_id())
                };
                self.emit_skip_event(
                    user_id,
                    sid_opt,
                    req.turn_number(),
                    reason,
                    &skip_breadcrumbs,
                );
                return SpawnDecision::Skipped;
            }
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
        let pending_guard = PendingGuard::new(pending, pending_done);
        let session_id = req.session_id().to_string();
        let work_guard = SessionWorkGuard::new(Arc::clone(&self.work), session_id.clone());
        tokio::spawn(async move {
            let _pending_guard = pending_guard;
            let _work_guard = work_guard;
            let mut next = Some((req, content_fingerprint));
            while let Some((request, fingerprint)) = next {
                Arc::clone(&svc).run_one(request, fingerprint).await;
                next = svc.take_queued_or_release(&session_id);
            }
        });
        SpawnDecision::Spawned
    }

    // ── internals ─────────────────────────────────────────────────────

    async fn run_one(self: Arc<Self>, req: ExtractionRequest, content_fingerprint: u64) {
        let session_id = req.session_id().to_string();
        let Some(user_id) = self.user_id.as_deref().map(str::to_string) else {
            tracing::error!(
                session_id = %session_id,
                "refusing session-memory worker execution without an authenticated owner"
            );
            return;
        };
        let turn = req.turn_number();
        let messages_count = req.messages.len() as u32;
        let started = Instant::now();
        // Process-local admission cannot see a worker that completed in a
        // previous CLI process or server pod. Read the durable snapshot before
        // spending selector tokens. A normal turn is idempotent once that
        // snapshot covers it; explicit correction/undo requests may replace a
        // same-turn snapshot with corrected state.
        let current = self.load_current_memory_with_freshness(&session_id).await;
        if current
            .as_ref()
            .and_then(|loaded| loaded.updated_turn)
            .is_some_and(|updated_turn| updated_turn >= turn)
            && !req.reanchors_current_objective
        {
            self.mark_session_extracted(&session_id, content_fingerprint, turn);
            let breadcrumbs = SessionMemoryExtractionBreadcrumbs {
                messages_count: Some(messages_count),
                selector_model: None,
                attempt: None,
                llm_reason: None,
                llm_detail: None,
                persist_detail: None,
            };
            self.emit_skip_event(
                &user_id,
                Some(&session_id),
                turn,
                SessionMemoryExtractionSkipReason::AlreadyCurrent,
                &breadcrumbs,
            );
            return;
        }
        let current_memory = current.map(|loaded| loaded.content).unwrap_or_default();
        let selector_candidates = self.inference_resolver.resolve_candidates(&user_id).await;
        let resolved_selector_model = selector_candidates
            .first()
            .map(|candidate| candidate.model_name().to_string());
        let effective_selectors: Vec<MemoryInferenceClient> = selector_candidates
            .into_iter()
            .filter(|candidate| self.health.is_healthy(candidate.model_name()))
            .collect();
        if resolved_selector_model.is_some() && effective_selectors.is_empty() {
            let cooldown_breadcrumbs = SessionMemoryExtractionBreadcrumbs {
                messages_count: Some(messages_count),
                selector_model: resolved_selector_model.clone(),
                attempt: None,
                llm_reason: None,
                llm_detail: None,
                persist_detail: None,
            };
            self.emit_skip_event(
                &user_id,
                Some(&session_id),
                turn,
                SessionMemoryExtractionSkipReason::SelectorCooldown,
                &cooldown_breadcrumbs,
            );
        }
        if !effective_selectors.is_empty() {
            self.broker.emit(BackgroundActivity::Started {
                session_id: session_id.clone(),
                turn,
            });
        }

        let artifacts = run_extraction(
            &self.memoria_client,
            &req.inference_scope,
            &req.messages,
            turn as usize,
            &current_memory,
            &req.session_facts,
            &effective_selectors,
            LLM_TIMEOUT,
            EXTRACTION_MAX_OUTPUT_TOKENS,
        )
        .await;
        let duration_ms = started.elapsed().as_millis() as u64;

        match artifacts {
            ExtractionArtifacts::Persisted {
                source,
                bytes_written,
                store_attempt,
                content,
                selector_model,
                failed_candidates,
            } => {
                self.record_selector_failures(&failed_candidates);
                // Memoria accepted a write → breaker closes (or stays
                // closed) and the consecutive-failure counter resets.
                self.memoria_health.record_success();
                // LLM source proved this selector model is reachable; lift
                // any recent failure cooldown after a proven successful call.
                if matches!(source, SessionMemoryExtractionSource::Llm)
                    && let Some(name) = selector_model.as_deref()
                {
                    self.health.clear(name);
                }
                let selector_model_for_event = match source {
                    SessionMemoryExtractionSource::Llm => selector_model.clone(),
                    SessionMemoryExtractionSource::RuleFallback => selector_model
                        .clone()
                        .or_else(|| resolved_selector_model.clone()),
                };
                if let Err(error) = self.write_required_local_snapshot(&session_id, &content) {
                    tracing::warn!(
                        session_id = %session_id,
                        error = %error,
                        "session_memory persisted remotely but could not refresh the required local current-session snapshot"
                    );
                    let bc = SessionMemoryExtractionBreadcrumbs {
                        messages_count: Some(messages_count),
                        selector_model: selector_model_for_event.clone(),
                        attempt: Some(store_attempt),
                        llm_reason: None,
                        llm_detail: None,
                        persist_detail: Some(error.clone()),
                    };
                    self.emit_error_event(
                        &user_id,
                        Some(&session_id),
                        turn,
                        SessionMemoryExtractionErrorReason::WriteFailed,
                        duration_ms,
                        &bc,
                    );
                    self.broker.emit(BackgroundActivity::Errored {
                        session_id: session_id.clone(),
                        turn,
                        reason: SessionMemoryExtractionErrorReason::WriteFailed,
                        detail: Some(error),
                        duration_ms,
                    });
                    return;
                }
                self.mark_session_extracted(&session_id, content_fingerprint, req.turn_number());
                self.broker.emit(BackgroundActivity::Finished {
                    session_id: session_id.clone(),
                    turn,
                    source,
                    duration_ms,
                });
                let bc = SessionMemoryExtractionBreadcrumbs {
                    messages_count: Some(messages_count),
                    selector_model: selector_model_for_event.clone(),
                    attempt: Some(store_attempt),
                    llm_reason: None,
                    llm_detail: None,
                    persist_detail: None,
                };
                self.emit_success_event(
                    &user_id,
                    Some(&session_id),
                    turn,
                    source,
                    bytes_written,
                    duration_ms,
                    &bc,
                );
                self.persist_success_metadata(
                    &session_id,
                    turn,
                    source,
                    selector_model_for_event.as_deref(),
                );
            }
            ExtractionArtifacts::LlmFailedPersistedFallback {
                error_reason,
                error_detail,
                bytes_written,
                store_attempt,
                content,
                selector_model,
                failed_candidates,
            } => {
                self.record_selector_failures(&failed_candidates);
                // Memoria persist still succeeded on this branch, so
                // the circuit breaker resets. Only the LLM selector
                // model is marked unhealthy.
                self.memoria_health.record_success();
                if let Err(error) = self.write_required_local_snapshot(&session_id, &content) {
                    tracing::warn!(
                        session_id = %session_id,
                        error = %error,
                        "session_memory fallback persisted remotely but could not refresh the required local current-session snapshot"
                    );
                    let bc = SessionMemoryExtractionBreadcrumbs {
                        messages_count: Some(messages_count),
                        selector_model: selector_model.clone(),
                        attempt: Some(store_attempt),
                        llm_reason: Some(error_reason),
                        llm_detail: error_detail.clone(),
                        persist_detail: Some(error.clone()),
                    };
                    self.emit_error_event(
                        &user_id,
                        Some(&session_id),
                        turn,
                        SessionMemoryExtractionErrorReason::WriteFailed,
                        duration_ms,
                        &bc,
                    );
                    self.broker.emit(BackgroundActivity::Errored {
                        session_id: session_id.clone(),
                        turn,
                        reason: SessionMemoryExtractionErrorReason::WriteFailed,
                        detail: Some(error),
                        duration_ms,
                    });
                    return;
                }
                self.mark_session_extracted(&session_id, content_fingerprint, req.turn_number());
                // LLM failed but rule-based content did land. Surface
                // the error live, but record the journal outcome as a
                // successful fallback write so postmortems stop reading
                // this branch as "nothing was persisted".
                let bc = SessionMemoryExtractionBreadcrumbs {
                    messages_count: Some(messages_count),
                    selector_model: selector_model.clone(),
                    attempt: Some(store_attempt),
                    llm_reason: Some(error_reason),
                    llm_detail: error_detail.clone(),
                    persist_detail: None,
                };
                self.emit_success_event(
                    &user_id,
                    Some(&session_id),
                    turn,
                    SessionMemoryExtractionSource::RuleFallback,
                    bytes_written,
                    duration_ms,
                    &bc,
                );
                self.persist_success_metadata(
                    &session_id,
                    turn,
                    SessionMemoryExtractionSource::RuleFallback,
                    selector_model.as_deref(),
                );
                self.broker.emit(BackgroundActivity::Errored {
                    session_id: session_id.clone(),
                    turn,
                    reason: error_reason,
                    detail: error_detail.clone(),
                    duration_ms,
                });
                self.broker.emit(BackgroundActivity::Finished {
                    session_id: session_id.clone(),
                    turn,
                    source: SessionMemoryExtractionSource::RuleFallback,
                    duration_ms,
                });
            }
            ExtractionArtifacts::PersistFailed {
                error_reason,
                persist_error_detail,
                llm_error_reason,
                llm_error_detail,
                selector_model,
                failed_candidates,
            } => {
                self.record_selector_failures(&failed_candidates);
                // Memoria persist failed → breaker counts it. Enough
                // consecutive failures trip the breaker and skip
                // future `maybe_spawn` until the cooldown elapses.
                self.memoria_health.record_failure();
                let bc = SessionMemoryExtractionBreadcrumbs {
                    messages_count: Some(messages_count),
                    selector_model: selector_model.clone(),
                    // `attempt` is unavailable on PersistFailed since
                    // run_extraction doesn't surface partial-attempt
                    // counts when nothing landed; use None so the
                    // field is omitted rather than misleadingly 0.
                    attempt: None,
                    llm_reason: llm_error_reason,
                    llm_detail: llm_error_detail.clone(),
                    persist_detail: persist_error_detail.clone(),
                };
                self.emit_error_event(
                    &user_id,
                    Some(&session_id),
                    turn,
                    error_reason,
                    duration_ms,
                    &bc,
                );
                self.broker.emit(BackgroundActivity::Errored {
                    session_id: session_id.clone(),
                    turn,
                    reason: error_reason,
                    detail: persist_error_detail
                        .clone()
                        .or_else(|| llm_error_detail.clone()),
                    duration_ms,
                });
            }
        }
    }

    fn take_queued_or_release(&self, session_id: &str) -> Option<(ExtractionRequest, u64)> {
        let mut work = self
            .work
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some((request, fingerprint)) = work.queued_latest.remove(session_id) {
            work.active_fingerprints
                .insert(session_id.to_string(), fingerprint);
            Some((request, fingerprint))
        } else {
            work.active_fingerprints.remove(session_id);
            None
        }
    }
}

struct PendingGuard {
    pending: Arc<std::sync::atomic::AtomicUsize>,
    pending_done: Arc<tokio::sync::Notify>,
}

impl PendingGuard {
    fn new(
        pending: Arc<std::sync::atomic::AtomicUsize>,
        pending_done: Arc<tokio::sync::Notify>,
    ) -> Self {
        Self {
            pending,
            pending_done,
        }
    }
}

impl Drop for PendingGuard {
    fn drop(&mut self) {
        use std::sync::atomic::Ordering;
        if self.pending.fetch_sub(1, Ordering::AcqRel) == 1 {
            self.pending_done.notify_waiters();
        }
    }
}

/// Panic-safe release of per-session work ownership.
struct SessionWorkGuard {
    work: Arc<std::sync::Mutex<SessionWorkCoordinator>>,
    session_id: String,
}

impl SessionWorkGuard {
    fn new(work: Arc<std::sync::Mutex<SessionWorkCoordinator>>, session_id: String) -> Self {
        Self { work, session_id }
    }
}

impl Drop for SessionWorkGuard {
    fn drop(&mut self) {
        let mut work = self
            .work
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        work.active_fingerprints.remove(&self.session_id);
        work.queued_latest.remove(&self.session_id);
    }
}

fn extraction_input_fingerprint(req: &ExtractionRequest) -> u64 {
    use std::hash::{Hash, Hasher};

    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    req.had_error.hash(&mut hasher);
    req.reanchors_current_objective.hash(&mut hasher);

    for file in &req.session_facts.active_files {
        file.path.hash(&mut hasher);
        file.last_action.hash(&mut hasher);
        file.turn.hash(&mut hasher);
    }
    for tool in &req.session_facts.recent_tool_calls {
        tool.name.hash(&mut hasher);
        tool.ok.hash(&mut hasher);
        tool.turn.hash(&mut hasher);
    }
    req.session_facts.blocked_tools.hash(&mut hasher);
    req.session_facts.error_state.total_errors.hash(&mut hasher);
    req.session_facts.error_state.last_error.hash(&mut hasher);
    req.session_facts
        .error_state
        .last_error_turn
        .hash(&mut hasher);

    let recent = req
        .messages
        .iter()
        .rev()
        .filter(|message| !is_runtime_owned_message(message))
        .take(64)
        .collect::<Vec<_>>();
    for message in recent.into_iter().rev() {
        message
            .get("role")
            .and_then(Value::as_str)
            .unwrap_or("")
            .hash(&mut hasher);
        if let Some(content) = message.get("content") {
            content.to_string().hash(&mut hasher);
        }
        if let Some(tool_calls) = message.get("tool_calls") {
            tool_calls.to_string().hash(&mut hasher);
        }
        message
            .get("tool_call_id")
            .and_then(Value::as_str)
            .hash(&mut hasher);
    }
    hasher.finish()
}

impl MemoryExtractionService {
    pub async fn current_session_memory_entry_for_pipeline(
        &self,
        session_id: &str,
    ) -> Option<astra_turn_core::context_sources::MemoryEntry> {
        let loaded = if self.require_local_current_snapshot {
            super::runner::load_current_session_memory_preferring_local_with_freshness(
                self.memoria_client.as_ref(),
                session_id,
            )
            .await?
        } else {
            super::runner::load_current_session_memory_with_freshness(
                self.memoria_client.as_ref(),
                session_id,
            )
            .await?
        };
        crate::turn::wire_assembly::session_memory_entry_for_user_turn(
            Some(&loaded.content),
            loaded.updated_turn,
        )
    }

    fn record_selector_failure(&self, model_name: &str, _detail: Option<&str>) {
        self.health.mark_failed(model_name);
    }

    fn record_selector_failures(&self, failures: &[super::runner::LlmCandidateFailure]) {
        for failure in failures {
            self.record_selector_failure(&failure.model_name, failure.detail.as_deref());
        }
    }

    fn mark_session_extracted(
        &self,
        session_id: &str,
        content_fingerprint: u64,
        current_turn: u32,
    ) {
        let mut map = self
            .session_states
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        map.entry(session_id.to_string())
            .or_default()
            .mark_extracted(content_fingerprint, current_turn);
    }

    async fn load_current_memory_with_freshness(
        &self,
        session_id: &str,
    ) -> Option<super::runner::LoadedSessionMemory> {
        if self.require_local_current_snapshot {
            super::runner::load_current_session_memory_preferring_local_with_freshness(
                self.memoria_client.as_ref(),
                session_id,
            )
            .await
        } else {
            super::runner::load_current_session_memory_with_freshness(
                self.memoria_client.as_ref(),
                session_id,
            )
            .await
        }
    }

    // ── event emission helpers ────────────────────────────────────────

    fn enqueue(&self, event: JournalEvent, user_id: &str) {
        if let Some(sink) = self.local_event_sink.as_ref() {
            sink(&event);
        }
        match IngestionEvent::from_journal_event(&event, user_id) {
            Ok(ingestion_event) => self.ingestion.enqueue(ingestion_event),
            Err(error) => tracing::warn!(
                target: "astra_runtime::session_memory",
                error = %error,
                "invalid session memory journal event for cloud ingestion"
            ),
        }
    }

    fn emit_skip_event(
        &self,
        user_id: &str,
        session_id: Option<&str>,
        turn: u32,
        reason: SessionMemoryExtractionSkipReason,
        breadcrumbs: &SessionMemoryExtractionBreadcrumbs,
    ) {
        self.enqueue(
            JournalEvent::session_memory_extraction(
                session_id,
                turn,
                0,
                SessionMemoryExtractionOutcome::Skipped { reason },
                breadcrumbs,
            ),
            user_id,
        );
    }

    fn emit_success_event(
        &self,
        user_id: &str,
        session_id: Option<&str>,
        turn: u32,
        source: SessionMemoryExtractionSource,
        bytes_written: u64,
        duration_ms: u64,
        breadcrumbs: &SessionMemoryExtractionBreadcrumbs,
    ) {
        self.enqueue(
            JournalEvent::session_memory_extraction(
                session_id,
                turn,
                duration_ms,
                SessionMemoryExtractionOutcome::Extracted {
                    source,
                    bytes_written,
                },
                breadcrumbs,
            ),
            user_id,
        );
    }

    fn emit_error_event(
        &self,
        user_id: &str,
        session_id: Option<&str>,
        turn: u32,
        reason: SessionMemoryExtractionErrorReason,
        duration_ms: u64,
        breadcrumbs: &SessionMemoryExtractionBreadcrumbs,
    ) {
        self.enqueue(
            JournalEvent::session_memory_extraction(
                session_id,
                turn,
                duration_ms,
                SessionMemoryExtractionOutcome::Errored { reason },
                breadcrumbs,
            ),
            user_id,
        );
    }
}

// ───────────────────────────────────────────────────────────────────────
// Tests
// ───────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory_hooks::DirectMemoryInferenceClient;
    use crate::turn::cloud::memoria_compact::MemoriaMemory;
    use astra_services::event_ingestion::IngestionEvent;
    use serde_json::json;
    use std::sync::Mutex;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    fn extraction_scope(
        session_id: impl Into<String>,
        turn: u32,
    ) -> astra_turn_types::InferenceInvocationScope {
        astra_turn_types::InferenceInvocationScope::Session {
            session_id: session_id.into(),
            turn,
            round: 0,
            operation_id: "memory_extraction".to_string(),
            logical_attempt: 0,
        }
    }

    /// Minimal capturing mock — records every `store` for assertion.
    #[derive(Default)]
    struct CapturingMemoria {
        stored: Mutex<Vec<(String, String, Option<String>)>>, // (content, memory_type, session_id)
        purged: Mutex<Vec<String>>,
    }

    #[async_trait]
    impl MemoriaPort for CapturingMemoria {
        async fn retrieve_ext(
            &self,
            _query: &str,
            session_id: Option<&str>,
            _top_k: usize,
            _filter_session: bool,
        ) -> Result<Vec<MemoriaMemory>, String> {
            Ok(self
                .stored
                .lock()
                .unwrap()
                .iter()
                .enumerate()
                .filter(|(_, (_, _, stored_session_id))| {
                    session_id.is_none_or(|expected| stored_session_id.as_deref() == Some(expected))
                })
                .map(
                    |(index, (content, memory_type, stored_session_id))| MemoriaMemory {
                        memory_id: format!("mem-{}", index + 1),
                        content: content.clone(),
                        memory_type: memory_type.clone(),
                        retrieval_score: None,
                        observed_at: None,
                        updated_at: None,
                        trust_tier: Some("T3".to_string()),
                        session_id: stored_session_id.clone(),
                        user_id: Some("test-user".to_string()),
                    },
                )
                .collect())
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

    #[derive(Clone, Default)]
    struct OwnerBindingMemoria {
        owner: Option<String>,
        bindings: Arc<Mutex<Vec<String>>>,
    }

    #[async_trait]
    impl MemoriaPort for OwnerBindingMemoria {
        fn bind_owner(&self, user_id: &str) -> Result<Arc<dyn MemoriaPort>, String> {
            let scope = astra_memoria::MemoryScope::new(user_id, "owner-binding-test")?;
            self.bindings.lock().unwrap().push(scope.user_id.clone());
            Ok(Arc::new(Self {
                owner: Some(scope.user_id),
                bindings: Arc::clone(&self.bindings),
            }))
        }

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
            _content: &str,
            _memory_type: &str,
            _session_id: Option<&str>,
            _trust_tier: Option<&str>,
        ) -> Result<String, String> {
            self.owner
                .as_ref()
                .map(|owner| format!("memory-for-{owner}"))
                .ok_or_else(|| "owner was not bound".to_string())
        }

        async fn purge_working(&self, _session_id: &str) -> Result<u64, String> {
            Ok(0)
        }
    }

    #[test]
    fn owner_scoped_services_are_cached_per_owner_and_isolate_session_state() {
        let port = Arc::new(OwnerBindingMemoria::default());
        let bindings = Arc::clone(&port.bindings);
        let (tx, _rx) = IngestionSender::for_tests(16);
        let root = Arc::new(MemoryExtractionService::new_owner_scoped_template(
            Arc::new(ConstMemoryInferenceResolver(None)),
            port,
            tx,
            Arc::new(BackgroundActivityBroker::new()),
        ));

        let alice_a = root.scoped_to_owner("alice").unwrap();
        let alice_b = root.scoped_to_owner("alice").unwrap();
        let bob = root.scoped_to_owner("bob").unwrap();

        assert!(Arc::ptr_eq(&alice_a, &alice_b));
        assert!(!Arc::ptr_eq(&alice_a, &bob));
        assert_eq!(root.owner_user_id(), None);
        assert_eq!(alice_a.owner_user_id(), Some("alice"));
        assert_eq!(bob.owner_user_id(), Some("bob"));
        assert_eq!(&*bindings.lock().unwrap(), &["alice", "bob"]);

        alice_a.mark_session_extracted("same-session", 11, 2);
        assert!(alice_a.peek_state("same-session").is_some());
        assert!(bob.peek_state("same-session").is_none());
        assert!(Arc::ptr_eq(&alice_a.pending, &bob.pending));
    }

    #[test]
    fn owner_scoped_service_cache_prunes_dead_owner_keys() {
        let port = Arc::new(OwnerBindingMemoria::default());
        let (tx, _rx) = IngestionSender::for_tests(16);
        let root = Arc::new(MemoryExtractionService::new_owner_scoped_template(
            Arc::new(ConstMemoryInferenceResolver(None)),
            port,
            tx,
            Arc::new(BackgroundActivityBroker::new()),
        ));

        let alice = root.scoped_to_owner("alice").unwrap();
        assert_eq!(
            root.owner_scoped_services
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .len(),
            1
        );
        drop(alice);

        let _bob = root.scoped_to_owner("bob").unwrap();
        let scoped = root
            .owner_scoped_services
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert_eq!(scoped.len(), 1, "dead owner keys must not accumulate");
        assert!(scoped.contains_key("bob"));
        assert!(!scoped.contains_key("alice"));
    }

    async fn spawn_json_server_with_status(
        assert_request: Arc<dyn Fn(&str) + Send + Sync>,
        status_code: u16,
        reason_phrase: &str,
        body: serde_json::Value,
    ) -> (String, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let body_text = body.to_string();
        let reason_phrase = reason_phrase.to_string();
        let handle = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 32 * 1024];
            let n = socket.read(&mut buf).await.unwrap_or(0);
            let request = String::from_utf8_lossy(&buf[..n]).into_owned();
            assert_request(&request);
            let response = format!(
                "HTTP/1.1 {status_code} {reason_phrase}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                body_text.len(),
                body_text
            );
            let _ = socket.write_all(response.as_bytes()).await;
        });
        (format!("http://{addr}"), handle)
    }

    struct TestCtx {
        svc: Arc<MemoryExtractionService>,
        rx: tokio::sync::mpsc::Receiver<IngestionEvent>,
        memoria: Arc<CapturingMemoria>,
    }

    fn boxed_inference_client(
        selector: Option<DirectMemoryInferenceClient>,
    ) -> Option<MemoryInferenceClient> {
        selector.map(|client| Arc::new(client) as MemoryInferenceClient)
    }

    fn build_ctx(selector: Option<DirectMemoryInferenceClient>) -> TestCtx {
        let (ingestion, rx) = IngestionSender::for_tests(256);
        let broker = Arc::new(BackgroundActivityBroker::new());
        let memoria = Arc::new(CapturingMemoria::default());
        let svc = Arc::new(MemoryExtractionService::new(
            Arc::new(ConstMemoryInferenceResolver(boxed_inference_client(
                selector,
            ))),
            Arc::clone(&memoria) as Arc<dyn MemoriaPort>,
            ingestion,
            "test-user",
            broker,
        ));
        TestCtx { svc, rx, memoria }
    }

    fn build_ctx_with_local_snapshot(selector: Option<DirectMemoryInferenceClient>) -> TestCtx {
        let (ingestion, rx) = IngestionSender::for_tests(256);
        let broker = Arc::new(BackgroundActivityBroker::new());
        let memoria = Arc::new(CapturingMemoria::default());
        let svc = Arc::new(
            MemoryExtractionService::new(
                Arc::new(ConstMemoryInferenceResolver(boxed_inference_client(
                    selector,
                ))),
                Arc::clone(&memoria) as Arc<dyn MemoriaPort>,
                ingestion,
                "test-user",
                broker,
            )
            .with_local_current_snapshot(),
        );
        TestCtx { svc, rx, memoria }
    }

    fn sample_req(session_id: &str, _tokens: usize, had_error: bool) -> ExtractionRequest {
        ExtractionRequest {
            inference_scope: extraction_scope(session_id, 1),
            messages: vec![
                json!({"role": "user", "content": "Design a durable runtime history boundary that separates root conversation history from child agent artifacts and keeps prompt cache stable."}),
                json!({"role": "assistant", "content": "I will inspect the restore path, session history tool, and event persistence boundary, then update the shared runtime code and regression tests."}),
            ],
            session_facts: astra_turn_types::session_facts::SessionFacts::default(),
            had_error,
            reanchors_current_objective: false,
        }
    }

    #[tokio::test]
    async fn owner_neutral_template_refuses_direct_extraction() {
        let memoria = Arc::new(OwnerBindingMemoria::default());
        let (ingestion, _rx) = IngestionSender::for_tests(16);
        let template = Arc::new(MemoryExtractionService::new_owner_scoped_template(
            Arc::new(ConstMemoryInferenceResolver(None)),
            memoria,
            ingestion,
            Arc::new(BackgroundActivityBroker::new()),
        ));

        assert_eq!(
            template.maybe_spawn(sample_req("session-without-owner", 1_000, false)),
            SpawnDecision::Skipped
        );
        assert_eq!(template.owner_user_id(), None);
        assert_eq!(
            template.wait_for_pending(Duration::from_millis(10)).await,
            0
        );
    }

    fn meaningful_shutdown_req(session_id: &str, _tokens: usize) -> ExtractionRequest {
        ExtractionRequest {
            inference_scope: extraction_scope(session_id, 1),
            messages: vec![
                json!({"role": "user", "content": "Need a cache-safe session memory design that still captures shutdown summaries for short sessions and resumed work."}),
                json!({"role": "assistant", "content": "I removed the legacy extractor, fixed the model poisoning bug, and am wiring a final shutdown flush plus resume recap next."}),
            ],
            session_facts: astra_turn_types::session_facts::SessionFacts::default(),
            had_error: false,
            reanchors_current_objective: false,
        }
    }

    fn short_conversation_req(session_id: &str) -> ExtractionRequest {
        ExtractionRequest {
            inference_scope: extraction_scope(session_id, 1),
            messages: vec![
                json!({"role": "user", "content": "1+1"}),
                json!({"role": "assistant", "content": "2"}),
            ],
            session_facts: astra_turn_types::session_facts::SessionFacts::default(),
            had_error: false,
            reanchors_current_objective: false,
        }
    }

    #[test]
    fn extraction_fingerprint_ignores_turn_counter() {
        let mut first = sample_req("fingerprint", 1_000, false);
        let mut second = first.clone();
        second.inference_scope = extraction_scope("fingerprint", 42);

        assert_eq!(
            extraction_input_fingerprint(&first),
            extraction_input_fingerprint(&second),
            "the runtime turn counter is not memory freshness evidence"
        );

        first.messages.push(json!({
            "role": "assistant",
            "content": "Implemented the typed runtime lane and verified its payload."
        }));
        assert_ne!(
            extraction_input_fingerprint(&first),
            extraction_input_fingerprint(&second),
            "new prompt-facing history must invalidate the snapshot"
        );
    }

    #[test]
    fn extraction_fingerprint_ignores_runtime_scaffolding_but_tracks_structured_facts() {
        let base = sample_req("fingerprint-runtime", 1_000, false);
        let mut with_runtime = base.clone();
        with_runtime
            .messages
            .push(astra_turn_types::runtime_owned_message(
                "system",
                "arbitrary runtime payload",
                astra_turn_types::RuntimeMessageDelivery::EphemeralControl,
            ));
        assert_eq!(
            extraction_input_fingerprint(&base),
            extraction_input_fingerprint(&with_runtime),
            "runtime-only messages must not create durable-memory churn"
        );

        let mut with_fact = base.clone();
        with_fact
            .session_facts
            .active_files
            .push(astra_turn_types::session_facts::FileEntry {
                path: "src/session_memory.rs".to_string(),
                last_action: "write".to_string(),
                turn: 2,
            });
        assert_ne!(
            extraction_input_fingerprint(&base),
            extraction_input_fingerprint(&with_fact),
            "successful structured workspace facts are freshness evidence"
        );
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

    fn nanos() -> u128 {
        use std::time::{SystemTime, UNIX_EPOCH};
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    }

    #[tokio::test]
    async fn short_conversation_is_admitted_by_structured_freshness() {
        let TestCtx { svc, .. } = build_ctx(None);
        let sid = format!("short-conversation-{}", nanos());
        let req = short_conversation_req(&sid);

        assert_eq!(svc.maybe_spawn(req), SpawnDecision::Spawned);
    }

    #[tokio::test]
    async fn shutdown_flush_uses_the_same_structured_gate_as_turn_end() {
        let TestCtx { svc, .. } = build_ctx(None);
        let sid = format!("shutdown-tool-info-{}", nanos());
        let mut req = short_conversation_req(&sid);
        req.messages.push(json!({
            "role": "assistant",
            "content": serde_json::Value::Null,
            "tool_calls": [{"function": {"name": "web_fetch"}}]
        }));

        assert_eq!(svc.maybe_spawn_shutdown_flush(req), SpawnDecision::Spawned);
        assert_eq!(svc.wait_for_pending(Duration::from_secs(2)).await, 0);
    }

    #[tokio::test]
    async fn first_meaningful_snapshot_ignores_indirect_size_thresholds() {
        let mut ctx = build_ctx(None);
        let req = sample_req("sess-below", 1_000, false);
        assert_eq!(ctx.svc.maybe_spawn(req), SpawnDecision::Spawned);
        ctx.svc.wait_for_pending(Duration::from_secs(2)).await;

        let events = collect_extraction_events(&mut ctx.rx);
        assert_eq!(events.len(), 1);
        let m = events[0].metadata.as_ref().unwrap();
        assert_eq!(m["outcome"], "extracted");
        assert!(!ctx.memoria.stored.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn ingestion_event_uses_service_owner_as_the_only_authoritative_user() {
        let (ingestion, mut rx) = IngestionSender::for_tests(8);
        let broker = Arc::new(BackgroundActivityBroker::new());
        let memoria = Arc::new(CapturingMemoria::default());
        let svc = Arc::new(MemoryExtractionService::new(
            Arc::new(ConstMemoryInferenceResolver(None)),
            memoria,
            ingestion,
            "service-owner",
            broker,
        ));
        let req = sample_req("sess-service-owner", 1_000, false);

        assert_eq!(svc.maybe_spawn(req), SpawnDecision::Spawned);
        svc.wait_for_pending(Duration::from_secs(2)).await;

        let events = collect_extraction_events(&mut rx);
        assert_eq!(events.len(), 1);
        assert_eq!(
            events[0].user_id, "service-owner",
            "cloud ingestion identity must come from the owner-bound service"
        );
    }

    #[tokio::test]
    async fn durable_snapshot_prevents_same_turn_reextraction_after_service_restart() {
        let memoria = Arc::new(CapturingMemoria::default());
        let (ingestion_a, _rx_a) = IngestionSender::for_tests(16);
        let service_a = Arc::new(MemoryExtractionService::new(
            Arc::new(ConstMemoryInferenceResolver(None)),
            Arc::clone(&memoria) as Arc<dyn MemoriaPort>,
            ingestion_a,
            "test-user",
            Arc::new(BackgroundActivityBroker::new()),
        ));
        let mut first = sample_req("restart-idempotent", 1_000, false);
        first.inference_scope = extraction_scope("restart-idempotent", 7);
        assert_eq!(service_a.maybe_spawn(first.clone()), SpawnDecision::Spawned);
        assert_eq!(service_a.wait_for_pending(Duration::from_secs(2)).await, 0);
        assert_eq!(memoria.stored.lock().unwrap().len(), 1);

        // A fresh coordinator has no process-local fingerprint state. It must
        // still observe the durable snapshot and avoid a second selector/store.
        let (ingestion_b, mut rx_b) = IngestionSender::for_tests(16);
        let service_b = Arc::new(MemoryExtractionService::new(
            Arc::new(ConstMemoryInferenceResolver(None)),
            Arc::clone(&memoria) as Arc<dyn MemoriaPort>,
            ingestion_b,
            "test-user",
            Arc::new(BackgroundActivityBroker::new()),
        ));
        assert_eq!(service_b.maybe_spawn(first), SpawnDecision::Spawned);
        assert_eq!(service_b.wait_for_pending(Duration::from_secs(2)).await, 0);
        assert_eq!(
            memoria.stored.lock().unwrap().len(),
            1,
            "same-turn restart must not write a second snapshot"
        );
        let events = collect_extraction_events(&mut rx_b);
        assert!(events.iter().any(|event| {
            event.metadata.as_ref().is_some_and(|metadata| {
                metadata["outcome"] == "skipped" && metadata["reason"] == "already_current"
            })
        }));
    }

    #[tokio::test]
    async fn shutdown_flush_spawns_for_short_meaningful_session() {
        let ctx = build_ctx(None);
        let sid = format!("shutdown-flush-{}", nanos());
        let req = meaningful_shutdown_req(&sid, 600);
        assert_eq!(
            ctx.svc.maybe_spawn_shutdown_flush(req),
            SpawnDecision::Spawned
        );
        let leftover = ctx.svc.wait_for_pending(Duration::from_secs(2)).await;
        assert_eq!(leftover, 0);
        assert_eq!(ctx.memoria.stored.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn persisted_extraction_writes_local_session_memory_artifact() {
        use astra_services::SessionArtifactStore;

        let tmp = tempfile::TempDir::new().unwrap();
        let _dir_guard = astra_services::session_journal::JournalDirGuard::new(tmp.path());
        let ctx = build_ctx_with_local_snapshot(None);
        let sid = format!("local-session-memory-{}", nanos());
        let req = meaningful_shutdown_req(&sid, 600);

        assert_eq!(
            ctx.svc.maybe_spawn_shutdown_flush(req),
            SpawnDecision::Spawned
        );
        let leftover = ctx.svc.wait_for_pending(Duration::from_secs(2)).await;
        assert_eq!(leftover, 0);

        let path = astra_services::local_session_artifact_store()
            .session_path(&sid, "session-memory.md")
            .unwrap();
        let body = std::fs::read_to_string(&path).expect("session-memory.md");
        assert!(
            body.contains("# Session Memory"),
            "expected local session-memory artifact after successful extraction, got: {body}"
        );
    }

    #[tokio::test]
    async fn persisted_extraction_without_local_snapshot_mode_skips_local_artifact_refresh() {
        use astra_services::SessionArtifactStore;

        let tmp = tempfile::TempDir::new().unwrap();
        let _dir_guard = astra_services::session_journal::JournalDirGuard::new(tmp.path());
        let ctx = build_ctx(None);
        let sid = format!("remote-only-session-memory-{}", nanos());
        let req = meaningful_shutdown_req(&sid, 600);

        assert_eq!(
            ctx.svc.maybe_spawn_shutdown_flush(req),
            SpawnDecision::Spawned
        );
        let leftover = ctx.svc.wait_for_pending(Duration::from_secs(2)).await;
        assert_eq!(leftover, 0);
        assert_eq!(ctx.memoria.stored.lock().unwrap().len(), 1);

        let path = astra_services::local_session_artifact_store()
            .session_path(&sid, "session-memory.md")
            .unwrap();
        assert!(
            !path.exists(),
            "non-CLI mode should not require or materialize a local session-memory artifact"
        );
    }

    #[tokio::test]
    async fn required_local_snapshot_failure_surfaces_write_failed() {
        let tmp = tempfile::TempDir::new().unwrap();
        let _dir_guard = astra_services::session_journal::JournalDirGuard::new(tmp.path());
        let mut ctx = build_ctx_with_local_snapshot(None);
        let sid = "bad/session-id";
        let req = meaningful_shutdown_req(sid, 600);

        assert_eq!(
            ctx.svc.maybe_spawn_shutdown_flush(req),
            SpawnDecision::Spawned
        );
        let leftover = ctx.svc.wait_for_pending(Duration::from_secs(2)).await;
        assert_eq!(leftover, 0);
        assert_eq!(ctx.memoria.stored.lock().unwrap().len(), 1);

        let events = collect_extraction_events(&mut ctx.rx);
        assert!(events.iter().any(|event| {
            let metadata = event.metadata.as_ref().unwrap();
            metadata["outcome"] == "errored" && metadata["reason"] == "write_failed"
        }));
        assert!(
            ctx.svc.peek_state(sid).is_none(),
            "current-session success state must not advance when the required local snapshot failed"
        );
    }

    #[tokio::test]
    async fn shutdown_flush_skips_an_unchanged_canonical_snapshot() {
        let ctx = build_ctx(None);
        let req = ExtractionRequest {
            inference_scope: extraction_scope(format!("shutdown-trivial-{}", nanos()), 1),
            messages: vec![
                json!({"role": "user", "content": "hi"}),
                json!({"role": "assistant", "content": "hello"}),
            ],
            session_facts: astra_turn_types::session_facts::SessionFacts::default(),
            had_error: false,
            reanchors_current_objective: false,
        };
        let fingerprint = extraction_input_fingerprint(&req);
        ctx.svc
            .mark_session_extracted(req.session_id(), fingerprint, req.turn_number());
        assert_eq!(
            ctx.svc.maybe_spawn_shutdown_flush(req),
            SpawnDecision::Skipped
        );
        assert!(ctx.memoria.stored.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn in_flight_dedup_emits_skipped_in_flight() {
        let mut ctx = build_ctx(None);
        let sid = format!("in-flight-{}", nanos());
        let req = sample_req(&sid, 50_000, false);
        let fingerprint = extraction_input_fingerprint(&req);
        {
            let mut work = ctx.svc.work.lock().unwrap();
            work.active_fingerprints.insert(sid.clone(), fingerprint);
        }
        assert_eq!(ctx.svc.maybe_spawn(req), SpawnDecision::Skipped);

        let events = collect_extraction_events(&mut ctx.rx);
        assert!(events.iter().any(|e| {
            let m = e.metadata.as_ref().unwrap();
            m["outcome"] == "skipped" && m["reason"] == "in_flight"
        }));
    }

    #[tokio::test]
    async fn in_flight_changed_snapshot_is_coalesced_as_latest_work() {
        let ctx = build_ctx(None);
        let sid = format!("in-flight-latest-{}", nanos());
        let active = sample_req(&sid, 50_000, false);
        let mut changed = active.clone();
        changed.messages.push(json!({
            "role": "assistant",
            "content": "The newer semantic state must survive the in-flight boundary."
        }));
        {
            let mut work = ctx.svc.work.lock().unwrap();
            work.active_fingerprints
                .insert(sid.clone(), extraction_input_fingerprint(&active));
        }

        assert_eq!(ctx.svc.maybe_spawn(changed.clone()), SpawnDecision::Queued);
        let work = ctx.svc.work.lock().unwrap();
        let (queued, fingerprint) = work.queued_latest.get(&sid).expect("latest queued work");
        assert_eq!(queued.messages, changed.messages);
        assert_eq!(*fingerprint, extraction_input_fingerprint(&changed));
    }

    #[tokio::test]
    async fn queued_latest_snapshot_runs_before_the_session_slot_is_released() {
        struct BlockingFirstRetrieve {
            retrieve_calls: std::sync::atomic::AtomicUsize,
            first_started: tokio::sync::Notify,
            release_first: tokio::sync::Notify,
            stored: Mutex<Vec<String>>,
        }

        #[async_trait]
        impl MemoriaPort for BlockingFirstRetrieve {
            async fn retrieve_ext(
                &self,
                _: &str,
                _: Option<&str>,
                _: usize,
                _: bool,
            ) -> Result<Vec<MemoriaMemory>, String> {
                if self
                    .retrieve_calls
                    .fetch_add(1, std::sync::atomic::Ordering::AcqRel)
                    == 0
                {
                    self.first_started.notify_one();
                    self.release_first.notified().await;
                }
                Ok(Vec::new())
            }

            async fn store(
                &self,
                content: &str,
                _: &str,
                _: Option<&str>,
                _: Option<&str>,
            ) -> Result<String, String> {
                let mut stored = self.stored.lock().unwrap();
                stored.push(content.to_string());
                Ok(format!("mem-{}", stored.len()))
            }

            async fn purge_working(&self, _: &str) -> Result<u64, String> {
                Ok(0)
            }
        }

        let memoria = Arc::new(BlockingFirstRetrieve {
            retrieve_calls: std::sync::atomic::AtomicUsize::new(0),
            first_started: tokio::sync::Notify::new(),
            release_first: tokio::sync::Notify::new(),
            stored: Mutex::new(Vec::new()),
        });
        let (svc, _rx, _broker) =
            build_ctx_with_memoria(None, Arc::clone(&memoria) as Arc<dyn MemoriaPort>);
        let sid = format!("queued-latest-worker-{}", nanos());
        let first = sample_req(&sid, 1_000, false);
        assert_eq!(svc.maybe_spawn(first.clone()), SpawnDecision::Spawned);
        memoria.first_started.notified().await;

        let mut latest = first;
        latest.inference_scope = extraction_scope(&sid, 2);
        latest
            .session_facts
            .active_files
            .push(astra_turn_types::session_facts::FileEntry {
                path: "src/latest-queued-state.rs".to_string(),
                last_action: "write".to_string(),
                turn: 2,
            });
        assert_eq!(svc.maybe_spawn(latest), SpawnDecision::Queued);
        memoria.release_first.notify_waiters();
        assert_eq!(svc.wait_for_pending(Duration::from_secs(2)).await, 0);

        let stored = memoria.stored.lock().unwrap();
        assert_eq!(
            stored.len(),
            2,
            "active and latest queued snapshots both ran"
        );
        let final_memory =
            crate::session_memory::runner::decode_session_memory_entry(&stored[1], &sid)
                .expect("latest canonical snapshot");
        assert!(
            final_memory.contains("src/latest-queued-state.rs"),
            "the queued request's producer-owned facts must reach the second durable snapshot"
        );
    }

    #[tokio::test]
    async fn worker_panic_releases_pending_and_in_flight_slot() {
        struct PanickingMemoria;

        #[async_trait]
        impl MemoriaPort for PanickingMemoria {
            async fn retrieve_ext(
                &self,
                _: &str,
                _: Option<&str>,
                _: usize,
                _: bool,
            ) -> Result<Vec<MemoriaMemory>, String> {
                panic!("intentional test panic in retrieve_ext")
            }

            async fn store(
                &self,
                _: &str,
                _: &str,
                _: Option<&str>,
                _: Option<&str>,
            ) -> Result<String, String> {
                Ok("unreachable".into())
            }

            async fn purge_working(&self, _: &str) -> Result<u64, String> {
                Ok(0)
            }
        }

        let (ingestion, _rx) = IngestionSender::for_tests(256);
        let broker = Arc::new(BackgroundActivityBroker::new());
        let svc = Arc::new(MemoryExtractionService::new(
            Arc::new(ConstMemoryInferenceResolver(None)),
            Arc::new(PanickingMemoria) as Arc<dyn MemoriaPort>,
            ingestion,
            "panic-cleanup-test",
            broker,
        ));
        let sid = format!("panic-cleanup-{}", nanos());

        assert_eq!(
            svc.maybe_spawn(sample_req(&sid, 50_000, false)),
            SpawnDecision::Spawned
        );

        // Generous timeout so CI runners under load don't misread a
        // slow tokio-task dispatch + panic unwind as a cleanup bug.
        // We expect zero leftover in milliseconds; the 2s bound is a
        // safety valve, not the expected duration.
        let leftover = svc.wait_for_pending(Duration::from_secs(2)).await;
        assert_eq!(
            leftover, 0,
            "pending counter must be decremented even if the worker panics"
        );
        assert!(
            !svc.work
                .lock()
                .unwrap()
                .active_fingerprints
                .contains_key(&sid),
            "in-flight slot must be released even if the worker panics"
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
    impl MemoriaPort for ScriptedMemoria {
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
        selector: Option<DirectMemoryInferenceClient>,
        memoria: Arc<dyn MemoriaPort>,
    ) -> (
        Arc<MemoryExtractionService>,
        tokio::sync::mpsc::Receiver<IngestionEvent>,
        Arc<BackgroundActivityBroker>,
    ) {
        let (ingestion, rx) = IngestionSender::for_tests(256);
        let broker = Arc::new(BackgroundActivityBroker::new());
        let svc = Arc::new(MemoryExtractionService::new(
            Arc::new(ConstMemoryInferenceResolver(boxed_inference_client(
                selector,
            ))),
            memoria,
            ingestion,
            "test-user",
            Arc::clone(&broker),
        ));
        (svc, rx, broker)
    }

    #[derive(Debug)]
    struct OrderedMemoryInferenceResolver(Vec<DirectMemoryInferenceClient>);

    #[async_trait]
    impl MemoryInferenceResolver for OrderedMemoryInferenceResolver {
        async fn resolve_candidates(&self, _user_id: &str) -> Vec<MemoryInferenceClient> {
            self.0
                .iter()
                .cloned()
                .map(|client| Arc::new(client) as MemoryInferenceClient)
                .collect()
        }
    }

    fn build_ctx_with_resolver(
        resolver: Arc<dyn MemoryInferenceResolver>,
        memoria: Arc<dyn MemoriaPort>,
    ) -> (
        Arc<MemoryExtractionService>,
        tokio::sync::mpsc::Receiver<IngestionEvent>,
        Arc<BackgroundActivityBroker>,
    ) {
        let (ingestion, rx) = IngestionSender::for_tests(256);
        let broker = Arc::new(BackgroundActivityBroker::new());
        let svc = Arc::new(MemoryExtractionService::new(
            resolver,
            memoria,
            ingestion,
            "test-user",
            Arc::clone(&broker),
        ));
        (svc, rx, broker)
    }

    #[tokio::test]
    async fn store_failure_emits_write_failed_event() {
        let memoria = Arc::new(ScriptedMemoria {
            store_fail_permanently: true,
            ..ScriptedMemoria::new()
        });
        let (svc, mut rx, _broker) =
            build_ctx_with_memoria(None, Arc::clone(&memoria) as Arc<dyn MemoriaPort>);
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
                    assert_eq!(m["persist_detail"], "store down");
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
    async fn persist_failure_does_not_advance_debounce_state() {
        let memoria = Arc::new(ScriptedMemoria {
            store_fail_permanently: true,
            ..ScriptedMemoria::new()
        });
        let (svc, _rx, _broker) =
            build_ctx_with_memoria(None, Arc::clone(&memoria) as Arc<dyn MemoriaPort>);
        let sid = format!("retry-after-fail-{}", nanos());
        let req = sample_req(&sid, 50_000, false);

        assert_eq!(svc.maybe_spawn(req.clone()), SpawnDecision::Spawned);
        svc.wait_for_pending(Duration::from_secs(5)).await;

        assert!(
            svc.peek_state(&sid).is_none(),
            "failed persistence must not mark the session as freshly extracted"
        );
        assert_eq!(
            svc.maybe_spawn(req),
            SpawnDecision::Spawned,
            "same counters should remain retryable after a failed attempt"
        );
    }

    #[tokio::test]
    async fn wait_for_pending_returns_zero_immediately_when_nothing_spawned() {
        let (svc, _rx, _broker) = build_ctx_with_memoria(
            None,
            Arc::new(CapturingMemoria::default()) as Arc<dyn MemoriaPort>,
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
        // An unhealthy selector should no longer leave session memory
        // empty. We degrade to the deterministic rule-fallback path and
        // persist a session-memory snapshot instead of skipping the whole run.
        let selector_params = DirectMemoryInferenceClient {
            base_url: "https://nope.invalid".to_string(),
            api_key: "k".to_string(),
            model_name: "cheap-selector".to_string(),
            wire_model_name: None,
            provider: "test".to_string(),
            request_body_overrides: None,
            header_overrides: std::collections::HashMap::new(),
            completions_url_override: None,
            request_timeout: None,
        };
        let memoria = Arc::new(CapturingMemoria::default());
        let (svc, mut rx, _broker) = build_ctx_with_memoria(
            Some(selector_params.clone()),
            Arc::clone(&memoria) as Arc<dyn MemoriaPort>,
        );
        // Pre-populate the health map with a recent failure so the
        // next attempt finds the selector unhealthy without us having
        // to actually hit the network.
        svc.health.mark_failed(&selector_params.model_name);

        let sid = format!("cooldown-{}", nanos());
        let req = sample_req(&sid, 50_000, false);
        assert_eq!(svc.maybe_spawn(req), SpawnDecision::Spawned);

        let deadline = Instant::now() + Duration::from_secs(5);
        let mut saw_cooldown_skip = false;
        let mut saw_fallback_extract = false;
        while Instant::now() < deadline && !(saw_cooldown_skip && saw_fallback_extract) {
            while let Ok(evt) = rx.try_recv() {
                if evt.event_type != "session_memory_extraction" {
                    continue;
                }
                let m = evt.metadata.as_ref().unwrap();
                if m["outcome"] == "skipped"
                    && m["reason"] == "selector_cooldown"
                    && m["selector_model"] == selector_params.model_name
                {
                    saw_cooldown_skip = true;
                }
                if m["outcome"] == "extracted"
                    && m["source"] == "rule_fallback"
                    && m["selector_model"] == selector_params.model_name
                {
                    saw_fallback_extract = true;
                }
            }
            if !(saw_cooldown_skip && saw_fallback_extract) {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        }
        assert!(
            saw_cooldown_skip,
            "unhealthy selector must still emit skipped{{reason=selector_cooldown}}"
        );
        assert!(
            saw_fallback_extract,
            "unhealthy selector must degrade to extracted{{source=rule_fallback}}"
        );
        assert!(
            !memoria.stored.lock().unwrap().is_empty(),
            "cooldown must still persist fallback memory"
        );
    }

    #[tokio::test]
    async fn selector_cooldown_skips_to_next_healthy_candidate() {
        let (failing_url, failing_handle) = spawn_json_server_with_status(
            Arc::new(|request: &str| {
                assert!(request.starts_with("POST /v1/chat/completions HTTP/1.1"));
            }),
            502,
            "Bad Gateway",
            json!({
                "error": {
                    "message": "selector two unavailable"
                }
            }),
        )
        .await;
        let first = DirectMemoryInferenceClient {
            base_url: "https://nope.invalid".to_string(),
            api_key: "k".to_string(),
            model_name: "selector-first".to_string(),
            wire_model_name: None,
            provider: "openai".to_string(),
            request_body_overrides: None,
            header_overrides: std::collections::HashMap::new(),
            completions_url_override: None,
            request_timeout: None,
        };
        let second = DirectMemoryInferenceClient {
            base_url: format!("{failing_url}/v1"),
            model_name: "selector-second".to_string(),
            ..first.clone()
        };
        let memoria = Arc::new(CapturingMemoria::default());
        let (svc, mut rx, _broker) = build_ctx_with_resolver(
            Arc::new(OrderedMemoryInferenceResolver(vec![
                first.clone(),
                second.clone(),
            ])),
            Arc::clone(&memoria) as Arc<dyn MemoriaPort>,
        );
        svc.health.mark_failed(&first.model_name);

        let sid = format!("next-candidate-{}", nanos());
        let req = sample_req(&sid, 50_000, false);
        assert_eq!(svc.maybe_spawn(req), SpawnDecision::Spawned);

        let deadline = Instant::now() + Duration::from_secs(5);
        let mut attempted_second = false;
        while Instant::now() < deadline && !attempted_second {
            while let Ok(evt) = rx.try_recv() {
                if evt.event_type != "session_memory_extraction" {
                    continue;
                }
                let metadata = evt.metadata.as_ref().unwrap();
                if metadata["outcome"] == "extracted"
                    && metadata["source"] == "rule_fallback"
                    && metadata["selector_model"] == second.model_name
                {
                    attempted_second = true;
                }
            }
            if !attempted_second {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        }
        assert!(
            attempted_second,
            "service should skip the cooled-down selector and attempt the next healthy candidate"
        );
        failing_handle.await.unwrap();
    }

    // ── concurrency / edge cases ─────────────────────────────────────

    // ── cross-turn state persistence (regression for turn-scoped state bug) ──

    /// Different sessions must not pollute each other's debounce state.
    #[tokio::test]
    async fn debounce_state_is_per_session() {
        let memoria = Arc::new(CapturingMemoria::default());
        let (svc, _rx, _broker) =
            build_ctx_with_memoria(None, Arc::clone(&memoria) as Arc<dyn MemoriaPort>);
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
        assert!(a_state.initialized);
        assert!(b_state.initialized);
        assert_eq!(a_state.turn_at_last_extraction, 1);
        assert_eq!(b_state.turn_at_last_extraction, 1);
    }

    /// `forget_session` must clear the entry so a second extraction for
    /// the same session_id after session-end starts fresh.
    #[tokio::test]
    async fn forget_session_clears_debounce_state() {
        let memoria = Arc::new(CapturingMemoria::default());
        let (svc, _rx, _broker) =
            build_ctx_with_memoria(None, Arc::clone(&memoria) as Arc<dyn MemoriaPort>);
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

        // A later reuse of the same sid starts fresh regardless of prompt size.
        let req2 = sample_req(&sid, 5_000, false);
        assert_eq!(
            svc.maybe_spawn(req2),
            SpawnDecision::Spawned,
            "after forget, the semantic snapshot should initialize again"
        );
        svc.wait_for_pending(Duration::from_secs(2)).await;
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
        impl MemoriaPort for FailingMemoria {
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
            Arc::new(ConstMemoryInferenceResolver(None)),
            Arc::new(FailingMemoria) as Arc<dyn MemoriaPort>,
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
    async fn memoria_cooldown_suppresses_repeated_background_attempts() {
        let (svc, mut rx, _broker) = build_breaker_ctx();

        // Two failing attempts trip the breaker.
        for i in 0..2 {
            let sid = format!("fail-{i}-{}", nanos());
            let req = sample_req(&sid, 20_000, false);
            assert_eq!(svc.maybe_spawn(req), SpawnDecision::Spawned);
            svc.wait_for_pending(Duration::from_secs(2)).await;
        }

        // Third attempt: endpoint is cooling down → skipped synchronously, no
        // new HTTP attempt, no spawn.
        let sid = format!("tripped-{}", nanos());
        let req = sample_req(&sid, 20_000, false);
        assert_eq!(
            svc.maybe_spawn(req),
            SpawnDecision::Skipped,
            "cooldown must suppress the third background attempt"
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
    async fn in_flight_skip_does_not_consume_cooldown_recovery() {
        let (svc, _rx, _broker) = build_breaker_ctx();

        for i in 0..2 {
            let sid = format!("trip-{i}-{}", nanos());
            assert_eq!(
                svc.maybe_spawn(sample_req(&sid, 20_000, false)),
                SpawnDecision::Spawned
            );
            svc.wait_for_pending(Duration::from_secs(2)).await;
        }

        tokio::time::sleep(Duration::from_millis(120)).await;

        let skipped_sid = format!("busy-probe-{}", nanos());
        let skipped_req = sample_req(&skipped_sid, 20_000, false);
        {
            let mut work = svc.work.lock().unwrap();
            work.active_fingerprints.insert(
                skipped_sid.clone(),
                extraction_input_fingerprint(&skipped_req),
            );
        }
        assert_eq!(
            svc.maybe_spawn(skipped_req),
            SpawnDecision::Skipped,
            "in-flight skip must not consume endpoint recovery"
        );
        {
            let mut work = svc.work.lock().unwrap();
            work.active_fingerprints.remove(&skipped_sid);
        }

        let next_sid = format!("next-probe-{}", nanos());
        assert_eq!(
            svc.maybe_spawn(sample_req(&next_sid, 20_000, false)),
            SpawnDecision::Spawned,
            "the next eligible request must still be allowed to retry"
        );
        svc.wait_for_pending(Duration::from_secs(2)).await;
    }

    /// A successful rule fallback is a successful Memoria write and must reset
    /// endpoint cooldown even when every selector model is cooling down.
    #[tokio::test]
    async fn successful_rule_fallback_resets_memoria_cooldown() {
        // A successful-Memoria client so the trip below is exclusively
        // driven by `record_failure` we call directly (no network in
        // the test).
        let memoria = Arc::new(CapturingMemoria::default());
        let selector_params = DirectMemoryInferenceClient {
            base_url: "https://nope.invalid".to_string(),
            api_key: "k".to_string(),
            model_name: "cheap-selector-leak".to_string(),
            wire_model_name: None,
            provider: "test".to_string(),
            request_body_overrides: None,
            header_overrides: std::collections::HashMap::new(),
            completions_url_override: None,
            request_timeout: None,
        };
        let (ingestion, _rx) = IngestionSender::for_tests(256);
        let broker = Arc::new(BackgroundActivityBroker::new());
        let mut svc = MemoryExtractionService::new(
            Arc::new(ConstMemoryInferenceResolver(boxed_inference_client(Some(
                selector_params.clone(),
            )))),
            Arc::clone(&memoria) as Arc<dyn MemoriaPort>,
            ingestion,
            "probe-leak-test",
            Arc::clone(&broker),
        );
        // Low threshold + fast cooldown keeps the test deterministic.
        svc.memoria_health = Arc::new(crate::session_memory::health::MemoriaHealth::with_config(
            1,
            Duration::from_millis(50),
        ));
        let svc = Arc::new(svc);

        // Start in cooldown by recording one failure directly.
        svc.memoria_health.record_failure();
        assert!(
            svc.memoria_health.state().tripped,
            "fixture pre-condition: endpoint must be cooling down"
        );

        // 2. Mark the selector unhealthy so `run_one` will degrade to
        //    the rule-fallback path instead of attempting the LLM.
        svc.health.mark_failed(&selector_params.model_name);

        // Wait until normal attempts are admitted again.
        tokio::time::sleep(Duration::from_millis(70)).await;

        // The worker stores fallback memory and records endpoint success.
        let sid = format!("probe-leak-{}", nanos());
        let req = sample_req(&sid, 20_000, false);
        assert_eq!(svc.maybe_spawn(req), SpawnDecision::Spawned);
        svc.wait_for_pending(Duration::from_secs(2)).await;

        // Cooldown and failure count reset after the successful write.
        let admit_after = svc.memoria_health.admit();
        assert_eq!(
            admit_after,
            crate::session_memory::health::MemoriaAdmit::Ready,
            "successful fallback during selector cooldown should reset endpoint health; got {admit_after:?}"
        );
    }

    /// Concurrent refreshes for one session have one owner; the failing owner
    /// updates endpoint cooldown once and the skipped callers do not mutate it.
    #[tokio::test]
    async fn concurrent_refresh_has_one_owner_and_one_cooldown_update() {
        let (svc, _rx, _broker) = build_breaker_ctx();

        // Trip the breaker (2 failures, cfg threshold = 2).
        for i in 0..2 {
            let sid = format!("trip-conc-{i}-{}", nanos());
            assert_eq!(
                svc.maybe_spawn(sample_req(&sid, 20_000, false)),
                SpawnDecision::Spawned
            );
            svc.wait_for_pending(Duration::from_secs(2)).await;
        }
        // Wait past cooldown so attempts are admitted again.
        tokio::time::sleep(Duration::from_millis(120)).await;

        // The in-flight set serializes callers to one worker.
        let sid = format!("race-probe-{}", nanos());
        let mut handles = Vec::new();
        for _ in 0..8 {
            let svc = Arc::clone(&svc);
            let sid = sid.clone();
            handles.push(tokio::spawn(async move {
                svc.maybe_spawn(sample_req(&sid, 20_000, false))
            }));
        }
        let mut spawned = 0usize;
        let mut queued = 0usize;
        let mut skipped = 0usize;
        for h in handles {
            match h.await.unwrap() {
                SpawnDecision::Spawned => spawned += 1,
                SpawnDecision::Queued => queued += 1,
                SpawnDecision::Skipped => skipped += 1,
            }
        }
        assert_eq!(
            spawned, 1,
            "exactly one racer must win the in-flight claim; got {spawned} spawned"
        );
        assert_eq!(
            queued, 0,
            "identical fingerprints should deduplicate, not queue"
        );
        assert_eq!(skipped, 7, "the rest must skip; got {skipped} skipped");

        svc.wait_for_pending(Duration::from_secs(3)).await;

        assert_eq!(
            svc.memoria_health.admit(),
            crate::session_memory::health::MemoriaAdmit::CoolingDown
        );
    }

    // ── Breadcrumb fields in emitted events ─────────────────────────

    #[tokio::test]
    async fn skip_event_carries_messages_count_but_no_attempt() {
        let memoria = Arc::new(CapturingMemoria::default());
        let (svc, mut rx, _broker) =
            build_ctx_with_memoria(None, Arc::clone(&memoria) as Arc<dyn MemoriaPort>);
        let sid = format!("bc-skip-{}", nanos());
        let req = ExtractionRequest {
            inference_scope: extraction_scope(sid, 1),
            messages: vec![json!({"role": "user", "content": "x"})],
            session_facts: astra_turn_types::session_facts::SessionFacts::default(),
            had_error: false,
            reanchors_current_objective: false,
        };
        let fingerprint = extraction_input_fingerprint(&req);
        svc.mark_session_extracted(req.session_id(), fingerprint, req.turn_number());
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
