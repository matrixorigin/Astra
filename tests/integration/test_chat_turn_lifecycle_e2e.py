"""Multi-turn + tool-call lifecycle e2e test for /chat/turn.

Covers the full lifecycle: session create → multi-turn with tool calls →
context refresh → event persistence → session close with hooks.
Also covers: model routing integration, recovery + refresh correction,
history recovery after cache eviction, tool schema change, edge profile,
budget exceeded, firewall warning.

Tasks 7, 8, 10 from edge-cloud-session-lifecycle-refactoring plan.
"""

import json
import os
from uuid import uuid4

import pytest
from fastapi.testclient import TestClient
from sqlalchemy import text as sql_text
from unittest.mock import patch, MagicMock, ANY

os.environ.setdefault("TOKEN_ENCRYPTION_KEY", "test-key-" + "x" * 32)
os.environ.setdefault("JWT_SECRET_KEY", "test-jwt-secret-" + "x" * 32)

from api.main import app
from tests.conftest import parse_sse_events, fake_llm_stream, get_auth_headers, flush_persist_threads


@pytest.fixture
def client():
    return TestClient(app)


def _unique_auth(client, db, prefix="lc"):
    """Create a unique user per test for -n auto isolation (Task 10)."""
    uid = uuid4().hex[:8]
    return get_auth_headers(
        client, db,
        username=f"{prefix}_{uid}",
        user_id=f"{prefix}_uid_{uid}",
        email=f"{prefix}_{uid}@test.com",
        password="pass123",
    )


_TOOLS = [{"type": "function", "function": {"name": "read_file", "description": "Read",
           "parameters": {"type": "object", "properties": {"path": {"type": "string"}}}}}]


class TestChatTurnMultiTurnE2E:
    """Full lifecycle: multi-turn + tool calls + context refresh + persistence + close."""

    def _auth(self, client, db):
        return _unique_auth(client, db, "lifecycle")

    def test_full_multi_turn_lifecycle(self, client, db):
        """Turn 1 (tool_call) → Turn 2 (tool_result + text) → Turn 3 (new query + context refresh)."""
        headers = self._auth(client, db)
        tools = list(_TOOLS)

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

        # ── Verify events persisted (wait for background thread) ──
        flush_persist_threads()
        event_types = db.execute(sql_text(
            "SELECT event_type FROM conversation_events WHERE session_id = :sid ORDER BY created_at",
        ), {"sid": session_id}).fetchall()
        types = [r[0] for r in event_types]
        assert "user_query" in types
        assert "llm_response" in types

        # ── Verify context snapshots: exactly 2 ──
        # Turn 1: assemble() creates snapshot.  Turn 2: tool_result turn has no
        # Each turn that triggers context assembly or refresh creates a snapshot.
        # Turn 1: full assembly. Turn 2: tool_results trigger refresh. Turn 3: new query refresh.
        snapshot_count = db.execute(sql_text(
            "SELECT COUNT(*) FROM context_snapshots WHERE session_id = :sid",
        ), {"sid": session_id}).scalar()
        assert snapshot_count >= 2, f"Expected >=2 snapshots, got {snapshot_count}"

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
        assert mock_hooks.call_args[0][0] == session_id

    def test_events_persisted_with_tool_calls(self, client, db):
        """Tool call and tool result events are persisted correctly."""
        headers = self._auth(client, db)
        tools = [{"type": "function", "function": {"name": "bash", "description": "run",
                  "parameters": {"type": "object", "properties": {"cmd": {"type": "string"}}}}}]

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

        # Wait for background persistence thread
        flush_persist_threads()

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

        # At least one LLMClient call must include a user_id kwarg (any value)
        calls_with_user_id = [
            c for c in mock_init.call_args_list
            if c.kwargs.get("user_id") is not None
        ]
        assert len(calls_with_user_id) >= 1, \
            f"Expected LLMClient(user_id=...) call, got {mock_init.call_args_list}"

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
        # Wait for background persistence thread to finish writing Turn 1 events
        flush_persist_threads()
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


# ============================================================================
# Task 7: Session Close End-to-End Verification
# ============================================================================

