# Optional judgment backends

Memory relevance, lesson dismissal, request classification, and skill selection can use a
registered TypeSafe System One Offering or an ordinary LLM explicitly selected with
`judgment_model`. This selects a model for judgments, not the main agent model. Astra never
silently reuses the main agent model when this binding is absent or unavailable.
Tool permissions and execution authority still belong to the runtime.

## Configure

Register one TypeSafe connection and API key. Runtime judgments and
`astra admin model compare` reuse that Offering; no extra environment variable is
needed.

1. Register a `provider: typesafe` model using the existing model management
   flow. Set `base_url: https://api.typesafe.ai`, a pinned model such as
   `jev-1.13.0`, and the provider key in `api_key`. See the commented example in
   `.models.yaml.example`. The registry stores credentials encrypted; CLI/Edge
   receives only Offering identity. Runtime routing uses admin configuration,
   not environment variables. An empty key cannot be used or bound.

2. Add `judgment_default: true` to that model entry in `.models.yaml`.
   `astra admin model load .models.yaml --update-existing` (also used by
   `make dev-seed`) checks the model and automatically binds it by name.
   Only one entry may declare this default. Missing or false declarations
   leave the existing binding unchanged; failed checks or rejected bindings
   return an error instead of claiming the default was enabled.
3. To switch an already active model manually, run
   `astra admin config set judgment_model jev-1.13.0`.
   Inspect it with `astra admin config get judgment_model`. Exact names are
   resolved against the deployment catalog; ambiguous names are rejected.
   The server still stores only the canonical Offering ID and validates
   activity and credentials before changing the binding.
4. Start a new Session to refresh the CLI's cached memory judgment Offering.
   Server request classification and skill selection resolve the configured Offering for each
   new judgment, under the requesting user's model-access policy.

The binding can also select an ordinary LLM Offering when that latency and cost
are acceptable for the deployment. It is an Offering routing mechanism, not a
separate registry or agent lifecycle. The requesting user's existing
model-access policy is enforced; deployment credentials are not made available
to users whose policy forbids them. Provider credentials and active status are
revalidated by the completion boundary for every call. If the binding is
missing or unavailable, memory uses its deterministic fallback and optional
skill selection is skipped. Work admission records typed degradation and does
not borrow the main model; without a configured judgment Offering, an explicit
primary-model `start_work` carrier remains the no-classifier path and still
passes runtime lifecycle/effect validation.

To disable the pilot, run
`astra admin config unset judgment_model`, then start a new Session.
Existing sessions retain their cached Offering selection. Other server instances
share the admin configuration and model registry through the database.

## Request classification and Work

Request classification uses one batched `JudgmentRequest` for both TypeSafe and
ordinary LLMs. The model name and provider are Offering configuration, not
business rules. A judgment Offering must be configured explicitly; the
request's admitted main model is never borrowed as an implicit classifier.

The result distinguishes ordinary requests from explicitly requested durable
Work, requested state changes, and execution topology. Only fields that affect
the current decision must be confident and consistent. For example, an ordinary
read-only request does not fail because a Work activation or external-effect
owner question is uncertain. An unknown critical answer does not authorize
execution or become a `not_required` decision; the existing admission degradation
and primary-model recovery rules apply. If no judgment Offering is available,
the runtime records that typed degradation without starting an unbounded LLM
replacement.

Ordinary requests require no task graph generation. When durable Work is
required, the main generation model builds its task graph under the locked
classification. Graph repair cannot downgrade the lifecycle or change mutation
scope. Jev is never asked to generate free-form tasks. Both calls retain their
own Offering, durable inference identity and reported usage. Work planning
remains a separate content-generation call on the main model and is not a
judgment fallback.

Classification may overlap the primary model request. Explain shows one
`Determine request requirements` interval through classification and any required
planning, ending when the background task actually finishes. A separate
`Wait for request requirements` interval appears only when the main path waits
for the result. Delayed consumption is not added to judgment duration, and
pending work is not reported as unavailable. Cancellation closes the live span.

Semantic uncertainty is not a malformed provider response. Request admission
retains bounded question-level evidence separately from provider transport
success. A valid probability in the abstention band does not authorize an
action; the confidence thresholds and required-field dependencies remain the
same for native probabilistic and ordinary chat judgments.

An unresolved semantic classification may receive at most one clarification
on the already selected judgment Offering during a turn. This is not a main
model fallback. Clarification shares the admission deadline, cancellation and
usage owner; loading another skill does not refill its budget. It must return
a complete valid classification and preserve previously determined necessary
answers, including negative answers. Malformed responses and infrastructure
failures do not start semantic clarification. Work graph generation and its
existing repair remain separate from this classification recovery.

