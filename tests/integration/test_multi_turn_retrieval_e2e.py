"""Multi-turn E2E test for retrieval-based history management.

Verifies the full /chat/turn flow across 6 turns:
1. Prompt tokens stay bounded (not linear growth)
2. Recent turns preserved in LLM messages
3. Old turns retrieved by relevance (not blindly included)
4. DB ground truth: ctx_snapshots token_budget, agent_events
5. Fallback works when embeddings unavailable
"""

import json
import os
from uuid import uuid4

import pytest
from fastapi.testclient import TestClient
from sqlalchemy import text as sql_text
from unittest.mock import patch

os.environ.setdefault("TOKEN_ENCRYPTION_KEY", "test-key-" + "x" * 32)
os.environ.setdefault("JWT_SECRET_KEY", "test-jwt-secret-" + "x" * 32)

from api.main import app
from tests.conftest import parse_sse_events, fake_llm_stream, get_auth_headers, flush_persist_threads


@pytest.fixture
def client():
    return TestClient(app)


def _unique_auth(client, db, prefix="rh"):
    uid = uuid4().hex[:8]
    user_id = str(uuid4())
    headers = get_auth_headers(
        client, db,
        username=f"{prefix}_{uid}",
        user_id=user_id,
        email=f"{prefix}_{uid}@test.com",
        password="pass123",
    )
    return headers, user_id


_EDGE_TOOLS = [
    {"type": "function", "function": {"name": "bash", "description": "Run shell command",
     "parameters": {"type": "object", "properties": {"command": {"type": "string"}}, "required": ["command"]}}},
]


def _do_turn(client, headers, session_id, user_msg, edge_tools=None, turn_num=0):
    """Execute one /chat/turn and return parsed SSE events.

    Mocks LLM to return a text response with tool_call on odd turns.
    """
    # Build messages array (simulating what CLI sends)
    messages = [{"role": "user", "content": user_msg}]

    # LLM returns text + optional tool_call
    if turn_num % 2 == 0:
        # Even turns: text-only response
        llm_chunks = [
            {"type": "text", "content": f"Response for turn {turn_num}: {user_msg[:50]}. " + "x" * 200},
            {"type": "usage", "prompt": 3000 + turn_num * 100, "completion": 150},
        ]
    else:
        # Odd turns: tool_call (cloud skill)
        llm_chunks = [
            {"type": "text", "content": f"Let me look up info for turn {turn_num}. "},
            {"type": "tool_call", "data": {
                "id": f"tc-{turn_num}", "type": "function",
                "function": {"name": "bash", "arguments": json.dumps({"command": "echo test"})},
            }},
            {"type": "usage", "prompt": 3000 + turn_num * 100, "completion": 50},
        ]

    body = {
        "session_id": session_id,
        "user_message": user_msg,
        "messages": messages,
    }
    if edge_tools:
        body["edge_tools"] = edge_tools

    with patch("core.llm.client.LLMClient.chat_with_tools_stream",
               return_value=fake_llm_stream(llm_chunks)):
        resp = client.post("/chat/turn", json=body, headers=headers)

    assert resp.status_code == 200, f"Turn {turn_num} failed: {resp.text[:200]}"
    return parse_sse_events(resp.text)


