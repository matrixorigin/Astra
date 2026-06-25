# Introspect and Reflect Observation Plane

**Date**: 2026-06-24
**Status**: Foundation Implemented; Provider and Persistence Phases Pending
**Audience**: Astra runtime, CLI, server, observability, learning, and SDK maintainers

## Executive Summary

Astra needs a single observation plane for agent self-understanding and session
diagnostics. The system should let the agent and user answer:

- What am I doing right now?
- How well is it going?
- What failed or degraded?
- What caused the failure?
- Which evidence supports that conclusion?
- Which structured observations can later be consumed by feedback, memory,
  policy, or tuning systems?

The core product asset is not the `introspect` tool or the `reflect` tool. The
core asset is the **Observation Graph**.

```text
Providers
  -> Inspection Service
  -> Observation Graph
    -> Introspect View / Reflect View / Debug UI / Timeline UI / Memory Analysis / Session Replay
    -> Adaptation Signals
      -> Tuning Control Plane / Feedback Tools
```

This design keeps two user-facing tool facades:

- `introspect`: live observation for the current runtime, current turn, and
  recent rounds.
- `reflect`: retrospective analysis over a historical window, selected
  execution trace, session, or explicitly authorized cross-session window.

Both tools are read-only. They observe, classify, correlate, and expose
evidence. They do not tune policies, write memories, submit feedback, or mutate
runtime configuration. They are views over the Observation Graph.

The Tuning Control Plane may consume graph slices and adaptation signals, then
write its own `TuningJob` status, reconcile events, evaluations, and
measurements back as graph evidence. That feedback path is outside the
`introspect` and `reflect` tool facades.

Current implementation status:

- Shared observation DTOs, evidence refs, confidence dimensions, failure
  clusters, adaptation signals, graph slices, data coverage, and budget result
  wire shapes are implemented in `astra_core`.
- `introspect` and `reflect` expose normalized observation-plane envelopes with
  `topic`, `facet`, `depth`, `horizon`, `source_policy`, `include_context`,
  `data_coverage`, observations, evidence, graph slices, failure clusters,
  causal chains, adaptation signals, action hints, and budget metadata.
- Legacy `introspect` parameters such as `subtopic` and `detail`, legacy
  `reflect` focus-style routing, and the public `reflect.evidence_graph`
  response field are removed from the runtime contract.
- A common `InspectionService` provider-fusion layer and persistent Observation
  Graph storage are follow-up phases, not current behavior.

## Design Principles

1. **Read-only by construction**
   `introspect` and `reflect` never apply adaptation. They only return
   observations and evidence.

2. **Same semantics across runtime modes**
   CLI/Edge and pure Server modes may have different available data, but the
   same request should return the same conceptual schema. Missing data is
   represented as coverage, not as a different behavior.

3. **Runtime errors are first-class data**
   Errors are not just counters. They are classified observations linked to
   tool calls, decisions, context state, traces, and causal chains.

4. **Evidence beats advice**
   The tools may include read-only action hints, but their primary output is
   evidence and structured signals that another tool can consume.

5. **Depth controls cost and blast radius**
   Agent-initiated calls default to compact live summaries. User-triggered or
   explicit forensic calls can return deeper evidence.

6. **Data provenance is always visible**
   Every response declares which providers contributed data, how fresh they
   are, and which expected providers were missing or stale.

7. **Graph first, views second**
   Debug UI, Timeline UI, Memory Analysis, Feedback Training, Session Replay,
   and the Tuning Control Plane should consume the same Observation Graph
   instead of each building a private interpretation of events.

## Non-Goals

- Applying tool priority changes.
- Writing memory or procedural lessons.
- Submitting learning feedback.
- Creating or reconciling `TuningJob` resources.
- Changing model routing, retry policy, prompt policy, or context policy.
- Scanning arbitrary workspace files outside already observed facts.
- Hiding Edge/Server data differences behind silent fallback.

## Why Two Tools?

The tools should be separated by time horizon and usage pattern, not by data
source.

| Tool | Primary Question | Default Horizon | Default Caller | Cost Profile |
| --- | --- | --- | --- | --- |
| `introspect` | What is happening now, and what active problems affect my next step? | `current_turn` | Agent | Low |
| `reflect` | What happened over a historical window, why, and with what evidence? | `session` | User or agent | Medium to high |

The implementation should share a common inspection service and provider layer.
The tools are facades with different defaults, budgets, and presentation
policies.

