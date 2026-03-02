# Reflect Call Chain Visualization — More Detailed Than SQL EXPLAIN

## Problem

When users ask "为什么 token 这么多？" or "为什么这么慢？", the current tools provide:

**Current `/explain` mode** (like SQL EXPLAIN ANALYZE):
- Per-turn aggregated stats (ms, tokens)
- Memory retrieval phase breakdown
- No causal chain visualization

**Current `reflect` tool**:
- Timeline table (flat list)
- Gap detection (numeric)
- Token summary (aggregated)

**Missing**: Multi-level detailed call chain showing:
1. **Causal relationships** (parent-child event links)
2. **Token flow breakdown** (prompt composition, completion generation)
3. **Time attribution** (where every millisecond went)
4. **Memory retrieval internals** (vector search, keyword fallback, merge)
5. **Tool execution details** (args, result size, API latency)
6. **LLM inference breakdown** (prompt assembly, model call, parsing)
7. **Cost attribution** (USD per node, cumulative)

## Solution: Multi-Level Execution Tree

Add **detailed execution tree** to `SessionReport.to_markdown()` that goes deeper than SQL EXPLAIN:

```
user_query: "list all open PRs" (0s)
  └─ llm_response (2.3s) [1.2K→0.8K tokens] $0.0023
      ├─ [prompt_assembly] (0.15s)
      │   ├─ system_prompt: 450 tokens
      │   ├─ memory_retrieval (0.12s)
      │   │   ├─ phase1_keyword: 3 hits (0.05s)
      │   │   ├─ phase2_vector: 5 hits (0.06s)
      │   │   └─ merge: 5 final (0.01s)
      │   ├─ skill_schemas: 320 tokens
      │   └─ conversation_history: 280 tokens
      ├─ [model_inference] (2.0s)
      │   ├─ model: gpt-4o-mini
      │   ├─ prompt_tokens: 1,250
      │   └─ completion_tokens: 85 (tool_call)
      ├─ [tool_execution] (5.3s)
      │   ├─ tool_call: list_prs (0.1s)
      │   │   ├─ args: {"state": "open", "limit": 50}
      │   │   └─ tool_result: list_prs (5.2s) ⚠️ SLOW
      │   │       ├─ api_latency: 5.1s (GitHub API)
      │   │       ├─ result_size: 12.5KB (50 PRs)
      │   │       └─ tokens_added: 3,200
      │   └─ tool_call: get_pr_details (0.1s)
      │       └─ tool_result: get_pr_details (3.1s)
      │           ├─ api_latency: 3.0s
      │           ├─ result_size: 8.3KB
      │           └─ tokens_added: 2,100
      └─ [final_response] (4.5s) [8.5K→1.2K tokens] $0.0095 ⚠️ HIGH TOKEN
          ├─ prompt_assembly (0.2s)
          │   ├─ previous_context: 1,250 tokens
          │   ├─ tool_results: 5,300 tokens ⚠️ LARGE CONTEXT
          │   └─ total_prompt: 8,500 tokens
          ├─ model_inference (4.2s)
          │   ├─ model: gpt-4o-mini
          │   └─ completion: 1,200 tokens
          └─ response_streaming (0.1s)

SUMMARY:
  Total time: 13.9s
    ├─ LLM inference: 6.2s (45%)
    ├─ API calls: 8.2s (59%) ⚠️ BOTTLENECK
    └─ Prompt assembly: 0.35s (3%)
  
  Total tokens: 12,235
    ├─ Prompt: 9,750 (80%)
    │   ├─ Tool results: 5,300 (54%) ⚠️ LARGEST CONTRIBUTOR
    │   ├─ System/skills: 770 (8%)
    │   └─ History: 1,530 (16%)
    └─ Completion: 2,485 (20%)
  
  Total cost: $0.0118
    ├─ Turn 1: $0.0023 (19%)
    └─ Turn 2: $0.0095 (81%)
```

### Key Features

1. **Multi-Level Tree Structure**: 
   - Top level: user_query → llm_response chain
   - Second level: prompt_assembly, model_inference, tool_execution phases
   - Third level: detailed breakdown (memory retrieval phases, tool args/results, token composition)

2. **Comprehensive Timing**:
   - Every node shows duration
   - Percentage attribution in summary
   - Bottleneck detection (>50% of parent time)

3. **Token Flow Breakdown**:
   - Prompt composition by source (system, memory, skills, history, tool_results)
   - Token count per component
   - Largest contributor highlighting

4. **Tool Execution Details**:
   - Input arguments (JSON preview)
   - API latency vs processing time
   - Result size (KB)
   - Tokens added to context

5. **Memory Retrieval Internals** (from existing EXPLAIN stats):
   - Phase 1: keyword search (hits, time)
   - Phase 2: vector search (hits, time)
   - Phase 3: merge (final count, time)

6. **Cost Attribution**:
   - USD per LLM call (model pricing × tokens)
   - Cumulative cost per turn
   - Cost breakdown by phase

7. **Issue Markers**:
   - `⚠️ SLOW`: >10s duration or >50% of parent time
   - `⚠️ HIGH TOKEN`: >5K tokens in single prompt
   - `⚠️ LARGE CONTEXT`: Tool result >2K tokens
   - `⚠️ BOTTLENECK`: Phase consuming >50% of total time
   - `⚠️ EXPENSIVE`: Single call >$0.01

8. **Summary Section**:
   - Time breakdown by category (LLM, API, assembly)
   - Token breakdown by source
   - Cost breakdown by turn
   - Root cause identification

## Implementation Plan

### Phase 1: Enhanced Data Model

Extend `SessionAnalyzer` to build a **detailed execution tree** with phase-level breakdown:

```python
@dataclass
class ExecutionNode:
    """Detailed execution node — goes deeper than event-level."""
    
    # Identity
    node_id: str  # event_id or synthetic (e.g. "prompt_assembly_e123")
    node_type: str  # "user_query", "llm_response", "prompt_assembly", "model_inference", etc.
    event_id: str | None  # Link to agent_events row
    
    # Timing
    ts: datetime
    duration_s: float
    parent_duration_pct: float | None  # Percentage of parent's time
    
    # Content
    detail: str
    metadata: dict[str, Any]  # Flexible storage for phase-specific data
    
    # Token accounting
    tokens_in: int | None  # Prompt tokens
    tokens_out: int | None  # Completion tokens
    token_breakdown: dict[str, int] | None  # {"system": 450, "memory": 320, ...}
    
    # Cost
    cost_usd: float | None
    
    # Tree structure
    children: list[ExecutionNode]
    
    # Issues
    issues: list[str]  # ["SLOW", "HIGH_TOKEN", "BOTTLENECK"]


@dataclass
class ExecutionTree:
    """Root of the execution tree."""
    root: ExecutionNode
    summary: ExecutionSummary


@dataclass
class ExecutionSummary:
    """Aggregated stats across the entire tree."""
    
    # Time breakdown
    total_duration_s: float
    time_by_category: dict[str, float]  # {"llm_inference": 6.2, "api_calls": 8.2, ...}
    bottleneck_category: str | None
    
    # Token breakdown
    total_tokens: int
    tokens_by_source: dict[str, int]  # {"tool_results": 5300, "system": 770, ...}
    largest_token_source: str | None
    
    # Cost breakdown
    total_cost_usd: float
    cost_by_turn: dict[int, float]
    
    # Root cause
    root_causes: list[str]  # ["GitHub API latency (8.2s)", "Large tool results (5.3K tokens)"]
```

### Phase 2: Data Collection

Enhance event logging to capture phase-level data:

#### 2.1 Prompt Assembly Phase

In `PromptAssembler.build_prompt()`, emit a synthetic event:

```python
# After building prompt sections
phase_start = time.time()
sections = self._build_sections(...)
phase_duration = time.time() - phase_start

# Emit prompt_assembly event (stored in event_metadata)
self._emit_phase_event(
    session_id=session_id,
    phase="prompt_assembly",
    parent_event_id=current_llm_event_id,
    duration_ms=phase_duration * 1000,
    metadata={
        "token_breakdown": {
            "system_prompt": len(sections.system_tokens),
            "memory": len(sections.memory_tokens),
            "skills": len(sections.skill_tokens),
            "history": len(sections.history_tokens),
        },
        "total_tokens": sum(...),
    }
)
```

#### 2.2 Memory Retrieval Phase

Already captured in `explain` stats — extract from `RetrievalStats`:

```python
# In _build_memory(), when explain=True
memory, stats = retriever.retrieve(..., explain=True)

# Store stats in event_metadata
self._emit_phase_event(
    phase="memory_retrieval",
    parent_event_id=prompt_assembly_event_id,
    duration_ms=stats.total_ms,
    metadata={
        "phase1_keyword": {
            "hits": stats.phase1_candidates,
            "duration_ms": stats.phase1_ms,
        },
        "phase2_vector": {
            "hits": stats.phase2_candidates,
            "duration_ms": stats.phase2_ms,
        },
        "merge": {
            "final_count": stats.final_count,
            "duration_ms": stats.merge_ms,
        },
    }
)
```

#### 2.3 Tool Execution Phase

Enhance tool_result events to include:

```python
# In tool execution loop
tool_start = time.time()
result = await tool.execute(**args)
tool_duration = time.time() - tool_start

# Emit enhanced tool_result
self._emit_tool_result(
    ...,
    metadata={
        "args": args,  # Input arguments
        "result_size_bytes": len(str(result)),
        "result_size_tokens": estimate_tokens(result),
        "api_latency_ms": extract_api_latency(result),  # If available
        "duration_ms": tool_duration * 1000,
    }
)
```

#### 2.4 Model Inference Phase

Already captured in `llm_response` event — extract timing:

```python
# In LLM call wrapper
inference_start = time.time()
response = await llm.chat(messages, ...)
inference_duration = time.time() - inference_start

# Store in event_metadata
event_metadata = {
    "inference_duration_ms": inference_duration * 1000,
    "model": model_name,
    "prompt_tokens": response.usage.prompt_tokens,
    "completion_tokens": response.usage.completion_tokens,
}
```

### Phase 3: Tree Building

In `SessionAnalyzer.analyze()`, build the execution tree:

