# Astra documentation

The root [README](../README.md) is the product overview and fastest path to a
running Astra instance. This index routes users, application developers,
operators, and kernel contributors to the level of detail they need.

> [!IMPORTANT]
> Quickstarts, guides, and references describe supported implementation paths.
> Documents in [`design/`](design/) are normative target contracts and may be
> ahead of the implementation on a given branch. Code, contract tests, and
> runtime-profile tests remain authoritative for current behavior.

## Choose your path

| Goal | Start here | Next |
| --- | --- | --- |
| **Evaluate or use Astra** | [Getting started](quickstart/README.md) | [CLI reference](reference/cli-commands.md), [TUI slash commands](reference/slash-commands.md), [examples](../examples/README.md) |
| **Build a product on Astra** | [TypeScript SDK](../packages/sdk/README.md) | [HTTP API](reference/api-reference.md), [configuration](reference/configuration.md) |
| **Deploy and operate Astra** | [Deployment overview](../deployment/README.md) | [Docker](quickstart/docker.md), [production](quickstart/production.md), [troubleshooting](guides/troubleshooting.md) |
| **Develop or contribute** | [Developer setup](quickstart/development.md) | [Development workflow](guides/development-workflow.md), [testing](guides/testing.md), [Make targets](reference/makefile-commands.md) |
| **Understand or extend the kernel** | [Architecture](design/ARCHITECTURE.md) | [Design index](design/README.md) and the [core reading path](#kernel-design-reading-path) below |

## Use and integrate Astra

| Document | What it covers |
| --- | --- |
| [Quick start](quickstart/README.md) | Source and Docker entry points, first health check, and where to go next |
| [CLI commands](reference/cli-commands.md) | Authentication, chat, sessions, models, skills, and administration |
| [TUI slash commands](reference/slash-commands.md) | Interactive workspace, planning, observability, memory, MCP, and team commands |
| [TypeScript SDK](../packages/sdk/README.md) | REST, SSE, WebSocket, React hooks, and browser integration |
| [HTTP API](reference/api-reference.md) | Authentication and server resource contracts |
| [Examples](../examples/README.md) | Maintained hands-on flows and integration-test examples |

## Deploy and operate Astra

| Document | What it covers |
| --- | --- |
| [Deployment overview](../deployment/README.md) | Supported deployment shapes and runtime-profile validation |
| [Docker quick start](quickstart/docker.md) | All-in-one Compose and API-container development |
| [Production deployment](quickstart/production.md) | Required secrets and Server-only or Server + User Runner startup |
| [Deployment guide](guides/deployment.md) | Recommended operational path and health verification |
| [Configuration reference](reference/configuration.md) | Server, database, authentication, model, Runner, and observability settings |
| [Troubleshooting](guides/troubleshooting.md) | First diagnostics for dependencies, server startup, and tests |
| [Run projection repair](guides/run-projection-repair.md) | Repair procedure when a derived run view is stale |

## Develop and contribute

| Document | What it covers |
| --- | --- |
| [Developer setup](quickstart/development.md) | Prerequisites, repository layout, local loop, and code conventions |
| [Development workflow](guides/development-workflow.md) | Server-only, Server + User Runner, and Docker development profiles |
| [Testing guide](guides/testing.md) | Offline, contract, online, and system test lanes |
| [Makefile reference](reference/makefile-commands.md) | Build, validation, test, and development targets |
| [Dependencies](reference/dependencies.md) | Required and optional development tools |
| [System E2E matrix](testing/system-e2e-matrix.md) | Cross-surface runtime behavior and coverage obligations |
| [Capability harness](testing/capability-harness.md) | Capability-provider and model/tool test contract |
| [Coverage matrix](testing/coverage-matrix.md) | Feature-to-test coverage map |

Before changing runtime behavior, read the owning design contract. Run the
narrowest relevant test while iterating, then `make check` and the applicable
offline or online lane before submitting a change.

## Kernel design reading path

Start with [Architecture](design/ARCHITECTURE.md), then use this path according
to the subsystem you are changing:

| Concern | Canonical design documents |
| --- | --- |
| One backbone and runtime profiles | [Agent backbone and capacity providers](design/agent-backbone-capacity-provider.md), [client surfaces and deployment](design/client-surfaces-and-deployment.md) |
| Durable Work and orchestration | [Runtime lifecycle](design/runtime-lifecycle.md), [durable agent runs](design/durable-agent-runs.md), [orchestration](design/orchestration.md) |
| User Runner and hybrid execution | [Edge-cloud execution](design/edge-cloud-execution.md), [edge runtime tool boundary](design/edge-runtime-tool-boundary.md), [Web agent runner](design/web-agent-runner.md), [cloud-edge sync](architecture/edge-cloud-sync-architecture.md) |
| Tools, providers, and policy | [Capability system](design/capability-system.md), [provider runtime](design/capability-provider-runtime.md), [safety and permissions](design/safety-and-permissions.md), [tool-result quality firewall](design/tool-result-quality-firewall.md) |
| Context Pipeline | [ContextPipe paper](https://arxiv.org/abs/2609.00749), [context and prompt](design/context-and-prompt.md), [prompt lifecycle](design/prompt-lifecycle.md), [context-window management](design/context-window-management.md), [memory](design/memory.md) |
| Trace, Explain, Introspect, and Reflect | [Observation plane](design/observation-plane.md), [introspect and reflect](design/introspect-and-reflect.md), [session observability](design/session-observability.md), [artifacts and debug bundles](design/artifacts-and-debug-bundles.md) |
| Models, data, and learning | [Model access and inference](design/model-access-and-inference.md), [data and storage](design/data-and-storage.md), [evaluation and learning](design/evaluation-and-learning.md), [tuning jobs](design/tuning-jobs.md) |

The [design index](design/README.md) is the complete map of design domains and
ownership boundaries.

## Documentation classes

| Directory | Contract |
| --- | --- |
| `quickstart/` | Short, outcome-oriented first-run paths |
| `guides/` | Task-oriented procedures, workflows, and runbooks |
| `reference/` | Current commands, APIs, configuration, and dependencies |
| `design/` | Normative architecture and target behavior |
| `architecture/` | Cross-domain architecture views |
| `testing/` | Test strategy, matrices, and coverage contracts |

## Documentation rules

- One design domain has one canonical document.
- Describe invariants, responsibilities, state, and failure semantics—not
  implementation chronology.
- Keep transient plans and verification transcripts out of durable docs.
- Link to a source of truth instead of duplicating it.
- Update the relevant quickstart, guide, or reference whenever a public
  workflow or interface changes.
- State test obligations for every behavioral design contract.

The governing principle is simple: Astra has one agent backbone and multiple
capacity providers. Web, CLI, Server, User Runner, MCP, and future providers
share lifecycle, context, policy, trace, reflection, checkpoint, and audit
semantics; capability differences come from providers, not separate agent
implementations.
