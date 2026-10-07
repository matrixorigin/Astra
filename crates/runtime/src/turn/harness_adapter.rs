//! Bridge between the agentic loop and the harness kernel.
//!
//! When the `harness` feature is disabled, all types and macros in this module
//! compile to zero-cost stubs (ZST + empty macro expansion).

// ─── Feature-gated implementation ───────────────────────────────────────────

#[cfg(feature = "harness")]
pub use enabled::*;

#[cfg(not(feature = "harness"))]
pub use disabled::*;

// ─── Enabled path ───────────────────────────────────────────────────────────

#[cfg(feature = "harness")]
mod enabled {
    use astra_harness::{
        DecisionRecord, HarnessKernel, HookPoint, HookVerdict, RuntimeSnapshot, SnapshotSink,
    };
    use std::sync::Arc;
    use std::time::{SystemTime, UNIX_EPOCH};

    use crate::turn::agentic_loop::host::AgenticLoopState;

    pub struct HarnessSlot {
        pub kernel: Option<Arc<dyn HarnessKernel>>,
        pub sink: Option<Arc<dyn SnapshotSink>>,
        pub(crate) session_start_unix_millis: u64,
        session_ended: std::sync::atomic::AtomicBool,
        /// Registry reference for cleanup on session end (prevents resource leak).
        pub(crate) registry: Option<crate::server::harness::handlers::HarnessSinkRegistry>,
        /// Session ID used to unregister from the registry on cleanup.
        pub(crate) session_id_for_cleanup: Option<String>,
        /// Concrete server sink reference for deferred user_id injection.
        pub(crate) server_sink:
            Option<Arc<crate::server::harness::server_sink::ServerSnapshotSink>>,
    }

    impl HarnessSlot {
        pub fn empty() -> Self {
            Self {
                kernel: None,
                sink: None,
                session_start_unix_millis: now_millis(),
                session_ended: std::sync::atomic::AtomicBool::new(false),
                registry: None,
                session_id_for_cleanup: None,
                server_sink: None,
            }
        }

        pub fn new(kernel: Arc<dyn HarnessKernel>, sink: Arc<dyn SnapshotSink>) -> Self {
            Self {
                kernel: Some(kernel),
                sink: Some(sink),
                session_start_unix_millis: now_millis(),
                session_ended: std::sync::atomic::AtomicBool::new(false),
                registry: None,
                session_id_for_cleanup: None,
                server_sink: None,
            }
        }

        /// Create an observe-only slot that writes to the parent's sink
        /// but has no kernel (no verifier enforcement in sub-runs).
        /// Sub-run snapshots appear in the parent's history.
        pub fn observe_only(sink: Arc<dyn SnapshotSink>) -> Self {
            Self {
                kernel: None,
                sink: Some(sink),
                session_start_unix_millis: now_millis(),
                session_ended: std::sync::atomic::AtomicBool::new(false),
                registry: None,
                session_id_for_cleanup: None,
                server_sink: None,
            }
        }

        /// Set the user_id on the server sink (deferred injection).
        /// Called by the run lifecycle after `build_initial_state` when
        /// the user_id becomes available.
        pub fn set_user_id(&self, user_id: &str) {
            if let Some(ref sink) = self.server_sink {
                sink.set_user_id(user_id.to_string());
            }
        }
    }

    impl Drop for HarnessSlot {
        fn drop(&mut self) {
            if let (Some(registry), Some(sid)) =
                (self.registry.take(), self.session_id_for_cleanup.take())
            {
                registry.unregister(&sid);
            }
        }
    }

