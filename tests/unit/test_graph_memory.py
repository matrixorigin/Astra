"""Unit tests for graph memory — Phase 1: types, store, builder, service."""

from unittest.mock import MagicMock

import pytest

from core.memory.graph.types import Edge, EdgeType, GraphNodeData, NodeType
from core.memory.graph.graph_builder import _compute_ingest_importance


class TestEdge:
    def test_fields(self):
        e = Edge("tgt", "causal", 1.5)
        assert e.target_id == "tgt"
        assert e.edge_type == "causal"
        assert e.weight == 1.5

    def test_default_weight(self):
        e = Edge("tgt", "temporal")
        assert e.weight == 1.0


class TestNodeType:
    def test_values(self):
        assert NodeType.EPISODIC.value == "episodic"
        assert NodeType.SEMANTIC.value == "semantic"
        assert NodeType.SCENE.value == "scene"


class TestEdgeType:
    def test_all_types(self):
        assert len(EdgeType) == 5


class TestGraphNodeData:
    def test_defaults(self):
        n = GraphNodeData(node_id="n1", user_id="u1", node_type=NodeType.SEMANTIC, content="test")
        assert n.confidence == 0.75
        assert n.trust_tier == "T3"
        assert n.importance == 0.0
        assert n.is_active is True
        assert n.source_nodes == []
        assert n.embedding is None


class TestIngestImportance:
    def test_episodic_base(self):
        assert _compute_ingest_importance(NodeType.EPISODIC) == pytest.approx(0.3)

    def test_semantic_base(self):
        assert _compute_ingest_importance(NodeType.SEMANTIC) == pytest.approx(0.5)

    def test_scene_base(self):
        assert _compute_ingest_importance(NodeType.SCENE) == pytest.approx(0.6)

    def test_tool_error_boost(self):
        val = _compute_ingest_importance(NodeType.EPISODIC, event={"event_type": "tool_error"})
        assert val == pytest.approx(0.5)

    def test_correction_boost(self):
        val = _compute_ingest_importance(
            NodeType.EPISODIC,
            event={"event_type": "user_query", "content": "No, that's wrong"},
        )
        assert val == pytest.approx(0.55)

    def test_high_confidence_memory_boost(self):
        mem = MagicMock()
        mem.initial_confidence = 0.9
        val = _compute_ingest_importance(NodeType.SEMANTIC, memory=mem)
        assert val == pytest.approx(0.6)

    def test_capped_at_1(self):
        mem = MagicMock()
        mem.initial_confidence = 0.9
        val = _compute_ingest_importance(
            NodeType.SCENE, memory=mem, neighbor_count=5,
        )
        assert val <= 1.0


class TestFactory:
    def test_tabular_is_default(self):
        from core.memory.factory import create_memory_service
        svc = create_memory_service(lambda: MagicMock(), backend="tabular")
        assert svc.__class__.__name__ == "MemoryService"
        assert svc.strategy_key == "vector:v1"

    def test_graph_backend(self):
        from core.memory.factory import create_memory_service
        svc = create_memory_service(lambda: MagicMock(), backend="graph")
        assert svc.__class__.__name__ == "MemoryService"
        assert svc.strategy_key == "activation:v1"


class TestGraphBuilder:
    def _make_builder(self):
        store = MagicMock()
        store.get_latest_episodic_in_session.return_value = None
        store.get_node_by_event_id.return_value = None
        store.get_node_by_memory_id.return_value = None
        store.find_similar_with_scores.return_value = []
        from core.memory.graph.graph_builder import GraphBuilder
        return GraphBuilder(store), store

    def _make_memory(self, mid="m1", sid="s1"):
        mem = MagicMock()
        mem.memory_id = mid
        mem.content = "test content"
        mem.embedding = [0.1] * 10
        mem.initial_confidence = 0.75
        mem.trust_tier = "T3"
        mem.session_id = sid
        return mem

    def test_ingest_creates_episodic_and_semantic_nodes(self):
        builder, store = self._make_builder()
        mem = self._make_memory()
        events = [{"event_id": "e1", "event_type": "user_query", "content": "hello"}]

        result = builder.ingest("u1", [mem], events, session_id="s1")

        assert len(result) == 2
        assert store.create_nodes_batch.call_count == 2

    def test_ingest_skips_existing_episodic(self):
        builder, store = self._make_builder()
        existing = GraphNodeData(
            node_id="existing", user_id="u1",
            node_type=NodeType.EPISODIC, content="old",
        )
        store.get_node_by_event_id.return_value = existing

        events = [{"event_id": "e1", "event_type": "user_query", "content": "hello"}]
        result = builder.ingest("u1", [], events, session_id="s1")

        assert len(result) == 1
        assert result[0].node_id == "existing"
        store.create_nodes_batch.assert_not_called()

    def test_ingest_builds_temporal_edges(self):
        builder, store = self._make_builder()
        prev = GraphNodeData(
            node_id="prev", user_id="u1",
            node_type=NodeType.EPISODIC, content="prev",
        )
        store.get_latest_episodic_in_session.return_value = prev

        events = [{"event_id": "e1", "event_type": "user_query", "content": "hello"}]
        builder.ingest("u1", [], events, session_id="s1")

        store.add_edges_batch.assert_called_once()
        edges = store.add_edges_batch.call_args[0][0]
        temporal = [e for e in edges if e[2] == "temporal"]
        assert len(temporal) == 1
        assert temporal[0][0] == "prev"

    def test_ingest_builds_abstraction_edges(self):
        builder, store = self._make_builder()
        mem = self._make_memory(sid="s1")
        events = [{"event_id": "e1", "event_type": "user_query", "content": "hello"}]

        builder.ingest("u1", [mem], events, session_id="s1")

        store.add_edges_batch.assert_called_once()
        edges = store.add_edges_batch.call_args[0][0]
        abstraction = [e for e in edges if e[2] == "abstraction"]
        assert len(abstraction) == 1

    def test_ingest_builds_association_edges_with_db_similarity(self):
        builder, store = self._make_builder()
        mem = self._make_memory()

        candidate = GraphNodeData(
            node_id="existing_sem", user_id="u1",
            node_type=NodeType.SEMANTIC, content="related",
        )
        store.find_similar_with_scores.return_value = [(candidate, 0.85)]

        builder.ingest("u1", [mem], [], session_id="s1")

        store.add_edges_batch.assert_called_once()
        edges = store.add_edges_batch.call_args[0][0]
        assoc = [e for e in edges if e[2] == "association"]
        assert len(assoc) == 1
        assert assoc[0][3] == 0.85


