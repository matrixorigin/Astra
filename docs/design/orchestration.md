# Orchestration

> Status: target design contract.
> Last updated: 2026-09-28.

Orchestration owns multi-agent coordination, delegation, fanout/fanin, model choice per agent, and result integration. It does not own provider routing or lifecycle state machines.

## Principles

- Delegated agents share the same backbone semantics.
- Sub-agent execution must preserve parent trace and task lineage.
- Fanout should be explicit, bounded, and observable.
- Delegation failure should degrade the relevant branch, not corrupt the parent run.

## Delegation model

A delegation should record:

- parent run id;
- child run id;
- parent task id when applicable;
- child agent profile;
- model override if any;
- provider/capability constraints;
- expected result contract;
- timeout and cancellation policy.

## Fanout/fanin

Fanout creates multiple child runs or work branches. Fanin merges results through a declared aggregation step.

Ordinary `agent.spawn` creates one independent child. A parent may propose several
spawns in one model response when the admitted execution topology permits
parallel children; each call retains its own identity, admission result, and
terminal state. A failed sibling does not roll back an already accepted child.
Use `agent_fanout.start` when the parent needs a fixed group with all-slot
preflight and group-level control. Preflight prevents launching a group with
an invalid slot, but provider or child execution can still fail after launch.
Neither carrier may override an authoritative primary-only Work topology.
Provider-authored parallel calls do not cancel a user-requested durable Work
graph or its deferred activation; a conflicting proposal is rejected.
An explicit Required Work decision is a turn-wide establishment fence:
neither a one-slot nor a multi-slot fanout proposal, valid or malformed, can
start work or authorize sibling calls before the `start_work` receipt. An
already-bound WorkItem retains its own execution authority.

Required fields:

```text
fanout_id
branch_id
parent_run_id
child_run_id
objective
result_contract
status
summary
```

## Asynchronous agent messages

`agent.send_message` returns `queued` with a message ID after the routing/transport
path accepts the envelope. The sender continues without waiting for the receiver
or an application receipt. Parent, child, and peer messages use the same mailbox
consumer at safe execution boundaries, including while waiting for child results.
Semantic messages can wake the waiting parent before a child completes; transient
progress does not require another model round.
Model-authored coordination messages are limited to 3,000 characters; larger
content belongs in an artifact rather than an accepted-but-truncated message.

Communication evidence distinguishes acceptance (`Sent`) from target-runtime
observation (`Received`). Neither proves model inclusion, compliance, or task
completion. Missing receiver evidence is unknown. Correlated requests and
responses remain semantic exchanges, including permission decisions; child
completion remains owned by the child lifecycle, not a model-authored result
message. Trace and Explain project these facts without inventing an applied state.

Transport owns consumption acknowledgement, claims, and redelivery. There is no
application ACK/NACK envelope, sender retry tracker, or in-memory dead-letter
queue. Immediate routing/transport errors remain visible to the sender. Durable
transport failure records belong to the transport; `/messaging` exposes observed
metrics, without a separate application delivery/retry status.
While the runtime remains alive, a cancelled registration or turn release retains
ownership until accepted volatile messages reach the next mailbox. Durable
messages keep their original database envelope and claim authority instead of
being resent as a new message.

Acceptance is not an end-to-end delivery guarantee. In-process messages are
volatile; durable transport recovery does not establish crash-safe model adoption,
exactly-once processing, or receipt by every broadcast recipient. This contract
adds no receipt persistence, sender sweep, or status-query database I/O.

## Model selection

Per-agent model override is an orchestration decision, but it must still respect budget, policy, and trace requirements.

When a user names a delegated model in natural language, one candidate-aware
judgment resolves the request against the current authorized Chat catalog.
It must preserve the requested family, version, variant, namespace and source;
an unavailable or ambiguous model stops the child before execution. Runtime
checks exact quoted text, candidate membership, canonical IDs/names, scope,
authorization and capability, but does not run a second lexical parser over
model aliases. Semantic alias interpretation can still be wrong; its accuracy
needs live evaluation and the chosen identity remains visible in Explain.
Explicit structured selectors retain their exact-match contract. The judgment
reuses the admission catalog snapshot without a second catalog read.

The auxiliary interpretation of one user turn covers all proposed children in
one bounded judgment. Its compact output identifies exact user evidence,
eligible candidate IDs, optional task scopes, and reasoning controls. Runtime
verifies the deterministic facts and binds them to the original user text and
catalog snapshot; semantic accuracy is evaluated separately. An invalid or
uncertain judgment cannot silently choose a nearby model, relax a hard
requirement, or authorize Auto. A tool's proposed selector is matching context,
never user authority. This interpretation adds no separate catalog query or
database write.

A model-only request does not imply a reasoning level. The same judgment must
distinguish a request to *use* a reasoning control from a phrase the child is
asked to explain, compare, quote or output, and must account for negations and
later corrections. Its typed reasoning and exact source quote travel together.
Runtime checks that pairing, positive budgets, slot conflicts and provider
support; it no longer guesses semantic intent from a fixed phrase, negation or
sentence-separator list. Ambiguous intent is unresolved. A plausible but wrong
semantic judgment remains a measurable risk, not a deterministic guarantee.
The runtime never downgrades an unsupported exact effort into generic thinking.

Dynamic spawn and fanout resolve reasoning separately from the Offering.
After per-slot and shared defaults, an omitted reasoning control inherits the
effective parent setting only when the Offering identity matches. An explicit
`model_default` suppresses inheritance; a different Offering starts with its own
default. The parent snapshot carries the effective control, including per-turn
adjustments, rather than reconstructing it from a display model name.

Batch admission, prefix compatibility and child execution consume the same
resolver. Unsupported inherited controls fail admission just like unsupported
explicit controls. Fixed token budgets remain exact and must fit the output
limit; they are not translated into effort levels. This resolution is in-memory
and requires no parent-run lookup or additional persistence.
User-authored delegation requirements retain their default or hard strength in
the frozen invocation. An explicit slot choice may override a default but not a
hard requirement; applicable hard requirements are resolved before defaults,
independent of extraction order. A CLI child without a trusted task binder
rejects new nested delegation when it inherits unresolved or constrained
descendant requirements rather than silently dropping them.
`max_output_tokens` is a ceiling for the first child model round, including its
retries. CLI carries it as validated `context.max_output_tokens`; internal
delegation carries the same typed cap. Catalog output limits remain separate.
Final request assembly cannot enlarge that cap or replace an exact reasoning
control through route defaults, convergence, or settlement heuristics.

## Failure handling

- Child failure is recorded as branch failure.
- Parent may continue if aggregation policy allows partial results.
- A launched `spawn` receipt or running `get_result` response is nonterminal,
  not a failed child. It stops blocking final reconciliation only after the
  same child's successful producer-owned terminal result has been observed by
  the parent; final quality evaluation consumes the same typed proof.
  Failed or unobserved terminal results still block completion. When the
  parent has no independent work, it proposes a final answer; the existing
  completion barrier waits for direct children and resumes synthesis with
  their results, without shell sleeps, polling, or an extra database read.
- Cancellation propagates according to delegation policy.
- Missing `action` or malformed delegation calls should produce targeted diagnostics and retry guidance.

## Test obligations

- Child run lineage survives resume.
- Parent cancellation handles children deterministically.
- Partial fanin is explicit.
- Tool/provider failures inside child runs preserve provider diagnostics.
- Sending and receiving semantic messages preserves asynchronous wakeups and
  request/response correlation without automatic application reply traffic.
- Message evidence and public tool descriptions distinguish acceptance and
  runtime observation from model inclusion and child completion.
