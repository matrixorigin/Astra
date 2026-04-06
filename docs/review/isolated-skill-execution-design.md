# Isolated Skill Execution via SubRunExecutor

> **Status:** Design  
> **Scope:** Wire `isolated: true` skills to run as sub-agent loops via `SubRunExecutor`  
> **Prerequisite:** SkillTool (f63470f6) + model/tool override wiring (9cba0721)

---

## 1. Problem Statement

Currently, all skills execute **inline** — the LLM calls the `skill` tool, receives the skill's
instructions as a tool result, and follows them within the same conversation context. This works
well for lightweight skills (formatting guidelines, review checklists) but has limitations:

| Issue | Impact |
|-------|--------|
| **Context pollution** | Skill instructions consume parent tokens; complex skills (2K+ tokens) crowd out history |
| **No isolation** | Skill can see/modify parent state; a buggy skill contaminates the main loop |
| **Token budget** | Parent loop's `max_tokens` applies; skill can't have its own budget |
| **No result summarization** | Skill output is raw assistant text mixed into parent conversation |
| **Tool leakage** | Even with `allowed_tools`, the deny-list hack adds fragility |

**Solution:** Skills marked `isolated: true` run in a **separate sub-agent loop** with their own
context window, tool set, model, and token budget. Only the summarized result returns to the parent.

---

## 2. Architecture Overview

```
Parent Loop (CliAgenticLoopHost / ServerAgenticLoopHost)
│
├── LLM call → tool_call: { name: "skill", arguments: { skill_name: "diagnose" } }
│
├── Step 3c: partition_and_execute_skills()
│   ├── is_skill_call() → YES
│   ├── resolver.resolve("diagnose") → ResolvedSkill { isolated: true, ... }
│   │
│   ├── [INLINE path — current]
│   │   └── Format instructions → return as tool result
│   │
│   └── [ISOLATED path — new]
│       ├── Build SubRunConfig from ResolvedSkill
│       ├── Execute via SkillSubRunExecutor (impl SubRunExecutor)
│       │   ├── Create fresh AgenticLoopHost + AgenticLoopState
│       │   ├── Skill instructions → system prompt
│       │   ├── Task context → user message
│       │   ├── allowed_tools → tool restriction
│       │   ├── model → LLM override
│       │   ├── max_tokens → turn budget
│       │   └── run_agentic_loop_with_host()
│       └── Return AgentResult.output as tool result to parent
│
└── Parent loop continues with summarized result
```

---

## 3. Data Model Changes

### 3.1 Add `isolated` to SkillInstruction

```rust
// skill_instructions.rs — SkillInstruction
pub struct SkillInstruction {
    pub name: String,
    pub description: String,
    pub user_invocable: bool,
    pub triggers: Vec<String>,
    pub allowed_tools: Vec<String>,
    pub when_to_use: Option<String>,
    pub model: Option<String>,
    pub max_tokens: u32,              // 0 = system default
    pub isolated: bool,               // NEW — default false
    pub instructions: String,
    pub instruction_tokens: u32,
}
```

### 3.2 Add `isolated` to SkillMetadata

```rust
// skill_instructions.rs — SkillMetadata
pub struct SkillMetadata {
    pub name: String,
    pub description: String,
    pub triggers: Vec<String>,
    pub user_invocable: bool,
    pub when_to_use: Option<String>,
    pub model: Option<String>,
    pub max_tokens: u32,
    pub isolated: bool,               // NEW — default false
    pub metadata_tokens: u32,
}
```

### 3.3 Add `isolated` to ResolvedSkill

```rust
// skill_tool.rs — ResolvedSkill
pub struct ResolvedSkill {
    pub name: String,
    pub instructions: String,
    pub model: Option<String>,
    pub max_tokens: Option<u32>,
    pub allowed_tools: Vec<String>,
    pub isolated: bool,               // NEW — default false
}
```

### 3.4 SKILL.md Frontmatter Example

