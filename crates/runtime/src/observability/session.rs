use std::collections::HashMap;
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use astra_services::session_journal::{JournalEvent, JournalWriter};
use astra_services::session_workspace::{
    ContextTraceBudgetSignal, ContextTraceHistorySignal, ContextTraceMemorySignal,
    ContextTraceSignal, ContextTraceTimingSignal, ContextTraceToolSurface,
};
use serde::{Deserialize, Serialize};

use astra_config::runtime_config::RuntimeConfig;
use astra_config::user_profile::{Scenario, UserProfile, UserProfileManager, UserProfileStore};
use astra_core::feedback::FeedbackSignal;
use astra_turn_core::context_assembly_trace::ContextAssemblyTrace;
use astra_turn_core::decision_explainer::DecisionExplanation;

use super::types::*;

fn record_observability_rollback_history(
    site: astra_core::history_work::HistoryWorkSite,
    original_query: &Option<String>,
    recent_queries: &[String],
    compressed_turns: &[u32],
    user_corrections: &[u32],
    context_traces: &[ContextAssemblyTrace],
) {
    astra_core::history_work::record_serialized_value(
        site,
        &(
            original_query,
            recent_queries,
            compressed_turns,
            user_corrections,
            context_traces,
        ),
    );
}

impl Clone for ObservabilitySessionRollbackSnapshot {
    fn clone(&self) -> Self {
        record_observability_rollback_history(
            astra_core::history_work::HistoryWorkSite::ObservabilityRollbackSnapshotClone,
            &self.original_query,
            &self.recent_queries,
            &self.compressed_turns,
            &self.user_corrections,
            &self.context_traces,
        );
        Self {
            config: self.config.clone(),
            original_query: self.original_query.clone(),
            recent_queries: self.recent_queries.clone(),
            compressed_turns: self.compressed_turns.clone(),
            user_corrections: self.user_corrections.clone(),
            context_traces: self.context_traces.clone(),
            last_query_at: self.last_query_at,
        }
    }
}

impl ObservabilitySession {
    /// Create a new session with loaded user profile.
    pub fn new(
        user_id: impl Into<String>,
        session_id: impl Into<String>,
        manager: &UserProfileManager,
    ) -> Self {
        let user_id = user_id.into();
        let profile = manager.get_profile(&user_id);

        // Load config from defaults + file hierarchy + env vars
        let mut config = RuntimeConfig::load();

        // Apply user preferences
        profile.preferences.apply_to_config(&mut config);

        Self {
            user_id,
            session_id: session_id.into(),
            turn_number: 0,
            profile,
            config,
            context_traces: Vec::new(),
            decision_explanations: Vec::new(),

            recent_queries: Vec::new(),
            compressed_turns: Vec::new(),
            user_corrections: Vec::new(),
            original_query: None,
            started_at: Instant::now(),
            last_query_at: None,
            turn_timings: Vec::new(),
            fuzzy_match_events: Vec::new(),
            last_scenario_change_turn: None,
            last_token_budget_direction: 0,
            last_token_budget_change_turn: None,
            last_strategy_application: None,
            last_guardrail_view: None,
            last_denial_pressure: None,
            recent_failing_tests: Vec::new(),
            recent_rejections: Vec::new(),
            recent_correction_excerpts: Vec::new(),
            stall_event_count: 0,
            outcome_bias: std::collections::BTreeMap::new(),
            low_confidence_tools: Vec::new(),
            active_scenario: None,
            cached_skill_names: Vec::new(),
            last_tool_health_export: Vec::new(),
            last_feedback_signals: Vec::new(),
            injection_history: astra_turn_core::injection_tracking::InjectionHistory::new(),
        }
    }

    /// Create a simple session without full profile infrastructure.
    ///
    /// This is useful for CLI contexts where we don't have access to the
    /// full UserProfileManager.
    pub fn new_simple(session_id: impl Into<String>) -> Self {
        Self {
            user_id: "anonymous".to_string(),
            session_id: session_id.into(),
            turn_number: 0,
            profile: UserProfile::new("anonymous"),
            config: RuntimeConfig::load(),
            context_traces: Vec::new(),
            decision_explanations: Vec::new(),

            recent_queries: Vec::new(),
            compressed_turns: Vec::new(),
            user_corrections: Vec::new(),
            original_query: None,
            started_at: Instant::now(),
            last_query_at: None,
            turn_timings: Vec::new(),
            fuzzy_match_events: Vec::new(),
            last_scenario_change_turn: None,
            last_token_budget_direction: 0,
            last_token_budget_change_turn: None,
            last_strategy_application: None,
            last_guardrail_view: None,
            last_denial_pressure: None,
            recent_failing_tests: Vec::new(),
            recent_rejections: Vec::new(),
            recent_correction_excerpts: Vec::new(),
            stall_event_count: 0,
            outcome_bias: std::collections::BTreeMap::new(),
            low_confidence_tools: Vec::new(),
            active_scenario: None,
            cached_skill_names: Vec::new(),
            last_tool_health_export: Vec::new(),
            last_feedback_signals: Vec::new(),
            injection_history: astra_turn_core::injection_tracking::InjectionHistory::new(),
        }
    }

