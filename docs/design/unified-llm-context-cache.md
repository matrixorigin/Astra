# Unified LLM Context and Prompt Cache Assembly

> Status: Implementation integrated; static audit complete; `cargo check -p astra-runtime` and `cargo check -p astra-cli` passed; targeted context-cache contract tests passed
> Date: 2026-05-15
> Scope: CLI agent, web agent, server agentic loop, `/chat/turn` bridge, prompt/context cache assembly, context manifest emission
> Baseline: `a77f397fd78a4857b653eff5c85325098e833f13`

## Summary

Astra currently has a strong prompt/context cache design in the CLI-era agentic loop implementation. The important behavior from `a77f397fd78a4857b653eff5c85325098e833f13` is:

- Stable context is placed before volatile context.
- `Global`, `Session`, and `None` cache scopes are explicit.
- `RuntimeIdentity` and `RuntimeVolatile` are split.
- Provider cache behavior is normalized through a provider cache policy.
- Anthropic and Bedrock use explicit cache markers.
- OpenAI-compatible providers rely on byte-stable prefix caching.
- MiniMax-style strict-history providers suppress volatile injection when needed.
- Tool schemas are selected, always-loaded, pruned, and cache-aligned.
- Message history and tool results are compacted through one shared wire assembly path.

The latest `main` already contains part of the right shared architecture: `ContextPipeline`, `context_pipeline_adapter`, `PipelineSession`, and `wire_assembly`. However, the CLI and web agent still differ in where context is collected, how tool visibility is decided, how the bridge lifecycle carries pipeline state, and how the new web-agent `context_manifest` is produced.

This document proposes a single shared LLM context assembly interface. CLI and web agent should remain different only at the source collection layer. Once sources are collected, both must use the same prompt planning, binding, optimization, serialization, compaction, provider cache annotation, and context manifest trace generation.

## Implementation status

The initial integration is implemented around `crates/runtime/src/turn/llm_context.rs`.

Implemented entry points:

- `assemble_bridge_context`: CLI/HTTP bridge adapter that preserves the mature prompt-cache behavior from `prompt_cache::assemble_bridge_pipeline_outcome`.
- `assemble_context_pipeline`: server/web-agent adapter over `ContextPipeline` and provider-aware stable/volatile splitting.
- `assemble_wire_messages`: shared server/web wire-message stitching over `wire_assembly`.
- `finalize_bridge_wire_messages`: bridge-specific final wire normalization for stale reasoning stripping and volatile user-tail placement.
- `annotate_tool_schemas_for_cache` and `apply_message_cache_metadata`: common cache annotation wrappers.
- `build_context_manifest_projection`: common persisted context-manifest projection.
- `augment_manifest_trace_with_wire`: final request-shape trace for message/tool counts and `cache_control` marker counts.

Current integration points:

- CLI bridge calls `llm_context::assemble_bridge_context`, preserves skill listing, and emits `context_manifest_trace` through SSE.
- CLI loop execution copies `ChatTurnSseAccum.context_manifest_trace` back into `AgenticLoopState` before context-manifest persistence.
- Web/server host calls `llm_context::assemble_context_pipeline`, `assemble_wire_messages`, and common tool-schema cache annotation.
- Effective tool schemas are merged as `always_load -> required -> dynamic -> remaining visible` and restricted inside the shared context module, so callers do not need to duplicate final deny-list filtering.
- `context_meta` SSE event construction is centralized in the shared context module.
- External run-event transformation preserves both already-shaped and persisted `context_meta` events.
- Web/server `context_meta` is emitted after final wire cache annotation, so the trace includes final wire marker counts.
- CLI bridge re-emits `context_meta` after context-window aggressive retry rewrites the wire request, so the final accumulated trace reflects the retried messages/tools.
- CLI bridge fixture/E2E and real-provider paths both emit the shared `context_meta` shape.
- The context-manifest writer consumes `AgenticLoopState.last_llm_context_manifest_trace` through the shared projection builder.
- Context pipeline aborts are propagated as classified errors; the shared module does not substitute an emergency fallback prompt.
- Pre-LLM failures without an assembly trace do not write a synthetic LLM-call manifest.

