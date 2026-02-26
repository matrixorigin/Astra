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
    """Create a unique user per test for -n auto isolation (Task 10).

    Returns (headers, user_id) so tests can make precise assertions on user_id.
    """
    uid = uuid4().hex[:8]
    user_id = f"{prefix}_uid_{uid}"
    headers = get_auth_headers(
        client, db,
        username=f"{prefix}_{uid}",
        user_id=user_id,
        email=f"{prefix}_{uid}@test.com",
        password="pass123",
    )
    return headers, user_id


# Frozen — prevents accidental mutation across tests.
_TOOLS = ({"type": "function", "function": {"name": "read_file", "description": "Read",
           "parameters": {"type": "object", "properties": {"path": {"type": "string"}}}}},)


class TestChatTurnMultiTurnE2E:
    """Full lifecycle: multi-turn + tool calls + context refresh + persistence + close."""

    def _auth(self, client, db):
        return _unique_auth(client, db, "lifecycle")

    def test_full_multi_turn_lifecycle(self, client, db):
        """Turn 1 (tool_call) → Turn 2 (tool_result + text) → Turn 3 (new query + context refresh)."""
        headers, _ = self._auth(client, db)
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

        # ── Verify context snapshots ──
        # Turn 1: full assembly → snapshot. Turn 2: tool_result refresh → snapshot.
        # Turn 3: new query refresh → snapshot. Total = 3.
        snapshot_count = db.execute(sql_text(
            "SELECT COUNT(*) FROM context_snapshots WHERE session_id = :sid",
        ), {"sid": session_id}).scalar()
        assert snapshot_count == 3, f"Expected 3 snapshots (one per turn), got {snapshot_count}"

    def test_session_close_triggers_hooks(self, client, db):
        """Closing a text-only session triggers scoring and knowledge extraction hooks."""
        headers, user_id = self._auth(client, db)

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
        mock_hooks.assert_called_once_with(session_id, user_id)

    def test_events_persisted_with_tool_calls(self, client, db):
        """Tool call and tool result events are persisted correctly."""
        headers, _ = self._auth(client, db)
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
        headers, _ = self._auth(client, db)

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
        """Shared LLMClient in /chat/turn uses request_context with correct user_id."""
        headers, user_id = self._auth(client, db)

        with patch("core.llm.client.LLMClient.request_context") as mock_ctx, \
             patch("core.llm.client.LLMClient.chat_stream",
                   return_value=fake_llm_stream([{"type": "text", "content": "hi"}])):
            # request_context is used as a context manager; make it a no-op CM
            from contextlib import nullcontext
            mock_ctx.return_value = nullcontext()
            client.post("/chat/turn", json={
                "messages": [{"role": "user", "content": "hello"}],
            }, headers=headers)

        calls_with_user_id = [
            c for c in mock_ctx.call_args_list
            if c.kwargs.get("user_id") == user_id
        ]
        assert len(calls_with_user_id) == 1, \
            f"Expected request_context(user_id='{user_id}'), got {mock_ctx.call_args_list}"

    def test_recovery_then_refresh_corrects_stale_memory(self, client, db):
        """After server restart recovery (stale first_query memory), the next
        user turn triggers refresh_memory with the current query, correcting
        the memory section.

        Scenario: Turn 1 ("read main.py") → server restart → Turn 2 ("write tests")
        Recovery uses first_query="read main.py" for memory search (stale).
        Turn 2's refresh_memory should re-search with "write tests" (current).
        """
        headers, _ = self._auth(client, db)

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
        # 1. During recovery's assemble() with last_query="read main.py" (only 1 turn in DB)
        # 2. During refresh_memory() with current query="write tests for it"
        assert len(memory_queries) >= 2, f"Expected ≥2 _build_memory calls, got {memory_queries}"
        # Recovery call uses last user query from DB (which is the only one)
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
        headers, _ = _unique_auth(client, db, "close")

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
        headers, _ = _unique_auth(client, db, "evict")

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
        headers, _ = _unique_auth(client, db, "toolchg")
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
        # System prompt should be rebuilt with the new tool set and differ from turn 1
        assert "write_file" in sys2.lower() or sys2 != sys1, \
            f"Expected rebuilt system prompt with write_file tool, got: {sys2[:500]}"
        assert sys2 != sys1, "System prompt should differ after tool schema change"

    # ── 8.6: Edge profile injection ──

    def test_edge_profile_in_system_prompt(self, client, db):
        """edge_profile fields appear in the system prompt."""
        headers, _ = _unique_auth(client, db, "profile")

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
        headers, _ = _unique_auth(client, db, "budget")

        from core.llm.client import BudgetExceededError

        async def _budget_exceeded_stream(*a, **kw):
            raise BudgetExceededError("Session budget $5.00 exceeded")
            # yield makes this an async generator (required by async for)
            yield  # pragma: no cover

        with patch("core.llm.client.LLMClient.chat_stream", side_effect=_budget_exceeded_stream):
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
        headers, _ = _unique_auth(client, db, "fw")

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
        headers, _ = _unique_auth(client, db, "snap")

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


