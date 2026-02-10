"""LLM client with provider abstraction."""

import json
import time
from datetime import UTC, datetime
from typing import Optional

from ulid import ULID

from core.llm.models import (
    LLMCallLog,
    LLMMessage,
    LLMProvider,
    LLMRequest,
    LLMResponse,
)
from sdk import Database


class LLMClient:
    """LLM client with swappable providers."""

    def __init__(self, db: Optional[Database] = None) -> None:
        self.db = db or Database()
        self._load_config()

    def _load_config(self) -> None:
        """Load LLM config from MatrixOne."""
        config = self.db.fetchone(
            "SELECT value FROM configs WHERE key_name = 'llm_config' LIMIT 1"
        )
        if config:
            self.config = json.loads(config["value"])
        else:
            # Default config
            self.config = {
                "provider": "openai",
                "model": "gpt-4",
                "temperature": 0.7,
                "max_tokens": 2000,
                "token_budget": 100000,
            }

    def chat(
        self,
        messages: list[LLMMessage],
        user_id: str,
        session_id: str = None,
        event_id: str = None,
        model: Optional[str] = None,
        temperature: Optional[float] = None,
        metadata: Optional[dict] = None,
    ) -> LLMResponse:
        """Send chat request to LLM."""
        start_time = time.time()

        # Generate event_id if not provided
        if not event_id:
            event_id = str(ULID())

        # Use config defaults if not specified
        model = model or self.config["model"]
        temperature = temperature or self.config["temperature"]

        request = LLMRequest(
            messages=messages,
            model=model,
            temperature=temperature,
            max_tokens=self.config.get("max_tokens"),
        )

        try:
            # Call provider
            provider = LLMProvider(self.config["provider"])
            response = self._call_provider(provider, request)

            # Calculate latency
            latency_ms = int((time.time() - start_time) * 1000)
            response.latency_ms = latency_ms

            # Log call
            self._log_call(
                event_id=event_id,
                user_id=user_id,
                provider=provider,
                response=response,
                status="success",
                metadata=metadata,
            )

            return response

        except Exception as e:
            latency_ms = int((time.time() - start_time) * 1000)
            self._log_call(
                event_id=event_id,
                user_id=user_id,
                provider=LLMProvider(self.config["provider"]),
                response=None,
                status="failed",
                error_message=str(e),
                latency_ms=latency_ms,
                metadata=metadata,
            )
            raise

    def _call_provider(
        self, provider: LLMProvider, request: LLMRequest
    ) -> LLMResponse:
        """Call specific LLM provider."""
        if provider == LLMProvider.OPENAI:
            return self._call_openai(request)
        elif provider == LLMProvider.GROQ:
            return self._call_groq(request)
        else:
            raise ValueError(f"Unsupported provider: {provider}")

    def _call_openai(self, request: LLMRequest) -> LLMResponse:
        """Call OpenAI API."""
        try:
            import openai
        except ImportError:
            raise ImportError("openai package not installed. Run: pip install openai")

        # Get API key from config
        api_key = self.config.get("openai_api_key") or self.db.fetchone(
            "SELECT value FROM configs WHERE key_name = 'openai_api_key' LIMIT 1"
        )
        if api_key and isinstance(api_key, dict):
            api_key = api_key["value"]

        client = openai.OpenAI(api_key=api_key)

        response = client.chat.completions.create(
            model=request.model,
            messages=[{"role": m.role, "content": m.content} for m in request.messages],
            temperature=request.temperature,
            max_tokens=request.max_tokens,
        )

        # Calculate cost (approximate)
        cost_usd = self._calculate_cost(
            provider=LLMProvider.OPENAI,
            model=request.model,
            tokens_prompt=response.usage.prompt_tokens,
            tokens_completion=response.usage.completion_tokens,
        )

        return LLMResponse(
            content=response.choices[0].message.content,
            model=response.model,
            provider=LLMProvider.OPENAI,
            tokens_prompt=response.usage.prompt_tokens,
            tokens_completion=response.usage.completion_tokens,
            tokens_total=response.usage.total_tokens,
            latency_ms=0,  # Will be set by caller
            cost_usd=cost_usd,
        )

    def _call_groq(self, request: LLMRequest) -> LLMResponse:
        """Call Groq API."""
        try:
            from groq import Groq
        except ImportError:
            raise ImportError("groq package not installed. Run: pip install groq")

        # Get API key from config
        api_key = self.config.get("groq_api_key") or self.db.fetchone(
            "SELECT value FROM configs WHERE key_name = 'groq_api_key' LIMIT 1"
        )
        if api_key and isinstance(api_key, dict):
            api_key = api_key["value"]

        client = Groq(api_key=api_key)

        response = client.chat.completions.create(
            model=request.model,
            messages=[{"role": m.role, "content": m.content} for m in request.messages],
            temperature=request.temperature,
            max_tokens=request.max_tokens,
        )

        # Calculate cost
        cost_usd = self._calculate_cost(
            provider=LLMProvider.GROQ,
            model=request.model,
            tokens_prompt=response.usage.prompt_tokens,
            tokens_completion=response.usage.completion_tokens,
        )

        return LLMResponse(
            content=response.choices[0].message.content,
            model=response.model,
            provider=LLMProvider.GROQ,
            tokens_prompt=response.usage.prompt_tokens,
            tokens_completion=response.usage.completion_tokens,
            tokens_total=response.usage.total_tokens,
            latency_ms=0,
            cost_usd=cost_usd,
        )

    def _calculate_cost(
        self,
        provider: LLMProvider,
        model: str,
        tokens_prompt: int,
        tokens_completion: int,
        call_timestamp: Optional[datetime] = None,
    ) -> float:
        """Calculate cost using historical pricing.
        
        Args:
            provider: LLM provider
            model: Model name
            tokens_prompt: Prompt tokens
            tokens_completion: Completion tokens
            call_timestamp: Timestamp for historical pricing (None = current)
        
        Returns:
            Cost in USD
        """
        if call_timestamp is None:
            call_timestamp = datetime.now(UTC)
        
        # Try to get pricing from database
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
        pricing = self.db.fetchone(
            query, (provider.value, model, call_timestamp, call_timestamp)
        )
        
        if pricing:
            # Use database pricing
            cost = (
                tokens_prompt * (float(pricing["price_per_1k_prompt"]) / 1000)
                + tokens_completion * (float(pricing["price_per_1k_completion"]) / 1000)
            )
            return round(cost, 6)
        
        # Fallback to hardcoded pricing (for backward compatibility)
        pricing_table = {
            LLMProvider.OPENAI: {
                "gpt-4": {"prompt": 0.03 / 1000, "completion": 0.06 / 1000},
                "gpt-4-turbo": {"prompt": 0.01 / 1000, "completion": 0.03 / 1000},
                "gpt-3.5-turbo": {"prompt": 0.0005 / 1000, "completion": 0.0015 / 1000},
            },
            LLMProvider.GROQ: {
                "llama3-70b": {"prompt": 0.0007 / 1000, "completion": 0.0008 / 1000},
                "mixtral-8x7b": {"prompt": 0.0003 / 1000, "completion": 0.0003 / 1000},
            },
        }

        model_pricing = pricing_table.get(provider, {}).get(
            model, {"prompt": 0, "completion": 0}
        )
        cost = (
            tokens_prompt * model_pricing["prompt"]
            + tokens_completion * model_pricing["completion"]
        )
        return round(cost, 6)

    def _log_call(
        self,
        event_id: str,
        user_id: str,
        provider: LLMProvider,
        response: Optional[LLMResponse],
        status: str,
        error_message: Optional[str] = None,
        latency_ms: int = 0,
        metadata: Optional[dict] = None,
    ) -> None:
        """Log LLM call to MatrixOne."""
        log_id = str(ULID())

        if response:
            query = """
                INSERT INTO llm_call_logs (
                    log_id, event_id, user_id, provider, model,
                    tokens_prompt, tokens_completion, tokens_total,
                    cost_usd, latency_ms, status, metadata, created_at
                ) VALUES (%s, %s, %s, %s, %s, %s, %s, %s, %s, %s, %s, %s, %s)
            """
            self.db.execute(
                query,
                (
                    log_id,
                    event_id,
                    user_id,
                    provider.value,
                    response.model,
                    response.tokens_prompt,
                    response.tokens_completion,
                    response.tokens_total,
                    response.cost_usd,
                    response.latency_ms,
                    status,
                    json.dumps(metadata) if metadata else None,
                    datetime.now(UTC),
                ),
            )
        else:
            query = """
                INSERT INTO llm_call_logs (
                    log_id, event_id, user_id, provider, model,
                    tokens_prompt, tokens_completion, tokens_total,
                    cost_usd, latency_ms, status, error_message, metadata, created_at
                ) VALUES (%s, %s, %s, %s, %s, %s, %s, %s, %s, %s, %s, %s, %s, %s)
            """
            self.db.execute(
                query,
                (
                    log_id,
                    event_id,
                    user_id,
                    provider.value,
                    "unknown",
                    0,
                    0,
                    0,
                    0.0,
                    latency_ms,
                    status,
                    error_message,
                    json.dumps(metadata) if metadata else None,
                    datetime.now(UTC),
                ),
            )

    def get_call_logs(
        self, event_id: Optional[str] = None, user_id: Optional[str] = None
    ) -> list[LLMCallLog]:
        """Get LLM call logs."""
        if event_id:
            query = "SELECT * FROM llm_call_logs WHERE event_id = %s ORDER BY created_at DESC"
            results = self.db.fetchall(query, (event_id,))
        elif user_id:
            query = "SELECT * FROM llm_call_logs WHERE user_id = %s ORDER BY created_at DESC LIMIT 100"
            results = self.db.fetchall(query, (user_id,))
        else:
            query = "SELECT * FROM llm_call_logs ORDER BY created_at DESC LIMIT 100"
            results = self.db.fetchall(query)

        return [self._to_log_model(r) for r in results]

    def _to_log_model(self, row: dict) -> LLMCallLog:
        """Convert database row to LLMCallLog model."""
        return LLMCallLog(
            log_id=row["log_id"],
            event_id=row["event_id"],
            user_id=row["user_id"],
            provider=LLMProvider(row["provider"]),
            model=row["model"],
            tokens_prompt=row["tokens_prompt"],
            tokens_completion=row["tokens_completion"],
            tokens_total=row["tokens_total"],
            cost_usd=float(row["cost_usd"]),
            latency_ms=row["latency_ms"],
            status=row["status"],
            error_message=row.get("error_message"),
            created_at=row["created_at"],
        )
