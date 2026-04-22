//! Self-Model — Unified introspection view of the agent's state.
//!
//! The SelfModel is a read-only snapshot that composes data from existing
//! components (ObservabilitySession, GoalTracker, ToolHealthTracker,
//! RuntimeConfig, AutoTuningEngine) into a single coherent view.
//!
//! This enables:
//! - Dynamic system prompt sections showing the agent its own state
//! - `get_agent_info` tool responses with rich introspection data
//! - Self-aware reasoning where the model can see its capabilities,
//!   constraints, and goal progress
//!
//! Design principle: **composition over creation** — no new data stores,
//! just a unified lens over what already exists.

use std::fmt::Write;

use serde::{Deserialize, Serialize};

use crate::auto_tuning::{FeedbackSignal, SignalType};
use crate::runtime_config::RuntimeConfig;
use crate::str_preview::truncate_str;
use crate::turn::context_assembly_trace::TokenBudgetTrace;
use crate::turn::goal_tracker::{GoalProgress, Milestone};
use crate::turn::tool_health::ToolHealthTracker;
use crate::user_profile::Scenario;

// ─── Core Self-Model ────────────────────────────────────────────────────────

/// A snapshot of the agent's self-awareness at a point in time.
///
/// Populated by composing data from existing runtime components.
/// Cheap to construct (no I/O, no LLM calls).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SelfModel {
    /// What tools and capabilities are available.
    pub capabilities: CapabilityView,
    /// Current execution state (turn, tokens, scenario).
    pub state: ExecutionState,
    /// Goal tracking (what we're trying to achieve).
    pub goals: GoalState,
    /// Recent feedback signals (what auto-tuning noticed).
    pub recent_signals: Vec<SignalSummary>,
    /// Constraints and safety bounds.
    pub constraints: ConstraintSet,
    /// Rolling-stats guardrail state (auto-tuned reflection threshold,
    /// recent failure rate). `None` when no tuner signal is available
    /// at snapshot time — keeps legacy tests / constructors unchanged.
    #[serde(default)]
    pub guardrail: Option<GuardrailView>,
    /// P3.1: most recent applied strategy-delta rendered as a structured
    /// before/after diff. `None` when the last reflection was a noop.
    #[serde(default)]
    pub skill_diff: Option<crate::turn::agentic_stage_bridge::SkillDiffEntry>,
    /// Cumulative permission-denial pressure for the current session.
    /// `None` when the permission layer is not wired up (unit tests / headless).
    /// Surfaced back into the system prompt so the agent can self-regulate
    /// before the session-wide fallback actually fires.
    #[serde(default)]
    pub denial_pressure: Option<DenialPressureView>,
    /// Gap 2: names of tests that failed in recent tool outcomes (e.g., the
    /// last few `cargo test` / `pytest` / `npm test` invocations). Empty
    /// when the session has no recent test failures.
    #[serde(default)]
    pub recent_failing_tests: Vec<String>,
    /// Gap 3: recent permission-rejection reasons keyed by tool, bounded to
    /// a small ring so the agent perceives *why* the runtime is refusing
    /// specific calls and can adjust scope instead of retrying blindly.
    #[serde(default)]
    pub recent_rejections: Vec<RejectionSummary>,
    /// Gap 5: short excerpts of the most recent user-correction utterances
    /// so the agent can recognize patterns ("wrong scope" vs "wrong tool")
    /// across turns instead of only seeing a raw correction count.
    #[serde(default)]
    pub recent_correction_excerpts: Vec<String>,
    /// Gap 6: per-tool outcome bias currently affecting the selector
    /// (`ToolHealthTracker::outcome_bias_by_tool`). Positive entries mildly
    /// boost the tool's score; negative entries penalize. Surfaced so the
    /// agent can audit why a tool is being preferred or avoided.
    #[serde(default)]
    pub outcome_bias: std::collections::BTreeMap<String, f64>,
    /// High-failure tools surfaced for the model to reason about — the
    /// runtime provides the signal; the model decides whether to avoid
    /// them or try anyway.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub low_confidence_tools: Vec<LowConfidenceTool>,
}

/// A tool that has been failing at a high rate in recent use, surfaced
/// to the model for reasoning.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct LowConfidenceTool {
    pub name: String,
    pub fail_rate: f64,
    pub samples: u32,
}

/// Gap 3: a single recent permission rejection, rendered into the
/// self-awareness section so the agent learns *why* calls are refused.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RejectionSummary {
    pub tool: String,
    pub reason: String,
}

/// Session-wide permission-denial pressure. The agent perceives both the
/// raw count and the configured hard ceiling so it can proactively escalate
/// to the user (or narrow scope) instead of looping on rejected prompts.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, Default)]
pub struct DenialPressureView {
    /// Cumulative deny decisions recorded this session.
    pub total_denials: u32,
    /// Session-wide hard ceiling beyond which the tracker forces
    /// fallback-to-user. `0` means "unbounded / unknown".
    pub max_total: u32,
}

impl DenialPressureView {
    /// Ratio of current denials to the configured ceiling (0.0 when
    /// `max_total == 0`). Clamped to `[0.0, 1.0]`.
    #[must_use]
    pub fn pressure(&self) -> f32 {
        if self.max_total == 0 {
            return 0.0;
        }
        (self.total_denials as f32 / self.max_total as f32).clamp(0.0, 1.0)
    }
}

/// Compact view of the guardrail auto-tuner, surfaced to the agent via
/// the self-awareness prompt section so Astra can see how its own
/// sensitivity has been tuned.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct GuardrailView {
    pub reflection_threshold: u32,
    pub last_delta: i32,
    /// None until MIN_SAMPLES turns have been observed.
    pub recent_fail_rate: Option<f32>,
    pub turns_observed: u32,
}

/// Summary of agent capabilities.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CapabilityView {
    /// Total tools available.
    pub total_tools: usize,
    /// Names of all available tools.
    pub tool_names: Vec<String>,
    /// Tools with health issues (name → health summary).
    pub tool_health: Vec<ToolHealthSummary>,
    /// Currently deprioritized tools.
    pub deprioritized_tools: Vec<String>,
    /// Pinned (always-available) tools.
    pub pinned_tools: Vec<String>,
    /// Discovered skills.
    pub skills: Vec<String>,
    /// Tools currently boosted by the last auto-reflection strategy delta.
    /// These are subtracted from any per-turn restricted set so the LLM sees
    /// them even when general deprioritization would hide them.
    #[serde(default)]
    pub boosted_tools: Vec<String>,
    /// Whether the next tool-visibility assembly will consume a one-shot
    /// `widen_selection` request from the pipeline (skipping the
    /// deprioritized→restricted merge).
    #[serde(default)]
    pub widen_selection_pending: bool,
    /// Compact recent signature-level execution memory surfaced back to the
    /// model so it can avoid blindly repeating identical tool calls.
    #[serde(default)]
    pub outcome_memory: Vec<OutcomeMemoryHint>,
}

/// Per-tool health summary.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolHealthSummary {
    pub name: String,
    pub total_calls: usize,
    pub success_rate: f64,
    pub deprioritized: bool,
    pub consecutive_failures: usize,
    pub rehabilitation_count: usize,
}

/// Compact signature-level execution memory hint.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OutcomeMemoryHint {
    pub tool_name: String,
    pub signature: String,
    pub success: bool,
    /// Stable snake_case tag of the structured failure class when the
    /// recorded outcome was a failure (e.g. `"timeout"`, `"permission_denied"`).
    /// Always `None` for successes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure_category: Option<String>,
}

/// Current execution state.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecutionState {
    /// Current turn number.
    pub turn_number: u32,
    /// Token budget information (if available from last context assembly).
    pub token_budget: Option<TokenBudgetSnapshot>,
    /// Detected scenario (if any).
    pub scenario: Option<String>,
    /// Active A/B experiment (if enrolled).
    pub active_experiment: Option<String>,
    /// Session elapsed time in seconds.
    pub session_elapsed_secs: u64,
    /// Number of user corrections detected this session.
    pub correction_count: usize,
    /// Number of history compressions triggered.
    pub compression_count: usize,
}

