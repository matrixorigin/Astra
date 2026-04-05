pub fn skill_content() -> String {
    format!(
        r#"---
name: skillify
description: "Capture this session's repeatable process into a reusable SKILL.md skill file"
version: "2.0.0"
allowed_tools:
  - read_file
  - write_file
  - bash
triggers:
  - skillify
  - "make a skill"
  - "save as skill"
  - "create skill"
when_to_use: "When the user explicitly wants to capture a useful workflow, prompt pattern, or set of instructions as a reusable skill"
category: meta
arguments:
  - name: DESCRIPTION
    description: "Description of the process to capture as a skill"
    required: false
tags:
  - meta
  - skill-creation
---
# Skillify: Capture Workflow as Reusable Skill

You are capturing this session's repeatable process as a reusable skill.

## Step 1: Analyze the Session

**Success criteria**: You can describe the process in 2-3 sentences.

Before asking any questions, analyze the conversation to identify:
- What repeatable process was performed
- What the inputs/parameters were
- The distinct steps (in order)
- The success criteria for each step
- Where the user corrected or steered you
- What tools and permissions were needed
- What the goals and success artifacts were

## Step 2: Interview the User

Use concise questions to clarify. Keep it brief for simple processes — skip rounds that don't add value.

**Round 1: High-level Confirmation**
- Suggest a name and description based on your analysis
- Suggest high-level goals and success criteria

**Round 2: Details**
- Present the steps you identified as a numbered list
- Suggest arguments the skill should accept
- Ask if the skill should run `inline` (in-conversation, user can steer) or `fork` (as a sub-agent, self-contained). Fork is better for self-contained tasks; inline for interactive ones.
- Ask where to save:
  - **This repo** (`.astra/skills/<name>/SKILL.md`) — for project-specific workflows
  - **Personal** (`~/.astra/skills/<name>/SKILL.md`) — follows the user across all repos

**Round 3: Step Breakdown** (for complex skills with 5+ steps)
For each major step, clarify if not obvious:
- What does this step produce that later steps need?
- What proves this step succeeded?
- Should the user confirm before proceeding? (especially for irreversible actions)
- Are any steps independent and could run in parallel?

Pay special attention to places where the user corrected you during the session.

**Round 4: Final** (if not already clear)
- Confirm when this skill should be invoked, and suggest trigger phrases
- Ask for any gotchas or edge cases

Stop interviewing once you have enough information. Don't over-ask for simple processes.

## Step 3: Write the SKILL.md

**Success criteria**: A complete, parseable SKILL.md file.

Create the skill directory and file at the chosen location.

Use this format:

```markdown
---
name: <skill-name>
description: <one-line description>
allowed_tools:
  - bash
  - read_file
  - write_file
  - delegate
when_to_use: <detailed description including trigger phrases>
arguments:
  - name: <arg_name>
    description: <arg description>
    required: <true/false>
context: <inline or fork — omit for inline>
---

# <Skill Title>

Description of skill.

## Goal
Clearly stated goal with defined success artifacts.

## Steps

### 1. Step Name
What to do. Be specific and actionable. Include commands when appropriate.

**Success criteria**: What proves this step is done.

...
```

**Frontmatter rules:**
- `allowed_tools`: list the specific tools needed (e.g., `bash`, `read_file`, `write_file`, `delegate` for sub-agents, or MCP tool names)
- `context`: only set `fork` for self-contained skills that don't need mid-process user input
- `when_to_use`: start with "Use when..." and include trigger phrases

## Step 4: Confirm and Save

**Success criteria**: Skill file written to disk and user informed of invocation.

Show the complete SKILL.md content for review. Ask "Does this look good to save?"

After writing, tell the user:
- Where the skill was saved
- How to invoke it: `/skill <skill-name> [arguments]`
- That they can edit the SKILL.md directly to refine it

## Rules
- Do NOT save without explicit user confirmation
- Trigger phrases must be specific enough to avoid false activation in normal conversation
- If a trigger would match >10% of typical developer messages, it's too broad — refine it
"#,
    )
}
