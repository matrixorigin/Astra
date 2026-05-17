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
//! The runtime/sub-agent gate now calls [`evaluate_permission`] so Flow B no
//! longer owns a second simplified decision chain. The CLI gate still has
//! some UI-specific state (trusted sandbox roots, denial throttling, project
//! persistence), so it is being migrated in stages; new pure checks should be
//! added here first and consumed by call sites instead of being copied.

use astra_sandbox::{is_dangerous_file_path, is_soft_violation, validate_git_command};
use serde_json::Value;

use crate::action_compensation::{explicit_approval_reason, primary_approval_reason};
use crate::approval_fingerprint::{ApprovalFingerprint, FingerprintedOverrides};
use crate::cloud_approval_policy::{
    CloudGatedToolKind, cloud_gated_tool_kind, cloud_gated_tool_kind_with_args,
};
use crate::parallel_tool_exec::is_read_only_tool_with_args;
use crate::permission_match_target::{
    AllowMatchTarget, allow_rule_for_match_target, default_match_target,
};
use crate::permission_memory_profile::resolved_write_path;
use crate::permission_types::{PermissionMode, PermissionSyncContext};
use crate::safety_middleware::{SafetyMiddlewareDecision, evaluate_tool_safety_request};
use crate::tool_argument_hints::{
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
    AskRules,
    ReadShortCircuit,
    SessionOverride,
    ExplicitApproval,
    AllowRules,
    Mode,
}

/// Canonical ordering. Any reorder must change this constant AND
/// flip the pinning test in `tests::evaluation_order_is_stable`.
pub const EVALUATION_ORDER: [EvaluationStep; 13] = [
    EvaluationStep::SchemaIdentity,
    EvaluationStep::DenyRules,
    EvaluationStep::SafetyMiddleware,
    EvaluationStep::GitSafety,
    EvaluationStep::SensitivePath,
    EvaluationStep::ExecuteHardDeny,
    EvaluationStep::SandboxExpand,
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
    /// Workspace not in the trust ledger; persistent rules from
    /// `.kiro/permissions.json` are downgraded to allow-only-once.
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

fn legacy_plan_mode_tool_alias(tool_name: &str, args: &Value) -> Option<&'static str> {
    let action = args.get("action").and_then(Value::as_str).map(str::trim);
    match tool_name {
        "session" => match action {
            Some("enter_plan_mode") => Some("enter_plan_mode"),
            Some("exit_plan_mode") => Some("exit_plan_mode"),
            _ => None,
        },
        "agent" if action == Some("run_chain") => match args.get("chain").and_then(Value::as_str) {
            Some("enter_plan_mode") => Some("enter_plan_mode"),
            Some("exit_plan_mode") => Some("exit_plan_mode"),
            _ => None,
        },
        _ => None,
    }
}

pub(crate) fn plan_mode_denial_reason(tool_name: &str, args: &Value) -> String {
    if let Some(canonical_tool) = legacy_plan_mode_tool_alias(tool_name, args) {
        return format!(
            "Tool '{tool_name}' denied by permission mode. Use `{canonical_tool}` directly — plan mode no longer routes through `{tool_name}`."
        );
    }
    format!(
        "Tool '{tool_name}' denied by permission mode. Plan mode allows read-only tools plus `enter_plan_mode` / `exit_plan_mode`; author the plan, then call `exit_plan_mode(plan=...)`."
    )
}