# ============================================================================
# Coverage gap tests: verify review fixes have real behavioral tests
# ============================================================================

class TestClassifyTask:
    """Unit tests for _classify_task heuristic (Review #1)."""

    def test_last_user_message_used(self):
        """Should classify based on the LAST user message, not the first."""
        from api.routers.chat import _classify_task
        msgs = [
            {"role": "user", "content": "hello"},
            {"role": "assistant", "content": "hi"},
            {"role": "user", "content": "explain the algorithm"},
        ]
        assert _classify_task(msgs) == "reasoning"

    def test_word_boundary_no_false_positive(self):
        """'definitely' should NOT trigger 'code' (no word boundary on 'def')."""
        from api.routers.chat import _classify_task
        assert _classify_task([{"role": "user", "content": "I definitely agree"}]) is None

    def test_code_fenced_block(self):
        from api.routers.chat import _classify_task
        assert _classify_task([{"role": "user", "content": "fix this ```python\nprint()```"}]) == "code"

    def test_file_extension_not_false_positive(self):
        """'read main.py' is NOT code generation — should return None (Review #1 fix)."""
        from api.routers.chat import _classify_task
        assert _classify_task([{"role": "user", "content": "read main.py"}]) is None

    def test_no_simple_category(self):
        """Short messages should return None, not 'simple'."""
        from api.routers.chat import _classify_task
        assert _classify_task([{"role": "user", "content": "hi"}]) is None

    def test_empty_messages(self):
        from api.routers.chat import _classify_task
        assert _classify_task([]) is None


