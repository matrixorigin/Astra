use std::collections::{BTreeSet, HashMap, HashSet};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

use crate::tool::args::shape::{tool_call_arguments_value, tool_call_name};
use crate::tool::categories::registry;
use crate::tool::result::semantics::tool_dedup_signature;

/// Errors from stall / divergence / reward-hacking heuristics (invalid configuration or inputs).
#[derive(Debug, Clone, Error, PartialEq)]
pub enum StallDetectionError {
    #[error("stall window or exploration budget must be > 0 (got {0})")]
    InvalidWindowOrBudget(usize),
    #[error("quality score must be finite (got {0})")]
    InvalidQuality(f64),
}

/// Require 3 consecutive identical tool call turns (not 2) to detect stall.
/// Window=2 was too aggressive: legitimate retries and exploration patterns
/// (e.g. read_file with different args each turn) triggered false stalls.
pub const SERVER_STALL_WINDOW: usize = 3;

/// When this many consecutive rounds emit the exact same tool-call
/// signature, the agent is stuck in a hard loop — no amount of additional
/// soft nudges is going to unstick it. Past this threshold we flip
/// `force_stop` on the next verdict so the dispatcher terminates the turn
/// with a clear reason.
///
/// Session 05e63cac t10 observed four identical `cargo clippy` calls
/// (r0-r3) and later three identical `git status` calls (r43/47/48)
/// followed by `echo ok` twice; the existing 3-nudge-limit ran out at
/// round ~3 and the loop kept burning tool rounds until
/// `token_budget_exceeded`. Hard-stop at >= 5 catches the pathology
/// well past the nudge quota while leaving room for legitimate
/// exponential-backoff retry patterns.
pub const CONSECUTIVE_IDENTICAL_SIGS_FORCE_STOP: usize = 5;

/// Count how many of the most recent `turn_sigs` entries share the same
/// full `name+args` signature set. Used to decide whether we've crossed
/// the [`CONSECUTIVE_IDENTICAL_SIGS_FORCE_STOP`] threshold.
#[must_use]
pub fn trailing_identical_sig_depth(turn_sigs: &[BTreeSet<String>]) -> usize {
    let Some(last) = turn_sigs.last() else {
        return 0;
    };
    // Trivial/degenerate inputs (empty sig set) don't represent real
    // tool activity and shouldn't count as a stall signal.
    if last.is_empty() {
        return 0;
    }
    let mut count = 1usize;
    for prev in turn_sigs.iter().rev().skip(1) {
        if prev == last {
            count += 1;
        } else {
            break;
        }
    }
    count
}

/// User-visible error prefix when the agentic loop exhausts the per-request remaining-turn budget.
/// Call sites append the actual budget number, e.g. `format!("{} (budget: {} turns)", MSG, n)`.
pub const CLI_AGENTIC_TURN_BUDGET_STALL_ABORT_MSG: &str = "Turn budget exhausted. To increase, set ASTRA_MAX_TURNS (interactive) or ASTRA_PLAN_SUBTASK_MAX_TURNS (plan subtasks).";

/// Maximum consecutive exploration-only rounds before triggering correction.
/// Lowered from 8→5→3: with auto-expanding read_file (full-file on 2nd+ ranged
/// read) and EdgeToolCache dedup, agents need fewer exploration rounds.
/// 5 was still too permissive — allowed long sequences of redundant reads.
pub const MAX_EXPLORATION_ROUNDS: usize = 3;

pub fn canonical_tool_args(raw: &str) -> String {
    match serde_json::from_str::<Value>(raw) {
        Ok(value) => serde_json::to_string(&value).unwrap_or_else(|_| raw.to_string()),
        Err(_) => raw.to_string(),
    }
}

pub fn server_tool_call_signature(tool_calls: &[Value]) -> BTreeSet<String> {
    tool_calls
        .iter()
        .map(|tool_call| {
            // Support both formats:
            //   Nested (OpenAI): {function: {name, arguments}}
            //   Flat (internal): {name, arguments}
            let (name, arguments) =
                if let Some(function) = tool_call.get("function").and_then(Value::as_object) {
                    let n = function
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or_default();
                    let a = function
                        .get("arguments")
                        .and_then(Value::as_str)
                        .unwrap_or_default();
                    (n.to_string(), a.to_string())
                } else {
                    let n = tool_call
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or_default();
                    let a = tool_call
                        .get("arguments")
                        .map(|v| serde_json::to_string(v).unwrap_or_default())
                        .unwrap_or_default();
                    (n.to_string(), a)
                };
            format!("{name}:{}", canonical_tool_args(&arguments))
        })
        .collect()
}

pub fn record_server_tool_signatures(
    tool_sigs: &mut Vec<BTreeSet<String>>,
    tool_calls: &[Value],
    window: usize,
) {
    if tool_calls.is_empty() {
        return;
    }

    tool_sigs.push(server_tool_call_signature(tool_calls));
    if tool_sigs.len() > window {
        let drain_count = tool_sigs.len() - window;
        tool_sigs.drain(0..drain_count);
    }
}

/// Detect exact-repetition stall: same tool calls with same args repeated N times.
pub fn detect_server_stall(
    tool_sigs: &[BTreeSet<String>],
    window: usize,
) -> Result<bool, StallDetectionError> {
    if window == 0 {
        return Err(StallDetectionError::InvalidWindowOrBudget(0));
    }
    if tool_sigs.len() < window {
        return Ok(false);
    }

    let recent = &tool_sigs[tool_sigs.len() - window..];
    Ok(recent.iter().all(|sig| sig == &recent[window - 1]))
}

// ─── CLI stream_chat_sse agentic loop (astra) ──────────────────────────────

/// Subtract this many "remaining inner-loop turns" when TurnGuard reports **critical** during the
/// CLI `/chat/turn` agentic loop (`apply_post_tool_turn_policy`).
pub const CLI_AGENTIC_VERDICT_REMAINING_PENALTY_CRITICAL: usize = 5;
/// Same for **warning** severity.
pub const CLI_AGENTIC_VERDICT_REMAINING_PENALTY_WARNING: usize = 2;
/// Minimum risk before the runtime guard actively throttles a repetitive turn.
pub const ACTIVE_REWARD_HACKING_RISK_THRESHOLD: f64 = 0.5;

/// Per-round signature set and tool-name set for astra flat `tool_calls` rows (`name` + `arguments` JSON).
pub fn round_tool_call_sig_and_names(tool_calls: &[Value]) -> (BTreeSet<String>, HashSet<String>) {
    let sig_set: BTreeSet<String> = tool_calls
        .iter()
        .map(|tc| {
            let name = tool_call_name(tc).unwrap_or("");
            let args = tool_call_arguments_value(tc);
            tool_dedup_signature(name, &args)
        })
        .collect();
    let name_set: HashSet<String> = tool_calls
        .iter()
        .map(|tc| tool_call_name(tc).unwrap_or("").to_string())
        .collect();
    (sig_set, name_set)
}

/// True when the last `window` rounds have **identical** tool-call signatures
/// (name + args). This is the CLI-loop equivalent of [`detect_server_stall`].
///
/// sets, which misfired for legitimate patterns like three consecutive
/// `read_file` calls with different paths. The signature-based version is
/// the general fix: progress comes from *arguments changing*, not from
/// avoiding any particular tool.
pub fn detect_cli_tool_sig_stall(
    turn_sigs: &[BTreeSet<String>],
    window: usize,
) -> Result<bool, StallDetectionError> {
    detect_server_stall(turn_sigs, window)
}

// ─── Divergence detection ───────────────────────────────────────────────────

/// Result of divergence analysis for the current turn sequence.
#[derive(Debug, Clone, PartialEq)]
pub enum DivergenceStatus {
    /// Agent is making progress.
    Healthy,
    /// Agent is exploring without progress — may be using wrong tools.
    Exploring(usize),
    /// Agent is actively diverging — inject correction prompt.
    Diverging(usize),
}

#[derive(Debug, Clone, PartialEq)]
pub struct RewardHackingAssessment {
    pub risk: f64,
    pub flags: Vec<String>,
}

fn max_duplicate_count(values: &[String]) -> usize {
    let mut counts = HashMap::new();
    for value in values {
        *counts.entry(value).or_insert(0usize) += 1;
    }
    counts.into_values().max().unwrap_or(0)
}

fn tool_call_signatures(tool_calls: &[Value]) -> Vec<String> {
    tool_calls
        .iter()
        .map(|tool_call| {
            let name = tool_call_name(tool_call).unwrap_or("");
            let args = tool_call_arguments_value(tool_call);
            format!(
                "{}:{}",
                name,
                serde_json::to_string(&args).unwrap_or_default()
            )
        })
        .collect()
}

fn ordered_tool_call_names(tool_calls: &[Value]) -> Vec<String> {
    tool_calls
        .iter()
        .map(|tool_call| tool_call_name(tool_call).unwrap_or("").to_string())
        .filter(|name| !name.is_empty())
        .collect()
}

