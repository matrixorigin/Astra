# Evaluation

> Status: target design contract.
> Last updated: 2026-09-18.

Evaluation defines how Astra measures agent quality, safety, reliability, and regression risk across prompts, tools, providers, memory, and orchestration.

Owner-level quality trends, drift, calibration, and session scores are read-only
diagnostics. They do not authorize activation or establish a verdict for an
arbitrary change. Evaluation has no legacy gate-validation, gate-history,
closed-loop execution, or drift-run API; drift is read through its GET endpoint.
The obsolete gate-results table is not part of the schema. Durable experiments,
observations, and immutable task assessments retain their own evidence contracts.

## Goals

- Make changes testable before activation.
- Evaluate complete agent behavior, not only final text.
- Include tool correctness, provider routing, context quality, and safety.
- Support replay with versioned inputs and clear non-replayable dependencies.
- Feed tuning jobs with trustworthy labels.

## Evaluation dimensions

| Dimension | Measures |
| --- | --- |
| Task success | Did the agent solve the user objective. |
| Tool validity | Were tool calls valid, necessary, and well-routed. |
| Context quality | Was the right memory/artifact/provider state included. |
| Safety | Were policies, permissions, and side-effect boundaries respected. |
| Robustness | Did the system handle failures and degraded providers. |
| Efficiency | Token cost, latency, retry waste, tool fanout waste. |
| User experience | Clear status, recoverability, useful diagnostics. |

## Case structure

```text
case_id
objective
input_transcript
context_snapshot_refs
provider_bindings
expected_behavior
forbidden_behavior
rubric
fixtures
privacy_scope
```

## Replay modes

| Mode | Meaning |
| --- | --- |
| Exact replay | Same context, tool facts, model config, no external live calls. |
| Simulated provider replay | External providers replaced by fixtures. |
| Live integration eval | Calls real providers under controlled policy. |
| Human review | Human judges output, trace, or behavior. |

## Regression gates

A change should not activate if it causes material regression in:

- safety;
- data loss risk;
- tool-call validity;
- provider fallback correctness;
- task success on critical workflows;
- cost/latency beyond policy budget.

## Relationship to learning

Evaluation produces labels and quality signals. It is not itself a training pipeline. Learning artifacts require the additional consent/redaction/lineage rules in [evaluation-and-learning.md](evaluation-and-learning.md).

## Generic comparison contract

Evaluation is a reusable controlled-comparison capability. A prompt, Skill,
tool policy, model/provider binding, memory policy, or workflow is only a
comparison input; none of them owns trial identity, assessment, or report
semantics. Historical quality-tracker data is observational context and cannot
stand in for a paired evaluation.

Judgment comparisons must establish a capable baseline without Jev and measure
the additional decision quality and efficiency when Jev is available. Core
functionality must not require Jev; an explicitly selected Jev-only strategy
retains its declared failure behavior.

Judgment policies are also comparison inputs. The judgment contract is
provider-neutral: an ordinary LLM can substitute for Jev, and Jev can be an
enhancement or the sole judgment backend. Any permitted backend fallback is
explicit and frozen; Jev-only never implies a fallback LLM call. Low-latency,
low-cost judgment providers can support frequent classification, relevance,
routing and direction decisions. Enhancement policies may enable decision
points or frequencies that are too expensive for the baseline, rather than
only substituting a cheaper backend for identical calls. Evaluation must
measure whether those decisions improve downstream
task outcomes, together with their added latency and all physical-attempt costs.
A judgment comparison must freeze the Offering, question contract, thresholds
and invocation policy; confidence or a successful judgment call alone is not
evidence of improved agent behavior. The supported `SkillRoutingJudgment`
adapter is the first concrete judgment comparison: it runs the same
owner-scoped pinned Skill
once with the candidate auto-route decision point and once with that decision
point disabled. The baseline and candidate therefore differ in one causal
factor. The candidate stores either an exact typed-judgment Offering plus its
`skill_auto_route` generation policy, or an explicit unavailable reason. A
missing default route keeps the basic Skill path executable; it does not count
as evidence that the enhancement was evaluated successfully. An explicitly
selected Offering is re-admitted at prepare time, so selecting Jev is
Jev-only and selecting an ordinary typed-judgment Offering is the explicit LLM
substitute.

