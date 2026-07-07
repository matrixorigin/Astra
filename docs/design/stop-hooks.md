# Stop hooks

> Status: target design contract.
> Last updated: 2026-07-07.

Stop hooks define controlled pause/stop/checkpoint behavior around model turns, tool calls, and terminal outputs. They are not a default hard-break mechanism.

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
