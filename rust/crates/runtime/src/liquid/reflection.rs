//! LLM-native self-reflection engine for the liquid pipeline.
//!
//! The reflection engine converts execution traces + signal history into a
//! compact context, prompts an LLM for structured improvement proposals, and
//! funnels those proposals into the existing evolution service.
//!
//! Design decisions:
//! - Uses the same `EvolutionProposal` / `EvolutionAxis` types so proposals
//!   flow through the existing approval pipeline.
//! - Keeps the LLM prompt small (~2K tokens) to stay within budget even for
//!   real-time triggers.
//! - All reflection is async and non-blocking — it never stalls the main loop.

use astra_services::self_surface::{EvolutionRecord, StepRecord, VerificationEventView};
use astra_services::session_journal::ToolCallRecord;
use serde::{Deserialize, Serialize};

use crate::evolution::types::{
    ApprovalStatus, CalibrationAxis, EvolutionAxis, EvolutionProposal, EvolutionSignal,
    PatternAction, SkillDiff, SkillSection,
};
use crate::pipeline::routing::{DomainHint, TaskType};

// ───────────────────────────────────────────────────────────────────────────
// L2.1 — ReflectionContext
// ───────────────────────────────────────────────────────────────────────────

/// A snapshot of recent execution state, assembled once per reflection cycle.
#[derive(Debug, Clone, Serialize)]
pub struct ReflectionContext {
    /// Session identifier for attribution.
    pub session_id: String,
    /// Number of turns executed so far.
    pub turns_completed: u32,
    /// Detected scenario (e.g., Debugging, CodeReview).
    pub scenario: Option<String>,
    /// Recent LLM-requiring evolution signals (the raw material for reflection).
    pub signals: Vec<SignalSummary>,
    /// Active experiment info (if any).
    pub active_experiment: Option<ExperimentSummary>,
    /// Recent tool usage statistics.
    pub tool_stats: Vec<ToolStat>,
    /// Token budget utilisation (0.0–1.0).
    pub token_utilisation: f64,
    /// Recent tactical actions taken by the step-level adapter.
    pub recent_tactical_actions: Vec<String>,
    /// Distilled goal / steering state for this reflection window.
    pub goal: Option<GoalSummary>,
    /// Distilled verification / acceptance state for this reflection window.
    pub verification: Option<VerificationSummary>,
    /// Distilled tool-health / risk state for this reflection window.
    pub health: Option<HealthSummary>,
    /// Recent tool-performance deltas comparing newer vs older recent-step windows.
    pub recent_performance_deltas: Vec<ReflectionEventSummary>,
    /// Recent tool-performance changes attributed to nearby adaptation windows.
    pub recent_adaptation_impacts: Vec<ReflectionEventSummary>,
    /// Recent verification-pressure changes attributed to nearby adaptation windows.
    pub recent_adaptation_verification_impacts: Vec<ReflectionEventSummary>,
    /// Recent steering / verification events that explain what changed.
    pub recent_evaluation_events: Vec<ReflectionEventSummary>,
    /// Recent adaptive / mutation records that explain what the system changed.
    pub recent_adaptations: Vec<ReflectionEventSummary>,
    /// Recent outcome signals observed after those adaptations landed.
    pub recent_adaptation_outcomes: Vec<ReflectionEventSummary>,
}

/// Compressed representation of an EvolutionSignal for the LLM prompt.
#[derive(Debug, Clone, Serialize)]
pub struct SignalSummary {
    pub kind: String,
    pub detail: String,
    pub skill_context: Option<String>,
    pub turn_id: String,
}

/// Active A/B experiment summary.
#[derive(Debug, Clone, Serialize)]
pub struct ExperimentSummary {
    pub experiment_id: String,
    pub variant: String,
    pub samples: u32,
}

/// Goal / steering summary for the reflection window.
#[derive(Debug, Clone, Serialize)]
pub struct GoalSummary {
    pub effective_goal: String,
    pub goal_source: String,
    pub tracking_status: String,
    pub progress_summary: Option<String>,
}

/// Verification / acceptance summary for the reflection window.
#[derive(Debug, Clone, Serialize)]
pub struct VerificationSummary {
    pub ok: bool,
    pub acceptance_ok: bool,
    pub objective_ok: bool,
    pub summary: String,
    pub pending_blockers: Vec<String>,
    pub latest_verification: Option<String>,
}

/// Tool-health / risk summary for the reflection window.
#[derive(Debug, Clone, Serialize)]
pub struct HealthSummary {
    pub risk_flags: Vec<String>,
    pub blocked_tools: Vec<String>,
    pub hotspots: Vec<String>,
    pub recent_failures: Vec<String>,
}

/// Compact steering / verification timeline entry.
#[derive(Debug, Clone, Serialize)]
pub struct ReflectionEventSummary {
    pub kind: String,
    pub turn: Option<u32>,
    pub detail: String,
}

/// Per-tool aggregate statistics for the reflection window.
#[derive(Debug, Clone, Serialize)]
pub struct ToolStat {
    pub tool_name: String,
    pub calls: u32,
    pub failures: u32,
    pub avg_latency_ms: u64,
}

impl ToolStat {
    /// Aggregate raw tool call records into a compact per-tool summary.
    pub fn summarize_records(records: &[ToolCallRecord], max_tools: usize) -> Vec<Self> {
        if max_tools == 0 || records.is_empty() {
            return Vec::new();
        }

        let mut by_tool: std::collections::HashMap<String, (u32, u32, u64)> =
            std::collections::HashMap::new();
        for record in records {
            let entry = by_tool.entry(record.name.clone()).or_insert((0, 0, 0));
            entry.0 += 1;
            if !record.ok {
                entry.1 += 1;
            }
            entry.2 += record.ms;
        }

        let mut stats: Vec<Self> = by_tool
            .into_iter()
            .map(|(tool_name, (calls, failures, total_latency_ms))| Self {
                tool_name,
                calls,
                failures,
                avg_latency_ms: if calls > 0 {
                    total_latency_ms / calls as u64
                } else {
                    0
                },
            })
            .collect();
        stats.sort_by(|a, b| {
            b.failures
                .cmp(&a.failures)
                .then_with(|| b.calls.cmp(&a.calls))
                .then_with(|| b.avg_latency_ms.cmp(&a.avg_latency_ms))
                .then_with(|| a.tool_name.cmp(&b.tool_name))
        });
        stats.truncate(max_tools);
        stats
    }
}

#[derive(Default)]
struct PerformanceDeltaWindow {
    calls: u32,
    failures: u32,
    total_latency_ms: u64,
    latest_turn: Option<u32>,
}

impl PerformanceDeltaWindow {
    fn record(&mut self, turn: Option<u32>, ok: bool, latency_ms: u64) {
        self.calls += 1;
        if !ok {
            self.failures += 1;
        }
        self.total_latency_ms += latency_ms;
        if self.latest_turn.is_none() {
            self.latest_turn = turn;
        }
    }

    fn success_rate(&self) -> f64 {
        if self.calls == 0 {
            1.0
        } else {
            (self.calls.saturating_sub(self.failures)) as f64 / self.calls as f64
        }
    }

    fn avg_latency_ms(&self) -> u64 {
        if self.calls == 0 {
            0
        } else {
            self.total_latency_ms / self.calls as u64
        }
    }
}

struct PerformanceDeltaCandidate {
    kind: &'static str,
    turn: Option<u32>,
    detail: String,
    score: u64,
}

