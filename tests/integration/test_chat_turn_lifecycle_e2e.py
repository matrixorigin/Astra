"""Multi-turn + tool-call lifecycle e2e test for /chat/turn.

Covers the full lifecycle: session create → multi-turn with tool calls →
context refresh → event persistence → session close with hooks.
Also covers: model routing integration, recovery + refresh correction.
"""

import json
import os

import pytest
from fastapi.testclient import TestClient
from sqlalchemy import text as sql_text
from unittest.mock import patch, MagicMock, ANY

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

    def test_refresh_memory_changes_system_prompt(self, client, db):
        """Turn 3 refresh_memory produces a different system prompt than turn 1.

        Mocks _build_memory to return query-dependent content, then verifies
        the system message in the LLM call actually contains the refreshed memory.
        """
        headers = self._auth(client, db)

        captured_messages: list[list] = []

        # Wrap chat_stream to capture the messages (system prompt) sent to LLM
        original_chat_stream = None

        async def _capturing_stream(messages, *args, **kwargs):
            captured_messages.append(list(messages))
            async for chunk in fake_llm_stream([{"type": "text", "content": "ok"}]):
                yield chunk

        # ── Turn 1: initial assemble ──
        with patch("core.llm.client.LLMClient.chat_stream", side_effect=_capturing_stream):
            r1 = client.post("/chat/turn", json={
                "messages": [{"role": "user", "content": "hello"}],
            }, headers=headers)
        session_id = parse_sse_events(r1.text)[0]["session_id"]
        assert len(captured_messages) == 1
        turn1_system = captured_messages[0][0]["content"]

        # ── Turn 2: new query triggers refresh_memory with different memory ──
        # Mock _build_memory to return distinctive content for the new query
        def _mock_build_memory(self_pa, user_id, session_id, query):
            if "explain" in query:
                return "## Refreshed Memory\nUser previously asked about main.py and got tool results."
            return None

        with patch("core.llm.client.LLMClient.chat_stream", side_effect=_capturing_stream), \
             patch("core.context.prompt_assembler.PromptAssembler._build_memory", _mock_build_memory):
            r2 = client.post("/chat/turn", json={
                "messages": [{"role": "user", "content": "explain the code"}],
                "session_id": session_id,
            }, headers=headers)

        assert len(captured_messages) == 2
        turn2_system = captured_messages[1][0]["content"]

        # Turn 2 system prompt must contain the refreshed memory
        assert "Refreshed Memory" in turn2_system, \
            f"Expected refreshed memory in system prompt, got: {turn2_system[:500]}"
        # And it must differ from turn 1 (which had no memory mock)
        assert turn1_system != turn2_system

    def test_model_routing_uses_user_id(self, client, db):
        """LLMClient in /chat/turn is created with user_id for model routing."""
        headers = self._auth(client, db)

        with patch("core.llm.client.LLMClient.__init__", return_value=None) as mock_init, \
             patch("core.llm.client.LLMClient.chat_stream",
                   return_value=fake_llm_stream([{"type": "text", "content": "hi"}])):
            client.post("/chat/turn", json={
                "messages": [{"role": "user", "content": "hello"}],
            }, headers=headers)

        # LLMClient is constructed multiple times (assembler internals, shared client, etc.)
        # but the one in event_generator() must pass user_id for model routing.
        calls_with_user_id = [
            c for c in mock_init.call_args_list
            if c.kwargs.get("user_id") == "lifecycle_uid"
               or (len(c.args) > 1 and c.args[1] == "lifecycle_uid")
        ]
        assert len(calls_with_user_id) == 1, \
            f"Expected exactly 1 LLMClient(user_id='lifecycle_uid'), got {mock_init.call_args_list}"

    def test_recovery_then_refresh_corrects_stale_memory(self, client, db):
        """After server restart recovery (stale first_query memory), the next
        user turn triggers refresh_memory with the current query, correcting
        the memory section.

        Scenario: Turn 1 ("read main.py") → server restart → Turn 2 ("write tests")
        Recovery uses first_query="read main.py" for memory search (stale).
        Turn 2's refresh_memory should re-search with "write tests" (current).
        """
        headers = self._auth(client, db)

        # ── Turn 1: establish session with events in DB ──
        with patch("core.llm.client.LLMClient.chat_stream",
                   return_value=fake_llm_stream([{"type": "text", "content": "Here's main.py"}])):
            r1 = client.post("/chat/turn", json={
                "messages": [{"role": "user", "content": "read main.py"}],
            }, headers=headers)
        session_id = parse_sse_events(r1.text)[0]["session_id"]

        # ── Simulate server restart: clear in-memory cache ──
        from api.routers import chat
        chat._session_cache.clear()

        # ── Turn 2: new query after restart ──
        # Track what query _build_memory receives during refresh
        memory_queries: list[str] = []
        original_build_memory = None

        def _tracking_build_memory(self_pa, user_id, session_id, query):
            memory_queries.append(query)
            return f"## Memory for: {query}"

        captured_messages: list[list] = []

        async def _capturing_stream(messages, *args, **kwargs):
            captured_messages.append(list(messages))
            async for chunk in fake_llm_stream([{"type": "text", "content": "ok"}]):
                yield chunk

        with patch("core.llm.client.LLMClient.chat_stream", side_effect=_capturing_stream), \
             patch("core.context.prompt_assembler.PromptAssembler._build_memory", _tracking_build_memory):
            r2 = client.post("/chat/turn", json={
                "messages": [{"role": "user", "content": "write tests for it"}],
                "session_id": session_id,
            }, headers=headers)

        assert r2.status_code == 200

        # _build_memory is called twice:
        # 1. During recovery's assemble() with first_query="read main.py"
        # 2. During refresh_memory() with current query="write tests for it"
        assert len(memory_queries) >= 2, f"Expected ≥2 _build_memory calls, got {memory_queries}"
        # Recovery call uses first_query (stale)
        assert memory_queries[0] == "read main.py"
        # Refresh call uses current query (corrected)
        assert memory_queries[-1] == "write tests for it"

        # The system prompt sent to LLM should contain the refreshed memory
        system_msg = captured_messages[0][0]["content"]
        assert "Memory for: write tests for it" in system_msg, \
            f"System prompt should contain refreshed memory, got: {system_msg[:500]}"
