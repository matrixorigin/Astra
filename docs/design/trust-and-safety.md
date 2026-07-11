# Trust and safety

> Status: target design contract.
> Last updated: 2026-07-07.

Trust and safety define how Astra keeps agent behavior auditable, bounded, recoverable, and aligned with user authority.

## Principles

- Trust is built from evidence, not model confidence alone.
- Side effects require authority and traceability.
- Claims should be attributable to context or tool evidence when possible.
- Safety boundaries should be explicit to both agent and user.
- Dangerous uncertainty should degrade or block before data loss.

## Trust surfaces

- permission decisions;
- provider decisions;
- tool result quality;
- context provenance;
- memory confidence;
- claim support;
- audit events;
- debug bundle access;
- learning consent.

## Claim support

When feasible, user-facing factual claims should be linked to:

- current transcript;
- tool evidence;
- memory item;
- artifact;
- external source;
- explicit inference.

Unsupported high-risk claims should be marked uncertain or avoided.

## Safety levels

| Level | Behavior |
| --- | --- |
| Allow | Safe and authorized. |
| Warn | Allowed with caveat or uncertainty. |
| Ask | Requires user approval. |
| Block | Policy or safety boundary prevents action. |
| Escalate | Needs admin, stronger auth, or human review. |

## Required audit

Audit facts should exist for:

- high-risk tool calls;
- permission changes;
- policy blocks;
- provider fallback;
- raw debug bundle access;
- learning data creation;
- deletion/retention actions.
