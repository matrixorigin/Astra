# Model access and inference

> Status: target design contract.
> Last updated: 2026-07-19.

Model access and inference defines how Astra presents model capability as a product, binds cloud accounts, resolves an eligible model to a trusted execution path, and records inference usage consistently across Web, CLI, Server, and Edge.

## Ownership

This document owns:

- the user-facing Model Access product model;
- TaaS instance and account-binding semantics;
- model catalog, Offering, connection, and credential boundaries;
- Server versus Edge inference placement;
- inference resolution, invocation, provider-attempt, and usage facts;
- billing-owner and data-boundary invariants;
- model-access failure and recovery behavior.

It does not own:

- quality/cost-based selection among eligible Offerings, owned by [model-routing.md](model-routing.md);
- session, run, cancel, pause, and resume semantics, owned by [runtime-lifecycle.md](runtime-lifecycle.md);
- generic Edge capability composition, owned by [edge-cloud-execution.md](edge-cloud-execution.md);
- general permission and sandbox semantics, owned by [safety-and-permissions.md](safety-and-permissions.md);
- durable state layering and retention, owned by [data-and-storage.md](data-and-storage.md);
- client-specific layout details, owned by the applicable client design.

## Goals

- A new Astra Cloud user can use an administrator-approved model without understanding provider wiring.
- Astra can link a user or organization to TaaS without making TaaS a special agent runtime.
- A user can use a non-TaaS provider or local model without uploading its secret to Astra Server.
- Web, CLI, Server Only, and Edge + Server expose the same model and run semantics.
- Every inference has an immutable execution placement, credential owner, billing owner, policy decision, and durable identity.
- Account, entitlement, credential, endpoint, and billing failures are visible and recoverable without corrupting run state.
- The design supports multi-tenant SaaS operation, hundreds of concurrent clients, and horizontal Server scaling.

## Non-goals

- A generic provider plugin marketplace or routing DSL.
- Arbitrary user-supplied inference URLs on Astra Cloud Server.
- Separate agent loops for TaaS, direct providers, or Edge models.
- Silent fallback across billing or data boundaries.
- Treating cached balance, health, or entitlement data as authoritative without freshness.
- Preserving obsolete request shapes such as client-selected gateways or request-scoped inference URLs.

## Product contract

Astra exposes three concepts to a normal user:

1. **Model Access** — where model capability comes from.
2. **Model** — what the user can select now.
3. **Run** — what was actually used, where it ran, and who paid.

TaaS bindings, endpoints, credentials, adapters, and resolved routes are implementation facts behind Model Access. They are not all independent product concepts.

The user contract is:

> Select an available model. Astra states where it runs and who pays, and it does not cross permission, data, or billing boundaries silently.

## Model Access sources

| Product source | Credential owner | Billing owner | Execution | Availability |
| --- | --- | --- | --- | --- |
| Astra Cloud | User-linked TaaS account | User TaaS account | Server | All clients |
| Workspace | Organization | Organization account | Server | Authorized workspace clients |
| This device | User/device | User external account or local compute | Edge | While the bound Edge is available |
| Self-hosted | Deployment administrator | Deployment administrator | Server | Self-hosted deployment |

The Server projects these sources through one client contract:

```rust
struct ModelAccessView {
    id: ModelAccessId,
    kind: ModelAccessKind,
    label: String,
    owner: AccessOwnerView,
    execution: ExecutionPlacementView,
    billing: BillingSummaryView,
    status: ModelAccessStatus,
    available_model_count: u32,
    actions: Vec<ModelAccessAction>,
    observed_at: Timestamp,
}

enum ModelAccessKind {
    AstraCloud,
    Workspace,
    ThisDevice,
    SelfHosted,
}
```

`ModelAccessView` is a projection, not a new independently writable source-of-truth table. Its fields are derived from bindings, entitlement, policy, connection health, and Edge presence.

### Astra Cloud

Astra Cloud is the default personal model-access product. It is backed by a TaaS account associated with the Astra user.

