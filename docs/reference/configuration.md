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
- `ASTRA_TUI_THEME` (optional) — terminal rendering profile: `auto` (default), `dark`, `light`, `dark-ansi`, `light-ansi`, or `plain`. Use an ANSI profile for terminals or multiplexers that do not reliably render truecolor; `NO_COLOR=1` also selects `plain`. Welcome-panel accents, conversation surfaces, syntax highlighting, and the exit resume hint follow the selected profile. CLI text written to stderr uses stderr's color capability, independently of redirected stdout.
  Auto queries the terminal foreground/background once at startup with OSC 10/11 on interactive macOS/Linux terminals, sharing a 300 ms response budget with the existing capability query.
  An explicit theme, `NO_COLOR`, or a valid `ASTRA_TERMINAL_BG` skips color queries; redirected input/output and `TERM=dumb` are not queried. Color precedence is per channel: `ASTRA_TERMINAL_FG`/`ASTRA_TERMINAL_BG`, then the queried color, then `COLORFGBG`. A background response alone is sufficient. A valid manual background intentionally skips both color queries because theme selection only needs the background; the foreground then uses `ASTRA_TERMINAL_FG`, `COLORFGBG`, or the terminal default. Setting only a foreground still allows querying the background. When the background is unknown, ordinary text and surfaces inherit terminal defaults while headings, the logo, status, and syntax keep terminal-defined ANSI colours. Unknown background does not select `plain`. Other platforms retain environment hints and explicit profiles. The selected theme stays fixed for the session; restart Astra after changing the terminal background. During the bounded startup query, a split Esc prefix may wait up to 40 ms; normal-session Esc handling is unchanged. A malformed, unterminated color response is discarded until its terminator or a new Esc sequence; press Esc to recover input if the terminator never arrives. Sixel query timeouts remain unknown, and late capability replies are consumed by the existing reader without a second terminal query.
- `ASTRA_TUI_GLYPHS` (optional) — `unicode` (default) or `ascii`. Select `ascii` for terminals or fonts that do not reliably render box-drawing and state glyphs; all state labels and actions remain available.
- `ASTRA_DATABASE` — logical database name
- `ASTRA_DATABASE_PREFIX` (optional) — effective name = `{PREFIX}{ASTRA_DATABASE}`
- `ASTRA_AUTO_CREATE_DATABASE` — `1` to auto-create database at startup
- `ASTRA_DATABASE_BOOTSTRAP_CATALOG` — catalog used for the auto-create step (default `mysql`)

### Application

- `ASTRA_ALLOW_INSECURE_DEFAULTS` — dev-only opt-in for bundled defaults on required keys
- `RUST_LOG` — standard tracing filter (e.g. `warn,astra_runtime=info`)
- `NO_COLOR` — when present, disables styles in CLI streaming Markdown, including code blocks and tables. `astra chat --no-color` also enables this behavior. Without it, Markdown bold, italic, and headings inherit the terminal foreground and use emphasis attributes so they remain readable on light and dark backgrounds.

### API server

- `ASTRA_API_HOST`, `ASTRA_API_PORT`, `ASTRA_CORS_ORIGINS`
- `ASTRA_POD_ID` — unique durable execution identity for this Server instance. Set a stable identity when its managed workspace directory survives restarts; never share one identity between concurrently running instances. Without it, the process uses a fresh UUID and cannot adopt a previous instance's local workspace.
- `ASTRA_SERVER_WORKSPACES` — base directory for the selected Server sandbox provider, captured when the lifecycle is constructed; defaults to the system temporary directory's `astra-workspaces` child. A persisted Server sandbox belongs to its recorded executor and exact session directory, not to any instance that can read the same database row.


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
astra admin model add <name> <provider> --api-key ... --context-window 128000 --base-url ...
astra admin model check <name>                    # probe + activate
astra admin model list                            # drains the authoritative paginated catalog
astra admin config set reasoning_offering_id <id> # optional: pin the judge/summary Offering
```

If `reasoning_offering_id` is not set, the server applies its governed default and currently selects the cheapest active Offering by `pricing.completion`. `astra admin model list` follows the server's seek-paginated catalog until completion; model names do not select execution routes, and clients must use the exact Offering ID from that complete projection.

### Memoria

- `MEMORIA_BASE_URL`, `MEMORIA_MASTER_KEY` — Memoria endpoint and deployment master secret. Configuring the secret alone does not grant end-user memory access.
- `MEMORIA_SELF_HOSTED_MASTER_ACCESS` — exact value `1` explicitly allows active local password accounts with no scoped binding or retained Memoria identity to use owner-scoped master authentication when `MEMORIA_WEB_URL` is unset. Existing scoped owner/consent always wins; disconnect, inactive/deleted accounts and lookup errors never fall back. Requires a Memoria release containing `matrixorigin/Memoria#250` (available in 0.5.2, not 0.5.1). The Server default remains disabled; the self-hosted all-in-one example explicitly enables it with a compatible pinned image. Existing env files are not automatically upgraded.
- `MEMORIA_ISSUER` — stable identity issuer URL; defaults to normalized `MEMORIA_BASE_URL`. Changing the issuer creates a different identity namespace. Keep it stable when changing only the service transport address.
- `MEMORIA_WEB_URL` — Server-owned browser sign-in website, advertised through `GET /auth/methods`. Unset preserves password login. Requires HTTPS except for explicit loopback development URLs. The CLI does not read this environment variable.
- `MEMORIA_EMBEDDING_PROVIDER`, `MEMORIA_EMBEDDING_MODEL`, `MEMORIA_EMBEDDING_DIM`, `MEMORIA_EMBEDDING_API_KEY`, `MEMORIA_EMBEDDING_BASE_URL`

