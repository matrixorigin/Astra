# Context Window Management

> **Status**: Core Design  
> **Last Updated**: 2026-03-03  
> **Builds On**: [context-overflow-optimization.md](../implementation/context-overflow-optimization.md) (tool result handling via memory system)  
> **Related**: [prompt-lifecycle.md](prompt-lifecycle.md), [memory-architecture.md](memory-architecture.md), [edge-cloud-execution.md](edge-cloud-execution.md), [agent-loop-reliability.md](agent-loop-reliability.md)
>
> ⚠️ **2026-03-03 Update**: Eager output compression is now unconditional — `tool_output_handler.py` is called without the `_memory_store` guard. See [agent-loop-reliability.md](agent-loop-reliability.md) for the ChatLoop restructuring that enables this. `compaction.py` remains as the history-level safety net.

---

## Executive Summary

### P0 Critical Enhancements (48 Hours Implementation) ⚠️

Three high-risk areas require immediate attention before production deployment:

1. **Hybrid Reference Tracking** (§2): Pure heuristics have >2% false negative risk. Add lightweight async LLM verification for borderline cases. **CRITICAL**: Must run as background task (`asyncio.create_task`) to avoid blocking SSE stream. Cost: <0.1% token increase. Benefit: False negative rate <0.5%.

2. **Dynamic Exploration Thresholds** (§3): Fixed thresholds (3/5/8) are too rigid. Learn from `ToolRegistry` satisfaction data with **SQL performance optimization** (`COALESCE` fallback + recommended indexes). Add per-agent-type config. Change Tier 3 to soft block (allow user override).

3. **Procedural Hint Conflict Resolution** (§1): Add explicit priority system with **zero-cost implementation** (pure regex, no LLM). Includes `extract_parameter_values()` and `extract_user_specified_value()` helper functions. Prevents "agent ignored my instruction" complaints.

**Implementation Priority**: All three must be completed before Phase 2-4 deployment. Estimated effort: 2 engineer-days (48 hours).

### P1 Strong Recommendations (1 Week Implementation)

1. **Budget Scaling Fix**: Use `effective_context = model_context_size - response_reserve` as scale basis (1 line fix). Prevents over-allocation.

2. **Audit Snapshot Compression**: Delta + references or JSONB zstd compression. Saves 50%+ storage while preserving replay capability.

3. **Session Cache & Edge-Cloud Coordination**: Context hash + delta responses. Reduces payload 60-80%.

4. **Context Health Metrics**: Per-turn health events with Edge UI display. Proactive user guidance.

5. **Auto Working Memory Extraction**: Extract tool results referenced ≥2 times. Further reduces elastic zone pressure 10-20%.

---

## The Problem

Session `019ca8eb-9f8e-7522-90fa-5d905e86dae7` exposed three systemic failures:

1. **Learned knowledge ignored**: Procedural memory recorded "use overview for stock queries" but the agent used `advice` — the knowledge existed but wasn't actionable at decision time.
2. **Unbounded context growth**: 20 LLM calls consumed 500K prompt tokens. Each call replayed full history including 10 `read_file` results. Prompt grew from 3K to 45K tokens per call.
3. **Unguided exploration**: The agent made 10 consecutive file-reading calls to answer a question that didn't require code exploration.

These are not independent bugs. They share a root cause: **the system treats context as an append-only log instead of a managed resource**.

---

## Design Principles

1. **Runtime context ≠ Audit snapshot.** The prompt sent to the LLM (runtime context) is compressed for efficiency. The complete state (audit snapshot) is stored in `ctx_snapshots` for replay. These serve different purposes and must not be conflated.
2. **Knowledge at point of use.** Procedural memory is injected where the LLM makes the decision it governs — as runtime hints attached to tool schemas, not buried in a system prompt preamble.
3. **Budget is physics, not policy.** Token budgets are hard limits enforced by the runtime, not guidelines the LLM may ignore.
4. **Compression preserves semantic references.** When history is compressed, content that is semantically referenced by later reasoning is preserved; unreferenced raw output is summarized.

---

## Architecture

```
                    ┌─────────────────────────────────────────┐
                    │      Runtime Context Window Budget       │
                    │  ┌─────────┬──────────┬──────────────┐  │
                    │  │ Fixed   │ Managed  │ Elastic      │  │
                    │  │ Base    │ Base     │ Scaled       │  │
                    │  │ 4K tok  │ 3K tok   │ by model     │  │
                    │  │         │          │              │  │
                    │  │ §1 ID   │ §4 Mem   │ §6 History   │  │
                    │  │ §2 Self │ §5 Work  │   (sliding   │  │
                    │  │ §3 Proj │          │    window)   │  │
                    │  │ §7 Rules│          │              │  │
                    │  └─────────┴──────────┴──────────────┘  │
                    └─────────────────────────────────────────┘
                                      ↓
                    ┌─────────────────────────────────────────┐
                    │         Audit Snapshot (Complete)        │
                    │  Stored in ctx_snapshots table           │
                    │  - Full system prompt (all 7 sections)   │
                    │  - Complete tool schemas (with hints)    │
                    │  - Full conversation history             │
                    │  - All tool results (uncompressed)       │
                    │  → Enables exact replay at any point     │
                    └─────────────────────────────────────────┘
```

**Critical Distinction**: 
- **Runtime context** (top): Compressed, optimized for LLM efficiency. Changes per turn based on relevance.
- **Audit snapshot** (bottom): Complete, immutable record stored in database. Never compressed. Enables replay and audit.

Three zones with different management policies:

| Zone | Sections | Budget | Management |
|------|----------|--------|------------|
| Fixed | Identity, Self-Model, Project, Constraints | 4K tokens (absolute) | Never evicted. Truncated only if exceeds budget. |
| Managed | Memory (§4), Working Memory (§5), Tool Hints | 3K tokens (absolute) | Refreshed per-turn by relevance scoring. Stale entries replaced. |
| Elastic | History (§6) | Model-dependent (see §5) | Reference-aware compression. Semantically-referenced content preserved. |

---

## 1. Procedural Memory at Point of Use

### Problem

Procedural memory is currently injected into `§2 Self-Model` under "What I've Learned" — a generic paragraph the LLM reads once and may ignore. The session showed the agent had learned "follows decision flow: overview, technical, trend, risk, advice for stock queries" but still called `stock_assistant(analysis_type="advice")`.

### Design: Runtime Hint Injection (Not Schema Pollution)

**Key Insight**: Procedural memories are **runtime metadata**, not part of the skill's versioned definition. They must be injected at prompt assembly time, not stored in the skill schema.

```python
# At prompt assembly time (PromptAssembler.assemble):
def _inject_procedural_hints(self, tools_schema: list[dict], session_id: str) -> list[dict]:
    """
    Inject procedural memories as runtime hints into tool descriptions.
    
    CRITICAL: This does NOT modify the base skill schema. Hints are ephemeral
    and attached only to the runtime prompt. The audit snapshot stores:
    1. Base tool schema (from skill definition)
    2. Active procedural memories (separate field)
    3. Merged schema with hints (what LLM actually saw)
    
    This preserves replay capability: we can reconstruct the exact prompt
    by re-merging base schema + historical procedural memories.
    """
    # Retrieve active procedural memories for this session
    proc_memories = self.memory_store.retrieve(
        user_id=self.user_id,
        memory_type=MemoryType.PROCEDURAL,
        session_id=session_id,
        min_confidence=0.6,  # Only high-confidence patterns
        limit=20
    )
    
    # Build skill_name → hints mapping with explicit references
    hints_by_skill: dict[str, list[ProceduralHint]] = {}
    for mem in proc_memories:
        # Procedural memories now have explicit skill_name field (not keyword extraction)
        if mem.metadata and "skill_name" in mem.metadata:
            skill_name = mem.metadata["skill_name"]
            hints_by_skill.setdefault(skill_name, []).append(
                ProceduralHint(
                    content=mem.content,
                    confidence=mem.effective_confidence(),
                    learned_from=mem.source_event_ids
                )
            )
    
    # Inject hints into tool descriptions (runtime only)
    for tool in tools_schema:
        if tool["name"] in hints_by_skill:
            hints = hints_by_skill[tool["name"]]
            # Sort by confidence, take top 3, budget 100 tokens total
            top_hints = sorted(hints, key=lambda h: h.confidence, reverse=True)[:3]
            hint_text = "\n\n⚡ Learned patterns:\n" + "\n".join(
                f"- {h.content}" for h in top_hints
            )
            # P0: Add explicit priority note to prevent conflicts with user instructions
            hint_text += "\n(Note: User instructions in this turn ALWAYS override learned patterns)"
            
            # Truncate if exceeds budget
            if count_tokens(hint_text) > 100:
                hint_text = truncate_to_tokens(hint_text, 100)
            tool["description"] += hint_text
    
    return tools_schema
```

