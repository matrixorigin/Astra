# Model routing

> Status: target design contract.
> Last updated: 2026-07-19.

Model routing defines how Astra selects models under quality, latency, cost, safety, context, and provider constraints.

Eligibility, account binding, credential placement, billing ownership, and inference execution are owned by [model-access-and-inference.md](model-access-and-inference.md). Routing may choose only among the effective Offerings produced by that contract.

## Principles

- Model routing is a policy decision and must be traceable.
- Cheap model use must not erase safety or correctness requirements.
- Escalation should be explicit and measurable.
- Per-agent overrides are allowed but bounded by policy.
- Routing decisions must be reproducible for evaluation.

## Inputs

- task type and risk;
- context length;
- tool complexity;
- user/account policy;
- agent profile;
- provider availability;
- latency/cost budget;
- prior eval results;
- safety requirements.

## Routing outcome

```text
effective_offering_id
resolved_route_id
reason
fallback_chain
budget
quality_tier
safety_tier
context_strategy
trace_event_id
```

Provider, endpoint, credential, execution placement, and billing owner are resolved Server-side from the selected Offering. A routing policy cannot invent or override them.

Server inference requires admitted Offering material before using a cached
request configuration. Legacy model-name fallback metadata does not authorize
a route change. Rate-limit waits and physical retries retain the admitted route;
a different Offering must pass model admission before execution.
Within a live Server execution, an admitted Offering is reused for up to
30 seconds across model and readonly-tool rounds. Config projection and cache
hits do not renew this grant. On expiry, canonical admission refreshes the
pinned Offering; failure blocks dispatch and identity drift requires explicit
reselection. Route/credential changes can therefore take up to this interval
to affect an already admitted execution. Resume and explicit model switches
retain fresh admission. This model-config lifetime does not grant identity,
permission, or execution custody.
Model dispatch consumes the existing exact Run owner's confirmed lease window
locally. Only the heartbeat renews that window; a model round never extends it.
Expired, retired, mismatched or settling ownership blocks another dispatch.
Local cancellation signals remain immediate. Cross-process control is observed
by the existing two-second watcher, subject to read latency; it is not a new
synchronous transaction at each model round. Effectful tools continue using
transactional dispatch admission. Unchanged input watermarks avoid empty guidance
reads; changed notifications and final settlement still reconcile durable input.
The shared cooldown interface records errors and wait/reject decisions only;
it does not select another model or maintain fallback state and counters.

Managed model quirks use a strict schema. Unknown controls, including the retired
model-name `fallback_chain`, are rejected instead of silently stored. Explicit
`request_body_overrides` remain the provider-extension boundary. CLI YAML imports
validate every model before sending the first write or connectivity check. A
quirks update replaces the complete object and remains subject to the existing
configuration concurrency fence.

## Execution attribution

Routing evaluation must join selection evidence to actual inference execution.
Offering admission owns route and credential authority. Agent physical requests
are observed in process under their existing Run authority; standalone completion
and background-accounting requests retain the inference ledger. Diagnostics may
project those facts, but must not become another route or execution authority.
Agent request observations and usage are not guaranteed to survive process failure.

A route identifier belongs to a logical invocation, not a whole user turn or
an adaptive routing decision. Physical retries share its route and invocation
identifiers while retaining distinct request identifiers. Future adaptive
decisions must reference these identities explicitly rather than group requests
by a display model name. Preserve the distinction between the configured model
name and the model name sent upstream.