pub fn assess_reward_hacking(
    tool_calls: &[Value],
    quality: f64,
    user_feedback_score: Option<i64>,
) -> Result<RewardHackingAssessment, StallDetectionError> {
    if !quality.is_finite() {
        return Err(StallDetectionError::InvalidQuality(quality));
    }
    let tool_names = ordered_tool_call_names(tool_calls);
    if tool_names.is_empty() {
        return Ok(RewardHackingAssessment {
            risk: 0.0,
            flags: Vec::new(),
        });
    }

    let mut risk = 0.0_f64;
    let mut flags = Vec::new();
    let identical_signature_count = max_duplicate_count(&tool_call_signatures(tool_calls));
    if identical_signature_count >= 2 {
        flags.push(format!(
            "repeated identical tool call x{identical_signature_count}"
        ));
        risk += if identical_signature_count >= 3 {
            0.55
        } else {
            0.35
        };
    }

    let repeated_tool_name_count = max_duplicate_count(&tool_names);
    // Only flag repeated tool names when the calls are also identical (same args).
    // Calling the same tool with different arguments (e.g. str_replace on 4 files)
    // is legitimate multi-target work, not reward hacking.
    if repeated_tool_name_count >= 3 && identical_signature_count >= 3 {
        flags.push(format!("repeated tool name x{repeated_tool_name_count}"));
        risk += 0.20;
    }

    if tool_names.len() >= 2
        && tool_names
            .iter()
            .all(|name| registry().is_exploration(name))
    {
        flags.push("exploration-only tool chain".to_string());
        risk += 0.25;
    }

    if !flags.is_empty() && quality >= 0.7 {
        flags.push("high quality attached to repetitive actions".to_string());
        risk += 0.15;
    }

    if !flags.is_empty() && user_feedback_score.is_some_and(|score| score < 50) {
        flags.push("low user feedback despite positive-looking outcome".to_string());
        risk += 0.20;
    }

    Ok(RewardHackingAssessment {
        risk: risk.clamp(0.0, 0.95),
        flags,
    })
}

pub fn reward_hacking_avoid_tools(tool_calls: &[Value]) -> Vec<String> {
    let tool_names = ordered_tool_call_names(tool_calls);
    if tool_names.is_empty() {
        return Vec::new();
    }

    let all_exploration = tool_names.len() >= 2
        && tool_names
            .iter()
            .all(|name| registry().is_exploration(name));

    let mut counts = HashMap::new();
    for name in tool_names {
        *counts.entry(name).or_insert(0usize) += 1;
    }

    let mut avoid_tools: Vec<String> = counts
        .into_iter()
        .filter_map(|(name, count)| {
            if count >= 2 || (all_exploration && !name.is_empty()) {
                Some(name)
            } else {
                None
            }
        })
        .collect();
    avoid_tools.sort();
    avoid_tools
}

pub fn build_reward_hacking_correction(
    assessment: &RewardHackingAssessment,
    avoid_tools: &[String],
) -> String {
    let mut message = format!(
        "⚠ Reward-hacking guard: the last tool batch looked repetitive or low-value ({}). \
Stop repeating cheap actions that do not advance the task.",
        assessment.flags.join("; ")
    );
    if !avoid_tools.is_empty() {
        message.push_str(&format!(
            "\nAvoid these tools next: [{}].",
            avoid_tools.join(", ")
        ));
    }
    message.push_str(
        "\nInstead, use a different tool that can make concrete progress, or summarize what you learned and answer the user.",
    );
    message
}

/// Progress assessment across recent rounds, based on tool-call signature
/// **diversity** rather than a hand-picked "exploration" whitelist.
///
/// - `NoProgress`  — last `window` rounds are literally the same call set
///   (identical signatures). This is the only state that warrants injecting
///   a correction; it's the real "stuck in a loop" condition.
/// - `LowNovelty(rate)` — distinct-signature rate over the last `window`
///   rounds is below `NOVELTY_FLOOR`. The agent is covering narrow ground
///   but NOT literally repeating itself; worth surfacing as a hint, but
///   not worth interrupting legitimate review / debug / analysis flows.
/// - `Healthy` — enough novelty, or not enough history to judge.
#[derive(Debug, Clone, PartialEq)]
pub enum ProgressStatus {
    Healthy,
    LowNovelty(f32),
    NoProgress,
}

/// Floor below which signature novelty is considered "low". Chosen so
/// that two-tool rotation with distinct args per call scores above it,
/// while 3 rounds of a single repeated tool scores below.
pub const NOVELTY_FLOOR: f32 = 0.34;

/// Total number of individual tool-call signatures across the last
/// `window` rounds. Used as the denominator for novelty rate.
fn total_sig_count(rounds: &[BTreeSet<String>]) -> usize {
    rounds.iter().map(|r| r.len()).sum()
}

/// Union of all signatures across the last `window` rounds.
fn distinct_sig_count(rounds: &[BTreeSet<String>]) -> usize {
    let mut seen = BTreeSet::new();
    for r in rounds {
        for s in r {
            seen.insert(s.clone());
        }
    }
    seen.len()
}

/// General-purpose progress assessment. Task-type agnostic: it does NOT
/// judge by which tools are used, only by whether the signature stream
/// is repeating or stagnating.
pub fn assess_progress(
    tool_sigs: &[BTreeSet<String>],
    window: usize,
) -> Result<ProgressStatus, StallDetectionError> {
    if window == 0 {
        return Err(StallDetectionError::InvalidWindowOrBudget(0));
    }
    if tool_sigs.len() < window {
        return Ok(ProgressStatus::Healthy);
    }
    let recent = &tool_sigs[tool_sigs.len() - window..];

    // Rounds with no tool calls don't count as progress signal either way.
    if recent.iter().any(|r| r.is_empty()) {
        return Ok(ProgressStatus::Healthy);
    }

    // Exact-repetition: every round has the same signature set.
    if recent.iter().all(|r| r == &recent[0]) {
        return Ok(ProgressStatus::NoProgress);
    }

    let total = total_sig_count(recent);
    let distinct = distinct_sig_count(recent);
    if total == 0 {
        return Ok(ProgressStatus::Healthy);
    }
    let novelty = distinct as f32 / total as f32;
    if novelty < NOVELTY_FLOOR {
        Ok(ProgressStatus::LowNovelty(novelty))
    } else {
        Ok(ProgressStatus::Healthy)
    }
}

/// Detect divergence using the default exploration window.
///
/// Delegates to [`assess_progress`]: only `NoProgress` flips to
/// `Diverging` (the single state that warrants injecting a correction).
/// `LowNovelty` maps to `Exploring` (surfaced as a hint, not a correction),
/// and `Healthy` maps to `Healthy`.
pub fn detect_divergence(
    tool_sigs: &[BTreeSet<String>],
) -> Result<DivergenceStatus, StallDetectionError> {
    detect_divergence_with_window(tool_sigs, MAX_EXPLORATION_ROUNDS)
}

pub fn detect_divergence_with_window(
    tool_sigs: &[BTreeSet<String>],
    exploration_round_window: usize,
) -> Result<DivergenceStatus, StallDetectionError> {
    match assess_progress(tool_sigs, exploration_round_window)? {
        ProgressStatus::Healthy => Ok(DivergenceStatus::Healthy),
        ProgressStatus::LowNovelty(_) => {
            // Report as Exploring (hint-only); callers should NOT inject
            // a correction — the agent may be doing legitimate analysis.
            Ok(DivergenceStatus::Exploring(exploration_round_window))
        }
        ProgressStatus::NoProgress => Ok(DivergenceStatus::Diverging(exploration_round_window)),
    }
}

/// Correction prompt injected when true no-progress is detected. The
/// text is intentionally task-agnostic — it does **not** recommend any
/// specific tool, because the right next action is task-dependent. The
/// agent is trusted to pick it based on context.
pub const DIVERGENCE_CORRECTION: &str = "\
⚠ The last few rounds produced the same tool calls with the same arguments — \
no new information is being gathered. Stop repeating. \
Either synthesize what you already have and respond to the user, \
or take a different action (a different tool, or the same tool with different arguments).";

// ─── Structured reflection nudge ────────────────────────────────────────────

/// Structured analysis of a stall condition — replaces the flat STALL_NUDGE.
/// Examines the tool call history to diagnose WHY the agent is stuck and
/// suggest specific corrective actions.
#[derive(Debug, Clone)]
pub struct StallReflection {
    /// What happened: description of the stall pattern.
    pub what_happened: String,
    /// Why: diagnosed root cause.
    pub why: String,
    /// What to try: specific actionable suggestions.
    pub what_to_try: String,
    /// Confidence in the diagnosis (0.0-1.0).
    pub confidence: f64,
    /// Tools to avoid (under health avoidance or repeatedly failing).
    pub avoid_tools: Vec<String>,
}

impl StallReflection {
    /// Format as a nudge message for injection into the conversation.
    pub fn to_nudge_message(&self) -> String {
        let mut parts = vec![
            format!("⚠ REFLECTION — Agent appears stuck.\n"),
            format!("What happened: {}\n", self.what_happened),
            format!("Why: {}\n", self.why),
            format!("What to try: {}", self.what_to_try),
        ];
        if !self.avoid_tools.is_empty() {
            parts.push(format!(
                "\nAvoid these tools: [{}]",
                self.avoid_tools.join(", ")
            ));
        }
        parts.join("")
    }
}

