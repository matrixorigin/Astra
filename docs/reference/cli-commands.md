# CLI Commands Reference

Current Rust CLI reference for the single `astra` CLI, including `astra admin`.

## Personal BYOK model setup

Run `astra model add` after logging in to configure a personal model interactively.

- **Model ID from your provider** is the exact identifier documented by the
  provider. Astra sends it unchanged to the provider API.
- **Configuration alias in Astra** is the account-local name used to select that
  configuration. Press Enter to use the model ID, or enter a unique alias to
  distinguish different credentials/endpoints for the same model.

For example, an alias `work-model` for provider model ID `deepseek-v4-flash`
is selected with `astra chat --model work-model`. Commands such as
`astra model show work-model` and `astra model probe work-model` use the alias.
The alias is not an account username or a provider model ID.

Scripts can still supply both explicitly:

```bash
astra model add work-model --provider deepseek --model deepseek-v4-flash --api-key-stdin
```

With `--api-key-stdin`, the alias and required provider/model arguments remain
mandatory; only the interactive wizard offers a default alias. Keys should be
entered at the hidden prompt or supplied via stdin, never as command arguments.

For OpenAI-compatible services, enter the provider's public HTTPS base URL
(including `/v1` if required). The CLI checks URL policy and DNS with the Server
before requesting the API key. This does not send a request to the provider or
prove that the key/model is valid; adding the model performs that check next.
The key prompt hides typed input; press Enter to submit it.

Public HTTPS endpoints do not require individual administrator registration by
default. Operators can opt into strict host/port approval by setting
`ASTRA_BYOK_ENDPOINT_POLICY=trusted-domains` on every Server replica and using
`PUT /admin/llm/trusted-domains`. Both modes block private/metadata addresses,
validate and pin DNS results, and disable redirects. Custom BYOK uses direct
queries to the Server's configured DNS servers, not OS Fake-IP caches. Operators
may override DNS IPs with `ASTRA_BYOK_DNS_SERVERS` and select an HTTP(S) CONNECT
or SOCKS5 proxy with `ASTRA_BYOK_PROXY_URL`. Proxies connect to validated public
IPs, retaining origin TLS verification; ambient proxy variables are not used.
Direct UDP DNS can still be intercepted by a VPN/proxy. Use an explicit
TCP-only entry such as `ASTRA_BYOK_DNS_SERVERS=tcp://10.0.0.53:53` when a
reachable DNS server supports TCP. TCP-only entries never fall back to UDP;
private/Fake-IP answers remain rejected. DNS failures retain resolver diagnostics
in Server logs without exposing them in public API errors.
These are Server settings, not options ordinary CLI users need to fill in.
The default policy is `public-https`; invalid values fail closed. Upgrade the
Server together with the CLI to provide `/me/models/validate-endpoint`.

## Installation

For day-to-day development builds:

```bash
make build-cli
```

For optimized release binaries:

```bash
make build-cli-release
```

Direct `cargo` equivalents:

```bash
cargo build --manifest-path Cargo.toml -p astra-cli -p astra-runtime --bins
cargo build --manifest-path Cargo.toml -p astra-cli -p astra-runtime --release --bins
```

Binary locations:

- Debug profile: `target/debug/astra`, `target/debug/astra-server`
- Release profile: `target/release/astra`, `target/release/astra-server`

`target/debug/` is the normal Cargo location for development builds; it is intentionally separate from `release/`.

## astra

Global options:

```bash
astra --api-url http://127.0.0.1:17001 --profile default <command>
```

Commands:

```bash
# Auth
astra login # Server discovery: configured browser sign-in, otherwise password prompts
astra register --username alice --email alice@example.com --password '***'
astra login --username alice --password '***'
astra interactive
astra whoami
astra refresh
astra logout

# Health
astra health

# Chat
astra chat -m "hello"
astra chat -m "继续" --session-id <session_id> --model gpt-4o

# Sessions
astra session list [--agent-id AGENT] [--status open] [--limit 20] [--offset 0]
astra session show <session_id>
astra session close <session_id>
astra session delete <session_id>

# Replay (reserved; currently unavailable for owned sessions and returns HTTP 501)

# Models
astra model list                         # consumes the complete paginated catalog
astra model show <model_name>

# Skills
astra skill list [--limit 50] [--offset 0]
astra skill show <skill_id> [--version 1.0.0]
astra skill status [--per-group 50]
```

