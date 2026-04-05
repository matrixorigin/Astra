---
name: analyze-session
description: "Deep diagnostic analysis of a coding agent session: context quality, tool/skill/MCP selection, token efficiency, error patterns, and actionable fixes. Works with any agent that logs structured events."
user_invocable: true
arguments:
  - name: TARGET
    description: "Session ID, JSON log file path, or keyword ('this', 'last'). If omitted, analyzes the current session."
    required: false
  - name: FOCUS
    description: "Optional analysis focus: 'context', 'tools', 'tokens', 'errors', 'flow', or 'all' (default: all)"
    required: false
allowed_tools:
  - bash
  - read_file
  - grep
  - glob
---
# Analyze Session Skill

Perform a deep diagnostic analysis of a coding agent session. Identifies inefficiencies,
bad tool choices, context bloat, error cascades, and missed opportunities — then prescribes fixes.

## Task

$ARGUMENTS

---

## Phase 1: Locate and Load Session Data

Determine the data source based on TARGET:

1. **JSON file path** (e.g. `/tmp/debug-*.json`) → Read directly
2. **Session ID** → Query from database:
   ```sql
   SELECT event_type, event_data, model, prompt_tokens, completion_tokens,
          tool_name, skill_name, error_message, created_at
   FROM agent_events
   WHERE session_id = '<id>'
   ORDER BY created_at;
   ```
3. **"this" / "current"** → Use current session ID from environment
4. **"last" / "previous"** → Query most recent completed session
5. **No TARGET** → Try current session, fall back to most recent

If the data source is a JSON file, parse it and extract:
- `messages[]` array (the conversation history)
- Each message's `role`, `content`, `tool_calls`, `tool_results`
- Token counts from usage metadata if present
- Any `reasoning_content` / thinking blocks

---

## Phase 2: Context Analysis

Evaluate whether the LLM received appropriate context at each turn.

### 2.1 System Prompt Quality
- **Size**: How many tokens? Is it bloated (>4000 tokens) or too sparse (<500)?
- **Relevance**: Does it contain stale instructions, dead rules, or contradictory guidance?
- **Tool definitions**: How many tools registered? Are there redundant tools?
- **Skill injection**: Were skill prompts injected? Were they relevant to the task?

### 2.2 Conversation History Shape
For each turn, compute:
- **Context window usage**: cumulative tokens vs model limit
- **History compression**: Was old context dropped or summarized? When?
- **Message role distribution**: ratio of system:user:assistant:tool messages
- **Tool result bloat**: Are tool results excessively large? (>2000 tokens per result is a red flag)

### 2.3 Information Density
- **Repeated context**: Is the same file/snippet sent to the LLM multiple times?
- **Unused context**: Was context provided but never referenced in the response?
- **Missing context**: Did the LLM ask for information it should have already had?
- **Stale context**: File contents that were read early but modified later

Flag each issue with severity: 🔴 critical, 🟡 warning, 🟢 ok

---

## Phase 3: Tool Selection Analysis

Evaluate every tool call for appropriateness and efficiency.

### 3.1 Tool Call Inventory
Build a table:
```
| # | Tool | Args (summary) | Result Size | Duration | Verdict |
|---|------|----------------|-------------|----------|---------|
```

### 3.2 Selection Quality Checks

For each tool call, assess:

- **Right tool?** Could a better tool have been used?
  - `bash("grep ...")` when `grep` tool exists → ❌ Wrong tool
  - `read_file` on entire 5000-line file when only one function needed → ❌ Over-read
  - `bash("cat file | head -20")` when `read_file` with range → ❌ Inefficient
  - Sequential `read_file` calls that could be parallel → ❌ Missed parallelism
  - `bash("find ...")` when `glob` tool exists → ❌ Wrong tool

- **Necessary?** Was this tool call needed at all?
  - Reading a file that was already in context → ❌ Redundant
  - Running `ls` on a directory already listed in context → ❌ Redundant
  - Running build/test when no code changed since last run → ❌ Wasted

- **Effective?** Did the result advance the task?
  - Tool returned error → check if error was handled or ignored
  - Tool returned empty/useless result → was query too broad/narrow?
  - Multiple retries of the same tool → what changed between retries?

### 3.3 MCP Tool Usage
If MCP tools were used:
- Were they appropriate for the task?
- Were there MCP tools available but not used that would have helped?
- Did MCP calls timeout or fail? What was the recovery?

### 3.4 Parallelism Opportunities
Identify tool calls that could have been batched:
- Multiple independent `read_file` calls in sequence
- Multiple independent `grep`/`glob` calls in sequence
- Independent `bash` commands run one-at-a-time

---

## Phase 4: Token Efficiency Analysis

### 4.1 Per-Turn Token Budget
For each LLM turn, report:
```
| Turn | Prompt Tokens | Completion Tokens | Tool Calls | Thinking Tokens | 
|------|---------------|-------------------|------------|-----------------|
```

### 4.2 Token Distribution
- **System prompt %** of total context
- **History %** of total context
- **Tool results %** of total context
- **User message %** of total context
- **Thinking tokens %** of total completion (for reasoning models)

### 4.3 Waste Indicators
- **Prompt token growth rate**: Is it linear (good) or exponential (history explosion)?
- **Completion/prompt ratio**: Very low (<5%) suggests prompt is bloated; very high (>50%) suggests over-generation
- **Thinking waste**: Long reasoning blocks that reach obvious conclusions
- **Repeated generation**: Same content generated multiple times

