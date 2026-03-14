"""Shared test fixtures."""

import pytest
from contextlib import contextmanager
from unittest.mock import MagicMock, patch

from core.llm.client import LLMClient
from core.llm.models import LLMProvider, LLMResponse
from core.llm.rate_limiter import RateLimiter
from core.llm.router import ModelConfig, ModelQuirks


@pytest.fixture
def db(db_session):
    """Database session for testing (uses shared db_session from root conftest)."""
    yield db_session


# ── Shared LLM client test helpers ──────────────────────────────────────────


def make_model_config(
    name: str = "test-model",
    provider: str = "openai",
    fixed_temp: float | None = None,
    strict_ids: bool = False,
) -> ModelConfig:
    """Build a ModelConfig with optional quirks for testing."""
    return ModelConfig(
        model_name=name,
        provider=provider,
        quirks=ModelQuirks(
            fixed_temperature=fixed_temp,
            strict_tool_call_ids=strict_ids,
        ),
    )


def make_ok_response(model: str = "test-model") -> LLMResponse:
    return LLMResponse(
        content="ok",
        model=model,
        provider=LLMProvider.OPENAI,
        tokens_prompt=10,
        tokens_completion=5,
        tokens_total=15,
        latency_ms=100,
        cost_usd=0.001,
    )


@contextmanager
def make_test_llm_client(provider_mock, model_cfgs: list[ModelConfig]):
    """Build LLMClient with mocked internals, bypassing DB.

    Shared across unit and integration tests that need to verify _dispatch
    behavior (temperature override, circuit breaker, tool_call_id rewriting).
    """
    with patch.object(LLMClient, "__init__", return_value=None):
        client = LLMClient.__new__(LLMClient)
    client.config = {"provider": "openai", "budget_usd": 0, "temperature": 0.7, "max_tokens": 4096}
    client._total_spend_usd = 0.0
    client._ctx_user_id = LLMClient._ctx_user_id
    client._ctx_router = LLMClient._ctx_router
    client._ctx_aux_calls = LLMClient._ctx_aux_calls
    client.user_id = "test"
    client.rate_limiter = RateLimiter()

    router = MagicMock()
    router.route.return_value = model_cfgs
    router.list_models.return_value = model_cfgs
    router.calculate_cost.return_value = 0.0
    client.router = router

    client._check_model_permission = MagicMock()
    client._resolve_model = MagicMock(return_value=model_cfgs[0].model_name)
    client._check_budget = MagicMock()
    client._check_context_overflow = MagicMock()
    client._log_call = MagicMock()
    client._get_provider = MagicMock(return_value=provider_mock)
    yield client
