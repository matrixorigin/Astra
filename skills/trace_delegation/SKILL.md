---
name: trace-delegation
description: "Developer skill: trace multi-agent delegation flows in astra — fan-out, pipeline, sequential, adversarial patterns. Visualizes sub-run hierarchy, token aggregation, verification gates, and pause/resume state."
user_invocable: true
when_to_use: "When the user wants to trace or visualize multi-agent delegation flows, sub-run hierarchies, or fan-out patterns"
arguments:
  - name: TARGET
    description: "Session ID, delegation ID, or 'last'. Omit for most recent delegation."
    required: false
  - name: DEPTH
    description: "Trace depth: 'summary' (top-level only), 'detail' (per sub-run), 'deep' (full message trace). Default: detail"
    required: false
allowed_tools:
  - bash
  - read_file
  - grep
  - glob
---
# Trace Delegation

Trace multi-agent delegation flows in astra. Visualizes the sub-run hierarchy, coordination
patterns, token aggregation, verification gates, and result quality. Essential for debugging
complex multi-agent task execution.

## Task

$ARGUMENTS

---

## Phase 1: Locate Delegation Data

### 1.1 Find Delegation Events

```bash
# Find delegation events in session journals
grep -h '"DelegationStarted"\|"DelegationSubRunCompleted"\|"DelegationCompleted"' \
  ~/.astra/sessions/*.jsonl 2>/dev/null | python3 -c "
import json, sys
events = [json.loads(l) for l in sys.stdin]
for e in events:
    ts = e.get('ts', '?')[:19]
    etype = e.get('type', '?')
    meta = e.get('metadata', {})
    did = meta.get('delegation_id', '?')[:12]
    print(f'{ts} | {etype:30s} | delegation={did}')
"
```

### 1.2 Resolve TARGET

| TARGET | Action |
|--------|--------|
| Delegation ID | Filter events by `metadata.delegation_id` |
| Session ID | Find all delegations in that session |
| `"last"` / omitted | Most recent `DelegationStarted` event |

### 1.3 Load Complete Delegation Timeline

For the target delegation, extract all related events in order:

```bash
grep '<DELEGATION_ID>' ~/.astra/sessions/<SESSION_ID>.jsonl | python3 -c "
import json, sys
for line in sys.stdin:
    e = json.loads(line)
    print(json.dumps(e, indent=2))
"
```

---

## Phase 2: Delegation Structure Analysis

### 2.1 Coordination Pattern

Astra supports 4 delegation patterns (`coordination.rs`):

| Pattern | Description | How to Identify |
|---------|-------------|----------------|
| **FanOut** | N agents in parallel, results aggregated | `pattern: "fan_out"`, multiple agent_ids |
| **Pipeline** | Sequential chain, output feeds next stage | `pattern: "pipeline"`, stages in metadata |
| **Sequential** | Simple sequential, optional stop-on-success | `pattern: "sequential"`, ordered agent_ids |
| **AdversarialReview** | Producer + reviewer iterate | `pattern: "adversarial"`, producer_id + reviewer_id |

From the `DelegationStarted` event metadata:
```json
{
  "delegation_id": "...",
  "parent_run_id": "...",
  "pattern": "fan_out",
  "agent_ids": ["agent-a", "agent-b", "agent-c"],
  "agent_count": 3
}
```

### 2.2 Agent Tier Analysis

Astra's agent hierarchy:
```
ORCHESTRATOR (tier 0) → can delegate to SYSTEM & USER
SYSTEM (tier 1) → can delegate to USER
USER (tier 2) → cannot delegate
```

For each agent in the delegation:
- What tier is it?
- Does it have a model override? (cheaper model for sub-tasks)
- What skill filter is applied?
- What's its max delegation depth?

### 2.3 Sub-Run Hierarchy Visualization

Build a tree from `DelegationTracker` data:

```
🌳 Delegation: {delegation_id}
│  Pattern: {pattern}
│  Parent: {parent_run_id}
│
├─ 🔵 sub-run: {run_id_1}
│  Agent: {agent_id} (tier: {tier})
│  Status: {completed/failed/paused}
│  Tokens: {prompt}+{completion} = {total}
│  Tools: {tool_count} calls
│  Duration: {duration}
│
├─ 🔵 sub-run: {run_id_2}
│  Agent: {agent_id} (tier: {tier})
│  Status: {status}
│  ...
│  │
│  └─ 🔵 nested delegation: {nested_id}
│     (If agent delegated further)
│
└─ 🟢 Aggregated Result
   Strategy: {FirstSuccess/Merge/VoteOnBest}
   Output: {preview}
```

---

## Phase 3: Per-Sub-Run Analysis

For each `DelegationSubRunCompleted` event:

### 3.1 Sub-Run Metrics

```
| Sub-Run | Agent | Status | Prompt Tokens | Completion Tokens | Tool Calls | Duration |
|---------|-------|--------|---------------|-------------------|------------|----------|
```

### 3.2 Sub-Run Quality Assessment

From each sub-run's result:
- Did it complete successfully?
- If failed, what was the error?
- Was the result used in aggregation? (or discarded)

### 3.3 Token Efficiency Across Sub-Runs

```
Total delegation tokens: {total}
├─ sub-run-1: {tokens} ({pct}%)
├─ sub-run-2: {tokens} ({pct}%)
└─ sub-run-3: {tokens} ({pct}%)

Overhead ratio: {total_sub_runs / equivalent_single_agent}
```

Flag:
- 🔴 One sub-run uses >70% of total tokens (imbalanced work distribution)
- 🟡 Total tokens >3x single-agent estimate (delegation overhead too high)
- 🟢 Sub-runs balanced and total <2x single-agent

---

## Phase 4: Verification Gate Analysis

### 4.1 VerificationGate (Post-Completion)

Astra's `VerificationGate` trait checks sub-run quality after completion:

```rust
trait VerificationGate {
    async fn verify(&self, result: &AgentResult, delegation_id: &str, attempt: u32) -> GateVerdict;
    fn max_retries(&self) -> u32 { 2 }
}

enum GateVerdict { Pass, Fail { reason, details }, Skip }
```

From delegation events, check:
- Was a verification gate configured?
- How many sub-runs passed on first attempt?
- How many required retries?
- Any sub-runs that failed all retries?

```
| Sub-Run | Gate Result | Attempt | Reason (if failed) |
|---------|-------------|---------|-------------------|
```

### 4.2 CheckpointGate (Mid-Execution)

Astra's `CheckpointGate` checks progress every N turns during execution:

```rust
trait CheckpointGate {
    async fn check(&self, run_id: &str, turn_index: u32, total_tool_calls: u32) -> Result<bool, String>;
    fn checkpoint_frequency(&self) -> u32 { 3 }  // Check every 3 turns
}
```

Check:
- Was a checkpoint gate configured?
- Was any sub-run aborted mid-execution?
- At what turn/tool-call count was it aborted?

### 4.3 Adversarial Review Pattern

For `AdversarialReview` delegations, trace the producer/reviewer cycle:

```
Round 1:
  Producer → {output preview}
  Reviewer → {critique preview}
  Verdict: Continue

Round 2:
  Producer → {revised output}
  Reviewer → {critique}
  Verdict: Accept (all criteria met)
```

Check:
- How many rounds were needed?
- Did the reviewer's critiques converge (getting shorter/fewer)?
- Were acceptance criteria used to guide the reviewer?
- Did the cycle hit max_rounds without convergence?

---

## Phase 5: Aggregation Analysis

### 5.1 Aggregation Strategy

From `DelegationCompleted` event:

| Strategy | What It Does |
|----------|-------------|
| `FirstSuccess` | Takes first completed result |
| `Merge` | JSON-merges all results |
| `VoteOnBest` | Consensus selection |
| `Custom` | Strategy-specific logic |

### 5.2 Result Aggregation Quality

```
DelegationResult:
  delegation_id: {id}
  status: {completed/partial/failed}
  total_prompt_tokens: {n}
  total_completion_tokens: {n}
  total_tool_calls: {n}
  output: {preview}
  errors: [{list}]
```