## Design goal

The end state should be:

```text
CLI source adapter
  -> UnifiedLlmContextAssembler
  -> LLM request
  -> usage feedback

Web agent source adapter
  -> UnifiedLlmContextAssembler
  -> LLM request
  -> usage feedback
```

The assembler is the only place that owns:

- Context section manifest construction.
- Cache scope ordering.
- Runtime identity versus runtime volatile placement.
- Provider cache policy.
- Anthropic, Bedrock, OpenAI-compatible, and strict-history cache behavior.
- Tool schema selection output normalization.
- Tool schema pruning.
- Always-load tool cache-prefix layout.
- Message compaction.
- Tool result compaction.
- Volatile preamble placement.
- Anthropic message and tool cache marker annotation.
- Token usage and context trace output.
- Context manifest emission inputs.

CLI and web agent should own only:

- How user messages enter the system.
- How tool catalogs are discovered.
- How tool execution happens.
- How memory, project context, session facts, approvals, and delegation state are fetched.
- How events are streamed to the local terminal or browser.

## Non-goals

This design does not propose a new prompt.

This design does not replace the existing `ContextPipeline` abstraction.

This design does not make the web-agent DB `context_manifest` the prompt builder.

This design does not introduce a fallback prompt path. Any emergency behavior must be explicit, configured, and visible in traces.

## Current state analysis

### Shared core that already exists

The current code already has the right shared primitives:

- `crates/astra-turn-core/src/context_sources.rs`
- `crates/astra-turn-core/src/pipeline_session.rs`
- `crates/runtime/src/turn/context_pipeline_adapter.rs`
- `crates/runtime/src/turn/wire_assembly.rs`
- `crates/runtime/src/turn/prompt_cache.rs`

The intended shape is clear:

```text
AgenticLoopState
  -> context_pipeline_adapter
  -> ContextSources
  -> PipelineSession::run_turn_adaptive
  -> serialized provider request
  -> wire_assembly::assemble_llm_messages
```

`context_pipeline_adapter` is already documented as the sole translation point from runtime state into typed pipeline inputs. It extracts edge profile data, environment static and volatile sections, skill listing, memory entries, effort hints, plan context, and tool guidance.

`wire_assembly` already states that it is shared by the server loop host and HTTP bridge. It owns Memoria compaction, continuation prompt insertion, volatile preamble folding, attachment reinjection, stale reasoning stripping, and Anthropic cache metadata application.

This should become the hard architectural boundary: no CLI-specific or web-specific path should assemble cache-sensitive prompt layout by hand after this point.

### CLI path today

The CLI path prepares a `/chat/turn` payload before the runtime bridge or server host constructs the final LLM request.

Relevant current source:

- `crates/astra-cli/src/cli/chat_stream/sse_loop/agentic_loop_turn.rs`
- `crates/astra-cli/src/cli/chat_stream/sse_loop/mod.rs`
- `crates/astra-turn-core/src/chat_turn_payload.rs`
- `crates/astra-turn-core/src/agentic_prepare_payload.rs`

The CLI path contributes these context sources:

| Source | Current behavior | Desired unified representation |
| --- | --- | --- |
| Conversation messages | `messages` in `/chat/turn` payload | `TurnState.messages` |
| Passive workspace diagnostics | Appended to payload messages | Typed volatile runtime source, not raw ad-hoc message injection |
| Base edge profile | CWD, git branch, workspace context, Memoria URL | `SessionContext.edge_profile` plus stable/volatile environment split |
| Active skills detected from user message | Merged into `edge_profile.active_skills` | `TurnState.active_skills` or stable skill source metadata |
| Skill listing prefix | Routed through `edge_profile` instead of leading system message | Stable `Session` section when listing is session-stable |
| Memory boost | Top-k memory search, semantic query normalization, digest rendering | `ExternalSources.memory_entries` plus optional volatile recall digest |
| Tool schemas | Declarative always-load core plus compact deferred catalog; explicit `tool_search` activation materializes additional full schemas | `ToolSurfacePlan.selected_schemas` |
| Invoked tool retention | Re-add schemas for tools used in tool loop | `ToolSurfacePlan.retained_invoked_schemas` |
| Skill allowed tools | Force-inject schemas declared by active skill | `ToolSurfacePlan.required_schemas` |
| Deferred tool catalog | Edge profile deferred block | `SessionContext.deferred_tools_block` |
| Runtime turn overrides | effort, agent type, subtask, rollback hints | `ExternalSources.effort_hint`, `plan_context`, or typed policy fields |
| Self-awareness | Injected into edge profile if meaningful | Stable or volatile runtime source depending on signal lifetime |
| Recent arg hints | Injected into edge profile | `RuntimeVolatile` |
| Memoria insights text | Injected into edge profile | `Memory` section or `RuntimeVolatile`, not both |
| Gateway append system prompt | Currently inserted as leading system message | Must become typed stable or volatile section with declared cache scope |
| Tool results | Callback-style `tool_results` in payload | `TurnState.tool_results` or message/tool-result pairs before compaction |

The CLI does a lot of useful pre-selection. That should not be deleted. The issue is that the output of that selection should be a typed context assembly input, not a partially assembled prompt shape.

### Web agent path today

The web agent runs primarily through the server-side agentic loop host.

Relevant current source:

- `crates/runtime/src/server/run_lifecycle.rs`
- `crates/runtime/src/server/server_loop_host.rs`
- `crates/runtime/src/server/server_tool_executor.rs`
- `crates/runtime/src/turn/agentic_loop_execution_phase.rs`
- `crates/runtime/src/turn/agentic_loop_tool_phase.rs`

The web path contributes these context sources:

| Source | Current behavior | Desired unified representation |
| --- | --- | --- |
| User message | Initial `AgenticLoopState.messages` | `TurnState.messages` |
| Server edge tools | Server-side tool catalog or browser/edge-provided tools | `ToolSurfacePlan` |
| Server-side tool executor | Executes tools without CLI edge agent | Execution concern, not prompt layout concern |
| Edge profile | Server workspace, CWD, branch, runtime metadata | `SessionContext.edge_profile` plus stable/volatile split |
| Project context | `state.project_context` | `SessionContext.project_context` |
| Current run/session IDs | Stored on `AgenticLoopState` | Metadata only; must not leak into prompt unless semantically needed |
| Invoked skills | Re-injected after compaction by server path attachments | `PostCompactAttachments.invoked_skills` |
| Recent file reads | Re-injected after compaction by server path attachments | `PostCompactAttachments.recent_file_reads` |
| Session facts | Available in state and memory service | Typed source for memory/working context |
| Session history tools | Server tools query transcript/history chunks | Tool execution concern; previews enter prompt via tool result compaction |
| Tool output batches | Persisted after tool phase | Manifest/audit source; prompt sees compacted previews |
| Context manifest pool | Enables per-call DB manifest writes | Manifest sink, not prompt source |

The server host already uses the shared pipeline:

```text
ServerAgenticLoopHost::execute_turn
  -> filtered_turn_tools
  -> PromptCacheConfig::latch
  -> run_turn_pipeline
  -> compact_messages_via_memoria
  -> assemble_llm_messages
  -> annotate_tool_schemas_for_caching
  -> call LLM
```

This was close to the target architecture before this implementation. The CLI bridge, web host, context manifest writer, and cache-sensitive input preparation now share the `runtime::turn::llm_context` contract. Compile validation has passed for `astra-runtime` and `astra-cli`; targeted context-cache contract tests have passed.

### Web-agent context manifest today

The latest `main` adds a per-LLM-call context manifest writer in:

- `crates/runtime/src/turn/agentic_loop_execution_phase.rs`
- `crates/services/src/context_manifest.rs`

It writes a DB record after each attempted LLM call. The manifest currently records zones like:

| Zone | Meaning |
| --- | --- |
| `session_anchor` | Reference to the agent run/session |
| `recent_tail` | Runtime messages snapshot |
| `system_tool_schemas` | Visible tool schema budget estimate |
| `tool_previews` | Tool result preview budget estimate |
| overflow `recent_tail` | Excluded progressive-loading item when estimated input exceeds cap |

This is useful as an audit and budget ledger. It is not the actual prompt assembly engine.

Current limitations:

- It estimates from `pre_llm_messages`, `state.tool_results`, and `state.always_load_tool_schema_tokens`.
- It does not consume the exact `PipelineSession` output.
- It records a fixed `budget_v1_8k` view even when the actual model context window is larger.
- It can drift from the real prompt if the wire assembler moves volatile content, reinjects attachments, prunes schemas, or annotates cache markers.

The manifest should be generated from the unified assembler output, not from a second estimation path.

## Baseline behavior from `a77f397fd78a4857b653eff5c85325098e833f13`

The unified module must preserve these behaviors as golden semantics.

### Section order and scope

Prompt sections are ordered by stability:

```text
Identity
Constraints
SelfModel
ProjectContext
Skills
RuntimeIdentity
RuntimeVolatile
WorkingMemory
Memory
Emergent sections
```

`Global` and `Session` sections form the stable cacheable prefix. `None` sections are turn-volatile and must not disturb the stable prefix.

### Runtime identity split

`RuntimeIdentity` is session-stable. It may include model, CWD, branch, stable environment data, output style, project context references, deferred tools, and stable skill listing.

`RuntimeVolatile` is per-turn. It may include effort hints, plan context, tool guidance, volatile git state, recent arg hints, memory recall snippets, and runtime nudges.

Session UUIDs and run UUIDs must not be emitted into prompt text unless a specific user-visible semantic need exists.

### Provider-aware cache behavior

Provider behavior is selected through a cache policy:

| Provider family | Cache behavior |
| --- | --- |
| Anthropic | Explicit `cache_control` markers |
| Bedrock Claude | Anthropic-style markers translated to Bedrock `cachePoint` |
| OpenAI-compatible | Byte-stable prefix caching, no `cache_control` |
| Strict-history OpenAI-compatible models | Avoid volatile history churn, including suppression when required |

The caller must not decide cache marker placement. The shared assembler must own it.

### Tool schema handling

Tool schemas are one of the largest prompt components. The baseline behavior is:

- Select schemas by semantic task need.
- Keep an always-load stable tool prefix.
- Re-add invoked tools during tool loops.
- Inject required skill tools if selection missed them.
- Prune schemas by compaction tier.
- Annotate the last always-load tool schema for Anthropic cache prefix.
- Keep dynamic or newly discovered tools after the always-load prefix.

### Message and volatile handling

For prefix-cache providers, volatile content must be prepended to the last user message rather than inserted as early system/history content. This preserves byte identity for the stable prefix.

For explicit-marker providers, volatile content must remain outside the marked stable prefix.

Mid-history volatile messages should be consolidated into the volatile tail lane.

### Rolling message cache markers

Anthropic-compatible paths should use rolling historical and tail message markers so tool-loop history becomes cacheable over time.

### Token accounting

Token usage must remain split into disjoint buckets:

```text
fresh input tokens
cached input tokens
cache creation tokens
output tokens
```

The assembler and feedback path must not recombine these into ambiguous totals.

## Proposed architecture

### New module boundary

Create a shared runtime module:

```text
crates/runtime/src/turn/llm_context/
```

Suggested files:

```text
mod.rs
input.rs
output.rs
assembler.rs
tool_surface.rs
manifest_trace.rs
adapters.rs
```

