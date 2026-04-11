//! Step-level signal collector — tracks per-tool-call outcomes within a turn
//! and surfaces adaptation triggers when pressure thresholds are breached.

use std::time::Instant;

/// Record of a single tool call outcome (within-turn granularity).
#[derive(Debug, Clone)]
pub struct StepOutcome {
    pub tool_name: String,
    pub ok: bool,
    pub latency_ms: u64,
    pub tokens_used: u64,
    pub error_hint: Option<String>,
}

/// Signals the tactical adapter can react to.
#[derive(Debug, Clone, PartialEq)]
pub enum AdaptationTrigger {
    /// Consecutive tool failures (≥ threshold).
    ErrorStreak { count: u32, last_tool: String },
    /// Token consumption is above the per-turn budget fraction.
    TokenPressure {
        used: u64,
        budget: u64,
        fraction: f64,
    },
    /// Latency of last call exceeds the threshold (ms).
    HighLatency { latency_ms: u64, threshold_ms: u64 },
    /// The same tool has been called repeatedly without success.
    ToolStall { tool_name: String, calls: u32 },
    /// All recent calls succeeded — pressure is low.
    Nominal,
}

/// Configuration knobs for the step signal collector.
#[derive(Debug, Clone)]
pub struct StepSignalConfig {
    /// How many consecutive failures before ErrorStreak fires.
    pub error_streak_threshold: u32,
    /// Token budget fraction (0.0–1.0) above which TokenPressure fires.
    pub token_pressure_fraction: f64,
    /// Latency (ms) above which HighLatency fires.
    pub high_latency_threshold_ms: u64,
    /// How many calls to the same tool (without success) before ToolStall fires.
    pub tool_stall_threshold: u32,
}

impl Default for StepSignalConfig {
    fn default() -> Self {
        Self {
            error_streak_threshold: 3,
            token_pressure_fraction: 0.75,
            high_latency_threshold_ms: 30_000,
            tool_stall_threshold: 4,
        }
    }
}

/// Collects per-tool-call outcomes and evaluates adaptation triggers.
#[derive(Debug)]
pub struct StepSignalCollector {
    config: StepSignalConfig,
    outcomes: Vec<StepOutcome>,
    turn_token_budget: u64,
    consecutive_errors: u32,
    total_tokens_used: u64,
    /// Per-tool failure streak: tool_name → consecutive failures without success.
    tool_failure_counts: std::collections::HashMap<String, u32>,
    _started_at: Instant,
}

impl StepSignalCollector {
    pub fn new(config: StepSignalConfig, turn_token_budget: u64) -> Self {
        Self {
            config,
            outcomes: Vec::new(),
            turn_token_budget,
            consecutive_errors: 0,
            total_tokens_used: 0,
            tool_failure_counts: std::collections::HashMap::new(),
            _started_at: Instant::now(),
        }
    }

    /// Record a tool call outcome and return any triggered signals.
    pub fn record(&mut self, outcome: StepOutcome) -> Vec<AdaptationTrigger> {
        let mut triggers = Vec::new();

        // Update token accumulator.
        self.total_tokens_used += outcome.tokens_used;

        // Update error streak.
        if outcome.ok {
            self.consecutive_errors = 0;
            self.tool_failure_counts.remove(&outcome.tool_name);
        } else {
            self.consecutive_errors += 1;
            let count = self
                .tool_failure_counts
                .entry(outcome.tool_name.clone())
                .or_insert(0);
            *count += 1;

            // ErrorStreak trigger.
            if self.consecutive_errors >= self.config.error_streak_threshold {
                triggers.push(AdaptationTrigger::ErrorStreak {
                    count: self.consecutive_errors,
                    last_tool: outcome.tool_name.clone(),
                });
            }

            // ToolStall trigger.
            if *count >= self.config.tool_stall_threshold {
                triggers.push(AdaptationTrigger::ToolStall {
                    tool_name: outcome.tool_name.clone(),
                    calls: *count,
                });
            }
        }

        // Token pressure trigger.
        if self.turn_token_budget > 0 {
            let fraction = self.total_tokens_used as f64 / self.turn_token_budget as f64;
            if fraction >= self.config.token_pressure_fraction {
                triggers.push(AdaptationTrigger::TokenPressure {
                    used: self.total_tokens_used,
                    budget: self.turn_token_budget,
                    fraction,
                });
            }
        }

        // High latency trigger.
        if outcome.latency_ms >= self.config.high_latency_threshold_ms {
            triggers.push(AdaptationTrigger::HighLatency {
                latency_ms: outcome.latency_ms,
                threshold_ms: self.config.high_latency_threshold_ms,
            });
        }

        // Nominal — everything looks fine.
        if triggers.is_empty() && outcome.ok {
            triggers.push(AdaptationTrigger::Nominal);
        }

        self.outcomes.push(outcome);
        triggers
    }

    /// Reset for a new turn.
    pub fn reset(&mut self, new_budget: u64) {
        self.outcomes.clear();
        self.consecutive_errors = 0;
        self.total_tokens_used = 0;
        self.tool_failure_counts.clear();
        self.turn_token_budget = new_budget;
        self._started_at = Instant::now();
    }

    pub fn outcomes(&self) -> &[StepOutcome] {
        &self.outcomes
    }

    pub fn total_tokens_used(&self) -> u64 {
        self.total_tokens_used
    }

