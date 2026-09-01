//! Post-compaction attachment recovery.
//!
//! After compacting message history, explicitly supplied attachments such as
//! active skills and session plans can be re-injected into the conversation.
//! Compaction itself never reads workspace files; exact or fresh bytes require
//! an ordinary admitted tool invocation.

use serde::{Deserialize, Serialize};
use serde_json::Value;

// ---------------------------------------------------------------------------
// Token budgets (chars ≈ tokens × 4)
// ---------------------------------------------------------------------------

/// Total char budget for skill attachments.
pub const SKILL_ATTACHMENT_CHAR_BUDGET: usize = 100_000; // ~25K tokens
/// Max chars per individual skill.
pub const MAX_CHARS_PER_SKILL: usize = 20_000; // ~5K tokens

// ---------------------------------------------------------------------------
// Attachment Types
// ---------------------------------------------------------------------------

/// A skill (reusable agent capability) to re-inject after compaction.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillAttachment {
    /// Skill name / identifier.
    pub name: String,
    /// Skill content (possibly truncated).
    pub content: String,
    /// True if the content was truncated.
    pub truncated: bool,
}

impl SkillAttachment {
    /// Create a skill attachment, truncating if needed.
    pub fn new(name: impl Into<String>, content: impl Into<String>) -> Self {
        let name = name.into();
        let raw = content.into();
        let char_limit = MAX_CHARS_PER_SKILL.saturating_sub(SKILL_TRUNCATION_MARKER.len());
        if raw.chars().count() > MAX_CHARS_PER_SKILL {
            let truncated: String = raw.chars().take(char_limit).collect();
            Self {
                name,
                content: truncated + SKILL_TRUNCATION_MARKER,
                truncated: true,
            }
        } else {
            Self {
                name,
                content: raw,
                truncated: false,
            }
        }
    }

    /// Format as a user message for injection.
    pub fn to_message(&self) -> Value {
        serde_json::json!({
            "role": "user",
            "content": format!(
                "[Post-compaction context: skill '{}']\n{}", self.name, self.content
            ),
            "attachment_metadata": {
                "kind": "skill",
                "name": self.name,
                "truncated": self.truncated,
            }
        })
    }
}

/// The session plan to re-inject after compaction.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlanAttachment {
    /// Plan content.
    pub content: String,
}

impl PlanAttachment {
    pub fn new(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
        }
    }

    /// Format as a user message for injection.
    pub fn to_message(&self) -> Value {
        serde_json::json!({
            "role": "user",
            "content": format!("[Post-compaction context: current plan]\n{}", self.content),
            "attachment_metadata": { "kind": "plan" }
        })
    }
}

/// Truncation marker for skill content.
pub const SKILL_TRUNCATION_MARKER: &str = "\n...[skill content truncated for compaction; use Read on the skill path if you need the full text]";

// ---------------------------------------------------------------------------
// PostCompactAttachments — collected set of attachments for a single compaction
// ---------------------------------------------------------------------------

/// The set of attachments to re-inject after a compaction event.
#[derive(Debug, Default, Clone)]
pub struct PostCompactAttachments {
    /// Active skills.
    pub skills: Vec<SkillAttachment>,
    /// Session plan, if one exists.
    pub plan: Option<PlanAttachment>,
}

impl PostCompactAttachments {
    /// Returns true if there are no attachments to inject.
    pub fn is_empty(&self) -> bool {
        self.skills.is_empty() && self.plan.is_none()
    }

    /// Convert all attachments to injectable messages, ordered:
    /// 1. Plan (if any)
    /// 2. Skills (if any)
    pub fn to_messages(&self) -> Vec<Value> {
        let mut msgs = Vec::new();
        if let Some(plan) = &self.plan {
            msgs.push(plan.to_message());
        }
        for skill in &self.skills {
            msgs.push(skill.to_message());
        }
        msgs
    }

    /// Total char count across all attachments (for budget tracking).
    pub fn total_chars(&self) -> usize {
        let skill_chars: usize = self.skills.iter().map(|s| s.content.len()).sum();
        let plan_chars = self.plan.as_ref().map(|p| p.content.len()).unwrap_or(0);
        skill_chars + plan_chars
    }
}

// ---------------------------------------------------------------------------
// Builder
// ---------------------------------------------------------------------------

/// Builder for [`PostCompactAttachments`] that enforces token budgets.
#[derive(Debug, Default)]
pub struct AttachmentBuilder {
    skills: Vec<SkillAttachment>,
    plan: Option<PlanAttachment>,
    skill_chars_used: usize,
}

impl AttachmentBuilder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a skill attachment if within budget limits.
    pub fn add_skill(&mut self, name: impl Into<String>, content: impl Into<String>) -> bool {
        if self.skill_chars_used >= SKILL_ATTACHMENT_CHAR_BUDGET {
            return false;
        }
        let attachment = SkillAttachment::new(name, content);
        let cost = attachment.content.len();
        if self.skill_chars_used + cost > SKILL_ATTACHMENT_CHAR_BUDGET {
            return false;
        }
        self.skill_chars_used += cost;
        self.skills.push(attachment);
        true
    }

    /// Set the plan attachment (replaces any previous value).
    pub fn set_plan(&mut self, content: impl Into<String>) {
        self.plan = Some(PlanAttachment::new(content));
    }

    /// Finalize and return the collected attachments.
    pub fn build(self) -> PostCompactAttachments {
        PostCompactAttachments {
            skills: self.skills,
            plan: self.plan,
        }
    }
}

// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn skill_attachment_truncates_at_limit() {
        let long = "y".repeat(MAX_CHARS_PER_SKILL + 100);
        let a = SkillAttachment::new("my-skill", long);
        assert!(a.truncated);
        assert!(a.content.contains(SKILL_TRUNCATION_MARKER));
    }

    #[test]
    fn post_compact_attachments_to_messages_order() {
        let mut b = AttachmentBuilder::new();
        b.set_plan("do things");
        b.add_skill("my-skill", "skill content");
        let result = b.build();
        let msgs = result.to_messages();
        // Order: plan, then skills.
        assert_eq!(msgs.len(), 2);
        assert!(
            msgs[0]["content"]
                .as_str()
                .unwrap()
                .contains("current plan")
        );
        assert!(msgs[1]["content"].as_str().unwrap().contains("skill"));
    }
}
