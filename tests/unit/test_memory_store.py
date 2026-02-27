"""Unit tests for MemoryStore — Task 1."""

from datetime import datetime
from unittest.mock import MagicMock, patch

import pytest

from core.memory.types import Memory, MemoryType, RetrievalWeights
from core.memory.store import MemoryStore


# ---------------------------------------------------------------------------
# Fixtures
# ---------------------------------------------------------------------------

@pytest.fixture
def mock_db():
    db = MagicMock()
    db.query.return_value.filter_by.return_value.first.return_value = None
    db.query.return_value.filter.return_value.all.return_value = []
    return db


@pytest.fixture
def store(mock_db):
    return MemoryStore(db_factory=lambda: mock_db)


def _mem(mid="m1", uid="u1", mtype=MemoryType.PROFILE, content="likes Go"):
    return Memory(memory_id=mid, user_id=uid, memory_type=mtype, content=content)


# ---------------------------------------------------------------------------
# Types
# ---------------------------------------------------------------------------

class TestMemoryTypes:
    def test_memory_type_values(self):
        assert MemoryType.PROFILE.value == "profile"
        assert MemoryType.SEMANTIC.value == "semantic"
        assert MemoryType.PROCEDURAL.value == "procedural"
        assert MemoryType.WORKING.value == "working"
        assert MemoryType.TOOL_RESULT.value == "tool_result"

    def test_memory_defaults(self):
        m = _mem()
        assert m.initial_confidence == 0.75
        assert m.is_active is True
        assert m.embedding is None
        assert m.source_event_ids == []
        assert m.superseded_by is None

    def test_retrieval_weights_default_sum(self):
        w = RetrievalWeights()
        assert abs(w.vector + w.keyword + w.temporal + w.confidence - 1.0) < 0.01

    def test_retrieval_weights_rejects_bad_sum(self):
        with pytest.raises(ValueError, match="sum to 1.0"):
            RetrievalWeights(vector=0.5, keyword=0.5, temporal=0.5, confidence=0.5)

    def test_retrieval_weights_custom(self):
        w = RetrievalWeights(vector=0.4, keyword=0.1, temporal=0.2, confidence=0.3)
        assert w.vector == 0.4


# ---------------------------------------------------------------------------
# Store CRUD
# ---------------------------------------------------------------------------

class TestMemoryStoreCreate:
    def test_create_adds_to_db(self, store, mock_db):
        m = _mem()
        result = store.create(m)
        mock_db.add.assert_called_once()
        mock_db.commit.assert_called_once()
        assert result.memory_id == "m1"

    def test_create_generates_id_if_empty(self, store, mock_db):
        m = _mem(mid="")
        result = store.create(m)
        assert result.memory_id != ""
        assert len(result.memory_id) == 32  # uuid hex

    def test_create_sets_observed_at(self, store, mock_db):
        m = _mem()
        assert m.observed_at is None
        store.create(m)
        assert m.observed_at is not None


class TestMemoryStoreGet:
    def test_get_returns_none_when_missing(self, store):
        assert store.get("nonexistent") is None

    def test_get_returns_memory(self, store, mock_db):
        row = MagicMock()
        row.memory_id = "m1"
        row.user_id = "u1"
        row.memory_type = "profile"
        row.content = "likes Go"
        row.initial_confidence = 0.9
        row.embedding = None
        row.source_event_ids = ["e1"]
        row.superseded_by = None
        row.is_active = 1
        row.observed_at = datetime(2026, 2, 26)
        row.created_at = datetime(2026, 2, 26)
        row.updated_at = datetime(2026, 2, 26)
        row.trust_tier = "T3"
        row.session_id = None
        mock_db.query.return_value.filter_by.return_value.first.return_value = row

        result = store.get("m1")
        assert result is not None
        assert result.memory_type == MemoryType.PROFILE
        assert result.content == "likes Go"
        assert result.is_active is True


class TestMemoryStoreListActive:
    def test_list_active_empty(self, store):
        assert store.list_active("u1") == []

    def test_list_active_filters_by_type(self, store, mock_db):
        store.list_active("u1", memory_type=MemoryType.SEMANTIC)
        # Verify filter was called (type filter added)
        assert mock_db.query.return_value.filter.return_value.filter.called


class TestMemoryStoreSupersede:
    def test_supersede_deactivates_old_and_inserts_new(self, store, mock_db):
        old_row = MagicMock()
        old_row.is_active = 1
        mock_db.query.return_value.filter_by.return_value.first.return_value = old_row

        new_mem = _mem(mid="m2", content="likes Rust")
        result = store.supersede("m1", new_mem)

        # Old deactivated
        assert old_row.is_active == 0
        assert old_row.superseded_by == "m2"
        # New inserted
        mock_db.add.assert_called_once()
        mock_db.commit.assert_called_once()
        assert result.memory_id == "m2"

    def test_supersede_chain(self, store, mock_db):
        """A→B→C: only C should be the final active memory."""
        old_a = MagicMock(is_active=1)
        mock_db.query.return_value.filter_by.return_value.first.return_value = old_a

        b = _mem(mid="b", content="v2")
        store.supersede("a", b)
        assert old_a.is_active == 0
        assert old_a.superseded_by == "b"

        old_b = MagicMock(is_active=1)
        mock_db.query.return_value.filter_by.return_value.first.return_value = old_b

        c = _mem(mid="c", content="v3")
        store.supersede("b", c)
        assert old_b.is_active == 0
        assert old_b.superseded_by == "c"


class TestMemoryStoreDeactivate:
    def test_deactivate_existing(self, store, mock_db):
        row = MagicMock()
        mock_db.query.return_value.filter_by.return_value.first.return_value = row
        assert store.deactivate("m1") is True
        assert row.is_active == 0
        mock_db.commit.assert_called_once()

    def test_deactivate_missing(self, store, mock_db):
        mock_db.query.return_value.filter_by.return_value.first.return_value = None
        assert store.deactivate("nonexistent") is False


class TestArchiveWorkingMemories:
    def test_archive_returns_count(self, store, mock_db):
        mock_db.execute.return_value.rowcount = 3
        count = store.archive_working_memories("sess_1")
        assert count == 3
        mock_db.commit.assert_called()
