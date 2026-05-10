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
  - git
  - git
  - git_show
  - git_log
  - github
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
| `commits:N` / `latest N commits` / `review the latest N commits` | `git_log(limit=N)` once → `git_show(sha)` for **all N** shas |
| `pr:<number>` / GitHub PR URL | `bash("gh pr diff N --repo owner/repo 2>&1")` |

No changes? `git {action: "status"}`, try `staged: true`. Still nothing? Ask user.

**🛑 Postcondition — "latest N commits" mode (P1, session 8d9e5903 regression):**

When the user asked you to review **N commits**, you MUST have exactly N `git_show` (or equivalent full-diff) fetches in your context before writing the report. The common failure mode is: fetch 3 out of 5, tell yourself "I have enough signal from the commit messages", and write the review anyway. This is a lie to yourself — you don't know what's in the 2 you skipped.

**Rule**: if your fetched-diff count < requested N, you have two choices and only two:
1. Keep fetching until count == N, OR
2. Explicitly tell the user "I only fetched K of N commits because [reason]; here is a partial review" — BEFORE the report.

Do NOT write a full "## Code Review" header and act as if you covered everything when you didn't. The phrases "I have enough", "enough signal", "without more fetches" are banned in this mode unless count == N.

---

## Step 2: Fetch Diff + Scan Signals

Fetch the full diff. Scan for signals and decide which checks to run in Step 3.

**Parallel review planning:**

- If you spawn focused reviewers or use isolated worktrees, make the capability check explicit before claiming parallelism.
- If `git worktree` or background agent isolation is unavailable, do not silently degrade to serial execution. Say so in the final report: `Parallelism degraded: git worktree unavailable; reviews ran serially.` Include the reason if the tool returned one.
- If a capability is required for the requested review depth and the fallback would materially reduce coverage, stop and ask whether to degrade or retry with the required capability.

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

Do not call `git_show` or `git {action: "diff"}` more than once on the same target. Do not invoke `skill(review-changes)` again in the same session.

---

## Step 3: Report

**Output NOTHING while making tool calls. Write the report only when all analysis is done.**

**Self-critique gate before final report:**

Run a final review pass over your own findings and the changed files before writing the report:

1. Re-read the diff summary and confirm every reported 🔴/🟡 issue still applies to the final diff.
2. Run mechanical gates that match the modified file types. Always include `git diff --check` for working-tree reviews; add formatter/test gates only when the diff changed code in that ecosystem.
3. If a gate fails because of the changes under review, report it as a finding. If the gate is unavailable, state it as residual risk instead of pretending it passed.

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
