# Generation Quality

> **Last Updated**: 2026-02-21

How astra-engine ensures the quality and safety of LLM-generated responses before delivery.

## Hallucination Firewall

**Module**: `core/verification/firewall.py`

The `HallucinationFirewall` intercepts every LLM response and verifies factual claims against the context snapshot the LLM actually saw.

### Verification Flow

```
LLM response
    → Extract claims (factual, causal, temporal, numeric)
    → For each claim: verify against context_snapshot
        - Substring match (fast path)
        - Embedding similarity ≥ 0.75 (semantic fallback)
    → Compute weighted confidence score
    → Decision: deliver / warn / block (based on mode)
```

### Claim-Type Weighting

Not all claims carry equal risk. Confidence is weighted by claim type:

| Claim Type | Weight | Rationale |
|-----------|--------|-----------|
| Causal | 1.0 | "X causes Y" — highest risk if wrong |
| Factual | 0.8 | "X is Y" — standard factual claims |
| Temporal | 0.6 | "X happened before Y" — ordering claims |
| Numeric | 0.5 | "X is 42" — often approximate |

### Degraded Mode Behavior

When infrastructure fails (snapshot unavailable, claim extraction error):

| Mode | Behavior |
|------|----------|
| `warn` | Deliver with `confidence=0.5`, log warning |
| `block` | Block delivery with `confidence=0.0` |

### Streaming Verification

**Module**: `core/verification/streaming_verifier.py`

For streaming responses, the `StreamingVerifier` accumulates text, detects sentence boundaries, and verifies each sentence against the context snapshot. Inline `⚠️` warnings are injected into the stream when claims fail verification.

## Chain-of-Thought Audit

**Module**: `core/verification/alignment_check.py`

`AlignmentCheck` compares the LLM's reasoning chain against its final output to detect reasoning-output misalignment — cases where the model "thinks" one thing but "says" another.

## Verification Logging

All verification results are persisted to `hallucination_checks` and `claim_evidence` tables using parameterized SQL (`sqlalchemy.text()` with named parameters), enabling post-hoc analysis of firewall accuracy.
