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
Ordinary Cargo builds without a source declaration or valid attestation nonce
report an unverified (`git_dirty=true`) identity and cannot enter this mode.
Use the manual harness workflow, or build all compared binaries with a valid
64-hex `ASTRA_BUILD_ATTESTATION_NONCE`; source-watched builds then detect
unstaged changes even when that nonce is reused. A trusted source orchestrator
may instead refresh both `ASTRA_BUILD_SOURCE_GIT_SHA` and
`ASTRA_BUILD_SOURCE_GIT_DIRTY` for every build, as the manual workflow does.
Preflight resolves the legacy execution profile before checking health and
passes that same explicit profile to health, model probes and case execution;
it does not validate a native MOI endpoint and then execute on a legacy one.
An automatic switch to the isolated `harness-auto` profile rechecks readiness
and revision before registration and again before retrying the model probe.
Each model probe has a 90-second deadline. On timeout, the CLI process group is
terminated and an exactly observed Server session is cancelled and deleted
before the case can run. Missing session identity is reported, not guessed.
This is a readiness limit, not a case time budget or evidence that the model is
inactive.
Artifact-only `--build-info-json` remains independent of profiles/configuration.
The built-in executor enables `--explain=on` and retains bounded canonical
Explain facts in each attempt's `outcome.explain_capture` before deleting its
temporary machine-event file or owned session. Cases cannot override this flag.
Multi-step/retry aggregates have no combined capture: inspect their individual
attempts and steps. Capture diagnostics and execution-scope coverage must be
checked before treating token counts as complete; absent capture means unknown,
not zero auxiliary calls. Use the canonical Explain graph reducer for attempt
deduplication and conflict detection, not differences between overall totals
and round totals. All comparison arms use the same observation mode; its local
snapshot/artifact overhead remains part of measured elapsed time.
The text summary and `tokens_between` use terminal-reported counters, which can
mix primary usage with only some auxiliary operations. They do not certify full
model cost. Do not add all auxiliary usage to those counters: already included
operations would be counted twice. Cost comparisons require separate canonical
primary and auxiliary evidence; missing token lanes remain unknown, not zero.
The primary prompt-cache percentage uses the canonical Explain graph and only
certifies complete, exact input buckets across closed execution scopes. Output
coverage is independent. Missing input evidence, conflicts or transport gaps
produce an unknown percentage while terminal-reported cache counts remain
visible under their separate, non-authoritative accounting label. When every
physical execution has a known provider cache-read lane, the summary also
shows its observed read-token total without inventing an input percentage. No
provider or model name is used to infer an unreported cache-write lane as zero.
In particular, terminal `cache-write=0` does not prove an explicitly reported
provider zero: those scalar counters no longer retain field presence. Retry
and multi-step cases use individual attempt/step captures, never the mixed
aggregate as a substitute for missing physical evidence. Cancelled and
unavailable rows are not executions and do not enter this measurement. Other
journal-based cache criteria and pipeline statistics have not yet all been
migrated to this coverage check.
`prompt_cache_tokens` uses canonical primary physical-attempt evidence, not
mixed terminal counters. A read-only floor (`min_creation: 0`, no
`max_creation`) accepts a provider-observed cache-read lane even when creation
is unknown. Creation bounds and input-share ratios still require complete,
exact buckets. Missing read evidence or overlapping captures cannot certify a
read floor, including when an earlier retry has no usage evidence. Auxiliary
judgments do not contribute.
`provider_prompt_cache_read_ratio` uses exact primary-only input qualification
before applying warmup. It preserves zero-input groups, combines adjacent
executions of the same user turn, and orders request groups by typed ModelRound
coordinates within each execution. Multiple executions require the same session;
overlapping archives, nonadjacent repeated turns, and multiple physical scopes
inside one capture are unavailable rather than assigned a guessed order. There
is no journal or terminal-counter fallback for this ratio.
The injected `required_cache_scope` hard gate uses qualified cache-read facts;
it does not require unrelated creation or fresh-input lanes.
Conversation reuse requires reads after the first user turn; intra-turn reuse
requires reads after the first ModelRound request in one user turn and run.
Physical retries alone cannot establish another request boundary. Consecutive
recovery captures of the same run may contribute, but different runs cannot
combine into an intra-run witness; nonadjacent run reappearance is unavailable.
This proves observed reads at the requested boundary, not which prefix was hit.
Pipeline absolute cache statistics also use qualified canonical primary request
groups, never feedback scalars or raw-response fallback. `pipeline_avg_cache_hit_ratio`
is the arithmetic mean of nonzero-input **ModelRound request-group** read shares,
not a user-turn average and not the token-weighted `provider_prompt_cache_read_ratio`.
Its numeric thresholds are unchanged, but old feedback-based measurements are not
numerically equivalent. All executions must qualify before zero-input groups are
excluded from this mean. Required checks cannot pass unknown or all-zero evidence;
optional checks explicitly report a skip.

In harness pipeline JSON, `cache_hit_ratios: null` and `avg_cache_hit_ratio: null`
mean unknown coverage; `cache_hit_ratios: []` with a null mean means qualified
all-zero input (n/a). Valid nonzero groups produce an array and numeric mean.
`feedback_observations` independently counts valid typed feedback events, replacing
the misleading `turns_with_feedback` field. The text report retains compaction
and alert diagnostics even without feedback. This changes harness report data,
not runtime wire or database storage. Raw cache-break heuristics and feedback-based
stable-prefix estimates remain separate diagnostics; canonical mean qualification
does not certify their coverage or accuracy.
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