### 4.4 Cost Estimate
If model pricing is known, estimate session cost:
```
Model: {model_name}
Input: {prompt_tokens} × ${input_price}/1M = ${input_cost}
Output: {completion_tokens} × ${output_price}/1M = ${output_cost}
Total: ${total_cost}
Benchmark: ${cost_per_user_query} per user query
```

---

## Phase 5: Error & Failure Analysis

### 5.1 Error Inventory
List every error encountered:
```
| # | Turn | Type | Tool | Error | Recovery | Impact |
|---|------|------|------|-------|----------|--------|
```

Error types:
- **Tool failure**: Tool returned error
- **LLM error**: API error (rate limit, context overflow, etc.)
- **Parse error**: LLM output couldn't be parsed
- **Validation error**: Output failed validation
- **Timeout**: Operation exceeded time limit
- **Permission**: Access denied to file/resource

### 5.2 Error Cascades
Identify when one error caused subsequent errors:
- Tool failure → LLM retried with wrong approach → more failures
- Context overflow → history truncated → LLM lost important context → wrong output
- Rate limit → delay → timeout on dependent operation

### 5.3 Recovery Quality
For each error:
- Was the error acknowledged by the LLM?
- Was recovery attempted?
- Was recovery successful?
- Was the approach changed or just blindly retried?

---

## Phase 6: Execution Flow Analysis

### 6.1 Task Decomposition
- Did the agent plan before acting?
- Was the plan appropriate for the task complexity?
- Did the agent follow its own plan?
- Were there unnecessary detours?

### 6.2 Turn Efficiency
- **Productive turns**: Made progress toward the goal
- **Wasted turns**: No meaningful progress (empty responses, failed tools, circular reasoning)
- **Recovery turns**: Fixing mistakes from previous turns
- **Overhead turns**: Setup, context gathering (necessary but not directly productive)

### 6.3 Decision Points
For key decisions the agent made:
- Was the right approach chosen?
- Were alternatives considered?
- Was the decision based on evidence (tool results) or assumption?
- In hindsight, what would have been better?

---

## Phase 7: Report

Generate a structured diagnostic report:

```
╔══════════════════════════════════════════════════════════════╗
║  Session Analysis: {session_id}                              ║
║  Model: {model} | Turns: {n} | Duration: {time}             ║
╠══════════════════════════════════════════════════════════════╣
║                                                              ║
║  📊 Overview                                                 ║
║  ├─ User queries: {n}                                        ║
║  ├─ LLM turns: {n}                                           ║
║  ├─ Tool calls: {n} ({parallel}% parallelized)               ║
║  ├─ Total tokens: {n} (${cost} estimated)                    ║
║  └─ Errors: {n} ({recovered}% recovered)                     ║
║                                                              ║
║  🎯 Health Score: {score}/100                                ║
║  ├─ Context quality: {score}/25                              ║
║  ├─ Tool selection: {score}/25                               ║
║  ├─ Token efficiency: {score}/25                             ║
║  └─ Error handling: {score}/25                               ║
║                                                              ║
║  🔴 Critical Issues ({n})                                    ║
║  ├─ {issue_1}                                                ║
║  └─ {issue_2}                                                ║
║                                                              ║
║  🟡 Warnings ({n})                                           ║
║  ├─ {warning_1}                                              ║
║  └─ {warning_2}                                              ║
║                                                              ║
║  💡 Recommendations                                          ║
║  1. {actionable_recommendation_1}                            ║
║  2. {actionable_recommendation_2}                            ║
║  3. {actionable_recommendation_3}                            ║
║                                                              ║
╚══════════════════════════════════════════════════════════════╝
```

### Detail Tables (print after summary)

#### Tool Efficiency
```
| Tool | Calls | Avg Result Size | Redundant | Wrong Tool | 
|------|-------|-----------------|-----------|------------|
```

#### Token Timeline
```
| Turn | Prompt | Completion | Cumulative | % of Limit | Event |
|------|--------|------------|------------|------------|-------|
```

#### Error Timeline
```
| Turn | Error | Tool | Recovered? | Impact |
|------|-------|------|------------|--------|
```

---

## Phase 8: Comparative Benchmarks (when possible)

If historical session data is available, compare:
- **This session vs average**: tokens/query, tools/query, error rate
- **This model vs other models**: cost efficiency, turn count
- **This task type vs similar tasks**: was this session typical or an outlier?

---

## Anti-Patterns to Flag

### Context Anti-Patterns
- 📛 **History explosion**: Context grows >50% per turn without compression
- 📛 **File re-reading**: Same file read >2 times without changes
- 📛 **Mega tool results**: Single tool result >3000 tokens
- 📛 **Dead system prompt**: System prompt rules that never triggered

### Tool Anti-Patterns
- 📛 **Shell-for-everything**: Using `bash` when specialized tools exist
- 📛 **Sequential reads**: >3 independent file reads not parallelized
- 📛 **Blind retry**: Same tool call repeated without parameter changes
- 📛 **Over-reading**: Reading entire files when only a section was needed

### Flow Anti-Patterns
- 📛 **Premature coding**: Writing code before understanding the problem
- 📛 **No verification**: Making changes without running tests/lint
- 📛 **Circular reasoning**: Revisiting the same approach after it failed
- 📛 **Scope creep**: Fixing unrelated issues during a focused task

### Token Anti-Patterns
- 📛 **Verbose thinking**: >1000 thinking tokens for trivial decisions
- 📛 **Regeneration**: Same content generated in multiple turns
- 📛 **Explanation inflation**: Long explanations when brief ones suffice
- 📛 **Unused generation**: Generated code/text that was never applied
