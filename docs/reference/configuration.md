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
- `NO_COLOR` — when present (including an empty value), disables styles in CLI tool progress and streaming Markdown, including status icons, descriptions, durations, summaries, spinner text, code blocks and tables. Cursor movement and row-clearing commands retain their existing behavior. `astra chat --no-color` also enables this behavior. Without it, Markdown bold, italic, and headings inherit the terminal foreground and use emphasis attributes so they remain readable on light and dark backgrounds.

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

### ChatGPT subscription connections

`ASTRA_CHATGPT_SUBSCRIPTION_ENABLED=1` enables explicit per-user ChatGPT
subscription connections. The default is disabled. Enabling also requires
`ASTRA_CHATGPT_HOST_ID=urn:uuid:<UUID>`: generate a UUID once per self-hosted
installation, retain it across restarts, and share it only with replicas of
that same installation on the same host. A different host ID requires explicit
reauthorization. Stable `ASTRA_TOKEN_ENCRYPTION_KEY` and MatrixOne are required;
back up this key through the deployment's secret-management process.

This initial integration targets personal self-hosted installations. Hosted
commercial offerings, multi-host installations and device-side inference are
outside this version. It uses OpenAI's public Sign in with ChatGPT dynamic
registration and direct Responses API, with fixed trusted origins. No provider
client secret or Codex authentication file is configured.

