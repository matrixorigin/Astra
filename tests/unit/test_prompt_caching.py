"""Tests for prompt caching: cache-aware cost, provider cache headers, token extraction."""

from unittest.mock import MagicMock, patch
import sys

import pytest

from core.llm.models import LLMProvider, LLMResponse
from core.llm.router import ModelConfig, ModelRouter


# ---------------------------------------------------------------------------
# 1. calculate_cost — cache-aware pricing
# ---------------------------------------------------------------------------


class TestCacheAwareCost:
    @pytest.fixture
    def router(self):
        r = ModelRouter()
        r.registry.register(
            ModelConfig(
                model_name="claude-sonnet",
                provider=LLMProvider.ANTHROPIC,
                pricing={"prompt": 0.003, "completion": 0.015},
            )
        )
        r.registry.register(
            ModelConfig(
                model_name="gpt-4o",
                provider=LLMProvider.OPENAI,
                pricing={"prompt": 0.005, "completion": 0.015},
            )
        )
        return r

    def test_no_cache_tokens_unchanged(self, router):
        """Without cache tokens, cost is same as before."""
        cost = router.calculate_cost("claude-sonnet", 1000, 500)
        expected = 1000 * 0.003 / 1000 + 500 * 0.015 / 1000
        assert abs(cost - expected) < 1e-9

    def test_anthropic_cache_read_cheaper(self, router):
        """Anthropic cache_read = 0.1x base, should be cheaper."""
        cost_normal = router.calculate_cost("claude-sonnet", 1000, 100)
        cost_cached = router.calculate_cost("claude-sonnet", 1000, 100, cache_read_tokens=900)
        assert cost_cached < cost_normal

    def test_anthropic_cache_write_more_expensive(self, router):
        """Anthropic cache_creation = 1.25x base, first call costs more."""
        cost_normal = router.calculate_cost("claude-sonnet", 1000, 100)
        cost_write = router.calculate_cost("claude-sonnet", 1000, 100, cache_creation_tokens=900)
        assert cost_write > cost_normal

    def test_anthropic_cache_math(self, router):
        """Verify exact Anthropic cache pricing: read=0.1x, write=1.25x."""
        # 1000 prompt: 100 regular + 800 cache_read + 100 cache_creation
        cost = router.calculate_cost(
            "claude-sonnet", 1000, 0, cache_read_tokens=800, cache_creation_tokens=100
        )
        base = 0.003 / 1000
        expected = 100 * base + 800 * base * 0.1 + 100 * base * 1.25
        assert abs(cost - expected) < 1e-9

    def test_openai_cache_read_half_price(self, router):
        """OpenAI cache_read = 0.5x base."""
        cost = router.calculate_cost("gpt-4o", 1000, 0, cache_read_tokens=800)
        base = 0.005 / 1000
        expected = 200 * base + 800 * base * 0.5
        assert abs(cost - expected) < 1e-9

    def test_backward_compatible_signature(self, router):
        """Old callers without cache args still work."""
        cost = router.calculate_cost("claude-sonnet", 500, 200)
        assert cost > 0

    def test_custom_cache_pricing(self):
        """Explicit cache pricing overrides provider defaults."""
        router = ModelRouter()
        router.registry.register(
            ModelConfig(
                model_name="custom",
                provider=LLMProvider.ANTHROPIC,
                pricing={
                    "prompt": 0.01,
                    "completion": 0.03,
                    "cache_read": 0.002,
                    "cache_write": 0.015,
                },
            )
        )
        cost = router.calculate_cost(
            "custom", 1000, 0, cache_read_tokens=500, cache_creation_tokens=200
        )
        expected = 300 * 0.01 / 1000 + 500 * 0.002 / 1000 + 200 * 0.015 / 1000
        assert abs(cost - expected) < 1e-9


# ---------------------------------------------------------------------------
# 2. ModelConfig.enable_cache
# ---------------------------------------------------------------------------


class TestEnableCache:
    def test_default_enabled(self):
        cfg = ModelConfig(model_name="x", provider=LLMProvider.ANTHROPIC)
        assert cfg.enable_cache is True

    def test_can_disable(self):
        cfg = ModelConfig(model_name="x", provider=LLMProvider.ANTHROPIC, enable_cache=False)
        assert cfg.enable_cache is False


# ---------------------------------------------------------------------------
# 3. LLMResponse cache fields
# ---------------------------------------------------------------------------


class TestLLMResponseCacheFields:
    def test_defaults_zero(self):
        r = LLMResponse(
            content="",
            model="m",
            provider=LLMProvider.OPENAI,
            tokens_prompt=0,
            tokens_completion=0,
            tokens_total=0,
            latency_ms=0,
            cost_usd=0.0,
        )
        assert r.cache_read_tokens == 0
        assert r.cache_creation_tokens == 0

    def test_set_cache_fields(self):
        r = LLMResponse(
            content="",
            model="m",
            provider=LLMProvider.ANTHROPIC,
            tokens_prompt=100,
            tokens_completion=50,
            tokens_total=150,
            latency_ms=0,
            cost_usd=0.0,
            cache_read_tokens=80,
            cache_creation_tokens=10,
        )
        assert r.cache_read_tokens == 80
        assert r.cache_creation_tokens == 10


