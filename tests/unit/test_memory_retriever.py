"""Unit tests for MemoryRetriever — ORM-based."""

from datetime import datetime, timezone
from unittest.mock import MagicMock

import pytest

from core.memory.retriever import TASK_WEIGHTS, MemoryRetriever
from core.memory.types import MemoryType


def _make_chain(rows=None):
    """Chainable ORM query mock."""
    chain = MagicMock()
    chain.filter.return_value = chain
    chain.order_by.return_value = chain
    chain.limit.return_value = chain
    chain.all.return_value = rows or []
    return chain


def _mem_row(memory_id, content="text", memory_type="semantic",
             confidence=0.8, observed_at=None, session_id=None,
             trust_tier="T3", relevance=1.0):
    """Simulate an ORM result row from _phase1."""
    r = MagicMock()
    r.memory_id = memory_id
    r.content = content
    r.memory_type = memory_type
    r.initial_confidence = confidence
    r.observed_at = observed_at or datetime(2026, 2, 26, tzinfo=timezone.utc)
    r.session_id = session_id
    r.trust_tier = trust_tier
    r.relevance = relevance
    return r


def _vec_row(memory_id, l2_dist=0.5, **kwargs):
    """Simulate an ORM result row from _phase2."""
    r = _mem_row(memory_id, **kwargs)
    r.l2_dist = l2_dist
    return r


class TestTaskWeights:
    def test_all_presets_sum_to_one(self):
        for name, w in TASK_WEIGHTS.items():
            total = w.vector + w.keyword + w.temporal + w.confidence
            assert abs(total - 1.0) < 0.01, f"{name} weights sum to {total}"

    def test_code_boosts_keyword(self):
        assert TASK_WEIGHTS["code"].keyword > TASK_WEIGHTS["reasoning"].keyword

    def test_recall_boosts_vector(self):
        assert TASK_WEIGHTS["recall"].vector > TASK_WEIGHTS["default"].vector


class TestRetrievePhase1:
    """Tests for keyword + fallback retrieval (no embedding)."""

    @pytest.fixture
    def mock_db(self):
        db = MagicMock()
        db.query.return_value = _make_chain()
        return db

    @pytest.fixture
    def retriever(self, mock_db):
        return MemoryRetriever(db_factory=lambda: mock_db)

    def test_returns_memories_from_fallback(self, retriever, mock_db):
        rows = [_mem_row("m1", "Go testing"), _mem_row("m2", "Python flask")]
        mock_db.query.return_value = _make_chain(rows)

        results, _ = retriever.retrieve("u1", "Go testing", session_id="s1")
        assert len(results) == 2
        assert results[0].memory_id == "m1"
        assert results[0].memory_type == MemoryType.SEMANTIC

    def test_empty_query_returns_fallback(self, retriever, mock_db):
        results, _ = retriever.retrieve("u1", "", session_id="s1")
        assert results == []
        assert mock_db.query.called

    def test_retrieve_invokes_orm_query(self, retriever, mock_db):
        """Verify retrieve uses ORM query (not raw execute)."""
        retriever.retrieve("u1", "test", session_id="s1")
        assert mock_db.query.called


class TestRetrievePhase2:
    """Tests for vector retrieval path."""

    @pytest.fixture
    def mock_db(self):
        db = MagicMock()
        db.query.return_value = _make_chain()
        return db

    @pytest.fixture
    def retriever(self, mock_db):
        return MemoryRetriever(db_factory=lambda: mock_db)

    def test_vector_path_invoked_with_embedding(self, retriever, mock_db):
        """When query_embedding is provided, phase2 should run."""
        retriever.retrieve("u1", "test", session_id="s1", query_embedding=[0.1] * 384)
        # At least 2 query calls: phase1 fallback + phase2 vector
        assert mock_db.query.call_count >= 2

    def test_vector_failure_graceful(self, retriever, mock_db):
        """Vector search failure should not crash — falls back to phase1 results."""
        call_count = 0

        def side_effect(*args, **kwargs):
            nonlocal call_count
            call_count += 1
            if call_count <= 1:
                return _make_chain([_mem_row("m1")])  # phase1
            raise RuntimeError("vector down")

        mock_db.query.side_effect = side_effect
        results, _ = retriever.retrieve("u1", "test", session_id="s1", query_embedding=[0.1] * 384)
        assert len(results) >= 1


class TestRetrieveExplain:
    """Tests for explain mode stats."""

    @pytest.fixture
    def mock_db(self):
        db = MagicMock()
        db.query.return_value = _make_chain()
        return db

    @pytest.fixture
    def retriever(self, mock_db):
        return MemoryRetriever(db_factory=lambda: mock_db)

    def test_explain_returns_stats(self, retriever, mock_db):
        _, stats = retriever.retrieve("u1", "test", session_id="s1", explain=True)
        assert stats is not None
        assert stats.total_ms >= 0

    def test_no_explain_returns_none(self, retriever, mock_db):
        _, stats = retriever.retrieve("u1", "test", session_id="s1", explain=False)
        assert stats is None