    /// Get the current scenario (if detected).
    pub fn current_scenario(&self) -> Option<Scenario> {
        self.profile.current_scenario
    }

    /// Observe every tracked prompt-injection channel for the current
    /// turn with a hybrid input: raw CLI-owned text (for channels the
    /// CLI authoritatively writes into `edge_profile`, so introspect
    /// can render a preview) + bridge-supplied fingerprints (for
    /// bridge-internal channels whose raw text stays off the HTTP
    /// wire per the wip-7 contract).
    ///
    /// Idempotent per fingerprint — calling this multiple times in
    /// the same turn with identical content only bumps the
    /// `last_seen_round` of the current run. A content change opens a
    /// new run so the freshness report resets `rounds_alive` to 0 and
    /// clears a stale status.
    ///
    /// The self-model channels (`RecentFailingTests`, `OutcomeBias`)
    /// are derived from this session's own state. Everything else
    /// comes from the caller because only the bridge / CLI sees what
    /// actually landed in `edge_profile` / `dynamic_sections` this
    /// turn.
    ///
    /// wip-7 fix #4 (missing event): if `injection_fingerprints` is
    /// `None`, the bridge did not emit the `injection_freshness`
    /// event this turn — the observation pipe is broken. Skip the
    /// bridge-internal channels entirely so their history stays
    /// `Untracked` rather than being defaulted to `Empty` (which
    /// would mask the broken pipe).
    ///
    /// Call site: CLI's post-turn observation hook in
    /// `cli_loop_host::execute_turn` — fires AFTER the bridge's SSE
    /// stream has drained so every live channel gets fingerprinted
    /// against the exact bytes the model consumed this turn.
    pub fn observe_injections_partial(
        &mut self,
        self_observed: InjectionPreviews<'_>,
        injection_fingerprints: Option<
            &astra_turn_core::chat_turn_sse_dispatch::InjectionFingerprints,
        >,
    ) {
        use astra_turn_core::injection_tracking::{InjectionChannel, InjectionFingerprint};
        let round = self.turn_number;

        let failing_text = self.recent_failing_tests.join(",");
        self.injection_history.observe(
            round,
            InjectionChannel::RecentFailingTests,
            InjectionFingerprint::from_content(&failing_text),
        );

        let bias_text = self
            .outcome_bias
            .iter()
            .map(|(t, b)| {
                let tag = b.last_failure_tag.as_deref().unwrap_or("");
                format!("{t}={:.3}:{tag}", b.score)
            })
            .collect::<Vec<_>>()
            .join(",");
        self.injection_history.observe(
            round,
            InjectionChannel::OutcomeBias,
            InjectionFingerprint::from_content(&bias_text),
        );

        // CLI-owned channels: fingerprint from the raw text the CLI
        // already has. Preview is populated so introspect can show
        // the first 80 chars of the actual injection.
        let InjectionPreviews {
            lessons,
            volatile,
            memoria_prefetch,
            self_awareness,
            recent_arg_hints,
            skill_listing,
            tool_round_guidance,
        } = self_observed;

        // Track which tags the CLI supplied raw text for (even if the
        // text is empty — `""` still counts as "observed as empty").
        // Tags listed here will NOT be overwritten by bridge
        // fingerprints below: CLI is authoritative for its own
        // channels. Tags not listed fall through to bridge-fingerprint
        // observation so the wire-derived hash/bytes still register.
        let cli_owned: &[(InjectionChannel, &str)] = &[
            (InjectionChannel::Lessons, lessons),
            (InjectionChannel::SelfAwareness, self_awareness),
        ];
        // CLI-owned pairs that may or may not carry text this turn —
        // if the CLI doesn't have an easy source in the post-turn
        // hook, it passes `""` and relies on the bridge fingerprint
        // (below) for the hash/bytes. Tracked here so we know not to
        // double-observe.
        let cli_passthrough: &[(InjectionChannel, &str)] = &[
            (InjectionChannel::RecentArgHints, recent_arg_hints),
            (InjectionChannel::SkillListing, skill_listing),
        ];

        for (channel, text) in cli_owned {
            self.injection_history.observe(
                round,
                *channel,
                InjectionFingerprint::from_content(text),
            );
        }
        // CLI-passthrough: if CLI actually has the text (non-empty),
        // prefer that — the preview is more useful than the bridge's
        // empty-preview fingerprint. If CLI has nothing, fall through
        // to bridge fingerprint below.
        let mut observed_tags: std::collections::HashSet<&'static str> =
            cli_owned.iter().map(|(c, _)| c.tag()).collect();
        for (channel, text) in cli_passthrough {
            if !text.is_empty() {
                self.injection_history.observe(
                    round,
                    *channel,
                    InjectionFingerprint::from_content(text),
                );
                observed_tags.insert(channel.tag());
            }
        }
        // If CLI provided text for bridge-internal channels (legacy
        // call sites), honour it so they still get observed — but
        // wip-7 contract: CLI should NOT have raw text for
        // bridge-internal channels, these are expected to be `""`.
        let cli_echo: &[(InjectionChannel, &str)] = &[
            (InjectionChannel::VolatilePending, volatile),
            (InjectionChannel::MemoriaPrefetch, memoria_prefetch),
            (InjectionChannel::ToolRoundGuidance, tool_round_guidance),
        ];
        for (channel, text) in cli_echo {
            if !text.is_empty() {
                self.injection_history.observe(
                    round,
                    *channel,
                    InjectionFingerprint::from_content(text),
                );
                observed_tags.insert(channel.tag());
            }
        }

        // wip-7 fix #4: missing event → skip bridge-internal channels.
        // DO NOT synthesize an empty bundle — that would mark every
        // bridge channel as `Empty` in the freshness report and
        // silently mask the fact that the observation pipe failed.
        if let Some(fps) = injection_fingerprints {
            for entry in &fps.channels {
                if observed_tags.contains(entry.tag.as_str()) {
                    // CLI already observed this channel with raw
                    // text — skip the wire fingerprint to avoid
                    // double-observation. First observation wins.
                    continue;
                }
                let Some(channel) = InjectionChannel::from_tag(&entry.tag) else {
                    continue;
                };
                self.injection_history.observe(
                    round,
                    channel,
                    InjectionFingerprint {
                        hash: entry.hash,
                        preview: String::new(),
                        is_empty: entry.is_empty,
                    },
                );
            }
        }
    }