```yaml
---
name: deep-review
description: "Thorough multi-file code review with security analysis"
user_invocable: true
isolated: true                        # ← NEW
model: "claude-sonnet-4-20250514"
max_tokens: 16384
allowed_tools:
  - bash
  - read_file
  - grep
  - glob
when_to_use: "Use for thorough code reviews requiring isolated analysis"
---

You are a code review specialist. Analyze the provided code for:
1. Security vulnerabilities (OWASP Top 10)
2. Logic errors and race conditions
3. Performance bottlenecks
...
```

---

## 4. Execution Flow — Detailed

### 4.1 Skill Resolution (unchanged)

`CliSkillResolver::resolve()` already returns all needed fields. We just add `isolated`
to the mapping in `resolve()`:

```rust
// skill_instructions.rs — CliSkillResolver::resolve()
Ok(ResolvedSkill {
    name: name.to_string(),
    instructions: instruction.instructions.clone(),
    model: instruction.model.clone(),
    max_tokens: if instruction.max_tokens > 0 { Some(instruction.max_tokens) } else { None },
    allowed_tools: instruction.allowed_tools.clone(),
    isolated: instruction.isolated,     // NEW
})
```

### 4.2 Branching in execute_skill()

The current `execute_skill()` is synchronous and returns `(String, Option<SkillActivation>)`.
For isolated execution, we need async + access to a sub-run executor. Two design options:

**Option A: Branch in `partition_and_execute_skills()` (Recommended)**

```rust
pub async fn partition_and_execute_skills(
    tool_calls: &[Value],
    resolver: &dyn SkillResolver,
    sub_run_executor: Option<&dyn SkillSubRunExecutor>,  // NEW param
) -> (Vec<(String, String)>, Vec<Value>, Option<SkillActivation>) {
    // ...
    for tc in tool_calls {
        if !is_skill_call(tc) { remaining.push(tc.clone()); continue; }

        let skill = resolver.resolve(&skill_name)?;

        if skill.isolated && sub_run_executor.is_some() {
            // ISOLATED PATH — run as sub-agent
            let result = sub_run_executor.unwrap()
                .execute_skill_subrun(&skill, task_hint)
                .await;
            skill_results.push((call_id, result));
            // No SkillActivation — sub-run is self-contained
        } else {
            // INLINE PATH — current behavior
            let (text, act) = execute_skill_inline(resolver, &skill, task_hint);
            if let Some(a) = act { activation = Some(a); }
            skill_results.push((call_id, text));
        }
    }
}
```

**Option B: Branch in the agentic loop Step 3c**

More invasive; requires the loop to know about isolation. **Not recommended** — Step 3c should
remain a thin interception layer.

**Decision: Option A** — keeps the branching inside `partition_and_execute_skills()` where all
skill logic is concentrated.

### 4.3 SkillSubRunExecutor Trait

```rust
// skill_tool.rs — new trait

/// Executor for isolated skill sub-runs.
/// Implemented by CLI (CliSkillSubRunExecutor) and Server (ServerSkillSubRunExecutor).
#[async_trait]
pub trait SkillSubRunExecutor: Send + Sync {
    /// Run a skill in an isolated sub-agent loop.
    /// Returns the final text output from the sub-run.
    async fn execute_skill_subrun(
        &self,
        skill: &ResolvedSkill,
        task_context: &str,
    ) -> String;
}
```

### 4.4 CLI Implementation — CliSkillSubRunExecutor

This is the most complex part. The CLI host uses references (`&'a`) extensively, so we
can't directly clone it. We need a builder pattern or owned-value wrapper.

