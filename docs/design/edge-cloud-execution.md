# Edge-Cloud Split Execution

> **Status**: Core Design — single source of truth for edge-cloud execution model
> **Last Updated**: 2026-02-27
> **Related**: [deployment-architecture.md §1.1](deployment-architecture.md), [skills-and-tools.md](skills-and-tools.md), [agents-and-orchestration.md](agents-and-orchestration.md)

---

## 1. The Problem

Agent tools need the user's filesystem. The server doesn't have it.

```
read_file("/home/alice/project/main.go")   → must run on Alice's machine
bash("go test ./...")                       → must run on Alice's machine
git_diff()                                  → must run on Alice's machine
memory_search("how did we fix auth?")       → must run on server (data in MatrixOne)
LLM call                                   → must run on server (API key security + enrichment)
```

No single execution location works for everything. The agentic loop must be **split**.

---

## 2. Execution Split

### What runs where

| Execution Location | What | Why |
|---|---|---|
| **Edge** (user's machine) | File ops, shell, git, grep, glob, MCP servers | Needs local filesystem |
| **Edge** | Agentic loop driver (EdgeChatLoop) | Must call local tools between LLM turns |
| **Edge** | Permission prompts (Y/N/Always/Deny) | Interactive, needs user's terminal |
| **Edge** | Terminal rendering | User's terminal |
| **Edge** | Project rules loading (.mo-agent/rules.md, CLAUDE.md) | Local files |
| **Cloud** | LLM call | API key security; context enrichment; prompt caching |
| **Cloud** | Context assembly (memory search, few-shot, skill index) | Data in MatrixOne |
| **Cloud** | Model routing, SLO escalation | Historical cost/quality data in DB |
| **Cloud** | Budget control, rate limiting | Per-user enforcement |
| **Cloud** | Audit logging (decision + context snapshot) | Source of truth in MatrixOne |
| **Cloud** | Firewall verification | Needs context snapshot for claim verification |
| **Cloud** | Event persistence | All events → MatrixOne |
| **Cloud** | Skill catalog (definitions, versions) | Source of truth; edge caches |

### What the cloud does per `/chat/turn` (not just proxying)

```
Edge sends: {session_id, messages, tool_results, project_rules?}
                                    │
Cloud receives                      ▼
  1. Auth (JWT verify)
  2. Rate limit check
  3. Persist tool_result events from edge
  4. Context assembly:
     ├── Memory search (episodic, semantic, procedural)
     ├── Few-shot retrieval (similar past interactions)
     ├── Cross-session context (continuity)
     └── Skill index (available tools for LLM)
  5. Prompt enrichment:
     ├── Inject memory into system prompt
     ├── Inject project rules (from edge, first turn)
     ├── Inject few-shot examples
     └── Token budget allocation
  6. Model routing:
     ├── Select model by task complexity
     ├── SLO escalation if needed
     └── Cost estimation
  7. Budget gate: reject if user's budget exhausted
  8. LLM call (streaming, API key server-side)
  9. Post-LLM:
     ├── Firewall verification (claims vs context snapshot)
     ├── Confidence scoring
     ├── Cost tracking
     └── Persistence:
         ├── Context snapshot + DecisionAudit (links decision to snapshot)
         ├── SkillSelectionEvent (when tool_calls present)
         ├── Observations via Observer (background thread, LLM extraction)
         └── Implicit feedback detection (heuristic, zero LLM cost)
  10. Return SSE stream: {text_deltas, tool_calls, usage}
```

---

## 3. The `/chat/turn` Protocol

### Request

```
POST /chat/turn
Authorization: Bearer <jwt>
Content-Type: application/json

{
  "session_id": "ses_abc123",          // required (except first turn, auto-created)
  "messages": [                         // current conversation (edge's view)
    {"role": "user", "content": "fix the bug in auth.go"}
  ],
  "tool_results": [                     // results from edge-executed tools (empty on first turn)
    {
      "tool_call_id": "tc_001",
      "name": "read_file",
      "result": "package auth\n\nfunc Login()..."
    }
  ],
  "project_rules": "# Rules\n...",     // optional, sent on first turn
  "agent_id": "dev-agent",             // optional
  "model": "claude-sonnet-4-20250514"  // optional override
}
```

### Response (SSE stream)

```
data: {"type": "session_info", "session_id": "ses_abc123", "run_id": "run_xyz"}

data: {"type": "text_delta", "content": "Let me read"}
data: {"type": "text_delta", "content": " the auth file..."}

data: {"type": "tool_call", "id": "tc_002", "name": "read_file", "arguments": {"path": "auth.go"}}
data: {"type": "tool_call", "id": "tc_003", "name": "grep", "arguments": {"pattern": "Login", "path": "."}}

data: {"type": "usage", "prompt_tokens": 1523, "completion_tokens": 89, "cache_read_tokens": 400}
data: {"type": "turn_complete", "has_tool_calls": true}
```

**Turn semantics**:
- `has_tool_calls: true` → edge must execute tools and call `/chat/turn` again with results
- `has_tool_calls: false` → final answer, conversation turn complete

**History integrity** (merge-then-heal): Cloud guarantees a valid OpenAI
message sequence regardless of edge or cloud failures.  On every `/chat/turn`,
cloud runs a two-phase process before calling the LLM:

1. **Merge**: Incoming `tool_results` are matched by `tool_call_id` to the
   assistant message that requested them and inserted in the correct position
   (right after the assistant's tool_call block).  This handles cloud restart
   (edge still has results), partial results, and normal operation.

2. **Heal**: Any assistant `tool_calls` that *still* lack matching tool
   messages after merging are filled with placeholders (`[not executed --
   edge disconnected]`).  These are truly abandoned (edge disconnected,
   max-turns, crash).

This covers all failure combinations:

| Scenario | Phase 1 (Merge) | Phase 2 (Heal) |
|---|---|---|
| Edge disconnects, never sends results | no-op | heals all |
| Edge sends partial results | merges available | heals rest |
| Cloud restarts, edge sends results normally | merges into correct position | no-op |
| Cloud restarts, edge sends partial results | merges available | heals rest |
| Cloud restarts, edge already gave up | no-op | heals all |
| DB has trailing tool_calls (cloud crashed mid-execution) | merges if edge retries | heals rest |
| tool_results for unknown tool_call_ids | ignored (not consumed) | n/a |

Cloud owns history integrity — edge is not required to track cloud state.

### Edge loop pseudocode

```python
async def edge_chat_loop(user_input, api_client, tool_router, permissions):
    messages = [{"role": "user", "content": user_input}]
    tool_results = []
    project_rules = load_project_rules()  # local files

    for turn in range(MAX_TURNS):
        # Call cloud
        response = await api_client.chat_turn(
            messages=messages,
            tool_results=tool_results,
            project_rules=project_rules if turn == 0 else None,
        )

        # Render text as it streams
        for event in response:
            if event.type == "text_delta":
                render(event.content)

        if not response.has_tool_calls:
            break  # final answer

        # Execute tools locally
        tool_results = []
        for tc in response.tool_calls:
            permission = permissions.check(tc.name, tc.arguments)
            if permission == "deny":
                tool_results.append({"tool_call_id": tc.id, "result": "Permission denied"})
                continue
            if permission == "ask" and not confirm(tc):
                tool_results.append({"tool_call_id": tc.id, "result": "User denied"})
                continue
            result = tool_router.execute(tc.name, tc.arguments)
            tool_results.append({"tool_call_id": tc.id, "name": tc.name, "result": result})
```

---

## 4. Skill Classification

Skills split into three categories based on where they execute:

### Edge Skills (execute on user's machine)

Tools that need local filesystem or local environment.

| Skill | Why Edge |
|---|---|
| `read_file`, `write_file`, `str_replace`, `list_dir` | User's filesystem |
| `bash` | User's shell environment |
| `grep`, `glob` | User's project files |
| `git_status`, `git_diff`, `git_log`, `git_commit` | User's git repo |
| MCP servers (filesystem, database, SaaS) | MCP servers run locally via stdio |

Edge skills are **defined by the edge** (tool schemas sent to cloud so LLM knows what's available) and **executed by the edge** (cloud returns tool_calls, edge runs them).

### Cloud Skills (execute on server)

Skills that need platform data or server-side resources.

| Skill | Why Cloud |
|---|---|
| `knowledge_search` | Queries MatrixOne knowledge tables |
| `memory_recall` | Searches episodic/semantic memory |
| `session_history` | Queries past sessions |
| `skill_marketplace` | Browses/installs skills from catalog |
| Background jobs (training, evaluation) | GPU/compute resources on server |

Cloud skills are executed server-side during `/chat/turn` processing or via `/jobs` API. The LLM can "call" them, but the cloud intercepts and executes before returning to edge.

### Hybrid Skills (cloud-orchestrated, edge-executed)

Skills where the cloud provides data/logic but edge does the actual work.

| Skill | Cloud Part | Edge Part |
|---|---|---|
| `code_review` | Fetch review rules, past review patterns from memory | Read files, apply suggestions |
| `test_runner` | Fetch test strategy from memory | Execute tests locally |
| `deploy` | Fetch deploy config, validate | Execute deploy commands locally |

In practice, hybrid skills decompose into: cloud enriches context → LLM generates tool_calls → edge executes. No special mechanism needed — the `/chat/turn` loop handles it naturally.

---

## 5. Edge State Model

### What edge holds

```
~/.mo-agent/
├── credentials.json          # JWT tokens (access + refresh)
├── config.json               # User-level config (model, api_url, preferences)
├── sessions/
│   └── <session_id>.json     # Recent session message history (local cache)
├── cache/
│   ├── skills.json           # Skill definitions cache (TTL-based)
│   └── models.json           # Available models cache
└── events/
    └── pending/              # Events queued for upload (offline resilience)

<project>/.mo-agent/
├── config.json               # Project-level config
├── rules.md                  # Project rules (injected into system prompt)
├── permissions.json          # Tool permission rules (allow/ask/deny)
└── steering/                 # Additional rule files (Kiro-compatible)
    └── *.md
```

### Sync model

```
Edge → Cloud (per /chat/turn call):
  - tool_results (tool execution results from this turn)
  - project_rules (first turn only)
  Cloud persists these as events in MatrixOne.

Cloud → Edge (in /chat/turn response):
  - LLM response (text + tool_calls)
  - usage stats
  - session_id (if auto-created)

Edge → Cloud (periodic / on-demand):
  - Pending events from offline buffer

Cloud → Edge (on-demand):
  - GET /sessions → session list
  - GET /sessions/{id}/events → session history (for resume)
  - GET /skills → skill catalog (for cache refresh)
  - GET /models → available models
```

### Offline degradation

If cloud is unreachable:
- Edge cannot make LLM calls (no API key locally)
- Edge can queue tool results and events locally
- On reconnection, edge syncs pending events to cloud
- **No offline LLM mode** — this is intentional. API key security and context enrichment require the cloud.

---

## 6. Security Model

### API key isolation

```
LLM API keys:
  ✅ Stored encrypted in MatrixOne (server-side)
  ✅ Decrypted only in API server process memory
  ❌ Never sent to edge
  ❌ Never in CLI process, env vars, or config files

User credentials:
  ✅ JWT tokens on edge (~/.mo-agent/credentials.json)
  ✅ Auto-refresh on expiry
  ✅ Scoped to user's permissions (RBAC)
```

### Tool execution safety (edge-side)

```
Permission levels:
  allow  → auto-execute (read_file, grep, glob, git_status, git_diff, git_log)
  ask    → interactive confirmation (write_file, bash, git_commit, git_push)
  deny   → blocked (rm -rf /, sudo *, mkfs, dd)

Confirmation UI:
  🔧 bash: git push origin main
  [Y]es  [N]o  [A]lways allow bash  [D]eny always  >

Session overrides:
  "Always allow" → allow for rest of session
  "Deny always" → deny for rest of session
```

### Trust boundary

```
Edge (untrusted):
  - Edge can send arbitrary tool_results to cloud
  - Cloud must NOT trust tool_results for security decisions
  - Cloud uses tool_results only as LLM context (same as user input)
  - Firewall verifies LLM claims against context snapshot, not tool_results

Cloud (trusted):
  - Holds API keys, user data, audit trail
  - Enforces auth, rate limits, budget
  - All security decisions made server-side
```

### Audit provenance: edge vs cloud events

Events persisted from edge-supplied data MUST be tagged with `source: "edge"` to distinguish from server-generated events. This prevents audit confusion — an edge-sourced tool_result is no more trustworthy than user input.

```
Event from edge tool execution:
  {
    event_type: "tool_result",
    source: "edge",              ← marks as unverified edge input
    content: "file contents...",
    metadata: {
      tool_name: "read_file",
      tool_call_id: "tc_002",
      edge_reported: true        ← redundant flag for query convenience
    }
  }

Event from cloud skill execution:
  {
    event_type: "tool_result",
    source: "cloud",             ← server-verified execution
    content: "search results...",
    metadata: {
      tool_name: "knowledge_search",
      tool_call_id: "tc_003"
    }
  }
```

**Audit implications**:
- `source: "edge"` events are treated as user-supplied input in audit queries — they could be tampered with
- `source: "cloud"` events are platform-verified — the server executed the skill and recorded the result
- Firewall verification NEVER uses `source: "edge"` events as ground truth for claim checking
- Decision audit reports MUST display source provenance so auditors can distinguish verified from unverified data

---

## 7. Relationship to Existing Components

### ChatLoop (server-side) — refactored, not replaced

Current `ChatLoop.run_step_stream()` runs the full agentic loop server-side. In the new architecture:

- **`ChatLoop.run_turn()`** (new) — executes ONE LLM turn: context assembly → LLM call → verification → return. Called by `/chat/turn` endpoint.
- **`ChatLoop.run_step_stream()`** (kept) — still works for server-side-only scenarios (background agents, system agents, API-only clients without local tools).

The refactoring is additive: `run_turn()` extracts one iteration of the existing loop.

### AgentExecutor — unchanged for cloud skills

`AgentExecutor.execute_skill()` still handles cloud-side skill execution (knowledge, memory, etc.). Edge tools bypass AgentExecutor entirely — they're executed by the edge's tool router.

### RunEngine — extended

`RunEngine` gains awareness of edge-driven runs:
- Edge-driven run: multiple `/chat/turn` calls, each is a "turn" within one run
- Server-driven run: existing `start_run()` flow, unchanged
- Both produce the same event trail in MatrixOne

### EventLogger — receives edge events

Tool results from edge are persisted as events by the cloud when received in `/chat/turn`. Same event schema, same causal chain tracking. The edge is just another event source.

---

## 8. Latency Mitigation: Batched and Parallel Tool Execution

### The Problem: Ping-Pong Latency

Every tool execution requires a full HTTP round-trip + context reload + LLM inference. If the LLM needs `ls`, then `grep`, then `cat` sequentially, the user waits 3× network latency + 3× LLM inference time.

### Strategy 1: Parallel Tool Calls

The LLM returns `tool_calls` as a list. When the LLM can predict multiple independent operations, it sends them in one batch:

```
Cloud returns:
  tool_calls: [
    {read_file: "auth.go"},
    {read_file: "auth_test.go"},
    {grep: {pattern: "Login", path: "."}}
  ]

Edge executes ALL THREE concurrently (asyncio.gather / ThreadPool)
Edge sends ALL results back in one /chat/turn
```

The edge tool router MUST execute independent tools concurrently, not sequentially. For I/O-bound tools (file reads, grep), this is nearly free. This is the single most impactful optimization — it reduces N sequential turns to 1.

### Strategy 2: LLM Batching Hints

The system prompt instructs the LLM to batch tool calls when possible:

```
When you need multiple pieces of information, request them all at once.
Instead of: read_file("a.go") → wait → read_file("b.go") → wait
Do: read_file("a.go") + read_file("b.go") in one response.
```

Good models (Claude, GPT-4o) already do this naturally. The system prompt reinforces it.

### Strategy 3: Optimistic Execution

For predictable sequences, the edge can speculatively execute before the formal `tool_call` arrives:

```
LLM streams: "Let me read auth.go and its test..."
                          │
Edge detects file mention │ (text parsing heuristic)
Edge pre-reads auth.go    │ (speculative, before tool_call)
                          ▼
LLM emits: tool_call(read_file, "auth.go")
Edge: cache hit → instant result, zero wait
```

Scope:
- **Safe for read-only tools** (read_file, grep, glob, git_status, git_diff) — no side effects if prediction is wrong
- **Never for write tools** (write_file, bash, git_commit) — must wait for explicit tool_call + permission
- Speculative results are discarded if the actual tool_call doesn't match

### Strategy 4: Streaming Context Reuse

Consecutive `/chat/turn` calls within the same session share most of their context. The server avoids full reassembly:

```
Turn 1: Full context assembly (memory search, few-shot, skill index)
         → Cache assembled context with version hash

Turn 2: Check if context inputs changed:
         ├── New events since last turn? → Incremental append
         ├── Memory changed? → Only if new tool results are semantically different
         └── Skills changed? → Rare, TTL-based
         → Reuse cached context + incremental delta

Result: Turn 2+ context assembly drops from ~100ms to ~10ms
```

### Strategy 5: Tool Result Streaming

For long-running tools (bash commands, large file reads), the edge streams partial results back to the cloud instead of waiting for completion:

```
Edge executes: bash("make test")
  ├── stdout line 1 → stream to cloud immediately
  ├── stdout line 2 → stream to cloud
  └── exit code → final result

Cloud can start LLM inference with partial results
  → LLM sees test output as it arrives
  → Reduces perceived latency for long commands
```

This requires extending `/chat/turn` to accept streaming tool results (WebSocket or chunked POST). The SSE response can begin before all tool results are complete.

### Latency Budget

| Component | Target | Notes |
|---|---|---|
| Network round-trip (edge ↔ cloud) | < 50ms | Localhost in dev; regional in prod |
| Server context assembly (first turn) | < 100ms | Full assembly |
| Server context assembly (subsequent) | < 10ms | Incremental reuse |
| LLM inference (first token) | 500ms-2s | Provider-dependent, not controllable |
| Edge tool execution (parallel batch) | < 1s | I/O-bound, concurrent |
| **Total per turn** | **< 3s** | Dominated by LLM inference |
| **Perceived latency** | **< 1s to first text** | Streaming hides LLM latency |

The key insight: LLM inference dominates. Network and context assembly are noise if kept under 150ms combined. The real win is reducing the NUMBER of turns via batched tool calls, and overlapping work via optimistic execution and streaming.

### Implementation Priority

| Strategy | Priority | Reason |
|---|---|---|
| 1. Parallel Tool Calls | P0 (day-1) | Trivial to implement, biggest impact |
| 2. LLM Batching Hints | P0 (day-1) | Just a system prompt change |
| 3. Optimistic Execution | P2 | Requires text stream parsing heuristics; diminishing returns if Strategy 1+2 work well |
| 4. Streaming Context Reuse | P1 | Straightforward caching; measurable latency reduction on multi-turn sessions |
| 5. Tool Result Streaming | P2 | Requires protocol extension (WebSocket/chunked POST); only matters for long-running commands |

---

## 9. References

- [Deployment Architecture §1.1](deployment-architecture.md) — architecture overview, endpoint mapping, auth flow
- [Implementation Plan, Workstream B](implementation-plan.md) — phased implementation plan
- [Skills and Tools](skills-and-tools.md) — skill execution model, progressive disclosure
- [Agents and Orchestration](agents-and-orchestration.md) — ChatLoop, planning, multi-agent
- [Durable Agent Runs](durable-agent-runs.md) — RunEngine, run lifecycle
