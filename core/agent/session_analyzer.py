"""Session Analyzer — produces human-readable diagnostic reports from event data.

Consumes raw events from the DB and produces structured timeline, gap detection,
root cause analysis, and actionable recommendations.
"""

import json
from dataclasses import dataclass, field
from datetime import datetime
from typing import Any

from sqlalchemy import text

from core.db_consumer import DbConsumer, DbFactory
from core.logging_config import get_logger

logger = get_logger(__name__)

GAP_THRESHOLD_S = 10  # Flag gaps longer than this


@dataclass
class TimelineEntry:
    ts: datetime
    event_type: str
    detail: str
    skill: str | None = None
    gap_s: float | None = None  # Gap from previous event


@dataclass
class SessionReport:
    session_id: str
    timeline: list[TimelineEntry]
    total_duration_s: float
    issues: list[dict[str, Any]]
    recommendations: list[str]
    stats: dict[str, Any]

    def to_markdown(self) -> str:
        lines = [f"## Session Analysis: `{self.session_id[:12]}…`", ""]

        # Timeline
        lines.append("### Timeline")
        lines.append("")
        lines.append("| Time | Event | Detail | Gap |")
        lines.append("|---|---|---|---|")
        for e in self.timeline:
            gap = ""
            if e.gap_s is not None and e.gap_s >= GAP_THRESHOLD_S:
                gap = f"⚠️ {e.gap_s:.0f}s"
            ts = e.ts.strftime("%H:%M:%S")
            lines.append(f"| {ts} | {e.event_type} | {e.detail} | {gap} |")
        lines.append("")

        # Issues
        if self.issues:
            lines.append("### Issues Found")
            lines.append("")
            for i, issue in enumerate(self.issues, 1):
                lines.append(f"{i}. **{issue['type']}**: {issue['description']}")
            lines.append("")

        # Recommendations
        if self.recommendations:
            lines.append("### Recommendations")
            lines.append("")
            for r in self.recommendations:
                lines.append(f"- {r}")
            lines.append("")

        # Stats
        lines.append("### Stats")
        lines.append("")
        for k, v in self.stats.items():
            lines.append(f"- {k}: {v}")

        return "\n".join(lines)

    def to_dict(self) -> dict[str, Any]:
        return {
            "session_id": self.session_id,
            "total_duration_s": self.total_duration_s,
            "timeline": [
                {"ts": str(e.ts), "type": e.event_type, "detail": e.detail,
                 "skill": e.skill, "gap_s": e.gap_s}
                for e in self.timeline
            ],
            "issues": self.issues,
            "recommendations": self.recommendations,
            "stats": self.stats,
        }


class SessionAnalyzer(DbConsumer):
    """Analyze a session and produce a diagnostic report."""

    def analyze(self, session_id: str) -> SessionReport:
        with self._db() as db:
            rows = db.execute(text(
                "SELECT event_id, event_type, content, skill_name, created_at, agent_id, metadata "
                "FROM agent_events WHERE session_id = :sid ORDER BY created_at"
            ), {"sid": session_id}).fetchall()

        if not rows:
            return SessionReport(
                session_id=session_id, timeline=[], total_duration_s=0,
                issues=[{"type": "empty", "description": "No events found"}],
                recommendations=[], stats={},
            )

        timeline: list[TimelineEntry] = []
        issues: list[dict[str, Any]] = []
        recommendations: list[str] = []
        prev_ts: datetime | None = None
        tool_call_counts: dict[str, int] = {}
        error_count = 0
        cloud_loop_count = 0

        for r in rows:
            event_id, event_type, content, skill_name, ts, agent_id, metadata = r

            # Compute gap
            gap_s = None
            if prev_ts:
                gap_s = (ts - prev_ts).total_seconds()

            # Build detail
            detail = self._summarize_event(event_type, content, skill_name)

            timeline.append(TimelineEntry(
                ts=ts, event_type=event_type, detail=detail,
                skill=skill_name, gap_s=gap_s,
            ))

            # Detect issues
            if gap_s is not None and gap_s >= GAP_THRESHOLD_S:
                issues.append({
                    "type": "slow_gap",
                    "description": f"{gap_s:.0f}s gap before {event_type}"
                                   + (f" ({skill_name})" if skill_name else ""),
                    "ts": str(ts),
                    "gap_s": gap_s,
                })

            if event_type == "tool_result" and content:
                try:
                    data = json.loads(content)
                    result_str = str(data.get("result", ""))
                    if "Malformed" in result_str or "Error" in result_str or "error" in result_str:
                        error_count += 1
                        name = data.get("name", "unknown")
                        issues.append({
                            "type": "tool_error",
                            "description": f"{name} returned error: {result_str[:150]}",
                            "ts": str(ts),
                        })
                except (json.JSONDecodeError, TypeError):
                    pass

            if event_type == "tool_call":
                # Detect cloud skill loops
                try:
                    data = json.loads(content) if content else {}
                    source = data.get("source", "")
                    name = data.get("name", skill_name or "unknown")
                except (json.JSONDecodeError, TypeError):
                    source = ""
                    name = skill_name or "unknown"
                tool_call_counts[name] = tool_call_counts.get(name, 0) + 1
                if source == "cloud":
                    cloud_loop_count += 1

            prev_ts = ts

        # Total duration
        total_s = (rows[-1][4] - rows[0][4]).total_seconds() if len(rows) > 1 else 0

        # Generate recommendations
        if cloud_loop_count >= 3:
            issues.append({
                "type": "cloud_loop_storm",
                "description": f"{cloud_loop_count} cloud skill calls in one session — "
                               "LLM may be over-elaborating",
            })
            recommendations.append(
                "Consider adding a cloud loop budget or early-exit when LLM "
                "has already produced a satisfactory answer"
            )

        if error_count > 0:
            recommendations.append(
                f"{error_count} tool error(s) detected — check argument validation "
                "and _try_repair_tool_args coverage"
            )

        slow_gaps = [i for i in issues if i["type"] == "slow_gap"]
        if slow_gaps:
            max_gap = max(i["gap_s"] for i in slow_gaps)
            if max_gap > 60:
                recommendations.append(
                    f"Largest gap is {max_gap:.0f}s — likely LLM inference latency. "
                    "Consider model routing to a faster model for simple queries"
                )

        # Stats
        stats = {
            "total_events": len(rows),
            "total_duration": f"{total_s:.0f}s",
            "tool_calls": tool_call_counts,
            "cloud_skill_calls": cloud_loop_count,
            "errors": error_count,
        }

        return SessionReport(
            session_id=session_id,
            timeline=timeline,
            total_duration_s=total_s,
            issues=issues,
            recommendations=recommendations,
            stats=stats,
        )

    @staticmethod
    def _summarize_event(event_type: str, content: str | None, skill: str | None) -> str:
        if event_type == "user_query":
            return (content or "")[:80]
        if event_type == "llm_response":
            return (content or "")[:80] or "(empty)"
        if event_type in ("tool_call", "tool_result"):
            try:
                data = json.loads(content) if content else {}
                name = data.get("name", skill or "?")
                if event_type == "tool_call":
                    return f"→ {name}"
                result = str(data.get("result", ""))[:80]
                return f"← {name}: {result}"
            except (json.JSONDecodeError, TypeError):
                return skill or "?"
        if event_type == "session_history_snapshot":
            return "snapshot"
        return event_type
