# astra-test-harness

Declarative end-to-end test framework for the astra CLI. Runs YAML
cases against one or more models, captures session state (journal +
step_events + stderr), evaluates success criteria (deterministic
matchers + optional LLM judger), and emits a scored report.

Harbor checks core execution readiness inside the actual task container before
chat: a connected database, supported interaction API and matching server build
are required. Optional-component degradation remains visible and does not block
unrelated tasks; memory-dependent tests still require memory readiness. The
strict monitoring semantics of `astra health` are unchanged. A pre-execution
failure or cancellation remains the primary error, with any collected artifacts
diagnostic only. Accepted runs, including scoreable interruptions, still require
a valid machine-event stream before metrics can be projected.

## Why a dedicated harness

Unit and integration tests prove code correctness; this harness
proves _end-to-end behavior_ — that a model, wired through the astra
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

### What this harness proves

`astra-test` is a **deployment/model smoke harness**. It starts the selected
`astra` CLI binary, then that CLI connects to the API endpoint configured by
its profile or `ASTRA_API_URL`. It does **not** start an HTTP server from the
current source tree. Consequently, `--astra-bin` selects the client under
test, not the server revision.

Use it to validate a deployed or explicitly started candidate Server with real
models. Do not use a passing remote run as evidence that uncommitted Server
code is correct. Branch acceptance requires the deterministic HTTP system
matrix against the current binary/DB wiring; a deployment smoke additionally
needs the target Server revision recorded by the release workflow.

For a revision-bound comparison, set `ASTRA_EXPECTED_BUILD_GIT_SHA` to the full
40-character commit SHA. Before any model probe, preflight requires the harness,
selected CLI and Server to report that exact revision and an explicitly clean
build. Missing identity or a dirty/mismatched build fails verification. This
mode cannot use `--skip-preflight`. `astra-test --build-info-json` prints the
same side-effect-free artifact identity as Server/CLI; record executable SHA-256
hashes alongside it. Without the expected revision, the harness remains a
deployment smoke and does not certify the current branch.
Revision verification requires the built-in CLI executor and is unavailable in
dashboard mode. Embedded identity is a build declaration: use the trusted build
workflow to refresh source metadata, particularly after uncommitted edits;
this check does not fingerprint runtime configuration or all dependencies.
Preflight resolves the legacy execution profile before checking health and
passes that same explicit profile to health, model probes and case execution;
it does not validate a native MOI endpoint and then execute on a legacy one.
An automatic switch to the isolated `harness-auto` profile rechecks readiness
and revision before registration and again before retrying the model probe.
Artifact-only `--build-info-json` remains independent of profiles/configuration.
The CLI binary is resolved to an absolute path before constructing any consumer,
so a case's working directory cannot select a different relative executable.
All CLI subprocesses inherit the caller's proxy and bypass environment unchanged;
ThinClient owns loopback bypass and remote endpoint proxy handling.
In revision-bound mode, selected cases cannot override endpoint/profile,
credential/settings roots, home directories, access tokens or proxy variables
through `cli_env`, or routing flags through `extra_cli_args`. These are rejected
before any live probe; configure shared routing in the harness environment or
`--profile` instead. Non-routing overrides such as `ASTRA_TRACE` remain allowed.
Strict mode loads the startup directory's dotenv once (unless the caller already
selected `ASTRA_CONFIG_SOURCE=explicit-env`), then exports `explicit-env` to the
whole execution chain. Probe/case working-directory dotenv files cannot supply
another endpoint, and cases cannot override `ASTRA_CONFIG_SOURCE`. This mode
also skips local ServerConfig files, including `ASTRA_SERVER_CONFIG`: settings
needed by local dependency probes must be available in the exported environment.
It does not change the remote Server's configuration.
Cases and their setup commands are trusted test inputs: this verification is
not isolation against scripts or concurrent processes changing CLI settings,
credentials, binaries or the deployment after preflight.

Invalid machine-event evidence fails the run even when terminal JSON reports
success. The report retains available terminal text and token diagnostics, but
clears session/run identities so it cannot certify an unbound journal. Such a
failure is a `BehaviorContractViolation`, not evidence of model incapability;
unclassified process/collection failures remain `Unknown`.