Each dispatched routing judgment records the frozen contract fingerprint, the
exact Offering/provider, request fingerprint, durable invocation ID, and
logical attempt. Observation settlement matches that identity against the
inference ledger before exposing judgment evidence. `uncertain` remains a
separate outcome from `negative`; it is usable evidence of an abstention but
keeps judgment coverage incomplete for a causal claim.

The initial planned isolation profile is `prompt_only_private`: the task input
and declared read-only resources are frozen, external side effects are
rejected, and production ranking, reflection, and learning writes are
disabled. When a task needs memory, each trial starts from the same Memoria
base snapshot and writes to its own memory branch; a branch is never merged by
evaluation implicitly. When a task needs structured data writes, each trial
uses a MatrixOne data snapshot/branch from the same base. These branches are
part of the composite snapshot for the trial; they do not isolate model
context, files, provider cache, or external APIs by themselves. Branch
creation or snapshot drift makes that trial unavailable rather than silently
changing its context. A Skillify flow supplies candidate Skill revisions to
this profile, but the same evaluation contract must work without a Skill
identity.

The baseline is the normal configuration or an immutable prior revision. The
candidate is loaded by its exact content hash. The object under comparison is
recorded in the experiment specification; it is not inferred later from tool
frequency or from a report filename.

Before dispatch, the specification freezes the cases, rubric/verifier
versions, model/provider configuration, fixed companion skills, repetition
and ordering plan, total budget, and composite snapshot references. The
scheduler persists a trial identity before creating a run. Run creation and
settlement are idempotent; an unknown provider outcome remains an uncertain
attempt rather than being silently retried as a new success.

The current contract records and validates these requirements. The durable
executor currently materializes prompt trials and owner-scoped,
instruction-only Skill trials; Memory/Data branches are still unavailable
until a materialization adapter is added. It must fail closed when a required
branch, snapshot, or isolation receipt is missing.
`Disabled` means the corresponding memory or data capability is prohibited for
the trial, not that it may be used without isolation.

Each trial input is addressed by a snapshot envelope. The envelope has a
UUIDv7 `snapshot_id` for lookup and idempotency, wraps the existing composite
snapshot references, and stores a canonical `snapshot_fingerprint` over its
owner, experiment/trial scope, context hash, policy hash, and component refs.
The UUID and the creation timestamp are metadata; the fingerprint and the
component references are the evidence of what was frozen. A materialization
receipt must still prove that an owner/trial-scoped Memory or MatrixOne branch
was created. A complete-looking envelope without that receipt is unavailable,
not an isolated execution.

The report must preserve all samples, including failures, cancellations,
timeouts, unavailable infrastructure, and missing measurements. It may state
that evidence is insufficient, but must not infer equivalence or causal credit
from a small successful-only sample. A report links each conclusion to the
trial facts that support it and distinguishes observed correlation from a
controlled version effect. The report renderer is separate from execution, so
disconnects or rendering failures do not lose completed trials. Missing usage
remains missing, and a partial or unavailable trial is never converted to zero
or silently excluded. The report distinguishes paired comparison support from
mechanism evidence; a trace being available does not by itself prove a causal
effect.

Measurements must have consistent status and value: `observed` requires a
finite numeric value (including a genuine zero); missing, unavailable, and
failed measurements carry no numeric value. The durable write/read boundary
and plan-bound report assessment enforce the same rule. Completed trials
with no measurements or no evidence references remain explicitly incomplete
in both structured coverage and Markdown. These basic checks do not prove
that every evaluation dimension has been measured or that a task verifier ran.

Every experiment requires `conditions.execution_config`, with no missing-field
fallback. This snapshot binds the admitted model and private route/proxy
identities, transport policy, context budget, primary thinking, static prompt
sections and metadata, experiment date, summary templates, cache enablement,
auxiliary generation policies and call gates, effective loop limits, and the
complete initial/hard/extension budget for each case. Case budgets must cover
exactly the declared cases. Unknown contract or renderer versions are rejected.

Prepare captures these values once. A new trial start loads the stored values
and verifies current model authorization, private routing and auxiliary gate
identity before binding execution. Both arms use the experiment date even when
started on different days. An exact prepare or Run retry returns its durable
identity before resolving new configuration. The same backbone consumers use
the frozen values; live provider admission and actual provider outcomes remain
external execution evidence rather than guarantees of identical model output.