#[derive(Default)]
struct VerificationDeltaWindow {
    events: u32,
    passed: u32,
    latest_turn: Option<u32>,
}

impl VerificationDeltaWindow {
    fn record(&mut self, turn: Option<u32>, passed: bool) {
        self.events += 1;
        if passed {
            self.passed += 1;
        }
        if self.latest_turn.is_none() {
            self.latest_turn = turn;
        }
    }

    fn pass_rate(&self) -> f64 {
        if self.events == 0 {
            1.0
        } else {
            self.passed as f64 / self.events as f64
        }
    }
}

pub fn summarize_recent_performance_deltas(
    steps: &[StepRecord],
    max_events: usize,
) -> Vec<ReflectionEventSummary> {
    const MIN_TOOL_STEPS: usize = 4;

    if max_events == 0 {
        return Vec::new();
    }

    let tool_steps = tool_steps_with_calls(steps);
    if tool_steps.len() < MIN_TOOL_STEPS {
        return Vec::new();
    }

    let split = tool_steps.len() / 2;
    if split == 0 || split >= tool_steps.len() {
        return Vec::new();
    }

    summarize_performance_delta_windows(&tool_steps[..split], &tool_steps[split..], max_events)
}

pub fn summarize_recent_adaptation_impacts(
    steps: &[StepRecord],
    records: &[EvolutionRecord],
    max_events: usize,
) -> Vec<ReflectionEventSummary> {
    const MIN_TOOL_STEPS: usize = 4;
    const ADAPTATION_WINDOW_STEP_LIMIT: usize = 3;

    if max_events == 0 {
        return Vec::new();
    }

    let tool_steps = tool_steps_with_calls(steps);
    if tool_steps.len() < MIN_TOOL_STEPS {
        return Vec::new();
    }

    let adaptations = reflection_adaptations_with_turn(records);
    let mut impacts = Vec::new();

    for (index, adaptation) in adaptations.iter().enumerate() {
        let current_turn = adaptation.turn.expect("turn checked above");
        let newer_bound = index
            .checked_sub(1)
            .and_then(|previous| adaptations.get(previous))
            .and_then(|record| record.turn);
        let older_bound = adaptations.get(index + 1).and_then(|record| record.turn);

        let newer_steps = tool_steps
            .iter()
            .copied()
            .filter(|step| {
                step.turn.is_some_and(|turn| {
                    turn >= current_turn && newer_bound.map_or(true, |upper| turn < upper)
                })
            })
            .take(ADAPTATION_WINDOW_STEP_LIMIT)
            .collect::<Vec<_>>();
        let older_steps = tool_steps
            .iter()
            .copied()
            .filter(|step| {
                step.turn.is_some_and(|turn| {
                    turn < current_turn && older_bound.map_or(true, |lower| turn >= lower)
                })
            })
            .take(ADAPTATION_WINDOW_STEP_LIMIT)
            .collect::<Vec<_>>();

        if let Some(delta) = summarize_performance_delta_windows(&newer_steps, &older_steps, 1)
            .into_iter()
            .next()
        {
            impacts.push(ReflectionEventSummary {
                kind: delta.kind,
                turn: adaptation.turn.or(delta.turn),
                detail: reflection_adaptation_impact_detail(adaptation, &delta.detail),
            });
        }
        if impacts.len() >= max_events {
            break;
        }
    }

    impacts
}

pub fn summarize_recent_adaptation_verification_impacts(
    verification_events: &[VerificationEventView],
    records: &[EvolutionRecord],
    max_events: usize,
) -> Vec<ReflectionEventSummary> {
    const MIN_VERIFICATION_EVENTS: usize = 2;
    const ADAPTATION_WINDOW_VERIFICATION_LIMIT: usize = 2;

    if max_events == 0 {
        return Vec::new();
    }

    let verification_events = verification_events_with_outcomes(verification_events);
    if verification_events.len() < MIN_VERIFICATION_EVENTS {
        return Vec::new();
    }

    let adaptations = reflection_adaptations_with_turn(records);
    let mut impacts = Vec::new();

    for (index, adaptation) in adaptations.iter().enumerate() {
        let current_turn = adaptation.turn.expect("turn checked above");
        let newer_bound = index
            .checked_sub(1)
            .and_then(|previous| adaptations.get(previous))
            .and_then(|record| record.turn);
        let older_bound = adaptations.get(index + 1).and_then(|record| record.turn);

        let newer_events = verification_events
            .iter()
            .copied()
            .filter(|event| {
                event.turn.is_some_and(|turn| {
                    turn >= current_turn && newer_bound.map_or(true, |upper| turn < upper)
                })
            })
            .take(ADAPTATION_WINDOW_VERIFICATION_LIMIT)
            .collect::<Vec<_>>();
        let older_events = verification_events
            .iter()
            .copied()
            .filter(|event| {
                event.turn.is_some_and(|turn| {
                    turn < current_turn && older_bound.map_or(true, |lower| turn >= lower)
                })
            })
            .take(ADAPTATION_WINDOW_VERIFICATION_LIMIT)
            .collect::<Vec<_>>();

        if let Some(delta) = summarize_verification_delta_windows(&newer_events, &older_events)
            .into_iter()
            .next()
        {
            impacts.push(ReflectionEventSummary {
                kind: delta.kind,
                turn: adaptation.turn.or(delta.turn),
                detail: reflection_adaptation_impact_detail(adaptation, &delta.detail),
            });
        }
        if impacts.len() >= max_events {
            break;
        }
    }

    impacts
}

fn summarize_performance_delta_windows(
    newer_steps: &[&StepRecord],
    older_steps: &[&StepRecord],
    max_events: usize,
) -> Vec<ReflectionEventSummary> {
    const MIN_COMBINED_CALLS: u32 = 3;
    const MIN_SUCCESS_RATE_DELTA: f64 = 0.34;
    const MIN_LATENCY_DELTA_MS: u64 = 100;
    const MIN_LATENCY_DELTA_RATIO: f64 = 0.25;

    if max_events == 0 || newer_steps.is_empty() || older_steps.is_empty() {
        return Vec::new();
    }

    let newer = summarize_performance_window(newer_steps);
    let older = summarize_performance_window(older_steps);
    let mut candidates = Vec::new();

    for (tool, newer_window) in &newer {
        let Some(older_window) = older.get(tool) else {
            continue;
        };
        if newer_window.calls + older_window.calls < MIN_COMBINED_CALLS {
            continue;
        }

        let older_success = older_window.success_rate();
        let newer_success = newer_window.success_rate();
        let success_delta = newer_success - older_success;
        if success_delta.abs() >= MIN_SUCCESS_RATE_DELTA {
            let kind = if success_delta.is_sign_positive() {
                "Improved"
            } else {
                "Regressed"
            };
            let score =
                (success_delta.abs() * 1000.0).round() as u64 + u64::from(newer_window.calls);
            candidates.push(PerformanceDeltaCandidate {
                kind,
                turn: newer_window.latest_turn.or(older_window.latest_turn),
                detail: format!(
                    "{} success {:.0}% -> {:.0}% (recent={} calls, prior={})",
                    tool,
                    older_success * 100.0,
                    newer_success * 100.0,
                    newer_window.calls,
                    older_window.calls
                ),
                score,
            });
        }

        let older_latency = older_window.avg_latency_ms();
        let newer_latency = newer_window.avg_latency_ms();
        let latency_delta_ms = newer_latency.abs_diff(older_latency);
        let latency_delta_ratio = latency_delta_ms as f64 / older_latency.max(1) as f64;
        if older_latency > 0
            && newer_latency > 0
            && latency_delta_ms >= MIN_LATENCY_DELTA_MS
            && latency_delta_ratio >= MIN_LATENCY_DELTA_RATIO
        {
            let kind = if newer_latency < older_latency {
                "Improved"
            } else {
                "Regressed"
            };
            let score = latency_delta_ms + (latency_delta_ratio * 100.0).round() as u64;
            candidates.push(PerformanceDeltaCandidate {
                kind,
                turn: newer_window.latest_turn.or(older_window.latest_turn),
                detail: format!(
                    "{} latency {}ms -> {}ms (recent={} calls, prior={})",
                    tool, older_latency, newer_latency, newer_window.calls, older_window.calls
                ),
                score,
            });
        }
    }

    candidates.sort_by(|a, b| {
        b.score
            .cmp(&a.score)
            .then_with(|| {
                performance_delta_kind_priority(b.kind)
                    .cmp(&performance_delta_kind_priority(a.kind))
            })
            .then_with(|| a.detail.cmp(&b.detail))
    });
    candidates.truncate(max_events);
    candidates
        .into_iter()
        .map(|candidate| ReflectionEventSummary {
            kind: candidate.kind.into(),
            turn: candidate.turn,
            detail: candidate.detail,
        })
        .collect()
}

