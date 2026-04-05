---
name: remember
description: "Review, organize, and promote working knowledge into persistent project memory across multiple layers"
version: "1.1.0"
triggers:
  - remember
  - "save context"
  - "what do you know"
  - "update memory"
  - memory
when_to_use: "When the user wants to review, organize, or persist knowledge learned during the session — conventions, decisions, gotchas — into project documentation or memory files"
category: meta
arguments:
  - name: CONTEXT
    description: "Additional context about what to remember or review"
    required: false
tags:
  - memory
  - context
  - documentation
---
# Memory Review and Organization

Review what you've learned during this session and propose organizing it into persistent memory.

## Additional Context

$ARGUMENTS

## Process

### 1. Gather All Memory Layers

Scan for existing memory/context files:
- **Project conventions**: `CLAUDE.md`, `AGENTS.md`, `RULES.md` in the project root
- **Local overrides**: `CLAUDE.local.md`, `.astra/config/`, `.astra/memory/`
- **Cursor rules**: `.cursor/rules/` (if applicable)
- **Project docs**: `README.md`, `CONTRIBUTING.md`

Read any that exist. Note which layers are missing. Your session memory and auto-memory content is already in your system prompt — review it there.

**Success criteria**: You have the contents of all memory layers and can compare them.

### 2. Classify Session Knowledge

Review the conversation and identify knowledge worth persisting. For each item, determine the best destination:

| Destination | What belongs there | Examples |
|---|---|---|
| **Project docs** (CLAUDE.md/AGENTS.md) | Conventions all contributors should follow | "use `cargo test` not `make test`", "API routes use kebab-case", "errors return JSON with `code` field" |
| **Local overrides** (CLAUDE.local.md) | Personal instructions for this user, not applicable to other contributors | "prefer concise responses", "always explain trade-offs", "run tests before committing" |
| **Stay in session** | Temporary context that won't matter next session | One-off debugging observations, transient environment details |

**Important distinctions:**
- Project docs contain instructions for the agent, not user preferences for external tools
- Workflow practices (PR conventions, merge strategies, branch naming) are ambiguous — ask the user whether they're personal or team-wide
- When unsure, ask rather than guess

**Success criteria**: Each entry has a proposed destination or is flagged as ambiguous.

### 3. Identify Cleanup Opportunities

Scan across all layers for:
- **Duplicates**: Session memory entries already captured in project docs → propose removing the duplicate
- **Outdated**: Project doc entries contradicted by newer session observations → propose updating the older layer
- **Conflicts**: Contradictions between any two layers → propose resolution, noting which is more recent

**Success criteria**: All cross-layer issues identified.

### 4. Present the Report

Output a structured report grouped by action type:
1. **Promotions** — entries to move into project docs or local config, with destination and rationale
2. **Cleanup** — duplicates, outdated entries, conflicts to resolve
3. **Ambiguous** — entries where you need user input on destination
4. **No action needed** — brief note on entries that should stay put

If session memory is empty, say so and offer to review existing project docs for cleanup.

**Success criteria**: User can review and approve/reject each proposal individually.

### 5. Apply (with confirmation)

Present ALL proposals before making any changes. Wait for explicit user approval before modifying any files.

When applying:
- Append to existing sections when possible (don't rewrite entire files)
- Create new files only if the target doesn't exist
- Remove promoted entries from session memory after successful write

## Rules
- Present proposals before applying — never silently modify memory files
- Ask about ambiguous entries — don't guess
- Prefer appending to existing sections over creating new files
- Keep entries concise and actionable
- Workflow practices are ambiguous by default — ask the user
