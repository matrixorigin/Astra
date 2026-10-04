//! Multi-agent coordination prompt templates.
//!
//! Each function generates context-aware instructions that are injected into
//! sub-run tasks so agents understand their role within a team execution.

/// Prompt preamble for fan-out agents executing in parallel.
///
/// Each agent learns about its siblings and the aggregation strategy so it can
/// produce output appropriate for the merge phase.
pub fn fan_out_agent_prompt(agent_id: &str, sibling_agents: &[&str], aggregation: &str) -> String {
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
        _ => {
            // AllResults or unknown
            "All agent outputs will be collected — be thorough but avoid redundancy with \
             the shared task description."
        }
    };

    format!(
        "## Team Coordination: Parallel Execution\n\
         {sibling_clause}\n\n\
         **Aggregation strategy:** {aggregation}\n\
         {aggregation_guidance}\n\n\
         **Efficiency:** Reuse completed work shared by the parent or siblings instead of \
         repeating it. Use the tools and verification needed to complete your assigned task, \
         including builds when needed to complete or verify it."
    )
}

/// Prompt for sequential stage agents.
///
/// Tells the agent where it sits in the sequence so it can build on prior
/// output rather than duplicating it.
pub fn sequential_stage_prompt(
    stage_index: usize,
    total_stages: usize,
    agent_id: &str,
    has_previous_output: bool,
    is_stop_on_success: bool,
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
        "You are the first in the sequence. Produce clear, structured output \
         that downstream agents can build upon."
    };

    let stop_clause = if is_stop_on_success {
        " The sequence stops on first success — aim for a complete solution."
    } else {
        ""
    };

    format!(
        "## Team Coordination: Sequential Execution (Stage {pos})\n\
         You are agent **{agent_id}**, the {position} stage in a {total_stages}-stage sequence.\n\
         {previous_clause}{stop_clause}",
        pos = stage_index + 1,
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
        let prompt = fan_out_agent_prompt("a", &["a", "b", "c"], "AllResults");
        assert!(prompt.contains("b, c"), "should list siblings: {prompt}");
    }

    #[test]
    fn fan_out_sole_agent() {
        let prompt = fan_out_agent_prompt("x", &["x"], "FirstSuccess");
        assert!(prompt.contains("sole agent"));
        assert!(prompt.contains("first success"));
    }

    #[test]
    fn fan_out_aggregation_strategies() {
        for strategy in &["FirstSuccess", "Consensus", "AllResults"] {
            let prompt = fan_out_agent_prompt("a", &["a", "b"], strategy);
            assert!(
                prompt.contains(strategy),
                "should mention strategy {strategy}: {prompt}"
            );
        }
    }

    #[test]
    fn sequential_first_stage() {
        let prompt = sequential_stage_prompt(0, 3, "coder", false, false);
        assert!(prompt.contains("first"));
        assert!(prompt.contains("3-stage"));
        assert!(prompt.contains("structured output"));
    }

    #[test]
    fn sequential_middle_stage_with_previous() {
        let prompt = sequential_stage_prompt(1, 3, "reviewer", true, false);
        assert!(prompt.contains("stage 2/3"));
        assert!(prompt.contains("Build on it"));
    }

    #[test]
    fn sequential_final_stage() {
        let prompt = sequential_stage_prompt(2, 3, "writer", true, false);
        assert!(prompt.contains("final"));
    }

    #[test]
    fn sequential_stop_on_success() {
        let prompt = sequential_stage_prompt(0, 2, "a", false, true);
        assert!(prompt.contains("stops on first success"));
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
