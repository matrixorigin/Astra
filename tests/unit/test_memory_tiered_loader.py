"""Unit tests for TieredMemoryLoader (context-layer, MemoryService-based)."""

from unittest.mock import MagicMock

import pytest

from core.context.tiered_loader import TieredMemoryLoader
from core.memory.explain import RetrievalStats
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
        mock_svc.retrieve.return_value = (
            [Memory(memory_id="m1", user_id="u1", memory_type=MemoryType.SEMANTIC,
                    content="discussed testing", initial_confidence=0.8)],
            None,
        )
        result, stats = loader.load_l1("u1", "s1", "testing")
        assert "discussed testing" in result
        assert "[semantic]" in result
        assert stats is None

    def test_returns_empty_when_no_memories(self, loader, mock_svc):
        mock_svc.retrieve.return_value = ([], None)
        result, stats = loader.load_l1("u1", "s1", "query")
        assert result == ""

    def test_no_episodic_in_retrieval_types(self, loader, mock_svc):
        """L1 retrieval should not request WORKING type."""
        mock_svc.retrieve.return_value = ([], None)
        loader.load_l1("u1", "s1", "query")
        call_kwargs = mock_svc.retrieve.call_args[1]
        for mt in call_kwargs["memory_types"]:
            assert mt != MemoryType.WORKING
        assert MemoryType.SEMANTIC in call_kwargs["memory_types"]
        assert MemoryType.PROCEDURAL in call_kwargs["memory_types"]

    def test_returns_empty_on_failure(self, loader, mock_svc):
        mock_svc.retrieve.side_effect = Exception("fail")
        result, stats = loader.load_l1("u1", "s1", "query")
        assert result == ""
        assert stats is None

    def test_passes_explain_to_service(self, loader, mock_svc):
        """explain=True must be forwarded to MemoryService.retrieve()."""
        ret_stats = RetrievalStats(
            keyword_attempted=True, keyword_hit=True,
            phase1_candidates=2, final_count=2,
        )
        mock_svc.retrieve.return_value = (
            [Memory(memory_id="m1", user_id="u1", memory_type=MemoryType.SEMANTIC,
                    content="fact", initial_confidence=0.8)],
            ret_stats,
        )
        _, stats = loader.load_l1("u1", "s1", "query", explain=True)
        assert mock_svc.retrieve.call_args[1]["explain"] is True
        assert stats is ret_stats

    def test_explain_false_no_stats(self, loader, mock_svc):
        """explain=False should forward False and return None stats."""
        mock_svc.retrieve.return_value = (
            [Memory(memory_id="m1", user_id="u1", memory_type=MemoryType.SEMANTIC,
                    content="fact", initial_confidence=0.8)],
            None,
        )
        _, stats = loader.load_l1("u1", "s1", "query", explain=False)
        assert mock_svc.retrieve.call_args[1]["explain"] is False
        assert stats is None

    def test_returns_stats_even_when_no_memories(self, loader, mock_svc):
        """When retrieval returns 0 memories but has stats, stats should flow through."""
        ret_stats = RetrievalStats(keyword_attempted=True, keyword_hit=False, final_count=0)
        mock_svc.retrieve.return_value = ([], ret_stats)
        result, stats = loader.load_l1("u1", "s1", "query", explain=True)
        assert result == ""
        assert stats is ret_stats


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

    def test_explain_propagates_retrieval_stats(self, loader):
        """build_section with explain=True must propagate RetrievalStats from L1."""
        ret_stats = RetrievalStats(
            keyword_attempted=True, keyword_hit=True,
            phase1_candidates=3, final_count=2,
            total_ms=5.0,
        )
        loader.load_l0 = MagicMock(return_value="")
        loader.load_l1 = MagicMock(return_value=("Relevant Memories:\n- fact1\n- fact2", ret_stats))
        _, stats = loader.build_section("u1", "s1", "query", explain=True)
        assert stats is not None
        assert stats.retrieval is ret_stats
        assert stats.retrieval.keyword_hit is True
        assert stats.retrieval.phase1_candidates == 3
        assert stats.l1_loaded is True
        assert stats.l1_count == 2
        assert stats.l1_tokens > 0

    def test_explain_retrieval_none_when_l1_empty(self, loader):
        """When L1 returns no memories and no stats, retrieval should be None."""
        loader.load_l0 = MagicMock(return_value="profile")
        loader.load_l1 = MagicMock(return_value=("", None))
        _, stats = loader.build_section("u1", "s1", "query", explain=True)
        assert stats is not None
        assert stats.retrieval is None
        assert stats.l1_loaded is False


class TestInvalidateProfile:
    def test_invalidates_cache(self, loader, mock_svc):
        loader.invalidate_profile("u1")
        mock_svc.invalidate_profile.assert_called_once_with("u1")
