---
inclusion: always
---

# End-to-End Verification Rules

**Philosophy: Real API keys, real DB writes, real assertions. No mocks for E2E.**

---

## Running Verification

```bash
# Core scenarios — no LLM needed, just DB
make verify

# Full verification — includes NL→Script via real LLM
make verify-llm

# Talk verification — real CLI + API + LLM, multi-turn conversations
make verify-talk

# Run one case
make verify-talk CASE=memory_basic

# Specify model explicitly (default: cheapest by pricing)
make verify-llm MODEL=deepseek-chat
make verify-talk MODEL=kimi-k2.5

# Verbose output
make verify VERBOSE=1
make verify-talk VERBOSE=1
```

### First-Time LLM Setup

```bash
cp config/models.example.yaml .models.yaml
# Edit .models.yaml — fill in API keys and endpoint IDs
mo-admin model load .models.yaml
make verify-llm
```

### What It Verifies

| Command | What | Needs |
|---------|------|-------|
| `make verify` | Core data path (sandbox, commit, dry-run, discard, multi-turn) | MatrixOne |
| `make verify-llm` | + NL→Script via real LLM | + LLM key |
| `make verify-talk` | Full conversations: tool calls, DB state, response quality | + API server |

### Talk Verification Cases

Cases live in `scripts/e2e/cases/*.yaml`. Each case defines:
- Multi-turn conversation (user messages in sequence)
- Per-turn rule checks (hard pass/fail): tool called, DB state, response content
- Per-turn LLM judge (soft scoring): response quality against criteria
- Final checks: `session_integrity` (event chain, parent_event_id, status, event_count), `turn_count_increases`

```bash
scripts/e2e/cases/
├── memory_basic.yaml      # 记忆注入和召回
├── memory_correct.yaml    # 记忆纠正
├── memory_purge.yaml      # 记忆删除，召回时不再出现
└── multi_session.yaml     # 跨 session 记忆持久化
```

Available rule types in YAML:
- `tool_called: <name>` — tool was invoked
- `response_contains: <text>` — response includes text
- `response_not_contains: <text>` — response excludes text
- `response_contains_any: [a, b, c]` — response includes at least one
- `db: {table, where, assert: {count, fields}}` — DB state check
- `session_integrity: true` — verifies event chain, parent_event_id, status, event_count consistency
- `turn_count_increases: true` — verifies event count grew since last snapshot
- `new_session: true` on a turn — starts a fresh session (same user, new session_id)

To add a new case, create a YAML file following the format in existing cases.

### Data Safety

- Uses `__verify_<uuid>` user ID prefix — cannot collide with real users
- Auto-cleans before and after every run
- Lives in `scripts/e2e/`, not in any user-facing CLI
- Invoked via `make verify` only — developers only

---

## When to Do E2E Verification

After implementing or modifying any feature that touches:
- LLM calls (model routing, prompt changes)
- Memory write path (inject, correct, purge, batch)
- Audit trail
- Sandbox / experiment lifecycle
- Embedding generation
- CLI commands

**Do NOT skip E2E because "unit tests pass". Unit tests mock the DB. E2E proves the real path.**

---

## Core Principle: Follow the Data

Every feature touches multiple DB tables. Your job is to trace the data from the call site
all the way to every table it should have written, and verify each one.

**Ask yourself for every operation:**
1. What tables should have been written?
2. What fields should have been set, and to what values?
3. How do the rows across tables link to each other?
4. What should NOT have been written (e.g. sandbox → production table should be empty)?

Don't stop at "the row exists". Verify the fields that matter for correctness.

---

## Table Relationships to Understand

The memory system writes to several tables per operation. Understand how they connect:

- `mem_memories` — the canonical store. Every inject/correct produces rows here.
- `mem_edit_log` — the audit trail. Every mutation produces entries here, linked to `mem_memories` via `target_ids`.
- `mem_experiments` — sandbox lifecycle. Links to `mem_edit_log` via `snapshot_before`.
- `mem_user_memory_config` — per-user strategy. Written by `tune` operations.
- `memory_graph_nodes/edges` — graph index. Written only for `activation:v1` strategy users.
- `infra_llm_models` — model registry. Verify the model is active before NL→script tests.

**Key invariants:**
- `mem_edit_log.target_ids` must contain the `memory_id`s written to `mem_memories`
- Every `programmer.execute()` produces 2 audit entries: one from the editor (inject/correct/purge), one from the programmer (program)
- For sandbox runs: `mem_experiments.experiment_id` == `mem_edit_log.snapshot_before`
- `mem_memories.session_id` must equal the `session_id` passed into `execute()` — NULL if none was passed

---

## What "Enough" Verification Looks Like

**Not enough:**
```python
assert result.actions_executed == 1   # only checks return value
assert row is not None                # only checks existence
```

**Enough:** re-query from DB (not from return value), then verify every field that the
feature is responsible for setting. Check nulls explicitly. Check that unrelated rows
were not accidentally modified.

---

## Adding New Verification Scenarios

When adding a new feature, add a corresponding scenario to `scripts/e2e/verify_cli.py`:

1. Create a `test_<feature>()` function
2. Use `__verify_` user ID (already set up)
3. Verify DB state with `query_one()` / `count()` helpers
4. Use `check(name, condition, msg)` for assertions
5. Register it in `main()` — before or after `--with-llm` gate as appropriate

---

## Common Pitfalls

| Symptom | Root cause to check |
|---|---|
| `session_id` NULL when it shouldn't be | Was it passed all the way from EdgeTool → programmer → editor? |
| `embedding` NULL | Is `embed_client` wired up in `create_editor()`? |
| `source_event_ids = '[]'` | Did the write bypass `editor.inject()` with direct SQL? |
| Only 1 audit entry instead of 2 | `_log_program_audit()` not called, or called before editor logs |
| `branch_db` identifier error | Contains hyphens — must use `generate_id()`, not `user_id[:N]` |
| Sandbox data in production | `_make_branch_editor()` using wrong `db_factory` |
