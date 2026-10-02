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
resume under this contract, and v1 decisions must not be treated as v2 offline
training evidence. Deployments that need those records require an explicit
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
inference under the default capacity-aware policy, `boundary_only`, or `always`.
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

## Offline router datasets and candidates (stage 4)

`astra-test router-offline` builds a consented structural dataset and fits an
**offline-only** candidate. It does not load credentials, query production
traces, invoke providers/tools, activate a policy, or change Offering admission.
The router corpus uses explicitly consented structural evidence, not a product
analytics exporter. Dataset validation belongs to `services::model_routing::offline`; the
pure categorical trainer belongs to `turn_core::model_routing::offline`. The
harness supplies the explicit local file boundary.

New decisions carry an optional version-1 `features` snapshot, frozen before
primary inference: assessment presence, difficulty and its confidence,
read-only primary execution, and supported input. The offline Auto comparison
uses the immutable recorded selection to choose a paired replay arm, preserving
catalog and model-contract fallbacks beyond feature eligibility. Missing historical
snapshots remain missing; they are not reconstructed from eventual outcomes.
Old decisions remain readable. Older strict readers cannot read new feature
fields, so mixed-version deployments require coordinated upgrades.

The input is an explicitly reviewed evidence bundle, not a raw trace directory.
An independent authorization file approves content hashes of complete source
envelopes for one owner, dataset, redaction revision, expiry, and
`offline_model_routing` use. Withdrawing an envelope removes its approval.
Explicit revocations also cover referenced feedback source IDs, observed/replayed
execution IDs, verifier evidence IDs and replay snapshot roots, even when their
containing envelope remains approved or is ineligible for training. The same
derived lineage drives build/revalidation checks and `complete.json` so operators
can invalidate every artifact that retains withdrawn evidence. Every build/train
checks these grants and revocations. These files are local operator
attestations, not authentication tokens or automated proof of consent/redaction.
The allowlisted export excludes Work plans, prompt text, tool output, selected
model display strings, credentials, and free-text judge explanations. References
and group keys must be opaque identifiers and must be reviewed for disclosure.

Each source binds its actual immutable routing decision, timestamp and input
prefix to optional observed execution, follow-up evidence, and paired replay.
Profiles pin Offering identity and model contract revision. Human or executable
verifier evidence must target an execution and the dataset's versioned acceptance
rubric. Transport completion, satisfaction, a model's historical selection, or
missing feedback never establish correctness. Follow-up assessments remain
outcome-only and must resolve to the observed response's exact canonical prefix.
Observed episodes retain their full start time, including admission/judge work:
`started_at <= decision_at <= completed_at`. Replayed episodes instead require
`decision_at <= started_at <= completed_at`. Both must finish by the dataset's
creation time, and verifier evidence cannot precede completion. An outcome horizon
applies to original-turn feedback; independently produced replay episodes have
their own horizon starting at replay execution.

Paired evidence requires the same decision-time input, environment/tool/budget
snapshot and candidate profiles. Mutable sandbox IDs must differ. Both models
must have passed capability/access checks. The importer validates these
attestations and identities; it does not implement live historical replay or
prove isolation from a string. The existing replay API is still unavailable.
Use the existing model-matrix harness in independently provisioned isolated
fixtures to collect evidence; never replay production side effects or treat a
recorded transcript as the unchosen model's rollout.

Examples are split by decision time. Shared sessions, identical input prefixes,
and supplied workspace/repository/duplicate-task group keys cannot cross splits.
Within a split, transitive related groups contribute at most one representative,
chosen by source ID before inspecting label availability. Related examples and
failures remain in the export and coverage report. The exporter is responsible
for supplying complete grouping keys; this stage does not discover semantic
near-duplicates or train across owners.

Training uses only complete paired groups with known verifier labels and
full-episode prices. Both-fail and economy-wins pairs are retained. Provider,
tool and cancellation failures, unavailable prices, incomplete horizons,
unsupported candidate pairs, absent snapshots and unknown labels remain
separately visible; they are not silently counted as successes. Consequently,
reported policy metrics apply to the disclosed complete-pair cohort, not all
production requests.

The baseline estimates per-candidate acceptability and mean full-episode cost
in categorical feature buckets. It requires minimum independent support and
uses a Wilson lower-bound threshold heuristic. Validation selects a threshold
subject to a configured quality-regression limit and lower cost than strong;
test labels never influence fitting or threshold selection. Unsupported inputs,
non-read-only tasks and insufficient evidence abstain to strong. Reports compare
always-economy, always-strong, deterministic Auto and the learned candidate on
the same held-out groups, including acceptance, cost per acceptable task,
latency, abstentions, coverage and an economy Brier score. All attempts and
auxiliary inference must be included in each supplied episode cost.

