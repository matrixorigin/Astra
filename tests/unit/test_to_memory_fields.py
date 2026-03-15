"""Regression tests for Bug 4: _to_memory must populate session_id and retrieval_score."""

from __future__ import annotations
import pytest


class TestToMemoryFields:
    """Bug 4: _to_memory was missing session_id and retrieval_score."""

    def _make_storage(self):
        from core.memory.backends.memoria_http import MemoriaStorage

        s = MemoriaStorage.__new__(MemoriaStorage)
        return s

    def test_session_id_populated(self):
        """session_id from API response must be set on Memory object."""
        from core.memory.types import MemoryType

        s = self._make_storage()
        m = s._to_memory(
            {
                "memory_id": "m1",
                "content": "test",
                "memory_type": "semantic",
                "session_id": "sess-abc",
            },
            "user1",
        )
        assert m.session_id == "sess-abc"

    def test_session_id_none_when_absent(self):
        """Missing session_id in response → Memory.session_id is None."""
        s = self._make_storage()
        m = s._to_memory(
            {"memory_id": "m1", "content": "test", "memory_type": "episodic"},
            "user1",
        )
        assert m.session_id is None

    def test_retrieval_score_populated(self):
        """retrieval_score from API response must be set on Memory object."""
        s = self._make_storage()
        m = s._to_memory(
            {
                "memory_id": "m1",
                "content": "test",
                "memory_type": "semantic",
                "retrieval_score": 0.87,
            },
            "user1",
        )
        assert m.retrieval_score == pytest.approx(0.87)

    def test_retrieval_score_none_when_absent(self):
        """Missing retrieval_score → Memory.retrieval_score is None."""
        s = self._make_storage()
        m = s._to_memory(
            {"memory_id": "m1", "content": "test", "memory_type": "semantic"},
            "user1",
        )
        assert m.retrieval_score is None

    def test_episodic_memory_type_preserved(self):
        """memory_type='episodic' must survive round-trip through _to_memory."""
        from core.memory.types import MemoryType

        s = self._make_storage()
        m = s._to_memory(
            {"memory_id": "m1", "content": "Session Summary: ...", "memory_type": "episodic"},
            "user1",
        )
        assert m.memory_type == MemoryType.EPISODIC
