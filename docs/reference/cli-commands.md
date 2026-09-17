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
astra session judge --model MODEL --message 'Rubric and evidence' [--timeout-seconds 120]

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

`astra session judge` returns JSON from one governed, tool-free evaluation in a
separate session. It does not execute the quoted task. The response includes
text, session/completion identities, Offering, usage, and finish reason. Only a
normally completed response exits successfully; errors preserve diagnostic
identity. The provider deadline accepts 1–120 seconds. Evaluation sessions are
closed after definite results; uncertain delivery retains a session for
inspection. See [quality judgment behavior](../guides/testing.md).

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
