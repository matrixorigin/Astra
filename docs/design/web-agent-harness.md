# Web Agent Harness

> Status: Proposed design contract.
> Scope: User-customizable, auditable Web agent workflows that combine data
> sources, files, sessions, agent runs, human review, and traceable outputs.
> Audience: Web UI, runtime, workflow, persistence, skill, and observability
> maintainers.

This document defines the product and runtime design for a Web **harness**
section: a place where users can build reusable agent-assisted workflows.

A harness is not a special chat and not a separate execution engine. It is a
versioned workflow product surface over existing Astra primitives:

- durable agent runs,
- workflow/run state,
- session artifacts,
- `agent_events` trace,
- project/session/user data sources,
- human input gates,
- skills and tools.

The goal is to let users customize repeatable workflows without turning them
into opaque natural-language automations.

## Product Intent

Users should be able to define workflows such as:

- Fill a bid-response Excel workbook from company technical documentation and
  bid history.
- Convert selected long chat sessions into reviewed, reusable personal skills.
- Review contracts against a company playbook.
- Extract findings from research sessions and publish a team report.

The workflow must be reusable, tunable, auditable, and safe to run again.

For example, in a bid-response harness:

1. The user selects a company knowledge source and uploads an Excel workbook.
2. The harness parses the workbook and extracts answerable requirement items.
3. Agents retrieve evidence from the selected data sources.
4. Agents propose answers with citations.
5. Items without sufficient evidence or confidence become pending review items.
6. A user approves, rejects, or edits pending items.
7. The harness writes approved answers back to Excel and emits an audit report.

Every filled answer must be tied to source evidence. If the system cannot find
evidence, the item remains blocked or pending. It must not silently invent an
answer.

## Goals

- Support user-defined harness workflows.
- Make customization typed, validated, reusable, and versioned.
- Keep workflow execution auditable from database state and `agent_events`.
- Reuse shared node implementations for parsing, retrieval, review, evidence
  validation, artifact generation, and skill drafting.
- Support human-in-the-loop review as a first-class workflow state.
- Treat evidence and citations as hard correctness contracts.
- Let runs resume or be inspected after process restart.
- Allow future data-source channels without changing each harness.
- Make the Web UI useful before a full visual builder exists.

## Non-Goals

- Do not create a second agent execution engine for harnesses.
- Do not make Next.js orchestrate multi-step agent execution client-side.
- Do not store harness state only in local files or process memory.
- Do not rely on local JSONL journals for Web product behavior.
- Do not encode all harness state as opaque `wf_runs.step_results` JSON.
- Do not allow arbitrary user code inside a harness definition in the first
  product slice.
- Do not let a node silently fall back to weaker behavior when a source,
  parser, model, or validator is unavailable.
- Do not duplicate retrieval, citation validation, Excel writing, or skill
  drafting logic per harness template.

## Core Principle

Harness customization is **typed customization**, not free-form automation.

Users can customize:

- which sources are bound,
- which input artifacts are required,
- which node graph runs,
- which evidence rules apply,
- which review gates are required,
- which outputs are generated,
- who can approve or publish,
- which version is active.

Users cannot rely on an unstructured prompt that lets the agent decide hidden
steps at runtime. The agent can plan inside specific nodes, but the harness
graph, node contracts, review gates, and evidence policy remain explicit.

## Existing Foundations

The design intentionally builds on existing Astra surfaces:

- [Web Session Traceability](web-session-traceability.md): DB-backed trace is
  the Web source of truth. Harness trace rendering must use runtime APIs backed
  by `agent_events`.
- [Durable Agent Runs](durable-agent-runs.md): harness execution should be
  resumable at step boundaries and should use durable run state.
- [Agents and Orchestration](agents-and-orchestration.md): harness agent work is
  ordinary agent execution, including multi-agent delegation.
- [Web Agent Dynamic Multi-Agent Support](web-agent-dynamic-multi-agent.md):
  harness should observe parent and child runs as first-class lineage.
- `astra-plan::ActionPlan`: typed actions and postconditions already encode the
  direction needed for verifiable workflow steps.
- `astra_services::workflows`: `wf_definitions` and `wf_runs` provide a thin
  workflow substrate, but are not rich enough to be the harness product model
  by themselves.

## Layer Ownership

Harness capability belongs in the server runtime layer. Web owns presentation
and interaction only.

Runtime/server owns:

- `HarnessService`,
- harness definition/version/run persistence,
- node catalog registration and validation,
- graph validation and activation,
- source snapshotting,
- evidence and citation policy,
- human review state transitions,
- agent run dispatch,
- subagent spawn, fan-out/fan-in, and child-run lineage,
- artifact generation,
- audit event emission,
- resumability and cancellation.

Web owns:

- harness library pages,
- guided template configuration,
- graph/flow preview,
- run console rendering,
- review queue rendering,
- user interaction for approve/reject/edit,
- API proxying and client-side refresh.

Explicit boundary:

- Web must not be the workflow orchestrator.
- Web must not store canonical harness run state.
- Web must not infer item status from local UI state.
- Runtime APIs must be sufficient for CLI, SDK, or future API clients to create
  and run harnesses without the Web UI.

## User Customization Levels

### 1. Template Customization

This is the first product surface.

Users start from an installed template:

- Bid Excel filler.
- Skillify from sessions.
- Contract review.
- Research synthesis.

The UI exposes safe parameters:

- source bindings,
- input file requirements,
- output format,
- evidence policy,
- review policy,
- confidence thresholds,
- role permissions,
- model or skill constraints.

The saved result is still a `HarnessVersion`.

### 2. Graph Customization

Advanced users can edit a typed workflow graph made of registered nodes.

Example:

```text
source.bind
  -> file.parse_excel
  -> agent.extract_requirements
  -> retrieval.search_evidence
  -> agent.propose_answer
  -> validate.citations
  -> human.review
  -> artifact.write_excel
  -> artifact.audit_report
```

The same graph can use subagents where parallel expert roles materially reduce
manual work:

```text
source.snapshot
  -> file.parse_excel
  -> agent.extract_requirements
  -> agent.fanout(role = technical_spec_reviewer, foreach = requirement_items)
  -> agent.fanout(role = commercial_terms_reviewer, foreach = requirement_items)
  -> agent.fanout(role = evidence_auditor, foreach = requirement_items)
  -> agent.reduce(role_outputs = blackboard.entries)
  -> validate.citations
  -> human.review(disputed_or_high_risk_items)
  -> artifact.write_excel
  -> artifact.audit_report
```

Subagents are useful when the work naturally splits by professional lens or by
batch item. They are not required for every harness and should not be used to
hide an untyped workflow inside agent-to-agent chat.

Each node has:

- stable node type,
- input schema,
- output schema,
- idempotency policy,
- timeout and cost policy,
- permission requirements,
- emitted artifact kinds,
- emitted event types,
- retry policy, if any.

### 3. Registered Extension Nodes

Later, teams can add custom nodes such as:

- `sap.lookup_equipment`,
- `crm.fetch_customer_profile`,
- `excel.company_mapping`,
- `legal.classify_clause`.

These are registered platform capabilities, not inline snippets embedded in
one harness. Registration requires schema, permissions, implementation owner,
versioning, and audit metadata.

## Domain Model

### HarnessDefinition

The stable product identity.

Fields:

- `harness_id`
- `owner_user_id`
- `team_id`
- `name`
- `description`
- `visibility`: `private | team | public`
- `active_version_id`
- `status`: `draft | active | archived`
- `created_at`
- `updated_at`

### HarnessVersion

An immutable executable definition.

Fields:

- `version_id`
- `harness_id`
- `version`
- `definition_json`
- `input_schema_json`
- `output_schema_json`
- `source_policy_json`
- `evidence_policy_json`
- `review_policy_json`
- `runtime_policy_json`
- `created_by`
- `created_at`
- `status`: `draft | validated | active | retired`

Rules:

- A run always binds one `version_id`.
- Active versions are immutable.
- Editing an active harness creates a new version.
- Validation happens before activation.

### HarnessRun

One execution instance.

Fields:

- `harness_run_id`
- `harness_id`
- `version_id`
- `user_id`
- `session_id`
- `workflow_run_id`
- `agent_run_id`
- `parent_agent_run_id`
- `status`
- `input_json`
- `current_node_id`
- `started_at`
- `completed_at`
- `error`

`agent_run_id` is the parent run that owns the harness execution. Subagent work
created by harness nodes must be represented as child runs of this parent, not
as detached background jobs.

Status:

- `pending`
- `validating_inputs`
- `running`
- `waiting_for_review`
- `waiting_for_external_input`
- `completed`
- `failed`
- `cancelled`

### HarnessSource

A source snapshot bound to a run or reusable source collection.

Fields:

- `source_id`
- `harness_run_id`
- `source_type`: `upload | project_files | sessions | memory | web_session | connector_record | ruleset`
- `source_ref`
- `snapshot_ref`
- `content_hash`
- `metadata_json`
- `status`: `pending | indexing | ready | failed | revoked`
- `created_at`

Important rule: harness runs use source snapshots. If a project file or session
changes later, old runs remain auditable against the source snapshot they used.

### HarnessItem

The atomic unit of work inside a run.

For the bid Excel case, one item can be one requirement row or one answer cell.

Fields:

- `item_id`
- `harness_run_id`
- `parent_item_id`
- `item_type`
- `locator_json`
- `input_json`
- `proposed_output_json`
- `final_output_json`
- `status`
- `confidence`
- `assigned_to`
- `created_at`
- `updated_at`