### Manual GitHub Actions run

The **Harness (live models)** workflow builds the selected Git revision's
Server, CLI and `astra-test`, starts disposable MatrixOne/Memoria dependencies,
then runs the existing YAML cases with DeepSeek-v4.1-Flash as both agent and judge. Dispatch defaults to the
full suite. The same execution job is reused by the automatic live regression
described below; offline PR checks still work without provider secrets.

In the repository's **Settings → Secrets and variables → Actions**, create:

- `HARNESS_GLM_BASE_URL`: the API base URL from the `glm-5.3` entry in your
  `.models.yaml` (HTTPS, without credentials, query parameters or fragments).
- `HARNESS_GLM_API_KEY`: that entry's API key.
- Optional `HARNESS_ADDITIONAL_MODELS_JSON`: a JSON array of genuine supplementary
  model entries in the existing admin model-file format. Each entry supplies its
  own endpoint, credentials, protocol and capacity. An explicit Flash entry
  replaces its shared-endpoint default; other entries inherit nothing.
  Duplicate names are rejected. Only cases that
  require additional models need this secret; ordinary delegation uses
  the selected matrix model, not a fixed GLM version.

Open **Actions → Harness (live models) → Run workflow**. Source inputs default to
`repository: matrixorigin/astra`, `ref: main` (its latest commit at checkout).
For a fork, enter e.g. `XuPeng-SH/Astra`; `ref` accepts a branch, tag or exact
commit SHA available in that repository. The workflow-dispatch branch selects
the workflow definition, not the source under test. Build checks and the report
use the actual checked-out SHA. Only select trusted source: its build scripts,
local actions and executables can access the provider credentials. The checkout
token must have read access; arbitrary private repositories are not guaranteed.
Manual/custom checkouts cannot upload build caches. Only automatic pushes of
the exact main revision save the shared harness cache; existing artifacts may
be restored in either mode. Sccache remains disabled.
There is no additional actor or repository allowlist: manual dispatch relies on
GitHub's repository write-access requirement. No organization membership token
is needed. This restricts who starts the workflow, not the code they select;
selected code can read and exfiltrate the provider key, so run only trusted commits.

The live log and Actions Summary show failed checks with numeric expected/observed
evidence: tool counts, outcome matches, individual JSON predicate matches (in YAML
order), ordered-receipt progress and duration limits, including failed composites.
Predicate counts are independent; a passing tool-count check still requires all
conditions on the **same** canonical call. Missing canonical evidence is reported
as unavailable, not as zero matches. No additional model or database calls are
made for this projection.

LLM summaries report the existing judge's grade/threshold or acceptance verdict,
and distinguish an evaluation error from a negative quality judgment. A case
without an explicit `judger`/`hard_judger` says **not configured**; a deterministic
failure does not automatically trigger another LLM diagnosis. These are observed
check results, not a claimed root-cause analysis. Raw prompts, tool/provider bodies
and free-form judge diagnostics remain private; public artifacts do not include
them. Checks without a safe numeric projection explicitly say that their detailed
payload is withheld. Full private diagnostics remain in the local suite report.

Prompt-cache scope checks retain their grading-time observation count, post-cold
cache-read sum, and bounded per-execution capture status, identity-match flags,
scope count, and cache-read counters. This distinguishes a missing/pending explain
capture from an observed absence of warm reuse without publishing session/run
identities, prompt content, provider bodies, or raw capture diagnostics. These
fields explain the existing verdict; they do not change cache-scope acceptance.

Once execution identity is verified, child-adoption checks publish a closed `child_adoption` diagnostic with status
`proven`, `contradicted`, or `unavailable`, and a fixed failure reason such as
`child_not_completed`, `fanout_start_repeated`, `model_mismatch`,
`result_digest_mismatch`, or `finalization_before_adoption`. Missing evidence has
separate unavailable reason codes. The grader and public projection use the same
canonical verifier; these diagnostics do not change verdicts or judge admission.
No child content, identities, arguments, expected values, or provider error text
are included. The safe-summary consumer rejects unknown codes and extra fields.

### Small automatic live regression

**Harness regression (live)** runs on relevant `main` pushes, same-repository
PRs and explicitly trusted forks. Its `pull_request_target` caller is taken from
the trusted base, not the PR. Before granting credentials it checks both the
head repository's full name and immutable GitHub ID:

| Trusted fork | Repository ID |
| --- | --- |
| `XuPeng-SH/Astra` | `1355430593` |
| `gouhongshen/Astra` | `1355443432` |
| `loveRhythm1990/Astra` | `1355489820` |

The allowlist lives in
[harness-regression.yml](../../.github/workflows/harness-regression.yml).
A fork cannot grant itself access by editing its copy. Recreated/lookalike
repositories fail the ID/name check. Adding a fork **trusts everyone who can
write code to that fork**: its build scripts, cases and executables run with
model credentials and can leak them. Log masking does not provide isolation.
This is deliberate credential access for these repositories, not safe execution
of arbitrary fork code. Revoke an entry if that trust changes.