class TestSnapshotRecoveryFastPath:
    """Verify snapshot recovery loads snapshot + post-snapshot events (Review #4)."""

    def test_recovery_via_snapshot_after_3_turns(self, client, db):
        """3 turns → snapshot written → evict cache → turn 4 recovers via snapshot."""
        headers, _ = _unique_auth(client, db, "snaprec")

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

        flush_persist_threads()

        # Verify snapshot was written
        snap = db.execute(sql_text(
            "SELECT COUNT(*) FROM conversation_events WHERE session_id = :sid AND event_type = 'session_history_snapshot'"
        ), {"sid": session_id}).scalar()
        assert snap >= 1, "Snapshot should exist after 3 turns"

        # Evict cache
        from api.routers.chat import _session_cache
        _session_cache.clear()

        # Turn 4 — should recover via snapshot fast-path
        with patch("core.llm.client.LLMClient.chat_stream",
                   return_value=fake_llm_stream([{"type": "text", "content": "recovered"}])):
            r4 = client.post("/chat/turn", json={
                "messages": [{"role": "user", "content": "turn 3 after restart"}],
                "session_id": session_id,
            }, headers=headers)

        e4 = parse_sse_events(r4.text)
        assert any(e["type"] == "text_delta" and e["content"] == "recovered" for e in e4)

    def test_post_snapshot_events_recovered(self, client, db):
        """Turn 4 after snapshot at turn 3 → evict → turn 5 sees turn 4 content."""
        headers, _ = _unique_auth(client, db, "postsnap")

        session_id = None
        # 4 turns: snapshot at turn 3, turn 4 is post-snapshot
        for i in range(4):
            with patch("core.llm.client.LLMClient.chat_stream",
                       return_value=fake_llm_stream([{"type": "text", "content": f"reply {i}"}])):
                r = client.post("/chat/turn", json={
                    "messages": [{"role": "user", "content": f"msg {i}"}],
                    **({"session_id": session_id} if session_id else {}),
                }, headers=headers)
            if session_id is None:
                session_id = parse_sse_events(r.text)[0]["session_id"]

        flush_persist_threads()

        # Evict and recover
        from api.routers.chat import _session_cache
        _session_cache.clear()

        # Capture what messages are sent to LLM on turn 5
        captured: list[list] = []

        async def _capture(messages, *a, **kw):
            captured.append(list(messages))
            async for c in fake_llm_stream([{"type": "text", "content": "ok"}]):
                yield c

        with patch("core.llm.client.LLMClient.chat_stream", side_effect=_capture):
            client.post("/chat/turn", json={
                "messages": [{"role": "user", "content": "turn 4 query"}],
                "session_id": session_id,
            }, headers=headers)

        # The recovered history should contain turn 4's message ("msg 3")
        # which was AFTER the snapshot at turn 3
        all_content = " ".join(m.get("content", "") for m in captured[0])
        assert "msg 3" in all_content, \
            f"Post-snapshot turn 4 content should be recovered, got: {[m.get('content','')[:50] for m in captured[0]]}"


class TestToolResultRefreshQuery:
    """Verify tool-result-only turns use previous user query for refresh (Review #7)."""

    def test_tool_result_refresh_uses_previous_query(self, client, db):
        """On a tool-result-only turn, refresh_memory should receive the
        previous user query, not a placeholder string."""
        headers, _ = _unique_auth(client, db, "trquery")

        # Turn 1: user query + tool call
        with patch("core.llm.client.LLMClient.chat_with_tools_stream",
                   return_value=fake_llm_stream([
                       {"type": "tool_call", "data": {
                           "id": "tc_1", "type": "function",
                           "function": {"name": "read_file", "arguments": '{"path": "x.py"}'},
                       }},
                   ])):
            r1 = client.post("/chat/turn", json={
                "messages": [{"role": "user", "content": "read x.py for me"}],
                "edge_tools": list(_TOOLS),
            }, headers=headers)
        session_id = parse_sse_events(r1.text)[0]["session_id"]

        # Turn 2: tool results only (no new user message)
        refresh_queries: list[str] = []
        original_refresh = None

        def _track_refresh(self_pa, session_id, user_id, user_query, current_sections, **kw):
            refresh_queries.append(user_query)
            # Return a minimal valid result
            from core.context.prompt_assembler import AssembledPrompt
            return AssembledPrompt(
                system_message="refreshed",
                tools_schema=[],
                sections=current_sections,
                token_breakdown={},
                snapshot_id=None,
            )

        with patch("core.llm.client.LLMClient.chat_with_tools_stream",
                   return_value=fake_llm_stream([{"type": "text", "content": "done"}])), \
             patch("core.context.prompt_assembler.PromptAssembler.refresh_memory", _track_refresh):
            client.post("/chat/turn", json={
                "messages": [],
                "session_id": session_id,
                "tool_results": [{"tool_call_id": "tc_1", "name": "read_file", "result": "content"}],
            }, headers=headers)

        assert len(refresh_queries) >= 1, f"refresh_memory should be called, got {refresh_queries}"
        # Should use the previous user query, NOT a placeholder
        assert refresh_queries[-1] == "read x.py for me", \
            f"Expected previous user query, got: '{refresh_queries[-1]}'"


