"""Unit tests for MemoryRetriever — Task 2."""

from collections import namedtuple
from datetime import datetime
from unittest.mock import MagicMock, patch

import pytest

from core.memory.retriever import MemoryRetriever, TASK_WEIGHTS, _KEYWORD_SQL, _FALLBACK_SQL
from core.memory.types import MemoryType, RetrievalWeights


MemRow = namedtuple("MemRow", ["memory_id", "content", "memory_type", "initial_confidence", "observed_at", "session_id", "trust_tier"])


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
            MemRow("m1", "Go testing", "semantic", 0.9, datetime(2026, 2, 26), None, "T3"),
            MemRow("m2", "Python flask", "semantic", 0.7, datetime(2026, 2, 25), None, "T3"),
        ]
        results, _ = retriever.retrieve("u1", "Go testing", session_id="s1")
        assert len(results) == 2
        assert results[0].memory_id == "m1"
        assert results[0].memory_type == MemoryType.SEMANTIC

    def test_uses_keyword_sql_with_query(self, retriever, mock_db):
        retriever.retrieve("u1", "test query", session_id="s1")
        # First call tries keyword SQL, may fall back
        assert mock_db.execute.called

    def test_passes_weights(self, retriever, mock_db):
        w = RetrievalWeights(vector=0.5, keyword=0.1, temporal=0.2, confidence=0.2)
        retriever.retrieve("u1", "test", session_id="s1", weights=w)
        params = mock_db.execute.call_args[0][1]
        # Weights are normalized for temporal + confidence only
        assert "w_time" in params
        assert "w_conf" in params

    def test_task_hint_selects_weights(self, retriever, mock_db):
        retriever.retrieve("u1", "test", session_id="s1", task_hint="code")
        # Verify weights are used (normalized)
        params = mock_db.execute.call_args[0][1]
        assert "w_time" in params

    def test_filters_by_memory_type(self, retriever, mock_db):
        retriever.retrieve("u1", "test", session_id="s1", memory_types=[MemoryType.PROFILE])
        params = mock_db.execute.call_args[0][1]
        assert params["types"] == ("profile",)


class TestRetrieveWithoutEmbedding:
    def test_uses_fallback_sql(self, retriever, mock_db):
        retriever.retrieve("u1", "", session_id="s1")  # Empty query
        call_args = mock_db.execute.call_args
        # SQL is now a text() object created from template
        sql_str = str(call_args[0][0])
        assert "FROM memories m" in sql_str
        assert "MATCH" not in sql_str  # Fallback doesn't use MATCH

    def test_redistributes_weights(self, retriever, mock_db):
        """Without query, weights should be normalized for temporal + confidence."""
        retriever.retrieve("u1", "", session_id="s1")
        params = mock_db.execute.call_args[0][1]
        # Fallback params should have w_time and w_conf
        assert "w_time" in params
        assert "w_conf" in params
        # Weights should sum to ~1.0
        total = params["w_time"] + params["w_conf"]
        assert abs(total - 1.0) < 0.01

    def test_returns_memories(self, retriever, mock_db):
        mock_db.execute.return_value.fetchall.return_value = [
            MemRow("m1", "Go testing", "procedural", 0.8, datetime(2026, 2, 26), None, "T3"),
        ]
        results, _ = retriever.retrieve("u1", "Go", session_id="s1")
        assert len(results) == 1
        assert results[0].memory_type == MemoryType.PROCEDURAL


class TestRetrieveDefaults:
    def test_default_types_include_all_non_working(self, retriever, mock_db):
        retriever.retrieve("u1", "test", session_id="s1")
        params = mock_db.execute.call_args[0][1]
        types = params["types"]
        assert "profile" in types
        assert "semantic" in types
        assert "procedural" in types
        assert "working" not in types

    def test_default_limit(self, retriever, mock_db):
        retriever.retrieve("u1", "test", session_id="s1")
        params = mock_db.execute.call_args[0][1]
        assert params["lim"] == 10


class TestSessionIsolation:
    """Tests for session-based memory isolation."""

    @pytest.fixture
    def session_retriever(self):
        mock_db = MagicMock()
        mock_db.execute.return_value.fetchall.return_value = []
        return MemoryRetriever(db_factory=lambda: mock_db), mock_db

    def test_session_filter_added_when_session_id_provided(self, session_retriever):
        retriever, mock_db = session_retriever
        with patch.object(retriever, "_db") as patched_db:
            patched_db.return_value.__enter__.return_value = mock_db
            patched_db.return_value.__exit__.return_value = None
            retriever.retrieve("u1", "", session_id="sess123")
        call_args = mock_db.execute.call_args
        sql_str = str(call_args[0][0])
        assert "session_id" in sql_str
        params = call_args[0][1]
        assert params["session_id"] == "sess123"

    def test_cross_session_included_by_default(self, session_retriever):
        retriever, mock_db = session_retriever
        with patch.object(retriever, "_db") as patched_db:
            patched_db.return_value.__enter__.return_value = mock_db
            patched_db.return_value.__exit__.return_value = None
            retriever.retrieve("u1", "", session_id="sess123", include_cross_session=True)
        call_args = mock_db.execute.call_args
        sql_str = str(call_args[0][0])
        assert "session_id IS NULL" in sql_str

    def test_cross_session_excluded_when_disabled(self, session_retriever):
        retriever, mock_db = session_retriever
        with patch.object(retriever, "_db") as patched_db:
            patched_db.return_value.__enter__.return_value = mock_db
            patched_db.return_value.__exit__.return_value = None
            retriever.retrieve("u1", "", session_id="sess123", include_cross_session=False)
        call_args = mock_db.execute.call_args
        sql_str = str(call_args[0][0])
        assert "session_id IS NULL" not in sql_str
        assert "session_id = :session_id" in sql_str

    def test_session_id_always_in_params(self, session_retriever):
        """session_id is required and always passed to SQL."""
        retriever, mock_db = session_retriever
        with patch.object(retriever, "_db") as patched_db:
            patched_db.return_value.__enter__.return_value = mock_db
            patched_db.return_value.__exit__.return_value = None
            retriever.retrieve("u1", "", session_id="sess123")
        call_args = mock_db.execute.call_args
        params = call_args[0][1]
        assert params["session_id"] == "sess123"
