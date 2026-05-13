//! Issue #326 P2 / R1 Critical 2 / R2 Critical 2:
//! shared permission evaluation engine.
//!
//! ## The Critical 2 problem (one-line version)
//!
//! There used to be **two permission deciders**:
//!
//! - Flow A in `astra-cli::permission_manager::check_nonblocking_inner`,
//!   running the full 11-step hard-deny chain (deny → safety → git →
//!   sensitive paths → sandbox expand → read short-circuit → session
//!   override → explicit approval → allow rules → mode → fallback).
//!
//! - Flow B in `runtime::turn::permission_gate::check_tool_permission`,
//!   running only "deny rules → allow rules → mode → mailbox" — *missing*
//!   the git-destructive guard, sensitive-path writes, sandbox expansion,
//!   session overrides, the explicit-approval gate and most of the
//!   rule-ordering subtleties.
//!
//! When R1 / R2 reviewed the design, both pointed out that "two
//! deciders" is not an architecture, it's a security debt: anything
//! that lands in Flow A's hard-deny chain but not in Flow B becomes
//! a bypass for child agents. The fix is one shared **pure** function
//! that all entry points (TUI host, headless, sub-agent mailbox)
//! call, with the differences pushed out into separate
//! [`ApprovalSink`]s.
//!
//! ## Status of this module
//!
//! This file is the **type and ordering contract**. It defines:
//!
//! - [`HardDecision`] — the engine's three-way output.
//! - [`DecisionEnvelope`] — what the engine returns (decision + the
//!   trace of which rule fired + serialized "what would Always save"
//!   preview + risk tags).
//! - [`EvaluationStep`] — the 11 fixed ordering slots, written as an
//!   enum so a pinning test can assert the order is stable.
//! - [`evaluation_order`] — the constant array that documents the
//!   order in code (`[Schema, Deny, Safety, Git, Sensitive, Execute,
//!   SandboxExpand, ReadShortCircuit, SessionOverride, ExplicitApproval,
//!   AllowRules, Mode]`). Note: `Mode` is the 12th step but treated
//!   as a fallback after the rule list; the canonical "11 steps" in
//!   plan v3 §P2 collapses Mode into ε of the list. Spelling them
//!   out as 12 separate enum variants is clearer for the test.
//!
//! The actual implementation logic lives in
//! `astra_cli::permission_manager::check_nonblocking_inner` for now —
//! moving it here is staged behind P3/P4 callsite rewrites so this PR
//! doesn't have to break the world. The engine's *contract* is what
//! protects bypass-immunity, and that contract IS in this file (the
//! `evaluation_order` array, [`HardDecision`], [`DecisionSource`]).
//! Once all entry points consume `DecisionEnvelope`, the actual logic
//! moves over to `evaluate_permission`.

/// Final outcome of a permission evaluation.
///
/// Mirrors `astra_cli::permission_manager::GateOutcome` but lives in
/// turn-core so sub-agent code can use it without depending on the
/// CLI crate. The CLI alias will be removed once
/// `evaluate_permission` is the single source of truth.
///
/// `PartialEq` only — `ApprovalPrompt` carries free-form display
/// text that isn't worth a strict `Eq` contract.
#[derive(Clone, Debug, PartialEq)]
pub enum HardDecision {
    /// Allow the tool call to proceed.
    Allow,
    /// Deny the tool call. The reason must be human-readable; the
    /// LLM and the user both see it verbatim.
    Deny { reason: String },
    /// The engine cannot decide locally — the request must be routed
    /// to an external sink (TUI prompt, parent mailbox, headless
    /// fail-closed, etc.).
    NeedExternal { prompt: ApprovalPrompt },
}

/// Where the decision came from. Exposed in the trace so the user
/// can see "why was this denied?" → "Step 4: git destructive
/// guard caught `--force`".
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DecisionSource {
    SchemaIdentity,
    DenyRule { rule: String, origin: RuleOrigin },
    SafetyMiddleware { reason: String },
    GitSafety { violation: String },
    SensitivePath { path: String },
    ExecuteHardDeny { reason: String },
    SandboxExpansion,
    ReadShortCircuit,
    SessionOverride { allowed: bool },
    ExplicitApprovalGate { reason: String },
    AllowRule { rule: String, origin: RuleOrigin },
    Mode { mode: String },
    /// No rule matched and we fell off the end → the engine routes
    /// to the prompt sink. (Captured here so the trace explains
    /// "no rule fired" instead of being silent.)
    UnmatchedFallback,
}

