# Sibling-Abort Policy for Batched Tool Execution

**Scope:** `astra_turn_core::parallel_tool_exec::execute_parallel_round` and the
CLI batch dispatcher at `astra_cli::cli::stream_render::execute_tools_batch`.

**Status:** Active as of PR #227 (broadened from bash-only to any-mutating).

## 1. Current behavior

When the LLM returns a batch of tool calls in a single response, the executor
partitions them:

| Partition    | Concurrency    | Abort semantics                              |
|--------------|----------------|----------------------------------------------|
| Read-only    | Parallel, ≤ 10 | **None** — failures are independent, results reported individually |
| Mutating     | Strictly serial, ordered | **Any failure aborts all remaining siblings**; aborted results carry `success=false` and content `"Aborted: a prior tool in this batch failed."` |

Failure is detected solely via the `success: bool` returned by the executor
(the same signal used for journaling and health tracking).

## 2. Why "any mutating failure" is the right default

A batched mutating sequence is almost always a **coherent plan**:

```
write_file(src/foo.rs) → str_replace(src/foo.rs) → git_add(src/foo.rs) → git_commit
```

If `write_file` fails (permission, disk full, path invalid):

- `str_replace` operates on stale or missing content → noisy secondary error.
- `git_add` stages whatever is on disk → may commit a partial or unintended state.
- `git_commit` either fails noisily or — worse — succeeds with the wrong payload.

The blast radius of continuing exceeds the cost of a clean re-plan on the next
turn. Aborting surfaces the root cause early and keeps the journal/observability
signal focused on the real failure, which is what `SelfModel.tool_health` and
the feedback signals depend on.

This also matches the existing `ToolErrorSeverity` taxonomy in
`tool_result_semantics.rs`: any mutating-tool failure already classifies as
`HardError` (see the `is_mutation_tool` switch there), and hard errors already
trigger rollback elsewhere in the pipeline. Sibling-abort is the batch-level
analogue.

## 3. When sibling-abort is **too aggressive**

There are legitimate cases where a single mutating failure should **not** cancel
its siblings. All of them share one property: the failing tool's effect is
**scoped so narrowly** that later siblings cannot observe, inherit, or be
corrupted by it.

### 3.1 Independent-target bulk mutations

```
write_file(a.md) → write_file(b.md) → write_file(c.md)   # 3 unrelated files
```

A permission failure on `a.md` has no bearing on `b.md`. Aborting loses
partial progress. **Current policy:** abort anyway. **Justification:** the
cost of re-attempting two successful writes on the next turn is trivial; the
cost of papering over a systemic permission problem (e.g. `EROFS` on the
whole workspace) by letting later writes silently succeed is much higher.

### 3.2 Idempotent bookkeeping tools

```
record_feedback → record_metric → log_event
```

These never corrupt shared state and are safe to retry. They almost never fail,
and when they do the failure is typically transient (disk I/O, RPC blip).
**Current policy:** abort anyway. **Justification:** these tools are unlikely
to appear *inside* a mutating batch — they are usually independent edge calls.
If they do appear, a single-turn re-plan is cheap.

### 3.3 Best-effort cleanup

```
git_stash → delete_file(tmp/) → bash(rm -rf tmp/)
```

Later cleanups often "repair" earlier failures. **Current policy:** abort
anyway. **Justification:** a failed `git_stash` means the working tree is
*not* in the expected state; running `delete_file` on files the user may still
need is actively dangerous.

## 4. Why we do **not** introduce a suppression list today

Three options were considered:

1. **Per-tool `abort_on_sibling_failure: bool`** — a new field on the tool
   registry. Rejected: the correct value depends on *batch context*, not on
   the tool itself. `write_file` in a `write → commit` batch should abort; in
   a `write → write → write` batch it arguably should not.

2. **Severity-gated abort** — use `classify_tool_error` severity to decide.
   Rejected: severity is already used for rollback. Conflating the two makes
   the state machine harder to reason about, and the pipeline's journal-level
   rollback already catches HardError cases that aren't in a batch.

3. **Dependency graph between tool calls** — infer data dependencies from
   arguments (same file path, same repo). Rejected: prohibitively complex for
   the benefit (a handful of avoided aborts per week).

The **simple "any mutating failure aborts" rule** is:

- Easy to reason about — one line of code, one invariant.
- Easy to test — one regression test covers the whole surface.
- Conservative in the right direction — false aborts cost one retry turn;
  false continuations can corrupt the workspace.
- Cheap to change — if production telemetry shows unwanted aborts dominate,
  we can relax on a case-by-case basis guided by data.

## 5. Escape hatches available today

If a specific call site truly needs independent mutation semantics, there are
three sanctioned options **without** changing the batch abort policy:

1. **Split the batch across turns.** Ask the LLM to issue independent
   mutations in separate responses. The batching-prompt guidance added in
   PR #201 already explains when to batch vs. serialise; the inverse lesson
   is implicit.
2. **Wrap as read-only.** If the "mutation" is idempotent *and* truly
   independent (e.g. `touch` to pre-create files), wrap it behind a
   read-only-classified adapter tool. The parallel partition has no abort
   semantics.
3. **Use bash with explicit `||` / `&&`.** A single `bash` call embedding the
   intended dependency semantics is one tool invocation; the LLM controls
   failure propagation inside the shell.

## 6. Signals to watch before revisiting

Revisit this policy if any of the following become measurable:

- `tracing` event `astra::tool_batching::batch_size` shows batches of
  ≥ 3 mutating tools regularly (today's expectation: rare).
- The new regression test
  `unhappy_any_failing_mutating_tool_aborts_siblings` starts being routinely
  disabled by contributors (signals policy friction).
- Journal analytics show more than ~5% of turns where an aborted sibling
  would obviously have succeeded in isolation (need a counter; not yet
  instrumented).
- A specific tool (e.g. `record_feedback`) has `sibling_aborted=true` more
  often than `success=true` — it should be migrated to the read-only partition.

Until then, "abort on any mutating failure" stays the default.

## 7. Related code & docs

- `rust/crates/astra-turn-core/src/parallel_tool_exec.rs` — `execute_parallel_round`
- `rust/crates/astra-turn-core/src/tool_result_semantics.rs` — `ToolErrorSeverity`,
  `is_mutation_tool`
- `rust/crates/astra-pipeline/src/step_protocol.rs` — `classify_tool_idempotency`
- `rust/crates/astra-turn-core/tests/parallel_tool_exec_cap_test.rs` — regression
  coverage for both happy (5-tool parallel) and unhappy (bash-failure,
  write-failure) abort paths.
- PR #222 / #224 / #225 / #227 — the post-merge review follow-ups that
  established the current invariant.
