"""Unit tests for TieredMemoryLoader (context-layer, MemoryService-based)."""

from unittest.mock import MagicMock

import pytest

from core.context.tiered_loader import TieredMemoryLoader
from core.memory.types import Memory, MemoryType


@pytest.fixture
def mock_svc():
    return MagicMock()


@pytest.fixture
def loader(mock_svc):
    return TieredMemoryLoader(memory_service=mock_svc)


class TestLoadL0:
    def test_returns_profile(self, loader, mock_svc):
        mock_svc.get_profile.return_value = "User Profile:\n- likes Go"
        result = loader.load_l0("u1")
        assert "likes Go" in result

    def test_returns_empty_on_failure(self, loader, mock_svc):
        mock_svc.get_profile.side_effect = Exception("fail")
        assert loader.load_l0("u1") == ""

    def test_returns_empty_when_none(self, loader, mock_svc):
        mock_svc.get_profile.return_value = None
        assert loader.load_l0("u1") == ""


class TestLoadL1:
    def test_returns_memories(self, loader, mock_svc):
        mock_svc.retrieve.return_value = [
            Memory(memory_id="m1", user_id="u1", memory_type=MemoryType.SEMANTIC,
                   content="discussed testing", initial_confidence=0.8),
        ]
        result, _ = loader.load_l1("u1", "s1", "testing")
        assert "discussed testing" in result
        assert "[semantic]" in result

    def test_returns_empty_when_no_memories(self, loader, mock_svc):
        mock_svc.retrieve.return_value = []
        result, _ = loader.load_l1("u1", "s1", "query")
        assert result == ""

    def test_no_episodic_in_retrieval_types(self, loader, mock_svc):
        """L1 retrieval should not request WORKING type."""
        mock_svc.retrieve.return_value = []
        loader.load_l1("u1", "s1", "query")
        call_kwargs = mock_svc.retrieve.call_args[1]
        for mt in call_kwargs["memory_types"]:
            assert mt != MemoryType.WORKING
        assert MemoryType.SEMANTIC in call_kwargs["memory_types"]
        assert MemoryType.PROCEDURAL in call_kwargs["memory_types"]

    def test_returns_empty_on_failure(self, loader, mock_svc):
        mock_svc.retrieve.side_effect = Exception("fail")
        result, _ = loader.load_l1("u1", "s1", "query")
        assert result == ""


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
    def test_invalidates_cache(self, loader, mock_svc):
        loader.invalidate_profile("u1")
        mock_svc.invalidate_profile.assert_called_once_with("u1")
