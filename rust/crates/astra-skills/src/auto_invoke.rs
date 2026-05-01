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

use serde::{Deserialize, Serialize};
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

// ── SkillDiagnosis — versioned payload injected back into SelfModel ─────────

/// Current schema version for [`SkillDiagnosis`]. Bump when fields are
/// renamed / removed so downstream LLM prompts can gate behaviour on it.
pub const SKILL_DIAGNOSIS_SCHEMA_VERSION: u32 = 1;

/// Output of an auto-invoked diagnostic skill, shaped for consumption by
/// [`SelfModel`]'s prompt renderer.
///
/// Deliberately small: the LLM will see this on every subsequent turn until
/// the next fire or until cleared, so every field has a hard budget.
///
/// All fields are renderer-friendly strings — the skill is responsible for
/// pre-summarising into short bullets (≤160 chars each) and returning at
/// most [`MAX_FINDINGS`] entries. The gate / caller enforces these caps
/// via [`SkillDiagnosis::new`], which truncates overflow.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillDiagnosis {
    /// Schema version. Always [`SKILL_DIAGNOSIS_SCHEMA_VERSION`] for
    /// newly-minted diagnoses; older values are preserved on deserialization
    /// so callers can migrate explicitly.
    pub schema_version: u32,
    /// Which skill produced this diagnosis (e.g. `"analyze_session"`).
    pub skill: String,
    /// Stable tag of the cause that triggered the skill
    /// ([`AutoInvokeCause::as_str`]).
    pub cause: String,
    /// One-sentence summary, ≤[`MAX_HEADLINE_LEN`] chars.
    pub headline: String,
    /// Bounded list of findings, each ≤[`MAX_FINDING_LEN`] chars.
    pub findings: Vec<String>,
    /// Optional recommended next action from the skill.
    pub recommended_action: Option<String>,
}

/// Max chars kept for `SkillDiagnosis::headline`.
pub const MAX_HEADLINE_LEN: usize = 160;
/// Max chars kept per `SkillDiagnosis::findings` entry.
pub const MAX_FINDING_LEN: usize = 160;
/// Max number of `SkillDiagnosis::findings` entries.
pub const MAX_FINDINGS: usize = 5;

impl SkillDiagnosis {
    /// Construct a diagnosis, truncating overflowing fields to the caps
    /// above. Returns a value that will always satisfy the size invariants
    /// regardless of what the skill produced.
    #[must_use]
    pub fn new(
        skill: impl Into<String>,
        cause: &AutoInvokeCause,
        headline: impl Into<String>,
        findings: impl IntoIterator<Item = String>,
        recommended_action: Option<String>,
    ) -> Self {
        Self {
            schema_version: SKILL_DIAGNOSIS_SCHEMA_VERSION,
            skill: skill.into(),
            cause: cause.as_str().to_string(),
            headline: truncate_chars(headline.into(), MAX_HEADLINE_LEN),
            findings: findings
                .into_iter()
                .map(|f| truncate_chars(f, MAX_FINDING_LEN))
                .take(MAX_FINDINGS)
                .collect(),
            recommended_action: recommended_action.map(|a| truncate_chars(a, MAX_FINDING_LEN)),
        }
    }

    /// Render the diagnosis as a compact multi-line block for injection
    /// into the self-model prompt section. Stable format so tests can pin
    /// the output shape.
    #[must_use]
    pub fn render_prompt_block(&self) -> String {
        use std::fmt::Write;
        let mut s = String::with_capacity(256);
        let _ = writeln!(
            s,
            "⚙ Auto-diagnosis [{}] (cause: {}): {}",
            self.skill, self.cause, self.headline
        );
        for finding in &self.findings {
            let _ = writeln!(s, "  - {finding}");
        }
        if let Some(ref action) = self.recommended_action {
            let _ = writeln!(s, "  → {action}");
        }
        s
    }
}

