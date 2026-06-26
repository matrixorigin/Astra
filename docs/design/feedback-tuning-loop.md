# Feedback and Tuning Control Plane

**Date**: 2026-06-24
**Status**: Design Proposal
**Audience**: Astra runtime, CLI, server, learning, memory, policy, skill, and tool maintainers

## Executive Summary

The feedback and tuning control plane is the write-side complement to the
read-only observation plane. The observation plane, defined in
[Introspect and Reflect Observation Plane](introspect-reflect-observation-plane.md),
produces observations, evidence refs, causal chains, and `adaptation_signals`.
This document defines how future feedback/adaptation systems should consume those
signals and run controlled optimization experiments.

The core tuning primitive is not "change the prompt" or "patch a skill". It is a
bounded operator-style experiment:

```text
desired state: TuningSpec
observed state: Observation Graph + harness results + applied interventions
reconcile loop: diagnose -> search -> evaluate -> decide -> apply -> measure
status: conditions, evidence refs, budgets, selected candidates, next action
```

The design borrows the useful parts of Kubernetes Operators: declarative spec,
observed state, idempotent reconciliation, status conditions, audit events, and
future finalizers for cleanup, rollback, and promotion review. It does not
assume Kubernetes as a runtime, and it does not imply automatic mutation.
Tuning is an optimization experiment, not deterministic convergence.

The design separates three related paths:

1. **Reactive feedback**: consume one or more signals and propose a scoped
   intervention.
2. **Benchmark-driven tuning**: create a typed tuning job with an objective
   function, benchmark, constraints, search budget, candidate artifacts,
   evaluation runs, approval gates, and measurement plan.
3. **Production learning**: measure applied interventions over future sessions
   and decide whether to keep, roll back, or promote them.

The core lifecycle is:

```text
observe -> classify -> propose -> approve/apply -> measure
```

For benchmark-driven jobs, the middle expands into a controlled search loop:

```text
baseline -> diagnose -> generate candidates -> evaluate -> select -> holdout -> approve/apply
```

Only observation and measurement are read-only. Applying changes must go through
dedicated write-side channels with permissions, audit events, and rollback
behavior.

## Goals

1. Convert observation-plane `adaptation_signals` into typed candidate
   interventions or tuning jobs.
2. Keep read-only observation tools separate from mutating feedback and tuning
   tools.
3. Support multiple feedback targets without mixing their safety models:
   - User guidance
   - Tool parameters
   - Tool policy
   - Skill patches
   - Tool upgrades
   - Runtime prompt nudges
   - Context policy
   - Retry policy
   - Model routing
   - Memory lessons
4. Make every durable intervention typed, explainable, auditable, scoped, and
   reversible where possible.
5. Let later observations measure whether an intervention helped.
6. Prevent benchmark overfitting and unsafe broad-scope promotion.
7. Treat tuning as a reproducible experiment with explicit objective,
   constraints, search budget, stopping rules, and measurement plan.
8. Use an operator-style reconcile model so jobs are resumable, inspectable,
   and explainable through status conditions.

## Non-Goals

- `introspect` and `reflect` do not directly mutate runtime behavior.
- Adaptation signals are not automatically applied.
- A candidate intervention is not approval.
- A benchmark win is not approval.
- Applying a candidate is not promotion.
- Runtime prompt nudges are not a substitute for durable fixes.
- Skill and tool upgrades should not bypass review workflows.
- Tool implementation rewrites and global routing changes are not MVP scope.

## Architecture

The system has five layers:

| Layer | Role | Mutates State |
| --- | --- | --- |
| Observation Plane | Produces observations, evidence refs, causal chains, and adaptation signals | No |
| Tuning Control Plane | Owns specs, job state, search budget, evaluation orchestration, and decision records | Yes, job state only |
| Harness Layer | Runs baselines, candidates, holdouts, and production measurement queries | No, except evaluation records |
| Feedback/Adaptation Services | Convert signals into channel-specific candidate interventions | Yes, proposal state |
| Write-Side Channels | Apply approved changes to runtime, memory, skills, tools, prompts, or routing | Yes |

The control plane must not bypass channel-specific approval. It can recommend an
intervention, but write-side channels own application, rollback, and promotion.

## Operator Model

Tuning should eventually behave like an AI-native operator over a durable
resource. This is the target architecture, not the MVP implementation burden.
The MVP should use the same resource shape and status vocabulary where useful,
but it should start with one `JobController` and the smallest closed loop.

| Operator Concept | Astra Tuning Equivalent |
| --- | --- |
| Custom resource | `TuningJob` resource containing `metadata`, `spec`, and `status` |
| Desired state | `TuningSpec`: objective, constraints, allowed changes, search budget, apply policy, measurement plan |
| Observed state | Runtime/Observation/Adaptation graph slices, benchmark runs, candidate artifacts, applied interventions, measurement reports |
| Reconcile loop | Diagnose, generate candidates, evaluate, select, apply after approval, measure, keep/rollback/promote |
| Status | Phase, conditions, budget usage, selected candidates, evidence refs, last decision, next action |
| Events | Audit events, evidence refs, decision refs, apply records |
| Finalizers | Rollback, measurement completion, promotion review, cleanup of ephemeral prompt/runtime changes |

This model gives tuning three important properties:

- **Idempotence**: a resumed reconcile should not duplicate candidates,
  evaluations, or apply attempts if the same refs already exist.
- **Explainability**: every phase transition cites evidence and decision refs.
- **Boundedness**: the reconcile loop stops on target met, budget exhausted,
  no improvement, rejection, approval denial, or blocked prerequisites.

### Reconcile Contract

Each reconcile iteration should:

1. Read the `TuningJob` resource and latest observed evidence.
2. Validate that prerequisites and approval barriers are satisfied.
3. Advance at most one material phase when possible.
4. Write status, conditions, events, and refs before returning.
5. Avoid mutating runtime behavior except through an approved write-side
   channel.

The reconcile loop can be implemented by one service, several controllers, or
agent harnesses. The external contract should remain the same.

### Controllers

The MVP should use one `JobController` that runs baseline, diagnosis,
candidate generation, evaluation, and decision in a controlled sequence. The
controller must still write durable refs and status after each material step.

Later versions can decompose this controller when the workflow is proven:

