"""Multi-turn + tool-call lifecycle e2e test for /chat/turn.

Covers the full lifecycle: session create → multi-turn with tool calls →
context refresh → event persistence → session close with hooks.
"""

import json
import os

import pytest
from fastapi.testclient import TestClient
from sqlalchemy import text as sql_text
from unittest.mock import patch, MagicMock

os.environ.setdefault("TOKEN_ENCRYPTION_KEY", "test-key-" + "x" * 32)
os.environ.setdefault("JWT_SECRET_KEY", "test-jwt-secret-" + "x" * 32)

from api.main import app
from api.models import User


async def _fake_stream_gen(chunks):
    for c in chunks:
        yield c


def _fake_stream(chunks):
    return _fake_stream_gen(chunks)


def _parse_sse(text: str) -> list[dict]:
    events = []
    for line in text.strip().split("\n"):
        if line.startswith("data: "):
            events.append(json.loads(line[6:]))
    return events


@pytest.fixture
def client():
    return TestClient(app)


@pytest.fixture(autouse=True)
def _clear_chat_cache():
    """Clear module-level caches before each test for isolation."""
    from api.routers import chat
    chat._session_cache.clear()
    chat._shared_llm_client = None
    yield
    chat._session_cache.clear()
    chat._shared_llm_client = None


