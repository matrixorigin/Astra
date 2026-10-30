# CLI Commands Reference

Current Rust CLI reference for the single `astra` CLI, including `astra admin`.

## Stdio app server

`astra serve stdio` accepts JSON-RPC requests over piped stdin/stdout. A
`turn/start` request with `permissionMode: "prompt"` (or `"accept_edits"`)
uses the same permission manager as the terminal. When a tool needs approval,
`approval/requested` carries its approval ID; answer with `approval/respond`
using `approvalId` and `decision` (`allow_once`, `always`, or `deny`).
`turn/interrupt` closes pending approvals without authorizing execution.
The approval transport does not provide the `ask_user` question interface.
Prompt execution without an approval transport or terminal prompt sink fails
before a model request is admitted.

## Turn summary

The completion line shows elapsed time, time to first token (`ttft`), primary
model tokens, and cache usage when reported. Complete input accounting shows
the cached-input percentage in both CLI and TUI, even when output usage is
unreported. The denominator is fresh input + cache reads + cache creation;
output and auxiliary calls are excluded. Incomplete input accounting shows lower bounds such as
`≥19.9k tokens · ≥17.9k cached`. Missing cache measurements are omitted, not
reported as zero. The `≥` counts are reported lower bounds; no cache
percentage is shown without a complete input total. Session totals remain
separate in parentheses.

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

## ChatGPT subscription models

On a self-hosted Server with subscription sharing enabled, sign in to Astra,
then link a ChatGPT account from a terminal with a local browser:

```bash
astra model connect chatgpt --label personal
astra model connections
astra model add coding --provider chatgpt --connection <connection-uuid> --model <visible-slug> --default
astra chat --model coding
```

`astra model add` also offers a guided ChatGPT setup and lists the models
currently visible to the chosen account. Multiple aliases/model slugs can share
one connection; link another account to create a separate credential source.
Adding/checking a model sends a short inference probe against that subscription.
Linking alone does not change the default model. Existing CLI/Web model pickers
can select a configured Offering; browser linking is available in the CLI only.

The browser asks OpenAI for sign-in and plan-use consent. Astra Server encrypts
renewable credentials with its existing token encryption key and can continue
inference after this terminal closes. Use a Server you trust and HTTPS for a
remote Server. This flow does not import the Codex CLI credential cache or
change Astra's login identity. Native Codex collaborators remain separately
configured.

Recovery and removal:

```bash
astra model connect chatgpt --connection <connection-uuid>
astra model check coding
astra model disconnect <connection-uuid>
```

A failed code exchange prints an authorization UUID; `astra model connect
chatgpt --retry <authorization-uuid>` retains the issued client registration.
Declined, expired or cancelled attempts require a new authorization. Reconnect
must use the same ChatGPT identity. Disconnect disables every model using that
connection and cancels its requests; recheck individual models to reactivate
them after reconnecting. The result reports whether upstream revocation was
confirmed. Inspect OpenAI's connected-app settings if it was not confirmed.
A disconnected default produces an actionable error until you recheck it or
explicitly choose another default. Quota errors never select a paid fallback.

