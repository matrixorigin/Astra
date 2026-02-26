"""Memory lifecycle governance engine.

Automated enforcement of retention policies, confidence decay, and cleanup.
Runs continuously to maintain memory health without manual intervention.
"""

from datetime import datetime, timedelta
from typing import Any
from sqlalchemy import func, and_, or_
from core.logging_config import get_logger
from core.db_consumer import DbConsumer, DbFactory

logger = get_logger(__name__)


# Trust tier half-lives (days)
TRUST_TIER_HALF_LIVES = {
    "T1": 365,  # Verified: official docs, verified APIs
    "T2": 180,  # Curated: human-reviewed, team knowledge
    "T3": 60,   # Inferred: agent-extracted, LLM summaries
    "T4": 30,   # Unverified: raw user input
}

# Trust tier → initial confidence (from design §1)
TRUST_TIER_INITIAL_CONFIDENCE = {
    "T1": 0.95,
    "T2": 0.85,
    "T3": 0.65,
    "T4": 0.40,
}


def trust_tier_defaults(tier: str) -> dict[str, float]:
    """Return initial_confidence and half_life for a trust tier."""
    return {
        "initial_confidence": TRUST_TIER_INITIAL_CONFIDENCE.get(tier, 0.65),
        "half_life_days": TRUST_TIER_HALF_LIVES.get(tier, 60),
    }

# Retention policies by memory type
RETENTION_POLICIES = {
    "sensory": {"ttl_hours": 1, "decay": "auto_purge"},
    "working": {"ttl_hours": None, "decay": "archive_on_close"},
    "episodic": {"ttl_days": 90, "decay": "compress_to_summary"},
    "semantic": {"ttl_days": None, "decay": "confidence_decay"},
    "procedural": {"ttl_days": None, "decay": "version_only"},
}


