# astra-test-harness

Declarative end-to-end test framework for the astra CLI. Runs YAML
cases against one or more models, captures session state (journal +
step_events + stderr), evaluates success criteria (deterministic
matchers + optional LLM judger), and emits a scored report.

## Why a dedicated harness

Unit and integration tests prove code correctness; this harness
proves *end-to-end behavior* — that a model, wired through the astra
CLI against a running server, produces the expected tool-call
sequence, session state, and performance characteristics. The
existing runtime tests exercise components in isolation; the harness
exercises the whole binary against real provider keys.

## Installation

The harness ships as a workspace binary named `astra-test`. Build
once:

```sh
cargo build -p astra-test-harness --release
```

Or use the Makefile target that does the build + a default smoke run:

```sh
make test-harness MODELS=qwen-flash
```

## Quick start

```sh
# Local smoke: one case, no judger, parallel 4
./rust/target/release/astra-test \
    --suite rust/crates/astra-test-harness/cases \
    --force-model qwen-flash \
    --no-judger --parallel 4

# Cross-family judger (recommended: different family than tested models)
./rust/target/release/astra-test \
    --suite rust/crates/astra-test-harness/cases \
    --models MiniMax-M2.7 \
    --judger-model us.anthropic.claude-sonnet-4-6 \
    --judger-n 3 --judger-agg median

# Filter to specific cases, repeat 3 times for flakiness detection
./rust/target/release/astra-test \
    --suite rust/crates/astra-test-harness/cases \
    --filter fork_prefix \
    --force-model us.anthropic.claude-sonnet-4-6 \
    --runs 3 --parallel 2
```

### `--astra-bin` resolution

The harness auto-detects the astra binary in this order:

1. `--astra-bin <PATH>` CLI flag.
2. `ASTRA_BIN` environment variable.
3. `astra` on `$PATH`.
4. `rust/target/release/astra` relative to the nearest Cargo
   workspace root above CWD.

The chosen path is logged to stderr. Fail-fast with an actionable
error when nothing resolves.

## Writing cases

A case is one YAML file. Example:

```yaml
name: fork_prefix_hit_end_to_end
description: |
  Spawn a child with inherit_prefix and assert a fork-cache HIT.
prompt: |
  You have a spawn_agent tool. Spawn child "G1", task
  "Reply: inherited-ok", inherit_prefix: {}. Surface the reply.
debug_log: true            # turn on session journal capture
timeout_seconds: 240
criteria:
  - type: exit_code
    code: 0
  - type: tool_called
    name: spawn_agent
  - type: tool_sequence
    tools: [spawn_agent]
  - type: tokens_between
    min: 0
    max: 5000
  - type: turn_rounds_between
    min: 1
    max: 5
  - type: duration_between
    min_ms: 0
    max_ms: 120000
  - type: stderr_matches
    pattern: '\[fork-cache\]'
  - type: fork_cache_outcome
    expect: [hit]
  - type: judger
    question: Did the child's reply appear in the final answer?
    threshold: 0.7
```

### Criteria types

| Type | What passes | Data source |
|------|-------------|-------------|
| `exit_code { code }` | subprocess exit equals `code` | envelope |
| `tool_called { name }` | `tools_used` envelope contains `name` | envelope |
| `tools_count_between { min, max }` | `tool_calls_count` inclusive | envelope |
| `tool_sequence { tools }` | `tools_used` contains tools as ordered subsequence | envelope |
| `tokens_between { min, max }` | total tokens (prompt + completion) in range | envelope |
| `duration_between { min_ms, max_ms }` | wall-clock duration in range | envelope |
| `turn_rounds_between { min, max }` | LLM round-trips (StepStarted events) in range | step_events |
| `cache_rate_above { threshold }` | tool cache hit rate ≥ threshold (0.0–1.0) | step_events |
| `stderr_matches { pattern }` | multi-line regex on stderr | stderr |
| `text_contains { needle }` | substring in final text | envelope |
| `fork_cache_outcome { expect }` | `[fork-cache]` event `outcome` ∈ `expect` | stderr |
| `session_event_count { event_type, min, optional }` | journal has ≥ `min` events of that type | journal |
| `journal_tool_called { name, optional }` | tool name appears in journal `tool_calls` | journal |
| `judger { question, threshold, model }` | LLM scores ≥ threshold | LLM |

