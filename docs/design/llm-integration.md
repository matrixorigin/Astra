# LLM Integration Design

**Version**: 1.0  
**Status**: Production-Ready  
**Last Updated**: 2026-02-10

## 1. Vision and Goals

### 1.1 Vision

Enable mo-dev-agent to leverage multiple LLM providers with:
- **Complete cost transparency** - Every token tracked in MatrixOne
- **Provider independence** - No vendor lock-in
- **Reproducibility** - Replay any LLM call from 10 years ago
- **Budget control** - Per-user, per-tenant, per-skill limits
- **Quality assurance** - A/B testing, prompt versioning, feedback loops

### 1.2 Goals

1. **Support Vision**: Enable all LLM-powered capabilities in vision-and-mission.md
2. **Cost Control**: Track every cent spent on LLM calls
3. **Provider Flexibility**: Swap providers without code changes
4. **Reproducibility**: "10 years later, reproduce today's LLM decision"
5. **Quality**: Continuous improvement through feedback loops
6. **Hallucination Prevention**: Real-time fact verification against versioned data before delivering responses
7. **Cost Prediction**: Predict execution cost from historical data before spending
8. **Performance**: Sub-second latency for 95% of calls

## 2. Architecture Overview

```
┌─────────────────────────────────────────────────────────────┐
│                        mo-dev-agent                          │
├─────────────────────────────────────────────────────────────┤
│  Skills Layer                                                │
│  ┌──────────────┐  ┌──────────────┐  ┌──────────────┐      │
│  │ PR Review    │  │ Issue Triage │  │ CI Analysis  │ ...  │
│  └──────┬───────┘  └──────┬───────┘  └──────┬───────┘      │
│         │                  │                  │              │
├─────────┴──────────────────┴──────────────────┴─────────────┤
│  LLM Client Layer                                            │
│  ┌──────────────────────────────────────────────────────┐   │
│  │  LLMClient (unified interface)                       │   │
│  │  - Config from MatrixOne (provider, model, budget)   │   │
│  │  - Provider abstraction (OpenAI, Groq, Anthropic)    │   │
│  │  - Cost calculation (per-token pricing)              │   │
│  │  - Budget enforcement (per-user, per-tenant limits)  │   │
│  │  - Call logging (all calls → llm_call_logs)          │   │
│  │  - Retry logic (exponential backoff)                 │   │
│  │  - Caching (semantic cache for repeated queries)     │   │
│  └──────────────────────────────────────────────────────┘   │
├─────────────────────────────────────────────────────────────┤
│  Prompt Management                                           │
│  ┌──────────────┐  ┌──────────────┐  ┌──────────────┐      │
│  │PromptRegistry│  │VersionControl│  │  A/B Testing │      │
│  └──────────────┘  └──────────────┘  └──────────────┘      │
├─────────────────────────────────────────────────────────────┤
│  MatrixOne (State Store)                                     │
│  - configs (LLM settings), prompt_templates (versioned)      │
│  - llm_call_logs (cost tracking), llm_budgets (limits)       │
│  - conversation_events (full context for replay)             │
└─────────────────────────────────────────────────────────────┘
```

## 3. Core Capabilities

### 3.1 Provider Abstraction

**Supported Providers**:
- **OpenAI**: GPT-4, GPT-4-turbo, GPT-3.5-turbo
- **Groq**: Llama3-70B, Mixtral-8x7B (ultra-fast inference)
- **Anthropic**: Claude-3 (Opus, Sonnet, Haiku)
- **Self-hosted**: Ollama, vLLM, TGI (for privacy-sensitive use cases)

**Unified Interface**:
```python
class LLMClient:
    def chat(
        self,
        messages: list[LLMMessage],
        event_id: str,
        user_id: str,
        model: Optional[str] = None,
        temperature: Optional[float] = None,
    ) -> LLMResponse:
        """Send chat request to LLM.
        
        - Loads config from MatrixOne
        - Routes to appropriate provider
        - Calculates cost
        - Logs call
        - Returns response
        """
```

**Provider Selection**:
1. **Explicit**: Skill specifies provider/model
2. **Config-based**: Load from `configs.llm_config`
3. **Fallback chain**: Primary → Secondary → Tertiary
4. **Cost-optimized**: Route to cheapest provider for task

