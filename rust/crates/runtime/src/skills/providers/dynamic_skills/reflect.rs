pub fn skill_content() -> String {
    format!(
        r#"---
name: reflect
description: "Pause and reflect — use runtime metrics (tokens, errors, stalls, tool health) to diagnose problems and course-correct"
version: "1.0.0"
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
composition:
  composable: true
  idempotent: true
  max_duration_sec: 120
---
# Reflect: Data-Driven Self-Assessment

Pause and critically examine your own behavior using both your conversation history and the runtime metrics below.

## Runtime Snapshot

| Metric | Value |
|--------|-------|
| Turn | ${{CTX_TURN_NUMBER}} of ${{CTX_TURN_NUMBER}}+${{CTX_TURNS_REMAINING}} |
| Prompt tokens (cumulative) | ${{CTX_TOTAL_PROMPT_TOKENS}} |
| Completion tokens (cumulative) | ${{CTX_TOTAL_COMPLETION_TOKENS}} |
| Tool calls (total) | ${{CTX_TOTAL_TOOL_CALLS}} |
| Stall nudges sent | ${{CTX_NUDGE_COUNT}} |
| Errors | ${{CTX_ERROR_COUNT}} |
| Deprioritized tools | ${{CTX_DEPRIORITIZED_TOOLS}} |
| Stall events | ${{CTX_STALL_EVENTS}} |
| Correction follow rate | ${{CTX_CORRECTION_FOLLOW_RATE}} |

Use these numbers — don't guess. A blank value means zero/none.

## Step 1: Diagnose from Metrics

Read the snapshot above and answer:

- **Token burn rate**: Is `prompt_tokens` growing faster than expected? Over 50k in <5 turns suggests context bloat (large tool results, repeated file reads, or compaction not triggering).
- **Tool failure rate**: `errors / tool_calls` — above 20% means something systemic is wrong. Check which tools are deprioritized.
- **Stall signals**: Any `nudge_count > 0` or `stall_events` means the system already detected you're stuck. What pattern triggered it? Are you still doing the same thing?
- **Correction compliance**: If `correction_follow_rate` is below 80%, you're ignoring the system's guidance. Why?

If all metrics look healthy (low errors, no stalls, reasonable token growth), skip to Step 3.

## Step 2: Root Cause

Based on Step 1, identify the root cause. Common patterns:

| Symptom | Likely Cause | Fix |
|---------|-------------|-----|
| High prompt tokens, few turns | Reading large files or getting huge tool outputs | Read specific line ranges; use grep first |
| Errors climbing | Wrong tool or wrong arguments | Check deprioritized list; switch tools |
| Stall detected | Repeating same approach | Stop. Try a completely different tool or strategy |
| Nudges ignored | Fixated on one approach | Respect the avoid list. Use suggested alternatives |
| Many tool calls, little progress | Exploring without a plan | State your plan in 3 bullet points, then execute |

## Step 3: Qualitative Check

- **Assumptions**: What are you treating as true without verification? List them.
- **Scope**: Are you still solving the user's actual problem, or have you drifted?
- **Simplicity**: Is there a simpler approach you haven't tried?

## Step 4: Decision

Choose one and act immediately:

1. **Continue** — metrics healthy, approach sound
2. **Pivot** — state what you'll change and why (reference the metric that triggered this)
3. **Ask** — you need user input to proceed
4. **Simplify** — strip back to minimal solution

Reflection without action is stalling. Decide and move.
"#,
    )
}
