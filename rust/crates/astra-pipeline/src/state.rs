//! TurnState: unified state for the cognitive agent runtime.
//!
//! Replaces 15+ implicit local variables in the old `stream_chat_sse()` monolith
//! with a single struct that flows through all phases. Every field change can
//! emit a [`TurnEvent`] for observability and replay.

use std::collections::{HashMap, HashSet};

use serde::{Deserialize, Serialize};
use serde_json::Value;

// ─── Agent Phase (typed state machine) ───────────────────────────────────────

/// The cognitive phases of a single agent turn.
///
/// ```text
///                  ┌──────────┐
///                  │ Perceive │ (once)
///                  └────┬─────┘
///                       │
///                  ┌────▼─────┐
///             ┌───►│   Plan   │
///             │    └────┬─────┘
///             │         │
///             │    ┌────▼──────┐
///             │    │  Execute  │ (parallel tool calls)
///             │    └────┬──────┘
///             │         │
///             │    ┌────▼──────┐
///             │    │ Evaluate  │───continue──►(Plan)
///             │    └──┬────┬──┘
///             │  stall│    │budget
///             │       │    │exhausted
///             │  ┌────▼──┐ │
///             └──┤Reflect│ ┌▼────────┐
///                └───────┘ │Complete/ │
///                          │ Failed   │
///                          └──────────┘
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum AgentPhase {
    /// Parse intent, extract entities, check memory, detect context shift.
    Perceive,
    /// Select tools, estimate token cost, set budget.
    Plan,
    /// Execute tool calls (in parallel where possible).
    Execute,
    /// Check progress, budget, stall detection.
    Evaluate,
    /// Structured self-correction when stalled.
    Reflect,
    /// Turn completed successfully.
    Complete,
    /// Turn failed (unrecoverable or budget exhausted).
    Failed,
}

impl AgentPhase {
    /// Valid transitions from this phase. Enforced at runtime; the type system
    /// makes invalid transitions impossible in well-typed code.
    pub fn valid_next(&self) -> &'static [AgentPhase] {
        use AgentPhase::*;
        match self {
            Perceive => &[Plan],
            Plan => &[Execute, Complete], // Complete if no tools needed
            Execute => &[Evaluate],
            Evaluate => &[Plan, Reflect, Complete, Failed],
            Reflect => &[Plan, Failed],
            Complete => &[],
            Failed => &[],
        }
    }

    /// Whether this is a terminal phase.
    pub fn is_terminal(&self) -> bool {
        matches!(self, AgentPhase::Complete | AgentPhase::Failed)
    }
}

// ─── TurnOutcome ─────────────────────────────────────────────────────────────

/// Structured outcome of a turn — replaces ad-hoc string returns.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TurnOutcome {
    pub status: TurnStatus,
    /// Final assistant text (may be empty for tool-only turns).
    pub content: String,
    /// If failed, why.
    pub failure_reason: Option<String>,
    /// Tools that were blocked or failed during this turn.
    pub failed_tools: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TurnStatus {
    Success,
    Failure,
    /// Budget (tokens, time, or rounds) exhausted.
    Exhausted,
}

// ─── Reflection ──────────────────────────────────────────────────────────────

/// Structured self-correction data from the Reflect phase.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Reflection {
    /// What happened that triggered reflection.
    pub what_happened: String,
    /// Root cause analysis.
    pub why: String,
    /// Proposed corrective action.
    pub what_to_try: String,
    /// Confidence in the proposed correction (0.0-1.0).
    pub confidence: f64,
    /// Strategy adjustments to apply.
    pub strategy_delta: StrategyDelta,
}

/// Adjustments to apply after reflection.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct StrategyDelta {
    /// Tools to add to blocked list.
    pub block_tools: Vec<String>,
    /// Tools to try that weren't in the original selection.
    pub add_tools: Vec<String>,
    /// Additional context to inject into the next prompt.
    pub inject_context: Option<String>,
    /// Whether to widen tool surface (lower threshold).
    pub widen_surface: bool,
}

