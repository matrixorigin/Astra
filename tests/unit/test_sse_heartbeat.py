"""Tests for SSE heartbeat wrapper (_with_heartbeat, _sse_ping)."""

import asyncio
import json
import time

import pytest

from api.routers.chat import (
    _sse_ping,
    _with_heartbeat,
)


# ---------------------------------------------------------------------------
# _sse_ping format
# ---------------------------------------------------------------------------

def test_ping_format():
    line = _sse_ping()
    assert line.startswith("data: ")
    assert line.endswith("\n\n")
    payload = json.loads(line[len("data: "):-2])
    assert payload["type"] == "ping"
    assert isinstance(payload["ts"], int)


# ---------------------------------------------------------------------------
# _with_heartbeat
# ---------------------------------------------------------------------------

@pytest.mark.asyncio
async def test_pings_emitted_on_slow_generator(monkeypatch):
    """Multiple pings appear when inner generator pauses longer than interval."""
    monkeypatch.setattr("api.routers.chat.HEARTBEAT_INTERVAL_S", 0.05)

    async def slow():
        yield "data: {\"type\":\"a\"}\n\n"
        await asyncio.sleep(0.5)
        yield "data: {\"type\":\"b\"}\n\n"

    events = [e async for e in _with_heartbeat(slow())]
    types = [json.loads(e[len("data: "):-2])["type"] for e in events]
    pings = [t for t in types if t == "ping"]
    assert types[0] == "a"
    assert types[-1] == "b"
    assert len(pings) >= 3


@pytest.mark.asyncio
async def test_no_ping_on_fast_generator(monkeypatch):
    monkeypatch.setattr("api.routers.chat.HEARTBEAT_INTERVAL_S", 5)

    async def fast():
        for i in range(5):
            yield f"data: {{\"type\":\"e{i}\"}}\n\n"

    events = [e async for e in _with_heartbeat(fast())]
    for e in events:
        payload = json.loads(e[len("data: "):-2])
        assert payload["type"] != "ping"
    assert len(events) == 5


@pytest.mark.asyncio
async def test_sentinel_terminates_cleanly(monkeypatch):
    monkeypatch.setattr("api.routers.chat.HEARTBEAT_INTERVAL_S", 5)

    async def two_events():
        yield "data: {\"type\":\"x\"}\n\n"
        yield "data: {\"type\":\"y\"}\n\n"

    events = [e async for e in _with_heartbeat(two_events())]
    assert len(events) == 2


@pytest.mark.asyncio
async def test_exception_propagates_through_queue(monkeypatch):
    monkeypatch.setattr("api.routers.chat.HEARTBEAT_INTERVAL_S", 5)

    async def exploding():
        yield "data: {\"type\":\"ok\"}\n\n"
        raise ValueError("boom")

    with pytest.raises(ValueError, match="boom"):
        async for _ in _with_heartbeat(exploding()):
            pass


@pytest.mark.asyncio
async def test_drain_task_cancelled_on_consumer_break(monkeypatch):
    """Breaking out of _with_heartbeat cancels the internal _drain task."""
    monkeypatch.setattr("api.routers.chat.HEARTBEAT_INTERVAL_S", 5)

    generator_closed = False

    async def infinite():
        nonlocal generator_closed
        try:
            i = 0
            while True:
                yield f"data: {{\"type\":\"e{i}\"}}\n\n"
                i += 1
                await asyncio.sleep(0.01)
        finally:
            generator_closed = True

    collected = []
    async for e in _with_heartbeat(infinite()):
        collected.append(e)
        if len(collected) >= 2:
            break

    await asyncio.sleep(0.05)
    assert len(collected) == 2
    assert generator_closed, "_drain task was not cancelled — generator still running"


# ---------------------------------------------------------------------------
# Server-side timeout (_next_with_timeout pattern used in event_generator)
#
# NOTE: These tests reproduce the _next_with_timeout closure from
# chat_turn's event_generator.  They verify the *pattern* (deadline +
# wait_for) works correctly, but do NOT exercise the real closure —
# that requires a full chat_turn integration test with DB + LLM.
# ---------------------------------------------------------------------------

@pytest.mark.asyncio
async def test_server_timeout_fires_on_hanging_stream(monkeypatch):
    """If the LLM stream hangs completely, the per-__anext__ timeout fires."""
    monkeypatch.setattr("api.routers.chat.SERVER_TURN_TIMEOUT_S", 0.05)

    async def hanging_llm_stream():
        yield "data: {\"type\":\"text_delta\",\"content\":\"ok\"}\n\n"
        await asyncio.sleep(10)  # simulate LLM hang
        yield "data: {\"type\":\"turn_complete\"}\n\n"

    from api.routers.chat import SERVER_TURN_TIMEOUT_S
    _deadline = time.monotonic() + SERVER_TURN_TIMEOUT_S

    async def _next_with_timeout(aiter):
        remaining = _deadline - time.monotonic()
        if remaining <= 0:
            raise asyncio.TimeoutError
        return await asyncio.wait_for(aiter.__anext__(), timeout=remaining)

    stream = hanging_llm_stream()
    collected = []
    timed_out = False
    try:
        while True:
            item = await _next_with_timeout(stream)
            collected.append(item)
    except StopAsyncIteration:
        pass
    except (asyncio.TimeoutError, TimeoutError):
        timed_out = True

    assert len(collected) == 1
    assert "ok" in collected[0]
    assert timed_out, "Expected timeout but stream completed normally"


@pytest.mark.asyncio
async def test_server_timeout_fires_on_zero_remaining():
    """When remaining time is already <= 0, TimeoutError is raised immediately."""
    _deadline = time.monotonic() - 1  # already expired

    async def _next_with_timeout(aiter):
        remaining = _deadline - time.monotonic()
        if remaining <= 0:
            raise asyncio.TimeoutError
        return await asyncio.wait_for(aiter.__anext__(), timeout=remaining)

    async def stream():
        yield "should not reach"

    with pytest.raises((asyncio.TimeoutError, TimeoutError)):
        await _next_with_timeout(stream())
