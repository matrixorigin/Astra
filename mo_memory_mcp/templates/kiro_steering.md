---
inclusion: always
---

<!-- trustmem-version: 0.2.4 -->

# Memory Integration (TrustMem Lite)

You have access to a shared memory service via MCP tools (TrustMem Lite — local single-user mode). Use it proactively:

## 🔴 MANDATORY: Start of every conversation
**ALWAYS call `memory_retrieve` with the user's first message before responding.**
This is not optional. Without this, you have no memory of past interactions.
If the response includes ⚠️ Memory health warnings, proactively inform the user and offer to help clean up.

## 🔴 MANDATORY: End of every conversation turn
After each response, check if the turn contained anything worth remembering:
- User stated a preference, fact, constraint, or decision → `memory_store`
- User corrected something you said → `memory_store` with the correction
- You learned something new about the user's project/workflow → `memory_store`

Do NOT store: greetings, trivial questions, things already in memory.

## Other triggers
- User says a stored memory is wrong → `memory_correct`
- User asks to forget something → `memory_purge` (single ID or by topic)
- User says "forget everything about X" → `memory_purge` with `topic="X"` to bulk-delete
- User asks "what do you know about me" → `memory_profile`

## Memory types
- `profile`: user/agent profiles
- `semantic`: project facts, technical decisions, architecture choices (default)
- `procedural`: how-to knowledge, workflows, processes
- `working`: temporary context for current task
- `tool_result`: results from tool executions

## Snapshots & Rollback
Use snapshots to save memory state before risky changes:
- Before major refactors or architecture changes → `memory_snapshot(name="before_refactor")`
- User asks to "take a snapshot" or "save memory state" → `memory_snapshot`
- User asks to "roll back" or "restore" → `memory_rollback`
- User asks to "show snapshots" → `memory_snapshots`

## Branches
Use branches for isolated experimentation with memory states (like git branches):
- User wants to evaluate an alternative approach → `memory_branch(name="eval_xyz")` then `memory_checkout`
- User asks "what would change if we merge" → `memory_diff`
- User decides to keep changes → `memory_merge(source="branch_name", strategy="replace")`
- User decides to discard → `memory_branch_delete`
- User asks to "show branches" → `memory_branches`
- User asks to "switch to main" → `memory_checkout(name="main")`

## Maintenance (only when user asks)
- User asks to "clean up memories" → `memory_governance` (1h cooldown)
- User asks to "check for contradictions" → `memory_consolidate` (30min cooldown)
- User asks to "find patterns" or "reflect" → `memory_reflect` (2h cooldown, requires LLM)
- Governance reports "needs_rebuild" → `memory_rebuild_index`
