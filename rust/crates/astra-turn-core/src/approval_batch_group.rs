//! Issue #326 P4 / R2 Major 1: UI batch grouping key.
//!
//! ## Why a separate key
//!
//! [`crate::approval_request_key::ApprovalRequestKey`] answers
//! "are these two pending tool calls **the same request**?" — it
//! must be precise (different args = different key). Plan v3 §P4
//! and the 50-scenario review (#16/#17/#18/#19) want a *second*
//! grouping that's coarser, used for visual aggregation only:
//!
//! > "20 read_file calls under src/auth/" → one batch card
//! > "5 edit_file diffs in src/" → one batch card
//! > "3 web_fetch calls to api.github.com" → one batch card
//!
//! These can't be merged with `ApprovalRequestKey` (per-item
//! exact key is what makes Allow safe), but they *can* be
//! grouped on the UI: "approve all 20" should be one user
//! gesture, not 20.
//!
//! Hence two keys, two contracts:
//!
//! - **Exact**: same `ApprovalRequestKey` → same approval slot
//!   (response broadcast to multiple senders).
//! - **Group**: same `ApprovalBatchGroupKey` → render together
//!   in the UI as a batch card; user actions on the card map
//!   to per-item Allow/Reject (so per-item args are still
//!   honoured).
//!
//! ## Group key shape
//!
//! Plan v3 P4 spells out the slots:
//!
//! - `tool_family` — `"Read"`, `"Edit"`, `"Bash(npm)"`,
//!   `"Network(api.github.com)"`. The first token of the rule
//!   grammar v2 plus a fast-path qualifier.
//! - `side_effect` — the engine's coarse class, projected to a
//!   string for `Eq + Hash`.
//! - `risk_tags` — sorted+stringified for stable hashing.
//! - `scope_root` — same package directory (P5 cwd-aware).
//! - `domain` — for network tools, the host.
//! - `source_agent` — different agents never share a card.
//! - `turn_id` — different turns never share a card.

use std::collections::BTreeSet;
use uuid::Uuid;

/// UI-only batch-grouping key.
///
/// Two requests with equal `ApprovalBatchGroupKey` are eligible
/// to be folded into the same batch card. They are NOT
/// equivalent for response purposes — the user still sees one
/// detail row per item and their Allow/Reject still applies
/// per-item.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ApprovalBatchGroupKey {
    /// The first segment of the rule grammar — `Bash(npm)` for
    /// shell, `Edit` for file-write, `Network(domain)` for
    /// network tools, etc. Allow rule families with the same
    /// scope and risk profile collapse onto the same card.
    pub tool_family: String,
    /// Side-effect projection (string so the type stays
    /// `Eq + Hash`). The engine's `SideEffect` enum is
    /// projected via `Display` at construction time.
    pub side_effect: String,
    /// Risk tags as sorted snake_case names — same set of
    /// classifications collapse together.
    pub risk_tags: BTreeSet<String>,
    /// Package root the requests live in. Different packages
    /// in a monorepo never share a batch card (scenario #28
    /// invariant).
    pub scope_root: Option<String>,
    /// For network tools: the destination host.
    pub domain: Option<String>,
    /// Sub-agent id, if any. Cross-agent batches are forbidden:
    /// the user must approve `[agent: A]` and `[agent: B]`
    /// requests separately.
    pub source_agent: Option<String>,
    /// LLM round id. Cross-turn batches are forbidden so
    /// "rest-of-turn" semantics stay clean.
    pub turn_id: Uuid,
}

impl ApprovalBatchGroupKey {
    /// Convenience constructor — most callers pre-compute the
    /// rule-family / scope-root / domain inside the engine and
    /// hand a partial value here.
    #[must_use]
    pub fn new(
        tool_family: impl Into<String>,
        side_effect: impl Into<String>,
        risk_tags: impl IntoIterator<Item = String>,
        turn_id: Uuid,
    ) -> Self {
        Self {
            tool_family: tool_family.into(),
            side_effect: side_effect.into(),
            risk_tags: risk_tags.into_iter().collect(),
            scope_root: None,
            domain: None,
            source_agent: None,
            turn_id,
        }
    }

    #[must_use]
    pub fn with_scope_root(mut self, root: impl Into<String>) -> Self {
        self.scope_root = Some(root.into());
        self
    }

    #[must_use]
    pub fn with_domain(mut self, domain: impl Into<String>) -> Self {
        self.domain = Some(domain.into());
        self
    }

    #[must_use]
    pub fn with_source_agent(mut self, agent: impl Into<String>) -> Self {
        self.source_agent = Some(agent.into());
        self
    }

