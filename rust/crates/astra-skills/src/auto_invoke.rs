//! Auto-invocation of diagnostic skills based on runtime self-observation.
//!
//! The agent already owns three LLM-facing diagnostic skills —
//! `analyze_session`, `optimize_prompt`, `evaluate_session`. Before P0.2 they
//! were only reachable through a user-typed slash command. This module makes
//! them fire when the session itself starts misbehaving, so the agent can
//! inspect its own runtime footprint without waiting for the human to ask.
//!
//! ## Design
//!
//! [`AutoInvokeGate`] is a **pure state machine**: zero I/O, zero global
//! clocks, no tokio. Callers feed it the current session state via
//! [`SessionSignals`] together with the caller's view of `now`, and it
//! returns zero or more [`AutoInvokeRequest`]s — each naming exactly one
//! skill to run plus the cause that triggered it. Execution belongs to the
//! caller, not to this module.
//!
//! ## Thresholds
//!
//! * **≥3 consecutive stalls** → invoke `analyze_session` (cooldown 60s)
//! * **budget pressure > 0.85** → invoke `optimize_prompt` (cooldown 120s)
//! * **≥3 user corrections in the trailing window** → invoke
//!   `evaluate_session --focus corrections` (cooldown 180s)
//!
//! Cooldown is tracked *per cause* — a single stall storm must not burn
//! through every diagnostic in one turn.

use std::time::{Duration, Instant};

// ── Request shape ────────────────────────────────────────────────────────────

/// Why the gate fired. Carries the observed magnitude so the skill can see
/// the triggering context.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum AutoInvokeCause {
    /// `count` consecutive stalls observed.
    ConsecutiveStalls { count: u32 },
    /// Budget pressure (0.0-1.0) exceeded the threshold.
    BudgetPressure { level: f64 },
    /// `count` user corrections in the trailing window.
    RepeatedCorrections { count: u32 },
}

impl AutoInvokeCause {
    /// Stable tag for journaling / JSON output.
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::ConsecutiveStalls { .. } => "consecutive_stalls",
            Self::BudgetPressure { .. } => "budget_pressure",
            Self::RepeatedCorrections { .. } => "repeated_corrections",
        }
    }
}

/// A single diagnostic skill invocation requested by the gate.
#[derive(Debug, Clone, PartialEq)]
pub struct AutoInvokeRequest {
    /// Skill name (e.g. `"analyze_session"`).
    pub skill: &'static str,
    /// Scoped focus string the skill understands
    /// (e.g. `"stalls"`, `"budget"`, `"corrections"`).
    pub focus: &'static str,
    /// What triggered this invocation.
    pub cause: AutoInvokeCause,
}

// ── Session signals ──────────────────────────────────────────────────────────

/// Live signals the gate inspects. All three are already tracked elsewhere in
/// the runtime (ReflectStage stalls, budget pressure, ImprovementTracker user
/// corrections) — this struct is just the minimal view the gate needs.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct SessionSignals {
    /// Number of stalls seen since the last non-stall turn.
    pub consecutive_stalls: u32,
    /// Current budget pressure, `0.0..=1.0`.
    pub budget_pressure: f64,
    /// Count of user corrections in the trailing `corrections_window` turns.
    pub recent_corrections: u32,
    /// Size of the trailing window used to count `recent_corrections`. Only
    /// used for context when logging; gate does not re-window.
    pub corrections_window: u32,
}

// ── Thresholds & cooldowns (constants — intentionally not configurable yet) ─

/// Minimum consecutive stalls to fire `analyze_session`.
pub const STALL_TRIGGER_COUNT: u32 = 3;
/// Minimum budget pressure (`0.0..=1.0`) to fire `optimize_prompt`.
pub const PRESSURE_TRIGGER_LEVEL: f64 = 0.85;
/// Minimum user corrections in the window to fire `evaluate_session`.
pub const CORRECTION_TRIGGER_COUNT: u32 = 3;

const STALL_COOLDOWN: Duration = Duration::from_secs(60);
const PRESSURE_COOLDOWN: Duration = Duration::from_secs(120);
const CORRECTION_COOLDOWN: Duration = Duration::from_secs(180);

// ── Gate ─────────────────────────────────────────────────────────────────────

/// Per-cause cooldown tracker. Reusable across turns of the same session.
#[derive(Debug, Default)]
pub struct AutoInvokeGate {
    last_stall_fire: Option<Instant>,
    last_pressure_fire: Option<Instant>,
    last_correction_fire: Option<Instant>,
}

impl AutoInvokeGate {
    /// Create a new gate with no prior fires.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Evaluate the current signals against all thresholds, returning every
    /// request that should fire *right now*. Each returned request updates
    /// the gate's cooldown for its cause.
    ///
    /// `now` is injected to keep the state machine deterministic under test.
    #[must_use]
    pub fn evaluate(&mut self, signals: &SessionSignals, now: Instant) -> Vec<AutoInvokeRequest> {
        let mut out = Vec::new();

        if signals.consecutive_stalls >= STALL_TRIGGER_COUNT
            && Self::cooldown_elapsed(self.last_stall_fire, now, STALL_COOLDOWN)
        {
            out.push(AutoInvokeRequest {
                skill: "analyze_session",
                focus: "stalls",
                cause: AutoInvokeCause::ConsecutiveStalls {
                    count: signals.consecutive_stalls,
                },
            });
            self.last_stall_fire = Some(now);
        }

        if signals.budget_pressure > PRESSURE_TRIGGER_LEVEL
            && Self::cooldown_elapsed(self.last_pressure_fire, now, PRESSURE_COOLDOWN)
        {
            out.push(AutoInvokeRequest {
                skill: "optimize_prompt",
                focus: "budget",
                cause: AutoInvokeCause::BudgetPressure {
                    level: signals.budget_pressure,
                },
            });
            self.last_pressure_fire = Some(now);
        }

        if signals.recent_corrections >= CORRECTION_TRIGGER_COUNT
            && Self::cooldown_elapsed(self.last_correction_fire, now, CORRECTION_COOLDOWN)
        {
            out.push(AutoInvokeRequest {
                skill: "evaluate_session",
                focus: "corrections",
                cause: AutoInvokeCause::RepeatedCorrections {
                    count: signals.recent_corrections,
                },
            });
            self.last_correction_fire = Some(now);
        }

        out
    }