## Conceptual API

### `introspect`

```text
introspect(
  topic = "runtime",
  facet = null,
  depth = "summary",
  horizon = "current_turn",
  source_policy = "auto",
  budget = null,
  include_context = false
)
```

`introspect` is optimized for live self-awareness:

- Current objective and phase, when known
- Current model and runtime binding
- Current and recent tool health
- Runtime errors and warnings
- Token pressure and cache behavior
- Stall/loop guard state
- Context freshness and volatile injections
- Short causal chains for recent failures
- Read-only adaptation signals derived from live observations

### `reflect`

```text
reflect(
  topic = "overview",
  facet = null,
  depth = "diagnostic",
  horizon = "session",
  source_policy = "auto",
  budget = null,
  include_context = false,
  selector = null,
  question = null
)
```

`reflect` is optimized for retrospective diagnosis:

- Session overview or selected trace analysis
- Failure root cause
- Tool-selection outcomes
- Performance bottlenecks
- Context quality and drift
- Memory extraction/injection behavior
- Durable evidence graph
- Read-only adaptation signals derived from historical evidence

## Horizons

Use `horizon` rather than overloading `scope` with multiple meanings.
`horizon` is only a time/window selector. It must not describe observation
content.

| Horizon | Meaning | Typical Tool |
| --- | --- | --- |
| `now` | Current runtime snapshot only | `introspect` |
| `current_turn` | Current turn and recent in-memory rounds | `introspect` |
| `recent` | Recent N turns or events | Both |
| `turn` | One explicit turn | `reflect` |
| `session` | Full active or selected session | `reflect` |
| `cross_session` | Explicitly authorized multi-session window | `reflect` |

`cross_session` must never be implicit. It requires an explicit user request,
policy approval, or a narrowly bounded learning workflow.

Specific traces or chains are selected with `selector`, not `horizon`:

```json
{
  "topic": "execution",
  "facet": "trace",
  "horizon": "session",
  "selector": {
    "trace_ref": "urn:astra:trace:cloud:trace_01H00000000000000000000001"
  }
}
```

## Topics

### Canonical Topic Vocabulary

Use a small top-level topic vocabulary. Detailed views are expressed through
`facet`.

| Topic | Meaning |
| --- | --- |
| `overview` | Summary of the selected horizon and graph slice |
| `runtime` | Live model, turn, execution binding, budget, cache, compaction, capability |
| `execution` | Actions, errors, tools, traces, metrics, progress, loops |
| `knowledge` | Context, memory, retrieval, facts, drift |
| `adaptation` | Read-only signals for future feedback/policy tools |

`tuning` may remain as a user-facing alias for `adaptation`, but the canonical
topic should be `adaptation`. This avoids implying that the tool applies tuning.

### Facets

`facet` narrows a top-level topic.

| Topic | Common Facets |
| --- | --- |
| `runtime` | `budget`, `cache`, `capability`, `binding`, `model` |
| `execution` | `progress`, `errors`, `tools`, `trace`, `performance`, `loop` |
| `knowledge` | `context`, `memory`, `retrieval`, `facts`, `drift` |
| `adaptation` | `signals`, `measurements`, `candidates` |
| `overview` | `summary`, `question` |

For CLI ergonomics, slash commands may accept path-like aliases such as
`execution/errors`, `knowledge/memory`, or `runtime/budget`. Internally these
normalize to `{topic, facet}`.

### Recommended Defaults

| Tool | Default Topic | Default Facet |
| --- | --- | --- |
| `introspect` | `runtime` | `summary` |
| `reflect` | `overview` | `summary` |

Aliases are useful for CLI compatibility and user ergonomics, but internal
storage should normalize to canonical topic and facet values.

## Depth

Depth controls output volume and evidence detail.

| Depth | Intended Use | Output |
| --- | --- | --- |
| `hint` | Agent needs a tiny live nudge | Top 1-3 observations, no heavy queries |
| `summary` | Normal live self-check | Status, active problems, compact evidence refs |
| `diagnostic` | Root-cause analysis | Classified observations, causal summaries, metrics |
| `forensic` | User/debug deep dive | Evidence graph, timeline, provider details |

Defaults:

- Agent-initiated `introspect`: `summary`
- Runtime-triggered loop/error self-check: `diagnostic` with strict budget cap
- User-triggered `/reflect`: `diagnostic`
- Explicit `/reflect execution/trace forensic`: `forensic`