// ─── Budget ──────────────────────────────────────────────────────────────────

/// Three-dimensional budget: tokens, time, rounds.
///
/// Budget expands/contracts based on policy decisions:
/// - Policy may expand via `expand_with_ceiling()`
/// - Exhaustion triggers Evaluate → Failed transition
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TurnBudget {
    /// Maximum rounds allowed (starts at max_rounds, can expand).
    pub max_rounds: u32,
    /// Base max rounds (for reset).
    pub base_max_rounds: u32,
    /// Maximum tokens for this turn.
    pub max_tokens: u64,
    /// Maximum wall-clock time.
    pub max_duration_ms: u64,
    /// Start time as epoch milliseconds (serializable replacement for Instant).
    pub start_epoch_ms: u64,
    /// Tokens consumed so far.
    pub tokens_consumed: u64,
    /// Current round (0-indexed).
    pub round: u32,
}

fn epoch_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

impl TurnBudget {
    pub fn new(max_rounds: u32, max_tokens: u64, max_duration_ms: u64) -> Self {
        Self {
            max_rounds,
            base_max_rounds: max_rounds,
            max_tokens,
            max_duration_ms,
            start_epoch_ms: epoch_ms(),
            tokens_consumed: 0,
            round: 0,
        }
    }

    /// Elapsed milliseconds since budget start.
    pub fn elapsed_ms(&self) -> u64 {
        epoch_ms().saturating_sub(self.start_epoch_ms)
    }

    /// Whether any budget dimension is exhausted.
    pub fn is_exhausted(&self) -> bool {
        self.round >= self.max_rounds
            || self.tokens_consumed >= self.max_tokens
            || self.elapsed_ms() >= self.max_duration_ms
    }

    /// Which dimension is closest to exhaustion (for reporting).
    pub fn pressure_dimension(&self) -> &'static str {
        let round_pct = self.round as f64 / self.max_rounds.max(1) as f64;
        let token_pct = self.tokens_consumed as f64 / self.max_tokens.max(1) as f64;
        let time_pct = self.elapsed_ms() as f64 / self.max_duration_ms.max(1) as f64;
        if round_pct >= token_pct && round_pct >= time_pct {
            "rounds"
        } else if token_pct >= time_pct {
            "tokens"
        } else {
            "time"
        }
    }

    /// Expand budget by a factor with an explicit absolute ceiling.
    ///
    /// Unlike [`expand`], this does not hardcode the 2× cap — the ceiling
    /// is provided by the policy. Use this from the policy-driven path.
    pub fn expand_with_ceiling(&mut self, factor: f64, max_ceiling: u32) {
        let new_rounds = (self.max_rounds as f64 * factor).ceil() as u32;
        self.max_rounds = new_rounds.min(max_ceiling);
    }

    /// Record token usage for this round.
    pub fn record_tokens(&mut self, tokens: u64) {
        self.tokens_consumed += tokens;
    }

    /// Advance to next round.
    pub fn advance_round(&mut self) {
        self.round += 1;
    }
}

// ─── TurnState ───────────────────────────────────────────────────────────────

/// The single source of truth for a cognitive agent turn.
///
/// Every field that was previously a local variable in `stream_chat_sse()`
/// lives here. Phases read and mutate this struct; every mutation can emit
/// a [`TurnEvent`] via the event log.
#[derive(Debug, Serialize, Deserialize)]
pub struct TurnState {
    // ── Identity ──
    pub session_id: Option<String>,
    pub run_id: Option<String>,
    pub user_query: String,

    // ── Conversation ──
    pub messages: Vec<Value>,
    pub history: Vec<(String, String)>,

    // ── Phase tracking ──
    pub phase: AgentPhase,

    // ── tool surface ──
    pub tools_schema: Vec<Value>,
    pub boost_terms: Vec<String>,

