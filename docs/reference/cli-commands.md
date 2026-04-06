# CLI Commands Reference

Current Rust CLI reference for `astra` and `astra-admin`.

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
cargo build --manifest-path rust/Cargo.toml -p astra-cli -p astra-admin-cli -p astra-runtime --bins
cargo build --manifest-path rust/Cargo.toml -p astra-cli -p astra-admin-cli -p astra-runtime --release --bins
```

Binary locations:

- Debug profile: `rust/target/debug/astra`, `rust/target/debug/astra-admin`, `rust/target/debug/astra-server`
- Release profile: `rust/target/release/astra`, `rust/target/release/astra-admin`, `rust/target/release/astra-server`

`rust/target/debug/` is the normal Cargo location for development builds; it is intentionally separate from `release/`.

## astra

Global options:

```bash
astra --api-url http://127.0.0.1:8000 --profile default <command>
```

Commands:

```bash
# Auth
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

# Replay
astra replay <session_id> [--sandbox-name test] [--mock-mode true] [--compare]

# Models
astra model list
astra model show <model_name>

# Skills
astra skill list [--limit 50] [--offset 0]
astra skill show <skill_id> [--version 1.0.0]
astra skill status [--per-group 50]
astra skill register --name my-skill --version 1.0.0 --code-file ./skill.json
astra skill register --name my-skill --version 1.0.0 --code '{"entry":"run"}' --metadata-json '{"owner":"team-a"}'
```

## astra-admin

Global options:

```bash
astra-admin --api-url http://127.0.0.1:8000 --profile admin <command>
```

Commands:

```bash
# Auth
astra-admin login --username admin --password '***'
astra-admin register --username admin --password '***' [--email admin@example.com]
astra-admin whoami
astra-admin interactive
astra-admin refresh
astra-admin logout

# Bootstrap
astra-admin init

# Audit
astra-admin audit [--user-id USER] [--since 2026-02-01] [--limit 100]

# User role management
astra-admin user grant-role alice astra_admin
astra-admin user revoke-role alice astra_admin

# Model management
astra-admin model list
astra-admin model add gpt-4 openai --api-key "$OPENAI_API_KEY" [--base-url URL]
astra-admin model show gpt-4
astra-admin model check gpt-4
astra-admin model delete gpt-4
astra-admin model load .models.yaml

# Token management
astra-admin token list [--token-type llm] [--scope global]
astra-admin token create --type llm --provider openai --scope global [--scope-id acme] [--token-value "$OPENAI_API_KEY"]

# Skill management
astra-admin skill list [--limit 50] [--offset 0]
astra-admin skill show <skill_id> [--version 1.0.0]
astra-admin skill versions <skill_name>

# Prompt / feedback
astra-admin prompt optimize --agent-id <agent_id> [--optimization-type quality]
astra-admin feedback stats [--agent-id <agent_id>] [--since 2026-02-01T00:00:00]
astra-admin feedback export [--agent-id <agent_id>] [--format jsonl]
```

## Notes

- CLIs share credential storage: `~/.astra/credentials.json` (tests may set `ASTRA_CREDENTIALS_DIR`)
- `--profile` lets you isolate credentials by environment/user
- API errors are returned with HTTP status and compact response body for easier debugging
- Interactive mode starts an in-terminal command loop (`help`/`exit` supported)