## Result Budgets

`forensic` does not mean unlimited. Every request may include a budget, and
every response should report truncation.

Example:

```json
{
  "depth": "forensic",
  "budget": {
    "max_events": 100,
    "max_chains": 20,
    "max_nodes": 500,
    "max_evidence_previews": 50,
    "max_tokens": 16000,
    "page_size": 100,
    "cursor": null
  }
}
```

Response budget metadata:

```json
{
  "budget_result": {
    "truncated": true,
    "next_cursor": "cursor_01H00000000000000000000001",
    "omitted": {
      "events": 930,
      "chains": 84,
      "nodes": 2450
    }
  }
}
```

Large sessions should return graph slices and cursors. UI and SDK consumers
should never be required to render an unbounded evidence graph.

## Source Policies

| Policy | Meaning |
| --- | --- |
| `auto` | Tool-specific default |
| `live_only` | Runtime state only; no durable reads |
| `live_first` | Prefer live/local state, merge durable data if available |
| `durable_first` | Prefer cloud/event-store data, merge live/local deltas |
| `local_only` | CLI/Edge local artifacts only; no cloud reads |
| `cloud_only` | Cloud/server durable data only; no local artifacts |

Default expansion:

- `introspect(auto)` = `live_first`
- `reflect(auto)` = `durable_first`

`include_context` is a separate boolean request option, not a source policy.
When enabled, observed facts from the current prompt/context surface may be
included. Context facts must be marked as observed context, not as durable
truth.

## Data Providers

The observation plane is implemented as provider fusion. Each provider returns
typed observations, evidence refs, freshness, and limitations.

| Provider | CLI/Edge | Pure Server | Notes |
| --- | --- | --- | --- |
| `live_runtime` | Yes | Yes | Authoritative for current runtime state |
| `visible_context` | Yes | Sometimes | Available only when the prompt/context surface is accessible |
| `local_journal` | Yes | Usually no | May be fresher than cloud but local-only |
| `local_workspace_metadata` | Yes | Usually no | Session config, persistence errors, workspace hints |
| `llm_capture` | Yes if enabled | Sometimes | Useful for cache and prompt forensics |
| `cloud_events` | When authenticated | Yes | Durable session/event source |
| `decision_audits` | When authenticated | Yes | Tool/model/context decision evidence |
| `session_activity` | When authenticated | Yes | Durable activity timeline |
| `memory_backend` | Via local/cloud proxy | Yes | Cross-session memory and lessons |
| `trace_store` | When present | Yes | Causal chains and graph reconstruction |
| `tuning_control_plane` | When local job store exists | Yes | `TuningJob` resources, reconcile events, status conditions, evaluations, measurements |

CLI/Edge may be a superset of Server data, but only when it is authenticated
and local artifacts exist. The response must describe the actual provider set.

## Graph Layers and Unified View

The Inspection Service fuses provider outputs into graph-backed evidence, but it
should not create one unbounded "everything graph." The logical model has three
layers with different ownership, query patterns, and permissions.

| Layer | Primary Entities | Meaning |
| --- | --- | --- |
| Runtime Graph | `Event`, `Trace`, `Decision` | What happened during execution |
| Observation Graph | `Observation`, `Signal`, `CausalChain`, `FailureCluster` | What the system inferred from runtime evidence |
| Adaptation Graph | `Candidate`, `EvaluationRun`, `Intervention`, `MeasurementRun` | What experiments or interventions were tried and measured |

External callers still receive a unified `GraphSlice` view. Internally, the
layers keep runtime evidence, observations, and experimental evidence from
collapsing into `GraphNode { type: any }`.

```text
ProviderObservation
  -> normalize evidence refs
  -> merge duplicate entities
  -> classify observations
  -> attach confidence and provenance
  -> materialize graph slice
  -> render view
```

### Graph Model

| Entity | Meaning |
| --- | --- |
| `RuntimeNode` | Event, trace, decision, runtime signal |
| `ObservationNode` | Observation, causal chain, failure cluster, adaptation signal |
| `AdaptationNode` | Candidate, evaluation run, intervention, measurement run, tuning job/status |
| `GraphEdge` | Causal, temporal, support, contradiction, derived-from, duplicate-of, measured-by, reconcile relation |
| `Observation` | Classified finding over one or more graph nodes |
| `FailureCluster` | Group of related failures that should be diagnosed together |
| `EvidenceRef` | Stable reference to source evidence or graph node |
| `GraphSlice` | Bounded subset returned to a tool, UI, replay, or training consumer |