    pub(crate) fn now_millis() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64
    }

    #[cfg(test)]
    pub(crate) fn capture_snapshot(
        state: &AgenticLoopState,
        session_start_unix_millis: u64,
    ) -> RuntimeSnapshot {
        capture_snapshot_at(state, session_start_unix_millis, now_millis())
    }

    /// Borrowed observation facts; this does not grant execution authority.
    pub struct HarnessSnapshotInput<'a> {
        pub session_id: Option<&'a str>,
        pub round_index: u32,
        pub turns_used: u32,
        pub turns_limit: Option<u32>,
        pub settlement_rounds_reserved: Option<u32>,
        pub session_turn: u32,
        pub prompt_tokens: u64,
        pub completion_tokens: u64,
        pub cache_read_tokens: u64,
        pub cache_creation_tokens: u64,
        pub input_budget_tokens: u64,
        pub measured_prompt_tokens: Option<u64>,
        pub message_count: usize,
        pub tool_calls: u32,
        pub tools_used: &'a std::collections::HashSet<String>,
        pub tool_signatures:
            &'a [std::collections::BTreeSet<astra_turn_core::stall::StallSignature>],
        pub final_text: &'a str,
        pub interruption: Option<&'a astra_turn_core::interruption::InterruptionRecord>,
        pub tool_records: &'a [astra_services::session_journal::ToolCallRecord],
        pub read_only_round_streak: usize,
        pub delegations: u32,
        pub recursion_depth: u8,
        pub consecutive_errors: u32,
        pub causal_chain_id: Option<&'a str>,
    }

    fn snapshot_input(state: &AgenticLoopState) -> HarnessSnapshotInput<'_> {
        // Bounded text-only settlement may reserve up to two extra rounds.
        let settlement_rounds_reserved = if state.hooks.completion_settlement.text_only
            || state.hooks.completion_settlement.work_settlement_only
        {
            state
                .agentic_turn_budget
                .hard_turn_limit
                .map(|limit| state.max_turns.saturating_sub(limit.get()).min(2) as u32)
        } else {
            Some(0)
        };
        HarnessSnapshotInput {
            session_id: state.current_session_id.as_deref(),
            round_index: state.current_round_index,
            turns_used: state.current_round_index + 1,
            turns_limit: (state.max_turns > 0).then_some(state.max_turns as u32),
            settlement_rounds_reserved,
            session_turn: state.session_turn,
            prompt_tokens: state.total_prompt,
            completion_tokens: state.total_completion,
            cache_read_tokens: state.total_cache_read,
            cache_creation_tokens: state.total_cache_creation,
            input_budget_tokens: state.max_turn_input_tokens,
            measured_prompt_tokens: state.last_measured_prompt_tokens,
            message_count: state.messages.len(),
            tool_calls: state.total_tool_calls,
            tools_used: &state.telemetry.all_tools_used,
            tool_signatures: &state.turn_guard.tool_sigs,
            final_text: &state.final_text,
            interruption: state.interruption.as_ref(),
            tool_records: &state.stall.tool_call_records,
            read_only_round_streak: state.stall.circuit_breaker.consecutive_read_only(),
            delegations: state.delegations_this_turn,
            recursion_depth: state.recursion_depth,
            consecutive_errors: state.error_recovery.consecutive_same_error,
            causal_chain_id: state.canonical_turn_chain_id.as_deref(),
        }
    }

    #[cfg(test)]
    pub(crate) fn capture_snapshot_at(
        state: &AgenticLoopState,
        session_start_unix_millis: u64,
        now: u64,
    ) -> RuntimeSnapshot {
        capture_turn_snapshot_at(snapshot_input(state), session_start_unix_millis, now)
    }

    fn capture_turn_snapshot_at(
        input: HarnessSnapshotInput<'_>,
        session_start_unix_millis: u64,
        now: u64,
    ) -> RuntimeSnapshot {
        let session_id = input.session_id.unwrap_or_default().to_owned();

        let turns_used = input.turns_used;
        let turns_limit = input.turns_limit;
        let settlement_rounds_reserved = input.settlement_rounds_reserved;

        let tokens_used_session = input.prompt_tokens
            + input.completion_tokens
            + input.cache_read_tokens
            + input.cache_creation_tokens;

        let context_budget_tokens = if input.input_budget_tokens > 0 {
            Some(input.input_budget_tokens as u32)
        } else {
            None
        };

        let context_total_tokens = input.measured_prompt_tokens.map(|t| t as u32);

        let context_utilization = match (context_total_tokens, context_budget_tokens) {
            (Some(total), Some(budget)) if budget > 0 => Some(total as f32 / budget as f32),
            _ => None,
        };

        let mut unique_tools: Vec<String> = input.tools_used.iter().cloned().collect();
        unique_tools.sort();

        let last_tool_called = input.tool_signatures.last().and_then(|sigs| {
            sigs.iter()
                .last()
                .map(|signature| signature.tool_name().to_owned())
        });

        let consecutive_same_tool = compute_consecutive_same_tool(input.tool_signatures);
        let has_final_text = !input.final_text.trim().is_empty();
        let interruption_kind = input
            .interruption
            .map(|interruption| interruption.kind.label().to_string());
        let final_state = Some(classify_final_state(
            has_final_text,
            interruption_kind.as_deref(),
        ));
        let last_tool_result_class = input
            .tool_records
            .iter()
            .rev()
            .find_map(|record| record.result_class.clone());
        let read_only_round_streak = input.read_only_round_streak.min(u32::MAX as usize) as u32;
        let redundant_read_count =
            astra_turn_core::evaluation::count_redundant_overlapping_reads(input.tool_records)
                .min(u32::MAX as usize) as u32;

        let elapsed = now.saturating_sub(session_start_unix_millis);

        RuntimeSnapshot {
            session_id,
            turn_number: input.round_index,
            model: None,
            context_total_tokens,
            context_budget_tokens,
            context_message_count: input.message_count as u32,
            context_system_prompt_tokens: None,
            context_utilization,
            turns_used,
            turns_limit,
            settlement_rounds_reserved,
            session_turn: input.session_turn,
            tokens_used_session,
            tokens_prompt: input.prompt_tokens,
            tokens_completion: input.completion_tokens,
            tokens_cache_read: input.cache_read_tokens,
            tokens_cache_creation: input.cache_creation_tokens,
            elapsed_millis: elapsed,
            tool_calls_this_session: input.tool_calls,
            unique_tools_used: unique_tools,
            last_tool_called,
            consecutive_same_tool,
            final_state,
            interruption_kind,
            has_final_text,
            last_tool_result_class,
            read_only_round_streak,
            redundant_read_count,
            delegations_this_turn: input.delegations,
            recursion_depth: input.recursion_depth,
            consecutive_errors: input.consecutive_errors,
            captured_at_unix_millis: now,
            session_start_unix_millis,
            causal_chain_id: input.causal_chain_id.map(str::to_owned),
            schema_version: 3,
        }
    }

    pub(crate) fn classify_final_state(
        has_final_text: bool,
        interruption_kind: Option<&str>,
    ) -> String {
        if interruption_kind.is_some() {
            "interrupted".to_string()
        } else if has_final_text {
            "completed".to_string()
        } else {
            "empty".to_string()
        }
    }

    pub(crate) fn compute_consecutive_same_tool(
        sigs: &[std::collections::BTreeSet<astra_turn_core::stall::StallSignature>],
    ) -> u32 {
        let count = astra_turn_core::stall::trailing_identical_sig_depth(sigs);
        // Snapshot convention: a single occurrence is not repetition.
        if count > 1 {
            u32::try_from(count).unwrap_or(u32::MAX)
        } else {
            0
        }
    }

    /// Execute harness hook. Returns `HookVerdict`.
    /// When kernel is None but sink is Some (observe-only), still captures snapshot.
    pub(crate) fn harness_fire(
        slot: &HarnessSlot,
        point: HookPoint,
        state: &AgenticLoopState,
    ) -> HookVerdict {
        slot.fire(point, snapshot_input(state))
    }

    impl HarnessSlot {
        /// Observe or enforce a hook from the same shared snapshot projection.
        /// The slot owns terminal deduplication across normal and error exits.
        pub fn fire(&self, point: HookPoint, input: HarnessSnapshotInput<'_>) -> HookVerdict {
            if point == HookPoint::SessionEnd
                && self
                    .session_ended
                    .swap(true, std::sync::atomic::Ordering::AcqRel)
            {
                return HookVerdict::Continue;
            }
            if self.kernel.is_none() && self.sink.is_none() {
                return HookVerdict::Continue;
            }
            let now = now_millis();
            let snapshot = capture_turn_snapshot_at(input, self.session_start_unix_millis, now);
            let record = DecisionRecord {
                session_id: snapshot.session_id.clone(),
                turn: snapshot.turn_number,
                point,
                wall_time_unix_millis: now,
                monotonic_millis_since_session: now.saturating_sub(self.session_start_unix_millis),
                snapshot,
            };
            if let Some(ref kernel) = self.kernel {
                kernel.on_record(&record)
            } else if let Some(ref sink) = self.sink {
                sink.update(&record);
                HookVerdict::Continue
            } else {
                HookVerdict::Continue
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use crate::turn::agentic_loop::host::tests::make_state;
        use astra_harness::{InMemorySnapshotSink, StandardKernel, verifiers::BudgetVerifier};

        #[test]
        fn capture_snapshot_from_state() {
            let state = make_state();
            let snap = capture_snapshot(&state, 1_000_000);
            assert_eq!(snap.turn_number, 0);
            assert_eq!(snap.tool_calls_this_session, 0);
            assert!(snap.unique_tools_used.is_empty());
            assert_eq!(snap.session_start_unix_millis, 1_000_000);
            assert_eq!(snap.final_state.as_deref(), Some("empty"));
            assert!(!snap.has_final_text);
            assert_eq!(snap.schema_version, 3);
        }

        #[test]
        fn classify_final_state_prefers_interruption() {
            assert_eq!(classify_final_state(true, None), "completed");
            assert_eq!(classify_final_state(false, None), "empty");
            assert_eq!(
                classify_final_state(true, Some("budget_exhausted")),
                "interrupted"
            );
        }

        #[test]
        fn harness_fire_continue_with_no_kernel() {
            let state = make_state();
            let slot = HarnessSlot::empty();
            let verdict = harness_fire(&slot, HookPoint::SessionStart, &state);
            assert!(matches!(verdict, HookVerdict::Continue));
        }

        #[test]
        fn harness_fire_with_kernel_and_budget_verifier() {
            let sink = InMemorySnapshotSink::arc();
            let verifier = BudgetVerifier {
                max_turns: Some(5),
                max_tokens: None,
                max_duration_millis: None,
            };
            let kernel = Arc::new(StandardKernel::new(
                sink.clone() as Arc<dyn astra_harness::SnapshotSink>,
                vec![Box::new(verifier)],
            ));
            let slot = HarnessSlot::new(
                kernel as Arc<dyn astra_harness::HarnessKernel>,
                sink as Arc<dyn astra_harness::SnapshotSink>,
            );
            let state = make_state();

            let verdict = harness_fire(&slot, HookPoint::PostTurn, &state);
            assert!(matches!(verdict, HookVerdict::Continue));

            assert!(slot.sink.as_ref().unwrap().latest().is_some());
        }

        #[test]
        fn consecutive_same_tool_computation() {
            use std::collections::BTreeSet;

            assert_eq!(compute_consecutive_same_tool(&[]), 0);
            assert_eq!(
                compute_consecutive_same_tool(&[BTreeSet::new(), BTreeSet::new()]),
                0
            );

            let a = BTreeSet::from([astra_turn_core::stall::StallSignature::new("bash", b"")]);
            assert_eq!(
                compute_consecutive_same_tool(&[a.clone(), a.clone(), BTreeSet::new()]),
                0
            );
            assert_eq!(
                compute_consecutive_same_tool(&[a.clone(), BTreeSet::new(), a.clone()]),
                0
            );
            assert_eq!(compute_consecutive_same_tool(std::slice::from_ref(&a)), 0);
            assert_eq!(compute_consecutive_same_tool(&[a.clone(), a.clone()]), 2);
            assert_eq!(
                compute_consecutive_same_tool(&[a.clone(), a.clone(), a.clone()]),
                3
            );

            let b = BTreeSet::from([astra_turn_core::stall::StallSignature::new(
                "read_file",
                b"",
            )]);
            assert_eq!(
                compute_consecutive_same_tool(&[a.clone(), a.clone(), b.clone(), a.clone()]),
                0
            );
            assert_eq!(
                compute_consecutive_same_tool(&[a.clone(), b.clone(), b.clone()]),
                2
            );
        }

        // ── Snapshot accuracy tests (Issue #8) ──────────────────────────

        #[test]
        fn capture_snapshot_token_sum_matches_state() {
            let mut state = make_state();
            state.total_prompt = 1000;
            state.total_completion = 500;
            state.total_cache_read = 200;
            state.total_cache_creation = 100;

            let snap = capture_snapshot(&state, 0);
            assert_eq!(snap.tokens_used_session, 1800);
        }

        #[test]
        fn capture_snapshot_context_utilization() {
            let mut state = make_state();
            state.last_measured_prompt_tokens = Some(80_000);
            state.max_turn_input_tokens = 200_000;

            let snap = capture_snapshot(&state, 0);
            let util = snap.context_utilization.unwrap();
            assert!((util - 0.4).abs() < 0.001);
        }

        #[test]
        fn capture_snapshot_turns_limit() {
            let mut state = make_state();
            state.max_turns = 25;
            state.current_round_index = 6; // inner loop round (0-based)
            state.session_turn = 3; // outer REPL turn

            let snap = capture_snapshot(&state, 0);
            assert_eq!(snap.turns_limit, Some(25));
            assert_eq!(snap.turns_used, 7); // current_round_index + 1
            assert_eq!(snap.session_turn, 3); // outer session turn
            assert_eq!(snap.settlement_rounds_reserved, Some(0));
        }

        #[test]
        fn uncapped_settlement_does_not_fabricate_reserved_round_count() {
            let mut state = make_state();
            state.agentic_turn_budget.hard_turn_limit = None;
            state.max_turns = 51;
            state.hooks.completion_settlement.text_only = true;
            assert_eq!(capture_snapshot(&state, 0).settlement_rounds_reserved, None);
            state.hooks.completion_settlement.text_only = false;
            assert_eq!(
                capture_snapshot(&state, 0).settlement_rounds_reserved,
                Some(0)
            );
        }

        #[test]
        fn capture_snapshot_reports_runtime_settlement_boundary() {
            let mut state = make_state();
            state.agentic_turn_budget.hard_turn_limit = std::num::NonZeroUsize::new(10);
            state.max_turns = 11;
            state.hooks.completion_settlement.text_only = true;

            let snap = capture_snapshot(&state, 0);
            assert_eq!(snap.settlement_rounds_reserved, Some(1));

            state.max_turns = 12;
            let snap = capture_snapshot(&state, 0);
            assert_eq!(snap.settlement_rounds_reserved, Some(2));

            state.max_turns = 13;
            let snap = capture_snapshot(&state, 0);
            assert_eq!(snap.settlement_rounds_reserved, Some(2));

            state.hooks.completion_settlement.text_only = false;
            let snap = capture_snapshot(&state, 0);
            assert_eq!(snap.settlement_rounds_reserved, Some(0));
        }

        #[test]
        fn capture_snapshot_delegation_and_error_fields() {
            let mut state = make_state();
            state.delegations_this_turn = 3;
            state.recursion_depth = 2;
            state.error_recovery.consecutive_same_error = 4;

            let snap = capture_snapshot(&state, 0);
            assert_eq!(snap.delegations_this_turn, 3);
            assert_eq!(snap.recursion_depth, 2);
            assert_eq!(snap.consecutive_errors, 4);
        }

        #[test]
        fn capture_snapshot_unique_tools_sorted() {
            let mut state = make_state();
            state.telemetry.all_tools_used = ["bash", "read_file", "edit_file"]
                .iter()
                .map(|s| s.to_string())
                .collect();

            let snap = capture_snapshot(&state, 0);
            assert_eq!(
                snap.unique_tools_used,
                vec!["bash", "edit_file", "read_file"]
            );
        }

        #[test]
        fn capture_snapshot_no_budget_means_none() {
            let mut state = make_state();
            state.max_turns = 0;
            state.max_turn_input_tokens = 0;

            let snap = capture_snapshot(&state, 0);
            assert_eq!(snap.turns_limit, None);
            assert_eq!(snap.context_budget_tokens, None);
            assert_eq!(snap.context_utilization, None);
        }

        #[test]
        fn observe_only_slot_writes_to_sink() {
            let sink = InMemorySnapshotSink::arc();
            let slot =
                HarnessSlot::observe_only(sink.clone() as Arc<dyn astra_harness::SnapshotSink>);
            assert!(slot.kernel.is_none());
            let state = make_state();
            let verdict = harness_fire(&slot, HookPoint::PostTurn, &state);
            assert!(matches!(verdict, HookVerdict::Continue));
            assert!(sink.latest().is_some(), "observe_only must write to sink");
        }

        #[test]
        fn normal_and_error_exits_publish_one_terminal_snapshot() {
            for enforce in [false, true] {
                let sink = InMemorySnapshotSink::arc();
                let slot = if enforce {
                    HarnessSlot::new(
                        Arc::new(StandardKernel::new(sink.clone(), Vec::new())),
                        sink.clone(),
                    )
                } else {
                    HarnessSlot::observe_only(sink.clone())
                };
                let mut state = make_state();
                slot.fire(HookPoint::SessionStart, snapshot_input(&state));
                state.final_text = "Delivered answer".into();
                slot.fire(HookPoint::SessionEnd, snapshot_input(&state));
                // Error cleanup must not replace a settled snapshot or run
                // the terminal verifiers a second time.
                state.final_text.clear();
                state.error_recovery.consecutive_same_error = 1;
                slot.fire(HookPoint::SessionEnd, snapshot_input(&state));
                let history = sink.history(10);
                assert_eq!(history.len(), 2);
                assert_eq!(history[0].final_state.as_deref(), Some("completed"));
                assert_eq!(history[0].consecutive_errors, 0);
            }
        }
    }
}

// ─── Disabled path (zero cost) ──────────────────────────────────────────────

#[cfg(not(feature = "harness"))]
mod disabled {
    pub struct HarnessSlot;

    impl HarnessSlot {
        pub fn empty() -> Self {
            Self
        }
    }
}

// ─── harness_at! macro ──────────────────────────────────────────────────────

#[cfg(feature = "harness")]
macro_rules! harness_at {
    ($slot:expr, $point:expr, $state:expr) => {{ $crate::turn::harness_adapter::harness_fire($slot, $point, $state) }};
}

#[cfg(not(feature = "harness"))]
macro_rules! harness_at {
    ($slot:expr, $point:expr, $state:expr) => {{}};
}

pub(crate) use harness_at;