```python
def _build_execution_tree(self, events: list[EventRow]) -> ExecutionTree:
    """Build detailed execution tree from flat event list."""
    
    # Step 1: Build event index
    event_map = {e.event_id: e for e in events}
    
    # Step 2: Build basic tree from causal_chain_id + parent_event_id
    root = self._build_basic_tree(events, event_map)
    
    # Step 3: Inject phase nodes (prompt_assembly, model_inference, etc.)
    self._inject_phase_nodes(root, event_map)
    
    # Step 4: Calculate derived metrics (duration_pct, cost, issues)
    self._calculate_metrics(root)
    
    # Step 5: Build summary
    summary = self._build_summary(root)
    
    return ExecutionTree(root=root, summary=summary)


def _inject_phase_nodes(self, node: ExecutionNode, event_map: dict) -> None:
    """Inject synthetic phase nodes under llm_response events."""
    
    if node.node_type == "llm_response":
        event = event_map[node.event_id]
        metadata = event.event_metadata or {}
        
        # Inject prompt_assembly phase
        if "prompt_assembly" in metadata:
            phase_node = ExecutionNode(
                node_id=f"prompt_assembly_{node.event_id}",
                node_type="prompt_assembly",
                event_id=None,
                ts=node.ts,
                duration_s=metadata["prompt_assembly"]["duration_ms"] / 1000,
                detail="Prompt assembly",
                metadata=metadata["prompt_assembly"],
                children=[],
                issues=[],
            )
            
            # Inject memory_retrieval sub-phase
            if "memory_retrieval" in metadata["prompt_assembly"]:
                mem_node = self._build_memory_phase_node(...)
                phase_node.children.append(mem_node)
            
            node.children.insert(0, phase_node)
        
        # Inject model_inference phase
        if "inference_duration_ms" in metadata:
            inference_node = ExecutionNode(
                node_id=f"model_inference_{node.event_id}",
                node_type="model_inference",
                event_id=None,
                ts=node.ts + timedelta(seconds=phase_node.duration_s),
                duration_s=metadata["inference_duration_ms"] / 1000,
                detail=f"Model: {metadata['model']}",
                tokens_in=metadata["prompt_tokens"],
                tokens_out=metadata["completion_tokens"],
                children=[],
                issues=[],
            )
            node.children.insert(1, inference_node)
    
    # Recurse
    for child in node.children:
        self._inject_phase_nodes(child, event_map)
```

### Phase 4: ASCII Rendering with Details

Extend `ExecutionNode.to_ascii()` to show multi-level details:

```python
def to_ascii(self, prefix: str = "", is_last: bool = True, depth: int = 0) -> list[str]:
    """Render this node and children as detailed ASCII tree."""
    lines = []
    
    # Connector
    if depth == 0:
        connector = ""
    else:
        connector = "└─ " if is_last else "├─ "
    
    # Node header
    line = f"{prefix}{connector}"
    
    # Node type and detail
    if self.node_type in ("prompt_assembly", "model_inference", "tool_execution"):
        line += f"[{self.node_type}]"
    else:
        line += f"{self.node_type}"
    
    if self.detail:
        line += f": {self.detail[:60]}"
    
    # Timing
    if self.duration_s > 0:
        line += f" ({self.duration_s:.2f}s"
        if self.parent_duration_pct:
            line += f", {self.parent_duration_pct:.0f}%"
        line += ")"
    
    # Tokens
    if self.tokens_in or self.tokens_out:
        line += f" [{self.tokens_in or 0}→{self.tokens_out or 0} tokens]"
    
    # Cost
    if self.cost_usd:
        line += f" ${self.cost_usd:.4f}"
    
    # Issues
    if self.issues:
        line += " " + " ".join(f"⚠️ {i}" for i in self.issues)
    
    lines.append(line)
    
    # Token breakdown (for prompt_assembly nodes)
    if self.token_breakdown and depth < 3:  # Limit depth to avoid clutter
        child_prefix = prefix + ("   " if is_last else "│  ")
        for source, count in sorted(self.token_breakdown.items(), key=lambda x: -x[1]):
            if count > 0:
                lines.append(f"{child_prefix}├─ {source}: {count:,} tokens")
    
    # Metadata details (for specific node types)
    if self.node_type == "memory_retrieval" and self.metadata:
        child_prefix = prefix + ("   " if is_last else "│  ")
        for phase, data in self.metadata.items():
            if isinstance(data, dict):
                hits = data.get("hits", 0)
                dur = data.get("duration_ms", 0)
                lines.append(f"{child_prefix}├─ {phase}: {hits} hits ({dur:.0f}ms)")
    
    if self.node_type == "tool_result" and self.metadata:
        child_prefix = prefix + ("   " if is_last else "│  ")
        if "api_latency_ms" in self.metadata:
            lines.append(f"{child_prefix}├─ api_latency: {self.metadata['api_latency_ms']:.0f}ms")
        if "result_size_bytes" in self.metadata:
            kb = self.metadata["result_size_bytes"] / 1024
            lines.append(f"{child_prefix}├─ result_size: {kb:.1f}KB")
        if "result_size_tokens" in self.metadata:
            lines.append(f"{child_prefix}└─ tokens_added: {self.metadata['result_size_tokens']:,}")
    
    # Children
    child_prefix = prefix + ("   " if is_last else "│  ")
    for i, child in enumerate(self.children):
        is_last_child = (i == len(self.children) - 1)
        lines.extend(child.to_ascii(child_prefix, is_last_child, depth + 1))
    
    return lines
```

### Phase 5: Summary Rendering

Add detailed summary section to `SessionReport.to_markdown()`:

```python
def _render_summary(self, summary: ExecutionSummary) -> list[str]:
    """Render detailed summary with breakdown and root cause."""
    lines = ["", "### SUMMARY", ""]
    
    # Time breakdown
    lines.append(f"**Total time**: {summary.total_duration_s:.1f}s")
    for category, duration in sorted(summary.time_by_category.items(), key=lambda x: -x[1]):
        pct = (duration / summary.total_duration_s) * 100
        marker = " ⚠️ BOTTLENECK" if category == summary.bottleneck_category else ""
        lines.append(f"  ├─ {category}: {duration:.1f}s ({pct:.0f}%){marker}")
    lines.append("")
    
    # Token breakdown
    lines.append(f"**Total tokens**: {summary.total_tokens:,}")
    prompt_total = sum(v for k, v in summary.tokens_by_source.items() if k != "completion")
    completion_total = summary.tokens_by_source.get("completion", 0)
    lines.append(f"  ├─ Prompt: {prompt_total:,} ({prompt_total/summary.total_tokens*100:.0f}%)")
    
    for source, count in sorted(summary.tokens_by_source.items(), key=lambda x: -x[1]):
        if source == "completion":
            continue
        pct = (count / prompt_total) * 100 if prompt_total > 0 else 0
        marker = " ⚠️ LARGEST CONTRIBUTOR" if source == summary.largest_token_source else ""
        lines.append(f"  │   ├─ {source}: {count:,} ({pct:.0f}%){marker}")
    
    lines.append(f"  └─ Completion: {completion_total:,} ({completion_total/summary.total_tokens*100:.0f}%)")
    lines.append("")
    
    # Cost breakdown
    lines.append(f"**Total cost**: ${summary.total_cost_usd:.4f}")
    for turn, cost in sorted(summary.cost_by_turn.items()):
        pct = (cost / summary.total_cost_usd) * 100
        lines.append(f"  ├─ Turn {turn}: ${cost:.4f} ({pct:.0f}%)")
    lines.append("")
    
    # Root causes
    if summary.root_causes:
        lines.append("**Root causes**:")
        for cause in summary.root_causes:
            lines.append(f"  • {cause}")
    
    return lines
```

