"""Tests for firewall verification in /chat/turn."""

import json
import os
from unittest.mock import MagicMock, patch

import pytest
from fastapi.testclient import TestClient

os.environ.setdefault("TOKEN_ENCRYPTION_KEY", "test-key-" + "x" * 32)
os.environ.setdefault("JWT_SECRET_KEY", "test-jwt-secret-" + "x" * 32)

from api.main import app
from tests.conftest import parse_sse_events, fake_llm_stream, get_auth_headers


@pytest.fixture
def client():
    return TestClient(app)


class TestChatTurnFirewall:
    """Verify firewall integration in /chat/turn."""

    def _auth(self, client, db):
        return get_auth_headers(client, db, username="fwuser",
                                user_id="fw_user", email="fw@test.com")

    def test_firewall_called_on_text_response(self, client, db):
        """Firewall runs with correct args when LLM produces text and snapshot exists."""
        headers = self._auth(client, db)
        mock_fw_result = MagicMock(safe_to_deliver=True, claims_failed=0)

        with patch("core.llm.client.LLMClient.chat_stream",
                   return_value=fake_llm_stream([{"type": "text", "content": "Hello!"}])), \
             patch("core.verification.firewall.HallucinationFirewall.verify_response",
                   return_value=mock_fw_result) as mock_verify:
            resp = client.post("/chat/turn", json={
                "messages": [{"role": "user", "content": "hi"}],
            }, headers=headers)

        events = parse_sse_events(resp.text)
        assert any(e["type"] == "turn_complete" for e in events)
        assert not any(e.get("type") == "warning" for e in events)
        # Verify firewall was actually called with the LLM output text
        mock_verify.assert_called_once()
        assert mock_verify.call_args[0][0] == "Hello!"

    def test_firewall_skipped_on_tool_call_only(self, client, db):
        """Firewall does NOT run when LLM only produces tool calls (no text)."""
        headers = self._auth(client, db)

        with patch("core.llm.client.LLMClient.chat_with_tools_stream",
                   return_value=fake_llm_stream([
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

        events = parse_sse_events(resp.text)
        assert any(e["type"] == "turn_complete" for e in events)
        mock_verify.assert_not_called()

    def test_firewall_failure_does_not_crash_stream(self, client, db):
        """Firewall exception is caught — stream completes normally."""
        headers = self._auth(client, db)

        with patch("core.llm.client.LLMClient.chat_stream",
                   return_value=fake_llm_stream([{"type": "text", "content": "Hello!"}])), \
             patch("core.verification.firewall.HallucinationFirewall.verify_response",
                   side_effect=RuntimeError("firewall boom")):
            resp = client.post("/chat/turn", json={
                "messages": [{"role": "user", "content": "hi"}],
            }, headers=headers)

        events = parse_sse_events(resp.text)
        assert any(e["type"] == "turn_complete" for e in events)
        assert not any(e.get("type") == "error" for e in events)

    def test_warning_before_turn_complete(self, client, db):
        """Warning event is emitted BEFORE turn_complete so edge receives it."""
        headers = self._auth(client, db)
        mock_fw_result = MagicMock(safe_to_deliver=False, claims_failed=2)

        with patch("core.llm.client.LLMClient.chat_stream",
                   return_value=fake_llm_stream([{"type": "text", "content": "The file contains X"}])), \
             patch("core.verification.firewall.HallucinationFirewall.verify_response",
                   return_value=mock_fw_result):
            resp = client.post("/chat/turn", json={
                "messages": [{"role": "user", "content": "what's in the file?"}],
            }, headers=headers)

        events = parse_sse_events(resp.text)
        warnings = [e for e in events if e.get("type") == "warning"]
        assert len(warnings) == 1
        assert warnings[0]["claims_failed"] == 2
        # Warning must come before turn_complete
        warning_idx = next(i for i, e in enumerate(events) if e.get("type") == "warning")
        complete_idx = next(i for i, e in enumerate(events) if e.get("type") == "turn_complete")
        assert warning_idx < complete_idx, "warning must arrive before turn_complete"
