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
use super::gate::{evaluate, GateDecision};
use super::health::{MemoriaAdmit, MemoriaHealth, SelectorHealth};
use super::observatory::{
    clip_preview, ExtractionOutcome as ObsExtractionOutcome,
    ExtractionRecord as ObsExtractionRecord, ExtractionTrigger, SessionMemoryObservatory,
};
use super::request::{ExtractionRequest, SpawnDecision};
use super::runner::{run_extraction, ExtractionArtifacts};

#[cfg(test)]
type MaybeSpawnAfterGateHook = std::sync::Arc<dyn Fn(&ExtractionRequest) + Send + Sync + 'static>;

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
    /// Set of session_ids currently being extracted. Guarded by a
    /// **std** mutex, not tokio — the critical section is a
    /// `HashSet::insert/remove` with no `.await` inside, so blocking
    /// is bounded to a cache-line update. Using `tokio::sync::Mutex`
    /// here was wrong: `maybe_spawn` is a sync entry point that used
    /// `try_lock()`, which races with the async `release_in_flight`
    /// holding `.lock().await` and spuriously fails — emitting
    /// `InFlight` skips for sessions that were admissible and, worse,
    /// consuming half-open breaker probe slots that should have
    /// retried. std::sync::Mutex eliminates the sync/async boundary
    /// and lets every caller use the same blocking `.lock()`.
    in_flight: Arc<std::sync::Mutex<std::collections::HashSet<String>>>,
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
    /// Optional post-hoc ring for operator introspection. `None` when
    /// the runtime boots without observability wiring (tests, minimal
    /// CLI modes). Every `maybe_spawn` / `run_one` path writes a record
    /// — including skips — when this is `Some`. No effect on LLM
    /// payloads or cache hashes by construction.
    observatory: Option<Arc<SessionMemoryObservatory>>,
    /// Per-service test hook fired right after the gate evaluates
    /// inside `maybe_spawn`, BEFORE the breaker/in-flight/spawn
    /// decision lands. Scoped to one service so parallel tests
    /// cannot pollute each other's critical-section timing.
    ///
    /// Previously this lived as a `static` — any parallel
    /// `#[tokio::test]` installing a hook (especially one with a
    /// deliberate `thread::sleep`) would fire against every other
    /// concurrent `maybe_spawn` call across unrelated tests, giving
    /// flaky failures and invalidating the very TOCTOU regression
    /// test the hook was added to support.
    #[cfg(test)]
    maybe_spawn_after_gate_hook: std::sync::Mutex<Option<MaybeSpawnAfterGateHook>>,
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
            in_flight: Arc::new(std::sync::Mutex::new(std::collections::HashSet::new())),
            broker,
            pending: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            pending_done: Arc::new(tokio::sync::Notify::new()),
            session_states: Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
            observatory: None,
            #[cfg(test)]
            maybe_spawn_after_gate_hook: std::sync::Mutex::new(None),
        }
    }

    /// Per-service test hook install / clear.
    /// Only available in test builds. Replacing any previously-set
    /// hook.
    #[cfg(test)]
    pub(super) fn set_maybe_spawn_after_gate_hook(&self, hook: Option<MaybeSpawnAfterGateHook>) {
        *self.maybe_spawn_after_gate_hook.lock().unwrap() = hook;
    }

    /// Fire the per-service test hook if one is installed.
    #[cfg(test)]
    fn run_maybe_spawn_after_gate_hook(&self, req: &ExtractionRequest) {
        let hook = self
            .maybe_spawn_after_gate_hook
            .lock()
            .unwrap()
            .as_ref()
            .cloned();
        if let Some(hook) = hook {
            hook(req);
        }
    }

    /// Attach a post-hoc observatory. Callers wire one per-process and
    /// share the `Arc` between the service and the compaction write
    /// site so both surfaces populate a single ring set for introspect.
    pub fn with_observatory(mut self, observatory: Arc<SessionMemoryObservatory>) -> Self {
        self.observatory = Some(observatory);
        self
    }

    /// Read-only handle to the observatory. `None` when the service
    /// was built without one. Callers: `introspect`, tests.
    pub fn observatory(&self) -> Option<&Arc<SessionMemoryObservatory>> {
        self.observatory.as_ref()
    }

    /// Live circuit breaker snapshot, for introspect. Cheap — no locks
    /// beyond the breaker's own.
    pub fn memoria_breaker_state(&self) -> super::health::MemoriaHealthSnapshot {
        self.memoria_health.state()
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
        // Breadcrumbs for sync-path skip events. `selector_model` and
        // `attempt` only make sense in the async worker after LLM
        // resolve / persist attempt.
        let skip_breadcrumbs = SessionMemoryExtractionBreadcrumbs {
            messages_count: Some(req.messages.len() as u32),
            selector_model: None,
            attempt: None,
        };

        enum Admission {
            Spawn {
                trigger: ExtractionTrigger,
                /// Breaker admission outcome that let us spawn
                /// (`Closed` or `HalfOpenProbe`). Carried into the
                /// worker so a [`ProbeGuard`](super::health::ProbeGuard)
                /// can auto-release the probe slot on any
                /// early-return path.
                memoria_admit: MemoriaAdmit,
            },
            /// Skip without touching the breaker. Used by gate-level
            /// skips (NoGrowth, BelowInitGate, NoSessionId) and by
            /// MemoriaUnhealthy — none of them were ever offered a
            /// probe slot, so there's nothing to cancel.
            Skip {
                trigger: ExtractionTrigger,
                reason: SessionMemoryExtractionSkipReason,
                label: &'static str,
            },
            /// Skip AND release a HalfOpenProbe slot that was
            /// speculatively granted by `admit()`. Only fires on the
            /// in-flight-collision branch: the breaker already handed
            /// out a probe, but another worker claimed the sid first,
            /// so this caller must return the probe via
            /// `record_probe_cancelled` before returning.
            ///
            /// Split from `Skip` so the two code paths are obvious at
            /// the match site and so a future contributor can't
            /// accidentally add a skip reason that silently forgets to
            /// cancel a probe by defaulting a `bool` field.
            SkipCancelProbe {
                trigger: ExtractionTrigger,
                reason: SessionMemoryExtractionSkipReason,
                label: &'static str,
            },
        }

        // Keep gate evaluation, external admission checks, and debounce
        // advancement in one critical section. Otherwise two callers can
        // both evaluate a stale pre-extraction state and the second can
        // spawn after the first worker has already completed.
        let admission = {
            let mut map = match self.session_states.lock() {
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
            // Infer trigger for observatory records. Mirrors the gate's
            // branches: error bypasses debounce regardless of init
            // state; absent init state means the init gate is the
            // first reason anything fires; otherwise a growth-delta
            // crossing.
            let trig = if req.had_error {
                ExtractionTrigger::ErrorOverride
            } else if !state.initialized {
                ExtractionTrigger::InitGate
            } else {
                ExtractionTrigger::GrowthGate
            };
            let dec = evaluate(
                state,
                &req.session_id,
                req.current_tokens,
                req.current_tool_calls,
                req.had_error,
                &req.config,
            );

            #[cfg(test)]
            self.run_maybe_spawn_after_gate_hook(&req);

            if let GateDecision::Skip(reason) = dec {
                Admission::Skip {
                    trigger: trig,
                    reason,
                    label: skip_reason_label(reason),
                }
            } else {
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
                let memoria_admit = self.memoria_health.admit();
                match memoria_admit {
                    MemoriaAdmit::Open => Admission::Skip {
                        trigger: trig,
                        reason: SessionMemoryExtractionSkipReason::MemoriaUnhealthy,
                        label: "memoria_unhealthy",
                    },
                    MemoriaAdmit::Closed | MemoriaAdmit::HalfOpenProbe => {
                        // Claim the in-flight slot synchronously. `in_flight`
                        // is a std mutex guarding only a HashSet — the
                        // critical section is short and `.await`-free, so
                        // blocking is bounded. Poisoning is treated as
                        // "someone panicked mid-update"; rather than refuse
                        // the caller we recover the inner state (the set's
                        // invariants don't depend on what the panicking
                        // thread was doing).
                        let mut set = self
                            .in_flight
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner);
                        if set.insert(req.session_id.clone()) {
                            let entry = map.entry(req.session_id.clone()).or_default();
                            entry.mark_extracted(req.current_tokens, req.current_tool_calls);
                            Admission::Spawn {
                                trigger: trig,
                                memoria_admit,
                            }
                        } else {
                            // Defer breaker mutation to the match arm
                            // below so `session_states` is unlocked
                            // first (avoids session_states →
                            // memoria_health nested-lock edge).
                            if matches!(memoria_admit, MemoriaAdmit::HalfOpenProbe) {
                                Admission::SkipCancelProbe {
                                    trigger: trig,
                                    reason: SessionMemoryExtractionSkipReason::InFlight,
                                    label: "in_flight",
                                }
                            } else {
                                Admission::Skip {
                                    trigger: trig,
                                    reason: SessionMemoryExtractionSkipReason::InFlight,
                                    label: "in_flight",
                                }
                            }
                        }
                    }
                }
            }
        };

        let (trigger, memoria_admit) = match admission {
            Admission::Spawn {
                trigger,
                memoria_admit,
            } => (trigger, memoria_admit),
            Admission::Skip {
                trigger,
                reason,
                label,
            } => {
                let sid_opt = if req.session_id.is_empty() {
                    None
                } else {
                    Some(req.session_id.as_str())
                };
                self.emit_skip_event(sid_opt, req.turn_number, reason, &skip_breadcrumbs);
                self.record_skipped(sid_opt, req.turn_number, trigger, label, None);
                return SpawnDecision::Skipped;
            }
            Admission::SkipCancelProbe {
                trigger,
                reason,
                label,
            } => {
                // Release the half-open probe slot that `admit()`
                // speculatively handed out. Done here — after the
                // `session_states` lock was released at the end of the
                // admission block — to avoid a session_states →
                // memoria_health nested-lock edge.
                self.memoria_health.record_probe_cancelled();
                let sid_opt = if req.session_id.is_empty() {
                    None
                } else {
                    Some(req.session_id.as_str())
                };
                self.emit_skip_event(sid_opt, req.turn_number, reason, &skip_breadcrumbs);
                self.record_skipped(sid_opt, req.turn_number, trigger, label, None);
                return SpawnDecision::Skipped;
            }
        };

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
        tokio::spawn(async move {
            let _pending_guard = pending_guard;
            svc.run_one(req, trigger, memoria_admit).await;
        });
        SpawnDecision::Spawned
    }

    // ── internals ─────────────────────────────────────────────────────

    async fn run_one(
        self: Arc<Self>,
        req: ExtractionRequest,
        trigger: ExtractionTrigger,
        memoria_admit: MemoriaAdmit,
    ) {
        // RAII guard: guarantees the breaker's probe slot (if any)
        // is released on every exit path. Previously each early
        // return had to remember to call `record_success` /
        // `record_failure` / `record_probe_cancelled`; the selector-
        // cooldown branch forgot, which stranded the breaker half-
        // open forever after a flaky LLM selector.
        //
        // Held in an `Option` so terminal arms can `.take()` and move
        // the guard into `record_success()` / `record_failure()`. If
        // none fires (selector_cooldown early-return, panic, etc.)
        // the guard is dropped here without disposition — which
        // calls `record_probe_cancelled` for HalfOpenProbe and is a
        // no-op for Closed.
        // Drop order is semantically load-bearing and relies on Rust's
        // reverse-declaration drop rule: at the end of `run_one`, locals
        // drop in *reverse* of declaration. We want:
        //
        //   1. `probe_guard` drops first  → breaker disposition settled
        //                                    (record_success / record_failure
        //                                    / record_probe_cancelled) BEFORE
        //                                    a follow-up `maybe_spawn` can
        //                                    observe the probe slot as free.
        //   2. `_in_flight_guard` drops next → the in-flight claim is
        //                                      released only AFTER the
        //                                      breaker state is coherent, so
        //                                      another caller that squeezes
        //                                      in on the just-freed slot sees
        //                                      the post-extraction breaker
        //                                      state, not a mid-transition
        //                                      one.
        //
        // Declaration order below is therefore: probe_guard, then
        // _in_flight_guard. Do not reorder.
        let mut probe_guard = Some(super::health::ProbeGuard::new(
            Arc::clone(&self.memoria_health),
            memoria_admit,
        ));
        let session_id = req.session_id.clone();
        let _in_flight_guard = InFlightGuard::new(Arc::clone(&self.in_flight), session_id.clone());
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
            let model_name = selector_params.as_ref().map(|p| p.model_name.clone());
            let bc = SessionMemoryExtractionBreadcrumbs {
                messages_count: Some(messages_count),
                selector_model: model_name.clone(),
                attempt: None,
            };
            self.emit_skip_event(
                Some(&session_id),
                turn,
                SessionMemoryExtractionSkipReason::SelectorCooldown,
                &bc,
            );
            self.record_skipped(
                Some(&session_id),
                turn,
                trigger,
                "selector_cooldown",
                model_name,
            );
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
        let latency = started.elapsed();
        if std::env::var("ASTRA_SESSION_MEMORY_TRACE").is_ok() {
            let tag = match &artifacts {
                ExtractionArtifacts::Persisted {
                    source,
                    bytes_written,
                    store_attempt,
                    ..
                } => {
                    format!(
                        "Persisted{{source={source:?}, bytes={bytes_written}, attempt={store_attempt}}}"
                    )
                }
                ExtractionArtifacts::LlmFailedPersistedFallback {
                    error_reason,
                    bytes_written,
                    store_attempt,
                    ..
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
                content,
            } => {
                // Memoria accepted a write → breaker closes (or stays
                // closed) and the consecutive-failure counter resets.
                if let Some(g) = probe_guard.take() {
                    g.record_success();
                }
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
                let (sections, preview) = summarize_persisted_content(&content);
                self.record_extraction_outcome(
                    &session_id,
                    turn,
                    trigger,
                    match source {
                        SessionMemoryExtractionSource::Llm => selector_model_used.clone(),
                        SessionMemoryExtractionSource::RuleFallback => None,
                    },
                    ObsExtractionOutcome::Persisted {
                        source: source.into(),
                        bytes_written,
                        store_attempt,
                    },
                    sections,
                    preview,
                    latency,
                );
            }
            ExtractionArtifacts::LlmFailedPersistedFallback {
                error_reason,
                bytes_written,
                store_attempt,
                content,
            } => {
                if let Some(name) = params_for_health.as_deref() {
                    self.health.mark_failed(name);
                }
                // Memoria persist still succeeded on this branch, so
                // the circuit breaker resets. Only the LLM selector
                // model is marked unhealthy.
                if let Some(g) = probe_guard.take() {
                    g.record_success();
                }
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
                let (sections, preview) = summarize_persisted_content(&content);
                self.record_extraction_outcome(
                    &session_id,
                    turn,
                    trigger,
                    selector_model_used.clone(),
                    ObsExtractionOutcome::LlmFailedFallbackPersisted {
                        reason: error_reason.into(),
                        bytes_written,
                        store_attempt,
                    },
                    sections,
                    preview,
                    latency,
                );
            }
            ExtractionArtifacts::PersistFailed { error_reason } => {
                // Memoria persist failed → breaker counts it. Enough
                // consecutive failures trip the breaker and skip
                // future `maybe_spawn` until the cooldown elapses.
                if let Some(g) = probe_guard.take() {
                    g.record_failure();
                }
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
                self.record_extraction_outcome(
                    &session_id,
                    turn,
                    trigger,
                    selector_model_used.clone(),
                    ObsExtractionOutcome::PersistFailed {
                        reason: error_reason.into(),
                    },
                    Vec::new(),
                    String::new(),
                    latency,
                );
            }
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

/// RAII release of an entry in the in-flight `HashSet`. Decoupled
/// from `MemoryExtractionService` so the guard doesn't keep the
/// whole service alive just for one set-removal — it holds only the
/// Arc to the set it needs to mutate, which is the same handle the
/// service itself uses (and which is already `Arc`-shared).
struct InFlightGuard {
    set: Arc<std::sync::Mutex<std::collections::HashSet<String>>>,
    session_id: String,
}

impl InFlightGuard {
    fn new(
        set: Arc<std::sync::Mutex<std::collections::HashSet<String>>>,
        session_id: String,
    ) -> Self {
        Self { set, session_id }
    }
}

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        // Mirrors `release_in_flight`: std mutex, no `.await`,
        // poison-recover so a panicking prior holder doesn't strand
        // future workers.
        let mut set = self
            .set
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        set.remove(&self.session_id);
    }
}

/// Map a skip reason enum to a stable string label. Kept out of
/// `MemoriaHealth` / `Gate` because observatory tags need to stay
/// consistent across `session_journal` schema evolution.
fn skip_reason_label(reason: SessionMemoryExtractionSkipReason) -> &'static str {
    match reason {
        SessionMemoryExtractionSkipReason::NoSessionId => "no_session_id",
        SessionMemoryExtractionSkipReason::BelowInitGate => "below_init_gate",
        SessionMemoryExtractionSkipReason::NoGrowth => "no_growth",
        SessionMemoryExtractionSkipReason::InFlight => "in_flight",
        SessionMemoryExtractionSkipReason::SelectorCooldown => "selector_cooldown",
        SessionMemoryExtractionSkipReason::MemoriaUnhealthy => "memoria_unhealthy",
    }
}

