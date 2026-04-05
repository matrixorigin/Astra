---
name: verify-task
description: "Developer skill: verify that a completed task actually works. Runs astra's verification engine (8 verifier types), generates acceptance criteria from task context, executes build/test/lint/grep checks, and produces a delivery report."
user_invocable: true
arguments:
  - name: TASK
    description: "What to verify: 'last' (most recent task), task ID, or natural language description of what should work."
    required: false
  - name: SCOPE
    description: "Verification scope: 'quick' (build+test only), 'full' (all criteria), 'custom' (user specifies checks). Default: full"
    required: false
allowed_tools:
  - bash
  - read_file
  - grep
  - glob
---
# Verify Task

Verify that a completed task actually works by running astra's verification framework.
This skill either uses existing acceptance criteria from a task contract, or generates
appropriate checks from the task context. Produces a structured delivery report.

**Philosophy**: "Done" means verified, not just "code was written." Every task completion
should have evidence that it works.

## Task

$ARGUMENTS

---

## Phase 1: Determine What to Verify

### 1.1 Resolve TASK

| TASK type | Action |
|-----------|--------|
| `"last"` or omitted | Find most recent task from journal PlanProgress events |
| Task/contract ID | Look up in journal or cloud task_contracts table |
| Natural language | Infer what should work from the description |

```bash
# Find recent plan execution in journal
grep '"PlanProgress"\|"DelegationCompleted"\|"VerificationCompleted"' \
  ~/.astra/sessions/*.jsonl 2>/dev/null | tail -10

# Or check git log for recent task-related commits
git --no-pager log --oneline -20
```

### 1.2 Gather Task Context

Collect everything needed to understand what should be verified:

1. **From journal**: PlanProgress events with subtask IDs and completion status
2. **From git**: Recent commits (what files changed, what was the intent)
3. **From working directory**: Current state of modified files

```bash
# What changed recently?
git --no-pager diff HEAD~5 --stat
git --no-pager log --oneline -5

# What files are in play?
git --no-pager diff HEAD~5 --name-only | sort -u
```

### 1.3 Find or Generate Acceptance Criteria

**If task contract exists** (from durable task system):
- Load `VerificationCriterion` list from contract
- Each has: id, description, verifier kind, required flag, timeout

**If no contract** (ad-hoc task):
- Generate criteria from context using the rules below

---

## Phase 2: Generate Verification Criteria

When no formal contract exists, generate criteria based on what changed.
Use astra's 8 `VerifierKind` types:

### 2.1 Always-Run Checks (Global)

These run for every task, regardless of scope:

```yaml
- id: global-build
  description: "Project builds without errors"
  verifier: BuildPass
  cmd: <detected build command>
  required: true

- id: global-test
  description: "All existing tests pass"
  verifier: TestPass
  cmd: <detected test command>
  min_pass_rate: 1.0
  required: true
```

**Build command detection** (check in order):
```bash
# Rust
[ -f Cargo.toml ] && echo "cargo build --workspace"

# Node.js
[ -f package.json ] && grep -q '"build"' package.json && echo "npm run build"

# Python
[ -f setup.py ] && echo "python setup.py build"
[ -f pyproject.toml ] && echo "python -m build"

# Go
[ -f go.mod ] && echo "go build ./..."

# Make
[ -f Makefile ] && echo "make build"
```

**Test command detection:**
```bash
# Rust
[ -f Cargo.toml ] && echo "cargo test --workspace"

# Node.js
[ -f package.json ] && grep -q '"test"' package.json && echo "npm test"

# Python
[ -d tests ] && echo "pytest tests/"

# Go
[ -f go.mod ] && echo "go test ./..."
```

### 2.2 Context-Specific Checks

Based on what files changed, generate targeted checks:

**New files created:**
```yaml
- id: file-exists-{name}
  description: "New file {path} exists"
  verifier: FileExists
  paths: ["{path}"]
  required: true
```

**Rust code changes:**
```yaml
- id: lint-clean
  description: "No new clippy warnings"
  verifier: Command
  cmd: "cargo clippy --workspace -- -D warnings"
  expected_exit: 0
  required: false  # Warning, not blocking

- id: no-unsafe
  description: "No new unsafe blocks without safety comments"
  verifier: GrepCheck
  file: "{changed_file}"
  pattern: "unsafe {"
  should_match: false  # Only if file didn't have unsafe before
```

**Test files changed:**
```yaml
- id: new-tests-pass
  description: "New/modified tests pass"
  verifier: TestPass
  cmd: "cargo test --package {package} -- {test_name}"
  min_pass_rate: 1.0
  required: true
```

**API/interface changes:**
```yaml
- id: api-compat
  description: "Existing callers still compile"
  verifier: BuildPass
  cmd: "cargo check --workspace"
  required: true
```

**Documentation changes:**
```yaml
- id: docs-valid
  description: "Documentation builds without errors"
  verifier: Command
  cmd: "cargo doc --no-deps --workspace"
  expected_exit: 0
  required: false
```

### 2.3 Astra-Specific Checks

For changes to the astra codebase itself:

```yaml
# If edge_tools.rs changed
- id: tool-registration
  description: "All tools registered and schemas valid"
  verifier: Command
  cmd: "cargo test --package astra-cli -- tool_schema"
  expected_exit: 0

# If journal/event types changed
- id: journal-compat
  description: "Journal serialization backward compatible"
  verifier: Command
  cmd: "cargo test --package astra-services -- journal"
  expected_exit: 0

# If durable_task.rs changed
- id: durable-task-tests
  description: "Durable task system tests pass"
  verifier: TestPass
  cmd: "cargo test --package astra-services -- durable"
  min_pass_rate: 1.0
```

---

## Phase 3: Execute Verification

