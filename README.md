<div align="center">

# Astra

### A durable runtime for enterprise agents that do real work

**One agent backbone. Many capacity providers.**

[![Test Suite](https://github.com/matrixorigin/astra/actions/workflows/test.yml/badge.svg)](https://github.com/matrixorigin/astra/actions/workflows/test.yml)
[![Static Checks](https://github.com/matrixorigin/astra/actions/workflows/static-checks.yml/badge.svg)](https://github.com/matrixorigin/astra/actions/workflows/static-checks.yml)
[![VLDB ADS](https://img.shields.io/badge/VLDB_ADS-Accepted-6F42C1)](#research-and-benchmarks)
[![Terminal-Bench](https://img.shields.io/badge/Terminal--Bench-Results_coming_soon-0A7EA4)](#research-and-benchmarks)
[![Rust 1.97](https://img.shields.io/badge/Rust-1.97-000000?logo=rust)](rust-toolchain.toml)
[![TypeScript](https://img.shields.io/badge/SDK-TypeScript-3178C6?logo=typescript&logoColor=white)](packages/sdk)

[Why Astra](#why-astra) · [Quick start](#quick-start) · [Architecture](#architecture) · [User Runner](#user-runner) · [Enterprise](#enterprise-agents-by-design) · [Research](#research-and-benchmarks) · [Docs](#documentation)

</div>

---

Astra is a Rust-first agent kernel and runtime for complex, multi-step
enterprise work. It brings models, tools, memory, durable Work, planning,
permissions, and observability into one system so an agent can keep moving
toward an outcome—not just answer one prompt.

The same session, run, turn, task, context, checkpoint, trace, and audit
semantics are shared across Web, CLI/TUI, SDK clients, and User Runners.
Different environments contribute different capabilities without creating
different agents.

> **Astra unifies CLI, Server, and User Runner execution through one durable
> backbone, with a research-backed Context Pipeline, governed execution, and a
> native observation plane.**

> [!IMPORTANT]
> Astra is under active development and public interfaces may change before
> 1.0. Documents in [`docs/design/`](docs/design/) define target contracts and
> may lead the implementation on a given branch. Current behavior is guarded by
> code, contract tests, and runtime-profile tests.

## Why Astra

Most agent applications begin as a model plus tools. Enterprise work quickly
adds harder requirements: state that survives requests, local data boundaries,
approvals, retries, reconnects, recovery, and evidence explaining what
happened. Astra makes those concerns part of the runtime.

| Core capability | What it gives you |
| --- | --- |
| **Durable Agent Kernel** | Sessions, runs, turns, canonical Work, task graphs, checkpoints, replay facts, recovery, and explicit lifecycle states |
| **One Backbone, Three Runtime Profiles** | CLI + Server, Server-only, and Server + User Runner use one agent implementation with different capacity |
| **User Runner** | User-bound workspace, file, shell, Git, build/test, network, and local MCP execution without ambient server authority |
| **Context Pipeline** | Governed assembly of system contract, Work state, provider state, memory, artifacts, compression, and prompt-cache-safe context |
| **Governed Execution** | One decision path for identity, policy, permission, side effects, provider admission, routing, fallback, and result quality |
| **Native Observation Plane** | Trace records facts; Introspect reads state; Explain presents causes; Reflect proposes changes; Audit preserves accountability |

### One backbone, three runtime profiles

| Runtime profile | Execution shape | Best suited for |
| --- | --- | --- |
| **CLI + Server** | The Server backbone plus CLI/TUI interaction and CLI-local workspace capacity | Developers, operators, automation, and terminal-first work |
| **Server-only** | Web, SDK, or enterprise apps use durable server state, server-safe tools, memory, and request-scoped providers | Central services, knowledge work, and controlled business workflows |
| **Server + User Runner** | The Server dispatches admitted local work to a user-bound Runner | Hybrid cloud/local work, source code, private data, and user-owned environments |

These are capacity profiles, not separate agents. Changing the surface or
Runner availability does not create a new memory, policy, lifecycle, or
evidence model.

If your use case is one stateless model call, a direct LLM API is usually
simpler. Astra is designed for the point where state, tools, permissions,
recovery, collaboration, or operational control become part of the product.

## Quick start

### Prerequisites

- Git and Make
- Docker with Docker Compose
- Rust via `rustup` (the repository pins Rust 1.97)
- Node.js 20 or newer and OpenSSL command-line tools
- An embedding API and at least one supported LLM endpoint

For Docker-only and production paths, start with the
[getting-started guide](docs/quickstart/README.md).

### 1. Initialize

```bash
git clone https://github.com/matrixorigin/astra.git
cd astra

cp .models.yaml.example .models.yaml
make dev-init
```

Set `MEMORIA_EMBEDDING_API_KEY` and `MEMORIA_EMBEDDING_BASE_URL` in `.env`,
then configure at least one provider in `.models.yaml`. Never commit either
local file.

### 2. Build and start Server-only

```bash
make build
make dev-start-server-only

export PATH="$PWD/target/release:$PATH"
astra health
```

| Service | Default URL |
| --- | --- |
| Web dashboard | <http://localhost:3536> |
| HTTP API | <http://localhost:17001> |
| Health check | <http://localhost:17001/health> |

### 3. Bootstrap an account and model

The first admin registration bootstraps a fresh installation and stores its
credentials in the local CLI profile.

```bash
astra admin register
astra admin model load .models.yaml --update-existing
astra admin model check YOUR_MODEL_NAME

astra
```

You can now use the TUI or send a one-shot request:

```bash
astra chat -m "Map this repository and explain its architecture"
```

### 4. Add a User Runner when local execution is needed

Server-only mode deliberately has no implicit access to your machine. Connect
a User Runner when a Web session needs local file, shell, Git, build/test, or
private-network capacity:

```bash
ASTRA_EDGE_WORKSPACE_DIR=/path/to/workspace make dev-edge-start
```

Use `make dev-start-server-edge` on later starts to bring up the Server, Web
dashboard, and local User Runner together.

## Use Astra

### Interactive, one-shot, and automation

```bash
astra                                      # interactive TUI
astra chat -m "Investigate the failing tests"
astra chat -m "Continue" --session-id SESSION_ID
astra -p "Summarize this text"             # print mode; no tools

astra chat -m "Review the diff" --quiet
astra chat -m "Summarize the changes" --json
astra chat -m "Design the migration" --permission-mode plan
astra chat -m "Run tests and fix failures" --permission-mode auto
```

Inside the TUI, type `/` to discover commands. Common entries include
`/model`, `/session`, `/skill`, `/memory`, `/plan`, `/checkpoint`, `/review`,
`/team`, `/explain`, and `/help`.

### Durable Work and inspection

```bash
astra work start --done-when "tests pass" "Diagnose and fix the regression"
astra work show WORK_ID
astra work continue WORK_ID "Also verify the migration path"

astra session list
astra audit list
astra self snapshot
```

### Skills, MCP, teams, and memory

```bash
astra skill list
astra mcp list
astra team list
astra memory search "deployment preferences"
```

### Embed Astra

```bash
astra serve http --host 127.0.0.1 --port 17001
astra serve stdio
```

HTTP mode exposes the Axum API, SSE, and WebSocket transports. Stdio mode is a
long-lived newline-delimited JSON-RPC app-server for parent processes. See the
[CLI reference](docs/reference/cli-commands.md),
[HTTP API](docs/reference/api-reference.md), and
[TypeScript SDK](packages/sdk/README.md) for complete integration contracts.

## Architecture

```text
Interaction surfaces
  Web dashboard · CLI/TUI · TypeScript SDK · API clients
                         │
                         ▼
Shared agent backbone
  Durable Kernel · Context Pipeline · Policy · Observation Plane
                         │
                         │ admitted capability + execution route
                         ▼
Capacity providers
  Server/cloud · User Runner · MCP/request-scoped · managed sandbox
                         │
                         │ typed result + provider status + evidence
                         ▼
Durable state and records
  MatrixOne · Memoria · transcript · artifacts · checkpoints · trace · audit
```

The central rule is:

```text
agent behavior = backbone semantics + context state + provider decisions + model output
```

The backbone owns lifecycle, context assembly, checkpoints, recovery, tool
protocol, permissions, and observation. Capacity providers declare what they
can do, where they execute, whether they are available, and which policy
applies.

| Provider | Typical capacity | Trust boundary |
| --- | --- | --- |
| Server | Shared state, memory, configured network access, reports, control-plane tools | Hosted runtime |
| User Runner / CLI | Files, shell, Git, builds, tests, local network, local MCP | User identity and user-owned workspace |
| MCP | Business APIs, databases, knowledge bases, ticketing, approvals | MCP server and request binding |
| Sandbox / managed runtime | Isolated scripts and provisioned workspace execution | Explicit runtime binding |

When a provider disconnects, its capacity can become unavailable without
erasing the session, plan, memory, or server-side Work. Read the
[architecture overview](docs/design/ARCHITECTURE.md) and
[agent-backbone contract](docs/design/agent-backbone-capacity-provider.md) for
the complete design.

## Core runtime systems

### Durable Agent Kernel

The kernel turns an interaction into durable, controllable Work. The
assistant's last message is not the authority for whether a task exists, which
attempt owns it, or whether it has been verified.

```text
Session
├── Run
│   ├── Turn
│   │   ├── Model boundary
│   │   └── Tool lifecycle
│   ├── Checkpoint
│   └── Events / artifacts
└── Work
    └── Versioned task graph
        └── Attempt → verification → delivery
```

- **Session** preserves the continuous user/agent relationship across surfaces.
- **Run** is a controllable execution attempt with ownership and terminal state.
- **Work** gives a long-lived goal stable identity beyond an individual chat.
- **Task graph** records decomposition, dependencies, attempts, verification,
  and delivery.
- **Checkpoints and events** make pause, resume, reconnect, and recovery
  reconstructable from durable facts.

```text
queued → running → completed
             ├── waiting ──→ running
             ├── paused  ──→ running
             ├── blocked ──→ running
             ├── cancelling → cancelled
             └── failed
```

### User Runner

A **User Runner** is Astra's user-bound execution plane. The shared backbone
plans, routes, observes, and persists Work; the Runner executes approved
operations inside a user-controlled workspace. Today this role is provided by
`astra-edge` and the CLI-local runtime.

```text
User / app
    │ submit durable Work
    ▼
Astra backbone ── identity · policy · provider decision
    │ admitted tool call
    ▼
User Runner ── workspace and permission boundary
    │
    ▼
User workspace ── file · shell · Git · build · local MCP
    │
    └──── typed result + execution identity + evidence ────► backbone
```

The Runner contributes bounded capacity, not a second agent brain:

- registration and dispatch remain bound to user, Runner, and workspace identity;
- advertised capabilities replace implicit server access to the user's machine;
- permissions and safety checks remain enforced at the execution boundary;
- heartbeat, reconnect, journals, and result reconciliation make disconnects
  observable and recoverable;
- results join the same transcript, task, trace, audit, and checkpoint model as
  server and MCP providers.

### Context Pipeline

The Context Pipeline is a core Astra kernel contribution and a central subject
of the Astra paper. It treats context as a governed, recoverable data
pipeline—not one indefinitely growing prompt string.

```text
System contract ──────────┐
Session · Run · Work ─────┤
Provider · policy state ──┤──► assemble ─► select/budget/compress ─► model
Memory · artifacts ───────┘                                      │
                                                                 ▼
                                              trace · checkpoint · usage
```

It provides stable prompt-cache-friendly contracts, typed dynamic state,
explicit precedence and provenance, budget-aware selection, semantic
compression, and reconstruction from checkpoints and durable facts.

### Governed execution

Tool visibility and tool execution use the same lifecycle:

```text
Projection → Admission → Execution → Result
```

Each decision considers identity, mode, side-effect class, permission scope,
workspace authority, provider binding and health, runtime location, fallback
policy, and result quality. The outcome drives the model-visible tool surface,
execution route, user diagnostics, trace, and audit.

| Outcome | Meaning |
| --- | --- |
| `Ready` | The provider is bound, healthy, and admitted now |
| `PolicyBlocked` | The capability exists, but current policy or mode rejects the call |
| `MissingRuntimeBinding` | A provider contract exists, but no executable runtime is attached |
| `ProviderOffline` | The selected User Runner or provider is disconnected |
| `Unsupported` | No provider owns the requested capability in this deployment |
| `FallbackSelected` | Policy approved a different provider and recorded why |

A narrow capability failure blocks that action rather than erasing the whole
session or pretending the capability never existed.

### Trace, Introspect, Explain, Reflect, and Audit

The observation plane is part of the agent contract, not an after-the-fact log
viewer:

```text
Runtime facts ──► Trace ──► Introspect ──► Explain
                    │             └──────► Reflect
                    │
Policy decisions ───┴─────────────► Audit
```

| Component | Question it answers | Authority |
| --- | --- | --- |
| **Trace** | What happened, in which causal order, through which model, tool, and provider? | Records measured execution facts |
| **Introspect** | What is the current run, context, task, budget, capability, and blocked state? | Reads and structures current facts |
| **Explain** | Why is the run working, waiting, degraded, blocked, or failed, and what can the user do? | Presents a user-facing projection of facts |
| **Reflect** | What may be wrong, and should strategy change or human help be requested? | Proposes advice; cannot grant permission or complete tasks |
| **Audit** | Which identity, permission, provider, fallback, and side-effect facts remain accountable? | Preserves durable accountability records |

Reflection cannot rewrite runtime truth, Explain does not expose private
chain-of-thought, and debug output is not automatically an audit record.

## Enterprise agents, by design

An enterprise agent is not a personal copilot moved onto a company server. It
must operate across users, teams, applications, data domains, and execution
environments while preserving identity, policy, evidence, and operational
control.

| Enterprise requirement | Astra design |
| --- | --- |
| Durable business work | Sessions, Work, task graphs, checkpoints, artifacts, and typed terminal states survive client disconnects |
| Identity and isolation | User-, session-, workspace-, provider-, and execution-scoped identities travel through admission, dispatch, persistence, and audit |
| User-owned execution | User Runners expose bounded local capacity without granting the Server ambient access to user machines |
| Governed side effects | Tool visibility, permission, provider admission, route, fallback, and result handling share one decision path |
| Human control | Plan mode, approvals, pause, resume, cancel, blocked states, and explicit continuation paths keep people in control |
| Enterprise integration | MCP, request-scoped providers, HTTP, SSE, WebSocket, and the TypeScript SDK join the same runtime semantics |
| Model governance | Model identity, endpoint, credentials, health, pricing, routing, fallback, and usage policy are managed independently of Work state |
| Accountability and resilience | Trace, audit, retries, reconnects, degraded states, health checks, and OpenTelemetry support production operation |

This lets an enterprise centralize governance and durable state while keeping
execution authority in the environment that owns the data, credentials,
network access, and side effects.

## How Astra differs from coding agents

[Claude Code](https://code.claude.com/docs/en/getting-started),
[Codex](https://developers.openai.com/codex),
[Pi](https://pi.dev/docs/latest), and
[DeepSeek Harness](https://www.deepseek.com/harness/en/) are strong systems for
interactive coding or composing an agent harness. Astra starts from a different
question:

> **How can an enterprise own and operate durable agents across users,
> applications, execution environments, and trust boundaries?**

| System | Primary design center | Astra's distinction |
| --- | --- | --- |
| Claude Code | Developer-facing coding agent across terminal, IDE, tools, and enterprise model endpoints | Astra makes the durable enterprise runtime—not one coding surface—the system of record |
| Codex | Coding agent across local, cloud, IDE, automation, and integration surfaces | Astra is model-provider independent and centers self-hosted backbone state, User Runners, and governed providers |
| Pi | Minimal terminal coding harness extended through TypeScript packages, skills, prompts, and themes | Astra centers a distributed Server/Runner architecture, durable Work, enterprise identity, and operations |
| DeepSeek Harness | Plugin-first harness with composable capabilities, runtime modes, and a traceable session log | Astra centers canonical lifecycle state, cross-user control, provider decisions, and user-bound execution |
| **Astra** | **Enterprise agent kernel and runtime** | **One durable backbone across custom Web apps, CLI/TUI, SDKs, cloud services, MCP, sandboxes, and User Runners** |

The comparison is about architectural emphasis, not whether another system can
implement an individual feature. Astra's durable distinctions are:

- **The enterprise owns the runtime** — identity, Work, tasks, provider
  bindings, model routes, trace, and audit live in an operable system.
- **Work is more than chat history** — goals, attempts, verification, delivery,
  checkpoints, and resumability are canonical state.
- **Execution follows authority** — local, server, MCP, and sandbox capacity is
  explicitly bound and governed.
- **Every surface shares semantics** — Web, TUI, SDK, and Runner-backed sessions
  see the same lifecycle, failure, and evidence model.
- **Models are replaceable; governance is not** — model routes can change
  without changing identity, Work history, policy, or evidence.

Coding is an important Astra workload, but it is not the product boundary.
Astra is designed to be self-hosted, embedded, extended, and exposed through
enterprise products.

## Research and benchmarks

Astra's agent kernel is the subject of a systems research paper accepted at
**VLDB ADS**. The paper describes durable agent execution, the Context
Pipeline, and the separation of shared agent semantics from execution capacity.

| Publication | Venue | Status | Links |
| --- | --- | --- | --- |
| Astra agent kernel paper *(title to be added)* | VLDB ADS | **Accepted** | Paper coming soon · Conference page coming soon |

<!--
Research placeholders:
- ASTRA_PAPER_TITLE
- ASTRA_PAPER_URL
- VLDB_ADS_URL
-->

### Terminal-Bench

[Terminal-Bench](https://github.com/harbor-framework/terminal-bench) evaluates
agents on difficult, realistic terminal tasks. Astra has a first-party Harbor
adapter and a scored-run contract designed for reproducible comparisons.

| Harness | Version / commit | Model snapshot | Benchmark release | Score | Artifacts |
| --- | --- | --- | --- | ---: | --- |
| **Astra** | Coming soon | Coming soon | Coming soon | **Coming soon** | Report · trajectories · logs coming soon |
| Claude Code | Coming soon | Coming soon | Coming soon | Coming soon | Coming soon |
| Codex | Coming soon | Coming soon | Coming soon | Coming soon | Coming soon |
| Pi | Coming soon | Coming soon | Coming soon | Coming soon | Coming soon |
| DeepSeek Harness | Coming soon | Coming soon | Coming soon | Coming soon | Coming soon |

<!--
Terminal-Bench placeholders:
- TERMINAL_BENCH_REPORT_URL / TERMINAL_BENCH_RELEASE
- ASTRA_RESULT / ASTRA_COMMIT / ASTRA_MODEL / ASTRA_ARTIFACT_URL
- CLAUDE_CODE_RESULT / VERSION / MODEL / ARTIFACT_URL
- CODEX_RESULT / VERSION / MODEL / ARTIFACT_URL
- PI_RESULT / VERSION / MODEL / ARTIFACT_URL
- DEEPSEEK_HARNESS_RESULT / VERSION / MODEL / ARTIFACT_URL
-->

Scored runs require a clean tracked checkout, a newly owned server, exact
source/binary revision checks, a fresh benchmark database, sealed model and
Harbor configuration, controlled network mode, and durable result provenance.
The canonical entry points are the
[scored-run launcher](scripts/harness/run_terminal_bench_current.sh) and
[Harbor adapter](crates/astra-test-harness/harbor_adapter.py).

## Deploy and operate

Astra supports local source development, all-in-one Compose, Kubernetes, and
Server + User Runner topologies. Start with the
[deployment overview](deployment/README.md).

Runtime and model configuration are intentionally separate:

- [`.env.example`](.env.example) covers database, authentication, Memoria,
  runtime limits, logging, and optional provider bindings.
- [`.models.yaml.example`](.models.yaml.example) defines model endpoints,
  credentials, capabilities, pricing, and fallback chains.
- [`config/server.toml.example`](config/server.toml.example) is the file-based
  server baseline; `ASTRA_*` environment variables take precedence.

Server logs use `tracing` and can export OTLP traces. CLI diagnostics remain on
stderr or in a dedicated JSONL file so machine-readable stdout stays clean.

```bash
RUST_LOG=info ASTRA_LOG_FORMAT=pretty make dev-api-start
ASTRA_DIAGNOSTIC_LOG=1 astra chat -m "hello"
ASTRA_LOG_FILE=/tmp/astra.jsonl astra doctor
```

See the [configuration reference](docs/reference/configuration.md),
[production guide](docs/quickstart/production.md), and
[troubleshooting guide](docs/guides/troubleshooting.md).

## Documentation

The README is the product overview and shortest runnable path. Detailed
documentation is organized by reader goal:

| I want to... | Start here | Continue with |
| --- | --- | --- |
| **Try and use Astra** | [Getting started](docs/quickstart/README.md) | [CLI commands](docs/reference/cli-commands.md) and [TUI slash commands](docs/reference/slash-commands.md) |
| **Build an application** | [TypeScript SDK](packages/sdk/README.md) | [HTTP API](docs/reference/api-reference.md) and [examples](examples/README.md) |
| **Deploy and operate** | [Deployment overview](deployment/README.md) | [Configuration](docs/reference/configuration.md) and [troubleshooting](docs/guides/troubleshooting.md) |
| **Develop and contribute** | [Developer setup](docs/quickstart/development.md) | [Workflow](docs/guides/development-workflow.md), [testing](docs/guides/testing.md), and [Make targets](docs/reference/makefile-commands.md) |
| **Understand the kernel** | [Architecture](docs/design/ARCHITECTURE.md) | [Design index](docs/design/README.md), [lifecycle](docs/design/runtime-lifecycle.md), and [capabilities](docs/design/capability-system.md) |

The [full documentation index](docs/README.md) separates current user and
operator guidance from normative design contracts.

## Development and contributing

```bash
make dev-status         # inspect local services
make test-offline       # unit, contract, SDK, Web, and runtime-profile tests
make test-contract      # focused HTTP/admin/config contracts
make check              # clippy, formatting, Rust types, and Web checks
make test               # complete suite; live dependencies are required
make dev-stop           # stop the local development environment
```

Use the smallest relevant package while iterating, then run the repository
gates before opening a pull request. The [testing guide](docs/guides/testing.md)
explains the offline, contract, live MatrixOne, and system-matrix lanes.

<details>
<summary>Repository layout</summary>

```text
crates/
  astra-cli/           Interactive TUI, scripting CLI, and local tools
  runtime/             Axum server, API routes, and runtime composition
  astra-turn-core/     Agent-turn orchestration and lifecycle semantics
  services/            Durable sessions, runs, auth, audit, and admin services
  astra-edge/          User Runner provider and cloud connection
  astra-tools/         Tool contracts and shared execution types
  astra-mcp/           MCP integration
  astra-skills/        Skill discovery and execution support
  astra-sandbox/       Managed execution boundaries
packages/sdk/          TypeScript and React SDK
web/                   Next.js dashboard
deployment/            Compose, Kubernetes, and cloud deployment examples
docs/                  Design contracts, guides, reference, and testing docs
```

</details>

Issues and pull requests are welcome. Before submitting a change:

1. Read the owning contract in [`docs/design/`](docs/design/).
2. Add or update tests at the narrowest responsible layer.
3. Run `make check` and the relevant offline or integration lane.
4. Describe user-visible behavior, failure semantics, and provider-boundary
   changes in the pull request.

For substantial behavior changes, open an issue first so the runtime contract
and implementation can evolve together.

---

<div align="center">

**Astra is not another chat window. It is the runtime that helps an agent carry work to a verifiable outcome.**

</div>