The exact paths are flexible, but the boundary is not: CLI/web/bridge/server code should call one public assembly entry point.

### Unified input

```rust
pub struct LlmContextAssemblyInput<'a> {
    pub session: LlmSessionIdentity<'a>,
    pub provider: ProviderIdentity<'a>,
    pub messages: &'a [serde_json::Value],
    pub tool_results: &'a [serde_json::Value],
    pub tool_surface: ToolSurfacePlan,
    pub edge_profile: EdgeProfileInput<'a>,
    pub project_context: Option<&'a str>,
    pub memory_entries: Vec<MemoryEntry>,
    pub runtime_signals: RuntimeSignals,
    pub attachments: PostCompactAttachmentInput<'a>,
    pub pipeline_session: &'a mut PipelineSession,
    pub token_state: TokenAccountingInput,
    pub turn_state: TurnControlState,
    pub manifest_sink: Option<ContextManifestSink>,
}
```

This input must be constructed by adapters, not by the assembler.

### Tool surface contract

```rust
pub struct ToolSurfacePlan {
    pub selected_schemas: Vec<Value>,
    pub always_load_schemas: Vec<Value>,
    pub dynamic_schemas: Vec<Value>,
    pub required_schemas: Vec<Value>,
    pub deferred_tools_block: String,
    pub restricted_tools: HashSet<String>,
}
```

The CLI adapter can fill this from the final visible tool surface.

The web adapter can fill this from server-side tools, browser/edge tools, plugin schemas, MCP tools, and any server-side tool search/deferred activation result.

After this point, both paths use identical pruning, pinning order, and cache annotation.

### Runtime signal contract

```rust
pub struct RuntimeSignals {
    pub stable_sections: Vec<PromptSection>,
    pub volatile_sections: Vec<PromptSection>,
    pub effort_hint: Option<String>,
    pub system_override: Option<String>,
    pub plan_context: Option<String>,
    pub tool_guidance: Option<String>,
}
```

Every runtime signal must declare whether it is stable or volatile. No caller may inject raw `role: system` messages for runtime context after this boundary.

Examples:

| Signal | Scope |
| --- | --- |
| Stable skill listing | `Session` |
| Deferred tools block | `Session` |
| Output style | `Session`, if latched |
| Static environment | `Session` |
| Dirty git state | `None` |
| Recent argument hints | `None` |
| Tool round guidance | `None` |
| Low confidence selector warning | `None` |
| Plan resume hint | Usually `None`, unless plan ID and step are stable for the visible turn |
| Memory recall digest | `None` |

### Unified output

```rust
pub struct LlmContextAssemblyOutput {
    pub llm_messages: Vec<Value>,
    pub tool_schemas: Vec<Value>,
    pub system_messages: Vec<Value>,
    pub compacted_messages: Vec<Value>,
    pub cache_policy: ProviderCachePolicy,
    pub compaction_tier: CompactionTier,
    pub token_estimates: ContextTokenEstimates,
    pub system_breakdown: SystemPromptBreakdown,
    pub manifest_trace: ContextManifestTrace,
    pub pipeline_output: PipelineTurnSummary,
}
```

The LLM caller should use `llm_messages` and `tool_schemas` directly. It should not reassemble messages, re-prune schemas, or re-place cache markers.

### Assembly flow

The unified assembler should perform:

```text
1. Normalize host inputs.
2. Build ExternalSources from runtime signals.
3. Build SessionContext from session/provider/model/project context.
4. Build AgentContext from ToolSurfacePlan.
5. Build TurnState from messages/tool results/token state.
6. Run PipelineSession::run_turn_adaptive.
7. Split stable and volatile system blocks according to provider capability.
8. Compact messages through MemoriaContext using the pipeline-selected tier.
9. Append continuation prompt when compaction boundary requires it.
10. Reinject post-compaction attachments.
11. Fold volatile content into the correct wire position.
12. Strip stale reasoning.
13. Annotate messages and tools for provider cache.
14. Produce ContextManifestTrace from the actual assembled output.
```

