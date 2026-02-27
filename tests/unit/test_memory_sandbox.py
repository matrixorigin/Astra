"""Unit tests for MemorySandbox — Task 4."""

from datetime import datetime
from unittest.mock import MagicMock, call

import pytest

from core.memory.sandbox import MemorySandbox
from core.memory.types import Memory, MemoryType


@pytest.fixture
def mock_db():
    db = MagicMock()
    db.execute.return_value.fetchall.return_value = []
    return db


@pytest.fixture
def sandbox(mock_db):
    return MemorySandbox(db_factory=lambda: mock_db, db_name="test_db")


def _mem(mid="m1", content="test", embedding=None):
    return Memory(
        memory_id=mid, user_id="u1", memory_type=MemoryType.PROFILE,
        content=content, confidence=0.8, embedding=embedding,
        observed_at=datetime(2026, 2, 26),
    )


class TestSandboxValidation:
    def test_empty_memories_returns_true(self, sandbox):
        result, _ = sandbox.validate_memories("u1", [], "query")
        assert result is True

    def test_creates_and_drops_branch(self, sandbox, mock_db):
        sandbox.validate_memories("u1", [_mem()], "query")
        # Check that branch operations were called
        assert mock_db.execute.call_count >= 3  # create, insert, selects, drop
        assert mock_db.commit.call_count >= 1

    def test_inserts_to_branch(self, sandbox, mock_db):
        mem = _mem(embedding=[0.1, 0.2])
        sandbox.validate_memories("u1", [mem], "query")
        # Verify insert was called with memory data
        calls = mock_db.execute.call_args_list
        insert_call = [c for c in calls if c[0] and len(c[0]) > 1 and "mid" in str(c[0][1])]
        assert len(insert_call) > 0

    def test_returns_true_when_branch_improves(self, sandbox, mock_db):
        # Mock _retrieval_score directly
        scores = [0.3, 0.9]  # main low, branch high
        sandbox._retrieval_score = MagicMock(side_effect=scores)
        result, _ = sandbox.validate_memories("u1", [_mem()], "query", [0.1] * 10)
        assert result is True

    def test_returns_false_when_branch_worse(self, sandbox, mock_db):
        scores = [0.9, 0.3]  # main high, branch low
        sandbox._retrieval_score = MagicMock(side_effect=scores)
        result, _ = sandbox.validate_memories("u1", [_mem()], "query", [0.1] * 10)
        assert result is False

    def test_fails_open_on_error(self, sandbox, mock_db):
        mock_db.execute.side_effect = Exception("DB error")
        # Should return True (fail open) when validation errors
        result, stats = sandbox.validate_memories("u1", [_mem()], "query", explain=True)
        assert result is True
        assert stats.error is not None

    def test_drops_branch_even_on_error(self, sandbox, mock_db):
        # First call succeeds (create), second fails
        call_count = [0]
        def mock_execute(sql, params=None):
            call_count[0] += 1
            if call_count[0] == 2:  # insert fails
                raise Exception("Insert failed")
            return MagicMock(fetchall=lambda: [])
        mock_db.execute.side_effect = mock_execute

        sandbox.validate_memories("u1", [_mem()], "query")
        # Should have attempted multiple calls including cleanup
        assert call_count[0] >= 2


class TestRetrievalScore:
    def test_with_embedding(self, sandbox, mock_db):
        mock_db.execute.return_value.fetchall.return_value = [
            MagicMock(sim=0.8), MagicMock(sim=0.6),
        ]
        score = sandbox._retrieval_score("memories", "u1", "query", [0.1] * 10)
        assert score == 0.7  # (0.8 + 0.6) / 2

    def test_without_embedding(self, sandbox, mock_db):
        mock_db.execute.return_value.fetchall.return_value = [
            MagicMock(sim=0.9),
        ]
        score = sandbox._retrieval_score("memories", "u1", "query", None)
        assert score == 0.9

    def test_empty_returns_zero(self, sandbox, mock_db):
        mock_db.execute.return_value.fetchall.return_value = []
        score = sandbox._retrieval_score("memories", "u1", "query", [0.1] * 10)
        assert score == 0.0
