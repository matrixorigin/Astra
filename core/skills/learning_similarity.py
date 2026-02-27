"""Similarity and matching utilities for self-improving selector."""

import json
import math
import re
from typing import Any

from sqlalchemy import text

from core.logging_config import get_logger

logger = get_logger(__name__)


def pattern_matches(pattern: str, query: str) -> bool:
    """Word-boundary aware pattern matching to avoid false positives."""
    return bool(re.search(r'\b' + re.escape(pattern) + r'\b', query))


def l2_similarity(left: list[float], right: list[float]) -> float:
    if not left or not right or len(left) != len(right):
        return 0.0
    distance = math.sqrt(sum((float(a) - float(b)) ** 2 for a, b in zip(left, right)))
    return 1.0 / (1.0 + distance)


def cosine_similarity(left: list[float], right: list[float]) -> float:
    if not left or not right or len(left) != len(right):
        return 0.0
    dot = sum(float(a) * float(b) for a, b in zip(left, right))
    left_norm = math.sqrt(sum(float(v) ** 2 for v in left))
    right_norm = math.sqrt(sum(float(v) ** 2 for v in right))
    denom = left_norm * right_norm
    return dot / denom if denom > 0 else 0.0


def parse_embedding(value: Any) -> list[float] | None:
    if value is None:
        return None
    if isinstance(value, list):
        return value
    if isinstance(value, str):
        try:
            parsed = json.loads(value)
        except json.JSONDecodeError:
            return None
        if isinstance(parsed, list):
            return parsed
    return None


def embedding_to_vec_str(embedding: list[float] | None) -> str | None:
    if not embedding:
        return None
    return json.dumps([float(v) for v in embedding], separators=(",", ":"))


def extract_context_features(query: str) -> dict[str, Any]:
    length = len(query)
    if length <= 50:
        length_bucket = "short"
    elif length <= 200:
        length_bucket = "medium"
    else:
        length_bucket = "long"
    contains_code = "```" in query or "def " in query or "class " in query or ";" in query
    return {"length_bucket": length_bucket, "contains_code": contains_code}


def context_matches(learning_features: dict[str, Any] | None, query_features: dict[str, Any]) -> bool:
    if not learning_features:
        return True
    return all(query_features.get(k) == v for k, v in learning_features.items())


def normalize_confidence(value: float | None) -> float:
    if value is None:
        return 0.0
    if value <= 1.0:
        return max(0.0, float(value))
    return min(1.0, float(value) / 100.0)


def semantic_similarity_map(
    session, query_embedding: list[float] | None, threshold: float, limit: int,
) -> dict[str, float] | None:
    """Query DB for semantically similar learnings using L2_DISTANCE."""
    if query_embedding is None:
        return None
    if not hasattr(session, "bind") or session.bind is None:
        return None
    vec_str = "[" + ",".join(str(x) for x in query_embedding) + "]"
    try:
        rows = session.execute(
            text("""
                SELECT learning_id, similarity FROM (
                    SELECT learning_id,
                           1.0 / (1.0 + L2_DISTANCE(query_embedding, :vec)) AS similarity
                    FROM skill_selection_learnings
                    WHERE query_embedding IS NOT NULL
                ) ranked
                WHERE similarity >= :threshold
                ORDER BY similarity DESC
                LIMIT :limit
            """),
            {"vec": vec_str, "limit": limit, "threshold": threshold},
        ).fetchall()
    except Exception as exc:
        logger.warning(f"Semantic similarity SQL failed: {exc}")
        return None
    result = {
        str(row.learning_id): float(row.similarity)
        for row in rows if row.similarity is not None
    }
    return result or None
