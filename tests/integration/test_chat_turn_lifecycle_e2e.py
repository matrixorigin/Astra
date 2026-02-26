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
from tests.conftest import parse_sse_events, fake_llm_stream, get_auth_headers


@pytest.fixture
def client():
    return TestClient(app)


class TestChatTurnMultiTurnE2E:
    """Full lifecycle: multi-turn + tool calls + context refresh + persistence + close."""

    def _auth(self, client, db):
        return get_auth_headers(client, db, username="lifecycle_user",
                                user_id="lifecycle_uid", email="lc@test.com",
                                password="pass123")

    def test_full_multi_turn_lifecycle(self, client, db):
        """Turn 1 (tool_call) → Turn 2 (tool_result + text) → Turn 3 (new query + context refresh)."""
        headers = self._auth(client, db)
        tools = [{"type": "function", "function": {"name": "read_file", "description": "Read", "parameters": {"type": "object", "properties": {"path": {"type": "string"}}}}}]

        # ── Turn 1: user message → LLM returns tool_call ──
        with patch("core.llm.client.LLMClient.chat_with_tools_stream",
                   return_value=fake_llm_stream([
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
        e1 = parse_sse_events(r1.text)
        assert e1[0]["type"] == "session_info"
        session_id = e1[0]["session_id"]
        tc_events = [e for e in e1 if e["type"] == "tool_call"]
        assert len(tc_events) == 1
        assert tc_events[0]["name"] == "read_file"
        assert e1[-1]["type"] == "turn_complete"
        assert e1[-1]["has_tool_calls"] is True

        # ── Turn 2: tool_results → LLM returns text ──
        with patch("core.llm.client.LLMClient.chat_with_tools_stream",
                   return_value=fake_llm_stream([
                       {"type": "text", "content": "The file contains a main function."},
                   ])):
            r2 = client.post("/chat/turn", json={
                "messages": [],
                "session_id": session_id,
                "tool_results": [{"tool_call_id": "tc_1", "name": "read_file", "result": "def main(): pass"}],
            }, headers=headers)

        e2 = parse_sse_events(r2.text)
        text_events = [e for e in e2 if e["type"] == "text_delta"]
        assert text_events[0]["content"] == "The file contains a main function."
        assert e2[-1]["type"] == "turn_complete"
        assert e2[-1]["has_tool_calls"] is False

        # ── Turn 3: new user query → context refresh should produce new snapshot ──
        with patch("core.llm.client.LLMClient.chat_with_tools_stream",
                   return_value=fake_llm_stream([
                       {"type": "text", "content": "Sure, here's a test."},
                   ])):
            r3 = client.post("/chat/turn", json={
                "messages": [{"role": "user", "content": "write a test for it"}],
                "session_id": session_id,
            }, headers=headers)

        e3 = parse_sse_events(r3.text)
        assert any(e["type"] == "text_delta" for e in e3)

        # ── Verify events persisted ──
        event_types = db.execute(sql_text(
            "SELECT event_type FROM conversation_events WHERE session_id = :sid ORDER BY created_at",
        ), {"sid": session_id}).fetchall()
        types = [r[0] for r in event_types]
        assert "user_query" in types
        assert "llm_response" in types

        # ── Verify context snapshots: exactly 2 ──
        # Turn 1: assemble() creates snapshot.  Turn 2: tool_result turn has no
        # user_query so refresh_memory is skipped (no snapshot).  Turn 3:
        # refresh_memory creates snapshot.
        snapshot_count = db.execute(sql_text(
            "SELECT COUNT(*) FROM context_snapshots WHERE session_id = :sid",
        ), {"sid": session_id}).scalar()
        assert snapshot_count == 2, f"Expected 2 snapshots (turn 1 + turn 3 refresh), got {snapshot_count}"

    def test_session_close_triggers_hooks(self, client, db):
        """Closing a text-only session triggers scoring and knowledge extraction hooks."""
        headers = self._auth(client, db)

        # Create a session via /chat/turn (no tools — uses chat_stream path)
        with patch("core.llm.client.LLMClient.chat_stream",
                   return_value=fake_llm_stream([{"type": "text", "content": "Hi"}])):
            r = client.post("/chat/turn", json={
                "messages": [{"role": "user", "content": "hello"}],
            }, headers=headers)
        session_id = parse_sse_events(r.text)[0]["session_id"]

        # Close session
        with patch("api.services.session_service.SessionService._run_close_hooks") as mock_hooks:
            resp = client.post(f"/sessions/{session_id}/close", headers=headers)
        assert resp.status_code == 200
        assert resp.json()["status"] == "closed"
        mock_hooks.assert_called_once()

    def test_events_persisted_with_tool_calls(self, client, db):
        """Tool call and tool result events are persisted correctly."""
        headers = self._auth(client, db)
        tools = [{"type": "function", "function": {"name": "bash", "description": "run", "parameters": {"type": "object", "properties": {"cmd": {"type": "string"}}}}}]

        # Turn 1: tool call
        with patch("core.llm.client.LLMClient.chat_with_tools_stream",
                   return_value=fake_llm_stream([
                       {"type": "tool_call", "data": {
                           "id": "tc_x", "type": "function",
                           "function": {"name": "bash", "arguments": '{"cmd": "ls"}'},
                       }},
                   ])):
            r1 = client.post("/chat/turn", json={
                "messages": [{"role": "user", "content": "list files"}],
                "edge_tools": tools,
            }, headers=headers)
        session_id = parse_sse_events(r1.text)[0]["session_id"]

        # Turn 2: tool result
        with patch("core.llm.client.LLMClient.chat_with_tools_stream",
                   return_value=fake_llm_stream([{"type": "text", "content": "Done."}])):
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
