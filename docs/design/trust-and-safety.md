# Trust and Safety

> **Status**: Core Design — single source of truth for audit, verification, and safety  
> **Last Updated**: 2026-02-14

---

## Why Trust Is the Product

The industry is converging on a hard truth: **intelligence is not the bottleneck for agent adoption — trust is.** McKinsey reports only 1% of organizations consider their AI adoption fully mature. The gap is not capability but governance, auditability, and safety.

mo-agent-engine's competitive position is not "smarter agents" but **provably trustworthy agents**. Every capability in this document is platform infrastructure — agents get it automatically, developers don't implement it.

---

## 1. Decision Audit Trail

### The Problem

An agent recommends a code fix. Three months later: "Why did the agent do that?" The data changed, the prompt was updated, the context window is gone. No reconstruction possible.

### The Solution: Decision Lineage

Every agent decision binds to a versioned data snapshot. The complete chain of inputs, reasoning, and outputs is recorded as it happens — not reconstructed after incidents.

```
Decision Record = {
  decision_id,
  event_id,                    -- The LLM response event
  context_snapshot_id,         -- Exactly what the LLM saw (see memory-and-context.md)
  prompt_template_id@version,  -- Which instructions
  skills_used: [{id, version}],-- Which capabilities
  llm_model_used,              -- Which model (including checkpoint)
  llm_params,                  -- temperature, seed, max_tokens
  output,                      -- What the agent decided
  confidence_score,            -- Pre-delivery confidence
  quality_score,               -- Post-delivery evaluation (filled later)
  causal_chain_id              -- Full request chain
}
```

**Reconstruction guarantee**: Given any `decision_id`, load the `context_snapshot`, resolve the `prompt_template` by version, and you have the exact input that produced the output. The only uncontrolled variable is LLM non-determinism — and that's a much smaller audit surface than "we have no idea what happened."

### What "Versioning" Means

Versioning means **recording**, not **constraining**. An agent using `temperature=0.9` for creative writing is equally auditable as one using `temperature=0` for code review. The goal is reproducibility of the audit trail, not reproducibility of the output.

---

## 2. Hallucination Firewall

### The Problem

LLM generates claims that contradict the data it was given. Users have no signal for reliability.

### The Solution: Snapshot-Consistent Verification

Before delivering any LLM response:

```
1. EXTRACT: Pull verifiable claims from response
   - Numeric values ("5 files changed")
   - Historical references ("as discussed yesterday")
   - Factual assertions ("the API returns JSON")
   - Causal claims ("the test fails because of X")

2. VERIFY: Check each claim against the SAME data snapshot the LLM saw
   - Use context_snapshot to identify the exact data state
   - Query using snapshot-consistent reads
   - Classify: verified | contradicted | unverifiable

3. DECIDE: Based on verification results
   - All verified → deliver with high confidence
   - Some unverifiable → deliver with annotations
   - Any contradicted → block or annotate with warnings

4. LOG: Record verification in hallucination_checks table
   - claims_total, claims_verified, claims_contradicted
   - contradiction details for debugging
   - safe_to_deliver decision
```

**Why snapshot-consistency matters**: If data changed between generation and verification, you get false positives/negatives. Verifying against the same snapshot eliminates this.

### Claim Extraction: LLM-Based, Not Regex

Current implementation uses regex patterns. The upgrade path:

```python
# Use a small, fast model (gpt-4o-mini) for claim extraction
claims = llm.extract_claims(
    response_text=response,
    context_summary=snapshot.summary,
    claim_types=["numeric", "temporal", "causal", "factual"]
)
```

This catches claims that regex misses: implicit assertions, comparative statements, causal reasoning.

---

## 3. Uncertainty Quantification

### The Problem

Users treat all agent responses as equally reliable. They shouldn't.

### The Solution: Pre-Delivery Confidence Scoring

Every response carries a `confidence_score` (0.0–1.0) computed from signals already in the pipeline:

