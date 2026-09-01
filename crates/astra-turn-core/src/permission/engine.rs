//! Issue #326 P2 / R1 Critical 2 / R2 Critical 2:
//! shared permission evaluation engine.
//!
//! ## The Critical 2 problem (one-line version)
//!
//! There used to be **two permission deciders**:
//!
//! - Flow A in `astra-cli::permission_manager::check_nonblocking_inner`,
//!   running the full hard-deny chain (deny → safety → git →
//!   execute hard-deny → sensitive paths → sandbox expand → ask rules →
//!   read short-circuit → session override → explicit approval →
//!   allow rules → mode → fallback).
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
//! - [`EvaluationStep`] — the fixed ordering slots, written as an
//!   enum so a pinning test can assert the order is stable.
//! - [`evaluation_order`] — the constant array that documents the
//!   order in code (`[Schema, Deny, Safety, Git, Execute, Sensitive,
//!   SandboxExpand, ToolAllowlist, AskRules, ReadShortCircuit,
//!   SessionOverride, ExplicitApproval, AllowRules, Mode]`).
//!
//! The runtime/sub-agent gate now calls [`evaluate_permission`] so Flow B no
//! longer owns a second simplified decision chain. The CLI gate still has
//! some UI-specific state (trusted sandbox roots, denial throttling, project
//! persistence), so it is being migrated in stages; new pure checks should be
//! added here first and consumed by call sites instead of being copied.

use astra_sandbox::{GitSafetyViolation, is_soft_violation, validate_git_command};
use astra_tools::agent_tool_contract::{
    AgentAction, AgentFanoutAction, agent_action_from_args, agent_fanout_action_from_args,
};
use serde_json::Value;
use std::collections::BTreeSet;

use crate::action_compensation::{explicit_approval_reason, primary_approval_reason};
use crate::approval_fingerprint::{ApprovalFingerprint, FingerprintedOverrides};
use crate::cloud::approval_policy::{
    CloudGatedToolKind, cloud_gated_tool_kind, cloud_gated_tool_kind_with_args,
};
use crate::parallel_tool_exec::is_read_only_tool_with_args;
use crate::permission::match_target::{
    AllowMatchTarget, allow_rule_for_match_target, default_match_target,
};
use crate::permission::memory_profile::resolved_write_path;
use crate::permission::path_sensitivity::sensitive_path_token_for_tool_args;
use crate::permission::types::{ManualApprovalPolicy, PermissionMode, PermissionSyncContext};
use crate::safety_middleware::{SafetyMiddlewareDecision, evaluate_tool_safety_request};
use crate::tool::args::hints::{
    command_hint_from_args, path_hint_from_args, permission_prompt_primary_detail,
};

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
    DenyRule {
        rule: String,
        origin: RuleOrigin,
    },
    SafetyMiddleware {
        reason: String,
    },
    GitSafety {
        violation: String,
    },
    SensitivePath {
        path: String,
    },
    ExecuteHardDeny {
        reason: String,
    },
    SandboxExpansion,
    AskRule {
        rule: String,
        origin: RuleOrigin,
    },
    ReadShortCircuit,
    InternalOrchestration,
    SessionOverride {
        allowed: bool,
    },
    ExplicitApprovalGate {
        reason: String,
    },
    AllowRule {
        rule: String,
        origin: RuleOrigin,
    },
    Mode {
        mode: String,
    },
    /// No rule matched and we fell off the end → the engine routes
    /// to the prompt sink. (Captured here so the trace explains
    /// "no rule fired" instead of being silent.)
    UnmatchedFallback,
}

/// Where a rule originally came from. UI uses this for the
/// "Saved to .astra/permissions.json" line in the approval card.
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

/// The fixed evaluation slots. Plan v3 §P2 freezes the order:
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
    ExecuteHardDeny,
    SensitivePath,
    SandboxExpand,
    ToolAllowlist,
    AskRules,
    ReadShortCircuit,
    SessionOverride,
    ExplicitApproval,
    AllowRules,
    Mode,
}

/// Canonical ordering. Any reorder must change this constant AND
/// flip the pinning test in `tests::evaluation_order_is_stable`.
pub const EVALUATION_ORDER: [EvaluationStep; 14] = [
    EvaluationStep::SchemaIdentity,
    EvaluationStep::DenyRules,
    EvaluationStep::SafetyMiddleware,
    EvaluationStep::GitSafety,
    EvaluationStep::ExecuteHardDeny,
    EvaluationStep::SensitivePath,
    EvaluationStep::SandboxExpand,
    EvaluationStep::ToolAllowlist,
    EvaluationStep::AskRules,
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
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
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
    /// A dynamic provider descriptor exists, but its effect claims are
    /// missing, contradictory, or not trusted enough to relax policy.
    ProviderUnknownCapability,
    /// A resolved dynamic provider descriptor is known to have side effects.
    ProviderSideEffect,
    /// Workspace not in the trust ledger; persistent rules from
    /// `.astra/permissions.json` are downgraded to allow-only-once.
    WorkspaceUntrusted,
    /// `sandbox_expand:*` request — the agent is asking us to widen
    /// the sandbox, not run a tool.
    SandboxExpansion,
}

/// Issue #326 P5 / R2 Major 5: MCP tool capability metadata.
///
/// Carries the standard MCP annotations
/// (`destructiveHint` / `readOnlyHint` / `openWorldHint`) plus the
/// origin server name from request to engine. Without this the
/// gate has no way to distinguish a benign `mcp_jira_list_issues`
/// (read-only) from a `mcp_jira_delete_project` (catastrophic),
/// even when the server declares the difference in its tool
/// schema.
///
/// All fields are `Option` because (a) annotations are themselves
/// optional in the MCP spec, and (b) we want a clear distinction
/// between "server said this is read-only" and "server didn't
/// say". The risk-tag emitter treats absent metadata as
/// [`RiskTag::MCPUnknownCapability`], which downstream UI uses to
/// disable the persistent-scope buttons.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ToolCapabilityMetadata {
    /// MCP `destructiveHint` — true if the server told us the
    /// tool may delete or otherwise irreversibly affect data.
    pub destructive_hint: Option<bool>,
    /// MCP `readOnlyHint` — true if the server told us the tool
    /// only reads, never writes.
    pub read_only_hint: Option<bool>,
    /// MCP `openWorldHint` — true if the tool may interact with
    /// systems outside the user's machine (network, third-party
    /// APIs).
    pub open_world_hint: Option<bool>,
    /// MCP server identifier (e.g. `"github"`, `"jira"`). Used
    /// for the `MCP(server="…")` rule grammar slot in P1.5b.
    pub server_name: Option<String>,
}

impl ToolCapabilityMetadata {
    /// Convenience: derive a [`RiskTag`] from the metadata.
    /// Returns `None` when the metadata explicitly tells us
    /// "this is read-only" — we don't tag those.
    #[must_use]
    pub fn risk_tag(&self) -> Option<RiskTag> {
        // Explicit read-only → no tag.
        if matches!(self.read_only_hint, Some(true)) && !matches!(self.destructive_hint, Some(true))
        {
            return None;
        }
        // Explicit destructive → known capability, but the UI
        // still wants the MCP-specific tag so the prompt header
        // shows "MCP destructive: ..." rather than just "RUN".
        if matches!(self.destructive_hint, Some(true)) {
            return Some(RiskTag::MCPUnknownCapability); // strong tag
        }
        // No annotation at all → unknown.
        if self.destructive_hint.is_none()
            && self.read_only_hint.is_none()
            && self.open_world_hint.is_none()
        {
            return Some(RiskTag::MCPUnknownCapability);
        }
        // Everything else (e.g. only open_world_hint set) → still
        // route to MCPUnknownCapability for consistency.
        Some(RiskTag::MCPUnknownCapability)
    }

    /// True iff the metadata is sufficient to make a confident
    /// decision (i.e. at least one hint is present). Used by the
    /// P3 UI to decide whether to show "MCPUnknownCapability"
    /// banner + disable persistent-scope buttons.
    #[must_use]
    pub fn is_known(&self) -> bool {
        self.destructive_hint.is_some()
            || self.read_only_hint.is_some()
            || self.open_world_hint.is_some()
    }
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
    /// "Always", this is the internal rule we'd persist. User-facing
    /// UI must translate this to product language instead of showing
    /// permission-rule syntax directly.
    pub will_save: Option<String>,
    /// Multi-tag risk classification.
    pub risk_tags: Vec<RiskTag>,
}

pub(crate) fn plan_mode_denial_reason(tool_name: &str) -> String {
    format!(
        "Tool '{tool_name}' denied by permission mode. Plan mode keeps the normal tool surface for exploration but allows only read-only invocations plus `enter_plan_mode` / `exit_plan_mode`; author the plan, then call `exit_plan_mode(plan=...)`."
    )
}

/// Evaluate a tool call against the synchronized parent/child permission
/// context.
///
/// This is the shared pure evaluator for runtime/sub-agent permission checks:
/// it runs absolute safety and policy guards before any relaxable rule or
/// mode branch, and returns `NeedExternal` instead of deciding how to ask the
/// user. Callers are responsible for routing that prompt to a TUI sink,
/// parent mailbox, or headless fail-closed path.
#[must_use]
pub fn evaluate_permission(
    tool_name: &str,
    args: &Value,
    ctx: &PermissionSyncContext,
) -> DecisionEnvelope {
    evaluate_permission_with_provider_policy(tool_name, args, ctx, None)
}

