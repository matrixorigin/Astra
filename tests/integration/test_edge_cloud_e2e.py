"""Edge→Cloud /chat/turn: ToolRegistry selection + DB ground truth.

Covers gaps identified in the refactoring session:
  1. ToolRegistry selection results within /chat/turn path
  2. skill_selection_events field completeness
  3. Cloud skill execution through the full path
  4. Multi-turn causal chain continuity
  5. Session state (event_count, last_active_at) after turn
"""

import json
import os
from uuid import uuid4

import pytest
from fastapi.testclient import TestClient
from sqlalchemy import text as sql_text
from unittest.mock import patch, AsyncMock

os.environ.setdefault("TOKEN_ENCRYPTION_KEY", "test-key-" + "x" * 32)
os.environ.setdefault("JWT_SECRET_KEY", "test-jwt-secret-" + "x" * 32)

from api.main import app
from api.database import SessionLocal
from tests.conftest import (
    parse_sse_events,
    fake_llm_stream,
    get_auth_headers,
    flush_persist_threads,
)


@pytest.fixture
def client():
    return TestClient(app)


def _unique_auth(client, db, prefix="ec"):
    uid = uuid4().hex
    user_id = str(uuid4())
    headers = get_auth_headers(
        client,
        db,
        username=f"{prefix}_{uid}",
        user_id=user_id,
        email=f"{prefix}_{uid}@test.com",
        password="pass123",
    )
    return headers, user_id


_TOOLS = (
    {
        "type": "function",
        "function": {
            "name": "bash",
            "description": "Run shell command",
            "parameters": {
                "type": "object",
                "properties": {"command": {"type": "string"}},
                "required": ["command"],
            },
        },
    },
    {
        "type": "function",
        "function": {
            "name": "read_file",
            "description": "Read file content",
            "parameters": {
                "type": "object",
                "properties": {"path": {"type": "string"}},
                "required": ["path"],
            },
        },
    },
    {
        "type": "function",
        "function": {
            "name": "grep",
            "description": "Search text patterns",
            "parameters": {
                "type": "object",
                "properties": {"pattern": {"type": "string"}},
                "required": ["pattern"],
            },
        },
    },
)


# ── Layer 1: ToolRegistry selects correct tools in /chat/turn ────


class TestToolRegistryInChatTurn:
    def _auth(self, client, db):
        return _unique_auth(client, db, "tr_sel")

    def test_edge_tools_reach_llm(self, client, db):
        """Edge tools sent in request appear in the tools passed to LLM."""
        headers, _ = self._auth(client, db)
        captured = {}

        orig_stream = None

        async def _capture_stream(messages, tools, **kw):
            captured["tools"] = tools
            for c in [{"type": "text", "content": "ok"}]:
                yield c

        # Use a user query that explicitly names all tools to ensure they are selected
        with patch("core.llm.client.LLMClient.chat_with_tools_stream", side_effect=_capture_stream):
            r = client.post(
                "/chat/turn",
                json={
                    "messages": [
                        {"role": "user", "content": "use bash, read_file, and grep tools"}
                    ],
                    "edge_tools": list(_TOOLS),
                },
                headers=headers,
            )

        assert r.status_code == 200
        events = parse_sse_events(r.text)
        assert any(e["type"] == "session_info" for e in events)

        tool_names = {t["function"]["name"] for t in (captured.get("tools") or [])}
        assert {"bash", "read_file", "grep"}.issubset(tool_names)

    def test_cloud_skills_merged_with_edge(self, client, db):
        """Cloud skills are merged with edge tools — total > edge count."""
        headers, _ = self._auth(client, db)
        captured = {}

        async def _capture_stream(messages, tools, **kw):
            captured["tools"] = tools
            for c in [{"type": "text", "content": "ok"}]:
                yield c

        # Use a user query that explicitly names tools to ensure they are selected
        with patch("core.llm.client.LLMClient.chat_with_tools_stream", side_effect=_capture_stream):
            r = client.post(
                "/chat/turn",
                json={
                    "messages": [
                        {"role": "user", "content": "use bash, read_file, and grep tools"}
                    ],
                    "edge_tools": list(_TOOLS),
                },
                headers=headers,
            )

        assert r.status_code == 200
        tool_names = {t["function"]["name"] for t in (captured.get("tools") or [])}
        # Edge pinned tools present
        assert "bash" in tool_names
        # Cloud skills added → total exceeds edge-only count
        assert len(tool_names) > len(_TOOLS)


