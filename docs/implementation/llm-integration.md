# LLM Integration

## Architecture

```
Skills / ChatLoop / Planner
        │
        ▼
LLMClient (unified interface)
  chat() / chat_stream() / chat_with_tools() / chat_with_tools_stream()
  + task_hint for routing
  + pre-call budget check
        │
        ▼
ModelRouter (pluggable strategy)
  FallbackChain │ TaskBased (MoE) │ CostOptimized
  ModelRegistry: configs, pricing, fallback_to
        │
        ▼
RateLimiter + CircuitBreaker
  TokenBucket (RPM + TPM) │ CircuitBreaker (per-provider)
        │
        ▼
Provider Adapters
  OpenAI │ Anthropic │ Groq │ Self-hosted (OpenAI-compatible)
        │
        ▼
MatrixOne (llm_call_logs, conversation_events)
```

Module: `core/llm/`

## Providers

| Provider | Models | Notes |
|---|---|---|
| OpenAI | GPT-4o, GPT-4o-mini, GPT-4-turbo, GPT-3.5-turbo | Standard OpenAI API |
| Anthropic | Claude-3.5-Sonnet, Claude-3-Haiku | System message separation, tool format conversion |
| Groq | Llama3-70B, Mixtral-8x7B | Ultra-fast inference |
| Self-hosted | Any OpenAI-compatible | Via `base_url` (Ollama, vLLM, TGI) |

Each provider implements `BaseProvider` with 4 methods: `complete`, `complete_stream`, `complete_with_tools`, `complete_with_tools_stream`. Provider-specific API differences (Anthropic's system message format, tool schema format) are handled inside the adapter.

## Routing Strategies

**FallbackChain** (default): Try primary model → on failure, try fallback_to chain.

**TaskBased**: Route by `task_hint` — simple tasks to cheap models, complex tasks to expensive models. See [agents-and-orchestration.md §7](../design/agents-and-orchestration.md) for the design.

**CostOptimized**: Select model with best quality/cost ratio from historical data.

Strategy is pluggable — set per agent or per request.

## Cost Tracking

Every LLM call logs:
- `input_tokens`, `output_tokens`, `total_cost`
- `model`, `provider`, `latency_ms`
- `trace_id` (links to causal chain)

Pre-call budget check: estimate cost from token count × model pricing. Reject if budget exceeded.

```python
# Budget check before call
estimated_cost = estimate_cost(messages, model)
if budget_remaining < estimated_cost:
    raise BudgetExceededError(f"Estimated ${estimated_cost}, budget ${budget_remaining}")
```

## Rate Limiting and Circuit Breaker

**Rate limiter**: Token bucket per provider (RPM + TPM limits). Queues requests when limit approached.

**Circuit breaker**: Per-provider. Opens after N consecutive failures. Half-open after cooldown. Prevents cascading failures when a provider is down.

```
CLOSED → (N failures) → OPEN → (cooldown) → HALF_OPEN → (success) → CLOSED
                                            → (failure) → OPEN
```

## Resilience

Provider failure → circuit breaker opens → router falls back to next model in chain → logs `provider_failover` event. See [agents-and-orchestration.md §10](../design/agents-and-orchestration.md) for cross-model consistency design.

## Scope-Based Configuration

Model availability and API keys are configured per scope (global → account → user). See [scope-configuration.md](scope-configuration.md).

```bash
# Admin: add model globally
mo-admin model add gpt-4 openai --scope global

# Admin: add model for specific account
mo-admin model add claude-3 anthropic --scope account --scope-id acme
```