`astra login --username alice` explicitly selects password login. `astra login --manual` accepts a scoped connection key when browser handoff is unavailable. Older Servers returning 404 for `/auth/methods` retain the password journey; network errors do not silently select another provider. Browser addresses come from the target Server's `MEMORIA_WEB_URL`, not the CLI environment.

### Interactive model selection

Cloud BYOK and This device are explicit, separate credential locations:

- `astra model add`, `probe`, and `delete` manage Cloud BYOK on Astra Server.
- `astra model local list`, `add`, `check`, `show`, and `remove` manage this
  device's deployment/account-scoped configuration. `list` and `show` are
  local-only status views: they report credential availability and the next
  action without contacting the provider. Provider keys are never uploaded.
- `/model add` in the TUI opens **local** setup; `/model` selects from the shared
  catalog, including Cloud BYOK and available Runner Offerings.

Commands never fall back between these locations, even when aliases match.

In the interactive CLI/TUI, `/model` opens the Offering picker. `/model <name>`
accepts a unique name (case-insensitive); when multiple Offerings have the same
name, including an offline Runner, choose from the picker or use
`/model <offering_id>`. Exact Offering IDs take precedence over display names.
An ambiguous, unknown, or unavailable selection leaves the current selection
unchanged. Existing thinking suffixes such as `(thinking:high)` are preserved.
The exact Offering selection belongs to the current session. Refreshing model
metadata for the next turn does not select a different account with the same
display name. `--model <offering_id>` also accepts an exact ID; unavailable IDs
fail with a repair message, without falling back to a namesake.

`/model add` opens local model setup. `/model status` (also `/model manage`)
shows saved device models, local credential readiness, and a concrete next
action without starting a Runner or contacting a provider. **Save without test** saves configuration
without a provider call and leaves the current selection unchanged. Selecting
that model later with `/model` does not test it. Run `astra model local check <name>`
for an explicit provider test; this can incur provider charges. A successful or
failed check is recorded against the exact binding revision as secret-safe
evidence; changing the endpoint, model, limits, or credential resets it.

Local Chat Completions setup accepts a base URL (for example,
`https://provider.example/v1`) or the full `/chat/completions` endpoint, including
query parameters. Checking and execution use the same endpoint rules and the
`max_completion_tokens` field. The explicit check makes one streaming request
with at most four completion tokens, including reasoning, and no automatic retry.
Endpoints requiring only the legacy `max_tokens` field are not supported by this
local profile. Passing the check verifies a short stream, not tool support or
answer quality.

The setup form validates fields before saving and keeps invalid input available
for correction. Tab or Up/Down moves between fields; Left/Right changes the
credential source; Ctrl+U clears a field. Enter opens a review step, and Esc
returns to editing or cancels without saving. API keys are masked, and paste
stays inside the form, never in the chat draft. A provider test may incur charges.
If your account, session, or model changes while **Test and use** runs, the
completed setup does not override that newer selection. Use `/model` to select
the ready model explicitly.

The picker always shows the access source and disambiguates duplicate names.
`/model info` displays the current exact Offering ID and session-wide usage.
It does not present a previous model's cached prices as BYOK pricing; consult
your provider for rates. Availability is refreshed through `/model`.