Other forks and Dependabot PRs are explicitly skipped, not certified as tested.
Their merged revision runs on `main`; a maintainer can also manually dispatch a
reviewed exact SHA. Automatic PR execution checks out the accepted fork's exact
**head SHA**, not the base SHA or a movable branch. The reusable workflow and
gate remain base-owned; checkout's explicit fork opt-in is reached only after
authorization. Token permissions stay `contents: read`, checkout credentials
are not persisted, and PR runs cannot save the shared build cache. The runner
is disposable. Keep this lane optional in branch protection: offline required
checks retain existing coverage. Superseded runs are cancelled by PR number.
Allowlist changes take effect only after they reach the trusted base branch.

The authoritative selection is [ci-regression.txt](ci-regression.txt). The
selector stages those original files unchanged into one suite; it does not copy
prompts into the workflow or implement another grader. The 18 roots cover
distinct delivery, tool, observability, context and delegation boundaries; child
communication and inherited-model checks bind to the selected model rather than
requiring an obsolete hard-coded provider version.

| Case | Level / evidence |
| --- | --- |
| `hello_text_contains` | Simple: real response, exact marker, no tools |
| `reasoning_multi_step_logic` | Simple: exact multi-step reasoning answer |
| `parallel_readonly_batching` | Simple tools: three repository reads, same-batch judgement |
| `task_agent_profile_callable` | Delegation: one completed child and actual result adoption |
| `recovered_validation_user_facing_completion` | Medium: failing baseline, edit, successful validation, user-facing delivery |
| `cache_provider_matrix_regression` | Four turns: stable-prefix/cache diagnostics (the optional cache-ratio check is not a hard cache-hit guarantee) |
| `flash_missing_child_model_fail_closed` | Unhappy: rejected unavailable model, zero children, no silent substitution |
| `same_model_fanout_handoff_flash` | Medium: two Flash children, exact slot identities, real child results and parent adoption |
| `file_edit_prepared_bytes` | File creation, localized edit, read-back and exact on-disk bytes |
| `read_only_shell_diagnostics` | Read-only shell authorization and truthful output delivery |
| `introspection_reflection_source_boundary` | Distinct live-process and durable-session evidence |
| `multi_turn_local_followup_focus` | Topic change, local follow-up and explicit historical recall |
| `flash_rejected_delegation_preserves_parent_tools` | Parent remains usable after a rejected child request |
| `flash_fanout_invalid_final_slot` | Atomic rejection: an invalid slot must not launch any child |
| `flash_child_question_parent_answer` | Correlated child question, parent answer and actual result adoption |
| `agent_workspace_evidence_chain` | Child reaches the inherited workspace, parent uses its evidence |
| `work_two_step_execution_journey` | Durable Work establishment, execution and settlement |
| `flash_model_name_is_output` | Quoted model name is task content, not an execution selector |

All parents and the judge use DeepSeek-v4.1-Flash; this is not independent-model judging.
Only Flash is registered by default, using the existing `HARNESS_GLM_BASE_URL` and
`HARNESS_GLM_API_KEY` secrets (the secret names do not select a model).
GLM-5.3 is temporarily excluded from this regression because its service is unstable.
The full suite retains `multi_model_fanout_handoff`; explicitly configure its GLM
dependency in `HARNESS_ADDITIONAL_MODELS_JSON` to run it. The regression does not
claim heterogeneous-model coverage.
An explicit `deepseek-v4.1-flash` entry in `HARNESS_ADDITIONAL_MODELS_JSON`
replaces its default configuration unchanged; other additional models remain
self-contained. Missing/unavailable models fail preflight;
they are never silently substituted or omitted. No Jev configuration is required.

Run once, serially, with an aggregate **15-minute execution budget** plus at most
two minutes for cancellation/settlement. This is a budget, not a measured
performance claim: build, service startup, registration and artifact publication
are separate and may make the entire workflow longer. Eighteen roots include
follow-up turns; these are not counted as additional roots. Per-case timings, results, error classes, usage
coverage and partial-timeout summaries use the same safe live progress as the
manual workflow. There is no retry-until-green policy. This sample does not
replace full-suite, long-task, capacity or deterministic protocol coverage.

To inspect or reproduce the same selection locally:

```sh
run_dir=$(mktemp -d "$PWD/target/ci-regression.XXXXXX")
python3 scripts/ci/select_harness_cases.py \
  --cases crates/astra-test-harness/cases \
  --list crates/astra-test-harness/ci-regression.txt \
  --destination "$run_dir/cases"
# Against a fresh, same-revision Server and a disposable checkout as workspace:
target/debug/astra-test --suite "$run_dir/cases" \
  --astra-bin "$PWD/target/debug/astra" --working-dir "$HARNESS_WORKSPACE" \
  --force-model deepseek-v4.1-flash --judger-model deepseek-v4.1-flash --parallel 1 --runs 1 \
  --progress-file "$run_dir/progress.jsonl" --report-file "$run_dir/report.json"
```

Local execution retains the cases' individual deadlines; the aggregate deadline
and owned-process cleanup are provided by the Actions supervisor. Local raw
reports can contain task/provider data and must not be uploaded as public artifacts.

