//! Multi-agent coordination prompt templates.
//!
//! Each function generates context-aware instructions that are injected into
//! sub-run tasks so agents understand their role within a team execution.

/// Prompt preamble for fan-out agents executing in parallel.
///
/// Each agent learns about its siblings and the aggregation strategy so it can
/// produce output appropriate for the merge phase.
pub fn fan_out_agent_prompt(
    agent_id: &str,
    sibling_agents: &[&str],
    aggregation: &str,
    has_gate: bool,
) -> String {
    let siblings = sibling_agents
        .iter()
        .filter(|a| **a != agent_id)
        .copied()
        .collect::<Vec<_>>();

    let sibling_clause = if siblings.is_empty() {
        "You are the sole agent on this task.".to_string()
    } else {
        format!(
            "You are working in parallel with: {}. Each agent works independently — \
             do NOT assume others will cover areas you skip.",
            siblings.join(", ")
        )
    };

    let aggregation_guidance = match aggregation {
        "FirstSuccess" => {
            "Results will be selected by first success — aim to be thorough and self-contained."
        }
        "Consensus" => {
            "Results will be compared for consensus — be precise and evidence-based so your \
             output can be meaningfully compared with peers."
        }
        "LlmGuided" => {
            "An LLM will synthesize all agent outputs — structure your output clearly with \
             headers and key findings so it can be effectively merged."
        }
        _ => {
            // AllResults or unknown
            "All agent outputs will be collected — be thorough but avoid redundancy with \
             the shared task description."
        }
    };

    let gate_clause = if has_gate {
        "\n\n⚡ Quality gate: Your output will be automatically verified. Ensure it is \
         substantive, non-repetitive, and directly addresses the task. Trivial or \
         error-dominated output will be rejected and retried."
    } else {
        ""
    };

    format!(
        "## Team Coordination: Parallel Execution\n\
         {sibling_clause}\n\n\
         **Aggregation strategy:** {aggregation}\n\
         {aggregation_guidance}{gate_clause}"
    )
}

/// Prompt for sequential/pipeline stage agents.
///
/// Tells the agent where it sits in the pipeline so it can build on prior
/// output rather than duplicating it.
pub fn sequential_stage_prompt(
    stage_index: usize,
    total_stages: usize,
    agent_id: &str,
    has_previous_output: bool,
    is_stop_on_success: bool,
    has_gate: bool,
) -> String {
    let position = if stage_index == 0 {
        "first".to_string()
    } else if stage_index == total_stages - 1 {
        "final".to_string()
    } else {
        format!("stage {}/{}", stage_index + 1, total_stages)
    };

    let previous_clause = if has_previous_output {
        "The previous agent's output is provided below. Build on it — \
         do NOT repeat what was already accomplished. Focus on your unique contribution."
    } else {
        "You are the first in the pipeline. Produce clear, structured output \
         that downstream agents can build upon."
    };

    let stop_clause = if is_stop_on_success {
        " The pipeline stops on first success — aim for a complete solution."
    } else {
        ""
    };

    let gate_clause = if has_gate {
        "\n\n⚡ Quality gate active: output is verified before passing downstream."
    } else {
        ""
    };

    format!(
        "## Team Coordination: Pipeline (Stage {pos})\n\
         You are agent **{agent_id}**, the {position} stage in a {total_stages}-stage pipeline.\n\
         {previous_clause}{stop_clause}{gate_clause}",
        pos = stage_index + 1,
    )
}

/// Prompt for the producer role in adversarial review.
pub fn adversarial_producer_prompt(
    reviewer_id: &str,
    max_rounds: u32,
    current_round: u32,
    has_previous_feedback: bool,
    has_gate: bool,
) -> String {
    let round_context = if current_round == 0 {
        format!(
            "This is round 1 of up to {max_rounds}. Produce your best work — \
             reviewer **{reviewer_id}** will critique it."
        )
    } else {
        format!(
            "This is round {cur} of {max_rounds}. The reviewer's feedback is in \
             the previous output. Address ALL feedback points — the reviewer will \
             check whether you incorporated their suggestions.",
            cur = current_round + 1,
        )
    };

    let revision_clause = if has_previous_feedback {
        "\n\n**Revision guidance:**\n\
         - Re-read the reviewer's feedback carefully\n\
         - Address each point explicitly\n\
         - If you disagree with a suggestion, explain why\n\
         - Produce a COMPLETE revised output (not just the changes)"
    } else {
        ""
    };

    let gate_clause = if has_gate {
        "\n\n⚡ Quality gate active: output must pass automated verification before review."
    } else {
        ""
    };

    format!(
        "## Team Coordination: Adversarial Review (Producer)\n\
         {round_context}{revision_clause}{gate_clause}"
    )
}