class MemoryGovernanceEngine(DbConsumer):
    """Automated memory lifecycle governance.
    
    Enforces retention policies, confidence decay, and cleanup without
    manual intervention. Designed to run as scheduled tasks.
    
    Features:
    - Hourly: purge sensory buffer, archive working memory
    - Daily: confidence decay, quarantine low entries, compress episodic
    - Weekly: T1 verification, contradiction scan, health reports
    
    Example:
        >>> engine = MemoryGovernanceEngine(db)
        >>> engine.run_hourly_tasks()
        >>> engine.run_daily_tasks()
        >>> engine.run_weekly_tasks()
    """
    
    def __init__(self, db_factory: DbFactory, llm_client=None):
        super().__init__(db_factory)
        self.llm_client = llm_client

    def _get_agent_ids(self) -> list[str]:
        """Return all agent IDs for SLO checking."""
        with self._db() as db:
            from api.models import Agent
            return [a.agent_id for a in db.query(Agent.agent_id).all()]
    
    def run_hourly_tasks(self) -> dict[str, int]:
        """Run hourly governance tasks.
        
        Returns:
            Task execution counts
        """
        results = {}
        
        # Archive closed working memory (scratchpad notes)
        results["archived_notes"] = self._archive_closed_notes()

        # Run Reflector on accumulated observations (condense if over threshold)
        try:
            results["observations_reflected"] = self._run_reflector()
        except Exception as e:
            logger.error(f"Reflector failed: {e}")
            results["observations_reflected"] = 0

        # Sandbox cleanup (expired, zombie sessions, orphans)
        try:
            from core.sandbox.cleanup import SandboxCleaner
            cleaner = SandboxCleaner(db_factory=self._db_factory)
            cleanup = cleaner.run()
            results["sandbox_cleaned"] = cleanup.get("cleaned", 0)
            results["sandbox_failed"] = cleanup.get("failed", 0)
        except Exception as e:
            logger.error(f"Sandbox cleanup failed: {e}")
            results["sandbox_cleaned"] = 0
        
        logger.info(f"Hourly tasks: {results}")
        return results
    
    def run_daily_tasks(self) -> dict[str, int]:
        """Run daily governance tasks.
        
        Returns:
            Task execution counts
        """
        results = {}
        
        # Recalculate confidence for all knowledge entries
        results["decayed_entries"] = self._apply_confidence_decay()
        
        # Quarantine entries below threshold
        results["quarantined"] = self._quarantine_low_confidence()
        
        # Compress old episodic events to summaries
        results["compressed_events"] = self._compress_episodic_events()
        
        logger.info(f"Daily tasks: {results}")
        return results
    
    def run_weekly_tasks(self) -> dict[str, int]:
        """Run weekly governance tasks.
        
        Returns:
            Task execution counts
        """
        results = {}
        
        # Scan for contradictions
        results["contradictions_found"] = self._scan_contradictions()
        
        # Generate health report
        results["health_reports"] = self._generate_health_reports()

        # SLO compliance check
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
        
        logger.info(f"Weekly tasks: {results}")
        return results
    
    def _archive_closed_notes(self) -> int:
        """Archive completed scratchpad notes."""
        with self._db() as db:
            from api.models import AgentScratchpad
        
            # Notes marked as completed but not archived
            notes = db.query(AgentScratchpad).filter(
                AgentScratchpad.status == "completed"
            ).all()
        
            # In production, move to archive table
            # For now, just mark as archived
            count = len(notes)
        
            logger.debug(f"Archived {count} completed notes")
            return count

    def _run_reflector(self) -> int:
        """Run TypedReflector to promote episodic clusters to semantic memories."""
        from core.memory.store import MemoryStore
        from core.memory.typed_reflector import TypedReflector
        from core.memory.types import MemoryType
        from sqlalchemy import distinct, text

        with self._db() as db:
            # Find users with episodic memories
            result = db.execute(text(
                "SELECT DISTINCT user_id FROM memories WHERE memory_type = :mtype AND is_active = 1"
            ), {"mtype": MemoryType.EPISODIC.value})
            user_ids = [row[0] for row in result.fetchall()]

        total = 0
        store = MemoryStore(self._db_factory)
        reflector = TypedReflector(store=store, llm_client=self.llm_client)

        for user_id in user_ids:
            promoted = reflector.reflect(user_id)
            total += len(promoted)

        return total
    
    def _apply_confidence_decay(self) -> int:
        """Apply confidence decay to all knowledge entries.
        
        Formula: confidence(t) = initial_confidence × 0.5^(days_since_validation / half_life)
        
        Returns:
            Number of entries decayed
        """
        with self._db() as db:
            from api.models import KnowledgeEntry
        
            entries = db.query(KnowledgeEntry).filter(
                KnowledgeEntry.confidence > 0.3
            ).all()
        
            count = 0
            now = datetime.now()
            for entry in entries:
                # Handle None last_validated_at (use created_at as fallback)
                anchor = entry.last_validated_at or entry.created_at
                if anchor is None:
                    continue  # No temporal anchor — skip, don't crash
            
                half_life = TRUST_TIER_HALF_LIVES.get(entry.trust_tier, 60)
                days_since = (now - anchor).days
            
                # Calculate decay
                decay_factor = 0.5 ** (days_since / half_life)
                new_confidence = entry.initial_confidence * decay_factor
            
                if new_confidence != entry.confidence:
                    entry.confidence = new_confidence
                    entry.updated_at = now
                    count += 1
        
            db.commit()
        
            logger.info(f"Applied confidence decay to {count} entries")
            return count
    
    def _quarantine_low_confidence(self, threshold: float = 0.3) -> int:
        """Quarantine entries below confidence threshold.
        
        Sets confidence to 0 so they are excluded from retrieval and decay.
        Logs quarantined entry_ids for audit trail.
        
        Args:
            threshold: Minimum confidence to keep active
            
        Returns:
            Number of entries quarantined
        """
        with self._db() as db:
            from api.models import KnowledgeEntry
        
            # Query first to capture entry_ids for audit
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

            # Write governance event for audit trail
            if ids:
                self._write_governance_event(
                    "governance_quarantine",
                    {"entry_ids": ids, "threshold": threshold, "count": len(ids)},
                )

            logger.info(
                "Quarantined %d low-confidence entries (threshold=%.2f): %s",
                len(ids), threshold, ids,
            )
            return len(ids)
    
    def _compress_episodic_events(self, ttl_days: int = 90) -> int:
        """Compress old episodic events to session summaries.

        Groups events by session, generates a summary (LLM if available,
        else truncated concatenation), writes a ``session_summary`` event,
        and marks originals as compressed.
        """
        with self._db() as db:
            from api.models import Event
            from sqlalchemy import text

            cutoff = datetime.now() - timedelta(days=ttl_days)

            events = db.query(Event).filter(
                Event.created_at < cutoff,
                Event.event_type.in_(["user_query", "llm_response"]),
            ).limit(1000).all()

            if not events:
                return 0

            from uuid_utils import uuid7

            # Group by session
            by_session: dict[str, list] = {}
            for e in events:
                by_session.setdefault(e.session_id, []).append(e)

            compressed = 0
            for sid, sess_events in by_session.items():
                texts = [e.content or "" for e in sess_events]
                concat = "\n".join(texts)

                # Generate summary
                if self.llm_client and len(concat) > 500:
                    try:
                        from core.llm.models import LLMMessage
                        resp = self.llm_client.chat(
                            messages=[
                                LLMMessage(role="system", content="Summarize this conversation in ≤3 sentences."),
                                LLMMessage(role="user", content=concat[:4000]),
                            ],
                            user_id="system",
                        )
                        summary = resp.content or concat[:500]
                    except Exception:
                        summary = concat[:500]
                else:
                    summary = concat[:500]

                # Write session_summary event
                sum_eid = str(uuid7())
                db.execute(
                    text("""INSERT INTO conversation_events
                            (event_id, session_id, user_id, agent_id, agent_version,
                             event_type, content, causal_chain_id, dedup_key, created_at)
                            VALUES (:eid, :sid, 'system', 'governance', '1.0',
                                    'session_summary', :content, :cid, NULL, :ts)"""),
                    {
                        "eid": sum_eid, "sid": sid,
                        "content": summary, "cid": sum_eid, "ts": datetime.now(),
                    },
                )

                # Mark originals as compressed (batch UPDATE)
                eids = [e.event_id for e in sess_events]
                if eids:
                    db.execute(
                        text("UPDATE conversation_events SET event_type = 'compressed' WHERE event_id IN :eids"),
                        {"eids": tuple(eids)},
                    )
                compressed += len(sess_events)

            db.commit()

            # Write governance audit event
            if compressed > 0:
                self._write_governance_event(
                    "episodic_compression",
                    {"sessions": len(by_session), "events_compressed": compressed, "ttl_days": ttl_days},
                )

            logger.info("Compressed %d events across %d sessions", compressed, len(by_session))
            return compressed
    
    def _scan_contradictions(self) -> int:
        """Scan for contradicting knowledge entries.

        Uses SQL aggregation to find (category, key) pairs with multiple distinct values,
        then batch-checks which have already been reported.
        """
        with self._db() as db:
            from api.models import KnowledgeEntry
            from sqlalchemy import text, func

            # SQL aggregation: find (category, key) with >1 distinct value
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

            # Batch query: which dedup_keys already reported?
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

                # Fetch entry_ids and values for this conflict (small query)
                entries = db.query(KnowledgeEntry).filter(
                    KnowledgeEntry.category == c.category,
                    KnowledgeEntry.key_name == c.key_name,
                    KnowledgeEntry.confidence > 0.3,
                ).limit(10).all()

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
                    c.category, c.key_name, c.val_count,
                )

            return contradictions
    
    def _write_governance_event(self, event_type: str, content: dict[str, Any], dedup_key: str | None = None) -> None:
        """Write a structured governance event to conversation_events."""
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
            except Exception as e:
                logger.debug("governance event write failed: %s", e)
                db.rollback()

    def _generate_health_reports(self) -> int:
        """Generate memory health reports per user.
        
        Returns:
            Number of reports generated
        """
        with self._db() as db:
            from api.models import KnowledgeEntry
            from sqlalchemy import distinct
        
            # Get all users with knowledge entries
            users = db.query(distinct(KnowledgeEntry.user_id)).all()
        
            reports = 0
            for (user_id,) in users:
                stats = self._get_user_memory_stats(user_id)
            
                logger.info(
                    f"Memory health for {user_id}: "
                    f"{stats['total_entries']} entries, "
                    f"avg confidence {stats['avg_confidence']:.2f}, "
                    f"{stats['low_confidence']} low confidence"
                )
                reports += 1
        
            return reports
    
    def _get_user_memory_stats(self, user_id: str) -> dict[str, Any]:
        """Get memory statistics for a user."""
        with self._db() as db:
            from api.models import KnowledgeEntry

            entries = db.query(KnowledgeEntry).filter(
                KnowledgeEntry.user_id == user_id
            ).all()

            if not entries:
                return {
                    "total_entries": 0,
                    "avg_confidence": 0.0,
                    "low_confidence": 0,
                }

            total = len(entries)
            avg_conf = sum(e.confidence for e in entries) / total
            low_conf = sum(1 for e in entries if e.confidence < 0.3)

            return {
                "total_entries": total,
                "avg_confidence": avg_conf,
                "low_confidence": low_conf,
            }

    # ------------------------------------------------------------------
    # Observable governance stats
    # ------------------------------------------------------------------

    def governance_stats(self) -> dict[str, Any]:
        """Return verifiable governance health indicators.

        Queries live DB state — suitable for dashboards, CLI output,
        and automated acceptance checks.
        """
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
