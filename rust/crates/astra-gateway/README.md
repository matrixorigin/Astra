# Astra Gateway

Chat platform gateway bridging WeChat/WeCom to AI agent CLIs (astra, claude, codex).

## Quick Start

```bash
cd rust/crates/astra-gateway

# 1. Build
make build

# 2. WeChat login (scan QR code)
make login-weixin

# 3. Run
make run
```

Or with environment variables:
```bash
cp .env.example .env   # edit credentials
make run
```

## Features

| Category | Features |
|----------|----------|
| **Multi-CLI** | astra, claude, codex — switch via `/cli` |
| **Multi-model** | `/model haiku\|sonnet\|opus\|minimax\|deepseek\|qwen\|glm` |
| **Sessions** | Per-CLI isolation, auto-reset (daily/idle), history, switch |
| **Tasks** | Cron jobs, one-time reminders, durable tasks (checkpoint/resume) |
| **Workspace** | `/workspace` to switch project directories, auto-discovery |
| **Observability** | `/trace`, `/running`, `/inspect` (harness), `/audit` (decision chain), `/usage` (cost) |
| **Skills** | Built-in gateway skill + user-defined `.md` files + agent self-creation |
| **Access** | Allowlist, open, disabled — per gateway |
| **Groups** | Per-user session isolation, @mention filtering |
| **WeChat** | Typing indicator, markdown rendering, voice transcription, media |
| **Reliability** | Per-conversation queues, durable delivery outbox, traceable retry/cancel, bounded parallelism |

## Configuration

```bash
cp gateway.example.yaml gateway.yaml
```

```yaml
astra:
  base_url: "http://127.0.0.1:8000"      # astra server

database:
  url: "mysql://root:111@127.0.0.1:6001/astra_gateway"

cli:                                       # default CLI
  type: astra
  bin: /path/to/astra
  permission_mode: auto
  model: "MiniMax-M2.7"

cli_profiles:                              # /cli switch targets
  claude:
    type: claude
    bin: claude
  astra:
    type: astra
    bin: /path/to/astra
    permission_mode: auto

platforms:
  weixin:
    enabled: true
    token: ""                              # from `make login-weixin`
    account_id: ""

# Optional
skills_dir: "~/.astra-gateway/skills"      # user-defined skills
project_dirs: ["~/github", "~/work"]       # auto-discover git projects
session_reset:
  idle:
    hours: 24
access: open                               # or: allowlist / disabled
action_policy:
  allow_slash_mutations: true
  allow_model_generated_mutations: true
  workspace_roots: []                       # optional allowed /workspace roots
max_concurrent_runs: 4                      # cross-conversation parallelism cap
group_sessions_per_user: true
group_require_mention: false
```

See `.env.example` for environment variable reference.

## User Commands

| Command | Description |
|---------|-------------|
| `/help` | All commands |
| `/status` | CLI + model + session + harness summary |
| `/new` | New conversation |
| `/cli` / `/cli claude` | Show/switch CLI backend |
| `/model` / `/model opus` | Show/switch model |
| `/workspace <path>` | Switch working directory |
| `/session list\|switch` | Session history |
| `/inspect` | Harness: tokens, cost, tools, warnings |
| `/audit` | Decision chain (last N turns) |
| `/running` | Queued/running gateway requests |
| `/trace <id>` | Request lifecycle and events |
| `/cancel <id>` | Cancel a queued request |
| `/task list\|cancel\|resume` | Durable task management |
| `/cron list\|add\|del` | Scheduled tasks |
| `/usage` | Token/cost statistics |

Natural language also works — agent handles cron/remind/task/workspace via `[[GATEWAY:action]]` tags.

## Development

```bash
make test          # unit tests (236+)
make test-live     # e2e with real LLM (requires astra server + DB)
make test-offline  # fixture-based, no LLM
make lint          # clippy + rustfmt
```

## Deployment

### Docker
```bash
make docker
docker run -v ./gateway.yaml:/app/gateway.yaml astra-gateway
```

### Binary
```bash
make build
../../target/release/astra-gateway --config gateway.yaml
```

### First-time Setup
```bash
make setup         # interactive wizard
```

## Architecture

```
WeChat/WeCom ──→ PlatformAdapter ──→ GatewayRunner
                                         │
                    ┌────────────────────┤
                    ↓                    ↓
              handle_fast          handle_message (async)
           (slash commands)        (CLI spawn in tokio::spawn)
              instant ↓                  ↓
                                    CLI bridge → astra/claude/codex
                                          ↓
                                    trace/run/outbox + policy checks
                                         ↓
                                   DB ops + response
                                         ↓
              ←── cli_resp channel ←─────┘
                    ↓
              PlatformAdapter.send_text() ──→ WeChat/WeCom
```

Slash commands respond instantly while regular chat requests are serialized per conversation.
Different conversations run concurrently up to `max_concurrent_runs`; final responses are written
to the durable outbox before platform delivery is acknowledged.

## Database

Auto-created on first run. Tables:

| Table | Purpose |
|-------|---------|
| `gw_users` | Profiles + preferences (CLI, model, workspace) |
| `gw_sessions` | Chat → CLI session mapping (per-CLI isolation) |
| `gw_cron_jobs` | Recurring + one-time scheduled tasks |
| `gw_durable_tasks` | Checkpointable long-running tasks |
| `gw_platform_credentials` | WeChat tokens, context tokens, sync cursors |
| `gw_pending_messages` | Crash recovery queue |
| `gw_trace_requests` | User/scheduler request state |
| `gw_trace_runs` | CLI/runtime attempt state |
| `gw_trace_events` | Append-only trace/audit event stream |
| `gw_trace_outbox` | Durable platform delivery queue |
| `gw_usage` | Per-message token/cost tracking |