## Example Output (More Detailed Than SQL EXPLAIN)

### Scenario 1: "为什么这么慢？"

User asks about slow response. Reflect returns:

```markdown
## Session Analysis: `abc123def456…`

### Execution Tree
```
user_query: "list all open PRs in matrixone/matrixone" (0s)
  └─ llm_response (13.9s) [1.2K→0.8K tokens] $0.0023
      ├─ [prompt_assembly] (0.15s, 1%)
      │   ├─ system_prompt: 450 tokens
      │   ├─ [memory_retrieval] (0.12s)
      │   │   ├─ phase1_keyword: 3 hits (52ms)
      │   │   ├─ phase2_vector: 5 hits (61ms)
      │   │   └─ merge: 5 final (7ms)
      │   ├─ skill_schemas: 320 tokens
      │   └─ conversation_history: 280 tokens
      ├─ [model_inference] (2.0s, 14%)
      │   ├─ model: gpt-4o-mini
      │   ├─ prompt_tokens: 1,250
      │   └─ completion_tokens: 85 (tool_call)
      └─ [tool_execution] (11.75s, 85%) ⚠️ BOTTLENECK
          ├─ tool_call: list_prs (0.05s)
          │   ├─ args: {"owner": "matrixorigin", "repo": "matrixone", "state": "open"}
          │   └─ tool_result: list_prs (8.5s, 72%) ⚠️ SLOW
          │       ├─ api_latency: 8.2s (GitHub API)
          │       ├─ result_size: 12.5KB
          │       └─ tokens_added: 3,200
          └─ tool_call: summarize_prs (0.05s)
              └─ tool_result: summarize_prs (3.15s, 27%)
                  ├─ processing_time: 3.1s
                  ├─ result_size: 2.8KB
                  └─ tokens_added: 720

### SUMMARY

**Total time**: 13.9s
  ├─ tool_execution: 11.75s (85%) ⚠️ BOTTLENECK
  ├─ model_inference: 2.0s (14%)
  └─ prompt_assembly: 0.15s (1%)

**Root causes**:
  • GitHub API latency: 8.2s (59% of total time)
  • Tool execution dominates: 11.75s across 2 tools
  • Network I/O is the bottleneck, not LLM inference

**Recommendations**:
  • Consider caching PR lists (TTL: 5 minutes)
  • Use GitHub webhooks for real-time updates
  • Parallelize independent tool calls (list_prs + get_repo_info)
```

### Scenario 2: "为什么 token 这么多？"

User asks about high token usage. Reflect returns:

```markdown
## Session Analysis: `def456ghi789…`

### Execution Tree
```
user_query: "explain the changes in PR #12345" (0s)
  ├─ llm_response (3.5s) [1.5K→0.5K tokens] $0.0023
  │   ├─ [prompt_assembly] (0.18s)
  │   │   ├─ system_prompt: 450 tokens
  │   │   ├─ memory_retrieval: 280 tokens (0.11s)
  │   │   ├─ skill_schemas: 320 tokens
  │   │   └─ conversation_history: 180 tokens
  │   ├─ [model_inference] (2.1s)
  │   │   └─ model: gpt-4o-mini
  │   └─ [tool_execution] (1.22s)
  │       ├─ tool_call: get_pr_details (0.05s)
  │       │   └─ tool_result: get_pr_details (2.1s)
  │       │       ├─ result_size: 8.3KB
  │       │       └─ tokens_added: 2,100
  │       └─ tool_call: get_pr_diff (0.05s)
  │           └─ tool_result: get_pr_diff (1.8s)
  │               ├─ result_size: 45.2KB ⚠️ LARGE
  │               └─ tokens_added: 11,500 ⚠️ LARGE CONTEXT
  └─ llm_response (5.2s) [15.8K→2.1K tokens] $0.0182 ⚠️ HIGH TOKEN ⚠️ EXPENSIVE
      ├─ [prompt_assembly] (0.25s)
      │   ├─ previous_context: 1,500 tokens
      │   ├─ tool_results: 13,600 tokens ⚠️ LARGEST CONTRIBUTOR
      │   │   ├─ get_pr_details: 2,100 tokens
      │   │   └─ get_pr_diff: 11,500 tokens (84% of tool results)
      │   ├─ system_prompt: 450 tokens
      │   └─ total_prompt: 15,800 tokens
      └─ [model_inference] (4.8s)
          ├─ model: gpt-4o-mini
          └─ completion: 2,100 tokens

### SUMMARY

**Total tokens**: 19,900
  ├─ Prompt: 17,300 (87%)
  │   ├─ tool_results: 13,600 (79%) ⚠️ LARGEST CONTRIBUTOR
  │   │   └─ get_pr_diff: 11,500 (85% of tool results)
  │   ├─ previous_context: 1,500 (9%)
  │   ├─ system_prompt: 900 (5%)
  │   └─ memory: 280 (2%)
  └─ Completion: 2,600 (13%)

**Total cost**: $0.0205
  ├─ Turn 1: $0.0023 (11%)
  └─ Turn 2: $0.0182 (89%) ⚠️ EXPENSIVE

**Root causes**:
  • get_pr_diff returned 45KB of diff → 11.5K tokens (58% of total)
  • Large diff inflated Turn 2 prompt to 15.8K tokens
  • Single tool result dominated token budget

**Recommendations**:
  • Truncate large diffs: show only changed files summary + first 100 lines
  • Use diff summarization skill before passing to LLM
  • Consider streaming diff analysis (process file-by-file)
  • Add token budget guard: reject tool results >5K tokens
```