- When manual binding mode is enabled, an administrator or the user may establish the binding directly.
- In the target flow, Astra provisions a TaaS account idempotently after signup or links an existing account through OAuth.
- TaaS owns its account, entitlement, credential, and financial billing facts.
- Astra owns the agent runtime, policy intersection, run budget, transcript, tool loop, memory, orchestration, and product experience.
- TaaS entitlement supplies candidates; Astra administrators remain authoritative for which Offerings are published and visible in Astra.

The model picker must not show both `Astra Cloud` and `My TaaS` for the same personal account. TaaS appears where the account or billing relationship matters, not as a duplicate model source.

### Workspace

Workspace access is owned and paid for by an organization. Personal Cloud and Workspace remain distinguishable even when they expose the same upstream model.

Routing between them is allowed only when policy explicitly permits crossing the billing owner and data boundary. A generic fallback flag is insufficient authority.

### This device

Non-TaaS provider credentials, private endpoints, Ollama, LM Studio, and other user-local models belong to This device.

- Secrets remain in the device vault.
- Edge advertises a typed, leased capability and non-secret model metadata.
- Server remains authoritative for the canonical run, transcript, task, route summary, and usage projection.
- Edge is an inference executor, not a second conversation implementation.

### Self-hosted

A deployment administrator may register Server-local or organization-trusted inference endpoints. This does not authorize ordinary Astra Cloud users to upload arbitrary Server-side provider URLs or keys.

## Product surfaces

### Chat

The chat surface remains restrained. It shows the current model, thinking mode, placement, and only status that affects the current action.

```text
Claude Sonnet · Thinking High · Cloud
```

Provider endpoint, binding ID, secret ID, gateway ID, and capability JSON do not appear in the normal message flow.

If the selected model becomes unavailable, it remains visible with a typed reason and a primary recovery action. It must not silently disappear or be replaced.

### Model Access settings

Settings presents task-oriented source cards:

```text
Astra Cloud    Ready · personal billing               Manage billing
Workspace      8 models · managed by MatrixOrigin     View policy
This device    Xupeng's Mac · online · 3 models       Manage device
```

Authentication, reauthorization, billing action, reconnect, and diagnostics expand only when needed.

If a deployment has one trusted TaaS instance, the user never selects or enters its URL. With multiple instances, the user chooses only an administrator-published instance.

### Usage and billing

Usage is attributable by run, agent, inference purpose, Model Access, and billing owner.

```text
Primary agent       120k tokens   Personal Cloud
3 subagents          84k tokens   Personal Cloud
Compaction            8k tokens   Personal Cloud
Memory extraction     skipped     no eligible route
Reflection            3k tokens   Workspace
```

TaaS financial data includes its source and observation time and links to the authoritative TaaS billing surface. Astra estimates must not be presented as the final TaaS ledger.

### Administration

The administration experience is organized by operator task, not by exposing every database entity:

- **Access** — trusted TaaS instances, user/organization bindings, Edge policy, and accounts requiring action;
- **Models** — catalog, effective Offerings, defaults, purpose policy, and workspace visibility;
- **Usage & Budgets** — billing sources, budgets, rate limits, concurrency, and reconciliation;
- **Health & Audit** — control/model endpoint health, credential lifecycle, route decisions, and changes.

Disabling an Offering must preview the effect on new inference, active streams, long-running agents, and existing sessions.

## Product invariants

1. One inference has exactly one Model Access, billing owner, credential owner, and execution placement.
2. Those facts do not change after provider execution begins.
3. Personal Cloud and Workspace remain distinguishable choices even when they resolve to the same model family.
4. Auto or fallback cannot cross a billing or data boundary unless policy explicitly permits it.
5. One owner and TaaS instance have at most one active binding.
6. A TaaS external account cannot be linked to unrelated Astra owners.
7. Create, link, reauthorize, and unlink operations are idempotent at the storage boundary.
8. Disabling access never deletes historical routes, transcript, usage, or billing attribution.
9. All clients render Server-projected typed state; clients do not infer behavior from error text.
10. Repairing access resumes from a durable inference boundary and does not restart completed agents.

## TaaS account binding

### Trusted instances

A TaaS URL is instance-level configuration, not a user identity or credential.