Status:

- `extracted`
- `searching_evidence`
- `answer_proposed`
- `pending_evidence`
- `pending_review`
- `approved`
- `rejected`
- `needs_revision`
- `finalized`
- `blocked`

### HarnessCitation

A structured evidence link. Citations are first-class records, not text pasted
into an answer.

Fields:

- `citation_id`
- `harness_run_id`
- `item_id`
- `source_id`
- `source_locator_json`
- `artifact_id`
- `quote_hash`
- `evidence_text_preview`
- `relevance_score`
- `created_by_node_id`
- `created_at`

The citation locator must be source-specific:

- uploaded file: page, sheet, row, cell, byte span, or text span,
- session: session id, transcript item seq, event id, message span,
- artifact: artifact id and JSON pointer,
- connector: connector object id and provider locator.

### HarnessDecision

Human decisions and edits.

Fields:

- `decision_id`
- `harness_run_id`
- `item_id`
- `reviewer_user_id`
- `decision`: `approve | reject | edit | request_revision`
- `before_json`
- `after_json`
- `reason`
- `created_at`
- `idempotency_key`

Every decision also emits an `agent_events` row so audit views can reconstruct
what happened from the trace plane.

### HarnessArtifact

Output artifacts generated by the harness.

Fields:

- `harness_artifact_id`
- `harness_run_id`
- `artifact_id`
- `artifact_kind`: `excel_output | audit_report | skill_draft | json_export`
- `status`
- `created_by_node_id`
- `created_at`

The actual content should live in the existing session artifact or object-store
path rather than duplicating large blobs in harness tables.

### HarnessExternalAction

A durable record for actions against external systems.

Many realistic harnesses need to read enterprise systems, write draft fields,
create review tickets, or upload artifacts. Those actions must be explicit,
permissioned, and auditable.

Fields:

- `external_action_id`
- `harness_run_id`
- `node_id`
- `item_id`
- `system_type`: `erp | crm | clm | qms | mes | cms | marketing | insurance | hospital | custom`
- `system_ref`
- `action_kind`: `read | download | draft_write | create_task | upload_draft | commit_write | submit`
- `status`: `planned | running | completed | blocked | failed | cancelled`
- `requires_decision_id`
- `idempotency_key`
- `before_ref`
- `after_ref`
- `evidence_ref`
- `created_at`
- `completed_at`

Rules:

- `read`, `download`, `draft_write`, `create_task`, and `upload_draft` may be
  allowed by a harness policy.
- `commit_write` requires explicit human approval.
- `submit` is disallowed by default and must be modeled as a final human action
  outside autonomous agent execution unless a future policy explicitly enables
  it for a narrow internal system.
- Every external action emits an `agent_events` row with the page/object
  locator, captured fields, and before/after references when applicable.

### HarnessRuleSetBinding

A versioned rule source used by validation and classification nodes.

Fields:

- `ruleset_binding_id`
- `harness_run_id`
- `ruleset_type`: `policy | legal | finance_control | medical | quality | marketing | custom`
- `source_id`
- `ruleset_name`
- `ruleset_version`
- `content_hash`
- `effective_at`
- `metadata_json`

Rules:

- Rule nodes must bind a concrete rule version before execution.
- A harness run cannot silently move to a newer rule version after it starts.
- If a required rule version cannot be resolved, the run blocks rather than
  continuing with a nearby version.

### HarnessAgentRole

A declared subagent role that can be used by agent nodes in a harness version.
This is a workflow role, not a separate permission principal.

Fields:

- `agent_role_id`
- `version_id`
- `role_name`
- `purpose`
- `input_schema_json`
- `output_schema_json`
- `tool_scope_json`
- `source_scope_json`
- `assertion_policy_json`
- `max_parallelism`
- `timeout_ms`
- `model_policy_json`

Rules:

- A subagent inherits the parent agent's authenticated user/session capability.
- A role can narrow allowed tools, sources, and outputs, but cannot broaden the
  parent agent's permissions.
- Role output must be schema-validated before it can update harness items,
  citations, artifacts, or blackboard entries.
- Roles that can produce business recommendations must declare allowed
  assertion kinds. High-risk final `decision` assertions remain human-owned
  unless a versioned rule node is explicitly authorized by policy.

### HarnessSubagentRun

A durable projection that links harness nodes and items to child agent runs.

Fields:

- `subagent_run_id`
- `harness_run_id`
- `node_id`
- `item_id`
- `agent_role_id`
- `parent_agent_run_id`
- `child_agent_run_id`
- `status`: `planned | running | completed | failed | cancelled`
- `input_ref`
- `output_ref`
- `failure_reason`
- `started_at`
- `completed_at`

Rules:

- The parent run remains the orchestration owner.
- Child runs may execute in parallel when the graph and role `max_parallelism`
  allow it.
- Child outputs cannot overwrite human decisions or another role's outputs.
  They write proposed facts, citations, risks, questions, or draft artifacts.
- Every child run must be visible in run trace through `agent_events`
  `parent_run_id` and `causal_chain_id`.

### HarnessBlackboardEntry

A structured shared state entry used by multiple subagents in one harness run.
This replaces passing long natural-language summaries between agents.

Fields:

- `blackboard_entry_id`
- `harness_run_id`
- `item_id`
- `created_by_node_id`
- `created_by_agent_role_id`
- `entry_kind`: `fact | hypothesis | risk | question | citation | draft | objection | decision_ref`
- `payload_json`
- `citation_ids`
- `confidence`
- `status`: `proposed | validated | disputed | superseded | rejected`
- `created_at`

Rules:

- Blackboard entries are append-only except for status transitions.
- Conflicting entries are not resolved by last writer wins.
- Reducer, validator, critic, or human review nodes decide whether entries are
  validated, disputed, superseded, or rejected.
- Final artifacts should cite blackboard entries and their supporting
  citations rather than opaque subagent prose.

## Workflow Definition

Harness workflow definitions are typed DAGs.

Minimal shape:

```json
{
  "schema_version": "harness.workflow.v1",
  "name": "bid_excel_filler",
  "inputs": {
    "datasource": { "type": "source_collection", "required": true },
    "workbook": { "type": "file", "mime_types": ["application/vnd.openxmlformats-officedocument.spreadsheetml.sheet"], "required": true }
  },
  "nodes": [
    {
      "id": "parse_workbook",
      "type": "file.parse_excel",
      "inputs": { "file": "$inputs.workbook" },
      "outputs": { "tables": "excel_tables" }
    },
    {
      "id": "extract_requirements",
      "type": "agent.extract_requirements",
      "inputs": { "tables": "$nodes.parse_workbook.outputs.tables" },
      "outputs": { "items": "requirement_items" }
    },
    {
      "id": "search_evidence",
      "type": "retrieval.search_evidence",
      "foreach": "$nodes.extract_requirements.outputs.items",
      "inputs": {
        "query": "$item.input.requirement_text",
        "sources": "$inputs.datasource"
      },
      "outputs": { "evidence": "candidate_evidence" }
    },
    {
      "id": "propose_answer",
      "type": "agent.answer_with_citations",
      "foreach": "$nodes.extract_requirements.outputs.items",
      "inputs": {
        "requirement": "$item.input",
        "evidence": "$nodes.search_evidence.outputs.evidence"
      },
      "outputs": { "answer": "proposed_answer", "citations": "citations" }
    },
    {
      "id": "validate_citations",
      "type": "validate.citations",
      "foreach": "$nodes.extract_requirements.outputs.items",
      "policy": "$policies.evidence"
    },
    {
      "id": "review_uncertain",
      "type": "human.review",
      "foreach": "$items.where(status in ['pending_review', 'pending_evidence'])",
      "policy": "$policies.review"
    },
    {
      "id": "write_excel",
      "type": "artifact.write_excel",
      "inputs": {
        "file": "$inputs.workbook",
        "items": "$items.where(status == 'approved')"
      }
    },
    {
      "id": "audit_report",
      "type": "artifact.audit_report",
      "inputs": { "harness_run": "$run" }
    }
  ],
  "policies": {
    "evidence": {
      "min_citations": 1,
      "require_source_snapshot": true,
      "on_missing_evidence": "block_item"
    },
    "review": {
      "require_human_for_confidence_below": 0.85,
      "require_human_for_fields": ["price", "performance_spec", "compliance_claim"]
    }
  }
}
```

This JSON is not a scripting language. It references registered node types and
typed values produced by previous nodes.

## Graph Constraints

Harness graph validation runs before a version can be activated.

V1 graph shape:

- The graph is a typed DAG.
- Cycles are invalid.
- Every node has a unique `id`.
- Every node `type` must exist in the node catalog.
- Every edge connects a producer output to a consumer input.
- A consumer input cannot read from a downstream node.
- All required harness inputs must be bound by the run request or source
  binding UI.
- All required node inputs must resolve from harness inputs, constants,
  policies, or upstream node outputs.
- A graph must have at least one terminal output node or explicitly declare that
  it is review-only.

Node constraints:

- Node inputs are validated against the node input schema.
- Node outputs are validated against the node output schema.
- Node versions are pinned by the immutable `HarnessVersion`.
- Agent nodes must declare output schemas; free-form assistant text is not a
  valid harness item output by itself.
