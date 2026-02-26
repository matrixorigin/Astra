"""Unit tests for MemoryRetriever — Task 2."""

from collections import namedtuple
from datetime import datetime
from unittest.mock import MagicMock, patch

import pytest

from core.memory.retriever import MemoryRetriever, TASK_WEIGHTS, _HYBRID_SQL, _FALLBACK_SQL
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
        results = retriever.retrieve("u1", "Go testing", query_embedding=[0.1] * 1536)
        assert len(results) == 2
        assert results[0].memory_id == "m1"
        assert results[0].memory_type == MemoryType.EPISODIC

    def test_uses_hybrid_sql(self, retriever, mock_db):
        retriever.retrieve("u1", "test", query_embedding=[0.1] * 1536)
        call_args = mock_db.execute.call_args
        assert call_args[0][0] is _HYBRID_SQL

    def test_passes_weights(self, retriever, mock_db):
        w = RetrievalWeights(vector=0.5, keyword=0.1, temporal=0.2, confidence=0.2)
        retriever.retrieve("u1", "test", query_embedding=[0.1] * 1536, weights=w)
        params = mock_db.execute.call_args[0][1]
        assert params["w_vec"] == 0.5
        assert params["w_kw"] == 0.1

    def test_task_hint_selects_weights(self, retriever, mock_db):
        retriever.retrieve("u1", "test", query_embedding=[0.1] * 1536, task_hint="code")
        params = mock_db.execute.call_args[0][1]
        assert params["w_kw"] == TASK_WEIGHTS["code"].keyword

    def test_filters_by_memory_type(self, retriever, mock_db):
        retriever.retrieve("u1", "test", query_embedding=[0.1] * 1536,
                          memory_types=[MemoryType.PROFILE])
        params = mock_db.execute.call_args[0][1]
        assert params["types"] == ("profile",)


class TestRetrieveWithoutEmbedding:
    def test_uses_fallback_sql(self, retriever, mock_db):
        retriever.retrieve("u1", "test", query_embedding=None)
        call_args = mock_db.execute.call_args
        assert call_args[0][0] is _FALLBACK_SQL

    def test_redistributes_weights(self, retriever, mock_db):
        """Without embedding, vector weight should be redistributed."""
        retriever.retrieve("u1", "test", query_embedding=None)
        params = mock_db.execute.call_args[0][1]
        # Fallback params should not have w_vec
        assert "w_vec" not in params
        # Remaining weights should be rescaled to sum to ~1.0
        total = params["w_kw"] + params["w_time"] + params["w_conf"]
        assert abs(total - 1.0) < 0.01

    def test_returns_memories(self, retriever, mock_db):
        mock_db.execute.return_value.fetchall.return_value = [
            MemRow("m1", "Go testing", "procedural", 0.8, datetime(2026, 2, 26)),
        ]
        results = retriever.retrieve("u1", "Go", query_embedding=None)
        assert len(results) == 1
        assert results[0].memory_type == MemoryType.PROCEDURAL


class TestRetrieveDefaults:
    def test_default_types_include_all_non_working(self, retriever, mock_db):
        retriever.retrieve("u1", "test", query_embedding=[0.1] * 1536)
        params = mock_db.execute.call_args[0][1]
        types = params["types"]
        assert "profile" in types
        assert "episodic" in types
        assert "semantic" in types
        assert "procedural" in types
        assert "working" not in types

    def test_default_limit(self, retriever, mock_db):
        retriever.retrieve("u1", "test", query_embedding=[0.1] * 1536)
        params = mock_db.execute.call_args[0][1]
        assert params["lim"] == 10
