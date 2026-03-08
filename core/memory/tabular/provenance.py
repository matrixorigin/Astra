"""MemoryProvenance — PITR queries, diff, rollback, impact analysis."""

from __future__ import annotations

import logging
from core.utils.id_generator import generate_prefixed_id
from datetime import datetime, timedelta
from typing import Optional

from sqlalchemy import text

from core.db_consumer import DbConsumer, DbFactory

logger = logging.getLogger(__name__)


class MemoryProvenance(DbConsumer):
    """Historical memory queries and rollback using MO PITR + snapshot."""

    def __init__(self, db_factory: DbFactory, db_name: str = "dev_agent"):
        super().__init__(db_factory)
        self.db_name = db_name

    def _exec_ddl(self, sql: str) -> None:
        """Execute DDL that requires autocommit (PITR/snapshot/restore)."""
        with self._db() as db:
            raw_conn = db.connection().connection
            raw_conn.autocommit(True)
            cursor = raw_conn.cursor()
            try:
                cursor.execute(sql)
            finally:
                cursor.close()
                raw_conn.autocommit(False)

    def setup_pitr(self, range_value: int = 14, range_unit: str = "d") -> None:
        """One-time setup: create PITR for memories table."""
        self._exec_ddl(
            f"create pitr if not exists memory_pitr for table {self.db_name} mem_memories "
            f"range {range_value} '{range_unit}'"
        )

    def memory_state_at(
        self, user_id: str, timestamp: datetime, limit: int = 100,
    ) -> list[dict]:
        """Read memory state at a past timestamp (PITR)."""
        ts_str = timestamp.strftime("%Y-%m-%d %H:%M:%S")
        with self._db() as db:
            rows = db.execute(text(f"""
                SELECT memory_id, content, memory_type, initial_confidence, observed_at
                FROM mem_memories {{timestamp = '{ts_str}'}}
                WHERE user_id = :uid AND is_active = 1
                ORDER BY observed_at DESC LIMIT :lim
            """), {"uid": user_id, "lim": limit}).fetchall()
        return [
            {"memory_id": r.memory_id, "content": r.content,
             "memory_type": r.memory_type, "initial_confidence": r.initial_confidence,
             "observed_at": r.observed_at}
            for r in rows
        ]

    def changes_around(
        self, user_id: str, approx_time: datetime, window_seconds: int = 300,
    ) -> list[dict]:
        """Get memory writes within ±window of approx_time."""
        before = approx_time - timedelta(seconds=window_seconds)
        after = approx_time + timedelta(seconds=window_seconds)
        with self._db() as db:
            rows = db.execute(text("""
                SELECT memory_id, content, memory_type, initial_confidence, observed_at
                FROM mem_memories
                WHERE user_id = :uid
                  AND observed_at BETWEEN :before AND :after
                ORDER BY observed_at
            """), {"uid": user_id, "before": before, "after": after}).fetchall()
        return [
            {"memory_id": r.memory_id, "content": r.content,
             "memory_type": r.memory_type, "observed_at": r.observed_at}
            for r in rows
        ]

    def rollback_before_memory(self, memory_id: str) -> bool:
        """Rollback to just before a specific memory was written."""
        with self._db() as db:
            row = db.execute(text(
                "SELECT observed_at FROM mem_memories WHERE memory_id = :mid"
            ), {"mid": memory_id}).fetchone()
            if not row or not row.observed_at:
                return False
            ts = row.observed_at - timedelta(seconds=1)
        return self.rollback_to_timestamp(ts)

    def rollback_to_timestamp(self, timestamp: datetime) -> bool:
        """Restore memories table to a timestamp via PITR."""
        ts_str = timestamp.strftime("%Y-%m-%d %H:%M:%S")
        try:
            self._exec_ddl(
                f"restore database {self.db_name} table mem_memories "
                f"from pitr memory_pitr '{ts_str}'"
            )
            logger.info("Rolled back memories to %s", ts_str)
            return True
        except Exception as e:
            logger.error("Rollback failed: %s", e)
            return False

    def rollback_to_snapshot(self, snapshot_name: str) -> bool:
        """Restore memories table from a named snapshot."""
        try:
            self._exec_ddl(
                f"restore account sys database {self.db_name} table mem_memories "
                f"from snapshot {snapshot_name}"
            )
            logger.info("Rolled back memories to snapshot %s", snapshot_name)
            return True
        except Exception as e:
            logger.error("Rollback failed: %s", e)
            return False

    def diff_since(self, user_id: str, timestamp: datetime) -> dict:
        """Diff current state against a past timestamp."""
        ts_str = timestamp.strftime("%Y-%m-%d %H:%M:%S")
        try:
            with self._db() as db:
                rows = db.execute(text(f"""
                    data branch diff memories against memories{{timestamp = '{ts_str}'}}
                """)).fetchall()
            return {"changes": len(rows), "rows": [dict(r._mapping) for r in rows]}
        except Exception as e:
            logger.warning("Diff failed: %s", e)
            return {"changes": 0, "rows": [], "error": str(e)}

    def create_milestone(self, name: Optional[str] = None) -> str:
        """Create a named snapshot for long-term anchor."""
        if not name:
            name = generate_prefixed_id("mem_milestone")
        self._exec_ddl(f"create snapshot {name} for account sys")
        return name

    def trace_source(self, memory_id: str) -> list[str]:
        """Get source event IDs for a memory."""
        with self._db() as db:
            row = db.execute(text(
                "SELECT source_event_ids FROM mem_memories WHERE memory_id = :mid"
            ), {"mid": memory_id}).fetchone()
        if not row or not row.source_event_ids:
            return []
        import json
        try:
            return json.loads(row.source_event_ids) if isinstance(row.source_event_ids, str) else row.source_event_ids
        except:
            return []