fn summarize_verification_delta_windows(
    newer_events: &[&VerificationEventView],
    older_events: &[&VerificationEventView],
) -> Vec<ReflectionEventSummary> {
    const MIN_COMBINED_EVENTS: u32 = 2;
    const MIN_PASS_RATE_DELTA: f64 = 0.5;

    if newer_events.is_empty() || older_events.is_empty() {
        return Vec::new();
    }

    let newer = summarize_verification_window(newer_events);
    let older = summarize_verification_window(older_events);
    if newer.events + older.events < MIN_COMBINED_EVENTS {
        return Vec::new();
    }

    let older_pass_rate = older.pass_rate();
    let newer_pass_rate = newer.pass_rate();
    let pass_rate_delta = newer_pass_rate - older_pass_rate;
    if pass_rate_delta.abs() < MIN_PASS_RATE_DELTA {
        return Vec::new();
    }

    vec![ReflectionEventSummary {
        kind: if pass_rate_delta.is_sign_positive() {
            "Improved".into()
        } else {
            "Regressed".into()
        },
        turn: newer.latest_turn.or(older.latest_turn),
        detail: format!(
            "verification pass rate {:.0}% -> {:.0}% (recent={} checks, prior={})",
            older_pass_rate * 100.0,
            newer_pass_rate * 100.0,
            newer.events,
            older.events
        ),
    }]
}

fn tool_steps_with_calls(steps: &[StepRecord]) -> Vec<&StepRecord> {
    steps
        .iter()
        .filter(|step| !step.tool_calls.is_empty())
        .collect::<Vec<_>>()
}

fn verification_events_with_outcomes(
    events: &[VerificationEventView],
) -> Vec<&VerificationEventView> {
    events
        .iter()
        .filter(|event| event.turn.is_some() && event.passed.is_some())
        .collect::<Vec<_>>()
}

fn reflection_adaptations_with_turn(records: &[EvolutionRecord]) -> Vec<&EvolutionRecord> {
    records
        .iter()
        .filter(|record| record.turn.is_some() && is_reflection_adaptation_record(record))
        .collect::<Vec<_>>()
}

fn reflection_adaptation_impact_detail(adaptation: &EvolutionRecord, detail: &str) -> String {
    match adaptation.turn {
        Some(turn) => format!(
            "after {} turn {} — {}",
            reflection_kind_label(&adaptation.kind),
            turn,
            detail
        ),
        None => format!(
            "after {} — {}",
            reflection_kind_label(&adaptation.kind),
            detail
        ),
    }
}

fn is_reflection_adaptation_record(record: &EvolutionRecord) -> bool {
    matches!(record.status.as_str(), "applied" | "enrolled" | "promoted")
        && !matches!(
            record.kind.as_str(),
            "verification" | "failure" | "stall" | "drift"
        )
}

fn reflection_kind_label(kind: &str) -> String {
    let mut chars = kind.chars();
    match chars.next() {
        Some(first) => format!("{}{}", first.to_ascii_uppercase(), chars.as_str()),
        None => "Event".to_string(),
    }
}

fn summarize_performance_window(
    steps: &[&StepRecord],
) -> std::collections::HashMap<String, PerformanceDeltaWindow> {
    let mut by_tool = std::collections::HashMap::new();
    for step in steps {
        for call in &step.tool_calls {
            by_tool
                .entry(call.name.clone())
                .or_insert_with(PerformanceDeltaWindow::default)
                .record(step.turn, call.ok, call.latency_ms);
        }
    }
    by_tool
}

fn summarize_verification_window(events: &[&VerificationEventView]) -> VerificationDeltaWindow {
    let mut window = VerificationDeltaWindow::default();
    for event in events {
        window.record(
            event.turn,
            event
                .passed
                .expect("verification event filtered to have outcome"),
        );
    }
    window
}

fn performance_delta_kind_priority(kind: &str) -> u8 {
    match kind {
        "Regressed" => 2,
        "Improved" => 1,
        _ => 0,
    }
}

impl ReflectionContext {
    /// Create an empty context for a given session.
    pub fn new(session_id: impl Into<String>) -> Self {
        Self {
            session_id: session_id.into(),
            turns_completed: 0,
            scenario: None,
            signals: Vec::new(),
            active_experiment: None,
            tool_stats: Vec::new(),
            token_utilisation: 0.0,
            recent_tactical_actions: Vec::new(),
            goal: None,
            verification: None,
            health: None,
            recent_performance_deltas: Vec::new(),
            recent_adaptation_impacts: Vec::new(),
            recent_adaptation_verification_impacts: Vec::new(),
            recent_evaluation_events: Vec::new(),
            recent_adaptations: Vec::new(),
            recent_adaptation_outcomes: Vec::new(),
        }
    }

    /// Populate signal summaries from raw EvolutionSignal list.
    pub fn add_signals(&mut self, signals: &[EvolutionSignal]) {
        for sig in signals {
            self.signals.push(SignalSummary::from_signal(sig));
        }
    }

