"""Tests for LLM routing, budget, retry, and config validation."""

import pytest
from unittest.mock import MagicMock, patch

from core.llm.client import BudgetExceededError, LLMClient
from core.llm.router import (
    ModelConfig,
    ModelRegistry,
    TaskBasedStrategy,
    LLMProvider,
)
from core.llm.providers import _should_retry, RETRY_BASE_DELAY, MAX_RETRIES, BaseProvider


# ── TaskBasedStrategy tie-breaking ──────────────────────────────


class TestTaskBasedStrategy:
    def test_tag_conflict_tiebreak_by_cost(self):
        """Models with same tag score should be ordered by cost (cheaper first)."""
        registry = ModelRegistry()
        expensive = ModelConfig(
            model_name="expensive",
            provider=LLMProvider.OPENAI,
            pricing={"prompt": 0.03, "completion": 0.06},
            tags=["code", "reasoning"],
        )
        cheap = ModelConfig(
            model_name="cheap",
            provider=LLMProvider.OPENAI,
            pricing={"prompt": 0.001, "completion": 0.002},
            tags=["code", "reasoning"],
        )
        registry.register(expensive)
        registry.register(cheap)

        strategy = TaskBasedStrategy()
        result = strategy.select("any", registry, task_hint="code")

        # Both match 2 tags — cheap should come first
        assert result[0].model_name == "cheap"
        assert result[1].model_name == "expensive"

    def test_higher_tag_score_wins_over_cost(self):
        """Model with more tag matches should rank higher even if more expensive."""
        registry = ModelRegistry()
        cheap_one_tag = ModelConfig(
            model_name="cheap",
            provider=LLMProvider.OPENAI,
            pricing={"prompt": 0.001, "completion": 0.001},
            tags=["code"],
        )
        expensive_two_tags = ModelConfig(
            model_name="expensive",
            provider=LLMProvider.OPENAI,
            pricing={"prompt": 0.03, "completion": 0.06},
            tags=["code", "reasoning"],
        )
        registry.register(cheap_one_tag)
        registry.register(expensive_two_tags)

        strategy = TaskBasedStrategy()
        result = strategy.select("any", registry, task_hint="code")

        assert result[0].model_name == "expensive"  # 2 tags > 1 tag


# ── Budget estimation ───────────────────────────────────────────


class TestBudgetCheck:
    def _make_client(self, budget_usd=1.0):
        with patch.object(LLMClient, "__init__", lambda self, **kw: None):
            client = LLMClient.__new__(LLMClient)
            client.config = {"budget_usd": budget_usd}
            client._total_spend_usd = 0.0
            client.router = MagicMock()
            client.router.estimate_cost.return_value = 0.01
            return client

    def test_estimates_from_messages(self):
        """Budget check should estimate tokens from actual message content."""
        client = self._make_client(budget_usd=1.0)
        messages = [{"role": "user", "content": "x" * 4000}]  # ~1000 tokens

        client._check_budget("gpt-4o", messages)

        # Should call estimate_cost with ~1000 tokens (4000 chars / 4)
        called_tokens = client.router.estimate_cost.call_args[0][1]
        assert called_tokens == 1000

    def test_fallback_to_default_without_messages(self):
        """Budget check without messages should use default 1000."""
        client = self._make_client(budget_usd=1.0)

        client._check_budget("gpt-4o")

        called_tokens = client.router.estimate_cost.call_args[0][1]
        assert called_tokens == 1000

    def test_minimum_token_estimate(self):
        """Short messages should still estimate at least 200 tokens."""
        client = self._make_client(budget_usd=1.0)
        messages = [{"role": "user", "content": "hi"}]

        client._check_budget("gpt-4o", messages)

        called_tokens = client.router.estimate_cost.call_args[0][1]
        assert called_tokens == 200


# ── Non-retryable errors in _dispatch ───────────────────────────


class TestDispatchNonRetryable:
    def _make_client_with_chain(self):
        with patch.object(LLMClient, "__init__", lambda self, **kw: None):
            client = LLMClient.__new__(LLMClient)
            client.config = {"budget_usd": 0, "provider": "openai"}
            client._total_spend_usd = 0.0
            client.router = MagicMock()
            client.rate_limiter = MagicMock()
            breaker = MagicMock()
            breaker.allow_request.return_value = True
            client.rate_limiter.get_breaker.return_value = breaker
            client.rate_limiter.wait_and_acquire.return_value = True

            model_cfg = ModelConfig(model_name="gpt-4o", provider=LLMProvider.OPENAI)
            client._resolve_chain = MagicMock(return_value=[model_cfg, model_cfg])

            provider = MagicMock()
            client._providers = {"openai": provider}
            client._get_provider = MagicMock(return_value=provider)
            return client, provider

    def test_budget_exceeded_not_retried(self):
        """BudgetExceededError should propagate immediately, not try next model."""
        client, provider = self._make_client_with_chain()
        provider.complete.side_effect = BudgetExceededError("over budget")

        with pytest.raises(BudgetExceededError):
            client._dispatch("gpt-4o", "complete")

        # Should only be called once — not retried on second model
        assert provider.complete.call_count == 1

    def test_permission_error_not_retried(self):
        """PermissionError should propagate immediately, not try next model."""
        client, provider = self._make_client_with_chain()
        provider.complete.side_effect = PermissionError("no access")

        with pytest.raises(PermissionError):
            client._dispatch("gpt-4o", "complete")

        assert provider.complete.call_count == 1

    def test_regular_error_tries_next_model(self):
        """Regular errors should fall through to next model in chain."""
        client, provider = self._make_client_with_chain()
        provider.complete.side_effect = [RuntimeError("fail"), (MagicMock(), None)]

        # Should succeed on second attempt
        result = client._dispatch("gpt-4o", "complete")
        assert provider.complete.call_count == 2