Missing selection rationale, historical route attribution, or response-quality
labels remain unknown. A provider request marked `succeeded` proves transport
completion under its inference contract; it does not prove task correctness.
Cost evaluation must respect the usage-coverage contract in
[observation-plane.md](observation-plane.md#model-request-attribution-and-usage).

An Auto routing decision that reuses a Work judgment also retains that
judgment's child-model requirement and authenticated user-intent identity.
Recovery may rebind it to a new run-owner generation and advanced event cursor
only when the user, session, run, turn chain, and instruction digest still
match. Missing or changed source evidence fails closed; a restored
`not_applicable` answer cannot erase a
previously positive child-model requirement. This remains one durable routing
fact, not a second judgment or database projection.

Decision schema v2 intentionally does not read v1 routing events: v1 did not
retain the child-model judgment or its source. Existing v1 Auto runs cannot
resume under this contract. Deployments that need those records require an explicit
separate migration decision; runtime does not guess the missing fields.

## Escalation

Escalate when:

- low-tier model reports uncertainty;
- tool plan is high-risk;
- context is complex;
- evaluation policy requires stronger model;
- safety classifier requests review;
- repeated retries indicate model/tool mismatch.

## Test obligations

- Routing decision is traceable.
- Per-agent override respects account policy.
- Safety-critical tasks do not route to disallowed models.
- Fallback preserves prompt/context contract.

## Next-turn observations (stage 2)

Work-admission judgment can emit an optional typed `assessment`. It keeps
response satisfaction (satisfied/mixed/dissatisfied/unknown) separate from the
new request's difficulty (easy/moderate/difficult/unknown) and urgency
(normal/urgent/unknown). Each dimension has its own categorical confidence;
these are judge estimates, not calibrated probabilities or correctness scores.
An angry simple correction need not require a stronger model. A polite complex
request may. Missing fields and unavailable judges produce no inferred labels.
The production Work-admission judge emits the same optional assessment in its
existing request. The shared turn entrypoint captures the source and preceding
exchange before primary rounds, and the completed admission records observations
on that source even when the result arrives later. Invalid optional assessments
are discarded without rejecting a valid Work decision. Fixed-default,
already-bound Work, and capacity-policy skips do not add a call for observation;
their missing assessments remain unknown. Native TypeSafe responses preserve the
optional assessment through answer validation for the admission parser to decode.

The canonical user-message semantics marker stores the assessment beside its
source text, without duplicating prompts or adding a database table. A missing
assessment can be filled after objective/feedback semantics have been recorded,
without replacing those semantics or replaying their effects. An existing
assessment and its response reference are preserved when the objective is later
resolved. When the judge explicitly targets the immediate previous response,
runtime binds a `feedback_response` reference to the sanitized canonical prefix
ending at that assistant message: its content root and message count. The reference owner
excludes optional user-turn semantics from the root, matching persistence's
content identity rule, so carrying those annotations forward cannot invalidate
the link. This is a snapshot
reference, not a model-generated ID or an inference `route_id`. Resolve it only
against matching retained canonical history in the source session/branch;
compacted or unavailable evidence stays unresolved. Earlier/multiple/ambiguous
targets remain unlinked. Source resolution prefers the submitted canonical
payload over an older occurrence of the raw intent. Assistant text uses the same
normalization for the judge and reference. Assistant rounds after the source
prompt are excluded, and a rewritten source is never relocated by matching text.
Telemetry identifies the feedback-producing run as `source_run_id`, rather than
claiming it is the response being rated.

The optional fields preserve reads of old records. Older strict readers reject
records containing the new fields, so mixed-version deployments need coordinated
upgrades. Canonical persistence and restore carry the fields with the existing
semantics marker. No additional model call, routing change, policy activation,
or automatic training export is introduced by this stage.

A next-turn assessment is a delayed, noisy observation
for the referenced response. It must never become an input feature for that
response's original routing decision. New-request difficulty/urgency belong to
the new request only. Silence and simple continuation are not approval; expressed
satisfaction is not task success. Dataset creation still requires the consent,
redaction, lineage, evaluation, and split controls in
[evaluation-and-learning.md](evaluation-and-learning.md).

## Deterministic Auto selection (stage 3)

Auto is opt-in for primary HTTP chat turns using Server-catalog Offerings.
Explicit selections and Server-default Work admission retain their existing
behavior. Configure a qualified pair in the Server's `runtime.toml`:

```toml
[model_routing]
revision = "evaluated-pair-v1"
economy_offering_id = "economy-offering"
strong_offering_id = "strong-offering"
```

No policy is enabled by default. The operator must establish the economy
candidate's lower cost and acceptable task quality for these exact model
revisions before enabling the pair. This implementation neither infers model
quality from names/prices nor claims measured savings. The policy revision
identifies that qualification; a model or pricing change requires re-evaluation.

Send `execution_policy.model_routing = "auto"` without `model_selection`.
The TypeScript SDK exposes this as `modelSelection: "auto"`. Explicit model
state combined with Auto is rejected, rather than silently overridden.
Provider-authorized model gateways, Device models, bound Work requests, and
fixed-default semantic admission are unsupported for Auto in this stage.
CLI/Web selectors and WebSocket chat continue to use explicit Offerings.

The configured strong Offering is admitted for the authenticated principal
first. Auto treats model selection as an explicit semantic-admission boundary:
it starts or awaits the shared bounded Work-admission request before primary
inference under the default `boundary_only` policy, explicit `capacity_aware`, or `always`.
It reuses an existing decision or in-flight request. `disabled` still suppresses
the judge, and missing or unusable assessments retain strong. Ordinary non-Auto
turns retain their existing admission timing. The pure policy
`easy-read-only-v1` selects economy only for high-confidence `easy` assessments,
read-only primary execution with no required Work or additional capability,
and text-only input/history with an exact source binding. Satisfaction and
urgency do not reduce the quality tier. Absent/uncertain observations and
unsupported input retain strong; there is no prompt-length or keyword rule.

Economy must be active in the principal's effective catalog, share the strong
Offering's access identity and execution placement, and pass canonical
Offering admission. It must preserve provider, generation/reasoning and cache
contracts and have known context/output limits at least as large as strong.
Unavailable or incompatible economy candidates retain strong. These
conservative conditions avoid introducing model-dependent prompt loss in the
initial policy. Existing request budgets remain in force; no new monetary
budget guarantee is introduced.

The policy snapshot is retained in run admission metadata. Before primary
dispatch, a generation-fenced immutable `model_routing_decision` run event
records the policy/algorithm versions, run/session identities, input-prefix
reference, assessment, selected Offering/model, contract digest, reason, and
the complete typed Work-admission decision (including its graph, topology,
capabilities, and skill revision). The existing fenced run-event transaction
atomically updates the run's effective Offering/model alongside that immutable
fact; child admission therefore inherits the same selected identity. There is
no new table. Failure to persist stops primary inference. Model material, loop invocation
material, runtime manifest, and model-specific tool policy are
updated together before context preparation. Dynamic-agent and forked-skill
executors created during baseline admission refresh inherited execution from
the durable choice and reauthorize it before use. Endpoints and credentials are
never stored in the decision. Primary inference ledger records join through
the owning run and selected Offering; auxiliary judge calls retain their own
purpose and usage attribution.

One choice governs all primary rounds of the run. Host recovery loads the
immutable decision and re-admits that Offering, even if the current policy
has changed. A recovered host restores the saved Work graph, topology, and
capabilities through the existing semantic owner before primary execution,
without another judge call or duplicate feedback observation. If no Work
decision was available at selection, normal semantic admission still runs.
A changed skill revision invalidates the restored Work decision through the
existing admission path. Missing decisions on a resumed nonzero round, revoked access,
malformed facts, and changed model contracts stop execution; they cannot
silently reroute or replay tool effects. Credential rotation is permitted
through normal reauthorization. Learned routing, mid-turn escalation, and
independent child routing remain later stages.


Auto uses only the deterministic policy. Learned candidates, offline router training, shadow/canary deployment, rollout registries and router administration APIs are retired. Saved decisions with rollout provenance or a different policy version are rejected before dispatch, inheritance or append; recovery never substitutes a new model.
