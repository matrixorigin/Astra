# Astra Agent Binding Registry

> Status: draft
>
> Scope: `astra-server` Agent Binding registry, immutable binding storage, binding-backed chat execution, integration-level model gateways, runtime capability discovery, and migration from request-scoped MCP runtime.
>
> Source alignment: generic Agent Binding contract from the pluggable capability architecture. This design intentionally avoids client-specific business concepts.

## Summary

Astra needs a provider-side runtime snapshot that any client can register once and use many times.

The target runtime contract is:

```text
client creates immutable Agent Binding
  -> Astra stores agent_md, binding-local MCP/skill capability servers, runtime_policy, and opaque metadata
  -> client starts a turn with selected_model.model, optional selected_model.gateway, optional runtime_auth, and optional agent_binding
  -> Astra resolves selected_model.model against its own model config when selected_model.gateway is absent
  -> Astra resolves selected_model.gateway through model_gateways when gateway is present
  -> if agent_binding is present, Astra loads the binding and discovers MCP tools / skills through selected binding-local capability servers
  -> Astra freezes the model descriptor, discovered tools, and discovered skills into this loop's context manifest
  -> Astra runs the agent loop using only that per-turn model descriptor and per-turn discovered capability surface
```

Agent Binding and model gateway are separate resources:

- Agent Binding stores static Agent prompt and Agent-scoped MCP/skill capability server refs.
- Model gateway stores optional runtime model resolve integration endpoints.
- Runtime request selects model by `selected_model.model`.
- Runtime request may select a model gateway by `selected_model.gateway`.
- Runtime request supplies one opaque authorization bearer through `runtime_auth.authorization` only when the selected path needs registered capability endpoints or a model gateway.

This is a generic Astra service protocol. Astra must not care which system uses it. Astra must treat binding metadata, binding-local refs, selected model values, and runtime bearer values as opaque protocol data. Astra must not parse client-specific workspace, user, agent, manifest, model authorization, or allowed-capability claims from the bearer.

The existing `runtime_mcp_bindings` path remains a request-scoped transition mode. It is not an implicit substitute for Agent Binding mode.

## Goals

- Add a generic Agent Binding registry to Astra.
- Store immutable binding snapshots with stable Astra-generated ids.
- Add an integration-level `model_gateways` registry outside Agent Binding storage.
- Let every `/chat/stream` path use the same `selected_model` object shape with required `model` and optional `gateway`.
- When `/chat/stream` includes `agent_binding`, make this loop's visible MCP tools and skills come only from discovery against the binding-registered MCP and skill endpoints selected by the request.
- Resolve model invocation per turn through Astra's own model config when `selected_model.gateway` is absent, or through `model_gateways` when it is present.
- Discover tool and skill schemas per turn through selected binding-local MCP/skill capability servers.
- Build an explicit per-loop context manifest before the first LLM call, containing the request-selected model descriptor and, when `agent_binding` is present, discovered tools and skills.
- Reuse existing MCP schema conversion and tool-call routing concepts where possible, but use credential-neutral HTTP transport pooling so bearer-bearing state stays request-local.
- Preserve runtime bearer secrecy: bearer values stay in memory for the turn and never enter DB, logs, SSE events, session state, checkpoints, or durable run attribution.
- Fail explicitly when selected model validation, binding lookup, selected refs, runtime bearer validation, model gateway lookup, model resolve, discovery, schema conversion, or runtime policy validation fails.
- Keep client-owned permissions, tool dispatch authorization, model authorization, credential resolution, quota, routing, and business semantics outside Astra.

## Non-Goals

- Do not implement Agent package load/export in Astra.
- Do not store concrete external tool records in Astra.
- Do not store per-user allowed tool lists, concrete skill lists, concrete model lists, or discovery results in Agent Binding.
- Do not store model gateway refs, default models, model allow-lists, model provider URLs, provider API keys, routing policy, quota, health state, or concrete provider model mappings in Agent Binding.
- Do not make Astra call downstream client-managed databases, HTTP tools, external MCP servers, or model providers directly. Astra calls only registered capability server endpoints and model invocation descriptors returned for the current turn.
- Do not silently convert failed Agent Binding execution into plain chat or request-scoped MCP execution.
- Do not silently choose another model, gateway, capability server, or credential when a selected resource fails.
- Do not mutate an active binding. A changed prompt, capability server registry, or runtime policy creates a new binding.
- Do not expose client-owned prompt/server/policy/content hashes as first-class Agent Binding API fields. Client-owned hashes belong in `metadata`.
- Do not change Astra native web/TUI/default chat behavior unless that client explicitly opts into Agent Binding registry mode.

## Existing Baseline

Current Astra already has useful pieces:

- `ChatRequest.runtime_mcp_bindings` carries request-scoped MCP endpoint refs and credentials.
- `runtime_mcp::prepare_request_scoped_runtime_bundle` validates endpoints, connects MCP servers, calls discovery, converts schemas, and fails before the loop on discovery errors.
- `ServerToolExecutor` forwards `mcp__*` calls through an in-memory MCP manager.
- `astra_turn_core::tool_surface` resolves server-visible built-in tools from static metadata and active runtime capabilities.
- `RequestConstraints.allowed_tools` can restrict a single request.

Missing pieces:

- No `agent_binding` / `runtime_auth` / strict object `selected_model` wire shape.
- No Agent Binding registry API.
- No immutable binding table.
- No explicit `idempotency_key` handling for binding registration.
- No binding-local MCP/skill capability server ref resolver.
- No integration-level model gateway registry.
- No model gateway resolve path.
- No skill capability server discovery path.
- No binding-backed prompt/policy assembly.
- No run attribution fields for binding id, selected refs, selected model, and selected gateway.
- No explicit runtime profile switch between `request_scoped_runtime_mcp` and `agent_binding_registry`.

## Conceptual Model

### Agent Binding

An Agent Binding is an immutable Astra-owned execution snapshot created by a client.

It contains static runtime inputs:

- `agent_md`: complete immutable static Agent prompt supplied by the client.
- `capability_servers`: binding-local MCP/skill capability server registry.
- `runtime_policy`: Astra agent-loop policy.
- `metadata`: opaque client-owned diagnostics and provenance.
- `binding_schema_version`: payload schema version.

It does not contain:

- Runtime bearer values.
- Per-user authorization results.
- Concrete tool/skill/model authorization lists.
- Runtime-discovered tool, skill, or model schemas.
- Model gateway refs.
- Model defaults or model allow-lists.
- Model provider config or downstream model credentials.
- Client-specific resource ids that Astra must parse for authorization.

### Capability Server

A capability server is a binding-local ref that tells Astra where to discover or invoke one Agent-scoped capability class.

V1 capability server shape:

```json
{
  "id": "tools",
  "type": "mcp",
  "transport": "streamable_http",
  "endpoint_url": "https://capabilities.example.com/api/v1/mcp/http"
}
```

Fields:

| Field | Meaning |
| --- | --- |
| `id` | Opaque binding-local id. Runtime requests refer to this id. It is not a URL, tool name, workspace id, or agent id. |
| `type` | Capability protocol class. V1 Agent Binding supports `mcp` and `skill`. Model gateways are not Agent Binding capability servers. |
| `transport` | Wire protocol from Astra to the capability server. V1 supports `streamable_http`. |
| `endpoint_url` | Absolute `http` or `https` endpoint Astra calls for discovery and invocation. It must not contain userinfo or credential-bearing query parameters. |

Validation rules:

- Values are exact strings. Astra must not trim, case-fold, alias, infer, normalize, or fill defaults.
- `id` must be non-empty, unique inside the binding, and must not contain path separators or leading/trailing whitespace.
- `type` must be one of `mcp` or `skill` in v1.
- `transport` must be `streamable_http` in v1.
- `endpoint_url` must be an absolute `http` or `https` URL.
- `endpoint_url` must not contain userinfo.
- V1 should reject query strings rather than trying to classify safe vs unsafe query parameters.
- The capability server object must not contain headers, authorization tokens, cookies, API keys, passwords, credential refs, bearer values, or secret-like fields.
- The binding must contain at least one `type=mcp` server and at least one `type=skill` server in v1 Agent Binding mode.

### Runtime Auth

Runtime auth is one per-turn opaque authorization bearer:

```json
{
  "authorization": "Bearer <opaque-runtime-bearer>"
}
```

Rules:

