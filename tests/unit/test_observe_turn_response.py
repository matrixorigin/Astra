"""Regression tests for Bug 10: observe_turn must extract 'memories' from API response dict."""

from __future__ import annotations
from unittest.mock import MagicMock
import pytest


class TestObserveTurnResponseParsing:
    """Bug 10: /v1/observe returns {"memories": [...], "warning": "..."}, not a list."""

    def _make_client(self, response_body):
        from core.memory.backends.memoria_http import MemoriaHTTPClient

        client = MemoriaHTTPClient.__new__(MemoriaHTTPClient)
        client.api_key = "key"
        client.master_key = None
        mock_resp = MagicMock()
        mock_resp.json.return_value = response_body
        mock_resp.raise_for_status = MagicMock()
        client.client = MagicMock()
        client.client.post.return_value = mock_resp
        return client

    def test_extracts_memories_from_dict_response(self):
        """observe_turn must return the 'memories' list, not the wrapper dict."""
        client = self._make_client(
            {
                "memories": [
                    {
                        "memory_id": "m1",
                        "content": "User prefers Python",
                        "memory_type": "semantic",
                    },
                ],
                "warning": "LLM not configured",
            }
        )
        result = client.observe_turn("user1", [{"role": "user", "content": "I prefer Python"}])
        assert isinstance(result, list)
        assert len(result) == 1
        assert result[0]["memory_id"] == "m1"

    def test_empty_memories_returns_empty_list(self):
        client = self._make_client({"memories": []})
        result = client.observe_turn("user1", [{"role": "user", "content": "hi"}])
        assert result == []

    def test_list_response_passthrough(self):
        """If API ever returns a list directly, it should still work."""
        client = self._make_client(
            [{"memory_id": "m1", "content": "test", "memory_type": "semantic"}]
        )
        result = client.observe_turn("user1", [{"role": "user", "content": "test"}])
        assert isinstance(result, list)
        assert len(result) == 1

    def test_storage_observe_turn_returns_memory_objects(self):
        """MemoriaStorage.observe_turn must return Memory objects, not strings."""
        from core.memory.backends.memoria_http import MemoriaStorage
        from core.memory.types import Memory

        storage = MemoriaStorage.__new__(MemoriaStorage)
        storage.client = MagicMock()
        storage.client.observe_turn.return_value = [
            {"memory_id": "m1", "content": "fact", "memory_type": "semantic"},
        ]

        result = storage.observe_turn("user1", [{"role": "user", "content": "test"}])

        assert isinstance(result, list)
        assert len(result) == 1
        assert isinstance(result[0], Memory)
        assert result[0].content == "fact"