```rust
struct TaasInstance {
    id: TaasInstanceId,
    base_url: TrustedServiceEndpoint,
    auth_methods: BTreeSet<TaasAuthMethod>,
    status: TaasInstanceStatus,
    revision: u64,
}
```

Only administrators register instances. Registration canonicalizes and validates the origin, rejects unsafe redirects and private/metadata targets unless explicitly part of a trusted self-hosted deployment, and records capability/version evidence.

Normal binding APIs accept `instance_id`, not an arbitrary URL. This prevents Model Access from becoming a general Server-side SSRF surface.

### Bindings

```rust
struct TaasAccountBinding {
    id: TaasAccountBindingId,
    owner: OwnershipScope,
    instance_id: TaasInstanceId,
    external_account_id: Option<String>,
    link_method: TaasLinkMethod,
    auth_ref: Option<SecretRef>,
    requested_by: PrincipalId,
    status: TaasBindingStatus,
    catalog_revision: Option<String>,
    billing_revision: Option<String>,
    revision: u64,
    created_at: Timestamp,
    linked_at: Option<Timestamp>,
    updated_at: Timestamp,
}

enum TaasLinkMethod {
    ManualKeyImport,
    OAuth,
    AutoProvisioned,
}

enum TaasBindingStatus {
    Provisioning,
    Active,
    ReauthRequired,
    FailedRetryable,
    Revoked,
}
```

`owner`, `requested_by`, and `link_method` answer different questions: who owns the account, who initiated the operation, and how authentication was established. They must not be collapsed into values such as `AdminConfigured` and `UserConfigured`.

Pending bindings may lack an external account ID or auth reference. Active bindings require a stable external account identity and a valid auth reference. Entitlement and billing remain separate facts: a correctly linked account may still require payment.

The binding relation is durable truth. A user-profile API may project a non-sensitive Model Access summary, but the profile does not duplicate the binding as an independently writable field.

### Binding transitions

```text
Provisioning ── verified ─────────▶ Active
Provisioning ── auth needed ──────▶ ReauthRequired
Provisioning ── transient error ──▶ FailedRetryable
FailedRetryable ── retry ─────────▶ Provisioning
Active ── credential invalid ─────▶ ReauthRequired
ReauthRequired ── reauthenticated ▶ Active
any nonterminal ── unlink/revoke ─▶ Revoked
```

`Revoked` is a historical terminal state. Relinking creates a new binding generation instead of reviving old credential material.

Signup and account linking are separate durable operations. Astra signup succeeds independently, then an idempotent outbox job provisions or links TaaS. A TaaS outage leaves Model Access in `SettingUp`; it does not roll back Astra identity creation.

## Product status projection

Binding, billing, connection health, and Edge presence are independent facts:

```text
Binding:    provisioning / active / reauth_required / failed_retryable / revoked
Billing:    unknown / active / action_required / suspended
Connection: unknown / healthy / degraded / unavailable
Edge:       unpaired / online / offline / stale
```

They project deterministically to:

```rust
enum ModelAccessStatus {
    SettingUp,
    Ready,
    Degraded { usable: bool, reason: TypedAccessReason },
    ActionRequired { reason: TypedAccessReason },
    Unavailable { reason: TypedAccessReason, retry_after: Option<Duration> },
    Disabled { reason: TypedAccessReason },
}
```

Projection rules include:

- provisioning or no completed binding → `SettingUp`;
- reauthorization, payment, or administrator action → `ActionRequired`;
- valid binding and billing with temporary endpoint failure → `Degraded` or `Unavailable`;
- administrator policy denial → `Disabled`;
- `Ready` requires usable entitlement, credential materialization, and at least one effective Offering.

Each status carries a typed reason, allowed actions, revision/freshness, and affected scope. Error-message matching is not a state machine.

## Model and access data model

### Model identity

`ModelSpec` describes provider-independent identity and static capability. It contains no secret, endpoint, account-specific availability, or customer price.

