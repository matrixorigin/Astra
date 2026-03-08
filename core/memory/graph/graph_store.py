"""GraphStore — CRUD for graph nodes and edges (normalized edge table)."""

from __future__ import annotations

import logging
import uuid
from typing import TYPE_CHECKING

from api.models.graph import GraphEdge, GraphNode
from core.db_consumer import DbConsumer
from core.memory.graph.types import Edge, GraphNodeData, NodeType

if TYPE_CHECKING:
    pass

logger = logging.getLogger(__name__)

MAX_EDGES_PER_NODE = 30


def _new_id() -> str:
    return uuid.uuid4().hex


def _to_domain(row: GraphNode) -> GraphNodeData:
    """Convert ORM row to domain object."""
    source_nodes = row.source_nodes.split(",") if row.source_nodes else []
    return GraphNodeData(
        node_id=row.node_id,
        user_id=row.user_id,
        node_type=NodeType(row.node_type),
        content=row.content,
        embedding=list(row.embedding) if row.embedding is not None else None,
        event_id=row.event_id,
        memory_id=row.memory_id,
        session_id=row.session_id,
        confidence=row.confidence or 0.75,
        trust_tier=row.trust_tier or "T3",
        importance=row.importance or 0.0,
        source_nodes=source_nodes,
        conflicts_with=row.conflicts_with,
        conflict_resolution=row.conflict_resolution,
        access_count=row.access_count or 0,
        cross_session_count=row.cross_session_count or 0,
        is_active=bool(row.is_active),
        superseded_by=row.superseded_by,
        created_at=str(row.created_at) if row.created_at else None,
    )


def _row_tuple_to_domain(row) -> GraphNodeData:
    """Convert a column-query Row (skeleton/partial load) to domain."""
    source_nodes = row.source_nodes.split(",") if row.source_nodes else []
    return GraphNodeData(
        node_id=row.node_id,
        user_id=row.user_id,
        node_type=NodeType(row.node_type),
        content=getattr(row, "content", ""),
        embedding=None,
        event_id=getattr(row, "event_id", None),
        memory_id=getattr(row, "memory_id", None),
        session_id=row.session_id,
        confidence=row.confidence or 0.75,
        trust_tier=row.trust_tier or "T3",
        importance=row.importance or 0.0,
        source_nodes=source_nodes,
        conflicts_with=row.conflicts_with,
        conflict_resolution=row.conflict_resolution,
        access_count=getattr(row, "access_count", 0) or 0,
        cross_session_count=getattr(row, "cross_session_count", 0) or 0,
        is_active=bool(row.is_active),
        superseded_by=getattr(row, "superseded_by", None),
        created_at=str(row.created_at) if getattr(row, "created_at", None) else None,
    )


def _to_row(node: GraphNodeData) -> dict:
    """Convert domain object to column dict for INSERT."""
    return {
        "node_id": node.node_id,
        "user_id": node.user_id,
        "node_type": node.node_type.value if isinstance(node.node_type, NodeType) else node.node_type,
        "content": node.content,
        "embedding": node.embedding,
        "event_id": node.event_id,
        "memory_id": node.memory_id,
        "session_id": node.session_id,
        "confidence": node.confidence,
        "trust_tier": node.trust_tier,
        "importance": node.importance,
        "source_nodes": ",".join(node.source_nodes) if node.source_nodes else None,
        "conflicts_with": node.conflicts_with,
        "conflict_resolution": node.conflict_resolution,
        "access_count": node.access_count,
        "cross_session_count": node.cross_session_count,
        "is_active": 1 if node.is_active else 0,
        "superseded_by": node.superseded_by,
    }


