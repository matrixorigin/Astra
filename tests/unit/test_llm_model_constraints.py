"""Tests: model-specific parameter constraints and error recovery.

Verifies:
- fixed_temperature override applied in _dispatch and streaming paths
- Client errors (400) do NOT trigger circuit breaker (they're our fault, not server's)
- Circuit breaker recovers after model switch (success resets state)
- kimi-k2.5 quirks: fixed_temperature=1.0, strict_tool_call_ids, preserve_reasoning_content
- deepseek quirks: no special constraints (baseline behavior)
"""

import pytest
from unittest.mock import MagicMock

from core.llm.rate_limiter import CircuitBreaker
from core.llm.router import ModelConfig, ModelQuirks

# Shared helpers from conftest — make_model_config, make_ok_response, make_test_llm_client
from tests.unit.conftest import make_model_config as _make_model_config
from tests.unit.conftest import make_ok_response as _ok_response
from tests.unit.conftest import make_test_llm_client as _make_test_llm_client


# ── fixed_temperature override in _dispatch ──────────────────────


class TestFixedTemperatureDispatch:
    """Verify _dispatch applies fixed_temperature from ModelConfig.quirks."""

    def test_overrides_temperature_when_set(self):
        """Model with fixed_temperature=1.0 should override caller's temperature."""
        provider = MagicMock()
        provider.complete.return_value = _ok_response("kimi-k2.5")
        cfg = _make_model_config("kimi-k2.5", "moonshot", fixed_temp=1.0)
        with _make_test_llm_client(provider, [cfg]) as client:
            client._dispatch("kimi-k2.5", "complete", messages=[], temperature=0.7)

        _, kwargs = provider.complete.call_args
        assert kwargs["temperature"] == 1.0

    def test_preserves_temperature_when_no_constraint(self):
        """Model without fixed_temperature should use caller's temperature."""
        provider = MagicMock()
        provider.complete.return_value = _ok_response("deepseek-chat")
        cfg = _make_model_config("deepseek-chat", "deepseek")
        with _make_test_llm_client(provider, [cfg]) as client:
            client._dispatch("deepseek-chat", "complete", messages=[], temperature=0.3)

        _, kwargs = provider.complete.call_args
        assert kwargs["temperature"] == 0.3

    def test_strict_tool_call_ids_rewrites_messages(self):
        """Model with strict_tool_call_ids should rewrite non-standard ids."""
        provider = MagicMock()
        provider.complete.return_value = _ok_response("kimi-k2.5")
        cfg = _make_model_config("kimi-k2.5", "moonshot", fixed_temp=1.0, strict_ids=True)
        with _make_test_llm_client(provider, [cfg]) as client:
            messages = [
                {"role": "assistant", "content": "", "tool_calls": [
                    {"id": "read_file:1", "type": "function", "function": {"name": "read_file", "arguments": "{}"}},
                ]},
                {"role": "tool", "tool_call_id": "read_file:1", "content": "data"},
            ]
            client._dispatch("kimi-k2.5", "complete", messages=messages, temperature=0.7)

        _, kwargs = provider.complete.call_args
        # The id should have been rewritten to call_xxx format
        tc_id = kwargs["messages"][0]["tool_calls"][0]["id"]
        assert tc_id.startswith("call_")
        assert kwargs["messages"][1]["tool_call_id"] == tc_id


# ── Circuit breaker should NOT trip on client errors ─────────────


