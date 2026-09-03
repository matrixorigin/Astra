# Documentation architecture

> Status: target documentation contract.
> Last updated: 2026-07-07.

This document defines how Astra documentation is organized so design stays useful and does not collapse into implementation history.

## Principles

- Design guides implementation; implementation does not define design by accident.
- One design domain has one owning document.
- Related documents may reference an invariant, but should not restate it differently.
- Implementation status belongs in plans, PR descriptions, or release notes, not canonical design docs.
- If a historical document contains durable ideas, migrate the ideas into the owning design doc and delete the diary.

## Document classes

| Class | Location | Purpose |
| --- | --- | --- |
| Target design | `docs/design/` | Normative architecture and behavior contracts. |
| Cross-domain architecture | `docs/architecture/` | System-wide views spanning multiple design domains. |
| Guide/runbook | `docs/guides/` | Operational instructions and repair procedures. |
| Quickstart | `docs/quickstart/` | Setup and first-run flows. |
| Reference | `docs/reference/` | API, CLI, config, command, and dependency reference. |
| Testing contract | `docs/testing/` | Test strategy, matrix, and coverage expectations. |
| Product baseline | `docs/product/` | Versioned acceptance evidence and milestone decisions, not current user guidance. |
| Local active plan | `plans/` (untracked) | Time-bound or branch-bound work plan, analysis, or migration path; move durable decisions into `docs/` before sharing. |

## Design doc template

A design doc should include:

```text
Status
Last updated
Ownership
Goals
Non-goals
Principles
State/data model
Failure semantics
Security/privacy boundary when relevant
Test obligations
References to owning docs for adjacent domains
```

## Forbidden patterns

- `Final implementation status` as a design doc.
- `PR complete` or `tests passing` as architecture evidence.
- Multiple docs claiming to be source of truth for the same domain.
- Copying long code-path descriptions into design docs.
- Keeping stale plans because they once contained useful ideas.
- Using current implementation gaps to weaken the target design.

## Migration rule

When a doc is obsolete:

1. Identify durable design ideas.
2. Move each idea to the owning canonical domain.
3. Remove implementation diary, status log, and one-off validation notes.
4. Delete the old doc or convert it into a guide/reference only if it still has a distinct purpose.

## Current canonical design map

Use [README.md](README.md) for the authoritative domain map.
