"""Regression tests for Bug 3: observe_turn must propagate session_id.

Without session_id, all observer-extracted memories get session_id=NULL,
making them indistinguishable from cross-session episodic memories and
breaking session-scoped retrieval.
"""

from __future__ import annotations

from unittest.mock import MagicMock, patch
import os
import pytest


class TestObserveTurnSessionId:
    """Bug 3: session_id must flow from run_pipeline → observe_turn → HTTP payload."""

    def test_http_client_observe_turn_sends_session_id(self):
        """MemoriaHTTPClient.observe_turn must include session_id in POST payload."""
        from core.memory.backends.memoria_http import MemoriaHTTPClient

        mock_resp = MagicMock()
        mock_resp.json.return_value = []
        mock_resp.raise_for_status = MagicMock()

        client = MemoriaHTTPClient.__new__(MemoriaHTTPClient)
        client.client = MagicMock()
        client.client.post.return_value = mock_resp
        client.api_key = "test-key"
        client.master_key = None

        client.observe_turn(
            user_id="user1",
            messages=[{"role": "user", "content": "hello, what is the weather today?"}],
            session_id="sess-abc",
        )

        call_kwargs = client.client.post.call_args
        payload = call_kwargs.kwargs.get("json") or call_kwargs.args[1]
        assert payload.get("session_id") == "sess-abc", (
            "session_id must be in POST /v1/observe payload"
        )

    def test_http_client_observe_turn_omits_session_id_when_none(self):
        """session_id=None must not be sent in payload (backward compat)."""
        from core.memory.backends.memoria_http import MemoriaHTTPClient

        mock_resp = MagicMock()
        mock_resp.json.return_value = []
        mock_resp.raise_for_status = MagicMock()

        client = MemoriaHTTPClient.__new__(MemoriaHTTPClient)
        client.client = MagicMock()
        client.client.post.return_value = mock_resp
        client.api_key = "test-key"
        client.master_key = None

        client.observe_turn(
            user_id="user1",
            messages=[{"role": "user", "content": "hello, what is the weather today?"}],
            session_id=None,
        )

        call_kwargs = client.client.post.call_args
        payload = call_kwargs.kwargs.get("json") or call_kwargs.args[1]
        assert "session_id" not in payload, (
            "session_id=None must not be sent — avoids overwriting existing session context"
        )

    def test_storage_run_pipeline_passes_session_id(self):
        """MemoriaStorage.run_pipeline must pass session_id to observe_turn."""
        from core.memory.backends.memoria_http import MemoriaStorage

        storage = MemoriaStorage.__new__(MemoriaStorage)
        storage.client = MagicMock()
        storage.client.observe_turn.return_value = []
        storage._to_memory = MagicMock(return_value=MagicMock())

        storage.run_pipeline(
            user_id="user1",
            messages=[{"role": "user", "content": "test the session id propagation flow"}],
            session_id="sess-xyz",
        )

        storage.client.observe_turn.assert_called_once()
        call_kwargs = storage.client.observe_turn.call_args.kwargs
        assert call_kwargs.get("session_id") == "sess-xyz"

    def test_turn_hooks_passes_session_id_to_run_pipeline(self):
        """TurnHooks.run_observer must pass session_id to svc.run_pipeline."""
        from core.agent.turn_hooks import TurnHooks

        hooks = TurnHooks.__new__(TurnHooks)
        hooks._db = MagicMock()

        svc = MagicMock()
        svc.run_pipeline.return_value = MagicMock(memories_extracted=0)
        hooks._maybe_trigger_episodic = MagicMock()

        # Capture the background function and run it synchronously
        captured_fn = {}

        def fake_thread(target=None, daemon=None):
            captured_fn["fn"] = target
            t = MagicMock()
            t.start = lambda: captured_fn["fn"]()
            return t

        with (
            patch("core.agent.turn_hooks.get_memoria_storage", return_value=svc),
            patch("core.agent.turn_hooks._shutdown_event") as mock_shutdown,
            patch("threading.Thread", side_effect=fake_thread),
        ):
            mock_shutdown.is_set.return_value = False
            hooks.run_observer(
                session_id="sess-123",
                user_id="user1",
                messages=[{"role": "user", "content": "hello, what is the weather today?"}],
            )

        svc.run_pipeline.assert_called_once()
        call_kwargs = svc.run_pipeline.call_args.kwargs
        assert call_kwargs.get("session_id") == "sess-123", (
            "session_id must be passed to run_pipeline so observer memories are session-scoped"
        )
