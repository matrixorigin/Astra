---
name: pr-review
description: "Review a pull request for correctness, style, security, and performance"
version: "1.0.0"
context: fork
triggers:
  - "pr review"
  - "review pr"
  - "code review"
  - "review pull request"
  - "check this pr"
when_to_use: "When the user wants a thorough code review of a pull request or a set of changes"
category: code-review
arguments:
  - name: TARGET
    description: "PR number, branch name, or commit range to review"
    required: false
tags:
  - review
  - quality
  - git
---
# PR Review: Comprehensive Code Review

Perform a thorough code review of the specified changes.

## Target

$ARGUMENTS

## Process

### 1. Gather the Diff

Determine what to review:
- If `$ARGUMENTS` is a PR number → `gh pr diff $ARGUMENTS`
- If `$ARGUMENTS` is a branch → `git diff main...$ARGUMENTS`
- If `$ARGUMENTS` is empty → `git diff main...HEAD` (current branch vs main)

Also gather context:
- PR description (if applicable): `gh pr view $ARGUMENTS`
- Commit messages: `git log --oneline main...HEAD`
- Files changed: `git diff --stat main...HEAD`

### 2. Understand Intent

Before reviewing code, understand what the PR is trying to accomplish:
- Read the PR description / commit messages
- Identify the user story or problem being solved
- Note the scope: is this a focused change or a broad refactor?

### 3. Review Pass 1 — Correctness

For each changed file:
- Does the logic correctly implement the stated intent?
- Are edge cases handled? (null/empty inputs, boundary values, error paths)
- Are there race conditions or concurrency issues?
- Are error messages helpful and actionable?
- Do tests cover the new behavior?

### 4. Review Pass 2 — Security

- Input validation: are user inputs sanitized?
- Authentication/authorization: are checks in place?
- Injection: SQL, command, path traversal risks?
- Secrets: are credentials, tokens, or keys exposed?
- Dependencies: are new dependencies trustworthy and pinned?

### 5. Review Pass 3 — Design & Maintainability

- Does this follow existing patterns in the codebase?
- Is the abstraction level appropriate? (not too much, not too little)
- Will this be easy to modify in 6 months?
- Are there unnecessary changes (formatting, unrelated refactors)?
- Is the commit structure logical? (should it be squashed or split?)

### 6. Review Pass 4 — Performance

- Any O(n²) or worse algorithms on potentially large inputs?
- Unnecessary allocations or copies in hot paths?
- Database queries: N+1 patterns, missing indexes?
- Network: unnecessary round-trips, missing caching?

### 7. Write Review Summary

Structure the review as:

**Overview**: One paragraph — overall assessment (approve, request changes, or needs discussion)

**Must Fix**: Issues that should be addressed before merge (bugs, security, correctness)

**Should Fix**: Important improvements (design, performance, maintainability)

**Nit**: Style, naming, minor suggestions (optional to address)

**Positive Notes**: Good patterns, clever solutions, well-written tests worth highlighting

## Rules
- Be specific — reference exact lines and suggest concrete fixes
- Distinguish must-fix from nice-to-have — not everything is a blocker
- Acknowledge good work, not just problems
- If you're unsure about something, say so rather than guessing
- Don't flag style issues that an autoformatter handles
- Focus on substance over style