# ── Layer 2: DB ground truth for agent_events ────────────────────


class TestEventPersistenceGroundTruth:
    def _auth(self, client, db):
        return _unique_auth(client, db, "evt_gt")

    def test_user_query_event_fields(self, client, db):
        """user_query event: all fields verified after /chat/turn."""
        headers, uid = self._auth(client, db)

        with patch(
            "core.llm.client.LLMClient.chat_with_tools_stream",
            return_value=fake_llm_stream(
                [
                    {"type": "text", "content": "Event sourcing stores changes as events."},
                ]
            ),
        ):
            r = client.post(
                "/chat/turn",
                json={
                    "messages": [{"role": "user", "content": "what is event sourcing?"}],
                    "edge_tools": list(_TOOLS),
                },
                headers=headers,
            )

        events = parse_sse_events(r.text)
        sid = next(e["session_id"] for e in events if e["type"] == "session_info")
        flush_persist_threads()

        row = (
            db.execute(
                sql_text(
                    "SELECT * FROM agent_events "
                    "WHERE session_id = :sid AND event_type = 'user_query' "
                    "ORDER BY created_at DESC LIMIT 1"
                ),
                {"sid": sid},
            )
            .mappings()
            .first()
        )

        assert row is not None
        assert row["session_id"] == sid
        assert row["user_id"] == uid
        assert row["event_type"] == "user_query"
        assert row["content"] == "what is event sourcing?"
        assert row["causal_chain_id"] is not None
        assert row["parent_event_id"] is None  # root event
        assert row["created_at"] is not None

    def test_llm_response_links_to_user_query(self, client, db):
        """llm_response event shares causal_chain_id and parent_event_id with user_query."""
        headers, _ = self._auth(client, db)

        with patch(
            "core.llm.client.LLMClient.chat_with_tools_stream",
            return_value=fake_llm_stream(
                [
                    {"type": "text", "content": "It stores state changes as events."},
                ]
            ),
        ):
            r = client.post(
                "/chat/turn",
                json={
                    "messages": [{"role": "user", "content": "explain event sourcing"}],
                    "edge_tools": list(_TOOLS),
                },
                headers=headers,
            )

        sid = next(e["session_id"] for e in parse_sse_events(r.text) if e["type"] == "session_info")
        flush_persist_threads()

        uq = (
            db.execute(
                sql_text(
                    "SELECT event_id, causal_chain_id FROM agent_events "
                    "WHERE session_id = :sid AND event_type = 'user_query' "
                    "ORDER BY created_at DESC LIMIT 1"
                ),
                {"sid": sid},
            )
            .mappings()
            .first()
        )

        lr = (
            db.execute(
                sql_text(
                    "SELECT * FROM agent_events "
                    "WHERE session_id = :sid AND event_type = 'llm_response' "
                    "ORDER BY created_at DESC LIMIT 1"
                ),
                {"sid": sid},
            )
            .mappings()
            .first()
        )

        assert lr is not None
        assert lr["parent_event_id"] == uq["event_id"]
        assert lr["causal_chain_id"] == uq["causal_chain_id"]
        assert "state changes" in lr["content"]

    def test_tool_call_event_fields(self, client, db):
        """tool_call event records function name, arguments, and skill_name."""
        headers, _ = self._auth(client, db)

        with patch(
            "core.llm.client.LLMClient.chat_with_tools_stream",
            return_value=fake_llm_stream(
                [
                    {
                        "type": "tool_call",
                        "data": {
                            "id": "tc_ec1",
                            "type": "function",
                            "function": {"name": "bash", "arguments": '{"command": "ls"}'},
                        },
                    },
                ]
            ),
        ):
            r = client.post(
                "/chat/turn",
                json={
                    "messages": [{"role": "user", "content": "list current directory"}],
                    "edge_tools": list(_TOOLS),
                },
                headers=headers,
            )

        sid = next(e["session_id"] for e in parse_sse_events(r.text) if e["type"] == "session_info")
        flush_persist_threads()

        tc = (
            db.execute(
                sql_text(
                    "SELECT * FROM agent_events "
                    "WHERE session_id = :sid AND event_type = 'tool_call' "
                    "ORDER BY created_at DESC LIMIT 1"
                ),
                {"sid": sid},
            )
            .mappings()
            .first()
        )

        assert tc is not None
        content = json.loads(tc["content"])
        assert content["name"] == "bash"
        assert '"ls"' in content["arguments"]
        assert tc["skill_name"] == "bash"
        assert tc["causal_chain_id"] is not None