/// Evaluate permission using the exact provider policy that was selected for
/// this invocation. Static tools pass `None` and retain their args-aware
/// registry behavior; dynamic tools must pass the descriptor-keyed policy used
/// by batching and execution.
#[must_use]
pub fn evaluate_permission_with_provider_policy(
    tool_name: &str,
    args: &Value,
    ctx: &PermissionSyncContext,
    provider_policy: Option<&crate::provider_resolution::ResolvedInvocationPolicy>,
) -> DecisionEnvelope {
    let mut trace = Vec::with_capacity(EVALUATION_ORDER.len());
    let mut risk_tags = risk_tags_for_request(tool_name, args);
    if let Some(policy) = provider_policy {
        // The descriptor is the capability authority for dynamic tools. Drop
        // tags inferred only from the public alias or the static-tool
        // registry; retaining those would let names such as `mcp__*`
        // contradict the exact descriptor used by batching and execution.
        // Intrinsic guards (for example explicit deny rules and git safety)
        // remain in the normal evaluation chain below.
        risk_tags.retain(|tag| {
            !matches!(
                tag,
                RiskTag::MCPUnknownCapability
                    | RiskTag::BashExecute
                    | RiskTag::WritesSensitiveFile
                    | RiskTag::WritesOutsidePackage
            )
        });
        let provider_tag = if policy.is_read_only() {
            None
        } else if policy.effect == astra_turn_types::ResolvedToolEffect::Mutating {
            Some(RiskTag::ProviderSideEffect)
        } else {
            Some(RiskTag::ProviderUnknownCapability)
        };
        if let Some(tag) = provider_tag {
            push_risk_tag(&mut risk_tags, tag);
        }
    }
    let will_save = Some(allow_rule_preview(tool_name, args));
    let rule_match_context =
        crate::permission::types::RuleMatchContext::from_tool_args(tool_name, args);

    push_skipped(
        &mut trace,
        EvaluationStep::SchemaIdentity,
        "schema accepted",
    );

    if ctx.is_denied_with_context(tool_name, &rule_match_context) {
        let decision = HardDecision::Deny {
            reason: format!("Tool '{tool_name}' denied by permission rules"),
        };
        push_matched(
            &mut trace,
            EvaluationStep::DenyRules,
            &decision,
            "deny rule matched",
        );
        return envelope(
            decision,
            DecisionSource::DenyRule {
                rule: tool_name.to_string(),
                origin: RuleOrigin::Inherited,
            },
            trace,
            will_save,
            risk_tags,
        );
    }
    push_skipped(
        &mut trace,
        EvaluationStep::DenyRules,
        "no deny rule matched",
    );

    match evaluate_tool_safety_request(tool_name, args) {
        SafetyMiddlewareDecision::Allow => {
            push_skipped(
                &mut trace,
                EvaluationStep::SafetyMiddleware,
                "safety guards passed",
            );
        }
        SafetyMiddlewareDecision::Deny(reason) => {
            // In auto mode, shell obfuscation rules (not catastrophic / destructive SQL)
            // are relaxed: the user has delegated trust to the agent.
            // Catastrophic commands (rm -rf /, fork bombs) and destructive SQL
            // (DROP TABLE, etc.) remain hard-denied regardless of mode.
            if ctx.mode().relaxes_soft_shell_obfuscation()
                && reason.contains("shell_obfuscation")
                && !reason.contains("catastrophic")
            {
                push_matched(
                    &mut trace,
                    EvaluationStep::SafetyMiddleware,
                    &HardDecision::Allow,
                    &format!("auto mode relaxed: {reason}"),
                );
                // continue to next step — do NOT return early
            } else {
                let decision = HardDecision::Deny {
                    reason: reason.clone(),
                };
                push_matched(
                    &mut trace,
                    EvaluationStep::SafetyMiddleware,
                    &decision,
                    &reason,
                );
                return envelope(
                    decision,
                    DecisionSource::SafetyMiddleware { reason },
                    trace,
                    will_save,
                    risk_tags,
                );
            }
        }
    }

    let git_violations = git_safety_violations_for_request(tool_name, args);
    let git_safety_skip_note = "no git violation";
    if !git_violations.is_empty() {
        let reasons: Vec<String> = git_violations.iter().map(ToString::to_string).collect();
        let has_hard_violation = git_violations.iter().any(|v| !is_soft_violation(v));
        match resolve_git_safety_guard(
            tool_name,
            args,
            ctx,
            &reasons,
            has_hard_violation,
            &risk_tags,
        ) {
            GuardResolution::Continue(notes) => {
                for note in notes {
                    push_skipped(&mut trace, EvaluationStep::GitSafety, &note);
                }
            }
            GuardResolution::Return {
                decision,
                source,
                detail,
                skipped_notes,
            } => {
                for note in skipped_notes {
                    push_skipped(&mut trace, EvaluationStep::GitSafety, &note);
                }
                push_matched(&mut trace, EvaluationStep::GitSafety, &decision, &detail);
                return envelope(decision, source, trace, will_save, risk_tags);
            }
        }
    }
    push_skipped(&mut trace, EvaluationStep::GitSafety, git_safety_skip_note);

    if let Some(reason) = execute_hard_deny_reason(tool_name, args) {
        if ctx.mode() == PermissionMode::Deny {
            let decision = HardDecision::Deny {
                reason: "Command hard-denied (deny mode)".to_string(),
            };
            push_matched(
                &mut trace,
                EvaluationStep::ExecuteHardDeny,
                &decision,
                &reason,
            );
            return envelope(
                decision,
                DecisionSource::ExecuteHardDeny { reason },
                trace,
                will_save,
                risk_tags,
            );
        }
        let decision = HardDecision::Deny {
            reason: reason.clone(),
        };
        push_matched(
            &mut trace,
            EvaluationStep::ExecuteHardDeny,
            &decision,
            &reason,
        );
        return envelope(
            decision,
            DecisionSource::ExecuteHardDeny { reason },
            trace,
            will_save,
            risk_tags,
        );
    }
    push_skipped(
        &mut trace,
        EvaluationStep::ExecuteHardDeny,
        "no execute hard deny",
    );

    if let Some(path) = sensitive_path_match(tool_name, args) {
        match resolve_sensitive_path_guard(tool_name, args, ctx, &path, &risk_tags) {
            GuardResolution::Continue(notes) => {
                for note in notes {
                    push_skipped(&mut trace, EvaluationStep::SensitivePath, &note);
                }
            }
            GuardResolution::Return {
                decision,
                source,
                detail,
                skipped_notes,
            } => {
                for note in skipped_notes {
                    push_skipped(&mut trace, EvaluationStep::SensitivePath, &note);
                }
                push_matched(
                    &mut trace,
                    EvaluationStep::SensitivePath,
                    &decision,
                    &detail,
                );
                return envelope(decision, source, trace, will_save, risk_tags);
            }
        }
    }
    push_skipped(
        &mut trace,
        EvaluationStep::SensitivePath,
        "no sensitive path",
    );

    if let Some(inner_tool) = tool_name.strip_prefix("sandbox_expand:") {
        let mode = ctx.mode();
        if mode.auto_resolves_approval_prompts() {
            let decision = HardDecision::Allow;
            push_matched(
                &mut trace,
                EvaluationStep::SandboxExpand,
                &decision,
                "sandbox expansion allowed by mode",
            );
            return envelope(
                decision,
                DecisionSource::SandboxExpansion,
                trace,
                will_save,
                risk_tags,
            );
        }
        let manual_policy = mode
            .manual_approval_policy()
            .expect("auto-resolving permission modes returned before sandbox match");
        return match manual_policy {
            ManualApprovalPolicy::Plan => {
                let decision = HardDecision::Deny {
                    reason: "Sandbox expansion denied (plan mode)".to_string(),
                };
                push_matched(
                    &mut trace,
                    EvaluationStep::SandboxExpand,
                    &decision,
                    "sandbox expansion denied by mode",
                );
                envelope(
                    decision,
                    DecisionSource::SandboxExpansion,
                    trace,
                    will_save,
                    risk_tags,
                )
            }
            ManualApprovalPolicy::AcceptEdits | ManualApprovalPolicy::Prompt => {
                let reason = args
                    .get("reason")
                    .and_then(Value::as_str)
                    .unwrap_or("Access to path outside project boundary")
                    .to_string();
                let action = sandbox_expand_action_label(inner_tool);
                let directory = args
                    .get("directory")
                    .and_then(Value::as_str)
                    .filter(|directory| !directory.trim().is_empty());
                let decision = HardDecision::NeedExternal {
                    prompt: ApprovalPrompt {
                        tool: tool_name.to_string(),
                        header: directory
                            .map(|directory| {
                                format!("{inner_tool} wants to {action} in {directory}")
                            })
                            .unwrap_or_else(|| {
                                format!("{inner_tool} wants to {action} outside the project")
                            }),
                        detail: directory.map(|directory| {
                            format!("Approve temporary sandbox access to {directory}")
                        }),
                        reason,
                        risk_tags: risk_tags.clone(),
                    },
                };
                push_matched(
                    &mut trace,
                    EvaluationStep::SandboxExpand,
                    &decision,
                    "sandbox expansion requires approval",
                );
                envelope(
                    decision,
                    DecisionSource::SandboxExpansion,
                    trace,
                    will_save,
                    risk_tags,
                )
            }
            ManualApprovalPolicy::Deny => {
                let decision = HardDecision::Deny {
                    reason: "Sandbox expansion denied (deny mode)".to_string(),
                };
                push_matched(
                    &mut trace,
                    EvaluationStep::SandboxExpand,
                    &decision,
                    "sandbox expansion denied by mode",
                );
                envelope(
                    decision,
                    DecisionSource::SandboxExpansion,
                    trace,
                    will_save,
                    risk_tags,
                )
            }
        };
    }
    push_skipped(
        &mut trace,
        EvaluationStep::SandboxExpand,
        "not a sandbox expansion",
    );

    if ctx.inherited.allowed_tools.is_some() {
        if !ctx.inherited.is_tool_allowed_by_allowlist(tool_name) {
            let decision = HardDecision::Deny {
                reason: format!("Tool '{tool_name}' not in allowed tools list"),
            };
            push_matched(
                &mut trace,
                EvaluationStep::ToolAllowlist,
                &decision,
                "tool not in allowed_tools",
            );
            return envelope(
                decision,
                DecisionSource::Mode {
                    mode: "agent policy allowlist".to_string(),
                },
                trace,
                will_save,
                risk_tags,
            );
        }
        push_skipped(
            &mut trace,
            EvaluationStep::ToolAllowlist,
            "tool in allowed_tools; continuing to permission mode",
        );
    } else {
        push_skipped(
            &mut trace,
            EvaluationStep::ToolAllowlist,
            "no tool allowlist",
        );
    }

    if let Some(rule) = ctx
        .inherited
        .ask_rule_with_context(tool_name, &rule_match_context)
    {
        if ctx.mode().skips_human_approval_prompts() {
            push_skipped(
                &mut trace,
                EvaluationStep::AskRules,
                "ask rule skipped by bypass mode",
            );
        } else {
            let reason = format!("Tool '{tool_name}' requires parent approval");
            let decision = HardDecision::NeedExternal {
                prompt: approval_prompt(tool_name, args, reason.clone(), risk_tags.clone()),
            };
            push_matched(&mut trace, EvaluationStep::AskRules, &decision, &reason);
            return envelope(
                decision,
                DecisionSource::AskRule {
                    rule: rule.to_string(),
                    origin: RuleOrigin::Inherited,
                },
                trace,
                will_save,
                risk_tags,
            );
        }
    } else {
        push_skipped(&mut trace, EvaluationStep::AskRules, "no ask rule matched");
    }

    let provider_requires_approval =
        provider_policy.is_some_and(|policy| policy.requires_approval());
    let static_explicit_reason = if provider_policy.is_none() {
        explicit_approval_reason(tool_name, args)
    } else {
        None
    };
    let resolved_read_only = provider_policy.map_or_else(
        || is_read_only_tool_with_args(tool_name, Some(args)),
        |policy| policy.is_read_only(),
    );
    if static_explicit_reason.is_none()
        && !provider_requires_approval
        && resolved_read_only
        && ctx.mode() != PermissionMode::Deny
    {
        let decision = HardDecision::Allow;
        push_matched(
            &mut trace,
            EvaluationStep::ReadShortCircuit,
            &decision,
            "read-only tool",
        );
        return envelope(
            decision,
            DecisionSource::ReadShortCircuit,
            trace,
            will_save,
            risk_tags,
        );
    }
    push_skipped(
        &mut trace,
        EvaluationStep::ReadShortCircuit,
        "not read-only",
    );

    if ctx.mode() == PermissionMode::Plan {
        push_skipped(
            &mut trace,
            EvaluationStep::SessionOverride,
            "plan mode ignores mutating session overrides",
        );
        push_skipped(
            &mut trace,
            EvaluationStep::ExplicitApproval,
            "plan mode never escalates mutating requests",
        );
        push_skipped(
            &mut trace,
            EvaluationStep::AllowRules,
            "plan mode ignores mutating allow rules",
        );
        let mode_label = ctx.mode().to_string();
        // Use the same args-aware plan-mode policy as execution preflight.
        // This keeps admission and execution aligned: read-only invocations,
        // plan-control tools, and plan-internal authoring pass; external
        // implementation side effects are denied.
        if !crate::plan_mode_policy::is_plan_mode_blocked_tool(tool_name, args) {
            let decision = HardDecision::Allow;
            push_matched(
                &mut trace,
                EvaluationStep::Mode,
                &decision,
                "allowed by plan-mode policy",
            );
            return envelope(
                decision,
                DecisionSource::Mode { mode: mode_label },
                trace,
                will_save,
                risk_tags,
            );
        }
        let decision = HardDecision::Deny {
            reason: plan_mode_denial_reason(tool_name),
        };
        push_matched(&mut trace, EvaluationStep::Mode, &decision, &mode_label);
        return envelope(
            decision,
            DecisionSource::Mode { mode: mode_label },
            trace,
            will_save,
            risk_tags,
        );
    }

    if let Some(allowed) = fingerprinted_override(tool_name, args, ctx) {
        let decision = if allowed {
            HardDecision::Allow
        } else {
            HardDecision::Deny {
                reason: "Skipped for session".to_string(),
            }
        };
        push_matched(
            &mut trace,
            EvaluationStep::SessionOverride,
            &decision,
            "fingerprinted session override matched",
        );
        return envelope(
            decision,
            DecisionSource::SessionOverride { allowed },
            trace,
            will_save,
            risk_tags,
        );
    }
    push_skipped(
        &mut trace,
        EvaluationStep::SessionOverride,
        "no fingerprinted override",
    );

    let explicit_reason = static_explicit_reason.or_else(|| {
        provider_requires_approval
            .then(|| "resolved provider capability requires approval".to_string())
    });
    if let Some(policy_reason) = explicit_reason {
        let prompt_reason =
            primary_approval_reason(tool_name, args).unwrap_or_else(|| policy_reason.clone());
        let mode = ctx.mode();
        if mode.auto_resolves_approval_prompts() {
            // ToolAllowlist step already denied unlisted tools, so anything
            // reaching ExplicitApproval in an auto-resolving mode is allowed.
            let decision = HardDecision::Allow;
            push_matched(
                &mut trace,
                EvaluationStep::ExplicitApproval,
                &decision,
                "explicit approval auto-allowed by mode",
            );
            return envelope(
                decision,
                DecisionSource::ExplicitApprovalGate {
                    reason: policy_reason,
                },
                trace,
                will_save,
                risk_tags,
            );
        }
        let manual_policy = mode
            .manual_approval_policy()
            .expect("auto-resolving permission modes returned before approval match");
        return match manual_policy {
            ManualApprovalPolicy::Plan => {
                let decision = HardDecision::Deny {
                    reason: "Explicit approval required (plan mode)".to_string(),
                };
                push_matched(
                    &mut trace,
                    EvaluationStep::ExplicitApproval,
                    &decision,
                    &policy_reason,
                );
                envelope(
                    decision,
                    DecisionSource::ExplicitApprovalGate {
                        reason: policy_reason,
                    },
                    trace,
                    will_save,
                    risk_tags,
                )
            }
            ManualApprovalPolicy::Deny => {
                let decision = HardDecision::Deny {
                    reason: "Explicit approval required (deny mode)".to_string(),
                };
                push_matched(
                    &mut trace,
                    EvaluationStep::ExplicitApproval,
                    &decision,
                    &policy_reason,
                );
                envelope(
                    decision,
                    DecisionSource::ExplicitApprovalGate {
                        reason: policy_reason,
                    },
                    trace,
                    will_save,
                    risk_tags,
                )
            }
            ManualApprovalPolicy::AcceptEdits => {
                // ToolAllowlist step already denied unlisted tools.
                let decision = HardDecision::NeedExternal {
                    prompt: approval_prompt(tool_name, args, prompt_reason, risk_tags.clone()),
                };
                push_matched(
                    &mut trace,
                    EvaluationStep::ExplicitApproval,
                    &decision,
                    &policy_reason,
                );
                envelope(
                    decision,
                    DecisionSource::ExplicitApprovalGate {
                        reason: policy_reason,
                    },
                    trace,
                    will_save,
                    risk_tags,
                )
            }
            ManualApprovalPolicy::Prompt => {
                let decision = HardDecision::NeedExternal {
                    prompt: approval_prompt(tool_name, args, prompt_reason, risk_tags.clone()),
                };
                push_matched(
                    &mut trace,
                    EvaluationStep::ExplicitApproval,
                    &decision,
                    &policy_reason,
                );
                envelope(
                    decision,
                    DecisionSource::ExplicitApprovalGate {
                        reason: policy_reason,
                    },
                    trace,
                    will_save,
                    risk_tags,
                )
            }
        };
    }
    push_skipped(
        &mut trace,
        EvaluationStep::ExplicitApproval,
        "no explicit approval requirement",
    );

    if ctx.is_allowed_with_context(tool_name, &rule_match_context) {
        let decision = HardDecision::Allow;
        push_matched(
            &mut trace,
            EvaluationStep::AllowRules,
            &decision,
            "allow rule matched",
        );
        return envelope(
            decision,
            DecisionSource::AllowRule {
                rule: tool_name.to_string(),
                origin: RuleOrigin::Inherited,
            },
            trace,
            will_save,
            risk_tags,
        );
    }
    push_skipped(
        &mut trace,
        EvaluationStep::AllowRules,
        "no allow rule matched",
    );

    if ctx.mode() != PermissionMode::Deny && is_internal_orchestration_control(tool_name, args) {
        let decision = HardDecision::Allow;
        push_matched(
            &mut trace,
            EvaluationStep::Mode,
            &decision,
            "internal orchestration control",
        );
        return envelope(
            decision,
            DecisionSource::InternalOrchestration,
            trace,
            will_save,
            risk_tags,
        );
    }

    let mode = ctx.mode();
    let (decision, mode_label) = if mode.auto_resolves_approval_prompts() {
        (HardDecision::Allow, mode.to_string())
    } else {
        let manual_policy = mode
            .manual_approval_policy()
            .expect("auto-resolving permission modes returned before mode match");
        match manual_policy {
            ManualApprovalPolicy::Plan => (
                HardDecision::Deny {
                    reason: format!("Tool '{tool_name}' denied by permission mode"),
                },
                mode.to_string(),
            ),
            ManualApprovalPolicy::AcceptEdits => {
                if accept_edits_auto_allows(tool_name, args) {
                    (HardDecision::Allow, mode.to_string())
                } else {
                    (
                        HardDecision::NeedExternal {
                            prompt: approval_prompt(
                                tool_name,
                                args,
                                "Write/execute tool requires approval".to_string(),
                                risk_tags.clone(),
                            ),
                        },
                        mode.to_string(),
                    )
                }
            }
            ManualApprovalPolicy::Deny => (
                HardDecision::Deny {
                    reason: format!("Tool '{tool_name}' denied by permission mode"),
                },
                mode.to_string(),
            ),
            ManualApprovalPolicy::Prompt => (
                HardDecision::NeedExternal {
                    prompt: approval_prompt(
                        tool_name,
                        args,
                        "Write/execute tool requires approval".to_string(),
                        risk_tags.clone(),
                    ),
                },
                mode.to_string(),
            ),
        }
    };
    push_matched(&mut trace, EvaluationStep::Mode, &decision, &mode_label);
    envelope(
        decision,
        DecisionSource::Mode { mode: mode_label },
        trace,
        will_save,
        risk_tags,
    )
}

