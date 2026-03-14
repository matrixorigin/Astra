"""Tests that LLM streaming methods yield control to the event loop.

These tests go through the real LLMClient methods (not just the to_thread call
in isolation) so they catch regressions if the method structure changes.  The
mock setup is unavoidable — LLMClient has DB, router, and rate-limiter deps.
"""

import asyncio
import time
from contextlib import suppress
from unittest.mock import MagicMock, patch

import pytest


def _make_slow_sync_iter(chunks, delay=0.1):
    """Return a sync iterator that sleeps between chunks."""

    def _iter():
        for c in chunks:
            time.sleep(delay)
            yield c

    return _iter()


def _make_client(provider_mock):
    """Build a minimal LLMClient with a mocked provider, bypassing DB."""
    with patch("core.llm.client.LLMClient.__init__", return_value=None):
        from core.llm.client import LLMClient

        client = LLMClient.__new__(LLMClient)
    client._providers = {"mock": provider_mock}
    client._model_keys = {}
    client.config = {"provider": "mock", "model": "m", "temperature": 0.7}
    client._total_spend_usd = 0.0
    client._ctx_user_id = LLMClient._ctx_user_id
    client._ctx_router = LLMClient._ctx_router
    client.user_id = "test"
    router_mock = MagicMock()
    router_mock.route.return_value = [
        MagicMock(
            model_name="m",
            provider="mock",
            enable_cache=False,
        )
    ]
    router_mock.calculate_cost.return_value = 0.0
    client.router = router_mock
    rl = MagicMock()
    breaker = MagicMock()
    breaker.allow_request.return_value = True
    rl.get_breaker.return_value = breaker
    client.rate_limiter = rl
    client._check_model_permission = MagicMock()
    client._resolve_model = MagicMock(return_value="m")
    client._check_budget = MagicMock()
    client._log_call = MagicMock()
    return client


_USAGE_CHUNK = {"type": "usage", "prompt": 1, "completion": 1, "cache_read": 0, "cache_creation": 0}


async def _assert_event_loop_not_blocked(async_gen):
    """Consume *async_gen* while a concurrent tick task runs; assert tick ran."""
    counter = 0

    async def tick():
        nonlocal counter
        while True:
            await asyncio.sleep(0.01)
            counter += 1

    task = asyncio.create_task(tick())
    # Yield once so tick task is scheduled before we start consuming.
    await asyncio.sleep(0)
    collected = []
    async for chunk in async_gen:
        collected.append(chunk)
    task.cancel()
    with suppress(asyncio.CancelledError):
        await task
    assert counter > 0, "Event loop was blocked — concurrent task never ran"
    return collected


@pytest.mark.asyncio
async def test_chat_with_tools_stream_yields_control():
    """Concurrent task runs while chat_with_tools_stream iterates."""
    provider = MagicMock()
    provider.complete_with_tools_stream.return_value = _make_slow_sync_iter(
        [{"type": "text", "content": "hi"}, _USAGE_CHUNK],
        delay=0.3,
    )
    client = _make_client(provider)
    collected = await _assert_event_loop_not_blocked(
        client.chat_with_tools_stream([], [], model="m"),
    )
    assert any(c["type"] == "text" for c in collected)


@pytest.mark.asyncio
async def test_chat_stream_yields_control():
    """Concurrent task runs while chat_stream iterates."""
    provider = MagicMock()
    provider.complete_stream.return_value = _make_slow_sync_iter(
        [{"type": "text", "content": "hello"}, _USAGE_CHUNK],
        delay=0.3,
    )
    client = _make_client(provider)
    collected = await _assert_event_loop_not_blocked(
        client.chat_stream([], "user1", model="m"),
    )
    assert any(c.get("content") == "hello" for c in collected)


@pytest.mark.asyncio
async def test_end_sentinel_terminates_cleanly():
    """Provider returns 3 text chunks then stops — all yielded, generator exits."""
    chunks = [
        {"type": "text", "content": "a"},
        {"type": "text", "content": "b"},
        {"type": "text", "content": "c"},
        _USAGE_CHUNK,
    ]
    provider = MagicMock()
    provider.complete_with_tools_stream.return_value = iter(chunks)
    client = _make_client(provider)

    texts = [
        c["content"]
        async for c in client.chat_with_tools_stream([], [], model="m")
        if c.get("type") == "text"
    ]
    assert texts == ["a", "b", "c"]