    fn cooldown_elapsed(last: Option<Instant>, now: Instant, cooldown: Duration) -> bool {
        match last {
            None => true,
            Some(t) => now.saturating_duration_since(t) >= cooldown,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn signals(stalls: u32, pressure: f64, corrections: u32) -> SessionSignals {
        SessionSignals {
            consecutive_stalls: stalls,
            budget_pressure: pressure,
            recent_corrections: corrections,
            corrections_window: 10,
        }
    }

    #[test]
    fn quiet_session_fires_nothing() {
        let mut gate = AutoInvokeGate::new();
        let now = Instant::now();
        assert!(gate.evaluate(&signals(0, 0.1, 0), now).is_empty());
        assert!(gate.evaluate(&signals(2, 0.8, 2), now).is_empty()); // all below thresholds
    }

    #[test]
    fn stall_threshold_fires_analyze_session() {
        let mut gate = AutoInvokeGate::new();
        let now = Instant::now();
        let out = gate.evaluate(&signals(3, 0.1, 0), now);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].skill, "analyze_session");
        assert_eq!(out[0].focus, "stalls");
        assert_eq!(
            out[0].cause,
            AutoInvokeCause::ConsecutiveStalls { count: 3 }
        );
    }

    #[test]
    fn pressure_threshold_fires_optimize_prompt() {
        let mut gate = AutoInvokeGate::new();
        let now = Instant::now();
        let out = gate.evaluate(&signals(0, 0.9, 0), now);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].skill, "optimize_prompt");
        assert_eq!(out[0].focus, "budget");
        match out[0].cause {
            AutoInvokeCause::BudgetPressure { level } => {
                assert!((level - 0.9).abs() < f64::EPSILON)
            }
            _ => panic!("wrong cause"),
        }
    }

    #[test]
    fn pressure_exactly_at_threshold_does_not_fire() {
        // Threshold is strict `>`, not `>=`. 0.85 is normal operation; 0.851 is pressure.
        let mut gate = AutoInvokeGate::new();
        let now = Instant::now();
        assert!(
            gate.evaluate(&signals(0, PRESSURE_TRIGGER_LEVEL, 0), now)
                .is_empty()
        );
    }

    #[test]
    fn correction_threshold_fires_evaluate_session() {
        let mut gate = AutoInvokeGate::new();
        let now = Instant::now();
        let out = gate.evaluate(&signals(0, 0.1, 3), now);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].skill, "evaluate_session");
        assert_eq!(out[0].focus, "corrections");
        assert_eq!(
            out[0].cause,
            AutoInvokeCause::RepeatedCorrections { count: 3 }
        );
    }

    #[test]
    fn all_three_triggers_fire_in_one_evaluation() {
        let mut gate = AutoInvokeGate::new();
        let now = Instant::now();
        let out = gate.evaluate(&signals(5, 0.95, 4), now);
        let names: Vec<&str> = out.iter().map(|r| r.skill).collect();
        assert_eq!(
            names,
            vec!["analyze_session", "optimize_prompt", "evaluate_session"]
        );
    }

    #[test]
    fn cooldown_prevents_refire_of_same_cause() {
        let mut gate = AutoInvokeGate::new();
        let t0 = Instant::now();
        let first = gate.evaluate(&signals(3, 0.1, 0), t0);
        assert_eq!(first.len(), 1);

        // Same moment, same cause — cooldown active.
        let immediate = gate.evaluate(&signals(3, 0.1, 0), t0);
        assert!(immediate.is_empty(), "must not refire inside cooldown");

        // Partway through cooldown — still blocked.
        let mid = gate.evaluate(&signals(3, 0.1, 0), t0 + Duration::from_secs(59));
        assert!(mid.is_empty());

        // After cooldown — allowed again.
        let later = gate.evaluate(&signals(3, 0.1, 0), t0 + STALL_COOLDOWN);
        assert_eq!(later.len(), 1);
    }

    #[test]
    fn cooldown_is_per_cause_independent() {
        // A stall fire must not silence pressure or correction causes.
        let mut gate = AutoInvokeGate::new();
        let t0 = Instant::now();
        let first = gate.evaluate(&signals(3, 0.1, 0), t0);
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].skill, "analyze_session");

        let second = gate.evaluate(&signals(3, 0.95, 3), t0);
        // stalls are on cooldown (0s elapsed), but pressure & corrections are fresh
        let names: Vec<&str> = second.iter().map(|r| r.skill).collect();
        assert_eq!(names, vec!["optimize_prompt", "evaluate_session"]);
    }

    #[test]
    fn cause_as_str_is_stable() {
        assert_eq!(
            AutoInvokeCause::ConsecutiveStalls { count: 3 }.as_str(),
            "consecutive_stalls"
        );
        assert_eq!(
            AutoInvokeCause::BudgetPressure { level: 0.9 }.as_str(),
            "budget_pressure"
        );
        assert_eq!(
            AutoInvokeCause::RepeatedCorrections { count: 3 }.as_str(),
            "repeated_corrections"
        );
    }
}