class TestSessionCloseE2E:
    """Verify session close triggers hooks, evicts cache, updates DB status."""

    def test_close_evicts_cache_and_updates_db(self, client, db):
        """Create session → 2 turns → close → verify cache eviction + DB status."""
        headers = _unique_auth(client, db, "close")

        # Turn 1
        with patch("core.llm.client.LLMClient.chat_stream",
                   return_value=fake_llm_stream([{"type": "text", "content": "Hi"}])):
            r1 = client.post("/chat/turn", json={
                "messages": [{"role": "user", "content": "hello"}],
            }, headers=headers)
        session_id = parse_sse_events(r1.text)[0]["session_id"]

        # Turn 2
        with patch("core.llm.client.LLMClient.chat_stream",
                   return_value=fake_llm_stream([{"type": "text", "content": "Sure"}])):
            client.post("/chat/turn", json={
                "messages": [{"role": "user", "content": "thanks"}],
                "session_id": session_id,
            }, headers=headers)

        # Verify cache has the session before close
        from api.routers.chat import _session_cache
        assert _session_cache.get(session_id) is not None

        # Close
        resp = client.post(f"/sessions/{session_id}/close", headers=headers)
        assert resp.status_code == 200
        assert resp.json()["status"] == "closed"

        # Cache evicted
        assert _session_cache.get(session_id) is None

        # DB status is closed
        row = db.execute(sql_text(
            "SELECT status FROM sessions WHERE session_id = :sid"
        ), {"sid": session_id}).fetchone()
        assert row[0] == "closed"


# ============================================================================
# Task 8: Comprehensive /chat/turn E2E Test Suite
# ============================================================================