/// Analyze stall history and build a structured reflection.
///
/// `tool_sigs`: per-turn tool signatures (name:args sets).
/// `error_tools`: tools that have active health avoidance due to repeated errors.
/// `nudge_count`: how many nudges have been sent already (escalation).
pub fn build_stall_reflection(
    tool_sigs: &[BTreeSet<String>],
    error_tools: &[&str],
    nudge_count: usize,
) -> StallReflection {
    let window = tool_sigs.len().min(6);
    let recent = &tool_sigs[tool_sigs.len().saturating_sub(window)..];

    // Analyze: what tools are being repeated?
    let mut tool_counts: HashMap<String, usize> = HashMap::new();
    for sigs in recent {
        for sig in sigs {
            let name = sig.split(':').next().unwrap_or("").to_string();
            if !name.is_empty() {
                *tool_counts.entry(name).or_default() += 1;
            }
        }
    }

    // Find the most repeated tool
    let top_tool = tool_counts
        .iter()
        .max_by_key(|(_, count)| *count)
        .map(|(name, count)| (name.clone(), *count));

    // Classify stall type
    let (what_happened, why, what_to_try, confidence) = match top_tool {
        Some((ref name, count)) if count >= 3 && is_exploration_tool(name) && is_read_only_tool(name) => (
            format!(
                "Used '{}' {} times in the last {} turns without progressing.",
                name, count, window
            ),
            "The file content is already in your context from earlier reads. Re-reading won't add new information.".to_string(),
            "The content you need is already in the conversation. Take direct action: \
                 use str_replace or write_file to make edits, or synthesize what you've learned \
                 and respond to the user.".to_string(),
            0.85,
        ),
        Some((ref name, count)) if count >= 3 && is_exploration_tool(name) => (
            format!(
                "Used '{}' {} times in the last {} turns without progressing.",
                name, count, window
            ),
            "Exploring without a clear plan. Each call finds new data but doesn't advance toward the goal.".to_string(),
            format!(
                "Stop using '{}'. Summarize what you've learned so far and take direct action: \
                 use a specific tool to accomplish the task, or ask the user for clarification.",
                name
            ),
            0.8,
        ),
        Some((ref name, count)) if count >= 3 => (
            format!(
                "Called '{}' {} times in {} turns with same or similar arguments.",
                name, count, window
            ),
            format!(
                "'{}' is not producing the desired result. Repeating it won't help.",
                name
            ),
            if nudge_count == 0 {
                "Try a different approach: use an alternative tool, change the arguments, \
                     or decompose the problem into smaller steps.".to_string()
            } else {
                "STOP and summarize what you've found. Tell the user what worked and what didn't. \
                 Ask for guidance on next steps."
                    .to_string()
            },
            if nudge_count == 0 { 0.7 } else { 0.5 },
        ),
        _ => {
            // Generic stall — tool names are changing but same signatures repeating
            let unique_tools: std::collections::HashSet<String> = tool_counts.keys().cloned().collect();
            (
                format!(
                    "Repeating the same tool call pattern across {} turns ({} unique tools tried).",
                    window,
                    unique_tools.len()
                ),
                "The current approach isn't working. The agent is trying variations without finding a solution.".to_string(),
                if nudge_count == 0 {
                    "Step back. What is the simplest way to accomplish the user's request? \
                     Try a completely different tool or approach."
                        .to_string()
                } else {
                    "FINAL WARNING: Summarize findings and respond to the user. Do NOT continue \
                     calling tools in the same pattern."
                        .to_string()
                },
                if nudge_count == 0 { 0.6 } else { 0.3 },
            )
        }
    };

    let mut avoid_tools: Vec<String> = error_tools.iter().map(|s| s.to_string()).collect();
    // Suggest avoiding the most-repeated tool — but never read-only tools.
    // Read-only tools are always needed for observation and should stay available;
    // the guidance message already tells the model to act on existing context.
    if let Some((name, count)) = &top_tool
        && *count >= 3
        && !avoid_tools.contains(name)
        && !is_read_only_tool(name)
    {
        avoid_tools.push(name.clone());
    }

    StallReflection {
        what_happened,
        why,
        what_to_try,
        confidence,
        avoid_tools,
    }
}

fn is_exploration_tool(name: &str) -> bool {
    registry().is_exploration_or_consultative(name)
}

fn is_read_only_tool(name: &str) -> bool {
    crate::turn_guard::is_read_only_never_restrict(name)
}

/// Detect if the LLM ignored a previous stall nudge by using tools
/// that were explicitly listed in `avoid_tools`.
/// Returns the list of violated (still-used) tools.
pub fn detect_nudge_ignored(
    avoid_tools: &[String],
    current_tool_names: &std::collections::HashSet<String>,
) -> Vec<String> {
    if avoid_tools.is_empty() {
        return Vec::new();
    }
    avoid_tools
        .iter()
        .filter(|t| current_tool_names.contains(t.as_str()))
        .cloned()
        .collect()
}

// ─── Adaptive stall thresholds ──────────────────────────────────────────────

/// Adaptive stall detection thresholds that can be tuned based on
/// accumulated correction effectiveness data.
///
/// Wired into [`crate::turn_guard::TurnGuard::evaluate()`] — after each
/// correction outcome is resolved, `adjust_from_effectiveness` is called
/// with the current follow_rate and effective_rate. The adjusted
/// `stall_window` overrides the static `TaskExecutionProfile::stall_window`
/// when corrections have been ineffective (window widens to reduce false
/// positives).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AdaptiveStallThresholds {
    /// Repetition window for stall detection (default: SERVER_STALL_WINDOW).
    pub stall_window: usize,
    /// Max exploration rounds before divergence (default: MAX_EXPLORATION_ROUNDS).
    pub max_exploration_rounds: usize,
    /// Intent drift detection window (default: INTENT_DRIFT_WINDOW).
    pub intent_drift_window: usize,
}

impl Default for AdaptiveStallThresholds {
    fn default() -> Self {
        Self {
            stall_window: SERVER_STALL_WINDOW,
            max_exploration_rounds: MAX_EXPLORATION_ROUNDS,
            intent_drift_window: INTENT_DRIFT_WINDOW,
        }
    }
}

impl AdaptiveStallThresholds {
    /// Adjust thresholds based on false-positive rate.
    /// If corrections are frequently not followed (low follow rate),
    /// the thresholds may be too sensitive.
    pub fn adjust_from_effectiveness(&mut self, follow_rate: f64, effective_rate: f64) {
        if follow_rate < 0.3 && self.stall_window < 5 {
            self.stall_window += 1;
            self.max_exploration_rounds += 1;
        } else if effective_rate < 0.2 && self.stall_window < 6 {
            // Only widen window for low effectiveness if follow rate wasn't already low
            self.stall_window += 1;
        }
    }
}

// ─── Intent drift detection ─────────────────────────────────────────────────

/// Result of intent drift analysis.
#[derive(Debug, Clone, PartialEq)]
pub enum IntentDrift {
    /// Agent is on-task — tools relate to the user's query.
    OnTask,
    /// Agent may be drifting — low relevance for N consecutive turns.
    Drifting {
        consecutive_off_task: usize,
        correction: String,
    },
}

/// Minimum consecutive off-task turns before flagging drift.
pub const INTENT_DRIFT_WINDOW: usize = 3;

/// Format correction message for intent drift.
pub fn format_drift_correction(user_query: &str, consecutive_off_task: usize) -> String {
    let original_snippet: String = user_query.chars().take(100).collect();
    format!(
        "⚠ INTENT DRIFT DETECTED — you have spent {} consecutive turns on tools \
         unrelated to the user's request: \"{}\". \
         STOP your current approach and refocus on what the user asked. \
         If you cannot accomplish the original task, explain why and ask for guidance.",
        consecutive_off_task, original_snippet
    )
}

#[cfg(test)]
#[allow(deprecated)]
mod tests {
    use super::*;
    use crate::tool::registry::state::word_boundary_match;

    fn make_sigs(rounds: &[&[&str]]) -> Vec<BTreeSet<String>> {
        rounds
            .iter()
            .map(|tools| tools.iter().map(|t| format!("{}:{{}}", t)).collect())
            .collect()
    }

    #[test]
    fn trailing_identical_sig_depth_behavior() {
        // Counts streak from tail
        assert_eq!(trailing_identical_sig_depth(&[]), 0);
        assert_eq!(trailing_identical_sig_depth(&make_sigs(&[&["bash"]])), 1);
        assert_eq!(
            trailing_identical_sig_depth(&make_sigs(&[&["bash"], &["bash"]])),
            2
        );
        assert_eq!(
            trailing_identical_sig_depth(&make_sigs(&[
                &["bash"],
                &["bash"],
                &["bash"],
                &["bash"],
                &["bash"]
            ])),
            5
        );
        // Resets when last entry differs
        assert_eq!(
            trailing_identical_sig_depth(&make_sigs(&[&["bash"], &["bash"], &["bash"], &["git"]])),
            1
        );
        // Empty sig sets don't count (avoid double-counting round gaps)
        assert_eq!(
            trailing_identical_sig_depth(&[BTreeSet::new(), BTreeSet::new()]),
            0
        );
    }

    // ── Stall detection ──

    #[test]
    fn detect_server_stall_window() {
        // Below window: no stall
        assert!(!detect_server_stall(&make_sigs(&[&["bash"], &["bash"]]), 3).unwrap());
        // Exact repeats at window: stall
        assert!(detect_server_stall(&make_sigs(&[&["bash"], &["bash"], &["bash"]]), 3).unwrap());
        // Different tools: no stall
        assert!(
            !detect_server_stall(&make_sigs(&[&["bash"], &["read_file"], &["bash"]]), 3).unwrap()
        );
    }

    // ── CLI agentic: sig/name helpers + name-only stall ──

