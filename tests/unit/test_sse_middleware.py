"""Unit tests for SSE exception handlers in api.main."""

import os
import pytest

os.environ.setdefault("TOKEN_ENCRYPTION_KEY", "test-key-" + "x" * 32)
os.environ.setdefault("JWT_SECRET_KEY", "test-jwt-secret-" + "x" * 32)

from fastapi.testclient import TestClient
from api.main import app
from tests.conftest import parse_sse_events, get_auth_headers


@pytest.fixture
def client():
    return TestClient(app, raise_server_exceptions=False)


class TestSSEExceptionHandlers:
    """SSE endpoints return text/event-stream errors, non-SSE return JSON."""

    def test_sse_endpoint_no_auth_returns_sse_error(self, client):
        """POST /chat/stream without auth → SSE error, not JSON 403."""
        resp = client.post("/chat/stream", json={"message": "hi"})
        assert resp.status_code == 200
        assert "text/event-stream" in resp.headers["content-type"]
        events = parse_sse_events(resp.text)
        assert len(events) >= 1
        assert events[0]["type"] == "error"
        assert events[0]["code"] == "AUTH_ERROR"

    def test_sse_endpoint_chat_turn_no_auth_returns_sse_error(self, client):
        """POST /chat/turn without auth → SSE error."""
        resp = client.post("/chat/turn", json={"messages": [{"role": "user", "content": "hi"}]})
        assert resp.status_code == 200
        assert "text/event-stream" in resp.headers["content-type"]
        events = parse_sse_events(resp.text)
        assert events[0]["type"] == "error"
        assert events[0]["code"] == "AUTH_ERROR"

    def test_sse_endpoint_validation_error_returns_sse(self, client, db_session):
        """POST /chat/stream with valid auth but missing required field → VALIDATION_ERROR."""
        headers = get_auth_headers(
            client,
            db_session,
            username="sse_val_user",
            user_id="sse_val_uid",
            email="sse_val@test.com",
        )
        resp = client.post("/chat/stream", json={}, headers=headers)
        assert resp.status_code == 200
        assert "text/event-stream" in resp.headers["content-type"]
        events = parse_sse_events(resp.text)
        assert events[0]["type"] == "error"
        assert events[0]["code"] == "VALIDATION_ERROR"
        # Verify clean message format (no raw Pydantic URLs)
        assert "pydantic.dev" not in events[0]["message"]
        assert "message" in events[0]["message"].lower()  # field name present

    def test_non_sse_endpoint_still_returns_json(self, client):
        """POST /chat without auth → normal JSON 401."""
        resp = client.post("/chat", json={"message": "hi"})
        assert resp.status_code in (401, 403)
        assert "application/json" in resp.headers["content-type"]

    def test_non_sse_endpoint_404_still_json(self, client):
        """GET /chat/runs/xxx (non-SSE) without auth → JSON error."""
        resp = client.get("/chat/runs/nonexistent")
        assert resp.status_code in (401, 403)
        assert "application/json" in resp.headers["content-type"]
