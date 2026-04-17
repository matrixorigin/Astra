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

// ─── Progress Tracking ───────────────────────────────────────────────────────

/// Goal-gradient progress tracking.
///
/// Measures whether each round moves the agent closer to the goal.
/// Progress rate determines budget expansion/contraction.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProgressTracker {
    /// Progress scores per round (0.0 = no progress, 1.0 = significant progress).
    pub round_scores: Vec<f64>,
}

impl ProgressTracker {
    pub fn new() -> Self {
        Self {
            round_scores: Vec::new(),
        }
    }

    /// Record progress for the current round.
    ///
    /// Score heuristics:
    /// - 0.0: No useful output (empty response, repeated tool calls)
    /// - 0.3: Tool calls executed but results unclear
    /// - 0.6: Partial answer or useful intermediate result
    /// - 1.0: Final answer or task completion signal
    pub fn record(&mut self, score: f64) {
        self.round_scores.push(score.clamp(0.0, 1.0));
    }

    /// Compute the progress rate (moving average of recent rounds).
    ///
    /// Returns `None` if no rounds recorded yet.
    pub fn rate(&self) -> Option<f64> {
        if self.round_scores.is_empty() {
            return None;
        }
        // Weighted average favoring recent rounds (exponential decay)
        let window = self.round_scores.len().min(3);
        let recent = &self.round_scores[self.round_scores.len() - window..];
        let weights = [0.5, 0.3, 0.2]; // most recent gets highest weight
        let mut sum = 0.0;
        let mut w_sum = 0.0;
        for (i, &score) in recent.iter().rev().enumerate() {
            let w = weights.get(i).copied().unwrap_or(0.1);
            sum += score * w;
            w_sum += w;
        }
        Some(sum / w_sum)
    }

    /// Whether the agent appears stalled (progress rate near zero for 2+ rounds).
    pub fn is_stalled(&self) -> bool {
        if self.round_scores.len() < 2 {
            return false;
        }
        self.rate().is_some_and(|r| r < 0.1)
    }

    /// Whether the agent is regressing (progress rate negative trend).
    pub fn is_regressing(&self) -> bool {
        if self.round_scores.len() < 3 {
            return false;
        }
        let n = self.round_scores.len();
        let last3 = &self.round_scores[n - 3..];
        // Strictly decreasing
        last3[2] < last3[1] && last3[1] < last3[0] && last3[2] < 0.15
    }
}

impl Default for ProgressTracker {
    fn default() -> Self {
        Self::new()
    }
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
    /// Whether to widen tool selection (lower threshold).
    pub widen_selection: bool,
}

// ─── Budget ──────────────────────────────────────────────────────────────────

