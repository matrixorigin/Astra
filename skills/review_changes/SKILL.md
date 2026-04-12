---
name: review-changes
description: "Developer skill: context-aware code review of uncommitted changes, branch diffs, or specific commits in the astra codebase. Combines git diff with code intelligence (symbol extraction, call analysis, import resolution) for deep structural review."
user_invocable: true
when_to_use: "When the user asks to review code changes, commits, diffs, PRs, or says 'review latest commit'"
arguments:
  - name: TARGET
    description: "What to review: 'staged', 'unstaged', 'branch:<name>', 'commit:<sha>', or 'pr:<number>'. Default: all uncommitted changes."
    required: false
  - name: FOCUS
    description: "Review focus: 'bugs', 'security', 'logic', 'api', 'tests', or 'all' (default: all)"
    required: false
allowed_tools:
  - git_diff
  - git_status
  - git_show
  - git_log
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

**Efficiency rules:**
- Use `read_file` with line ranges instead of reading entire files
- Make **parallel** tool calls when reading multiple independent files
- Never read the same file twice — gather all needed ranges in one pass
- Use `git_diff` with `stat_only: true` FIRST to plan which files to deep-dive
- Use `git_diff` with `path: "file.rs"` for targeted per-file diffs instead of full repo diff

## Task

$ARGUMENTS

---

## Phase 1: Gather the Diff

### 1.1 Resolve TARGET

**⚠ CRITICAL: Use the built-in `git_diff`, `git_status`, `git_show`, `git_log` tools — NOT `bash` with raw git commands.** The built-in tools have automatic output truncation that prevents context explosion.

| TARGET | Tool call |
|--------|-----------|
| Default (uncommitted) | `git_diff` (no args) |
| `staged` | `git_diff` with `staged: true` |
| `unstaged` | `git_diff` (default is worktree vs HEAD) |
| `branch:<name>` | `git_diff` with `ref: "main"` (or the base branch) |
| `commit:<sha>` | `git_show` with the SHA |
| Stat overview | `git_diff` with `stat_only: true` |
| Filter by path | `git_diff` with `path: "rust/crates/..."` |

**When `git_diff` returns "No changes":**
1. Check `git_status` — are there staged changes? Try `git_diff` with `staged: true`
2. If still nothing, **ask the user** what they want reviewed. Do NOT automatically expand to branch diff against main — that can be hundreds of files.

### 1.2 Assess Scope Before Deep Dive

**⚠ CRITICAL workflow — always follow this exact sequence:**

1. **First call**: `git_diff` with `stat_only: true` to get the file list and line counts
2. **Then**: For each interesting file, call `git_show` or `git_diff` with `path: "specific/file.rs"` to get per-file diffs
3. **Never**: Request the full diff of all files at once — this wastes context

Scope guidelines based on the stat output:
- **Trivial** (<50 lines, 1-2 files): Quick inline review
- **Medium** (50-300 lines, 3-10 files): Per-file review with cross-file analysis
- **Large** (>300 lines, >10 files): Review only the most critical files (core logic, public API, largest hunks)

For large diffs, **do not** read every file. Focus on:
- Core logic changes (skip config, docs, generated files)
- Public API changes
- Files with the most line changes

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

Use the built-in `grep` tool for caller analysis:
```json
{"pattern": "function_name(", "path": "rust/crates/", "glob": "*.rs"}
```

### 2.2 Import/Dependency Changes

From the diff output, check import changes (`use`, `import`, `from`, `require`):
- Was a dependency added? Check `Cargo.toml` / `package.json` changes
- Was a module restructured? Check if old import paths still work
- Any circular dependency introduced?

### 2.3 Type/API Surface Changes

From the diff, identify public API changes (`pub fn`, `pub struct`, `pub enum`, `pub trait`, `export`):
- **Breaking change?** Removed parameter, changed type, removed variant
- **Semver impact?** Major (breaking), minor (additive), patch (internal)
- **Migration needed?** Find all callers that need updating

---

## Phase 3: Deep Review (Per-File)

For each file with core logic changes (skip trivial formatting):

### 3.0 Read Context Efficiently

**⚠ CRITICAL: NEVER read an entire large file.** Use line ranges from the diff hunk headers:

1. Parse the `@@ -45,7 +45,9 @@` markers from the diff output
2. Call `read_file` with `start_line` / `end_line` for ~30 lines around each hunk
3. For files >200 lines, use `outline: true` first to understand structure, then read specific ranges
4. If the diff touches multiple scattered hunks, make **parallel** `read_file` calls for each range

Example: if the diff shows `@@ -120,8 +120,12 @@`, call:
```json
{"path": "src/foo.rs", "start_line": 105, "end_line": 140}
```

**Merge commits:** `git_show` on a merge commit shows the combined first-parent diff.
If the output looks incomplete (e.g., no code hunks, only file renames), use
`git_diff` with `ref: "<first-parent-sha>"` to get the real code diff instead.

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

### 3.3 Test Coverage

For each changed function:
1. Does a test exist? Use `grep` to search for the function name in test files
2. Does the test cover the new/changed behavior?
3. Are edge cases tested?

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
- Is it marked `parallel_safe` if read-only? (see `plan_executor.rs`)
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

**⚠ CRITICAL: Do NOT start writing the review report until ALL analysis in Phases 1-5 is
complete. While you are still making tool calls to gather information, output NOTHING as text.
The review box below must be your FINAL text output — no tool calls after it.**

**⚠ NEVER wrap the review box in markdown code fences (triple backticks).** Output the
box-drawing characters directly — they render correctly without code fences. Code fences
cause rendering leakage in non-TTY / piped output modes.

**⚠ Gather ALL information BEFORE writing.** If you realize mid-review you need more data,
make the tool calls SILENTLY (no text output) before starting the review box. Any text you
produce between tool calls will be discarded and wastes tokens.

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
| Code intelligence | `rust/crates/astra-cli/src/edge_tools/code_intel.rs` |
| Git tools | `rust/crates/astra-cli/src/edge_tools/git_gix.rs` |
| Edge tools registry | `rust/crates/astra-cli/src/edge_tools.rs` |
| Tool parallel safety | `rust/crates/astra-cli/src/cli/plan_executor.rs` (parallel_safe) |
| Session journal schema | `rust/crates/services/src/session_journal.rs` |
| Durable task states | `rust/crates/services/src/durable_task.rs` |
