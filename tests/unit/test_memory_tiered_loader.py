"""Unit tests for TieredMemoryLoader."""

from unittest.mock import MagicMock, patch

import pytest

from core.memory.tiered_loader import TieredMemoryLoader
from core.memory.types import Memory, MemoryType


@pytest.fixture
def mock_db():
    return MagicMock()


@pytest.fixture
def loader(mock_db):
    return TieredMemoryLoader(db_factory=lambda: mock_db)


class TestLoadL0:
    def test_returns_profile(self, loader):
        with patch.object(loader, '_ensure_initialized', return_value=True):
            loader._profile_mgr = MagicMock()
            loader._profile_mgr.get_profile.return_value = "User Profile:\n- likes Go"
            result = loader.load_l0("u1")
            assert "likes Go" in result

    def test_returns_empty_on_failure(self, loader):
        with patch.object(loader, '_ensure_initialized', return_value=False):
            assert loader.load_l0("u1") == ""


class TestLoadL1:
    def test_returns_memories(self, loader):
        with patch.object(loader, '_ensure_initialized', return_value=True):
            loader._retriever = MagicMock()
            loader._retriever.retrieve.return_value = ([
                Memory(memory_id="m1", user_id="u1", memory_type=MemoryType.SEMANTIC,
                       content="discussed testing", initial_confidence=0.8),
            ], None)
            result, _ = loader.load_l1("u1", "s1", "testing")
            assert "discussed testing" in result
            assert "[semantic]" in result

    def test_returns_empty_when_no_memories(self, loader):
        with patch.object(loader, '_ensure_initialized', return_value=True):
            loader._retriever = MagicMock()
            loader._retriever.retrieve.return_value = ([], None)
            result, _ = loader.load_l1("u1", "s1", "query")
            assert result == ""

    def test_no_episodic_in_retrieval_types(self, loader):
        """L1 retrieval should not request EPISODIC — type eliminated."""
        with patch.object(loader, '_ensure_initialized', return_value=True):
            loader._retriever = MagicMock()
            loader._retriever.retrieve.return_value = ([], None)
            loader.load_l1("u1", "s1", "query")
            call_kwargs = loader._retriever.retrieve.call_args[1]
            for mt in call_kwargs["memory_types"]:
                assert mt != MemoryType.WORKING  # WORKING excluded
            assert MemoryType.SEMANTIC in call_kwargs["memory_types"]
            assert MemoryType.PROCEDURAL in call_kwargs["memory_types"]


class TestBuildSection:
    def test_combines_l0_and_l1(self, loader):
        loader.load_l0 = MagicMock(return_value="User Profile:\n- likes Go")
        loader.load_l1 = MagicMock(return_value=("Relevant Memories:\n- [semantic] test", None))
        result, _ = loader.build_section("u1", "s1", "query")
        assert "User Profile" in result
        assert "Relevant Memories" in result

    def test_handles_empty_l0(self, loader):
        loader.load_l0 = MagicMock(return_value="")
        loader.load_l1 = MagicMock(return_value=("Relevant Memories:\n- test", None))
        result, _ = loader.build_section("u1", "s1", "query")
        assert "Relevant Memories" in result
        assert "User Profile" not in result

    def test_handles_both_empty(self, loader):
        loader.load_l0 = MagicMock(return_value="")
        loader.load_l1 = MagicMock(return_value=("", None))
        result, _ = loader.build_section("u1", "s1", "query")
        assert result == ""

    def test_explain_returns_stats(self, loader):
        loader.load_l0 = MagicMock(return_value="profile")
        loader.load_l1 = MagicMock(return_value=("mem_memories", None))
        _, stats = loader.build_section("u1", "s1", "query", explain=True)
        assert stats is not None
        assert stats.l0_loaded is True
        assert stats.total_ms >= 0


class TestInvalidateProfile:
    def test_invalidates_cache(self, loader):
        loader._profile_mgr = MagicMock()
        loader.invalidate_profile("u1")
        loader._profile_mgr.invalidate.assert_called_once_with("u1")