```
confidence_score = weighted_average(
  context_coverage:      How much relevant data was available in context?
  claim_verifiability:   What fraction of claims could be checked against snapshot?
  knowledge_freshness:   How current is the underlying data?
  skill_reliability:     Historical success rate of the skills used?
  model_agreement:       (future) Do multiple models agree on this response?
)

uncertainty_factors = {
  context_coverage: 0.92,
  claim_verifiability: 0.85,
  knowledge_freshness: 0.78,
  skill_reliability: 0.95
}
```

**Calibration**: `confidence` (pre-delivery prediction) is calibrated against `quality_score` (post-delivery evaluation from user feedback or auto-metrics). If the system consistently over- or under-estimates confidence, the scoring weights are adjusted. This measures how well the system knows what it doesn't know.

**User-facing**: High-risk decisions (medical, financial, legal) can require minimum confidence thresholds. Users see confidence level before acting.

---

## 4. Regression Gate

### The Problem

Prompt/skill changes might break existing good behavior. Teams either iterate too slowly (afraid to break things) or too recklessly (ship and pray).

### The Solution: Automated Quality Gate

Before any change reaches production:

```
1. SNAPSHOT: Capture current production state
2. SELECT: Load golden sessions (quality_score >= 4.0, last 50)
3. SANDBOX: Create isolated environment from snapshot
4. REPLAY: Run golden sessions against the change (with tool mocking)
5. MEASURE: Compute quality metrics
   - Error rate (must be < 5%)
   - Score delta (must not regress on high-score sessions)
   - Latency delta
   - Token efficiency
   - Skill accuracy
6. DECIDE: Pass/fail with full data lineage
7. RECORD: Store in gate_results with snapshot reference, sessions tested, metrics
8. CLEANUP: Destroy sandbox
```

**Triggers**: skill_version_changed, prompt_template_changed, config_changed, model_changed.

**Data lineage**: Every gate result records which snapshot was used, which sessions were tested, and what metrics were computed. This is auditable — you can prove that a change was tested before deployment.

---

## 5. Observability

### Agentic Observability ≠ Traditional APM

Traditional observability captures what happened. Agentic observability captures **why decisions were made**. This is the distinction that DataRobot, Portkey, and the industry are converging on.

### What We Observe

| Layer | Metrics | Why |
|-------|---------|-----|
| **Decision** | confidence_score, quality_score, hallucination_rate | Trust calibration |
| **Context** | assembly_time, token_utilization, cache_hit_rate, retrieval_relevance | Context engineering effectiveness |
| **Memory** | knowledge_entry_count, decay_rate, retrieval_precision | Memory health |
| **Skill** | selection_accuracy, execution_success_rate, side_effect_profile | Skill reliability |
| **LLM** | latency (p50/p95/p99), cost_per_call, error_rate, provider_availability | Infrastructure health |
| **Session** | turns_per_session, compaction_frequency, cross_session_continuity | User experience |

### Alerts

- **Trust**: confidence_score consistently below threshold, hallucination rate spike
- **Quality**: quality_score drop, regression gate failure rate increase
- **Cost**: budget threshold reached, unusual spend spike
- **Infrastructure**: provider circuit breaker open, high latency, high error rate

---

## 6. Guardrails: Defense in Depth

Following the industry consensus on layered AI safety:

```
Layer 1: INPUT VALIDATION
  - Prompt injection detection
  - Input length limits
  - PII detection and masking

Layer 2: PERMISSION ENFORCEMENT
  - Skill-level: side_effect_profile checked before execution
  - Resource-level: ownership verification (owner_user_id)
  - Budget-level: pre-call cost estimation

Layer 3: EXECUTION ISOLATION
  - Data sandbox: separate MatrixOne database
  - Tool mocking: recorded results for replay (no real API calls)
  - Code sandbox: Docker container with resource limits

Layer 4: OUTPUT VERIFICATION
  - Hallucination firewall (snapshot-consistent)
  - Confidence scoring
  - Claim annotation

Layer 5: POST-DELIVERY MONITORING
  - User feedback collection
  - Auto-metric evaluation
  - Regression detection
```