- `runtime_auth.authorization` is required when `agent_binding` is present.
- `runtime_auth.authorization` is required when `selected_model.gateway` is present.
- `runtime_auth.authorization` is not required when the request has no `agent_binding` and no `selected_model.gateway`.
- It must contain exactly one `Bearer <token>` value.
- Missing value, empty token, leading/trailing whitespace around the token, non-Bearer schemes, or multiple credentials are invalid.
- Astra forwards the value unchanged when calling selected MCP/skill capability servers, selected model gateway resolve endpoint, and returned model invocation endpoint.
- Astra must not parse bearer claims for authorization, model selection, client identity, manifest scope, or allowed-capability scope.
- Runtime auth is never stored in Agent Binding, model gateway rows, run attribution, logs, SSE, or checkpoints.

### Runtime Capability Server Refs

The runtime request selects binding-local server ids only when an Agent Binding is present:

```json
{
  "agent_binding": {
    "id": "ab_018f05f5-c7dd-7f43-83e6-93d56d9d7391",
    "capability_server_refs": {
      "mcp": "tools",
      "skills": "skills"
    }
  }
}
```

The request must not resend capability server definitions. It only selects ids that already exist in the stored binding.

For Agent Binding registry v1:

- `agent_binding` is optional. Plain chat omits it.
- When present, `agent_binding.id` must load an active binding.
- `capability_server_refs.mcp` must resolve to a binding server with `type=mcp`.
- `capability_server_refs.skills` must resolve to a binding server with `type=skill`.
- `capability_server_refs` must not contain `models`; model gateway selection comes from `selected_model.gateway`.
- Missing refs, duplicate server ids, type mismatches, unsupported transports, missing runtime auth, or endpoint validation failures fail before the loop starts.

### Selected Model

`selected_model` is required for every `/chat/stream` turn. It has one required field and one optional field:

```json
{
  "selected_model": {
    "model": "model-name-for-this-turn",
    "gateway": "optional-model-gateway-id"
  }
}
```

Rules:

- `selected_model` must be an object with required field `model` and optional field `gateway`.
- `selected_model.model` is the client-selected model name for this turn.
- `selected_model.model` must be a non-empty exact string for every `/chat/stream` request, including Astra's native web/TUI/default chat.
- If `selected_model.gateway` is absent, Astra uses its own model configuration to invoke `selected_model.model`.
- If `selected_model.gateway` is present, it must be a non-empty exact string referencing `model_gateways.id`.
- `selected_model.gateway` is not a binding-local capability server id, not a URL, and not a downstream provider name.
- Values must match exactly. Astra must not trim, case-fold, alias, default, or substitute either field.
- Model selection and model gateway selection are not stored in Agent Binding.
- Astra native web/TUI/default chat must be updated to send `selected_model.model`; it is not required to send `selected_model.gateway`.

## Storage Design

### Binding Id

Astra generates the binding id server-side. The client never supplies it.

Format:

```text
ab_<uuid-v7>
```

Implementation:

```rust
let binding_id = format!("ab_{}", uuid::Uuid::now_v7());
```

Rules:

- Use the lower-case hyphenated UUID string emitted by `uuid::Uuid`.
- Generate the id only after the create request has passed payload validation.
- If a generated id collides on the primary key, generate a new UUIDv7 and retry the insert a bounded number of times. Persistent collision is an internal error.
- The id is not derived from client workspace, Agent, version, `binding_name`, `idempotency_key`, or metadata hashes.
- API responses call this value `agent_binding_id`; the DB column may be named `id`.

Rationale:

- UUIDv7 is sortable enough for operations and already available through the workspace `uuid` dependency.
- Keeping the id opaque prevents Astra APIs and clients from depending on client-specific identity structure.

### Idempotency

Registration idempotency is controlled by a client-supplied opaque `idempotency_key`, not by first-class hash fields.

Rules:

- `idempotency_key` is required on `POST /agent-bindings`.
- Repeating the same `idempotency_key` with the same structurally equivalent binding payload returns the existing `agent_binding_id`.
- Repeating the same `idempotency_key` with a different binding payload returns `409 agent_binding_idempotency_conflict`.
- Reusing an existing `binding_name` with a different `idempotency_key` or structurally different binding payload returns `409 agent_binding_conflict`.
- `binding_name` is unique for the lifetime of the Astra registry. Disabled bindings do not free the name.

Structural equality:

- Compare parsed binding fields, not raw request bytes.
- Object member order is insignificant.
- Array order is significant.
- String values must match exactly.
- Astra must not trim strings, sort arrays, fill defaults, remove unknown fields, or otherwise repair two different requests into the same payload.
- V1 should reject unknown fields under `binding_schema_version=v1`; later schema versions can add explicit fields.

### Agent Binding Logical DDL

```sql
CREATE TABLE agent_bindings (
  id VARCHAR(64) NOT NULL PRIMARY KEY,
  binding_name VARCHAR(255) NOT NULL,
  idempotency_key VARCHAR(255) NOT NULL,
  status VARCHAR(32) NOT NULL,
  agent_md TEXT NOT NULL,
  capability_servers_json JSON NOT NULL,
  runtime_policy_json JSON NOT NULL,
  metadata_json JSON NULL,
  binding_schema_version VARCHAR(32) NOT NULL,
  created_at TIMESTAMP NOT NULL,
  disabled_at TIMESTAMP NULL,
  CHECK (status IN ('active', 'disabled', 'invalid')),
  UNIQUE (binding_name),
  UNIQUE (idempotency_key)
);
```

This is a logical Astra-owned table. Astra may adapt concrete column types to MatrixOne/MySQL protocol constraints, but the fields, uniqueness constraints, and immutability rules are required.

Field semantics:

| Field | Required | Meaning |
| --- | --- | --- |
| `id` | Yes | Astra-generated opaque binding id returned as `agent_binding_id`, format `ab_<uuid-v7>`. |
| `binding_name` | Yes | Client-supplied opaque logical name. Unique for the lifetime of the registry. |
| `idempotency_key` | Yes | Client-supplied opaque registration idempotency key. Unique for the lifetime of the registry. |
| `status` | Yes | Binding lifecycle state: `active`, `disabled`, or `invalid`. Only `active` bindings may start new runs. |
| `agent_md` | Yes | Complete immutable static Agent prompt supplied by the client. |
| `capability_servers_json` | Yes | Canonical JSON array of binding-local MCP/skill capability server refs. This is the only persisted Agent Binding capability endpoint registry. |
| `runtime_policy_json` | Yes | Canonical JSON runtime policy for Astra agent-loop execution. |
| `metadata_json` | No | Opaque client-owned diagnostics and provenance. No secrets. Astra stores and returns it but must not route, authorize, index, execute, or reconcile from it. |
| `binding_schema_version` | Yes | Binding payload schema version, initially `v1`. |
| `created_at` | Yes | Insert timestamp. |
| `disabled_at` | No | Time the binding was disabled for new runs. Existing runs are not cancelled by this field. |

Required means required in persisted rows. Optional request fields may still be normalized into persisted non-null defaults only when the protocol explicitly defines that default. In v1, `metadata` is the only optional binding payload field; if absent, service code may store SQL `NULL` or canonical `{}` consistently.

### Agent Binding MatrixOne DDL

Existing Astra storage frequently uses `LONGTEXT` for structured payloads and validates JSON in service code. Use the same pattern unless the target MatrixOne version reliably supports native JSON and CHECK constraints for this workload.

```sql
CREATE TABLE IF NOT EXISTS agent_bindings (
    id VARCHAR(64) PRIMARY KEY,
    binding_name VARCHAR(255) NOT NULL,
    idempotency_key VARCHAR(255) NOT NULL,
    status VARCHAR(32) NOT NULL,
    agent_md LONGTEXT NOT NULL,
    capability_servers_json LONGTEXT NOT NULL,
    runtime_policy_json LONGTEXT NOT NULL,
    metadata_json LONGTEXT NULL,
    binding_schema_version VARCHAR(32) NOT NULL,
    created_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
    disabled_at DATETIME(6) NULL,
    updated_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6) ON UPDATE CURRENT_TIMESTAMP(6),
    UNIQUE KEY uq_agent_bindings_name (binding_name),
    UNIQUE KEY uq_agent_bindings_idempotency_key (idempotency_key),
    INDEX idx_agent_bindings_status_created (status, created_at)
);
```

The service must enforce allowed status values even if the concrete DB does not enforce CHECK constraints.

Columns intentionally not present:

| Column | Reason |
| --- | --- |
| `model_policy_json` | Model selection is per turn through `selected_model.model`; model invocation uses Astra native model config unless optional `selected_model.gateway` is present. |
| `model_gateway_id` | Model gateway selection is optional and per turn from `selected_model.gateway`, not from Agent Binding. |
| `source_system`, `source_ref_json` | Client-owned source refs belong in `metadata_json`. Astra must not promote client resource identity to first-class columns. |
| `prompt_hash`, `capability_servers_hash`, `policy_hash`, `binding_content_hash` | Client-owned hashes belong in `metadata_json`. They are not part of the Agent Binding protocol. |
| `mcp_servers_json`, `skill_servers_json`, `model_servers_json` | `capability_servers_json` is the single registry for Agent-scoped MCP/skill servers. Model servers are not Agent Binding capability servers. |
| `allowed_tools_json`, `allowed_skills_json`, `allowed_models_json` | Per-turn authorization is represented by runtime auth and enforced by capability servers / model gateways. |

### Model Gateway Logical DDL

Astra must have an integration-level model gateway table outside Agent Binding storage. Clients register gateway rows through the model gateway API. Astra does not assume who the client is or how often it performs registration.

```sql
CREATE TABLE model_gateways (
  id VARCHAR(128) NOT NULL PRIMARY KEY,
  resolve_url TEXT NOT NULL,
  model_protocol VARCHAR(64) NOT NULL,
  status VARCHAR(32) NOT NULL,
  metadata_json JSON NULL,
  created_at TIMESTAMP NOT NULL,
  updated_at TIMESTAMP NOT NULL,
  disabled_at TIMESTAMP NULL,
  CHECK (status IN ('active', 'disabled', 'invalid'))
);
```

Field semantics:

| Field | Required | Meaning |
| --- | --- | --- |
| `id` | Yes | Stable opaque model gateway id referenced by runtime `selected_model.gateway`. |
| `resolve_url` | Yes | Absolute `http` or `https` model resolve endpoint. It must not contain userinfo or credential-bearing query parameters. |
| `model_protocol` | Yes | Invocation protocol Astra expects from this gateway's per-turn descriptor. V1 supports `openai_chat_completions`. |
| `status` | Yes | Gateway lifecycle state: `active`, `disabled`, or `invalid`. Only `active` gateways may be used for new turns. |
| `metadata_json` | No | Opaque integration diagnostics only. Astra must not route, authorize, or execute from fields inside it. |
| `created_at` | Yes | Insert timestamp. |
| `updated_at` | Yes | Last lifecycle update timestamp. |
| `disabled_at` | No | Time the gateway was disabled for new turns. |

### Model Gateway MatrixOne DDL

```sql
CREATE TABLE IF NOT EXISTS model_gateways (
    id VARCHAR(128) PRIMARY KEY,
    resolve_url LONGTEXT NOT NULL,
    model_protocol VARCHAR(64) NOT NULL,
    status VARCHAR(32) NOT NULL,
    metadata_json LONGTEXT NULL,
    created_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
    updated_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6) ON UPDATE CURRENT_TIMESTAMP(6),
    disabled_at DATETIME(6) NULL,
    INDEX idx_model_gateways_status_created (status, created_at)
);
```

Model gateway rules:

- `id` is opaque to Astra; Astra must not parse vendor, deployment, tenant, or client semantics from it.
- `resolve_url` must be absolute `http` or `https`, with no userinfo and no credential-bearing query parameters. V1 should reject query strings.
- `model_protocol` v1 supports only `openai_chat_completions`. Other protocol ids must be rejected at registration until Astra implements the adapter, streaming parser, tool-call mapping, error mapping, and tests.
- `metadata_json` is diagnostics only and must not contain secrets, bearer values, provider credentials, downstream provider URLs, or authorization scopes.
- The row is immutable after creation except `status`, `updated_at`, and `disabled_at`.
- Endpoint rotation is modeled by registering a different gateway id and sending that id in later turns. Astra must not silently mutate an existing active gateway definition.
- Repeating the same `id` with a structurally equivalent payload returns the existing row.
- Repeating the same `id` with a different `resolve_url`, `model_protocol`, or metadata payload is rejected.
- Astra must not trim, case-fold, normalize, rewrite, or repair model gateway fields during comparison.

### Optional Registration Attempts

Registration attempts are not required for runtime correctness. Add this table only if the registration APIs must expose durable operation history after the HTTP response is gone.

```sql
CREATE TABLE IF NOT EXISTS agent_binding_registration_attempts (
    attempt_id VARCHAR(64) PRIMARY KEY,
    binding_name VARCHAR(255) NULL,
    idempotency_key VARCHAR(255) NULL,
    status VARCHAR(32) NOT NULL,
    binding_id VARCHAR(64) NULL,
    error_code VARCHAR(128) NULL,
    error_message TEXT NULL,
    request_id VARCHAR(128) NULL,
    trace_id VARCHAR(128) NULL,
    created_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
    INDEX idx_binding_attempt_key_created (idempotency_key, created_at),
    INDEX idx_binding_attempt_name_created (binding_name, created_at),
    INDEX idx_binding_attempt_status_created (status, created_at)
);
```

Allowed attempt statuses:

```text
accepted
idempotent
rejected
failed
```

Attempt rows must never store runtime bearer values, secret-bearing payload excerpts, or model invocation descriptors.

### Run Attribution Columns

`agent_runs` should record the binding and selected runtime refs used to start a run. These fields are attribution and diagnostics, not authorization sources.

```sql
ALTER TABLE agent_runs
    ADD COLUMN agent_binding_id VARCHAR(64) NULL,
    ADD COLUMN agent_binding_name VARCHAR(255) NULL,
    ADD COLUMN agent_binding_schema_version VARCHAR(32) NULL,
    ADD COLUMN selected_model_json LONGTEXT NULL,
    ADD COLUMN selected_model_name VARCHAR(255) NULL,
    ADD COLUMN selected_model_gateway VARCHAR(128) NULL,
    ADD COLUMN capability_server_refs_json LONGTEXT NULL,
    ADD COLUMN runtime_profile VARCHAR(64) NULL;

ALTER TABLE agent_runs
    ADD INDEX idx_agent_runs_binding (agent_binding_id, created_at),
    ADD INDEX idx_agent_runs_model_gateway (selected_model_gateway, created_at);
```

For idempotent migrations, add these with `add_column_if_missing` / `add_index_if_missing`.

Do not persist runtime bearer values or model invocation descriptors as binding state. If run events need model diagnostics, store only redacted, non-secret execution metadata and never reuse it for another turn.

### Session Metadata

Do not add `default_agent_binding_id` to `agent_sessions` in this step. A binding is selected per run. UI-level defaults belong to the client or a future Astra conversation profile, not the binding registry.

## Rust Types

New crate/module location:

```text
rust/crates/services/src/agent_bindings.rs
rust/crates/services/src/model_gateways.rs
rust/crates/runtime/src/server/agent_binding_handlers.rs
rust/crates/runtime/src/server/agent_binding_runtime.rs
rust/crates/runtime/src/server/model_gateway_handlers.rs
rust/crates/runtime/src/server/model_gateway_runtime.rs
```

### Binding API Types

```rust
pub struct AgentBindingCreateRequestData {
    pub idempotency_key: String,
    pub binding: AgentBindingPayload,
}

pub struct AgentBindingPayload {
    pub binding_name: String,
    pub agent_md: String,
    pub capability_servers: Vec<CapabilityServerEndpoint>,
    pub runtime_policy: RuntimePolicy,
    pub metadata: Option<serde_json::Value>,
    pub binding_schema_version: String,
}

pub struct CapabilityServerEndpoint {
    pub id: String,
    pub server_type: CapabilityServerType,
    pub transport: CapabilityServerTransport,
    pub endpoint_url: String,
}

pub enum CapabilityServerType {
    Mcp,
    Skill,
}

pub enum CapabilityServerTransport {
    StreamableHttp,
}

pub struct RuntimePolicy {
    pub max_steps: Option<u32>,
    pub tool_mode: ToolMode,
}

pub enum ToolMode {
    McpGateway,
}

pub struct AgentBindingRecord {
    pub id: String,
    pub binding_name: String,
    pub idempotency_key: String,
    pub status: AgentBindingStatus,
    pub agent_md: String,
    pub capability_servers: Vec<CapabilityServerEndpoint>,
    pub runtime_policy: RuntimePolicy,
    pub metadata: Option<serde_json::Value>,
    pub binding_schema_version: String,
    pub created_at: String,
    pub disabled_at: Option<String>,
}
```

Serde mapping should keep the wire field `type` even if the Rust field is named `server_type`.

V1 structs should deny unknown fields. This prevents Astra from silently accepting and dropping client intent, which would break structural idempotency.

### Model Gateway Types

