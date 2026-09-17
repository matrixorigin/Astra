# Session product convergence: M0 baseline

This directory closes Milestone 0 of the session-product convergence plan. It
is a versioned register of repository facts and decisions, not a claim that the
later product milestones are complete.

The machine-readable source of truth is [`baseline.json`](baseline.json), with
the pinned real-repository inputs in [`quality-corpus.json`](quality-corpus.json).
Together they contain:

- independent Track A, B, and C proof ledgers;
- the canonical WorkRepository resource manifest and the old/new
  identity/mutation-authority matrices;
- the current topology call paths and their known divergence;
- the current Web/TUI information architecture and supported V1 envelope;
- the versioned quality corpus, fixed no-reflection control arm, and
  deterministic performance environment/target manifest;
- every known legacy authority/path that must be archived or deleted, with a
  named removal milestone.

## M0 verdict

M0 closure means that there is no undecided public Work identity, branch
identity, mutation owner, supported V1 journey, benchmark matrix, or
legacy-path removal owner. The contract test prevents those decisions and
their repository evidence from silently becoming incomplete or stale; the
test itself is not product proof.

It does **not** mean that a `partial` Phase gate is proven. In particular, the
current bridge-owned loop, file coordinator, local fork/publish paths, old
task/plan completion owners, and chat/session product routes are recorded as
present. Their absence is a later exit gate and cannot be inferred from this
baseline.

The quality corpus and no-reflection control protocol are fixed at M0, while
their outcome rows remain explicitly uncaptured. Those results belong to the
later quality A/B gate and must come from real runs; M0 does not manufacture a
score from design prose or a model review.

Current strict status:

| Ledger | Result at M0 |
|---|---|
| Track A — original Phase 1–7 | all seven gates are `partial`; none is closed |
| Track B — Developer Workbench | canonical Work foundation is integrated; the single-loop/cutover gates remain partial or missing |
| Track C — Reflection/Correction | fact inputs exist; durable bounded Reflection and quality A/B remain missing |

## Authority decisions

- Public product identity is `work_id`.
- A Work approach is identified by `work_id + branch_id`.
- `DatabaseWorkRepository` is the only declared-work mutation authority.
- `WorkBranch.current_graph_revision` selects the immutable declared task
  graph. A display projection is never task authority.
- `agent_runs`/invocations own execution facts; Work items do not copy their
  lifecycle state.
- `work_check_runs` and acceptance decisions own verification and delivery
  facts; model text cannot mark Work complete.
- `SessionContextCoordinator` owns the internal conversation head and fenced
  writer. Session identity is internal to a Work branch and is not the public
  Work identity.
- Policy/grant admission owns permission. Reflection can only propose a typed
  action and cannot grant itself authority.

## Updating the baseline

An entry may advance from `missing` to `partial` or `proven` only when its
evidence points at a checked-in source, test, benchmark, or externally archived
artifact. Design prose is context, not proof. `proven` requires every listed
gap to be empty and a repeatable command or artifact.

When code removes a legacy path, update its cleanup entry from `present` to
`removed` in the same commit and add an absence assertion. Do not keep an
adapter, dual reader, or fallback merely to make this manifest green.

Validate the baseline with:

```bash
cargo test -p astra-services --test session_product_m0_baseline
```
