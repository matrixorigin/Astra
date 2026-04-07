---
name: batch-parallel
description: "Execute a batch of independent tasks in parallel using git worktrees for isolation. Each task runs in its own worktree, preventing file conflicts. Results are merged back into the main branch."
version: "1.0.0"
user_invocable: true
arguments:
  - name: TASKS
    description: "Description of the batch tasks to execute. Can be a list of items, a pattern, or a high-level goal to be decomposed."
    required: true
  - name: WORKERS
    description: "Number of parallel workers (default: 3, max: 8)"
    required: false
  - name: STRATEGY
    description: "Execution strategy: 'independent' (no cross-task deps), 'sequential-merge' (each builds on prior), 'fan-out-merge' (work then combine). Default: independent"
    required: false
triggers:
  - batch
  - parallel
  - bulk
  - many
  - concurrent
  - worktree
allowed_tools:
  - bash
  - read_file
  - write_file
  - str_replace
  - grep
  - glob
  - git_worktree
  - git_commit
  - git_diff
  - git_log
when_to_use: "When the user wants to perform multiple independent code changes, refactors, or tasks in parallel — especially when tasks touch different files and benefit from isolation."
model: null
max_tokens: 32768
context: fork
category: "automation"
tags:
  - parallel
  - batch
  - worktree
  - automation
  - refactor
---

# Batch Parallel Execution

Execute multiple independent tasks in parallel using git worktrees for full filesystem
isolation. Each worker operates in its own worktree branch, preventing conflicts between
concurrent file modifications.

## Overview

```
Main Branch ──┬── Worker 1 (worktree-1) ── task A ── commit ──┐
              ├── Worker 2 (worktree-2) ── task B ── commit ──┤── Merge ── Done
              └── Worker 3 (worktree-3) ── task C ── commit ──┘
```

## Task

$ARGUMENTS

---

## Phase 1: Task Decomposition

### 1.1 Parse the batch request

Analyze `$TASKS` and decompose into discrete, independent work items.
Each item must be self-contained — it should not depend on changes from another item.

```
For each item, define:
- ID: short kebab-case identifier (e.g., "fix-auth-handler")
- Description: what to do
- Files: expected files to touch (if known)
- Acceptance: how to verify it's done
```

### 1.2 Validate independence

Check that no two items touch the same file. If they do:
- If STRATEGY is `independent`: warn the user and suggest splitting differently
- If STRATEGY is `sequential-merge`: order them so later items build on earlier
- If STRATEGY is `fan-out-merge`: proceed but plan a manual merge step

### 1.3 Determine worker count

```
WORKERS = min($WORKERS or 3, number_of_items, 8)
```

## Phase 2: Worktree Setup

### 2.1 Create worktrees

For each worker, create an isolated worktree:

```bash
# Get current branch
CURRENT_BRANCH=$(git rev-parse --abbrev-ref HEAD)

# For each worker i:
git worktree add ../batch-worker-{i} -b batch/{item_id} $CURRENT_BRANCH
```

Use the `git_worktree` tool with action `add` for each worker.

### 2.2 Verify setup

List worktrees to confirm all are created:

```bash
git worktree list
```

## Phase 3: Parallel Execution

### 3.1 Execute tasks

For each worker/task pair, execute the task in the worker's worktree directory.

**Critical rules:**
- Always `cd` into the worktree directory before any file operations
- Use absolute paths when referencing worktree files
- Each task should end with a commit in its worktree branch
- If a task fails, log the error but continue with other tasks

### 3.2 Commit results

In each worktree, stage and commit the changes:

```bash
cd ../batch-worker-{i}
git add -A
git commit -m "batch: {item_id} — {description}"
```

## Phase 4: Merge & Cleanup

### 4.1 Merge results back

For each completed worker branch:

```bash
# Return to main worktree
cd {original_directory}

# Merge worker branch
git merge --no-ff batch/{item_id} -m "merge: batch/{item_id}"
```

If merge conflicts occur:
- For `independent` strategy: resolve automatically if possible, skip if not
- For `fan-out-merge`: attempt resolution, report conflicts to user
- Always report which items merged cleanly and which had issues

### 4.2 Clean up worktrees

Remove all batch worktrees:

```bash
git worktree remove ../batch-worker-{i}
git branch -d batch/{item_id}
```

Use the `git_worktree` tool with action `remove` and `delete_branch: true`.

### 4.3 Verify final state

```bash
# Ensure build still passes
cargo check  # or npm run build, etc.

# Ensure tests pass
cargo test   # or npm test, etc.

# Show summary of all changes
git log --oneline -N  # where N = number of merged items
```

## Phase 5: Report

Produce a summary table:

```
╔═══════════════════════════════════════════════╗
║  Batch Execution Report                       ║
╠═══════════════════════════════════════════════╣
║  Total items:    N                            ║
║  Succeeded:      X                            ║
║  Failed:         Y                            ║
║  Merge conflicts: Z                           ║
╠═══════════════════════════════════════════════╣
║  Item         Status    Branch         Files  ║
║  ──────────── ──────── ────────────── ─────── ║
║  fix-auth     ✅ done   batch/fix-auth   3    ║
║  add-tests    ✅ done   batch/add-tests  5    ║
║  update-docs  ❌ fail   batch/update-..  0    ║
╚═══════════════════════════════════════════════╝
```

## Error Handling

- **Worktree creation fails**: Fall back to sequential execution in main branch
- **Task execution fails**: Log error, mark as failed, continue others
- **Merge conflict**: Report conflicting files, offer manual resolution or skip
- **Build fails after merge**: Bisect to find which merge broke the build

## Guidelines

- Never modify files in the main worktree while workers are active
- Always clean up worktrees even if tasks fail (use finally-style cleanup)
- Prefer small, focused tasks over large sweeping changes
- Report progress as each worker completes (don't wait for all to finish)
- If the project has a lockfile (Cargo.lock, package-lock.json), be careful with concurrent installs
