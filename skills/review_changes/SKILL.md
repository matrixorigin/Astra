---
name: review-changes
description: "Developer skill: context-aware code review of uncommitted changes, branch diffs, specific commits, or GitHub PR URLs. Signal-driven analysis routing for efficient deep review."
user_invocable: true
when_to_use: "When the user asks to review code changes, commits, diffs, PRs, or GitHub PR URLs (e.g. 'review https://github.com/.../pull/123', 'review latest commit', 'review staged'). Scales automatically — small diffs get a quick path, larger diffs (>5 files or >200 lines) get a deeper multi-step review."
arguments:
  - name: TARGET
    description: "What to review: 'staged', 'unstaged', 'branch:<name>', 'commit:<sha>', 'pr:<number>', or a GitHub PR URL (e.g. 'https://github.com/owner/repo/pull/123'). Default: all uncommitted changes."
    required: false
  - name: FOCUS
    description: "Review focus: 'bugs', 'security', 'logic', 'api', 'tests', or 'all' (default: all)"
    required: false
allowed_tools:
  - git_diff
  - git_status
  - git_show
  - git_log
  - github_get_pr
  - read_file
  - grep
  - glob
  - bash
---

# Review Changes

**Only surface issues that matter**: bugs, security, logic errors, API breakage, missing tests. Never comment on style/formatting.

## Task

$ARGUMENTS

---

## Step 1: Size Check

Call `git_diff(stat_only: true)` (or `git_show(sha, stat_only: true)` for a commit, `bash("gh pr diff N --repo owner/repo 2>&1")` for a PR).

**If stat returns empty:** report "No changes found" immediately. Do NOT call more tools.

| Size | Next step |
|------|-----------|
| ≤ 80 lines, ≤ 5 files | Fetch full diff → go to Step 3 directly |
| > 80 lines or > 5 files | Go to Step 2 |

**TARGET resolution:**

| TARGET | Tool |
|--------|------|
| Default (uncommitted) | `git_diff()` |
| `staged` | `git_diff(staged: true)` |
| `branch:<name>` | `git_diff(ref: "main")` |
| `commit:<sha>` / `latest commit` | `git_log()` once → `git_show(sha)` |
| `pr:<number>` / GitHub PR URL | `bash("gh pr diff N --repo owner/repo 2>&1")` |

No changes? `git_status`, try `staged: true`. Still nothing? Ask user.

---

## Step 2: Fetch Diff + Scan Signals

Fetch the full diff. Scan for signals and decide which checks to run in Step 3.

**Signals → checks:**

| Signal in diff | Check to run |
|----------------|--------------|
| `unsafe`, `Command::new`, SQL string interp, `unwrap()` outside tests | Security |
| `pub fn/struct/enum/trait` signature change | API callers (`grep` for call sites) |
| `impl Foo for Bar`, `#[async_trait]` | Trait conformance |
| Error type/variant change, `thiserror` | `?`/`From`/`Into` chain |
| Config struct, `Cargo.toml` dep change | Backward compat, defaults |
| Tool schema/register, `edge_tools` | Registered? `parallel_safe`? Schema matches impl? |
| `JournalEvent` change | `.jsonl` backward compat? New fields optional? |
| `SubtaskStage` change | State transitions valid? Display updated? |
| DB schema / cloud-synced struct | SQL migration needed? Sync adapter updated? |
| Pure docs/comments/whitespace | Skip deep analysis |

**Context budget:** at most 3 `read_file` calls total. Before each one, name the exact question it answers. If you can't, skip it.

Do not call `git_show` or `git_diff` more than once on the same target. Do not invoke `skill(review-changes)` again in the same session.

---

## Step 3: Report

**Output NOTHING while making tool calls. Write the report only when all analysis is done.**

```
## Code Review: {target}
Scope: {n} files, +{added}/-{removed} lines

### 🔴 Critical
- {file}:{line} — {issue and why it matters}

### 🟡 Important
- {file}:{line} — {issue and why it matters}

### 💡 Suggestions
- {file}:{line} — {improvement and benefit}

### ✅ Looks Good
{what's well done, or "LGTM" if nothing material found}
```

No style/formatting comments. Every 🔴🟡 needs file:line and explains **why**. If nothing material: `LGTM` + one sentence on residual risk.