Scoped credentials drive login, refresh, memory proxy, explicit tools, recall, extraction and session-end governance. Self-hosted master access is an explicit per-user fallback on those same paths, never a replacement for a scoped binding or failed lookup. Runtime builders receive this policy from composition rather than independently reading environment variables. See [authentication](../design/authentication.md).

### Runtime tuning (optional)

`tool_policy` owns workflow guard limits and model profiles for root runs,
children, skill runs and automatic model changes. The duplicate `tool_selection`
configuration section is retired. The default per-round tool limit is 100;
built-in Opus, Sonnet-4, Haiku and GPT-5 profiles use 128, 100, 48 and 128.
Explicit `tool_policy.model_profiles` take precedence over built-in profiles.
Each execution selects its tool policy once. Subsequent rounds and automatic
model changes use that selected policy; edits apply to newly admitted executions.
The execution circuit breaker uses the same admitted policy, including its
resolved default thresholds and absolute round limit. Reassembling an active
execution preserves its existing breaker observations.
Child and skill executions select their own policy. This does not change the
process-level selection of their Server execution-round ceilings.


Runtime configuration exposes controls consumed by execution: compression,
retrieval, tool policy and tracing. The retired `verification`,
`memory_pressure`, `context_window` and `token_budget` sections are not supported.
Execution input budgets belong to RuntimeLimits (`ASTRA_MAX_TURN_INPUT_TOKENS`)
and the admitted model context window; self budget views report observed budget
state instead of an inactive configuration cap. Model context
window metadata and tool verification contracts retain their existing owners.

Unknown top-level runtime fields are rejected by the configuration parser.
User and project files, environment values and CLI settings apply in that order.
The configuration editor saves user defaults under `ASTRA_LOCAL_STATE_ROOT/config/runtime.toml`
when that root is set, otherwise under `~/.astra/config/runtime.toml`; local versions
live in the adjacent `versions` directory. Saving defaults retains invocation CLI settings
and any complete configuration snapshot belonging to the current session. The current
session version identifies its effective configuration, not the saved defaults file.
Server execution-round limits are configured on the Server; the CLI `/config`
editor does not expose controls for the remote Server’s `runtime_limits`.
File and JSON layers retain only explicitly supplied fields: missing fields preserve
lower layers, while defaults, `false`, zero and empty arrays replace them. JSON `null`
clears optional values and is rejected for non-optional fields. When a layer supplies a model-routing policy, it must declare it completely. CLI trace flags take precedence
over `--settings`; level/category flags preserve unspecified trace fields, while
production/dev profile flags select their complete presets. Compression presets and
trace normalization still run after configuration selection. Non-finite environment
compression thresholds are rejected with a warning and preserve the configured value;
an invalid environment value cannot discard explicit CLI settings.
`--settings` reports the parse error. A saved session configuration is a complete snapshot, not an overlay: restoring
it replaces the execution configuration before deriving context budgets and the observability projection,
including values equal to built-in defaults. The configuration version identifies the effective
snapshot; an explicit `/explain --format` choice for the current CLI session retains precedence.
Starting a new conversation with `/clear` selects the current process configuration,
rather than inheriting a restored session snapshot. Explicit CLI Explain preferences and the
selected model remain in effect; budgets, configuration version and observability are derived again.
Cold startup, new conversations, no-snapshot recovery and telemetry select profile identity from the
account identity installed at entry, not ingestion metadata. An account without stored
preferences uses its own default profile; only a CLI without an installed account identity uses
anonymous preferences.
`astra self mutate preview/apply` uses the process configuration when no complete
session snapshot exists. Applying one setting preserves the other snapshot values.
A snapshot is cleared only when the complete result equals the process baseline.
Profile identity, preferences and statistics do not override runtime configuration.
Preview reports failed configuration checks without writing. Apply rejects an
invalid candidate before changing the snapshot, revision or journal. A valid
candidate may repair an existing invalid configuration.
Authentication changes select preferences for the verified target account after credentials are saved;
re-authenticating the same account without resetting its conversation retains that session configuration.
A saved snapshot must parse and satisfy the current invariants before resume changes the active session; invalid
snapshots remain unchanged and are not migrated or filtered. This also rejects
full snapshots that contain the retired sections, even if their values were defaults.
The existing disk
configuration loader reports a warning and skips an invalid user/project layer.


