"""Unit tests for TieredMemoryLoader — Task 8."""

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
            loader._retriever.retrieve.return_value = [
                Memory(memory_id="m1", user_id="u1", memory_type=MemoryType.EPISODIC,
                       content="discussed testing", confidence=0.8),
            ]
            result = loader.load_l1("u1", "testing")
            assert "discussed testing" in result
            assert "[episodic]" in result

    def test_returns_empty_when_no_memories(self, loader):
        with patch.object(loader, '_ensure_initialized', return_value=True):
            loader._retriever = MagicMock()
            loader._retriever.retrieve.return_value = []
            assert loader.load_l1("u1", "query") == ""


class TestBuildSection:
    def test_combines_l0_and_l1(self, loader):
        loader.load_l0 = MagicMock(return_value="User Profile:\n- likes Go")
        loader.load_l1 = MagicMock(return_value="Relevant Memories:\n- [episodic] test")
        result = loader.build_section("u1", "query")
        assert "User Profile" in result
        assert "Relevant Memories" in result

    def test_handles_empty_l0(self, loader):
        loader.load_l0 = MagicMock(return_value="")
        loader.load_l1 = MagicMock(return_value="Relevant Memories:\n- test")
        result = loader.build_section("u1", "query")
        assert "Relevant Memories" in result
        assert "User Profile" not in result

    def test_handles_both_empty(self, loader):
        loader.load_l0 = MagicMock(return_value="")
        loader.load_l1 = MagicMock(return_value="")
        result = loader.build_section("u1", "query")
        assert result == ""


class TestInvalidateProfile:
    def test_invalidates_cache(self, loader):
        loader._profile_mgr = MagicMock()
        loader.invalidate_profile("u1")
        loader._profile_mgr.invalidate.assert_called_once_with("u1")