| Controller | Responsibility | Writes |
| --- | --- | --- |
| `JobController` | MVP controller for baseline, diagnose, candidate, evaluate, decide | Job status, candidates, evaluations, decisions |
| `SpecController` | Validate/default `TuningSpec`, create job generation, set initial conditions | Job status |
| `BaselineController` | Run baseline harness and record baseline metrics | `EvaluationRun`, status |
| `DiagnosisController` | Cluster failures and create hypotheses from evidence | `TuningHypothesis`, status |
| `CandidateController` | Generate/materialize bounded candidate artifacts | `CandidateArtifact`, candidate status |
| `EvaluationController` | Run dev, holdout, regression, and measurement evaluations | `EvaluationRun`, status |
| `DecisionController` | Rank candidates, apply gates, select finalist, recommend apply/reject/iterate | `TuningDecision`, status |
| `ApplyController` | Request or record approved write-side application | `ApplyDecision`, `AppliedIntervention`, status |
| `MeasurementController` | Track post-apply production window and before/after metrics | `MeasurementReport`, status |
| `PromotionController` | Decide keep, rollback, promote, or continue measuring | `PromotionDecision`, status |

Controllers should communicate through durable refs and status conditions rather
than hidden in-memory agent state.

## Practical `/tuning` Implementation

`/tuning` should be implemented as a durable job surface, closer to `/plan` and
the task system than to a transient slash-command handler.
Its entry experience should feel like a specialized plan mode: draft the spec,
ask only for blocking missing fields, then create and start the durable job once
the user confirms or the intent is unambiguous.

Implementation details live in
[Tuning Job MVP Implementation Plan](tuning-job-implementation.md).

Recommended runtime shape:

```text
/tuning command
  -> create or inspect TuningJob
  -> persist spec/status/checkpoint
  -> enqueue or resume JobController
  -> JobController advances one state-machine step at a time
  -> append events and update checkpoint after each material step
  -> CLI/Web render status from durable projection
```

The command should not run a long optimization loop synchronously in the chat
turn. It should create or resume durable work, then return a job id and current
status. The agent or user can inspect progress through `/tuning status`,
`/tuning list`, a future adaptation-measurement reflect provider, or a UI
panel.

Do not force tuning into the generic task schema if that hides domain structure.
Tuning needs first-class candidates, evaluations, failure clusters, and
decisions. A dedicated `TuningJobRepository` can still mirror the task-store
contract.

## Maturity Levels

The design should be implemented in levels. This keeps the first version useful
without turning the tuning system into a broad, unsafe optimizer.

| Level | Capability | Examples | Approval |
| --- | --- | --- | --- |
| L0 | User-facing guidance only | Suggest clarifying intent or changing workflow | None or user-visible only |
| L1 | Ephemeral runtime guidance | Current-turn/session prompt nudge, retry hint | Runtime policy may allow |
| L2 | Session/workspace policy or artifact patch | Prompt patch, skill patch, context policy | User confirmation |
| L3 | Tool/schema maintenance | Tool schema patch, validator improvement request | Review workflow |
| L4 | Profile/team/global promotion | User preference, team skill, model-routing policy | Strong governance |

MVP should cover L0-L2. L3-L4 should remain explicit future work.

## System Boundary

The observation plane produces:

- `observations`
- `evidence_refs`
- `causal_chains`
- `adaptation_signals`
- Optional read-only `action_hints`

The tuning control plane consumes selected `adaptation_signals` and may produce:

- Feedback records
- Candidate interventions
- User-facing suggestions
- Runtime policy updates
- Memory feedback records
- Skill patch proposals
- Tool upgrade requests
- Prompt runtime nudges
- Model-routing updates
- Measurement reports

## Tuning Modes

Tuning modes share entities and evidence refs, but they have different strength
requirements.

| Mode | Trigger | Evidence Requirement | Typical Output |
| --- | --- | --- | --- |
| `reactive_feedback` | Runtime signal, user correction, or local failure | One or more observations and source signal refs | Candidate intervention, prompt nudge, feedback record |
| `benchmark_tuning` | Explicit `/tuning` goal or API request | Baseline, dev split, holdout or regression suite | Candidate artifact and `recommend_apply` decision |
| `production_learning` | Post-apply measurement window or periodic review | Applied intervention, before/after metrics, production observations | Keep, rollback, or promote decision |

The same `AdaptationSignal` may start a reactive feedback path or seed a
benchmark tuning job. A benchmark win may seed production learning, but it does
not automatically justify promotion.

## Lifecycle

| Stage | Owner | Side Effects | Description |
| --- | --- | --- | --- |
| `observe` | `introspect` / `reflect` | No | Produce observations, evidence refs, causal chains, and adaptation signals |
| `classify` | Feedback/adaptation service | Usually no | Decide what kind of tuning target the signal implies |
| `propose` | Feedback/adaptation service or agent | No, unless persisted as feedback | Produce one or more candidate interventions |
| `evaluate` | Harness layer | No | Run baseline, candidate, holdout, or production measurement |
| `approve/apply` | User, policy engine, or write-side tool | Yes | Apply an allowed intervention through a dedicated mutating tool |
| `measure` | Observation plane | No | Later observation reports whether the intervention helped |
| `promote/rollback` | User, policy engine, or write-side channel | Yes | Broaden, keep, or revert an applied intervention |

## Stable Entities

Implementation should use typed entities instead of passing loosely structured
JSON between agents.

| Entity | Purpose | Created By |
| --- | --- | --- |
| `AdaptationSignal` | Observation refs plus consumer metadata from the Observation Graph | `introspect` / `reflect` view |
| `TuningSpec` | Objective, benchmark, constraints, allowed changes, search budget, apply policy, measurement plan | `/tuning` or API |
| `TuningJob` | Durable execution instance of a `TuningSpec` | Tuning control plane |
| `TuningStatus` | Current phase, conditions, budget usage, selected refs, next action | Controllers |
| `TuningCondition` | Stable condition such as `BaselineReady`, `AwaitingApproval`, or `Blocked` | Controllers |
| `ReconcileEvent` | Audit event for one controller step or phase transition | Controllers |
| `ObjectiveFunction` | Metrics to maximize/minimize and hard constraints to satisfy | Tuning control plane |
| `SearchBudget` | Limits for rounds, candidates, cost, time, tokens, and stopping rules | Tuning control plane |
| `FailureCluster` | Group of related failures used to derive hypotheses | Diagnosis step |
| `TuningHypothesis` | Testable explanation for failures and proposed improvement direction | Tuning control plane or diagnostician |
| `CandidateIntervention` | Proposed change linked to signals, hypotheses, and artifacts | Adaptation service or candidate agent |
| `CandidateArtifact` | Concrete diff or generated artifact to evaluate | Candidate agent or tool |
| `EvaluationRun` | Baseline/candidate/holdout execution result | Harness |
| `TuningDecision` | Select, reject, iterate, or recommend apply | Supervisor |
| `ApplyDecision` | User/policy approval or denial | User or policy engine |
| `AppliedIntervention` | Actual applied change with rollback metadata | Write-side channel |
| `MeasurementPlan` | Future measurement scope, baseline, metrics, and rollback triggers | Tuning spec or measurement service |
| `MeasurementRun` | One measurement window or query result | Observation plane or harness |
| `MeasurementReport` | Before/after outcome assessment | Observation plane |
| `MeasurementDecision` | Done, continue measuring, or rollback | Supervisor, user, or policy engine |
| `PromotionDecision` | Future decision to promote beyond current scope | Supervisor, user, or policy engine |

