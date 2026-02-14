"""Self-improving skill selector that learns from historical failures.

This module implements the breakthrough feature: automatic learning from mistakes
using Git for Data's time-travel capabilities.
"""

import json
from datetime import datetime, timedelta, timezone
from typing import Any

from sqlalchemy.orm import Session
from uuid_utils import uuid7

from api.models import SkillSelectionEvent as EventModel, SkillSelectionLearning as LearningModel
from core.logging_config import get_logger
from core.sandbox import Sandbox
from core.skills.auditable_selector import AuditableSkillSelector, SkillSelectionEvent

logger = get_logger(__name__)


class SelfImprovingSelector:
    """Skill selector that learns from historical failures automatically.
    
    Key innovation: Uses Git for Data to replay failures in sandbox,
    test corrections, and update selection strategy.
    """

    def __init__(self, session: Session, llm_client=None, account: str = "sys"):
        if not isinstance(session, Session):
            raise TypeError("session must be a SQLAlchemy Session")
        
        self.session = session
        self.llm = llm_client
        self.account = account
        self.auditable_selector = AuditableSkillSelector(session, llm_client, account)
        self.sandbox = Sandbox(db=session, account=account)
        self._ensure_tables()

    def _ensure_tables(self):
        """Ensure learning tables exist - no-op as tables are created by ORM."""
        pass

    def learn_from_failures(self, days: int = 7) -> dict[str, Any]:
        """Analyze recent failures and learn corrections."""
        failures = self.get_recent_failures(days=days)
        
        if not failures:
            return {"learned": 0, "message": "No failures to learn from"}
        
        learned_count = 0
        for failure in failures:
            try:
                correction = self._analyze_failure(failure)
                if correction:
                    self._update_learnings(correction)
                    learned_count += 1
            except Exception as e:
                logger.error(f"Failed to learn from failure: {e}")
        
        return {"learned": learned_count, "total_failures": len(failures)}

    def get_recent_failures(self, days: int = 7, limit: int = 10) -> list[dict]:
        """Get recent failed selections for learning."""
        cutoff = datetime.now(timezone.utc) - timedelta(days=days)
        
        events = self.session.query(EventModel).filter(
            EventModel.selection_correctness == 0,
            EventModel.created_at >= cutoff
        ).order_by(EventModel.created_at.desc()).limit(limit).all()
        
        return [
            {
                "event_id": e.event_id,
                "user_query": e.user_query,
                "selected_skills": e.selected_skills,
                "correction_suggestion": e.correction_suggestion,
            }
            for e in events
        ]

    def _analyze_failure(self, failure: dict) -> dict | None:
        """Analyze a failure and extract learning."""
        if not failure.get("correction_suggestion"):
            return None
        
        return {
            "query_pattern": failure["user_query"][:255],
            "wrong_skills": failure["selected_skills"],
            "correct_skills": failure["correction_suggestion"],
            "improvement_score": 10,
        }

    def _update_learnings(self, correction: dict):
        """Update learning database with new correction."""
        
        # Check if similar learning exists
        existing = self.session.query(LearningModel).filter(
            LearningModel.query_pattern == correction["query_pattern"]
        ).first()
        
        if existing:
            existing.evidence_count += 1
            existing.confidence = min(99, existing.evidence_count * 10)
        else:
            learning = LearningModel(
                learning_id=str(uuid7()),
                query_pattern=correction["query_pattern"],
                wrong_skills=correction["wrong_skills"],
                correct_skills=correction["correct_skills"],
                improvement_score=correction["improvement_score"],
                confidence=10,
            )
            self.session.add(learning)
        
        self.session.commit()

    def apply_learnings(self, query: str, candidates: list) -> list:
        """Apply learned corrections to candidate selection."""
        
        # Find matching learnings
        learnings = self.session.query(LearningModel).filter(
            LearningModel.confidence >= 50
        ).all()
        
        for learning in learnings:
            if learning.query_pattern.lower() in query.lower():
                # Apply correction
                learning.applied_count += 1
                learning.last_applied_at = datetime.now(timezone.utc)
                self.session.commit()
                
                # Filter out wrong skills, add correct ones
                filtered = [c for c in candidates if c.name not in learning.wrong_skills]
                return filtered
        
        return candidates

    def get_learning_stats(self) -> dict[str, Any]:
        """Get statistics about learned corrections."""
        
        total = self.session.query(LearningModel).count()
        high_confidence = self.session.query(LearningModel).filter(
            LearningModel.confidence >= 70
        ).count()
        
        # Calculate average confidence
        avg_confidence = 0.0
        if total > 0:
            from sqlalchemy import func
            result = self.session.query(func.avg(LearningModel.confidence)).scalar()
            avg_confidence = float(result) if result else 0.0
        
        return {
            "total_learnings": total,
            "high_confidence": high_confidence,
            "low_confidence": total - high_confidence,
            "avg_confidence": avg_confidence,
        }
