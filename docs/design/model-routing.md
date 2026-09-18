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