/// Simplified token budget view.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TokenBudgetSnapshot {
    /// Maximum context window tokens.
    pub max_tokens: u32,
    /// Tokens used so far.
    pub total_used: u32,
    /// Remaining tokens available.
    pub remaining: u32,
    /// Budget pressure (0.0 = relaxed, 1.0 = at limit).
    pub pressure: f64,
    /// Whether compression was triggered.
    pub compression_triggered: bool,
}

/// Goal tracking state.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GoalState {
    /// The effective goal currently steering the agent.
    pub goal: Option<String>,
    /// The session goal (original user query).
    pub session_goal: Option<String>,
    /// The active plan goal, if plan execution currently owns steering.
    pub plan_goal: Option<String>,
    /// The goal currently tracked by the live GoalTracker.
    pub tracked_goal: Option<String>,
    /// Which source currently provides the effective goal.
    pub goal_source: String,
    /// Whether tracked progress is aligned with the effective goal.
    pub tracking_status: String,
    /// Goal progress analysis.
    pub progress: Option<GoalProgress>,
    /// Recent milestones (last 5).
    pub recent_milestones: Vec<Milestone>,
    /// Total milestone count.
    pub milestone_count: usize,
}

/// Simplified feedback signal for display.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SignalSummary {
    /// Signal type name.
    pub signal_type: String,
    /// Associated turn (if any).
    pub turn_id: Option<String>,
    /// Seconds ago this signal was recorded.
    pub secs_ago: u64,
}

/// Safety constraints and bounds.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConstraintSet {
    /// Max config mutations per turn.
    pub max_mutations_per_turn: u32,
    /// Max config drift from baseline (0.0–1.0).
    pub config_drift_ceiling: f64,
    /// Minimum tools that must remain available.
    pub min_tool_pool_size: usize,
    /// Fraction of tokens always held in reserve.
    pub token_reserve_fraction: f64,
}

impl Default for ConstraintSet {
    fn default() -> Self {
        Self {
            max_mutations_per_turn: 2,
            config_drift_ceiling: 0.30,
            min_tool_pool_size: 5,
            token_reserve_fraction: 0.20,
        }
    }
}

// ─── Construction ───────────────────────────────────────────────────────────

impl SelfModel {
    /// Build a self-model snapshot from existing components.
    ///
    /// All parameters are optional — missing components produce empty/default
    /// sections rather than errors.
    pub fn snapshot(
        tool_names: &[&str],
        pinned_tools: &[String],
        manual_deprioritized_tools: &[String],
        skills: &[String],
        tool_health: Option<&ToolHealthTracker>,
        turn_number: u32,
        latest_budget: Option<&TokenBudgetTrace>,
        scenario: Option<&Scenario>,
        active_experiment: Option<&str>,
        session_elapsed_secs: u64,
        correction_count: usize,
        compression_count: usize,
        session_goal: Option<&str>,
        plan_goal: Option<&str>,
        tracked_goal: Option<&str>,
        goal_progress: Option<&GoalProgress>,
        milestones: Option<&[Milestone]>,
        recent_signals: &[FeedbackSignal],
        _config: &RuntimeConfig,
    ) -> Self {
        Self::snapshot_with_strategy(
            tool_names,
            pinned_tools,
            manual_deprioritized_tools,
            skills,
            tool_health,
            turn_number,
            latest_budget,
            scenario,
            active_experiment,
            session_elapsed_secs,
            correction_count,
            compression_count,
            session_goal,
            plan_goal,
            tracked_goal,
            goal_progress,
            milestones,
            recent_signals,
            _config,
            None,
        )
    }

    /// Same as [`Self::snapshot`] but also incorporates the most recent
    /// pipeline [`StrategyApplication`] so the rendered self-awareness section
    /// surfaces `boosted_tools` / `widen_selection_pending` to the agent.
    #[allow(clippy::too_many_arguments)]
    pub fn snapshot_with_strategy(
        tool_names: &[&str],
        pinned_tools: &[String],
        manual_deprioritized_tools: &[String],
        skills: &[String],
        tool_health: Option<&ToolHealthTracker>,
        turn_number: u32,
        latest_budget: Option<&TokenBudgetTrace>,
        scenario: Option<&Scenario>,
        active_experiment: Option<&str>,
        session_elapsed_secs: u64,
        correction_count: usize,
        compression_count: usize,
        session_goal: Option<&str>,
        plan_goal: Option<&str>,
        tracked_goal: Option<&str>,
        goal_progress: Option<&GoalProgress>,
        milestones: Option<&[Milestone]>,
        recent_signals: &[FeedbackSignal],
        _config: &RuntimeConfig,
        last_strategy: Option<&crate::turn::agentic_stage_bridge::StrategyApplication>,
    ) -> Self {
        // ── Capabilities ──
        let mut tool_health_summaries = Vec::new();
        let mut deprioritized = Vec::new();
        let mut outcome_memory = Vec::new();

        if let Some(health) = tool_health {
            for (name, h) in health.all() {
                if h.total_calls > 0 {
                    tool_health_summaries.push(ToolHealthSummary {
                        name: name.clone(),
                        total_calls: h.total_calls,
                        success_rate: h.success_rate(),
                        deprioritized: h.deprioritized,
                        consecutive_failures: h.consecutive_failures,
                        rehabilitation_count: h.rehabilitation_count,
                    });
                }
                if h.deprioritized {
                    deprioritized.push(name.clone());
                }
            }
            // Sort by name for stability
            tool_health_summaries.sort_by(|a, b| a.name.cmp(&b.name));
            outcome_memory = health
                .latest_outcomes(4)
                .into_iter()
                .map(|hint| OutcomeMemoryHint {
                    tool_name: hint.tool_name,
                    signature: hint.signature,
                    success: hint.success,
                    failure_category: hint
                        .failure_category
                        .map(|c| astra_turn_core::tool_health::failure_category_tag(c).to_string()),
                })
                .collect();
        }

        for tool in manual_deprioritized_tools {
            if !deprioritized.contains(tool) {
                deprioritized.push(tool.clone());
            }
        }
        deprioritized.sort();

        let (boosted_tools, widen_selection_pending) = match last_strategy {
            Some(app) => {
                let mut boosted: Vec<String> = app
                    .newly_boosted
                    .iter()
                    .chain(app.already_boosted.iter())
                    .cloned()
                    .collect();
                boosted.sort();
                boosted.dedup();
                (boosted, app.widen_requested)
            }
            None => (Vec::new(), false),
        };

        let capabilities = CapabilityView {
            total_tools: tool_names.len(),
            tool_names: tool_names.iter().map(|s| s.to_string()).collect(),
            tool_health: tool_health_summaries,
            deprioritized_tools: deprioritized,
            pinned_tools: pinned_tools.to_vec(),
            skills: skills.to_vec(),
            boosted_tools,
            widen_selection_pending,
            outcome_memory,
        };

        // ── Execution state ──
        let token_budget = latest_budget.map(|b| TokenBudgetSnapshot {
            max_tokens: b.max_tokens,
            total_used: b.total_used,
            remaining: b.max_tokens.saturating_sub(b.total_used),
            pressure: b.budget_pressure,
            compression_triggered: b.compression_triggered,
        });

        let state = ExecutionState {
            turn_number,
            token_budget,
            scenario: scenario.map(|s| format!("{:?}", s)),
            active_experiment: active_experiment.map(|s| s.to_string()),
            session_elapsed_secs,
            correction_count,
            compression_count,
        };

        // ── Goals ──
        let recent_milestones = milestones
            .map(|ms| {
                let n = ms.len();
                let start = n.saturating_sub(5);
                ms[start..].to_vec()
            })
            .unwrap_or_default();
        let milestone_count = milestones.map(|ms| ms.len()).unwrap_or(0);
        let (goal, goal_source) = resolve_effective_goal(session_goal, plan_goal, tracked_goal);
        let tracking_status = goal_tracking_status(goal.as_deref(), tracked_goal).to_string();

        let goals = GoalState {
            goal,
            session_goal: session_goal.map(|s| s.to_string()),
            plan_goal: plan_goal.map(|s| s.to_string()),
            tracked_goal: tracked_goal.map(|s| s.to_string()),
            goal_source: goal_source.to_string(),
            tracking_status,
            progress: goal_progress.cloned(),
            recent_milestones,
            milestone_count,
        };

        // ── Recent signals (last 10) ──
        let now = std::time::SystemTime::now();
        let signal_summaries: Vec<SignalSummary> = recent_signals
            .iter()
            .rev()
            .take(10)
            .map(|sig| {
                let secs_ago = now
                    .duration_since(sig.timestamp)
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                SignalSummary {
                    signal_type: signal_type_display(&sig.signal_type),
                    turn_id: sig.turn_id.clone(),
                    secs_ago,
                }
            })
            .collect();

        Self {
            capabilities,
            state,
            goals,
            recent_signals: signal_summaries,
            constraints: ConstraintSet::default(),
            guardrail: None,
            skill_diff: last_strategy.and_then(|app| app.diff_entry.clone()),
            denial_pressure: None,
            recent_failing_tests: Vec::new(),
            recent_rejections: Vec::new(),
            recent_correction_excerpts: Vec::new(),
            outcome_bias: std::collections::BTreeMap::new(),
            low_confidence_tools: Vec::new(),
        }
    }