- Agent nodes that spawn subagents must reference declared `HarnessAgentRole`
  records and declare fan-out, fan-in, and conflict handling.
- Human review is represented by explicit `human.*` nodes, not hidden prompt
  instructions.
- Validation nodes cannot be skipped for outputs covered by evidence policy.

Edge constraints:

- Schema compatibility is checked at activation time.
- Collection edges must declare whether they pass a whole collection or one
  item through `foreach`.
- `foreach` can only run over bounded collections produced by earlier nodes or
  harness inputs.
- Fan-out outputs must define how item-level failures aggregate back to the run:
  `block_run`, `block_item`, or `review_item`.
- Multi-subagent fan-in must use an explicit reducer or validator node. A
  downstream artifact node cannot read unordered child-run prose directly.

Subagent constraints:

- Subagents inherit the parent agent's authenticated permissions and session
  context; the harness role may only narrow access.
- Subagent roles must declare allowed source scopes, tool scopes, output schema,
  and assertion kinds.
- Subagent writes go through harness item, citation, artifact, external action,
  or blackboard APIs. Direct child-run side effects are invalid unless declared
  as node side effects.
- Subagent disagreement is a workflow state. Conflicting facts, citations,
  recommendations, or rule versions must become `disputed` or
  `pending_review`; they cannot be resolved by prompt wording.
- A child run timeout or schema failure follows the node's failure aggregation
  policy. It cannot silently reuse another role's output.
- Human decisions supersede subagent outputs for the same item and revision.

Side-effect constraints:

- Nodes that write files, publish skills, mutate memory, or call external
  systems must declare `side_effects`.
- Side-effect nodes must declare an idempotency strategy.
- Side-effect nodes cannot run before their validation dependencies have
  completed.
- A rerun from an edited item can only replay the affected downstream subgraph
  unless the user explicitly starts a full rerun.

Evidence constraints:

- Any output field marked `requires_citation` must be produced before
  `validate.citations`.
- If `allow_model_knowledge` is false, model-only claims are invalid final
  outputs.
- A human override of weak evidence must be allowed by policy and recorded as a
  `HarnessDecision`.

External action constraints:

- Nodes that touch external systems must use `HarnessExternalAction`.
- Draft writes must capture before/after state or enough field-level evidence to
  audit the write.
- Final submit, payment release, claim approval, platform final submission,
  production publish, and official appeal submission are not ordinary node
  writes. They require explicit human action gates.
- If an external page/object version changes after approval, the dependent
  review decision is stale and the affected items return to review.

## Node Catalog

The platform owns a central node catalog. Harness templates compose nodes from
this catalog.

Initial node families:

- `source.*`: bind, snapshot, and validate data sources.
- `connector.*`: read and draft-write enterprise systems through permissioned
  sessions or API connectors.
- `file.*`: parse uploaded files and normalize extracted content.
- `ocr.*`: extract text from screenshots, scanned PDFs, and design assets.
- `retrieval.*`: search and rank source evidence.
- `rule.*`: apply versioned policy, legal, finance, medical, quality, or
  marketing rule sets.
- `agent.*`: run parent-agent or subagent tasks with explicit input/output
  contracts.
- `validate.*`: enforce schema, evidence, and safety policies.
- `human.*`: create review items and consume decisions.
- `artifact.*`: generate downloadable outputs.
- `skill.*`: draft, validate, and publish skills.

Initial nodes:

- `source.snapshot`
- `connector.read_record`
- `connector.draft_write`
- `connector.create_review_task`
- `file.parse_excel`
- `file.extract_text`
- `ocr.extract_visible_text`
- `rule.apply_ruleset`
- `agent.extract_requirements`
- `agent.fanout`
- `agent.reduce`
- `agent.critic`
- `retrieval.search_evidence`
- `agent.answer_with_citations`
- `validate.citations`
- `validate.output_schema`
- `human.review`
- `artifact.write_excel`
- `artifact.audit_report`
- `skill.draft_from_sessions`
- `skill.validate_draft`

Each node definition must specify:

- `node_type`
- `version`
- `input_schema`
- `output_schema`
- `side_effects`
- `idempotency`
- `timeout_ms`
- `permission_requirements`
- `event_contract`
- `artifact_contract`

## Subagent Execution Model

Harness should use the existing subagent capability instead of inventing a
parallel agent framework. A harness run has one parent agent run. Agent nodes
may spawn child subagent runs for bounded work when the node definition and
`HarnessAgentRole` allow it.

Subagent execution modes:

- `single`: run one agent task in the parent run or one child subagent.
- `foreach_subagent`: spawn child runs across bounded items, such as payment
  rows, claim cases, bid requirements, or contract clauses.
- `role_fanout`: spawn multiple role-specific child runs for the same item.
- `critic`: run a child subagent whose only output is objections, missing
  evidence, stale-source flags, or schema violations.
- `reducer`: merge validated child outputs into proposed item output, disputed
  entries, review questions, or draft artifacts.

Subagent roles should be used for real separation of concerns:

- extraction roles collect facts and citations,
- domain roles classify risks or apply professional lenses,
- drafting roles generate controlled prose or artifact sections,
- critic roles challenge evidence, permissions, stale versions, and overclaims,
- reducer roles reconcile structured outputs but do not erase disagreements.

Permission model:

- The parent agent owns the authenticated capability.
- Child subagents inherit that capability through existing run/session
  mechanics.
- Harness role policies can narrow tools, source collections, connector
  objects, and external actions for a child run.
- Harness role policies cannot grant access that the parent run or user lacks.
- External side effects from child runs still require `HarnessExternalAction`
  and the same human gates as parent-run side effects.

State-sharing model:

- Child runs receive scoped inputs and source snapshots.
- Child runs write structured outputs and blackboard entries.
- They do not pass unbounded chat context to other child runs.
- Reducer and critic nodes consume blackboard entries, citations, and artifacts
  through typed APIs.

When to use subagents:

- The workflow has independent items that can be processed in parallel.
- The workflow needs multiple professional perspectives on the same item.
- A critic role can catch likely overclaims, stale sources, or weak evidence.
- External-system reads can be parallelized without violating rate limits or
  permission constraints.

When not to use subagents:

- The node is a deterministic parser, rule evaluator, citation validator, or
  file writer.
- The task is small enough that child-run orchestration costs more than it
  saves.
- The goal is only to make a vague prompt look structured.
- The child agents would need broader permissions than the parent.

Example bid-harness role fan-out:

```text
requirement_item
  -> technical_spec_reviewer: match product specs and cite datasheets
  -> commercial_terms_reviewer: detect price, delivery, warranty, and SLA risk
  -> legal_clause_reviewer: detect contract redlines and required approvals
  -> evidence_auditor: verify every proposed answer has valid citations
  -> reducer: produce proposed response or disputed review packet
```

Example payment-harness role fan-out:

```text
payment_item
  -> invoice_reviewer: verify invoice authenticity and amounts
  -> po_contract_reviewer: verify PO, contract, and receiving evidence
  -> bank_account_reviewer: verify vendor account and recent changes
  -> control_critic: apply payment rules and flag missing evidence
  -> reducer: produce pass, block, or human-review recommendation
```

## Evidence Contract

Evidence is mandatory where the harness version says it is mandatory.

For a citation-bound item:

- A final output is invalid without enough citations.
- A citation must point to a source snapshot or durable artifact.
- The answer must identify which citation supports which claim when the output
  has multiple claims.
- If the model cannot find evidence, the item becomes `pending_evidence` or
  `blocked`.
- Human approval can accept an item with weak evidence only if policy allows
  that explicitly and records the override as a decision.

No node may replace a missing citation with general model knowledge unless the
harness evidence policy explicitly allows `allow_model_knowledge: true`.

The default for business workflows is `allow_model_knowledge: false`.

## Human Review

Human review is a workflow node, not an afterthought.

The review UI should show:

- item input,
- proposed answer,
- citations and source previews,
- confidence and validator failures,
- prior decisions,
- edit controls,
- approve/reject/request-revision actions.

Decision rules:

- Approval records the current proposed output as final output.
- Rejection clears the final output and stores a reason.
- Edit records before/after payloads and may mark the item approved if policy
  allows editor approval.
- Request revision creates a new node input patch and reruns only affected
  downstream nodes.

Every decision requires an idempotency key.

## Runtime Integration

Harness execution should be owned by a new `HarnessService`.

Responsibilities:

- validate harness definitions,
- create immutable versions,
- create runs,
- bind and snapshot sources,
- dispatch workflow nodes,
- spawn subagents for agent nodes that declare subagent execution,
- enforce subagent role source/tool narrowing,
- collect child-run outputs into blackboard entries,
- run explicit reducers and critics for multi-subagent fan-in,
- create and update harness items,
- enforce evidence and review policy,
- map node execution to agent runs and workflow runs,
- persist projections for Web UI,
- emit trace events.

The existing `WorkflowService` can remain as a low-level substrate, but harness
product state should not be squeezed into only `wf_runs.step_results`.

V1 decision: `HarnessService` owns the product API and persistence model.
`WorkflowService` may be used internally as a dispatcher/run substrate, but it
is not the user-facing harness model and should not gain harness-specific
business logic.

Recommended layering:

```text
Web UI
  -> Next.js API proxy
  -> Runtime HarnessService
  -> Workflow dispatcher
  -> RunLifecycleService / Agent runs
  -> Node implementations
  -> agent_events + harness tables + session_artifacts
```

