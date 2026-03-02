"""Session Analyzer — produces human-readable diagnostic reports from event data.

Consumes raw events from the DB and produces structured timeline, gap detection,
root cause analysis, and actionable recommendations.
"""

import json
from dataclasses import dataclass, field
from datetime import datetime
from typing import Any

from sqlalchemy import text

from core.db_consumer import DbConsumer
from core.logging_config import get_logger

logger = get_logger(__name__)

GAP_THRESHOLD_S = 10  # Flag gaps longer than this
SLOW_NODE_THRESHOLD_S = 10  # Flag individual nodes slower than this
HIGH_TOKEN_THRESHOLD = 5000
EXPENSIVE_THRESHOLD_USD = 0.01
LARGE_CONTEXT_THRESHOLD = 2000

# Model pricing (USD per 1M tokens).
# Keyed by prefix — lookup tries exact match first, then longest prefix match.
MODEL_PRICING: dict[str, dict[str, float]] = {
    "gpt-4o": {"input": 2.50, "output": 10.00},
    "gpt-4o-mini": {"input": 0.15, "output": 0.60},
    "gpt-4-turbo": {"input": 10.00, "output": 30.00},
    "claude-3-5-sonnet": {"input": 3.00, "output": 15.00},
    "claude-3-5-haiku": {"input": 0.80, "output": 4.00},
}


def _calculate_cost(model: str, prompt_tokens: int, completion_tokens: int) -> float:
    """Calculate USD cost for LLM call.

    Tries exact match on MODEL_PRICING, then longest prefix match
    (e.g. "gpt-4o-2024-08-06" matches "gpt-4o").
    Returns 0.0 and logs a warning when no pricing is found.
    """
    pricing = MODEL_PRICING.get(model)
    if not pricing:
        # Longest-prefix fallback (e.g. "gpt-4o-2024-08-06" → "gpt-4o")
        for key in sorted(MODEL_PRICING, key=len, reverse=True):
            if model.startswith(key):
                pricing = MODEL_PRICING[key]
                break
    if not pricing:
        logger.warning("No pricing data for model %r — cost will be 0", model)
        return 0.0
    return (prompt_tokens * pricing["input"] / 1_000_000 +
            completion_tokens * pricing["output"] / 1_000_000)


@dataclass
class ExecutionNode:
    """Detailed execution node with phase-level breakdown."""
    node_id: str
    node_type: str
    event_id: str | None
    ts: datetime
    duration_s: float
    detail: str = ""
    metadata: dict[str, Any] = field(default_factory=dict)
    tokens_in: int | None = None
    tokens_out: int | None = None
    token_breakdown: dict[str, int] | None = None
    cost_usd: float | None = None
    children: list["ExecutionNode"] = field(default_factory=list)
    issues: list[str] = field(default_factory=list)
    parent_duration_pct: float | None = None

    def to_ascii(self, prefix: str = "", is_last: bool = True, depth: int = 0) -> list[str]:
        """Render node and children as detailed ASCII tree."""
        lines = []

        # Connector
        if depth == 0:
            connector = ""
        else:
            connector = "└─ " if is_last else "├─ "

        # Node header
        line = f"{prefix}{connector}"

        # Node type
        if self.node_type in ("prompt_assembly", "model_inference", "tool_execution", "memory_retrieval"):
            line += f"[{self.node_type}]"
        else:
            line += self.node_type

        if self.detail:
            line += f": {self.detail[:60]}"

        # Timing
        if self.duration_s > 0:
            line += f" ({self.duration_s:.2f}s"
            if self.parent_duration_pct is not None:
                line += f", {self.parent_duration_pct:.0f}%"
            line += ")"

        # Tokens
        if self.tokens_in is not None or self.tokens_out is not None:
            line += f" [{self.tokens_in or 0}→{self.tokens_out or 0} tokens]"

        # Cost
        if self.cost_usd is not None and self.cost_usd > 0:
            line += f" ${self.cost_usd:.4f}"

        # Issues
        if self.issues:
            line += " " + " ".join(f"⚠️ {i}" for i in self.issues)

        lines.append(line)

        # Token breakdown (limit depth)
        if self.token_breakdown and depth < 3:
            child_prefix = prefix + ("   " if is_last else "│  ")
            for source, count in sorted(self.token_breakdown.items(), key=lambda x: -x[1]):
                if count > 0:
                    lines.append(f"{child_prefix}├─ {source}: {count:,} tokens")

        # Metadata details
        if self.node_type == "memory_retrieval" and self.metadata:
            child_prefix = prefix + ("   " if is_last else "│  ")
            for phase_name, data in self.metadata.items():
                if isinstance(data, dict) and "hits" in data:
                    hits = data.get("hits", 0)
                    dur = data.get("duration_ms", 0)
                    lines.append(f"{child_prefix}├─ {phase_name}: {hits} hits ({dur:.0f}ms)")

        if self.node_type == "tool_result" and self.metadata:
            child_prefix = prefix + ("   " if is_last else "│  ")
            if "api_latency_ms" in self.metadata:
                lines.append(f"{child_prefix}├─ api_latency: {self.metadata['api_latency_ms']:.0f}ms")
            if "result_size_bytes" in self.metadata:
                kb = self.metadata["result_size_bytes"] / 1024
                lines.append(f"{child_prefix}├─ result_size: {kb:.1f}KB")
            if "result_size_tokens" in self.metadata:
                lines.append(f"{child_prefix}└─ tokens_added: {self.metadata['result_size_tokens']:,}")

        # Children
        child_prefix = prefix + ("   " if is_last else "│  ")
        for i, child in enumerate(self.children):
            is_last_child = (i == len(self.children) - 1)
            lines.extend(child.to_ascii(child_prefix, is_last_child, depth + 1))

        return lines


