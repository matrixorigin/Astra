# Configuration Reference

## Source of Truth

Use these files as the canonical configuration references:

- `.env.example` (development)
- `.env.production.example` (production)
- `deployment/all-in-one/.env.example`
- `crates/core/src/config.rs` — `AppSettings`, all struct loaders

## Core Variables

### Database: MatrixOne

- `MATRIXONE_HOST`, `MATRIXONE_PORT`, `MATRIXONE_USER`, `MATRIXONE_PASSWORD`
- `ASTRA_MYSQL_TLS_MODE` (optional) — endpoint TLS policy used by `scripts/dev/mysql-client.sh`: `auto` (default; local endpoints may probe then fall back to plaintext, remote endpoints require TLS), `required`, or `disabled`. The adapter selects the supported MySQL/MariaDB client flag; callers should not pass client-specific `--ssl-mode`/`--skip-ssl` values through configuration.
- `ASTRA_TUI_THEME` (optional) — terminal rendering profile: `auto` (default), `dark`, `light`, `dark-ansi`, `light-ansi`, or `plain`. Use an ANSI profile for terminals or multiplexers that do not reliably render truecolor; `NO_COLOR=1` also selects `plain`.
- `ASTRA_TUI_GLYPHS` (optional) — `unicode` (default) or `ascii`. Select `ascii` for terminals or fonts that do not reliably render box-drawing and state glyphs; all state labels and actions remain available.
- `ASTRA_DATABASE` — logical database name
- `ASTRA_DATABASE_PREFIX` (optional) — effective name = `{PREFIX}{ASTRA_DATABASE}`
- `ASTRA_AUTO_CREATE_DATABASE` — `1` to auto-create database at startup
- `ASTRA_DATABASE_BOOTSTRAP_CATALOG` — catalog used for the auto-create step (default `mysql`)

### Application

- `ASTRA_ALLOW_INSECURE_DEFAULTS` — dev-only opt-in for bundled defaults on required keys
- `RUST_LOG` — standard tracing filter (e.g. `warn,astra_runtime=info`)

### API server

- `ASTRA_API_HOST`, `ASTRA_API_PORT`, `ASTRA_CORS_ORIGINS`

`ASTRA_API_HOST` defaults to `127.0.0.1`. Container deployments set
`0.0.0.0` explicitly inside the container and control external exposure at the
Compose, Kubernetes Service, or ingress boundary.

`ASTRA_API_PORT` defaults to `17001` across source, Docker API, and all-in-one
stack modes. In the all-in-one compose stack, this value controls the
host-facing published port; the API container listens on port `17001`.

Server-hosted optional capacity is declared structurally in `server.toml`, not
with one environment variable per tool. Public-network tools are unavailable
on the server unless its provider explicitly declares outbound network capacity:

```toml
[deployment.provider_capabilities]
server-builtin = ["public_network"]
```

Credential-backed connectors additionally require the generic
`credential_broker` provider capability. This declares a usable credential
resolution boundary; it still does not enable any connector for a user turn:

```toml
[deployment.provider_capabilities]
server-builtin = ["public_network", "credential_broker"]
```

Without this declaration, `web_search` and `web_fetch` can still become
available through a ready bound Edge provider. Capability availability does
not enable either tool for a user turn; Web and SDK clients must explicitly
select optional tools separately.

### Auth secrets (REQUIRED in production)

- `ASTRA_JWT_SECRET`
- `ASTRA_JWT_ALGORITHM` (default `HS256`)
- `ASTRA_JWT_ACCESS_TTL_MINUTES` (default `10080` in code; production should override)
- `ASTRA_JWT_REFRESH_TTL_DAYS` (default `7`)
- `ASTRA_TOKEN_ENCRYPTION_KEY` (high-entropy secret from which Astra derives Fernet encryption; changing it makes existing provider credentials undecryptable)
- `ASTRA_RUNTIME_ROOT_SECRET`

### Provider Request Auth

Provider-originated service requests are authenticated under `auth.provider_request_auth` in
`server.toml`. Astra validates these request tokens locally; it does not call a provider callback
endpoint during request admission.

```toml
[[auth.provider_request_auth]]
provider = "moi"
type = "hmac"
key = "${ASTRA_PROVIDER_HMAC_KEY}"
```

For MOI, `ASTRA_PROVIDER_HMAC_KEY` is an unpadded base64url text secret derived
by MOI deployment tooling. Astra uses the configured string's UTF-8 bytes
directly as the provider request HMAC key; it does not base64url-decode the
string before verifying request tokens.

### Edge Token Auth

MOI edge-registration tokens (`moi-user-token-v1.*`) presented by sandbox/runner
edge agents are verified locally under `auth.edge_token_auth` in `server.toml`
using a shared HMAC key (the MOI `jwt_secret`). Whenever `key` is configured,
`check_endpoint` is **required** — config validation rejects a key without one so
revocation can never be silently skipped. Astra then performs a jti revocation
check against moi-core on **every surface that
accepts an edge token** — the edge WebSocket connect and every HTTP request —
with a 30-second positive-only cache per jti (denials and check-endpoint
outages are never cached; both fail closed). Worst-case revocation propagation
on astra surfaces is therefore ≤ 30 seconds.

```toml
[auth.edge_token_auth]
key = "${ASTRA_EDGE_TOKEN_HMAC_KEY}"
check_endpoint = "http://moi-catalog:8081/api/v1/astra/edge-tokens/check"
```