# ── Layer 3: skill_selection_events ground truth ─────────────────


class TestSkillSelectionEventGroundTruth:
    def _auth(self, client, db):
        return _unique_auth(client, db, "sse_gt")

    def test_selection_event_written_on_tool_call(self, client, db):
        """A turn with tool_calls writes a skill_selection_events row with correct fields."""
        headers, _ = self._auth(client, db)

        with patch(
            "core.llm.client.LLMClient.chat_with_tools_stream",
            return_value=fake_llm_stream(
                [
                    {
                        "type": "tool_call",
                        "data": {
                            "id": "tc_sse1",
                            "type": "function",
                            "function": {"name": "read_file", "arguments": '{"path": "/tmp/x"}'},
                        },
                    },
                ]
            ),
        ):
            r = client.post(
                "/chat/turn",
                json={
                    "messages": [{"role": "user", "content": "read /tmp/x"}],
                    "edge_tools": list(_TOOLS),
                },
                headers=headers,
            )

        sid = next(e["session_id"] for e in parse_sse_events(r.text) if e["type"] == "session_info")
        flush_persist_threads()

        row = (
            db.execute(
                sql_text(
                    "SELECT * FROM skill_selection_events "
                    "WHERE session_id = :sid ORDER BY created_at DESC LIMIT 1"
                ),
                {"sid": sid},
            )
            .mappings()
            .first()
        )

        assert row is not None
        assert row["session_id"] == sid
        assert row["event_id"] is not None
        assert row["user_query"] == "read /tmp/x"
        assert row["skill_name"] == "read_file"
        assert row["selection_method"] == "llm_tool_choice"
        # selected_skills is JSON list containing the tool name
        selected = row["selected_skills"]
        if isinstance(selected, str):
            selected = json.loads(selected)
        assert "read_file" in selected
        assert row["created_at"] is not None

    def test_no_selection_event_on_text_only(self, client, db):
        """A turn with only text (no tool_calls) does NOT write skill_selection_events."""
        headers, _ = self._auth(client, db)

        with patch(
            "core.llm.client.LLMClient.chat_with_tools_stream",
            return_value=fake_llm_stream(
                [
                    {"type": "text", "content": "Hello!"},
                ]
            ),
        ):
            r = client.post(
                "/chat/turn",
                json={
                    "messages": [{"role": "user", "content": "hi"}],
                    "edge_tools": list(_TOOLS),
                },
                headers=headers,
            )

        sid = next(e["session_id"] for e in parse_sse_events(r.text) if e["type"] == "session_info")
        flush_persist_threads()

        row = db.execute(
            sql_text("SELECT COUNT(*) FROM skill_selection_events WHERE session_id = :sid"),
            {"sid": sid},
        ).scalar()
        assert row == 0


# ── Layer 4: session state updated ───────────────────────────────


class TestSessionStateUpdate:
    def _auth(self, client, db):
        return _unique_auth(client, db, "sess_st")

    def test_event_count_and_last_active(self, client, db):
        """agent_sessions.event_count >= 2 and last_active_at set after a turn."""
        headers, _ = self._auth(client, db)

        # Use a user query that ensures tools are selected, and patch both chat methods
        with (
            patch(
                "core.llm.client.LLMClient.chat_with_tools_stream",
                return_value=fake_llm_stream(
                    [
                        {"type": "text", "content": "done"},
                    ]
                ),
            ),
            patch(
                "core.llm.client.LLMClient.chat_stream",
                return_value=fake_llm_stream(
                    [
                        {"type": "text", "content": "done"},
                    ]
                ),
            ),
        ):
            r = client.post(
                "/chat/turn",
                json={
                    "messages": [
                        {"role": "user", "content": "use bash, read_file, and grep tools"}
                    ],
                    "edge_tools": list(_TOOLS),
                },
                headers=headers,
            )

        sid = next(e["session_id"] for e in parse_sse_events(r.text) if e["type"] == "session_info")
        flush_persist_threads()

        fresh_db = SessionLocal()
        try:
            row = (
                fresh_db.execute(
                    sql_text(
                        "SELECT event_count, last_active_at FROM agent_sessions WHERE session_id = :sid"
                    ),
                    {"sid": sid},
                )
                .mappings()
                .first()
            )
        finally:
            fresh_db.close()

        assert row is not None
        assert row["event_count"] >= 2  # user_query + llm_response
        assert row["last_active_at"] is not None