    /// Attach a guardrail view (called by edge_tools after `snapshot_with_strategy`).
    pub fn with_guardrail(mut self, g: GuardrailView) -> Self {
        self.guardrail = Some(g);
        self
    }

    /// Attach a cumulative denial-pressure view so the agent can perceive
    /// its own session-wide rejection rate. `max_total == 0` is treated
    /// as "unknown ceiling" and will still render the raw count.
    pub fn with_denial_pressure(mut self, view: DenialPressureView) -> Self {
        self.denial_pressure = Some(view);
        self
    }

    /// Gap 2: attach names of tests that failed in recent tool outcomes.
    /// Empty vectors are preserved as empty (renderer skips).
    pub fn with_recent_failing_tests(mut self, names: Vec<String>) -> Self {
        self.recent_failing_tests = names;
        self
    }

    /// Gap 3: attach a bounded list of recent permission-rejection reasons.
    pub fn with_recent_rejections(mut self, rejections: Vec<RejectionSummary>) -> Self {
        self.recent_rejections = rejections;
        self
    }

    /// Gap 5: attach short excerpts of recent user-correction utterances so
    /// the agent can perceive *what* it is being corrected on, not just
    /// that it happened.
    pub fn with_recent_correction_excerpts(mut self, excerpts: Vec<String>) -> Self {
        self.recent_correction_excerpts = excerpts;
        self
    }

    /// Gap 6: attach a per-tool outcome-bias snapshot
    /// (`ToolHealthTracker::outcome_bias_by_tool`). Only non-zero entries
    /// should be passed in; zeros will still render as "neutral" rows if
    /// supplied.
    pub fn with_outcome_bias(mut self, bias: std::collections::BTreeMap<String, f64>) -> Self {
        self.outcome_bias = bias;
        self
    }

    /// Attach high-failure tool signals so the model can reason about
    /// alternatives. Overwrites any previously-attached list.
    pub fn with_low_confidence_tools(mut self, tools: Vec<LowConfidenceTool>) -> Self {
        self.low_confidence_tools = tools;
        self
    }

    /// Attach an explicit skill-diff entry. Useful for tests and for callers
    /// that want to inject a diff independently of `last_strategy`.
    pub fn with_skill_diff(
        mut self,
        diff: crate::turn::agentic_stage_bridge::SkillDiffEntry,
    ) -> Self {
        self.skill_diff = Some(diff);
        self
    }
}

fn resolve_effective_goal(
    session_goal: Option<&str>,
    plan_goal: Option<&str>,
    tracked_goal: Option<&str>,
) -> (Option<String>, &'static str) {
    if let Some(goal) = plan_goal {
        return (Some(goal.to_string()), "plan_goal");
    }
    if let Some(goal) = session_goal {
        return (Some(goal.to_string()), "session_goal");
    }
    if let Some(goal) = tracked_goal {
        return (Some(goal.to_string()), "tracked_goal");
    }
    (None, "none")
}

fn goal_tracking_status(effective_goal: Option<&str>, tracked_goal: Option<&str>) -> &'static str {
    match (effective_goal, tracked_goal) {
        (None, None) => "idle",
        (Some(_), None) => "untracked",
        (Some(goal), Some(tracked)) if goal == tracked => "aligned",
        (Some(_), Some(_)) => "stale",
        (None, Some(_)) => "tracked_only",
    }
}

fn render_outcome_memory_hint(hint: &OutcomeMemoryHint) -> String {
    let status = if hint.success {
        "ok".to_string()
    } else if let Some(ref cat) = hint.failure_category {
        format!("fail[{cat}]")
    } else {
        "fail".to_string()
    };
    format!("{} {}", status, truncate_str(&hint.signature, 56))
}

// ─── System Prompt Rendering ────────────────────────────────────────────────