    /// Returns true iff this group is safe to expose an
    /// "Accept all" button for. Plan v3 §P4 forbids one-click
    /// approval across destructive groups (the user must
    /// individually confirm rm -rf, git push --force, etc.).
    #[must_use]
    pub fn allows_accept_all(&self) -> bool {
        let dangerous = [
            "WritesSensitiveFile",
            "GitDestructive",
            "WritesOutsideWorkspace",
            "CredentialAccess",
            "MCPUnknownCapability",
        ];
        !self
            .risk_tags
            .iter()
            .any(|tag| dangerous.contains(&tag.as_str()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixed_turn() -> Uuid {
        Uuid::nil()
    }

    #[test]
    fn equal_groups_collapse() {
        let a = ApprovalBatchGroupKey::new(
            "Read",
            "ReadOnly",
            ["BashExecute".to_string()],
            fixed_turn(),
        )
        .with_scope_root("/repo/src");
        let b = ApprovalBatchGroupKey::new(
            "Read",
            "ReadOnly",
            ["BashExecute".to_string()],
            fixed_turn(),
        )
        .with_scope_root("/repo/src");
        assert_eq!(a, b);
    }

    #[test]
    fn different_scope_root_does_not_collapse() {
        // Scenario #28 invariant: web/ and api/ are different
        // packages even with identical tool/family.
        let web = ApprovalBatchGroupKey::new(
            "Bash(npm)",
            "Execute",
            ["BashExecute".to_string()],
            fixed_turn(),
        )
        .with_scope_root("/repo/web");
        let api = ApprovalBatchGroupKey::new(
            "Bash(npm)",
            "Execute",
            ["BashExecute".to_string()],
            fixed_turn(),
        )
        .with_scope_root("/repo/api");
        assert_ne!(web, api);
    }

    #[test]
    fn different_source_agent_does_not_collapse() {
        let parent = ApprovalBatchGroupKey::new(
            "Bash(npm)",
            "Execute",
            ["BashExecute".to_string()],
            fixed_turn(),
        );
        let child = parent.clone().with_source_agent("review");
        assert_ne!(parent, child);
    }

    #[test]
    fn different_domain_does_not_collapse() {
        // scenario #19: 3 distinct domains must yield 3 separate
        // batch cards.
        let github = ApprovalBatchGroupKey::new(
            "Network",
            "NetworkOpen",
            ["NetworkExfiltration".to_string()],
            fixed_turn(),
        )
        .with_domain("api.github.com");
        let jira = ApprovalBatchGroupKey::new(
            "Network",
            "NetworkOpen",
            ["NetworkExfiltration".to_string()],
            fixed_turn(),
        )
        .with_domain("jira.example.com");
        assert_ne!(github, jira);
    }

    #[test]
    fn different_turn_does_not_collapse() {
        let a = ApprovalBatchGroupKey::new(
            "Bash(npm)",
            "Execute",
            ["BashExecute".to_string()],
            Uuid::nil(),
        );
        let b = ApprovalBatchGroupKey::new(
            "Bash(npm)",
            "Execute",
            ["BashExecute".to_string()],
            Uuid::from_u128(42),
        );
        assert_ne!(a, b);
    }

    #[test]
    fn risk_tags_are_order_independent() {
        let a = ApprovalBatchGroupKey::new(
            "Edit",
            "Write",
            ["WritesOutsidePackage".to_string(), "BashExecute".to_string()],
            fixed_turn(),
        );
        let b = ApprovalBatchGroupKey::new(
            "Edit",
            "Write",
            ["BashExecute".to_string(), "WritesOutsidePackage".to_string()],
            fixed_turn(),
        );
        assert_eq!(a, b, "BTreeSet should make tag order irrelevant");
    }

    #[test]
    fn allows_accept_all_for_safe_groups() {
        let safe = ApprovalBatchGroupKey::new(
            "Read",
            "ReadOnly",
            ["BashExecute".to_string()],
            fixed_turn(),
        );
        assert!(safe.allows_accept_all());
    }

    #[test]
    fn forbids_accept_all_for_destructive_groups() {
        // Plan v3 §P4: user must confirm destructive items
        // individually.
        for tag in [
            "WritesSensitiveFile",
            "GitDestructive",
            "WritesOutsideWorkspace",
            "CredentialAccess",
            "MCPUnknownCapability",
        ] {
            let group = ApprovalBatchGroupKey::new(
                "Bash(rm)",
                "Execute",
                [tag.to_string()],
                fixed_turn(),
            );
            assert!(
                !group.allows_accept_all(),
                "tag {tag} should disable Accept all"
            );
        }
    }
}