    // ── Tool execution ──
    /// Pending tool calls from the LLM (populated by Plan/CallCloud).
    pub pending_tool_calls: Vec<Value>,
    /// Tools that failed during this turn (name → error messages).
    pub tool_failures: HashMap<String, Vec<String>>,
    /// Tools blocked from further use (circuit-breaker tripped).
    pub blocked_tools: HashSet<String>,
    /// All tools invoked during this turn (for dedup + reporting).
    pub tools_used: HashSet<String>,

    // ── Budget ──
    pub budget: TurnBudget,

    // ── Policy ──
    pub budget_policy: astra_core::observation_journal::BudgetPolicy,

    // ── Reflection ──
    pub reflections: Vec<Reflection>,

    // ── Pending signals (from policy InjectSignal) ──
    pub pending_signals: Vec<astra_core::observation_journal::PendingSignal>,

    // ── Output ──
    pub final_text: String,
    pub outcome: Option<TurnOutcome>,

    // ── Aggregated metrics ──
    pub total_prompt_tokens: u64,
    pub total_completion_tokens: u64,
    pub total_tool_calls: u32,

    // ── Evaluation snapshots (for delta-based round progress) ──
    /// Text length at last evaluation (for delta computation).
    pub prev_text_len: usize,
    /// Tool call count at last evaluation.
    pub prev_tool_calls: u32,
    /// Failure count at last evaluation.
    pub prev_failure_count: usize,
    /// Tool name sets per round (for stall detection).
    pub round_tool_signatures: Vec<HashSet<String>>,
    /// Per-round outcome tracking: true if round produced observable outcome.
    pub round_outcomes: Vec<bool>,
}

impl TurnState {
    /// Create a new TurnState for a user query.
    pub fn new(
        user_query: impl Into<String>,
        history: Vec<(String, String)>,
        max_rounds: u32,
        max_tokens: u64,
        max_duration_ms: u64,
    ) -> Self {
        let user_query = user_query.into();
        // Build initial messages from history
        let mut messages: Vec<Value> = history
            .iter()
            .flat_map(|(u, a)| {
                if u.is_empty() {
                    vec![serde_json::json!({"role": "assistant", "content": a})]
                } else {
                    vec![
                        serde_json::json!({"role": "user", "content": u}),
                        serde_json::json!({"role": "assistant", "content": a}),
                    ]
                }
            })
            .collect();
        messages.push(serde_json::json!({"role": "user", "content": &user_query}));

        Self {
            session_id: None,
            run_id: None,
            user_query,
            messages,
            history,
            phase: AgentPhase::Perceive,
            tools_schema: Vec::new(),
            boost_terms: Vec::new(),
            pending_tool_calls: Vec::new(),
            tool_failures: HashMap::new(),
            blocked_tools: HashSet::new(),
            tools_used: HashSet::new(),
            budget: TurnBudget::new(max_rounds, max_tokens, max_duration_ms),
            budget_policy: astra_core::observation_journal::BudgetPolicy::default(),
            reflections: Vec::new(),
            pending_signals: Vec::new(),
            final_text: String::new(),
            outcome: None,
            total_prompt_tokens: 0,
            total_completion_tokens: 0,
            total_tool_calls: 0,
            prev_text_len: 0,
            prev_tool_calls: 0,
            prev_failure_count: 0,
            round_tool_signatures: Vec::new(),
            round_outcomes: Vec::new(),
        }
    }

    /// Transition to a new phase. Panics if the transition is invalid.
    pub fn transition(&mut self, next: AgentPhase) {
        assert!(
            self.phase.valid_next().contains(&next),
            "Invalid phase transition: {:?} → {:?} (valid: {:?})",
            self.phase,
            next,
            self.phase.valid_next()
        );
        self.phase = next;
    }

