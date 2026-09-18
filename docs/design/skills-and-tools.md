# Skills and tools

> Status: target design contract.
> Last updated: 2026-07-19.

Skills and tools define model-facing capabilities. The capability system owns routing and admission; this document defines packaging and user/product semantics.

## Skills

A skill is a packaged capability with:

- instructions;
- examples;
- resources;
- optional tools or MCP bindings;
- input/output contract;
- permission requirements;
- evaluation cases;
- version metadata.

Skill maturity is progressive: prompt-only, structured prompt, tool-backed,
resource-backed, evaluated, then governed. A package must not claim a maturity
level unless its production discovery and activation path demonstrates it.

## Tools

A tool is a callable schema. Tool visibility and execution are decided by the capability system.

Resident tools expose a small stable `tools[]` contract. `tool_search` selects
deferred invocation contracts into canonical conversation evidence; it does not
inject their schemas into later `tools[]` requests. Invoke selected tools through
the resident `invoke_tool` carrier and reuse the selection across turns while
its contract and capability remain current. Selection is knowledge, not a
permission grant: request projection and execution still enforce current provider
and policy admission. After compaction removes necessary argument knowledge,
rediscovery is legitimate; do not require or prohibit it merely by turn count.

## Relationship

A skill may require tools, but it does not make those tools available by itself. Provider decision still controls whether a required capability can run in the current session.

## Skill lifecycle

```text
draft -> validated -> published -> activated -> deprecated -> archived
```

Activation may be scoped by user, workspace, agent, or policy.

The operating workflow is:

```text
author -> validate -> evaluate -> publish -> activate -> observe -> tune -> version
```

Governed skills declare their stable identity and version, required
capabilities, allowed providers, instructions/resources, input/output and
permission contracts, evaluation cases, compatibility, and rollback policy.

## Implementation constraints

- One shared owner parses manifests, resolves discovery paths, and validates
  packages. CLI, server, Web, and providers consume that owner instead of
  maintaining local loaders or compatibility-shaped copies.
- Before adding a registry, parser, provider, or lifecycle state, identify the
  current owner and production callers. A second implementation requires a real
  deployment or authority boundary, not convenience for one caller.
- A replacement migrates callers and removes the superseded implementation and
  its self-only tests in the same change. Temporary dual paths require an owner,
  expiry condition, and convergence test.
- Exported types and parser unit tests do not prove a usable skill. Tests must
  cover discovery, capability admission, activation, required tool/resource
  availability, failure diagnostics, and the user-visible outcome.
- Persistence changes require the real schema, query, transaction, migration,
  and rollback/failure path to be exercised against the supported database.

## Compatibility

Skill updates should declare:

- instruction-only change;
- schema-compatible change;
- schema-breaking change;
- provider requirement change;
- permission change.

Provider or permission changes require stronger review.

## Discovery

Skill discovery should be progressive:

- stable small index in prompt;
- deferred loading for full instructions/resources;
- capability-aware filtering;
- deterministic ordering;
- clear diagnostics for unavailable skill dependencies.

## Observability

Track per skill version: invocation and success/failure counts, tool-call
validity, user correction rate, provider fallback/block rate, token cost, and
regression failures.

## Evidence-backed evaluation and Skillify adapter

Evaluation is a shared capability for prompts, skills, routing, provider/model
bindings, memory policies, and workflows. The first executable slices are
deliberately bounded to private prompt comparisons and owner-scoped,
instruction-only Skill comparisons whose task outputs can be checked without
mutating an external system. Skillify is the first authoring adapter:
it turns selected work evidence into a candidate revision and citations, then
hands the candidate to the shared evaluation owner. It does not own trial
identity, assessment, or report semantics.

The Skillify adapter extends the existing personal-skill version store and
Skillify harness; it does not create a second skill registry, local adoption
authority, or execution loop.

Creation and optimization consume selected conversation evidence (Context,
Trace, and Journal facts). A source range, evidence watermark, redaction
policy, and the resulting candidate content are recorded together. A draft may
be useful before it is proven, but it must say when it is based on one source,
has missing facts, or contains unresolved user corrections. Tool frequency is
observational evidence only and never grants a capability or permission.

An evaluated revision is immutable. Its experiment specification pins the
baseline and candidate content hashes, frozen cases, verifier versions, model
and provider bindings, other skills, isolation profile, repetition/order plan,
and bounded budget. Each case/arm/repetition has a stable trial identity;
retries are separate attempts and retain their own cost and uncertainty. The
no-skill arm keeps normal base capabilities, and the executor cannot read the
authoring conversation, the other arm's output, hidden answers, or production
learning state. Unsupported tool-backed or external-side-effect tasks fail
preflight instead of receiving a misleading score. The current Skill execution
adapter pins one published owner-scoped revision, recomputes its
manifest-plus-Markdown identity, and rejects tool-backed, forked, remote,
hook, or other external-side-effect surfaces at preflight.

Reports are derived from persisted trial facts and expose content, behavior,
and result differences with links to authorized evidence. Measured,
verified, assessed, inferred, and unavailable facts remain distinct. Missing
usage or incomplete traces are reported as missing; they are never converted
to zero. A report can be regenerated without rerunning a trial.

Private adoption is an explicit compare-and-set against the currently active
revision. The activation request carries `expected_active_version_id`; `null`
means that no revision is expected. The server locks only the target session,
checks the revision content hash before writing, and returns a conflict when
the expectation is stale. Repeating the same target is idempotent only when
the request names the currently active revision as its expectation. A conflict
preserves both revisions and is visible to the user. Running trials keep their
pinned revision. Rollback is another recorded adoption, and a follow-up real
invocation must prove that the adopted revision is loaded with its version/hash
identity intact.
