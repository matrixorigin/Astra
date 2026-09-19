# Evaluation

> Status: target design contract.
> Last updated: 2026-09-18.

Evaluation defines how Astra measures agent quality, safety, reliability, and regression risk across prompts, tools, providers, memory, and orchestration.

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
  freezes the resulting first-adapter plan;
* `POST /evaluation/experiments` remains the lower-level registration boundary
  for trusted/internal callers that already have a complete `ExperimentSpec`;
* `GET /evaluation/experiments/{experiment_id}` reads a consistent projection
  of the plan, binding, canonical Run status, and terminal observations; and
* `GET /evaluation/experiments/{experiment_id}/report` returns the structured
  comparison, deterministic Markdown, coverage, observation references, and
  content/artifact fingerprints.

The execution entrypoint is also owner-authenticated and deliberately narrow:

* `POST /evaluation/experiments/{experiment_id}/trials/{trial_id}/start`
  starts one planned trial. The message and Prompt revision content are
  optional when the prepare endpoint froze them; if supplied, they must match
  those frozen bytes. Skill starts may omit the owner-scoped Skill name when
  prepare froze it; if supplied, it must match that frozen name.
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

The clean-session check is scoped to the derived Run identity: the first
request must see no prior session state, while a concurrent retry may observe
that same Run's in-flight rows. A different Run or pre-existing session state
still makes the trial unavailable. If a process dies after the Run claim but
before evaluation admission, recovery reads the trusted admission intent from
the canonical `run_started` event, binds the same Run generation to the
planned trial, and records a failed/cancelled observation after the crash
terminal transition. It never creates a replacement Run.

The API accepts client intent only. It never accepts client-supplied
observations and never starts a provider from a read request. Every read uses
the authenticated owner and one database transaction, so a concurrent
settlement cannot be rendered as a mixture of old and new facts. A missing or
unknown Run status is exposed as unavailable; it is not treated as running or
successful. The report is recomputable and deterministic at this stage; a
future artifact persistence layer may attach a durable download reference
without changing its fact or identity contract.

## Local workspace evaluation delivery boundary

The existing local `worktree` tool accepts `source_commit` for `action=enter`.
It resolves the reference (default HEAD) once, creates from that full commit,
and verifies the created HEAD/tree before switching the session. Result metadata
contains `source_commit` and `source_tree`. This identifies the creation source;
shared Git metadata means it is not an Eval isolation receipt.

Status: required delivery contract; the current text adapter does not implement
this path. Its HTTP integration test proves routing through the canonical Run
and reporting, not TUI operation or local tool isolation.

Developer-facing coding evaluation must be usable from TUI with an explicitly
selected Edge or User Runner. HTTP controls the experiment; it does not determine
where tools execute. Local shell, files, and Git remain on the selected provider
through the existing execution binding, permission admission, and durable tool
ledger. When CLI/TUI is the selected Edge tool host, it must service the
canonical tool-dispatch channel throughout execution. An independently running
User Runner retains its own lifecycle; the TUI may disconnect and reconnect
through the existing observation APIs.

The first workspace delivery covers one repository, one fixed case, and two
serial arms starting from the same verified source snapshot. Each arm has its
own Session, execution directory, and writable Git metadata. Worktree separation
alone does not establish isolation: shared temporary paths, environment,
credentials, Git metadata, and external side effects must be accounted for by
the selected sandbox and frozen tool policy. Unsupported isolation is reported
before execution rather than silently using ordinary local permissions.

The plan records the source identity, selected executor, effective tool and
environment policy, and comparison inputs. An authenticated workspace
materializer supplies the actual trial instance and executor binding generation
as evidence. A client-provided path or hash is selection intent, not proof of
materialization. Retry retains that instance and its outputs; cleanup may remove
only the instance whose ownership the materializer can prove. Edge disconnect
does not authorize Server-local fallback.

Acceptance requires a real client and Edge tool host to execute file and shell
operations, produce test output and patch evidence, and preserve the source and
other arm. Wrong owner, stale binding, unavailable runner, cancellation, and
tool-result replay must follow their canonical contracts. Reports distinguish
Run completion, Skill invocation, verifier success, and missing evidence. This
delivery is required for coding Skill evaluation; completing the text adapter
does not complete the developer-facing Eval goal.

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