If classification remains unresolved, dependent parallel calls stay rejected.
Their structured diagnostic distinguishes uncertainty from a valid decision
against parallelism and identifies whether clarification was attempted. An
in-turn non-retryable rejection does not mean that a new user turn or changed
context can never be reassessed. Ordinary independently admitted primary tools
retain their existing degradation behavior. Raw prompts and provider responses
are not added to normal diagnostics.

## Skill selection and inference cost

Skill selection sends one batch containing the visible catalog and the user's
request. It selects a workflow only when exactly one candidate is confidently
appropriate and its competitors are confidently excluded. Ambiguous, conflicting,
truncated or unavailable judgments leave selection to the main assistant; there
is no format-repair call or highest-score shortcut.

Judgment output allowances scale with the question IDs in the batch, rather than
the main model's content-generation allowance or evidence length. Introspection
uses the same existing execution deadline for Jev and ordinary LLMs. Free-form
Work planning and memory extraction remain content-generation operations.

Explain groups auxiliary input, output and cache tokens by actual provider,
Offering, model and operation: request classification, skill selection and Work
planning appear separately even when they use the same model. Jev appears as
`Jev`; ordinary LLM calls retain their provider name. Trace model-request events
carry the same actual identities and operation. Explain keeps unreported Jev
cache counts unknown. Trace does not derive a cache hit rate from the normalized
accounting buckets' cache zeros, which are not provider-reported cache evidence.
Session `reflect` also includes a bounded, owner-scoped summary of physical
judgment attempts by provider, offering, model and operation. It reports attempt count
and exact-usage coverage; incomplete counters are lower bounds, not zero
consumption. This view spans the session, while Explain's auxiliary section
is scoped to a selected turn.
Request classification and Work planning contribute once to the runtime run
total, but never to a primary request's context-size estimate or per-request
cache hit rate. This total is marked `runtime_accounted_usage`, without a
single model cache ratio. It is not an all-provider billing total: other
auxiliary callers have their own accounting boundaries. Use the canonical
physical-request ledger and Explain auxiliary groups for provider/operation
detail, including skill selection and memory judgments.

## Behavior and limits

The existing `/models/memory` catalog defaults to extraction candidates.
`?operation=judgment` selects the optional judgment binding; extraction retains
its original selector chain and excludes TypeSafe. No new execution endpoint is
added. The existing authenticated completion proxy and durable inference ledger
own provider execution, request hashes, deadlines, attempt settlement, and usage.

Requests carry the version-2 structured judgment contract: shared JSON evidence
and keyed questions. Memory code constructs its Noul relevance/dismissal
questions; the TypeSafe adapter only encodes the provider protocol and preserves
native probabilities and model identity, with usage in the existing completion
envelope. Ordinary LLMs return an explicit typed `answers` map. Their discrete
yes/no/unknown decisions never become probabilities; memory relevance retains
unknown candidates after clear matches, while dismissal excludes unknowns.
Memory, request classification, skill selection, and the comparison command
share the formatter and strict response normalization in `astra-turn-types`.
Question IDs are exact JSON strings, including numeric-looking keys such as
`"0"`; duplicate keys, missing/extra IDs, and wrong answer types are rejected.
Dismissal questions also state their positive and negative criteria explicitly:
invalidating a lesson is different from postponing an action, changing tasks, or
making a current-task exception.
Memory evidence is keyed by the exact question IDs; questions reference those
keys instead of requiring the model to count positions in an unlabelled array.
This preserves identity when a batch contains multi-digit IDs.
Relevance judges each memory's contribution, not whether that one memory can
answer the entire task. A requested fact or currently applicable instruction
is sufficient; another scope, an overridden preference or an explicitly
excluded detail is not. Positive and negative criteria are shared by both
backends. The decision remains strictly greater than 0.5; this does not make
the returned probability a relevance magnitude or relax uncertainty handling.
Typed completion-proxy operations accept that canonical two-role message
envelope and derive the minimum output budget from the question IDs before
provider admission. The caller's `max_tokens` and the selected Offering's
catalog cap must both fit the complete answer; free-form memory extraction is
the only completion operation outside this typed contract. The judgment cap is
derived from the largest serialized answer shape and is only an admission and
generation limit; it is not a tokenizer measurement, reported usage, or billed
token count. Actual usage remains sourced from provider-reported usage.
Unknown or repeated IDs are invalid, not silently discarded. Native provider
probabilities remain probabilities; discrete LLM decisions are never displayed
as calibrated confidence. Uncertain memory decisions do not select or dismiss
candidates. Business owners retain their thresholds and fallback behavior.
The shared version-2 primitive also supports native and discrete Choice/Score
answers for other callers; the memory selector deliberately asks only Noul
questions and does not interpret Choice/Score distributions.

One Jev Offering contains the connection and encrypted key. Future operations
reuse that Offering, rather than creating scenario-specific Jev credentials.
Typed nonstream judgments can retain the canonical memory-retrieval,
introspection or verification-judge purpose. This enables new business callers
without modifying the provider adapter or mislabeling their purpose.