```rust
struct ModelSpec {
    id: ModelSpecId,
    provider_family: ProviderFamily,
    canonical_name: String,
    display_name: String,
    context_window: u32,
    max_output_tokens: Option<u32>,
    modalities: ModelModalities,
    declared_capabilities: ModelCapabilities,
    lifecycle_status: ModelLifecycleStatus,
}
```

### Inference connection

`InferenceConnection` describes a governed endpoint/protocol path and the credential category it requires. A shared TaaS connection does not embed one user's account credential.

```rust
struct InferenceConnection {
    id: ConnectionId,
    owner: OwnershipScope,
    kind: ConnectionKind,
    execution: ExecutionPlacement,
    protocol: InferenceProtocol,
    endpoint_ref: EndpointRef,
    credential_requirement: CredentialRequirement,
    data_boundary: DataBoundary,
    region: Option<String>,
    status: ConnectionStatus,
    revision: u64,
}

enum CredentialRequirement {
    TaasOwnerBinding { instance_id: TaasInstanceId },
    ServerVault(SecretRef),
    WorkloadIdentity(WorkloadIdentityRef),
    EdgeVault { edge_id: EdgeId, credential_ref: EdgeCredentialRef },
    None,
}
```

The resolver selects the exact personal, Workspace, or deployment binding for an invocation and records it in the route.

### Offering definition and effective Offering

A shared catalog definition and a user's currently selectable product are different facts.

```rust
struct ModelOfferingDefinition {
    id: OfferingDefinitionId,
    model_spec_id: ModelSpecId,
    connection_id: ConnectionId,
    upstream_model_name: String,
    audience: AudienceScope,
    display: OfferingDisplay,
    capabilities: ModelCapabilities,
    route_quirks: RouteQuirks,
    base_pricing: OfferingPricing,
    allowed_purposes: BTreeSet<InferencePurpose>,
    revision: u64,
}

struct EffectiveModelOffering {
    id: EffectiveOfferingId,
    definition_id: OfferingDefinitionId,
    access_id: ModelAccessId,
    display: OfferingDisplay,
    effective_capabilities: ModelCapabilities,
    effective_pricing: OfferingPricing,
    availability: OfferingAvailability,
    definition_revision: u64,
    access_revision: u64,
    policy_version: PolicyVersion,
}
```

Offering definitions are stored once. Effective Offerings are computed from definition, Model Access, entitlement, policy, and reachability; they need not be persisted once per user.

The client wire field remains `offering_id`, whose semantic type is `EffectiveOfferingId`. The opaque value is bound to the principal, access source, definition, and revisions and is revalidated by Server.

## Policy and eligibility

Effective Offerings are the intersection of independently authoritative facts:

```text
catalog definition
  ∩ TaaS or organization entitlement
  ∩ platform policy
  ∩ organization/workspace policy
  ∩ user policy
  ∩ session data boundary
  ∩ inference-purpose requirements
  ∩ current reachability
= effective Offerings
```

Lower scopes may narrow but cannot expand higher-scope authorization.

Administrator allowlists, entitlement, budget, credential availability, and data-region policy are real execution boundaries. They may block an inference and must return typed recovery options. Quality advice, reflection, and guardrail evidence do not become hidden retry/abort commands.

## Inference purposes

Every model call declares its purpose:

```rust
enum InferencePurpose {
    PrimaryAgent,
    SubAgent,
    RequiredCompaction,
    MemoryExtraction,
    MemoryRetrievalRerank,
    Reflection,
    Introspection,
    VerificationJudge,
    Embedding,
}
```

- The primary agent uses the user selection or an explicit Auto policy.
- Subagents inherit the parent's policy snapshot and data boundary.
- Required compaction remains within the same data boundary or reports that no valid route exists.
- Optional memory, reflection, and introspection degrade with structured evidence when no eligible route exists.
- Background purposes cannot consume personal Cloud or Device billing silently.

The usage tree exposes the actual route and cost of background and delegated inference.

## Resolved route

For each inference, Server persists an immutable route before provider execution:

