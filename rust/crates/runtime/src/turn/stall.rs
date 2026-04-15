use std::collections::{BTreeSet, HashMap, HashSet};

use astra_core::RuntimeLimits;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::tool_call_shape::{tool_call_arguments_value, tool_call_name};

/// Require 3 consecutive identical tool call turns (not 2) to detect stall.
/// Window=2 was too aggressive: legitimate retries and exploration patterns
/// (e.g. read_file with different args each turn) triggered false stalls.
pub const SERVER_STALL_WINDOW: usize = 3;

/// User-visible error prefix when the agentic loop exhausts the per-request remaining-turn budget.
/// Call sites append the actual budget number, e.g. `format!("{} (budget: {} turns)", MSG, n)`.
pub const CLI_AGENTIC_TURN_BUDGET_STALL_ABORT_MSG: &str = "Turn budget exhausted. To increase, set MO_MAX_TURNS (interactive) or MO_PLAN_SUBTASK_MAX_TURNS (plan subtasks).";

/// User-visible error when the legacy in-process bridge exhausts the tool-round budget.
pub fn cli_agentic_tool_round_budget_abort_msg(current_limit: usize) -> String {
    format!(
        "Tool-round budget exhausted. To increase, set MO_MAX_TOOL_ROUNDS to a larger value (current limit: {}, default: {}).",
        current_limit,
        RuntimeLimits::default().max_tool_rounds
    )
}

/// Tools considered "exploration" — low-value if used repeatedly without
/// a "productive" tool call in between.
const EXPLORATION_TOOLS: &[&str] = &[
    "bash",
    "list_dir",
    "read_file",
    "glob",
    "grep",
    "symbol_search",
    "hover_info",
    "call_graph",
    "find_definition",
    "find_references",
    "symbols",
];

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
        tool_sigs.clear();
        return;
    }

    tool_sigs.push(server_tool_call_signature(tool_calls));
    if tool_sigs.len() > window {
        let drain_count = tool_sigs.len() - window;
        tool_sigs.drain(0..drain_count);
    }
}

/// Detect exact-repetition stall: same tool calls with same args repeated N times.
pub fn detect_server_stall(tool_sigs: &[BTreeSet<String>], window: usize) -> bool {
    if tool_sigs.len() < window {
        return false;
    }

    let recent = &tool_sigs[tool_sigs.len() - window..];
    recent.iter().all(|sig| sig == &recent[window - 1])
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
            format!(
                "{}:{}",
                name,
                serde_json::to_string(&args).unwrap_or_default()
            )
        })
        .collect();
    let name_set: HashSet<String> = tool_calls
        .iter()
        .map(|tc| tool_call_name(tc).unwrap_or("").to_string())
        .collect();
    (sig_set, name_set)
}