### 3.2 Cost Management

#### 3.2.1 Cost Calculation

**Pricing Table** (stored in MatrixOne):
```sql
CREATE TABLE llm_pricing (
  pricing_id          VARCHAR(64) PRIMARY KEY,
  provider            VARCHAR(50) NOT NULL,
  model               VARCHAR(100) NOT NULL,
  price_per_1k_prompt DECIMAL(10, 6) NOT NULL,
  price_per_1k_completion DECIMAL(10, 6) NOT NULL,
  effective_from      TIMESTAMP NOT NULL,
  effective_to        TIMESTAMP,  -- NULL = current pricing
  created_at          TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
  
  INDEX idx_provider_model (provider, model),
  INDEX idx_effective (effective_from, effective_to)
);
```

**Cost Calculation** (with historical pricing support):
```python
def calculate_cost(
    provider: str,
    model: str,
    tokens_prompt: int,
    tokens_completion: int,
    call_timestamp: datetime,  # ✅ Use call timestamp for replay
) -> float:
    """Calculate cost using historical pricing.
    
    Query pricing valid at call_timestamp:
    - effective_from <= call_timestamp
    - (effective_to IS NULL OR effective_to > call_timestamp)
    """
    query = """
        SELECT price_per_1k_prompt, price_per_1k_completion
        FROM llm_pricing
        WHERE provider = %s 
          AND model = %s
          AND effective_from <= %s
          AND (effective_to IS NULL OR effective_to > %s)
        ORDER BY effective_from DESC
        LIMIT 1
    """
    pricing = db.fetchone(query, (provider, model, call_timestamp, call_timestamp))
    
    if not pricing:
        # Fallback to hardcoded pricing (for backward compatibility)
        pricing = get_default_pricing(provider, model)
    
    cost_usd = (
        tokens_prompt * (pricing['price_per_1k_prompt'] / 1000) +
        tokens_completion * (pricing['price_per_1k_completion'] / 1000)
    )
    return round(cost_usd, 6)
```

**Historical Pricing Example**:
```sql
-- Original pricing (2026-01-01)
INSERT INTO llm_pricing VALUES (
  'pricing_1', 'openai', 'gpt-4',
  0.03, 0.06,  -- $0.03/1K prompt, $0.06/1K completion
  '2026-01-01 00:00:00', '2026-02-01 00:00:00',
  NOW()
);

-- Price drop (2026-02-01)
INSERT INTO llm_pricing VALUES (
  'pricing_2', 'openai', 'gpt-4',
  0.02, 0.04,  -- New lower pricing
  '2026-02-01 00:00:00', NULL,  -- NULL = current
  NOW()
);
```

**Replay Accuracy**:
- Call on 2026-01-15 → Uses pricing_1 ($0.03/$0.06)
- Call on 2026-02-10 → Uses pricing_2 ($0.02/$0.04)
- Replay 2026-01-15 call on 2026-02-10 → Still uses pricing_1 ✅

#### 3.2.2 Budget Control

**Budget Table**:
```sql
CREATE TABLE llm_budgets (
  budget_id           VARCHAR(64) PRIMARY KEY,
  scope_type          VARCHAR(50) NOT NULL,  -- 'user' | 'tenant' | 'skill' | 'global'
  scope_id            VARCHAR(255),
  budget_period       VARCHAR(50) NOT NULL,  -- 'daily' | 'weekly' | 'monthly'
  budget_limit_usd    DECIMAL(10, 2) NOT NULL,
  current_spend_usd   DECIMAL(10, 2) DEFAULT 0,
  period_start        TIMESTAMP NOT NULL,
  period_end          TIMESTAMP NOT NULL,
  alert_threshold     DECIMAL(3, 2) DEFAULT 0.8,  -- Alert at 80%
  is_active           BOOLEAN DEFAULT TRUE,
  
  INDEX idx_scope (scope_type, scope_id),
  INDEX idx_period (period_start, period_end)
);
```

**Budget Enforcement**:
1. **Pre-call check**: Verify budget available
2. **Soft limit**: Alert at 80% (continue execution)
3. **Hard limit**: Block at 100% (return error)
4. **Rollover**: Unused budget can rollover (configurable)