/// Preview the rule that an "Always allow" action would persist for this call.
#[must_use]
pub fn allow_rule_preview(tool_name: &str, args: &Value) -> String {
    allow_rule_for_match_target(tool_name, args, &default_match_target(tool_name, args))
}

#[must_use]
pub fn allow_rule_preview_for_match_target(
    tool_name: &str,
    args: &Value,
    target: &AllowMatchTarget,
) -> String {
    allow_rule_for_match_target(tool_name, args, target)
}

fn envelope(
    decision: HardDecision,
    source: DecisionSource,
    trace: Vec<DecisionTraceStep>,
    will_save: Option<String>,
    risk_tags: Vec<RiskTag>,
) -> DecisionEnvelope {
    let will_save = if matches!(&decision, HardDecision::NeedExternal { .. }) {
        will_save
    } else {
        None
    };
    DecisionEnvelope {
        decision,
        source,
        trace,
        will_save,
        risk_tags,
    }
}

fn push_skipped(trace: &mut Vec<DecisionTraceStep>, step: EvaluationStep, note: &str) {
    trace.push(DecisionTraceStep {
        step,
        outcome: TraceOutcome::Skipped,
        note: note.to_string(),
    });
}

fn push_matched(
    trace: &mut Vec<DecisionTraceStep>,
    step: EvaluationStep,
    decision: &HardDecision,
    note: &str,
) {
    trace.push(DecisionTraceStep {
        step,
        outcome: TraceOutcome::Matched(decision.clone()),
        note: note.to_string(),
    });
}

fn sandbox_expand_action_label(inner_tool: &str) -> &'static str {
    match cloud_gated_tool_kind(inner_tool) {
        Some(CloudGatedToolKind::Write) => "write",
        Some(CloudGatedToolKind::Execute) => "execute",
        None if matches!(
            inner_tool,
            "read_file"
                | "list_dir"
                | "grep"
                | "glob"
                | "symbols"
                | "find_definition"
                | "find_references"
                | "symbol_search"
                | "hover_info"
                | "extract_members"
        ) =>
        {
            "read"
        }
        None => "access",
    }
}

fn approval_prompt(
    tool_name: &str,
    args: &Value,
    reason: String,
    risk_tags: Vec<RiskTag>,
) -> ApprovalPrompt {
    let header = match cloud_gated_tool_kind_with_args(tool_name, Some(args)) {
        Some(CloudGatedToolKind::Execute) => format!("Execute {tool_name}"),
        Some(CloudGatedToolKind::Write) => format!("Write with {tool_name}"),
        None => format!("{tool_name} requires approval"),
    };
    ApprovalPrompt {
        tool: tool_name.to_string(),
        header,
        detail: permission_prompt_primary_detail(tool_name, args),
        reason,
        risk_tags,
    }
}

fn is_execute_tool(tool_name: &str, args: &Value) -> bool {
    matches!(
        cloud_gated_tool_kind_with_args(tool_name, Some(args)),
        Some(CloudGatedToolKind::Execute)
    )
}

fn git_safety_violations_for_request(tool_name: &str, args: &Value) -> Vec<GitSafetyViolation> {
    if is_execute_tool(tool_name, args) {
        return command_hint_from_args(args)
            .map(validate_git_command)
            .unwrap_or_default();
    }

    structured_git_command_hint(tool_name, args)
        .map(|command| validate_git_command(&command))
        .unwrap_or_default()
}

fn structured_git_command_hint(tool_name: &str, args: &Value) -> Option<String> {
    if tool_name != "git" {
        return None;
    }
    let action = args.get("action").and_then(Value::as_str)?;
    if action != "push"
        || !args
            .get("force_with_lease")
            .and_then(Value::as_bool)
            .unwrap_or(false)
    {
        return None;
    }

    let mut command = String::from("git push --force-with-lease");
    if let Some(remote) = args.get("remote").and_then(Value::as_str) {
        command.push(' ');
        command.push_str(remote);
    }
    if let Some(branch) = args.get("branch").and_then(Value::as_str) {
        command.push(' ');
        command.push_str(branch);
    }
    Some(command)
}

fn sensitive_path_match(tool_name: &str, args: &Value) -> Option<String> {
    sensitive_path_token_for_tool_args(tool_name, args)
}

enum GuardResolution {
    Continue(Vec<String>),
    Return {
        decision: HardDecision,
        source: DecisionSource,
        detail: String,
        skipped_notes: Vec<String>,
    },
}

fn resolve_git_safety_guard(
    tool_name: &str,
    args: &Value,
    ctx: &PermissionSyncContext,
    reasons: &[String],
    has_hard_violation: bool,
    risk_tags: &[RiskTag],
) -> GuardResolution {
    let joined_reasons = reasons.join(", ");
    let mut skipped_notes = Vec::new();
    if ctx.mode() == PermissionMode::Deny {
        return GuardResolution::Return {
            decision: HardDecision::Deny {
                reason: "Git safety violation (deny mode)".to_string(),
            },
            source: DecisionSource::GitSafety {
                violation: joined_reasons.clone(),
            },
            detail: joined_reasons,
            skipped_notes,
        };
    }

    if let Some((stored_override, allowed)) = fingerprinted_override_match(tool_name, args, ctx) {
        if allowed && !stored_override_allows_git_safety(&stored_override) {
            skipped_notes.push("broad session override cannot bypass git safety".to_string());
        } else if ctx.inherited.allowed_tools.is_some() {
            skipped_notes.push(format!(
                "session override matched git violation, deferring to allowlist: {joined_reasons}",
            ));
            return GuardResolution::Continue(skipped_notes);
        } else {
            let decision = if allowed {
                HardDecision::Allow
            } else {
                HardDecision::Deny {
                    reason: "Skipped for session".to_string(),
                }
            };
            return GuardResolution::Return {
                decision,
                source: DecisionSource::SessionOverride { allowed },
                detail: "fingerprinted session override matched git safety request".to_string(),
                skipped_notes,
            };
        }
    }

    if ctx.mode().skips_human_approval_prompts() {
        skipped_notes.push(format!(
            "git risk retained as advisory in bypass mode: {joined_reasons}",
        ));
        return GuardResolution::Continue(skipped_notes);
    }

    if ctx.mode().auto_allows_soft_git_policy() && !has_hard_violation {
        if ctx.inherited.allowed_tools.is_some() {
            skipped_notes.push(format!(
                "auto mode soft violation, deferring to allowlist: {joined_reasons}",
            ));
            return GuardResolution::Continue(skipped_notes);
        }
        skipped_notes.push(format!(
            "soft git risk retained as advisory in auto mode: {joined_reasons}",
        ));
        return GuardResolution::Continue(skipped_notes);
    }

    let reason = format!("Git safety: {joined_reasons}");
    GuardResolution::Return {
        decision: HardDecision::NeedExternal {
            prompt: approval_prompt(tool_name, args, reason.clone(), risk_tags.to_vec()),
        },
        source: DecisionSource::GitSafety {
            violation: reason.clone(),
        },
        detail: reason,
        skipped_notes,
    }
}

fn resolve_sensitive_path_guard(
    tool_name: &str,
    args: &Value,
    ctx: &PermissionSyncContext,
    path: &str,
    risk_tags: &[RiskTag],
) -> GuardResolution {
    let mut skipped_notes = Vec::new();
    if ctx.mode() == PermissionMode::Deny {
        return GuardResolution::Return {
            decision: HardDecision::Deny {
                reason: "Sensitive path (deny mode)".to_string(),
            },
            source: DecisionSource::SensitivePath {
                path: path.to_string(),
            },
            detail: path.to_string(),
            skipped_notes,
        };
    }

    if let Some((stored_override, allowed)) = fingerprinted_override_match(tool_name, args, ctx) {
        if allowed && !stored_override_allows_sensitive_path(&stored_override) {
            skipped_notes.push("broad session override cannot bypass sensitive path".to_string());
        } else if ctx.inherited.allowed_tools.is_some() {
            skipped_notes.push(
                "session override matched sensitive path, deferring to allowlist".to_string(),
            );
            return GuardResolution::Continue(skipped_notes);
        } else {
            let decision = if allowed {
                HardDecision::Allow
            } else {
                HardDecision::Deny {
                    reason: "Sensitive path denied for session".to_string(),
                }
            };
            return GuardResolution::Return {
                decision,
                source: DecisionSource::SessionOverride { allowed },
                detail: "fingerprinted session override matched sensitive path".to_string(),
                skipped_notes,
            };
        }
    }

    if ctx.mode().skips_human_approval_prompts() {
        if ctx.inherited.allowed_tools.is_some() {
            skipped_notes.push("bypass mode sensitive path, deferring to allowlist".to_string());
            return GuardResolution::Continue(skipped_notes);
        }
        return GuardResolution::Return {
            decision: HardDecision::Allow,
            source: DecisionSource::SensitivePath {
                path: path.to_string(),
            },
            detail: "sensitive path allowed by bypass mode".to_string(),
            skipped_notes,
        };
    }

    let reason = "Targets a sensitive file path and requires manual approval".to_string();
    GuardResolution::Return {
        decision: HardDecision::NeedExternal {
            prompt: approval_prompt(tool_name, args, reason.clone(), risk_tags.to_vec()),
        },
        source: DecisionSource::SensitivePath {
            path: path.to_string(),
        },
        detail: path.to_string(),
        skipped_notes,
    }
}

fn execute_hard_deny_reason(tool_name: &str, args: &Value) -> Option<String> {
    if !is_execute_tool(tool_name, args) {
        return None;
    }
    let command = command_hint_from_args(args)?;
    let lower = command.to_ascii_lowercase();
    if crate::safety_middleware::absolute_dangerous_command_reason(command).is_some() {
        return Some("Dangerous command refused".to_string());
    }
    if lower.contains("shred ") || lower.contains("wipefs") {
        return Some("Dangerous command refused: destructive disk/file wiping".to_string());
    }
    if ["fdisk", "parted "].iter().any(|p| lower.contains(p)) {
        return Some("Dangerous command refused: low-level disk mutation".to_string());
    }
    if contains_pipe_to(&lower, "sh")
        || contains_pipe_to(&lower, "bash")
        || contains_pipe_to(&lower, "/bin/sh")
        || contains_pipe_to(&lower, "/bin/bash")
    {
        return Some("Dangerous command refused: shell interpreter pipeline".to_string());
    }
    None
}