`agent_events` remains the trace plane. Harness tables are the product state
projection. Session artifacts store generated files and durable JSON artifacts.

## Event Contract

Harness execution emits structured events into `agent_events`.

Initial event types:

- `harness_run_started`
- `harness_run_completed`
- `harness_run_failed`
- `harness_node_started`
- `harness_node_completed`
- `harness_node_failed`
- `harness_subagent_spawned`
- `harness_subagent_completed`
- `harness_subagent_failed`
- `harness_blackboard_entry_created`
- `harness_blackboard_entry_status_changed`
- `harness_source_snapshot_created`
- `harness_item_created`
- `harness_item_updated`
- `harness_citation_created`
- `harness_review_requested`
- `harness_review_decision`
- `harness_artifact_created`

Common metadata:

```json
{
  "harness": {
    "harness_id": "...",
    "version_id": "...",
    "harness_run_id": "...",
    "node_id": "...",
    "item_id": "...",
    "source_id": "..."
  }
}
```

Events tied to agent execution must include run lineage:

- `session_id`,
- `run_id`,
- `parent_run_id`,
- `child_run_id` when the event is emitted for a subagent,
- `agent_id`,
- `agent_role_id`,
- `causal_chain_id`,
- `tool_call_id` when available.

## Data Sources

Data-source handling must be shared across harnesses.

Initial source types:

- `upload`: user-uploaded files.
- `project_files`: files already attached to a Web project.
- `sessions`: selected chat sessions and their transcript/event snapshots.
- `memory`: approved user/team memory.
- `web_session`: permissioned browser sessions for enterprise systems.
- `connector_record`: durable records read through API connectors.
- `ruleset`: versioned policy, control, legal, medical, quality, or finance
  rules used by `rule.*` nodes.

Future source types:

- GitHub repositories,
- object stores,
- CRM,
- ticket systems,
- internal document stores,
- databases.

Source rules:

- A harness run must snapshot source identities and content hashes.
- Source access is checked when binding and when reading.
- Enterprise system sources must snapshot object IDs, page URLs or API paths,
  captured fields, connector/session identity, and capture time.
- If source access is revoked during a run, the affected nodes fail or block
  explicitly.
- Uploaded file bytes must be stored durably. Metadata-only file records are not
  enough for harness execution.
- Source policy must record data classification, retention, redaction, and
  export restrictions for sensitive data.

## Web UI

The Web harness section should have four main views.

### Harness Library

Shows installed and user-created harnesses:

- name,
- description,
- active version,
- last run,
- owner,
- status.

Actions:

- create from template,
- duplicate,
- archive,
- open builder,
- run.

### Harness Builder

First slice: template parameter editor.

Later slice: graph editor backed by the node catalog.

Builder must show:

- required inputs,
- source bindings,
- evidence policy,
- review policy,
- output policy,
- validation errors before activation.

### Run Console

Shows one harness run:

- current status,
- active node,
- item counts by status,
- agent progress,
- generated artifacts,
- trace link.

### Review Queue

Shows pending items across one run or many runs:

- item summary,
- proposed answer,
- citations,
- validator failures,
- decision controls.

## Customization UX

V1 does not require a drag-and-drop builder. The primary customization surface is
guided configuration over a typed template, with a readable flow preview.

The design target is: ordinary users configure harnesses by filling forms and
choosing sources; advanced users can open structured graph settings later.

### Bid Excel Guided Setup

The template page presents:

```text
Bid Excel Filler

Inputs
  Company DS
  Excel workbook

Flow
  Parse Excel
  -> Extract requirements
  -> Search DS
  -> Propose answers
  -> Validate citations
  -> Review uncertain items
  -> Export Excel + audit report
```

The user configures:

- data sources: project files, uploaded files, prior sessions, or future
  connectors,
- Excel workbook,
- sheet and column mapping,
- answer-cell mapping,
- evidence rule: minimum citations, model-knowledge policy, citation source
  types,
- review rule: confidence threshold and fields that always need human review,
- output rule: write-back workbook, audit report, JSON export.

The advanced panel can expose structured settings without exposing raw graph
editing first:

```yaml
evidence:
  min_citations: 1
  allow_model_knowledge: false

review:
  require_human_for_confidence_below: 0.85
  require_human_for_fields:
    - performance_spec
    - compliance_claim

output:
  workbook: true
  audit_report: true
```

User effort should be:

1. Select DS.
2. Upload workbook.
3. Confirm sheet/column mapping.
4. Choose evidence and review strictness.
5. Run.
6. Resolve review queue.

### Skillify Guided Setup

The template page presents:

```text
Skillify From Sessions

Inputs
  Selected sessions
  Skill scope

Flow
  Snapshot sessions
  -> Extract candidate rules
  -> Merge duplicates
  -> Review candidates
  -> Generate draft skill
  -> Validate skill
  -> Publish decision
```

The user configures:

- sessions to learn from,
- extraction target: writing preferences, workflow steps, tool habits, project
  conventions, or all,
- target scope: personal or project,
- review mode: review every candidate or review merged candidate groups,
- publish mode: create draft only or ask for publish decision after validation.

User effort should be:

1. Select chats.
2. Choose extraction target and scope.
3. Review candidate rules with citations.
4. Edit or reject noisy candidates.
5. Generate and validate a draft skill.
6. Approve activation.

The graph and node catalog make this repeatable, but the UI should feel like a
guided setup plus review queue, not a workflow programming environment.

## Scenario Walkthrough Corrections

The ten concrete business scenarios in the appendix force several corrections
to the base design. These are not optional polish; without them harness becomes
an attractive demo surface rather than a reliable workflow system.

### 1. Enterprise Systems Are First-Class Sources

Real harnesses do not only read uploaded files and chat sessions. They read
SAP, Salesforce, Coupa, CLM, QMS, MES, insurance claim systems, hospital EMR,
Figma, CMS, CRM campaign tools, and customer portals.

Design impact:

- `source_type` must include permissioned enterprise systems and web sessions.
- Connector reads must snapshot object IDs, page URLs, captured fields, capture
  time, and session/connector identity.
- Connector write nodes must be separate from connector read nodes.

### 2. Draft Writes And Final Submits Are Different Capabilities

Across bid submission, PoC configuration, claim review, payment release,
marketing launch, and医保申诉, the safe action is often “write draft” or “create
review task”, while final submission must remain human-owned.

Design impact:

- External actions need `action_kind`.
- Harness policies can allow `draft_write` but still block `submit`.
- The run UI must show pending final human actions explicitly rather than
  implying the agent completed the whole business process.

### 3. Outputs Need Semantic Assertion Types

Quality, medical, legal, finance, and compliance workflows repeatedly separate:

- facts extracted from systems,
- inference from those facts,
- professional recommendations,
- binding business decisions.

Design impact:

- Node output schemas should support assertion kinds such as `fact`,
  `hypothesis`, `recommendation`, `risk`, `calculation`, and `decision`.
- Only humans or explicitly authorized rule nodes can produce final
  `decision` outputs in high-risk workflows.
- Agent-generated hypotheses must remain reviewable and cannot be promoted to
  facts by formatting.

### 4. Rule Sets Must Be Versioned Inputs

Payment controls, revenue recognition, marketing law checklists, privacy
playbooks, DRG/DIP rules, insurance policy rules, quality standards, and vendor
risk policies all appear as rule sources.

Design impact:

- Rule nodes bind `HarnessRuleSetBinding`.
- Rule version, content hash, and effective date are part of the audit record.
- A missing rule version blocks the run.

### 5. Review Routing Is Business-State, Not A Comment Thread

Every scenario has role-specific gates: legal, privacy, safety, quality
manager, medical reviewer, finance controller, bid owner, service manager,
clinical doctor, or coding specialist.

Design impact:

- `human.review` needs role routing, required reviewer roles, due dates, and
  stale-decision handling.
- Conflicting review decisions must block the item.
- Agent output cannot overwrite a human decision.

### 6. Version Locks Need To Cover External Assets

Marketing assets, contracts, DPA templates, Figma frames, CMS entries, Braze
templates, payment batches, and bid attachments can change after approval.

Design impact:

- Source snapshots must capture hashes, revision IDs, version timestamps, or
  stable object versions.
- If an approved source changes, dependent review decisions become stale.
- The UI must explain which approvals were invalidated and why.

### 7. Batch Workflows Need Item-Level Recovery

Claims, payments, revenue samples, DRG appeals, requirement rows, and quality
evidence packages are batch-shaped workflows. One blocked item should not
always fail the whole run.

Design impact:

- `HarnessItem` is mandatory for batch harnesses.
- Failure aggregation is graph-configured: `block_run`, `block_item`, or
  `review_item`.
- Partial rerun starts from affected items and downstream nodes.

### 8. Evidence Packages Need A Common Shape

All scenarios independently converged on the same need: a field-level evidence
package that identifies source system, object ID, file version, locator,
captured fields, and action trace ID.

Design impact:

- `HarnessCitation` must support structured field captures, not only text
  previews.
- Artifacts should include machine-readable `*_evidence.json` packages.
- Audit reports render evidence packages, not ad hoc prose.

### 9. Sensitive Data Policy Is Part Of Harness Runtime

Healthcare, insurance, finance, vendor, and customer-support scenarios all
touch regulated or sensitive data.

Design impact:

- Source policy needs data classification, allowed retention, redaction, and
  export controls.
- Artifact generation must respect source retention and masking policy.
- Review UI should hide or mask fields based on reviewer role.