Entity references must be stable and auditable. Candidate artifacts and
evaluation runs should remain available even when rejected.

MVP should only require `TuningSpec`, `TuningJob`, `FailureCluster`,
`TuningHypothesis`, `CandidateArtifact`, `EvaluationRun`, and `TuningDecision`.
`ObjectiveFunction` and `SearchBudget` can be embedded fields inside
`TuningSpec`. `TuningStatus` can be a minimal field set on `TuningJob`. Rich
conditions, reconcile events, finalizers, promotion decisions, and
multi-controller decomposition are target-architecture concerns.

`AdaptationSignal` must not duplicate observation severity, confidence, or
evidence. Consumers should dereference its `observation_refs` in the Observation
Graph.

## Causality and Confidence

The system should not overclaim causality. Signals, hypotheses, and decisions
must distinguish:

| Level | Meaning |
| --- | --- |
| `causal` | Direct intervention/outcome link established by controlled comparison |
| `likely_causal` | Strong evidence from traces and repeated behavior |
| `correlated` | Co-occurs with the problem but may not be causal |
| `unknown` | Insufficient evidence |

Benchmark-driven tuning may upgrade a signal from `correlated` to `causal` only
after a controlled candidate evaluation or clear before/after measurement.
Confidence scores should use the observation-plane model:
`classification`, `evidence`, and `causal`.

Tuning consumes two evidence classes:

| Evidence Class | Examples | Use |
| --- | --- | --- |
| `observed_evidence` | Runtime events, traces, decisions, observations, failure clusters | Diagnose what failed |
| `experimental_evidence` | Candidate evaluations, holdout results, measurement runs | Decide whether a change helped |

Observed evidence can justify a hypothesis. Experimental evidence is required
before claiming that a candidate improved the system.

## Feedback Targets

`adaptation_signals.consumer.target_type` identifies the downstream system that
may consume the signal.

| Target Type | Example Signal | Possible Downstream Action |
| --- | --- | --- |
| `user_guidance` | User intent is ambiguous or repeated correction appears | Ask user, show suggestion, request confirmation |
| `tool_parameters` | A tool succeeds only with narrower timeout, cwd, or query pattern | Adjust session-scoped tool defaults |
| `tool_policy` | A tool repeatedly fails or is misused for a task class | Deprioritize, pin alternative, or add selection hint |
| `skill_patch` | A skill instruction causes repeated bad behavior | Open a skill-edit proposal or patch PR |
| `tool_upgrade` | Tool lacks needed capability or schema is misleading | File upgrade request, add schema hints, improve validator |
| `prompt_nudge` | Agent needs short-lived guidance for current session | Inject scoped runtime nudge or lesson |
| `context_policy` | Context is stale, too large, or polluted | Compact, release context, change retrieval/injection policy |
| `retry_policy` | Errors are transient or require changed inputs | Change retry backoff, retry condition, or repair strategy |
| `model_routing` | Current model/tool combination underperforms | Route future similar work to another model class |
| `memory_lesson` | Pattern is stable and reusable | Store procedural memory or lesson after confirmation/policy check |

## Candidate Intervention Shape

A write-side feedback/adaptation service may convert selected signals into
candidate interventions:

```json
{
  "candidate_interventions": [
    {
      "candidate_ref": "urn:astra:candidate:local:cand_01H00000000000000000000001",
      "source_signal_refs": [
        "urn:astra:signal:graph:sig_tool_policy_01H00000000000000000000001"
      ],
      "source_observation_refs": [
        "urn:astra:observation:graph:obs_err_01H00000000000000000000001"
      ],
      "failure_cluster_refs": [
        "urn:astra:failure_cluster:graph:fc_tool_timeout_01H00000000000000000000001"
      ],
      "hypothesis_refs": [
        "urn:astra:hypothesis:local:hyp_01H00000000000000000000001"
      ],
      "target_type": "tool_parameters",
      "target": {
        "tool": "bash",
        "parameter": "timeout_ms"
      },
      "change_type": "session_default_override",
      "summary": "Use a shorter timeout for broad exploratory shell commands in this session.",
      "scope": "session",
      "risk": "medium",
      "requires_confirmation": true,
      "rollback": {
        "supported": true,
        "mechanism": "session_config_rollback"
      },
      "artifact_refs": [
        "urn:astra:artifact:local:runtime_policy_diff_01H00000000000000000000001"
      ],
      "success_metrics": [
        "lower_timeout_rate",
        "fewer_repeated_tool_errors"
      ]
    }
  ]
}
```

Candidate interventions are proposals. They do not imply permission to apply
changes.

## Intervention Scopes

All tuning actions need an explicit scope.

| Scope | Meaning | Typical Approval |
| --- | --- | --- |
| `current_turn` | Temporary nudge for the next model call | Runtime policy may allow automatically |
| `session` | Applies only to the active session | Agent may propose; user/policy may approve |
| `workspace` | Applies to this repository/workspace | User confirmation required |
| `user_profile` | Applies to the user's future sessions | Strong user confirmation required |
| `team` | Applies to shared team agents or skills | Admin or policy approval required |
| `global` | Product-level change | Engineering/release process only |

Default safe behavior is to keep interventions at `current_turn` or `session`
unless explicitly escalated.

## Feedback Channels

The system should distinguish feedback channels instead of treating all tuning
as the same operation.

| Channel | Writes To | Examples |
| --- | --- | --- |
| `user_feedback` | Feedback/event store | User says result was wrong, too slow, or helpful |
| `runtime_policy` | Session/runtime config | Tool priority, retry defaults, context-release policy |
| `memory_feedback` | Memory backend | Promote, demote, correct, or forget lesson |
| `skill_maintenance` | Skill files or skill registry workflow | Patch skill instructions or metadata |
| `tool_maintenance` | Tool schema/runtime backlog | Improve schema, validator, timeout model, transport |
| `prompt_runtime` | Volatile prompt nudge lane | Short-lived corrective guidance |
| `model_routing` | Routing policy store | Model override or model-class preference |