# ── Layer 5: multi-turn causal chain ─────────────────────────────


class TestMultiTurnCausalChain:
    def _auth(self, client, db):
        return _unique_auth(client, db, "chain")

    def test_continuation_shares_chain(self, client, db):
        """Tool result continuation reuses the same causal_chain_id."""
        headers, _ = self._auth(client, db)

        # Turn 1: LLM returns tool_call
        with patch(
            "core.llm.client.LLMClient.chat_with_tools_stream",
            return_value=fake_llm_stream(
                [
                    {
                        "type": "tool_call",
                        "data": {
                            "id": "tc_chain1",
                            "type": "function",
                            "function": {"name": "bash", "arguments": '{"command": "ls"}'},
                        },
                    },
                ]
            ),
        ):
            r1 = client.post(
                "/chat/turn",
                json={
                    "messages": [{"role": "user", "content": "list files"}],
                    "edge_tools": list(_TOOLS),
                },
                headers=headers,
            )

        sid = next(
            e["session_id"] for e in parse_sse_events(r1.text) if e["type"] == "session_info"
        )
        flush_persist_threads()

        chain1 = db.execute(
            sql_text(
                "SELECT causal_chain_id FROM agent_events "
                "WHERE session_id = :sid AND event_type = 'user_query' "
                "ORDER BY created_at DESC LIMIT 1"
            ),
            {"sid": sid},
        ).scalar()

        # Turn 2: continuation with tool_result → LLM returns text
        with patch(
            "core.llm.client.LLMClient.chat_with_tools_stream",
            return_value=fake_llm_stream(
                [
                    {"type": "text", "content": "Directory listing complete."},
                ]
            ),
        ):
            client.post(
                "/chat/turn",
                json={
                    "messages": [],
                    "session_id": sid,
                    "tool_results": [
                        {
                            "tool_call_id": "tc_chain1",
                            "name": "bash",
                            "result": "file1.py\nfile2.py",
                        }
                    ],
                },
                headers=headers,
            )

        flush_persist_threads()

        chain2 = db.execute(
            sql_text(
                "SELECT causal_chain_id FROM agent_events "
                "WHERE session_id = :sid AND event_type = 'llm_response' "
                "ORDER BY created_at DESC LIMIT 1"
            ),
            {"sid": sid},
        ).scalar()

        assert chain1 is not None
        assert chain2 == chain1

    def test_new_query_starts_new_chain(self, client, db):
        """A new user query in the same session gets a fresh causal_chain_id."""
        headers, _ = self._auth(client, db)

        # Turn 1
        with patch(
            "core.llm.client.LLMClient.chat_with_tools_stream",
            return_value=fake_llm_stream([{"type": "text", "content": "ok"}]),
        ):
            r1 = client.post(
                "/chat/turn",
                json={
                    "messages": [{"role": "user", "content": "turn 1"}],
                    "edge_tools": list(_TOOLS),
                },
                headers=headers,
            )

        sid = next(
            e["session_id"] for e in parse_sse_events(r1.text) if e["type"] == "session_info"
        )
        flush_persist_threads()

        chain1 = db.execute(
            sql_text(
                "SELECT causal_chain_id FROM agent_events "
                "WHERE session_id = :sid AND event_type = 'user_query' "
                "ORDER BY created_at ASC LIMIT 1"
            ),
            {"sid": sid},
        ).scalar()

        # Turn 2: new user query
        with patch(
            "core.llm.client.LLMClient.chat_with_tools_stream",
            return_value=fake_llm_stream([{"type": "text", "content": "ok2"}]),
        ):
            client.post(
                "/chat/turn",
                json={
                    "messages": [{"role": "user", "content": "turn 2"}],
                    "session_id": sid,
                    "edge_tools": list(_TOOLS),
                },
                headers=headers,
            )

        flush_persist_threads()

        chain2 = db.execute(
            sql_text(
                "SELECT causal_chain_id FROM agent_events "
                "WHERE session_id = :sid AND event_type = 'user_query' "
                "ORDER BY created_at DESC LIMIT 1"
            ),
            {"sid": sid},
        ).scalar()

        assert chain1 is not None
        assert chain2 is not None
        assert chain1 != chain2


