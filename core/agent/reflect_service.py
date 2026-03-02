"""ReflectService — diagnostic evidence builder for the reflect endpoint."""

import json
from typing import Any, Callable, Literal

from core.db_consumer import DbConsumer, DbFactory
from core.logging_config import get_logger

logger = get_logger(__name__)

_ReflectFocus = Literal[
    "auto", "skill_failure", "unexpected_result",
    "data_quality", "tool_selection", "history",
]


def _escape_like(text: str) -> str:
    """Escape LIKE wildcards (%, _) in user-supplied text."""
    return text.replace("\\", "\\\\").replace("%", "\\%").replace("_", "\\_")


class ReflectService(DbConsumer):
    """Build diagnostic evidence for a session.

    Parameters
    ----------
    db_factory : DbFactory
        Callable returning a SQLAlchemy Session.
    skill_registry : optional
        SkillRegistry (or compatible) for cloud skill listing.
    peek_session : optional
        Callable(session_id) → dict|None for reading cached edge tools.
    """

    def __init__(
        self,
        db_factory: DbFactory,
        skill_registry: Any = None,
        peek_session: Callable[[str], dict[str, Any] | None] | None = None,
    ):
        super().__init__(db_factory)
        self._registry = skill_registry
        self._peek_session = peek_session or (lambda _: None)

    # ------------------------------------------------------------------
    # Public API
    # ------------------------------------------------------------------

    def build_evidence(
        self,
        session_id: str,
        user_id: str,
        focus: _ReflectFocus,
        last_n: int,
        question: str = "",
    ) -> dict[str, Any]:
        """Unified diagnostic evidence: events, skill decisions, tool selection, history."""
        result: dict[str, Any] = {"session_id": session_id, "focus": focus}
        hints: list[str] = []

        with self._db() as db:
            # 1. Event trail
            from api.models.agent import Event as EventModel
            from sqlalchemy import case

            content_col = case(
                (EventModel.event_type.in_(("tool_call", "tool_result")), EventModel.content),
                else_=None,
            ).label("content")

            rows = (
                db.query(
                    EventModel.event_type, content_col, EventModel.event_metadata,
                    EventModel.created_at, EventModel.llm_model_used, EventModel.skill_name,
                    EventModel.token_usage,
                )
                .filter(EventModel.session_id == session_id)
                .order_by(EventModel.created_at.desc())
                .limit(int(last_n))
                .all()
            )

            events: list[dict[str, Any]] = []
            fail_counts: dict[str, int] = {}
            total_prompt = 0
            total_completion = 0
            llm_calls = 0
            cost_by_model: dict[str, dict[str, int]] = {}

            for r in reversed(rows):
                evt: dict[str, Any] = {"type": r[0], "ts": str(r[3]) if r[3] else None}
                if r[4]:
                    evt["model"] = r[4]
                if r[5]:
                    evt["skill"] = r[5]

                if r[0] == "llm_response" and r[6]:
                    usage = r[6] if isinstance(r[6], dict) else {}
                    try:
                        if isinstance(r[6], str):
                            usage = json.loads(r[6])
                    except (json.JSONDecodeError, TypeError):
                        usage = {}
                    p = usage.get("prompt_tokens", usage.get("prompt", 0)) or 0
                    c = usage.get("completion_tokens", usage.get("completion", 0)) or 0
                    total_prompt += p
                    total_completion += c
                    llm_calls += 1
                    model = r[4] or "unknown"
                    entry = cost_by_model.setdefault(model, {"prompt": 0, "completion": 0, "calls": 0})
                    entry["prompt"] += p
                    entry["completion"] += c
                    entry["calls"] += 1

                if r[0] == "tool_result" and r[1]:
                    try:
                        content = json.loads(r[1])
                        evt["tool_name"] = content.get("name", "")
                        result_str = str(content.get("result", ""))[:200]
                        evt["result_preview"] = result_str
                        if "Error" in result_str or "error" in result_str:
                            evt["failed"] = True
                            name = content.get("name", "unknown")
                            fail_counts[name] = fail_counts.get(name, 0) + 1
                    except (json.JSONDecodeError, TypeError):
                        pass
                elif r[0] == "tool_call" and r[1]:
                    try:
                        content = json.loads(r[1])
                        evt["tool_name"] = content.get("name", "")
                    except (json.JSONDecodeError, TypeError):
                        pass
                events.append(evt)
            result["event_summary"] = events

            # Auto-detect focus
            if focus == "auto":
                has_failure = any(e.get("failed") for e in events)
                has_missing_provenance = any(
                    e.get("type") == "tool_result" and e.get("result_preview")
                    and "data_source" not in e.get("result_preview", "")
                    for e in events
                )
                if has_failure:
                    focus = "skill_failure"
                elif has_missing_provenance:
                    focus = "data_quality"
                else:
                    focus = "unexpected_result"
                result["focus"] = focus

            for name, count in fail_counts.items():
                if count >= 2:
                    hints.append(f"Skill '{name}' failed {count} times in this session")

            # 2. Skill selection history
            from api.models.skill import SkillSelectionEvent

            sel_rows = (
                db.query(
                    SkillSelectionEvent.skill_name, SkillSelectionEvent.selected_skills,
                    SkillSelectionEvent.selection_reasoning, SkillSelectionEvent.execution_success,
                    SkillSelectionEvent.execution_time_ms, SkillSelectionEvent.created_at,
                )
                .filter(SkillSelectionEvent.session_id == session_id)
                .order_by(SkillSelectionEvent.created_at.desc())
                .limit(5)
                .all()
            )
            result["skill_history"] = [
                {
                    "skill": r[0], "selected": r[1], "reasoning": (r[2] or "")[:200],
                    "success": bool(r[3]) if r[3] is not None else None,
                    "time_ms": r[4], "ts": str(r[5]) if r[5] else None,
                }
                for r in sel_rows
            ]

            # 3. Past lessons
            try:
                from core.memory.store import MemoryStore
                from core.memory.types import MemoryType

                store = MemoryStore(self._db_factory)
                memories = store.list_active(user_id, MemoryType.PROCEDURAL, limit=5)
                result["past_lessons"] = [m.content for m in memories]
                for m in memories:
                    for name in fail_counts:
                        if name in m.content:
                            hints.append(f"Past lesson matches: {m.content[:150]}")
                            break
            except Exception:
                result["past_lessons"] = []

            # 4. Implicit feedback signals
            try:
                from api.models.context import PromptFeedback

                subq = (
                    db.query(EventModel.event_id)
                    .filter(EventModel.session_id == session_id, EventModel.event_type == "user_query")
                    .subquery()
                )
                fb_rows = (
                    db.query(PromptFeedback.user_comment, PromptFeedback.created_at)
                    .filter(PromptFeedback.llm_request_id.in_(subq))
                    .order_by(PromptFeedback.created_at.desc())
                    .limit(5)
                    .all()
                )
                result["feedback_signals"] = [
                    {"signal": r[0], "ts": str(r[1]) if r[1] else None}
                    for r in fb_rows
                ]
            except Exception:
                result["feedback_signals"] = []

            # 5. Data quality hints
            for evt in events:
                if evt.get("type") == "tool_result" and evt.get("result_preview"):
                    preview = evt.get("result_preview", "")
                    if "data_source" not in preview and evt.get("tool_name"):
                        hints.append(f"Tool '{evt['tool_name']}' result has no data_source provenance")
                        break

            # 6. Tool selection
            if focus in ("tool_selection", "auto"):
                self._gather_tool_selection(session_id, question, db, hints, result)

            # 7. Cross-session history
            if focus in ("history", "auto"):
                self._gather_history(session_id, user_id, question, db, result)

            # 8. Token summary
            result["token_summary"] = {
                "total_prompt_tokens": total_prompt,
                "total_completion_tokens": total_completion,
                "total_tokens": total_prompt + total_completion,
                "llm_calls": llm_calls,
                "by_model": {
                    model: {"prompt_tokens": v["prompt"], "completion_tokens": v["completion"], "calls": v["calls"]}
                    for model, v in cost_by_model.items()
                },
            }

            # 9. Tool quality summary
            try:
                tq_rows = (
                    db.query(EventModel.event_metadata)
                    .filter(
                        EventModel.session_id == session_id,
                        EventModel.event_type == "tool_result_quality",
                    )
                    .order_by(EventModel.created_at.desc())
                    .limit(20)
                    .all()
                )
                quality_items = []
                for (meta,) in tq_rows:
                    if not meta:
                        continue
                    m = meta if isinstance(meta, dict) else {}
                    try:
                        if isinstance(meta, str):
                            m = json.loads(meta)
                    except (json.JSONDecodeError, TypeError):
                        continue
                    grade = m.get("quality_grade", "")
                    if grade and grade != "complete":
                        quality_items.append({
                            "tool": m.get("tool_name", "unknown"),
                            "grade": grade,
                            "score": m.get("quality_score"),
                            "missing_fields": m.get("missing_fields", []),
                        })
                result["tool_quality_summary"] = quality_items
            except Exception:
                result["tool_quality_summary"] = []

            if total_prompt > 50000:
                hints.append(f"High token usage: {total_prompt + total_completion:,} total tokens across {llm_calls} LLM calls")

        result["diagnosis_hints"] = hints
        return result

    # ------------------------------------------------------------------
    # Private helpers
    # ------------------------------------------------------------------

    def _gather_tool_selection(
        self, session_id: str, question: str, db: Any,
        hints: list[str], result: dict[str, Any],
    ) -> None:
        """Cloud skills, edge tools, and usage counts."""
        try:
            cloud_skills: list[dict[str, Any]] = []
            seen_skills: set[str] = set()
            if self._registry:
                skills_iter = getattr(self._registry, 'list_skills', None)
                if callable(skills_iter):
                    try:
                        skills = skills_iter()
                        if not isinstance(skills, list):
                            raise TypeError
                    except (TypeError, AttributeError):
                        skills = list(self._registry._skills.values())
                else:
                    skills = list(self._registry._skills.values())
                for skill in skills:
                    if skill.name in seen_skills:
                        continue
                    seen_skills.add(skill.name)
                    schema = skill.to_openai_schema()
                    cloud_skills.append({
                        "name": skill.name,
                        "description": skill.description,
                        "parameters": schema.get("function", {}).get("parameters", {}),
                    })
            result["cloud_skills"] = cloud_skills
        except Exception:
            logger.debug("Failed to load cloud skills for tool_selection", exc_info=True)
            result["cloud_skills"] = []

        entry = self._peek_session(session_id)
        result["edge_tools"] = [
            {"name": t.get("function", {}).get("name", "?"),
             "description": t.get("function", {}).get("description", "")[:80]}
            for t in (entry.get("tools", []) if entry else [])
        ]

        from api.models.agent import Event as EventModel

        usage_rows = (
            db.query(EventModel.content)
            .filter(EventModel.session_id == session_id, EventModel.event_type == "tool_call")
            .order_by(EventModel.created_at.desc()).limit(50).all()
        )
        tool_usage: dict[str, int] = {}
        for (c,) in usage_rows:
            try:
                name = json.loads(c).get("name", "unknown") if c else "unknown"
                tool_usage[name] = tool_usage.get(name, 0) + 1
            except (json.JSONDecodeError, TypeError):
                pass
        result["tool_usage_counts"] = tool_usage

        unused = {s["name"] for s in result.get("cloud_skills", [])} - set(tool_usage)
        if unused:
            hints.append(f"Cloud skills available but never called: {', '.join(sorted(unused))}")

        if question:
            for s in result.get("cloud_skills", []):
                if any(w in s["name"] for w in question.lower().split()):
                    hints.append(f"Skill '{s['name']}' params: {json.dumps(s['parameters'])[:200]}")

    def _gather_history(
        self, session_id: str, user_id: str, question: str, db: Any,
        result: dict[str, Any],
    ) -> None:
        """Find similar queries from past sessions via index scan + Python filter."""
        from datetime import datetime, timedelta, timezone
        from api.models.agent import Event as EventModel

        cur_query = db.query(EventModel.content).filter(
            EventModel.session_id == session_id, EventModel.event_type == "user_query",
        ).order_by(EventModel.created_at.desc()).first()
        cur_text = (cur_query[0] if cur_query else question) or ""

        if not cur_text:
            result["related_history"] = []
            return

        keywords = [w.lower() for w in cur_text.split() if len(w) > 3][:3]
        if not keywords:
            result["related_history"] = []
            return

        # Index scan on (user_id, event_type, created_at), then Python keyword filter
        cutoff = datetime.now(timezone.utc) - timedelta(days=30)
        candidates = (
            db.query(EventModel.session_id, EventModel.content, EventModel.created_at)
            .filter(
                EventModel.user_id == user_id,
                EventModel.event_type == "user_query",
                EventModel.session_id != session_id,
                EventModel.created_at >= cutoff,
            )
            .order_by(EventModel.created_at.desc())
            .limit(100)
            .all()
        )

        matched = []
        for r in candidates:
            text_lower = (r[1] or "").lower()
            if all(kw in text_lower for kw in keywords):
                matched.append({"session_id": r[0], "query": (r[1] or "")[:200], "ts": str(r[2])})
                if len(matched) >= 5:
                    break

        result["related_history"] = matched
