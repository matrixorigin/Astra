<div align="center">

# Astra

### The self-hosted runtime for agents that must act inside private systems

**Work survives. Access stays local. Every action is traceable.**

[![Test Suite](https://github.com/matrixorigin/Astra/actions/workflows/test.yml/badge.svg)](https://github.com/matrixorigin/Astra/actions/workflows/test.yml)
[![Static Checks](https://github.com/matrixorigin/Astra/actions/workflows/static-checks.yml/badge.svg)](https://github.com/matrixorigin/Astra/actions/workflows/static-checks.yml)
[![VLDB ADS](https://img.shields.io/badge/VLDB_ADS-Accepted-6F42C1)](https://vldb-ads.top/)
[![arXiv](https://img.shields.io/badge/arXiv-2609.00749-B31B1B?logo=arxiv&logoColor=white)](https://arxiv.org/abs/2609.00749)
[![Terminal-Bench](https://img.shields.io/badge/Terminal--Bench_2.1-67.4%25-0A7EA4)](#terminal-bench-21)
[![Rust 1.97](https://img.shields.io/badge/Rust-1.97-000000?logo=rust)](rust-toolchain.toml)
[![TypeScript](https://img.shields.io/badge/SDK-TypeScript-3178C6?logo=typescript&logoColor=white)](packages/sdk)
[![License](https://img.shields.io/badge/License-Apache%202.0-blue.svg)](LICENSE)

[Why Astra](#why-astra) · [Research](#research-and-benchmarks) · [Quick start](#quick-start) · [Architecture](#architecture) · [Runner](#runner-and-private-enterprise-it) · [Comparison](#how-astra-differs-from-coding-agents) · [Docs](#documentation)

</div>

---

Astra is not another coding agent or a library for wrapping one model call. It
is the open-source, self-hosted runtime behind CLI, Web, and application agents
that must keep working across requests and reconnects, act inside private
environments under the right user or enterprise identity, and leave verifiable
evidence of every action.

| Work survives | Access stays local | Every action is traceable |
| --- | --- | --- |
| Durable Work, checkpoints, retries, and handoffs outlive any request or chat connection. | The Server coordinates; a User Runner executes beside private repositories, tools, data, network, and credentials. | Policy admits each action before execution; Trace and Audit preserve its route, result, and evidence afterward. |

Astra includes the Server, CLI/TUI, Web dashboard, APIs, and TypeScript SDK. You
bring the LLM and embedding endpoints, then connect tools and data through a
Runner or MCP.

**Codex starts with the agent experience. DeepSeek Harness starts with the
plugin graph. Astra starts with durable enterprise Work, then routes each
admitted action to the identity and environment allowed to perform it.**

<div align="center">
  <img alt="Illustrative 20-second Astra flow: a request becomes durable Work on the Server, context and policy determine an execution route, a user-bound Runner acts inside a private environment, and the result returns with trace and audit evidence." src="docs/assets/astra-cli-demo.gif" width="900">
</div>

<p align="center"><sub>The Server keeps the Work. The Runner keeps private access local. Policy governs the boundary. Trace preserves the evidence.</sub></p>

### Pick the layer you need

All three can run models and tools. The difference is what each system makes
authoritative.

| | [Codex](https://developers.openai.com/codex/) | [DeepSeek Harness](https://www.deepseek.com/harness/en/) | **Astra** |
| --- | --- | --- | --- |
| **You are choosing** | A ready-to-use agent and embeddable harness | A composable agent harness | A shared, self-hosted agent runtime |
| **System of record** | The Codex task and workspace | An append-only session event stream assembled from plugins | Durable Work, identity, policy, provider decisions, trace, and audit across users and applications |
| **Execution boundary** | The local or cloud environment selected for the task | Configured tools, sandboxes, storage, and runtime plugins | The Server admits the action; a user-bound Runner executes inside the private environment |
| **Best fit** | Use an agent directly to complete work | Build or customize a coding agent and its runtime | Operate governed infrastructure behind many agents, products, and trust boundaries |

Use Astra when an agent becomes shared infrastructure across users,
applications, private networks, and approval boundaries. If all you need is a
prompt-and-tools loop on one machine, a lighter agent framework or coding agent
is usually simpler.

> **Context decides what the agent knows. Policy decides what it may do. The
> Runner carries out the action where the relevant systems live. Trace makes
> the result explainable and accountable.**

**Research-backed:** Astra's Context Pipeline is presented in
**[ContextPipe: Database-Inspired Context Assembly for Long-Horizon
Agents](https://arxiv.org/abs/2609.00749)**, accepted at
[ADS 2026](https://vldb-ads.top/), co-located with VLDB 2026.

> [!NOTE]
> Astra is under active development and public interfaces may change before
> 1.0. Documents in [`docs/design/`](docs/design/) define target contracts and
> may lead the implementation on a given branch. Current behavior is guarded by
> code, contract tests, and runtime-profile tests.

## Why Astra

A model plus tools is a useful starting point. Enterprise work adds longer time
horizons and fragmented environments: private repositories, internal APIs,
databases, local tools, user credentials, approval boundaries, and systems that
cannot simply be exposed to a hosted agent. The hard problem is no longer just
generating the next answer. It is carrying governed Work from context to
execution and retaining evidence of the result.

Astra makes that full loop part of the runtime:

| Runtime responsibility | Enterprise question | Astra system |
| --- | --- | --- |
| **Durable Work** | How does work survive requests, reconnects, retries, and handoffs? | Agent Kernel: Session, Run, Turn, Work, task graphs, checkpoints, and recovery |
| **Context** | What should the agent know right now? | [Context Pipeline](#context-pipeline): governed assembly, precedence, provenance, budgets, compression, and cache-stable structure |
| **Control** | What is this identity allowed to do? | Policy and provider admission: permission, side effects, routing, fallback, and result quality |
| **Execution** | Where should the action happen? | Server providers, User Runners deployed through CLI or Edge, MCP, and managed sandboxes |
| **Evidence** | What happened, why, and what should happen next? | Trace, Introspect, Explain, Reflect, and Audit |

### From context to execution

<div align="center">
<picture>
  <source media="(prefers-color-scheme: dark)" srcset="docs/assets/diagrams/context-to-execution-dark.svg">
  <img alt="Vertical flow: the Context Pipeline assembles task, enterprise, runtime and memory state; the model decides; policy and provider admission binds identity, capability, permission and execution route; a Runner executes inside the owning environment; and Trace, Introspect, Explain and Reflect feed durable Work and future context back into the pipeline." src="docs/assets/diagrams/context-to-execution.svg" width="660">
</picture>
</div>

<details>
<summary>Text version</summary>

```text
Context Pipeline
      │  assemble task, enterprise, runtime, and memory state
      ▼
Model decision
      │
      ▼
Policy + provider admission
      │  bind identity, capability, permission, and execution route
      ▼
Runner inside the owning environment
      │  tools · workspace · private network · enterprise systems
      ▼
Trace ──► Introspect ──► Explain / Reflect
      │
      └──► durable Work and future context
```

</details>

Models and tools can change. Astra preserves the context, execution boundary,
lifecycle, provider decision, and evidence model around them.

Astra uses the same backbone across **CLI + Server**, **Server-only**, and
**Server + Edge / User Runner** deployments. In private environments, the
Server coordinates while the Runner acts alongside the systems that own the
tools, data, network, and credentials.

If your use case is one stateless model call, a direct LLM API is usually
simpler. Astra is designed for the point where state, tools, permissions,
recovery, collaboration, or operational control become part of the product.

## Research and benchmarks

### ContextPipe

**[ContextPipe: Database-Inspired Context Assembly for Long-Horizon
Agents](https://arxiv.org/abs/2609.00749)** presents Astra's Context Pipeline as
a five-phase system—Plan, Bind, Optimize, Execute, and Feedback—with structured
data sources, deterministic cache-aware optimization, and an EXPLAIN ANALYZE
trace.

Peng Xu, Zuyu Zhang, Yuze Sun, Feng Tian, Long Wang, and Chen Zhang ·
**[Accepted at ADS 2026](https://vldb-ads.top/#program)**, co-located with VLDB
2026 · [arXiv](https://arxiv.org/abs/2609.00749) ·
[PDF](https://arxiv.org/pdf/2609.00749) · [Citation](CITATION.cff)

In a preliminary evaluation on the SWE-bench Pro Qutebrowser subset,
ContextPipe reduced total token volume by **31%**, LLM calls by **23%**, and
response time by **9%** compared with append-only context construction, with a
lower KV cache-hit ratio as the measured tradeoff.

### Terminal-Bench 2.1

[Terminal-Bench](https://github.com/harbor-framework/terminal-bench) evaluates
agents on difficult, realistic terminal tasks. Across its 89 tasks, **Astra
ranks first with 60 verifier-passing results (67.42%)**.

> **Model:** GLM-5.2 for every agent in the comparison.

| Agent | Overall | Easy | Medium | Hard |
| --- | ---: | ---: | ---: | ---: |
| **Astra** | **60 / 89 (67.42%)** | 4 / 4 (100%) | **42 / 55 (76.36%)** | 14 / 30 (46.67%) |
| Pi | 54 / 89 (60.67%) | 4 / 4 (100%) | 36 / 55 (65.45%) | 14 / 30 (46.67%) |
| Hermes | 51 / 89 (57.30%) | 4 / 4 (100%) | 32 / 55 (58.18%) | **15 / 30 (50.00%)** |
| DeepSeek Harness (DSH) | 48 / 89 (53.93%) | 4 / 4 (100%) | 31 / 55 (56.36%) | 13 / 30 (43.33%) |

Astra's lead is clearest on the 55 medium-difficulty tasks: it passes six more
than Pi, ten more than Hermes, and eleven more than DSH. On hard tasks, Astra
ties Pi, finishes one task behind Hermes, and one ahead of DSH.

## Quick start

Two supported paths, both ending at a working agent response. Start with
**Docker** to evaluate Astra without a Rust or Node toolchain; **build from
source** when you want the Web dashboard, CLI-local Runner capacity, or to
change Astra itself. For production topologies, use the
[getting-started guide](docs/quickstart/README.md).

| Path | What you get | Additional prerequisites |
| --- | --- | --- |
| [Docker](#docker) | MatrixOne, Memoria, and `astra-server` from published images, plus prebuilt CLI and User Runner binaries | Docker Compose and OpenSSL |
| [From source](#build-from-source) | The same backbone built locally, plus the Web dashboard and CLI-local Runner capacity | Rust 1.97 and Node.js 24, both pinned in the repository |

Both paths need Git, Make, and at least one supported LLM endpoint. Semantic
memory needs an embedding API; deterministic mock embeddings are also available
for local evaluation and tests.

### Docker

No Rust or Node toolchain. These steps run all the way to a real agent
response, not just a healthy port.

#### 1. Install the client binaries

One checksum-verified archive installs both the `astra` CLI and the
`astra-edge` User Runner — Linux and macOS, `amd64` and `arm64`:

```bash
curl --proto '=https' --tlsv1.2 -fsSL https://raw.githubusercontent.com/matrixorigin/Astra/main/scripts/install-astra.sh | sh
```

#### 2. Run the guided setup

Clone Astra and follow one guided flow from embedding configuration through the
first administrator and model:

```bash
git clone https://github.com/matrixorigin/Astra.git
cd Astra
make stack-setup
```

The guided setup asks for the embedding mode, starts the stack, verifies health
and a memory round trip, then opens an interactive administrator/model setup.
API keys are hidden while typing and the local `.env` is written with owner-only
permissions. Choose mock embeddings for deterministic evaluation; use a real
OpenAI-compatible endpoint for production retrieval.
It runs in Linux/macOS terminals and Windows WSL or Git Bash; on a restricted
Windows shell, use the scripted stack targets and run `astra admin setup`
directly.

For a non-interactive local evaluation, use deterministic mock embeddings:

```bash
MEMORIA_EMBEDDING_PROVIDER=mock make stack-start
```

For semantic memory, set `MEMORIA_EMBEDDING_BASE_URL` and, when required,
`MEMORIA_EMBEDDING_API_KEY`, then run `make stack-start`. The command generates
local secrets, starts Compose, waits for health, and verifies an exact memory
round trip. For lower-level automation, run `make stack-env`, `make stack-up`,
and `make stack-verify` explicitly.

| Service | Default URL |
| --- | --- |
| HTTP API | <http://localhost:17001> |
| Health check | <http://localhost:17001/health> |

#### 2. Check the CLI and service URLs

The stack ships `astra-server`; the prebuilt `astra` binary installed in step 1
drives it:

```bash
astra health
```

The CLI defaults to `http://127.0.0.1:17001`, which is where the stack binds,
so no extra configuration is needed. If you remapped `ASTRA_API_PORT`, set
`ASTRA_API_URL` or pass `--api-url` to each command. Pass `-v <version>` to the
installer, and set `ASTRA_IMAGE` before `make stack-up`, when you need a pinned
CLI and server pair.

#### 3. Bootstrap the admin account

```bash
astra admin setup
```

The wizard asks whether the database is fresh or already has an admin, stores
the returned credentials in the local CLI profile, and probes the model before
declaring it ready. Use `astra admin register` and `astra admin model ...` for
scripted or advanced flows.

#### 4. Register a model

```bash
astra admin model add MODEL_NAME openai \
  --api-key "$LLM_API_KEY" --context-window 128000 \
  --base-url https://your-endpoint/v1
astra admin model check MODEL_NAME
```

`model check` probes the endpoint and reports `is_active` and `connectivity`.
A model reaches `is_active: true` only when the probe succeeds, so this is the
step that tells you routing will work. For more than one model, write a
`.models.yaml` and run
`astra admin model load .models.yaml --update-existing`.

#### 5. First agent response

```bash
astra chat -m "Explain what you can and cannot do in this deployment"
astra session list
```

You are through the loop when the request returns a model response and
`session list` shows the durable session it created.

This stack is Server-only by design. Server-side agent turns, memory, planning,
MCP, and introspection are available, while file, shell, Git, build/test, and
private-network tools stay unavailable until a Runner connects — which is what
the answer above should tell you.

#### 6. Connect a User Runner when local execution is needed

After the CLI has stored your account credentials, expose one deliberate local
workspace to the Server:

```bash
astra-edge --workspace-dir /path/to/workspace
```

The Runner inherits the selected Astra CLI profile and reconnects on transient
disconnects. Stop it to remove that execution capacity; the Server remains
available without ambient access to the machine. Operate the stack with
`make stack-status`, `make stack-logs SERVICE=api`, and `make stack-down`. The
[all-in-one guide](deployment/all-in-one/README.md) covers the server+edge
profile; the [Docker quick start](docs/quickstart/docker.md) covers ports and
troubleshooting.

### Build from source

This path builds the `astra` binary and starts the Server-only profile with the
Web dashboard.

#### 1. Initialize

```bash
git clone https://github.com/matrixorigin/Astra.git
cd Astra

cp .models.yaml.example .models.yaml
make dev-init
```

Configure a real embedding endpoint in `.env` for semantic memory, adding an
API key only when that endpoint requires one, or set
`MEMORIA_EMBEDDING_PROVIDER=mock` for local evaluation. Then configure at least
one model provider in `.models.yaml`. Never commit either local file.

#### 2. Build and start Server-only

```bash
make build-cli-debug
make dev-start

export PATH="$PWD/target/debug:$PATH"
astra health
```

| Service | Default URL |
| --- | --- |
| Web dashboard | <http://localhost:3536> |
| HTTP API | <http://localhost:17001> |
| Health check | <http://localhost:17001/health> |

#### 3. Bootstrap an account and model

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

#### 4. Add a User Runner when local execution is needed

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

Astra has one durable agent backbone and multiple bounded capacity providers.
Interfaces do not own separate agent loops; each environment contributes the
capabilities it can safely execute.

<div align="center">
<picture>
  <source media="(prefers-color-scheme: dark)" srcset="docs/assets/diagrams/architecture-dark.svg">
  <img alt="Experience surfaces sit above a durable control backbone. The Context Pipeline, model decision and policy decision fan out to execution capacity: Server provider, User Runner on CLI or Edge, and MCP or sandbox. The User Runner reaches private enterprise IT. Evidence returns to the Context Pipeline as future context, over durable facts in MatrixOne and Memoria." src="docs/assets/diagrams/architecture.svg" width="900">
</picture>
</div>

<details>
<summary>Text version</summary>

```text
Experience
  Web dashboard · CLI/TUI · TypeScript SDK · API clients
        │
        ▼
Durable control backbone
  Server · Session/Run/Work · identity · orchestration · checkpoints
        │
        ▼
Context Pipeline ──► model decision ──► Policy + provider decision
        ▲                                      │
        │                                      ▼
        │                            Execution capacity
        │                  ┌─────────────┼───────────────┐
        │                  ▼             ▼               ▼
        │           Server provider  User Runner     MCP / sandbox
        │                            CLI or Edge      scoped runtime
        │                               │
        │                               ▼
        │                    Private enterprise IT
        │                 workspace · network · tools · data
        │                               │
        └──── Trace · Introspect · Explain · Reflect · Audit

Durable facts
  MatrixOne · Memoria · transcript · artifacts · checkpoints · trace · audit
```

</details>

One lifecycle connects four system planes: **Intelligence** assembles context,
**Control** owns durable Work and policy, **Execution** supplies bounded
capacity, and **Evidence** preserves facts and turns them into operational
understanding.

### One runtime from CLI to Server to Edge

| Runtime profile | Execution shape | Best suited for |
| --- | --- | --- |
| **CLI + Server** | Server backbone, CLI/TUI interaction, and CLI-local workspace capacity | Developers, operators, automation, and terminal-first work |
| **Server-only** | Web, SDK, or enterprise apps use durable Server state and governed Server-side providers | Central services, knowledge work, and controlled business workflows |
| **Server + Edge / User Runner** | Server dispatches admitted work to a Runner inside a user or enterprise environment | Hybrid cloud/private execution, source code, internal systems, and user-owned environments |

These profiles change available capacity, not agent identity. CLI is an
interaction surface with optional local execution; Server is the durable
orchestration and control backbone; Edge places Runner capacity close to
private IT. All three share one context, policy, lifecycle, and evidence model.

The central invariant is:

```text
enterprise agent = durable Work + governed context + authorized execution + verifiable evidence
```

The backbone owns lifecycle and runtime truth. The Context Pipeline determines
what enters the model boundary. Policy and provider admission determine which
capabilities are visible and where they may execute. Runners and other capacity
providers declare what they can do, where they execute, whether they are
available, and which trust boundary applies. Their results return to the same
Work, trace, audit, and context lifecycle.

| Provider | Typical capacity | Trust boundary |
| --- | --- | --- |
| Server | Shared state, memory, configured network access, reports, control-plane tools | Hosted runtime |
| User Runner (CLI-local or Edge) | Files, shell, Git, builds, tests, local network, local MCP, and access to existing private IT through locally available tools | User or enterprise identity, workspace, network, and runtime |
| MCP | Business APIs, databases, knowledge bases, ticketing, approvals | MCP server and request binding |
| Sandbox / managed runtime | Isolated scripts and provisioned workspace execution | Explicit runtime binding |

When a provider disconnects, its capacity can become unavailable without
erasing the session, plan, memory, or server-side Work. Read the
[architecture overview](docs/design/ARCHITECTURE.md) and
[agent-backbone contract](docs/design/agent-backbone-capacity-provider.md) for
the complete design.

## How Astra differs from coding agents

[Claude Code](https://code.claude.com/docs/en/getting-started),
[Codex](https://developers.openai.com/codex/),
[Pi](https://pi.dev/docs/latest), and
[DeepSeek Harness](https://www.deepseek.com/harness/en/) are strong systems for
completing work or composing an agent harness. Astra expands the system of
record from an agent task or session to an enterprise-owned runtime across
users, applications, private environments, and trust boundaries.

> **Models decide. Runners act. Astra governs and traces the entire loop.**

| System | Primary design center | Astra's distinction |
| --- | --- | --- |
| Claude Code | Developer-facing coding agent across terminal, IDE, tools, and enterprise model endpoints | Astra makes the durable enterprise runtime—not one coding surface—the system of record |
| Codex | Agent product across local, cloud, IDE, automation, and integration surfaces | Astra is model-provider independent and centers self-hosted backbone state, User Runners, and governed providers |
| Pi | Minimal terminal coding harness extended through TypeScript packages, skills, prompts, and themes | Astra centers a distributed Server/Runner architecture, durable Work, enterprise identity, and operations |
| DeepSeek Harness | Local-first, plugin-composable coding agent and runtime with a traceable session log | Astra centers canonical lifecycle state, cross-user control, provider decisions, and user-bound execution |
| **Astra** | **Enterprise context-to-execution runtime** | **One durable backbone connecting governed context to execution across Web, CLI, Server, Edge, MCP, sandboxes, and User Runners** |

The distinction is architectural:

- **The enterprise owns durable Work** — identity, tasks, model routes, policy,
  trace, and audit live in an operable system of record.
- **Context is a pipeline** — structured inputs are assembled, budgeted,
  compressed, traced, and recovered as runtime state.
- **Execution follows authority** — Server, User Runner, MCP, and sandbox
  capacity is explicitly bound, admitted, routed, and governed.
- **Every surface shares semantics** — Web, TUI, SDK, and Runner-backed
  sessions use the same lifecycle, failure, and evidence model.

Coding is an important Astra workload, but it is not the product boundary.
Astra is designed to be self-hosted, embedded, extended, and exposed through
enterprise products.

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

<div align="center">
<picture>
  <source media="(prefers-color-scheme: dark)" srcset="docs/assets/diagrams/run-lifecycle-dark.svg">
  <img alt="A run moves from queued to running to completed. From running it can enter waiting, paused or blocked and return to running, or move to cancelling and then cancelled, or to failed." src="docs/assets/diagrams/run-lifecycle.svg" width="860">
</picture>
</div>

<details>
<summary>Text version</summary>

```text
queued → running → completed
             ├── waiting ──→ running
             ├── paused  ──→ running
             ├── blocked ──→ running
             ├── cancelling → cancelled
             └── failed
```

</details>

### Context Pipeline

The Context Pipeline is a core Astra kernel contribution described in
[ContextPipe](https://arxiv.org/abs/2609.00749), Astra's systems paper accepted
at ADS 2026, co-located with VLDB 2026. It is the intelligence plane between
durable enterprise state and each model decision. Context is treated as a
governed, recoverable data pipeline—not one indefinitely growing prompt string
and not a one-time retrieval step.

<div align="center">
<picture>
  <source media="(prefers-color-scheme: dark)" srcset="docs/assets/diagrams/context-pipeline-dark.svg">
  <img alt="Six structured inputs are assembled, then selected, budgeted and compressed into the model boundary. Decision, usage and checkpoint output returns as future context." src="docs/assets/diagrams/context-pipeline.svg" width="880">
</picture>
</div>

<details>
<summary>Text version</summary>

```text
System contract ─────────────┐
Session · Run · Work ────────┤
Memory · enterprise facts ───┤
Artifacts · tool results ────┤──► assemble ─► select/budget/compress ─► model
Runner · provider · policy ──┤                                      │
Trace · reflection ──────────┘                                      ▼
                                              decision · usage · checkpoint
                                                        │
                                                        └──► future context
```

</details>

At every model boundary, the pipeline turns the current Work, memory,
artifacts, runtime availability, policy state, and prior execution facts into a
bounded context for the next decision. Runner results and Trace then become
inputs to future turns, closing the loop between knowing and acting.

The pipeline provides stable prompt-cache-friendly contracts, typed dynamic
state, explicit precedence and provenance, budget-aware selection, semantic
compression, and reconstruction from checkpoints and durable facts.

### Policy and governed execution

Tool visibility and tool execution use the same lifecycle:

```text
Projection → Admission → Execution → Result
```

Each decision considers identity, mode, side-effect class, permission scope,
workspace authority, provider binding and health, runtime location, fallback
policy, and result quality. The outcome drives the model-visible tool surface,
execution route, user diagnostics, trace, and audit.
Ready, policy-blocked, unbound, offline, unsupported, and fallback outcomes are
explicit runtime facts. A narrow capability failure blocks that action rather
than erasing the session or pretending the capability never existed. See the
[capability contract](docs/design/capability-system.md) for the full state
model.

### Runner and private enterprise IT

A **Runner** is Astra's deployable execution boundary: the place where an
admitted action becomes real work. A **User Runner** binds that execution to a
specific user and workspace. Today this role is provided by `astra-edge` and
the CLI-local runtime.

> **The Server coordinates. The Runner acts.**

| Term | Meaning |
| --- | --- |
| **Runner** | The execution contract that supplies bounded capabilities to the shared agent backbone |
| **User Runner** | A Runner bound to a user's identity, workspace, tools, network, and permission boundary |
| **Edge** | A deployment topology that places Runner capacity near the private systems where work must happen |

Runner and Edge are therefore not synonyms: Runner describes the execution
boundary; Edge describes where that boundary is deployed. A User Runner
describes who owns and authorizes it.

<div align="center">
<picture>
  <source media="(prefers-color-scheme: dark)" srcset="docs/assets/diagrams/runner-boundary-dark.svg">
  <img alt="A user or app submits durable Work to the Astra Server, which owns durable Work, identity, context, policy and the provider decision. An admitted tool call goes to a User Runner inside the user or enterprise trust boundary, which reaches private enterprise IT: file, shell, Git, builds, private network and local MCP. A typed result with execution identity and evidence returns to the backbone." src="docs/assets/diagrams/runner-boundary.svg" width="660">
</picture>
</div>

<details>
<summary>Text version</summary>

```text
User / app
    │ submit durable Work
    ▼
Astra Server ── durable Work · identity · context · policy · provider decision
    │ admitted tool call
    ▼
User Runner ── inside the user or enterprise trust boundary
    │
    ▼
Private enterprise IT
    file · shell · Git · builds · private network · local MCP
    │
    └──── typed result + execution identity + evidence ────► backbone
```

</details>

This is the last-mile integration layer between an agent and the systems where
enterprise work already lives. Instead of exposing every internal system to a
hosted agent or granting the Server ambient machine access, an enterprise can
place Runner capacity alongside its existing workspace, network, tools, and
identity controls.

Runner placement controls execution locality; it does not by itself guarantee
data residency. Model endpoints, context disclosure, and tool-result handling
remain explicit deployment and policy choices.

The Runner contributes bounded execution capacity, not a second agent brain:

- registration and dispatch stay bound to user, Runner, and workspace identity;
- explicit capabilities replace implicit Server access to the user's machine;
- permissions remain enforced where execution occurs;
- heartbeats, journals, reconnects, and reconciliation make results observable,
  recoverable, and part of the same transcript, trace, audit, and checkpoints.

### Trace, Introspect, Explain, Reflect, and Audit

The observation plane is part of the agent contract, not an after-the-fact log
viewer:

<div align="center">
<picture>
  <source media="(prefers-color-scheme: dark)" srcset="docs/assets/diagrams/observation-plane-dark.svg">
  <img alt="Runtime facts flow into Trace, then Introspect, which projects into Explain and Reflect. Trace and policy decisions both feed Audit." src="docs/assets/diagrams/observation-plane.svg" width="700">
</picture>
</div>

<details>
<summary>Text version</summary>

```text
Runtime facts ──► Trace ──► Introspect ──► Explain
                    │             └──────► Reflect
                    │
Policy decisions ───┴─────────────► Audit
```

</details>

| Component | Question it answers | Authority |
| --- | --- | --- |
| **Trace** | What happened, in which causal order, through which model, tool, and provider? | Records measured execution facts |
| **Introspect** | What is the current run, context, task, budget, capability, and blocked state? | Reads and structures current facts |
| **Explain** | Why is the run working, waiting, degraded, blocked, or failed, and what can the user do? | Presents a user-facing projection of facts |
| **Reflect** | What may be wrong, and should strategy change or human help be requested? | Proposes advice; cannot grant permission or complete tasks |
| **Audit** | Which identity, permission, provider, fallback, and side-effect facts remain accountable? | Preserves durable accountability records |

Reflection cannot rewrite runtime truth, Explain does not expose private
chain-of-thought, and debug output is not automatically an audit record.

## Deploy and operate

Astra supports local source development, all-in-one Compose, Kubernetes, and
Server + Edge / User Runner topologies. Start with the
[deployment overview](deployment/README.md).

Runtime and model configuration are intentionally separate:

- [`.env.example`](.env.example) covers database, authentication, Memoria,
  runtime limits, logging, and optional provider bindings.
- [`.models.yaml.example`](.models.yaml.example) defines model endpoints,
  credentials, capabilities, pricing, and fallback chains.
- [`config/server.toml.example`](config/server.toml.example) is the file-based
  server baseline; `ASTRA_*` environment variables take precedence.

Server observability uses structured logs and optional OTLP export, while CLI
diagnostics stay separate from machine-readable output. See the
[configuration reference](docs/reference/configuration.md),
[production guide](docs/quickstart/production.md), and
[troubleshooting guide](docs/guides/troubleshooting.md).

## Documentation

The README is the product overview and shortest runnable path. Detailed
documentation is organized by reader goal:

| I want to... | Start here | Continue with |
| --- | --- | --- |
| **Try and use Astra** | [Getting started](docs/quickstart/README.md) | [CLI commands](docs/reference/cli-commands.md) and [TUI slash commands](docs/reference/slash-commands.md) |
| **Build an application** | [TypeScript SDK](packages/sdk/README.md) | [HTTP API](docs/reference/api-reference.md) and [configuration](docs/reference/configuration.md) |
| **Deploy and operate** | [Deployment overview](deployment/README.md) | [Configuration](docs/reference/configuration.md) and [troubleshooting](docs/guides/troubleshooting.md) |
| **Get help or report a problem** | [Support](SUPPORT.md) | [Questions, bug reports, documentation issues, and feature requests](https://github.com/matrixorigin/Astra/issues/new/choose) |
| **Develop and contribute** | [Contributing guide](CONTRIBUTING.md) | [Developer setup](docs/quickstart/development.md), [workflow](docs/guides/development-workflow.md), [testing](docs/guides/testing.md), and [Make targets](docs/reference/makefile-commands.md) |
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

## Citation

If Astra or the Context Pipeline is useful in your research, please cite the
ContextPipe paper:

```bibtex
@inproceedings{xu2026contextpipe,
  title         = {ContextPipe: Database-Inspired Context Assembly for Long-Horizon Agents},
  author        = {Xu, Peng and Zhang, Zuyu and Sun, Yuze and Tian, Feng and Wang, Long and Zhang, Chen},
  booktitle     = {Applied Data Science Track (ADS 2026), co-located with VLDB 2026},
  year          = {2026},
  eprint        = {2609.00749},
  archivePrefix = {arXiv},
  url           = {https://arxiv.org/abs/2609.00749}
}
```

[`CITATION.cff`](CITATION.cff) also lets GitHub generate APA and BibTeX entries
for the software itself through **Cite this repository**.

## License

Astra is licensed under the [Apache License, Version 2.0](LICENSE).

---

<div align="center">

**Astra connects enterprise context to governed execution across CLI, Server, and Edge—with traceability built in.**

</div>