```rust
struct ResolvedInferenceRoute {
    id: RouteId,
    effective_offering_id: EffectiveOfferingId,
    offering_definition_id: OfferingDefinitionId,
    offering_revision: u64,
    access_id: ModelAccessId,
    access_revision: u64,
    model_spec_id: ModelSpecId,
    connection_id: ConnectionId,
    connection_revision: u64,
    upstream_model_name: String,
    protocol: InferenceProtocol,
    execution: ExecutionPlacement,
    credential_binding: ResolvedCredentialBinding,
    credential_owner: OwnershipScope,
    billing_owner: BillingOwner,
    data_boundary: DataBoundary,
    purpose: InferencePurpose,
    policy_version: PolicyVersion,
    fallback_policy: ResolvedFallbackPolicy,
}

enum ResolvedCredentialBinding {
    TaasAccount {
        binding_id: TaasAccountBindingId,
        binding_revision: u64,
    },
    ServerSecret {
        secret_ref: SecretRef,
        secret_revision: u64,
    },
    WorkloadIdentity {
        identity_ref: WorkloadIdentityRef,
        identity_revision: u64,
    },
    Edge {
        edge_id: EdgeId,
        connection_id: ConnectionId,
        lease_epoch: u64,
    },
    None,
}
```

The route contains references and revisions, never a bearer token, API key, signed URL, or serializable secret material.

A trusted materializer creates short-lived `InvocationMaterial` in the execution process. The material is not `Debug`, not serializable, and never written to run state, journal, transcript, SSE, or ordinary logs.

## Invocation and provider attempts

A logical inference and a physical provider request are distinct:

- `InferenceInvocation` is the budget, lifecycle, and aggregate-usage unit.
- `ProviderAttempt` is one actual request, including provider request ID and delivery certainty.

Every invocation has exactly one durable owner scope:

```rust
enum InferenceOwnerScope {
    Run {
        session_id: SessionId,
        run_id: RunId,
        turn: u32,
        round: u32,
        operation_id: OperationId,
        logical_attempt: u32,
    },
    Session {
        session_id: SessionId,
        turn: u32,
        round: u32,
        operation_id: OperationId,
        logical_attempt: u32,
    },
}
```

Primary agent and subagent inference is run-owned. Work that is genuinely
outside an active run—such as pre-turn memory reranking or post-turn memory
extraction—is session-owned. Both scopes verify the authenticated user's
durable ownership before provider I/O. Producers must never invent a run ID to
make auxiliary work billable, and consumers must never infer ownership from an
operation label or prompt text.

Server persists route, admitted invocation, and first provider-attempt identity before contacting the provider.

Retries reuse the logical invocation but create a new provider attempt. The invocation ID is sent upstream as an idempotency key only when the final provider explicitly supports that contract. If delivery may have occurred and the provider cannot answer idempotently, the result is `DeliveryUnknown`; Astra does not blindly retry or claim zero usage.

## Control plane and data plane

```text
Client / Web / CLI
        │ offering_id
        ▼
┌──────────────── Astra control plane ────────────────┐
│ principal · effective catalog · policy · resolver   │
│ budget admission · durable route/invocation         │
└──────────────────────┬──────────────────────────────┘
                       │ canonical inference request
              ┌────────┴────────┐
              ▼                 ▼
      Server executor       Edge executor
      provider adapters     device secret vault
      secret materializer   local/provider adapter
              │                 │
              ▼                 ▼
        model endpoint      local/provider model

Astra Server ── account/link/entitlement/billing/credential ──▶ TaaS control API
```

TaaS account, OAuth, billing, and credential APIs remain on the control/materialization path. Prompt, tool schema, and inference stream go only to the resolved model endpoint. If TaaS itself serves the model endpoint, it is handled by a normal provider/gateway adapter.

The agent loop does not branch on whether a credential originated from TaaS, a Server vault, workload identity, or an Edge vault.

## Canonical inference contract

Server and Edge share one request and stream contract:

```rust
struct InferenceRequest {
    invocation_id: InferenceInvocationId,
    provider_attempt_id: ProviderAttemptId,
    owner: InferenceOwnerScope,
    route: ResolvedInferenceRoute,
    messages: Vec<CanonicalMessage>,
    tools: Vec<CanonicalToolSchema>,
    response_contract: ResponseContract,
    thinking: ThinkingConfig,
    output_limit: u32,
    cache: CacheIntent,
    deadline: Timestamp,
}

enum InferenceStreamEvent {
    Accepted,
    ThinkingDelta(String),
    TextDelta(String),
    ToolCallDelta(ToolCallDelta),
    Usage(Usage),
    Completed(CompletionMetadata),
    Failed(StructuredInferenceError),
}
```

