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

## 8. Intrinsic Robustness: Self-Correction Within the Chain

### The Gap

Sections 1–7 address "external trust" — audit, replay, guardrails. But an agent can pass every guardrail and still be wrong: it misuses a tool, reasons from a false premise, or drifts because the underlying model changed. These are **chain-internal** failures that no output filter catches.

### Three Self-Correction Mechanisms

**1. Tool-Use Verification (pre-execution)**

Before executing a tool call, the framework validates the call against the skill's declared contract:

```
Agent proposes: git_merge(branch="main", target="main")
  │
  ▼
Contract check:
  - branch == target → self-merge, nonsensical → BLOCK
  - Required param "message" missing → BLOCK
  - Param type mismatch → BLOCK
  │
  ▼
Blocked calls → injected as error event into context
  → Agent sees its own mistake → self-corrects on next turn
```

This is not a guardrail (guardrails filter outputs). This is **structural validation of the agent's reasoning artifact** before it becomes an action.

**2. Reasoning Consistency Check (mid-chain)**

For multi-step plans (PAOR loop), each step's output is checked against the plan's stated goal:

```
Plan: "Deploy service to staging"
  Step 1 output: "Deleted production database"
  │
  ▼
  Consistency check: step output vs plan goal
  - Semantic similarity below threshold → HALT + escalate
  - Destructive action not in plan scope → HALT + escalate
  │
  ▼
  Agent receives: "Step 1 diverged from plan. Halted. Reason: ..."
  → Re-plan or escalate to human
```

**3. Model Drift Detection (cross-session)**

The same prompt + same context should produce similar quality over time. When it doesn't, the model has drifted.

```sql
-- Detect quality drift for a specific prompt template
SELECT
  DATE(created_at) AS day,
  AVG(quality_score) AS avg_quality,
  AVG(quality_score) - LAG(AVG(quality_score), 7) OVER (ORDER BY DATE(created_at)) AS week_delta
FROM conversation_events
WHERE metadata->>'$.prompt_template' = @template_id
  AND event_type = 'llm_response'
GROUP BY DATE(created_at)
HAVING week_delta < -0.5;  -- quality dropped >0.5 in a week
```

When drift is detected:
- Alert: "Prompt template X quality dropped 15% this week"
- Auto-trigger: replay golden sessions with current model → compare
- If confirmed regression: route to fallback model or pin to last-known-good model version

### Drift Auto-Correction Pipeline

Detection alone is insufficient. The system must **automatically correct** without waiting for human intervention:

```
Drift detected (quality_score week_delta < -0.5)
  │
  ▼
Phase 1: CONFIRM (avoid false positives)
  - Replay 20 golden sessions with current model in clone
  - Compare quality scores vs historical baseline
  - If delta < -0.3 confirmed → proceed
  - If delta within noise → dismiss alert
  │
  ▼
Phase 2: DIAGNOSE
  - Which task types degraded? (code_review? planning? Q&A?)
  - Which prompt templates affected?
  - Is it model-wide or template-specific?
  │
  ▼
Phase 3: CORRECT (automatic, ordered by risk)
  ├── Template-specific drift:
  │   → Try prompt variants from evolution history (§4 in evaluation-and-evolution.md)
  │   → Replay golden sessions with each variant
  │   → Best-scoring variant auto-promoted (if > baseline)
  │
  ├── Model-wide drift:
  │   → Route affected task types to fallback model
  │   → Log model_fallback event with reason
  │   → Continue monitoring — if primary recovers, auto-restore
  │
  └── Persistent drift (>7 days, no auto-fix):
      → Escalate to human (via HITL policy §9)
      → Package: drift report + affected sessions + attempted corrections
```

Every correction action is an event — the system's self-repair is as auditable as its decisions.

### The Pattern

External trust (audit, guardrails) catches **what the agent did wrong**. Intrinsic robustness catches **that the agent is going wrong** — before the damage is done.

---

## 9. Human-in-the-Loop: Policy-Driven Supervision

### Beyond "Confirm Side Effects"

Current design requires human approval for destructive tool calls. That's necessary but insufficient. Real-world deployment needs a **policy engine** that determines when humans must be involved based on context, not just action type.

### Supervision Policy Schema

```python
@dataclass
class SupervisionPolicy:
    name: str
    trigger: SupervisionTrigger    # WHEN to involve human
    action: SupervisionAction      # WHAT happens
    scope: str                     # agent / tenant / global

@dataclass
class SupervisionTrigger:
    # Any combination — evaluated as OR
    cost_exceeds: float | None          # estimated cost > threshold
    confidence_below: float | None      # agent confidence < threshold
    affects_resources: list[str] | None # touches production / billing / auth
    plan_depth_exceeds: int | None      # plan has > N steps
    novel_skill_use: bool               # first time using this skill
    escalated_by_agent: bool            # agent explicitly asked for help

class SupervisionAction(Enum):
    APPROVE_REJECT = "approve_reject"       # binary gate
    REVIEW_AND_EDIT = "review_and_edit"     # human can modify before execution
    OBSERVE_ONLY = "observe_only"           # human notified, execution continues
    TAKEOVER = "takeover"                   # human takes control of session
```