    #[test]
    fn round_tool_call_sig_and_names_shapes() {
        // Flat shape
        let c1 = vec![serde_json::json!({"name": "read_file", "arguments": {"path": "a.rs"}})];
        let (sigs, names) = round_tool_call_sig_and_names(&c1);
        assert!(
            sigs.iter()
                .any(|s| s.contains("read_file") && s.contains("a.rs"))
        );
        assert!(names.contains("read_file"));

        // Canonical (OpenAI) shape
        let c2 = vec![serde_json::json!({
            "id": "call_1", "type": "function",
            "function": {"name": "read_file", "arguments": "{\"path\":\"a.rs\"}"}
        })];
        let (sigs, names) = round_tool_call_sig_and_names(&c2);
        assert!(
            sigs.iter()
                .any(|s| s.contains("read_file") && s.contains("a.rs"))
        );
        assert!(names.contains("read_file"));
    }

    #[test]
    fn round_tool_call_sig_canonicalizes_equivalent_diff_tools() {
        let bash = vec![serde_json::json!({
            "id": "call_bash",
            "type": "function",
            "function": {
                "name": "bash",
                "arguments": "{\"command\":\"git diff -- src/\"}"
            }
        })];
        let structured = vec![serde_json::json!({
            "id": "call_git_diff",
            "type": "function",
            "function": {
                "name": "git_diff",
                "arguments": "{\"path\":\"src\",\"ref\":\"HEAD\"}"
            }
        })];

        let (bash_sigs, bash_names) = round_tool_call_sig_and_names(&bash);
        let (structured_sigs, structured_names) = round_tool_call_sig_and_names(&structured);

        assert_eq!(bash_sigs, structured_sigs);
        assert!(bash_names.contains("bash"));
        assert!(structured_names.contains("git_diff"));
    }

    // ── assess_progress (general progress-aware stall) ──