```rust
pub struct ModelGatewayCreateRequestData {
    pub id: String,
    pub resolve_url: String,
    pub model_protocol: ModelProtocol,
    pub metadata: Option<serde_json::Value>,
}

pub enum ModelProtocol {
    OpenAiChatCompletions,
}

pub enum ModelGatewayStatus {
    Active,
    Disabled,
    Invalid,
}

pub struct ModelGatewayRecord {
    pub id: String,
    pub resolve_url: String,
    pub model_protocol: ModelProtocol,
    pub status: ModelGatewayStatus,
    pub metadata: Option<serde_json::Value>,
    pub created_at: String,
    pub updated_at: String,
    pub disabled_at: Option<String>,
}

pub struct SelectedModel {
    pub model: String,
    pub gateway: Option<String>,
}

pub struct ModelResolveRequest {
    pub model: String,
    pub gateway: String,
}

pub struct ModelResolveResponse {
    pub model: String,
    pub status: ModelResolveStatus,
    pub protocol: ModelProtocol,
    pub invoke: ModelInvokeDescriptor,
}

pub struct ModelInvokeDescriptor {
    pub url: String,
}
```

The resolve response is per-turn state. It must be stored only in run-local memory.

### Chat Runtime Types

Extend `ChatRequestData` and HTTP type layer:

```rust
pub struct AgentBindingRuntimeRequest {
    pub id: String,
    pub capability_server_refs: CapabilityServerRefs,
}

pub struct CapabilityServerRefs {
    pub mcp: String,
    pub skills: String,
}

pub struct RuntimeAuthRequest {
    pub authorization: String,
}

pub struct ChatRequestData {
    // existing fields...
    pub agent_binding: Option<AgentBindingRuntimeRequest>,
    pub selected_model: SelectedModel,
    pub runtime_auth: Option<RuntimeAuthRequest>,
    pub runtime_profile: Option<RuntimeProfileRequest>,
}

pub enum RuntimeProfileRequest {
    RequestScopedRuntimeMcp,
    AgentBindingRegistry,
}
```

The wire contract should be strict:

- `selected_model.model` is required for every `/chat/stream` request.
- `selected_model.gateway` is optional. If absent, model invocation uses Astra's own model configuration.
- `runtime_auth.authorization` is required when `agent_binding` is present, because MCP/skill discovery uses registered capability endpoints.
- `runtime_auth.authorization` is required when `selected_model.gateway` is present, because model gateway resolve/invocation uses registered model gateway endpoints.
- `runtime_auth.authorization` is not required for native/default chat that has no `agent_binding` and no `selected_model.gateway`.
- If `agent_binding` is present, that field is the explicit opt-in to Agent Binding registry semantics. `runtime_profile` may be omitted; when present, it must be `agent_binding_registry`.
- If `runtime_mcp_bindings` is non-empty, `runtime_profile` must be `request_scoped_runtime_mcp` after the migration flag is enabled.
- A request must not set both `agent_binding` and `runtime_mcp_bindings`.
- A request must not set both `agent_binding` and `mcp_binding_ids`.
- Agent Binding mode must not carry capability server definitions in the chat request.
- Plain chat with `selected_model.gateway` omits `agent_binding` but still carries `selected_model` and `runtime_auth`.
- Native/default chat omits `agent_binding`, omits `selected_model.gateway`, and carries `selected_model.model`.

During the compatibility window, existing `runtime_mcp_bindings` clients may omit `runtime_profile` only behind an explicit server config flag such as `runtime.allow_implicit_request_scoped_mcp=true`. That path must emit deprecation diagnostics. Agent Binding does not rely on implicit request-scoped detection: the presence of `agent_binding` is the explicit runtime selector.

## API Design

### Create Binding

```text
POST /agent-bindings
```

Request:

```json
{
  "idempotency_key": "agent-binding-register-01HZ7Y9Q9W0J6F3Q1J8R4K6N2A",
  "binding": {
    "binding_name": "agent-binding-01HZ7Y9Q9W0J6F3Q1J8R4K6N2A",
    "agent_md": "... complete immutable agent prompt ...",
    "capability_servers": [
      {
        "id": "tools",
        "type": "mcp",
        "transport": "streamable_http",
        "endpoint_url": "https://capabilities.example.com/api/v1/mcp/http"
      },
      {
        "id": "skills",
        "type": "skill",
        "transport": "streamable_http",
        "endpoint_url": "https://capabilities.example.com/api/v1/skills/http"
      }
    ],
    "runtime_policy": {
      "max_steps": 5000,
      "tool_mode": "mcp_gateway"
    },
    "metadata": {
      "source_ref": "agent-version-opaque-ref",
      "source_package_ref": "agent-package-opaque-ref",
      "prompt_hash": "sha256:...",
      "capability_server_set_hash": "sha256:...",
      "runtime_policy_hash": "sha256:...",
      "binding_content_hash": "sha256:...",
      "trace_ref": "opaque-debug-ref"
    },
    "binding_schema_version": "v1"
  }
}
```

Response:

```json
{
  "agent_binding_id": "ab_018f05f5-c7dd-7f43-83e6-93d56d9d7391",
  "binding_name": "agent-binding-01HZ7Y9Q9W0J6F3Q1J8R4K6N2A",
  "status": "active"
}
```

The registration API must not require or return `binding_content_hash`, `prompt_hash`, `capability_server_set_hash`, or `runtime_policy_hash` as first-class protocol fields. A client may include those values in `binding.metadata`; Astra treats them as opaque metadata.

Validation:

- `idempotency_key`, `binding.binding_name`, `binding.agent_md`, and `binding.binding_schema_version` are required.
- `binding_schema_version` v1 must equal `v1`.
- `idempotency_key` and `binding_name` must be non-empty exact strings without path separators, control characters, or leading/trailing whitespace.
- `agent_md` must not exceed configured max bytes.
- `capability_servers` must be non-empty.
- `capability_servers[].id` must be unique inside the binding.
- Each capability server must include `id`, `type`, `transport`, and `endpoint_url`.
- V1 supports `type=mcp` and `type=skill`.
- V1 supports `transport=streamable_http`.
- `endpoint_url` must be absolute `http` or `https`; it must not include userinfo, fragment, or query.
- For Agent Binding registry v1, the binding must contain at least one `type=mcp` and one `type=skill` capability server.
- Capability server objects and `metadata` must not contain inline secrets, bearer tokens, downstream provider credentials, user tokens, plaintext client-specific authorization scope, per-user allowed tools, or runtime-discovered tool/model schemas.
- Payload must not contain first-class `model_policy`, `selected_model`, `model_gateway`, `allowed_tools`, `allowed_skills`, `allowed_models`, `tool_specs`, `runtime_token`, `headers`, `authorization`, `credentials`, or discovery result fields.
- `runtime_policy.tool_mode` must be `mcp_gateway`.
- `runtime_policy.max_steps`, when present, must be positive and must not exceed the server global max.

Idempotency:

- Duplicate `idempotency_key` plus structurally equivalent binding returns the existing binding.
- Duplicate `idempotency_key` plus structurally different binding returns `409 agent_binding_idempotency_conflict`.
- Duplicate `binding_name` with a different `idempotency_key` or structurally different payload returns `409 agent_binding_conflict`.
- If `agent_binding_registration_attempts` is enabled, record accepted, idempotent, rejected, and failed attempts there. Without that optional table, the HTTP response and structured request log are the registration result.

### Get Binding

```text
GET /agent-bindings/{id}
```

Returns the stored binding payload and opaque metadata for diagnostics. It must never return runtime bearer values because none are stored. Standard GET should not need to return `idempotency_key`; keep that internal unless an admin-only diagnostics contract explicitly requires it.

### Disable Binding

```text
POST /agent-bindings/{id}/disable
```

Disables the binding for new runs.

Rules:

- Existing running runs are not cancelled by disabling.
- New `/chat/stream` requests with a disabled binding fail before loop start.
- Re-enabling is not supported in v1. A client should register a new binding.
- Disabling may update only `status`, `disabled_at`, and row update metadata. Immutable payload fields must not change.

### Create Model Gateway

```text
POST /model-gateways
```

Request:

```json
{
  "id": "primary-model-gateway",
  "resolve_url": "https://models.example.com/api/v1/models/resolve",
  "model_protocol": "openai_chat_completions",
  "metadata": {
    "trace_ref": "opaque-debug-ref"
  }
}
```

Response:

```json
{
  "id": "primary-model-gateway",
  "status": "active"
}
```

Validation:

- `id`, `resolve_url`, and `model_protocol` are required.
- `id` must be non-empty and must not contain path separators, control characters, or leading/trailing whitespace.
- `resolve_url` must be absolute `http` or `https`; it must not include userinfo, fragment, or query.
- `model_protocol` v1 must be `openai_chat_completions`.
- `metadata` must not contain secrets, bearer tokens, downstream provider credentials, provider-internal URLs, user tokens, selected-model authorization scope, or runtime-discovered model schemas.

### Get Model Gateway

```text
GET /model-gateways/{id}
```

Returns gateway definition and opaque metadata for diagnostics. It must never return runtime bearer values or downstream provider credentials.

### Disable Model Gateway

```text
POST /model-gateways/{id}/disable
```

Disables the gateway for new turns. Existing running turns are not cancelled by this field.

### Chat Stream

```text
POST /chat/stream
```

Native/default chat request using Astra model config:

```json
{
  "message": "user question",
  "parts": [],
  "attachments": [],
  "session_id": "optional-provider-session-id",
  "selected_model": {
    "model": "model-name-for-this-turn"
  },
  "context": {
    "conversation_ref": "conversation-opaque-ref",
    "task_ref": "task-opaque-ref",
    "correlation_ref": "turn-opaque-ref"
  }
}
```

Agent turn request using Astra model config:

```json
{
  "message": "user question",
  "parts": [],
  "attachments": [],
  "session_id": "optional-provider-session-id",
  "agent_binding": {
    "id": "ab_018f05f5-c7dd-7f43-83e6-93d56d9d7391",
    "capability_server_refs": {
      "mcp": "tools",
      "skills": "skills"
    }
  },
  "selected_model": {
    "model": "model-name-for-this-turn"
  },
  "runtime_auth": {
    "authorization": "Bearer <opaque-runtime-bearer>"
  },
  "context": {
    "conversation_ref": "conversation-opaque-ref",
    "task_ref": "task-opaque-ref",
    "correlation_ref": "turn-opaque-ref"
  }
}
```

Agent turn request using a model gateway:

```json
{
  "message": "user question",
  "parts": [],
  "attachments": [],
  "session_id": "optional-provider-session-id",
  "agent_binding": {
    "id": "ab_018f05f5-c7dd-7f43-83e6-93d56d9d7391",
    "capability_server_refs": {
      "mcp": "tools",
      "skills": "skills"
    }
  },
  "selected_model": {
    "model": "model-name-for-this-turn",
    "gateway": "primary-model-gateway"
  },
  "runtime_auth": {
    "authorization": "Bearer <opaque-runtime-bearer>"
  },
  "context": {
    "conversation_ref": "conversation-opaque-ref",
    "task_ref": "task-opaque-ref",
    "correlation_ref": "turn-opaque-ref",
    "source": "agent-binding-runtime"
  }
}
```

Plain chat request using a model gateway:

```json
{
  "message": "user question",
  "parts": [],
  "attachments": [],
  "session_id": "optional-provider-session-id",
  "selected_model": {
    "model": "model-name-for-this-turn",
    "gateway": "primary-model-gateway"
  },
  "runtime_auth": {
    "authorization": "Bearer <opaque-runtime-bearer>"
  },
  "context": {
    "conversation_ref": "conversation-opaque-ref",
    "task_ref": "task-opaque-ref",
    "correlation_ref": "turn-opaque-ref"
  }
}
```

Rejected combinations:

```text
agent_binding + runtime_mcp_bindings
agent_binding + mcp_binding_ids
missing selected_model
selected_model without selected_model.model
selected_model.gateway as an empty string
agent_binding without runtime_auth.authorization
selected_model.gateway without runtime_auth.authorization
runtime_profile=request_scoped_runtime_mcp with agent_binding
runtime_profile=agent_binding_registry without agent_binding
agent_binding.capability_server_refs.models
chat request capability server definitions in Agent Binding mode
selected_model as a string instead of an object
runtime_auth.credentials map instead of runtime_auth.authorization
```

Runtime `agent_binding` parsing protocol:

- `agent_binding` is optional.
- When present, it must be an object with exact fields `id` and `capability_server_refs`.
- `agent_binding.id` must be a non-empty Astra binding id. It is used only to load `agent_bindings.id`.
- `capability_server_refs` must be an object with exact fields `mcp` and `skills`.
- `capability_server_refs.mcp` and `.skills` are binding-local server ids.
- Values must match exactly. Astra must not trim, case-fold, alias, default, or substitute missing refs.
- Runtime request fields cannot override `endpoint_url` or `transport`; those come only from the stored binding.

Runtime `selected_model` parsing protocol:

- `selected_model` must be present for every `/chat/stream` request.
- `selected_model` must be an object with required field `model` and optional field `gateway`.
- `selected_model.model` must be a non-empty exact string.
- Missing `selected_model`, JSON `null`, an empty object, a string value, a missing `model`, or an empty `model` fails with a typed 400 before binding load, discovery, model config lookup, model gateway lookup, or model resolve.
- If `selected_model.gateway` is absent, Astra resolves `selected_model.model` through its own configured model registry/provider settings.
- If `selected_model.gateway` is present, it must be a non-empty exact string and must resolve to an active `model_gateways.id`.
- An empty `gateway` string fails with a typed 400. Omit the field to use Astra's own model configuration.

### Native Astra Client Compatibility

This design changes Astra's built-in web, TUI, CLI, and default `/chat/stream` request shape to use `selected_model.model` for model selection. It must not force those native clients to use Agent Binding, model gateways, or runtime auth.

Request routing must be explicit:

```text
all requests:
  require selected_model object
  require selected_model.model

model invocation path:
  if selected_model.gateway is present:
    resolve selected_model.gateway through model_gateways
    require runtime_auth.authorization
  else:
    resolve selected_model.model through Astra native model config

capability path:
  if request has agent_binding:
    use agent_binding_registry
    require runtime_auth.authorization

  else if request has runtime_mcp_bindings or runtime_profile == request_scoped_runtime_mcp:
    use existing request-scoped MCP path

  else:
    use existing Astra native/default capability path
```

Rules:

- The native/default chat path must provide `selected_model.model`.
- The native/default chat path must not be forced to provide `agent_binding`, `capability_server_refs`, `selected_model.gateway`, or `runtime_auth.authorization`.
- The native/default chat path keeps its current built-in tools, session behavior, and web UI contract except for the model-selection request shape.
- If native/default chat omits `selected_model.gateway`, model invocation uses Astra's own model configuration for `selected_model.model`.
- Agent Binding discovery results must not be written into session-global state that the native/default chat path can see.
- A failed Agent Binding registry request must return a typed error in that mode. It must not fall through to native/default chat.
- Passing `agent_binding` is an explicit opt-in to Agent Binding registry semantics, even if `runtime_profile` is omitted.

## Runtime Flow

Unified chat runtime:

```text
validate request profile and reject mixed runtime modes
  -> parse selected_model object exactly
  -> reject missing selected_model or empty selected_model.model
  -> if selected_model.gateway is present, reject empty selected_model.gateway
  -> if agent_binding is present, load agent_bindings.id
  -> require binding status=active
  -> parse capability_server_refs exactly
  -> resolve refs against capability_servers_json
  -> require mcp ref resolves to type=mcp
  -> require skills ref resolves to type=skill
  -> if agent_binding is present, require runtime_auth.authorization
  -> if selected_model.gateway is present:
       - require runtime_auth.authorization
       - resolve selected_model.gateway against model_gateways
       - require gateway status=active
  -> if selected_model.gateway is absent:
       - resolve selected_model.model against Astra native model config
       - reject unknown or disabled native model config
  -> preflight network calls before the first LLM call:
       - if selected_model.gateway is present, call model_gateways.resolve_url with selected_model and runtime bearer
       - if agent_binding is present, call selected MCP capability server tools/list with runtime bearer
       - if agent_binding is present, call selected skill capability server skills/list with runtime bearer
  -> build model invocation descriptor from native model config or validate the gateway-returned descriptor
  -> convert returned tools and skills into Astra LLM capability surface
  -> freeze this loop's context manifest:
       - selected_model.model
       - optional selected_model.gateway
       - resolved native model config or model gateway invocation descriptor
       - optional agent_binding id and binding schema version
       - optional selected MCP/skill capability server ids
       - optional discovered tool schemas
       - optional discovered skill schemas
       - optional agent_md and runtime_policy
       - dynamic turn message, parts, attachments, and context
  -> if agent_binding is absent, manifest has no Agent prompt, no Agent MCP tools, and no Agent skills
  -> run agent loop
  -> route tools/call through selected MCP capability server using the same request bearer
  -> route model calls only through the resolved native model config or model gateway invocation descriptor for this turn
  -> persist redacted run attribution columns
```

