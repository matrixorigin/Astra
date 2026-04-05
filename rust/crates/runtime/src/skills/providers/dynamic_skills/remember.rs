pub fn skill_content() -> String {
    format!(
        r#"---
name: remember
description: "Review and manage persistent memories — promote, clean up, and organize knowledge across sessions"
version: "2.0.0"
triggers:
  - remember
  - memories
  - "what do you know"
  - "clean up memories"
  - "memory hygiene"
when_to_use: "When the user wants to review what's been remembered across sessions, clean up stale memories, or promote working knowledge to long-term storage"
category: meta
tags:
  - memory
  - knowledge-management
---
# Remember: Memory Management

You help the user review, organize, and maintain persistent memories stored via Memoria.

## Step 1: Retrieve Current State

Use the Memoria memory tools to gather the current state:
- List active memories (most recent first)
- Check memory profile for usage patterns
- Note any memories flagged as low-confidence or contradictory

If Memoria tools are not available, tell the user and stop.

## Step 2: Categorize Memories

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

For each category, suggest specific actions:

- **Stale**: "These 3 memories are about the old auth system which was replaced. Remove?"
- **Contradictory**: "Memory A says X, Memory B says Y. Which is correct?"
- **Redundant**: "These 2 memories both say the same thing about testing. Merge into: [proposed merged text]?"
- **Vague**: "This memory is too broad to be actionable. Sharpen to: [proposed text]? Or remove?"

## Step 4: Execute with Confirmation

For each proposed action, get user confirmation before executing:
- **Remove**: Use memory purge
- **Update**: Use memory correct with the new text
- **Keep**: No action needed

After all actions, summarize what changed:
- Memories removed: N
- Memories updated: N
- Memories kept: N
- Total active: N

## When to Proactively Suggest

If you notice during normal work that:
- A memory was retrieved but was wrong or outdated → suggest correction
- The same fact is stored multiple times → suggest dedup
- A memory contradicts what you just learned → flag it

## Rules
- NEVER delete memories without explicit user confirmation
- When merging, preserve the most specific and actionable version
- If the user says "clean up everything", still present the plan before executing
"#,
    )
}
