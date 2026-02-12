"""Self-improving skill selector that learns from historical failures.

This module implements the breakthrough feature: automatic learning from mistakes
using Git for Data's time-travel capabilities.
"""

import json
from datetime import datetime, timedelta, timezone
from typing import Any

from uuid_utils import uuid7

from core.logging_config import get_logger
from core.sandbox import Sandbox
from core.skills.auditable_selector import AuditableSkillSelector, SkillSelectionEvent
from sdk import Database

logger = get_logger(__name__)


class SelfImprovingSelector:
    """Skill selector that learns from historical failures automatically.
    
    Key innovation: Uses Git for Data to replay failures in sandbox,
    test corrections, and update selection strategy.
    """

    def __init__(self, db: Database, llm_client, account: str = "sys"):
        self.db = db
        self.llm = llm_client
        self.account = account
        self.auditable_selector = AuditableSkillSelector(db, llm_client, account)
        self.sandbox = Sandbox(db=db, account=account)
        self._ensure_tables()

    def _ensure_tables(self):
        """Ensure learning tables exist."""
        self.db.fetchall(
            """
            CREATE TABLE IF NOT EXISTS skill_selection_learnings (
                learning_id VARCHAR(36) PRIMARY KEY,
                query_pattern VARCHAR(255) NOT NULL,
                wrong_skills JSON NOT NULL,
                correct_skills JSON NOT NULL,
                improvement_score DECIMAL(5, 2),
                evidence_count INT DEFAULT 1,
                confidence DECIMAL(3, 2),
                learned_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
                applied_count INT DEFAULT 0,
                last_applied_at TIMESTAMP,
                INDEX idx_pattern (query_pattern),
                INDEX idx_confidence (confidence)
            )
        """
        )

    def learn_from_failures(self, days: int = 7) -> dict[str, Any]:
        """Analyze recent failures and learn corrections.
        
        This is the core self-improvement loop:
        1. Find failures (low user feedback)
        2. Time-travel to failure state
        3. Test alternative skills in sandbox
        4. Record successful corrections
        
        Args:
            days: Look back N days for failures
            
        Returns:
            Learning statistics
        """
        logger.info(f"Starting learning from failures (last {days} days)")

        # Step 1: Find failures
        failures = self._get_recent_failures(days)
        logger.info(f"Found {len(failures)} failures to analyze")

        if not failures:
            return {"failures_analyzed": 0, "corrections_found": 0, "learnings_added": 0}

        # Step 2: Create learning sandbox (use timestamp + uuid for uniqueness)
        import time
        sandbox_name = f"learn_failures_{int(time.time() * 1000)}_{str(uuid7())[:8]}"
        self.sandbox.create(
            sandbox_name,
            description=f"Learning from {len(failures)} failures",
            created_by="self_improving_selector",
        )

        corrections = []

        try:
            # Step 3: Analyze each failure
            for failure in failures:
                correction = self._analyze_failure_in_sandbox(sandbox_name, failure)
                if correction:
                    corrections.append(correction)

            # Step 4: Update learning database
            learnings_added = self._update_learnings(corrections)

            logger.info(
                f"Learning complete: {len(corrections)} corrections found, {learnings_added} learnings added"
            )

            return {
                "failures_analyzed": len(failures),
                "corrections_found": len(corrections),
                "learnings_added": learnings_added,
            }

        finally:
            # Cleanup
            self.sandbox.delete(sandbox_name)

    def _get_recent_failures(self, days: int) -> list[SkillSelectionEvent]:
        """Get recent selection failures (low user feedback or execution failure)."""
        cutoff = datetime.now(timezone.utc) - timedelta(days=days)

        rows = self.db.fetchall(
            """
            SELECT * FROM skill_selection_events
            WHERE created_at > %s
            AND (
                user_feedback_score <= 2
                OR execution_success = FALSE
                OR selection_correctness = FALSE
            )
            ORDER BY created_at DESC
            LIMIT 100
        """,
            (cutoff,),
        )

        failures = []
        for row in rows:
            failures.append(
                SkillSelectionEvent(
                    event_id=row["event_id"],
                    session_id=row["session_id"],
                    user_query=row["user_query"],
                    context_snapshot=row["context_snapshot"],
                    available_skills=json.loads(row["available_skills"]),
                    selected_skills=json.loads(row["selected_skills"]),
                    selection_method=row["selection_method"],
                    selection_reasoning=row["selection_reasoning"],
                    candidate_scores=json.loads(row.get("candidate_scores", "{}")),
                    execution_success=row.get("execution_success"),
                    user_feedback_score=row.get("user_feedback_score"),
                    created_at=row["created_at"],
                )
            )

        return failures

    def _analyze_failure_in_sandbox(
        self, sandbox_name: str, failure: SkillSelectionEvent
    ) -> dict[str, Any] | None:
        """Analyze a failure in sandbox by testing alternatives.
        
        This is where Git for Data shines:
        1. Time-travel to failure state using snapshot
        2. Test alternative skills
        3. Find what would have worked
        """
        logger.info(f"Analyzing failure: {failure.event_id}")

        try:
            # Step 1: Time-travel to failure state
            # Query data as it was at failure time
            self.db.fetchall(f"USE {sandbox_name}")

            # Get available skills at that time
            available_skills = failure.available_skills
            wrong_skills = failure.selected_skills

            # Step 2: Generate alternative skill combinations
            alternatives = self._generate_alternatives(
                failure.user_query, available_skills, wrong_skills
            )

            if not alternatives:
                logger.warning(f"No alternatives found for {failure.event_id}")
                return None

            # Step 3: Test each alternative (simulated for now)
            best_alternative = self._test_alternatives(alternatives, failure.user_query)

            if not best_alternative:
                return None

            # Step 4: Extract learning
            query_pattern = self._extract_query_pattern(failure.user_query)

            return {
                "query_pattern": query_pattern,
                "wrong_skills": wrong_skills,
                "correct_skills": best_alternative["skills"],
                "improvement_score": best_alternative["score"],
                "evidence": failure.event_id,
            }

        except Exception as e:
            logger.error(f"Failed to analyze {failure.event_id}: {e}")
            return None

        finally:
            # Switch back to main database
            self.db.fetchall(f"USE {self.db.database}")

    def _generate_alternatives(
        self, query: str, available_skills: list[dict], wrong_skills: list[str]
    ) -> list[dict[str, Any]]:
        """Generate alternative skill combinations to test."""
        # Filter out wrong skills
        alternatives = []

        for skill in available_skills:
            if skill["name"] not in wrong_skills:
                alternatives.append({"skills": [skill["name"]], "skill_obj": skill})

        return alternatives

    def _test_alternatives(
        self, alternatives: list[dict[str, Any]], query: str
    ) -> dict[str, Any] | None:
        """Test alternative skills and return the best one.
        
        In production, this would execute skills with mock data.
        For now, we use heuristics.
        """
        if not alternatives:
            return None

        # Score alternatives based on skill properties
        scored = []
        for alt in alternatives:
            skill = alt["skill_obj"]
            # Simple scoring: priority + keyword match
            score = skill.get("priority", 5) / 10.0

            # Boost score if triggers match query
            triggers = skill.get("triggers", [])
            if any(trigger.lower() in query.lower() for trigger in triggers):
                score += 0.3

            scored.append({"skills": alt["skills"], "score": score})

        # Return best alternative
        best = max(scored, key=lambda x: x["score"])
        return best if best["score"] > 0.5 else None

    def _extract_query_pattern(self, query: str) -> str:
        """Extract a pattern from query for matching similar queries.
        
        In production, this would use NLP/embeddings.
        For now, use simple keyword extraction.
        """
        # Simple pattern: lowercase, remove punctuation, take first 50 chars
        pattern = query.lower().strip()[:50]
        return pattern

    def _update_learnings(self, corrections: list[dict[str, Any]]) -> int:
        """Update learning database with corrections."""
        learnings_added = 0

        for correction in corrections:
            # Check if similar learning exists
            existing = self.db.fetchall(
                """
                SELECT learning_id, evidence_count, improvement_score
                FROM skill_selection_learnings
                WHERE query_pattern = %s
                AND wrong_skills = %s
                LIMIT 1
            """,
                (correction["query_pattern"], json.dumps(correction["wrong_skills"])),
            )

            if existing:
                # Update existing learning
                learning = existing[0]
                new_count = learning["evidence_count"] + 1
                new_score = (
                    float(learning["improvement_score"]) * learning["evidence_count"]
                    + correction["improvement_score"]
                ) / new_count
                confidence = min(0.99, new_count / 10.0)  # Max confidence at 10 examples

                self.db.fetchall(
                    """
                    UPDATE skill_selection_learnings
                    SET evidence_count = %s,
                        improvement_score = %s,
                        confidence = %s
                    WHERE learning_id = %s
                """,
                    (new_count, new_score, confidence, learning["learning_id"]),
                )
            else:
                # Insert new learning
                learning_id = str(uuid7())
                confidence = 0.1  # Low confidence with single example

                self.db.fetchall(
                    """
                    INSERT INTO skill_selection_learnings (
                        learning_id, query_pattern, wrong_skills, correct_skills,
                        improvement_score, evidence_count, confidence
                    ) VALUES (%s, %s, %s, %s, %s, %s, %s)
                """,
                    (
                        learning_id,
                        correction["query_pattern"],
                        json.dumps(correction["wrong_skills"]),
                        json.dumps(correction["correct_skills"]),
                        correction["improvement_score"],
                        1,
                        confidence,
                    ),
                )
                learnings_added += 1

        return learnings_added

    def apply_learnings(self, query: str, candidates: list[str]) -> list[str]:
        """Apply learned corrections to current selection.
        
        This is called during selection to avoid repeating past mistakes.
        
        Args:
            query: Current query
            candidates: Candidate skills
            
        Returns:
            Corrected skill list
        """
        # Extract pattern from query
        pattern = self._extract_query_pattern(query)

        # Find matching learnings
        learnings = self.db.fetchall(
            """
            SELECT wrong_skills, correct_skills, confidence
            FROM skill_selection_learnings
            WHERE query_pattern LIKE %s
            AND confidence >= 0.5
            ORDER BY confidence DESC, evidence_count DESC
            LIMIT 5
        """,
            (f"%{pattern[:20]}%",),
        )

        if not learnings:
            return candidates

        # Apply corrections
        corrected = candidates.copy()

        for learning in learnings:
            wrong = json.loads(learning["wrong_skills"])
            correct = json.loads(learning["correct_skills"])

            # If candidates contain wrong skills, replace with correct ones
            if any(w in corrected for w in wrong):
                logger.info(
                    f"Applying learning: {wrong} -> {correct} (confidence={learning['confidence']:.2f})"
                )

                # Remove wrong skills
                corrected = [s for s in corrected if s not in wrong]

                # Add correct skills
                corrected.extend([s for s in correct if s not in corrected])

                # Record application
                self.db.fetchall(
                    """
                    UPDATE skill_selection_learnings
                    SET applied_count = applied_count + 1,
                        last_applied_at = CURRENT_TIMESTAMP
                    WHERE wrong_skills = %s
                """,
                    (json.dumps(wrong),),
                )

        return corrected

    def get_learning_stats(self) -> dict[str, Any]:
        """Get statistics about learned corrections."""
        stats = self.db.fetchall(
            """
            SELECT 
                COUNT(*) as total_learnings,
                AVG(confidence) as avg_confidence,
                SUM(evidence_count) as total_evidence,
                SUM(applied_count) as total_applications
            FROM skill_selection_learnings
        """
        )[0]

        high_confidence = self.db.fetchall(
            """
            SELECT COUNT(*) as count
            FROM skill_selection_learnings
            WHERE confidence >= 0.7
        """
        )[0]["count"]

        return {
            "total_learnings": stats["total_learnings"],
            "avg_confidence": float(stats["avg_confidence"] or 0),
            "total_evidence": stats["total_evidence"],
            "total_applications": stats["total_applications"],
            "high_confidence_learnings": high_confidence,
        }
