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
        "Diagnose issues by querying server-side data invisible to your context: "
        "event trails with timing, skill selection history, available cloud skills "
        "and their schemas, tool usage counts, past lessons, and similar queries "
        "from previous sessions. "
        "Use focus='tool_selection' to see what tools are available and why one wasn't used. "
        "Use focus='skill_failure' after a tool fails. "
        "Use focus='history' to find how similar questions were handled before."
    )
    parameters = {
        "type": "object",
        "properties": {
            "focus": {
                "type": "string",
                "enum": ["auto", "skill_failure", "unexpected_result", "data_quality", "tool_selection", "history"],
                "description": "What to investigate. 'tool_selection' shows available skills and usage. 'history' finds similar past queries.",
            },
            "question": {
                "type": "string",
                "description": "Optional: what to investigate, e.g. 'why wasn't list_prs used?'",
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
