# Introspect and reflect

> Status: target design contract.
> Last updated: 2026-09-28.

Introspect and reflect are first-class backbone capabilities. They are not debug-only tools and not prompt decorations.

## Ownership

This document owns:

- agent self-observation contract;
- introspection dimensions and response shape;
- reflection boundaries;
- model-visible diagnostics for state, context, capability, trace, provider, sync, and task state;
- safety boundaries for what the agent may inspect.

It does not own:

- raw trace storage, owned by [observation-plane.md](observation-plane.md);
- provider routing, owned by [capability-system.md](capability-system.md);
- context assembly, owned by [context-and-prompt.md](context-and-prompt.md);
- lifecycle transitions, owned by [runtime-lifecycle.md](runtime-lifecycle.md).

## Principle

```text
Introspect reports system facts. Reflect reasons over those facts.
```

Introspection must be factual, structured, and bounded. Reflection may synthesize strategy, uncertainty, and next actions, but should not mutate state by itself.

The errors facet includes current-run pre-dispatch refusals from the existing
tool records, explicitly marked `admission_rejected`, separately from execution
failures. It does not increment executor health or infer a dispatch. Records
without captured wall-clock time show unknown age rather than a fabricated
timestamp. The bounded projection retains the rejection's call and round
identity and credential-safe reason, without another storage read.

## On-demand authorized model discovery

`model_catalog({"limit":16})` discovers authorized active Chat models as JSON.
It is a deferred tool discoverable through `tool_search(select:model_catalog)`;
resident introspect points to it. Its only optional inputs are `limit`, `cursor`,
and `catalog_revision`. Unknown inputs and invalid pagination are typed errors.
Introspect retains its observation, Explain, and artifact-recovery selectors,
but model discovery no longer shares those diagnostic parameters.

Authentication binds an inert, non-serialized reader, inherited by dynamic
children and skill forks. HTTP and introspection share the authorization owner:
normal principals use `ModelService::user_model_catalog`; restricted Edge
registrations use `AuthService::external_catalog_by_scope`, never the owner's
full catalog. Chat/WebSocket and Work turns retain the authenticated principal.
Missing bindings (including local CLI) return `unsupported`; owner mismatches
return `unauthorized`. Reconstruction requires a fresh authenticated binding.

Only explicit discovery reads the catalog: one service read, potentially multiple
SQL queries, with a five-second deadline. Ordinary turns and other facets add
zero catalog/filesystem reads. Discovery bypasses diagnostic reads and never
searches local configuration. No cache, background refresh or admission grant
is created; execution still revalidates authorization and provider capabilities.

The top-level page carries Chat purpose, scope, time, revision,
items, total, returned count, cursor and coverage. Its allowlisted items expose
Offering/name/provider/access identities, placement, context limits and nullable
thinking capability/pricing—not descriptions, raw configuration, keys or endpoints.
Unknown prices remain null; configuration timestamps are not billing facts.

Pages default to 16, maximum 32 entries and 16 KiB for the complete envelope.
Continue with `cursor=next_cursor` and the same `catalog_revision`.
Sorting, revision and pagination use the one loaded snapshot; cursors identify
the last returned row and must exist in the current authorized set. A changed
revision returns `catalog_changed` and requires restarting. Oversized or unsafe
rows fail explicitly; source-bounded presentation preserves full inline JSON.
`complete` means the whole catalog fits this response; continuation pages remain
`page`. Empty success has total zero; failure has null total and a fixed typed
error with reason-specific retryability, never raw backend error text.

## Runtime and artifact observations

Explain snapshots are discovered lazily through the same `introspect` tool:
`explain={target:"previous"}` excludes the current server root, while
`explain={target:"run",run_id:"…"}` selects an exact authorized root in the
active session. Discovery returns the first bounded window and a fixed opaque
artifact handle for subsequent pages. Ordinary server chat preparation does
not discover or recover reports. Local CLI/Edge selectors explicitly report
unsupported; existing local handles remain usable through their local reader.
Identity, physical-absence-only recovery, capture completeness and window
semantics belong to [Explain mode](explain-mode.md).