    /// Whether the turn is in a terminal phase.
    pub fn is_done(&self) -> bool {
        self.phase.is_terminal()
    }

    /// Record a tool failure. Blocks the tool after 3 consecutive failures.
    pub fn record_tool_failure(&mut self, tool_name: &str, error: &str) {
        let failures = self.tool_failures.entry(tool_name.to_string()).or_default();
        failures.push(error.to_string());
        if failures.len() >= 3 {
            self.blocked_tools.insert(tool_name.to_string());
        }
    }

    /// Whether a tool is blocked.
    pub fn is_tool_blocked(&self, tool_name: &str) -> bool {
        self.blocked_tools.contains(tool_name)
    }

    /// Detect stall from tool signature repetition across rounds.
    ///
    /// Returns a reason string if stalled, None otherwise.
    pub fn detect_stall(&self) -> Option<String> {
        let sigs = &self.round_tool_signatures;
        const STALL_WINDOW: usize = 3;

        if sigs.len() < STALL_WINDOW {
            return None;
        }

        let recent = &sigs[sigs.len() - STALL_WINDOW..];
        let all_same = recent.windows(2).all(|w| w[0] == w[1]);

        if all_same && !recent[0].is_empty() {
            let mut tools: Vec<&String> = recent[0].iter().collect();
            tools.sort();
            Some(format!(
                "Same tools called {} times in a row: {:?}",
                STALL_WINDOW, tools,
            ))
        } else {
            None
        }
    }

    /// Record which tools were called this round (for stall detection).
    pub fn record_round_tools(&mut self, tools: HashSet<String>) {
        self.round_tool_signatures.push(tools);
    }
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn phase_valid_transitions() {
        // Perceive → Plan is valid
        assert!(
            AgentPhase::Perceive
                .valid_next()
                .contains(&AgentPhase::Plan)
        );
        // Perceive → Execute is NOT valid
        assert!(
            !AgentPhase::Perceive
                .valid_next()
                .contains(&AgentPhase::Execute)
        );
        // Evaluate → Reflect is valid
        assert!(
            AgentPhase::Evaluate
                .valid_next()
                .contains(&AgentPhase::Reflect)
        );
        // Complete is terminal
        assert!(AgentPhase::Complete.is_terminal());
        assert!(AgentPhase::Complete.valid_next().is_empty());
    }

    #[test]
    fn phase_transition_valid() {
        let mut state = TurnState::new("test", vec![], 10, 100_000, 30_000);
        assert_eq!(state.phase, AgentPhase::Perceive);
        state.transition(AgentPhase::Plan);
        assert_eq!(state.phase, AgentPhase::Plan);
        state.transition(AgentPhase::Execute);
        assert_eq!(state.phase, AgentPhase::Execute);
    }

    #[test]
    #[should_panic(expected = "Invalid phase transition")]
    fn phase_transition_invalid_panics() {
        let mut state = TurnState::new("test", vec![], 10, 100_000, 30_000);
        state.transition(AgentPhase::Execute); // Perceive → Execute is invalid
    }

    #[test]
    fn turn_state_initial_messages() {
        let history = vec![
            ("hello".to_string(), "hi there".to_string()),
            ("".to_string(), "[compacted context]".to_string()),
        ];
        let state = TurnState::new("what's up", history, 10, 100_000, 30_000);
        // history(2 entries) = 3 messages (user+assistant, compacted_assistant) + 1 current user
        assert_eq!(state.messages.len(), 4);
        assert_eq!(state.messages.last().unwrap()["content"], "what's up");
    }

    #[test]
    fn tool_failure_tracking() {
        let mut state = TurnState::new("test", vec![], 10, 100_000, 30_000);
        assert!(!state.is_tool_blocked("bash"));

        state.record_tool_failure("bash", "permission denied");
        state.record_tool_failure("bash", "timeout");
        assert!(!state.is_tool_blocked("bash")); // 2 failures, not blocked yet

        state.record_tool_failure("bash", "connection reset");
        assert!(state.is_tool_blocked("bash")); // 3 failures → blocked
    }