class TestCircuitBreakerClientErrors:
    """400-class errors are our fault (bad params), not server failures.

    The circuit breaker should only open on server errors (5xx, timeouts).
    Client errors should propagate immediately without poisoning the breaker.
    """

    def test_400_error_does_not_open_circuit(self):
        """_dispatch must propagate 400 errors WITHOUT calling breaker.record_failure().

        Regression: before the _is_client_error fix, a 400 'invalid temperature'
        from kimi-k2.5 would trip the circuit breaker, blocking ALL subsequent
        requests to the provider even though the error was our fault (bad params).
        """
        provider = MagicMock()
        err = type("BadRequestError", (Exception,), {})()
        err.status_code = 400
        provider.complete.side_effect = err

        cfg = _make_model_config("kimi-k2.5", "moonshot", fixed_temp=1.0)
        with _make_test_llm_client(provider, [cfg]) as client:
            breaker = client.rate_limiter.get_breaker("moonshot")

            # _dispatch should raise the 400 immediately (not retry, not trip breaker)
            with pytest.raises(Exception) as exc_info:
                client._dispatch("kimi-k2.5", "complete", messages=[], temperature=0.7)
            assert exc_info.value.status_code == 400

            # Critical: breaker must still be closed — 400 is our fault, not server's
            assert breaker.state == "closed"
            assert breaker._failures == 0

    def test_server_errors_open_circuit(self):
        """5xx errors should open the circuit after threshold."""
        breaker = CircuitBreaker(failure_threshold=2, recovery_timeout=60.0)

        breaker.record_failure()
        assert breaker.state == "closed"

        breaker.record_failure()
        assert breaker.state == "open"
        assert breaker.allow_request() is False

    def test_success_resets_after_failures(self):
        """A single success should reset failure count and close circuit."""
        breaker = CircuitBreaker(failure_threshold=3, recovery_timeout=60.0)

        breaker.record_failure()
        breaker.record_failure()
        assert breaker.state == "closed"  # not yet at threshold

        breaker.record_success()
        assert breaker._failures == 0
        assert breaker.state == "closed"

        # Now need 3 more failures to open
        breaker.record_failure()
        breaker.record_failure()
        assert breaker.state == "closed"


# ── Model switch recovery ────────────────────────────────────────


class TestModelSwitchRecovery:
    """After switching models, errors from old model should not block new model."""

    def test_different_providers_have_independent_breakers(self):
        """moonshot breaker open should not affect deepseek breaker."""
        from core.llm.rate_limiter import RateLimiter
        rl = RateLimiter()

        moonshot_breaker = rl.get_breaker("moonshot")
        deepseek_breaker = rl.get_breaker("deepseek")

        # Trip moonshot breaker
        for _ in range(5):
            moonshot_breaker.record_failure()
        assert moonshot_breaker.state == "open"

        # deepseek should be unaffected
        assert deepseek_breaker.state == "closed"
        assert deepseek_breaker.allow_request() is True

    def test_dispatch_skips_open_breaker_tries_next(self):
        """If primary model's breaker is open, _dispatch should try fallback."""
        provider = MagicMock()
        provider.complete.return_value = _ok_response("deepseek-chat")

        kimi_cfg = _make_model_config("kimi-k2.5", "moonshot", fixed_temp=1.0)
        ds_cfg = _make_model_config("deepseek-chat", "deepseek")
        with _make_test_llm_client(provider, [kimi_cfg, ds_cfg]) as client:
            # Open moonshot breaker
            breaker = client.rate_limiter.get_breaker("moonshot")
            for _ in range(5):
                breaker.record_failure()

            result, used_cfg = client._dispatch("kimi-k2.5", "complete", messages=[], temperature=0.7)

        # Should have used deepseek (moonshot was skipped)
        assert result.model == "deepseek-chat"


# ── Streaming path: fixed_temperature ────────────────────────────


