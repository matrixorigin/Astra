use std::collections::BTreeSet;

use serde_json::Value;

pub const SERVER_STALL_WINDOW: usize = 2;

/// Tools considered "exploration" — low-value if used repeatedly without
/// a "productive" tool call in between.
const EXPLORATION_TOOLS: &[&str] = &["bash", "list_dir", "read_file", "glob", "grep"];

/// Maximum consecutive exploration-only rounds before triggering correction.
pub const MAX_EXPLORATION_ROUNDS: usize = 2;

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
            let function = tool_call
                .get("function")
                .and_then(Value::as_object)
                .cloned()
                .unwrap_or_default();
            let name = function
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let arguments = function
                .get("arguments")
                .and_then(Value::as_str)
                .unwrap_or_default();
            format!("{name}:{}", canonical_tool_args(arguments))
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

fn tool_names_from_sigs(sigs: &BTreeSet<String>) -> Vec<String> {
    sigs.iter()
        .filter_map(|sig| sig.split(':').next().map(String::from))
        .collect()
}

/// Detect if the agent is diverging: last N rounds used ONLY exploration tools
/// (bash, list_dir, read_file, grep, glob) with no productive tool calls.
pub fn detect_divergence(tool_sigs: &[BTreeSet<String>]) -> DivergenceStatus {
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
        n if n >= MAX_EXPLORATION_ROUNDS => DivergenceStatus::Diverging(n),
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

use std::collections::HashMap;

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
        // With MAX_EXPLORATION_ROUNDS=2, two consecutive exploration rounds → Diverging
        let sigs = make_sigs(&[&["github_list_prs"], &["bash"], &["list_dir"]]);
        assert_eq!(detect_divergence(&sigs), DivergenceStatus::Diverging(2));
    }

    #[test]
    fn divergence_detected_three() {
        let sigs = make_sigs(&[&["bash"], &["list_dir"], &["read_file"]]);
        assert_eq!(detect_divergence(&sigs), DivergenceStatus::Diverging(3));
    }

    #[test]
    fn divergence_detected_four() {
        let sigs = make_sigs(&[&["bash"], &["list_dir"], &["grep"], &["read_file"]]);
        assert_eq!(detect_divergence(&sigs), DivergenceStatus::Diverging(4));
    }

    #[test]
    fn divergence_reset_by_productive() {
        // Productive tool in the middle resets the counter, but 2 consecutive
        // exploration rounds at the end → Diverging with MAX_EXPLORATION_ROUNDS=2
        let sigs = make_sigs(&[
            &["bash"],
            &["list_dir"],
            &["github_list_prs"],
            &["bash"],
            &["list_dir"],
        ]);
        assert_eq!(detect_divergence(&sigs), DivergenceStatus::Diverging(2));
    }

    #[test]
    fn divergence_multi_tool_with_productive() {
        let sigs = make_sigs(&[&["bash", "memory_store"]]);
        assert_eq!(detect_divergence(&sigs), DivergenceStatus::Healthy);
    }

    #[test]
    fn divergence_multi_tool_exploration_only() {
        let sigs = make_sigs(&[
            &["bash", "grep"],
            &["list_dir", "read_file"],
            &["bash", "glob"],
        ]);
        assert_eq!(detect_divergence(&sigs), DivergenceStatus::Diverging(3));
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
}