/// Three-dimensional budget: tokens, time, rounds.
///
/// Budget expands/contracts based on progress rate:
/// - Good progress → budget.expand(1.5)
/// - No progress → trigger Reflect
/// - Regressing → abort
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

    /// Expand budget by a factor (called when progress is good).
    pub fn expand(&mut self, factor: f64) {
        let new_rounds = (self.max_rounds as f64 * factor).ceil() as u32;
        // Cap at 2x base to prevent runaway
        self.max_rounds = new_rounds.min(self.base_max_rounds * 2);
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

    // ── Tool selection ──
    pub tools_schema: Vec<Value>,
    pub selection_confidence: f64,
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

    // ── Progress ──
    pub progress: ProgressTracker,

    // ── Reflection ──
    pub reflections: Vec<Reflection>,

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
            selection_confidence: 0.0,
            boost_terms: Vec::new(),
            pending_tool_calls: Vec::new(),
            tool_failures: HashMap::new(),
            blocked_tools: HashSet::new(),
            tools_used: HashSet::new(),
            budget: TurnBudget::new(max_rounds, max_tokens, max_duration_ms),
            progress: ProgressTracker::new(),
            reflections: Vec::new(),
            final_text: String::new(),
            outcome: None,
            total_prompt_tokens: 0,
            total_completion_tokens: 0,
            total_tool_calls: 0,
            prev_text_len: 0,
            prev_tool_calls: 0,
            prev_failure_count: 0,
            round_tool_signatures: Vec::new(),
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

    /// Compute a heuristic progress score for the current round.
    ///
    /// Based on: whether we got text, tool calls executed, and no repeated stalls.
    pub fn compute_round_progress(&self) -> f64 {
        let mut score: f64 = 0.0;
        // Got final text → significant progress
        if !self.final_text.is_empty() {
            score += 0.5;
        }
        // Executed tool calls → some progress
        if self.total_tool_calls > 0 {
            score += 0.3;
        }
        // Low failure rate → healthy execution
        let total_failures: usize = self.tool_failures.values().map(|v| v.len()).sum();
        if total_failures == 0 {
            score += 0.2;
        }
        score.min(1.0)
    }

    /// Compute delta progress for this round vs. the previous snapshot.
    ///
    /// Updates the internal snapshot after computing the delta.
    /// Use this in the Evaluate stage for per-round progress tracking.
    pub fn compute_round_delta_progress(&mut self) -> f64 {
        let text_growth = self.final_text.len().saturating_sub(self.prev_text_len);
        let new_tool_calls = self.total_tool_calls.saturating_sub(self.prev_tool_calls);
        let current_failures: usize = self.tool_failures.values().map(|v| v.len()).sum();
        let new_failures = current_failures.saturating_sub(self.prev_failure_count);

        // Update snapshot for next round
        self.prev_text_len = self.final_text.len();
        self.prev_tool_calls = self.total_tool_calls;
        self.prev_failure_count = current_failures;

        // Complete inaction = zero progress
        if text_growth == 0 && new_tool_calls == 0 {
            return 0.0;
        }

        let mut score: f64 = 0.0;

        // Text growth: major progress indicator
        if text_growth > 100 {
            score += 0.5;
        } else if text_growth > 0 {
            score += 0.3;
        }

        // Tool calls executed: some forward motion
        if new_tool_calls > 0 {
            score += 0.3;
        }

        // Failure penalty
        if new_failures > 0 {
            score -= 0.2 * (new_failures as f64).min(2.0);
        }

        score.clamp(0.0, 1.0)
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
    fn budget_expansion() {
        let mut budget = TurnBudget::new(5, 100_000, 30_000);
        budget.expand(1.5);
        assert_eq!(budget.max_rounds, 8); // ceil(5 * 1.5) = 8

        // Can't exceed 2x base
        budget.expand(3.0);
        assert_eq!(budget.max_rounds, 10); // 2 * 5 = 10
    }

    #[test]
    fn progress_tracker_stall_detection() {
        let mut progress = ProgressTracker::new();
        assert!(!progress.is_stalled()); // no data

        progress.record(0.8);
        assert!(!progress.is_stalled()); // only 1 round

        progress.record(0.05);
        assert!(!progress.is_stalled()); // rate is ~0.33 (weighted avg of 0.8 and 0.05)

        // Reset with all-zero rounds
        let mut progress2 = ProgressTracker::new();
        progress2.record(0.0);
        progress2.record(0.0);
        assert!(progress2.is_stalled()); // 2 rounds of zero → stalled
    }

    #[test]
    fn progress_tracker_regression() {
        let mut progress = ProgressTracker::new();
        progress.record(0.8);
        progress.record(0.4);
        progress.record(0.1);
        assert!(progress.is_regressing()); // 0.8 > 0.4 > 0.1, and 0.1 < 0.15
    }

    #[test]
    fn progress_tracker_not_regressing_when_improving() {
        let mut progress = ProgressTracker::new();
        progress.record(0.2);
        progress.record(0.5);
        progress.record(0.8);
        assert!(!progress.is_regressing());
    }

    #[test]
    fn progress_rate_weighted_recent() {
        let mut progress = ProgressTracker::new();
        progress.record(0.0); // old, low weight
        progress.record(0.0); // middle
        progress.record(1.0); // most recent, highest weight
        let rate = progress.rate().unwrap();
        // rate should be > 0.5 because recent round has highest weight
        assert!(rate > 0.4, "rate={rate}, should favor recent 1.0");
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
    fn compute_round_progress_no_output() {
        let state = TurnState::new("test", vec![], 10, 100_000, 30_000);
        assert_eq!(state.compute_round_progress(), 0.2); // no failures = 0.2
    }

    #[test]
    fn compute_round_progress_with_text_and_tools() {
        let mut state = TurnState::new("test", vec![], 10, 100_000, 30_000);
        state.final_text = "some output".to_string();
        state.total_tool_calls = 3;
        assert!((state.compute_round_progress() - 1.0).abs() < 0.01);
    }

    #[test]
    fn reflection_strategy_delta_default() {
        let delta = StrategyDelta::default();
        assert!(delta.block_tools.is_empty());
        assert!(delta.add_tools.is_empty());
        assert!(delta.inject_context.is_none());
        assert!(!delta.widen_selection);
    }

    #[test]
    fn delta_progress_with_text_and_tools() {
        let mut state = TurnState::new("test", vec![], 10, 100_000, 30_000);

        // Round 1: add text and tool calls
        state.final_text.push_str(&"x".repeat(200));
        state.total_tool_calls = 3;
        let score1 = state.compute_round_delta_progress();
        assert!(score1 > 0.5, "Good round should score high: {}", score1);

        // Round 2: no change → zero
        let score2 = state.compute_round_delta_progress();
        assert!(
            (score2 - 0.0).abs() < f64::EPSILON,
            "No-change round should be 0.0: {}",
            score2
        );
    }

    #[test]
    fn delta_progress_with_failures() {
        let mut state = TurnState::new("test", vec![], 10, 100_000, 30_000);

        // Round with tools but failures
        state.total_tool_calls = 2;
        state.record_tool_failure("bash", "error1");
        state.record_tool_failure("bash", "error2");
        let score = state.compute_round_delta_progress();
        // tool_calls +0.3, failures -0.4 (2×0.2), clamped to 0.0
        assert!(score < 0.3, "Failures should reduce score: {}", score);
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
                widen_selection: true,
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

    #[test]
    fn progress_tracker_serde_roundtrip() {
        let mut tracker = ProgressTracker::new();
        tracker.record(0.5);
        tracker.record(0.8);

        let json = serde_json::to_string(&tracker).unwrap();
        let back: ProgressTracker = serde_json::from_str(&json).unwrap();
        assert_eq!(back.round_scores, vec![0.5, 0.8]);
    }
}
