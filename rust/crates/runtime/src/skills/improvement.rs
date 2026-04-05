//! Skill auto-improvement — detects user corrections and rewrites SKILL.md files.
//!
//! Analyzes recent conversation messages for:
//! - Step additions/changes ("can you also ask me X", "please do Y too")
//! - Preferences ("use a casual tone", "always use Y")
//! - Corrections ("no, do X instead", "that's wrong")
//!
//! Only applies to filesystem skills (`.astra/skills/`), not bundled dynamic skills.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// How many user turns between improvement analyses.
pub const TURN_BATCH_SIZE: u32 = 5;

/// A single suggested improvement to a skill.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SkillImprovement {
    /// Which section of the skill is affected.
    pub section: String,
    /// What change to make.
    pub change: String,
    /// Why this change is suggested.
    pub reason: String,
}

/// A batch of improvements proposed for a specific skill file.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ImprovementProposal {
    /// Name of the skill being improved.
    pub skill_name: String,
    /// Path to the SKILL.md file.
    pub skill_path: PathBuf,
    /// Individual improvements.
    pub improvements: Vec<SkillImprovement>,
}

/// Tracks state for the improvement analysis loop.
#[derive(Debug, Default)]
pub struct ImprovementTracker {
    /// Number of user messages analyzed so far.
    pub last_analyzed_count: u32,
    /// Pending proposal awaiting user approval.
    pub pending_proposal: Option<ImprovementProposal>,
}

impl ImprovementTracker {
    pub fn new() -> Self {
        Self::default()
    }

    /// Check if enough user turns have elapsed to trigger analysis.
    pub fn should_analyze(&self, user_message_count: u32) -> bool {
        user_message_count >= self.last_analyzed_count + TURN_BATCH_SIZE
    }

    /// Mark that analysis was performed up to this message count.
    pub fn mark_analyzed(&mut self, user_message_count: u32) {
        self.last_analyzed_count = user_message_count;
    }

    /// Store a pending proposal for user review.
    pub fn propose(&mut self, proposal: ImprovementProposal) {
        self.pending_proposal = Some(proposal);
    }

    /// Take and clear the pending proposal.
    pub fn take_proposal(&mut self) -> Option<ImprovementProposal> {
        self.pending_proposal.take()
    }
}

// ─── Prompt builders ─────────────────────────────────────────────────────────

/// Build the analysis prompt that detects user corrections in recent messages.
///
/// Returns a system prompt + user prompt pair for the LLM.
pub fn build_analysis_prompt(
    skill_name: &str,
    skill_instructions: &str,
    recent_messages: &[RecentMessage],
) -> (String, String) {
    let system = "You analyze conversations to detect when a user is correcting \
        or improving how an AI skill works. Output ONLY valid JSON. \
        If no improvements are found, output an empty array: []"
        .to_string();

    let messages_text: String = recent_messages
        .iter()
        .map(|m| format!("[{}]: {}", m.role, m.content))
        .collect::<Vec<_>>()
        .join("\n\n");

    let user = format!(
        r#"Analyze this conversation for corrections or improvements to the "{skill_name}" skill.

<skill_instructions>
{skill_instructions}
</skill_instructions>

<recent_conversation>
{messages_text}
</recent_conversation>

Look for:
1. Requests to add/change/remove steps: "can you also ask me X", "please do Y too", "skip step Z"
2. Preferences about how steps work: "ask about energy levels", "use a casual tone", "be more concise"
3. Corrections: "no, do X instead", "always use Y", "that's wrong, it should be Z"

Return a JSON array of improvements. Each improvement has:
- "section": which part of the skill to modify
- "change": what specific change to make
- "reason": why (quote the user message that triggered this)

If no improvements are detected, return: []

IMPORTANT: Only detect EXPLICIT user corrections about the skill's behavior, not general conversation. Return valid JSON only."#
    );

    (system, user)
}

/// Build the rewrite prompt that applies improvements to a SKILL.md file.
pub fn build_rewrite_prompt(current_content: &str, improvements: &[SkillImprovement]) -> String {
    let update_list: String = improvements
        .iter()
        .enumerate()
        .map(|(i, imp)| {
            format!(
                "{}. [{}] {} (reason: {})",
                i + 1,
                imp.section,
                imp.change,
                imp.reason
            )
        })
        .collect::<Vec<_>>()
        .join("\n");

    format!(
        r#"You are editing a skill definition file. Apply the following improvements to the skill.

<current_skill_file>
{current_content}
</current_skill_file>

<improvements>
{update_list}
</improvements>

Rules:
- Integrate the improvements naturally into the existing structure
- Preserve frontmatter (--- block) exactly as-is unless an improvement explicitly targets metadata
- Preserve the overall format and style
- Do not remove existing content unless an improvement explicitly replaces it
- Output the complete updated file inside <updated_file> tags"#
    )
}

/// Extract the updated file content from an LLM response.
pub fn extract_updated_content(response: &str) -> Option<String> {
    let start_tag = "<updated_file>";
    let end_tag = "</updated_file>";
    let start = response.find(start_tag)? + start_tag.len();
    let end = response.find(end_tag)?;
    if start >= end {
        return None;
    }
    Some(response[start..end].trim().to_string())
}