Per-case `cli_env` values are applied only to the spawned `astra` CLI process.
They cannot configure the Server selected by the profile or `ASTRA_API_URL`.
Cases that require a Server policy must run against an explicitly configured
candidate Server and assert durable Server-owned evidence. In particular, a
CLI-local token threshold cannot prove Server compaction. The default live suite
therefore does not manufacture compaction pressure; compaction mechanics and
`CompactionFired` are certified by deterministic Server-owned runtime gates.

## Quick start

```sh
# Local smoke: one case, no judger, parallel 4
./target/release/astra-test \
    --suite crates/astra-test-harness/cases \
    --force-model qwen-flash \
    --no-judger --parallel 4

# Cross-family judger (recommended: different family than tested models)
./target/release/astra-test \
    --suite crates/astra-test-harness/cases \
    --models MiniMax-M2.7 \
    --judger-model us.anthropic.claude-sonnet-4-6 \
    --judger-n 3 --judger-agg median

# Filter to specific cases, repeat 3 times for flakiness detection
./target/release/astra-test \
    --suite crates/astra-test-harness/cases \
    --filter fork_prefix \
    --force-model us.anthropic.claude-sonnet-4-6 \
    --runs 3 --parallel 2
```

### `--astra-bin` resolution

The harness auto-detects the astra binary in this order:

1. `--astra-bin <PATH>` CLI flag.
2. `ASTRA_BIN` environment variable.
3. `astra` on `$PATH`.
4. `target/release/astra` relative to the nearest Cargo
   workspace root above CWD.

The chosen path is logged to stderr. Fail-fast with an actionable
error when nothing resolves.

## Writing cases

A case is one YAML file. Example:

```yaml
name: fork_prefix_spawn_inherits
description: |
  Spawn a child with required prefix inheritance and assert its durable result.
prompt: |
  Use agent once to spawn child "G1" with prompt
  "Reply: inherited-ok" and inherit_prefix: {required: true}.
  Surface the reply.
prompt_variants:
  - id: zh
    prompt: |
      使用 agent 一次创建名为 G1 的子 agent，prompt 为“Reply: inherited-ok”，
      并设置 inherit_prefix: {required: true}。呈现其持久化结果。
debug_log: true # turn on session journal capture
timeout_seconds: 240
criteria:
  - type: exit_code
    code: 0
  - type: tool_called
    name: agent
  - type: tokens_between
    min: 0
    max: 5000
  - type: turn_rounds_between
    min: 1
    max: 5
  - type: duration_between
    min_ms: 0
    max_ms: 120000
  - type: journal_tool_json
    name: agent
    document: result
    path: /status
    equals: completed
```

`prompt_variants` are dormant by default. `--prompt-variants` expands the
canonical journey and every meaning-preserving user-turn rewrite into separate
case rows that share the exact same criteria. The case's weight is divided across the
equivalence class, so robustness coverage cannot inflate its aggregate score.
Variant ids are stable lowercase ASCII report suffixes (`case@variant`). Empty,
duplicate, unsafe, or trim-equivalent rewrites fail at suite load time.
By default a variant replaces the initial prompt; `step_index: 0` targets the
first follow-up in `steps`, so the same contract can probe long-session local
focus without duplicating the whole scripted journey.

### Criteria types

