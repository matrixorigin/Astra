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
- `ASTRA_MYSQL_CLIENT_TLS_ARG` (optional) — one TLS option supported by the installed MySQL client. Leave unset to negotiate TLS when the client supports `--ssl-mode` (TLS when available, plaintext otherwise); clients without that option retain their default. Use an explicit option such as `--ssl-mode=REQUIRED` or `--skip-ssl` only when the endpoint has an explicit policy.
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

`ASTRA_API_PORT` defaults to `17001` across source, Docker API, and all-in-one
stack modes. In the all-in-one compose stack, this value controls the
host-facing published port; the API container listens on port `17001`.

### Auth secrets (REQUIRED in production)

- `ASTRA_JWT_SECRET`
- `ASTRA_JWT_ALGORITHM` (default `HS256`)
- `ASTRA_JWT_ACCESS_TTL_MINUTES` (default `10080` in code; production should override)
- `ASTRA_JWT_REFRESH_TTL_DAYS` (default `7`)
- `ASTRA_TOKEN_ENCRYPTION_KEY` (Fernet key)
- `ASTRA_BRIDGE_SECRET`

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

### LLM

LLM models are **not** configured via env vars. Use the admin CLI:

```bash
astra admin model add <name> <provider> --api-key ... --base-url ...
astra admin model check <name>                    # probe + activate
astra admin model list                            # see all configured models
astra admin config set reasoning_model_name <n>   # optional: pin the judge/summary model
```

If `reasoning_model_name` is not set, the server falls back to the cheapest active model by `pricing.completion`.

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
- `ASTRA_AUX_LLM_POLICY` — global policy for optional auxiliary LLM calls; `capacity_aware` by default skips optional auxiliary calls when provider admission is enabled, `always` preserves them, `disabled` turns them off
- `ASTRA_CAPTURE_TRACES`

Diagnostic DB history is controlled through `runtime.toml` trace categories, not separate environment variables. Production defaults keep high-volume diagnostic tables off; `trace.profile = "dev"` enables them. For custom profiles, enable `context_assembly` for context manifests, `prompt_assembly` for prompt request deltas, and `harness_snapshots` for durable harness snapshot history.

Provider admission is intentionally configured by capacity inputs only. Scope is fixed at provider level; window size, retention, cleanup cadence, burst, and fail-closed behavior are internal runtime policy rather than deployment knobs.

Server-loop Memoria observer and post-loop memory cleanup are fixed internal async best-effort side effects with bounded in-process concurrency. They are intentionally not environment-configurable; they must not hold run admission slots or become deployment-specific tuning surfaces.

### Observability

- `ASTRA_LOG_FORMAT`, `ASTRA_SERVICE_NAME`, `ASTRA_OTEL_ENABLED`

### CLI overrides (optional)

- `ASTRA_CLI_USER_ID`, `ASTRA_CLI_SESSION_ID`, `ASTRA_CLI_SESSION_NAME`
- `ASTRA_CLI_AUTO_APPROVE`, `ASTRA_CLI_MAX_TURNS`
- `ASTRA_CLI_ALLOWED_TOOLS`, `ASTRA_CLI_DISALLOWED_TOOLS`, `ASTRA_CLI_ADD_DIRS`
- `ASTRA_CLI_CREDENTIALS_DIR`

### Edge / multi-agent (optional)

- `ASTRA_EDGE_REGISTRY`, `ASTRA_EDGE_HEARTBEAT_SECS`, `ASTRA_EDGE_EXECUTOR_ID`, `ASTRA_EDGE_AGENT_ID`

### Testing

- `ASTRA_TEST_DB_IT`, `ASTRA_TEST_DB_IT_TEST_THREADS`, `ASTRA_TEST_BRIDGE_SECRET`
- `ASTRA_TEST_PROMPT_CACHE_DISABLED`, `ASTRA_TEST_DB_URL`
- `ASTRA_TEST_SDK_E2E`, `ASTRA_TEST_SDK_ONLINE_E2E`, `ASTRA_TEST_SDK_BASE_URL`

## Validation

```bash
make dev-init
make check
```
