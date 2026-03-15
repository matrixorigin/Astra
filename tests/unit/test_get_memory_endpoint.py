"""Regression tests: get_memory must use GET /memories/{id}, not O(N) list scan."""

from __future__ import annotations
from unittest.mock import MagicMock
import pytest


class TestGetMemoryEndpoint:
    def _make_client(self):
        from core.memory.backends.memoria_http import MemoriaHTTPClient
        c = MemoriaHTTPClient.__new__(MemoriaHTTPClient)
        c.api_key = "key"
        c.master_key = None
        c.client = MagicMock()
        return c

    def test_get_memory_calls_get_endpoint(self):
        """get_memory must use GET /v1/memories/{id}, not list_memories scan."""
        client = self._make_client()
        mock_resp = MagicMock()
        mock_resp.status_code = 200
        mock_resp.json.return_value = {
            "memory_id": "m1", "content": "test", "memory_type": "semantic"
        }
        mock_resp.raise_for_status = MagicMock()
        client.client.get.return_value = mock_resp

        result = client.get_memory("user1", "m1")

        client.client.get.assert_called_once()
        url = client.client.get.call_args.args[0]
        assert "/v1/memories/m1" in url
        assert result["memory_id"] == "m1"

    def test_get_memory_returns_none_on_404(self):
        client = self._make_client()
        mock_resp = MagicMock()
        mock_resp.status_code = 404
        client.client.get.return_value = mock_resp

        result = client.get_memory("user1", "nonexistent")

        assert result is None

    def test_get_memory_does_not_call_list_memories(self):
        """Must not fall back to O(N) list scan."""
        client = self._make_client()
        mock_resp = MagicMock()
        mock_resp.status_code = 200
        mock_resp.json.return_value = {"memory_id": "m1", "content": "x", "memory_type": "semantic"}
        mock_resp.raise_for_status = MagicMock()
        client.client.get.return_value = mock_resp

        client.get_memory("user1", "m1")

        # list_memories would call GET /v1/memories (no ID), not /v1/memories/m1
        # Verify only one GET call was made (the direct endpoint)
        assert client.client.get.call_count == 1