Provider adapters translate the canonical contract to OpenAI, Anthropic, Bedrock, local, or other supported protocols. Adapter differences do not leak into agent state-machine behavior.

## Client and SDK contract

Clients obtain one Server projection containing Model Access, effective Offerings, default selection, typed statuses/actions, and a catalog revision.

Chat submits only the user selection:

```json
{
  "message": "...",
  "model_selection": {
    "offering_id": "offer_01...",
    "thinking": "high"
  }
}
```

Normal run requests never contain:

- base URL;
- API key or authorization header;
- connection or gateway ID;
- TaaS account/OAuth payload;
- request-scoped model-service URL;
- claimed Server/Edge placement.

The SDK exposes the same semantics to Web, CLI, and integrations:

```text
get_model_access(principal, workspace, purpose)
create_run(model_selection, data_boundary_profile, budget, idempotency_key)
submit_turn(run_id, message, optional_model_selection, idempotency_key)
stream_run_events(run_id, cursor)
get_inference_usage(run_id)
get_session_inference_usage(session_id, cursor)
```

Refresh or reconnect resumes from a durable event cursor. The SDK does not invent client-specific provider fields.

`ModelSelection`, `InferencePurpose`, and `InferenceInvocationScope` are shared
wire types, not parallel Web/CLI/Server structs. Non-streaming SDK calls use a
typed `CompletionRequest` and `CompletionResponse`; callers do not construct a
free-form JSON envelope or index into an unvalidated response. A completion
scope is either an authenticated agent run or a real session-owned operation.
It never fabricates a run ID for memory, compaction, or other session work.

This contract intentionally replaces the former `selected_model` and raw
model/provider/gateway request shapes. There is no dual interpretation or
legacy fallback: clients upgrade by selecting an `offering_id` obtained from
the current Model Access projection.

## Resolution lifecycle

Each inference boundary performs:

1. Authenticate the principal and determine organization, workspace, user, and device context.
2. Load the selected effective Offering or governed default.
3. Revalidate principal binding, definition/access revisions, policy, purpose, capabilities, data boundary, and reachability.
4. Resolve the exact connection, credential owner, billing owner, and execution placement.
5. Reserve budget and concurrency capacity.
6. Persist the route, admitted invocation, and provider-attempt identity.
7. Materialize short-lived credential data inside the selected executor.
8. Execute and stream typed events with bounded backpressure.
9. Persist provider request identity, usage, cost, and terminal state.
10. Settle the reservation and publish durable UI/SDK projection updates.

A selected Offering is not a trusted route. Client selection never bypasses Server-side resolution.

## Credential lifecycle

TaaS credentials are normalized as leases even when the underlying service returns a static API key:

```rust
struct CredentialLease {
    lease_id: CredentialLeaseId,
    secret: SecretString,
    token_type: CredentialTokenType,
    audience: CredentialAudience,
    scopes: BTreeSet<String>,
    generation: String,
    issued_at: Timestamp,
    expires_at: Option<Timestamp>,
}
```

Rules:

- validate audience and scope against the resolved connection;
- keep usable leases only in bounded, short-lived process memory;
- key cache by binding, audience, scope, and credential generation;
- use singleflight for concurrent refresh and jitter before expiry;
- invalidate on revoke or generation change;
- never persist lease secret in normal platform tables;
- do not use indefinitely stale credential when validity cannot be proven.

A reliable encrypted secret backend may store a manually imported key. Public APIs and runtime resolution still operate on first-class account bindings, not generic token-type/provider string conventions.

## Deployment semantics

### Server Only and Web Agent

Available sources are Astra Cloud, Workspace, and administrator-trusted Server-local access. Device Offerings are absent unless a real Edge is bound and online.

The existence of a browser does not imply local inference capability.