### 10. Reuse Comes From Shared Modules, Not Template Copying

The scenario set repeats common modules: clause extraction, rule matching,
field mapping, evidence packing, OCR, version locking, external draft writes,
review routing, and batch item lifecycle.

Design impact:

- These must be node catalog capabilities.
- Templates should compose the modules; they should not reimplement them.
- Tuning should update shared node configs, rule sets, or source mappings
  rather than adding harness-specific patches.

### 11. Some Harnesses Need Multiple Subagents

Several scenarios are materially better when multiple subagents work in
parallel under one parent harness run:

- bid response: technical, commercial, legal, qualification, and evidence
  auditor roles;
- supplier payment: invoice, PO/contract, bank-account, and control critic
  roles;
- health claim review: material completeness, policy rule, fee reasonableness,
  prior-condition, and medical critic roles;
- DRG/DIP appeal: payer feedback, home-page coding, EMR fact extraction, rule
  matching, and clinical/coding critic roles.

Design impact:

- Harness does not need a new permission system for these roles. Subagents
  inherit parent-agent capability and are narrowed by role policy.
- Multi-agent execution must be explicit in the graph through role fan-out,
  blackboard entries, reducers, critics, and review gates.
- Subagent output is not final state until schema validation, citation
  validation, conflict handling, and required human review pass.
- The trace view must show child-run lineage and which role produced each
  fact, citation, objection, draft, or recommendation.

## API Surface

Runtime API:

- `GET /harnesses/templates`
- `GET /harnesses/node-catalog`
- `GET /harnesses`
- `POST /harnesses`
- `GET /harnesses/{harness_id}`
- `POST /harnesses/{harness_id}/versions`
- `POST /harnesses/{harness_id}/versions/{version_id}/validate`
- `POST /harnesses/{harness_id}/versions/{version_id}/activate`
- `POST /harnesses/{harness_id}/runs`
- `GET /harnesses/runs/{harness_run_id}`
- `GET /harnesses/runs/{harness_run_id}/items`
- `GET /harnesses/runs/{harness_run_id}/review`
- `POST /harnesses/runs/{harness_run_id}/items/{item_id}/decision`
- `GET /harnesses/runs/{harness_run_id}/subagents`
- `GET /harnesses/runs/{harness_run_id}/blackboard`
- `GET /harnesses/runs/{harness_run_id}/external-actions`
- `POST /harnesses/runs/{harness_run_id}/external-actions/{external_action_id}/approve`
- `GET /harnesses/runs/{harness_run_id}/artifacts`
- `GET /harnesses/runs/{harness_run_id}/trace`

Next.js should proxy these routes for Web auth/session handling, matching the
existing Web runtime-client pattern. The proxy must not contain harness graph
validation, node execution, review state transitions, or evidence policy logic.

## Persistence

Proposed tables:

- `harness_definitions`
- `harness_versions`
- `harness_runs`
- `harness_sources`
- `harness_items`
- `harness_citations`
- `harness_decisions`
- `harness_artifacts`
- `harness_external_actions`
- `harness_ruleset_bindings`
- `harness_agent_roles`
- `harness_subagent_runs`
- `harness_blackboard_entries`
- `harness_templates`
- `harness_node_catalog`

Do not store large file bytes in these tables. Store file content through the
existing artifact/object-store path and keep references in harness rows.

Indexes should support:

- owner/team harness listing,
- active version lookup,
- run lookup by harness/version/session/user/status,
- pending review queues,
- item lookup by status and run,
- citation lookup by item,
- external action lookup by run/status/action kind,
- rule-set lookup by run and rule type,
- subagent lookup by run/node/item/role/status,
- blackboard lookup by run/item/kind/status,
- artifact lookup by run.

## Bid Excel Harness

Initial built-in template:

Inputs:

- one Excel workbook,
- one or more data sources,
- optional mapping hints.

Workflow:

```text
source.snapshot
  -> file.parse_excel
  -> agent.extract_requirements
  -> retrieval.search_evidence
  -> agent.answer_with_citations
  -> validate.citations
  -> human.review
  -> artifact.write_excel
  -> artifact.audit_report
```

Multi-subagent variant for enterprise bid work:

```text
source.snapshot
  -> file.parse_excel
  -> agent.extract_requirements
  -> retrieval.search_evidence
  -> agent.fanout(
       roles = [
         technical_spec_reviewer,
         commercial_terms_reviewer,
         legal_clause_reviewer,
         evidence_auditor
       ],
       foreach = requirement_items
     )
  -> agent.reduce
  -> validate.citations
  -> human.review(disputed_or_high_risk_items)
  -> artifact.write_excel
  -> artifact.audit_report
```

The guided setup can keep this simple by exposing role toggles such as
“technical review”, “commercial review”, “legal review”, and “evidence audit”.
The saved harness version records those toggles as `HarnessAgentRole` bindings
and graph fan-out policy.

Key item fields:

- sheet name,
- row,
- column,
- requirement text,
- answer cell locator,
- proposed answer,
- final answer,
- citations.

Completion condition:

- Every required answer cell is approved or explicitly rejected.
- Every approved answer satisfies evidence policy or has a recorded human
  override allowed by policy.
- The output workbook and audit report are generated.

## Skillify Harness

Initial built-in template:

Inputs:

- selected sessions,
- optional topic or goal,
- target scope: personal or project.

Workflow:

```text
source.snapshot_sessions
  -> agent.extract_skill_candidates
  -> validate.skill_candidates
  -> human.review
  -> skill.draft_from_sessions
  -> skill.validate_draft
  -> human.publish_decision
```

Multi-subagent variant for large session sets:

```text
source.snapshot_sessions
  -> agent.fanout(
       roles = [
         preference_extractor,
         workflow_pattern_extractor,
         writing_style_extractor,
         contradiction_critic
       ],
       foreach = session_chunk
     )
  -> agent.reduce
  -> validate.skill_candidates
  -> human.review
  -> skill.draft_from_sessions
  -> skill.validate_draft
  -> human.publish_decision
```

Rules:

- The harness creates a draft skill, not an automatically active skill.
- User review is required before activation.
- Each learned preference or workflow rule must cite the session source it came
  from.
- Rejected candidate rules must not leak into the final skill draft.

## Failure Policy

Harness must fail or block explicitly.

Examples:

- Parser cannot read Excel: run status becomes `failed` with a parser error.
- Retrieval source unavailable: affected items become `blocked`; run may enter
  `waiting_for_external_input` if policy allows source replacement.
- Citation validator fails: item becomes `pending_evidence` or
  `pending_review`.
- Agent node returns invalid output schema: node fails and records the schema
  violation.
- Subagent child run returns invalid output schema: the child run fails, its
  role output is excluded from reducer input, and the node follows declared
  failure aggregation.
- Subagent roles disagree on a material fact, citation, or recommendation:
  blackboard entries become `disputed` and the affected item enters review or
  block state according to policy.
- Human decision conflicts with current item revision: API returns conflict;
  caller reloads the current item.

There is no silent fallback to model-only answers, stale source content, or
best-effort output generation.

A harness version may declare an explicit degraded mode, but that is not a
runtime fallback. It must be visible during version validation and activation,
recorded in `runtime_policy_json`, and surfaced in the run UI before execution.

## Security And Permissions

Required checks:

- user can view and run the harness,
- user can access each source,
- user can upload or reference input artifacts,
- user can approve the item if the review policy requires a role,
- user can activate or publish a harness version,
- user can download generated artifacts.
- subagent role scopes are subsets of the parent run's source/tool/action
  capability.

Sensitive source excerpts and tool outputs must follow existing redaction and
artifact retention policies.

## Testing Strategy

Unit tests:

- harness definition validation,
- graph schema validation,
- graph node/edge/topology constraints,
- node input/output validation,
- evidence policy evaluator,
- external action policy evaluator,
- rule-set binding and version-lock validation,
- subagent role validation,
- blackboard entry status transitions,
- review decision state transitions,
- version immutability.

Integration tests:

- create harness from template,
- run bid Excel harness with fixture workbook and fixture source,
- block an item with missing evidence,
- approve/edit/reject pending items,
- export workbook and audit report,
- reconstruct trace from DB events,
- write a draft external-system field without final submit,
- spawn role fan-out subagents and reduce validated outputs,
- route disputed subagent outputs to human review,
- invalidate approvals when a source version changes,
- rerun only affected downstream nodes after an edit.

Contract tests:

- Web routes proxy runtime harness APIs without becoming orchestration logic,
- node catalog schema compatibility,
- no active version mutation,
- no final answer without required citation,
- no final external submit without explicit human action gate,
- no subagent scope broader than the parent run capability,
- no unordered child-run prose as reducer input,
- no Web trace dependency on local JSONL.

## Delivery Plan

### Phase 0: Design And Contracts

- Land this design.
- Define harness workflow schema.
- Define node catalog schema.
- Define graph validation constraints.
- Define subagent role schema, blackboard schema, and child-run lineage events.
- Define template configuration schema.
- Define bid Excel and Skillify template definitions.

### Phase 1: Runtime Foundation

- Add harness tables.
- Add `HarnessService`.
- Add definition/version/run APIs.
- Add template and node catalog APIs.
- Add source snapshot records.
- Add event emission contract.
- Add subagent run projections and blackboard entry persistence.
- Add minimal Web list/detail pages.

### Phase 2: Bid Excel MVP