/// Prompt for the reviewer role in adversarial review.
pub fn adversarial_reviewer_prompt(
    producer_id: &str,
    max_rounds: u32,
    current_round: u32,
) -> String {
    format!(
        "## Team Coordination: Adversarial Review (Reviewer)\n\
         You are reviewing output from **{producer_id}** (round {cur} of {max_rounds}).\n\n\
         **Review protocol:**\n\
         1. Assess correctness — are there factual errors or logical flaws?\n\
         2. Assess completeness — does it fully address the original task?\n\
         3. Assess quality — is it well-structured, clear, and actionable?\n\
         4. Provide specific, constructive feedback with concrete suggestions\n\
         5. If the output is satisfactory, say so explicitly\n\n\
         **Output format:**\n\
         - Start with a verdict: APPROVE, NEEDS_REVISION, or REJECT\n\
         - List specific issues (if any) with suggested fixes\n\
         - Be precise — vague feedback wastes revision rounds",
        cur = current_round + 1,
    )
}

/// Prompt for fork children (enhanced version of existing fork_task).
pub fn fork_child_prompt(
    fork_index: usize,
    total_forks: usize,
    has_parent_context: bool,
) -> String {
    let context_clause = if has_parent_context {
        "Parent conversation context is provided for reference."
    } else {
        "No parent context available."
    };

    format!(
        "## Team Coordination: Fork (Child #{idx} of {total_forks})\n\
         You are an independent fork executing a portion of a larger task.\n\
         {context_clause}\n\n\
         **Rules:**\n\
         - Execute your assigned task directly — do NOT delegate further\n\
         - Be self-contained: your output should stand alone\n\
         - Be concise but thorough",
        idx = fork_index + 1,
    )
}

/// Budget awareness prompt injected when team has resource constraints.
pub fn budget_awareness_prompt(max_tokens: Option<u64>, max_duration_secs: Option<u64>) -> String {
    let mut parts = Vec::new();

    if let Some(tokens) = max_tokens.filter(|&t| t > 0) {
        let k = tokens / 1000;
        parts.push(format!(
            "- Token budget: ~{k}K tokens shared across all team agents. Be efficient."
        ));
    }

    if let Some(secs) = max_duration_secs.filter(|&s| s > 0) {
        let mins = secs / 60;
        if mins > 0 {
            parts.push(format!(
                "- Time budget: {mins} minute(s). Prioritize impact."
            ));
        } else {
            parts.push(format!(
                "- Time budget: {secs} second(s). Be extremely focused."
            ));
        }
    }

    if parts.is_empty() {
        return String::new();
    }

    format!("\n\n## Resource Constraints\n{}", parts.join("\n"))
}

/// Combine a coordination prompt with the original task.
///
/// Prepends the team context block before the actual task, separated by a
/// clear delimiter so the LLM can distinguish meta-instructions from work.
pub fn wrap_task_with_coordination(coordination_prompt: &str, original_task: &str) -> String {
    if coordination_prompt.is_empty() {
        return original_task.to_string();
    }
    format!("{coordination_prompt}\n\n---\n\n{original_task}")
}