### Scenario 3: Multi-Turn with Memory Retrieval Details

```markdown
## Session Analysis: `ghi789jkl012…`

### Execution Tree
```
user_query: "what did we discuss about database indexing?" (0s)
  └─ llm_response (2.8s) [2.1K→0.6K tokens] $0.0028
      ├─ [prompt_assembly] (0.35s, 13%)
      │   ├─ system_prompt: 450 tokens
      │   ├─ [memory_retrieval] (0.28s, 80% of assembly) ⚠️ SLOW
      │   │   ├─ phase1_keyword: 0 hits (120ms) ⚠️ MISS
      │   │   │   └─ query: "database indexing"
      │   │   ├─ phase2_vector: 8 hits (150ms)
      │   │   │   ├─ embedding_generation: 45ms
      │   │   │   ├─ vector_search: 98ms
      │   │   │   └─ candidates: 8 memories
      │   │   └─ merge: 5 final (10ms)
      │   │       ├─ deduplication: 3 removed
      │   │       └─ ranking: semantic + temporal
      │   ├─ memory_content: 850 tokens (5 memories)
      │   ├─ skill_schemas: 320 tokens
      │   └─ conversation_history: 180 tokens
      ├─ [model_inference] (2.3s, 82%)
      │   ├─ model: gpt-4o
      │   └─ completion: 620 tokens
      └─ [response_streaming] (0.15s, 5%)

### SUMMARY

**Total time**: 2.8s
  ├─ model_inference: 2.3s (82%)
  ├─ prompt_assembly: 0.35s (13%)
  │   └─ memory_retrieval: 0.28s (80% of assembly)
  └─ response_streaming: 0.15s (5%)

**Memory retrieval breakdown**:
  • Keyword search missed (0 hits) — query too specific
  • Vector search succeeded (8 hits in 150ms)
  • Fallback to vector-only worked as designed

**Recommendations**:
  • Keyword miss is expected for semantic queries
  • Vector search latency (150ms) is acceptable
  • Consider caching embeddings for repeated queries
