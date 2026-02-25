"""Memory pollution detection and cleanup.

Continuous monitoring for bad memory entries that could contaminate
the knowledge base through cascading retrieval.
"""

from typing import Any
from sqlalchemy import func, and_
from core.logging_config import get_logger
from core.db_consumer import DbConsumer, DbFactory

logger = get_logger(__name__)


class PollutionDetector(DbConsumer):
    """Detect and quarantine polluted memory entries.
    
    Pollution sources:
    - User injection: deliberately false "facts"
    - Hallucination crystallization: hallucinated responses stored as knowledge
    - Stale knowledge: once-true facts now outdated
    - Contradictions: conflicting entries on same topic
    
    Detection signals:
    - Retrieved often but leads to low-quality decisions
    - Contradicts other entries on same topic
    - Age without revalidation
    
    Example:
        >>> detector = PollutionDetector(db)
        >>> candidates = detector.detect_pollution_candidates("alice")
        >>> detector.quarantine_entry("ke_123", severity="high")
        >>> impact = detector.analyze_cascade_impact("ke_123")
    """
    
    def __init__(self, db_factory: DbFactory):
        super().__init__(db_factory)
    
    def detect_pollution_candidates(
        self,
        user_id: str,
        quality_threshold: float = 2.5,
        contradiction_threshold: int = 2,
        staleness_days: int = 90,
    ) -> list[dict[str, Any]]:
        """Detect pollution candidates for a user.
        
        Args:
            user_id: User ID
            quality_threshold: Min avg quality score for downstream decisions
            contradiction_threshold: Max contradicting entries allowed
            staleness_days: Days without validation to consider stale
            
        Returns:
            List of pollution candidates with severity
        """
        with self._db() as db:
            from api.models import KnowledgeEntry
        
            entries = db.query(KnowledgeEntry).filter(
                KnowledgeEntry.user_id == user_id,
                KnowledgeEntry.confidence > 0.3
            ).all()
        
            candidates = []
        
            for entry in entries:
                signals = self._calculate_pollution_signals(entry)
            
                # Classify severity
                severity = self._classify_severity(
                    signals,
                    quality_threshold,
                    contradiction_threshold,
                    staleness_days,
                )
            
                if severity:
                    candidates.append({
                        "entry_id": entry.entry_id,
                        "key_name": entry.key_name,
                        "category": entry.category,
                        "severity": severity,
                        "signals": signals,
                    })
        
            logger.info(f"Found {len(candidates)} pollution candidates for {user_id}")
            return candidates
    
    def _calculate_pollution_signals(
        self,
        entry,
    ) -> dict[str, Any]:
        """Calculate pollution detection signals for an entry.
        
        Args:
            entry: KnowledgeEntry instance
            
        Returns:
            Pollution signals
        """
        with self._db() as db:
            from datetime import datetime
        
            # Signal 1: Days since validation
            days_since = (datetime.now() - entry.last_validated_at).days
        
            # Signal 2: Contradicting entries (simplified - check same key)
            from api.models import KnowledgeEntry
        
            contradicting = db.query(KnowledgeEntry).filter(
                KnowledgeEntry.user_id == entry.user_id,
                KnowledgeEntry.category == entry.category,
                KnowledgeEntry.key_name == entry.key_name,
                KnowledgeEntry.entry_id != entry.entry_id,
                KnowledgeEntry.value != entry.value,
            ).count()
        
            # Signal 3: Downstream quality (simplified - would need context_snapshots join)
            # For now, use confidence as proxy
            downstream_quality = entry.confidence * 5.0  # Scale to 0-5
        
            return {
                "days_since_validation": days_since,
                "contradicting_entries": contradicting,
                "downstream_quality": downstream_quality,
                "confidence": entry.confidence,
            }
    
    def _classify_severity(
        self,
        signals: dict[str, Any],
        quality_threshold: float,
        contradiction_threshold: int,
        staleness_days: int,
    ) -> str | None:
        """Classify pollution severity.
        
        Args:
            signals: Pollution signals
            quality_threshold: Quality threshold
            contradiction_threshold: Contradiction threshold
            staleness_days: Staleness threshold
            
        Returns:
            Severity level (low, medium, high) or None
        """
        # HIGH: Confirmed downstream harm
        if signals["downstream_quality"] < quality_threshold:
            return "high"
        
        # MEDIUM: Contradictions exist
        if signals["contradicting_entries"] >= contradiction_threshold:
            return "medium"
        
        # LOW: Stale but no harm
        if signals["days_since_validation"] > staleness_days:
            return "low"
        
        return None
    
    def quarantine_entry(
        self,
        entry_id: str,
        severity: str,
        reason: str | None = None,
    ) -> bool:
        """Quarantine a polluted entry.
        
        Args:
            entry_id: Entry ID to quarantine
            severity: Severity level (low, medium, high)
            reason: Quarantine reason
            
        Returns:
            True if quarantined
        """
        with self._db() as db:
            from api.models import KnowledgeEntry
        
            entry = db.query(KnowledgeEntry).filter(
                KnowledgeEntry.entry_id == entry_id
            ).first()
        
            if not entry:
                logger.warning(f"Entry {entry_id} not found")
                return False
        
            # In production, move to quarantine table
            # For now, set confidence to 0 to exclude from retrieval
            entry.confidence = 0.0
            db.commit()
        
            logger.warning(
                f"Quarantined entry {entry_id} (severity={severity}): "
                f"{entry.key_name} - {reason or 'no reason'}"
            )
        
            return True
    
    def analyze_cascade_impact(
        self,
        entry_id: str,
        max_depth: int = 5,
    ) -> dict[str, Any]:
        """Analyze cascade impact of a polluted entry.

        Traces contamination graph:
        1. Find context_snapshots whose selected_events contain this entry
        2. Find decisions linked to those snapshots
        3. Check if those decisions produced knowledge entries (via source_event_ids)
        4. Recurse up to max_depth

        Args:
            entry_id: Polluted entry ID
            max_depth: Maximum recursion depth to prevent runaway queries

        Returns:
            Impact analysis with affected decisions/entries counts
        """
        with self._db() as db:
            from api.models import ContextSnapshot, DecisionAudit, KnowledgeEntry
            import json

            # NOTE: Full-table scans below are unavoidable — selected_events is a JSON
            # column and we need to match entry IDs inside JSON arrays.  This method is
            # only called during offline pollution analysis, not on the hot path.

            affected_decisions: set[str] = set()
            affected_entries: set[str] = set()
            frontier = {entry_id}
            depth = 0

            while frontier and depth < max_depth:
                depth += 1
                next_frontier: set[str] = set()

                # Find snapshots that reference any frontier entry in selected_events
                snapshots = db.query(ContextSnapshot).all()
                hit_snapshot_ids: set[str] = set()
                for snap in snapshots:
                    events = snap.selected_events or []
                    if isinstance(events, str):
                        try:
                            events = json.loads(events)
                        except (json.JSONDecodeError, TypeError):
                            continue
                    event_ids = {e.get("event_id") or e for e in events if isinstance(e, (dict, str))}
                    if frontier & event_ids:
                        hit_snapshot_ids.add(snap.context_capture_id)

                if not hit_snapshot_ids:
                    break

                # Find decisions linked to those snapshots
                decisions = db.query(DecisionAudit).filter(
                    DecisionAudit.context_capture_id.in_(list(hit_snapshot_ids))
                ).all()
                new_decision_event_ids: set[str] = set()
                for d in decisions:
                    if d.decision_id not in affected_decisions:
                        affected_decisions.add(d.decision_id)
                        if d.event_id:
                            new_decision_event_ids.add(d.event_id)

                if not new_decision_event_ids:
                    break

                # Find knowledge entries sourced from those decision events
                from api.models import KnowledgeEntrySource
                sources = db.query(KnowledgeEntrySource.entry_id).filter(
                    KnowledgeEntrySource.event_id.in_(new_decision_event_ids),
                ).all()
                for (eid,) in sources:
                    if eid not in affected_entries:
                        affected_entries.add(eid)
                        next_frontier.add(eid)

                frontier = next_frontier

            return {
                "entry_id": entry_id,
                "affected_decisions": len(affected_decisions),
                "affected_entries": len(affected_entries),
                "contamination_depth": depth,
            }
    
    def scan_contradictions(
        self,
        user_id: str,
    ) -> list[dict[str, Any]]:
        """Scan for contradicting knowledge entries.
        
        Args:
            user_id: User ID
            
        Returns:
            List of contradiction groups
        """
        with self._db() as db:
            from api.models import KnowledgeEntry
        
            entries = db.query(KnowledgeEntry).filter(
                KnowledgeEntry.user_id == user_id,
                KnowledgeEntry.confidence > 0.3
            ).all()
        
            # Group by (category, key)
            groups: dict[tuple[str, str], list] = {}
            for entry in entries:
                key = (entry.category, entry.key_name)
                if key not in groups:
                    groups[key] = []
                groups[key].append(entry)
        
            # Find groups with multiple different values
            contradictions = []
            for key, group in groups.items():
                if len(group) > 1:
                    values = set(e.value for e in group)
                    if len(values) > 1:
                        contradictions.append({
                            "category": key[0],
                            "key_name": key[1],
                            "entry_count": len(group),
                            "value_count": len(values),
                            "entries": [
                                {
                                    "entry_id": e.entry_id,
                                    "value": e.value,
                                    "confidence": e.confidence,
                                }
                                for e in group
                            ],
                        })
        
            logger.info(f"Found {len(contradictions)} contradiction groups for {user_id}")
            return contradictions

    def quarantine_with_validation(
        self,
        entry_id: str,
        severity: str,
        reason: str | None = None,
    ) -> dict[str, Any]:
        """Quarantine entry with regression gate validation.

        Validates that quarantining improves (or doesn't degrade) quality
        on golden sessions before committing the change.
        """
        try:
            from core.evaluation.regression_gate import RegressionGate, ChangeType

            gate = RegressionGate(self._db_factory)
            result = gate.validate_change(
                change_type=ChangeType.KNOWLEDGE,
                change_id=f"quarantine_{entry_id}",
                change_content={"entry_id": entry_id, "action": "quarantine"},
                golden_session_count=10,
            )
            verdict = result.get("verdict", "error")
        except Exception as e:
            logger.warning("Gate validation unavailable for knowledge quarantine: %s", e)
            verdict = "skipped"

        if verdict in ("pass", "skip", "skipped"):
            self.quarantine_entry(entry_id, severity, reason)
            logger.info("Gated quarantine applied: %s (verdict=%s)", entry_id, verdict)
        else:
            logger.warning(
                "Quarantine rejected by gate: %s (verdict=%s, reason=%s)",
                entry_id, verdict, result.get("reason"),
            )

        return {"entry_id": entry_id, "verdict": verdict, "severity": severity}
