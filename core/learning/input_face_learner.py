"""Unified meta-learning loop for all input faces.

Design doc: docs/design/evaluation-and-evolution.md §5

Generalizes learning patterns to cover:
- Prompt quality (delegates to PromptOptimizer)
- Context budget allocation (adjusts per-task-type ratios)
- Knowledge freshness (triggers targeted decay/revalidation)

Each input face follows the same loop:
  DIAGNOSE → PROPOSE → VALIDATE → DEPLOY → RECORD
"""

from __future__ import annotations

import json
import threading
from dataclasses import dataclass, field
from datetime import datetime, timezone
from enum import Enum
from typing import Any

from sqlalchemy import text
from sqlalchemy.orm import Session

from core.db_consumer import DbConsumer, DbFactory
from core.logging_config import get_logger

logger = get_logger(__name__)


class InputFace(str, Enum):
    """Learnable input dimensions."""

    PROMPT = "prompt"
    CONTEXT_BUDGET = "context_budget"
    KNOWLEDGE = "knowledge"


@dataclass
class DiagnosisResult:
    """Output of the diagnose step."""

    input_face: InputFace
    bottleneck: str  # human-readable diagnosis
    evidence: dict[str, Any] = field(default_factory=dict)
    proposal: dict[str, Any] | None = None  # proposed fix
    applied: bool = False
    gate_verdict: str = "pending"
    error: str | None = None