fn contains_pipe_to(command: &str, target: &str) -> bool {
    let bytes = command.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != b'|' || (i > 0 && bytes[i - 1] == b'\\') {
            i += 1;
            continue;
        }
        let mut j = i + 1;
        while j < bytes.len() && bytes[j].is_ascii_whitespace() {
            j += 1;
        }
        let rest = &command[j..];
        if rest == target
            || rest
                .strip_prefix(target)
                .is_some_and(|tail| tail.is_empty() || tail.starts_with(char::is_whitespace))
        {
            return true;
        }
        i += 1;
    }
    false
}

fn fingerprinted_override(
    tool_name: &str,
    args: &Value,
    ctx: &PermissionSyncContext,
) -> Option<bool> {
    fingerprinted_override_match(tool_name, args, ctx).map(|(_, allowed)| allowed)
}

fn fingerprinted_override_match(
    tool_name: &str,
    args: &Value,
    ctx: &PermissionSyncContext,
) -> Option<(ApprovalFingerprint, bool)> {
    let json = ctx.inherited.fingerprinted_overrides.as_ref()?;
    // Fail-closed: a corrupt overrides blob must NOT silently fall through
    // to broader (potentially permissive) rules. Distinguish "no overrides
    // present" (None → caller falls through normally) from "overrides present
    // but undecodable" (Some(false) → deny). The latter is a security signal,
    // not a recoverable state.
    let overrides = match serde_json::from_value::<FingerprintedOverrides>(json.clone()) {
        Ok(parsed) => parsed,
        Err(err) => {
            tracing::warn!(
                error = %err,
                "fingerprinted_overrides blob is present but undecodable; denying tool {} as fail-closed",
                tool_name,
            );
            return Some((ApprovalFingerprint::bare(tool_name), false));
        }
    };
    content_aware_fingerprint_candidates(tool_name, args)
        .into_iter()
        .find_map(|fp| {
            overrides
                .matching_rule(&fp)
                .map(|(stored, allowed)| (stored.clone(), *allowed))
        })
}

fn stored_override_allows_git_safety(stored: &ApprovalFingerprint) -> bool {
    stored.command_exact.is_some()
}

fn stored_override_allows_sensitive_path(stored: &ApprovalFingerprint) -> bool {
    if let Some(command) = stored
        .command_exact
        .as_deref()
        .or(stored.command_prefix.as_deref())
    {
        return sensitive_path_match("bash", &serde_json::json!({ "command": command })).is_some();
    }

    let Some(path) = stored.path_pattern.as_deref() else {
        return false;
    };

    sensitive_path_match(&stored.tool_name, &serde_json::json!({ "path": path })).is_some()
}

fn accept_edits_auto_allows(tool_name: &str, args: &Value) -> bool {
    matches!(
        (
            cloud_gated_tool_kind_with_args(tool_name, Some(args)),
            default_match_target(tool_name, args),
        ),
        (Some(CloudGatedToolKind::Write), AllowMatchTarget::Prefix(_))
    )
}

fn is_internal_orchestration_control(tool_name: &str, args: &Value) -> bool {
    match tool_name {
        "agent" => matches!(
            agent_action_from_args(args),
            Ok(AgentAction::Spawn | AgentAction::GetResult | AgentAction::SendMessage)
        ),
        "agent_fanout" => matches!(
            agent_fanout_action_from_args(args),
            Ok(AgentFanoutAction::Start | AgentFanoutAction::GetResults)
        ),
        // Establishes internal declared-work state around the exact current
        // run. It grants no external capability and cannot claim completion.
        "start_work" => true,
        "propose_work_plan" => is_safe_additive_work_plan_proposal(args),
        // This action only persists a bounded non-authoritative proposal.
        // Accepted Done-when criteria require a separate explicit decision,
        // so model-authored statement/command text is not a policy signal.
        "propose_work_criteria" => true,
        _ => false,
    }
}

const SAFE_WORK_PLAN_MAX_ADDITIONS: usize = 16;
const SAFE_WORK_PLAN_MAX_DEPENDENCIES: usize = 64;

/// The no-interruption Work admission class.
///
/// This deliberately examines only the typed graph shape. Model-authored
/// objective/result prose is not a policy signal. A dependency may point from
/// accepted work into a new node, but may not add a new prerequisite to an
/// already accepted node.
fn is_safe_additive_work_plan_proposal(args: &Value) -> bool {
    let Some(additions) = args.get("additions").and_then(Value::as_array) else {
        return false;
    };
    let Some(dependencies) = args.get("dependencies").and_then(Value::as_array) else {
        return false;
    };
    let Some(revisions) = args.get("revisions").and_then(Value::as_array) else {
        return false;
    };
    let Some(dependency_removals) = args.get("dependency_removals").and_then(Value::as_array)
    else {
        return false;
    };
    if additions.is_empty()
        || additions.len() > SAFE_WORK_PLAN_MAX_ADDITIONS
        || dependencies.len() > SAFE_WORK_PLAN_MAX_DEPENDENCIES
        || !revisions.is_empty()
        || !dependency_removals.is_empty()
    {
        return false;
    }
    let new_item_ids = additions
        .iter()
        .filter_map(|item| item.get("item_id").and_then(Value::as_str))
        .collect::<BTreeSet<_>>();
    if new_item_ids.len() != additions.len() {
        return false;
    }
    dependencies.iter().all(|edge| {
        edge.get("successor_item_id")
            .and_then(Value::as_str)
            .is_some_and(|successor| new_item_ids.contains(successor))
    })
}

fn content_aware_fingerprint(tool_name: &str, args: &Value) -> ApprovalFingerprint {
    match cloud_gated_tool_kind(tool_name) {
        Some(CloudGatedToolKind::Execute) => command_hint_from_args(args).map_or_else(
            || ApprovalFingerprint::bare(tool_name),
            |cmd| {
                ApprovalFingerprint::shell(
                    tool_name,
                    cmd,
                    is_read_only_tool_with_args(tool_name, Some(args)),
                )
            },
        ),
        Some(CloudGatedToolKind::Write) => {
            let path = path_hint_from_args(args);
            path.map_or_else(
                || ApprovalFingerprint::bare(tool_name),
                |path| {
                    ApprovalFingerprint::file_op_exact(
                        file_write_fingerprint_tool(tool_name),
                        Some(&path),
                    )
                },
            )
        }
        None => ApprovalFingerprint::bare(tool_name),
    }
}

fn file_write_fingerprint_tool(tool_name: &str) -> &str {
    if crate::tool::categories::registry().is_file_op(tool_name) {
        "file_write"
    } else {
        tool_name
    }
}

fn content_aware_fingerprint_candidates(tool_name: &str, args: &Value) -> Vec<ApprovalFingerprint> {
    let primary = content_aware_fingerprint(tool_name, args);
    let mut candidates = vec![primary.clone()];
    if matches!(
        cloud_gated_tool_kind(tool_name),
        Some(CloudGatedToolKind::Write)
    ) && let Some(path) = path_hint_from_args(args)
    {
        if crate::tool::categories::registry().is_file_op(tool_name) {
            push_unique_fingerprint(
                &mut candidates,
                ApprovalFingerprint::file_op_exact("file_write", Some(&path)),
            );
        }
        if let Some(resolved) = resolved_write_path(&path) {
            push_unique_fingerprint(
                &mut candidates,
                ApprovalFingerprint::file_op_exact(tool_name, Some(&resolved)),
            );
            if crate::tool::categories::registry().is_file_op(tool_name) {
                push_unique_fingerprint(
                    &mut candidates,
                    ApprovalFingerprint::file_op_exact("file_write", Some(&resolved)),
                );
            }
        }
    }
    candidates
}

fn push_unique_fingerprint(
    candidates: &mut Vec<ApprovalFingerprint>,
    candidate: ApprovalFingerprint,
) {
    if !candidates.iter().any(|existing| existing == &candidate) {
        candidates.push(candidate);
    }
}

#[must_use]
pub fn risk_tags_for_request(tool_name: &str, args: &Value) -> Vec<RiskTag> {
    let mut tags = Vec::new();
    let has_git_safety_violation = !git_safety_violations_for_request(tool_name, args).is_empty();
    if tool_name.starts_with("sandbox_expand:") {
        push_risk_tag(&mut tags, RiskTag::SandboxExpansion);
    }
    if tool_name.starts_with("mcp_") {
        push_risk_tag(&mut tags, RiskTag::MCPUnknownCapability);
    }
    match cloud_gated_tool_kind_with_args(tool_name, Some(args)) {
        Some(CloudGatedToolKind::Execute) => {
            push_risk_tag(&mut tags, RiskTag::BashExecute);
        }
        Some(CloudGatedToolKind::Write) if sensitive_path_match(tool_name, args).is_some() => {
            push_risk_tag(&mut tags, RiskTag::WritesSensitiveFile);
        }
        Some(CloudGatedToolKind::Write) => {
            push_risk_tag(&mut tags, RiskTag::WritesOutsidePackage);
        }
        None => {}
    }
    if has_git_safety_violation {
        push_risk_tag(&mut tags, RiskTag::GitDestructive);
    }
    if tool_name == "mo_query"
        && let SafetyMiddlewareDecision::Deny(reason) =
            evaluate_tool_safety_request(tool_name, args)
        && reason
            .to_ascii_lowercase()
            .contains("statements are blocked")
    {
        push_risk_tag(&mut tags, RiskTag::SqlDestructive);
    }
    tags
}

