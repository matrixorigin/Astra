"""Memory Governance — frequency-separated cleanup, quarantine, health.

Confidence decay removed — decay is now query-time only via effective_confidence().
Reflector removed — no episodic→semantic promotion.

Governance cycles:
  - hourly: tool_result cleanup, working memory archival
  - daily: stale inactive cleanup, quarantine low effective_confidence
  - weekly: orphan branch cleanup, snapshot cleanup, health report
"""

from __future__ import annotations

import logging
import time
from dataclasses import dataclass, field
from datetime import datetime, timedelta, timezone
from typing import Any, Optional

from sqlalchemy import text

from core.db_consumer import DbConsumer, DbFactory
from core.memory.config import MemoryGovernanceConfig, DEFAULT_CONFIG
from core.memory.health import MemoryHealth
from core.memory.metrics import MemoryMetrics


from core.memory.store import MemoryStore
from core.memory.types import TRUST_TIER_HALF_LIVES, TrustTier, _utcnow

logger = logging.getLogger(__name__)


@dataclass
class GovernanceCycleResult:
    # Hourly
    cleaned_tool_results: int = 0
    archived_working: int = 0
    # Daily
    cleaned_stale: int = 0
    quarantined: int = 0
    # Weekly
    cleaned_branches: int = 0
    cleaned_snapshots: int = 0
    # Health
    pollution_detected: bool = False
    errors: list[str] = field(default_factory=list)
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

    # ── Convenience: run all ──────────────────────────────────────────

    def run_cycle(self, user_id: str) -> GovernanceCycleResult:
        """Run all governance frequencies. Convenience for single-instance deployments."""
        result = GovernanceCycleResult()
        start = time.time()

        h = self.run_hourly()
        result.cleaned_tool_results = h.cleaned_tool_results
        result.archived_working = h.archived_working
        result.errors.extend(h.errors)

        d = self.run_daily(user_id)
        result.cleaned_stale = d.cleaned_stale
        result.quarantined = d.quarantined
        result.pollution_detected = d.pollution_detected
        result.errors.extend(d.errors)

        w = self.run_weekly()
        result.cleaned_branches = w.cleaned_branches
        result.cleaned_snapshots = w.cleaned_snapshots
        result.errors.extend(w.errors)

        result.total_ms = (time.time() - start) * 1000
        return result

    # ── Hourly ────────────────────────────────────────────────────────

    def run_hourly(self) -> GovernanceCycleResult:
        """Hourly: tool_result cleanup + working memory archival."""
        result = GovernanceCycleResult()
        try:
            result.cleaned_tool_results = self._cleanup_tool_results()
        except Exception as e:
            logger.error("Tool result cleanup failed: %s", e)
            result.errors.append(f"tool_results: {e}")
        try:
            result.archived_working = self._archive_stale_working()
        except Exception as e:
            logger.error("Working memory archival failed: %s", e)
            result.errors.append(f"working_archival: {e}")
        return result

    # ── Daily ─────────────────────────────────────────────────────────

    def run_daily_all(self) -> GovernanceCycleResult:
        """Daily governance for ALL users. Used by scheduler."""
        combined = GovernanceCycleResult()
        batch_size = 2000
        last_uid = ""
        with self._db() as db:
            while True:
                rows = db.execute(
                    text(
                        "SELECT DISTINCT user_id FROM mem_memories "
                        "WHERE is_active = 1 AND user_id > :last "
                        "ORDER BY user_id LIMIT :limit"
                    ),
                    {"last": last_uid, "limit": batch_size},
                ).fetchall()
                if not rows:
                    break
                for (uid,) in rows:
                    r = self.run_daily(uid)
                    combined.cleaned_stale += r.cleaned_stale
                    combined.quarantined += r.quarantined
                    combined.errors.extend(r.errors)
                last_uid = rows[-1][0]
                if len(rows) < batch_size:
                    break
        return combined

    def run_daily(self, user_id: str) -> GovernanceCycleResult:
        """Daily: stale cleanup + quarantine low effective_confidence."""
        result = GovernanceCycleResult()
        try:
            result.cleaned_stale = self._cleanup_stale(user_id)
        except Exception as e:
            logger.error("Stale cleanup failed: %s", e)
            result.errors.append(f"stale: {e}")
        try:
            result.quarantined = self._quarantine_low_confidence(user_id)
        except Exception as e:
            logger.error("Quarantine failed: %s", e)
            result.errors.append(f"quarantine: {e}")
        try:
            pollution = self.health.detect_pollution(user_id, _utcnow() - timedelta(days=1))
            result.pollution_detected = pollution.get("is_polluted", False)
        except Exception as e:
            logger.error("Pollution detection failed: %s", e)
            result.errors.append(f"pollution: {e}")
        return result

    # ── Weekly ────────────────────────────────────────────────────────

    def run_weekly(self) -> GovernanceCycleResult:
        """Weekly: orphan branch cleanup + snapshot cleanup."""
        result = GovernanceCycleResult()
        try:
            result.cleaned_branches = self.health.cleanup_orphan_branches()
        except Exception as e:
            logger.error("Branch cleanup failed: %s", e)
            result.errors.append(f"branches: {e}")
        try:
            result.cleaned_snapshots = self.health.cleanup_snapshots(
                keep_last_n=self.config.milestone_snapshot_keep_n
            )
        except Exception as e:
            logger.error("Snapshot cleanup failed: %s", e)
            result.errors.append(f"snapshots: {e}")
        return result

    # ── Internal steps ────────────────────────────────────────────────

    def _cleanup_tool_results(self) -> int:
        ttl = self.config.tool_result_ttl_hours
        total = 0
        batch_limit = 5000
        with self._db() as db:
            while True:
                result = db.execute(text("""
                    DELETE FROM mem_memories
                    WHERE memory_type = :mtype
                      AND TIMESTAMPDIFF(HOUR, observed_at, NOW()) > :ttl
                    LIMIT :batch
                """), {"mtype": "tool_result", "ttl": ttl, "batch": batch_limit})
                db.commit()
                total += result.rowcount
                if result.rowcount < batch_limit:
                    break
        if total > 0:
            logger.info("Cleaned %d expired TOOL_RESULT memories (TTL=%dh)", total, ttl)
        return total

    def _archive_stale_working(self) -> int:
        """Archive working memories from sessions inactive > threshold hours."""
        stale_hours = self.config.working_memory_stale_hours
        with self._db() as db:
            result = db.execute(text("""
                UPDATE mem_memories SET is_active = 0, updated_at = NOW()
                WHERE memory_type = 'working' AND is_active = 1
                  AND TIMESTAMPDIFF(HOUR, observed_at, NOW()) > :stale_hours
            """), {"stale_hours": stale_hours})
            db.commit()
            count = result.rowcount
        if count > 0:
            logger.info("Archived %d stale working memories (>%dh)", count, stale_hours)
        return count

    def _cleanup_stale(self, user_id: str, confidence_threshold: float = 0.1) -> int:
        """Delete inactive memories with low initial_confidence (already superseded)."""
        with self._db() as db:
            result = db.execute(text("""
                DELETE FROM mem_memories
                WHERE user_id = :uid
                  AND is_active = 0
                  AND initial_confidence < :threshold
            """), {"uid": user_id, "threshold": confidence_threshold})
            db.commit()
            return result.rowcount

    def _quarantine_low_confidence(self, user_id: str) -> int:
        """Deactivate memories whose effective_confidence is below quarantine threshold.

        Uses per-tier half-life: T1=365d, T2=180d, T3=60d, T4=30d.
        Memories with no trust_tier default to T3 (60d).
        """
        threshold = self.config.quarantine_threshold
        quarantined = 0
        with self._db() as db:
            for tier in TrustTier:
                hl = TRUST_TIER_HALF_LIVES[tier]
                result = db.execute(text("""
                    UPDATE mem_memories SET is_active = 0, updated_at = NOW()
                    WHERE user_id = :uid AND is_active = 1
                      AND COALESCE(trust_tier, 'T3') = :tier
                      AND (initial_confidence * EXP(-TIMESTAMPDIFF(DAY, observed_at, NOW()) / :hl)) < :threshold
                """), {"uid": user_id, "tier": tier.value, "hl": hl, "threshold": threshold})
                quarantined += result.rowcount
            db.commit()
        if quarantined > 0:
            logger.info("Quarantined %d memories below threshold %.2f", quarantined, threshold)
        return quarantined