Consumers should ask for bounded graph slices, not the whole graph. Slices may
join across layers, but permissions and retention should remain layer-aware.

### Edge Kinds

Recommended edge kinds:

- `precedes`
- `causes`
- `likely_causes`
- `correlates_with`
- `supports`
- `contradicts`
- `derived_from`
- `duplicates`
- `measures`
- `references`
- `reconciles`
- `selects`
- `applies`
- `finalizes`

## Evidence References

Evidence refs must be standardized before providers, storage, SDKs, and UIs
invent incompatible identifiers.

Canonical format:

```text
urn:astra:<kind>:<namespace>:<id>
```

Examples:

```text
urn:astra:event:cloud:event_01H00000000000000000000001
urn:astra:event:edge:session_abc:seq_42
urn:astra:decision:cloud:decision_01H00000000000000000000001
urn:astra:trace:cloud:trace_01H00000000000000000000001
urn:astra:observation:graph:obs_01H00000000000000000000001
urn:astra:artifact:local:prompt_patch_01H00000000000000000000001
urn:astra:job:local:tune_01H00000000000000000000001
urn:astra:failure_cluster:graph:fc_01H00000000000000000000001
urn:astra:condition:local:cond_01H00000000000000000000001
urn:astra:reconcile:local:reconcile_01H00000000000000000000001
urn:astra:measurement:local:measurement_01H00000000000000000000001
```

Allowed `kind` values:

- `event`
- `decision`
- `trace`
- `observation`
- `signal`
- `memory`
- `artifact`
- `evaluation`
- `intervention`
- `spec`
- `job`
- `failure_cluster`
- `hypothesis`
- `candidate`
- `condition`
- `reconcile`
- `measurement`
- `context`

Allowed `namespace` values:

- `cloud`
- `edge`
- `local`
- `graph`
- `memory`
- `external`

Structured evidence refs should include parsed fields:

```json
{
  "ref": "urn:astra:event:edge:session_abc:seq_42",
  "kind": "event",
  "namespace": "edge",
  "source_provider": "local_journal",
  "stable": false,
  "aliases": [
    "urn:astra:event:cloud:event_01H00000000000000000000001"
  ]
}
```

Local refs may be unstable before cloud sync. When a cloud ID is assigned, the
graph should preserve an alias edge so old local refs remain resolvable.

## Evidence Classes

Evidence refs identify source material, but callers also need to know what kind
of evidence they are looking at.

| Evidence Class | Examples | Meaning |
| --- | --- | --- |
| `observed_evidence` | Runtime event, trace, decision, context fact | Something that happened or was present |
| `inferred_evidence` | Observation, causal chain, failure cluster, signal | A classified or inferred finding over observed evidence |
| `experimental_evidence` | Evaluation run, candidate result, measurement run | Result of a controlled or semi-controlled experiment |
| `audit_evidence` | Reconcile event, condition, apply decision | Control-plane or write-side decision record |

`Observation` should not be overloaded to mean `EvaluationRun`. Evaluation and
measurement results belong to experimental evidence. They may support or refute
observations, hypotheses, and adaptation decisions, but they are not themselves
runtime observations.

## Data Coverage

Every response includes coverage.

```json
{
  "data_coverage": {
    "overall": "partial",
    "providers": {
      "live_runtime": {
        "status": "fresh",
        "freshness_ms": 0
      },
      "local_journal": {
        "status": "fresh",
        "freshness_ms": 1800
      },
      "cloud_events": {
        "status": "stale",
        "freshness_ms": 45000
      },
      "memory_backend": {
        "status": "missing",
        "reason": "not_configured"
      }
    },
    "warnings": [
      "local_journal_ahead_of_cloud",
      "memory_backend_unavailable"
    ]
  }
}
```

Provider statuses:

- `fresh`
- `stale`
- `partial`
- `missing`
- `unavailable`
- `denied`
- `error`

The tools should prefer partial but explicit results over opaque fallback.

## Source Fusion Rules

For live state:

1. `live_runtime`
2. `visible_context`
3. `local_journal`
4. `cloud_events`

For historical state:

1. `cloud_events` and `decision_audits`
2. `trace_store`
3. `local_journal`
4. `llm_capture`
5. `visible_context`
6. `live_runtime`