The ordinary Prompt and Skill targets admit a chat-capable primary Offering and
do not add a judgment call. `SkillRoutingJudgment` freezes an independent typed
judgment Offering and its auxiliary generation policy; it never resolves the
live admin route during an isolated trial. Broader judgment comparisons still
need their own explicit target adapter and frozen question contract.

Every experiment requires `measurement_profile: "instruction-only.v1"`.
This immutable version defines the required task, tool, context, provider,
safety, reliability, and cost metrics and their units. Missing or unsupported
profiles are rejected. Submission retries return the frozen specification.
Reports enumerate gaps for every planned trial and metric, including trials
without observations. Numeric measurements and textual basis labels alone do
not prove scoped assessment or complete collection. The canonical run
accounting event supplies requested/executed tool counts, tool validity as
`successful_requested / requested`, and attempted policy denials; a trial with
no requested tool call has no tool-validity ratio. The physical inference
ledger supplies prompt/completion usage, latency, provider binding, and cost
only when every logical invocation, physical attempt, exact usage fact, terminal
state, and frozen route pricing snapshot is covered. Provider fallback count is
derived from the same ledger by comparing every physical attempt with its
logical invocation route; a missing route identity keeps that metric unavailable.
A provider-mismatched attempt is not counted as priced because the current
snapshot is route-scoped, so its cost remains unavailable. Missing prices or
usage remain gaps. Freezing a profile does not imply verifier execution or a
monetary estimate. The current frozen adapter does not switch providers, so a
complete zero proves route consistency for that run; it does not claim that a
Jev-to-LLM fallback chain is enabled.

The prepare API requires one tagged `case.verifier_config`. Prompt-only cases
use `json_value_equals` version `1`; workspace coding cases use
`workspace_command` version `1` with a frozen command, expected exit code, and
timeout. The server records the implementation
manifest, fixed rubric, and canonical configuration hashes in the case; these
are part of the experiment identity. The expected value is evaluator input,
never trial prompt content. A changed configuration conflicts on submission
retry. Cases carry the complete frozen verifier contract; separate caller-provided
verifier identity fields are not accepted. The shared pure JSON criterion
is also used by the test harness: it consumes the complete document and uses
JSON value equality, without extracting code fences or interpreting prose.
The workspace verifier runs only after agent execution settles. Edge captures
the final Git patch, applies it to a second clean clone, executes the command
against that replay with evaluator-owned Git metadata read-only, filesystem and
network namespace isolation, and a deterministic environment, then proves the
replayed Git tree did not change during verification. Symlinks and nested Git
metadata are outside this first coding profile and fail closed. Server persists the patch, output, exit
code, source/tree identity, and isolation facts before terminal settlement.
Missing capture, isolation, settlement, or artifact persistence yields an
unavailable assessment rather than inferred success.
Canonical atomic terminal settlement now writes `run_output_recorded` in the
same transaction, binding the output to its owner, Session, Run, generation,
and transcript `source_event_id`, with a content-only hash and byte count.
No output means no output receipt. This receipt proves recorded output identity,
not task success. The trusted assessment service consumes this output evidence
and the complete matching transcript text, verifying ownership, Run generation,
source event, content hash, and byte count before running the frozen criterion.
It persists one immutable assessment per owner/trial. Pass and Fail mean only
that the frozen structured-output criterion passed or failed; Run completion
alone does not establish either verdict. Non-completed executions and proven
absence of terminal output have no numeric task-success value.
Before the criterion runs, assessment independently verifies the complete Run
action history and hot/archive invocation ledger in the same transaction. The
persisted assessment includes the exact admitted tool identities and typed
completion references; missing, unresolved, or contradictory invocation
coverage leaves the assessment pending or unavailable.

`POST /evaluation/experiments/{experiment_id}/trials/{trial_id}/assess` takes
only authenticated owner and path identities. It returns an existing assessment
before reading historical output again. Otherwise it can repair the existing
Run's missing terminal observation and assess the persisted evidence. It never
starts a Run, resumes execution, or calls a provider. This works for a completed
first arm while the second arm is still waiting. Observation remains the
sequential-arm barrier; assessment does not introduce another scheduling gate.
Recorded assessments return HTTP 200, evidence not yet ready returns 202,
integrity conflicts return 409, and temporary storage failures return 503.
Missing evidence and transient read failures must not occupy an immutable verdict.
Historical verification does not re-admit the current model, require an active
Session, or renew the original materialization receipts.