Local model configuration requires a signed-in profile (`astra login`). Desired
definitions and stored secrets are scoped to the Astra deployment URL and the
server-issued account ID, not the profile's display name. `astra model local show
<name>` reports the selected configuration path. A different deployment or
account starts with no inherited local definitions or provider credentials.
Older unscoped `models.json` / `model-secrets` files are not automatically loaded
or copied; re-add the intended definitions in the signed-in scope. Existing
files are left intact.

CLI-managed model capacity uses `astra-edge --inference-only`. This process does
not advertise a workspace/tool executor. The launcher pins the selected profile
and account; inherited `ASTRA_TOKEN`, `ASTRA_TOKEN_FILE`, and token-renewal URL
overrides do not replace that identity. An authenticated account mismatch stops
the attachment before loading model credentials. On reconnect, the shared host
reloads the same profile's latest access token and checks the account again;
normal CLI login renewal does not require creating a new Runner identity.
Logout, a missing token, or a changed account stops that connection rather than
borrowing another profile. A separately managed inference Runner
must also opt in with `--inference-only`; an ordinary `astra-edge` tool Runner
does not load local model configuration. Bash tools inherit the canonical safe
environment baseline plus explicit host-provided call environment, not all
variables exported by the launching terminal. This is an inheritance boundary,
not an OS sandbox against other programs running as the same local user.

On Linux/macOS, terminals signed into the same deployment/account share one
inference-only host and durable journal. Each terminal owns a separate private
socket lease. Saved/keyless model Offerings survive host restart; environment
keys stay in memory and are available only through that terminal's Offering.
**Test and use** selects the exact local attachment, even when two terminals
use the same variable name with different keys. Closing a terminal removes its
environment Offering but does not cancel requests already started. Once all
terminals close, the host drains those requests within their original deadlines,
tries to flush retained results, and exits after its idle grace. Restart reopens
the same journal; ambiguous attempts are not sent to the provider again.

If an environment Offering has expired, reopen `/model` and explicitly select
the new attachment. Neither a namesake nor another terminal's key is an automatic
replacement. A catalog containing only Runner models also requires explicit
selection; catalog order is not permission to spend a personal key.

With no saved local models, opening Astra does not start a local inference host.
The first successful `/model add` save starts it when needed. **Test and use**
checks a candidate before applying it: a failed test leaves the existing
configuration and credentials unchanged. **Save without test** applies the
configuration without a provider request. If publication or local hosting fails
after apply, setup reports that the configuration is saved and needs connection
repair; it does not claim to have rolled back. A fresh `--print` request with a
confirmed Server Offering does not start an unrelated local host.
If setup reports a local-host protocol version mismatch after an upgrade,
upgrade Astra and `astra-edge` together. Close the other local Astra sessions,
allow their active work to drain and the shared host to exit after its idle
grace, then reopen Astra. Existing journals and provider results are retained;
setup does not force another terminal's host to stop.
Local-host startup failures do not prevent reading existing work. Sign-out or
switching the selected profile to another account expires that window's local
credential lease; it never transfers the lease to the new account. After a host
crash or expired connection, retrying local model setup reconnects in the same
window; it does not revive the old environment lease or replay provider work.
Reopening Astra also reconnects. Same-account profile
aliases share saved local definitions, while environment values stay terminal-local.

Proxy variables and `ASTRA_RUNNER_CA_BUNDLE` are the supported host network
settings. Terminals with different settings cannot share the same host: the
attachment reports a network-policy mismatch without showing private values.
Close other local Astra sessions and reopen with the intended settings, or use
an explicitly managed inference Runner. No lock or journal should be deleted
to force a takeover. Automatic shared hosting currently supports Linux/macOS;
Windows named-pipe hosting is not implemented.

Personal Runner bindings currently cover agent/subagent inference and required
compaction. Optional operations on `/v1/chat/completions` (memory extraction,
reranking, turn/skill routing, verification) require a Server Offering. Selecting
a Runner there reports `runner_inference_purpose_unsupported`; it does not
silently charge that personal key or switch accounts.

## astra admin

Global options:

```bash
astra admin --api-url http://127.0.0.1:17001 --profile admin <command>
```

Commands:

```bash
# Auth
astra admin login --username admin --password '***'
astra admin register --username admin --password '***' [--email admin@example.com]
astra admin setup                    # guided admin + model first run
astra admin whoami
astra admin interactive
astra admin refresh
astra admin logout

# Bootstrap
astra admin init

# Audit
astra admin audit [--user-id USER] [--since 2026-02-01] [--limit 100]

# User role management
astra admin user grant-role alice astra_admin
astra admin user revoke-role alice astra_admin

# Model management
astra admin model list                   # complete catalog; server pages are drained
astra admin model add gpt-4 openai --api-key "$OPENAI_API_KEY" --context-window 128000 [--base-url URL]
astra admin model show gpt-4
astra admin model check gpt-4
astra admin model delete gpt-4
astra admin model load .models.yaml

# Token management
astra admin token list [--token-type llm] [--scope global]
astra admin token create --type llm --provider openai --scope global [--scope-id acme] [--token-value "$OPENAI_API_KEY"]

# Skill management
astra admin skill list [--limit 50] [--offset 0]
astra admin skill show <skill_id> [--version 1.0.0]
astra admin skill versions <skill_name>

# Prompt / feedback
astra admin prompt optimize --agent-id <agent_id> [--optimization-type quality]
astra admin feedback stats [--agent-id <agent_id>] [--since 2026-02-01T00:00:00]
astra admin feedback export [--agent-id <agent_id>] [--format jsonl]
```

## Notes

- CLIs share credential storage: `~/.astra/credentials.json` (tests may set `ASTRA_CREDENTIALS_DIR`)
- `--profile` lets you isolate credentials by environment/user
- API errors are returned with HTTP status and compact response body for easier debugging
- Interactive mode launches the TUI (requires a TTY; use `astra chat -m` or `--print` for non-interactive invocations)
