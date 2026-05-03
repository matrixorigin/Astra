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

    use crate::turn::agentic_loop_host::AgenticLoopState;

    pub struct HarnessSlot {
        pub kernel: Option<Arc<dyn HarnessKernel>>,
        pub sink: Option<Arc<dyn SnapshotSink>>,
        pub(crate) session_start_unix_millis: u64,
        pub(crate) session_ended: bool,
        /// Registry reference for cleanup on session end (prevents resource leak).
        pub(crate) registry: Option<crate::server::harness_handlers::HarnessSinkRegistry>,
        /// Session ID used to unregister from the registry on cleanup.
        pub(crate) session_id_for_cleanup: Option<String>,
        /// Concrete server sink reference for deferred user_id injection.
        pub(crate) server_sink: Option<Arc<crate::server::harness_server_sink::ServerSnapshotSink>>,
    }

    impl HarnessSlot {
        pub fn empty() -> Self {
            Self {
                kernel: None,
                sink: None,
                session_start_unix_millis: now_millis(),
                session_ended: false,
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
                session_ended: false,
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
                session_ended: false,
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

    pub(crate) fn capture_snapshot_at(
        state: &AgenticLoopState,
        session_start_unix_millis: u64,
        now: u64,
    ) -> RuntimeSnapshot {
        let session_id = state.current_session_id.clone().unwrap_or_default();

        let turns_used = state.current_round_index + 1;
        let turns_limit = if state.max_turns > 0 {
            Some(state.max_turns as u32)
        } else {
            None
        };

        let tokens_used_session = state.total_prompt
            + state.total_completion
            + state.total_cache_read
            + state.total_cache_creation;

        let context_budget_tokens = if state.max_turn_input_tokens > 0 {
            Some(state.max_turn_input_tokens as u32)
        } else {
            None
        };

        let context_total_tokens = state.last_measured_prompt_tokens.map(|t| t as u32);

        let context_utilization = match (context_total_tokens, context_budget_tokens) {
            (Some(total), Some(budget)) if budget > 0 => Some(total as f32 / budget as f32),
            _ => None,
        };

        let mut unique_tools: Vec<String> =
            state.telemetry.all_tools_used.iter().cloned().collect();
        unique_tools.sort();

        let last_tool_called = state
            .turn_guard
            .tool_sigs
            .last()
            .and_then(|sigs| sigs.iter().last().cloned());

        let consecutive_same_tool = compute_consecutive_same_tool(&state.turn_guard.tool_sigs);

        let elapsed = now.saturating_sub(session_start_unix_millis);

        RuntimeSnapshot {
            session_id,
            turn_number: state.current_round_index,
            model: None,
            context_total_tokens,
            context_budget_tokens,
            context_message_count: state.messages.len() as u32,
            context_system_prompt_tokens: None,
            context_utilization,
            turns_used,
            turns_limit,
            session_turn: state.session_turn,
            tokens_used_session,
            tokens_prompt: state.total_prompt,
            tokens_completion: state.total_completion,
            tokens_cache_read: state.total_cache_read,
            tokens_cache_creation: state.total_cache_creation,
            elapsed_millis: elapsed,
            tool_calls_this_session: state.total_tool_calls,
            unique_tools_used: unique_tools,
            last_tool_called,
            consecutive_same_tool,
            delegations_this_turn: state.delegations_this_turn,
            recursion_depth: state.recursion_depth,
            consecutive_errors: state.error_recovery.consecutive_same_error,
            captured_at_unix_millis: now,
            session_start_unix_millis,
            causal_chain_id: state.bridge_turn_chain_id.clone(),
            schema_version: 2,
        }
    }

    pub(crate) fn compute_consecutive_same_tool(
        sigs: &[std::collections::BTreeSet<String>],
    ) -> u32 {
        if sigs.len() < 2 {
            return 0;
        }
        let last = &sigs[sigs.len() - 1];
        let mut count = 1u32;
        for prev in sigs[..sigs.len() - 1].iter().rev() {
            if prev == last {
                count += 1;
            } else {
                break;
            }
        }
        if count > 1 { count } else { 0 }
    }

    /// Execute harness hook. Returns `HookVerdict`.
    /// When kernel is None but sink is Some (observe-only), still captures snapshot.
    pub(crate) fn harness_fire(
        slot: &HarnessSlot,
        point: HookPoint,
        state: &AgenticLoopState,
    ) -> HookVerdict {
        if slot.kernel.is_none() && slot.sink.is_none() {
            return HookVerdict::Continue;
        }

        let now = now_millis();
        let elapsed = now.saturating_sub(slot.session_start_unix_millis);
        let snapshot = capture_snapshot_at(state, slot.session_start_unix_millis, now);

        let record = DecisionRecord {
            session_id: snapshot.session_id.clone(),
            turn: snapshot.turn_number,
            point,
            wall_time_unix_millis: now,
            monotonic_millis_since_session: elapsed,
            snapshot,
        };

        if let Some(ref kernel) = slot.kernel {
            kernel.on_record(&record)
        } else if let Some(ref sink) = slot.sink {
            sink.update(&record);
            HookVerdict::Continue
        } else {
            HookVerdict::Continue
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use crate::turn::agentic_loop_host::tests::make_state;
        use astra_harness::{InMemorySnapshotSink, StandardKernel, verifiers::BudgetVerifier};

        #[test]
        fn capture_snapshot_from_state() {
            let state = make_state();
            let snap = capture_snapshot(&state, 1_000_000);
            assert_eq!(snap.turn_number, 0);
            assert_eq!(snap.tool_calls_this_session, 0);
            assert!(snap.unique_tools_used.is_empty());
            assert_eq!(snap.session_start_unix_millis, 1_000_000);
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

            let a: BTreeSet<String> = ["bash".to_string()].into();
            assert_eq!(compute_consecutive_same_tool(&[a.clone()]), 0);
            assert_eq!(compute_consecutive_same_tool(&[a.clone(), a.clone()]), 2);
            assert_eq!(
                compute_consecutive_same_tool(&[a.clone(), a.clone(), a.clone()]),
                3
            );

            let b: BTreeSet<String> = ["read_file".to_string()].into();
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