fn push_risk_tag(tags: &mut Vec<RiskTag>, tag: RiskTag) {
    if !tags.contains(&tag) {
        tags.push(tag);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn create_current_session_artifact() -> (
        tempfile::TempDir,
        astra_services::session_journal::JournalDirGuard,
        std::path::PathBuf,
    ) {
        let temp = tempfile::tempdir().unwrap();
        let sessions_root = temp.path().join("sessions");
        let guard = astra_services::session_journal::JournalDirGuard::new(&sessions_root);
        let artifact_path = sessions_root.join("session-1/tool-results/call_abc.txt");
        std::fs::create_dir_all(artifact_path.parent().unwrap()).unwrap();
        std::fs::write(&artifact_path, "child output").unwrap();
        (temp, guard, artifact_path)
    }

    fn create_current_session_journal() -> (
        tempfile::TempDir,
        astra_services::session_journal::JournalDirGuard,
        std::path::PathBuf,
    ) {
        let temp = tempfile::tempdir().unwrap();
        let sessions_root = temp.path().join("sessions");
        let guard = astra_services::session_journal::JournalDirGuard::new(&sessions_root);
        std::fs::create_dir_all(&sessions_root).unwrap();
        let journal_path = sessions_root.join("550e8400-e29b-41d4-a716-446655440000.jsonl");
        std::fs::write(&journal_path, "{}\n").unwrap();
        (temp, guard, journal_path)
    }

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
                EvaluationStep::ExecuteHardDeny,
                EvaluationStep::SensitivePath,
                EvaluationStep::SandboxExpand,
                EvaluationStep::ToolAllowlist,
                EvaluationStep::AskRules,
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

    /// Fail-closed pinning: a corrupt `fingerprinted_overrides` blob must
    /// resolve to an explicit deny (`Some(false)`), NOT fall through to
    /// broader rules via `None`. The old `.ok()?` path silently downgraded
    /// to whatever the caller did next — the exact bug class this module
    /// is supposed to prevent.
    #[test]
    fn fingerprinted_override_corrupt_json_denies() {
        let ctx = crate::permission::types::PermissionSyncContext::new(
            crate::permission::types::InheritedPermissions {
                mode: crate::permission::types::PermissionMode::AcceptEdits,
                fingerprinted_overrides: Some(serde_json::json!({"not": "a valid shape"})),
                ..Default::default()
            },
        );
        // Call the private fn directly — we're inside the module.
        let result =
            fingerprinted_override("bash", &serde_json::json!({"command": "echo hi"}), &ctx);
        assert_eq!(
            result,
            Some(false),
            "corrupt fingerprinted_overrides blob must fail-closed to Some(false), not fall through as None"
        );
    }

    /// Positive pinning: a well-formed empty overrides blob must still
    /// return `None` (no override matched) so the caller falls through
    /// normally. This guards against the fail-closed change accidentally
    /// treating "no overrides" the same as "corrupt overrides".
    #[test]
    fn fingerprinted_override_empty_blob_falls_through() {
        let ctx = crate::permission::types::PermissionSyncContext::new(
            crate::permission::types::InheritedPermissions {
                mode: crate::permission::types::PermissionMode::AcceptEdits,
                fingerprinted_overrides: None,
                ..Default::default()
            },
        );
        let result =
            fingerprinted_override("bash", &serde_json::json!({"command": "echo hi"}), &ctx);
        assert_eq!(
            result, None,
            "missing fingerprinted_overrides blob must fall through as None"
        );
    }

    #[test]
    fn sandbox_expand_prompt_headers_describe_inner_tool_action() {
        for mode in [
            crate::permission::types::PermissionMode::Prompt,
            crate::permission::types::PermissionMode::AcceptEdits,
        ] {
            let ctx = crate::permission::types::PermissionSyncContext::new(
                crate::permission::types::InheritedPermissions {
                    mode,
                    ..Default::default()
                },
            );

            for (tool, expected_header) in [
                (
                    "sandbox_expand:read_file",
                    "read_file wants to read outside the project",
                ),
                (
                    "sandbox_expand:write_file",
                    "write_file wants to write outside the project",
                ),
                (
                    "sandbox_expand:bash",
                    "bash wants to execute outside the project",
                ),
            ] {
                let envelope = evaluate_permission(
                    tool,
                    &serde_json::json!({
                        "reason": "Path '/tmp/outside' is outside the project directory '/tmp/project'; sandbox approval is required for this external path."
                    }),
                    &ctx,
                );

                match envelope.decision {
                    HardDecision::NeedExternal { prompt } => {
                        assert_eq!(prompt.tool, tool);
                        assert_eq!(prompt.header, expected_header);
                        assert!(prompt.detail.is_none());
                    }
                    other => {
                        panic!("{mode:?} sandbox expansion should prompt for {tool}; got {other:?}")
                    }
                }
                assert_eq!(envelope.source, DecisionSource::SandboxExpansion);
            }
        }
    }

    #[test]
    fn sandbox_expand_prompt_uses_the_requested_directory_when_available() {
        let ctx = crate::permission::types::PermissionSyncContext::new(
            crate::permission::types::InheritedPermissions {
                mode: crate::permission::types::PermissionMode::Prompt,
                ..Default::default()
            },
        );
        let envelope = evaluate_permission(
            "sandbox_expand:bash",
            &serde_json::json!({
                "reason": "The current tool sandbox needs a broader workspace boundary.",
                "directory": "/workspace/astra"
            }),
            &ctx,
        );

        match envelope.decision {
            HardDecision::NeedExternal { prompt } => {
                assert_eq!(prompt.header, "bash wants to execute in /workspace/astra");
                assert_eq!(
                    prompt.detail.as_deref(),
                    Some("Approve temporary sandbox access to /workspace/astra")
                );
            }
            other => panic!("sandbox scope should prompt; got {other:?}"),
        }
    }

    /// Catastrophic-command checks (the `rm -rf /` circuit breaker)
    /// run inside `SafetyMiddleware`, which must precede everything
    /// the user could relax.
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
            RiskTag::ProviderUnknownCapability,
            RiskTag::ProviderSideEffect,
            RiskTag::WorkspaceUntrusted,
            RiskTag::SandboxExpansion,
        ];
        let unique: std::collections::HashSet<_> = tags.iter().collect();
        assert_eq!(
            unique.len(),
            tags.len(),
            "RiskTag variants must be distinct"
        );
    }

    #[test]
    fn allow_rule_preview_uses_workspace_scope_for_safe_writes() {
        let rule = allow_rule_preview(
            "write_file",
            &serde_json::json!({"path": "zzzz3.md", "content": "# zzzz3"}),
        );

        let cwd = std::env::current_dir()
            .unwrap()
            .canonicalize()
            .unwrap_or_else(|_| std::env::current_dir().unwrap())
            .to_string_lossy()
            .into_owned();
        assert_eq!(
            rule,
            format!(r#"file_write(path_prefix="{cwd}", op="write", cwd_root="{cwd}")"#)
        );
    }

    #[test]
    fn allow_rule_preview_keeps_exact_scope_for_workspace_external_writes() {
        let rule = allow_rule_preview(
            "write_file",
            &serde_json::json!({"path": "/tmp/zzzz3.md", "content": "# zzzz3"}),
        );

        assert_eq!(rule, r#"file_write(path_glob="/tmp/zzzz3.md", op="write")"#);
    }

    #[test]
    fn auto_mode_allowlist_blocks_unlisted_tool() {
        let ctx = crate::permission::types::PermissionSyncContext::new(
            crate::permission::types::InheritedPermissions {
                mode: crate::permission::types::PermissionMode::Auto,
                allowed_tools: Some(std::collections::HashSet::from(["read_file".to_string()])),
                ..Default::default()
            },
        );
        let envelope = evaluate_permission("bash", &serde_json::json!({}), &ctx);
        assert!(
            matches!(envelope.decision, HardDecision::Deny { .. }),
            "unexpected decision from {:?}: {:?}",
            envelope.source,
            envelope.trace
        );
        assert!(
            matches!(
                envelope.source,
                DecisionSource::Mode { ref mode }
                    if mode == "agent policy allowlist"
            ) || matches!(envelope.source, DecisionSource::ExplicitApprovalGate { .. }),
            "unexpected source for auto allowlist denial: {:?}",
            envelope.source
        );
    }

    #[test]
    fn prompt_mode_allowlist_does_not_locally_allow_listed_write_tool() {
        let ctx = crate::permission::types::PermissionSyncContext::new(
            crate::permission::types::InheritedPermissions {
                mode: crate::permission::types::PermissionMode::Prompt,
                allowed_tools: Some(std::collections::HashSet::from(["write_file".to_string()])),
                ..Default::default()
            },
        );
        let envelope = evaluate_permission(
            "write_file",
            &serde_json::json!({"path": "src/lib.rs", "content": "pub fn x() {}\n"}),
            &ctx,
        );

        assert!(
            matches!(envelope.decision, HardDecision::NeedExternal { .. }),
            "allowed_tools restricts the child tool surface but must not approve writes in Prompt mode; got {:?}",
            envelope.decision
        );
        assert_eq!(
            envelope.source,
            DecisionSource::Mode {
                mode: "prompt".to_string()
            }
        );
    }

    #[test]
    fn prompt_mode_allowlist_blocks_unlisted_explicit_tool_before_prompting() {
        let ctx = crate::permission::types::PermissionSyncContext::new(
            crate::permission::types::InheritedPermissions {
                mode: crate::permission::types::PermissionMode::Prompt,
                allowed_tools: Some(std::collections::HashSet::from(["read_file".to_string()])),
                ..Default::default()
            },
        );
        let envelope =
            evaluate_permission("bash", &serde_json::json!({"command": "git status"}), &ctx);

        assert!(matches!(envelope.decision, HardDecision::Deny { .. }));
        assert_eq!(
            envelope.source,
            DecisionSource::Mode {
                mode: "agent policy allowlist".to_string()
            }
        );
    }

    #[test]
    fn prompt_mode_allowlist_still_allows_listed_read_only_tool() {
        let ctx = crate::permission::types::PermissionSyncContext::new(
            crate::permission::types::InheritedPermissions {
                mode: crate::permission::types::PermissionMode::Prompt,
                allowed_tools: Some(std::collections::HashSet::from(["read_file".to_string()])),
                ..Default::default()
            },
        );
        let envelope = evaluate_permission(
            "read_file",
            &serde_json::json!({"path": "src/lib.rs"}),
            &ctx,
        );

        assert!(matches!(envelope.decision, HardDecision::Allow));
        assert_eq!(envelope.source, DecisionSource::ReadShortCircuit);
    }

    #[test]
    fn prompt_mode_allows_agent_lifecycle_control_without_external_approval() {
        let ctx = crate::permission::types::PermissionSyncContext::new(
            crate::permission::types::InheritedPermissions {
                mode: crate::permission::types::PermissionMode::Prompt,
                ..Default::default()
            },
        );

        for (tool, args) in [
            (
                "agent",
                serde_json::json!({
                    "action": "spawn",
                    "description": "Review",
                    "prompt": "Check the diff"
                }),
            ),
            (
                "agent",
                serde_json::json!({
                    "action": "get_result",
                    "agent_id": "agent-1"
                }),
            ),
            (
                "agent_fanout",
                serde_json::json!({
                    "action": "start",
                    "group_id": "review",
                    "target_count": 1,
                    "slots": [{"id": "slot-1", "prompt": "Review"}]
                }),
            ),
            (
                "agent_fanout",
                serde_json::json!({
                    "action": "get_results",
                    "group_id": "review"
                }),
            ),
        ] {
            let envelope = evaluate_permission(tool, &args, &ctx);
            assert!(
                matches!(envelope.decision, HardDecision::Allow),
                "{tool} {args} should be internal orchestration control; got {:?} from {:?}",
                envelope.decision,
                envelope.source
            );
            assert_eq!(envelope.source, DecisionSource::InternalOrchestration);
        }
    }

    #[test]
    fn prompt_mode_still_prompts_agent_actions_that_can_mutate_runtime_state() {
        let ctx = crate::permission::types::PermissionSyncContext::new(
            crate::permission::types::InheritedPermissions {
                mode: crate::permission::types::PermissionMode::Prompt,
                ..Default::default()
            },
        );

        for (tool, args) in [
            (
                "agent",
                serde_json::json!({
                    "action": "run_chain",
                    "prompt": "Run the release pipeline"
                }),
            ),
            (
                "agent_fanout",
                serde_json::json!({
                    "action": "stop_slot",
                    "group_id": "review",
                    "slot_index": 0
                }),
            ),
            (
                "agent_fanout",
                serde_json::json!({
                    "action": "stop_group",
                    "group_id": "review"
                }),
            ),
        ] {
            let envelope = evaluate_permission(tool, &args, &ctx);
            assert!(
                matches!(envelope.decision, HardDecision::NeedExternal { .. }),
                "{tool} {args} should still route through prompt mode; got {:?} from {:?}",
                envelope.decision,
                envelope.source
            );
            assert_eq!(
                envelope.source,
                DecisionSource::Mode {
                    mode: "prompt".to_string()
                }
            );
        }
    }

    #[test]
    fn ask_rule_precedes_internal_orchestration_short_circuit() {
        let ctx = crate::permission::types::PermissionSyncContext::new(
            crate::permission::types::InheritedPermissions {
                mode: crate::permission::types::PermissionMode::Prompt,
                ask_rules: vec![crate::permission::types::PermissionRule::tool("agent")],
                ..Default::default()
            },
        );

        let envelope = evaluate_permission(
            "agent",
            &serde_json::json!({
                "action": "spawn",
                "description": "Review",
                "prompt": "Check the diff"
            }),
            &ctx,
        );

        assert!(matches!(
            envelope.decision,
            HardDecision::NeedExternal { .. }
        ));
        assert!(matches!(envelope.source, DecisionSource::AskRule { .. }));
    }

    #[test]
    fn plan_mode_allows_read_only_tools_but_denies_mutations() {
        let ctx = crate::permission::types::PermissionSyncContext::new(
            crate::permission::types::InheritedPermissions {
                mode: crate::permission::types::PermissionMode::Plan,
                allowed_tools: Some(std::collections::HashSet::from([
                    "read_file".to_string(),
                    "write_file".to_string(),
                    "bash".to_string(),
                ])),
                ..Default::default()
            },
        );

        let read_only =
            evaluate_permission("read_file", &serde_json::json!({"path": "README.md"}), &ctx);
        assert!(matches!(read_only.decision, HardDecision::Allow));

        let write_file = evaluate_permission(
            "write_file",
            &serde_json::json!({"path": "src/lib.rs", "content": "pub fn x() {}\n"}),
            &ctx,
        );
        assert!(matches!(write_file.decision, HardDecision::Deny { .. }));

        let mutating_bash = evaluate_permission(
            "bash",
            &serde_json::json!({"command": "touch plan.txt"}),
            &ctx,
        );
        assert!(matches!(mutating_bash.decision, HardDecision::Deny { .. }));

        let read_only_bash = evaluate_permission(
            "bash",
            &serde_json::json!({"command": "git status --short"}),
            &ctx,
        );
        assert!(
            matches!(read_only_bash.decision, HardDecision::Allow),
            "plan mode must allow args-aware read-only shell exploration: {read_only_bash:?}"
        );
        assert_eq!(read_only_bash.source, DecisionSource::ReadShortCircuit);
    }

    #[test]
    fn plan_mode_allows_plan_control_tools_so_model_can_exit() {
        // Regression for session 4cb6b459: in plan mode the model
        // saw `exit_plan_mode` in its schema (kept by
        // `tool_schema_prune::PLAN_MODE_REQUIRED_TOOLS`), called it
        // to surface the authored plan, but the permission engine's
        // fallback denied it as "denied by permission mode" — leaving
        // the agent stuck in plan mode forever. The two layers must
        // agree: anything kept in the plan-mode schema must also pass
        // the runtime gate.
        let ctx = crate::permission::types::PermissionSyncContext::new(
            crate::permission::types::InheritedPermissions {
                mode: crate::permission::types::PermissionMode::Plan,
                ..Default::default()
            },
        );
        for tool in crate::tool::schema::prune::PLAN_MODE_REQUIRED_TOOLS {
            let envelope = evaluate_permission(tool, &serde_json::json!({}), &ctx);
            assert!(
                matches!(envelope.decision, HardDecision::Allow),
                "plan mode must allow `{tool}` so the agent can leave plan mode; got {:?}",
                envelope.decision
            );
        }
    }

    #[test]
    fn plan_mode_denial_reason_does_not_special_case_unsupported_plan_tool_shapes() {
        for tool_name in ["session", "agent"] {
            let reason = plan_mode_denial_reason(tool_name);
            assert!(reason.contains("read-only invocations"));
            assert!(reason.contains("exit_plan_mode(plan=...)"));
            assert!(!reason.contains("no longer routes through"));
            assert!(!reason.contains("Use `exit_plan_mode` directly"));
        }
    }

    #[test]
    fn generic_plan_mode_denial_reason_points_to_exit_tool() {
        let reason = plan_mode_denial_reason("bash");
        assert!(reason.contains("read-only invocations"));
        assert!(reason.contains("exit_plan_mode(plan=...)"));
    }

    #[test]
    fn deny_mode_overrides_read_short_circuit_allowlist() {
        let ctx = crate::permission::types::PermissionSyncContext::new(
            crate::permission::types::InheritedPermissions {
                mode: crate::permission::types::PermissionMode::Deny,
                allowed_tools: Some(std::collections::HashSet::from(["bash".to_string()])),
                ..Default::default()
            },
        );
        let envelope =
            evaluate_permission("bash", &serde_json::json!({"command": "git status"}), &ctx);

        assert!(matches!(envelope.decision, HardDecision::Deny { .. }));
        assert_eq!(
            envelope.source,
            DecisionSource::Mode {
                mode: "deny".to_string()
            }
        );
    }

    #[test]
    fn ask_rule_precedes_read_short_circuit() {
        let ctx = crate::permission::types::PermissionSyncContext::new(
            crate::permission::types::InheritedPermissions {
                mode: crate::permission::types::PermissionMode::Prompt,
                ask_rules: vec![crate::permission::types::PermissionRule::parse("bash()")],
                ..Default::default()
            },
        );
        let envelope =
            evaluate_permission("bash", &serde_json::json!({"command": "git status"}), &ctx);
        assert!(matches!(
            envelope.decision,
            HardDecision::NeedExternal { .. }
        ));
        assert!(matches!(envelope.source, DecisionSource::AskRule { .. }));
    }

    #[test]
    fn evaluate_denies_catastrophic_shell_before_auto_mode() {
        let ctx = crate::permission::types::PermissionSyncContext::root(
            crate::permission::types::PermissionMode::Auto,
        );
        let envelope =
            evaluate_permission("bash", &serde_json::json!({"command": "rm -rf /"}), &ctx);
        match envelope.decision {
            HardDecision::Deny { reason } => assert!(reason.contains("catastrophic command")),
            other => panic!("expected safety denial, got {other:?}"),
        }
        assert!(matches!(
            envelope.source,
            DecisionSource::SafetyMiddleware { .. }
        ));
    }

    #[test]
    fn evaluate_denies_catastrophic_shell_before_bypass_mode() {
        let ctx = crate::permission::types::PermissionSyncContext::root(
            crate::permission::types::PermissionMode::Bypass,
        );
        let envelope =
            evaluate_permission("bash", &serde_json::json!({"command": "rm -rf /"}), &ctx);
        assert!(matches!(envelope.decision, HardDecision::Deny { .. }));
        assert!(matches!(
            envelope.source,
            DecisionSource::SafetyMiddleware { .. }
        ));
    }

    #[test]
    fn evaluate_non_catastrophic_rm_requires_approval_not_hard_deny() {
        let ctx = crate::permission::types::PermissionSyncContext::root(
            crate::permission::types::PermissionMode::Prompt,
        );
        let envelope = evaluate_permission(
            "bash",
            &serde_json::json!({"command": "rm -rf /tmp/foo"}),
            &ctx,
        );

        assert!(
            matches!(envelope.decision, HardDecision::NeedExternal { .. }),
            "bounded destructive rm should be reviewable, got {:?}",
            envelope.decision
        );
        assert!(matches!(
            envelope.source,
            DecisionSource::ExplicitApprovalGate { .. }
        ));
    }

    #[test]
    fn evaluate_sudo_root_rm_stays_hard_denied_without_tmp_overmatch() {
        let ctx = crate::permission::types::PermissionSyncContext::root(
            crate::permission::types::PermissionMode::Prompt,
        );
        let root = evaluate_permission(
            "bash",
            &serde_json::json!({"command": "SUDO -n rm -rf /"}),
            &ctx,
        );
        assert!(
            matches!(root.decision, HardDecision::Deny { .. }),
            "sudo root rm must be hard denied even through wrapper options, got {:?}",
            root.decision
        );

        let tmp = evaluate_permission(
            "bash",
            &serde_json::json!({"command": "sudo rm -rf /tmp/foo"}),
            &ctx,
        );
        assert!(
            matches!(tmp.decision, HardDecision::NeedExternal { .. }),
            "sudo rm under /tmp should be reviewable, got {:?}",
            tmp.decision
        );
        assert!(matches!(
            tmp.source,
            DecisionSource::ExplicitApprovalGate { .. }
        ));
    }

    #[test]
    fn execute_hard_deny_root_chmod_does_not_overmatch_tmp_paths() {
        let ctx = crate::permission::types::PermissionSyncContext::root(
            crate::permission::types::PermissionMode::Prompt,
        );
        let envelope = evaluate_permission(
            "bash",
            &serde_json::json!({"command": "chmod 777 /tmp/foo"}),
            &ctx,
        );

        assert!(
            matches!(envelope.decision, HardDecision::NeedExternal { .. }),
            "chmod under /tmp should be reviewable, got {:?}",
            envelope.decision
        );
        assert!(matches!(
            envelope.source,
            DecisionSource::ExplicitApprovalGate { .. }
        ));
    }

    #[test]
    fn evaluate_explicit_approval_precedes_allow_rule_in_prompt_mode() {
        let ctx = crate::permission::types::PermissionSyncContext::new(
            crate::permission::types::InheritedPermissions {
                mode: crate::permission::types::PermissionMode::Prompt,
                allow_rules: vec![crate::permission::types::PermissionRule::parse("git")],
                ..Default::default()
            },
        );
        let envelope = evaluate_permission(
            "git",
            &serde_json::json!({"action": "commit", "message": "ship it"}),
            &ctx,
        );
        assert!(matches!(
            envelope.decision,
            HardDecision::NeedExternal { .. }
        ));
        assert!(matches!(
            envelope.source,
            DecisionSource::ExplicitApprovalGate { .. }
        ));
    }

    #[test]
    fn evaluate_sensitive_path_requests_external_approval_in_auto_mode() {
        let ctx = crate::permission::types::PermissionSyncContext::root(
            crate::permission::types::PermissionMode::Auto,
        );
        let envelope = evaluate_permission(
            "write_file",
            &serde_json::json!({"path": ".env", "content": "TOKEN=x"}),
            &ctx,
        );
        assert!(matches!(
            envelope.decision,
            HardDecision::NeedExternal { .. }
        ));
        assert!(matches!(
            envelope.source,
            DecisionSource::SensitivePath { .. }
        ));
    }

    #[test]
    fn evaluate_sensitive_path_is_allowed_in_bypass_mode() {
        let ctx = crate::permission::types::PermissionSyncContext::root(
            crate::permission::types::PermissionMode::Bypass,
        );
        let envelope = evaluate_permission(
            "write_file",
            &serde_json::json!({"path": ".env", "content": "TOKEN=x"}),
            &ctx,
        );
        assert!(matches!(envelope.decision, HardDecision::Allow));
        assert!(matches!(
            envelope.source,
            DecisionSource::SensitivePath { .. }
        ));
    }

    #[test]
    fn evaluate_sensitive_path_session_override_is_honored() {
        let args = serde_json::json!({"path": ".env", "content": "TOKEN=x"});
        let mut overrides = FingerprintedOverrides::default();
        overrides.insert(content_aware_fingerprint("write_file", &args), true);
        let ctx = crate::permission::types::PermissionSyncContext::new(
            crate::permission::types::InheritedPermissions {
                mode: crate::permission::types::PermissionMode::Prompt,
                fingerprinted_overrides: Some(serde_json::to_value(overrides).unwrap()),
                ..Default::default()
            },
        );

        let envelope = evaluate_permission("write_file", &args, &ctx);

        assert!(matches!(envelope.decision, HardDecision::Allow));
        assert!(matches!(
            envelope.source,
            DecisionSource::SessionOverride { allowed: true }
        ));
    }

    #[test]
    fn evaluate_sensitive_path_broad_session_override_does_not_fall_through() {
        let args = serde_json::json!({"path": ".env", "content": "TOKEN=x"});
        let mut overrides = FingerprintedOverrides::default();
        overrides.insert(ApprovalFingerprint::bare("write_file"), true);
        let ctx = crate::permission::types::PermissionSyncContext::new(
            crate::permission::types::InheritedPermissions {
                mode: crate::permission::types::PermissionMode::Auto,
                fingerprinted_overrides: Some(serde_json::to_value(overrides).unwrap()),
                ..Default::default()
            },
        );

        let envelope = evaluate_permission("write_file", &args, &ctx);

        assert!(matches!(
            envelope.decision,
            HardDecision::NeedExternal { .. }
        ));
        assert!(matches!(
            envelope.source,
            DecisionSource::SensitivePath { .. }
        ));
        assert!(
            envelope.trace.iter().any(|step| step
                .note
                .contains("broad session override cannot bypass sensitive path")),
            "ignored broad override should remain visible in trace: {:?}",
            envelope.trace
        );
    }

    #[test]
    fn evaluate_reading_session_tool_result_artifact_is_allowed_in_auto_mode() {
        let ctx = crate::permission::types::PermissionSyncContext::root(
            crate::permission::types::PermissionMode::Auto,
        );
        let (_temp, _guard, artifact_path) = create_current_session_artifact();
        let artifact_path = artifact_path.to_string_lossy().to_string();

        let read_file = evaluate_permission(
            "read_file",
            &serde_json::json!({"path": artifact_path.clone()}),
            &ctx,
        );
        assert!(
            matches!(read_file.decision, HardDecision::Allow),
            "system-generated tool result artifacts must be readable in Auto mode: {read_file:?}"
        );

        let bash_read = evaluate_permission(
            "bash",
            &serde_json::json!({"command": format!("cat {artifact_path}")}),
            &ctx,
        );
        assert!(
            matches!(bash_read.decision, HardDecision::Allow),
            "read-only processing of tool result artifacts must not require manual approval: {bash_read:?}"
        );
    }

    #[test]
    fn evaluate_reading_session_journal_is_allowed_in_auto_mode() {
        let ctx = crate::permission::types::PermissionSyncContext::root(
            crate::permission::types::PermissionMode::Auto,
        );
        let (_temp, _guard, journal_path) = create_current_session_journal();
        let journal_path = journal_path.to_string_lossy().to_string();

        let grep = evaluate_permission(
            "grep",
            &serde_json::json!({
                "pattern": "str_replace|str replace",
                "path": journal_path.clone()
            }),
            &ctx,
        );
        assert!(
            matches!(grep.decision, HardDecision::Allow),
            "session journals are first-party diagnostic artifacts and must be searchable in Auto mode: {grep:?}"
        );

        let bash_read = evaluate_permission(
            "bash",
            &serde_json::json!({"command": format!("grep 'str_replace' {journal_path}")}),
            &ctx,
        );
        assert!(
            matches!(bash_read.decision, HardDecision::Allow),
            "read-only shell searches of session journals must not require manual approval: {bash_read:?}"
        );
    }

    #[test]
    fn evaluate_reading_hidden_home_app_logs_is_allowed_in_auto_mode() {
        let ctx = crate::permission::types::PermissionSyncContext::root(
            crate::permission::types::PermissionMode::Auto,
        );
        let log_path = "~/.xxx/logs/session.log".to_string();

        let read_file = evaluate_permission(
            "read_file",
            &serde_json::json!({"path": log_path.clone()}),
            &ctx,
        );
        assert!(
            matches!(read_file.decision, HardDecision::Allow),
            "read-only hidden app logs should not require directory-specific opt-in: {read_file:?}"
        );

        let bash_read = evaluate_permission(
            "bash",
            &serde_json::json!({"command": format!("tail -20 {log_path}")}),
            &ctx,
        );
        assert!(
            matches!(bash_read.decision, HardDecision::Allow),
            "read-only shell inspection of hidden app logs should be allowed: {bash_read:?}"
        );
    }

    #[test]
    fn evaluate_writing_hidden_home_app_state_requires_approval_in_auto_mode() {
        let ctx = crate::permission::types::PermissionSyncContext::root(
            crate::permission::types::PermissionMode::Auto,
        );
        let config_path = "~/.yyy/config.toml".to_string();

        let write_file = evaluate_permission(
            "write_file",
            &serde_json::json!({"path": config_path.clone(), "content": "mode = 'new'\n"}),
            &ctx,
        );
        assert!(
            matches!(write_file.decision, HardDecision::NeedExternal { .. }),
            "hidden home app state writes must remain gated: {write_file:?}"
        );

        let bash_rm = evaluate_permission(
            "bash",
            &serde_json::json!({"command": format!("rm -f {config_path}")}),
            &ctx,
        );
        assert!(
            matches!(bash_rm.decision, HardDecision::NeedExternal { .. }),
            "destructive shell operations on hidden home app state must remain gated: {bash_rm:?}"
        );
    }

    #[test]
    fn evaluate_reading_hidden_home_secret_requires_approval_in_auto_mode() {
        let ctx = crate::permission::types::PermissionSyncContext::root(
            crate::permission::types::PermissionMode::Auto,
        );
        let secret_path = "~/.xxx/.env".to_string();

        let read_file = evaluate_permission(
            "read_file",
            &serde_json::json!({"path": secret_path.clone()}),
            &ctx,
        );
        assert!(
            matches!(read_file.decision, HardDecision::NeedExternal { .. }),
            "hidden app state is readable only until it is credential-shaped: {read_file:?}"
        );
        assert!(matches!(
            read_file.source,
            DecisionSource::SensitivePath { .. }
        ));

        let bash_read = evaluate_permission(
            "bash",
            &serde_json::json!({"command": format!("cat {secret_path}")}),
            &ctx,
        );
        assert!(
            matches!(bash_read.decision, HardDecision::NeedExternal { .. }),
            "shell reads of hidden-home credentials must still gate: {bash_read:?}"
        );
        assert!(matches!(
            bash_read.source,
            DecisionSource::SensitivePath { .. }
        ));
    }

    #[test]
    fn evaluate_interpreter_pipeline_over_tool_result_skips_sensitive_path_gate() {
        let ctx = crate::permission::types::PermissionSyncContext::root(
            crate::permission::types::PermissionMode::Auto,
        );
        let (_temp, _guard, artifact_path) = create_current_session_artifact();
        let artifact_path = artifact_path.to_string_lossy().to_string();

        let bash_read = evaluate_permission(
            "bash",
            &serde_json::json!({"command": format!("cat {artifact_path} | python3 -c 'import sys; print(sys.stdin.read()[:10])'")}),
            &ctx,
        );
        assert!(
            matches!(bash_read.decision, HardDecision::Allow),
            "Auto mode should let the existing shell gate decide after sensitive-path skips internal artifacts: {bash_read:?}"
        );
        assert!(matches!(
            bash_read.source,
            DecisionSource::ExplicitApprovalGate { .. }
        ));
    }

    #[test]
    fn sensitive_path_match_ignores_internal_artifact_refs_inside_shell_pipelines() {
        let (_temp, _guard, artifact_path) = create_current_session_artifact();
        std::fs::write(&artifact_path, "{\"ok\":true}").unwrap();
        let artifact_path = artifact_path.to_string_lossy().to_string();

        let args = serde_json::json!({
            "command": format!("cat {artifact_path} | python3 -c 'import sys, json; print(json.load(sys.stdin))'")
        });

        assert_eq!(
            sensitive_path_match("bash", &args),
            None,
            "internal tool-result artifacts must not trigger the sensitive-path opt-in gate"
        );
    }

    #[test]
    fn evaluate_read_only_bash_mixed_internal_and_secret_path_requires_approval() {
        let ctx = crate::permission::types::PermissionSyncContext::root(
            crate::permission::types::PermissionMode::Auto,
        );
        let (_temp, _guard, artifact_path) = create_current_session_artifact();
        let artifact_path = artifact_path.to_string_lossy().to_string();

        let bash_read = evaluate_permission(
            "bash",
            &serde_json::json!({"command": format!("cat {artifact_path} ~/.ssh/id_rsa")}),
            &ctx,
        );
        assert!(
            matches!(bash_read.decision, HardDecision::NeedExternal { .. }),
            "an internal artifact must not mask a separate sensitive path: {bash_read:?}"
        );
        assert!(matches!(
            bash_read.source,
            DecisionSource::SensitivePath { .. }
        ));
    }

    #[test]
    fn evaluate_grep_pattern_mentioning_sensitive_name_is_not_sensitive_in_any_mode() {
        let args = serde_json::json!({
            "command": r#"grep -n "fn resolve_checked\|sensitive credential\|SANDBOX_DENIED_PREFIX\|\.ssh\|credentials.json" crates/astra-cli/src/edge_tools/shell.rs"#
        });

        for mode in [
            crate::permission::types::PermissionMode::Prompt,
            crate::permission::types::PermissionMode::AcceptEdits,
            crate::permission::types::PermissionMode::Auto,
        ] {
            let ctx = crate::permission::types::PermissionSyncContext::root(mode);
            let envelope = evaluate_permission("bash", &args, &ctx);
            assert!(
                !matches!(envelope.source, DecisionSource::SensitivePath { .. }),
                "{mode:?} must not treat grep search text as a sensitive path: {envelope:?}"
            );
        }
    }

    #[test]
    fn evaluate_grep_sensitive_file_operand_is_sensitive_in_any_mode() {
        let args = serde_json::json!({
            "command": "grep -n needle ~/.ssh/id_rsa"
        });

        for mode in [
            crate::permission::types::PermissionMode::Prompt,
            crate::permission::types::PermissionMode::AcceptEdits,
            crate::permission::types::PermissionMode::Auto,
        ] {
            let ctx = crate::permission::types::PermissionSyncContext::root(mode);
            let envelope = evaluate_permission("bash", &args, &ctx);
            assert!(
                matches!(envelope.source, DecisionSource::SensitivePath { .. }),
                "{mode:?} must gate a real sensitive file operand: {envelope:?}"
            );
        }
    }

    #[test]
    fn evaluate_writing_session_tool_result_artifact_still_requires_approval() {
        let ctx = crate::permission::types::PermissionSyncContext::root(
            crate::permission::types::PermissionMode::Auto,
        );
        let artifact_path = "/Users/test/.astra/sessions/session-1/tool-results/call_abc.txt";

        let write_file = evaluate_permission(
            "write_file",
            &serde_json::json!({"path": artifact_path, "content": "tamper"}),
            &ctx,
        );
        assert!(
            !matches!(write_file.decision, HardDecision::Allow),
            "tool result artifacts are read-only system state"
        );

        let bash_rm = evaluate_permission(
            "bash",
            &serde_json::json!({"command": format!("rm -f {artifact_path}")}),
            &ctx,
        );
        assert!(
            !matches!(bash_rm.decision, HardDecision::Allow),
            "destructive shell operations on tool result artifacts must remain gated"
        );
    }

    #[test]
    fn evaluate_writing_session_journal_still_requires_approval() {
        let ctx = crate::permission::types::PermissionSyncContext::root(
            crate::permission::types::PermissionMode::Auto,
        );
        let (_temp, _guard, journal_path) = create_current_session_journal();
        let journal_path = journal_path.to_string_lossy().to_string();

        let write_file = evaluate_permission(
            "write_file",
            &serde_json::json!({"path": journal_path.clone(), "content": "tamper"}),
            &ctx,
        );
        assert!(
            !matches!(write_file.decision, HardDecision::Allow),
            "session journals are read-only diagnostic state"
        );

        let bash_rm = evaluate_permission(
            "bash",
            &serde_json::json!({"command": format!("rm -f {journal_path}")}),
            &ctx,
        );
        assert!(
            !matches!(bash_rm.decision, HardDecision::Allow),
            "destructive shell operations on session journals must remain gated"
        );
    }

    #[test]
    fn evaluate_execute_hard_deny_stays_denied_in_auto_mode() {
        let ctx = crate::permission::types::PermissionSyncContext::root(
            crate::permission::types::PermissionMode::Auto,
        );
        let envelope = evaluate_permission(
            "bash",
            &serde_json::json!({"command": "shred /dev/sda"}),
            &ctx,
        );
        assert!(matches!(envelope.decision, HardDecision::Deny { .. }));
        assert!(matches!(
            envelope.source,
            DecisionSource::ExecuteHardDeny { .. }
        ));
    }

    #[test]
    fn bypass_git_advisory_does_not_skip_later_execute_boundary() {
        let ctx = crate::permission::types::PermissionSyncContext::root(
            crate::permission::types::PermissionMode::Bypass,
        );
        let envelope = evaluate_permission(
            "bash",
            &serde_json::json!({"command": "cd /workspace && git status | bash"}),
            &ctx,
        );

        assert!(matches!(envelope.decision, HardDecision::Deny { .. }));
        assert!(matches!(
            envelope.source,
            DecisionSource::ExecuteHardDeny { .. }
        ));
        assert!(envelope.risk_tags.contains(&RiskTag::BashExecute));
    }

    #[test]
    fn bypass_allows_read_only_cd_git_pipeline_without_approval() {
        let ctx = crate::permission::types::PermissionSyncContext::root(
            crate::permission::types::PermissionMode::Bypass,
        );
        let envelope = evaluate_permission(
            "bash",
            &serde_json::json!({
                "command": "cd /workspace && git diff origin/main...HEAD --stat | awk '{print $1}'"
            }),
            &ctx,
        );

        assert!(matches!(envelope.decision, HardDecision::Allow));
    }

    #[test]
    fn structured_git_force_push_feature_branch_is_soft_in_auto_mode() {
        let ctx = crate::permission::types::PermissionSyncContext::root(
            crate::permission::types::PermissionMode::Auto,
        );
        let envelope = evaluate_permission(
            "git",
            &serde_json::json!({
                "action": "push",
                "remote": "origin",
                "branch": "feature/my-branch",
                "force_with_lease": true
            }),
            &ctx,
        );

        assert!(matches!(envelope.decision, HardDecision::Allow));
        assert!(envelope.risk_tags.contains(&RiskTag::GitDestructive));
    }

    #[test]
    fn structured_git_force_push_feature_branch_is_allowed_in_bypass_mode() {
        let ctx = crate::permission::types::PermissionSyncContext::root(
            crate::permission::types::PermissionMode::Bypass,
        );
        let envelope = evaluate_permission(
            "git",
            &serde_json::json!({
                "action": "push",
                "remote": "origin",
                "branch": "feature/my-branch",
                "force_with_lease": true
            }),
            &ctx,
        );

        assert!(matches!(envelope.decision, HardDecision::Allow));
        assert!(envelope.risk_tags.contains(&RiskTag::GitDestructive));
    }

    #[test]
    fn structured_git_force_push_protected_branch_requires_approval_in_auto_mode() {
        let ctx = crate::permission::types::PermissionSyncContext::root(
            crate::permission::types::PermissionMode::Auto,
        );
        let envelope = evaluate_permission(
            "git",
            &serde_json::json!({
                "action": "push",
                "remote": "origin",
                "branch": "main",
                "force_with_lease": true
            }),
            &ctx,
        );

        assert!(matches!(
            envelope.decision,
            HardDecision::NeedExternal { .. }
        ));
        assert!(matches!(envelope.source, DecisionSource::GitSafety { .. }));
        assert!(envelope.risk_tags.contains(&RiskTag::GitDestructive));
    }

    #[test]
    fn structured_git_force_push_protected_branch_is_advisory_in_bypass_mode() {
        let ctx = crate::permission::types::PermissionSyncContext::root(
            crate::permission::types::PermissionMode::Bypass,
        );
        let envelope = evaluate_permission(
            "git",
            &serde_json::json!({
                "action": "push",
                "remote": "origin",
                "branch": "main",
                "force_with_lease": true
            }),
            &ctx,
        );

        assert!(matches!(envelope.decision, HardDecision::Allow));
        assert!(envelope.risk_tags.contains(&RiskTag::GitDestructive));
    }

    #[test]
    fn git_worktree_destructive_bash_is_advisory_in_auto_mode() {
        let ctx = crate::permission::types::PermissionSyncContext::root(
            crate::permission::types::PermissionMode::Auto,
        );
        let envelope = evaluate_permission(
            "bash",
            &serde_json::json!({
                "command": "git restore --staged --worktree crates/foo/src/lib.rs"
            }),
            &ctx,
        );

        assert!(matches!(envelope.decision, HardDecision::Allow));
        assert!(envelope.risk_tags.contains(&RiskTag::GitDestructive));
        assert!(
            envelope
                .trace
                .iter()
                .any(|step| step.note.contains("soft git risk retained as advisory"))
        );
    }

    #[test]
    fn git_worktree_destructive_bash_session_override_is_honored() {
        let args = serde_json::json!({
            "command": "git restore --staged --worktree crates/foo/src/lib.rs"
        });
        let mut overrides = FingerprintedOverrides::default();
        overrides.insert(content_aware_fingerprint("bash", &args), true);
        let ctx = crate::permission::types::PermissionSyncContext::new(
            crate::permission::types::InheritedPermissions {
                mode: crate::permission::types::PermissionMode::Auto,
                fingerprinted_overrides: Some(serde_json::to_value(overrides).unwrap()),
                ..Default::default()
            },
        );

        let envelope = evaluate_permission("bash", &args, &ctx);

        assert!(matches!(envelope.decision, HardDecision::Allow));
        assert!(matches!(
            envelope.source,
            DecisionSource::SessionOverride { allowed: true }
        ));
    }

    #[test]
    fn git_worktree_destructive_broad_session_override_remains_advisory() {
        let args = serde_json::json!({
            "command": "git restore --staged --worktree crates/foo/src/lib.rs"
        });
        let mut overrides = FingerprintedOverrides::default();
        overrides.insert(ApprovalFingerprint::bare("bash"), true);
        let ctx = crate::permission::types::PermissionSyncContext::new(
            crate::permission::types::InheritedPermissions {
                mode: crate::permission::types::PermissionMode::Auto,
                fingerprinted_overrides: Some(serde_json::to_value(overrides).unwrap()),
                ..Default::default()
            },
        );

        let envelope = evaluate_permission("bash", &args, &ctx);

        assert!(matches!(envelope.decision, HardDecision::Allow));
        assert!(
            envelope.trace.iter().any(|step| step
                .note
                .contains("broad session override cannot bypass git safety")),
            "ignored broad override should stay visible in trace: {:?}",
            envelope.trace
        );
    }

    #[test]
    fn git_worktree_destructive_prefix_session_override_remains_advisory() {
        let args = serde_json::json!({
            "command": "git restore --staged --worktree crates/foo/src/lib.rs"
        });
        let mut overrides = FingerprintedOverrides::default();
        overrides.insert(
            ApprovalFingerprint::shell_prefix("bash", "git restore", false),
            true,
        );
        let ctx = crate::permission::types::PermissionSyncContext::new(
            crate::permission::types::InheritedPermissions {
                mode: crate::permission::types::PermissionMode::Auto,
                fingerprinted_overrides: Some(serde_json::to_value(overrides).unwrap()),
                ..Default::default()
            },
        );

        let envelope = evaluate_permission("bash", &args, &ctx);

        assert!(matches!(envelope.decision, HardDecision::Allow));
        assert!(
            envelope.trace.iter().any(|step| step
                .note
                .contains("broad session override cannot bypass git safety")),
            "ignored prefix override should stay visible in trace: {:?}",
            envelope.trace
        );
    }

    #[test]
    fn structured_git_force_push_respects_auto_allowlist() {
        let ctx = crate::permission::types::PermissionSyncContext::new(
            crate::permission::types::InheritedPermissions {
                mode: crate::permission::types::PermissionMode::Auto,
                allowed_tools: Some(std::collections::HashSet::from(["read_file".to_string()])),
                ..Default::default()
            },
        );
        let envelope = evaluate_permission(
            "git",
            &serde_json::json!({
                "action": "push",
                "remote": "origin",
                "branch": "feature/my-branch",
                "force_with_lease": true
            }),
            &ctx,
        );

        assert!(matches!(envelope.decision, HardDecision::Deny { .. }));
        assert_eq!(
            envelope.source,
            DecisionSource::Mode {
                mode: "agent policy allowlist".to_string()
            }
        );
    }

    #[test]
    fn bypass_mode_git_violation_still_respects_child_allowlist() {
        let ctx = crate::permission::types::PermissionSyncContext::new(
            crate::permission::types::InheritedPermissions {
                mode: crate::permission::types::PermissionMode::Bypass,
                allowed_tools: Some(std::collections::HashSet::from(["read_file".to_string()])),
                ..Default::default()
            },
        );
        let envelope = evaluate_permission(
            "git",
            &serde_json::json!({
                "action": "push",
                "remote": "origin",
                "branch": "feature/my-branch",
                "force_with_lease": true
            }),
            &ctx,
        );

        assert!(matches!(envelope.decision, HardDecision::Deny { .. }));
        assert_eq!(
            envelope.source,
            DecisionSource::Mode {
                mode: "agent policy allowlist".to_string()
            }
        );
    }

    #[test]
    fn accept_edits_mode_allows_workspace_write_tools() {
        let ctx = crate::permission::types::PermissionSyncContext::root(
            crate::permission::types::PermissionMode::AcceptEdits,
        );

        for (tool, args) in [
            (
                "write_file",
                serde_json::json!({"path": "src/lib.rs", "content": "pub fn demo() {}\n"}),
            ),
            (
                "str_replace",
                serde_json::json!({"path": "src/lib.rs", "old_str": "demo", "new_str": "ship"}),
            ),
        ] {
            let envelope = evaluate_permission(tool, &args, &ctx);
            assert!(
                matches!(envelope.decision, HardDecision::Allow),
                "{tool} should auto-allow in accept_edits: {:?}",
                envelope
            );
            assert_eq!(
                envelope.source,
                DecisionSource::Mode {
                    mode: "accept_edits".to_string()
                }
            );
        }
    }

    #[test]
    fn accept_edits_mode_prompts_for_bash_execution() {
        let ctx = crate::permission::types::PermissionSyncContext::root(
            crate::permission::types::PermissionMode::AcceptEdits,
        );
        let envelope =
            evaluate_permission("bash", &serde_json::json!({"command": "cargo test"}), &ctx);

        assert!(matches!(
            envelope.decision,
            HardDecision::NeedExternal { .. }
        ));
        assert!(matches!(
            envelope.source,
            DecisionSource::ExplicitApprovalGate { .. }
        ));
    }

    #[test]
    fn accept_edits_mode_prompts_for_parent_relative_write_escape() {
        let ctx = crate::permission::types::PermissionSyncContext::root(
            crate::permission::types::PermissionMode::AcceptEdits,
        );
        let envelope = evaluate_permission(
            "write_file",
            &serde_json::json!({"path": "../outside.rs", "content": "nope"}),
            &ctx,
        );

        assert!(matches!(
            envelope.decision,
            HardDecision::NeedExternal { .. }
        ));
    }

    #[test]
    fn accept_edits_mode_allowlist_blocks_unlisted_bash() {
        let ctx = crate::permission::types::PermissionSyncContext::new(
            crate::permission::types::InheritedPermissions {
                mode: crate::permission::types::PermissionMode::AcceptEdits,
                allowed_tools: Some(std::collections::HashSet::from(["write_file".to_string()])),
                ..Default::default()
            },
        );
        let envelope =
            evaluate_permission("bash", &serde_json::json!({"command": "cargo test"}), &ctx);

        assert!(matches!(envelope.decision, HardDecision::Deny { .. }));
        assert_eq!(
            envelope.source,
            DecisionSource::Mode {
                mode: "agent policy allowlist".to_string()
            }
        );
    }

    fn provider_policy(
        effect: astra_turn_types::ResolvedToolEffect,
        parallelizable: bool,
        approval: crate::provider_resolution::ProviderApprovalBaseline,
    ) -> crate::provider_resolution::ResolvedInvocationPolicy {
        crate::provider_resolution::ResolvedInvocationPolicy {
            descriptor: astra_turn_types::ResolvedToolDescriptorRef::new(
                astra_turn_types::ToolIdentity::new(
                    astra_turn_types::ProviderBindingRef::new("binding").unwrap(),
                    astra_turn_types::NativeToolId::new("native").unwrap(),
                ),
                "version",
            )
            .unwrap(),
            effect,
            parallelizable,
            approval,
            idempotency: if effect == astra_turn_types::ResolvedToolEffect::ReadOnly {
                astra_turn_types::ResolvedToolIdempotency::PureRead
            } else {
                astra_turn_types::ResolvedToolIdempotency::NonIdempotent
            },
            semantic_cache: astra_turn_types::ResolvedSemanticCacheBaseline::Disabled,
            diagnostics: Vec::new(),
        }
    }

    #[test]
    fn provider_read_policy_drives_the_same_permission_short_circuit_as_batching() {
        let ctx = crate::permission::types::PermissionSyncContext::root(
            crate::permission::types::PermissionMode::Prompt,
        );
        let policy = provider_policy(
            astra_turn_types::ResolvedToolEffect::ReadOnly,
            true,
            crate::provider_resolution::ProviderApprovalBaseline::NoAdditionalApproval,
        );
        let envelope = evaluate_permission_with_provider_policy(
            "mcp__provider__read",
            &serde_json::json!({}),
            &ctx,
            Some(&policy),
        );

        assert_eq!(envelope.decision, HardDecision::Allow);
        assert_eq!(envelope.source, DecisionSource::ReadShortCircuit);
        assert!(
            envelope.risk_tags.is_empty(),
            "resolved read-only provider inherited static risk tags: {:?}",
            envelope.risk_tags
        );
        assert!(policy.parallelizable);
    }

    #[test]
    fn provider_unknown_and_mutating_policies_require_explicit_approval() {
        let ctx = crate::permission::types::PermissionSyncContext::root(
            crate::permission::types::PermissionMode::Prompt,
        );
        for (effect, expected_tag) in [
            (
                astra_turn_types::ResolvedToolEffect::Unknown,
                RiskTag::ProviderUnknownCapability,
            ),
            (
                astra_turn_types::ResolvedToolEffect::Mutating,
                RiskTag::ProviderSideEffect,
            ),
        ] {
            let policy = provider_policy(
                effect,
                false,
                crate::provider_resolution::ProviderApprovalBaseline::RequiresApproval,
            );
            let envelope = evaluate_permission_with_provider_policy(
                "mcp__provider__effect",
                &serde_json::json!({}),
                &ctx,
                Some(&policy),
            );

            assert!(matches!(
                envelope.decision,
                HardDecision::NeedExternal { .. }
            ));
            assert!(matches!(
                envelope.source,
                DecisionSource::ExplicitApprovalGate { .. }
            ));
            assert!(envelope.risk_tags.contains(&expected_tag));
            assert!(!envelope.risk_tags.contains(&RiskTag::MCPUnknownCapability));
            assert!(!envelope.risk_tags.contains(&RiskTag::BashExecute));
            assert!(!policy.parallelizable);
        }
    }

    #[test]
    fn provider_read_policy_does_not_bypass_an_explicit_deny_rule() {
        let ctx = crate::permission::types::PermissionSyncContext::new(
            crate::permission::types::InheritedPermissions {
                mode: crate::permission::types::PermissionMode::Prompt,
                deny_rules: vec![crate::permission::types::PermissionRule::tool(
                    "mcp__provider__read",
                )],
                ..Default::default()
            },
        );
        let policy = provider_policy(
            astra_turn_types::ResolvedToolEffect::ReadOnly,
            true,
            crate::provider_resolution::ProviderApprovalBaseline::NoAdditionalApproval,
        );
        let envelope = evaluate_permission_with_provider_policy(
            "mcp__provider__read",
            &serde_json::json!({}),
            &ctx,
            Some(&policy),
        );

        assert!(matches!(envelope.decision, HardDecision::Deny { .. }));
        assert!(matches!(envelope.source, DecisionSource::DenyRule { .. }));
    }

    // ── Issue #326 P5 / R2 Major 5: MCP capability metadata ──

    #[test]
    fn mcp_metadata_known_when_any_hint_set() {
        let meta = ToolCapabilityMetadata {
            destructive_hint: Some(true),
            ..Default::default()
        };
        assert!(meta.is_known());

        let meta = ToolCapabilityMetadata {
            read_only_hint: Some(false),
            ..Default::default()
        };
        assert!(meta.is_known());
    }

    #[test]
    fn mcp_metadata_unknown_when_no_hints() {
        let meta = ToolCapabilityMetadata::default();
        assert!(!meta.is_known());
        assert_eq!(meta.risk_tag(), Some(RiskTag::MCPUnknownCapability));
    }

    #[test]
    fn mcp_metadata_read_only_skips_tag() {
        let meta = ToolCapabilityMetadata {
            read_only_hint: Some(true),
            destructive_hint: Some(false),
            ..Default::default()
        };
        // No risk tag for explicit read-only — the engine can
        // route this through ReadShortCircuit without prompting.
        assert!(meta.risk_tag().is_none());
    }

    #[test]
    fn mcp_metadata_destructive_overrides_read_only() {
        // Server contradicting itself (read_only=true AND
        // destructive=true) is suspicious; the destructive flag
        // wins so the user is asked.
        let meta = ToolCapabilityMetadata {
            read_only_hint: Some(true),
            destructive_hint: Some(true),
            ..Default::default()
        };
        assert_eq!(meta.risk_tag(), Some(RiskTag::MCPUnknownCapability));
    }

    #[test]
    fn work_plan_policy_auto_admits_only_typed_non_disruptive_increments() {
        let ctx = crate::permission::types::PermissionSyncContext::root(
            crate::permission::types::PermissionMode::Prompt,
        );
        let safe = serde_json::json!({
            "context_id": "work-plan-context:basis",
            "reason": "Add the next independently verifiable task",
            "additions": [{
                "item_id": "new-task",
                "kind": "task",
                "objective": "Text is not a policy signal, even if it says delete everything",
                "expected_result": "A typed new node exists"
            }],
            "revisions": [],
            "dependencies": [{
                "predecessor_item_id": "accepted-task",
                "successor_item_id": "new-task"
            }],
            "dependency_removals": []
        });
        let admitted = evaluate_permission("propose_work_plan", &safe, &ctx);
        assert!(matches!(admitted.decision, HardDecision::Allow));
        assert!(matches!(
            admitted.source,
            DecisionSource::InternalOrchestration
        ));

        let mut reorders_accepted_work = safe;
        reorders_accepted_work["dependencies"][0]["predecessor_item_id"] =
            Value::String("new-task".to_string());
        reorders_accepted_work["dependencies"][0]["successor_item_id"] =
            Value::String("accepted-task".to_string());
        let reviewed = evaluate_permission("propose_work_plan", &reorders_accepted_work, &ctx);
        assert!(matches!(
            reviewed.decision,
            HardDecision::NeedExternal { .. }
        ));

        let broad = serde_json::json!({
            "context_id": "work-plan-context:basis",
            "reason": "Expand the bounded frontier",
            "additions": (0..=SAFE_WORK_PLAN_MAX_ADDITIONS)
                .map(|index| serde_json::json!({
                    "item_id": format!("task-{index}"),
                    "kind": "task",
                    "objective": "Typed objective",
                    "expected_result": "Typed result"
                }))
                .collect::<Vec<_>>(),
            "revisions": [],
            "dependencies": [],
            "dependency_removals": []
        });
        let broad = evaluate_permission("propose_work_plan", &broad, &ctx);
        assert!(matches!(broad.decision, HardDecision::NeedExternal { .. }));

        let mut revises_existing_work = reorders_accepted_work.clone();
        revises_existing_work["dependencies"] = serde_json::json!([]);
        revises_existing_work["revisions"] = serde_json::json!([{
            "item_id": "accepted-task",
            "expected_revision": 1,
            "kind": "task",
            "objective": "Keep the declared objective",
            "expected_result": "Keep the declared result",
            "declaration_state": "cancelled"
        }]);
        let reviewed = evaluate_permission("propose_work_plan", &revises_existing_work, &ctx);
        assert!(matches!(
            reviewed.decision,
            HardDecision::NeedExternal { .. }
        ));

        let deny_ctx = crate::permission::types::PermissionSyncContext::root(
            crate::permission::types::PermissionMode::Deny,
        );
        let denied = evaluate_permission("propose_work_plan", &reorders_accepted_work, &deny_ctx);
        assert!(matches!(denied.decision, HardDecision::Deny { .. }));
    }

    #[test]
    fn provisional_criteria_proposal_never_uses_authored_text_as_policy_input() {
        let ctx = crate::permission::types::PermissionSyncContext::root(
            crate::permission::types::PermissionMode::Prompt,
        );
        for statement in [
            "Relevant tests pass.",
            "Words such as delete, network, or production are not policy signals.",
        ] {
            let proposal = serde_json::json!({
                "context_id": "work-plan-context:basis",
                "members": [{
                    "member_kind": "new",
                    "criterion_id": "tests-pass",
                    "definition": {
                        "kind": "test_check",
                        "statement": statement,
                        "command": "cargo test"
                    }
                }]
            });
            let decision = evaluate_permission("propose_work_criteria", &proposal, &ctx);
            assert!(matches!(decision.decision, HardDecision::Allow));
            assert!(matches!(
                decision.source,
                DecisionSource::InternalOrchestration
            ));
        }
    }
}
