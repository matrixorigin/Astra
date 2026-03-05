"""Reflect tool — unified diagnostic: event trails, skill decisions, tool selection, history.

Merges the former reflect + decision_trace into one tool.
Edge tool that calls /chat/session/{sid}/reflect with focus modes.
"""

import json
import logging
from typing import Any

from cli.tools.base import EdgeTool, SideEffect

logger = logging.getLogger(__name__)


class ReflectTool(EdgeTool):
    """Unified diagnostic: events, skill decisions, tool selection, cross-session history."""

    def __init__(self, api_client: Any = None, session_info: dict[str, Any] | None = None):
        self._api_client = api_client
        self._session = session_info or {}

    name = "reflect"
    description = (
        "Inspect my own internal state and decision-making for previous turns: "
        "what I remember about the user, why I chose certain tools, what context I saw. "
        "Use when user asks about memories, what I know about them, "
        "decision process, why I did something, or wants to understand my past behavior."
    )
    parameters = {
        "type": "object",
        "properties": {
            "focus": {
                "type": "string",
                "enum": ["auto", "skill_failure", "unexpected_result", "data_quality", "tool_selection", "history", "performance"],
                "description": (
                    "What to investigate. "
                    "'performance': timing, gaps, bottlenecks, high token usage. "
                    "'skill_failure': why a tool failed. "
                    "'unexpected_result': wrong or surprising answer. "
                    "'data_quality': irrelevant or stale context. "
                    "'tool_selection': available skills and why one wasn't used. "
                    "'history': similar past tool calls across sessions."
                ),
            },
            "question": {
                "type": "string",
                "description": (
                    "What specifically to investigate. Always provide this for better results. "
                    "Examples: 'why wasn't list_prs used?', 'why was the last response slow?', "
                    "'why did search return irrelevant results?'"
                ),
            },
            "last_n": {
                "type": "integer",
                "description": "How many recent events to analyze (default 20)",
            },
        },
    }
    side_effect = SideEffect.READ

    async def execute(self, focus: str = "auto", question: str = "", last_n: int = 20, **_: Any) -> str:
        session_id = self._session.get("session_id")
        if not session_id:
            return json.dumps({"error": "No session_id available"})
        if self._api_client is None:
            return json.dumps({"error": "No API client available"})

        try:
            data = await self._api_client.get_reflect(
                session_id, focus=focus, last_n=last_n, question=question)
            return json.dumps(data, ensure_ascii=False)
        except Exception as e:
            return json.dumps({"error": f"Reflect failed: {e}"})
