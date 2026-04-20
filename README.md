# astra-engine

Rust-first agent platform for auditable chat runs, session history, replay, skills, admin operations, and MatrixOne-backed state.

## Quick Start

### 1. Configure

```bash
cp .env.example .env                       # fill in MEMORIA_MASTER_KEY + MEMORIA_EMBEDDING_API_KEY
cp .models.yaml.example .models.yaml       # fill in your LLM API keys
make dev-init                              # generate JWT_SECRET_KEY + TOKEN_ENCRYPTION_KEY
```

### 2. Build

```bash
make build
```

### 3. Start

```bash
make dev-deps-up      # MatrixOne + Memoria
make dev-api-start    # API server (auto-creates database schema)
```

Open `http://localhost:8000/health` to verify.

### Logging (API server and CLI diagnostics)

- **`RUST_LOG`**: standard `tracing` filter (e.g. `RUST_LOG=info`, or per-target `astra_runtime=debug`).
- **`ASTRA_LOG_FORMAT`**: `json` (default when stderr is not a TTY), `pretty`, or `compact`. Unknown values print a one-line warning to stderr and fall back like “unset”.
- **`ASTRA_SERVICE_NAME`**: optional label on the startup log line.
- HTTP handlers run inside a per-request span; access lines use target `astra.http.access` and include `request_id` (same value as the `x-request-id` response header). The **`/health`** probe omits that access line to reduce noise (headers and tracing span are unchanged).
- **`astra` CLI**: set **`ASTRA_DIAGNOSTIC_LOG=1`** for structured logs on stderr, or **`ASTRA_LOG_FILE=/path/to.log`** for JSON lines appended to a file (does not replace interactive REPL output). Equivalent hidden flags: **`--diagnostic-log`**, **`--log-file /path/to.log`** (see `cli_args` module docs for priority vs env).
- **OpenTelemetry (optional)**: build with **`--features otel`** on **`astra-runtime`** (for `astra-server`) or **`astra-edge`** so `astra-logging` links the OTLP exporter. Tracing export turns on when **`ASTRA_OTEL_ENABLED=1`** or **`OTEL_EXPORTER_OTLP_ENDPOINT`** / **`OTEL_EXPORTER_OTLP_TRACES_ENDPOINT`** is set to a non-empty value. Use standard OTel variables (**`OTEL_SERVICE_NAME`**, **`OTEL_RESOURCE_ATTRIBUTES`**, **`OTEL_EXPORTER_OTLP_*`**, etc.). The API server stops gracefully on **SIGTERM** or **Ctrl+C** and flushes OTLP batches; **`kill -9`** can drop the last spans.

### 4. Load Models & Login

```bash
astra-admin register
astra-admin login
astra-admin model load .models.yaml

astra chat -m "hello"   # prompts login/register on first run
```

Binaries are at `rust/target/release/`. Add to your PATH or use full paths:

```bash
export PATH="$PWD/rust/target/release:$PATH"
```

## Basic Usage

```bash
astra        # start interactive chat
astra chat -m "hello"   # one-shot
```

Inside the interactive REPL, type `/` to see all commands. Key ones:

```
/model          switch model
/session        session history
/skill          list / run skills
/memory         search memory
/plan           structured planning mode
/checkpoint     save a checkpoint
/review         review git changes
/team           multi-agent team
/help           all commands
```

## Scripting / Integration

For CI or scripted workflows, use non-interactive commands:

```bash
# One-shot chat (returns when done)
astra chat -m "summarize recent changes" --quiet

# Continue an existing session
astra chat -m "follow up" --session-id <id>

# Auto-approve all tool calls (CI mode)
astra chat -m "run tests and fix failures" --permission-mode auto

# Session management
astra session list
astra session show <id>
astra replay <session-id>

# Skills
astra skill list

# Health check
astra health
```

## Daily Commands

```bash
make dev-deps-up        # start deps
make dev-api-restart    # restart API
make dev-deps-down      # stop deps
make dev-status         # check status
```

## Testing

```bash
make test-offline       # unit + contract tests (no DB required)
make test               # full suite (deps must be running)
make check              # lint + format
```

## Repository Layout

```
rust/crates/
  core/        shared types and config
  services/    sessions, journals, durable tasks
  runtime/     Axum HTTP server + contract tests
  astra-cli/   CLI, plan executor, code intel
  astra-admin/ admin CLI
deployment/
  all-in-one/  Docker Compose (deps + app)
skills/        Agent skill definitions
web/           Next.js admin dashboard
```

## Documentation

- `docs/guides/testing.md`
- `docs/reference/makefile-commands.md`
- `deployment/all-in-one/README.md`