- Durable upload storage for harness inputs.
- Excel parser node.
- Requirement extraction node.
- Evidence retrieval node.
- Citation validator.
- Optional technical/commercial/legal/evidence subagent role fan-out.
- Guided template configuration UI.
- Review queue.
- Excel output writer.
- Audit report artifact.

### Phase 3: User Graph Customization

- Node catalog UI.
- Graph validation UI.
- Version diff and activation flow.
- Partial rerun from edited item/node.

### Phase 4: Skillify Built-In Harness

- Session source snapshots.
- Candidate extraction.
- Optional preference/workflow/style/critic subagent role fan-out.
- User review.
- Draft skill generation through personal skill store.
- Skill activation flow.

### Phase 5: Extension Nodes

- Team-scoped node registration.
- Connector source nodes.
- Permissioned custom business nodes.

## V1 Decisions

- Keep `WorkflowService` internal to harness execution; do not expose it as the
  product model.
- Store uploaded harness input bytes through the shared artifact/object-store
  path, then reference those artifacts from harness rows.
- Store citation previews plus hashes in `harness_citations`; full source text
  follows source retention policy.
- Support three review roles in v1: owner, editor, reviewer.
- Ship template-parameter editing before the full graph builder. The persisted
  definition is still a graph, so graph customization can open later without
  migrating existing harness versions.

## Appendix: Scenario Corpus

These scenarios were generated from five role perspectives: bid/sales
engineering, legal/compliance, manufacturing quality/service, finance/internal
audit, and healthcare/insurance operations. They are intentionally concrete and
operational. Their purpose is to keep the harness design grounded in real
workflows rather than generic automation claims.

### A1. Manufacturing Equipment Bid Response And Compliance Review

Role perspective: enterprise bid and presales lead.

Business context:

A heavy-equipment manufacturer is bidding for an energy group's annual
procurement of compressor units and maintenance service. The company has five
working days to produce technical responses, commercial responses, deviation
tables, qualification attachments, and contract risk notes.

Inputs:

- Tender portal web session with project notice, attachments, clarification
  files, and upload draft area.
- Files: `招标文件正文.pdf`, `附件1_技术规格书.pdf`, `附件2_评分办法.xlsx`,
  `附件3_合同模板.docx`, `附件4_资格审查要求.pdf`, `澄清纪要_第1轮.pdf`,
  historical bid records, standard technical solution templates, and company
  qualification material index.
- Internal sources: equipment model database, legal clause playbook, ERP quote
  session, CRM customer history.

Workflow:

1. Snapshot all tender attachments from the portal, with URL, timestamp, file
   hash, and page screenshot.
2. Parse PDFs, Word files, and scoring Excel into a requirement matrix.
3. Map technical requirements to product models and specification records.
4. Extract commercial and contract clauses for payment, delivery, warranty,
   liquidated damages, IP, and confidentiality.
5. Map scoring items to draft response sections and evidence materials.
6. Generate technical response, commercial response, deviation tables,
   qualification checklist, and pending clarification questions.
7. Route technical deviations to technical experts, commercial terms to business
   managers, contract risks to legal, and attachment completeness to the
   qualification owner.
8. Draft-upload files to the tender portal only after validation; final submit
   remains human-owned.

Human gates:

- Any `not satisfied`, `partially satisfied`, or `needs clarification`
  technical item.
- Contract redline/high-risk clause.
- Quote, delivery date, warranty, or service commitment.
- Final portal submission.

Evidence requirements:

- Every tender requirement cites file name, page, section, or Excel cell.
- Every technical response cites product model record, product manual page, or
  historical project case.
- Every contract risk cites original contract text and legal playbook rule.
- Every portal action records URL, timestamp, screenshot, and file hash.

Outputs:

- `投标要求矩阵.xlsx`
- `技术响应草稿.docx`
- `商务响应草稿.docx`
- `技术偏离表.xlsx`
- `商务偏离表.xlsx`
- `评分点响应索引.xlsx`
- `合同风险清单.docx`
- `平台上传前检查报告.pdf`
- audit log with source files, agent actions, human reviews, and versions.

Blockers:

- Missing or unparsable technical spec, scoring method, or contract template.
- Any mandatory requirement without response or evidence.
- Any quote sourced from an unauthorized system.
- Portal upload mismatch or attempted final submit by the agent.

Design pressure:

This scenario requires `connector.draft_write`, `HarnessExternalAction`,
source version locks, role-based review routing, and explicit distinction
between draft upload and final submit.

### A2. Fintech Presales PoC Clarification And Demo Readiness

Role perspective: solution architect and presales lead.

Business context:

A fintech vendor competes for a bank's intelligent operations platform project.
Within two weeks the team must consolidate RFP requirements, clarify evolving
meeting notes, configure a PoC environment, prepare demo scripts, answer
security questions, and generate final customer materials.

Inputs:

- Files: customer RFP, two clarification meeting notes, PoC test cases,
  customer interface field sheet, security checklist, competitor notes, product
  capability matrix, solution whitepaper.
- Sessions: CRM customer account, Jira PoC tasks, Confluence product docs,
  demo environment admin console, BI dashboard configuration system.
- Web sources: bank public site, annual report, public regulatory references.

Workflow:

1. Create a PoC project workspace with customer, deadline, demo environment,
   target evaluators, and output folder.
2. Extract business, technical, security, demo, and delivery requirements from
   RFP, notes, and CRM records.
3. Resolve latest effective requirement when meeting notes override RFP text.
4. Map each requirement to product support status: standard, configurable,
   temporary mock, custom development, or unsupported.
5. Build PoC use-case matrix with prerequisites, account, test data, menu path,
   expected result, and screenshot requirement.
6. Draft-write demo environment configuration for tenant, roles, knowledge
   import, and workflows. Do not publish.
7. Create Jira tasks for missing mock APIs, demo data, configuration, and bugs.
8. Generate demo script and customer Q&A with evidence.
9. Run rehearsal, capture screenshots, duration, and failed steps.
10. Freeze final customer package after human review.

Human gates:

- Any `custom development`, `unsupported`, or `temporary mock` support status.
- Security, regulatory, personal-data, audit-log, SLA, performance, or
  implementation-time commitment.
- Demo environment publish and customer-visible dataset.
- Jira customer acceptance closure.

Evidence requirements:

- Each requirement cites RFP page, meeting paragraph, CRM meeting ID, or
  customer Q&A ID.
- Product support cites product matrix row, Confluence page version, or actual
  PoC screenshot.
- Demo steps cite environment URL, menu path, and screenshot.

Outputs:

- requirement traceability matrix,
- product capability mapping,
- PoC use-case matrix,
- demo environment draft config,
- Jira task map,
- demo script,
- security compliance checklist,
- Q&A sheet,
- rehearsal report.

Blockers:

- Conflicting requirement versions without latest-effective decision.
- Mandatory PoC case without prerequisite data, account, or expected result.
- Demo data containing real personal or production customer data.
- Unsupported capability rewritten as supported.

Design pressure:

This scenario requires requirement version override tracking, source-version
staleness, draft-write external actions, rehearsal artifacts, and support-status
outputs that cannot be promoted to delivery commitments without human approval.

### A3. Vendor Onboarding Compliance Review

Role perspective: enterprise legal and compliance operations lead.

Business context:

A cross-border SaaS company wants to onboard an India-based outsourced support
vendor for US and EU customers. The vendor may access customer names, email,
subscription plan, support ticket content, billing last-four digits, and
attachments. Procurement wants a decision in five working days.

Inputs:

- Coupa supplier onboarding request `COUPA-2026-0417`.
- Ironclad session with `MSA_IndigoSupport_v3.docx`,
  `DPA_IndigoSupport_supplier_template.pdf`, and `SOW_CustomerSupport_Q3_2026.docx`.
- Whistic/OneTrust assessment `VRA-2026-1192` with CAIQ, SOC 2, ISO 27001, and
  penetration test summary.
- Confluence policies: vendor risk policy, data classification standard,
  third-party access standard, vendor MSA legal playbook.
- Screening case `SAN-883921`, company registry data, privacy regulation
  library, and historical Jira vendor reviews.

Workflow:

1. Pull supplier, contract, security, screening, and policy sources.
2. Infer service scope and data-processing profile from SOW and request.
3. Classify data categories and initial risk level.
4. Extract MSA clauses for confidentiality, data protection, security,
   subprocessors, audit, breach notice, deletion, liability, indemnity, law.
5. Map DPA to GDPR Article 28 processor requirements.
6. Check SCC/TIA need for EU to India transfers.
7. Cross-check security questionnaire assertions against SOC 2, ISO, and test
   evidence.
8. Evaluate sanctions potential matches.
9. Compare historical similar vendor reviews.
10. Generate risk matrix, negotiation issues, approval packet, and system
    draft updates.

Human gates:

- High initial risk.
- EU data transfer to non-adequate country.
- Vendor DPA materially differs from company DPA.
- Any sanctions potential match.
- Low liability cap, data breach exclusion, or unsupported security control.
- Final approval with medium or higher residual risk.

Evidence requirements:

- Contract citations include file, version, clause, page, and excerpt.
- Security citations include report date, control ID, page or row.
- Policy citations include page title, version, section, and updated time.
- Screening citations include case ID, match status, and pull time.

Outputs:

- vendor compliance review memo,
- contract redline issue list,
- privacy and data transfer checklist,
- security evidence matrix,
- approval packet,
- draft updates to Coupa, Jira, Ironclad, and OneTrust.

