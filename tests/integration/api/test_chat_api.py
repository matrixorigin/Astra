"""Integration tests for /chat API — TestClient + real DB, only mock LLM/engine."""

import pytest
from unittest.mock import AsyncMock, MagicMock, patch
from uuid import uuid4

from fastapi.testclient import TestClient

from api.main import app
from api.database import get_db_session
from api.repositories.user_repository import UserRepository
from core.agent.run import AgentRun, RunStatus
from core.events.models import StreamEvent, StreamEventType
from tests.conftest import parse_sse_events


@pytest.fixture
def client():
    return TestClient(app)


@pytest.fixture
def db_session():
    session = next(get_db_session())
    yield session
    session.close()


# auth_headers fixture now provided by tests/integration/conftest.py


def _mock_engine(reply="Hello back!", status=RunStatus.PENDING):
    """Return a mock RunEngine."""
    engine = MagicMock()

    def _create_run(**kwargs):
        run = AgentRun(
            session_id=kwargs.get("session_id", "test"),
            user_id=kwargs.get("user_id", "test"),
            user_input=kwargs.get("user_input", "hi"),
        )
        run.status = status
        engine.get_run.return_value = run
        engine.restore_run.return_value = run
        return run

    engine.create_run.side_effect = _create_run
    # Default for get/restore before create_run is called
    default_run = AgentRun(session_id="test", user_id="test", user_input="hi")
    default_run.status = status
    engine.get_run.return_value = default_run
    engine.restore_run.return_value = default_run
    engine.get_agent_run_events.return_value = [
        {"event_type": "text_done", "data": {"text": reply}},
    ]
    engine.cancel_run.return_value = True

    async def stream_events(run_id, last_index=0):
        yield {"event_type": "text_delta", "data": {"chunk": reply}, "run_id": run_id}
        yield {"event_type": "run_finished", "data": {}, "run_id": run_id}

    engine.stream_agent_run_events = stream_events
    engine.start_run = AsyncMock()
    return engine


class TestChatAPI:
    """Integration: auth → route → DB session creation → response."""

    @patch("api.routers.chat._get_engine")
    def test_chat_auto_create_session(self, mock_get_engine, client, auth_headers):
        mock_get_engine.return_value = _mock_engine()
        resp = client.post("/chat", json={"message": "hi"}, headers=auth_headers)
        assert resp.status_code == 200
        data = resp.json()
        assert data["session_id"]
        assert data["run_id"]
        assert data["status"] == "pending"

    @patch("api.routers.chat._get_engine")
    def test_chat_with_existing_session(self, mock_get_engine, client, auth_headers, db_session, test_user):
        from core.events.session_manager import SessionManager
        mgr = SessionManager(db_session)
        session = mgr.create_session(user_id=test_user.user_id)

        mock_get_engine.return_value = _mock_engine("reply2")
        resp = client.post("/chat", json={"session_id": session.session_id, "message": "hi"}, headers=auth_headers)
        assert resp.status_code == 200
        assert resp.json()["session_id"] == session.session_id

    def test_chat_no_auth(self, client):
        resp = client.post("/chat", json={"message": "hi"})
        assert resp.status_code == 401

    @patch("api.routers.chat._get_engine")
    def test_chat_nonexistent_session(self, mock_get_engine, client, auth_headers):
        resp = client.post("/chat", json={"session_id": "no_such_id", "message": "hi"}, headers=auth_headers)
        assert resp.status_code == 404

    @patch("api.routers.chat._get_engine")
    def test_stream_returns_sse(self, mock_get_engine, client, auth_headers):
        mock_get_engine.return_value = _mock_engine("streamed")
        resp = client.post("/chat/stream", json={"message": "hi"}, headers=auth_headers)
        assert resp.status_code == 200
        assert "text/event-stream" in resp.headers["content-type"]
        assert "session_info" in resp.text
        assert "run_id" in resp.text

    @patch("api.routers.chat._get_engine")
    def test_get_run_status(self, mock_get_engine, client, auth_headers, test_user):
        engine = _mock_engine()
        # Make the mock run match the authenticated user
        run = AgentRun(session_id="test", user_id=test_user.user_id, user_input="hi")
        engine.get_run.return_value = run
        engine.restore_run.return_value = run
        mock_get_engine.return_value = engine
        resp = client.get("/chat/runs/test_run_123", headers=auth_headers)
        assert resp.status_code == 200
        data = resp.json()
        assert data["status"] == "pending"

    @patch("api.routers.chat._get_engine")
    def test_cancel_run(self, mock_get_engine, client, auth_headers, test_user):
        engine = _mock_engine()
        run = AgentRun(session_id="test", user_id=test_user.user_id, user_input="hi")
        run.status = RunStatus.RUNNING
        engine.get_run.return_value = run
        engine.restore_run.return_value = run
        mock_get_engine.return_value = engine
        resp = client.delete("/chat/runs/test_run_123", headers=auth_headers)
        assert resp.status_code == 200
        assert resp.json()["status"] == "cancelled"

    @patch("api.routers.chat._get_engine")
    def test_cancel_run_wrong_user(self, mock_get_engine, client, auth_headers):
        """Cannot cancel another user's run."""
        engine = _mock_engine()
        run = AgentRun(session_id="test", user_id="other_user_id", user_input="hi")
        run.status = RunStatus.RUNNING
        engine.get_run.return_value = run
        engine.restore_run.return_value = run
        mock_get_engine.return_value = engine
        resp = client.delete("/chat/runs/test_run_123", headers=auth_headers)
        assert resp.status_code == 403

    @patch("api.routers.chat._get_engine")
    def test_get_run_status_wrong_user(self, mock_get_engine, client, auth_headers):
        """Cannot view another user's run."""
        engine = _mock_engine()
        run = AgentRun(session_id="test", user_id="other_user_id", user_input="hi")
        engine.get_run.return_value = run
        engine.restore_run.return_value = run
        mock_get_engine.return_value = engine
        resp = client.get("/chat/runs/test_run_123", headers=auth_headers)
        assert resp.status_code == 403

    @patch("api.routers.chat._get_engine")
    def test_cancel_finished_run_returns_409(self, mock_get_engine, client, auth_headers, test_user):
        """Cancelling an already-finished run returns 409."""
        engine = _mock_engine()
        run = AgentRun(session_id="test", user_id=test_user.user_id, user_input="hi")
        run.status = RunStatus.COMPLETED
        engine.get_run.return_value = run
        engine.restore_run.return_value = run
        engine.cancel_run.return_value = False  # Already finished
        mock_get_engine.return_value = engine
        resp = client.delete("/chat/runs/test_run_123", headers=auth_headers)
        assert resp.status_code == 409


