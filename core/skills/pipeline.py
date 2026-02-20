"""Unified skill selection pipeline: retrieve → audit → feedback.

Design doc: docs/design/unified-selector-pipeline.md
"""

from __future__ import annotations

import json
import threading
import time
from dataclasses import dataclass, field
from datetime import datetime, timezone
from typing import Any
from sqlalchemy import text

from sqlalchemy.orm import Session
from uuid_utils import uuid7

from core.logging_config import get_logger
from core.skills.learning_signals import SignalType, SignalWeights
from core.skills.modern_selector import ModernSkillSelector
from core.skills.self_improving_selector import SelfImprovingSelector

logger = get_logger(__name__)


# ---------------------------------------------------------------------------
# Public data types
# ---------------------------------------------------------------------------

@dataclass
class SkillCandidate:
    """Skill candidate for learning application."""
    name: str
    version: str = "1.0.0"
    confidence: float = 1.0


@dataclass
class ToolsResult:
    """Result of skill selection — ready for LLM function calling."""

    tools: list[dict[str, Any]]          # OpenAI tools schema
    event_id: str | None = None          # Audit event ID (None if audit off)
    candidates: int = 0                  # Candidates considered
    retrieval_method: str = "keyword"    # "semantic" or "keyword"


@dataclass
class LearningResult:
    """Result of a learning cycle."""

    learned: int = 0
    total_failures: int = 0
    signals_by_type: dict[str, int] = field(default_factory=dict)
    gate_verdict: str = "skipped"
    improvement_pct: float = 0.0
    error: str | None = None


# ---------------------------------------------------------------------------
# Feedback buffer (batched writes)
# ---------------------------------------------------------------------------

class _FeedbackBuffer:
    """Thread-safe buffer that batches feedback signals before DB write."""

    def __init__(self, db: Session, *, batch_size: int = 50, flush_interval: float = 2.0):
        self._db = db
        self._batch_size = batch_size
        self._flush_interval = flush_interval
        self._buffer: list[dict[str, Any]] = []
        self._lock = threading.Lock()
        self._last_flush = time.time()

    def add(self, event_id: str, signal: SignalType, data: dict[str, Any]) -> None:
        with self._lock:
            self._buffer.append({
                "signal_id": str(uuid7()),
                "selection_event_id": event_id,
                "signal_type": signal.value,
                "signal_data": json.dumps(data),
                "created_at": datetime.now(timezone.utc),
            })
            if len(self._buffer) >= self._batch_size:
                self._flush_locked()

    def flush(self) -> int:
        with self._lock:
            return self._flush_locked()

    def maybe_flush(self) -> int:
        """Flush if interval elapsed."""
        if time.time() - self._last_flush >= self._flush_interval:
            return self.flush()
        return 0

    def _flush_locked(self) -> int:
        """Must be called with self._lock held."""
        if not self._buffer:
            return 0
        batch = self._buffer[:]
        self._buffer.clear()
        self._last_flush = time.time()

        try:
            for row in batch:
                self._db.execute(
                    text("""INSERT INTO skill_learning_signals
                           (signal_id, selection_event_id, signal_type, signal_data, created_at)
                           VALUES (:signal_id, :selection_event_id, :signal_type, :signal_data, :created_at)"""),
                    {
                        "signal_id": row["signal_id"],
                        "selection_event_id": row["selection_event_id"],
                        "signal_type": row["signal_type"],
                        "signal_data": row["signal_data"],
                        "created_at": row["created_at"],
                    },
                )
            self._db.commit()
            logger.debug("Flushed %d feedback signals", len(batch))
            return len(batch)
        except Exception as e:
            logger.error("Feedback flush failed: %s", e)
            self._db.rollback()
            # Re-queue
            self._buffer.extend(batch)
            return 0


# ---------------------------------------------------------------------------
# SkillPipeline
# ---------------------------------------------------------------------------