For conflicts:

- Do not silently overwrite.
- Return both refs when useful.
- Emit a coverage warning such as `local_ahead_of_cloud`,
  `cloud_missing_local_event`, `event_version_conflict`, or
  `provider_clock_skew`.

## Shared Response Envelope

Both tools return the same observation envelope at the root. Some fields may be
empty depending on topic, depth, and coverage. Tool-specific compatibility
fields may still exist beside this envelope, but consumers should use the
observation-plane fields for new integrations.

```json
{
  "schema_version": 1,
  "tool": "introspect",
  "topic": "execution",
  "facet": "errors",
  "depth": "diagnostic",
  "horizon": "current_turn",
  "source_policy": "live_first",
  "include_context": false,
  "data_coverage": {},
  "summary": "Repeated shell timeouts are slowing repository inspection.",
  "view": {
    "topic": "execution",
    "facet": "errors",
    "depth": "diagnostic",
    "horizon": "current_turn",
    "data_coverage": {}
  },
  "observations": [],
  "evidence": [],
  "action_hints": [],
  "failure_clusters": [],
  "causal_chains": [],
  "adaptation_signals": [],
  "graph_slice": {},
  "budget_result": {}
}
```

Reserved future fields include `mode`, `generated_at`, and `current_state`.
They should only become part of the public contract when every producer can
fill them with explicit provenance and tests.

Existing `reflect` compatibility fields such as `session_id`, `analysis_view`,
`overview`, `diagnoses`, `insights`, `recommendations`, `reflection_context`,
and `prompt_preview` may continue to exist during migration. New consumers
should not depend on those fields for observation-plane behavior.

### Field Semantics

| Field | Meaning |
| --- | --- |
| `schema_version` | Observation-plane response schema version |
| `tool` | Producing facade, usually `introspect` or `reflect` |
| `topic`, `facet`, `depth`, `horizon`, `source_policy`, `include_context` | Normalized request scope |
| `data_coverage` | Root coverage summary; must match `view.data_coverage` |
| `view` | Normalized view descriptor repeated for consumers that already read nested view metadata |
| `summary` | Human/agent-readable compact result string |
| `graph_slice` | Bounded Observation Graph subset used to render the view |
| `observations` | Classified read-only findings |
| `failure_clusters` | Optional groups of related failures used for diagnosis and tuning |
| `causal_chains` | Decision/action/outcome chains |
| `evidence` | Materialized evidence refs and previews |
| `adaptation_signals` | Read-only inputs for the Tuning Control Plane and write-side feedback/adaptation tools |
| `action_hints` | Optional read-only hints, not commands |
| `budget_result` | Truncation and cursor metadata for bounded responses |

## Confidence Model

Confidence values must state what they measure. A single `confidence: 0.82`
is ambiguous across providers.

Use this shape where confidence is needed:

```json
{
  "confidence": {
    "classification": 0.82,
    "evidence": 0.74,
    "causal": 0.61
  }
}
```

| Field | Meaning |
| --- | --- |
| `classification` | Confidence that the observation category is correct |
| `evidence` | Confidence that the cited evidence is complete and reliable |
| `causal` | Confidence that the proposed causal relation is correct |

Providers may omit dimensions they cannot estimate. The Inspection Service may
aggregate provider-level confidence, but it must preserve provenance so scores
remain explainable.

## Progress and Current Work Model

The agent needs to answer "what am I doing?" as well as "what failed?".

This is a provider-phase target, not part of the current foundation contract.
Once implemented, `topic=execution, facet=progress` should expose:

```json
{
  "current_state": {
    "objective": "Analyze the current introspect and reflect design.",
    "phase": "analysis",
    "status": "on_track",
    "confidence": {
      "classification": 0.76,
      "evidence": 0.7
    },
    "last_meaningful_progress": {
      "ref": "urn:astra:event:edge:session_abc:seq_41",
      "summary": "Located the runtime introspect renderer and server reflect service."
    },
    "open_blockers": []
  }
}
```

Allowed `phase` values should be coarse and stable:

- `planning`
- `investigating`
- `editing`
- `running`
- `verifying`
- `waiting`
- `blocked`
- `summarizing`

Allowed `status` values:

- `on_track`
- `slow`
- `degraded`
- `stalled`
- `blocked`
- `regressing`
- `unknown`

## Runtime Errors as First-Class Observations

Runtime errors should be represented as observations with evidence and
causality.