### CLI + Server

CLI is a client of the same Server catalog and submits the same Offering selection. Local inference appears only through a typed Edge capability, whether implemented by a separate Edge process or a deliberately embedded Edge runtime.

CLI configuration cannot create a parallel model-selection or billing truth.

### Edge + Server

Edge adds This device Offerings. Server remains authoritative for canonical run and invocation state; Edge owns the local secret and execution. Disconnect is a recoverable transport state, not a second session.

An extreme mode where prompt and agent state never reach Astra Server requires an Edge-owned backbone and is a separate architecture, not a `local=true` flag.

## Long-running and multi-agent behavior

- A run stores the user selection and immutable route facts for each inference.
- UI model changes affect only a later inference boundary, never an active provider request.
- Subagents may use purpose-specific Offerings only within inherited data and billing policy.
- If access expires, only branches needing another inference pause; completed transcript and task state remain readable.
- Repair resumes from durable invocation state and does not relaunch completed children.
- Agent Workbench shows actual model, Model Access, state, and usage for each branch.

## Failure semantics

| Condition | Required behavior |
| --- | --- |
| TaaS unavailable during signup/link | Keep Astra identity; leave access in `SettingUp`; retry through durable work. |
| OAuth or key requires user action | Project `ActionRequired` with one primary repair action. |
| Account linked but payment required | Keep binding active; billing projects `ActionRequired`; do not report auth failure. |
| Entitlement revoked | Reject new inference and return eligible alternatives. |
| Credential receives 401 | Refresh or request reauthorization for that binding; do not disable the global model identity. |
| Provider returns 429/overload | Use bounded exponential backoff with jitter within deadline and budget. |
| Provider delivery is uncertain | Enter `DeliveryUnknown`; reconcile when possible; do not blind retry. |
| Edge offline before start | Mark Offering offline and offer wait, choose model, or cancel. |
| Edge disconnects during stream | Preserve durable partial state and query by invocation ID after reconnect. |
| Fixed Offering unavailable | Do not silently replace it. |
| Optional memory/reflection route unavailable | Record structured degraded evidence and continue the primary task. |
| Required primary route unavailable | Block that inference with typed reason and recovery choices, not the whole session history. |
| Administrator emergency revoke | Prevent new inference immediately; handle active streams according to explicit revoke policy; audit the action. |
| Server crashes after upstream accept | Recover from durable route/attempt facts and provider identity; never invent terminal usage. |

Retry is coordinated at one layer. Server, gateway, and provider adapters cannot independently multiply retries.

## Security and privacy

- TaaS instance registration validates origin, redirects, DNS results, and network policy.
- Normal users cannot turn a binding request into arbitrary Server egress.
- Secret material never appears in profile responses, route records, transcript, journal, SSE, traces, errors, or snapshots.
- Effective Offering IDs are principal-bound and revalidated.
- Tenant/organization/user/device ownership is enforced at query and mutation boundaries.
- Cache namespaces include trust, connection, credential generation, and tenant scope where content may be sensitive.
- Control-plane TaaS requests do not include prompt, messages, tool schema, or model stream.
- Edge claims require authenticated device identity and leases; client-declared placement is not trusted.
- Revocation affects new inference immediately through revision invalidation.

## Durability, concurrency, and scale

Shared durable state contains binding revisions, Offering definitions, policy revisions, routes, invocations, provider attempts, usage, and terminal outcomes. In-process caches are disposable accelerators.

Storage-enforced invariants include:

- unique idempotency keys for binding and inference creation;
- at most one active binding per owner/instance;
- no external-account link across unrelated owners;
- CAS or transactional binding transitions;
- idempotent usage settlement;
- immutable historical route ownership.

Server instances remain horizontally replaceable:

- no permission, billing, or terminal inference truth exists only in a process `HashMap`;
- resolver caches are revision-keyed and invalidated on revoke/update;
- credential refresh uses binding-scoped singleflight;
- request queues and stream channels are bounded;
- slow TaaS accounts, providers, Edge devices, or clients are isolated by connection/tenant admission;
- no database query occurs per token or stream chunk;
- hundreds of simultaneous sessions do not serialize on one global lock.

