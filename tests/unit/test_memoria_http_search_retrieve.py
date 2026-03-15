"""Regression tests for Bug 16 (search returns list) and Bug 17 (retrieve KeyError)."""

from __future__ import annotations
from unittest.mock import MagicMock


def _make_client():
    from core.memory.backends.memoria_http import MemoriaHTTPClient
    c = MemoriaHTTPClient.__new__(MemoriaHTTPClient)
    c.client = MagicMock()
    c.api_key = None
    c.master_key = None
    return c


def _make_storage():
    from core.memory.backends.memoria_http import MemoriaStorage
    s = MemoriaStorage.__new__(MemoriaStorage)
    s.client = MagicMock()
    s.user_id = "u1"
    return s


class TestSearchReturnsListBug16:
    def test_search_extracts_results_list(self):
        """Bug 16: search() must return list, not raw dict {"results": [...]}."""
        c = _make_client()
        mock_resp = MagicMock()
        mock_resp.json.return_value = {"results": [{"content": "foo", "memory_id": "m1"}]}
        c.client.post.return_value = mock_resp

        result = c.search("u1", "query")

        assert isinstance(result, list)
        assert result[0]["content"] == "foo"

    def test_search_empty_results(self):
        c = _make_client()
        mock_resp = MagicMock()
        mock_resp.json.return_value = {"results": []}
        c.client.post.return_value = mock_resp

        result = c.search("u1", "query")
        assert result == []


class TestRetrieveKeyErrorBug17:
    def test_retrieve_no_keyerror_on_unexpected_format(self):
        """Bug 17: retrieve() must not raise KeyError if API returns unexpected format."""
        from core.memory.types import MemoryType
        s = _make_storage()
        s.client.retrieve.return_value = {"unexpected_key": []}  # no "results"

        memories, _ = s.retrieve("u1", "query", memory_types=[MemoryType.SEMANTIC])
        assert memories == []

    def test_retrieve_reads_results_key(self):
        """retrieve() must read from 'results' key."""
        from core.memory.types import MemoryType
        s = _make_storage()
        s.client.retrieve.return_value = {
            "results": [{"memory_id": "m1", "content": "hello", "memory_type": "semantic",
                         "trust_tier": "T3", "initial_confidence": 0.8}]
        }

        memories, _ = s.retrieve("u1", "query", memory_types=[MemoryType.SEMANTIC])
        assert len(memories) == 1
        assert memories[0].content == "hello"