    /// Render the context as a compact text prompt section.
    pub fn render_prompt_section(&self) -> String {
        let mut out = String::with_capacity(2048);
        out.push_str(&format!(
            "Session: {} | Turns: {} | Scenario: {}\n",
            self.session_id,
            self.turns_completed,
            self.scenario.as_deref().unwrap_or("unknown"),
        ));
        out.push_str(&format!(
            "Token utilisation: {:.0}%\n",
            self.token_utilisation * 100.0
        ));

        if let Some(ref exp) = self.active_experiment {
            out.push_str(&format!(
                "Active experiment: {} (variant={}, samples={})\n",
                exp.experiment_id, exp.variant, exp.samples
            ));
        }

        if let Some(ref goal) = self.goal {
            out.push_str(&format!(
                "Effective goal: {} (source={}, tracking={})\n",
                goal.effective_goal, goal.goal_source, goal.tracking_status
            ));
            if let Some(ref progress_summary) = goal.progress_summary {
                out.push_str(&format!("Goal progress: {progress_summary}\n"));
            }
        }

        if let Some(ref verification) = self.verification {
            out.push_str(&format!(
                "Verification: ok={} acceptance_ok={} objective_ok={}\n",
                verification.ok, verification.acceptance_ok, verification.objective_ok
            ));
            out.push_str(&format!("Verification summary: {}\n", verification.summary));
            if !verification.pending_blockers.is_empty() {
                out.push_str(&format!(
                    "Pending blockers: {}\n",
                    verification.pending_blockers.join("; ")
                ));
            }
            if let Some(ref latest) = verification.latest_verification {
                out.push_str(&format!("Latest verification: {latest}\n"));
            }
        }

        if let Some(ref health) = self.health {
            out.push_str("\nTool health:\n");
            if !health.risk_flags.is_empty() {
                out.push_str(&format!("  Risk flags: {}\n", health.risk_flags.join(", ")));
            }
            if !health.blocked_tools.is_empty() {
                out.push_str(&format!(
                    "  Blocked tools: {}\n",
                    health.blocked_tools.join(", ")
                ));
            }
            if !health.hotspots.is_empty() {
                out.push_str("  Hotspots:\n");
                for hotspot in &health.hotspots {
                    out.push_str(&format!("    - {hotspot}\n"));
                }
            }
            if !health.recent_failures.is_empty() {
                out.push_str("  Recent tool failures:\n");
                for failure in &health.recent_failures {
                    out.push_str(&format!("    - {failure}\n"));
                }
            }
        }

        if !self.recent_performance_deltas.is_empty() {
            out.push_str("\nRecent performance deltas:\n");
            for event in &self.recent_performance_deltas {
                match event.turn {
                    Some(turn) => out.push_str(&format!(
                        "  - turn {} [{}] {}\n",
                        turn, event.kind, event.detail
                    )),
                    None => out.push_str(&format!("  - [{}] {}\n", event.kind, event.detail)),
                }
            }
        }

        if !self.recent_evaluation_events.is_empty() {
            out.push_str("\nRecent evaluation events:\n");
            for event in &self.recent_evaluation_events {
                match event.turn {
                    Some(turn) => out.push_str(&format!(
                        "  - turn {} [{}] {}\n",
                        turn, event.kind, event.detail
                    )),
                    None => out.push_str(&format!("  - [{}] {}\n", event.kind, event.detail)),
                }
            }
        }

        if !self.recent_adaptations.is_empty() {
            out.push_str("\nRecent adaptations:\n");
            for event in &self.recent_adaptations {
                match event.turn {
                    Some(turn) => out.push_str(&format!(
                        "  - turn {} [{}] {}\n",
                        turn, event.kind, event.detail
                    )),
                    None => out.push_str(&format!("  - [{}] {}\n", event.kind, event.detail)),
                }
            }
        }

        if !self.recent_adaptation_outcomes.is_empty() {
            out.push_str("\nRecent adaptation outcomes:\n");
            for event in &self.recent_adaptation_outcomes {
                match event.turn {
                    Some(turn) => out.push_str(&format!(
                        "  - turn {} [{}] {}\n",
                        turn, event.kind, event.detail
                    )),
                    None => out.push_str(&format!("  - [{}] {}\n", event.kind, event.detail)),
                }
            }
        }

        if !self.recent_adaptation_impacts.is_empty() {
            out.push_str("\nRecent adaptation impacts:\n");
            for event in &self.recent_adaptation_impacts {
                match event.turn {
                    Some(turn) => out.push_str(&format!(
                        "  - turn {} [{}] {}\n",
                        turn, event.kind, event.detail
                    )),
                    None => out.push_str(&format!("  - [{}] {}\n", event.kind, event.detail)),
                }
            }
        }

        if !self.recent_adaptation_verification_impacts.is_empty() {
            out.push_str("\nRecent adaptation verification impacts:\n");
            for event in &self.recent_adaptation_verification_impacts {
                match event.turn {
                    Some(turn) => out.push_str(&format!(
                        "  - turn {} [{}] {}\n",
                        turn, event.kind, event.detail
                    )),
                    None => out.push_str(&format!("  - [{}] {}\n", event.kind, event.detail)),
                }
            }
        }

        if !self.tool_stats.is_empty() {
            out.push_str("\nTool statistics:\n");
            for ts in &self.tool_stats {
                out.push_str(&format!(
                    "  {} — calls={}, failures={}, avg_ms={}\n",
                    ts.tool_name, ts.calls, ts.failures, ts.avg_latency_ms
                ));
            }
        }

        if !self.signals.is_empty() {
            out.push_str("\nSignals requiring reflection:\n");
            for (i, sig) in self.signals.iter().enumerate() {
                out.push_str(&format!("  {}. [{}] {}", i + 1, sig.kind, sig.detail));
                if let Some(ref sk) = sig.skill_context {
                    out.push_str(&format!(" (skill: {})", sk));
                }
                out.push('\n');
            }
        }

        if !self.recent_tactical_actions.is_empty() {
            out.push_str("\nRecent tactical actions:\n");
            for a in &self.recent_tactical_actions {
                out.push_str(&format!("  - {}\n", a));
            }
        }

        out
    }
}

impl SignalSummary {
    pub fn from_signal(signal: &EvolutionSignal) -> Self {
        match signal {
            EvolutionSignal::ToolFailure {
                tool_name,
                error_snippet,
                failure_category: _,
                skill_context,
                turn_id,
            } => Self {
                kind: "ToolFailure".into(),
                detail: format!("{}: {}", tool_name, truncate(error_snippet, 120)),
                skill_context: skill_context.clone(),
                turn_id: turn_id.clone(),
            },
            EvolutionSignal::UserCorrection {
                correction_text,
                skill_context,
                turn_id,
                ..
            } => Self {
                kind: "UserCorrection".into(),
                detail: truncate(correction_text, 120),
                skill_context: skill_context.clone(),
                turn_id: turn_id.clone(),
            },
            EvolutionSignal::PatternDrift {
                pattern_signature,
                historical_rate,
                recent_rate,
                ..
            } => Self {
                kind: "PatternDrift".into(),
                detail: format!(
                    "{}: {:.0}% → {:.0}%",
                    pattern_signature,
                    historical_rate * 100.0,
                    recent_rate * 100.0
                ),
                skill_context: None,
                turn_id: String::new(),
            },
            EvolutionSignal::RepeatedStall {
                tool_chain,
                stall_count,
                turn_id,
            } => Self {
                kind: "RepeatedStall".into(),
                detail: format!("{}×: {}", stall_count, tool_chain.join(" → ")),
                skill_context: None,
                turn_id: turn_id.clone(),
            },
            EvolutionSignal::LlmReflection { context_id } => Self {
                kind: "LlmReflection".into(),
                detail: format!("reflection:{}", context_id),
                skill_context: None,
                turn_id: String::new(),
            },
        }
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        let end = s.floor_char_boundary(max);
        format!("{}…", &s[..end])
    }
}

// ───────────────────────────────────────────────────────────────────────────
// L2.2 — ReflectionEngine
// ───────────────────────────────────────────────────────────────────────────

/// A structured LLM reflection response.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ReflectionResponse {
    pub proposals: Vec<RawProposal>,
    pub summary: String,
}