V1 supports text and Astra function tools through the Responses HTTP API.
Images/audio, hosted tools, realtime/WebSocket sessions, custom endpoints and
reasoning controls are not exposed. Availability and eligibility come from
OpenAI; a ChatGPT subscription does not imply every model is available.
See [Server configuration](configuration.md#chatgpt-subscription-connections)
for deployment and rollback requirements.

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
astra session show <session_id> [--execution [--transcript] [--run-events]] # bounded read-only facts; optional transcript and recent run-event tails
astra session close <session_id>
astra session delete <session_id>
astra session judge --model MODEL --message 'JudgmentRequest JSON' [--timeout-seconds 120]
# Stream a complete request without command-line argument size limits:
astra session judge --model MODEL --message - < judgment.json

# Models
astra model list                         # consumes the complete paginated catalog
astra model show <model_name>

# Skills
astra skill list [--limit 50] [--offset 0]
astra skill show <skill_id> [--version 1.0.0]
astra skill status [--per-group 50]
```

`astra login --username alice` explicitly selects password login. `astra login --manual` accepts a scoped connection key when browser handoff is unavailable. Older Servers returning 404 for `/auth/methods` retain the password journey; network errors do not silently select another provider. Browser addresses come from the target Server's `MEMORIA_WEB_URL`, not the CLI environment.

`astra session judge` returns JSON from one governed, tool-free evaluation in a
separate session. It does not execute the quoted task. The response includes
text, session/completion identities, Offering, usage, and finish reason. Only a
normally completed response exits successfully; errors preserve diagnostic
identity. The provider deadline accepts 1–120 seconds. Evaluation sessions are
closed after definite results; uncertain delivery retains a session for
inspection. See [quality judgment behavior](../guides/testing.md).

## MCP configuration

Standalone MCP configuration commands run locally without connecting to a server:

```bash
astra mcp list                        # project scope, with a hint to inspect the effective view
astra mcp list -s user                # user scope
astra mcp list --effective            # the configurations selected at chat startup
astra mcp get example                 # winning definition's type, scope and source
astra mcp add -s project example /path/to/mcp-server -- --server-argument
astra mcp add-json -s user remote '{"type":"streamable_http","url":"https://example.com/mcp"}'
astra mcp remove -s user remote
```

Project configuration is `$CWD/.astra/mcp.json`; user configuration defaults to
`~/.astra/mcp.json`. When `ASTRA_LOCAL_STATE_ROOT` is set, its `mcp.json` is the
user configuration instead. Both files use a top-level `mcpServers` object:

```json
{
  "mcpServers": {
    "example": {"command": "/path/to/mcp-server", "args": ["--server-argument"]},
    "remote": {"type": "streamable_http", "url": "https://example.com/mcp"}
  }
}
```

`add-json` accepts **one server entry**, such as the `remote` object above,
without the `mcpServers` wrapper. It supports `stdio` (the default, using
`command`, `args`, and optional `env`), `sse`, `streamable_http`, and `ws`
(using `url`, optional `headers`, and optional `auth_token`). Use the exact
`streamable_http` spelling in configuration; `http`, `streamable-http`, and
`websocket` are not accepted aliases.
`add` writes a stdio entry. Both commands default to project scope and reject
an existing name in that file; remove the existing entry or edit the file to
replace it. Adding configuration does not install an executable or test access.

Chat discovery selects the **first enabled definition of each name**:
project config, then skill `manifest.yaml` files in the existing skill search
path priority, then user config. Manifest files within each skill root are
sorted by path. A standalone entry with `disabled: true` or a skill entry with
`enabled: false` does not reserve its name; a lower priority enabled definition
can still win. Unsupported or incomplete standalone entries are skipped with
a warning. A malformed file produces a warning and contributes no entries;
a missing optional file is normal. Configuration changes apply to the next
chat startup. Cooperating Astra configuration writers serialize each destination
through its stable sibling lockfile before reading and atomically replacing it;
different destinations remain independent. External editors do not participate
in that lock protocol.

`list` and `get` show enabled configurations eligible for discovery, including
the winning source path. They hide connection settings, including commands,
arguments, URLs, environment values, headers, and tokens. The effective view
uses the same loader as chat; it describes configuration, without proving a
connection or tool availability. In an active chat, `/mcp` shows connected
servers and tools. The standalone `astra mcp test NAME -s project|user` and
`astra mcp ping NAME -s project|user` explicitly connect to the selected server;
for stdio this launches its configured command. Each diagnostic performs one
top-level connection attempt using the existing transport deadlines. Both
commands use the chat loader for the selected file and reject disabled or
invalid definitions; they do not fall back to other scopes or skill manifests.
A malformed typed field can invalidate the whole file, just as at chat startup.
Chat connection recovery retains its own retry policy.

The CLI agent receives stable guidance to discover these existing commands and
use only its selected local executor. Configuration JSON and raw connection
errors are omitted from Astra-owned command output. `test` can still print
server-provided tool/resource data; diagnostic logs and shell history are not
secret stores. Use protected local configuration files for credentials rather
than placing them in inline shell arguments.

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
astra admin feedback export [--agent-id <agent_id>] [--format jsonl]
```

For a custom OpenAI-compatible deployment, transport compatibility does not prove
support for thinking controls. If the deployment has been verified to support
`reasoning_effort`, add `thinking_protocol: reasoning_effort` alongside `name`
and `provider` in its YAML model entry, then run
`astra admin model load .models.yaml --update-existing`. Keep all existing model
fields and quirks: supplying quirks replaces the complete stored object.
`astra admin model check <name>` repeats the check; `model show <name>` exposes
the observation and diagnostics. An `effort_only` result allows effort selection,
not thinking Off. An inconclusive check establishes no new capability. A matching
prior capability may remain available while diagnostics record the latest error.

Explicit CLI model selectors such as `'<name>(thinking:low)'`,
`'<name>(thinking:medium)'` and `'<name>(thinking:high)'` select depth. A plain
`<name>` retains the provider default. The declaration alone does not force Low
and a successful effort probe does not prove that the provider honors every
level; verify the deployment's supported values before declaring its protocol.

## Notes

- CLIs share credential storage: `~/.astra/credentials.json` (tests may set `ASTRA_CREDENTIALS_DIR`)
- `--profile` lets you isolate credentials by environment/user
- API errors are returned with HTTP status and compact response body for easier debugging
- Once a chat stream has started, `astra chat --json` and `--print --output-format json` also return a structured result on hard failure (exit code 3 for API failures). It retains the validated root run identity, partial output and observed usage, with `success: false` and partial evidence coverage. A request rejected before Run admission has no run identity; an error does not imply successful or resumable execution.
- An unsupported local workspace coordination host fails before admission with exit code `1`, `error_kind: "tool_unavailable"`, `error_code: "workspace_host_unsupported"`, non-retryable metadata, and `server_admission_not_dispatched: true`. No model or tool ran. Interactive mode restores the draft and preserves recovery state. Human diagnostics remain on stderr in text, JSON, and stream-json modes. Repair the host prerequisite described in the diagnostic; changing workspace grants does not repair it.
- Interactive mode launches the TUI (requires a TTY; use `astra chat -m` or `--print` for non-interactive invocations). There is no separate console slash frontend.

In the interactive composer, a leading `/` selects a slash action. To send literal
slash text, escape it with another slash: `//tmp/project inspect this` sends
`/tmp/project inspect this`, and `//stop` sends `/stop` as conversation text.
The escape also applies to queued messages and guidance for an active run.
Returned conversation can merge with an existing conversation draft. Commands
and shell drafts, including multiline pasted content, stay separate. Intentional
whitespace-only drafts also remain editable. “Unsent drafts” shows retained input, which
moves into the composer after the current draft is sent or cleared. Each draft
requires a new submission gesture; restoration never sends it automatically.
Navigation also preserves the draft suspended while browsing history, including
pasted content. An unchanged recalled message remains in history; edited recall
and distinct original drafts return for editing through the same unsent list.
These editable drafts remain local to the running client.
Ctrl+R also restores the latest conversation with its slash escape, so editing
and resending `/exit` text keeps it a message. The transcript shows the original
conversation text, without the composer escape.
Unknown or ambiguous single-slash actions still report a command error.
