//! Emergent context: data discovered during execution, fed to the next turn.
//!
//! Emergent context flows backward from Execute(N) to Bind(N+1). Without
//! guardrails, stale attachments accumulate, duplicates inflate token usage,
//! and unbounded lists cause the next turn to overflow.
//!
//! Every item carries TTL (turn-based), a dedup hash, and each list has a cap.

use serde::{Deserialize, Serialize};

/// Default TTL: items are consumed on the immediately next turn only.
pub const DEFAULT_MAX_AGE: u32 = 1;

/// Wrapper enforcing TTL and dedup for every emergent item.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EmergentItem<T> {
    pub value: T,
    /// Turn number when this item was created. Bind skips items where
    /// `current_turn - created_at_turn > max_age`.
    pub created_at_turn: u32,
    /// Dedup key. Execute rejects duplicates at write time.
    pub content_hash: u64,
}

/// Context discovered during the previous turn's execution.
///
/// Lifecycle: populated by Execute(turn N), consumed by Bind(turn N+1),
/// then cleared.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct EmergentContext {
    /// Skills discovered from tool execution. Cap: 4.
    pub discovered_skills: Vec<EmergentItem<DiscoveredSkill>>,
    /// Memory prefetched during model streaming. Cap: 8.
    pub prefetched_memory: Vec<EmergentItem<PrefetchedMemory>>,
    /// Tool use summaries from a smaller model. Cap: 1 (latest wins).
    pub tool_summaries: Vec<EmergentItem<ToolUseSummary>>,
    /// Attachment-style context from tool execution side-effects. Cap: 16.
    pub attachments: Vec<EmergentItem<Attachment>>,
}

const SKILL_CAP: usize = 4;
const MEMORY_CAP: usize = 8;
const SUMMARY_CAP: usize = 1;
const ATTACHMENT_CAP: usize = 16;

impl EmergentContext {
    pub fn push_skill(&mut self, item: EmergentItem<DiscoveredSkill>) {
        push_with_dedup_and_cap(&mut self.discovered_skills, item, SKILL_CAP);
    }

    pub fn push_memory(&mut self, item: EmergentItem<PrefetchedMemory>) {
        push_with_dedup_and_cap(&mut self.prefetched_memory, item, MEMORY_CAP);
    }

    pub fn push_summary(&mut self, item: EmergentItem<ToolUseSummary>) {
        push_with_dedup_and_cap(&mut self.tool_summaries, item, SUMMARY_CAP);
    }

    pub fn push_attachment(&mut self, item: EmergentItem<Attachment>) {
        push_with_dedup_and_cap(&mut self.attachments, item, ATTACHMENT_CAP);
    }

    /// Drain items within TTL, clearing consumed items from this context.
    /// Returns a new EmergentContext containing only live items.
    ///
    /// **Consumption semantics**: this is a one-shot drain. After the call,
    /// `self` is empty — a second call in the same turn returns an empty context.
    pub fn drain_live(&mut self, current_turn: u32, max_age: u32) -> EmergentContext {
        EmergentContext {
            discovered_skills: drain_live_items(&mut self.discovered_skills, current_turn, max_age),
            prefetched_memory: drain_live_items(&mut self.prefetched_memory, current_turn, max_age),
            tool_summaries: drain_live_items(&mut self.tool_summaries, current_turn, max_age),
            attachments: drain_live_items(&mut self.attachments, current_turn, max_age),
        }
    }

    /// Whether all lists are empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.discovered_skills.is_empty()
            && self.prefetched_memory.is_empty()
            && self.tool_summaries.is_empty()
            && self.attachments.is_empty()
    }
}

fn push_with_dedup_and_cap<T>(list: &mut Vec<EmergentItem<T>>, item: EmergentItem<T>, cap: usize) {
    // Dedup: reject if same hash already exists
    if list
        .iter()
        .any(|existing| existing.content_hash == item.content_hash)
    {
        return;
    }
    // Cap: drop oldest if at capacity. Caps are deliberately tiny (<=16), so
    // Vec::remove(0) keeps the API simple without material runtime cost.
    while list.len() >= cap {
        list.remove(0);
    }
    list.push(item);
}

fn drain_live_items<T: Clone>(
    source: &mut Vec<EmergentItem<T>>,
    current_turn: u32,
    max_age: u32,
) -> Vec<EmergentItem<T>> {
    let mut live = Vec::new();

    for item in source.iter() {
        if current_turn.saturating_sub(item.created_at_turn) <= max_age {
            live.push(item.clone());
        }
    }

    // Drain is a consumption boundary: live items move into the returned
    // context, and expired items are discarded so stale data cannot resurface.
    source.clear();

    live
}