Each channel should have separate permissions, audit events, rollback behavior,
and measurement hooks.

## Benchmark-Driven Tuning Jobs

Some tuning should be goal-driven rather than reactive. For example, an NL2SQL
workflow may need to improve benchmark accuracy to a target value while keeping
the final prompt under a strict token budget.

This should be modeled as a tuning job, potentially exposed through a command
such as `/tuning`.

Example:

```text
/tuning goal="Improve NL2SQL exact-match accuracy to 88%"
        benchmark="nl2sql_eval_v3"
        constraints="prompt_tokens <= 3500, p95_latency_ms <= 5000"
        allowed_changes="prompt_patch,skill_patch,context_policy_patch"
```

The command starts an optimization loop. It does not mean the system can apply
all changes automatically. Durable changes still go through scoped approval and
the appropriate feedback channel.

The first implementation should restrict `allowed_changes` to:

- `prompt_patch`
- `skill_patch`
- `context_policy_patch`

`tool_schema_patch` can be included once schema review exists. `tool_impl_patch`
and broad `model_routing_patch` should remain out of MVP.

### TuningJob Resource

This is the target resource shape. MVP may keep `status` minimal as long as it
persists phase, budget usage, selected refs, and terminal decision.

```json
{
  "api_version": "astra.ai/v1alpha1",
  "kind": "TuningJob",
  "metadata": {
    "job_ref": "urn:astra:job:local:tune_01H00000000000000000000001",
    "generation": 1,
    "created_by": "user",
    "source_signal_refs": [
      "urn:astra:signal:graph:sig_context_policy_01H00000000000000000000001"
    ]
  },
  "spec": {
    "spec_ref": "urn:astra:spec:local:tuning_spec_01H00000000000000000000001",
    "domain": "nl2sql",
    "mode": "benchmark_tuning",
    "objective": {
      "primary": {
        "metric": "exact_match_accuracy",
        "direction": "maximize",
        "target": 0.88,
        "min_delta": 0.02
      },
      "secondary": [
        {
          "metric": "execution_accuracy",
          "direction": "maximize"
        },
        {
          "metric": "prompt_tokens",
          "direction": "minimize"
        },
        {
          "metric": "p95_latency_ms",
          "direction": "minimize"
        }
      ],
      "hard_constraints": {
        "prompt_tokens_max": 3500,
        "p95_latency_ms_max": 5000,
        "cost_per_case_max_usd": 0.02,
        "sql_safety_pass_rate_min": 0.99,
        "must_not_regress": [
          "schema_linking_accuracy",
          "sql_safety_checks"
        ]
      }
    },
    "benchmark": {
      "id": "nl2sql_eval_v3",
      "split_policy": "train_dev_holdout",
      "primary_split": "dev",
      "holdout_required": true,
      "regression_suite_required": true
    },
    "allowed_change_types": [
      "prompt_patch",
      "skill_patch",
      "context_policy_patch"
    ],
    "search_budget": {
      "max_rounds": 3,
      "max_candidates_per_round": 5,
      "max_total_candidates": 12,
      "max_eval_cases_per_candidate": 300,
      "max_wall_clock_minutes": 90,
      "max_cost_usd": 20,
      "stop_when_target_met": true,
      "stop_when_no_improvement_rounds": 2
    },
    "apply_policy": {
      "apply_scope": "workspace",
      "requires_user_confirmation": true
    },
    "promotion_policy": {
      "initial_scope": "workspace",
      "max_auto_scope": "session",
      "requires_explicit_promotion": true
    },
    "measurement_plan": {
      "window": "next_20_matching_cases",
      "compare_to": "baseline_and_previous_version",
      "rollback_on": [
        "safety_regression",
        "primary_metric_regression",
        "latency_constraint_violation"
      ]
    }
  },
  "status": {
    "observed_generation": 1,
    "phase": "created",
    "conditions": [
      {
        "type": "SpecAccepted",
        "status": "true",
        "reason": "ValidSpec",
        "message": "The tuning spec is valid and ready for baseline.",
        "last_transition_ref": "urn:astra:event:local:event_01H00000000000000000000001"
      }
    ],
    "budget_usage": {
      "rounds_used": 0,
      "candidates_evaluated": 0,
      "cost_usd": 0
    },
    "current_reconcile": {
      "next_controller": "BaselineController",
      "blocked_reason": null
    }
  }
}
```

`spec` is the desired state and reproducibility boundary. `status` is derived
observed state. Controllers may update status, conditions, events, and derived
records, but they must not mutate `spec` except through an explicit new
generation.

### Status Conditions

Conditions are the target user and agent-facing status contract. MVP can start
with a small `phase`, `reason`, `selected_candidate_ref`, and `decision_ref`
shape, then graduate to conditions when CLI, UI, SDK, and subagents need a
stable status API.

Recommended condition types:

- `SpecAccepted`
- `BaselineReady`
- `Diagnosed`
- `CandidatesReady`
- `DevEvaluationReady`
- `FinalistSelected`
- `HoldoutReady`
- `ApplyRecommended`
- `AwaitingApproval`
- `Applied`
- `Measuring`
- `MeasurementReady`
- `RollbackRecommended`
- `PromotionRecommended`
- `Complete`
- `Blocked`
- `Failed`

Each condition should include `status`, `reason`, `message`, timestamp or event
ref, and evidence refs when relevant. Status should also expose budget usage,
selected candidate refs, current controller, and next required action.

### Objective Function

The objective function should model optimization explicitly:

```text
maximize exact_match_accuracy
subject to prompt_tokens <= 3500
subject to p95_latency_ms <= 5000
subject to sql_safety_pass_rate >= 0.99
subject to no regression on schema_linking_accuracy
prefer lower cost and smaller artifacts when candidates tie
```

Hard constraints filter candidates before ranking. Secondary objectives rank
remaining candidates. The supervisor should prefer the smallest effective
change that satisfies the target and constraints.

For richer jobs, the objective can be represented as a small DSL:

```text
maximize exact_match_accuracy
subject_to prompt_tokens <= 3500
subject_to p95_latency_ms <= 5000
subject_to cost_per_case_usd <= 0.02
subject_to sql_safety_pass_rate >= 0.99
score = exact_match_accuracy - latency_penalty - cost_penalty
```

The MVP does not need a full parser. It should store equivalent structured
fields and reserve the DSL as the user-facing and future API shape.

### Search Budget and Stop Conditions

Search must be bounded before candidate generation starts.

