"""DB-backed semantic skill index using MatrixOne L2_DISTANCE vector search.

Stores embeddings in the ``skills_registry.embedding`` column and queries
via ``ORDER BY l2_distance(embedding, %s) LIMIT k``.  Falls back to empty
results when no embeddings exist or embed_fn is None.
"""

from __future__ import annotations

from typing import TYPE_CHECKING, Protocol, runtime_checkable

from sqlalchemy import text

from core.db_consumer import DbConsumer
from core.logging_config import get_logger

if TYPE_CHECKING:
    from collections.abc import Callable

logger = get_logger(__name__)


@runtime_checkable
class Embeddable(Protocol):
    """Minimum interface a skill object must satisfy for embedding."""

    name: str
    description: str | None
    triggers: list[str] | None


def _skill_text(skill: Embeddable) -> str:
    """Build the text blob that represents a skill for embedding."""
    parts = [skill.name]
    if skill.description:
        parts.append(skill.description)
    if skill.triggers:
        parts.append(" ".join(skill.triggers))
    return " | ".join(parts)


class SkillIndex(DbConsumer):
    """DB-backed vector search over skill embeddings.

    ``embed_fn`` generates embeddings; the DB stores and searches them.
    ``db_factory`` provides short-lived sessions (via DbConsumer._db()).
    """

    # L2 distance threshold — empirically tuned for 384-dim all-MiniLM-L6-v2.
    # Lower = more similar.  L2 ≈ sqrt(2 * (1 - cosine)) for unit vectors.
    MAX_L2_DISTANCE = 1.15

    def __init__(
        self,
        embed_fn: Callable[[str], list[float]] | None = None,
        db_factory=None,
    ):
        if db_factory is not None:
            super().__init__(db_factory)
        else:
            # Inert mode — no DB available.  We set _db_factory directly
            # rather than calling super().__init__() because DbConsumer
            # requires a callable and would reject None.
            self._db_factory = None
        self._embed = embed_fn
        # Cache expected dimension from config to reject mismatched vectors
        # before they reach the DB (prevents "vector ops between different
        # dimensions" errors from stale or misconfigured embeddings).
        from api.models._constants import EMBEDDING_DIM
        self._expected_dim: int = EMBEDDING_DIM

    # ------------------------------------------------------------------
    # Build / update
    # ------------------------------------------------------------------

    def build(self, skills: list[Embeddable], *, force: bool = False) -> int:
        """Compute and store embeddings for skills.

        By default only embeds skills that lack an embedding (``force=False``).
        Pass ``force=True`` to re-embed all skills.

        Returns number of skills newly embedded.

        .. note:: Embeds sequentially.  For large registries consider batching
           via the embedding provider's batch API.
        """
        if not self._embed or not self._db_factory:
            return 0
        with self._db() as db:
            count = 0
            for skill in skills:
                if self._upsert_embedding(db, skill, force=force):
                    count += 1
            db.commit()
            logger.info("SkillIndex build: %d new embeddings stored", count)
            return count

    def add(self, skill: Embeddable) -> bool:
        """Compute and store embedding for a single skill."""
        if not self._embed or not self._db_factory:
            return False
        with self._db() as db:
            ok = self._upsert_embedding(db, skill)
            db.commit()
            return ok

    def remove(self, name: str) -> bool:
        """Clear embedding for a skill by name."""
        if not self._db_factory:
            return False
        with self._db() as db:
            r = db.execute(
                text(
                    "UPDATE skills_registry SET embedding = NULL"
                    " WHERE skill_name = :name AND is_active = 1"
                ),
                {"name": name},
            )
            db.commit()
            return r.rowcount > 0  # type: ignore[union-attr]

    def _upsert_embedding(self, db, skill: Embeddable, *, force: bool = False) -> bool:
        """Embed one skill and UPDATE its row.  Returns True if a row was written."""
        try:
            vec = self._embed(_skill_text(skill))
            if self._expected_dim and len(vec) != self._expected_dim:
                logger.warning(
                    "Dimension mismatch for %s: got %d, expected %d",
                    skill.name, len(vec), self._expected_dim,
                )
                return False
            vec_literal = "[" + ",".join(str(v) for v in vec) + "]"
            sql = (
                "UPDATE skills_registry SET embedding = :vec"
                " WHERE skill_name = :name AND is_active = 1"
            )
            if not force:
                sql += " AND embedding IS NULL"
            result = db.execute(text(sql), {"vec": vec_literal, "name": skill.name})
            return result.rowcount > 0  # type: ignore[union-attr]
        except Exception as e:
            logger.warning("Failed to embed skill %s: %s", skill.name, e)
            return False

    # ------------------------------------------------------------------
    # Query
    # ------------------------------------------------------------------

    def query(
        self, text_query: str, top_k: int = 10, max_distance: float | None = None,
    ) -> list[str]:
        """Return top-k skill names by L2 distance to *text_query*.

        Returns empty list if no embeddings exist or embed_fn/db_factory is None.
        Skills with L2 distance > *max_distance* are excluded.
        """
        if not self._embed or not self._db_factory:
            return []
        try:
            q_vec = self._embed(text_query)
        except Exception as e:
            logger.warning("Query embedding failed: %s", e)
            return []

        if len(q_vec) != self._expected_dim:
            logger.warning(
                "Query vector dimension %d != expected %d, skipping",
                len(q_vec), self._expected_dim,
            )
            return []

        threshold = max_distance if max_distance is not None else self.MAX_L2_DISTANCE
        vec_literal = "[" + ",".join(str(v) for v in q_vec) + "]"

        with self._db() as db:
            try:
                rows = db.execute(
                    text(
                        "SELECT skill_name, l2_distance(embedding, :vec) AS dist"
                        " FROM skills_registry"
                        " WHERE is_active = 1 AND embedding IS NOT NULL"
                        " ORDER BY dist ASC LIMIT :k"
                    ),
                    {"vec": vec_literal, "k": top_k},
                ).fetchall()
            except Exception as e:
                # Dimension mismatch from stale embeddings — clear them and
                # return empty so the caller falls back to keyword matching.
                logger.warning("Vector query failed (stale embeddings?): %s", e)
                db.rollback()
                self._clear_stale_embeddings(db)
                return []
            return [r[0] for r in rows if r[1] <= threshold]

    def _clear_stale_embeddings(self, db) -> int:
        """NULL out embeddings that don't match the expected dimension.

        MatrixOne doesn't expose a vector_dims() function, so we attempt a
        dummy l2_distance against a correctly-sized zero vector.  Rows that
        cause a dimension error are the stale ones — but since we can't
        identify them row-by-row efficiently, we clear ALL embeddings and
        let the next build() re-embed with the correct dimension.
        """
        r = db.execute(
            text("UPDATE skills_registry SET embedding = NULL WHERE embedding IS NOT NULL"),
        )
        db.commit()
        cleared = r.rowcount  # type: ignore[union-attr]
        if cleared:
            logger.warning("Cleared %d stale embeddings (dimension mismatch)", cleared)
        return cleared