```

## Comparison: Reflect vs SQL EXPLAIN

| Feature | SQL EXPLAIN ANALYZE | Current `/explain` | **New Reflect Call Chain** |
|---------|---------------------|-------------------|---------------------------|
| **Execution plan** | ✅ Query plan tree | ❌ No plan | ✅ Full execution tree |
| **Timing breakdown** | ✅ Per-node timing | ✅ Per-turn timing | ✅ Per-phase + per-node timing |
| **Cost estimation** | ✅ Row estimates | ❌ No cost | ✅ USD cost per node |
| **Bottleneck detection** | ✅ Slow nodes highlighted | ⚠️ Manual inspection | ✅ Auto-detected with % attribution |
| **Resource usage** | ✅ Buffers, I/O | ⚠️ Token aggregates | ✅ Token breakdown by source |
| **Phase breakdown** | ✅ Scan, join, sort | ❌ No phases | ✅ Assembly, inference, execution |
| **Nested details** | ⚠️ 2-3 levels | ⚠️ 1 level | ✅ 4+ levels (tree + phases + sub-phases) |
| **Root cause analysis** | ❌ Manual | ❌ Manual | ✅ Auto-generated with recommendations |
| **Visual hierarchy** | ✅ Indented tree | ❌ Flat list | ✅ ASCII tree with connectors |
| **Metadata richness** | ⚠️ DB-specific | ⚠️ Basic stats | ✅ Args, results, API latency, token sources |

### Key Advantages Over SQL EXPLAIN

1. **Multi-dimensional attribution**: Time + tokens + cost in one view
2. **Causal chain tracking**: Parent-child relationships across async operations
3. **Phase-level breakdown**: See inside "black boxes" (prompt assembly, memory retrieval)
4. **Actionable recommendations**: Not just "what happened" but "what to do"
5. **Token source tracking**: Know exactly where tokens came from (tool results vs history vs system)
6. **API latency separation**: Distinguish network I/O from processing time
7. **Cost-aware**: Show USD impact of each decision

### What SQL EXPLAIN Does Better

- **Predictive**: Shows estimated plan before execution
- **Optimizer hints**: Can influence execution strategy
- **Index usage**: Shows which indexes were used/missed
- **Join strategies**: Nested loop vs hash join vs merge join

### Why Reflect Call Chain is More Detailed

SQL EXPLAIN focuses on **query optimization** (how to execute faster).

Reflect Call Chain focuses on **diagnostic observability** (why it was slow/expensive).

For agent systems, we need:
- Token accounting (no SQL equivalent)
- Multi-turn causal chains (SQL is single-query)
- LLM inference breakdown (SQL has no "model call" phase)
- Tool result impact (SQL has no "external API" phase)
- Memory retrieval phases (SQL has no "semantic search" phase)

## Implementation Checklist

### Phase 1: Data Model (Week 1)
- [ ] Add `ExecutionNode`, `ExecutionTree`, `ExecutionSummary` dataclasses to `session_analyzer.py`
- [ ] Add `_build_execution_tree()` method to `SessionAnalyzer`
- [ ] Add `_inject_phase_nodes()` for synthetic phase nodes
- [ ] Add `_calculate_metrics()` for derived metrics (duration_pct, cost, issues)
- [ ] Add `_build_summary()` for aggregated stats
- [ ] Unit tests: tree building from flat events

### Phase 2: Data Collection (Week 2)
- [ ] Enhance `PromptAssembler.build_prompt()` to emit `prompt_assembly` phase event
- [ ] Extract token breakdown by source (system, memory, skills, history)
- [ ] Enhance `_build_memory()` to emit `memory_retrieval` phase event with `RetrievalStats`
- [ ] Enhance tool execution loop to emit detailed `tool_result` metadata (args, size, latency)
- [ ] Enhance LLM call wrapper to emit `model_inference` phase metadata
- [ ] Integration tests: verify phase events are persisted

### Phase 3: Tree Building (Week 3)
- [ ] Implement `_build_basic_tree()` from `causal_chain_id` + `parent_event_id`
- [ ] Implement `_inject_phase_nodes()` for all phase types
- [ ] Implement `_build_memory_phase_node()` with sub-phases (keyword, vector, merge)
- [ ] Implement `_build_tool_execution_phase()` with args and result details
- [ ] Implement `_calculate_metrics()`: duration_pct, cost_usd, issue detection
- [ ] Unit tests: phase injection, metric calculation

### Phase 4: Rendering (Week 4)
- [ ] Implement `ExecutionNode.to_ascii()` with multi-level details
- [ ] Implement token breakdown rendering (indented list under prompt_assembly)
- [ ] Implement memory retrieval sub-phase rendering
- [ ] Implement tool result metadata rendering (api_latency, result_size, tokens_added)
- [ ] Implement `_render_summary()` with time/token/cost breakdowns
- [ ] Unit tests: ASCII rendering, summary formatting

### Phase 5: Integration (Week 5)
- [ ] Update `SessionReport.to_markdown()` to include execution tree
- [ ] Update `ReflectService.build_evidence()` to call `SessionAnalyzer.analyze()`
- [ ] Update `ReflectTool` description to mention detailed call chain
- [ ] Add `focus='performance'` auto-trigger when user asks about speed/tokens
- [ ] Integration tests: end-to-end reflect with call chain
- [ ] Performance tests: ensure tree building scales to 100+ events

### Phase 6: Polish (Week 6)
- [ ] Add root cause auto-detection (bottleneck category, largest token source)
- [ ] Add actionable recommendations based on detected issues
- [ ] Add cost estimation using model pricing table
- [ ] Add issue markers (SLOW, HIGH_TOKEN, BOTTLENECK, EXPENSIVE, LARGE_CONTEXT)
- [ ] Add depth limiting to prevent clutter (max 4 levels)
- [ ] Documentation: update reflect tool docs with examples

### Phase 7: Advanced Features (Future)
- [ ] Parallel execution visualization (side-by-side tool calls)
- [ ] Flamegraph export for deeply nested chains
- [ ] Interactive HTML tree with expand/collapse
- [ ] Historical comparison (diff between sessions)
- [ ] Token budget prediction (estimate before execution)
- [ ] Cost optimization suggestions (model routing, caching)

## Testing Strategy

### Unit Tests

#### Tree Building
```python
def test_build_execution_tree_from_flat_events():
    """Build multi-level tree from flat event list."""
    events = [
        Event(event_id="1", type="user_query", parent=None, ts=t0),
        Event(event_id="2", type="llm_response", parent="1", ts=t0+2, 
              metadata={"inference_duration_ms": 2000, "prompt_tokens": 1200}),
        Event(event_id="3", type="tool_call", parent="2", ts=t0+2.1),
        Event(event_id="4", type="tool_result", parent="3", ts=t0+7.3,
              metadata={"api_latency_ms": 5100, "result_size_bytes": 12800}),
    ]
    tree = analyzer._build_execution_tree(events)
    
    # Verify structure
    assert tree.root.node_type == "user_query"
    assert len(tree.root.children) == 1
    llm_node = tree.root.children[0]
    assert llm_node.node_type == "llm_response"
    
    # Verify phase injection
    assert any(c.node_type == "model_inference" for c in llm_node.children)
    assert any(c.node_type == "tool_execution" for c in llm_node.children)
    
    # Verify metrics
    tool_result = llm_node.children[-1].children[0].children[0]
    assert tool_result.duration_s == 5.2
    assert "SLOW" in tool_result.issues


def test_token_breakdown_calculation():
    """Token breakdown sums to total prompt tokens."""
    metadata = {
        "prompt_assembly": {
            "token_breakdown": {
                "system_prompt": 450,
                "memory": 320,
                "skills": 280,
                "history": 200,
            }
        }
    }
    node = analyzer._build_prompt_assembly_node(metadata)
    
    assert node.token_breakdown == metadata["prompt_assembly"]["token_breakdown"]
    assert sum(node.token_breakdown.values()) == 1250


def test_cost_calculation():
    """Cost calculated from model pricing × tokens."""
    node = ExecutionNode(
        node_type="model_inference",
        tokens_in=10000,
        tokens_out=2000,
        metadata={"model": "gpt-4o-mini"},
    )
    analyzer._calculate_cost(node)
    
    # gpt-4o-mini: $0.15/1M input, $0.60/1M output
    expected = (10000 * 0.15 / 1_000_000) + (2000 * 0.60 / 1_000_000)
    assert abs(node.cost_usd - expected) < 0.0001