// ── Placeholder content types ──────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiscoveredSkill {
    pub skill_name: String,
    pub trigger: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PrefetchedMemory {
    pub content: String,
    pub relevance_score: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolUseSummary {
    pub summary: String,
    pub tool_calls_covered: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Attachment {
    pub kind: String,
    pub content: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_item<T>(value: T, turn: u32, hash: u64) -> EmergentItem<T> {
        EmergentItem {
            value,
            created_at_turn: turn,
            content_hash: hash,
        }
    }

    fn make_skill(name: &str, turn: u32, hash: u64) -> EmergentItem<DiscoveredSkill> {
        make_item(
            DiscoveredSkill {
                skill_name: name.to_string(),
                trigger: "test".to_string(),
            },
            turn,
            hash,
        )
    }

    #[test]
    fn push_enforces_cap() {
        let mut ctx = EmergentContext::default();
        for i in 0..5 {
            ctx.push_skill(make_skill(&format!("skill_{i}"), 1, i as u64));
        }
        assert_eq!(ctx.discovered_skills.len(), SKILL_CAP);
        // Oldest (skill_0) should have been dropped
        assert_eq!(ctx.discovered_skills[0].value.skill_name, "skill_1");
        assert_eq!(ctx.discovered_skills[3].value.skill_name, "skill_4");
    }

    #[test]
    fn drain_live_treats_future_created_turn_as_live_once_then_clears_source() {
        let mut ctx = EmergentContext::default();
        ctx.push_skill(make_skill("future", 10, 10));

        let live = ctx.drain_live(5, DEFAULT_MAX_AGE);

        assert_eq!(live.discovered_skills.len(), 1);
        assert_eq!(live.discovered_skills[0].value.skill_name, "future");
        assert!(ctx.discovered_skills.is_empty());
    }

    #[test]
    fn push_dedup_by_hash() {
        let mut ctx = EmergentContext::default();
        ctx.push_skill(make_skill("first", 1, 42));
        ctx.push_skill(make_skill("second", 1, 42)); // same hash
        assert_eq!(ctx.discovered_skills.len(), 1);
        assert_eq!(ctx.discovered_skills[0].value.skill_name, "first");
    }

    #[test]
    fn drain_live_respects_ttl() {
        let mut ctx = EmergentContext::default();
        ctx.push_skill(make_skill("old", 1, 1));
        ctx.push_skill(make_skill("new", 5, 2));

        let live = ctx.drain_live(5, DEFAULT_MAX_AGE);
        assert_eq!(live.discovered_skills.len(), 1);
        assert_eq!(live.discovered_skills[0].value.skill_name, "new");
        assert!(
            ctx.discovered_skills.is_empty(),
            "expired and consumed items must both be removed from source"
        );
    }

    #[test]
    fn drain_live_clears_consumed() {
        let mut ctx = EmergentContext::default();
        ctx.push_skill(make_skill("a", 5, 1));
        ctx.push_skill(make_skill("b", 5, 2));

        let live = ctx.drain_live(5, DEFAULT_MAX_AGE);
        assert_eq!(live.discovered_skills.len(), 2);
        // Source should be empty after drain
        assert!(ctx.discovered_skills.is_empty());
    }

    #[test]
    fn summary_cap_is_one() {
        let mut ctx = EmergentContext::default();
        ctx.push_summary(make_item(
            ToolUseSummary {
                summary: "first".into(),
                tool_calls_covered: 3,
            },
            1,
            1,
        ));
        ctx.push_summary(make_item(
            ToolUseSummary {
                summary: "second".into(),
                tool_calls_covered: 5,
            },
            1,
            2,
        ));
        assert_eq!(ctx.tool_summaries.len(), 1);
        assert_eq!(ctx.tool_summaries[0].value.summary, "second");
    }

    #[test]
    fn empty_context_is_empty() {
        let ctx = EmergentContext::default();
        assert!(ctx.is_empty());
    }

    #[test]
    fn drain_live_double_call_returns_empty_second_time() {
        let mut ctx = EmergentContext::default();
        ctx.push_skill(make_skill("alpha", 5, 100));
        ctx.push_skill(make_skill("beta", 5, 200));

        // First drain: returns both live items
        let first = ctx.drain_live(5, DEFAULT_MAX_AGE);
        assert_eq!(first.discovered_skills.len(), 2);

        // Second drain in same turn: source is empty, returns nothing
        let second = ctx.drain_live(5, DEFAULT_MAX_AGE);
        assert!(second.is_empty());
        assert!(ctx.is_empty());
    }
}
