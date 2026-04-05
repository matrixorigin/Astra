# Evaluate Session Skill

Agent self-assessment skill that evaluates performance metrics and provides optimization recommendations.

## Overview

This skill enables the agent to:
- Analyze token and call efficiency for a session
- Assess skill usage patterns
- Optimize for cost, accuracy, latency, or balanced performance
- Provide actionable recommendations with trade-off analysis

## Usage

### Basic Evaluation
```
"Evaluate the performance of session 019ca9f1-3dc6-72b3-9813-1f38f7349c53"
"How efficient was I in this conversation?"
```

### With Optimization
```
"Evaluate this session and optimize for cost"
"Analyze last session, focus on accuracy"
```

### As a Tool Call
```json
{
  "tool": "evaluate_session",
  "arguments": {
    "target_session_id": "019ca9f1-...",
    "objective": "cost",
    "include_details": false
  }
}
```

## Optimization Objectives

| Objective | Focus | Trade-offs |
|-----------|-------|------------|
| **cost** | Minimize tokens, compress history | May reduce context quality |
| **accuracy** | Maximize correctness, add verification | Higher cost and latency |
| **latency** | Reduce call count, parallelize | May miss context updates |
| **balanced** | Weighted combination (cost 0.3, accuracy 0.4, latency 0.3) | Moderate |

## Efficiency Ratings

**Token Efficiency** (tokens per query):

| Rating | Threshold |
|--------|-----------|
| excellent | < 10,000 |
| good | 10,000 - 19,999 |
| moderate | 20,000 - 39,999 |
| needs_improvement | ≥ 40,000 |

**Call Efficiency** (LLM calls per query):

| Rating | Threshold |
|--------|-----------|
| excellent | ≤ 2 |
| good | 2.1 - 4 |
| moderate | 4.1 - 6 |
| needs_improvement | > 6 |

## Implementation Notes

- Reads from `agent_events` table
- Uses `target_session_id` parameter (not `session_id`) to avoid collision with framework-injected fields
- Optimization scores are relative to session baseline
