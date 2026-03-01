"""Reflect tool — agent self-reflection via server-side diagnostic evidence.

Edge tool that calls the server's /chat/session/{sid}/reflect endpoint to
gather evidence the LLM cannot see in its context window: event trails with
timing, skill selection history, procedural memories, implicit feedback, and
deterministic diagnosis hints.

Same pattern as GetAgentInfoTool calling /introspection/* endpoints.
"""

import json
import logging
from typing import Any

from cli.tools.base import EdgeTool, SideEffect

logger = logging.getLogger(__name__)


class ReflectTool(EdgeTool):
    """Gather diagnostic evidence for self-correction from server-side data."""

    def __init__(self, api_client: Any = None, session_info: dict[str, Any] | None = None):
        self._api_client = api_client
        self._session = session_info or {}

    name = "reflect"
    description = (
        "Gather diagnostic evidence from server-side data that you cannot see "
        "in your context window: event trails with timing, skill selection "
        "history and outcomes, past lessons from procedural memory, implicit "
        "user feedback signals, and automated diagnosis hints. "
        "Use when a tool fails, results are unexpected, or you need to "
        "understand what went wrong before retrying."
    )
    parameters = {
        "type": "object",
        "properties": {
            "focus": {
                "type": "string",
                "enum": ["auto", "skill_failure", "unexpected_result", "data_quality"],
                "description": "What to investigate. 'auto' detects from recent events.",
            },
            "last_n": {
                "type": "integer",
                "description": "How many recent events to analyze (default 20)",
            },
        },
    }
    side_effect = SideEffect.READ

    async def execute(self, focus: str = "auto", last_n: int = 20, **_: Any) -> str:
        session_id = self._session.get("session_id")
        if not session_id:
            return json.dumps({"error": "No session_id available"})
        if self._api_client is None:
            return json.dumps({"error": "No API client available"})

        try:
            data = await self._api_client.get_reflect(session_id, focus=focus, last_n=last_n)
            return json.dumps(data)
        except Exception as e:
            return json.dumps({"error": f"Reflect failed: {e}"})
