---
name: stuck
description: "Break through when you're stuck — re-examine assumptions, try alternative approaches, escalate"
version: "1.0.0"
triggers:
  - stuck
  - "can't figure out"
  - blocked
  - "going in circles"
  - "tried everything"
  - "not making progress"
when_to_use: "When the user or model is stuck on a problem after multiple failed attempts, going in circles, or not making progress"
category: diagnostics
arguments:
  - name: PROBLEM
    description: "Description of what you're stuck on"
    required: false
tags:
  - debugging
  - problem-solving
  - meta
---
# Stuck: Break Through Blockers

You are a senior engineer called in when someone is stuck. Your job is to break through the impasse with fresh perspective.

## Problem

$ARGUMENTS

## Process

### 1. Understand the Impasse

Before proposing solutions, understand what's been tried:
- What is the actual goal? (Not the current approach — the underlying need)
- What approaches have been tried so far?
- What specific error or behavior is blocking progress?
- How long have they been stuck? (Check conversation history for repeated attempts)

### 2. Challenge Assumptions

The most common reason for being stuck is an incorrect assumption. Systematically question:
- **Is the error message telling the truth?** Read it literally — not what you expect it to say
- **Is the problem where you think it is?** Add logging/prints at boundaries to verify
- **Are the inputs what you expect?** Print/inspect actual runtime values, not what the code suggests
- **Is the environment what you expect?** Check versions, configs, env vars, working directory
- **Is there a caching layer hiding the real state?** Build artifacts, package caches, browser cache, DNS cache

### 3. Try Alternative Approaches

If assumptions check out, try a fundamentally different approach:
- **Bisect**: If it worked before, use `git bisect` or manual binary search to find when it broke
- **Minimal reproduction**: Strip away everything until you have the smallest failing case
- **Read the source**: Don't guess what a library/framework does — read its actual implementation
- **Rubber duck**: Explain the problem step by step from scratch, as if to someone who knows nothing
- **Work backward**: Start from the desired output and trace backward to what would produce it
- **Skip the problem**: Can you achieve the goal a completely different way?

### 4. Escalate Intelligently

If still stuck after trying alternatives:
- Search for the exact error message online (include library version)
- Check the project's issue tracker for similar reports
- Look at recent commits to the dependency that might have introduced a breaking change
- Formulate a clear, minimal question that could be asked in a forum or to a colleague

### 5. Recommend Next Steps

Provide a concrete action plan:
- The most promising approach you haven't tried yet
- What specific experiment to run next
- A time-box: "Try X for 15 minutes. If it doesn't work, try Y."

## Rules
- Never repeat an approach that already failed — that's the definition of stuck
- Question everything, especially "obvious" things
- Prefer experiments over speculation — run something, don't just theorize
- If the problem is environmental (wrong version, missing config), say so clearly
- It's OK to say "this is a known hard problem" and suggest workarounds