### Criterion severity levels

Each criterion has a severity that controls how failures are treated:

| Severity | Meaning | Criteria types |
|----------|---------|----------------|
| **Hard** | Fundamental correctness — failure means the case did not work. Blocks the LLM judger from running (no point scoring a broken run). | `exit_code`, `tool_called`, `text_contains`, `tool_sequence`, `fork_cache_outcome` |
| **Soft** | Efficiency / performance bounds — failure means the case worked but outside acceptable limits. Does NOT block the judger. | `tools_count_between`, `tokens_between`, `duration_between`, `turn_rounds_between`, `cache_rate_above`, `stderr_matches` |
| **Quality** | Continuous quality score (0.0-1.0) rather than binary pass/fail. | `judger`, `session_event_count`, `journal_tool_called` |

Severity is assigned automatically based on criterion type (see
`criterion_severity()` in `src/criteria.rs`). Case authors do not set
severity manually.

### Auto-judger

When `--no-judger` is NOT set and a case has no explicit `judger`
criterion, the harness auto-attaches a default quality-check judger:

> "Given the task: {prompt}\nDid the agent complete it correctly and
> efficiently? Score 0.0 for wrong/incomplete, 0.5 for partially
> correct, 1.0 for fully correct."

This ensures every case gets a quality assessment without requiring
manual judger config in each YAML. Disable with `--no-judger`.

### Session-based criteria semantics

`session_event_count` and `journal_tool_called` require a loaded
session. **Default is strict:** if the session isn't available and
the criterion was declared, the case FAILs with a hint telling the
reviewer how to enable capture. Set `optional: true` to skip-pass
when the session is missing.

Enable capture by either:

- `debug_log: true` on the case, or
- `--capture-session` on the CLI (forces capture for every case).

### Reserved CLI flags

`extra_cli_args` supports pass-through flags like `--explain`, but
rejects at case-load time any flag the harness manages:

- Prompt / input: `-m`, `--message`, `--stdin`
- Model selection: `--model`
- Output format: `--json`, `--quiet`
- Tool approval: `-y`, `--yes`, `--auto-approve`
- Permission mode: `--permission-mode` (silently expands auth)
- System prompt: `--system-prompt` (bypasses judger anti-gaming preamble)

The authoritative list lives in `RESERVED_CLI_ARGS` in `src/case.rs`.

## Judger

The harness's LLM judger scores free-form questions ("did the agent
correctly do X?"). Key features:

- **Anti-gaming rubric**: the prompt's data sections are wrapped in
  fenced `` ```data `` blocks with an explicit preamble calling out
  untrusted data. A fabricated `SCORE:` line in an agent's output
  can't hijack the judge's output.
- **Sees stderr**: the `[fork-cache]` / `[selector]` observability
  events are embedded (head+tail truncated to 8k chars) so the
  judger can read them.
- **Quorum voting**: `--judger-n 3 --judger-agg median` runs the
  judger three times and takes the median, smoothing single-call
  variance. Dissenting votes are preserved in `full_rationale` so a
  FAIL report shows the outliers.
- **Same-family warning**: stderr warns when the judger model is in
  the same family (anthropic / openai / alibaba / minimax / etc.)
  as any tested model — same-family judging tends to inflate scores.

## FAIL report artifacts

On FAIL, each case report includes:

- `text:` / `stderr:` preview (head of each, truncated).
- `journal:` absolute path to `~/.astra/sessions/<id>.jsonl`.
- `hint:` a ready-to-paste `jq` command that extracts the tool-call
  sequence from the journal.
- `rerun:` the exact shell command to re-run the case.
- `digest:` an auto-captured summary from `astra journal digest`
  (turns, tokens, errors, tool calls). Turn off with
  `--no-digest-on-fail`.
- `judger full_detail`: the untruncated rationale for every judge
  vote when quorum is on.
- `failure_class`: automated classification (infra, model, flaky, etc.)

## CLI reference

```text
astra-test [OPTIONS] --suite <DIR>