```rust
// New file: rust/crates/astra-cli/src/cli/chat_stream/sse_loop/skill_subrun.rs

pub struct CliSkillSubRunExecutor {
    api: Arc<ThinClient>,            // Shared API client (upgrade from &'a to Arc)
    token: String,                   // Owned copy
    project_root: PathBuf,
    executor: ToolExecutor,          // Can be cloned per sub-run
    registry: ToolRegistry,          // Tool schemas
    all_schemas: Vec<Value>,
    skill_registry: SharedSkillRegistry,
    default_model: Option<String>,
}

#[async_trait]
impl SkillSubRunExecutor for CliSkillSubRunExecutor {
    async fn execute_skill_subrun(
        &self,
        skill: &ResolvedSkill,
        task_context: &str,
    ) -> String {
        // 1. Build system prompt from skill instructions
        let system_prompt = format!(
            "You are executing the '{}' skill.\n\n{}\n\n\
             Complete the task and provide a clear, concise summary of results.",
            skill.name, skill.instructions,
        );

        // 2. Determine model
        let model = skill.model.as_deref()
            .or(self.default_model.as_deref());

        // 3. Build tool restriction set
        let restricted_tools: HashSet<String> = if skill.allowed_tools.is_empty() {
            HashSet::new()
        } else {
            let allowed: HashSet<&str> = skill.allowed_tools.iter()
                .map(String::as_str).collect();
            self.all_tool_names.iter()
                .filter(|t| !allowed.contains(t.as_str()))
                .cloned()
                .collect()
        };

        // 4. Build task message
        let user_msg = if task_context.is_empty() {
            "Execute the skill as described in the system prompt.".to_string()
        } else {
            task_context.to_string()
        };

        // 5. Build AgenticLoopState
        let messages = vec![
            serde_json::json!({ "role": "system", "content": system_prompt }),
            serde_json::json!({ "role": "user", "content": user_msg }),
        ];

        let mut state = AgenticLoopState {
            messages,
            restricted_tools,
            // ... remaining fields initialized with defaults
            skill_resolver: None,        // Sub-runs don't chain skills
            delegation_engine: None,     // Sub-runs don't delegate
            skill_model_override: None,
            skill_allowed_tools: None,
            // Token limits from skill
            // max_turns: calculated from max_tokens
        };

        // 6. Build host and run loop
        // (see Section 5 for lifetime management)
        let mut host = /* ... build CliAgenticLoopHost ... */;

        match run_agentic_loop_with_host(&mut host, &mut state).await {
            Ok(AgenticLoopOutcome::Completed) => {
                // Extract final assistant text
                state.messages.iter().rev()
                    .find(|m| m["role"] == "assistant")
                    .and_then(|m| m["content"].as_str())
                    .unwrap_or("[Skill completed with no output]")
                    .to_string()
            }
            Ok(AgenticLoopOutcome::Cancelled) =>
                "[Skill execution was cancelled]".to_string(),
            Ok(AgenticLoopOutcome::Waiting(reason)) =>
                format!("[Skill is waiting: {}]", reason),
            Ok(AgenticLoopOutcome::Error(e)) | Err(e) =>
                format!("[Skill execution failed: {}]", e),
        }
    }
}
```

### 4.5 Server Implementation — ServerSkillSubRunExecutor

The server side is simpler because `ServerSubRunExecutor` already handles sub-runs.
We create a thin wrapper that converts `ResolvedSkill` → `SubRunConfig`:

```rust
// server/delegation_engine.rs or a new file

pub struct ServerSkillSubRunExecutor {
    executor: Arc<dyn SubRunExecutor>,
}

#[async_trait]
impl SkillSubRunExecutor for ServerSkillSubRunExecutor {
    async fn execute_skill_subrun(
        &self,
        skill: &ResolvedSkill,
        task_context: &str,
    ) -> String {
        let profile = AgentProfile {
            agent_id: format!("skill-{}", skill.name),
            name: skill.name.clone(),
            tier: AgentTier::User,
            system_prompt: Some(skill.instructions.clone()),
            skill_filter: skill.allowed_tools.clone(),
            model_override: skill.model.clone(),
            can_delegate: false,
            delegate_to: vec![],
            max_delegation_depth: 0,
            triggers: vec![],
            metadata: HashMap::new(),
        };

        let config = SubRunConfig {
            run_id: format!("skill-{}-{}", skill.name, uuid::Uuid::new_v4()),
            agent_profile: profile,
            task: task_context.to_string(),
            session_id: "skill-subrun".to_string(),
            user_id: "system".to_string(),
            previous_output: None,
            context: HashMap::new(),
            pause_flag: None,
            checkpoint_gate: None,
        };

        match self.executor.execute(config).await {
            Ok(result) => result.output.unwrap_or_default(),
            Err(e) => format!("[Skill sub-run failed: {}]", e),
        }
    }
}
```

