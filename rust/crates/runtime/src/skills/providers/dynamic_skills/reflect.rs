pub fn skill_content() -> String {
    format!(
        r#"---
name: reflect
description: "Pause and reflect on your own behavior — examine context usage, tool choices, approach effectiveness, and course-correct"
version: "2.0.0"
allowed_tools:
  - bash
  - read_file
triggers:
  - reflect
  - "step back"
  - "what are you doing"
  - "why did you"
  - reconsider
when_to_use: "When you sense you might be going down the wrong path, when the user questions your approach, or after multiple failed attempts at a task"
category: meta
tags:
  - meta
  - self-assessment
---
# Reflect: Agent Self-Assessment

Pause execution and critically examine your own behavior. This is a meta-skill — you are analyzing yourself, not the user's code.

**Working directory**: ${{CTX_WORK_DIR}}
**Available tools**: ${{CTX_AVAILABLE_TOOLS}}

## Step 1: Audit Context Usage

**Success criteria**: Clear separation of confirmed facts vs assumptions.

Review how you've been using your context window:

- **What do you actually know?** List the files you've read, tools you've called, and facts you've established. Separate confirmed facts from assumptions.
- **What are you assuming?** Identify anything you're treating as true without having verified it. Flag these explicitly.
- **What's missing?** What information would change your approach if you had it? Is it obtainable?
- **Are you hallucinating?** Check: did you reference a function, file, or API that you haven't actually seen? If so, verify before continuing.

## Step 2: Evaluate Tool Selection

**Success criteria**: Identified at least one tool usage that could be improved.

Review your recent tool calls:

- **Right tool for the job?** Did you use grep when you should have read the file? Did you read files you didn't need? Did you use bash when a more targeted tool existed?
- **Efficiency**: Are you making redundant calls? Reading the same file twice? Running commands you've already run?
- **Missing tools**: Is there a tool available that would solve this more directly? Check what tools you have access to.
- **Over-reliance**: Are you leaning too heavily on one approach? (e.g., always grepping when you should be reading documentation, or always reading code when you should be running it)

## Step 3: Assess Approach Effectiveness

**Success criteria**: Honest answer to "Is this working?"

- **Is the current approach working?** If you've been at this for multiple turns without progress, the approach is wrong — not the execution.
- **Scope creep**: Are you solving the problem the user asked about, or have you drifted into a related but different problem?
- **Complexity bias**: Are you building something elaborate when a simple solution exists? The best fix is often the smallest one.
- **Confirmation bias**: Are you looking for evidence that supports your current theory while ignoring evidence that contradicts it?

## Step 4: Course-Correct

**Success criteria**: A concrete decision with reasoning, followed by immediate action.

Based on your self-assessment, decide:

1. **Continue** — current approach is sound, proceed with more focus
2. **Pivot** — change strategy based on what you found (explain what and why)
3. **Ask** — you need information from the user to proceed effectively
4. **Simplify** — you've been over-engineering; strip back to the minimal solution

State your decision clearly and explain your reasoning. Then act on it immediately — reflection without action is just stalling.
"#,
    )
}
