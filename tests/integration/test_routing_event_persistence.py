"""Integration tests for routing event persistence + explain trace.

Verifies:
  1. routing_decision event persisted to conversation_events with all fields
  2. explain SSE contains full routing metadata
  3. model_override skips routing (no event, explain shows skipped)
  4. Causal chain links user_query → routing_decision → llm_response
"""

import json
import os
import pytest
from unittest.mock import patch

from fastapi.testclient import TestClient
from sqlalchemy import text as sql_text

os.environ.setdefault("TOKEN_ENCRYPTION_KEY", "test-key-" + "x" * 32)
os.environ.setdefault("JWT_SECRET_KEY", "test-jwt-secret-" + "x" * 32)

from api.database import get_db_session
from api.main import app
from api.models import User
from api.models.agent import Event as EventModel
from core.auth.jwt_manager import create_access_token
from core.auth.password import hash_password
from tests.integration.helpers import fake_stream_gen, parse_sse, get_session_id
from tests.conftest import flush_persist_threads


@pytest.fixture
def client(db_session):
    def override_get_db():
        try:
            yield db_session
        finally:
            pass

    app.dependency_overrides[get_db_session] = override_get_db
    try:
        yield TestClient(app)
    finally:
        app.dependency_overrides.pop(get_db_session, None)


@pytest.fixture
def auth_headers(client, db_session):
    user = db_session.query(User).filter(User.username == "routing_event_user").first()
    if not user:
        user = User(
            user_id="routing_evt_user",
            username="routing_event_user",
            email="routing_evt@test.com",
            password_hash=hash_password("password123"),
        )
        db_session.add(user)
        db_session.commit()
    token = create_access_token({"sub": user.user_id, "username": user.username})
    return {"Authorization": f"Bearer {token}"}


@pytest.fixture(autouse=True)
def clear_caches():
    from api.routers import chat

    chat._session_cache.clear()
    yield
    chat._session_cache.clear()


def _chat_turn(client, auth_headers, message, **extra):
    """Send a /chat/turn request and return (events, response)."""

    async def mock_stream(messages, *args, **kwargs):
        async for chunk in fake_stream_gen([{"type": "text", "content": "ok"}]):
            yield chunk

    with patch("core.llm.client.LLMClient.chat_stream", side_effect=mock_stream):
        resp = client.post(
            "/chat/turn",
            json={
                "messages": [{"role": "user", "content": message}],
                **extra,
            },
            headers=auth_headers,
        )
    assert resp.status_code == 200
    return parse_sse(resp.text), resp


class TestRoutingEventPersistence:
    """Verify routing_decision event is persisted with correct fields."""

    def test_routing_event_persisted_all_fields(self, client, auth_headers, db_session):
        """Normal routing → routing_decision event in DB with all metadata."""
        events, resp = _chat_turn(client, auth_headers, "记住我用vim")
        session_id = get_session_id(resp.text)
        flush_persist_threads()

        # Re-query DB for routing_decision event
        row = (
            db_session.query(EventModel)
            .filter(
                EventModel.session_id == session_id,
                EventModel.event_type == "routing_decision",
            )
            .order_by(EventModel.created_at.desc())
            .first()
        )
        assert row is not None, "routing_decision event not found in DB"
        assert row.event_type == "routing_decision"
        assert row.session_id == session_id
        assert row.user_id is not None
        assert row.created_at is not None

        # Verify content JSON has all routing fields
        content = json.loads(row.content) if isinstance(row.content, str) else row.content
        assert content["router"] == "default"
        assert content["intent"] in ("preference", "command", "feedback", "question")
        assert 0.0 <= content["confidence"] <= 1.0
        assert content["tier"] in (0, 1)
        assert content["matched_by"] in ("regex", "heuristic", "both", "llm", "fallback", "forced")
        assert 0.70 <= content["threshold"] <= 0.95
        assert content["latency_ms"] >= 0
        assert isinstance(content["skipped_sections"], list)
        assert content["estimated_tokens"] > 0

        # Verify metadata
        meta = (
            json.loads(row.event_metadata)
            if isinstance(row.event_metadata, str)
            else row.event_metadata
        )
        assert meta["intent"] == content["intent"]
        assert meta["tier"] == content["tier"]

        # Verify causal chain — routing_decision shares chain with user_query
        assert row.causal_chain_id is not None
        user_event = (
            db_session.query(EventModel)
            .filter(
                EventModel.session_id == session_id,
                EventModel.event_type == "user_query",
            )
            .first()
        )
        assert user_event is not None
        assert row.causal_chain_id == user_event.causal_chain_id
        assert row.parent_event_id == user_event.event_id

    def test_model_override_skips_routing_event(self, client, auth_headers, db_session):
        """model override → no routing_decision event persisted."""
        events, resp = _chat_turn(client, auth_headers, "hello", model="gpt-4")
        session_id = get_session_id(resp.text)
        flush_persist_threads()

        row = (
            db_session.query(EventModel)
            .filter(
                EventModel.session_id == session_id,
                EventModel.event_type == "routing_decision",
            )
            .first()
        )
        assert row is None, "routing_decision should NOT be persisted when model is overridden"


class TestExplainRoutingTrace:
    """Verify explain SSE event contains full routing metadata."""

    def test_explain_contains_routing_fields(self, client, auth_headers):
        """explain=true → SSE explain event has routing with all fields."""
        events, _ = _chat_turn(client, auth_headers, "run the tests", explain=True)

        explain_events = [e for e in events if e.get("type") == "explain"]
        assert len(explain_events) == 1
        explain = explain_events[0]

        assert "routing" in explain, f"explain event missing 'routing': {explain.keys()}"
        routing = explain["routing"]
        assert routing["router"] == "default"
        assert routing["intent"] in ("preference", "command", "feedback", "question")
        assert isinstance(routing["confidence"], (int, float))
        assert routing["tier"] in (0, 1)
        assert routing["matched_by"] in ("regex", "heuristic", "both", "llm", "fallback", "forced")
        assert isinstance(routing["threshold"], (int, float))
        assert routing["latency_ms"] >= 0
        assert isinstance(routing["skipped_sections"], list)
        assert routing["estimated_tokens"] > 0

    def test_explain_model_override_shows_skipped(self, client, auth_headers):
        """model override + explain → routing shows skipped reason."""
        events, _ = _chat_turn(client, auth_headers, "hello", model="gpt-4", explain=True)

        explain_events = [e for e in events if e.get("type") == "explain"]
        assert len(explain_events) == 1
        routing = explain_events[0].get("routing", {})
        assert routing.get("skipped") is True
        assert routing.get("reason") == "model_override"

    def test_explain_false_no_explain_event(self, client, auth_headers):
        """explain=false → no explain SSE event emitted."""
        events, _ = _chat_turn(client, auth_headers, "hello")
        explain_events = [e for e in events if e.get("type") == "explain"]
        assert len(explain_events) == 0