Current message/candidate truncation budgets are preserved. Provider errors,
timeouts or malformed answers retain the existing lexical relevance fallback
and no-dismissal fallback. The latter does not delete persistent memory: this
path removes injected session lessons only. No extra LLM retry is added.

TypeSafe streaming, tools, ordinary chat, extraction, reasoning and arbitrary
wire overrides are rejected before dispatch. Shared memory/reasoning defaults
exclude it, and chat default projection will not select it.

## Compare without changing deployment configuration

Use your existing Astra login and registered Offerings. Obtain the IDs with
`astra admin model list`, then run:

```sh
astra admin model compare <baseline-offering-id> <jev-offering-id>
```

This repeats the twelve built-in memory cases three times through the existing
authenticated Server completion endpoint and ledger. It creates a separate
comparison Session and never changes your active Session or deployment binding.
Backend order alternates by repetition. Reports go to a new private directory
under `/tmp`; the command prints its location and a readable summary.
Unavailable calls and malformed answers remain distinct from label errors.

For another replacement, provide a JSON case file:

```sh
astra admin model compare <baseline-offering-id> <jev-offering-id> \
  --cases examples/judgment-compare.json --repeat 3
```

Each case defines its operation, structured evidence/questions, optional expected
question IDs, and a probability threshold. All task criteria belong in the shared
questions/state. The LLM system message only specifies output formatting, so both
Offerings evaluate the same input. The example uses `verification_judge`; other
supported completion operations include `turn_intent` and `skill_auto_route`.
Comparison requests do not change the deployment's judgment binding.

`manifest.json` records the binary hash/version, normalized fixture hash, exact
Offerings, shared instructions and deadline. `results.jsonl` is flushed after each
call and retains raw probabilities, usage, actual typed-response model, input
hash, ordering coordinates, latency and failures. `summary.json` reports valid
and unavailable calls, label agreement and latency across all calls, including
failed and malformed responses. Private case contents stay
in the local report; users choose any custom evidence sent to their configured
Offerings. Costs are not guessed: the public catalog has no price fields, so the
runner reports actual token usage for comparison against Offering prices.

Thresholds can be compared against saved Jev probabilities without making new
paid calls. Repeated synthetic cases are not independent production coverage;
the concise-preference relevance labels are subjective. Compare task quality
and unhappy paths on a representative corpus before enabling a new replacement.

## See Jev usage in Explain

Enable `/explain on` before sending a request. The finished report separates
main-model tokens from auxiliary judgments, for example:

```text
Auxiliary tokens · Jev (jev1.13.0) · Memory judgment · in 420 · cache read 0 · cache write 0 · out 24 · 1/1 requests reported
```

This is an illustrative layout, not measured usage. TUI, text/HTML artifacts and
Web use the same physical-attempt facts. Missing provider counters show
`unknown`; partial reports remain labeled partial. Retries are separate physical
attempts, while replayed snapshots do not count them twice. A capture failure
shows `capture unavailable` and preserves the normal answer. This section does
not invent auxiliary timings or claim that Jev reduces main-agent rounds.

## Failure behavior

| Condition | Expected behavior |
| --- | --- |
| Empty Jev key | Cannot bind/use the Jev Offering; no provider dispatch |
| Unauthorized, rate limited, unavailable or timed out | Relevance uses the existing lexical fallback; no dismissal is inferred |
| Invalid answers, probabilities or question identities | Reject the judgment and use the same conservative business fallback |
| Valid answers with missing/invalid token metadata | Keep the judgment; missing counters remain unknown |
| Explain usage capture fails or exceeds its budget | Preserve the answer; report auxiliary capture unavailable |
| A comparison call fails or returns invalid answers | Keep every observation, label the failure and return an unsuccessful comparison |
| Comparison output directory already exists | Refuse overwrite and preserve the existing report |

These describe the contract and its deterministic regression coverage, not proof
that every live-provider failure has been reproduced. Optional auxiliary
inference failure does not become a primary task `ExecutionIncomplete` condition.

## Validate

Normal offline and online tests use local mock providers and fake keys. They do
not load `JEV_KEY`, connect to Jev/DeepSeek, or run `admin model compare` against
real Offerings. Real-provider evaluation is an explicit harness action, separate
from these tests; the commands above perform that action and may incur charges.

```sh
cargo test -p astra-runtime --lib turn::llm::typesafe::tests
cargo test -p astra-runtime --lib memory_hooks::relevance::tests
cargo test -p astra-runtime --lib memory_catalog_query_tests
cargo test -p astra-cli --lib admin_cli::judgment_compare::tests
```

Only claim lower rounds if an existing call is eliminated: replacing one selector
LLM call with one Jev request reduces general-purpose LLM calls, not total model
requests or main-agent rounds. Existing selectors already batch all candidates.