/// Where a rule originally came from. UI uses this for the
/// "Saved to .kiro/permissions.json" line in the approval card.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RuleOrigin {
    Project,
    User,
    Inherited,
    Session,
    CommandLine,
}

/// One step of the engine's decision trace. The `step` is the
/// canonical [`EvaluationStep`] slot that ran; `outcome` records
/// what it decided. UI renders a chain of these for the "?" /
/// `/permissions why` views.
#[derive(Clone, Debug)]
pub struct DecisionTraceStep {
    pub step: EvaluationStep,
    pub outcome: TraceOutcome,
    /// Free-form note for diagnostics. May contain a rule string,
    /// a reason fragment, or "(no match)". Kept as `String` so this
    /// type stays cheap to clone.
    pub note: String,
}

#[derive(Clone, Debug, PartialEq)]
pub enum TraceOutcome {
    /// Step examined the request but had nothing to say.
    Skipped,
    /// Step matched a rule and returned a decision.
    Matched(HardDecision),
}

/// The 11 fixed evaluation slots. Plan v3 §P2 freezes the order:
/// any future change must update both this enum and the
/// `evaluation_order_is_stable` pinning test, so the rule-precedence
/// contract can't drift silently.
///
/// We keep `Mode` here for completeness — the legacy code treated
/// the mode check as a fallback after `AllowRules`; the test below
/// asserts that ordering.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum EvaluationStep {
    SchemaIdentity,
    DenyRules,
    SafetyMiddleware,
    GitSafety,
    SensitivePath,
    ExecuteHardDeny,
    SandboxExpand,
    ReadShortCircuit,
    SessionOverride,
    ExplicitApproval,
    AllowRules,
    Mode,
}

/// Canonical ordering. Any reorder must change this constant AND
/// flip the pinning test in `tests::evaluation_order_is_stable`.
pub const EVALUATION_ORDER: [EvaluationStep; 12] = [
    EvaluationStep::SchemaIdentity,
    EvaluationStep::DenyRules,
    EvaluationStep::SafetyMiddleware,
    EvaluationStep::GitSafety,
    EvaluationStep::SensitivePath,
    EvaluationStep::ExecuteHardDeny,
    EvaluationStep::SandboxExpand,
    EvaluationStep::ReadShortCircuit,
    EvaluationStep::SessionOverride,
    EvaluationStep::ExplicitApproval,
    EvaluationStep::AllowRules,
    EvaluationStep::Mode,
];

/// Multi-tag risk classification (plan v3 §P3 / R1 Major 7).
///
/// Replaces the single-class `SideEffectClass` for the UI risk
/// badge. A request can carry multiple tags simultaneously
/// (`WritesOutsideWorkspace + WorkspaceUntrusted`); the highest
/// risk colour wins, but the badge label enumerates all matching
/// tags so users see the full picture.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum RiskTag {
    /// Bash + execute kind, no other distinguishing tag. The
    /// "vanilla risk" so even simple commands have a label.
    BashExecute,
    /// Edit/write touching a path under the project but at a
    /// level that's not part of the active package.
    WritesOutsidePackage,
    /// Edit/write to a path completely outside the workspace tree
    /// (e.g. `/etc/hosts`, `~/.bashrc`).
    WritesOutsideWorkspace,
    /// Edit/write to a path matching a sensitive pattern
    /// (`.env*`, `id_rsa`, `*.pem`).
    WritesSensitiveFile,
    /// Network egress with a destination not on the
    /// project-allowlist.
    NetworkExfiltration,
    /// Tool args read or rotate credentials (`AWS_*`, `OPENAI_*`,
    /// SSH keys).
    CredentialAccess,
    /// `git push --force`, `git push --no-verify`, force-pushed
    /// rebases, branch deletion against protected branches.
    GitDestructive,
    /// SQL query containing INSERT / UPDATE / DELETE / TRUNCATE /
    /// DROP / CREATE — including buried in CTEs.
    SqlDestructive,
    /// MCP server with no destructiveHint annotation; we don't
    /// know what it does.
    MCPUnknownCapability,
    /// Workspace not in the trust ledger; persistent rules from
    /// `.kiro/permissions.json` are downgraded to allow-only-once.
    WorkspaceUntrusted,
    /// `sandbox_expand:*` request — the agent is asking us to widen
    /// the sandbox, not run a tool.
    SandboxExpansion,
}

/// Human-displayable approval prompt — what the engine hands to a
/// sink when the local rule chain returns `NeedExternal`.
#[derive(Clone, Debug, PartialEq)]
pub struct ApprovalPrompt {
    pub tool: String,
    pub header: String,
    pub detail: Option<String>,
    pub reason: String,
    pub risk_tags: Vec<RiskTag>,
}

