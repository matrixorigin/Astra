"""Shared test helpers for integration tests."""

import json
from uuid import uuid4


def unique_test_id() -> str:
    """Generate unique ID for test isolation (xdist-safe).

    Uses the full 32-char hex of uuid4 — never truncate UUIDs,
    truncation dramatically increases collision probability under
    parallel execution.  Prefix kept to 3 chars so total (36) fits
    VARCHAR(36) columns.
    """
    return f"tt_{uuid4().hex}"


class NullRenderer:
    """No-op renderer for edge_chat_loop tests."""

    def text(self, c):
        pass

    def tool_start(self, n, a):
        pass

    def tool_done(self, n, r, e):
        pass

    def error(self, m):
        pass

    def info(self, m):
        pass


async def fake_stream_gen(chunks):
    """Async generator that yields chunks — used to mock LLM streaming."""
    for c in chunks:
        yield c


def fake_stream(chunks):
    """Wrap chunks in an async generator (convenience for mock return_value)."""
    return fake_stream_gen(chunks)


def parse_sse(response_text: str) -> list[dict]:
    """Parse SSE response text into list of event dicts."""
    events = []
    for line in response_text.strip().split("\n"):
        if line.startswith("data: "):
            events.append(json.loads(line[6:]))
    return events


def get_session_id(response_text: str) -> str:
    """Extract session_id from SSE response, searching all events."""
    for event in parse_sse(response_text):
        if "session_id" in event:
            return event["session_id"]
    raise KeyError("session_id not found in any SSE event")