Recommended error taxonomy:

- `tool_invalid_args`
- `tool_timeout`
- `tool_unavailable`
- `permission_denied`
- `workspace_binding`
- `missing_file`
- `test_failure`
- `provider_error`
- `rate_limit`
- `stream_error`
- `context_window`
- `context_stale`
- `loop_detected`
- `budget_exhausted`
- `memory_unavailable`
- `database_error`
- `policy_denied`
- `unknown`

Example:

```json
{
  "observations": [
    {
      "observation_id": "obs_err_01H00000000000000000000001",
      "kind": "tool_timeout",
      "severity": "warning",
      "horizon": "current_turn",
      "subject": {
        "type": "tool",
        "id": "bash"
      },
      "summary": "Three recent shell commands timed out before producing useful output.",
      "likely_cause": "The commands were too broad for the current repository size.",
      "retryability": "retry_with_changed_inputs",
      "confidence": {
        "classification": 0.9,
        "evidence": 0.82,
        "causal": 0.64
      },
      "evidence_refs": [
        "urn:astra:event:edge:session_abc:seq_40",
        "urn:astra:event:edge:session_abc:seq_41",
        "urn:astra:decision:cloud:decision_01H00000000000000000000001"
      ],
      "metrics": {
        "calls": 3,
        "failures": 3,
        "avg_duration_ms": 30000
      }
    }
  ]
}
```

`retryability` values:

- `retry_same`
- `retry_with_changed_inputs`
- `retry_after_wait`
- `switch_tool`
- `requires_user_input`
- `not_retryable`
- `unknown`

## Causal Chains

Causal chains connect user intent, agent decisions, tool actions, context state,
and outcomes.

The chain must not overclaim certainty. Use `causal_level` to distinguish
direct causality from weaker evidence:

- `causal`
- `likely_causal`
- `correlated`
- `unknown`

Example:

```json
{
  "causal_chains": [
    {
      "chain_id": "chain_01H00000000000000000000001",
      "summary": "A broad repository search timed out, then a similar search was retried, triggering loop-guard degradation.",
      "causal_level": "likely_causal",
      "confidence": {
        "evidence": 0.8,
        "causal": 0.69
      },
      "nodes": [
        {
          "ref": "urn:astra:event:edge:session_abc:seq_1",
          "kind": "user_intent",
          "summary": "User requested an implementation analysis."
        },
        {
          "ref": "urn:astra:decision:cloud:decision_01H00000000000000000000001",
          "kind": "decision",
          "summary": "Agent selected a broad shell search."
        },
        {
          "ref": "urn:astra:event:edge:session_abc:seq_40",
          "kind": "outcome",
          "summary": "Command timed out."
        },
        {
          "ref": "urn:astra:event:edge:session_abc:seq_42",
          "kind": "runtime_signal",
          "summary": "Loop guard detected repeated ineffective behavior."
        }
      ]
    }
  ]
}
```

`introspect` should return short recent chains. `reflect` may return full
chains or graph references, especially at `forensic` depth.

## Failure Clusters

Failure clusters group related observations before they become tuning
hypotheses. This prevents hypotheses from degenerating into unstructured lists
of failed cases.

Recommended chain:

```text
Observation
  -> FailureCluster
  -> Hypothesis
  -> Candidate
```

Example:

```json
{
  "failure_clusters": [
    {
      "cluster_ref": "urn:astra:failure_cluster:graph:fc_schema_linking_01H00000000000000000000001",
      "label": "schema_linking_ambiguity",
      "summary": "Several NL2SQL failures involve similarly named columns across related tables.",
      "observation_refs": [
        "urn:astra:observation:graph:obs_sql_017",
        "urn:astra:observation:graph:obs_sql_044"
      ],
      "evidence_class": "inferred_evidence",
      "confidence": {
        "classification": 0.81,
        "evidence": 0.76
      }
    }
  ]
}
```

Failure clusters may be produced by deterministic grouping, embedding-assisted
clustering, or LLM-assisted summarization. The response must preserve source
observation refs so downstream hypotheses remain auditable.

## Adaptation Signals

Adaptation signals are read-only consumer hints derived from observations.
They should not duplicate observation severity, confidence, or evidence. A
signal is an observation or failure-cluster reference plus consumer metadata.

They are not tuning actions, and they do not create or reconcile `TuningJob`
resources by themselves.

Example:

```json
{
  "adaptation_signals": [
    {
      "signal_id": "urn:astra:signal:graph:sig_tool_policy_01H00000000000000000000001",
      "observation_refs": [
        "urn:astra:observation:graph:obs_err_01H00000000000000000000001"
      ],
      "failure_cluster_refs": [
        "urn:astra:failure_cluster:graph:fc_tool_timeout_01H00000000000000000000001"
      ],
      "consumer": {
        "suggested_tool_family": "tuning_control_plane",
        "target_type": "tool_policy",
        "payload_kind": "tool_policy_signal",
        "priority": "medium",
        "scope_hint": "session"
      }
    }
  ]
}
```

Allowed `consumer.target_type` values:

- `user_guidance`
- `tool_parameters`
- `tool_policy`
- `skill_patch`
- `tool_upgrade`
- `prompt_nudge`
- `context_policy`
- `retry_policy`
- `model_routing`
- `memory_lesson`
- `workspace_binding`
- `permission_policy`
- `test_strategy`

Recommended loop:

```text
agent observes trouble
-> introspect(topic="execution", facet="errors", depth="diagnostic")
-> receives observations and adaptation_signals
-> agent or controller decides whether a tuning job or write-side tool is appropriate
-> Tuning Control Plane or feedback/memory/policy tool consumes selected signal refs
```

User-triggered loop:

```text
/reflect adaptation/signals forensic
-> returns durable signals and evidence graph
-> user or agent selects signals for a TuningJob or write-side tool
```

### Tuning Operator Consumption

When the Tuning Control Plane consumes an adaptation signal, it should preserve
the observation-plane contract:

1. Copy selected signal refs into `TuningJob.metadata.source_signal_refs`.
2. Dereference the signal's `observation_refs` and failure cluster refs before
   generating hypotheses.
3. Cite the source observations and failure clusters in every derived hypothesis, candidate,
   decision, evaluation, and measurement.
4. Emit reconcile events and status conditions back into the Observation Graph.
5. Keep apply, rollback, and promotion decisions as write-side records with
   stable evidence refs.

This keeps the loop inspectable:

```text
Observation
  -> FailureCluster
  -> AdaptationSignal
  -> TuningJob.spec
  -> Hypothesis
  -> ReconcileEvent / TuningCondition
  -> CandidateArtifact / EvaluationRun
  -> AppliedIntervention
  -> MeasurementReport
  -> MeasurementDecision
  -> optional future PromotionDecision
```

The write-side feedback and tuning loop is intentionally specified in a
separate design document:
[Feedback and Tuning Control Plane](feedback-tuning-loop.md).
This document only defines the observation-plane contract that produces
`adaptation_signals`.

## Action Hints

The observation tools may return `action_hints`, but these are not side effects.
They are local recommendations derived from evidence.

Example:

```json
{
  "action_hints": [
    {
      "hint_id": "hint_01H00000000000000000000001",
      "kind": "switch_strategy",
      "summary": "Use targeted file search before another broad shell command.",
      "evidence_refs": [
        "urn:astra:observation:graph:obs_err_01H00000000000000000000001"
      ],
      "confidence": {
        "classification": 0.74,
        "evidence": 0.7
      }
    }
  ]
}
```

Agents may ignore hints. Any durable adaptation must happen through a separate
write-side tool.

## Behavior by Runtime Mode

### CLI/Edge Mode

CLI/Edge may use local and cloud providers:

- Live local runtime snapshot
- Local journal
- Local workspace metadata
- LLM captures when enabled
- Cloud DB when authenticated
- Memory backend through local or cloud proxy

In `introspect(auto)`, local live data should dominate.

In `reflect(auto)`, durable cloud data should dominate when available, but local
artifacts should be merged as fresher deltas. If local data is ahead of cloud,
return `local_journal_ahead_of_cloud`.

### Pure Server Mode

Pure Server may use:

- Server runtime snapshot
- Server session state
- Cloud event store
- Decision audits
- Session activity
- Server-side trace store
- Server-accessible memory backend

It may not have local user machine artifacts. The schema remains the same, with
those providers marked `missing` or `unavailable`.

## Removed Legacy Parameters

The runtime contract does not read old `introspect` parameters such as
`subtopic` or `detail`, and does not read old `reflect` parameters such as
`focus`. CLI, Edge, and Server paths should share the normalized router:
`topic`, `facet`, `depth`, `horizon`, `source_policy`, `include_context`, and
bounded evidence limits.

