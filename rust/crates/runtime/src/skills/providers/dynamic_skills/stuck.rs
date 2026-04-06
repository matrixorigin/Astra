pub fn skill_content() -> String {
    format!(
        r#"---
name: stuck
description: "Break through when stuck — use runtime diagnostics to identify the real blocker, then try a fundamentally different approach"
version: "1.0.0"
allowed_tools:
  - bash
  - read_file
  - delegate
triggers:
  - stuck
  - "can't figure out"
  - "going in circles"
  - "tried everything"
  - "not making progress"
when_to_use: "When you or the user is stuck on a problem after multiple failed attempts, going in circles, or not making progress despite trying"
category: diagnostics
arguments:
  - name: PROBLEM
    description: "Description of what you're stuck on"
    required: false
tags:
  - debugging
  - problem-solving
---
# Stuck: Break Through the Impasse

You've been going in circles or hitting dead ends. Use the runtime data below to understand WHY, then try a fundamentally different approach.

## Runtime Diagnostics

| Metric | Value |
|--------|-------|
| Turn | ${{CTX_TURN_NUMBER}} / ${{CTX_TURN_NUMBER}}+${{CTX_TURNS_REMAINING}} |
| Prompt tokens (cumulative) | ${{CTX_TOTAL_PROMPT_TOKENS}} |
| Tool calls (total) | ${{CTX_TOTAL_TOOL_CALLS}} |
| Stall nudges sent | ${{CTX_NUDGE_COUNT}} |
| Errors | ${{CTX_ERROR_COUNT}} |
| Deprioritized tools | ${{CTX_DEPRIORITIZED_TOOLS}} |
| Stall events | ${{CTX_STALL_EVENTS}} |
| Correction follow rate | ${{CTX_CORRECTION_FOLLOW_RATE}} |

## Step 1: Diagnose the Impasse

Read the metrics and classify the blocker:

| Pattern | Diagnosis | Go to |
|---------|-----------|-------|
| `nudge_count` ≥ 2 | System already told you to stop. You ignored it. | Step 3 option 5 |
| `error_count` / `tool_calls` > 30% | Tool or environment is broken | Step 2: challenge assumption #1–#4 |
| `deprioritized_tools` non-empty | These tools are failing repeatedly — stop using them | Step 3: use different tools |
| `stall_events` present | Exact stall type tells you what's repeating | Step 3: pick the opposite strategy |
| High `tool_calls`, low progress | Exploring without a plan | Step 3 option 1 |
| High `prompt_tokens`, few turns | Context bloated from large reads | Step 3 option 2 |

Then summarize in one sentence: what is the actual goal (not the approach — the goal), and what has been tried.

## Step 2: Challenge Assumptions

At least one of these is wrong. Check each:

1. **The bug is where you think it is** — the error points to the symptom, not the cause. Search upstream.
2. **The API works as documented** — read the source, not the docs. Check the actual version installed.
3. **The data is what you expect** — log actual values. Print types, lengths, encodings.
4. **The environment matches** — compare versions: `rustc --version`, `node --version`, etc.
5. **Your previous fix worked** — verify by reverting it.

For each assumption: "What if this is wrong? What would I see?"

## Step 3: Try a Different Strategy

Pick ONE. Time-box to 15 minutes — if no progress, switch to the next:

1. **Binary search** — `git bisect`, or comment out half the code. Narrow to smallest failing case.
2. **Minimal reproduction** — new empty project, add only what's needed. If it works in isolation, the bug is in the interaction.
3. **Read the source** — the dependency's code, not yours. The answer is in the implementation.
4. **Invert the problem** — instead of "why does this fail?", ask "under what conditions would this succeed?" and verify each.
5. **Stop and report** — if `nudge_count` ≥ 2, summarize what you've found and ask the user for guidance. Do NOT keep trying.

## Step 4: Escalate

If nothing works:
- Search for the exact error message (include version numbers)
- Check the project's issue tracker
- Formulate: "I'm trying to [goal]. Expected [X], got [Y]. Tried [A, B, C]. Minimal repro: [code]."

## Rules
- Do NOT keep trying the same approach with small variations
- If `deprioritized_tools` lists a tool, do NOT use it
- If `nudge_count` ≥ 3, you MUST stop and ask the user — no more autonomous attempts
"#,
    )
}
