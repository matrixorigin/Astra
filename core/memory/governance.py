"""Memory Governance — scheduled cleanup, health checks.

Confidence decay removed — decay is now query-time only via effective_confidence().
Reflector removed — no episodic→semantic promotion.
"""

from __future__ import annotations

import logging
import time
from dataclasses import dataclass, field
from datetime import datetime, timedelta
from typing import Any, Optional

from sqlalchemy import text

from core.db_consumer import DbConsumer, DbFactory
from core.memory.config import MemoryGovernanceConfig, DEFAULT_CONFIG
from core.memory.health import MemoryHealth
from core.memory.metrics import MemoryMetrics
from core.memory.store import MemoryStore

logger = logging.getLogger(__name__)


@dataclass
class GovernanceStepStats:
    executed: bool = False
    success: bool = False
    error: Optional[str] = None
    count: int = 0
    elapsed_ms: float = 0.0


@dataclass
class GovernanceCycleResult:
    cleaned_stale: int = 0
    cleaned_branches: int = 0
    cleaned_snapshots: int = 0
    cleaned_tool_results: int = 0
    pollution_detected: bool = False
    errors: list[str] = field(default_factory=list)

    health_stats: Optional[GovernanceStepStats] = None
    cleanup_stale_stats: Optional[GovernanceStepStats] = None
    cleanup_branches_stats: Optional[GovernanceStepStats] = None
    cleanup_snapshots_stats: Optional[GovernanceStepStats] = None
    cleanup_tool_results_stats: Optional[GovernanceStepStats] = None
    total_ms: float = 0.0


class GovernanceScheduler(DbConsumer):
    """Periodic memory governance tasks.

    No longer mutates confidence — decay is query-time only.
    No longer runs Reflector — episodic type eliminated.
    """

    def __init__(
        self,
        db_factory: DbFactory,
        config: Optional[MemoryGovernanceConfig] = None,
        metrics: Optional[MemoryMetrics] = None,
    ):
        super().__init__(db_factory)
        self.config = config or DEFAULT_CONFIG
        self._metrics = metrics or MemoryMetrics()
        self.store = MemoryStore(db_factory, metrics=self._metrics)
        self.health = MemoryHealth(
            db_factory,
            pollution_threshold=self.config.pollution_threshold,
        )
        self._last_cycle: dict[str, datetime] = {}

    def run_cycle(self, user_id: str, explain: bool = False) -> GovernanceCycleResult:
        start = time.time() if explain else 0
        result = GovernanceCycleResult()
        last_run = self._last_cycle.get(user_id, datetime.utcnow() - timedelta(days=1))

        # 1. Health check
        self._run_step(
            result, "health", explain,
            lambda: self._health_check(user_id, last_run, result),
        )

        # 2. Cleanup stale inactive memories
        self._run_step(
            result, "cleanup_stale", explain,
            lambda: self._cleanup_stale(user_id),
        )

        # 3. Cleanup orphan branches
        self._run_step(
            result, "cleanup_branches", explain,
            lambda: self.health.cleanup_orphan_branches(),
        )

        # 4. Cleanup old snapshots
        self._run_step(
            result, "cleanup_snapshots", explain,
            lambda: self.health.cleanup_snapshots(keep_last_n=self.config.milestone_snapshot_keep_n),
        )

        # 5. Cleanup expired TOOL_RESULT memories
        self._run_step(
            result, "cleanup_tool_results", explain,
            lambda: self._cleanup_tool_results(),
        )

        if explain:
            result.total_ms = (time.time() - start) * 1000
        self._last_cycle[user_id] = datetime.utcnow()
        return result

    def _run_step(self, result: GovernanceCycleResult, name: str, explain: bool, fn) -> None:
        step_start = time.time() if explain else 0
        try:
            count = fn() or 0
            if name == "health":
                pass  # health_check sets result fields directly
            else:
                setattr(result, f"cleaned_{name.replace('cleanup_', '')}", count)
            if explain:
                setattr(result, f"{name}_stats", GovernanceStepStats(
                    executed=True, success=True, count=count,
                    elapsed_ms=(time.time() - step_start) * 1000,
                ))
        except Exception as e:
            logger.error("Governance step %s failed: %s", name, e)
            result.errors.append(f"{name}: {e}")
            if explain:
                setattr(result, f"{name}_stats", GovernanceStepStats(
                    executed=True, success=False, error=str(e),
                    elapsed_ms=(time.time() - step_start) * 1000,
                ))

    def _health_check(self, user_id: str, last_run: datetime, result: GovernanceCycleResult) -> int:
        pollution = self.health.detect_pollution(user_id, last_run)
        result.pollution_detected = pollution.get("is_polluted", False)
        if result.pollution_detected:
            logger.warning("Pollution detected for %s: ratio=%.2f", user_id, pollution.get("ratio", 0))
        return 0

    def _cleanup_stale(self, user_id: str, confidence_threshold: float = 0.1) -> int:
        """Delete inactive memories with low initial_confidence (already superseded)."""
        with self._db() as db:
            result = db.execute(
                text("""
                    DELETE FROM memories
                    WHERE user_id = :uid
                      AND is_active = 0
                      AND initial_confidence < :threshold
                """),
                {"uid": user_id, "threshold": confidence_threshold},
            )
            db.commit()
            return result.rowcount

    def _cleanup_tool_results(self) -> int:
        ttl_hours = self.config.tool_result_ttl_hours
        with self._db() as db:
            result = db.execute(
                text("""
                    DELETE FROM memories
                    WHERE memory_type = :mtype
                      AND TIMESTAMPDIFF(HOUR, observed_at, NOW()) > :ttl
                """),
                {"mtype": "tool_result", "ttl": ttl_hours},
            )
            db.commit()
            count = result.rowcount
            if count > 0:
                logger.info("Cleaned %d expired TOOL_RESULT memories (TTL=%dh)", count, ttl_hours)
            return count
