---
name: skillify
description: "Capture this session's repeatable process into a reusable SKILL.md skill file"
version: "1.1.0"
triggers:
  - skillify
  - "make a skill"
  - "save as skill"
  - "create skill"
  - "capture workflow"
when_to_use: "When the user wants to capture a useful workflow, prompt pattern, or set of instructions as a reusable skill"
category: meta
arguments:
  - name: DESCRIPTION
    description: "Description of the process to capture as a skill"
    required: false
tags:
  - meta
  - skill-creation
  - automation
---
# Skillify — Capture Workflows as Reusable Skills

You are capturing this session's repeatable process as a reusable skill.

## Context

$ARGUMENTS

## Step 1: Analyze the Session

Before asking any questions, analyze the conversation to identify:
- What repeatable process was performed
- What the inputs/parameters were
- The distinct steps (in order)
- The success artifacts/criteria for each step (e.g., not just "writing code" but "an open PR with CI passing")
- Where the user corrected or steered you
- What tools and permissions were needed
- What agents or sub-agents were used
- What the goals and success criteria were

## Step 2: Interview the User

Ask focused questions to clarify. Use one round per topic. Important:
- For each round, iterate until the user is satisfied before moving on.
- The user always has freeform input available — don't add your own "Other" or "Needs tweaking" choice.

**Round 1 — High-level confirmation:**
- Suggest a name and description for the skill based on your analysis. Ask the user to confirm or rename.
- Suggest high-level goal(s) and specific success criteria.

**Round 2 — Structure:**
- Present the high-level steps you identified as a numbered list.
- If you think the skill will require arguments, suggest arguments based on what you observed. Make sure you understand what someone would need to provide.
- If it's not clear, ask if this skill should run inline (in conversation) or fork (as a sub-agent). Fork is better for self-contained tasks that don't need mid-process user input; inline for user-steered processes.
- Ask where the skill should be saved:
  - **This repo** (`.astra/skills/<name>/SKILL.md`) — for workflows specific to this project
  - **Personal** (`~/.astra/skills/<name>/SKILL.md`) — follows you across all repos

**Round 3 — Step details (for non-trivial skills):**
For each major step, if not glaringly obvious, ask:
- What does this step produce that later steps need? (data, artifacts, IDs)
- What proves this step succeeded, and that we can move on?
- Should the user be asked to confirm before proceeding? (especially irreversible actions like merging, deploying, or sending messages)
- Are any steps independent and could run in parallel?
- What are the hard constraints or hard preferences? Things that must or must not happen?

You may do multiple rounds here, one per step, for complex workflows. Iterate as needed.

**Round 4 — Final questions:**
- Confirm when this skill should be invoked, and suggest/confirm trigger phrases. (e.g., "Use when the user wants to cherry-pick a PR. Examples: 'cherry-pick to release', 'CP this PR', 'hotfix.'")
- Ask for any other gotchas or edge cases.

Stop interviewing once you have enough. Don't over-ask for simple processes!

## Step 3: Write the SKILL.md

Create the skill directory and file at the location the user chose.

Use this format:

```
---
name: <kebab-case-name>
description: "<one-line description>"
version: "1.0.0"
triggers:
  - <keyword1>
  - <keyword2>
when_to_use: "<detailed description of when to auto-invoke, with trigger phrases>"
arguments:
  - name: TARGET
    description: "The target to process"
    required: true
context: inline             # or "fork" for isolated execution
allowed_tools:              # optional: restrict tool access
  - <tool1>
  - <tool2>
---
# Skill Title

Description of skill.

## Inputs
- `$TARGET`: Description of this input

## Goal
Clearly stated goal. Best with defined artifacts or criteria for completion.

## Steps

### 1. Step Name
What to do. Be specific and actionable. Include commands when appropriate.

**Success criteria**: REQUIRED on every step. Shows when we can move on.

...
```

**Per-step annotations** (use where appropriate):
- **Success criteria** — REQUIRED. How to know the step is done.
- **Execution** — `Direct` (default), `Task agent` (sub-agent), or `[human]` (user does it). Only specify if not direct.
- **Artifacts** — Data this step produces that later steps need (e.g., PR number, commit SHA).
- **Human checkpoint** — When to pause and ask the user (irreversible actions, error judgment, output review).
- **Rules** — Hard constraints. User corrections during the session are especially useful here.

**Step structure tips:**
- Steps that can run concurrently use sub-numbers: 3a, 3b
- Steps requiring user action get `[human]` in the title
- Keep simple skills simple — a 2-step skill doesn't need annotations

**Frontmatter rules:**
- `allowed_tools`: Minimum permissions needed (use patterns like `bash(gh:*)` not just `bash`)
- `context`: Only set `context: fork` for self-contained skills that don't need mid-process user input
- `when_to_use` is CRITICAL — tells the model when to auto-invoke. Start with "Use when..." and include trigger phrases.
- `arguments` and argument-hint: Only include if the skill takes parameters. Use `$NAME` in the body for substitution.

## Step 4: Confirm and Save

Before writing the file, output the complete SKILL.md content as a code block so the user can review it. Then ask for confirmation.

After writing, tell the user:
- Where the skill was saved
- How to invoke it: `/<skill-name> [arguments]`
- That they can edit the SKILL.md directly to refine it

## Rules
- Keep instructions concise — context window is shared with conversation
- Use `when_to_use` to prevent accidental activation
- Default to `context: inline` unless the skill needs isolated execution
- Use `$ARGUMENTS` in the body for parameterized skills
- Name skills with kebab-case: `code-review`, `deploy-staging`, `fix-types`
- Pay special attention to places where the user corrected you during the session