| Field | Meaning |
| --- | --- |
| `max_rounds` | Maximum diagnose/generate/evaluate iterations |
| `max_candidates_per_round` | Candidate fan-out cap per round |
| `max_total_candidates` | Job-wide candidate cap |
| `max_eval_cases_per_candidate` | Evaluation sample cap before finalist selection |
| `max_wall_clock_minutes` | Runtime cap |
| `max_cost_usd` | Spend cap |
| `stop_when_target_met` | Stop early when a candidate passes all gates |
| `stop_when_no_improvement_rounds` | Stop after repeated rounds without meaningful improvement |

When the budget is exhausted, the job should finish with `rejected` or
`blocked`, not continue searching silently.

### Job State Machine

`TuningJob` should be durable and resumable.

| State | Meaning |
| --- | --- |
| `created` | Job accepted but not started |
| `baseline_running` | Baseline harness run in progress |
| `diagnosing` | Failure clusters and hypotheses being generated |
| `round_planning` | Supervisor chooses hypotheses and candidate budget for the next round |
| `generating_candidates` | Candidate artifacts being produced |
| `evaluating_candidates` | Development split evaluation in progress |
| `selecting` | Supervisor comparing candidates |
| `holdout_running` | Finalist holdout or regression run in progress |
| `awaiting_approval` | Candidate recommended but not applied |
| `applying` | Write-side channel is applying approved intervention |
| `measuring` | Post-apply measurement window active |
| `promotion_review` | Supervisor decides keep, rollback, or promote after measurement |
| `complete` | Job finished successfully |
| `rejected` | No acceptable candidate found or approval denied |
| `blocked` | Job needs user input or missing harness/data |
| `failed` | Infrastructure or harness failure |

State transitions must be evented so future reflect measurement views can
explain where the job stands and why.

`phase` should stay coarse and mostly linear. `conditions` carry richer,
composable status such as `AwaitingApproval`, `BudgetExhausted`,
`HoldoutFailed`, `RollbackRecommended`, or `Blocked`. This keeps clients from
depending on a large cross-product state enum.

### Failure Cluster Shape

Failure clusters sit between signals and hypotheses:

```text
Signal
  -> FailureCluster
  -> Hypothesis
  -> Candidate
  -> Evaluation
  -> Decision
```

They group related failures so the tuning job can reason about patterns rather
than individual failed cases.

```json
{
  "failure_cluster": {
    "cluster_ref": "urn:astra:failure_cluster:graph:fc_schema_linking_01H00000000000000000000001",
    "job_ref": "urn:astra:job:local:tune_01H00000000000000000000001",
    "label": "schema_linking_ambiguity",
    "summary": "NL2SQL failures cluster around similarly named columns in related tables.",
    "source_signal_refs": [
      "urn:astra:signal:graph:sig_context_policy_01H00000000000000000000001"
    ],
    "observation_refs": [
      "urn:astra:observation:graph:obs_sql_017",
      "urn:astra:observation:graph:obs_sql_044"
    ],
    "failed_case_refs": [
      "urn:astra:evaluation:external:nl2sql_eval_v3:dev:017",
      "urn:astra:evaluation:external:nl2sql_eval_v3:dev:044"
    ],
    "confidence": {
      "classification": 0.81,
      "evidence": 0.76
    }
  }
}
```

### Hypothesis Shape

Hypotheses make the search inspectable and prevent candidate generation from
becoming opaque prompt churn.

```json
{
  "hypothesis": {
    "hypothesis_ref": "urn:astra:hypothesis:local:hyp_01H00000000000000000000001",
    "job_ref": "urn:astra:job:local:tune_01H00000000000000000000001",
    "summary": "NL2SQL failures cluster around schema-linking ambiguity for similarly named columns.",
    "causal_level": "likely_causal",
    "failure_cluster_refs": [
      "urn:astra:failure_cluster:graph:fc_schema_linking_01H00000000000000000000001"
    ],
    "evidence_refs": [
      "urn:astra:evaluation:external:nl2sql_eval_v3:dev:017",
      "urn:astra:evaluation:external:nl2sql_eval_v3:dev:044",
      "urn:astra:observation:graph:obs_context_01H00000000000000000000001"
    ],
    "proposed_change_types": [
      "prompt_patch",
      "context_policy_patch"
    ],
    "expected_metric_effect": {
      "exact_match_accuracy": "+0.03 to +0.06",
      "prompt_tokens": "+100 to +400"
    }
  }
}
```

### Tunable Surfaces

Benchmark-driven tuning may propose changes to multiple surfaces.

| Surface | Examples | Channel |
| --- | --- | --- |
| Prompt | Rewrite system instructions, compress examples, add error-specific guidance | `prompt_runtime` for temporary tests, then `runtime_policy` or artifact patch |
| Skill | Modify NL2SQL skill instructions, when-to-use rules, examples, allowed tools | `skill_maintenance` |
| Tool schema | Clarify SQL generator arguments, add enum constraints, improve descriptions | `tool_maintenance` |
| Tool implementation | Add validator, SQL repair pass, schema linker, execution checker | `tool_maintenance` |
| Context policy | Retrieve schema docs, relevant tables, few-shot examples, prior errors | `runtime_policy` / context provider |
| Model routing | Route hard cases to a stronger model or specialized agent | `model_routing` |
| Memory lesson | Store reusable procedural lessons from repeated failures | `memory_feedback` |

The tuning job may explore these surfaces, but each resulting intervention keeps
its own channel, scope, approval, and rollback policy.

### Harness Integration

A tuning job needs a harness contract. The harness may be:

- An existing external benchmark harness.
- An Astra-owned evaluation harness.
- A self-harness created by the tuning agent for the task.
- A multi-agent harness where subagents generate, evaluate, and critique
  candidates.

Minimal harness contract:

```json
{
  "harness": {
    "id": "nl2sql_eval_v3",
    "input_schema": {
      "question": "string",
      "database_schema": "object",
      "gold_sql": "string"
    },
    "metrics": [
      "exact_match_accuracy",
      "execution_accuracy",
      "sql_safety_pass_rate",
      "prompt_tokens",
      "latency_ms",
      "cost_usd"
    ],
    "run_modes": [
      "baseline",
      "candidate",
      "holdout"
    ]
  }
}
```

Harness outputs become experimental evidence in the Adaptation Graph:

```json
{
  "benchmark_result": {
    "run_ref": "urn:astra:evaluation:local:eval_01H00000000000000000000001",
    "candidate_ref": "urn:astra:candidate:local:cand_prompt_01H00000000000000000000001",
    "metrics": {
      "exact_match_accuracy": 0.89,
      "execution_accuracy": 0.93,
      "prompt_tokens": 3280,
      "p95_latency_ms": 4200
    },
    "failed_case_refs": [
      "urn:astra:evaluation:external:nl2sql_eval_v3:dev:017",
      "urn:astra:evaluation:external:nl2sql_eval_v3:dev:044"
    ],
    "passed_constraints": true
  }
}
```

