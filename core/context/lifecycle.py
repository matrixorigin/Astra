"""Memory lifecycle governance engine for sk_knowledge_entries.

Manages the knowledge skill's entry lifecycle: quarantine, contradiction
scanning, health reports. Confidence decay and episodic compression have
been removed — decay is query-time only (see core/memory/types.py),
and session summaries are handled by SessionSummarizer.

The `memories` table governance is handled by core.memory.governance.GovernanceScheduler.
"""

from datetime import datetime
from typing import Any
from sqlalchemy import func
from core.logging_config import get_logger
from core.db_consumer import DbConsumer, DbFactory

logger = get_logger(__name__)


# Re-export for backward compatibility — canonical source is core.memory.types
from core.memory.types import trust_tier_defaults  # noqa: F401


class MemoryGovernanceEngine(DbConsumer):
    """Knowledge entry lifecycle governance (sk_knowledge_entries table).

    Hourly: archive closed scratchpad notes, sandbox cleanup
    Daily: quarantine low-confidence entries
    Weekly: contradiction scan, health reports, SLO check
    """

    def __init__(self, db_factory: DbFactory, llm_client=None):
        super().__init__(db_factory)
        self.llm_client = llm_client

    def _get_agent_ids(self) -> list[str]:
        with self._db() as db:
            from api.models import Agent
            return [a.agent_id for a in db.query(Agent.agent_id).all()]

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
            for aid in (agent_ids or ["dev-agent"]):
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
            notes = db.query(AgentScratchpad).filter(
                AgentScratchpad.status == "completed"
            ).all()
            count = len(notes)
            logger.debug("Archived %d completed notes", count)
            return count

    def _quarantine_low_confidence(self, threshold: float = 0.3) -> int:
        with self._db() as db:
            from api.models import KnowledgeEntry
            to_quarantine = db.query(
                KnowledgeEntry.entry_id, KnowledgeEntry.key_name, KnowledgeEntry.confidence,
            ).filter(
                KnowledgeEntry.confidence < threshold,
                KnowledgeEntry.confidence > 0,
            ).all()
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
            logger.info("Quarantined %d low-confidence entries (threshold=%.2f)", len(ids), threshold)
            return len(ids)

    def _scan_contradictions(self) -> int:
        with self._db() as db:
            from api.models import KnowledgeEntry
            from sqlalchemy import text
            conflicts = db.query(
                KnowledgeEntry.category,
                KnowledgeEntry.key_name,
                func.count(func.distinct(KnowledgeEntry.value)).label("val_count"),
            ).filter(
                KnowledgeEntry.confidence > 0.3
            ).group_by(
                KnowledgeEntry.category, KnowledgeEntry.key_name
            ).having(
                func.count(func.distinct(KnowledgeEntry.value)) > 1
            ).limit(100).all()
            if not conflicts:
                return 0
            dedup_keys = [f"{c.category}:{c.key_name}" for c in conflicts]
            reported = set()
            if dedup_keys:
                rows = db.execute(
                    text("""SELECT dedup_key FROM conversation_events
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
                entries = db.query(KnowledgeEntry).filter(
                    KnowledgeEntry.category == c.category,
                    KnowledgeEntry.key_name == c.key_name,
                    KnowledgeEntry.confidence > 0.3,
                ).limit(10).all()
                contradictions += 1
                self._write_governance_event(
                    "contradiction_detected",
                    {"dedup_key": dk, "category": c.category, "key": c.key_name,
                     "entry_ids": [e.entry_id for e in entries],
                     "values": list(set(e.value for e in entries))[:5]},
                    dedup_key=dk,
                )
                logger.warning("Contradiction: %s.%s has %d different values", c.category, c.key_name, c.val_count)
            return contradictions

    def _write_governance_event(self, event_type: str, content: dict[str, Any], dedup_key: str | None = None) -> None:
        with self._db() as db:
            import json
            from uuid_utils import uuid7
            from sqlalchemy import text
            eid = str(uuid7())
            try:
                db.execute(
                    text("""INSERT INTO conversation_events
                            (event_id, session_id, user_id, agent_id, agent_version,
                             event_type, content, causal_chain_id, dedup_key, created_at)
                            VALUES (:eid, 'system_governance', 'system', 'governance', '1.0',
                                    :etype, :content, :cid, :dk, :ts)"""),
                    {"eid": eid, "etype": event_type,
                     "content": json.dumps(content, default=str),
                     "cid": eid, "dk": dedup_key, "ts": datetime.now()},
                )
                db.commit()
            except Exception as e:
                logger.debug("governance event write failed: %s", e)
                db.rollback()

    def _generate_health_reports(self) -> int:
        with self._db() as db:
            from api.models import KnowledgeEntry
            from sqlalchemy import distinct
            users = db.query(distinct(KnowledgeEntry.user_id)).all()
            reports = 0
            for (user_id,) in users:
                stats = self._get_user_memory_stats(user_id)
                logger.info(
                    "Memory health for %s: %d entries, avg confidence %.2f, %d low confidence",
                    user_id, stats["total_entries"], stats["avg_confidence"], stats["low_confidence"],
                )
                reports += 1
            return reports

    def _get_user_memory_stats(self, user_id: str) -> dict[str, Any]:
        with self._db() as db:
            from api.models import KnowledgeEntry
            entries = db.query(KnowledgeEntry).filter(KnowledgeEntry.user_id == user_id).all()
            if not entries:
                return {"total_entries": 0, "avg_confidence": 0.0, "low_confidence": 0}
            total = len(entries)
            avg_conf = sum(e.confidence for e in entries) / total
            low_conf = sum(1 for e in entries if e.confidence < 0.3)
            return {"total_entries": total, "avg_confidence": avg_conf, "low_confidence": low_conf}

    def governance_stats(self) -> dict[str, Any]:
        with self._db() as db:
            from api.models import KnowledgeEntry
            entries = db.query(KnowledgeEntry).all()
            total = len(entries)
            if total == 0:
                return {"total_entries": 0}
            confidences = [e.confidence for e in entries]
            tier_counts: dict[str, int] = {}
            quarantined = 0
            for e in entries:
                tier_counts[e.trust_tier] = tier_counts.get(e.trust_tier, 0) + 1
                if e.confidence < 0.3:
                    quarantined += 1
            contradictions = self._scan_contradictions()
            return {
                "total_entries": total,
                "avg_confidence": sum(confidences) / total,
                "min_confidence": min(confidences),
                "quarantined": quarantined,
                "quarantine_pct": round(quarantined / total * 100, 1),
                "tier_distribution": tier_counts,
                "contradictions": contradictions,
            }
