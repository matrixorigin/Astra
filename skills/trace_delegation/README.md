# trace-delegation

Trace multi-agent delegation flows in **astra**. Visualizes sub-run hierarchy,
coordination patterns, token distribution, verification gates, and aggregation quality.

## Usage

```
/skill trace-delegation
/skill trace-delegation --depth deep
/skill trace-delegation --target <delegation-id>
```

## Delegation Patterns

| Pattern | Description |
|---------|------------|
| **FanOut** | N agents in parallel, results aggregated (FirstSuccess/Merge/VoteOnBest) |
| **Pipeline** | Sequential chain, each output feeds next stage |
| **Sequential** | Simple order, optional stop-on-success |
| **AdversarialReview** | Producer + reviewer iterate until criteria met |

## Agent Tiers

```
ORCHESTRATOR (tier 0) → delegates to SYSTEM & USER
SYSTEM (tier 1) → delegates to USER
USER (tier 2) → no delegation
```

## What It Shows

- 🌳 Sub-run hierarchy tree (nested delegations)
- 📊 Token distribution per agent (balance analysis)
- 🔬 Verification gate results (pass/fail/retry)
- 📋 Timeline (start → sub-run complete → gate → aggregate)
- 🎯 Pattern fit assessment and efficiency score
