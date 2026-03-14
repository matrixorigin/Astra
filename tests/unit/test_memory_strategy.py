"""Tests for the new memory architecture: strategy registry, descriptor, and service facade.

Phase 0 verification: MemoriaStorage + RetrievalStrategy + IndexManager.
"""

from __future__ import annotations

from unittest.mock import MagicMock

import pytest

from core.memory.strategy.registry import StrategyDescriptor, StrategyRegistry
from core.memory.types import MemoryType


class TestStrategyDescriptor:
    def test_parse_valid(self):
        d = StrategyDescriptor.parse("vector:v1")
        assert d.strategy_type == "vector"
        assert d.version == "v1"
        assert d.key == "vector:v1"
        assert d.params == {}

    def test_parse_with_params(self):
        d = StrategyDescriptor.parse("activation:v1", {"spreading_factor": 0.9})
        assert d.params == {"spreading_factor": 0.9}

    def test_parse_invalid_no_colon(self):
        with pytest.raises(ValueError, match="Invalid strategy key"):
            StrategyDescriptor.parse("tabular")

    def test_frozen(self):
        d = StrategyDescriptor.parse("vector:v1")
        with pytest.raises(AttributeError):
            d.strategy_type = "other"


class TestStrategyRegistry:
    def test_register_and_create(self):
        reg = StrategyRegistry()
        mock_factory = MagicMock(return_value="strategy_instance")
        reg.register("test:v1", mock_factory)

        desc = StrategyDescriptor.parse("test:v1")
        result = reg.create_strategy(desc, db_factory=lambda: None)
        assert result == "strategy_instance"
        mock_factory.assert_called_once()

    def test_unknown_strategy_raises(self):
        reg = StrategyRegistry()
        desc = StrategyDescriptor.parse("unknown:v1")
        with pytest.raises(ValueError, match="Unknown strategy"):
            reg.create_strategy(desc)

    def test_list_available(self):
        reg = StrategyRegistry()
        reg.register("a:v1", MagicMock())
        reg.register("b:v2", MagicMock())
        assert sorted(reg.list_available()) == ["a:v1", "b:v2"]

    def test_index_manager_none_when_not_registered(self):
        reg = StrategyRegistry()
        reg.register("test:v1", MagicMock())
        desc = StrategyDescriptor.parse("test:v1")
        assert reg.create_index_manager(desc) is None

    def test_index_manager_created_when_registered(self):
        reg = StrategyRegistry()
        mock_idx = MagicMock(return_value="index_instance")
        reg.register("test:v1", MagicMock(), mock_idx)
        desc = StrategyDescriptor.parse("test:v1")
        assert reg.create_index_manager(desc) == "index_instance"


class TestFactoryStrategyResolution:
    def test_tabular_maps_to_vector_v1(self):
        from core.memory.factory import _resolve_strategy
        assert _resolve_strategy(None, None, "tabular", None) == "vector:v1"

    def test_graph_maps_to_activation_v1(self):
        from core.memory.factory import _resolve_strategy
        assert _resolve_strategy(None, None, "graph", None) == "activation:v1"

    def test_explicit_strategy_overrides_backend(self):
        from core.memory.factory import _resolve_strategy
        assert _resolve_strategy(None, None, "graph", "vector:v1") == "vector:v1"

    def test_env_fallback(self, monkeypatch):
        from core.memory.factory import _resolve_strategy
        monkeypatch.setenv("MEM_RETRIEVAL_STRATEGY", "activation:v1")
        assert _resolve_strategy(None, None, None, None) == "activation:v1"

    def test_hardcoded_fallback(self, monkeypatch):
        from core.memory.factory import _resolve_strategy
        monkeypatch.delenv("MEM_RETRIEVAL_STRATEGY", raising=False)
        assert _resolve_strategy(None, None, None, None) == "activation:v1"


class TestMemoryServiceFacade:
    def test_store_notifies_index_manager(self):
        from core.memory.service import MemoryService
        from core.memory.types import Memory, MemoryType

        storage = MagicMock()
        retrieval = MagicMock()
        retrieval.strategy_key = "test:v1"
        index_mgr = MagicMock()

        mem = Memory(
            memory_id="m1", user_id="alice",
            memory_type=MemoryType.SEMANTIC, content="test",
        )
        storage.store.return_value = mem

        svc = MemoryService(storage, retrieval, index_mgr)
        result = svc.store("alice", "test", memory_type=MemoryType.SEMANTIC)

        assert result == mem
        storage.store.assert_called_once()
        index_mgr.on_memories_stored.assert_called_once_with(
            "alice", [mem], session_id=None,
        )

    def test_store_without_index_manager(self):
        from core.memory.service import MemoryService
        from core.memory.types import Memory, MemoryType

        storage = MagicMock()
        retrieval = MagicMock()
        retrieval.strategy_key = "vector:v1"

        mem = Memory(
            memory_id="m1", user_id="alice",
            memory_type=MemoryType.SEMANTIC, content="test",
        )
        storage.store.return_value = mem

        svc = MemoryService(storage, retrieval, index_manager=None)
        result = svc.store("alice", "test", memory_type=MemoryType.SEMANTIC)
        assert result == mem

    def test_governance_calls_index_manager(self):
        from core.memory.interfaces import GovernanceReport
        from core.memory.service import MemoryService

        storage = MagicMock()
        storage.run_governance.return_value = GovernanceReport()
        retrieval = MagicMock()
        retrieval.strategy_key = "test:v1"
        index_mgr = MagicMock()

        svc = MemoryService(storage, retrieval, index_mgr)
        svc.run_governance("alice")

        storage.run_governance.assert_called_once_with("alice")
        index_mgr.on_governance.assert_called_once_with("alice")

    def test_retrieve_delegates_to_strategy(self):
        from core.memory.service import MemoryService

        storage = MagicMock()
        retrieval = MagicMock()
        retrieval.strategy_key = "vector:v1"
        retrieval.retrieve.return_value = ([], None)

        svc = MemoryService(storage, retrieval)
        svc.retrieve("alice", "test query")

        retrieval.retrieve.assert_called_once()


