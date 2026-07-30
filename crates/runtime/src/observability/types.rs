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

pub struct ObservabilitySession {
    /// User ID for this session.
    pub user_id: String,

    /// Session ID.
    pub session_id: String,

    /// Current turn number.
    pub turn_number: u32,

    /// User profile (loaded at session start).
    pub profile: UserProfile,

    /// Runtime config.
    pub config: RuntimeConfig,

    /// Context assembly traces for this session.
    pub context_traces: Vec<ContextAssemblyTrace>,

    /// Decision explanations for this session.
    pub decision_explanations: Vec<DecisionExplanation>,

    /// Drift detector state.

    /// Recent queries for drift analysis.
    pub recent_queries: Vec<String>,

    /// Turns where history compression occurred.
    pub compressed_turns: Vec<u32>,

    /// Turns where user provided a direct correction.
    pub user_corrections: Vec<u32>,

    /// The original user query at session start (for drift comparison).
    pub original_query: Option<String>,

    /// Session start time.
    pub started_at: Instant,

    /// When the last user query was observed.
    pub(crate) last_query_at: Option<Instant>,

    /// Turn timing data.
    pub turn_timings: Vec<TurnTiming>,

    /// Fuzzy str_replace matching telemetry for this session.
    pub fuzzy_match_events: Vec<FuzzyMatchEvent>,

    // ── Anti-flap dampening state ──
    /// The turn at which the last scenario change occurred.
    pub last_scenario_change_turn: Option<u32>,

    /// Previous direction of per-turn token budget adjustments (+1 = increase, -1 = decrease).
    /// Used to prevent oscillation.
    pub last_token_budget_direction: i8,

    /// Turn at which the last per-turn token budget change occurred.
    pub last_token_budget_change_turn: Option<u32>,

    /// Most recent [`StrategyApplication`] summary, if any, published into
    /// observability for self-model rendering so the agent passively "knows"
    /// what tool-surface surfaces were adjusted (blocked/boosted/widened).
    ///
    /// Reset lazily — it lingers until the next published strategy update.
    pub last_strategy_application: Option<crate::turn::agentic::stage_bridge::StrategyApplication>,

    /// Most recent [`GuardrailView`] snapshot published by the auto-reflection
    /// path after adjusting the reflection threshold. Surfaced into the SelfModel
    /// so the agent passively "knows" how sensitive it currently is.
    pub last_guardrail_view: Option<crate::self_model::GuardrailView>,

    /// Cumulative permission-denial pressure published by the CLI-side
    /// `PermissionManager`. `None` when the CLI has not published yet (e.g.
    /// headless runtime). Surfaced into the SelfModel so the agent perceives
    /// its own session-wide rejection rate and can self-regulate before the
    /// hard fallback threshold fires.
    pub last_denial_pressure: Option<crate::self_model::DenialPressureView>,

    /// Gap 2: bounded ring of the most recent failing test names observed
    /// in tool outcomes (e.g., `cargo test` / `pytest` / `npm test`).
    /// Newest at the back. Capacity managed by the publisher.
    pub recent_failing_tests: Vec<String>,

    /// Gap 3: bounded ring of recent `(tool, reason)` permission rejections
    /// so the SelfModel can tell the agent *why* its last calls were
    /// refused. Newest at the back.
    pub recent_rejections: Vec<crate::self_model::RejectionSummary>,

    /// Gap 5: short excerpts of user utterances that were detected as
    /// corrections. Newest at the back. Capped by the publisher.
    pub recent_correction_excerpts: Vec<String>,

    /// Cumulative session-wide stall event count. Incremented once per
    /// pipeline-detected stall via [`Self::record_stall_event`]. Used by
    /// the session-end lesson extractor to emit `PromptShape` lessons
    /// when the agent loops too often.
    pub stall_event_count: u32,

    /// Gap 6: per-tool outcome bias currently applied as advisory health signal
    /// (`ToolHealthTracker::outcome_bias_by_tool`). Sorted by tool name.
    ///
    /// Values are `OutcomeBiasEntry { score, last_failure_tag }`. The tag
    /// (e.g. `"timeout"`, `"permission"`) is populated only for negative
    /// biases and surfaces the actual failure class back to the agent.
    pub outcome_bias:
        std::collections::BTreeMap<String, astra_turn_core::tool_health::OutcomeBiasEntry>,

    /// High-failure tools surfaced for SelfModel reasoning (name, fail_rate, samples).
    /// Populated from tool-health observations; consumed by the SelfModel snapshot
    /// builder. Replaced (not appended to) on each publish.
    pub low_confidence_tools: Vec<(String, f64, u32)>,

