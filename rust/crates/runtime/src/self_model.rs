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
        // ── Capabilities ──
        let mut tool_health_summaries = Vec::new();
        let mut deprioritized = Vec::new();

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
        }

        for tool in manual_deprioritized_tools {
            if !deprioritized.contains(tool) {
                deprioritized.push(tool.clone());
            }
        }
        deprioritized.sort();

        let capabilities = CapabilityView {
            total_tools: tool_names.len(),
            tool_names: tool_names.iter().map(|s| s.to_string()).collect(),
            tool_health: tool_health_summaries,
            deprioritized_tools: deprioritized,
            pinned_tools: pinned_tools.to_vec(),
            skills: skills.to_vec(),
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
        }
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
}
