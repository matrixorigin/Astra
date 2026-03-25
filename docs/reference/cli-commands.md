# CLI Commands Reference

Current Rust CLI reference for `mo-agent` and `mo-admin`.

## Installation

For day-to-day development builds:

```bash
make cli-build
```

For optimized release binaries:

```bash
make cli-build-release
```

Direct `cargo` equivalents:

```bash
cargo build --manifest-path rust/Cargo.toml -p mo-agent-runtime --bins
cargo build --manifest-path rust/Cargo.toml -p mo-agent-runtime --release --bins
```

Binary locations:

- Debug profile: `rust/target/debug/mo-agent`, `rust/target/debug/mo-admin`, `rust/target/debug/mo-agent-server`
- Release profile: `rust/target/release/mo-agent`, `rust/target/release/mo-admin`, `rust/target/release/mo-agent-server`

`rust/target/debug/` is the normal Cargo location for development builds; it is intentionally separate from `release/`.

## mo-agent

Global options:

```bash
mo-agent --api-url http://127.0.0.1:8000 --profile default <command>
```

Commands:

```bash
# Auth
mo-agent register --username alice --email alice@example.com --password '***'
mo-agent login --username alice --password '***'
mo-agent interactive
mo-agent whoami
mo-agent refresh
mo-agent logout

# Health
mo-agent health

# Chat
mo-agent chat -m "hello"
mo-agent chat -m "继续" --session-id <session_id> --model gpt-4o

# Sessions
mo-agent session list [--agent-id AGENT] [--status open] [--limit 20] [--offset 0]
mo-agent session show <session_id>
mo-agent session close <session_id>
mo-agent session delete <session_id>

# Replay
mo-agent replay <session_id> [--sandbox-name test] [--mock-mode true] [--compare]

# Models
mo-agent model list
mo-agent model show <model_name>

# Skills
mo-agent skill list [--limit 50] [--offset 0]
mo-agent skill show <skill_id> [--version 1.0.0]
mo-agent skill status [--per-group 50]
mo-agent skill register --name my-skill --version 1.0.0 --code-file ./skill.json
mo-agent skill register --name my-skill --version 1.0.0 --code '{"entry":"run"}' --metadata-json '{"owner":"team-a"}'
```

## mo-admin

Global options:

```bash
mo-admin --api-url http://127.0.0.1:8000 --profile admin <command>
```

Commands:

```bash
# Auth
mo-admin login --username admin --password '***'
mo-admin register --username admin --password '***' [--email admin@example.com]
mo-admin whoami
mo-admin interactive
mo-admin refresh
mo-admin logout

# Bootstrap
mo-admin init

# Audit
mo-admin audit [--user-id USER] [--since 2026-02-01] [--limit 100]

# User role management
mo-admin user grant-role alice mo_agent_admin
mo-admin user revoke-role alice mo_agent_admin

# Model management
mo-admin model list
mo-admin model add gpt-4 openai --api-key "$OPENAI_API_KEY" [--base-url URL]
mo-admin model show gpt-4
mo-admin model check gpt-4
mo-admin model delete gpt-4
mo-admin model load .models.yaml

# Token management
mo-admin token list [--token-type llm] [--scope global]
mo-admin token create --type llm --provider openai --scope global [--scope-id acme] [--token-value "$OPENAI_API_KEY"]

# Skill management
mo-admin skill list [--limit 50] [--offset 0]
mo-admin skill show <skill_id> [--version 1.0.0]
mo-admin skill versions <skill_name>

# Prompt / feedback
mo-admin prompt optimize --agent-id <agent_id> [--optimization-type quality]
mo-admin feedback stats [--agent-id <agent_id>] [--since 2026-02-01T00:00:00]
mo-admin feedback export [--agent-id <agent_id>] [--format jsonl]
```

## Notes

- CLIs share credential storage: `~/.mo-agent/credentials.json`
- `--profile` lets you isolate credentials by environment/user
- API errors are returned with HTTP status and compact response body for easier debugging
- Interactive mode starts an in-terminal command loop (`help`/`exit` supported)