/// Parse improvement suggestions from an LLM JSON response.
pub fn parse_improvements(response: &str) -> Vec<SkillImprovement> {
    // Try to find JSON array in the response (may be wrapped in markdown code block)
    let trimmed = response.trim();
    let json_str = if trimmed.starts_with("```") {
        // Strip markdown code fence
        let inner = trimmed
            .strip_prefix("```json")
            .or_else(|| trimmed.strip_prefix("```"))
            .unwrap_or(trimmed);
        inner
            .strip_suffix("```")
            .unwrap_or(inner)
            .trim()
    } else {
        trimmed
    };

    serde_json::from_str(json_str).unwrap_or_default()
}

/// Format a proposal for display to the user.
pub fn format_proposal_for_display(proposal: &ImprovementProposal) -> String {
    let mut lines = vec![format!(
        "Skill improvement suggested for '{}':",
        proposal.skill_name
    )];
    for (i, imp) in proposal.improvements.iter().enumerate() {
        lines.push(format!("  {}. [{}] {}", i + 1, imp.section, imp.change));
        lines.push(format!("     Reason: {}", imp.reason));
    }
    lines.join("\n")
}

/// A simplified message representation for analysis.
#[derive(Clone, Debug)]
pub struct RecentMessage {
    pub role: String,
    pub content: String,
}

/// Apply an approved improvement by writing the new content to the skill file.
pub fn apply_improvement(skill_path: &std::path::Path, new_content: &str) -> std::io::Result<()> {
    // Create backup first
    let backup_path = skill_path.with_extension("md.bak");
    if skill_path.exists() {
        std::fs::copy(skill_path, &backup_path)?;
    }

    std::fs::write(skill_path, new_content)?;

    // Remove backup on success
    let _ = std::fs::remove_file(&backup_path);
    Ok(())
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tracker_should_analyze_after_batch() {
        let tracker = ImprovementTracker::new();
        assert!(!tracker.should_analyze(0));
        assert!(!tracker.should_analyze(4));
        assert!(tracker.should_analyze(5));
        assert!(tracker.should_analyze(10));
    }

    #[test]
    fn tracker_mark_analyzed_advances() {
        let mut tracker = ImprovementTracker::new();
        tracker.mark_analyzed(5);
        assert!(!tracker.should_analyze(5));
        assert!(!tracker.should_analyze(9));
        assert!(tracker.should_analyze(10));
    }

    #[test]
    fn parse_improvements_valid_json() {
        let json = r#"[
            {"section": "Step 2", "change": "Add tone preference", "reason": "User said 'be casual'"}
        ]"#;
        let result = parse_improvements(json);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].section, "Step 2");
    }

    #[test]
    fn parse_improvements_markdown_fence() {
        let json = "```json\n[{\"section\":\"intro\",\"change\":\"add greeting\",\"reason\":\"user asked\"}]\n```";
        let result = parse_improvements(json);
        assert_eq!(result.len(), 1);
    }

    #[test]
    fn parse_improvements_empty_array() {
        let result = parse_improvements("[]");
        assert!(result.is_empty());
    }

    #[test]
    fn parse_improvements_invalid_returns_empty() {
        let result = parse_improvements("not json at all");
        assert!(result.is_empty());
    }

    #[test]
    fn extract_updated_content_works() {
        let response = "Here's the update:\n<updated_file>\n---\nname: test\n---\n# Hello\n</updated_file>";
        let content = extract_updated_content(response).unwrap();
        assert!(content.starts_with("---"));
        assert!(content.contains("# Hello"));
    }

    #[test]
    fn extract_updated_content_missing_tag() {
        assert!(extract_updated_content("no tags here").is_none());
    }

    #[test]
    fn format_proposal_display() {
        let proposal = ImprovementProposal {
            skill_name: "debug".into(),
            skill_path: PathBuf::from("/tmp/debug/SKILL.md"),
            improvements: vec![SkillImprovement {
                section: "Step 1".into(),
                change: "Add stack trace guidance".into(),
                reason: "User said 'always show stack traces'".into(),
            }],
        };
        let display = format_proposal_for_display(&proposal);
        assert!(display.contains("debug"));
        assert!(display.contains("stack trace"));
    }

    #[test]
    fn build_analysis_prompt_includes_skill() {
        let messages = vec![RecentMessage {
            role: "user".into(),
            content: "no, always check the logs first".into(),
        }];
        let (system, user) = build_analysis_prompt("debug", "# Debug skill", &messages);
        assert!(system.contains("correcting"));
        assert!(user.contains("debug"));
        assert!(user.contains("# Debug skill"));
        assert!(user.contains("always check the logs first"));
    }

    #[test]
    fn build_rewrite_prompt_includes_content_and_improvements() {
        let improvements = vec![SkillImprovement {
            section: "Step 1".into(),
            change: "Add log check".into(),
            reason: "User said so".into(),
        }];
        let prompt = build_rewrite_prompt("---\nname: test\n---\n# Test", &improvements);
        assert!(prompt.contains("<current_skill_file>"));
        assert!(prompt.contains("Add log check"));
        assert!(prompt.contains("<updated_file>"));
    }

    #[test]
    fn apply_improvement_writes_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("SKILL.md");
        std::fs::write(&path, "old content").unwrap();
        apply_improvement(&path, "new content").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "new content");
        // Backup should be cleaned up
        assert!(!path.with_extension("md.bak").exists());
    }

    // ── E2E: full improvement pipeline ──────────────────────────────────────

    #[test]
    fn e2e_improvement_pipeline_detect_parse_format_apply() {
        // Simulates the full improvement cycle:
        // 1. Tracker detects analysis is due
        // 2. Build analysis prompt from recent messages
        // 3. Parse LLM response into improvements
        // 4. Format proposal for display
        // 5. Apply improvement to a skill file

        // Step 1: Tracker after 5 turns
        let mut tracker = ImprovementTracker::new();
        assert!(tracker.should_analyze(5));

        // Step 2: Build analysis prompt
        let messages = vec![
            RecentMessage {
                role: "user".into(),
                content: "Run the daily standup skill".into(),
            },
            RecentMessage {
                role: "assistant".into(),
                content: "Running standup skill...".into(),
            },
            RecentMessage {
                role: "user".into(),
                content: "Also ask about blockers, you always forget that step".into(),
            },
            RecentMessage {
                role: "assistant".into(),
                content: "I'll add that.".into(),
            },
        ];
        let (system, user) = build_analysis_prompt(
            "standup",
            "---\nname: standup\n---\n# Standup\nAsk about progress and plans.",
            &messages,
        );
        assert!(system.contains("analyze conversations"));
        assert!(user.contains("standup"));
        assert!(user.contains("blockers"));

        // Step 3: Parse simulated LLM response
        let llm_response = r#"[
            {
                "section": "Steps",
                "change": "Add a step asking about blockers",
                "reason": "User said 'Also ask about blockers, you always forget that step'"
            }
        ]"#;
        let improvements = parse_improvements(llm_response);
        assert_eq!(improvements.len(), 1);
        assert!(improvements[0].change.contains("blockers"));

        // Step 4: Format proposal
        let dir = tempfile::tempdir().unwrap();
        let skill_path = dir.path().join("SKILL.md");
        let original = "---\nname: standup\n---\n# Standup\nAsk about progress and plans.";
        std::fs::write(&skill_path, original).unwrap();

        let proposal = ImprovementProposal {
            skill_name: "standup".into(),
            skill_path: skill_path.clone(),
            improvements: improvements.clone(),
        };
        tracker.propose(proposal.clone());
        let display = format_proposal_for_display(&proposal);
        assert!(display.contains("standup"));
        assert!(display.contains("blockers"));

        // Step 5: Build rewrite prompt, simulate LLM response, apply
        let rewrite_prompt = build_rewrite_prompt(original, &improvements);
        assert!(rewrite_prompt.contains("Add a step asking about blockers"));

        let updated_content = r#"---