# ── Layer 6: cloud skill execution ───────────────────────────────


class TestCloudSkillExecution:
    def _auth(self, client, db):
        return _unique_auth(client, db, "cloud")

    def test_cloud_skill_result_persisted(self, client, db):
        """Cloud skill execution writes tool_result with source=cloud metadata."""
        headers, _ = self._auth(client, db)
        call_count = [0]

        async def _two_phase(messages, tools, **kw):
            call_count[0] += 1
            if call_count[0] == 1:
                yield {
                    "type": "tool_call",
                    "data": {
                        "id": "tc_cloud1",
                        "type": "function",
                        "function": {
                            "name": "list_prs",
                            "arguments": json.dumps({"repo": "test/repo"}),
                        },
                    },
                }
            else:
                yield {"type": "text", "content": "Found 3 PRs"}

        with (
            patch("core.llm.client.LLMClient.chat_with_tools_stream", side_effect=_two_phase),
            patch("api.routers.chat._execute_cloud_skill", new_callable=AsyncMock) as mock_exec,
        ):
            mock_exec.return_value = json.dumps({"prs": [{"number": 1, "title": "Fix bug"}]})

            r = client.post(
                "/chat/turn",
                json={
                    "messages": [{"role": "user", "content": "show PRs for test/repo"}],
                    "edge_tools": list(_TOOLS),
                },
                headers=headers,
            )

        assert r.status_code == 200
        sid = next(e["session_id"] for e in parse_sse_events(r.text) if e["type"] == "session_info")
        flush_persist_threads()

        rows = (
            db.execute(
                sql_text(
                    "SELECT content, metadata FROM agent_events "
                    "WHERE session_id = :sid AND event_type = 'tool_result' "
                    "ORDER BY created_at"
                ),
                {"sid": sid},
            )
            .mappings()
            .all()
        )

        cloud_results = [r for r in rows if r["metadata"] and "cloud" in str(r["metadata"])]
        assert len(cloud_results) >= 1, f"Expected cloud tool_result, got {len(rows)} total"
        # Verify the content contains the skill name
        content = json.loads(cloud_results[0]["content"])
        assert content["name"] == "list_prs"

    def test_cloud_tool_call_has_source_cloud(self, client, db):
        """Cloud tool_call event content includes source=cloud."""
        headers, _ = self._auth(client, db)
        call_count = [0]

        async def _two_phase(messages, tools, **kw):
            call_count[0] += 1
            if call_count[0] == 1:
                yield {
                    "type": "tool_call",
                    "data": {
                        "id": "tc_cloud2",
                        "type": "function",
                        "function": {"name": "list_prs", "arguments": '{"repo": "o/r"}'},
                    },
                }
            else:
                yield {"type": "text", "content": "Done"}

        with (
            patch("core.llm.client.LLMClient.chat_with_tools_stream", side_effect=_two_phase),
            patch("api.routers.chat._execute_cloud_skill", new_callable=AsyncMock) as mock_exec,
        ):
            mock_exec.return_value = json.dumps({"success": True, "result": "ok"})

            r = client.post(
                "/chat/turn",
                json={
                    "messages": [{"role": "user", "content": "list PRs"}],
                    "edge_tools": list(_TOOLS),
                },
                headers=headers,
            )

        sid = next(e["session_id"] for e in parse_sse_events(r.text) if e["type"] == "session_info")
        flush_persist_threads()

        tc = (
            db.execute(
                sql_text(
                    "SELECT content FROM agent_events "
                    "WHERE session_id = :sid AND event_type = 'tool_call' AND skill_name = 'list_prs' "
                    "ORDER BY created_at DESC LIMIT 1"
                ),
                {"sid": sid},
            )
            .mappings()
            .first()
        )

        assert tc is not None
        content = json.loads(tc["content"])
        assert content.get("source") == "cloud"
