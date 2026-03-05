---
inclusion: always
---

# Cloud Skill Design Rules

## Core Principle

**The skill is responsible for information density. The LLM must not do secondary trimming.**

A skill fetches raw API data and returns a structured, pre-digested summary at the requested
detail level. The LLM receives exactly what it needs — no more, no less. It should never need
to truncate, summarize, or filter the skill's output before presenting it to the user.

---

## Detail Levels (Mandatory for All Data-Fetching Skills)

Every skill that returns content from an external source MUST support a `detail` parameter
with these four levels. Default is always `brief`.

| Level | When to use | Information density |
|-------|-------------|---------------------|
| `brief` | Default. Lists, overviews, "what's going on" | Minimum viable for a decision |
| `normal` | User asks "what is this" or "tell me about X" | Key context added |
| `detailed` | User asks "why did it fail" or "show me details" | Diagnostic / explanatory info |
| `full` | User explicitly asks for "everything" or "complete" | Near-raw, but noise still stripped |

**Rules:**
- `brief` is the default — never return more than needed unless asked
- Each level is a strict superset of the level below it
- `full` still truncates at a token budget (see below) — it is NOT "no truncation"
- The LLM description must state what each level adds

---

## Truncation Rules

Apply these limits consistently across all skills:

| Content type | `brief` | `normal` | `detailed` | `full` |
|---|---|---|---|---|
| Title / name | 80 chars | 80 chars | full | full |
| Body / description | omit | 200 chars | 500 chars | 2000 chars |
| Log / diff content | omit | omit | 500 chars | 2000 chars |
| List items | 10 (default) | 10 | 20 | 50 |
| Total output budget | ~500 chars | ~1500 chars | ~4000 chars | ~8000 chars |

Always append `[truncated]` when content is cut. Never silently drop content.

---

## Structured Output Requirements

### Field presence must be predictable

Every detail level must return a **fixed, documented field set**. The LLM must not need to
check `if field in result` — fields are always present, just empty/None when not applicable.

```python
# ❌ Bad: LLM has to guess which fields exist
class CIStatusOutput(SkillOutput):
    workflows: list[dict]  # dict shape varies by detail level

# ✅ Good: Fixed schema, detail level controls content depth
class WorkflowRun(BaseModel):
    name: str
    conclusion: str          # success / failure / pending / skipped / cancelled
    branch: str
    triggered_at: str        # YYYY-MM-DD HH:MM (always this format)
    duration_seconds: int | None   # None in brief
    pr_number: int | None          # None if not a PR run
    pr_title: str | None           # None in brief
    failed_jobs: list[str]         # [] in brief/normal, populated in detailed/full
    failed_steps: list[dict]       # [] unless full
```

### Normalize values — never pass through raw API strings

```python
# ❌ Bad: Raw GitHub values leak through
conclusion = github_run["conclusion"]  # could be None, "success", "failure", ...

# ✅ Good: Normalized to a known set
_CONCLUSION_MAP = {
    "success": "success",
    "failure": "failure",
    "cancelled": "cancelled",
    "skipped": "skipped",
    None: "pending",
    "in_progress": "pending",
    "queued": "pending",
    "waiting": "pending",
}
conclusion = _CONCLUSION_MAP.get(github_run["conclusion"], "unknown")
```

### Timestamps: always `YYYY-MM-DD HH:MM`

```python
# ❌ Bad: ISO 8601 is noisy for LLMs
"2026-03-04T06:47:17Z"

# ✅ Good: Human-readable, consistent
"2026-03-04 14:47"
```

---

## CI-Specific Skill Requirements

For skills that deal with CI/CD (workflow runs, job status, build logs):

```
brief:
  - workflow name
  - conclusion (success/failure/pending/skipped/cancelled)
  - branch
  - triggered_at (YYYY-MM-DD HH:MM)

normal (adds):
  - pr_number + pr_title (if triggered by PR)
  - commit message (truncated to 80 chars)
  - duration

detailed (adds):
  - names of failed jobs
  - first failed step name + error message per job (truncated to 200 chars)

full (adds):
  - all job statuses (not just failed)
  - failed step log snippets (truncated to 500 chars each)
  - total token budget: ~8000 chars
```