impl SelfModel {
    /// Render a compact self-awareness section for the system prompt.
    ///
    /// Designed to be ~200-400 tokens — enough for the model to reason
    /// about its own state without excessive overhead.
    pub fn to_system_prompt_section(&self) -> String {
        let mut s = String::with_capacity(1024);

        s.push_str("\n## Self-Awareness\n");

        // ── Session state ──
        let _ = write!(s, "Turn: {}", self.state.turn_number);
        if let Some(ref budget) = self.state.token_budget {
            let pct_remaining = if budget.max_tokens > 0 {
                (budget.remaining as f64 / budget.max_tokens as f64 * 100.0) as u32
            } else {
                100
            };
            let _ = write!(
                s,
                " | Tokens: {}/{} ({}% remaining",
                budget.total_used, budget.max_tokens, pct_remaining
            );
            if budget.pressure > 0.7 {
                s.push_str(", ⚠ HIGH PRESSURE");
            }
            if budget.compression_triggered {
                s.push_str(", compressed");
            }
            s.push(')');
        }
        s.push('\n');

        if let Some(ref scenario) = self.state.scenario {
            let _ = writeln!(s, "Scenario: {}", scenario);
        }

        // ── Goal progress ──
        if let Some(ref goal) = self.goals.goal {
            let truncated = truncate_str(goal, 100);
            let _ = write!(s, "Goal: \"{}\"", truncated);
            if self.goals.goal_source != "none" && self.goals.goal_source != "session_goal" {
                let _ = write!(s, " [{}]", self.goals.goal_source);
            }
            if let Some(ref progress) = self.goals.progress {
                let _ = write!(
                    s,
                    " (progress: {:.0}%, momentum: {})",
                    progress.completion_score * 100.0,
                    if progress.momentum > 0.1 {
                        "↑"
                    } else if progress.momentum < -0.1 {
                        "↓"
                    } else {
                        "→"
                    }
                );
            }
            if self.goals.tracking_status == "stale" {
                s.push_str(" (tracking stale)");
            }
            s.push('\n');
        }

        // ── Tool health ──
        if !self.capabilities.deprioritized_tools.is_empty() {
            let _ = writeln!(
                s,
                "Deprioritized tools: {} (repeated failures — try alternatives)",
                self.capabilities.deprioritized_tools.join(", ")
            );
        }
        if !self.capabilities.outcome_memory.is_empty() {
            let failures: Vec<String> = self
                .capabilities
                .outcome_memory
                .iter()
                .filter(|hint| !hint.success)
                .take(2)
                .map(render_outcome_memory_hint)
                .collect();
            let successes: Vec<String> = self
                .capabilities
                .outcome_memory
                .iter()
                .filter(|hint| hint.success)
                .take(2)
                .map(render_outcome_memory_hint)
                .collect();
            let mut parts = Vec::new();
            if !failures.is_empty() {
                parts.push(format!(
                    "recent identical failures: {}",
                    failures.join("; ")
                ));
            }
            if !successes.is_empty() {
                parts.push(format!(
                    "recent identical successes: {}",
                    successes.join("; ")
                ));
            }
            if !parts.is_empty() {
                let _ = writeln!(
                    s,
                    "Outcome memory: {}. Reuse or retry only if the context truly changed.",
                    parts.join(" | ")
                );
            }
        }

        // ── Strategy signals from last auto-reflection ──
        if !self.capabilities.boosted_tools.is_empty() {
            let _ = writeln!(
                s,
                "Boosted tools: {} (auto-reflection added these — prefer when the task fits)",
                self.capabilities.boosted_tools.join(", ")
            );
        }
        if self.capabilities.widen_selection_pending {
            s.push_str(
                "Tool selection: widened for next turn (deprioritized set relaxed to recover from tool failures).\n",
            );
        }
        // P3.1: surface the structured before/after diff of the most recent
        // strategy-delta application so the agent can audit its own tuning.
        if let Some(diff) = &self.skill_diff {
            let _ = writeln!(s, "Strategy diff: {}", diff.summary_line());
        }

        // ── Guardrail auto-tuning state (rolling stats → bounded Δ) ──
        if let Some(g) = &self.guardrail {
            let delta_tag = match g.last_delta.cmp(&0) {
                std::cmp::Ordering::Less => " (tuned down → reacting faster)",
                std::cmp::Ordering::Greater => " (tuned up → backing off)",
                std::cmp::Ordering::Equal => "",
            };
            match g.recent_fail_rate {
                Some(rate) => {
                    let _ = writeln!(
                        s,
                        "Guardrail: reflection triggers after {} signals{} · recent fail-rate {:.0}% over {} turns",
                        g.reflection_threshold,
                        delta_tag,
                        rate * 100.0,
                        g.turns_observed
                    );
                }
                None => {
                    let _ = writeln!(
                        s,
                        "Guardrail: reflection triggers after {} signals{} · warming up ({} turns observed)",
                        g.reflection_threshold, delta_tag, g.turns_observed,
                    );
                }
            }
        }

        // ── Cumulative permission-denial pressure ──
        // Surfaced so the agent can self-regulate (narrow scope, ask the
        // user) before the hard fallback-to-user threshold actually fires.
        if let Some(dp) = &self.denial_pressure {
            if dp.total_denials > 0 {
                let pressure = dp.pressure();
                let warning = if pressure >= 0.8 {
                    " — ⚠ APPROACHING HARD FALLBACK, consider asking the user for scope"
                } else if pressure >= 0.5 {
                    " — elevated; prefer narrower scope or ask the user before retrying"
                } else {
                    ""
                };
                if dp.max_total > 0 {
                    let _ = writeln!(
                        s,
                        "Denial pressure: {}/{} denies this session{}",
                        dp.total_denials, dp.max_total, warning
                    );
                } else {
                    let _ = writeln!(
                        s,
                        "Denial pressure: {} denies this session{}",
                        dp.total_denials, warning
                    );
                }
            }
        }

        // ── Gap 2: recent test failure names ──
        // Specific failing test names are high-density signal — the agent
        // can retry just those or inspect their code, rather than reruning
        // the whole suite.
        if !self.recent_failing_tests.is_empty() {
            let sample: Vec<&str> = self
                .recent_failing_tests
                .iter()
                .take(5)
                .map(String::as_str)
                .collect();
            let extra = self.recent_failing_tests.len().saturating_sub(sample.len());
            let suffix = if extra > 0 {
                format!(" (+{extra} more)")
            } else {
                String::new()
            };
            let _ = writeln!(
                s,
                "Recent test failures: {}{} — fix or investigate these specifically before re-running the whole suite",
                sample.join(", "),
                suffix
            );
        }

        // ── Gap 3: recent permission-rejection reasons ──
        // Surfaces *why* the runtime refused recent calls so the agent
        // adjusts scope instead of retrying blindly.
        if !self.recent_rejections.is_empty() {
            let parts: Vec<String> = self
                .recent_rejections
                .iter()
                .take(3)
                .map(|r| format!("{} ({})", r.tool, r.reason))
                .collect();
            let _ = writeln!(
                s,
                "Recent rejections: {} — adjust scope or approach; don't re-issue the same call",
                parts.join("; ")
            );
        }

        // ── Gap 6: per-tool outcome bias applied by the selector ──
        // Explains which tools the selector is currently boosting /
        // penalizing based on recent success / failure history.
        if !self.outcome_bias.is_empty() {
            let mut entries: Vec<(String, f64)> = self
                .outcome_bias
                .iter()
                .filter(|(_, b)| b.abs() >= 0.005)
                .map(|(t, b)| (t.clone(), *b))
                .collect();
            entries.sort_by(|a, b| {
                b.1.abs()
                    .partial_cmp(&a.1.abs())
                    .unwrap_or(std::cmp::Ordering::Equal)
            });
            if !entries.is_empty() {
                let rendered: Vec<String> = entries
                    .into_iter()
                    .take(4)
                    .map(|(tool, bias)| {
                        let arrow = if bias > 0.0 { "↑" } else { "↓" };
                        let reason = if bias > 0.0 {
                            "recent successes"
                        } else {
                            "recent failures"
                        };
                        format!("{tool} {arrow}{:.2} ({reason})", bias.abs())
                    })
                    .collect();
                let _ = writeln!(
                    s,
                    "Tool outcome bias (selector-applied): {}",
                    rendered.join(" · ")
                );
            }
        }

        // ── Recent signals ──
        if !self.recent_signals.is_empty() {
            s.push_str("Recent signals: ");
            let signal_strs: Vec<String> = self
                .recent_signals
                .iter()
                .take(5)
                .map(|sig| {
                    if sig.secs_ago < 60 {
                        format!("{} ({}s ago)", sig.signal_type, sig.secs_ago)
                    } else {
                        format!("{} ({}m ago)", sig.signal_type, sig.secs_ago / 60)
                    }
                })
                .collect();
            s.push_str(&signal_strs.join(", "));
            s.push('\n');
        }

        // ── Corrections / compression ──
        if self.state.correction_count > 0 {
            let _ = writeln!(
                s,
                "User corrections: {} this session — adjust approach accordingly",
                self.state.correction_count
            );
            // Gap 5: render up to 3 recent correction excerpts so the agent
            // can see *what* it's being corrected on, not just the count.
            if !self.recent_correction_excerpts.is_empty() {
                let rendered: Vec<String> = self
                    .recent_correction_excerpts
                    .iter()
                    .rev()
                    .take(3)
                    .map(|e| format!("\"{}\"", truncate_str(e, 80)))
                    .collect();
                let _ = writeln!(
                    s,
                    "  Recent corrections (most recent first): {}",
                    rendered.join(" · ")
                );
            }
        }

        // ── Tools summary ──
        let _ = writeln!(
            s,
            "Tools: {} available{}",
            self.capabilities.total_tools,
            if !self.capabilities.skills.is_empty() {
                format!(", {} skills", self.capabilities.skills.len())
            } else {
                String::new()
            }
        );

        if !self.low_confidence_tools.is_empty() {
            let mut sorted = self.low_confidence_tools.clone();
            sorted.sort_by(|a, b| {
                b.fail_rate
                    .partial_cmp(&a.fail_rate)
                    .unwrap_or(std::cmp::Ordering::Equal)
            });
            s.push_str("⚠ Recent high-failure tools (consider alternatives):\n");
            for entry in sorted.iter().take(5) {
                let _ = writeln!(
                    s,
                    "  - {}: failure_rate={:.2} over {} samples",
                    entry.name, entry.fail_rate, entry.samples
                );
            }
        }

        s
    }

