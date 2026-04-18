---
name: review-changes
description: "Developer skill: context-aware code review of uncommitted changes, branch diffs, specific commits, or GitHub PR URLs. Signal-driven analysis routing for efficient deep review."
user_invocable: true
when_to_use: "When the user asks to review code changes, commits, diffs, PRs, GitHub PR URLs (e.g. 'review https://github.com/.../pull/123'), or says 'review latest commit'"
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

Signal-driven code review. Goes beyond line-by-line diff — dynamically selects analysis depth based on what actually changed.

**Only surface issues that matter**: bugs, security, logic errors, API breakage, missing tests. Never comment on style/formatting.

## Task

$ARGUMENTS

---

## Phase 1: Gather + Route

### 1.1 Resolve TARGET

| TARGET | Tool call |
|--------|-----------|
| Default (uncommitted) | `git_diff()` |
| `staged` | `git_diff(staged: true)` |
| `branch:<name>` | `git_diff(ref: "main")` |
| `commit:<sha>` | `git_show(sha)` |
| `latest commit` / `review latest commit` | `git_log()` once to resolve the SHA, then `git_show(resolved_sha)` once |
| `pr:<number>` | See PR workflow below |
| GitHub PR URL | See PR workflow below |
| Stat overview | `git_diff(stat_only: true)` first, then per-file |

**PR workflow** (for `pr:<number>` or GitHub PR URL like `https://github.com/owner/repo/pull/123`):

1. Parse `owner/repo` and PR number from the URL (if URL given)
2. Get the diff via `gh pr diff N --repo owner/repo 2>&1` using `bash`
   - Always use `bash` + `gh` for PR diffs — it works with the user's `gh auth` credentials regardless of whether `GITHUB_TOKEN` is set
   - If `gh` fails (not installed, not authenticated, repo not accessible), report the error and stop
3. Optionally use `github_get_pr` with `detail: "normal"` for PR metadata (title, body, changed files). If it fails (no GITHUB_TOKEN for private repos), skip — the diff is sufficient for review

No changes? Check `git_status`, try `staged: true`. Still nothing? Ask user.

### 1.2 Reuse gathered evidence

- For `latest commit`, resolve the SHA once and reuse that same commit diff for the rest of the review.
- **Do not call `git_show` on the same commit more than once** unless you truly need a different object than the full diff you already fetched.
- When searching changed symbols, prefer one decisive `grep` over a scoped `grep` followed by the same repo-wide `grep`.
- Once the diff already identified the file to inspect, prefer `read_file` for local context instead of re-reading the whole commit diff.

### 1.3 Strategy Router

After reading the diff, route based on **signals** in the diff:

**FOCUS override** (skip signal scan):
| FOCUS | Phases |
|-------|--------|
| `bugs` | 1 → 3.1 → 6 |
| `security` | 1 → 3.1 → 3.2 → 6 |
| `logic` | 1 → 3.1 → 3.3 → 6 |
| `api` | 1 → 2 → 3.1 → 4 → 6 |
| `tests` | 1 → 3.3 → 6 |

**Signal-based routing** (FOCUS unset):
| Signal | Trigger |
|--------|---------|
| `unsafe`, `Command::new`, SQL interp, `unwrap()` non-test | → 3.2 |
| `pub fn/struct/enum/trait` signature change | → 2 + 4 |
| Tool schema/register, `JournalEvent`, `SubtaskStage`, `SERVER_EXECUTOR_TOOL_NAMES` | → 5 |
| `impl Foo for Bar`, `#[async_trait]` | → 4.1 |
| Error type/variant change, `thiserror` | → 4.2 |
| Config struct, `Cargo.toml` | → 4.3 |
| Pure docs/comments/config (no code logic) | → skip 2, 3, 4 |

Output: `📋 Strategy: 1 → 3.1 → 5.1 → 6 | Skipped: 2, 3.2, 3.3, 4`

**Common patterns:**
| Profile | Phases | Tool calls |
|---------|--------|------------|
| Trivial (<50 lines, no signals) | 1 → 3.1* → 6 | 2-3 |
| Schema/config addition | 1 → 3.1* → 5.1 → 6 | 3-4 |
| Bug fix with logic | 1 → 3.1 → 3.3 → 6 | 4-6 |
| API change | 1 → 2 → 3.1 → 4 → 6 | 6-10 |
| Security-relevant | 1 → 3.1 → 3.2 → 5 → 6 | 6-10 |
| Large refactor (>300 lines) | 1 → 2 → 3 → 4 → 5 → 6 | 10-15 |

*\*3.1 trivial = light scan only (5-10 diff lines for obvious bugs). Skip full checklist.*

**Rules:** Always run 1 + 3.1 + 6. Re-route if Phase 3 reveals new signals. When in doubt, include.

---

## Phase 2: Structural Analysis *(conditional)*

1. Extract changed symbols — are they `pub`?
2. Find callers via `grep`
3. Signature changes? Assess semver impact (breaking vs additive)
4. Import changes? Old paths still valid?

---

## Phase 3: Deep Review *(3.1 always; 3.2/3.3 conditional)*

### 3.0 Context (only if diff is ambiguous)

You already read the diff in Phase 1 — don't re-read it. Only `read_file` surrounding context when the diff alone is unclear. For files >200 lines, `outline: true` first.

### 3.1 Bug Detection

Trivial: light scan for obvious bugs.
Medium/Large: full scan — logic errors, concurrency issues, error handling gaps.

### 3.2 Security *(conditional)*

Command injection, path traversal, credential exposure, SQL injection, `unsafe` without safety comments.

### 3.3 Test Coverage *(conditional)*

For changed code paths: test exists? Covers new behavior? Edge cases?

---

## Phase 4: Cross-File Consistency *(conditional)*

| # | Signal | Check |
|---|--------|-------|
| 4.1 | Trait modified | All impls conform? Defaults/mocks updated? |
| 4.2 | Error type changed | `?`/`From`/`Into` still valid? |
| 4.3 | Config struct changed | Defaults sensible? Backward compatible? |
| 4.4 | Public API changed | Docs/examples updated? |

---

## Phase 5: Astra-Specific *(conditional, sub-phases independent)*

| # | Signal | Check |
|---|--------|-------|
| 5.1 | Tool schema/register | Registered in edge_tools? Category correct? `parallel_safe` if read-only? Schema matches impl? |
| 5.2 | JournalEvent changed | .jsonl backward compat? New fields optional? Cloud ingestion updated? |
| 5.3 | SubtaskStage changed | State transitions valid? Display handles new states? |
| 5.4 | Cloud-synced struct | SQL migration needed? Sync adapter updated? |

---

## Phase 6: Report

**⚠ Write the report ONLY after all analysis is complete. While making tool calls, output NOTHING.**

---

## Code Review: {target}

**Scope:** {n} files, +{added}/-{removed} lines

### 🔴 Critical ({n})
{issues with file:line and why}

### 🟡 Important ({n})
{issues with file:line and why}

### 💡 Suggestions ({n})
{non-blocking improvements with benefit}

### ✅ Looks Good
{what's well done}

### 📊 Impact Assessment
| Aspect | Status |
|--------|--------|
| Public API | {n} ({breaking/additive/internal}) |
| Test coverage | {adequate/needs-work/missing} |
| Cross-file impact | {files affected} |
| Semver | {major/minor/patch} |

**Rules:** No style/formatting comments. Every issue needs file:line. Every 🔴🟡 must explain **why**. Every 💡 must explain **benefit**.
