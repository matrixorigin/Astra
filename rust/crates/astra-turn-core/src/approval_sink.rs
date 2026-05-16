//! Issue #326 P2: approval sinks.
//!
//! When [`crate::permission_engine::evaluate_permission`] returns
//! [`HardDecision::NeedExternal`][crate::permission_engine::HardDecision::NeedExternal],
//! the host needs to ask *somebody*. Three things might be that
//! somebody:
//!
//! - The user, via the TUI approval queue ([`TuiApprovalSink`]).
//!   Used in `astra` / `astra --tui` when a NeedApproval rises out
//!   of the engine.
//!
//! - A parent agent, via the agent mailbox ([`MailboxApprovalSink`]).
//!   Used by sub-agents that don't own the TUI but still want to
//!   ask an upstream interactive session.
//!
//! - Nobody, with a fail-closed contract ([`HeadlessFailClosedSink`]).
//!   Used by `astra exec` / `astra -p` and any sub-run that has
//!   neither a TUI nor a parent that can answer. This sink turns
//!   every NeedExternal into a Deny with a clear reason
//!   ("approval required but no TUI; pass --mode auto or add allow
//!   rule") so the LLM gets a useful error instead of the request
//!   silently hanging.
//!
//! The trait is `async` to fit naturally on top of
//! `tokio::sync::oneshot::Receiver<ApprovalResponse>` — the existing
//! TUI plumbing already speaks oneshot, and the runtime mailbox is
//! also async.

use crate::permission_engine::{ApprovalPrompt, HardDecision};

/// User's response to an approval prompt at the sink layer.
///
/// Mirrors `astra_cli::cli::chat_stream::params::ApprovalResponse`
/// but lives in turn-core so sub-agents and headless callers can
/// produce one without depending on the CLI crate.
///
/// Issue #326 P0: `AutoRunSession` was removed (its semantics — flip
/// the whole session into Auto mode — clashed with P3's per-fingerprint
/// `RestOfSession` scope). Global mode changes go through a separate
/// surface (status line / `/mode auto` slash command), keeping this
/// enum focused on per-call decisions.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalResponse {
    AllowOnce,
    AlwaysAllow,
    Deny,
}

impl ApprovalResponse {
    #[must_use]
    pub fn is_approved(self) -> bool {
        matches!(self, Self::AllowOnce | Self::AlwaysAllow)
    }
}

/// Trait every entry point must implement to satisfy the engine's
/// `NeedExternal` contract.
///
/// Implementors are expected to ALWAYS resolve the future — either
/// with a real user/parent decision, or with a fail-closed `Deny`
/// when interaction isn't possible. Returning a value that maps to
/// `Allow` without an explicit user decision violates the
/// bypass-immunity contract.
#[async_trait::async_trait]
pub trait ApprovalSink: Send + Sync {
    /// Ask the sink to resolve the prompt. Implementors may return
    /// any of the [`ApprovalResponse`] variants; the engine maps
    /// them back to a [`HardDecision`].
    async fn ask(&self, prompt: ApprovalPrompt) -> ApprovalResponse;

    /// Convenience wrapper that converts the sink's response back
    /// into a `HardDecision`. Default impl is enough for most
    /// sinks.
    async fn resolve(&self, prompt: ApprovalPrompt) -> HardDecision {
        let prompt_for_deny = prompt.clone();
        match self.ask(prompt).await {
            ApprovalResponse::AllowOnce | ApprovalResponse::AlwaysAllow => HardDecision::Allow,
            ApprovalResponse::Deny => HardDecision::Deny {
                reason: format!(
                    "User denied approval for {}: {}",
                    prompt_for_deny.tool, prompt_for_deny.header
                ),
            },
        }
    }
}

/// Headless fail-closed sink — used when there's nothing that can
/// answer (no TUI, no parent mailbox).
///
/// Always returns `Deny` with a reason that points the user at the
/// fix (`--mode auto`). This is the contract that protects
/// `astra -p` / `astra exec` / sub-runs from silently hanging on
/// approvals.
pub struct HeadlessFailClosedSink;

#[async_trait::async_trait]
impl ApprovalSink for HeadlessFailClosedSink {
    async fn ask(&self, _prompt: ApprovalPrompt) -> ApprovalResponse {
        // Maps to the standard ApprovalResponse::Deny in `resolve`,
        // which produces a HardDecision::Deny with a clear reason.
        ApprovalResponse::Deny
    }

    async fn resolve(&self, prompt: ApprovalPrompt) -> HardDecision {
        HardDecision::Deny {
            reason: format!(
                "approval required for {} but no interactive sink available; \
                 pass --mode auto or add an allow rule \
                 (rule preview: {})",
                prompt.tool, prompt.header
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::permission_engine::{ApprovalPrompt, HardDecision};

    fn fixture_prompt() -> ApprovalPrompt {
        ApprovalPrompt {
            tool: "bash".to_string(),
            header: "rm -rf /tmp/something".to_string(),
            detail: None,
            reason: "execute".to_string(),
            risk_tags: Vec::new(),
        }
    }

    #[tokio::test]
    async fn approval_response_is_approved_correctly() {
        assert!(ApprovalResponse::AllowOnce.is_approved());
        assert!(ApprovalResponse::AlwaysAllow.is_approved());
        assert!(!ApprovalResponse::Deny.is_approved());
    }

    #[tokio::test]
    async fn headless_sink_always_denies_with_actionable_reason() {
        let sink = HeadlessFailClosedSink;
        let decision = sink.resolve(fixture_prompt()).await;
        match decision {
            HardDecision::Deny { reason } => {
                assert!(
                    reason.contains("--mode auto"),
                    "deny reason must point user to a fix, got: {reason}"
                );
                assert!(
                    reason.contains("bash"),
                    "deny reason must include the tool name, got: {reason}"
                );
            }
            other => panic!("HeadlessFailClosedSink must Deny; got {other:?}"),
        }
    }

    /// Mock sink for testing — answers Yes regardless. We use this
    /// to verify the trait's default `resolve` impl maps `AllowOnce`
    /// to `HardDecision::Allow`.
    struct YesSink;
    #[async_trait::async_trait]
    impl ApprovalSink for YesSink {
        async fn ask(&self, _prompt: ApprovalPrompt) -> ApprovalResponse {
            ApprovalResponse::AllowOnce
        }
    }

    #[tokio::test]
    async fn yes_sink_resolves_to_allow() {
        let sink = YesSink;
        let decision = sink.resolve(fixture_prompt()).await;
        assert_eq!(decision, HardDecision::Allow);
    }

    /// Sub-agent mailbox simulation: parent denies the request.
    struct DenyingParent;
    #[async_trait::async_trait]
    impl ApprovalSink for DenyingParent {
        async fn ask(&self, _prompt: ApprovalPrompt) -> ApprovalResponse {
            ApprovalResponse::Deny
        }
    }

    #[tokio::test]
    async fn denying_parent_resolves_to_deny_with_user_reason() {
        let sink = DenyingParent;
        let decision = sink.resolve(fixture_prompt()).await;
        match decision {
            HardDecision::Deny { reason } => {
                assert!(reason.contains("User denied"));
                assert!(reason.contains("bash"));
            }
            other => panic!("expected Deny, got {other:?}"),
        }
    }
}