@dataclass
class ExecutionSummary:
    """Aggregated stats across execution tree."""
    total_duration_s: float
    time_by_category: dict[str, float]
    bottleneck_category: str | None
    total_tokens: int
    tokens_by_source: dict[str, int]
    largest_token_source: str | None
    total_cost_usd: float
    cost_by_turn: dict[int, float]
    root_causes: list[str]


@dataclass
class TimelineEntry:
    ts: datetime
    event_type: str
    detail: str
    skill: str | None = None
    gap_s: float | None = None


@dataclass
class SessionReport:
    session_id: str
    timeline: list[TimelineEntry]
    total_duration_s: float
    issues: list[dict[str, Any]]
    recommendations: list[str]
    stats: dict[str, Any]
    execution_tree: ExecutionNode | None = None
    summary: ExecutionSummary | None = None

    def to_markdown(self) -> str:
        lines = [f"## Session Analysis: `{self.session_id[:12]}…`", ""]

        # Execution Tree (NEW)
        if self.execution_tree:
            lines.append("### Execution Tree")
            lines.append("```")
            lines.extend(self.execution_tree.to_ascii())
            lines.append("```")
            lines.append("")

        # Summary (NEW)
        if self.summary:
            lines.extend(self._render_summary(self.summary))

        # Timeline
        lines.append("### Timeline (Detailed)")
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

    def _render_summary(self, summary: ExecutionSummary) -> list[str]:
        """Render detailed summary with breakdown."""
        lines = ["### SUMMARY", ""]

        # Time breakdown
        lines.append(f"**Total time**: {summary.total_duration_s:.1f}s")
        for category, duration in sorted(summary.time_by_category.items(), key=lambda x: -x[1]):
            pct = (duration / summary.total_duration_s) * 100 if summary.total_duration_s > 0 else 0
            marker = " ⚠️ BOTTLENECK" if category == summary.bottleneck_category else ""
            lines.append(f"  ├─ {category}: {duration:.1f}s ({pct:.0f}%){marker}")
        lines.append("")

        # Token breakdown
        if summary.total_tokens > 0:
            lines.append(f"**Total tokens**: {summary.total_tokens:,}")
            prompt_total = sum(v for k, v in summary.tokens_by_source.items() if k != "completion")
            completion_total = summary.tokens_by_source.get("completion", 0)

            if prompt_total > 0:
                lines.append(f"  ├─ Prompt: {prompt_total:,} ({prompt_total/summary.total_tokens*100:.0f}%)")
                for source, count in sorted(summary.tokens_by_source.items(), key=lambda x: -x[1]):
                    if source == "completion":
                        continue
                    pct = (count / prompt_total) * 100
                    marker = " ⚠️ LARGEST CONTRIBUTOR" if source == summary.largest_token_source else ""
                    lines.append(f"  │   ├─ {source}: {count:,} ({pct:.0f}%){marker}")

            if completion_total > 0:
                lines.append(f"  └─ Completion: {completion_total:,} ({completion_total/summary.total_tokens*100:.0f}%)")
            lines.append("")

        # Cost breakdown
        if summary.total_cost_usd > 0:
            lines.append(f"**Total cost**: ${summary.total_cost_usd:.4f}")
            for turn, cost in sorted(summary.cost_by_turn.items()):
                pct = (cost / summary.total_cost_usd) * 100
                lines.append(f"  ├─ Turn {turn}: ${cost:.4f} ({pct:.0f}%)")
            lines.append("")

        # Root causes
        if summary.root_causes:
            lines.append("**Root causes**:")
            for cause in summary.root_causes:
                lines.append(f"  • {cause}")
            lines.append("")

        return lines

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

        # Build execution tree (NEW)
        execution_tree = None
        summary = None
        try:
            execution_tree = self._build_execution_tree(rows)
            summary = self._build_summary(execution_tree)
        except Exception as e:
            logger.debug(f"Failed to build execution tree: {e}", exc_info=True)

        return SessionReport(
            session_id=session_id,
            timeline=timeline,
            total_duration_s=total_s,
            issues=issues,
            recommendations=recommendations,
            stats=stats,
            execution_tree=execution_tree,
            summary=summary,
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

    def _build_execution_tree(self, rows: list) -> ExecutionNode:
        """Build execution tree from a flat, timestamp-ordered event list.

        The tree structure mirrors the actual agent execution flow:
          user_query
            └─ llm_response (turn 1)
                ├─ tool_call: foo
                │   └─ tool_result: foo
                └─ tool_call: bar
                    └─ tool_result: bar
            └─ llm_response (turn 2)   ← after tool results, LLM is called again
                ...

        A session may contain multiple user_query → llm_response* sequences
        (multi-turn). Each user_query becomes a top-level child of the root.
        """
        if not rows:
            return ExecutionNode(
                node_id="empty", node_type="empty", event_id=None,
                ts=datetime.min, duration_s=0,
            )

        events = []
        for r in rows:
            event_id, event_type, content, skill_name, ts, _agent_id, metadata = r
            events.append({
                "event_id": event_id,
                "event_type": event_type,
                "content": content,
                "skill_name": skill_name,
                "ts": ts,
                "metadata": metadata if isinstance(metadata, dict) else {},
            })

        root = self._build_basic_tree(events)
        self._enrich_llm_metrics(root, events)
        self._calculate_metrics(root, None)
        return root

    def _build_basic_tree(self, events: list[dict]) -> ExecutionNode:
        """Build tree by walking the event list sequentially.

        Algorithm: iterate events in timestamp order and maintain a simple
        state machine that groups events into the correct parent-child
        hierarchy.  This correctly handles multi-turn tool-use loops:

            user_query → llm_response → tool_call → tool_result →
            llm_response → tool_call → tool_result → llm_response (final)
        """
        if not events:
            return ExecutionNode(
                node_id="empty", node_type="empty", event_id=None,
                ts=datetime.min, duration_s=0,
            )

        first_ts = events[0]["ts"]
        last_ts = events[-1]["ts"]

        # Synthetic root that spans the entire session
        root = ExecutionNode(
            node_id="session_root",
            node_type="session",
            event_id=None,
            ts=first_ts,
            duration_s=(last_ts - first_ts).total_seconds(),
        )

        # State: current user_query node and current llm_response node
        current_uq: ExecutionNode | None = None
        current_llm: ExecutionNode | None = None
        prev_ts = first_ts

        for e in events:
            etype = e["event_type"]
            ts = e["ts"]

            if etype == "user_query":
                node = ExecutionNode(
                    node_id=e["event_id"],
                    node_type="user_query",
                    event_id=e["event_id"],
                    ts=ts,
                    duration_s=0,  # will be finalized later
                    detail=e["content"][:60] if e["content"] else "",
                    metadata=e["metadata"],
                )
                root.children.append(node)
                current_uq = node
                current_llm = None
                prev_ts = ts

            elif etype == "llm_response":
                # Each llm_response is a child of the current user_query.
                # Duration = time since previous event (approximates inference time).
                node = ExecutionNode(
                    node_id=e["event_id"],
                    node_type="llm_response",
                    event_id=e["event_id"],
                    ts=ts,
                    duration_s=(ts - prev_ts).total_seconds(),
                    detail=e["content"][:60] if e["content"] else "",
                    metadata=e["metadata"],
                )
                parent = current_uq or root
                parent.children.append(node)
                current_llm = node
                prev_ts = ts

            elif etype == "tool_call":
                try:
                    data = json.loads(e["content"]) if e["content"] else {}
                    tool_name = data.get("name", e["skill_name"] or "unknown")
                except (json.JSONDecodeError, TypeError, KeyError):
                    tool_name = e["skill_name"] or "unknown"

                node = ExecutionNode(
                    node_id=e["event_id"],
                    node_type="tool_call",
                    event_id=e["event_id"],
                    ts=ts,
                    duration_s=0,  # tool_call itself is instantaneous; tool_result carries the real duration
                    detail=tool_name,
                    metadata=e["metadata"],
                )
                parent = current_llm or current_uq or root
                parent.children.append(node)
                prev_ts = ts

            elif etype == "tool_result":
                try:
                    data = json.loads(e["content"]) if e["content"] else {}
                    result_name = data.get("name", e["skill_name"] or "")
                except (json.JSONDecodeError, TypeError, KeyError):
                    result_name = e["skill_name"] or ""

                # Find the matching tool_call by walking backwards through
                # the current llm_response's children (or root's children).
                parent_container = current_llm or current_uq or root
                matched = False
                for candidate in reversed(parent_container.children):
                    if candidate.node_type != "tool_call":
                        continue
                    # Match by name; only accept unnamed match if there is
                    # exactly one unmatched tool_call to avoid ambiguity.
                    if candidate.detail == result_name or (
                        not result_name and not candidate.children
                    ):
                        result_node = ExecutionNode(
                            node_id=e["event_id"],
                            node_type="tool_result",
                            event_id=e["event_id"],
                            ts=ts,
                            duration_s=(ts - candidate.ts).total_seconds(),
                            detail=candidate.detail,
                            metadata=e["metadata"],
                        )
                        candidate.children.append(result_node)
                        matched = True
                        break

                if not matched:
                    # Orphan tool_result — attach to parent directly
                    orphan = ExecutionNode(
                        node_id=e["event_id"],
                        node_type="tool_result",
                        event_id=e["event_id"],
                        ts=ts,
                        duration_s=(ts - prev_ts).total_seconds(),
                        detail=result_name or "unknown",
                        metadata=e["metadata"],
                    )
                    parent_container.children.append(orphan)

                prev_ts = ts

            else:
                # Other event types (system_message, snapshot, etc.) — attach as leaf
                node = ExecutionNode(
                    node_id=e["event_id"],
                    node_type=etype,
                    event_id=e["event_id"],
                    ts=ts,
                    duration_s=0,
                    detail=e["content"][:60] if e["content"] else "",
                    metadata=e["metadata"],
                )
                parent = current_uq or root
                parent.children.append(node)
                prev_ts = ts

        # Finalize user_query durations: from its ts to the ts of its last descendant
        for child in root.children:
            if child.node_type == "user_query" and child.children:
                last_child_ts = self._last_ts(child)
                child.duration_s = (last_child_ts - child.ts).total_seconds()

        return root

    @staticmethod
    def _last_ts(node: "ExecutionNode") -> datetime:
        """Return the latest timestamp in the subtree rooted at *node*."""
        latest = node.ts
        for child in node.children:
            candidate = SessionAnalyzer._last_ts(child)
            if candidate > latest:
                latest = candidate
        return latest

    def _enrich_llm_metrics(self, node: ExecutionNode, events: list[dict]) -> None:
        """Populate token counts and cost on llm_response nodes from their metadata."""
        if node.node_type == "llm_response" and node.metadata:
            tokens_in = node.metadata.get("prompt_tokens", 0)
            tokens_out = node.metadata.get("completion_tokens", 0)
            model = node.metadata.get("model", "unknown")

            if tokens_in or tokens_out:
                node.tokens_in = tokens_in
                node.tokens_out = tokens_out
                node.cost_usd = _calculate_cost(model, tokens_in, tokens_out)

        for child in node.children:
            self._enrich_llm_metrics(child, events)

    def _calculate_metrics(self, node: ExecutionNode, parent: ExecutionNode | None) -> None:
        """Calculate derived metrics (duration_pct, issues)."""
        # Calculate parent duration percentage
        if parent and parent.duration_s > 0:
            node.parent_duration_pct = (node.duration_s / parent.duration_s) * 100

        # Detect issues
        if node.duration_s >= SLOW_NODE_THRESHOLD_S:
            node.issues.append("SLOW")

        if node.parent_duration_pct is not None and node.parent_duration_pct > 50:
            node.issues.append("BOTTLENECK")

        if node.tokens_in is not None and node.tokens_in > HIGH_TOKEN_THRESHOLD:
            node.issues.append("HIGH_TOKEN")

        if node.cost_usd is not None and node.cost_usd > EXPENSIVE_THRESHOLD_USD:
            node.issues.append("EXPENSIVE")

        # Check tool result size
        if node.node_type == "tool_result" and node.metadata:
            result_tokens = node.metadata.get("result_size_tokens", 0)
            if result_tokens > LARGE_CONTEXT_THRESHOLD:
                node.issues.append("LARGE_CONTEXT")

        for child in node.children:
            self._calculate_metrics(child, node)

    def _build_summary(self, root: ExecutionNode) -> ExecutionSummary:
        """Build aggregated summary from execution tree.

        Time accounting: only *leaf* nodes contribute to ``time_by_category``
        so that parent durations (which include children) are not double-counted.

        Turn numbering: each llm_response encountered (depth-first) increments
        the global turn counter, so sibling llm_responses under the same
        user_query get distinct turn numbers.
        """
        time_by_category: dict[str, float] = {}
        tokens_by_source: dict[str, int] = {}
        cost_by_turn: dict[int, float] = {}
        root_causes: list[str] = []
        turn_counter = [0]  # mutable counter shared across recursion

        def traverse(node: ExecutionNode) -> None:
            category = node.node_type
            if category in ("tool_call", "tool_result"):
                category = "tool_execution"
            elif category == "llm_response":
                category = "llm_inference"
                turn_counter[0] += 1

            current_turn = max(turn_counter[0], 1)

            # Only leaf nodes contribute time to avoid double-counting.
            if not node.children:
                time_by_category[category] = time_by_category.get(category, 0) + node.duration_s

            if node.tokens_in is not None:
                tokens_by_source["prompt"] = tokens_by_source.get("prompt", 0) + node.tokens_in
            if node.tokens_out is not None:
                tokens_by_source["completion"] = tokens_by_source.get("completion", 0) + node.tokens_out

            if node.cost_usd is not None and node.cost_usd > 0:
                cost_by_turn[current_turn] = cost_by_turn.get(current_turn, 0) + node.cost_usd

            if "SLOW" in node.issues:
                root_causes.append(f"{node.node_type} '{node.detail}' took {node.duration_s:.1f}s")
            if "HIGH_TOKEN" in node.issues and node.tokens_in is not None:
                root_causes.append(f"{node.node_type} used {node.tokens_in:,} tokens")

            for child in node.children:
                traverse(child)

        traverse(root)

        bottleneck = max(time_by_category.items(), key=lambda x: x[1])[0] if time_by_category else None
        largest_source = max(tokens_by_source.items(), key=lambda x: x[1])[0] if tokens_by_source else None

        return ExecutionSummary(
            total_duration_s=root.duration_s,
            time_by_category=time_by_category,
            bottleneck_category=bottleneck,
            total_tokens=sum(tokens_by_source.values()),
            tokens_by_source=tokens_by_source,
            largest_token_source=largest_source,
            total_cost_usd=sum(cost_by_turn.values()),
            cost_by_turn=cost_by_turn,
            root_causes=root_causes[:5],
        )