Flag:
- 🔴 `status: "failed"` — all sub-runs failed
- 🟡 `status: "partial"` — some sub-runs failed, partial result used
- 🟢 `status: "completed"` — all sub-runs succeeded

### 5.3 Information Loss in Aggregation

For `Merge` and `VoteOnBest` strategies:
- Was any sub-run output discarded?
- Did merge produce conflicts?
- Was the "best" vote well-justified?

---

## Phase 6: Pause/Resume State

### 6.1 Pause Flag State

Astra's `DelegationTracker` manages per-run pause flags (`Arc<AtomicBool>`):

From journal, check for pause/resume events:
- Was any sub-run paused?
- Was a bulk delegation pause issued?
- Were paused sub-runs resumed?
- Did pause cause any state inconsistency?

### 6.2 Cooperative Pause Mechanism

Sub-runs check `pause_flag` at tool execution boundaries. Check:
- How quickly did the sub-run respond to pause?
- Was any tool execution interrupted?
- Was state preserved correctly across pause/resume?

---

## Phase 7: Delegation Trace Report

```
╔══════════════════════════════════════════════════════════════╗
║  🌐 Delegation Trace                                         ║
║  Delegation: {delegation_id}                                 ║
║  Pattern: {pattern} | Agents: {n}                            ║
╠══════════════════════════════════════════════════════════════╣
║                                                              ║
║  🌳 Hierarchy                                                ║
║  {tree visualization}                                        ║
║                                                              ║
║  📊 Token Distribution                                       ║
║  {bar chart showing per-agent token usage}                   ║
║  Total: {total} tokens | Overhead: {ratio}x single-agent    ║
║                                                              ║
║  🔬 Verification Gates                                       ║
║  ├─ {sub-run}: {Pass/Fail} (attempt {n})                    ║
║  └─ ...                                                      ║
║                                                              ║
║  📋 Timeline                                                 ║
║  {timestamp} Started: {agent} (tier {n})                     ║
║  {timestamp} Sub-run completed: {agent} → {status}           ║
║  {timestamp} Gate: {verdict}                                 ║
║  {timestamp} Aggregation: {strategy} → {status}              ║
║                                                              ║
║  🎯 Assessment                                               ║
║  ├─ Pattern fit: {good/poor} — {explanation}                 ║
║  ├─ Work balance: {balanced/skewed}                          ║
║  ├─ Gate effectiveness: {high/low}                           ║
║  └─ Overall efficiency: {score}                              ║
║                                                              ║
║  💡 Recommendations                                          ║
║  {specific suggestions}                                      ║
║                                                              ║
╚══════════════════════════════════════════════════════════════╝
```

---

## Common Delegation Issues

| Issue | Symptom | Fix |
|-------|---------|-----|
| Wrong pattern chosen | FanOut for sequential task, Pipeline for independent tasks | Match pattern to task structure |
| Imbalanced fan-out | One agent does 80% of work | Split task more evenly |
| No verification gate | Bad sub-run results aggregated | Add VerificationGate |
| Excessive adversarial rounds | 5+ rounds without convergence | Add acceptance criteria, increase max_rounds threshold |
| Unnecessary delegation | Single sub-task delegated to one agent | Skip delegation, execute directly |
| Deep nesting | 3+ levels of nested delegation | Flatten hierarchy |

---

## Reference: Key Source Files

| Component | File |
|-----------|------|
| DelegationEngine | `rust/crates/runtime/src/server/delegation_engine.rs` |
| CoordinationPattern | `rust/crates/services/src/coordination.rs` |
| AgentProfile & tiers | `rust/crates/services/src/coordination.rs` |
| Delegation events | `rust/crates/services/src/session_journal.rs` |
| Durable bridge | `rust/crates/astra-cli/src/cli/durable_bridge.rs` |
| Verification gate | `rust/crates/runtime/src/server/delegation_engine.rs` (VerificationGate trait) |
| Checkpoint gate | `rust/crates/runtime/src/server/delegation_engine.rs` (CheckpointGate trait) |