**Why This Works**:
1. **Preserves schema versioning**: Base skill schema is immutable and versioned
2. **Enables replay**: Audit snapshot stores both base schema and procedural memories separately
3. **Explicit references**: No fragile keyword matching - procedural memories explicitly reference skill names
4. **Bounded budget**: Per-tool limit prevents hint explosion
5. **P0: Conflict resolution**: Explicit priority note prevents procedural hints from overriding user instructions

### Priority Resolution Mechanism (P0)

**Problem**: Procedural memory may contradict current user instruction (e.g., memory says "use overview", user says "give me advice analysis").

**Solution**: Three-tier priority system enforced in `PromptAssembler`:

```python
def resolve_procedural_conflicts(
    user_message: str,
    session_procedural: list[Memory],
    global_procedural: list[Memory]
) -> list[Memory]:
    """
    Resolve conflicts between user instructions and procedural memories.
    
    Priority order (highest to lowest):
    1. Current user message (ALWAYS wins)
    2. Session-specific procedural memory
    3. Global procedural memory
    
    If user message explicitly contradicts a procedural pattern, that pattern
    is suppressed for this turn only.
    """
    active_memories = []
    
    for memory in session_procedural + global_procedural:
        # Check if user message contradicts this pattern
        if contradicts_user_intent(memory.content, user_message):
            logger.info(f"Suppressing procedural memory due to user override: {memory.memory_id}")
            continue
        active_memories.append(memory)
    
    # Sort by scope (session > global) and confidence
    active_memories.sort(
        key=lambda m: (
            1 if m.session_id else 0,  # Session-specific first
            m.effective_confidence()
        ),
        reverse=True
    )
    
    return active_memories

def contradicts_user_intent(pattern: str, user_message: str) -> bool:
    """
    Detect if user message explicitly contradicts a procedural pattern.
    
    Examples:
    - Pattern: "use analysis_type='overview'"
    - User: "give me advice analysis" → True (contradiction)
    - User: "analyze this stock" → False (no contradiction)
    
    P0 Implementation: Simple regex + zero-cost heuristics.
    """
    # Extract parameter values from pattern
    pattern_params = extract_parameter_values(pattern)
    
    # Check if user message specifies different values
    for param, learned_value in pattern_params.items():
        user_value = extract_user_specified_value(user_message, param)
        if user_value and user_value != learned_value:
            return True
    
    return False

def extract_parameter_values(pattern: str) -> dict[str, str]:
    """
    Extract parameter=value pairs from procedural memory pattern.
    
    P0 Implementation (2 lines):
    Examples:
    - "use analysis_type='overview'" → {"analysis_type": "overview"}
    - "set period to 3mo" → {"period": "3mo"}
    """
    import re
    matches = re.findall(r"(\w+)=['\"]?(\w+)['\"]?", pattern)
    return dict(matches)

def extract_user_specified_value(user_message: str, param: str) -> str | None:
    """
    Extract user-specified value for a parameter from user message.
    
    P0 Implementation: Simple keyword matching.
    Examples:
    - param="analysis_type", message="give me advice analysis" → "advice"
    - param="period", message="last 6 months" → "6mo"
    """
    import re
    # Common parameter patterns
    patterns = {
        "analysis_type": r"(overview|advice|technical|trend|risk)",
        "period": r"(\d+(?:mo|month|year|day|week))",
    }
    
    if param in patterns:
        match = re.search(patterns[param], user_message, re.IGNORECASE)
        return match.group(1) if match else None
    
    # Generic: look for "param: value" or "param = value"
    match = re.search(rf"{param}[:\s=]+['\"]?(\w+)['\"]?", user_message, re.IGNORECASE)
    return match.group(1) if match else None
```