Important: request-scoped MCP code is safe because it is run-local, but it creates credential-bearing clients per request. Agent Binding mode should introduce credential-neutral client layers with per-call bearer injection instead of reusing a long-lived authorization-scoped MCP client across runtime credentials.

The context manifest is a per-loop object, not a session default and not an authorization source. It is built once before the first model call. Astra must not rediscover tools or skills in the middle of the loop, must not add tools from previous turns in the same session, and must not broaden the manifest from request body fields. A later `/chat/stream` request may discover a different surface because it is a different loop with a different bearer, selected model, or binding refs.

### Prompt Assembly

`agent_md` is the complete static Agent prompt supplied by the client. It includes role, mission, business sections, answer style, sensitive-word rules, tool-use policy, and static output contracts.

It is still not the entire final raw LLM prompt. Astra still owns final runtime assembly:

- runtime scaffolding;
- current date;
- tool and skill schemas returned for this runtime bearer;
- runtime safety and tool-use mechanics;
- dynamic user message, parts, attachments, and context;
- context compression and cache annotations;
- the per-turn resolved model invocation descriptor.

Implementation should add a dedicated stable prompt section:

```text
Agent Binding Instruction
<agent_md>
```

Do not add client business semantics in Astra prompt templates. Business terms, answer style, and customer-specific rules belong in `agent_md`, discovered skills, or client-owned semantic resources.

### Model Resolution

Model selection is not stored in Agent Binding. The runtime request always sends `selected_model.model`; it may also send `selected_model.gateway`.

There are two model resolution paths.

Path A, no `selected_model.gateway`:

1. Parse `selected_model.model`.
2. Resolve it against Astra's native model registry/provider configuration.
3. Reject the request if the model is unknown, disabled, or not invokable under Astra's local configuration.
4. Build the per-loop model invocation descriptor from Astra's native model config.

Rules:

- Astra native model config resolution must use `selected_model.model` exactly.
- Astra must not silently substitute the server default model when `selected_model.model` is missing or unknown.
- UI defaults belong in the web/TUI/CLI client: the client selects a model and sends it as `selected_model.model`.
- This path does not require `runtime_auth.authorization` unless `agent_binding` is also present for MCP/skill discovery.

Path B, `selected_model.gateway` present:

1. Parse `selected_model.model` and `selected_model.gateway`.
2. Resolve `selected_model.gateway` to an active `model_gateways` row.
3. Reject the request if the gateway id is unknown, disabled, or invalid.
4. Require `runtime_auth.authorization`.
5. Call `model_gateways.resolve_url`.
6. Attach `runtime_auth.authorization`.
7. Send body:

```json
{
  "model": "model-name-for-this-turn",
  "gateway": "primary-model-gateway"
}
```

Request body must not include client identity, client resource ids, provider base URL, provider API key, credential ref, quota override, fallback model, or binding-local capability server ids.

Expected response:

```json
{
  "model": "model-name-for-this-turn",
  "status": "ready",
  "protocol": "openai_chat_completions",
  "invoke": {
    "url": "https://models.example.com/api/v1/models/openai/chat/completions"
  }
}
```

Rules:

- The response is a per-turn invocation descriptor, not binding state.
- Astra must use it only for the current turn.
- Astra must not cache it as binding state or reuse it for another runtime bearer.
- The response must not include downstream provider API keys, customer credentials, client resource ids, bearer claims, or provider-internal URLs unless the registered integration contract intentionally makes `invoke.url` the only model invocation URL visible to Astra.
- `protocol` must exactly equal `model_gateways.model_protocol`.
- `invoke.url` must be absolute `http` or `https`, with no userinfo and no credential-bearing query parameters.
- `invoke.url` is the only model endpoint Astra may call for this turn.
- Model invocation must attach the exact `runtime_auth.authorization` bearer from the request. Future bearer-refresh support would require an explicit protocol extension; Astra must not invent it silently.
- If the invocation protocol carries a model field, it must exactly equal the resolved descriptor's `model` and request `selected_model.model`.
- Denied, unavailable, unhealthy, over-quota, or malformed model resolution fails before the loop starts.
- Astra must not choose a fallback model or direct provider endpoint.

### Runtime Policy

`runtime_policy.max_steps` maps to the agentic hard turn ceiling for this run. It must be positive and not exceed server global max.

`runtime_policy.tool_mode` v1 supports only `mcp_gateway`.

If a request tries to use Agent Binding mode with server-local built-in tools beyond Astra runtime needs, the runtime policy must decide whether they are allowed. For Agent Binding mode, the clean default is:

```text
tool_mode=mcp_gateway
server built-ins exposed only for Astra runtime needs, not as client-visible tools
```

This prevents Agent Binding clients from accidentally exposing Astra local filesystem/shell tools to the loop.

## Capability Discovery

### Discovery Timing

Discovery is part of `/chat/stream` preflight for one Astra loop.

Required timing:

1. Parse and validate `selected_model` locally.
2. If `agent_binding` is present or `selected_model.gateway` is present, parse and validate `runtime_auth.authorization` locally.
3. If `agent_binding` is present, load the active binding and resolve `capability_server_refs` locally.
4. If `selected_model.gateway` is present, resolve the active model gateway row locally.
5. If `selected_model.gateway` is absent, resolve `selected_model.model` against Astra native model config locally.
6. Before the first LLM call, perform all required network preflight calls:
   - model resolve through `selected_model.gateway` only when gateway is present;
   - `tools/list` through the selected binding-local MCP endpoint when `agent_binding` is present;
   - `skills/list` through the selected binding-local skill endpoint when `agent_binding` is present.
7. Build and freeze this loop's context manifest from the resolved model descriptor and discovered capability schemas.
8. Start the agent loop only after the context manifest is complete.

When multiple network preflight calls are required, they may run concurrently after local validation has succeeded because they are independent authorization checks from Astra's point of view. Concurrency is an implementation optimization only; completion of all required calls is still a hard precondition before the first LLM request.

Failure rules:

- If `selected_model` is missing or `selected_model.model` is empty, fail before any model config lookup, model gateway lookup, binding lookup, or discovery call.
- If `selected_model.gateway` is present but empty, fail before any model config lookup, model gateway lookup, binding lookup, or discovery call.
- If `selected_model.gateway` is absent and Astra native model config cannot resolve `selected_model.model`, fail before the loop starts.
- If `selected_model.gateway` is present and model gateway resolve fails, fail the request even if tool/skill discovery would have succeeded.
- If `agent_binding` is present and MCP discovery fails, fail the request before the loop starts.
- If `agent_binding` is present and skill discovery fails, fail the request before the loop starts.
- If discovery returns an empty tool list or empty skill list under a valid bearer, the loop may continue with an empty surface for that class.
- Astra must not retry against a different model gateway, a different capability server, a previous discovery result, or request-scoped MCP.

Reuse rules:

- Discovery results are scoped to one `/chat/stream` loop.
- Discovery results are not session state.
- Discovery results are not stored back into Agent Binding.
- Discovery results are not reused for a later turn, even in the same `session_id`.
- Astra does not rediscover tools or skills mid-loop. If a tool or skill changes after preflight, the change is visible only to a later `/chat/stream` request after that request performs its own discovery.
- Tool calls during the loop must target the selected MCP endpoint from the context manifest and use the same request bearer.
- Skill execution or skill expansion during the loop must target the selected skill endpoint from the context manifest and use the same request bearer.

### Pooling Strategy

The runtime must scale to many concurrent client-originated turns without creating a long-lived authorization-scoped MCP client for every user when the protocol does not require it.

The pool boundary is:

```text
safe to pool:
  reqwest clients, TCP/TLS connections, DNS state, and stateless endpoint adapters

not safe to pool globally:
  logical MCP sessions, discovered tool/skill surfaces, bearer headers, model descriptors, or any state derived from one runtime bearer
```

For 1000 concurrent turns, Astra should not share one logical MCP client keyed only by `binding_id` or `capability_server_id`. Tool and skill visibility is derived from the runtime bearer, so a client/session that caches discovery or headers for one bearer cannot be reused for another bearer.

Recommended v1 approach:

