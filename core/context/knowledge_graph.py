"""Knowledge Graph — entity-relationship layer over sk_knowledge_entries.

Ref: memory-and-context.md §9 — "Knowledge Graphs for Semantic Memory"

Adds relationship edges between knowledge entries, enabling:
  - 1-hop graph expansion during retrieval (find related knowledge)
  - Contradiction detection via "contradicts" edges
  - Dependency tracking via "depends_on" edges
"""

from __future__ import annotations

from sqlalchemy import text
from sqlalchemy.orm import Session
from uuid_utils import uuid7

from core.logging_config import get_logger

logger = get_logger(__name__)


def add_relation(
    db: Session,
    subject_id: str,
    predicate: str,
    object_id: str,
    *,
    weight: float = 1.0,
    source: str = "extraction",
) -> str | None:
    """Add a directed edge between two knowledge entries.

    Uses INSERT ON DUPLICATE KEY UPDATE so repeated calls are idempotent.
    Returns relation_id on success, None on failure.
    """
    rid = str(uuid7())
    try:
        db.execute(
            text("""
                INSERT INTO sk_knowledge_relations
                (relation_id, subject_id, predicate, object_id, weight, source, created_at)
                VALUES (:rid, :sid, :pred, :oid, :w, :src, NOW())
                ON DUPLICATE KEY UPDATE weight = VALUES(weight), source = VALUES(source)
            """),
            {"rid": rid, "sid": subject_id, "pred": predicate, "oid": object_id, "w": weight, "src": source},
        )
        db.commit()
        return rid
    except Exception as e:
        logger.warning("Failed to add relation %s -[%s]-> %s: %s", subject_id, predicate, object_id, e)
        db.rollback()
        return None


def get_neighbors(
    db: Session,
    entry_id: str,
    *,
    predicates: list[str] | None = None,
    direction: str = "both",
    limit: int = 20,
) -> list[dict]:
    """Get 1-hop neighbors of a knowledge entry.

    Args:
        entry_id: Source knowledge entry ID.
        predicates: Filter by relationship types (None = all).
        direction: "outgoing", "incoming", or "both".
        limit: Max neighbors to return.

    Returns:
        List of dicts with neighbor_id, predicate, weight, direction.
    """
    clauses = []
    params: dict = {"eid": entry_id, "limit": limit}

    # Build predicate filter with dynamic placeholders
    pred_filter = ""
    if predicates:
        pred_ph = ", ".join(f":p{i}" for i in range(len(predicates)))
        pred_filter = f"AND predicate IN ({pred_ph})"
        for i, p in enumerate(predicates):
            params[f"p{i}"] = p

    if direction in ("outgoing", "both"):
        clauses.append(f"""
            SELECT object_id AS neighbor_id, predicate, weight, 'outgoing' AS dir
            FROM sk_knowledge_relations WHERE subject_id = :eid {pred_filter}
        """)
    if direction in ("incoming", "both"):
        clauses.append(f"""
            SELECT subject_id AS neighbor_id, predicate, weight, 'incoming' AS dir
            FROM sk_knowledge_relations WHERE object_id = :eid {pred_filter}
        """)

    sql = " UNION ALL ".join(clauses) + " ORDER BY weight DESC LIMIT :limit"

    try:
        rows = db.execute(text(sql), params).fetchall()
        return [
            {"neighbor_id": r.neighbor_id, "predicate": r.predicate, "weight": float(r.weight), "direction": r.dir}
            for r in rows
        ]
    except Exception as e:
        logger.warning("get_neighbors failed for %s: %s", entry_id, e)
        return []


def expand_with_graph(
    db: Session,
    entry_ids: list[str],
    *,
    limit_per_entry: int = 3,
) -> list[str]:
    """1-hop graph expansion: given seed entry IDs, find related entries.

    Returns additional entry IDs (not including seeds) sorted by total weight.
    """
    if not entry_ids:
        return []

    placeholders = ", ".join(f":e{i}" for i in range(len(entry_ids)))
    params = {f"e{i}": eid for i, eid in enumerate(entry_ids)}
    params["lim"] = limit_per_entry * len(entry_ids)

    sql = text(f"""
        SELECT neighbor_id, SUM(weight) AS total_weight FROM (
            SELECT object_id AS neighbor_id, weight
            FROM sk_knowledge_relations WHERE subject_id IN ({placeholders})
            UNION ALL
            SELECT subject_id AS neighbor_id, weight
            FROM sk_knowledge_relations WHERE object_id IN ({placeholders})
        ) t
        WHERE neighbor_id NOT IN ({placeholders})
        GROUP BY neighbor_id
        ORDER BY total_weight DESC
        LIMIT :lim
    """)

    try:
        rows = db.execute(sql, params).fetchall()
        return [r.neighbor_id for r in rows]
    except Exception as e:
        logger.warning("expand_with_graph failed: %s", e)
        return []
