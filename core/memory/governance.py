"""Memory Governance — scheduled decay, cleanup, health checks.

Supports explain=True for EXPLAIN ANALYZE style execution stats.
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
from core.memory.store import MemoryStore
from core.memory.typed_reflector import TypedReflector

logger = logging.getLogger(__name__)


@dataclass
class GovernanceStepStats:
    """Stats for a single governance step."""
    executed: bool = False
    success: bool = False
    error: Optional[str] = None
    count: int = 0
    elapsed_ms: float = 0.0


@dataclass
class GovernanceCycleResult:
    """Result of one governance cycle with detailed stats."""

    decayed_count: int = 0
    promoted_count: int = 0
    cleaned_stale: int = 0
    cleaned_branches: int = 0
    cleaned_snapshots: int = 0
    cleaned_tool_results: int = 0
    pollution_detected: bool = False
    errors: list[str] = field(default_factory=list)
    
    # Detailed stats per step (populated when explain=True)
    decay_stats: Optional[GovernanceStepStats] = None
    reflector_stats: Optional[GovernanceStepStats] = None
    health_stats: Optional[GovernanceStepStats] = None
    cleanup_stale_stats: Optional[GovernanceStepStats] = None
    cleanup_branches_stats: Optional[GovernanceStepStats] = None
    cleanup_snapshots_stats: Optional[GovernanceStepStats] = None
    cleanup_tool_results_stats: Optional[GovernanceStepStats] = None
    total_ms: float = 0.0


class GovernanceScheduler(DbConsumer):
    """Periodic memory governance tasks."""

    def __init__(
        self,
        db_factory: DbFactory,
        config: Optional[MemoryGovernanceConfig] = None,
        llm_client: Any = None,
        embed_fn: Any = None,
    ):
        super().__init__(db_factory)
        self.config = config or DEFAULT_CONFIG
        self.store = MemoryStore(db_factory)
        self.health = MemoryHealth(
            db_factory,
            pollution_threshold=self.config.pollution_threshold,
        )
        self.reflector = TypedReflector(
            store=self.store,
            llm_client=llm_client,
            embed_fn=embed_fn,
            cluster_similarity=self.config.reflector_cluster_similarity,
            cluster_min_size=self.config.reflector_cluster_min_size,
        )
        self._last_cycle: dict[str, datetime] = {}

    def run_cycle(self, user_id: str, explain: bool = False) -> GovernanceCycleResult:
        """Run one governance cycle for a user.
        
        Args:
            explain: If True, populate detailed stats for each step.
        """
        start = time.time() if explain else 0
        result = GovernanceCycleResult()
        last_run = self._last_cycle.get(user_id, datetime.utcnow() - timedelta(days=1))

        # 1. Confidence decay
        step_start = time.time() if explain else 0
        try:
            result.decayed_count = self._apply_decay(user_id)
            if explain:
                result.decay_stats = GovernanceStepStats(
                    executed=True, success=True, count=result.decayed_count,
                    elapsed_ms=(time.time() - step_start) * 1000
                )
        except Exception as e:
            logger.error("Decay failed for %s: %s", user_id, e)
            result.errors.append(f"decay: {e}")
            if explain:
                result.decay_stats = GovernanceStepStats(
                    executed=True, success=False, error=str(e),
                    elapsed_ms=(time.time() - step_start) * 1000
                )

        # 2. Reflector (episodic→semantic)
        step_start = time.time() if explain else 0
        try:
            reflect_result = self.reflector.reflect(user_id)
            result.promoted_count = reflect_result.get("promoted", 0)
            if explain:
                result.reflector_stats = GovernanceStepStats(
                    executed=True, success=True, count=result.promoted_count,
                    elapsed_ms=(time.time() - step_start) * 1000
                )
        except Exception as e:
            logger.error("Reflector failed for %s: %s", user_id, e)
            result.errors.append(f"reflector: {e}")
            if explain:
                result.reflector_stats = GovernanceStepStats(
                    executed=True, success=False, error=str(e),
                    elapsed_ms=(time.time() - step_start) * 1000
                )

        # 3. Health check
        step_start = time.time() if explain else 0
        try:
            pollution = self.health.detect_pollution(user_id, last_run)
            result.pollution_detected = pollution.get("is_polluted", False)
            if result.pollution_detected:
                logger.warning(
                    "Pollution detected for %s: ratio=%.2f",
                    user_id,
                    pollution.get("ratio", 0),
                )
            if explain:
                result.health_stats = GovernanceStepStats(
                    executed=True, success=True,
                    elapsed_ms=(time.time() - step_start) * 1000
                )
        except Exception as e:
            logger.error("Health check failed for %s: %s", user_id, e)
            result.errors.append(f"health: {e}")
            if explain:
                result.health_stats = GovernanceStepStats(
                    executed=True, success=False, error=str(e),
                    elapsed_ms=(time.time() - step_start) * 1000
                )

        # 4. Cleanup stale inactive memories
        step_start = time.time() if explain else 0
        try:
            result.cleaned_stale = self._cleanup_stale(user_id)
            if explain:
                result.cleanup_stale_stats = GovernanceStepStats(
                    executed=True, success=True, count=result.cleaned_stale,
                    elapsed_ms=(time.time() - step_start) * 1000
                )
        except Exception as e:
            logger.error("Stale cleanup failed for %s: %s", user_id, e)
            result.errors.append(f"cleanup_stale: {e}")
            if explain:
                result.cleanup_stale_stats = GovernanceStepStats(
                    executed=True, success=False, error=str(e),
                    elapsed_ms=(time.time() - step_start) * 1000
                )

        # 5. Cleanup orphan branches (global, not per-user)
        step_start = time.time() if explain else 0
        try:
            result.cleaned_branches = self.health.cleanup_orphan_branches()
            if explain:
                result.cleanup_branches_stats = GovernanceStepStats(
                    executed=True, success=True, count=result.cleaned_branches,
                    elapsed_ms=(time.time() - step_start) * 1000
                )
        except Exception as e:
            logger.error("Branch cleanup failed: %s", e)
            result.errors.append(f"cleanup_branches: {e}")
            if explain:
                result.cleanup_branches_stats = GovernanceStepStats(
                    executed=True, success=False, error=str(e),
                    elapsed_ms=(time.time() - step_start) * 1000
                )

        # 6. Cleanup old snapshots
        step_start = time.time() if explain else 0
        try:
            result.cleaned_snapshots = self.health.cleanup_snapshots(
                keep_last_n=self.config.milestone_snapshot_keep_n
            )
            if explain:
                result.cleanup_snapshots_stats = GovernanceStepStats(
                    executed=True, success=True, count=result.cleaned_snapshots,
                    elapsed_ms=(time.time() - step_start) * 1000
                )
        except Exception as e:
            logger.error("Snapshot cleanup failed: %s", e)
            result.errors.append(f"cleanup_snapshots: {e}")
            if explain:
                result.cleanup_snapshots_stats = GovernanceStepStats(
                    executed=True, success=False, error=str(e),
                    elapsed_ms=(time.time() - step_start) * 1000
                )

        # 7. Cleanup expired TOOL_RESULT memories (TTL-based)
        step_start = time.time() if explain else 0
        try:
            result.cleaned_tool_results = self._cleanup_tool_results()
            if explain:
                result.cleanup_tool_results_stats = GovernanceStepStats(
                    executed=True, success=True, count=result.cleaned_tool_results,
                    elapsed_ms=(time.time() - step_start) * 1000
                )
        except Exception as e:
            logger.error("Tool result cleanup failed: %s", e)
            result.errors.append(f"cleanup_tool_results: {e}")
            if explain:
                result.cleanup_tool_results_stats = GovernanceStepStats(
                    executed=True, success=False, error=str(e),
                    elapsed_ms=(time.time() - step_start) * 1000
                )

        if explain:
            result.total_ms = (time.time() - start) * 1000
        self._last_cycle[user_id] = datetime.utcnow()
        return result

    def _apply_decay(self, user_id: str) -> int:
        """Apply confidence decay: conf *= exp(-age_days / half_life)."""
        half_life = self.config.confidence_decay_half_life_days
        with self._db() as db:
            result = db.execute(
                text("""
                    UPDATE memories
                    SET confidence = confidence * EXP(
                        -TIMESTAMPDIFF(DAY, observed_at, NOW()) / :half_life
                    )
                    WHERE user_id = :uid
                      AND is_active = 1
                      AND TIMESTAMPDIFF(DAY, observed_at, NOW()) > 0
                """),
                {"uid": user_id, "half_life": half_life},
            )
            db.commit()
            return result.rowcount

    def _cleanup_stale(
        self, user_id: str, confidence_threshold: float = 0.1
    ) -> int:
        """Delete inactive memories with confidence below threshold."""
        with self._db() as db:
            result = db.execute(
                text("""
                    DELETE FROM memories
                    WHERE user_id = :uid
                      AND is_active = 0
                      AND confidence < :threshold
                """),
                {"uid": user_id, "threshold": confidence_threshold},
            )
            db.commit()
            return result.rowcount

    def _cleanup_tool_results(self) -> int:
        """Delete TOOL_RESULT memories older than configured TTL.

        Unlike other memory types, TOOL_RESULT has a hard TTL (default 24h)
        independent of confidence decay. This prevents tool output accumulation.
        """
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
