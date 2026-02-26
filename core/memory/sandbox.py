"""MemorySandbox — write-ahead validation using MO zero-copy branch.

Validates new memories in an isolated branch before committing to main table.
"""

from __future__ import annotations

import logging
import uuid
from typing import Optional

from sqlalchemy import text

from core.db_consumer import DbConsumer, DbFactory
from core.memory.types import Memory

logger = logging.getLogger(__name__)


class MemorySandbox(DbConsumer):
    """Validate memories in a branch before committing."""

    def __init__(self, db_factory: DbFactory, db_name: str = "mo_agent"):
        super().__init__(db_factory)
        self.db_name = db_name

    def validate_memories(
        self,
        user_id: str,
        new_memories: list[Memory],
        query_text: str,
        query_embedding: Optional[list[float]] = None,
    ) -> bool:
        """Validate that new memories improve retrieval quality.

        Flow:
        1. Create branch table (zero-copy)
        2. Insert new memories into branch
        3. Compare retrieval quality: branch vs main
        4. Drop branch (always, regardless of result)

        Returns True if new memories improve quality, False otherwise.
        """
        if not new_memories:
            return True

        branch_name = f"memories_sandbox_{uuid.uuid4().hex[:8]}"

        try:
            self._create_branch(branch_name)
            self._insert_to_branch(branch_name, new_memories)

            score_main = self._retrieval_score(
                "memories", user_id, query_text, query_embedding
            )
            score_branch = self._retrieval_score(
                branch_name, user_id, query_text, query_embedding
            )

            improved = score_branch >= score_main
            logger.debug(
                "Sandbox validation: main=%.3f branch=%.3f improved=%s",
                score_main, score_branch, improved,
            )
            return improved

        except Exception as e:
            logger.warning("Sandbox validation failed: %s", e)
            return True  # Fail open: allow write if validation errors

        finally:
            self._drop_branch(branch_name)

    def _create_branch(self, branch_name: str) -> None:
        with self._db() as db:
            db.execute(text(
                f"data branch create table {branch_name} from memories"
            ))
            db.commit()

    def _insert_to_branch(self, branch_name: str, memories: list[Memory]) -> None:
        with self._db() as db:
            for m in memories:
                vec_str = (
                    "[" + ",".join(str(v) for v in m.embedding) + "]"
                    if m.embedding else "NULL"
                )
                source_ids = str(m.source_event_ids).replace("'", '"')
                db.execute(text(f"""
                    INSERT INTO {branch_name}
                    (memory_id, user_id, memory_type, content, confidence,
                     embedding, source_event_ids, is_active, observed_at)
                    VALUES (:mid, :uid, :mtype, :content, :conf,
                            {vec_str}, :sources, 1, :obs_at)
                """), {
                    "mid": m.memory_id,
                    "uid": m.user_id,
                    "mtype": m.memory_type.value,
                    "content": m.content,
                    "conf": m.confidence,
                    "sources": source_ids,
                    "obs_at": m.observed_at,
                })
            db.commit()

    def _retrieval_score(
        self,
        table_name: str,
        user_id: str,
        query_text: str,
        query_embedding: Optional[list[float]],
    ) -> float:
        """Compute aggregate retrieval score for top-5 results."""
        with self._db() as db:
            if query_embedding:
                vec_str = "[" + ",".join(str(v) for v in query_embedding) + "]"
                rows = db.execute(text(f"""
                    SELECT (1.0 / (1.0 + L2_DISTANCE(embedding, '{vec_str}'))) AS sim
                    FROM {table_name}
                    WHERE user_id = :uid AND is_active = 1
                    ORDER BY sim DESC LIMIT 5
                """), {"uid": user_id}).fetchall()
            else:
                rows = db.execute(text(f"""
                    SELECT confidence AS sim
                    FROM {table_name}
                    WHERE user_id = :uid AND is_active = 1
                    ORDER BY confidence DESC LIMIT 5
                """), {"uid": user_id}).fetchall()

            if not rows:
                return 0.0
            return sum(r.sim for r in rows) / len(rows)

    def _drop_branch(self, branch_name: str) -> None:
        try:
            with self._db() as db:
                db.execute(text(
                    f"data branch delete table {self.db_name}.{branch_name}"
                ))
                db.commit()
        except Exception as e:
            logger.warning("Failed to drop branch %s: %s", branch_name, e)
