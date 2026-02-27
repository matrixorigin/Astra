"""MemoryHealth — pollution detection, stats, cleanup."""

from __future__ import annotations

import logging
from datetime import datetime, timedelta
from typing import Optional

from sqlalchemy import text

from core.db_consumer import DbConsumer, DbFactory

logger = logging.getLogger(__name__)


class MemoryHealth(DbConsumer):
    """Memory health analytics and pollution detection."""

    def __init__(
        self,
        db_factory: DbFactory,
        db_name: str = "dev_agent",
        pollution_threshold: float = 0.3,
    ):
        super().__init__(db_factory)
        self.db_name = db_name
        self.pollution_threshold = pollution_threshold

    def analyze(self, user_id: str) -> dict:
        """Get per-type stats: count, avg_confidence, contradiction_rate, staleness."""
        with self._db() as db:
            rows = db.execute(text("""
                SELECT
                    memory_type,
                    COUNT(*) as total,
                    AVG(initial_confidence) as avg_confidence,
                    COUNT(CASE WHEN superseded_by IS NOT NULL THEN 1 END) as superseded,
                    AVG(TIMESTAMPDIFF(HOUR, observed_at, NOW())) as avg_staleness_hours
                FROM mem_memories
                WHERE user_id = :uid
                GROUP BY memory_type
            """), {"uid": user_id}).fetchall()

        stats = {}
        for r in rows:
            contradiction_rate = r.superseded / r.total if r.total > 0 else 0
            stats[r.memory_type] = {
                "total": r.total,
                "avg_confidence": float(r.avg_confidence or 0),
                "contradiction_rate": contradiction_rate,
                "avg_staleness_hours": float(r.avg_staleness_hours or 0),
            }
        return stats

    def detect_pollution(self, user_id: str, since_timestamp: datetime) -> dict:
        """Detect pollution by checking supersede/delete ratio since timestamp."""
        ts_str = since_timestamp.strftime("%Y-%m-%d %H:%M:%S")
        try:
            with self._db() as db:
                # Count changes since timestamp
                result = db.execute(text("""
                    SELECT
                        COUNT(*) as total_changes,
                        COUNT(CASE WHEN superseded_by IS NOT NULL THEN 1 END) as supersedes
                    FROM mem_memories
                    WHERE user_id = :uid AND updated_at >= :ts
                """), {"uid": user_id, "ts": since_timestamp}).fetchone()

            total = result.total_changes or 0
            supersedes = result.supersedes or 0
            ratio = supersedes / total if total > 0 else 0
            is_polluted = ratio > self.pollution_threshold

            return {
                "is_polluted": is_polluted,
                "total_changes": total,
                "supersedes": supersedes,
                "ratio": ratio,
                "threshold": self.pollution_threshold,
            }
        except Exception as e:
            logger.warning("Pollution detection failed: %s", e)
            return {"is_polluted": False, "error": str(e)}

    def suggest_rollback_target(self, user_id: str) -> Optional[str]:
        """Find the most likely bad memory (low confidence, recent, caused supersedes)."""
        with self._db() as db:
            row = db.execute(text("""
                SELECT memory_id
                FROM mem_memories
                WHERE user_id = :uid
                  AND is_active = 1
                  AND initial_confidence < 0.5
                ORDER BY observed_at DESC
                LIMIT 1
            """), {"uid": user_id}).fetchone()
        return row.memory_id if row else None

    def cleanup_snapshots(self, keep_last_n: int = 5) -> int:
        """Drop old milestone snapshots, keep last N."""
        with self._db() as db:
            rows = db.execute(text("""
                SELECT sname FROM mo_catalog.mo_snapshots
                WHERE sname LIKE 'mem_milestone_%'
                ORDER BY create_time DESC
            """)).fetchall()

        if len(rows) <= keep_last_n:
            return 0

        to_drop = [r.sname for r in rows[keep_last_n:]]
        dropped = 0

        # Use autocommit for DDL
        with self._db() as db:
            raw_conn = db.connection().connection
            raw_conn.autocommit(True)
            cursor = raw_conn.cursor()
            try:
                for name in to_drop:
                    try:
                        cursor.execute(f"drop snapshot {name}")
                        dropped += 1
                    except Exception as e:
                        logger.warning("Failed to drop snapshot %s: %s", name, e)
            finally:
                cursor.close()
                raw_conn.autocommit(False)

        logger.info("Cleaned up %d old snapshots", dropped)
        return dropped

    def cleanup_orphan_branches(self) -> int:
        """Clean up sandbox branches that were not properly dropped."""
        with self._db() as db:
            rows = db.execute(text("""
                SELECT table_name FROM information_schema.tables
                WHERE table_name LIKE 'memories_sandbox_%'
            """)).fetchall()

        if not rows:
            return 0

        cleaned = 0
        with self._db() as db:
            for r in rows:
                try:
                    db.execute(text(
                        f"data branch delete table {self.db_name}.{r.table_name}"
                    ))
                    db.commit()
                    cleaned += 1
                    logger.info("Cleaned orphan branch: %s", r.table_name)
                except Exception as e:
                    logger.warning("Failed to clean branch %s: %s", r.table_name, e)

        return cleaned

    def get_storage_stats(self, user_id: str) -> dict:
        """Get storage statistics for monitoring."""
        with self._db() as db:
            row = db.execute(text("""
                SELECT
                    COUNT(*) as total,
                    SUM(CASE WHEN is_active = 1 THEN 1 ELSE 0 END) as active,
                    AVG(LENGTH(content)) as avg_content_size,
                    MIN(observed_at) as oldest,
                    MAX(observed_at) as newest
                FROM mem_memories
                WHERE user_id = :uid
            """), {"uid": user_id}).fetchone()

        return {
            "total": row.total or 0,
            "active": row.active or 0,
            "inactive": (row.total or 0) - (row.active or 0),
            "avg_content_size": float(row.avg_content_size or 0),
            "oldest": row.oldest,
            "newest": row.newest,
        }
