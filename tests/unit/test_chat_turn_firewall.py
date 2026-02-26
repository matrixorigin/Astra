"""Tests for firewall verification in /chat/turn."""

import json
import os
from unittest.mock import MagicMock, patch

import pytest
from fastapi.testclient import TestClient

os.environ.setdefault("TOKEN_ENCRYPTION_KEY", "test-key-" + "x" * 32)
os.environ.setdefault("JWT_SECRET_KEY", "test-jwt-secret-" + "x" * 32)

from api.main import app
from api.models import User


def _parse_sse(text: str) -> list[dict]:
    events = []
    for line in text.strip().split("\n"):
        if line.startswith("data: "):
            events.append(json.loads(line[6:]))
    return events


async def _fake_stream_gen(chunks):
    for c in chunks:
        yield c


def _fake_stream(chunks):
    return _fake_stream_gen(chunks)


@pytest.fixture
def client():
    return TestClient(app)


class TestChatTurnFirewall:
    """Verify firewall integration in /chat/turn."""

    def _get_auth_headers(self, client, db):
        from core.auth.password import hash_password
        user = db.query(User).filter(User.username == "fwuser").first()
        if not user:
            user = User(user_id="fw_user", username="fwuser",
                        email="fw@test.com", password_hash=hash_password("password123"))
            db.add(user)
            db.commit()
        resp = client.post("/auth/login", json={"username": "fwuser", "password": "password123"})
        return {"Authorization": f"Bearer {resp.json()['access_token']}"}

    def test_firewall_called_on_text_response(self, client, db):
        """Firewall runs when LLM produces text and snapshot exists."""
        headers = self._get_auth_headers(client, db)

        mock_fw_result = MagicMock(safe_to_deliver=True, claims_failed=0)

        with patch("core.llm.client.LLMClient.chat_stream", return_value=_fake_stream([
            {"type": "text", "content": "Hello!"},
        ])), \
             patch("core.verification.firewall.HallucinationFirewall.verify_response",
                   return_value=mock_fw_result) as mock_verify:
            resp = client.post("/chat/turn", json={
                "messages": [{"role": "user", "content": "hi"}],
            }, headers=headers)

        events = _parse_sse(resp.text)
        assert any(e["type"] == "turn_complete" for e in events)
        # No warning since safe_to_deliver=True
        assert not any(e.get("type") == "warning" for e in events)

    def test_firewall_skipped_on_tool_call_only(self, client, db):
        """Firewall does NOT run when LLM only produces tool calls (no text)."""
        headers = self._get_auth_headers(client, db)

        with patch("core.llm.client.LLMClient.chat_with_tools_stream", return_value=_fake_stream([
            {"type": "tool_call", "data": {
                "id": "tc_1", "type": "function",
                "function": {"name": "read_file", "arguments": '{"path": "a.py"}'},
            }},
        ])), \
             patch("core.verification.firewall.HallucinationFirewall.verify_response") as mock_verify:
            resp = client.post("/chat/turn", json={
                "messages": [{"role": "user", "content": "read a.py"}],
                "edge_tools": [{"type": "function", "function": {"name": "read_file", "description": "r", "parameters": {"type": "object", "properties": {"path": {"type": "string"}}}}}],
            }, headers=headers)

        events = _parse_sse(resp.text)
        assert any(e["type"] == "turn_complete" for e in events)
        mock_verify.assert_not_called()

    def test_firewall_failure_does_not_crash_stream(self, client, db):
        """Firewall exception is caught — stream completes normally."""
        headers = self._get_auth_headers(client, db)

        with patch("core.llm.client.LLMClient.chat_stream", return_value=_fake_stream([
            {"type": "text", "content": "Hello!"},
        ])), \
             patch("core.verification.firewall.HallucinationFirewall.verify_response",
                   side_effect=RuntimeError("firewall boom")):
            resp = client.post("/chat/turn", json={
                "messages": [{"role": "user", "content": "hi"}],
            }, headers=headers)

        events = _parse_sse(resp.text)
        assert any(e["type"] == "turn_complete" for e in events)
        assert not any(e.get("type") == "error" for e in events)

    def test_firewall_warning_emitted_on_unsafe(self, client, db):
        """Warning event emitted when firewall says response is unsafe."""
        headers = self._get_auth_headers(client, db)

        mock_fw_result = MagicMock(safe_to_deliver=False, claims_failed=2)

        with patch("core.llm.client.LLMClient.chat_stream", return_value=_fake_stream([
            {"type": "text", "content": "The file contains X"},
        ])), \
             patch("core.verification.firewall.HallucinationFirewall.verify_response",
                   return_value=mock_fw_result):
            resp = client.post("/chat/turn", json={
                "messages": [{"role": "user", "content": "what's in the file?"}],
            }, headers=headers)

        events = _parse_sse(resp.text)
        warnings = [e for e in events if e.get("type") == "warning"]
        assert len(warnings) == 1
        assert warnings[0]["claims_failed"] == 2