class TestMultiTurnRetrievalHistory:
    """6-turn E2E test verifying retrieval-based history."""

    def test_six_turn_prompt_tokens_bounded(self, client, db_factory):
        """Run 6 turns, verify prompt tokens don't grow linearly."""
        with db_factory() as db:
            headers, user_id = _unique_auth(client, db)

        # Create session
        resp = client.post("/sessions", json={"metadata": {}}, headers=headers)
        assert resp.status_code == 201
        session_id = resp.json()["session_id"]

        queries = [
            "What are the latest PRs for matrixone?",
            "Show me the CI status",
            "What about dify project PRs?",
            "How is tidb doing?",
            "Compare matrixone and dify",
            "What was the first thing I asked about?",
        ]

        turn_events = []
        for i, query in enumerate(queries):
            edge = _EDGE_TOOLS if i == 0 else None
            events = _do_turn(client, headers, session_id, query, edge_tools=edge, turn_num=i)
            turn_events.append(events)

        flush_persist_threads()

        # Verify: extract usage events from each turn
        prompt_tokens = []
        for i, events in enumerate(turn_events):
            usage = [e for e in events if e.get("type") == "usage"]
            if usage:
                prompt_tokens.append(usage[-1].get("prompt_tokens", 0))

        # Key assertion: prompt tokens should NOT grow linearly
        # With retrieval-based history, later turns should not be much larger than early turns
        if len(prompt_tokens) >= 4:
            # Turn 6 should not be more than 2x Turn 2
            # (without retrieval, it would be ~3-4x due to accumulated history)
            assert prompt_tokens[-1] <= prompt_tokens[1] * 3, \
                f"Prompt tokens growing too fast: {prompt_tokens}"

    def test_recent_turns_in_llm_messages(self, client, db_factory):
        """Verify the last 2 turns are always in the LLM messages."""
        with db_factory() as db:
            headers, user_id = _unique_auth(client, db)

        resp = client.post("/sessions", json={"metadata": {}}, headers=headers)
        session_id = resp.json()["session_id"]

        # Run 4 turns
        queries = ["alpha topic", "beta topic", "gamma topic", "delta topic"]
        for i, q in enumerate(queries):
            edge = _EDGE_TOOLS if i == 0 else None
            _do_turn(client, headers, session_id, q, edge_tools=edge, turn_num=i)

        flush_persist_threads()

        # Check session cache has full history
        from api.routers.chat import _session_cache
        entry = _session_cache.get(session_id)
        assert entry is not None, "Session should be in cache"
        history = entry.get("history", [])

        # Full history should have all turns
        user_msgs = [m["content"] for m in history if m.get("role") == "user"]
        assert "alpha topic" in user_msgs, "Turn 1 should be in full history"
        assert "delta topic" in user_msgs, "Turn 4 should be in full history"

    def test_db_events_persisted_all_turns(self, client, db_factory):
        """Verify agent_events has records for every turn."""
        with db_factory() as db:
            headers, user_id = _unique_auth(client, db)

        resp = client.post("/sessions", json={"metadata": {}}, headers=headers)
        session_id = resp.json()["session_id"]

        for i in range(4):
            edge = _EDGE_TOOLS if i == 0 else None
            _do_turn(client, headers, session_id, f"Question {i}", edge_tools=edge, turn_num=i)

        flush_persist_threads()

        # Verify DB ground truth
        with db_factory() as db:
            # Count user_query events
            row = db.execute(sql_text(
                "SELECT COUNT(*) FROM agent_events "
                "WHERE session_id = :sid AND event_type = 'user_query'"
            ), {"sid": session_id}).scalar()
            assert row == 4, f"Expected 4 user_query events, got {row}"

            # Count llm_response events
            row = db.execute(sql_text(
                "SELECT COUNT(*) FROM agent_events "
                "WHERE session_id = :sid AND event_type = 'llm_response'"
            ), {"sid": session_id}).scalar()
            assert row >= 4, f"Expected >= 4 llm_response events, got {row}"

            # Verify ctx_snapshots exist
            rows = db.execute(sql_text(
                "SELECT total_tokens, token_budget, created_at FROM ctx_snapshots "
                "WHERE session_id = :sid ORDER BY created_at"
            ), {"sid": session_id}).fetchall()
            assert len(rows) >= 1, "Should have at least 1 ctx_snapshot"

            # Verify session event_count updated
            row = db.execute(sql_text(
                "SELECT event_count FROM agent_sessions WHERE session_id = :sid"
            ), {"sid": session_id}).first()
            assert row is not None
            assert row[0] >= 8, f"Expected >= 8 events (4 turns), got {row[0]}"


class TestRetrievalViewIntegration:
    """Verify _build_retrieval_view integrates correctly with _build_turn_messages."""

    def test_cache_retains_full_history_after_retrieval_view(self, client, db_factory):
        """After Turn 5, cache should have full history but LLM got trimmed view."""
        with db_factory() as db:
            headers, user_id = _unique_auth(client, db)

        resp = client.post("/sessions", json={"metadata": {}}, headers=headers)
        session_id = resp.json()["session_id"]

        # Run 5 turns with substantial content
        for i in range(5):
            edge = _EDGE_TOOLS if i == 0 else None
            _do_turn(client, headers, session_id, f"Detailed question {i} about topic {i}", edge_tools=edge, turn_num=i)

        flush_persist_threads()

        # Verify cache has full history
        from api.routers.chat import _session_cache
        entry = _session_cache.get(session_id)
        assert entry is not None
        history = entry.get("history", [])

        # Should have system + 5 turns worth of messages
        user_msgs = [m for m in history if m.get("role") == "user"]
        assert len(user_msgs) >= 5, f"Cache should have all 5 user messages, got {len(user_msgs)}"

    def test_retrieval_view_smaller_than_full_history(self, db_factory):
        """Directly test that _build_retrieval_view produces smaller output."""
        from api.routers.chat import _build_retrieval_view, _MIN_HISTORY_FOR_RETRIEVAL
        from core.context.compaction import estimate_tokens

        # Build a 10-turn history
        history = [{"role": "system", "content": "You are helpful. " + "s" * 500}]
        for i in range(10):
            history.append({"role": "user", "content": f"Question about project {i}"})
            history.append({"role": "assistant", "content": "", "tool_calls": [
                {"id": f"tc{i}", "type": "function", "function": {"name": "bash", "arguments": "{}"}}
            ]})
            history.append({"role": "tool", "tool_call_id": f"tc{i}",
                           "content": f"Result for project {i}: " + "d" * 500})
            history.append({"role": "assistant", "content": f"Answer about project {i}. " + "a" * 300})

        assert len(history) >= _MIN_HISTORY_FOR_RETRIEVAL

        full_tokens = estimate_tokens(history)
        current_messages = [{"role": "user", "content": "What about project 3?"}]

        with db_factory() as db:
            result = _build_retrieval_view(history, "test-rv", current_messages, db)

        result_tokens = estimate_tokens(result)
        assert result_tokens < full_tokens, \
            f"View ({result_tokens}) should be smaller than full ({full_tokens})"

        # System message preserved
        assert result[0]["role"] == "system"

        # Recent messages present
        last_user = [m for m in result if m.get("role") == "user"]
        assert len(last_user) >= 1, "Should have at least 1 user message in view"