    /// Render detailed self-model as structured text for get_agent_info responses.
    pub fn to_detailed_text(&self) -> String {
        let mut s = String::with_capacity(4096);

        // ── Header ──
        s.push_str("# Agent Self-Model\n\n");

        // ── State ──
        s.push_str("## Execution State\n");
        let _ = writeln!(s, "- Turn: {}", self.state.turn_number);
        if let Some(ref budget) = self.state.token_budget {
            let _ = writeln!(
                s,
                "- Token budget: {}/{} ({:.1}% pressure)",
                budget.total_used,
                budget.max_tokens,
                budget.pressure * 100.0
            );
            let _ = writeln!(s, "- Remaining tokens: {}", budget.remaining);
            let _ = writeln!(
                s,
                "- Compression triggered: {}",
                budget.compression_triggered
            );
        }
        if let Some(ref scenario) = self.state.scenario {
            let _ = writeln!(s, "- Scenario: {}", scenario);
        }
        if let Some(ref exp) = self.state.active_experiment {
            let _ = writeln!(s, "- Active experiment: {}", exp);
        }
        let _ = writeln!(s, "- Session elapsed: {}s", self.state.session_elapsed_secs);
        let _ = writeln!(s, "- User corrections: {}", self.state.correction_count);
        let _ = writeln!(
            s,
            "- History compressions: {}",
            self.state.compression_count
        );

        // ── Goals ──
        s.push_str("\n## Goals\n");
        if let Some(ref goal) = self.goals.goal {
            let _ = writeln!(s, "- Effective goal: \"{}\"", goal);
        } else {
            s.push_str("- No explicit goal set\n");
        }
        if let Some(ref goal) = self.goals.session_goal {
            let _ = writeln!(s, "- Session goal: \"{}\"", goal);
        }
        if let Some(ref goal) = self.goals.plan_goal {
            let _ = writeln!(s, "- Plan goal: \"{}\"", goal);
        }
        if let Some(ref goal) = self.goals.tracked_goal {
            let _ = writeln!(s, "- Tracked goal: \"{}\"", goal);
        }
        let _ = writeln!(s, "- Goal source: {}", self.goals.goal_source);
        let _ = writeln!(s, "- Tracking status: {}", self.goals.tracking_status);
        if let Some(ref progress) = self.goals.progress {
            let _ = writeln!(s, "- Completion: {:.0}%", progress.completion_score * 100.0);
            let _ = writeln!(s, "- Momentum: {:.2}", progress.momentum);
            let _ = writeln!(s, "- Milestones: {}", progress.milestone_count);
            let _ = writeln!(s, "- Summary: {}", progress.summary);
        }
        if !self.goals.recent_milestones.is_empty() {
            s.push_str("- Recent milestones:\n");
            for m in &self.goals.recent_milestones {
                let _ = writeln!(
                    s,
                    "  - Turn {}: {:?} (relevance: {:.2})",
                    m.turn, m.signal, m.relevance
                );
            }
        }

        // ── Capabilities ──
        s.push_str("\n## Capabilities\n");
        let _ = writeln!(s, "- Total tools: {}", self.capabilities.total_tools);
        if !self.capabilities.pinned_tools.is_empty() {
            let _ = writeln!(
                s,
                "- Pinned tools: {}",
                self.capabilities.pinned_tools.join(", ")
            );
        }
        if !self.capabilities.deprioritized_tools.is_empty() {
            let _ = writeln!(
                s,
                "- Deprioritized tools: {}",
                self.capabilities.deprioritized_tools.join(", ")
            );
        }
        if !self.capabilities.skills.is_empty() {
            let _ = writeln!(
                s,
                "- Active skills: {}",
                self.capabilities.skills.join(", ")
            );
        }
        if !self.capabilities.outcome_memory.is_empty() {
            let rendered: Vec<String> = self
                .capabilities
                .outcome_memory
                .iter()
                .take(4)
                .map(render_outcome_memory_hint)
                .collect();
            let _ = writeln!(s, "- Outcome memory: {}", rendered.join("; "));
        }

        // ── Tool health (only tools with issues) ──
        let troubled: Vec<&ToolHealthSummary> = self
            .capabilities
            .tool_health
            .iter()
            .filter(|t| t.deprioritized || t.success_rate < 0.8 || t.consecutive_failures > 0)
            .collect();
        if !troubled.is_empty() {
            s.push_str("\n## Tool Health (issues only)\n");
            for t in troubled {
                let _ = writeln!(
                    s,
                    "- {}: {:.0}% success ({} calls, {} consecutive failures{})",
                    t.name,
                    t.success_rate * 100.0,
                    t.total_calls,
                    t.consecutive_failures,
                    if t.deprioritized {
                        ", DEPRIORITIZED"
                    } else {
                        ""
                    }
                );
            }
        }

        // ── Recent signals ──
        if !self.recent_signals.is_empty() {
            s.push_str("\n## Recent Feedback Signals\n");
            for sig in &self.recent_signals {
                let _ = write!(s, "- {}", sig.signal_type);
                if let Some(ref turn) = sig.turn_id {
                    let _ = write!(s, " (turn: {})", turn);
                }
                let _ = writeln!(s, " — {}s ago", sig.secs_ago);
            }
        }

        // ── Constraints ──
        s.push_str("\n## Constraints\n");
        let _ = writeln!(
            s,
            "- Max mutations per turn: {}",
            self.constraints.max_mutations_per_turn
        );
        let _ = writeln!(
            s,
            "- Config drift ceiling: {:.0}%",
            self.constraints.config_drift_ceiling * 100.0
        );
        let _ = writeln!(
            s,
            "- Min tool pool size: {}",
            self.constraints.min_tool_pool_size
        );
        let _ = writeln!(
            s,
            "- Token reserve: {:.0}%",
            self.constraints.token_reserve_fraction * 100.0
        );

        s
    }
}

// ─── Helpers ────────────────────────────────────────────────────────────────

fn signal_type_display(st: &SignalType) -> String {
    match st {
        SignalType::Retry { count } => format!("Retry({})", count),
        SignalType::Correction => "Correction".to_string(),
        SignalType::Interruption => "Interruption".to_string(),
        SignalType::Acceptance => "Acceptance".to_string(),
        SignalType::QuickFollowUp { .. } => "QuickFollowUp".to_string(),
        SignalType::LongPause { .. } => "LongPause".to_string(),
        SignalType::FocusDrift => "FocusDrift".to_string(),
        SignalType::ThumbsRating { positive } => {
            if *positive {
                "👍".to_string()
            } else {
                "👎".to_string()
            }
        }
        SignalType::StarRating { stars } => format!("⭐{}", stars),
        SignalType::TextFeedback { .. } => "TextFeedback".to_string(),
        SignalType::HighTokenUsage { tokens, .. } => format!("HighTokenUsage({})", tokens),
        SignalType::ToolChurn { calls, .. } => format!("ToolChurn({}calls)", calls),
        SignalType::TaskSuccess => "TaskSuccess".to_string(),
        SignalType::TaskFailure { .. } => "TaskFailure".to_string(),
        SignalType::ToolDeprioritized { tool_name } => {
            format!("ToolDeprioritized({})", tool_name)
        }
        SignalType::ToolRehabilitated { tool_name } => {
            format!("ToolRehabilitated({})", tool_name)
        }
    }
}

