# Introspect and reflect

> Status: target design contract.
> Last updated: 2026-07-07.

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
The session view covers the supported judgment operations (request admission,
skill routing, memory relevance/feedback, tool-result selection, verification,
and completion-proxy turn intent), not every auxiliary model call. Routine hint/summary projections
bound group detail and report how many groups were omitted.

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
check), reusing applicable observations. System guidance, tool descriptions,
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
the causal graph. Omitted material is reported through the result budget;
retained observations and actions must not contain dangling evidence references.
Explicit `diagnostic` and `forensic` requests retain deeper evidence and graph
inspection. This is progressive disclosure, not a usage quota or tool disablement.
Server-backed and local-journal reflection share the same report projection.

CLI reflection also projects typed semantic judgment traces from an explicitly
owner-local journal window through the shared strict decoder/projector. The
scope is `local_journal_at_read`, not server history or the requested time
horizon. Reads retain at most 512 journal records and read at most 256 KiB;
boundary records may be conservatively omitted. Truncation, malformed records,
and session/turn mismatches remain coverage gaps. Missing or unreadable journals
have unavailable counts, not known zero judgments. Even an empty existing
journal cannot establish that no judgments occurred upstream.

`local_only` CLI reflection bypasses cloud restoration entirely. CLI reflection
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
