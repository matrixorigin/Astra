"""Unified skill selection pipeline: retrieve → audit → feedback.

Design doc: docs/design/unified-selector-pipeline.md
"""

from __future__ import annotations

import json
import threading
import time
from dataclasses import dataclass, field
from datetime import datetime, timedelta, timezone
from typing import Any
from sqlalchemy import text

from sqlalchemy.orm import Session
from core.db_consumer import DbFactory
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
    retrieval_method: str | None = None  # "semantic" or "keyword", None if unknown
    latency_ms: int = 0                  # End-to-end selection latency


@dataclass
class LearningResult:
    """Result of a learning cycle."""

    learned: int = 0
    total_failures: int = 0
    signals_by_type: dict[str, int] = field(default_factory=dict)
    gate_verdict: str = "skipped"
    improvement_pct: float = 0.0
    input_face_results: list[dict[str, Any]] = field(default_factory=list)
    error: str | None = None


# ---------------------------------------------------------------------------
# Feedback buffer (batched writes)
# ---------------------------------------------------------------------------

class _FeedbackBuffer:
    """Thread-safe buffer that batches feedback signals before DB write.

    Uses a dedicated Session for flush to avoid contention with the
    caller's Session when multiple threads add/flush concurrently.
    """

    def __init__(self, db_factory: DbFactory, *, batch_size: int = 50, flush_interval: float = 2.0):
        _tmp = db_factory()
        self._engine = _tmp.get_bind()  # Engine is thread-safe; extract once
        _tmp.close()
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
        """Must be called with self._lock held. Uses independent connection."""
        if not self._buffer:
            return 0
        batch = self._buffer[:]
        self._buffer.clear()
        self._last_flush = time.time()

        try:
            with self._engine.connect() as conn:
                for row in batch:
                    conn.execute(
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
                conn.execute(text("COMMIT"))
            logger.debug("Flushed %d feedback signals", len(batch))
            return len(batch)
        except Exception as e:
            logger.error("Feedback flush failed: %s", e)
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
        db_factory: DbFactory,
        llm_client: Any,
        *,
        audit: bool = True,
        learning: bool = True,
        learning_weights: SignalWeights | None = None,
        embed_fn=None,
    ):
        self._db_factory = db_factory
        self._llm = llm_client
        self._audit = audit
        self._learning = learning

        # Resolve embed_fn: explicit > EmbeddingService > None
        if embed_fn is None:
            try:
                from core.context.embeddings import EmbeddingService
                _svc = EmbeddingService(db_factory)
                if _svc.provider != "mock":
                    embed_fn = _svc.embed_text
            except Exception:  # noqa: BLE001
                pass  # no embeddings available — keyword fallback

        # Internal engines (not exposed)
        db = db_factory()
        self._modern = ModernSkillSelector(db, llm_client, embed_fn=embed_fn)
        self._improver: SelfImprovingSelector | None = None
        if learning:
            self._improver = SelfImprovingSelector(db_factory, llm_client, weights=learning_weights)

        self._feedback = _FeedbackBuffer(db_factory)

    def reload_skills(self, registry=None):
        """Reload skills from DB after registration."""
        self._modern.rule_selector._load_skills()
        if registry is not None:
            self._modern._registry = registry

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
        t0 = time.monotonic()
        # Stage 1a: retrieve + rank (progressive disclosure)
        tools, retrieval_method = self._modern.get_tools_schema(
            query, max_candidates=max_candidates, context_budget=context_budget,
        )
        skill_names = [t["function"]["name"] for t in tools]

        # Stage 1b: apply learned corrections (order-preserving)
        if self._improver and skill_names:
            candidates = [SkillCandidate(name=n) for n in skill_names]
            corrected = self._improver.apply_learnings(query, candidates)
            corrected_names = [c.name for c in corrected]

            if corrected_names != skill_names:
                logger.info(
                    "Learning correction: %s → %s",
                    skill_names, corrected_names,
                )
                # Rebuild tools in corrected order
                tool_by_name = {t["function"]["name"]: t for t in tools}
                ordered_tools = []
                for name in corrected_names:
                    if name in tool_by_name:
                        ordered_tools.append(tool_by_name[name])
                    else:
                        # New skill added by correction — look up by exact name
                        schema = self._modern._skill_to_tool_schema_by_name(name)
                        if schema:
                            ordered_tools.append(schema)
                tools = ordered_tools

        # Stage 2: audit
        event_id = None
        if self._audit:
            event_id = self._record_selection(query, session_id, tools, retrieval_method)

        # Opportunistic flush
        self._feedback.maybe_flush()

        latency_ms = int((time.monotonic() - t0) * 1000)
        logger.debug("select_tools latency=%dms tools=%d", latency_ms, len(tools))
        return ToolsResult(
            tools=tools,
            event_id=event_id,
            candidates=len(tools),
            retrieval_method=retrieval_method,
            latency_ms=latency_ms,
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
            1. Skill selection learning (SelfImprovingSelector)
            2. Input face learning (prompt, context budget, knowledge)
            3. Validate via RegressionGate (unless skip_gate=True)
            4. Activate learnings only if gate passes

        Args:
            days: Look-back window for failure analysis.
            skip_gate: Skip regression validation (dev/testing only).
        """
        if not self._improver:
            return LearningResult(error="Learning disabled")

        try:
            # Stage 1: Skill selection learning
            result = self._improver.learn_from_failures(days=days)
            selector_learned = result.get("learned", 0)

            # Stage 1b: Input face learning (prompt, context, knowledge)
            input_face_results = []
            face_applied = 0
            try:
                from core.learning.input_face_learner import InputFaceLearner
                face_learner = InputFaceLearner(self._db_factory, self._llm)
                input_face_results = face_learner.diagnose_and_fix(
                    days=days, dry_run=skip_gate,
                )
                face_applied = sum(1 for fr in input_face_results if fr.applied)
            except Exception as e:
                logger.warning("Input face learning unavailable: %s", e)

            learned = selector_learned + face_applied
            face_dicts = [
                {"face": r.input_face.value, "bottleneck": r.bottleneck, "applied": r.applied}
                for r in input_face_results
            ]
            if learned == 0:
                return LearningResult(
                    total_failures=result.get("total_failures", 0),
                    gate_verdict="skipped",
                    input_face_results=face_dicts,
                )

            # Stage 2: Validate via unified RegressionGate
            gate_verdict = "skipped"
            improvement_pct = 0.0

            if not skip_gate:
                try:
                    from core.evaluation.regression_gate import RegressionGate, ChangeType
                    gate = RegressionGate(self._db_factory)
                    gate_result = gate.validate_change(
                        change_type=ChangeType.SELECTOR,
                        change_id=f"learning_cycle_{days}d",
                        change_content=result.get("signals_by_type", {}),
                        golden_session_count=20,
                    )
                    gate_verdict = gate_result["verdict"]
                    improvement_pct = gate_result.get("metrics", {}).get("score_delta", 0.0)

                    # Stage 3: Rollback selector learnings if gate fails
                    # (input face changes are independently validated, not rolled back)
                    if gate_verdict == "fail":
                        self._rollback_learnings(days)
                        selector_learned = 0
                        logger.warning("Learning rolled back: gate failed (%s)", gate_result.get("reason"))
                except Exception as e:
                    logger.warning("Gate validation unavailable, learnings kept: %s", e)
                    gate_verdict = "error"

            return LearningResult(
                learned=selector_learned + face_applied,
                total_failures=result.get("total_failures", 0),
                signals_by_type=result.get("signals_by_type", {}),
                gate_verdict=gate_verdict,
                improvement_pct=improvement_pct,
                input_face_results=face_dicts,
            )
        except Exception as e:
            logger.error("Learning cycle failed: %s", e)
            return LearningResult(error=str(e))

    def _rollback_learnings(self, days: int) -> None:
        """Soft-delete learnings created in the current cycle via SelfImprovingSelector."""
        if not self._improver:
            return
        since = datetime.now(timezone.utc) - timedelta(days=days)
        try:
            count = self._improver.rollback_learnings(since=since)
            logger.info("Rolled back %d learnings (since %s)", count, since)
        except Exception as e:
            logger.error("Learning rollback failed: %s", e)
            pass  # improver handles its own sessions

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

        sql = "SELECT event_id, session_id, user_query, selected_skills, skill_name, selection_method, created_at FROM skill_selection_events"
        params: dict[str, Any] = {}
        if session_id:
            sql += " WHERE session_id = :sid"
            params["sid"] = session_id
        sql += " ORDER BY created_at DESC LIMIT :lim"
        params["lim"] = limit

        db = self._db_factory()
        try:
            rows = db.execute(text(sql), params).fetchall()
            return [
                {
                    "event_id": r[0], "session_id": r[1], "user_query": r[2],
                    "selected_skills": r[3], "skill_name": r[4],
                    "selection_method": r[5], "created_at": str(r[6]),
                }
                for r in rows
            ]
        except Exception as e:
            logger.error("Failed to get selection history: %s", e)
            return []
        finally:
            db.close()

    # ------------------------------------------------------------------
    # Internal
    # ------------------------------------------------------------------

    def _record_selection(
        self, query: str, session_id: str, tools: list[dict], retrieval_method: str,
    ) -> str:
        """Write audit event to DB. Returns event_id.

        ``skill_name`` stores the top-ranked candidate (index 0).  This is
        the skill ChatLoop will actually invoke, so deprecation / regression
        queries that filter by ``skill_name`` match the *executed* skill.
        """
        event_id = str(uuid7())
        skill_names = [t["function"]["name"] for t in tools]
        top_skill = skill_names[0] if skill_names else None

        db = self._db_factory()
        try:
            # Resolve current active version from registry
            skill_version = None
            if top_skill:
                try:
                    row = db.execute(
                        text("SELECT version FROM skills_registry WHERE skill_name = :n AND is_active = 1 ORDER BY created_at DESC LIMIT 1"),
                        {"n": top_skill},
                    ).fetchone()
                    if row:
                        skill_version = row[0]
                except Exception:
                    pass  # version is best-effort

            db.execute(
                text("""INSERT INTO skill_selection_events
                       (event_id, session_id, user_query, selected_skills,
                        skill_name, skill_version, selection_method, created_at)
                       VALUES (:event_id, :session_id, :user_query, :selected_skills,
                        :skill_name, :skill_version, :selection_method, :created_at)"""),
                {
                    "event_id": event_id,
                    "session_id": session_id,
                    "user_query": query,
                    "selected_skills": json.dumps(skill_names),
                    "skill_name": top_skill,
                    "skill_version": skill_version,
                    "selection_method": retrieval_method,
                    "created_at": datetime.now(timezone.utc).replace(tzinfo=None),
                },
            )
            db.commit()
        except Exception as e:
            logger.warning("Audit event write failed: %s", e)
            db.rollback()
        finally:
            db.close()

        return event_id