# ---------------------------------------------------------------------------
# 4. AnthropicProvider — cache_control headers & usage extraction
# ---------------------------------------------------------------------------


class TestAnthropicProviderCaching:
    @pytest.fixture
    def provider(self):
        mock_module = MagicMock()
        with patch.dict("sys.modules", {"anthropic": mock_module}):
            from core.llm.providers import AnthropicProvider

            p = AnthropicProvider(api_key="fake")
            return p

    def test_split_system_cache_enabled(self, provider):
        provider.cache_enabled = True
        msgs = [{"role": "system", "content": "Be helpful"}, {"role": "user", "content": "Hi"}]
        system, user_msgs = provider._split_system(msgs)
        # Should be list of content blocks with cache_control
        assert isinstance(system, list)
        assert system[0]["type"] == "text"
        assert system[0]["text"] == "Be helpful"
        assert system[0]["cache_control"] == {"type": "ephemeral"}
        assert len(user_msgs) == 1

    def test_split_system_cache_disabled(self, provider):
        provider.cache_enabled = False
        msgs = [{"role": "system", "content": "Be helpful"}, {"role": "user", "content": "Hi"}]
        system, user_msgs = provider._split_system(msgs)
        # Should be plain string
        assert isinstance(system, str)
        assert system == "Be helpful"

    def test_split_system_no_system_msg(self, provider):
        provider.cache_enabled = True
        msgs = [{"role": "user", "content": "Hi"}]
        system, user_msgs = provider._split_system(msgs)
        assert isinstance(system, list)
        assert "helpful" in system[0]["text"].lower()

    def test_convert_tools_with_cache_enabled(self, provider):
        provider.cache_enabled = True
        tools = [
            {
                "function": {
                    "name": "t1",
                    "description": "d1",
                    "parameters": {"type": "object", "properties": {}},
                }
            },
            {
                "function": {
                    "name": "t2",
                    "description": "d2",
                    "parameters": {"type": "object", "properties": {}},
                }
            },
        ]
        result = provider._convert_tools_with_cache(tools)
        assert len(result) == 2
        # Only last tool has cache_control
        assert "cache_control" not in result[0]
        assert result[-1]["cache_control"] == {"type": "ephemeral"}

    def test_convert_tools_with_cache_disabled(self, provider):
        provider.cache_enabled = False
        tools = [
            {
                "function": {
                    "name": "t1",
                    "description": "d1",
                    "parameters": {"type": "object", "properties": {}},
                }
            },
        ]
        result = provider._convert_tools_with_cache(tools)
        assert "cache_control" not in result[0]

    def test_extract_cache_usage(self, provider):
        from core.llm.providers import AnthropicProvider

        usage = MagicMock()
        usage.cache_read_input_tokens = 500
        usage.cache_creation_input_tokens = 100
        r, c = AnthropicProvider._extract_cache_usage(usage)
        assert r == 500
        assert c == 100

    def test_extract_cache_usage_missing_fields(self, provider):
        from core.llm.providers import AnthropicProvider

        usage = MagicMock(spec=[])  # no attributes
        r, c = AnthropicProvider._extract_cache_usage(usage)
        assert r == 0
        assert c == 0


# ---------------------------------------------------------------------------
# 5. OpenAI — cached_tokens extraction
# ---------------------------------------------------------------------------


class TestOpenAICacheExtraction:
    def test_extract_with_details(self):
        from core.llm.providers import _extract_openai_cached_tokens

        usage = MagicMock()
        usage.prompt_tokens_details.cached_tokens = 300
        assert _extract_openai_cached_tokens(usage) == 300

    def test_extract_no_details(self):
        from core.llm.providers import _extract_openai_cached_tokens

        usage = MagicMock(spec=["prompt_tokens", "completion_tokens"])
        assert _extract_openai_cached_tokens(usage) == 0

    def test_extract_details_none_cached(self):
        from core.llm.providers import _extract_openai_cached_tokens

        usage = MagicMock()
        usage.prompt_tokens_details.cached_tokens = None
        assert _extract_openai_cached_tokens(usage) == 0

    def test_extract_usage_includes_cache(self):
        from core.llm.providers import _extract_usage

        resp = MagicMock()
        resp.usage.prompt_tokens = 100
        resp.usage.completion_tokens = 50
        resp.usage.total_tokens = 150
        resp.usage.prompt_tokens_details.cached_tokens = 80
        result = _extract_usage(resp)
        assert result["cache_read_tokens"] == 80