class TestNodeTypeToMemoryType:
    """Verify _node_type_to_memory_type preserves original node type."""

    def test_episodic_maps_to_working(self):
        from core.memory.graph.types import NodeType
        from core.memory.strategy.activation_v1 import _node_type_to_memory_type
        assert _node_type_to_memory_type(NodeType.EPISODIC) == MemoryType.WORKING

    def test_semantic_maps_to_semantic(self):
        from core.memory.graph.types import NodeType
        from core.memory.strategy.activation_v1 import _node_type_to_memory_type
        assert _node_type_to_memory_type(NodeType.SEMANTIC) == MemoryType.SEMANTIC

    def test_scene_maps_to_semantic(self):
        from core.memory.graph.types import NodeType
        from core.memory.strategy.activation_v1 import _node_type_to_memory_type
        assert _node_type_to_memory_type(NodeType.SCENE) == MemoryType.SEMANTIC

    def test_unknown_string_defaults_to_semantic(self):
        from core.memory.strategy.activation_v1 import _node_type_to_memory_type
        assert _node_type_to_memory_type("nonexistent") == MemoryType.SEMANTIC


class TestReflectionCandidatesProtocol:
    """Verify get_reflection_candidates uses protocol, not hasattr."""

    def test_uses_index_manager_candidates(self):
        from core.memory.service import MemoryService

        storage = MagicMock()
        retrieval = MagicMock()
        retrieval.strategy_key = "activation:v1"
        index_mgr = MagicMock()
        index_mgr.get_reflection_candidates.return_value = ["candidate_1"]

        svc = MemoryService(storage, retrieval, index_mgr)
        result = svc.get_reflection_candidates("alice")

        assert result == ["candidate_1"]
        index_mgr.get_reflection_candidates.assert_called_once_with(
            "alice", since_hours=24,
        )
        storage.get_reflection_candidates.assert_not_called()

    def test_falls_back_when_index_returns_none(self):
        from core.memory.service import MemoryService

        storage = MagicMock()
        storage.get_reflection_candidates.return_value = ["canonical_candidate"]
        retrieval = MagicMock()
        retrieval.strategy_key = "activation:v1"
        index_mgr = MagicMock()
        index_mgr.get_reflection_candidates.return_value = None

        svc = MemoryService(storage, retrieval, index_mgr)
        result = svc.get_reflection_candidates("alice")

        assert result == ["canonical_candidate"]

    def test_no_index_manager_uses_canonical(self):
        from core.memory.service import MemoryService

        storage = MagicMock()
        storage.get_reflection_candidates.return_value = ["canonical"]
        retrieval = MagicMock()
        retrieval.strategy_key = "vector:v1"

        svc = MemoryService(storage, retrieval, index_manager=None)
        result = svc.get_reflection_candidates("alice")

        assert result == ["canonical"]


class TestGraphSpecificMethods:
    """Verify get_graph_stats/consolidate use getattr, not hasattr on privates."""

    def test_get_graph_stats_with_capable_index(self):
        from core.memory.service import MemoryService

        storage = MagicMock()
        retrieval = MagicMock()
        retrieval.strategy_key = "activation:v1"
        index_mgr = MagicMock()
        index_mgr.get_graph_stats.return_value = {"total_nodes": 42}

        svc = MemoryService(storage, retrieval, index_mgr)
        assert svc.get_graph_stats("alice") == {"total_nodes": 42}

    def test_get_graph_stats_without_method(self):
        from core.memory.service import MemoryService

        storage = MagicMock()
        retrieval = MagicMock()
        retrieval.strategy_key = "vector:v1"
        # index_manager without get_graph_stats
        index_mgr = MagicMock(spec=[])

        svc = MemoryService(storage, retrieval, index_mgr)
        assert svc.get_graph_stats("alice") == {"total_nodes": 0}

    def test_get_graph_stats_no_index_manager(self):
        from core.memory.service import MemoryService

        storage = MagicMock()
        retrieval = MagicMock()
        retrieval.strategy_key = "vector:v1"

        svc = MemoryService(storage, retrieval, index_manager=None)
        assert svc.get_graph_stats("alice") == {"total_nodes": 0}

    def test_consolidate_with_capable_index(self):
        from core.memory.service import MemoryService

        storage = MagicMock()
        retrieval = MagicMock()
        index_mgr = MagicMock()
        index_mgr.consolidate.return_value = "consolidation_result"

        svc = MemoryService(storage, retrieval, index_mgr)
        assert svc.consolidate("alice") == "consolidation_result"

    def test_consolidate_no_index_manager(self):
        from core.memory.service import MemoryService

        storage = MagicMock()
        retrieval = MagicMock()

        svc = MemoryService(storage, retrieval, index_manager=None)
        assert svc.consolidate("alice") is None