    fn sig_set(s: &[&str]) -> BTreeSet<String> {
        s.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn assess_progress_healthy_when_insufficient_history() {
        let rounds = vec![sig_set(&["read_file:a"])];
        assert_eq!(
            assess_progress(&rounds, 3).unwrap(),
            ProgressStatus::Healthy
        );
    }

    #[test]
    fn assess_progress_no_progress_on_exact_repeat() {
        let r = sig_set(&["bash:"]);
        let rounds = vec![r.clone(), r.clone(), r];
        assert_eq!(
            assess_progress(&rounds, 3).unwrap(),
            ProgressStatus::NoProgress
        );
    }

    /// Regression: the real review-task pattern from session
    /// bc74b214-3e2e — three consecutive distinct `read_file` calls.
    /// Must be classified as Healthy, NOT LowNovelty / NoProgress, so
    /// no DIVERGENCE_CORRECTION is injected for legitimate exploration.
    #[test]
    fn assess_progress_healthy_on_distinct_reads() {
        let rounds = vec![
            sig_set(&["read_file:a"]),
            sig_set(&["read_file:b"]),
            sig_set(&["read_file:c"]),
        ];
        assert_eq!(
            assess_progress(&rounds, 3).unwrap(),
            ProgressStatus::Healthy
        );
    }

    #[test]
    fn assess_progress_low_novelty_on_narrow_rotation() {
        // Two signatures alternating — 2 distinct / 6 total ≈ 0.33 < floor.
        let a = sig_set(&["bash:x", "read_file:y"]);
        let b = sig_set(&["bash:x", "read_file:y"]);
        let c = sig_set(&["bash:x", "read_file:y"]);
        // NOTE: these are IDENTICAL, so this actually hits NoProgress.
        let rounds = vec![a, b, c];
        assert_eq!(
            assess_progress(&rounds, 3).unwrap(),
            ProgressStatus::NoProgress
        );
    }

    #[test]
    fn assess_progress_healthy_on_diverse_multi_tool_review() {
        let rounds = vec![
            sig_set(&["grep:pat1", "read_file:a"]),
            sig_set(&["grep:pat2", "read_file:b"]),
            sig_set(&["list_dir:/x", "read_file:c"]),
        ];
        assert_eq!(
            assess_progress(&rounds, 3).unwrap(),
            ProgressStatus::Healthy
        );
    }

    #[test]
    fn assess_progress_invalid_window() {
        assert!(matches!(
            assess_progress(&[], 0),
            Err(StallDetectionError::InvalidWindowOrBudget(0))
        ));
    }

    /// Regression: `detect_divergence` must NOT flip to Diverging on the
    /// real review pattern (3 rounds of distinct read_file calls). This
    /// was the root cause of the false-positive "Stall correction
    /// injected" in session bc74b214-3e2e turn 2.
    #[test]
    fn detect_divergence_review_pattern_is_not_diverging() {
        let rounds = vec![
            sig_set(&["read_file:a"]),
            sig_set(&["read_file:b"]),
            sig_set(&["read_file:c"]),
        ];
        assert_eq!(
            detect_divergence(&rounds).unwrap(),
            DivergenceStatus::Healthy
        );
    }

    #[test]
    fn detect_divergence_exact_repeat_is_diverging() {
        let r = sig_set(&["bash:"]);
        let rounds = vec![r.clone(), r.clone(), r];
        assert!(matches!(
            detect_divergence(&rounds).unwrap(),
            DivergenceStatus::Diverging(_)
        ));
    }

    // ── Divergence detection ──

    #[test]
    fn divergence_healthy_empty() {
        assert_eq!(detect_divergence(&[]).unwrap(), DivergenceStatus::Healthy);
    }

    #[test]
    fn divergence_healthy_productive() {
        let sigs = make_sigs(&[&["github"], &["memory"]]);
        assert_eq!(detect_divergence(&sigs).unwrap(), DivergenceStatus::Healthy);
    }

    // ─── New progress-aware semantics ───────────────────────────────
    // Prior tests encoded the whitelist-based "3 exploration rounds =
    // diverging" heuristic. Under the new progress-aware judge, mixed
    // distinct-signature rounds are Healthy; only exact signature
    // repetition (genuine loops) promotes to Diverging.

    #[test]
    fn divergence_diverse_rounds_are_healthy() {
        // The old false-positive pattern: 3+ rounds of distinct exploration
        // tool calls. Under progress-aware detection these are Healthy
        // because each round contributes a new signature (distinct_sigs / total > floor).
        let sigs = make_sigs(&[&["bash"], &["list_dir"], &["read_file"]]);
        assert_eq!(detect_divergence(&sigs).unwrap(), DivergenceStatus::Healthy);

        let sigs = make_sigs(&[
            &["bash"],
            &["list_dir"],
            &["grep"],
            &["read_file"],
            &["glob"],
        ]);
        assert_eq!(detect_divergence(&sigs).unwrap(), DivergenceStatus::Healthy);

        let sigs = make_sigs(&[
            &["bash", "grep"],
            &["list_dir", "read_file"],
            &["bash", "glob"],
        ]);
        assert_eq!(detect_divergence(&sigs).unwrap(), DivergenceStatus::Healthy);
    }

    #[test]
    fn divergence_exact_repeat_is_diverging() {
        let sigs = make_sigs(&[&["bash"], &["bash"], &["bash"]]);
        assert!(matches!(
            detect_divergence(&sigs).unwrap(),
            DivergenceStatus::Diverging(_)
        ));
    }

    #[test]
    fn divergence_exact_multi_tool_repeat_is_diverging() {
        let sigs = make_sigs(&[
            &["bash", "read_file"],
            &["bash", "read_file"],
            &["bash", "read_file"],
        ]);
        assert!(matches!(
            detect_divergence(&sigs).unwrap(),
            DivergenceStatus::Diverging(_)
        ));
    }

    #[test]
    fn divergence_productive_call_diverse_remains_healthy() {
        let sigs = make_sigs(&[
            &["bash"],
            &["list_dir"],
            &["github"],
            &["bash"],
            &["list_dir"],
        ]);
        assert_eq!(detect_divergence(&sigs).unwrap(), DivergenceStatus::Healthy);
    }

    #[test]
    fn divergence_multi_tool_with_productive() {
        let sigs = make_sigs(&[&["bash", "memory"]]);
        assert_eq!(detect_divergence(&sigs).unwrap(), DivergenceStatus::Healthy);
    }

    /// Regression for session bc74b214-3e2e turn-2 false positive:
    /// a normal code-analysis pattern (grep/read_file/grep/grep etc.)
    /// with differing tool *presence* per round must NOT flip to Diverging.
    /// The previous whitelist-based detector misfired here.
    #[test]
    fn normal_code_analysis_is_healthy() {
        let sigs = make_sigs(&[
            &["grep", "grep"],
            &["read_file"],
            &["grep"],
            &["grep", "grep"],
        ]);
        assert_eq!(detect_divergence(&sigs).unwrap(), DivergenceStatus::Healthy);
    }

    #[test]
    fn reward_hacking_flags_repeated_identical_exploration() {
        let tool_calls = vec![
            serde_json::json!({"function": {"name": "read_file", "arguments": {"path": "src/lib.rs"}}}),
            serde_json::json!({"function": {"name": "read_file", "arguments": {"path": "src/lib.rs"}}}),
            serde_json::json!({"function": {"name": "read_file", "arguments": {"path": "src/lib.rs"}}}),
        ];
        let assessment = assess_reward_hacking(&tool_calls, 0.9, None).unwrap();
        assert!(assessment.risk >= 0.8, "{assessment:?}");
        assert!(
            assessment
                .flags
                .iter()
                .any(|flag| flag.contains("repeated identical tool call"))
        );
        assert!(
            assessment
                .flags
                .iter()
                .any(|flag| flag.contains("exploration-only"))
        );
    }

    /// Regression: calling the same tool with different arguments (e.g.
    /// str_replace on 4 different files) is legitimate multi-target work,
    /// not reward hacking. Only flag when calls are truly identical.
    #[test]
    fn reward_hacking_ignores_same_tool_different_args() {
        let tool_calls = vec![
            serde_json::json!({"name": "str_replace", "arguments": {"path": "a.rs", "old": "x", "new": "y"}}),
            serde_json::json!({"name": "str_replace", "arguments": {"path": "b.rs", "old": "x", "new": "y"}}),
            serde_json::json!({"name": "str_replace", "arguments": {"path": "c.rs", "old": "x", "new": "y"}}),
            serde_json::json!({"name": "str_replace", "arguments": {"path": "d.rs", "old": "x", "new": "y"}}),
        ];
        let assessment = assess_reward_hacking(&tool_calls, 0.5, None).unwrap();
        assert!(
            !assessment
                .flags
                .iter()
                .any(|f| f.contains("repeated tool name")),
            "same tool with different args should not be flagged: {assessment:?}"
        );
        assert!(
            assessment.risk < ACTIVE_REWARD_HACKING_RISK_THRESHOLD,
            "risk should be below threshold for legitimate multi-file edits: {assessment:?}"
        );
    }

    #[test]
    fn reward_hacking_avoid_tools_prefers_repeated_or_exploration_tools() {
        let tool_calls = vec![
            serde_json::json!({"name": "read_file", "arguments": {"path": "src/lib.rs"}}),
            serde_json::json!({"name": "read_file", "arguments": {"path": "src/lib.rs"}}),
            serde_json::json!({"name": "grep", "arguments": {"pattern": "TurnGuard"}}),
        ];

        assert_eq!(
            reward_hacking_avoid_tools(&tool_calls),
            vec!["grep".to_string(), "read_file".to_string()]
        );
    }

    #[test]
    fn reward_hacking_correction_mentions_flags_and_avoid_list() {
        let assessment = RewardHackingAssessment {
            risk: 0.6,
            flags: vec![
                "repeated identical tool call x2".into(),
                "exploration-only tool chain".into(),
            ],
        };

        let message = build_reward_hacking_correction(
            &assessment,
            &["read_file".to_string(), "grep".to_string()],
        );

        assert!(message.contains("Reward-hacking guard"));
        assert!(message.contains("repeated identical tool call x2"));
        assert!(message.contains("Avoid these tools next: [read_file, grep]"));
    }

    // ── Universal stemming ──

    #[test]
    fn word_boundary_match_stemming() {
        // Plurals match
        assert!(word_boundary_match(
            "list all pull requests and issues",
            "pull request"
        ));
        assert!(word_boundary_match(
            "list all pull requests and issues",
            "issue"
        ));
        // Gerund matches
        assert!(word_boundary_match(
            "committing changes to the branch",
            "commit"
        ));
        // Past tense matches
        assert!(word_boundary_match("committed the fix yesterday", "commit"));
        // No false positive on partial substring
        assert!(!word_boundary_match("the community is growing", "commit"));
        // Exact match still works
        assert!(word_boundary_match("git diff", "git"));
        assert!(word_boundary_match("git diff", "diff"));
        // Plurals: "prs" matches "pr"
        assert!(word_boundary_match("show me the prs", "pr"));
    }

    // ── Structured reflection ──

    #[test]
    fn reflection_exploration_stall() {
        let sigs = make_sigs(&[&["bash"], &["bash"], &["bash"]]);
        let reflection = build_stall_reflection(&sigs, &[], 0);
        assert!(reflection.what_happened.contains("bash"));
        assert!(reflection.what_happened.contains("3"));
        assert!(reflection.confidence >= 0.7);
        assert!(reflection.avoid_tools.contains(&"bash".to_string()));
    }

    #[test]
    fn reflection_non_exploration_stall() {
        let sigs = make_sigs(&[&["github"], &["github"], &["github"]]);
        let reflection = build_stall_reflection(&sigs, &[], 0);
        assert!(reflection.what_happened.contains("github"));
        assert!(reflection.what_to_try.contains("different"));
        assert!(reflection.confidence >= 0.6);
    }

    #[test]
    fn reflection_escalates_on_second_nudge() {
        let sigs = make_sigs(&[&["bash"], &["bash"], &["bash"]]);
        let r0 = build_stall_reflection(&sigs, &[], 0);
        let r1 = build_stall_reflection(&sigs, &[], 1);
        // Second nudge should have lower confidence (escalation)
        assert!(r1.confidence <= r0.confidence);
    }

    /// Regression (2026-04-23, session 26f73ee4): three consecutive rounds
    /// of `skill` calls with zero file mutations must trigger the
    /// "exploration stall" diagnostic just like three `bash` / `grep` rounds
    /// do. Before adding `skill`/`discover_skills` to the consultative
    /// classifier, an agent could consult skills forever while narrating
    /// implementation as markdown code blocks without ever tripping the
    /// stall detector.
    #[test]
    fn reflection_triggers_on_skill_obsession() {
        let sigs = make_sigs(&[&["skill"], &["skill"], &["skill"]]);
        let reflection = build_stall_reflection(&sigs, &[], 0);
        assert!(
            reflection.what_happened.contains("skill"),
            "expected 'skill' in diagnosis, got: {}",
            reflection.what_happened
        );
        assert!(
            reflection.confidence >= 0.7,
            "skill-obsession must be classified as high-confidence \
             exploration stall, got {}",
            reflection.confidence
        );
        assert!(
            reflection.avoid_tools.contains(&"skill".to_string()),
            "avoid_tools should include `skill`, got: {:?}",
            reflection.avoid_tools
        );
    }

    /// `discover_skills` is the other consultative tool — same contract.
    #[test]
    fn reflection_triggers_on_discover_skills_loop() {
        let sigs = make_sigs(&[
            &["discover_skills"],
            &["discover_skills"],
            &["discover_skills"],
        ]);
        let reflection = build_stall_reflection(&sigs, &[], 0);
        assert!(reflection.what_happened.contains("discover_skills"));
        assert!(reflection.confidence >= 0.7);
    }

    #[test]
    fn reflection_includes_error_tools() {
        let sigs = make_sigs(&[&["read_file"], &["bash"], &["read_file"]]);
        let reflection = build_stall_reflection(&sigs, &["bash", "git"], 0);
        assert!(reflection.avoid_tools.contains(&"bash".to_string()));
        assert!(reflection.avoid_tools.contains(&"git".to_string()));
    }

    #[test]
    fn reflection_to_nudge_message_format() {
        let sigs = make_sigs(&[&["bash"], &["bash"], &["bash"]]);
        let reflection = build_stall_reflection(&sigs, &["git"], 0);
        let msg = reflection.to_nudge_message();
        assert!(msg.contains("REFLECTION"));
        assert!(msg.contains("What happened:"));
        assert!(msg.contains("Why:"));
        assert!(msg.contains("What to try:"));
        assert!(msg.contains("Avoid these tools:"));
    }

    #[test]
    fn reflection_generic_stall_pattern() {
        // Different tool patterns repeating — generic stall
        let sigs = make_sigs(&[
            &["bash", "read_file"],
            &["bash", "read_file"],
            &["bash", "read_file"],
        ]);
        let reflection = build_stall_reflection(&sigs, &[], 0);
        assert!(reflection.confidence > 0.0);
        assert!(!reflection.what_happened.is_empty());
    }

    // ── Nudge-ignore detection ──

    #[test]
    fn nudge_ignored_detects_violation() {
        let avoid = vec!["bash".to_string(), "git".to_string()];
        let mut used = std::collections::HashSet::new();
        used.insert("bash".to_string());
        used.insert("memory".to_string());
        let ignored = detect_nudge_ignored(&avoid, &used);
        assert_eq!(ignored, vec!["bash".to_string()]);
    }

    #[test]
    fn nudge_ignored_empty_when_obeyed() {
        let avoid = vec!["bash".to_string()];
        let mut used = std::collections::HashSet::new();
        used.insert("memory".to_string());
        let ignored = detect_nudge_ignored(&avoid, &used);
        assert!(ignored.is_empty());
    }

    #[test]
    fn nudge_ignored_empty_when_no_avoid() {
        let avoid: Vec<String> = Vec::new();
        let mut used = std::collections::HashSet::new();
        used.insert("bash".to_string());
        let ignored = detect_nudge_ignored(&avoid, &used);
        assert!(ignored.is_empty());
    }

    // ─── IntentDrift and format_drift_correction tests ─────────────────────

    #[allow(dead_code)]
    fn make_intent_turns(turns: &[(&[&str], &str)]) -> Vec<(Vec<String>, String)> {
        turns
            .iter()
            .map(|(names, args)| {
                (
                    names.iter().map(|n| n.to_string()).collect(),
                    args.to_string(),
                )
            })
            .collect()
    }

    #[test]
    fn format_drift_correction_truncates_long_query() {
        let long_query = "a".repeat(200);
        let correction = format_drift_correction(&long_query, 5);
        assert!(correction.contains("INTENT DRIFT"));
        assert!(correction.contains("5 consecutive turns"));
        // Query should be truncated to 100 chars
        assert!(correction.len() < 200 + 200);
    }

    #[test]
    fn format_drift_correction_includes_consecutive_count() {
        let correction = format_drift_correction("fix auth bug", 3);
        assert!(correction.contains("3 consecutive turns"));
        assert!(correction.contains("fix auth bug"));
    }

    #[test]
    fn intent_drift_variants_constructible() {
        let on_task = IntentDrift::OnTask;
        assert_eq!(on_task, IntentDrift::OnTask);

        let drifting = IntentDrift::Drifting {
            consecutive_off_task: 4,
            correction: "test".to_string(),
        };
        if let IntentDrift::Drifting {
            consecutive_off_task,
            correction,
        } = drifting
        {
            assert_eq!(consecutive_off_task, 4);
            assert_eq!(correction, "test");
        } else {
            panic!("Expected Drifting variant");
        }
    }

    // ── Tool call signature format tests ──

    #[test]
    fn signature_nested_openai_format() {
        // OpenAI format: {function: {name, arguments}}
        let tool_calls = vec![serde_json::json!({
            "function": {
                "name": "read_file",
                "arguments": r#"{"path":"src/main.rs"}"#
            }
        })];
        let sigs = server_tool_call_signature(&tool_calls);
        assert_eq!(sigs.len(), 1);
        let sig = sigs.iter().next().unwrap();
        assert!(
            sig.starts_with("read_file:"),
            "expected read_file prefix, got: {sig}"
        );
        assert!(
            sig.contains("main.rs"),
            "expected main.rs in sig, got: {sig}"
        );
    }

    #[test]
    fn signature_flat_internal_format() {
        // Internal flat format: {name, arguments}
        let tool_calls = vec![serde_json::json!({
            "name": "read_file",
            "arguments": {"path": "src/main.rs"}
        })];
        let sigs = server_tool_call_signature(&tool_calls);
        assert_eq!(sigs.len(), 1);
        let sig = sigs.iter().next().unwrap();
        assert!(
            sig.starts_with("read_file:"),
            "expected read_file prefix, got: {sig}"
        );
        assert!(
            sig.contains("main.rs"),
            "expected main.rs in sig, got: {sig}"
        );
    }

    #[test]
    fn signature_flat_format_different_tools_not_equal() {
        // Two different tool calls in flat format must produce different signatures
        let calls_a = vec![serde_json::json!({
            "name": "read_file",
            "arguments": {"path": "src/main.rs"}
        })];
        let calls_b = vec![serde_json::json!({
            "name": "read_file",
            "arguments": {"path": "src/lib.rs"}
        })];
        let sigs_a = server_tool_call_signature(&calls_a);
        let sigs_b = server_tool_call_signature(&calls_b);
        assert_ne!(
            sigs_a, sigs_b,
            "different paths must produce different signatures"
        );
    }

    #[test]
    fn signature_flat_format_different_tool_names() {
        let calls_a = vec![serde_json::json!({
            "name": "read_file",
            "arguments": {"path": "src/main.rs"}
        })];
        let calls_b = vec![serde_json::json!({
            "name": "list_dir",
            "arguments": {"path": "src/"}
        })];
        let sigs_a = server_tool_call_signature(&calls_a);
        let sigs_b = server_tool_call_signature(&calls_b);
        assert_ne!(
            sigs_a, sigs_b,
            "different tool names must produce different signatures"
        );
    }

    /// Regression test for session 2c701822: flat-format tool calls were
    /// all producing signature ":" (empty name, empty args), causing false
    /// stall detection on EVERY round after the first.
    #[test]
    fn no_false_stall_with_flat_format_different_args() {
        // Simulate 4 rounds of read_file with different paths (flat format)
        let mut tool_sigs: Vec<BTreeSet<String>> = Vec::new();
        let window = SERVER_STALL_WINDOW.max(MAX_EXPLORATION_ROUNDS) + 2;

        // Round 1: read_file(Cargo.toml)
        let calls_1 = vec![serde_json::json!({
            "name": "read_file",
            "arguments": {"path": "rust/crates/astra/Cargo.toml"}
        })];
        record_server_tool_signatures(&mut tool_sigs, &calls_1, window);
        assert!(!detect_server_stall(&tool_sigs, SERVER_STALL_WINDOW).unwrap());

        // Round 2: list_dir + read_file(different path)
        let calls_2 = vec![
            serde_json::json!({"name": "list_dir", "arguments": {"path": "rust/crates/astra/src/edge_tools"}}),
            serde_json::json!({"name": "read_file", "arguments": {"path": "rust/crates/astra/src/edge_tools/nonexistent.rs"}}),
        ];
        record_server_tool_signatures(&mut tool_sigs, &calls_2, window);
        assert!(
            !detect_server_stall(&tool_sigs, SERVER_STALL_WINDOW).unwrap(),
            "different tool calls across rounds must not trigger stall"
        );

        // Round 3: read_file(yet another path)
        let calls_3 = vec![serde_json::json!({
            "name": "read_file",
            "arguments": {"path": "rust/crates/astra/src/edge_tools/mo_tools.rs"}
        })];
        record_server_tool_signatures(&mut tool_sigs, &calls_3, window);
        assert!(
            !detect_server_stall(&tool_sigs, SERVER_STALL_WINDOW).unwrap(),
            "read_file with different paths across rounds must not trigger stall"
        );

        // Round 4: str_replace (completely different tool)
        let calls_4 = vec![serde_json::json!({
            "name": "str_replace",
            "arguments": {"path": "Cargo.toml", "old_str": "foo", "new_str": "bar"}
        })];
        record_server_tool_signatures(&mut tool_sigs, &calls_4, window);
        assert!(
            !detect_server_stall(&tool_sigs, SERVER_STALL_WINDOW).unwrap(),
            "str_replace after read_file must not trigger stall"
        );
    }

    /// Verify that ACTUAL stall (same tool, same args) is still detected with flat format
    #[test]
    fn real_stall_detected_with_flat_format() {
        let mut tool_sigs: Vec<BTreeSet<String>> = Vec::new();
        let window = SERVER_STALL_WINDOW.max(MAX_EXPLORATION_ROUNDS) + 2;

        // Same exact call 3x in a row (SERVER_STALL_WINDOW=3)
        let calls = vec![serde_json::json!({
            "name": "read_file",
            "arguments": {"path": "src/main.rs"}
        })];
        record_server_tool_signatures(&mut tool_sigs, &calls, window);
        assert!(!detect_server_stall(&tool_sigs, SERVER_STALL_WINDOW).unwrap());

        record_server_tool_signatures(&mut tool_sigs, &calls, window);
        assert!(
            !detect_server_stall(&tool_sigs, SERVER_STALL_WINDOW).unwrap(),
            "2 identical calls should not trigger stall with window=3"
        );

        record_server_tool_signatures(&mut tool_sigs, &calls, window);
        assert!(
            detect_server_stall(&tool_sigs, SERVER_STALL_WINDOW).unwrap(),
            "3 identical tool calls across rounds must trigger stall"
        );
    }

    // ══════════════════════════════════════════════════════════════════════
    //  canonical_tool_args
    // ══════════════════════════════════════════════════════════════════════

    #[test]
    fn canonical_tool_args_normalization() {
        // Normalizes whitespace and key ordering
        let raw = r#"{  "path" :  "src/main.rs" ,  "line": 42 }"#;
        assert_eq!(
            canonical_tool_args(raw),
            r#"{"line":42,"path":"src/main.rs"}"#
        );
        // Invalid JSON returns raw
        assert_eq!(canonical_tool_args("not json"), "not json");
        // Empty string passthrough
        assert_eq!(canonical_tool_args(""), "");
        // Nested objects/arrays
        assert_eq!(
            canonical_tool_args(r#"{"a": [1, 2, {"b": 3}]}"#),
            r#"{"a":[1,2,{"b":3}]}"#
        );
        // Key ordering normalized (two different-order inputs produce same output)
        assert_eq!(
            canonical_tool_args(r#"{"z": 1, "a": 2}"#),
            canonical_tool_args(r#"{"a": 2, "z": 1}"#)
        );
        // Plain string
        assert_eq!(canonical_tool_args(r#""hello""#), r#""hello""#);
        // Number
        assert_eq!(canonical_tool_args("42"), "42");
        // Empty object
        assert_eq!(canonical_tool_args("{}"), "{}");
    }

    // ══════════════════════════════════════════════════════════════════════
    //  server_tool_call_signature — edge cases
    // ══════════════════════════════════════════════════════════════════════

    #[test]
    fn signature_empty_tool_calls() {
        let sigs = server_tool_call_signature(&[]);
        assert!(sigs.is_empty());
    }

    #[test]
    fn signature_multiple_tool_calls_returns_set() {
        let tool_calls = vec![
            serde_json::json!({"function": {"name": "bash", "arguments": r#"{"cmd":"ls"}"#}}),
            serde_json::json!({"function": {"name": "read_file", "arguments": r#"{"path":"a.rs"}"#}}),
        ];
        let sigs = server_tool_call_signature(&tool_calls);
        assert_eq!(sigs.len(), 2);
    }

    #[test]
    fn signature_dedup_identical_tool_calls() {
        let tc = serde_json::json!({"function": {"name": "bash", "arguments": r#"{"cmd":"ls"}"#}});
        let sigs = server_tool_call_signature(&[tc.clone(), tc]);
        assert_eq!(sigs.len(), 1, "BTreeSet should dedup identical signatures");
    }

    #[test]
    fn signature_missing_name_field() {
        let tool_calls = vec![serde_json::json!({"function": {"arguments": r#"{"x":1}"#}})];
        let sigs = server_tool_call_signature(&tool_calls);
        assert_eq!(sigs.len(), 1);
        let sig = sigs.iter().next().unwrap();
        assert!(
            sig.starts_with(':'),
            "missing name should produce empty prefix"
        );
    }

    #[test]
    fn signature_missing_arguments_field() {
        let tool_calls = vec![serde_json::json!({"function": {"name": "bash"}})];
        let sigs = server_tool_call_signature(&tool_calls);
        let sig = sigs.iter().next().unwrap();
        assert!(sig.starts_with("bash:"));
    }

    #[test]
    fn signature_completely_empty_object() {
        let tool_calls = vec![serde_json::json!({})];
        let sigs = server_tool_call_signature(&tool_calls);
        assert_eq!(sigs.len(), 1);
        // Falls through to flat branch: empty name, empty args
        let sig = sigs.iter().next().unwrap();
        assert!(sig.starts_with(':'));
    }

    // ══════════════════════════════════════════════════════════════════════
    //  record_server_tool_signatures
    // ══════════════════════════════════════════════════════════════════════

    #[test]
    fn record_sigs_empty_tool_calls_preserves_history() {
        let mut sigs = vec![BTreeSet::from(["bash:{}".to_string()])];
        record_server_tool_signatures(&mut sigs, &[], 5);
        assert_eq!(
            sigs.len(),
            1,
            "text-only turn (empty tool_calls) must preserve stall history"
        );
    }

    #[test]
    fn record_sigs_window_trims_oldest() {
        let mut sigs: Vec<BTreeSet<String>> = Vec::new();
        let calls = vec![serde_json::json!({"name": "bash", "arguments": {"cmd": "ls"}})];
        for _ in 0..5 {
            record_server_tool_signatures(&mut sigs, &calls, 3);
        }
        assert_eq!(sigs.len(), 3, "should trim to window size");
    }

    #[test]
    fn record_sigs_single_call() {
        let mut sigs: Vec<BTreeSet<String>> = Vec::new();
        let calls = vec![serde_json::json!({"name": "grep", "arguments": {"pattern": "foo"}})];
        record_server_tool_signatures(&mut sigs, &calls, 5);
        assert_eq!(sigs.len(), 1);
        assert!(sigs[0].iter().any(|s| s.contains("grep")));
    }

    #[test]
    fn record_sigs_exactly_at_window_no_trim() {
        let mut sigs: Vec<BTreeSet<String>> = Vec::new();
        let calls = vec![serde_json::json!({"name": "bash", "arguments": {}})];
        for _ in 0..3 {
            record_server_tool_signatures(&mut sigs, &calls, 3);
        }
        assert_eq!(sigs.len(), 3, "exactly at window should not over-trim");
    }

    // ══════════════════════════════════════════════════════════════════════
    //  detect_server_stall — additional cases
    // ══════════════════════════════════════════════════════════════════════

    #[test]
    fn stall_empty_input() {
        assert!(!detect_server_stall(&[], 3).unwrap());
    }

    #[test]
    fn stall_detected_in_longer_history() {
        // Varied history followed by 3 identical → stall
        let sigs = make_sigs(&[&["grep"], &["list_dir"], &["bash"], &["bash"], &["bash"]]);
        assert!(detect_server_stall(&sigs, 3).unwrap());
    }

    #[test]
    fn stall_not_detected_when_last_entry_differs() {
        let sigs = make_sigs(&[&["bash"], &["bash"], &["grep"]]);
        assert!(!detect_server_stall(&sigs, 3).unwrap());
    }

    #[test]
    fn stall_multi_tool_identical_rounds() {
        // Multi-tool rounds that are identical
        let round: BTreeSet<String> = ["bash:{}".to_string(), "grep:{}".to_string()]
            .into_iter()
            .collect();
        let sigs = vec![round.clone(), round.clone(), round];
        assert!(detect_server_stall(&sigs, 3).unwrap());
    }

    #[test]
    fn stall_multi_tool_one_round_differs() {
        let round_a: BTreeSet<String> = ["bash:{}".to_string(), "grep:{}".to_string()]
            .into_iter()
            .collect();
        let round_b: BTreeSet<String> = ["bash:{}".to_string(), "list_dir:{}".to_string()]
            .into_iter()
            .collect();
        let sigs = vec![round_a.clone(), round_b, round_a];
        assert!(!detect_server_stall(&sigs, 3).unwrap());
    }

    // ══════════════════════════════════════════════════════════════════════
    //  detect_divergence_with_window — custom windows
    // ══════════════════════════════════════════════════════════════════════

    #[test]
    fn divergence_with_budget_2_triggers_at_exact_repeat() {
        // New semantics: window=2, both rounds identical sig → Diverging.
        let sigs = make_sigs(&[&["bash"], &["bash"]]);
        assert!(matches!(
            detect_divergence_with_window(&sigs, 2).unwrap(),
            DivergenceStatus::Diverging(_)
        ));
    }

    #[test]
    fn divergence_with_budget_2_distinct_rounds_healthy() {
        // New semantics: two distinct rounds within window=2 → Healthy
        // (novelty = 2/2 = 100%).
        let sigs = make_sigs(&[&["bash"], &["read_file"]]);
        assert_eq!(
            detect_divergence_with_window(&sigs, 2).unwrap(),
            DivergenceStatus::Healthy
        );
    }

    #[test]
    fn divergence_with_budget_1_single_round_diverging() {
        // window=1 → a single round trivially equals itself → Diverging.
        let sigs = make_sigs(&[&["bash"]]);
        assert!(matches!(
            detect_divergence_with_window(&sigs, 1).unwrap(),
            DivergenceStatus::Diverging(_)
        ));
    }

    #[test]
    fn divergence_with_budget_larger_than_history_is_healthy() {
        // Not enough history to judge → Healthy (new semantics).
        let sigs = make_sigs(&[&["bash"], &["read_file"]]);
        assert_eq!(
            detect_divergence_with_window(&sigs, 10).unwrap(),
            DivergenceStatus::Healthy
        );
    }

    #[test]
    fn divergence_with_budget_empty_sigs() {
        assert_eq!(
            detect_divergence_with_window(&[], 5).unwrap(),
            DivergenceStatus::Healthy
        );
    }

    #[test]
    fn divergence_with_budget_zero_is_error() {
        assert_eq!(
            detect_divergence_with_window(&[], 0),
            Err(StallDetectionError::InvalidWindowOrBudget(0))
        );
    }

    #[test]
    fn divergence_empty_sig_set_round_in_window_is_healthy() {
        // An empty sig set means the agent produced no tool calls that
        // round — can't judge progress from that. New semantics: Healthy.
        let mut sigs = make_sigs(&[&["bash"], &["read_file"]]);
        sigs.push(BTreeSet::new());
        sigs.extend(make_sigs(&[&["bash"]]));
        assert_eq!(detect_divergence(&sigs).unwrap(), DivergenceStatus::Healthy);
    }

    #[test]
    fn divergence_mixed_productive_and_exploration_healthy() {
        // Any mix of distinct signatures → Healthy, regardless of which
        // tools are "productive" vs "exploratory" (no whitelist in new logic).
        let sigs = make_sigs(&[
            &["bash"],
            &["read_file"],
            &["write_file"],
            &["bash"],
            &["grep"],
        ]);
        assert_eq!(detect_divergence(&sigs).unwrap(), DivergenceStatus::Healthy);
    }

    // ══════════════════════════════════════════════════════════════════════
    //  detect_nudge_ignored — additional cases
    // ══════════════════════════════════════════════════════════════════════

    #[test]
    fn nudge_ignored_all_tools_violated() {
        let avoid = vec!["bash".to_string(), "grep".to_string()];
        let used: HashSet<String> = HashSet::from_iter(["bash".to_string(), "grep".to_string()]);
        let mut ignored = detect_nudge_ignored(&avoid, &used);
        ignored.sort();
        assert_eq!(ignored, vec!["bash".to_string(), "grep".to_string()]);
    }

    #[test]
    fn nudge_ignored_empty_current_tools() {
        let avoid = vec!["bash".to_string()];
        let used: HashSet<String> = HashSet::new();
        assert!(detect_nudge_ignored(&avoid, &used).is_empty());
    }

    // ══════════════════════════════════════════════════════════════════════
    //  build_stall_reflection — additional cases
    // ══════════════════════════════════════════════════════════════════════

    #[test]
    fn reflection_empty_sigs() {
        let reflection = build_stall_reflection(&[], &[], 0);
        assert!(!reflection.what_happened.is_empty());
        assert!(reflection.confidence > 0.0);
    }

    #[test]
    fn reflection_single_sig() {
        let sigs = make_sigs(&[&["bash"]]);
        let reflection = build_stall_reflection(&sigs, &[], 0);
        assert!(!reflection.what_happened.is_empty());
    }

    #[test]
    fn reflection_non_exploration_escalation_with_nudge() {
        let sigs = make_sigs(&[&["github"], &["github"], &["github"]]);
        let r = build_stall_reflection(&sigs, &[], 1);
        assert!(
            r.what_to_try.contains("STOP"),
            "nudge_count=1 should escalate: {}",
            r.what_to_try
        );
        assert!(r.confidence <= 0.7);
    }

    #[test]
    fn reflection_generic_stall_with_nudge_escalation() {
        // Few occurrences of each tool → generic stall path
        let sigs = make_sigs(&[&["tool_a"], &["tool_b"]]);
        let r0 = build_stall_reflection(&sigs, &[], 0);
        let r1 = build_stall_reflection(&sigs, &[], 1);
        assert!(
            r1.what_to_try.contains("FINAL WARNING"),
            "second nudge on generic stall should escalate"
        );
        assert!(r1.confidence < r0.confidence);
    }

    #[test]
    fn reflection_avoid_tools_dedup_with_error_tools() {
        // top_tool is already in error_tools — should not duplicate
        let sigs = make_sigs(&[&["bash"], &["bash"], &["bash"]]);
        let r = build_stall_reflection(&sigs, &["bash"], 0);
        let bash_count = r.avoid_tools.iter().filter(|t| *t == "bash").count();
        assert_eq!(bash_count, 1, "bash should appear only once in avoid_tools");
    }

    #[test]
    fn reflection_window_caps_at_six() {
        // 10 rounds but only 6 should be analyzed
        let sigs = make_sigs(&[
            &["bash"],
            &["bash"],
            &["bash"],
            &["bash"],
            &["bash"],
            &["bash"],
            &["bash"],
            &["bash"],
            &["bash"],
            &["bash"],
        ]);
        let r = build_stall_reflection(&sigs, &[], 0);
        // "6 turns" in the what_happened string (window caps at 6)
        assert!(
            r.what_happened.contains("6"),
            "window should cap at 6: {}",
            r.what_happened
        );
    }

    // ══════════════════════════════════════════════════════════════════════
    //  StallReflection::to_nudge_message — additional format tests
    // ══════════════════════════════════════════════════════════════════════

    #[test]
    fn nudge_message_without_avoid_tools() {
        let r = StallReflection {
            what_happened: "test".to_string(),
            why: "because".to_string(),
            what_to_try: "something".to_string(),
            confidence: 0.5,
            avoid_tools: vec![],
        };
        let msg = r.to_nudge_message();
        assert!(!msg.contains("Avoid these tools"));
    }

    #[test]
    fn nudge_message_with_multiple_avoid_tools() {
        let r = StallReflection {
            what_happened: "test".to_string(),
            why: "because".to_string(),
            what_to_try: "something".to_string(),
            confidence: 0.5,
            avoid_tools: vec![
                "bash".to_string(),
                "grep".to_string(),
                "list_dir".to_string(),
            ],
        };
        let msg = r.to_nudge_message();
        assert!(msg.contains("bash, grep, list_dir"));
    }

    #[test]
    fn nudge_message_contains_all_sections() {
        let r = StallReflection {
            what_happened: "WHAT".to_string(),
            why: "WHY".to_string(),
            what_to_try: "TRY".to_string(),
            confidence: 0.9,
            avoid_tools: vec!["tool_x".to_string()],
        };
        let msg = r.to_nudge_message();
        assert!(msg.contains("WHAT"));
        assert!(msg.contains("WHY"));
        assert!(msg.contains("TRY"));
        assert!(msg.contains("tool_x"));
    }

    #[test]
    fn server_stall_text_turn_does_not_clear_history() {
        let bash_ls = vec![serde_json::json!({
            "function": {"name": "bash", "arguments": "{\"cmd\":\"ls\"}"}
        })];
        let window = 3;
        let mut sigs = Vec::new();

        // Turn 1: bash ls
        record_server_tool_signatures(&mut sigs, &bash_ls, window);
        assert_eq!(sigs.len(), 1);

        // Turn 2: bash ls
        record_server_tool_signatures(&mut sigs, &bash_ls, window);
        assert_eq!(sigs.len(), 2);

        // Turn 3: text-only (empty tool_calls) — must NOT clear history
        record_server_tool_signatures(&mut sigs, &[], window);
        assert_eq!(sigs.len(), 2, "text-only turn must not wipe stall history");

        // Turn 4: bash ls — this is the 3rd identical tool turn
        record_server_tool_signatures(&mut sigs, &bash_ls, window);
        assert_eq!(sigs.len(), 3);

        // Stall should be detected: 3 identical tool turns in window of 3
        let stalled = detect_server_stall(&sigs, window).unwrap();
        assert!(
            stalled,
            "stall must be detected despite interleaved text turn"
        );
    }

    // ── P0-D: Adaptive stall threshold behavioral tests ─────────────

    /// When corrections are frequently ignored (low follow_rate), the stall
    /// window should widen to reduce false positives.
    #[test]
    fn adaptive_thresholds_widen_on_low_follow_rate() {
        let mut thresholds = AdaptiveStallThresholds::default();
        let original_window = thresholds.stall_window;

        // Low follow rate + decent effectiveness → widen
        thresholds.adjust_from_effectiveness(0.2, 0.5);
        assert!(
            thresholds.stall_window > original_window,
            "stall window must widen when follow_rate < 0.3"
        );
        assert!(
            thresholds.max_exploration_rounds > MAX_EXPLORATION_ROUNDS,
            "exploration budget must also widen"
        );
    }

    /// When corrections are effective, thresholds should NOT change.
    #[test]
    fn adaptive_thresholds_stable_when_effective() {
        let mut thresholds = AdaptiveStallThresholds::default();
        let original = thresholds.clone();

        // High follow rate + high effectiveness → no change
        thresholds.adjust_from_effectiveness(0.8, 0.7);
        assert_eq!(
            thresholds.stall_window, original.stall_window,
            "effective corrections should not change thresholds"
        );
    }

    /// Thresholds have an upper bound — they can't widen indefinitely.
    #[test]
    fn adaptive_thresholds_have_upper_bound() {
        let mut thresholds = AdaptiveStallThresholds::default();

        // Repeatedly adjust with low rates
        for _ in 0..20 {
            thresholds.adjust_from_effectiveness(0.1, 0.1);
        }

        assert!(
            thresholds.stall_window <= 6,
            "stall window must not exceed 6, got {}",
            thresholds.stall_window
        );
    }

    // ── P1-E: Reward-hacking detection behavioral tests ─────────────

    /// Scenario: Agent makes 3 identical tool calls with high quality.
    /// This should trigger reward hacking detection.
    #[test]
    fn reward_hacking_assessment_high_risk_on_identical_calls() {
        let calls = vec![
            serde_json::json!({"name": "bash", "arguments": "{\"command\": \"echo ok\"}"}),
            serde_json::json!({"name": "bash", "arguments": "{\"command\": \"echo ok\"}"}),
            serde_json::json!({"name": "bash", "arguments": "{\"command\": \"echo ok\"}"}),
        ];
        let assessment = assess_reward_hacking(&calls, 0.9, None).unwrap();
        assert!(
            assessment.risk >= ACTIVE_REWARD_HACKING_RISK_THRESHOLD,
            "3 identical calls + high quality must trigger reward hacking (risk={})",
            assessment.risk
        );
        assert!(!assessment.flags.is_empty(), "must have diagnostic flags");
    }

    /// Scenario: Agent calls the same tool with DIFFERENT args (e.g.,
    /// str_replace on 4 different files). This is legitimate work.
    #[test]
    fn no_reward_hacking_on_same_tool_different_args() {
        let calls = vec![
            serde_json::json!({"name": "str_replace", "arguments": "{\"path\": \"a.rs\", \"old\": \"x\", \"new\": \"y\"}"}),
            serde_json::json!({"name": "str_replace", "arguments": "{\"path\": \"b.rs\", \"old\": \"x\", \"new\": \"y\"}"}),
            serde_json::json!({"name": "str_replace", "arguments": "{\"path\": \"c.rs\", \"old\": \"x\", \"new\": \"y\"}"}),
        ];
        let assessment = assess_reward_hacking(&calls, 0.8, None).unwrap();
        assert!(
            assessment.risk < ACTIVE_REWARD_HACKING_RISK_THRESHOLD,
            "same tool with different args is legitimate (risk={})",
            assessment.risk
        );
    }

    /// Scenario: Low user feedback score on repetitive actions should
    /// increase reward hacking risk.
    #[test]
    fn low_user_feedback_amplifies_reward_hacking_risk() {
        let calls = vec![
            serde_json::json!({"name": "bash", "arguments": "{\"command\": \"echo ok\"}"}),
            serde_json::json!({"name": "bash", "arguments": "{\"command\": \"echo ok\"}"}),
        ];
        let without_feedback = assess_reward_hacking(&calls, 0.5, None).unwrap();
        let with_low_feedback = assess_reward_hacking(&calls, 0.5, Some(20)).unwrap();
        assert!(
            with_low_feedback.risk > without_feedback.risk,
            "low user feedback must amplify risk ({} vs {})",
            with_low_feedback.risk,
            without_feedback.risk
        );
    }

    // ─── Optimization: read_file stall gives context-aware guidance ──────────

    #[test]
    fn read_file_stall_reflection_suggests_direct_edit_not_avoid() {
        let sigs = make_sigs(&[
            &["read_file"],
            &["read_file"],
            &["read_file"],
            &["read_file"],
        ]);
        let reflection = build_stall_reflection(&sigs, &[], 0);

        // The guidance should tell the model to use the content already in context
        assert!(
            reflection.what_to_try.contains("already in")
                || reflection.what_to_try.contains("str_replace")
                || reflection.what_to_try.contains("write_file")
                || reflection.what_to_try.contains("direct action"),
            "read_file stall must suggest using content already in context, not just 'stop using read_file'. Got: {}",
            reflection.what_to_try
        );
        // Must NOT suggest removing read_file from available tools
        assert!(
            !reflection.what_to_try.contains("Stop using 'read_file'"),
            "guidance must not tell model to stop using read_file entirely. Got: {}",
            reflection.what_to_try
        );
    }

    #[test]
    fn read_file_stall_does_not_add_to_avoid_tools() {
        let sigs = make_sigs(&[
            &["read_file"],
            &["read_file"],
            &["read_file"],
            &["read_file"],
        ]);
        let reflection = build_stall_reflection(&sigs, &[], 0);

        assert!(
            !reflection.avoid_tools.contains(&"read_file".to_string()),
            "read_file must not be in avoid_tools — it's read-only and may be needed later. Got: {:?}",
            reflection.avoid_tools
        );
    }
}