### Full manual suite and result reporting

The case-name filter defaults to `*` (all top-level YAML cases, not recursive),
with 1–3 repetitions. Use `hello*` for a small smoke test. Running all cases can
incur significant cost and requires each case's additional capabilities. Execution is
serial, with a 60-minute suite limit and a 120-minute job limit. Cases execute
in an independent Git clone with the selected SHA and actual origin history.
Historical-fixture cases still require that history to exist in the selected
repository. Filters match case names,
not filenames. No match, unavailable cases and failed cases are not passes.

During execution, Actions displays each case's start, periodic elapsed-time
heartbeat, terminal result, harness lifecycle duration, error classification,
failed root/step checks and cumulative counts. A heartbeat proves the harness
is still waiting, not that the model is making progress. For example:

```text
[harness] PASSED [12/63] "delegation / glm-5.3 / run 1" · 62.90s · exit=0 · error=none · counts: 10 passed, 2 failed, 0 cancelled, 0 unavailable
```

The inner run budget is 55 minutes, followed by up to two minutes for existing
cancellation and settlement before the outer 60-minute Actions limit. Completed
results are saved incrementally; timeout/crash summaries retain completed rows,
the last observed active case/elapsed time and queued cases. Unfinished cases
remain unfinished, not passing or falsely certified as cancelled. Follow-up
turns are shown under their owning case, not counted as additional cases.
Deadline termination is unsuccessful even if its completed rows passed.
The CI command runs inside the existing Linux harness process supervisor, so
separately grouped or daemonized descendants are also reaped after hard stop.
Final success requires the supervised Actions step to succeed, not merely a
complete-looking inner report. The CI helper refuses unsupervised execution.

`astra-test --progress-file PATH` opts into this same secret-free, versioned
JSONL stream; it requires a new file and cannot be combined with dashboard mode.
The CLI drains its observer before normal or failing suite exit. Observation
loss or file-write failure cancels via the existing runner and fails closed.
Selected custom source revisions must support this option and the repository
helper; unsupported sources fail explicitly, without an old-log fallback.

Progress uses `astra.harness_progress.v4`. Argument failures include bounded
validator-authored rule codes and canonical built-in field paths; array indices
become `*`, and unknown/provider fields are opaque. Private error text, arguments,
provider responses and judge rationale are not public artifacts. Issue/outcome
omissions are counted. If a receipt lacks rule details, analysis explicitly says
the exact invalid field is unknown; an argument rejection does not prove model
admission was reached. Failed execution predicates also report one bounded first
structural difference per predicate: declared paths, JSON types, fixed reasons
and counts, never actual values or unexpected member names. Missing keys, nulls,
extra members and value differences remain distinct; truncated diagnostics are
explicit and never change matching or the verdict.
The unavailable-model negative control intentionally
requires one valid first attempt: accepting a schema error would erase coverage.

The default registered model is DeepSeek-v4.1-Flash and uses the deployment's declared
`reasoning_effort` protocol, 128,000-token context and 84,000-token output
ceiling. Prices are deliberately omitted: supply deployment pricing separately
before making monetary comparisons. Both advisory and required LLM graders run,
using DeepSeek-v4.1-Flash unless a criterion explicitly selects another model. Grading adds
provider calls and cost; a same-model judge is not independent quality evidence.
CLI judgments reserve at least 2,048 generation tokens (or the larger typed-answer
minimum); the Server bounds the effective request by the admitted Offering's
output limit and its 8,192-token safety ceiling. Incomplete or malformed judgments
remain failures/unknown evidence, never accepted truncated answers or automatic retries.
Complete semantic child evidence has one 64 KiB serialized-request ceiling,
including questions, escaping, metadata and backend wrappers. It is never
truncated to obtain a verdict; over-budget requests remain unjudged failures.
This input allowance is separate from ordinary 8,000-byte text/stderr previews
and does not increase output limits, deadlines or retry counts.
Cases may declare `required_models` for supplementary models, independently of
their parent `models` matrix. These are checked by the existing fail-fast
preflight after filtering and are not replaced by `--force-model`.
Prompts and assertion string values can use the explicit `{{model}}` marker,
bound independently to each resolved matrix model (including `--force-model`).
This includes follow-up prompts and criteria, nested assertions and exact child
model identity checks. Unknown or malformed markers fail before execution.
Model lists, grader model overrides, shell hooks and environment are literal,
not templates. The delegation follow-up uses this binding and needs no second model.
The safe preflight sidecar identifies selected model IDs and the unavailable
model without provider error details. A nonzero agent probe alone does not prove
model unavailability. Execution/protocol failures leave `unavailable_model`
null; validated terminal error kinds and codes appear in the diagnostic.
The `failure` object identifies the model, stage (`execution`, `terminal_protocol`,
`probe_process` or `session_cleanup`) and available process exit code. It never
includes raw provider messages. The manual and regression workflows preserve
these same fields in the safe artifact and job summary.
Default all-case selection is not a claim that a GLM-5.3-only deployment can run
every case.
Cases requiring other models, external clients or special policy need those
capabilities and must not be interpreted as GLM-only coverage. Memoria uses mock embeddings;
these runs do not certify production embedding quality.