/// Evaluate a tool call against the synchronized parent/child permission
/// context.
///
/// This is the shared pure evaluator for runtime/sub-agent permission checks:
/// it runs the same bypass-immune guards before any relaxable rule or mode
/// branch, and returns `NeedExternal` instead of deciding how to ask the user.
/// Callers are responsible for routing that prompt to a TUI sink, parent
/// mailbox, or headless fail-closed path.
#[must_use]
pub fn evaluate_permission(
    tool_name: &str,
    args: &Value,
    ctx: &PermissionSyncContext,
) -> DecisionEnvelope {
    let mut trace = Vec::with_capacity(EVALUATION_ORDER.len());
    let risk_tags = risk_tags_for_request(tool_name, args);
    let will_save = Some(allow_rule_preview(tool_name, args));
    let rule_match_context =
        crate::permission_types::RuleMatchContext::from_tool_args(tool_name, args);

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

    if is_execute_tool(tool_name, args) {
        let git_violations = command_hint_from_args(args)
            .map(validate_git_command)
            .unwrap_or_default();
        if !git_violations.is_empty() {
            let reasons: Vec<String> = git_violations.iter().map(ToString::to_string).collect();
            let all_soft = git_violations.iter().all(is_soft_violation);
            if ctx.mode() == PermissionMode::Deny {
                let decision = HardDecision::Deny {
                    reason: "Git safety violation (deny mode)".to_string(),
                };
                push_matched(
                    &mut trace,
                    EvaluationStep::GitSafety,
                    &decision,
                    &reasons.join(", "),
                );
                return envelope(
                    decision,
                    DecisionSource::GitSafety {
                        violation: reasons.join(", "),
                    },
                    trace,
                    will_save,
                    risk_tags,
                );
            }
            if all_soft && ctx.mode() == PermissionMode::Auto {
                let decision = HardDecision::Allow;
                push_matched(
                    &mut trace,
                    EvaluationStep::GitSafety,
                    &decision,
                    &reasons.join(", "),
                );
                return envelope(
                    decision,
                    DecisionSource::GitSafety {
                        violation: reasons.join(", "),
                    },
                    trace,
                    will_save,
                    risk_tags,
                );
            }
            let reason = format!("Git safety: {}", reasons.join(", "));
            let decision = HardDecision::NeedExternal {
                prompt: approval_prompt(tool_name, args, reason.clone(), risk_tags.clone()),
            };
            push_matched(&mut trace, EvaluationStep::GitSafety, &decision, &reason);
            return envelope(
                decision,
                DecisionSource::GitSafety { violation: reason },
                trace,
                will_save,
                risk_tags,
            );
        }
    }
    push_skipped(&mut trace, EvaluationStep::GitSafety, "no git violation");

    if let Some(path) = sensitive_path_match(args) {
        if ctx.mode() == PermissionMode::Deny {
            let decision = HardDecision::Deny {
                reason: "Sensitive path (deny mode)".to_string(),
            };
            push_matched(&mut trace, EvaluationStep::SensitivePath, &decision, &path);
            return envelope(
                decision,
                DecisionSource::SensitivePath { path },
                trace,
                will_save,
                risk_tags,
            );
        }
        let reason = "Targets a sensitive file path and requires manual approval".to_string();
        let decision = HardDecision::NeedExternal {
            prompt: approval_prompt(tool_name, args, reason.clone(), risk_tags.clone()),
        };
        push_matched(&mut trace, EvaluationStep::SensitivePath, &decision, &path);
        return envelope(
            decision,
            DecisionSource::SensitivePath { path },
            trace,
            will_save,
            risk_tags,
        );
    }
    push_skipped(
        &mut trace,
        EvaluationStep::SensitivePath,
        "no sensitive path",
    );

    if let Some(reason) = execute_hard_deny_reason(tool_name, args) {
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

    if let Some(inner_tool) = tool_name.strip_prefix("sandbox_expand:") {
        return match ctx.mode() {
            PermissionMode::Auto => {
                let decision = HardDecision::Allow;
                push_matched(
                    &mut trace,
                    EvaluationStep::SandboxExpand,
                    &decision,
                    "sandbox expansion allowed by mode",
                );
                envelope(
                    decision,
                    DecisionSource::SandboxExpansion,
                    trace,
                    will_save,
                    risk_tags,
                )
            }
            PermissionMode::Plan => {
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
            PermissionMode::AcceptEdits | PermissionMode::Prompt => {
                let reason = args
                    .get("reason")
                    .and_then(Value::as_str)
                    .unwrap_or("Access to path outside project boundary")
                    .to_string();
                let decision = HardDecision::NeedExternal {
                    prompt: ApprovalPrompt {
                        tool: tool_name.to_string(),
                        header: format!("{inner_tool} wants to read outside the project"),
                        detail: None,
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
            PermissionMode::Deny => {
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

    if let Some(rule) = ctx
        .inherited
        .ask_rule_with_context(tool_name, &rule_match_context)
    {
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
    push_skipped(&mut trace, EvaluationStep::AskRules, "no ask rule matched");

    if explicit_approval_reason(tool_name, args).is_none()
        && is_read_only_tool_with_args(tool_name, Some(args))
        && ctx.mode() != PermissionMode::Deny
        && ctx.inherited.is_tool_allowed_by_allowlist(tool_name)
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
        // Plan-control tools (`enter_plan_mode` / `exit_plan_mode`)
        // must remain callable in plan mode — they are the model's
        // only escape hatch (the `tool_schema_prune` module
        // deliberately keeps them in the visible schema for the same
        // reason). Without this exemption schema and runtime
        // disagree: the model sees `exit_plan_mode`, calls it to
        // surface its plan, runtime denies it as "denied by permission
        // mode", and the agent is stuck in plan mode forever.
        // Regression: session 4cb6b459.
        if crate::tool_schema_prune::PLAN_MODE_REQUIRED_TOOLS.contains(&tool_name) {
            let decision = HardDecision::Allow;
            push_matched(
                &mut trace,
                EvaluationStep::Mode,
                &decision,
                "plan-control tool — exempt from plan-mode deny",
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
            reason: plan_mode_denial_reason(tool_name, args),
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

    if let Some(policy_reason) = explicit_approval_reason(tool_name, args) {
        let prompt_reason =
            primary_approval_reason(tool_name, args).unwrap_or_else(|| policy_reason.clone());
        return match ctx.mode() {
            PermissionMode::Plan => {
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
            PermissionMode::Deny => {
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
            PermissionMode::Auto => {
                let decision = if ctx.inherited.is_tool_allowed_by_allowlist(tool_name) {
                    HardDecision::Allow
                } else {
                    HardDecision::Deny {
                        reason: format!("Tool '{tool_name}' not in allowed tools list"),
                    }
                };
                push_matched(
                    &mut trace,
                    EvaluationStep::ExplicitApproval,
                    &decision,
                    "explicit approval relaxed by mode",
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
            PermissionMode::AcceptEdits => {
                let decision = if ctx.inherited.allowed_tools.is_some()
                    && !ctx.inherited.is_tool_allowed_by_allowlist(tool_name)
                {
                    HardDecision::Deny {
                        reason: format!("Tool '{tool_name}' not in allowed tools list"),
                    }
                } else {
                    HardDecision::NeedExternal {
                        prompt: approval_prompt(tool_name, args, prompt_reason, risk_tags.clone()),
                    }
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
            PermissionMode::Prompt => {
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

    let mode = ctx.mode();
    let (decision, mode_label) = match mode {
        PermissionMode::Auto => {
            if !ctx.inherited.is_tool_allowed_by_allowlist(tool_name) {
                (
                    HardDecision::Deny {
                        reason: format!("Tool '{tool_name}' not in allowed tools list"),
                    },
                    mode.to_string(),
                )
            } else {
                (HardDecision::Allow, mode.to_string())
            }
        }
        PermissionMode::Plan => (
            HardDecision::Deny {
                reason: format!("Tool '{tool_name}' denied by permission mode"),
            },
            mode.to_string(),
        ),
        PermissionMode::AcceptEdits if ctx.inherited.allowed_tools.is_some() => {
            if !ctx.inherited.is_tool_allowed_by_allowlist(tool_name) {
                (
                    HardDecision::Deny {
                        reason: format!("Tool '{tool_name}' not in allowed tools list"),
                    },
                    "agent policy allowlist".to_string(),
                )
            } else if accept_edits_auto_allows(tool_name, args) {
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
        PermissionMode::AcceptEdits => {
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
        PermissionMode::Deny => (
            HardDecision::Deny {
                reason: format!("Tool '{tool_name}' denied by permission mode"),
            },
            mode.to_string(),
        ),
        PermissionMode::Prompt if ctx.inherited.allowed_tools.is_some() => {
            if ctx.inherited.is_tool_allowed_by_allowlist(tool_name) {
                (HardDecision::Allow, "agent policy allowlist".to_string())
            } else {
                (
                    HardDecision::Deny {
                        reason: format!("Tool '{tool_name}' not in allowed tools list"),
                    },
                    "agent policy allowlist".to_string(),
                )
            }
        }
        PermissionMode::Prompt => (
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

fn sensitive_path_match(args: &Value) -> Option<String> {
    if let Some(path) = path_hint_from_args(args)
        && !path.is_empty()
        && (is_dangerous_file_path(&path)
            || crate::permission_redact::matches_sensitive_path(&path))
    {
        return Some(path);
    }
    if let Some(cmd) = command_hint_from_args(args)
        && !cmd.is_empty()
        && (is_dangerous_file_path(cmd) || crate::permission_redact::matches_sensitive_path(cmd))
    {
        return Some(cmd.to_string());
    }
    None
}

fn execute_hard_deny_reason(tool_name: &str, args: &Value) -> Option<String> {
    if !is_execute_tool(tool_name, args) {
        return None;
    }
    let command = command_hint_from_args(args)?;
    let lower = command.to_ascii_lowercase();
    if ["rm -rf /", "rm -fr /", ":(){ :|:& };:", "chmod 777 /"]
        .iter()
        .any(|p| lower.contains(p))
    {
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
    let json = ctx.inherited.fingerprinted_overrides.as_ref()?;
    let overrides = serde_json::from_value::<FingerprintedOverrides>(json.clone()).ok()?;
    content_aware_fingerprint_candidates(tool_name, args)
        .into_iter()
        .find_map(|fp| overrides.check(&fp))
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
                |path| ApprovalFingerprint::file_op_exact(tool_name, Some(&path)),
            )
        }
        None => ApprovalFingerprint::bare(tool_name),
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
        if crate::tool_categories::registry().is_file_op(tool_name) {
            push_unique_fingerprint(
                &mut candidates,
                ApprovalFingerprint::file_op_exact("edit", Some(&path)),
            );
        }
        if let Some(resolved) = resolved_write_path(&path) {
            push_unique_fingerprint(
                &mut candidates,
                ApprovalFingerprint::file_op_exact(tool_name, Some(&resolved)),
            );
            if crate::tool_categories::registry().is_file_op(tool_name) {
                push_unique_fingerprint(
                    &mut candidates,
                    ApprovalFingerprint::file_op_exact("edit", Some(&resolved)),
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

fn risk_tags_for_request(tool_name: &str, args: &Value) -> Vec<RiskTag> {
    let mut tags = Vec::new();
    if tool_name.starts_with("sandbox_expand:") {
        push_risk_tag(&mut tags, RiskTag::SandboxExpansion);
    }
    if tool_name.starts_with("mcp_") {
        push_risk_tag(&mut tags, RiskTag::MCPUnknownCapability);
    }
    match cloud_gated_tool_kind_with_args(tool_name, Some(args)) {
        Some(CloudGatedToolKind::Execute) => {
            push_risk_tag(&mut tags, RiskTag::BashExecute);
            if command_hint_from_args(args)
                .map(validate_git_command)
                .is_some_and(|violations| !violations.is_empty())
            {
                push_risk_tag(&mut tags, RiskTag::GitDestructive);
            }
        }
        Some(CloudGatedToolKind::Write) if sensitive_path_match(args).is_some() => {
            push_risk_tag(&mut tags, RiskTag::WritesSensitiveFile);
        }
        Some(CloudGatedToolKind::Write) => {}
        None => {}
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
            format!(r#"Edit(path_prefix="{cwd}", op="write", cwd_root="{cwd}")"#)
        );
    }

    #[test]
    fn allow_rule_preview_keeps_exact_scope_for_workspace_external_writes() {
        let rule = allow_rule_preview(
            "write_file",
            &serde_json::json!({"path": "/tmp/zzzz3.md", "content": "# zzzz3"}),
        );

        assert_eq!(rule, r#"Edit(path_glob="/tmp/zzzz3.md", op="write")"#);
    }

    #[test]
    fn auto_mode_allowlist_blocks_unlisted_tool_after_read_short_circuit() {
        let ctx = crate::permission_types::PermissionSyncContext::new(
            crate::permission_types::InheritedPermissions {
                mode: crate::permission_types::PermissionMode::Auto,
                allowed_tools: Some(std::collections::HashSet::from(["view".to_string()])),
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
    }

    #[test]
    fn prompt_mode_allowlist_locally_allows_listed_non_explicit_tool() {
        let ctx = crate::permission_types::PermissionSyncContext::new(
            crate::permission_types::InheritedPermissions {
                mode: crate::permission_types::PermissionMode::Prompt,
                allowed_tools: Some(std::collections::HashSet::from(["write_file".to_string()])),
                ..Default::default()
            },
        );
        let envelope = evaluate_permission(
            "write_file",
            &serde_json::json!({"path": "src/lib.rs", "content": "pub fn x() {}\n"}),
            &ctx,
        );

        assert!(matches!(envelope.decision, HardDecision::Allow));
        assert_eq!(
            envelope.source,
            DecisionSource::Mode {
                mode: "agent policy allowlist".to_string()
            }
        );
    }

    #[test]
    fn prompt_mode_allowlist_blocks_unlisted_tool() {
        let ctx = crate::permission_types::PermissionSyncContext::new(
            crate::permission_types::InheritedPermissions {
                mode: crate::permission_types::PermissionMode::Prompt,
                allowed_tools: Some(std::collections::HashSet::from(["view".to_string()])),
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
    fn plan_mode_allows_read_only_tools_but_denies_mutations() {
        let ctx = crate::permission_types::PermissionSyncContext::new(
            crate::permission_types::InheritedPermissions {
                mode: crate::permission_types::PermissionMode::Plan,
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
        let ctx = crate::permission_types::PermissionSyncContext::new(
            crate::permission_types::InheritedPermissions {
                mode: crate::permission_types::PermissionMode::Plan,
                ..Default::default()
            },
        );
        for tool in crate::tool_schema_prune::PLAN_MODE_REQUIRED_TOOLS {
            let envelope = evaluate_permission(tool, &serde_json::json!({}), &ctx);
            assert!(
                matches!(envelope.decision, HardDecision::Allow),
                "plan mode must allow `{tool}` so the agent can leave plan mode; got {:?}",
                envelope.decision
            );
        }
    }

    #[test]
    fn plan_mode_denial_reason_guides_legacy_plan_tool_aliases() {
        let session_reason =
            plan_mode_denial_reason("session", &serde_json::json!({"action": "exit_plan_mode"}));
        assert!(session_reason.contains("Use `exit_plan_mode` directly"));
        assert!(session_reason.contains("no longer routes through `session`"));

        let agent_reason = plan_mode_denial_reason(
            "agent",
            &serde_json::json!({"action": "run_chain", "chain": "exit_plan_mode"}),
        );
        assert!(agent_reason.contains("Use `exit_plan_mode` directly"));
        assert!(agent_reason.contains("no longer routes through `agent`"));
    }

    #[test]
    fn generic_plan_mode_denial_reason_points_to_exit_tool() {
        let reason =
            plan_mode_denial_reason("bash", &serde_json::json!({"command": "touch plan.txt"}));
        assert!(reason.contains("Plan mode allows read-only tools"));
        assert!(reason.contains("exit_plan_mode(plan=...)"));
    }

    #[test]
    fn deny_mode_overrides_read_short_circuit_allowlist() {
        let ctx = crate::permission_types::PermissionSyncContext::new(
            crate::permission_types::InheritedPermissions {
                mode: crate::permission_types::PermissionMode::Deny,
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
        let ctx = crate::permission_types::PermissionSyncContext::new(
            crate::permission_types::InheritedPermissions {
                mode: crate::permission_types::PermissionMode::Prompt,
                ask_rules: vec![crate::permission_types::PermissionRule::parse("bash(*)")],
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
        let ctx = crate::permission_types::PermissionSyncContext::root(
            crate::permission_types::PermissionMode::Auto,
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
    fn evaluate_explicit_approval_precedes_allow_rule_in_prompt_mode() {
        let ctx = crate::permission_types::PermissionSyncContext::new(
            crate::permission_types::InheritedPermissions {
                mode: crate::permission_types::PermissionMode::Prompt,
                allow_rules: vec![crate::permission_types::PermissionRule::parse("git_commit")],
                ..Default::default()
            },
        );
        let envelope = evaluate_permission(
            "git_commit",
            &serde_json::json!({"message": "ship it"}),
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
    fn evaluate_sensitive_path_needs_external_even_in_auto_mode() {
        let ctx = crate::permission_types::PermissionSyncContext::root(
            crate::permission_types::PermissionMode::Auto,
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
    fn accept_edits_mode_allows_workspace_write_tools() {
        let ctx = crate::permission_types::PermissionSyncContext::root(
            crate::permission_types::PermissionMode::AcceptEdits,
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
        let ctx = crate::permission_types::PermissionSyncContext::root(
            crate::permission_types::PermissionMode::AcceptEdits,
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
        let ctx = crate::permission_types::PermissionSyncContext::root(
            crate::permission_types::PermissionMode::AcceptEdits,
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
        let ctx = crate::permission_types::PermissionSyncContext::new(
            crate::permission_types::InheritedPermissions {
                mode: crate::permission_types::PermissionMode::AcceptEdits,
                allowed_tools: Some(std::collections::HashSet::from(["write_file".to_string()])),
                ..Default::default()
            },
        );
        let envelope =
            evaluate_permission("bash", &serde_json::json!({"command": "cargo test"}), &ctx);

        assert!(matches!(envelope.decision, HardDecision::Deny { .. }));
        assert!(matches!(
            envelope.source,
            DecisionSource::ExplicitApprovalGate { .. }
        ));
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
}