# ── Config validation ───────────────────────────────────────────


class TestConfigValidation:
    def _make_client_for_validation(self, config):
        with patch.object(LLMClient, "__init__", lambda self, **kw: None):
            client = LLMClient.__new__(LLMClient)
            client.config = config
            return client

    def test_negative_budget_rejected(self):
        client = self._make_client_for_validation({"budget_usd": -1})
        with pytest.raises(ValueError, match="budget_usd"):
            client._validate_config()

    def test_invalid_temperature_rejected(self):
        client = self._make_client_for_validation({"budget_usd": 0, "temperature": 5.0})
        with pytest.raises(ValueError, match="temperature"):
            client._validate_config()

    def test_negative_max_tokens_rejected(self):
        client = self._make_client_for_validation({"budget_usd": 0, "max_tokens": -1})
        with pytest.raises(ValueError, match="max_tokens"):
            client._validate_config()

    def test_valid_config_passes(self):
        client = self._make_client_for_validation(
            {"budget_usd": 10, "temperature": 0.7, "max_tokens": 2000}
        )
        client._validate_config()  # Should not raise


# ── Retry jitter ────────────────────────────────────────────────


class TestResolveChain:
    def test_resolve_chain_checks_permission(self):
        """_resolve_chain must call _check_model_permission (streaming fix)."""
        with patch.object(LLMClient, "__init__", lambda self, **kw: None):
            client = LLMClient.__new__(LLMClient)
            client.config = {"provider": "openai"}
            client.router = MagicMock()
            client.router.route.return_value = [
                ModelConfig(model_name="gpt-4o", provider=LLMProvider.OPENAI)
            ]
            client._check_model_permission = MagicMock(
                side_effect=PermissionError("denied")
            )

            with pytest.raises(PermissionError, match="denied"):
                client._resolve_chain("gpt-4o")

            client._check_model_permission.assert_called_once_with("gpt-4o")


class TestRetryJitter:
    def test_retry_delay_has_jitter(self):
        """Retry delays should include random jitter."""
        delays = []

        class FakeProvider(BaseProvider):
            provider = LLMProvider.OPENAI

            def complete(self, **kw):
                raise type("RateLimitError", (Exception,), {})()

            def complete_stream(self, *a, **kw):
                yield {}

            def complete_with_tools(self, *a, **kw):
                return {}

            def complete_with_tools_stream(self, *a, **kw):
                yield {}

        p = FakeProvider()

        with patch("core.llm.providers.time.sleep", side_effect=delays.append):
            with pytest.raises(Exception):
                p._with_retry(p.complete)

        # Should have MAX_RETRIES - 1 delays
        assert len(delays) == MAX_RETRIES - 1
        # Each delay should be >= base * 2^attempt (the jitter adds, never subtracts)
        for i, d in enumerate(delays):
            base = RETRY_BASE_DELAY * (2**i)
            assert d >= base
            # Jitter adds up to 50% of base
            assert d <= base * 1.5


class TestChatStreamReasoningPassthrough:
    """Verify chat_stream yields reasoning chunks from provider."""

    @pytest.mark.asyncio
    async def test_reasoning_chunks_forwarded(self):
        """Provider reasoning chunks become {"type": "reasoning"} in chat_stream output."""
        from core.llm.client import LLMClient

        chunks = [
            {"type": "reasoning", "content": "thinking..."},
            {"type": "text", "content": "answer"},
            {"type": "usage", "prompt": 10, "completion": 5},
        ]

        class FakeProvider:
            provider = LLMProvider.OPENAI
            def complete_stream(self, *a, **kw):
                yield from chunks

        client = LLMClient.__new__(LLMClient)
        client.config = {"temperature": 0.7}
        client.router = MagicMock()
        client.router.resolve_model.return_value = MagicMock(
            model_name="test", provider=LLMProvider.OPENAI, enable_cache=False,
        )
        client.router.resolve_chain.return_value = [
            MagicMock(model_name="test", provider=LLMProvider.OPENAI, enable_cache=False),
        ]
        client.router.calculate_cost.return_value = 0.0
        client.rate_limiter = MagicMock()
        client.rate_limiter.get_breaker.return_value = MagicMock(allow_request=MagicMock(return_value=True))
        client.rate_limiter.wait_and_acquire = MagicMock()
        client._providers = {LLMProvider.OPENAI: FakeProvider()}
        client._spend_usd = 0.0
        client._budget_usd = None
        client._call_log = []
        client._resolve_model = MagicMock(return_value="test")
        client._check_budget = MagicMock()
        client._resolve_chain = MagicMock(return_value=[
            MagicMock(model_name="test", provider=LLMProvider.OPENAI, enable_cache=False),
        ])
        client._get_provider = MagicMock(return_value=FakeProvider())
        client._record_spend = MagicMock()
        client._log_call = MagicMock()

        collected = []
        async for item in client.chat_stream([], "user1"):
            collected.append(item)

        assert collected[0] == {"type": "reasoning", "content": "thinking..."}
        assert collected[1] == {"type": "text", "content": "answer"}
        assert len(collected) == 2  # usage is consumed, not yielded