Experiment and report GETs remain read-only. Reports derive `task_success` only
from persisted assessments and include their identities in the report manifest
and content fingerprint. An assessment closes only its task metric coverage;
uncollected tool, context, provider, safety, reliability, and cost evidence
remains explicitly incomplete. Evaluation keeps observational telemetry but
does not write its samples to production quality or learning sinks.

## Durable registration boundary

The first durable Eval boundary is intentionally small. An owner-scoped
experiment row stores the immutable `ExperimentSpec` and its submission
idempotency key. Its trial rows store the canonical planned `TrialUnit` and
remain `planned` until an existing owner-scoped Session and Run are bound. Eval
does not create a second execution state machine: progress and terminal state
come from the normal Run/Session backbone, while the binding table only records
which trial owns which execution identity.

Every registration and binding query includes the authenticated owner. A
repeated owner/spec/submission tuple returns the same plan; a reused experiment
ID or submission key with different content is a conflict. A Run can belong to
at most one trial for an owner, and a binding is an atomic compare-and-set, so
two sessions or edges cannot silently charge the same execution twice. Foreign
owners receive the same not-found/conflict boundary without learning another
owner's trial data.

The persistence adapter currently accepts a bounded plan (4,096 trials and a
bounded serialized payload) and reads it as one consistent snapshot. This is a
deliberate admission limit for predictable multi-tenant latency; a future
large-plan scheduler must add explicit batching and pagination before raising
it. Required Memory or MatrixOne branches are still unavailable until a
materialization receipt is recorded; registration alone never claims isolation.

## Control-plane API boundary

The generic control-plane API exposes five owner-authenticated operations:

* `POST /evaluation/experiments/prepare` accepts user intent (a Prompt's
  baseline/candidate text or two owner-scoped published Skill revisions, one
  fixed case, a model Offering, and bounded budget), resolves trusted model and
  Skill facts, computes the server-owned hashes/profile, and idempotently
  freezes the resulting first-adapter plan. A `SkillRoutingJudgment` request
  must use the same Skill revision on both arms; it may provide an exact
  `judgment_model_offering_id`, otherwise the configured judgment Offering is
  captured at prepare time. An optional workspace policy freezes the
  owner-scoped Edge executor, source commit, and exact tool surface; without
  that policy the adapter remains prompt-only;
* `POST /evaluation/experiments` remains the lower-level registration boundary
  for trusted/internal callers that already have a complete `ExperimentSpec`;
* `GET /evaluation/experiments/{experiment_id}` reads a consistent projection
  of the plan, binding, canonical Run status, and terminal observations; and
* `GET /evaluation/experiments/by-submission/{submission_idempotency_key}` is
  an owner-scoped recovery lookup for a lost prepare response. It returns the
  already registered experiment identity and never accepts new execution
  input; and
* `GET /evaluation/experiments/{experiment_id}/report` returns the structured
  comparison, deterministic Markdown, coverage, observation references, and
  content/artifact fingerprints.

The execution entrypoint is also owner-authenticated and deliberately narrow:

* `POST /evaluation/experiments/{experiment_id}/trials/{trial_id}/start`
  starts one planned trial. The message and Prompt revision content are
  optional when the prepare endpoint froze them; if supplied, they must match
  those frozen bytes. Skill starts may omit the owner-scoped Skill name when
  prepare froze it; if supplied, it must match that frozen name.
  For a workspace-backed plan, the canonical binding owner resolves the
  frozen Edge executor's active registration, constructs the typed
  workspace/executor binding, and rejects root, materialization, source, or
  tool-surface drift before claiming a new Run. An existing durable Run is
  replayed before that dynamic check; a different executor for the same trial
  conflicts. A plain text plan rejects Edge selection at start.
  Before creating the Run, the server re-admits the selected Offering and
  fails closed if its provider or supported cache contract has drifted. It
  derives the Session and Run identities from `(owner, experiment, trial)`,
  applies the frozen model and budget, and calls the normal Run lifecycle. It
  never accepts a provider, tool policy, memory branch, receipt set, or
  terminal observation from the client.

One exact trial has one durable Session and one durable Run. Retries and
concurrent requests with the same normalized payload replay that identity;
they do not spend another session/run quota or invoke a second provider. A
different payload for the same trial is a conflict. The Session bootstrap
identity is stored in its own immutable database field, separate from mutable
user metadata. The Run start claim is the existing owner lease/CAS boundary,
and ProviderTask/WorkTurn identities keep their existing lifecycle paths.