| Type                                                | What passes                                                  | Data source |
| --------------------------------------------------- | ------------------------------------------------------------ | ----------- |
| `exit_code { code }`                                | subprocess exit equals `code`                                | envelope    |
| `tool_called { name }`                              | `tools_used` envelope contains `name`                        | envelope    |
| `tools_count_between { min, max }`                  | `tool_calls_count` inclusive                                 | envelope    |
| `tool_sequence { tools }`                           | `tools_used` contains tools as ordered subsequence           | envelope    |
| `tokens_between { min, max }`                       | total tokens (prompt + completion) in range                  | envelope    |
| `duration_between { min_ms, max_ms }`               | wall-clock duration in range                                 | envelope    |
| `turn_rounds_between { min, max }`                  | Provider LLM round-trips (`LlmRoundStarted`, with a bounded legacy fallback) in range | step_events |
| `cache_rate_above { threshold }`                    | tool cache hit rate ≥ threshold (0.0–1.0)                    | step_events |
| `prompt_cache_tokens { min_read, min_creation }`    | provider prompt-cache read/write token buckets meet minimums | envelope    |
| `provider_prompt_cache_read_ratio { min, warmup_turns, warmup_rounds }` | token-weighted cache-read ratio after explicit turn- or provider-round warm-up ≥ `min` | journal |
| `provider_prompt_cache_read_nonregression_ratio { min, min_pairs, max_identity_transitions_per_run }` | within typed system/tool identity epochs, primary-request `current cache_read / previous cache_read` ≥ `min`, with enough pairs in every multi-observation run and bounded identity transitions; only the first pair with a zero previous read per epoch is exempt. Ratios may exceed 1.0; reads include history. Auxiliary requests are outside this metric; `provider_prompt_cache_read_ratio` measures absolute share from aggregate turn/round usage, which can include them | canonical pipeline feedback |
| `provider_stable_prefix_cache_coverage { min, min_observations }` | token-weighted provider cache reads capped at the runtime-estimated stable system/tool prefix; requires enough typed `provider-prefix-v1` observations and valid pipeline evidence | canonical pipeline feedback |
| `stderr_matches { pattern }`                        | multi-line regex on stderr                                   | stderr      |
| `text_contains { needle }`                          | substring in final text                                      | envelope    |
| `text_not_contains { needle }`                      | substring is absent from final text                          | envelope    |
| `text_equals { expected }`                          | trimmed final text exactly matches the output contract       | envelope    |
| `text_json_value { path, equals }`                  | complete final text is one JSON value and the RFC 6901 pointer equals the expected value | envelope |
| `text_json_array_count { path, min, max }`          | selected JSON array length is within the inclusive range      | envelope |
| `text_json_path_absent { path }`                    | selected JSON pointer is absent (`null` is still present)      | envelope |
| `text_json_dag { nodes_path, node_id_path, node_required_string_paths?, edges_path, predecessor_path, successor_path }` | required node strings are non-empty, endpoints are unique/resolved, and the graph is acyclic | envelope |
| `fork_cache_outcome { expect }`                     | `[fork-cache]` event `outcome` ∈ `expect`                    | stderr      |
| `session_event_count { event_type, min, max?, optional }` | journal event count is within bounds; `min: 0, max: 0` proves absence | journal |
| `journal_tool_called { name, optional }`            | tool name appears in journal `tool_calls`                    | journal     |
| `journal_turn_tool_hidden { name }`                 | tool is absent from every canonical coordinator tool surface | journal     |
| `journal_tool_call_count { name, min, max }`        | complete durable calls for `name` are within the range       | journal     |
| `journal_tool_success_ratio { min, min_calls, allowed_failures? }` | raw and expected-negative-adjusted typed tool success meet the minimum | journal |
| `journal_tool_json { name, document, path, equals }`| arguments, result, or bounded runtime metadata has the exact JSON-pointer value | journal     |
| `journal_tool_json_contains { name, document, path, contains }` | arguments, result, failure error, or bounded runtime metadata has a string at the JSON pointer containing the semantic marker; formatting remains provider data | journal |
| `journal_tool_sequence { tools }` | durable tool calls contain the ordered lifecycle subsequence | journal |
| `journal_tool_precedence { predecessor, successor }` | every durable successor call happens after its predecessor | journal |
| `journal_artifact_consumed { producer, consumer }` | consumer used the exact session artifact advertised by a prior producer result | journal |
| `journal_tool_value_flow { producer, producer_document, producer_path, producer_filter?, consumer, consumer_document, consumer_paths, consumer_filter? }` | successful consumer call satisfying its structural predicate used an exact scalar emitted by a prior matching producer; `*` path segments project any array/object child without relying on result order | journal |
| `journal_tool_value_flow_bound { producer, producer_document, producer_path, producer_filters, consumer, consumer_document, consumer_paths, consumer_filters, min_turns_after_producer? }` | conjunctive typed filters bind scope/type to successful value flow; optional visible-turn separation proves a later-turn consumer without using prose or event-order guesses | journal |
| `journal_work_item_execution_from_start { min_distinct_items }` | completed `run_next_work_item` calls report that many distinct server-selected runnable WorkItems from prior `start_work` | journal |
| `journal_work_replacement_lifecycle { initial_items, cancelled_items, added_items, delivered_items, cancellation_after_deliveries?, added_execution_after_initial_deliveries?, require_added_at_start? }` | exact canonical replacement identities and outcomes; optional bounds separately require prior deliveries before cancellation and prior initial-item deliveries before any added assignment/execution; require_added_at_start checks all added identities are present in the complete genesis board | journal |
| `journal_work_graph_patch { require_addition, require_retired_revision, … }` | canonical mutation evidence comes from an admission snapshot, an accepted plan proposal, or a deferred mutation observed after its settlement boundary; retirement is cancellation or supersession, never prose | journal |
| `judger { question, threshold, model }`             | LLM scores ≥ threshold                                       | LLM         |
| `hard_judger { question, threshold, model }`        | LLM scores ≥ threshold and failure fails the case            | LLM         |