    /// Publish the four SelfModel inputs that were previously hard-coded to
    /// empty at the `build_self_model_snapshot` call site. `recent_signals`
    /// is bounded to the most recent 16 entries (tail-preserving) so the
    /// SelfModel does not balloon when the feedback buffer retains a long
    /// window.
    pub fn ingest_self_model_inputs(
        &mut self,
        skills: Vec<String>,
        tool_health_entries: Vec<astra_pipeline::ToolHealthEntry>,
        scenario: Option<Scenario>,
        recent_signals: Vec<FeedbackSignal>,
    ) {
        const MAX_SIGNALS: usize = 16;
        self.cached_skill_names = skills;
        self.last_tool_health_export = tool_health_entries;
        self.active_scenario = scenario;
        let trimmed = if recent_signals.len() > MAX_SIGNALS {
            let start = recent_signals.len() - MAX_SIGNALS;
            recent_signals.into_iter().skip(start).collect()
        } else {
            recent_signals
        };
        self.last_feedback_signals = trimmed;
    }

    /// Record a context assembly trace.
    pub fn record_context_trace(&mut self, trace: ContextAssemblyTrace) {
        const MAX_CONTEXT_TRACES: usize = 50;
        if self.context_traces.len() >= MAX_CONTEXT_TRACES {
            self.context_traces.drain(..1);
        }
        self.context_traces.push(trace);
    }

    /// Record a decision explanation.
    pub fn record_decision(&mut self, explanation: DecisionExplanation) {
        self.decision_explanations.push(explanation);
    }

    /// Record turn timing.
    pub fn record_turn_timing(&mut self, timing: TurnTiming) {
        self.turn_timings.push(timing);
        self.turn_number += 1;
    }

