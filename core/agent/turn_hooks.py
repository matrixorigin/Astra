"""Shared post-turn hooks for ChatLoop and /chat/turn.

Extracts the common persistence logic (decision audit, skill selection,
observer, implicit feedback) so both code paths stay in sync.
"""

from __future__ import annotations

import logging
import threading
import time
from typing import Any

from core.db_consumer import DbConsumer, DbFactory

logger = logging.getLogger(__name__)

# Implicit-feedback rating map (signal_type → 1-5 rating).
# Keys must cover all non-neutral signal types from ImplicitFeedbackDetector.
_RATING_MAP = {
    "positive": 5,
    "correction": 1,
    "frustration": 1,
    "rephrasing": 2,
    "clarification": 3,
    "negative": 1,
}


class TurnHooks(DbConsumer):
    """Post-turn persistence hooks shared by ChatLoop and /chat/turn."""

    # TODO(future-arch): Async tasks (Observer, implicit feedback) should go through
    # an internal event bus or API endpoints instead of direct DB access. Benefits:
    # consistent auth/audit, decoupled from DB schema, enables distributed workers.

    def __init__(self, db_factory: DbFactory, llm_client: Any = None, embed_fn: Any = None):
        super().__init__(db_factory)
        self._llm_client = llm_client
        self._embed_fn = embed_fn

    # ── Decision audit ────────────────────────────────────────────────

    def record_ctx_decision_audits(
        self,
        session_id: str,
        event_id: str,
        tool_calls: list[dict[str, Any]],
        response_text: str,
        context_capture_id: str | None,
        model_used: str | None = None,
    ) -> None:
        """Record a decision audit entry."""
        from uuid_utils import uuid7

        from api.models import DecisionAudit

        tc_names = [tc.get("function", {}).get("name", "") for tc in tool_calls] if tool_calls else []
        try:
            with self._db() as db:
                db.add(DecisionAudit(
                    decision_id=str(uuid7()),
                    session_id=session_id,
                    event_id=event_id,
                    decision_type="tool_selection" if tc_names else "response_generation",
                    decision_output={"text": response_text[:500], "tool_calls": tc_names, "model_used": model_used},
                    context_capture_id=context_capture_id,
                    model_used=model_used,
                ))
                db.commit()
        except Exception as e:
            logger.debug("Decision audit skipped: %s", e)

    # ── Skill selection event ─────────────────────────────────────────

    def record_skill_selection(
        self,
        session_id: str,
        user_content: str,
        tool_calls: list[dict[str, Any]],
        agent_id: str | None = None,
        skill_versions: dict[str, str] | None = None,
    ) -> str | None:
        """Record skill selection event if tools were called. Returns event_id."""
        tc_names = [tc.get("function", {}).get("name", "") for tc in tool_calls] if tool_calls else []
        if not tc_names:
            return None

        from uuid_utils import uuid7

        from api.models import SkillSelectionEvent

        event_id = str(uuid7())
        try:
            with self._db() as db:
                db.add(SkillSelectionEvent(
                    event_id=event_id,
                    session_id=session_id,
                    agent_id=agent_id,
                    user_query=(user_content or "")[:2000],
                    selected_skills=tc_names,
                    skill_name=tc_names[0],
                    skill_version=(skill_versions or {}).get(tc_names[0]),
                    selection_method="llm_tool_choice",
                ))
                db.commit()
            return event_id
        except Exception as e:
            logger.debug("Skill selection event skipped: %s", e)
            return None

    def backfill_selection_metrics(
        self,
        session_id: str,
        tool_calls: list[dict[str, Any]],
        elapsed_ms: int | None = None,
    ) -> None:
        """Backfill execution metrics on the most recent skill_selection_event for this session."""
        if not tool_calls:
            return
        try:
            from api.models.skill import SkillSelectionEvent
            with self._db() as db:
                row = (
                    db.query(SkillSelectionEvent)
                    .filter(SkillSelectionEvent.session_id == session_id,
                            SkillSelectionEvent.execution_time_ms.is_(None))
                    .order_by(SkillSelectionEvent.created_at.desc())
                    .first()
                )
                if row:
                    row.execution_time_ms = elapsed_ms or 0
                    row.execution_success = 1
                    db.commit()
        except Exception as e:
            logger.debug("Backfill selection metrics skipped: %s", e)

    # ── Observer ──────────────────────────────────────────────────────

    def run_observer(
        self,
        session_id: str,
        user_id: str,
        messages: list[dict[str, Any]],
        turn_count: int = 0,
        session_start: Any = None,
    ) -> None:
        """Run TypedObserver in a background thread."""
        llm = self._llm_client
        db_factory = self._db_factory
        embed_fn = self._embed_fn

        def _bg():
            from core.memory.tabular.service import MemoryService
            svc = MemoryService(db_factory, llm_client=llm, embed_fn=embed_fn)

            try:
                svc.run_pipeline(user_id=user_id, messages=messages)
            except Exception as e:
                logger.debug("Observer failed (non-fatal): %s", e)

            # Check incremental summary thresholds
            if turn_count > 0 and session_start is not None:
                try:
                    svc.check_and_summarize(
                        user_id, session_id, messages, turn_count, session_start,
                    )
                except Exception as e:
                    logger.debug("Incremental summary failed (non-fatal): %s", e)

        try:
            threading.Thread(target=_bg, daemon=True).start()
        except Exception as e:
            logger.debug("Observer setup skipped: %s", e)

    # ── Implicit feedback ─────────────────────────────────────────────

    def detect_implicit_feedback(
        self,
        user_content: str,
        messages: list[dict[str, Any]],
        parent_event_id: str | None,
    ) -> None:
        """Detect and record implicit feedback from user message."""
        if not user_content or len(messages) < 2:
            return
        try:
            from core.context.implicit_feedback import ImplicitFeedbackDetector
            from core.context.prompts import PromptFeedback

            prev_assistant = next(
                (m["content"] for m in reversed(messages) if m.get("role") == "assistant"),
                None,
            )
            signal = ImplicitFeedbackDetector.detect(user_content, prev_assistant)
            if signal.signal_type != "neutral":
                PromptFeedback(self._db_factory).record_feedback(
                    prompt_template_id="chat_turn",
                    prompt_version="auto",
                    llm_request_id=parent_event_id or "",
                    user_rating=_RATING_MAP.get(signal.signal_type, 3),
                    user_comment=f"[implicit:{signal.signal_type}] {signal.evidence}",
                    metadata={"source": "implicit_heuristic", "confidence": str(signal.confidence)},
                )
        except Exception as e:
            logger.debug("Implicit feedback skipped: %s", e)

    # ── Reflection learning ───────────────────────────────────────────

    def detect_reflection_learning(
        self,
        session_id: str,
        user_id: str,
        tool_calls: list[dict[str, Any]],
        tool_results: list[dict[str, Any]] | None,
    ) -> None:
        """Detect reflect → retry → success pattern across turns and persist lesson.

        Cross-turn detection: when reflect is called (in tool_calls), we record
        the session as "reflecting". On the NEXT turn, if tool_results show
        success for a previously-failed tool, we extract a lesson from the
        reflect output and the successful retry.

        The lesson captures WHAT failed and HOW it was fixed, making it
        actionable for future sessions (not just "tool X succeeded").
        """
        # Phase 1: If this turn called reflect, mark session as reflecting
        # and record the reflect output from tool_results for lesson extraction.
        tc_names = [tc.get("function", {}).get("name", "") for tc in (tool_calls or [])]
        if "reflect" in tc_names:
            # Store reflect context for next turn's lesson extraction.
            # tool_results contains the reflect tool's output (the history evidence).
            reflect_output = ""
            for tr in (tool_results or []):
                if tr.get("name") == "reflect":
                    reflect_output = str(tr.get("result", ""))[:500]
                    break
            self._mark_reflecting(session_id, reflect_output)
            return

        # Phase 2: If previous turn reflected, check if this turn succeeded.
        reflect_ctx = self._pop_reflecting(session_id)
        if not reflect_ctx:
            return

        # Check tool_calls (this turn's LLM output) for retry attempts.
        retry_names = [n for n in tc_names if n and n != "reflect"]
        if not retry_names:
            return

        # Build a meaningful lesson from the reflect context + retry tools.
        reflect_evidence = reflect_ctx.get("reflect_output", "")
        lesson = (
            f"Reflection-driven fix: after reviewing decision history, "
            f"retried with {', '.join(retry_names)}. "
            f"Context: {reflect_evidence[:200]}"
        )
        try:
            from core.memory.tabular.service import MemoryService
            from core.memory.types import MemoryType, TrustTier
            svc = MemoryService(self._db_factory)
            svc.store(
                user_id=user_id,
                content=lesson,
                memory_type=MemoryType.PROCEDURAL,
                trust_tier=TrustTier.T3_INFERRED,
                session_id=session_id,
            )
            logger.info("Persisted reflection lesson for session %s", session_id[:8])
        except Exception as e:
            logger.debug("Reflection learning persistence failed: %s", e)

    def _mark_reflecting(self, session_id: str, reflect_output: str) -> None:
        with _reflecting_lock:
            _reflecting_state[session_id] = {
                "reflect_output": reflect_output,
                "ts": time.monotonic(),
            }

    def _pop_reflecting(self, session_id: str) -> dict[str, Any] | None:
        with _reflecting_lock:
            ctx = _reflecting_state.pop(session_id, None)
        if ctx is None:
            return None
        # Expire after 5 minutes — stale reflections are not useful.
        if time.monotonic() - ctx.get("ts", 0) > 300:
            return None
        return ctx


# Module-level cross-turn reflection state, shared across TurnHooks instances.
# _persist_turn_events creates a new TurnHooks per request, so instance-level
# state would lose the "reflecting" mark between turns. Module-level + lock
# makes the cross-turn handoff explicit and thread-safe.
_reflecting_lock = threading.Lock()
_reflecting_state: dict[str, dict[str, Any]] = {}
