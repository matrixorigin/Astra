# Design documentation index

> Status: canonical documentation map.
> Last updated: 2026-07-15.

The design docs are organized by orthogonal ownership boundaries. If a topic appears to belong in multiple places, write the invariant in the owning domain and reference it elsewhere instead of duplicating it.

These documents describe target contracts. They should not be read as proof that the current implementation already satisfies every requirement.

## Canonical domains

| Domain | Canonical document | Owns |
| --- | --- | --- |
| System architecture | [ARCHITECTURE.md](ARCHITECTURE.md) | Runtime shape, state layers, system-wide invariants. |
| Documentation architecture | [documentation-architecture.md](documentation-architecture.md) | Documentation class rules, domain ownership, and migration policy. |
| Agent/provider model | [agent-backbone-capacity-provider.md](agent-backbone-capacity-provider.md) | Shared backbone semantics and capacity provider contract. |
| Runtime lifecycle | [runtime-lifecycle.md](runtime-lifecycle.md) | Session, run, turn, task, plan, cancel, resume, recovery. |
| Durable runs | [durable-agent-runs.md](durable-agent-runs.md) | Lease, checkpoint, resume, terminal outcome, crash recovery details. |
| Capabilities | [capability-system.md](capability-system.md) | Tools, skills, MCP, provider decisions, admission, fallback. |
| Capability provider runtime | [capability-provider-runtime.md](capability-provider-runtime.md) | Provider adapters, discovery snapshots, internal tool identity, invocation and typed outcome contracts. |
| Skills and tools | [skills-and-tools.md](skills-and-tools.md) | Skill maturity, packaging, lifecycle, compatibility, discovery, evaluation, and rollout. |
| Context/prompt | [context-and-prompt.md](context-and-prompt.md) | Context assembly, prompt cache, compaction, dynamic state, memory injection. |
| Prompt lifecycle | [prompt-lifecycle.md](prompt-lifecycle.md) | Prompt assembly, versioning, stable prefix, cache and evolution boundary. |
| Context window | [context-window-management.md](context-window-management.md) | Token budgets, compaction, eviction, and context preservation. |
| Observation | [observation-plane.md](observation-plane.md) | Trace, audit, introspect, reflect, status, diagnostics. |
| Artifacts/debug bundles | [artifacts-and-debug-bundles.md](artifacts-and-debug-bundles.md) | Artifact manifests, large output handling, raw diagnostic bundle lifecycle. |
| Introspect/reflect | [introspect-and-reflect.md](introspect-and-reflect.md) | Agent self-observation, reflection boundaries, and introspection dimensions. |
| Session observability | [session-observability.md](session-observability.md) | User/support visible status, stream projection, stuck diagnosis, reconnect. |
| Tool result quality | [tool-result-quality-firewall.md](tool-result-quality-firewall.md) | Tool output validation, quality annotation, retry/fallback hints. |
| Edge/cloud execution | [edge-cloud-execution.md](edge-cloud-execution.md) | Edge local capacity and server-safe fallback. |
| Cloud-edge sync | [../architecture/edge-cloud-sync-architecture.md](../architecture/edge-cloud-sync-architecture.md) | Durable outbox, sync facts, repair, retention. |
| Orchestration | [orchestration.md](orchestration.md) | Multi-agent delegation, model selection per agent, coordination. |
| Model routing | [model-routing.md](model-routing.md) | Model/provider selection, escalation, fallback chains, and traceability. |
| Multi-agent runtime | [multi-agent-runtime.md](multi-agent-runtime.md) | Durable child runs, fanout/fanin, delegation lineage, and bounded parallelism. |
| Memory | [memory.md](memory.md) | Cross-session and in-session memory semantics. |
| Safety | [safety-and-permissions.md](safety-and-permissions.md) | Permission, sandbox, side-effect, policy, trust boundaries. |
| Permission sync | [permission-sync.md](permission-sync.md) | Cross-surface scoped approvals, revocation, expiration, and audit. |
| Stop hooks | [stop-hooks.md](stop-hooks.md) | Controlled stop/pause/checkpoint hook points and outcomes. |
| Trust and safety | [trust-and-safety.md](trust-and-safety.md) | Evidence, claim support, trust levels, and audit obligations. |
| Data/storage | [data-and-storage.md](data-and-storage.md) | MatrixOne usage, state layering, retention. |
| MatrixOne-native paradigm | [matrixone-native-paradigm.md](matrixone-native-paradigm.md) | Database-native facts, analytics, replay, and governance leverage. |
| Data versioning | [data-versioning.md](data-versioning.md) | Reproducible decision inputs, snapshots, branching, and replay. |
| Evaluation/learning | [evaluation-and-learning.md](evaluation-and-learning.md) | Eval, feedback, learning artifacts, quality gates. |
| Evaluation | [evaluation.md](evaluation.md) | Case structure, replay modes, regression gates, and behavioral metrics. |
| Feedback control loop | [feedback-control-loop.md](feedback-control-loop.md) | Feedback collection, classification, proposal, activation, and monitoring. |
| Tuning jobs | [tuning-jobs.md](tuning-jobs.md) | Controlled prompt/skill/routing/memory/model improvement workflows. |
| Client/deployment | [client-surfaces-and-deployment.md](client-surfaces-and-deployment.md) | Web/CLI/TUI client boundaries and deployment topology. |

## Anti-duplication rules

- Tool visibility belongs to capability system, not prompt lifecycle.
- Tool execution routing belongs to capability system, not Web Agent runner.
- Plan mode belongs to runtime lifecycle, not a separate agent architecture.
- Introspection and reflection belong to observation plane, not context pipeline.
- Prompt cache belongs to context/prompt, not provider routing.
- Edge local filesystem and shell authority belong to edge-cloud execution and safety, not Web Agent docs.
- Sync durability belongs to cloud-edge sync, not session lifecycle.
- Learning data belongs to evaluation/learning, not raw debug or audit docs.

## Deprecated document pattern

Historical implementation notes should not live in `docs/design/`. If a deleted document contained a still-valid invariant, it should now be represented in one of the canonical documents above.