class TestStreamingFixedTemperature:
    """Verify streaming methods also apply fixed_temperature."""

    @pytest.mark.asyncio
    async def test_chat_stream_uses_fixed_temperature(self):
        """chat_stream should pass fixed_temperature to provider."""
        chunks = [
            {"type": "text", "content": "hello"},
            {"type": "usage", "prompt": 10, "completion": 5, "cache_read": 0, "cache_creation": 0},
        ]
        provider = MagicMock()
        provider.complete_stream.return_value = iter(chunks)

        cfg = _make_model_config("kimi-k2.5", "moonshot", fixed_temp=1.0)
        with _make_test_llm_client(provider, [cfg]) as client:
            collected = []
            async for item in client.chat_stream([], "user1", model="kimi-k2.5"):
                collected.append(item)

        # Positional: complete_stream(messages, model, temperature, max_tokens)
        args = provider.complete_stream.call_args[0]
        assert args[1] == "kimi-k2.5", f"Anchor: arg[1] must be model name, got {args[1]}"
        assert args[2] == 1.0, f"Expected fixed temperature 1.0, got {args[2]}"

    @pytest.mark.asyncio
    async def test_chat_with_tools_stream_uses_fixed_temperature(self):
        """chat_with_tools_stream should pass fixed_temperature to provider."""
        chunks = [
            {"type": "text", "content": "hi"},
            {"type": "usage", "prompt": 10, "completion": 5, "cache_read": 0, "cache_creation": 0},
        ]
        provider = MagicMock()
        provider.complete_with_tools_stream.return_value = iter(chunks)

        cfg = _make_model_config("kimi-k2.5", "moonshot", fixed_temp=1.0)
        with _make_test_llm_client(provider, [cfg]) as client:
            collected = []
            async for item in client.chat_with_tools_stream([], [], model="kimi-k2.5"):
                collected.append(item)

        # Positional: complete_with_tools_stream(messages, tools, model, tool_choice, temperature, max_tokens)
        args = provider.complete_with_tools_stream.call_args[0]
        assert args[2] == "kimi-k2.5", f"Anchor: arg[2] must be model name, got {args[2]}"
        assert args[4] == 1.0, f"Expected fixed temperature 1.0, got {args[4]}"


# ── Kimi-specific: combined quirks ───────────────────────────────


class TestKimiQuirksCombined:
    """kimi-k2.5 needs: fixed_temperature=1.0 + strict_tool_call_ids + preserve_reasoning_content."""

    def test_kimi_config_from_seed(self):
        """Verify seed data produces correct ModelConfig with all quirks."""
        from core.llm.seed_models import SEED_MODELS
        kimi = next(m for m in SEED_MODELS if m["model_name"] == "kimi-k2.5")

        cfg = ModelConfig(**kimi)

        assert cfg.fixed_temperature == 1.0
        assert cfg.quirks.strict_tool_call_ids is True
        assert cfg.quirks.preserve_reasoning_content is True
        assert cfg.quirks.fixed_temperature == 1.0

    def test_deepseek_has_no_quirks(self):
        """deepseek-chat should have default (no-op) quirks."""
        from core.llm.seed_models import SEED_MODELS
        ds = next(m for m in SEED_MODELS if m["model_name"] == "deepseek-chat")

        cfg = ModelConfig(**ds)

        assert cfg.fixed_temperature is None
        assert cfg.quirks.strict_tool_call_ids is False
        assert cfg.quirks.preserve_reasoning_content is False


# ── _is_client_error classification ──────────────────────────────


class TestClientErrorClassification:
    """Verify _is_client_error correctly distinguishes 4xx from 5xx."""

    def test_400_is_client_error(self):
        from core.llm.client import _is_client_error
        err = type("BadRequestError", (Exception,), {})()
        err.status_code = 400
        assert _is_client_error(err) is True

    def test_422_is_client_error(self):
        from core.llm.client import _is_client_error
        err = type("ValidationError", (Exception,), {})()
        err.status_code = 422
        assert _is_client_error(err) is True

    def test_500_is_not_client_error(self):
        from core.llm.client import _is_client_error
        err = type("InternalServerError", (Exception,), {})()
        err.status_code = 500
        assert _is_client_error(err) is False

    def test_no_status_code_is_not_client_error(self):
        from core.llm.client import _is_client_error
        err = RuntimeError("connection reset")
        assert _is_client_error(err) is False

    def test_error_message_with_400(self):
        """Error message containing '400' should be detected."""
        from core.llm.client import _is_client_error
        err = Exception("Error code: 400 - {'error': {'message': 'invalid temperature'}}")
        assert _is_client_error(err) is True

    def test_429_is_not_client_error(self):
        """429 (rate limit) should NOT be treated as client error — it's retryable."""
        from core.llm.client import _is_client_error
        err = type("RateLimitError", (Exception,), {})()
        err.status_code = 429
        assert _is_client_error(err) is False