/// True when the last `window` rounds have **identical** tool-name sets (name-only stall in CLI loop).
pub fn detect_cli_tool_name_stall(turn_tool_names: &[HashSet<String>], window: usize) -> bool {
    turn_tool_names.len() >= window
        && turn_tool_names[turn_tool_names.len() - window..]
            .windows(2)
            .all(|w| w[0] == w[1])
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

fn tool_names_from_sigs(sigs: &BTreeSet<String>) -> Vec<String> {
    sigs.iter()
        .filter_map(|sig| sig.split(':').next().map(String::from))
        .collect()
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
) -> RewardHackingAssessment {
    let tool_names = ordered_tool_call_names(tool_calls);
    if tool_names.is_empty() {
        return RewardHackingAssessment {
            risk: 0.0,
            flags: Vec::new(),
        };
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
    if repeated_tool_name_count >= 3 {
        flags.push(format!("repeated tool name x{repeated_tool_name_count}"));
        risk += 0.20;
    }

    if tool_names.len() >= 2
        && tool_names
            .iter()
            .all(|name| EXPLORATION_TOOLS.contains(&name.as_str()))
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

    RewardHackingAssessment {
        risk: risk.clamp(0.0, 0.95),
        flags,
    }
}

pub fn dampen_quality_for_reward_hacking(
    quality: f64,
    assessment: &RewardHackingAssessment,
) -> f64 {
    (quality * (1.0 - assessment.risk)).clamp(0.0, 1.0)
}

pub fn reward_hacking_avoid_tools(tool_calls: &[Value]) -> Vec<String> {
    let tool_names = ordered_tool_call_names(tool_calls);
    if tool_names.is_empty() {
        return Vec::new();
    }

    let all_exploration = tool_names.len() >= 2
        && tool_names
            .iter()
            .all(|name| EXPLORATION_TOOLS.contains(&name.as_str()));

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

/// Detect if the agent is diverging: last N rounds used ONLY exploration tools
/// (bash, list_dir, read_file, grep, glob) with no productive tool calls.
pub fn detect_divergence(tool_sigs: &[BTreeSet<String>]) -> DivergenceStatus {
    detect_divergence_with_budget(tool_sigs, MAX_EXPLORATION_ROUNDS)
}

pub fn detect_divergence_with_budget(
    tool_sigs: &[BTreeSet<String>],
    exploration_round_budget: usize,
) -> DivergenceStatus {
    if tool_sigs.is_empty() {
        return DivergenceStatus::Healthy;
    }

    let mut consecutive_exploration = 0;
    for sigs in tool_sigs.iter().rev() {
        let names = tool_names_from_sigs(sigs);
        if names.is_empty() {
            break;
        }
        let all_exploration = names
            .iter()
            .all(|n| EXPLORATION_TOOLS.contains(&n.as_str()));
        if all_exploration {
            consecutive_exploration += 1;
        } else {
            break;
        }
    }

    match consecutive_exploration {
        0 => DivergenceStatus::Healthy,
        n if n >= exploration_round_budget => DivergenceStatus::Diverging(n),
        n => DivergenceStatus::Exploring(n),
    }
}

/// Correction prompt injected when divergence is detected.
pub const DIVERGENCE_CORRECTION: &str = "\
⚠ You have been exploring (bash/find/list_dir/read_file) for multiple rounds \
without using a specific tool to accomplish the task. This wastes tokens. \
STOP exploring and either:\n\
1. Use a specific tool (memory_store, github_list_prs, etc.) to accomplish the user's request, OR\n\
2. Tell the user what you found and ask for clarification.\n\
Do NOT continue with bash/find/read_file unless you have a specific file path to examine.";

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
    /// Tools to avoid (deprioritized or repeatedly failing).
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
/// `error_tools`: tools that have been deprioritized due to repeated errors.
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
    // Also suggest avoiding the most-repeated tool if it's not already blocked
    if let Some((name, count)) = &top_tool
        && *count >= 3
        && !avoid_tools.contains(name)
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
    EXPLORATION_TOOLS.contains(&name)
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

/// Tools that are always considered on-task (utility/meta tools).
const ALWAYS_ON_TASK_TOOLS: &[&str] =
    &["memory_search", "memory_store", "reflect", "get_agent_info"];

/// Extract keywords from user query for intent matching.
/// Lowercases and splits on whitespace/punctuation, filters short words.
fn extract_intent_keywords(query: &str) -> Vec<String> {
    query
        .to_lowercase()
        .split(|c: char| c.is_whitespace() || c.is_ascii_punctuation())
        .filter(|w| w.len() >= 2)
        .map(String::from)
        .collect()
}

/// Check if a set of tool names + their arguments have any relevance to
/// the user's original query keywords.
fn tools_relate_to_intent(
    tool_names: &[String],
    tool_args_text: &str,
    intent_keywords: &[String],
) -> bool {
    if intent_keywords.is_empty() || tool_names.is_empty() {
        return true; // can't judge → assume on-task
    }
    // Always-on-task tools are by definition relevant
    if tool_names
        .iter()
        .any(|n| ALWAYS_ON_TASK_TOOLS.contains(&n.as_str()))
    {
        return true;
    }

    let combined = format!("{} {}", tool_names.join(" "), tool_args_text.to_lowercase());

    // Check if any intent keyword appears in tool names or args
    let match_count = intent_keywords
        .iter()
        .filter(|kw| combined.contains(kw.as_str()))
        .count();

    // At least 1 keyword match, or >20% overlap
    match_count > 0 || {
        // Fallback: check if tool names themselves suggest the right domain
        let query_lower = intent_keywords.join(" ");
        // Git-related queries match git tools
        (query_lower.contains("commit")
            || query_lower.contains("git")
            || query_lower.contains("review")
            || query_lower.contains("diff")
            || query_lower.contains("blame"))
            && tool_names
                .iter()
                .any(|n| n.starts_with("git_") || n == "bash")
    }
}

/// Detect if the agent has drifted from the user's original intent.
///
/// `user_query`: the original user message.
/// `recent_tool_turns`: for each recent turn, the (tool_names, concatenated_args) used.
///
/// Returns `IntentDrift::Drifting` if the last N turns used tools
/// unrelated to the user's query.
pub fn detect_intent_drift(
    user_query: &str,
    recent_tool_turns: &[(Vec<String>, String)],
) -> IntentDrift {
    let keywords = extract_intent_keywords(user_query);
    if keywords.is_empty() || recent_tool_turns.is_empty() {
        return IntentDrift::OnTask;
    }

    // Count consecutive off-task turns from the end
    let mut consecutive_off_task = 0;
    for (names, args_text) in recent_tool_turns.iter().rev() {
        if tools_relate_to_intent(names, args_text, &keywords) {
            break;
        }
        consecutive_off_task += 1;
    }

    if consecutive_off_task >= INTENT_DRIFT_WINDOW {
        let original_snippet: String = user_query.chars().take(100).collect();
        IntentDrift::Drifting {
            consecutive_off_task,
            correction: format!(
                "⚠ INTENT DRIFT DETECTED — you have spent {} consecutive turns on tools \
                 unrelated to the user's request: \"{}\". \
                 STOP your current approach and refocus on what the user asked. \
                 If you cannot accomplish the original task, explain why and ask for guidance.",
                consecutive_off_task, original_snippet
            ),
        }
    } else {
        IntentDrift::OnTask
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool_registry::state::word_boundary_match;

    fn make_sigs(rounds: &[&[&str]]) -> Vec<BTreeSet<String>> {
        rounds
            .iter()
            .map(|tools| tools.iter().map(|t| format!("{}:{{}}", t)).collect())
            .collect()
    }

    #[test]
    fn tool_round_abort_message_points_to_tool_round_limit() {
        assert_eq!(
            cli_agentic_tool_round_budget_abort_msg(15),
            "Tool-round budget exhausted. To increase, set MO_MAX_TOOL_ROUNDS to a larger value (current limit: 15, default: 15)."
        );
    }

    // ── Stall detection ──

    #[test]
    fn stall_not_detected_below_window() {
        let sigs = make_sigs(&[&["bash"], &["bash"]]);
        assert!(!detect_server_stall(&sigs, 3));
    }

    #[test]
    fn stall_detected_repeated_exact() {
        let sigs = make_sigs(&[&["bash"], &["bash"], &["bash"]]);
        assert!(detect_server_stall(&sigs, 3));
    }

    #[test]
    fn stall_not_detected_different_tools() {
        let sigs = make_sigs(&[&["bash"], &["read_file"], &["bash"]]);
        assert!(!detect_server_stall(&sigs, 3));
    }

    // ── CLI agentic: sig/name helpers + name-only stall ──

    #[test]
    fn round_tool_call_sig_and_names_flat_shape() {
        let calls = vec![serde_json::json!({
            "name": "read_file",
            "arguments": {"path": "a.rs"}
        })];
        let (sigs, names) = round_tool_call_sig_and_names(&calls);
        assert!(
            sigs.iter()
                .any(|s| s.contains("read_file") && s.contains("a.rs"))
        );
        assert!(names.contains("read_file"));
    }

    #[test]
    fn round_tool_call_sig_and_names_canonical_shape() {
        let calls = vec![serde_json::json!({
            "id": "call_1",
            "type": "function",
            "function": {
                "name": "read_file",
                "arguments": "{\"path\":\"a.rs\"}"
            }
        })];
        let (sigs, names) = round_tool_call_sig_and_names(&calls);
        assert!(
            sigs.iter()
                .any(|s| s.contains("read_file") && s.contains("a.rs"))
        );
        assert!(names.contains("read_file"));
    }

    #[test]
    fn cli_tool_name_stall_three_identical_name_rounds() {
        let one: HashSet<String> = HashSet::from_iter([String::from("read_file")]);
        let v = vec![one.clone(), one.clone(), one];
        assert!(detect_cli_tool_name_stall(&v, SERVER_STALL_WINDOW));
    }

    #[test]
    fn cli_tool_name_stall_not_triggered_when_middle_round_differs() {
        let rf: HashSet<String> = HashSet::from_iter([String::from("read_file")]);
        let bash: HashSet<String> = HashSet::from_iter([String::from("bash")]);
        let v = vec![rf.clone(), bash, rf];
        assert!(!detect_cli_tool_name_stall(&v, SERVER_STALL_WINDOW));
    }

    // ── Divergence detection ──

    #[test]
    fn divergence_healthy_empty() {
        assert_eq!(detect_divergence(&[]), DivergenceStatus::Healthy);
    }

    #[test]
    fn divergence_healthy_productive() {
        let sigs = make_sigs(&[&["github_list_prs"], &["memory_store"]]);
        assert_eq!(detect_divergence(&sigs), DivergenceStatus::Healthy);
    }

    #[test]
    fn divergence_exploring_one() {
        let sigs = make_sigs(&[&["bash"], &["github_list_prs"], &["bash"]]);
        assert_eq!(detect_divergence(&sigs), DivergenceStatus::Exploring(1));
    }

    #[test]
    fn divergence_exploring_two() {
        // With MAX_EXPLORATION_ROUNDS=5, two consecutive exploration rounds → Exploring(2)
        let sigs = make_sigs(&[&["github_list_prs"], &["bash"], &["list_dir"]]);
        assert_eq!(detect_divergence(&sigs), DivergenceStatus::Exploring(2));
    }

    #[test]
    fn divergence_exploring_three() {
        // 3 consecutive exploration rounds → Diverging (hits threshold of 3)
        let sigs = make_sigs(&[&["bash"], &["list_dir"], &["read_file"]]);
        assert_eq!(detect_divergence(&sigs), DivergenceStatus::Diverging(3));
    }

    #[test]
    fn divergence_exploring_four() {
        // 4 consecutive exploration rounds → Diverging (past threshold of 3)
        let sigs = make_sigs(&[&["bash"], &["list_dir"], &["grep"], &["read_file"]]);
        assert_eq!(detect_divergence(&sigs), DivergenceStatus::Diverging(4));
    }

    #[test]
    fn divergence_detected_at_five() {
        // 5 consecutive exploration rounds → Diverging (hits threshold)
        let sigs = make_sigs(&[
            &["bash"],
            &["list_dir"],
            &["grep"],
            &["read_file"],
            &["glob"],
        ]);
        assert_eq!(detect_divergence(&sigs), DivergenceStatus::Diverging(5));
    }

    #[test]
    fn divergence_detected_at_eight() {
        // 8 consecutive exploration rounds → Diverging (well past threshold)
        let sigs = make_sigs(&[
            &["bash"],
            &["list_dir"],
            &["grep"],
            &["read_file"],
            &["glob"],
            &["bash"],
            &["list_dir"],
            &["grep"],
        ]);
        assert_eq!(detect_divergence(&sigs), DivergenceStatus::Diverging(8));
    }

    #[test]
    fn divergence_detected_nine() {
        let sigs = make_sigs(&[
            &["bash"],
            &["list_dir"],
            &["grep"],
            &["read_file"],
            &["glob"],
            &["bash"],
            &["list_dir"],
            &["grep"],
            &["read_file"],
        ]);
        assert_eq!(detect_divergence(&sigs), DivergenceStatus::Diverging(9));
    }

    #[test]
    fn divergence_reset_by_productive() {
        // Productive tool in the middle resets the counter;
        // only 2 exploration rounds at the end → Exploring(2) (below threshold of 5)
        let sigs = make_sigs(&[
            &["bash"],
            &["list_dir"],
            &["github_list_prs"],
            &["bash"],
            &["list_dir"],
        ]);
        assert_eq!(detect_divergence(&sigs), DivergenceStatus::Exploring(2));
    }

    #[test]
    fn divergence_multi_tool_with_productive() {
        let sigs = make_sigs(&[&["bash", "memory_store"]]);
        assert_eq!(detect_divergence(&sigs), DivergenceStatus::Healthy);
    }

    #[test]
    fn divergence_multi_tool_exploration_only() {
        // 3 rounds of multi-tool exploration → Diverging(3), hits threshold of 3
        let sigs = make_sigs(&[
            &["bash", "grep"],
            &["list_dir", "read_file"],
            &["bash", "glob"],
        ]);
        assert_eq!(detect_divergence(&sigs), DivergenceStatus::Diverging(3));
    }

    #[test]
    fn divergence_multi_tool_exploration_at_threshold() {
        // 5 rounds of multi-tool exploration → Diverging(5)
        let sigs = make_sigs(&[
            &["bash", "grep"],
            &["list_dir", "read_file"],
            &["bash", "glob"],
            &["grep", "read_file"],
            &["bash", "list_dir"],
        ]);
        assert_eq!(detect_divergence(&sigs), DivergenceStatus::Diverging(5));
    }

    /// Regression test for session f9903b97: grep→read_file→grep→grep is
    /// a normal code analysis pattern, but 4 consecutive exploration-only
    /// rounds now exceed the lowered threshold of 3.
    #[test]
    fn normal_code_analysis_diverges_after_four() {
        let sigs = make_sigs(&[
            &["grep", "grep"], // round 0: search
            &["read_file"],    // round 1: read result
            &["grep"],         // round 2: refine search
            &["grep", "grep"], // round 3: more search
        ]);
        // 4 consecutive exploration rounds → Diverging(4) with threshold=3
        assert_eq!(detect_divergence(&sigs), DivergenceStatus::Diverging(4));
    }

    #[test]
    fn reward_hacking_flags_repeated_identical_exploration() {
        let tool_calls = vec![
            serde_json::json!({"function": {"name": "read_file", "arguments": {"path": "src/lib.rs"}}}),
            serde_json::json!({"function": {"name": "read_file", "arguments": {"path": "src/lib.rs"}}}),
            serde_json::json!({"function": {"name": "read_file", "arguments": {"path": "src/lib.rs"}}}),
        ];
        let assessment = assess_reward_hacking(&tool_calls, 0.9, None);
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

    #[test]
    fn reward_hacking_dampens_quality() {
        let assessment = RewardHackingAssessment {
            risk: 0.6,
            flags: vec!["repeated identical tool call x3".into()],
        };
        assert!((dampen_quality_for_reward_hacking(0.9, &assessment) - 0.36).abs() < 0.01);
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
    fn stemming_plurals_match() {
        let lower = "list all pull requests and issues";
        let chars: Vec<char> = lower.chars().collect();
        assert!(word_boundary_match(lower, &chars, "pull request"));
        assert!(word_boundary_match(lower, &chars, "issue"));
    }

    #[test]
    fn stemming_gerund_match() {
        let lower = "committing changes to the branch";
        let chars: Vec<char> = lower.chars().collect();
        assert!(word_boundary_match(lower, &chars, "commit"));
    }

    #[test]
    fn stemming_past_tense_match() {
        let lower = "committed the fix yesterday";
        let chars: Vec<char> = lower.chars().collect();
        assert!(word_boundary_match(lower, &chars, "commit"));
    }

    #[test]
    fn stemming_no_false_positive() {
        let lower = "the community is growing";
        let chars: Vec<char> = lower.chars().collect();
        assert!(!word_boundary_match(lower, &chars, "commit"));
    }

    #[test]
    fn stemming_exact_still_works() {
        let lower = "git diff";
        let chars: Vec<char> = lower.chars().collect();
        assert!(word_boundary_match(lower, &chars, "git"));
        assert!(word_boundary_match(lower, &chars, "diff"));
    }

    #[test]
    fn stemming_prs_matches_pr() {
        let lower = "show me the prs";
        let chars: Vec<char> = lower.chars().collect();
        assert!(word_boundary_match(lower, &chars, "pr"));
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
        let sigs = make_sigs(&[
            &["github_list_prs"],
            &["github_list_prs"],
            &["github_list_prs"],
        ]);
        let reflection = build_stall_reflection(&sigs, &[], 0);
        assert!(reflection.what_happened.contains("github_list_prs"));
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

    #[test]
    fn reflection_includes_error_tools() {
        let sigs = make_sigs(&[&["read_file"], &["bash"], &["read_file"]]);
        let reflection = build_stall_reflection(&sigs, &["bash", "git_log"], 0);
        assert!(reflection.avoid_tools.contains(&"bash".to_string()));
        assert!(reflection.avoid_tools.contains(&"git_log".to_string()));
    }

    #[test]
    fn reflection_to_nudge_message_format() {
        let sigs = make_sigs(&[&["bash"], &["bash"], &["bash"]]);
        let reflection = build_stall_reflection(&sigs, &["git_log"], 0);
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
        let avoid = vec!["bash".to_string(), "git_log".to_string()];
        let mut used = std::collections::HashSet::new();
        used.insert("bash".to_string());
        used.insert("memory_store".to_string());
        let ignored = detect_nudge_ignored(&avoid, &used);
        assert_eq!(ignored, vec!["bash".to_string()]);
    }

    #[test]
    fn nudge_ignored_empty_when_obeyed() {
        let avoid = vec!["bash".to_string()];
        let mut used = std::collections::HashSet::new();
        used.insert("memory_store".to_string());
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

    // ── Intent drift detection ──

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
    fn intent_drift_on_task_when_tools_match_query() {
        let turns = make_intent_turns(&[
            (&["git_log"], ""),
            (&["git_show"], r#"{"commit":"abc123"}"#),
            (&["git_diff"], r#"{"commit":"abc123"}"#),
        ]);
        let result = detect_intent_drift("review 最新的commit", &turns);
        assert_eq!(result, IntentDrift::OnTask);
    }

    #[test]
    fn intent_drift_detected_when_unrelated_tools() {
        // User asked to review a commit, but agent writes a skill file
        let turns = make_intent_turns(&[
            (&["git_log"], ""),
            (&["write_file"], r#"{"path":"skills/web_search.py"}"#),
            (&["list_dir"], r#"{"path":"skills/"}"#),
            (&["write_file"], r#"{"path":"skills/test.py"}"#),
        ]);
        let result = detect_intent_drift("review 最新的commit", &turns);
        assert!(matches!(result, IntentDrift::Drifting { .. }));
    }

    #[test]
    fn intent_drift_not_triggered_below_window() {
        // Only 2 off-task turns (below INTENT_DRIFT_WINDOW=3)
        let turns = make_intent_turns(&[
            (&["git_log"], ""),
            (&["write_file"], r#"{"path":"random.txt"}"#),
            (&["list_dir"], r#"{"path":"random/"}"#),
        ]);
        let result = detect_intent_drift("review 最新的commit", &turns);
        assert_eq!(result, IntentDrift::OnTask);
    }

    #[test]
    fn intent_drift_reset_by_on_task_tool() {
        // 2 off-task, then 1 on-task, then 2 off-task → no drift (reset in middle)
        let turns = make_intent_turns(&[
            (&["write_file"], r#"{"path":"random.txt"}"#),
            (&["list_dir"], r#"{"path":"random/"}"#),
            (&["git_show"], r#"{"commit":"abc"}"#), // on-task: resets counter
            (&["write_file"], r#"{"path":"random2.txt"}"#),
            (&["list_dir"], r#"{"path":"random2/"}"#),
        ]);
        let result = detect_intent_drift("review 最新的commit", &turns);
        assert_eq!(result, IntentDrift::OnTask);
    }

    #[test]
    fn intent_drift_meta_tools_always_on_task() {
        let turns = make_intent_turns(&[
            (&["write_file"], "random"),
            (&["write_file"], "random"),
            (&["memory_search"], "anything"), // meta tool: always on-task
        ]);
        let result = detect_intent_drift("review 最新的commit", &turns);
        assert_eq!(result, IntentDrift::OnTask);
    }

    #[test]
    fn intent_drift_keyword_in_args_counts() {
        // Tools are generic but args contain keywords from the query
        let turns = make_intent_turns(&[
            (&["bash"], r#"{"command":"cat commit.txt"}"#),
            (&["read_file"], r#"{"path":"review_notes.md"}"#),
            (&["bash"], r#"{"command":"echo review done"}"#),
        ]);
        let result = detect_intent_drift("review 最新的commit", &turns);
        assert_eq!(result, IntentDrift::OnTask);
    }

    #[test]
    fn intent_drift_empty_query_is_on_task() {
        let turns = make_intent_turns(&[
            (&["write_file"], "random"),
            (&["write_file"], "random"),
            (&["write_file"], "random"),
        ]);
        let result = detect_intent_drift("", &turns);
        assert_eq!(result, IntentDrift::OnTask);
    }

    #[test]
    fn intent_drift_correction_includes_user_query() {
        let turns = make_intent_turns(&[
            (&["write_file"], "skill.py"),
            (&["write_file"], "test.py"),
            (&["list_dir"], "skills/"),
        ]);
        if let IntentDrift::Drifting { correction, .. } =
            detect_intent_drift("review 最新的commit", &turns)
        {
            assert!(correction.contains("review"));
            assert!(correction.contains("INTENT DRIFT"));
        } else {
            panic!("Expected Drifting");
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
        assert!(!detect_server_stall(&tool_sigs, SERVER_STALL_WINDOW));

        // Round 2: list_dir + read_file(different path)
        let calls_2 = vec![
            serde_json::json!({"name": "list_dir", "arguments": {"path": "rust/crates/astra/src/edge_tools"}}),
            serde_json::json!({"name": "read_file", "arguments": {"path": "rust/crates/astra/src/edge_tools/nonexistent.rs"}}),
        ];
        record_server_tool_signatures(&mut tool_sigs, &calls_2, window);
        assert!(
            !detect_server_stall(&tool_sigs, SERVER_STALL_WINDOW),
            "different tool calls across rounds must not trigger stall"
        );

        // Round 3: read_file(yet another path)
        let calls_3 = vec![serde_json::json!({
            "name": "read_file",
            "arguments": {"path": "rust/crates/astra/src/edge_tools/mo_tools.rs"}
        })];
        record_server_tool_signatures(&mut tool_sigs, &calls_3, window);
        assert!(
            !detect_server_stall(&tool_sigs, SERVER_STALL_WINDOW),
            "read_file with different paths across rounds must not trigger stall"
        );

        // Round 4: str_replace (completely different tool)
        let calls_4 = vec![serde_json::json!({
            "name": "str_replace",
            "arguments": {"path": "Cargo.toml", "old_str": "foo", "new_str": "bar"}
        })];
        record_server_tool_signatures(&mut tool_sigs, &calls_4, window);
        assert!(
            !detect_server_stall(&tool_sigs, SERVER_STALL_WINDOW),
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
        assert!(!detect_server_stall(&tool_sigs, SERVER_STALL_WINDOW));

        record_server_tool_signatures(&mut tool_sigs, &calls, window);
        assert!(
            !detect_server_stall(&tool_sigs, SERVER_STALL_WINDOW),
            "2 identical calls should not trigger stall with window=3"
        );

        record_server_tool_signatures(&mut tool_sigs, &calls, window);
        assert!(
            detect_server_stall(&tool_sigs, SERVER_STALL_WINDOW),
            "3 identical tool calls across rounds must trigger stall"
        );
    }

    // ══════════════════════════════════════════════════════════════════════
    //  canonical_tool_args
    // ══════════════════════════════════════════════════════════════════════

    #[test]
    fn canonical_tool_args_normalizes_whitespace() {
        let raw = r#"{  "path" :  "src/main.rs" ,  "line": 42 }"#;
        let canonical = canonical_tool_args(raw);
        assert_eq!(canonical, r#"{"line":42,"path":"src/main.rs"}"#);
    }

    #[test]
    fn canonical_tool_args_invalid_json_returns_raw() {
        let raw = "not json at all";
        assert_eq!(canonical_tool_args(raw), raw);
    }

    #[test]
    fn canonical_tool_args_empty_string() {
        assert_eq!(canonical_tool_args(""), "");
    }

    #[test]
    fn canonical_tool_args_nested_objects_and_arrays() {
        let raw = r#"{"a": [1, 2, {"b": 3}]}"#;
        let canonical = canonical_tool_args(raw);
        assert_eq!(canonical, r#"{"a":[1,2,{"b":3}]}"#);
    }

    #[test]
    fn canonical_tool_args_key_ordering_normalized() {
        let raw_a = r#"{"z": 1, "a": 2}"#;
        let raw_b = r#"{"a": 2, "z": 1}"#;
        assert_eq!(canonical_tool_args(raw_a), canonical_tool_args(raw_b));
    }

    #[test]
    fn canonical_tool_args_plain_string_value() {
        let raw = r#""hello""#;
        assert_eq!(canonical_tool_args(raw), r#""hello""#);
    }

    #[test]
    fn canonical_tool_args_number_value() {
        assert_eq!(canonical_tool_args("42"), "42");
    }

    #[test]
    fn canonical_tool_args_empty_object() {
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
    fn record_sigs_empty_tool_calls_clears_history() {
        let mut sigs = vec![BTreeSet::from(["bash:{}".to_string()])];
        record_server_tool_signatures(&mut sigs, &[], 5);
        assert!(sigs.is_empty());
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
        assert!(!detect_server_stall(&[], 3));
    }

    #[test]
    fn stall_detected_in_longer_history() {
        // Varied history followed by 3 identical → stall
        let sigs = make_sigs(&[&["grep"], &["list_dir"], &["bash"], &["bash"], &["bash"]]);
        assert!(detect_server_stall(&sigs, 3));
    }

    #[test]
    fn stall_not_detected_when_last_entry_differs() {
        let sigs = make_sigs(&[&["bash"], &["bash"], &["grep"]]);
        assert!(!detect_server_stall(&sigs, 3));
    }

    #[test]
    fn stall_multi_tool_identical_rounds() {
        // Multi-tool rounds that are identical
        let round: BTreeSet<String> = ["bash:{}".to_string(), "grep:{}".to_string()]
            .into_iter()
            .collect();
        let sigs = vec![round.clone(), round.clone(), round];
        assert!(detect_server_stall(&sigs, 3));
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
        assert!(!detect_server_stall(&sigs, 3));
    }

    // ══════════════════════════════════════════════════════════════════════
    //  detect_cli_tool_name_stall — additional cases
    // ══════════════════════════════════════════════════════════════════════

    #[test]
    fn cli_name_stall_empty_input() {
        assert!(!detect_cli_tool_name_stall(&[], 3));
    }

    #[test]
    fn cli_name_stall_single_entry() {
        let one: HashSet<String> = HashSet::from_iter([String::from("bash")]);
        assert!(!detect_cli_tool_name_stall(&[one], 3));
    }

    #[test]
    fn cli_name_stall_multi_tool_identical_sets() {
        let set: HashSet<String> =
            HashSet::from_iter(["bash".to_string(), "read_file".to_string()]);
        let v = vec![set.clone(), set.clone(), set];
        assert!(detect_cli_tool_name_stall(&v, 3));
    }

    #[test]
    fn cli_name_stall_window_of_one() {
        let one: HashSet<String> = HashSet::from_iter([String::from("bash")]);
        assert!(detect_cli_tool_name_stall(&[one], 1));
    }

    // ══════════════════════════════════════════════════════════════════════
    //  detect_divergence_with_budget — custom budgets
    // ══════════════════════════════════════════════════════════════════════

    #[test]
    fn divergence_with_budget_2_triggers_at_2() {
        let sigs = make_sigs(&[&["bash"], &["read_file"]]);
        assert_eq!(
            detect_divergence_with_budget(&sigs, 2),
            DivergenceStatus::Diverging(2)
        );
    }

    #[test]
    fn divergence_with_budget_2_exploring_at_1() {
        let sigs = make_sigs(&[&["github_list_prs"], &["bash"]]);
        assert_eq!(
            detect_divergence_with_budget(&sigs, 2),
            DivergenceStatus::Exploring(1)
        );
    }

    #[test]
    fn divergence_with_budget_1_triggers_immediately() {
        let sigs = make_sigs(&[&["bash"]]);
        assert_eq!(
            detect_divergence_with_budget(&sigs, 1),
            DivergenceStatus::Diverging(1)
        );
    }

    #[test]
    fn divergence_with_budget_larger_than_history() {
        let sigs = make_sigs(&[&["bash"], &["read_file"]]);
        assert_eq!(
            detect_divergence_with_budget(&sigs, 10),
            DivergenceStatus::Exploring(2)
        );
    }

    #[test]
    fn divergence_with_budget_empty_sigs() {
        assert_eq!(
            detect_divergence_with_budget(&[], 5),
            DivergenceStatus::Healthy
        );
    }

    #[test]
    fn divergence_empty_sig_set_breaks_chain() {
        // An empty sig set in the middle should break the consecutive chain
        let mut sigs = make_sigs(&[&["bash"], &["read_file"]]);
        sigs.push(BTreeSet::new()); // empty round
        sigs.extend(make_sigs(&[&["bash"]]));
        assert_eq!(detect_divergence(&sigs), DivergenceStatus::Exploring(1));
    }

    #[test]
    fn divergence_non_exploration_tool_breaks_chain() {
        // A single productive tool among exploration breaks the chain
        let sigs = make_sigs(&[
            &["bash"],
            &["read_file"],
            &["write_file"], // productive!
            &["bash"],
            &["grep"],
        ]);
        assert_eq!(detect_divergence(&sigs), DivergenceStatus::Exploring(2));
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
        let sigs = make_sigs(&[
            &["github_list_prs"],
            &["github_list_prs"],
            &["github_list_prs"],
        ]);
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

    // ══════════════════════════════════════════════════════════════════════
    //  detect_intent_drift — additional cases
    // ══════════════════════════════════════════════════════════════════════

    #[test]
    fn intent_drift_empty_turns() {
        assert_eq!(
            detect_intent_drift("review commit", &[]),
            IntentDrift::OnTask
        );
    }

    #[test]
    fn intent_drift_all_on_task_git_tools() {
        let turns = make_intent_turns(&[
            (&["git_log"], ""),
            (&["git_diff"], "abc"),
            (&["git_show"], "def"),
            (&["bash"], r#"git blame file.rs"#),
        ]);
        assert_eq!(
            detect_intent_drift("review commit diff", &turns),
            IntentDrift::OnTask
        );
    }

    #[test]
    fn intent_drift_long_query_truncated_in_correction() {
        // Build a long query with real keywords (>=2 chars each)
        let long_query = "deploy kubernetes cluster ".repeat(20); // 500 chars
        let turns = make_intent_turns(&[
            (&["write_file"], "random"),
            (&["write_file"], "random"),
            (&["write_file"], "random"),
        ]);
        if let IntentDrift::Drifting { correction, .. } = detect_intent_drift(&long_query, &turns) {
            // Correction should contain the query truncated to 100 chars
            assert!(correction.len() < long_query.len() + 200);
            assert!(correction.contains("INTENT DRIFT"));
        } else {
            panic!("Expected Drifting for unrelated tools");
        }
    }

    #[test]
    fn intent_drift_short_keywords_filtered() {
        // Query with only 1-char words should be treated as empty → OnTask
        let turns = make_intent_turns(&[
            (&["write_file"], "x"),
            (&["write_file"], "y"),
            (&["write_file"], "z"),
        ]);
        assert_eq!(detect_intent_drift("a b c", &turns), IntentDrift::OnTask);
    }

    #[test]
    fn intent_drift_always_on_task_tools_at_end() {
        // Off-task tools then a meta-tool at the very end → resets counter
        let turns = make_intent_turns(&[
            (&["write_file"], "random"),
            (&["write_file"], "random"),
            (&["reflect"], "anything"),
        ]);
        assert_eq!(
            detect_intent_drift("review commit", &turns),
            IntentDrift::OnTask
        );
    }

    #[test]
    fn intent_drift_exactly_at_window_boundary() {
        // Exactly INTENT_DRIFT_WINDOW off-task turns → triggers drift
        let mut turns = Vec::new();
        for _ in 0..INTENT_DRIFT_WINDOW {
            turns.push((vec!["write_file".to_string()], "random".to_string()));
        }
        assert!(matches!(
            detect_intent_drift("review commit", &turns),
            IntentDrift::Drifting { .. }
        ));
    }

    // ══════════════════════════════════════════════════════════════════════
    //  round_tool_call_sig_and_names — additional cases
    // ══════════════════════════════════════════════════════════════════════

    #[test]
    fn round_sig_and_names_empty_calls() {
        let (sigs, names) = round_tool_call_sig_and_names(&[]);
        assert!(sigs.is_empty());
        assert!(names.is_empty());
    }

    #[test]
    fn round_sig_and_names_multiple_tools() {
        let calls = vec![
            serde_json::json!({"name": "bash", "arguments": {"cmd": "ls"}}),
            serde_json::json!({"name": "read_file", "arguments": {"path": "a.rs"}}),
        ];
        let (sigs, names) = round_tool_call_sig_and_names(&calls);
        assert_eq!(sigs.len(), 2);
        assert!(names.contains("bash"));
        assert!(names.contains("read_file"));
    }

    #[test]
    fn round_sig_and_names_dedup_names() {
        // Two calls to bash with different args → 2 sigs but 1 unique name
        let calls = vec![
            serde_json::json!({"name": "bash", "arguments": {"cmd": "ls"}}),
            serde_json::json!({"name": "bash", "arguments": {"cmd": "pwd"}}),
        ];
        let (sigs, names) = round_tool_call_sig_and_names(&calls);
        assert_eq!(sigs.len(), 2, "different args → different sigs");
        assert_eq!(names.len(), 1, "same tool name → one entry in HashSet");
    }

    #[test]
    fn round_sig_and_names_missing_fields() {
        let calls = vec![serde_json::json!({})];
        let (sigs, names) = round_tool_call_sig_and_names(&calls);
        assert_eq!(sigs.len(), 1);
        assert!(names.contains(""));
    }
}