---

## 5. Lifetime Management (CLI-specific)

The biggest challenge on the CLI side is `CliAgenticLoopHost`'s use of references:

```rust
pub(crate) struct CliAgenticLoopHost<'a> {
    pub api: &'a ThinClient,        // reference
    pub token: &'a str,             // reference
    pub model: Option<&'a str>,     // reference
    pub selector: &'a dyn ToolSelector,  // reference
    // ...
}
```

### Options

**Option A: Create `OwnedCliLoopHost` (Recommended)**

A new host type that owns all its data. This avoids the reference lifetime problem entirely.

```rust
pub struct OwnedCliLoopHost {
    api: Arc<ThinClient>,
    token: String,
    model: Option<String>,
    project_root: PathBuf,
    executor: ToolExecutor,
    selector: Box<dyn ToolSelector>,
    registry: ToolRegistry,
    all_schemas: Vec<Value>,
    valid_tool_names: HashSet<String>,
    quiet: bool,  // Sub-runs are quiet (no terminal rendering)
}

#[async_trait]
impl AgenticLoopHost for OwnedCliLoopHost {
    async fn execute_turn(&mut self, state: &mut AgenticLoopState) -> Result<HostTurnResult, String> {
        // Similar to CliAgenticLoopHost::execute_turn but with owned values
        // Suppress terminal rendering (sub-run is headless from parent's perspective)
    }
    // ...
}
```

**Option B: Pass references through scoped async**

Use `tokio::task::spawn_local` or structured concurrency to keep parent references alive.
More complex, fragile, and limits parallelism.

**Option C: Reuse `ServerAgenticLoopHost`**

The CLI already has a `ThinClient` that talks to the LLM API. Could create a
`ServerAgenticLoopHost` with the CLI's credentials. But this mixes abstractions.

**Decision: Option A** — cleanest separation, most future-proof.

---

## 6. Wiring into AgenticLoopState

### 6.1 New field on AgenticLoopState

```rust
pub struct AgenticLoopState {
    // ... existing fields ...

    /// Optional executor for running isolated skill sub-runs.
    /// When set, skills with `isolated: true` are executed via this executor
    /// instead of being inlined as tool results.
    pub skill_subrun_executor: Option<Arc<dyn SkillSubRunExecutor>>,
}
```

### 6.2 Construction sites