This is mostly present today, but spread between `server_loop_host`, `prompt_cache`, bridge helpers, `context_pipeline_adapter`, and `wire_assembly`. The design is to make that sequence explicit and callable by both CLI and web paths.

## CLI adapter design

The CLI adapter should preserve CLI's existing strengths:

- Semantic query extraction from active task attachments.
- Memory boost search and ranking.
- Preferred repo boost terms.
- Tool registry selection.
- Invoked tool schema retention.
- Skill `allowed_tools` injection.
- Deferred tool surface text.
- Permission and interaction-mode restrictions.
- Self-awareness gating.
- Recent argument hints.
- Turn identity and trace metadata.

But it should stop producing partially prompt-shaped structures.

Current CLI output should map as follows:

| Current CLI output | New target |
| --- | --- |
| `payload.messages` | `LlmContextAssemblyInput.messages` |
| `payload.edge_tools` | `ToolSurfacePlan.selected_schemas` |
| `payload.tool_results` | `LlmContextAssemblyInput.tool_results` |
| `edge_profile.environment_static` | stable runtime section |
| `edge_profile.environment_volatile` | volatile runtime section |
| `edge_profile.memoria_insights_text` | memory entries or volatile memory digest |
| `edge_profile.recent_arg_hints_text` | volatile runtime section |
| `edge_profile.self_awareness_text` | stable or volatile runtime section based on signal type |
| `EDGE_PROFILE_KEY_DEFERRED_TOOLS_TEXT` | `SessionContext.deferred_tools_block` |
| skill listing text | `SessionContext.skill_listing_block` or stable section |
| `append_system_prompt` | typed stable/volatile section; never direct message insertion |

The CLI can still call `/chat/turn`, but the bridge/server side should deserialize this into the unified input and call the same assembler as web agent.

## Web agent adapter design

The web agent adapter should preserve web-specific capabilities:

- Server-side tool execution.
- Browser session streaming.
- DB-backed session/run identity.
- Session transcript tools.
- Tool output batch persistence.
- Context manifest persistence.
- Browser disconnect cancellation.
- Workspace artifact persistence.
- Plan-mode server tools.
- Server-side MCP/plugin schema discovery.

These are source and execution concerns. They should not fork prompt/cache assembly.

Current web/server state should map as follows:

| Current web/server state | New target |
| --- | --- |
| `AgenticLoopState.messages` | `LlmContextAssemblyInput.messages` |
| `ServerAgenticLoopHost.edge_tools` | `ToolSurfacePlan.selected_schemas` |
| `state.tool_results` | `LlmContextAssemblyInput.tool_results` |
| `state.project_context` | `SessionContext.project_context` |
| `ServerAgenticLoopHost.edge_profile` | `EdgeProfileInput` |
| `state.skills.invoked` | `PostCompactAttachmentInput.invoked_skills` |
| `state.recent_file_reads` | `PostCompactAttachmentInput.recent_file_reads` |
| `state.session_facts` | memory/session-facts source |
| `state.context_manifest_*` | `ContextManifestSink` |
| `state.pipeline_session` | shared pipeline session |

The web host should no longer own a separate `run_turn_pipeline` implementation. It should call `UnifiedLlmContextAssembler::assemble`.

## Context manifest integration

The DB `context_manifest` must become a projection of the actual assembly output.

Today it estimates:

```text
pre_llm_messages
state.tool_results
state.always_load_tool_schema_tokens
```

The new source of truth should be:

```text
LlmContextAssemblyOutput.manifest_trace
```

The trace should include:

```rust
pub struct ContextManifestTrace {
    pub policy_version: String,
    pub model_context_window_tokens: u32,
    pub budget_template_id: Option<String>,
    pub turn_intent: Option<String>,
    pub zones: Vec<ContextManifestZoneTrace>,
    pub dropped: Vec<ContextDroppedItemTrace>,
    pub cache: ContextCacheTrace,
}
```

