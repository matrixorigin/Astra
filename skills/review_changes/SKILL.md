---
name: review-changes
description: "Developer skill: context-aware code review of uncommitted changes, branch diffs, or specific commits in the astra codebase. Combines git diff with code intelligence (symbol extraction, call analysis, import resolution) for deep structural review."
user_invocable: true
arguments:
  - name: TARGET
    description: "What to review: 'staged', 'unstaged', 'branch:<name>', 'commit:<sha>', or 'pr:<number>'. Default: all uncommitted changes."
    required: false
  - name: FOCUS
    description: "Review focus: 'bugs', 'security', 'logic', 'api', 'tests', or 'all' (default: all)"
    required: false
allowed_tools:
  - bash
  - read_file
  - grep
  - glob
---
# Review Changes

Context-aware code review combining git diffs with astra's code intelligence system.
Goes beyond line-by-line diff review — understands symbol relationships, import changes,
trait implementations, and call graph impact.

**Signal-to-noise philosophy**: Only surface issues that genuinely matter — bugs, security
vulnerabilities, logic errors, API breakage, missing tests. Never comment on style,
formatting, or trivial matters.

## Task

$ARGUMENTS

---

## Phase 1: Gather the Diff

### 1.1 Resolve TARGET

```bash
# Default: all uncommitted changes
git --no-pager diff HEAD --stat
git --no-pager diff HEAD

# Staged only
git --no-pager diff --cached --stat
git --no-pager diff --cached

# Unstaged only
git --no-pager diff --stat
git --no-pager diff

# Branch diff (against main/master)
git --no-pager diff main...<branch> --stat
git --no-pager diff main...<branch>

# Specific commit
git --no-pager show <sha> --stat
git --no-pager show <sha>

# PR (needs gh cli)
gh pr diff <number>
```

### 1.2 Build File Inventory

From the diff stat, categorize changed files:

```
| File | Language | Lines +/- | Category |
|------|----------|-----------|----------|
```

Categories:
- **Core logic**: `src/`, `lib/`
- **Tests**: `tests/`, `*_test.*`, `test_*.*`
- **Config**: `Cargo.toml`, `package.json`, `.toml`, `.yaml`
- **Docs**: `*.md`, `docs/`
- **Build**: `Makefile`, `Dockerfile`, `build.rs`

### 1.3 Assess Scope

- **Trivial** (<50 lines, 1-2 files): Quick inline review
- **Medium** (50-300 lines, 3-10 files): Per-file review with cross-file analysis
- **Large** (>300 lines, >10 files): Structural review first, then targeted deep dives

---

## Phase 2: Structural Analysis

### 2.1 Symbol-Level Impact Assessment

For each changed file, use astra's code intelligence to extract symbols:

**What astra's code_intel.rs provides** (9 languages supported):
- `extract_symbols()`: All functions, methods, types, traits, constants
- `extract_calls()`: Function call sites with argument counts
- `extract_imports()`: Import statements with resolved paths
- `find_rust_impls()`: Trait implementations (Rust-specific)
- `extract_members()`: Struct/class member fields and types

**Analysis steps:**

1. **Extract symbols from changed hunks only** — what functions/types were modified?
2. **Check if changed symbols are public API** — `pub fn`, `export`, etc.
3. **Find callers of changed symbols** — grep for function names across codebase
4. **Check if signature changed** — parameter types, return type, generics

```bash
# For each modified function, find callers
grep -rn "function_name(" --include="*.rs" rust/crates/ | grep -v "test" | head -20

# For Rust: check if trait impl changed
grep -rn "impl.*TraitName.*for" --include="*.rs" rust/crates/
```

### 2.2 Import/Dependency Changes

```bash
# Extract import changes from diff
git --no-pager diff HEAD | grep -E "^[+-].*(use |import |from |require\()" | head -30
```

For each changed import:
- Was a dependency added? Check `Cargo.toml` / `package.json` changes
- Was a module restructured? Check if old import paths still work
- Any circular dependency introduced?

### 2.3 Type/API Surface Changes

For Rust files specifically:
```bash
# Find pub items that changed
git --no-pager diff HEAD -- "*.rs" | grep -E "^[+-]\s*(pub\s+(fn|struct|enum|trait|type|const|static))" | head -20
```

For each public API change:
- **Breaking change?** Removed parameter, changed type, removed variant
- **Semver impact?** Major (breaking), minor (additive), patch (internal)
- **Migration needed?** Find all callers that need updating

---

## Phase 3: Deep Review (Per-File)

For each file with core logic changes (skip trivial formatting):

### 3.1 Bug Detection

Check the diff hunks for:

**Logic errors:**
- Off-by-one: loop bounds, array indexing, range expressions
- Null/None handling: unwrap() without check, missing Option handling
- Type coercion: lossy casts (as u32, parseInt without validation)
- Boundary conditions: empty collections, zero values, overflow

**Concurrency issues (Rust-specific):**
- Arc/Mutex usage: deadlock potential, lock ordering
- async/await: missing .await, holding lock across await point
- Channel usage: unbounded channels, dropped receivers

**Error handling:**
- `unwrap()` or `expect()` on fallible operations in non-test code
- Swallowed errors: `let _ = result;` or `if let Ok(x) = ...`
- Error type changes: new error variant without handling in callers