### Optimization Loop

MVP benchmark-driven tuning should prove the smallest useful loop:

```text
Signal
-> FailureCluster
-> Hypothesis
-> Candidate
-> Evaluation
-> Decision
```

The full target loop can extend that core:

```text
validate TuningSpec
-> run baseline
-> inspect failures and traces
-> create failure clusters
-> generate hypotheses
-> allocate round budget
-> propose candidate artifacts
-> run candidate evaluations
-> supervise and critique results
-> select or iterate
-> run holdout/regression checks
-> request approval
-> apply scoped changes
-> measure future production outcomes
-> keep or rollback
```

The job should preserve every candidate and benchmark result as evidence, even
for rejected candidates. Failed candidates are useful for future learning.

### Candidate Search Strategy

A tuning job should evaluate multiple candidates under the `SearchBudget`.
Single-candidate tuning is allowed for small jobs, but benchmark-driven
optimization should default to a bounded round-based search:

1. Generate 2-5 candidate hypotheses for the first round.
2. Produce at most one artifact per hypothesis unless the supervisor allocates
   additional budget.
3. Run candidates on the development split or a bounded development sample.
4. Filter candidates that violate hard constraints.
5. Rank remaining candidates with the objective function.
6. Select a small finalist set.
7. Run holdout and regression checks only for finalists.
8. Recommend apply only if all acceptance gates pass.
9. Stop when the target is met, budget is exhausted, or no meaningful
   improvement appears for the configured number of rounds.

Search should balance exploitation and exploration:

- Allocate about 70% of candidate budget to improving the best current
  hypothesis or candidate family.
- Allocate about 30% to new failure clusters, alternative explanations, or
  different change surfaces.
- If all exploitation candidates plateau, shift the next round toward
  exploration.
- If an exploratory candidate wins, make it the new exploitation baseline.

Pareto comparison should consider:

- Primary metric improvement
- Prompt or artifact size
- Latency
- Cost
- Safety pass rate
- Regression count
- Scope and rollback complexity

The supervisor should prefer the smallest effective change. A 1-point accuracy
gain from a 3,000-token prompt increase should lose to a 0.8-point gain from a
200-token skill clarification when both satisfy the target.

### Candidate Lifecycle

Candidates must move through explicit states instead of being implicit files or
agent messages.

| State | Meaning |
| --- | --- |
| `proposed` | Candidate was generated but not materialized |
| `materialized` | Candidate artifact or intervention exists and has stable refs |
| `dev_evaluating` | Candidate is running on development data |
| `dev_passed` | Candidate passed development constraints |
| `dev_failed` | Candidate failed development constraints |
| `finalist` | Candidate selected for holdout/regression |
| `holdout_passed` | Candidate passed holdout and regression gates |
| `holdout_failed` | Candidate failed holdout or regression |
| `recommended` | Supervisor recommends scoped apply |
| `applied` | Approved candidate was applied through a write-side channel |
| `measuring` | Production measurement window is active |
| `kept` | Measurement supports keeping at current scope |
| `rolled_back` | Candidate was reverted after failure or regression |
| `promoted` | Candidate was approved for broader scope |

State changes must emit audit events with source hypothesis refs, candidate
artifact refs, evaluation refs, and decision refs.

### Overfitting and Leakage Controls

Benchmark-driven tuning must include guardrails:

- Baseline, development, and holdout splits must be distinct.
- Candidate-generation agents must not see holdout gold answers.
- The holdout split should be run late, after candidate selection.
- Failed cases may be summarized for diagnosis, but direct answer leakage into
  prompts or skills is forbidden.
- Regression cases must include prior passing examples.
- Prompt patches should be inspected for benchmark-specific memorization.
- Any candidate that improves the primary metric by violating safety or leakage
  constraints is rejected.

For NL2SQL specifically:

- Do not include gold SQL from holdout in generated prompts or skill examples.
- Evaluate both exact-match and execution accuracy.
- Track SQL safety and schema-linking metrics separately.
- Check whether improvements are limited to one schema or generalize across
  schemas.

### Subagent Supervision

A tuning job may use subagents, but the supervisor must own the final decision.

Useful roles:

| Role | Responsibility |
| --- | --- |
| `diagnostician` | Cluster failures and identify likely causes |
| `prompt_candidate_agent` | Propose prompt changes under size constraints |
| `skill_candidate_agent` | Propose skill edits and example changes |
| `tool_candidate_agent` | Propose schema or implementation improvements |
| `evaluator` | Run harness and summarize metrics |
| `critic` | Check overfitting, leakage, regressions, and constraint violations |
| `supervisor` | Select candidates, enforce approval policy, and decide whether to stop |

Supervision rules:

- Candidate generation and evaluation should be separated where possible.
- The evaluator should not know holdout answers beyond the harness contract.
- The critic must check prompt-size, latency, safety, and regression
  constraints.
- The supervisor must reject candidates that improve the target metric by
  violating constraints.
- Durable changes require explicit approval according to scope.

### Candidate Artifacts

Candidate interventions may reference concrete artifacts:

```json
{
  "candidate_artifact": {
    "candidate_ref": "urn:astra:candidate:local:cand_prompt_01H00000000000000000000001",
    "artifact_type": "prompt_patch",
    "base_ref": "urn:astra:artifact:local:prompt_nl2sql_v4",
    "diff_ref": "urn:astra:artifact:local:prompt_patch_diff_01H00000000000000000000001",
    "estimated_prompt_tokens": 3280,
    "source_signal_refs": [
      "urn:astra:signal:graph:sig_context_policy_01H00000000000000000000001",
      "urn:astra:signal:graph:sig_tool_schema_01H00000000000000000000001"
    ],
    "constraints": {
      "prompt_tokens_max": 3500
    }
  }
}
```

Full artifact type vocabulary:

- `prompt_patch`
- `skill_patch`
- `tool_schema_patch`
- `tool_impl_patch`
- `context_policy_patch`
- `model_routing_patch`
- `memory_lesson_candidate`

MVP should only support `prompt_patch`, `skill_patch`, and
`context_policy_patch`. Other artifact types require the later-phase safeguards
listed in the implementation plan.

### Acceptance Gates

A tuning job can only recommend apply when all required gates pass.

Common gates:

- Target metric meets or exceeds goal.
- Prompt size stays under limit.
- Latency and cost stay under limit.
- Holdout performance improves or does not regress.
- Safety checks pass.
- No benchmark leakage is detected.
- Regression suite passes.
- Candidate has rollback plan.
- Required approval is available.

