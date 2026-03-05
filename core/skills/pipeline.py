"""Unified skill selection pipeline: retrieve → audit → feedback.

Design doc: docs/design/skills-and-tools.md
"""

from __future__ import annotations

import json
import threading
import time
from dataclasses import dataclass, field
from datetime import datetime, timedelta, timezone
from typing import Any
from sqlalchemy import text

from core.db_consumer import DbConsumer, DbFactory
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
    high_confidence_skill: str | None = None  # If set, can skip LLM tool selection
    scores: list[tuple[str, float]] = field(default_factory=list)  # (name, score) pairs
    catalog: str | None = None           # Lightweight tool list for two-phase selection
    pre_filter_applied: bool = False     # Whether pre-filtering narrowed candidates


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

    _MAX_BUFFER_SIZE = 10_000
    _MAX_RETRIES = 3

    def __init__(self, db_factory: DbFactory, *, batch_size: int = 50, flush_interval: float = 2.0):
        _tmp = db_factory()
        self._engine = _tmp.get_bind()  # Engine is thread-safe; extract once
        _tmp.close()
        self._batch_size = batch_size
        self._flush_interval = flush_interval
        self._buffer: list[dict[str, Any]] = []
        self._retry_counts: dict[str, int] = {}  # signal_id → retry count
        self._lock = threading.Lock()
        self._last_flush = time.time()

    def add(self, event_id: str, signal: SignalType, data: dict[str, Any]) -> None:
        with self._lock:
            if len(self._buffer) >= self._MAX_BUFFER_SIZE:
                dropped = self._buffer[:self._batch_size]
                self._buffer = self._buffer[self._batch_size:]
                for d in dropped:
                    self._retry_counts.pop(d["signal_id"], None)
                logger.warning("Feedback buffer full (%d), dropped %d oldest signals",
                               self._MAX_BUFFER_SIZE, len(dropped))
            sid = str(uuid7())
            self._buffer.append({
                "signal_id": sid,
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
            for row in batch:
                self._retry_counts.pop(row["signal_id"], None)
            return len(batch)
        except Exception as e:
            logger.error("Feedback flush failed: %s", e)
            # Re-queue with retry limit
            requeued = []
            for row in batch:
                sid = row["signal_id"]
                count = self._retry_counts.get(sid, 0) + 1
                if count <= self._MAX_RETRIES:
                    self._retry_counts[sid] = count
                    requeued.append(row)
                else:
                    self._retry_counts.pop(sid, None)
            if requeued:
                self._buffer.extend(requeued)
            dropped = len(batch) - len(requeued)
            if dropped:
                logger.warning("Dropped %d feedback signals after %d retries",
                               dropped, self._MAX_RETRIES)
            return 0


# ---------------------------------------------------------------------------
# SkillPipeline
# ---------------------------------------------------------------------------

class SkillPipeline(DbConsumer):
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
        super().__init__(db_factory)
        self._llm = llm_client
        self._audit = audit
        self._learning = learning

        # Resolve embed_fn: explicit > EmbeddingClient singleton > None
        if embed_fn is None:
            try:
                from core.context.embeddings import get_embedding_client
                embed_fn = get_embedding_client().embed
            except Exception:  # noqa: BLE001
                pass  # no embeddings available — keyword fallback

        # Internal engines (not exposed)
        self._modern = ModernSkillSelector(db_factory, llm_client, embed_fn=embed_fn)
        self._improver: SelfImprovingSelector | None = None
        if learning:
            self._improver = SelfImprovingSelector(db_factory, llm_client, weights=learning_weights, embed_fn=embed_fn)

        self._feedback = _FeedbackBuffer(db_factory)

    def reload_skills(self, registry=None):
        """Reload skills from DB after registration."""
        self._modern.rule_selector._load_skills()
        if registry is not None:
            self._modern._registry = registry

    # ------------------------------------------------------------------
    # Stage 1 + 1.5 + 2: retrieve → pre-filter → (corrections) → audit
    # ------------------------------------------------------------------

    def get_tools_schema(
        self,
        query: str,
        session_id: str,
        *,
        max_candidates: int = 5,
        context_budget: int = 2000,
        conversation_state: Any = None,
    ) -> ToolsResult:
        """Select skills and return tools schema for LLM.

        Stage 1: Retrieve candidates via semantic/keyword + confidence scoring.
                 Apply learned corrections if learning is enabled.
        Stage 1.5: Post-retrieval pre-filter — reorder candidates by conversation
                   state + skill tags (0 tokens, no shared state mutation).
        Stage 2: Record audit event with selection metadata.

        Args:
            conversation_state: Optional ConversationState for pre-filtering.

        If high_confidence_skill is set in result, caller can skip LLM tool selection.
        """
        from core.skills.prefilter import pre_filter

        t0 = time.monotonic()

        # Stage 1a: retrieve + rank with confidence scoring
        selection = self._modern.select_tools(
            query, max_candidates=max_candidates, context_budget=context_budget,
        )

        # Stage 1.5: post-retrieval pre-filter — reorder tools by conversation
        # state + skill tags.  Operates on the retrieved tools list (a local copy),
        # never mutates shared rule_selector.skills.
        pre_filter_applied = False
        if conversation_state is not None and selection.tools:
            skills_lookup = self._modern.rule_selector.skills  # read-only lookup
            tool_names = [t["function"]["name"] for t in selection.tools]
            # Resolve SkillMetadata for each retrieved tool (pre_filter needs .tags)
            metadata_list = [
                skills_lookup[n] for n in tool_names if n in skills_lookup
            ]
            if metadata_list:
                reordered, pre_filter_applied = pre_filter(metadata_list, conversation_state)
                if pre_filter_applied:
                    # Reorder tools to match pre_filter output
                    tool_by_name = {t["function"]["name"]: t for t in selection.tools}
                    selection.tools[:] = [
                        tool_by_name[m.name] for m in reordered if m.name in tool_by_name
                    ]

        tools = selection.tools
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
            event_id = self._record_selection(query, session_id, tools, selection.retrieval_method)

        # Opportunistic flush
        self._feedback.maybe_flush()

        latency_ms = int((time.monotonic() - t0) * 1000)
        logger.debug("select_tools latency=%dms tools=%d pre_filter=%s", latency_ms, len(tools), pre_filter_applied)
        return ToolsResult(
            tools=tools,
            event_id=event_id,
            candidates=len(tools),
            retrieval_method=selection.retrieval_method,
            latency_ms=latency_ms,
            high_confidence_skill=selection.high_confidence_skill,
            scores=selection.scores,
            catalog=selection.catalog,
            pre_filter_applied=pre_filter_applied,
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
        """Get learning statistics and per-skill execution metrics.

        Returns:
            Dict with 'learning' (from SelfImprovingSelector) and 'per_skill'
            (selection_count, success_rate, avg_cost_usd, avg_time_ms per skill).
        """
        from sqlalchemy import case, func

        from api.models.skill import SkillSelectionEvent

        result: dict[str, Any] = {}

        # Learning stats
        if self._improver:
            result["learning"] = self._improver.get_learning_stats()
        else:
            result["learning"] = {"error": "Learning disabled"}

        # Per-skill execution metrics via ORM
        try:
            with self._db() as db:
                rows = db.query(
                    SkillSelectionEvent.skill_name,
                    func.count().label("selection_count"),
                    func.sum(case(
                        (SkillSelectionEvent.execution_success == 1, 1),
                        else_=0,
                    )).label("success_count"),
                    func.avg(SkillSelectionEvent.execution_cost).label("avg_cost"),
                    func.avg(SkillSelectionEvent.execution_time_ms).label("avg_time"),
                ).filter(
                    SkillSelectionEvent.skill_name.isnot(None),
                ).group_by(
                    SkillSelectionEvent.skill_name,
                ).all()

                per_skill = {}
                for r in rows:
                    total = r.selection_count
                    per_skill[r.skill_name] = {
                        "selection_count": total,
                        "success_rate": (r.success_count or 0) / total if total else 0.0,
                        "avg_cost_usd": round(float(r.avg_cost or 0), 6),
                        "avg_time_ms": round(float(r.avg_time or 0), 1),
                    }
                result["per_skill"] = per_skill
        except Exception as e:
            logger.warning("Failed to query per-skill stats: %s", e)
            result["per_skill"] = {}

        return result

    def selection_history(
        self, session_id: str | None = None, limit: int = 100,
    ) -> list[dict[str, Any]]:
        """Get selection history for analysis."""
        from api.models.skill import SkillSelectionEvent

        try:
            with self._db() as db:
                q = db.query(SkillSelectionEvent)
                if session_id:
                    q = q.filter(SkillSelectionEvent.session_id == session_id)
                rows = q.order_by(SkillSelectionEvent.created_at.desc()).limit(limit).all()
                return [
                    {
                        "event_id": r.event_id, "session_id": r.session_id,
                        "user_query": r.user_query, "selected_skills": r.selected_skills,
                        "skill_name": r.skill_name, "selection_method": r.selection_method,
                        "created_at": str(r.created_at),
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
        """Write audit event to DB. Returns event_id.

        ``skill_name`` stores the top-ranked candidate (index 0).  This is
        the skill ChatLoop will actually invoke, so deprecation / regression
        queries that filter by ``skill_name`` match the *executed* skill.
        """
        from api.models.skill import SkillRegistry as SkillModel, SkillSelectionEvent

        event_id = str(uuid7())
        skill_names = [t["function"]["name"] for t in tools]
        top_skill = skill_names[0] if skill_names else None

        try:
            with self._db() as db:
                # Resolve current active version from registry
                skill_version = None
                if top_skill:
                    try:
                        reg = db.query(SkillModel.version).filter(
                            SkillModel.skill_name == top_skill,
                            SkillModel.is_active == 1,
                        ).order_by(SkillModel.created_at.desc()).first()
                        if reg:
                            skill_version = reg.version
                    except Exception:
                        pass  # version is best-effort

                evt = SkillSelectionEvent(
                    event_id=event_id,
                    session_id=session_id,
                    user_query=query,
                    selected_skills=skill_names,
                    skill_name=top_skill,
                    skill_version=skill_version,
                    selection_method=retrieval_method,
                    created_at=datetime.now(timezone.utc).replace(tzinfo=None),
                )
                db.add(evt)
                db.commit()
        except Exception as e:
            logger.warning("Audit event write failed: %s", e)

        return event_id