class SkillPipeline:
    """Unified skill selection: retrieve → audit → feedback.

    Usage in ChatLoop::

        result = pipeline.get_tools_schema(query, session_id)
        llm_response = llm.chat_with_tools(messages, tools=result.tools)
        # after execution:
        pipeline.record_feedback(result.event_id, SignalType.EXECUTION_TIME, {"ms": 150})
    """

    def __init__(
        self,
        db: Session,
        llm_client: Any,
        *,
        audit: bool = True,
        learning: bool = True,
        learning_weights: SignalWeights | None = None,
        embed_fn=None,
    ):
        self._db = db
        self._llm = llm_client
        self._audit = audit
        self._learning = learning

        # Resolve embed_fn: explicit > EmbeddingService > None
        if embed_fn is None:
            try:
                from core.context.embeddings import EmbeddingService
                _svc = EmbeddingService(db)
                embed_fn = _svc.embed_text
            except Exception:  # noqa: BLE001
                pass  # no embeddings available — keyword fallback

        # Internal engines (not exposed)
        self._modern = ModernSkillSelector(db, llm_client, embed_fn=embed_fn)
        self._improver: SelfImprovingSelector | None = None
        if learning:
            self._improver = SelfImprovingSelector(db, llm_client, weights=learning_weights)

        self._feedback = _FeedbackBuffer(db)

    # ------------------------------------------------------------------
    # Stage 1 + 2: retrieve → rank → (apply corrections) → audit
    # ------------------------------------------------------------------

    def get_tools_schema(
        self,
        query: str,
        session_id: str,
        *,
        max_candidates: int = 5,
        context_budget: int = 2000,
    ) -> ToolsResult:
        """Select skills and return tools schema for LLM.

        Stage 1: Retrieve candidates via rule-based + LLM ranking.
                 Apply learned corrections if learning is enabled.
        Stage 2: Record audit event with selection metadata.
        """
        # Stage 1a: retrieve + rank (progressive disclosure)
        tools, retrieval_method = self._modern.get_tools_schema(
            query, max_candidates=max_candidates, context_budget=context_budget,
        )
        skill_names = [t["function"]["name"] for t in tools]

        # Stage 1b: apply learned corrections
        if self._improver and skill_names:
            candidates = [SkillCandidate(name=n) for n in skill_names]
            corrected = self._improver.apply_learnings(query, candidates)
            corrected_names = {c.name for c in corrected}

            if corrected_names != set(skill_names):
                logger.info(
                    "Learning correction: %s → %s",
                    skill_names, list(corrected_names),
                )
                # Filter tools to corrected set, preserving schema
                tools = [t for t in tools if t["function"]["name"] in corrected_names]
                # Add tools for newly-added skills (corrections may add skills)
                existing = {t["function"]["name"] for t in tools}
                for name in corrected_names - existing:
                    extra, _ = self._modern.get_tools_schema(name, max_candidates=1)
                    tools.extend(extra)

        # Stage 2: audit
        event_id = None
        if self._audit:
            event_id = self._record_selection(query, session_id, tools, retrieval_method)

        # Opportunistic flush
        self._feedback.maybe_flush()

        return ToolsResult(
            tools=tools,
            event_id=event_id,
            candidates=len(tools),
            retrieval_method=retrieval_method,
        )

    # ------------------------------------------------------------------
    # Stage 3: feedback (buffered)
    # ------------------------------------------------------------------

    def record_feedback(
        self,
        event_id: str | None,
        signal: SignalType,
        data: dict[str, Any],
    ) -> None:
        """Buffer a feedback signal. No-op if event_id is None."""
        if not event_id or not self._learning:
            return
        self._feedback.add(event_id, signal, data)

    def flush_feedback(self) -> int:
        """Force-flush buffered feedback. Call on session close."""
        return self._feedback.flush()

    # ------------------------------------------------------------------
    # Learning (called by scheduler / API, not by ChatLoop)
    # ------------------------------------------------------------------

    def learn(self, *, days: int = 7, skip_gate: bool = False) -> LearningResult:
        """Run learning cycle: learn → verify → activate.

        Pipeline:
            1. Extract signals from recent failures (staging)
            2. Validate via RegressionGate (unless skip_gate=True)
            3. Activate learnings only if gate passes

        Args:
            days: Look-back window for failure analysis.
            skip_gate: Skip regression validation (dev/testing only).
        """
        if not self._improver:
            return LearningResult(error="Learning disabled")

        try:
            # Stage 1: Learn (writes to DB — learnings are "staged")
            result = self._improver.learn_from_failures(days=days)
            learned = result.get("learned", 0)
            if learned == 0:
                return LearningResult(
                    total_failures=result.get("total_failures", 0),
                    gate_verdict="skipped",
                )

            # Stage 2: Validate via unified RegressionGate
            gate_verdict = "skipped"
            improvement_pct = 0.0

            if not skip_gate:
                try:
                    from core.evaluation.regression_gate import RegressionGate, ChangeType
                    gate = RegressionGate(self._db)
                    gate_result = gate.validate_change(
                        change_type=ChangeType.SELECTOR,
                        change_id=f"learning_cycle_{days}d",
                        change_content=result.get("signals_by_type", {}),
                        golden_session_count=20,
                    )
                    gate_verdict = gate_result["verdict"]
                    improvement_pct = gate_result.get("metrics", {}).get("score_delta", 0.0)

                    # Stage 3: Rollback if gate fails
                    if gate_verdict == "fail":
                        self._rollback_learnings(days)
                        logger.warning("Learning rolled back: gate failed (%s)", gate_result.get("reason"))
                except Exception as e:
                    logger.warning("Gate validation unavailable, learnings kept: %s", e)
                    gate_verdict = "error"

            return LearningResult(
                learned=learned,
                total_failures=result.get("total_failures", 0),
                signals_by_type=result.get("signals_by_type", {}),
                gate_verdict=gate_verdict,
                improvement_pct=improvement_pct,
            )
        except Exception as e:
            logger.error("Learning cycle failed: %s", e)
            return LearningResult(error=str(e))

    def _rollback_learnings(self, days: int) -> None:
        """Remove learnings created in the current cycle."""
        try:
            self._db.execute(
                text(
                    "DELETE FROM selector_learnings "
                    "WHERE created_at > DATE_SUB(UTC_TIMESTAMP(), INTERVAL :days DAY)"
                ),
                {"days": days},
            )
            self._db.commit()
        except Exception as e:
            logger.error("Learning rollback failed: %s", e)
            self._db.rollback()

    def stats(self) -> dict[str, Any]:
        """Get learning statistics."""
        if not self._improver:
            return {"error": "Learning disabled"}
        return self._improver.get_learning_stats()

    def selection_history(
        self, session_id: str | None = None, limit: int = 100,
    ) -> list[dict[str, Any]]:
        """Get selection history for analysis."""
        from sqlalchemy import text

        sql = "SELECT event_id, session_id, user_query, selected_skills, selection_method, created_at FROM skill_selection_events"
        params: dict[str, Any] = {}
        if session_id:
            sql += " WHERE session_id = :sid"
            params["sid"] = session_id
        sql += " ORDER BY created_at DESC LIMIT :lim"
        params["lim"] = limit

        try:
            rows = self._db.execute(text(sql), params).fetchall()
            return [
                {
                    "event_id": r[0], "session_id": r[1], "user_query": r[2],
                    "selected_skills": r[3], "selection_method": r[4],
                    "created_at": str(r[5]),
                }
                for r in rows
            ]
        except Exception as e:
            logger.error("Failed to get selection history: %s", e)
            return []

    # ------------------------------------------------------------------
    # Internal
    # ------------------------------------------------------------------

    def _record_selection(
        self, query: str, session_id: str, tools: list[dict], retrieval_method: str,
    ) -> str:
        """Write audit event to DB. Returns event_id."""
        event_id = str(uuid7())
        skill_names = [t["function"]["name"] for t in tools]

        try:
            self._db.execute(
                text("""INSERT INTO skill_selection_events
                       (event_id, session_id, user_query, selected_skills,
                        selection_method, created_at)
                       VALUES (:event_id, :session_id, :user_query, :selected_skills,
                        :selection_method, :created_at)"""),
                {
                    "event_id": event_id,
                    "session_id": session_id,
                    "user_query": query,
                    "selected_skills": ",".join(skill_names),
                    "selection_method": retrieval_method,
                    "created_at": datetime.now(timezone.utc).replace(tzinfo=None),
                },
            )
            self._db.commit()
        except Exception as e:
            logger.warning("Audit event write failed: %s", e)
            self._db.rollback()
            return event_id  # Return ID anyway; selection still works

        return event_id
