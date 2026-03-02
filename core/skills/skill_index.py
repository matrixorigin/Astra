"""DB-backed semantic skill index using MatrixOne L2_DISTANCE vector search.

Stores embeddings in the ``skills_registry.embedding`` column and queries
via ``ORDER BY l2_distance(embedding, %s) LIMIT k``.  Falls back to empty
results when no embeddings exist or embed_fn is None.
"""

from __future__ import annotations

from typing import TYPE_CHECKING, Protocol, runtime_checkable

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
        from api.models.skill import SkillRegistry
        with self._db() as db:
            updated = (
                db.query(SkillRegistry)
                .filter(SkillRegistry.skill_name == name, SkillRegistry.is_active == 1)
                .update({SkillRegistry.embedding: None})
            )
            db.commit()
            return updated > 0

    def _upsert_embedding(self, db, skill: Embeddable, *, force: bool = False) -> bool:
        """Embed one skill and UPDATE its row.  Returns True if a row was written."""
        from api.models.skill import SkillRegistry
        try:
            vec = self._embed(_skill_text(skill))
            if len(vec) != self._expected_dim:
                logger.warning(
                    "Dimension mismatch for %s: got %d, expected %d",
                    skill.name, len(vec), self._expected_dim,
                )
                return False
            vec_literal = "[" + ",".join(str(v) for v in vec) + "]"
            query = db.query(SkillRegistry).filter(
                SkillRegistry.skill_name == skill.name,
                SkillRegistry.is_active == 1,
            )
            if not force:
                query = query.filter(SkillRegistry.embedding.is_(None))
            updated = query.update({SkillRegistry.embedding: vec_literal})
            return updated > 0
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

        from matrixone.sqlalchemy_ext import l2_distance

        from api.models.skill import SkillRegistry
        dist_expr = l2_distance(SkillRegistry.embedding, q_vec).label("dist")

        with self._db() as db:
            try:
                rows = (
                    db.query(SkillRegistry.skill_name, dist_expr)
                    .filter(
                        SkillRegistry.is_active == 1,
                        SkillRegistry.embedding.isnot(None),
                    )
                    .order_by("dist")
                    .limit(top_k)
                    .all()
                )
            except Exception as e:
                # Dimension mismatch from stale embeddings — clear them and
                # return empty so the caller falls back to keyword matching.
                logger.warning("Vector query failed (stale embeddings?): %s", e)
                db.rollback()
                self._clear_stale_embeddings(db)
                return []
            return [name for name, dist in rows if dist <= threshold]

    def _clear_stale_embeddings(self, db) -> int:
        """NULL out all embeddings so next build() re-embeds with correct dimension.

        Called when l2_distance fails due to dimension mismatch from stale data.
        """
        from api.models.skill import SkillRegistry
        cleared = (
            db.query(SkillRegistry)
            .filter(SkillRegistry.embedding.isnot(None))
            .update({SkillRegistry.embedding: None})
        )
        db.commit()
        if cleared:
            logger.warning("Cleared %d stale embeddings (dimension mismatch)", cleared)
        return cleared