Example decision:

```json
{
  "tuning_decision": {
    "job_ref": "urn:astra:job:local:tune_01H00000000000000000000001",
    "selected_candidate_ref": "urn:astra:candidate:local:cand_prompt_01H00000000000000000000001",
    "decision": "recommend_apply",
    "reason": "Accuracy reached 0.89 with prompt_tokens=3280 and no holdout regression.",
    "gates": {
      "target_metric": "passed",
      "prompt_size": "passed",
      "latency": "passed",
      "holdout": "passed",
      "safety": "passed",
      "rollback": "passed"
    },
    "requires_confirmation": true
  }
}
```

### Apply and Promotion Split

Applying a candidate and promoting it are separate decisions.

MVP should stop at:

```text
apply -> measure -> done_or_rollback
```

Promotion is intentionally out of MVP. It becomes relevant only after candidate
search, evaluation quality, and workspace-scope measurement are reliable.

| Decision | Meaning | Typical Evidence |
| --- | --- | --- |
| `apply_ephemeral` | Use candidate for current turn or session | Local signal and low-risk runtime policy |
| `apply_workspace` | Apply candidate to this workspace/repository | Benchmark result, holdout, rollback plan, user confirmation |
| `keep_current_scope` | Continue using applied candidate at same scope | Post-apply measurement without regression |
| `rollback` | Revert applied candidate | Regression, safety issue, or violated constraint |
| `promote_user_profile` | Apply beyond workspace to user's future sessions | Cross-session measurement and explicit user confirmation |
| `promote_team` | Apply to shared team assets | Review workflow and admin approval |
| `promote_global` | Product-level rollout | Engineering release process |

The tuning control plane may recommend any of these decisions, but only the
appropriate write-side channel can execute them. Promotion must cite both the
original candidate evidence and post-apply measurement evidence.

Example promotion decision:

```json
{
  "promotion_decision": {
    "job_ref": "urn:astra:job:local:tune_01H00000000000000000000001",
    "applied_intervention_ref": "urn:astra:intervention:local:intervention_01H00000000000000000000001",
    "decision": "keep_current_scope",
    "reason": "Production measurement improved execution accuracy without latency or safety regression.",
    "measurement_refs": [
      "urn:astra:measurement:local:measurement_01H00000000000000000000001"
    ],
    "next_review": "after_next_100_matching_cases"
  }
}
```

### Finalizers and Cleanup

Any tuning job that applies temporary runtime state should register a finalizer.
Finalizers prevent a job from disappearing while cleanup or measurement is still
required.

Finalizers are target-architecture mechanics. MVP can implement equivalent
cleanup with explicit job status and rollback records, then introduce finalizers
when jobs become long-running or externally cancellable.

Recommended finalizers:

- `finalizer.astra.ai/rollback-runtime-policy`
- `finalizer.astra.ai/expire-prompt-nudges`
- `finalizer.astra.ai/complete-measurement-window`
- `finalizer.astra.ai/promotion-review`
- `finalizer.astra.ai/archive-candidate-artifacts`

Finalizers should be cleared only after the relevant rollback, expiration,
measurement, promotion review, or archival event has been written. A failed
finalizer should set a `Blocked` or `Failed` condition with evidence refs.

## Channel Semantics

### `user_feedback`

Use when the best response is to ask for, record, or surface user feedback.

Examples:

- Ask the user to disambiguate intent.
- Record that a result was unhelpful.
- Surface a concise user-facing suggestion.

### `runtime_policy`

Use for session-scoped or workspace-scoped runtime behavior.

Examples:

- Deprioritize a tool in the current session.
- Adjust retry defaults.
- Change context-release behavior.

### `memory_feedback`

Use when a stable lesson should be remembered, corrected, demoted, or forgotten.

Examples:

- Store a procedural lesson after repeated evidence.
- Mark a memory as stale.
- Attach negative feedback to an incorrect lesson.

### `skill_maintenance`

Use when the skill content or metadata appears to cause repeated bad behavior.

Examples:

- Patch a skill instruction.
- Update allowed tools.
- Improve when-to-use guidance.

Skill changes should be reviewable and should not be applied silently at broad
scope.

### `tool_maintenance`

Use when the tool itself needs a schema, validation, timeout, transport, or
implementation change.

Examples:

- Add schema hints.
- Improve argument validation.
- Change timeout defaults.
- File a tool-upgrade request.

### `prompt_runtime`

Use for short-lived corrective guidance injected into the runtime prompt lane.

Examples:

- Warn the agent to stop repeating a failing action.
- Inject a temporary task-specific lesson.
- Remind the agent that a context fact is stale.

Prompt runtime changes must expire automatically.

### `model_routing`

Use when evidence suggests a model class or routing rule is underperforming for
a task class.

Examples:

- Route codebase forensics to a stronger reasoning model.
- Avoid a model/tool combination that repeatedly emits invalid tool arguments.

## Approval and Safety Rules

Downstream tuning must be governed by safety rules:

- `introspect` and `reflect` cannot directly call mutating tools.
- Any durable intervention must cite one or more `adaptation_signals`.
- Higher scopes require stronger confirmation.
- Skill and tool upgrades should go through reviewable artifacts or PR-like
  workflows.
- Runtime prompt nudges must be scoped and expire automatically.
- Memory lessons must preserve source evidence and support correction.
- Every applied intervention emits an audit event with source signal refs.
- Rollback must be available for session/workspace runtime policy changes when
  feasible.
- Broad user-profile, team, or global changes require explicit governance.

## Measurement

The loop is not complete until later observations can evaluate the intervention
outside the benchmark. Measurement is a first-class output of tuning, not a
best-effort afterthought.

The measurement plan comes from `TuningSpec` and should define:

- Matching criteria for future cases.
- Comparison baseline.
- Minimum sample size or time window.
- Metrics to track.
- Rollback triggers.
- Promotion review criteria.

Long term, measurement should become its own small subsystem:

| Entity | Meaning |
| --- | --- |
| `MeasurementPlan` | What to measure, over which future cases, and against which baseline |
| `MeasurementRun` | One observed measurement window or evaluation query |
| `MeasurementDecision` | Done, continue measuring, rollback, or later promotion review |

MVP may keep these fields embedded in `TuningSpec` and `MeasurementReport`.

The observation plane should support before/after comparison:

```json
{
  "measurement": {
    "measurement_ref": "urn:astra:measurement:local:measurement_01H00000000000000000000001",
    "intervention_ref": "urn:astra:intervention:local:intervention_01H00000000000000000000001",
    "source_signal_refs": [
      "urn:astra:signal:graph:sig_tool_policy_01H00000000000000000000001"
    ],
    "window": "next_20_matching_cases",
    "compare_to": "baseline_and_previous_version",
    "metrics": {
      "tool_timeout_rate_before": 0.6,
      "tool_timeout_rate_after": 0.1,
      "repeated_error_count_before": 5,
      "repeated_error_count_after": 1
    },
    "assessment": "improved",
    "confidence": {
      "classification": 0.82,
      "evidence": 0.78,
      "causal": 0.71
    },
    "recommended_next_decision": {
      "decision": "keep_current_scope",
      "reason": "Observed improvement meets measurement plan and no rollback trigger fired."
    }
  }
}
```

This measurement remains observational. If the system decides to persist,
promote, revert, or broaden an intervention, it must do so through the
appropriate write-side channel.

Measurement can produce three outcomes:

| Outcome | Meaning | Next Step |
| --- | --- | --- |
| `improved` | Metrics improved and constraints still hold | Keep or review for promotion |
| `inconclusive` | Sample is too small or evidence is weak | Continue measuring or run another benchmark |
| `regressed` | Primary, safety, latency, cost, or regression constraints failed | Recommend rollback |

## Example Flows

### Live Tool Failure

```text
introspect(topic="execution", facet="errors", depth="diagnostic")
-> adaptation signal: repeated_timeout for bash
-> feedback service classifies as tool_parameters + tool_policy
-> proposes session-scoped timeout/default change
-> user or policy approves
-> runtime_policy applies change
-> later introspect/reflect measures timeout rate
```

### User Correction

```text
user says result is wrong
-> reflect(topic="overview", facet="question", depth="diagnostic")
-> adaptation signal: user_guidance or memory_lesson
-> feedback service records correction
-> optional memory_feedback updates lesson
-> later reflect checks whether similar future sessions improved
```

### Skill Improvement

```text
reflect(topic="execution", facet="errors", horizon="session", depth="forensic")
-> selected failure refs show repeated bad skill guidance
-> feedback service opens skill patch proposal
-> review workflow approves
-> skill registry updates version
-> later sessions compare outcomes by skill version
```

### NL2SQL Benchmark Tuning

```text
/tuning goal="Improve NL2SQL exact-match accuracy to 88%"
        benchmark="nl2sql_eval_v3"
        constraints="prompt_tokens <= 3500, p95_latency_ms <= 5000"
        allowed_changes="prompt_patch,skill_patch,context_policy_patch"
-> control plane creates TuningSpec and TuningJob
-> harness runs baseline on dev split
-> diagnostician clusters schema-linking failures
-> candidate agents produce bounded prompt/skill/context-policy patches
-> evaluator runs candidate benchmarks
-> critic checks leakage, regressions, cost, latency, and safety
-> supervisor selects finalist and runs holdout
-> supervisor recommends workspace apply if all gates pass
-> user approves apply
-> measurement plan tracks next matching production cases
-> supervisor marks done or recommends rollback
-> later production-learning phase may consider promotion
```

## Implementation Plan

### Phase 1: Observation Plane

Define the read-only evidence base:

- Event
- Decision
- Observation
- Signal

### Phase 2: Introspect and Reflect

Expose unified graph views over the layered graph model:

- Runtime graph slices
- Observation graph slices
- Adaptation signal views
- Coverage and budget metadata

### Phase 3: Tuning MVP

Prove the core loop before implementing the full operator architecture:

```text
Signal -> FailureCluster -> Hypothesis -> Candidate -> Evaluation -> Decision
```

MVP schemas:

- `TuningSpec` with embedded objective and search budget
- `TuningJob`
- `FailureCluster`
- `TuningHypothesis`
- `CandidateArtifact`
- `EvaluationRun`
- `TuningDecision`

MVP execution:

1. Add `TuningJobRepository` with `create`, `get`, `list`, `checkpoint`,
   `append_event`, `transition`, and `cancel` operations.
2. Add `tuning_jobs` and `tuning_events` persistence.
3. Use one `JobController`, not many controllers.
4. Support `/tuning` job creation from a bounded `TuningSpec`.
5. Add minimal API routes:
   - `POST /tuning/jobs`
   - `GET /tuning/jobs`
   - `GET /tuning/jobs/{job_id}`
   - `POST /tuning/jobs/{job_id}/start`
   - `POST /tuning/jobs/{job_id}/cancel`
   - `GET /tuning/jobs/{job_id}/events`
6. Support only:
   - `prompt_patch`
   - `skill_patch`
   - `context_policy_patch`
7. Integrate one harness path with baseline/dev/holdout or regression-suite
   support.
8. Enforce `SearchBudget` before candidate generation.
9. Generate bounded candidate rounds and evaluate them on the development split.
10. Produce `recommend_apply`, `reject`, or `iterate` decisions without automatic
   apply.
11. Persist audit refs linking signals, clusters, hypotheses, candidates,
   evaluations, and decisions.

### Phase 4: Apply and Measurement

Add write-side apply only after the tuning MVP can produce useful decisions.

```text
Apply -> Measure -> Done or Rollback
```

Add:

- `ApplyDecision`
- `AppliedIntervention`
- `MeasurementPlan`
- `MeasurementRun`
- `MeasurementDecision`
- `MeasurementReport`

Finalizers may be introduced here only if jobs become long-running or need
external cancellation cleanup.

### Phase 5: Production Learning

Add broader learning only after apply and measurement are reliable:

- Promotion review
- Cross-session learning
- User-profile, team, or global promotion
- Multi-controller decomposition
- Tool schema and tool implementation patches
- Model-routing changes
- Periodic production-learning jobs

## Open Questions

1. Which harness interface should be canonical for external benchmarks?
2. Should candidate interventions be generated deterministically, by an LLM, or
   by a hybrid service?
3. Which current-turn/session scopes may the agent apply automatically without
   user confirmation?
4. Should prompt runtime nudges be visible in transcript by default?
5. How should skill/tool maintenance proposals map to repository PRs versus
   registry updates?
6. When should repeated session-scoped improvements be promoted to workspace or
   user-profile scope?
7. What confidence threshold is required before a memory lesson can be written?
8. Should `TuningJob` resources live in the existing database, an event-sourced
   store, or a CRD-like resource API?
9. What retry/backoff policy should each controller use for blocked harnesses,
   failed evaluations, or unavailable write-side channels?
10. How long should completed jobs, rejected candidates, and finalizer evidence
   remain queryable?
11. Should `JobController` run in the CLI process, server worker pool, or a
   shared queue/lease system when Edge and Server both can observe the job?