    pub fn record_fuzzy_match_event(
        &mut self,
        path: impl Into<String>,
        strategy: impl Into<String>,
        outcome: FuzzyMatchOutcome,
    ) {
        self.fuzzy_match_events.push(FuzzyMatchEvent {
            turn: self.turn_number + 1,
            path: path.into(),
            strategy: strategy.into(),
            outcome,
        });
    }

    /// Record a query for drift analysis.
    ///
    /// On the first turn, this also sets `original_query` for baseline comparison
    /// and initializes the goal tracker.
    pub fn record_query(&mut self, query: &str) {
        self.record_query_at(query, Instant::now());
    }

    fn record_query_at(&mut self, query: &str, query_time: Instant) {
        // Set original query on first turn
        if self.original_query.is_none() {
            self.original_query = Some(query.to_string());
        }
        self.recent_queries.push(query.to_string());
        // Keep only the last N queries
        if self.recent_queries.len() > 20 {
            self.recent_queries.remove(0);
        }

        self.last_query_at = Some(query_time);
    }

    /// Record that history compression occurred at the given turn.
    ///
    /// Called by the compression pipeline when turns are compressed/dropped.
    pub fn record_compression(&mut self, turn: u32) {
        if !self.compressed_turns.contains(&turn) {
            self.compressed_turns.push(turn);
        }
    }

    /// Record that user provided a direct correction at the current turn.
    ///
    /// Called for hard corrections such as "no, that's wrong". Reanchors and
    /// additional constraints are tracked as feedback, but they must not
    /// inflate session correction pressure.
    pub fn record_user_correction(&mut self) -> bool {
        let turn = self.turn_number;
        if self.user_corrections.contains(&turn) {
            return false;
        }
        self.user_corrections.push(turn);
        true
    }

    /// Gap 5: push a compact excerpt of the most recent user-correction
    /// utterance. Keeps at most the latest 5 so prompt surface stays bounded.
    pub fn record_correction_excerpt(&mut self, query: &str) {
        const MAX_EXCERPTS: usize = 5;
        // Clip to a reasonable prompt-safe length; renderer will further
        // truncate for display.
        let excerpt: String = query.chars().take(120).collect();
        self.recent_correction_excerpts.push(excerpt);
        if self.recent_correction_excerpts.len() > MAX_EXCERPTS {
            let drop = self.recent_correction_excerpts.len() - MAX_EXCERPTS;
            self.recent_correction_excerpts.drain(0..drop);
        }
    }

    /// Increment the cumulative stall counter. Call once per pipeline-
    /// detected stall; saturates at `u32::MAX` (any sane session tops
    /// out in single digits, so the cap is paranoia, not a constraint).
    pub fn record_stall_event(&mut self) {
        self.stall_event_count = self.stall_event_count.saturating_add(1);
    }

    /// Gap 2: publish names of tests that failed in a recent tool outcome.
    /// Dedup preserves the newest occurrence; the ring is bounded.
    pub fn record_failing_test_names(&mut self, names: impl IntoIterator<Item = String>) {
        const MAX_NAMES: usize = 8;
        for name in names {
            if name.trim().is_empty() {
                continue;
            }
            self.recent_failing_tests.retain(|n| n != &name);
            self.recent_failing_tests.push(name);
        }
        if self.recent_failing_tests.len() > MAX_NAMES {
            let drop = self.recent_failing_tests.len() - MAX_NAMES;
            self.recent_failing_tests.drain(0..drop);
        }
    }

    /// Tier 1 expiry: clear the bounded failing-test ring because a
    /// later build/test run came back green. Called by the shell
    /// handler when parsed output reports `passed == true` and no error
    /// messages — at that point, any previously-recorded failures are
    /// either (a) actually fixed, or (b) not part of the scope being
    /// validated; in either case the signal is stale and should not be
    /// re-rendered into the self-awareness block for the next turn.
    ///
    /// If the scope-b case is relevant (operator ran only a subset and
    /// other tests remain broken), the next red test run will
    /// re-populate the ring within one turn — so clearing is self-
    /// healing, not information-destroying. This fixes the f85a02bb
    /// regression where a mis-cwd Cargo.toml failure recorded on round
    /// 0 persisted for 58 consecutive rounds after every later cargo
    /// invocation succeeded.
    pub fn clear_failing_tests(&mut self) {
        self.recent_failing_tests.clear();
    }