Blockers:

- Missing MSA, DPA, or SOW.
- OCR below threshold.
- Confirmed sanctions match.
- Personal-data processing without DPA.
- EU transfer without SCC or equivalent mechanism.
- Unclosed human objections.

Design pressure:

This scenario forces versioned legal/privacy/security rule sets, evidence
strength ratings, residual-risk acceptance decisions, and no silent use of an
older rule or contract version.

### A4. Marketing Campaign Compliance Launch Review

Role perspective: marketing legal and compliance lead.

Business context:

A consumer fintech company plans a referral cashback campaign across California,
New York, and Texas. Channels include web landing page, app banner, email, SMS,
push, and paid social. Users get 25 USD after qualified referral activity. The
team wants compliance review in 48 hours.

Inputs:

- Jira epic `MKT-REFERRAL-2026-Q2` and channel tasks.
- Figma file `Referral Campaign Q2 2026` with landing, app banner, modal, paid
  social frames.
- Contentful entries for landing page and FAQ.
- Braze campaign `BRAZE-CAMP-8842` with email, SMS, and push templates.
- Google Drive legal folder with terms, eligibility rules, reward fulfillment,
  and claim substantiation spreadsheet.
- Internal marketing legal playbook, financial advertising standard, SMS/email
  compliance standard, privacy referral standard.
- FTC, CAN-SPAM, TCPA, and state checklists.
- Optimizely experiment `EXP-REF-2026-052` variants.

Workflow:

1. Pull campaign scope, channel list, owners, and launch date from Jira.
2. OCR Figma frames and extract visible claims.
3. Pull CMS, Braze, push, SMS, email, and experiment variants.
4. Build claim inventory with text, channel, position, audience, and risk tags.
5. Compare each claim to terms, eligibility, reward timing, state limits, and
   substantiation.
6. Detect high-risk words: instant, guaranteed, free money, no strings attached,
   everyone qualifies, boost your credit.
7. Run channel-specific compliance checks for SMS, email, push, paid social,
   app, and web.
8. Check privacy risk for contact upload and referral outreach.
9. Generate blockers, recommended copy, owners, and review tasks.
10. Lock approved asset versions; invalidate approval if Figma/CMS/Braze changes.

Human gates:

- High-risk claim words.
- Any claim inconsistent with terms.
- SMS invite or contact upload.
- Missing substantiation the business wants to keep.
- OCR confidence below threshold.
- Asset change after approval.

Evidence requirements:

- Figma citations include file, frame, node ID, version timestamp, OCR text.
- CMS citations include entry ID, field, locale, draft/published state.
- Braze citations include campaign/template/variant/channel.
- Terms and policy citations include file, version, clause/page/section.

Outputs:

- marketing compliance review memo,
- claim inventory,
- channel compliance checklist,
- redline and copy recommendation sheet,
- launch blocker list,
- final approval record,
- system draft comments/status updates.

Blockers:

- Core channel inaccessible.
- Final launch version cannot be identified.
- Terms missing or version mismatch.
- Unsubstantiated amount, instant, guaranteed, or financial-results claim.
- SMS without STOP or email without unsubscribe/address.
- Approval role missing.

Design pressure:

This scenario requires OCR as a first-class node, asset version locking, stale
approval invalidation, and `claim` as a structured item type with citations.

### A5. Automotive Supplier 8D Customer Complaint Closure

Role perspective: manufacturing quality lead.

Business context:

An automotive component supplier receives an OEM complaint: electric-drive
controller housings have shifted mounting holes and fail assembly. The OEM
requires containment in 24 hours, an 8D draft in five working days, and
permanent corrective action verification in ten working days.

Inputs:

- OEM supplier quality portal complaint with complaint ID, plant, model, part
  number, batch, defect quantity, photos, deadline.
- QMS historical NCR/CAPA/8D records.
- MES batch history with work order, line, machine, operator, shift, fixture,
  process parameters.
- SPC Excel for mounting-hole X/Y coordinates and CPK.
- CMM PDF report and outbound inspection records.
- ERP/WMS shipment and inventory records.
- Photos, videos, emails, and meeting notes.

Workflow:

1. Read customer complaint and download attachments.
2. Search QMS for same part and similar defect pattern over 24 months.
3. Trace batch in MES to work order, machine, fixture, shift, and parameters.
4. Analyze SPC trends around the complaint batch.
5. Compare CMM and outbound inspection with customer defect description.
6. Generate D1-D5 draft: team, problem statement, containment, root-cause
   hypotheses, corrective action suggestions.
7. Generate 24-hour containment response and customer 8D draft.
8. Mark facts, hypotheses, and confirmed conclusions separately.

Human gates:

- Supplier responsibility conclusion.
- Root cause to be sent externally.
- Containment scope.
- Stop-line, recall, or on-site support decision.
- Cost, claim, or liability language.
- Customer-facing 8D submission.

Evidence requirements:

- Complaint facts cite portal complaint ID and attachments.
- Batch facts cite MES work order, machine, fixture, and production time.
- SPC facts cite file, sheet, measurement point, sample time.
- CMM facts cite report ID, sample ID, and date.
- Root-cause hypotheses must be labeled as hypotheses until validated.

Outputs:

- `8D_Report_Draft.docx`
- containment response email draft,
- evidence index,
- batch traceability summary,
- SPC trend analysis,
- human review checklist,
- audit trail.

Blockers:

- Portal login/download failure.
- Batch cannot match MES/ERP/WMS.
- Missing key SPC/CMM data.
- Customer photo cannot confirm defect location.
- Root cause inferred without direct evidence.
- Quality manager review missing.

Design pressure:

This scenario requires semantic assertion types (`fact`, `hypothesis`,
`validated_conclusion`), evidence packages for numeric fields, and human review
before external responsibility statements.

### A6. Industrial Equipment Field Service Diagnosis And Service Report

Role perspective: manufacturing field-service lead.

Business context:

A semiconductor packaging customer reports that an automated dispensing machine
has unstable dispense volume for three days and has been paused from
production. A service engineer needs remote diagnosis, spare-part preparation,
on-site repair, recovery validation, customer report, and knowledge-base update.

Inputs:

- Field service management work order with customer, device serial, SLA, history.
- Equipment remote monitoring portal with alarms, pressure/temperature curves,
  valve counts, recipe changes.
- Uploaded log package: `alarm.log`, `process_data.csv`,
  `recipe_snapshot.json`, `maintenance_counter.csv`, `vision_offset.csv`.
- CRM contract/warranty status.
- Spare-parts system for valve, pressure sensor, filter, control board stock.
- Service knowledge base with fault codes, service bulletins, SOPs, cases.
- Photos/videos and customer emails.

Workflow:

1. Read service order and service history.
2. Pull seven days of remote monitoring data.
3. Parse log package and compute alarms, pressure variance, recipe diffs,
   maintenance counts, and vision offset drift.
4. Search knowledge base for same device/alarm/failure mode.
5. Generate ranked hypotheses with evidence, counter-evidence, and field tests.
6. Generate spare-parts and tool preparation list.
7. Draft customer update after service manager review.
8. After field work, ingest repair record, part serials, validation data,
   photos, and customer sign-off.
9. Generate service report and knowledge-base case draft.

Human gates:

- Permission to read customer recipe/production data.
- Customer communication of diagnosis hypothesis.
- Stop/continue/limited operation recommendation.
- Warranty responsibility or free part decision.
- Major customer incident escalation.
- Customer-facing report.
- Knowledge-base publication.

Evidence requirements:

- Fault facts cite FSM order and customer description.
- Alarm facts cite `alarm.log` code/time.
- Parameter anomalies cite `process_data.csv` field/time/range.
- Recipe changes cite `recipe_snapshot.json` diffs.
- Maintenance facts cite counter values and standard limits.
- Recovery facts cite before/after validation measurements.

Outputs:

- remote diagnosis summary,
- field service preparation checklist,
- customer update email draft,
- service report draft,
- spare-parts recommendation,
- failure evidence index,
- knowledge-base case draft.

Blockers:

- No customer authorization for remote data.
- Corrupt log package.
- Device serial mismatch.
- Missing baseline recipe or maintenance counter.
- Spare-part availability unknown.
- No post-repair validation data or customer sign-off.

Design pressure:

This scenario requires binary/blob file parsing nodes, connector permission
scopes, before/after validation evidence, and a rule that “recovered” cannot be
output without validation data.

### A7. Supplier Payment Compliance Review

Role perspective: finance control and internal audit lead.

Business context:

Each week the company processes supplier payments ranging from 50,000 to
3,000,000 CNY. Finance control must confirm each payment satisfies purchase
order, contract, invoice, acceptance, budget, bank-account, and approval rules
before release.

Inputs:

- SAP S/4HANA session for vendor invoice, purchase order, payment batch, vendor
  master, payment block status, bank account.
- Coupa session for PO detail, contract workspace, approval history.
- Invoice platform with PDF, OCR fields, and verification status.
- Contract repository with PDF contracts, amendments, payment terms.
- Uploaded `payment_batch_YYYYMMDD.xlsx`.
- `payment_control_rules.yaml`.

Workflow:

