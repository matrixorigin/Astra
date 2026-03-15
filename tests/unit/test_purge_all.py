"""Regression tests for purge_all — must call real purge API, not fake success."""

from __future__ import annotations
from unittest.mock import MagicMock
import pytest


class TestPurgeAll:
    def _make_storage(self):
        from core.memory.backends.memoria_http import MemoriaStorage
        s = MemoriaStorage.__new__(MemoriaStorage)
        s.client = MagicMock()
        s.user_id = "user1"
        return s

    def test_purge_all_calls_purge_api(self):
        """purge_all must call client.purge, not return fake success."""
        storage = self._make_storage()
        storage.client.purge.return_value = {"purged": 5}

        result = storage.purge_all("user1")

        storage.client.purge.assert_called_once()
        call_kwargs = storage.client.purge.call_args.kwargs
        assert call_kwargs["user_id"] == "user1"
        assert result["status"] == "success"

    def test_purge_all_returns_purged_count(self):
        storage = self._make_storage()
        storage.client.purge.return_value = {"purged": 12}

        result = storage.purge_all("user1")

        assert result["purged"] == 12

    def test_purge_all_handles_error_gracefully(self):
        storage = self._make_storage()
        storage.client.purge.side_effect = Exception("connection refused")

        result = storage.purge_all("user1")

        assert result["status"] == "error"
        assert "connection refused" in result["message"]