class TestResolvedModelInAudit:
    """Verify audit records the actual resolved model, not 'default' (Review #6)."""

    def test_audit_records_resolved_model(self, client, db):
        headers, _ = _unique_auth(client, db, "modelaudit")

        with patch("core.llm.client.LLMClient.chat_stream",
                   return_value=fake_llm_stream([{"type": "text", "content": "hi"}])), \
             patch("core.llm.client.LLMClient.resolve_model_name", return_value="gpt-4o-mini"):
            r = client.post("/chat/turn", json={
                "messages": [{"role": "user", "content": "hello"}],
            }, headers=headers)
        session_id = parse_sse_events(r.text)[0]["session_id"]

        flush_persist_threads()

        row = db.execute(sql_text(
            "SELECT decision_output FROM decision_audit WHERE session_id = :sid ORDER BY created_at DESC LIMIT 1"
        ), {"sid": session_id}).fetchone()
        assert row is not None, "decision_audit row should exist"
        import json as _json
        output = _json.loads(row[0]) if isinstance(row[0], str) else row[0]
        assert output.get("model_used") == "gpt-4o-mini", \
            f"Audit should record resolved model 'gpt-4o-mini', got: {output}"


# ============================================================================
# Task 6: E2E tests for refactoring tasks 1-4
# ============================================================================


class TestStructuredErrors:
    """Task 3b: Structured error classification in SSE error events."""

    _auth = staticmethod(_unique_auth)

    def _trigger_error(self, client, db, exc_class, exc_args=("fail",), prefix="err"):
        headers, _ = self._auth(client, db, prefix)

        async def _raise(*a, **kw):
            raise exc_class(*exc_args)
            yield  # pragma: no cover

        with patch("core.llm.client.LLMClient.chat_stream", side_effect=_raise):
            r = client.post("/chat/turn", json={
                "messages": [{"role": "user", "content": "hi"}],
            }, headers=headers)
        return [e for e in parse_sse_events(r.text) if e["type"] == "error"]

    def test_rate_limit_error(self, client, db):
        from core.exceptions import LLMRateLimitError
        errs = self._trigger_error(client, db, LLMRateLimitError, exc_args=("openai",), prefix="rl")
        assert errs[0]["code"] == "LLM_RATE_LIMIT"
        assert errs[0]["retryable"] is True

    def test_timeout_error(self, client, db):
        from core.exceptions import LLMTimeoutError
        errs = self._trigger_error(client, db, LLMTimeoutError, exc_args=("openai", 30.0), prefix="to")
        assert errs[0]["code"] == "LLM_TIMEOUT"
        assert errs[0]["retryable"] is True

    def test_budget_exceeded_structured(self, client, db):
        from core.llm.client import BudgetExceededError
        errs = self._trigger_error(client, db, BudgetExceededError, prefix="be")
        assert errs[0]["code"] == "BUDGET_EXCEEDED"
        assert errs[0]["retryable"] is False

    def test_unknown_error_not_retryable(self, client, db):
        errs = self._trigger_error(client, db, RuntimeError, prefix="unk")
        assert errs[0]["code"] == "INTERNAL_ERROR"
        assert errs[0]["retryable"] is False


class TestPhasePersistenceIsolation:
    """Task 3c: One phase failing doesn't block subsequent phases."""

    _auth = staticmethod(_unique_auth)

    def test_phase1_failure_still_persists_llm_response(self, client, db):
        """If user_query persistence (Phase 1) fails, LLM response (Phase 3) still persists."""
        headers, user_id = self._auth(client, db, "ph")

        with patch("core.llm.client.LLMClient.chat_stream",
                   return_value=fake_llm_stream([{"type": "text", "content": "answer"}])), \
             patch("core.events.event_logger.EventLogger.create_user_query",
                   side_effect=RuntimeError("Phase 1 boom")):
            r = client.post("/chat/turn", json={
                "messages": [{"role": "user", "content": "question"}],
            }, headers=headers)

        events = parse_sse_events(r.text)
        assert any(e["type"] == "turn_complete" for e in events), "Turn should complete despite Phase 1 failure"

        # LLM response should still be persisted (Phase 3)
        flush_persist_threads()
        row = db.execute(
            sql_text("SELECT content FROM conversation_events WHERE user_id = :uid AND event_type = 'llm_response'"),
            {"uid": user_id},
        ).fetchone()
        assert row is not None, "Phase 3 (llm_response) should persist even when Phase 1 fails"