1. Import payment batch and create item per row.
2. Match each row to unique SAP invoice.
3. Verify invoice status, seller/buyer, amount, tax, and PDF.
4. Match PO and contract; block high-value no-PO payments.
5. Check goods receipt or service entry/acceptance evidence.
6. Extract approval history and compare to authorization matrix.
7. Check vendor bank account and recent changes.
8. Run payment control rules.
9. Generate per-payment evidence package and batch release recommendation.
10. Route high-risk items to finance control review; agent cannot release
    payment.

Human gates:

- Non-PO invoice above threshold.
- Bank account changed in past 30 days.
- Ambiguous milestone/payment terms.
- New supplier first high-value payment.
- PO/contract/invoice amount variance with business explanation.
- Related-party, consulting, marketing service, prepayment, or one-time true-up.

Evidence requirements:

- Every control result cites source system, object ID, captured fields, capture
  time, and action trace.
- Contract evidence includes file, version, path, page, clause, excerpt.
- Approval evidence includes approver, role, time, action, and authority source.

Outputs:

- payment batch review report,
- per-payment evidence JSON,
- payment release decision sheet,
- audit log with system pages, rules, evidence, and human conclusions.

Blockers:

- SAP invoice missing or ambiguous.
- Invoice cancelled/invalid/unverifiable.
- Payment bank account differs from vendor master.
- Payment exceeds remaining contract value.
- Required approval missing or insufficient.
- Duplicate invoice across payment batches.

Design pressure:

This scenario requires item-level batch lifecycle, field-level system captures,
finance rule-set binding, and a hard distinction between recommendation and
payment release.

### A8. Monthly Revenue Recognition And Major Contract Review

Role perspective: finance control and internal audit lead.

Business context:

During monthly close, revenue accounting and internal audit review the top 30
recognized revenue customers, especially SaaS subscriptions, implementation
services, usage-based billing, discounts, amendments, and SLA service credits.

Inputs:

- NetSuite/SAP revenue arrangement, revenue element, journal entry, invoice, and
  deferred revenue roll-forward pages.
- Salesforce account, opportunity, quote, contract, order pages.
- CLM files: MSA, order form, SOW, amendments.
- Product usage admin portal and monthly CSV usage report.
- Jira/ServiceNow/Zendesk implementation and acceptance tickets.
- Uploaded `revenue_close_sample_YYYYMM.xlsx`.
- `revenue_recognition_controls.yaml`.

Workflow:

1. Import sample and prioritize by materiality/risk.
2. Pull ERP revenue records and journal references.
3. Pull Salesforce contract/order/approval fields.
4. Resolve latest signed contract package, including amendments.
5. Extract service period, payment, acceptance, cancellation, refund, free
   period, SLA credit, usage pricing, discount clauses.
6. Branch by revenue type: subscription, implementation, usage, hybrid bundle.
7. Check activation, customer acceptance, usage, invoice, and service period.
8. Run revenue recognition controls and produce evidence IDs.
9. Classify differences: FX, discount, amendment, partial period, usage
   adjustment, manual journal, unexplained.
10. Generate workpaper and exception list; human reviewers decide judgmental
    issues.

Human gates:

- Non-standard terms, refund/cancel/free-period/SLA credit.
- Amendment affecting price/period/scope/acceptance.
- Implementation acceptance judgment.
- Hybrid bundle performance-obligation split.
- Manual journal entry.
- Contract version uncertainty.
- Materiality threshold exceeded.

Evidence requirements:

- ERP evidence includes arrangement, element, journal, amount, period, capture
  time.
- Salesforce evidence includes account, opportunity, contract, quote, status,
  approval.
- Contract evidence includes file/version/signature/page/clause/amendment
  impact.
- Delivery evidence includes ticket/project ID, go-live/acceptance date, owner.
- Usage evidence includes period, metric, quantity, unit price source, export
  file/time.

Outputs:

- monthly revenue close review report,
- per-customer workpaper,
- evidence JSON,
- revenue exceptions sheet,
- management control summary.

Blockers:

- Arrangement missing or inaccessible.
- ERP and sample amounts mismatch without explanation.
- Contract not signed/effective.
- Latest contract version cannot be resolved.
- Amendment missing.
- Revenue recognized before service start or without acceptance/usage evidence.
- Mandatory reviewer incomplete.

Design pressure:

This scenario requires latest-effective document resolution, rule nodes that
cannot replace professional accounting judgment, and output assertions that
separate risk findings from final accounting decisions.

### A9. Commercial Health Insurance Inpatient Claim Pre-Review

Role perspective: insurance claims operations lead.

Business context:

A commercial health insurer receives high volumes of inpatient claims through
its app. Customers upload discharge summaries, invoices, medical expense
details, insurance settlement sheets, and diagnosis certificates. Operations
must decide coverage, exclusions, waiting period, deductible, reasonable
medical cost, duplicate reimbursement, and medical-review routing.

Inputs:

- Claim core system session: claim detail, policy detail, customer history,
  payment recommendation draft page.
- Customer uploads: PDFs, images, XLSX expense details.
- Policy rule system with benefits, waiting period, deductible, payout ratio,
  exclusions, disease definitions, hospital level limits.
- Customer 360 with historical policies, claims, refusals, disclosure forms.
- Drug/medical catalog CSV or web query.

Workflow:

1. Open claim by `claim_id` and capture claim basics.
2. Check required documents and clarity.
3. Extract diagnoses, operation, length of stay, history, treatment, and expense
   category totals.
4. Cross-check invoice, medical settlement, and expense detail amounts.
5. Read policy terms and check coverage period, waiting period, hospital level,
   benefits, deductible, ratio, annual limit, and exclusions.
6. Check claim history and disclosure for suspected pre-existing condition.
7. Screen expense reasonableness and non-covered items.
8. Calculate recommended payable amount.
9. Draft initial review opinion in the claim system; do not submit final claim
   decision.

Human gates:

- Rejection or partial payout.
- Suggested payout above threshold.
- Suspected pre-existing condition, disclosure issue, or exclusion.
- High-value implants, special drugs, imported drugs, non-catalog items.
- Low OCR confidence or blurred materials.
- Amount mismatch across invoice/settlement/detail.
- Hospital level uncertain or terms ambiguous.

Evidence requirements:

- Every judgment cites policy rule, page/path, file name/page, page field, and
  calculation step.
- Rejection, partial payout, and medical-review routing require explicit
  evidence list.
- No “based on experience” wording.

Outputs:

- claim pre-review summary,
- document completeness checklist,
- expense review table,
- rule matching explanation,
- human review tasks.

Blockers:

- Claim or policy page inaccessible.
- Required invoice/discharge summary missing.
- Attachment corrupt, encrypted, or unreadable.
- Policy version unresolved.
- Amount fields inconsistent without explanation.
- Diagnosis and policy responsibility relationship unclear.

Design pressure:

This scenario requires strict sensitive-data policy, medical/insurance rule-set
binding, calculation evidence, and hard separation between draft recommendation
and final claim approval.

### A10. Hospital DRG/DIP Settlement Appeal

Role perspective: hospital医保 operations lead.

Business context:

A tertiary hospital receives daily DRG/DIP settlement feedback from the medical
insurance bureau. Operations must inspect deduction reasons,病案首页,
electronic medical records, rule files, and decide whether to appeal.

Inputs:

- Hospital settlement exception system session with case details and appeal
  draft page.
- Insurance bureau DRG/DIP feedback platform with batch, case ID, group,
  payment standard, actual cost, payment, deduction amount, reason, rule ID.
- EMR session: admission note, discharge note, operation note, progress notes,
  lab/imaging, orders.
- Medical record home page system with diagnosis/procedure codes, admission
  path, discharge mode, ICD-10 and ICD-9-CM-3.
- Rule files: DRG/DIP payment rules PDF, group mapping XLSX, deduction reason
  catalog XLSX.

Workflow:

1. Locate exception case by batch and case ID.
2. Read payer feedback, accepted group, reported group, difference, deduction
   reason, and rule ID.
3. Extract medical record home-page diagnosis/procedure fields.
4. Read EMR facts relevant to the deduction reason.
5. Locate current rule version and rule clause.
6. Match diagnosis, procedure, age, length of stay, fee structure, complication
   evidence, and operation record to DRG/DIP rules.
7. Classify recommendation: appeal, do not appeal, or needs coding/clinician
   confirmation.
8. Draft appeal reason and evidence links in internal system; do not submit to
   bureau.
9. Create review tasks for coder, clinician, and医保 office lead.

Human gates:

- Diagnosis or procedure code change.
- Appeal recommendation.
- Deduction amount above threshold.
- Death, ICU, malignancy, major surgery.
- Complication/comorbidity judgment.
- Medical record and home-page inconsistency.
- Rule ambiguity.
- Final bureau submission.

Evidence requirements:

- Each appeal judgment cites payer feedback, medical-record facts, and rule
  basis.
- Medical facts cite document name, date, field/page, and exact fact.
- Rule citations include rule file name, version, clause, and excerpt.

Outputs:

- exception case summary,
- home-page vs payer feedback comparison,
- medical fact excerpts,
- rule matching explanation,
- appeal draft,
- human review task list.

Blockers:

- Platform session expired or captcha blocked.
- Case mismatch across systems.
- EMR permission insufficient.
- Home page missing/unfiled.
- Rule version unresolved.
- Rule ID missing from rule file.
- Medical records conflict materially.
- Professional medical judgment required but review task not created.

Design pressure:

This scenario requires regulated-data masking, multi-evidence appeal packages,
rule-version locks, and enforced clinical/coding human review before official
submission.