- `ASTRA_MAX_TURNS` — optional positive ordinary execution-round cap. Bounded settlement/closing allowances remain separate, so this is not an absolute cap on all model calls or cost. Unset means renewable slices without an implicit round cap; it does not disable cancellation, execution-health checks, or individual operation timeouts.
- `ASTRA_PLAN_SUBTASK_MAX_TURNS` — optional positive plan-subtask cap; unset inherits `ASTRA_MAX_TURNS`. Explicit zero or malformed round caps are rejected, not treated as unlimited.
- `ASTRA_TURN_TIMEOUT_S`
- `ASTRA_GLOBAL_OUTPUT_LIMIT`, `ASTRA_TOOL_OUTPUT_LIMIT`
- `ASTRA_MAX_TOOL_RETRIES`, `ASTRA_RETRY_BASE_MS`
- `ASTRA_MAX_RETRIEVED`, `ASTRA_MAX_HISTORY_TOKENS`, `ASTRA_COMPRESSION_THRESHOLD`
- `ASTRA_RETRIEVAL_TOP_K`, `ASTRA_MAX_TURN_INPUT_TOKENS`
- `ASTRA_LLM_PROVIDER_ADMISSION_MODE` — provider admission mode; unset/`disabled` by default, `db_fixed_window` enables MatrixOne-backed RPM/TPM claims before outbound LLM attempts
- `ASTRA_LLM_PROVIDER_ADMISSION_RPM`, `ASTRA_LLM_PROVIDER_ADMISSION_TPM` — provider budget used by admission; at least one is required when admission is enabled
- `ASTRA_LLM_CONNECT_TIMEOUT_S`, `ASTRA_LLM_NONSTREAM_TIMEOUT_S`, `ASTRA_LLM_TOTAL_BUDGET_S`, `ASTRA_LLM_ACTION_PROGRESS_TIMEOUT_S` — provider transport/progress bounds. The `300s` total-budget default is per provider call including retries, not an end-to-end session limit; turn profiles and resource policy still bound the overall run. Interactive resource policy is 30s for a single tool execution, while long-session profiles explicitly allow 300s.
- `ASTRA_AUX_LLM_POLICY` — policy for bounded auxiliary LLM calls. When unset, Astra uses `capacity_aware`: Work admission is evaluated at an existing typed effect/topology boundary, where the result has an immediate admission consumer; ordinary unbound primary turns stay on the single-request path, while unrelated optional judges remain capacity-gated. `boundary_only` keeps the same Work boundary but also suppresses unrelated optional auxiliary judges. An unavailable auxiliary decision is recorded as typed degradation and does not discard a primary response that already passed the canonical tool/lifecycle boundary; an explicitly `disabled` policy under Auto still fails closed before action or completion, and a client that deliberately omits classification must explicitly request `FixedDefault`. Set `always` only when a deployment deliberately wants speculative Work admission on ordinary turns and all eligible auxiliary calls regardless of capacity policy.
- `ASTRA_CAPTURE_TRACES`
- `ASTRA_RUN_CONCURRENCY_LIMIT` — positive agentic loop slots per Astra Server process. For a multi-server deployment, set `ASTRA_CAPACITY_POD_COUNT` to the number of equivalent server processes sharing the same durable admission scope; the cross-pod weighted budget is derived from both values and is fenced when a server presents a different capacity snapshot. Change the declared budget only after active reservations have drained, and use one snapshot across all participating servers.
- `ASTRA_CAPACITY_POD_COUNT` — positive server process count used for the cluster capacity model and durable canonical-turn admission. This is an operator declaration, not service discovery; keep it identical across pods that share a database.

### Explain Analyze presentation

The TUI keeps the live Explain Analyze tree in a compact status lane so it
does not hide the conversation. Configure the row budget in
`~/.astra/config/runtime.toml` (or the project override):

```toml
[explain]
live_rows = 5 # 1–5, default 5
report_format = "html" # html, markdown, or text; default html
```

The same setting is available in the `/config` editor as **Live Explain
Analyze rows (1–5)** and takes effect for the next live capture immediately.
The settled Explain cell and the local report are not truncated by this
live-row setting. HTML is the default because it keeps the report readable
across terminals and browsers; `/explain --format markdown` and
`/explain --format text` select a different derived companion for the next
capture.

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

Saved configuration versions are inspected locally with `astra config version`
(`list`, `show`, `diff`, and `current`). There is no configuration cloud-pull
command; Server ingestion retains owner-scoped version evidence.

The local version index uses one JSON record per nonempty line. Inspection
reports a corrupt row instead of returning a partially decoded index.