The job summary and seven-day artifact contain per-case status, timing,
terminal-reported token/cache counters, binary hashes and root/step criterion
results with severity and scores. An advisory judge without a score is explicitly
`unscored`, with `passed: null`, not a successful evaluation. Required judges may
return a boolean acceptance without a numeric score: their canonical pass/fail
remains `reported`, without claiming evaluation provenance. Non-judge booleans
also remain reported harness results, not a new execution certificate. These counters are
not complete task-tree cost or certified cache ratios; consult usage coverage,
and never interpret missing evidence as zero. Provider credentials are written
only into a private temporary model file and removed during cleanup. Raw
transcripts, Server logs, model configuration and full reports are **not**
uploaded, because they may contain sensitive provider responses. A setup failure
without a report fails the workflow. Safe failure diagnostics retain local
Server readiness status, model registration/activation, lookup exit code and
step outcomes, without retaining arbitrary provider error text.
Artifacts also contain a validated safe progress prefix (`progress.safe.jsonl`)
and complete or explicitly incomplete `summary.json`. A truncated trailing
record or malformed stream never erases previously validated results and never
turns an incomplete run into a pass. Raw progress input is not uploaded if its
schema cannot be certified. Timing distinguishes whole-case lifecycle elapsed
from agent-reported duration; token counters keep the existing coverage limits.

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
name: agent_child_reply
description: |
  Spawn an ordinary child and assert its durable result.
prompt: |
  Use agent once to spawn a child with description "Bounded reply"
  and prompt "Reply: child-ok".
  Surface the reply.
prompt_variants:
  - id: zh
    prompt: |
      使用 agent 一次创建子 agent，description 为“Bounded reply”，
      prompt 为“Reply: child-ok”。呈现其持久化结果。
debug_log: true # turn on session journal capture
timeout_seconds: 240
# Optional explicit CLI deadline when the case requires earlier settlement;
# it must be >72 and at least 15 seconds below timeout_seconds.
# cli_wall_time_seconds: 220
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
    equals: launched
  - type: session_child_result_adopted
    expected_result: child-ok
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
| `tokens_between { min, max }`                       | terminal-reported input (fresh + cache) and output in range; not full model cost | envelope    |
| `duration_between { min_ms, max_ms }`               | wall-clock duration in range                                 | envelope    |
| `turn_rounds_between { min, max }`                  | Provider LLM round-trips (`LlmRoundStarted`, with a bounded legacy fallback) in range | step_events |
| `cache_rate_above { threshold }`                    | tool cache hit rate ≥ threshold (0.0–1.0)                    | step_events |
| `prompt_cache_tokens { min_read, min_creation }`    | observed primary read floor when creation is unconstrained; creation bounds require exact complete buckets | canonical Explain execution facts |
| `provider_prompt_cache_read_ratio { min, warmup_turns, warmup_rounds }` | token-weighted primary cache-read ratio after explicit user-turn or ModelRound-request warmup ≥ `min`; physical retries share their parent group, phase attempts remain distinct; all executions must qualify before warmup | canonical Explain execution facts |
| `pipeline_avg_cache_hit_ratio { min, optional }` | arithmetic mean of complete primary ModelRound request-group cache-read shares ≥ `min`; zero input is n/a, unknown is not zero; optional unavailable checks explicitly skip | canonical Explain execution facts |
| `provider_prompt_cache_read_nonregression_ratio { min, min_pairs, max_identity_transitions_per_run }` | within typed system/tool identity epochs, primary-request `current cache_read / previous cache_read` ≥ `min`, with enough pairs in every multi-observation run and bounded identity transitions; only the first pair with a zero previous read per epoch is exempt. Ratios may exceed 1.0; reads include history. Auxiliary requests are outside this metric; `provider_prompt_cache_read_ratio` measures absolute primary-input share, also excluding them | canonical pipeline feedback |
| `provider_stable_prefix_cache_coverage { min, min_observations }` | token-weighted provider cache reads capped at the runtime-estimated stable system/tool prefix; requires enough typed `provider-prefix-v1` observations and valid pipeline evidence | canonical pipeline feedback |
| `stderr_matches { pattern }`                        | multi-line regex on stderr                                   | stderr      |
| `text_contains { needle }`                          | substring in final text                                      | envelope    |
| `text_not_contains { needle }`                      | substring is absent from final text                          | envelope    |
| `text_equals { expected }`                          | trimmed final text exactly matches the output contract       | envelope    |
| `text_json_value { path, equals }`                  | complete final text is one JSON value and the RFC 6901 pointer equals the expected value | envelope |
| `text_json_array_count { path, min, max }`          | selected JSON array length is within the inclusive range      | envelope |
| `text_json_path_absent { path }`                    | selected JSON pointer is absent (`null` is still present)      | envelope |
| `text_json_dag { nodes_path, node_id_path, node_required_string_paths?, edges_path, predecessor_path, successor_path }` | required node strings are non-empty, endpoints are unique/resolved, and the graph is acyclic | envelope |
| `session_event_count { event_type, min, max?, optional }` | session-capture event count is within bounds; JSON predicates can link a step event's `run_id` to another event such as its `agent_spawned` record | journal + step events |
| `session_child_result_adopted { expected_result?, spawn_match?, allow_get_result? }` | exact completed child result reaches its parent; without `expected_result`, a unique matching spawn must complete, then its full staged and adopted result hashes must match before parent finalization. This mode requires `spawn_match`; `allow_get_result` also accepts an exact complete foreground `agent(get_result)` or `agent_fanout(get_results)` read followed by the same parent's later finalization. | journal + trace events |
| `journal_tool_called { name, optional }`            | tool name appears in journal `tool_calls`                    | journal     |
| `journal_turn_tool_hidden { name }`                 | tool is absent from every canonical coordinator tool surface | journal     |
| `journal_tool_call_count { name, min, max, root_only? }` | complete durable calls for `name` are within the range; `root_only: true` excludes child runs and fails on unattributed matching calls | journal |
| `journal_tool_success_ratio { min, min_calls, allowed_failures? }` | raw and expected-negative-adjusted typed tool success meet the minimum | journal |
| `journal_tool_json { name, document, path, equals, where_match? }`| arguments, result, or bounded runtime metadata has the exact JSON-pointer value; optional `{ document, path, equals }` predicate constrains the same call | journal     |
| `journal_tool_json_contains { name, document, path, contains }` | arguments, result, failure error, or bounded runtime metadata has a string at the JSON pointer containing the semantic marker; formatting remains provider data | journal |
| `journal_tool_sequence { tools }` | durable tool calls contain the ordered lifecycle subsequence | journal |
| `journal_tool_precedence { predecessor, successor }` | every durable successor call happens after its predecessor | journal |
| `journal_artifact_consumed { producer, consumer }` | consumer used the exact session artifact advertised by a prior producer result | journal |
| `journal_tool_value_flow { producer, producer_document, producer_path, producer_filter?, consumer, consumer_document, consumer_paths, consumer_filter? }` | successful consumer call satisfying its structural predicate used an exact scalar emitted by a prior matching producer; `*` path segments project any array/object child without relying on result order | journal |
| `journal_tool_value_flow_bound { producer, producer_document, producer_path, producer_filters, consumer, consumer_document, consumer_paths, consumer_filters, min_turns_after_producer? }` | conjunctive typed filters bind scope/type to successful value flow; optional visible-turn separation proves a later-turn consumer without using prose or event-order guesses | journal |
| `journal_work_item_execution_from_start { min_distinct_items }` | completed `run_next_work_item` calls report that many distinct server-selected runnable WorkItems from prior `start_work` | journal |
| `journal_work_replacement_lifecycle { initial_items, cancelled_items, added_items, delivered_items, cancellation_after_deliveries?, added_execution_after_initial_deliveries?, require_added_at_start?, require_added_before_execution? }` | exact canonical replacement identities and outcomes; optional bounds require prior deliveries before cancellation and before added execution; require_added_at_start requires the complete genesis board, while require_added_before_execution proves every addition was committed by admission or an accepted scoped proposal before the first scoped execution, including failed executions, and corroborated by a subsequent complete board | journal |
| `journal_work_graph_patch { require_addition, require_retired_revision, … }` | canonical mutation evidence comes from an admission snapshot, an accepted plan proposal, or a deferred mutation observed after its settlement boundary; retirement is cancellation or supersession, never prose | journal |
| `judger { question, threshold, model }`             | LLM scores ≥ threshold                                       | LLM         |
| `hard_judger { question, threshold, model }`        | LLM scores ≥ threshold and failure fails the case            | LLM         |