// ─── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime_config::RuntimeConfig;

    #[test]
    fn snapshot_minimal() {
        let config = RuntimeConfig::default();
        let model = SelfModel::snapshot(
            &["bash", "read_file", "write_file"],
            &[],
            &[],
            &[],
            None,
            3,
            None,
            None,
            None,
            120,
            0,
            0,
            None,
            None,
            None,
            None,
            None,
            &[],
            &config,
        );
        assert_eq!(model.capabilities.total_tools, 3);
        assert_eq!(model.state.turn_number, 3);
        assert!(model.goals.goal.is_none());
        assert!(model.goals.session_goal.is_none());
        assert_eq!(model.goals.goal_source, "none");
        assert_eq!(model.goals.tracking_status, "idle");
        assert!(model.recent_signals.is_empty());
    }

    #[test]
    fn snapshot_with_goal() {
        let config = RuntimeConfig::default();
        let progress = GoalProgress {
            completion_score: 0.45,
            momentum: 0.3,
            milestone_count: 5,
            summary: "Making good progress".to_string(),
        };
        let model = SelfModel::snapshot(
            &["bash"],
            &[],
            &[],
            &[],
            None,
            7,
            None,
            None,
            None,
            300,
            1,
            0,
            Some("Fix the auth bug"),
            None,
            Some("Fix the auth bug"),
            Some(&progress),
            None,
            &[],
            &config,
        );
        assert_eq!(model.goals.goal.as_deref(), Some("Fix the auth bug"));
        assert_eq!(
            model.goals.session_goal.as_deref(),
            Some("Fix the auth bug")
        );
        assert_eq!(
            model.goals.tracked_goal.as_deref(),
            Some("Fix the auth bug")
        );
        assert_eq!(model.goals.goal_source, "session_goal");
        assert_eq!(model.goals.tracking_status, "aligned");
        assert_eq!(
            model.goals.progress.as_ref().unwrap().completion_score,
            0.45
        );
        assert_eq!(model.state.correction_count, 1);
    }

    #[test]
    fn snapshot_with_plan_goal_reports_stale_tracking() {
        let config = RuntimeConfig::default();
        let model = SelfModel::snapshot(
            &["bash"],
            &[],
            &[],
            &[],
            None,
            8,
            None,
            None,
            None,
            300,
            0,
            0,
            Some("Fix the auth bug"),
            Some("Execute the migration plan"),
            Some("Fix the auth bug"),
            None,
            None,
            &[],
            &config,
        );

        assert_eq!(
            model.goals.goal.as_deref(),
            Some("Execute the migration plan")
        );
        assert_eq!(
            model.goals.session_goal.as_deref(),
            Some("Fix the auth bug")
        );
        assert_eq!(
            model.goals.plan_goal.as_deref(),
            Some("Execute the migration plan")
        );
        assert_eq!(
            model.goals.tracked_goal.as_deref(),
            Some("Fix the auth bug")
        );
        assert_eq!(model.goals.goal_source, "plan_goal");
        assert_eq!(model.goals.tracking_status, "stale");
    }

    #[test]
    fn snapshot_with_tool_health() {
        let config = RuntimeConfig::default();
        let mut health = ToolHealthTracker::new();
        health.record_success("bash");
        health.record_success("bash");
        health.record_failure("web_search");
        health.record_failure("web_search");
        health.record_failure("web_search");

        let model = SelfModel::snapshot(
            &["bash", "web_search"],
            &[],
            &[],
            &[],
            Some(&health),
            1,
            None,
            None,
            None,
            10,
            0,
            0,
            None,
            None,
            None,
            None,
            None,
            &[],
            &config,
        );
        assert_eq!(model.capabilities.deprioritized_tools, vec!["web_search"]);
        assert_eq!(model.capabilities.tool_health.len(), 2);
    }

    #[test]
    fn system_prompt_section_surfaces_outcome_memory() {
        let config = RuntimeConfig::default();
        let mut health = ToolHealthTracker::new();
        health.record_outcome(
            r#"bash:{"command":"pwd"}"#,
            crate::turn::tool_health::ToolOutcome {
                success: true,
                latency_ms: 9,
                result_hash: 11,
                at_epoch: 10,
                failure_category: None,
            },
        );
        health.record_outcome(
            r#"grep:{"pattern":"TODO"}"#,
            crate::turn::tool_health::ToolOutcome {
                success: false,
                latency_ms: 12,
                result_hash: 22,
                at_epoch: 20,
                failure_category: Some(
                    astra_turn_core::action_compensation::FailureCategory::Timeout,
                ),
            },
        );

        let model = SelfModel::snapshot(
            &["bash", "grep"],
            &[],
            &[],
            &[],
            Some(&health),
            2,
            None,
            None,
            None,
            10,
            0,
            0,
            Some("Inspect duplicate work"),
            None,
            None,
            None,
            None,
            &[],
            &config,
        );

        let section = model.to_system_prompt_section();
        assert!(section.contains("Outcome memory:"), "got: {section}");
        assert!(
            section.contains("recent identical failures"),
            "got: {section}"
        );
        assert!(
            section.contains("recent identical successes"),
            "got: {section}"
        );
        assert!(
            section.contains(r#"grep:{"pattern":"TODO"}"#),
            "got: {section}"
        );
        assert!(
            section.contains(r#"bash:{"command":"pwd"}"#),
            "got: {section}"
        );
        assert!(
            section.contains("fail[timeout]"),
            "failure category tag should be rendered alongside signature, got: {section}"
        );
    }

    #[test]
    fn system_prompt_section_compact() {
        let config = RuntimeConfig::default();
        let budget = TokenBudgetTrace {
            max_tokens: 128000,
            system_prompt_tokens: 5000,
            history_tokens: 20000,
            memory_tokens: 3000,
            tool_schema_tokens: 8000,
            user_message_tokens: 500,
            total_used: 48000,
            budget_pressure: 0.375,
            compression_triggered: false,
        };
        let model = SelfModel::snapshot(
            &["bash", "read_file", "write_file", "grep", "glob"],
            &[],
            &[],
            &["debugging".to_string()],
            None,
            5,
            Some(&budget),
            Some(&Scenario::Debugging),
            None,
            240,
            0,
            0,
            Some("Fix the auth bug in user_service.rs"),
            None,
            None,
            None,
            None,
            &[],
            &config,
        );
        let section = model.to_system_prompt_section();
        assert!(section.contains("Self-Awareness"));
        assert!(section.contains("Turn: 5"));
        assert!(section.contains("128000"));
        assert!(section.contains("Debugging"));
        assert!(section.contains("Fix the auth bug"));
        assert!(section.contains("5 available"));
        assert!(section.contains("1 skills"));
    }

    #[test]
    fn system_prompt_section_handles_multibyte_goal_without_panicking() {
        let config = RuntimeConfig::default();
        let goal = "在tmp目录下面生成一个非常漂亮的介绍astra的web文档,内容丰富,动态展示,科技风格";
        assert!(
            goal.len() > 100,
            "regression requires byte length > 100 to trigger the old slicing bug"
        );
        assert!(
            goal.chars().count() < 100,
            "regression requires char count < 100 so UTF-8 bytes, not logical length, caused truncation"
        );

        let model = SelfModel::snapshot(
            &["bash", "write_file"],
            &[],
            &[],
            &[],
            None,
            3,
            None,
            None,
            None,
            240,
            0,
            0,
            Some(goal),
            None,
            None,
            None,
            None,
            &[],
            &config,
        );

        let section = model.to_system_prompt_section();
        assert!(section.contains("Goal:"));
        assert!(
            section.contains(goal),
            "goal should remain intact when it is under the char limit even if its UTF-8 byte length exceeds 100: {section}"
        );
    }

    #[test]
    fn detailed_text_complete() {
        let config = RuntimeConfig::default();
        let model = SelfModel::snapshot(
            &["bash", "read_file"],
            &["bash".to_string()],
            &[],
            &[],
            None,
            10,
            None,
            None,
            Some("exp-123"),
            600,
            2,
            1,
            Some("Implement feature X"),
            None,
            None,
            None,
            None,
            &[],
            &config,
        );
        let text = model.to_detailed_text();
        assert!(text.contains("Agent Self-Model"));
        assert!(text.contains("Turn: 10"));
        assert!(text.contains("exp-123"));
        assert!(text.contains("User corrections: 2"));
        assert!(text.contains("Pinned tools: bash"));
        assert!(text.contains("Implement feature X"));
    }

    #[test]
    fn signal_summaries_limited_to_10() {
        let config = RuntimeConfig::default();
        let signals: Vec<FeedbackSignal> = (0..20)
            .map(|_| FeedbackSignal::new(SignalType::Acceptance))
            .collect();
        let model = SelfModel::snapshot(
            &[],
            &[],
            &[],
            &[],
            None,
            1,
            None,
            None,
            None,
            0,
            0,
            0,
            None,
            None,
            None,
            None,
            None,
            &signals,
            &config,
        );
        assert_eq!(model.recent_signals.len(), 10);
    }

    #[test]
    fn default_constraints() {
        let c = ConstraintSet::default();
        assert_eq!(c.max_mutations_per_turn, 2);
        assert!((c.config_drift_ceiling - 0.30).abs() < f64::EPSILON);
        assert_eq!(c.min_tool_pool_size, 5);
        assert!((c.token_reserve_fraction - 0.20).abs() < f64::EPSILON);
    }

    #[test]
    fn snapshot_with_strategy_renders_boosted_and_widen() {
        let config = RuntimeConfig::default();
        let app = crate::turn::agentic_stage_bridge::StrategyApplication {
            newly_blocked: vec![],
            already_blocked: vec![],
            widen_requested: true,
            newly_boosted: vec!["read_file".into(), "grep".into()],
            already_boosted: vec!["bash".into()],
            diff_entry: None,
        };
        let model = SelfModel::snapshot_with_strategy(
            &["bash", "read_file", "grep"],
            &[],
            &[],
            &[],
            None,
            3,
            None,
            None,
            None,
            10,
            0,
            0,
            None,
            None,
            None,
            None,
            None,
            &[],
            &config,
            Some(&app),
        );
        assert_eq!(
            model.capabilities.boosted_tools,
            vec!["bash", "grep", "read_file"]
        );
        assert!(model.capabilities.widen_selection_pending);
        let rendered = model.to_system_prompt_section();
        assert!(
            rendered.contains("Boosted tools: bash, grep, read_file"),
            "got: {rendered}"
        );
        assert!(
            rendered.contains("widened for next turn"),
            "got: {rendered}"
        );
    }

    #[test]
    fn snapshot_with_skill_diff_renders_strategy_diff_line() {
        use crate::turn::agentic_stage_bridge::{DiffSnapshot, SkillDiffEntry};
        let config = RuntimeConfig::default();
        let diff = SkillDiffEntry {
            skill: "pipeline.tool_selection".to_string(),
            before: DiffSnapshot::default(),
            after: DiffSnapshot {
                blocked_tools: vec!["flaky_http".to_string()],
                boosted_tools: vec![],
                widen_pending: true,
            },
            reason: "auto-reflection".to_string(),
        };
        let model = SelfModel::snapshot(
            &["bash"],
            &[],
            &[],
            &[],
            None,
            1,
            None,
            None,
            None,
            1,
            0,
            0,
            None,
            None,
            None,
            None,
            None,
            &[],
            &config,
        )
        .with_skill_diff(diff);
        let rendered = model.to_system_prompt_section();
        assert!(rendered.contains("Strategy diff:"), "got: {rendered}");
        assert!(rendered.contains("flaky_http"), "got: {rendered}");
        assert!(rendered.contains("+widen"), "got: {rendered}");
    }

    #[test]
    fn snapshot_without_strategy_omits_boost_and_widen_lines() {
        let config = RuntimeConfig::default();
        let model = SelfModel::snapshot(
            &["bash"],
            &[],
            &[],
            &[],
            None,
            1,
            None,
            None,
            None,
            1,
            0,
            0,
            None,
            None,
            None,
            None,
            None,
            &[],
            &config,
        );
        assert!(model.capabilities.boosted_tools.is_empty());
        assert!(!model.capabilities.widen_selection_pending);
        let rendered = model.to_system_prompt_section();
        assert!(!rendered.contains("Boosted tools"), "got: {rendered}");
        assert!(
            !rendered.contains("widened for next turn"),
            "got: {rendered}"
        );
    }

    #[test]
    fn snapshot_without_guardrail_omits_line() {
        let config = RuntimeConfig::default();
        let model = SelfModel::snapshot(
            &["bash"],
            &[],
            &[],
            &[],
            None,
            1,
            None,
            None,
            None,
            10,
            0,
            0,
            None,
            None,
            None,
            None,
            None,
            &[],
            &config,
        );
        assert!(model.guardrail.is_none());
        let rendered = model.to_system_prompt_section();
        assert!(!rendered.contains("Guardrail:"), "got: {rendered}");
    }

    #[test]
    fn snapshot_with_guardrail_renders_threshold_line() {
        let config = RuntimeConfig::default();
        let model = SelfModel::snapshot(
            &["bash"],
            &[],
            &[],
            &[],
            None,
            6,
            None,
            None,
            None,
            60,
            0,
            0,
            None,
            None,
            None,
            None,
            None,
            &[],
            &config,
        )
        .with_guardrail(GuardrailView {
            reflection_threshold: 2,
            last_delta: -1,
            recent_fail_rate: Some(0.5),
            turns_observed: 10,
        });
        let rendered = model.to_system_prompt_section();
        assert!(
            rendered.contains("Guardrail: reflection triggers after 2 signals"),
            "got: {rendered}"
        );
        assert!(
            rendered.contains("reacting faster"),
            "delta tag missing: {rendered}"
        );
        assert!(rendered.contains("50% over 10 turns"), "got: {rendered}");
    }

    #[test]
    fn snapshot_with_guardrail_warming_up_renders_no_rate() {
        let config = RuntimeConfig::default();
        let model = SelfModel::snapshot(
            &["bash"],
            &[],
            &[],
            &[],
            None,
            2,
            None,
            None,
            None,
            20,
            0,
            0,
            None,
            None,
            None,
            None,
            None,
            &[],
            &config,
        )
        .with_guardrail(GuardrailView {
            reflection_threshold: 3,
            last_delta: 0,
            recent_fail_rate: None,
            turns_observed: 2,
        });
        let rendered = model.to_system_prompt_section();
        assert!(
            rendered.contains("warming up (2 turns observed)"),
            "got: {rendered}"
        );
        assert!(!rendered.contains("%"), "got: {rendered}");
    }

    fn minimal_model() -> SelfModel {
        let config = RuntimeConfig::default();
        SelfModel::snapshot(
            &["bash"],
            &[],
            &[],
            &[],
            None,
            0,
            None,
            None,
            None,
            0,
            0,
            0,
            None,
            None,
            None,
            None,
            None,
            &[],
            &config,
        )
    }

    #[test]
    fn denial_pressure_omitted_when_zero() {
        let model = minimal_model().with_denial_pressure(DenialPressureView {
            total_denials: 0,
            max_total: 20,
        });
        let rendered = model.to_system_prompt_section();
        assert!(
            !rendered.contains("Denial pressure"),
            "should not render zero-denial state, got: {rendered}"
        );
    }

    #[test]
    fn denial_pressure_low_renders_plain_count() {
        let model = minimal_model().with_denial_pressure(DenialPressureView {
            total_denials: 2,
            max_total: 20,
        });
        let rendered = model.to_system_prompt_section();
        assert!(
            rendered.contains("Denial pressure: 2/20 denies this session"),
            "got: {rendered}"
        );
        assert!(!rendered.contains("elevated"), "got: {rendered}");
        assert!(!rendered.contains("APPROACHING"), "got: {rendered}");
    }

    #[test]
    fn denial_pressure_medium_renders_elevated_warning() {
        let model = minimal_model().with_denial_pressure(DenialPressureView {
            total_denials: 12,
            max_total: 20,
        });
        let rendered = model.to_system_prompt_section();
        assert!(
            rendered.contains("Denial pressure: 12/20"),
            "got: {rendered}"
        );
        assert!(rendered.contains("elevated"), "got: {rendered}");
    }

    #[test]
    fn denial_pressure_high_renders_hard_fallback_warning() {
        let model = minimal_model().with_denial_pressure(DenialPressureView {
            total_denials: 17,
            max_total: 20,
        });
        let rendered = model.to_system_prompt_section();
        assert!(
            rendered.contains("APPROACHING HARD FALLBACK"),
            "got: {rendered}"
        );
    }

    #[test]
    fn denial_pressure_unknown_ceiling_renders_count_only() {
        let model = minimal_model().with_denial_pressure(DenialPressureView {
            total_denials: 5,
            max_total: 0,
        });
        let rendered = model.to_system_prompt_section();
        assert!(
            rendered.contains("Denial pressure: 5 denies this session"),
            "got: {rendered}"
        );
        assert!(
            !rendered.contains("Denial pressure: 5/"),
            "should not render ceiling slash when max_total=0, got: {rendered}"
        );
    }

    #[test]
    fn denial_pressure_view_ratio() {
        assert_eq!(
            DenialPressureView {
                total_denials: 0,
                max_total: 0
            }
            .pressure(),
            0.0
        );
        assert_eq!(
            DenialPressureView {
                total_denials: 10,
                max_total: 20
            }
            .pressure(),
            0.5
        );
        assert_eq!(
            DenialPressureView {
                total_denials: 30,
                max_total: 20
            }
            .pressure(),
            1.0
        );
    }

    #[test]
    fn recent_failing_tests_renders_compact_list() {
        let model = minimal_model().with_recent_failing_tests(vec![
            "tests::parses_json".into(),
            "tests::handles_empty".into(),
        ]);
        let rendered = model.to_system_prompt_section();
        assert!(
            rendered.contains("Recent test failures: tests::parses_json, tests::handles_empty"),
            "got: {rendered}"
        );
        assert!(
            !rendered.contains("more"),
            "no +N more when list is short: {rendered}"
        );
    }

    #[test]
    fn recent_failing_tests_truncates_and_counts_overflow() {
        let tests: Vec<String> = (0..8).map(|i| format!("t{i}")).collect();
        let model = minimal_model().with_recent_failing_tests(tests);
        let rendered = model.to_system_prompt_section();
        assert!(rendered.contains("t0, t1, t2, t3, t4"), "got: {rendered}");
        assert!(rendered.contains("(+3 more)"), "got: {rendered}");
    }

    #[test]
    fn recent_failing_tests_empty_renders_nothing() {
        let model = minimal_model().with_recent_failing_tests(Vec::new());
        let rendered = model.to_system_prompt_section();
        assert!(
            !rendered.contains("Recent test failures"),
            "got: {rendered}"
        );
    }

    #[test]
    fn recent_rejections_renders_reasons() {
        let model = minimal_model().with_recent_rejections(vec![
            RejectionSummary {
                tool: "bash".into(),
                reason: "sandbox: write outside workspace".into(),
            },
            RejectionSummary {
                tool: "write_file".into(),
                reason: "denied path".into(),
            },
        ]);
        let rendered = model.to_system_prompt_section();
        assert!(
            rendered.contains("Recent rejections: bash (sandbox: write outside workspace); write_file (denied path)"),
            "got: {rendered}"
        );
    }

    #[test]
    fn recent_rejections_empty_renders_nothing() {
        let model = minimal_model().with_recent_rejections(Vec::new());
        let rendered = model.to_system_prompt_section();
        assert!(!rendered.contains("Recent rejections"), "got: {rendered}");
    }

    #[test]
    fn correction_excerpts_render_only_when_correction_count_positive() {
        // With zero corrections, excerpts are suppressed even if provided
        // (the count line itself is gated).
        let model = minimal_model()
            .with_recent_correction_excerpts(vec!["no I meant read the file".into()]);
        let rendered = model.to_system_prompt_section();
        assert!(!rendered.contains("Recent corrections"), "got: {rendered}");
    }

    #[test]
    fn correction_excerpts_render_most_recent_first() {
        let config = RuntimeConfig::default();
        let model = SelfModel::snapshot(
            &["bash"],
            &[],
            &[],
            &[],
            None,
            0,
            None,
            None,
            None,
            0,
            2, // correction_count > 0
            0,
            None,
            None,
            None,
            None,
            None,
            &[],
            &config,
        )
        .with_recent_correction_excerpts(vec![
            "first correction".into(),
            "second correction".into(),
        ]);
        let rendered = model.to_system_prompt_section();
        assert!(rendered.contains("User corrections: 2"), "got: {rendered}");
        // Most-recent-first ordering
        let pos_first = rendered.find("first correction");
        let pos_second = rendered.find("second correction");
        assert!(
            pos_second.is_some() && pos_first.is_some(),
            "got: {rendered}"
        );
        assert!(
            pos_second < pos_first,
            "second (most recent) should render first: {rendered}"
        );
    }

    #[test]
    fn outcome_bias_renders_sorted_by_magnitude() {
        let mut bias = std::collections::BTreeMap::new();
        bias.insert("bash".to_string(), -0.10);
        bias.insert("write_file".to_string(), 0.08);
        bias.insert("read_file".to_string(), 0.02);
        let model = minimal_model().with_outcome_bias(bias);
        let rendered = model.to_system_prompt_section();
        assert!(
            rendered.contains("Tool outcome bias (selector-applied)"),
            "got: {rendered}"
        );
        let line = rendered
            .lines()
            .find(|l| l.starts_with("Tool outcome bias"))
            .unwrap();
        let pos_bash = line.find("bash").unwrap();
        let pos_write = line.find("write_file").unwrap();
        let pos_read = line.find("read_file").unwrap();
        assert!(pos_bash < pos_write, "|0.10| > |0.08|: {line}");
        assert!(pos_write < pos_read, "|0.08| > |0.02|: {line}");
        assert!(line.contains("↓0.10"), "got: {line}");
        assert!(line.contains("↑0.08"), "got: {line}");
    }

    #[test]
    fn outcome_bias_filters_near_zero_entries() {
        let mut bias = std::collections::BTreeMap::new();
        bias.insert("noise".to_string(), 0.001);
        let model = minimal_model().with_outcome_bias(bias);
        let rendered = model.to_system_prompt_section();
        assert!(
            !rendered.contains("Tool outcome bias"),
            "sub-threshold entries should be filtered: {rendered}"
        );
    }

    #[test]
    fn outcome_bias_empty_renders_nothing() {
        let model = minimal_model();
        let rendered = model.to_system_prompt_section();
        assert!(!rendered.contains("Tool outcome bias"), "got: {rendered}");
    }

    #[test]
    fn low_confidence_tools_rendered_sorted_by_fail_rate_desc() {
        let model = minimal_model().with_low_confidence_tools(vec![
            LowConfidenceTool {
                name: "bash".into(),
                fail_rate: 0.3,
                samples: 10,
            },
            LowConfidenceTool {
                name: "write_file".into(),
                fail_rate: 0.75,
                samples: 8,
            },
            LowConfidenceTool {
                name: "grep".into(),
                fail_rate: 0.5,
                samples: 15,
            },
        ]);
        let rendered = model.to_system_prompt_section();
        assert!(rendered.contains("high-failure tools"));
        let pos_write = rendered.find("write_file").unwrap();
        let pos_grep = rendered.find("grep").unwrap();
        let pos_bash = rendered.find("bash").unwrap();
        assert!(pos_write < pos_grep, "write_file should appear before grep");
        assert!(pos_grep < pos_bash, "grep should appear before bash");
    }

    #[test]
    fn low_confidence_tools_empty_produces_no_output() {
        let model = minimal_model().with_low_confidence_tools(vec![]);
        let rendered = model.to_system_prompt_section();
        assert!(
            !rendered.contains("high-failure"),
            "should not render section when empty"
        );
    }
}
