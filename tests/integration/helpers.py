"""Shared test helpers for integration tests."""

import json
from uuid import uuid4


def unique_test_id() -> str:
    """Generate unique ID for test isolation (xdist-safe)."""
    return f"test_{uuid4().hex[:16]}"


class NullRenderer:
    """No-op renderer for edge_chat_loop tests."""
    def text(self, c): pass
    def tool_start(self, n, a): pass
    def tool_done(self, n, r, e): pass
    def error(self, m): pass
    def info(self, m): pass


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
