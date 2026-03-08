"""Graph memory DB integration tests — normalized edge table.

Tests:
1. DDL: both tables exist
2. Node CRUD: insert, read, batch, deactivate
3. Edge CRUD: batch insert, dedup, outgoing/incoming queries
4. Vector: l2_distance, cosine_distance, pair similarity
5. Conflict marking: atomic multi-row update
6. Multi-hop: neighbor expansion via edge table
7. Skeleton load: column-query without embedding
"""

from uuid import uuid4

import pytest

from api.models._constants import EMBEDDING_DIM
from core.memory.graph.graph_store import GraphStore
from core.memory.graph.types import Edge, EdgeType, GraphNodeData, NodeType


def _uid() -> str:
    return f"graph_e2e_{uuid4().hex[:12]}"


def _embed(seed: float = 0.1) -> list[float]:
    return [seed] * EMBEDDING_DIM


def _similar_embed() -> list[float]:
    e = [0.1] * EMBEDDING_DIM
    for i in range(EMBEDDING_DIM // 10):
        e[i] = 0.15
    return e


def _different_embed() -> list[float]:
    e = [0.0] * EMBEDDING_DIM
    e[0] = 1.0
    return e


@pytest.fixture
def db_factory():
    from api.database import SessionLocal
    return SessionLocal


@pytest.fixture
def store(db_factory):
    return GraphStore(db_factory)


@pytest.fixture
def user_id():
    return _uid()


@pytest.fixture(autouse=True)
def cleanup(db_factory, user_id):
    yield
    from sqlalchemy import text
    db = db_factory()
    try:
        db.execute(text("DELETE FROM memory_graph_edges WHERE user_id = :uid"), {"uid": user_id})
        db.execute(text("DELETE FROM memory_graph_nodes WHERE user_id = :uid"), {"uid": user_id})
        db.commit()
    finally:
        db.close()


class TestDDL:
    def test_both_tables_exist(self, db_factory):
        from sqlalchemy import inspect as sa_inspect
        db = db_factory()
        try:
            tables = set(sa_inspect(db.bind).get_table_names())
            assert "memory_graph_nodes" in tables
            assert "memory_graph_edges" in tables
        finally:
            db.close()


class TestNodeCRUD:
    def test_create_and_read(self, store, user_id):
        node = GraphNodeData(
            node_id=uuid4().hex, user_id=user_id,
            node_type=NodeType.EPISODIC, content="test",
            embedding=_embed(), event_id="evt1", session_id="s1",
            confidence=0.9, trust_tier="T2", importance=0.5,
        )
        store.create_node(node)

        loaded = store.get_node(node.node_id)
        assert loaded is not None
        assert loaded.content == "test"
        assert loaded.confidence == pytest.approx(0.9)
        assert loaded.trust_tier == "T2"
        assert loaded.is_active is True
        assert len(loaded.embedding) == EMBEDDING_DIM

    def test_batch_create(self, store, user_id):
        nodes = [
            GraphNodeData(
                node_id=uuid4().hex, user_id=user_id,
                node_type=NodeType.SEMANTIC, content=f"n{i}",
            )
            for i in range(5)
        ]
        store.create_nodes_batch(nodes)
        assert store.count_user_nodes(user_id) == 5

    def test_deactivate(self, store, user_id):
        node = GraphNodeData(
            node_id=uuid4().hex, user_id=user_id,
            node_type=NodeType.SEMANTIC, content="bye",
        )
        store.create_node(node)
        store.deactivate_node(node.node_id, superseded_by="new")

        loaded = store.get_node(node.node_id)
        assert loaded.is_active is False
        assert loaded.superseded_by == "new"
        assert store.count_user_nodes(user_id) == 0

    def test_conflict_resolution_persisted(self, store, user_id):
        node = GraphNodeData(
            node_id=uuid4().hex, user_id=user_id,
            node_type=NodeType.SEMANTIC, content="c",
            conflict_resolution="kept",
        )
        store.create_node(node)
        assert store.get_node(node.node_id).conflict_resolution == "kept"


class TestEdgeCRUD:
    def test_add_and_query_edges(self, store, user_id):
        a_id, b_id, c_id = uuid4().hex, uuid4().hex, uuid4().hex
        for nid, content in [(a_id, "a"), (b_id, "b"), (c_id, "c")]:
            store.create_node(GraphNodeData(
                node_id=nid, user_id=user_id,
                node_type=NodeType.SEMANTIC, content=content,
            ))

        store.add_edges_batch([
            (a_id, b_id, EdgeType.ASSOCIATION.value, 0.8),
            (a_id, c_id, EdgeType.TEMPORAL.value, 1.0),
            (b_id, c_id, EdgeType.CAUSAL.value, 1.5),
        ], user_id)

        # Outgoing from a
        out_a = store.get_outgoing_edges(a_id)
        assert len(out_a) == 2
        targets = {e.target_id for e in out_a}
        assert b_id in targets and c_id in targets

        # Incoming to c
        in_c = store.get_incoming_edges(c_id)
        assert len(in_c) == 2

        # Batch query
        all_out = store.get_edges_for_nodes({a_id, b_id})
        assert len(all_out[a_id]) == 2
        assert len(all_out[b_id]) == 1

    def test_duplicate_edge_not_added(self, store, user_id):
        a_id, b_id = uuid4().hex, uuid4().hex
        for nid in [a_id, b_id]:
            store.create_node(GraphNodeData(
                node_id=nid, user_id=user_id,
                node_type=NodeType.SEMANTIC, content="x",
            ))

        edge = [(a_id, b_id, EdgeType.ASSOCIATION.value, 0.8)]
        store.add_edges_batch(edge, user_id)
        store.add_edges_batch(edge, user_id)

        assert len(store.get_outgoing_edges(a_id)) == 1

    def test_neighbor_ids(self, store, user_id):
        a_id, b_id, c_id = uuid4().hex, uuid4().hex, uuid4().hex
        for nid in [a_id, b_id, c_id]:
            store.create_node(GraphNodeData(
                node_id=nid, user_id=user_id,
                node_type=NodeType.SEMANTIC, content="x",
            ))

        store.add_edges_batch([
            (a_id, b_id, "association", 0.8),
            (c_id, a_id, "temporal", 1.0),
        ], user_id)

        neighbors = store.get_neighbor_ids({a_id})
        assert b_id in neighbors  # outgoing
        assert c_id in neighbors  # incoming

    def test_association_edges_query(self, store, user_id):
        a_id, b_id = uuid4().hex, uuid4().hex
        for nid in [a_id, b_id]:
            store.create_node(GraphNodeData(
                node_id=nid, user_id=user_id,
                node_type=NodeType.SEMANTIC, content="x",
            ))

        store.add_edges_batch([
            (a_id, b_id, "association", 0.8),
            (a_id, b_id, "temporal", 1.0),  # different type, same pair
        ], user_id)

        assoc = store.get_association_edges(user_id, min_weight=0.7)
        assert len(assoc) == 1
        assert assoc[0][2] == pytest.approx(0.8)


class TestVectorSearch:
    def _seed(self, store, user_id):
        a = GraphNodeData(
            node_id=uuid4().hex, user_id=user_id,
            node_type=NodeType.SEMANTIC, content="A", embedding=_embed(0.1),
        )
        b = GraphNodeData(
            node_id=uuid4().hex, user_id=user_id,
            node_type=NodeType.SEMANTIC, content="B", embedding=_similar_embed(),
        )
        d = GraphNodeData(
            node_id=uuid4().hex, user_id=user_id,
            node_type=NodeType.SEMANTIC, content="D", embedding=_different_embed(),
        )
        store.create_nodes_batch([a, b, d])
        return a, b, d

    def test_l2_distance(self, store, user_id):
        a, b, d = self._seed(store, user_id)
        results = store.find_similar_nodes(user_id, _embed(0.1), top_k=3)
        assert results[0].node_id == a.node_id

    def test_cosine_with_scores(self, store, user_id):
        a, b, d = self._seed(store, user_id)
        results = store.find_similar_with_scores(user_id, _embed(0.1), top_k=3)
        assert results[0][0].node_id == a.node_id
        assert results[0][1] > 0.9
        diff_score = next(s for n, s in results if n.node_id == d.node_id)
        assert diff_score < results[0][1]

    def test_pair_similarity(self, store, user_id):
        a, b, d = self._seed(store, user_id)
        sim_ab = store.get_pair_similarity(a.node_id, b.node_id)
        sim_ad = store.get_pair_similarity(a.node_id, d.node_id)
        assert sim_ab is not None and sim_ad is not None
        assert sim_ab > sim_ad


class TestMarkConflict:
    def test_atomic(self, store, user_id):
        older = GraphNodeData(
            node_id=uuid4().hex, user_id=user_id,
            node_type=NodeType.SEMANTIC, content="old", confidence=0.8,
        )
        newer = GraphNodeData(
            node_id=uuid4().hex, user_id=user_id,
            node_type=NodeType.SEMANTIC, content="new", confidence=0.9,
        )
        store.create_nodes_batch([older, newer])

        store.mark_conflict(
            older_id=older.node_id, newer_id=newer.node_id,
            confidence_factor=0.5, old_confidence=0.8,
        )

        lo = store.get_node(older.node_id)
        assert lo.confidence == pytest.approx(0.4)
        assert lo.conflicts_with == newer.node_id
        assert lo.conflict_resolution == "superseded"

        ln = store.get_node(newer.node_id)
        assert ln.conflict_resolution == "kept"
        assert ln.confidence == pytest.approx(0.9)


class TestSkeletonLoad:
    def test_no_embedding(self, store, user_id):
        store.create_node(GraphNodeData(
            node_id=uuid4().hex, user_id=user_id,
            node_type=NodeType.SEMANTIC, content="test", embedding=_embed(),
        ))
        nodes = store.get_user_nodes(user_id, load_embedding=False)
        assert len(nodes) == 1
        assert nodes[0].embedding is None
        assert nodes[0].content == "test"


class TestOpinionEvolution:
    """§4.5 — opinion evolution against real MatrixOne."""

    def _seed_scene_and_event(self, store, user_id, scene_embed, event_embed):
        """Create a scene node and a new event node, return their IDs."""
        scene = GraphNodeData(
            node_id=uuid4().hex, user_id=user_id,
            node_type=NodeType.SCENE, content="User prefers verbose errors",
            embedding=scene_embed, confidence=0.5, trust_tier="T4",
        )
        event_node = GraphNodeData(
            node_id=uuid4().hex, user_id=user_id,
            node_type=NodeType.EPISODIC, content="Show me detailed errors",
            embedding=event_embed, confidence=1.0, trust_tier="T1",
        )
        store.create_nodes_batch([scene, event_node])

        # Edge so activation can reach the scene from the event
        store.add_edges_batch([
            (event_node.node_id, scene.node_id, EdgeType.ABSTRACTION.value, 0.8),
        ], user_id)

        return scene, event_node

    def test_supporting_evidence_increases_confidence(self, store, user_id):
        from core.memory.graph.opinion import evolve_opinions

        # Similar embeddings → high cosine similarity → supporting
        base = _embed(0.1)
        scene, event_node = self._seed_scene_and_event(store, user_id, base, _similar_embed())

        result = evolve_opinions(store, event_node.node_id, user_id)

        assert result.scenes_evaluated == 1
        assert result.supporting == 1

        # Verify DB state
        updated = store.get_node(scene.node_id)
        assert updated.confidence > 0.5  # increased

    def test_contradicting_evidence_decreases_confidence(self, store, user_id):
        from core.memory.graph.opinion import evolve_opinions

        # Very different embeddings → low cosine similarity → contradicting
        scene, event_node = self._seed_scene_and_event(
            store, user_id, _embed(0.1), _different_embed(),
        )

        result = evolve_opinions(store, event_node.node_id, user_id)

        assert result.scenes_evaluated == 1
        assert result.contradicting == 1

        updated = store.get_node(scene.node_id)
        assert updated.confidence < 0.5  # decreased

    def test_quarantine_deactivates_node(self, store, user_id):
        from core.memory.graph.opinion import evolve_opinions

        # Start at very low confidence, contradicting will push below quarantine
        scene = GraphNodeData(
            node_id=uuid4().hex, user_id=user_id,
            node_type=NodeType.SCENE, content="Weak belief",
            embedding=_embed(0.1), confidence=0.15, trust_tier="T4",
        )
        event_node = GraphNodeData(
            node_id=uuid4().hex, user_id=user_id,
            node_type=NodeType.EPISODIC, content="Contradicts",
            embedding=_different_embed(), confidence=1.0, trust_tier="T1",
        )
        store.create_nodes_batch([scene, event_node])
        store.add_edges_batch([
            (event_node.node_id, scene.node_id, EdgeType.ABSTRACTION.value, 0.8),
        ], user_id)

        result = evolve_opinions(store, event_node.node_id, user_id)

        assert result.quarantined == 1
        updated = store.get_node(scene.node_id)
        assert updated.is_active is False

    def test_opinion_does_not_promote_instantly(self, store, user_id):
        """§4.7: promotion requires age gate — opinion evolution only updates confidence."""
        from core.memory.graph.opinion import evolve_opinions

        scene = GraphNodeData(
            node_id=uuid4().hex, user_id=user_id,
            node_type=NodeType.SCENE, content="Strong belief",
            embedding=_embed(0.1), confidence=0.78, trust_tier="T4",
        )
        event_node = GraphNodeData(
            node_id=uuid4().hex, user_id=user_id,
            node_type=NodeType.EPISODIC, content="Supporting",
            embedding=_similar_embed(), confidence=1.0, trust_tier="T1",
        )
        store.create_nodes_batch([scene, event_node])
        store.add_edges_batch([
            (event_node.node_id, scene.node_id, EdgeType.ABSTRACTION.value, 0.8),
        ], user_id)

        result = evolve_opinions(store, event_node.node_id, user_id)

        assert result.supporting == 1
        updated = store.get_node(scene.node_id)
        assert updated.confidence > 0.8  # confidence increased past threshold
        assert updated.trust_tier == "T4"  # but tier NOT promoted — needs age gate

    def test_update_confidence_and_tier(self, store, user_id):
        """Verify the update_confidence_and_tier method works in real DB."""
        node = GraphNodeData(
            node_id=uuid4().hex, user_id=user_id,
            node_type=NodeType.SCENE, content="test",
            confidence=0.5, trust_tier="T4",
        )
        store.create_node(node)

        store.update_confidence_and_tier(node.node_id, 0.85, "T3")

        loaded = store.get_node(node.node_id)
        assert loaded.confidence == pytest.approx(0.85)
        assert loaded.trust_tier == "T3"


class TestTrustTierLifecycleE2E:
    """§4.7 — full lifecycle against real MatrixOne.

    Tests the complete closed loop:
    1. Scene created at T4 with low confidence
    2. Supporting evidence raises confidence above threshold
    3. Consolidation promotes T4→T3 (with age gate)
    4. Contradicting evidence drops confidence
    5. Consolidation demotes stale T3→T4
    """

    def test_full_promotion_lifecycle(self, store, user_id):
        """Closed-loop: create → support → promote → verify."""
        from core.memory.graph.consolidation import GraphConsolidator
        from core.memory.graph.opinion import evolve_opinions
        from api.database import SessionLocal

        consolidator = GraphConsolidator(SessionLocal)

        # 1. Create scene at T4, confidence 0.5
        scene = GraphNodeData(
            node_id=uuid4().hex, user_id=user_id,
            node_type=NodeType.SCENE, content="User prefers verbose errors",
            embedding=_embed(0.1), confidence=0.5, trust_tier="T4",
        )
        store.create_node(scene)

        # 2. Simulate multiple supporting events to push confidence above 0.8
        for i in range(7):
            ev = GraphNodeData(
                node_id=uuid4().hex, user_id=user_id,
                node_type=NodeType.EPISODIC, content=f"verbose error request {i}",
                embedding=_similar_embed(), confidence=1.0, trust_tier="T1",
            )
            store.create_node(ev)
            store.add_edges_batch([
                (ev.node_id, scene.node_id, EdgeType.ABSTRACTION.value, 0.8),
            ], user_id)
            evolve_opinions(store, ev.node_id, user_id)

        # Verify confidence rose above threshold
        after_support = store.get_node(scene.node_id)
        assert after_support.confidence >= 0.8
        assert after_support.trust_tier == "T4"  # not promoted yet — too young

        # 3. Consolidation should NOT promote (node is < 7 days old)
        result = consolidator.consolidate(user_id)
        assert result.promoted == 0

        still_t4 = store.get_node(scene.node_id)
        assert still_t4.trust_tier == "T4"

        # 4. Fake the age by updating created_at directly
        from sqlalchemy import text
        from api.models.graph import GraphNode
        with SessionLocal() as db:
            db.query(GraphNode).filter_by(node_id=scene.node_id).update({
                "created_at": text("DATE_SUB(NOW(), INTERVAL 10 DAY)"),
            })
            db.commit()

        # 5. Now consolidation should promote T4→T3
        result2 = consolidator.consolidate(user_id)
        assert result2.promoted == 1

        promoted = store.get_node(scene.node_id)
        assert promoted.trust_tier == "T3"
        assert promoted.confidence >= 0.8

    def test_full_demotion_lifecycle(self, store, user_id):
        """Closed-loop: T3 scene with low confidence + old age → demoted to T4."""
        from core.memory.graph.consolidation import GraphConsolidator
        from api.database import SessionLocal

        consolidator = GraphConsolidator(SessionLocal)

        # 1. Create scene at T3, confidence below threshold
        scene = GraphNodeData(
            node_id=uuid4().hex, user_id=user_id,
            node_type=NodeType.SCENE, content="Stale belief",
            embedding=_embed(0.1), confidence=0.5, trust_tier="T3",
        )
        store.create_node(scene)

        # 2. Not old enough yet — should NOT demote
        result = consolidator.consolidate(user_id)
        assert result.demoted == 0

        # 3. Fake age to 65 days
        from sqlalchemy import text
        from api.models.graph import GraphNode
        with SessionLocal() as db:
            db.query(GraphNode).filter_by(node_id=scene.node_id).update({
                "created_at": text("DATE_SUB(NOW(), INTERVAL 65 DAY)"),
            })
            db.commit()

        # 4. Now consolidation should demote T3→T4
        result2 = consolidator.consolidate(user_id)
        assert result2.demoted == 1

        demoted = store.get_node(scene.node_id)
        assert demoted.trust_tier == "T4"
        assert demoted.confidence == pytest.approx(0.5)  # confidence unchanged
