"""Cross-model consistency verification.

Design ref: agents-and-orchestration.md §10 "Cross-Model Consistency"

Verifies that outputs from different models are consistent.
Builds compatibility matrix for failover decisions.

Distributed-safe: all state in DB, no in-memory caches.
"""

from __future__ import annotations

import json
import logging
from dataclasses import dataclass
from enum import Enum
from typing import Any

from sqlalchemy import text
from core.db_consumer import DbConsumer, DbFactory

logger = logging.getLogger(__name__)


class ToleranceClass(str, Enum):
    """Tolerance for output variation."""

    STRICT = "strict"  # Structural identity required
    SEMANTIC = "semantic"  # Same conclusion, different wording OK
    RELAXED = "relaxed"  # Different approaches OK if quality maintained


@dataclass
class SkillConsistencyPolicy:
    """Consistency policy for a skill."""

    skill_id: str
    tolerance: ToleranceClass
    verification_model: str | None = None
    max_retries: int = 2
    reference_output: str | None = None


@dataclass
class ConsistencyCheck:
    """Result of a consistency check."""

    passed: bool
    reason: str | None = None
    score: float = 1.0  # 0-1, higher is more consistent


class ConsistencyVerifier(DbConsumer):
    """Verify cross-model consistency.

    Distributed-safe: all state in DB.
    """

    def __init__(self, db_factory: DbFactory, llm_client=None, embedding_service=None) -> None:
        super().__init__(db_factory)
        self.llm_client = llm_client
        self.embedding_service = embedding_service

    def check_structural(self, output: Any, expected_schema: dict[str, Any]) -> ConsistencyCheck:
        """Check if output matches expected schema.

        Args:
            output: Model output
            expected_schema: Expected structure

        Returns:
            ConsistencyCheck result
        """
        if not isinstance(output, dict):
            return ConsistencyCheck(
                passed=False,
                reason="Output is not a dict",
            )

        # Check required fields
        required = expected_schema.get("required", [])
        for field in required:
            if field not in output:
                return ConsistencyCheck(
                    passed=False,
                    reason=f"Missing required field: {field}",
                )

        # Check field types
        properties = expected_schema.get("properties", {})
        for field, value in output.items():
            if field in properties:
                expected_type = properties[field].get("type")
                if expected_type and not self._type_matches(value, expected_type):
                    return ConsistencyCheck(
                        passed=False,
                        reason=f"Field {field} has wrong type",
                    )

        return ConsistencyCheck(passed=True)

    def check_semantic(
        self,
        output: str,
        reference: str | None = None,
        prior_outputs: list[str] | None = None,
    ) -> ConsistencyCheck:
        """Check semantic consistency.

        Args:
            output: Current output
            reference: Reference output to compare against
            prior_outputs: Prior outputs in session (for contradiction detection)

        Returns:
            ConsistencyCheck result
        """
        if not self.llm_client:
            # No LLM available, skip semantic check
            return ConsistencyCheck(passed=True)

        # Simple heuristic: check for contradictions with prior outputs
        if prior_outputs:
            for prior in prior_outputs:
                if self._contradicts(output, prior):
                    return ConsistencyCheck(
                        passed=False,
                        reason="Contradicts prior output",
                        score=0.3,
                    )

        # If reference available, check semantic equivalence
        if reference:
            similarity = self._semantic_similarity(output, reference)
            if similarity < 0.7:
                return ConsistencyCheck(
                    passed=False,
                    reason="Low semantic similarity to reference",
                    score=similarity,
                )

        return ConsistencyCheck(passed=True, score=0.9)

    def record_compatibility(
        self,
        task_type: str,
        model_a: str,
        model_b: str,
        compatible: bool,
        score: float,
    ) -> None:
        """Record model compatibility for a task type.

        Args:
            task_type: Type of task
            model_a: Source model
            model_b: Target model (fallback)
            compatible: Whether they're compatible
            score: Compatibility score (0-1)
        """
        with self._db() as db:
            from uuid_utils import uuid7

            db.execute(
                text(
                    "INSERT INTO model_compatibility "
                    "(compat_id, task_type, model_a, model_b, compatible, score, recorded_at) "
                    "VALUES (:id, :task_type, :model_a, :model_b, :compatible, :score, NOW())"
                ),
                {
                    "id": str(uuid7()),
                    "task_type": task_type,
                    "model_a": model_a,
                    "model_b": model_b,
                    "compatible": compatible,
                    "score": score,
                },
            )
            db.commit()
            logger.info(f"Compatibility recorded: {model_a} → {model_b} for {task_type}: {score}")

    def get_compatibility_score(
        self,
        task_type: str,
        model_a: str,
        model_b: str,
    ) -> float:
        """Get compatibility score between two models for a task type.

        Args:
            task_type: Type of task
            model_a: Source model
            model_b: Target model

        Returns:
            Compatibility score (0-1), or 0.5 if unknown
        """
        with self._db() as db:
            row = db.execute(
                text(
                    "SELECT score FROM model_compatibility "
                    "WHERE task_type = :task_type AND model_a = :model_a AND model_b = :model_b "
                    "ORDER BY recorded_at DESC LIMIT 1"
                ),
                {"task_type": task_type, "model_a": model_a, "model_b": model_b},
            ).fetchone()

            if row:
                return float(row[0])
            return 0.5  # Unknown, assume neutral

    def should_failover(
        self,
        task_type: str,
        primary_model: str,
        fallback_model: str,
    ) -> bool:
        """Decide whether to failover to a different model.

        Args:
            task_type: Type of task
            primary_model: Primary model
            fallback_model: Fallback model

        Returns:
            True if failover is safe
        """
        score = self.get_compatibility_score(task_type, primary_model, fallback_model)
        # Failover if compatibility > 70%
        return score > 0.7

    def _type_matches(self, value: Any, expected_type: str) -> bool:
        """Check if value matches expected type."""
        type_map = {
            "string": str,
            "number": (int, float),
            "integer": int,
            "boolean": bool,
            "array": list,
            "object": dict,
        }
        expected = type_map.get(expected_type)
        if expected is None:
            return True
        return isinstance(value, expected)

    def _contradicts(self, output: str, prior: str) -> bool:
        """Detect contradictions between output and prior.

        Uses LLM if available, otherwise falls back to semantic similarity.
        The old negation-word heuristic had too many false positives.
        """
        # If LLM available, use NLI-style contradiction detection
        if self.llm_client:
            return self._llm_contradiction_check(output, prior)

        # Fallback: low semantic similarity suggests potential contradiction
        # (not perfect, but better than negation-word matching)
        similarity = self._semantic_similarity(output, prior)
        # Very low similarity on the same topic suggests contradiction
        if similarity < 0.3:
            return True
        return False

    def _llm_contradiction_check(self, output: str, prior: str) -> bool:
        """Use LLM for NLI-style contradiction detection."""
        import re as _re

        prompt = (
            "Do these two statements contradict each other? "
            "Reply with ONLY 'yes' or 'no'.\n\n"
            f"Statement A: {prior[:500]}\n\n"
            f"Statement B: {output[:500]}"
        )
        try:
            response = self.llm_client.chat(
                messages=[{"role": "user", "content": prompt}],
                user_id="consistency_verifier",
                temperature=0.0,
            )
            answer = (response.content or "").strip().lower()
            return answer.startswith("yes")
        except Exception as e:
            logger.warning(f"LLM contradiction check failed: {e}")
            # Fallback to similarity
            return self._semantic_similarity(output, prior) < 0.3

    def _semantic_similarity(self, text_a: str, text_b: str) -> float:
        """Estimate semantic similarity (0-1)."""
        if self.embedding_service:
            try:
                emb_a = self.embedding_service.embed(text_a)
                emb_b = self.embedding_service.embed(text_b)
                from core.utils.similarity import cosine_similarity
                return cosine_similarity(emb_a, emb_b)
            except Exception:
                pass  # Fallback to word overlap
        
        # Fallback: word overlap (0.5 = uncertain when input is empty)
        from core.utils.similarity import word_overlap
        wa, wb = text_a.lower().split(), text_b.lower().split()
        if not wa or not wb:
            return 0.5
        return word_overlap(text_a, text_b)