Prompt-cache identity includes actual provider, upstream model, connection, trust scope, and cache protocol. Per-turn runtime feedback remains outside the stable prompt prefix.

## Observability and audit

Inference spans and durable facts include non-secret identifiers for:

- invocation, provider attempt, run, turn, and purpose;
- effective Offering, definition, connection, model, and Model Access;
- execution placement and billing owner;
- policy and relevant revisions;
- admitted, queued, first-token, and complete latency;
- token usage, cache status, retry count, and typed outcome.

Metrics include resolution latency/errors, active/queued inference, provider 401/429/5xx, TaaS link/billing/credential health, Edge disconnect/recovery, per-purpose usage, fallback, and optional-inference degradation.

Audit covers instance registration, binding/link/revoke, Offering publication, policy change, emergency revoke, route/fallback reason, and billing-owner changes. It references secret IDs but never secret values.

## Test obligations

Tests validate behavior, persisted facts, wire payloads, streams, and product projections. Source-text matching and assertions that a helper was called are not substitutes for behavior tests.

### Domain

- Policy intersection only narrows access.
- Personal and Workspace billing remain distinct.
- Binding, billing, and health project deterministically to product status.
- Invalid state transitions and stale revisions are rejected.
- Fixed Offerings do not silently fallback.
- Optional inference degrades without blocking required work.
- Serialized routes never contain secret material.

### MatrixOne online integration

- Binding/idempotency uniqueness under concurrent requests.
- External-account and cross-tenant isolation.
- CAS transition and revoke races.
- Route/invocation/attempt persistence before provider execution.
- Run-owned and session-owned admission, including cross-user rejection and no
  fabricated run identity for memory/compaction/reflection work.
- Idempotent usage settlement and crash recovery.
- Revision invalidation across multiple Server instances.

### TaaS online contract

- Manual binding, OAuth, and automatic provisioning where supported.
- Duplicate submit, timeout, retry, revoke, reauthorization, and unlink.
- Catalog/entitlement/billing revision and stale-data behavior.
- Credential expiry, audience/scope validation, refresh, rotation, and singleflight.
- 401, 429, timeout, and 5xx isolation to the affected binding/connection.
- Control requests contain no inference payload.
- Model streaming and usage reconcile to the correct billing owner.

### Provider and Edge end-to-end

- Canonical message/tool/thinking payload and stream translation.
- Cancellation, deadline, context overflow, and typed provider error.
- Edge secret never reaches Server storage or logs.
- Offline-before-start, mid-stream disconnect, completion-ack loss, and reconnect.
- A device Offering cannot be selected by another user.

### Product journeys

Cover Web, CLI + Server, Server Only, and Edge + Server:

- zero-configuration Cloud first use;
- manual Cloud activation mode;
- TaaS reauthorization, payment action, suspend, revoke, and recovery;
- administrator model disable and impact on existing sessions;
- Edge offline/recovery and explicit boundary-changing fallback;
- subagent, memory, reflection, and compaction route/usage display;
- page refresh and stream reconnect with complete durable history.

### Security and load

- SSRF, DNS rebinding, redirect, metadata-address, forged Offering, and forged Edge identity cases.
- Secret-negative scans across serde, Debug, trace, SSE, journal, error, and snapshots.
- 100, 500, and 1,000 concurrent sessions across multiple Server instances.
- Bounded memory with slow providers, TaaS, Edge, and clients.
- Binding-scoped singleflight and absence of global resolver lock contention.

## Acceptance criteria

- A normal user can understand available models without provider configuration knowledge.
- The UI always states execution placement and billing owner when it affects a decision.
- TaaS account handling never becomes a special branch in the agent loop.
- Non-TaaS personal credentials remain on Edge.
- All inference purposes use one resolver and invocation contract.
- Every upstream request is attributable to a durable provider attempt.
- No client can select an endpoint, credential, or placement directly.
- Refresh, reconnect, process crash, credential rotation, and Server failover preserve truthful run state.
- Online tests validate multi-tenant isolation and actual TaaS/MatrixOne contracts.