### Criterion severity levels

Each criterion has a severity that controls how failures are treated:

| Severity    | Meaning                                                                                                                            | Criteria types                                                                                                                                  |
| ----------- | ---------------------------------------------------------------------------------------------------------------------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------- |
| **Hard**    | Fundamental correctness — failure means the case did not work. The judger still runs for diagnostics.                              | envelope correctness, `hard_judger`, strict journal checks, durable tool JSON/count checks                                                     |
| **Soft**    | Efficiency / performance bounds — failure means the case worked but outside acceptable limits. Does NOT block the judger.          | `tools_count_between`, `tokens_between`, `duration_between`, `turn_rounds_between`, `cache_rate_above`, provider prompt-cache checks, `stderr_matches` |
| **Quality** | Advisory quality signal; failure is a warning rather than a product failure.                                                       | `judger`, optional journal checks                                                                                                               |

Severity is assigned automatically based on criterion type (see
`criterion_severity()` in `src/criteria.rs`). Case authors do not set
severity manually.

### Judger scope

LLM judging runs only for an explicit `judger` or `hard_judger` criterion.
Deterministic cases do not create an extra product session for a generic
quality opinion. This keeps tenant quotas, memory, cache metrics, and session
inventories from being contaminated by the test oracle itself.

The four-turn cache observation case checks actual reuse, bounded creation,
exact ACK responses, and zero tool calls. Its historical 98% inclusive read-share
threshold is not a product correctness requirement: stable requests can still
incur fresh conversation and auxiliary input. The report retains absolute costs
and the aggregate read share including warm-up; it does not label that aggregate
as the former post-warm-up ratio. Exact prefix continuity requires request-level
evidence. Cases with an independently justified cost SLO can still use the generic
provider ratio criterion. Old reports retain their original criteria and verdicts.

### Session-based criteria semantics

`journal_tool_call_count` can combine `ok` with its document/path/equality filter.
Both conditions apply to the same durable invocation: a failed remember plus a
successful recall cannot count as a successful remember. Missing outcome evidence
fails an explicit `ok` filter.

All journal criteria require a loaded session. `session_event_count` and
`journal_tool_called` are hard requirements by default; set `optional: true`
only when missing evidence is explicitly acceptable. `journal_tool_call_count`
and `journal_tool_json` always fail without complete durable evidence. The JSON
criterion parses `args_full`/`result_full` and applies an RFC 6901 pointer, so
truncated previews and assistant self-reports cannot satisfy it.

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
  fenced ` ```data ` blocks with an explicit preamble calling out
  untrusted data. A fabricated `SCORE:` line in an agent's output
  can't hijack the judge's output.
- **Sees stderr**: the `[fork-cache]` / `[selector]` observability
  events are embedded (head+tail truncated to 8k chars) so the
  judger can read them.
- **Sees durable tool receipts**: when a session is captured, bounded complete
  call arguments/results are projected from the journal into the judger's
  untrusted-data section. The durable journal remains the source of truth.
- **Quorum voting**: `--judger-n 3 --judger-agg median` runs the
  judger three times and takes the median, smoothing single-call
  variance. Dissenting votes are preserved in `full_rationale` so a
  FAIL report shows the outliers.
- **Same-family warning**: stderr warns when the judger model is in
  the same family (anthropic / openai / alibaba / minimax / etc.)
  as any tested model — same-family judging tends to inflate scores.

## Product capability coverage

The product capability matrix in `astra-harness` is the inventory of user-visible
runtime contracts. Each entry has two independent proofs:

- a deterministic system test that owns the typed correctness boundary;
- either a model-driven YAML probe or an explicit reason the boundary must remain
  deterministic-only (for example tenant isolation, callback idempotency, races,
  and fault injection).

The audit also rejects a model probe that has only process success, generic
lifecycle/efficiency bounds, or LLM judgement. Every complete journey—including
follow-up steps—must contain at least one deterministic product oracle; semantic
judging may supplement that evidence but cannot replace it.

Audit the bridge without starting a server:

```sh
target/release/astra-test \
  --suite crates/astra-test-harness/cases \
  --audit-capabilities