### Criterion severity levels

Use deterministic execution criteria for child identity, actual model execution,
completion, and original-result custody. For natural-language task correctness,
omit `expected_result` in one top-level `execution_child_results_adopted` and add
a required `hard_judger`. The existing judge then receives the complete verified
child bodies, identities, original-result digests, parent requests and response.
Its rubric should evaluate each child's own answer and faithful parent reporting;
a correct parent answer cannot compensate for an incorrect child.

This mode requires a single execution, not follow-up steps. Missing, conflicting,
oversized or truncated required evidence cannot pass, and `--no-judger` cannot
disable the requirement. Existing explicit `text`, `contains`, and `json` child
expectations remain deterministic; use them only when that output contract is
actually required, not to enforce incidental prose or JSON whitespace.

Each criterion has a severity that controls how failures are treated:

| Severity    | Meaning                                                                                                                            | Criteria types                                                                                                                                  |
| ----------- | ---------------------------------------------------------------------------------------------------------------------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------- |
| **Hard**    | Fundamental correctness — failure means the case did not work. The judger still runs for diagnostics.                              | envelope correctness, `hard_judger`, strict journal checks, durable tool JSON/count checks                                                     |
| **Soft**    | Efficiency / performance bounds — failure means the case worked but outside acceptable limits. Does NOT block the judger.          | `tools_count_between`, `tokens_between`, `duration_between`, `turn_rounds_between`, `cache_rate_above`, provider prompt-cache checks, `stderr_matches` |
| **Quality** | Advisory quality signal; failure is a warning rather than a product failure.                                                       | `judger`, optional journal checks                                                                                                               |

Severity is assigned automatically based on criterion type (see
`criterion_severity()` in `src/criteria.rs`). Case authors do not set
severity manually.