New trial admission locks the owner-scoped experiment before the canonical
Session and Run boundaries. The same transaction checks the frozen trial order,
checks capacity, creates the Run, and binds the trial. All earlier trials in the
canonical sequence must already be bound. Within each `(case_id, repetition)`
pair, the second arm waits for the first arm's immutable terminal observation;
a failed or cancelled observation also releases that dependency. BaselineFirst,
CandidateFirst, and Balanced use the same sequence-derived rule. Bound trials
without an observation occupy the frozen `max_concurrency` budget. Exact Run
retries reuse their binding and do not consume another slot. A premature start
is retryable; admission does not schedule or automatically start other trials.

The current disabled-memory profile selects no production Memoria client,
extraction service, or post-turn observer at runtime composition. Prompt
trials have an empty Skill catalog; Skill trials receive only the admitted
owner-scoped pinned revision. Neither trial kind falls back to the mutable
production catalog. Canonical trace, transcript, accounting, and output
receipts remain part of the normal Run settlement.

Normal canonical terminal settlement also commits finalized accounting in
that same transaction. Its v2 batch fingerprint binds owner, Session, Run,
generation, terminal state, usage totals, and the complete ordered event batch;
write-time CAS preconditions are not part of the committed fact identity.
After a restart, the Run store verifies the complete batch anchored by its
finalized-accounting event. Later appended events do not change that proof.
Evaluation can then rebuild a missing observation without a
`run_settlement_finished` marker or another provider invocation. This does not
claim that all post-loop cleanup finished, nor that usage collection is complete.
Control cancellation still requires its own drain/settlement fence.

The clean-session check is scoped to the derived Run identity: the first
request must see no prior session state, while a concurrent retry may observe
that same Run's in-flight rows. A different Run or pre-existing session state
still makes the trial unavailable. Each canonical recovery claim records its
generation transition in the same transaction as ownership and the event
watermark. Evaluation verifies a complete owner/Session/Run custody chain from
the original admission to a failed or cancelled recovery terminal. That terminal
has its own generation-scoped, hashed receipt committed with the Run status;
it does not claim accounting or executor drain has finished.

Trial binding, admission, and materialization receipts retain the original
generation. Observations separately persist `admission_run_generation` and the
terminal `execution_run_generation`. The observation write transaction verifies
both identities, the admission, and the custody/terminal evidence. Run creation
and trial binding commit together, so recovery never creates or repairs a trial
binding. A crash before materialization can produce a failed observation without
inventing materialization receipts. Binding confirmation and materializer writes
retain their current-generation fence.

Recovery evidence stops at the verified terminal event, so later status events
do not change the observation fingerprint. Without finalized accounting for
that terminal generation, usage remains unknown. Custody does not authorize a
cross-generation successful evaluation or a replacement Run. Resuming successful
evaluations across execution generations remains unsupported.

The API accepts client intent only. It never accepts client-supplied
observations and never starts a provider from a read request. Every read uses
the authenticated owner and one database transaction, so a concurrent
settlement cannot be rendered as a mixture of old and new facts. A missing or
unknown Run status is exposed as unavailable; it is not treated as running or
successful. The report is recomputable and deterministic at this stage; a
future artifact persistence layer may attach a durable download reference
without changing its fact or identity contract.

An evaluation Session with a bound trial retains its Run and inference
evidence. Session close skips ordinary post-session governance for that
boundary, and hard deletion returns a conflict while the bound experiment is
retained. The owner can delete the experiment after its Runs are terminal,
then delete the released Sessions through the ordinary lifecycle.
Retention is checked under the same Session fence used by trial start, before
recording deletion intent and again before deleting rows. A binding committed
while deletion waits for that fence must therefore retain its evidence.

Bound evaluation Runs reject live guidance at canonical durable admission with
`evaluation_input_frozen` (HTTP 409). Changing input requires a new experiment;
cancellation remains available. Web evaluation orchestration supplies explicit
request deadlines. Shared API requests have no implicit deadline, allowing
synchronous operations such as Skillify generation to finish.

## Local workspace evaluation delivery boundary

The existing local `worktree` tool accepts `source_commit` for `action=enter`.
It resolves the reference (default HEAD) once, creates from that full commit,
and verifies the created HEAD/tree before switching the session. Result metadata
contains `source_commit` and `source_tree`. This identifies the creation source;
shared Git metadata means it is not an Eval isolation receipt.

