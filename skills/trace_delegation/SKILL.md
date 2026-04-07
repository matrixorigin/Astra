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
patterns, token aggregation, verification gates, and result quality.

## Task

$ARGUMENTS

---

## Phase 1: Locate Delegation Data

### 1.1 Find Delegation Events

```bash
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

---

## Phase 2: Delegation Structure Analysis

### 2.1 Coordination Pattern

Astra supports 4 delegation patterns (defined in `coordination.rs`):

| Pattern | Description | Journal field |
|---------|-------------|---------------|
| **FanOut** | N agents in parallel, results aggregated | `pattern: "fan_out"` |
| **Pipeline** | Sequential chain, output feeds next stage | `pattern: "pipeline"` |
| **Sequential** | Simple sequential, optional stop-on-success | `pattern: "sequential"` |
| **AdversarialReview** | Producer + reviewer iterate | `pattern: "adversarial"` |

### 2.2 Agent Tier Hierarchy

```
ORCHESTRATOR (tier 0) → can delegate to SYSTEM & USER
SYSTEM (tier 1) → can delegate to USER
USER (tier 2) → cannot delegate
```

For each agent: check tier, model override, skill filter, max delegation depth.

### 2.3 Sub-Run Hierarchy Visualization

```
🌳 Delegation: {delegation_id}
│  Pattern: {pattern} | Parent: {parent_run_id}
│
├─ 🔵 sub-run: {run_id_1}
│  Agent: {agent_id} (tier {n}) | Status: {status}
│  Tokens: {prompt}+{completion} = {total} | Tools: {n} calls
│
├─ 🔵 sub-run: {run_id_2}
│  └─ 🔵 nested delegation: {nested_id} (if further delegated)
│
└─ 🟢 Aggregated Result
   Strategy: {FirstSuccess/Merge/VoteOnBest} | Output: {preview}
```

---

## Phase 3: Per-Sub-Run Analysis

For each `DelegationSubRunCompleted` event:

| Sub-Run | Agent | Status | Prompt Tokens | Completion Tokens | Tool Calls | Duration |
|---------|-------|--------|---------------|-------------------|------------|----------|

### Token Distribution

```
Total delegation tokens: {total}
├─ sub-run-1: {tokens} ({pct}%)
├─ sub-run-2: {tokens} ({pct}%)
└─ sub-run-3: {tokens} ({pct}%)
Overhead ratio: {total_sub_runs / equivalent_single_agent}
```

Flags:
- 🔴 One sub-run uses >70% of total tokens (imbalanced work distribution)
- 🟡 Total tokens >3x single-agent estimate (delegation overhead too high)
- 🟢 Sub-runs balanced and total <2x single-agent

---

## Phase 4: Verification, Aggregation & Pause State

### 4.1 Verification Gates

`VerificationGate` checks sub-run quality after completion (max 2 retries by default).
`CheckpointGate` checks progress every N turns during execution (default: every 3 turns).

| Sub-Run | Gate Result | Attempt | Reason (if failed) |
|---------|-------------|---------|-------------------|

Check: first-attempt pass rate, retries needed, any sub-runs that failed all retries.

### 4.2 Adversarial Review Cycles

For `AdversarialReview` delegations, trace producer/reviewer rounds:
- How many rounds needed? Did critiques converge?
- Were acceptance criteria used? Did it hit max_rounds?

### 4.3 Aggregation

| Strategy | What It Does |
|----------|-------------|
| `FirstSuccess` | Takes first completed result |
| `Merge` | JSON-merges all results |
| `VoteOnBest` | Consensus selection |

Flags:
- 🔴 `status: "failed"` — all sub-runs failed
- 🟡 `status: "partial"` — some sub-runs failed
- 🟢 `status: "completed"` — all succeeded

### 4.4 Pause/Resume

Sub-runs check pause flags at tool execution boundaries. Check:
- Was any sub-run or bulk delegation paused/resumed?
- Did pause cause state inconsistency?

---

## Phase 5: Trace Report

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
║  Total: {total} tokens | Overhead: {ratio}x single-agent    ║
║                                                              ║
║  🔬 Verification Gates                                       ║
║  ├─ {sub-run}: {Pass/Fail} (attempt {n})                    ║
║  └─ ...                                                      ║
║                                                              ║
║  🎯 Assessment                                               ║
║  ├─ Pattern fit: {good/poor}                                 ║
║  ├─ Work balance: {balanced/skewed}                          ║
║  ├─ Gate effectiveness: {high/low}                           ║
║  └─ Overall efficiency: {score}                              ║
║                                                              ║
║  💡 Recommendations                                          ║
║  {specific suggestions}                                      ║
╚══════════════════════════════════════════════════════════════╝
```

---

## Common Delegation Issues

| Issue | Symptom | Fix |
|-------|---------|-----|
| Wrong pattern chosen | FanOut for sequential task | Match pattern to task structure |
| Imbalanced fan-out | One agent does 80% of work | Split task more evenly |
| No verification gate | Bad sub-run results aggregated | Add VerificationGate |
| Excessive adversarial rounds | 5+ rounds without convergence | Add acceptance criteria |
| Unnecessary delegation | Single sub-task to one agent | Skip delegation, execute directly |
| Deep nesting | 3+ levels of nested delegation | Flatten hierarchy |

---

## Reference: Key Source Files

| Component | File |
|-----------|------|
| DelegationEngine, VerificationGate, CheckpointGate | `rust/crates/runtime/src/server/delegation_engine.rs` |
| CoordinationPattern, AgentProfile, tiers | `rust/crates/services/src/coordination.rs` |
| Delegation events | `rust/crates/services/src/session_journal.rs` |
| Durable bridge | `rust/crates/astra-cli/src/cli/durable_bridge.rs` |