    /// Scenario cached from the user profile for SelfModel rendering.
    /// Mirrors `profile.current_scenario` so the snapshot builder can consume
    /// it without reaching back through the profile store.
    pub active_scenario: Option<Scenario>,

    /// Skill names surfaced for SelfModel's `capabilities.skills` list.
    /// Published per-turn by the agentic loop from the active skill source
    /// (registry / selection log). Empty when no skill source is reachable.
    pub cached_skill_names: Vec<String>,

    /// Per-turn export of `ToolHealthTracker::export()` so SelfModel can
    /// reconstruct summaries + health avoidance/outcome memory without holding
    /// a reference to the live tracker.
    pub last_tool_health_export: Vec<astra_pipeline::ToolHealthEntry>,

    /// Recent feedback signals mirrored onto the session so SelfModel can
    /// render them. Bounded to the most recent 16 entries.
    pub last_feedback_signals: Vec<FeedbackSignal>,

    /// Per-channel fingerprint history for runtime-injected prompt signals
    /// (recent_failing_tests, outcome_bias, lessons, volatile_pending).
    /// Observed once per SelfModel snapshot build so the
    /// `introspect facet=noise` renderer can flag channels that have
    /// been re-rendered unchanged for many turns — session f85a02bb's
    /// 58-round stale-signal case.
    pub injection_history: astra_turn_core::injection_tracking::InjectionHistory,
}

#[derive(Debug)]
pub struct ObservabilitySessionRollbackSnapshot {
    pub config: RuntimeConfig,
    pub original_query: Option<String>,
    pub recent_queries: Vec<String>,
    pub compressed_turns: Vec<u32>,
    pub user_corrections: Vec<u32>,
    pub context_traces: Vec<ContextAssemblyTrace>,
    pub last_query_at: Option<Instant>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QueryBehavior {
    pub delay_since_last_query_ms: Option<u64>,
}

/// Raw per-turn text for the CLI-owned subset of
/// [`astra_turn_core::injection_tracking::InjectionChannel`] variants.
/// The CLI has the authoritative source for these channels (either
/// wrote them into `edge_profile` or can re-read them from the
/// executor) so it fingerprints them client-side with a full preview
/// — wip-7 keeps raw channel text off the HTTP wire.
///
/// Bridge-internal channels (`memoria_prefetch`, `tool_round_guidance`,
/// `volatile_pending`)
/// are not in this struct — the CLI receives opaque fingerprints for
/// those via [`astra_turn_core::chat_turn_sse_dispatch::BridgeInjectionFingerprints`]
/// and observes them with an empty preview (introspect still shows
/// tag + hash + bytes).
///
/// Empty `""` for a field means "CLI is tracking this channel this
/// turn but the content is empty" — the distinction between `Empty`
/// (known-silent) and `Untracked` (never-observed) depends on being
/// explicit here. If the CLI doesn't have the text for a channel
/// (e.g. it's not easily retrievable post-turn), leave it `""` and
/// pair with a bridge fingerprint — the bridge fingerprint carries
/// the authoritative hash/bytes while the CLI-side preview stays
/// empty for that channel.
#[derive(Debug, Clone, Copy, Default)]
pub struct BridgeInjectionPreviews<'a> {
    pub lessons: &'a str,
    pub volatile: &'a str,
    pub memoria_prefetch: &'a str,
    pub self_awareness: &'a str,
    pub recent_arg_hints: &'a str,
    pub skill_listing: &'a str,
    pub tool_round_guidance: &'a str,
}

impl BridgeInjectionPreviews<'_> {
    pub const EMPTY: Self = Self {
        lessons: "",
        volatile: "",
        memoria_prefetch: "",
        self_awareness: "",
        recent_arg_hints: "",
        skill_listing: "",
        tool_round_guidance: "",
    };
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TurnTiming {
    pub turn: u32,
    pub context_assembly_ms: u64,
    /// Time to first LLM token (ms). Measures server responsiveness.
    pub ttft_ms: u64,
    /// Total LLM round-trip time including full stream (ms).
    pub llm_total_ms: u64,
    pub tool_execution_ms: u64,
    pub total_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum FuzzyMatchOutcome {
    Matched,
    NotFound,
    Ambiguous,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FuzzyMatchEvent {
    pub turn: u32,
    pub path: String,
    pub strategy: String,
    pub outcome: FuzzyMatchOutcome,
}
