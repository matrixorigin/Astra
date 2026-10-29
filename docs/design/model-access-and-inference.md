# Model access and inference

> Status: target design contract.
> Last updated: 2026-09-10.

## Implemented scope: generation-policy stages 1 and 2

The full route/admission design below is a target, not a claim that every type
and invariant is implemented by these stages.

- Implemented: conservative Server summary temperature resolution, endpoint-bound
  canonical defaults, propagation of fixed temperature and thinking protocol to
  main/auxiliary execution, configuration-bound persisted thinking observations,
  and a bounded introspection budget. Unknown/custom endpoints (regardless of
  their provider label), custom completion URLs and Bedrock gateways use the
  provider default unless an explicit temperature is configured.
- Implemented: numeric override validation at summary resolution and the main
  streaming/nonstreaming entrypoints. The main call's explicit temperature still
  takes precedence over a route override unless fixed_temperature is declared.
- Not implemented: removing temperature from the generic override map at model
  admission, making it runtime-owned in all routes, or replacing all request
  types with the target ResolvedGenerationPolicy below. Overrides are still
  merged and reconciled by the existing payload builder. Admission-time rejection
  and one universal generation-policy owner remain target work.
- Stage 2 adds typed maintained endpoint/model thinking contracts and administrator
  declarations. Stage-1-only statements about adding no new provider matchers do
  not describe the complete stage-2 adapter set. User Runner purpose authorization
  is unchanged; these changes do not enable Runner Work admission.

Thinking checks use a 30-second total budget, including client setup and both
legs, with at most 15 seconds per HTTP request and a 1 MiB response limit.
The separate connectivity check can add its own time; 30 seconds is not an
end-to-end model-check API deadline. EnableThinking probes use streamed SSE in
both legs to cover streaming-only deployments. Missing finish metadata is
accepted with valid output; truncation cannot prove absence of reasoning.
Connection/DNS/TLS, timeout, body and HTTP failures remain distinguishable
without exposing upstream response bodies or credentials.

Model access and inference defines how Astra presents model capability as a product, binds cloud accounts, resolves an eligible model to a trusted execution path, and records inference usage consistently across Web, CLI, Server, and Edge.

## Implemented ChatGPT subscription connection boundary

