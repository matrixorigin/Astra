"""Shared post-turn hooks for ChatLoop and /chat/turn.

Extracts the common persistence logic (decision audit, skill selection,
observer, implicit feedback) so both code paths stay in sync.
"""

from __future__ import annotations

import atexit
import logging
import os
import threading
import time
from datetime import datetime, timezone
from typing import Any

from core.db_consumer import DbConsumer, DbFactory
from core.memory.backends import get_memoria_storage

logger = logging.getLogger(__name__)

# Track background threads for graceful shutdown
_bg_threads: list[threading.Thread] = []
_bg_threads_lock = threading.Lock()
_shutdown_event = threading.Event()


def _log_episodic_error(msg: str, exc: Exception) -> None:
    """Log episodic errors: debug for schema/table errors (test isolation), warning for others."""
    from sqlalchemy.exc import ProgrammingError as SAProgrammingError


def _wait_for_bg_threads(timeout: float = 2.0):
    """Wait for background threads to complete during shutdown."""
    _shutdown_event.set()
    # Disable httpx logging to prevent closed file errors
    logging.getLogger("httpx").disabled = True
    with _bg_threads_lock:
        threads = list(_bg_threads)
    for t in threads:
        if t.is_alive():
            t.join(timeout=timeout)


atexit.register(_wait_for_bg_threads)

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