    #[test]
    fn tool_failure_different_tools_independent() {
        let mut state = TurnState::new("test", vec![], 10, 100_000, 30_000);
        state.record_tool_failure("bash", "err1");
        state.record_tool_failure("bash", "err2");
        state.record_tool_failure("bash", "err3");
        state.record_tool_failure("read_file", "err1");

        assert!(state.is_tool_blocked("bash"));
        assert!(!state.is_tool_blocked("read_file")); // only 1 failure
    }

    #[test]
    fn budget_exhaustion() {
        let mut budget = TurnBudget::new(5, 100_000, 30_000);
        assert!(!budget.is_exhausted());

        // Advance to max rounds
        for _ in 0..5 {
            budget.advance_round();
        }
        assert!(budget.is_exhausted());
        assert_eq!(budget.pressure_dimension(), "rounds");
    }

    #[test]
    fn budget_token_exhaustion() {
        let mut budget = TurnBudget::new(100, 1000, 60_000);
        budget.record_tokens(1001);
        assert!(budget.is_exhausted());
        assert_eq!(budget.pressure_dimension(), "tokens");
    }

    #[test]
    fn turn_state_is_done() {
        let mut state = TurnState::new("test", vec![], 10, 100_000, 30_000);
        assert!(!state.is_done());
        state.transition(AgentPhase::Plan);
        state.transition(AgentPhase::Complete);
        assert!(state.is_done());
    }

    #[test]
    fn reflection_strategy_delta_default() {
        let delta = StrategyDelta::default();
        assert!(delta.block_tools.is_empty());
        assert!(delta.add_tools.is_empty());
        assert!(delta.inject_context.is_none());
        assert!(!delta.widen_surface);
    }

    #[test]
    fn detect_stall_requires_3_same_rounds() {
        let mut state = TurnState::new("test", vec![], 10, 100_000, 30_000);
        let tools: HashSet<String> = ["bash".to_string()].into();

        state.record_round_tools(tools.clone());
        assert!(state.detect_stall().is_none()); // 1 round

        state.record_round_tools(tools.clone());
        assert!(state.detect_stall().is_none()); // 2 rounds

        state.record_round_tools(tools);
        assert!(state.detect_stall().is_some()); // 3 same rounds → stall
    }

    #[test]
    fn detect_stall_different_tools_no_stall() {
        let mut state = TurnState::new("test", vec![], 10, 100_000, 30_000);

        state.record_round_tools(["bash".to_string()].into());
        state.record_round_tools(["github".to_string()].into());
        state.record_round_tools(["bash".to_string()].into());

        assert!(state.detect_stall().is_none());
    }

    #[test]
    fn detect_stall_empty_tools_no_stall() {
        let mut state = TurnState::new("test", vec![], 10, 100_000, 30_000);

        state.record_round_tools(HashSet::new());
        state.record_round_tools(HashSet::new());
        state.record_round_tools(HashSet::new());

        // Empty tool sets don't count as stall (no action is different from repeated action)
        assert!(state.detect_stall().is_none());
    }

    // ── Serialization roundtrip tests ────────────────────────────────────────

    #[test]
    fn turn_budget_serde_roundtrip() {
        let budget = TurnBudget::new(10, 100_000, 30_000);
        let json = serde_json::to_string(&budget).unwrap();
        let back: TurnBudget = serde_json::from_str(&json).unwrap();
        assert_eq!(back.max_rounds, 10);
        assert_eq!(back.max_tokens, 100_000);
        assert_eq!(back.max_duration_ms, 30_000);
        assert_eq!(back.start_epoch_ms, budget.start_epoch_ms);
        assert_eq!(back.round, 0);
    }

