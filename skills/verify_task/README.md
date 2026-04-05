# verify-task

Verify that a completed task actually works using **astra's verification engine**.
Runs build/test/lint/grep checks and produces a structured delivery report with evidence.

## Usage

```
/skill verify-task
/skill verify-task --scope quick
/skill verify-task --task "Implement auth module" --scope full
/skill verify-task --task last --scope custom
```

## Philosophy

**"Done" means verified.** Every task completion should have evidence:
- Build passes ✅
- Tests pass ✅
- New files exist ✅
- Code patterns present/absent ✅
- Lint clean ✅

## Verification Scopes

| Scope | What It Runs |
|-------|-------------|
| **quick** | Build + test suite only (fast iteration) |
| **full** | All criteria: build, test, lint, file existence, grep patterns, API compat |
| **custom** | User specifies what to check |

## Verifier Types (8)

Maps to astra's `VerifierKind` enum in `durable_task.rs`:

| Type | What It Does | Example |
|------|-------------|---------|
| `Command` | Run cmd, check exit code | `cargo build` exits 0 |
| `CommandOutput` | Run cmd, check stdout | Output contains "200 OK" |
| `FileExists` | Check paths exist | `src/auth/mod.rs` exists |
| `GrepCheck` | Pattern in file | `pub fn login` in auth.rs |
| `BuildPass` | Build succeeds | `cargo build --workspace` |
| `TestPass` | Tests pass (with min rate) | 516/516 tests pass |
| `LlmJudge` | LLM semantic evaluation | "Code follows project conventions" |
| `Composite` | AND/OR of sub-criteria | Build AND test AND lint |

## Output

Structured delivery report:
- **Verdict**: ✅ Verified / ⚠️ Partial / ❌ Failed
- **Per-criterion results** with evidence and duration
- **Failure detail** with expected vs actual + fix suggestions
- **Pre-existing check** — distinguishes new failures from old ones