class TestGraphServiceWiring:
    def test_store_delegates_to_tabular(self):
        from core.memory.graph.service import GraphMemoryService
        from core.memory.types import MemoryType, TrustTier

        svc = GraphMemoryService(lambda: MagicMock())
        svc._tabular = MagicMock()
        mock_mem = MagicMock()
        mock_mem.memory_id = "mem-1"
        svc._tabular.store.return_value = mock_mem

        # Mock graph builder so it doesn't hit real DB
        mock_builder = MagicMock()
        mock_builder.ingest.return_value = []
        svc._graph_builder = mock_builder

        result = svc.store(
            "u1", "test", memory_type=MemoryType.SEMANTIC,
            trust_tier=TrustTier.T3_INFERRED,
        )
        assert result == mock_mem
        svc._tabular.store.assert_called_once()
        mock_builder.ingest.assert_called_once()


class TestGraphServiceErrorHandling:
    """Regression: GraphMemoryService must only catch recoverable errors."""

    def _make_service(self) -> "GraphMemoryService":
        from core.memory.graph.service import GraphMemoryService
        svc = GraphMemoryService(lambda: MagicMock())
        svc._tabular = MagicMock()
        return svc

    def test_retrieve_propagates_programming_error(self):
        """TypeError in activation retriever must NOT be swallowed."""
        svc = self._make_service()
        mock_retriever = MagicMock()
        mock_retriever.retrieve.side_effect = TypeError("bad arg")
        svc._activation_retriever = mock_retriever

        with pytest.raises(TypeError, match="bad arg"):
            svc.retrieve("u1", "query", query_embedding=[0.1])

    def test_retrieve_catches_db_error(self):
        """SQLAlchemy errors in retriever should fall back to tabular."""
        from sqlalchemy.exc import OperationalError
        svc = self._make_service()
        mock_retriever = MagicMock()
        mock_retriever.retrieve.side_effect = OperationalError("db", {}, Exception())
        svc._activation_retriever = mock_retriever
        svc._tabular.retrieve.return_value = ["fallback"]

        result = svc.retrieve("u1", "query", query_embedding=[0.1])
        assert result == ["fallback"]

    def test_store_queues_pending_on_graph_failure(self):
        """Graph ingest failure queues memory_id for retry."""
        from sqlalchemy.exc import OperationalError
        svc = self._make_service()
        mock_mem = MagicMock()
        mock_mem.memory_id = "mem-123"
        svc._tabular.store.return_value = mock_mem

        mock_builder = MagicMock()
        mock_builder.ingest.side_effect = OperationalError("db", {}, Exception())
        svc._graph_builder = mock_builder

        result = svc.store("u1", "test", memory_type=MagicMock(), trust_tier=MagicMock())
        assert result.memory_id == "mem-123"
        assert svc.pending_graph_sync_count == 1
        assert svc.drain_pending_graph_sync() == ["mem-123"]
        assert svc.pending_graph_sync_count == 0

    def test_store_propagates_programming_error_from_graph(self):
        """TypeError in graph builder must NOT be swallowed."""
        svc = self._make_service()
        svc._tabular.store.return_value = MagicMock(memory_id="mem-1")

        mock_builder = MagicMock()
        mock_builder.ingest.side_effect = TypeError("wrong type")
        svc._graph_builder = mock_builder

        with pytest.raises(TypeError, match="wrong type"):
            svc.store("u1", "test", memory_type=MagicMock(), trust_tier=MagicMock())

    def test_observe_turn_queues_pending_on_graph_failure(self):
        """Graph ingest failure during observe_turn queues for retry."""
        from sqlalchemy.exc import OperationalError
        svc = self._make_service()
        mock_mem = MagicMock()
        mock_mem.memory_id = "mem-456"
        mock_mem.session_id = "s1"
        svc._tabular.observe_turn.return_value = [mock_mem]

        mock_builder = MagicMock()
        mock_builder.ingest.side_effect = OperationalError("db", {}, Exception())
        svc._graph_builder = mock_builder

        result = svc.observe_turn("u1", [{"role": "user", "content": "hi"}])
        assert len(result) == 1
        assert svc.pending_graph_sync_count == 1

    def test_candidates_propagates_programming_error(self):
        """TypeError in graph candidates must NOT be swallowed."""
        svc = self._make_service()
        mock_candidates = MagicMock()
        mock_candidates.get_reflection_candidates.side_effect = TypeError("bad")
        svc._graph_candidates = mock_candidates

        with pytest.raises(TypeError, match="bad"):
            svc.get_reflection_candidates("u1")