    #[test]
    fn turn_budget_elapsed_ms_is_monotonic() {
        let budget = TurnBudget::new(10, 100_000, 60_000);
        let t1 = budget.elapsed_ms();
        std::thread::sleep(std::time::Duration::from_millis(5));
        let t2 = budget.elapsed_ms();
        assert!(t2 >= t1);
    }

    #[test]
    fn turn_budget_start_epoch_ms_is_recent() {
        let budget = TurnBudget::new(10, 100_000, 30_000);
        let now = super::epoch_ms();
        // start_epoch_ms should be within 100ms of now
        assert!(now - budget.start_epoch_ms < 100);
    }

    #[test]
    fn agent_phase_serde_roundtrip() {
        for phase in &[
            AgentPhase::Perceive,
            AgentPhase::Plan,
            AgentPhase::Execute,
            AgentPhase::Evaluate,
            AgentPhase::Reflect,
            AgentPhase::Complete,
            AgentPhase::Failed,
        ] {
            let json = serde_json::to_string(phase).unwrap();
            let back: AgentPhase = serde_json::from_str(&json).unwrap();
            assert_eq!(*phase, back);
        }
    }

    #[test]
    fn turn_state_serde_roundtrip() {
        let mut state = TurnState::new("hello world", vec![], 10, 100_000, 30_000);
        state.session_id = Some("sess-1".to_string());
        state.tools_used.insert("bash".to_string());
        state.total_tool_calls = 5;

        let json = serde_json::to_string(&state).unwrap();
        let back: TurnState = serde_json::from_str(&json).unwrap();

        assert_eq!(back.user_query, "hello world");
        assert_eq!(back.session_id, Some("sess-1".to_string()));
        assert!(back.tools_used.contains("bash"));
        assert_eq!(back.total_tool_calls, 5);
        assert_eq!(back.phase, AgentPhase::Perceive);
        assert_eq!(back.budget.max_rounds, 10);
    }

    #[test]
    fn turn_state_with_outcome_serde() {
        let mut state = TurnState::new("query", vec![], 5, 50_000, 10_000);
        state.outcome = Some(TurnOutcome {
            status: TurnStatus::Success,
            content: "done".to_string(),
            failure_reason: None,
            failed_tools: vec![],
        });

        let json = serde_json::to_string(&state).unwrap();
        let back: TurnState = serde_json::from_str(&json).unwrap();
        let outcome = back.outcome.unwrap();
        assert_eq!(outcome.status, TurnStatus::Success);
        assert_eq!(outcome.content, "done");
    }

    #[test]
    fn turn_state_with_reflections_serde() {
        let mut state = TurnState::new("test", vec![], 5, 50_000, 10_000);
        state.reflections.push(Reflection {
            what_happened: "stall".to_string(),
            why: "repeated calls".to_string(),
            what_to_try: "different tool".to_string(),
            confidence: 0.7,
            strategy_delta: StrategyDelta {
                block_tools: vec!["bash".to_string()],
                add_tools: vec!["grep".to_string()],
                inject_context: Some("try grep".to_string()),
                widen_surface: true,
            },
        });

        let json = serde_json::to_string(&state).unwrap();
        let back: TurnState = serde_json::from_str(&json).unwrap();
        assert_eq!(back.reflections.len(), 1);
        assert_eq!(back.reflections[0].confidence, 0.7);
        assert_eq!(back.reflections[0].strategy_delta.block_tools, vec!["bash"]);
    }

    #[test]
    fn turn_state_with_history_serde() {
        let history = vec![
            ("user msg".to_string(), "assistant reply".to_string()),
            ("follow up".to_string(), "second reply".to_string()),
        ];
        let state = TurnState::new("new query", history.clone(), 5, 50_000, 10_000);
        let json = serde_json::to_string(&state).unwrap();
        let back: TurnState = serde_json::from_str(&json).unwrap();
        assert_eq!(back.history, history);
        assert_eq!(back.messages.len(), 5); // 2 pairs + current
    }
}