These probabilities and intervals are not empirically calibrated guarantees.
Threshold selection is not a rollout gate: every report has
`production_qualified: false`, and candidates have `activation: offline_only`.
Representative data, verifier calibration, critical-task evaluation, broader
strata, powered quality/cost comparisons and controlled canaries remain necessary
before activation through the tuning-job owner. No production savings claim is
made from synthetic fixtures.

See [the offline router workflow](../guides/model-router-offline.md) for commands,
evidence preparation, outputs, and revocation handling.

## Qualification and offline shadow scoring (stage 5)

The local harness exposes `router-qualify`, `router-config-hash`,
`router-plan-hash`, and `router-shadow`. Services owns the typed tuning protocol and record in `tuning`;
`turn_core::model_routing::qualification` owns the pure evaluation gate and the
observational scorer. Both use the existing trainer, eligibility policy, dataset
builder, group representatives and lineage. There is no alternate online router,
activation registry, or new database projection.

A protocol pins owner, dataset, fitting configuration and evaluation-plan digests,
test/stratum sample floors, coverage, quality margin, required cost reduction, episode cost bound,
p95 latency ceiling, confidence and required structural feature strata. Its
`registered_at` is the outcome-free roster seal time, after all recorded routing
decisions and strictly before every held-out replay begins. It is distinct from
`validation_before`, which splits decisions into validation and test populations.
Supplied held-out observed completions and follow-up feedback must also follow
the seal. These checks include incomplete and nonrepresentative sources. The seal
must precede the final evidence snapshot's `created_at`. Registration is an
operator attestation; the workflow cannot prove that omitted outcomes were unseen.
The services-owned evaluation-plan digest covers the full manifest
(including split boundaries and outcome cutoff/horizon), source roster, grouping,
canonical input references, frozen decision-time features and recorded selected
Offering/contract identities used by the Auto baseline. It excludes replay
outcomes, costs and feedback, so those may arrive after the plan is sealed under
renewed source authorization. Source and group order are normalized. Changing
splits, membership, grouping, model/rubric scope or features requires a new reviewed
protocol; a reused dataset ID is insufficient. `router-plan-hash` computes this
digest without granting source authorization. Protocol files without the digest
are rejected. Training/validation replay labels and their horizons must mature before
the test period. Labels from later replays cannot qualify earlier test turns.

Qualification rebuilds from current authorized evidence. Test groups are chosen
before inspecting outcomes, using the same transitive grouping as training.
Incomplete representatives reduce coverage, and missing/unexpected feature strata
anywhere in the test evidence reject qualification, including nonrepresentative
members of related groups. It compares the learned candidate with both always-strong
and deterministic Auto on the same complete paired cohort. Overall quality
non-inferiority and cost improvement must pass, along with quality and descriptive
p95 latency gates in every prespecified stratum. A stratum may keep the baseline's
selection without independently saving money.

The statistical bounds are one-sided Hoeffding bounds over paired independent
representatives, with Bonferroni correction across the two baselines, quality/cost
statistics and all prespecified cohorts. Quality differences lie in `[-1, 1]`.
The cost statistic is `(1 - required_saving) * baseline_cost - candidate_cost`;
its lower bound must be positive overall. The episode cost ceiling is fixed in
the protocol: every known held-out replay cost is checked before eligibility
filtering, including failed/incomplete pairs, partial prices and examples excluded
by group representative selection. An exceeded ceiling rejects the gate and
disables its bounds, rather than clipping or dropping expensive failures. Missing
prices remain unknown and still reduce complete-pair coverage. Unknowns, insufficient power,
quality regressions, missing strata, excessive latency and absent validation
thresholds fail closed. These are conservative bounds conditional on the supplied
independence/grouping and bounded-cost assumptions, not evidence of production
representativeness or guarantees about the router's task probabilities. Repeated
protocol/model selection against the same test set invalidates that interpretation;
use a fresh holdout for another candidate selection cycle.

The tuning artifact binds candidate, dataset, protocol, source lineage, evaluation
time and expiry, and ends in `rejected` or `ready_for_shadow`. It always retains
`production_qualified: false`; the candidate remains `offline_only`.

Offline shadow scoring requalifies the candidate, then imports a separate currently
authorized dataset for the same owner, policy revision, rubric and model contracts.
Its decisions must follow the qualification dataset's creation time and cannot
share source IDs or supplied/implicit task groups with that dataset. Revocations
from either authorization apply to both datasets. It emits each historical and
proposed profile, frozen features, disagreement/abstention, scorer time and combined
deletion lineage. Out-of-scope or absent features abstain to strong. Scoring does
not require paired outcomes and does not read those outcomes to choose a model.

This is retrospective observational scoring, not an online shadow deployment.
Local scoring nanoseconds exclude admission, I/O and provider overhead. Disagreement
cannot establish that an unchosen model would have succeeded. Runtime Auto remains
the admitted deterministic policy. Representative paired collection, verifier
calibration, safety/critical-task review, live shadow overhead, stable session
canaries, authenticated activation, kill switch and rollback remain prerequisites
for a later production rollout.