The opt-in self-hosted ChatGPT adapter uses the same Offering admission, agent
backbone, invocation/provider-attempt ledger, tool execution, cancellation,
canonical history and checkpoint paths as API-key models. `chatgpt` selects
the shared Responses codec; it does not create another session state machine.
The supported deployment and CLI/API contracts are documented in
[configuration](../reference/configuration.md#chatgpt-subscription-connections),
[CLI commands](../reference/cli-commands.md#chatgpt-subscription-models), and
[API reference](../reference/api-reference.md#personal-subscription-connections).

`user_llm_connections` owns one encrypted renewable credential per Astra owner,
issued client registration and provider subject. Multiple personal model rows
reference it. A row has exactly one source: API-key ciphertext or connection ID;
admission rejects malformed combinations. `user_llm_authorizations` owns
short-lived, owner-bound OAuth attempts, encrypted PKCE verifiers and keyed
state/nonce digests. ID tokens are verified and discarded; only renewable
credentials are stored encrypted. Credentials never enter serializable routes, catalogs,
checkpoints or public errors. The CLI relays only the browser callback and uses
a loopback listener; Server never reads local CLI/Codex credentials.

Refresh rotation uses a local gate per connection and a database compare-and-set
lease plus the rejected credential version. Discovery/JWKS metadata is cached
and prepared before taking the lease; the full rotation operation has a
25-second budget within its 30-second lease. A proven pre-dispatch connection
failure releases the lease and preserves credentials. A lost/expired refresh
outcome requires reauthentication instead of reusing a potentially consumed
refresh token. A delayed 401 for an older version reuses a committed winner. Connection generations fence reauthorization
and disconnect against old execution bindings. Disconnect clears ciphertext,
disables dependent models and cancels local requests; each replica shares one
database generation watcher per active connection, and each request performs a
fresh fence before accepting completion. Watchers stop with their last request.
Interrupted delivered requests settle as delivery-unknown through the existing
attempt owner. A disabled selected default cannot silently choose deployment
credentials. OAuth identity is never Astra identity or login-session authority.

Responses requests always send full history with `store:false` and `stream:true`.
Astra functions occupy the `astra` namespace. Completed provider output items,
including encrypted reasoning and message phase, live in private canonical
assistant-message metadata before tool execution/final checkpoint. Opaque
provider-encrypted reasoning is base64 encoded for storage so generic text
redaction cannot corrupt ciphertext. Function arguments come from the canonical
assistant tool calls; private metadata retains no second raw argument copy. Resume
replays them only for the same connection and model; other protocols remove
that metadata at the existing internal-marker projection boundary. No
`previous_response_id` thread store or parallel history projection exists.
Only completed streams authorize tool dispatch; partial/truncated streams fail.

## Ownership

Opt-in Auto chat routing uses the existing authenticated Offering admission
for its baseline and selected model. It cannot authorize a new access source,
change billing ownership, or replace a provider-bound model with a Server
credential. The currently implemented scope and selection policy are described
in [model routing](model-routing.md#deterministic-auto-selection-stage-3).

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
- A user can explicitly choose Cloud BYOK, where Astra Server encrypts the provider credential and executes inference, or This device, where the secret is never uploaded.
- Web, CLI, Server Only, and Edge + Server expose the same model and run semantics.
- Every inference has an immutable execution placement, credential owner, billing owner, policy decision and causal owner. Agent calls reuse the durable Run; standalone accounting retains durable invocation identities.
- Account, entitlement, credential, endpoint, and billing failures are visible and recoverable without corrupting run state.
- Protocol compatibility and optional generation-parameter capability remain
  separate facts; an OpenAI-compatible route is never assumed to accept every
  OpenAI request field.
- The design supports multi-tenant SaaS operation, hundreds of concurrent clients, and horizontal Server scaling.

## Non-goals

- A generic provider plugin marketplace or routing DSL.
- Ungoverned or request-scoped inference URLs on Astra Cloud Server. Personal
  BYOK configuration may register public HTTPS endpoints under the policy below.
- Separate agent loops for TaaS, direct providers, or Edge models.
- Silent fallback across billing or data boundaries.
- Treating cached balance, health, or entitlement data as authoritative without freshness.
- Preserving obsolete request shapes such as client-selected gateways or request-scoped inference URLs.
- Hostname- or model-name-specific request shaping in runtime hot paths.

## Product contract

Astra exposes three concepts to a normal user:

1. **Model Access** — where model capability comes from.
2. **Model** — what the user can select now.
3. **Run** — what was actually used, where it ran, and who paid.

TaaS bindings, endpoints, credentials, adapters, and resolved routes are implementation facts behind Model Access. They are not all independent product concepts.

The user contract is:

> Select an available model. Astra states where it runs and who pays, and it does not cross permission, data, or billing boundaries silently.

### Catalog transport contract

`GET /models` and `GET /model-access` return the same seek-paginated catalog
shape, not a first-page array. Each response contains `items`/`offerings`, a
stable `(provider, model_name, model_id)` `next_cursor`, the global `total`,
and a catalog-wide `catalog_revision`. Clients must follow the cursor until it
is absent, reject a repeated cursor or changing revision, and treat a missing
Offering after the complete drain as a definitive admission failure.

Model Access readiness and `available_model_count` are calculated from the
complete effective catalog, not from the current page. Only the first page
may carry `default_offering_id`; continuation pages carry `null` and clients
must preserve the first-page server decision.

Catalog entries expose `pricing` only when the deployment configuration has
explicit `currency=USD`, `unit=per_token`, and valid input and output rates.
An explicit zero is a rate; unannotated legacy prices, missing or malformed
rates, other currencies, personal BYOK, Genesis, and device Offerings
report `null`, not a fabricated free price. Optional cache rates stay nullable.
Administrators must explicitly reconfigure legacy prices before they can be
shown or used by future cost routing; the system never infers their currency.
`configuration_updated_at` dates the whole model configuration, not a provider
price verification or a bill. This projection uses the existing catalog query
and does not replace execution-time admission or physical-usage accounting.

Both endpoints accept `purpose=chat|typed_judgment`, defaulting to `chat`.
Only `/models` additionally accepts `purpose=all` for registry inspection;
`/model-access?purpose=all` returns `400 model_catalog_purpose_invalid`, since
Model Access describes usable inference products rather than inactive registry rows.
The shared protocol-purpose capability policy filters the complete authorized
catalog before pagination, revision, totals, defaults and access counts are
computed. Chat catalogs contain active chat-capable Offerings; TypeSafe's
typed-only protocol is not selectable there. Typed-judgment catalogs retain
TypeSafe and ordinary LLMs that implement the canonical discrete judgment
contract. Registry inspection (`all`) preserves existing visibility rules,
including inactive administrator entries; it does not grant inference authority.
Administrative CLI registry lists request `all`; CLI session judgment and judgment
comparison request `typed_judgment`. Guided setup bootstraps chat and requests
`chat`, like ordinary Web/CLI selection: a judgment-only registry must not count
as an existing chat-ready model configuration.
Continuation cursors are scoped to the requested purpose: clients must retain
the same purpose throughout a drain, as well as checking revision and total.

#### Catalog response compatibility

The executable support baseline is published CLI and Edge **0.2.12**. The two
HTTP catalog endpoints retain that release's complete response field set,
including `thinking_protocol` and `pricing`. A missing
`x-astra-model-catalog-version` header and explicit `2` select the same format;
current thin-client and TypeScript SDK requests send `2` on every page.
Unknown, repeated, or malformed versions (including `1`) return
`400 model_catalog_version_unsupported`. The response identifies its format
in the same header and is not cached.

Unversioned 0.2.10 and 0.2.12 catalog requests have no trustworthy distinction.
The Server must not infer a version from identity or strip protocol metadata
needed by the supported 0.2.12 picker. CLI 0.2.10's strict catalog decoder and
its embedded schema-v1 executor are outside the support baseline, as is
standalone Edge 0.2.10. Users of those binaries must upgrade to 0.2.12 or later.
The installed CLI can still report an unknown-field/invalid-JSON error before
execution; a new Server error code cannot repair that old diagnostic. This
policy does not claim to repair the original 0.2.10 conversation failure.

Version validation does not authorize access. The response uses the existing
service-owned authorized catalog, Offering identity, capabilities, access
policy, pagination, revision, defaults, and execution admission. Unsupported
explicit reasoning controls are rejected with protocol-specific errors; they are
never silently translated to a default, toggle, effort, or budget.

Current `ModelListItemResponse`, `ModelListPageResponse`, and
`ModelAccessProjectionResponse` readers tolerate additive metadata at those
object boundaries. This is defensive reader behavior for non-authoritative
metadata, independent of the producer's frozen response obligations; it does
not replace the historical decoder tests. After an explicitly reviewed support
baseline change establishes that every supported consumer safely ignores a
particular class of optional metadata, v2 may be extended within that class.
The corresponding frozen-consumer coverage must change with that decision;
current reader tolerance alone is insufficient. Their nested access/status,
default-resolution, cursor,
required-field, and typed enum contracts remain strict. This reader tolerance
is not permission to extend v2 responses: the published 0.2.12 decoder is
strict, and an independent frozen fixture protects its complete response
shape. New response fields, type changes, or enum/state values require a new
explicitly negotiated format while unversioned responses retain the 0.2.12
contract. Future clients may continue requesting v2 when its semantics suffice;
requesting a newer format from an older Server is an explicit compatibility
failure. A generic HTTP 400 must not trigger automatic downgrade. Any future
negotiation strategy must recognize the version-specific machine code and
verify that the selected format can represent the required semantics.
Authentication, execution requests, approvals, and Edge advertisements are not
made permissive.

Historical fixtures and binary identities do not move with the producer.
Support changes must be reviewed separately and document affected features
and migration. See [client compatibility tests](../guides/testing.md#released-cli-and-edge-compatibility)
for the fixed artifact inventory and required behavior.

Explicit Offering IDs cannot bypass this policy. Primary chat admission and
provider-boundary revalidation reject incompatible protocol-purpose pairs before
provider dispatch, using `model_purpose_unsupported` at HTTP admission. Typed
internal judgments and `judgment_model` resolution remain available independently
of chat selection; no automatic model substitution is introduced.

## Model Access sources

| Product source | Credential owner | Billing owner | Execution | Availability |
| --- | --- | --- | --- | --- |
| Astra Cloud | User-linked TaaS account | User TaaS account | Server | All clients |
| Cloud BYOK | User | User external account | Server | All clients |
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
    reason: Option<ModelAccessReason>,
    usable: bool,
    retry_after_seconds: Option<u32>,
    available_model_count: u32,
    actions: Vec<ModelAccessAction>,
    observed_at: Timestamp,
}

enum ModelAccessKind {
    AstraCloud,
    CloudByok,
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

### Cloud BYOK

Cloud BYOK is an explicit personal access source for users who want Astra
Server to remain available without a connected Edge while billing inference to
their own provider account.

- `POST /me/models` accepts a provider credential once over an authenticated
  TLS connection. Owner identity is derived from the Astra access token; the
  request cannot select another owner.
- Astra Server encrypts the credential with the configured secret backend and
  stores only ciphertext in `user_llm_models`.
- `GET /me/models`, `GET /me/models/{model_id}`, catalog, trace, error, and
  audit projections never return credential material or a reversible prefix.
- `PUT /me/models/{model_id}` rotates the credential or changes activation and
  default state. `DELETE /me/models/{model_id}` removes only the authenticated
  user's row. Cross-owner access returns `404` so resource existence is not
  disclosed.
- User configuration presents OpenAI, Anthropic, DeepSeek, and
  OpenAI-compatible. The first three use fixed official endpoints. The
  `openai-compatible` provider requires an HTTPS `base_url` and a model ID;
  public HTTPS endpoints do not require administrator registration by default.
  `ASTRA_BYOK_ENDPOINT_POLICY=trusted-domains` opts a deployment into the
  existing administrator registry; a host-only entry admits port 443 only.
  The default is `public-https`; invalid configuration fails closed.
- Custom endpoint policy is rechecked at create, credential rotation, probe,
  activation, and inference admission. Each outbound attempt pins a freshly
  validated public DNS address set and forbids redirects. DNS queries go directly
  to system-configured nameservers instead of OS synthetic-address caches;
  `ASTRA_BYOK_DNS_SERVERS` optionally selects comma-separated DNS IPs (ports
  optional); prefix an entry with `tcp://` for TCP-only DNS without a UDP
  attempt or UDP fallback. This is an explicit operator route, not automatic
  retry of rejected private/Fake-IP answers. Nameserver priority is preserved
  instead of racing answers from different DNS servers. There is no hardcoded
  public DNS or OS resolver fallback.
  Direct egress is the default. Operators may set `ASTRA_BYOK_PROXY_URL` to an
  HTTP/HTTPS CONNECT or SOCKS5/SOCKS5h proxy. A request-owned authenticated
  loopback adapter sends only validated public IP targets to that proxy,
  retaining the origin hostname for end-to-end TLS, SNI and Host. SOCKS5h does
  not delegate target DNS. Dropping the client cancels its tunnel tasks.
  Ambient proxy variables cannot override this route; proxy failure never
  falls back to direct access. Private/Fake-IP DNS answers remain rejected.
- Authenticated `POST /me/models/validate-endpoint` accepts only `base_url` and
  returns `204` after URL, policy, and DNS checks, without provider HTTP traffic,
  credentials, or stored model state. CLI runs it immediately after custom URL
  input, before asking for the key. This preflight is advisory: every later
  operation still enforces policy and every outbound attempt revalidates DNS.
  Server DNS/egress configuration failures return `502` with code
  `model_endpoint_network`; malformed or forbidden URLs remain `400`.
- `astra model add` prompts for provider, model ID, alias, context window,
  default selection and a hidden key. Explicit flags and `--api-key-stdin`
  support non-interactive configuration. Custom endpoints reuse the existing
  OpenAI transport; adding user ownership does not create another agent loop.
- Inference admission validates `(user_id, offering_id, is_active)` and
  materializes the route and credential. Server agent Runs reuse that exact
  admission for at most 30 seconds from its original admission timestamp;
  configuration-cache hits do not extend it. At the next provider boundary
  after expiry, owner-scoped revalidation refreshes or rejects the material.
  Recovery and explicit selection changes require a new admission.
  Server child fanout admits a bounded set of distinct Offerings with batched
  owner-scoped and deployment reads, preserving per-Offering freshness without
  one database round trip per child. The all-or-nothing batch is validated
  before any child is launched; inherited parent admission adds no read.
  Strict compatible-endpoint policy checks distinct child endpoints in one
  trusted-domain query; the default public-HTTPS policy uses no domain read.
- Create, credential rotation and explicit probe validate connectivity with a
  small output budget using the same provider-specific wire-field rule as
  inference: OpenAI (including o-series) and the generic OpenAI-compatible
  adapter use `max_completion_tokens`; native DeepSeek Chat Completions and
  Anthropic Messages use `max_tokens`. Sharing the Chat Completions message
  format does not imply identical optional parameters. Provider credential/model
  errors remain failures; failed checks do not persist a new or rotated credential.
- Memoria-authenticated identities can only use their own BYOK Offerings;
  deployment Offerings are excluded from both their catalog and execution.
  `ASTRA_DEPLOYMENT_MODE=cloud-byok` applies the same restriction to all
  Server-authenticated users. `self-hosted` (the default) retains deployment
  models for non-Memoria users. Admin registry management is separate from
  end-user inference eligibility. Unknown mode values do not grant access.
- Background memory selectors obey the same owner eligibility and fresh
  admission checks. A memory read/write grant never authorizes spending a
  deployment model credential. Without an explicitly eligible background model
  route, personal Cloud BYOK uses the existing deterministic/degraded path;
  it does not silently select the deployment registry or a personal default.
- Missing a personal default in Cloud BYOK requires model configuration or
  selection; it never falls back to a deployment reasoning model. The same
  owner gate applies when resuming runs and executing child runs.
- Cloud BYOK never silently imports an existing This device credential. Moving
  a credential between those access sources requires an explicit user action.

### Workspace

Workspace access is owned and paid for by an organization. Personal Cloud and Workspace remain distinguishable even when they expose the same upstream model.

Routing between them is allowed only when policy explicitly permits crossing the billing owner and data boundary. A generic fallback flag is insufficient authority.

### This device

Non-TaaS provider credentials, private endpoints, Ollama, LM Studio, and other user-local models belong to This device.

This device remains distinct from Cloud BYOK. Selecting it is the explicit
choice that keeps a personal provider credential outside Astra Server.

- Secrets remain in the device vault.
- Edge advertises a typed, leased capability and non-secret model metadata.
- Server remains authoritative for the canonical run, transcript, task, route summary, and usage projection.
- Edge is an inference executor, not a second conversation implementation.

### Self-hosted

A deployment administrator may register Server-local or organization-trusted inference endpoints. Ordinary users remain subject to the public-network Cloud BYOK policy; administrator access does not implicitly authorize their private-network endpoints.

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
The final physical provider request may report an inclusive input total without
reporting every cache billing lane or output count. That validated input total
drives context occupancy and the measured per-turn input budget; unknown billing
lanes remain unknown. Logical retry and run aggregates never replace the final
physical request's context measurement. Server stream usage carries this
measurement separately as `last_request_input_tokens` or nested
`last_request_usage.input_total_tokens`. CLI context observers forward that
validated measurement independently of complete billing lanes, including
updates that arrive while those lanes remain unknown.

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
    Degraded,
    ActionRequired,
    Unavailable,
    Disabled,
}

enum ModelAccessReason {
    Provisioning,
    NoEligibleOfferings,
    ReauthenticationRequired,
    BillingActionRequired,
    ConnectionDegraded,
    ConnectionUnavailable,
    DeviceOffline,
    PolicyDisabled,
}
```

Projection rules include:

- provisioning or no completed binding → `SettingUp`;
- reauthorization, payment, or administrator action → `ActionRequired`;
- valid binding and billing with temporary endpoint failure → `Degraded` or `Unavailable`;
- administrator policy denial → `Disabled`;
- `Ready` requires usable entitlement, credential materialization, and at least one effective Offering.

The wire projection keeps `status`, optional typed `reason`, `usable`, optional
`retry_after_seconds`, and allowed actions as separate fields. This avoids a
client-specific tagged-union encoding while preserving the same state
semantics. A source declared `Ready` with no effective Offering projects to
`ActionRequired / NoEligibleOfferings`; a non-usable source that still exposes
an effective Offering is a contract error. Error-message matching is not a
state machine. The projection revision and observation timestamp provide
freshness for the complete snapshot.

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

There is no independently writable `Model Gateway` registry. Server-owned
endpoints are governed `InferenceConnection` facts. Provider- or Edge-owned
endpoints arrive as authenticated, leased runtime capabilities and become
route inputs only after admission. They are neither durable global gateway
rows nor client-selectable routing identities. This keeps endpoint authority
with its actual owner and prevents an administrative resource that appears
configurable but has no effect on inference.

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
    generation_policy: ResolvedGenerationPolicy,
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

Every request has exactly one causal owner scope. A scope is attribution,
not an additional lease or state machine:

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

Primary agent, subagent, summary and classification calls within a Run reuse
that Run's execution authority. Work outside an active Run, including standalone
completion calls and post-turn extraction, retains its session-owned accounting
boundary. Offering admission and authenticated ownership precede provider I/O. Producers must never invent a run ID to
make auxiliary work billable, and consumers must never infer ownership from an
operation label or prompt text.

A forked skill retains its parent run/session and execution authority. Physical
requests have request-local identities; they are observations, not durable
execution grants. Tool invocation identity and custody remain durable.

Standalone accounting persists invocation and provider-attempt identity before
contacting the provider. Agent requests do not persist a second invocation,
provider-attempt lease, request-context pair, canonical transition WAL or recovery
debt. Their observations feed the existing trace and usage projection.

Retries reuse the logical invocation but create a new provider attempt. The invocation ID is sent upstream as an idempotency key only when the final provider explicitly supports that contract. If delivery may have occurred and the provider cannot answer idempotently, the result is `DeliveryUnknown`; Astra does not blindly retry or claim zero usage.

### Agent request execution boundary

The canonical transport prepares immutable provider bytes once. The existing
Run authority fences physical attempts using its shared cancellation, pause,
lease-loss and deadline controls. Main responses and auxiliary summaries use
the same transport observer; auxiliary calls do not allocate durable attempt
pairs or retry database identity collisions. Offering revalidation and provider
capacity policy remain at their existing owners.

Physical attempts retain exact wire identity, dispatch, terminal and qualified
usage observations in process. Missing usage remains unknown. Trace persistence
must not become a prerequisite for consuming a model response. A process crash
may lose in-flight model output and diagnostics; users may need to resend or
continue. There is no per-model-request replay or recovery promise. Explicit
handoff checkpoints carry their complete checked continuation without loading a
provider-attempt WAL. Run/Context commits and tool side-effect admission and
deduplication keep their independent contracts.

### Provider timeout ownership

Ordinary agent streaming calls use rolling transport-idle and semantic-progress
watchdogs, not a default whole-response wall-clock limit. Text, reasoning, and
authorized tool argument progress refresh semantic liveness; heartbeats alone
do not. The shared OpenAI-compatible, Anthropic and Bedrock collectors retain
cancellation, response-size limits, and bounded terminal draining.
Terminal draining has a fixed deadline, at most 500ms after the first terminal
signal. Empty events and duplicate terminal signals cannot extend that window.
Bedrock returns as soon as both `messageStop` and usage metadata are present;
missing metadata at the deadline remains a partial transport failure.
Anthropic completes immediately on `message_stop` and does not enter a tail wait.

The HTTP transport must not add a hidden total body timeout, including for
user-owned compatible endpoints. An explicit caller budget or positive
`ASTRA_LLM_TOTAL_BUDGET_S` override still limits the logical call across retries
and leaves its existing terminalization reserve. Without such a limit, setup
and retries are bounded until a response starts, and ledger operations have
independent bounded waits; they do not consume a progressing stream's lifetime.
Non-stream, auxiliary inference, model checks, and run/resource deadlines retain
their own limits. Unknown provider delivery is still not blindly retried.
An unset, empty, or whitespace-only total-budget variable leaves ordinary
streaming uncapped. Invalid nonempty values produce an actionable
`InvalidRequest` before streaming attempt admission, rather than selecting a
default cap.

## Control plane and data plane

```text
Client / Web / CLI
        │ offering_id
        ▼
┌──────────────── Astra control plane ────────────────┐
│ principal · effective catalog · policy · resolver   │
│ budget admission · Run execution authority         │
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

TaaS account, OAuth, billing, and credential APIs remain on the control/materialization path. Prompt, tool schema, and inference stream go only to the resolved model endpoint. If TaaS itself serves the model endpoint, it is handled by a normal provider adapter.

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

OpenAI-compatible response decoding accepts string `reasoning_content` and
`reasoning` fields in both streamed deltas and non-streaming assistant messages.
The first nonempty string in that order is used once; mirrored fields are not
concatenated. Both feed the existing reasoning result and stream event contract.
Meaningful reasoning refreshes semantic activity but is not an actionable
answer. Empty/whitespace reasoning does not refresh that deadline; idle,
provider-work, cancellation and response-size limits remain authoritative.

Primary recovery uses the same admitted thinking-control predicate as auxiliary
selection: prefer supported suppression, then supported low effort, while
preserving explicitly exact token budgets and requested thinking when capability
is unknown or preservation is required. Low effort does not prove a fast
response. An omitted or unsupported control does not prove thinking is disabled;
therefore a short suppressed-response budget cannot be inferred from the fact
that recovery was scheduled. Requested/effective controls and the actual work
slice are diagnostic facts; the ordinary client and absolute execution budgets
remain authoritative. Lifecycle reservation and exhaustion belong to
[runtime lifecycle](runtime-lifecycle.md).

### Captured reasoning replay

The canonical reasoning capability owner distinguishes required field backfill
(DeepSeek and Kimi) from exact captured replay (the GLM 4.5, 4.6, 4.7 and 5
interleaved-thinking families). Exact replay is selected by the admitted upstream model identity,
including maintained vendor namespaces, rather than an offering display alias.
Both shared wire assembly and its final send-time guard preserve every captured
reasoning string, including empty and whitespace strings, along with signatures,
tool identifiers and result order. They do not replace earlier blocks with a
placeholder or invent missing historical reasoning. Current thinking controls
and historical replay requirements are independent. The same contract applies
to streaming, non-streaming and recovery calls; append-only cache projections
retain their existing exact-prefix protection.

This preserves originals that remain available. It cannot reconstruct history
already discarded by earlier projections. Signed Anthropic and Bedrock replay
continues to require its existing protocol and model authority. The upstream
[GLM thinking contract](https://docs.z.ai/guides/capabilities/thinking-mode)
requires complete original interleaved reasoning; this is not evidence that
replay stripping caused a particular provider's latency or deadline.

## Provider request capability contract

### Problem and invariant

An inference protocol describes the transport envelope. It does not prove that
every optional field, value, or interaction accepted by one provider is
accepted by another provider implementing the same envelope. In particular,
`openai-compatible` does not by itself prove support for:

- an explicit `temperature`, including `0.0`;
- one provider's thinking-control field;
- `stream_options` or usage-in-stream behavior;
- a particular tool-choice representation;
- one completion-token-limit field;
- structured-output or system-role extensions.

The invariant is:

> Astra emits an optional provider field only when the resolved route proves
> that the field/value is supported, or when the user or administrator has
> supplied an explicit typed Offering capability. Unknown capability means
> omission, not OpenAI-default behavior.

This rule applies to primary, delegated, compaction, memory, reflection,
verification, and introspection calls. Auxiliary paths must not create their
own provider heuristics.

Model catalogs expose the resolved, non-secret thinking protocol alongside the
probed capability. Model pickers and execution admission share the exact control
support policy: a binary protocol supports `On`/`Off`, not an invented effort or
token budget. Explicit effort and budget requests are rejected when unsupported,
never silently converted to a toggle. An unsuffixed model preserves its default;
`(thinking:on)` and `(thinking:off)` retain explicit intent across surfaces.

An authenticated provider using `capability_descriptors.model_gateway` can
supply an exact gateway-wire declaration in `metadata.reasoning_control`:

```json
{"protocol":"reasoning_effort","supported_efforts":["low","high","max"]}
```

The provider must derive this declaration from the selected model's catalog,
not from the turn's requested effort. Astra accepts it only at the existing
provider-authorized gateway admission boundary. The protocol must be
`reasoning_effort`, and efforts must be a nonempty, unique subset of
`low`, `medium`, `high`, and `max`. Malformed declarations fail admission.
Omission leaves gateway reasoning capability unknown; it does not enable an
explicit effort. The admitted execution retains the exact inventory through
primary and auxiliary routes, delegation validation, and final provider calls.
An explicit adaptive effort must be a member and is emitted unchanged. The
bounded auxiliary policy may select low only if low is declared. This contract
is additive inside the existing metadata map and does not alter ordinary
catalog Offering admission or require a reasoning declaration for default chat.

The unconnected per-turn complexity dampener (`TurnComplexitySignals`,
`scale_for_turn`, and `capped_at`) is retired from the source API; requested
thinking controls, Offering admission and provider emission keep their existing
contracts.

### Resolved generation policy

Model admission resolves the call-level generation behavior before provider
payload construction:

```rust
struct ResolvedGenerationPolicy {
    thinking: ResolvedThinkingPolicy,
    temperature: TemperatureEmission,
    provenance: GenerationPolicyProvenance,
}

enum ThinkingConfig {
    ModelDefault,
    Off,
    On,
    Enabled { budget_tokens: u32 },
    Adaptive { effort: ThinkingEffort },
}

struct ResolvedThinkingPolicy {
    requested: ThinkingConfig,
    emission: ThinkingEmission,
    provenance: GenerationPolicyProvenance,
}

enum ThinkingEmission {
    // Emit no control field and allow the endpoint to select its default.
    ProviderDefault,
    // Emit the adapter's typed disable control.
    Disabled,
    // Emit the adapter's typed enable/effort/budget control.
    Enabled {
        effort: Option<ThinkingEffort>,
        budget_tokens: Option<u32>,
    },
}

enum TemperatureEmission {
    // The final payload must not contain temperature; use endpoint default.
    ProviderDefault,
    // The final payload must not contain temperature because the selected
    // thinking/provider protocol rejects it.
    Forbidden,
    Explicit(f64),
}

enum GenerationPolicyProvenance {
    OfferingCapability,
    CanonicalProviderContract,
    ConservativeProtocolDefault,
}
```

`ThinkingConfig` is the requested product behavior. `ResolvedThinkingPolicy`
is the admitted wire behavior. `ProviderDefault` is not equivalent to
`ThinkingConfig::Off`: it makes no claim that provider-side reasoning is
disabled. `Disabled` is legal only when a typed adapter capability can encode
the disable control; otherwise the resolver must choose `ProviderDefault` or
reject a request that strictly requires disabled reasoning.

`TemperatureEmission` is deliberately a final wire decision, not a guessed
model attribute. Both `ProviderDefault` and `Forbidden` assert that the field
is absent after all overrides; they differ so trace and replay can distinguish
endpoint-default behavior from a protocol prohibition. Its non-secret value
and provenance are persisted with the resolved route and provider attempt.

`ResolvedInferenceRoute.generation_policy` is the single owner. An
`InferenceRequest` carries the route and does not duplicate the policy.
Provider payload builders read only `request.route.generation_policy`.

Resolution uses the following precedence:

1. Resolve requested `ThinkingConfig` against the admitted thinking
   capability. An unclassified OpenAI-compatible route uses
   `ThinkingEmission::ProviderDefault`; it must not pretend that thinking is
   disabled.
2. A selected thinking protocol that forbids temperature resolves to
   `TemperatureEmission::Forbidden`.
3. A validated per-Offering temperature capability for the selected thinking
   mode resolves to `Explicit(value)`, `ProviderDefault`, or `Forbidden`.
4. A canonical provider/model contract may resolve bounded low-variance
   introspection to `Explicit(0.0)` only when that exact contract is maintained
   and tested by Astra.
5. An unclassified OpenAI-compatible route resolves to `ProviderDefault`.
6. All non-introspection purposes retain their existing product policy; they
   do not inherit the auxiliary classifier's low-variance preference.

Contradictory capabilities fail model admission before provider I/O. Provider
payload assembly consumes this resolved value after applying provider-specific
thinking rules.

Before resolution, a numeric legacy `request_body_overrides.temperature` is
validated, normalized into an Offering-scoped `Explicit(value)` capability,
and removed from the generic override map. An invalid value fails admission.
After that extraction, `temperature` is runtime-owned: generic overrides cannot
reintroduce it, and the final payload must exactly match
`TemperatureEmission`. This preserves the administrator's existing explicit
temperature control while removing the current behavior where an
introspection-level `0.0` silently overwrites it.

Mode-dependent models require a mode-dependent capability shape:

```rust
struct TemperatureCapabilities {
    provider_default: TemperatureConstraint,
    thinking_disabled: Option<TemperatureConstraint>,
    thinking_enabled: Option<TemperatureConstraint>,
}

enum TemperatureConstraint {
    ProviderDefault,
    Forbidden,
    Fixed(f64),
    ExplicitZeroSupported,
}
```

The existing scalar `QuirksData.fixed_temperature` is a backward-compatible
declaration for a temperature that is fixed across every admitted mode of that
Offering. It cannot describe a model whose fixed value changes with thinking
mode. Such a model uses `ProviderDefault` in the first implementation stage;
a later typed `TemperatureCapabilities` declaration may select the exact value
per mode without a new runtime heuristic.

### Persisted thinking protocol (stage 2)

OpenAI-chat probes and Server inference share `astra_core::model_wire::thinking`.
`QuirksData.thinking_protocol` is an optional administrator declaration with
`unknown`, `enable_thinking`, `thinking_object`, `reasoning_effort`, or
`moonshot` values. Absence selects the maintained canonical adapter; explicit
`unknown` opts out of the new adapter, retaining established request assembly
and explicit overrides. Endpoint authority and upstream model, not a local alias or
an arbitrary URL substring, select the Moonshot adapter. It covers the official
`/v1` endpoints and the maintained `kimi-k2.5`, `kimi-k2.6`, and
`kimi-k3` IDs. K2.5/K2.6 follow the
[official toggle documentation](https://platform.kimi.com/docs/guide/kimi-k2-6-quickstart);
K3 is covered by the optional real-provider toggle contract. Other model IDs or
gateways remain unknown unless explicitly configured. This is a maintained
adapter registry, not a Work-admission-specific model matcher.

The protocol reaches admitted execution, normal streaming/nonstreaming calls,
memory inference, and bounded summaries. Final OpenAI-chat emission removes
controls belonging to other protocols after generic body overrides when a
protocol is known. Unknown adds and removes nothing: existing thinking-off
sanitization and provider behavior remain authoritative. Anthropic Messages and Bedrock retain
their existing native envelope adapters; this stage does not redefine those
providers' probing semantics or authorize Runner introspection.

Moonshot introspection defaults sampling to the provider rather than injecting
the classifier's zero temperature. The toggle adapter does not impose sampling
limits or remove explicit temperature, top_p or penalty settings. A verified
toggle is not evidence of a fixed-temperature contract for every model version.
The mode-independent Offering `fixed_temperature` uses the same configuration
resolver in main and auxiliary requests; conflicting explicit overrides are
reported. Existing native adapter restrictions remain authoritative. Generic
OpenAI-compatible routes retain their pre-existing effort and override behavior;
Cloud BYOK does not require a protocol configuration UI to keep working.

Explicit model checks perform the probe; model creation and normal inference do
not. A supported binary protocol is tested in both enabled and disabled modes
with a bounded 1024-token request. Both responses must have completed visible
content; the enabled response must expose reasoning and the disabled response
must not. This establishes observed toggle behavior, not proof of zero internal
reasoning tokens. Truncated/malformed output, ignored controls, transport errors,
or unknown protocols cannot become a successful `both` observation. Unknown
protocols use a baseline request without guessed controls and can establish only
observed native reasoning (`native_only`), not suppression support. Declared
effort protocols send high effort within the same bounded probe budget and require observable reasoning (including
reported reasoning token usage) before reporting `effort_only`. This confirms
reasoning under the declared protocol, not a measurement of effort effectiveness.
Bounded summaries and governed completions share one capability-based preference:
supported suppression, otherwise supported low effort, otherwise the model default.
Low effort can legitimately produce zero reasoning tokens; it is not an output-budget guarantee.
BYOK probing retains the
same pinned public-endpoint transport and owner authorization as inference.

Both `infra_llm_models` and `user_llm_models` have nullable
`thinking_probe_json` observations containing adapter revision, configuration
fingerprint, protocol, capability/error, and observation time. The fingerprint
binds provider, endpoint, upstream model, encrypted credential generation and
administrator configuration. It contains no plaintext credential. Results are
published using configuration- and previous-observation-matching conditional updates, so a concurrent
rotation cannot publish a result for the replacement configuration. Resolution
reuses only matching observations, without network probing; explicit checks
refresh them. Credential/configuration changes invalidate observations. Old
rows remain usable without a reasoning-control capability. Only a snapshot bound
to the current configuration, protocol and adapter revision supplies capability
or probe diagnostics. Explicit
model checks refresh missing or stale observations without probing during resolution.
A failed/inconclusive check retains a prior capability only for the same bound
configuration and protocol while recording the latest error separately. Stale
or malformed snapshots provide no capability or diagnostics.

User model responses optionally include `thinking_probe`; a missing or stale
observation is absent, while an inconclusive check has an error. A thinking
probe failure does not disable a model whose connectivity check succeeded.
Connectivity status is also committed only to the checked row and configuration;
rotation or replacement during either probe returns a conflict requiring a fresh check.
Administrator credential updates validate the effective replacement configuration
before a short transaction commits all PATCH fields and observation invalidation together.
Concurrent row or configuration edits return 409 and require an explicit refresh
and retry; no partial update or automatic replay is performed. Empty PATCHes
preserve configuration versions and observations, and supplied quirks replace
the complete quirks object.

`ASTRA_INTROSPECTION_TOTAL_BUDGET_S` defaults to 8 seconds, capped by the global
LLM budget, for no-tool introspection provider execution. It uses the existing
shared transport budget and the existing Run cancellation/deadline authority.
Primary and other purposes retain their budgets. This is **not** an eight-second
end-to-end TTFT guarantee: Offering authorization and context assembly precede
provider execution. Existing auxiliary failure handling remains responsible for
degraded Work admission.
The provider work allowance leaves a tail reserve for durable terminalization.
That reserve is not a separate maximum for database settlement: an early provider
response leaves its unused time available for settlement until the same logical
deadline. Cancellation and the logical deadline still bound foreground delivery;
late durable success does not reauthorize a cancelled or expired caller.
Auxiliary summary calls preserve typed inference errors across the runtime
boundary, including database and contract failures. A returned failure's
observed provider usage is accounting evidence, not successful execution:
Work admission consumes that usage once before propagating the failure.
Missing usage remains unknown rather than being reconstructed from error text.
The default is a latency policy, not a provider-success guarantee: K3 samples
have exceeded the approximately 7.2-second provider work allowance. Rollout must
measure Work-admission success/degradation rate and end-to-end TTFT together;
compatibility tests with a larger budget do not establish the default-budget SLO.

### Immediate compatibility decision

The first implementation stage applies to Server-executed Cloud BYOK and
changes only bounded introspection request construction:

| Route | Introspection temperature behavior |
| --- | --- |
| Exact canonical provider/model Offering with a tested zero-temperature contract | Send `0.0` only when the resolved thinking policy permits it. |
| Offering with a validated mode-independent `fixed_temperature` | Send the configured value. |
| Offering with an explicit numeric legacy `request_body_overrides.temperature` | Normalize it to `Explicit(value)` and preserve that value unless thinking forbids temperature. |
| Generic `openai-compatible` route without an explicit capability | Use provider default: remove `temperature` from the final payload. |
| Route whose enabled thinking protocol forbids temperature | Use `Forbidden`: remove `temperature` from the final payload. |
| Primary agent and other inference purposes | Preserve their existing policy. |

The immediate Cloud BYOK fix requires no new user setting, public API field, or
database migration. Existing `openai-compatible` rows acquire the conservative
behavior on Server upgrade. The CLI continues to ask only for provider, base
URL, upstream model ID, alias, context window, default selection, and key.

For an unclassified OpenAI-compatible route, stage 1 also resolves thinking to
`ThinkingEmission::ProviderDefault`. It guarantees that Astra does not send the
known-invalid zero-temperature combination; it does **not** guarantee that
provider-side thinking is disabled or that latency decreases. A provider may
still return malformed or empty classifier output, which remains a separate
semantic failure class. The originating issue is complete only after a strict
Kimi-like contract test and a hosted Kimi end-to-end check both produce a
parsed Work-admission decision, or after Astra rejects that auxiliary purpose
before provider I/O with an explicit typed policy.

The target implementation uses one shared resolver for every execution
placement that authorizes the purpose. It must not introduce a matcher for
`api.moonshot.cn`, `kimi-*`, or any other new hostname/model string. Existing
Offering metadata such as `fixed_temperature` may feed the typed resolver, but
it must be carried through the canonical admitted route rather than read
directly inside a provider adapter.

The initial implementation follows the existing execution-material path:

```text
Offering/provider capability
  -> ResolvedActiveLlmModel
  -> AdmittedModelExecution
  -> ResolvedTurnLlmConfig
  -> OwnedLlmExecutionRoute
  -> RuntimeSummaryClient
  -> provider payload reconciliation
```

The stage-1 policy resolver is a pure function at the Server summary-call
boundary. `RuntimeSummaryClient` supplies the inference purpose and desired
thinking mode; the route supplies admitted capabilities. Its result shape is
kept independent of Server state so it can move to the shared model-call
boundary before another execution placement authorizes `Introspection`. The
provider payload builder remains the final enforcement point and removes
temperature whenever the resolved thinking protocol forbids it.

The current `openai_thinking_control(provider, base_url)` helper is a
grandfathered transition mechanism, not a second product-policy owner. During
stage 1 the Server summary resolver may consult it to select the bounded
thinking policy, while final payload assembly invokes the same helper only to
serialize an admitted `Off` policy into the maintained DeepSeek or DashScope
wire control. Contract tests must keep those two uses consistent. No Kimi,
Moonshot, new hostname, or model-name branch is added. In the target state,
the helper's facts belong to the admitted Offering/connection and payload
assembly consumes the resolved typed control without inspecting endpoint text.
The non-goal on hostname/model matching applies to this target state and
prohibits adding new runtime matchers during the transition.

User Runner authorization is a separate gate. PR #712 currently allows only
`PrimaryAgent`, `SubAgent`, and `RequiredCompaction`; it rejects
`Introspection` before provider I/O even though its proposed summary client
uses no temperature. Stage 1 therefore fixes Server Cloud BYOK only. It must
not add a vacuous Server/Runner parity test. If Runner later authorizes
`Introspection`, that authorization change requires its own design and tests
and must consume this same resolver before Work admission is enabled.

### Rejected alternatives

- **Match Kimi, Moonshot, or a hostname.** The same model may be exposed by a
  gateway or proxy, and another model can have the same constraint.
- **Treat every OpenAI-compatible endpoint as OpenAI.** This is the faulty
  assumption the contract removes.
- **Ask every Cloud BYOK user for temperature/thinking internals.** Safe default
  behavior must not require provider expertise; advanced capability declaration
  can remain an administrator or future probe concern.
- **Retry a 400 after deleting optional fields.** A generic 400 does not identify
  the incompatible field, and retry can add latency and bill twice.
- **Omit temperature for all providers and purposes.** That is safe at the wire
  level but unnecessarily changes maintained canonical-provider behavior and
  established low-variance classifier behavior where an exact contract is
  known.
- **Implement a separate Work-admission HTTP client.** It would duplicate route,
  credential, usage, error, and policy semantics instead of fixing the shared
  inference boundary.

### Auxiliary inference and latency

Work admission is a bounded introspection call. It starts concurrently with the
primary model request and settles before a plain-text completion or provider
tool batch crosses its execution boundary. Therefore its latency is not added
unconditionally to primary latency; the pre-output critical path is:

```text
request preparation
  + max(primary provider path, Work-admission provider path)
  + required reconciliation and client delivery
```

If Work admission finishes first, the primary path determines the remaining
time. If the primary response finishes first, the remaining Work-admission
deadline can delay the first executable completion boundary. Provider cache
hits, endpoint load, network variance, and scheduler delay can change the
winner between otherwise identical turns.

The compatibility fix must not add a second provider attempt after a generic
HTTP 400. Retrying after stripping fields would increase latency and may double
billing without proving which field was rejected. Astra instead sends the
conservative payload on the first attempt. An unavailable optional judge keeps
the existing typed Primary degradation; a required primary inference does not
silently switch Offering, credential owner, billing owner, or data boundary.

Latency optimization after correctness requires phase telemetry, not inference
from end-to-end TTFT. Every primary and auxiliary attempt records:

- queue/admission, provider-request-start, provider-first-byte, and completion
  timestamps;
- resolved temperature emission and provenance, without request-body or secret
  values;
- cache read/write token facts when the provider reports them;
- Work-admission reconciliation wait after the primary result;
- first durable output and first client-delivery timestamps;
- typed failure class and whether semantic Primary degradation occurred.

### Compatibility and rollout

- Historical administrator `quirks.fixed_temperature` values were previously
  persisted without affecting inference. They now become active for primary and
  auxiliary requests, take precedence over a call-level temperature, and reject
  conflicting route overrides. This can change sampling or cause a local
  configuration error after upgrade; it is not a behavior-neutral migration.
  Before rollout, audit configured values without printing credentials:

  ```sql
  SELECT model_id, model_name, JSON_EXTRACT(quirks, '$.fixed_temperature') AS fixed_temperature
  FROM infra_llm_models
  WHERE JSON_EXTRACT(quirks, '$.fixed_temperature') IS NOT NULL;
  ```

  Review non-null values and their compatibility with every enabled thinking
  mode. Change unintended values through the existing administrator model API;
  schema migration must not silently delete or rewrite them.
- Existing Cloud BYOK rows remain readable and require no backfill.
- Existing canonical provider behavior remains unchanged unless its adapter no
  longer has a tested zero-temperature contract.
- All routes without a maintained endpoint-bound zero-temperature contract or
  explicit temperature become less prescriptive, not just routes literally
  labelled `openai-compatible` (e.g. DashScope, OpenRouter and custom gateways);
  omission lets the endpoint apply its own valid default.
- A numeric `request_body_overrides.temperature` is an intentional typed
  declaration after admission. Today bounded introspection silently replaces
  that value with `0.0`; after this change the declared value remains
  authoritative unless the selected thinking protocol forbids temperature.
  This is a deliberate user-visible correction, not a no-op refactor.
- The scalar `fixed_temperature` field keeps its persisted shape and requires
  no backfill, but stage 1 treats it as mode-independent. A model whose fixed
  temperature varies with thinking mode stays on `ProviderDefault` until a
  richer typed capability is admitted.
- No provider response body is exposed merely to diagnose compatibility; raw
  non-authentication 4xx bodies remain protected by the existing secret
  reflection boundary.
- The change is rolled back by restoring the previous Server binary; no stored
  state must be reverted.
- A later richer capability probe or user-visible advanced setting is a
  separate contract change. It must not be required to make default Cloud BYOK
  safe.

Rollout observes per-purpose provider 400 rates, Work-admission availability,
TTFT, provider-first-byte latency, and reconciliation wait. A rise in malformed
judge responses is evaluated separately from transport compatibility: omitting
temperature may change sampling, but it must not be treated as a transport
failure.

## Client and SDK contract

Clients obtain one Server projection containing Model Access, effective Offerings, default selection, typed statuses/actions, and a catalog revision.

Normal chat clients submit only the user selection:

```json
{
  "message": "...",
  "model_selection": {
    "offering_id": "offer_01...",
    "thinking": "high"
  }
}
```

A locally orchestrated CLI child run may also send `expected_model_name` after
batch preflight. It is only an expected-value assertion: Server freshly admits
the selected Offering and returns `model_identity_changed` if its resolved name
drifted. The field never authorizes a route or replaces Offering admission.
Ordinary clients omit it.

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
scope is an authenticated agent run or a real session-owned operation. Memory
and compaction require session ownership; neither fabricates a run or session
identity.

Version-2 typed judgment responses carry execution-owned `judgment_provenance`
alongside text and the adapter's model identity. TypeSafe System One supplies
native Noul/Choice/Score values and distributions; ordinary model execution
supplies explicit discrete Noul/Choice/Score answers with unknown represented
directly, never as a fabricated probability. Both streaming and non-streaming
adapters preserve this distinction through summary, memory and Server completion
boundaries. Ordinary prose responses omit the field. Judgment consumers reject
missing provenance, mismatched answer formats and missing execution identity:
answer JSON and model-name strings do not select the decoder or establish native
probability capability. Raw TypeSafe response JSON is checked for duplicate
object keys before conversion into a map. The model identity is the existing
adapter's execution identity, not a guarantee that every upstream provider
exposes a resolved model version.

Choice questions declare their local `optional` answer policy explicitly.
Required answers retain strict identity, distribution and execution-provenance
validation. Missing or invalid optional observations are omitted by the shared
codec, without invented probabilities; unknown IDs, duplicate JSON and invalid
response envelopes still fail. TypeSafe requests strip this local policy and
send the provider's existing Choice schema. Its adapter retains only the local
question contract across I/O, rather than copying user evidence for validation.

These response facts do not add a ledger, query or persistence projection.
Malformed required judgments use the consumer's existing failure/baseline
behavior; they do not authorize another clarification request.

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
- Repair uses Run checkpoints and durable tool custody; an in-flight model request
  is not recoverable. Completed children are not relaunched.
- Agent Workbench shows actual model, Model Access, state, and usage for each branch.

### Execution-authority boundaries

The following boundaries are normative for every inference surface:

1. **Offering admission owns route material.** A Server agent Run may reuse
   the exact admitted Offering, route and credential for at most 30 seconds from
   the original admission. Every provider boundary checks that admission clock;
   resolving a cached configuration or reconstructing a host cannot renew it.
   After expiry, the next provider boundary performs owner-scoped revalidation:
   a disabled Offering is rejected, and credential or endpoint rotation refreshes
   the route without restarting the Run. Recovery and explicit selection changes
   require new admission. Standalone inference surfaces retain their own fresh
   request admission; neither a selection-cache hit nor an Offering ID alone
   authorizes provider execution.
2. **The existing execution owner controls inference.** Agent calls borrow the
   Run's cancellation, pause, lease-loss and original deadline. They add no
   persistent inference owner or settlement debt. Standalone accounting retains
   its cancellation-safe invocation supervisor and truthful delivery uncertainty.
   Dropped agent requests retain only observations already published; persistence
   and exact request recovery are not a second execution authority.
3. **Each API surface owns its producer identity.** Public completion proxy calls
   are session-owned auxiliary inference, not arbitrary internal run or harness
   work. Server derives their producer namespace, purpose, and durable scope from a
   typed operation. A client cannot claim an internal producer identity, attach an
   inference to a run, or pre-create an identity later needed by runtime work.
4. **Only product policy selects an Offering.** Skills declare capabilities and
   instructions; they do not pin a provider model or deployment-specific Offering.
   An agent profile may carry an opaque `ModelSelection`. A child with no explicit
   selection inherits its parent's Offering. A child with an explicit selection is
   independently admitted under the same principal, data, billing, and purpose
   policy before its run starts. Bare model-name overrides do not cross an
   execution boundary.

These rules deliberately keep caches, skill metadata, client requests, and caller
futures as consumers of lifecycle truth rather than additional lifecycle
producers.

For a live run paused before provider delivery, admission retains the original
logical plan and physical request identity. Resume does not replay the Host or
reset its deadline, retry budget, or one-shot controls. Waiting uses the existing
run control signal outside the single admission-I/O timeout; it adds no database
polling. Cancellation, execution-lease loss, and the original total deadline still
bound the wait. New guidance fences the frozen request and returns control to the
ordinary input boundary before provider delivery.

Uncertain admission must first recover an authoritative commit/settlement fact.
A paused scope permits reuse only when recovery proves that the original plan
is absent. An already-settled identity cannot be revived; it follows the existing
bounded replacement path. A pause signal alone is not proof of either outcome.

### Causal verification gates

Offline contract tests cover typed request rejection, inheritance versus explicit
child selection, single deadline ownership, and terminal-state transitions. Online
tests use MatrixOne plus a controllable mock provider and prove observable causes:

- disable an Offering between two requests and assert that the second request
  never reaches the provider;
- rotate route or credential generation and assert that the next request uses the
  new material;
- abort a non-streaming call after the provider accepts it and assert durable
  provider-attempt and logical-invocation terminals;
- retry an agent auxiliary request concurrently and assert distinct observed
  physical identities under the same Run authority, without an inference DB owner;
- reject public attempts to claim run, harness, or internal producer ownership;
- independently admit an explicitly selected child Offering, while an unselected
  child inherits the parent's Offering.

Tests must inspect typed responses and durable rows. Source-text matching, sleeps
as correctness assertions, and tests that only prove rendering or lack of panic do
not satisfy these gates.

## Failure semantics

| Condition | Required behavior |
| --- | --- |
| TaaS unavailable during signup/link | Keep Astra identity; leave access in `SettingUp`; retry through durable work. |
| OAuth or key requires user action | Project `ActionRequired` with one primary repair action. |
| Account linked but payment required | Keep binding active; billing projects `ActionRequired`; do not report auth failure. |
| Entitlement revoked | Reject new inference and return eligible alternatives. |
| Credential receives 401 | Refresh or request reauthorization for that binding; do not disable the global model identity. |
| Provider returns 429/overload | Use bounded exponential backoff with jitter within deadline and budget. |
| Optional field capability is unknown | Resolve it to the typed provider-default policy and enforce absence from the final payload; do not infer support from protocol compatibility. |
| Auxiliary provider request is rejected as incompatible | Record typed degradation and preserve the Primary path; do not retry by stripping guessed fields. |
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
- Cloud BYOK ciphertext is owner-scoped at every query and mutation boundary;
  plaintext exists only in bounded process memory while checking or executing
  the selected provider request.
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

Deployment-model selection currently uses a disposable 30-second cache, capped
at 256 entries, 4 MiB of conservative retained-allocation accounting and 128 KiB
per entry. Accounting includes keys, string/vector capacities and JSON container
allowances; it is not a whole-process RSS limit. Larger configurations remain
usable without being cached. Expired entries may supply the existing database-error
fallback until eviction; inactive, missing or unauthorized selections do not gain
that fallback. Model-name and Offering-ID namespaces remain distinct.

Sixty-four fixed process-local mutexes coalesce selection misses without retaining
per-key locks after cancellation. Hash collisions can serialize unrelated cold
fills; cache hits do not wait for a fill lock. Existing caller deadlines and
admission budgets still apply. Invalidation clears values without replacing locks
or cancelling in-flight fills, and is not a cross-Server revocation protocol.
The selection cache is not execution authority. Provider execution requires an
admitted execution; Server agent Runs reuse it only within the original
30-second admission window, then perform owner-scoped database revalidation.

Prompt-cache identity includes actual provider, upstream model, connection, trust scope, and cache protocol. Per-turn runtime feedback remains outside the stable prompt prefix.

## Observability and audit

Inference spans and durable facts include non-secret identifiers for:

- invocation, provider attempt, run, turn, and purpose;
- effective Offering, definition, connection, model, and Model Access;
- execution placement and billing owner;
- policy and relevant revisions;
- admitted, queued, first-token, and complete latency;
- resolved generation-parameter emission/provenance and post-primary
  reconciliation wait;
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
  fabricated run/session identity for memory, compaction, or reflection work.
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
- Generic OpenAI-compatible introspection omits unproven optional sampling
  fields on its first and only attempt.
- Strict mock endpoints that reject `temperature: 0.0` accept the conservative
  Work-admission payload and return a parsed structured decision.
- A Kimi-like default-thinking mock rejects `temperature: 0.0`, accepts the
  provider-default request, returns separate reasoning plus structured final
  content within the classifier token cap, and produces a parsed Work decision.
- A numeric legacy `request_body_overrides.temperature` is normalized to a
  typed `Explicit(value)` capability and is not silently replaced with `0.0`;
  invalid values fail admission, while `ProviderDefault` and `Forbidden`
  remove the field from the final payload.
- Canonical OpenAI, Anthropic, and DeepSeek adapter fixtures preserve their
  tested request behavior; primary, compaction, memory, and reflection calls do
  not inherit the introspection-only temperature rule.
- Server Cloud BYOK resolves generation policy from the admitted route without
  adding a Kimi, Moonshot, hostname, or model-name matcher.
- Until User Runner authorizes `Introspection`, it returns a typed error before
  provider I/O. If that purpose is authorized later, its contract test must
  prove that it consumes the shared resolver rather than a Runner-local policy.
- An opt-in hosted Kimi check, kept outside required CI because it needs a real
  credential, produces a parsed Work-admission decision. Failure keeps the
  originating compatibility issue open even if the strict mock passes.
- Incompatible auxiliary inference records typed Primary degradation without
  leaking provider response bodies or creating a second billed attempt.
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
- Non-TaaS personal credentials remain on Edge when the user selects This
  device; Cloud BYOK credentials are explicitly uploaded and encrypted on
  Astra Server.
- All inference purposes use one resolver and invocation contract.
- OpenAI-compatible protocol selection alone never authorizes an optional
  provider request field or value.
- Every upstream request has exact physical-attempt observations under its causal
  owner. Agent observations may be lost on process failure; standalone accounting
  retains durable provider attempts.
- No client can select an endpoint, credential, or placement directly.
- Refresh and reconnect preserve committed Run state. Process failure does not
  manufacture model delivery, usage or output that was not committed.
- Online tests validate multi-tenant isolation and actual TaaS/MatrixOne contracts.

Cache-break diagnosis consumes dispatched provider-attempt receipts with exact
request and attempt identity. Missing cache evidence remains unknown; planned
prompt snapshots and normalized zero cache-read counts cannot substitute for
physical-attempt evidence. Runtime feedback still owns section accounting,
truncation recovery and turn statistics. The unused source-only cache compression
hint and warm-cache rollout diagnostic APIs are retired; compression and rollout
policy remain at their active owners.
Cache checkpoints persist structural and usage-bearing baselines independently.
Restoration does not seed a missing usage baseline from structural-only evidence.

The source-only `record_turn` and default-source shortcuts are retired. Callers
submit explicit physical request/attempt identities to the receipt API. Planned
system, tool, and cache-marker hashes remain diagnostic metadata; they never
substitute for provider-final identities in structural break classification.
Protocol changes are attributed separately; protocol-native marker changes are
represented by the exact component identity that contains those markers.

### Typed judgment generation allowance

Typed completion callers distinguish the minimum serialized answer budget from
total generation allowance. The shared judgment request provides a bounded
8,192-token total allowance for ordinary judgments, covering reasoning and the
answer together. Larger answer envelopes retain their explicit minimum and remain
subject to the transport and model limits. Model admission
still checks the complete answer minimum, and Server dispatch caps total generation
at the admitted model limit. Non-normal finishes remain unscorable; increasing the
allowance never turns a truncated response into a judgment.