Upgrade every Server replica to this source before enabling, then follow
[CLI linking](cli-commands.md#chatgpt-subscription-models). Startup adds two
owner-scoped tables and a nullable connection reference to personal models;
existing API-key model rows remain valid. Do not run older and newer binaries
with subscription models in the same database. Disable the flag on every
replica and restart to stop subscription execution. Disconnect and remove
subscription model rows before rolling back to an older binary, which expects
an API key on every personal model. Retain session history and the encryption
key. Disabling the flag alone is a behavior rollback for the new binary, not
an older-binary schema compatibility guarantee.

### CLI native collaborator capacity

Ordinary CLI and TUI turns use the selected Astra model and the existing local
workspace tools. They do not automatically discover or attach an external native
agent runtime merely because its executable is installed.

Set `ASTRA_NATIVE_COLLABORATORS=true` (or `1`) in the CLI process to explicitly
select native collaborator capacity. It must become ready before the turn is
admitted; unavailable or timed-out preparation is an error, rather than silently
submitting a turn against a withdrawn Edge registration. Default is `false`;
`false`, `0`, or an empty value disable it. Other values are rejected.
This does not enable native tool execution or bypass its invocation-time
permission, sandbox, account, or Session attachment checks.

### Outbound HTTPS trust

Astra HTTP clients using the workspace reqwest transport trust both bundled
public CA roots and the process platform trust store. This supports managed
private CAs, including certificates used by an HTTPS inspection proxy, while
retaining certificate-chain and hostname verification and existing proxy routing.

For a process-specific CA configuration, set `SSL_CERT_FILE` to a PEM CA bundle
and/or `SSL_CERT_DIR` to a directory of PEM CA files before starting Astra. These
variables select the native-root source instead of platform discovery; bundled
public roots remain available. Configure them only in the selected CLI, User
Runner, or Server process environment. They do not transfer a user machine’s
trust store to the Server. Restart the process after changing its trust settings.

### Auth secrets (REQUIRED in production)

- `ASTRA_JWT_SECRET`
- `ASTRA_JWT_ALGORITHM` (default `HS256`)
- `ASTRA_JWT_ACCESS_TTL_MINUTES` (default `10080` in code; production should override)
- `ASTRA_JWT_REFRESH_TTL_DAYS` (code default `30`; `.env.example` selects `7`)
- `ASTRA_TOKEN_ENCRYPTION_KEY` (shared high-entropy secret for stored provider and browser session credentials; changing it makes provider credentials undecryptable and requires browser sign-in again)
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
`memory_pressure`, `context_window`, `token_budget` and `budget_policy` sections
are not supported. Execution capacity uses the admitted limits and canonical
adaptive renewal; the retired outcome-streak policy never controlled it.
Execution input budgets belong to RuntimeLimits (`ASTRA_MAX_TURN_INPUT_TOKENS`)
and the admitted model context window; self budget views report observed budget
state instead of an inactive configuration cap. Model context
window metadata and tool verification contracts retain their existing owners.

Unknown top-level runtime fields are rejected by the configuration parser.
User and project files, environment values and CLI settings apply in that order.
Local cache and credential-directory overrides do not relocate the canonical
physical-device identity. Local CLI/Edge execution requires
`~/.astra/edge-materializations/device.id` and its canonical checkout identity
files under the default home state root. Initialize Astra on the same device
with that directory writable before running in a restricted home environment;
retain the canonical identity files alongside any isolated cache. A writable
`ASTRA_LOCAL_STATE_ROOT` or credential directory alone is insufficient. The
existing identity preflight reports this prerequisite before turn admission;
do not delete identity files or login credentials as a repair step.

`ASTRA_LOCAL_STATE_ROOT` relocates user-level state, including the user
configuration, permissions and workspace trust, so it must be set in the
launching shell. The CLI exits with an error when a workspace `.env` supplies it,
as it does for `MOI_AUTH_DIR`. The same applies to `HOME`, the fallback root for
all of these state locations, when the launching environment does not set it.

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
- `ASTRA_LLM_CONNECT_TIMEOUT_S`, `ASTRA_LLM_NONSTREAM_TIMEOUT_S` — connection and non-stream request timeouts. Non-stream and auxiliary calls remain bounded.
- `ASTRA_LLM_SEMANTIC_PROGRESS_TIMEOUT_S` — rolling stream progress timeout, default `300s`. Non-whitespace text, reasoning, and advancing authorized tool arguments refresh it; SSE heartbeats and empty deltas do not. Independent per-read idle and response-size limits remain active.
- `ASTRA_LLM_TOTAL_BUDGET_S` — optional total budget per logical streaming call, including retries. Unset, empty, or whitespace-only means no deployment cap; nonempty values must be positive integer seconds within the supported deadline range, otherwise streaming calls fail with an actionable `InvalidRequest` before attempt admission. Ordinary agent streams have no default elapsed-time cap: a progressing response can continue beyond five minutes. A valid deployment override or explicit caller budget enables the hard cap and its durable-terminalization reserve (up to 10s). Without a total cap, request startup/retries and each ledger operation remain bounded by 300s; response-start configuration can impose a shorter header timeout without capping the streaming body. User cancellation and explicit run/resource deadlines still apply. Interactive resource policy is 30s for a single tool execution, while long-session profiles explicitly allow 300s.
- `ASTRA_AUX_LLM_POLICY` — policy for bounded auxiliary LLM calls. Unset, empty or unknown values use `boundary_only`: Work admission is evaluated at an existing typed effect/topology boundary, where the result has an immediate admission consumer. Ordinary chat skips optional user observation, skill auto-routing and proactive pre-turn LLM compaction; explicit skill execution, inherited restrictions and reactive context-pressure handling remain available. Explicit `capacity_aware` keeps the same Work boundary and enables capacity-gated optional auxiliaries. An unavailable auxiliary decision is recorded as typed degradation and does not discard a primary response that already passed the canonical tool/lifecycle boundary; an explicitly `disabled` policy under Auto still fails closed before action or completion, and a client that deliberately omits classification must explicitly request `FixedDefault`. Set `always` only when a deployment deliberately wants speculative Work admission on ordinary turns and all eligible auxiliary calls regardless of capacity policy.
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

### Local `web_fetch` behind fake-IP DNS

Edge/CLI `web_fetch` and `web_search` block destinations that resolve to a
known private or reserved address, including `198.18.0.0/15`. Local VPN/proxy tools
in fake-IP mode (Clash/mihomo, Surge, sing-box) answer every lookup from that
range, so every fetch is blocked. Users on such a network can relax the
private-network boundary for fake-IP answers in the user-level
`~/.astra/config/runtime.toml`:

```toml
[web_fetch]
fake_ip = "allow_doh_screened"                   # block (default) or allow_doh_screened
doh_resolvers = ["https://1.12.12.12/dns-query"] # required, tried in order
# fake_ip_ranges = ["198.18.0.0/16"]             # optional, within 198.18.0.0/15
```

This is a relaxation, not a verification. When every address from the local
DNS lies in an IPv4 fake range, the fetch goes to the fake address and the VPN
resolves the hostname and picks the real destination; Astra cannot see or
check it. Before allowing the fetch, Astra screens the hostname by asking the
resolvers (RFC 8484) for A and AAAA records and blocks it if any falls in
Astra's blocked address ranges. The list is not an exhaustive registry of all
special-purpose addresses; broader classification is tracked in #1140.
Resolvers are tried in order when one is unreachable, gives
an unusable response, or returns no address. Literal fake addresses and hosts
with no usable resolver answer stay blocked, and a private address reported for either
record type blocks the fetch even if the other query failed. There is no
built-in resolver: pick one your network does not block, because many networks
that use fake-IP DNS also block well-known DoH endpoints. Resolver URLs must
not contain userinfo; paths and query parameters may contain sensitive values,
and errors name a resolver only by its position and host.
The `0.0.0.0/8` this-network range is also blocked, including when embedded in
IPv4-mapped or NAT64 IPv6 addresses.

Only IPv4 fake answers are supported. IPv6 fake-IP ranges (for example
`2001:2::/48`, used in sing-box's documented configuration) and answers that mix
fake addresses with real ones, such as an IPv4 fake address next to a public
IPv6 address, are rejected with an "unsupported" error whether or not the option
is enabled. Configure the VPN to answer with IPv4 fake addresses only if you
need this option.

Only the user-level file and an explicit `--settings` overlay can set this
section. A project `.astra/config/runtime.toml` cannot; the section is ignored
with a warning, and the config editor omits it when saving project defaults.
If the project and user file are the same file, save as user defaults instead.
User defaults and local config-version blobs are written with private file
permissions on Unix. There is intentionally no environment variable, because the CLI
loads `.env` from the working directory. For the same reason a workspace `.env`
cannot set `ASTRA_LOCAL_STATE_ROOT` or a missing `HOME`, which locate the
user-level file. Server
`web_fetch` never reads the section. An invalid resolver or fake-IP range
keeps the default block and reports the configuration error when a fake-IP
host is fetched. A syntactically invalid user file is rejected as a whole;
use `--diagnostic-log` to see the configuration-loader warning.

Screening does not bind the VPN's destination. A hostname that answers the
resolver with a public address and the VPN's DNS with a private one (DNS
rebinding by an attacker-controlled domain, or split-horizon DNS) passes the
screen, and the VPN then connects to the private address. With this option on,
Edge/CLI `web_fetch` therefore no longer guarantees that requests stay off
private networks for hosts answered with fake addresses; it relies on the VPN's
routing instead. Keep it at `block` unless fake-IP DNS blocks web access. Like other Astra
configuration, the file is protected from agent writes only by the permission
mode: Bypass, or Auto with `allow_sensitive_path_writes`, lets the agent edit it.

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
command. Configuration changes retain their version IDs in the canonical
journal; full configuration bodies remain in the local version store. The old
cloud body mirror and its dedicated ingestion protocol are retired.

The local version index uses one JSON record per nonempty line. Inspection
reports a corrupt row instead of returning a partially decoded index.