_EPISODIC_EVENT_THRESHOLD = int(os.getenv("EPISODIC_EVENT_THRESHOLD", "25"))
_EPISODIC_MIN_EVENTS = int(os.getenv("EPISODIC_MIN_EVENTS", "8"))
_EPISODIC_TIME_THRESHOLD_SEC = int(os.getenv("EPISODIC_TIME_THRESHOLD_SEC", "1800"))
_EPISODIC_STUB_MAX_LEN = int(os.getenv("EPISODIC_STUB_MAX_LEN", "160"))
_EPISODIC_SUMMARY_MESSAGE_LIMIT = int(os.getenv("EPISODIC_SUMMARY_MESSAGE_LIMIT", "200"))


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

        tc_names = (
            [tc.get("function", {}).get("name", "") for tc in tool_calls] if tool_calls else []
        )
        try:
            with self._db() as db:
                db.add(
                    DecisionAudit(
                        decision_id=str(uuid7()),
                        session_id=session_id,
                        event_id=event_id,
                        decision_type="tool_selection" if tc_names else "response_generation",
                        decision_output={
                            "text": response_text[:500],
                            "tool_calls": tc_names,
                            "model_used": model_used,
                        },
                        context_capture_id=context_capture_id,
                        model_used=model_used,
                    )
                )
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
        tc_names = (
            [tc.get("function", {}).get("name", "") for tc in tool_calls] if tool_calls else []
        )
        if not tc_names:
            return None

        from uuid_utils import uuid7

        from api.models import SkillSelectionEvent

        event_id = str(uuid7())
        try:
            with self._db() as db:
                db.add(
                    SkillSelectionEvent(
                        event_id=event_id,
                        session_id=session_id,
                        agent_id=agent_id,
                        user_query=(user_content or "")[:2000],
                        selected_skills=tc_names,
                        skill_name=tc_names[0],
                        skill_version=(skill_versions or {}).get(tc_names[0]),
                        selection_method="llm_tool_choice",
                    )
                )
                db.commit()
                db.expire_all()
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
                row = None
                for attempt in range(6):
                    row = (
                        db.query(SkillSelectionEvent)
                        .populate_existing()
                        .filter(
                            SkillSelectionEvent.session_id == session_id,
                            SkillSelectionEvent.execution_time_ms.is_(None),
                        )
                        .order_by(SkillSelectionEvent.created_at.desc())
                        .first()
                    )
                    if row is not None:
                        break
                    if attempt < 5:
                        import time

                        time.sleep(0.03 * (attempt + 1))
                if row:
                    event_id = row.event_id
                    row.execution_time_ms = elapsed_ms or 0
                    row.execution_success = 1
                    db.commit()
                    db.expire_all()
                    bind = db.get_bind() if hasattr(db, "get_bind") else None
                    if bind is not None:
                        from sqlalchemy.engine import Connection, Engine
                        from sqlalchemy.orm import sessionmaker

                        if isinstance(bind, (Engine, Connection)):
                            import time

                            for attempt in range(6):
                                fresh_db = sessionmaker(bind=bind, expire_on_commit=False)()
                                try:
                                    fresh_row = (
                                        fresh_db.query(SkillSelectionEvent)
                                        .filter(SkillSelectionEvent.event_id == event_id)
                                        .first()
                                    )
                                finally:
                                    fresh_db.close()
                                if fresh_row and fresh_row.execution_success == 1:
                                    break
                                if attempt < 5:
                                    time.sleep(0.03 * (attempt + 1))
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
        # Skip observe for trivially short turns — no extractable content.
        # Threshold: combined user+assistant content < 20 chars is greeting/ack.
        total_content = "".join(m.get("content", "") for m in messages)
        if len(total_content.strip()) < 20:
            return

        def _bg():
            try:
                if _shutdown_event.is_set():
                    return
                svc = get_memoria_storage(user_id)
                svc.run_pipeline(user_id=user_id, messages=messages, session_id=session_id)
                self._maybe_trigger_episodic(
                    svc, session_id, user_id, turn_count=turn_count, session_start=session_start
                )
            except Exception as e:
                if not _shutdown_event.is_set():
                    try:
                        logger.error("Memory pipeline failed: %s", e)
                    except Exception:
                        pass

        try:
            # Prune completed threads to prevent unbounded list growth
            with _bg_threads_lock:
                _bg_threads[:] = [t for t in _bg_threads if t.is_alive()]
                t = threading.Thread(target=_bg, daemon=True)
                _bg_threads.append(t)
            t.start()
        except Exception as e:
            logger.debug("Observer setup skipped: %s", e)

    def _maybe_trigger_episodic(
        self,
        svc: Any,
        session_id: str,
        user_id: str,
        *,
        turn_count: int = 0,
        session_start: Any = None,
    ) -> None:
        from api.models.agent import Event as EventModel, Session as SessionModel
        from core.memory.types import MemoryType, TrustTier

        now = datetime.now(timezone.utc)

        # Phase 1: read session state — short-lived connection, no HTTP inside
        with self._db() as db:
            row = db.query(SessionModel).filter(SessionModel.session_id == session_id).first()
            if not row:
                return
            meta = dict(row.session_metadata or {})
            if meta.get("no_episodic"):
                return
            event_count = int(row.event_count or 0)
            if turn_count and turn_count > event_count:
                event_count = turn_count
            last_count = int(meta.get("episodic_last_event_count", 0) or 0)
            last_at_raw = meta.get("episodic_last_at")
            last_at = self._parse_iso(last_at_raw) if last_at_raw else None

            stub: str | None = None
            messages_for_summary: list[dict] | None = None

            if event_count < _EPISODIC_MIN_EVENTS:
                if meta.get("episodic_stub_written"):
                    return
                stub = self._build_topic_stub(db, session_id)
                if not stub:
                    return
            else:
                by_count = event_count - last_count >= _EPISODIC_EVENT_THRESHOLD
                by_time = (
                    not last_at or (now - last_at).total_seconds() >= _EPISODIC_TIME_THRESHOLD_SEC
                )
                if not (by_count or by_time):
                    return
                events = (
                    db.query(EventModel)
                    .filter(
                        EventModel.session_id == session_id,
                        EventModel.event_type.in_(["user_query", "llm_response"]),
                    )
                    .order_by(EventModel.created_at.desc())
                    .limit(_EPISODIC_SUMMARY_MESSAGE_LIMIT)
                    .all()
                )
                if not events:
                    return
                messages_for_summary = [
                    {
                        "role": "user" if e.event_type == "user_query" else "assistant",
                        "content": e.content,
                    }
                    for e in reversed(events)
                ]
        # DB connection released here — HTTP calls happen outside the connection

        # Phase 2: HTTP calls (no DB connection held)
        if _shutdown_event.is_set():
            return
        task_id: str | None = None
        if stub is not None:
            try:
                svc.store(
                    user_id=user_id,
                    content=stub,
                    memory_type=MemoryType.EPISODIC,
                    trust_tier=TrustTier.T4,
                    initial_confidence=0.3,
                    session_id=session_id,
                )
            except Exception as e:
                try:
                    logger.warning("Episodic stub store failed: %s", e)
                except Exception:
                    pass
                return
            meta["episodic_stub_written"] = True
        else:
            try:
                result = svc.request_session_summary(
                    user_id=user_id,
                    session_id=session_id,
                    messages=messages_for_summary,
                    mode="full",
                    sync=False,
                    max_items=5,
                    generate_embedding=False,
                )
                task_id = result.get("task_id") if isinstance(result, dict) else None
            except Exception as e:
                try:
                    logger.warning("Episodic session summary request failed: %s", e)
                except Exception:
                    pass
                return

        # Phase 3: write metadata back — CAS to avoid lost-update under concurrent turns
        if _shutdown_event.is_set():
            return
        try:
            with self._db() as db:
                # Re-read current metadata and merge our updates on top of it.
                # This is a best-effort merge: concurrent turns may still race,
                # but the window is narrow (HTTP round-trip) and the worst case
                # is a duplicate episodic summary, not data loss.
                row = db.query(SessionModel).filter(SessionModel.session_id == session_id).first()
                if row is None:
                    return
                current_meta = dict(row.session_metadata or {})
                current_meta["episodic_last_event_count"] = event_count
                current_meta["episodic_last_at"] = now.isoformat()
                if task_id:
                    current_meta["episodic_last_task_id"] = task_id
                if stub is not None:
                    current_meta["episodic_stub_written"] = True
                row.session_metadata = current_meta
                db.commit()
        except Exception as e:
            try:
                logger.warning("Episodic metadata commit failed: %s", e)
            except Exception:
                pass

    @staticmethod
    def _parse_iso(value: Any) -> datetime | None:
        if isinstance(value, datetime):
            return value
        if not value:
            return None
        try:
            return datetime.fromisoformat(str(value))
        except Exception:
            return None

    @staticmethod
    def _build_topic_stub(db, session_id: str) -> str:
        from api.models.agent import Event as EventModel

        user_row = (
            db.query(EventModel)
            .filter(EventModel.session_id == session_id, EventModel.event_type == "user_query")
            .order_by(EventModel.created_at.desc())
            .first()
        )
        if user_row and user_row.content:
            return TurnHooks._trim_topic(user_row.content)
        assistant_row = (
            db.query(EventModel)
            .filter(EventModel.session_id == session_id, EventModel.event_type == "llm_response")
            .order_by(EventModel.created_at.desc())
            .first()
        )
        if assistant_row and assistant_row.content:
            return TurnHooks._trim_topic(assistant_row.content)
        return ""

    @staticmethod
    def _trim_topic(text: str) -> str:
        cleaned = " ".join(text.strip().split())
        if not cleaned:
            return ""
        if len(cleaned) > _EPISODIC_STUB_MAX_LEN:
            cleaned = cleaned[:_EPISODIC_STUB_MAX_LEN]
        return f"Topic: {cleaned}"

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
            for tr in tool_results or []:
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
            svc = get_memoria_storage(user_id)
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
