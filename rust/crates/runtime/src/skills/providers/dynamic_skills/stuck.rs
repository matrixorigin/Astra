pub fn skill_content() -> String {
    format!(
        r#"---
name: stuck
description: "Break through when stuck — re-examine assumptions, try alternative approaches, escalate intelligently"
version: "2.0.0"
triggers:
  - stuck
  - "can't figure out"
  - blocked
  - "going in circles"
  - "tried everything"
  - "not making progress"
when_to_use: "When you or the user is stuck on a problem after multiple failed attempts, going in circles, or not making progress"
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

You've been going in circles or hitting dead ends. Time to step back and try a fundamentally different approach.

## Step 1: Understand the Impasse

**Success criteria**: Clear statement of what's been tried and why it failed.

Summarize:
- What is the actual goal? (not the approach — the goal)
- What approaches have been tried?
- What happened with each? (exact errors, unexpected behavior)
- How long have you been stuck?

## Step 2: Challenge Assumptions

**Success criteria**: At least one assumption identified that might be wrong.

Common wrong assumptions:
1. **The bug is where you think it is** — the error message points to symptom, not cause. Search upstream.
2. **The API works as documented** — read the source, not the docs. Check the actual version installed.
3. **The data is what you expect** — log the actual values at each step. Print types, lengths, encodings.
4. **The environment matches** — compare dev vs prod, local vs CI. Check versions: `rustc --version`, `node --version`, etc.
5. **Your previous fix worked** — verify by reverting it. Confirmation bias is real.

For each assumption you hold, ask: "What if this is wrong? What would I see?"

## Step 3: Try Alternative Approaches

**Success criteria**: Progress on at least one new path.

Try these in order. Time-box each to 15 minutes — if no progress, switch:

1. **Binary search the problem space** — `git bisect`, or comment out half the code to isolate. Narrow down to the smallest failing case.
2. **Minimal reproduction** — create a new, empty project and add ONLY what's needed to reproduce. If it works in isolation, the bug is in the interaction.
3. **Read the source** — not your code, the dependency's code. The answer is often in the implementation, not the docs.
4. **Invert the problem** — instead of "why does this fail?", ask "under what conditions would this succeed?" and verify each condition.
5. **Rubber duck** — explain the problem step by step as if to someone who knows nothing about it. Say it out loud (or type it). Surprisingly effective.
6. **Sleep on it** — if you've been at this for hours, suggest the user take a break. Fresh eyes solve more bugs than tired ones.

## Step 4: Escalate Intelligently

**Success criteria**: A concrete, answerable question that an expert could help with.

If alternative approaches don't work:
- Search for the exact error message online (include version numbers)
- Check the project's issue tracker for similar reports
- Formulate a clear question: "I'm trying to [goal]. I expected [X] but got [Y]. I've tried [A, B, C]. Here's a minimal reproduction: [link/code]."

## Rules
- Do NOT keep trying the same approach with small variations — that's the definition of stuck
- Each new attempt must be a fundamentally different strategy
- If you've spent 3x the expected time, it's time to ask for help, not try harder
"#,
    )
}