def test_issue_detection():
    """Issues detected based on thresholds."""
    # Slow node
    node1 = ExecutionNode(duration_s=15.0, parent_duration_pct=None)
    analyzer._detect_issues(node1)
    assert "SLOW" in node1.issues
    
    # High token
    node2 = ExecutionNode(tokens_in=8000, tokens_out=500)
    analyzer._detect_issues(node2)
    assert "HIGH_TOKEN" in node2.issues
    
    # Bottleneck (>50% of parent)
    node3 = ExecutionNode(duration_s=8.0, parent_duration_pct=75.0)
    analyzer._detect_issues(node3)
    assert "BOTTLENECK" in node3.issues
    
    # Expensive
    node4 = ExecutionNode(cost_usd=0.015)
    analyzer._detect_issues(node4)
    assert "EXPENSIVE" in node4.issues
```

#### ASCII Rendering
```python
def test_ascii_rendering_basic():
    """ASCII tree renders with correct connectors."""
    root = ExecutionNode(
        node_type="user_query",
        detail="test query",
        duration_s=10.0,
        children=[
            ExecutionNode(node_type="llm_response", duration_s=8.0, children=[]),
        ],
    )
    lines = root.to_ascii()
    
    assert lines[0] == "user_query: test query (10.00s)"
    assert lines[1].startswith("   └─ llm_response")


def test_ascii_rendering_with_token_breakdown():
    """Token breakdown rendered as indented list."""
    node = ExecutionNode(
        node_type="prompt_assembly",
        duration_s=0.15,
        token_breakdown={"system_prompt": 450, "memory": 320, "skills": 280},
        children=[],
    )
    lines = node.to_ascii()
    
    assert "[prompt_assembly]" in lines[0]
    assert "system_prompt: 450 tokens" in lines[1]
    assert "memory: 320 tokens" in lines[2]


def test_ascii_rendering_with_issues():
    """Issue markers appended to line."""
    node = ExecutionNode(
        node_type="tool_result",
        duration_s=12.0,
        issues=["SLOW", "LARGE_CONTEXT"],
        children=[],
    )
    lines = node.to_ascii()
    
    assert "⚠️ SLOW" in lines[0]
    assert "⚠️ LARGE_CONTEXT" in lines[0]
```

#### Summary Generation
```python
def test_summary_time_breakdown():
    """Summary aggregates time by category."""
    tree = ExecutionTree(
        root=...,  # Tree with multiple phases
        summary=None,
    )
    summary = analyzer._build_summary(tree.root)
    
    assert "llm_inference" in summary.time_by_category
    assert "tool_execution" in summary.time_by_category
    assert summary.bottleneck_category == "tool_execution"  # Largest


def test_summary_token_breakdown():
    """Summary aggregates tokens by source."""
    tree = ExecutionTree(root=..., summary=None)
    summary = analyzer._build_summary(tree.root)
    
    assert "tool_results" in summary.tokens_by_source
    assert "system_prompt" in summary.tokens_by_source
    assert summary.largest_token_source == "tool_results"


def test_summary_root_cause_detection():
    """Root causes auto-detected from issues."""
    tree = ExecutionTree(root=..., summary=None)
    summary = analyzer._build_summary(tree.root)
    
    assert len(summary.root_causes) > 0
    assert any("API latency" in cause for cause in summary.root_causes)
```

### Integration Tests

```python
def test_reflect_performance_focus_shows_detailed_tree(db_session):
    """reflect(focus='performance') includes detailed execution tree."""
    # Setup: Create session with slow tool call and large result
    session = create_test_session(db_session)
    create_user_query(session, "list PRs")
    create_llm_response(session, metadata={
        "prompt_assembly": {"duration_ms": 150, "token_breakdown": {...}},
        "inference_duration_ms": 2000,
    })
    create_tool_call(session, "list_prs")
    create_tool_result(session, "list_prs", metadata={
        "api_latency_ms": 8200,
        "result_size_bytes": 12800,
        "result_size_tokens": 3200,
    })
    
    # Execute
    result = reflect_service.build_evidence(
        session.session_id, "alice", focus="performance", last_n=20
    )
    
    # Verify
    md = result["session_report_markdown"]
    assert "### Execution Tree" in md
    assert "[prompt_assembly]" in md
    assert "[model_inference]" in md
    assert "[tool_execution]" in md
    assert "api_latency: 8200ms" in md
    assert "⚠️ SLOW" in md
    assert "### SUMMARY" in md
    assert "⚠️ BOTTLENECK" in md


def test_reflect_token_focus_shows_breakdown(db_session):
    """reflect with high token usage shows detailed breakdown."""
    session = create_session_with_large_tool_result(db_session, result_tokens=11500)
    
    result = reflect_service.build_evidence(
        session.session_id, "alice", focus="performance", last_n=20
    )
    
    md = result["session_report_markdown"]
    assert "⚠️ HIGH_TOKEN" in md
    assert "⚠️ LARGEST CONTRIBUTOR" in md
    assert "tool_results: 11,500" in md
    assert "Root causes" in md
    assert "large diff" in md.lower()


def test_reflect_memory_retrieval_shows_phases(db_session):
    """Memory retrieval phases shown in tree."""
    session = create_session_with_memory_query(db_session)
    
    result = reflect_service.build_evidence(
        session.session_id, "alice", focus="performance", last_n=20
    )
    
    md = result["session_report_markdown"]
    assert "[memory_retrieval]" in md
    assert "phase1_keyword" in md
    assert "phase2_vector" in md
    assert "merge" in md