class TestSSEProtocolCompliance:
    """SSE endpoints return text/event-stream errors, not JSON."""

    def test_stream_nonexistent_session_returns_sse_error(self, client, auth_headers):
        """POST /chat/stream with bad session_id → SSE error event."""
        resp = client.post("/chat/stream", json={"session_id": "no_such_id", "message": "hi"}, headers=auth_headers)
        assert resp.status_code == 200
        assert "text/event-stream" in resp.headers["content-type"]
        events = parse_sse_events(resp.text)
        assert any(e.get("type") == "error" and "not found" in e.get("message", "").lower() for e in events)

    @patch("api.routers.chat._get_engine")
    def test_stream_run_not_found_returns_sse_error(self, mock_get_engine, client, auth_headers):
        """GET /chat/runs/{run_id}/stream with bad run_id → SSE error event."""
        engine = _mock_engine()
        engine.get_run.return_value = None
        engine.restore_run.return_value = None
        mock_get_engine.return_value = engine
        resp = client.get("/chat/runs/nonexistent_run/stream", headers=auth_headers)
        assert resp.status_code == 200
        assert "text/event-stream" in resp.headers["content-type"]
        events = parse_sse_events(resp.text)
        assert any(e.get("type") == "error" and e.get("code") == "NOT_FOUND" for e in events)

    @patch("api.routers.chat._get_engine")
    def test_stream_run_wrong_user_returns_sse_error(self, mock_get_engine, client, auth_headers):
        """GET /chat/runs/{run_id}/stream for another user's run → SSE error."""
        engine = _mock_engine()
        run = AgentRun(session_id="test", user_id="other_user_id", user_input="hi")
        engine.get_run.return_value = run
        engine.restore_run.return_value = run
        mock_get_engine.return_value = engine
        resp = client.get("/chat/runs/test_run/stream", headers=auth_headers)
        assert resp.status_code == 200
        assert "text/event-stream" in resp.headers["content-type"]
        events = parse_sse_events(resp.text)
        assert any(e.get("type") == "error" and e.get("code") == "AUTH_ERROR" for e in events)