### Example Policies

```yaml
policies:
  - name: "high-cost-gate"
    trigger: { cost_exceeds: 5.00 }
    action: approve_reject
    scope: global

  - name: "production-deploy-review"
    trigger: { affects_resources: ["production", "database"] }
    action: review_and_edit
    scope: global

  - name: "low-confidence-escalation"
    trigger: { confidence_below: 0.6 }
    action: review_and_edit
    scope: agent

  - name: "long-plan-checkpoint"
    trigger: { plan_depth_exceeds: 5 }
    action: approve_reject  # approve plan before execution begins
    scope: tenant

  - name: "new-skill-observation"
    trigger: { novel_skill_use: true }
    action: observe_only
    scope: agent
```

### Execution Flow

```
Agent proposes action
  │
  ▼
Policy engine evaluates ALL active policies
  │
  ├── No policy triggered → execute
  │
  ├── OBSERVE_ONLY triggered → execute + notify human async
  │
  ├── APPROVE_REJECT triggered → pause execution
  │   → Human approves → resume
  │   → Human rejects → inject rejection reason into context → agent re-plans
  │
  ├── REVIEW_AND_EDIT triggered → pause execution
  │   → Human edits action params → execute edited version
  │
  └── TAKEOVER triggered → pause agent
      → Human operates directly → agent observes (learns from human actions)
```

### Key Design Decisions

- Policies are **data, not code** — stored in MatrixOne, versioned, auditable
- Multiple policies can trigger simultaneously — most restrictive action wins
- Every policy evaluation is logged as an event — "why was the human involved?" is always answerable
- Agent learns from human overrides: rejected actions become negative training signal, human edits become preference data

---

## 10. Deployment Isolation: Multi-Tenancy as Transparent Infrastructure

### The Principle

**Agents have no concept of tenants.** An agent's logic, memory, skills, and orchestration are identical whether running in a single-tenant deployment or a 1000-tenant SaaS platform. Multi-tenancy is a deployment-time isolation strategy — the platform provides it transparently, the agent never sees it.

This is a deliberate design choice. If agent code contains `tenant_id` checks, you've leaked a deployment concern into the domain model. Every future feature must then ask "does this work in multi-tenant mode?" — a tax that compounds forever.

### What the Agent Sees vs What the Platform Does

| Agent's View | Platform's Reality (Multi-Tenant Deploy) | Platform's Reality (Single-Tenant Deploy) |
|---|---|---|
| `conversation_events` table | Tenant A's database, invisible to Tenant B | The only database |
| `knowledge_entries` table | Scoped to this account's namespace | The only namespace |
| Skill registry | Tenant-local + subscribed marketplace skills | All registered skills |
| Sandbox (CREATE CLONE) | Clone within this account's scope | Clone of the database |
| Snapshot / time-travel | Scoped to this account | Scoped to the database |

The agent issues the same SQL, the same API calls, the same skill invocations. The platform's deployment layer determines the isolation boundary.

### How MatrixOne Makes This Transparent

MatrixOne Multi-Account provides database-level namespace isolation:

```
Single-tenant deployment:
  └── Database: mo_agent
      ├── conversation_events
      ├── knowledge_entries
      └── skills_registry

Multi-tenant deployment:
  ├── Account: tenant_a → Database: mo_agent  (same schema, same queries)
  ├── Account: tenant_b → Database: mo_agent  (completely separate namespace)
  └── Account: sys → Platform admin (cross-account visibility for ops)
```

The agent code connects to `mo_agent` database in both cases. The connection string determines which account — this is infrastructure configuration, not application logic.

**What this eliminates**: `tenant_id` columns, `WHERE tenant_id = ?` on every query, application-level access control middleware, cross-tenant data leak bugs. The isolation is structural (database engine enforced), not logical (application code enforced).

### Cross-Tenant Sharing (When Needed)

Some resources are intentionally shared across tenants — skill marketplace, curated knowledge bases. This uses MatrixOne Publication:

```sql
-- Platform publishes shared resources (ops action, not agent action)
CREATE PUBLICATION skill_marketplace DATABASE shared_skills ACCOUNT ALL;

-- Tenant subscribes (admin action, not agent action)
CREATE DATABASE marketplace_skills FROM sys PUBLICATION skill_marketplace;
```

The agent sees `marketplace_skills` as just another local database. It doesn't know the data comes from a cross-tenant publication.

### Within-Tenant Visibility

Within a single tenant, user-level visibility (private vs team-shared) is handled by views:

```sql
CREATE VIEW my_knowledge AS
SELECT * FROM knowledge_entries
WHERE (visibility = 'user' AND user_id = CURRENT_USER())
   OR visibility IN ('team', 'public');
```

This is the only place where "who can see what" appears in the data model — and it's user-level, not tenant-level.

### Audit Immutability

Orthogonal to tenancy. Works identically in single-tenant and multi-tenant:

```sql
SELECT * FROM conversation_events {SNAPSHOT = 'audit_q1'}
WHERE event_id = 'evt_suspicious';
-- Compare with current state — any difference = tampering evidence
```

---

## 11. Agent-Level SLOs and Platform SLA

### The Gap

Traditional SLOs measure infrastructure (uptime, latency, error rate). Agent platforms need SLOs that measure **agent effectiveness** — did the agent actually help? How reliably? At what cost? Without these, "the platform is up" and "the platform is useful" are conflated.

### Agent SLO Definitions

| SLO | Metric | Target | Measurement |
|---|---|---|---|
| **Response Quality** | avg(quality_score) per agent per day | ≥ 4.0 / 5.0 | Auto-scored by evaluation pipeline (§1-2 in evaluation-and-evolution.md) |
| **Task Completion** | tasks_completed / tasks_attempted | ≥ 95% | Tracked via PAOR loop terminal states |
| **Response Latency** | p95 end-to-end (user query → final response) | < 10s (interactive), < 60s (background) | Measured from event timestamps |
| **Hallucination Rate** | hallucination_detected / total_responses | < 2% | Hallucination firewall (§2) verdicts |
| **Cost Efficiency** | quality_score / cost_per_task | Improving quarter-over-quarter | Router + quality data (§7 in agents-and-orchestration.md) |
| **Regression Gate Pass Rate** | gate_passed / gate_runs | ≥ 98% | Replay gating results (§4) |
| **Self-Correction Rate** | auto_corrected / errors_detected | ≥ 80% | Intrinsic robustness (§8) events |
| **HITL Escalation Rate** | human_escalations / total_decisions | < 5% (trending down) | HITL policy (§9) events |

### SLO Monitoring

```sql
-- Dynamic table: real-time SLO dashboard per agent
CREATE DYNAMIC TABLE agent_slo_dashboard AS
SELECT
  agent_id,
  DATE(created_at) AS day,
  -- Quality SLO
  AVG(quality_score) AS avg_quality,
  AVG(quality_score) >= 4.0 AS quality_slo_met,
  -- Hallucination SLO
  SUM(CASE WHEN metadata->>'$.hallucination_detected' = 'true' THEN 1 ELSE 0 END) * 1.0
    / COUNT(*) AS hallucination_rate,
  -- Latency SLO (seconds between user_query and final response in same chain)
  AVG(CAST(metadata->>'$.response_latency_ms' AS DECIMAL)) / 1000 AS avg_latency_s,
  -- Cost efficiency
  AVG(quality_score) / NULLIF(AVG(CAST(metadata->>'$.total_cost' AS DECIMAL)), 0) AS cost_efficiency
FROM conversation_events
WHERE event_type = 'llm_response'
GROUP BY agent_id, DATE(created_at);
```

### SLO Burn Rate Alerts

Borrowed from SRE practice — don't alert on instantaneous violations, alert on **burn rate** (how fast you're consuming your error budget):

```
Monthly error budget for quality SLO (target 4.0):
  allowed_bad_days = 30 × (1 - 0.95) = 1.5 days

Current burn rate:
  bad_days_this_month / days_elapsed × 30

If projected_bad_days > allowed_bad_days:
  → Alert: "Agent X quality SLO at risk — burning error budget at 3x rate"
  → Auto-action: increase model tier for this agent (cost vs quality tradeoff)
```

### Platform SLA (Composed from Agent SLOs)

The platform SLA is not a single number — it's a composition:

```
Platform SLA = {
  availability: 99.9%                    -- platform is reachable
  agent_quality: 95% of agents meet quality SLO on any given day
  task_completion: 95% platform-wide
  data_durability: 99.999%               -- no event loss (MatrixOne guarantee)
  audit_completeness: 100%               -- every decision has a snapshot
  failover_time: < 30s                   -- provider failover (§10 in agents-and-orchestration.md)
}
```

### SLO Violation Response

| Severity | Condition | Auto-Response |
|---|---|---|
| **Warning** | Burn rate > 1.5x | Alert team, increase monitoring frequency |
| **Critical** | Burn rate > 3x | Auto-upgrade model tier, trigger replay gate on recent sessions |
| **Breach** | SLO violated for the period | Post-mortem event created, agent flagged for review, HITL policy tightened |

Every SLO evaluation and violation response is an event — the platform's operational health is as auditable as its agent decisions.

---

## References

- [DataRobot: Agentic AI Observability](https://www.datarobot.com/blog/agentic-ai-observability/)
- [Microsoft: Zero-Trust Agent Architecture](https://techcommunity.microsoft.com/blog/educatordeveloperblog/zero-trust-agent-architecture-how-to-actually-secure-your-agents/4473995)
- [Authority Partners: AI Agent Guardrails Production Guide 2026](https://authoritypartners.com/insights/ai-agent-guardrails-production-guide-for-2026/)
- [Elixir Data: Decision Lineage](https://www.elixirdata.co/product/decision-lineage/)
- [Portkey: Complete Guide to LLM Observability 2026](https://portkey.ai/blog/the-complete-guide-to-llm-observability/)

Content was rephrased for compliance with licensing restrictions.