Internal judgment usage is a physical-attempt fact. Session reflection reports
provider, offering, model, operation, attempt count, and known input/output tokens from
the authenticated inference ledger. An attempt without complete usage remains
visible with incomplete token coverage; it is never a zero-token call. Explain
uses the same ledger at turn scope. Classification confidence and reflection's
inferred confidence are distinct; neither proves that a direction was applied
or that Work was delivered.

Reflect also summarizes its existing bounded request-context window by run,
agent, Offering, provider, configured/upstream model, and purpose. Physical
retries count separately; repeated terminal request facts count once. At most
eight identity groups are rendered, with an explicit omitted-group count.
Deduplication uses the canonical physical request ID, not a second composite
identity derived from the attempt index. Conflicting terminal facts for that
ID count as one unknown request; their usage and model attribution are excluded
and the conflict count is visible. Input order cannot decide which conflicting
identity or usage wins.
The aggregate covers captured terminal requests, not complete session billing.
Exact, partial, unavailable, and unknown usage remain separate. Missing usage
renders unknown; a reported zero remains zero. Cache percentage is shown only
when every captured terminal request has an exact usage payload, the producer
reports cache coverage for every positive-input request, and the input
denominator is positive. Incomplete usage retains known token counts without
claiming a cache percentage. Display identities are bounded and escaped.
The session view covers the supported judgment operations (request admission,
skill routing, memory relevance/feedback, tool-result selection, verification,
and completion-proxy turn intent), not every auxiliary model call. Routine hint/summary projections
bound group detail and report how many groups were omitted.

The typed `model_requests` section reports ledger-window coverage separately
from terminal usage: accepted rows do not establish dispatch, a missing
provider-response ID does not establish that no request reached the provider,
and `delivery_unknown` remains distinct from failure or cancellation. Child
groups retain `parent_run_id`; an unavailable, empty, or capped capture never
means zero child inference or complete session billing. Execution facts may
include a scoped tool's disposition and typed result class, but not its prompt,
arguments, output preview, or arbitrary metadata.

Runtime introspection also exposes typed `judgment_usage` in its snapshot and
session/overview/recent/trace reports, using the same owner/session-scoped
service ledger projection. These are individual physical attempts with actual
provider, offering, model and operation identities, provider usage status and
nullable input/cache/output buckets. They are not invocation totals, primary
model usage, a judgment result, or permission to act. Their scope is supported
judgment operations in the session at ledger-read time, independently of the
live runtime snapshot's earlier cutoff and the requested recent/turn horizon.

The optional read has a two-second deadline and a 128-attempt capture cap.
Capture overflow retains the bounded physical attempts with `capture_truncated`
coverage. Captured attempt counts and token sums remain lower bounds; omitted
capture rows have an unknown count, distinct from exact display-omission counts.
Even fully reported captured attempts cannot establish complete session totals.
No pool, timeout, query failure, unavailable
capture, and an excluded durable source are typed coverage states and do not
fail introspection. `live_only` and `local_only` skip this durable read.
Missing token buckets stay unknown, including unreported cache inputs; text
reports known input subtotals as incomplete.
Aggregate known input/output lower bounds and independent completeness flags
cover all captured attempts before display truncation, including omitted detail.
The same full capture is grouped by provider/offering/model/operation, with
physical attempt counts, known input/output subtotals and independent
completeness flags. Group detail uses the same depth limits and reports
`omitted_groups`; no displayed group's totals are computed from the truncated
attempt list. Mixed providers therefore never become a claimed Jev-only total.
Unavailable ledger totals remain null; missing usage contributes no known tokens
and marks the corresponding total incomplete. Hint/summary/diagnostic/forensic
retain at most 2/8/16/32 attempts with explicit omitted counts. Identity display
fields are capped at 128 characters and truncation is reported; these display
identities are never execution references. Other facets do not load this data.

