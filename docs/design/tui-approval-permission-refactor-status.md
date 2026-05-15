# TUI Approval / Permission Refactor Status

Last updated: 2026-05-14

This document tracks the implementation status for issue #326 after continuing
the Copilot session in
`/Users/ghs-mo/.copilot/session-state/4dfc6484-689d-40c1-a6ab-11ee73640b59`.
The corresponding Copilot process log is
`/Users/ghs-mo/.copilot/logs/process-1778722345361-2836.log`.

## Task Purpose

The refactor makes TUI the only interactive approval surface and removes drift
between permission decision paths. A tool call should go through the same
ordered checks regardless of whether it comes from the root TUI loop, cloud
approval, a sandbox retry, or a child agent/runtime mailbox.

The stable decision order is:

1. schema/request identity
2. deny rules
3. safety middleware
4. git safety
5. sensitive path
6. execute hard-deny
7. sandbox expansion
8. read-only short-circuit
9. session override
10. explicit approval
11. allow rules
12. permission mode

## Progress Snapshot

Copilot's session todo database reports `48 / 48` items complete, but the code
audit found real design drift after that point. This status file is the
repository-level source of truth for what is done versus still outstanding.

Implemented and verified in code:

- P1/P2 type consolidation contract: `PermissionMode` / `PermissionRule` are
  turn-core types, and `GateOutcome` is separated from sync decisions.
- P2 runtime Flow B now calls `astra_turn_core::permission_engine::evaluate_permission`
  instead of owning a second simplified decision tree.
- P2 root CLI/TUI Flow A now also records and consumes the shared
  `DecisionEnvelope`; the old `check_nonblocking_inner` decision order is no
  longer an independent permission policy.
- P2 hard checks are covered in turn-core tests: catastrophic shell commands
  beat Auto mode, explicit approval beats allow rules in Prompt mode, and
  sensitive-path writes still require external approval in Auto mode.
- Permission modes are intentionally limited to `auto`, `prompt`, and `deny`;
  the former extra auto-approve alias/mode has been removed.
- P3 backend scope semantics: local and cloud Always decisions map to Project,
  RestOfSession, RestOfTurn, or OnceThisCall according to risk and persistence
  safety.
- P3 TUI scope picker is now explicit: approval cards expose Turn, Session,
  Project, and User scope buttons, with unavailable scopes disabled through the
  shared `permitted_scopes` policy. Scope selection is only the first dimension:
  the TUI then asks for the match target (`Exact`, `This tool`, or
  `Custom prefix`) with an English explanation of the resulting approval.
  Custom prefix input only accepts characters that keep the value a real prefix
  of the current command/path. Turn approvals use a per-turn override cache
  that is cleared at the start of each SSE turn; User approvals persist to
  `~/.astra/permissions.json` via the same lock/reload/merge/save path as
  project rules. Project/User persistence uses the same match-target builder as
  Turn/Session matching, and session permission-audit events record both
  dimensions (`scope` and `match_target`).
- P4 approval queue batch safety: group resolution is exact; dangerous
  ungrouped "accept all" is disabled.
- P5b workspace trust is enforced at production entry points. Interactive TUI
  startup applies project allow rules only when the user trust ledger says the
  workspace is trusted and the `.kiro/permissions.json` hash still matches.
  Unknown, untrusted, corrupt-ledger, or changed-rule workspaces keep project
  deny rules but ignore project allow rules.
- P5d permission persistence uses lock/reload/merge/save semantics instead of
  blind overwrite.
- P1.5b grammar-v2 rule strings are validated at load time. Unknown v2 keys or
  malformed structured rules now surface as `InvalidRule` load errors instead
  of being silently interpreted as legacy patterns.
- P1.5b/P5 grammar-v2 enforcement is wired into the shared matcher:
  `PermissionRule` carries `op`, `cwd_root`, `git_branch`, `domain`, and
  `capability` constraints, and `RuleMatchContext` derives the current command,
  path, operation kind, cwd, git branch, URL domain, and capability hints from
  tool arguments. Root CLI checks, inherited/runtime checks, and the shared
  permission engine all use the same context-aware matcher. Missing context
  fails closed by not matching a scoped rule.
- P5f stale revalidation blocks approved file mutations if the target changed
  between approval enqueue and execution.
- P6 local trace surface exists through `/allow trace` in TUI and
  `astra permissions trace` in CLI; redacted JSONL export is available through
  `/allow trace --export <path>` and `astra permissions trace --export <path>`.
  The same evaluated/resolved/persisted audit events are also appended to the
  durable session journal as `permission_audit` records, so
  `~/.astra/sessions/<session>.jsonl` can reconstruct the permission chain after
  a run.

## Remaining Boundaries

These are intentional compatibility boundaries rather than hidden fallbacks:

- Legacy v1 rules remain accepted for compatibility. The v2 parser handles
  structured rules first; old `Tool(pattern:*)` strings continue to work through
  the same `PermissionRule` type.
- Capability-scoped MCP rules only match when the tool call exposes capability
  metadata in its arguments or metadata-derived hints. If that context is
  absent, the rule does not match.
- Unsupported persistence failures are surfaced as save errors and downgraded
  to session-only for the current approval, matching the existing project-rule
  failure path.

## Verification Run

Focused verification performed after the latest changes:

- `cargo test -p astra-turn-core permission_engine --lib`
- `cargo test -p astra-turn-core permission_types --lib -- --nocapture`
- `cargo test -p astra-turn-core permission_rule_grammar --lib -- --nocapture`
- `cargo test -p astra-turn-core permission_audit --lib`
- `cargo test -p astra-turn-core permission_audit --lib -- --nocapture`
- `cargo test -p astra-runtime permission_gate --lib -- --nocapture`
- `cargo test -p astra-turn-core normalized_argv_prefix_stops_at_flags_and_shell_meta --lib`
- `cargo test -p astra-cli --bin astra make_allow_rule`
- `cargo test -p astra-cli --bin astra permissions_trace_renders_trace_arg`
- `cargo test -p astra-cli --bin astra permissions_trust_commands_render_args`
- `cargo test -p astra-cli --bin astra workspace_trust`
- `cargo test -p astra-cli --bin astra permission_manager::tests`
- `cargo test -p astra-cli --bin astra permission_manager::tests::allow_rules_enforce_v2 -- --nocapture`
- `cargo test -p astra-cli --bin astra stream_render::tests::approval_ -- --nocapture`
- `cargo test -p astra-cli --bin astra approval_memory_action_maps_always_scope_to_storage_effect`
- `cargo test -p astra-cli --bin astra button_row`
- `cargo test -p astra-cli --bin astra history_cell::approval::tests`
- `cargo check -p astra-cli --bin astra`
- `git diff --check`
- `find . -name '*.snap.new' -print`