`all_of` and `any_of` are explicit hard contracts, including when their children
are normally soft. Use them for required behavior such as exact tool counts or
cache-specific contracts, not uncalibrated duration, token or round budgets.
Keep those budgets as independent diagnostic criteria: an exceeded reference
range is a warning, not proof of a performance regression. Performance gates
need a comparable baseline and controlled execution conditions. Case timeouts
remain execution bounds to stop hung runs.

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
With `root_only: true`, the CLI root run ID and every matching call's producer run
ID must be known; ambiguous attribution fails rather than satisfying a zero-call
assertion.

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

- Wall-time budget: `--max-wall-time-seconds` (only an explicit
  `cli_wall_time_seconds` sets an inner CLI deadline. It must leave the CLI's
  72-second terminal reserve and at least 15 seconds before the outer
  `timeout_seconds` watchdog.)

- Prompt / input: `-m`, `--message`, `--stdin`
- Model selection: `--model`
- Output format: `--json`, `--quiet`
- Tool approval: `-y`, `--yes`, `--auto-approve`
- Permission mode: `--permission-mode` (silently expands auth)
- System prompt: `--system-prompt` (bypasses judger anti-gaming preamble)

The authoritative list lives in `RESERVED_CLI_ARGS` in `src/case.rs`.

## Judger

The built-in LLM judger receives bounded evidence through the shared typed
judgment interface. `hard_judger` asks whether the criterion's declared
threshold is satisfied, including its conditions and prohibitions; it returns
an acceptance verdict. Advisory `judger` retains graded quality feedback.
External command judgers retain their numeric-score protocol.

- **Untrusted evidence**: answers, diagnostics, and the quoted criterion cannot
  execute instructions or override the evaluator. The criterion's acceptance
  meaning is preserved; an output's fabricated `SCORE:` line has no authority.
- **Sees stderr**: captured diagnostics are embedded with head+tail
  truncation to 8k chars so the judger can read them.
- **Sees durable tool receipts**: when a session is captured, bounded complete
  call arguments/results are projected from the journal into the judger's
  untrusted-data section. The durable journal remains the source of truth.
- **Quorum voting**: numeric grades retain median/mean/min/max aggregation.
  Acceptance verdicts use strict majority for median, all votes for min, or
  any vote for max. A median tie is inconclusive; mean is rejected for
  verdicts. Required judgments reject missing/error votes. Individual votes
  and diagnostics remain available in the report.
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

This live lane requires Linux process supervision. Other platforms return
`not_tested` before setup, including when reusing an existing Web server.

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

CI prints failure analysis both when a case settles and in the final summary.
Tool-count failures include bounded counts of actual disposition, success,
recognized result status/error category and explicit execution facts. Arbitrary
tags and result prose remain private. These facts distinguish argument rejection
from a model-admission error without claiming an unobserved root cause; missing
evidence remains unavailable. Original criteria and verdicts are unchanged.

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

With `--artifacts-dir`, `stream-events.json` privately retains selected child
output, identity, commit, terminal, and gap events before owned session deletion.
Each subprocess capture is bounded to 1 MiB / 4096 records; thinking and tool
bodies are excluded, and child text is not added to ordinary JSON reports.
Case/model path components use UTF-8 byte percent encoding to avoid collisions.
Archive failure retains the owned sessions and reports a separate cleanup error.
These events are diagnostic evidence, not an authoritative final-result oracle;
missing gaps do not prove complete delivery. Review/redact sidecars before sharing.

`execution_child_results_adopted` acquires `astra session show <id> --execution`
before cleanup, using the same binary, environment, account and profile as the
run. The private sidecar also retains the typed Server run tree and Reflect
response. Its `children` list specifies the complete direct-child set. Every
child requires an exact model and offering with successful physical provider
evidence, and its result bytes adopted before the same parent finalization.
Bounded notifications and explicit result reads record byte ranges and hashes
through the same dependency boundary. Explicit reads count only when present in
the final request of a successful provider turn. The harness verifies each range
against the canonical child result and requires gap-free coverage; overlap and
out-of-order pages are allowed, while a preview alone cannot prove a full read.
It never falls back to local journal
events. The run tree must be complete; partial reflection can prove positive
facts but cannot prove absence. Capture is bounded to 5 MiB and one total
20-second observation window. Because trace ingestion is asynchronous, a
completed root/children snapshot without the root's finalization fact is sampled
again at most every two seconds. This waits for observation settlement, not a
matching answer: settled failures are not retried. An unsettled final snapshot
is retained and classified as verification unavailable. Each snapshot's two
Server reads are not transactional and do not enter model usage; unrelated
cases and normal conversations do not pay this observation cost.

