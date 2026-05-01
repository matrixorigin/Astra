# astra-test-harness

Declarative end-to-end test framework for the astra CLI. Runs YAML
cases against one or more models, captures session state (journal +
stderr), evaluates success criteria (deterministic matchers +
optional LLM judger), and emits a scored report.

## Why a dedicated harness

Unit and integration tests prove code correctness; this harness
proves *end-to-end behavior* — that a model, wired through the astra
CLI against a running server, produces the expected tool-call
sequence and session state. The existing runtime tests exercise
components in isolation; the harness exercises the whole binary
against real provider keys.

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
# Local smoke: one case, no judger, no session capture
./rust/target/release/astra-test \
    --suite rust/crates/astra-test-harness/cases \
    --models qwen-flash \
    --no-judger

# Cross-family judger (recommended: different family than tested models)
./rust/target/release/astra-test \
    --suite rust/crates/astra-test-harness/cases \
    --models MiniMax-M2.7 \
    --judger-model us.anthropic.claude-sonnet-4-6 \
    --judger-n 3 --judger-agg median
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
  - type: stderr_matches
    pattern: '\[fork-cache\]'
  - type: fork_cache_outcome
    expect: [hit]
  - type: judger
    question: Did the child's reply appear in the final answer?
    threshold: 0.7
```

### Criteria types

| Type | What passes |
|------|-------------|
| `exit_code { code }` | subprocess exit equals `code` |
| `tool_called { name }` | `tools_used` envelope contains `name` |
| `tools_count_between { min, max }` | `tool_calls_count` inclusive |
| `stderr_matches { pattern }` | multi-line regex on stderr |
| `text_contains { needle }` | substring in final text |
| `fork_cache_outcome { expect }` | `[fork-cache]` event `outcome` ∈ `expect` (one of `hit`, `partial_drift`, `miss`, `exceeded_expected`) |
| `session_event_count { event_type, min, optional }` | journal has ≥ `min` events of that type |
| `journal_tool_called { name, optional }` | tool name appears in journal `tool_calls` |
| `judger { question, threshold, model }` | LLM scores ≥ threshold |

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

## CLI reference

```text
astra-test [OPTIONS] --suite <DIR>

Main options:
  --suite <DIR>                case YAML directory (recursive? NO, flat)
  --models <CSV>               fallback model list (case `models:` wins)
  --astra-bin <PATH>           astra CLI to spawn (auto-detected otherwise)
  --working-dir <DIR>          cd subprocess here
  --format text|json           output format
  --verbose                    always dump text+stderr + load session journal

Judger:
  --judger-model <MODEL>       scoring model (default claude-sonnet-4-6)
  --judger-timeout <SEC>       hard timeout per judger call
  --judger-n <N>               run N times and aggregate (default 1)
  --judger-agg median|mean|min|max   aggregation for --judger-n
  --no-judger                  skip judger criteria entirely

Session capture:
  --capture-session            load journals for every case (not just
                               cases with debug_log: true)

Digest:
  --no-digest-on-fail          disable on-FAIL digest auto-capture
  --digest-timeout <SEC>       digest subprocess timeout (default 15s; raise
                               to 30-60 on cold CI)
```

## Execution model: serial

The harness runs cases × models strictly serially. A 16-case × 3-model
matrix with ~60–120s per run serializes to 45–100 minutes. This is
intentional for the current phase:

- Each subprocess hits a single running astra-server; true
  concurrency is capped by server capacity anyway.
- Session-capture races (two cases writing to overlapping paths) are
  avoided by construction.
- Quorum judger calls within a single case already serialize — the
  dominant cost is there, not in the case loop.

If you need a concurrency knob (independent model rows in parallel,
or distinct suites), open an issue. The library-side primitive
(`SuiteRunner`) is ready to accept a semaphore-gated `FuturesUnordered`
pass — the decision to hold off is about surface-area commitments,
not implementation difficulty.

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

## Related

- `astra journal digest` — stable aggregate metrics for a session.
- `astra journal tree` — delegation / sub-run tree view.
- `astra journal diff A B` — compare two runs.
- `skills/analyze_session` — human workflow for single-session
  analysis (superseded in automation by this harness).
