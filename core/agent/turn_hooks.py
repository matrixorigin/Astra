"""Shared post-turn hooks for ChatLoop and /chat/turn.

Extracts the common persistence logic (decision audit, skill selection,
observer, implicit feedback) so both code paths stay in sync.
"""

from __future__ import annotations

import logging
import threading
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

    def record_decision_audit(
        self,
        session_id: str,
        event_id: str,
        tool_calls: list[dict[str, Any]],
        response_text: str,
        context_capture_id: str | None,
        model_used: str | None = None,
    ) -> None:
        """Record a decision audit entry."""
        from api.models import DecisionAudit
        from uuid_utils import uuid7

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
    ) -> None:
        """Record skill selection event if tools were called."""
        tc_names = [tc.get("function", {}).get("name", "") for tc in tool_calls] if tool_calls else []
        if not tc_names:
            return

        from api.models import SkillSelectionEvent
        from uuid_utils import uuid7

        try:
            with self._db() as db:
                db.add(SkillSelectionEvent(
                    event_id=str(uuid7()),
                    session_id=session_id,
                    user_query=(user_content or "")[:2000],
                    selected_skills=tc_names,
                    skill_name=tc_names[0],
                    selection_method="llm_tool_choice",
                ))
                db.commit()
        except Exception as e:
            logger.debug("Skill selection event skipped: %s", e)

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
        from core.memory.typed_pipeline import run_typed_memory_pipeline

        llm = self._llm_client
        db_factory = self._db_factory
        embed_fn = self._embed_fn

        def _bg():
            try:
                run_typed_memory_pipeline(
                    db_factory=db_factory,
                    user_id=user_id,
                    messages=messages,
                    llm_client=llm,
                    embed_fn=embed_fn,
                )
            except Exception as e:
                logger.debug("Observer failed (non-fatal): %s", e)

            # Check incremental summary thresholds
            if turn_count > 0 and session_start is not None:
                try:
                    from core.memory.store import MemoryStore
                    from core.memory.session_summary import SessionSummarizer
                    store = MemoryStore(db_factory)
                    summarizer = SessionSummarizer(store, llm_client=llm, embed_fn=embed_fn)
                    summarizer.check_and_summarize(
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
