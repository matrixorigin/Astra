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


@pytest.fixture
def client():
    return TestClient(app)


@pytest.fixture
def db_session():
    session = next(get_db_session())
    yield session
    session.close()


@pytest.fixture
def test_user(db_session):
    repo = UserRepository(db_session)
    user = repo.get_by_username("chatuser")
    if user:
        repo.delete(user.user_id)
        db_session.commit()

    from core.auth.password import hash_password
    user = repo.create({
        "user_id": str(uuid4()),
        "username": "chatuser",
        "email": "chat@example.com",
        "password_hash": hash_password("password123"),
        "is_active": 1,
    })
    yield user
    repo.delete(user.user_id)
    db_session.commit()


@pytest.fixture
def auth_headers(client, test_user):
    resp = client.post("/auth/login", json={"username": "chatuser", "password": "password123"})
    return {"Authorization": f"Bearer {resp.json()['access_token']}"}


def _mock_engine(reply="Hello back!", status=RunStatus.PENDING):
    """Return a mock RunEngine."""
    engine = MagicMock()
    run = AgentRun(session_id="test", user_id="test", user_input="hi")
    run.status = status
    engine.create_run.return_value = run
    engine.start_run_with_timeout = AsyncMock(return_value=run)
    engine.get_run_events.return_value = [
        {"event_type": "text_done", "data": {"text": reply}},
    ]
    engine.get_run.return_value = run
    engine.restore_run.return_value = None
    engine.cancel_run.return_value = True

    async def stream_events(run_id, last_index=0):
        yield {"event_type": "text_delta", "data": {"chunk": reply}, "run_id": run_id}
        yield {"event_type": "run_finished", "data": {}, "run_id": run_id}

    engine.stream_run_events = stream_events
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
        assert resp.status_code == 403

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
    def test_get_run_status(self, mock_get_engine, client, auth_headers):
        engine = _mock_engine()
        mock_get_engine.return_value = engine
        resp = client.get("/chat/runs/test_run_123", headers=auth_headers)
        assert resp.status_code == 200
        data = resp.json()
        assert data["status"] == "pending"

    @patch("api.routers.chat._get_engine")
    def test_cancel_run(self, mock_get_engine, client, auth_headers):
        mock_get_engine.return_value = _mock_engine()
        resp = client.delete("/chat/runs/test_run_123", headers=auth_headers)
        assert resp.status_code == 200
        assert resp.json()["status"] == "cancelled"