The bounded model view retains a compact `judgment_usage` ledger summary ahead
of routine observations. It copies the captured totals and coverage without
recounting displayed groups, omits individual attempts with explicit counts,
and adds complete identity groups only while the model budget permits. If the
summary itself cannot fit, `projection_budget.omitted_fields` names
`judgment_usage`. Missing auxiliary evidence must not be inferred from the
separately scoped runtime request/run accounting.

Semantic judgment results are separate from physical usage. The shared
owner/session-scoped C3 projection exposes captured request classifications,
closed abstention/conflict/invalid-response reasons and explicit preparation or
execution unavailability. Initial classification and clarification remain
separate stages; subsequent planning failure does not overwrite their results.
Normalized answer values retain their provenance; discrete values are category
encodings, not calibrated confidence. Invocation correlation is unknown unless
an authoritative invocation reference is available. Consumers must not infer a
provider, token count, or physical call count from semantic observations.

The projection bounds candidate trace rows as well as displayed observations.
Exact duplicates collapse; conflicting observation identities are excluded and
reported as a coverage gap. Even an empty successful query describes captured
observations only: trace buffering, ingestion and retention can lose events.
Unavailable sources, source-policy exclusion, capture truncation and display
omission remain distinct. A valid classification does not prove model adoption
or improved task outcomes. Missing observations do not prove inactivity. The
bounded recent trace window is not a complete session-wide judgment count.

Explain presents these semantic facts as fixed-label preparation milestones.
Their zero-length intervals mark observation instants, not inference latency;
measured provider duration and usage retain their existing owners. Labels are
derived from the typed facts and are never parsed back into semantic state.

Historical tool-result selection observations retain a separate shared read
projection for explicit `facet=trace` introspection and reflection; routine
overview, recent, and session views omit this historical source without querying
it. Omission is not evidence that no historical judgments exist. The agent loop no longer evaluates
or applies new tool-result selection decisions: the optional semantic rerank did
not demonstrate a reliable reduction of the final provider context, while even
the no-auxiliary route scanned candidates and read frozen decisions. Trace
decoding remains for historical audit; the unused recommendation builder and
trace producer are removed rather than kept as a dormant execution path.
Historical evaluation traces describe recommendations, and immutable receipts
describe
what an earlier provider wire contained; neither is evidence of a new runtime
selection. A recommendation without a receipt has unconfirmed application. A
receipt without a trace is valid historical application evidence but does not
recover the missing evaluation rationale. A `Started` trace without a terminal
trace is reported as missing terminal evidence, not inferred to be a
cancellation or interruption. Evaluation and application capture have
independent bounded-coverage states; conflicting identities are quarantined
without discarding unrelated facts. Default text is a compact outcome summary;
hashes, ranges and internal identities remain diagnostic details.

Runtime tool fallback permitted by source policy uses the populated runtime snapshot.
The unused `InspectionService` journal-derived summary path is retired; missing runtime evidence
remains unknown rather than being reconstructed as zero or task progress.

## Goals

- Give the agent accurate self-awareness without exposing unsafe internals.
- Let the agent explain why a tool is unavailable, blocked, degraded, or hidden.
- Let the agent understand current run/session/task/sync/provider stage.
- Preserve Web/CLI/Edge parity at the backbone level.
- Avoid repeated exploration caused by missing state visibility.
- Keep prompt-cache stable by exposing dynamic state through compact structured introspection.

## Introspection dimensions

