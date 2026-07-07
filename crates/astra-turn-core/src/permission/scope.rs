//! Issue #326 P3 / R2 Major 1 / scenarios #6/#9/#15: scope-aware
//! "Always allow" policy.
//!
//! Plan v3 §P3 introduces a five-variant scope picker for the
//! Always button:
//!
//! - `OnceThisCall`   — equivalent to AllowOnce, no persistence
//! - `RestOfTurn`     — auto-approve identical fingerprints for
//!   this LLM round only
//! - `RestOfSession`  — auto-approve until the session ends
//!   (per-fingerprint, NOT a global mode flip)
//! - `Project`        — write to .astra/permissions.json
//! - `User`           — write to ~/.astra/permissions.json
//!
//! Plan v3 §P3 also says destructive risk tags
//! (rm -rf-style, git push --force, chmod 777) MUST disable
//! the Project / User scopes — those are persistent rules
//! that the user almost never wants to grant unconditionally.
//! "Allow once" is fine, "always allow rm -rf" almost never is.
//!
//! This module owns the policy decision. The actual scope
//! picker UI in the TUI (a dropdown or chip row) calls
//! `permitted_scopes(...)` to know which entries to grey out.

use crate::permission::engine::RiskTag;
use crate::permission::memory_profile::{PersistentMemoryBlock, permission_memory_profile};

/// Allowed-rule scope. Mirrors the dropdown choices in the
/// approval card.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AllowScope {
    /// One call only; no persistence anywhere.
    OnceThisCall,
    /// This LLM round only; identical fingerprints don't re-prompt.
    RestOfTurn,
    /// Until session ends; per-fingerprint, not a global Auto flip.
    RestOfSession,
    /// Persist to .astra/permissions.json (project-shared).
    Project,
    /// Persist to ~/.astra/permissions.json (per-user).
    User,
}

impl AllowScope {
    /// True iff selecting this scope writes to a file on disk.
    #[must_use]
    pub fn persists(self) -> bool {
        matches!(self, Self::Project | Self::User)
    }

    /// True iff selecting this scope outlives the current LLM
    /// round.
    #[must_use]
    pub fn outlives_turn(self) -> bool {
        !matches!(self, Self::OnceThisCall | Self::RestOfTurn)
    }
}

/// Reason a scope is greyed out in the dropdown.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ScopeUnavailableReason {
    /// Risk tags include a destructive class; persistent
    /// scopes are forbidden by policy.
    DestructiveRisk { tag: RiskTag },
    /// Compound shell command — argv is not a sound rule shape
    /// (which subcommand would the rule allow?).
    CompoundCommand,
    /// Sub-agent issued the request; only the user's own session
    /// should be able to extend project rules.
    SubAgentRequest,
    /// MCP tool with no capability metadata; we don't know what
    /// it does, so we won't let the user persist a rule.
    MCPUnknownCapability,
    /// Workspace not in the trust ledger; project rules are
    /// off-limits for this session.
    UntrustedWorkspace,
    /// Dynamic-eval shell ($(…) / backticks) — same reason as
    /// CompoundCommand: rule shape is undefined.
    DynamicEvalCommand,
    /// The tool request lacks a stable match target, so persisting
    /// an Always rule would become broader than the user approved.
    UnsafeRuleShape,
}

/// Inputs to the scope-availability decision. Aggregated as a
/// struct so the call site reads as plain English.
#[derive(Default, Debug, Clone)]
pub struct ScopeAvailabilityContext {
    pub risk_tags: Vec<RiskTag>,
    pub is_compound_command: bool,
    pub has_dynamic_eval: bool,
    pub source_agent_present: bool,
    pub mcp_unknown_capability: bool,
    pub workspace_untrusted: bool,
    pub unsafe_rule_shape: bool,
}

/// Decision for one scope: available or not, and why.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScopeAvailability {
    pub scope: AllowScope,
    pub available: bool,
    pub reason: Option<ScopeUnavailableReason>,
}

/// Build the scope policy input for one concrete tool request.
///
/// Risk classification is supplied by the permission engine so callers do not
/// duplicate git/path/MCP risk detection. This function owns the orthogonal
/// memory-shape facts: whether the request has a stable target and whether
/// that target is too broad to remember beyond the current turn.
#[must_use]
pub fn scope_context_for_tool_request(
    tool: &str,
    args: &serde_json::Value,
    risk_tags: Vec<RiskTag>,
    source_agent_present: bool,
    workspace_untrusted: bool,
) -> ScopeAvailabilityContext {
    let memory_profile = permission_memory_profile(tool, args);
    let mut ctx = ScopeAvailabilityContext {
        risk_tags,
        source_agent_present,
        workspace_untrusted,
        unsafe_rule_shape: !memory_profile.has_stable_target
            || matches!(
                memory_profile.persistent_block,
                Some(PersistentMemoryBlock::UnsafeRuleShape)
            ),
        is_compound_command: matches!(
            memory_profile.persistent_block,
            Some(PersistentMemoryBlock::CompoundCommand)
        ),
        has_dynamic_eval: matches!(
            memory_profile.persistent_block,
            Some(PersistentMemoryBlock::DynamicEval)
        ),
        ..Default::default()
    };

    if tool.starts_with("mcp_") {
        ctx.mcp_unknown_capability = true;
        if !ctx.risk_tags.contains(&RiskTag::MCPUnknownCapability) {
            ctx.risk_tags.push(RiskTag::MCPUnknownCapability);
        }
    }

    ctx
}