class InputFaceLearner(DbConsumer):
    """Unified learning loop across prompt, context, and knowledge input faces.

    Usage::

        learner = InputFaceLearner(db, llm_client)
        results = learner.diagnose_and_fix(days=7)
        # Returns list of DiagnosisResult — one per input face that had issues
    """

    # Diagnose and quarantine use the same threshold to avoid count mismatch
    _STALE_CONFIDENCE = 0.3
    _budget_lock = threading.Lock()  # protects read-modify-write on _BUDGET_RATIOS

    def __init__(self, db_factory: DbFactory, llm_client: Any):
        super().__init__(db_factory)
        self._llm = llm_client

    # ------------------------------------------------------------------
    # Public API
    # ------------------------------------------------------------------

    def diagnose_and_fix(
        self,
        *,
        days: int = 7,
        faces: list[InputFace] | None = None,
        dry_run: bool = False,
    ) -> list[DiagnosisResult]:
        """Run diagnose→propose→validate→deploy for each input face.

        Args:
            days: Look-back window for failure analysis.
            faces: Which faces to check (default: all).
            dry_run: If True, diagnose and propose but don't deploy.

        Returns:
            List of DiagnosisResult for faces that had actionable findings.
        """
        if faces is None:
            faces = list(InputFace)

        results = []
        for face in faces:
            try:
                result = self._run_face(face, days=days, dry_run=dry_run)
                if result:
                    results.append(result)
            except Exception as e:
                logger.error("InputFaceLearner failed on %s: %s", face.value, e)
                results.append(
                    DiagnosisResult(
                        input_face=face,
                        bottleneck="",
                        error=str(e),
                    )
                )

        return results

    # ------------------------------------------------------------------
    # Per-face handlers
    # ------------------------------------------------------------------

    def _run_face(
        self,
        face: InputFace,
        *,
        days: int,
        dry_run: bool,
    ) -> DiagnosisResult | None:
        if face == InputFace.PROMPT:
            return self._handle_prompt(days=days, dry_run=dry_run)
        if face == InputFace.CONTEXT_BUDGET:
            return self._handle_context_budget(days=days, dry_run=dry_run)
        if face == InputFace.KNOWLEDGE:
            return self._handle_knowledge(days=days, dry_run=dry_run)
        return None

    # ── Prompt ────────────────────────────────────────────────────

    def _handle_prompt(
        self,
        *,
        days: int,
        dry_run: bool,
    ) -> DiagnosisResult | None:
        """Find low-rated prompt templates and trigger PromptOptimizer."""
        with self._db() as db:
            row = db.execute(
                text("""
                    SELECT f.prompt_template_id, COUNT(*) as cnt,
                           AVG(f.rating) as avg_rating
                    FROM eval_llm_feedback f
                    WHERE f.rating <= 2
                      AND f.created_at > DATE_SUB(UTC_TIMESTAMP(), INTERVAL :days DAY)
                      AND f.prompt_template_id IS NOT NULL
                    GROUP BY f.prompt_template_id
                    HAVING cnt >= 3
                    ORDER BY avg_rating ASC
                    LIMIT 1
                """),
                {"days": days},
            ).first()

            if not row:
                return None

            template_id, case_count, avg_rating = row[0], row[1], row[2]
            result = DiagnosisResult(
                input_face=InputFace.PROMPT,
                bottleneck=f"Template '{template_id}' has {case_count} low-rated cases (avg={avg_rating:.1f})",
                evidence={
                    "template_id": template_id,
                    "cases": case_count,
                    "avg_rating": float(avg_rating),
                },
            )

            from core.context.prompt_optimizer import PromptOptimizer

            optimizer = PromptOptimizer(self._db_factory, self._llm)
            try:
                opt_result = optimizer.optimize(template_id, dry_run=dry_run)
            except Exception as e:
                logger.error("PromptOptimizer failed for %s: %s", template_id, e)
                result.error = str(e)
                return result

            result.proposal = {
                "old_version": opt_result.old_version,
                "new_version": opt_result.new_version,
                "diagnosis": opt_result.diagnosis,
            }
            result.applied = opt_result.activated
            result.gate_verdict = opt_result.gate_verdict
            result.error = opt_result.error

            if result.applied:
                self._record_learning_event(InputFace.PROMPT, result)

            return result

    # ── Context Budget ────────────────────────────────────────────

    def _handle_context_budget(
        self,
        *,
        days: int,
        dry_run: bool,
    ) -> DiagnosisResult | None:
        """Detect task types where context was insufficient (truncation or low quality)."""
        with self._db() as db:
            rows = db.execute(
                text("""
                    SELECT cs.task_type,
                           COUNT(*) as total,
                           SUM(CASE WHEN cs.truncated_sections IS NOT NULL
                                     AND cs.truncated_sections != '[]' THEN 1 ELSE 0 END) as truncated,
                           AVG(COALESCE(f.rating, 3)) as avg_rating
                    FROM ctx_snapshots cs
                    LEFT JOIN eval_llm_feedback f ON cs.llm_request_id = f.llm_request_id
                    WHERE cs.created_at > DATE_SUB(UTC_TIMESTAMP(), INTERVAL :days DAY)
                    GROUP BY cs.task_type
                    HAVING truncated > total * 0.3 OR avg_rating < 2.5
                    ORDER BY avg_rating ASC
                    LIMIT 1
                """),
                {"days": days},
            ).first()

            if not rows:
                return None

            task_type, total, truncated, avg_rating = rows[0], rows[1], rows[2], rows[3]
            truncation_rate = truncated / total if total else 0

            result = DiagnosisResult(
                input_face=InputFace.CONTEXT_BUDGET,
                bottleneck=(
                    f"Task type '{task_type}': {truncation_rate:.0%} truncation rate, "
                    f"avg rating {avg_rating:.1f}"
                ),
                evidence={
                    "task_type": task_type,
                    "total": total,
                    "truncated": truncated,
                    "truncation_rate": truncation_rate,
                    "avg_rating": float(avg_rating),
                },
            )

            # Propose + apply under lock to prevent concurrent read-modify-write
            with self._budget_lock:
                proposal = self._propose_budget_adjustment(task_type, truncation_rate)
                result.proposal = proposal

                if dry_run:
                    result.gate_verdict = "dry_run"
                    return result

                # Apply: update runtime config
                self._apply_budget_adjustment(task_type, proposal)
            result.applied = True
            result.gate_verdict = "auto"  # budget changes are low-risk, auto-deploy
            self._record_learning_event(InputFace.CONTEXT_BUDGET, result)

            return result

    def _propose_budget_adjustment(
        self,
        task_type: str,
        truncation_rate: float,
    ) -> dict[str, Any]:
        """Propose budget ratio adjustment based on truncation patterns."""
        from core.context.manager import _BUDGET_RATIOS, TaskType

        try:
            tt = TaskType(task_type)
        except ValueError:
            tt = TaskType.GENERAL

        current = dict(_BUDGET_RATIOS.get(tt, _BUDGET_RATIOS[TaskType.GENERAL]))

        # Heuristic: the smallest-budget section is most likely truncated.
        # Boost it by taking from the largest section.
        sections = sorted(current.items(), key=lambda x: x[1])
        smallest_key = sections[0][0]
        largest_key = sections[-1][0]

        shift = min(0.10, current[largest_key] - 0.10)  # don't shrink largest below 0.10
        shift = max(0.0, shift)
        proposed = dict(current)
        proposed[smallest_key] = round(current[smallest_key] + shift, 2)
        proposed[largest_key] = round(current[largest_key] - shift, 2)

        return {"task_type": task_type, "current": current, "proposed": proposed}

    def _apply_budget_adjustment(
        self,
        task_type: str,
        proposal: dict[str, Any],
    ) -> None:
        """Apply budget adjustment to runtime config."""
        from core.context.manager import _BUDGET_RATIOS, TaskType

        try:
            tt = TaskType(task_type)
        except ValueError:
            return

        proposed = proposal.get("proposed")
        if proposed:
            _BUDGET_RATIOS[tt] = proposed
            logger.info("Budget adjusted for %s: %s", task_type, proposed)

    # ── Knowledge ─────────────────────────────────────────────────

    def _handle_knowledge(
        self,
        *,
        days: int,
        dry_run: bool,
    ) -> DiagnosisResult | None:
        """Detect stale or contradictory knowledge causing quality issues."""
        with self._db() as db:
            row = db.execute(
                text("""
                    SELECT COUNT(*) as stale_count
                    FROM sk_knowledge_entries
                    WHERE status = 'active'
                      AND confidence < :threshold
                      AND last_validated_at < DATE_SUB(UTC_TIMESTAMP(), INTERVAL :days DAY)
                """),
                {"days": days, "threshold": self._STALE_CONFIDENCE},
            ).first()

            stale_count = row[0] if row else 0
            if stale_count == 0:
                return None

            result = DiagnosisResult(
                input_face=InputFace.KNOWLEDGE,
                bottleneck=f"{stale_count} knowledge entries are stale (confidence < {self._STALE_CONFIDENCE}, not validated in {days}d)",
                evidence={"stale_count": stale_count, "days": days},
            )

            if dry_run:
                result.proposal = {"action": "revalidate_or_quarantine", "count": stale_count}
                result.gate_verdict = "dry_run"
                return result

            # Apply: quarantine + audit in one transaction
            quarantined = self._quarantine_and_record(days, result)
            result.proposal = {"action": "quarantined", "count": quarantined}
            result.applied = quarantined > 0
            result.gate_verdict = "auto"

            return result

    def _quarantine_and_record(self, days: int, result: DiagnosisResult) -> int:
        """Quarantine stale entries and record audit event in one transaction.

        Ensures quarantine and its audit trail are atomic — either both
        commit or neither does.
        """
        with self._db() as db:
            try:
                from core.utils.id_generator import generate_id

                qr = db.execute(
                    text("""
                        UPDATE sk_knowledge_entries
                        SET status = 'quarantined', confidence = 0, updated_at = UTC_TIMESTAMP()
                        WHERE status = 'active'
                          AND confidence < :threshold
                          AND last_validated_at < DATE_SUB(UTC_TIMESTAMP(), INTERVAL :days DAY)
                    """),
                    {"days": days, "threshold": self._STALE_CONFIDENCE},
                )
                count = qr.rowcount

                if count > 0:
                    # Audit in same transaction
                    result.proposal = {"action": "quarantined", "count": count}
                    result.applied = True
                    result.gate_verdict = "auto"
                    eid = generate_id()
                    db.execute(
                        text("""
                            INSERT INTO agent_events
                            (event_id, session_id, user_id, agent_id, agent_version,
                             event_type, content, causal_chain_id, created_at)
                            VALUES (:eid, 'system', 'system', 'system', '1.0.0',
                                    :etype, :content, :eid, UTC_TIMESTAMP())
                        """),
                        {
                            "eid": eid,
                            "etype": "input_face_learning",
                            "content": json.dumps(
                                {
                                    "face": InputFace.KNOWLEDGE.value,
                                    "bottleneck": result.bottleneck,
                                    "proposal": result.proposal,
                                    "gate_verdict": result.gate_verdict,
                                    "applied": result.applied,
                                }
                            ),
                        },
                    )

                db.commit()
                logger.info("Quarantined %d stale entries (with audit)", count)
                return count
            except Exception as e:
                logger.error("Quarantine+audit failed: %s", e)
                db.rollback()
                return 0

    # ------------------------------------------------------------------
    # Audit
    # ------------------------------------------------------------------

    def _record_learning_event(
        self,
        face: InputFace,
        result: DiagnosisResult,
    ) -> None:
        """Record learning action as a conversation event for audit trail."""
        with self._db() as db:
            try:
                from core.utils.id_generator import generate_id

                eid = generate_id()
                db.execute(
                    text("""
                        INSERT INTO agent_events
                        (event_id, session_id, user_id, agent_id, agent_version,
                         event_type, content, causal_chain_id, created_at)
                        VALUES (:eid, 'system', 'system', 'system', '1.0.0',
                                :etype, :content, :eid, UTC_TIMESTAMP())
                    """),
                    {
                        "eid": eid,
                        "etype": "input_face_learning",
                        "content": json.dumps(
                            {
                                "face": face.value,
                                "bottleneck": result.bottleneck,
                                "proposal": result.proposal,
                                "gate_verdict": result.gate_verdict,
                                "applied": result.applied,
                            }
                        ),
                    },
                )
                db.commit()
            except Exception as e:
                logger.warning("Failed to record learning event: %s", e)
                db.rollback()