`execution_tool_count` and `execution_tool_sequence` additionally request
`--transcript`. They join typed requests and terminal results by exact run and
call identity, and reconcile the count with the authoritative run tree. The
explicit inspection reads at most 4,096 transcript items and 2 MiB across pages,
within the same CLI inspection deadline. Missing pages, conflicting identities,
unknown statuses and count mismatches fail verification; an empty local journal
cannot prove that a tool was not called. JSON predicates inspect the retained
Server projection, not a reconstructed legacy journal or model-authored prose.
Runtime advisories travel separately in the typed tool receipt; TUI rendering
adds them to a disposable display copy, never to the result document. Truncated
or malformed result documents remain unavailable to JSON predicates.
Negative JSON-document predicates require raw evidence and are rejected here;
display projection cannot certify that a field was absent from the original.
`execution_child_count` uses the complete run tree to prove admission cardinality.
`execution_tool_count.error_contains` checks a positive diagnostic substring
only in that exact joined call's failed/rejected result body. It requires
`ok: false`, a positive minimum and a nonempty needle of at most 4,096 bytes.
Arguments, runtime advisories, successful results and another call's output
cannot satisfy it. This accepts plain-text errors without fabricating JSON;
the typed terminal disposition remains the failure authority.
Each child's `expected_result` is either `{text: "literal"}`
or `{json: <JSON value>}`. Both expectations require that exact child's bounded,
unambiguous terminal transcript document. Text compares the actual bytes;
JSON compares the parsed value. In both cases the untouched observed bytes must
match the adopted digest; parent output is checked separately.
Literal expected results are limited to 4,096 serialized bytes. Observed content
uses the bounded capture and semantic-request budgets, not the notification
preview limit. Semantic judging receives each child's actual tool arguments,
outcomes and results along with its final answer; custody alone does not certify
the answer's claims. The complete serialized judge request must fit 64 KiB or
verification is unavailable, never silently truncated or accepted.
Whitespace equivalence never excuses a wrong digest, foreign run, missing
document, or premature finalization. Scalar expectations are not supported.
Natural parallel children are matched bijectively by distinct model identities,
never by their output. For same-model children or explicit fanout, specify
`fanout_group: {kind: id, value: <group-id>}` or
`fanout_group: {kind: title, value: <group-title>}` and each child's `slot_index`.
Titles are independent of IDs, including the product's default title when one is
omitted. Multiple matching starts are ambiguous. The successful `started`
receipt must agree with the request and its canonical fanout/observation
identity projections, including the exact parent. Missing identity evidence is
unavailable; contradictory identity evidence fails. String selectors are not accepted.
Producer-typed pre-execution rejections are not launch candidates, but remain
visible to call-count checks and the judge. Unknown execution is not a rejection;
contradictory rejection/execution facts fail rather than being discarded.
One successful root start receipt
must bind unique slot indexes, agent and run IDs to exactly the authoritative children;
optional slot labels are not execution identities.
array position and aggregate counts are insufficient. A successful `get_results`
or collection receipt alone does not prove model observation: both mailbox and
explicit-read delivery require the same `results_adopted` digest boundary.
Known cardinality, model, slot or result contradictions fail the contract;
missing bounded physical/adoption evidence is verification unavailable, and
cannot hide another child's known contradiction.
The optional per-child `initial_thinking` uses the shared `ThinkingConfig` contract and
additionally requests `--run-events`. It compares the complete admitted initial
thinking object from that exact child's `run_started`, after proving physical
execution and result adoption. This is not proof of provider consumption.
Run projections are bounded recent tails, read in batches of at most four under
the same captured owner, 15-second CLI deadline and 5 MiB envelope limit.
Optional per-child `logical_rounds` also requests these projections. It compares
the existing execution owner's cumulative `llm_rounds_completed`, published as
`run_finished.turn_evaluation.metadata.llm_rounds` with matching producer and
owner generation. This includes failed logical calls, is not a physical-attempt
count, and is not the length of a bounded diagnostics ring. Missing or ambiguous
terminal counters are unavailable, not zero. The fixed two-model fanout requires
one logical call per child; an extra failed call followed by success is not one.
Missing initial admission, conflicting identities or incomplete thinking make
verification unavailable; a valid different control fails the behavior check.
An absent tail event never proves that an event did not occur.
Catalog recovery and pagination are not failed delivery solely because they
require multiple calls. The parent-tools rejection case requires successful
authorized Chat discovery; all calls and their failures remain in the report.

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

## Offline router training

`astra-test router-source-hashes --input evidence.json` computes review digests.
`astra-test router-offline --input evidence.json --authorization authorization.json
--output new-artifact-directory` validates approved structural evidence and
builds an offline-only candidate plus dataset and comparison report. These
commands bypass live suite/preflight/provider execution. See the
[offline router guide](../../docs/guides/model-router-offline.md) for the evidence
contract, synthetic fixture, split controls, and revocation requirements.

`astra-test router-config-hash`, `router-plan-hash`, `router-qualify`, and
`router-shadow` extend the
local router workflow with a pinned evaluation protocol and source roster, held-out quality/cost
gates and observational scoring of separately authorized later decisions. They
require no provider or runtime process. A qualification report can be rejected
even when the command succeeds; inspect `tuning.status`. Passing permits only
offline shadow scoring, never production activation. See
[the operator guide](../../docs/guides/model-router-offline.md).

Child-adoption verification still requires a completed root. When a complete,
owner-bound run tree contains a root in another canonical lifecycle state, the
safe diagnostic reports `root_running`, `root_waiting`, `root_paused`,
`root_delegated`, `root_interrupted`, `root_failed`, or `root_cancelled` instead
of implying that capture was missing. Invalid or incomplete trees remain
`run_tree_unavailable`. Neither diagnostic certifies child adoption.
Parent question/answer verification uses canonical reply obligations and durable
root/child communication observations, including stable parent mailbox identity;
it does not impose a second recipient-spelling contract on tool arguments.