| Dimension | Answers |
| --- | --- |
| `state` | Current session/run/turn/task status, stage, terminal/resumable state. |
| `capability` | Available, hidden, blocked, offline, degraded, or unsupported capabilities. |
| `provider` | Provider bindings, selected routes, fallback policy, health, offline reason. |
| `tool` | Visible tools, why hidden/blocked, expected argument contract, last failures. |
| `context` | Loaded context blocks, compaction status, memory/artifact references. |
| `invocation lifecycle` | Prepared/dispatched/terminal counts, dispatch certainty, reconciliation, archive/reference ownership, maintenance progress. |
| `prompt_cache` | Stable prefix identity, dynamic block changes, cache-affecting differences. |
| `trace` | Recent causal events, tool lifecycle, retry/cache/provider decisions. |
| `sync` | Outbox/ack/degraded/poison/action-needed state. |
| `memory` | Retrieved memories, confidence, conflicts, provenance. |
| `plan` | Plan mode state, blocked mutation policy, pending plan tasks. |
| `safety` | Permission state, sandbox boundary, side-effect policy. |
| `budget` | Token, cost, retry, fanout, and time budget when available. |

## Response contract

An introspection response should be structured:

```text
dimension
status
summary
facts[]
blocked[]
degraded[]
next_actions[]
refs[]
```

Facts should be concise and attributable. Raw logs should not be returned by default.

The resident `reflect` schema accepts a concrete `question` for its default
summary view. For typed options such as `facet`, `depth`, or `horizon`, select
`reflect` with `tool_search(query="select:reflect")`, then call `invoke_tool`
with the selected contract. Selection preserves the resident schema and its
prompt-cache identity. A diagnostic succeeds only when its tool outcome
succeeds; requesting valid parameters alone does not prove observations were
obtained.

Routine self-diagnosis starts with a summary overview (or hint for a quick
check), reusing applicable observations. Current-state questions use Introspect;
persisted execution questions can use resident Reflect directly, without a
mandatory live observation first. Keep each fact's run/turn scope separate from
the observing invocation: current snapshot counters are not previous-run totals,
and waiting duration is not the child's execution duration. Captured execution
does not establish independent task verification; verification is warranted by
an unmet acceptance condition or counter-evidence, not delegation alone.
System guidance, tool descriptions,
and bundled workflows must agree on this default. A concrete evidence gap or
an explicit deep-audit request can justify deeper inspection; no fixed call
quota limits recovery. Snapshot claims must exclude later diagnostic calls,
and completed-turn totals must not be presented as totals for an ongoing turn.

Oversized structured introspection has a distinct bounded model projection;
the full durable report remains unchanged. This projection preserves complete
JSON and supporting evidence identities, prioritizes important observations,
and explicitly lists omitted fields and counts separately from the source
report's budget. It does not split JSON or duplicate the causal graph. Another
live introspection obtains a new snapshot, not a continuation of the old one;
omission alone is not a reason to request more evidence.

## Capability introspection

Capability introspection must distinguish:

- tool does not exist;
- no provider owns the capability;
- provider exists but offline;
- runtime binding missing;
- plan/policy blocks the call;
- argument shape is malformed;
- fallback is available;
- fallback was selected.

The agent should never have to infer these from generic tool errors.

## Context introspection

Context introspection should report:

- which context blocks were loaded;
- why they were loaded;
- what was compacted;
- unresolved constraints;
- memory conflicts;
- artifact references;
- provider/sync state included in prompt.

It should not dump the whole prompt unless explicit debug permission allows it.

## Reflection

Routine reflection defaults to `summary`. `hint` and `summary` return bounded
observations, supporting evidence, and prioritized actions without expanding
the full causal graph. Summary may retain a bounded, run-scoped execution spine
from the existing event window: spawning, waiting, termination, and result
adoption are distinct facts, not interchangeable completion verdicts. Typed run
and agent identities establish attribution; missing, conflicting, redacted, or
budgeted-away facts remain unknown. Absence of a `get_result` call does not prove
that no result was delivered. This projection adds no database queries or
lifecycle authority. Reflect and Introspect share support-aware evidence
selection, keeping selected observations and their evidence together.
Omitted material is reported through the result budget;
retained observations and actions must not contain dangling evidence references.
Explicit `diagnostic` and `forensic` requests retain deeper evidence and graph
inspection. This is progressive disclosure, not a usage quota or tool disablement.
Server-backed and local-journal reflection share the same report projection.

