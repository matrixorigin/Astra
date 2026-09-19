# Memory

> Status: target design contract.
> Last updated: 2026-07-07.

Memory owns durable knowledge across and within sessions. Context injection uses memory, but memory lifecycle, provenance, confidence, and deletion belong here.

## Memory classes

| Class | Meaning |
| --- | --- |
| Working memory | Current turn/session scratch state. |
| Episodic memory | Past session events and outcomes. |
| Semantic memory | Durable facts and concepts. |
| Procedural memory | Reusable procedures, preferences, and skills. |
| Project memory | Workspace/repository-specific facts. |

## Requirements

- Every memory item has provenance.
- Confidence and freshness are explicit.
- Conflicts are represented, not silently overwritten.
- User deletion propagates to derived memory.
- Memory used in a response is traceable.
- Memory injection is bounded by context budget and task relevance.

## User-requested retention

Acknowledging a fact supplied in the conversation does not require lookup or
persistence. A request to retain user facts or preferences requires a successful
memory write before the assistant claims storage. Conversation-only scope and
explicit tool bans remain authoritative; they do not grant permission to persist.
If persistence was requested but cannot be performed under those constraints,
the assistant must distinguish acknowledgment from an actual stored memory.
A write receipt proves storage, not a later-session recall that has not run.

## Loading policy

Memory loading is intent-driven:

- current task determines candidate memories;
- recent session facts outrank old memory on conflict;
- procedural memory should be loaded at point of use;
- low-confidence memory should be marked as uncertain;
- sensitive memory follows permission and redaction policy.

## Backend boundary

The design does not require one physical backend. Vector, fulltext, graph, tabular, or MCP-backed memory can coexist as long as they satisfy the same provenance, confidence, deletion, and trace contract.

## Credential authority and admission

The Server composes one per-user memory authority policy. Prompt recall, background extraction, explicit `memory` tools, HTTP memory routes and session-end governance must use that same policy.

Hosted or browser-login deployments use the application-scoped credential resolver owned by authentication. Each operation resolves the current owner binding and generation; no master-key fallback is inferred by a generic pool builder.

- Missing binding or `none`: normal disabled state. Prompt recall reports `NotAttempted` and does not contact Memoria.
- `read_only`: recall is allowed; write-oriented extraction, reflection and session-end cleanup are not admitted.
- `read_write`: read and write operations are allowed. Transport checks remain in place to catch revocation or changes after admission.

Consent denials in explicit tools and HTTP routes point to Memoria Settings → Connected apps → Astra Cloud → Memory sharing settings. They distinguish disabled sharing from read-only write denial and do not prescribe `/login` or a deployment master key as a hosted-user remedy. Missing server wiring reports a configuration problem for the administrator; local execution retains its deployment-specific configuration guidance.

Trusted self-hosted deployments may explicitly enable `MEMORIA_SELF_HOSTED_MASTER_ACCESS=1` with `MEMORIA_MASTER_KEY` and no `MEMORIA_WEB_URL`. A persisted scoped binding always wins, preserving its owner namespace and consent. Only an active local password account with no scoped binding and no retained Memoria identity may select the owner-bound master port. Disconnect, account deactivation/deletion and lookup errors fail closed, including for runtime ports created before the lifecycle change. These data requests authenticate with Memoria's owner-scoped master scheme, which validates the deployment secret but removes administrator authority before routing to memory handlers. Every request projects the authenticated Astra user as the Memoria owner; an unbound or incompatible backend fails closed. Memoria must contain the `Memoria-Owner` support introduced by `matrixorigin/Memoria#250` (not present in v0.5.1).

The background coordinator may launch a lightweight admission task, but it checks consent before loading snapshots, resolving an LLM, generating memory, or scheduling persistence. See [authentication](authentication.md) for issuer, credential replacement and retention.

MOI native UC uses the same resolver and consumers with an explicitly enabled
product-owned authority, not the local-password fallback or a second Memoria
login. Current UC status and canonical issuer mapping are checked for every
operation. Its bounded owner namespace and server-only credential are described
in [MOI native login](../guides/moi-native-login.md#built-in-memory). Automatic
prompt recall budgets both authorization and retrieval together; one successful
session-start lane remains usable if the other lane times out.

## Learning boundary

Memory is not training data by default. Learning artifacts require consent, redaction, quality gate, lineage, and deletion propagation.

A memory retention request alone does not establish a durable Work task. A turn
that prohibits tools must not create Work through automatic admission. Ordinary
conversation-only acknowledgment needs no lookup or storage caveat. The compact
resident memory surface supports remember/recall; select the full memory contract
and use its invocation carrier for operations such as forget or update.
