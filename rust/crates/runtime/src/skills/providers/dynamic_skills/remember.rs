pub fn skill_content() -> String {
    format!(
        r#"---
name: remember
description: "Review and manage persistent memories — promote, clean up, and organize knowledge across sessions"
version: "1.0.0"
allowed_tools:
  - memoria-memory_list
  - memoria-memory_profile
  - memoria-memory_purge
  - memoria-memory_correct
  - memoria-memory_search
  - memoria-memory_store
  - memoria-memory_governance
triggers:
  - remember
  - memories
  - "clean up memories"
when_to_use: "When the user explicitly wants to review stored memories, clean up stale knowledge, or organize what the agent remembers across sessions"
category: meta
tags:
  - memory
  - knowledge-management
---
# Remember: Memory Management

You help the user review, organize, and maintain persistent memories stored via Memoria.

**Session**: ${{CTX_SESSION_ID}}
**Available tools**: ${{CTX_AVAILABLE_TOOLS}}

## Step 1: Retrieve Current State

**Success criteria**: Full picture of current memory state.

Use the Memoria MCP tools to gather the current state:
- `memoria-memory_list` — list active memories (most recent first)
- `memoria-memory_profile` — check usage patterns and profile summary
- `memoria-memory_search` with broad query — find memories that might be stale or contradictory

If Memoria tools are not available, tell the user and stop.

## Step 2: Categorize Memories

**Success criteria**: Every memory assigned to exactly one category.

Group memories into:

| Category | Description | Action |
|----------|-------------|--------|
| **Active** | Currently relevant, frequently retrieved | Keep |
| **Stale** | About completed projects, old decisions, outdated facts | Candidate for cleanup |
| **Contradictory** | Two memories that disagree | Resolve — keep the correct one |
| **Redundant** | Duplicates or near-duplicates | Merge into one |
| **Vague** | Too broad to be useful ("use good practices") | Sharpen or remove |

Present the categorized list to the user.

## Step 3: Propose Actions

**Success criteria**: Specific, actionable proposal for each non-Active memory.

For each category, suggest specific actions:

- **Stale**: "These 3 memories are about the old auth system which was replaced. Remove?"
- **Contradictory**: "Memory A says X, Memory B says Y. Which is correct?"
- **Redundant**: "These 2 memories both say the same thing about testing. Merge into: [proposed merged text]?"
- **Vague**: "This memory is too broad to be actionable. Sharpen to: [proposed text]? Or remove?"

## Step 4: Execute with Confirmation

**Success criteria**: All approved changes applied; summary provided.

For each proposed action, get user confirmation before executing:
- **Remove**: Use `memoria-memory_purge` with the memory ID
- **Update**: Use `memoria-memory_correct` with new text and reason
- **Store new**: Use `memoria-memory_store` for merged or sharpened memories
- **Keep**: No action needed

After all actions, summarize what changed:
- Memories removed: N
- Memories updated: N
- Memories kept: N
- Total active: N

## Rules
- NEVER delete memories without explicit user confirmation
- When merging, preserve the most specific and actionable version
- If the user says "clean up everything", still present the plan before executing
"#,
    )
}
