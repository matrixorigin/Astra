"""Unit tests for MemoryRetriever — Task 2."""

from collections import namedtuple
from datetime import datetime
from unittest.mock import MagicMock, patch

import pytest

from core.memory.retriever import MemoryRetriever, TASK_WEIGHTS, _KEYWORD_SQL, _FALLBACK_SQL
from core.memory.types import MemoryType, RetrievalWeights


MemRow = namedtuple("MemRow", ["memory_id", "content", "memory_type", "confidence", "observed_at"])


@pytest.fixture
def mock_db():
    db = MagicMock()
    db.execute.return_value.fetchall.return_value = []
    return db


@pytest.fixture
def retriever(mock_db):
    return MemoryRetriever(db_factory=lambda: mock_db)


class TestTaskWeights:
    def test_all_presets_sum_to_one(self):
        for name, w in TASK_WEIGHTS.items():
            total = w.vector + w.keyword + w.temporal + w.confidence
            assert abs(total - 1.0) < 0.01, f"{name} weights sum to {total}"

    def test_code_boosts_keyword(self):
        assert TASK_WEIGHTS["code"].keyword > TASK_WEIGHTS["reasoning"].keyword

    def test_recall_boosts_vector(self):
        assert TASK_WEIGHTS["recall"].vector > TASK_WEIGHTS["default"].vector


class TestRetrieveWithEmbedding:
    def test_returns_memories(self, retriever, mock_db):
        mock_db.execute.return_value.fetchall.return_value = [
            MemRow("m1", "Go testing", "episodic", 0.9, datetime(2026, 2, 26)),
            MemRow("m2", "Python flask", "semantic", 0.7, datetime(2026, 2, 25)),
        ]
        results = retriever.retrieve("u1", "Go testing")
        assert len(results) == 2
        assert results[0].memory_id == "m1"
        assert results[0].memory_type == MemoryType.EPISODIC

    def test_uses_keyword_sql_with_query(self, retriever, mock_db):
        retriever.retrieve("u1", "test query")
        # First call tries keyword SQL, may fall back
        assert mock_db.execute.called

    def test_passes_weights(self, retriever, mock_db):
        w = RetrievalWeights(vector=0.5, keyword=0.1, temporal=0.2, confidence=0.2)
        retriever.retrieve("u1", "test", weights=w)
        params = mock_db.execute.call_args[0][1]
        # Weights are normalized for temporal + confidence only
        assert "w_time" in params
        assert "w_conf" in params

    def test_task_hint_selects_weights(self, retriever, mock_db):
        retriever.retrieve("u1", "test", task_hint="code")
        # Verify weights are used (normalized)
        params = mock_db.execute.call_args[0][1]
        assert "w_time" in params

    def test_filters_by_memory_type(self, retriever, mock_db):
        retriever.retrieve("u1", "test", memory_types=[MemoryType.PROFILE])
        params = mock_db.execute.call_args[0][1]
        assert params["types"] == ("profile",)


class TestRetrieveWithoutEmbedding:
    def test_uses_fallback_sql(self, retriever, mock_db):
        retriever.retrieve("u1", "")  # Empty query
        call_args = mock_db.execute.call_args
        assert call_args[0][0] is _FALLBACK_SQL

    def test_redistributes_weights(self, retriever, mock_db):
        """Without query, weights should be normalized for temporal + confidence."""
        retriever.retrieve("u1", "")
        params = mock_db.execute.call_args[0][1]
        # Fallback params should have w_time and w_conf
        assert "w_time" in params
        assert "w_conf" in params
        # Weights should sum to ~1.0
        total = params["w_time"] + params["w_conf"]
        assert abs(total - 1.0) < 0.01

    def test_returns_memories(self, retriever, mock_db):
        mock_db.execute.return_value.fetchall.return_value = [
            MemRow("m1", "Go testing", "procedural", 0.8, datetime(2026, 2, 26)),
        ]
        results = retriever.retrieve("u1", "Go")
        assert len(results) == 1
        assert results[0].memory_type == MemoryType.PROCEDURAL


class TestRetrieveDefaults:
    def test_default_types_include_all_non_working(self, retriever, mock_db):
        retriever.retrieve("u1", "test")
        params = mock_db.execute.call_args[0][1]
        types = params["types"]
        assert "profile" in types
        assert "episodic" in types
        assert "semantic" in types
        assert "procedural" in types
        assert "working" not in types

    def test_default_limit(self, retriever, mock_db):
        retriever.retrieve("u1", "test")
        params = mock_db.execute.call_args[0][1]
        assert params["lim"] == 10