**Budget Hierarchy**:
```
Global Budget (e.g., $10,000/month)
  ├─ Tenant Budget (e.g., $2,000/month per tenant)
  │   ├─ User Budget (e.g., $500/month per user)
  │   └─ Skill Budget (e.g., $1,000/month for PR Review)
```

### 3.3 Prompt Management

#### 3.3.1 Prompt Versioning

**Prompt Template Structure**:
```sql
CREATE TABLE prompt_templates (
  template_id         VARCHAR(64) NOT NULL,
  version             VARCHAR(32) NOT NULL,
  content             TEXT NOT NULL,
  variables           JSON,  -- {repo_url, pr_number, ...}
  model_constraints   JSON,  -- {min_tokens: 2000, max_tokens: 4000}
  effective_at        TIMESTAMP,
  is_active           BOOLEAN DEFAULT TRUE,
  created_by          VARCHAR(255),
  created_at          TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
  metadata            JSON,  -- {description, tags, performance_metrics}
  
  PRIMARY KEY (template_id, version),
  INDEX idx_active (template_id, is_active, effective_at)
);
```

**Version Selection**:
1. **Latest active**: Default behavior
2. **Pinned version**: Skill specifies version
3. **A/B testing**: Random selection from test variants
4. **Rollback**: Revert to previous version on failure

#### 3.3.2 Prompt Composition

**Dynamic Prompt Assembly**:
```python
def assemble_prompt(
    template_id: str,
    variables: dict,
    context: dict,  # {repo_context, user_context, session_history}
) -> list[LLMMessage]:
    """Assemble prompt from template + context.
    
    1. Load template from MatrixOne
    2. Inject variables
    3. Add context (repo, user, history)
    4. Apply token budget constraints
    5. Return messages
    """
```

**Context Injection**:
- **Repo context**: Current repo, permissions, metadata
- **User context**: User preferences, history, expertise level
- **Session history**: Recent conversation (sliding window)
- **Skill context**: Skill-specific data (e.g., PR diff, CI logs)

**Token Budget Allocation**:
```
Total Budget: 8000 tokens
├─ System prompt: 500 tokens (fixed)
├─ Skill instructions: 1000 tokens (fixed)
├─ Context: 3000 tokens (dynamic)
│   ├─ Repo context: 500 tokens
│   ├─ User context: 500 tokens
│   └─ Session history: 2000 tokens (truncate if needed)
└─ Task input: 3500 tokens (user query + data)
```

### 3.4 Call Logging and Audit

**LLM Call Log**:
```sql
CREATE TABLE llm_call_logs (
  log_id              VARCHAR(64) PRIMARY KEY,
  event_id            VARCHAR(64) NOT NULL,  -- Link to conversation_events
  user_id             VARCHAR(255) NOT NULL,
  session_id          VARCHAR(64),
  skill_id            VARCHAR(64),
  provider            VARCHAR(50) NOT NULL,
  model               VARCHAR(100) NOT NULL,
  prompt_template_id  VARCHAR(64),
  prompt_version      VARCHAR(32),
  tokens_prompt       INT NOT NULL,
  tokens_completion   INT NOT NULL,
  tokens_total        INT NOT NULL,
  cost_usd            DECIMAL(10, 6) NOT NULL,
  latency_ms          INT NOT NULL,
  status              VARCHAR(50) NOT NULL,  -- 'success' | 'failed' | 'cached'
  error_message       TEXT,
  cache_hit           BOOLEAN DEFAULT FALSE,
  created_at          TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
  metadata            JSON,  -- {temperature, max_tokens, stop_sequences}
  
  INDEX idx_event_id (event_id),
  INDEX idx_user_id (user_id, created_at),
  INDEX idx_session_id (session_id),
  INDEX idx_skill_id (skill_id, created_at),
  INDEX idx_provider (provider, created_at),
  INDEX idx_status (status)
);
```

**Audit Trail** (5W + How):
- **Who**: `user_id`, `session_id`
- **What**: `skill_id`, `prompt_template_id`, `prompt_version`
- **When**: `created_at`
- **Where**: `provider`, `model`
- **Why**: `event_id` → `conversation_events` → `causal_chain_id`
- **How**: `tokens_*`, `cost_usd`, `latency_ms`, `metadata`

