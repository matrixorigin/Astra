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

## Execution attribution

Routing evaluation must join selection evidence to actual inference execution.
The canonical inference ledger owns the admitted route, logical invocation,
physical provider attempts, and their terminal outcomes. Request diagnostics
project that attribution through `ModelRequestContextEvent.route`; they must
not construct a second route or credential authority.

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

The shared `TurnIntentJudge` can emit an optional typed `assessment`. It keeps
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
are discarded without rejecting a valid Work decision. Full injected turn-intent
judges use the same assessment contract and runtime binding. Fixed-default,
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

For offline training, a next-turn assessment is a delayed, noisy outcome label
for the referenced response. It must never become an input feature for that
response's original routing decision. New-request difficulty/urgency belong to
the new request only. Silence and simple continuation are not approval; expressed
satisfaction is not task success. Dataset creation still requires the consent,
redaction, lineage, evaluation, and split controls in
[evaluation-and-learning.md](evaluation-and-learning.md).