/// A single improvement proposal from the LLM, in a parse-friendly format.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct RawProposal {
    /// Target axis: "skill", "pattern", "calibration", "entity".
    pub axis: String,
    /// Human-readable description of the proposed change.
    pub description: String,
    /// Confidence level (0.0–1.0).
    pub confidence: f64,
    /// Axis-specific details.
    #[serde(default)]
    pub details: serde_json::Value,
}

/// The reflection engine: takes a ReflectionContext, prompts an LLM, parses
/// the response into EvolutionProposal items.
pub struct ReflectionEngine {
    /// System prompt prefix for the reflection LLM call.
    system_prompt: String,
}

impl Default for ReflectionEngine {
    fn default() -> Self {
        Self::new()
    }
}

impl ReflectionEngine {
    pub fn new() -> Self {
        Self {
            system_prompt: DEFAULT_REFLECTION_SYSTEM_PROMPT.to_string(),
        }
    }

    /// Build the full prompt from a ReflectionContext.
    pub fn build_prompt(&self, ctx: &ReflectionContext) -> (String, String) {
        let system = self.system_prompt.clone();
        let user = format!(
            "Analyze the following execution context and propose improvements.\n\
             Respond with a JSON object: {{ \"proposals\": [...], \"summary\": \"...\" }}\n\n\
             {}",
            ctx.render_prompt_section()
        );
        (system, user)
    }

    /// Parse an LLM text response into a ReflectionResponse.
    /// Tolerant of markdown fences and partial JSON.
    pub fn parse_response(&self, text: &str) -> Result<ReflectionResponse, String> {
        // Strip markdown code fences if present
        let cleaned = text
            .trim()
            .strip_prefix("```json")
            .or_else(|| text.trim().strip_prefix("```"))
            .unwrap_or(text.trim());
        let cleaned = cleaned.strip_suffix("```").unwrap_or(cleaned).trim();

        serde_json::from_str(cleaned)
            .map_err(|e| format!("Failed to parse reflection response: {}", e))
    }

    /// Convert raw proposals into typed EvolutionProposals.
    pub fn convert_proposals(
        &self,
        raw: &[RawProposal],
        source_context: &ReflectionContext,
    ) -> Vec<EvolutionProposal> {
        raw.iter()
            .filter(|p| p.confidence >= 0.3) // minimum confidence threshold
            .filter_map(|p| self.convert_one(p, source_context))
            .collect()
    }

    fn convert_one(
        &self,
        raw: &RawProposal,
        _ctx: &ReflectionContext,
    ) -> Option<EvolutionProposal> {
        let axis = match raw.axis.to_lowercase().as_str() {
            "skill" => {
                let skill_name = raw
                    .details
                    .get("skill_name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown")
                    .to_string();
                let section_str = raw
                    .details
                    .get("section")
                    .and_then(|v| v.as_str())
                    .unwrap_or("troubleshooting");
                let section = match section_str {
                    "instructions" => SkillSection::Instructions,
                    "examples" => SkillSection::Examples,
                    _ => SkillSection::Troubleshooting,
                };
                let content = raw
                    .details
                    .get("content")
                    .and_then(|v| v.as_str())
                    .unwrap_or(&raw.description)
                    .to_string();
                EvolutionAxis::Skill {
                    skill_name,
                    section,
                    diff: SkillDiff::Append { content },
                }
            }
            "pattern" => {
                let signature = raw
                    .details
                    .get("signature")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown")
                    .to_string();
                let action_str = raw
                    .details
                    .get("action")
                    .and_then(|v| v.as_str())
                    .unwrap_or("demote");
                let action = match action_str {
                    "boost" => PatternAction::Boost,
                    "block" => PatternAction::Block,
                    _ => PatternAction::Demote,
                };
                EvolutionAxis::Pattern { signature, action }
            }
            "calibration" => {
                let axis_str = raw
                    .details
                    .get("axis")
                    .and_then(|v| v.as_str())
                    .unwrap_or("intent:general");
                let cal_axis = if let Some(intent) = axis_str.strip_prefix("intent:") {
                    CalibrationAxis::Intent(intent.to_string())
                } else if let Some(domain) = axis_str.strip_prefix("domain:") {
                    CalibrationAxis::Domain(parse_domain(domain))
                } else if let Some(task) = axis_str.strip_prefix("task:") {
                    CalibrationAxis::Task(parse_task_type(task))
                } else {
                    CalibrationAxis::Intent(axis_str.to_string())
                };
                let adjustment = raw
                    .details
                    .get("adjustment")
                    .and_then(|v| v.as_f64())
                    .unwrap_or(0.0)
                    .clamp(-0.5, 0.5);
                EvolutionAxis::Calibration {
                    axis: cal_axis,
                    adjustment,
                }
            }
            _ => return None,
        };

        Some(EvolutionProposal {
            id: format!("reflect-{:x}", {
                use std::hash::{Hash, Hasher};
                let mut h = std::collections::hash_map::DefaultHasher::new();
                raw.description.hash(&mut h);
                raw.axis.hash(&mut h);
                h.finish()
            }),
            signal: EvolutionSignal::LlmReflection {
                context_id: format!("reflect-{:x}", {
                    use std::hash::{Hash, Hasher};
                    let mut h = std::collections::hash_map::DefaultHasher::new();
                    raw.description.hash(&mut h);
                    h.finish()
                }),
            },
            axis,
            confidence: raw.confidence,
            reasoning: raw.description.clone(),
            created_at: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs(),
            status: ApprovalStatus::Pending,
            promotion_verdict: None,
        })
    }
}

fn parse_domain(s: &str) -> DomainHint {
    match s.to_lowercase().as_str() {
        "github" => DomainHint::GitHub,
        "git" => DomainHint::Git,
        "code" => DomainHint::Code,
        "memory" => DomainHint::Memory,
        "web" | "frontend" => DomainHint::Web,
        "system" | "systems" | "infra" | "infrastructure" => DomainHint::System,
        "database" | "data" | "db" => DomainHint::Database,
        _ => DomainHint::Code,
    }
}

fn parse_task_type(s: &str) -> TaskType {
    match s.to_lowercase().as_str() {
        "code" | "code_generation" | "generation" => TaskType::Code,
        "reasoning" | "analysis" | "debugging" | "debug" => TaskType::Reasoning,
        "fetch" | "exploration" | "explore" | "review" | "code_review" => TaskType::Fetch,
        "mutate" | "refactoring" | "refactor" => TaskType::Mutate,
        "memory" => TaskType::Memory,
        "conversational" | "documentation" | "docs" => TaskType::Conversational,
        "compound" | "testing" | "test" => TaskType::Compound,
        _ => TaskType::Unknown,
    }
}

const DEFAULT_REFLECTION_SYSTEM_PROMPT: &str = r#"You are an execution improvement advisor for an AI coding agent.

Your job is to analyze execution traces and propose concrete improvements.

Rules:
1. Only propose changes you are confident will improve execution quality.
2. Each proposal must target one axis: "skill", "pattern", "calibration", or "entity".
3. Skill proposals add troubleshooting notes or improved instructions.
4. Pattern proposals boost effective tool chains or demote failing ones.
5. Calibration proposals nudge scenario detection or domain classification thresholds.
6. Keep proposals minimal and actionable — avoid vague suggestions.
7. Set confidence 0.0–1.0 based on evidence strength.

Respond with a JSON object:
{
  "proposals": [
    {
      "axis": "skill|pattern|calibration",
      "description": "...",
      "confidence": 0.8,
      "details": { ... axis-specific fields ... }
    }
  ],
  "summary": "One-sentence summary of your analysis."
}

