"""Decision trace tool — inspect why the LLM chose (or didn't choose) specific tools.

Edge tool that calls /chat/session/{sid}/decision-trace to surface:
- All available tools (edge + cloud) with their schemas
- Cloud skill execution results within the current turn
- Tool selection reasoning from the LLM's perspective

Helps developers diagnose: "why didn't the LLM use skill X?"
"""

import json
import logging
from typing import Any

from cli.tools.base import EdgeTool, SideEffect

logger = logging.getLogger(__name__)


class DecisionTraceTool(EdgeTool):
    """Inspect tool selection decisions: what was available, what was chosen, why."""

    def __init__(self, api_client: Any = None, session_info: dict[str, Any] | None = None):
        self._api_client = api_client
        self._session = session_info or {}

    name = "decision_trace"
    description = (
        "Inspect the decision-making process: what tools (edge + cloud) were "
        "available to you, which cloud skills exist and their schemas, and "
        "what happened in previous tool selections this session. "
        "Use when you need to understand why a specific skill wasn't used, "
        "or to diagnose tool selection issues."
    )
    parameters = {
        "type": "object",
        "properties": {
            "question": {
                "type": "string",
                "description": "What to investigate, e.g. 'why wasn't list_prs used?' or 'what cloud skills are available?'",
            },
        },
    }
    side_effect = SideEffect.READ

    async def execute(self, question: str = "", **_: Any) -> str:
        session_id = self._session.get("session_id")
        if not session_id:
            return json.dumps({"error": "No session_id available"})
        if self._api_client is None:
            return json.dumps({"error": "No API client available"})

        try:
            data = await self._api_client.get_decision_trace(session_id, question=question)
            return json.dumps(data, ensure_ascii=False)
        except Exception as e:
            return json.dumps({"error": f"Decision trace failed: {e}"})