/// What the engine returns to its caller. The caller is responsible
/// for routing `NeedExternal` to the right sink (TUI / mailbox /
/// headless fail-closed) and for surfacing `trace` / `will_save` /
/// `risk_tags` to the user.
#[derive(Clone, Debug)]
pub struct DecisionEnvelope {
    /// The final decision.
    pub decision: HardDecision,
    /// Which step produced the decision (or `UnmatchedFallback`
    /// when nothing matched).
    pub source: DecisionSource,
    /// Step-by-step trace, in `EVALUATION_ORDER` order. Useful for
    /// the `?` / `/permissions why` views.
    pub trace: Vec<DecisionTraceStep>,
    /// If the decision was `NeedExternal` and the user pressed
    /// "Always", this is the rule we'd persist. Pre-computed so
    /// the UI can show "Will save: Bash(npm test:*)" before the
    /// user actually clicks.
    pub will_save: Option<String>,
    /// Multi-tag risk classification.
    pub risk_tags: Vec<RiskTag>,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **Pinning test** — Plan v3 §P2 requires a fixed evaluation
    /// order to make rule precedence auditable. Any reorder must
    /// be deliberate and update this test.
    #[test]
    fn evaluation_order_is_stable() {
        assert_eq!(
            EVALUATION_ORDER,
            [
                EvaluationStep::SchemaIdentity,
                EvaluationStep::DenyRules,
                EvaluationStep::SafetyMiddleware,
                EvaluationStep::GitSafety,
                EvaluationStep::SensitivePath,
                EvaluationStep::ExecuteHardDeny,
                EvaluationStep::SandboxExpand,
                EvaluationStep::ReadShortCircuit,
                EvaluationStep::SessionOverride,
                EvaluationStep::ExplicitApproval,
                EvaluationStep::AllowRules,
                EvaluationStep::Mode,
            ],
            "evaluation order must not drift; if you intend to change it, update plan v3 §P2 first"
        );
    }

    /// Hard-deny rules MUST sit before sandbox expansion. That's the
    /// concrete bug from R1 Major 3 (current code:
    /// `permission_manager.rs:1601` sandbox_expand fires before
    /// `:1649` deny rules). Once the engine's evaluate_permission
    /// implementation lives here, this test will run against it
    /// directly.
    #[test]
    fn deny_rules_precede_sandbox_expand() {
        let deny_idx = EVALUATION_ORDER
            .iter()
            .position(|s| *s == EvaluationStep::DenyRules)
            .expect("DenyRules must be in EVALUATION_ORDER");
        let sandbox_idx = EVALUATION_ORDER
            .iter()
            .position(|s| *s == EvaluationStep::SandboxExpand)
            .expect("SandboxExpand must be in EVALUATION_ORDER");
        assert!(
            deny_idx < sandbox_idx,
            "DenyRules ({deny_idx}) must run before SandboxExpand ({sandbox_idx})"
        );
    }

    /// Catastrophic-command checks (the `rm -rf /` circuit breaker)
    /// run inside `SafetyMiddleware`, which must precede everything
    /// the user could relax — so even YOLO can't bypass it.
    #[test]
    fn safety_middleware_precedes_user_relaxable_steps() {
        let safety_idx = EVALUATION_ORDER
            .iter()
            .position(|s| *s == EvaluationStep::SafetyMiddleware)
            .unwrap();
        for relaxable in [
            EvaluationStep::AllowRules,
            EvaluationStep::SessionOverride,
            EvaluationStep::Mode,
        ] {
            let i = EVALUATION_ORDER
                .iter()
                .position(|s| *s == relaxable)
                .unwrap();
            assert!(
                safety_idx < i,
                "SafetyMiddleware must precede {relaxable:?}"
            );
        }
    }

    #[test]
    fn risk_tag_variants_are_distinct() {
        // Tiny test that catches accidental enum-merge regressions
        // when somebody adds a new tag.
        let tags = [
            RiskTag::BashExecute,
            RiskTag::WritesOutsidePackage,
            RiskTag::WritesOutsideWorkspace,
            RiskTag::WritesSensitiveFile,
            RiskTag::NetworkExfiltration,
            RiskTag::CredentialAccess,
            RiskTag::GitDestructive,
            RiskTag::SqlDestructive,
            RiskTag::MCPUnknownCapability,
            RiskTag::WorkspaceUntrusted,
            RiskTag::SandboxExpansion,
        ];
        let unique: std::collections::HashSet<_> = tags.iter().collect();
        assert_eq!(unique.len(), tags.len(), "RiskTag variants must be distinct");
    }
}