/// Pick the strongest safe default scope for the "Always" button.
#[must_use]
pub fn default_always_scope(ctx: &ScopeAvailabilityContext) -> AllowScope {
    let scopes = permitted_scopes(ctx);
    let is_available = |target| {
        scopes
            .iter()
            .any(|entry| entry.scope == target && entry.available)
    };

    if is_available(AllowScope::Project) {
        return AllowScope::Project;
    }
    if is_available(AllowScope::RestOfSession) {
        return AllowScope::RestOfSession;
    }
    if is_available(AllowScope::RestOfTurn) {
        return AllowScope::RestOfTurn;
    }

    AllowScope::OnceThisCall
}

/// Compute the scope picker state.
///
/// Returns one entry per [`AllowScope`] in dropdown order:
/// OnceThisCall → RestOfTurn → RestOfSession → Project → User.
/// Each entry carries an `available` flag and (when not
/// available) the dominant reason.
#[must_use]
pub fn permitted_scopes(ctx: &ScopeAvailabilityContext) -> Vec<ScopeAvailability> {
    [
        AllowScope::OnceThisCall,
        AllowScope::RestOfTurn,
        AllowScope::RestOfSession,
        AllowScope::Project,
        AllowScope::User,
    ]
    .iter()
    .map(|scope| {
        let reason = unavailable_reason_for(*scope, ctx);
        ScopeAvailability {
            scope: *scope,
            available: reason.is_none(),
            reason,
        }
    })
    .collect()
}