### 3.1 Run Each Criterion

Execute verifications in dependency order:
1. **Build checks first** (if build fails, skip downstream)
2. **Test checks second** (run test suite)
3. **File/grep checks** (structural verification)
4. **Lint checks last** (non-blocking)

For each criterion:

```bash
# BuildPass / TestPass / Command
timeout {timeout_sec} bash -c '{cmd}' 2>&1
echo "EXIT_CODE=$?"

# FileExists
for path in {paths}; do
  [ -f "$path" ] && echo "✔ $path exists" || echo "✘ $path MISSING"
done

# GrepCheck
grep -n '{pattern}' '{file}' 2>/dev/null
# should_match=true → exit 0 means pass
# should_match=false → exit 0 means FAIL (pattern found when it shouldn't be)

# CommandOutput
output=$({cmd} 2>&1)
for needle in {contains}; do
  echo "$output" | grep -q "$needle" || echo "MISSING: $needle"
done
for needle in {not_contains}; do
  echo "$output" | grep -q "$needle" && echo "UNEXPECTED: $needle"
done
```

### 3.2 Record Results

For each criterion, record:
```
{
  "criterion_id": "...",
  "passed": true/false,
  "evidence": "<actual output>",
  "expected": "<what was expected>",
  "duration_ms": <execution time>,
  "error": "<error message if failed>"
}
```

### 3.3 Handle Failures

When a criterion fails:
1. **Capture full output** (stdout + stderr) as evidence
2. **Analyze failure** — is it a real issue or test flake?
3. **Check if pre-existing** — did this test fail before the change?

```bash
# Quick check: was this test already failing?
git stash && {test_cmd} 2>&1 | tail -5; git stash pop
```

---

## Phase 4: Delivery Report

### 4.1 Summary View

```
╔══════════════════════════════════════════════════════════════╗
║  🔬 Task Verification Report                                 ║
║  Task: {task_description}                                    ║
║  Status: {✅ Verified / ⚠️ Partial / ❌ Failed}              ║
╠══════════════════════════════════════════════════════════════╣
║                                                              ║
║  📋 Criteria Results                                         ║
║  ├─ ✅ global-build: Project builds (1.2s)                   ║
║  ├─ ✅ global-test: All tests pass — 516/516 (8.3s)         ║
║  ├─ ✅ file-exists: New module created                       ║
║  ├─ ❌ lint-clean: 2 new clippy warnings                    ║
║  └─ ✅ api-compat: All callers compile                       ║
║                                                              ║
║  📊 Summary                                                  ║
║  ├─ Required: {passed}/{total} passed                        ║
║  ├─ Optional: {passed}/{total} passed                        ║
║  └─ Duration: {total_time}                                   ║
║                                                              ║
║  {if failures:}                                              ║
║  ❌ Failures Detail                                          ║
║  ├─ lint-clean:                                              ║
║  │  Expected: No warnings                                    ║
║  │  Actual: warning[clippy::needless_return] at src/foo.rs:42║
║  │  Action: Remove explicit return                           ║
║  └─ ...                                                      ║
║                                                              ║
╚══════════════════════════════════════════════════════════════╝
```

### 4.2 Verdict Logic

| Condition | Verdict |
|-----------|---------|
| All required criteria pass | ✅ **Verified** |
| All required pass, some optional fail | ⚠️ **Verified with warnings** |
| Any required criterion fails | ❌ **Verification failed** |
| Verification itself errors (timeout, crash) | ⚠️ **Inconclusive** |

### 4.3 Failure Triage

For each failed criterion, provide:
1. **What failed**: Criterion description + verifier type
2. **Evidence**: Actual output vs expected
3. **Root cause**: Quick analysis of why it failed
4. **Fix suggestion**: Specific action to resolve
5. **Pre-existing?**: Was this failing before the task?

---

## Phase 5: Quick vs Full Scope

### Quick Mode (`SCOPE=quick`)

Only run:
- `global-build` (project compiles)
- `global-test` (test suite passes)

Use for rapid iteration — verify basics before deep review.

### Full Mode (`SCOPE=full`, default)

Run all generated criteria:
- Build + test + lint
- File existence checks
- Grep/pattern checks
- API compatibility
- Astra-specific checks (if applicable)

### Custom Mode (`SCOPE=custom`)

Ask the user what to verify, then generate criteria dynamically.
Use this when the standard checks don't cover the task's acceptance criteria.

---

## Astra Verification Engine Reference

The verification framework lives in `durable_task.rs` and supports:

| VerifierKind | What It Does | When to Use |
|-------------|-------------|-------------|
| `Command` | Run cmd, check exit code | Build, lint, any CLI tool |
| `CommandOutput` | Run cmd, check stdout contains/not-contains | Output validation |
| `FileExists` | Check file paths exist | New file creation tasks |
| `GrepCheck` | Grep pattern in file | Code pattern verification |
| `BuildPass` | Build command (exit 0) | Always-run global check |
| `TestPass` | Test command with pass rate | Always-run global check |
| `LlmJudge` | LLM semantic evaluation | Subjective quality checks |
| `Composite` | AND/OR of sub-criteria | Complex acceptance |

---

## Reference: Key Source Files

| Component | File |
|-----------|------|
| VerificationRunner | `rust/crates/services/src/durable_task.rs` |
| VerifierKind enum | `rust/crates/services/src/durable_task.rs` |
| Delivery report display | `rust/crates/mo-agent/src/mo_agent/durable_bridge.rs` |
| Plan executor | `rust/crates/mo-agent/src/mo_agent/plan_executor.rs` |
| Build/test detection | `rust/crates/mo-agent/src/edge_tools.rs` |
| Session journal | `rust/crates/services/src/session_journal.rs` |