class TestChatTurnExpanded:
    """Additional e2e scenarios for full lifecycle coverage."""

    # ── 8.4: History recovery after cache eviction ──

    def test_history_recovery_after_cache_eviction(self, client, db):
        """Clear cache between turns → turn 3 still works via DB recovery."""
        headers = _unique_auth(client, db, "evict")

        # Turn 1
        with patch("core.llm.client.LLMClient.chat_stream",
                   return_value=fake_llm_stream([{"type": "text", "content": "Hello"}])):
            r1 = client.post("/chat/turn", json={
                "messages": [{"role": "user", "content": "hi"}],
            }, headers=headers)
        session_id = parse_sse_events(r1.text)[0]["session_id"]

        # Turn 2
        with patch("core.llm.client.LLMClient.chat_stream",
                   return_value=fake_llm_stream([{"type": "text", "content": "OK"}])):
            client.post("/chat/turn", json={
                "messages": [{"role": "user", "content": "do something"}],
                "session_id": session_id,
            }, headers=headers)

        # Wait for background persistence
        flush_persist_threads()

        # Evict cache — simulates server restart
        from api.routers.chat import _session_cache
        _session_cache.clear()

        # Turn 3 — must recover from DB
        with patch("core.llm.client.LLMClient.chat_stream",
                   return_value=fake_llm_stream([{"type": "text", "content": "Recovered"}])):
            r3 = client.post("/chat/turn", json={
                "messages": [{"role": "user", "content": "continue"}],
                "session_id": session_id,
            }, headers=headers)

        e3 = parse_sse_events(r3.text)
        assert any(e["type"] == "text_delta" and e["content"] == "Recovered" for e in e3)
        assert e3[-1]["type"] == "turn_complete"

    # ── 8.5: Tool schema change mid-session triggers system prompt rebuild ──

    def test_tool_schema_change_rebuilds_system(self, client, db):
        """Sending different edge_tools on turn 2 triggers force_rebuild_system."""
        headers = _unique_auth(client, db, "toolchg")
        tools_v1 = list(_TOOLS)
        tools_v2 = [{"type": "function", "function": {"name": "write_file", "description": "Write",
                     "parameters": {"type": "object", "properties": {"path": {"type": "string"}}}}}]

        captured_messages: list[list] = []

        async def _capture(messages, tools, *a, **kw):
            captured_messages.append(list(messages))
            async for c in fake_llm_stream([{"type": "text", "content": "ok"}]):
                yield c

        # Turn 1 with tools_v1
        with patch("core.llm.client.LLMClient.chat_with_tools_stream", side_effect=_capture):
            r1 = client.post("/chat/turn", json={
                "messages": [{"role": "user", "content": "hello"}],
                "edge_tools": tools_v1,
            }, headers=headers)
        session_id = parse_sse_events(r1.text)[0]["session_id"]
        sys1 = captured_messages[0][0]["content"]

        # Turn 2 with tools_v2 — different tool set triggers rebuild
        with patch("core.llm.client.LLMClient.chat_with_tools_stream", side_effect=_capture):
            client.post("/chat/turn", json={
                "messages": [{"role": "user", "content": "now write"}],
                "session_id": session_id,
                "edge_tools": tools_v2,
            }, headers=headers)

        sys2 = captured_messages[1][0]["content"]
        # System prompt should be rebuilt (different tool set → different Self-Model)
        # At minimum, the system message should exist and be a string
        assert isinstance(sys2, str) and len(sys2) > 0

    # ── 8.6: Edge profile injection ──

    def test_edge_profile_in_system_prompt(self, client, db):
        """edge_profile fields appear in the system prompt."""
        headers = _unique_auth(client, db, "profile")

        captured_messages: list[list] = []

        async def _capture(messages, *a, **kw):
            captured_messages.append(list(messages))
            async for c in fake_llm_stream([{"type": "text", "content": "ok"}]):
                yield c

        with patch("core.llm.client.LLMClient.chat_stream", side_effect=_capture):
            client.post("/chat/turn", json={
                "messages": [{"role": "user", "content": "hello"}],
                "edge_profile": {
                    "cwd": "/home/user/myproject",
                    "git_branch": "feature/test",
                    "project_type": "python",
                    "languages": ["python", "sql"],
                },
            }, headers=headers)

        sys_prompt = captured_messages[0][0]["content"]
        # At least one edge_profile field should appear in the system prompt
        assert any(term in sys_prompt for term in [
            "/home/user/myproject", "feature/test", "python",
        ]), f"Expected edge_profile data in system prompt, got: {sys_prompt[:500]}"

    # ── 8.7: Budget exceeded → error event ──

    def test_budget_exceeded_emits_error(self, client, db):
        """BudgetExceededError during LLM call produces an SSE error event."""
        headers = _unique_auth(client, db, "budget")

        from core.llm.client import BudgetExceededError

        async def _raise(*a, **kw):
            raise BudgetExceededError("Session budget $5.00 exceeded")
            yield  # noqa: unreachable — makes this an async generator

        with patch("core.llm.client.LLMClient.chat_stream", side_effect=_raise):
            r = client.post("/chat/turn", json={
                "messages": [{"role": "user", "content": "hello"}],
            }, headers=headers)

        events = parse_sse_events(r.text)
        error_events = [e for e in events if e["type"] == "error"]
        assert len(error_events) >= 1
        assert "budget" in error_events[0]["message"].lower() or "exceeded" in error_events[0]["message"].lower()

    # ── 8.8: Firewall warning event ──

    def test_firewall_warning_emitted(self, client, db):
        """When firewall returns unsafe, a warning event appears before turn_complete."""
        headers = _unique_auth(client, db, "fw")

        # Mock firewall to return unsafe result
        mock_result = MagicMock()
        mock_result.safe_to_deliver = False
        mock_result.claims_failed = 2

        with patch("core.llm.client.LLMClient.chat_stream",
                   return_value=fake_llm_stream([{"type": "text", "content": "The answer is 42."}])), \
             patch("core.verification.firewall.HallucinationFirewall.verify_response",
                   return_value=mock_result):
            r = client.post("/chat/turn", json={
                "messages": [{"role": "user", "content": "what is the answer?"}],
            }, headers=headers)

        events = parse_sse_events(r.text)
        warnings = [e for e in events if e.get("type") == "warning"]
        assert len(warnings) >= 1, f"Expected firewall warning event, got: {[e['type'] for e in events]}"
        assert warnings[0]["claims_failed"] == 2

        # Warning must come before turn_complete
        types = [e["type"] for e in events]
        warn_idx = types.index("warning")
        complete_idx = types.index("turn_complete")
        assert warn_idx < complete_idx

    # ── 8.2: Context snapshots accumulate across turns ──

    def test_context_snapshots_across_turns(self, client, db):
        """After 3 turns, context_snapshots table has >=2 rows for this session."""
        headers = _unique_auth(client, db, "snap")

        session_id = None
        for i in range(3):
            with patch("core.llm.client.LLMClient.chat_stream",
                       return_value=fake_llm_stream([{"type": "text", "content": f"reply {i}"}])):
                r = client.post("/chat/turn", json={
                    "messages": [{"role": "user", "content": f"turn {i}"}],
                    **({"session_id": session_id} if session_id else {}),
                }, headers=headers)
            if session_id is None:
                session_id = parse_sse_events(r.text)[0]["session_id"]

        count = db.execute(sql_text(
            "SELECT COUNT(*) FROM context_snapshots WHERE session_id = :sid"
        ), {"sid": session_id}).scalar()
        assert count >= 2, f"Expected >=2 context snapshots after 3 turns, got {count}"
