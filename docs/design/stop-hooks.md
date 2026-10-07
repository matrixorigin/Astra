# Stop hooks

> Status: target design contract.
> Last updated: 2026-07-07.

Stop hooks define controlled pause/stop/checkpoint behavior around model turns, tool calls, and terminal outputs. They are not a default hard-break mechanism.

The implemented declarative completion configuration is `.astra/stop-hooks.yaml`
or `.astra/stop-hooks.yml`. Its `when` field accepts `stop` (the default) or
`task_completed` for a plan subtask. Invalid configuration rejects execution
preparation with the file path and validation reason; it does not become an
empty verification contract. Declared checks execute through ordinary tool
admission and the selected provider, and require successful execution evidence.

For remote workspaces, CLI collects declarations at the user-local boundary and
passes `completion_checks` through HTTP or WebSocket admission. Both `stop` and
`task_completed` arrays are required. The declarations add verification
obligations; they grant no tool permission. Server-owned workspaces load their
configuration from the selected provider's resolved directory and reject a
competing client declaration source. Client `cwd` and `git_root` are never
Server filesystem authority.

Root admission freezes both phases and the selected completion boundary in the
existing `workspace_bound` record, after the execution claim and provider
materialization. The loop consumes those same prepared facts. Binding receipts
expose routing information to clients; the internal completion contract is not
part of the live or replayed binding event schema.

Declarations are bounded to 64 checks and 256 KiB. Dependencies must refer to
checks in the same phase, remain acyclic, and cannot make an authoritative
check depend on advisory guidance. There is no independent skip-on-pass hook
cache: the canonical verification frontier tracks actual invocation evidence
and invalidates it after recorded workspace mutations. Recovery restores the frozen
complete declarations and selected phase from Control V4, using the original
task profile; it does not reread configuration or reset validation evidence.

## Principle

```text
Stop hooks should preserve recoverability and explainability.
```

Use precise degraded or blocked states when possible. Hard stop is reserved for safety, data loss risk, or consistency boundaries.

## Hook points

| Hook | Purpose |
| --- | --- |
| pre-model | Validate context, provider state, budget, policy. |
| post-model | Validate tool calls, unsafe output, stop condition. |
| pre-tool | Validate provider, permission, side-effect, arguments. |
| post-tool | Validate result quality, redaction, retry/fallback. |
| pre-terminal | Validate final answer, unresolved blockers, artifact refs. |
| checkpoint | Persist recoverable state before risky boundary. |

## Outcomes

```text
continue
continue_with_warning
retry
fallback
ask_user
block_tool
pause_run
cancel_run
fail_run
```

`fail_run` should be rare and structured.

## Requirements

- Hook outcome must be traceable.
- Blocking one tool should not automatically stop the entire run.
- User-facing messages must include reason and next action.
- Hook state must survive resume when it changes run semantics.
- Stop hooks must not mutate permission or provider state outside normal contracts.

## Test obligations

- Unsafe tool call blocks tool, not whole run, when possible.
- Post-tool malformed output becomes degraded/blocked with trace.
- Pre-model missing provider reports provider state.
- Hard stop includes terminal reason and resumability.