No single layer is sufficient. The combination provides defense in depth.

---

## 7. Side-Effect Isolation

### The Fatal Gap

Data sandbox (MatrixOne DB isolation) ≠ Execution sandbox (external API isolation). Replaying a session that merged a PR must NOT merge the PR again.

### Three-Layer Isolation

| Layer | Scope | Mechanism |
|-------|-------|-----------|
| **Data** | MatrixOne database | Separate DB via CLONE/SNAPSHOT |
| **Execution** | Tool/Skill invocations | ToolMockingLayer: production / replay / dry_run modes |
| **Code** | Generated code execution | Docker container: no network, limited CPU/memory, timeout |

### Skill Classification

Every skill declares its side-effect profile:

```python
side_effect_profile = {
    "category": "read" | "write" | "destructive",
    "external_apis": ["github", "slack"],
    "idempotent": True | False,
    "reversible": True | False,
    "mock_strategy": "recorded" | "noop" | "error"
}
```

| Category | Production | Replay | Dry Run |
|----------|-----------|--------|---------|
| **read** | Real call | Real call (safe) | Validate only |
| **write** | Real call + record result | Return recorded result | Validate only |
| **destructive** | Real call + record + confirm | Block + error | Block + error |

---

## 7. Multi-Tenant Isolation as Database Guarantee

### The Problem

Application-level access control has bugs. Every SaaS company has had a cross-tenant data leak caused by a missing WHERE clause or a broken middleware check.

### The Solution: MatrixOne Multi-Account

Tenant isolation is not enforced by application code. It's enforced by the database engine.

```
Platform (sys account)
  ├── Tenant A (account: tenant_a)
  │   ├── conversation_events  ← invisible to Tenant B, enforced by engine
  │   ├── knowledge_entries    ← isolated
  │   └── skills_registry      ← tenant-specific + subscribed marketplace skills
  │
  ├── Tenant B (account: tenant_b)
  │   └── ... completely separate namespace
  │
  └── Shared knowledge (via Publication)
      └── Platform publishes curated knowledge bases
          Tenants subscribe → read-only access, auto-updated
```

**What this eliminates**: Row-level security policies, tenant_id columns on every table, middleware tenant checks, "forgot the WHERE clause" bugs. The isolation is structural, not logical.

### Within-Tenant Visibility

For user-private vs team-shared memory within a single tenant:

```sql
CREATE VIEW my_knowledge AS
SELECT * FROM knowledge_entries
WHERE (visibility = 'user' AND user_id = CURRENT_USER())
   OR visibility IN ('team', 'public');
```

### Audit Immutability

MatrixOne's MVCC guarantees that historical events are never modified. Time-travel queries prove data integrity:

```sql
-- Verify no events were tampered with since audit checkpoint
SELECT * FROM conversation_events {SNAPSHOT = 'audit_q1'}
WHERE event_id = 'evt_suspicious';
-- Compare with current state — any difference = tampering evidence
```

---

## References

- [DataRobot: Agentic AI Observability](https://www.datarobot.com/blog/agentic-ai-observability/)
- [Microsoft: Zero-Trust Agent Architecture](https://techcommunity.microsoft.com/blog/educatordeveloperblog/zero-trust-agent-architecture-how-to-actually-secure-your-agents/4473995)
- [Authority Partners: AI Agent Guardrails Production Guide 2026](https://authoritypartners.com/insights/ai-agent-guardrails-production-guide-for-2026/)
- [Elixir Data: Decision Lineage](https://www.elixirdata.co/product/decision-lineage/)
- [Portkey: Complete Guide to LLM Observability 2026](https://portkey.ai/blog/the-complete-guide-to-llm-observability/)

Content was rephrased for compliance with licensing restrictions.
