"""Integration tests for /chat API — TestClient + real DB, only mock LLM."""

import pytest
from unittest.mock import AsyncMock, MagicMock, patch
from uuid import uuid4

from fastapi.testclient import TestClient

from api.main import app
from api.database import get_db_session
from api.repositories.user_repository import UserRepository
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


def _mock_chat_loop(reply="Hello back!"):
    """Return a mock ChatLoop that returns a fixed reply."""
    loop = MagicMock()
    loop.run_step = AsyncMock(return_value=reply)

    async def stream(*a, **kw):
        yield StreamEvent(event_type=StreamEventType.TEXT_DELTA, data={"chunk": reply}, event_id="e1")
        yield StreamEvent(event_type=StreamEventType.RUN_FINISHED, data={}, event_id="e2")

    loop.run_step_stream = stream
    loop.set_observer = MagicMock()
    return loop


class TestChatAPI:
    """Integration: auth → route → DB session creation → response."""

    @patch("api.routers.chat._build_chat_loop")
    def test_chat_auto_create_session(self, mock_build, client, auth_headers):
        mock_build.return_value = _mock_chat_loop()
        resp = client.post("/chat", json={"message": "hi"}, headers=auth_headers)
        assert resp.status_code == 200
        data = resp.json()
        assert data["session_id"]  # auto-created
        assert data["message"] == "Hello back!"

    @patch("api.routers.chat._build_chat_loop")
    def test_chat_with_existing_session(self, mock_build, client, auth_headers, db_session, test_user):
        # Create a real session first
        from core.events.session_manager import SessionManager
        mgr = SessionManager(db_session)
        session = mgr.create_session(user_id=test_user.user_id)

        mock_build.return_value = _mock_chat_loop("reply2")
        resp = client.post("/chat", json={"session_id": session.session_id, "message": "hi"}, headers=auth_headers)
        assert resp.status_code == 200
        assert resp.json()["session_id"] == session.session_id

    def test_chat_no_auth(self, client):
        resp = client.post("/chat", json={"message": "hi"})
        assert resp.status_code == 403

    @patch("api.routers.chat._build_chat_loop")
    def test_chat_nonexistent_session(self, mock_build, client, auth_headers):
        resp = client.post("/chat", json={"session_id": "no_such_id", "message": "hi"}, headers=auth_headers)
        assert resp.status_code == 404

    @patch("api.routers.chat._build_chat_loop")
    def test_stream_returns_sse(self, mock_build, client, auth_headers):
        mock_build.return_value = _mock_chat_loop("streamed")
        resp = client.post("/chat/stream", json={"message": "hi"}, headers=auth_headers)
        assert resp.status_code == 200
        assert "text/event-stream" in resp.headers["content-type"]
        assert "session_info" in resp.text
        assert "text_delta" in resp.text