/// Truncate `s` to at most `max` characters (not bytes), adding an ellipsis
/// glyph when clipped. Character-based so multi-byte UTF-8 stays valid.
fn truncate_chars(s: String, max: usize) -> String {
    if s.chars().count() <= max {
        return s;
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
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

    // ── SkillDiagnosis ──────────────────────────────────────────────────

    #[test]
    fn diagnosis_has_schema_version_and_stable_cause_tag() {
        let cause = AutoInvokeCause::ConsecutiveStalls { count: 4 };
        let diag = SkillDiagnosis::new(
            "analyze_session",
            &cause,
            "agent looping on grep",
            ["tried grep 4 times with no new matches".to_string()],
            Some("switch to rg or narrow scope".to_string()),
        );
        assert_eq!(diag.schema_version, SKILL_DIAGNOSIS_SCHEMA_VERSION);
        assert_eq!(diag.schema_version, 1);
        assert_eq!(diag.skill, "analyze_session");
        assert_eq!(diag.cause, "consecutive_stalls");
    }

    #[test]
    fn diagnosis_truncates_overflowing_fields() {
        let cause = AutoInvokeCause::BudgetPressure { level: 0.9 };
        let long = "x".repeat(500);
        let diag = SkillDiagnosis::new(
            "optimize_prompt",
            &cause,
            long.clone(),
            (0..10).map(|_| long.clone()),
            Some(long.clone()),
        );
        assert!(diag.headline.chars().count() <= MAX_HEADLINE_LEN);
        assert!(diag.headline.ends_with('…'));
        assert_eq!(diag.findings.len(), MAX_FINDINGS);
        for f in &diag.findings {
            assert!(f.chars().count() <= MAX_FINDING_LEN);
        }
        let action = diag.recommended_action.unwrap();
        assert!(action.chars().count() <= MAX_FINDING_LEN);
    }

    #[test]
    fn diagnosis_truncate_preserves_utf8_boundaries() {
        // Multi-byte characters must not split: each '喵' is 3 bytes.
        let cause = AutoInvokeCause::RepeatedCorrections { count: 3 };
        let long: String = "喵".repeat(200);
        let diag = SkillDiagnosis::new("evaluate_session", &cause, long, [], None);
        // Must be valid UTF-8 (String roundtrip) and within char budget.
        assert!(diag.headline.chars().count() <= MAX_HEADLINE_LEN);
        // Round-trip via JSON to confirm no invalid bytes leaked through.
        let json = serde_json::to_string(&diag).unwrap();
        let back: SkillDiagnosis = serde_json::from_str(&json).unwrap();
        assert_eq!(back, diag);
    }

    #[test]
    fn diagnosis_render_prompt_block_has_stable_shape() {
        let cause = AutoInvokeCause::ConsecutiveStalls { count: 3 };
        let diag = SkillDiagnosis::new(
            "analyze_session",
            &cause,
            "stuck on grep",
            [
                "grep timed out on large repo".to_string(),
                "tried 3×".into(),
            ],
            Some("narrow to a subdir".into()),
        );
        let rendered = diag.render_prompt_block();
        assert!(rendered.starts_with(
            "⚙ Auto-diagnosis [analyze_session] (cause: consecutive_stalls): stuck on grep"
        ));
        assert!(rendered.contains("  - grep timed out on large repo"));
        assert!(rendered.contains("  - tried 3×"));
        assert!(rendered.contains("  → narrow to a subdir"));
    }

    #[test]
    fn diagnosis_render_without_action_omits_arrow_line() {
        let cause = AutoInvokeCause::BudgetPressure { level: 0.9 };
        let diag = SkillDiagnosis::new("optimize_prompt", &cause, "trim schemas", [], None);
        let rendered = diag.render_prompt_block();
        assert!(!rendered.contains("→"));
    }

    #[test]
    fn diagnosis_serde_roundtrip() {
        let cause = AutoInvokeCause::RepeatedCorrections { count: 5 };
        let diag = SkillDiagnosis::new(
            "evaluate_session",
            &cause,
            "repeated scope corrections",
            ["user re-scoped 5× in 8 turns".to_string()],
            None,
        );
        let json = serde_json::to_string(&diag).unwrap();
        let back: SkillDiagnosis = serde_json::from_str(&json).unwrap();
        assert_eq!(diag, back);
    }
}
