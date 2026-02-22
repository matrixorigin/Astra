"""Knowledge regression detection — P2 Data Versioning.

Identify past decisions invalidated by knowledge updates via time-travel queries.
"""

from __future__ import annotations

import json
from dataclasses import dataclass, field
from datetime import datetime
from enum import Enum
from typing import Optional

from sqlalchemy import text
from sqlalchemy.orm import Session


class RegressionType(str, Enum):
    """Type of knowledge regression."""
    SKILL_DEPRECATED = "skill_deprecated"
    SKILL_UPDATED = "skill_updated"
    KNOWLEDGE_CHANGED = "knowledge_changed"
    PERFORMANCE_DEGRADED = "performance_degraded"


@dataclass
class RegressionSignal:
    """Signal indicating potential regression."""
    signal_id: str
    regression_type: RegressionType
    affected_skill: str
    affected_sessions: int
    affected_decisions: int
    confidence: float  # 0-1
    detected_at: datetime
    metadata: dict = field(default_factory=dict)


@dataclass
class RegressionReport:
    """Report of detected regressions."""
    report_id: str
    signals: list[RegressionSignal]
    total_affected_sessions: int
    total_affected_decisions: int
    generated_at: datetime


class KnowledgeRegression:
    """Detect knowledge regressions via time-travel queries and historical analysis."""
    
    def __init__(self, db: Session, source_db: str = "dev_agent"):
        """Initialize regression detector.
        
        Args:
            db: Database session
            source_db: Source database for queries
        """
        self.db = db
        self.source_db = source_db
    
    def detect_skill_deprecation(
        self,
        skill_name: str,
        deprecated_at: datetime,
    ) -> RegressionSignal:
        """Detect sessions using deprecated skill before deprecation.
        
        Args:
            skill_name: Skill name
            deprecated_at: When skill was deprecated
            
        Returns:
            RegressionSignal with affected session/decision count
        """
        # Query sessions that used skill before deprecation
        result = self.db.execute(text(f"""
            SELECT COUNT(DISTINCT session_id) as session_count,
                   COUNT(DISTINCT event_id) as decision_count
            FROM {self.source_db}.conversation_events
            WHERE event_type = 'skill_selection'
            AND JSON_EXTRACT(metadata, '$.skill_name') = :skill_name
            AND created_at < :deprecated_at
        """), {"skill_name": skill_name, "deprecated_at": deprecated_at}).fetchone()
        
        session_count = result[0] if result and result[0] else 0
        decision_count = result[1] if result and result[1] else 0
        
        return RegressionSignal(
            signal_id=f"deprecation_{skill_name}_{int(deprecated_at.timestamp())}",
            regression_type=RegressionType.SKILL_DEPRECATED,
            affected_skill=skill_name,
            affected_sessions=session_count,
            affected_decisions=decision_count,
            confidence=1.0 if session_count > 0 else 0.0,
            detected_at=datetime.utcnow(),
            metadata={"deprecated_at": deprecated_at.isoformat()},
        )
    
    def detect_skill_update_regression(
        self,
        skill_name: str,
        old_version: str,
        new_version: str,
    ) -> RegressionSignal:
        """Detect performance regression after skill update via quality score comparison.
        
        Args:
            skill_name: Skill name
            old_version: Previous version
            new_version: New version
            
        Returns:
            RegressionSignal with regression metrics
        """
        # Query quality scores before/after update
        before = self.db.execute(text(f"""
            SELECT AVG(CAST(JSON_EXTRACT(metadata, '$.quality_score') AS DECIMAL(3,2))) as avg_quality,
                   COUNT(*) as sample_count,
                   STDDEV(CAST(JSON_EXTRACT(metadata, '$.quality_score') AS DECIMAL(3,2))) as stddev
            FROM {self.source_db}.conversation_events
            WHERE event_type = 'llm_response'
            AND JSON_EXTRACT(metadata, '$.skill_name') = :skill_name
            AND JSON_EXTRACT(metadata, '$.skill_version') = :old_version
        """), {"skill_name": skill_name, "old_version": old_version}).fetchone()
        
        after = self.db.execute(text(f"""
            SELECT AVG(CAST(JSON_EXTRACT(metadata, '$.quality_score') AS DECIMAL(3,2))) as avg_quality,
                   COUNT(*) as sample_count,
                   STDDEV(CAST(JSON_EXTRACT(metadata, '$.quality_score') AS DECIMAL(3,2))) as stddev
            FROM {self.source_db}.conversation_events
            WHERE event_type = 'llm_response'
            AND JSON_EXTRACT(metadata, '$.skill_name') = :skill_name
            AND JSON_EXTRACT(metadata, '$.skill_version') = :new_version
        """), {"skill_name": skill_name, "new_version": new_version}).fetchone()
        
        before_quality = float(before[0]) if before and before[0] else 1.0
        before_count = int(before[1]) if before and before[1] else 0
        after_quality = float(after[0]) if after and after[0] else 1.0
        after_count = int(after[1]) if after and after[1] else 0
        
        # Detect regression if quality dropped significantly
        quality_drop = before_quality - after_quality
        regression_detected = quality_drop > 0.05  # 5% drop threshold
        confidence = min(abs(quality_drop), 1.0) if regression_detected else 0.0
        
        return RegressionSignal(
            signal_id=f"update_{skill_name}_{old_version}_{new_version}",
            regression_type=RegressionType.SKILL_UPDATED,
            affected_skill=skill_name,
            affected_sessions=before_count + after_count,
            affected_decisions=before_count + after_count,
            confidence=confidence,
            detected_at=datetime.utcnow(),
            metadata={
                "old_version": old_version,
                "new_version": new_version,
                "before_quality": before_quality,
                "after_quality": after_quality,
                "quality_drop": quality_drop,
                "before_sample_count": before_count,
                "after_sample_count": after_count,
            },
        )
    
    def detect_knowledge_change_impact(
        self,
        knowledge_domain: str,
        changed_at: datetime,
    ) -> RegressionSignal:
        """Detect sessions affected by knowledge change via context snapshots.
        
        Args:
            knowledge_domain: Domain of knowledge (e.g., "matrixone_sql")
            changed_at: When knowledge changed
            
        Returns:
            RegressionSignal with affected session count
        """
        # Query sessions that referenced domain before change
        result = self.db.execute(text(f"""
            SELECT COUNT(DISTINCT session_id) as session_count,
                   COUNT(DISTINCT event_id) as decision_count
            FROM {self.source_db}.conversation_events
            WHERE event_type = 'context_snapshot'
            AND JSON_EXTRACT(metadata, '$.knowledge_domain') = :domain
            AND created_at < :changed_at
        """), {"domain": knowledge_domain, "changed_at": changed_at}).fetchone()
        
        session_count = result[0] if result and result[0] else 0
        decision_count = result[1] if result and result[1] else 0
        
        return RegressionSignal(
            signal_id=f"knowledge_{knowledge_domain}_{int(changed_at.timestamp())}",
            regression_type=RegressionType.KNOWLEDGE_CHANGED,
            affected_skill=knowledge_domain,
            affected_sessions=session_count,
            affected_decisions=decision_count,
            confidence=0.7 if session_count > 0 else 0.0,
            detected_at=datetime.utcnow(),
            metadata={"changed_at": changed_at.isoformat()},
        )
    
    def generate_regression_report(
        self,
        start_date: datetime,
        end_date: datetime,
    ) -> RegressionReport:
        """Generate comprehensive regression report for time period.
        
        Args:
            start_date: Report start date
            end_date: Report end date
            
        Returns:
            RegressionReport with all detected signals
        """
        signals = []
        
        # Query regression tracking table (if exists)
        try:
            rows = self.db.execute(text(f"""
                SELECT signal_id, regression_type, affected_skill, affected_sessions, 
                       affected_decisions, confidence, detected_at, metadata
                FROM {self.source_db}.regression_signals
                WHERE detected_at BETWEEN :start_date AND :end_date
                ORDER BY confidence DESC
            """), {"start_date": start_date, "end_date": end_date}).fetchall()
            
            for row in rows:
                signal_id, reg_type, skill, sessions, decisions, conf, detected, meta = row
                signals.append(RegressionSignal(
                    signal_id=signal_id,
                    regression_type=RegressionType(reg_type),
                    affected_skill=skill,
                    affected_sessions=sessions,
                    affected_decisions=decisions,
                    confidence=conf,
                    detected_at=detected,
                    metadata=json.loads(meta) if isinstance(meta, str) else meta or {},
                ))
        except Exception:
            pass  # Table may not exist
        
        total_affected_sessions = sum(s.affected_sessions for s in signals)
        total_affected_decisions = sum(s.affected_decisions for s in signals)
        
        return RegressionReport(
            report_id=f"report_{int(start_date.timestamp())}_{int(end_date.timestamp())}",
            signals=signals,
            total_affected_sessions=total_affected_sessions,
            total_affected_decisions=total_affected_decisions,
            generated_at=datetime.utcnow(),
        )
    
    def get_affected_sessions(
        self,
        signal_id: str,
        limit: int = 100,
    ) -> list[dict]:
        """Get sessions affected by regression signal with full context.
        
        Args:
            signal_id: Signal ID
            limit: Max sessions to return
            
        Returns:
            List of affected session info dicts
        """
        # Parse signal_id to determine query
        if signal_id.startswith("deprecation_"):
            parts = signal_id.split("_")
            skill_name = "_".join(parts[1:-1])
            
            rows = self.db.execute(text(f"""
                SELECT DISTINCT 
                    e.session_id,
                    COUNT(e.event_id) as event_count,
                    MIN(e.created_at) as first_use,
                    MAX(e.created_at) as last_use
                FROM {self.source_db}.conversation_events e
                WHERE e.event_type = 'skill_selection'
                AND JSON_EXTRACT(e.metadata, '$.skill_name') = :skill_name
                GROUP BY e.session_id
                LIMIT :limit
            """), {"skill_name": skill_name, "limit": limit}).fetchall()
            
            return [
                {
                    "session_id": row[0],
                    "event_count": row[1],
                    "first_use": row[2].isoformat() if row[2] else None,
                    "last_use": row[3].isoformat() if row[3] else None,
                }
                for row in rows
            ]
        
        return []