Status: the first workspace-backed adapter is now explicit in the prepare
request. It freezes one authenticated Edge executor, one full Git source
commit, one sorted built-in tool allowlist, and the authenticated confinement
contract into the experiment conditions. The contract identifies the supported
Linux profile and all declared read-only toolchain inputs, launcher, and supervisor
by content digest. Prepare and Run admission share capability checks and policy
fingerprinting; a changed live contract cannot replace the frozen inputs. Exact
submission retries return the stored experiment without resolving a new capability.
Missing confinement capability rejects new workspace preparations. Ordinary Edge
registration makes no confinement claim. A dedicated provider publishes the
exact confinement contract it verified at startup; the Server then freezes that
contract into the experiment and rechecks it at allocation and Run admission.

The dedicated Edge entrypoint is `astra-edge --evaluation-config
/etc/astra/evaluation.json --workspace-dir /var/lib/astra-eval/allocations/source`
with the usual authenticated connection arguments. This mode requires the
verified deployment described below. It publishes no confinement capability
when startup verification fails, and it never falls back to ordinary Edge
execution. Allocation, tool, finalization, and verifier receipts remain bound
to the existing evaluation materialization and Run evidence path.

Its first supported deployment is Linux x86-64 with an exclusive non-root
service UID, no supplementary groups or capabilities, and a root-owned,
read-only SquashFS service root. The private writable allocation root has mode
0700 beneath root-controlled ancestors; its only child mount is the root-owned,
read-only SquashFS source checkout. Toolchain trees must be flattened (no
symlinks or special files), immutable, and match the frozen content manifest.
The service also needs real procfs, `/dev/null`, private writable `/tmp`, and a
writable Edge state/journal directory outside trial mounts. Host temporary
coordination retains its existing root-owned sticky-directory requirements.
Configuration and manifest live in `/etc/astra` in the service image. Startup
checks the actual mounted inputs and launcher/supervisor digests, then requires
a real confined launch with verified setup and authoritative process settlement.
Host administration, image production, kernel, and exclusive service-UID
provisioning are trusted deployment responsibilities.

Configuration is strict JSON with `schema_version: 1`, `deployment_id`,
`expected_uid`, `allocation_root`, `toolchain_manifest_path`,
`toolchain_manifest_sha256`, and
`dedicated_service_assumption: "exclusive_uid_trusted_host_v1"`.
The manifest contains the shared `WorkspaceConfinementContract`; its file digest
covers the exact JSON bytes. The provider owns the versioned tree-content hash
encoding. Dedicated allocations retain directory identity and one canonical tool
executor across reconnects, bound to the authenticated owner and frozen
Session/Run. Explicit never-started tool refusals allow corrected requests;
started invocations without authoritative settlement quarantine the allocation.
Unknown paths after process restart remain quarantined rather than being
adopted from their name. Deployment-level qualification is still required.

The canonical Session binding still resolves the owner's active registration;
it rejects owner mismatch, an unavailable registration, a missing
materialization/root, or root/materialization drift before execution. The Edge
capability advertisement must also carry the authenticated source checkout
identity, a source tree, the Edge workspace binding, and every frozen tool.
The registry then creates an independent Git clone for this trial start
attempt, checks out the frozen commit, and returns a live source/tree/clean
snapshot that must match that frozen commit before the Run can claim
Available execution. The server-authored clone root travels inside the durable
Edge tool envelope, so replayed tools use the same trial clone. Clean clone
release is fenced to the connection generation that created it; a dirty clone
is retained for evidence. The registry and live clone proofs are converted into
a `workspace` materialization receipt. An evaluation that omits this policy
remains prompt-only and cannot select Edge or workspace tools.

Developer-facing coding evaluation must be usable from TUI with an explicitly
selected Edge or User Runner. HTTP controls the experiment; it does not determine
where tools execute. Local shell, files, and Git remain on the selected provider
through the existing execution binding, permission admission, and durable tool
ledger. When CLI/TUI is the selected Edge tool host, it must service the
canonical tool-dispatch channel throughout execution. An independently running
User Runner retains its own lifecycle; the TUI may disconnect and reconnect
through the existing observation APIs.