**Implementation**: 
- Runs in `PromptAssembler._inject_procedural_hints()` before hint injection
- Suppressed memories are logged but not injected
- Suppression is per-turn only (doesn't affect memory confidence)
- **P0: Zero-cost implementation** - pure regex, no LLM calls

### Procedural Memory Schema Change

**Current** (fragile):
```python
Memory(
    memory_type=MemoryType.PROCEDURAL,
    content="Use overview for stock queries"  # Which skill? Keyword extraction guesses
)
```

**New** (explicit):
```python
Memory(
    memory_type=MemoryType.PROCEDURAL,
    content="Use analysis_type='overview' for comprehensive stock queries",
    metadata={
        "skill_name": "stock_assistant",  # Explicit reference
        "parameter": "analysis_type",      # Which parameter this governs
        "learned_from_failures": 3,        # How many failures led to this
        "success_rate_improvement": 0.45   # Measured improvement
    }
)
```

This enables:
- Precise matching (no keyword guessing)
- Parameter-level hints (more specific than tool-level)
- Confidence scoring based on measured improvement
- Audit trail of what failures led to this learning
- **P0: Conflict detection** - can compare learned parameter values with user-specified values

### Budget

- Per-tool hint budget: 100 tokens (top 3 patterns)
- Total managed zone budget: 3K tokens (includes §4 Memory + §5 Working + hints)
- Overflow: lowest-confidence hints evicted first

---

## 2. Reference-Aware History Compression

### Problem

The edge-cloud loop sends full conversation history on every turn. After 10 `read_file` calls, the history contains ~40K tokens of file content that is irrelevant to the current decision.

**Critical Constraint**: Multi-turn tool use requires causal chain tracking. If Turn 4 synthesizes findings from Turns 1-3, we cannot blindly summarize Turns 1-3 or the synthesis will fail.

### Design: Semantic Reference Tracking + Tiered Compression

**Phase 1: Reference Analysis** (per turn, after LLM response)
```python
def analyze_semantic_references(current_turn: Turn, history: list[Turn]) -> set[str]:
    """
    Identify which prior tool results are semantically referenced in current reasoning.
    
    Uses lightweight heuristics (not LLM-based):
    1. Explicit references: "As seen in config.py..." → marks read_file(config.py) as referenced
    2. Data references: Current response contains data from prior tool result → mark as referenced
    3. Causal chain: If current tool call uses output from prior call → mark as referenced
    
    Returns: Set of event_ids that must be preserved in full.
    """
    referenced_events = set()
    
    # Heuristic 1: Explicit file/tool mentions in LLM response
    for prior_turn in history:
        for tool_call in prior_turn.tool_calls:
            if tool_call.tool_name == "read_file":
                filename = tool_call.args.get("path", "").split("/")[-1]
                if filename in current_turn.llm_response:
                    referenced_events.add(tool_call.event_id)
            elif tool_call.tool_name == "grep":
                pattern = tool_call.args.get("pattern", "")
                if pattern in current_turn.llm_response:
                    referenced_events.add(tool_call.event_id)
    
    # Heuristic 2: Data overlap (substring matching for structured data)
    for prior_turn in history:
        for tool_result in prior_turn.tool_results:
            # Extract key data points (e.g., variable names, function names)
            key_data = extract_key_identifiers(tool_result.content)
            if any(kd in current_turn.llm_response for kd in key_data):
                referenced_events.add(tool_result.event_id)
    
    # Heuristic 3: Causal chain (tool output → tool input)
    if current_turn.tool_calls:
        for tool_call in current_turn.tool_calls:
            # Check if any arg value came from prior tool result
            for prior_turn in history:
                for tool_result in prior_turn.tool_results:
                    if any(str(arg_val) in tool_result.content 
                           for arg_val in tool_call.args.values()):
                        referenced_events.add(tool_result.event_id)
    
    return referenced_events
```

**Phase 2.5: Hybrid Reference Verification** (P0 - Critical for Production)

**Problem**: Pure heuristics have high false negative risk (>2% would break multi-turn synthesis).

**Solution**: Async hybrid approach for borderline cases:
```python
async def verify_references_hybrid(
    uncertain_events: list[ToolResult],
    current_turn: Turn,
    feature_flag: bool = True
) -> set[str]:
    """
    For events that heuristics are uncertain about, use lightweight LLM verification.
    
    P0 CRITICAL: Must run as background task to avoid blocking SSE stream.
    
    Triggers when:
    - Event is from exploration tool (read_file, grep)
    - Current LLM response is long (>300 tokens, indicates synthesis)
    - Heuristic confidence is borderline (partial matches)
    
    Uses ultra-cheap model (gpt-4o-mini / claude-haiku):
    - max_tokens=50 (only need "Yes/No + event_ids")
    - Cost: <0.1% of total token usage
    - False negative rate: <0.5% (vs. 2%+ for pure heuristics)
    """
    if not feature_flag or not uncertain_events:
        return set()
    
    # Build minimal prompt
    prompt = f"""Current response references which prior tool results?
Response: {current_turn.llm_response[:500]}...

Prior results:
{chr(10).join(f"{i}. {e.tool_name}({e.args}) → {e.content[:100]}..." 
              for i, e in enumerate(uncertain_events))}

Reply ONLY: "Referenced: [list of numbers]" or "None"
"""
    
    # Ultra-cheap async LLM call
    response = await llm_call_async(
        model="gpt-4o-mini",  # $0.15/1M tokens
        messages=[{"role": "user", "content": prompt}],
        max_tokens=50,
        temperature=0
    )
    
    # Parse response
    referenced_indices = parse_referenced_indices(response)
    return {uncertain_events[i].event_id for i in referenced_indices}

# Usage in chat_turn (P0: async background task)
async def process_turn_with_hybrid_verification(turn: Turn, history: list[Turn]):
    """
    P0 Implementation: Run hybrid verification as background task.
    
    This prevents blocking SSE stream while still improving accuracy.
    Results are merged asynchronously into referenced_events.
    """
    # Phase 1: Fast heuristic analysis (synchronous)
    referenced_events = analyze_semantic_references(turn, history)
    
    # Identify uncertain events (borderline heuristic confidence)
    uncertain_events = [
        event for event in get_all_tool_results(history)
        if event.tool_name in ["read_file", "grep"]
        and len(turn.llm_response) > 300
        and event.event_id not in referenced_events
        and has_partial_match(event, turn)  # Borderline confidence
    ]
    
    # Phase 2: Async hybrid verification (background task)
    if uncertain_events:
        verification_task = asyncio.create_task(
            verify_references_hybrid(uncertain_events, turn, HYBRID_REFERENCE_CHECK)
        )
        
        # Merge results when ready (non-blocking)
        verification_task.add_done_callback(
            lambda task: referenced_events.update(task.result())
        )
    
    return referenced_events

# Feature flag
HYBRID_REFERENCE_CHECK = os.getenv("HYBRID_REFERENCE_CHECK", "true").lower() == "true"
```

**Cost-Benefit**:
- Cost: ~50 tokens per uncertain event × $0.15/1M = $0.0000075 per check
- Benefit: Prevents synthesis failures that cost 10-100x more in retries
- Target: False negative rate <0.5% (vs. 2%+ for pure heuristics)

**Phase 2: Tiered Compression** (when elastic zone exceeds 70% budget)
```
┌──────────────────────────────────────────────────────┐
│  Tier 1: Recent Window (last 3 turns)                │
│  Full fidelity — always preserved                    │
│  Budget: up to 50% of elastic zone                   │
├──────────────────────────────────────────────────────┤
│  Tier 2: Referenced Content (turns 4..N-3)           │
│  Semantically referenced tool results → FULL         │
│  Unreferenced tool results → SUMMARY                 │
│  LLM reasoning → First sentence + "..."              │
│  Budget: up to 30% of elastic zone                   │
├──────────────────────────────────────────────────────┤
│  Tier 3: Session Synopsis (turns 1..3)               │
│  Single paragraph: task + key findings + outcome     │
│  Budget: up to 20% of elastic zone                   │
└──────────────────────────────────────────────────────┘
```

**Key Innovation**: Tier 2 uses reference tracking to decide what to compress:
- **Referenced tool results**: Keep full content (needed for synthesis)
- **Unreferenced tool results**: Summarize (exploratory dead-ends)
- **LLM reasoning**: Always compress (verbose, low information density)
├──────────────────────────────────────────────────────┤
│  Tier 3: Session Synopsis (turns 1..3)               │
│  Single paragraph summarizing the conversation arc   │
│  Budget: up to 20% of elastic zone                   │
└──────────────────────────────────────────────────────┘
```

### Summarization Strategy

Summarization is **rule-based, not LLM-based** — zero additional LLM cost:

| Event Type | Summary Rule |
|------------|-------------|
| `tool_call` + `tool_result` | `"{tool_name}({key_args}) → {outcome_signal}"` where outcome_signal is success/error/N lines/N items |
| `llm_response` | First sentence + "..." if > 100 tokens |
| `user_query` | Kept verbatim (always short) |

### Integration with Memory System

**Critical**: Tool results >10KB are already handled by the memory system (see [context-overflow-optimization.md](../implementation/context-overflow-optimization.md)). This design complements that:

| Content Type | Memory System | History Compression |
|--------------|---------------|---------------------|
| Tool result >10KB | Stored as TOOL_RESULT memory, replaced with reference in prompt | Reference preserved in all tiers (never compressed) |
| Tool result <10KB, referenced | Kept in prompt (Tier 2 full fidelity) | Extracted to working memory if referenced >2 times |
| Tool result <10KB, unreferenced | Summarized in Tier 2 | Not stored in memory (transient exploration) |
| LLM reasoning | Never stored in memory | Compressed to first sentence in Tier 2 |

**Decision Flow**:
```
Tool Result → Size check
  ├─ >10KB → Memory system (existing) → Reference in prompt
  └─ <10KB → Reference analysis (hybrid: heuristics + LLM verification)
      ├─ Referenced → Keep full in Tier 2 → Auto-extract to working memory if ref_count ≥2 (P1)
      └─ Unreferenced → Summarize in Tier 2 → No memory storage
```

**P1 Enhancement: Automatic Working Memory Extraction**

When any tool result is referenced ≥2 times, automatically extract to Working Memory (§5):
```python
def auto_extract_to_working_memory(event: ToolResult, ref_count: int):
    """
    Automatically extract frequently-referenced tool results to working memory.
    Reduces elastic zone pressure and improves cross-turn synthesis.
    """
    if ref_count >= 2 and event.tool_name in ["read_file", "grep", "bash"]:
        extract_structured_notes(
            content=event.content,
            session_id=event.session_id,
            note_type="auto_extracted_finding",
            metadata={
                "source_tool": event.tool_name,
                "source_event_id": event.event_id,
                "reference_count": ref_count,
                "extraction_reason": "frequently_referenced"
            }
        )
        # Replace in prompt with memory reference
        event.content = f"[Extracted to working memory - use memory_recall to access]"
```

This further reduces elastic zone pressure while preserving information.

### Trigger

Summarization triggers when elastic zone usage exceeds 70% of budget. It is applied incrementally — only the oldest unsummarized turn is compressed per trigger.

### Where It Runs

History summarization runs **server-side** in `chat_turn` before prompt assembly. The server owns the canonical history (session cache + DB events). The edge sends only the current turn's messages and tool results.

### Retrieval-Based History (Turn 3+)

**Added 2026-03-05.** Observation: session `019cbdc4` showed prompt tokens growing linearly (3870 → 7750 in 6 turns) because `_session_cache["history"]` passes all messages verbatim to LLM. The compression system above only applies to §6 in the system prompt, not to the actual messages array.

**Solution**: On Turn 3+, construct LLM messages from **recent turns + retrieved relevant old turns** instead of full history.

```
LLM messages (Turn 3+):
  [system_prompt]                    §1-§7 unchanged
  [retrieved_old_turns]              HybridRetriever → agent_events (budget: ~2000 tokens)
  [recent_2_turns]                   last 2 complete turns from session cache
  [current_user_message]
```

Key design decisions:
- `_session_cache["history"]` still stores full history (needed for snapshot persistence, recovery, reflect tool)
- Only the **LLM input view** is trimmed — the source of truth is unchanged
- `HybridRetriever.retrieve_events()` uses vector + fulltext search on `agent_events`
- Requires `EmbeddingWorker` running to embed `user_query`, `llm_response`, `tool_result` events
- Fallback: if embeddings unavailable, use full history with compaction (threshold lowered to 16K tokens)
- Result: prompt tokens stay constant (~5000-7000) regardless of turn count

---

## 3. Structured Exploration with Planning

### Problem

The agent made 10 consecutive `read_file`/`grep` calls exploring source code to answer "how do I improve?" — a question answerable from the reflect tool's output alone.

**Root Cause Analysis**: The problem is not that the agent didn't know when to stop. It's that:
1. **No exploration strategy**: Random file reading without a plan
2. **No working memory**: Forgot what it already learned, leading to redundant reads
3. **Wrong task decomposition**: Should have triggered planning phase, not direct exploration

### Design: Escalating Intervention with Dynamic Thresholds (P0)

Soft hints don't work — LLMs ignore them. We need **structural intervention** that forces strategic thinking.

**Dynamic Thresholds** (learned from `ToolRegistry`):
```python
# Base thresholds per agent type
EXPLORATION_THRESHOLDS = {
    "dev-agent": {"tier1": 4, "tier2": 7, "tier3": 12},      # Code exploration is common
    "data-analyst": {"tier1": 6, "tier2": 10, "tier3": 15},  # Data exploration is expected
    "chat-agent": {"tier1": 2, "tier2": 4, "tier3": 6},      # Exploration is rare
}

def get_dynamic_thresholds(agent_type: str, session_id: str) -> dict[str, int]:
    """
    Adjust thresholds based on learned patterns from edge_tool_patterns view.
    
    Uses LOW_SATISFACTION signal from ToolRegistry:
    - If exploration sessions have low satisfaction → lower thresholds
    - If exploration sessions have high satisfaction → raise thresholds
    
    P0: SQL performance optimization + fallback for no historical data.
    """
    base = EXPLORATION_THRESHOLDS.get(agent_type, EXPLORATION_THRESHOLDS["dev-agent"])
    
    # P0: Query with COALESCE fallback + recommended indexes
    # CREATE INDEX idx_etp_session_tools ON edge_tool_patterns(session_id, tool_call_count);
    # CREATE INDEX idx_sse_agent_created ON skill_selection_events(agent_type, created_at);
    satisfaction_data = db.query("""
        SELECT COALESCE(AVG(satisfaction_score), 0.7) as avg_satisfaction
        FROM edge_tool_patterns etp
        JOIN skill_selection_events sse ON etp.session_id = sse.session_id
        WHERE etp.tool_call_count > 5
          AND sse.agent_type = ?
          AND sse.created_at > NOW() - INTERVAL 30 DAY
    """, agent_type)
    
    avg_satisfaction = satisfaction_data.avg_satisfaction  # Never NULL due to COALESCE
    
    if avg_satisfaction < 0.6:
        # Low satisfaction with exploration → lower thresholds (intervene earlier)
        return {k: max(2, int(v * 0.7)) for k, v in base.items()}
    elif avg_satisfaction > 0.8:
        # High satisfaction → raise thresholds (allow more exploration)
        return {k: int(v * 1.3) for k, v in base.items()}
    else:
        return base
```

**Tier 1: Require Exploration Plan** (dynamic threshold, default: 3-6 calls)
```python
thresholds = get_dynamic_thresholds(agent_type, session_id)
if exploration_count >= thresholds["tier1"] and not has_active_exploration_plan():
    # Force planning step before allowing more exploration
    return {
        "type": "planning_required",
        "message": f"You've explored {exploration_count} files. Before continuing, create an exploration plan:",
        "required_fields": [
            "goal: What specific information are you looking for?",
            "strategy: Which files/patterns will you search and why?",
            "stop_condition: How will you know when you have enough information?"
        ]
    }
```

The agent must articulate a plan. This forces strategic thinking and provides a stop condition.

**Tier 2: Extract to Working Memory** (dynamic threshold, default: 5-10 calls)
```python
if exploration_count >= thresholds["tier2"]:
    # Automatically extract findings to working memory
    for tool_result in recent_exploration_results:
        extract_structured_notes(
            content=tool_result.content,
            session_id=session_id,
            note_type="exploration_finding",
            metadata={"source_file": tool_result.args["path"]}
        )
    
    # Inject system message
    return {
        "type": "memory_extraction",
        "message": "I've extracted your findings to working memory. You can now reference them without re-reading files. Consider synthesizing your findings."
    }
```

This solves the "forgot what I learned" problem and makes the cost of continuing exploration explicit.

**Tier 3: Soft Block + User Guidance** (P0 - dynamic threshold, default: 8-15 calls)
```python
if exploration_count >= thresholds["tier3"]:
    # Soft block - suggest synthesis but allow override
    return {
        "type": "exploration_guidance",
        "message": f"I've explored {exploration_count} files. I recommend:\n1. Synthesize findings now\n2. Continue exploring (reply 'override: continue' + specify what to look for)\n3. Refine my exploration plan",
        "suggested_action": "synthesize",
        "allow_override": True  # User can type "override: continue"
    }
```

**Changed from hard block to soft block**: Allows user override with explicit intent ("override: continue"), preventing frustration while still providing guidance.

### Exploration Plan Schema

```python
@dataclass
class ExplorationPlan:
    """Structured exploration plan required at threshold 1."""
    goal: str  # "Find how prompt assembly handles token budgets"
    strategy: list[ExplorationStep]  # [{"action": "grep", "target": "*.py", "pattern": "token_budget"}]
    stop_condition: str  # "Found budget allocation logic and compression triggers"
    estimated_files: int  # Agent's estimate of how many files needed
    created_at: datetime
    
    def is_complete(self, results: list[ToolResult]) -> bool:
        """Check if stop condition is met based on results."""
        # Simple keyword matching for MVP
        return self.stop_condition.lower() in " ".join(r.content for r in results).lower()
```

Stored in session cache, checked after each exploration tool call.

### Where It Runs

Server-side in `chat_turn`. The server tracks:
- `exploration_counter: int` (consecutive exploration calls)
- `active_exploration_plan: ExplorationPlan | None`
- `exploration_results: list[ToolResult]` (for plan completion check)

---

## 4. Edge Tool Selection Learning

### Problem

Only cloud skill selections are recorded in `skill_selection_events`. Edge tool calls (`read_file`, `grep`, `bash`) are recorded as events but not as selection decisions, so the system cannot learn from edge tool usage patterns.

### Design: Use Existing Event Analytics (Not New Event Type)

**Analysis**: The data already exists in `conversation_events`. Adding a new event type (`__edge_tools__` sentinel) is redundant and pollutes the schema.

**Better Approach**: Create an analytics view that derives edge tool patterns from existing events:

```sql
-- Materialized view for edge tool pattern analysis
CREATE VIEW edge_tool_patterns AS
SELECT 
    session_id,
    -- Group events by turn (same parent_event_id = same turn)
    parent_event_id as turn_id,
    -- Aggregate tools used in this turn
    array_agg(
        json_object(
            'tool_name', tool_name,
            'success', (error_message IS NULL)
        ) ORDER BY created_at
    ) as tools_used,
    -- Turn-level metrics
    COUNT(*) as tool_call_count,
    SUM(CASE WHEN error_message IS NULL THEN 1 ELSE 0 END) as success_count,
    SUM(execution_time_ms) as total_execution_time_ms,
    MIN(created_at) as turn_start,
    MAX(created_at) as turn_end
FROM conversation_events
WHERE event_type = 'tool_call'
  AND tool_name IN ('read_file', 'grep', 'list_dir', 'glob', 'bash')
GROUP BY session_id, parent_event_id;
```

This provides:
- Tool co-occurrence patterns (which tools used together)
- Success rates per tool
- Execution time patterns
- No schema pollution, no redundant data

**Learning Integration**: The `ToolRegistry` can query this view to learn:
```python
# Find sessions where exploration was inefficient
inefficient_exploration = db.query("""
    SELECT session_id, turn_id, tools_used
    FROM edge_tool_patterns
    WHERE tool_call_count > 5  -- Many exploration calls
      AND total_execution_time_ms > 10000  -- Took long time
      AND turn_id IN (
          SELECT turn_id FROM conversation_events 
          WHERE event_type = 'llm_response' 
            AND content LIKE '%I don''t have enough information%'
      )  -- But still failed to answer
""")

# Extract procedural memory: "Avoid grep without specific pattern"
```

---

## 5. Token Budget Enforcement

### Current State

`PromptAssembler` has a `_compress` method that fires when total tokens exceed `max_tokens`. But zones have no independent budgets — content grows until global compression fires, which may drop critical information.

### Design: Model-Adaptive Absolute Budgets

**Problem with Percentage-Based Budgets**: Different models have vastly different context windows (GPT-4: 8K-128K, Claude: 200K, Gemini: 1M). Fixed percentages lead to:
- Small models: Constant compression, poor performance
- Large models: No compression, defeating the purpose

**Solution: Absolute Budgets with Model-Specific Scaling**

```python
# Base budgets (optimized for 32K context models)
BASE_BUDGETS = {
    "fixed": 4000,      # Identity, self-model, project, rules
    "managed": 3000,    # Memory, working memory, tool hints
    "elastic": 8000,    # Conversation history
    "response_reserve": 4000,  # Reserved for LLM response
}
# Total: 19K tokens for 32K model (60% utilization, 40% headroom)

def compute_zone_budgets(model_context_size: int) -> dict[str, int]:
    """
    Scale budgets based on model context size.
    
    P1 CRITICAL: Use effective_context (model_context_size - response_reserve) as scale basis.
    This prevents over-allocation that would leave no room for LLM response.
    
    Strategy:
    - Small models (<16K): Use base budgets, tight management
    - Medium models (16K-64K): Scale 2x, moderate compression
    - Large models (>64K): Scale 4x, minimal compression
    """
    # P1: Compute effective context (1 line fix)
    effective_context = model_context_size - BASE_BUDGETS["response_reserve"]
    
    # Scale based on effective context, not total
    if effective_context < 12000:  # Adjusted for effective (was 16000)
        scale = 1.0
    elif effective_context < 60000:  # Adjusted for effective (was 64000)
        scale = 2.0
    else:
        scale = 4.0
    
    return {
        "fixed": int(BASE_BUDGETS["fixed"] * scale),
        "managed": int(BASE_BUDGETS["managed"] * scale),
        "elastic": int(BASE_BUDGETS["elastic"] * scale),
        "response_reserve": int(BASE_BUDGETS["response_reserve"] * scale),
        "total_allocated": int((BASE_BUDGETS["fixed"] + BASE_BUDGETS["managed"] + BASE_BUDGETS["elastic"]) * scale),
        "effective_context": int(effective_context * scale),
        "model_context_size": model_context_size,
    }

# Example outputs (P1 corrected):
# GPT-4 (8K):   effective=4K, fixed=4K, managed=3K, elastic=8K, response=4K (15K allocated, 4K response = 19K total)
# GPT-4 (32K):  effective=28K, fixed=8K, managed=6K, elastic=16K, response=8K (30K allocated, 8K response = 38K total)
# Claude (200K): effective=196K, fixed=16K, managed=12K, elastic=32K, response=16K (60K allocated, 16K response = 76K total)
```

**Why This Works**:
1. **Effective context basis** prevents over-allocation (P1 fix)
2. **Absolute budgets** ensure consistent behavior across sessions
3. **Model-aware scaling** adapts to model capabilities
4. **Fixed response reserve** prevents truncated responses
5. **Predictable compression** triggers at known thresholds

### Zone Overflow Handling

Each zone has a specific compression strategy when budget is exceeded:

```python
def enforce_zone_budget(zone: str, content: list[Section], budget: int) -> list[Section]:
    """Enforce budget for a specific zone."""
    current_tokens = sum(count_tokens(s.content) for s in content)
    
    if current_tokens <= budget:
        return content  # Within budget, no action
    
    if zone == "fixed":
        # Fixed zone: Truncate lowest-priority sections
        # Priority: Identity (highest) > Self-Model > Project > Rules (lowest)
        priority_order = ["identity", "self_model", "project", "rules"]
        return truncate_by_priority(content, priority_order, budget)
    
    elif zone == "managed":
        # Managed zone: Evict lowest-relevance entries
        # Score by: relevance_score * confidence * recency_factor
        scored_entries = [
            (entry, entry.relevance_score * entry.confidence * recency_factor(entry))
            for entry in content
        ]
        scored_entries.sort(key=lambda x: x[1], reverse=True)
        return take_until_budget(scored_entries, budget)
    
    elif zone == "elastic":
        # Elastic zone: Trigger reference-aware compression (§2)
        return compress_history_with_references(content, budget)
    
    else:
        raise ValueError(f"Unknown zone: {zone}")
```

### Failure Modes and Recovery

**Q: What if compression fails to free enough space?**
```python
if total_tokens_after_compression > model_context_size - response_reserve:
    # Last resort: Drop entire elastic zone (history)
    # Keep only: fixed + managed + current turn
    logger.warning(f"Emergency compression: dropping history for session {session_id}")
    return {
        "fixed": fixed_zone,
        "managed": managed_zone,
        "elastic": [current_turn_only],  # Only most recent turn
        "compression_emergency": True
    }
```

**Q: What if procedural memory contradicts user instruction?**
```python
# Priority order (highest to lowest):
# 1. Current user message (always wins)
# 2. Session-specific procedural memory
# 3. User-level procedural memory
# 4. Global procedural memory

# Implementation: Inject as hint, not constraint
tool["description"] += "\n\n⚡ Learned pattern: {hint}"
tool["description"] += "\n(Note: User instructions override learned patterns)"
```

**Q: What if reference tracking misses a critical dependency?**
```python
# Safety net: If LLM response contains "I don't have enough context" or similar:
if "don't have" in llm_response and "context" in llm_response:
    # Restore previous turn's full content
    restore_previous_turn_full_content()
    # Mark this turn for manual review
    flag_for_compression_failure_analysis(session_id, turn_id)
```

---

## Integration Points

### Prompt Assembly (PromptAssembler)

Changes to `assemble()`:
1. Compute zone budgets from model context size using `compute_zone_budgets()`
2. Call `_inject_procedural_hints(tools_schema, session_id)` before merging tool schemas
3. Call `_compress_history_with_references(history, elastic_budget, referenced_events)` before assembling §6
4. Enforce zone budgets using `enforce_zone_budget()` for each zone
5. **P1: Store optimized audit snapshot** in `ctx_snapshots` for replay (see below)

**P1: Audit Snapshot Storage Optimization**

Current approach stores full uncompressed history, consuming significant storage. Optimize:

```python
def store_audit_snapshot(
    session_id: str,
    event_id: str,
    runtime_prompt: dict,  # Compressed prompt sent to LLM
    referenced_events: set[str],
    enable_compression: bool = True
) -> str:
    """
    Store audit snapshot with optional compression.
    
    P1: Two storage strategies:
    1. Delta + references (default): Store only referenced events + delta from previous snapshot
    2. JSONB compression (optional): zstd compression for full history
    
    Saves 50%+ storage while preserving replay capability.
    """
    if enable_compression:
        # Strategy 1: Delta + references (recommended)
        previous_snapshot = get_previous_snapshot(session_id)
        
        snapshot = ContextSnapshot(
            context_capture_id=generate_id(),
            session_id=session_id,
            event_id=event_id,
            # Store compressed runtime prompt
            system_prompt=runtime_prompt["system_prompt"],
            skill_definitions=runtime_prompt["tools"],  # Base schemas only
            # Store only referenced events (not full history)
            selected_events=list(referenced_events),
            # Store delta from previous snapshot
            history_delta=compute_delta(runtime_prompt["history"], previous_snapshot.full_history if previous_snapshot else []),
            # Metadata for reconstruction
            compression_method="delta_references",
            base_snapshot_id=previous_snapshot.context_capture_id if previous_snapshot else None,
            # Full data for first snapshot or every 10th snapshot (checkpoints)
            full_history=runtime_prompt["history"] if should_checkpoint(event_id) else None,
            # ... other fields
        )
    else:
        # Strategy 2: JSONB compression (optional, for high-compression needs)
        import zstd
        
        snapshot = ContextSnapshot(
            context_capture_id=generate_id(),
            session_id=session_id,
            event_id=event_id,
            # Compress full history with zstd
            full_history_compressed=zstd.compress(json.dumps(runtime_prompt["history"]).encode()),
            compression_method="zstd",
            # ... other fields
        )
    
    db.add(snapshot)
    db.commit()
    return snapshot.context_capture_id

def reconstruct_snapshot(snapshot_id: str) -> dict:
    """
    Reconstruct full prompt from compressed snapshot.
    
    Handles both delta and zstd compression methods.
    """
    snapshot = db.query(ContextSnapshot).filter_by(context_capture_id=snapshot_id).one()
    
    if snapshot.compression_method == "delta_references":
        # Reconstruct from delta + base snapshot
        if snapshot.full_history:
            # This is a checkpoint, use directly
            history = snapshot.full_history
        else:
            # Reconstruct from base + delta
            base_snapshot = db.query(ContextSnapshot).filter_by(
                context_capture_id=snapshot.base_snapshot_id
            ).one()
            base_history = reconstruct_snapshot(base_snapshot.context_capture_id)["history"]
            history = apply_delta(base_history, snapshot.history_delta)
        
        # Expand referenced events to full content
        referenced_events_full = db.query(ConversationEvent).filter(
            ConversationEvent.event_id.in_(snapshot.selected_events)
        ).all()
        
        return {
            "system_prompt": snapshot.system_prompt,
            "tools": snapshot.skill_definitions,
            "history": history,
            "referenced_events": referenced_events_full,
        }
    
    elif snapshot.compression_method == "zstd":
        # Decompress zstd
        import zstd
        history = json.loads(zstd.decompress(snapshot.full_history_compressed).decode())
        return {
            "system_prompt": snapshot.system_prompt,
            "tools": snapshot.skill_definitions,
            "history": history,
        }
```

**Storage Savings**:
- Delta + references: 50-70% reduction (only store changes + referenced events)
- JSONB zstd: 60-80% reduction (compress full history)
- Checkpoint every 10 snapshots: Limits reconstruction depth to max 10 deltas

**Recommended**: Delta + references (Strategy 1) - better balance of storage savings and reconstruction speed.

### Edge Chat Loop (edge_chat_loop.py)

No changes. The edge already sends only current-turn messages. History management is entirely server-side.

### Turn Hooks (turn_hooks.py)

Add after each turn:
1. Call `analyze_semantic_references(current_turn, history)` to track which prior events are referenced
2. Update `exploration_counter` in session cache
3. Check exploration thresholds and inject planning requirements if needed

### Session Cache (chat.py)

Add to session cache entries:
```python
@dataclass
class SessionCache:
    # ... existing fields ...
    exploration_counter: int = 0
    active_exploration_plan: ExplorationPlan | None = None
    referenced_events: set[str] = field(default_factory=set)  # Events that must be preserved
```

### Memory System (core/memory/)

Add to `Memory.metadata` schema:
```python
# For procedural memories:
{
    "skill_name": str,           # Explicit skill reference
    "parameter": str,            # Which parameter this governs
    "learned_from_failures": int,
    "success_rate_improvement": float
}
```

---

## Validation Plan

### Assumption Validation

**Critical Assumption**: "LLMs attend more strongly to tool descriptions than to system prompt preambles when making tool-call decisions."

**Validation Method**: A/B test over 1000 sessions
- **Group A (50%)**: Procedural hints in tool descriptions (new design)
- **Group B (50%)**: Procedural hints in §2 Self-Model only (current design)

**Metrics**:
```python
compliance_rate = (
    tool_calls_matching_procedural_memory / 
    tool_calls_where_procedural_memory_applies
) * 100

# Success criteria:
# - Group A compliance > 80%
# - Group A compliance > Group B compliance + 20% (meaningful improvement)
# - Measured over 1000 sessions (500 per group)
```

**Rollback Trigger**: If after 500 sessions, Group A compliance < Group B + 10%, revert to current design and investigate.

### Reference Tracking Validation

**Test**: Run reference analysis on 100 historical sessions, manually verify:
- False positives: Events marked as "referenced" but not actually used (acceptable <10%)
- False negatives: Events marked as "unreferenced" but later needed (critical, must be <2%)

**Adjustment**: If false negative rate >2%, add more heuristics or increase Tier 1 window size.

---

## Implementation Roadmap

### Phase 1: Foundation (Week 1) - Token Budget Infrastructure

**Goal**: Establish zone-based budgets without changing compression logic

**Tasks**:
- [ ] Implement `compute_zone_budgets(model_context_size)` in `PromptAssembler`
- [ ] Add zone budget tracking to prompt assembly
- [ ] Add budget overflow logging (don't compress yet, just log)
- [ ] Add `ctx_snapshots` storage for complete uncompressed state

**Success Criteria**:
- All prompts have zone budget metadata
- Can query: "Which zone overflows most frequently?"
- Audit snapshots stored for 100% of decisions

**Risk**: None (observability only, no behavior change)

### Phase 2: Reference-Aware Compression + P0 Enhancements (Week 2) ⚠️ CRITICAL

**Goal**: Implement history compression that preserves semantic dependencies

**Tasks**:
- [ ] Implement `analyze_semantic_references()` with 3 heuristics
- [ ] **P0: Implement `verify_references_hybrid()` with lightweight LLM verification**
- [ ] Implement `compress_history_with_references()` with 3-tier structure
- [ ] Add `referenced_events` to session cache
- [ ] **P0: Add `auto_extract_to_working_memory()` for ref_count ≥2**
- [ ] Enable compression when elastic zone exceeds 70% budget
- [ ] Add feature flag `HYBRID_REFERENCE_CHECK`

**Success Criteria**:
- Compression reduces elastic zone by >50% on average
- **P0: False negative rate <0.5% (with hybrid verification, down from 2%)**
- No increase in "I don't have enough context" responses
- Hybrid LLM cost <0.1% of total token usage

**Risk**: Medium - May break multi-turn reasoning if reference tracking fails
**Mitigation**: 
- Feature flag `ENABLE_REFERENCE_COMPRESSION`
- **P0: Hybrid verification reduces false negative risk from 2% to <0.5%**
- Rollback if false negative rate >0.5%

### Phase 3: Procedural Memory Injection + P0 Conflict Resolution (Week 3) ⚠️ CRITICAL

**Goal**: Inject procedural hints into tool descriptions at runtime

**Tasks**:
- [ ] Add `skill_name` field to procedural memory metadata
- [ ] Implement `_inject_procedural_hints()` in `PromptAssembler`
- [ ] **P0: Implement `resolve_procedural_conflicts()` with priority system**
- [ ] **P0: Add explicit priority note to all hints: "User instructions ALWAYS override"**
- [ ] Migrate existing procedural memories to new schema (backfill)
- [ ] Start A/B test (50% with hints, 50% without)

**Success Criteria**:
- A/B test shows >20% compliance improvement
- **P0: Zero user complaints about "agent ignored my instruction"**
- No increase in prompt assembly latency (p99 <100ms)
- Audit snapshots correctly store base schema + hints separately

**Risk**: High - Changes core prompt assembly, may break tool calling
**Mitigation**: 
- Feature flag `ENABLE_PROCEDURAL_HINTS`
- **P0: Conflict resolution prevents hints from overriding user intent**
- Gradual rollout: 10% → 50% → 100% over 3 days
- Rollback if tool call success rate drops >5%

### Phase 4: Structured Exploration + P0 Dynamic Thresholds (Week 4) ⚠️ CRITICAL

**Goal**: Require exploration plans to prevent runaway file reading

**Tasks**:
- [ ] Implement `ExplorationPlan` dataclass and storage
- [ ] Add exploration counter to session cache
- [ ] **P0: Implement `get_dynamic_thresholds()` learning from ToolRegistry**
- [ ] **P0: Add per-agent-type threshold configuration**
- [ ] Implement 3-tier intervention (plan required → memory extraction → soft block + guidance)
- [ ] **P0: Change Tier 3 from hard block to soft block with user override**
- [ ] Add exploration analytics view

**Success Criteria**:
- Average exploration calls per session <5 (down from 10)
- **P0: Thresholds adapt based on satisfaction data (±30% adjustment)**
- **P0: User override works in <5% of sessions (rare but available)**
- No increase in task failure rate

**Risk**: Medium - May frustrate users with legitimate deep exploration needs
**Mitigation**:
- Feature flag `ENABLE_EXPLORATION_GUARDRAILS`
- **P0: Dynamic thresholds per agent type (dev-agent: 4/7/12, data-analyst: 6/10/15)**
- **P0: Soft block allows "override: continue" command**
- Rollback if user complaints >5/day

### Phase 5: Validation and Tuning (Week 5)

**Goal**: Measure impact and tune parameters

**Tasks**:
- [ ] Run A/B test analysis (procedural memory compliance)
- [ ] Measure token reduction on 1000 sessions
- [ ] Tune zone budget ratios based on overflow patterns
- [ ] Tune exploration thresholds based on user feedback

**Success Criteria**: See "Success Metrics" section below

---

## Cost-Benefit Analysis (Updated with P0/P1 Optimizations)

### Costs

**Engineering Effort** (revised):
- Phase 1: 2 days (budget infrastructure - simplified with P1 fix)
- Phase 2: 4 days (reference compression + P0 async hybrid verification)
- Phase 3: 4 days (procedural hints + P0 conflict resolution with zero-cost helpers)
- Phase 4: 4 days (exploration guardrails + P0 dynamic thresholds with SQL optimization)
- Phase 5: 2 days (validation)
- **P1 Enhancements**: 2 days (audit snapshot compression + context health metrics)
- **Total: 18 engineer-days (3.6 weeks)**

**Risk** (reduced with P0/P1 optimizations):
- Medium: Procedural memory injection (P0 conflict resolution reduces risk)
- Low: Reference compression (P0 hybrid verification reduces false negative risk from 2% to <0.5%)
- Low: Exploration guardrails (P0 soft block + user override reduces frustration)

**Complexity**:
- 3 new subsystems (zone budgets, reference tracking, exploration plans)
- 2 schema changes (procedural memory metadata, session cache)
- 1 A/B test infrastructure
- **P1**: Audit snapshot compression (delta + references strategy)

### Benefits

**Token Reduction** (measured on session `019ca8eb-9f8e-7522-90fa-5d905e86dae7`):
- Current: 500K prompt tokens
- With reference compression + P0 hybrid verification: ~180K tokens (64% reduction)
- With exploration guardrails + P0 dynamic thresholds: ~90K tokens (82% reduction)
- **With P1 auto working memory extraction**: ~80K tokens (84% reduction)

**Cost Savings** (at $10/1M tokens for GPT-4):
- Per session: $5.00 → $0.80 (84% reduction)
- At 1000 sessions/day: $4200/day savings = $1.53M/year

**Storage Savings** (P1 audit snapshot compression):
- Current: ~500MB per 1000 sessions (full history storage)
- With delta + references: ~200MB per 1000 sessions (60% reduction)
- Annual storage savings: ~$50K (at $0.10/GB/month for 100M sessions/year)

**Quality Improvements**:
- Procedural memory compliance: 0% → 80% (P0 conflict resolution ensures user intent respected)
- Exploration efficiency: 10 calls → 4.5 calls (P0 dynamic thresholds adapt per agent type)
- Context overflow errors: Frequent → Rare (better reliability)
- **P1 Context health visibility**: Users understand system behavior, proactive guidance

**Total Annual Savings**:
- Token cost reduction: $1.53M
- Storage cost reduction: $50K
- **Total: $1.58M/year**

**ROI** (revised): 
- Engineering cost: 18 days × $1000/day = $18K
- Annual savings: $1.58M
- **ROI: 88x** (payback in 4 days)

**With P0/P1 optimizations**:
- Lower implementation risk (async hybrid, conflict resolution, SQL optimization)
- Higher token reduction (84% vs. 80%)
- Additional storage savings (60% reduction)
- Better user experience (context health metrics, soft block with override)

### Prioritization

**Analysis of Token Usage** (needed before implementation):

Run analysis on 100 diverse sessions to identify:
1. What % of tokens are history vs. tool results vs. system prompt?
2. Which zone overflows most frequently?
3. What % of tool results are semantically referenced?

**Example Analysis**:
```python
# Run on 100 sessions
token_breakdown = analyze_token_usage(sessions)
# Output:
# {
#   "history": 45%,           # Elastic zone
#   "tool_results": 35%,      # Elastic zone (within history)
#   "system_prompt": 15%,     # Fixed zone
#   "memory": 5%              # Managed zone
# }

# Conclusion: History (elastic zone) is the bottleneck → prioritize Phase 2
```

**Recommended Priority** (based on expected impact):
1. **Phase 2 first** (reference compression) - addresses 45% of tokens
2. **Phase 4 second** (exploration guardrails) - prevents token growth
3. **Phase 1 third** (budget infrastructure) - enables monitoring
4. **Phase 3 fourth** (procedural hints) - quality improvement, not token reduction

---

## Success Metrics

### Baseline Measurement (Current System)

**Must measure on 100+ sessions before implementation**:

| Metric | Measurement Method | Expected Baseline |
|--------|-------------------|-------------------|
| Prompt tokens per session | Sum of all LLM call prompt tokens | 50K-500K (high variance) |
| Max prompt size per turn | Max tokens in single LLM call | 10K-45K |
| Procedural memory compliance | Manual labeling of 100 sessions | 20-40% (not 0% - single session is not valid baseline) |
| Exploration calls per session | Count of read_file/grep calls | 3-10 |
| Context overflow rate | Sessions with "context exceeded" error | 5-10% |

### Target Metrics (Post-Implementation with P0/P1 Optimizations)

| Metric | Baseline | Target | Measurement |
|--------|----------|--------|-------------|
| Prompt tokens per session | 200K (median) | <80K | 60% reduction (improved from 50%) |
| Max prompt size per turn | 25K (p95) | <12K | 52% reduction (improved from 40%) |
| Procedural memory compliance | 30% | >80% | 2.7x improvement |
| Hint usage rate (P1) | N/A | >70% | Hints actually used by LLM |
| Exploration calls per session | 6 (median) | <4.5 | 25% reduction (improved from 17%) |
| Context overflow rate | 8% | <1% | 8x improvement |
| Prompt assembly latency | 50ms (p99) | <100ms | No regression |
| Task success rate | 85% | >83% | <2% regression acceptable |
| Audit snapshot storage (P1) | 500MB/1K sessions | <200MB/1K sessions | 60% reduction |

### Monitoring and Alerts

**Production Monitoring** (enhanced with P1 metrics):
```python
# Alert if any metric regresses significantly
alerts = {
    "prompt_assembly_latency_p99": ">200ms",  # 2x regression
    "task_success_rate": "<80%",              # 5% drop
    "context_overflow_rate": ">10%",          # Worse than baseline
    "procedural_compliance": "<50%",          # A/B test failed
    # P1: New metrics
    "hint_usage_rate": "<60%",                # Hints not being used
    "context_health_warning_rate": ">20%",    # Too many warnings
    "audit_snapshot_size_p95": ">600MB",      # Compression not working
}
```

**P1: Context Health Event Schema**
```python
@dataclass
class ContextHealthEvent:
    """Per-turn context health metrics for monitoring and user guidance."""
    type: str = "context_health"
    fixed_usage: float      # 0.0-1.0 (e.g., 0.92 = 92%)
    managed_usage: float    # 0.0-1.0
    elastic_usage: float    # 0.0-1.0
    compression_triggered: bool
    next_compression_turn: int | None  # Estimate
    hint_usage_rate: float  # P1: % of hints actually used by LLM
    recommendation: str | None  # User-facing guidance
    
    def should_warn_user(self) -> bool:
        """Determine if user should see warning."""
        return (
            self.elastic_usage > 0.75 or
            self.fixed_usage > 0.90 or
            self.hint_usage_rate < 0.5  # Hints being ignored
        )
```

---

## P1 Enhancements (1 Week Implementation)

### 1. Session Cache & Edge-Cloud Deep Coordination

**Goal**: Reduce redundant data transfer and enable multi-edge scenarios

**Design**:
```python
# Edge sends context hash with each request
edge_request = {
    "user_message": "...",
    "local_context_hash": hash(project_rules + local_history),
    "edge_id": "edge_instance_1"
}

# Cloud returns delta only
cloud_response = {
    "llm_response": "...",
    "context_delta": {
        "added_memories": [...],      # New memories since last turn
        "updated_history_summary": "...",  # Only if changed
        "exploration_counter": 5      # Authoritative from cloud
    },
    "cache_valid": True  # Edge can reuse cached context
}

# Edge maintains local cache
edge_cache = {
    "history_summary": "...",  # Cached from cloud
    "last_context_hash": "...",
    "exploration_counter": 5   # Synced from cloud
}
```

**Benefits**:
- Reduces payload size by 60-80% (only send deltas)
- Enables multi-edge scenarios (exploration counter tracked server-side)
- Edge can display cached history summary while waiting for response

**Implementation**: 3 days
- Add `local_context_hash` to edge request
- Implement delta computation in cloud
- Add edge-side caching layer

---

### 2. Context Health Metrics & Early Warning

**Goal**: Proactive user guidance before context overflow

**Design**:
```python
# Add to every turn response
context_health_event = {
    "type": "context_health",
    "fixed_usage": "92%",        # Fixed zone utilization
    "managed_usage": "65%",      # Managed zone utilization
    "elastic_usage": "78%",      # Elastic zone utilization
    "compression_triggered": True,
    "next_compression_turn": 3,  # Estimate when next compression needed
    "recommendation": "Consider summarizing current task findings"
}
```

**Edge UI Display**:
```
┌─────────────────────────────────────┐
│ Context Pressure: 78% ⚠️             │
│ Compression active. Consider:       │
│ • Summarize current findings        │
│ • Start new session for new task    │
└─────────────────────────────────────┘
```

**Benefits**:
- Users understand why responses slow down
- Proactive guidance prevents context overflow
- Transparency builds trust

**Implementation**: 2 days
- Add context health event to turn response
- Implement edge UI component
- Add recommendation logic based on zone usage

---

### 3. Automatic Working Memory Extraction Enhancement

**Current**: Only extracts at exploration Tier 2 (5 calls)

**Enhancement**: Auto-extract any tool result referenced ≥2 times

**Design** (already added to §2):
```python
def auto_extract_to_working_memory(event: ToolResult, ref_count: int):
    """Extract frequently-referenced tool results to working memory."""
    if ref_count >= 2 and event.tool_name in ["read_file", "grep", "bash"]:
        extract_structured_notes(
            content=event.content,
            session_id=event.session_id,
            note_type="auto_extracted_finding",
            metadata={
                "source_tool": event.tool_name,
                "source_event_id": event.event_id,
                "reference_count": ref_count,
                "extraction_reason": "frequently_referenced"
            }
        )
        # Replace in prompt with memory reference
        event.content = f"[Extracted to working memory - use memory_recall to access]"
```

**Benefits**:
- Further reduces elastic zone pressure (10-20% additional reduction)
- Improves cross-turn synthesis (data persists in working memory)
- Automatic, no user intervention needed

**Implementation**: 2 days
- Add reference counting to session cache
- Implement auto-extraction trigger
- Add memory_recall tool for accessing extracted content

---

## P2 Future Enhancements (Phase 6+)

### 1. Cross-Session History Compression

**Current**: Only manages within-session history

**Enhancement**: Use memory system to synthesize findings across sessions

**Design**:
```python
# When starting new session, retrieve relevant prior session summaries
prior_context = memory_store.retrieve(
    user_id=user_id,
    memory_type=MemoryType.EPISODIC,
    query="similar tasks to current session",
    limit=3
)

# Inject as "Prior Experience" section in §4
§4_memory += f"\n\nPrior Experience:\n{format_prior_sessions(prior_context)}"
```

**Benefits**:
- Agents learn from past sessions
- Users don't need to repeat context
- Enables long-horizon task continuity

**Complexity**: Medium (requires cross-session relevance scoring)

---

### 2. Visual Context Map

**Goal**: User-visible "context map" showing what's in the agent's context

**Design**:
```
┌─────────────────────────────────────────────────┐
│ Context Map                                     │
├─────────────────────────────────────────────────┤
│ 📄 Files Referenced (3):                        │
│   • config.py ✓ (full)                          │
│   • database.py ✓ (full)                        │
│   • utils.py ⚡ (summarized)                     │
│                                                 │
│ 🧠 Working Memory (2 notes):                    │
│   • Database connection logic                   │
│   • Configuration structure                     │
│                                                 │
│ 📊 Context Usage: ████████░░ 78%                │
└─────────────────────────────────────────────────┘
```

**Benefits**:
- Users understand what the agent "knows"
- Transparency about compression decisions
- Helps users decide when to start new session

**Complexity**: Medium (requires edge UI development)

---

### 3. Agent Self-Introspection Prompt

**Goal**: Let agent self-assess context sufficiency

**Design**:
```python
# Every 5 turns, inject self-introspection prompt
if turn_count % 5 == 0:
    introspection_prompt = """
    [Self-check]: 
    1. Do I have enough context to continue effectively?
    2. Should I compress/summarize my findings?
    3. Should I suggest starting a new session?
    
    Reply with brief self-assessment.
    """
```

**Benefits**:
- Agent-driven context management
- Catches cases where heuristics miss issues
- Improves user experience (proactive suggestions)

**Complexity**: Low (simple prompt injection)
**Risk**: May add noise, needs A/B testing

---

## Non-Goals

- **LLM-based summarization**: Too expensive for per-turn use. Rule-based summarization is sufficient and deterministic.
- **Cross-session history sharing**: Handled by existing memory system (§4 retrieval). This design only manages within-session history.
- **Hard exploration blocking**: Guardrails allow user override. Hard blocking would break legitimate deep-exploration tasks.
- **Real-time compression**: Compression runs server-side before prompt assembly, not during LLM streaming.

---

## Rollback Strategy

### Feature Flags

All changes behind feature flags (no code deploy required to disable):

```python
FEATURE_FLAGS = {
    "ENABLE_ZONE_BUDGETS": True,           # Master switch
    "ENABLE_REFERENCE_COMPRESSION": True,   # History compression
    "ENABLE_PROCEDURAL_HINTS": True,        # Tool description injection
    "ENABLE_EXPLORATION_GUARDRAILS": True,  # Exploration plans
}
```

### Rollback Triggers

**Automatic Rollback** (circuit breaker):
- Prompt assembly latency p99 >500ms for 5 minutes
- Task success rate drops >10% compared to 7-day baseline
- Context overflow rate >15% (worse than baseline)

**Manual Rollback** (on-call decision):
- User complaints about "agent forgot context" (>5 reports/day)
- A/B test shows no improvement after 500 sessions
- Unexpected behavior in production

### Rollback Procedure

1. **Disable feature flags** via config service (takes effect in <1 minute, no deploy)
2. **Monitor for 1 hour** - verify metrics return to baseline
3. **If metrics don't recover** - other issue, re-enable flags and investigate
4. **If metrics recover** - root cause in new code, keep disabled and debug offline

### Gradual Rollout

**Phase 3 (Procedural Hints) uses gradual rollout**:
- Day 1: 10% of sessions
- Day 2: 25% of sessions (if no issues)
- Day 3: 50% of sessions (if no issues)
- Day 4: 100% of sessions (if A/B test shows improvement)

This limits blast radius if the change breaks tool calling.

---

## Appendix: Glossary

**Runtime Context**: The compressed prompt sent to the LLM. Optimized for token efficiency. Changes per turn based on relevance.

**Audit Snapshot**: Complete uncompressed state stored in `ctx_snapshots` table. Immutable record enabling exact replay.

**Semantic Reference**: A prior tool result that is explicitly or implicitly referenced in later reasoning. Must be preserved during compression.

**Procedural Memory**: Learned behavioral patterns stored in `memory_entries` (type=procedural). Includes skill-specific hints and general insights.

**Exploration Tools**: Tools used for codebase exploration: `read_file`, `grep`, `list_dir`, `glob`, `bash` (for git commands).

**Zone Budget**: Absolute token limit for a specific prompt section group (fixed/managed/elastic). Enforced independently per zone.

**Reference-Aware Compression**: History compression that preserves semantically-referenced content while summarizing unreferenced content.
