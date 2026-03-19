"""Memory lifecycle governance engine for sk_knowledge_entries.

Manages the knowledge skill's entry lifecycle: quarantine, contradiction
scanning, health reports. Confidence decay and episodic compression have
been removed — decay is query-time only (see core/memory/types.py),
and session summaries are handled by SessionSummarizer.

The `memories` table governance is handled by core.memory.governance.GovernanceScheduler.
"""

import time
from datetime import datetime
from typing import Any

from sqlalchemy import case, func
from sqlalchemy.engine import Connection, Engine
from sqlalchemy.orm import sessionmaker

from core.db_consumer import DbConsumer, DbFactory
from core.logging_config import get_logger

logger = get_logger(__name__)


# Re-export for backward compatibility — canonical source is core.memory.types
from core.memory.types import trust_tier_defaults  # noqa: F401


_LOW_CONFIDENCE_THRESHOLD = 0.3


def _low_confidence_expr(model, threshold: float = _LOW_CONFIDENCE_THRESHOLD):
    """Reusable SQL expression: COUNT entries below confidence threshold."""
    return func.sum(case((model.confidence < threshold, 1), else_=0))


class MemoryGovernanceEngine(DbConsumer):
    """Knowledge entry lifecycle governance (sk_knowledge_entries table).

    Hourly: archive closed scratchpad notes, sandbox cleanup
    Daily: quarantine low-confidence entries
    Weekly: contradiction scan, health reports, SLO check
    """

    def __init__(self, db_factory: DbFactory, llm_client=None):
        super().__init__(db_factory)
        self.llm_client = llm_client

    # Safety caps for background batch queries — prevents runaway scans.
    _MAX_AGENTS = 100
    _MAX_HEALTH_REPORT_USERS = 200

    def _get_agent_ids(self) -> list[str]:
        with self._db() as db:
            from api.models import Agent

            return [a.agent_id for a in db.query(Agent.agent_id).limit(self._MAX_AGENTS).all()]

    # ── Hourly ────────────────────────────────────────────────────────

    def run_hourly_tasks(self) -> dict[str, int]:
        results = {}
        results["archived_notes"] = self._archive_closed_notes()
        try:
            from core.sandbox.cleanup import SandboxCleaner

            cleaner = SandboxCleaner(db_factory=self._db_factory)
            cleanup = cleaner.run()
            results["sandbox_cleaned"] = cleanup.get("cleaned", 0)
            results["sandbox_failed"] = cleanup.get("failed", 0)
        except Exception as e:
            logger.error("Sandbox cleanup failed: %s", e)
            results["sandbox_cleaned"] = 0
        logger.info("Hourly tasks: %s", results)
        return results

    # ── Daily ─────────────────────────────────────────────────────────

    def run_daily_tasks(self) -> dict[str, int]:
        results = {}
        results["quarantined"] = self._quarantine_low_confidence()
        logger.info("Daily tasks: %s", results)
        return results

    # ── Weekly ────────────────────────────────────────────────────────

    def run_weekly_tasks(self) -> dict[str, int]:
        results = {}
        results["contradictions_found"] = self._scan_contradictions()
        results["health_reports"] = self._generate_health_reports()
        try:
            from core.evaluation.slo_monitor import SLOMonitor

            monitor = SLOMonitor(self._db_factory)
            agent_ids = self._get_agent_ids()
            total_violations = 0
            for aid in agent_ids or ["dev-agent"]:
                report = monitor.check_agent(aid, period_days=7)
                total_violations += sum(1 for s in report.statuses if not s.met)
            results["slo_violations"] = total_violations
        except Exception as e:
            logger.debug("SLO check skipped: %s", e)
        logger.info("Weekly tasks: %s", results)
        return results

    # ── Internal ──────────────────────────────────────────────────────

    def _archive_closed_notes(self) -> int:
        with self._db() as db:
            from api.models import AgentScratchpad

            count = (
                db.query(AgentScratchpad)
                .filter(AgentScratchpad.status == "completed")
                .update(
                    {
                        AgentScratchpad.status: "archived",
                        AgentScratchpad.updated_at: datetime.now(),
                    },
                    synchronize_session=False,
                )
            )
            if count:
                db.commit()
            logger.debug("Archived %d completed notes", count)
            return count

    def _quarantine_low_confidence(self, threshold: float = 0.3) -> int:
        with self._db() as db:
            from api.models import KnowledgeEntry

            to_quarantine = (
                db.query(
                    KnowledgeEntry.entry_id,
                    KnowledgeEntry.key_name,
                    KnowledgeEntry.confidence,
                )
                .filter(
                    KnowledgeEntry.confidence < threshold,
                    KnowledgeEntry.confidence > 0,
                )
                .all()
            )
            if not to_quarantine:
                return 0
            ids = [r[0] for r in to_quarantine]
            db.query(KnowledgeEntry).filter(
                KnowledgeEntry.entry_id.in_(ids),
            ).update(
                {KnowledgeEntry.confidence: 0, KnowledgeEntry.updated_at: datetime.now()},
                synchronize_session=False,
            )
            db.commit()
            if ids:
                self._write_governance_event(
                    "governance_quarantine",
                    {"entry_ids": ids, "threshold": threshold, "count": len(ids)},
                )
            logger.info(
                "Quarantined %d low-confidence entries (threshold=%.2f)", len(ids), threshold
            )
            return len(ids)

    def _scan_contradictions(self) -> int:
        with self._db() as db:
            from api.models import KnowledgeEntry
            from sqlalchemy import text

            conflicts = (
                db.query(
                    KnowledgeEntry.category,
                    KnowledgeEntry.key_name,
                    func.count(func.distinct(KnowledgeEntry.value)).label("val_count"),
                )
                .filter(KnowledgeEntry.confidence > 0.3)
                .group_by(KnowledgeEntry.category, KnowledgeEntry.key_name)
                .having(func.count(func.distinct(KnowledgeEntry.value)) > 1)
                .limit(100)
                .all()
            )
            if not conflicts:
                return 0
            dedup_keys = [f"{c.category}:{c.key_name}" for c in conflicts]
            reported = set()
            if dedup_keys:
                rows = db.execute(
                    text("""SELECT dedup_key FROM agent_events
                            WHERE event_type = 'contradiction_detected'
                            AND dedup_key IN :keys"""),
                    {"keys": tuple(dedup_keys)},
                ).fetchall()
                reported = {r[0] for r in rows}
            contradictions = 0
            for c in conflicts:
                dk = f"{c.category}:{c.key_name}"
                if dk in reported:
                    continue
                entries = (
                    db.query(KnowledgeEntry)
                    .filter(
                        KnowledgeEntry.category == c.category,
                        KnowledgeEntry.key_name == c.key_name,
                        KnowledgeEntry.confidence > 0.3,
                    )
                    .limit(10)
                    .all()
                )
                contradictions += 1
                self._write_governance_event(
                    "contradiction_detected",
                    {
                        "dedup_key": dk,
                        "category": c.category,
                        "key": c.key_name,
                        "entry_ids": [e.entry_id for e in entries],
                        "values": list(set(e.value for e in entries))[:5],
                    },
                    dedup_key=dk,
                )
                logger.warning(
                    "Contradiction: %s.%s has %d different values",
                    c.category,
                    c.key_name,
                    c.val_count,
                )
            return contradictions

    def _write_governance_event(
        self, event_type: str, content: dict[str, Any], dedup_key: str | None = None
    ) -> None:
        with self._db() as db:
            import json
            from uuid_utils import uuid7
            from sqlalchemy import text

            eid = str(uuid7())
            try:
                db.execute(
                    text("""INSERT INTO agent_events
                            (event_id, session_id, user_id, agent_id, agent_version,
                             event_type, content, causal_chain_id, dedup_key, created_at)
                            VALUES (:eid, 'system_governance', 'system', 'governance', '1.0',
                                    :etype, :content, :cid, :dk, :ts)"""),
                    {
                        "eid": eid,
                        "etype": event_type,
                        "content": json.dumps(content, default=str),
                        "cid": eid,
                        "dk": dedup_key,
                        "ts": datetime.now(),
                    },
                )
                db.commit()
                self._wait_for_governance_event(db, eid)
            except Exception as e:
                logger.debug("governance event write failed: %s", e)
                db.rollback()

    @staticmethod
    def _wait_for_governance_event(
        db,
        event_id: str,
        *,
        attempts: int = 6,
        delay_seconds: float = 0.03,
    ) -> bool:
        if not hasattr(db, "get_bind"):
            return False
        bind = db.get_bind()
        if not isinstance(bind, (Engine, Connection)):
            return False

        from sqlalchemy import text

        for attempt in range(attempts):
            fresh_db = sessionmaker(bind=bind, expire_on_commit=False)()
            try:
                row = fresh_db.execute(
                    text("SELECT 1 FROM agent_events WHERE event_id = :eid"),
                    {"eid": event_id},
                ).fetchone()
            finally:
                fresh_db.close()
            if row is not None:
                return True
            if attempt < attempts - 1:
                time.sleep(delay_seconds * (attempt + 1))
        return False

    def _generate_health_reports(self) -> int:
        with self._db() as db:
            from api.models import KnowledgeEntry

            low_expr = _low_confidence_expr(KnowledgeEntry)
            rows = (
                db.query(
                    KnowledgeEntry.user_id,
                    func.count(KnowledgeEntry.entry_id),
                    func.avg(KnowledgeEntry.confidence),
                    low_expr,
                )
                .group_by(KnowledgeEntry.user_id)
                .limit(self._MAX_HEALTH_REPORT_USERS)
                .all()
            )
            for user_id, total, avg_conf, low_conf in rows:
                logger.info(
                    "Memory health for %s: %d entries, avg confidence %.2f, %d low confidence",
                    user_id,
                    total,
                    float(avg_conf or 0),
                    int(low_conf or 0),
                )
            return len(rows)

    def _get_user_memory_stats(self, user_id: str) -> dict[str, Any]:
        with self._db() as db:
            from api.models import KnowledgeEntry

            low_expr = _low_confidence_expr(KnowledgeEntry)
            row = (
                db.query(
                    func.count(KnowledgeEntry.entry_id),
                    func.avg(KnowledgeEntry.confidence),
                    low_expr,
                )
                .filter(KnowledgeEntry.user_id == user_id)
                .first()
            )
            total = row[0] or 0
            if total == 0:
                return {"total_entries": 0, "avg_confidence": 0.0, "low_confidence": 0}
            return {
                "total_entries": total,
                "avg_confidence": float(row[1] or 0),
                "low_confidence": int(row[2] or 0),
            }

    def governance_stats(self) -> dict[str, Any]:
        with self._db() as db:
            from api.models import KnowledgeEntry

            low_expr = _low_confidence_expr(KnowledgeEntry)
            row = db.query(
                func.count(KnowledgeEntry.entry_id),
                func.avg(KnowledgeEntry.confidence),
                func.min(KnowledgeEntry.confidence),
                low_expr,
            ).first()
            total = row[0] or 0
            if total == 0:
                return {"total_entries": 0}
            quarantined = int(row[3] or 0)

            tier_rows = (
                db.query(
                    KnowledgeEntry.trust_tier,
                    func.count(KnowledgeEntry.entry_id),
                )
                .group_by(KnowledgeEntry.trust_tier)
                .all()
            )
            tier_counts = {r[0]: r[1] for r in tier_rows}

            contradiction_count = (
                db.query(
                    func.count(
                        func.distinct(
                            func.concat(KnowledgeEntry.category, ":", KnowledgeEntry.key_name)
                        )
                    )
                )
                .filter(KnowledgeEntry.confidence > _LOW_CONFIDENCE_THRESHOLD)
                .having(func.count(func.distinct(KnowledgeEntry.value)) > 1)
                .scalar()
                or 0
            )

            return {
                "total_entries": total,
                "avg_confidence": float(row[1] or 0),
                "min_confidence": float(row[2] or 0),
                "quarantined": quarantined,
                "quarantine_pct": round(quarantined / total * 100, 1),
                "tier_distribution": tier_counts,
                "contradictions": contradiction_count,
            }