class TestChatTurnMultiTurnE2E:
    """Full lifecycle: multi-turn + tool calls + context refresh + persistence + close."""

    def _get_auth(self, client, db):
        from core.auth.password import hash_password
        user = db.query(User).filter(User.username == "lifecycle_user").first()
        if not user:
            user = User(user_id="lifecycle_uid", username="lifecycle_user",
                        email="lc@test.com", password_hash=hash_password("pass123"))
            db.add(user)
            db.commit()
        resp = client.post("/auth/login", json={"username": "lifecycle_user", "password": "pass123"})
        return {"Authorization": f"Bearer {resp.json()['access_token']}"}

    def test_full_multi_turn_lifecycle(self, client, db):
        """Turn 1 (tool_call) → Turn 2 (tool_result + text) → Turn 3 (new query + context refresh)."""
        headers = self._get_auth(client, db)
        tools = [{"type": "function", "function": {"name": "read_file", "description": "Read", "parameters": {"type": "object", "properties": {"path": {"type": "string"}}}}}]

        # ── Turn 1: user message → LLM returns tool_call ──
        with patch("core.llm.client.LLMClient.chat_with_tools_stream", return_value=_fake_stream([
            {"type": "tool_call", "data": {
                "id": "tc_1", "type": "function",
                "function": {"name": "read_file", "arguments": '{"path": "main.py"}'},
            }},
        ])):
            r1 = client.post("/chat/turn", json={
                "messages": [{"role": "user", "content": "read main.py"}],
                "edge_tools": tools,
                "project_rules": "# Test project",
            }, headers=headers)

        assert r1.status_code == 200
        e1 = _parse_sse(r1.text)
        assert e1[0]["type"] == "session_info"
        session_id = e1[0]["session_id"]
        tc_events = [e for e in e1 if e["type"] == "tool_call"]
        assert len(tc_events) == 1
        assert tc_events[0]["name"] == "read_file"
        assert e1[-1]["type"] == "turn_complete"
        assert e1[-1]["has_tool_calls"] is True

        # ── Turn 2: tool_results → LLM returns text ──
        with patch("core.llm.client.LLMClient.chat_with_tools_stream", return_value=_fake_stream([
            {"type": "text", "content": "The file contains a main function."},
        ])):
            r2 = client.post("/chat/turn", json={
                "messages": [],
                "session_id": session_id,
                "tool_results": [{"tool_call_id": "tc_1", "name": "read_file", "result": "def main(): pass"}],
            }, headers=headers)

        e2 = _parse_sse(r2.text)
        text_events = [e for e in e2 if e["type"] == "text_delta"]
        assert text_events[0]["content"] == "The file contains a main function."
        assert e2[-1]["type"] == "turn_complete"
        assert e2[-1]["has_tool_calls"] is False

        # ── Turn 3: new user query → context refresh should produce new snapshot ──
        with patch("core.llm.client.LLMClient.chat_with_tools_stream", return_value=_fake_stream([
            {"type": "text", "content": "Sure, here's a test."},
        ])):
            r3 = client.post("/chat/turn", json={
                "messages": [{"role": "user", "content": "write a test for it"}],
                "session_id": session_id,
            }, headers=headers)

        e3 = _parse_sse(r3.text)
        assert any(e["type"] == "text_delta" for e in e3)

        # ── Verify events persisted ──
        event_types = db.execute(sql_text(
            "SELECT event_type FROM conversation_events WHERE session_id = :sid ORDER BY created_at",
        ), {"sid": session_id}).fetchall()
        types = [r[0] for r in event_types]
        # Should have user_query, tool_call, tool_result, llm_response events
        assert "user_query" in types
        assert "llm_response" in types

        # ── Verify context snapshots created (at least 2: turn 1 + turn 3 refresh) ──
        snapshot_count = db.execute(sql_text(
            "SELECT COUNT(*) FROM context_snapshots WHERE session_id = :sid",
        ), {"sid": session_id}).scalar()
        assert snapshot_count >= 2, f"Expected ≥2 snapshots, got {snapshot_count}"

    def test_session_close_triggers_hooks(self, client, db):
        """Closing a session triggers scoring and knowledge extraction hooks."""
        headers = self._get_auth(client, db)

        # Create a session via /chat/turn
        with patch("core.llm.client.LLMClient.chat_stream", return_value=_fake_stream([
            {"type": "text", "content": "Hi"},
        ])):
            r = client.post("/chat/turn", json={
                "messages": [{"role": "user", "content": "hello"}],
            }, headers=headers)
        session_id = _parse_sse(r.text)[0]["session_id"]

        # Close session
        with patch("api.services.session_service.SessionService._run_close_hooks") as mock_hooks:
            resp = client.post(f"/sessions/{session_id}/close", headers=headers)
        assert resp.status_code == 200
        assert resp.json()["status"] == "closed"
        mock_hooks.assert_called_once()

    def test_events_persisted_with_tool_calls(self, client, db):
        """Tool call and tool result events are persisted correctly."""
        headers = self._get_auth(client, db)
        tools = [{"type": "function", "function": {"name": "bash", "description": "run", "parameters": {"type": "object", "properties": {"cmd": {"type": "string"}}}}}]

        # Turn 1: tool call
        with patch("core.llm.client.LLMClient.chat_with_tools_stream", return_value=_fake_stream([
            {"type": "tool_call", "data": {
                "id": "tc_x", "type": "function",
                "function": {"name": "bash", "arguments": '{"cmd": "ls"}'},
            }},
        ])):
            r1 = client.post("/chat/turn", json={
                "messages": [{"role": "user", "content": "list files"}],
                "edge_tools": tools,
            }, headers=headers)
        session_id = _parse_sse(r1.text)[0]["session_id"]

        # Turn 2: tool result
        with patch("core.llm.client.LLMClient.chat_with_tools_stream", return_value=_fake_stream([
            {"type": "text", "content": "Done."},
        ])):
            client.post("/chat/turn", json={
                "messages": [],
                "session_id": session_id,
                "tool_results": [{"tool_call_id": "tc_x", "name": "bash", "result": "a.py b.py"}],
            }, headers=headers)

        # Verify tool_call and tool_result events in DB
        rows = db.execute(sql_text(
            "SELECT event_type FROM conversation_events WHERE session_id = :sid AND event_type IN ('tool_call', 'tool_result')",
        ), {"sid": session_id}).fetchall()
        types = [r[0] for r in rows]
        assert "tool_call" in types
        assert "tool_result" in types