Zones should be derived from real pipeline sections and wire components:

| Zone | Derived from |
| --- | --- |
| `session_anchor` | session/run metadata, reference only |
| `identity` | `Identity` section |
| `constraints` | `Constraints` section |
| `project_context` | `ProjectContext` section |
| `skills` | `Skills` and skill listing |
| `runtime_identity` | stable runtime identity |
| `runtime_volatile` | volatile runtime sections |
| `memory` | selected memory entries |
| `working_memory` | pipeline working memory |
| `recent_tail` | compacted conversation messages |
| `tool_previews` | compacted tool results and post-compaction file previews |
| `system_tool_schemas` | final pruned and annotated tool schemas |
| `attachments` | invoked skills and recent files restored after compaction |
| `overflow` | explicit dropped/spilled/compacted items |

The existing `budget_v1_8k` can remain as a UI-friendly budget template, but it must not be treated as the true model context budget. The trace should record both:

```text
actual_model_context_window_tokens
manifest_display_budget_template_id
```

If a zone is estimated under a display template, the trace should say so.

## Feedback loop

After each LLM call, the unified caller should feed usage back into the same `PipelineSession`:

```text
fresh input
cached input
cache creation
output
truncation/error signals
section fingerprints
compaction effectiveness
```

This feedback must be shared by CLI and web agent. Otherwise, cache-break detection and adaptive compaction will improve one path but not the other.

## Error and fallback policy

This design should not add silent fallback behavior.

The current code has some defensive fallback paths, such as emergency system content on pipeline abort or reference-only manifest degrade paths. The unified design should make these explicit:

```rust
pub enum ContextAssemblyFailurePolicy {
    FailClosed,
    EmergencyPromptWithAudit,
}
```

Default production behavior should be chosen deliberately. A fallback must emit:

- A structured trace alert.
- A user-visible or operator-visible event.
- A context manifest item with `included=false` and a concrete reason.
- No hidden prompt shape change.

For cache-sensitive paths, silent fallback is especially dangerous because it can destroy prefix stability while making the request appear successful.

## Migration plan

### Phase 1: Introduce shared types

Add `LlmContextAssemblyInput`, `ToolSurfacePlan`, `RuntimeSignals`, `PostCompactAttachmentInput`, and `LlmContextAssemblyOutput`.

No behavior changes yet.

### Phase 2: Extract server host assembly

Move these responsibilities out of `ServerAgenticLoopHost` into the shared assembler:

- `run_turn_pipeline`
- `compact_messages_via_memoria`
- server wrapper around `wire_assembly::assemble_llm_messages`
- tool schema cache annotation
- context manifest trace construction

`ServerAgenticLoopHost::execute_turn` should orchestrate model resolution, rate-limit cooldown, LLM call, streaming, and tool delivery only.

### Phase 3: Switch web agent to shared assembler

Replace web/server prompt assembly with:

```text
WebAgentContextAdapter::collect
  -> UnifiedLlmContextAssembler::assemble
  -> call LLM
```

The output should be byte-equivalent to the current server path except for intentional fixes.

### Phase 4: Switch CLI bridge to shared assembler

The CLI can continue preparing rich source data, but the bridge must convert it into the same input contract.

The bridge should stop using an ephemeral `PipelineSession` when a persistent session is available. It should either:

- Use the server-side session's `PipelineSession`.
- Restore a `PipelineSessionSnapshot` from session state.
- Receive and return a pipeline snapshot through the bridge protocol during rollout.

The first option is preferred for web/server. The second option is preferred for durable resume. The third option is acceptable only as a temporary migration step.

### Phase 5: Rebuild context manifest from assembler trace

Change `persist_context_manifest_for_llm_call` so it consumes `LlmContextAssemblyOutput.manifest_trace`.

Keep the existing DB schema initially. Populate the current coarse zones from the richer trace.

After that is stable, add richer per-section zone rows if needed.