fn unavailable_reason_for(
    scope: AllowScope,
    ctx: &ScopeAvailabilityContext,
) -> Option<ScopeUnavailableReason> {
    // OnceThisCall and RestOfTurn are always allowed — they are
    // the soft-default the user should be able to fall back to
    // when nothing else fits.
    if matches!(scope, AllowScope::OnceThisCall | AllowScope::RestOfTurn) {
        return None;
    }

    // Compound shell + dynamic eval forbid ANY scope that
    // outlives the current call. The rule shape isn't sound.
    if ctx.is_compound_command && scope.outlives_turn() {
        return Some(ScopeUnavailableReason::CompoundCommand);
    }
    if ctx.has_dynamic_eval && scope.outlives_turn() {
        return Some(ScopeUnavailableReason::DynamicEvalCommand);
    }
    if ctx.unsafe_rule_shape && scope.outlives_turn() {
        return Some(ScopeUnavailableReason::UnsafeRuleShape);
    }

    // Sub-agent requests can never extend project / user rules
    // on the parent's behalf.
    if ctx.source_agent_present && scope.persists() {
        return Some(ScopeUnavailableReason::SubAgentRequest);
    }

    // MCP unknown capability blocks persistent scopes — we
    // don't know what the tool does.
    if ctx.mcp_unknown_capability && scope.persists() {
        return Some(ScopeUnavailableReason::MCPUnknownCapability);
    }

    // Untrusted workspace blocks Project (it's the file from
    // that workspace). User scope is still allowed — the user's
    // own ~/.astra/permissions.json is theirs to extend.
    if ctx.workspace_untrusted && scope == AllowScope::Project {
        return Some(ScopeUnavailableReason::UntrustedWorkspace);
    }

    // Destructive risk tags block project / user persistence.
    let destructive = [
        RiskTag::WritesSensitiveFile,
        RiskTag::GitDestructive,
        RiskTag::WritesOutsideWorkspace,
        RiskTag::CredentialAccess,
    ];
    if scope.persists() {
        for tag in destructive {
            if ctx.risk_tags.contains(&tag) {
                return Some(ScopeUnavailableReason::DestructiveRisk { tag });
            }
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx() -> ScopeAvailabilityContext {
        ScopeAvailabilityContext::default()
    }

    fn availability(scopes: &[ScopeAvailability], target: AllowScope) -> &ScopeAvailability {
        scopes.iter().find(|s| s.scope == target).unwrap()
    }

    #[test]
    fn benign_request_allows_all_scopes() {
        let scopes = permitted_scopes(&ctx());
        for scope in scopes {
            assert!(
                scope.available,
                "{scope:?} should be available for benign request"
            );
        }
    }

    #[test]
    fn destructive_tag_blocks_project_and_user() {
        let mut c = ctx();
        c.risk_tags.push(RiskTag::GitDestructive);
        let scopes = permitted_scopes(&c);

        assert!(availability(&scopes, AllowScope::OnceThisCall).available);
        assert!(availability(&scopes, AllowScope::RestOfTurn).available);
        assert!(availability(&scopes, AllowScope::RestOfSession).available);
        assert!(!availability(&scopes, AllowScope::Project).available);
        assert!(!availability(&scopes, AllowScope::User).available);

        assert!(matches!(
            availability(&scopes, AllowScope::Project).reason,
            Some(ScopeUnavailableReason::DestructiveRisk {
                tag: RiskTag::GitDestructive
            })
        ));
    }

    #[test]
    fn writes_outside_workspace_blocks_persistence() {
        let mut c = ctx();
        c.risk_tags.push(RiskTag::WritesOutsideWorkspace);
        let scopes = permitted_scopes(&c);
        assert!(!availability(&scopes, AllowScope::Project).available);
        assert!(!availability(&scopes, AllowScope::User).available);
    }

    #[test]
    fn compound_command_blocks_anything_persistent() {
        let mut c = ctx();
        c.is_compound_command = true;
        let scopes = permitted_scopes(&c);
        // OnceThisCall + RestOfTurn still on; the rest off.
        assert!(availability(&scopes, AllowScope::OnceThisCall).available);
        assert!(availability(&scopes, AllowScope::RestOfTurn).available);
        assert!(!availability(&scopes, AllowScope::RestOfSession).available);
        assert!(!availability(&scopes, AllowScope::Project).available);
        assert!(!availability(&scopes, AllowScope::User).available);
    }

    #[test]
    fn dynamic_eval_blocks_persistence_too() {
        let mut c = ctx();
        c.has_dynamic_eval = true;
        let scopes = permitted_scopes(&c);
        assert!(!availability(&scopes, AllowScope::Project).available);
        assert!(matches!(
            availability(&scopes, AllowScope::Project).reason,
            Some(ScopeUnavailableReason::DynamicEvalCommand)
        ));
    }

    #[test]
    fn unsafe_rule_shape_blocks_anything_beyond_turn() {
        let mut c = ctx();
        c.unsafe_rule_shape = true;
        let scopes = permitted_scopes(&c);
        assert!(availability(&scopes, AllowScope::OnceThisCall).available);
        assert!(availability(&scopes, AllowScope::RestOfTurn).available);
        assert!(!availability(&scopes, AllowScope::RestOfSession).available);
        assert!(!availability(&scopes, AllowScope::Project).available);
        assert!(matches!(
            availability(&scopes, AllowScope::Project).reason,
            Some(ScopeUnavailableReason::UnsafeRuleShape)
        ));
    }

    #[test]
    fn sub_agent_blocks_project_and_user() {
        let mut c = ctx();
        c.source_agent_present = true;
        let scopes = permitted_scopes(&c);
        assert!(!availability(&scopes, AllowScope::Project).available);
        assert!(!availability(&scopes, AllowScope::User).available);
        assert!(availability(&scopes, AllowScope::RestOfTurn).available);
        assert!(matches!(
            availability(&scopes, AllowScope::Project).reason,
            Some(ScopeUnavailableReason::SubAgentRequest)
        ));
    }

    #[test]
    fn untrusted_workspace_blocks_project_only() {
        let mut c = ctx();
        c.workspace_untrusted = true;
        let scopes = permitted_scopes(&c);
        assert!(!availability(&scopes, AllowScope::Project).available);
        // User scope is still allowed — it's the user's own file.
        assert!(availability(&scopes, AllowScope::User).available);
    }

    #[test]
    fn mcp_unknown_capability_blocks_persistence() {
        let mut c = ctx();
        c.mcp_unknown_capability = true;
        let scopes = permitted_scopes(&c);
        assert!(!availability(&scopes, AllowScope::Project).available);
        assert!(!availability(&scopes, AllowScope::User).available);
        assert!(availability(&scopes, AllowScope::RestOfTurn).available);
    }

    #[test]
    fn allow_scope_serializes_as_snake_case() {
        assert_eq!(
            serde_json::to_string(&AllowScope::RestOfSession).unwrap(),
            "\"rest_of_session\""
        );
        assert_eq!(
            serde_json::to_string(&AllowScope::OnceThisCall).unwrap(),
            "\"once_this_call\""
        );
    }

    #[test]
    fn persists_helper_distinguishes_disk_scopes() {
        assert!(AllowScope::Project.persists());
        assert!(AllowScope::User.persists());
        assert!(!AllowScope::OnceThisCall.persists());
        assert!(!AllowScope::RestOfTurn.persists());
        assert!(!AllowScope::RestOfSession.persists());
    }

    #[test]
    fn outlives_turn_helper() {
        assert!(!AllowScope::OnceThisCall.outlives_turn());
        assert!(!AllowScope::RestOfTurn.outlives_turn());
        assert!(AllowScope::RestOfSession.outlives_turn());
        assert!(AllowScope::Project.outlives_turn());
        assert!(AllowScope::User.outlives_turn());
    }
}