- Use a shared `reqwest::Client` or equivalent HTTP transport pool per Astra process.
- Read `endpoint_url` from the stored binding for MCP/skill discovery and invocation.
- Read `resolve_url` from `model_gateways` for model resolve only when `selected_model.gateway` is present.
- Use Astra native model config for model invocation when `selected_model.gateway` is absent.
- Use `invoke.url` from the per-turn model gateway descriptor when `selected_model.gateway` is present.
- Attach `runtime_auth.authorization` per registered endpoint call, not at pooled-client construction time.
- Treat `tools/list`, `tools/call`, `skills/list`, model gateway resolve, and model gateway invocation as per-turn/per-call RPCs over shared transport.
- Keep discovered tools, discovered skills, and resolved model descriptors in the run context only.
- Bound concurrent RPCs per endpoint and fail with explicit backpressure errors when the limit is exceeded.

Optional future optimization:

- A short-lived logical MCP session cache is allowed only for transports whose protocol explicitly supports per-call credentials and whose session state is not authorization scoped.
- Any such cache must be keyed by binding id, capability server id, endpoint URL, transport, and a non-persisted digest of the exact bearer scope, with TTL no longer than the bearer expiry.
- It must never broaden discovery or reuse a model descriptor across different runtime bearer values.

Effective concurrency model:

```text
current request-scoped MCP:
  active logical MCP clients = active runs * selected MCP servers per run

target Agent Binding adapter:
  active turn contexts = active runs
  physical HTTP connections = bounded shared transport pool
  authorization/discovery/model state = run-local
```

### MCP Capability Server

`type=mcp` capability servers use the credential-neutral endpoint client described above. They may reuse existing `astra-mcp` schema conversion and naming rules, but should not require a long-lived authorization-scoped `McpClientManager` per user when the endpoint supports stateless per-call JSON-RPC over HTTP.

Public tool names remain:

```text
mcp__<server_id>__<tool_name>
```

`server_id` is the sanitized binding-local capability server id, not a client resource id.

Discovery rules:

- Call `tools/list` using the selected MCP server endpoint and runtime bearer.
- Expose only tools returned by discovery for this runtime bearer.
- Do not add tools from a previous turn, a binding-level static allow-list, or request body overrides.
- Empty discovery under a valid runtime bearer is allowed. Astra must not synthesize substitute tools.
- Discovery happens before the first LLM call and contributes to this loop's context manifest.
- Discovery does not run lazily after the model asks for a tool.

Tool call rules:

- Route `tools/call` through the selected MCP server endpoint.
- Use the exact same `runtime_auth.authorization` bearer from the request.
- Do not let request body fields override client authorization scope.
- If the endpoint rejects the bearer during `tools/call`, fail the tool call or run explicitly. Astra must not rediscover through another endpoint or continue with a substitute tool.

### Skill Capability Server

`type=skill` capability servers are part of v1. Astra must not accept a v1 binding and then ignore skills.

Runtime rules:

- Call `skills/list` using the selected skill server endpoint and runtime bearer.
- Convert returned skills into Astra's active skill surface or equivalent LLM-facing schema.
- Expose only skills returned by discovery for this runtime bearer.
- Empty skills discovery under a valid runtime bearer is allowed.
- If Astra cannot convert the `skills/list` response shape, fail before the loop starts.
- Discovery happens before the first LLM call and contributes to this loop's context manifest.
- Astra must not accept request body skill definitions in Agent Binding mode.

Implementation note: if the existing Astra skill resolver cannot ingest remote per-turn skills yet, this is required work in the same milestone as Agent Binding runtime support. Do not store `type=skill` in the binding and silently ignore it.

## Security Rules

### Secret Validation

Reject secret-like fields in binding payloads and model gateway payloads where they would carry runtime or provider secrets.

High-risk keys:

```text
authorization
auth_token
api_key
token
secret
password
cookie
set-cookie
credential
headers
runtime_token
provider_api_key
provider_base_url
client_workspace_id
client_user_id
allowed_tools
tool_schemas
model_schemas
runtime_discovery
```

Hash-like diagnostic keys inside `metadata` are allowed only as opaque client-owned diagnostics. Astra must not interpret them as binding truth.

Reject capability server `endpoint_url`, model gateway `resolve_url`, and model descriptor `invoke.url` with userinfo, fragment, or query. V1 rejects query strings rather than maintaining a fragile allow/deny list of query parameter names.

### Runtime Bearer Handling

Runtime bearer values:

- may be copied into in-memory transport headers for the current call;
- must be redacted from errors with the existing redaction helper plus exact-value replacement;
- must not enter `Debug`, SSE, run events, session state, checkpoints, or DB rows;
- must not be parsed by Astra for client authorization or selected-model claims.

### Authorization Boundary

Astra validates:

- selected_model object shape;
- selected_model non-empty `model`;
- selected_model non-empty `gateway` when gateway is present;
- Astra native model config exists and is enabled when `selected_model.gateway` is absent;
- binding exists;
- binding active;
- model gateway exists when `selected_model.gateway` is present;
- model gateway active when `selected_model.gateway` is present;
- runtime profile is Agent Binding registry mode;
- selected capability refs exist and match expected server types when `agent_binding` is present;
- capability server endpoint shape;
- runtime bearer shape and presence;
- runtime policy;
- model resolve response shape;
- discovery response shape.

Astra does not validate:

- client user permissions;
- client workspace or tenant membership;
- client Agent or manifest status;
- client selected model authorization;
- client tool/data scope;
- client credential ownership;
- client side-effect policy;
- quota and provider routing policy.

Those are enforced by the registered MCP, skill, and model gateway endpoints.

## Error Codes

| Code | HTTP | Meaning |
| --- | --- | --- |
| `agent_binding_invalid` | 400 | Binding create payload shape is invalid |
| `agent_binding_conflict` | 409 | Same binding name exists with a different idempotency key or structurally different payload |
| `agent_binding_idempotency_conflict` | 409 | Same idempotency key exists with structurally different payload |
| `agent_binding_not_found` | 404 | Binding id does not exist |
| `agent_binding_disabled` | 409 | Binding is disabled for new runs |
| `agent_binding_policy_invalid` | 400 | Runtime policy invalid |
| `agent_binding_runtime_auth_missing` | 400 | Runtime auth bearer missing when agent_binding or selected_model.gateway requires registered endpoint calls |
| `agent_binding_runtime_auth_invalid` | 400 | Runtime auth bearer shape invalid |
| `agent_binding_runtime_profile_conflict` | 400 | Mixed binding mode and request-scoped mode |
| `selected_model_missing` | 400 | /chat/stream request omitted selected_model or sent JSON null |
| `selected_model_invalid` | 400 | selected_model is not an object, has unknown fields, is missing model, has empty model, or has empty gateway when gateway is present |
| `selected_model_not_configured` | 404 | selected_model.gateway is absent and selected_model.model does not resolve to an enabled Astra native model config |
| `agent_binding_capability_ref_missing` | 400 | Required binding-local capability server ref is missing |
| `agent_binding_capability_ref_invalid` | 400 | Runtime ref does not resolve to a server of the required type |
| `agent_binding_capability_transport_unsupported` | 400 | Binding declares a capability server transport this runtime cannot execute |
| `model_gateway_invalid` | 400 | Model gateway create payload shape is invalid |
| `model_gateway_conflict` | 409 | Same model gateway id exists with structurally different payload |
| `model_gateway_not_found` | 404 | Runtime selected model gateway does not exist |
| `model_gateway_disabled` | 409 | Runtime selected model gateway is disabled for new turns |
| `model_gateway_protocol_unsupported` | 400 | Model gateway protocol is not implemented by Astra |
| `model_gateway_resolve_failed` | 502 | Model gateway denied, failed, or was unavailable |
| `model_gateway_descriptor_invalid` | 502 | Model resolve response cannot be converted into an invocation descriptor |
| `agent_binding_discovery_failed` | 502 | MCP/skill discovery failed |
| `agent_binding_schema_invalid` | 502 | Discovery response cannot be converted to tool/skill schema |

MCP transport-level errors may keep existing codes when they happen inside reused MCP conversion or compatibility paths, but the outer handler should include `agent_binding_id` and binding-local server id in redacted diagnostics.

## Migration Plan

### Phase 1: Storage and Services

- Add v1 `agent_bindings` DDL with `id`, `binding_name`, `idempotency_key`, `status`, `agent_md`, `capability_servers_json`, `runtime_policy_json`, `metadata_json`, and `binding_schema_version`.
- Add v1 `model_gateways` DDL with `id`, `resolve_url`, `model_protocol`, `status`, `metadata_json`, `created_at`, `updated_at`, and `disabled_at`.
- Do not add first-class `model_policy_json`, hash columns, or separate MCP/skill/model server tables.
- Add `AgentBindingService` and `ModelGatewayService` traits plus database implementations.
- Implement structural idempotency comparison from parsed payloads.
- Add create/get/disable unit tests for both registries.
- Add validation tests for duplicate name/id, duplicate idempotency key, secret-like payload, invalid URL, unsupported type/transport/protocol, missing MCP/skill server, and exact-string handling.

