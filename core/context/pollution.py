"""Memory pollution detection and cleanup.

Continuous monitoring for bad memory entries that could contaminate
the knowledge base through cascading retrieval.
"""

from typing import Any
from sqlalchemy.orm import Session
from sqlalchemy import func, and_
from core.logging_config import get_logger

logger = get_logger(__name__)


class PollutionDetector:
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
    
    def __init__(self, db: Session):
        self.db = db
    
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
        from api.models import KnowledgeEntry
        
        entries = self.db.query(KnowledgeEntry).filter(
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
        from datetime import datetime
        
        # Signal 1: Days since validation
        days_since = (datetime.now() - entry.last_validated_at).days
        
        # Signal 2: Contradicting entries (simplified - check same key)
        from api.models import KnowledgeEntry
        
        contradicting = self.db.query(KnowledgeEntry).filter(
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
        from api.models import KnowledgeEntry
        
        entry = self.db.query(KnowledgeEntry).filter(
            KnowledgeEntry.entry_id == entry_id
        ).first()
        
        if not entry:
            logger.warning(f"Entry {entry_id} not found")
            return False
        
        # In production, move to quarantine table
        # For now, set confidence to 0 to exclude from retrieval
        entry.confidence = 0.0
        self.db.commit()
        
        logger.warning(
            f"Quarantined entry {entry_id} (severity={severity}): "
            f"{entry.key_name} - {reason or 'no reason'}"
        )
        
        return True
    
    def analyze_cascade_impact(
        self,
        entry_id: str,
    ) -> dict[str, Any]:
        """Analyze cascade impact of a polluted entry.
        
        NOTE: This is a placeholder implementation. Full cascade analysis requires
        parsing context_snapshots JSON to find which decisions used this entry.
        
        Traces contamination graph:
        1. Find decisions that used this entry (via context_snapshots)
        2. Check if those decisions became memory entries
        3. Recursively trace contamination chain
        
        Args:
            entry_id: Polluted entry ID
            
        Returns:
            Impact analysis with affected events/entries counts
        """
        from api.models import Event, KnowledgeEntry
        import json
        
        # TODO: Implement full cascade analysis by parsing context_snapshots
        # For now, return placeholder counts
        logger.warning(
            f"Cascade impact analysis for {entry_id} is placeholder - "
            "requires context_snapshots JSON parsing"
        )
        
        return {
            "entry_id": entry_id,
            "affected_events": 0,
            "affected_entries": 0,
            "contamination_depth": 0,
            "note": "Placeholder implementation - requires context_snapshots parsing",
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
        from api.models import KnowledgeEntry
        
        entries = self.db.query(KnowledgeEntry).filter(
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