Main options:
  --suite <DIR>                case YAML directory (flat)
  --models <CSV>               fallback model list (case `models:` wins)
  --force-model <MODEL>        override all cases to use this model
  --filter <PATTERN>           run only cases whose name contains pattern
  --parallel <N>               concurrent case execution (default 1)
  --runs <N>                   repeat each (case, model) pair N times
  --astra-bin <PATH>           astra CLI to spawn (auto-detected otherwise)
  --working-dir <DIR>          cd subprocess here
  --format text|json           output format
  --verbose                    always dump text+stderr + load session journal
  --skip-preflight             skip login/model availability checks
  --retry-on-429              retry rate-limited cases (default: classify as infra fail)

Judger:
  --judger-model <MODEL>       scoring model (default claude-sonnet-4-6)
  --judger-timeout <SEC>       hard timeout per judger call
  --judger-n <N>               run N times and aggregate (default 1)
  --judger-agg median|mean|min|max   aggregation for --judger-n
  --no-judger                  skip judger criteria entirely (also disables auto-judger)

Session capture:
  --capture-session            load journals for every case (not just
                               cases with debug_log: true)

Digest:
  --no-digest-on-fail          disable on-FAIL digest auto-capture
  --digest-timeout <SEC>       digest subprocess timeout (default 15s; raise
                               to 30-60 on cold CI)
```

## Execution model: parallel with circuit breaker

The harness runs cases × models with configurable concurrency
(`--parallel N`). Default is serial (`--parallel 1`).

- **Parallel mode** (`--parallel 4`): uses a semaphore-gated
  `FuturesUnordered` pool. Results are sorted by
  `(case_name, model, run_index)` for stable diffs regardless of
  completion order.
- **Circuit breaker**: 3 consecutive infrastructure failures
  (timeout, spawn error, rate limit) abort the remaining queue.
  Prevents burning provider credits on a broken server.
- **Run index**: when `--runs N > 1`, each report carries a
  `run_index` (0-based) for disambiguation.

## Preflight checks

Before running cases, the harness validates:

1. **Login**: verifies credentials are valid (auto-registers a
   `harness-test` user if needed, preserving existing credentials).
2. **Model availability**: sends a minimal probe to each model in
   the matrix to catch misconfigured keys early.
3. **Binary resolution**: confirms the astra binary exists and is
   executable.

Skip with `--skip-preflight` for faster iteration when you know the
environment is healthy.

## Failure classification

Every FAIL is auto-classified into one of:

| Class | Meaning | Suggested action |
|-------|---------|-----------------|
| `Infra` | timeout, spawn error, rate limit | retry / check server |
| `Model` | wrong answer, bad tool choice | adjust prompt or model |
| `Platform` | exit ≠ 0 with tool errors | investigate astra bug |
| `Flaky` | passes on retry | add to flaky watchlist |

## Design principles

1. **Cases are data**, not code. YAML only.
2. **Criteria stack cheap→expensive**: deterministic matchers first,
   LLM judger last. Saves provider calls on already-failing cases.
3. **Orchestration lives in the library** (`SuiteRunner::run_all`).
   `main.rs` is ~110 lines of clap + dispatch — third-party embedders
   reuse the library without forking the binary.
4. **Trait-injectable**: `CaseExecutor`, `Judger`, `SessionLoader`,
   `DigestCollector` are traits with fake impls in test support, so
   the suite runner is unit-tested against stable fakes rather than
   real subprocesses.
5. **Session state is a first-class artifact**: after each run the
   harness loads the session's local journal (via session_id from
   the JSON output) and makes it available to criteria evaluators.

## Extending the harness

See [HACKING.md](HACKING.md) for the "verify against real journals
before writing a session-based criterion" checklist and the list of
pitfalls caught in review. Skipping that step produced four review
rounds' worth of drift; the integration tests under
`tests/real_journal_wire_shape.rs` are the canonical guard.

## Related

- `astra journal digest` — stable aggregate metrics for a session.
- `astra journal tree` — delegation / sub-run tree view.
- `astra journal diff A B` — compare two runs.
- `skills/analyze_session` — human workflow for single-session
  analysis (superseded in automation by this harness).