| Location | Value |
|----------|-------|
| `sse_loop/mod.rs` (CLI) | `Some(Arc::new(CliSkillSubRunExecutor { ... }))` |
| `server_loop_host.rs` | `Some(Arc::new(ServerSkillSubRunExecutor { ... }))` or `None` |
| `run_lifecycle.rs` (2 places) | `None` (sub-runs don't spawn sub-sub-runs) |
| `agentic_loop_host.rs` (test) | `None` |
| `loop_dispatcher.rs` | `None` |

### 6.3 Passing to partition_and_execute_skills

In `agentic_loop_host.rs` Step 3c:

```rust
let (sr, remaining, activation) =
    crate::turn::skill_tool::partition_and_execute_skills(
        effective_tool_calls,
        resolver.as_ref(),
        state.skill_subrun_executor.as_deref(),  // NEW
    )
    .await;
```

---

## 7. Terminal Output for Sub-runs

When a CLI sub-run executes, we need to show progress without mixing into the parent's
streaming output. Strategy:

### 7.1 Headless sub-runs

Sub-runs use `OwnedCliLoopHost` with `quiet: true` and `suppress_intermediate_output: true`.
The parent loop shows a single status line:

```
⚙ Running isolated skill "deep-review"...
```

When the sub-run completes, the parent shows:

```
✓ Skill "deep-review" completed (3 turns, 2847 tokens)
```

### 7.2 Verbose mode

When `--explain` is enabled, sub-run turns are displayed with indentation:

```
⚙ Running isolated skill "deep-review"...
  │ Turn 1: 4 tool calls (bash, read_file×2, grep)
  │ Turn 2: 2 tool calls (read_file, bash)
  │ Turn 3: Final response (1204 tokens)
✓ Skill "deep-review" completed (3 turns, 2847 tokens)
```

### 7.3 Result injection

The sub-run's final assistant message is formatted and injected as the tool result:

```
## Skill Result: deep-review

[Sub-run's final assistant text, possibly summarized if over 4K tokens]

---
*Executed in isolated sub-run: 3 turns, 2847 tokens, model: claude-sonnet-4-20250514*
```

---

## 8. Token Budget Enforcement

### 8.1 max_tokens from skill metadata

If the skill specifies `max_tokens: 16384`, the sub-run's `AgenticLoopState` should enforce this:

```rust
// In CliSkillSubRunExecutor::execute_skill_subrun()
let max_turns = if let Some(max_tok) = skill.max_tokens {
    // Rough heuristic: ~4K tokens per turn average
    (max_tok / 4000).max(3).min(15) as u32
} else {
    10  // default sub-run limit
};
```

### 8.2 Global budget tracking

The parent loop should track tokens consumed by sub-runs and include them in
`state.total_prompt` / `state.total_completion` for accurate reporting.

---

## 9. Error Handling

| Scenario | Behavior |
|----------|----------|
| Sub-run times out | Return `[Skill timed out after {n} turns]` as tool result |
| Sub-run hits tool error | Sub-run's own error handling applies; parent only sees final text |
| Sub-run panics | Catch at tokio task boundary; return error string |
| Skill not found | Same as current: `Failed to load skill '{name}'` |
| `isolated: true` but no executor | Fall back to inline execution with warning |
| Sub-run produces empty output | Return `[Skill completed with no output]` |

---

## 10. Implementation Plan

### Phase 1: Data Model (LOW risk)

| # | Task | Files |
|---|------|-------|
| 1.1 | Add `isolated: bool` to `SkillInstruction` | `skill_instructions.rs` |
| 1.2 | Add `isolated: bool` to `SkillMetadata` | `skill_instructions.rs` |
| 1.3 | Add `isolated: bool` to `ResolvedSkill` | `skill_tool.rs` |
| 1.4 | Parse `isolated` from SKILL.md frontmatter | `skill_instructions.rs` |
| 1.5 | Wire `isolated` through `CliSkillResolver::resolve()` | `skill_instructions.rs` |
| 1.6 | Update existing tests | `skill_tool.rs` |

### Phase 2: SkillSubRunExecutor Trait + Branching (MEDIUM risk)

| # | Task | Files |
|---|------|-------|
| 2.1 | Define `SkillSubRunExecutor` trait | `skill_tool.rs` |
| 2.2 | Add `skill_subrun_executor` to `AgenticLoopState` | `agentic_loop_host.rs` |
| 2.3 | Update all AgenticLoopState constructions (`None`) | 6+ files |
| 2.4 | Update `partition_and_execute_skills()` signature | `skill_tool.rs` |
| 2.5 | Add isolation branch in partition function | `skill_tool.rs` |
| 2.6 | Update Step 3c caller in agentic loop | `agentic_loop_host.rs` |
| 2.7 | Refactor `execute_skill()` → `execute_skill_inline()` | `skill_tool.rs` |
| 2.8 | Add tests for isolation branch | `skill_tool.rs` |

### Phase 3: OwnedCliLoopHost (HIGH risk — new host impl)

| # | Task | Files |
|---|------|-------|
| 3.1 | Create `OwnedCliLoopHost` struct | `sse_loop/owned_host.rs` (new) |
| 3.2 | Implement `AgenticLoopHost` for `OwnedCliLoopHost` | `sse_loop/owned_host.rs` |
| 3.3 | Handle `execute_turn` with owned values | `sse_loop/owned_host.rs` |
| 3.4 | Suppress terminal output (headless mode) | `sse_loop/owned_host.rs` |

### Phase 4: CliSkillSubRunExecutor (MEDIUM risk)

| # | Task | Files |
|---|------|-------|
| 4.1 | Create `CliSkillSubRunExecutor` struct | `sse_loop/skill_subrun.rs` (new) |
| 4.2 | Implement `SkillSubRunExecutor` | `sse_loop/skill_subrun.rs` |
| 4.3 | Build `AgenticLoopState` for sub-run | `sse_loop/skill_subrun.rs` |
| 4.4 | Wire into `stream_chat_sse()` state construction | `sse_loop/mod.rs` |
| 4.5 | Add status output (spinner, completion message) | `sse_loop/skill_subrun.rs` |

### Phase 5: Server Implementation (LOW risk — thin wrapper)

| # | Task | Files |
|---|------|-------|
| 5.1 | Create `ServerSkillSubRunExecutor` | `server/skill_subrun.rs` (new) |
| 5.2 | Wire into server state construction | `server/run_lifecycle.rs` |

### Phase 6: Testing & Integration

| # | Task | Files |
|---|------|-------|
| 6.1 | Unit tests for isolation branch | `skill_tool.rs` |
| 6.2 | Unit tests for OwnedCliLoopHost | `sse_loop/owned_host.rs` |
| 6.3 | Integration test with mock executor | `skill_tool.rs` |
| 6.4 | Create example isolated skill in `skills/` | `skills/example-isolated/SKILL.md` |
| 6.5 | `make check` + `cargo test --workspace` | — |

---

## 11. Risk Assessment

| Risk | Severity | Mitigation |
|------|----------|------------|
| `OwnedCliLoopHost` duplicates `CliAgenticLoopHost` logic | HIGH | Share via trait methods or extract common helper functions |
| Sub-run token costs surprise users | MEDIUM | Show token summary in explain mode; enforce `max_tokens` |
| Recursive skill calls (skill calls skill) | LOW | Sub-runs have `skill_resolver: None` — no chaining |
| Permission manager sharing across sub-runs | MEDIUM | Sub-runs should auto-approve (skill already approved by parent) |
| ToolExecutor concurrency (sandboxes, file locks) | MEDIUM | Each sub-run gets its own `ToolExecutor` instance |

---

## 12. Future Extensions

- **Streaming sub-run output** — Forward SSE events from sub-run to parent for real-time display
- **Skill chaining** — Allow isolated skills to call other skills (`skill_resolver: Some(...)`)
- **Parallel skill execution** — Multiple isolated skills running concurrently (fan-out)
- **Skill result caching** — Cache sub-run results by task hash for repeated invocations
- **Skill telemetry** — Track sub-run metrics separately in step recorder

---

## Appendix A: Current Flow (Inline Skills)

```
LLM → tool_call("skill", { skill_name: "review" })
  → partition_and_execute_skills()
    → execute_skill_inline()  (synchronous)
      → resolver.resolve("review")
      → Format instructions as markdown
      → Return (text, Option<SkillActivation>)
    → Apply SkillActivation to state (model override, tool restrictions)
  → Inject formatted text as tool result message
  → Next LLM turn sees instructions in conversation
  → LLM follows instructions within same context
```

## Appendix B: New Flow (Isolated Skills)

```
LLM → tool_call("skill", { skill_name: "deep-review" })
  → partition_and_execute_skills()
    → resolver.resolve("deep-review") → isolated: true
    → skill_subrun_executor.execute_skill_subrun()  (async)
      → Build OwnedCliLoopHost (fresh context)
      → Build AgenticLoopState:
        ├── system: skill instructions
        ├── user: task context
        ├── restricted_tools: complement of allowed_tools
        ├── model: skill.model override
        └── max_turns: derived from max_tokens
      → run_agentic_loop_with_host()
        ├── Turn 1: LLM analyzes task, calls tools
        ├── Turn 2: LLM processes tool results
        ├── Turn N: LLM produces final summary
        └── AgenticLoopOutcome::Completed
      → Extract final assistant text
      → Format as "## Skill Result: deep-review\n\n{text}"
    → No SkillActivation (sub-run is self-contained)
  → Inject formatted result as tool result message
  → Next LLM turn sees summarized result (not raw instructions)
  → Parent loop continues with its own context intact
```