## Controlled online rollout (stage 6)

The authenticated `/admin/model-router/{owner}` boundary publishes a per-owner
versioned deployment through `services::tuning::rollout`. Publication reruns the
canonical offline qualification over explicitly uploaded reviewed evidence and
current source authorization. A saved `ready_for_shadow` file alone cannot enable
routing. The operator supplies separate online-consent, verifier-calibration and
safety-review references with an expiry. These references are auditable operator
attestations, not automated proof of those reviews or authenticated historical
preregistration. No production corpus is read or replayed by the Server.

Publication starts in **live shadow**. The shared Auto preparation scores the
frozen features and checks the current owner-scoped Offering pair, billing scope
and model contracts. It records the proposed choice, abstention and elapsed
routing overhead (registry read, scorer and candidate admission) in the existing
immutable routing event. Actual selection remains deterministic Auto. No second
provider request is sent. The separate existing judge phase is not included in
this routing-overhead measurement; episode metrics must include it.

An admin can advance a current shadow deployment to a 1–1000 basis-point canary
only after its prereviewed minimum independent shadow-session count, admission
checks and p95 routing-overhead budget pass. The canary fraction is fixed for the
deployment and always leaves a control group. A salted hash of owner and session
assigns the cohort consistently across turns and host restarts. The durable run
fact pins deployment ID, revision, candidate digest, cohort and cohort assignment
probability; this probability is **not** a conditional model-action propensity.
Control uses deterministic Auto; treatment uses the canonical learned scorer.
Out-of-scope/unsupported/insufficient-evidence features abstain to strong. Current
Offering admission and contract compatibility still constrain every choice.
There is no promotion to unrestricted production or automatic savings claim.

Deployment state and consent revocations occupy one owner-keyed C0 table,
`model_router_deployments`. Compare-and-swap revisions reject stale operators;
each mutation and its authenticated admin audit record commit in one transaction.
Source revocations persist across replacements and stop deployments that depend
on them. Expiry disables new treatment. Operators must propagate withdrawals via
the revoke API and online-consent withdrawal via rollback; local authorization
files are not watched by the Server.

Rollback is an explicit terminal deployment state. New runs use deterministic
Auto. Pinned treatment runs recheck current deployment state at recovery and each
primary execution authorization, and stop if rolled back, expired, replaced or
revoked. They do not silently change model or replay completed effects. Requests
already authorized or in flight cannot be recalled. Control/shadow runs keep their
pinned baseline. Treatment is limited to primary execution: child admission is
rejected until child rollout provenance is implemented. This is separate from
stage 3's ordinary Auto inheritance. Explicit model requests remain unaffected.

The admin status endpoint projects an operational dashboard from canonical run
facts. It reports first-run-per-session cohorts, selection/abstention/disagreement,
and terminal completion. Admission failures, critical violations and routing
overhead include all observed runs. The first representative run is
chosen before looking at outcomes; repeated correction turns cannot overweight a
session. Shadow and canary epochs are separate. A bounded 10,000-row scan exposes
truncation and cannot satisfy the shadow promotion gate when truncated.

Admin-reviewed terminal-run outcomes reference a versioned acceptance rubric and
evidence, with separately optional correctness, correction, full-episode cost and
latency. They append idempotently to the existing run ledger with reviewer/time
provenance, including for terminal delegated runs. Sequential and concurrent
identical submissions preserve the first accepted reviewer/time envelope;
conflicting reports cannot change deployment state or its audit. Unknown quality
and prices remain unknown; cost per acceptable task is null unless all session
representatives have quality and complete prices.
These operational comparisons are descriptive, not a powered online qualification
or proof that unchosen models would succeed. A reported critical access/safety
violation is accepted in the run ledger before it rolls back the same deployment
under its database row lock. Acknowledgment requires that stop, even if its
revision advanced concurrently; an identical retry finishes the stop if the
previous request was interrupted after acceptance. Under the deployment lock,
critical reports against an already stopped deployment preserve the winning stop
reason, revision and audit. Other quality, cost,
correction and latency regressions require operator review and rollback; the API
does not continuously inspect production traces or invent missing labels.

Offline stage-4/5 qualification rejects treatment decisions as deterministic Auto
baseline evidence. Randomized canary analysis requires its own preregistered
protocol; cohort assignment must not be mistaken for action propensity.

See [the rollout workflow](../guides/model-router-rollout.md) for operator API
requests, revocation, reporting and storage compatibility.

Routing-budget failures retain their cohort, measured overhead and explicit failure
reason in the canonical routing decision before the run fails. They remain in
the dashboard and can receive reviewed outcomes; recovery cannot dispatch them.
The decision also pins the review rubric. Delayed outcomes and identical retries
remain valid after deployment replacement, using the original run's deployment
and rubric. A historical critical report never stops a replacement deployment.