    /// Gap 3: record a permission rejection with its reason. Dedups by
    /// `(tool, reason)` so repeated identical rejections don't flood the
    /// prompt surface.
    pub fn record_rejection(&mut self, tool: &str, reason: &str) {
        const MAX_REJECTIONS: usize = 5;
        let summary = crate::self_model::RejectionSummary {
            tool: tool.to_string(),
            reason: reason.to_string(),
        };
        self.recent_rejections
            .retain(|r| !(r.tool == summary.tool && r.reason == summary.reason));
        self.recent_rejections.push(summary);
        if self.recent_rejections.len() > MAX_REJECTIONS {
            let drop = self.recent_rejections.len() - MAX_REJECTIONS;
            self.recent_rejections.drain(0..drop);
        }
    }

    /// Gap 6: publish the current per-tool outcome bias snapshot. Passing
    /// an empty map clears the prompt surface for this signal.
    pub fn set_outcome_bias(
        &mut self,
        bias: std::collections::BTreeMap<String, astra_turn_core::tool_health::OutcomeBiasEntry>,
    ) {
        self.outcome_bias = bias;
    }

    /// Record a query and return timing/correction behavior for signal wiring.
    pub fn observe_query_behavior(&mut self, query: &str) -> QueryBehavior {
        let query_time = Instant::now();
        let delay_since_last_query_ms = self
            .last_query_at
            .map(|previous| query_time.duration_since(previous).as_millis() as u64);
        self.record_query_at(query, query_time);
        QueryBehavior {
            delay_since_last_query_ms,
        }
    }

    /// Get session duration.
    pub fn duration(&self) -> Duration {
        self.started_at.elapsed()
    }

    /// Record a tool result (no-op; previously fed the goal tracker).
    pub fn record_tool_result(&mut self, _tool_name: &str, _output: &str, _exit_code: Option<i32>) {
    }

    pub fn rollback_snapshot(&self) -> ObservabilitySessionRollbackSnapshot {
        record_observability_rollback_history(
            astra_core::history_work::HistoryWorkSite::ObservabilityRollbackSnapshotClone,
            &self.original_query,
            &self.recent_queries,
            &self.compressed_turns,
            &self.user_corrections,
            &self.context_traces,
        );
        ObservabilitySessionRollbackSnapshot {
            config: self.config.clone(),
            original_query: self.original_query.clone(),
            recent_queries: self.recent_queries.clone(),
            compressed_turns: self.compressed_turns.clone(),
            user_corrections: self.user_corrections.clone(),
            context_traces: self.context_traces.clone(),
            last_query_at: self.last_query_at,
        }
    }

    pub fn restore_rollback_snapshot(&mut self, snapshot: &ObservabilitySessionRollbackSnapshot) {
        record_observability_rollback_history(
            astra_core::history_work::HistoryWorkSite::ObservabilityRollbackRestoreClone,
            &snapshot.original_query,
            &snapshot.recent_queries,
            &snapshot.compressed_turns,
            &snapshot.user_corrections,
            &snapshot.context_traces,
        );
        self.config = snapshot.config.clone();
        self.original_query = snapshot.original_query.clone();
        self.recent_queries = snapshot.recent_queries.clone();
        self.compressed_turns = snapshot.compressed_turns.clone();
        self.user_corrections = snapshot.user_corrections.clone();
        self.context_traces = snapshot.context_traces.clone();
        self.last_query_at = snapshot.last_query_at;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn observe_query_behavior_records_timing_without_classifying_text() {
        let mut session = ObservabilitySession::new_simple("s-correction");

        let behavior = session.observe_query_behavior("arbitrary user input");

        assert_eq!(behavior.delay_since_last_query_ms, None);
        assert!(session.user_corrections.is_empty());
    }

    #[test]
    fn rollback_snapshot_clone_and_restore_preserve_structured_history() {
        let mut session = ObservabilitySession::new_simple("s-rollback");
        session.original_query = Some("原始问题".to_string());
        session.recent_queries = vec!["first".to_string(), "第二个".to_string()];
        session.compressed_turns = vec![2, 7];
        session.user_corrections = vec![3];

        let snapshot = session.rollback_snapshot();
        let retained = snapshot.clone();
        session.original_query = Some("changed".to_string());
        session.recent_queries.clear();
        session.restore_rollback_snapshot(&retained);

        assert_eq!(session.original_query.as_deref(), Some("原始问题"));
        assert_eq!(session.recent_queries, ["first", "第二个"]);
        assert_eq!(session.compressed_turns, [2, 7]);
        assert_eq!(session.user_corrections, [3]);
    }
}
