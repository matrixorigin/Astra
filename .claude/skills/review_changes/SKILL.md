---
name: review-changes
description: "Signal-driven code review of uncommitted changes, branch diffs, commits, or PR diffs. Findings first; focus on bugs, regressions, data loss, security, API breakage, and missing tests."
user_invocable: true
when_to_use: "When the user asks to review code changes, commits, diffs, PRs, staged/unstaged work, or latest commits."
arguments:
  - name: TARGET
    description: "staged, unstaged, branch:<name>, commit:<sha>, commits:<N>, pr:<number>, PR URL, or omitted for all uncommitted changes."
    required: false
  - name: FOCUS
    description: "bugs, security, logic, api, tests, or all. Default: all."
    required: false
allowed_tools:
  - bash
  - read_file
  - grep
  - glob
  - git
---

# Review Changes

Review like a maintainer. Report only material issues: correctness, security,
data loss, API/contract breakage, concurrency/resource bugs, and meaningful test gaps.
Do not comment on style unless it hides a bug.

Hard rule: treat production control flow driven by natural-language text
matching as a correctness issue. Safety, admission, routing, blocking, retry,
recovery, evaluation, and state transitions must use typed fields such as enums,
`ErrorKind`, `result_class`, `exit_semantics`, structured JSON, protocol
parsers, AST/token parsers, or exact machine-owned sentinel fields. Text
matching is acceptable only for UI display/search, rendered-text tests, or
explicitly named legacy `fallback` parsers that are not the primary decision
path.

## Task

$ARGUMENTS

## Step 1: Resolve Diff

Start with status and a stat summary:

```bash
git status --short
git diff --stat
```

Target resolution:

| Target                    | Diff command                                                                    |
| ------------------------- | ------------------------------------------------------------------------------- |
| omitted / unstaged+staged | `git diff` plus `git diff --cached` when staged changes exist                   |
| `staged`                  | `git diff --cached`                                                             |
| `unstaged`                | `git diff`                                                                      |
| `branch:<name>`           | `git diff <name>...HEAD`                                                        |
| `commit:<sha>`            | `git show --stat <sha>` then `git show <sha>`                                   |
| `commits:<N>`             | `git log --oneline -N`, then `git show` for every one of the N commits          |
| `pr:<number>` / PR URL    | `gh pr diff` if `gh` is installed and authenticated; otherwise ask for the diff |

For a commit, take file/add/delete totals from Git's canonical summary:
`git show --shortstat --format= <sha>`. If a bounded file map needs `--numstat`,
also pass `--format=` so commit headers cannot be coerced into numeric totals by
an `awk`/parser pipeline. Do not replace Git's shortstat with hand-summed raw
`git show --numstat` output.

If the diff is empty after checking staged and unstaged changes, report "No changes found."

Hard rule for `commits:<N>`: cover all N commits before presenting a full review,
but inspect them in bounded commit/file sections instead of requesting one
monolithic diff. If only K of N are available, label the review partial before
findings.

If the user requests parallel reviews or multiple reviewers, start one fixed-size
parallel reviewer group immediately after the status/stat/name map. On Astra, use
the skill-exposed `agent_fanout` directly; do not spend a discovery round searching
for it and do not replace the group with multiple independent `agent` calls. On
another host, use its canonical parallel delegation capability or disclose that the
capability is unavailable. The parent coordinates coverage and validates results;
it must not perform a duplicate broad review while the reviewers are running.
Read-only reviewers may share immutable source. Use separate worktrees only when a
reviewer may mutate files or reviewers need different refs.

Keep review evidence bounded at the source. Prefer per-file hunks and exact line
ranges over full repository/commit dumps. If a tool returns an artifact reference,
do not pass `artifact://` to shell/file tools; rerun the evidence query with a
narrower scope or use the artifact owner supplied by the host.

## Step 2: Build Review Map

For broad diffs, group files by owner before reading details:

- Runtime/server: `crates/runtime/`
- Services/storage/tasks/journal: `crates/services/`
- Turn/tool/prompt core: `crates/astra-turn-*`, `crates/astra-prompts/`, `crates/astra-pipeline/`
- CLI/TUI: `crates/astra-cli/`
- Skills/docs: `.claude/skills/`, `.agent/skills/`, docs
- Frontend/SDK: `web/`, `packages/`

Then inspect the highest-risk files first:

1. Public API or schema changes.
2. Persistence writes, migrations, sync, restore.
3. Async/concurrency/cancellation code.
4. Auth/permissions/capability/tool visibility.
5. Error handling and retry paths.
6. Tests that claim coverage for the changed behavior.

For a refactor or subsystem addition, also build an ownership delta:

- Which implementation owned the behavior before and after?
- Did the change add another writer, status vocabulary, registry, observer,
  parser, table, compatibility path, or retry loop for an existing fact?
- Were replaced implementations and self-only tests deleted?
- Does a real CLI/server/web/edge entrypoint reach the new path?

Treat an unwired implementation, dual writable truth, or a replacement that
leaves the old owner active as a correctness finding. Do not recommend another
compensating layer until the canonical owner and retirement path are clear.

## Step 3: Validate Findings

Before reporting a finding:

- Confirm the changed path is reachable.
- Identify the concrete failure mode and user/system consequence.
- Check whether an existing test would catch it.
- Include file and line, using the new file line when possible.

Reviewer output is candidate evidence, not verified truth. Agreement between
reviewers does not raise confidence by itself. The parent must confirm each finding
against current source, a reachable failure sequence, and the actual ownership
boundary before calling it verified. Stop exploring once every changed ownership
boundary has evidence and every reported finding passes this gate.

Do not report:

- speculative risks without a reachable path;
- preferences;
- "consider" suggestions that do not block correctness;
- issues already fixed later in the same diff or commit range.

## Output Contract

Self-critique gate: before publishing the final report, re-scan the diff with
`git diff --check` to catch trailing whitespace and merge-conflict markers that
may have been introduced.

Findings first, ordered by severity:

```text
Critical:
- <file:line> <bug and concrete consequence>

Important:
- <file:line> <bug/test gap and why it matters>

Questions:
- <only blockers to judging correctness>

Summary:
- <one or two sentences on scope>

Tests:
- <tests inspected or missing>
```

If no material issues are found, say so directly and mention residual risk or unrun tests.
