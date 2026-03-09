"""ReflectService — diagnostic evidence builder for the reflect endpoint."""

import json
from typing import Any, Callable, Literal

from core.db_consumer import DbConsumer, DbFactory
from core.logging_config import get_logger

logger = get_logger(__name__)

_ReflectFocus = Literal[
    "auto", "skill_failure", "unexpected_result",
    "data_quality", "tool_selection", "history", "performance",
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
        original_focus = focus
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
                    EventModel.event_type, content_col,
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
                evt: dict[str, Any] = {"type": r[0], "ts": str(r[2]) if r[2] else None}
                if r[3]:
                    evt["model"] = r[3]
                if r[4]:
                    evt["skill"] = r[4]

                if r[0] == "llm_response" and r[5]:
                    usage = r[5] if isinstance(r[5], dict) else {}
                    try:
                        if isinstance(r[5], str):
                            usage = json.loads(r[5])
                    except (json.JSONDecodeError, TypeError):
                        usage = {}
                    p = usage.get("prompt_tokens", usage.get("prompt", 0)) or 0
                    c = usage.get("completion_tokens", usage.get("completion", 0)) or 0
                    total_prompt += p
                    total_completion += c
                    llm_calls += 1
                    model = r[3] or "unknown"
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
                from core.memory import create_memory_service
                from core.memory.types import MemoryType

                svc = create_memory_service(self._db_factory, user_id=user_id)
                # Only list_active() is called — no llm_client/embed_fn needed
                # (those are only required for observe/pipeline paths).
                memories = svc.list_active(user_id, MemoryType.PROCEDURAL, limit=5, load_embedding=False)
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

            # 6. Tool selection — always collected (focus only affects hints)
            self._gather_tool_selection(session_id, question, db, hints, result)

            # 7. Cross-session history — always collected
            self._gather_history(session_id, user_id, question, db, result)

            # 8. Token summary with tool vs non-tool breakdown
            # Query ctx_snapshots for token budget breakdown
            tool_tokens_total = 0
            non_tool_tokens_total = 0
            context_budgets: list[dict[str, Any]] = []
            try:
                from sqlalchemy import text as sql_text
                budget_rows = db.execute(
                    sql_text("""
                        SELECT token_budget, created_at FROM ctx_snapshots
                        WHERE session_id = :sid
                        ORDER BY created_at DESC LIMIT :n
                    """),
                    {"sid": session_id, "n": last_n},
                ).fetchall()
                for budget_json, snap_ts in budget_rows:
                    if budget_json:
                        budget = json.loads(budget_json) if isinstance(budget_json, str) else budget_json
                        tool_tokens_total += budget.get("tool_schemas", 0)
                        non_tool_tokens_total += sum(
                            v for k, v in budget.items()
                            if k != "tool_schemas" and isinstance(v, (int, float))
                        )
                        context_budgets.append({"ts": str(snap_ts), **budget})
            except Exception:
                pass

            total_managed = tool_tokens_total + non_tool_tokens_total
            result["token_summary"] = {
                "total_prompt_tokens": total_prompt,
                "total_completion_tokens": total_completion,
                "total_tokens": total_prompt + total_completion,
                "llm_calls": llm_calls,
                "tool_tokens": tool_tokens_total,
                "non_tool_tokens": non_tool_tokens_total,
                "tool_ratio": round(tool_tokens_total / total_managed, 2) if total_managed > 0 else 0,
                "by_model": {
                    model: {"prompt_tokens": v["prompt"], "completion_tokens": v["completion"], "calls": v["calls"]}
                    for model, v in cost_by_model.items()
                },
            }
            if context_budgets:
                result["context_budgets"] = context_budgets
            if total_managed > 0 and tool_tokens_total / total_managed > 0.6:
                hints.append(f"Tool schemas consuming {tool_tokens_total / total_managed:.0%} of managed context — consider enabling high-confidence selection")

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

        # 10. Session performance analysis (timeline, gaps, root cause)
        if original_focus in ("performance", "auto"):
            try:
                from core.agent.session_analyzer import SessionAnalyzer
                analyzer = SessionAnalyzer(self._db_factory)
                report = analyzer.analyze(session_id)
                result["session_report"] = report.to_dict()
                result["session_report_markdown"] = report.to_markdown()
                # Merge analyzer issues into hints
                for issue in report.issues:
                    hints.append(f"[{issue['type']}] {issue['description']}")
                result["diagnosis_hints"] = hints
            except Exception:
                logger.debug("Session analysis failed", exc_info=True)

        return self._compact_output(result)

    # ------------------------------------------------------------------
    # Output compaction
    # ------------------------------------------------------------------

    _OUTPUT_BUDGET_CHARS = 3000

    @staticmethod
    def _compact_output(result: dict[str, Any]) -> dict[str, Any]:
        """Reduce output size to fit token budget.

        Structural rules (not ad-hoc trimming):
        1. event_summary: exclude reflect's own events (prevent recursion bloat)
        2. cloud_skills: drop parameters, truncate description
        3. edge_tools: names only
        4. session_report_markdown: drop (redundant with session_report)
        """
        # 1. Filter out reflect's own events from event_summary
        evts = result.get("event_summary", [])
        result["event_summary"] = [
            e for e in evts
            if e.get("skill") != "reflect" and e.get("tool_name") != "reflect"
        ]

        # 2. Compact cloud_skills: name + short description only
        for skill in result.get("cloud_skills", []):
            skill.pop("parameters", None)
            desc = skill.get("description", "")
            if len(desc) > 80:
                skill["description"] = desc[:80] + "…"

        # 3. Compact edge_tools: names only
        tools = result.get("edge_tools", [])
        if tools:
            result["edge_tools"] = [t.get("name", "?") for t in tools]

        return result

    # ------------------------------------------------------------------
    # Private helpers
    # ------------------------------------------------------------------

    def _gather_tool_selection(
        self, session_id: str, question: str, db: Any,
        hints: list[str], result: dict[str, Any],
    ) -> None:
        """Cloud skills, edge tools, and usage counts.

        To avoid bloating the response when hundreds of skills exist,
        only return full details for: (1) skills used in this session,
        (2) skills matching the question, (3) up to 10 unused skills.
        The rest are summarized as a count.
        """
        # 1. Collect usage counts first — we need them to filter
        from api.models.agent import Event as EventModel
        from sqlalchemy import func as sa_func

        usage_rows = (
            db.query(sa_func.json_unquote(sa_func.json_extract(EventModel.content, "$.name")))
            .filter(EventModel.session_id == session_id, EventModel.event_type == "tool_call")
            .order_by(EventModel.created_at.desc()).limit(50).all()
        )
        tool_usage: dict[str, int] = {}
        for (name,) in usage_rows:
            n = name or "unknown"
            tool_usage[n] = tool_usage.get(n, 0) + 1
        result["tool_usage_counts"] = tool_usage
        used_names = set(tool_usage)

        # 2. Build full skill index (name-deduped)
        _MAX_UNUSED_DETAIL = 10
        question_words = [w.lower() for w in (question or "").split() if len(w) > 2]

        try:
            all_skills: dict[str, dict[str, Any]] = {}
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
                    if skill.name in all_skills:
                        continue
                    all_skills[skill.name] = {
                        "name": skill.name,
                        "description": skill.description,
                        "parameters": skill.to_openai_schema().get("function", {}).get("parameters", {}),
                    }

            # 3. Partition: used / question-relevant / other
            cloud_skills: list[dict[str, Any]] = []
            other_names: list[str] = []

            for name, info in all_skills.items():
                if name in used_names:
                    cloud_skills.append(info)
                elif question_words and any(w in name.lower() or w in (info["description"] or "").lower()
                                            for w in question_words):
                    cloud_skills.append(info)
                else:
                    other_names.append(name)

            # Include up to N unused skills with full detail
            for name in other_names[:_MAX_UNUSED_DETAIL]:
                cloud_skills.append(all_skills[name])

            omitted = len(other_names) - _MAX_UNUSED_DETAIL
            result["cloud_skills"] = cloud_skills
            result["cloud_skills_total"] = len(all_skills)
            if omitted > 0:
                result["cloud_skills_omitted"] = omitted

        except Exception:
            logger.debug("Failed to load cloud skills for tool_selection", exc_info=True)
            result["cloud_skills"] = []

        entry = self._peek_session(session_id)
        result["edge_tools"] = [
            {"name": t.get("function", {}).get("name", "?"),
             "description": t.get("function", {}).get("description", "")[:80]}
            for t in (entry.get("tools", []) if entry else [])
        ]

        unused = {s["name"] for s in result.get("cloud_skills", [])} - used_names
        if unused and len(unused) <= 20:
            hints.append(f"Cloud skills available but never called: {', '.join(sorted(unused))}")
        elif unused:
            hints.append(f"{len(unused)} cloud skills available but never called in this session")

        if question_words:
            for s in result.get("cloud_skills", []):
                if any(w in s["name"] for w in question_words):
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