### Phase 2: HTTP API

- Add `/agent-bindings` routes.
- Add `/model-gateways` routes.
- Add server auth policy matching existing admin/runtime integration routes.
- Add SDK types and path constants.
- Add contract tests for request/response shape and absence of first-class hash fields / bearer fields.

### Phase 3: Chat Wire Shape

- Change all `/chat/stream` clients, including native web/TUI/CLI, to send strict object `selected_model` with required `model`.
- Add optional `selected_model.gateway`, `agent_binding`, `runtime_auth.authorization`, and `runtime_profile` to `ChatRequest`.
- Reject mixed `agent_binding` and `runtime_mcp_bindings`.
- Reject every `/chat/stream` request without `selected_model.model`.
- Reject `selected_model` string form.
- Reject empty `selected_model.gateway` when gateway is present.
- Require `runtime_auth.authorization` when `agent_binding` is present or `selected_model.gateway` is present.
- Reject `runtime_auth.credentials` map in paths that require `runtime_auth.authorization`.
- Persist binding/model attribution columns on `agent_runs`.
- Keep request-scoped MCP behavior unchanged for explicit `request_scoped_runtime_mcp` requests.

### Phase 4: Model Gateway Runtime

- Add model gateway resolver:
  - run only when `selected_model.gateway` is present;
  - resolve `selected_model.gateway`;
  - call `model_gateways.resolve_url`;
  - attach `runtime_auth.authorization`;
  - validate response protocol and invocation URL;
  - wire model calls through the returned invocation URL for this turn only.
- Add native model config resolver for the gateway-absent path.
- Add tests for unknown native model, unknown gateway, disabled gateway, denied model, malformed descriptor, protocol mismatch, invocation URL with credentials, and no fallback model.

### Phase 5: MCP and Skill Discovery

- Add `agent_binding_runtime` resolver:
  - load active binding when `agent_binding` is present;
  - resolve selected `mcp` and `skills` refs;
  - call MCP endpoint with per-call runtime bearer through shared HTTP transport;
  - convert discovered MCP tools with existing naming/schema rules;
  - call `skills/list` and convert returned skills to Astra runtime skill surface;
  - inject `agent_md` section;
  - install discovered tools/skills and model descriptor into the loop.
- Add E2E with fake MCP, skill, and model gateway endpoints.
- Verify runtime bearer does not appear in DB, logs exposed to tests, or SSE events.

### Phase 6: Client Protocol Adoption

- A client registers model gateways before sending turns that reference them.
- A client registers bindings before sending Agent turns that reference them.
- A client stores its own resource-to-`agent_binding_id` mapping outside Astra if it needs one.
- A client starts Astra with `selected_model` for every turn; it adds `runtime_auth` only when it uses `agent_binding` or `selected_model.gateway`.
- A client selects either `request_scoped_runtime_mcp`, `agent_binding_registry`, or the existing native/default chat path explicitly. No silent substitution.

## Tests

### Unit Tests

- Server-generated binding id has `ab_<uuid-v7>` format.
- Same `idempotency_key` and structurally equal binding payload returns existing binding.
- Same `idempotency_key` and different payload is conflict.
- Same `binding_name` with different idempotency key is conflict.
- Same model gateway id and structurally equal payload returns existing gateway.
- Same model gateway id and different payload is conflict.
- Object member order does not affect structural equality.
- Array order affects structural equality.
- String differences, including whitespace, affect structural equality.
- Unknown fields in v1 payload are rejected.
- Disabled binding rejects Agent turn start.
- Disabled model gateway rejects turn start.
- Every `/chat/stream` path rejects missing `selected_model`, JSON null `selected_model`, string `selected_model`, missing `selected_model.model`, and empty `selected_model.model` before binding load, discovery, native model config lookup, or model gateway lookup.
- Empty `selected_model.gateway` is rejected when gateway is present.
- Native/default chat with `selected_model.model` and no `selected_model.gateway` resolves through Astra native model config.
- Native/default chat with unknown `selected_model.model` and no `selected_model.gateway` fails explicitly instead of using a server default model.
- Capability server `endpoint_url`, model gateway `resolve_url`, and model descriptor `invoke.url` with scheme problems, userinfo, fragment, or query are rejected.
- Binding with `type=model` capability server is rejected.
- Missing `type=mcp` or `type=skill` binding server is rejected.
- Missing runtime auth bearer fails before capability server calls when `agent_binding` is present and before model gateway calls when `selected_model.gateway` is present.
- Runtime auth `Debug` output redacts bearer values.

### Integration Tests

- Register model gateway, register binding, start `/chat/stream`, fake model gateway returns deterministic invocation descriptor.
- Plain chat with `selected_model.gateway` and without `agent_binding` resolves model through the selected model gateway.
- Native/default web chat sends `selected_model.model`, omits `selected_model.gateway`, uses Astra native model config, and does not require `runtime_auth.authorization`.
- Agent turn with `agent_binding`, `selected_model.model`, and no `selected_model.gateway` uses Astra native model config while still discovering tools/skills from the binding endpoints.
- Fake MCP `tools/list` result appears in LLM tool surface.
- Fake skill `skills/list` result appears in Astra skill surface.
- LLM tool call to `mcp__tools__query` forwards through selected MCP server with the same bearer.
- Model resolve failure returns structured error and does not start the agent loop.
- Discovery failure returns structured error and does not start the agent loop.
- Empty tool or skill discovery result under a valid bearer succeeds and does not synthesize substitute tools or skills.
- `agent_runs` records `agent_binding_id`, `agent_binding_name`, `agent_binding_schema_version`, `selected_model_json`, `selected_model_name`, `selected_model_gateway`, `capability_server_refs_json`, and `runtime_profile`.

### Security Regression Tests

- Runtime bearer value never appears in:
  - `agent_bindings`;
  - `model_gateways`;
  - `agent_run_events.payload_json`;
  - `agent_events.content`;
  - SSE events;
  - error response body except redacted placeholders.
- Request body cannot override capability server `endpoint_url` or `transport`.
- Request body cannot add unregistered capability servers in Agent Binding mode.
- Request body cannot select a model gateway through `agent_binding.capability_server_refs`.
- Request `allowed_tools` cannot broaden discovery result.
- Astra does not parse runtime bearer claims for authorization or model selection.
- Model descriptor is not cached as binding state and is not reused across runtime bearer values.

## Open Decisions

1. **Authentication for registry APIs**: use existing admin auth or introduce a dedicated runtime-provider integration token.
2. **Discovered skill representation**: decide the exact internal adapter from `skills/list` response into Astra active skill surface.
3. **Model invocation integration**: decide whether the per-turn model invocation descriptor is represented as a temporary provider config, a thin client, or a dedicated model gateway executor.
4. **Future capability server transports**: v1 supports `streamable_http` for binding-local MCP/skill endpoints. Direct external MCP/model/skill endpoints with different auth behavior need an explicit new transport/security model.

## Compatibility Contract

Three runtime compatibility paths exist and must be selected explicitly during migration:

```text
request_scoped_runtime_mcp
  Client sends runtime_mcp_bindings every turn.

native/default chat
  Client sends selected_model.model.
  Client omits agent_binding.
  Client may omit selected_model.gateway to use Astra native model config.
  Client does not need runtime_auth.authorization unless it sends selected_model.gateway.

agent_binding_registry
  Client registers binding once.
  Client sends selected_model.model every turn in this mode.
  Client may omit selected_model.gateway to use Astra native model config.
  Client registers and sends selected_model.gateway only when it wants model gateway resolution.
  Client sends runtime_auth.authorization whenever agent_binding is present or selected_model.gateway is present.
  Client sends agent_binding only for Agent turns in this mode.
  Client does not resend full prompt, capability server definitions, static runtime policy, model gateway definitions, or authorization lists.
```

The runtime must never treat a failed Agent Binding lookup, native model config lookup, model gateway lookup, model resolve failure, discovery failure, or runtime bearer rejection as permission to use `runtime_mcp_bindings`, plain chat, a fallback model, a fallback gateway, or no-tool execution. If `selected_model.gateway` is present and gateway resolution fails, Astra must not fall back to native model config. If `selected_model.gateway` is absent and native model config lookup fails, Astra must not fall back to a server default model.