### Phase 6: Remove duplicate prompt-shaping logic

Delete or narrow code paths that:

- Insert runtime system messages directly.
- Estimate context manifest tokens independently of the assembler.
- Re-prune tool schemas after pipeline output.
- Reconstruct provider cache placement outside the shared module.
- Keep a bridge-only system assembly implementation.

## Required invariants

These invariants should be enforced with tests.

| Invariant | Test shape |
| --- | --- |
| CLI and web produce identical wire payloads from identical normalized inputs | Golden request comparison |
| OpenAI-compatible volatile content does not change stable system/history prefix | Byte-prefix equality across two turns |
| Anthropic emits at most four cache markers | Request inspection |
| Bedrock Claude receives cache points after translation | Request inspection |
| MiniMax strict-history suppresses volatile churn after round 0 | Matrix provider test |
| Session UUID does not appear in prompt text | String scan |
| Skill listing is session-stable when unchanged | Cache scope assertion |
| Project context is session-stable | Cache scope assertion |
| Always-load tools precede dynamic tools | Tool schema order assertion |
| Last always-load tool gets cache marker for Anthropic | Tool schema annotation assertion |
| Invoked tools remain visible in the next tool-loop round | Tool-loop test |
| Context manifest zones are derived from actual assembly output | Manifest trace versus wire output test |
| Token usage buckets are disjoint | Usage parser test |
| Pipeline abort does not silently emit empty prompt | Failure-policy test |

## Concrete example

### CLI turn

User asks:

```text
Fix the failing test and explain the cause.
```

The CLI adapter collects:

```text
messages
semantic query
memory hits
selected tool schemas
retained invoked tools
skill allowed tools
deferred tools block
environment static
environment volatile
recent arg hints
tool results from previous round
```

It builds:

```text
LlmContextAssemblyInput
```

The shared assembler emits:

```text
stable system prefix
volatile tail
compacted history
post-compaction attachments
pruned and cache-annotated tools
context manifest trace
```

The CLI renderer and SSE consumer do not know how cache markers were placed.

### Web agent turn

Browser user asks:

```text
Run the app and verify the page.
```

The web adapter collects:

```text
messages from server session state
server-side browser/tool catalog
server workspace edge profile
project context
session facts
recent file reads
invoked skills
tool output batches
context manifest sink
```

It builds the same:

```text
LlmContextAssemblyInput
```

The shared assembler emits the same output shape. The web host streams events to the browser and writes the manifest trace to DB.

## Open questions

### Tool selection parity

CLI has a mature tool registry selector. Web agent currently has server-side tool visibility, server-side tool executor, plugin schemas, and deferred activation. The design should not force web to use CLI's exact selector implementation if the available tool catalog differs, but both paths should produce the same `ToolSurfacePlan` contract.

### Pipeline session persistence for bridge calls

The CLI bridge path historically has a more ephemeral lifecycle than server web sessions. Cache and compaction quality improve when `PipelineSession` persists across turns. The bridge should stop constructing fresh pipeline sessions once durable session state can carry a snapshot.

### Context manifest granularity

The current DB schema can store coarse zones. The assembler trace can be richer than the schema. The first migration should map rich trace to existing rows, then evaluate whether section-level rows are needed.

### Emergency behavior

Existing code has pragmatic fallback paths. The unified design should decide which ones are acceptable. My recommendation is `FailClosed` by default for prompt assembly failures, with `EmergencyPromptWithAudit` only when explicitly configured for availability.

## Recommendation

Use the `a77f397fd78a4857b653eff5c85325098e833f13` CLI prompt/cache behavior as the golden semantic contract.

Do not make the web-agent `context_manifest` a second prompt planner.

Build one shared LLM context assembler and make CLI/web differ only in source adapters.

Generate the DB context manifest from the shared assembler output.

This gives both CLI and web agent the same prompt/context cache improvements, keeps cache-sensitive logic modular, and prevents future drift between local CLI behavior and cloud web-agent behavior.
