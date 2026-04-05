---
name: commit-msg
description: "Generate a clear, conventional commit message from staged changes"
version: "1.0.0"
triggers:
  - commit
  - "commit message"
  - "write commit"
  - "git commit"
when_to_use: "When the user wants to commit changes and needs a well-written commit message"
category: git
arguments:
  - name: HINT
    description: "Optional hint about the intent of the change"
    required: false
tags:
  - git
  - commit
  - workflow
---
# Commit Message Generator

Generate a clear, well-structured commit message for the staged changes.

## User Hint

$ARGUMENTS

## Process

### 1. Inspect Changes

```
git diff --cached --stat
git diff --cached
```

If nothing is staged, check unstaged changes with `git diff` and suggest what to stage.

### 2. Analyze the Change

Determine:
- **Type**: feat, fix, refactor, docs, test, chore, perf, ci, style, build
- **Scope**: Which module/component is primarily affected
- **What changed**: The concrete modifications
- **Why it changed**: The motivation (from the diff context, user hint, or recent conversation)

### 3. Write the Message

Follow conventional commits format:

```
<type>(<scope>): <summary in imperative mood, ≤72 chars>

<body — explain WHY, not WHAT (the diff shows what)>

<footer — breaking changes, issue refs, co-authors>
```

Rules for the summary line:
- Imperative mood: "add" not "added" or "adds"
- Lowercase after the colon
- No period at the end
- ≤72 characters total

Rules for the body:
- Focus on WHY and context the diff can't convey
- Skip the body for trivial changes (typo fix, version bump)
- Wrap at 72 characters

### 4. Present and Apply

Show the proposed commit message. If the user approves, execute the commit.

If the user has a signing key configured (`git config commit.gpgsign`), respect it.

## Rules
- Never include file lists in the message — `git log --stat` already shows that
- Don't narrate the diff ("changed X from Y to Z") — explain intent
- If there are unrelated changes staged together, suggest splitting into separate commits
- Respect any existing commit message conventions in `git log --oneline -20`