    pub fn consecutive_errors(&self) -> u32 {
        self.consecutive_errors
    }
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn make_collector() -> StepSignalCollector {
        StepSignalCollector::new(StepSignalConfig::default(), 100_000)
    }

    fn ok_outcome(tool: &str, tokens: u64, latency_ms: u64) -> StepOutcome {
        StepOutcome {
            tool_name: tool.into(),
            ok: true,
            latency_ms,
            tokens_used: tokens,
            error_hint: None,
        }
    }

    fn fail_outcome(tool: &str, tokens: u64, latency_ms: u64) -> StepOutcome {
        StepOutcome {
            tool_name: tool.into(),
            ok: false,
            latency_ms,
            tokens_used: tokens,
            error_hint: Some("error".into()),
        }
    }

    #[test]
    fn nominal_on_success() {
        let mut c = make_collector();
        let triggers = c.record(ok_outcome("bash", 1000, 100));
        assert_eq!(triggers, vec![AdaptationTrigger::Nominal]);
    }

    #[test]
    fn error_streak_after_threshold() {
        let mut c = make_collector();
        // First 2 failures — no ErrorStreak yet (threshold = 3).
        let t1 = c.record(fail_outcome("bash", 500, 100));
        assert!(
            !t1.iter()
                .any(|t| matches!(t, AdaptationTrigger::ErrorStreak { .. }))
        );
        let t2 = c.record(fail_outcome("view", 500, 100));
        assert!(
            !t2.iter()
                .any(|t| matches!(t, AdaptationTrigger::ErrorStreak { .. }))
        );

        // Third failure — ErrorStreak should trigger.
        let t3 = c.record(fail_outcome("grep", 500, 100));
        assert!(
            t3.iter()
                .any(|t| matches!(t, AdaptationTrigger::ErrorStreak { count: 3, .. }))
        );
    }

    #[test]
    fn error_streak_resets_on_success() {
        let mut c = make_collector();
        c.record(fail_outcome("bash", 500, 100));
        c.record(fail_outcome("bash", 500, 100));
        c.record(ok_outcome("bash", 500, 100)); // reset

        let t = c.record(fail_outcome("bash", 500, 100));
        assert!(
            !t.iter()
                .any(|t| matches!(t, AdaptationTrigger::ErrorStreak { .. }))
        );
        assert_eq!(c.consecutive_errors(), 1);
    }

    #[test]
    fn token_pressure_trigger() {
        let mut c = StepSignalCollector::new(StepSignalConfig::default(), 10_000);
        // Use 80% of budget → should trigger (threshold = 0.75).
        let t = c.record(ok_outcome("bash", 8_000, 100));
        assert!(t.iter().any(
            |t| matches!(t, AdaptationTrigger::TokenPressure { fraction, .. } if *fraction >= 0.75)
        ));
    }

    #[test]
    fn high_latency_trigger() {
        let mut c = make_collector();
        let t = c.record(ok_outcome("bash", 1000, 35_000)); // > 30s threshold
        assert!(t.iter().any(|t| matches!(
            t,
            AdaptationTrigger::HighLatency {
                latency_ms: 35_000,
                ..
            }
        )));
    }

    #[test]
    fn tool_stall_trigger() {
        let mut c = make_collector();
        // Default threshold = 4 consecutive failures on same tool.
        c.record(fail_outcome("bash", 100, 50));
        c.record(fail_outcome("bash", 100, 50));
        c.record(fail_outcome("bash", 100, 50));
        let t4 = c.record(fail_outcome("bash", 100, 50));
        assert!(
            t4.iter()
                .any(|t| matches!(t, AdaptationTrigger::ToolStall { calls: 4, .. }))
        );
    }

    #[test]
    fn tool_stall_resets_on_success() {
        let mut c = make_collector();
        c.record(fail_outcome("bash", 100, 50));
        c.record(fail_outcome("bash", 100, 50));
        c.record(fail_outcome("bash", 100, 50));
        c.record(ok_outcome("bash", 100, 50)); // reset
        let t = c.record(fail_outcome("bash", 100, 50));
        assert!(
            !t.iter()
                .any(|t| matches!(t, AdaptationTrigger::ToolStall { .. }))
        );
    }

    #[test]
    fn reset_clears_state() {
        let mut c = make_collector();
        c.record(fail_outcome("bash", 5000, 100));
        c.record(fail_outcome("bash", 5000, 100));
        assert_eq!(c.consecutive_errors(), 2);
        assert_eq!(c.total_tokens_used(), 10_000);

        c.reset(200_000);
        assert_eq!(c.consecutive_errors(), 0);
        assert_eq!(c.total_tokens_used(), 0);
        assert!(c.outcomes().is_empty());
    }

    #[test]
    fn multiple_triggers_can_fire_simultaneously() {
        let mut c = StepSignalCollector::new(
            StepSignalConfig {
                error_streak_threshold: 1,
                token_pressure_fraction: 0.5,
                high_latency_threshold_ms: 100,
                tool_stall_threshold: 1,
            },
            1000,
        );
        let triggers = c.record(fail_outcome("bash", 600, 200));
        // Should have: ErrorStreak + ToolStall + TokenPressure + HighLatency
        assert!(
            triggers.len() >= 3,
            "expected multiple triggers, got {:?}",
            triggers
        );
    }
}