### 3.5 Caching Strategy

**Semantic Cache**:
```sql
CREATE TABLE llm_cache (
  cache_id            VARCHAR(64) PRIMARY KEY,
  cache_key           VARCHAR(64) NOT NULL,  -- Hash of (prompt + model + params)
  prompt_hash         VARCHAR(64) NOT NULL,
  model               VARCHAR(100) NOT NULL,
  response_content    TEXT NOT NULL,
  tokens_prompt       INT NOT NULL,
  tokens_completion   INT NOT NULL,
  cost_usd            DECIMAL(10, 6) NOT NULL,
  hit_count           INT DEFAULT 0,
  last_hit_at         TIMESTAMP,
  created_at          TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
  expires_at          TIMESTAMP,
  
  UNIQUE KEY idx_cache_key (cache_key),
  INDEX idx_prompt_hash (prompt_hash),
  INDEX idx_expires (expires_at)
);
```

**Cache Strategy**:
1. **Exact match**: Hash(prompt + model + params) → cache_key
2. **Semantic similarity**: Embedding-based (future enhancement)
3. **TTL**: Configurable expiration (default: 24 hours)
4. **Invalidation**: Manual or automatic (on prompt version change)

**Cache Hit Savings**:
- Log cache hits to `llm_call_logs` with `cache_hit=TRUE`
- Track cost savings: `cost_saved = original_cost * hit_count`
- Report cache hit rate per skill/user

### 3.6 Quality Assurance

#### 3.6.1 A/B Testing

**A/B Test Configuration**:
```sql
CREATE TABLE llm_ab_tests (
  test_id             VARCHAR(64) PRIMARY KEY,
  test_name           VARCHAR(255) NOT NULL,
  skill_id            VARCHAR(64),
  variant_a_template  VARCHAR(64) NOT NULL,  -- template_id@version
  variant_b_template  VARCHAR(64) NOT NULL,
  traffic_split       DECIMAL(3, 2) DEFAULT 0.5,  -- 50/50 split
  start_date          TIMESTAMP NOT NULL,
  end_date            TIMESTAMP,
  status              VARCHAR(50) DEFAULT 'active',  -- 'active' | 'completed' | 'paused'
  success_metric      VARCHAR(100),  -- 'user_rating' | 'task_completion' | 'cost_efficiency'
  
  INDEX idx_skill (skill_id, status),
  INDEX idx_dates (start_date, end_date)
);
```

**A/B Test Execution**:
1. **Random assignment**: User → variant (deterministic by user_id hash)
2. **Metric collection**: Track success metric per variant
3. **Statistical analysis**: Chi-square test for significance
4. **Winner selection**: Promote winning variant to production

#### 3.6.2 Feedback Loop

**Feedback Collection**:
```sql
CREATE TABLE llm_feedback (
  feedback_id         VARCHAR(64) PRIMARY KEY,
  log_id              VARCHAR(64) NOT NULL,  -- Link to llm_call_logs
  event_id            VARCHAR(64) NOT NULL,
  user_id             VARCHAR(255) NOT NULL,
  rating              INT,  -- 1-5 stars
  feedback_type       VARCHAR(50),  -- 'thumbs_up' | 'thumbs_down' | 'report'
  feedback_text       TEXT,
  created_at          TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
  
  INDEX idx_log_id (log_id),
  INDEX idx_event_id (event_id),
  INDEX idx_rating (rating)
);
```

**Feedback-Driven Improvement**:
1. **Low-rated calls**: Flag for prompt review
2. **High-rated calls**: Use for fine-tuning dataset
3. **Pattern detection**: Identify common failure modes
4. **Automatic retry**: Re-run with different prompt on low rating

### 3.7 Hallucination Firewall

The Hallucination Firewall verifies LLM responses against versioned data before delivery.