```

Run the complete model-driven product probe pack with DeepSeek Flash:

```sh
make test-harness-capabilities
```

This builds the current `astra` CLI, audits typed capability anchors, and runs
exactly the declared model probes. The complete report, structured evaluation,
and per-case evidence are persisted under
`target/astra-test-harness/capabilities/` (ignored by git).

## Cross-surface Work journey (TUI + Web)

The YAML harness does not drive a browser or a controlling terminal. To verify
the user journey across surfaces, run the separate opt-in live harness:

```sh
# Point this at a disposable owner and a candidate Server built from this checkout.
ASTRA_HARNESS_ACCESS_TOKEN="$(cat /path/to/disposable-owner-token)" \
  make test-work-live
```

The runner starts a real TUI in a `forkpty` terminal and an isolated Web dev
server. It creates one Work from `/work start`, waits for Web to discover the
same Work in **Now**, continues it from the TUI, and requires the browser to
observe the same root Run while it is active. Only then does it send SIGHUP to
the TUI. The run is a pass only when the same Run ID reports a larger event
count and a terminal result after the TUI has exited; a page that merely still
says “working” is insufficient evidence. A successful result must be
`completed`; failed, cancelled, interrupted, or delegated Runs remain explicit
failures and cannot satisfy the journey.

The candidate API is never restarted, reused, or cleaned up by this command.
Before the journey it checks `/health`, interaction protocol 3, the exact
`build_git_sha`, a valid owner token, and the Work API major. A mismatch is
reported as **Not tested** rather than silently connecting to an old server.
Use a disposable owner because the current Work API intentionally retains the
created Work as evidence. The run directory contains `state.json`, the raw
PTY/Web/Playwright logs, and screenshots/videos on browser failure; it never
contains the access token.

`--model` (or `ASTRA_WORK_LIVE_MODEL`) selects the TUI's bootstrap model. The
Work continuation is a Server-owned turn and therefore uses the candidate
Server's canonical default model; the live report does not claim that the TUI
selector changed that Server decision.

This lane intentionally proves one safe cross-surface slice: TUI starts the
Work, Web observes it, and the Server-owned continuation outlives the TUI. Web
write control, Web-started Work, Edge or Server placement migration, and
replacement of a failed task are reported as separate unsupported/not-tested
coverage until their own deterministic journeys are added.

The browser control file is a one-way coordination handshake. It is not a
source of truth: Work, branch, run, activity, and post-exit event facts are
read from the authenticated Server, while the Web assertions come from the
real `/now` and `/works/<id>` pages. A provider that settles before the Web
observation window is reported as an unsuccessful/not-tested journey instead
of being stretched with a fixed sleep.

`--force-model` intentionally makes the probe pack independent of case-local
model defaults. Model output can drive and assess semantic behavior, but it never
replaces journal, trace, identity, isolation, or protocol assertions.

## Run result contract

Each case/model row has an authoritative `status` in the JSON report:
`passed`, `failed`, `cancelled`, or `unavailable`. `unavailable` means the
harness deliberately could not execute the case (for example, an explicitly
incompatible prompt-cache scope or a model-resolution error). `cancelled`
means the work was planned but stopped by user cancellation or the circuit
breaker. Both states remain visible as terminal rows, never count as a pass,
and are excluded from capability/runtime evidence; cancelled rows remain in
the planned denominator so an interrupted suite cannot look fully green.
`status` is the only aggregation authority. Model IDs are canonical,
case-sensitive execution identities: surrounding whitespace is rejected from
the matrix, and duplicate IDs are an error. `weight × difficulty` is the
single scoring weight shared by text and structured evaluation. Unknown model
metadata does not skip a case: the harness runs it so criteria can provide
actual evidence, including the injected hard gate for the declared cache
scope. The asynchronous post-loop health gate is injected only for cases that
explicitly set `requires_memoria: true`, clean up memory records, or declare
`session_subsystem_healthy`; an optional memory outage must not turn unrelated
tool or reasoning cases into identical failures. A negative case can still use
`capability: memory` while leaving `requires_memoria` false when its contract
is specifically to verify behavior without invoking the memory backend.

When a report includes `execution`, its `scope` is authoritative: `session`
describes one selected journal, while `case_attempts` aggregates every
captured root-attempt journal. `captured_capture_count` must equal
`expected_capture_count` and `evidence_complete` must be true before a
`case_attempts` projection can be treated as complete case attribution; a
complete `session` projection proves only that selected session. Rejected,
reused, suppressed, and deferred calls remain counted in their own buckets and never
inflate `executed_tool_calls` or settlement success.

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
  --prompt-variants            run meaning-preserving user-turn rewrites with shared criteria
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
  --judger-timeout <SEC>       built-in provider deadline (1–120s); external judge process timeout
  --judger-n <N>               run N times and aggregate (default 1)
  --judger-agg median|mean|min|max   aggregation for --judger-n
  --no-judger                  skip advisory judger criteria; required hard_judger fails closed

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
  Prevents burning provider credits on a broken server; every abandoned
  planned item is emitted as an explicit `cancelled` terminal row.
- **Cancellation**: serial and semaphore-queued parallel work re-check the
  cancellation flag immediately before execution, so a permit becoming free
  cannot start work after cancellation.
- **Run index**: when `--runs N > 1`, each report carries a
  `run_index` (0-based) for disambiguation.

### User-visible lifecycle

The live dashboard exposes the same lifecycle for every planned case:

```text
queued → running → periodic executing heartbeat → terminal report
```

`queued` means the case is waiting for a suite execution permit; it is not
evidence that the model or server is stuck. A heartbeat proves only that the
harness is still awaiting the case. It deliberately does not claim semantic
model progress; tool counts, journal evidence, criteria, and the terminal
report remain authoritative. A reconnect receives the current queued/running
projection from the server snapshot instead of falling back to an ambiguous
`running=true` flag.

This distinction is important for unhappy-path diagnosis: a long queue wait is
harness scheduling pressure, a heartbeat without new typed evidence is an
efficiency/non-convergence signal, and a terminal report with incomplete
evidence is never promoted to success merely because the process stayed alive.

## Preflight checks

Before running cases, the harness validates:

1. **Login**: verifies credentials are valid (auto-registers a
   `harness-test` user if needed, preserving existing credentials).
2. **Model availability**: sends a minimal probe to each model in
   the matrix to catch misconfigured keys early.
3. **Binary resolution**: confirms the astra binary exists and is
   executable.
4. **Self-hosted Memoria owner contract**: when the selected cases need
   Memoria and `MEMORIA_SELF_HOSTED_MASTER_ACCESS=1`, performs a bounded,
   read-only `Memoria-Owner` retrieval probe. This catches an old or
   incompatible Memoria image even when the administrator bearer health
   endpoint is green.

The local development stack pins the owner-auth capable Memoria image and
`make dev-deps-wait` performs the same probe before declaring dependencies
ready. A failed probe reports the dependency/authentication action instead of
spending model calls on doomed cases.

Skip with `--skip-preflight` for faster iteration when you know the
environment is healthy.

## Failure classification

Every FAIL is auto-classified into one of:

| Class      | Meaning                          | Suggested action       |
| ---------- | -------------------------------- | ---------------------- |
| `Infra`    | timeout, spawn error, rate limit | retry / check server   |
| `Model`    | wrong answer, bad tool choice    | adjust prompt or model |
| `Platform` | exit ≠ 0 with tool errors        | investigate astra bug  |
| `Flaky`    | passes on retry                  | add to flaky watchlist |

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
- `.agent/skills/analyze_session` — human workflow for single-session
  analysis (superseded in automation by this harness).

Canonical Work receipts include a bounded task-board excerpt in judge evidence.
It preserves the producing call, Work/branch identity, graph and item revisions,
and declaration/execution/delivery states before allocating space to large raw
result previews. Snapshot and upsert remain distinct; omitted task counts are
explicit. The excerpt copies typed receipt facts and never infers cancellation
or completion from the assistant's answer.