Local self-surface should become a provider inside the shared reflect service,
not a separate response shape.

### Memory Reflection

Memory reflection is separate from session reflection.

Use:

- `reflect` for session observation and selected trace analysis.
- `memory(action="reflect")` or `memory.reflect` for memory synthesis.

The two may share evidence refs, but they should not share the same tool name or
response contract.

## Example Calls

### Live Runtime Check

```json
{
  "tool": "introspect",
  "arguments": {
    "topic": "runtime",
    "facet": "summary",
    "depth": "summary"
  }
}
```

### Live Error Diagnosis

```json
{
  "tool": "introspect",
  "arguments": {
    "topic": "execution",
    "facet": "errors",
    "depth": "diagnostic",
    "source_policy": "live_first"
  }
}
```

### User Session Failure Analysis

```text
/reflect execution/errors diagnostic
```

Normalized request:

```json
{
  "tool": "reflect",
  "arguments": {
    "horizon": "session",
    "topic": "execution",
    "facet": "errors",
    "depth": "diagnostic"
  }
}
```

### Trace Forensics

```text
/reflect execution/trace forensic
```

### Adaptation Data for Tuning or Feedback

```json
{
  "tool": "reflect",
  "arguments": {
    "horizon": "session",
    "topic": "adaptation",
    "facet": "signals",
    "depth": "diagnostic"
  }
}
```

## Implementation Status and Plan

Implemented foundation:

1. Shared DTOs in `astra_core::observation`:
   - `ObservationView`
   - `ObservationDataCoverage`
   - `ObservationProviderCoverage`
   - `ObservationBudgetResult`
   - `ObservationRecord`
   - `ObservationEvidence`
   - `ObservationConfidence`
   - `ObservationFailureCluster`
   - `ObservationCausalChain`
   - `ObservationAdaptationSignal`
   - `ObservationActionHint`
   - `ObservationGraphSlice`
   - `EvidenceRef`
2. `introspect` normalized routing:
   - `topic`
   - `facet`
   - `depth`
   - `horizon`
   - `source_policy`
   - `include_context`
   - JSON observation envelope
3. `reflect` normalized routing:
   - `topic`
   - `facet`
   - `depth`
   - `horizon`
   - `source_policy`
   - `include_context`
   - server database coverage warnings
   - shared graph-slice projection
4. First-class error and provider-unavailable observations for the current
   runtime and server database surfaces.
5. Read-only adaptation signals that reference observations and failure
   clusters instead of duplicating severity, confidence, or evidence.
6. Removal of obsolete public contracts:
   - `introspect.subtopic`
   - `introspect.detail`
   - legacy `reflect.focus` routing
   - public `reflect.evidence_graph`
   - implicit tuning control-loop runtime code

Next phases:

1. Define provider traits:
   - `LiveRuntimeProvider`
   - `ContextProvider`
   - `LocalSessionProvider`
   - `CloudEventProvider`
   - `DecisionAuditProvider`
   - `TraceProvider`
   - `MemoryObservationProvider`
   - `TuningControlPlaneProvider`
2. Build an `InspectionService` that normalizes topic, facet, depth, horizon,
   selector, budget, and source policy before calling providers.
3. Move CLI local reflect self-surface behind `LocalSessionProvider`.
4. Add persistent Observation Graph storage with logical runtime, observation,
   and adaptation layers.
5. Ingest tuning job status, reconcile events, evaluations, and measurements as
   Observation Graph evidence once the write-side tuning control plane exists.
6. Update SDK bindings and richer slash-command renderers to consume the shared
   envelope directly.
7. Keep `last_n` only as a bounded evidence limit, not as a horizon alias.

## Open Questions

1. Should large `forensic` graph slices be returned only by cursor pagination,
   or should the server also materialize downloadable artifacts?
2. What alias-retention policy is required after local evidence refs are synced
   to cloud IDs?
3. Should visible-context excerpts be returned directly, summarized, or only
   referenced?
4. Should adaptation signals be persisted only when consumed by a tuning job or
   write-side feedback tool?
5. How should `cross_session` enforce user, workspace, team, and privacy
   boundaries?
6. Should action hints be generated by deterministic rules only, or may they
   include LLM-assisted summaries when evidence is sufficient?
7. Which `TuningJob` status conditions, reconcile events, and measurement
   reports should be retained as long-lived graph evidence versus short-lived
   operator status?