Key design:
- Extract verifiable claims from LLM response (numeric claims, historical references)
- Verify each claim against the same data snapshot the LLM saw (using context_snapshot's snapshot reference)
- Annotate response with verification status (verified/contradicted/unverifiable)
- Block delivery if contradictions found; return corrected response
- Log verification results for quality tracking

This leverages Git for Data's time-travel queries to ensure verification operates on the exact same data state as generation.

### 3.8 Cost-Aware Branching

Before executing any LLM call, predict cost from historical data:
- Query llm_call_logs for same skill + similar parameters in last 30 days
- Use max historical cost * 1.2 as conservative estimate
- If estimated cost > remaining budget: block and suggest cheaper alternative
- Cheaper alternatives found by querying same-category skills with lower avg cost
- All predictions logged for accuracy tracking

This transforms budget control from reactive (block at 100%) to predictive (warn before spending).

## 4. Advanced Features

### 4.1 Multi-Model Routing

**Routing Strategy**:
```python
def route_to_model(
    task_type: str,
    complexity: str,
    budget: float,
) -> tuple[str, str]:  # (provider, model)
    """Route task to optimal model.
    
    Rules:
    - Simple tasks → GPT-3.5-turbo (cheap, fast)
    - Complex reasoning → GPT-4 (expensive, accurate)
    - Code generation → Claude-3 (good at code)
    - Ultra-fast → Groq Llama3 (lowest latency)
    - Privacy-sensitive → Self-hosted (no data leaves)
    """
```

**Task Classification**:
- **Simple**: Issue triage, label suggestion
- **Medium**: PR review, code explanation
- **Complex**: Architecture design, debugging

### 4.2 Streaming Responses

**Streaming API**:
```python
async def chat_stream(
    messages: list[LLMMessage],
    event_id: str,
    user_id: str,
) -> AsyncIterator[str]:
    """Stream LLM response token-by-token.
    
    - Lower perceived latency
    - Better UX for long responses
    - Still logs full call to MatrixOne
    """
```

**Use Cases**:
- Interactive CLI
- Web UI
- Real-time PR review comments

### 4.3 Function Calling

**Function Definition**:
```python
functions = [
    {
        "name": "search_code",
        "description": "Search for code in repository",
        "parameters": {
            "type": "object",
            "properties": {
                "query": {"type": "string"},
                "repo_url": {"type": "string"},
            },
            "required": ["query", "repo_url"],
        },
    }
]
```

**Execution Flow**:
1. LLM returns function call
2. Agent executes function
3. Result fed back to LLM
4. LLM generates final response
5. All steps logged to MatrixOne

### 4.4 Retry and Fallback

**Retry Strategy**:
```python
@retry(
    max_attempts=3,
    backoff=exponential_backoff(base=2, max_delay=60),
    retry_on=[RateLimitError, TimeoutError],
)
def call_llm(...):
    """Call LLM with automatic retry."""
```

**Fallback Chain**:
```
Primary: OpenAI GPT-4
  ↓ (on failure)
Secondary: Groq Llama3-70B
  ↓ (on failure)
Tertiary: Self-hosted Llama3
  ↓ (on failure)
Return error
```

## 5. Reproducibility

### 5.1 Replay Capability

**Replay Requirements**:
1. **Prompt**: `prompt_template_id@version` from `llm_call_logs`
2. **Model**: `provider` + `model` from `llm_call_logs`
3. **Parameters**: `metadata` (temperature, max_tokens, etc.)
4. **Context**: `event_id` → `conversation_events` → full context
5. **Pricing**: Historical pricing from `llm_pricing` using `created_at` timestamp

**Replay API**:
```python
def replay_llm_call(log_id: str) -> LLMResponse:
    """Replay LLM call from log.
    
    1. Load call log
    2. Reconstruct prompt from template + context
    3. Call same model with same parameters
    4. Calculate cost using HISTORICAL pricing (call timestamp)
    5. Compare response (for validation)
    6. Log replay as new call
    """
    # Load original call
    log = get_call_log(log_id)
    
    # Get historical pricing (critical for accuracy)
    cost = calculate_cost(
        provider=log.provider,
        model=log.model,
        tokens_prompt=log.tokens_prompt,
        tokens_completion=log.tokens_completion,
        call_timestamp=log.created_at,  # ✅ Use original timestamp
    )
    
    # Verify cost matches original (within rounding)
    assert abs(cost - log.cost_usd) < 0.000001, "Cost mismatch!"
    
    return response
```

**Historical Pricing Query**:
```sql
-- Get pricing valid at specific timestamp
SELECT price_per_1k_prompt, price_per_1k_completion
FROM llm_pricing
WHERE provider = 'openai' 
  AND model = 'gpt-4'
  AND effective_from <= '2026-01-15 10:30:00'  -- Call timestamp
  AND (effective_to IS NULL OR effective_to > '2026-01-15 10:30:00')
ORDER BY effective_from DESC
LIMIT 1;
```

**Replay Use Cases**:
- **Debugging**: "Why did the agent say X on 2026-02-10?"
- **Audit**: "What was the prompt that led to this decision?"
- **Testing**: "Does the new prompt produce better results?"
- **Cost analysis**: "How much did this conversation cost?" (with accurate historical pricing)
- **Compliance**: "Prove the cost calculation was correct at that time"

### 5.2 Deterministic Responses

**Challenges**:
- LLMs are non-deterministic (even with temperature=0)
- Exact replay may produce different responses

**Solutions**:
1. **Store response**: Save original response in `llm_call_logs`
2. **Semantic comparison**: Compare meaning, not exact text
3. **Acceptance criteria**: Define "close enough" threshold
4. **Seed control**: Use seed parameter (if supported by provider)

### 5.3 Snapshot-Consistent Verification

When verifying LLM outputs for hallucination, the verification query must use the same data snapshot that was used to build the LLM's context. This is achieved by:
1. Recording the snapshot name in context_snapshot at generation time
2. Using {SNAPSHOT = 'name'} syntax for all verification queries
3. This ensures verification and generation see identical data, eliminating false positives from data drift

## 6. Performance Optimization

### 6.1 Latency Targets

| Percentile | Target | Provider |
|------------|--------|----------|
| P50 | < 500ms | Groq |
| P95 | < 2s | OpenAI |
| P99 | < 5s | Any |

### 6.2 Optimization Strategies

**1. Caching**:
- Semantic cache for repeated queries
- Cache hit rate target: > 30%

**2. Prompt Optimization**:
- Shorter prompts = lower cost + faster response
- Remove redundant context
- Use prompt compression techniques

**3. Model Selection**:
- Use smaller models for simple tasks
- GPT-3.5-turbo is 10x cheaper than GPT-4

**4. Parallel Calls**:
- Multiple independent LLM calls in parallel
- Example: PR review (summary + security + style in parallel)

**5. Streaming**:
- Stream responses for better perceived latency
- User sees first token in < 500ms

## 7. Security and Privacy

### 7.1 Data Privacy

**Sensitive Data Handling**:
- **PII masking**: Mask emails, phone numbers, addresses before sending to LLM
- **Secret detection**: Never send API keys, passwords to LLM
- **Data residency**: Use self-hosted models for privacy-sensitive data

**Provider Data Policies**:
- **OpenAI**: Data not used for training (API)
- **Anthropic**: Data not used for training
- **Self-hosted**: Data never leaves infrastructure

### 7.2 Prompt Injection Prevention

**Defenses**:
1. **Input sanitization**: Remove suspicious patterns
2. **Prompt structure**: Clear separation of instructions vs. user input
3. **Output validation**: Verify response format
4. **Rate limiting**: Prevent abuse

**Example**:
```python
# Bad: User input directly in prompt
prompt = f"Summarize this PR: {user_input}"

# Good: Structured prompt
messages = [
    {"role": "system", "content": "You are a PR reviewer."},
    {"role": "user", "content": f"PR content:\n{sanitize(user_input)}"},
]
```

### 7.3 Cost Attack Prevention

**Attack Vectors**:
- Malicious user sends extremely long inputs
- Repeated calls to expensive models
- Prompt injection to generate long responses

**Defenses**:
1. **Input length limits**: Max 10K tokens per call
2. **Budget limits**: Per-user daily/monthly caps
3. **Rate limiting**: Max 100 calls/hour per user
4. **Anomaly detection**: Flag unusual usage patterns

## 8. Monitoring and Observability

### 8.1 Metrics

**Cost Metrics**:
- Total spend (daily/weekly/monthly)
- Cost per user, per skill, per provider
- Budget utilization (% of limit used)
- Cost per conversation, per event

**Performance Metrics**:
- Latency (P50, P95, P99)
- Token usage (prompt, completion, total)
- Cache hit rate
- Error rate

**Quality Metrics**:
- User rating (average, distribution)
- Task completion rate
- Retry rate
- Feedback sentiment

### 8.2 Dashboards

**Cost Dashboard**:
- Real-time spend tracking
- Budget vs. actual
- Cost breakdown (by provider, model, skill, user)
- Trend analysis

**Performance Dashboard**:
- Latency distribution
- Token usage trends
- Cache performance
- Error rates

**Quality Dashboard**:
- User ratings over time
- A/B test results
- Feedback analysis
- Prompt performance comparison

### 8.3 Alerts

**Cost Alerts**:
- Budget threshold reached (80%, 90%, 100%)
- Unusual spend spike (> 2x daily average)
- Expensive model overuse

**Performance Alerts**:
- High latency (P95 > 5s)
- High error rate (> 5%)
- Low cache hit rate (< 20%)

**Quality Alerts**:
- Low user ratings (< 3.0 average)
- High retry rate (> 10%)
- Negative feedback spike

## 9. Implementation Phases

### Phase 1: Foundation (Current) ✅
- ✅ LLMClient with provider abstraction
- ✅ Cost calculation
- ✅ Call logging
- ✅ Config management

### Phase 2: Cost Control
- Budget management (per-user, per-tenant)
- Budget enforcement (soft/hard limits)
- Cost alerts
- Cost dashboard

### Phase 3: Quality
- Prompt versioning
- A/B testing framework
- Feedback collection
- Quality metrics

### Phase 4: Performance
- Semantic caching
- Multi-model routing
- Streaming responses
- Parallel calls

### Phase 5: Advanced
- Function calling
- Self-hosted models
- Fine-tuning pipeline
- Prompt optimization tools

## 10. Success Criteria

### 10.1 Functional Requirements
- ✅ Support 3+ LLM providers
- ✅ Cost tracking for every call
- ✅ Config stored in MatrixOne
- ✅ Call logs linked to events
- ⏳ Budget enforcement
- ⏳ Prompt versioning
- ⏳ A/B testing

### 10.2 Performance Requirements
- P95 latency < 2s
- Cache hit rate > 30%
- Error rate < 1%
- Uptime > 99.9%

### 10.3 Cost Requirements
- Cost per conversation < $0.10 (average)
- Budget overrun rate < 1%
- Cost visibility: 100% of spend tracked

### 10.4 Quality Requirements
- User rating > 4.0 (out of 5)
- Task completion rate > 90%
- Prompt improvement cycle < 1 week
- Hallucination detection rate > 80% for verifiable claims
- Cost prediction accuracy within 20% of actual
- Zero budget overruns with predictive cost control

## 11. Comparison with Industry Standards

### vs. LangChain
- ❌ They: No built-in cost tracking
- ✅ Us: Every call logged with cost in MatrixOne

### vs. LlamaIndex
- ❌ They: Limited provider support
- ✅ Us: 4+ providers with unified interface

### vs. OpenAI API Direct
- ❌ They: Vendor lock-in
- ✅ Us: Provider abstraction, easy to switch

### vs. Anthropic API Direct
- ❌ They: No budget control
- ✅ Us: Per-user, per-tenant budget limits

## 12. Key Insights

**"LLM Calls as First-Class Events"**:
- Every LLM call is an event in MatrixOne
- Full context available for replay
- Cost, latency, quality all tracked
- Enables "10 years later, reproduce today's LLM decision"

**"Cost Transparency = Trust"**:
- Users see exactly what they're spending
- Admins can set and enforce budgets
- No surprise bills

**"Provider Independence = Freedom"**:
- Switch providers without code changes
- Route to optimal provider per task
- Negotiate better pricing with leverage

**"Quality Through Feedback"**:
- User feedback drives prompt improvement
- A/B testing validates changes
- Continuous improvement loop

---

**Document Status**: Production-Ready  
**Next Review**: After Phase 2 implementation  
**Owner**: mo-dev-agent team