class GraphStore(DbConsumer):
    """CRUD for graph nodes + normalized edge table.

    Edges live in memory_graph_edges — no JSON adjacency lists.
    All graph traversal is DB-side.
    """

    # ── Node Create ───────────────────────────────────────────────────

    def create_node(self, node: GraphNodeData) -> GraphNodeData:
        if not node.node_id:
            node.node_id = _new_id()
        with self._db() as db:
            db.add(GraphNode(**_to_row(node)))
            db.commit()
        return node

    def create_nodes_batch(self, nodes: list[GraphNodeData]) -> list[GraphNodeData]:
        if not nodes:
            return []
        for n in nodes:
            if not n.node_id:
                n.node_id = _new_id()
        with self._db() as db:
            db.bulk_save_objects([GraphNode(**_to_row(n)) for n in nodes])
            db.commit()
        return nodes

    # ── Node Read ─────────────────────────────────────────────────────

    def get_node(self, node_id: str) -> GraphNodeData | None:
        with self._db() as db:
            row = db.query(GraphNode).filter_by(node_id=node_id).first()
            return _to_domain(row) if row else None

    def get_nodes_by_ids(self, node_ids: list[str]) -> list[GraphNodeData]:
        if not node_ids:
            return []
        with self._db() as db:
            rows = db.query(GraphNode).filter(GraphNode.node_id.in_(node_ids)).all()
            return [_to_domain(r) for r in rows]

    def get_user_nodes(
        self,
        user_id: str,
        *,
        node_type: NodeType | None = None,
        active_only: bool = True,
        load_embedding: bool = True,
    ) -> list[GraphNodeData]:
        with self._db() as db:
            if load_embedding:
                q = db.query(GraphNode).filter_by(user_id=user_id)
            else:
                cols = [c for c in GraphNode.__table__.columns if c.name != "embedding"]
                q = db.query(*cols).filter_by(user_id=user_id)
            if active_only:
                q = q.filter(GraphNode.is_active == 1)
            if node_type is not None:
                q = q.filter_by(node_type=node_type.value)
            if load_embedding:
                return [_to_domain(r) for r in q.all()]
            return [_row_tuple_to_domain(r) for r in q.all()]

    def get_node_by_event_id(self, event_id: str) -> GraphNodeData | None:
        with self._db() as db:
            row = (
                db.query(GraphNode)
                .filter_by(event_id=event_id, node_type=NodeType.EPISODIC.value)
                .first()
            )
            return _to_domain(row) if row else None

    def get_node_by_memory_id(self, memory_id: str) -> GraphNodeData | None:
        with self._db() as db:
            row = (
                db.query(GraphNode)
                .filter_by(memory_id=memory_id, node_type=NodeType.SEMANTIC.value)
                .first()
            )
            return _to_domain(row) if row else None

    def count_user_nodes(self, user_id: str) -> int:
        with self._db() as db:
            return db.query(GraphNode).filter_by(user_id=user_id, is_active=1).count()

    # ── Vector Search ─────────────────────────────────────────────────

    def find_similar_nodes(
        self, user_id: str, embedding: list[float],
        *, top_k: int = 5, node_type: NodeType | None = None,
    ) -> list[GraphNodeData]:
        from matrixone.sqlalchemy_ext import l2_distance

        with self._db() as db:
            dist = l2_distance(GraphNode.embedding, embedding)
            q = (
                db.query(GraphNode)
                .filter_by(user_id=user_id, is_active=1)
                .filter(GraphNode.embedding.isnot(None))
            )
            if node_type is not None:
                q = q.filter_by(node_type=node_type.value)
            return [_to_domain(r) for r in q.order_by(dist).limit(top_k).all()]

    def find_similar_with_scores(
        self, user_id: str, embedding: list[float],
        *, top_k: int = 5, node_type: NodeType | None = None,
    ) -> list[tuple[GraphNodeData, float]]:
        """Top-K nodes with cosine similarity (DB-side)."""
        from matrixone.sqlalchemy_ext import cosine_distance

        with self._db() as db:
            cos_dist = cosine_distance(GraphNode.embedding, embedding)
            cos_sim = (1.0 - cos_dist).label("cos_sim")
            q = (
                db.query(GraphNode, cos_sim)
                .filter_by(user_id=user_id, is_active=1)
                .filter(GraphNode.embedding.isnot(None))
            )
            if node_type is not None:
                q = q.filter_by(node_type=node_type.value)
            return [
                (_to_domain(row), float(sim))
                for row, sim in q.order_by(cos_dist).limit(top_k).all()
            ]

    def get_pair_similarity(self, node_a_id: str, node_b_id: str) -> float | None:
        """Cosine similarity between two nodes (single DB query, self-join)."""
        from matrixone.sqlalchemy_ext import cosine_distance
        from sqlalchemy.orm import aliased

        with self._db() as db:
            A = aliased(GraphNode, name="a")
            B = aliased(GraphNode, name="b")
            result = (
                db.query((1.0 - cosine_distance(A.embedding, B.embedding)).label("sim"))
                .filter(A.node_id == node_a_id, B.node_id == node_b_id)
                .filter(A.embedding.isnot(None), B.embedding.isnot(None))
                .first()
            )
            return float(result.sim) if result else None

    # ── Edge Operations (normalized table) ────────────────────────────

    def add_edges_batch(
        self, edges: list[tuple[str, str, str, float]], user_id: str,
    ) -> None:
        """Insert edges, ignoring duplicates (composite PK)."""
        if not edges:
            return
        with self._db() as db:
            for src_id, tgt_id, etype, weight in edges:
                existing = (
                    db.query(GraphEdge)
                    .filter_by(source_id=src_id, target_id=tgt_id, edge_type=etype)
                    .first()
                )
                if existing:
                    if existing.weight != weight:
                        existing.weight = weight
                else:
                    db.add(GraphEdge(
                        source_id=src_id, target_id=tgt_id,
                        edge_type=etype, weight=weight, user_id=user_id,
                    ))
            db.commit()

    def get_outgoing_edges(self, node_id: str) -> list[Edge]:
        """All outgoing edges from a node."""
        with self._db() as db:
            rows = db.query(GraphEdge).filter_by(source_id=node_id).all()
            return [Edge(r.target_id, r.edge_type, r.weight) for r in rows]

    def get_incoming_edges(self, node_id: str) -> list[Edge]:
        """All incoming edges to a node."""
        with self._db() as db:
            rows = db.query(GraphEdge).filter_by(target_id=node_id).all()
            return [Edge(r.source_id, r.edge_type, r.weight) for r in rows]

    def get_edges_for_nodes(self, node_ids: set[str]) -> dict[str, list[Edge]]:
        """Batch: all outgoing edges for a set of nodes. Single query."""
        if not node_ids:
            return {}
        with self._db() as db:
            rows = (
                db.query(GraphEdge)
                .filter(GraphEdge.source_id.in_(list(node_ids)))
                .all()
            )
            result: dict[str, list[Edge]] = {nid: [] for nid in node_ids}
            for r in rows:
                result[r.source_id].append(Edge(r.target_id, r.edge_type, r.weight))
            return result

    def get_incoming_for_nodes(self, node_ids: set[str]) -> dict[str, list[Edge]]:
        """Batch: all incoming edges for a set of nodes. Single query."""
        if not node_ids:
            return {}
        with self._db() as db:
            rows = (
                db.query(GraphEdge)
                .filter(GraphEdge.target_id.in_(list(node_ids)))
                .all()
            )
            result: dict[str, list[Edge]] = {nid: [] for nid in node_ids}
            for r in rows:
                result[r.target_id].append(Edge(r.source_id, r.edge_type, r.weight))
            return result

    def get_neighbor_ids(self, node_ids: set[str]) -> set[str]:
        """All 1-hop neighbor IDs (both directions). Single query."""
        if not node_ids:
            return set()
        with self._db() as db:
            out_rows = (
                db.query(GraphEdge.target_id)
                .filter(GraphEdge.source_id.in_(list(node_ids)))
                .all()
            )
            in_rows = (
                db.query(GraphEdge.source_id)
                .filter(GraphEdge.target_id.in_(list(node_ids)))
                .all()
            )
            return {r[0] for r in out_rows} | {r[0] for r in in_rows}

    def get_user_edge_count(self, user_id: str) -> int:
        with self._db() as db:
            return db.query(GraphEdge).filter_by(user_id=user_id).count()

    def get_association_edges(self, user_id: str, min_weight: float = 0.0) -> list[tuple[str, str, float]]:
        """All association edges for a user. For consolidation conflict scan."""
        with self._db() as db:
            rows = (
                db.query(GraphEdge.source_id, GraphEdge.target_id, GraphEdge.weight)
                .filter_by(user_id=user_id, edge_type="association")
                .filter(GraphEdge.weight >= min_weight)
                .all()
            )
            return [(r.source_id, r.target_id, r.weight) for r in rows]

    # ── Node Update ───────────────────────────────────────────────────

    def deactivate_node(self, node_id: str, *, superseded_by: str | None = None) -> None:
        with self._db() as db:
            updates: dict = {"is_active": 0}
            if superseded_by:
                updates["superseded_by"] = superseded_by
            db.query(GraphNode).filter_by(node_id=node_id).update(updates)
            db.commit()

    def update_importance(self, node_id: str, importance: float) -> None:
        with self._db() as db:
            db.query(GraphNode).filter_by(node_id=node_id).update({"importance": importance})
            db.commit()

    def update_confidence(self, node_id: str, confidence: float) -> None:
        with self._db() as db:
            db.query(GraphNode).filter_by(node_id=node_id).update({"confidence": confidence})
            db.commit()

    def mark_conflict(
        self, older_id: str, newer_id: str,
        *, confidence_factor: float = 0.5, old_confidence: float = 0.75,
    ) -> None:
        """Atomic conflict marking — single transaction."""
        with self._db() as db:
            db.query(GraphNode).filter_by(node_id=older_id).update({
                "confidence": old_confidence * confidence_factor,
                "conflicts_with": newer_id,
                "conflict_resolution": "superseded",
            })
            db.query(GraphNode).filter_by(node_id=newer_id).update({
                "conflict_resolution": "kept",
            })
            db.commit()

    # ── Session-level queries ─────────────────────────────────────────

    def get_latest_episodic_in_session(self, user_id: str, session_id: str) -> GraphNodeData | None:
        with self._db() as db:
            row = (
                db.query(GraphNode)
                .filter_by(
                    user_id=user_id, session_id=session_id,
                    node_type=NodeType.EPISODIC.value, is_active=1,
                )
                .order_by(GraphNode.created_at.desc())
                .first()
            )
            return _to_domain(row) if row else None