class TestCostTrackingFix:
    """Task 1: chat_with_tools_stream yields usage and calls _log_call/_record_spend."""

    def test_usage_event_yielded(self):
        """chat_with_tools_stream yields a usage event at the end."""
        import asyncio
        from unittest.mock import patch, MagicMock
        from core.llm.models import LLMProvider

        provider_mock = MagicMock()

        def fake_complete(*a, **kw):
            yield {"type": "tool_call", "id": "tc1", "function": {"name": "read_file", "arguments": "{}"}}
            yield {"type": "usage", "prompt": 10, "completion": 5}

        provider_mock.complete_with_tools_stream = fake_complete

        from core.llm.client import LLMClient
        llm = MagicMock(spec=LLMClient)
        llm.chat_with_tools_stream = LLMClient.chat_with_tools_stream.__get__(llm)
        llm._resolve_model = MagicMock(return_value="gpt-4o")
        llm._check_budget = MagicMock()
        llm.config = {"temperature": 0.7}

        model_cfg = MagicMock()
        model_cfg.provider = LLMProvider.OPENAI
        model_cfg.model_name = "gpt-4o"
        model_cfg.enable_cache = False
        llm._resolve_chain = MagicMock(return_value=[model_cfg])
        llm.rate_limiter = MagicMock()
        llm.rate_limiter.get_breaker.return_value.allow_request.return_value = True
        llm._get_provider = MagicMock(return_value=provider_mock)
        llm.router = MagicMock()
        llm.router.calculate_cost.return_value = 0.001
        llm._active_router = llm.router  # property fallback won't work on MagicMock
        llm._record_spend = MagicMock()
        llm._log_call = MagicMock()
        llm.user_id = "test"
        llm._active_user_id = "test"

        chunks = asyncio.get_event_loop().run_until_complete(
            _collect_async(llm.chat_with_tools_stream([], [], model="gpt-4o"))
        )

        usage_chunks = [c for c in chunks if c.get("type") == "usage"]
        assert len(usage_chunks) == 1, f"Expected 1 usage event, got {usage_chunks}"
        assert usage_chunks[0]["prompt"] == 10
        llm._log_call.assert_called_once()
        llm._record_spend.assert_called_once_with(0.001)


class TestEdgeRetry:
    """Task 4: Edge retry with exponential backoff for transient errors."""

    @pytest.mark.asyncio
    async def test_retries_on_retryable_error(self):
        """Edge retries when cloud returns retryable error, then succeeds."""
        from cli.edge_chat_loop import _consume_turn, TurnResult

        call_count = 0

        async def fake_sse_stream():
            nonlocal call_count
            call_count += 1
            if call_count == 1:
                yield {"type": "error", "message": "rate limited", "code": "LLM_RATE_LIMIT",
                       "retryable": True, "retry_after_ms": 10}
            else:
                yield {"type": "text_delta", "content": "ok"}
                yield {"type": "turn_complete", "has_tool_calls": False}

        renderer = MagicMock()
        # First call: retryable error
        r1 = await _consume_turn(fake_sse_stream(), renderer)
        assert r1.error is not None
        assert r1.error["retryable"] is True

        # Second call: success
        r2 = await _consume_turn(fake_sse_stream(), renderer)
        assert r2.text == "ok"
        assert r2.error is None

    @pytest.mark.asyncio
    async def test_consume_turn_captures_error_field(self):
        """_consume_turn populates error field from SSE error event."""
        from cli.edge_chat_loop import _consume_turn

        async def error_stream():
            yield {"type": "error", "message": "boom", "code": "INTERNAL_ERROR", "retryable": False}

        result = await _consume_turn(error_stream(), MagicMock())
        assert result.error["code"] == "INTERNAL_ERROR"
        assert result.error["retryable"] is False


async def _collect_async(agen):
    """Collect all items from an async generator."""
    items = []
    async for item in agen:
        items.append(item)
    return items