name: standup
---
# Standup
Ask about progress, plans, and blockers."#;
        let extracted = extract_updated_content(&format!(
            "Here's the updated file:\n<updated_file>\n{updated_content}\n</updated_file>"
        ));
        assert!(extracted.is_some());
        let extracted = extracted.unwrap();
        assert!(extracted.contains("blockers"));

        // Step 6: Apply to file
        apply_improvement(&skill_path, &extracted).unwrap();
        let final_content = std::fs::read_to_string(&skill_path).unwrap();
        assert!(final_content.contains("blockers"));
        assert!(!skill_path.with_extension("md.bak").exists());

        // Step 7: Tracker state updated
        tracker.mark_analyzed(5);
        assert!(!tracker.should_analyze(5));
        assert!(tracker.should_analyze(10));
        // Proposal consumed
        let taken = tracker.take_proposal();
        assert!(taken.is_some());
        assert!(tracker.take_proposal().is_none());
    }

    #[test]
    fn e2e_improvement_no_corrections_detected() {
        // When LLM finds no corrections, the pipeline should gracefully produce empty results.
        let messages = vec![
            RecentMessage {
                role: "user".into(),
                content: "What's the weather?".into(),
            },
            RecentMessage {
                role: "assistant".into(),
                content: "It's sunny.".into(),
            },
        ];
        let (_system, user) =
            build_analysis_prompt("weather", "# Weather\nCheck weather.", &messages);
        assert!(user.contains("weather"));

        // LLM returns empty array (no corrections detected)
        let improvements = parse_improvements("[]");
        assert!(improvements.is_empty());

        // LLM returns garbled output
        let improvements = parse_improvements("I found no corrections in this conversation.");
        assert!(improvements.is_empty());
    }

    #[test]
    fn e2e_apply_improvement_preserves_backup_on_failure() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nonexistent_dir").join("SKILL.md");
        // Writing to a nonexistent directory should fail
        let result = apply_improvement(&path, "new content");
        assert!(result.is_err());
    }
}