/// Extract narrative section titles + clipped preview from persisted
/// L1 content. Safe on malformed content — returns empty sections and
/// a whatever-is-there preview.
fn summarize_persisted_content(content: &str) -> (Vec<String>, String) {
    use crate::turn::cloud::session_memory_protocol::SessionMemory;
    let sections = SessionMemory::parse(content)
        .map(|m| {
            m.section_names()
                .iter()
                .map(|s| s.to_string())
                .collect::<Vec<String>>()
        })
        .unwrap_or_default();
    (sections, clip_preview(content))
}

impl MemoryExtractionService {
    async fn load_current_memory(&self, session_id: &str) -> String {
        use crate::turn::cloud::session_memory_protocol::{pick_latest_l1, SESSION_MEMORY_PREFIX};
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

    // ── event emission helpers ────────────────────────────────────────

    fn enqueue(&self, event: JournalEvent) {
        let ingestion_event = IngestionEvent::from_journal_event(&event, &self.user_id);
        self.ingestion.enqueue(ingestion_event);
    }

    // ── observatory helpers (all no-op when observatory=None) ────────

    fn record_skipped(
        &self,
        session_id: Option<&str>,
        turn: u32,
        trigger: ExtractionTrigger,
        reason: &str,
        selector_model: Option<String>,
    ) {
        let Some(obs) = self.observatory.as_ref() else {
            return;
        };
        let Some(sid) = session_id else {
            return;
        };
        obs.record_extraction(ObsExtractionRecord {
            session_id: sid.to_string(),
            turn,
            at: std::time::SystemTime::now(),
            trigger,
            selector_model,
            outcome: ObsExtractionOutcome::Skipped {
                reason: reason.to_string(),
            },
            narrative_sections: Vec::new(),
            content_preview: String::new(),
            latency: Duration::ZERO,
        });
    }

    fn record_extraction_outcome(
        &self,
        session_id: &str,
        turn: u32,
        trigger: ExtractionTrigger,
        selector_model: Option<String>,
        outcome: ObsExtractionOutcome,
        narrative_sections: Vec<String>,
        content_preview: String,
        latency: Duration,
    ) {
        let Some(obs) = self.observatory.as_ref() else {
            return;
        };
        obs.record_extraction(ObsExtractionRecord {
            session_id: session_id.to_string(),
            turn,
            at: std::time::SystemTime::now(),
            trigger,
            selector_model,
            outcome,
            narrative_sections,
            content_preview,
            latency,
        });
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

    /// Drop-guard that clears the hook on the specific service it
    /// was installed on. Owning an `Arc` to the service keeps the
    /// service alive for the guard's lifetime, so there's no dangling-
    /// reference hazard even if the test future drops early.
    struct MaybeSpawnAfterGateHookGuard {
        svc: Arc<MemoryExtractionService>,
    }

    impl Drop for MaybeSpawnAfterGateHookGuard {
        fn drop(&mut self) {
            self.svc.set_maybe_spawn_after_gate_hook(None);
        }
    }

    /// Install a hook that fires after the gate evaluates but
    /// before breaker/in-flight/spawn, on this specific service
    /// only. Parallel tests operating on other services are
    /// unaffected.
    fn install_maybe_spawn_after_gate_hook(
        svc: &Arc<MemoryExtractionService>,
        hook: MaybeSpawnAfterGateHook,
    ) -> MaybeSpawnAfterGateHookGuard {
        svc.set_maybe_spawn_after_gate_hook(Some(hook));
        MaybeSpawnAfterGateHookGuard {
            svc: Arc::clone(svc),
        }
    }

    struct ProbeCancelHookGuard {
        svc: Arc<MemoryExtractionService>,
    }

    impl Drop for ProbeCancelHookGuard {
        fn drop(&mut self) {
            self.svc
                .memoria_health
                .set_record_probe_cancelled_hook(None);
        }
    }

    fn install_probe_cancel_hook(
        svc: &Arc<MemoryExtractionService>,
        hook: Arc<dyn Fn() + Send + Sync + 'static>,
    ) -> ProbeCancelHookGuard {
        svc.memoria_health
            .set_record_probe_cancelled_hook(Some(hook));
        ProbeCancelHookGuard {
            svc: Arc::clone(svc),
        }
    }

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
            let mut set = ctx.svc.in_flight.lock().unwrap();
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
    async fn worker_panic_releases_pending_and_in_flight_slot() {
        struct PanickingMemoria;

        #[async_trait]
        impl MemoriaClient for PanickingMemoria {
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
            Arc::new(ConstSelectorResolver(None)),
            Arc::new(PanickingMemoria) as Arc<dyn MemoriaClient>,
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
            !svc.in_flight.lock().unwrap().contains(&sid),
            "in-flight slot must be released even if the worker panics"
        );
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

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn concurrent_same_session_cannot_use_stale_gate_after_first_worker_finishes() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        struct CountingMemoria {
            stored: Arc<AtomicUsize>,
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
                self.stored.fetch_add(1, Ordering::AcqRel);
                Ok("ok".into())
            }

            async fn purge_working(&self, _: &str) -> Result<u64, String> {
                Ok(0)
            }
        }

        let stored = Arc::new(AtomicUsize::new(0));
        let memoria = Arc::new(CountingMemoria {
            stored: Arc::clone(&stored),
        });
        let (svc, _rx, _broker) =
            build_ctx_with_memoria(None, Arc::clone(&memoria) as Arc<dyn MemoriaClient>);
        let sid = format!("stale-gate-{}", nanos());
        let entered = Arc::new(AtomicUsize::new(0));

        let hook_sid = sid.clone();
        let hook_entered = Arc::clone(&entered);
        let hook_stored = Arc::clone(&stored);
        let _hook_guard = install_maybe_spawn_after_gate_hook(
            &svc,
            Arc::new(move |req| {
                if req.session_id != hook_sid {
                    return;
                }
                match req.turn_number {
                    1 => {
                        hook_entered.fetch_add(1, Ordering::AcqRel);
                        let deadline = Instant::now() + Duration::from_millis(100);
                        while hook_entered.load(Ordering::Acquire) < 2 && Instant::now() < deadline
                        {
                            std::thread::yield_now();
                        }
                    }
                    2 => {
                        hook_entered.fetch_add(1, Ordering::AcqRel);
                        let deadline = Instant::now() + Duration::from_secs(1);
                        while hook_stored.load(Ordering::Acquire) == 0 && Instant::now() < deadline
                        {
                            std::thread::sleep(Duration::from_millis(1));
                        }
                    }
                    _ => {}
                }
            }),
        );

        let mut req1 = sample_req(&sid, 50_000, false);
        req1.turn_number = 1;
        let mut req2 = sample_req(&sid, 50_000, false);
        req2.turn_number = 2;

        let svc1 = Arc::clone(&svc);
        let first = tokio::spawn(async move { svc1.maybe_spawn(req1) });
        let deadline = Instant::now() + Duration::from_secs(1);
        while entered.load(Ordering::Acquire) == 0 && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        assert_eq!(
            entered.load(Ordering::Acquire),
            1,
            "first call must reach the test-controlled gate window"
        );

        let svc2 = Arc::clone(&svc);
        let second = tokio::spawn(async move { svc2.maybe_spawn(req2) });

        let d1 = first.await.unwrap();
        let d2 = second.await.unwrap();

        assert_eq!(d1, SpawnDecision::Spawned);
        assert_eq!(
            d2,
            SpawnDecision::Skipped,
            "second call must re-check updated debounce state instead of using a stale Run decision"
        );

        svc.wait_for_pending(Duration::from_secs(2)).await;
        assert_eq!(
            stored.load(Ordering::Acquire),
            1,
            "only the first extraction should store for the unchanged counters"
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

    #[tokio::test]
    async fn half_open_probe_not_consumed_when_spawn_is_skipped_in_flight() {
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
        {
            let mut set = svc.in_flight.lock().unwrap();
            set.insert(skipped_sid.clone());
        }
        assert_eq!(
            svc.maybe_spawn(sample_req(&skipped_sid, 20_000, false)),
            SpawnDecision::Skipped,
            "in-flight skip must not consume the half-open Memoria probe"
        );
        {
            let mut set = svc.in_flight.lock().unwrap();
            set.remove(&skipped_sid);
        }

        let next_sid = format!("next-probe-{}", nanos());
        assert_eq!(
            svc.maybe_spawn(sample_req(&next_sid, 20_000, false)),
            SpawnDecision::Spawned,
            "the next eligible request must still be allowed to probe"
        );
        svc.wait_for_pending(Duration::from_secs(2)).await;
    }

    #[tokio::test]
    async fn half_open_in_flight_skip_cancels_probe_after_state_lock_is_released() {
        let (svc, _rx, _broker) = build_breaker_ctx();

        for i in 0..2 {
            let sid = format!("trip-deferred-{i}-{}", nanos());
            assert_eq!(
                svc.maybe_spawn(sample_req(&sid, 20_000, false)),
                SpawnDecision::Spawned
            );
            svc.wait_for_pending(Duration::from_secs(2)).await;
        }

        tokio::time::sleep(Duration::from_millis(120)).await;

        let skipped_sid = format!("busy-probe-deferred-{}", nanos());
        {
            let mut set = svc.in_flight.lock().unwrap();
            set.insert(skipped_sid.clone());
        }

        let observed_svc = Arc::clone(&svc);
        let _guard = install_probe_cancel_hook(
            &svc,
            Arc::new(move || {
                assert!(
                    observed_svc.session_states.try_lock().is_ok(),
                    "probe cancellation must happen after releasing session_states"
                );
            }),
        );

        assert_eq!(
            svc.maybe_spawn(sample_req(&skipped_sid, 20_000, false)),
            SpawnDecision::Skipped
        );
    }

    /// Regression for the probe-slot leak on the `selector_cooldown`
    /// early-return inside `run_one`: if `admit()` returned
    /// `HalfOpenProbe` (so `probe_in_flight = true`) and the worker
    /// then bails because the LLM selector is cooling down, the
    /// breaker must still release its probe slot so the next
    /// eligible caller can perform the real recovery probe.
    ///
    /// Before the `ProbeGuard` RAII fix, the worker called only
    /// `release_in_flight` on that branch — `probe_in_flight` stayed
    /// true forever, `admit()` returned `Open` until process
    /// restart.
    #[tokio::test]
    async fn half_open_probe_released_when_run_one_bails_on_selector_cooldown() {
        // A successful-Memoria client so the trip below is exclusively
        // driven by `record_failure` we call directly (no network in
        // the test).
        let memoria = Arc::new(CapturingMemoria::default());
        let selector_params = LlmConnParams {
            base_url: "https://nope.invalid".to_string(),
            api_key: "k".to_string(),
            model_name: "cheap-selector-leak".to_string(),
            provider: "test".to_string(),
        };
        let (ingestion, _rx) = IngestionSender::for_tests(256);
        let broker = Arc::new(BackgroundActivityBroker::new());
        let mut svc = MemoryExtractionService::new(
            Arc::new(ConstSelectorResolver(Some(selector_params.clone()))),
            Arc::clone(&memoria) as Arc<dyn MemoriaClient>,
            ingestion,
            "probe-leak-test",
            Arc::clone(&broker),
        );
        // Low threshold + fast cooldown so we can reach HalfOpenProbe
        // in-test without sleeping seconds.
        svc.memoria_health = Arc::new(crate::session_memory::health::MemoriaHealth::with_config(
            1,
            Duration::from_millis(50),
        ));
        let svc = Arc::new(svc);

        // 1. Trip the breaker by marking one failure directly.
        svc.memoria_health.record_failure();
        assert!(
            svc.memoria_health.state().tripped,
            "fixture pre-condition: breaker must be tripped"
        );

        // 2. Mark the selector unhealthy so `run_one` will hit the
        //    `selector_cooldown` branch.
        svc.health.mark_failed(&selector_params.model_name);

        // 3. Wait past cooldown so the next `admit()` returns
        //    HalfOpenProbe.
        tokio::time::sleep(Duration::from_millis(70)).await;

        // 4. Fire maybe_spawn. The sync path admits (HalfOpenProbe),
        //    spawns run_one; run_one hits selector_cooldown early
        //    return. ProbeGuard::drop must release the probe slot.
        let sid = format!("probe-leak-{}", nanos());
        let req = sample_req(&sid, 20_000, false);
        assert_eq!(svc.maybe_spawn(req), SpawnDecision::Spawned);
        svc.wait_for_pending(Duration::from_secs(2)).await;

        // 5. Assertion: the breaker must be back to admitting a
        //    probe — the slot was released, not consumed.
        let admit_after = svc.memoria_health.admit();
        assert_eq!(
            admit_after,
            crate::session_memory::health::MemoriaAdmit::HalfOpenProbe,
            "probe slot must be released after selector_cooldown early-return; got {admit_after:?}"
        );
    }

    /// Concurrency regression: N parallel `maybe_spawn` calls on the
    /// same session while the breaker is in `HalfOpenProbe` must
    /// release the probe slot exactly once. The serial
    /// `half_open_in_flight_skip_cancels_probe_after_state_lock_is_released`
    /// test only proves lock-order; it does not prove the
    /// cancel-path survives racing claimants. This test does.
    ///
    /// Invariants:
    ///   * exactly one caller wins the in-flight claim and returns
    ///     `Spawned` (consuming the probe slot);
    ///   * every other caller returns `Skipped`;
    ///   * none of the losers records `record_probe_cancelled`
    ///     (because the slot was already consumed by the winner —
    ///     `memoria_admit` for losers must be evaluated AFTER the
    ///     winner already claimed, but our code currently evaluates
    ///     `admit()` while holding `session_states`, so each caller
    ///     sees its own `admit()` result; the losers that saw
    ///     `HalfOpenProbe` must call `record_probe_cancelled`).
    ///
    /// The assertion we CAN make without deep-wiring the breaker: the
    /// post-run breaker state must be self-consistent — either one
    /// probe succeeded or was cancelled, never "lost" (i.e.
    /// `probe_in_flight=true` with no worker running).
    #[tokio::test]
    async fn concurrent_maybe_spawn_on_half_open_probe_never_strands_slot() {
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
        // Wait past cooldown so admit() returns HalfOpenProbe.
        tokio::time::sleep(Duration::from_millis(120)).await;

        // Fire N parallel maybe_spawn on the SAME sid. The in-flight
        // set serialises to one winner; the rest hit the probe-cancel
        // path.
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
        let mut skipped = 0usize;
        for h in handles {
            match h.await.unwrap() {
                SpawnDecision::Spawned => spawned += 1,
                SpawnDecision::Skipped => skipped += 1,
            }
        }
        assert_eq!(
            spawned, 1,
            "exactly one racer must win the in-flight claim; got {spawned} spawned"
        );
        assert_eq!(skipped, 7, "the rest must skip; got {skipped} skipped");

        svc.wait_for_pending(Duration::from_secs(3)).await;

        // The breaker must be in a consistent state: no probe
        // stranded in-flight. Equivalent: a fresh admit() must
        // terminate (Open/Closed/HalfOpenProbe) rather than hanging
        // on a never-released probe. We assert the stronger
        // property: after cooldown elapses the next call must once
        // again be able to probe (i.e. `probe_in_flight=false`).
        tokio::time::sleep(Duration::from_millis(120)).await;
        let admit_after = svc.memoria_health.admit();
        assert!(
            matches!(
                admit_after,
                crate::session_memory::health::MemoriaAdmit::HalfOpenProbe
                    | crate::session_memory::health::MemoriaAdmit::Closed
            ),
            "breaker must not be stranded after the race; got {admit_after:?}"
        );
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

    // ── Observatory wiring tests (unhappy first) ─────────────────────

    /// Build a context with a wired observatory so each terminal path
    /// can be asserted end-to-end.
    fn build_ctx_with_obs() -> (
        TestCtx,
        Arc<crate::session_memory::SessionMemoryObservatory>,
    ) {
        let (ingestion, rx) = IngestionSender::for_tests(256);
        let broker = Arc::new(BackgroundActivityBroker::new());
        let memoria = Arc::new(CapturingMemoria::default());
        let obs = Arc::new(crate::session_memory::SessionMemoryObservatory::new());
        let svc = Arc::new(
            MemoryExtractionService::new(
                Arc::new(ConstSelectorResolver(None)),
                Arc::clone(&memoria) as Arc<dyn MemoriaClient>,
                ingestion,
                "test-user",
                Arc::clone(&broker),
            )
            .with_observatory(Arc::clone(&obs)),
        );
        (
            TestCtx {
                svc,
                rx,
                broker,
                memoria,
            },
            obs,
        )
    }

    #[tokio::test]
    async fn observatory_records_skip_below_init_gate() {
        let (ctx, obs) = build_ctx_with_obs();
        let req = sample_req("obs-skip", 1_000, false); // below 10K init gate
        assert_eq!(ctx.svc.maybe_spawn(req), SpawnDecision::Skipped);

        let snap = obs.extractions_snapshot();
        assert_eq!(snap.len(), 1);
        let rec = &snap[0];
        assert_eq!(rec.session_id, "obs-skip");
        assert!(matches!(
            rec.outcome,
            crate::session_memory::ExtractionOutcome::Skipped { ref reason } if reason == "below_init_gate"
        ));
        assert_eq!(
            rec.trigger,
            crate::session_memory::ExtractionTrigger::InitGate
        );
        assert!(
            rec.content_preview.is_empty(),
            "skip record has no persisted content"
        );
    }

    #[tokio::test]
    async fn observatory_records_error_override_when_had_error_below_init() {
        // First-turn error below the init gate MUST extract. Before
        // the gate fix (commit for #42) the init gate swallowed all
        // first-turn failures — the most diagnostically valuable
        // sessions never got captured. Now had_error always triggers
        // Run regardless of tokens, and the observatory tags the
        // record as ErrorOverride.
        let (ctx, obs) = build_ctx_with_obs();
        let sid = format!("obs-err-{}", nanos());
        let req = sample_req(&sid, 1_000, true);
        assert_eq!(
            ctx.svc.maybe_spawn(req),
            SpawnDecision::Spawned,
            "below-init error must bypass init gate"
        );
        ctx.svc.wait_for_pending(Duration::from_secs(2)).await;

        let snap = obs.extractions_snapshot();
        assert_eq!(snap.len(), 1);
        assert_eq!(
            snap[0].trigger,
            crate::session_memory::ExtractionTrigger::ErrorOverride,
            "trigger must reflect the error-driven bypass"
        );
    }

    #[tokio::test]
    async fn observatory_records_persisted_after_rule_based_store() {
        let (ctx, obs) = build_ctx_with_obs();
        let sid = format!("obs-ok-{}", nanos());
        let req = sample_req(&sid, 50_000, false); // past init gate, no selector
        assert_eq!(ctx.svc.maybe_spawn(req), SpawnDecision::Spawned);
        wait_for_memoria_store(&ctx.memoria).await;

        // Wait for the async record — observatory writes after the
        // persist returns, which can trail the store Mutex push by a
        // scheduler tick on CI hosts.
        for _ in 0..50 {
            if !obs.extractions_snapshot().is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let snap = obs.extractions_snapshot();
        assert_eq!(snap.len(), 1, "expected one record");
        let rec = &snap[0];
        match &rec.outcome {
            crate::session_memory::ExtractionOutcome::Persisted {
                source,
                bytes_written,
                store_attempt,
            } => {
                assert_eq!(
                    *source,
                    crate::session_memory::ExtractionSource::RuleFallback
                );
                assert!(*bytes_written > 0);
                assert!(*store_attempt >= 1);
            }
            other => panic!("expected Persisted, got {other:?}"),
        }
        assert!(
            !rec.content_preview.is_empty(),
            "persisted record must carry a preview"
        );
        assert!(
            rec.content_preview
                .starts_with(crate::turn::cloud::session_memory_protocol::SESSION_MEMORY_PREFIX),
            "preview must include the v1 prefix; got {:.40}…",
            rec.content_preview
        );
    }

    #[tokio::test]
    async fn observatory_is_silent_when_not_attached() {
        // Default service has no observatory; verifies the `None` path
        // is truly a no-op — no panic, no behaviour change.
        let mut ctx = build_ctx(None);
        let req = sample_req("no-obs", 1_000, false);
        assert_eq!(ctx.svc.maybe_spawn(req), SpawnDecision::Skipped);
        // Service has no observatory — nothing to check beyond the
        // event stream still landing normally.
        let events = collect_extraction_events(&mut ctx.rx);
        assert_eq!(events.len(), 1);
    }

    #[tokio::test]
    async fn observatory_skips_record_on_empty_session_id() {
        // Sanity: NoSessionId + empty session_id must not create a
        // phantom record with "" as the key. `record_skipped` guards
        // with `Some(sid)`.
        let (ctx, obs) = build_ctx_with_obs();
        let req = sample_req("", 50_000, false);
        assert_eq!(ctx.svc.maybe_spawn(req), SpawnDecision::Skipped);
        let snap = obs.extractions_snapshot();
        assert!(
            snap.is_empty(),
            "no session id means the record must be suppressed, not saved with empty id; got: {snap:?}",
        );
    }
}