---

## PR-Specific Skill Requirements

```
brief:
  - number, title (80 chars), author, state, created_at
  - ci_conclusion (success/failure/pending — single top-level status)

normal (adds):
  - body summary (200 chars)
  - labels, reviewers
  - changed_files count

detailed (adds):
  - additions/deletions
  - key changed files list (top 10 by change size)
  - review comments count
  - merge status / conflicts

full (adds):
  - complete body (2000 chars)
  - per-file diff summary (500 chars per file, top 20 files)
  - all review comments (200 chars each)
```

---

## Issue-Specific Skill Requirements

```
brief:
  - number, title (80 chars), state, labels, created_at

normal (adds):
  - body (200 chars)
  - assignee, milestone, comment_count

detailed (adds):
  - most recent 3 comments (200 chars each)
  - linked PRs

full (adds):
  - complete body (2000 chars)
  - all comments (200 chars each, up to 20)
```

---

## Repo Auto-Resolution

All GitHub skills MUST support bare project names (e.g. `milvus`) in addition to
`owner/repo` format. Resolution uses GitHub Search API ranked by star count.

```python
# Standard pattern — copy this exactly
resolved_by_search = isinstance(repo, str) and "/" not in repo
resolved = self.github.resolve_repo(repo) if resolved_by_search else repo

# Output must include these fields so LLM can inform user
resolved_repo: str | None = None      # set when resolved_by_search=True
resolved_by_search: bool = False
```

**LLM description must say:**
> "If `resolved_by_search=True` in the result, show results first, then add a note:
> 'Auto-resolved to {resolved_repo} — let me know if you meant a different repo.'
> Do NOT ask for confirmation before showing results."

**LLM description must also say:**
> "If the user gives `owner/repo` format and it returns an error (404/not found),
> do NOT retry with just the project name and do NOT substitute a similar-sounding repo —
> tell the user the repo was not found or is private. Auto-search is only for bare project names."

**Why this matters:** GitHub Search API ranks by star count. A bare name like
`mo-auto-test` may match a completely unrelated high-star repo. Auto-search is
best-effort and only appropriate when the user explicitly gives a bare name.

---

## Skill Description Template

Every cloud skill description must cover:

1. **What it does** (one sentence)
2. **When to use it** (trigger conditions)
3. **repo format** — `owner/repo` or bare name, auto-resolved
4. **detail levels** — what each level adds (or reference to default)
5. **resolved_by_search** — confirmation instruction
6. **Do NOT call proactively** (if applicable)

```python
description = (
    "List pull requests in a GitHub repository. "                          # what
    "Use when user asks about PRs, recent changes, or what's in review. "  # when
    "repo can be 'owner/repo' or just a project name — auto-resolved. "    # repo format
    "detail: 'brief' (default) = number/title/author/state/CI; "           # levels
    "'normal' adds body summary + reviewers; "
    "'detailed' adds file counts + review comments; "
    "'full' adds complete body + diff summary. "
    "If resolved_by_search=True, tell user which repo was used and confirm."  # confirm
)
```

---

## Anti-Patterns

```python
# ❌ Return raw API response — LLM has to parse it
return {"raw": github_api_response}

# ❌ Return everything always — ignores detail level
return {"body": full_pr_body, "diff": full_diff, "all_comments": [...]}

# ❌ Inconsistent field presence — LLM has to guard every field
if detail == "full":
    result["diff"] = ...  # only sometimes present

# ❌ Pass through raw GitHub conclusion values
conclusion = run.conclusion  # None, "in_progress", etc.

# ❌ ISO 8601 timestamps
"created_at": "2026-03-04T06:47:17Z"

# ❌ Silent truncation
body = body[:200]  # no indication it was cut

# ✅ Correct truncation
body = body[:200] + " [truncated]" if len(body) > 200 else body
```