def test_reflect_scales_to_large_sessions(db_session):
    """Tree building scales to 100+ events."""
    session = create_large_session(db_session, num_turns=20, tools_per_turn=5)
    
    start = time.time()
    result = reflect_service.build_evidence(
        session.session_id, "alice", focus="performance", last_n=100
    )
    duration = time.time() - start
    
    assert duration < 2.0  # Should complete in <2s
    assert "### Execution Tree" in result["session_report_markdown"]
```

### Performance Tests

```python
def test_tree_building_performance():
    """Tree building is O(n) in number of events."""
    sizes = [10, 50, 100, 500]
    times = []
    
    for n in sizes:
        events = generate_test_events(n)
        start = time.time()
        tree = analyzer._build_execution_tree(events)
        times.append(time.time() - start)
    
    # Verify linear scaling (not quadratic)
    assert times[-1] / times[0] < (sizes[-1] / sizes[0]) * 2


def test_ascii_rendering_performance():
    """ASCII rendering handles deep trees efficiently."""
    tree = generate_deep_tree(depth=10, branching_factor=3)
    
    start = time.time()
    lines = tree.root.to_ascii()
    duration = time.time() - start
    
    assert duration < 0.1  # Should render in <100ms
    assert len(lines) < 1000  # Depth limiting prevents explosion
```

## Data Schema Changes

### New Event Metadata Fields

To support detailed call chain visualization, enhance `event_metadata` JSON column:

```sql
-- For llm_response events
{
  "prompt_assembly": {
    "duration_ms": 150,
    "token_breakdown": {
      "system_prompt": 450,
      "memory": 320,
      "skills": 280,
      "history": 200,
      "tool_results": 0  -- First turn has no tool results
    }
  },
  "inference_duration_ms": 2000,
  "model": "gpt-4o-mini",
  "prompt_tokens": 1250,
  "completion_tokens": 85
}

-- For tool_result events
{
  "args": {"owner": "matrixorigin", "repo": "matrixone", "state": "open"},
  "result_size_bytes": 12800,
  "result_size_tokens": 3200,
  "api_latency_ms": 8200,  -- Time spent in external API
  "processing_time_ms": 100,  -- Time spent in tool code
  "duration_ms": 8300  -- Total
}

-- For memory_retrieval phase (synthetic event or nested in prompt_assembly)
{
  "phase1_keyword": {
    "attempted": true,
    "hits": 3,
    "duration_ms": 52,
    "query": "database indexing"
  },
  "phase2_vector": {
    "attempted": true,
    "hits": 8,
    "duration_ms": 150,
    "embedding_generation_ms": 45,
    "vector_search_ms": 98
  },
  "merge": {
    "candidates": 8,
    "final_count": 5,
    "deduplication_removed": 3,
    "duration_ms": 10
  }
}
```

### No Schema Migration Required

All new fields are stored in existing `event_metadata` JSONB column. Backward compatible:
- Old events without new fields: tree building skips phase injection
- New events with fields: full detailed tree

### Model Pricing Table

Add in-memory pricing table for cost calculation:

```python
# core/agent/model_pricing.py
MODEL_PRICING = {
    "gpt-4o": {"input": 2.50, "output": 10.00},  # USD per 1M tokens
    "gpt-4o-mini": {"input": 0.15, "output": 0.60},
    "gpt-4-turbo": {"input": 10.00, "output": 30.00},
    "claude-3-5-sonnet": {"input": 3.00, "output": 15.00},
    "claude-3-5-haiku": {"input": 0.80, "output": 4.00},
}

def calculate_cost(model: str, prompt_tokens: int, completion_tokens: int) -> float:
    """Calculate USD cost for LLM call."""
    pricing = MODEL_PRICING.get(model)
    if not pricing:
        return 0.0
    return (prompt_tokens * pricing["input"] / 1_000_000 +
            completion_tokens * pricing["output"] / 1_000_000)
```

## Future Enhancements

### Phase 8: Advanced Visualizations (Future)

1. **Parallel Execution Visualization**
   - Show concurrent tool calls side-by-side in ASCII
   - Use horizontal layout for parallel branches
   ```
   [tool_execution] (5.2s)
     ├─ [parallel_group] (5.2s)
     │   ├─ list_prs (5.2s) ⚠️ SLOW
     │   └─ get_repo_info (2.1s)  [ran in parallel]
     └─ summarize (1.5s)  [sequential]
   ```

2. **Flamegraph Export**
   - Generate flamegraph SVG for deeply nested chains
   - X-axis: time, Y-axis: call stack depth
   - Click to zoom into specific phase

3. **Interactive HTML Tree**
   - Generate HTML with expand/collapse nodes
   - Click node to see full metadata
   - Hover to see tooltip with details

4. **Historical Comparison**
   - Diff two sessions side-by-side
   - Highlight regressions (slower, more tokens, higher cost)
   - Show what changed between versions

5. **Token Budget Prediction**
   - Estimate token usage before execution
   - Based on historical data for similar queries
   - Warn if predicted to exceed budget

6. **Cost Optimization Suggestions**
   - Suggest model routing (use mini for simple queries)
   - Suggest caching (repeated tool calls)
   - Suggest prompt compression (remove redundant context)

7. **Distributed Tracing Integration**
   - Export to OpenTelemetry format
   - Integrate with Jaeger/Zipkin for cross-service tracing
   - Correlate agent events with backend API spans

8. **Real-Time Streaming Visualization**
   - Show call tree as it builds (during execution)
   - Update timing/tokens incrementally
   - Useful for long-running sessions

## References

- Existing: `core/agent/session_analyzer.py` — timeline + gap detection
- Existing: `core/agent/reflect_service.py` — evidence gathering
- Related: `causal_chain_id` in `agent_events` table — enables tree building