The ordinary Linux managed-workspace wrapper does not satisfy this full isolation
contract: it retains readable host files, and namespace isolation alone does
not block pathname Unix sockets exposed through mounted directories. The dedicated
mode now selects the restricted-root shell and retained native file authority,
but persisted proof of the complete confinement profile and deployment acceptance
remain pending; its Evaluation capability is consequently withheld. A
successful namespace probe or read-only root mount is therefore insufficient
evidence of confidentiality or external-side-effect isolation. The coding E2E
must prove these boundaries before this adapter can be considered complete.

Confinement belongs to the shared `ShellProcessBoundary`; process ownership and
settlement remain with `BashInvocationOwner`. Linux restricted execution must
expose only the selected workspace and declared read-only system/toolchain
inputs, allocate private HOME/TMP outside the captured source tree, and block
host IPC access. Native file operations need the same authority through pinned,
descriptor-relative access, including batch and rollback paths. The verifier
uses fresh private storage and protects its replay Git metadata. Frozen profile
and toolchain identities, successful setup, and authoritative process settlement
must be carried in materialization and terminal evidence; launcher failure is
not a verifier exit result. These are required implementation and acceptance
conditions, not guarantees established by the existing namespace booleans.

The first workspace delivery covers one repository, one fixed case, and two
serial arms starting from the same verified source snapshot. Each arm has its
own Session, execution directory, and writable Git metadata. Worktree separation
alone does not establish isolation: shared temporary paths, environment,
credentials, Git metadata, and external side effects must be accounted for by
the selected sandbox and frozen tool policy. Unsupported isolation is reported
before execution rather than silently using ordinary local permissions.

The plan records the source identity, selected executor, effective tool and
environment policy, and comparison inputs. An authenticated Edge registration
supplies the actual root, materialization identity, source commit/tree proof,
and executor binding as evidence. A client-provided path or hash is frozen
intent, not proof of materialization. Retry retains the same trial identity and
receipt set; cleanup may remove only the instance whose ownership the
materializer can prove. Edge disconnect does not authorize Server-local
fallback.

The dedicated Edge allocation owner issues an opaque allocation identity bound to
owner, Session, Run, deployment, materialization, source commit/tree, and frozen
confinement fingerprint. Snapshot, finalization, and release require that exact
retained receipt. Admitted tool results carry the same allocation receipt.
Before Run admission, the receipt is persisted as an immutable Session artifact
bound to the experiment, trial, Run generation, and authenticated connection.
Available workspace materialization references this artifact and its canonical
content hash. Both receipt issuance and execution admission resolve the artifact
and reject absent, expired, changed, or incorrectly bound evidence. A path or Git
tree alone is no longer accepted as workspace materialization evidence.

Execution admission carries that resolved artifact into the canonical tool
executor. Each durable dispatch decision freezes the allocation evidence, and
both dispatch and terminal-result replay check it against the admitted Run.
Edge requests carry the expected receipt; the dedicated provider validates it
against its retained allocation before new execution. Its durable invocation
journal binds the receipt alongside the invocation identity and arguments, so
active redelivery and completed replay cannot substitute another allocation.
Completed replay retains the original evidence even after workspace release;
it does not claim a new execution or require recreating the old workspace.

The adapter's execution machinery routes Edge file/shell calls through canonical
tool dispatch and records the source checkout as a receipt. New workspace trials
remain unavailable until a qualified provider advertises the required capability.
Coding assessment
now captures a durable final patch and runs the frozen verifier against a clean
materialization of that patch; it never infers test success from tool names or final
text. Acceptance still requires an end-to-end Edge client run across the public
control plane. Wrong owner, stale binding, unavailable runner,
cancellation, and tool-result replay must follow their canonical contracts.
Reports distinguish Run completion, Skill invocation, verifier success, and
missing evidence. A workspace receipt therefore enables the execution path;
it does not by itself complete developer-facing coding evaluation.

## Materialization receipt boundary

Materialization is an append-only evidence boundary between a planned trial
and an executor. A trusted server-side materializer records one receipt per
component (`context`, `policy`, `memory`, `data`, or `workspace`) with the
owner, experiment, trial, Session, spec fingerprint, exact snapshot envelope,
component address/content identity, branch base when applicable, materializer
kind, provider/runner binding and generation, outcome, and optional expiry. A
user-supplied request cannot choose the owner, provider, or runner identity
through this boundary. The receipt provider binding must equal the frozen
experiment provider, and `execution_run_id`/`execution_run_generation` must
equal the bound canonical Run identity. These fields identify the Run
generation, not an Edge or User Runner capability; a future adapter must add
its own authenticated capability binding instead of overloading this field.