CLI reflection and introspection resolve the profile-local and attached
authenticated-account journal owners from one CLI identity snapshot; a
conversation cursor is lineage, never read authority. Each authorized source
uses the same bounded observation reader (at most 512 records / 256 KiB per
source), with session identity checked before merging and duplicate run/round
facts counted once. Account runtime rounds and trace spans can therefore be
observed even when the root conversation is profile-local. Conflicting rounds
are excluded and reported, not attributed by file order. Typed semantic
judgments from both authorized sources use one canonical deduplication and
conflict projection; an empty account window cannot hide a local fact. This is bounded historical
evidence (`local_journal_at_read`), not complete session history or a billing
ledger. Missing sources, truncation, malformed records and identity mismatches
remain explicit coverage gaps; an empty window does not prove no judgment ran.
These reads are local files only and do not add database or network calls.
An I/O error in either journal degrades that source's coverage without
discarding valid observations from the other authorized source; a
session-identity mismatch still fails closed. Default CLI reflection reuses this already-read bounded window for
local evidence, while retaining its existing cloud snapshot lookup; it does
not reread the entire local journal for presentation.
Execution reflection can report the count and slowest duration of captured
LLM rounds plus the longest captured trace span; overlapping spans are not
summed into wall time. Preview metadata is an allowlist, not raw trace attrs.

`local_only` CLI reflection bypasses cloud restoration entirely. CLI reflection
with `cloud_only` reads only the cloud snapshot, not local workspace or journal;
`live_only` has no live CLI observation source and reports unavailable without
falling back to persisted state. `auto`, `live_first`, and `durable_first`
reuse the bounded local window and the existing cloud snapshot lookup.
CLI reflection
and introspection can read physical judgment usage from the latest owner-local
typed Explain artifact, bounded to 4 MiB with a 16 KiB index. The reader checks
handle, checksum, size, schema and session/run/turn identity, then uses the
canonical graph's auxiliary-attempt projection and shared operation filter.
The scope is `local_captured_run_turn`, not session-ledger totals or necessarily
the current turn. Historical capture remains incomplete; known token sums are
lower bounds and unknown cache buckets stay unknown. Missing, invalid or
unavailable captures never fall back to an older artifact or generic LLM-round
counters. Local semantic journal coverage and captured-run usage are independent
sources. Source-excluded and unrelated facets do not read these local artifacts.
Repeated physical attempts are counted once. Conflicting attribution or known
token buckets, or conflicting turn/usage facts, make captured usage unavailable;
a higher usage-status rank cannot override contradictory evidence. Explain's
text, TUI, SDK/Web and HTML views share this rule. A conflicting incoming turn
fact remains a coverage conflict even when it is discarded and the retained
node has no usage; consumers must show unavailable totals rather than hide the
usage section. Conflicts are neither zero usage nor producer truncation.
Lightweight reflection retains scoped usage in typed
fields without replacing execution diagnoses in the summary.

Reflection may produce:

- uncertainty assessment;
- strategy adjustment;
- retry/fallback recommendation;
- request for user clarification;
- risk summary;
- next-step proposal.

Reflection must not directly execute tools, change tasks, alter provider bindings, or approve permissions. It may request those actions through normal lifecycle/capability paths.

## Plan mode

Plan mode should preserve introspection and reflection. Mutating tools may be blocked, but the agent still needs to know:

- what it would do outside plan mode;
- which provider would execute it;
- what approval or state transition is required.

## Prompt-cache interaction

Do not rewrite large system prompt sections to update introspection state. Keep the introspection protocol stable and put dynamic facts in compact blocks or tool responses.

## Test obligations

- Missing provider binding is visible through capability introspection.
- Plan mode reports policy blocks without hiding all tools.
- Edge offline is reported as provider state, not generic failure.
- Compacted context remains explainable.
- Reflection cannot mutate state directly.
- Introspection works in Web without Edge.
