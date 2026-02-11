"""
LLM Integration Example

Demonstrates:
1. Config loading from MatrixOne
2. LLM call with cost tracking
3. Call log retrieval
"""

from core.llm import LLMClient
from sdk import Database

print("=" * 80)
print("LLM Integration Example")
print("=" * 80)

db = Database()
client = LLMClient(db=db)

# ============================================================================
# 1. Config Management
# ============================================================================
print("\n1. LLM Configuration (loaded from MatrixOne)")
print("-" * 80)
print(f"Provider: {client.config['provider']}")
print(f"Model: {client.config['model']}")
print(f"Temperature: {client.config['temperature']}")
print(f"Max Tokens: {client.config.get('max_tokens', 'N/A')}")

# ============================================================================
# 2. Cost Calculation
# ============================================================================
print("\n2. Cost Calculation (without API call)")
print("-" * 80)

from core.llm.models import LLMProvider

# Example: GPT-4 cost
cost_gpt4 = client._calculate_cost(
    provider=LLMProvider.OPENAI,
    model="gpt-4",
    tokens_prompt=1000,
    tokens_completion=500,
)
print(f"GPT-4 (1000 prompt + 500 completion tokens): ${cost_gpt4:.6f}")

# Example: GPT-3.5 cost
cost_gpt35 = client._calculate_cost(
    provider=LLMProvider.OPENAI,
    model="gpt-3.5-turbo",
    tokens_prompt=1000,
    tokens_completion=500,
)
print(f"GPT-3.5-turbo (1000 prompt + 500 completion tokens): ${cost_gpt35:.6f}")

# Example: Groq cost
cost_groq = client._calculate_cost(
    provider=LLMProvider.GROQ,
    model="llama3-70b",
    tokens_prompt=1000,
    tokens_completion=500,
)
print(f"Groq Llama3-70B (1000 prompt + 500 completion tokens): ${cost_groq:.6f}")

# ============================================================================
# 3. Call Logs
# ============================================================================
print("\n3. LLM Call Logs (from MatrixOne)")
print("-" * 80)

# Get recent logs
logs = client.get_call_logs()
if logs:
    print(f"Total logs: {len(logs)}")
    print("\nMost recent call:")
    log = logs[0]
    print(f"  - Provider: {log.provider.value}")
    print(f"  - Model: {log.model}")
    print(f"  - Tokens: {log.tokens_total}")
    print(f"  - Cost: ${log.cost_usd:.6f}")
    print(f"  - Latency: {log.latency_ms}ms")
    print(f"  - Status: {log.status}")
else:
    print("No call logs found")

# ============================================================================
# 4. Integration with Event System
# ============================================================================
print("\n4. Integration with Event System")
print("-" * 80)
print("""
LLM calls are logged to MatrixOne with:
- event_id: Links to conversation_events (full context)
- user_id: User who triggered the call
- tokens: Prompt, completion, total
- cost_usd: Estimated cost
- latency_ms: Response time
- status: success | failed

This enables:
- Cost tracking per user/session
- Performance monitoring
- Audit trail (who called what, when)
- Replay capability (reproduce LLM calls)
""")

# ============================================================================
# 5. Provider Swapping
# ============================================================================
print("\n5. Provider Swapping")
print("-" * 80)
print("""
To switch providers, update config in MatrixOne:

UPDATE configs
SET value = '{"provider": "groq", "model": "llama3-70b", ...}'
WHERE key_name = 'llm_config';

Supported providers:
- openai (GPT-4, GPT-3.5-turbo)
- groq (Llama3-70B, Mixtral-8x7B)
- anthropic (Claude) - coming soon
- self_hosted - coming soon

No code changes needed - just update config!
""")

print("=" * 80)
print("LLM Integration Summary")
print("=" * 80)
print("""
✅ Key Features:

1. Config in MatrixOne
   - All LLM settings stored in configs table
   - No hardcoded values
   - Easy to update

2. Cost Tracking
   - Every call logged with cost
   - Per-user cost analysis
   - Budget monitoring

3. Provider Abstraction
   - Swappable providers (OpenAI, Groq, etc.)
   - Unified interface
   - No vendor lock-in

4. Audit Trail
   - All calls linked to events
   - Full context available
   - Replay capability

5. Performance Monitoring
   - Latency tracking
   - Token usage stats
   - Error logging

🌟 "Everything in MatrixOne" - LLM calls are first-class events!
""")
print("=" * 80)