Skill details: { "skill_name": "...", "section": "instructions|examples|troubleshooting", "content": "..." }
Pattern details: { "signature": "tool1→tool2", "action": "boost|demote|block" }
Calibration details: { "axis": "intent:X|domain:Y|task:Z", "adjustment": -0.5..0.5 }
"#;

// ───────────────────────────────────────────────────────────────────────────
// Tests
// ───────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_signals() -> Vec<EvolutionSignal> {
        vec![
            EvolutionSignal::ToolFailure {
                tool_name: "bash".into(),
                error_snippet: "Permission denied: /etc/shadow".into(),
                failure_category: None,
                skill_context: Some("file-ops".into()),
                turn_id: "t1".into(),
            },
            EvolutionSignal::UserCorrection {
                correction_text: "Use sudo for that command".into(),
                prior_assistant_text: "cat /etc/shadow".into(),
                skill_context: Some("file-ops".into()),
                turn_id: "t2".into(),
                reason: None,
                apply_when: None,
            },
            EvolutionSignal::PatternDrift {
                pattern_signature: "bash→grep→sed".into(),
                task_type: TaskType::Reasoning,
                domain: Some(DomainHint::System),
                historical_rate: 0.85,
                recent_rate: 0.40,
            },
        ]
    }

    fn step_with_calls(turn: u32, calls: &[(&str, bool, u64)]) -> StepRecord {
        StepRecord {
            id: format!("step-{turn}"),
            turn: Some(turn),
            ts: format!("2026-01-01T00:00:{turn:02}Z"),
            event_type: "turn".into(),
            actor: "assistant".into(),
            phase: "execute".into(),
            summary: format!("turn {turn}"),
            selected_tools: calls
                .iter()
                .map(|(name, _, _)| (*name).to_string())
                .collect(),
            used_tools: calls
                .iter()
                .map(|(name, _, _)| (*name).to_string())
                .collect(),
            selected_skills: Vec::new(),
            tool_calls: calls
                .iter()
                .map(
                    |(name, ok, latency_ms)| astra_services::self_surface::ToolCallView {
                        name: (*name).to_string(),
                        ok: *ok,
                        latency_ms: *latency_ms,
                        error: (!ok).then(|| "tool error".to_string()),
                        args_preview: None,
                        result_preview: None,
                    },
                )
                .collect(),
            duration_ms: None,
            tokens_in: None,
            tokens_out: None,
            budget_pressure: None,
            error: None,
        }
    }

    fn adaptation_record(turn: u32, kind: &str) -> EvolutionRecord {
        EvolutionRecord {
            id: format!("evolution-{turn}"),
            turn: Some(turn),
            ts: format!("2026-01-01T00:01:{turn:02}Z"),
            kind: kind.into(),
            status: "applied".into(),
            summary: format!("{kind} applied"),
            evidence_refs: Vec::new(),
        }
    }

    fn verification_event(
        turn: u32,
        passed: bool,
        scope: &str,
        target: &str,
        summary: &str,
    ) -> VerificationEventView {
        VerificationEventView {
            ts: format!("2026-01-01T00:02:{turn:02}Z"),
            turn: Some(turn),
            scope: Some(scope.into()),
            target: Some(target.into()),
            passed: Some(passed),
            summary: summary.into(),
        }
    }

    #[test]
    fn context_render_includes_all_sections() {
        let mut ctx = ReflectionContext::new("test-session");
        ctx.turns_completed = 10;
        ctx.scenario = Some("Debugging".into());
        ctx.token_utilisation = 0.65;
        ctx.add_signals(&sample_signals());
        ctx.tool_stats.push(ToolStat {
            tool_name: "bash".into(),
            calls: 15,
            failures: 3,
            avg_latency_ms: 250,
        });
        ctx.health = Some(HealthSummary {
            risk_flags: vec!["recent_tool_failures".into()],
            blocked_tools: vec!["bash".into()],
            hotspots: vec!["bash success=80%, deprioritized".into()],
            recent_failures: vec!["turn 9 bash — permission denied".into()],
        });
        ctx.recent_performance_deltas = vec![ReflectionEventSummary {
            kind: "Improved".into(),
            turn: Some(9),
            detail: "bash success 0% -> 100% (recent=2 calls, prior=2)".into(),
        }];
        ctx.recent_adaptation_impacts = vec![ReflectionEventSummary {
            kind: "Improved".into(),
            turn: Some(9),
            detail:
                "after Adaptation turn 8 — bash latency 350ms -> 110ms (recent=2 calls, prior=2)"
                    .into(),
        }];
        ctx.recent_adaptation_verification_impacts = vec![ReflectionEventSummary {
            kind: "Regressed".into(),
            turn: Some(9),
            detail: "after Adaptation turn 8 — verification pass rate 100% -> 0% (recent=1 checks, prior=1)".into(),
        }];
        ctx.recent_tactical_actions
            .push("IncreaseVerification".into());

        let rendered = ctx.render_prompt_section();
        assert!(rendered.contains("test-session"));
        assert!(rendered.contains("Turns: 10"));
        assert!(rendered.contains("Debugging"));
        assert!(rendered.contains("65%"));
        assert!(rendered.contains("bash"));
        assert!(rendered.contains("ToolFailure"));
        assert!(rendered.contains("UserCorrection"));
        assert!(rendered.contains("PatternDrift"));
        assert!(rendered.contains("Tool health:"));
        assert!(rendered.contains("Blocked tools: bash"));
        assert!(rendered.contains("Recent performance deltas:"));
        assert!(rendered.contains("[Improved]"));
        assert!(rendered.contains("Recent adaptation impacts:"));
        assert!(rendered.contains("Recent adaptation verification impacts:"));
        assert!(rendered.contains("IncreaseVerification"));
    }

    #[test]
    fn summarize_records_aggregates_and_limits_tool_stats() {
        let records = vec![
            ToolCallRecord {
                name: "bash".into(),
                ok: false,
                ms: 200,
                error: Some("permission denied".into()),
                input_bytes: Some(10),
                output_bytes: Some(5),
                args_preview: None,
                result_preview: None,
            },
            ToolCallRecord {
                name: "bash".into(),
                ok: true,
                ms: 100,
                error: None,
                input_bytes: Some(8),
                output_bytes: Some(20),
                args_preview: None,
                result_preview: None,
            },
            ToolCallRecord {
                name: "web_fetch".into(),
                ok: false,
                ms: 50,
                error: Some("timeout".into()),
                input_bytes: Some(4),
                output_bytes: Some(0),
                args_preview: None,
                result_preview: None,
            },
        ];

        let stats = ToolStat::summarize_records(&records, 1);
        assert_eq!(stats.len(), 1);
        assert_eq!(stats[0].tool_name, "bash");
        assert_eq!(stats[0].calls, 2);
        assert_eq!(stats[0].failures, 1);
        assert_eq!(stats[0].avg_latency_ms, 150);
    }

    #[test]
    fn summarize_recent_performance_deltas_detects_improvements_and_regressions() {
        let steps = vec![
            step_with_calls(8, &[("bash", true, 100), ("rg", true, 500)]),
            step_with_calls(7, &[("bash", true, 120), ("rg", true, 450)]),
            step_with_calls(6, &[("bash", false, 400), ("rg", true, 180)]),
            step_with_calls(5, &[("bash", false, 300), ("rg", true, 160)]),
        ];

        let deltas = summarize_recent_performance_deltas(&steps, 4);
        assert_eq!(deltas.len(), 3);
        assert_eq!(deltas[0].kind, "Improved");
        assert!(deltas[0].detail.contains("bash success 0% -> 100%"));
        assert!(deltas.iter().any(|delta| {
            delta.kind == "Improved" && delta.detail.contains("bash latency 350ms -> 110ms")
        }));
        assert!(deltas.iter().any(|delta| {
            delta.kind == "Regressed" && delta.detail.contains("rg latency 170ms -> 475ms")
        }));
    }

    #[test]
    fn summarize_recent_adaptation_impacts_attributes_regression_to_adaptation_window() {
        let steps = vec![
            step_with_calls(8, &[("bash", false, 220)]),
            step_with_calls(7, &[("bash", false, 200)]),
            step_with_calls(6, &[("bash", true, 80)]),
            step_with_calls(5, &[("bash", true, 90)]),
        ];
        let records = vec![
            adaptation_record(7, "adaptation"),
            adaptation_record(4, "scenario"),
        ];

        let impacts = summarize_recent_adaptation_impacts(&steps, &records, 2);
        assert_eq!(impacts.len(), 1);
        assert_eq!(impacts[0].kind, "Regressed");
        assert!(impacts[0].detail.contains("after Adaptation turn 7"));
        assert!(impacts[0].detail.contains("bash success 100% -> 0%"));
    }

    #[test]
    fn summarize_recent_adaptation_verification_impacts_attributes_regression_to_adaptation_window()
    {
        let verification_events = vec![
            verification_event(
                8,
                false,
                "global",
                "subtask-1",
                "integration tests timed out",
            ),
            verification_event(7, true, "global", "subtask-1", "unit checks passed"),
        ];
        let records = vec![adaptation_record(8, "adaptation")];

        let impacts =
            summarize_recent_adaptation_verification_impacts(&verification_events, &records, 2);
        assert_eq!(impacts.len(), 1);
        assert_eq!(impacts[0].kind, "Regressed");
        assert!(impacts[0].detail.contains("after Adaptation turn 8"));
        assert!(
            impacts[0]
                .detail
                .contains("verification pass rate 100% -> 0%")
        );
    }

    #[test]
    fn signal_summary_from_all_variants() {
        let signals = sample_signals();
        for sig in &signals {
            let summary = SignalSummary::from_signal(sig);
            assert!(!summary.kind.is_empty());
            assert!(!summary.detail.is_empty());
        }

        let stall = EvolutionSignal::RepeatedStall {
            tool_chain: vec!["bash".into(), "grep".into()],
            stall_count: 3,
            turn_id: "t5".into(),
        };
        let s = SignalSummary::from_signal(&stall);
        assert_eq!(s.kind, "RepeatedStall");
        assert!(s.detail.contains("bash → grep"));
    }

    #[test]
    fn engine_build_prompt() {
        let engine = ReflectionEngine::new();
        let mut ctx = ReflectionContext::new("sess-1");
        ctx.goal = Some(GoalSummary {
            effective_goal: "stabilize the parser".into(),
            goal_source: "plan_goal".into(),
            tracking_status: "aligned".into(),
            progress_summary: Some("2/3 milestones complete".into()),
        });
        ctx.verification = Some(VerificationSummary {
            ok: false,
            acceptance_ok: true,
            objective_ok: false,
            summary: "objective blocked: integration tests pending".into(),
            pending_blockers: vec!["integration tests pending".into()],
            latest_verification: Some("turn 8: parser unit suite passed".into()),
        });
        ctx.health = Some(HealthSummary {
            risk_flags: vec!["recent_tool_failures".into(), "deprioritized_tools".into()],
            blocked_tools: vec!["bash".into()],
            hotspots: vec!["bash success=50%, deprioritized, consecutive_failures=1".into()],
            recent_failures: vec!["turn 9 bash — command timed out".into()],
        });
        ctx.recent_performance_deltas = vec![ReflectionEventSummary {
            kind: "Regressed".into(),
            turn: Some(9),
            detail: "bash success 100% -> 0% (recent=2 calls, prior=2)".into(),
        }];
        ctx.recent_adaptation_impacts = vec![ReflectionEventSummary {
            kind: "Regressed".into(),
            turn: Some(9),
            detail: "after Adaptation turn 6 — bash success 100% -> 0% (recent=2 calls, prior=2)"
                .into(),
        }];
        ctx.recent_adaptation_verification_impacts = vec![ReflectionEventSummary {
            kind: "Regressed".into(),
            turn: Some(9),
            detail:
                "after Adaptation turn 6 — verification pass rate 100% -> 0% (recent=1 checks, prior=1)"
                    .into(),
        }];
        ctx.recent_evaluation_events = vec![
            ReflectionEventSummary {
                kind: "GoalSteered".into(),
                turn: Some(7),
                detail: "plan_execution_start: fix auth -> stabilize the parser".into(),
            },
            ReflectionEventSummary {
                kind: "Verification".into(),
                turn: Some(8),
                detail: "failed global subtask-1 — parser integration timed out".into(),
            },
        ];
        ctx.recent_adaptations = vec![ReflectionEventSummary {
            kind: "Adaptation".into(),
            turn: Some(6),
            detail: "applied — adaptive per-turn changes applied".into(),
        }];
        ctx.recent_adaptation_outcomes = vec![ReflectionEventSummary {
            kind: "Verification".into(),
            turn: Some(7),
            detail: "after Adaptation turn 6 — verification failed global subtask-1 — parser integration timed out".into(),
        }];
        let (system, user) = engine.build_prompt(&ctx);
        assert!(system.contains("execution improvement advisor"));
        assert!(user.contains("Analyze the following"));
        assert!(user.contains("sess-1"));
        assert!(user.contains("Effective goal: stabilize the parser"));
        assert!(
            user.contains("Verification summary: objective blocked: integration tests pending")
        );
        assert!(user.contains("Tool health:"));
        assert!(user.contains("Blocked tools: bash"));
        assert!(user.contains("Recent performance deltas:"));
        assert!(user.contains("[Regressed]"));
        assert!(user.contains("Recent evaluation events:"));
        assert!(user.contains("[GoalSteered]"));
        assert!(user.contains("Recent adaptations:"));
        assert!(user.contains("[Adaptation]"));
        assert!(user.contains("Recent adaptation outcomes:"));
        assert!(user.contains("[Verification]"));
        assert!(user.contains("Recent adaptation impacts:"));
        assert!(user.contains("Recent adaptation verification impacts:"));
    }

    #[test]
    fn engine_parse_clean_json() {
        let engine = ReflectionEngine::new();
        let json = r#"{
            "proposals": [
                {
                    "axis": "pattern",
                    "description": "Demote bash→grep chain",
                    "confidence": 0.7,
                    "details": { "signature": "bash→grep", "action": "demote" }
                }
            ],
            "summary": "Tool chain underperforming."
        }"#;

        let resp = engine.parse_response(json).unwrap();
        assert_eq!(resp.proposals.len(), 1);
        assert_eq!(resp.proposals[0].axis, "pattern");
        assert_eq!(resp.summary, "Tool chain underperforming.");
    }

    #[test]
    fn engine_parse_markdown_fenced_json() {
        let engine = ReflectionEngine::new();
        let fenced = "```json\n{\"proposals\": [], \"summary\": \"Nothing to change.\"}\n```";
        let resp = engine.parse_response(fenced).unwrap();
        assert!(resp.proposals.is_empty());
    }

    #[test]
    fn engine_parse_bad_json_returns_error() {
        let engine = ReflectionEngine::new();
        let result = engine.parse_response("not json at all");
        assert!(result.is_err());
    }

    #[test]
    fn convert_skill_proposal() {
        let engine = ReflectionEngine::new();
        let raw = vec![RawProposal {
            axis: "skill".into(),
            description: "Add sudo hint to file-ops".into(),
            confidence: 0.8,
            details: serde_json::json!({
                "skill_name": "file-ops",
                "section": "troubleshooting",
                "content": "Use sudo for privileged files."
            }),
        }];
        let ctx = ReflectionContext::new("s1");
        let proposals = engine.convert_proposals(&raw, &ctx);
        assert_eq!(proposals.len(), 1);
        assert!(proposals[0].id.starts_with("reflect-"));
        assert!(proposals[0].confidence >= 0.3);
        match &proposals[0].axis {
            EvolutionAxis::Skill {
                skill_name,
                section,
                ..
            } => {
                assert_eq!(skill_name, "file-ops");
                assert_eq!(*section, SkillSection::Troubleshooting);
            }
            other => panic!("Expected Skill axis, got {:?}", other),
        }
    }

    #[test]
    fn convert_pattern_proposal() {
        let engine = ReflectionEngine::new();
        let raw = vec![RawProposal {
            axis: "pattern".into(),
            description: "Boost effective chain".into(),
            confidence: 0.9,
            details: serde_json::json!({
                "signature": "grep→sed",
                "action": "boost"
            }),
        }];
        let ctx = ReflectionContext::new("s1");
        let proposals = engine.convert_proposals(&raw, &ctx);
        assert_eq!(proposals.len(), 1);
        match &proposals[0].axis {
            EvolutionAxis::Pattern { signature, action } => {
                assert_eq!(signature, "grep→sed");
                assert_eq!(*action, PatternAction::Boost);
            }
            other => panic!("Expected Pattern axis, got {:?}", other),
        }
    }

    #[test]
    fn convert_calibration_proposal() {
        let engine = ReflectionEngine::new();
        let raw = vec![RawProposal {
            axis: "calibration".into(),
            description: "Nudge debugging detection".into(),
            confidence: 0.6,
            details: serde_json::json!({
                "axis": "task:debugging",
                "adjustment": 0.15
            }),
        }];
        let ctx = ReflectionContext::new("s1");
        let proposals = engine.convert_proposals(&raw, &ctx);
        assert_eq!(proposals.len(), 1);
        match &proposals[0].axis {
            EvolutionAxis::Calibration { axis, adjustment } => {
                assert!(matches!(axis, CalibrationAxis::Task(TaskType::Reasoning)));
                assert!((adjustment - 0.15).abs() < 0.001);
            }
            other => panic!("Expected Calibration axis, got {:?}", other),
        }
    }

    #[test]
    fn low_confidence_proposals_filtered() {
        let engine = ReflectionEngine::new();
        let raw = vec![
            RawProposal {
                axis: "pattern".into(),
                description: "Maybe demote".into(),
                confidence: 0.1, // below threshold
                details: serde_json::json!({"signature": "x", "action": "demote"}),
            },
            RawProposal {
                axis: "pattern".into(),
                description: "Definitely boost".into(),
                confidence: 0.9,
                details: serde_json::json!({"signature": "y", "action": "boost"}),
            },
        ];
        let ctx = ReflectionContext::new("s1");
        let proposals = engine.convert_proposals(&raw, &ctx);
        assert_eq!(proposals.len(), 1);
        assert_eq!(proposals[0].reasoning, "Definitely boost");
    }

    #[test]
    fn unknown_axis_skipped() {
        let engine = ReflectionEngine::new();
        let raw = vec![RawProposal {
            axis: "unknown_axis".into(),
            description: "Whatever".into(),
            confidence: 0.9,
            details: serde_json::json!({}),
        }];
        let ctx = ReflectionContext::new("s1");
        let proposals = engine.convert_proposals(&raw, &ctx);
        assert!(proposals.is_empty());
    }

    #[test]
    fn truncate_helper() {
        assert_eq!(truncate("short", 10), "short");
        assert_eq!(truncate("hello world!", 5), "hello…");
    }

    #[test]
    fn parse_domain_variants() {
        assert!(matches!(parse_domain("web"), DomainHint::Web));
        assert!(matches!(parse_domain("frontend"), DomainHint::Web));
        assert!(matches!(parse_domain("data"), DomainHint::Database));
        assert!(matches!(parse_domain("infra"), DomainHint::System));
        assert!(matches!(parse_domain("github"), DomainHint::GitHub));
        assert!(matches!(parse_domain("git"), DomainHint::Git));
        assert!(matches!(parse_domain("systems"), DomainHint::System));
        assert!(matches!(parse_domain("something"), DomainHint::Code));
    }

    #[test]
    fn parse_task_type_variants() {
        assert!(matches!(parse_task_type("debugging"), TaskType::Reasoning));
        assert!(matches!(parse_task_type("code_generation"), TaskType::Code));
        assert!(matches!(parse_task_type("testing"), TaskType::Compound));
        assert!(matches!(parse_task_type("exploration"), TaskType::Fetch));
        assert!(matches!(parse_task_type("review"), TaskType::Fetch));
        assert!(matches!(parse_task_type("xyz"), TaskType::Unknown));
    }

    #[test]
    fn end_to_end_parse_and_convert() {
        let engine = ReflectionEngine::new();
        let llm_output = r#"```json
{
  "proposals": [
    {
      "axis": "skill",
      "description": "Add permission error handling to file-ops skill",
      "confidence": 0.85,
      "details": {
        "skill_name": "file-ops",
        "section": "troubleshooting",
        "content": "When encountering 'Permission denied', check if sudo is needed."
      }
    },
    {
      "axis": "pattern",
      "description": "Demote bash→grep chain due to drift",
      "confidence": 0.7,
      "details": { "signature": "bash→grep", "action": "demote" }
    }
  ],
  "summary": "Permission errors and drifting tool chains need attention."
}
```"#;

        let resp = engine.parse_response(llm_output).unwrap();
        assert_eq!(resp.proposals.len(), 2);

        let ctx = ReflectionContext::new("e2e-test");
        let proposals = engine.convert_proposals(&resp.proposals, &ctx);
        assert_eq!(proposals.len(), 2);
        assert!(proposals.iter().all(|p| p.id.starts_with("reflect-")));
    }

    #[test]
    fn calibration_clamps_extreme_adjustment() {
        let engine = ReflectionEngine::new();
        let raw = vec![RawProposal {
            axis: "calibration".into(),
            description: "Extreme nudge".into(),
            confidence: 0.5,
            details: serde_json::json!({
                "axis": "intent:general",
                "adjustment": 5.0 // way beyond ±0.5
            }),
        }];
        let ctx = ReflectionContext::new("s1");
        let proposals = engine.convert_proposals(&raw, &ctx);
        assert_eq!(proposals.len(), 1);
        if let EvolutionAxis::Calibration { adjustment, .. } = &proposals[0].axis {
            assert!(
                *adjustment <= 0.5 && *adjustment >= -0.5,
                "Adjustment should be clamped to ±0.5, got {}",
                adjustment
            );
        }
    }
}