// ─── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fan_out_includes_siblings() {
        let prompt = fan_out_agent_prompt("a", &["a", "b", "c"], "AllResults", false);
        assert!(prompt.contains("b, c"), "should list siblings: {prompt}");
        assert!(!prompt.contains("Quality gate"), "no gate when disabled");
    }

    #[test]
    fn fan_out_sole_agent() {
        let prompt = fan_out_agent_prompt("x", &["x"], "FirstSuccess", false);
        assert!(prompt.contains("sole agent"));
        assert!(prompt.contains("first success"));
    }

    #[test]
    fn fan_out_with_gate() {
        let prompt = fan_out_agent_prompt("a", &["a", "b"], "Consensus", true);
        assert!(prompt.contains("Quality gate"));
    }

    #[test]
    fn fan_out_aggregation_strategies() {
        for strategy in &["FirstSuccess", "Consensus", "LlmGuided", "AllResults"] {
            let prompt = fan_out_agent_prompt("a", &["a", "b"], strategy, false);
            assert!(
                prompt.contains(strategy),
                "should mention strategy {strategy}: {prompt}"
            );
        }
    }

    #[test]
    fn sequential_first_stage() {
        let prompt = sequential_stage_prompt(0, 3, "coder", false, false, false);
        assert!(prompt.contains("first"));
        assert!(prompt.contains("3-stage"));
        assert!(prompt.contains("structured output"));
    }

    #[test]
    fn sequential_middle_stage_with_previous() {
        let prompt = sequential_stage_prompt(1, 3, "reviewer", true, false, true);
        assert!(prompt.contains("stage 2/3"));
        assert!(prompt.contains("Build on it"));
        assert!(prompt.contains("Quality gate"));
    }

    #[test]
    fn sequential_final_stage() {
        let prompt = sequential_stage_prompt(2, 3, "writer", true, false, false);
        assert!(prompt.contains("final"));
    }

    #[test]
    fn sequential_stop_on_success() {
        let prompt = sequential_stage_prompt(0, 2, "a", false, true, false);
        assert!(prompt.contains("stops on first success"));
    }

    #[test]
    fn adversarial_producer_first_round() {
        let prompt = adversarial_producer_prompt("reviewer-1", 3, 0, false, false);
        assert!(prompt.contains("round 1 of"));
        assert!(prompt.contains("reviewer-1"));
        assert!(!prompt.contains("Revision guidance"));
    }

    #[test]
    fn adversarial_producer_revision_round() {
        let prompt = adversarial_producer_prompt("reviewer-1", 3, 1, true, true);
        assert!(prompt.contains("round 2 of 3"));
        assert!(prompt.contains("Revision guidance"));
        assert!(prompt.contains("Quality gate"));
    }

    #[test]
    fn adversarial_reviewer_format() {
        let prompt = adversarial_reviewer_prompt("coder-1", 3, 0);
        assert!(prompt.contains("Reviewer"));
        assert!(prompt.contains("coder-1"));
        assert!(prompt.contains("APPROVE"));
        assert!(prompt.contains("NEEDS_REVISION"));
        assert!(prompt.contains("REJECT"));
    }

    #[test]
    fn fork_child_basic() {
        let prompt = fork_child_prompt(0, 4, true);
        assert!(prompt.contains("Child #1 of 4"));
        assert!(prompt.contains("do NOT delegate"));
        assert!(prompt.contains("Parent conversation"));
    }

    #[test]
    fn fork_child_no_context() {
        let prompt = fork_child_prompt(2, 3, false);
        assert!(prompt.contains("No parent context"));
    }

    #[test]
    fn budget_awareness_tokens() {
        let prompt = budget_awareness_prompt(Some(100_000), None);
        assert!(prompt.contains("100K"));
    }

    #[test]
    fn budget_awareness_duration() {
        let prompt = budget_awareness_prompt(None, Some(300));
        assert!(prompt.contains("5 minute"));
    }

    #[test]
    fn budget_awareness_empty_when_no_limits() {
        let prompt = budget_awareness_prompt(None, None);
        assert!(prompt.is_empty());
    }

    #[test]
    fn budget_awareness_zero_values_treated_as_none() {
        let prompt = budget_awareness_prompt(Some(0), Some(0));
        assert!(prompt.is_empty());
    }

    #[test]
    fn wrap_task_preserves_original_when_empty() {
        let result = wrap_task_with_coordination("", "do the thing");
        assert_eq!(result, "do the thing");
    }

    #[test]
    fn wrap_task_prepends_coordination() {
        let result = wrap_task_with_coordination("## Context\nYou are agent A.", "do the thing");
        assert!(result.starts_with("## Context"));
        assert!(result.contains("---"));
        assert!(result.ends_with("do the thing"));
    }
}