### LLM

LLM models are **not** configured via env vars. Use the admin CLI:

```bash
astra admin model add <name> <provider> --api-key ... --base-url ...
astra admin model check <name>                    # probe + activate
astra admin model list                            # drains the authoritative paginated catalog
astra admin config set reasoning_offering_id <id> # optional: pin the judge/summary Offering
```

If `reasoning_offering_id` is not set, the server applies its governed default and currently selects the cheapest active Offering by `pricing.completion`. `astra admin model list` follows the server's seek-paginated catalog until completion; model names do not select execution routes, and clients must use the exact Offering ID from that complete projection.

### Memoria

- `MEMORIA_BASE_URL`, `MEMORIA_MASTER_KEY`
- `MEMORIA_EMBEDDING_PROVIDER`, `MEMORIA_EMBEDDING_MODEL`, `MEMORIA_EMBEDDING_DIM`, `MEMORIA_EMBEDDING_API_KEY`, `MEMORIA_EMBEDDING_BASE_URL`

### Runtime tuning (optional)

- `ASTRA_MAX_TURNS`, `ASTRA_PLAN_SUBTASK_MAX_TURNS`, `ASTRA_TURN_TIMEOUT_S`
- `ASTRA_GLOBAL_OUTPUT_LIMIT`, `ASTRA_TOOL_OUTPUT_LIMIT`
- `ASTRA_MAX_TOOL_RETRIES`, `ASTRA_RETRY_BASE_MS`
- `ASTRA_MAX_RETRIEVED`, `ASTRA_MAX_HISTORY_TOKENS`, `ASTRA_COMPRESSION_THRESHOLD`
- `ASTRA_RETRIEVAL_TOP_K`, `ASTRA_MAX_TURN_INPUT_TOKENS`
- `ASTRA_LLM_PROVIDER_ADMISSION_MODE` — provider admission mode; unset/`disabled` by default, `db_fixed_window` enables MatrixOne-backed RPM/TPM claims before outbound LLM attempts
- `ASTRA_LLM_PROVIDER_ADMISSION_RPM`, `ASTRA_LLM_PROVIDER_ADMISSION_TPM` — provider budget used by admission; at least one is required when admission is enabled
- `ASTRA_LLM_CONNECT_TIMEOUT_S`, `ASTRA_LLM_NONSTREAM_TIMEOUT_S`, `ASTRA_LLM_TOTAL_BUDGET_S`, `ASTRA_LLM_ACTION_PROGRESS_TIMEOUT_S` — provider transport/progress bounds. The `300s` total-budget default is per provider call including retries, not an end-to-end session limit; turn profiles and resource policy still bound the overall run. Interactive resource policy is 30s for a single tool execution, while long-session profiles explicitly allow 300s.
- `ASTRA_AUX_LLM_POLICY` — policy for bounded auxiliary LLM calls. When unset, Astra uses `capacity_aware`: every eligible primary turn receives one bounded Work-admission decision, while provider admission accounts for its quota like any other inference; unrelated optional judges remain capacity-gated. Set `boundary_only` when a deployment deliberately prefers the single-request fast path, `disabled` to remove every auxiliary call (and accept typed-topology fallback), or `always` to require all eligible auxiliary calls regardless of capacity policy.
- `ASTRA_CAPTURE_TRACES`

Diagnostic DB history is controlled through `runtime.toml` trace categories, not separate environment variables. Production defaults keep high-volume diagnostic tables off; `trace.profile = "dev"` enables them. For custom profiles, enable `context_assembly` for context manifests, `prompt_assembly` for prompt request deltas, and `harness_snapshots` for durable harness snapshot history.

Provider admission is intentionally configured by capacity inputs only. Scope is fixed at provider level; window size, retention, cleanup cadence, burst, and fail-closed behavior are internal runtime policy rather than deployment knobs.

Server-loop Memoria observer and post-loop memory cleanup are fixed internal async best-effort side effects with bounded in-process concurrency. They are intentionally not environment-configurable; they must not hold run admission slots or become deployment-specific tuning surfaces.

### Observability

- `ASTRA_LOG_FORMAT`, `ASTRA_SERVICE_NAME`, `ASTRA_OTEL_ENABLED`

### CLI overrides (optional)

- `ASTRA_CLI_SESSION_ID`, `ASTRA_CLI_SESSION_NAME`
- `ASTRA_CLI_AUTO_APPROVE`
- `ASTRA_CLI_ALLOWED_TOOLS`, `ASTRA_CLI_DISALLOWED_TOOLS`, `ASTRA_CLI_ADD_DIRS`
- `ASTRA_CLI_CREDENTIALS_DIR`

### Edge / multi-agent (optional)

- `ASTRA_EDGE_REGISTRY`, `ASTRA_EDGE_HEARTBEAT_SECS`, `ASTRA_EDGE_EXECUTOR_ID`, `ASTRA_EDGE_AGENT_ID`

### Testing

- `ASTRA_TEST_DB_IT`, `ASTRA_TEST_DB_IT_TEST_THREADS`, `ASTRA_TEST_E2E_SECRET`
- `ASTRA_TEST_PROMPT_CACHE_DISABLED`, `ASTRA_TEST_DB_URL`
- `ASTRA_TEST_SDK_E2E`, `ASTRA_TEST_SDK_ONLINE_E2E`, `ASTRA_TEST_SDK_BASE_URL`

## Validation

```bash
make dev-init
make check
```
