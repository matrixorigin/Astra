"""GraphConsolidator — periodic graph maintenance.

1. Detect cross-session contradictions (via edge table scan)
2. Check scene node source integrity

See docs/design/memory/graph-memory.md §4.2, §5.4, §5.5
"""

from __future__ import annotations

import logging
from dataclasses import dataclass, field
from typing import TYPE_CHECKING

from core.memory.graph.graph_store import GraphStore
from core.memory.graph.types import NodeType

if TYPE_CHECKING:
    from core.db_consumer import DbFactory

logger = logging.getLogger(__name__)

CONTRADICTION_ASSOCIATION_THRESHOLD = 0.7
SOURCE_INTEGRITY_RATIO = 0.5


@dataclass
class ConsolidationResult:
    merged_nodes: int = 0
    conflicts_detected: int = 0
    orphaned_scenes: int = 0
    errors: list[str] = field(default_factory=list)


class GraphConsolidator:
    def __init__(self, db_factory: DbFactory) -> None:
        self._store = GraphStore(db_factory)

    def consolidate(self, user_id: str) -> ConsolidationResult:
        result = ConsolidationResult()
        try:
            result.conflicts_detected = self._detect_conflicts(user_id)
        except Exception as e:
            logger.warning("Conflict detection failed: %s", e)
            result.errors.append(f"conflicts: {e}")
        try:
            result.orphaned_scenes = self._check_source_integrity(user_id)
        except Exception as e:
            logger.warning("Source integrity check failed: %s", e)
            result.errors.append(f"integrity: {e}")
        return result

    def _detect_conflicts(self, user_id: str) -> int:
        """Detect contradictions: high association + low content similarity + cross-session.

        Scans edge table directly — no node loading for graph traversal.
        Only loads the specific node pairs that are conflict candidates.
        """
        # 1. Get all strong association edges (DB-side filter)
        strong_assoc = self._store.get_association_edges(
            user_id, min_weight=CONTRADICTION_ASSOCIATION_THRESHOLD,
        )
        if not strong_assoc:
            return 0

        # 2. Collect candidate node IDs
        candidate_ids: set[str] = set()
        for src, tgt, _ in strong_assoc:
            candidate_ids.add(src)
            candidate_ids.add(tgt)

        # 3. Load only candidate nodes (not full graph)
        nodes = self._store.get_nodes_by_ids(list(candidate_ids))
        node_map = {n.node_id: n for n in nodes}

        conflicts_found = 0
        for src_id, tgt_id, _weight in strong_assoc:
            node = node_map.get(src_id)
            neighbor = node_map.get(tgt_id)
            if not node or not neighbor:
                continue
            if not node.is_active or not neighbor.is_active:
                continue
            if node.node_type != NodeType.SEMANTIC or neighbor.node_type != NodeType.SEMANTIC:
                continue
            if node.conflicts_with or neighbor.conflicts_with:
                continue
            if node.session_id == neighbor.session_id:
                continue

            # DB-side content similarity check
            content_sim = self._store.get_pair_similarity(node.node_id, neighbor.node_id)
            if content_sim is None:
                content_sim = 0.5
            if content_sim > 0.4:
                continue

            # Confirmed contradiction
            if node.node_id < neighbor.node_id:
                older, newer = node, neighbor
            else:
                older, newer = neighbor, node

            self._store.mark_conflict(
                older_id=older.node_id, newer_id=newer.node_id,
                confidence_factor=0.5, old_confidence=older.confidence,
            )
            conflicts_found += 1

        return conflicts_found

    def _check_source_integrity(self, user_id: str) -> int:
        """Check scene nodes for orphaned sources."""
        scene_nodes = self._store.get_user_nodes(
            user_id, node_type=NodeType.SCENE, active_only=True,
            load_embedding=False,
        )
        orphaned = 0
        for scene in scene_nodes:
            if not scene.source_nodes:
                continue
            source_nodes = self._store.get_nodes_by_ids(scene.source_nodes)
            active_sources = [n for n in source_nodes if n.is_active]
            if len(active_sources) == 0:
                self._store.deactivate_node(scene.node_id)
                orphaned += 1
            elif len(active_sources) < len(scene.source_nodes) * SOURCE_INTEGRITY_RATIO:
                self._store.update_confidence(scene.node_id, scene.confidence * 0.8)
        return orphaned