### 3.2 Security Review

Check for:
- **Command injection**: User input passed to `Command::new()` or bash
- **Path traversal**: User-controlled paths without sanitization
- **Credential exposure**: Hardcoded tokens, API keys, passwords
- **SQL injection**: String interpolation in SQL queries (relevant for MatrixOne)
- **Unsafe code**: New `unsafe` blocks without safety comments

```bash
# Quick security scan on changed files
git --no-pager diff HEAD --name-only | xargs grep -n "unsafe\|unwrap()\|Command::new\|exec\|eval" 2>/dev/null | head -20
```

### 3.3 Test Coverage

For each changed function:
1. Does a test exist? Search `tests/` and `#[test]` / `#[tokio::test]`
2. Does the test cover the new/changed behavior?
3. Are edge cases tested?

```bash
# Find tests for changed functions
for func in <changed_functions>; do
  grep -rn "$func" --include="*.rs" rust/crates/ | grep "#\[test\]" -A 5 | head -10
done
```

Flag:
- 🔴 Public API change with no test update
- 🟡 New code path with no test
- 🟢 Test exists and covers the change

---

## Phase 4: Cross-File Consistency

### 4.1 Interface Contracts

If a trait/interface was modified:
- Do all implementations conform to the new contract?
- Are default implementations updated?
- Are mock implementations in tests updated?

### 4.2 Error Propagation

If error types changed:
- Are all `?` operators still valid?
- Do error conversions (From/Into) still compile?
- Are error messages helpful?

### 4.3 Configuration Consistency

If config structures changed:
- Are default values sensible?
- Are config files (TOML/YAML/JSON) updated to match?
- Is backward compatibility maintained? (old config files still load)

### 4.4 Documentation Consistency

If public API changed:
- Are doc comments updated?
- Are README/docs files updated?
- Are examples still correct?

---

## Phase 5: Astra-Specific Checks

### 5.1 Tool Registration

If a new tool was added or tool schema changed:
- Is it registered in `edge_tools.rs` tool list?
- Is it in the appropriate tool category for selection?
- Is it in `PARALLEL_SAFE_TOOLS` if read-only? (34 tools currently)
- Does the tool schema match the implementation?

### 5.2 Journal Event Changes

If `JournalEvent` or `JournalEventType` was modified:
- Is backward compatibility maintained for existing `.jsonl` files?
- Are new fields optional (with defaults)?
- Is cloud event ingestion updated?

### 5.3 State Machine Changes

If `SubtaskStage` or plan execution flow changed:
- Are all state transitions valid?
- Are new states handled in display code?
- Are serialization/deserialization updated?

### 5.4 Cloud Sync Impact

If cloud-synced data structures changed:
- Are SQL schema migrations needed?
- Is the sync adapter updated?
- Are older cloud records still readable?

---

## Phase 6: Review Report

```
╔══════════════════════════════════════════════════════════════╗
║  📝 Code Review: {target_description}                        ║
║  Scope: {n} files, +{added}/-{removed} lines                ║
╠══════════════════════════════════════════════════════════════╣
║                                                              ║
║  🔴 Critical ({n})                                           ║
║  {issue with file:line and explanation}                      ║
║                                                              ║
║  🟡 Important ({n})                                          ║
║  {issue with file:line and explanation}                      ║
║                                                              ║
║  💡 Suggestions ({n})                                        ║
║  {non-blocking improvements}                                 ║
║                                                              ║
║  ✅ Looks Good                                               ║
║  {aspects of the change that are well done}                  ║
║                                                              ║
║  📊 Impact Assessment                                        ║
║  ├─ Public API changes: {n} ({breaking/additive/internal})   ║
║  ├─ Test coverage: {adequate/needs-work/missing}             ║
║  ├─ Cross-file impact: {files affected by changes}           ║
║  └─ Semver: {major/minor/patch}                              ║
║                                                              ║
╚══════════════════════════════════════════════════════════════╝
```

### Issue Severity Guide

| Severity | Criteria | Action |
|----------|----------|--------|
| 🔴 Critical | Bugs, security issues, data loss, API breakage | Must fix before merge |
| 🟡 Important | Missing tests, error handling gaps, logic concerns | Should fix |
| 💡 Suggestion | Performance, readability, alternative approaches | Nice to have |
| ✅ Looks Good | Acknowledge well-written code, good patterns | Positive feedback |

**Rules:**
- Never mention formatting, whitespace, or style (rustfmt handles that)
- Never suggest renaming unless the name is actively misleading
- Every issue must include the specific file and line number
- Every critical/important issue must explain **why** it's a problem
- Suggestions must explain the **benefit** of the change

---

## Reference: Key Source Files

| Component | File |
|-----------|------|
| Code intelligence | `rust/crates/mo-agent/src/edge_tools/code_intel.rs` |
| Git tools | `rust/crates/mo-agent/src/edge_tools/git_gix.rs` |
| Edge tools registry | `rust/crates/mo-agent/src/edge_tools.rs` |
| Tool parallel safety | `rust/crates/mo-agent/src/mo_agent/chat_stream.rs` (PARALLEL_SAFE_TOOLS) |
| Session journal schema | `rust/crates/services/src/session_journal.rs` |
| Durable task states | `rust/crates/services/src/durable_task.rs` |