Receipt writes lock the owner-scoped bound trial in the same transaction. The
owner and idempotency key are unique; the same request is safe to retry and a
different request with that key is a conflict. Receipts are never updated or
selected by recency. A read requires the exact owner, trial, Session, and
envelope supplied by the consumer, so a second user or session cannot reuse an
unrelated materialization.

Before execution, the consumer supplies the exact receipt set. The required
set is derived from the frozen spec: Context and Policy are always required,
and Memory/Data are required only for their explicit per-trial branch modes.
The validator rejects missing, duplicate, expired, mismatched, failed,
unavailable, disabled, or undeclared receipts. Context and Policy receipts
must carry the frozen content hashes; Memory and Data receipts must carry a
component content identity, the declared base snapshot, and the corresponding
snapshot dimension in the envelope. Disabled dimensions never receive a
synthetic `available` receipt. Replaying an expired registration with the same
idempotency key returns the original immutable fact, while use of that fact is
rejected. Until a verified Memory/Data materializer adapter exists, those
components may record unavailable/failed facts only; an unverified branch
address cannot be marked available. These receipts prove the recorded
identity and outcome, while the eventual materializer remains responsible for
provider ACLs and the executor remains responsible for refusing side effects
outside the declared profile. The execution-facing validation is a
point-in-time check: after its transaction commits, the Run generation may
advance. An executor must carry the returned generation into the canonical Run
admission/fencing CAS and refuse to start if it changed.

### Local shell confinement implementation status

Managed Edge native file operations use a retained workspace directory authority.
Linux opens reject symlinks, magic links, and mount crossings; prepared mutations
retain parent identities through validation, commit, and cleanup. Staged files
live outside the guest workspace in host-private storage on the same filesystem;
workspace-visible staging labels never authorize commit or cleanup. The provider
must keep that storage inaccessible to untrusted executions. Operation state is
fresh for each invocation and shared with its evidence projection.
Protected directories retain object identity across renames. Unsupported delegated
tools are denied before dispatch, and native edits do not launch formatters.
These operations use the shared workspace observation and desired-state
convergence receipts: a full read can confirm a write, while a repeated unchanged
write does not advance the workspace generation. This native file boundary does
not establish shell confinement or an Evaluation materialization receipt.

The CLI executor has an opt-in, immutable host shell boundary, independent of
permission-mode and Skill-policy changes. On macOS it wraps foreground process
launch with Seatbelt before the canonical Bash process owner. Workspace, private
HOME/TMP, and explicit read-only toolchain directories are validated at launch;
network access is denied and user environment overlays are not inherited.
System executable/library directories remain readable, and file metadata reads
are permitted globally. This is not a claim that all host data is invisible.
Detached and environment-lifetime background launches are rejected under this
boundary. Unsupported hosts fail rather than falling back to ordinary shell.

This is a shell launch primitive, not a workspace Eval admission profile. File
tools, implicit helpers, provisioning, trusted Runner receipts, and the Eval/TUI
binding still require integration. It does not upgrade macOS process-group
ownership into proof that every escaped descendant has terminated; a trial
cannot claim complete settlement or safely recycle its workspace on that basis.

Native CLI file admission intersects the immutable host roots with the ordinary
permission policy: explicit toolchain roots are read-only, and permission
expansion cannot widen the host boundary. This remains a path admission check,
not descriptor-relative isolation against concurrent symlink replacement.
Under this host constraint, native edits skip automatic formatters and do not
schedule passive LSP, Cargo, or TypeScript checks. Required formatter/verifier
commands must run explicitly through confined shell so their execution appears
in the trial evidence. Other delegated tools still need constraint-aware
admission before enabling a complete workspace evaluation profile.

### Owner-requested deletion

`DELETE /evaluation/experiments/{experiment_id}` deletes the authenticated owner's
experiment, trial bindings, observations, assessments, and materialization receipts
in one transaction. Missing or foreign experiments return 404. Active trial Runs
return 409: cancel them and wait for terminal status before retrying. Planned and
failed experiments do not require an assessment to be deleted.

Deletion takes the experiment mutex followed by Session fences, Session rows, Run
rows, and trial rows, so concurrent starts and evidence writers cannot recreate
orphan evidence. Session retention remains enforced until this transaction commits.
Delete the released Sessions through the ordinary Session API to clean up runtime
evidence; experiment deletion does not bypass that lifecycle.
